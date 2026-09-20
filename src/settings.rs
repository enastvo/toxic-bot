use crate::personalities::Personality;
use crate::store::SettingsRow;

pub struct EffectiveParams {
    pub num_predict: i32,
    pub num_ctx: u32,
    pub temperature: f32,
    pub top_p: f32,
    pub repeat_penalty: f32,
    pub repeat_last_n: u32,
    pub keep_alive: String,
}

pub fn resolve(global: &SettingsRow, p: &Personality) -> EffectiveParams {
    EffectiveParams {
        num_predict: p.num_predict.unwrap_or(global.num_predict) as i32,
        num_ctx: p.num_ctx_override.unwrap_or(global.num_ctx as u32),
        temperature: p.temperature_override.unwrap_or(global.default_temperature as f32),
        top_p: p.top_p_override.unwrap_or(global.default_top_p as f32),
        repeat_penalty: p.repeat_penalty.unwrap_or(global.repeat_penalty as f32),
        repeat_last_n: global.repeat_last_n as u32,
        keep_alive: global.keep_alive.clone(),
    }
}

pub fn validate(row: &SettingsRow) -> Result<(), String> {
    if !(10..=600).contains(&row.ollama_timeout_secs) {
        return Err(format!(
            "ollama_timeout_secs must be between 10 and 600, got {}",
            row.ollama_timeout_secs
        ));
    }
    if !(16..=4096).contains(&row.num_predict) {
        return Err(format!("num_predict must be between 16 and 4096, got {}", row.num_predict));
    }
    if !(512..=32768).contains(&row.num_ctx) {
        return Err(format!("num_ctx must be between 512 and 32768, got {}", row.num_ctx));
    }
    if !(0.5..=2.0).contains(&row.repeat_penalty) {
        return Err(format!(
            "repeat_penalty must be between 0.5 and 2.0, got {}",
            row.repeat_penalty
        ));
    }
    if !(0..=2048).contains(&row.repeat_last_n) {
        return Err(format!(
            "repeat_last_n must be between 0 and 2048, got {}",
            row.repeat_last_n
        ));
    }
    if !(0.0..=2.0).contains(&row.default_temperature) {
        return Err(format!(
            "default_temperature must be between 0.0 and 2.0, got {}",
            row.default_temperature
        ));
    }
    if !(0.0..=1.0).contains(&row.default_top_p) {
        return Err(format!(
            "default_top_p must be between 0.0 and 1.0, got {}",
            row.default_top_p
        ));
    }
    if !(1..=168).contains(&row.summary_interval_hours) {
        return Err(format!(
            "summary_interval_hours must be between 1 and 168, got {}",
            row.summary_interval_hours
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::personalities::{Personality, ProactiveConfig};
    use crate::store::SettingsRow;

    fn global() -> SettingsRow {
        SettingsRow {
            keep_alive: "30m".into(),
            ollama_timeout_secs: 300,
            repeat_penalty: 1.3,
            repeat_last_n: 256,
            num_predict: 512,
            num_ctx: 8192,
            default_temperature: 0.7,
            default_top_p: 0.9,
            summary_enabled: true,
            summary_interval_hours: 6,
        }
    }

    fn pers(np: Option<i64>, rp: Option<f32>) -> Personality {
        Personality {
            name: "p".into(),
            label: "L".into(),
            description: None,
            system_prompt: "x".into(),
            model: "m".into(),
            temperature: 0.5,
            top_p: 0.9,
            num_ctx: 8192,
            proactive: ProactiveConfig { relevance_threshold: 0.5, cooldown_secs: 60, max_per_hour: 10 },
            num_predict: np,
            num_ctx_override: None,
            temperature_override: None,
            top_p_override: None,
            repeat_penalty: rp,
        }
    }

    #[test]
    fn override_beats_global() {
        let p = pers(Some(800), Some(1.5));
        let e = resolve(&global(), &p);
        assert_eq!(e.num_predict, 800);
        assert_eq!(e.repeat_penalty, 1.5);
        assert_eq!(e.num_ctx, 8192); // not overridden -> global
    }

    #[test]
    fn validate_rejects_out_of_range() {
        let mut g = global();
        g.num_predict = 99999;
        assert!(validate(&g).is_err());
        g.num_predict = 512;
        g.repeat_penalty = 5.0;
        assert!(validate(&g).is_err());
        assert!(validate(&global()).is_ok());
    }
}
