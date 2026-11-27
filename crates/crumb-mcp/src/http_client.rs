use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use crumb_agent::{
    CancellationToken, CredentialReference, McpServer, McpTransport, RiskClass, ToolDescriptor,
    ToolHandler, ToolHost, ToolOutput, ToolTransport,
};
use crumb_auth::SecretString;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::{Client, Response};
use serde_json::{Value, json};

use crate::client::{
    DiscoveredTool, McpDiagnostic, parse_tool, parse_tool_output, redacted_error,
    validate_component,
};

const PROTOCOL_VERSION: &str = "2025-11-25";
const SESSION_HEADER: &str = "mcp-session-id";

#[derive(Clone, Copy, Debug)]
pub struct StreamableHttpLimits {
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    pub max_response_bytes: usize,
    pub max_tools: usize,
}

impl Default for StreamableHttpLimits {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            max_response_bytes: 1024 * 1024,
            max_tools: 128,
        }
    }
}

/// Registers enabled Streamable HTTP MCP servers. Credential resolution is
/// supplied by the caller so this transport never reads or persists secrets.
pub fn register_streamable_http_servers<F>(
    host: &mut ToolHost,
    servers: &[McpServer],
    limits: StreamableHttpLimits,
    mut resolve_credential: F,
) -> Vec<McpDiagnostic>
where
    F: FnMut(&CredentialReference) -> Result<SecretString>,
{
    let mut diagnostics = Vec::new();
    for server in servers
        .iter()
        .filter(|server| server.enabled && server.transport == McpTransport::StreamableHttp)
    {
        let credential = match server
            .credential
            .as_ref()
            .map(&mut resolve_credential)
            .transpose()
        {
            Ok(value) => value,
            Err(error) => {
                diagnostics.push(McpDiagnostic {
                    server_id: server.id.clone(),
                    message: redacted_error(&error),
                });
                continue;
            }
        };
        if let Err(error) = register_server(host, server, credential, limits) {
            diagnostics.push(McpDiagnostic {
                server_id: server.id.clone(),
                message: redacted_error(&error),
            });
        }
    }
    diagnostics
}

fn register_server(
    host: &mut ToolHost,
    server: &McpServer,
    credential: Option<SecretString>,
    limits: StreamableHttpLimits,
) -> Result<()> {
    validate_component(&server.id, "server ID")?;
    let endpoint = server
        .url
        .as_deref()
        .context("external MCP HTTP URL is missing")?;
    let client = Arc::new(StreamableHttpClient::connect(endpoint, credential, limits)?);
    let tools = client.discover()?;
    let mut names = BTreeSet::new();
    for tool in &tools {
        let namespaced = format!("mcp__{}__{}", server.id, tool.name);
        if !names.insert(namespaced.clone()) || host.descriptor(&namespaced).is_some() {
            bail!("external MCP tool namespace collision");
        }
    }
    for tool in tools {
        register_tool(host, server, &client, tool)?;
    }
    Ok(())
}

fn register_tool(
    host: &mut ToolHost,
    server: &McpServer,
    client: &Arc<StreamableHttpClient>,
    tool: DiscoveredTool,
) -> Result<()> {
    let namespaced = format!("mcp__{}__{}", server.id, tool.name);
    host.register(
        ToolDescriptor {
            name: namespaced,
            description: tool.description,
            input_schema: tool.input_schema,
            risk: RiskClass::NetworkAccess,
            transport: ToolTransport::Mcp {
                server_id: server.id.clone(),
            },
        },
        Arc::new(HttpToolHandler {
            client: Arc::clone(client),
            remote_name: tool.name,
        }),
    )
}

struct StreamableHttpClient {
    client: Client,
    runtime: Mutex<tokio::runtime::Runtime>,
    endpoint: String,
    credential: Option<Arc<SecretString>>,
    session_id: Mutex<Option<String>>,
    request_lock: Mutex<()>,
    next_id: AtomicU64,
    limits: StreamableHttpLimits,
}

