//! Reading GitHub: one small trait, its REST client, and the requests one refresh makes.

use crate::config::Config;
use crate::ledger::{self, LEDGER_BRANCH, LEDGER_DIR, LedgerFile};
use crate::raw::{Comment, Event, Issue, MergedPull, Raw, Review, TreeEntry, records};
use crate::snapshot::{GAP, READY, closing_numbers, is_claim_issue, is_held, is_outside};
use anyhow::{Context, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, LINK, USER_AGENT};
use serde_json::Value;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};

const API: &str = "https://api.github.com";
const RAW: &str = "https://raw.githubusercontent.com";
const MAX_PAGES: usize = 10;
// Each closed claim issue costs one request once. Without a token the hourly limit of 60 cannot
// afford a backlog in one refresh, so Workshop works through it a few at a time.
const CLOSED_CHECKS_WITHOUT_TOKEN: usize = 2;
const CLOSED_CHECKS_WITH_TOKEN: usize = 100;
// A pull request description is untrusted and each reference costs a request.
const MAX_CLOSING_REFERENCES: usize = 5;

/// GitHub answered 404: the record does not exist, which is not a failed refresh.
#[derive(Debug)]
pub struct NotFound;

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("404 Not Found")
    }
}

impl std::error::Error for NotFound {}

pub trait GitHub: Send + Sync {
    /// The JSON body of every page of a GET on `path`, which starts with `/`.
    fn get(&self, path: &str) -> impl Future<Output = anyhow::Result<Vec<Value>>> + Send;

    /// A file from raw.githubusercontent.com, at `path` after the host, which starts with `/`.
    fn raw(&self, path: &str) -> impl Future<Output = anyhow::Result<String>> + Send;

    fn authenticated(&self) -> bool;
}

/// Requests one refresh made: to the API, which the hourly limit counts, and to raw files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestCount {
    pub api: usize,
    pub raw: usize,
}

pub struct RestClient {
    http: reqwest::Client,
    token: Option<String>,
    requests: AtomicUsize,
    raw_requests: AtomicUsize,
}

impl RestClient {
    pub fn new(token: Option<String>) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            token,
            requests: AtomicUsize::new(0),
            raw_requests: AtomicUsize::new(0),
        })
    }

    pub fn take_request_count(&self) -> RequestCount {
        RequestCount {
            api: self.requests.swap(0, Ordering::Relaxed),
            raw: self.raw_requests.swap(0, Ordering::Relaxed),
        }
    }
}

