//! Personality definitions loaded from `*.toml` files in the personalities
//! directory. The file stem is the personality's id; a `default` personality
//! is mandatory. The loaded set is swapped atomically on reload so readers
//! always see a complete, consistent map.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, Deserialize)]
pub struct ProactiveConfig {
    pub relevance_threshold: f32,
    pub cooldown_secs: u64,
    pub max_per_hour: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Personality {
    #[serde(skip)] pub name: String,
    pub label: String,
    #[serde(default)] pub description: Option<String>,
    pub system_prompt: String,
    #[serde(default = "default_model")] pub model: String,
    /// Persona sampling temperature. Used when set (unless
    /// `temperature_override` is also set); `None` falls back to the global
    /// setting. See `settings::resolve`.
    #[serde(default)] pub temperature: Option<f32>,
    /// Persona top-p; same precedence as `temperature`.
    #[serde(default)] pub top_p: Option<f32>,
    /// Legacy context size, retained for back-compat. NOT consulted for
    /// generation — `settings::resolve` reads `num_ctx_override` (falling back to
    /// the global setting) instead. See `num_ctx_override`.
    #[serde(default = "def_ctx")] pub num_ctx: u32,
    pub proactive: ProactiveConfig,
    #[serde(default)] pub num_predict: Option<i64>,
    /// The per-personality context-size override actually used for generation
    /// (via `settings::resolve`); `None` falls back to the global `num_ctx`.
    #[serde(default)] pub num_ctx_override: Option<u32>,
    #[serde(default)] pub temperature_override: Option<f32>,
    #[serde(default)] pub top_p_override: Option<f32>,
    #[serde(default)] pub repeat_penalty: Option<f32>,
    /// Extra web-search domains available to THIS personality only, merged on
    /// top of the global whitelist for its turns. Lets a persona (e.g. boomer)
    /// reach its own sources without polluting the shared whitelist.
    #[serde(default)] pub extra_search_domains: Vec<String>,
}
fn default_model() -> String { "qwen3:8b".into() }
fn def_ctx() -> u32 { 8192 }

pub struct Personalities { inner: RwLock<Arc<HashMap<String, Arc<Personality>>>> }

impl Personalities {
    pub fn load_dir(dir: &Path) -> anyhow::Result<Personalities> {
        let map = Self::read_map(dir)?;
        if !map.contains_key("default") {
            anyhow::bail!("required personality 'default' not found in {}", dir.display());
        }
        Ok(Personalities { inner: RwLock::new(Arc::new(map)) })
    }

    fn read_map(dir: &Path) -> anyhow::Result<HashMap<String, Arc<Personality>>> {
        let mut map = HashMap::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") { continue; }
            let name = path.file_stem().unwrap().to_string_lossy().to_string();
            let text = std::fs::read_to_string(&path)?;
            match toml::from_str::<Personality>(&text) {
                Ok(mut p) => { p.name = name.clone(); map.insert(name, Arc::new(p)); }
                Err(e) => tracing::warn!("skipping personality {}: {e}", path.display()),
            }
        }
        Ok(map)
    }

    pub fn reload(&self, dir: &Path) -> anyhow::Result<()> {
        let map = Self::read_map(dir)?;
        if !map.contains_key("default") { anyhow::bail!("reload aborted: 'default' missing"); }
        *self.inner.write().unwrap() = Arc::new(map);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Arc<Personality>> {
        self.inner.read().unwrap().get(name).cloned()
    }
    pub fn get_or_default(&self, name: Option<&str>) -> Arc<Personality> {
        let guard = self.inner.read().unwrap();
        name.and_then(|n| guard.get(n)).or_else(|| guard.get("default")).cloned().unwrap()
    }
    pub fn list(&self) -> Vec<Arc<Personality>> {
        let mut v: Vec<_> = self.inner.read().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &std::path::Path, name: &str, body: &str) {
        let mut f = std::fs::File::create(dir.join(name)).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    const OK: &str = r#"label="X"
system_prompt="hi"
model="qwen3:8b"
temperature=0.5
top_p=0.9
num_ctx=4096
[proactive]
relevance_threshold=0.7
cooldown_secs=60
max_per_hour=5
"#;

    #[test]
    fn loads_and_requires_default() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "default.toml", OK);
        write(d.path(), "sage.toml", OK);
        let p = Personalities::load_dir(d.path()).unwrap();
        assert_eq!(p.list().len(), 2);
        assert_eq!(p.get("sage").unwrap().name, "sage");
        // unknown falls back to default
        assert_eq!(p.get_or_default(Some("nope")).name, "default");
    }

    #[test]
    fn missing_default_is_error() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "sage.toml", OK);
        assert!(Personalities::load_dir(d.path()).is_err());
    }

    #[test]
    fn bad_file_is_skipped_not_fatal() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "default.toml", OK);
        write(d.path(), "broken.toml", "not valid = = toml");
        let p = Personalities::load_dir(d.path()).unwrap();
        assert!(p.get("broken").is_none());
        assert!(p.get("default").is_some());
    }

    #[test]
    fn personality_optional_overrides_parse() {
        let d = tempfile::tempdir().unwrap();
        let body = "label=\"X\"\nsystem_prompt=\"hi\"\nmodel=\"qwen3:8b\"\ntemperature=0.5\ntop_p=0.9\nnum_ctx=8192\nnum_predict=800\nrepeat_penalty=1.5\n[proactive]\nrelevance_threshold=0.7\ncooldown_secs=60\nmax_per_hour=5\n";
        std::fs::write(d.path().join("default.toml"), body).unwrap();
        let p = Personalities::load_dir(d.path()).unwrap();
        let d0 = p.get("default").unwrap();
        assert_eq!(d0.num_predict, Some(800));
        assert_eq!(d0.repeat_penalty, Some(1.5));
    }
}
