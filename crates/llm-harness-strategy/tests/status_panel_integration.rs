//! 集成测试：StatusPanelPlugin install 后端到端验证。
//!
//! spec §11: "集成测试：install 后跑一轮 loop，断言 <agent_status> 出现在末尾"
//! spec §11: "TODO 工具写入后下一轮状态栏内容更新"

use std::sync::Arc;

use llm_harness_agent::{HarnessHooks, Plugin};
use llm_harness_loop::test_utils::NoOpEnv;
use llm_harness_strategy::status_panel::{StatusPanelConfig, StatusPanelPlugin};
use llm_harness_types::{
    AgentContext, AgentMessage, AssistantMessage, AssistantMessageKind, ContentBlock, ExecutionEnv,
    RunContext, RunRequest, Tool, ToolContext, ToolProgress, TransformContextCtx, UserMessage,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn make_ctx<'a>(
    context: AgentContext,
    run: &'a RunContext,
    turn_index: u32,
) -> TransformContextCtx<'a> {
    TransformContextCtx {
        run,
        turn_index,
        abort: CancellationToken::new(),
        context,
    }
}

fn first_transform_hook(hooks: &HarnessHooks) -> Arc<dyn llm_harness_types::TransformContextHook> {
    hooks
        .transform_context
        .first()
        .expect("StatusBarHook should be registered")
        .clone()
}

fn make_tool_ctx(run: Arc<RunContext>) -> ToolContext {
    let (tx, _rx) = mpsc::channel::<ToolProgress>(1);
    ToolContext {
        run,
        env: Arc::new(NoOpEnv) as Arc<dyn ExecutionEnv>,
        abort: CancellationToken::new(),
        tool_use_id: "test".to_string(),
        turn_index: 0,
        assistant_message: Arc::new(AssistantMessage {
            kind: AssistantMessageKind::Progress,
            message_id: "m1".into(),
            turn_id: "t1".into(),
            content: vec![],
            stop_reason: None,
            timestamp: chrono::Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        }),
        update_tx: tx,
    }
}

fn extract_last_xml(messages: &[AgentMessage]) -> String {
    match messages.last().expect("should have a message") {
        AgentMessage::User(u) => match &u.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text block"),
        },
        _ => panic!("expected user message"),
    }
}

#[tokio::test]
async fn agent_status_appears_at_end_after_install() {
    let plugin = StatusPanelPlugin::with_defaults();
    let mut hooks = HarnessHooks::none();
    plugin.register_hooks(&mut hooks);
    let hook = first_transform_hook(&hooks);

    let run = RunContext::new(RunRequest::default());
    let ctx = make_ctx(
        AgentContext {
            system_prompt: None,
            messages: vec![],
        },
        &run,
        0,
    );

    let result = hook.transform(ctx).await.unwrap();
    let xml = extract_last_xml(&result.messages);
    assert!(
        xml.starts_with("<agent_status>"),
        "last message should be <agent_status>, got: {xml}"
    );
    assert!(xml.contains("<environment>"));
}

#[tokio::test]
async fn todo_write_updates_status_bar_next_turn() {
    let plugin = StatusPanelPlugin::with_defaults();

    // 注册工具
    let mut tools: Vec<Arc<dyn Tool>> = vec![];
    plugin.register_tools(&mut tools);
    let todo_tool = &tools[0];

    // 注册 hooks
    let mut hooks = HarnessHooks::none();
    plugin.register_hooks(&mut hooks);
    let hook = first_transform_hook(&hooks);

    let run = Arc::new(RunContext::new(RunRequest::default()));

    // 第一轮：空 TODO，状态栏应不含 <todo_list>
    let ctx1 = make_ctx(
        AgentContext {
            system_prompt: None,
            messages: vec![],
        },
        &run,
        0,
    );
    let result1 = hook.transform(ctx1).await.unwrap();
    let xml1 = extract_last_xml(&result1.messages);
    assert!(!xml1.contains("<todo_list>"));

    // 用 todo_write 工具写入 TODO
    let tool_ctx = make_tool_ctx(run.clone());
    let args = serde_json::json!({
        "action": "rewrite",
        "items": [
            { "id": "1", "content": "Write tests", "status": "in_progress" }
        ]
    });
    let _ = todo_tool
        .execute(args, &tool_ctx)
        .await
        .expect("todo_write should succeed");

    // 第二轮：状态栏应含 <todo_list> 且包含 "Write tests"
    let ctx2 = make_ctx(
        AgentContext {
            system_prompt: None,
            messages: vec![],
        },
        &run,
        1,
    );
    let result2 = hook.transform(ctx2).await.unwrap();
    let xml2 = extract_last_xml(&result2.messages);
    assert!(xml2.contains("<todo_list>"), "should contain todo_list");
    assert!(
        xml2.contains("Write tests"),
        "should contain the todo content"
    );
}