impl GitHub for RestClient {
    async fn get(&self, path: &str) -> anyhow::Result<Vec<Value>> {
        let mut pages = Vec::new();
        let mut url = format!("{API}{path}");
        loop {
            let mut request = self
                .http
                .get(&url)
                .header(USER_AGENT, "workshop")
                .header(ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28");
            if let Some(token) = &self.token {
                request = request.header(AUTHORIZATION, format!("Bearer {token}"));
            }
            self.requests.fetch_add(1, Ordering::Relaxed);
            let response = request
                .send()
                .await
                .with_context(|| format!("GET {path}"))?;
            let status = response.status();
            if status == reqwest::StatusCode::NOT_FOUND {
                return Err(anyhow::Error::new(NotFound).context(format!("GET {path}")));
            }
            if !status.is_success() {
                bail!(
                    "GET {path}: {status}{}",
                    rate_limit_note(response.headers())
                );
            }
            let next = next_page(response.headers());
            pages.push(
                response
                    .json()
                    .await
                    .with_context(|| format!("GET {path}: body is not JSON"))?,
            );
            match next {
                Some(next) if pages.len() < MAX_PAGES && next.starts_with(API) => url = next,
                _ => return Ok(pages),
            }
        }
    }

    async fn raw(&self, path: &str) -> anyhow::Result<String> {
        let mut request = self
            .http
            .get(format!("{RAW}{path}"))
            .header(USER_AGENT, "workshop");
        if let Some(token) = &self.token {
            request = request.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        self.raw_requests.fetch_add(1, Ordering::Relaxed);
        let response = request
            .send()
            .await
            .with_context(|| format!("GET raw {path}"))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(anyhow::Error::new(NotFound).context(format!("GET raw {path}")));
        }
        if !status.is_success() {
            bail!("GET raw {path}: {status}");
        }
        response
            .text()
            .await
            .with_context(|| format!("GET raw {path}: body is not text"))
    }

    fn authenticated(&self) -> bool {
        self.token.is_some()
    }
}

fn rate_limit_note(headers: &HeaderMap) -> String {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    match (header("x-ratelimit-remaining"), header("x-ratelimit-reset")) {
        (Some("0"), Some(reset)) => format!(" (rate limit exhausted until Unix time {reset})"),
        _ => String::new(),
    }
}

fn next_page(headers: &HeaderMap) -> Option<String> {
    let link = headers.get(LINK)?.to_str().ok()?;
    link.split(',').find_map(|part| {
        let (url, rel) = part.split_once(';')?;
        rel.contains("rel=\"next\"").then(|| {
            url.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_owned()
        })
    })
}

/// The records in a set of pages: list pages, search pages (`items`) or a single object.
fn items(pages: Vec<Value>) -> Vec<Value> {
    pages
        .into_iter()
        .flat_map(|page| match page {
            Value::Array(values) => values,
            Value::Object(mut object) if object.get("items").is_some_and(Value::is_array) => {
                match object.remove("items") {
                    Some(Value::Array(values)) => values,
                    _ => Vec::new(),
                }
            }
            value => vec![value],
        })
        .collect()
}

#[derive(Debug, Clone)]
struct MergedDetails {
    reviews: Vec<Review>,
    comments: Option<Vec<Comment>>,
    referenced: Vec<Issue>,
}

/// What Workshop keeps between refreshes so a refresh reads only what changed. Every entry is a
/// copy of a GitHub record that a restart reads again.
#[derive(Debug, Default)]
pub struct Cache {
    /// A merged pull request's reviews, lane and closed issues. Merged pull requests do not change.
    merged: HashMap<u64, MergedDetails>,
    /// Ledger files by path, with the blob SHA their text has.
    ledger: BTreeMap<String, (String, String)>,
    /// Closed `gap` issues, and the events of each as of the `updated_at` they were read at.
    closed: BTreeMap<u64, Issue>,
    closed_events: HashMap<u64, (Option<DateTime<Utc>>, Vec<Event>)>,
    closed_since: Option<DateTime<Utc>>,
}

pub fn merged_search_path(config: &Config) -> String {
    let mut query = format!("repo:{} is:pr is:merged", config.repository);
    for maintainer in &config.maintainers {
        query.push_str(&format!(" -author:{maintainer}"));
    }
    format!("/search/issues?q={}&per_page=100", query.replace(' ', "+"))
}

pub fn ledger_tree_path(config: &Config) -> String {
    format!(
        "/repos/{}/git/trees/{LEDGER_BRANCH}:{LEDGER_DIR}?recursive=1",
        config.repository
    )
}

pub fn ledger_raw_path(config: &Config, path: &str) -> String {
    format!(
        "/{}/{LEDGER_BRANCH}/{}",
        config.repository,
        ledger::repository_path(path)
    )
}

/// One API request lists the Ledger with each file's blob SHA. A file is read from
/// raw.githubusercontent.com only when its SHA is new, and kept only when its text has that SHA:
/// the raw host caches for minutes, so a copy older than the tree waits for a later refresh.
async fn fetch_ledger<G: GitHub>(
    gh: &G,
    config: &Config,
    cache: &mut Cache,
) -> anyhow::Result<(Vec<LedgerFile>, usize)> {
    let entries: Vec<TreeEntry> = gh
        .get(&ledger_tree_path(config))
        .await?
        .into_iter()
        .flat_map(|page| match page {
            Value::Object(mut object) => match object.remove("tree") {
                Some(Value::Array(entries)) => entries,
                _ => Vec::new(),
            },
            _ => Vec::new(),
        })
        .filter_map(|value| serde_json::from_value(value).ok())
        .filter(|entry: &TreeEntry| entry.kind == "blob" && ledger::is_ledger_path(&entry.path))
        .collect();
    let mut waiting = 0;
    for entry in &entries {
        if cache
            .ledger
            .get(&entry.path)
            .is_some_and(|(sha, _)| *sha == entry.sha)
        {
            continue;
        }
        match gh.raw(&ledger_raw_path(config, &entry.path)).await {
            Ok(text) if ledger::blob_sha(text.as_bytes()) == entry.sha => {
                cache
                    .ledger
                    .insert(entry.path.clone(), (entry.sha.clone(), text));
            }
            Ok(_) => waiting += 1,
            Err(error) if error.downcast_ref::<NotFound>().is_some() => waiting += 1,
            Err(error) => return Err(error),
        }
    }
    let listed: HashSet<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    cache
        .ledger
        .retain(|path, _| listed.contains(path.as_str()));
    let files = cache
        .ledger
        .iter()
        .map(|(path, (_, text))| LedgerFile {
            path: path.clone(),
            text: text.clone(),
        })
        .collect();
    Ok((files, waiting))
}

pub fn closed_gap_path(config: &Config, since: Option<DateTime<Utc>>) -> String {
    let mut path = format!(
        "/repos/{}/issues?state=closed&labels={GAP}&sort=updated&direction=asc&per_page=100",
        config.repository
    );
    if let Some(since) = since {
        path.push_str("&since=");
        path.push_str(&since.to_rfc3339_opts(SecondsFormat::Secs, true));
    }
    path
}

pub async fn fetch<G: GitHub>(gh: &G, config: &Config, cache: &mut Cache) -> anyhow::Result<Raw> {
    let repo = format!("/repos/{}", config.repository);
    let list = |path: String| async move { gh.get(&path).await.map(items) };

    let (ledger, ledger_waiting) = fetch_ledger(gh, config, cache).await?;
    let gap_issues: Vec<Issue> = records(
        list(format!(
            "{repo}/issues?state=open&labels={GAP}&per_page=100"
        ))
        .await?,
    );
    let open_pulls: Vec<crate::raw::Pull> =
        records(list(format!("{repo}/pulls?state=open&per_page=100")).await?);
    let merged_pulls: Vec<MergedPull> = records(list(merged_search_path(config)).await?);

    let mut raw = Raw {
        authenticated: gh.authenticated(),
        ..Raw::default()
    };
    for issue in gap_issues.iter().filter(|issue| is_held(issue)) {
        let events = list(format!(
            "{repo}/issues/{}/events?per_page=100",
            issue.number
        ))
        .await?;
        raw.claim_events.insert(issue.number, records(events));
    }

    // Closed claim issues change rarely, so after the first read only the updated ones come back.
    let closed: Vec<Issue> = records(list(closed_gap_path(config, cache.closed_since)).await?);
    for issue in closed.into_iter().filter(|issue| !issue.is_pull_request()) {
        cache.closed_since = cache.closed_since.max(issue.updated_at);
        cache.closed.insert(issue.number, issue);
    }
    for issue in &gap_issues {
        cache.closed.remove(&issue.number);
    }
    let closed = &cache.closed;
    cache.closed_events.retain(|number, (updated_at, _)| {
        closed
            .get(number)
            .is_some_and(|issue| issue.updated_at == *updated_at)
    });
    let budget = if gh.authenticated() {
        CLOSED_CHECKS_WITH_TOKEN
    } else {
        CLOSED_CHECKS_WITHOUT_TOKEN
    };
    let unread: Vec<&Issue> = cache
        .closed
        .values()
        .filter(|issue| is_claim_issue(issue) && !cache.closed_events.contains_key(&issue.number))
        .collect();
    raw.closed_unchecked = unread.len().saturating_sub(budget);
    for issue in unread.into_iter().take(budget) {
        let events = match list(format!(
            "{repo}/issues/{}/events?per_page=100",
            issue.number
        ))
        .await
        {
            Ok(events) => records(events),
            Err(error) if error.downcast_ref::<NotFound>().is_some() => Vec::new(),
            Err(error) => return Err(error),
        };
        cache
            .closed_events
            .insert(issue.number, (issue.updated_at, events));
    }
    for (number, (_, events)) in &cache.closed_events {
        raw.claim_events.insert(*number, events.clone());
    }
    raw.closed_gap_issues = cache.closed.values().cloned().collect();

    for pull in open_pulls
        .iter()
        .filter(|pull| is_outside(&pull.user, config))
    {
        let comments = list(format!(
            "{repo}/issues/{}/comments?per_page=100",
            pull.number
        ))
        .await?;
        let reviews = list(format!("{repo}/pulls/{}/reviews?per_page=100", pull.number)).await?;
        raw.comments.insert(pull.number, records(comments));
        raw.reviews.insert(pull.number, records(reviews));
    }
    for pull in merged_pulls
        .iter()
        .filter(|pull| is_outside(&pull.user, config))
    {
        let details = match cache.merged.entry(pull.number) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(merged_details(&list, &repo, pull).await?),
        };
        raw.reviews.insert(pull.number, details.reviews.clone());
        if let Some(comments) = &details.comments {
            raw.comments.insert(pull.number, comments.clone());
        }
        for issue in &details.referenced {
            raw.referenced_issues.insert(issue.number, issue.clone());
        }
    }
    raw.ledger = ledger;
    raw.ledger_waiting = ledger_waiting;
    raw.gap_issues = gap_issues;
    raw.open_pulls = open_pulls;
    raw.merged_pulls = merged_pulls;
    Ok(raw)
}

