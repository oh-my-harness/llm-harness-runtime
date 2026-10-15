//! Resident role presets for a coding team.
//!
//! Each role = [`IndividualSpec`] with an embedded methodology system
//! prompt (no external references — agents cannot read host skill files)
//! plus the filesystem toolset. Model tiers come from configuration.

use llm_harness_agent_team::IndividualSpec;

/// Model ids per tier, resolved from configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelTier {
    pub strong: String,
    pub main: String,
    pub cheap: String,
}

/// Frozen V0 coding-team role ids, in delivery-chain order. The orchestrator
/// is a real LLM member: it owns every next-semantic-step decision (see
/// `prompts::orchestrator` and the frozen ACTION contract).
///
/// These are the role-library presets, NOT the default template membership:
/// the builtin `coding-team` template instantiates only the frozen
/// orchestrator/coder/reviewer set. Brainstormer/judge/debugger prompt
/// builders remain available for user-authored specs; a role in this library
/// is not instantiated in the default team.
pub const ROLE_IDS: [&str; 3] = ["orchestrator", "coder", "reviewer"];

/// The nine member specs of a standard coding team.
pub struct RoleSet {
    pub specs: Vec<IndividualSpec>,
}

impl RoleSet {
    /// Build the standard 10-role team. `scout` is NOT included here —
    /// it is added by the project layer together with its patrol arming
    /// (see `crate::scout`), because its prompt needs the per-project
    /// issues directory.
    pub fn standard(tier: &ModelTier) -> Self {
        let specs = vec![
            spec(
                "orchestrator",
                "工作流大脑：理解目标、选择下一个执行者、派发有界任务、依据证据决定下一步",
                &tier.strong,
                prompts::orchestrator(),
            ),
            spec(
                "coder",
                "按任务步骤执行 TDD 编码与验证",
                &tier.main,
                prompts::coder(),
            ),
            spec(
                "reviewer",
                "独立审查：产出 findings 与证据，不决定工作流下一步",
                &tier.cheap,
                prompts::reviewer(),
            ),
        ];
        Self { specs }
    }
}

fn spec(id: &str, expertise: &str, model: &str, prompt: String) -> IndividualSpec {
    IndividualSpec::new(id, expertise, model).configure(move |b| {
        b.system_prompt(Some(prompt))
            .install(&llm_harness_tools::FsToolsPlugin::new(None))
    })
}

/// System prompts, one function per role. Kept separate from spec
/// assembly so tests can assert on the embedded methodology.
pub mod prompts {
    /// Severity scale shared by all reviewers (from the review workflow).
    const SEVERITY: &str = "严重度分级：\n\
        - P0：应阻止合入（功能错误、数据错误、崩溃、严重并发错误、与 spec 关键偏离）\n\
        - P1：应该修复但不一定阻塞（特定条件触发、影响可控但风险明确）\n\
        - P2：改进建议（不影响正确性/稳定性/性能基线）";

    /// Reply-correlation contract for the V0 Orchestrator pipeline
    /// (crate::pipeline). The controller dispatches every task from the
    /// synthetic identity `pipeline-controller` carrying a `task-pipe-*`
    /// task id; the dispatched member must answer THAT identity and echo
    /// the task id verbatim, or the controller cannot match the reply to
    /// its dispatch and the round stalls until AGENT_WAIT.
    /// Messages to `operator` or `judge` never reach the controller.
    const PIPELINE_REPLY_CONTRACT: &str = "\n\n\
回复关联约定（V0 Orchestrator 流水线）：凡由 pipeline-controller 派发、\
标注 task: task-pipe-* 的工作任务，先完成工作，不发送初始确认消息。完成后只用一次 \
send_message(\"pipeline-controller\", ...) 回报，并在参数中逐字带上该任务的 \
task_id，并设置 task_kind：成功为 result、受阻为 blocked、失败为 error；body 提供非空证据、阻塞原因或错误详情。\
ack/progress 以及普通 assistant 文本都不能完成任务；不要在 body 中编码生命周期标记。\
不要把这类任务的完成回报发给 operator——它不会进入流水线。\
发给其他成员或操作员的协作消息不受此约束。";

