use crate::personalities::Personalities;
use std::path::PathBuf;
use std::sync::Arc;

pub fn spawn(p: Arc<Personalities>, dir: PathBuf) {
    // notify watcher
    let (p1, d1) = (p.clone(), dir.clone());
    std::thread::spawn(move || {
        use notify::{RecursiveMode, Watcher};
        let (tx, rx) = std::sync::mpsc::channel();
        let mut w = match notify::recommended_watcher(tx) { Ok(w) => w, Err(e) => { tracing::error!("watcher: {e}"); return; } };
        if let Err(e) = w.watch(&d1, RecursiveMode::NonRecursive) { tracing::error!("watch: {e}"); return; }
        while rx.recv().is_ok() {
            std::thread::sleep(std::time::Duration::from_millis(500));
            while rx.try_recv().is_ok() {}
            match p1.reload(&d1) { Ok(_) => tracing::info!("personalities reloaded"), Err(e) => tracing::error!("reload: {e}") }
        }
    });

    // SIGHUP
    #[cfg(unix)]
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut hup = match signal(SignalKind::hangup()) { Ok(s) => s, Err(e) => { tracing::error!("sighup: {e}"); return; } };
        while hup.recv().await.is_some() {
            match p.reload(&dir) { Ok(_) => tracing::info!("personalities reloaded (SIGHUP)"), Err(e) => tracing::error!("reload: {e}") }
        }
    });
}
