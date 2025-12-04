use crate::errors::AksError;
use anyhow::Result;
use clap::{ArgAction, Parser};

// Default values
const DEFAULT_PORT: u16 = 8080;
const DEFAULT_CACHE_TTL: u64 = 3600;
const DEFAULT_PREVIEW: &str = "false";

#[derive(Parser, Clone)]
#[clap(author, version, about, long_about = None)]
pub struct Config {
    #[arg(env = "AZ_SUBSCRIPTION_ID")]
    pub subscription_id: String,

    #[arg(long, env = "SHOW_PREVIEW", default_value = DEFAULT_PREVIEW, value_parser = clap::value_parser!(bool), action = ArgAction::Set)]
    pub show_preview: bool,

    #[arg(long, env = "HTTP_PORT", default_value_t = DEFAULT_PORT)]
    pub port: u16,

    #[arg(long, env = "CACHE_TTL_SECONDS", default_value_t = DEFAULT_CACHE_TTL)]
    pub cache_ttl_seconds: u64,
}

impl Config {
    pub fn from_env() -> Result<Self, AksError> {
        match Config::try_parse() {
            Ok(config) => Ok(config),
            Err(e) => {
                if e.kind() == clap::error::ErrorKind::DisplayHelp
                    || e.kind() == clap::error::ErrorKind::DisplayVersion
                {
                    e.exit();
                }
                Err(AksError::Config(format!("Configuration error: {e}")))
            }
        }
    }
}
