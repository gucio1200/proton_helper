use std::env;
use std::net::Ipv4Addr;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::Url;

#[derive(Debug, Clone)]
pub struct Config {
    pub gateway: Ipv4Addr,
    pub internal_port: u16,
    pub public_port: u16,
    pub lifetime: u32,
    pub refresh_interval: Duration,
    pub qbittorrent_url: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|name| env::var(name).ok())
    }

    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let vars = Vars(lookup);
        let refresh_interval = vars.parsed::<u64>("REFRESH_INTERVAL", 30)?;
        if refresh_interval == 0 {
            bail!("REFRESH_INTERVAL must be greater than 0");
        }
        let host = vars
            .optional("QBITTORRENT_HOST")
            .unwrap_or_else(|| "http://127.0.0.1".to_string());
        let port = vars.parsed::<u16>("QBITTORRENT_PORT", 8080)?;

        Ok(Self {
            gateway: vars.parsed("NATPMP_GATEWAY", Ipv4Addr::new(10, 2, 0, 1))?,
            internal_port: vars.parsed("INTERNAL_PORT", 0)?,
            public_port: vars.parsed("PUBLIC_PORT", 1)?,
            lifetime: vars.parsed("MAPPING_LIFETIME", 60)?,
            refresh_interval: Duration::from_secs(refresh_interval),
            qbittorrent_url: qbittorrent_url(&host, port)?,
        })
    }

    pub fn qbittorrent_display(&self) -> String {
        Url::parse(&self.qbittorrent_url)
            .map(|url| url.origin().ascii_serialization())
            .unwrap_or_else(|_| "invalid".to_string())
    }
}

struct Vars<F>(F);

impl<F: Fn(&str) -> Option<String>> Vars<F> {
    fn optional(&self, name: &str) -> Option<String> {
        (self.0)(name)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    fn parsed<T>(&self, name: &str, default: T) -> Result<T>
    where
        T: FromStr,
        T::Err: std::error::Error + Send + Sync + 'static,
    {
        match self.optional(name) {
            Some(value) => value
                .parse::<T>()
                .with_context(|| format!("invalid {name}={value}")),
            None => Ok(default),
        }
    }
}

fn qbittorrent_url(host: &str, port: u16) -> Result<String> {
    let base = format!("{}:{port}", host.trim_end_matches('/'));
    let url = Url::parse(&base).context("invalid QBITTORRENT_HOST or QBITTORRENT_PORT")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("invalid QBITTORRENT_HOST: expected http://host or https://host");
    }
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(vars: &[(&str, &str)]) -> Result<Config> {
        Config::from_lookup(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
    }

    #[test]
    fn defaults() {
        let config = config(&[]).unwrap();
        assert_eq!(config.gateway, Ipv4Addr::new(10, 2, 0, 1));
        assert_eq!(config.internal_port, 0);
        assert_eq!(config.public_port, 1);
        assert_eq!(config.lifetime, 60);
        assert_eq!(config.refresh_interval, Duration::from_secs(30));
        assert_eq!(config.qbittorrent_url, "http://127.0.0.1:8080");
    }

    #[test]
    fn overrides() {
        let config = config(&[
            ("NATPMP_GATEWAY", "10.96.0.1"),
            ("INTERNAL_PORT", "5"),
            ("PUBLIC_PORT", "6"),
            ("MAPPING_LIFETIME", "120"),
            ("REFRESH_INTERVAL", " 45 "),
            ("QBITTORRENT_HOST", "http://localhost/"),
            ("QBITTORRENT_PORT", "8090"),
        ])
        .unwrap();
        assert_eq!(config.gateway, Ipv4Addr::new(10, 96, 0, 1));
        assert_eq!(config.internal_port, 5);
        assert_eq!(config.public_port, 6);
        assert_eq!(config.lifetime, 120);
        assert_eq!(config.refresh_interval, Duration::from_secs(45));
        assert_eq!(config.qbittorrent_url, "http://localhost:8090");
    }

    #[test]
    fn empty_value_uses_default() {
        let config = config(&[("REFRESH_INTERVAL", ""), ("QBITTORRENT_PORT", " ")]).unwrap();
        assert_eq!(config.refresh_interval, Duration::from_secs(30));
        assert_eq!(config.qbittorrent_url, "http://127.0.0.1:8080");
    }

    #[test]
    fn invalid_values_are_rejected() {
        let error = format!(
            "{:#}",
            config(&[("QBITTORRENT_PORT", "80800")]).unwrap_err()
        );
        assert!(error.contains("QBITTORRENT_PORT=80800"), "{error}");
        assert!(config(&[("NATPMP_GATEWAY", "gateway")]).is_err());
        assert!(config(&[("REFRESH_INTERVAL", "0")]).is_err());
        assert!(config(&[("QBITTORRENT_HOST", "127.0.0.1")]).is_err());
        assert!(config(&[("QBITTORRENT_HOST", "ftp://127.0.0.1")]).is_err());
    }

    #[test]
    fn display_hides_credentials() {
        let config = config(&[("QBITTORRENT_HOST", "http://admin:hunter2@127.0.0.1")]).unwrap();
        assert_eq!(config.qbittorrent_display(), "http://127.0.0.1:8080");
    }

    #[test]
    fn host_error_does_not_echo_the_value() {
        let error = format!(
            "{:#}",
            config(&[("QBITTORRENT_HOST", "http://admin:hunter2@127.0.0.1:8080")]).unwrap_err()
        );
        assert!(!error.contains("hunter2"), "{error}");
    }
}
