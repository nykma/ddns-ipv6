# Concepts

Shared domain vocabulary for this project — entities, named processes, and status concepts with project-specific meaning. Seeded with core domain vocabulary, then accretes as ce-compound and ce-compound-refresh process learnings; direct edits are fine. Glossary only, not a spec or catch-all.

## Addressing Model

### Prefix
The upper 64 bits of an IPv6 address, assigned by the ISP and shared by all hosts on the LAN. When the ISP rotates the prefix, every host's global address changes — the daemon's job is to detect the new prefix and update AAAA records accordingly.

### Suffix
The lower 64 bits of an IPv6 address, per-host and stable across prefix rotations. Configured by the user as a per-host value in `[[hosts]]`. The suffix survives ISP prefix changes; only the prefix portion of the address changes.

### Combine
The operation `prefix + suffix → full IPv6 address`. The daemon never stores full addresses — it computes them on demand from the currently detected prefix and the configured per-host suffixes. Implemented in `src/util.rs` as `combine(prefix, suffix)`.

## Plugin Architecture

### Prefix Detector
A trait (`PrefixDetector`) that produces the current ISP IPv6 prefix and signals when it changes. Three implementations exist: DNS resolution of a reference domain's AAAA record (polling), netlink address monitoring on a local interface (event-driven), and raw ICMPv6 Router Advertisement capture (event-driven). Selected via `[prefix] method = "dns" | "netlink" | "ra"` in the config.

### DNS Updater
A trait (`DnsUpdater`) that reads and writes AAAA DNS records for a given domain. Two implementations exist: Cloudflare API (zone-scoped, bearer-token auth) and RFC 2136 dynamic update with TSIG authentication (per-zone, shared-secret auth). Selected via `[dns] provider = "cloudflare" | "rfc2136"` in the config.

## Configuration

### Host Entry
A configured mapping of suffix → domain that the daemon keeps updated. Each entry specifies a per-host IPv6 suffix and the fully qualified domain name whose AAAA record should track it. Defined in the `[[hosts]]` array in the config file. The daemon skips updates when the DNS record already matches the computed address.