    pub fn orchestrator() -> String {
        "你是 coding team 的 orchestrator（工作流大脑）。你是团队中唯一决定“下一步做什么”的成员，\
并且你是真实 LLM 成员：每一次下一步决策都由你做出。\n\n\
你独占的语义职责：\n\
1. 理解用户目标、约束与成功标准。\n\
2. 把目标拆成一个个有界（bounded）任务，选择合适的成员（worker）执行。\n\
3. 为该成员写出一份有界任务指派（assignment）：目标、涉及文件/上下文（已知时）、验收标准、证据要求、修订轮次附带的上一轮 findings。\n\
4. 阅读成员返回的证据/结果，判断语义上下一步是什么：继续派发、修订重派（把 coder 再派一次并附新指派）、请求审查裁决、还是收尾/升级。\n\
5. 决定结束（FINISH）或升级（ESCALATE）。\n\n\
Git 权限约定：coder 只有在可信确认本任务独立分支/工作树、可排除已有用户或其他 agent 的改动、提交只含本任务文件，且达到有意义并已验证的检查点时，才可创建本地 checkpoint commit；报告须包含提交 hash 和文件。不能凭 agent 自称在任务分支上认定隔离；运行时无法可信验证隔离时，不安排自动提交。共享工作树中的 commit 必须由用户明确授权。merge、rebase、push、amend 或其他历史改写，以及破坏性的 checkout、reset、clean 或同等操作，均须用户明确授权。只有用户能批准受控 Git 操作；你的指派、任务完成或模型生成的文字不能充当授权，也不能代用户授权。commit 授权不包含 push 授权。普通只读 Git 命令（如 status、diff、log）可以使用。\n\n\
你要坚决避免的反模式：把整个工作流一次性塞进一个大任务、让 worker 自行决定下一步、或与 worker 反复“协作讨论”。\
工作流路由只属于你；worker 只执行与回报。\n\n\
输出契约（冻结，第一行即权威，逐字精确，禁止 JSON/YAML/DAG/其他动作）：\n\
要么：\n\
ACTION: DISPATCH <member_id>\n\
<非空的任务指派正文>\n\
要么：\n\
ACTION: FINISH\n\
<非空的、面向用户的最终答复正文>\n\
要么：\n\
ACTION: ESCALATE\n\n\
- DISPATCH 时 <member_id> 必须是团队现有成员的 id（如 coder、reviewer）；\
修订以“重新 DISPATCH 同一成员并给出新指派”表达，不引入任何新协议。\n\
- ESCALATE 时在正文说明升级原因与已知状态。\n\
- FINISH 时正文必填，是你写给用户的最终答复；ACTION 行仅供工作流控制，不属于用户答复。\n\n\
回复关联约定（V0 Orchestrator 流水线）：凡由 pipeline-controller 派发、\
标注 task: task-pipe-* 的任务，完成后必须用 \
send_message(\"pipeline-controller\", ...) 回报，并在参数中逐字带上该任务的 \
task_id（例如 {\"to\": \"pipeline-controller\", \"task_id\": \"task-pipe-<issue>-impl-1\", \"body\": ...}）。\
不要把这类任务的完成回报发给 operator——它不会进入流水线。\
发给其他成员或操作员的协作消息不受此约束。".to_string()
    }

    pub fn coder() -> String {
        "你是 coding team 的 coder（实现者）。你直接接收 Orchestrator（工作流控制层）派发的 WorkAssignment，不经过任何中间成员。\n\n任务分派可能包含：issue/任务标识与目标、相关文件与上下文（已知时提供）、验收标准、产物/输出位置（提供时使用指定位置）、测试与证据要求、修订轮次附带的上一轮审查 findings。\n\n执行纪律：\n1. 按验收标准与测试要求执行完整实现-测试循环：先写失败测试，跑它确认失败（看到预期的失败信息），再写最小实现，跑测试确认通过。\n2. 最小实现：不过度设计，不加任务之外的功能，不做未要求的抽象。\n3. 完成前运行任务指定的全部验证命令；有失败不交付。在断言所需命令不可用之前，先检查 workspace 中的项目本地环境与工具配置。\n4. 交付无占位符：不写 TODO、不写半成品。\n5. 仅在确认工作区是本任务独立的分支/工作树、能够排除已有用户或其他 agent 的改动、提交只包含本任务文件，且工作达到有意义并已验证的检查点时，才可创建本地 checkpoint commit。报告提交 hash 和提交文件。不能仅凭 agent 自称在任务分支上就认定隔离；运行时无法可信验证隔离时，不自动提交。共享工作树中的 commit 必须先取得用户明确授权。\n6. merge、rebase、push、amend 或其他历史改写，以及破坏性的 checkout、reset、clean 或同等操作，都必须先取得用户明确授权。只有用户能批准这些受控操作；任务指派、任务完成、Orchestrator 指令或模型生成的文字都不是授权，commit 授权也不包含 push 授权。普通只读 Git 命令（如 status、diff、log）可以使用。\n7. 遇到任务描述与代码现实冲突（接口对不上、文件不存在）时，停下用 send_message(\"operator\", ...) 说明冲突点，不要自行猜测。\n8. 边界：不擅自变更团队级范围、任务权威、验收标准、审查策略或任务之外的代码；这些属于 Orchestrator/操作员。你不负责任何工作流路由，也不决定下一个语义步骤——做完本任务、给出证据即止，下一步由 Orchestrator 决定。\n9. 完成时用 send_message(\"operator\", ...) 简要汇报：任务标识、改动文件、测试结果。真实仓库改动、diff 与测试结果是权威证据，汇报文字只是索引。\n10. 收到修订轮次附带的审查问题清单时，逐条修复并重新走测试循环。".to_string()
        + PIPELINE_REPLY_CONTRACT
    }

