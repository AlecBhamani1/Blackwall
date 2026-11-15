use std::{env, net::SocketAddr, process::ExitCode};

use blackwall_relay::{app_with_storage, OwnershipError, RelayConfig};
use thiserror::Error;
use tokio::net::TcpListener;

const DEFAULT_BIND: &str = "127.0.0.1:8787";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("blackwall-relay: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), StartupError> {
    let bind_value =
        environment_value("BLACKWALL_RELAY_BIND").unwrap_or_else(|| DEFAULT_BIND.to_owned());
    let bind = bind_value
        .parse::<SocketAddr>()
        .map_err(|source| StartupError::InvalidBind {
            value: bind_value,
            source,
        })?;
    let registration_token = environment_value("BLACKWALL_RELAY_TOKEN");
    // Without a token anyone could fill the permanent address ledger and pending invitations.
    if registration_token.is_none() && !bind.ip().is_loopback() {
        return Err(StartupError::MissingToken { bind });
    }
    let listener = TcpListener::bind(bind).await?;
    let local_addr = listener.local_addr()?;
    let config = RelayConfig {
        registration_token,
        ..RelayConfig::default()
    };
    let directory = environment_value("BLACKWALL_RELAY_DATA_DIR")
        .unwrap_or_else(|| "blackwall-relay-data".to_owned());
    let router = app_with_storage(config, std::path::Path::new(&directory))?;
    eprintln!("blackwall-relay listening on {local_addr}");
    axum::serve(listener, router).await?;
    Ok(())
}

fn environment_value(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[derive(Debug, Error)]
enum StartupError {
    #[error(transparent)]
    Ownership(#[from] OwnershipError),
    #[error("invalid BLACKWALL_RELAY_BIND value {value:?}: {source}")]
    InvalidBind {
        value: String,
        source: std::net::AddrParseError,
    },
    #[error(
        "BLACKWALL_RELAY_TOKEN must be set to a long random secret when BLACKWALL_RELAY_BIND \
         ({bind}) is not a loopback address"
    )]
    MissingToken { bind: SocketAddr },
    #[error("relay listener failed: {0}")]
    Io(#[from] std::io::Error),
}
