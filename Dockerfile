# Stage 1: Build
FROM rust:1.96-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src/ ./src/

RUN cargo build --release --locked && \
    cp target/release/ddns-ipv6 /usr/local/bin/ddns-ipv6

# Stage 2: Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/ddns-ipv6 /usr/local/bin/ddns-ipv6

# RA listener requires CAP_NET_RAW to open raw ICMPv6 sockets.
# Unprivileged containers can't grant this — use `cap_add: NET_RAW` in
# docker-compose, or stick to the dns / netlink detection methods.

USER 65534:65534
ENTRYPOINT ["ddns-ipv6"]
CMD ["--config", "/etc/ddns-ipv6/config.toml"]
