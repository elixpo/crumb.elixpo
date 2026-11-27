//! Read-only GitHub access: repositories, issues, pull requests, search,
//! files, releases, and Actions runs.
//!
//! Uses the same credentials as `github_issue` (`GH_TOKEN`, `GITHUB_TOKEN`, or
//! the `gh` CLI login) and otherwise reads public data anonymously. Only GET
//! requests to GitHub's fixed API origin are made, and credentials never
//! appear in tool output.

use std::fmt::Write as _;
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use crumb_agent::{
    CancellationToken, RiskClass, ToolDescriptor, ToolHandler, ToolHost, ToolOutput, ToolTransport,
};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use url::Url;

use crate::bounded_text;
use crate::github::{GitHubIssueLimits, github_token, valid_slug};

pub const GITHUB: &str = "github";
const OPERATIONS: [&str; 8] = [
    "repo",
    "issues",
    "pulls",
    "pull_files",
    "search",
    "file",
    "releases",
    "runs",
];
const DEFAULT_LIMIT: u64 = 20;
const MAX_LIMIT: u64 = 50;
const JSON: &str = "application/vnd.github+json";
const RAW: &str = "application/vnd.github.raw+json";

/// Registers the read-only `github` tool.
///
/// # Errors
///
/// Returns an error when limits are invalid or the tool cannot be registered.
pub fn register_github_read_tool(host: &mut ToolHost, limits: GitHubIssueLimits) -> Result<()> {
    if limits.timeout.is_zero() || limits.max_output_bytes == 0 {
        bail!("GitHub limits must be positive");
    }
    host.register(
        ToolDescriptor {
            name: GITHUB.to_owned(),
            description: "Read GitHub without changing anything. operation: repo (metadata), \
                          issues or pulls (list, by state), pull_files (files a PR changes), \
                          search (issues and PRs, GitHub search syntax), file (contents at a \
                          ref, or a directory listing), releases, runs (Actions workflow runs). \
                          Uses existing GitHub authentication when available."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "operation": {"type": "string", "enum": OPERATIONS},
                    "repo": {"type": "string", "description": "owner/name; required except for search"},
                    "number": {"type": "integer", "minimum": 1, "description": "pull request number for pull_files"},
                    "query": {"type": "string", "description": "search query"},
                    "path": {"type": "string", "description": "file or directory path for file"},
                    "ref": {"type": "string", "description": "branch, tag, or commit for file"},
                    "state": {"type": "string", "enum": ["open", "closed", "all"]},
                    "branch": {"type": "string", "description": "filter runs by branch"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_LIMIT}
                },
                "required": ["operation"],
                "additionalProperties": false
            }),
            risk: RiskClass::NetworkAccess,
            transport: ToolTransport::Native,
        },
        Arc::new(GitHubRead { limits }),
    )
}

struct GitHubRead {
    limits: GitHubIssueLimits,
}

impl ToolHandler for GitHubRead {
    fn call(&self, arguments: &Value, cancellation: &CancellationToken) -> Result<ToolOutput> {
        let request = plan(arguments)?;
        let limits = self.limits;
        let cancellation = cancellation.clone();
        thread::Builder::new()
            .name("crumb-github".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("failed to start GitHub runtime")?;
                runtime.block_on(run(request, limits, &cancellation))
            })
            .context("failed to start GitHub worker")?
            .join()
            .map_err(|_| anyhow!("GitHub worker failed"))?
    }
}

/// One validated API request and how to render its answer.
#[derive(Debug)]
struct Request {
    operation: &'static str,
    url: Url,
    accept: &'static str,
    limit: usize,
}

