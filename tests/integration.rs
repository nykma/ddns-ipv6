//! Integration tests for ddns-ipv6.
//!
//! These tests require a real DNS setup and are skipped by default.
//! Run with: `cargo test -- --ignored`
//!
//! Required environment variables:
//! - For Cloudflare: `DDNS_CF_ZONE_ID`, `DDNS_CF_API_TOKEN`, `DDNS_TEST_DOMAIN`
//! - For RFC 2136: `DDNS_RFC2136_SERVER`, `DDNS_RFC2136_KEY_NAME`,
//!   `DDNS_RFC2136_KEY_ALGORITHM`, `DDNS_RFC2136_KEY_SECRET`, `DDNS_TEST_DOMAIN`

use std::net::Ipv6Addr;

use ddns_ipv6::DnsUpdater;

#[ignore = "requires Cloudflare API credentials"]
#[test]
fn cloudflare_set_and_get_record() {
    // This test requires:
    // - DDNS_CF_ZONE_ID env var
    // - DDNS_CF_API_TOKEN env var
    // - DDNS_TEST_DOMAIN env var (a real domain in the zone)
    //
    // It creates/updates an AAAA record and verifies it can be read back.
    let zone_id = std::env::var("DDNS_CF_ZONE_ID").ok();
    let api_token = std::env::var("DDNS_CF_API_TOKEN").ok();
    let domain = std::env::var("DDNS_TEST_DOMAIN").ok();

    let (Some(zone_id), Some(api_token), Some(domain)) = (zone_id, api_token, domain) else {
        eprintln!("skipping: missing env vars");
        return;
    };

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let updater = ddns_ipv6::updater::cloudflare::CloudflareUpdater::new(
            zone_id,
            api_token,
        );

        let test_addr: Ipv6Addr = "2001:db8:test::1".parse().unwrap();

        // Set the record
        updater
            .set_record(&domain, &test_addr)
            .await
            .expect("set_record failed");

        // Read it back
        let result = updater
            .get_record(&domain)
            .await
            .expect("get_record failed");

        assert_eq!(result, Some(test_addr), "record should match what was set");
    });
}

#[test]
#[ignore = "requires RFC 2136 server"]
fn rfc2136_set_and_get_record() {
    let server = std::env::var("DDNS_RFC2136_SERVER").ok();
    let key_name = std::env::var("DDNS_RFC2136_KEY_NAME").ok();
    let key_algorithm = std::env::var("DDNS_RFC2136_KEY_ALGORITHM").ok();
    let key_secret = std::env::var("DDNS_RFC2136_KEY_SECRET").ok();
    let domain = std::env::var("DDNS_TEST_DOMAIN").ok();

    let (Some(server), Some(key_name), Some(key_algorithm), Some(key_secret), Some(domain)) =
        (server, key_name, key_algorithm, key_secret, domain)
    else {
        eprintln!("skipping: missing env vars");
        return;
    };

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let updater = ddns_ipv6::updater::rfc2136::Rfc2136Updater::new(
            server,
            key_name,
            key_algorithm,
            key_secret,
        )
        .expect("failed to create Rfc2136Updater");

        let test_addr: Ipv6Addr = "2001:db8:test::1".parse().unwrap();

        updater
            .set_record(&domain, &test_addr)
            .await
            .expect("set_record failed");

        let result = updater
            .get_record(&domain)
            .await
            .expect("get_record failed");

        assert_eq!(result, Some(test_addr));
    });
}
