use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::{Notify, RwLock};

use rap_protocol::{CallbackClient, RapInvocation, send_subscription_event};

// ── Subscription types ──

#[derive(Debug, Clone, Deserialize)]
pub struct SubscribeArgs {
    pub owner: String,
    pub repo: String,
    pub event_type: Option<String>,
    pub sha: Option<String>,
    pub pr_number: Option<u64>,
    pub issue_number: Option<u64>,
    pub action: Option<String>,
    pub branch: Option<String>,
    pub actor: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Subscription {
    pub tool_call_id: String,
    pub call_id: Option<String>,
    pub callback_url: String,
    pub group_id: String,
    pub filters: Filters,
}

#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub event_type: Option<String>,
    pub sha: Option<String>,
    pub pr_number: Option<u64>,
    pub issue_number: Option<u64>,
    pub action: Option<String>,
    pub branch: Option<String>,
    pub actor: Option<String>,
}

impl Filters {
    fn from_args(args: &SubscribeArgs) -> Self {
        Self {
            event_type: args.event_type.clone(),
            sha: args.sha.clone(),
            pr_number: args.pr_number,
            issue_number: args.issue_number,
            action: args.action.clone(),
            branch: args.branch.clone(),
            actor: args.actor.clone(),
        }
    }
}

// ── Per-repo polling state ──

#[derive(Debug)]
struct RepoState {
    /// Subscriptions keyed by tool_call_id
    subscriptions: HashMap<String, Subscription>,
    /// ETag from last poll (for conditional requests)
    etag: Option<String>,
    /// Poll interval from GitHub's X-Poll-Interval header (default 60s)
    poll_interval: Duration,
    /// ID of the most recent event we've seen; events at or before this are skipped.
    last_seen_id: Option<String>,
    /// False until the first successful poll (we skip delivering historical events).
    initialized: bool,
}

// ── Poller ──

pub struct Poller<C: CallbackClient> {
    /// Map from "owner/repo" -> RepoState
    repos: Arc<RwLock<HashMap<String, RepoState>>>,
    /// Notified when subscriptions go from 0 -> 1
    wake: Arc<Notify>,
    callback_client: Arc<C>,
    http: reqwest::Client,
}

impl<C: CallbackClient> Poller<C> {
    pub fn new(callback_client: C, github_token: Option<String>) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "Accept",
            "application/vnd.github+json"
                .parse()
                .expect("bug: invalid Accept header"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            "2022-11-28"
                .parse()
                .expect("bug: invalid API version header"),
        );
        if let Some(ref token) = github_token {
            headers.insert(
                "Authorization",
                format!("Bearer {token}")
                    .parse()
                    .expect("bug: invalid Authorization header"),
            );
        }

        let http = reqwest::Client::builder()
            .user_agent("rap-github-event-poller")
            .default_headers(headers)
            .build()
            .expect("failed to build reqwest client");

