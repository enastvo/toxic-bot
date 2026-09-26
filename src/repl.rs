//! Interactive REPL for exercising the reply pipeline without Signal. Reads
//! `room|group(0/1)|mention(0/1)|sender|text` lines from stdin, runs them
//! through a real [`Router`] + Ollama with a [`MockSignal`] transport, and
//! prints the reply (or `[silent]`).

use crate::llm::OllamaClient;
use crate::personalities::Personalities;
use crate::router::Router;
use crate::signal::MockSignal;
use crate::store::Store;
use crate::types::IncomingMessage;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};

pub async fn run(store: Store, personalities: Arc<Personalities>, dry_run: bool, ollama_url: String) -> anyhow::Result<()> {
    let sig = Arc::new(MockSignal::new());
    let llm = Arc::new(OllamaClient::new(ollama_url, 300));
    let router = Router::new(store, personalities, llm, sig, "+bot".into(), dry_run, crate::metrics::Metrics::new(), None, None, vec![]);
    eprintln!("REPL: room|group(0/1)|mention(0/1)|sender|text  (Ctrl-D to exit)");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let parts: Vec<&str> = line.splitn(5, '|').collect();
        if parts.len() != 5 { eprintln!("bad format"); continue; }
        let msg = IncomingMessage {
            room_id: parts[0].into(), is_group: parts[1] == "1", is_mention: parts[2] == "1",
            sender_id: parts[3].into(), sender_name: Some(parts[3].into()),
            body: parts[4].into(), quoted_msg: None, timestamp: 0,
        };
        match router.handle(msg).await {
            Ok(Some(reply)) => println!("BOT> {reply}"),
            Ok(None) => println!("[silent]"),
            Err(e) => eprintln!("error: {e}"),
        }
    }
    Ok(())
}
