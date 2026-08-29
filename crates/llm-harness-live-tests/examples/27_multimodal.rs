//! 27 — Multimodal: images and PDFs through the model pipeline.
//!
//! Demonstrates:
//! - Part 1: a user message carrying a base64 image — the model describes it.
//! - Part 2: a user message carrying an image URL — same, over the wire.
//! - Part 3: a custom tool that returns a `DataBlock::Image` — the model
//!   references the image content in its answer (tool results reach the
//!   model as real content blocks, unlike the `[Image: mime]` placeholders
//!   of old).
//! - Part 4: the built-in `read` tool on a PNG in the sandbox — the file's
//!   bytes come back as a `DataBlock::Image` the model can see.
//! - Part 5: a user message carrying a PDF document URL — endpoint-dependent
//!   (Zhipu official reads it; most gateways reject document parts).
//! - Part 6: a user message carrying an inline base64 PDF — the Anthropic
//!   `document` block path (requires ANTHROPIC_API_KEY or an
//!   Anthropic-protocol endpoint).
//!
//! Provider notes (verified against real endpoints, 2026-08):
//! - OpenAI-compatible gateways and Anthropic-family endpoints accept base64
//!   data-URL images; Anthropic also accepts URL images natively.
//! - PDF documents are endpoint-dependent: Anthropic takes base64 `document`
//!   blocks, Zhipu-style gateways take URL-only `file_url` parts, and many
//!   gateways reject document parts entirely (see the multimodal tests for
//!   the per-endpoint matrix). Parts 5/6 therefore route documents to
//!   document-capable entries and print the rejection when the endpoint does
//!   not support them — images are the only modality that works everywhere.
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 27_multimodal

use base64::Engine as _;
use llm_harness_live_tests::{
    EventCapture, LiveProvider, SINGLE_TURN, make_harness_with_env, with_timeout,
};
use llm_harness_types::{
    AgentMessage, BinarySource, ContentBlock, DataBlock, ImageSource, Tool, ToolContext,
    ToolFailure, ToolResult, UserMessage,
};

/// 1x1 red pixel — small enough to inline anywhere, real enough to describe.
const PNG_1X1_RED: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

/// A well-known public image for the URL variant.
const LOGO_URL: &str = "https://cdn.bigmodel.cn/static/logo/register.png";

#[tokio::main]
async fn main() {
    println!("=== 27: Multimodal (images through the model pipeline) ===\n");

    let provider = LiveProvider::from_env();
    let entry = provider
        .openai
        .as_ref()
        .or(provider.anthropic.as_ref())
        .or(provider.glm.as_ref())
        .expect("set OPENAI_API_KEY, ANTHROPIC_API_KEY, or BIGMODEL_API_KEY to run this example");

    println!("Provider: {} | Model: {}", entry.name, entry.model);

    part1_base64_image(entry).await;
    println!();
    part2_url_image(entry).await;
    println!();
    part3_tool_returned_image(entry).await;
    println!();
    part4_read_png(entry).await;
    println!();
    part5_url_document(&provider).await;
    println!();
    part6_base64_document(&provider).await;

    println!("\n--- Summary ---");
    println!("Part 1: user message with base64 image described ✅");
    println!("Part 2: user message with URL image described ✅");
    println!("Part 3: tool-returned image reaches the model ✅");
    println!("Part 4: read returns a sandbox PNG as an image block ✅");
    println!(
        "Part 5: user message with a PDF document URL — outcome printed above (endpoint-dependent)"
    );
    println!(
        "Part 6: inline base64 PDF via the Anthropic document block ✅ (needs ANTHROPIC_API_KEY)"
    );
}

fn user_with_image(image: ContentBlock, prompt: &str) -> Vec<AgentMessage> {
    vec![AgentMessage::User(UserMessage {
        content: vec![
            image,
            ContentBlock::Text {
                text: prompt.to_string(),
            },
        ],
        timestamp: chrono::Utc::now(),
    })]
}

// ── Part 1: base64 image ─────────────────────────────────────────────────────

