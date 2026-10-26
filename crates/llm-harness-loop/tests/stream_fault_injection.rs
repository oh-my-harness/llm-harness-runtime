//! Fault-injection integration tests for the agent loop + real adapter.
//!
//! These tests deliberately avoid `MockLlmClient`: the question under test is
//! what a **real** `llm_adapter::openai::OpenAIProvider` (reqwest body reader,
//! SSE parser, `read_timeout`) does when a local raw-TCP server misbehaves, and
//! how the harness loop reacts (request-level retry, discarded partials, tool
//! replay). A scripted mock cannot observe any of the transport behavior.
//!
//! Each scenario runs against `tokio::net::TcpListener` serving hand-written
//! HTTP/1.1 chunked responses, and counts real `POST` requests server-side.
//! Every test asserts the server-side POST count, because the number of
//! `RetryAttempt` events is *not* the same thing as the number of requests that
//! actually reached the wire (the first attempt never emits one, and a retry
//! can fail before the client observes it).
//!
//! Measurements are printed as `[MEASURE] ...` lines and appended to
//! `/tmp/v0-sse-B-measurements.log` so the experiment report can quote real
//! timings instead of reconstructed ones.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use llm_adapter::provider::Provider;
use llm_harness_loop::{
    DefaultConvertToLlm, FinalAnswerMode, LoopConfig, OpenAIProvider, RetryConfig, agent_loop,
};
use llm_harness_types::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ─────────────────────────────────────────────────────────────────────────────
// Raw HTTP/1.1 SSE server
// ─────────────────────────────────────────────────────────────────────────────

const SSE_HEADERS: &str = "HTTP/1.1 200 OK\r\n\
     Content-Type: text/event-stream\r\n\
     Cache-Control: no-cache\r\n\
     Transfer-Encoding: chunked\r\n\
     Connection: close\r\n\
     \r\n";

/// One scripted step the server performs after writing the response headers.
#[derive(Clone, Debug)]
enum Action {
    /// A complete, well-formed SSE data event: `data: <payload>\n\n`.
    Data(String),
    /// A complete SSE comment (heartbeat): `: keep-alive\n\n`.
    Comment,
    /// Pause without writing anything (connection stays open).
    Sleep(Duration),
    /// Write the terminal zero-length chunk: clean end of a chunked body.
    EndChunked,
    /// Close the socket abruptly, without the terminal chunk.
    Close,
}

/// Handle to a running scripted server.
struct Server {
    base_url: String,
    /// Real `POST` requests observed on the wire (request line), not attempts.
    posts: Arc<AtomicUsize>,
    /// Raw request bodies, in arrival order.
    bodies: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn posts(&self) -> usize {
        self.posts.load(Ordering::SeqCst)
    }

    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Read one HTTP/1.1 request (head + `Content-Length` body) off the socket.
async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<(String, String)> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 8192];
        loop {
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..pos + 4]).to_string();
                let content_length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if !name.eq_ignore_ascii_case("content-length") {
                            return None;
                        }
                        value.trim().parse::<usize>().ok()
                    })
                    .unwrap_or(0);
                let mut body = buf[pos + 4..].to_vec();
                while body.len() < content_length {
                    let n = socket.read(&mut tmp).await.ok()?;
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&tmp[..n]);
                }
                return Some((head, String::from_utf8_lossy(&body).to_string()));
            }
            let n = socket.read(&mut tmp).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
    })
    .await
    .ok()
    .flatten()
}

async fn write_http_chunk(socket: &mut tokio::net::TcpStream, payload: &str) {
    let framed = format!("{:x}\r\n{}\r\n", payload.len(), payload);
    let _ = socket.write_all(framed.as_bytes()).await;
    let _ = socket.flush().await;
}

/// Start a server whose request `i` (0-based, by real POST order) runs
/// `scripts[i]`. If more requests arrive than scripts, the last script repeats.
async fn start_server(scripts: Vec<Vec<Action>>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let posts = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));

    let posts_srv = Arc::clone(&posts);
    let bodies_srv = Arc::clone(&bodies);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let posts = Arc::clone(&posts_srv);
            let bodies = Arc::clone(&bodies_srv);
            let scripts = scripts.clone();
            tokio::spawn(async move {
                let Some((head, body)) = read_request(&mut socket).await else {
                    return;
                };
                if !head.starts_with("POST ") {
                    return;
                }
                let index = posts.fetch_add(1, Ordering::SeqCst);
                bodies.lock().unwrap().push(body);
                let script = scripts
                    .get(index)
                    .or_else(|| scripts.last())
                    .cloned()
                    .unwrap_or_default();

                let _ = socket.write_all(SSE_HEADERS.as_bytes()).await;
                let _ = socket.flush().await;
                for action in script {
                    match action {
                        Action::Data(payload) => {
                            write_http_chunk(&mut socket, &format!("data: {payload}\n\n")).await;
                        }
                        Action::Comment => {
                            write_http_chunk(&mut socket, ": keep-alive\n\n").await;
                        }
                        Action::Sleep(duration) => tokio::time::sleep(duration).await,
                        Action::EndChunked => {
                            let _ = socket.write_all(b"0\r\n\r\n").await;
                            let _ = socket.flush().await;
                        }
                        Action::Close => return,
                    }
                }
                // Dropping the socket here truncates the chunked body.
            });
        }
    });

    Server {
        base_url: format!("http://{addr}"),
        posts,
        bodies,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SSE payload builders (OpenAI Chat Completions wire format)
// ─────────────────────────────────────────────────────────────────────────────

fn delta(text: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-fault",
        "model": "test-model",
        "choices": [{"delta": {"content": text}, "finish_reason": null}],
    })
    .to_string()
}

