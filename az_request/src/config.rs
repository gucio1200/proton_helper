use clap::{ArgAction, Parser};

// Default values
const DEFAULT_PREVIEW: &str = "false";

#[derive(Parser, Clone)]
#[clap(author, version, about, long_about = None)]
pub struct Config {
    #[arg(env = "AZ_SUBSCRIPTION_ID")]
    pub subscription_id: String,

    #[arg(long, env = "SHOW_PREVIEW", default_value = DEFAULT_PREVIEW, value_parser = clap::value_parser!(bool), action = ArgAction::Set)]
    pub show_preview: bool,

    #[arg(long, env = "HTTP_PORT", default_value_t = 8080)]
    pub port: u16,

    #[arg(long, env = "CACHE_TTL_SECONDS", default_value_t = 3600)]
    pub cache_ttl_seconds: u64,
}

//impl Config {}
