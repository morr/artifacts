mod api;
mod store;

use std::sync::Arc;

use api::AppState;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
}

#[tokio::main]
async fn main() {
    let token = std::env::var("ARTIFACTS_TOKEN").unwrap_or_else(|_| {
        eprintln!("ARTIFACTS_TOKEN is required");
        std::process::exit(2);
    });
    let port: u16 = env_or("PORT", "8080").parse().unwrap_or_else(|_| {
        eprintln!("PORT must be a number");
        std::process::exit(2);
    });
    let data_dir = env_or("ARTIFACTS_DATA_DIR", "./data");
    let max_body: usize = env_or("ARTIFACTS_MAX_BODY", "10485760").parse().unwrap_or_else(|_| {
        eprintln!("ARTIFACTS_MAX_BODY must be a number of bytes");
        std::process::exit(2);
    });
    let public_base = env_or("ARTIFACTS_PUBLIC_BASE", &format!("http://localhost:{port}"));

    let store = store::FileStore::new(&data_dir).unwrap_or_else(|e| {
        eprintln!("cannot create data dir {data_dir}: {e}");
        std::process::exit(2);
    });

    let state = Arc::new(AppState { store, token: token.into_bytes(), public_base });

    let app = api::router(state, max_body);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .unwrap_or_else(|e| {
            eprintln!("cannot bind 0.0.0.0:{port}: {e}");
            std::process::exit(2);
        });
    eprintln!("artifacts-server listening on 0.0.0.0:{port}, data dir {data_dir}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sig) => sig.recv().await,
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
