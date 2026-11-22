use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::{Client, Response};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, COOKIE, RETRY_AFTER, SET_COOKIE};
use reqwest::{Method, StatusCode};
use serde_json::Value;
use tracing::{error, info, warn};

use crate::config::{Config, Secret};
use crate::metrics::{Metrics, Refresh};
use crate::session::{Origin, Session, SessionStore};

const MAX_ATTEMPTS: u32 = 4;

pub struct Proton {
    http: Client,
    base: String,
    app_version: String,
    session: Session,
    store: SessionStore,
    metrics: Arc<Metrics>,
}

impl Proton {
    pub fn new(
        config: &Config,
        store: SessionStore,
        metrics: Arc<Metrics>,
    ) -> Result<(Self, Origin)> {
        let (session, origin) = store.load(&config.seed);
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("protonvpn-wg/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok((
            Self {
                http,
                base: config.proton_api_url.clone(),
                app_version: config.app_version.clone(),
                session,
                store,
                metrics,
            },
            origin,
        ))
    }

    pub fn can_refresh(&self) -> bool {
        self.session.refresh_token.is_some()
    }

    pub fn get(&mut self, path: &str) -> Result<Value> {
        self.call(Method::GET, path, None)
    }

    pub fn post(&mut self, path: &str, body: &Value) -> Result<Value> {
        self.call(Method::POST, path, Some(body))
    }

    fn headers(&self, cookie: &str) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert("x-pm-appversion", HeaderValue::from_str(&self.app_version)?);
        headers.insert("x-pm-uid", HeaderValue::from_str(&self.session.uid)?);
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.protonmail.v1+json"),
        );
        let mut cookie = HeaderValue::from_str(cookie)?;
        cookie.set_sensitive(true);
        headers.insert(COOKIE, cookie);
        Ok(headers)
    }

    fn api_cookie(&self) -> String {
        format!(
            "AUTH-{}={}; Session-Id={}",
            self.session.uid,
            self.session.access_token.expose(),
            self.session.session_id.expose()
        )
    }

    fn call(&mut self, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let mut refreshed = false;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut request = self
                .http
                .request(method.clone(), &url)
                .headers(self.headers(&self.api_cookie())?);
            if let Some(body) = body {
                request = request.json(body);
            }
            let resp = match request.send() {
                Ok(resp) => resp,
                Err(e) if attempt < MAX_ATTEMPTS && (e.is_timeout() || e.is_connect()) => {
                    backoff(path, attempt, None, &e.to_string());
                    continue;
                }
                Err(e) => {
                    return Err(anyhow!(e).context(format!("Proton API {path} request failed")))
                }
            };
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && !refreshed && self.can_refresh() {
                warn!(path, "Proton access token expired, refreshing the session");
                self.refresh()?;
                refreshed = true;
                continue;
            }
            if (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                && attempt < MAX_ATTEMPTS
            {
                let retry_after = retry_after(&resp);
                backoff(path, attempt, retry_after, &format!("HTTP {status}"));
                continue;
            }
            let json: Value = resp.json().with_context(|| {
                format!("Proton API {path} answered HTTP {status} without JSON")
            })?;
            if status.is_success() && matches!(json["Code"].as_u64(), Some(1000) | Some(1001)) {
                return Ok(json);
            }
            bail!(api_error(path, status, &json, self.can_refresh()));
        }
    }

    pub fn refresh(&mut self) -> Result<()> {
        let refresh_token = self.session.refresh_token.clone().ok_or_else(|| {
            anyhow!("no REFRESH_TOKEN, so the Proton session cannot be renewed; log in again and update the protonvpn item in 1Password")
        })?;
        let cookie = format!(
            "REFRESH-{}={}; Session-Id={}",
            self.session.uid,
            refresh_token.expose(),
            self.session.session_id.expose()
        );
        let resp = self
            .http
            .post(format!("{}/api/auth/refresh", self.base))
            .headers(self.headers(&cookie)?)
            .send();
        let resp = match resp {
            Ok(resp) => resp,
            Err(e) => {
                self.metrics.refresh(Refresh::Failed);
                return Err(anyhow!(e).context("Proton session refresh request failed"));
            }
        };
        let status = resp.status();
        let cookies = set_cookies(resp.headers());
        let body: Value = resp.json().unwrap_or(Value::Null);
        match status.as_u16() {
            200..=299 => {}
            400 | 401 | 422 => {
                self.metrics.refresh(Refresh::Invalid);
                bail!(
                    "Proton rejected the session refresh (HTTP {status}, {}); log in again and update AUTH_SERVER, AUTH_TOKEN, REFRESH_TOKEN and SESSION_ID in 1Password",
                    api_message(&body)
                );
            }
            _ => {
                self.metrics.refresh(Refresh::Failed);
                bail!(
                    "Proton session refresh failed (HTTP {status}, {}), retrying later",
                    api_message(&body)
                );
            }
        }

        let uid = self.session.uid.clone();
        let access = cookies
            .get(&format!("AUTH-{uid}"))
            .cloned()
            .or_else(|| body["AccessToken"].as_str().map(str::to_string));
        let Some(access) = access else {
            self.metrics.refresh(Refresh::Failed);
            bail!("Proton session refresh answered without a new access token");
        };
        self.session.access_token = Secret::new(access);
        if let Some(refresh) = cookies
            .get(&format!("REFRESH-{uid}"))
            .cloned()
            .or_else(|| body["RefreshToken"].as_str().map(str::to_string))
        {
            self.session.refresh_token = Some(Secret::new(refresh));
        }
        if let Some(session_id) = cookies.get("Session-Id") {
            self.session.session_id = Secret::new(session_id.clone());
        }
        self.metrics.refresh(Refresh::Ok);
        info!("Proton session refreshed");

        match self.store.save(&self.session) {
            Ok(()) => self.metrics.session_persisted(true),
            Err(e) => {
                self.metrics.session_persisted(false);
                error!(error = %format!("{e:#}"), "cannot store the rotated Proton session, it is lost on restart");
            }
        }
        Ok(())
    }
}

