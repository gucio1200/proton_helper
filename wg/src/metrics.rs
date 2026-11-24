use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use tracing::{info, warn};

#[derive(Default)]
pub struct Metrics {
    tunnel_up: AtomicU64,
    tunnel_checked: AtomicU64,
    checks_ok: AtomicU64,
    checks_failed: AtomicU64,
    last_success: AtomicU64,
    regen_applied: AtomicU64,
    regen_no_server: AtomicU64,
    regen_failed: AtomicU64,
    refresh_ok: AtomicU64,
    refresh_invalid: AtomicU64,
    refresh_failed: AtomicU64,
    session_invalid: AtomicU64,
    session_unconfirmed: AtomicU64,
    session_persist_failed: AtomicU64,
    heartbeat: AtomicU64,
    server: Mutex<Option<CurrentServer>>,
}

pub struct CurrentServer {
    pub name: String,
    pub country: String,
    pub score: f64,
    pub load: u64,
}

pub enum Regeneration {
    Applied,
    NoServer,
    Failed,
}

pub enum Refresh {
    Ok,
    Invalid,
    Failed,
}

impl Metrics {
    pub fn heartbeat(&self) {
        self.heartbeat.store(now(), Ordering::Relaxed);
    }

    pub fn check(&self, ok: bool) {
        self.tunnel_checked.store(1, Ordering::Relaxed);
        self.tunnel_up.store(u64::from(ok), Ordering::Relaxed);
        if ok {
            self.checks_ok.fetch_add(1, Ordering::Relaxed);
            self.last_success.store(now(), Ordering::Relaxed);
        } else {
            self.checks_failed.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn regeneration(&self, outcome: Regeneration) {
        let counter = match outcome {
            Regeneration::Applied => &self.regen_applied,
            Regeneration::NoServer => &self.regen_no_server,
            Regeneration::Failed => &self.regen_failed,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn refresh(&self, outcome: Refresh) {
        match outcome {
            Refresh::Ok => {
                self.refresh_ok.fetch_add(1, Ordering::Relaxed);
                self.session_invalid.store(0, Ordering::Relaxed);
            }
            Refresh::Invalid => {
                self.refresh_invalid.fetch_add(1, Ordering::Relaxed);
                self.session_invalid.store(1, Ordering::Relaxed);
            }
            Refresh::Failed => {
                self.refresh_failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn session_confirmed(&self, confirmed: bool) {
        self.session_unconfirmed
            .store(u64::from(!confirmed), Ordering::Relaxed);
    }

    pub fn session_persisted(&self, ok: bool) {
        if !ok {
            self.session_persist_failed.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn server(&self, current: CurrentServer) {
        if let Ok(mut slot) = self.server.lock() {
            *slot = Some(current);
        }
    }

    pub fn healthy(&self, max_age: Duration) -> bool {
        now().saturating_sub(self.heartbeat.load(Ordering::Relaxed)) <= max_age.as_secs()
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        let load = |value: &AtomicU64| value.load(Ordering::Relaxed);
        let checked = load(&self.tunnel_checked) == 1;

        gauge(
            &mut out,
            "protonvpn_tunnel_up",
            "1 when the last tunnel check returned the expected content",
            if checked {
                load(&self.tunnel_up) as f64
            } else {
                f64::NAN
            },
        );
        gauge(
            &mut out,
            "protonvpn_tunnel_last_success_timestamp_seconds",
            "Unix time of the last successful tunnel check",
            load(&self.last_success) as f64,
        );
        counter(
            &mut out,
            "protonvpn_tunnel_checks_total",
            "Tunnel checks by result",
            &[
                ("result=\"ok\"", load(&self.checks_ok)),
                ("result=\"failed\"", load(&self.checks_failed)),
            ],
        );
        counter(
            &mut out,
            "protonvpn_regenerations_total",
            "VPN config regenerations by result",
            &[
                ("result=\"applied\"", load(&self.regen_applied)),
                ("result=\"no_server\"", load(&self.regen_no_server)),
                ("result=\"failed\"", load(&self.regen_failed)),
            ],
        );
        counter(
            &mut out,
            "protonvpn_session_refreshes_total",
            "Proton session refreshes by result",
            &[
                ("result=\"ok\"", load(&self.refresh_ok)),
                ("result=\"invalid\"", load(&self.refresh_invalid)),
                ("result=\"failed\"", load(&self.refresh_failed)),
            ],
        );
        gauge(
            &mut out,
            "protonvpn_session_invalid",
            "1 when Proton rejected the session and a new login is needed",
            load(&self.session_invalid) as f64,
        );
        gauge(
            &mut out,
            "protonvpn_session_unconfirmed",
            "1 while the current Proton tokens have not been used yet, so the refresh token is still provisional",
            load(&self.session_unconfirmed) as f64,
        );
        counter(
            &mut out,
            "protonvpn_session_persist_failures_total",
            "Failed writes of the rotated session to the Kubernetes secret",
            &[("", load(&self.session_persist_failed))],
        );
        gauge(
            &mut out,
            "protonvpn_heartbeat_timestamp_seconds",
            "Unix time of the last main loop heartbeat",
            load(&self.heartbeat) as f64,
        );
        if let Ok(slot) = self.server.lock() {
            if let Some(current) = slot.as_ref() {
                let labels = format!(
                    "server=\"{}\",country=\"{}\"",
                    escape(&current.name),
                    escape(&current.country)
                );
                let _ = writeln!(
                    out,
                    "# HELP protonvpn_server_info Proton server applied by the last regeneration\n# TYPE protonvpn_server_info gauge\nprotonvpn_server_info{{{labels}}} 1"
                );
                gauge(
                    &mut out,
                    "protonvpn_server_score",
                    "Proton Score of the applied server when it was selected (lower is better)",
                    current.score,
                );
                gauge(
                    &mut out,
                    "protonvpn_server_load",
                    "Proton load percent of the applied server when it was selected",
                    current.load as f64,
                );
            }
        }
        out
    }
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn gauge(out: &mut String, name: &str, help: &str, value: f64) {
    let _ = writeln!(
        out,
        "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}"
    );
}

fn counter(out: &mut String, name: &str, help: &str, series: &[(&str, u64)]) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter");
    for (labels, value) in series {
        if labels.is_empty() {
            let _ = writeln!(out, "{name} {value}");
        } else {
            let _ = writeln!(out, "{name}{{{labels}}} {value}");
        }
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn serve(addr: &str, metrics: Arc<Metrics>, max_heartbeat_age: Duration) -> Result<()> {
    let server =
        tiny_http::Server::http(addr).map_err(|e| anyhow!("cannot listen on {addr}: {e}"))?;
    info!(addr, "HTTP endpoint listening for /metrics and /healthz");
    thread::Builder::new()
        .name("http".to_string())
        .spawn(move || {
            for request in server.incoming_requests() {
                let path = request.url().split('?').next().unwrap_or("").to_string();
                let (status, body, content_type) = match path.as_str() {
                    "/metrics" => (200, metrics.render(), "text/plain; version=0.0.4"),
                    "/healthz" | "/readyz" => {
                        if metrics.healthy(max_heartbeat_age) {
                            (200, "ok\n".to_string(), "text/plain")
                        } else {
                            (503, "main loop stalled\n".to_string(), "text/plain")
                        }
                    }
                    _ => (404, "not found\n".to_string(), "text/plain"),
                };
                let mut response = tiny_http::Response::from_string(body).with_status_code(status);
                if let Ok(header) =
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
                {
                    response.add_header(header);
                }
                if let Err(e) = request.respond(response) {
                    warn!(error = %e, path, "failed to answer HTTP request");
                }
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_before_first_check_reports_unknown_tunnel() {
        let metrics = Metrics::default();
        assert!(metrics.render().contains("protonvpn_tunnel_up NaN"));
        metrics.check(true);
        let rendered = metrics.render();
        assert!(rendered.contains("protonvpn_tunnel_up 1"));
        assert!(rendered.contains("protonvpn_tunnel_checks_total{result=\"ok\"} 1"));
    }

    #[test]
    fn renders_current_server() {
        let metrics = Metrics::default();
        assert!(!metrics.render().contains("protonvpn_server_info"));
        metrics.server(CurrentServer {
            name: "DE#12".to_string(),
            country: "DE".to_string(),
            score: 1.25,
            load: 37,
        });
        let rendered = metrics.render();
        assert!(rendered.contains("protonvpn_server_info{server=\"DE#12\",country=\"DE\"} 1"));
        assert!(rendered.contains("protonvpn_server_score 1.25"));
        assert!(rendered.contains("protonvpn_server_load 37"));
    }

    #[test]
    fn heartbeat_health() {
        let metrics = Metrics::default();
        assert!(!metrics.healthy(Duration::from_secs(60)));
        metrics.heartbeat();
        assert!(metrics.healthy(Duration::from_secs(60)));
    }
}
