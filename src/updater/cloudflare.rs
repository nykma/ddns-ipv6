use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::net::Ipv6Addr;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::Error;

const CF_API_BASE: &str = "https://api.cloudflare.com/client/v4";

#[derive(Debug, Deserialize)]
struct CfApiResponse<T> {
    success: bool,
    errors: Vec<CfApiError>,
    result: Option<T>,
    #[serde(default)]
    result_info: Option<CfResultInfo>,
}

#[derive(Debug, Deserialize)]
struct CfApiError {
    code: u32,
    message: String,
}

#[derive(Debug, Deserialize)]
struct CfResultInfo {
    #[serde(default)]
    total_count: u32,
}

#[derive(Debug, Deserialize)]
struct CfDnsRecord {
    id: String,
    #[serde(rename = "type")]
    _type: String,
    name: String,
    content: String,
    ttl: u32,
}

#[derive(Debug, Serialize)]
struct CfCreateRecord {
    #[serde(rename = "type")]
    _type: &'static str,
    name: String,
    content: String,
    ttl: u32,
}

#[derive(Debug, Serialize)]
struct CfUpdateRecord {
    content: String,
}

pub struct CloudflareUpdater {
    zone_id: String,
    api_token: String,
    client: reqwest::Client,
}

impl CloudflareUpdater {
    pub fn new(zone_id: String, api_token: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client should always build");

        Self {
            zone_id,
            api_token,
            client,
        }
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&impl Serialize>,
    ) -> Result<reqwest::Response, Error> {
        let url = format!("{CF_API_BASE}{path}");
        let mut req = self
            .client
            .request(method, &url)
            .header("Authorization", format!("Bearer {}", self.api_token))
            .header("Content-Type", "application/json");

        if let Some(b) = body {
            req = req.json(b);
        }

        let response = req.send().await.map_err(|e| Error::Update {
            domain: url.clone(),
            source: Box::new(e),
        })?;

        Ok(response)
    }

    async fn request_with_retry<T: for<'de> Deserialize<'de>>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&impl Serialize>,
    ) -> Result<CfApiResponse<T>, Error> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let response = self.request(method.clone(), path, body).await?;

            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let retry_after = response
                    .headers()
                    .get("Retry-After")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(5);
                warn!(
                    attempt,
                    retry_after_secs = retry_after,
                    "Cloudflare rate limited"
                );
                if attempt < 3 {
                    tokio::time::sleep(Duration::from_secs(retry_after)).await;
                    continue;
                }
            }

            let status = response.status();
            let body_text = response.text().await.map_err(|e| Error::Update {
                domain: path.to_string(),
                source: Box::new(e),
            })?;

            if !status.is_success() {
                return Err(Error::Update {
                    domain: path.to_string(),
                    source: format!("HTTP {status}: {body_text}").into(),
                });
            }

            return serde_json::from_str::<CfApiResponse<T>>(&body_text).map_err(|e| {
                Error::Update {
                    domain: path.to_string(),
                    source: Box::new(e),
                }
            });
        }
    }
}

#[async_trait]
impl super::DnsUpdater for CloudflareUpdater {
    async fn get_record(&self, domain: &str) -> Result<Option<Ipv6Addr>, Error> {
        let path = format!(
            "/zones/{}/dns_records?type=AAAA&name={}",
            self.zone_id,
            urlencoding(domain)
        );

        let response: CfApiResponse<Vec<CfDnsRecord>> =
            self.request_with_retry(reqwest::Method::GET, &path, None::<&()>).await?;

        if !response.success {
            let errors: Vec<String> = response.errors.iter().map(|e| e.message.clone()).collect();
            return Err(Error::Update {
                domain: domain.to_string(),
                source: format!("API errors: {errors:?}").into(),
            });
        }

        match response.result {
            Some(records) if !records.is_empty() => {
                let content = &records[0].content;
                let addr: Ipv6Addr = content.parse().map_err(|e| Error::Update {
                    domain: domain.to_string(),
                    source: format!("invalid AAAA in Cloudflare response '{content}': {e}").into(),
                })?;
                Ok(Some(addr))
            }
            _ => Ok(None),
        }
    }

    async fn set_record(&self, domain: &str, addr: &Ipv6Addr) -> Result<(), Error> {
        let addr_str = addr.to_string();
        let domain_encoded = urlencoding(domain);

        // First, check if the record already exists
        let list_path = format!(
            "/zones/{}/dns_records?type=AAAA&name={}",
            self.zone_id, domain_encoded
        );

        let response: CfApiResponse<Vec<CfDnsRecord>> =
            self.request_with_retry(reqwest::Method::GET, &list_path, None::<&()>).await?;

        if !response.success {
            let errors: Vec<String> = response.errors.iter().map(|e| e.message.clone()).collect();
            return Err(Error::Update {
                domain: domain.to_string(),
                source: format!("API errors on list: {errors:?}").into(),
            });
        }

        let existing = response.result;

        match existing {
            Some(records) if !records.is_empty() => {
                let record = &records[0];
                // Check if already correct
                if record.content == addr_str {
                    debug!(domain, address = %addr_str, "record already correct, skipping update");
                    return Ok(());
                }

                // Update existing record
                let update_path = format!(
                    "/zones/{}/dns_records/{}",
                    self.zone_id, record.id
                );
                let body = CfUpdateRecord {
                    content: addr_str.clone(),
                };

                info!(domain, old = %record.content, new = %addr_str, "updating AAAA record");
                let resp: CfApiResponse<CfDnsRecord> = self
                    .request_with_retry(reqwest::Method::PATCH, &update_path, Some(&body))
                    .await?;

                if !resp.success {
                    let errors: Vec<String> =
                        resp.errors.iter().map(|e| e.message.clone()).collect();
                    return Err(Error::Update {
                        domain: domain.to_string(),
                        source: format!("API errors on update: {errors:?}").into(),
                    });
                }
            }
            _ => {
                // Create new record
                let create_path = format!("/zones/{}/dns_records", self.zone_id);
                let body = CfCreateRecord {
                    _type: "AAAA",
                    name: domain.to_string(),
                    content: addr_str.clone(),
                    ttl: 120,
                };

                info!(domain, address = %addr_str, "creating AAAA record");
                let resp: CfApiResponse<CfDnsRecord> = self
                    .request_with_retry(reqwest::Method::POST, &create_path, Some(&body))
                    .await?;

                if !resp.success {
                    let errors: Vec<String> =
                        resp.errors.iter().map(|e| e.message.clone()).collect();
                    return Err(Error::Update {
                        domain: domain.to_string(),
                        source: format!("API errors on create: {errors:?}").into(),
                    });
                }
            }
        }

        Ok(())
    }
}

/// Simple percent-encoding for domain names in query parameters.
fn urlencoding(s: &str) -> String {
    s.replace(' ', "%20")
}
