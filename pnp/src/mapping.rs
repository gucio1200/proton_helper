use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use natpmp::{Natpmp, Protocol, Response};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tokio_retry::strategy::jitter;
use tokio_retry::Retry;
use tracing::debug;

use crate::config::Config;
use crate::report::Mapping;

const RETRIES: u32 = 5;
const RETRY_BASE: Duration = Duration::from_millis(50);
const POLL: Duration = Duration::from_millis(50);

pub struct Mapper {
    gateway: Ipv4Addr,
    internal_port: u16,
    public_port: u16,
    lifetime: u32,
    client: Option<Arc<Mutex<Natpmp>>>,
}

impl Mapper {
    pub fn new(config: &Config) -> Self {
        Self {
            gateway: config.gateway,
            internal_port: config.internal_port,
            public_port: config.public_port,
            lifetime: config.lifetime,
            client: None,
        }
    }

    pub async fn refresh(&mut self) -> Result<Mapping> {
        let result = self.request_both().await;
        if result.is_err() {
            self.client = None;
        }
        result
    }

    async fn request_both(&mut self) -> Result<Mapping> {
        let client = self.client()?;
        let tcp = self.request(&client, Protocol::TCP).await?;
        let udp = self.request(&client, Protocol::UDP).await?;
        Ok(Mapping { tcp, udp })
    }

    fn client(&mut self) -> Result<Arc<Mutex<Natpmp>>> {
        if let Some(client) = &self.client {
            return Ok(client.clone());
        }
        let client = Natpmp::new_with(self.gateway).context("cannot open the NAT-PMP socket")?;
        let client = Arc::new(Mutex::new(client));
        self.client = Some(client.clone());
        Ok(client)
    }

    async fn request(&self, client: &Arc<Mutex<Natpmp>>, protocol: Protocol) -> Result<u16> {
        let (internal_port, public_port, lifetime) =
            (self.internal_port, self.public_port, self.lifetime);
        Retry::spawn(backoff(), || {
            let client = client.clone();
            async move {
                let mut client = client.lock().await;
                map_port(&mut client, protocol, internal_port, public_port, lifetime)
                    .await
                    .inspect_err(|e| {
                        debug!(
                            protocol = ?protocol,
                            error = %format!("{e:#}"),
                            "NAT-PMP request failed"
                        )
                    })
            }
        })
        .await
        .with_context(|| format!("NAT-PMP {protocol:?} mapping"))
    }
}

fn backoff() -> impl Iterator<Item = Duration> {
    (0..RETRIES)
        .map(|attempt| RETRY_BASE * 2u32.pow(attempt))
        .map(jitter)
}

async fn map_port(
    client: &mut Natpmp,
    protocol: Protocol,
    internal_port: u16,
    public_port: u16,
    lifetime: u32,
) -> Result<u16> {
    client
        .send_port_mapping_request(protocol, internal_port, public_port, lifetime)
        .context("cannot send the NAT-PMP request")?;

    loop {
        match client.read_response_or_retry() {
            Ok(Response::TCP(resp)) if protocol == Protocol::TCP => return Ok(resp.public_port()),
            Ok(Response::UDP(resp)) if protocol == Protocol::UDP => return Ok(resp.public_port()),
            Ok(_) => bail!("unexpected NAT-PMP response type"),
            Err(natpmp::Error::NATPMP_TRYAGAIN) => sleep(POLL).await,
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_short_and_bounded() {
        let delays: Vec<Duration> = backoff().collect();
        assert_eq!(delays.len(), 5);
        for (attempt, delay) in delays.iter().enumerate() {
            assert!(*delay <= RETRY_BASE * 2u32.pow(attempt as u32), "{delay:?}");
        }
        assert!(delays.iter().sum::<Duration>() <= Duration::from_millis(1550));
    }
}
