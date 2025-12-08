use actix_request_identifier::RequestIdentifier;
use actix_web::{middleware::Logger, web, App, HttpServer};
use anyhow::Result;
use clap::Parser;
use tracing::info;

mod azure_client;
mod config;
mod errors;
mod handlers;
mod state;
mod worker;

use azure_client::token::refresh_and_cache_token;
use config::Config;
use handlers::{aks_versions, status};
use state::AppState;

#[actix_web::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();

    let config = Config::parse();
    info!(port = config.port, "Starting AKS service");

    let state = AppState::new(config.clone())?;
    let app_data = web::Data::new(state);

    // Initial Token
    info!("Acquiring initial token...");
    refresh_and_cache_token(app_data.credential.as_ref(), &app_data.token_cache)
        .await
        .map_err(|e| anyhow::anyhow!("Initial token fail: {}", e))?;

    worker::start(app_data.clone());

    HttpServer::new(move || {
        App::new()
            .app_data(app_data.clone())
            .wrap(RequestIdentifier::with_uuid())
            .wrap(Logger::default())
            .service(aks_versions)
            .service(status)
    })
    .bind(("0.0.0.0", config.port))?
    .run()
    .await?;

    Ok(())
}