fn plan(arguments: &Value) -> Result<Request> {
    let operation = text(arguments, "operation")?
        .and_then(|name| OPERATIONS.into_iter().find(|known| *known == name))
        .context("operation must be one of repo, issues, pulls, pull_files, search, file, releases, runs")?;
    let limit = arguments
        .get("limit")
        .map(|value| value.as_u64().context("limit must be a positive integer"))
        .transpose()?
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);
    let per_page = limit.to_string();
    let mut url = Url::parse("https://api.github.com/").context("GitHub API origin")?;
    let mut accept = JSON;
    if operation == "search" {
        let query = text(arguments, "query")?.context("search needs a query")?;
        url.set_path("search/issues");
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("per_page", &per_page);
    } else {
        let (owner, name) = repository(arguments)?;
        let mut segments = vec!["repos", owner, name];
        let state = text(arguments, "state")?.unwrap_or("open");
        if !matches!(state, "open" | "closed" | "all") {
            bail!("state must be open, closed, or all");
        }
        let mut query = vec![("per_page", per_page)];
        let number;
        match operation {
            "repo" => {}
            "issues" | "pulls" => {
                segments.push(operation);
                query.push(("state", state.to_owned()));
                if operation == "issues" {
                    // This endpoint counts pull requests too and they are
                    // filtered out afterwards, so ask for a full page.
                    "100".clone_into(&mut query[0].1);
                }
            }
            "pull_files" => {
                number = arguments
                    .get("number")
                    .and_then(Value::as_u64)
                    .filter(|number| *number > 0)
                    .context("pull_files needs a pull request number")?
                    .to_string();
                segments.extend(["pulls", number.as_str(), "files"]);
            }
            "file" => {
                accept = RAW;
                segments.push("contents");
                segments.extend(file_path(text(arguments, "path")?.unwrap_or(""))?);
                query.clear();
                if let Some(reference) = text(arguments, "ref")? {
                    query.push(("ref", git_ref(reference)?.to_owned()));
                }
            }
            "releases" => segments.push("releases"),
            _ => {
                segments.extend(["actions", "runs"]);
                if let Some(branch) = text(arguments, "branch")? {
                    query.push(("branch", git_ref(branch)?.to_owned()));
                }
            }
        }
        url.path_segments_mut()
            .map_err(|()| anyhow!("GitHub API origin cannot take a path"))?
            .pop_if_empty()
            .extend(segments);
        if !query.is_empty() && operation != "repo" {
            url.query_pairs_mut().extend_pairs(query);
        }
    }
    Ok(Request {
        operation,
        url,
        accept,
        limit: usize::try_from(limit).unwrap_or(usize::MAX),
    })
}

fn text<'a>(arguments: &'a Value, name: &str) -> Result<Option<&'a str>> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let value = value
                .as_str()
                .with_context(|| format!("{name} must be a string"))?
                .trim();
            Ok((!value.is_empty()).then_some(value))
        }
    }
}

fn repository(arguments: &Value) -> Result<(&str, &str)> {
    let repo = text(arguments, "repo")?.context("this operation needs repo as owner/name")?;
    let repo = repo
        .strip_prefix("https://github.com/")
        .unwrap_or(repo)
        .trim_end_matches('/');
    let (owner, name) = repo.split_once('/').context("repo must be owner/name")?;
    if !valid_slug(owner) || !valid_slug(name) || name == "." || name == ".." {
        bail!("repo must be owner/name");
    }
    Ok((owner, name))
}

/// Splits a repository path into checked segments; `..` and absolute paths
/// are refused so the request cannot leave the repository's contents API.
fn file_path(path: &str) -> Result<Vec<&str>> {
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments
        .iter()
        .any(|segment| matches!(*segment, "." | "..") || segment.contains('\\'))
    {
        bail!("path must stay inside the repository");
    }
    Ok(segments)
}

fn git_ref(reference: &str) -> Result<&str> {
    let valid = reference.len() <= 200
        && !reference.contains("..")
        && reference
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'));
    if valid {
        Ok(reference)
    } else {
        bail!("ref and branch may contain only letters, digits, and - _ . /")
    }
}

async fn run(
    request: Request,
    limits: GitHubIssueLimits,
    cancellation: &CancellationToken,
) -> Result<ToolOutput> {
    let token = github_token(cancellation);
    run_with_token(request, limits, cancellation, token).await
}

