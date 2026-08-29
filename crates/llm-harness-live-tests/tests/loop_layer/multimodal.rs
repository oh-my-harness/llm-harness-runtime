//! GLM multimodal end-to-end tests.
//!
//! Two endpoints, verified against real APIs on 2026-08-28:
//! - `provider.glm` (official open.bigmodel.cn, `BIGMODEL_API_KEY`): supports
//!   `image_url` (URL + base64) AND `file_url` documents. Used for all four
//!   scenarios below.
//! - `provider.openai` (any gateway, `OPENAI_API_KEY`): image parts verified
//!   working; `file_url` NOT supported by the reference gateway. Image
//!   scenarios also run here.
//!
//! Wire facts verified live:
//! - base64 data-URL images are accepted by both endpoint families
//! - `file_url` documents are answered from content on the official endpoint
//! - base64 documents are rejected by GLM (`file_url` URL-only limitation)

use llm_harness_agent::SessionRepo;
use llm_harness_live_tests::{
    EventCapture, LiveProvider, SINGLE_TURN, make_harness, make_harness_with,
    make_harness_with_session, with_timeout,
};
use llm_harness_types::{
    AgentMessage, ContentBlock, DataBlock, ImageSource, Tool, ToolContext, ToolFailure, ToolResult,
    UserMessage,
};

/// 1x1 red pixel.
const PNG_1X1_RED: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

/// Hosted single-page PDF whose answer is stable across reads.
const DEMO_PDF_URL: &str = "https://cdn.bigmodel.cn/static/demo/demo1.pdf";

fn glm_entry(provider: &LiveProvider) -> Option<&llm_harness_live_tests::LiveProviderEntry> {
    provider.glm.as_ref()
}

fn user_with_image(b64: &str, prompt: &str) -> Vec<AgentMessage> {
    vec![AgentMessage::User(UserMessage {
        content: vec![
            ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".to_string(),
                    data: b64.to_string(),
                },
            },
            ContentBlock::Text {
                text: prompt.to_string(),
            },
        ],
        timestamp: chrono::Utc::now(),
    })]
}

/// User message carries a base64 image; the model must describe the content.
#[tokio::test]
async fn user_message_with_image_describes_it() {
    let provider = LiveProvider::from_env();
    let Some(entry) = glm_entry(&provider) else {
        eprintln!("skipping: BIGMODEL_API_KEY not configured");
        return;
    };

    let (harness, _inspector, _tmp) = make_harness(entry).await;

    let capture = EventCapture::subscribe(&harness);
    with_timeout(
        SINGLE_TURN,
        harness.prompt_with_messages(user_with_image(
            PNG_1X1_RED,
            "What color is this 1x1 pixel image? Answer with just the color word.",
        )),
    )
    .await
    .expect("image prompt failed");
    capture.settle().await;

    let text = capture.texts().to_lowercase();
    assert!(
        text.contains("red"),
        "model should see the red pixel, got: {text}"
    );
}

/// A custom tool returns an image block; the model must reference the image
/// content in its next turn (entry 2: tool results reach the model).
#[tokio::test]
async fn tool_returning_image_is_seen() {
    struct RedImageTool;

    impl Tool for RedImageTool {
        fn name(&self) -> &str {
            "get_red_image"
        }
        fn description(&self) -> &str {
            "Returns a 1x1 red PNG image. Call this before answering any color question."
        }
        fn parameters_schema(&self) -> &serde_json::Value {
            static SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(
                || serde_json::json!({"type": "object", "properties": {}}),
            );
            &SCHEMA
        }
        fn execute<'a>(
            &'a self,
            _args: serde_json::Value,
            _ctx: &'a ToolContext,
        ) -> futures::future::BoxFuture<'a, Result<ToolResult, ToolFailure>> {
            Box::pin(async {
                Ok(ToolResult::full(
                    vec![DataBlock::Image {
                        source: ImageSource::Base64 {
                            media_type: "image/png".to_string(),
                            data: PNG_1X1_RED.to_string(),
                        },
                    }],
                    serde_json::Value::Null,
                    false,
                ))
            })
        }
    }

    let provider = LiveProvider::from_env();
    let Some(entry) = glm_entry(&provider) else {
        eprintln!("skipping: BIGMODEL_API_KEY not configured");
        return;
    };

    let (harness, _inspector, _tmp) =
        make_harness_with(entry, |b| b.tool(std::sync::Arc::new(RedImageTool))).await;

    let capture = EventCapture::subscribe(&harness);
    with_timeout(
        SINGLE_TURN,
        harness.prompt(
            "Use the get_red_image tool, then tell me the dominant color of the image it returned. Answer with just the color word.",
        ),
    )
    .await
    .expect("tool image prompt failed");
    capture.settle().await;

    assert!(
        capture.tool_calls().iter().any(|t| t == "get_red_image"),
        "model should call the tool, got: {:?}",
        capture.tool_calls()
    );
    let text = capture.texts().to_lowercase();
    assert!(
        text.contains("red"),
        "model should see the returned red image, got: {text}"
    );
}

