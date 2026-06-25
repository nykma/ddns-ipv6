use async_trait::async_trait;
use std::net::Ipv6Addr;

#[async_trait]
pub trait DnsUpdater: Send + Sync {
    async fn set_record(&self, domain: &str, addr: &Ipv6Addr) -> Result<(), crate::Error>;
    async fn get_record(&self, domain: &str) -> Result<Option<Ipv6Addr>, crate::Error>;
}

pub mod cloudflare;
pub mod rfc2136;
