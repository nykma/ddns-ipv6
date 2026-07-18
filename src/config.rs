use std::net::Ipv6Addr;
use std::path::Path;

use serde::{Deserialize, Deserializer};

use crate::ConfigError;

// ── Secret string that supports env: prefix ────────────────────────────────

/// A string value that resolves the `env:VAR` prefix by reading the named
/// environment variable. Plain strings are used as-is.
#[derive(Debug, Clone)]
pub struct Secret(String);

impl Secret {
    pub fn into_string(self) -> String {
        self.0
    }
}

fn resolve_env(value: &str) -> Result<Secret, ConfigError> {
    match value.strip_prefix("env:") {
        Some(var) => {
            let val = std::env::var(var).map_err(|_| ConfigError::EnvNotSet(var.to_string()))?;
            Ok(Secret(val))
        }
        None => Ok(Secret(value.to_string())),
    }
}

// ── Ipv6Addr deserialization helper ────────────────────────────────────────

fn ipv6addr_from_str<'de, D>(deserializer: D) -> Result<Ipv6Addr, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    s.parse()
        .map_err(|e| serde::de::Error::custom(format!("invalid suffix '{s}': {e}")))
}

// ── Public types (used by main.rs) ─────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum PrefixConfig {
    Dns { reference_domain: String },
    Netlink { interface: String },
    Ra { interface: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct HostEntry {
    #[serde(deserialize_with = "ipv6addr_from_str")]
    pub suffix: Ipv6Addr,
    pub domain: String,
}

#[derive(Debug, Clone)]
pub struct DockerConfig {
    pub socket_path: String,
}

/// The validated, env-resolved DNS provider config consumed by the main loop.
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

fn default_interval() -> u64 {
    300
}

fn default_socket_path() -> String {
    "/var/run/docker.sock".to_string()
}

// ── Internal deserialization helpers ───────────────────────────────────────

/// Deserialized from TOML — secrets are still raw strings at this stage.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    prefix: RawPrefix,
    dns: RawDns,
    #[serde(default)]
    hosts: Vec<HostEntry>,
    #[serde(default = "default_interval")]
    interval_secs: u64,
    docker: Option<RawDocker>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "method")]
enum RawPrefix {
    #[serde(rename = "dns")]
    Dns { reference_domain: String },
    #[serde(rename = "netlink")]
    Netlink { interface: String },
    #[serde(rename = "ra")]
    Ra { interface: String },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "provider")]
enum RawDns {
    #[serde(rename = "cloudflare")]
    Cloudflare { zone_id: String, api_token: String },
    #[serde(rename = "rfc2136")]
    Rfc2136 {
        server: String,
        key_name: String,
        key_algorithm: String,
        key_secret: String,
    },
}

#[derive(Debug, Deserialize)]
struct RawDocker {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_socket_path")]
    socket_path: String,
}

// ── Public API ────────────────────────────────────────────────────────────

/// Fully validated configuration ready for use by the main loop.
#[derive(Debug, Clone)]
pub struct ValidatedConfig {
    pub prefix: PrefixConfig,
    pub dns: ProviderConfig,
    pub hosts: Vec<HostEntry>,
    pub interval_secs: u64,
    pub docker: Option<DockerConfig>,
}

impl ValidatedConfig {
    /// Load, deserialize, and validate configuration from a TOML file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path)?;
        let raw: RawConfig = toml::from_str(&contents)?;
        validate(raw)
    }
}

fn validate(raw: RawConfig) -> Result<ValidatedConfig, ConfigError> {
    let prefix = match raw.prefix {
        RawPrefix::Dns { reference_domain } => PrefixConfig::Dns { reference_domain },
        RawPrefix::Netlink { interface } => PrefixConfig::Netlink { interface },
        RawPrefix::Ra { interface } => PrefixConfig::Ra { interface },
    };

    let dns = match raw.dns {
        RawDns::Cloudflare { zone_id, api_token } => {
            let resolved = resolve_env(&api_token)?;
            ProviderConfig::Cloudflare {
                zone_id,
                api_token: resolved.into_string(),
            }
        }
        RawDns::Rfc2136 {
            server,
            key_name,
            key_algorithm,
            key_secret,
        } => {
            // Validate algorithm
            match key_algorithm.as_str() {
                "hmac-sha256" | "hmac-sha512" => {}
                other => return Err(ConfigError::UnknownAlgorithm(other.to_string())),
            }
            let resolved = resolve_env(&key_secret)?;
            ProviderConfig::Rfc2136 {
                server,
                key_name,
                key_algorithm,
                key_secret: resolved.into_string(),
            }
        }
    };

    let docker = raw.docker.filter(|d| d.enabled).map(|d| DockerConfig {
        socket_path: d.socket_path,
    });

    if raw.hosts.is_empty() && docker.is_none() {
        return Err(ConfigError::EmptyHosts);
    }

    Ok(ValidatedConfig {
        prefix,
        dns,
        hosts: raw.hosts,
        interval_secs: raw.interval_secs,
        docker,
    })
}