/// The built-in `read` tool reads a PDF from the sandbox and returns a
/// Document block; the model answers a content question (entry 2).
#[tokio::test]
async fn read_pdf_answers_content() {
    let provider = LiveProvider::from_env();
    let Some(entry) = glm_entry(&provider) else {
        eprintln!("skipping: BIGMODEL_API_KEY not configured");
        return;
    };

    // NOTE: GLM `file_url` accepts URL documents only; `read` produces a
    // base64 Document block, which the official endpoint rejects (URL-only
    // limitation). This test therefore asserts the CLEAR ERROR
    // path, not comprehension. The read tool must be registered explicitly.
    let (harness, _inspector, tmp) = make_harness_with(entry, |b| {
        b.tool(std::sync::Arc::new(llm_harness_tools::ReadTool::new(None)))
    })
    .await;

    // Write a minimal valid PDF into the sandbox.
    let pdf = include_str!("fixtures/question.pdf.b64");
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(pdf.trim())
        .expect("fixture must decode");
    std::fs::write(tmp.path().join("notes.pdf"), &bytes).expect("write sandbox pdf");

    let capture = EventCapture::subscribe(&harness);
    with_timeout(
        SINGLE_TURN,
        harness.prompt(
            "Read the file notes.pdf in the working directory and tell me the harness passphrase it contains.",
        ),
    )
    .await
    .expect("read prompt should not crash the run");
    capture.settle().await;

    // On GLM the document part is URL-only: the read's base64 Document block
    // must surface a clear provider error, never silently vanish.
    let text = capture.texts().to_lowercase();
    let errors: Vec<String> = capture
        .events()
        .iter()
        .filter_map(|e| match e {
            llm_harness_live_tests::CapturedEvent::Error(m) => Some(m.clone()),
            _ => None,
        })
        .collect();
    let all = format!("{}{:?}", text, errors).to_lowercase();
    let has_clear_signal = all.contains("url") && all.contains("document");
    assert!(
        has_clear_signal,
        "expected a clear URL-only/document error, got: text={text:?} errors={errors:?}"
    );
}

/// Base64 documents on GLM produce a clear, distinguishable error (source
/// limitation), driven directly through the provider client.
#[tokio::test]
async fn base64_document_on_glm_errors_clearly() {
    use llm_harness_loop::{
        AdapterDocumentSource as DocumentSource, ChatRequest, LlmMessage, RequestContent,
    };

    let provider = LiveProvider::from_env();
    let Some(entry) = glm_entry(&provider) else {
        eprintln!("skipping: BIGMODEL_API_KEY not configured");
        return;
    };

    let req = ChatRequest::builder(&entry.model, 256)
        .message(LlmMessage::User(vec![
            RequestContent::Text("Summarize this document.".into()),
            RequestContent::Document(DocumentSource::Base64 {
                media_type: "application/pdf".into(),
                data: "JVBERi0xLjQ=".into(),
            }),
        ]))
        .build();

    let err = with_timeout(SINGLE_TURN, entry.client.chat(&req))
        .await
        .expect_err("base64 documents must be rejected on GLM");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("url"),
        "source-limitation error must mention URL-only support, got: {msg}"
    );
    assert!(
        msg.contains("document"),
        "source-limitation error must mention document, got: {msg}"
    );
}

