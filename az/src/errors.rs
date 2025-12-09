use actix_web::{HttpResponse, ResponseError};
use serde::Deserialize;
use thiserror::Error;
use tracing::error;

#[derive(Debug, Deserialize)]
pub struct AzureErrorBody {
    pub error: AzureInnerError,
}

#[derive(Debug, Deserialize)]
pub struct AzureInnerError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Error, Clone)]
pub enum AksError {
    // 5xx or 429 Errors (Retryable)
    #[error("Azure API request failed ({status}): {message} at {url}")]
    AzureHttp {
        status: u16,
        message: String,
        url: String,
    },

    // Connectivity Errors (Timeout, DNS)
    #[error("Azure client error: {message}")]
    AzureClient { message: String },

    #[error("Failed to parse response: {0}")]
    Parse(String),

    // 400 Errors (User Fault - Do Not Retry)
    #[error("Location parameter cannot be empty")]
    Validation,

    // Holds the full Azure message 'details' so we can pass it to the user.
    // This allows the user to see the list of valid locations provided by Azure.
    #[error("Invalid Azure location: '{location}'. Details: {details}")]
    InvalidLocation { location: String, details: String },

    #[error("HTTP client initialization failed: {0}")]
    ClientBuild(String),
}

impl ResponseError for AksError {
    fn error_response(&self) -> HttpResponse {
        let status = match self {
            // Return 400 Bad Request for validation or bad location
            // These are client errors, so the client should NOT retry them.
            AksError::Validation | AksError::InvalidLocation { .. } => {
                actix_web::http::StatusCode::BAD_REQUEST
            }
            AksError::AzureHttp { status, .. } => actix_web::http::StatusCode::from_u16(*status)
                .unwrap_or(actix_web::http::StatusCode::SERVICE_UNAVAILABLE),
            AksError::AzureClient { .. } => actix_web::http::StatusCode::SERVICE_UNAVAILABLE,
            _ => actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
        };

        // LOGGING STRATEGY:
        // - 5xx/Connectivity errors: Log as Error (needs attention).
        // - 4xx errors: Do NOT log as Error (avoids log spam from bad user input).
        if !status.is_client_error() {
            error!(error = %self, "Request failed");
        }

        // Pass specific details back to the client JSON so they know exactly why it failed.
        let response_body = match self {
            AksError::InvalidLocation { location, details } => serde_json::json!({
                "error": "Invalid Location",
                "location": location,
                "azure_message": details
            }),
            AksError::AzureHttp { message, .. } => serde_json::json!({
                "error": "Azure API Error",
                "message": message
            }),
            _ => serde_json::json!({
                "error": self.to_string()
            }),
        };

        HttpResponse::build(status).json(response_body)
    }
}
