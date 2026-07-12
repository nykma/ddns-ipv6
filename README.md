# ddns-ipv6

A daemon that dynamically updates AAAA records when your ISP rotates the IPv6 prefix.

> [中文文档](README.zh-CN.md)

## What it does

When your ISP rotates the IPv6 prefix, every SLAAC-configured host on your LAN gets a new global address — and every AAAA record goes stale at once. `ddns-ipv6` detects the current `/64` prefix, combines it with per-host suffixes you configure, and batch-updates DNS records.

- **Three prefix detection methods**: DNS polling (resolve a reference domain's AAAA record), netlink address monitoring (watch interface IPv6 address changes), raw ICMPv6 RA capture (listen for Router Advertisement packets directly)
- **Two DNS update backends**: Cloudflare API (Bearer Token, per-zone), RFC 2136 dynamic update (TSIG-signed)
- **Signal control**: `SIGUSR1` triggers an immediate detection + update cycle; `SIGTERM`/`SIGINT` shuts down gracefully
- **Idempotent**: in-memory cache of current DNS state — skips API calls when the address hasn't changed

## When to use

Good fit when:

- Your ISP assigns a dynamic IPv6 prefix (PPPoE / DHCPv6-PD) that changes periodically
- Multiple hosts on your LAN need independent AAAA records (each has a fixed suffix, only the prefix changes)
- You use Cloudflare DNS or run a nameserver that supports RFC 2136 + TSIG
- Your runtime is Linux (netlink and RA detection are Linux-only; DNS method is cross-platform)

Not a good fit when:

- You only have one host needing DDNS (an IPv4 DDNS client is simpler)
- Your ISP provides a static IPv6 prefix
- You need RA detection but can't grant `CAP_NET_RAW`

## Configuration

```toml
[prefix]
method = "dns"                # dns | netlink | ra
reference_domain = "my-router.example.com"   # required when method = "dns"
# interface = "eth0"         # required when method = "netlink" or "ra"

[dns]
provider = "cloudflare"      # cloudflare | rfc2136

[dns.cloudflare]
zone_id = "abc123"
api_token = "env:CF_API_TOKEN"

# [dns.rfc2136]
# provider = "rfc2136"
# server = "tcp://ns1.example.com:53"
# key_name = "ddns-key."
# key_algorithm = "hmac-sha256"
# key_secret = "env:TSIG_SECRET"

[[hosts]]
suffix = "::1"
domain = "server-a.example.com"

[[hosts]]
suffix = "::dead:beef"
domain = "server-b.example.com"

interval_secs = 300   # polling interval for dns method only
```

- `api_token` and `key_secret` support `env:VAR` references resolved from environment variables
- `suffix` is each host's IPv6 suffix (only the lower 64 bits are used)
- Add multiple hosts via repeated `[[hosts]]` entries

## Deployment

### Direct

```bash
cargo build --release
./target/release/ddns-ipv6 --config /etc/ddns-ipv6/config.toml
```

### systemd

```bash
sudo cp contrib/ddns-ipv6.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now ddns-ipv6
```

- DNS method: no special privileges needed
- Netlink method: no special privileges needed
- RA method: uncomment `AmbientCapabilities=CAP_NET_RAW` in the service file

### Docker

#### Method 1: DNS detection (simplest)

The DNS detection method only makes outbound DNS queries and API calls — the default bridge network is sufficient.

```yaml
# docker-compose.yml
services:
  ddns-ipv6:
    image: ghcr.io/nykma/ddns-ipv6:latest
    container_name: ddns-ipv6
    restart: unless-stopped
    volumes:
      - ./config.toml:/etc/ddns-ipv6/config.toml:ro
    environment:
      - RUST_LOG=info
      - CF_API_TOKEN=${CF_API_TOKEN}
```

Use `method = "dns"` in `config.toml`. The container reaches the internet through the default bridge for DNS resolution and Cloudflare API calls.

#### Method 2: Netlink / RA detection (macvlan required)

Netlink and RA detection need direct access to the host's network interface. The container must join a macvlan network. A macvlan-connected container appears as an independent host on the LAN with its own MAC address, and can receive SLAAC prefixes and RA packets from the router.

**Step 1: Create the macvlan network**

Find your physical interface name (`ip link show`) and your LAN subnet details.

```bash
docker network create \
  --driver macvlan \
  --opt parent=eth0 \
  --ipv6 \
  --subnet 10.0.0.0/16 \
  --gateway 10.0.0.1 \
  --subnet fd0d:7eb5:2afd::/64 \
  --gateway fd0d:7eb5:2afd::1 \
  ipv6-host
```

- `--opt parent=eth0`: the host's physical interface
- `--ipv6`: enable IPv6 (requires `{"ipv6": true}` in `/etc/docker/daemon.json` and a Docker restart)
- Replace subnet and gateway values with your LAN's actual configuration
- `macvlan_mode` defaults to `bridge` — no need to specify it

The resulting network structure:

```
Driver: macvlan
EnableIPv6: true
IPAM: 10.0.0.0/16 (gateway 10.0.0.1) + fd0d:7eb5:2afd::/64 (gateway fd0d:7eb5:2afd::1)
Options: parent=eth0
```

**Step 2: docker-compose.yml**

```yaml
services:
  ddns-ipv6:
    image: ghcr.io/nykma/ddns-ipv6:latest
    container_name: ddns-ipv6
    restart: unless-stopped
    networks:
      ipv6-host:
        ipv4_address: 10.0.3.1
    dns:
      - 10.0.0.1
    volumes:
      - ./config.toml:/etc/ddns-ipv6/config.toml:ro
    environment:
      - RUST_LOG=info
      - CF_API_TOKEN=${CF_API_TOKEN}
    # For RA method, also uncomment:
    # cap_add:
    #   - NET_RAW

networks:
  ipv6-host:
    external: true
```

- `ipv4_address`: a static IPv4 for outbound traffic (DNS lookups, API calls). Pick an unused address from the macvlan subnet
- No `ipv6_address` needed — the container gets one via SLAAC automatically. `ddns-ipv6` reads the prefix directly from the interface
- `dns`: DNS server for the container (typically your router or LAN gateway)
- `networks.<name>.external: true`: references the network created in Step 1

**Step 3: config.toml**

```toml
# netlink — watches interface address changes
[prefix]
method = "netlink"
interface = "eth0"

# or RA — captures Router Advertisement packets
# [prefix]
# method = "ra"
# interface = "eth0"
```

For the RA method, uncomment `cap_add: [NET_RAW]` in docker-compose.yml.

### Notes

- **macvlan host isolation**: In bridge mode, the host cannot directly reach macvlan containers. To access them from the host, create an additional macvlan sub-interface on the host and add a route.
- **IPv6 forwarding**: If the macvlan subnet differs from the host's subnet, enable IPv6 forwarding: `sysctl -w net.ipv6.conf.all.forwarding=1`.
- **RA method**: Requires `CAP_NET_RAW` (Docker: `cap_add: [NET_RAW]`, systemd: `AmbientCapabilities=CAP_NET_RAW`). In containerized deployments, also ensure multicast RA packets reach the macvlan interface.

#### Container auto-discovery

Instead of listing every container's suffix in `[[hosts]]`, let the daemon discover them automatically through the Docker API. Add a `ddns.domain` label to each target container:

```yaml
services:
  my-app:
    image: my-app:latest
    networks:
      ipv6-host:
    labels:
      ddns.domain: "my-app.example.com"

  ddns-ipv6:
    image: ghcr.io/nykma/ddns-ipv6:latest
    restart: unless-stopped
    networks:
      ipv6-host:
    volumes:
      - ./config.toml:/etc/ddns-ipv6/config.toml:ro
      - /var/run/docker.sock:/var/run/docker.sock:ro
    environment:
      - RUST_LOG=info
      - CF_API_TOKEN=${CF_API_TOKEN}
```

Then enable discovery in `config.toml`:

```toml
[docker]
enabled = true
```

- The daemon inspects each labeled container's network settings for a global unicast IPv6 address and extracts the lower 64 bits as the suffix.
- Discovered hosts merge with static `[[hosts]]` entries — you can use both.
- When a container stops, its domain silently drops from the next update cycle. No DNS records are deleted.

**Permissions:** Mounting `docker.sock` grants effective root access to the host. For least-privilege setups, use [docker-socket-proxy](https://github.com/Tecnativa/docker-socket-proxy) and set `socket_path` to the proxy endpoint:

```toml
[docker]
enabled = true
socket_path = "tcp://docker-proxy:2375"
```


## Build

```bash
cargo build --release
cargo test
```

## License

MIT
