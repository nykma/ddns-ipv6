pub mod config;
pub mod error;
pub mod prefix;
pub mod updater;
pub mod util;
pub mod docker;

pub use error::{ConfigError, Error};
pub use prefix::PrefixDetector;
pub use updater::DnsUpdater;
