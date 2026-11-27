//! The website: server-rendered pages over the current snapshot.

use crate::mcp::claim_command;
use crate::snapshot::{Lane, OpenPull, SCHEMA_VERSION, Snapshot, Task};
use crate::state::AppState;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, Utc};
use maud::{DOCTYPE, Markup, html};
use serde_json::{Value, json};
use std::sync::Arc;

const STYLE: &str = include_str!("style.css");

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(start_page))
        .route("/tasks", get(tasks_page))
        .route("/tasks.json", get(tasks_json))
        .route("/garden", get(garden_page))
        .route("/reviews", get(reviews_page))
        .route("/leaderboard", get(leaderboard_page))
        .route("/connect", get(connect_page))
        .route("/copy.js", get(copy_script))
        .route("/snapshot.json", get(snapshot_json))
        .route("/healthz", get(healthz))
        .route("/style.css", get(style))
        .nest_service("/mcp", crate::mcp::service(state.clone()))
        .with_state(state)
}

pub fn age(from: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let minutes = (now - from).num_minutes().max(0);
    match minutes {
        0 => "under a minute".to_owned(),
        1..=59 => format!("{minutes} min"),
        60..=2879 => format!("{} h", minutes / 60),
        _ => format!("{} d", minutes / 1440),
    }
}

/// The address an agent should use for `/mcp`, from the Host the reader reached us by.
pub fn mcp_url(host: Option<&str>, forwarded_proto: Option<&str>, fallback: &str) -> String {
    let host = host
        .filter(|host| {
            !host.is_empty()
                && host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
        })
        .unwrap_or(fallback);
    let local = ["localhost", "127.0.0.1", "[::1]"]
        .iter()
        .any(|name| host == *name || host.starts_with(&format!("{name}:")));
    let scheme = match forwarded_proto {
        Some("http") => "http",
        Some("https") => "https",
        _ if local => "http",
        _ => "https",
    };
    format!("{scheme}://{host}/mcp")
}

const NOT_READY: &str = "Workshop has not read GitHub yet. Try again in a minute.";

fn page(title: &str, state: &AppState, snapshot: Option<&Snapshot>, body: Markup) -> Markup {
    let now = Utc::now();
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · Workshop" }
                link rel="stylesheet" href="/style.css";
                script src="/copy.js" defer {}
            }
            body {
                header {
                    a.brand href="/" { "Workshop" }
                    nav {
                        a href="/tasks" { "Tasks" }
                        a href="/reviews" { "Reviews" }
                        a href="/leaderboard" { "Leaderboard" }
                        a href="/connect" { "MCP" }
                    }
                }
                main {
                    h1 { (title) }
                    (body)
                }
                footer {
                    p {
                        "Read from "
                        a href={ "https://github.com/" (state.config.repository) } { (state.config.repository) }
                        @if let Some(snapshot) = snapshot {
                            " " (age(snapshot.built_at, now)) " ago"
                        }
                        ". Text from GitHub is shown as written by its authors. "
                        a href="/snapshot.json" { "snapshot.json" }
                    }
                }
            }
        }
    }
}

fn respond(markup: Markup, ready: bool) -> Response {
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        markup.into_string(),
    )
        .into_response()
}

fn lane_badge(lane: Option<Lane>) -> Markup {
    html! {
        @match lane {
            Some(lane) => span class={ "lane " (lane_class(lane)) } { (lane.name()) },
            None => span.lane { "no lane yet" },
        }
    }
}

fn lane_class(lane: Lane) -> &'static str {
    match lane {
        Lane::Fast => "fast",
        Lane::Slow => "slow",
        Lane::Failed => "failed",
    }
}

fn pull_line(pull: &OpenPull) -> Markup {
    html! {
        a href=(pull.url) { "#" (pull.number) } " by " (pull.author) " "
        @if pull.draft { span.badge { "draft" } " " }
        @if pull.outside { (lane_badge(pull.lane)) }
    }
}

fn copyable(text: &str) -> Markup {
    html! {
        div.copy {
            pre { code { (text) } }
            button type="button" hidden { "Copy" }
        }
    }
}

