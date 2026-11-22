use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use reqwest::blocking::{Client, Response};
use reqwest::{Certificate, StatusCode};
use serde_json::{json, Value};

const SA_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

pub struct Kube {
    http: Client,
    base: String,
    namespace: String,
}

impl Kube {
    pub fn in_cluster() -> Result<Option<Self>> {
        let (host, port) = match (
            env::var("KUBERNETES_SERVICE_HOST"),
            env::var("KUBERNETES_SERVICE_PORT"),
        ) {
            (Ok(host), Ok(port)) => (host, port),
            _ => return Ok(None),
        };
        let ca = fs::read(format!("{SA_DIR}/ca.crt")).context(
            "cannot read the service account CA (is automountServiceAccountToken enabled?)",
        )?;
        let namespace = fs::read_to_string(format!("{SA_DIR}/namespace"))
            .context("cannot read the service account namespace")?
            .trim()
            .to_string();
        let http = Client::builder()
            .add_root_certificate(Certificate::from_pem(&ca)?)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()?;
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host
        };
        Ok(Some(Self {
            http,
            base: format!("https://{host}:{port}"),
            namespace,
        }))
    }

    fn token() -> Result<String> {
        Ok(fs::read_to_string(format!("{SA_DIR}/token"))
            .context("cannot read the service account token")?
            .trim()
            .to_string())
    }

    fn secrets_url(&self) -> String {
        format!("{}/api/v1/namespaces/{}/secrets", self.base, self.namespace)
    }

    pub fn read_secret(&self, name: &str) -> Result<Option<BTreeMap<String, String>>> {
        let resp = self
            .http
            .get(format!("{}/{name}", self.secrets_url()))
            .bearer_auth(Self::token()?)
            .send()
            .with_context(|| format!("GET secret {name}"))?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body = success_json(resp, "GET", name)?;
        let mut data = BTreeMap::new();
        if let Some(entries) = body["data"].as_object() {
            for (key, value) in entries {
                if let Some(encoded) = value.as_str() {
                    let decoded = STANDARD
                        .decode(encoded)
                        .with_context(|| format!("secret {name} key {key} is not base64"))?;
                    data.insert(key.clone(), String::from_utf8(decoded)?);
                }
            }
        }
        Ok(Some(data))
    }

    pub fn write_secret(&self, name: &str, data: &BTreeMap<String, String>) -> Result<()> {
        let token = Self::token()?;
        let resp = self
            .http
            .patch(format!("{}/{name}", self.secrets_url()))
            .bearer_auth(&token)
            .header("Content-Type", "application/merge-patch+json")
            .body(json!({ "stringData": data }).to_string())
            .send()
            .with_context(|| format!("PATCH secret {name}"))?;
        if resp.status() != StatusCode::NOT_FOUND {
            success_json(resp, "PATCH", name)?;
            return Ok(());
        }
        let resp = self
            .http
            .post(self.secrets_url())
            .bearer_auth(&token)
            .json(&json!({
                "apiVersion": "v1",
                "kind": "Secret",
                "metadata": {
                    "name": name,
                    "labels": { "app.kubernetes.io/managed-by": "protonvpn-wg" }
                },
                "type": "Opaque",
                "stringData": data
            }))
            .send()
            .with_context(|| format!("POST secret {name}"))?;
        success_json(resp, "POST", name)?;
        Ok(())
    }
}

fn success_json(resp: Response, verb: &str, name: &str) -> Result<Value> {
    let status = resp.status();
    let body: Value = resp.json().unwrap_or(Value::Null);
    if !status.is_success() {
        bail!(
            "{verb} secret {name}: HTTP {status} {}",
            body["message"].as_str().unwrap_or("")
        );
    }
    Ok(body)
}
