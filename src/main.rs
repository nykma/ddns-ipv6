use clap::Parser;
use std::collections::HashMap;
use std::io::IsTerminal;
use std::net::Ipv6Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::select;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use ddns_ipv6::config::{Config, PrefixConfig, ProviderConfig};
use ddns_ipv6::prefix::dns::DnsResolver;
use ddns_ipv6::updater::cloudflare::CloudflareUpdater;
use ddns_ipv6::util;
use ddns_ipv6::{DnsUpdater, Error, PrefixDetector};
use ddns_ipv6::docker::DockerDiscoverer;

#[derive(Parser)]
#[command(name = "ddns-ipv6", about = "Dynamic DNS updater for IPv6 prefixes")]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let is_tty = std::io::stderr().is_terminal();
    if is_tty {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .init();
    } else {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .init();
    }

    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    info!(path = %cli.config.display(), "config loaded");

    // Build prefix detector
    let detector: Arc<dyn PrefixDetector> = build_detector(&config)?;

    // Build DNS updater
    let updater: Arc<dyn DnsUpdater> = build_updater(&config)?;
    // Parse static suffixes
    let static_suffixes: Vec<(Ipv6Addr, String)> = config
        .hosts
        .iter()
        .map(|h| (h.suffix, h.domain.clone()))
        .collect();

    // Build Docker container discoverer (optional)
    let docker_discoverer: Option<DockerDiscoverer> = match &config.docker {
        Some(dc) => {
            info!("docker discovery enabled");
            Some(DockerDiscoverer::new(dc)?)
        }
        None => None,
    };

    // Cache: domain -> current Ipv6Addr
    let mut cache: HashMap<String, Ipv6Addr> = HashMap::new();
    // Startup: query current DNS state to seed the cache, then run one update cycle
    info!("running startup check...");
    let discovered = if let Some(dd) = &docker_discoverer {
        dd.discover().await
    } else {
        Vec::new()
    };
    let all_suffixes: Vec<(Ipv6Addr, String)> = static_suffixes
        .iter()
        .cloned()
        .chain(discovered.iter().map(|h| (h.suffix, h.domain.clone())))
        .collect();
    for (_suffix, domain) in &all_suffixes {
        match updater.get_record(domain).await {
            Ok(Some(addr)) => {
                info!(domain, %addr, "current DNS record");
                cache.insert(domain.clone(), addr);
            }
            Ok(None) => {
                info!(domain, "no existing AAAA record");
            }
            Err(e) => {
                warn!(domain, error = %e, "failed to query existing record");
            }
        }
    }

    // Run one full update cycle at startup
    run_update_cycle(
        &*detector,
        &*updater,
        &static_suffixes,
        docker_discoverer.as_ref(),
        &mut cache,
    )
    .await;

    // Setup signals
    let cancel_token = CancellationToken::new();
    let force_refresh = Arc::new(Notify::new());

    // SIGTERM/SIGINT → cancel
    {
        let cancel = cancel_token.clone();
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            info!("received SIGINT, shutting down");
            cancel.cancel();
        });
    }

    // SIGUSR1 (Unix only) → force refresh
    #[cfg(unix)]
    {
        let force = force_refresh.clone();
        tokio::spawn(async move {
            let mut usr1 = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
                .expect("failed to register SIGUSR1 handler");
            loop {
                usr1.recv().await;
                info!("received SIGUSR1, forcing refresh");
                force.notify_one();
            }
        });
    }

    let mut change_rx = detector.changes();

    info!("entering main loop");
    loop {
        select! {
            _ = change_rx.changed() => {
                // Prefix change detected (or poll tick)
            }
            _ = force_refresh.notified() => {
                // SIGUSR1 received
            }
            _ = cancel_token.cancelled() => {
                info!("shutting down");
                break;
            }
        }

        run_update_cycle(&*detector, &*updater, &static_suffixes, docker_discoverer.as_ref(), &mut cache).await;
    }

    info!("shutdown complete");
    Ok(())
}