impl StreamableHttpClient {
    fn connect(
        endpoint: &str,
        credential: Option<SecretString>,
        limits: StreamableHttpLimits,
    ) -> Result<Self> {
        if limits.max_response_bytes == 0 || limits.max_tools == 0 {
            bail!("MCP limits must be positive");
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("external MCP HTTP runtime is unavailable")?;
        let client = runtime.block_on(async {
            Client::builder()
                .connect_timeout(limits.startup_timeout)
                .timeout(limits.request_timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .context("external MCP HTTP client is unavailable")
        })?;
        let result = Self {
            client,
            runtime: Mutex::new(runtime),
            endpoint: endpoint.to_owned(),
            credential: credential.map(Arc::new),
            session_id: Mutex::new(None),
            request_lock: Mutex::new(()),
            next_id: AtomicU64::new(1),
            limits,
        };
        result.initialize()?;
        Ok(result)
    }

    fn initialize(&self) -> Result<()> {
        let result = self.request(
            "initialize",
            &json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "crumb", "version": env!("CARGO_PKG_VERSION")}
            }),
            self.limits.startup_timeout,
            &CancellationToken::default(),
        )?;
        if result.get("protocolVersion").and_then(Value::as_str) != Some(PROTOCOL_VERSION) {
            bail!("external MCP protocol version is invalid");
        }
        self.notification("notifications/initialized", &json!({}))
    }

    fn discover(&self) -> Result<Vec<DiscoveredTool>> {
        let result = self.request(
            "tools/list",
            &json!({}),
            self.limits.startup_timeout,
            &CancellationToken::default(),
        )?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .context("external MCP tool list is malformed")?;
        if tools.len() > self.limits.max_tools {
            bail!("external MCP advertised too many tools");
        }
        tools.iter().map(parse_tool).collect()
    }

    fn call(
        &self,
        name: &str,
        arguments: &Value,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput> {
        let result = self.request(
            "tools/call",
            &json!({"name": name, "arguments": arguments}),
            self.limits.request_timeout,
            cancellation,
        )?;
        parse_tool_output(&result, self.limits.max_response_bytes)
    }

    fn request(
        &self,
        method: &str,
        params: &Value,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<Value> {
        let _guard = self
            .request_lock
            .lock()
            .map_err(|_| anyhow!("external MCP HTTP request state is unavailable"))?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let retry_safe = matches!(method, "initialize" | "tools/list");
        let (response, session_id) =
            self.send(&message, Some(id), timeout, cancellation, retry_safe)?;
        self.update_session(session_id)?;
        if response.get("error").is_some() {
            bail!("external MCP request failed");
        }
        response
            .get("result")
            .cloned()
            .context("external MCP response has no result")
    }

    fn notification(&self, method: &str, params: &Value) -> Result<()> {
        let message = json!({"jsonrpc":"2.0","method":method,"params":params});
        let (_, session_id) = self.send(
            &message,
            None,
            self.limits.startup_timeout,
            &CancellationToken::default(),
            true,
        )?;
        self.update_session(session_id)
    }

    fn send(
        &self,
        message: &Value,
        expected_id: Option<u64>,
        timeout: Duration,
        cancellation: &CancellationToken,
        retry_safe: bool,
    ) -> Result<(Value, Option<String>)> {
        let encoded = serde_json::to_vec(message)?;
        if encoded.len() > self.limits.max_response_bytes {
            bail!("external MCP request exceeded the byte limit");
        }
        let session_id = self
            .session_id
            .lock()
            .map_err(|_| anyhow!("external MCP HTTP session state is unavailable"))?
            .clone();
        let maximum = self.limits.max_response_bytes;
        let request = HttpRequest {
            encoded,
            expected_id,
            retry_safe,
        };
        let runtime = self
            .runtime
            .lock()
            .map_err(|_| anyhow!("external MCP HTTP runtime is unavailable"))?;
        runtime
            .block_on(async {
                match tokio::time::timeout(
                    timeout,
                    perform_request(
                        &self.client,
                        &self.endpoint,
                        self.credential.as_deref(),
                        session_id.as_deref(),
                        maximum,
                        &request,
                        cancellation,
                    ),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => bail!("external MCP request timed out"),
                }
            })
            .and_then(|result| {
                if cancellation.is_cancelled() {
                    bail!("external MCP request cancelled");
                }
                Ok(result)
            })
    }

    fn update_session(&self, session_id: Option<String>) -> Result<()> {
        if let Some(session_id) = session_id {
            *self
                .session_id
                .lock()
                .map_err(|_| anyhow!("external MCP HTTP session state is unavailable"))? =
                Some(session_id);
        }
        Ok(())
    }
}

struct HttpRequest {
    encoded: Vec<u8>,
    expected_id: Option<u64>,
    retry_safe: bool,
}

async fn perform_request(
    client: &Client,
    endpoint: &str,
    credential: Option<&SecretString>,
    session_id: Option<&str>,
    maximum: usize,
    request: &HttpRequest,
    cancellation: &CancellationToken,
) -> Result<(Value, Option<String>)> {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        "mcp-protocol-version",
        HeaderValue::from_static(PROTOCOL_VERSION),
    );
    if let Some(session_id) = session_id {
        headers.insert(
            SESSION_HEADER,
            HeaderValue::from_str(session_id).context("external MCP session ID is invalid")?,
        );
    }
    if let Some(credential) = credential {
        let mut value = HeaderValue::from_str(&format!("Bearer {}", credential.expose()))
            .context("external MCP credential is invalid")?;
        value.set_sensitive(true);
        headers.insert(AUTHORIZATION, value);
    }
    let send = || {
        client
            .post(endpoint)
            .headers(headers.clone())
            .body(request.encoded.clone())
            .send()
    };
    let first_attempt = tokio::select! {
        result = send() => result,
        () = cancellation.cancelled() => bail!("external MCP request cancelled"),
    };
    let response = match first_attempt {
        Ok(response) => response,
        Err(_) if request.retry_safe && !cancellation.is_cancelled() => tokio::select! {
            result = send() => result.map_err(|error| sanitized_http_error(&error))?,
            () = cancellation.cancelled() => bail!("external MCP request cancelled"),
        },
        Err(error) => return Err(sanitized_http_error(&error)),
    };
    let session_id = response
        .headers()
        .get(SESSION_HEADER)
        .map(|value| value.to_str().map(str::to_owned))
        .transpose()
        .context("external MCP session ID is invalid")?;
    if request.expected_id.is_none() {
        if response.status().is_success() {
            return Ok((Value::Null, session_id));
        }
        bail!("external MCP HTTP request failed");
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let response = response
        .error_for_status()
        .context("external MCP HTTP request failed")?;
    let body = read_bounded(response, maximum, cancellation).await?;
    if cancellation.is_cancelled() {
        bail!("external MCP request cancelled");
    }
    let value = if content_type.starts_with("text/event-stream") {
        parse_sse(&body, request.expected_id.expect("checked above"))?
    } else {
        serde_json::from_slice(&body).context("external MCP returned malformed JSON")?
    };
    if value.get("id").and_then(Value::as_u64) != request.expected_id {
        bail!("external MCP response ID is invalid");
    }
    Ok((value, session_id))
}

fn sanitized_http_error(error: &reqwest::Error) -> anyhow::Error {
    if error.is_timeout() {
        anyhow!("external MCP request timed out")
    } else {
        anyhow!("external MCP HTTP request failed")
    }
}

async fn read_bounded(
    mut response: Response,
    maximum: usize,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = tokio::select! {
        result = response.chunk() => result.context("external MCP HTTP response failed")?,
        () = cancellation.cancelled() => bail!("external MCP request cancelled"),
    } {
        if body.len().saturating_add(chunk.len()) > maximum {
            bail!("external MCP response exceeded the byte limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn parse_sse(body: &[u8], expected_id: u64) -> Result<Value> {
    let text = std::str::from_utf8(body).context("external MCP SSE response is malformed")?;
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let value: Value =
            serde_json::from_str(data.trim()).context("external MCP SSE response is malformed")?;
        if value.get("id").and_then(Value::as_u64) == Some(expected_id) {
            return Ok(value);
        }
    }
    bail!("external MCP SSE response is malformed")
}

struct HttpToolHandler {
    client: Arc<StreamableHttpClient>,
    remote_name: String,
}

impl ToolHandler for HttpToolHandler {
    fn call(&self, arguments: &Value, cancellation: &CancellationToken) -> Result<ToolOutput> {
        self.client.call(&self.remote_name, arguments, cancellation)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;

    use crumb_agent::{AgentMode, ConfiguredApprovals, DenyAllApprovals};

    use super::*;

    #[derive(Clone, Copy)]
    enum FixtureBehavior {
        Normal,
        HoldHeaders(&'static str),
        StreamThenHold(&'static str),
        DropFirst(&'static str),
    }

    struct FixtureSignals<'a> {
        calls: &'a AtomicUsize,
        methods: &'a Mutex<Vec<String>>,
        requests: &'a mpsc::SyncSender<String>,
        chunks_sent: &'a mpsc::SyncSender<()>,
        connections_closed: &'a mpsc::SyncSender<bool>,
    }

    struct Fixture {
        url: String,
        calls: Arc<AtomicUsize>,
        methods: Arc<Mutex<Vec<String>>>,
        requests: mpsc::Receiver<String>,
        chunks_sent: mpsc::Receiver<()>,
        connections_closed: mpsc::Receiver<bool>,
        stopped: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl Fixture {
        fn start(malformed_list: bool) -> Self {
            Self::start_with_behavior(malformed_list, FixtureBehavior::Normal)
        }

        fn start_with_behavior(malformed_list: bool, behavior: FixtureBehavior) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
            listener.set_nonblocking(true).expect("fixture nonblocking");
            let address = listener.local_addr().expect("fixture address");
            let calls = Arc::new(AtomicUsize::new(0));
            let methods = Arc::new(Mutex::new(Vec::new()));
            let stopped = Arc::new(AtomicBool::new(false));
            let worker_calls = Arc::clone(&calls);
            let worker_methods = Arc::clone(&methods);
            let worker_stopped = Arc::clone(&stopped);
            let (request_sender, requests) = mpsc::sync_channel(16);
            let (chunk_sender, chunks_sent) = mpsc::sync_channel(2);
            let (closed_sender, connections_closed) = mpsc::sync_channel(2);
            let worker = thread::spawn(move || {
                while !worker_stopped.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            handle_request(
                                &mut stream,
                                &FixtureSignals {
                                    calls: &worker_calls,
                                    methods: &worker_methods,
                                    requests: &request_sender,
                                    chunks_sent: &chunk_sender,
                                    connections_closed: &closed_sender,
                                },
                                malformed_list,
                                behavior,
                            );
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                url: format!("http://{address}/mcp"),
                calls,
                methods,
                requests,
                chunks_sent,
                connections_closed,
                stopped,
                worker: Some(worker),
            }
        }

        fn server(&self) -> McpServer {
            McpServer {
                id: "remote".to_owned(),
                transport: McpTransport::StreamableHttp,
                command: std::path::PathBuf::new(),
                arguments: Vec::new(),
                url: Some(self.url.clone()),
                credential: None,
                enabled: true,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::Release);
            let _ = TcpStream::connect(
                self.url
                    .trim_start_matches("http://")
                    .trim_end_matches("/mcp"),
            );
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn handle_request(
        stream: &mut TcpStream,
        fixture: &FixtureSignals<'_>,
        malformed_list: bool,
        behavior: FixtureBehavior,
    ) {
        // Windows inherits the listener's nonblocking mode on accept; block so the 1s
        // read timeout bounds the request instead of rejecting bytes that have not arrived.
        stream
            .set_nonblocking(false)
            .expect("fixture stream blocking");
        let Some(body) = read_http_body(stream) else {
            return;
        };
        let Ok(message) = serde_json::from_slice::<Value>(&body) else {
            return;
        };
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let attempt = {
            let mut methods = fixture.methods.lock().expect("fixture request methods");
            methods.push(method.to_owned());
            methods
                .iter()
                .filter(|seen| seen.as_str() == method)
                .count()
        };
        let _ = fixture.requests.send(method.to_owned());

        if let FixtureBehavior::DropFirst(drop_method) = behavior
            && method == drop_method
            && attempt == 1
        {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return;
        }
        if let FixtureBehavior::HoldHeaders(held_method) = behavior
            && method == held_method
        {
            report_connection_close(stream, fixture.connections_closed);
            return;
        }
        if let FixtureBehavior::StreamThenHold(held_method) = behavior
            && method == held_method
        {
            let body = b"{\"jsonrpc\":\"2.0\",";
            let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 256\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(response);
            let _ = stream.write_all(body);
            let _ = stream.flush();
            let _ = fixture.chunks_sent.send(());
            report_connection_close(stream, fixture.connections_closed);
            return;
        }
        if method == "notifications/initialized" {
            let _ = stream.write_all(
                b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            return;
        }
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let payload = match method {
            "initialize" => json!({
                "jsonrpc":"2.0", "id":id,
                "result":{"protocolVersion":PROTOCOL_VERSION,"capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}
            }),
            "tools/list" if malformed_list => {
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1\r\nConnection: close\r\n\r\n{");
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Write);
                return;
            }
            "tools/list" => json!({
                "jsonrpc":"2.0", "id":id,
                "result":{"tools":[{"name":"echo","description":"fixture","inputSchema":{"type":"object"}}]}
            }),
            "tools/call" => {
                fixture.calls.fetch_add(1, Ordering::Relaxed);
                json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":"called"}],"isError":false}})
            }
            _ => return,
        };
        let encoded = serde_json::to_vec(&payload).expect("fixture JSON");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: fixture-session\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            encoded.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.write_all(&encoded);
    }

    fn report_connection_close(stream: &mut TcpStream, closed: &mpsc::SyncSender<bool>) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut byte = [0_u8; 1];
        let disconnected = match stream.read(&mut byte) {
            Ok(0) => true,
            Err(error) => matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::NotConnected
            ),
            Ok(_) => false,
        };
        let _ = closed.send(disconnected);
    }

    fn read_http_body(stream: &mut TcpStream) -> Option<Vec<u8>> {
        stream.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).ok()?;
            request.push(byte[0]);
            if request.len() > 16 * 1024 {
                return None;
            }
        }
        let headers = std::str::from_utf8(&request).ok()?;
        let length = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })?;
        let mut body = vec![0; length];
        stream.read_exact(&mut body).ok()?;
        Some(body)
    }

    fn wait_for_request(fixture: &Fixture, expected_method: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let method = fixture
                .requests
                .recv_timeout(remaining)
                .expect("fixture receives the expected request");
            if method == expected_method {
                return;
            }
        }
    }

    fn spawn_tool_call(
        client: StreamableHttpClient,
        cancellation: CancellationToken,
    ) -> (mpsc::Receiver<Result<ToolOutput>>, thread::JoinHandle<()>) {
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let result = client.call("echo", &json!({}), &cancellation);
            let _ = sender.send(result);
        });
        (receiver, worker)
    }

    fn assert_connection_closed(fixture: &Fixture) {
        assert!(
            fixture
                .connections_closed
                .recv_timeout(Duration::from_secs(3))
                .expect("fixture observes client connection termination")
        );
    }

    #[test]
    fn remote_calls_are_discovered_and_denied_before_network() {
        let fixture = Fixture::start(false);
        let mut host = ToolHost::default();
        let diagnostics = register_streamable_http_servers(
            &mut host,
            &[fixture.server()],
            StreamableHttpLimits::default(),
            |_| unreachable!("fixture has no credential"),
        );
        assert_eq!(diagnostics, []);
        assert_eq!(
            host.descriptor("mcp__remote__echo")
                .expect("remote descriptor")
                .risk,
            RiskClass::NetworkAccess
        );
        assert!(
            host.call(
                "mcp__remote__echo",
                &json!({}),
                AgentMode::Auto,
                &DenyAllApprovals,
                &CancellationToken::default(),
            )
            .is_err()
        );
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 0);

        let approvals = ConfiguredApprovals::default()
            .with_mcp_tools(["mcp__remote__echo".to_owned()].into_iter().collect());
        let output = host
            .call(
                "mcp__remote__echo",
                &json!({}),
                AgentMode::Auto,
                &approvals,
                &CancellationToken::default(),
            )
            .expect("approved remote call");
        assert_eq!(output.text, "called");
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn malformed_remote_servers_are_isolated() {
        let fixture = Fixture::start(true);
        let mut host = ToolHost::default();
        let diagnostics = register_streamable_http_servers(
            &mut host,
            &[fixture.server()],
            StreamableHttpLimits::default(),
            |_| unreachable!("fixture has no credential"),
        );
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("malformed"));
        assert!(host.descriptor("mcp__remote__echo").is_none());
    }

    #[test]
    fn credential_resolution_errors_are_redacted() {
        let fixture = Fixture::start(false);

        let mut host = ToolHost::default();
        let mut credential_server = fixture.server();
        credential_server.credential = Some(CredentialReference::Environment {
            name: "FIXTURE_SECRET".to_owned(),
        });
        let diagnostics = register_streamable_http_servers(
            &mut host,
            &[credential_server],
            StreamableHttpLimits::default(),
            |_| bail!("credential super-secret-value is missing"),
        );
        assert_eq!(diagnostics.len(), 1);
        assert!(!diagnostics[0].message.contains("super-secret-value"));
    }

    #[test]
    fn active_remote_calls_can_be_cancelled() {
        let fixture =
            Fixture::start_with_behavior(false, FixtureBehavior::HoldHeaders("tools/call"));
        let client = StreamableHttpClient::connect(
            &fixture.url,
            None,
            StreamableHttpLimits {
                request_timeout: Duration::from_secs(1),
                ..StreamableHttpLimits::default()
            },
        )
        .expect("client connects");
        let cancellation = CancellationToken::default();
        let (result, worker) = spawn_tool_call(client, cancellation.clone());
        wait_for_request(&fixture, "tools/call");
        cancellation.cancel();
        let error = result
            .recv_timeout(Duration::from_secs(3))
            .expect("cancelled call returns promptly")
            .expect_err("remote call is cancelled");
        worker.join().expect("tool call worker");
        assert!(error.to_string().contains("cancelled"));
        assert_connection_closed(&fixture);
    }

    #[test]
    fn cancellation_stops_response_body_streaming() {
        let fixture =
            Fixture::start_with_behavior(false, FixtureBehavior::StreamThenHold("tools/call"));
        let client = StreamableHttpClient::connect(
            &fixture.url,
            None,
            StreamableHttpLimits {
                request_timeout: Duration::from_secs(1),
                ..StreamableHttpLimits::default()
            },
        )
        .expect("client connects");
        let cancellation = CancellationToken::default();
        let (result, worker) = spawn_tool_call(client, cancellation.clone());
        wait_for_request(&fixture, "tools/call");
        fixture
            .chunks_sent
            .recv_timeout(Duration::from_secs(3))
            .expect("fixture sends a partial response body");
        cancellation.cancel();
        let error = result
            .recv_timeout(Duration::from_secs(3))
            .expect("cancelled streaming call returns promptly")
            .expect_err("remote call is cancelled");
        worker.join().expect("tool call worker");
        assert!(error.to_string().contains("cancelled"));
        assert_connection_closed(&fixture);
    }

    #[test]
    fn request_timeout_closes_the_in_flight_connection() {
        let fixture =
            Fixture::start_with_behavior(false, FixtureBehavior::HoldHeaders("tools/call"));
        let client = StreamableHttpClient::connect(
            &fixture.url,
            None,
            StreamableHttpLimits {
                request_timeout: Duration::from_millis(100),
                ..StreamableHttpLimits::default()
            },
        )
        .expect("client connects");
        let error = client
            .call("echo", &json!({}), &CancellationToken::default())
            .expect_err("delayed response times out");
        assert!(error.to_string().contains("timed out"));
        assert_connection_closed(&fixture);
    }

    #[test]
    fn startup_timeout_closes_the_initialization_connection() {
        let fixture =
            Fixture::start_with_behavior(false, FixtureBehavior::HoldHeaders("initialize"));
        let url = fixture.url.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let result = StreamableHttpClient::connect(
                &url,
                None,
                StreamableHttpLimits {
                    startup_timeout: Duration::from_millis(100),
                    request_timeout: Duration::from_secs(1),
                    ..StreamableHttpLimits::default()
                },
            );
            let _ = sender.send(result);
        });
        wait_for_request(&fixture, "initialize");
        let result = receiver
            .recv_timeout(Duration::from_secs(3))
            .expect("initialization timeout returns promptly");
        let error = result.err().expect("delayed initialization times out");
        worker.join().expect("initialization worker");
        assert!(error.to_string().contains("timed out"));
        assert_connection_closed(&fixture);
    }

    #[test]
    fn retries_only_idempotent_mcp_requests() {
        let discovery_fixture =
            Fixture::start_with_behavior(false, FixtureBehavior::DropFirst("tools/list"));
        StreamableHttpClient::connect(
            &discovery_fixture.url,
            None,
            StreamableHttpLimits::default(),
        )
        .expect("tool discovery retries once");
        assert_eq!(
            discovery_fixture
                .methods
                .lock()
                .expect("fixture methods")
                .iter()
                .filter(|method| method.as_str() == "tools/list")
                .count(),
            2
        );

        let call_fixture =
            Fixture::start_with_behavior(false, FixtureBehavior::DropFirst("tools/call"));
        let client =
            StreamableHttpClient::connect(&call_fixture.url, None, StreamableHttpLimits::default())
                .expect("client connects");
        assert!(
            client
                .call("echo", &json!({}), &CancellationToken::default())
                .is_err()
        );
        assert_eq!(
            call_fixture
                .methods
                .lock()
                .expect("fixture methods")
                .iter()
                .filter(|method| method.as_str() == "tools/call")
                .count(),
            1
        );
    }

    #[test]
    fn sse_responses_select_the_matching_json_rpc_event() {
        let body = b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n\n";
        assert_eq!(
            parse_sse(body, 7).expect("valid SSE response")["id"],
            json!(7)
        );
    }
}
