use std::env;

use tracing_subscriber::EnvFilter;

pub fn init() {
    let filter = EnvFilter::try_from_env("LOG_LEVEL").unwrap_or_else(|_| EnvFilter::new("info"));
    let text = env::var("LOG_FORMAT")
        .map(|format| format.trim().eq_ignore_ascii_case("text"))
        .unwrap_or(false);

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stdout);

    if text {
        builder.init();
    } else {
        builder
            .json()
            .flatten_event(true)
            .with_current_span(false)
            .with_span_list(false)
            .init();
    }
}
