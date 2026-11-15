//! Spotifast jam server: shared playback and a shared queue for listeners
//! who each play on their own Spotify account.
//!
//! See README.md for the API.

mod protocol;
mod routes;
mod state;

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Address and port to listen on.
    #[arg(long, env = "JAM_BIND", default_value = "0.0.0.0:8787")]
    bind: String,
    /// Forget a jam nobody has touched for this many hours.
    #[arg(long, env = "JAM_IDLE_HOURS", default_value_t = 12)]
    idle_hours: u64,
    /// End a jam whose host has not been heard from for this many minutes.
    #[arg(long, env = "JAM_HOST_GRACE_MINUTES", default_value_t = 5)]
    host_grace_minutes: u64,
    /// Most jams held at once.
    #[arg(long, env = "JAM_MAX_SESSIONS", default_value_t = 10_000)]
    max_sessions: usize,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "spotifast_jam_server=info,tower_http=warn".into()),
        )
        .init();
    let args = Args::parse();
    let store = Arc::new(state::Store::new(state::Config {
        idle_ttl_ms: args.idle_hours * 60 * 60 * 1000,
        host_grace_ms: args.host_grace_minutes * 60 * 1000,
        max_sessions: args.max_sessions,
        ..state::Config::default()
    }));

    let sweeper = Arc::clone(&store);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        loop {
            tick.tick().await;
            sweeper.sweep(routes::now_ms());
        }
    });

    let listener = tokio::net::TcpListener::bind(&args.bind).await?;
    tracing::info!("listening on {}", listener.local_addr()?);
    axum::serve(listener, routes::router(store))
        .with_graceful_shutdown(shutdown())
        .await
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = term => {}
    }
    tracing::info!("shutting down");
}