async fn merged_details<F, Fut>(
    list: &F,
    repo: &str,
    pull: &MergedPull,
) -> anyhow::Result<MergedDetails>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = anyhow::Result<Vec<Value>>>,
{
    let reviews =
        records(list(format!("{repo}/pulls/{}/reviews?per_page=100", pull.number)).await?);
    let mut referenced = Vec::new();
    for number in closing_numbers(pull.body.as_deref().unwrap_or(""))
        .into_iter()
        .take(MAX_CLOSING_REFERENCES)
    {
        match list(format!("{repo}/issues/{number}")).await {
            Ok(issue) => referenced.extend(records::<Issue>(issue)),
            Err(error) if error.downcast_ref::<NotFound>().is_some() => {}
            Err(error) => return Err(error),
        }
    }
    let closed_a_task = referenced
        .iter()
        .any(|issue| !issue.is_pull_request() && (issue.has_label(GAP) || issue.has_label(READY)));
    let comments = if closed_a_task {
        Some(records(
            list(format!(
                "{repo}/issues/{}/comments?per_page=100",
                pull.number
            ))
            .await?,
        ))
    } else {
        None
    };
    Ok(MergedDetails {
        reviews,
        comments,
        referenced,
    })
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::sync::Mutex;

    fn fixture_text(file: &str) -> String {
        let file = format!("{}/tests/fixtures/{file}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&file).unwrap_or_else(|_| panic!("{file}"))
    }

    /// Serves fixture files by request path and records every path asked for. A `since` query
    /// is recorded but does not change the answer.
    pub struct FakeGitHub {
        routes: HashMap<String, Value>,
        raw_routes: HashMap<String, String>,
        pub requests: Mutex<Vec<String>>,
    }

    impl FakeGitHub {
        pub fn new(routes: &[(&str, &str)]) -> Self {
            Self::with_raw(routes, &[])
        }

        pub fn with_raw(routes: &[(&str, &str)], raw_routes: &[(&str, &str)]) -> Self {
            Self {
                routes: routes
                    .iter()
                    .map(|(path, file)| {
                        (
                            path.to_string(),
                            serde_json::from_str(&fixture_text(file)).unwrap(),
                        )
                    })
                    .collect(),
                raw_routes: raw_routes
                    .iter()
                    .map(|(path, file)| (path.to_string(), fixture_text(file)))
                    .collect(),
                requests: Mutex::new(Vec::new()),
            }
        }

        pub fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        pub fn raw_count(&self) -> usize {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|path| path.starts_with("raw "))
                .count()
        }
    }

    impl GitHub for FakeGitHub {
        async fn get(&self, path: &str) -> anyhow::Result<Vec<Value>> {
            self.requests.lock().unwrap().push(path.to_owned());
            let route = path.split("&since=").next().unwrap_or(path);
            match self.routes.get(route) {
                Some(value) => Ok(vec![value.clone()]),
                None => Err(anyhow::Error::new(NotFound).context(format!("GET {path}"))),
            }
        }

        async fn raw(&self, path: &str) -> anyhow::Result<String> {
            self.requests.lock().unwrap().push(format!("raw {path}"));
            match self.raw_routes.get(path) {
                Some(text) => Ok(text.clone()),
                None => Err(anyhow::Error::new(NotFound).context(format!("GET raw {path}"))),
            }
        }

        fn authenticated(&self) -> bool {
            false
        }
    }

    const R: &str = "/repos/LyraCoreProject/LyraCore";
    pub const LEDGER_FILES: [&str; 5] = [
        "combat/broken.toml",
        "creatures/creature_world_behavior.toml",
        "mail/mail_return.toml",
        "pets/pet_follow.toml",
        "spells/cast_and_interrupt.toml",
    ];

    fn route(path: &str) -> String {
        format!("{R}{path}")
    }

    /// A hand-written repository: a Ledger, claims real and forged, pull requests, reviews, closed
    /// claim issues and a landed fix.
    pub fn community() -> FakeGitHub {
        let config = crate::config::test_config();
        let mut routes: Vec<(String, String)> = vec![
            (ledger_tree_path(&config), "ledger/tree.json".into()),
            (
                route("/issues?state=open&labels=gap&per_page=100"),
                "community/issues_gap_open.json".into(),
            ),
            (
                closed_gap_path(&config, None),
                "community/issues_gap_closed.json".into(),
            ),
            (
                route("/pulls?state=open&per_page=100"),
                "community/pulls_open.json".into(),
            ),
            (
                merged_search_path(&config),
                "community/search_merged.json".into(),
            ),
            (route("/issues/1900"), "community/issue_1900.json".into()),
            (route("/issues/1901"), "community/issue_1901.json".into()),
            (route("/issues/1952"), "community/issue_1952.json".into()),
        ];
        for number in [1950, 1951, 1952, 1953, 1954, 2002, 2003, 2004, 2005] {
            routes.push((
                route(&format!("/issues/{number}/events?per_page=100")),
                format!("community/events_{number}.json"),
            ));
        }
        for number in [2900, 3001, 3002] {
            routes.push((
                route(&format!("/issues/{number}/comments?per_page=100")),
                format!("community/comments_{number}.json"),
            ));
        }
        for number in [2900, 2901, 3001, 3002] {
            routes.push((
                route(&format!("/pulls/{number}/reviews?per_page=100")),
                format!("community/reviews_{number}.json"),
            ));
        }
        let raw_routes: Vec<(String, String)> = LEDGER_FILES
            .iter()
            .map(|file| (ledger_raw_path(&config, file), format!("ledger/{file}")))
            .collect();
        let routes: Vec<(&str, &str)> = routes
            .iter()
            .map(|(p, f)| (p.as_str(), f.as_str()))
            .collect();
        let raw_routes: Vec<(&str, &str)> = raw_routes
            .iter()
            .map(|(p, f)| (p.as_str(), f.as_str()))
            .collect();
        FakeGitHub::with_raw(&routes, &raw_routes)
    }

    pub async fn community_snapshot() -> crate::snapshot::Snapshot {
        let config = crate::config::test_config();
        let raw = fetch(&community(), &config, &mut Cache::default())
            .await
            .unwrap();
        crate::snapshot::build(&raw, &config, "2026-11-27T20:00:00Z".parse().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeGitHub, LEDGER_FILES, community};
    use super::*;
    use crate::config::test_config;
    use crate::snapshot::{Lane, build};

    #[test]
    fn the_merged_search_leaves_out_each_maintainer() {
        assert_eq!(
            merged_search_path(&test_config()),
            "/search/issues?q=repo:LyraCoreProject/LyraCore+is:pr+is:merged+-author:Mathih13&per_page=100"
        );
    }

    #[test]
    fn the_ledger_is_one_tree_request_and_raw_files_at_the_branch() {
        let config = test_config();
        assert_eq!(
            ledger_tree_path(&config),
            "/repos/LyraCoreProject/LyraCore/git/trees/main:conformance/features?recursive=1"
        );
        assert_eq!(
            ledger_raw_path(&config, "spells/cast_and_interrupt.toml"),
            "/LyraCoreProject/LyraCore/main/conformance/features/spells/cast_and_interrupt.toml"
        );
    }

    #[tokio::test]
    async fn a_refresh_reads_only_what_changes_and_scores_outside_work() {
        let gh = community();
        let config = test_config();
        let mut cache = Cache::default();
        let raw = fetch(&gh, &config, &mut cache).await.unwrap();
        // Tree, open gap issues, open pull requests, merged search, 4 held claim issues, closed
        // gap issues, 2 closed claim checks, 2 per open outside pull request, and the merged
        // details once, one of them for an issue that does not exist.
        assert_eq!(gh.request_count() - gh.raw_count(), 4 + 4 + 1 + 2 + 4 + 7);
        assert_eq!(gh.raw_count(), LEDGER_FILES.len());
        assert_eq!(raw.ledger_waiting, 1, "the mail file's raw copy is older");
        assert_eq!(raw.closed_unchecked, 3);

        let now = "2026-11-27T20:00:00Z".parse().unwrap();
        let snapshot = build(&raw, &config, now);
        assert_eq!(snapshot.ledger.files, 4);
        assert_eq!(
            snapshot.ledger.unparsed,
            vec!["conformance/features/combat/broken.toml"]
        );

        let order: Vec<&str> = snapshot.tasks.iter().map(|task| task.id.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "cast_and_interrupt-4f97715e",
                "creature_world_behavior-aaedc5f1",
                "pet_follow-d760a50f",
                "cast_and_interrupt-52f5323c",
                "creature_world_behavior-4b52062e",
            ]
        );
        assert!(
            snapshot
                .task("cast_and_interrupt-4f97715e")
                .unwrap()
                .claim
                .is_none(),
            "a claim issue whose gap label its author added is not a claim"
        );

        let held = snapshot.task("cast_and_interrupt-52f5323c").unwrap();
        let claim = held.claim.as_ref().unwrap();
        assert_eq!((claim.holder.as_str(), claim.issue), ("ada-dev", 2002));
        assert_eq!(claim.since, Some("2026-11-27T10:00:00Z".parse().unwrap()));
        assert_eq!(claim.expires_at, None, "an open pull request covers it");
        assert_eq!(held.pull_requests, vec![3001]);

        let stale = snapshot
            .task("creature_world_behavior-4b52062e")
            .unwrap()
            .claim
            .as_ref()
            .unwrap();
        assert_eq!(
            stale.expires_at,
            Some("2026-11-27T14:00:00Z".parse().unwrap())
        );

        let maintainer = snapshot.task("pet_follow-d760a50f").unwrap();
        assert_eq!(maintainer.claim.as_ref().unwrap().expires_at, None);
        assert_eq!(maintainer.pull_requests, vec![3003]);

        let outside: Vec<u64> = snapshot.outside_pulls().map(|pull| pull.number).collect();
        assert_eq!(outside, vec![3001, 3002]);
        let fix = snapshot.pull(3001).unwrap();
        assert_eq!(fix.lane, Some(Lane::Fast));
        let reviewers: Vec<&str> = fix.reviews.iter().map(|r| r.reviewer.as_str()).collect();
        assert_eq!(reviewers, vec!["cy-dev"]);
        assert_eq!(snapshot.pull(3002).unwrap().lane, None);

        let wanted: Vec<u64> = snapshot
            .reviews_wanted("dee-dev")
            .iter()
            .map(|p| p.number)
            .collect();
        assert_eq!(wanted, vec![3002, 3001]);
        let wanted: Vec<u64> = snapshot
            .reviews_wanted("CY-DEV")
            .iter()
            .map(|p| p.number)
            .collect();
        assert_eq!(wanted, vec![3001]);

        assert_eq!(snapshot.landed_fixes.len(), 1);
        assert_eq!(snapshot.landed_fixes[0].number, 2900);
        assert_eq!(snapshot.landed_fixes[0].issues, vec![1900, 1952]);
        let scores: Vec<(&str, u32)> = snapshot
            .leaderboard
            .iter()
            .map(|standing| (standing.login.as_str(), standing.score))
            .collect();
        assert_eq!(scores, vec![("ada-dev", 15), ("cy-dev", 2)]);

        let before = gh.request_count();
        let raw = fetch(&gh, &config, &mut cache).await.unwrap();
        let requests = gh.requests.lock().unwrap()[before..].to_vec();
        assert_eq!(
            requests.len(),
            4 + 4 + 1 + 2 + 4 + 1,
            "unchanged Ledger files and merged details come from the cache: {requests:#?}"
        );
        assert!(
            requests
                .iter()
                .any(|path| path.ends_with("&since=2026-11-26T10:00:00Z")),
            "closed claim issues are read from the last update on"
        );
        assert_eq!(raw.closed_unchecked, 1);
        let raw = fetch(&gh, &config, &mut cache).await.unwrap();
        assert_eq!(raw.closed_unchecked, 0);
    }

    #[tokio::test]
    async fn a_failed_request_fails_the_refresh() {
        let gh = FakeGitHub::new(&[]);
        let error = fetch(&gh, &test_config(), &mut Cache::default())
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("404"));
    }

    #[test]
    fn the_next_page_comes_from_the_link_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            LINK,
            "<https://api.github.com/x?page=2>; rel=\"next\", <https://api.github.com/x?page=5>; rel=\"last\""
                .parse()
                .unwrap(),
        );
        assert_eq!(
            next_page(&headers).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
    }
}
