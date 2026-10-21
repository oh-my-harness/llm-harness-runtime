//! Loopback HTTP/SSE fault fixture. Receives and counts complete POST bodies.
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(dead_code)] // each test binary compiles this support module independently
pub enum Fault {
    DropPost,
    PartialSse,
    Http502,
    Http504,
    Http408,
    Invalid,
    Refused,
    Idle,
    Timeout,
    RateLimit,
    RedirectRefused,
    NoTerminal,
    MalformedAction,
}
pub struct Server {
    pub url: String,
    pub posts: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn chunk(delta: serde_json::Value, finish: serde_json::Value) -> String {
    format!(
        "data: {}\n\n",
        json!({"id":"mock", "object":"chat.completion.chunk", "created":1,"model":"test-model","choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    )
}
fn tool(name: &str, args: serde_json::Value) -> String {
    let mut s = chunk(
        json!({"role":"assistant","tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":name,"arguments":args.to_string()}}]}),
        json!(null),
    );
    s += &chunk(json!({}), json!("tool_calls"));
    s += "data: {\"id\":\"mock\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"test-model\",\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\ndata: [DONE]\n\n";
    s
}
fn plain_stop() -> String {
    let mut s = chunk(
        json!({"role":"assistant","content":"done working"}),
        json!("stop"),
    );
    s += "data: {\"id\":\"mock\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"test-model\",\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\ndata: [DONE]\n\n";
    s
}
pub async fn server(fault: Fault, work: bool, success: bool, role: &str, task_id: &str) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let posts = Arc::new(AtomicUsize::new(0));
    let count = posts.clone();
    let role = role.to_owned();
    let task_id = task_id.to_owned();
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let count = count.clone();
            let role = role.clone();
            let task_id = task_id.clone();
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut buf = [0; 4096];
                loop {
                    let n = socket.read(&mut buf).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&bytes[..end]);
                        let len = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + len {
                            assert!(head.starts_with("POST "));
                            break;
                        }
                    }
                }
                let n = count.fetch_add(1, Ordering::SeqCst);
                let body = if matches!(fault, Fault::MalformedAction) {
                    // Every generation repeats the same 3-POST cycle, so a
                    // REPLAYED run re-executes the side-effect tool (POST n
                    // with n % 3 == 0) — exactly what the guard must
                    // prevent. First generation: tool (n=0), correlated but
                    // malformed ACTION delivery (n=1), plain stop (n=2).
                    match n % 3 {
                        0 => Some(tool("effect", json!({}))),
                        1 => Some(tool(
                            "send_message",
                            json!({"to":"pipeline-controller","task_id":task_id,"body":"prose first\nACTION: FINISH\nlate"}),
                        )),
                        _ => Some(plain_stop()),
                    }
                } else if matches!(fault, Fault::NoTerminal) {
                    // Normal completion: the first POST optionally runs the
                    // tool, later POSTs return plain assistant text with no
                    // tool call and no ACTION/report — the "tool progress
                    // then silent end" case.
                    if work && n == 0 {
                        Some(tool("effect", json!({})))
                    } else {
                        Some(plain_stop())
                    }
                } else if work && n == 0 {
                    Some(tool("effect", json!({})))
                } else if success {
                    let args = if role == "orchestrator" {
                        json!({"to":"pipeline-controller","task_id":task_id,"body":"ACTION: FINISH\ncomplete"})
                    } else {
                        json!({"to":"pipeline-controller","task_id":task_id,"task_kind":"result","body":"complete"})
                    };
                    Some(tool("send_message", args))
                } else {
                    None
                };
                if let Some(body) = body {
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await.unwrap();
                    return;
                }
                match fault {
                    Fault::DropPost => {}
                    Fault::Timeout => {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    Fault::RedirectRefused => {
                        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                        let location =
                            format!("http://{}/redirected", closed.local_addr().unwrap());
                        drop(closed);
                        socket.write_all(format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                    }
                    Fault::PartialSse | Fault::Idle => {
                        let body = chunk(
                            json!({"role":"assistant","content":"partial generation"}),
                            json!(null),
                        );
                        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}").as_bytes()).await.unwrap();
                        if matches!(fault, Fault::Idle) {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                    _ => {
                        let status = if matches!(fault, Fault::Invalid) {
                            "400 Bad Request"
                        } else if matches!(fault, Fault::RateLimit) {
                            "429 Too Many Requests"
                        } else if matches!(fault, Fault::Http504) {
                            "504 Gateway Timeout"
                        } else if matches!(fault, Fault::Http408) {
                            "408 Request Timeout"
                        } else {
                            "502 Bad Gateway"
                        };
                        socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 7\r\nConnection: close\r\n\r\ninvalid").as_bytes()).await.unwrap();
                    }
                }
            });
        }
    });
    Server {
        url: format!("http://{addr}"),
        posts,
        task,
    }
}