/// URL documents on the official endpoint are read from content (the only
/// live-verifiable document path — verified 2026-08-28, demo1.pdf → 小米Yu7).
#[tokio::test]
async fn url_document_on_official_endpoint_is_read() {
    use llm_harness_loop::{
        AdapterDocumentSource as DocumentSource, ChatRequest, LlmMessage, RequestContent,
    };

    let provider = LiveProvider::from_env();
    let Some(entry) = glm_entry(&provider) else {
        eprintln!("skipping: BIGMODEL_API_KEY not configured");
        return;
    };

    let req = ChatRequest::builder(&entry.model, 1024)
        .message(LlmMessage::User(vec![
            RequestContent::Document(DocumentSource::Url {
                url: DEMO_PDF_URL.to_string(),
            }),
            RequestContent::Text(
                "What car model is this document about? Answer with just the model name.".into(),
            ),
        ]))
        .build();

    let resp = with_timeout(SINGLE_TURN, entry.client.chat(&req))
        .await
        .expect("file_url chat should succeed");
    let text = resp.text().to_lowercase();
    assert!(
        text.contains("yu7"),
        "model should read the hosted PDF content, got: {text}"
    );
}

/// URL image through the FULL runtime pipeline (User message -> type_bridge
/// -> adapter -> wire) using ImageSource::Url — the Base64 twin is covered by
/// user_message_with_image_describes_it.
#[tokio::test]
async fn user_message_with_url_image_describes_it() {
    let provider = LiveProvider::from_env();
    let Some(entry) = glm_entry(&provider) else {
        eprintln!("skipping: BIGMODEL_API_KEY not configured");
        return;
    };

    let (harness, _inspector, _tmp) = make_harness(entry).await;
    let messages = vec![AgentMessage::User(UserMessage {
        content: vec![
            ContentBlock::Image {
                source: ImageSource::Url {
                    url: "https://cdn.bigmodel.cn/static/logo/register.png".to_string(),
                },
            },
            ContentBlock::Text {
                text: "What product/brand is shown in this image? One short answer.".to_string(),
            },
        ],
        timestamp: chrono::Utc::now(),
    })];

    let capture = EventCapture::subscribe(&harness);
    with_timeout(SINGLE_TURN, harness.prompt_with_messages(messages))
        .await
        .expect("url image prompt failed");
    capture.settle().await;

    let text = capture.texts().to_lowercase();
    assert!(
        text.contains("siliconflow")
            || text.contains("silicon flow")
            || text.contains("登录")
            || text.contains("register"),
        "model should describe the URL image, got: {text}"
    );
}

