use async_trait::async_trait;
use nlink::NetworkEvent;
use nlink::netlink::{Connection, Route, RtnetlinkGroup};
use std::net::{IpAddr, Ipv6Addr};
use tokio::sync::watch;
use tokio_stream::StreamExt;
use tracing::{debug, error, info, warn};

use crate::Error;

/// Detects the /64 prefix by monitoring IPv6 addresses on an interface via netlink.
pub struct NetlinkWatcher {
    interface: String,
    change_tx: watch::Sender<()>,
}

impl NetlinkWatcher {
    pub fn new(interface: String) -> Result<Self, Error> {
        let (change_tx, _change_rx) = watch::channel(());

        // Spawn background event listener
        let bg_tx = change_tx.clone();

        tokio::spawn(async move {
            if let Err(e) = run_event_loop(bg_tx).await {
                error!(error = %e, "netlink event loop failed");
            }
        });

        Ok(Self {
            interface,
            change_tx,
        })
    }
}

async fn run_event_loop(tx: watch::Sender<()>) -> Result<(), Error> {
    let conn = Connection::<Route>::new().map_err(|e| Error::Other(e.into()))?;

    conn.subscribe(&[RtnetlinkGroup::Ipv6Addr])
        .map_err(|e| Error::Other(e.into()))?;

    let mut events = conn.into_events().await;

    while let Some(event) = events.next().await {
        match event {
            Ok(NetworkEvent::NewAddress(addr_msg)) => {
                if let Some(IpAddr::V6(_)) = addr_msg.address() {
                    debug!("IPv6 address change detected via netlink");
                    let _ = tx.send(());
                }
            }
            Ok(NetworkEvent::DelAddress(addr_msg)) => {
                if let Some(IpAddr::V6(_)) = addr_msg.address() {
                    debug!("IPv6 address removed via netlink");
                    let _ = tx.send(());
                }
            }
            Ok(_) => {}
            Err(e) => {
                warn!(error = %e, "netlink event error");
            }
        }
    }

    info!("netlink event stream ended");
    Ok(())
}

async fn detect_prefix(interface: &str) -> Result<Ipv6Addr, Error> {
    let conn = Connection::<Route>::new().map_err(|e| Error::Other(e.into()))?;

    let addresses = conn
        .get_addresses_by_name(interface)
        .await
        .map_err(|e| Error::Prefix(format!("netlink: failed to list addresses: {e}")))?;

    let mut candidates: Vec<(&Ipv6Addr, u32, u32)> = Vec::new();

    for addr_msg in &addresses {
        if !addr_msg.is_ipv6() {
            continue;
        }
        if let Some(IpAddr::V6(v6)) = addr_msg.address() {
            if !is_global_unicast(v6) {
                continue;
            }
            // Skip temporary privacy addresses
            if addr_msg.is_secondary() && !addr_msg.is_permanent() {
                continue;
            }

            let cache = addr_msg.cache_info();
            let valid_lft = cache.map(|c| c.valid).unwrap_or(u32::MAX);
            let preferred_lft = cache.map(|c| c.preferred).unwrap_or(u32::MAX);

            candidates.push((v6, valid_lft, preferred_lft));
        }
    }

    if candidates.is_empty() {
        return Err(Error::Prefix(format!(
            "no global unicast IPv6 address found on interface '{interface}'"
        )));
    }

    candidates.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.2.cmp(&a.2)));

    let best = candidates[0].0;
    let prefix = mask_to_64(best);

    info!(interface, prefix = %prefix, "detected prefix via netlink");
    Ok(prefix)
}

fn is_global_unicast(addr: &Ipv6Addr) -> bool {
    let segments = addr.segments();
    (segments[0] & 0xe000) == 0x2000
}

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
impl super::PrefixDetector for NetlinkWatcher {
    async fn detect(&self) -> Result<Ipv6Addr, Error> {
        detect_prefix(&self.interface).await
    }

    fn changes(&self) -> watch::Receiver<()> {
        self.change_tx.subscribe()
    }
}