fn finish(reason: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-fault",
        "model": "test-model",
        "choices": [{"delta": {}, "finish_reason": reason}],
        "usage": {"prompt_tokens": 7, "completion_tokens": 3},
    })
    .to_string()
}

fn tool_call_chunk(id: &str, name: &str, args: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-fault",
        "model": "test-model",
        "choices": [{
            "delta": {"tool_calls": [{
                "index": 0,
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": args},
            }]},
            "finish_reason": "tool_calls",
        }],
        "usage": {"prompt_tokens": 7, "completion_tokens": 3},
    })
    .to_string()
}

const DONE: &str = "[DONE]";

/// `count` text deltas (`{prefix}{i}|`), then finish chunk, `[DONE]`, clean EOF.
fn text_stream(prefix: &str, count: usize) -> Vec<Action> {
    let mut actions: Vec<Action> = (0..count)
        .map(|i| Action::Data(delta(&format!("{prefix}{i}|"))))
        .collect();
    actions.push(Action::Data(finish("stop")));
    actions.push(Action::Data(DONE.to_string()));
    actions.push(Action::EndChunked);
    actions
}

/// `count` text deltas, an optional silent gap, a tail delta, then a normal end.
fn text_stream_with_gap(prefix: &str, count: usize, gap: Option<Duration>) -> Vec<Action> {
    let mut actions: Vec<Action> = (0..count)
        .map(|i| Action::Data(delta(&format!("{prefix}{i}|"))))
        .collect();
    if let Some(gap) = gap {
        actions.push(Action::Sleep(gap));
    }
    actions.push(Action::Data(delta(&format!("{prefix}TAIL|"))));
    actions.push(Action::Data(finish("stop")));
    actions.push(Action::Data(DONE.to_string()));
    actions.push(Action::EndChunked);
    actions
}

fn expected_text(prefix: &str, count: usize) -> String {
    let mut text: String = (0..count).map(|i| format!("{prefix}{i}|")).collect();
    text.push_str(&format!("{prefix}TAIL|"));
    text
}

fn dropped_after(prefix: &str, count: usize) -> Vec<Action> {
    let mut actions: Vec<Action> = (0..count)
        .map(|i| Action::Data(delta(&format!("{prefix}{i}|"))))
        .collect();
    // Abrupt close: complete SSE data events, then a truncated chunked body.
    actions.push(Action::Close);
    actions
}

// ─────────────────────────────────────────────────────────────────────────────
// Harness driver
// ─────────────────────────────────────────────────────────────────────────────

/// Remove proxy interference: the developer shell exports `http_proxy`, which
/// would make reqwest send local test traffic through a proxy and mask real
/// transport behavior with a 502.
fn bypass_process_proxy() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: every test in this binary builds its reqwest client after
        // this call, and no other code reads these variables at runtime.
        unsafe {
            std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
            std::env::set_var("no_proxy", "127.0.0.1,localhost");
            std::env::remove_var("HTTP_PROXY");
            std::env::remove_var("http_proxy");
            std::env::remove_var("HTTPS_PROXY");
            std::env::remove_var("https_proxy");
        }
    });
}

struct LoopOptions {
    read_timeout: Duration,
    retry: Option<RetryConfig>,
    stream_idle_timeout_ms: Option<u64>,
    tools: Vec<Arc<dyn Tool>>,
}

impl Default for LoopOptions {
    fn default() -> Self {
        Self {
            read_timeout: Duration::from_millis(100),
            retry: Some(RetryConfig::new(2, 0)),
            stream_idle_timeout_ms: None,
            tools: Vec::new(),
        }
    }
}

fn make_config(options: LoopOptions) -> LoopConfig {
    let stream_options = StreamOptions {
        stream_idle_timeout_ms: options.stream_idle_timeout_ms,
        ..StreamOptions::default()
    };
    LoopConfig {
        run: Arc::new(RunContext::new(RunRequest::default())),
        model: "test-model".into(),
        max_tokens: 256,
        temperature: None,
        thinking_level: ThinkingLevel::Off,
        tools: options.tools,
        active_tools: None,
        default_execution_mode: ToolExecutionMode::Parallel,
        final_answer_mode: FinalAnswerMode::Heuristic,
        env: Arc::new(UnsupportedEnv::new()),
        abort: tokio_util::sync::CancellationToken::new(),
        stream_options,
        convert_to_llm: Arc::new(DefaultConvertToLlm::new()),
        transform_context: None,
        prepare_next_turn: None,
        should_stop: None,
        before_provider_request: None,
        after_provider_response: None,
        final_answer_validator: None,
        provider_error: None,
        max_provider_error_retries: None,
        steer_rx: None,
        follow_up_rx: None,
        retry: options.retry,
        response_format: None,
    }
}

