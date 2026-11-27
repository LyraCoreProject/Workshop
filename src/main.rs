mod config;
mod github;
mod ledger;
mod mcp;
mod raw;
mod snapshot;
mod state;
mod web;

use chrono::Utc;
use config::{Config, TOKEN_VARIABLE};
use github::{Cache, RestClient};
use state::AppState;
use std::path::PathBuf;
use std::sync::Arc;

fn config_path() -> anyhow::Result<PathBuf> {
    let mut args = std::env::args().skip(1);
    match (args.next().as_deref(), args.next(), args.next()) {
        (Some("--config"), Some(path), None) => Ok(PathBuf::from(path)),
        _ => anyhow::bail!("usage: workshop --config workshop.toml"),
    }
}

async fn refresh_forever(state: Arc<AppState>, gh: RestClient) {
    let mut cache = Cache::default();
    let mut interval = tokio::time::interval(state.config.refresh_interval());
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let result = github::fetch(&gh, &state.config, &mut cache).await;
        let requests = gh.take_request_count();
        match result {
            Ok(raw) => {
                let snapshot = snapshot::build(&raw, &state.config, Utc::now());
                eprintln!(
                    "refreshed: {} tasks from {} Ledger files ({} unparsed, {} waiting), {} open pull requests, {} API and {} raw requests",
                    snapshot.tasks.len(),
                    snapshot.ledger.files,
                    snapshot.ledger.unparsed.len(),
                    snapshot.ledger.waiting,
                    snapshot.open_pulls.len(),
                    requests.api,
                    requests.raw
                );
                state.replace(snapshot, requests);
            }
            Err(error) => {
                eprintln!(
                    "refresh failed after {} API and {} raw requests: {error:#}",
                    requests.api, requests.raw
                );
                state.record_error(Utc::now(), format!("{error:#}"), requests);
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::load(&config_path()?)?;
    let token = std::env::var(TOKEN_VARIABLE)
        .ok()
        .filter(|token| !token.is_empty());
    let gh = RestClient::new(token)?;
    let listen = config.listen;
    let state = Arc::new(AppState::new(config));
    tokio::spawn(refresh_forever(state.clone(), gh));
    let listener = tokio::net::TcpListener::bind(listen).await?;
    eprintln!("listening on http://{listen}");
    axum::serve(listener, web::router(state)).await?;
    Ok(())
}
