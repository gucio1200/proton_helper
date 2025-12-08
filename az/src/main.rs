use actix_web::{middleware::Logger, web, App, HttpServer};
use anyhow::Result;
use tracing::info;
mod azure_client;
mod config;
mod errors;
mod handlers;
mod state;
use actix_request_identifier::RequestIdentifier;
use azure_client::token::refresh_and_cache_token;
use clap::Parser;
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

    info!(
        port = config.port,
        cache_ttl = config.cache_ttl_seconds,
        show_preview = config.show_preview,
        "Starting AKS versions service"
    );

    let state = AppState::new(config.clone())?;
    let app_data = web::Data::new(state);

    // Synchronously acquire initial token before binding the server
    info!("Acquiring initial Azure access token...");
    if let Err(e) =
        refresh_and_cache_token(app_data.credential.as_ref(), &app_data.token_cache).await
    {
        return Err(anyhow::anyhow!("Failed to acquire initial token: {}", e));
    }
    info!("Initial token acquired successfully.");

    // Start the background refresh task
    state::start_token_refresher(app_data.clone());

    let bind_addr = ("0.0.0.0", config.port);

    info!("Binding to {}:{}", bind_addr.0, bind_addr.1);

    HttpServer::new(move || {
        App::new()
            .app_data(app_data.clone())
            .wrap(RequestIdentifier::with_uuid())
            .wrap(Logger::default())
            .service(aks_versions)
            .service(status)
    })
    .bind(bind_addr)?
    .run()
    .await?;

    Ok(())
}
