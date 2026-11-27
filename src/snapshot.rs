//! The snapshot: what Workshop knows, computed from raw GitHub records without I/O.

use crate::config::Config;
use crate::ledger::{self, Ledger};
use crate::raw::{Comment, Event, Issue, Raw, Review, User};
use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

pub const SCHEMA_VERSION: u32 = 2;
pub const GAP: &str = "gap";
/// The label tasks had before they came from the Ledger. Fixes that closed one still score.
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

static GAP_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^[ \t]*<!-- lyracore-gap: ([a-z0-9_]+-[0-9a-f]{8}) -->[ \t]*\r?$").unwrap()
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
    pub ledger: LedgerSummary,
    pub tasks: Vec<Task>,
    pub open_pulls: Vec<OpenPull>,
    pub landed_fixes: Vec<LandedFix>,
    pub leaderboard: Vec<Standing>,
    pub garden: Garden,
}

#[derive(Debug, Clone, Serialize)]
pub struct LedgerSummary {
    pub files: usize,
    pub unparsed: Vec<String>,
    /// Files left out because their raw copy did not yet match the tree.
    pub waiting: usize,
    pub gaps: usize,
    pub data_gaps: usize,
    pub needs_real_client: usize,
}

/// A known gap in the Feature Ledger that an outside agent can take.
#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub id: String,
    pub system: String,
    pub behavior: String,
    pub description: String,
    pub path: String,
    pub symbol: String,
    pub evidence: String,
    pub scenarios: Vec<String>,
    pub vmangos: Option<VmangosRef>,
    pub behavior_file: String,
    pub behavior_url: String,
    pub claim: Option<Claim>,
    /// Open pull requests that close the claim issue.
    pub pull_requests: Vec<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VmangosRef {
    pub commit: String,
    pub path: String,
    pub symbol: String,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Claim {
    pub holder: String,
    pub issue: u64,
    pub issue_url: String,
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
    /// Open claim issues this pull request closes.
    pub closes_claims: Vec<u64>,
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
    /// The `gap` or `ready-for-agent` issues it closed.
    pub issues: Vec<u64>,
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

/// What the maintainer gardens: per system, the tasks and the claims that ended without a fix.
#[derive(Debug, Clone, Serialize)]
pub struct Garden {
    pub systems: Vec<GardenSystem>,
    pub closed_claims_checked: usize,
    /// Closed claim issues not read yet. Without a token Workshop reads a few each refresh.
    pub closed_claims_unchecked: usize,
    pub authenticated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct GardenSystem {
    pub system: String,
    pub tasks: usize,
    pub claimed: usize,
    /// Tasks whose claim issue closed without a merged pull request, most often first.
    pub unfinished: Vec<Unfinished>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Unfinished {
    pub id: String,
    pub behavior: String,
    pub description: String,
    pub issues: Vec<u64>,
}

pub fn closing_numbers(body: &str) -> Vec<u64> {
    let numbers: BTreeSet<u64> = CLOSING_REFERENCE
        .captures_iter(body)
        .filter_map(|captures| captures[1].parse().ok())
        .collect();
    numbers.into_iter().collect()
}

/// The gap id in a claim issue's marker line.
pub fn gap_marker(body: &str) -> Option<&str> {
    GAP_MARKER
        .captures(body)
        .and_then(|captures| captures.get(1))
        .map(|id| id.as_str())
}

pub fn is_outside(user: &User, config: &Config) -> bool {
    !user.is_bot() && !config.is_maintainer(&user.login)
}

/// An issue that says it claims a gap. Its events decide whether to trust it.
pub fn is_claim_issue(issue: &Issue) -> bool {
    !issue.is_pull_request()
        && issue.has_label(GAP)
        && gap_marker(issue.body.as_deref().unwrap_or("")).is_some()
}

pub fn is_held(issue: &Issue) -> bool {
    is_claim_issue(issue) && issue.has_label(IN_PROGRESS) && !issue.assignees.is_empty()
}

fn last_labelled<'a>(events: &'a [Event], label: &str) -> Option<&'a Event> {
    events.iter().rev().find(|event| {
        event.event == "labeled" && event.label.as_ref().is_some_and(|l| l.name == label)
    })
}

/// An issue form can add `gap` for its author, so only the workflow's label makes a claim issue.
pub fn labelled_by_workflow(events: Option<&Vec<Event>>) -> bool {
    events
        .and_then(|events| last_labelled(events, GAP))
        .and_then(|event| event.actor.as_ref())
        .is_some_and(|actor| actor.login == CLAIM_BOT)
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

/// The claim a held claim issue makes. It starts when `in progress` was last added.
fn claim(repository: &str, issue: &Issue, events: Option<&Vec<Event>>, covered: bool) -> Claim {
    let labelled = events.and_then(|events| last_labelled(events, IN_PROGRESS));
    let since = labelled.map(|event| event.created_at);
    let by_workflow = labelled
        .and_then(|event| event.actor.as_ref())
        .is_some_and(|actor| actor.login == CLAIM_BOT);
    Claim {
        holder: issue.assignees[0].login.clone(),
        issue: issue.number,
        issue_url: issue_url(repository, issue.number),
        since,
        expires_at: since
            .filter(|_| by_workflow && !covered)
            .map(|since| since + Duration::hours(CLAIM_LIFETIME_HOURS)),
    }
}

pub fn issue_url(repository: &str, number: u64) -> String {
    format!("https://github.com/{repository}/issues/{number}")
}

pub fn pull_url(repository: &str, number: u64) -> String {
    format!("https://github.com/{repository}/pull/{number}")
}

fn vmangos_ref(reference: &ledger::VmangosRef) -> VmangosRef {
    let hex = |text: &str| !text.is_empty() && text.chars().all(|c| c.is_ascii_hexdigit());
    let url = (hex(&reference.commit) && !reference.path.is_empty()).then(|| {
        format!(
            "https://github.com/vmangos/core/blob/{}/{}",
            reference.commit, reference.path
        )
    });
    VmangosRef {
        commit: reference.commit.clone(),
        path: reference.path.clone(),
        symbol: reference.symbol.clone(),
        url,
    }
}

fn ledger_summary(ledger: &Ledger, waiting: usize) -> LedgerSummary {
    let gaps = ledger.behaviors.iter().flat_map(|b| &b.known_gaps);
    LedgerSummary {
        files: ledger.behaviors.len() + ledger.unparsed.len(),
        unparsed: ledger.unparsed.clone(),
        waiting,
        gaps: gaps.clone().count(),
        data_gaps: gaps.clone().filter(|gap| gap.is_data_gap()).count(),
        needs_real_client: gaps.filter(|gap| gap.needs_real_client).count(),
    }
}

/// The tasks in the Ledger, without claims. A repeated description in one Behavior is one task.
fn ledger_tasks(ledger: &Ledger, repository: &str) -> Vec<Task> {
    let mut seen = HashSet::new();
    let mut tasks = Vec::new();
    for behavior in &ledger.behaviors {
        for gap in behavior.known_gaps.iter().filter(|gap| gap.is_task()) {
            let id = ledger::gap_id(&behavior.id, &gap.description);
            if !seen.insert(id.clone()) {
                continue;
            }
            tasks.push(Task {
                id,
                system: behavior.system.clone(),
                behavior: behavior.id.clone(),
                description: gap.description.clone(),
                path: gap.path.clone(),
                symbol: gap.symbol.clone(),
                evidence: gap.evidence.clone(),
                scenarios: behavior.scenarios.clone(),
                vmangos: behavior.vmangos.as_ref().map(vmangos_ref),
                behavior_file: behavior.file.clone(),
                behavior_url: format!(
                    "https://github.com/{repository}/blob/{}/{}",
                    ledger::LEDGER_BRANCH,
                    behavior.file
                ),
                claim: None,
                pull_requests: Vec::new(),
            });
        }
    }
    tasks
}

pub fn build(raw: &Raw, config: &Config, now: DateTime<Utc>) -> Snapshot {
    let repository = config.repository.as_str();
    let ledger = ledger::parse(&raw.ledger);
    let mut tasks = ledger_tasks(&ledger, repository);

    // Every held claim issue the workflow made, oldest first, so the first claim on a gap wins.
    let mut held: Vec<&Issue> = raw
        .gap_issues
        .iter()
        .filter(|issue| is_held(issue) && labelled_by_workflow(raw.claim_events.get(&issue.number)))
        .collect();
    held.sort_by_key(|issue| issue.number);
    let held_numbers: HashSet<u64> = held.iter().map(|issue| issue.number).collect();

    let mut open_pulls: Vec<OpenPull> = raw
        .open_pulls
        .iter()
        .filter_map(|pull| {
            let closes_claims: Vec<u64> = closing_numbers(pull.body.as_deref().unwrap_or(""))
                .into_iter()
                .filter(|number| held_numbers.contains(number))
                .collect();
            let outside = is_outside(&pull.user, config);
            (outside || !closes_claims.is_empty()).then(|| OpenPull {
                number: pull.number,
                title: pull.title.clone(),
                url: pull_url(repository, pull.number),
                author: pull.user.login.clone(),
                outside,
                draft: pull.draft,
                created_at: pull.created_at,
                closes_claims,
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

    let task_index: HashMap<String, usize> = tasks
        .iter()
        .enumerate()
        .map(|(i, task)| (task.id.clone(), i))
        .collect();
    for issue in held {
        let id = gap_marker(issue.body.as_deref().unwrap_or("")).unwrap_or_default();
        let Some(&i) = task_index.get(id) else {
            continue;
        };
        if tasks[i].claim.is_some() {
            continue;
        }
        let pull_requests: Vec<u64> = open_pulls
            .iter()
            .filter(|pull| pull.closes_claims.contains(&issue.number))
            .map(|pull| pull.number)
            .collect();
        tasks[i].claim = Some(claim(
            repository,
            issue,
            raw.claim_events.get(&issue.number),
            !pull_requests.is_empty(),
        ));
        tasks[i].pull_requests = pull_requests;
    }
    tasks.sort_by(|a, b| {
        (
            a.claim.is_some(),
            a.scenarios.is_empty(),
            &a.system,
            &a.behavior,
            &a.id,
        )
            .cmp(&(
                b.claim.is_some(),
                b.scenarios.is_empty(),
                &b.system,
                &b.behavior,
                &b.id,
            ))
    });

    let mut landed_fixes = Vec::new();
    let mut fix_points: BTreeMap<String, (u32, Vec<PullRef>)> = BTreeMap::new();
    let mut reviewed: BTreeMap<String, Vec<PullRef>> = BTreeMap::new();
    let mut closed_by_merge: HashSet<u64> = HashSet::new();
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
        let closed = closing_numbers(pull.body.as_deref().unwrap_or(""));
        closed_by_merge.extend(&closed);
        let issues: Vec<u64> = closed
            .into_iter()
            .filter(|number| {
                raw.referenced_issues.get(number).is_some_and(|issue| {
                    !issue.is_pull_request() && (issue.has_label(GAP) || issue.has_label(READY))
                })
            })
            .collect();
        if !issues.is_empty() {
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
                issues,
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

    let garden = garden(raw, &tasks, &closed_by_merge);
    Snapshot {
        schema_version: SCHEMA_VERSION,
        repository: config.repository.clone(),
        built_at: now,
        ledger: ledger_summary(&ledger, raw.ledger_waiting),
        tasks,
        open_pulls,
        landed_fixes,
        leaderboard,
        garden,
    }
}

/// A claim issue that closed without a merged pull request was released or lapsed. A gap that
/// keeps ending that way probably wants splitting or rewording.
fn garden(raw: &Raw, tasks: &[Task], closed_by_merge: &HashSet<u64>) -> Garden {
    let mut unfinished: HashMap<&str, Vec<u64>> = HashMap::new();
    let mut checked = 0;
    for issue in raw.closed_gap_issues.iter().filter(|i| is_claim_issue(i)) {
        let Some(events) = raw.claim_events.get(&issue.number) else {
            continue;
        };
        checked += 1;
        if labelled_by_workflow(Some(events)) && !closed_by_merge.contains(&issue.number) {
            let id = gap_marker(issue.body.as_deref().unwrap_or("")).unwrap_or_default();
            unfinished.entry(id).or_default().push(issue.number);
        }
    }
    let mut systems: BTreeMap<&str, GardenSystem> = BTreeMap::new();
    for task in tasks {
        let system = systems
            .entry(task.system.as_str())
            .or_insert_with(|| GardenSystem {
                system: task.system.clone(),
                tasks: 0,
                claimed: 0,
                unfinished: Vec::new(),
            });
        system.tasks += 1;
        system.claimed += usize::from(task.claim.is_some());
        if let Some(issues) = unfinished.get(task.id.as_str()) {
            let mut issues = issues.clone();
            issues.sort_unstable();
            system.unfinished.push(Unfinished {
                id: task.id.clone(),
                behavior: task.behavior.clone(),
                description: task.description.clone(),
                issues,
            });
        }
    }
    let mut systems: Vec<GardenSystem> = systems.into_values().collect();
    for system in &mut systems {
        system
            .unfinished
            .sort_by(|a, b| b.issues.len().cmp(&a.issues.len()).then(a.id.cmp(&b.id)));
    }
    Garden {
        systems,
        closed_claims_checked: checked,
        closed_claims_unchecked: raw.closed_unchecked,
        authenticated: raw.authenticated,
    }
}

impl Snapshot {
    pub fn tasks(&self, unclaimed_only: bool) -> impl Iterator<Item = &Task> {
        self.tasks
            .iter()
            .filter(move |task| !unclaimed_only || task.claim.is_none())
    }

    pub fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|task| task.id == id)
    }

    pub fn pull(&self, number: u64) -> Option<&OpenPull> {
        self.open_pulls.iter().find(|pull| pull.number == number)
    }

    pub fn pulls_for<'a>(&'a self, task: &'a Task) -> impl Iterator<Item = &'a OpenPull> {
        task.pull_requests
            .iter()
            .filter_map(|number| self.pull(*number))
    }

    /// The tasks of each system, systems in name order and tasks in snapshot order.
    pub fn tasks_by_system(&self) -> Vec<(&str, Vec<&Task>)> {
        let mut systems: BTreeMap<&str, Vec<&Task>> = BTreeMap::new();
        for task in &self.tasks {
            systems.entry(task.system.as_str()).or_default().push(task);
        }
        systems.into_iter().collect()
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
                task.claim
                    .as_ref()
                    .is_some_and(|claim| claim.holder.eq_ignore_ascii_case(login))
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
    use crate::ledger::LedgerFile;
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

    fn ledger_file(path: &str) -> LedgerFile {
        let file = format!(
            "{}/tests/fixtures/ledger/{path}",
            env!("CARGO_MANIFEST_DIR")
        );
        LedgerFile {
            path: path.into(),
            text: std::fs::read_to_string(file).unwrap(),
        }
    }

    fn now() -> DateTime<Utc> {
        "2026-11-27T20:00:00Z".parse().unwrap()
    }

    fn workflow_label(label: &str, at: &str) -> Value {
        json!({"event": "labeled", "actor": {"login": "github-actions[bot]", "type": "Bot"},
               "label": {"name": label}, "created_at": at})
    }

    #[test]
    fn closing_numbers_read_every_keyword_form_and_nothing_else() {
        let body = "Closes #12.\nfixes #3, Resolved: #40\nsee #99, refs #5, closes#6, prefixes #7";
        assert_eq!(closing_numbers(body), vec![3, 12, 40]);
    }

    #[test]
    fn the_gap_marker_is_a_line_of_its_own() {
        assert_eq!(
            gap_marker("Claimed.\r\n<!-- lyracore-gap: pet_follow-d760a50f -->\r\nMore."),
            Some("pet_follow-d760a50f")
        );
        for body in [
            "see <!-- lyracore-gap: pet_follow-d760a50f --> inline",
            "<!-- lyracore-gap: pet_follow-D760A50F -->",
            "<!-- lyracore-gap: pet_follow -->",
            "<!-- lyracore-gap: Pet_follow-d760a50f -->",
        ] {
            assert_eq!(gap_marker(body), None, "{body}");
        }
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
    fn only_the_workflow_adding_the_gap_label_makes_a_claim_issue() {
        let mut events: Vec<Value> = fixture("real/events_1385.json");
        assert!(!labelled_by_workflow(Some(&records(events.clone()))));
        events.insert(0, workflow_label(GAP, "2026-11-27T15:00:00Z"));
        assert!(labelled_by_workflow(Some(&records(events.clone()))));
        events.push(
            json!({"event": "labeled", "actor": {"login": "Mathih13", "type": "User"},
                           "label": {"name": GAP}, "created_at": "2026-11-27T16:00:00Z"}),
        );
        assert!(
            !labelled_by_workflow(Some(&records(events))),
            "the last time the label was added decides"
        );
        assert!(!labelled_by_workflow(None));
    }

    #[test]
    fn a_claim_starts_when_the_in_progress_label_was_added() {
        let issue: Issue = records(vec![json!({
            "number": 7,
            "body": "<!-- lyracore-gap: pet_follow-d760a50f -->",
            "labels": [{"name": GAP}, {"name": IN_PROGRESS}],
            "assignees": [{"login": "Mathih13", "type": "User"}],
        })])
        .remove(0);
        assert!(is_held(&issue));
        let events: Vec<Event> = records(fixture("real/events_1385.json"));
        let claim = claim("o/r", &issue, Some(&events), false);
        assert_eq!(claim.since, Some("2026-11-27T15:02:54Z".parse().unwrap()));
        assert_eq!(claim.expires_at, None, "a maintainer's label never expires");
        let events: Vec<Event> = records(vec![workflow_label(IN_PROGRESS, "2026-11-27T15:00:00Z")]);
        let claim = super::claim("o/r", &issue, Some(&events), false);
        assert_eq!(
            claim.expires_at,
            Some("2026-11-28T15:00:00Z".parse().unwrap())
        );
        let unknown = super::claim("o/r", &issue, None, false);
        assert_eq!((unknown.since, unknown.expires_at), (None, None));
    }

    #[test]
    fn data_gaps_and_gaps_that_need_a_real_client_are_not_tasks() {
        let raw = Raw {
            ledger: vec![
                ledger_file("spells/cast_and_interrupt.toml"),
                ledger_file("creatures/creature_world_behavior.toml"),
                ledger_file("combat/broken.toml"),
            ],
            ledger_waiting: 2,
            ..Raw::default()
        };
        let snapshot = build(&raw, &test_config(), now());
        let ids: Vec<&str> = snapshot.tasks.iter().map(|task| task.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "cast_and_interrupt-4f97715e",
                "cast_and_interrupt-52f5323c",
                "creature_world_behavior-4b52062e",
                "creature_world_behavior-aaedc5f1",
            ]
        );
        let summary = &snapshot.ledger;
        assert_eq!(
            (
                summary.files,
                summary.gaps,
                summary.data_gaps,
                summary.needs_real_client
            ),
            (3, 7, 2, 1)
        );
        assert_eq!(summary.waiting, 2);
        assert_eq!(
            summary.unparsed,
            vec!["conformance/features/combat/broken.toml"]
        );
        let task = &snapshot.tasks[0];
        assert_eq!(task.system, "spells");
        assert_eq!(task.symbol, "finish_item_creation");
        assert_eq!(
            task.behavior_url,
            "https://github.com/LyraCoreProject/LyraCore/blob/main/conformance/features/spells/cast_and_interrupt.toml"
        );
        let vmangos = task.vmangos.as_ref().unwrap();
        assert_eq!(
            vmangos.url.as_deref(),
            Some(
                "https://github.com/vmangos/core/blob/d9173eca3ea08dd985c52dabbcc826898d1d0b77/src/game/Handlers/SpellHandler.cpp"
            )
        );
        assert!(
            snapshot
                .garden
                .systems
                .iter()
                .all(|s| s.unfinished.is_empty())
        );
    }

    #[test]
    fn a_repeated_description_in_one_behavior_is_one_task() {
        let text = "id = \"swing\"\nsystem = \"combat\"\n[[known_gaps]]\ndescription = \"d\"\n[[known_gaps]]\ndescription = \"d\"\n";
        let raw = Raw {
            ledger: vec![LedgerFile {
                path: "combat/swing.toml".into(),
                text: text.into(),
            }],
            ..Raw::default()
        };
        assert_eq!(build(&raw, &test_config(), now()).tasks.len(), 1);
    }

    #[test]
    fn the_real_repository_keeps_a_fix_that_closed_a_ready_for_agent_issue() {
        let raw = Raw {
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
        assert_eq!(snapshot.landed_fixes.len(), 1);
        assert_eq!(snapshot.landed_fixes[0].number, 683);
        assert_eq!(snapshot.landed_fixes[0].issues, vec![669]);
        assert_eq!(snapshot.landed_fixes[0].lane, None);
        assert_eq!(snapshot.leaderboard.len(), 1);
        assert_eq!(snapshot.leaderboard[0].login, "suprsokr");
        assert_eq!(snapshot.leaderboard[0].score, 10);
    }

    #[tokio::test]
    async fn the_garden_counts_claims_that_ended_without_a_merge_most_often_first() {
        let gh = crate::github::fake::community();
        let config = test_config();
        let mut cache = crate::github::Cache::default();
        let mut raw = Raw::default();
        for _ in 0..3 {
            raw = crate::github::fetch(&gh, &config, &mut cache)
                .await
                .unwrap();
        }
        let garden = build(&raw, &config, now()).garden;
        assert_eq!(
            (garden.closed_claims_checked, garden.closed_claims_unchecked),
            (5, 0)
        );
        let counts: Vec<String> = garden
            .systems
            .iter()
            .map(|system| {
                let unfinished: Vec<String> = system
                    .unfinished
                    .iter()
                    .map(|gap| format!("{} {:?}", gap.id, gap.issues))
                    .collect();
                format!(
                    "{}: {} tasks, {} claimed, {unfinished:?}",
                    system.system, system.tasks, system.claimed
                )
            })
            .collect();
        // 1952 closed through a merged pull request and 1953 was labelled by its author.
        assert_eq!(
            counts,
            vec![
                r#"creatures: 2 tasks, 1 claimed, ["creature_world_behavior-aaedc5f1 [1950, 1951]"]"#,
                r#"pets: 1 tasks, 1 claimed, ["pet_follow-d760a50f [1954]"]"#,
                "spells: 2 tasks, 1 claimed, []",
            ]
        );
    }

    #[test]
    fn a_maintainer_pull_request_is_kept_only_when_it_closes_a_claim() {
        let issue = json!({
            "number": 105,
            "body": "<!-- lyracore-gap: swing-18ac3e73 -->",
            "labels": [{"name": GAP}, {"name": IN_PROGRESS}],
            "assignees": [{"login": "ada-dev", "type": "User"}],
        });
        let raw = Raw {
            ledger: vec![LedgerFile {
                path: "combat/swing.toml".into(),
                text: "id = \"swing\"\nsystem = \"combat\"\n[[known_gaps]]\ndescription = \"d\"\n"
                    .into(),
            }],
            gap_issues: records(vec![issue]),
            claim_events: [(
                105,
                records(vec![workflow_label(GAP, "2026-11-27T10:00:00Z")]),
            )]
            .into(),
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
        assert_eq!(
            snapshot.task("swing-18ac3e73").unwrap().pull_requests,
            vec![1, 2]
        );
        assert!(snapshot.outside_pulls().next().is_none());
    }
}