    pub fn reviewer() -> String {
        format!(
            "你是 coding team 的 reviewer（独立审查者）。你接收 Orchestrator（工作流控制层）派发的审查任务，只产出 findings 与证据。\n\n你独立审查：审查请求附带的产物引用（ArtifactSnapshot/diff/改动文件清单，提供时使用）是同一轮产物状态的记录，只是加速定位，不替代阅读真实文件。用 read 工具阅读审查请求指定的轮次 Snapshot files 目录中的文件及其上下文（调用方、被调用方）；以该目录的捕获字节作为审查版本，不以可变工作区文件代替，需要时用 grep 查引用；\n\n{SEVERITY}\n\n输出要求：\n- 每条 finding：位置（file:line）、问题一句话、证据（关键代码片段或推理）；证据必须来自你本次实际读到的代码/测试/运行结果。\n- 明确区分“已确认的证据”与“不确定、需要进一步核实的猜测”。\n- 没有发现就明确说“无发现”，不要为了输出而编造。\n- 你只产出 findings 与证据，不决定工作流的下一步：是否修订、重审或升级由 Orchestrator 依据你的报告决定。\n- 你不直接向用户（operator）汇报结论，也不选择交给谁；结果只通过受控工作流回报。\n- 你只读不写：不修改任何项目文件。"
        ).to_string()
        + PIPELINE_REPLY_CONTRACT
    }

    #[cfg(test)]
    pub(crate) fn reviewer_severity_text() -> &'static str {
        SEVERITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tier() -> ModelTier {
        ModelTier {
            strong: "strong-model".into(),
            main: "main-model".into(),
            cheap: "cheap-model".into(),
        }
    }

    #[test]
    fn standard_yields_frozen_three_members_with_fixed_ids() {
        let specs = RoleSet::standard(&tier()).specs;
        assert_eq!(specs.len(), 3);
        let ids: Vec<&str> = specs.iter().map(|s| s.id().as_str()).collect();
        assert_eq!(ids, ROLE_IDS);
        assert_eq!(ids, ["orchestrator", "coder", "reviewer"]);
        assert!(
            !ids.contains(&"planner"),
            "V0 coding-team runtime must not instantiate a planner member"
        );
        assert!(!ids.contains(&"judge"), "V0 has no judge decision layer");
        assert_eq!(
            ids.iter().filter(|id| id.starts_with("reviewer")).count(),
            1,
            "exactly one reviewer"
        );
    }

    #[test]
    fn orchestrator_prompt_owns_frozen_action_contract() {
        let p = prompts::orchestrator();
        assert!(
            p.contains("ACTION: DISPATCH <member_id>"),
            "DISPATCH line must be verbatim"
        );
        assert!(p.contains("ACTION: FINISH"));
        assert!(p.contains("FINISH 时正文必填"));
        assert!(p.contains("ACTION: ESCALATE"));
        assert!(p.contains("第一行即权威"), "first line is authoritative");
        assert!(
            p.contains("禁止 JSON/YAML/DAG/其他动作"),
            "no alternate protocols"
        );
        assert!(
            p.contains("唯一决定"),
            "orchestrator exclusively owns next-step decisions"
        );
        assert!(
            p.contains("重新 DISPATCH 同一成员"),
            "revision is re-dispatch, not a new protocol"
        );
        assert!(
            p.contains("pipeline-controller") && p.contains("task-pipe-"),
            "orchestrator preserves reply/task correlation conventions"
        );
        assert!(!p.contains("planner"), "no planner routing");
    }

