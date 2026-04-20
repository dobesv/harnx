//! Retry configuration used by the client layer's retry/fallback logic.
//! Pure serde data; no runtime references.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

fn default_retry_attempts() -> u32 {
    3
}
fn default_initial_delay_ms() -> u64 {
    1000
}
fn default_max_delay_ms() -> u64 {
    60000
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RetryConfig {
    #[serde(default = "default_retry_attempts")]
    pub attempts: u32,
    #[serde(default = "default_initial_delay_ms")]
    pub initial_delay_ms: u64,
    #[serde(default = "default_max_delay_ms")]
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            attempts: default_retry_attempts(),
            initial_delay_ms: default_initial_delay_ms(),
            max_delay_ms: default_max_delay_ms(),
        }
    }
}

/// A far-future instant used as "infinite" cooldown (~100 years from process start).
static INFINITE_INSTANT: LazyLock<Instant> =
    LazyLock::new(|| Instant::now() + Duration::from_secs(100 * 365 * 24 * 3600));

/// Tracks per-model cooldowns for the retry/fallback orchestration.
#[derive(Debug, Clone, Default)]
pub struct ModelCooldownMap {
    cooldowns: HashMap<String, Instant>,
}

impl ModelCooldownMap {
    pub fn is_on_cooldown(&self, model_id: &str) -> bool {
        self.cooldowns
            .get(model_id)
            .is_some_and(|expires| Instant::now() < *expires)
    }

    pub fn set_cooldown(&mut self, model_id: &str, duration: Duration) {
        let expires = Instant::now()
            .checked_add(duration)
            .unwrap_or(*INFINITE_INSTANT);
        self.cooldowns.insert(model_id.to_string(), expires);
    }

    pub fn set_infinite_cooldown(&mut self, model_id: &str) {
        self.cooldowns
            .insert(model_id.to_string(), *INFINITE_INSTANT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cooldown_map_basic() {
        let mut map = ModelCooldownMap::default();
        assert!(!map.is_on_cooldown("model-a"));

        map.set_cooldown("model-a", Duration::from_secs(60));
        assert!(map.is_on_cooldown("model-a"));
        assert!(!map.is_on_cooldown("model-b"));
    }

    #[test]
    fn test_cooldown_map_expired() {
        let mut map = ModelCooldownMap::default();
        // Set cooldown that has already expired (0 duration)
        map.cooldowns.insert(
            "model-a".to_string(),
            Instant::now() - Duration::from_secs(1),
        );
        assert!(!map.is_on_cooldown("model-a"));
    }

    #[test]
    fn test_infinite_cooldown() {
        let mut map = ModelCooldownMap::default();
        map.set_infinite_cooldown("model-a");
        assert!(map.is_on_cooldown("model-a"));
    }
}
