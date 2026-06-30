use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointRecord {
    pub id: String,
    pub branch: String,
    pub source_rev: Option<String>,
    pub created_at: DateTime<Utc>,
    pub message: Option<String>,
    #[serde(default)]
    pub tracker_states: Vec<TrackerStateRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrackerStateRecord {
    pub name: String,
    pub definition_rev: String,
    pub state_ref: Option<String>,
}