/// Drive the real agent loop against `server`; returns all events and elapsed.
async fn drive(server: &Server, options: LoopOptions) -> (Vec<AgentEvent>, Duration) {
    bypass_process_proxy();
    let provider = OpenAIProvider::builder("test-key")
        .base_url(server.base_url.clone())
        .read_timeout(options.read_timeout)
        .connect_timeout(Duration::from_secs(2))
        .build();
    let client: Arc<dyn Provider> = Arc::new(provider);
    let config = make_config(options);
    let ctx = AgentContext {
        system_prompt: None,
        messages: vec![AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "ping".into(),
            }],
            timestamp: chrono::Utc::now(),
        })],
    };
    let started = Instant::now();
    // Hard guard: a misbehaving transport (e.g. read_timeout not firing) must
    // fail the test, not hang the binary. 20s is far above every scenario.
    let events: Vec<AgentEvent> = tokio::time::timeout(
        Duration::from_secs(20),
        agent_loop(client, ctx, config).collect::<Vec<_>>(),
    )
    .await
    .expect("agent loop did not finish within 20s");
    (events, started.elapsed())
}

// ─────────────────────────────────────────────────────────────────────────────
// Event helpers
// ─────────────────────────────────────────────────────────────────────────────

fn retries(events: &[AgentEvent]) -> Vec<(u32, u64, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::RetryAttempt {
                attempt,
                delay_ms,
                error,
                ..
            } => Some((*attempt, *delay_ms, error.clone())),
            _ => None,
        })
        .collect()
}

fn discarded_messages(events: &[AgentEvent]) -> Vec<&AssistantMessage> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageEnd {
                message,
                discarded: true,
                ..
            } => Some(message),
            _ => None,
        })
        .collect()
}

fn final_answer(events: &[AgentEvent]) -> Option<&AssistantMessage> {
    events.iter().rev().find_map(|event| match event {
        AgentEvent::MessageEnd {
            message,
            discarded: false,
            ..
        } if message.kind == AssistantMessageKind::FinalAnswer => Some(message),
        _ => None,
    })
}

fn first_error(events: &[AgentEvent]) -> Option<&AgentError> {
    events.iter().find_map(|event| match event {
        AgentEvent::Error(error) => Some(error),
        _ => None,
    })
}

fn new_messages(events: &[AgentEvent]) -> Option<&Vec<AgentMessage>> {
    events.iter().rev().find_map(|event| match event {
        AgentEvent::AgentEnd { new_messages } => Some(new_messages),
        _ => None,
    })
}

fn all_assistant_text(messages: &[AgentMessage]) -> String {
    let mut out = String::new();
    for message in messages {
        if let AgentMessage::Assistant(assistant) = message {
            out.push_str(&assistant.text_content());
            out.push('\n');
        }
    }
    out
}

/// Assert the surfaced error is a `StreamIncomplete`, returning its stable
/// transport classification, its detail, and its per-attempt chunk count.
fn expect_stream_incomplete(error: &AgentError) -> (StreamEndKind, Option<String>, u32) {
    match error {
        AgentError::ProviderTyped {
            kind:
                ProviderErrorKind::StreamIncomplete {
                    kind,
                    detail,
                    received_chunks,
                    ..
                },
            ..
        } => (*kind, detail.clone(), *received_chunks),
        other => panic!("expected StreamIncomplete, got {other:?}"),
    }
}