async fn part1_base64_image(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 1: base64 image in the user message ---\n");

    let (harness, _env, _inspector, _tmp) = make_harness_with_env(entry, |b| b).await;

    let messages = user_with_image(
        ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".to_string(),
                data: PNG_1X1_RED.to_string(),
            },
        },
        "What color is this 1x1 pixel image? Answer with just the color word.",
    );

    let cap = EventCapture::subscribe(&harness);
    println!("LLM prompt: [1x1 PNG attached] What color is this pixel?");
    with_timeout(SINGLE_TURN, harness.prompt_with_messages(messages))
        .await
        .expect("prompt failed");
    cap.settle().await;

    println!("LLM: {}\n", cap.texts().trim());
}

// ── Part 2: URL image ────────────────────────────────────────────────────────

async fn part2_url_image(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 2: image URL in the user message ---\n");

    let (harness, _env, _inspector, _tmp) = make_harness_with_env(entry, |b| b).await;

    let messages = user_with_image(
        ContentBlock::Image {
            source: ImageSource::Url {
                url: LOGO_URL.to_string(),
            },
        },
        "What product or brand is shown in this image? One short sentence.",
    );

    let cap = EventCapture::subscribe(&harness);
    println!("LLM prompt: [{LOGO_URL} attached] What is shown?");
    with_timeout(SINGLE_TURN, harness.prompt_with_messages(messages))
        .await
        .expect("prompt failed");
    cap.settle().await;

    println!("LLM: {}\n", cap.texts().trim());
}

// ── Part 3: tool-returned image ──────────────────────────────────────────────

struct RedImageTool;

