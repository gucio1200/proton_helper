use std::cmp::Ordering;

use anyhow::{anyhow, bail, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono::Local;
use rand::seq::IndexedRandom;
use rand::Rng;
use serde_json::{json, Value};
use sha2::{Digest, Sha512};
use tracing::info;

use crate::config::{MikrotikConfig, Secret, ServerFilter};
use crate::mikrotik;
use crate::proton::Proton;

pub enum Outcome {
    Applied(Applied),
    NoServer,
}

pub struct Applied {
    pub server: String,
    pub country: String,
    pub endpoint: String,
    pub score: f64,
    pub load: u64,
}

pub struct Selection<'a> {
    pub server: &'a Value,
    pub physical: &'a Value,
    pub country: String,
    pub countries: usize,
    pub candidates: usize,
    pub over_load_limit: bool,
}

pub fn regenerate(
    proton: &mut Proton,
    filter: &ServerFilter,
    mikrotik_cfg: &MikrotikConfig,
    avoid: Option<&str>,
) -> Result<Outcome> {
    let logicals = proton.get("/api/vpn/v1/logicals")?;
    let servers = logicals["LogicalServers"]
        .as_array()
        .ok_or_else(|| anyhow!("Proton API /api/vpn/v1/logicals returned no LogicalServers"))?;
    let Some(selection) = select(servers, filter, avoid, &mut rand::rng()) else {
        info!(total = servers.len(), "no Proton server matches the filter");
        return Ok(Outcome::NoServer);
    };
    let server = selection.server;
    let name = name_of(server).to_string();
    info!(
        total = servers.len(),
        candidates = selection.candidates,
        countries = selection.countries,
        country = %selection.country,
        server = %name,
        score = score_of(server),
        load = load_of(server),
        over_load_limit = selection.over_load_limit,
        "Proton server selected"
    );

    let keys = proton.get("/api/vpn/v1/certificate/key/EC")?;
    let private_b64 = pem_body(keys["PrivateKey"].as_str().unwrap_or(""))
        .ok_or_else(|| anyhow!("Proton API returned no private key"))?;
    let public_b64 = pem_body(keys["PublicKey"].as_str().unwrap_or(""))
        .ok_or_else(|| anyhow!("Proton API returned no public key"))?;
    let wg_private = x25519_private_key(&private_b64)?;

    let registration = proton.post(
        "/api/vpn/v1/certificate",
        &certificate_request(server, selection.physical, &public_b64),
    )?;
    let peer_public = registration["Features"]["peerPublicKey"]
        .as_str()
        .unwrap_or("");
    let endpoint = registration["Features"]["peerIp"].as_str().unwrap_or("");
    if peer_public.is_empty() || endpoint.is_empty() {
        bail!("Proton API /api/vpn/v1/certificate returned no peer key or endpoint");
    }

    mikrotik::apply_peer(mikrotik_cfg, &wg_private, peer_public, endpoint)?;
    Ok(Outcome::Applied(Applied {
        server: name,
        country: selection.country,
        endpoint: endpoint.to_string(),
        score: score_of(server),
        load: load_of(server),
    }))
}

pub fn select<'a, R: Rng + ?Sized>(
    servers: &'a [Value],
    filter: &ServerFilter,
    avoid: Option<&str>,
    rng: &mut R,
) -> Option<Selection<'a>> {
    let eligible: Vec<&Value> = servers
        .iter()
        .filter(|s| matches(s, filter) && enabled_physical(s).next().is_some())
        .collect();
    let pool: Vec<&Value> = match avoid {
        Some(previous) if eligible.iter().any(|s| name_of(s) != previous) => eligible
            .iter()
            .copied()
            .filter(|s| name_of(s) != previous)
            .collect(),
        _ => eligible,
    };

    let mut countries: Vec<&str> = pool.iter().map(|s| exit_country(s)).collect();
    countries.sort_unstable();
    countries.dedup();
    let country = *countries.choose(rng)?;

    let in_country: Vec<&Value> = pool
        .iter()
        .copied()
        .filter(|s| exit_country(s) == country)
        .collect();
    let under_limit: Vec<&Value> = in_country
        .iter()
        .copied()
        .filter(|s| load_of(s) <= filter.max_load)
        .collect();
    let over_load_limit = under_limit.is_empty();
    let ranked = if over_load_limit {
        in_country
    } else {
        under_limit
    };
    let server = ranked.into_iter().min_by(|a, b| rank(a, b))?;
    let physical: Vec<&Value> = enabled_physical(server).collect();
    let physical = *physical.choose(rng)?;

    Some(Selection {
        server,
        physical,
        country: country.to_string(),
        countries: countries.len(),
        candidates: pool.len(),
        over_load_limit,
    })
}

