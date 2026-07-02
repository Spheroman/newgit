use std::collections::BTreeMap;

use camino::Utf8PathBuf;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{NewgitError, Result};

/// The binding record. The workspace directory is disposable; this is not.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BranchInstance {
    pub id: String,
    pub name: String,
    pub slug: String,
    /// Git branch in the store repo (also the branch checked out in the clone).
    pub source_ref: String,
    /// Revision the workspace was materialized at.
    pub source_rev: String,
    pub workspace_path: Utf8PathBuf,
    pub status: InstanceStatus,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub trackers: BTreeMap<String, TrackerBinding>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resources: BTreeMap<String, ResourceBinding>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Which concrete instance of a resource this branch instance is bound to:
/// its allocated ports and rendered exports.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceBinding {
    pub definition_rev: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resolved_ports: BTreeMap<String, u16>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resolved_exports: BTreeMap<String, String>,
    pub status: ResourceStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ResourceStatus {
    /// Bound; prepare has not succeeded yet.
    Pending,
    Ready,
    Failed,
    /// Not attempted because a resource dependency is failed or blocked.
    Blocked,
}

/// Which content revision of a tracker this instance is bound to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrackerBinding {
    pub definition_rev: String,
    /// None when the tracker is bound but no content has been captured yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_rev: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum InstanceStatus {
    Active,
}

impl BranchInstance {
    pub fn new(
        name: &str,
        source_ref: impl Into<String>,
        source_rev: impl Into<String>,
        workspace_path: Utf8PathBuf,
    ) -> Result<Self> {
        validate_name(name)?;
        let slug = branch_slug(name);
        let now = Utc::now();
        Ok(Self {
            id: format!("br_{slug}_{}", now.timestamp_millis()),
            name: name.to_owned(),
            slug,
            source_ref: source_ref.into(),
            source_rev: source_rev.into(),
            workspace_path,
            status: InstanceStatus::Active,
            trackers: BTreeMap::new(),
            resources: BTreeMap::new(),
            created_at: now,
            updated_at: now,
        })
    }

    pub fn short_rev(&self) -> &str {
        self.source_rev.get(..8).unwrap_or(&self.source_rev)
    }
}

/// Filesystem-safe identifier derived from the instance name; used for the
/// workspace directory and the record filename.
pub fn branch_slug(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut previous_dash = false;

    for ch in name.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            previous_dash = false;
        } else if !previous_dash {
            slug.push('-');
            previous_dash = true;
        }
    }

    slug.trim_matches('-').to_owned()
}

/// Names double as Git branch names, so stay well inside ref-name rules.
pub fn validate_name(name: &str) -> Result<()> {
    let starts_ok = name
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphanumeric());
    let chars_ok = name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '/'));
    let refname_ok = !name.contains("..") && !name.ends_with('/') && !name.ends_with(".lock");

    if starts_ok && chars_ok && refname_ok {
        Ok(())
    } else {
        Err(NewgitError::InvalidName(name.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::{branch_slug, validate_name};

    #[test]
    fn slug_flattens_and_lowercases() {
        assert_eq!(branch_slug("Feature/A_b"), "feature-a-b");
        assert_eq!(branch_slug("auth-refactor"), "auth-refactor");
    }

    #[test]
    fn names_stay_inside_git_ref_rules() {
        assert!(validate_name("feature/login").is_ok());
        assert!(validate_name("-flag").is_err());
        assert!(validate_name("a..b").is_err());
        assert!(validate_name("a.lock").is_err());
        assert!(validate_name("").is_err());
    }
}