async fn run_update_cycle(
    detector: &dyn PrefixDetector,
    updater: &dyn DnsUpdater,
    static_suffixes: &[(Ipv6Addr, String)],
    docker_discoverer: Option<&DockerDiscoverer>,
    cache: &mut HashMap<String, Ipv6Addr>,
) {
    let prefix = match detector.detect().await {
        Ok(p) => p,
        Err(e) => {
            error!(error = %e, "prefix detection failed, skipping update cycle");
            return;
        }
    };

    info!(prefix = %prefix, "detected prefix");

    // Merge Docker-discovered hosts with static suffixes
    let discovered = if let Some(dd) = docker_discoverer {
        dd.discover().await
    } else {
        Vec::new()
    };
    let combined: Vec<(Ipv6Addr, String)> = static_suffixes
        .iter()
        .cloned()
        .chain(discovered.iter().map(|h| (h.suffix, h.domain.clone())))
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut suffixes: Vec<(Ipv6Addr, String)> = Vec::new();
    for (suffix, domain) in combined {
        if !seen.insert(domain.clone()) {
            warn!(%domain, "duplicate domain in host list; last entry wins");
        }
        suffixes.push((suffix, domain));
    }

    let mut error_count = 0usize;
    for (suffix, domain) in &suffixes {
        let new_addr = util::combine(&prefix, suffix);
        if cache.get(domain) == Some(&new_addr) {
            continue;
        }

        match updater.set_record(domain, &new_addr).await {
            Ok(()) => {
                info!(domain, %new_addr, "updated AAAA record");
                cache.insert(domain.clone(), new_addr);
            }
            Err(e) => {
                error!(domain, error = %e, "update failed");
                error_count += 1;
            }
        }
    }

    if error_count > 0 {
        warn!(count = error_count, "some updates failed");
    }
}

fn build_detector(config: &Config) -> Result<Arc<dyn PrefixDetector>, Error> {
    match &config.prefix {
        PrefixConfig::Dns { reference_domain } => {
            info!(
                method = "dns",
                domain = %reference_domain,
                interval_secs = config.interval_secs,
                "using DNS prefix detection"
            );
            let detector = DnsResolver::new(
                reference_domain.clone(),
                Duration::from_secs(config.interval_secs),
            )?;
            Ok(Arc::new(detector))
        }
        #[cfg(target_os = "linux")]
        PrefixConfig::Netlink { interface } => {
            info!(method = "netlink", %interface, "using netlink prefix detection");
            let detector = ddns_ipv6::prefix::netlink::NetlinkWatcher::new(interface.clone())?;
            Ok(Arc::new(detector))
        }
        #[cfg(target_os = "linux")]
        PrefixConfig::Ra { interface } => {
            info!(method = "ra", %interface, "using RA prefix detection");
            let detector = ddns_ipv6::prefix::ra::RaListener::new(interface.clone())?;
            Ok(Arc::new(detector))
        }
        #[cfg(not(target_os = "linux"))]
        PrefixConfig::Netlink { .. } | PrefixConfig::Ra { .. } => {
            Err(Error::Config(ddns_ipv6::ConfigError::MissingField(
                "netlink and ra methods are Linux-only".into(),
            )))
        }
    }
}

fn build_updater(config: &Config) -> Result<Arc<dyn DnsUpdater>, Error> {
    match &config.dns {
        ProviderConfig::Cloudflare { zone_id, api_token } => {
            info!(provider = "cloudflare", zone_id = %zone_id, "using Cloudflare DNS updater");
            let updater = CloudflareUpdater::new(zone_id.clone(), api_token.clone());
            Ok(Arc::new(updater))
        }
        ProviderConfig::Rfc2136 {
            server,
            key_name,
            key_algorithm,
            key_secret,
        } => {
            info!(provider = "rfc2136", server = %server, "using RFC 2136 DNS updater");
            let updater = ddns_ipv6::updater::rfc2136::Rfc2136Updater::new(
                server.clone(),
                key_name.clone(),
                key_algorithm.clone(),
                key_secret.clone(),
            )?;
            Ok(Arc::new(updater))
        }
    }
}