fn backoff(path: &str, attempt: u32, retry_after: Option<Duration>, reason: &str) {
    let delay = retry_after
        .unwrap_or_else(|| Duration::from_secs(1 << attempt.min(5)))
        .min(Duration::from_secs(60));
    warn!(
        path,
        attempt,
        delay_secs = delay.as_secs(),
        reason,
        "Proton API temporarily unavailable, retrying"
    );
    thread::sleep(delay);
}

fn retry_after(resp: &Response) -> Option<Duration> {
    resp.headers()
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

pub fn set_cookies(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(parse_set_cookie)
        .collect()
}

fn parse_set_cookie(header: &str) -> Option<(String, String)> {
    let pair = header.split(';').next()?.trim();
    let (name, value) = pair.split_once('=')?;
    let (name, value) = (name.trim(), value.trim());
    if name.is_empty() || value.is_empty() {
        None
    } else {
        Some((name.to_string(), value.to_string()))
    }
}

fn api_message(body: &Value) -> String {
    format!(
        "Code {}, {}",
        body["Code"],
        body["Error"].as_str().unwrap_or("no error message")
    )
}

pub fn api_error(path: &str, status: StatusCode, body: &Value, can_refresh: bool) -> String {
    let hint = match (body["Code"].as_u64(), status) {
        (Some(5003), _) => " - update x-pm-appversion in 1Password",
        (_, StatusCode::UNAUTHORIZED) if !can_refresh => {
            " - the Proton session expired and there is no REFRESH_TOKEN; log in again and update the protonvpn item in 1Password"
        }
        _ => "",
    };
    format!(
        "Proton API {path} failed (HTTP {status}, {}){hint}",
        api_message(body)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn parses_set_cookie_headers() {
        let mut headers = HeaderMap::new();
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("AUTH-abc=new-access; Path=/api/; Secure; HttpOnly"),
        );
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("REFRESH-abc=%7B%22a%22%7D; Path=/api/auth/refresh"),
        );
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("Tag=; Expires=Thu, 01 Jan 1970 00:00:00 GMT"),
        );
        let cookies = set_cookies(&headers);
        assert_eq!(
            cookies.get("AUTH-abc").map(String::as_str),
            Some("new-access")
        );
        assert_eq!(
            cookies.get("REFRESH-abc").map(String::as_str),
            Some("%7B%22a%22%7D")
        );
        assert!(!cookies.contains_key("Tag"));
    }

    #[test]
    fn api_error_hints() {
        let outdated = serde_json::json!({"Code": 5003, "Error": "This web page is out of date"});
        assert!(api_error("/x", StatusCode::BAD_REQUEST, &outdated, true)
            .ends_with("update x-pm-appversion in 1Password"));
        let expired = serde_json::json!({"Code": 401, "Error": "Invalid access token"});
        assert!(
            api_error("/x", StatusCode::UNAUTHORIZED, &expired, false).contains("REFRESH_TOKEN")
        );
        assert!(!api_error("/x", StatusCode::UNAUTHORIZED, &expired, true).contains(" - "));
    }
}
