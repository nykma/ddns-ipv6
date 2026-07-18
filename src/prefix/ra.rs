use async_trait::async_trait;
use socket2::{Domain, Protocol, Socket, Type};
use std::mem::MaybeUninit;
use std::net::Ipv6Addr;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use crate::Error;

/// Detects the /64 prefix by listening for IPv6 Router Advertisements.
///
/// Requires `CAP_NET_RAW`. Set via `setcap cap_net_raw=+ep /path/to/ddns-ipv6`
/// or systemd `AmbientCapabilities=CAP_NET_RAW`.
pub struct RaListener {
    #[allow(dead_code)]
    interface: String,
    last_prefix: Arc<Mutex<Option<Ipv6Addr>>>,
    change_tx: watch::Sender<()>,
}

impl RaListener {
    pub fn new(interface: String) -> Result<Self, Error> {
        let (change_tx, _change_rx) = watch::channel(());
        let last_prefix: Arc<Mutex<Option<Ipv6Addr>>> = Arc::new(Mutex::new(None));

        let bg_interface = interface.clone();
        let bg_tx = change_tx.clone();
        let bg_last = last_prefix.clone();

        std::thread::spawn(move || {
            if let Err(e) = run_ra_listener(&bg_interface, bg_tx, bg_last) {
                error!(error = %e, "RA listener thread failed");
            }
        });

        Ok(Self {
            interface,
            last_prefix,
            change_tx,
        })
    }
}

fn run_ra_listener(
    interface: &str,
    tx: watch::Sender<()>,
    last_prefix: Arc<Mutex<Option<Ipv6Addr>>>,
) -> Result<(), Error> {
    let socket = Socket::new(Domain::IPV6, Type::RAW, Some(Protocol::ICMPV6))
        .map_err(|e| Error::Other(e.into()))?;

    set_icmp6_filter(&socket)?;
    bind_to_device(&socket, interface)?;

    let mut buf: [MaybeUninit<u8>; 1500] = unsafe { MaybeUninit::uninit().assume_init() };

    info!(
        interface,
        "RA listener started, waiting for Router Advertisements"
    );

    loop {
        match socket.recv(&mut buf) {
            Ok(n) => {
                // SAFETY: recv filled n bytes; transmute to initialized slice
                let data = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, n) };
                if let Some(prefix) = parse_ra_packet(data) {
                    debug!(%prefix, "RA prefix detected");
                    if let Ok(mut last) = last_prefix.lock() {
                        *last = Some(prefix);
                    }
                    let _ = tx.send(());
                }
            }
            Err(e) => {
                warn!(error = %e, "RA socket recv error");
            }
        }
    }
}

fn set_icmp6_filter(socket: &Socket) -> Result<(), Error> {
    let fd = socket.as_raw_fd();
    let mut filter = [0u32; 8];

    for v in filter.iter_mut() {
        *v = 0xFFFF_FFFF;
    }

    // Unblock type 134 (Router Advertisement)
    let word = 134 / 32;
    let bit = 134 % 32;
    filter[word as usize] &= !(1u32 << bit);

    let ret = unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_ICMPV6,
            1, // ICMP6_FILTER
            filter.as_ptr() as *const libc::c_void,
            std::mem::size_of_val(&filter) as libc::socklen_t,
        )
    };

    if ret != 0 {
        return Err(Error::Other(std::io::Error::last_os_error().into()));
    }

    Ok(())
}

fn bind_to_device(socket: &Socket, interface: &str) -> Result<(), Error> {
    let fd = socket.as_raw_fd();
    let mut ifname = [0u8; libc::IFNAMSIZ];
    let name_bytes = interface.as_bytes();
    if name_bytes.len() >= libc::IFNAMSIZ {
        return Err(Error::Other(anyhow::anyhow!("interface name too long")));
    }
    ifname[..name_bytes.len()].copy_from_slice(name_bytes);

    let ret = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            ifname.as_ptr() as *const libc::c_void,
            name_bytes.len() as libc::socklen_t,
        )
    };

    if ret != 0 {
        return Err(Error::Other(std::io::Error::last_os_error().into()));
    }

    Ok(())
}