/// The two things a contributor copies: a clone command, then a prompt for their agent.
fn start_block(state: &AppState) -> Markup {
    let repository = &state.config.repository;
    html! {
        p.lead {
            "Two steps put your agent to work on " (repository) ". It sets up the machine, claims an " a href="/tasks" { "open task" } ", "
            "fixes it and opens the pull request. You need git and a signed-in "
            a href="https://cli.github.com/" { "GitHub CLI" } " (" code { "gh auth login" } ")."
        }
        ol.steps {
            li {
                p { "Run this in a terminal." }
                (copyable(&state.config.clone_command()))
            }
            li {
                p { "Start your agent in that folder and paste this." }
                (copyable(&state.config.start_prompt))
            }
        }
        p.meta {
            "Only maintainers merge. The full rules are in "
            a href={ "https://github.com/" (repository) "/blob/main/CONTRIBUTING.md" } { "CONTRIBUTING.md" } "."
        }
    }
}

pub fn render_start(state: &AppState, snapshot: Option<&Snapshot>) -> Markup {
    page("Lend your agent", state, snapshot, start_block(state))
}

const HEADLINE_CHARS: usize = 160;

/// A gap's first sentence, or its first words, for a one-line headline. True when text was cut.
pub fn headline(description: &str) -> (String, bool) {
    let text = description.trim();
    let sentence = text.find(". ").map(|end| &text[..=end]).unwrap_or(text);
    if sentence.chars().count() <= HEADLINE_CHARS {
        return (sentence.to_owned(), sentence.len() < text.len());
    }
    let cut: String = sentence.chars().take(HEADLINE_CHARS).collect();
    let cut = match cut.rfind(' ') {
        Some(space) => &cut[..space],
        None => cut.as_str(),
    };
    (format!("{}…", cut.trim_end_matches([',', ';', ':'])), true)
}

fn claim_line(task: &Task, now: DateTime<Utc>) -> Markup {
    html! {
        @match &task.claim {
            Some(claim) => {
                "Held by " (claim.holder) " in " a href=(claim.issue_url) { "#" (claim.issue) }
                @if let Some(since) = claim.since { " for " (age(since, now)) }
                @if let Some(expires) = claim.expires_at {
                    @if expires <= now { ", claim lapsed" }
                    @else { ", needs a pull request within " (age(now, expires)) }
                }
            },
            None => span.open { "Unclaimed" },
        }
    }
}

fn task_item(snapshot: &Snapshot, task: &Task, now: DateTime<Utc>) -> Markup {
    let (headline, cut) = headline(&task.description);
    html! {
        li id=(task.id) {
            details {
                summary.title { (headline) }
                @if cut { p { (task.description) } }
                @if !task.path.is_empty() {
                    p.meta { "In " code { (task.path) } @if !task.symbol.is_empty() { " " code { (task.symbol) } } }
                }
                @if !task.evidence.is_empty() { p.meta { "Evidence: " (task.evidence) } }
                @if let Some(vmangos) = &task.vmangos {
                    p.meta {
                        "vmangos: "
                        @match &vmangos.url {
                            Some(url) => a href=(url) { (vmangos.path) },
                            None => (vmangos.path),
                        }
                        @if !vmangos.symbol.is_empty() { " " code { (vmangos.symbol) } }
                    }
                }
                @if task.claim.is_none() {
                    p.meta { "Claim: " code { (claim_command(&snapshot.repository, &task.id)) } }
                }
            }
            p.meta {
                a href=(task.behavior_url) { (task.behavior) } " · "
                @if task.scenarios.is_empty() { "no scenario" }
                @else { "scenario " (task.scenarios.join(", ")) }
                " · " (claim_line(task, now))
            }
            p.meta { code.id { (task.id) } }
            @for pull in snapshot.pulls_for(task) {
                p.meta { "Pull request " (pull_line(pull)) }
            }
        }
    }
}

