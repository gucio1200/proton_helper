use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use reqwest::{Client, RequestBuilder, Response};
use serde_json::Value;
use tokio::time::sleep;

use crate::report::Reporter;

const READY_POLL: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const BODY_LIMIT: usize = 200;

pub struct QBittorrent {
    client: Client,
    base: String,
}

impl QBittorrent {
    pub fn new(base: &str) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("cannot build the HTTP client")?;
        Ok(Self {
            client,
            base: base.to_string(),
        })
    }

    pub async fn wait_ready(&self, reporter: &mut Reporter) {
        loop {
            match self.version().await {
                Ok(()) => {
                    reporter.qbittorrent_reachable(Instant::now());
                    return;
                }
                Err(e) => reporter.qbittorrent_unreachable(&e, Instant::now()),
            }
            sleep(READY_POLL).await;
        }
    }

    async fn version(&self) -> Result<()> {
        send(self.client.get(self.url("version")))
            .await
            .map(drop)
            .context("GET /api/v2/app/version")
    }

    pub async fn listen_port(&self) -> Result<u16> {
        let request = async {
            let resp = send(self.client.get(self.url("preferences"))).await?;
            let preferences: Value = resp.json().await.map_err(redact)?;
            parse_listen_port(&preferences)
        };
        request.await.context("GET /api/v2/app/preferences")
    }

    pub async fn set_listen_port(&self, port: u16) -> Result<()> {
        let payload = format!(r#"{{"listen_port":{port}}}"#);
        let request = self
            .client
            .post(self.url("setPreferences"))
            .form(&[("json", payload)]);
        send(request)
            .await
            .map(drop)
            .context("POST /api/v2/app/setPreferences")
    }

    fn url(&self, method: &str) -> String {
        format!("{}/api/v2/app/{method}", self.base)
    }
}

async fn send(request: RequestBuilder) -> Result<Response> {
    let resp = request.send().await.map_err(redact)?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let body = summary(&body);
    if body.is_empty() {
        bail!("HTTP {status}");
    }
    bail!("HTTP {status}: {body}");
}

fn summary(body: &str) -> String {
    body.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(BODY_LIMIT)
        .collect()
}

fn redact(e: reqwest::Error) -> anyhow::Error {
    anyhow::Error::new(e.without_url())
}

fn parse_listen_port(preferences: &Value) -> Result<u16> {
    let port = preferences
        .get("listen_port")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("listen_port is missing in the preferences"))?;
    u16::try_from(port).map_err(|_| anyhow!("listen_port {port} is out of range"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn listen_port_from_preferences() {
        let preferences = json!({"listen_port": 53606, "upnp": false});
        assert_eq!(parse_listen_port(&preferences).unwrap(), 53606);
    }

    #[test]
    fn listen_port_missing_or_invalid() {
        assert!(parse_listen_port(&json!({"upnp": false})).is_err());
        assert!(parse_listen_port(&json!({"listen_port": "53606"})).is_err());
        assert!(parse_listen_port(&json!({"listen_port": 70000})).is_err());
        assert!(parse_listen_port(&json!({"listen_port": -1})).is_err());
    }

    #[test]
    fn body_summary_is_single_line_and_bounded() {
        assert_eq!(summary("  Forbidden\n"), "Forbidden");
        assert_eq!(summary("a\r\nb\tc"), "a b c");
        assert_eq!(summary(&"x".repeat(1000)).len(), BODY_LIMIT);
    }

    #[tokio::test]
    async fn request_errors_do_not_contain_the_url() {
        let qbittorrent = QBittorrent::new("http://admin:hunter2@127.0.0.1:1").unwrap();
        let errors = [
            (
                "GET /api/v2/app/version: ",
                qbittorrent.version().await.unwrap_err(),
            ),
            (
                "GET /api/v2/app/preferences: ",
                qbittorrent.listen_port().await.unwrap_err(),
            ),
            (
                "POST /api/v2/app/setPreferences: ",
                qbittorrent.set_listen_port(42624).await.unwrap_err(),
            ),
        ];
        for (prefix, error) in errors {
            let error = format!("{error:#}");
            assert!(error.starts_with(prefix), "{error}");
            assert!(!error.contains("hunter2"), "{error}");
            assert!(!error.contains("127.0.0.1"), "{error}");
        }
    }
}
