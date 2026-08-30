//! Scout: the periodic repo-patrol role.
//!
//! The scout spec is assembled here (not in `roles`) because its system
//! prompt needs the per-project issues directory. Arming = submitting the
//! first patrol task through the operator channel; renewal is the scout's
//! own contract — its prompt requires it to call `set_reminder` at the end
//! of every patrol, so the loop self-sustains via the timeout subsystem.

use llm_harness_agent_team::AgentTeam;

use crate::issue::IssueStore;
use crate::roles::ModelTier;

/// Hard cap on pending issues per project: past this the scout stops
/// filing and asks the operator to triage instead.
pub const PENDING_CAP: usize = 50;

/// The scout's member id.
pub const SCOUT_ID: &str = "scout";

/// Assemble the scout `IndividualSpec` (filesystem tools + patrol prompt).
pub fn spec(tier: &ModelTier) -> llm_harness_agent_team::IndividualSpec {
    llm_harness_agent_team::IndividualSpec::new(
        SCOUT_ID,
        "定时巡检仓库：跑测试、扫 TODO、检查可疑改动，产出候选 issue",
        &tier.cheap,
    )
    .configure(|b| {
        b.system_prompt(Some(prompts::scout()))
            .install(&llm_harness_tools::FsToolsPlugin::new(None))
    })
}

/// Build the patrol task text (the contract between the arming side and
/// the scout prompt; tests assert on its parts).
pub fn task_text(issues_dir: &str, interval_secs: u64, pending_cap: usize) -> String {
    format!(
        "执行仓库巡检（定时任务，本消息由系统发出）。\n\
         1. 在项目仓库运行测试、扫描 TODO/FIXME、检查最近 commit 的可疑改动与 warning 积累。\n\
         2. 发现问题用 write 工具写候选 issue 到 {issues_dir}：文件名 I-NNN-<slug>.md，\n\
            frontmatter 五行固定为 id/status: pending/source: scout/created/title。\n\
         3. 写之前先读该目录现有 issue 查重（grep 关键词）；pending 数量达到 {pending_cap} 时本次不写，\n\
            改为 send_message 提醒 operator 清理。\n\
         4. 完成后必须调用 set_reminder 预约 {interval_secs} 秒后的下一次巡检——\n\
            这是巡检循环自续约的唯一机制，漏掉则巡检停止。\n\
         你只提报，不改代码；issue 状态只有人能改。"
    )
}

/// Submit the first patrol task via the operator channel. Call once after
/// `team.build`. The scout renews itself from there on.
pub fn arm(team: &AgentTeam, issues: &IssueStore, interval_secs: u64) {
    let text = task_text(&issues.root().to_string_lossy(), interval_secs, PENDING_CAP);
    if let Err(e) = team.submit(SCOUT_ID, &text) {
        // Arming failure must not fail project registration; log loudly.
        tracing::error!("scout arming failed: {e}");
    }
}

/// One-shot smoke: used by tests to assert the prompt carries the contract.
pub mod prompts {
    pub(super) fn scout() -> String {
        "你是 coding team 的 scout（巡检员）。你按固定间隔巡检项目仓库，产出候选 issue，不改代码。\n\n\
巡检内容：\n\
- 运行项目测试，记录失败；\n\
- 扫描 TODO/FIXME/HACK 注释，识别该转 issue 的长期欠账；\n\
- 检查最近 commit 的可疑改动（缺少测试、大段删除的错误处理等）；\n\
- 积累的 warning（编译/构建输出）。\n\n\
产出规则：\n\
- 每个问题写一个 issue 文件（frontmatter 固定五行，status 一律 pending）；\n\
- 写之前先查重：grep 现有 issue 的关键词，重复的不写；\n\
- issue 描述给足证据：位置、现象、为什么值得修、建议方向；\n\
- pending 积压过多时停止提报，提醒 operator 清理。\n\n\
纪律：\n\
- 你不修改项目代码，只写 issue 文件；\n\
- issue 状态只有人能改，你绝不手动改 status；\n\
- 每次巡检结束必须 set_reminder 预约下一次（间隔以任务消息为准）——漏掉即停摆。"
            .to_string()
    }

    #[cfg(test)]
    pub(crate) fn scout_text_for_test() -> String {
        scout()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::ModelTier;

    #[test]
    fn task_text_contains_contract() {
        let text = task_text("/abs/issues", 3600, PENDING_CAP);
        assert!(text.contains("/abs/issues"));
        assert!(text.contains("3600"));
        assert!(text.contains(&PENDING_CAP.to_string()));
        assert!(text.contains("set_reminder"));
        assert!(text.contains("status: pending"));
    }

    #[test]
    fn scout_prompt_carries_discipline() {
        let p = prompts::scout_text_for_test();
        assert!(p.contains("set_reminder"));
        assert!(p.contains("查重"));
        assert!(p.contains("不改代码"));
        assert!(p.contains("pending"));
    }

    #[tokio::test]
    async fn arm_pushes_inbox_message_via_operator_channel() {
        use llm_harness_loop::test_utils::MockLlmClient;
        use std::sync::Arc;

        let tier = ModelTier {
            strong: "m".into(),
            main: "m".into(),
            cheap: "m".into(),
        };
        let providers: Vec<(String, Arc<dyn llm_harness_loop::LlmClient>)> = vec![(
            "m".into(),
            Arc::new(MockLlmClient::new(vec![])) as Arc<dyn llm_harness_loop::LlmClient>,
        )];
        let repo = std::env::temp_dir().join(format!("ct-scout-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let mut team = AgentTeam::new().member(spec(&tier));
        let env: Arc<dyn llm_harness_types::ExecutionEnv> =
            Arc::new(llm_harness_sandbox::OsEnv::new(&repo));
        team.build(env, providers).await.unwrap();

        let issues_root =
            std::env::temp_dir().join(format!("ct-scout-issues-{}", std::process::id()));
        let issues = IssueStore::new(&issues_root);

        arm(&team, &issues, 3600);

        let ctx = team.social_context().unwrap();
        let drained = ctx.drain_inbox(&llm_harness_agent_team::AgentId::new(SCOUT_ID));
        assert!(
            drained.iter().any(|m| m.body.contains("巡检")),
            "scout inbox must carry the patrol task"
        );
        team.shutdown();
    }
}
