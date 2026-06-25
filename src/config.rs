
use std::net::Ipv6Addr;
use std::path::Path;

use crate::ConfigError;

fn default_interval() -> u64 {
    300
}

#[derive(Debug, Clone)]
pub struct Config {
    pub prefix: PrefixConfig,
    pub dns: ProviderConfig,
    pub hosts: Vec<HostEntry>,
    pub interval_secs: u64,
}

#[derive(Debug, Clone)]
pub enum PrefixConfig {
    Dns { reference_domain: String },
    Netlink { interface: String },
    Ra { interface: String },
}

#[derive(Debug, Clone)]
pub enum ProviderConfig {
    Cloudflare {
        zone_id: String,
        api_token: String,
    },
    Rfc2136 {
        server: String,
        key_name: String,
        key_algorithm: String,
        key_secret: String,
    },
}

#[derive(Debug, Clone)]
pub struct HostEntry {
    pub suffix: Ipv6Addr,
    pub domain: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path)?;
        let root: toml::Table =
            toml::from_str(&contents).map_err(ConfigError::Parse)?;

        let prefix = parse_prefix(&root)?;
        let dns = parse_dns(&root)?;
        let hosts = parse_hosts(&root)?;
        let interval_secs = root
            .get("interval_secs")
            .and_then(|v| v.as_integer())
            .map(|v| v as u64)
            .unwrap_or_else(default_interval);

        Ok(Config {
            prefix,
            dns,
            hosts,
            interval_secs,
        })
    }
}

fn parse_prefix(root: &toml::Table) -> Result<PrefixConfig, ConfigError> {
    let prefix_table = root
        .get("prefix")
        .and_then(|v| v.as_table())
        .ok_or_else(|| ConfigError::MissingField("prefix".into()))?;

    let method = prefix_table
        .get("method")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ConfigError::MissingField("prefix.method".into()))?;

    match method {
        "dns" => {
            let reference_domain = get_string(prefix_table, "reference_domain")?;
            Ok(PrefixConfig::Dns { reference_domain })
        }
        "netlink" => {
            let interface = get_string(prefix_table, "interface")?;
            Ok(PrefixConfig::Netlink { interface })
        }
        "ra" => {
            let interface = get_string(prefix_table, "interface")?;
            Ok(PrefixConfig::Ra { interface })
        }
        other => Err(ConfigError::MissingField(format!(
            "unknown prefix method: {other}"
        ))),
    }
}

fn parse_dns(root: &toml::Table) -> Result<ProviderConfig, ConfigError> {
    let dns_table = root
        .get("dns")
        .and_then(|v| v.as_table())
        .ok_or_else(|| ConfigError::MissingField("dns".into()))?;

    let provider = dns_table
        .get("provider")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ConfigError::MissingField("dns.provider".into()))?;

    match provider {
        "cloudflare" => {
            let zone_id = get_string(dns_table, "zone_id")?;
            let api_token = resolve_env_str(&get_string(dns_table, "api_token")?)?;
            Ok(ProviderConfig::Cloudflare { zone_id, api_token })
        }
        "rfc2136" => {
            let server = get_string(dns_table, "server")?;
            let key_name = get_string(dns_table, "key_name")?;
            let key_algorithm = get_string(dns_table, "key_algorithm")?;
            let key_secret = resolve_env_str(&get_string(dns_table, "key_secret")?)?;

            // Validate algorithm
            match key_algorithm.as_str() {
                "hmac-sha256" | "hmac-sha512" => {}
                other => return Err(ConfigError::UnknownAlgorithm(other.to_string())),
            }

            Ok(ProviderConfig::Rfc2136 {
                server,
                key_name,
                key_algorithm,
                key_secret,
            })
        }
        other => Err(ConfigError::MissingField(format!(
            "unknown DNS provider: {other}"
        ))),
    }
}

fn parse_hosts(root: &toml::Table) -> Result<Vec<HostEntry>, ConfigError> {
    let hosts_raw = root
        .get("hosts")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ConfigError::MissingField("hosts".into()))?;

    let mut hosts = Vec::new();
    for entry in hosts_raw {
        let table = entry.as_table().ok_or_else(|| {
            ConfigError::MissingField("hosts entry must be a table".into())
        })?;
        let suffix_str = get_string(table, "suffix")?;
        let suffix: Ipv6Addr = suffix_str
            .parse()
            .map_err(|e: std::net::AddrParseError| ConfigError::InvalidSuffix(suffix_str.clone(), e.to_string()))?;
        let domain = get_string(table, "domain")?;
        hosts.push(HostEntry { suffix, domain });
    }

    if hosts.is_empty() {
        return Err(ConfigError::EmptyHosts);
    }

    Ok(hosts)
}

fn get_string(table: &toml::Table, key: &str) -> Result<String, ConfigError> {
    table
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| ConfigError::MissingField(key.to_string()))
}

