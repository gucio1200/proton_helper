mod check;
mod config;
mod kube;
mod logging;
mod metrics;
mod mikrotik;
mod proton;
mod session;
mod vpn;

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::metrics::{CurrentServer, Metrics, Regeneration};
use crate::proton::Proton;
use crate::session::{Origin, SessionStore};

const HEARTBEAT_MAX_AGE: Duration = Duration::from_secs(300);
const SETTLE_AFTER_APPLY: Duration = Duration::from_secs(15);

fn main() -> ExitCode {
    let _ = dotenvy::dotenv();
    logging::init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %format!("{e:#}"), "fatal error");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let config = Config::from_env()?;
    info!(
        version = env!("CARGO_PKG_VERSION"),
        app_version = %config.app_version,
        check_url = %config.check.url,
        check_interval_secs = config.check.interval.as_secs(),
        check_failures = config.check.failures,
        regen_cooldown_secs = config.check.cooldown.as_secs(),
        regenerate_on_start = config.regenerate_on_start,
        wg_interface = %config.mikrotik.wg_interface,
        session_secret = config.session_secret.as_deref().unwrap_or("disabled"),
        "starting"
    );

    let metrics = Arc::new(Metrics::default());
    metrics.heartbeat();
    metrics::serve(&config.http_addr, metrics.clone(), HEARTBEAT_MAX_AGE)?;

    let running = Arc::new(AtomicBool::new(true));
    {
        let running = running.clone();
        ctrlc::set_handler(move || running.store(false, Ordering::SeqCst))?;
    }

    let store = SessionStore::new(config.session_secret.clone(), &config.seed);
    let persistent = store.persistent();
    let (mut proton, origin) = Proton::new(&config, store, metrics.clone())?;
    if !proton.can_refresh() {
        warn!("REFRESH_TOKEN is not set, the Proton session cannot be renewed automatically");
    }
    let check = check::TunnelCheck::new(
        &config.check.url,
        &config.check.expect,
        config.check.timeout,
    )?;

    let mut last_refresh = Instant::now();
    if origin == Origin::Seed && persistent && proton.can_refresh() {
        info!("validating the Proton session from 1Password");
        if let Err(e) = proton.refresh() {
            warn!(error = %format!("{e:#}"), "Proton session refresh at startup failed");
        }
        last_refresh = Instant::now();
    }

    let mut last_server: Option<String> = None;
    if config.regenerate_on_start {
        info!("REGENERATE_ON_START enabled, generating a fresh VPN config");
        if regenerate(&mut proton, &config, &metrics, &mut last_server) {
            pause(&running, &metrics, SETTLE_AFTER_APPLY);
        }
    }

    let mut failures = 0u32;
    let mut healthy: Option<bool> = None;
    while running.load(Ordering::SeqCst) {
        metrics.heartbeat();
        match check.run() {
            Ok(exit_ip) => {
                metrics.check(true);
                if healthy != Some(true) {
                    info!(exit_ip = %exit_ip, "tunnel OK");
                }
                healthy = Some(true);
                failures = 0;
            }
            Err(e) => {
                metrics.check(false);
                failures += 1;
                healthy = Some(false);
                warn!(
                    failures,
                    threshold = config.check.failures,
                    error = %format!("{e:#}"),
                    "tunnel check failed"
                );
            }
        }

        if failures >= config.check.failures {
            info!(failures, "tunnel down, regenerating VPN config");
            regenerate(&mut proton, &config, &metrics, &mut last_server);
            failures = 0;
            pause(&running, &metrics, config.check.cooldown);
            continue;
        }

        if proton.can_refresh() && last_refresh.elapsed() >= config.session_refresh_interval {
            info!("scheduled Proton session refresh");
            if let Err(e) = proton.refresh() {
                warn!(error = %format!("{e:#}"), "scheduled Proton session refresh failed");
            }
            last_refresh = Instant::now();
        }

        pause(&running, &metrics, config.check.interval);
    }

    info!("stopping");
    Ok(())
}

fn regenerate(
    proton: &mut Proton,
    config: &Config,
    metrics: &Metrics,
    last_server: &mut Option<String>,
) -> bool {
    match vpn::regenerate(
        proton,
        &config.servers,
        &config.mikrotik,
        last_server.as_deref(),
    ) {
        Ok(vpn::Outcome::Applied(applied)) => {
            metrics.regeneration(Regeneration::Applied);
            info!(
                server = %applied.server,
                country = %applied.country,
                endpoint = %applied.endpoint,
                score = applied.score,
                load = applied.load,
                "VPN config regenerated"
            );
            *last_server = Some(applied.server.clone());
            metrics.server(CurrentServer {
                name: applied.server,
                country: applied.country,
                score: applied.score,
                load: applied.load,
            });
            true
        }
        Ok(vpn::Outcome::NoServer) => {
            metrics.regeneration(Regeneration::NoServer);
            warn!(
                countries = ?config.servers.countries,
                exclude_countries = ?config.servers.exclude_countries,
                tier = config.servers.tier,
                features = ?config.servers.features,
                max_load = config.servers.max_load,
                "no Proton server matches the filter"
            );
            false
        }
        Err(e) => {
            metrics.regeneration(Regeneration::Failed);
            error!(error = %format!("{e:#}"), "VPN regeneration failed");
            false
        }
    }
}

fn pause(running: &AtomicBool, metrics: &Metrics, total: Duration) {
    let deadline = Instant::now() + total;
    while running.load(Ordering::SeqCst) {
        metrics.heartbeat();
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep((deadline - now).min(Duration::from_secs(1)));
    }
}