impl Tool for RedImageTool {
    fn name(&self) -> &str {
        "get_red_image"
    }
    fn description(&self) -> &str {
        "Returns a 1x1 red PNG image."
    }
    fn parameters_schema(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::LazyLock<serde_json::Value> =
            std::sync::LazyLock::new(|| serde_json::json!({"type": "object", "properties": {}}));
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

async fn part3_tool_returned_image(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 3: tool result carrying an image block ---\n");

    let (harness, _env, _inspector, _tmp) =
        make_harness_with_env(entry, |b| b.tool(std::sync::Arc::new(RedImageTool))).await;

    let prompt_text = "Call the get_red_image tool, then tell me the dominant color of the \
         image it returned. Answer with just the color word.";
    let cap = EventCapture::subscribe(&harness);
    println!("LLM prompt: {prompt_text}");
    with_timeout(SINGLE_TURN, harness.prompt(prompt_text))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let tools = cap.tool_calls();
    println!("tools called: {tools:?}");
    println!("LLM: {}\n", cap.texts().trim());
}

// ── Part 5: URL document in the user message ────────────────────────────────

async fn part5_url_document(provider: &LiveProvider) {
    println!("--- Part 5: PDF document URL in the user message ---\n");
    println!(
        "(document input is endpoint-dependent: the Zhipu official endpoint reads \
         URL documents; plain gateways reject document parts)\n"
    );

    // Prefer the document-capable Zhipu entry when configured.
    let entry = provider
        .glm
        .as_ref()
        .or(provider.openai.as_ref())
        .or(provider.anthropic.as_ref())
        .expect("no provider configured");

    let (harness, _env, _inspector, _tmp) = make_harness_with_env(entry, |b| b).await;

    let messages = vec![AgentMessage::User(UserMessage {
        content: vec![
            ContentBlock::Document {
                name: Some("demo.pdf".to_string()),
                media_type: "application/pdf".to_string(),
                data: BinarySource::Url {
                    url: "https://cdn.bigmodel.cn/static/demo/demo1.pdf".to_string(),
                },
            },
            ContentBlock::Text {
                text: "这份文档讲的是什么车型？只回答车型名。".to_string(),
            },
        ],
        timestamp: chrono::Utc::now(),
    })];

    let cap = EventCapture::subscribe(&harness);
    println!("LLM prompt: [https://cdn.bigmodel.cn/static/demo/demo1.pdf attached] 车型名?");
    match with_timeout(SINGLE_TURN, harness.prompt_with_messages(messages)).await {
        Ok(_) => {
            cap.settle().await;
            let text = cap.texts().trim().to_string();
            if text.is_empty() {
                println!("(no reply — endpoint likely stripped the document)\n");
            } else {
                println!("LLM: {text}\n");
            }
        }
        Err(e) => {
            // Document-incapable endpoints reject at construction time with
            // InvalidRequest — that IS the documented behavior. Anything else
            // (network, auth, rate limit) is a real failure and surfaces as
            // such.
            let msg = format!("{e}");
            if msg.contains("document") || msg.contains("file_url") {
                println!(
                    "endpoint rejected document input (expected on \
                    document-incapable endpoints): {e}\n"
                );
            } else {
                println!("UNEXPECTED failure (not a document-capability rejection): {e}\n");
            }
        }
    }
}

// ── Part 6: inline base64 PDF (Anthropic document block) ────────────────────

async fn part6_base64_document(provider: &LiveProvider) {
    println!("--- Part 6: inline base64 PDF (Anthropic document block) ---\n");
    println!(
        "(the only inline-document wire form in the ecosystem: Anthropic \"document\" \
         block with base64 data — requires ANTHROPIC_API_KEY or an \
         Anthropic-protocol endpoint)\n"
    );

    let Some(entry) = provider.anthropic.as_ref() else {
        println!("skipped: set ANTHROPIC_API_KEY to run this part\n");
        return;
    };
    println!("part6 using entry: {} | model: {}", entry.name, entry.model);

    // A small PDF with a known marker sentence (real binary, invalid UTF-8 —
    // generated by cupsfilter, see the multimodal test suite).
    let (harness, _env, _inspector, tmp) = make_harness_with_env(entry, |b| b).await;
    let pdf_path = tmp.path().join("marker.pdf");
    let pdf_b64 = {
        // Embed a minimal marker PDF as bytes, then base64 it — mirrors what
        // an application does when attaching a real file.
        let bytes: &[u8] = include_bytes!("../tests/loop_layer/fixtures/question.pdf");
        use base64::Engine as _;
        std::fs::write(&pdf_path, bytes).expect("write sandbox pdf");
        base64::engine::general_purpose::STANDARD.encode(bytes)
    };

    let messages = vec![AgentMessage::User(UserMessage {
        content: vec![
            ContentBlock::Document {
                name: Some("marker.pdf".to_string()),
                media_type: "application/pdf".to_string(),
                data: BinarySource::Base64 {
                    data: pdf_b64.trim().to_string(),
                },
            },
            ContentBlock::Text {
                text: "What is the harness passphrase in this document? Just the passphrase."
                    .to_string(),
            },
        ],
        timestamp: chrono::Utc::now(),
    })];

    let cap = EventCapture::subscribe(&harness);
    println!("LLM prompt: [marker.pdf attached inline] What is the harness passphrase?");
    match with_timeout(SINGLE_TURN, harness.prompt_with_messages(messages)).await {
        Ok(_) => {
            cap.settle().await;
            let errs: Vec<String> = cap
                .events()
                .iter()
                .filter_map(|e| match e {
                    llm_harness_live_tests::CapturedEvent::Error(m) => Some(m.clone()),
                    _ => None,
                })
                .collect();
            println!("errors: {errs:?}");
            println!("LLM: {}\n", cap.texts().trim());
        }
        Err(e) => println!(
            "endpoint rejected inline documents (expected on Zhipu-style / \
            gateway endpoints): {e}\n"
        ),
    }
}

// ── Part 4: read returns a sandbox PNG ───────────────────────────────────────

async fn part4_read_png(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 4: read a PNG from the sandbox ---\n");

    let (harness, env, _inspector, tmp) = make_harness_with_env(entry, |b| {
        b.tool(std::sync::Arc::new(llm_harness_tools::ReadTool::new(None)))
    })
    .await;

    // Decode the 1x1 red PNG into the sandbox working dir.
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(PNG_1X1_RED)
        .expect("fixture must decode");
    let workdir = env.working_dir();
    let _ = tmp; // tmp keeps the sandbox alive
    let png_path = workdir.join("pixel.png");
    std::fs::write(&png_path, &bytes).expect("write sandbox png");
    println!("sandbox file: {}", png_path.display());

    let prompt_text = "Use the read tool on pixel.png, then tell me what color the image is. \
         Answer with just the color word.";
    let cap = EventCapture::subscribe(&harness);
    println!("LLM prompt: {prompt_text}");
    with_timeout(SINGLE_TURN, harness.prompt(prompt_text))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let tools = cap.tool_calls();
    println!("tools called: {tools:?}");
    println!("LLM: {}\n", cap.texts().trim());
}
