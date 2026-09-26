//! Signal transport: parsing signal-cli JSON-RPC `receive` notifications,
//! the [`SignalTransport`] send abstraction, a [`MockSignal`] for tests, and
//! [`SignalCli`], which supervises a `signal-cli daemon` child over a UNIX
//! socket.

use async_trait::async_trait;
use std::sync::{Arc, Mutex};

use crate::types::IncomingMessage;

/// Parse a signal-cli JSON-RPC "receive" notification into an `IncomingMessage`.
/// Returns `None` for anything that isn't a data message (receipts, typing, sync, etc).
pub fn parse_envelope(v: &serde_json::Value, bot_id: &str, aliases: &[String]) -> Option<IncomingMessage> {
    let env = v.get("params")?.get("envelope")?;
    let dm = env.get("dataMessage")?;
    let body = dm.get("message")?.as_str()?.to_string();
    let source = env.get("source")?.as_str()?.to_string();
    let source_name = env.get("sourceName").and_then(|x| x.as_str()).map(String::from);
    let ts = env.get("timestamp").and_then(|x| x.as_i64()).unwrap_or(0);
    let group_id = dm.get("groupInfo").and_then(|g| g.get("groupId")).and_then(|x| x.as_str());
    let is_group = group_id.is_some();
    let room_id = group_id.map(String::from).unwrap_or_else(|| source.clone());
    // Addressed if there's a native Signal @-mention of the bot's number, OR the
    // message text names the bot (its number or a configured alias like its
    // profile name). The text match lets people write "toxic-trash, ..." instead
    // of a formal @-mention.
    let native_mention = dm.get("mentions").and_then(|m| m.as_array())
        .map(|arr| arr.iter().any(|m| m.get("number").and_then(|n| n.as_str()) == Some(bot_id)))
        .unwrap_or(false);
    let is_mention = native_mention || body_addresses_bot(&body, bot_id, aliases);
    let quoted_msg = dm.get("quote").and_then(|q| q.get("text")).and_then(|x| x.as_str()).map(String::from);
    Some(IncomingMessage { room_id, sender_id: source, sender_name: source_name, body,
        is_group, is_mention, quoted_msg, timestamp: ts })
}

/// Normalize a handle/message for name matching: lowercase, and treat `-` and `_`
/// as spaces, collapsing runs of whitespace. So "Toxic-Trash", "toxic trash" and
/// "toxic_trash" all normalize to "toxic trash".
fn normalize_handle(s: &str) -> String {
    let spaced: String = s.chars()
        .map(|c| if c == '-' || c == '_' { ' ' } else { c.to_ascii_lowercase() })
        .collect();
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// True if the message text names the bot — by its own id/number or any
/// configured alias (e.g. its profile name). Case-insensitive; `-`/`_` match
/// spaces. This is a text fallback for when someone addresses the bot by name
/// instead of using a formal Signal @-mention.
fn body_addresses_bot(body: &str, bot_id: &str, aliases: &[String]) -> bool {
    let nb = normalize_handle(body);
    std::iter::once(bot_id)
        .chain(aliases.iter().map(String::as_str))
        .any(|a| {
            let na = normalize_handle(a);
            !na.is_empty() && nb.contains(&na)
        })
}

#[async_trait]
pub trait SignalTransport: Send + Sync {
    async fn send(&self, room_id: &str, is_group: bool, text: &str) -> anyhow::Result<()>;
}

#[derive(Clone, Default)]
pub struct MockSignal { pub sent: Arc<Mutex<Vec<(String, String)>>> }
impl MockSignal { pub fn new() -> Self { Self::default() } }

#[async_trait]
impl SignalTransport for MockSignal {
    async fn send(&self, room_id: &str, _is_group: bool, text: &str) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push((room_id.to_string(), text.to_string()));
        Ok(())
    }
}

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex as AsyncMutex};

/// Real Signal transport: drives `signal-cli` in JSON-RPC daemon mode over a UNIX socket.
///
/// The child process and its socket connection are supervised in a background task
/// (spawned by `SignalCli::spawn`): if the child exits or the connection drops, it is
/// respawned with capped exponential backoff (1s, 2s, 4s, ... up to 30s).
pub struct SignalCli {
    account: String,
    writer: Arc<AsyncMutex<Option<OwnedWriteHalf>>>,
    next_id: AtomicU64,
}