// ── Tests ──────────────────────────────────────────────────────────────────

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
        let config = ValidatedConfig::load(&path).unwrap();
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
        let config = ValidatedConfig::load(&path).unwrap();
        std::fs::remove_file(&path).ok();

        match &config.dns {
            ProviderConfig::Rfc2136 {
                server,
                key_name,
                key_algorithm,
                ..
            } => {
                assert_eq!(server, "tcp://ns1.example.com:53");
                assert_eq!(key_name, "ddns-key.");
                assert_eq!(key_algorithm, "hmac-sha256");
            }
            _ => panic!("expected RFC 2136 provider"),
        }
    }

    #[test]
    fn empty_hosts_and_no_docker_errors() {
        let toml = "\
[prefix]
method = \"dns\"
reference_domain = \"router.example.com\"

[dns]
provider = \"cloudflare\"
zone_id = \"abc\"
api_token = \"token\"
";
        let path = write_tmp(toml);
        let result = ValidatedConfig::load(&path);
        std::fs::remove_file(&path).ok();
        assert!(matches!(result.unwrap_err(), ConfigError::EmptyHosts));
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
        let result = ValidatedConfig::load(&path);
        std::fs::remove_file(&path).ok();
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(
            err_str.contains("invalid suffix"),
            "expected suffix error, got: {err_str}"
        );
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
        let config = ValidatedConfig::load(&path).unwrap();
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
        let result = ValidatedConfig::load(&path);
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
        let config = ValidatedConfig::load(&path).unwrap();
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
        let result = ValidatedConfig::load(&path);
        std::fs::remove_file(&path).ok();
        match result.unwrap_err() {
            ConfigError::EnvNotSet(var) => assert_eq!(var, "DDNS_MISSING_VAR"),
            e => panic!("expected EnvNotSet, got {e:?}"),
        }
    }

    // ── Docker config tests ──

    #[test]
    fn docker_enabled_parses() {
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

[docker]
enabled = true
";
        let path = write_tmp(toml);
        let config = ValidatedConfig::load(&path).unwrap();
        std::fs::remove_file(&path).ok();

        let dc = config.docker.expect("docker config should be present");
        assert_eq!(dc.socket_path, "/var/run/docker.sock");
    }

    #[test]
    fn docker_custom_socket_path() {
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

[docker]
enabled = true
socket_path = \"/custom/path/docker.sock\"
";
        let path = write_tmp(toml);
        let config = ValidatedConfig::load(&path).unwrap();
        std::fs::remove_file(&path).ok();

        let dc = config.docker.expect("docker config should be present");
        assert_eq!(dc.socket_path, "/custom/path/docker.sock");
    }

    #[test]
    fn docker_disabled_returns_none() {
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

[docker]
enabled = false
";
        let path = write_tmp(toml);
        let config = ValidatedConfig::load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert!(config.docker.is_none());
    }

    #[test]
    fn docker_missing_returns_none() {
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
        let config = ValidatedConfig::load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert!(config.docker.is_none());
    }

    #[test]
    fn docker_empty_section_returns_none() {
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

[docker]
";
        let path = write_tmp(toml);
        let config = ValidatedConfig::load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert!(config.docker.is_none());
    }

    #[test]
    fn docker_only_no_static_hosts() {
        let toml = "\
[prefix]
method = \"dns\"
reference_domain = \"router.example.com\"

[dns]
provider = \"cloudflare\"
zone_id = \"abc\"
api_token = \"token\"

[docker]
enabled = true
";
        let path = write_tmp(toml);
        let config = ValidatedConfig::load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert!(config.hosts.is_empty());
        assert!(config.docker.is_some());
    }
}
