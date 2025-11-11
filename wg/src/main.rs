use std::env;
use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::thread;
use std::time::Duration;

use rand::seq::SliceRandom;
use ssh2::Session;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono::Local;
use ctrlc;
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;
use sha2::{Digest, Sha512};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv::dotenv().ok();

    // --- Environment setup ---
    let auth_server = env::var("AUTH_SERVER")?;
    let auth_token = env::var("AUTH_TOKEN")?;
    let session_id = env::var("SESSION_ID")?;

    let mikrotik_host = env::var("MIKROTIK_HOST")?;
    let mikrotik_user = env::var("MIKROTIK_USER")?;
    let mikrotik_pass = env::var("MIKROTIK_PASS")?;

    let countries_str = env::var("COUNTRIES").unwrap_or_else(|_| "RO".to_string());
    let countries: Vec<&str> = countries_str.split(',').collect();

    let tier: u32 = env::var("TIER")
        .unwrap_or_else(|_| "2".to_string())
        .parse()?;
    let features_str = env::var("FEATURES").unwrap_or_else(|_| "P2P".to_string());
    let features: Vec<&str> = features_str.split(',').collect();

    let mut headers = HeaderMap::new();
    headers.insert(
        "x-pm-appversion",
        HeaderValue::from_static("web-account@5.0.310.0"),
    );
    headers.insert("x-pm-uid", HeaderValue::from_str(&auth_server)?);
    headers.insert(
        "Accept",
        HeaderValue::from_static("application/vnd.protonmail.v1+json"),
    );
    headers.insert(
        "Cookie",
        HeaderValue::from_str(&format!(
            "AUTH-{}={}; Session-Id={}",
            auth_server, auth_token, session_id
        ))?,
    );

    let client = Client::builder().default_headers(headers.clone()).build()?;

    // --- Ctrl+C handler ---
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        println!("\n[INFO] Ctrl+C pressed, exiting...");
        r.store(false, std::sync::atomic::Ordering::SeqCst);
    })?;

    // --- Monitoring loop ---
    while running.load(std::sync::atomic::Ordering::SeqCst) {
        let mut fail_count = 0;
        while fail_count < 5 && running.load(std::sync::atomic::Ordering::SeqCst) {
            if host_reachable_port("10.2.0.1", 51820) {
                fail_count = 0;
            } else {
                fail_count += 1;
                println!(
                    "[WARN] Host 10.2.0.1 unreachable on port 51820 (fail {}/5)",
                    fail_count
                );
            }
            thread::sleep(Duration::from_secs(2));
        }

        if !running.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }

        if fail_count >= 5 {
            println!("[INFO] 5 consecutive failures detected → regenerating VPN config");
            if let Err(e) = regenerate_vpn_flow(
                &client,
                &countries,
                tier,
                &features,
                &mikrotik_host,
                &mikrotik_user,
                &mikrotik_pass,
            ) {
                println!("[ERROR] VPN regeneration failed: {}", e);
            }
        }
    }

    println!("[INFO] Program terminated gracefully.");
    Ok(())
}

/// --- Helper: port check with 1s timeout ---
fn host_reachable_port(host: &str, port: u16) -> bool {
    let addr_str = format!("{}:{}", host, port);
    match addr_str.to_socket_addrs() {
        Ok(mut addrs) => {
            if let Some(addr) = addrs.next() {
                let timeout = Duration::from_secs(2);
                match TcpStream::connect_timeout(&addr, timeout) {
                    Ok(_) => {
                        println!("[DEBUG] {}:{} reachable", host, port);
                        true
                    }
                    Err(e) => {
                        println!("[DEBUG] {}:{} unreachable ({})", host, port, e);
                        false
                    }
                }
            } else {
                println!("[ERROR] Could not resolve {}", host);
                false
            }
        }
        Err(e) => {
            println!("[ERROR] Address error {}: {}", addr_str, e);
            false
        }
    }
}