pub fn render_tasks(state: &AppState, snapshot: Option<&Snapshot>, now: DateTime<Utc>) -> Markup {
    let body = html! {
        @if let Some(snapshot) = snapshot {
            @let unclaimed = snapshot.tasks(true).count();
            p.lead {
                "Known gaps in the Feature Ledger that an agent can take: " (snapshot.tasks.len()) " tasks, "
                (unclaimed) " unclaimed. In each system, unclaimed tasks come first, then those whose Behavior has a scenario to measure the fix. "
                "Open a task for its evidence and claim command, or " a href="/" { "lend your agent" } " to pick one."
            }
            @if snapshot.tasks.is_empty() {
                p { "No open tasks right now." }
            }
            @let systems = snapshot.tasks_by_system();
            nav.systems {
                @for (system, tasks) in &systems {
                    a href={ "#system-" (system) } { (system) " " span.count { (tasks.len()) } }
                }
            }
            @for (system, tasks) in &systems {
                h2 id={ "system-" (system) } { (system) " " span.count { (tasks.len()) } }
                ul.items {
                    @for task in tasks { (task_item(snapshot, task, now)) }
                }
            }
        } @else { p { (NOT_READY) } }
    };
    page("Open tasks", state, snapshot, body)
}

pub fn render_garden(state: &AppState, snapshot: Option<&Snapshot>) -> Markup {
    let body = html! {
        @if let Some(snapshot) = snapshot {
            @let garden = &snapshot.garden;
            p.lead {
                "For the maintainer. Per system: its tasks, how many are claimed, and the tasks whose claim issue closed without a merged pull request, "
                "most often first. A task that keeps being released or lapsing may want splitting or rewording in the Ledger."
            }
            @if garden.closed_claims_unchecked > 0 {
                p.notice {
                    (garden.closed_claims_unchecked) " closed claim issues are not counted yet. Each costs one request, once. "
                    @if garden.authenticated { "Workshop reads them over the next refreshes." }
                    @else {
                        "Workshop runs without a GitHub token, so it reads only a few a refresh to stay inside the limit of 60 requests an hour. "
                        "Set " code { (crate::config::TOKEN_VARIABLE) } " to read them all at once."
                    }
                }
            }
            p.meta { (garden.closed_claims_checked) " closed claim issues counted." }
            table.garden {
                thead { tr { th { "System" } th { "Tasks" } th { "Claimed" } th { "Ended unfixed" } } }
                tbody {
                    @for system in &garden.systems {
                        tr {
                            td { a href={ "/tasks#system-" (system.system) } { (system.system) } }
                            td { (system.tasks) }
                            td { (system.claimed) }
                            td { (system.unfinished.len()) }
                        }
                    }
                }
            }
            @for system in garden.systems.iter().filter(|system| !system.unfinished.is_empty()) {
                h2 { (system.system) }
                ul.items {
                    @for gap in &system.unfinished {
                        li {
                            p.title { a href={ "/tasks#" (gap.id) } { (headline(&gap.description).0) } }
                            p.meta {
                                (gap.behavior) " · ended unfixed " (gap.issues.len()) " times: "
                                @for (i, number) in gap.issues.iter().enumerate() {
                                    @if i > 0 { ", " }
                                    a href=(crate::snapshot::issue_url(&snapshot.repository, *number)) { "#" (number) }
                                }
                            }
                        }
                    }
                }
            }
        } @else { p { (NOT_READY) } }
    };
    page("Garden", state, snapshot, body)
}

/// What an agent reads with curl to pick a task: the open tasks in snapshot order.
pub fn tasks_view(snapshot: &Snapshot) -> Value {
    let tasks: Vec<Value> = snapshot
        .tasks
        .iter()
        .map(|task| {
            json!({
                "id": task.id,
                "system": task.system,
                "behavior": task.behavior,
                "description": task.description,
                "path": task.path,
                "symbol": task.symbol,
                "has_scenario": !task.scenarios.is_empty(),
                "behavior_url": task.behavior_url,
                "claimed": task.claim.is_some(),
                "claim_issue": task.claim.as_ref().map(|claim| claim.issue),
                "claim_command": claim_command(&snapshot.repository, &task.id),
            })
        })
        .collect();
    json!({
        "schema_version": SCHEMA_VERSION,
        "built_at": snapshot.built_at,
        "tasks": tasks,
    })
}

