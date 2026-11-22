use std::collections::BTreeMap;

use anyhow::Result;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::config::{ProtonSeed, Secret};
use crate::kube::Kube;

#[derive(Debug, Clone)]
pub struct Session {
    pub uid: String,
    pub access_token: Secret,
    pub refresh_token: Option<Secret>,
    pub session_id: Secret,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Stored,
    Seed,
}

impl Session {
    fn from_seed(seed: &ProtonSeed) -> Self {
        Self {
            uid: seed.uid.clone(),
            access_token: seed.access_token.clone(),
            refresh_token: seed.refresh_token.clone(),
            session_id: seed.session_id.clone(),
        }
    }

    fn from_data(data: &BTreeMap<String, String>) -> Option<Self> {
        let get = |key: &str| data.get(key).filter(|v| !v.is_empty()).cloned();
        Some(Self {
            uid: get("UID")?,
            access_token: Secret::new(get("ACCESS_TOKEN")?),
            refresh_token: get("REFRESH_TOKEN").map(Secret::new),
            session_id: Secret::new(get("SESSION_ID")?),
        })
    }

    fn to_data(&self, seed_hash: &str) -> BTreeMap<String, String> {
        let mut data = BTreeMap::new();
        data.insert("UID".to_string(), self.uid.clone());
        data.insert(
            "ACCESS_TOKEN".to_string(),
            self.access_token.expose().to_string(),
        );
        data.insert(
            "REFRESH_TOKEN".to_string(),
            self.refresh_token
                .as_ref()
                .map(|t| t.expose().to_string())
                .unwrap_or_default(),
        );
        data.insert(
            "SESSION_ID".to_string(),
            self.session_id.expose().to_string(),
        );
        data.insert("SEED_HASH".to_string(), seed_hash.to_string());
        data.insert("UPDATED_AT".to_string(), chrono::Utc::now().to_rfc3339());
        data
    }
}

pub struct SessionStore {
    kube: Option<Kube>,
    name: Option<String>,
    seed_hash: String,
}

impl SessionStore {
    pub fn new(name: Option<String>, seed: &ProtonSeed) -> Self {
        let kube = match &name {
            None => None,
            Some(_) => {
                match Kube::in_cluster() {
                    Ok(Some(kube)) => Some(kube),
                    Ok(None) => {
                        warn!("not running in Kubernetes, rotated Proton tokens are kept in memory only");
                        None
                    }
                    Err(e) => {
                        warn!(error = %format!("{e:#}"), "Kubernetes API unavailable, rotated Proton tokens are kept in memory only");
                        None
                    }
                }
            }
        };
        Self {
            kube,
            name,
            seed_hash: seed_hash(seed),
        }
    }

    pub fn persistent(&self) -> bool {
        self.kube.is_some() && self.name.is_some()
    }

    pub fn load(&self, seed: &ProtonSeed) -> (Session, Origin) {
        let seeded = Session::from_seed(seed);
        let (Some(kube), Some(name)) = (&self.kube, &self.name) else {
            return (seeded, Origin::Seed);
        };
        match kube.read_secret(name) {
            Ok(Some(data)) if data.get("SEED_HASH") == Some(&self.seed_hash) => {
                match Session::from_data(&data) {
                    Some(session) => {
                        info!(secret = %name, "using the rotated Proton session from the Kubernetes secret");
                        (session, Origin::Stored)
                    }
                    None => {
                        warn!(secret = %name, "stored Proton session is incomplete, using the 1Password seed");
                        (seeded, Origin::Seed)
                    }
                }
            }
            Ok(Some(_)) => {
                info!(secret = %name, "Proton session in 1Password changed, it replaces the stored session");
                (seeded, Origin::Seed)
            }
            Ok(None) => {
                info!(secret = %name, "no stored Proton session yet, using the 1Password seed");
                (seeded, Origin::Seed)
            }
            Err(e) => {
                warn!(secret = %name, error = %format!("{e:#}"), "cannot read the stored Proton session, using the 1Password seed");
                (seeded, Origin::Seed)
            }
        }
    }

    pub fn save(&self, session: &Session) -> Result<()> {
        let (Some(kube), Some(name)) = (&self.kube, &self.name) else {
            return Ok(());
        };
        kube.write_secret(name, &session.to_data(&self.seed_hash))
    }
}

pub fn seed_hash(seed: &ProtonSeed) -> String {
    let mut hasher = Sha256::new();
    for part in [
        seed.uid.as_str(),
        seed.access_token.expose(),
        seed.refresh_token
            .as_ref()
            .map(Secret::expose)
            .unwrap_or(""),
        seed.session_id.expose(),
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(refresh: &str) -> ProtonSeed {
        ProtonSeed {
            uid: "uid".to_string(),
            access_token: Secret::new("access"),
            refresh_token: Some(Secret::new(refresh)),
            session_id: Secret::new("sid"),
        }
    }

    #[test]
    fn seed_hash_changes_with_any_field() {
        assert_eq!(seed_hash(&seed("r1")), seed_hash(&seed("r1")));
        assert_ne!(seed_hash(&seed("r1")), seed_hash(&seed("r2")));
        assert_eq!(seed_hash(&seed("r1")).len(), 64);
    }

    #[test]
    fn session_round_trip() {
        let session = Session::from_seed(&seed("r1"));
        let data = session.to_data("hash");
        let restored = Session::from_data(&data).expect("complete session");
        assert_eq!(restored.uid, "uid");
        assert_eq!(restored.access_token.expose(), "access");
        assert_eq!(
            restored.refresh_token.as_ref().map(Secret::expose),
            Some("r1")
        );
        assert_eq!(data.get("SEED_HASH").map(String::as_str), Some("hash"));
    }

    #[test]
    fn incomplete_session_is_rejected() {
        let mut data = Session::from_seed(&seed("r1")).to_data("hash");
        data.remove("ACCESS_TOKEN");
        assert!(Session::from_data(&data).is_none());
    }
}
