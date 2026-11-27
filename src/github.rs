//! Reading GitHub: one small trait, its REST client, and the requests one refresh makes.

use crate::config::Config;
use crate::raw::{Comment, Issue, MergedPull, Raw, Review, records};
use crate::snapshot::{READY, closing_numbers, is_claimed, is_outside};
use anyhow::{Context, bail};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, LINK, USER_AGENT};
use serde_json::Value;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};

const API: &str = "https://api.github.com";
const MAX_PAGES: usize = 10;

pub trait GitHub: Send + Sync {
    /// The JSON body of every page of a GET on `path`, which starts with `/`.
    fn get(&self, path: &str) -> impl Future<Output = anyhow::Result<Vec<Value>>> + Send;
}

pub struct RestClient {
    http: reqwest::Client,
    token: Option<String>,
    requests: AtomicUsize,
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
        })
    }

    pub fn take_request_count(&self) -> usize {
        self.requests.swap(0, Ordering::Relaxed)
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

/// What a merged pull request's lane, reviews and closed tasks were. Merged pull requests do not
/// change, so one process reads each only once; this is what keeps a refresh small.
#[derive(Debug, Default)]
pub struct MergedCache {
    details: HashMap<u64, MergedDetails>,
}

pub fn merged_search_path(config: &Config) -> String {
    let mut query = format!("repo:{} is:pr is:merged", config.repository);
    for maintainer in &config.maintainers {
        query.push_str(&format!(" -author:{maintainer}"));
    }
    format!("/search/issues?q={}&per_page=100", query.replace(' ', "+"))
}

pub async fn fetch<G: GitHub>(
    gh: &G,
    config: &Config,
    cache: &mut MergedCache,
) -> anyhow::Result<Raw> {
    let repo = format!("/repos/{}", config.repository);
    let list = |path: String| async move { gh.get(&path).await.map(items) };

    let tasks: Vec<Issue> = records(
        list(format!(
            "{repo}/issues?state=open&labels={READY}&per_page=100"
        ))
        .await?,
    );
    let open_pulls: Vec<crate::raw::Pull> =
        records(list(format!("{repo}/pulls?state=open&per_page=100")).await?);
    let merged_pulls: Vec<MergedPull> = records(list(merged_search_path(config)).await?);

    let mut raw = Raw::default();
    for task in tasks
        .iter()
        .filter(|task| !task.is_pull_request() && is_claimed(task))
    {
        let events = list(format!("{repo}/issues/{}/events?per_page=100", task.number)).await?;
        raw.claim_events.insert(task.number, records(events));
    }
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
        let details = match cache.details.entry(pull.number) {
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
    raw.tasks = tasks;
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
    for number in closing_numbers(pull.body.as_deref().unwrap_or("")) {
        referenced.extend(records::<Issue>(
            list(format!("{repo}/issues/{number}")).await?,
        ));
    }
    let closed_a_task = referenced
        .iter()
        .any(|issue| !issue.is_pull_request() && issue.has_label(READY));
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

    /// Serves fixture files by request path and records every path asked for.
    pub struct FakeGitHub {
        routes: HashMap<String, Value>,
        pub requests: Mutex<Vec<String>>,
    }

    impl FakeGitHub {
        pub fn new(routes: &[(&str, &str)]) -> Self {
            let routes = routes
                .iter()
                .map(|(path, file)| {
                    let file = format!("{}/tests/fixtures/{file}", env!("CARGO_MANIFEST_DIR"));
                    let text = std::fs::read_to_string(&file).unwrap_or_else(|_| panic!("{file}"));
                    (path.to_string(), serde_json::from_str(&text).unwrap())
                })
                .collect();
            Self {
                routes,
                requests: Mutex::new(Vec::new()),
            }
        }

        pub fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    impl GitHub for FakeGitHub {
        async fn get(&self, path: &str) -> anyhow::Result<Vec<Value>> {
            self.requests.lock().unwrap().push(path.to_owned());
            match self.routes.get(path) {
                Some(value) => Ok(vec![value.clone()]),
                None => bail!("GET {path}: 404 Not Found"),
            }
        }
    }

    const R: &str = "/repos/LyraCoreProject/LyraCore";

    fn route(path: &str) -> String {
        format!("{R}{path}")
    }

    /// A hand-written repository with outside claims, pull requests, reviews and a landed fix.
    pub fn community() -> FakeGitHub {
        let search = merged_search_path(&crate::config::test_config());
        let routes: Vec<(String, &str)> = vec![
            (
                route("/issues?state=open&labels=ready-for-agent&per_page=100"),
                "community/issues_ready.json",
            ),
            (
                route("/pulls?state=open&per_page=100"),
                "community/pulls_open.json",
            ),
            (search, "community/search_merged.json"),
            (
                route("/issues/2002/events?per_page=100"),
                "community/events_2002.json",
            ),
            (
                route("/issues/2003/events?per_page=100"),
                "community/events_2003.json",
            ),
            (
                route("/issues/2004/events?per_page=100"),
                "community/events_2004.json",
            ),
            (
                route("/issues/3001/comments?per_page=100"),
                "community/comments_3001.json",
            ),
            (
                route("/pulls/3001/reviews?per_page=100"),
                "community/reviews_3001.json",
            ),
            (
                route("/issues/3002/comments?per_page=100"),
                "community/comments_3002.json",
            ),
            (
                route("/pulls/3002/reviews?per_page=100"),
                "community/reviews_3002.json",
            ),
            (
                route("/pulls/2900/reviews?per_page=100"),
                "community/reviews_2900.json",
            ),
            (
                route("/pulls/2901/reviews?per_page=100"),
                "community/reviews_2901.json",
            ),
            (route("/issues/1900"), "community/issue_1900.json"),
            (route("/issues/1901"), "community/issue_1901.json"),
            (
                route("/issues/2900/comments?per_page=100"),
                "community/comments_2900.json",
            ),
        ];
        let borrowed: Vec<(&str, &str)> = routes.iter().map(|(p, f)| (p.as_str(), *f)).collect();
        FakeGitHub::new(&borrowed)
    }

    pub async fn community_snapshot() -> crate::snapshot::Snapshot {
        let config = crate::config::test_config();
        let raw = fetch(&community(), &config, &mut MergedCache::default())
            .await
            .unwrap();
        crate::snapshot::build(&raw, &config, "2026-11-27T20:00:00Z".parse().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeGitHub, community};
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

    #[tokio::test]
    async fn a_refresh_reads_only_what_changes_and_scores_outside_work() {
        let gh = community();
        let config = test_config();
        let mut cache = MergedCache::default();
        let raw = fetch(&gh, &config, &mut cache).await.unwrap();
        // 3 lists, 3 claimed tasks, 2 per open outside pull request, and the merged details once.
        assert_eq!(gh.request_count(), 3 + 3 + 4 + 5);

        let now = "2026-11-27T20:00:00Z".parse().unwrap();
        let snapshot = build(&raw, &config, now);

        let order: Vec<u64> = snapshot.tasks.iter().map(|task| task.number).collect();
        assert_eq!(order, vec![2002, 2001, 2004, 2003]);

        let held = snapshot.task(2002).unwrap();
        let claim = held.claim.as_ref().unwrap();
        assert_eq!(claim.holders, vec!["ada-dev"]);
        assert_eq!(claim.since, Some("2026-11-27T10:00:00Z".parse().unwrap()));
        assert_eq!(claim.expires_at, None, "an open pull request covers it");
        assert_eq!(held.pull_requests, vec![3001]);

        let stale = snapshot.task(2003).unwrap().claim.as_ref().unwrap();
        assert_eq!(
            stale.expires_at,
            Some("2026-11-27T14:00:00Z".parse().unwrap())
        );

        let maintainer = snapshot.task(2004).unwrap();
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
        let scores: Vec<(&str, u32)> = snapshot
            .leaderboard
            .iter()
            .map(|standing| (standing.login.as_str(), standing.score))
            .collect();
        assert_eq!(scores, vec![("ada-dev", 15), ("cy-dev", 2)]);

        let before = gh.request_count();
        fetch(&gh, &config, &mut cache).await.unwrap();
        assert_eq!(
            gh.request_count() - before,
            3 + 3 + 4,
            "merged details come from the cache"
        );
    }

    #[tokio::test]
    async fn a_failed_request_fails_the_refresh() {
        let gh = FakeGitHub::new(&[]);
        let error = fetch(&gh, &test_config(), &mut MergedCache::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("404"));
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
