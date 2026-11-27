//! The snapshot: what Workshop knows, computed from raw GitHub records without I/O.

use crate::config::Config;
use crate::raw::{Comment, Event, Issue, Raw, Review, User};
use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::LazyLock;

pub const SCHEMA_VERSION: u32 = 1;
pub const READY: &str = "ready-for-agent";
pub const IN_PROGRESS: &str = "in progress";
pub const CLAIM_BOT: &str = "github-actions[bot]";
pub const CHECK_MARKER: &str = "<!-- lyracore-contribution-check -->";
const CHECK_HEADING: &str = "### Contribution check: ";
const CLAIM_LIFETIME_HOURS: i64 = 24;
const FIX_POINTS: u32 = 10;
const FAST_LANE_POINTS: u32 = 3;
const REVIEW_POINTS: u32 = 2;

// GitHub's closing keywords, as LyraCore's claim workflow reads them.
static CLOSING_REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\b:?[ \t]+#(\d+)\b").unwrap()
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Lane {
    #[serde(rename = "fast lane")]
    Fast,
    #[serde(rename = "slow lane")]
    Slow,
    #[serde(rename = "failed")]
    Failed,
}

impl Lane {
    pub fn name(self) -> &'static str {
        match self {
            Lane::Fast => "fast lane",
            Lane::Slow => "slow lane",
            Lane::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub schema_version: u32,
    pub repository: String,
    pub built_at: DateTime<Utc>,
    pub tasks: Vec<Task>,
    pub open_pulls: Vec<OpenPull>,
    pub landed_fixes: Vec<LandedFix>,
    pub leaderboard: Vec<Standing>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub body: String,
    pub priority: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub claim: Option<Claim>,
    pub pull_requests: Vec<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Claim {
    pub holders: Vec<String>,
    pub since: Option<DateTime<Utc>>,
    /// Set only for a claim the workflow made and no open pull request covers yet.
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenPull {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
    pub outside: bool,
    pub draft: bool,
    pub created_at: Option<DateTime<Utc>>,
    pub closes_tasks: Vec<u64>,
    pub lane: Option<Lane>,
    pub reviews: Vec<ReviewEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewEntry {
    pub reviewer: String,
    pub state: String,
    pub submitted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LandedFix {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
    pub merged_at: Option<DateTime<Utc>>,
    pub tasks: Vec<u64>,
    pub lane: Option<Lane>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Standing {
    pub login: String,
    pub score: u32,
    pub fixes: Vec<PullRef>,
    pub reviews: Vec<PullRef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PullRef {
    pub number: u64,
    pub title: String,
    pub url: String,
}

pub fn closing_numbers(body: &str) -> Vec<u64> {
    let numbers: BTreeSet<u64> = CLOSING_REFERENCE
        .captures_iter(body)
        .filter_map(|captures| captures[1].parse().ok())
        .collect();
    numbers.into_iter().collect()
}

pub fn is_outside(user: &User, config: &Config) -> bool {
    !user.is_bot() && !config.is_maintainer(&user.login)
}

pub fn is_claimed(issue: &Issue) -> bool {
    issue.has_label(IN_PROGRESS) && !issue.assignees.is_empty()
}

/// The lane from the check's own comment. Anyone can paste the marker, so only the bot's counts.
pub fn lane(comments: &[Comment]) -> Option<Lane> {
    let body = comments
        .iter()
        .rev()
        .filter(|comment| comment.user.login == CLAIM_BOT)
        .filter_map(|comment| comment.body.as_deref())
        .find(|body| body.contains(CHECK_MARKER))?;
    let heading = body
        .lines()
        .find_map(|line| line.trim().strip_prefix(CHECK_HEADING))?;
    match heading.trim() {
        "fast lane" => Some(Lane::Fast),
        "slow lane" => Some(Lane::Slow),
        "failed" => Some(Lane::Failed),
        _ => None,
    }
}

/// Reviews that count: submitted, with a body, by neither the author nor a bot.
fn counted_reviews(reviews: &[Review], author: &str) -> Vec<ReviewEntry> {
    reviews
        .iter()
        .filter(|review| !review.state.is_empty() && review.state != "PENDING")
        .filter(|review| {
            review
                .body
                .as_deref()
                .is_some_and(|body| !body.trim().is_empty())
        })
        .filter(|review| !review.user.is_bot() && !review.user.login.eq_ignore_ascii_case(author))
        .map(|review| ReviewEntry {
            reviewer: review.user.login.clone(),
            state: review.state.clone(),
            submitted_at: review.submitted_at,
        })
        .collect()
}

fn priority(issue: &Issue) -> Option<String> {
    issue
        .labels
        .iter()
        .map(|label| label.name.as_str())
        .find(|name| matches!(*name, "P1" | "P2" | "P3" | "P4"))
        .map(str::to_owned)
}

fn priority_rank(priority: &Option<String>) -> u8 {
    match priority.as_deref() {
        Some("P1") => 1,
        Some("P2") => 2,
        Some("P3") => 3,
        Some("P4") => 4,
        _ => 5,
    }
}

fn claim(issue: &Issue, events: Option<&Vec<Event>>, covered: bool) -> Option<Claim> {
    if !is_claimed(issue) {
        return None;
    }
    let labelled = events.and_then(|events| {
        events.iter().rev().find(|event| {
            event.event == "labeled"
                && event
                    .label
                    .as_ref()
                    .is_some_and(|label| label.name == IN_PROGRESS)
        })
    });
    let since = labelled.map(|event| event.created_at);
    let by_workflow = labelled
        .and_then(|event| event.actor.as_ref())
        .is_some_and(|actor| actor.login == CLAIM_BOT);
    Some(Claim {
        holders: issue
            .assignees
            .iter()
            .map(|user| user.login.clone())
            .collect(),
        since,
        expires_at: since
            .filter(|_| by_workflow && !covered)
            .map(|since| since + Duration::hours(CLAIM_LIFETIME_HOURS)),
    })
}

pub fn issue_url(repository: &str, number: u64) -> String {
    format!("https://github.com/{repository}/issues/{number}")
}

pub fn pull_url(repository: &str, number: u64) -> String {
    format!("https://github.com/{repository}/pull/{number}")
}

pub fn build(raw: &Raw, config: &Config, now: DateTime<Utc>) -> Snapshot {
    let repository = config.repository.as_str();
    let tasks_raw: Vec<&Issue> = raw
        .tasks
        .iter()
        .filter(|issue| !issue.is_pull_request() && issue.has_label(READY))
        .collect();
    let task_numbers: HashSet<u64> = tasks_raw.iter().map(|issue| issue.number).collect();

    let mut open_pulls: Vec<OpenPull> = raw
        .open_pulls
        .iter()
        .filter_map(|pull| {
            let closes_tasks: Vec<u64> = closing_numbers(pull.body.as_deref().unwrap_or(""))
                .into_iter()
                .filter(|number| task_numbers.contains(number))
                .collect();
            let outside = is_outside(&pull.user, config);
            (outside || !closes_tasks.is_empty()).then(|| OpenPull {
                number: pull.number,
                title: pull.title.clone(),
                url: pull_url(repository, pull.number),
                author: pull.user.login.clone(),
                outside,
                draft: pull.draft,
                created_at: pull.created_at,
                closes_tasks,
                lane: raw.comments.get(&pull.number).and_then(|c| lane(c)),
                reviews: raw
                    .reviews
                    .get(&pull.number)
                    .map(|reviews| counted_reviews(reviews, &pull.user.login))
                    .unwrap_or_default(),
            })
        })
        .collect();
    open_pulls.sort_by_key(|pull| (pull.created_at.is_none(), pull.created_at, pull.number));

    let mut tasks: Vec<Task> = tasks_raw
        .iter()
        .map(|issue| {
            let pull_requests: Vec<u64> = open_pulls
                .iter()
                .filter(|pull| pull.closes_tasks.contains(&issue.number))
                .map(|pull| pull.number)
                .collect();
            Task {
                number: issue.number,
                title: issue.title.clone(),
                url: issue_url(repository, issue.number),
                body: issue.body.clone().unwrap_or_default(),
                priority: priority(issue),
                created_at: issue.created_at,
                claim: claim(
                    issue,
                    raw.claim_events.get(&issue.number),
                    !pull_requests.is_empty(),
                ),
                pull_requests,
            }
        })
        .collect();
    tasks.sort_by_key(|task| (priority_rank(&task.priority), task.number));

    let mut landed_fixes = Vec::new();
    let mut fix_points: BTreeMap<String, (u32, Vec<PullRef>)> = BTreeMap::new();
    let mut reviewed: BTreeMap<String, Vec<PullRef>> = BTreeMap::new();
    for pull in raw
        .merged_pulls
        .iter()
        .filter(|pull| pull.merged_at().is_some() && is_outside(&pull.user, config))
    {
        let pull_ref = PullRef {
            number: pull.number,
            title: pull.title.clone(),
            url: pull_url(repository, pull.number),
        };
        let tasks: Vec<u64> = closing_numbers(pull.body.as_deref().unwrap_or(""))
            .into_iter()
            .filter(|number| {
                raw.referenced_issues
                    .get(number)
                    .is_some_and(|issue| !issue.is_pull_request() && issue.has_label(READY))
            })
            .collect();
        if !tasks.is_empty() {
            let lane = raw.comments.get(&pull.number).and_then(|c| lane(c));
            let entry = fix_points.entry(pull.user.login.clone()).or_default();
            entry.0 += FIX_POINTS
                + if lane == Some(Lane::Fast) {
                    FAST_LANE_POINTS
                } else {
                    0
                };
            entry.1.push(pull_ref.clone());
            landed_fixes.push(LandedFix {
                number: pull.number,
                title: pull.title.clone(),
                url: pull_ref.url.clone(),
                author: pull.user.login.clone(),
                merged_at: pull.merged_at(),
                tasks,
                lane,
            });
        }
        let reviewers: BTreeSet<String> = raw
            .reviews
            .get(&pull.number)
            .map(|reviews| counted_reviews(reviews, &pull.user.login))
            .unwrap_or_default()
            .into_iter()
            .map(|review| review.reviewer)
            .filter(|login| !config.is_maintainer(login))
            .collect();
        for reviewer in reviewers {
            reviewed.entry(reviewer).or_default().push(pull_ref.clone());
        }
    }
    landed_fixes.sort_by(|a, b| b.merged_at.cmp(&a.merged_at).then(b.number.cmp(&a.number)));

    let logins: BTreeSet<&String> = fix_points.keys().chain(reviewed.keys()).collect();
    let mut leaderboard: Vec<Standing> = logins
        .into_iter()
        .map(|login| {
            let (points, fixes) = fix_points.get(login).cloned().unwrap_or_default();
            let reviews = reviewed.get(login).cloned().unwrap_or_default();
            Standing {
                login: login.clone(),
                score: points + REVIEW_POINTS * reviews.len() as u32,
                fixes,
                reviews,
            }
        })
        .collect();
    leaderboard.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.login.to_lowercase().cmp(&b.login.to_lowercase()))
    });

    Snapshot {
        schema_version: SCHEMA_VERSION,
        repository: config.repository.clone(),
        built_at: now,
        tasks,
        open_pulls,
        landed_fixes,
        leaderboard,
    }
}

impl Snapshot {
    pub fn tasks(&self, unclaimed_only: bool) -> impl Iterator<Item = &Task> {
        self.tasks
            .iter()
            .filter(move |task| !unclaimed_only || task.claim.is_none())
    }

    pub fn task(&self, number: u64) -> Option<&Task> {
        self.tasks.iter().find(|task| task.number == number)
    }

    pub fn pull(&self, number: u64) -> Option<&OpenPull> {
        self.open_pulls.iter().find(|pull| pull.number == number)
    }

    pub fn pulls_for<'a>(&'a self, task: &'a Task) -> impl Iterator<Item = &'a OpenPull> {
        task.pull_requests
            .iter()
            .filter_map(|number| self.pull(*number))
    }

    /// Open outside pull requests, oldest first.
    pub fn outside_pulls(&self) -> impl Iterator<Item = &OpenPull> {
        self.open_pulls.iter().filter(|pull| pull.outside)
    }

    /// Open outside pull requests the login did not write, fewest reviews first.
    pub fn reviews_wanted(&self, login: &str) -> Vec<&OpenPull> {
        let mut pulls: Vec<&OpenPull> = self
            .outside_pulls()
            .filter(|pull| !pull.author.eq_ignore_ascii_case(login))
            .collect();
        pulls.sort_by_key(|pull| pull.reviews.len());
        pulls
    }

    pub fn claims_of(&self, login: &str) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|task| {
                task.claim.as_ref().is_some_and(|claim| {
                    claim
                        .holders
                        .iter()
                        .any(|holder| holder.eq_ignore_ascii_case(login))
                })
            })
            .collect()
    }

    pub fn pulls_of(&self, login: &str) -> Vec<&OpenPull> {
        self.open_pulls
            .iter()
            .filter(|pull| pull.author.eq_ignore_ascii_case(login))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_config;
    use crate::raw::records;
    use serde_json::{Value, json};

    fn fixture(name: &str) -> Vec<Value> {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        match serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap() {
            Value::Array(values) => values,
            Value::Object(mut object) if object.contains_key("items") => {
                match object.remove("items").unwrap() {
                    Value::Array(values) => values,
                    _ => panic!("{path}: items is not an array"),
                }
            }
            value => vec![value],
        }
    }

    fn now() -> DateTime<Utc> {
        "2026-11-27T20:00:00Z".parse().unwrap()
    }

    #[test]
    fn closing_numbers_read_every_keyword_form_and_nothing_else() {
        let body = "Closes #12.\nfixes #3, Resolved: #40\nsee #99, refs #5, closes#6, prefixes #7";
        assert_eq!(closing_numbers(body), vec![3, 12, 40]);
    }

    #[test]
    fn the_real_contribution_check_comment_reads_as_failed() {
        let comments: Vec<Comment> = records(fixture("real/comments_1396.json"));
        assert_eq!(lane(&comments), Some(Lane::Failed));
    }

    #[test]
    fn a_marker_pasted_by_a_person_is_not_a_lane() {
        let comments: Vec<Comment> = records(vec![json!({
            "user": {"login": "mallory", "type": "User"},
            "body": "<!-- lyracore-contribution-check -->\n### Contribution check: fast lane"
        })]);
        assert_eq!(lane(&comments), None);
    }

    #[test]
    fn an_unknown_heading_leaves_the_lane_empty() {
        let comments: Vec<Comment> = records(vec![json!({
            "user": {"login": "github-actions[bot]", "type": "Bot"},
            "body": "<!-- lyracore-contribution-check -->\n### Contribution check: express lane"
        })]);
        assert_eq!(lane(&comments), None);
    }

    #[test]
    fn a_claim_starts_when_the_in_progress_label_was_added() {
        let mut issue: Issue = records(fixture("real/issue_669.json")).remove(0);
        issue.labels.push(crate::raw::Label {
            name: IN_PROGRESS.into(),
        });
        issue.assignees = records(vec![json!({"login": "Mathih13", "type": "User"})]);
        let events: Vec<Event> = records(fixture("real/events_1385.json"));
        let claim = claim(&issue, Some(&events), false).unwrap();
        assert_eq!(claim.since, Some("2026-11-27T15:02:54Z".parse().unwrap()));
        assert_eq!(claim.expires_at, None, "a maintainer's label never expires");
        let unknown = super::claim(&issue, None, false).unwrap();
        assert_eq!((unknown.since, unknown.expires_at), (None, None));
    }

    #[test]
    fn the_real_repository_builds_three_open_tasks_and_one_landed_fix() {
        let raw = Raw {
            tasks: records(fixture("real/issues_ready.json")),
            merged_pulls: records(fixture("real/search_merged.json")),
            comments: [(683, records(fixture("real/comments_683.json")))].into(),
            reviews: [
                (683, records(fixture("real/reviews_683.json"))),
                (660, records(fixture("real/reviews_660.json"))),
            ]
            .into(),
            referenced_issues: [
                (669, records(fixture("real/issue_669.json")).remove(0)),
                (659, records(fixture("real/issue_659.json")).remove(0)),
            ]
            .into(),
            ..Raw::default()
        };
        let snapshot = build(&raw, &test_config(), now());
        let order: Vec<(u64, Option<&str>)> = snapshot
            .tasks
            .iter()
            .map(|task| (task.number, task.priority.as_deref()))
            .collect();
        assert_eq!(
            order,
            vec![(105, Some("P2")), (36, Some("P3")), (1320, Some("P3"))]
        );
        assert!(snapshot.tasks.iter().all(|task| task.claim.is_none()));
        assert_eq!(snapshot.landed_fixes.len(), 1);
        assert_eq!(snapshot.landed_fixes[0].number, 683);
        assert_eq!(snapshot.landed_fixes[0].tasks, vec![669]);
        assert_eq!(snapshot.landed_fixes[0].lane, None);
        assert_eq!(snapshot.leaderboard.len(), 1);
        assert_eq!(snapshot.leaderboard[0].login, "suprsokr");
        assert_eq!(snapshot.leaderboard[0].score, 10);
    }

    #[test]
    fn a_maintainer_pull_request_is_kept_only_when_it_closes_a_task() {
        let raw = Raw {
            tasks: records(fixture("real/issues_ready.json")),
            open_pulls: records(vec![
                json!({"number": 1, "title": "t", "body": "Closes #105", "user": {"login": "Mathih13", "type": "User"}}),
                json!({"number": 2, "title": "t", "body": "Closes #105", "user": {"login": "Mathih13", "type": "User"}}),
                json!({"number": 3, "title": "t", "body": "Closes #999", "user": {"login": "MATHIH13", "type": "User"}}),
            ]),
            ..Raw::default()
        };
        let snapshot = build(&raw, &test_config(), now());
        let numbers: Vec<u64> = snapshot.open_pulls.iter().map(|pull| pull.number).collect();
        assert_eq!(numbers, vec![1, 2]);
        assert_eq!(snapshot.task(105).unwrap().pull_requests, vec![1, 2]);
        assert!(snapshot.outside_pulls().next().is_none());
    }
}
