# ddns-ipv6 — Technical Plan

## Problem Statement

IPv4 DDNS tools assume one public IP behind NAT — query your own IP, push to DNS. IPv6 breaks this model: **every host has its own globally routable address**, derived from an ISP-assigned **prefix** + a host-local **suffix** (typically SLAAC-generated or manually configured). When the ISP rotates the prefix, every host's address changes, and all AAAA records must be updated simultaneously.

This tool separates concerns: the user configures per-host suffixes once; the tool detects the current prefix (by one of two methods), combines them, and updates the corresponding AAAA records in batch.

---

## Architecture Overview

```
┌──────────┐    ┌──────────────────┐    ┌──────────────┐
│ Config   │───▶│  Prefix Detector │───▶│ DNS Updater  │
│ (TOML)   │    │  · DNS query     │    │  · RFC 2136  │
│          │    │  · Netlink addr  │    │  · Cloudflare │
│ suffixes │    │  · Raw ICMPv6 RA │    │  · extensible │
└──────────┘    └──────────────────┘    └──────────────┘
                         │                      │
                    prefix (/64)          per-host AAAA
                         │                      │
                         └──────┬───────────────┘
                                │
                     prefix + suffix → full IPv6
```

### Core Data Flow

1. Load config: list of `(suffix, domain, interface?)` tuples + DNS provider credentials
2. Detect current /64 prefix via the configured method
3. For each host: `prefix::suffix` → full `Ipv6Addr`
4. Compare against cached last-known address; skip if unchanged
5. Update AAAA record via configured DNS provider
6. Repeat on a configurable interval (or on prefix-change event)

---

## Component Breakdown

### 1. Configuration (`config.rs`)

Format: TOML. One file, all state.

```toml
# How to detect the prefix
[prefix]
method = "dns"            # "dns" | "netlink" | "ra"
# method = "dns"
reference_domain = "my-router.example.com"   # AAAA of this domain → extract /64
# method = "netlink" | "ra"
interface = "enp3s0"      # interface to watch

# How to push DNS updates
[dns]
provider = "cloudflare"   # "cloudflare" | "rfc2136" | (future: "dnspod", "route53", …)

[dns.cloudflare]
zone_id = "abc123"
api_token = "env:CF_API_TOKEN"   # "env:VAR" reads from env

[dns.rfc2136]
server = "ns1.example.com:53"
key_name = "ddns-key."
key_algorithm = "hmac-sha256"
key_secret = "env:TSIG_SECRET"

# Per-host mapping: suffix → domain
[[hosts]]
suffix = "::1"                    # the lower 64 bits, any valid IPv6 notation
domain = "server-a.example.com"

[[hosts]]
suffix = "::dead:beef"
domain = "server-b.example.com"

[[hosts]]
suffix = "0216:3eff:feb4:a100"   # EUI-64 style suffix
domain = "nas.example.com"

# Optional: update interval (default: poll every 300s; netlink/ra are event-driven)
interval_secs = 300
```

- Suffix parsing: accept any valid IPv6 address string, mask to lower 64 bits (`u128::from(parsed) & 0xFFFF_FFFF_FFFF_FFFF`).
- `env:VAR` syntax resolved at startup; no secrets in config file.

### 2. Prefix Detection (`prefix.rs`)

Trait:

```rust
#[async_trait]
trait PrefixDetector: Send + Sync {
    /// Returns the current /64 prefix for the monitored network.
    async fn detect(&self) -> Result<Ipv6Net>;
    /// Returns a channel that fires when the prefix changes (event-driven methods).
    /// Polling methods return a ticker channel.
    fn changes(&self) -> Receiver<()>;
}
```

Three implementations:

#### A. DNS-based detection (`DnsResolver`)

- Resolve the `reference_domain` AAAA record via `hickory-resolver`.
- If multiple AAAA records returned, take the first global unicast address (match `2000::/3`).
- Extract `/64`: `Ipv6Net::new(addr, 64)?`.
- Poll on `interval_secs`, skip update if unchanged.
- **Crate**: `hickory-resolver`
- **Pros**: zero privilege, works everywhere (including Docker without MACVLAN).
- **Cons**: requires an existing stable DDNS host whose AAAA is always current.