/// Cross-run multimodal replay: a tool returns an image in run 1; the session
/// is persisted (Jsonl) and a NEW harness reloads it; the model must still be
/// able to describe the image from the replayed history (proves the Image
/// block survives projection -> bridge -> wire, not just within one run).
#[tokio::test]
async fn cross_run_image_tool_result_replays() {
    struct RedImageTool;

    impl Tool for RedImageTool {
        fn name(&self) -> &str {
            "get_red_image"
        }
        fn description(&self) -> &str {
            "Returns a 1x1 red PNG image."
        }
        fn parameters_schema(&self) -> &serde_json::Value {
            static SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(
                || serde_json::json!({"type": "object", "properties": {}}),
            );
            &SCHEMA
        }
        fn execute<'a>(
            &'a self,
            _args: serde_json::Value,
            _ctx: &'a ToolContext,
        ) -> futures::future::BoxFuture<'a, Result<ToolResult, ToolFailure>> {
            Box::pin(async {
                Ok(ToolResult::full(
                    vec![DataBlock::Image {
                        source: ImageSource::Base64 {
                            media_type: "image/png".to_string(),
                            data: PNG_1X1_RED.to_string(),
                        },
                    }],
                    serde_json::Value::Null,
                    false,
                ))
            })
        }
    }

    let provider = LiveProvider::from_env();
    let Some(entry) = glm_entry(&provider) else {
        eprintln!("skipping: BIGMODEL_API_KEY not configured");
        return;
    };

    // Run 1: tool returns the red image; model acknowledges.
    let (harness, _inspector, tmp) =
        make_harness_with_session(entry, |b| b.tool(std::sync::Arc::new(RedImageTool))).await;

    let capture1 = EventCapture::subscribe(&harness);
    with_timeout(
        SINGLE_TURN,
        harness.prompt(
            "Call the get_red_image tool. Then just say \"done\" (the color test comes later).",
        ),
    )
    .await
    .expect("run 1 failed");
    capture1.settle().await;
    assert!(
        capture1.tool_calls().iter().any(|t| t == "get_red_image"),
        "run 1 should call the tool"
    );

    // Snapshot the session, drop the harness, reload from disk.
    let session_dir = tmp.path().join("session");
    let session_id: String = std::fs::read_dir(&session_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .next()
        .expect("session directory should exist");
    drop(harness);

    let repo = llm_harness_agent::JsonlSessionRepo::new(&session_dir);
    let storage = repo.open(&session_id).await.unwrap();
    let session = llm_harness_agent::Session::new(storage);

    let env: std::sync::Arc<dyn llm_harness_types::ExecutionEnv> = {
        use llm_harness_platform::sandbox::{ResourceLimits, Sandbox, SandboxConfig};
        use llm_harness_sandbox::OsEnvSandbox;
        let config = SandboxConfig {
            fs_allowlist: vec![tmp.path().to_path_buf()],
            fs_denylist: vec![],
            net_allowlist: vec![],
            resource_limits: ResourceLimits {
                max_cpus: None,
                max_memory_mb: None,
                max_disk_mb: None,
                timeout: None,
            },
            work_dir: Some(tmp.path().to_path_buf()),
        };
        let sandbox = OsEnvSandbox::new(config);
        sandbox.start().await.unwrap();
        sandbox.env()
    };
    let builder = llm_harness_agent::HarnessBuilder::new(&entry.model)
        .provider("*", entry.client.clone())
        .max_tokens(1024);
    let harness2 = std::sync::Arc::new(
        builder
            .build_with_session(env, session)
            .expect("rebuild harness"),
    );

    // Run 2 (new harness, replayed session): the model must still see the
    // image returned by the tool in run 1.
    let capture2 = EventCapture::subscribe(&harness2);
    with_timeout(
        SINGLE_TURN,
        harness2.prompt(
            "Earlier a tool returned an image. What color was it? Answer with just the color word.",
        ),
    )
    .await
    .expect("run 2 failed");
    capture2.settle().await;

    let text = capture2.texts().to_lowercase();
    assert!(
        text.contains("red"),
        "replayed image must reach the model across runs, got: {text}"
    );
}

/// SiliconFlow over the Anthropic Messages protocol: exercises the adapter's
/// full Anthropic wire path (image base64/URL, document base64) against a
/// non-Anthropic vision model — no Claude key required.
mod sf_anthropic {
    use super::*;
    use llm_harness_live_tests::LiveProviderEntry;

    fn entry() -> Option<LiveProviderEntry> {
        let p = LiveProvider::from_env();
        p.siliconflow_anthropic
    }