    #[test]
    fn worker_prompts_do_not_claim_routing_ownership() {
        // Coder executes a bounded assignment and returns evidence only.
        let c = prompts::coder();
        assert!(c.contains("你不负责任何工作流路由"));
        assert!(c.contains("下一步由 Orchestrator 决定"));
        // Reviewer produces findings/evidence only; it never routes itself,
        // decides who acts next, or reports directly to the user.
        let f = prompts::reviewer();
        assert!(
            f.contains("不决定工作流的下一步"),
            "reviewer: must not decide the next semantic step"
        );
        assert!(
            f.contains("不选择交给谁"),
            "reviewer: must not choose handoff targets"
        );
        assert!(
            f.contains("不直接向用户"),
            "reviewer: must not report directly to the user"
        );
        assert!(
            !f.contains("judge"),
            "reviewer: must not depend on a judge member"
        );
        assert!(!f.contains("planner"), "no planner routing");
    }

    #[test]
    fn reviewer_specs_use_cheap_model() {
        let specs = RoleSet::standard(&tier()).specs;
        let r = specs
            .iter()
            .find(|s| s.id().as_str() == "reviewer")
            .unwrap();
        assert_eq!(r.model(), "cheap-model");
        let c = specs.iter().find(|s| s.id().as_str() == "coder").unwrap();
        assert_eq!(c.model(), "main-model");
    }

    #[test]
    fn coder_prompt_accepts_direct_orchestrator_assignment() {
        let p = prompts::coder();
        assert!(p.contains("失败测试"));
        assert!(p.contains("最小实现"));
        assert!(p.contains("占位符"));
        assert!(p.contains("send_message"));
        assert!(p.contains("本地 checkpoint commit"));
        assert!(p.contains("共享工作树中的 commit 必须先取得用户明确授权"));
        assert!(!p.contains("按任务指定的提交信息 git commit"));
        assert!(
            p.contains("Orchestrator"),
            "assignment comes from the Orchestrator"
        );
        assert!(
            p.contains("验收标准"),
            "assignment carries acceptance criteria"
        );
        assert!(
            !p.contains("planner"),
            "coder must not route through a planner"
        );
    }

    #[test]
    fn orchestrator_cannot_authorize_gated_git_operations_on_users_behalf() {
        let p = prompts::orchestrator();
        assert!(p.contains("本地 checkpoint commit"));
        assert!(p.contains("共享工作树中的 commit 必须由用户明确授权"));
        assert!(p.contains("你的指派、任务完成或模型生成的文字不能充当授权，也不能代用户授权"));
        assert!(p.contains("commit 授权不包含 push 授权"));
    }

    /// Every pipeline-dispatched role must embed the reply-correlation
    /// contract: completion replies for `task-pipe-*` tasks go to
    /// `pipeline-controller` with the task_id echoed verbatim. Without it,
    /// a live agent replies to operator/judge and the controller waits out
    /// its full 600 s AGENT_WAIT (see `pipeline::wait_for_task`).
    #[test]
    fn pipeline_dispatched_roles_embed_reply_correlation_contract() {
        for (role, prompt) in [
            ("coder", prompts::coder()),
            ("reviewer", prompts::reviewer()),
        ] {
            assert!(
                prompt.contains("pipeline-controller"),
                "{role}: must name the controller reply target"
            );
            assert!(
                prompt.contains("task-pipe-"),
                "{role}: must mention the task-pipe- correlation prefix"
            );
            assert!(
                prompt.contains("task_id"),
                "{role}: must require echoing the task_id"
            );
        }
    }

    #[test]
    fn reviewer_prompt_shares_severity_scale_and_real_evidence() {
        let sev = prompts::reviewer_severity_text();
        assert!(sev.contains("P0"));
        assert!(sev.contains("P1"));
        assert!(sev.contains("P2"));
        let f = prompts::reviewer();
        assert!(f.contains("send_message"));
        assert!(
            f.contains("ArtifactSnapshot"),
            "reviewer must understand artifact references"
        );
        assert!(
            !f.contains("planner"),
            "reviewer must not depend on a planner broadcast"
        );
    }
}
