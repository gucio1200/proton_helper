use std::io::Read;
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ssh2::Session;
use tracing::{debug, info};

use crate::config::{MikrotikConfig, Secret};

pub fn apply_peer(
    cfg: &MikrotikConfig,
    private_key: &Secret,
    peer_public_key: &str,
    endpoint: &str,
) -> Result<()> {
    if !is_wireguard_key(peer_public_key) {
        bail!("Proton returned an invalid WireGuard peer public key");
    }
    if !is_wireguard_key(private_key.expose()) {
        bail!("derived WireGuard private key is invalid");
    }
    let endpoint: IpAddr = endpoint
        .parse()
        .with_context(|| format!("Proton returned an invalid endpoint {endpoint:?}"))?;
    let wg = &cfg.wg_interface;

    let session = connect(cfg)?;
    apply(
        &session,
        &format!(
            "/interface/wireguard/set {wg} private-key=\"{}\"",
            private_key.expose()
        ),
        &format!("set the private key of {wg}"),
    )?;
    apply(
        &session,
        &format!(
            "/interface/wireguard/peers/set [find interface={wg}] public-key=\"{peer_public_key}\" endpoint-address={endpoint}"
        ),
        &format!("set the peer of {wg}"),
    )?;
    let peers = exec(
        &session,
        &format!("/interface/wireguard/peers/print terse where interface={wg}"),
    )?;
    if !peers.contains(peer_public_key) || !peers.contains(&format!("endpoint-address={endpoint}"))
    {
        bail!("RouterOS did not apply the new peer on {wg}");
    }
    info!(interface = %wg, endpoint = %endpoint, "MikroTik WireGuard peer updated");
    Ok(())
}

fn connect(cfg: &MikrotikConfig) -> Result<Session> {
    let addr = (cfg.host.as_str(), cfg.port)
        .to_socket_addrs()
        .with_context(|| format!("cannot resolve MikroTik host {}", cfg.host))?
        .next()
        .ok_or_else(|| anyhow!("MikroTik host {} has no address", cfg.host))?;
    let tcp = TcpStream::connect_timeout(&addr, cfg.timeout.min(Duration::from_secs(10)))
        .with_context(|| format!("cannot connect to MikroTik at {addr}"))?;
    let mut session = Session::new()?;
    session.set_tcp_stream(tcp);
    session.set_timeout(u32::try_from(cfg.timeout.as_millis()).unwrap_or(u32::MAX));
    session
        .handshake()
        .context("SSH handshake with MikroTik failed")?;
    session
        .userauth_password(&cfg.user, cfg.password.expose())
        .context("SSH login to MikroTik failed")?;
    if !session.authenticated() {
        bail!("SSH login to MikroTik failed");
    }
    Ok(session)
}

fn exec(session: &Session, command: &str) -> Result<String> {
    let mut channel = session.channel_session()?;
    channel.exec(command)?;
    let mut output = String::new();
    channel.read_to_string(&mut output)?;
    let mut stderr = String::new();
    channel.stderr().read_to_string(&mut stderr)?;
    channel.wait_close()?;
    output.push_str(&stderr);
    Ok(output)
}

fn apply(session: &Session, command: &str, label: &str) -> Result<()> {
    let output = exec(session, command)?;
    if let Some(problem) = routeros_error(&output) {
        bail!("RouterOS rejected '{label}': {problem}");
    }
    debug!(label, "RouterOS command applied");
    Ok(())
}

pub fn routeros_error(output: &str) -> Option<&str> {
    output.lines().map(str::trim).find(|line| {
        let line = line.to_ascii_lowercase();
        line.starts_with("failure")
            || line.starts_with("error")
            || line.contains("bad command")
            || line.contains("syntax error")
            || line.contains("expected ")
            || line.contains("no such item")
            || line.contains("invalid value")
    })
}

pub fn is_wireguard_key(value: &str) -> bool {
    value.len() == 44
        && STANDARD
            .decode(value)
            .map(|bytes| bytes.len() == 32)
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_routeros_errors() {
        assert!(routeros_error("").is_none());
        assert!(routeros_error("\r\n").is_none());
        assert!(routeros_error("no such item (4)").is_some());
        assert!(routeros_error("failure: already have such entry").is_some());
        assert!(routeros_error("expected end of command (line 1 column 30)").is_some());
    }

    #[test]
    fn validates_wireguard_keys() {
        assert!(is_wireguard_key(
            "yfZJe4K4rNMEFlSW0f3+Gisay8zg31plWe1EffyCWFg="
        ));
        assert!(!is_wireguard_key("not-a-key"));
        assert!(!is_wireguard_key(
            "key\" ; /system reboot ; \"aaaaaaaaaaaaaaaaaaaaaa"
        ));
    }
}
