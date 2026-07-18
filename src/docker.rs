use std::collections::HashMap;
use std::net::Ipv6Addr;

use bollard::Docker;
use bollard::query_parameters::ListContainersOptions;
use tracing::{debug, info, warn};

use crate::config::{DockerConfig, HostEntry};
use crate::util;

/// Label key that marks a container for DDNS discovery.
/// The label value is the fully qualified domain name.
const DDNS_DOMAIN_LABEL: &str = "ddns.domain";

/// Discovers Docker containers with the `ddns.domain` label,
/// extracts their IPv6 suffix from network settings, and returns
/// a list of `HostEntry` to merge with static config.
pub struct DockerDiscoverer {
    docker: Docker,
}

impl DockerDiscoverer {
    /// Connect to the Docker daemon at the configured socket path.
    pub fn new(config: &DockerConfig) -> Result<Self, crate::Error> {
        let docker =
            Docker::connect_with_unix(&config.socket_path, 120, bollard::API_DEFAULT_VERSION)
                .map_err(|e| crate::Error::Other(e.into()))?;
        info!(
            socket = %config.socket_path,
            "connected to Docker daemon"
        );
        Ok(Self { docker })
    }

    /// Discover containers with the `ddns.domain` label and return their
    /// suffix→domain mappings. Errors contacting Docker are logged and
    /// an empty list is returned — discovery is best-effort.
    pub async fn discover(&self) -> Vec<HostEntry> {
        let containers = match self.list_labeled_containers().await {
            Ok(c) => c,
            Err(e) => {
                warn!(%e, "failed to list Docker containers");
                return Vec::new();
            }
        };

        let mut hosts = Vec::new();
        for (container_id, domain) in containers {
            if domain.is_empty() {
                debug!(%container_id, "ddns.domain label value is empty; skipping");
                continue;
            }
            match self.extract_suffix(&container_id).await {
                Ok(Some(suffix)) => {
                    debug!(%container_id, %domain, %suffix, "discovered container");
                    hosts.push(HostEntry { suffix, domain });
                }
                Ok(None) => {
                    debug!(%container_id, %domain, "no global unicast IPv6 address; skipping");
                }
                Err(e) => {
                    warn!(%container_id, %domain, %e, "failed to inspect container; skipping");
                }
            }
        }

        hosts
    }

    // List containers with the `ddns.domain` label, returning (id, domain) pairs.
    async fn list_labeled_containers(
        &self,
    ) -> Result<Vec<(String, String)>, bollard::errors::Error> {
        let mut filters = HashMap::new();
        filters.insert("label".to_string(), vec![DDNS_DOMAIN_LABEL.to_string()]);

        let options = ListContainersOptions {
            all: true,
            filters: Some(filters),
            ..Default::default()
        };

        let containers = self.docker.list_containers(Some(options)).await?;

        let mut result = Vec::new();
        for c in containers {
            let id = match &c.id {
                Some(id) => id.clone(),
                None => {
                    debug!("container with no id; skipping");
                    continue;
                }
            };

            let labels = match &c.labels {
                Some(l) => l,
                None => {
                    // Should not happen — we filtered by label
                    debug!(%id, "container matched label filter but has no labels; skipping");
                    continue;
                }
            };

            let domain = match labels.get(DDNS_DOMAIN_LABEL) {
                Some(d) => d.clone(),
                None => {
                    debug!(%id, "container matched label filter but ddns.domain label missing in labels map; skipping");
                    continue;
                }
            };

            result.push((id, domain));
        }

        Ok(result)
    }

    // Inspect a container's network settings and extract the lower 64 bits
    // of the first global unicast IPv6 address found. Returns `None` if
    // the container has no matching address.
    async fn extract_suffix(&self, container_id: &str) -> Result<Option<Ipv6Addr>, crate::Error> {
        let inspect = self
            .docker
            .inspect_container(
                container_id,
                None::<bollard::query_parameters::InspectContainerOptions>,
            )
            .await
            .map_err(|e| crate::Error::Other(e.into()))?;

        let networks = match inspect.network_settings.and_then(|ns| ns.networks) {
            Some(n) => n,
            None => return Ok(None),
        };

        for (_net_name, endpoint) in networks {
            let addr_str = match endpoint.global_ipv6_address.as_deref() {
                Some("") | None => continue,
                Some(s) => s,
            };

            // Strip zone ID suffix if present (e.g., "2001:db8::1%eth0")
            let clean = addr_str.split('%').next().unwrap_or(addr_str);

            let addr: Ipv6Addr = match clean.parse() {
                Ok(a) => a,
                Err(e) => {
                    debug!(%addr_str, %e, "failed to parse container IPv6 address; skipping");
                    continue;
                }
            };

            if util::is_global_unicast(&addr) {
                let suffix = util::suffix_from_addr(&addr);
                return Ok(Some(suffix));
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv6Addr;

    #[test]
    fn global_unicast_accepts_2000_prefix() {
        let addr: Ipv6Addr = "2001:db8:1:2:3:4:5:6".parse().unwrap();
        assert!(crate::util::is_global_unicast(&addr));
    }

    #[test]
    fn global_unicast_rejects_link_local() {
        let addr: Ipv6Addr = "fe80::1".parse().unwrap();
        assert!(!crate::util::is_global_unicast(&addr));
    }

    #[test]
    fn global_unicast_rejects_loopback() {
        let addr: Ipv6Addr = "::1".parse().unwrap();
        assert!(!crate::util::is_global_unicast(&addr));
    }

    #[test]
    fn global_unicast_rejects_ula() {
        let addr: Ipv6Addr = "fd00::1".parse().unwrap();
        assert!(!crate::util::is_global_unicast(&addr));
    }

    #[test]
    fn global_unicast_rejects_multicast() {
        let addr: Ipv6Addr = "ff02::1".parse().unwrap();
        assert!(!crate::util::is_global_unicast(&addr));
    }
}
