use async_trait::async_trait;
use hickory_resolver::Resolver;
use hickory_resolver::config::ResolverConfig;
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use std::net::Ipv6Addr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::Error;

/// Detects the /64 prefix by resolving a reference domain's AAAA record.
pub struct DnsResolver {
    reference_domain: String,
    resolver: Resolver<TokioRuntimeProvider>,
    change_tx: watch::Sender<()>,
}

impl DnsResolver {
    pub fn new(reference_domain: String, interval: Duration) -> Result<Self, Error> {
        let resolver = Resolver::builder_with_config(
            ResolverConfig::default(),
            TokioRuntimeProvider::default(),
        )
        .build()
        .map_err(|e| Error::Other(e.into()))?;

        let (change_tx, _change_rx) = watch::channel(());

        // Spawn background polling task
        let bg_resolver = resolver.clone();
        let bg_domain = reference_domain.clone();
        let bg_tx = change_tx.clone();
        let bg_last: Arc<tokio::sync::Mutex<Option<Ipv6Addr>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                match resolve_prefix_inner(&bg_resolver, &bg_domain).await {
                    Ok(new_prefix) => {
                        let mut last = bg_last.lock().await;
                        if *last != Some(new_prefix) {
                            info!(
                                old = ?*last,
                                new = %new_prefix,
                                "prefix changed (DNS poll)"
                            );
                            *last = Some(new_prefix);
                            let _ = bg_tx.send(());
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "DNS resolution failed in background poll");
                    }
                }
            }
        });

        Ok(Self {
            reference_domain,
            resolver,
            change_tx,
        })
    }
}

/// Check if this is a global unicast address (2000::/3).
fn is_global_unicast(addr: &Ipv6Addr) -> bool {
    let segments = addr.segments();
    (segments[0] & 0xe000) == 0x2000
}

/// Mask an Ipv6Addr to its /64 prefix (zero out lower 64 bits).
fn mask_to_64(addr: &Ipv6Addr) -> Ipv6Addr {
    let segments = addr.segments();
    Ipv6Addr::new(
        segments[0],
        segments[1],
        segments[2],
        segments[3],
        0,
        0,
        0,
        0,
    )
}

#[async_trait]
impl super::PrefixDetector for DnsResolver {
    async fn detect(&self) -> Result<Ipv6Addr, Error> {
        resolve_prefix_inner(&self.resolver, &self.reference_domain).await
    }

    fn changes(&self) -> watch::Receiver<()> {
        self.change_tx.subscribe()
    }
}

async fn resolve_prefix_inner(
    resolver: &Resolver<TokioRuntimeProvider>,
    domain: &str,
) -> Result<Ipv6Addr, Error> {
    let response = resolver
        .lookup_ip(domain)
        .await
        .map_err(|e| Error::Prefix(format!("DNS resolution failed: {e}")))?;

    for addr in response.iter() {
        if let std::net::IpAddr::V6(v6) = addr {
            if is_global_unicast(&v6) {
                return Ok(mask_to_64(&v6));
            }
        }
    }

    Err(Error::Prefix(format!(
        "no global unicast AAAA record found for {domain}"
    )))
}
