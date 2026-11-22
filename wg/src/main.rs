use std::env;
use std::io::Read;
use std::net::TcpStream;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rand::seq::SliceRandom;
use ssh2::Session;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono::Local;
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;
use sha2::{Digest, Sha512};

const ACCOUNT_URL: &str = "https://account.protonvpn.com";

struct CheckConfig {
    url: String,
    expect: String,
    interval: Duration,
    failures: u32,
    cooldown: Duration,
}

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

    let check = CheckConfig {
        url: env::var("CHECK_URL").unwrap_or_else(|_| "https://1.1.1.1/cdn-cgi/trace".to_string()),
        expect: env::var("CHECK_EXPECT").unwrap_or_else(|_| "ip=".to_string()),
        interval: Duration::from_secs(env_or::<u64>("CHECK_INTERVAL_SECS", 30)?),
        failures: env_or::<u32>("CHECK_FAILURES", 3)?.max(1),
        cooldown: Duration::from_secs(env_or::<u64>("REGEN_COOLDOWN_SECS", 600)?),
    };
    let regenerate_on_start = env_flag("REGENERATE_ON_START", false)?;
    let configured_app_version = env::var("PROTON_APP_VERSION")
        .ok()
        .and_then(|v| normalize_app_version(&v));

    let check_client = Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .no_proxy()
        .build()?;

    let web_client = Client::builder().timeout(Duration::from_secs(30)).build()?;

    // --- Ctrl+C handler ---
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        println!("\n[INFO] Termination signal received, exiting...");
        r.store(false, Ordering::SeqCst);
    })?;

    let regenerate_with = |app_version: &str| -> Result<bool, Box<dyn std::error::Error>> {
        println!("[INFO] Proton API client version {}", app_version);
        let client = proton_client(&auth_server, &auth_token, &session_id, app_version)?;
        regenerate_vpn_flow(
            &client,
            &countries,
            tier,
            &features,
            &mikrotik_host,
            &mikrotik_user,
            &mikrotik_pass,
        )
    };

    let regenerate = || -> Result<bool, Box<dyn std::error::Error>> {
        let configured = match &configured_app_version {
            Some(version) => version,
            None => {
                let detected = detect_app_version(&web_client).map_err(|e| {
                    format!("PROTON_APP_VERSION not set and detection failed: {}", e)
                })?;
                return regenerate_with(&detected);
            }
        };
        match regenerate_with(configured) {
            Err(e) if e.to_string().contains("Code 5003") => {
                let detected = detect_app_version(&web_client)
                    .map_err(|d| format!("{}; version detection failed: {}", e, d))?;
                println!(
                    "[WARN] PROTON_APP_VERSION {} rejected as outdated, retrying with {} (update it in 1Password)",
                    configured, detected
                );
                regenerate_with(&detected)
            }
            result => result,
        }
    };

    if regenerate_on_start {
        println!("[INFO] REGENERATE_ON_START enabled → generating a fresh VPN config");
        match regenerate() {
            Ok(true) => sleep_while_running(&running, Duration::from_secs(15)),
            Ok(false) => {}
            Err(e) => println!("[ERROR] VPN regeneration failed: {}", e),
        }
    }

    println!(
        "[INFO] Tunnel check: GET {} every {}s, regenerate after {} failures, cooldown {}s",
        check.url,
        check.interval.as_secs(),
        check.failures,
        check.cooldown.as_secs()
    );

    // --- Monitoring loop ---
    let mut failures = 0u32;
    let mut healthy: Option<bool> = None;
    while running.load(Ordering::SeqCst) {
        match tunnel_check(&check_client, &check.url, &check.expect) {
            Ok(exit_ip) => {
                if healthy != Some(true) {
                    println!("[INFO] Tunnel OK, exit IP {}", exit_ip);
                }
                healthy = Some(true);
                failures = 0;
            }
            Err(e) => {
                failures += 1;
                healthy = Some(false);
                println!(
                    "[WARN] Tunnel check failed ({}/{}): {}",
                    failures, check.failures, e
                );
            }
        }

        if failures < check.failures {
            sleep_while_running(&running, check.interval);
            continue;
        }

        println!(
            "[INFO] {} consecutive failures detected → regenerating VPN config",
            failures
        );
        if let Err(e) = regenerate() {
            println!("[ERROR] VPN regeneration failed: {}", e);
        }
        failures = 0;
        sleep_while_running(&running, check.cooldown);
    }

    println!("[INFO] Program terminated gracefully.");
    Ok(())
}

