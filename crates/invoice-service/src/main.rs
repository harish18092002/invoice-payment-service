mod app;
mod auth;
mod config;
mod customers;
mod domain;
mod error;
mod events;
mod idempotency;
mod invoices;
mod keys;
mod money;
mod pagination;
mod payments;
mod psp;
mod reconciler;
mod request_id;
mod webhooks;

use std::{sync::Arc, time::Duration};

use app::{AppState, build_router};
use config::Config;
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() {
    // RUST_LOG controls verbosity; default is info.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = match Config::from_env() {
        Ok(c) => c,
        Err(msg) => {
            tracing::error!("{msg}");
            std::process::exit(1);
        }
    };

    // One shared connection pool; sqlx clones are cheap handles to the same pool.
    let pool = match PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&config.database_url)
        .await
    {
        Ok(pool) => pool,
        Err(e) => {
            tracing::error!("could not connect to the database: {e}");
            std::process::exit(1);
        }
    };

    // The migrations/ folder is compiled into the binary; applied ones are skipped.
    if let Err(e) = sqlx::migrate!("./migrations").run(&pool).await {
        tracing::error!("migrations failed: {e}");
        std::process::exit(1);
    }
    tracing::info!("migrations applied");

    // The dispatcher runs for the life of the process, delivering webhooks from the outbox.
    // reqwest note: redirects are off and every request has a 5 second timeout.
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .expect("failed to build the HTTP client");
    tokio::spawn(webhooks::dispatcher::run(
        pool.clone(),
        http,
        Duration::from_millis(config.webhook_poll_ms),
        config.webhook_delay_scale,
    ));

    let psp = psp::PspClient::new(
        config.psp_url.clone(),
        Duration::from_secs(config.psp_timeout_secs),
    );
    // Finishes payment attempts the normal path left pending (a crash, a slow PSP).
    tokio::spawn(reconciler::run(
        pool.clone(),
        psp.clone(),
        reconciler::Settings {
            interval: Duration::from_secs(config.reconcile_interval_secs),
            min_age_secs: config.reconcile_min_age_secs as f64,
            not_found_after_secs: config.reconcile_not_found_after_secs as f64,
        },
    ));

    let state = Arc::new(AppState { config, pool, psp });
    let router = build_router(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080")
        .await
        .expect("failed to bind 0.0.0.0:8080");
    tracing::info!("invoice-service listening on 0.0.0.0:8080");

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
    tracing::info!("shut down cleanly");
}

// Resolves on Ctrl-C or (on unix) SIGTERM, which is what `docker stop` sends.
// axum then stops accepting new connections and lets in-flight requests finish.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
