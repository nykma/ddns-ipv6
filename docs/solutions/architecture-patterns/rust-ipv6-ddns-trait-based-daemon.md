---
title: "Rust IPv6 DDNS trait-based daemon architecture"
date: 2026-06-25
category: docs/solutions/architecture-patterns
module: ddns-ipv6
problem_type: architecture_pattern
component: tooling
severity: medium
applies_when:
  - "Building system daemons in Rust with multiple pluggable backends behind a shared trait"
  - "Implementing event-driven main loops that react to OS signals and async notification streams"
  - "Handling IPv6 prefix rotation for dynamic DNS updates across multiple hosts"
  - "Designing config-driven factory wiring where config enum variants determine which impl to construct"
tags:
  - rust
  - ipv6
  - ddns
  - daemon
  - tokio
  - traits
  - dns
  - serde
  - icmpv6
  - socket-programming
---

# Rust IPv6 DDNS trait-based daemon architecture

## Context

`ddns-ipv6` is a Rust daemon that updates AAAA DNS records when ISP IPv6 prefix rotations occur. Unlike IPv4 DDNS (one NAT IP), IPv6 gives every host its own global address composed of a shared ISP prefix and a per-host suffix. When the ISP rotates the prefix, every host's AAAA record becomes stale simultaneously.

The daemon was built from a detailed implementation plan that specified library versions, API shapes, and protocol-level packet layouts. During implementation, eight categories of "plan code meets real crate APIs" surfaced — each required adapting the plan's assumptions to the actual dependency APIs available at build time, plus fixing subtle bugs in protocol-level parsers and test infrastructure.

The daemon uses three IPv6 prefix-detection methods (DNS polling via `hickory-resolver`, netlink address monitoring via `nlink`, raw ICMPv6 Router Advertisement capture via `socket2`), two DNS update backends (Cloudflare API via `reqwest`, RFC 2136 TSIG via `dns-update`), and a `tokio::select!`-based main loop with signal handling.

## Guidance

### 1. Trait-based pluggable architecture with config-driven factory wiring

Both prefix detection and DNS updating use a trait + multiple implementations pattern. The config determines which implementation to construct at startup — no dynamic dispatch over the config, just one `Arc<dyn Trait>` per concern.

```rust
// prefix.rs — one trait, three impls
#[async_trait]
pub trait PrefixDetector: Send + Sync {
    async fn detect(&self) -> Result<Ipv6Addr, Error>;
    fn changes(&self) -> watch::Receiver<()>;
}

// main.rs — config-driven construction
fn build_detector(config: &Config) -> Result<Arc<dyn PrefixDetector>, Error> {
    match &config.prefix {
        PrefixConfig::Dns { reference_domain } => { /* DnsResolver::new */ }
        PrefixConfig::Netlink { interface } => { /* NetlinkWatcher::new */ }
        PrefixConfig::Ra { interface } => { /* RaListener::new */ }
    }
}
```

The `changes()` method returns a `watch::Receiver<()>` — the main loop doesn't need to know whether changes come from a polling timer, a netlink event stream, or a blocking RA socket thread. Each implementation manages its own notification mechanism internally.

### 2. Main loop: tokio select! over watch, Notify, and CancellationToken

Three event sources multiplexed cleanly:

```rust
let cancel_token = CancellationToken::new();
let force_refresh = Arc::new(Notify::new());
let mut change_rx = detector.changes();

loop {
    select! {
        _ = change_rx.changed() => {}         // prefix change or poll tick
        _ = force_refresh.notified() => {}    // SIGUSR1 received
        _ = cancel_token.cancelled() => break; // shutdown
    }
    run_update_cycle(&*detector, &*updater, &suffixes, &mut cache).await;
}
```

Signal handlers are spawned as background tasks:
- `SIGTERM`/`SIGINT` → `cancel_token.cancel()`
- `SIGUSR1` → `force_refresh.notify_one()`

### 3. In-memory cache with DNS as source of truth

On startup, the daemon queries current DNS state to seed the cache. During updates, records matching the computed address are skipped. No persistent cache file — DNS is authoritative, and an extra API call on startup is cheaper than a stale cache.

