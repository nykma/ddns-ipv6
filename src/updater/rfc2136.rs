use async_trait::async_trait;
use dns_update::{DnsRecord, DnsRecordType, DnsUpdater as DnsUpdateClient, TsigAlgorithm};
use std::net::Ipv6Addr;
use tracing::{debug, info};

use crate::{ConfigError, Error};

pub struct Rfc2136Updater {
    updater: DnsUpdateClient,
}

impl Rfc2136Updater {
    pub fn new(
        server: String,
        key_name: String,
        key_algorithm: String,
        key_secret: String,
    ) -> Result<Self, Error> {
        let algorithm = match key_algorithm.as_str() {
            "hmac-sha256" => TsigAlgorithm::HmacSha256,
            "hmac-sha512" => TsigAlgorithm::HmacSha512,
            other => {
                return Err(Error::Config(ConfigError::UnknownAlgorithm(
                    other.to_string(),
                )))
            }
        };

        let updater = DnsUpdateClient::new_rfc2136_tsig(
            server.as_str(),
            key_name,
            key_secret.into_bytes(),
            algorithm,
        )
        .map_err(|e| Error::Other(e.into()))?;

        Ok(Self { updater })
    }

    /// Extract the zone (origin) from a fully qualified domain name.
    /// e.g. "server-a.example.com" -> "example.com"
    fn zone_from_domain(domain: &str) -> &str {
        // Find the first dot, return everything after it.
        // If no dot, return the whole domain.
        domain.find('.').map(|i| &domain[i + 1..]).unwrap_or(domain)
    }
}

#[async_trait]
impl super::DnsUpdater for Rfc2136Updater {
    async fn get_record(&self, domain: &str) -> Result<Option<Ipv6Addr>, Error> {
        let origin = Self::zone_from_domain(domain);

        let records = self
            .updater
            .list_rrset(domain, DnsRecordType::AAAA, origin)
            .await
            .map_err(|e| Error::Update {
                domain: domain.to_string(),
                source: Box::new(e),
            })?;

        for record in records {
            if let DnsRecord::AAAA(addr) = record {
                return Ok(Some(addr));
            }
        }

        Ok(None)
    }

    async fn set_record(&self, domain: &str, addr: &Ipv6Addr) -> Result<(), Error> {
        let origin = Self::zone_from_domain(domain);

        // Check if record already has the correct value
        if let Ok(Some(current)) = self.get_record(domain).await {
            if current == *addr {
                debug!(domain, address = %addr, "record already correct, skipping update");
                return Ok(());
            }
        }

        info!(domain, address = %addr, "updating AAAA record via RFC 2136");

        self.updater
            .set_rrset(
                domain,
                DnsRecordType::AAAA,
                120, // TTL
                vec![DnsRecord::AAAA(*addr)],
                origin,
            )
            .await
            .map_err(|e| Error::Update {
                domain: domain.to_string(),
                source: Box::new(e),
            })?;

        Ok(())
    }
}