pub fn render_reviews(state: &AppState, snapshot: Option<&Snapshot>, now: DateTime<Utc>) -> Markup {
    let body = html! {
        @if let Some(snapshot) = snapshot {
            p.lead {
                "Outside pull requests, oldest first. A review counts when it has a body: say what you checked. "
                "Review with " code { "gh pr review N -R " (snapshot.repository) " --comment" } "."
            }
            @let pulls: Vec<&OpenPull> = snapshot.outside_pulls().collect();
            @if pulls.is_empty() { p { "No outside pull requests are open." } }
            ul.items {
                @for pull in pulls {
                    li {
                        p.title { a href=(pull.url) { "#" (pull.number) " " (pull.title) } }
                        p.meta {
                            "By " (pull.author)
                            @if let Some(created) = pull.created_at { ", open " (age(created, now)) }
                            " " @if pull.draft { span.badge { "draft" } " " }
                            (lane_badge(pull.lane))
                        }
                        p.meta {
                            @if pull.reviews.is_empty() { "No reviews yet" }
                            @else {
                                "Reviews: "
                                @for (i, review) in pull.reviews.iter().enumerate() {
                                    @if i > 0 { ", " }
                                    (review.reviewer) " (" (review.state.to_lowercase().replace('_', " ")) ")"
                                }
                            }
                        }
                    }
                }
            }
        } @else { p { (NOT_READY) } }
    };
    page("Reviews wanted", state, snapshot, body)
}

pub fn render_leaderboard(state: &AppState, snapshot: Option<&Snapshot>) -> Markup {
    let body = html! {
        @if let Some(snapshot) = snapshot {
            p.lead {
                "10 points for a landed fix, 3 more if it was in the fast lane when it merged, and 2 for a review on an outside pull request that later merged. "
                "The score is a courtesy. It decides nothing about what merges."
            }
            @if snapshot.leaderboard.is_empty() { p { "Nobody has landed a fix or a review yet." } }
            ol.items {
                @for standing in &snapshot.leaderboard {
                    li {
                        p.title {
                            a href={ "https://github.com/" (standing.login) } { (standing.login) }
                            " · " (standing.score) " points"
                        }
                        @if !standing.fixes.is_empty() {
                            p.meta {
                                "Fixes: "
                                @for (i, fix) in standing.fixes.iter().enumerate() {
                                    @if i > 0 { ", " }
                                    a href=(fix.url) { "#" (fix.number) } " " (fix.title)
                                }
                            }
                        }
                        @if !standing.reviews.is_empty() {
                            p.meta {
                                "Reviews: "
                                @for (i, pull) in standing.reviews.iter().enumerate() {
                                    @if i > 0 { ", " }
                                    a href=(pull.url) { "#" (pull.number) }
                                }
                            }
                        }
                    }
                }
            }
        } @else { p { (NOT_READY) } }
    };
    page("Leaderboard", state, snapshot, body)
}

pub fn render_connect(state: &AppState, snapshot: Option<&Snapshot>, url: &str) -> Markup {
    let repository = &state.config.repository;
    let json = format!(
        "{{\n  \"mcpServers\": {{\n    \"workshop\": {{ \"type\": \"http\", \"url\": \"{url}\" }}\n  }}\n}}"
    );
    let body = html! {
        p.lead {
            "Optional. Your agent needs none of this to contribute: the " a href="/" { "two steps" } " are enough. "
            "The MCP server lets an agent read the tasks, claims, reviews and leaderboard. It speaks streamable HTTP at "
            code { (url) } ", needs no sign-in, and every tool only reads."
        }
        h2 { "Claude Code" }
        (copyable(&format!("claude mcp add --transport http workshop {url}")))
        h2 { "Another agent" }
        p { "Most agents take a server entry like this one:" }
        (copyable(&json))
        h2 { "Then" }
        p {
            "Ask your agent to call " code { "rules" } " and pick a task. Workshop never claims, comments or reviews for you: "
            "it names the " code { "gh" } " command, and your agent runs it with your GitHub account."
        }
        p {
            "The full rules are in "
            a href={ "https://github.com/" (repository) "/blob/main/CONTRIBUTING.md" } { "CONTRIBUTING.md" }
            "."
        }
    };
    page("MCP server", state, snapshot, body)
}

