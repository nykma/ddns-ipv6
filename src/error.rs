use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("config: {0}")]
    Config(#[from] ConfigError),

    #[error("prefix detection failed: {0}")]
    Prefix(String),

    #[error("DNS update failed for {domain}: {source}")]
    Update {
        domain: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("IO error reading config: {0}")]
    Io(#[from] std::io::Error),

    #[error("TOML parse error: {0}")]
    Parse(#[from] toml::de::Error),

    #[error("missing required field: {0}")]
    MissingField(String),

    #[error("environment variable not set: {0}")]
    EnvNotSet(String),

    #[error("hosts list is empty")]
    EmptyHosts,

    #[error("invalid suffix '{0}': {1}")]
    InvalidSuffix(String, String),

    #[error("unknown key algorithm '{0}'; expected hmac-sha256 or hmac-sha512")]
    UnknownAlgorithm(String),
}
