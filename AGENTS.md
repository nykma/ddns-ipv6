# Repository Guidelines

## Project Overview

**ddns-ipv6** is a Dynamic DNS updater for IPv6. It detects the current IPv6 `/64` prefix on the local machine, combines it with per-host suffixes to form full IPv6 addresses, and updates AAAA DNS records when the prefix changes. It supports three prefix detection strategies (DNS, Netlink, RA) and two DNS providers (Cloudflare API, RFC 2136 nsupdate).

## Architecture & Data Flow

```
Config (TOML) → build_detector() → Arc<dyn PrefixDetector>  ─┐
                build_updater()  → Arc<dyn DnsUpdater>       │
                                                            │
  ┌─────────────────────────────────────────────────────────┘
  ▼
main loop: tokio::select!
  ├── detector.changes() (watch::Receiver)
  ├── SIGUSR1 force-refresh (Unix)
  └── CancellationToken shutdown
        │
        ▼
  run_update_cycle(detector, updater, hosts)
    1. detector.detect() → Ipv6Addr /64 prefix
    2. for each (suffix, domain) in hosts:
       util::combine(prefix, suffix) → full IPv6
       skip if cache matches
       updater.set_record(domain, addr)
       update cache (HashMap<domain, addr>)
```

### Traits

- **`PrefixDetector`** (`src/prefix.rs`): `detect() -> Ipv6Addr`, `changes() -> watch::Receiver<()>`
- **`DnsUpdater`** (`src/updater.rs`): `set_record(domain, addr)`, `get_record(domain) -> Option<Ipv6Addr>`

Both are `Send + Sync` + `#[async_trait]`. Both return `Result<_, Error>` (from `error.rs`).

### Prefix Detectors

| Implementation | Platform | Mechanism |
|---|---|---|
| `RaListener` | Linux only | Raw ICMPv6 socket (type 134), `std::thread`, passive |
| `NetlinkWatcher` | Linux only | `nlink` netlink query + multicast events, tokio async |
| `DnsResolver` | All | Periodic AAAA DNS poll via `hickory-resolver`, tokio async |

### DNS Updaters

| Implementation | Mechanism | Idempotency |
|---|---|---|
| `CloudflareUpdater` | Cloudflare API v4, `reqwest` with rustls, JSON | Checks current record first, skips if matches |
| `Rfc2136Updater` | RFC 2136 via `dns-update` crate, TSIG auth | Checks current record first, skips if matches |

## Key Directories

| Path | Purpose |
|---|---|
| `src/` | Application source (single binary crate) |
| `src/prefix/` | Prefix detection: `ra.rs`, `netlink.rs`, `dns.rs` |
| `src/updater/` | DNS record updaters: `rfc2136.rs`, `cloudflare.rs` |
| `tests/` | Integration tests (all `#[ignore]`d, require live services) |
| `contrib/` | Systemd unit file |
| `docs/solutions/` | Archived learnings |
| `.github/workflows/` | CI: multi-arch Docker build & push to GHCR |

## Development Commands

```bash
# Build
cargo build
cargo build --release --locked

# Test (unit tests only, fast)
cargo test

# Integration tests (require credentials + live DNS)
cargo test -- --ignored

# Lint
cargo clippy

# Format
cargo fmt

# Run locally
cargo run -- --config config.toml

# Nix dev shell (provides rustc, cargo, clippy, rustfmt, rust-analyzer)
nix develop
```

### Environment Variables for Integration Tests

| Variable | Test |
|---|---|
| `DDNS_CF_ZONE_ID` | Cloudflare |
| `DDNS_CF_API_TOKEN` | Cloudflare |
| `DDNS_TEST_DOMAIN` | Both |
| `DDNS_RFC2136_SERVER` | RFC 2136 |
| `DDNS_RFC2136_KEY_NAME` | RFC 2136 |
| `DDNS_RFC2136_KEY_ALGORITHM` | RFC 2136 |
| `DDNS_RFC2136_KEY_SECRET` | RFC 2136 |

## Code Conventions & Common Patterns

### Error Handling

Two enums in `src/error.rs`, both `#[derive(Error, Debug)]` via `thiserror`:

- **`Error`**: `Config(ConfigError)`, `Prefix(String)`, `Update { domain, source }`, `Io(std::io::Error)`, `Other(anyhow::Error)`
- **`ConfigError`**: `Io`, `Parse(toml::de::Error)`, `MissingField(String)`, `EnvNotSet(String)`, `EmptyHosts`, `InvalidSuffix(String, String)`, `UnknownAlgorithm(String)`

`main()` returns `Result<(), Box<dyn std::error::Error>>`. Internal fns return `Result<_, Error>`.

### Async Runtime

- `tokio` with `#[tokio::main]`, full features.
- `tokio::select!` in main loop.
- `tokio::spawn` for signal handlers and background tasks.
- `tokio_util::sync::CancellationToken` for graceful shutdown.
- `tokio::sync::Notify` for SIGUSR1 force-refresh.
- `tokio::sync::watch` channel for prefix change detection (trait contract).