fn rank(a: &Value, b: &Value) -> Ordering {
    score_of(a)
        .total_cmp(&score_of(b))
        .then_with(|| load_of(a).cmp(&load_of(b)))
}

fn matches(server: &Value, filter: &ServerFilter) -> bool {
    let exit = exit_country(server);
    let flags = feature_flags(server["Features"].as_u64().unwrap_or(0));
    server["Status"].as_u64() == Some(1)
        && server["Tier"].as_u64() == Some(filter.tier)
        && filter.countries.iter().any(|c| c == exit)
        && !filter.exclude_countries.iter().any(|c| c == exit)
        && filter.features.iter().all(|f| flags.contains(&f.as_str()))
}

fn enabled_physical(server: &Value) -> impl Iterator<Item = &Value> {
    server["Servers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| {
            p["Status"].as_u64() != Some(0)
                && p["EntryIP"].as_str().is_some_and(|ip| !ip.is_empty())
                && p["X25519PublicKey"]
                    .as_str()
                    .is_some_and(|key| !key.is_empty())
        })
}

fn name_of(server: &Value) -> &str {
    server["Name"].as_str().unwrap_or("")
}

fn exit_country(server: &Value) -> &str {
    server["ExitCountry"].as_str().unwrap_or("")
}

fn score_of(server: &Value) -> f64 {
    server["Score"].as_f64().unwrap_or(f64::MAX)
}

