use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

pub const REPEAT_AFTER: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapping {
    pub tcp: u16,
    pub udp: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    NatPmp,
    QBittorrent,
}

impl Stage {
    fn as_str(self) -> &'static str {
        match self {
            Stage::NatPmp => "natpmp",
            Stage::QBittorrent => "qbittorrent",
        }
    }
}

struct Streak<K> {
    key: K,
    count: u32,
    since: Instant,
    logged: Instant,
}

impl<K: PartialEq> Streak<K> {
    fn record(
        slot: &mut Option<Self>,
        key: K,
        now: Instant,
        repeat: Duration,
    ) -> Option<(u32, Duration)> {
        let Some(streak) = slot else {
            *slot = Some(Streak {
                key,
                count: 1,
                since: now,
                logged: now,
            });
            return Some((1, Duration::ZERO));
        };
        streak.count += 1;
        if streak.key == key && now.duration_since(streak.logged) < repeat {
            return None;
        }
        streak.key = key;
        streak.logged = now;
        Some((streak.count, now.duration_since(streak.since)))
    }
}

pub struct Reporter {
    repeat: Duration,
    mapping: Option<Mapping>,
    failure: Option<Streak<(Stage, String)>>,
    outage: Option<Streak<String>>,
    reachable: bool,
}

impl Reporter {
    pub fn new(repeat: Duration) -> Self {
        Self {
            repeat,
            mapping: None,
            failure: None,
            outage: None,
            reachable: false,
        }
    }

    pub fn mapped(&mut self, current: Mapping) {
        match self.mapping.replace(current) {
            None => info!(
                port = current.tcp,
                udp_port = current.udp,
                "port mapping established"
            ),
            Some(previous) if previous != current => info!(
                port = current.tcp,
                udp_port = current.udp,
                previous_port = previous.tcp,
                previous_udp_port = previous.udp,
                "forwarded port changed"
            ),
            Some(_) => debug!(
                port = current.tcp,
                udp_port = current.udp,
                "port mapping refreshed"
            ),
        }
    }

    pub fn listen_port(&self, previous: u16, current: u16) {
        if previous == current {
            debug!(port = current, "qBittorrent listen port up to date");
        } else {
            info!(
                port = current,
                previous_port = previous,
                "qBittorrent listen port updated"
            );
        }
    }

    pub fn failed(&mut self, stage: Stage, error: &anyhow::Error, now: Instant) {
        let error = format!("{error:#}");
        match Streak::record(&mut self.failure, (stage, error.clone()), now, self.repeat) {
            Some((failures, _)) => warn!(
                stage = stage.as_str(),
                failures,
                error = %error,
                "port refresh failed"
            ),
            None => debug!(stage = stage.as_str(), error = %error, "port refresh failed again"),
        }
    }

    pub fn recovered(&mut self, stage: Stage, now: Instant) {
        if self
            .failure
            .as_ref()
            .is_some_and(|streak| streak.key.0 == stage)
        {
            self.succeeded(now);
        }
    }

    pub fn succeeded(&mut self, now: Instant) {
        if let Some(streak) = self.failure.take() {
            info!(
                failures = streak.count,
                downtime_secs = now.duration_since(streak.since).as_secs(),
                "port refresh recovered"
            );
        }
    }

    pub fn qbittorrent_unreachable(&mut self, error: &anyhow::Error, now: Instant) {
        let error = format!("{error:#}");
        match Streak::record(&mut self.outage, error.clone(), now, self.repeat) {
            Some((attempts, waiting)) if self.reachable || waiting >= self.repeat => warn!(
                attempts,
                waiting_secs = waiting.as_secs(),
                error = %error,
                "qBittorrent WebUI unreachable"
            ),
            Some(_) => info!(error = %error, "waiting for qBittorrent WebUI"),
            None => debug!(error = %error, "qBittorrent WebUI still unreachable"),
        }
    }

