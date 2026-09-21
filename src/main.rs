use clap::Parser;
use signal_bot::config::{AppConfig, Cli};
use signal_bot::llm::{LlmBackend, OllamaClient};
use signal_bot::metrics::Metrics;
use signal_bot::orchestrator::Dispatcher;
use signal_bot::personalities::Personalities;
use signal_bot::router::Router;
use signal_bot::signal::SignalCli;
use signal_bot::store::Store;
use signal_bot::summarizer;
use signal_bot::web::{self, AppState};
use std::sync::{Arc, OnceLock};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let filter = if cli.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into()),
        )
        .init();

    let cfg = AppConfig::load(&cli.config)?;
    let store = Store::connect(&cfg.db_url()).await?;

    if let Some(username) = cli.set_admin.as_deref() {
        let password = std::env::var("SIGNAL_BOT_ADMIN_PASSWORD").unwrap_or_default();
        if password.is_empty() {
            eprintln!(
                "error: --set-admin requires the SIGNAL_BOT_ADMIN_PASSWORD env var to be set (non-empty); refusing to read the password from argv"
            );
            std::process::exit(1);
        }
        store.set_admin(username, &password).await?;
        println!("admin credential set for '{username}'");
        return Ok(());
    }

    // Seed the `settings` table (row id=1) from config defaults on first run.
    // Skipped for the `--set-admin` bootstrap branch above, which already
    // returned early; this runs before both the repl branch and the main
    // loop so both benefit.
    if !store.settings_exists().await? {
        store.upsert_settings(&cfg.seed_settings_row()).await?;
    }

    let personalities = Arc::new(Personalities::load_dir(&cfg.personalities_dir)?);
    let dry_run = cli.dry_run || cfg.dry_run;

    // Metrics + the Ollama client are created early (before the web server)
    // and shared with the Router built later, so `/api/metrics` and the
    // actual generation path report on the exact same instances.
    let timeout = store.get_settings().await?.ollama_timeout_secs as u64;
    let metrics = Metrics::new();
    let ollama = Arc::new(OllamaClient::new(cfg.ollama_url.clone(), timeout));

    // The Dispatcher doesn't exist yet at this point (it needs `SignalCli` and
    // the `Router`, built further below), but the web server is spawned now.
    // This cell lets `/api/metrics` read dispatcher gauges once it's ready,
    // reporting empty gauges in the meantime.
    let dispatcher_cell: Arc<OnceLock<Arc<Dispatcher>>> = Arc::new(OnceLock::new());

    // web state + server
    let state = AppState::new(
        store.clone(),
        personalities.clone(),
        metrics.clone(),
        ollama.clone(),
        dispatcher_cell.clone(),
    );
    {
        let (bind, cert, key, st) = (
            cfg.bind_addr.clone(),
            cfg.cert_path.clone(),
            cfg.key_path.clone(),
            state.clone(),
        );
        tokio::spawn(async move {
            loop {
                if let Err(e) = web::serve_tls(st.clone(), &bind, &cert, &key).await {
                    tracing::error!("web server exited: {e}; restarting in 5s");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        });
    }

    if cli.repl {
        signal_bot::repl::run(store, personalities, dry_run, cfg.ollama_url.clone()).await?;
        return Ok(());
    }

    // real signal + llm + router
    let (signal, mut rx) = SignalCli::spawn(
        &cfg.signal_bin,
        &cfg.signal_account,
        &cfg.socket_path(),
        &cfg.data_dir,
    )
    .await?;
    // Web-search provider (Tavily). Built only when an API key is configured
    // (secret, from config.toml). Absent key => web search unavailable.
    let search: Option<Arc<dyn signal_bot::search::SearchProvider>> = cfg
        .search_api_key
        .clone()
        .filter(|k| !k.trim().is_empty())
        .map(|key| {
            Arc::new(signal_bot::search::TavilyClient::new(key, 20))
                as Arc<dyn signal_bot::search::SearchProvider>
        });
    if search.is_some() {
        tracing::info!("web-search provider configured (Tavily)");
    }
    let router = Arc::new(Router::new(
        store.clone(),
        personalities.clone(),
        ollama.clone() as Arc<dyn LlmBackend>,
        signal,
        cfg.signal_account.clone(),
        dry_run,
        metrics.clone(),
        Some(state.tx.clone()),
        search,
    ));
    let dispatcher = Dispatcher::new(router);
    let _ = dispatcher_cell.set(dispatcher.clone());

    // hot-reload watcher
    signal_bot::personalities_watch::spawn(personalities.clone(), cfg.personalities_dir.clone());

    // Per-room summarization sweep: shares the dispatcher's global inference
    // permit so a sweep never competes with a user-facing generation for the
    // model (it just waits its turn). Settings-gated.
    let settings = store.get_settings().await?;
    if settings.summary_enabled {
        let model = personalities.get_or_default(None).model.clone();
        let interval = std::time::Duration::from_secs((settings.summary_interval_hours.max(1) as u64) * 3600);
        summarizer::spawn(store.clone(), ollama.clone() as Arc<dyn LlmBackend>, model, dispatcher.inference_permit(), interval);
    }

    // receive loop: hand off to the dispatcher, which routes each message to
    // its room's actor (coalescing bursts, serializing generation behind the
    // global inference permit). SSE publishing happens inside the router's
    // turn path (see Router::handle_burst), not here.
    while let Some(msg) = rx.recv().await {
        dispatcher.dispatch(msg).await;
    }
    Ok(())
}