fn load_of(server: &Value) -> u64 {
    server["Load"].as_u64().unwrap_or(100)
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

fn certificate_request(server: &Value, physical: &Value, client_public_key: &str) -> Value {
    let name = name_of(server);
    json!({
        "ClientPublicKey": client_public_key,
        "Mode": "persistent",
        "DeviceName": format!("{}-{}", Local::now().format("%d/%m"), name),
        "Features": {
            "peerName": name,
            "peerIp": physical["EntryIP"].as_str().unwrap_or(""),
            "peerPublicKey": physical["X25519PublicKey"].as_str().unwrap_or(""),
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

    use crate::config::expand_countries;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    fn filter() -> ServerFilter {
        ServerFilter {
            countries: vec!["RO".to_string()],
            exclude_countries: Vec::new(),
            tier: 2,
            features: vec!["P2P".to_string(), "-SecureCore".to_string()],
            max_load: 80,
        }
    }

    fn europe_without_pl() -> ServerFilter {
        ServerFilter {
            countries: expand_countries(&["EUROPE".to_string(), "RU".to_string()]),
            exclude_countries: vec!["PL".to_string(), "RU".to_string()],
            ..filter()
        }
    }

    fn server(name: &str, country: &str, score: f64, load: u64) -> Value {
        json!({
            "Name": name, "EntryCountry": country, "ExitCountry": country, "Tier": 2,
            "Status": 1, "Features": 4, "Score": score, "Load": load,
            "Servers": [
                {"Status": 0, "EntryIP": "192.0.2.1", "X25519PublicKey": "off"},
                {"Status": 1, "EntryIP": "192.0.2.2", "X25519PublicKey": "on"}
            ]
        })
    }

    #[test]
    fn server_filter() {
        let good = json!({"EntryCountry": "RO", "ExitCountry": "RO", "Tier": 2, "Status": 1, "Features": 4});
        let offline = json!({"EntryCountry": "RO", "ExitCountry": "RO", "Tier": 2, "Status": 0, "Features": 4});
        let secure_core = json!({"EntryCountry": "CH", "ExitCountry": "RO", "Tier": 2, "Status": 1, "Features": 5});
        let other_country = json!({"EntryCountry": "DE", "ExitCountry": "DE", "Tier": 2, "Status": 1, "Features": 4});
        let free = json!({"EntryCountry": "RO", "ExitCountry": "RO", "Tier": 0, "Status": 1, "Features": 4});
        let no_p2p = json!({"EntryCountry": "RO", "ExitCountry": "RO", "Tier": 2, "Status": 1, "Features": 0});
        assert!(matches(&good, &filter()));
        assert!(!matches(&offline, &filter()));
        assert!(!matches(&secure_core, &filter()));
        assert!(!matches(&other_country, &filter()));
        assert!(!matches(&free, &filter()));
        assert!(!matches(&no_p2p, &filter()));
    }

    #[test]
    fn picks_best_score_in_a_random_european_country_never_poland() {
        let servers = vec![
            server("PL#1", "PL", 0.1, 5),
            server("RU#1", "RU", 0.1, 5),
            server("DE#1", "DE", 3.0, 20),
            server("DE#2", "DE", 1.5, 30),
            server("NL#1", "NL", 2.0, 10),
            server("NL#2", "NL", 2.5, 10),
            server("US#1", "US", 0.1, 5),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for seed in 0..64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let pick = select(&servers, &europe_without_pl(), None, &mut rng).expect("selection");
            let name = name_of(pick.server);
            assert!(name == "DE#2" || name == "NL#1", "unexpected {name}");
            assert_eq!(pick.countries, 2);
            assert_eq!(pick.physical["EntryIP"], "192.0.2.2");
            seen.insert(pick.country);
        }
        assert_eq!(seen.len(), 2, "both countries should come up");
    }

    #[test]
    fn load_limit_skips_overloaded_servers_unless_all_are() {
        let busy = vec![server("DE#1", "DE", 1.0, 95), server("DE#2", "DE", 2.0, 40)];
        let mut rng = StdRng::seed_from_u64(1);
        let pick = select(&busy, &europe_without_pl(), None, &mut rng).expect("selection");
        assert_eq!(name_of(pick.server), "DE#2");
        assert!(!pick.over_load_limit);

        let all_busy = vec![server("DE#1", "DE", 1.0, 95), server("DE#2", "DE", 2.0, 99)];
        let pick = select(&all_busy, &europe_without_pl(), None, &mut rng).expect("selection");
        assert_eq!(name_of(pick.server), "DE#1");
        assert!(pick.over_load_limit);
    }

    #[test]
    fn avoids_the_previous_server_when_there_is_an_alternative() {
        let servers = vec![server("DE#1", "DE", 1.0, 10), server("DE#2", "DE", 2.0, 10)];
        let mut rng = StdRng::seed_from_u64(7);
        let pick =
            select(&servers, &europe_without_pl(), Some("DE#1"), &mut rng).expect("selection");
        assert_eq!(name_of(pick.server), "DE#2");

        let only = vec![server("DE#1", "DE", 1.0, 10)];
        let pick = select(&only, &europe_without_pl(), Some("DE#1"), &mut rng).expect("selection");
        assert_eq!(name_of(pick.server), "DE#1");
    }

    #[test]
    fn skips_logicals_without_an_enabled_physical_server() {
        let mut dead = server("DE#1", "DE", 0.5, 10);
        dead["Servers"] = json!([{"Status": 0, "EntryIP": "192.0.2.9", "X25519PublicKey": "k"}]);
        let servers = vec![dead, server("DE#2", "DE", 2.0, 10)];
        let mut rng = StdRng::seed_from_u64(3);
        let pick = select(&servers, &europe_without_pl(), None, &mut rng).expect("selection");
        assert_eq!(name_of(pick.server), "DE#2");
        assert!(select(&[], &europe_without_pl(), None, &mut rng).is_none());
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