fn env_or<T>(name: &str, default: T) -> Result<T, Box<dyn std::error::Error>>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    match env::var(name) {
        Ok(value) => value
            .trim()
            .parse::<T>()
            .map_err(|e| format!("invalid {}={}: {}", name, value, e).into()),
        Err(_) => Ok(default),
    }
}

fn env_flag(name: &str, default: bool) -> Result<bool, Box<dyn std::error::Error>> {
    let value = match env::var(name) {
        Ok(value) => value,
        Err(_) => return Ok(default),
    };
    match parse_flag(&value) {
        Some(flag) => Ok(flag),
        None => Err(format!("invalid {}={}: expected true or false", name, value).into()),
    }
}

fn parse_flag(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" | "" => Some(false),
        _ => None,
    }
}

fn sleep_while_running(running: &AtomicBool, total: Duration) {
    let deadline = Instant::now() + total;
    while running.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep((deadline - now).min(Duration::from_secs(1)));
    }
}

fn tunnel_check(client: &Client, url: &str, expect: &str) -> Result<String, String> {
    let resp = client.get(url).send().map_err(describe_request_error)?;
    let status = resp.status();
    let body = resp.text().map_err(describe_request_error)?;
    if !status.is_success() {
        return Err(format!("HTTP {}", status));
    }
    if !body.contains(expect) {
        return Err(format!("response does not contain {:?}", expect));
    }
    Ok(exit_ip(&body).unwrap_or("unknown").to_string())
}

fn describe_request_error(e: reqwest::Error) -> String {
    if e.is_timeout() {
        "timeout".to_string()
    } else if e.is_connect() {
        format!("connect error: {}", e)
    } else {
        e.to_string()
    }
}

fn exit_ip(body: &str) -> Option<&str> {
    body.lines()
        .find_map(|line| line.strip_prefix("ip="))
        .map(str::trim)
}

fn proton_client(
    uid: &str,
    token: &str,
    session_id: &str,
    app_version: &str,
) -> Result<Client, Box<dyn std::error::Error>> {
    let mut headers = HeaderMap::new();
    headers.insert("x-pm-appversion", HeaderValue::from_str(app_version)?);
    headers.insert("x-pm-uid", HeaderValue::from_str(uid)?);
    headers.insert(
        "Accept",
        HeaderValue::from_static("application/vnd.protonmail.v1+json"),
    );
    headers.insert(
        "Cookie",
        HeaderValue::from_str(&format!(
            "AUTH-{}={}; Session-Id={}",
            uid, token, session_id
        ))?,
    );
    Ok(Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(30))
        .build()?)
}

fn detect_app_version(client: &Client) -> Result<String, Box<dyn std::error::Error>> {
    let html = client.get(format!("{}/", ACCOUNT_URL)).send()?.text()?;
    let script = between(&html, "src=\"/assets/static/index.", "\"")
        .ok_or("index script not found on the account page")?;
    let js = client
        .get(format!("{}/assets/static/index.{}", ACCOUNT_URL, script))
        .send()?
        .text()?;
    let version = between(&js, "APP_VERSION:\"", "\"").ok_or("APP_VERSION not found")?;
    if version.is_empty() || !version.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Err(format!("unexpected APP_VERSION {:?}", version).into());
    }
    Ok(format!("web-account@{}", version))
}

fn normalize_app_version(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else if value.contains('@') {
        Some(value.to_string())
    } else {
        Some(format!("web-account@{}", value))
    }
}

fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = text.find(start)? + start.len();
    let len = text[from..].find(end)?;
    Some(&text[from..from + len])
}

