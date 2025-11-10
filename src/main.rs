use std::{env, net::Ipv4Addr, sync::Arc, time::Duration};
use natpmp::{Natpmp, Protocol, Response, Error};
use tokio::sync::Mutex;
use tokio::time::interval;
use tokio_retry::strategy::{ExponentialBackoff, jitter};
use tokio_retry::Retry;
use reqwest::Client;
use anyhow::{Result, anyhow};
use chrono::Local;
use serde_json::Value;

#[tokio::main]
async fn main() -> Result<()> {
    // Read environment variables
    let gateway: Ipv4Addr = env::var("NATPMP_GATEWAY").unwrap_or("10.2.0.1".to_string()).parse()?;
    let internal_port: u16 = env::var("INTERNAL_PORT").unwrap_or("0".to_string()).parse()?; // internal port 0
    let public_port: u16 = env::var("PUBLIC_PORT").unwrap_or("1".to_string()).parse()?;      // public port 1
    let lifetime: u32 = env::var("MAPPING_LIFETIME").unwrap_or("60".to_string()).parse()?;
    let refresh_interval: u64 = env::var("REFRESH_INTERVAL").unwrap_or("30".to_string()).parse()?;
    let qbittorrent_host = env::var("QBITTORRENT_HOST").unwrap_or("http://127.0.0.1".to_string());
    let qbittorrent_port: u16 = env::var("QBITTORRENT_PORT").unwrap_or("8080".to_string()).parse()?;

    let client = Arc::new(Mutex::new(Natpmp::new_with(gateway)?));
    let mut ticker = interval(Duration::from_secs(refresh_interval));

    println!(
        "[{}] Starting NAT-PMP refresher for gateway {}",
        Local::now().format("%H:%M:%S"),
        gateway
    );

    // Ctrl+C future
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("failed to install Ctrl+C handler");
        println!("Received Ctrl+C, shutting down...");
    };

    // Unix SIGTERM future (declare term_signal as mutable)
    #[cfg(unix)]
    let mut term_signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    #[cfg(unix)]
    let shutdown_signal = async {
        tokio::select! {
            _ = ctrl_c => {},
            _ = term_signal.recv() => {
                println!("Received SIGTERM, shutting down...");
            }
        }
    };

    #[cfg(not(unix))]
    let shutdown_signal = ctrl_c;

    // Main loop with shutdown support
    tokio::select! {
        _ = async {
            loop {
                ticker.tick().await;

                // Wait for qBittorrent availability
                wait_for_qbittorrent(&qbittorrent_host, qbittorrent_port).await?;

                let client_clone = client.clone();
                let mapping_strategy = ExponentialBackoff::from_millis(50).map(jitter).take(5);

                // TCP NAT-PMP mapping
                let tcp_port = Retry::spawn(mapping_strategy.clone(), move || {
                    let client_clone = client_clone.clone();
                    async move {
                        let mut c = client_clone.lock().await;
                        refresh_nat_mapping(&mut *c, Protocol::TCP, internal_port, public_port, lifetime).await
                    }
                }).await?;

                // UDP NAT-PMP mapping
                let client_clone = client.clone();
                let udp_port = Retry::spawn(mapping_strategy, move || {
                    let client_clone = client_clone.clone();
                    async move {
                        let mut c = client_clone.lock().await;
                        refresh_nat_mapping(&mut *c, Protocol::UDP, internal_port, public_port, lifetime).await
                    }
                }).await?;

                println!(
                    "[{}] Public TCP port: {}, UDP port: {}",
                    Local::now().format("%H:%M:%S"),
                    tcp_port,
                    udp_port
                );

                // Check qBittorrent current listen port
                let current_qb_port = get_qbittorrent_listen_port(&qbittorrent_host, qbittorrent_port).await?;

                if current_qb_port != tcp_port {
                    set_qbittorrent_listen_port(&qbittorrent_host, qbittorrent_port, tcp_port).await?;
                    println!(
                        "[{}] qBittorrent listen_port updated from {} to {}",
                        Local::now().format("%H:%M:%S"),
                        current_qb_port,
                        tcp_port
                    );
                } else {
                    println!(
                        "[{}] qBittorrent listen_port {} is up-to-date",
                        Local::now().format("%H:%M:%S"),
                        current_qb_port
                    );
                }
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        } => {},
        _ = shutdown_signal => {
            println!("Graceful shutdown complete.");
        }
    }

    Ok(())
}

/// Refresh NAT-PMP mapping and return public port
async fn refresh_nat_mapping(
    client: &mut Natpmp,
    protocol: Protocol,
    internal_port: u16,
    public_port: u16,
    lifetime: u32,
) -> Result<u16> {
    client.send_port_mapping_request(protocol, internal_port, public_port, lifetime)
        .map_err(|e| anyhow!("Failed to send NAT-PMP request: {:?}", e))?;

    loop {
        match client.read_response_or_retry() {
            Ok(Response::TCP(resp)) if protocol == Protocol::TCP => return Ok(resp.public_port()),
            Ok(Response::UDP(resp)) if protocol == Protocol::UDP => return Ok(resp.public_port()),
            Ok(_) => return Err(anyhow!("Unexpected NAT-PMP response type")),
            Err(e) if e == Error::NATPMP_TRYAGAIN => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(e) => return Err(anyhow!("NAT-PMP error: {:?}", e)),
        }
    }
}

/// Update qBittorrent listen port
async fn set_qbittorrent_listen_port(host: &str, port: u16, new_port: u16) -> Result<()> {
    let client = Client::new();
    let url = format!("{}:{}/api/v2/app/setPreferences", host, port);
    let payload = format!(r#"{{"listen_port":{}}}"#, new_port);

    let resp = client.post(&url)
        .form(&[("json", payload)])
        .send()
        .await?;

    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("qBittorrent failed to set listen_port: {}", text);
    }

    Ok(())
}

/// Fetch current qBittorrent listen_port
async fn get_qbittorrent_listen_port(host: &str, port: u16) -> Result<u16> {
    let client = Client::new();
    let url = format!("{}:{}/api/v2/app/preferences", host, port);

    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("Failed to get qBittorrent preferences: HTTP {}", resp.status());
    }

    let json: Value = resp.json().await?;
    if let Some(lp) = json.get("listen_port").and_then(|v| v.as_u64()) {
        Ok(lp as u16)
    } else {
        anyhow::bail!("listen_port field missing in qBittorrent preferences");
    }
}

/// Wait until qBittorrent WebUI is available
async fn wait_for_qbittorrent(host: &str, port: u16) -> Result<()> {
    let client = Client::new();
    let url = format!("{}:{}/api/v2/app/version", host, port);

    loop {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => break,
            Ok(resp) => {
                println!("qBittorrent returned HTTP {}. Retrying...", resp.status());
            }
            Err(_) => {
                println!("qBittorrent not reachable. Retrying...");
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    Ok(())
}
