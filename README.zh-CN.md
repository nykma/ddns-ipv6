# ddns-ipv6

动态更新 AAAA 记录的 IPv6 DDNS 守护进程。

## 功能

当 ISP 轮转 IPv6 前缀时，局域网内所有使用 SLAAC 的主机的全球单播地址都会变化，导致 AAAA 记录全部过期。`ddns-ipv6` 检测当前 ISP 分配的 `/64` 前缀，与用户为每台主机配置的固定后缀组合出完整地址，批量更新 DNS 记录。

- **三种前缀检测方式**：DNS 轮询（解析参考域名的 AAAA 记录）、netlink 地址监听（监控本机接口的 IPv6 地址变化）、原始 ICMPv6 RA 监听（直接捕获路由器通告报文）
- **两种 DNS 更新后端**：Cloudflare API（Bearer Token 认证，按 Zone 操作）、RFC 2136 动态更新（TSIG 签名认证）
- **信号控制**：`SIGUSR1` 立即触发检测+更新，`SIGTERM`/`SIGINT` 优雅退出
- **幂等**：内存缓存当前 DNS 状态，地址未变时跳过 API 调用

## 适用范围

适用于以下场景：

- ISP 提供动态 IPv6 前缀（PPPoE / DHCPv6-PD），前缀不定期变化
- 局域网内多台主机需要各自独立的 AAAA 记录（每台主机的后缀固定，前缀跟随 ISP）
- 有 Cloudflare 托管的域名，或者有自建 DNS 服务器（支持 RFC 2136 + TSIG）
- 运行环境为 Linux（netlink 和 RA 方式仅 Linux 可用；DNS 方式跨平台）

不适合的场景：

- 只有一台主机需要 DDNS（IPv4 DDNS 客户端足够）
- ISP 提供固定 IPv6 前缀
- 运行环境不支持 `CAP_NET_RAW` 且必须使用 RA 检测方式

## 配置

```toml
[prefix]
method = "dns"                # dns | netlink | ra
reference_domain = "my-router.example.com"   # method = dns 时需要
# interface = "eth0"         # method = netlink 或 ra 时需要

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

interval_secs = 300   # DNS 轮询间隔（仅 method = dns 时生效）
```

- `api_token` 和 `key_secret` 支持 `env:VAR` 格式，从环境变量读取
- `suffix` 为每个主机的 IPv6 后缀（仅低 64 位有效）
- 多台主机通过 `[[hosts]]` 数组配置

## 部署

### 直接运行

```bash
# 编译
cargo build --release

# 运行
./target/release/ddns-ipv6 --config /etc/ddns-ipv6/config.toml
```

### systemd 服务

```bash
sudo cp contrib/ddns-ipv6.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now ddns-ipv6
```

- DNS 方式无需额外权限
- netlink 方式无需额外权限
- RA 方式需要在 service 文件中取消 `AmbientCapabilities=CAP_NET_RAW` 注释

### Docker 部署

#### 方式一：DNS 检测（最简单）

DNS 检测方式只做 DNS 查询和 API 调用，容器使用默认 bridge 网络即可。

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

`config.toml` 中使用 `method = "dns"`，容器通过默认网络出站即可完成 DNS 解析和 Cloudflare API 调用。

#### 方式二：netlink / RA 检测（需要 macvlan 网络）

netlink 和 RA 方式需要直接访问宿主机的网络接口，容器必须接入 macvlan 网络。macvlan 让容器在二层网络上表现为独立主机，拥有自己的 MAC 地址，可以直接接收路由器发送的 SLAAC 前缀和 RA 报文。

**步骤 1：创建 macvlan 网络**

确认宿主机的物理接口名称（`ip link show`），以及 LAN 的子网信息。

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

- `--opt parent=eth0`：宿主机的物理接口
- `--ipv6`：启用 IPv6（需先在 `/etc/docker/daemon.json` 中设置 `{"ipv6": true}` 并重启 Docker）
- 子网和网关替换为你的 LAN 实际值
- 不需要指定 `macvlan_mode`，默认就是 bridge

创建后的网络结构：
```
Driver: macvlan
EnableIPv6: true
IPAM: 10.0.0.0/16 (gateway 10.0.0.1) + fd0d:7eb5:2afd::/64 (gateway fd0d:7eb5:2afd::1)
Options: parent=eth0
```

**步骤 2：docker-compose.yml**

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
    # RA 方式额外需要：
    # cap_add:
    #   - NET_RAW

networks:
  ipv6-host:
    external: true
```

- `ipv4_address`：静态 IPv4（用于 DNS 解析和 API 调用等出站流量），从 macvlan 子网中选一个未使用的地址
- IPv6 地址**不需要手动分配** — 容器会通过 SLAAC 自动获取（ddns-ipv6 用 netlink/RA 检测时会从接口上直接读取）
- `dns`：指定 DNS 服务器（通常是路由器或 LAN 网关）
- `networks.<name>.external: true`：引用步骤 1 创建的外部网络

**步骤 3：config.toml 中使用对应检测方式**

```toml
# netlink 方式 — 监控本机接口的地址变化
[prefix]
method = "netlink"
interface = "eth0"

# 或 RA 方式 — 捕获路由器通告报文
# [prefix]
# method = "ra"
# interface = "eth0"
```

RA 方式需要在 docker-compose.yml 中取消 `cap_add: [NET_RAW]` 注释。

### 注意事项

- **macvlan 与宿主机隔离**：macvlan bridge 模式下，宿主机默认无法直接访问 macvlan 容器。如需从宿主机访问，需在宿主机上创建额外的 macvlan 子接口并配置路由。
- **IPv6 内核转发**：如果 macvlan 子网与宿主机不在同一网段，需在宿主机上启用 IPv6 转发：`sysctl -w net.ipv6.conf.all.forwarding=1`。
- **RA 方式**：需要 `CAP_NET_RAW`（Docker: `cap_add: [NET_RAW]`，systemd: `AmbientCapabilities=CAP_NET_RAW`）。容器化部署时还需确保 macvlan 接口能收到多播 RA 报文。

## 编译

```bash
cargo build --release
cargo test
```

## 许可

MIT
