use crate::errors::{AksError, AzureErrorBody};
use reqwest::{Client, Response};
use tracing::instrument;

// Azure API Constants
pub const AKS_API_VERSION: &str = "2020-11-01";
pub const AZURE_MGMT_BASE: &str = "https://management.azure.com";

#[inline]
fn build_orchestrators_url(subscription_id: &str, location: &str) -> String {
    format!(
        "{}/subscriptions/{}/providers/Microsoft.ContainerService/locations/{}/orchestrators?api-version={}",
        AZURE_MGMT_BASE, subscription_id, location, AKS_API_VERSION
    )
}

#[instrument(skip(resp))]
pub async fn handle_azure_response(resp: Response) -> Result<Response, AksError> {
    let status = resp.status();

    if !status.is_success() {
        let url = resp.url().to_string();
        let status_code = status.as_u16();

        let body = resp.text().await.unwrap_or_else(|_| "No body".to_string());

        if let Ok(azure_err) = serde_json::from_str::<AzureErrorBody>(&body) {
            Err(AksError::AzureHttp {
                status: status_code,
                message: azure_err.error.to_string(),
                url,
            })
        } else {
            Err(AksError::AzureHttp {
                status: status_code,
                message: format!("HTTP response error body unparsable. Body: {}", body),
                url,
            })
        }
    } else {
        Ok(resp)
    }
}

#[instrument(skip(client, token), fields(location = %location))]
pub async fn fetch_aks_versions(
    client: &Client,
    subscription_id: &str,
    location: &str,
    token: &str,
) -> Result<Response, AksError> {
    let url = build_orchestrators_url(subscription_id, location);

    let resp = client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| AksError::AzureClient {
            message: format!("Request failed: {e}"),
        })?;

    handle_azure_response(resp).await
}

// Re-export nested modules
pub mod retry;
pub mod token;
