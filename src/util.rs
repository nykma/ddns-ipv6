use std::net::Ipv6Addr;
use tracing::warn;

/// Mask an Ipv6Addr to its lower 64 bits.
/// Warns if the upper 64 bits were set (likely a full address, not a suffix).
pub fn suffix_from_addr(addr: &Ipv6Addr) -> Ipv6Addr {
    let segments = addr.segments();
    // Lower 64 bits = segments 4-7
    let lower = Ipv6Addr::new(
        0,
        0,
        0,
        0,
        segments[4],
        segments[5],
        segments[6],
        segments[7],
    );
    let upper = Ipv6Addr::new(
        segments[0],
        segments[1],
        segments[2],
        segments[3],
        0,
        0,
        0,
        0,
    );
    if !upper.is_unspecified() {
        warn!(
            upper = %upper,
            "suffix has upper 64 bits set; masking to lower 64 bits only"
        );
    }
    lower
}

/// Return true if the address is a global unicast IPv6 address (2000::/3).
pub fn is_global_unicast(addr: &Ipv6Addr) -> bool {
    let segments = addr.segments();
    (0x2000..=0x3FFF).contains(&segments[0])
}

/// Combine a /64 prefix (upper 64 bits of an Ipv6Addr) with a suffix (lower 64 bits).
///
/// # Panics
///
/// Panics if the lower 64 bits of `prefix` or the upper 64 bits of `suffix` are non-zero,
/// as that indicates a logic error in the caller.
pub fn combine(prefix: &Ipv6Addr, suffix: &Ipv6Addr) -> Ipv6Addr {
    let p = prefix.segments();
    let s = suffix.segments();

    // Prefix must have zero lower bits; suffix must have zero upper bits.
    debug_assert_eq!(
        Ipv6Addr::new(0, 0, 0, 0, p[4], p[5], p[6], p[7]),
        Ipv6Addr::UNSPECIFIED,
        "prefix lower 64 bits should be zero"
    );
    debug_assert_eq!(
        Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0),
        Ipv6Addr::UNSPECIFIED,
        "suffix upper 64 bits should be zero"
    );

    Ipv6Addr::new(p[0], p[1], p[2], p[3], s[4], s[5], s[6], s[7])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    #[test]
    fn combine_basic() {
        let prefix: Ipv6Addr = "2001:db8:1:2::".parse().unwrap();
        let suffix: Ipv6Addr = "::1".parse().unwrap();
        let result = combine(&prefix, &suffix);
        assert_eq!(result.to_string(), "2001:db8:1:2::1");
    }

    #[test]
    fn combine_dead_beef() {
        let prefix: Ipv6Addr = "2001:db8:1:2::".parse().unwrap();
        let suffix: Ipv6Addr = "::dead:beef".parse().unwrap();
        let result = combine(&prefix, &suffix);
        assert_eq!(result.to_string(), "2001:db8:1:2::dead:beef");
    }

    #[test]
    fn suffix_from_addr_full_address() {
        let addr: Ipv6Addr = "2001:db8:1:2:3:4:5:6".parse().unwrap();
        let suffix = suffix_from_addr(&addr);
        assert_eq!(suffix.to_string(), "::3:4:5:6");
    }

    #[test]
    fn suffix_from_addr_pure_suffix() {
        let addr: Ipv6Addr = "::dead:beef".parse().unwrap();
        let suffix = suffix_from_addr(&addr);
        assert_eq!(suffix.to_string(), "::dead:beef");
    }
}