    pub fn qbittorrent_reachable(&mut self, now: Instant) {
        self.reachable = true;
        if let Some(streak) = self.outage.take() {
            info!(
                waited_secs = now.duration_since(streak.since).as_secs(),
                "qBittorrent WebUI reachable"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use anyhow::anyhow;
    use serde_json::Value;

    use super::*;

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture(f: impl FnOnce()) -> Vec<Value> {
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let output = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn mapping(port: u16) -> Mapping {
        Mapping {
            tcp: port,
            udp: port,
        }
    }

    fn at(start: Instant, secs: u64) -> Instant {
        start + Duration::from_secs(secs)
    }

    #[test]
    fn first_mapping_logs_once() {
        let mut reporter = Reporter::new(REPEAT_AFTER);
        let lines = capture(|| reporter.mapped(mapping(53606)));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["level"], "INFO");
        assert_eq!(lines[0]["message"], "port mapping established");
        assert_eq!(lines[0]["port"], 53606);
        assert_eq!(lines[0]["udp_port"], 53606);
    }

    #[test]
    fn same_port_twice_logs_nothing() {
        let mut reporter = Reporter::new(REPEAT_AFTER);
        reporter.mapped(mapping(53606));
        let lines = capture(|| {
            reporter.mapped(mapping(53606));
            reporter.listen_port(53606, 53606);
            reporter.succeeded(Instant::now());
            reporter.mapped(mapping(53606));
            reporter.listen_port(53606, 53606);
            reporter.succeeded(Instant::now());
        });
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn port_change_logs_once() {
        let mut reporter = Reporter::new(REPEAT_AFTER);
        reporter.mapped(mapping(53606));
        let lines = capture(|| {
            reporter.mapped(mapping(42624));
            reporter.mapped(mapping(42624));
            reporter.mapped(mapping(42624));
        });
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["message"], "forwarded port changed");
        assert_eq!(lines[0]["port"], 42624);
        assert_eq!(lines[0]["previous_port"], 53606);
    }

    #[test]
    fn udp_only_change_is_reported() {
        let mut reporter = Reporter::new(REPEAT_AFTER);
        reporter.mapped(mapping(53606));
        let lines = capture(|| {
            reporter.mapped(Mapping {
                tcp: 53606,
                udp: 53607,
            })
        });
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["udp_port"], 53607);
        assert_eq!(lines[0]["previous_udp_port"], 53606);
    }

    #[test]
    fn listen_port_update_logs_once() {
        let reporter = Reporter::new(REPEAT_AFTER);
        let lines = capture(|| {
            reporter.listen_port(53606, 42624);
            reporter.listen_port(42624, 42624);
        });
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["message"], "qBittorrent listen port updated");
        assert_eq!(lines[0]["port"], 42624);
        assert_eq!(lines[0]["previous_port"], 53606);
    }

    #[test]
    fn recovery_after_error_logs_once() {
        let mut reporter = Reporter::new(REPEAT_AFTER);
        let start = Instant::now();
        let error = anyhow!("network failure").context("NAT-PMP TCP mapping");
        let lines = capture(|| {
            reporter.failed(Stage::NatPmp, &error, start);
            reporter.failed(Stage::NatPmp, &error, at(start, 30));
            reporter.failed(Stage::NatPmp, &error, at(start, 60));
        });
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["level"], "WARN");
        assert_eq!(lines[0]["message"], "port refresh failed");
        assert_eq!(lines[0]["stage"], "natpmp");
        assert_eq!(lines[0]["failures"], 1);
        assert_eq!(lines[0]["error"], "NAT-PMP TCP mapping: network failure");

        let lines = capture(|| {
            reporter.succeeded(at(start, 90));
            reporter.succeeded(at(start, 120));
        });
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["level"], "INFO");
        assert_eq!(lines[0]["message"], "port refresh recovered");
        assert_eq!(lines[0]["failures"], 3);
        assert_eq!(lines[0]["downtime_secs"], 90);
    }

    #[test]
    fn mapping_recovery_is_logged_without_waiting_for_qbittorrent() {
        let mut reporter = Reporter::new(REPEAT_AFTER);
        let start = Instant::now();
        let error = anyhow!("network failure");
        reporter.failed(Stage::NatPmp, &error, start);
        let lines = capture(|| {
            reporter.recovered(Stage::QBittorrent, at(start, 30));
            reporter.recovered(Stage::NatPmp, at(start, 60));
            reporter.recovered(Stage::NatPmp, at(start, 90));
        });
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["message"], "port refresh recovered");
        assert_eq!(lines[0]["downtime_secs"], 60);
    }

    #[test]
    fn qbittorrent_failure_is_not_cleared_by_mapping_success() {
        let mut reporter = Reporter::new(REPEAT_AFTER);
        let start = Instant::now();
        let error = anyhow!("HTTP 403 Forbidden");
        reporter.failed(Stage::QBittorrent, &error, start);
        let lines = capture(|| reporter.recovered(Stage::NatPmp, at(start, 30)));
        assert!(lines.is_empty(), "{lines:?}");
        let lines = capture(|| reporter.succeeded(at(start, 60)));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["message"], "port refresh recovered");
    }

    #[test]
    fn changed_error_logs_again() {
        let mut reporter = Reporter::new(REPEAT_AFTER);
        let start = Instant::now();
        let refused = anyhow!("connection refused");
        let forbidden = anyhow!("HTTP 403 Forbidden");
        let lines = capture(|| {
            reporter.failed(Stage::QBittorrent, &refused, start);
            reporter.failed(Stage::QBittorrent, &forbidden, at(start, 30));
            reporter.failed(Stage::QBittorrent, &forbidden, at(start, 60));
            reporter.failed(Stage::NatPmp, &forbidden, at(start, 90));
        });
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[1]["error"], "HTTP 403 Forbidden");
        assert_eq!(lines[1]["failures"], 2);
        assert_eq!(lines[2]["stage"], "natpmp");
    }

    #[test]
    fn persistent_error_is_repeated_after_interval() {
        let mut reporter = Reporter::new(REPEAT_AFTER);
        let start = Instant::now();
        let error = anyhow!("network failure");
        let lines = capture(|| {
            for secs in (0..=1200).step_by(30) {
                reporter.failed(Stage::NatPmp, &error, at(start, secs));
            }
        });
        let failures: Vec<&Value> = lines.iter().map(|line| &line["failures"]).collect();
        assert_eq!(failures, [1, 21, 41]);
    }

    #[test]
    fn qbittorrent_startup_wait_is_info() {
        let mut reporter = Reporter::new(REPEAT_AFTER);
        let start = Instant::now();
        let error = anyhow!("connection refused");
        let lines = capture(|| {
            reporter.qbittorrent_unreachable(&error, start);
            reporter.qbittorrent_unreachable(&error, at(start, 5));
            reporter.qbittorrent_reachable(at(start, 10));
            reporter.qbittorrent_reachable(at(start, 40));
        });
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0]["level"], "INFO");
        assert_eq!(lines[0]["message"], "waiting for qBittorrent WebUI");
        assert_eq!(lines[1]["message"], "qBittorrent WebUI reachable");
        assert_eq!(lines[1]["waited_secs"], 10);
    }

    #[test]
    fn qbittorrent_outage_after_start_is_warning() {
        let mut reporter = Reporter::new(REPEAT_AFTER);
        let start = Instant::now();
        let error = anyhow!("connection refused");
        reporter.qbittorrent_reachable(start);
        let lines = capture(|| {
            for secs in (60..=700).step_by(5) {
                reporter.qbittorrent_unreachable(&error, at(start, secs));
            }
            reporter.qbittorrent_reachable(at(start, 705));
        });
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0]["level"], "WARN");
        assert_eq!(lines[0]["message"], "qBittorrent WebUI unreachable");
        assert_eq!(lines[1]["level"], "WARN");
        assert_eq!(lines[1]["waiting_secs"], 600);
        assert_eq!(lines[2]["message"], "qBittorrent WebUI reachable");
        assert_eq!(lines[2]["waited_secs"], 645);
    }

    #[test]
    fn startup_wait_turns_into_warning_after_interval() {
        let mut reporter = Reporter::new(REPEAT_AFTER);
        let start = Instant::now();
        let error = anyhow!("connection refused");
        let lines = capture(|| {
            for secs in (0..=600).step_by(5) {
                reporter.qbittorrent_unreachable(&error, at(start, secs));
            }
        });
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0]["level"], "INFO");
        assert_eq!(lines[1]["level"], "WARN");
        assert_eq!(lines[1]["waiting_secs"], 600);
    }
}
