use async_trait::async_trait;
use tokio::sync::watch;

#[async_trait]
pub trait PrefixDetector: Send + Sync {
    /// Returns the current /64 prefix network address (upper 64 bits set, lower 64 zero).
    async fn detect(&self) -> Result<std::net::Ipv6Addr, crate::Error>;

    /// Returns a receiver that yields `()` on each prefix change (or tick for polling methods).
    fn changes(&self) -> watch::Receiver<()>;
}

pub mod dns;
#[cfg(target_os = "linux")]
pub mod netlink;
#[cfg(target_os = "linux")]
pub mod ra;
