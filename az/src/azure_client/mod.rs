use crate::errors::{AksError, AzureErrorBody};
use reqwest::Client;
use semver::Version;
use serde::Deserialize;
use std::sync::Arc;
use tracing::debug;

pub mod retry;
pub mod token;

pub const AKS_API_VERSION: &str = "2020-11-01";
const AZURE_MGMT_BASE: &str = "https://management.azure.com";

// --- Internal structs ---
// These structs strictly mirror the Azure Resource Manager (ARM) JSON response.
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

/// Fetches the list of available Kubernetes versions from Azure, parses the JSON,
/// filters by preview status, and sorts them semantically.
pub async fn fetch_and_parse(
    client: &Client,
    subscription_id: &str,
    location: &str,
    token: &str,
    show_preview: bool,
) -> Result<Arc<[String]>, AksError> {
    // 1. Construct the ARM Endpoint URL
    let url_str = format!(
        "{}/subscriptions/{}/providers/Microsoft.ContainerService/locations/{}/orchestrators?api-version={}",
        AZURE_MGMT_BASE, subscription_id, location, AKS_API_VERSION
    );

    // 2. Execute HTTP Request
    let resp = client
        .get(&url_str)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| AksError::AzureClient {
            message: e.to_string(),
        })?;

    // 3. Capture Metadata & Body
    // We must read the body into a String immediately so we can both LOG it and PARSE it.
    let status = resp.status();
    let request_url = resp.url().to_string();

    let body_text = resp
        .text()
        .await
        .map_err(|e| AksError::AzureClient {
            message: format!("Failed to read response body: {e}"),
        })?;

    // Run with `RUST_LOG=debug cargo run` to see the full JSON response in your terminal.
    debug!(
        status = status.as_u16(),
        url = %request_url,
        body = %body_text,
        "Azure API Response Received"
    );

    // 5. Handle HTTP Errors
    if !status.is_success() {
        // 5a. Check for Invalid Location (400/404)
        if status.as_u16() == 400 || status.as_u16() == 404 {
            if body_text.contains("Location") || body_text.contains("location") {
                return Err(AksError::InvalidLocation(location.to_string()));
            }
        }

        // 5b. Generic Error Handling
        let msg = serde_json::from_str::<AzureErrorBody>(&body_text)
            .map(|e| e.error.to_string())
            .unwrap_or_else(|_| body_text.clone());

        return Err(AksError::AzureHttp {
            status: status.as_u16(),
            message: msg,
            url: request_url,
        });
    }

    // 6. Parse JSON Response
    // We parse from the `body_text` string we downloaded in step 3.
    let json: OrchestratorsResponse = serde_json::from_str(&body_text)
        .map_err(|e| AksError::Parse(format!("JSON fail: {e}")))?;

    // 7. Filter, Transform, and Sort
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