        Self {
            repos: Arc::new(RwLock::new(HashMap::new())),
            wake: Arc::new(Notify::new()),
            callback_client: Arc::new(callback_client),
            http,
        }
    }

    /// Add a subscription. Returns a human-readable confirmation.
    pub async fn subscribe(&self, invocation: &RapInvocation) -> String {
        let args: SubscribeArgs = match serde_json::from_value(invocation.arguments.clone()) {
            Ok(a) => a,
            Err(e) => return format!("Error: invalid arguments: {e}"),
        };

        let key = format!("{}/{}", args.owner, args.repo);
        let filters = Filters::from_args(&args);
        let sub = Subscription {
            tool_call_id: invocation.id.clone(),
            call_id: invocation.call_id.clone(),
            callback_url: invocation.callback_url.clone(),
            group_id: invocation.group_id.clone(),
            filters: filters.clone(),
        };

        let was_empty = {
            let repos = self.repos.read().await;
            repos.values().all(|r| r.subscriptions.is_empty())
        };

        {
            let mut repos = self.repos.write().await;
            let state = repos.entry(key).or_insert_with(|| RepoState {
                subscriptions: HashMap::new(),
                etag: None,
                poll_interval: Duration::from_secs(60),
                last_seen_id: None,
                initialized: false,
            });
            state.subscriptions.insert(invocation.id.clone(), sub);
        }

        if was_empty {
            self.wake.notify_one();
        }

        let filter_desc = describe_filters(&filters);
        format!(
            "Subscribed to events for {}/{}. {filter_desc}",
            args.owner, args.repo
        )
    }

    /// Remove a subscription by tool_call_id.
    pub async fn cancel(&self, tool_call_id: &str) {
        let mut repos = self.repos.write().await;
        for state in repos.values_mut() {
            state.subscriptions.remove(tool_call_id);
        }
        // Remove repos with no subscriptions
        repos.retain(|_, state| !state.subscriptions.is_empty());
    }

    /// Run the polling loop. This never returns.
    pub async fn run(&self) -> ! {
        loop {
            // Wait until there's at least one subscription
            {
                let repos = self.repos.read().await;
                if repos.values().all(|r| r.subscriptions.is_empty()) {
                    drop(repos);
                    tracing::info!("no active subscriptions, sleeping until woken");
                    self.wake.notified().await;
                    continue;
                }
            }

            // Snapshot the repos we need to poll and their intervals
            let snapshot: Vec<(String, Duration, Option<String>)> = {
                let repos = self.repos.read().await;
                repos
                    .iter()
                    .filter(|(_, s)| !s.subscriptions.is_empty())
                    .map(|(k, s)| (k.clone(), s.poll_interval, s.etag.clone()))
                    .collect()
            };

            for (repo_key, _interval, etag) in &snapshot {
                self.poll_repo(repo_key, etag.as_deref()).await;
            }

            // Sleep for the minimum poll interval across all repos
            let min_interval = {
                let repos = self.repos.read().await;
                repos
                    .values()
                    .filter(|s| !s.subscriptions.is_empty())
                    .map(|s| s.poll_interval)
                    .min()
                    .unwrap_or(Duration::from_secs(60))
            };

            tracing::debug!("sleeping for {}s before next poll", min_interval.as_secs());
            tokio::time::sleep(min_interval).await;
        }
    }

    async fn poll_repo(&self, repo_key: &str, etag: Option<&str>) {
        let url = format!("https://api.github.com/repos/{repo_key}/events?per_page=30");

        let mut req = self.http.get(&url);
        if let Some(etag) = etag {
            req = req.header("If-None-Match", etag);
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("failed to poll {repo_key}: {e}");
                return;
            }
        };

        // Update poll interval from header
        let new_interval = resp
            .headers()
            .get("x-poll-interval")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs);

        let new_etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_owned());

        let status = resp.status();

        // Update repo state with new etag/interval
        {
            let mut repos = self.repos.write().await;
            if let Some(state) = repos.get_mut(repo_key) {
                if let Some(interval) = new_interval {
                    state.poll_interval = interval;
                }
                if let Some(ref etag) = new_etag {
                    state.etag = Some(etag.clone());
                }
            }
        }

        if status == reqwest::StatusCode::NOT_MODIFIED {
            tracing::debug!("{repo_key}: 304 Not Modified");
            return;
        }

        if !status.is_success() {
            tracing::warn!("{repo_key}: GitHub API returned {status}");
            return;
        }

        let events: Vec<GitHubEvent> = match resp.json().await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("{repo_key}: failed to parse events: {e}");
                return;
            }
        };

        if events.is_empty() {
            return;
        }

        // Read current state
        let (was_initialized, last_seen_id) = {
            let repos = self.repos.read().await;
            repos
                .get(repo_key)
                .map(|s| (s.initialized, s.last_seen_id.clone()))
                .unwrap_or((false, None))
        };

        // Record the newest event ID (events are newest-first)
        let newest_id = events[0].id.clone();
        {
            let mut repos = self.repos.write().await;
            if let Some(state) = repos.get_mut(repo_key) {
                state.last_seen_id = Some(newest_id);
                state.initialized = true;
            }
        }

        // On first poll, skip delivering historical events
        if !was_initialized {
            tracing::info!(
                "{repo_key}: first poll, recorded baseline ({} events), skipping delivery",
                events.len()
            );
            return;
        }

        // Filter to only new events (stop at last_seen_id)
        let new_events: Vec<&GitHubEvent> = events
            .iter()
            .take_while(|e| Some(&e.id) != last_seen_id.as_ref())
            .collect();

        if new_events.is_empty() {
            tracing::debug!("{repo_key}: no new events since last poll");
            return;
        }

        tracing::debug!("{repo_key}: {} new events", new_events.len());

        // Match events against subscriptions
        let subs: Vec<Subscription> = {
            let repos = self.repos.read().await;
            repos
                .get(repo_key)
                .map(|s| s.subscriptions.values().cloned().collect())
                .unwrap_or_default()
        };

        // Deliver in chronological order (reverse since API returns newest-first)
        for event in new_events.into_iter().rev() {
            let event_data = extract_event_data(event);
            for sub in &subs {
                if matches_filters(&sub.filters, &event_data) {
                    let text = serde_json::to_string_pretty(&serde_json::json!({
                        "event_type": event.r#type,
                        "payload": event.payload,
                    }))
                    .unwrap_or_default();

                    send_subscription_event(
                        self.callback_client.as_ref(),
                        &sub.callback_url,
                        sub.group_id.clone(),
                        sub.tool_call_id.clone(),
                        &text,
                        false,
                        false,
                    )
                    .await;
                    tracing::info!(
                        "sent event {} to subscription {}",
                        event.r#type,
                        sub.tool_call_id
                    );
                }
            }
        }
    }
}