#### B. Netlink address monitor (`NetlinkWatcher`)

- Open a netlink socket (`NETLINK_ROUTE`), subscribe to `RTMGRP_IPV6_IFADDR`.
- Filter `RTM_NEWADDR` messages for the configured `interface`, `IFA_F_GLOBAL` scope, non-temporary addresses.
- When a new global /64 address appears (or an existing one's prefix changes), extract the prefix and signal.
- **Crates**: `rtnetlink`, `netlink-packet-route`
- **Pros**: no `CAP_NET_RAW`, no polling, event-driven.
- **Cons**: indirect — monitors the kernel's *reaction* to RAs, not RAs themselves. Linux only.

Implementation note: `rtnetlink` crate provides a `RouteHandle` with an async `link_addr_message_stream()`. We filter for `AddressMessage` carrying `AddressScope::Global` and `IFAF_F_PERMANENT` (to skip temporary privacy addresses).

#### C. Raw ICMPv6 RA listener (`RaListener`)

- Create `socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6)` via `socket2`.
- Use `setsockopt(ICMP6_FILTER)` to pass only ICMPv6 type 134 (Router Advertisement).
- Bind to the configured interface (SO_BINDTODEVICE) or join `ff02::2` (all-routers, though RAs go to `ff02::1`).
- Parse the RA: skip ICMPv6 header (8 bytes) + RA body (12 bytes), iterate options. Type 3 = Prefix Information Option. Extract the prefix field (16 bytes) + prefix length.
- On valid RA with a global prefix → signal change.
- **Crates**: `socket2`
- **Pros**: most direct, receives actual RAs, no dependency on kernel address configuration.
- **Cons**: requires `CAP_NET_RAW` (or root). Linux only.

RA packet layout for parsing:

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|     Type=134  |     Code=0    |          Checksum             |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
| Cur Hop Limit |M|O|  Reserved |       Router Lifetime         |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                         Reachable Time                        |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                          Retrans Timer                        |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|   Options ...
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-
```

Prefix Information Option (type=3):

```
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|     Type=3    |    Length=4   | Prefix Length |L|A| Reserved1 |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                         Valid Lifetime                        |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                       Preferred Lifetime                      |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                           Reserved2                           |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                                                               |
+                                                               +
|                                                               |
+                          Prefix (16 bytes)                    +
|                                                               |
+                                                               +
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

Only take options with A=1 (autonomous address-configuration flag) and a global prefix.

#### Recommendation

Default: **netlink** — best balance of zero-privilege and event-driven behavior. Provide `dns` as the universal fallback. Offer `ra` for users who want maximal precision and can grant `CAP_NET_RAW` (e.g., systemd unit with `AmbientCapabilities=CAP_NET_RAW`).

### 3. DNS Updater (`updater.rs`)

Trait:

```rust
#[async_trait]
trait DnsUpdater: Send + Sync {
    /// Set or update the AAAA record for `domain` to `addr`.
    async fn set_record(&self, domain: &str, addr: &Ipv6Addr) -> Result<()>;
    /// Get the current AAAA record value, if any.
    async fn get_record(&self, domain: &str) -> Result<Option<Ipv6Addr>>;
}
```

Two initial providers:

#### A. Cloudflare API v4

- `PATCH /zones/:zone_id/dns_records/:record_id` or `PUT` + create-if-not-exists logic.
- `GET /zones/:zone_id/dns_records?type=AAAA&name=<domain>` to check current value.
- Auth: `Authorization: Bearer <api_token>`.
- **Crate**: `reqwest` + `serde_json`.

#### B. RFC 2136 (TSIG-signed Dynamic Update)

- Construct DNS UPDATE message with TSIG signature.
- Send via UDP (or TCP for large messages) to authoritative nameserver.
- **Crate**: `hickory-proto` with `dnssec` feature for TSIG support.

Future providers (DNSPod, Alibaba Cloud DNS, Route53) follow the same trait — each is roughly 80 lines of provider-specific JSON serialization around `reqwest`.

### 4. Main Loop (`main.rs`)

```
1. Parse CLI (clap) → path to config file
2. Load + validate config (check env:VAR resolution)
3. Construct detector + updater from config
4. For each host, resolve current DNS AAAA into a cache
5. Loop:
   a. Wait for prefix change signal (or ticker)
   b. Detect current prefix
   c. For each host:
      - Combine prefix + suffix → full Ipv6Addr
      - Compare with cache
      - If changed: call updater.set_record(), update cache
      - Log the transition
```

### 5. Suffix-Prefix Combination

```rust
fn combine(prefix: Ipv6Net, suffix: &Ipv6Addr) -> Ipv6Addr {
    let suffix_u128 = u128::from(*suffix) & 0x0000_0000_0000_0000_FFFF_FFFF_FFFF_FFFF;
    let net_u128 = u128::from(prefix.network());
    Ipv6Addr::from(net_u128 | suffix_u128)
}
```

Validation: reject suffixes with bits set in the upper 64 bits (i.e., user accidentally provided a full address). Warn, don't error — mask silently.

### 6. Logging & Observability

- **Crate**: `tracing` + `tracing-subscriber` (JSON + pretty formats).
- Structured fields: `domain`, `old_addr`, `new_addr`, `prefix`, `detector`.
- `RUST_LOG=ddns_ipv6=debug` for verbose prefix-change and update traces.
- Signal handling: on `SIGUSR1`, force an immediate detection + update cycle (useful for DHCPv6-PD prefix changes that bypass SLAAC).

---

## Crate Inventory

| Crate | Purpose |
|---|---|
| `tokio` (full) | Async runtime, signal handling |
| `clap` v4 (derive) | CLI argument parsing |
| `serde` + `serde_json` | Deserialization, JSON API payloads |
| `toml` | Config file parsing |
| `hickory-resolver` | DNS AAAA resolution (Method A) |
| `hickory-proto` (features: `dnssec`) | RFC 2136 TSIG update messages |
| `rtnetlink` + `netlink-packet-route` | Netlink address monitoring (Method B) |
| `socket2` | Raw ICMPv6 socket creation (Method C) |
| `reqwest` | HTTPS client for provider APIs |
| `tracing` + `tracing-subscriber` | Structured logging |
| `thiserror` | Error type derivation |
| `anyhow` | Application-level error propagation |
| `tokio-util` | `CancellationToken` for graceful shutdown |
| `ipnet` | Ipv6Net for prefix operations (optional; `std::net` + manual bitops also sufficient) |

`ipnet` is **optional** — `std::net::Ipv6Addr` with manual bit masking is enough and removes a dependency. Decision: **skip `ipnet`**, use manual `u128` bitops.

---

## Non-Functional Requirements

### Security

- **Zero secrets in config files**: `env:VAR` pattern for all credentials (API tokens, TSIG keys).
- **Least privilege**: `netlink` method needs no special capabilities. `ra` method documents the exact `CAP_NET_RAW` requirement.
- **No network listeners**: the tool is client-only; no open ports.

### Reliability

- **Idempotent updates**: skip DNS API calls when address hasn't changed (cache in memory).
- **Retry with backoff**: provider API failures get exponential backoff (1s → 2s → 4s → … max 60s).
- **Graceful degradation**: if one host's update fails, continue with the next; report all errors at the end of the cycle.
- **Startup check**: on launch, immediately detect prefix and verify all cached addresses; update stale ones before entering the loop.

### Portability

- **Linux primary target** (netlink, raw ICMPv6, systemd integration).
- **macOS secondary**: `dns` method works everywhere; `ra` method may need `AF_INET6` + `IPPROTO_ICMPV6` tweaks (BSD uses `BIOCSETF` for BPF on raw sockets). `netlink` is Linux-only. Defer macOS `ra` support to v2.
- **Docker**: recommend `network_mode: host` for `netlink`/`ra` methods, or use `dns` method for standard bridged networking.
- **systemd service file** shipped in `contrib/` with appropriate `AmbientCapabilities` and `ProtectSystem=strict`.

---

## Project Structure

```
ddns-ipv6/
├── Cargo.toml
├── PLAN.md                    # this document
├── README.md                  # (write after implementation)
├── contrib/
│   └── ddns-ipv6.service      # systemd unit
├── config.example.toml        # annotated example config
└── src/
    ├── main.rs                # CLI, wiring, main loop
    ├── config.rs              # config types + TOML parsing
    ├── prefix.rs              # PrefixDetector trait + impls
    │   ├── dns.rs             #   DnsResolver
    │   ├── netlink.rs         #   NetlinkWatcher
    │   └── ra.rs              #   RaListener
    ├── updater.rs             # DnsUpdater trait + impls
    │   ├── cloudflare.rs      #   Cloudflare API
    │   └── rfc2136.rs         #   RFC 2136 nsupdate
    └── util.rs                # combine(), env resolution, address helpers
```

---

## Implementation Phases

### Phase 1: Skeleton + DNS Method + Cloudflare
- Cargo init, all dependencies, CLI (config path only).
- Config types + TOML deserialization.
- `DnsResolver` prefix detector.
- `CloudflareUpdater`.
- Main loop: poll → combine → compare → update.
- Verify end-to-end: `cargo run -- --config test.toml`.

### Phase 2: Netlink Method
- `NetlinkWatcher` via `rtnetlink`.
- Event-driven prefix changes (no polling).
- Integration test with a real interface.

### Phase 3: RFC 2136 Updater
- `hickory-proto` TSIG update message construction.
- UDP transport to authoritative server.
- Test against BIND/nsupdate-compatible server.

### Phase 4: Raw RA Method
- `socket2` raw ICMPv6 socket.
- RA + PIO parsing.
- `CAP_NET_RAW` documentation + systemd unit.

### Phase 5: Polish
- `SIGUSR1` force-refresh.
- Graceful shutdown (`SIGTERM` → finish current update cycle → exit).
- Error aggregation.
- `contrib/ddns-ipv6.service`.

---

## Open Questions

1. **Multiple prefixes on one interface?** The user's environment shows 3 global /64 prefixes on `enp3s0` (likely from multiple ISP delegations or prefix changes over time). The tool should use the **most recently acquired** global prefix (highest `preferred_lft`). When netlink reports a new address, we pick the one with the longest preferred lifetime among non-deprecated globals.

2. **Non-/64 prefixes?** Some ISPs delegate /60 or /56. The user configures subnetting on their router. The tool assumes the LAN-facing prefix is /64 (which is the SLAAC requirement). If a user has a non-/64 on the LAN, we'd need a configurable prefix length. **Decision**: hardcode /64 for v1; add `prefix_length` config field if requested.

3. **IPv6 temporary addresses (RFC 4941)?** The kernel creates temporary/privacy addresses with random suffixes. The tool ignores these — it only tracks `mngtmpaddr` (managed temporary) addresses whose prefixes come from RAs. The netlink watcher filters out `IFA_F_TEMPORARY`.

4. **Multiple DNS providers for different domains?** Some users might want `server-a` on Cloudflare but `server-b` on Route53. **Decision**: v1 uses one provider globally; add per-host `[hosts.provider]` override in v2 if requested.

---

## Known Risks

| Risk | Mitigation |
|---|---|
| `hickory-proto` TSIG support may be incomplete or poorly documented | Verify with integration test against BIND; fall back to shelling out to `nsupdate` if necessary |
| `rtnetlink` crate churn (netlink ecosystem is active) | Pin specific versions; the API surface we need is small (`AddressMessage` stream) |
| Raw ICMPv6 may require different approaches on different kernels | Feature-gate `ra` module behind `#[cfg(target_os = "linux")]` |
| ISP rotates prefix but old address remains preferred for hours (overlap) | Always pick the address with the **longest valid_lft** among non-temporary globals; both old and new prefixes coexist during transition — update DNS to the **new** prefix immediately |
