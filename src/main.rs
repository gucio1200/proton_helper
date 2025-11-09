use anyhow::{anyhow, Result};
use chrono::Local;
use natpmp::{Error, Natpmp, Protocol, Response};
use reqwest::Client;
use std::{env, net::Ipv4Addr, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tokio::time::interval;
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::Retry;

#[tokio::main]
async fn main() -> Result<()> {
    // Environment variables
    let gateway: Ipv4Addr = env::var("NATPMP_GATEWAY")
        .unwrap_or("10.2.0.1".to_string())
        .parse()?;
    let internal_port: u16 = env::var("INTERNAL_PORT")
        .unwrap_or("0".to_string())
        .parse()?;
    let public_port: u16 = env::var("PUBLIC_PORT").unwrap_or("1".to_string()).parse()?;
    let lifetime: u32 = env::var("MAPPING_LIFETIME")
        .unwrap_or("60".to_string())
        .parse()?;
    let refresh_interval: u64 = env::var("REFRESH_INTERVAL")
        .unwrap_or("30".to_string())
        .parse()?;
    let qbittorrent_host = env::var("QBITTORRENT_HOST").unwrap_or("http://127.0.0.1".to_string());
    let qbittorrent_port: u16 = env::var("QBITTORRENT_PORT")
        .unwrap_or("8080".to_string())
        .parse()?;

    let client = Arc::new(Mutex::new(Natpmp::new_with(gateway)?));
    let mut ticker = interval(Duration::from_secs(refresh_interval));
    let mut last_tcp_port: Option<u16> = None;

    println!(
        "[{}] Starting NAT-PMP refresher for gateway {}",
        Local::now().format("%H:%M:%S"),
        gateway
    );

    loop {
        ticker.tick().await;

        let client_clone = client.clone();
        let mapping_strategy = ExponentialBackoff::from_millis(50).map(jitter).take(5);

        // TCP mapping
        let tcp_port = Retry::spawn(mapping_strategy.clone(), move || {
            let client_clone = client_clone.clone();
            async move {
                let mut c = client_clone.lock().await;
                refresh_nat_mapping(&mut *c, Protocol::TCP, internal_port, public_port, lifetime)
                    .await
            }
        })
        .await?;

        // UDP mapping
        let client_clone = client.clone();
        let udp_port = Retry::spawn(mapping_strategy, move || {
            let client_clone = client_clone.clone();
            async move {
                let mut c = client_clone.lock().await;
                refresh_nat_mapping(&mut *c, Protocol::UDP, internal_port, public_port, lifetime)
                    .await
            }
        })
        .await?;

        println!(
            "[{}] Public TCP port: {}, UDP port: {}",
            Local::now().format("%H:%M:%S"),
            tcp_port,
            udp_port
        );

        // Update qBittorrent only if TCP port changed
        if last_tcp_port != Some(tcp_port) {
            set_qbittorrent_listen_port(&qbittorrent_host, qbittorrent_port, tcp_port).await?;
            last_tcp_port = Some(tcp_port);
        }
    }
}

/// Refresh NAT-PMP mapping and return public port
async fn refresh_nat_mapping(
    client: &mut Natpmp,
    protocol: Protocol,
    internal_port: u16,
    public_port: u16,
    lifetime: u32,
) -> Result<u16> {
    client
        .send_port_mapping_request(protocol, internal_port, public_port, lifetime)
        .map_err(|e| anyhow!("Failed to send NAT-PMP request: {:?}", e))?;

    loop {
        match client.read_response_or_retry() {
            Ok(Response::TCP(resp)) if protocol == Protocol::TCP => return Ok(resp.public_port()),
            Ok(Response::UDP(resp)) if protocol == Protocol::UDP => return Ok(resp.public_port()),
            Ok(_) => return Err(anyhow!("Unexpected NAT-PMP response type")),
            Err(e) if e == Error::NATPMP_TRYAGAIN => {
                tokio::time::sleep(Duration::from_millis(50)).await
            }
            Err(e) => return Err(anyhow!("NAT-PMP error: {:?}", e)),
        }
    }
}

/// Update qBittorrent listen port (no login required)
async fn set_qbittorrent_listen_port(host: &str, port: u16, new_port: u16) -> Result<()> {
    let client = Client::new();
    let url = format!("{}:{}/api/v2/app/setPreferences", host, port);

    // Working method: send 'json={"listen_port":...}' as form
    let payload = format!(r#"{{"listen_port":{}}}"#, new_port);

    let resp = client.post(&url).form(&[("json", payload)]).send().await?;

    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("qBittorrent failed to set listen_port: {}", text);
    }

    println!(
        "[{}] Updated qBittorrent listen_port to {}",
        chrono::Local::now().format("%H:%M:%S"),
        new_port
    );

    Ok(())
}
