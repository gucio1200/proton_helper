use crate::errors::{AksError, AzureErrorBody};
use reqwest::Client;
use semver::Version;
use serde::Deserialize;
use std::sync::Arc;

pub mod retry;
pub mod token;

pub const AKS_API_VERSION: &str = "2020-11-01";
const AZURE_MGMT_BASE: &str = "https://management.azure.com";

// --- Internal structs ---
#[derive(Deserialize)]
struct OrchestratorsResponse {
    properties: Properties,
}
#[derive(Deserialize)]
struct Properties {
    orchestrators: Vec<OrchestratorItem>,
}
#[derive(Deserialize)]
struct OrchestratorItem {
    #[serde(rename = "orchestratorType")]
    type_: String,
    #[serde(rename = "orchestratorVersion")]
    version: String,
    #[serde(rename = "isPreview", default)]
    is_preview: bool,
}

// --- Logic ---

pub async fn fetch_and_parse(
    client: &Client,
    subscription_id: &str,
    location: &str,
    token: &str,
    show_preview: bool,
) -> Result<Arc<[String]>, AksError> {
    let url_str = format!(
        "{}/subscriptions/{}/providers/Microsoft.ContainerService/locations/{}/orchestrators?api-version={}",
        AZURE_MGMT_BASE, subscription_id, location, AKS_API_VERSION
    );

    let resp = client
        .get(&url_str)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| AksError::AzureClient {
            message: e.to_string(),
        })?;

    let status = resp.status();

    if !status.is_success() {
        let request_url = resp.url().to_string();

        let body = resp.text().await.unwrap_or_default();

        // 1. Check for Invalid Location (400/404)
        if status.as_u16() == 400 || status.as_u16() == 404 {
            if body.contains("Location") || body.contains("location") {
                return Err(AksError::InvalidLocation(location.to_string()));
            }
        }

        // 2. Generic Error Handling
        let msg = serde_json::from_str::<AzureErrorBody>(&body)
            .map(|e| e.error.to_string())
            .unwrap_or(body);

        return Err(AksError::AzureHttp {
            status: status.as_u16(),
            message: msg,
            url: request_url,
        });
    }

    let json: OrchestratorsResponse = resp
        .json()
        .await
        .map_err(|e| AksError::Parse(format!("JSON fail: {e}")))?;

    let mut versions: Vec<Version> = json
        .properties
        .orchestrators
        .into_iter()
        .filter(|o| o.type_ == "Kubernetes" && (show_preview || !o.is_preview))
        .map(|o| Version::parse(&o.version))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AksError::Parse(format!("SemVer fail: {e}")))?;

    versions.sort_unstable();

    let final_strings: Vec<String> = versions.iter().map(|v| v.to_string()).collect();
    Ok(final_strings.into())
}
