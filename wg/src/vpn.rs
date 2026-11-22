use anyhow::{anyhow, bail, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono::Local;
use rand::seq::IndexedRandom;
use serde_json::{json, Value};
use sha2::{Digest, Sha512};
use tracing::info;

use crate::config::{MikrotikConfig, Secret, ServerFilter};
use crate::mikrotik;
use crate::proton::Proton;

pub enum Outcome {
    Applied { server: String, endpoint: String },
    NoServer,
}

pub fn regenerate(
    proton: &mut Proton,
    filter: &ServerFilter,
    mikrotik_cfg: &MikrotikConfig,
) -> Result<Outcome> {
    let logicals = proton.get("/api/vpn/v1/logicals")?;
    let servers = logicals["LogicalServers"]
        .as_array()
        .ok_or_else(|| anyhow!("Proton API /api/vpn/v1/logicals returned no LogicalServers"))?;
    let candidates: Vec<&Value> = servers.iter().filter(|s| matches(s, filter)).collect();
    info!(
        total = servers.len(),
        matching = candidates.len(),
        "Proton servers fetched"
    );
    let Some(server) = candidates.choose(&mut rand::rng()) else {
        return Ok(Outcome::NoServer);
    };
    let name = server["Name"].as_str().unwrap_or("").to_string();
    info!(server = %name, "Proton server selected");

    let keys = proton.get("/api/vpn/v1/certificate/key/EC")?;
    let private_b64 = pem_body(keys["PrivateKey"].as_str().unwrap_or(""))
        .ok_or_else(|| anyhow!("Proton API returned no private key"))?;
    let public_b64 = pem_body(keys["PublicKey"].as_str().unwrap_or(""))
        .ok_or_else(|| anyhow!("Proton API returned no public key"))?;
    let wg_private = x25519_private_key(&private_b64)?;

    let registration = proton.post(
        "/api/vpn/v1/certificate",
        &certificate_request(server, &public_b64),
    )?;
    let peer_public = registration["Features"]["peerPublicKey"]
        .as_str()
        .unwrap_or("");
    let endpoint = registration["Features"]["peerIp"].as_str().unwrap_or("");
    if peer_public.is_empty() || endpoint.is_empty() {
        bail!("Proton API /api/vpn/v1/certificate returned no peer key or endpoint");
    }

    mikrotik::apply_peer(mikrotik_cfg, &wg_private, peer_public, endpoint)?;
    Ok(Outcome::Applied {
        server: name,
        endpoint: endpoint.to_string(),
    })
}

fn matches(server: &Value, filter: &ServerFilter) -> bool {
    let entry = server["EntryCountry"].as_str().unwrap_or("");
    let exit = server["ExitCountry"].as_str().unwrap_or("");
    let flags = feature_flags(server["Features"].as_u64().unwrap_or(0));
    server["Status"].as_u64() == Some(1)
        && server["Tier"].as_u64() == Some(filter.tier)
        && filter.countries.iter().any(|c| c == entry || c == exit)
        && filter.features.iter().all(|f| flags.contains(&f.as_str()))
}

fn feature_flags(bits: u64) -> [&'static str; 5] {
    let flag =
        |bit: u64, on: &'static str, off: &'static str| if bits & bit != 0 { on } else { off };
    [
        flag(1, "SecureCore", "-SecureCore"),
        flag(2, "TOR", "-TOR"),
        flag(4, "P2P", "-P2P"),
        flag(8, "XOR", "-XOR"),
        flag(16, "IPv6", "-IPv6"),
    ]
}

fn pem_body(pem: &str) -> Option<String> {
    let body: String = pem
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("-----"))
        .collect();
    if body.is_empty() {
        None
    } else {
        Some(body)
    }
}

fn x25519_private_key(ed25519_pkcs8_b64: &str) -> Result<Secret> {
    let decoded = STANDARD.decode(ed25519_pkcs8_b64)?;
    if decoded.len() < 32 {
        bail!("Proton private key is shorter than 32 bytes");
    }
    let hash = Sha512::digest(&decoded[decoded.len() - 32..]);
    let mut key = hash[..32].to_vec();
    key[0] &= 0xf8;
    key[31] &= 0x7f;
    key[31] |= 0x40;
    Ok(Secret::new(STANDARD.encode(&key)))
}

fn certificate_request(server: &Value, client_public_key: &str) -> Value {
    let name = server["Name"].as_str().unwrap_or("");
    json!({
        "ClientPublicKey": client_public_key,
        "Mode": "persistent",
        "DeviceName": format!("{}-{}", Local::now().format("%d/%m"), name),
        "Features": {
            "peerName": name,
            "peerIp": server["Servers"][0]["EntryIP"].as_str().unwrap_or(""),
            "peerPublicKey": server["Servers"][0]["X25519PublicKey"].as_str().unwrap_or(""),
            "platform": "Windows",
            "SplitTCP": true,
            "PortForwarding": true,
            "RandomNAT": true,
            "NetShieldLevel": 0
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter() -> ServerFilter {
        ServerFilter {
            countries: vec!["RO".to_string()],
            tier: 2,
            features: vec!["P2P".to_string(), "-SecureCore".to_string()],
        }
    }

    #[test]
    fn server_filter() {
        let good = json!({"EntryCountry": "RO", "ExitCountry": "RO", "Tier": 2, "Status": 1, "Features": 4});
        let offline = json!({"EntryCountry": "RO", "ExitCountry": "RO", "Tier": 2, "Status": 0, "Features": 4});
        let secure_core = json!({"EntryCountry": "CH", "ExitCountry": "RO", "Tier": 2, "Status": 1, "Features": 5});
        let other_country = json!({"EntryCountry": "DE", "ExitCountry": "DE", "Tier": 2, "Status": 1, "Features": 4});
        assert!(matches(&good, &filter()));
        assert!(!matches(&offline, &filter()));
        assert!(!matches(&secure_core, &filter()));
        assert!(!matches(&other_country, &filter()));
    }

    #[test]
    fn derives_clamped_x25519_key() {
        let pkcs8 = STANDARD.encode([7u8; 48]);
        let key = x25519_private_key(&pkcs8).expect("key");
        let bytes = STANDARD.decode(key.expose()).expect("base64");
        assert_eq!(bytes.len(), 32);
        assert_eq!(bytes[0] & 0x07, 0);
        assert_eq!(bytes[31] & 0x80, 0);
        assert_eq!(bytes[31] & 0x40, 0x40);
        assert!(crate::mikrotik::is_wireguard_key(key.expose()));
    }

    #[test]
    fn pem_body_strips_armor() {
        let pem =
            "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEI\nAAAA\n-----END PRIVATE KEY-----\n";
        assert_eq!(pem_body(pem).as_deref(), Some("MC4CAQAwBQYDK2VwBCIEIAAAA"));
        assert_eq!(pem_body(""), None);
    }
}