impl SignalCli {
    /// Launch `signal-cli -a <account> --config <data_dir> daemon --socket <socket>`,
    /// connect to its JSON-RPC UNIX socket, and return a handle for sending messages
    /// plus a channel that yields parsed incoming messages as `receive` notifications
    /// arrive.
    pub async fn spawn(
        bin: &str,
        account: &str,
        socket: &Path,
        data_dir: &Path,
        mention_aliases: Vec<String>,
    ) -> anyhow::Result<(Arc<SignalCli>, mpsc::Receiver<IncomingMessage>)> {
        let bin = bin.to_string();
        let account = account.to_string();
        let socket: PathBuf = socket.to_path_buf();
        let data_dir: PathBuf = data_dir.to_path_buf();

        let (tx, rx) = mpsc::channel::<IncomingMessage>(256);
        let writer: Arc<AsyncMutex<Option<OwnedWriteHalf>>> = Arc::new(AsyncMutex::new(None));

        let client = Arc::new(SignalCli {
            account: account.clone(),
            writer: writer.clone(),
            next_id: AtomicU64::new(1),
        });

        // Supervisor task: spawn the child, connect, read notifications, and respawn
        // with capped exponential backoff whenever the connection or the child dies.
        tokio::spawn(async move {
            const MIN_BACKOFF: Duration = Duration::from_secs(1);
            const MAX_BACKOFF: Duration = Duration::from_secs(30);
            let mut backoff = MIN_BACKOFF;

            loop {
                // Best-effort cleanup of a stale socket left behind by a previous run.
                let _ = std::fs::remove_file(&socket);

                tracing::info!(account = %account, socket = %socket.display(), "spawning signal-cli daemon");

                let mut child = match Command::new(&bin)
                    .arg("-a").arg(&account)
                    .arg("--config").arg(&data_dir)
                    .arg("daemon").arg("--socket").arg(&socket)
                    .kill_on_drop(true)
                    .spawn()
                {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to spawn signal-cli binary; retrying");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                        continue;
                    }
                };

                // Wait for the socket file to appear and become connectable.
                let poll_interval = Duration::from_millis(200);
                let socket_wait_cap = Duration::from_secs(30);
                let mut waited = Duration::ZERO;
                let stream = loop {
                    if socket.exists() {
                        if let Ok(s) = UnixStream::connect(&socket).await {
                            break Some(s);
                        }
                    }
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            tracing::error!(?status, "signal-cli exited before its socket became ready");
                            break None;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::error!(error = %e, "error polling signal-cli child status");
                            break None;
                        }
                    }
                    if waited >= socket_wait_cap {
                        tracing::error!("timed out waiting for signal-cli socket to appear");
                        break None;
                    }
                    tokio::time::sleep(poll_interval).await;
                    waited += poll_interval;
                };

                let stream = match stream {
                    Some(s) => s,
                    None => {
                        let _ = child.kill().await;
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                        continue;
                    }
                };

                tracing::info!("connected to signal-cli daemon socket");
                backoff = MIN_BACKOFF; // reset backoff after a successful connection

                let (read_half, write_half) = stream.into_split();
                *writer.lock().await = Some(write_half);

                let mut lines = BufReader::new(read_half).lines();
                loop {
                    tokio::select! {
                        line = lines.next_line() => {
                            match line {
                                Ok(Some(line)) => {
                                    if line.trim().is_empty() { continue; }
                                    match serde_json::from_str::<serde_json::Value>(&line) {
                                        Ok(v) => {
                                            if let Some(msg) = parse_envelope(&v, &account, &mention_aliases) {
                                                if tx.send(msg).await.is_err() {
                                                    // Receiver dropped: nothing left to do.
                                                    return;
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(error = %e, line = %line, "failed to parse signal-cli JSON-RPC line");
                                        }
                                    }
                                }
                                Ok(None) => {
                                    tracing::warn!("signal-cli socket closed (EOF); will reconnect");
                                    break;
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "error reading signal-cli socket; will reconnect");
                                    break;
                                }
                            }
                        }
                        status = child.wait() => {
                            match status {
                                Ok(s) => tracing::warn!(status = %s, "signal-cli child exited; will respawn"),
                                Err(e) => tracing::warn!(error = %e, "error waiting on signal-cli child; will respawn"),
                            }
                            break;
                        }
                    }
                }

                // Connection lost: clear the writer so `send` fails fast until reconnected.
                *writer.lock().await = None;
                let _ = child.kill().await;

                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        });