fn resolve_env_str(value: &str) -> Result<String, ConfigError> {
    if let Some(var) = value.strip_prefix("env:") {
        std::env::var(var).map_err(|_| ConfigError::EnvNotSet(var.to_string()))
    } else {
        Ok(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(content: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ddns-ipv6-test-{}-{n}.toml", std::process::id()));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn valid_dns_cloudflare_config() {
        let toml = "\
[prefix]
method = \"dns\"
reference_domain = \"my-router.example.com\"

[dns]
provider = \"cloudflare\"
zone_id = \"abc123\"
api_token = \"test-token\"

[[hosts]]
suffix = \"::1\"
domain = \"server-a.example.com\"

[[hosts]]
suffix = \"::dead:beef\"
domain = \"server-b.example.com\"

interval_secs = 300
";
        let path = write_tmp(toml);
        let config = Config::load(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(config.interval_secs, 300);
        assert_eq!(config.hosts.len(), 2);
        assert_eq!(config.hosts[0].suffix.to_string(), "::1");
        assert_eq!(config.hosts[1].domain, "server-b.example.com");
        match &config.prefix {
            PrefixConfig::Dns { reference_domain } => {
                assert_eq!(reference_domain, "my-router.example.com");
            }
            _ => panic!("expected DNS prefix config"),
        }
    }

    #[test]
    fn valid_rfc2136_config() {
        let toml = "\
[prefix]
method = \"dns\"
reference_domain = \"router.example.com\"

[dns]
provider = \"rfc2136\"
server = \"tcp://ns1.example.com:53\"
key_name = \"ddns-key.\"
key_algorithm = \"hmac-sha256\"
key_secret = \"dGVzdC1zZWNyZXQ=\"

[[hosts]]
suffix = \"::1\"
domain = \"test.example.com\"
";
        let path = write_tmp(toml);
        let config = Config::load(&path).unwrap();
        std::fs::remove_file(&path).ok();

        match &config.dns {
            ProviderConfig::Rfc2136 { server, key_name, key_algorithm, .. } => {
                assert_eq!(server, "tcp://ns1.example.com:53");
                assert_eq!(key_name, "ddns-key.");
                assert_eq!(key_algorithm, "hmac-sha256");
            }
            _ => panic!("expected RFC 2136 provider"),
        }
    }

    #[test]
    fn empty_hosts_errors() {
        let toml = "\
[prefix]
method = \"dns\"
reference_domain = \"router.example.com\"

[dns]
provider = \"cloudflare\"
zone_id = \"abc\"
api_token = \"token\"

hosts = []
";
        let path = write_tmp(toml);
        let result = Config::load(&path);
        std::fs::remove_file(&path).ok();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ConfigError::EmptyHosts | ConfigError::MissingField(_)),
            "expected EmptyHosts or MissingField, got {err:?}"
        );
    }

    #[test]
    fn invalid_suffix_errors() {
        let toml = "\
[prefix]
method = \"dns\"
reference_domain = \"router.example.com\"

[dns]
provider = \"cloudflare\"
zone_id = \"abc\"
api_token = \"token\"

[[hosts]]
suffix = \"not-an-ip\"
domain = \"test.example.com\"
";
        let path = write_tmp(toml);
        let result = Config::load(&path);
        std::fs::remove_file(&path).ok();
        assert!(result.is_err());
        match result.unwrap_err() {
            ConfigError::InvalidSuffix(s, _) => assert_eq!(s, "not-an-ip"),
            e => panic!("expected InvalidSuffix, got {e:?}"),
        }
    }

    #[test]
    fn default_interval() {
        let toml = "\
[prefix]
method = \"dns\"
reference_domain = \"router.example.com\"

[dns]
provider = \"cloudflare\"
zone_id = \"abc\"
api_token = \"token\"

[[hosts]]
suffix = \"::1\"
domain = \"test.example.com\"
";
        let path = write_tmp(toml);
        let config = Config::load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(config.interval_secs, 300);
    }

    #[test]
    fn unknown_algorithm_errors() {
        let toml = "\
[prefix]
method = \"dns\"
reference_domain = \"router.example.com\"

[dns]
provider = \"rfc2136\"
server = \"tcp://ns1.example.com:53\"
key_name = \"ddns-key.\"
key_algorithm = \"hmac-md5\"
key_secret = \"dGVzdA==\"

[[hosts]]
suffix = \"::1\"
domain = \"test.example.com\"
";
        let path = write_tmp(toml);
        let result = Config::load(&path);
        std::fs::remove_file(&path).ok();
        assert!(result.is_err());
        match result.unwrap_err() {
            ConfigError::UnknownAlgorithm(algo) => assert_eq!(algo, "hmac-md5"),
            e => panic!("expected UnknownAlgorithm, got {e:?}"),
        }
    }

    #[test]
    fn env_var_resolution() {
        unsafe { std::env::set_var("DDNS_TEST_TOKEN", "env-resolved-token") };
        let toml = "\
[prefix]
method = \"dns\"
reference_domain = \"router.example.com\"

[dns]
provider = \"cloudflare\"
zone_id = \"abc\"
api_token = \"env:DDNS_TEST_TOKEN\"

[[hosts]]
suffix = \"::1\"
domain = \"test.example.com\"
";
        let path = write_tmp(toml);
        let config = Config::load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        unsafe { std::env::remove_var("DDNS_TEST_TOKEN") };

        match &config.dns {
            ProviderConfig::Cloudflare { api_token, .. } => {
                assert_eq!(api_token, "env-resolved-token");
            }
            _ => panic!("expected cloudflare provider"),
        }
    }

    #[test]
    fn missing_env_var_errors() {
        unsafe { std::env::remove_var("DDNS_MISSING_VAR") };
        let toml = "\
[prefix]
method = \"dns\"
reference_domain = \"router.example.com\"

[dns]
provider = \"cloudflare\"
zone_id = \"abc\"
api_token = \"env:DDNS_MISSING_VAR\"

[[hosts]]
suffix = \"::1\"
domain = \"test.example.com\"
";
        let path = write_tmp(toml);
        let result = Config::load(&path);
        std::fs::remove_file(&path).ok();
        match result.unwrap_err() {
            ConfigError::EnvNotSet(var) => assert_eq!(var, "DDNS_MISSING_VAR"),
            e => panic!("expected EnvNotSet, got {e:?}"),
        }
    }
}