```rust
// Startup: query DNS to seed cache
for (_suffix, domain) in &suffixes {
    if let Ok(Some(addr)) = updater.get_record(domain).await {
        cache.insert(domain.clone(), addr);
    }
}

// Update cycle: skip if already correct
let new_addr = util::combine(&prefix, suffix);
if cache.get(domain) == Some(&new_addr) { continue; }
updater.set_record(domain, &new_addr).await?;
cache.insert(domain.clone(), new_addr);
```

### 4. Prefer manual TOML dispatch over serde tagged enums

The `toml` crate does not reliably support `#[serde(tag)]` + `#[serde(flatten)]` combinations with nested-table TOML syntax. Parse into `toml::Table` and dispatch manually:

```rust
fn parse_dns(root: &toml::Table) -> Result<ProviderConfig, ConfigError> {
    let dns_table = root.get("dns")
        .and_then(|v| v.as_table())
        .ok_or_else(|| ConfigError::MissingField("dns".into()))?;
    let provider = dns_table.get("provider")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ConfigError::MissingField("dns.provider".into()))?;
    match provider {
        "cloudflare" => { /* extract zone_id, api_token with env:VAR resolution */ }
        "rfc2136" => { /* extract server, key_name, key_algorithm, key_secret */ }
        other => Err(/* ... */)
    }
}
```

This is more verbose than serde derive, but it's robust, easy to debug, and works with the flat-key TOML layout users expect.

### 5. ICMPv6 Router Advertisement: offset 16, not 20

The ICMPv6 header is 4 bytes, not 8. The RA body (hop limit, flags, router lifetime, reachable time, retrans timer) adds 12 bytes. PIO options start at offset 16:

```rust
fn parse_ra_packet(data: &[u8]) -> Option<Ipv6Addr> {
    if data.len() < 16 { return None; }   // need header + RA body
    if data[0] != 134 { return None; }    // type == RA

    let mut offset = 16;  // ICMPv6 header (4) + RA body (12)
    while offset + 2 <= data.len() {
        let opt_type = data[offset];
        let opt_len = data[offset + 1] as usize;
        // type 3 = Prefix Information Option
        // A flag = bit 6 (0x40), L flag = bit 7 (0x80)
    }
}
```

### 6. socket2 0.6: recv takes `&mut [MaybeUninit<u8>]`

socket2 0.5's `recv` took `&mut [u8]`. Version 0.6 changed to `&mut [MaybeUninit<u8>]` for soundness without zero-initialization. The RA listener must handle this:

```rust
let mut buf: [MaybeUninit<u8>; 1500] = unsafe { MaybeUninit::uninit().assume_init() };

match socket.recv(&mut buf) {
    Ok(n) => {
        // SAFETY: recv filled n bytes
        let data = unsafe {
            std::slice::from_raw_parts(buf.as_ptr() as *const u8, n)
        };
        if let Some(prefix) = parse_ra_packet(data) { /* ... */ }
    }
    Err(e) => { /* ... */ }
}
```

### 7. libc gaps: raw constants when the crate doesn't export them

`libc::ICMP6_FILTER` is not exported on Linux. Use the raw constant `1`:

```rust
let ret = unsafe {
    libc::setsockopt(
        fd, libc::IPPROTO_ICMPV6,
        1,  // ICMP6_FILTER (not in libc crate)
        filter.as_ptr() as *const libc::c_void,
        std::mem::size_of_val(&filter) as libc::socklen_t,
    )
};
```

### 8. Rust 2024 edition: env var mutation is unsafe

`std::env::set_var` and `std::env::remove_var` are `unsafe` in edition 2024. Tests must wrap them:

```rust
#[test]
fn env_var_resolution() {
    unsafe { std::env::set_var("DDNS_TEST_TOKEN", "env-resolved-token") };
    let config = Config::load(&path).unwrap();
    unsafe { std::env::remove_var("DDNS_TEST_TOKEN") };
    assert_eq!(config.api_token, "env-resolved-token");
}
```

### 9. Test file isolation: AtomicU64 counter beyond PID

When tests in the same process share temp files, add a monotonically increasing counter:

