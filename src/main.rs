mod cache;
mod channels;
mod config;
mod github_fetch;
mod keypool;
mod logger;
mod mcp;
mod model;
mod router;
mod runtime;
mod server;
mod url_guard;
mod workbuddy_accounts;

use std::sync::Arc;

use config::Config;
use server::AppState;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "search_x=info".into()),
        )
        .init();

    let config_path =
        std::env::var("SEARCH_X_CONFIG").unwrap_or_else(|_| "config.toml".to_string());
    let mut config = match Config::load(&config_path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    runtime::import_grok_key(&mut config);
    let bind = config.server.bind.clone();
    let runtime = match runtime::build(&config) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };

    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("无法监听 {bind}: {error}");
            std::process::exit(1);
        }
    };
    tracing::info!("search-x listening on {bind}");
    let state = Arc::new(AppState::new(runtime, config_path));
    if let Err(error) = axum::serve(listener, server::build(state)).await {
        eprintln!("server error: {error}");
    }
}