fn measure(key: &str, value: impl std::fmt::Display) {
    let line = format!("[MEASURE] {key}: {value}");
    eprintln!("{line}");
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/v0-sse-B-measurements.log")
    {
        let _ = writeln!(file, "{line}");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 1 — a well-formed stream is one POST, no retry
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn s01_complete_stream_is_single_post_without_retry() {
    let server = start_server(vec![text_stream("OK", 3)]).await;
    let (events, elapsed) = drive(
        &server,
        LoopOptions {
            read_timeout: Duration::from_millis(500),
            ..LoopOptions::default()
        },
    )
    .await;
    measure("s01.elapsed_ms", elapsed.as_millis());
    measure("s01.posts", server.posts());

    assert_eq!(server.posts(), 1, "expected exactly one real POST");
    assert!(retries(&events).is_empty(), "no retry expected");
    assert!(first_error(&events).is_none(), "no error expected");
    assert!(discarded_messages(&events).is_empty(), "nothing to discard");
    let answer = final_answer(&events).expect("final answer expected");
    assert_eq!(answer.text_content(), "OK0|OK1|OK2|");
    measure("s01.final_text", answer.text_content());
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 2 — one mid-stream drop, second request succeeds
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn s02_mid_stream_drop_retries_once_then_succeeds() {
    // Attempt 1: three complete data events, then the socket is closed.
    // Attempt 2: a complete stream.
    let server = start_server(vec![dropped_after("OLD", 3), text_stream("NEW", 2)]).await;
    let (events, elapsed) = drive(&server, LoopOptions::default()).await;
    measure("s02.elapsed_ms", elapsed.as_millis());
    measure("s02.posts", server.posts());

    assert_eq!(server.posts(), 2, "one original + one retry");
    let retries = retries(&events);
    assert_eq!(retries.len(), 1, "exactly one RetryAttempt: {retries:?}");
    assert_eq!(retries[0].0, 0, "first retry carries attempt=0");
    assert!(
        retries[0].2.contains("stream incomplete (transport_error)"),
        "retry error must classify the transport failure: {}",
        retries[0].2
    );
    assert!(
        retries[0].2.contains("3 chunks received"),
        "retry error must carry the per-attempt chunk count: {}",
        retries[0].2
    );

    let discarded = discarded_messages(&events);
    assert_eq!(discarded.len(), 1, "failed attempt must close its partial");
    assert_eq!(
        discarded[0].text_content(),
        "",
        "discarded message must not carry the partial text"
    );

    let answer = final_answer(&events).expect("final answer expected");
    assert_eq!(answer.text_content(), "NEW0|NEW1|", "attempt-2 text only");
    assert!(
        !answer.text_content().contains("OLD"),
        "discarded attempt text must not leak into the final answer"
    );

    let messages = new_messages(&events).expect("AgentEnd expected");
    let text = all_assistant_text(messages);
    assert!(
        !text.contains("OLD0|"),
        "discarded partial must not be committed to context: {text}"
    );
    measure("s02.partial_discarded", "yes");
    measure("s02.retry_error", &retries[0].2);
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 2b — realistic incident shape: data then silence (no socket close)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn s02b_silence_after_partial_stream_times_out_then_retry_succeeds() {
    // The production failures were "8 chunks received ... flags=[timeout]":
    // the socket stayed open, the upstream simply stopped writing.
    let mut stalled = (0..4)
        .map(|i| Action::Data(delta(&format!("OLD{i}|"))))
        .collect::<Vec<_>>();
    stalled.push(Action::Sleep(Duration::from_secs(10)));

    let server = start_server(vec![stalled, text_stream("NEW", 2)]).await;
    let (events, elapsed) = drive(
        &server,
        LoopOptions {
            read_timeout: Duration::from_millis(120),
            ..LoopOptions::default()
        },
    )
    .await;
    measure("s02b.elapsed_ms", elapsed.as_millis());
    measure("s02b.posts", server.posts());

    assert_eq!(server.posts(), 2);
    assert!(
        elapsed >= Duration::from_millis(100),
        "must actually wait out read_timeout: {elapsed:?}"
    );
    let retries = retries(&events);
    assert_eq!(retries.len(), 1);
    assert!(
        retries[0].2.contains("transport_error") && retries[0].2.contains("timeout"),
        "silence must surface as a transport timeout: {}",
        retries[0].2
    );
    assert!(
        retries[0].2.contains("4 chunks received"),
        "chunk count must be per attempt: {}",
        retries[0].2
    );
    let answer = final_answer(&events).expect("retry must recover");
    assert_eq!(answer.text_content(), "NEW0|NEW1|");
    assert!(!answer.text_content().contains("OLD"));
    assert_eq!(discarded_messages(&events).len(), 1);
    measure("s02b.retry_error", &retries[0].2);
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 3 — two mid-stream drops, third request succeeds
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn s03_two_mid_stream_drops_retry_twice_then_succeeds() {
    let server = start_server(vec![
        dropped_after("A", 2),
        dropped_after("B", 4),
        text_stream("C", 1),
    ])
    .await;
    let (events, elapsed) = drive(&server, LoopOptions::default()).await;
    measure("s03.elapsed_ms", elapsed.as_millis());
    measure("s03.posts", server.posts());

    assert_eq!(server.posts(), 3, "one original + two retries");
    let retries = retries(&events);
    assert_eq!(retries.len(), 2, "{retries:?}");
    assert_eq!(retries[0].0, 0);
    assert_eq!(retries[1].0, 1);
    assert!(
        retries[0].2.contains("2 chunks received") && retries[1].2.contains("4 chunks received"),
        "each retry reports its own attempt's chunk count: {retries:?}"
    );
    assert_eq!(discarded_messages(&events).len(), 2);
    let answer = final_answer(&events).expect("third attempt succeeds");
    assert_eq!(answer.text_content(), "C0|");
    assert!(!answer.text_content().contains("A0|"));
    assert!(!answer.text_content().contains("B0|"));
    measure("s03.partial_discarded", "yes (2 partials)");
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 4 — three consecutive drops exhaust the retry budget
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn s04_three_mid_stream_drops_exhaust_budget() {
    // max_retries = 2 -> original + 2 retries = 3 POSTs, then a hard error.
    let server = start_server(vec![
        dropped_after("X", 8),
        dropped_after("Y", 8),
        dropped_after("Z", 8),
    ])
    .await;
    let (events, elapsed) = drive(&server, LoopOptions::default()).await;
    measure("s04.elapsed_ms", elapsed.as_millis());
    measure("s04.posts", server.posts());

    assert_eq!(server.posts(), 3, "budget is original + max_retries");
    let retries = retries(&events);
    assert_eq!(retries.len(), 2);
    assert_eq!(discarded_messages(&events).len(), 3);

    let error = first_error(&events).expect("budget exhaustion must surface an error");
    let (kind, detail, received_chunks) = expect_stream_incomplete(error);
    assert_eq!(kind.as_str(), "transport_error");
    assert_eq!(
        received_chunks, 8,
        "chunk count must belong to the LAST attempt, not the accumulated sum"
    );
    let text = error.to_string();
    assert!(text.contains("8 chunks received"), "{text}");
    assert!(
        !text.contains("24 chunks received"),
        "attempts must not be summed: {text}"
    );
    assert!(text.contains("transport_error"), "{text}");
    measure("s04.error", &text);
    measure("s04.detail", detail.unwrap_or_else(|| "<none>".into()));

    // No request may follow the terminal error.
    let before = server.posts();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(server.posts(), before, "no POST after the terminal error");
    let committed = all_assistant_text(new_messages(&events).expect("AgentEnd expected"));
    assert!(
        !committed.contains("X0|") && !committed.contains("Y0|") && !committed.contains("Z0|"),
        "discarded partials must never be committed: {committed}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 5a — 8 data chunks then silence, no retry: final classification
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn s05a_silence_after_eight_chunks_surfaces_transport_timeout() {
    let read_timeout = Duration::from_millis(120);
    let mut stalled = (0..8)
        .map(|i| Action::Data(delta(&format!("P{i}|"))))
        .collect::<Vec<_>>();
    stalled.push(Action::Sleep(Duration::from_secs(10)));

    let server = start_server(vec![stalled]).await;
    let (events, elapsed) = drive(
        &server,
        LoopOptions {
            read_timeout,
            retry: None,
            ..LoopOptions::default()
        },
    )
    .await;
    measure("s05a.read_timeout_ms", read_timeout.as_millis());
    measure("s05a.elapsed_ms", elapsed.as_millis());
    measure("s05a.posts", server.posts());

    assert_eq!(server.posts(), 1, "retry disabled: exactly one POST");
    assert!(
        elapsed >= Duration::from_millis(100),
        "must wait for read_timeout after the last byte: {elapsed:?}"
    );
    let error = first_error(&events).expect("silence must surface an error");
    let (kind, detail, received_chunks) = expect_stream_incomplete(error);
    assert_eq!(kind.as_str(), "transport_error");
    assert_eq!(received_chunks, 8, "8 data events were delivered first");
    let detail = detail.expect("transport failures carry a detail");
    // Observed detail format is `http=200 ct=... cause=transport flags=[timeout,decode]`
    // — the `transport_error` label lives on the error kind / Display, not in
    // the redacted detail itself. Assert both, without widening either.
    assert!(detail.contains("cause=transport"), "{detail}");
    assert!(detail.contains("timeout"), "{detail}");
    assert!(detail.contains("flags=[timeout"), "{detail}");
    assert!(error.to_string().contains("transport_error"), "{error}");
    assert_eq!(discarded_messages(&events).len(), 1);
    measure("s05a.error", error.to_string());
    measure("s05a.detail", &detail);
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 5a-retry — same stall twice, retry enabled: POST=2
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn s05a_retry_silence_attempts_twice_then_errors() {
    let read_timeout = Duration::from_millis(120);
    let mut stalled = (0..8)
        .map(|i| Action::Data(delta(&format!("Q{i}|"))))
        .collect::<Vec<_>>();
    stalled.push(Action::Sleep(Duration::from_secs(10)));

    // Both attempts hit the same silent server.
    let server = start_server(vec![stalled.clone(), stalled]).await;
    let (events, elapsed) = drive(
        &server,
        LoopOptions {
            read_timeout,
            retry: Some(RetryConfig::new(1, 0)),
            ..LoopOptions::default()
        },
    )
    .await;
    measure("s05a_retry.read_timeout_ms", read_timeout.as_millis());
    measure("s05a_retry.elapsed_ms", elapsed.as_millis());
    measure("s05a_retry.posts", server.posts());

    assert_eq!(server.posts(), 2, "retry enabled -> one extra real request");
    let retries = retries(&events);
    assert_eq!(retries.len(), 1);
    assert!(
        retries[0].2.contains("timeout") && retries[0].2.contains("8 chunks received"),
        "retry must carry the first attempt's transport timeout: {}",
        retries[0].2
    );
    let error = first_error(&events).expect("retries exhausted");
    let (_, _, received_chunks) = expect_stream_incomplete(error);
    assert_eq!(
        received_chunks, 8,
        "chunk count is the last attempt's, not cumulative"
    );
    assert!(
        elapsed >= Duration::from_millis(200),
        "two read_timeout waits are expected: {elapsed:?}"
    );
    assert_eq!(discarded_messages(&events).len(), 2);
    measure("s05a_retry.error", error.to_string());
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 5b — silence BEFORE the first body byte
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn s05b_silence_before_first_body_byte_respects_read_timeout() {
    let read_timeout = Duration::from_millis(120);
    // Headers only, then nothing at all (no data event, connection open).
    let server = start_server(vec![vec![Action::Sleep(Duration::from_secs(10))]]).await;
    let (events, elapsed) = drive(
        &server,
        LoopOptions {
            read_timeout,
            retry: None,
            ..LoopOptions::default()
        },
    )
    .await;
    measure("s05b.read_timeout_ms", read_timeout.as_millis());
    measure("s05b.elapsed_ms", elapsed.as_millis());
    measure("s05b.posts", server.posts());

    assert_eq!(server.posts(), 1);
    assert!(
        elapsed >= Duration::from_millis(100),
        "read_timeout must cover the wait for the FIRST body byte: {elapsed:?}"
    );
    let error = first_error(&events).expect("silence must surface an error");
    let (kind, detail, received_chunks) = expect_stream_incomplete(error);
    assert_eq!(kind.as_str(), "transport_error");
    assert_eq!(received_chunks, 0, "no SSE event was ever delivered");
    assert_eq!(
        discarded_messages(&events).len(),
        1,
        "the orphaned MessageStart must be closed with a discarded MessageEnd"
    );
    let detail = detail.expect("transport failures carry a detail");
    assert!(detail.contains("timeout"), "{detail}");
    measure("s05b.error", error.to_string());
    measure("s05b.detail", &detail);
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 6a — do comment/heartbeat bytes reset read_timeout?
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn s06a_heartbeat_comments_reset_read_timeout() {
    let read_timeout = Duration::from_millis(150);
    let beat_every = Duration::from_millis(50);
    let beats = 10;
    let mut script = Vec::new();
    for _ in 0..beats {
        script.push(Action::Comment);
        script.push(Action::Sleep(beat_every));
    }
    // Heartbeats stop, connection stays open: only read_timeout can end this.
    script.push(Action::Sleep(Duration::from_secs(10)));

    let server = start_server(vec![script]).await;
    let (events, elapsed) = drive(
        &server,
        LoopOptions {
            read_timeout,
            retry: None,
            ..LoopOptions::default()
        },
    )
    .await;
    let beat_phase = beat_every * beats;
    measure("s06a.read_timeout_ms", read_timeout.as_millis());
    measure("s06a.heartbeat_phase_ms", beat_phase.as_millis());
    measure("s06a.elapsed_ms", elapsed.as_millis());
    measure("s06a.posts", server.posts());

    assert_eq!(server.posts(), 1);
    assert!(
        elapsed >= Duration::from_millis(450),
        "if comments reset read_timeout, the timeout can only fire after the \
         {beat_phase:?} heartbeat phase (measured {elapsed:?})"
    );
    let error = first_error(&events).expect("silence after heartbeats must time out");
    let (kind, detail, received_chunks) = expect_stream_incomplete(error);
    assert_eq!(kind.as_str(), "transport_error");
    assert_eq!(
        received_chunks, 0,
        "comments are not SSE data events, so no chunk is counted"
    );
    let detail = detail.expect("transport failures carry a detail");
    assert!(detail.contains("timeout"), "{detail}");
    assert_eq!(discarded_messages(&events).len(), 1);
    measure("s06a.verdict", "comment bytes DO reset read_timeout");
    measure("s06a.detail", &detail);
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 6b — heartbeats never reach the harness, so the idle watchdog fires
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn s06b_heartbeat_comments_do_not_feed_idle_watchdog() {
    let read_timeout = Duration::from_millis(1000);
    let idle_timeout_ms = 250u64;
    let beat_every = Duration::from_millis(50);
    let mut script = Vec::new();
    for _ in 0..40 {
        script.push(Action::Comment);
        script.push(Action::Sleep(beat_every));
    }

    let server = start_server(vec![script]).await;
    let (events, elapsed) = drive(
        &server,
        LoopOptions {
            read_timeout,
            retry: None,
            stream_idle_timeout_ms: Some(idle_timeout_ms),
            ..LoopOptions::default()
        },
    )
    .await;
    measure("s06b.idle_timeout_ms", idle_timeout_ms);
    measure("s06b.read_timeout_ms", read_timeout.as_millis());
    measure("s06b.elapsed_ms", elapsed.as_millis());
    measure("s06b.posts", server.posts());

    assert_eq!(server.posts(), 1);
    match first_error(&events).expect("watchdog must fire") {
        AgentError::StreamIdle { timeout_ms } => assert_eq!(*timeout_ms, idle_timeout_ms),
        other => panic!("heartbeats must not satisfy the idle watchdog, got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_millis(1000),
        "watchdog must fire while heartbeats are still flowing: {elapsed:?}"
    );
    assert_eq!(discarded_messages(&events).len(), 1);
    measure(
        "s06b.verdict",
        "stream_idle_timeout_ms counts only StreamEvents; heartbeat bytes do not reset it",
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 6c — heartbeats then a clean EOF classify as eof_without_done
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn s06c_heartbeat_then_clean_eof_is_eof_without_done() {
    let read_timeout = Duration::from_millis(150);
    let beat_every = Duration::from_millis(30);
    let beats = 6; // 180ms > read_timeout, all covered by comment bytes
    let mut script = Vec::new();
    for _ in 0..beats {
        script.push(Action::Comment);
        script.push(Action::Sleep(beat_every));
    }
    script.push(Action::EndChunked);
    script.push(Action::Close);

    let server = start_server(vec![script]).await;
    let (events, elapsed) = drive(
        &server,
        LoopOptions {
            read_timeout,
            retry: None,
            ..LoopOptions::default()
        },
    )
    .await;
    measure("s06c.read_timeout_ms", read_timeout.as_millis());
    measure("s06c.elapsed_ms", elapsed.as_millis());
    measure("s06c.posts", server.posts());

    assert_eq!(server.posts(), 1);
    assert!(
        elapsed >= Duration::from_millis(150),
        "streaming must have lasted longer than read_timeout without a timeout: {elapsed:?}"
    );
    let error = first_error(&events).expect("EOF without [DONE] must error");
    let (kind, detail, received_chunks) = expect_stream_incomplete(error);
    assert_eq!(kind.as_str(), "eof_without_done");
    assert_eq!(received_chunks, 0);
    assert_eq!(discarded_messages(&events).len(), 1);
    let detail = detail.unwrap_or_default();
    // A clean EOF must not be classified as a transport failure. Do not test
    // for the bare token `timeout`: the diagnostic detail legitimately carries
    // the configuration field `read_timeout_ms`, so a substring check would
    // conflate configuration with classification. The transport cause marker
    // is only emitted on the transport-error path.
    assert!(
        !detail.contains("cause=transport"),
        "clean EOF after heartbeats must not be a transport failure: {detail}"
    );
    assert!(
        !detail.contains("flags="),
        "a clean EOF has no transport error flags: {detail}"
    );
    measure("s06c.error", error.to_string());
    measure("s06c.detail", &detail);
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 7 — a completed tool turn must not be replayed by a stream retry
// ─────────────────────────────────────────────────────────────────────────────

struct CountingTool {
    calls: Arc<AtomicUsize>,
}

impl Tool for CountingTool {
    fn name(&self) -> &str {
        "count_probe"
    }

    fn description(&self) -> &str {
        "Counts how many times the loop actually executed it."
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
            serde_json::json!({
                "type": "object",
                "properties": {"n": {"type": "integer"}},
            })
        });
        &SCHEMA
    }

    fn execute<'a>(&'a self, _args: serde_json::Value, _ctx: &'a ToolContext) -> ToolFuture<'a> {
        let calls = Arc::clone(&self.calls);
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult::full(
                vec![],
                serde_json::json!({"ok": true}),
                false,
            ))
        })
    }
}

#[tokio::test]
async fn s07_stream_drop_after_tool_turn_does_not_replay_tool() {
    let calls = Arc::new(AtomicUsize::new(0));

    let tool_turn = vec![
        Action::Data(tool_call_chunk("call_tool_1", "count_probe", "{\"n\":1}")),
        Action::Data(DONE.to_string()),
        Action::EndChunked,
    ];
    // Turn 2: partial text then an abrupt drop; the retry (request 3) succeeds.
    let turn2 = dropped_after("PARTIAL_JUNK_", 2);
    let turn3 = text_stream("FINAL_OK_", 2);

    let server = start_server(vec![tool_turn, turn2, turn3]).await;
    let (events, elapsed) = drive(
        &server,
        LoopOptions {
            read_timeout: Duration::from_millis(500),
            tools: vec![Arc::new(CountingTool {
                calls: Arc::clone(&calls),
            })],
            ..LoopOptions::default()
        },
    )
    .await;
    measure("s07.elapsed_ms", elapsed.as_millis());
    measure("s07.posts", server.posts());
    measure("s07.tool_executions", calls.load(Ordering::SeqCst));

    assert_eq!(server.posts(), 3, "tool turn + failed turn + retry");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the completed tool turn must not be replayed"
    );
    let retries = retries(&events);
    assert_eq!(retries.len(), 1, "only the text turn retried: {retries:?}");
    assert_eq!(discarded_messages(&events).len(), 1);

    let answer = final_answer(&events).expect("turn 3 provides the final answer");
    let text = answer.text_content();
    assert_eq!(text, "FINAL_OK_0|FINAL_OK_1|");
    assert!(!text.contains("PARTIAL_JUNK"), "{text}");

    // The discarded partial must never be persisted or resent.
    let committed = all_assistant_text(new_messages(&events).expect("AgentEnd expected"));
    assert!(
        !committed.contains("PARTIAL_JUNK"),
        "discarded partial leaked into committed context: {committed}"
    );
    let bodies = server.bodies();
    assert_eq!(bodies.len(), 3);
    assert!(!bodies[1].contains("PARTIAL_JUNK"), "request 2 body");
    assert!(
        !bodies[2].contains("PARTIAL_JUNK"),
        "retry request must not resend the discarded partial"
    );
    assert!(
        bodies[2].contains("call_tool_1"),
        "retry request must still carry the committed tool result"
    );
    measure("s07.partial_discarded", "yes");
    measure("s07.tool_replayed", "no");
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 8 — stall recovery vs read_timeout ratio (millisecond-scaled)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn s08a_stall_below_read_timeout_keeps_the_generation() {
    let read_timeout = Duration::from_millis(100);
    let stall = Duration::from_millis(50);
    let server = start_server(vec![text_stream_with_gap("S", 8, Some(stall))]).await;
    let (events, elapsed) = drive(
        &server,
        LoopOptions {
            read_timeout,
            retry: Some(RetryConfig::new(2, 0)),
            ..LoopOptions::default()
        },
    )
    .await;
    measure("s08a.read_timeout_ms", read_timeout.as_millis());
    measure("s08a.stall_ms", stall.as_millis());
    measure("s08a.elapsed_ms", elapsed.as_millis());
    measure("s08a.posts", server.posts());
    measure("s08a.retries", retries(&events).len());

    assert_eq!(server.posts(), 1, "a sub-timeout stall must not fail");
    assert!(retries(&events).is_empty());
    assert!(first_error(&events).is_none());
    let answer = final_answer(&events).expect("generation survives the stall");
    assert_eq!(answer.text_content(), expected_text("S", 8));
    measure("s08a.success", "yes");
}

#[tokio::test]
async fn s08b_stall_above_read_timeout_discards_and_regenerates() {
    let read_timeout = Duration::from_millis(100);
    let stall = Duration::from_millis(150);
    // Attempt 1 stalls past the timeout; attempt 2 is a fresh, complete stream.
    let first = text_stream_with_gap("OLD", 8, Some(stall));
    let second = text_stream_with_gap("NEW", 8, None);
    let server = start_server(vec![first, second]).await;
    let (events, elapsed) = drive(
        &server,
        LoopOptions {
            read_timeout,
            retry: Some(RetryConfig::new(2, 0)),
            ..LoopOptions::default()
        },
    )
    .await;
    measure("s08b.read_timeout_ms", read_timeout.as_millis());
    measure("s08b.stall_ms", stall.as_millis());
    measure("s08b.elapsed_ms", elapsed.as_millis());
    measure("s08b.posts", server.posts());
    measure("s08b.retries", retries(&events).len());

    assert_eq!(server.posts(), 2, "the stalled attempt is retried");
    assert_eq!(retries(&events).len(), 1);
    assert_eq!(discarded_messages(&events).len(), 1);
    let answer = final_answer(&events).expect("second attempt succeeds");
    assert_eq!(
        answer.text_content(),
        expected_text("NEW", 8),
        "only the regenerated attempt is kept"
    );
    let text = answer.text_content();
    assert!(
        !text.contains("OLD0|"),
        "the 8 data blocks of the discarded attempt must be dropped: {text}"
    );
    measure("s08b.old_chunks_discarded", "yes");
    measure("s08b.success", "yes");
}

#[tokio::test]
async fn s08c_scaled_read_timeout_rescues_the_same_stall() {
    // 3x the read_timeout of 8a/8b (millisecond analogue of 60s -> 180s),
    // with the same absolute 150ms stall that failed in 8b.
    let read_timeout = Duration::from_millis(300);
    let stall = Duration::from_millis(150);
    let server = start_server(vec![text_stream_with_gap("R", 8, Some(stall))]).await;
    let (events, elapsed) = drive(
        &server,
        LoopOptions {
            read_timeout,
            retry: Some(RetryConfig::new(2, 0)),
            ..LoopOptions::default()
        },
    )
    .await;
    measure("s08c.read_timeout_ms", read_timeout.as_millis());
    measure("s08c.stall_ms", stall.as_millis());
    measure("s08c.elapsed_ms", elapsed.as_millis());
    measure("s08c.posts", server.posts());
    measure("s08c.retries", retries(&events).len());

    assert_eq!(server.posts(), 1, "the raised threshold absorbs the stall");
    assert!(retries(&events).is_empty());
    let answer = final_answer(&events).expect("generation survives with 3x timeout");
    assert_eq!(answer.text_content(), expected_text("R", 8));
    measure("s08c.success", "yes");
}
