//! GitHub REST records, reduced to the fields Workshop reads.

use crate::ledger::LedgerFile;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub login: String,
    #[serde(rename = "type", default)]
    pub kind: String,
}

impl User {
    pub fn is_bot(&self) -> bool {
        self.kind == "Bot" || self.login.ends_with("[bot]")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Issue {
    pub number: u64,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub assignees: Vec<User>,
    #[serde(default)]
    pub pull_request: Option<Value>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

impl Issue {
    pub fn has_label(&self, name: &str) -> bool {
        self.labels.iter().any(|label| label.name == name)
    }

    pub fn is_pull_request(&self) -> bool {
        self.pull_request.is_some()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Pull {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub user: User,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
}

/// A merged pull request as the search API returns it: issue-shaped, with the merge time nested.
#[derive(Debug, Clone, Deserialize)]
pub struct MergedPull {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub user: User,
    #[serde(default)]
    pub pull_request: Option<MergeFacts>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MergeFacts {
    #[serde(default)]
    pub merged_at: Option<DateTime<Utc>>,
}

impl MergedPull {
    pub fn merged_at(&self) -> Option<DateTime<Utc>> {
        self.pull_request.as_ref().and_then(|facts| facts.merged_at)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Comment {
    pub user: User,
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Review {
    pub user: User,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub submitted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    pub event: String,
    #[serde(default)]
    pub actor: Option<User>,
    #[serde(default)]
    pub label: Option<Label>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TreeEntry {
    pub path: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub sha: String,
}

/// Everything one refresh read from GitHub. A missing map entry is a fact Workshop does not know.
#[derive(Debug, Clone, Default)]
pub struct Raw {
    pub ledger: Vec<LedgerFile>,
    /// Ledger files whose raw copy did not yet match the tree, so they are left out this time.
    pub ledger_waiting: usize,
    pub gap_issues: Vec<Issue>,
    pub closed_gap_issues: Vec<Issue>,
    /// Closed claim issues whose events Workshop has not read yet, so it cannot trust them.
    pub closed_unchecked: usize,
    pub authenticated: bool,
    pub claim_events: HashMap<u64, Vec<Event>>,
    pub open_pulls: Vec<Pull>,
    pub merged_pulls: Vec<MergedPull>,
    pub comments: HashMap<u64, Vec<Comment>>,
    pub reviews: HashMap<u64, Vec<Review>>,
    pub referenced_issues: HashMap<u64, Issue>,
}

/// Parses each record on its own, so one record in an unexpected shape drops only itself.
pub fn records<T: DeserializeOwned>(values: Vec<Value>) -> Vec<T> {
    values
        .into_iter()
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_record_in_an_unexpected_shape_drops_only_itself() {
        let issues: Vec<Issue> = records(vec![
            json!({"number": "seven", "title": "bad"}),
            json!({"number": 7, "title": "good", "labels": [{"name": "P2"}]}),
            json!(null),
        ]);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].has_label("P2"));
        assert_eq!(issues[0].body, None);
    }

    #[test]
    fn a_login_ending_in_bot_is_a_bot_even_without_a_type() {
        let user: User = serde_json::from_value(json!({"login": "renovate[bot]"})).unwrap();
        assert!(user.is_bot());
    }
}
