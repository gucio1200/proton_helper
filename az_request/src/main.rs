use std::sync::Arc;
use azure_identity::{WorkloadIdentityCredential, WorkloadIdentityCredentialOptions};
use azure_core::credentials::TokenCredential;
use serde::Deserialize;
use reqwest;

// ----------------------
// Deserialize structs
// ----------------------
#[derive(Debug, Deserialize)]
struct UpgradeItem {
    #[serde(rename = "orchestratorVersion")]
    orchestrator_version: String,
    #[serde(rename = "orchestratorType", default)]
    orchestrator_type: String,
    #[serde(rename = "isPreview", default)]
    is_preview: bool,
}

#[derive(Debug, Deserialize)]
struct OrchestratorVersion {
    #[serde(rename = "orchestratorType")]
    orchestrator_type: String,
    #[serde(rename = "orchestratorVersion")]
    orchestrator_version: String,
    #[serde(default)]
    default: bool,
    #[serde(default)]
    upgrades: Vec<UpgradeItem>,
}

#[derive(Debug, Deserialize)]
struct OrchestratorsResponseProperties {
    orchestrators: Vec<OrchestratorVersion>,
}

#[derive(Debug, Deserialize)]
struct OrchestratorsResponse {
    properties: OrchestratorsResponseProperties,
}

// ----------------------
// Main async function
// ----------------------
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Get subscription ID from env
    let subscription_id = std::env::var("AZ_SUBSCRIPTION_ID")?;
    let location = "eastus";

    // ------------------------
    // API version as variable
    // ------------------------
    let api_version = "2020-11-01";

    let url = format!(
        "https://management.azure.com/subscriptions/{}/providers/Microsoft.ContainerService/locations/{}/orchestrators?api-version={}",
        subscription_id, location, api_version
    );

    // 2. WorkloadIdentityCredential
    let cred_options = WorkloadIdentityCredentialOptions::default();
    let credential = Arc::new(WorkloadIdentityCredential::new(Some(cred_options))?);

    // 3. Acquire token
    let scopes = &["https://management.azure.com/.default"];
    let token_response = credential.get_token(scopes, None).await?;
    let token = token_response.token.secret();

    // 4. REST API call
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?
        .json::<OrchestratorsResponse>()
        .await?;

    // 5. Print available versions, default flag, and upgrades
    println!("Available Kubernetes Versions in {}:", location);
    for o in resp.properties.orchestrators {
        let default_str = if o.default { " (default)" } else { "" };
        println!("- {} ({}){}", o.orchestrator_version, o.orchestrator_type, default_str);

        for u in &o.upgrades {
            let preview_str = if u.is_preview { " (preview)" } else { "" };
            println!("  upgrade → {}{} [{}]", u.orchestrator_version, preview_str, u.orchestrator_type);
        }
    }

    Ok(())
}