### Dependency Injection

Concrete implementations are constructed in `build_detector()` / `build_updater()` factory functions and passed as `Arc<dyn Trait>` — main loop operates only on the trait interfaces.

### Configuration

- File: TOML, path from `--config` CLI arg (default `config.toml`).
- **Manual parsing** with `toml::Table` + field extraction — no `serde` derive. Allows custom `env:VAR` secret resolution.
- Secrets: values starting with `env:` resolve from process environment via `resolve_env_str()`.
- `config.sample.toml` documents all keys and accepted values. See `src/config.rs` for the `Config`, `PrefixConfig`, `ProviderConfig`, `HostEntry` types.

### Logging

- `tracing` + `tracing-subscriber` (env-filter, json features).
- Structured key-value pairs: `info!(domain, %new_addr, "updated AAAA record")`.
- TTY → pretty output; non-TTY → JSON to stderr.
- `RUST_LOG` env var controls filter, default `info`.

### Platform Gating

Netlink and RA detectors are `#[cfg(target_os = "linux")]`. Non-Linux builds get a compile-time config error for those methods. DNS resolver is cross-platform.

### IPv6 Utilities (`src/util.rs`)

- `suffix_from_addr(&Ipv6Addr) -> Ipv6Addr` — mask to lower 64 bits.
- `combine(prefix, suffix) -> Ipv6Addr` — upper 64 from prefix, lower 64 from suffix.
- `debug_assert!` guards enforce invariants in debug builds.

### Cache Pattern

`run_update_cycle()` maintains a `HashMap<String, Ipv6Addr>` (domain → address). At startup, seeds from live DNS. Skips `set_record()` calls when address is unchanged.

### Duplication to Note

`is_global_unicast()` and `mask_to_64()` are identically duplicated across `ra.rs`, `netlink.rs`, and `dns.rs`.

## Important Files

| File | Role |
|---|---|
| `src/main.rs` | Entry point, CLI, main loop, factory functions |
| `src/lib.rs` | Crate root, re-exports, public module declarations |
| `src/config.rs` | Config structs + TOML parser |
| `src/error.rs` | `Error` and `ConfigError` enums |
| `src/util.rs` | IPv6 address manipulation |
| `src/prefix.rs` | `PrefixDetector` trait |
| `src/prefix/ra.rs` | RA listener (raw ICMPv6 socket) |
| `src/prefix/netlink.rs` | Netlink watcher (IPv6 address monitor) |
| `src/prefix/dns.rs` | DNS resolver (periodic AAAA poll) |
| `src/updater.rs` | `DnsUpdater` trait |
| `src/updater/cloudflare.rs` | Cloudflare API updater |
| `src/updater/rfc2136.rs` | RFC 2136 nsupdate updater |
| `Cargo.toml` | Single-crate package, edition 2024, no workspace |
| `config.sample.toml` | Documented config template |
| `Dockerfile` | Multi-stage: `rust:1.96-slim-bookworm` build, `debian:bookworm-slim` runtime |
| `docker-compose.yml` | macvlan network, env vars, volume mount |
| `.github/workflows/docker-build.yml` | Multi-arch Docker build (amd64, arm64) → GHCR |
| `flake.nix` | Nix dev shell with fenix Rust toolchain |
| `contrib/ddns-ipv6.service` | Systemd unit with sandboxing |
| `CONCEPTS.md` | Domain glossary (Prefix, Suffix, Combine, Detector, Updater) |
| `PLAN.md` | Full technical plan with ICMPv6 packet diagrams |

## Runtime / Tooling Preferences

- **Rust edition**: 2024
- **Async runtime**: `tokio` (full features)
- **TLS**: `rustls` (via `reqwest`, no OpenSSL dependency at runtime)
- **Package manager**: Cargo (no workspace, single crate)
- **CI**: GitHub Actions — multi-arch Docker builds to `ghcr.io/nykma/ddns-ipv6`
- **Nix**: `fenix` for toolchain, `nixfmt` as formatter; devShell only (no package output)
- **Container**: `debian:bookworm-slim` runtime, unprivileged user `65534:65534`
- **Systemd**: `DynamicUser=yes`, `ProtectSystem=strict`, `ProtectHome=true`

## Testing & QA

- **Unit tests**: In-source (`#[cfg(test)] mod tests`) — `util.rs` has 4, `ra.rs` has 5.
- **Integration tests**: `tests/integration.rs` — both `#[ignore]`d, require real DNS credentials.
  - `cloudflare_set_and_get_record` — creates AAAA via API, reads back, asserts.
  - `rfc2136_set_and_get_record` — nsupdate AAAA, reads back, asserts.
- **No mocking layer** — integration tests are end-to-end against live services.
- Run unit tests with `cargo test`, integration with `cargo test -- --ignored`.
- CI does **not** run `cargo test` — Docker build only.