/// Parse an RA packet and extract the first valid PIO prefix.
fn parse_ra_packet(data: &[u8]) -> Option<Ipv6Addr> {
    if data.len() < 16 {
        return None;
    }

    if data[0] != 134 {
        return None;
    }

    let mut offset = 16;

    while offset + 2 <= data.len() {
        let opt_type = data[offset];
        let opt_len = data[offset + 1] as usize;
        offset += 2;

        if opt_len == 0 {
            break;
        }

        let opt_data_len = opt_len * 8 - 2;
        if offset + opt_data_len > data.len() {
            break;
        }

        if opt_type == 3 {
            if opt_data_len < 30 {
                offset += opt_data_len;
                continue;
            }

            let flags = data[offset + 1];
            let a_flag = (flags & 0x40) != 0;

            if !a_flag {
                offset += opt_data_len;
                continue;
            }

            let prefix_start = offset + 14;
            if prefix_start + 16 > data.len() {
                break;
            }

            let mut segments = [0u16; 8];
            for i in 0..8 {
                let base = prefix_start + i * 2;
                segments[i] = u16::from_be_bytes([data[base], data[base + 1]]);
            }
            let prefix = Ipv6Addr::new(
                segments[0],
                segments[1],
                segments[2],
                segments[3],
                segments[4],
                segments[5],
                segments[6],
                segments[7],
            );

            if is_global_unicast(&prefix) {
                return Some(mask_to_64(&prefix));
            }
        }

        offset += opt_data_len;
    }

    None
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
impl super::PrefixDetector for RaListener {
    async fn detect(&self) -> Result<Ipv6Addr, Error> {
        if let Ok(guard) = self.last_prefix.lock() {
            if let Some(prefix) = *guard {
                return Ok(prefix);
            }
        }
        Err(Error::Prefix(
            "RA listener: no prefix received yet; waiting for Router Advertisement".into(),
        ))
    }

    fn changes(&self) -> watch::Receiver<()> {
        self.change_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal RA packet with a Prefix Information Option.
    /// ICMPv6 header: 4 bytes (type, code, checksum)
    /// RA body: 12 bytes (hop_limit, flags, router_lifetime, reachable_time, retrans_timer)
    /// PIO: 32 bytes (type=3, len=4, prefix_len, flags, valid, preferred, reserved, prefix)
    fn build_ra_packet(prefix: Ipv6Addr, a_flag: bool) -> Vec<u8> {
        let mut pkt = Vec::with_capacity(48);

        // ICMPv6 header
        pkt.push(134); // type = Router Advertisement
        pkt.push(0); // code
        pkt.extend_from_slice(&[0u8, 0]); // checksum (zero for test)

        // RA body (12 bytes)
        pkt.push(64); // cur_hop_limit
        pkt.push(0); // flags (M=0, O=0)
        pkt.extend_from_slice(&1800u16.to_be_bytes()); // router_lifetime
        pkt.extend_from_slice(&0u32.to_be_bytes()); // reachable_time
        pkt.extend_from_slice(&0u32.to_be_bytes()); // retrans_timer

        // PIO
        pkt.push(3); // type = Prefix Information
        pkt.push(4); // length = 4 (32 bytes)
        pkt.push(64); // prefix_length
        let flags: u8 = if a_flag { 0xC0 } else { 0x80 }; // A+L or L-only
        pkt.push(flags);
        pkt.extend_from_slice(&86400u32.to_be_bytes()); // valid_lifetime
        pkt.extend_from_slice(&14400u32.to_be_bytes()); // preferred_lifetime
        pkt.extend_from_slice(&[0u8; 4]); // reserved
        pkt.extend_from_slice(&prefix.octets()); // prefix (16 bytes)

        assert_eq!(pkt.len(), 48);
        pkt
    }

    #[test]
    fn parse_ra_with_global_pio() {
        let prefix: Ipv6Addr = "2001:db8:1:2::".parse().unwrap();
        let pkt = build_ra_packet(prefix, true);
        let result = parse_ra_packet(&pkt);
        assert!(result.is_some(), "should parse global prefix");
        assert_eq!(
            result.unwrap(),
            "2001:db8:1:2::".parse::<Ipv6Addr>().unwrap()
        );
    }

    #[test]
    fn parse_ra_without_a_flag() {
        let prefix: Ipv6Addr = "2001:db8:1:2::".parse().unwrap();
        let pkt = build_ra_packet(prefix, false);
        let result = parse_ra_packet(&pkt);
        assert!(
            result.is_none(),
            "should not return prefix when A flag is unset"
        );
    }

    #[test]
    fn parse_ra_non_global_prefix() {
        // fe80::/10 is link-local, not global unicast
        let prefix: Ipv6Addr = "fe80::".parse().unwrap();
        let pkt = build_ra_packet(prefix, true);
        let result = parse_ra_packet(&pkt);
        assert!(result.is_none(), "should skip link-local prefixes");
    }

    #[test]
    fn parse_ra_short_packet() {
        let result = parse_ra_packet(&[134, 0, 0, 0]);
        assert!(result.is_none());
    }

    #[test]
    fn parse_ra_wrong_type() {
        // ICMPv6 type 135 is Neighbor Solicitation
        let prefix: Ipv6Addr = "2001:db8:1:2::".parse().unwrap();
        let mut pkt = build_ra_packet(prefix, true);
        pkt[0] = 135;
        let result = parse_ra_packet(&pkt);
        assert!(result.is_none());
    }
}