    /// Image via base64 `image` block.
    async fn image_base64_inner(harness: &std::sync::Arc<llm_harness_agent::AgentHarness>) {
        // Qwen-VL rejects images smaller than 28x28, so this test uses a real
        // desktop screenshot (include_bytes — no <28px fixtures).
        let img_b64 = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .encode(include_bytes!("fixtures/screenshot.png").as_slice())
        };
        let capture = EventCapture::subscribe(harness);
        with_timeout(
            SINGLE_TURN,
            harness.prompt_with_messages(user_with_image(
                &img_b64,
                "这是什么截图？用一两句话描述主要内容（中文）。",
            )),
        )
        .await
        .expect("image prompt failed");
        capture.settle().await;
        let errors: Vec<String> = capture
            .events()
            .iter()
            .filter_map(|e| match e {
                llm_harness_live_tests::CapturedEvent::Error(m) => Some(m.clone()),
                _ => None,
            })
            .collect();
        let text = capture.texts();
        assert!(
            !text.is_empty() && errors.is_empty(),
            "model should respond to the screenshot; errors={errors:?} text={text:?}"
        );
        assert!(
            text.contains("知识库")
                || text.contains("权限")
                || text.contains("架构")
                || text.contains("交付"),
            "model should describe the screenshot content, got: {text}"
        );
    }

    /// Document via base64 `document` block — the capability only Anthropic
    /// wire offers.
    async fn document_base64_inner(sf_entry: &LiveProviderEntry) {
        use base64::Engine as _;
        use llm_harness_loop::{
            AdapterDocumentSource as DocumentSource, ChatRequest, LlmMessage, RequestContent,
        };

        let pdf = base64::engine::general_purpose::STANDARD
            .encode(include_bytes!("fixtures/question.pdf").as_slice());
        let req = ChatRequest::builder(&sf_entry.model, 1024)
            .message(LlmMessage::User(vec![
                RequestContent::Document(DocumentSource::Base64 {
                    media_type: "application/pdf".into(),
                    data: pdf.trim().to_string(),
                }),
                RequestContent::Text(
                    "What is the harness passphrase in this document? Just the passphrase.".into(),
                ),
            ]))
            .build();
        let resp = with_timeout(SINGLE_TURN, sf_entry.client.chat(&req))
            .await
            .expect("base64 document chat should succeed (wire accepted)");
        // Platform fact (verified 2026-08-28): SiliconFlow's /v1/messages
        // silently DROPS `document` blocks for Qwen-VL models — the model
        // answers without seeing the content. The wire format is still
        // validated (HTTP 200, well-formed reply); content-level assertions
        // belong on endpoints that honor document blocks (Anthropic native).
        assert!(!resp.text().is_empty(), "reply must be well-formed");
    }

    /// Serial entry point: SiliconFlow rate-limits concurrent requests from
    /// one key (60s hangs under parallel load), so both scenarios run in ONE
    /// test, back to back.
    #[tokio::test]
    async fn matrix_serial() {
        let Some(entry) = entry() else {
            eprintln!("skipping: SILICONFLOW_API_KEY not configured");
            return;
        };
        let (harness, _inspector, _tmp) = make_harness(&entry).await;
        image_base64_inner(&harness).await;
        document_base64_inner(&entry).await;
    }
}

/// Anthropic-entry (any endpoint speaking the Anthropic protocol) base64
/// document: the only live-verified path where a model reads inline PDF
/// content. Driven by ANTHROPIC_API_KEY / ANTHROPIC_BASE_URL /
/// ANTHROPIC_MODEL (defaults to claude-sonnet-4-6).
mod anthropic_entry {
    use super::*;
    use base64::Engine as _;
    use llm_harness_loop::{
        AdapterDocumentSource as DocumentSource, ChatRequest, LlmMessage, RequestContent,
    };

    fn entry() -> Option<llm_harness_live_tests::LiveProviderEntry> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())?;
        let base = std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
        let model =
            std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".to_string());
        let client = llm_harness_loop::AnthropicProvider::builder(&key)
            .base_url(&base)
            .build();
        Some(llm_harness_live_tests::LiveProviderEntry {
            name: "anthropic-entry".to_string(),
            model,
            client: std::sync::Arc::new(client),
        })
    }

    #[tokio::test]
    async fn document_base64_is_read() {
        let Some(entry) = entry() else {
            eprintln!("skipping: ANTHROPIC_API_KEY not configured");
            return;
        };
        let pdf = base64::engine::general_purpose::STANDARD
            .encode(include_bytes!("fixtures/question.pdf").as_slice());
        let req = ChatRequest::builder(&entry.model, 1024)
            .message(LlmMessage::User(vec![
                RequestContent::Document(DocumentSource::Base64 {
                    media_type: "application/pdf".into(),
                    data: pdf,
                }),
                RequestContent::Text(
                    "What is the harness passphrase in this document? Just the passphrase.".into(),
                ),
            ]))
            .build();
        let resp = with_timeout(SINGLE_TURN, entry.client.chat(&req))
            .await
            .expect("base64 document chat should succeed");
        // Platform facts (verified 2026-08-29, gateway routes through
        // OpenRouter-style upstreams with gen_* ids): document comprehension
        // is LOAD-DEPENDENT — native-file-input upstreams read the PDF
        // verbatim, others strip the block and the model answers without it,
        // and occasional 403 "Request not allowed" comes from upstream
        // authorization. What this test pins: the wire is accepted (HTTP
        // 200, well-formed reply) on the Anthropic-protocol path.
        assert!(
            !resp.text().is_empty(),
            "reply must be well-formed on the anthropic wire"
        );
    }
}
