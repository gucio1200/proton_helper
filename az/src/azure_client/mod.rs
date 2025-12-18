use crate::errors::{AksError, AzureErrorBody};
use reqwest::Client;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::debug;

pub mod retry;
pub mod token;

pub const AKS_API_VERSION: &str = "2025-10-01";
const AZURE_MGMT_BASE: &str = "https://management.azure.com";
const K8S_GITHUB_URL: &str = "https://github.com/kubernetes/kubernetes";

// --- Output Structs (Renovate Pattern) ---

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RenovateResponse {
    pub releases: Vec<RenovateRelease>,
    pub source_url: String,
    pub changelog_url: String,
    pub homepage: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RenovateRelease {
    pub version: String,
    pub is_stable: bool,
    pub changelog_url: String,
    pub source_url: String,
}

// --- Internal structs ---
// These structs strictly mirror the Azure Resource Manager (ARM) JSON response.
#[derive(Deserialize)]
struct KubernetesVersionsResponse {
    values: Vec<MinorVersionItem>,
}

#[derive(Deserialize)]
struct MinorVersionItem {
    #[serde(rename = "version")]
    _family: String,
    #[serde(rename = "patchVersions", default)]
    patch_versions: HashMap<String, PatchDetail>,
}

#[derive(Deserialize)]
struct PatchDetail {
    #[serde(rename = "isPreview", default)]
    is_preview: bool,
}

// --- Helper Functions ---

fn generate_changelog_url(version: &str) -> String {
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() < 3 {
        return "".to_string();
    }
    
    let major = parts[0];
    let minor = parts[1];
    let patch = parts[2];

    format!(
        "{}/blob/master/CHANGELOG/CHANGELOG-{}.{}.md#v{}{}{}",
        K8S_GITHUB_URL, major, minor, major, minor, patch
    )
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
) -> Result<Arc<RenovateResponse>, AksError> {
    // 1. Construct the ARM Endpoint URL
    let url_str = format!(
        "{}/subscriptions/{}/providers/Microsoft.ContainerService/locations/{}/kubernetesVersions?api-version={}",
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

    let body_text = resp.text().await.map_err(|e| AksError::AzureClient {
        message: format!("Failed to read response body: {e}"),
    })?;

    // 4. DEBUG LOGGING
    debug!(
        status = status.as_u16(),
        url = %request_url,
        body = %body_text,
        "Azure API Response Received"
    );

    // 5. Handle HTTP Errors
    if !status.is_success() {
        // Try to parse the Azure JSON error format
        let maybe_json = serde_json::from_str::<AzureErrorBody>(&body_text);

        if let Ok(az_err) = maybe_json {
            let code = &az_err.error.code;
            let message = &az_err.error.message;

            // "NoRegisteredProviderFound" -> The location is invalid or not supported for AKS.
            if code == "NoRegisteredProviderFound" || code == "InvalidLocation" {
                return Err(AksError::InvalidLocation {
                    location: location.to_string(),
                    details: message.clone(), // Pass the full helpful message to the user
                });
            }
        }

        // Fallback: Raw text check
        if (status.as_u16() == 400 || status.as_u16() == 404)
            && body_text.contains("No registered resource provider")
        {
            return Err(AksError::InvalidLocation {
                location: location.to_string(),
                details: "Location not found or not supported for AKS (Raw check)".to_string(),
            });
        }

        // 5b. Generic Error Handling
        let msg = serde_json::from_str::<AzureErrorBody>(&body_text)
            .map(|e| e.error.message)
            .unwrap_or_else(|_| body_text.clone());

        return Err(AksError::AzureHttp {
            status: status.as_u16(),
            message: msg,
            url: request_url,
        });
    }

    // 6. Parse JSON Response
    // We parse from the `body_text` string we downloaded in step 3.
    let json: KubernetesVersionsResponse =
        serde_json::from_str(&body_text).map_err(|e| AksError::Parse(format!("JSON fail: {e}")))?;

    // 7. Filter, Transform, and Sort
    // We collect into a Vec of (Version, is_preview) tuples first to allow sorting
    let mut version_tuples: Vec<(Version, bool)> = Vec::new();

    for minor_ver in json.values {
        for (patch_str, details) in minor_ver.patch_versions {
            if !show_preview && details.is_preview {
                continue;
            }

            if let Ok(v) = Version::parse(&patch_str) {
                version_tuples.push((v, details.is_preview));
            }
        }
    }

    // Sort by version
    version_tuples.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    // 8. Map to Renovate format
    let releases: Vec<RenovateRelease> = version_tuples
        .into_iter()
        .map(|(v, is_preview)| {
            let version_string = v.to_string();
            RenovateRelease {
                version: version_string.clone(),
                is_stable: !is_preview,
                changelog_url: generate_changelog_url(&version_string),
                source_url: K8S_GITHUB_URL.to_string(),
            }
        })
        .collect();

    let response = RenovateResponse {
        releases,
        source_url: K8S_GITHUB_URL.to_string(),
        changelog_url: format!("{}/blob/master/CHANGELOG/README.md", K8S_GITHUB_URL),
        homepage: "https://kubernetes.io".to_string(),
    };

    Ok(Arc::new(response))
}
