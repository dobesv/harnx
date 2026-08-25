use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionActivity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_activation_at: Option<DateTime<Utc>>,
    pub last_activity_at: DateTime<Utc>,
}

impl SessionActivity {
    pub fn reserved() -> Self {
        Self {
            first_activation_at: None,
            last_activity_at: Utc::now(),
        }
    }

    pub(super) fn activated(previous: Option<Self>) -> Self {
        let now = Utc::now();
        Self {
            first_activation_at: previous
                .and_then(|activity| activity.first_activation_at)
                .or(Some(now)),
            last_activity_at: now,
        }
    }
}
