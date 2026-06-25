pub mod config;
pub mod error;
pub mod prefix;
pub mod updater;
pub mod util;

pub use error::{ConfigError, Error};
pub use prefix::PrefixDetector;
pub use updater::DnsUpdater;
