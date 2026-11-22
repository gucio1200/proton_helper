mod config;
mod logging;
mod mapping;
mod qbittorrent;
mod report;

use std::convert::Infallible;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{error, info, warn};

use crate::config::Config;
use crate::mapping::Mapper;
use crate::qbittorrent::QBittorrent;
use crate::report::{Mapping, Reporter, Stage, REPEAT_AFTER};

#[tokio::main]
async fn main() -> ExitCode {
    logging::init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %format!("{e:#}"), "fatal error");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let config = Config::from_env()?;
    info!(
        version = env!("CARGO_PKG_VERSION"),
        gateway = %config.gateway,
        internal_port = config.internal_port,
        public_port = config.public_port,
        lifetime_secs = config.lifetime,
        refresh_interval_secs = config.refresh_interval.as_secs(),
        qbittorrent = %config.qbittorrent_display(),
        "starting"
    );
    if config.refresh_interval.as_secs() >= u64::from(config.lifetime) {
        warn!(
            "REFRESH_INTERVAL is not shorter than MAPPING_LIFETIME, the mapping can expire between refreshes"
        );
    }

    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let qbittorrent = QBittorrent::new(&config.qbittorrent_url)?;
    let mut mapper = Mapper::new(&config);
    let mut reporter = Reporter::new(REPEAT_AFTER);

    let signal = tokio::select! {
        never = refresh(&mut mapper, &qbittorrent, &mut reporter, config.refresh_interval) => match never {},
        _ = terminate.recv() => "SIGTERM",
        _ = interrupt.recv() => "SIGINT",
    };
    info!(signal, "stopping");
    Ok(())
}

async fn refresh(
    mapper: &mut Mapper,
    qbittorrent: &QBittorrent,
    reporter: &mut Reporter,
    every: Duration,
) -> Infallible {
    let mut ticker = interval(every);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let mapping = match mapper.refresh().await {
            Ok(mapping) => mapping,
            Err(e) => {
                reporter.failed(Stage::NatPmp, &e, Instant::now());
                continue;
            }
        };
        reporter.mapped(mapping);
        reporter.recovered(Stage::NatPmp, Instant::now());

        if let Err(e) = qbittorrent.version().await {
            reporter.qbittorrent_unreachable(&e, Instant::now());
            continue;
        }
        reporter.qbittorrent_reachable(Instant::now());

        match sync(qbittorrent, reporter, mapping).await {
            Ok(()) => reporter.succeeded(Instant::now()),
            Err(e) => reporter.failed(Stage::QBittorrent, &e, Instant::now()),
        }
    }
}

async fn sync(qbittorrent: &QBittorrent, reporter: &Reporter, mapping: Mapping) -> Result<()> {
    let current = qbittorrent.listen_port().await?;
    if current != mapping.tcp {
        qbittorrent.set_listen_port(mapping.tcp).await?;
    }
    reporter.listen_port(current, mapping.tcp);
    Ok(())
}