```rust
fn write_tmp(content: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = format!("/tmp/ddns-ipv6-test-{}-{n}.toml", std::process::id());
    std::fs::write(&path, content).unwrap();
    path.into()
}
```

## Why This Matters

Plans that specify library versions and API shapes are snapshots of a moving target. Crates advance by minor versions between plan date and implementation day. Recognizing these as **version-skew adaptations** rather than plan-errors prevents wasted time forcing old APIs.

Protocol-level bugs (ICMPv6 header size, PIO flag bits) demonstrate that RFC references in plans are insufficient — the implementer must verify byte layouts against RFC diagrams or packet captures, not prose descriptions.

The trait-based architecture with `watch::Receiver<()>` as the change-notification contract decouples the main loop from implementation details, making it trivial to add new prefix detection or DNS update methods without touching the core state machine.

## When to Apply

- When implementing a Rust project from a plan that specifies dependency versions — expect API drift between plan and implementation day
- When designing a daemon with multiple backends — use traits with `Arc<dyn Trait>`, config-driven factory construction, and `watch::Receiver` as the change-notification contract
- When dealing with raw sockets or packet capture — verify header sizes against RFC diagrams, not plan prose
- When using Rust 2024 edition — audit all `set_var`/`remove_var` calls for `unsafe` blocks
- When writing tests that create temp files — always include a process-unique counter beyond PID

## Examples

**Config-driven factory** — `src/main.rs`:

```rust
fn build_detector(config: &Config) -> Result<Arc<dyn PrefixDetector>, Error> {
    match &config.prefix {
        PrefixConfig::Dns { reference_domain } => {
            let detector = DnsResolver::new(
                reference_domain.clone(),
                Duration::from_secs(config.interval_secs),
            )?;
            Ok(Arc::new(detector))
        }
        #[cfg(target_os = "linux")]
        PrefixConfig::Netlink { interface } => {
            let detector = NetlinkWatcher::new(interface.clone())?;
            Ok(Arc::new(detector))
        }
        #[cfg(target_os = "linux")]
        PrefixConfig::Ra { interface } => {
            let detector = RaListener::new(interface.clone())?;
            Ok(Arc::new(detector))
        }
        #[cfg(not(target_os = "linux"))]
        PrefixConfig::Netlink { .. } | PrefixConfig::Ra { .. } => {
            Err(Error::Config(ConfigError::MissingField(
                "netlink and ra methods are Linux-only".into(),
            )))
        }
    }
}
```

**RA packet builder for tests** — `src/prefix/ra.rs`:

```rust
fn build_ra_packet(prefix: Ipv6Addr, a_flag: bool) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(48);

    // ICMPv6 header (4 bytes)
    pkt.push(134); pkt.push(0);
    pkt.extend_from_slice(&[0u8, 0]);  // checksum

    // RA body (12 bytes)
    pkt.push(64); pkt.push(0);                           // hop_limit, flags
    pkt.extend_from_slice(&1800u16.to_be_bytes());       // router_lifetime
    pkt.extend_from_slice(&0u32.to_be_bytes());          // reachable_time
    pkt.extend_from_slice(&0u32.to_be_bytes());          // retrans_timer

    // PIO (32 bytes) — starts at offset 16
    pkt.push(3); pkt.push(4); pkt.push(64);              // type, len, prefix_len
    let flags: u8 = if a_flag { 0xC0 } else { 0x80 };    // A+L or L-only
    pkt.push(flags);
    pkt.extend_from_slice(&86400u32.to_be_bytes());      // valid_lifetime
    pkt.extend_from_slice(&14400u32.to_be_bytes());      // preferred_lifetime
    pkt.extend_from_slice(&[0u8; 4]);                    // reserved
    pkt.extend_from_slice(&prefix.octets());             // prefix (16 bytes)

    assert_eq!(pkt.len(), 48);  // 4 + 12 + 32
    pkt
}
```

## Related

- Implementation plan: `PLAN.md` (original 10-step plan)
- Source: `src/prefix.rs`, `src/updater.rs`, `src/main.rs`, `src/config.rs`
- Dependencies: `hickory-resolver` 0.26, `nlink` 0.21, `dns-update` 0.5.3, `socket2` 0.6, `tokio` 1.x