fn api_error(endpoint: &str, resp: &Value) -> String {
    format!(
        "Proton API {} returned no usable data (Code {}, {})",
        endpoint,
        resp["Code"],
        resp["Error"].as_str().unwrap_or("no error message")
    )
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
) -> Result<bool, Box<dyn std::error::Error>> {
    // Fetch ProtonVPN servers
    let resp: Value = client
        .get("https://account.protonvpn.com/api/vpn/v1/logicals")
        .send()?
        .json()?;
    let mut servers: Vec<Value> = match resp["LogicalServers"].as_array() {
        Some(list) => list.to_owned(),
        None => return Err(api_error("logicals", &resp).into()),
    };

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
        return Ok(false);
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
    let x25519_priv = get_x25519_priv(&keys[2])?;

    let peer_public = reg["Features"]["peerPublicKey"].as_str().unwrap_or("");
    let endpoint_ip = reg["Features"]["peerIp"].as_str().unwrap_or("");
    if peer_public.is_empty() || endpoint_ip.is_empty() {
        return Err(api_error("certificate", &reg).into());
    }
    println!("[INFO] New endpoint: {}:51820", endpoint_ip);

    // Update MikroTik
    update_mikrotik_wg(
        mikrotik_host,
        mikrotik_user,
        mikrotik_pass,
        &x25519_priv,
        peer_public,
        endpoint_ip,
    )?;

    Ok(true)
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
    if priv_key_stripped.is_empty() || pub_key_stripped.is_empty() {
        return Err(api_error("certificate/key/EC", &resp).into());
    }
    Ok([priv_key_full, pub_key_stripped, priv_key_stripped])
}

fn get_x25519_priv(priv_key: &str) -> Result<String, Box<dyn std::error::Error>> {
    let decoded = STANDARD.decode(priv_key)?;
    if decoded.len() < 32 {
        return Err("Proton private key is shorter than 32 bytes".into());
    }
    let hash = Sha512::digest(&decoded[decoded.len() - 32..]);
    let mut h = hash[..32].to_vec();
    h[0] &= 0xf8;
    h[31] &= 0x7f;
    h[31] |= 0x40;
    Ok(STANDARD.encode(&h))
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
        println!(
            "[SSH] {}",
            cmd.split(" private-key=").next().unwrap_or(&cmd)
        );
        channel.exec(&cmd)?;
        let mut s = String::new();
        channel.read_to_string(&mut s)?;
        println!("[SSH Output]:\n{}", s);
        channel.wait_close()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{between, exit_ip, normalize_app_version, parse_flag};

    #[test]
    fn app_version_normalization() {
        assert_eq!(
            normalize_app_version("5.0.426.1").as_deref(),
            Some("web-account@5.0.426.1")
        );
        assert_eq!(
            normalize_app_version(" web-account@5.0.426.1 ").as_deref(),
            Some("web-account@5.0.426.1")
        );
        assert_eq!(normalize_app_version("  "), None);
    }

    #[test]
    fn between_finds_app_version() {
        let js = "x={APP_VERSION:\"5.0.426.1\",APP_NAME:\"proton-account\"}";
        assert_eq!(between(js, "APP_VERSION:\"", "\""), Some("5.0.426.1"));
        assert_eq!(between(js, "MISSING:\"", "\""), None);
    }

    #[test]
    fn flag_values() {
        assert_eq!(parse_flag("true"), Some(true));
        assert_eq!(parse_flag(" TRUE "), Some(true));
        assert_eq!(parse_flag("1"), Some(true));
        assert_eq!(parse_flag("false"), Some(false));
        assert_eq!(parse_flag(""), Some(false));
        assert_eq!(parse_flag("enabled"), None);
    }

    #[test]
    fn exit_ip_from_cloudflare_trace() {
        let body = "fl=12f1\nh=1.1.1.1\nip=203.0.113.7\nts=1795375164.1\nwarp=off\n";
        assert_eq!(exit_ip(body), Some("203.0.113.7"));
    }

    #[test]
    fn exit_ip_missing() {
        assert_eq!(exit_ip("<html>blocked</html>"), None);
    }
}
