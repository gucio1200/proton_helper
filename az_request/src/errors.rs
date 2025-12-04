use actix_web::{HttpResponse, ResponseError};
use serde::Deserialize;
use thiserror::Error;
use tracing::error;

#[derive(Debug, Deserialize)]
pub struct AzureErrorBody {
    pub error: serde_json::Value,
}

#[derive(Debug, Error, Clone)]
pub enum AksError {
    #[error("Azure API request failed ({status}): {message} at {url}")]
    AzureHttp {
        status: u16,
        message: String,
        url: String,
    },

    #[error("Azure client error: {message}")]
    AzureClient { message: String },

    #[error("Failed to parse response: {0}")]
    Parse(String),

    #[error("Location parameter cannot be empty")]
    Validation,

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("HTTP client initialization failed: {0}")]
    ClientBuild(String),
}

impl ResponseError for AksError {
    fn error_response(&self) -> HttpResponse {
        let status = match self {
            AksError::Validation => actix_web::http::StatusCode::BAD_REQUEST,
            AksError::AzureHttp { status, .. } => actix_web::http::StatusCode::from_u16(*status)
                .unwrap_or(actix_web::http::StatusCode::SERVICE_UNAVAILABLE),
            AksError::AzureClient { .. } => actix_web::http::StatusCode::SERVICE_UNAVAILABLE,
            _ => actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
        };

        error!(error = %self, "Request failed");

        HttpResponse::build(status).json(serde_json::json!({
            "error": self.to_string()
        }))
    }
}