async fn run_with_token(
    request: Request,
    limits: GitHubIssueLimits,
    cancellation: &CancellationToken,
    token: Option<String>,
) -> Result<ToolOutput> {
    let client = Client::builder()
        .timeout(limits.timeout)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("crumb-cli")
        .build()
        .context("failed to construct GitHub client")?;
    let mut builder = client
        .get(request.url.clone())
        .header(reqwest::header::ACCEPT, request.accept)
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(token) = token.as_deref() {
        builder = builder.bearer_auth(token);
    }
    let mut response = tokio::select! {
        result = builder.send() => result.context("GitHub request failed")?,
        () = cancellation.cancelled() => bail!("GitHub request cancelled"),
    };
    let status = response.status();
    if let Some(message) = status_message(status, token.is_some()) {
        return Ok(ToolOutput::error(message));
    }
    let decode_limit = limits.max_output_bytes.saturating_mul(8).max(64 * 1024);
    let mut bytes = Vec::new();
    while let Some(chunk) = tokio::select! {
        result = response.chunk() => result.context("GitHub response failed")?,
        () = cancellation.cancelled() => bail!("GitHub request cancelled"),
    } {
        if bytes.len().saturating_add(chunk.len()) > decode_limit {
            return Ok(ToolOutput::error(
                "GitHub response exceeded the configured size limit; narrow the request",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(ToolOutput::text(bounded_text(
        render(&request, &bytes)?,
        limits.max_output_bytes,
    )))
}

fn status_message(status: StatusCode, authenticated: bool) -> Option<String> {
    if status.is_success() {
        return None;
    }
    Some(match status {
        StatusCode::UNAUTHORIZED => {
            "GitHub authentication failed. Run `gh auth login`, then retry.".to_owned()
        }
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => {
            "GitHub refused the request (permissions or rate limit). Signing in with \
             `gh auth login` raises the rate limit."
                .to_owned()
        }
        StatusCode::NOT_FOUND if authenticated => {
            "Not found, or not visible to the signed-in GitHub account.".to_owned()
        }
        StatusCode::NOT_FOUND => {
            "Not found. If it is private, run `gh auth login`, then retry.".to_owned()
        }
        status => format!("GitHub API returned HTTP {}", status.as_u16()),
    })
}

fn render(request: &Request, bytes: &[u8]) -> Result<String> {
    if request.operation == "file" {
        // A directory comes back as a JSON listing even when raw is requested.
        if let Ok(Value::Array(entries)) = serde_json::from_slice::<Value>(bytes) {
            return Ok(lines(&entries, request.limit, |entry| {
                format!("{}\t{}", field(entry, "/type"), field(entry, "/path"))
            }));
        }
        return Ok(String::from_utf8_lossy(bytes).into_owned());
    }
    let value: Value = serde_json::from_slice(bytes).context("GitHub returned invalid JSON")?;
    let empty = Vec::new();
    let items = value.as_array().unwrap_or(&empty);
    Ok(match request.operation {
        "repo" => render_repo(&value),
        "issues" => {
            // The issues endpoint also returns pull requests; list them under pulls.
            let issues = items
                .iter()
                .filter(|item| item.get("pull_request").is_none())
                .cloned()
                .collect::<Vec<_>>();
            lines(&issues, request.limit, render_item)
        }
        "pulls" => lines(items, request.limit, render_item),
        "pull_files" => lines(items, request.limit, |file| {
            format!(
                "{}\t+{} -{}\t{}",
                field(file, "/status"),
                field(file, "/additions"),
                field(file, "/deletions"),
                field(file, "/filename")
            )
        }),
        "search" => {
            let empty = Vec::new();
            let found = value
                .get("items")
                .and_then(Value::as_array)
                .unwrap_or(&empty);
            format!(
                "{} results\n{}",
                field(&value, "/total_count"),
                lines(found, request.limit, |item| {
                    let repo = field(item, "/repository_url")
                        .rsplit('/')
                        .take(2)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join("/");
                    format!("{repo} {}", render_item(item))
                })
            )
        }
        "releases" => lines(items, request.limit, |release| {
            let mut line = format!(
                "{}\t{}\t{}",
                field(release, "/tag_name"),
                field(release, "/name"),
                field(release, "/published_at")
            );
            if release.get("prerelease") == Some(&Value::Bool(true)) {
                line.push_str("\tprerelease");
            }
            line
        }),
        _ => {
            let empty = Vec::new();
            let runs = value
                .get("workflow_runs")
                .and_then(Value::as_array)
                .unwrap_or(&empty);
            lines(runs, request.limit, |run| {
                let result = match field(run, "/conclusion").as_str() {
                    "" => field(run, "/status"),
                    conclusion => conclusion.to_owned(),
                };
                format!(
                    "{}\t{result}\t{}\t{}\t{}",
                    field(run, "/name"),
                    field(run, "/head_branch"),
                    field(run, "/created_at"),
                    field(run, "/html_url")
                )
            })
        }
    })
}

fn render_repo(repo: &Value) -> String {
    let topics = repo
        .get("topics")
        .and_then(Value::as_array)
        .map(|topics| {
            topics
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    format!(
        "{}: {}\nDefault branch: {} · Stars: {} · Forks: {} · Open issues and PRs: {}\nLicense: {} · Visibility: {} · Last push: {}\nTopics: {}\nURL: {}",
        field(repo, "/full_name"),
        field(repo, "/description"),
        field(repo, "/default_branch"),
        field(repo, "/stargazers_count"),
        field(repo, "/forks_count"),
        field(repo, "/open_issues_count"),
        field(repo, "/license/spdx_id"),
        field(repo, "/visibility"),
        field(repo, "/pushed_at"),
        if topics.is_empty() { "none" } else { &topics },
        field(repo, "/html_url"),
    )
}

fn render_item(item: &Value) -> String {
    let kind = if item.get("pull_request").is_some() || item.get("head").is_some() {
        "PR "
    } else {
        ""
    };
    let labels = item
        .get("labels")
        .and_then(Value::as_array)
        .map(|labels| {
            labels
                .iter()
                .filter_map(|label| label.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    let mut line = format!(
        "{kind}#{} [{}] {} (@{}, updated {})",
        field(item, "/number"),
        field(item, "/state"),
        field(item, "/title"),
        field(item, "/user/login"),
        field(item, "/updated_at"),
    );
    if !labels.is_empty() {
        let _ = write!(line, " [{labels}]");
    }
    line
}

fn lines(items: &[Value], limit: usize, render: impl Fn(&Value) -> String) -> String {
    if items.is_empty() {
        return "none".to_owned();
    }
    items
        .iter()
        .take(limit)
        .map(render)
        .collect::<Vec<_>>()
        .join("\n")
}

fn field(value: &Value, pointer: &str) -> String {
    match value.pointer(pointer) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::Duration;

    use crumb_agent::CancellationToken;
    use reqwest::StatusCode;
    use serde_json::json;

    use super::{
        GitHubIssueLimits, Request, plan, render, run_with_token, status_message,
    };

    #[derive(Clone)]
    struct ResponsePlan {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        gate_headers: bool,
        stream_first_chunk: Option<usize>,
    }

    impl ResponsePlan {
        fn json(status: u16, body: impl Into<Vec<u8>>) -> Self {
            Self {
                status,
                headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
                body: body.into(),
                gate_headers: false,
                stream_first_chunk: None,
            }
        }

        fn redirect(status: u16) -> Self {
            Self {
                status,
                headers: vec![
                    ("Location".to_owned(), "/redirect-target".to_owned()),
                    ("Content-Length".to_owned(), "0".to_owned()),
                ],
                body: Vec::new(),
                gate_headers: false,
                stream_first_chunk: None,
            }
        }
    }

    struct HttpFixture {
        url: String,
        address: String,
        calls: Arc<AtomicUsize>,
        requests: mpsc::Receiver<String>,
        chunks: mpsc::Receiver<()>,
        header_gate: Option<mpsc::SyncSender<()>>,
        body_gate: Option<mpsc::SyncSender<()>>,
        stopped: Arc<std::sync::atomic::AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl HttpFixture {
        fn start(plan: ResponsePlan) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
            let address = listener.local_addr().expect("fixture address");
            let address_text = address.to_string();
            let calls = Arc::new(AtomicUsize::new(0));
            let worker_calls = Arc::clone(&calls);
            let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let worker_stopped = Arc::clone(&stopped);
            let (request_sender, requests) = mpsc::sync_channel(8);
            let (chunk_sender, chunks) = mpsc::sync_channel(8);

            let (header_gate, header_gate_receiver) = if plan.gate_headers {
                let (sender, receiver) = mpsc::sync_channel(1);
                (Some(sender), Some(receiver))
            } else {
                (None, None)
            };
            let (body_gate, body_gate_receiver) = if plan.stream_first_chunk.is_some() {
                let (sender, receiver) = mpsc::sync_channel(1);
                (Some(sender), Some(receiver))
            } else {
                (None, None)
            };

            let worker = thread::spawn(move || {
                while !worker_stopped.load(Ordering::Acquire) {
                    let Ok((mut stream, _)) = listener.accept() else {
                        break;
                    };
                    if worker_stopped.load(Ordering::Acquire) {
                        break;
                    }
                    worker_calls.fetch_add(1, Ordering::Relaxed);
                    let Some(request) = read_request_head(&mut stream) else {
                        continue;
                    };
                    let _ = request_sender.send(request);

                    if let Some(gate) = header_gate_receiver.as_ref() {
                        let _ = gate.recv();
                    }

                    let reason = match plan.status {
                        200 => "OK",
                        302 => "Found",
                        401 => "Unauthorized",
                        403 => "Forbidden",
                        404 => "Not Found",
                        429 => "Too Many Requests",
                        status => "Error",
                    };
                    let mut headers = plan.headers.clone();
                    if !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("Content-Length")) {
                        headers.push((
                            "Content-Length".to_owned(),
                            plan.body.len().to_string(),
                        ));
                    }
                    headers.push(("Connection".to_owned(), "close".to_owned()));
                    let mut response = format!(
                        "HTTP/1.1 {} {}\r\n",
                        plan.status,
                        reason
                    );
                    for (name, value) in &headers {
                        response.push_str(&format!("{name}: {value}\r\n"));
                    }
                    response.push_str("\r\n");
                    let _ = stream.write_all(response.as_bytes());

                    if let Some(split) = plan.stream_first_chunk {
                        let split = split.min(plan.body.len());
                        let _ = stream.write_all(&plan.body[..split]);
                        let _ = stream.flush();
                        let _ = chunk_sender.send(());
                        if let Some(gate) = body_gate_receiver.as_ref() {
                            let _ = gate.recv();
                        }
                        let _ = stream.write_all(&plan.body[split..]);
                        let _ = stream.flush();
                    } else {
                        let _ = stream.write_all(&plan.body);
                        let _ = stream.flush();
                    }
                }
            });

            Self {
                url: format!("http://{address}/"),
                address: address_text,
                calls,
                requests,
                chunks,
                header_gate,
                body_gate,
                stopped,
                worker: Some(worker),
            }
        }
    }

    impl Drop for HttpFixture {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::Release);
            if let Some(gate) = &self.header_gate {
                let _ = gate.send(());
            }
            if let Some(gate) = &self.body_gate {
                let _ = gate.send(());
            }
            if let Ok(stream) = TcpStream::connect(&self.address) {
                drop(stream);
            }
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn read_request_head(stream: &mut TcpStream) -> Option<String> {
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .ok()?;
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).ok()?;
            request.push(byte[0]);
            if request.len() > 16 * 1024 {
                return None;
            }
        }
        String::from_utf8(request).ok()
    }

    fn request(operation: &str, url: &str) -> Request {
        match operation {
            "repo" => Request {
                operation,
                url: url.parse().expect("valid fixture URL"),
                accept: "application/vnd.github+json",
                limit: 20,
            },
            "file" => Request {
                operation,
                url: url.parse().expect("valid fixture URL"),
                accept: "application/vnd.github.raw+json",
                limit: 20,
            },
            _ => panic!("unsupported fixture operation"),
        }
    }

    fn limits(timeout: Duration, max_output_bytes: usize) -> GitHubIssueLimits {
        GitHubIssueLimits {
            timeout,
            max_output_bytes,
        }
    }

    fn execute(request: Request, fixture_limit: GitHubIssueLimits, token: Option<&str>, cancellation: &CancellationToken) -> Result<crumb_agent::ToolOutput, anyhow::Error> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("fixture runtime");
        runtime.block_on(run_with_token(
            request,
            fixture_limit,
            cancellation,
            token.map(str::to_owned),
        ))
    }

    fn url(arguments: &serde_json::Value) -> String {
        plan(arguments).expect("valid request").url.to_string()
    }

    #[test]
    fn requests_map_to_fixed_read_only_endpoints() {
        assert_eq!(
            url(&json!({"operation": "repo", "repo": "elixpo/crumb.elixpo"})),
            "https://api.github.com/repos/elixpo/crumb.elixpo"
        );
        assert_eq!(
            url(&json!({"operation": "issues", "repo": "o/r", "state": "closed", "limit": 5})),
            "https://api.github.com/repos/o/r/issues?per_page=100&state=closed"
        );
        assert_eq!(
            url(&json!({"operation": "pull_files", "repo": "o/r", "number": 12})),
            "https://api.github.com/repos/o/r/pulls/12/files?per_page=20"
        );
        assert_eq!(
            url(&json!({"operation": "search", "query": "repo:o/r is:open label:bug"})),
            "https://api.github.com/search/issues?q=repo%3Ao%2Fr+is%3Aopen+label%3Abug&per_page=20"
        );
        assert_eq!(
            url(&json!({"operation": "file", "repo": "o/r", "path": "src/main.rs", "ref": "v1.0"})),
            "https://api.github.com/repos/o/r/contents/src/main.rs?ref=v1.0"
        );
        assert_eq!(
            url(&json!({"operation": "runs", "repo": "o/r", "branch": "main", "limit": 99})),
            "https://api.github.com/repos/o/r/actions/runs?per_page=50&branch=main"
        );
    }

    #[test]
    fn unsafe_or_incomplete_requests_are_refused() {
        for arguments in [
            json!({"operation": "delete", "repo": "o/r"}),
            json!({"operation": "repo"}),
            json!({"operation": "repo", "repo": "o/.."}),
            json!({"operation": "repo", "repo": "o"}),
            json!({"operation": "search"}),
            json!({"operation": "pull_files", "repo": "o/r"}),
            json!({"operation": "file", "repo": "o/r", "path": "../secrets"}),
            json!({"operation": "file", "repo": "o/r", "path": "a", "ref": "x..y"}),
            json!({"operation": "issues", "repo": "o/r", "state": "merged"}),
        ] {
            assert!(plan(&arguments).is_err(), "{arguments}");
        }
    }

    #[test]
    fn issue_listings_leave_out_pull_requests_and_respect_the_limit() {
        let request = Request {
            operation: "issues",
            ..plan(&json!({"operation": "issues", "repo": "o/r", "limit": 1})).unwrap()
        };
        let body = json!([
            {"number": 3, "state": "open", "title": "A PR", "pull_request": {}, "user": {"login": "a"}},
            {"number": 2, "state": "open", "title": "Bug", "user": {"login": "b"},
             "labels": [{"name": "bug"}], "updated_at": "2026-10-27"},
            {"number": 1, "state": "open", "title": "Old", "user": {"login": "c"}}
        ]);
        let text = render(&request, body.to_string().as_bytes()).unwrap();
        assert_eq!(text, "#2 [open] Bug (@b, updated 2026-10-27) [bug]");
    }

    #[test]
    fn status_messages_cover_github_error_classes() {
        assert_eq!(
            status_message(StatusCode::UNAUTHORIZED, false),
            Some("GitHub authentication failed. Run `gh auth login`, then retry.".to_owned())
        );
        assert_eq!(
            status_message(StatusCode::FORBIDDEN, true),
            Some(
                "GitHub refused the request (permissions or rate limit). Signing in with \
                 `gh auth login` raises the rate limit."
                    .to_owned()
            )
        );
        assert_eq!(
            status_message(StatusCode::TOO_MANY_REQUESTS, true),
            Some(
                "GitHub refused the request (permissions or rate limit). Signing in with \
                 `gh auth login` raises the rate limit."
                    .to_owned()
            )
        );
        assert_eq!(
            status_message(StatusCode::NOT_FOUND, false),
            Some(
                "Not found. If it is private, run `gh auth login`, then retry.".to_owned()
            )
        );
        assert_eq!(
            status_message(StatusCode::NOT_FOUND, true),
            Some("Not found, or not visible to the signed-in GitHub account.".to_owned())
        );
        assert_eq!(
            status_message(StatusCode::BAD_GATEWAY, false),
            Some("GitHub API returned HTTP 502".to_owned())
        );
    }

    #[test]
    fn successful_http_response_is_rendered_bounded_and_keeps_credentials_out_of_output() {
        let fixture = HttpFixture::start(ResponsePlan::json(
            200,
            json!({
                "full_name": "elixpo/crumb.elixpo",
                "description": "A".repeat(1000),
                "default_branch": "main",
                "stargazers_count": 42,
                "forks_count": 7,
                "open_issues_count": 3,
                "license": {"spdx_id": "MIT"},
                "visibility": "public",
                "pushed_at": "2026-11-27",
                "topics": ["rust", "agent"],
                "html_url": "https://github.com/elixpo/crumb.elixpo"
            })
            .to_string()
            .into_bytes(),
        ));
        let output = execute(
            request("repo", &fixture.url),
            limits(Duration::from_secs(2), 200),
            Some("synthetic_test_token"),
            &CancellationToken::default(),
        )
        .expect("successful repository response");

        assert!(!output.is_error);
        assert!(output.text.starts_with("elixpo/crumb.elixpo"));
        assert!(output.text.contains("Default branch: main"));
        assert!(output.text.len() <= 200);
        assert!(!output.text.contains("synthetic_test_token"));
        assert!(
            output
                .structured
                .as_ref()
                .is_none_or(|value| !value.to_string().contains("synthetic_test_token"))
        );

        let request_head = fixture
            .requests
            .recv_timeout(Duration::from_secs(1))
            .expect("fixture received request");
        assert!(request_head.starts_with("GET / HTTP/1.1\r\n"));
        assert!(request_head.contains("Authorization: Bearer synthetic_test_token"));
        assert!(request_head.contains("Accept: application/vnd.github+json"));
        assert!(request_head.contains("X-GitHub-Api-Version: 2022-11-28"));
    }

    #[test]
    fn http_errors_return_existing_controlled_messages() {
        for (status, authenticated, expected) in [
            (
                401,
                false,
                "GitHub authentication failed. Run `gh auth login`, then retry.",
            ),
            (
                403,
                true,
                "GitHub refused the request (permissions or rate limit)",
            ),
            (404, false, "Not found. If it is private"),
            (404, true, "Not found, or not visible"),
            (429, true, "GitHub refused the request"),
            (418, false, "GitHub API returned HTTP 418"),
        ] {
            let fixture = HttpFixture::start(ResponsePlan::json(status, Vec::new()));
            let output = execute(
                request("repo", &fixture.url),
                limits(Duration::from_secs(2), 1024),
                authenticated.then_some("synthetic_test_token"),
                &CancellationToken::default(),
            )
            .expect("HTTP status is handled as a tool error");
            assert!(output.is_error, "status {status}");
            assert!(
                output.text.contains(expected),
                "status {status}: {}",
                output.text
            );
            assert!(!output.text.contains("synthetic_test_token"));
            let _ = fixture.requests.recv_timeout(Duration::from_secs(1));
        }
    }

    #[test]
    fn malformed_json_returns_a_controlled_error() {
        let fixture = HttpFixture::start(ResponsePlan::json(200, b"{".to_vec()));
        let error = execute(
            request("repo", &fixture.url),
            limits(Duration::from_secs(2), 1024),
            None,
            &CancellationToken::default(),
        )
        .expect_err("malformed JSON must fail");
        assert!(error.to_string().contains("GitHub returned invalid JSON"));
        let _ = fixture.requests.recv_timeout(Duration::from_secs(1));
    }

    #[test]
    fn oversized_response_is_rejected_before_unbounded_rendering() {
        let fixture = HttpFixture::start(ResponsePlan::json(
            200,
            vec![b'x'; 65 * 1024],
        ));
        let output = execute(
            request("repo", &fixture.url),
            limits(Duration::from_secs(2), 1024),
            None,
            &CancellationToken::default(),
        )
        .expect("oversized response is a tool error");
        assert!(output.is_error);
        assert_eq!(
            output.text,
            "GitHub response exceeded the configured size limit; narrow the request"
        );
        assert!(output.text.len() < 1024);
        let _ = fixture.requests.recv_timeout(Duration::from_secs(1));
    }

    #[test]
    fn redirect_is_not_followed() {
        let fixture = HttpFixture::start(ResponsePlan::redirect(302));
        let output = execute(
            request("repo", &fixture.url),
            limits(Duration::from_secs(2), 1024),
            None,
            &CancellationToken::default(),
        )
        .expect("redirect is handled without following");
        assert!(output.is_error);
        assert_eq!(output.text, "GitHub API returned HTTP 302");
        let request_head = fixture
            .requests
            .recv_timeout(Duration::from_secs(1))
            .expect("initial request");
        assert!(request_head.starts_with("GET / HTTP/1.1\r\n"));
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn cancellation_while_waiting_for_headers_returns_cancelled() {
        let mut plan = ResponsePlan::json(200, json!({"full_name": "o/r"}).to_string().into_bytes());
        plan.gate_headers = true;
        let fixture = HttpFixture::start(plan);
        let cancellation = CancellationToken::default();
        let trigger = cancellation.clone();
        let worker_cancellation = cancellation.clone();
        let worker_request = request("repo", &fixture.url);
        let worker = thread::spawn(move || {
            execute(
                worker_request,
                limits(Duration::from_secs(5), 1024),
                None,
                &worker_cancellation,
            )
        });

        fixture
            .requests
            .recv_timeout(Duration::from_secs(1))
            .expect("request reached fixture");
        trigger.cancel();
        let result = worker.join().expect("cancellation worker");
        let error = result.expect_err("awaiting response is cancelled");
        assert!(error.to_string().contains("GitHub request cancelled"));
    }

    #[test]
    fn cancellation_during_response_streaming_stops_processing() {
        let mut plan = ResponsePlan::json(
            200,
            json!({
                "full_name": "o/r",
                "description": "streamed"
            })
            .to_string()
            .into_bytes(),
        );
        plan.stream_first_chunk = Some(1);
        let fixture = HttpFixture::start(plan);
        let cancellation = CancellationToken::default();
        let trigger = cancellation.clone();
        let worker_cancellation = cancellation.clone();
        let worker_request = request("repo", &fixture.url);
        let worker = thread::spawn(move || {
            execute(
                worker_request,
                limits(Duration::from_secs(5), 1024),
                Some("synthetic_test_token"),
                &worker_cancellation,
            )
        });

        fixture
            .chunks
            .recv_timeout(Duration::from_secs(1))
            .expect("first response chunk reached network");
        trigger.cancel();
        let result = worker.join().expect("stream cancellation worker");
        let error = result.expect_err("streaming response is cancelled");
        assert!(error.to_string().contains("GitHub response cancelled"));
    }
}
