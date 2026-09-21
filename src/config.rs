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
    /// Subcommand-less admin bootstrap: `--set-admin username`.
    /// The password is NOT taken from argv (visible via /proc/<pid>/cmdline);
    /// it must be supplied via the `SIGNAL_BOT_ADMIN_PASSWORD` env var.
    #[arg(long)]
    pub set_admin: Option<String>,
}

fn default_keep_alive() -> String { "30m".to_string() }
fn default_ollama_timeout_secs() -> i64 { 300 }
fn default_repeat_penalty() -> f64 { 1.3 }
fn default_repeat_last_n() -> i64 { 256 }
fn default_num_predict() -> i64 { 512 }
fn default_num_ctx() -> i64 { 8192 }
fn default_temperature() -> f64 { 0.7 }
fn default_top_p() -> f64 { 0.9 }
fn default_summary_enabled() -> bool { true }
fn default_summary_interval_hours() -> i64 { 6 }
fn default_tools_enabled() -> bool { false }
fn default_web_search_enabled() -> bool { false }
fn default_max_tool_rounds() -> i64 { 2 }
/// Curated, low-malware-risk default set of domains the bot may search.
/// Editable live from the Settings page; kept in sync with migration 0004.
pub const DEFAULT_SEARCH_WHITELIST: &str = "wikipedia.org,wikidata.org,wiktionary.org,britannica.com,merriam-webster.com,nasa.gov,noaa.gov,weather.gov,nih.gov,ncbi.nlm.nih.gov,cdc.gov,nist.gov,who.int,arxiv.org,nature.com,science.org,reuters.com,apnews.com,bbc.com,npr.org,pbs.org,theguardian.com,economist.com,developer.mozilla.org,docs.python.org,docs.rs,stackoverflow.com,github.com,man7.org";
fn default_search_whitelist() -> String { DEFAULT_SEARCH_WHITELIST.to_string() }

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

    // Seed defaults for the `settings` table (row id=1), used to populate it on
    // first run. Task 13 will source these from DB settings thereafter; until
    // then `ollama_timeout_secs` here also drives the OllamaClient HTTP timeout.
    #[serde(default = "default_keep_alive")] pub keep_alive: String,
    #[serde(default = "default_ollama_timeout_secs")] pub ollama_timeout_secs: i64,
    #[serde(default = "default_repeat_penalty")] pub repeat_penalty: f64,
    #[serde(default = "default_repeat_last_n")] pub repeat_last_n: i64,
    #[serde(default = "default_num_predict")] pub num_predict: i64,
    #[serde(default = "default_num_ctx")] pub num_ctx: i64,
    #[serde(default = "default_temperature")] pub default_temperature: f64,
    #[serde(default = "default_top_p")] pub default_top_p: f64,
    #[serde(default = "default_summary_enabled")] pub summary_enabled: bool,
    #[serde(default = "default_summary_interval_hours")] pub summary_interval_hours: i64,
    #[serde(default = "default_tools_enabled")] pub tools_enabled: bool,
    #[serde(default = "default_web_search_enabled")] pub web_search_enabled: bool,
    #[serde(default = "default_search_whitelist")] pub search_whitelist: String,
    #[serde(default = "default_max_tool_rounds")] pub max_tool_rounds: i64,

    /// Web-search provider (Tavily) API key. SECRET: kept in config.toml only
    /// (root-owned), never stored in the DB or shown in the web UI. `None`
    /// disables web search regardless of the DB toggle.
    #[serde(default)] pub search_api_key: Option<String>,
}

impl AppConfig {
    pub fn load(path: &std::path::Path) -> anyhow::Result<AppConfig> {
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }
    pub fn db_url(&self) -> String { format!("sqlite://{}/bot.sqlite?mode=rwc", self.data_dir.display()) }
    pub fn socket_path(&self) -> PathBuf { self.data_dir.join("signal-cli.sock") }

    /// Build the `settings` table's seed row (id=1) from this config's seed fields.
    pub fn seed_settings_row(&self) -> crate::store::SettingsRow {
        crate::store::SettingsRow {
            keep_alive: self.keep_alive.clone(),
            ollama_timeout_secs: self.ollama_timeout_secs,
            repeat_penalty: self.repeat_penalty,
            repeat_last_n: self.repeat_last_n,
            num_predict: self.num_predict,
            num_ctx: self.num_ctx,
            default_temperature: self.default_temperature,
            default_top_p: self.default_top_p,
            summary_enabled: self.summary_enabled,
            summary_interval_hours: self.summary_interval_hours,
            tools_enabled: self.tools_enabled,
            web_search_enabled: self.web_search_enabled,
            search_whitelist: self.search_whitelist.clone(),
            max_tool_rounds: self.max_tool_rounds,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL_TOML: &str = r#"
        data_dir = "/var/lib/signal-bot"
        personalities_dir = "/etc/signal-bot/personalities"
        signal_bin = "/usr/local/bin/signal-cli"
        signal_account = "+15551234567"
        ollama_url = "http://127.0.0.1:11434"
        bind_addr = "0.0.0.0:8443"
        cert_path = "/etc/signal-bot/tls/bot.local.crt"
        key_path = "/etc/signal-bot/tls/bot.local.key"
    "#;

    #[test]
    fn minimal_toml_gets_seed_defaults() {
        let cfg: AppConfig = toml::from_str(MINIMAL_TOML).expect("parse minimal config");
        assert_eq!(cfg.keep_alive, "30m");
        assert_eq!(cfg.ollama_timeout_secs, 300);
        assert_eq!(cfg.repeat_penalty, 1.3);
        assert_eq!(cfg.repeat_last_n, 256);
        assert_eq!(cfg.num_predict, 512);
        assert_eq!(cfg.num_ctx, 8192);
        assert_eq!(cfg.default_temperature, 0.7);
        assert_eq!(cfg.default_top_p, 0.9);
        assert!(cfg.summary_enabled);
        assert_eq!(cfg.summary_interval_hours, 6);
        // tools default off; whitelist seeded; no API key by default
        assert!(!cfg.tools_enabled);
        assert!(!cfg.web_search_enabled);
        assert_eq!(cfg.max_tool_rounds, 2);
        assert!(cfg.search_whitelist.contains("wikipedia.org"));
        assert!(cfg.search_api_key.is_none());
    }

    #[test]
    fn seed_settings_row_maps_defaults() {
        let cfg: AppConfig = toml::from_str(MINIMAL_TOML).expect("parse minimal config");
        let row = cfg.seed_settings_row();
        assert_eq!(row.keep_alive, "30m");
        assert_eq!(row.ollama_timeout_secs, 300);
        assert_eq!(row.repeat_penalty, 1.3);
        assert_eq!(row.repeat_last_n, 256);
        assert_eq!(row.num_predict, 512);
        assert_eq!(row.num_ctx, 8192);
        assert_eq!(row.default_temperature, 0.7);
        assert_eq!(row.default_top_p, 0.9);
        assert!(row.summary_enabled);
        assert_eq!(row.summary_interval_hours, 6);
    }
}
