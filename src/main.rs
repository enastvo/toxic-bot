use clap::Parser;
use signal_bot::config::{AppConfig, Cli};
use signal_bot::llm::OllamaClient;
use signal_bot::personalities::Personalities;
use signal_bot::router::Router;
use signal_bot::signal::SignalCli;
use signal_bot::store::Store;
use signal_bot::web::{self, AppState, SseEvent};
use std::sync::Arc;

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

    if let Some(spec) = cli.set_admin.as_deref() {
        let (u, p) = spec
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("use --set-admin user:pass"))?;
        store.set_admin(u, p).await?;
        println!("admin credential set for '{u}'");
        return Ok(());
    }

    let personalities = Arc::new(Personalities::load_dir(&cfg.personalities_dir)?);
    let dry_run = cli.dry_run || cfg.dry_run;

    // web state + server
    let state = AppState::new(store.clone(), personalities.clone());
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
    let llm = Arc::new(OllamaClient::new(cfg.ollama_url.clone()));
    let router = Arc::new(Router::new(
        store,
        personalities.clone(),
        llm,
        signal,
        cfg.signal_account.clone(),
        dry_run,
    ));

    // hot-reload watcher
    signal_bot::personalities_watch::spawn(personalities.clone(), cfg.personalities_dir.clone());

    // receive loop
    while let Some(msg) = rx.recv().await {
        let router = router.clone();
        let tx = state.tx.clone();
        tokio::spawn(async move {
            let sender = msg.sender_name.clone().unwrap_or_else(|| msg.sender_id.clone());
            let room_id = msg.room_id.clone();
            let body = msg.body.clone();
            let _ = tx.send(SseEvent { room_id: room_id.clone(), sender, body });
            match router.handle(msg).await {
                Ok(Some(reply)) => {
                    let _ = tx.send(SseEvent { room_id, sender: "bot".into(), body: reply });
                }
                Ok(None) => {}
                Err(e) => tracing::error!("handle error: {e}"),
            }
        });
    }
    Ok(())
}