async fn start_page(State(state): State<Arc<AppState>>) -> Response {
    let snapshot = state.snapshot();
    respond(render_start(&state, snapshot.as_deref()), true)
}

async fn tasks_page(State(state): State<Arc<AppState>>) -> Response {
    let snapshot = state.snapshot();
    respond(
        render_tasks(&state, snapshot.as_deref(), Utc::now()),
        snapshot.is_some(),
    )
}

async fn garden_page(State(state): State<Arc<AppState>>) -> Response {
    let snapshot = state.snapshot();
    respond(
        render_garden(&state, snapshot.as_deref()),
        snapshot.is_some(),
    )
}

async fn reviews_page(State(state): State<Arc<AppState>>) -> Response {
    let snapshot = state.snapshot();
    respond(
        render_reviews(&state, snapshot.as_deref(), Utc::now()),
        snapshot.is_some(),
    )
}

async fn leaderboard_page(State(state): State<Arc<AppState>>) -> Response {
    let snapshot = state.snapshot();
    respond(
        render_leaderboard(&state, snapshot.as_deref()),
        snapshot.is_some(),
    )
}

async fn connect_page(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let header_value = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    let url = mcp_url(
        header_value("host"),
        header_value("x-forwarded-proto"),
        &state.config.listen.to_string(),
    );
    let snapshot = state.snapshot();
    respond(render_connect(&state, snapshot.as_deref(), &url), true)
}

async fn tasks_json(State(state): State<Arc<AppState>>) -> Response {
    match state.snapshot() {
        Some(snapshot) => (
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_string_pretty(&tasks_view(&snapshot)).unwrap_or_default(),
        )
            .into_response(),
        None => (StatusCode::SERVICE_UNAVAILABLE, "no snapshot yet\n").into_response(),
    }
}

async fn snapshot_json(State(state): State<Arc<AppState>>) -> Response {
    match state.snapshot() {
        Some(snapshot) => (
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_string_pretty(&*snapshot).unwrap_or_default(),
        )
            .into_response(),
        None => (StatusCode::SERVICE_UNAVAILABLE, "no snapshot yet\n").into_response(),
    }
}

pub fn health_text(state: &AppState, now: DateTime<Utc>) -> (StatusCode, String) {
    let health = state.health();
    let snapshot = state.snapshot();
    let mut text = match &snapshot {
        Some(snapshot) => format!(
            "ok\nsnapshot age: {}s\n",
            (now - snapshot.built_at).num_seconds().max(0)
        ),
        None => "no snapshot yet\n".to_owned(),
    };
    text.push_str(&format!(
        "last refresh requests: {} API, {} raw\n",
        health.last_request_count.api, health.last_request_count.raw
    ));
    if let Some(snapshot) = &snapshot {
        let ledger = &snapshot.ledger;
        text.push_str(&format!(
            "ledger files: {}, unparsed: {}, waiting for raw: {}\n",
            ledger.files,
            ledger.unparsed.len(),
            ledger.waiting
        ));
        for path in &ledger.unparsed {
            text.push_str(&format!("unparsed: {path}\n"));
        }
        text.push_str(&format!(
            "ledger gaps: {}, data gaps: {}, needs real client: {}, tasks: {}\n",
            ledger.gaps,
            ledger.data_gaps,
            ledger.needs_real_client,
            snapshot.tasks.len()
        ));
    }
    if let Some((at, error)) = health.last_error {
        text.push_str(&format!(
            "last refresh failed at {}: {error}\n",
            at.to_rfc3339()
        ));
    }
    let status = if snapshot.is_some() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, text)
}

async fn healthz(State(state): State<Arc<AppState>>) -> Response {
    health_text(&state, Utc::now()).into_response()
}

async fn copy_script() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("copy.js"),
    )
        .into_response()
}

