use clap::Parser;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Parser, Debug)]
pub struct Cli {
    #[arg(long, default_value = "/etc/signal-bot/config.toml")]
    pub config: PathBuf,
    #[arg(long, env = "SIGNAL_BOT_DEBUG")]
    pub debug: bool,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub repl: bool,
    /// Subcommand-less admin bootstrap: `--set-admin user:pass`
    #[arg(long)]
    pub set_admin: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub data_dir: PathBuf,
    pub personalities_dir: PathBuf,
    pub signal_bin: String,
    pub signal_account: String,
    pub ollama_url: String,
    pub bind_addr: String,     // e.g. 0.0.0.0:8443 (LAN)
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    #[serde(default)] pub debug: bool,
    #[serde(default)] pub dry_run: bool,
}

impl AppConfig {
    pub fn load(path: &std::path::Path) -> anyhow::Result<AppConfig> {
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }
    pub fn db_url(&self) -> String { format!("sqlite://{}/bot.sqlite?mode=rwc", self.data_dir.display()) }
    pub fn socket_path(&self) -> PathBuf { self.data_dir.join("signal-cli.sock") }
}
