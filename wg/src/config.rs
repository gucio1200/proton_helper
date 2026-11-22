use std::env;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug, Clone)]
pub struct ProtonSeed {
    pub uid: String,
    pub access_token: Secret,
    pub refresh_token: Option<Secret>,
    pub session_id: Secret,
}

#[derive(Debug, Clone)]
pub struct MikrotikConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Secret,
    pub wg_interface: String,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct ServerFilter {
    pub countries: Vec<String>,
    pub tier: u64,
    pub features: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CheckConfig {
    pub url: String,
    pub expect: String,
    pub interval: Duration,
    pub failures: u32,
    pub cooldown: Duration,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub proton_api_url: String,
    pub app_version: String,
    pub seed: ProtonSeed,
    pub session_secret: Option<String>,
    pub session_refresh_interval: Duration,
    pub mikrotik: MikrotikConfig,
    pub servers: ServerFilter,
    pub check: CheckConfig,
    pub regenerate_on_start: bool,
    pub http_addr: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let seed = ProtonSeed {
            uid: required("AUTH_SERVER")?,
            access_token: Secret::new(required("AUTH_TOKEN")?),
            refresh_token: optional("REFRESH_TOKEN").map(Secret::new),
            session_id: Secret::new(required("SESSION_ID")?),
        };

        let wg_interface = optional("WG_INTERFACE").unwrap_or_else(|| "wg1".to_string());
        if !is_routeros_name(&wg_interface) {
            bail!("invalid WG_INTERFACE={wg_interface}: use letters, digits, '-', '_' or '.'");
        }

        let check = CheckConfig {
            url: optional("CHECK_URL")
                .unwrap_or_else(|| "https://1.1.1.1/cdn-cgi/trace".to_string()),
            expect: optional("CHECK_EXPECT").unwrap_or_else(|| "ip=".to_string()),
            interval: secs("CHECK_INTERVAL_SECS", 30)?,
            failures: parsed::<u32>("CHECK_FAILURES", 3)?.max(1),
            cooldown: secs("REGEN_COOLDOWN_SECS", 600)?,
            timeout: secs("CHECK_TIMEOUT_SECS", 15)?,
        };

        Ok(Self {
            proton_api_url: optional("PROTON_API_URL")
                .unwrap_or_else(|| "https://account.protonvpn.com".to_string())
                .trim_end_matches('/')
                .to_string(),
            app_version: required("PROTON_APP_VERSION")?,
            seed,
            session_secret: match optional("SESSION_SECRET_NAME") {
                Some(name) if name.eq_ignore_ascii_case("none") => None,
                Some(name) => Some(name),
                None => Some("protonvpn-session".to_string()),
            },
            session_refresh_interval: secs("SESSION_REFRESH_INTERVAL_SECS", 43_200)?,
            mikrotik: MikrotikConfig {
                host: required("MIKROTIK_HOST")?,
                port: parsed::<u16>("MIKROTIK_PORT", 22)?,
                user: required("MIKROTIK_USER")?,
                password: Secret::new(required("MIKROTIK_PASS")?),
                wg_interface,
                timeout: secs("MIKROTIK_TIMEOUT_SECS", 30)?,
            },
            servers: ServerFilter {
                countries: list("COUNTRIES", "RO"),
                tier: parsed::<u64>("TIER", 2)?,
                features: list("FEATURES", "P2P"),
            },
            check,
            regenerate_on_start: flag("REGENERATE_ON_START", false)?,
            http_addr: optional("HTTP_ADDR").unwrap_or_else(|| "0.0.0.0:8080".to_string()),
        })
    }
}

fn optional(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn required(name: &str) -> Result<String> {
    optional(name).ok_or_else(|| anyhow!("{name} is not set or empty"))
}

fn parsed<T>(name: &str, default: T) -> Result<T>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match optional(name) {
        Some(value) => value
            .parse::<T>()
            .with_context(|| format!("invalid {name}={value}")),
        None => Ok(default),
    }
}

fn secs(name: &str, default: u64) -> Result<Duration> {
    parsed::<u64>(name, default).map(Duration::from_secs)
}

fn flag(name: &str, default: bool) -> Result<bool> {
    match optional(name) {
        Some(value) => parse_flag(&value)
            .ok_or_else(|| anyhow!("invalid {name}={value}: expected true or false")),
        None => Ok(default),
    }
}

fn list(name: &str, default: &str) -> Vec<String> {
    optional(name)
        .unwrap_or_else(|| default.to_string())
        .split(',')
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

pub fn parse_flag(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" | "" => Some(false),
        _ => None,
    }
}

pub fn is_routeros_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_values() {
        assert_eq!(parse_flag("true"), Some(true));
        assert_eq!(parse_flag(" TRUE "), Some(true));
        assert_eq!(parse_flag("1"), Some(true));
        assert_eq!(parse_flag("false"), Some(false));
        assert_eq!(parse_flag(""), Some(false));
        assert_eq!(parse_flag("enabled"), None);
    }

    #[test]
    fn secret_is_redacted_in_debug() {
        let seed = ProtonSeed {
            uid: "uid".to_string(),
            access_token: Secret::new("access-value"),
            refresh_token: Some(Secret::new("refresh-value")),
            session_id: Secret::new("session-value"),
        };
        let printed = format!("{seed:?}");
        assert!(!printed.contains("access-value"));
        assert!(!printed.contains("refresh-value"));
        assert!(!printed.contains("session-value"));
        assert!(printed.contains("<redacted>"));
    }

    #[test]
    fn routeros_names() {
        assert!(is_routeros_name("wg1"));
        assert!(is_routeros_name("wireguard-vps"));
        assert!(!is_routeros_name("wg1\"; /system reboot"));
        assert!(!is_routeros_name(""));
    }
}