/// --- VPN regeneration flow ---
fn regenerate_vpn_flow(
    client: &Client,
    countries: &[&str],
    tier: u32,
    features: &[&str],
    mikrotik_host: &str,
    mikrotik_user: &str,
    mikrotik_pass: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Fetch ProtonVPN servers
    let resp: Value = client
        .get("https://account.protonvpn.com/api/vpn/logicals")
        .send()?
        .json()?;
    let mut servers: Vec<Value> = resp["LogicalServers"]
        .as_array()
        .ok_or("Expected LogicalServers array")?
        .to_owned();

    // Filter by country/tier/features/Status=1
    servers.retain(|s| {
        let entry_country = s["EntryCountry"].as_str().unwrap_or("");
        let exit_country = s["ExitCountry"].as_str().unwrap_or("");
        let server_tier = s["Tier"].as_u64().unwrap_or(0) as u32;
        let status = s["Status"].as_u64().unwrap_or(0);

        let feat_flags = [
            if s["Features"].as_u64().unwrap_or(0) & 1 == 1 {
                "SecureCore"
            } else {
                "-SecureCore"
            },
            if s["Features"].as_u64().unwrap_or(0) & 2 == 2 {
                "TOR"
            } else {
                "-TOR"
            },
            if s["Features"].as_u64().unwrap_or(0) & 4 == 4 {
                "P2P"
            } else {
                "-P2P"
            },
            if s["Features"].as_u64().unwrap_or(0) & 8 == 8 {
                "XOR"
            } else {
                "-XOR"
            },
            if s["Features"].as_u64().unwrap_or(0) & 16 == 16 {
                "IPv6"
            } else {
                "-IPv6"
            },
        ];

        status == 1
            && (!countries.contains(&entry_country) && !countries.contains(&exit_country)) == false
            && server_tier == tier
            && !features.iter().any(|f| !feat_flags.contains(f))
    });

    if servers.is_empty() {
        println!("[WARN] No servers found matching the given criteria.");
        return Ok(());
    }

    // Shuffle and select one
    let mut rng = rand::rng();
    servers.shuffle(&mut rng);
    let server = &servers[0];

    println!(
        "[INFO] Selected server: {}",
        server["Name"].as_str().unwrap_or("")
    );

    let keys = get_keys_from_protonvpn(client)?;
    let reg = register_config(client, server, &keys)?;
    let x25519_priv = get_x25519_priv(&keys[2]);

    let endpoint_ip = reg["Features"]["peerIp"].as_str().unwrap_or("");
    println!("[INFO] New endpoint: {}:51820", endpoint_ip);

    // Update MikroTik
    update_mikrotik_wg(
        mikrotik_host,
        mikrotik_user,
        mikrotik_pass,
        &x25519_priv,
        reg["Features"]["peerPublicKey"].as_str().unwrap_or(""),
        endpoint_ip,
    )?;

    Ok(())
}

// ------------------- ProtonVPN + SSH helpers -------------------

fn get_keys_from_protonvpn(client: &Client) -> Result<[String; 3], Box<dyn std::error::Error>> {
    let resp: Value = client
        .get("https://account.protonvpn.com/api/vpn/v1/certificate/key/EC")
        .send()?
        .json()?;
    let priv_key_full = resp["PrivateKey"].as_str().unwrap_or("").to_string();
    let pub_key_full = resp["PublicKey"].as_str().unwrap_or("").to_string();
    let priv_key_stripped = priv_key_full.lines().nth(1).unwrap_or("").to_string();
    let pub_key_stripped = pub_key_full.lines().nth(1).unwrap_or("").to_string();
    Ok([priv_key_full, pub_key_stripped, priv_key_stripped])
}

fn get_x25519_priv(priv_key: &str) -> String {
    let decoded = STANDARD.decode(priv_key).unwrap();
    let hash = Sha512::digest(&decoded[decoded.len() - 32..]);
    let mut h = hash[..32].to_vec();
    h[0] &= 0xf8;
    h[31] &= 0x7f;
    h[31] |= 0x40;
    STANDARD.encode(&h)
}

fn register_config(
    client: &Client,
    server: &Value,
    priv_keys: &[String; 3],
) -> Result<Value, Box<dyn std::error::Error>> {
    let device_name = format!(
        "{}-{}",
        Local::now().format("%d/%m"),
        server["Name"].as_str().unwrap_or("")
    );
    let body = serde_json::json!({
        "ClientPublicKey": priv_keys[1],
        "Mode": "persistent",
        "DeviceName": device_name,
        "Features": {
            "peerName": server["Name"].as_str().unwrap_or(""),
            "peerIp": server["Servers"][0]["EntryIP"].as_str().unwrap_or(""),
            "peerPublicKey": server["Servers"][0]["X25519PublicKey"].as_str().unwrap_or(""),
            "platform": "Windows",
            "SplitTCP": true,
            "PortForwarding": true,
            "RandomNAT": true,
            "NetShieldLevel": 0
        }
    });

    let resp: Value = client
        .post("https://account.protonvpn.com/api/vpn/v1/certificate")
        .json(&body)
        .send()?
        .json()?;
    Ok(resp)
}

fn update_mikrotik_wg(
    host: &str,
    user: &str,
    pass: &str,
    wg_private: &str,
    peer_public: &str,
    endpoint_ip: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("[SSH] Connecting to MikroTik at {}", host);
    let tcp = TcpStream::connect(format!("{}:22", host))?;
    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);
    sess.handshake()?;
    sess.userauth_password(user, pass)?;
    if !sess.authenticated() {
        return Err("SSH authentication failed".into());
    }

    let commands = vec![
        format!(
            "/interface/wireguard/set wg1 private-key=\"{}\"",
            wg_private
        ),
        format!(
            "/interface/wireguard/peers/set numbers=0 public-key=\"{}\"",
            peer_public
        ),
        format!(
            "/interface/wireguard/peers/set numbers=0 endpoint-address=\"{}\"",
            endpoint_ip
        ),
    ];

    for cmd in commands {
        let mut channel = sess.channel_session()?;
        println!("[SSH] {}", cmd);
        channel.exec(&cmd)?;
        let mut s = String::new();
        channel.read_to_string(&mut s)?;
        println!("[SSH Output]:\n{}", s);
        channel.wait_close()?;
    }

    Ok(())
}