        Ok((client, rx))
    }
}

#[async_trait]
impl SignalTransport for SignalCli {
    async fn send(&self, room_id: &str, is_group: bool, text: &str) -> anyhow::Result<()> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let params = if is_group {
            serde_json::json!({"groupId": room_id, "message": text})
        } else {
            serde_json::json!({"recipient": [room_id], "message": text})
        };
        let req = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "send", "params": params});
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');

        let mut guard = self.writer.lock().await;
        let w = guard.as_mut().ok_or_else(|| {
            anyhow::anyhow!("signal-cli socket for account {} is not connected", self.account)
        })?;
        // Bound the write under the lock: if signal-cli stops draining its socket
        // (connected but hung) an unbounded write here would hold `writer` forever,
        // which would also block the supervisor task's disconnect-clearing write and
        // stall failure-detection/respawn.
        tokio::time::timeout(Duration::from_secs(10), async {
            w.write_all(line.as_bytes()).await?;
            w.flush().await?;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .map_err(|_| anyhow::anyhow!("timed out writing to signal-cli socket"))??;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_records_sends() {
        let m = MockSignal::new();
        m.send("g1", true, "hello").await.unwrap();
        assert_eq!(m.sent.lock().unwrap().clone(), vec![("g1".to_string(), "hello".to_string())]);
    }

    #[test]
    fn parses_group_data_message() {
        let v: serde_json::Value = serde_json::from_str(r#"{
          "method":"receive","params":{"envelope":{
            "source":"+1000","sourceName":"Alice","timestamp":1710000000000,
            "dataMessage":{"message":"hey @bot","mentions":[{"number":"+15555550100"}],
              "groupInfo":{"groupId":"GID=="}}}}}"#).unwrap();
        let m = parse_envelope(&v, "+15555550100", &[]).unwrap();
        assert_eq!(m.room_id, "GID==");
        assert!(m.is_group);
        assert!(m.is_mention);
        assert_eq!(m.sender_id, "+1000");
        assert_eq!(m.body, "hey @bot");
    }

    fn group_msg(text: &str) -> serde_json::Value {
        serde_json::json!({"method":"receive","params":{"envelope":{
            "source":"+1000","sourceName":"Alice","timestamp":1,
            "dataMessage":{"message":text,"groupInfo":{"groupId":"GID=="}}}}})
    }

    #[test]
    fn text_name_addresses_bot_without_native_mention() {
        let aliases = vec!["toxic-trash".to_string()];
        // hyphen, space and underscore spellings all count as addressing the bot
        for text in ["toxic-trash what do you think?", "hey toxic trash you up?", "yo TOXIC_TRASH"] {
            let m = parse_envelope(&group_msg(text), "+15555550100", &aliases).unwrap();
            assert!(m.is_mention, "should be addressed by name: {text:?}");
        }
        // the bot's own number in text also counts
        let m = parse_envelope(&group_msg("call +15555550100 maybe"), "+15555550100", &aliases).unwrap();
        assert!(m.is_mention);
    }

    #[test]
    fn unrelated_group_text_is_not_addressed() {
        let aliases = vec!["toxic-trash".to_string()];
        let m = parse_envelope(&group_msg("anyone want tacos later"), "+15555550100", &aliases).unwrap();
        assert!(!m.is_mention);
    }

    #[test]
    fn direct_message_room_is_sender() {
        let v: serde_json::Value = serde_json::from_str(r#"{
          "method":"receive","params":{"envelope":{
            "source":"+1000","sourceName":"Alice","timestamp":1,
            "dataMessage":{"message":"hi"}}}}"#).unwrap();
        let m = parse_envelope(&v, "+15555550100", &[]).unwrap();
        assert_eq!(m.room_id, "+1000");
        assert!(!m.is_group);
        assert!(!m.is_mention);
    }

    #[test]
    fn non_data_message_is_none() {
        let v: serde_json::Value = serde_json::from_str(r#"{"method":"receive","params":{"envelope":{"source":"+1","receiptMessage":{}}}}"#).unwrap();
        assert!(parse_envelope(&v, "+15555550100", &[]).is_none());
    }
}