// ── GitHub Events API types ──

#[derive(Debug, Deserialize)]
struct GitHubEvent {
    pub id: String,
    pub r#type: String,
    pub payload: serde_json::Value,
}

// ── Event data extraction (mirrors the webhook receiver logic) ──

#[derive(Debug, Default)]
struct EventData {
    event_type: Option<String>,
    action: Option<String>,
    actor: Option<String>,
    sha: Option<String>,
    pr_number: Option<u64>,
    issue_number: Option<u64>,
    branch: Option<String>,
    head_branch: Option<String>,
    base_branch: Option<String>,
}

fn extract_event_data(event: &GitHubEvent) -> EventData {
    let p = &event.payload;
    let mut d = EventData {
        event_type: Some(event.r#type.clone()),
        action: p.get("action").and_then(|v| v.as_str()).map(String::from),
        actor: p
            .pointer("/sender/login")
            .or_else(|| p.pointer("/actor/login"))
            .and_then(|v| v.as_str())
            .map(String::from),
        ..Default::default()
    };

    // SHA extraction chain
    d.sha = p
        .get("head_sha")
        .or_else(|| p.get("sha"))
        .or_else(|| p.get("after"))
        .or_else(|| p.pointer("/pull_request/head/sha"))
        .or_else(|| p.pointer("/check_run/head_sha"))
        .or_else(|| p.pointer("/check_suite/head_sha"))
        .or_else(|| p.pointer("/workflow_run/head_sha"))
        .and_then(|v| v.as_str())
        .map(String::from);

    // PR number
    d.pr_number = p
        .pointer("/pull_request/number")
        .or_else(|| {
            if p.pointer("/issue/pull_request").is_some() {
                p.pointer("/issue/number")
            } else {
                None
            }
        })
        .and_then(|v| v.as_u64());

    // Issue number
    if d.pr_number.is_none() {
        d.issue_number = p.pointer("/issue/number").and_then(|v| v.as_u64());
    }

    // Branch
    d.branch = p
        .get("ref")
        .and_then(|v| v.as_str())
        .map(|s| s.strip_prefix("refs/heads/").unwrap_or(s).to_owned());
    d.head_branch = p
        .pointer("/pull_request/head/ref")
        .and_then(|v| v.as_str())
        .map(String::from);
    d.base_branch = p
        .pointer("/pull_request/base/ref")
        .and_then(|v| v.as_str())
        .map(String::from);

    d
}

fn matches_filters(filters: &Filters, data: &EventData) -> bool {
    if let Some(ref et) = filters.event_type
        && data.event_type.as_deref() != Some(et)
    {
        return false;
    }
    if let Some(ref sha) = filters.sha
        && data.sha.as_deref() != Some(sha)
    {
        return false;
    }
    if let Some(pr) = filters.pr_number
        && data.pr_number != Some(pr)
    {
        return false;
    }
    if let Some(issue) = filters.issue_number
        && data.issue_number != Some(issue)
    {
        return false;
    }
    if let Some(ref action) = filters.action
        && data.action.as_deref() != Some(action)
    {
        return false;
    }
    if let Some(ref actor) = filters.actor
        && data.actor.as_deref() != Some(actor)
    {
        return false;
    }
    if let Some(ref branch) = filters.branch
        && data.branch.as_deref() != Some(branch)
        && data.head_branch.as_deref() != Some(branch)
        && data.base_branch.as_deref() != Some(branch)
    {
        return false;
    }
    true
}

fn describe_filters(f: &Filters) -> String {
    let mut parts = Vec::new();
    if let Some(ref v) = f.event_type {
        parts.push(format!("event_type={v}"));
    }
    if let Some(ref v) = f.sha {
        parts.push(format!("sha={v}"));
    }
    if let Some(v) = f.pr_number {
        parts.push(format!("pr_number={v}"));
    }
    if let Some(v) = f.issue_number {
        parts.push(format!("issue_number={v}"));
    }
    if let Some(ref v) = f.action {
        parts.push(format!("action={v}"));
    }
    if let Some(ref v) = f.branch {
        parts.push(format!("branch={v}"));
    }
    if let Some(ref v) = f.actor {
        parts.push(format!("actor={v}"));
    }
    if parts.is_empty() {
        "No filters (will match all events).".to_owned()
    } else {
        format!("Filters: {}", parts.join(", "))
    }
}