async fn style() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        STYLE,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_config;
    use crate::github::fake::community_snapshot;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    fn now() -> DateTime<Utc> {
        "2026-11-27T20:00:00Z".parse().unwrap()
    }

    async fn ready_state() -> Arc<AppState> {
        let state = Arc::new(AppState::new(test_config()));
        state.replace(
            community_snapshot().await,
            crate::github::RequestCount { api: 22, raw: 5 },
        );
        state
    }

    async fn get(state: Arc<AppState>, path: &str) -> (StatusCode, String) {
        let response = router(state)
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn mcp(state: Arc<AppState>, body: Value) -> Value {
        let request = Request::post("/mcp")
            .header("host", "workshop.example")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-06-18")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = router(state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn call(state: Arc<AppState>, tool: &str, arguments: Value) -> (bool, String) {
        let reply = mcp(
            state,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                   "params": {"name": tool, "arguments": arguments}}),
        )
        .await;
        let result = &reply["result"];
        let text = result["content"][0]["text"].as_str().unwrap().to_owned();
        (result["isError"].as_bool().unwrap_or(false), text)
    }

    #[test]
    fn ages_read_in_the_largest_whole_unit() {
        let at = |text: &str| -> DateTime<Utc> { text.parse().unwrap() };
        assert_eq!(age(at("2026-11-27T19:59:30Z"), now()), "under a minute");
        assert_eq!(age(at("2026-11-27T19:15:00Z"), now()), "45 min");
        assert_eq!(age(at("2026-11-26T20:00:00Z"), now()), "24 h");
        assert_eq!(age(at("2026-11-20T20:00:00Z"), now()), "7 d");
        assert_eq!(age(at("2026-11-28T20:00:00Z"), now()), "under a minute");
    }

    #[test]
    fn the_mcp_address_follows_the_host_the_reader_used() {
        assert_eq!(
            mcp_url(Some("workshop.example"), None, "x"),
            "https://workshop.example/mcp"
        );
        assert_eq!(
            mcp_url(Some("127.0.0.1:8080"), None, "x"),
            "http://127.0.0.1:8080/mcp"
        );
        assert_eq!(
            mcp_url(Some("a.example"), Some("http"), "x"),
            "http://a.example/mcp"
        );
        assert_eq!(
            mcp_url(Some("evil\"><script>"), None, "127.0.0.1:8080"),
            "http://127.0.0.1:8080/mcp"
        );
    }

    #[tokio::test]
    async fn every_page_escapes_text_from_github() {
        let state = ready_state().await;
        for path in [
            "/",
            "/tasks",
            "/garden",
            "/reviews",
            "/leaderboard",
            "/connect",
        ] {
            let (status, html) = get(state.clone(), path).await;
            assert_eq!(status, StatusCode::OK, "{path}");
            assert!(!html.contains("<script>"), "{path}");
            assert!(!html.contains("<b>bold</b>"), "{path}");
            assert!(html.contains("/style.css"), "{path}");
            assert_eq!(
                html.matches("<script").count(),
                1,
                "{path} has only copy.js"
            );
        }
        let tasks = render_tasks(&state, state.snapshot().as_deref(), now()).into_string();
        let start = render_start(&state, state.snapshot().as_deref()).into_string();
        assert!(start.contains(
            "git clone https://github.com/LyraCoreProject/LyraCore &amp;&amp; cd LyraCore"
        ));
        assert!(start.contains("Follow the skill and fix one issue."));
        assert!(!start.contains("Kick does not interrupt"));
        assert!(tasks.contains(
            "<summary class=\"title\">&lt;script&gt;alert(1)&lt;/script&gt; Kick does not interrupt a channelled spell.</summary>"
        ));
        assert!(tasks.contains(r##"Held by ada-dev in <a href="https://github.com/LyraCoreProject/LyraCore/issues/2002">#2002</a> for 10 h"##));
        assert!(tasks.contains("Held by bob-dev in"));
        assert!(tasks.contains("for 30 h, claim lapsed"));
        assert!(tasks.contains("fast lane"));
        assert!(
            tasks.contains(r##"<h2 id="system-spells">spells <span class="count">2</span></h2>"##)
        );
        assert!(tasks.contains("scenario cast_and_interrupt, spell_direct_damage_outcomes"));
        assert!(tasks.contains("gh issue create --repo LyraCoreProject/LyraCore --title '/claim cast_and_interrupt-4f97715e' --body ''"));
        assert!(
            !tasks.contains("Flesh Eating Worm"),
            "a Data Gap is not a task"
        );
        assert!(
            !tasks.contains("drifts against"),
            "a gap that needs a real client is not a task"
        );
        let (_, reviews) = get(state.clone(), "/reviews").await;
        assert!(reviews.contains("a &lt;b&gt;bold&lt;/b&gt; claim about loot"));
        assert!(reviews.contains("cy-dev (commented)"));
        let (_, board) = get(state, "/leaderboard").await;
        assert!(board.contains("ada-dev</a> · 15 points"));
    }

    #[tokio::test]
    async fn before_the_first_refresh_pages_say_so_and_health_is_unavailable() {
        let state = Arc::new(AppState::new(test_config()));
        state.record_error(
            now(),
            "GET /x: 403 Forbidden".into(),
            crate::github::RequestCount { api: 1, raw: 0 },
        );
        let (status, _) = get(state.clone(), "/").await;
        assert_eq!(status, StatusCode::OK);
        let (status, html) = get(state.clone(), "/tasks").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(html.contains(NOT_READY));
        let (status, _) = get(state.clone(), "/connect").await;
        assert_eq!(status, StatusCode::OK);
        let (status, health) = get(state, "/healthz").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(health.contains("403 Forbidden"));
    }

    #[tokio::test]
    async fn a_failed_refresh_keeps_the_snapshot_and_reports_the_error() {
        let state = ready_state().await;
        state.record_error(
            now(),
            "GET /x: 502 Bad Gateway".into(),
            crate::github::RequestCount { api: 3, raw: 0 },
        );
        let (status, health) = get(state.clone(), "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert!(health.starts_with("ok\nsnapshot age: "));
        assert!(health.contains("502 Bad Gateway"));
        let (_, json) = get(state, "/snapshot.json").await;
        let snapshot: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(snapshot["schema_version"], 2);
        assert_eq!(snapshot["tasks"].as_array().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn health_names_the_ledger_files_it_could_not_read() {
        let (_, health) = get(ready_state().await, "/healthz").await;
        assert!(health.contains("last refresh requests: 22 API, 5 raw\n"));
        assert!(health.contains("ledger files: 4, unparsed: 1, waiting for raw: 1\n"));
        assert!(health.contains("unparsed: conformance/features/combat/broken.toml\n"));
        assert!(health.contains("ledger gaps: 8, data gaps: 2, needs real client: 1, tasks: 5\n"));
    }

    #[tokio::test]
    async fn tasks_json_lists_open_tasks_in_order_with_their_claim_command() {
        let (status, json) = get(ready_state().await, "/tasks.json").await;
        assert_eq!(status, StatusCode::OK);
        let view: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(view["schema_version"], 2);
        let tasks = view["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 5);
        assert_eq!(tasks[0]["id"], "cast_and_interrupt-4f97715e");
        assert_eq!(tasks[0]["claimed"], false);
        assert_eq!(tasks[0]["has_scenario"], true);
        assert_eq!(
            tasks[0]["claim_command"],
            "gh issue create --repo LyraCoreProject/LyraCore --title '/claim cast_and_interrupt-4f97715e' --body ''"
        );
        assert_eq!(tasks[3]["claimed"], true);
        assert_eq!(tasks[3]["claim_issue"], 2002);
        let state = Arc::new(AppState::new(test_config()));
        assert_eq!(
            get(state, "/tasks.json").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn the_garden_says_plainly_what_it_has_not_counted_without_a_token() {
        let (status, html) = get(ready_state().await, "/garden").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("3 closed claim issues are not counted yet"));
        assert!(html.contains("Workshop runs without a GitHub token"));
        assert!(html.contains("WORKSHOP_GITHUB_TOKEN"));
        assert!(html.contains("ended unfixed 2 times"));
        assert!(html.contains(r##"<td><a href="/tasks#system-creatures">creatures</a></td><td>2</td><td>1</td><td>1</td>"##));
    }

    #[test]
    fn a_headline_is_the_first_sentence_or_the_first_words() {
        assert_eq!(
            headline("One sentence. Then more."),
            ("One sentence.".to_owned(), true)
        );
        assert_eq!(headline("Only this"), ("Only this".to_owned(), false));
        let long = format!("{} end.", "word ".repeat(60));
        let (text, cut) = headline(&long);
        assert!(cut && text.ends_with("word…") && text.chars().count() <= 161);
        let (text, _) = headline(&"é".repeat(300));
        assert_eq!(text.chars().count(), 161);
    }

    #[tokio::test]
    async fn the_mcp_server_initializes_and_lists_its_tools_without_a_session() {
        let state = ready_state().await;
        let init = mcp(
            state.clone(),
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "test", "version": "1"}}}),
        )
        .await;
        assert_eq!(init["result"]["serverInfo"]["name"], "workshop");
        let list = mcp(
            state,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .await;
        let mut names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "get_task",
                "leaderboard",
                "list_reviews_wanted",
                "list_tasks",
                "my_work",
                "rules"
            ]
        );
    }

    #[tokio::test]
    async fn every_tool_result_with_github_text_says_it_is_data() {
        let state = ready_state().await;
        let calls = [
            ("list_tasks", json!({})),
            ("get_task", json!({"id": "cast_and_interrupt-52f5323c"})),
            ("my_work", json!({"login": "ada-dev"})),
            ("list_reviews_wanted", json!({"login": "dee-dev"})),
            ("leaderboard", json!({})),
        ];
        for (tool, arguments) in calls {
            let (is_error, text) = call(state.clone(), tool, arguments).await;
            assert!(!is_error, "{tool}: {text}");
            assert!(text.starts_with(crate::mcp::UNTRUSTED), "{tool}");
        }
    }

    #[tokio::test]
    async fn get_task_names_the_exact_claim_command() {
        let state = ready_state().await;
        let (_, text) = call(
            state.clone(),
            "get_task",
            json!({"id": "cast_and_interrupt-4f97715e"}),
        )
        .await;
        let view: Value = serde_json::from_str(text.split_once("\n\n").unwrap().1).unwrap();
        assert_eq!(
            view["claim_command"],
            "gh issue create --repo LyraCoreProject/LyraCore --title '/claim cast_and_interrupt-4f97715e' --body ''"
        );
        assert!(
            view["evidence"]
                .as_str()
                .unwrap()
                .contains("AddExecuteLogInfo")
        );
        assert_eq!(view["scenarios"][0], "cast_and_interrupt");
        assert_eq!(
            view["vmangos"]["symbol"],
            "WorldSession::HandleCastSpellOpcode"
        );
        assert!(view["note"].as_str().unwrap().contains("Closes #N"));
        let (is_error, _) = call(state.clone(), "get_task", json!({"id": "nope-00000000"})).await;
        assert!(is_error);
        let (_, text) = call(state.clone(), "list_tasks", json!({"unclaimed_only": true})).await;
        let view: Value = serde_json::from_str(text.split_once("\n\n").unwrap().1).unwrap();
        let ids: Vec<&str> = view["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|task| task["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            vec![
                "cast_and_interrupt-4f97715e",
                "creature_world_behavior-aaedc5f1"
            ]
        );
        let (_, text) = call(state, "list_tasks", json!({"system": "pets"})).await;
        let view: Value = serde_json::from_str(text.split_once("\n\n").unwrap().1).unwrap();
        assert_eq!(view["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(view["tasks"][0]["claim"]["holder"], "Mathih13");
    }

    #[tokio::test]
    async fn my_work_finds_a_login_in_any_case() {
        let state = ready_state().await;
        let (_, text) = call(state, "my_work", json!({"login": "ADA-DEV"})).await;
        let view: Value = serde_json::from_str(text.split_once("\n\n").unwrap().1).unwrap();
        assert_eq!(view["claims"][0]["id"], "cast_and_interrupt-52f5323c");
        assert_eq!(view["claims"][0]["claim"]["issue"], 2002);
        assert_eq!(view["open_pull_requests"][0]["number"], 3001);
        assert_eq!(view["open_pull_requests"][0]["lane"], "fast lane");
    }
}
