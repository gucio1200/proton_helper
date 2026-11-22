use std::time::Duration;

use anyhow::{bail, Result};
use reqwest::blocking::Client;

pub struct TunnelCheck {
    client: Client,
    url: String,
    expect: String,
}

impl TunnelCheck {
    pub fn new(url: &str, expect: &str, timeout: Duration) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5).min(timeout))
            .timeout(timeout)
            .no_proxy()
            .user_agent(concat!("protonvpn-wg/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            client,
            url: url.to_string(),
            expect: expect.to_string(),
        })
    }

    pub fn run(&self) -> Result<String> {
        let resp = self.client.get(&self.url).send().map_err(describe)?;
        let status = resp.status();
        let body = resp.text().map_err(describe)?;
        if !status.is_success() {
            bail!("HTTP {status}");
        }
        if !body.contains(&self.expect) {
            bail!("response does not contain {:?}", self.expect);
        }
        Ok(exit_ip(&body).unwrap_or("unknown").to_string())
    }
}

fn describe(e: reqwest::Error) -> anyhow::Error {
    if e.is_timeout() {
        anyhow::anyhow!("timeout")
    } else if e.is_connect() {
        anyhow::anyhow!("connect error: {e}")
    } else {
        anyhow::anyhow!("{e}")
    }
}

pub fn exit_ip(body: &str) -> Option<&str> {
    body.lines()
        .find_map(|line| line.strip_prefix("ip="))
        .map(str::trim)
}

#[cfg(test)]
mod tests {
    use super::exit_ip;

    #[test]
    fn exit_ip_from_cloudflare_trace() {
        let body = "fl=12f1\nh=1.1.1.1\nip=203.0.113.7\nts=1795375164.1\nwarp=off\n";
        assert_eq!(exit_ip(body), Some("203.0.113.7"));
    }

    #[test]
    fn exit_ip_missing() {
        assert_eq!(exit_ip("<html>blocked</html>"), None);
    }
}
