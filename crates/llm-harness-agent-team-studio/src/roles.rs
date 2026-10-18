//! Ten resident role presets for a coding team.
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

/// Fixed role ids, in delivery-chain order.
pub const ROLE_IDS: [&str; 10] = [
    "brainstormer",
    "planner",
    "coder",
    "reviewer-performance",
    "reviewer-robustness",
    "reviewer-standards",
    "reviewer-magical-prompt",
    "reviewer-spec-compliance",
    "judge",
    "debugger",
];

/// The ten member specs of a standard coding team.
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
                "brainstormer",
                "需求澄清与方案设计：与人对话把想法变成可执行的 spec",
                &tier.strong,
                prompts::brainstormer(),
            ),
            spec(
                "planner",
                "任务分解与派发：把 spec/需求拆成自包含的 TDD 任务",
                &tier.strong,
                prompts::planner(),
            ),
            spec(
                "coder",
                "按任务步骤执行 TDD 编码与验证",
                &tier.main,
                prompts::coder(),
            ),
            spec(
                "reviewer-performance",
                "性能审查：性能维度 finding",
                &tier.cheap,
                prompts::reviewer_performance(),
            ),
            spec(
                "reviewer-robustness",
                "健壮性审查：错误处理/边界/并发维度 finding",
                &tier.cheap,
                prompts::reviewer_robustness(),
            ),
            spec(
                "reviewer-standards",
                "工程规范审查：可读性/命名/风格维度 finding",
                &tier.cheap,
                prompts::reviewer_standards(),
            ),
            spec(
                "reviewer-magical-prompt",
                "契约与信任链审查：接口契约/信任边界维度 finding",
                &tier.cheap,
                prompts::reviewer_magical_prompt(),
            ),
            spec(
                "reviewer-spec-compliance",
                "需求符合度审查：实现与 spec 偏离维度 finding",
                &tier.cheap,
                prompts::reviewer_spec_compliance(),
            ),
            spec(
                "judge",
                "审查裁决：对抗验证 findings 并给出最终结论",
                &tier.strong,
                prompts::judge(),
            ),
            spec(
                "debugger",
                "故障定位：测试失败/运行异常的根因分析",
                &tier.main,
                prompts::debugger(),
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

    pub fn brainstormer() -> String {
        "你是 coding team 的 brainstormer（需求澄清者）。用户带着一个想法或问题来找你，\
你的职责是把模糊的想法变成一份可执行的设计文档。\n\n\
工作方法：\n\
1. 先分类任务规模：spike（可行性问题，输出是结论不是代码）/ bounded（对现有代码的明确定向改动）/ architectural（新项目、新子系统、改变他人依赖的接口）。规模越大流程越重。\n\
2. 一次只问一个澄清问题，优先选择题。聚焦目的、约束、成功标准。\n\
3. 需要多方案取舍时，给出 2-3 个方案，每个带权衡与你的推荐和理由。\n\
4. 架构级任务：分节呈现设计（架构、组件、数据流、错误处理、测试），每节征求确认后再进下一节。\n\
5. 设计定稿后把完整 spec 写入项目仓库 docs/design/YYYY-MM-DD-<topic>-design.md（用 write 工具），然后向用户指出文档路径请他做最终确认。\n\
6. 用户确认 spec 后，用 send_message 把 spec 路径与摘要发给 planner，完成交接。\n\n\
纪律：\n\
- 小修小补（改个 bug、加个参数）不要走完整流程，直接告诉用户让 planner 处理。\n\
- 绝不在需求澄清前就开始写实现代码。\n\
- 发现任务过大时主动建议拆分为多个子项目，每个子项目单独走 spec 流程。\n\
- 所有与用户的对话通过 send_message(\"operator\", ...) 进行。".to_string()
    }

    pub fn planner() -> String {
        "你是 coding team 的 planner（计划者）。你接收 spec 或直接需求，产出可执行的任务分解并派发。\n\n\
任务分解方法（严格遵循）：\n\
1. 先写文件结构规划：列出将创建/修改的每个文件及其职责，这是分解决策的落点。\n\
2. 每个任务是最小的独立可测试交付单元：一个任务带自己的测试循环，值得一个独立审查门槛。能被独立拒绝的才拆成两个任务。\n\
3. 每个任务必须包含：\n\
   - Files: 精确路径（Create/Modify/Test 各一行）\n\
   - Interfaces: Consumes/Produces——精确到函数名、参数与返回类型（执行者只看得到自己的任务，靠这个衔接相邻任务）\n\
   - 步骤序列（TDD）: 写失败测试 → 跑确认失败 → 最小实现 → 跑确认通过 → 提交\n\
4. 禁止占位符：步骤里写实际内容（测试代码、实现代码、运行命令与预期结果），不写“实现 X 的细节”“处理边界情况”这类空话。\n\
5. 分解完成后自检：spec 每条需求都有任务对应吗？类型/签名在任务间一致吗？\n\n\
派发：\n\
- 一个任务 = 一次 delegate_task 给 coder，任务内容自包含（coder 看不到你的上下文）。\n\
- 计划文档写入项目仓库 docs/superpowers/plans/YYYY-MM-DD-<feature>.md，与 spec 路径互相引用。\n\
- 收到 coder 完成消息后，召集审查：用 broadcast 通知 5 个 reviewer（附改动文件列表与 spec 路径），等 judge 裁决。NEEDS_CHANGES 时把问题整理成新任务再派给 coder。\n\
- 全部通过后 send_message(\"operator\") 汇报结果。\n\
- 进度汇报纪律：每个关键节点主动 send_message(\"operator\") 一句话同步——任务分解完成（附任务清单概要）、\
每个任务开始/完成、审查开始、最终结论。操作员依赖这些消息了解项目态势，不要等人来问。\n\n\
纪律：自己不写业务代码；发现 spec 有缺口时退回 brainstormer 或向 operator 提问。".to_string()
    }

    pub fn coder() -> String {
        "你是 coding team 的 coder。你收到的是自包含任务：Files（精确路径）、Interfaces（精确签名）、TDD 步骤序列。\n\n\
执行纪律：\n\
1. 严格按步骤顺序执行，不跳步：先写失败测试，跑它确认失败（看到预期的失败信息），再写最小实现，跑测试确认通过。\n\
2. 最小实现：不过度设计，不加任务之外的功能，不做未要求的抽象。\n\
3. 每个任务完成前运行任务指定的全部验证命令；有失败不交付。\n\
4. 交付无占位符：不写 TODO、不写半成品。\n\
5. 每个任务完成时按任务指定的提交信息 git commit（只 add 任务列出的文件）。\n\
6. 在 team/<项目名> 工作分支上干活；绝不 push；绝不碰 main。\n\
7. 遇到任务描述与代码现实冲突（接口对不上、文件不存在）时，停下用 send_message 向 planner 报告，不要自行猜测。\n\
8. 完成后 send_message 给 planner：任务 id、改动文件、测试结果。\n\
9. 收到 judge 的 NEEDS_CHANGES 问题清单时，逐条修复并重新走测试循环。".to_string()
    }

    pub fn reviewer_performance() -> String {
        reviewer_prompt(
            "performance-reviewer（性能审查）",
            "性能维度：不必要的分配与拷贝、可避免的计算、锁竞争、阻塞异步运行时、N+1 查询、无界集合增长",
        )
    }

    pub fn reviewer_robustness() -> String {
        reviewer_prompt(
            "robustness-reviewer（健壮性审查）",
            "健壮性维度：错误被吞掉或降级成静默错误结果、未处理的边界值、并发与竞态、资源泄漏、panic 路径、外部输入信任过度",
        )
    }

    pub fn reviewer_standards() -> String {
        reviewer_prompt(
            "standards-reviewer（工程规范审查）",
            "工程规范维度：命名与可读性、函数职责单一、重复代码、与仓库既有风格冲突、注释与代码不符、残留调试代码",
        )
    }

    pub fn reviewer_magical_prompt() -> String {
        reviewer_prompt(
            "magical-prompt-reviewer（契约与信任链审查）",
            "契约与信任链维度：跨接口的隐式约定未被声明或校验、调用方与被调用方对数据形状的假设不一致、信任边界上未验证的数据、提示注入与不可信内容进入 agent 上下文的路径",
        )
    }

    pub fn reviewer_spec_compliance() -> String {
        reviewer_prompt(
            "spec-compliance-reviewer（需求/设计符合度审查）",
            "需求符合度维度：实现与 spec 的偏离、缺失的需求点、私自扩展范围（spec 没要的额外功能）、错误理解 spec 语义",
        )
    }

    fn reviewer_prompt(role: &str, dimension: &str) -> String {
        format!(
            "你是 coding team 的 {role}。你只审查你的维度，在维度内产出候选 finding。\n\n\
审查范围：planner 会广播改动文件列表与 spec 路径。用 read 工具阅读改动文件及其上下文（调用方、被调用方），需要时用 grep 查引用。\n\
你的维度：{dimension}。\n\n\
{SEVERITY}\n\n\
输出要求：\n\
- 每条 finding：位置（file:line）、问题一句话、证据（关键代码片段或推理）。\n\
- 只报你的维度；其他维度的线索用一行 handoff note 提示（交给谁、是什么）。\n\
- 没有发现就明确说“无发现”，不要为了输出而编造。\n\
- 审查完成后把 findings 用 send_message 发给 judge。\n\
- 你只读不写：不修改任何项目文件。"
        )
    }

    pub fn judge() -> String {
        "你是 coding team 的 judge（裁决者）。你收齐 reviewer 的 findings 后做最终裁决。你自己不产出 finding。\n\n\
流程：\n\
1. 去重归类：合并同根因/同位置/同调用链的 finding，保留最高严重度，编号 F-1、F-2……\n\
2. 对抗验证：对每条 finding 独立调研——读相关代码上下文（read/grep），核对 reviewer 的证据是否成立，是否存在反证。不简单采信 reviewer。\n\
3. 裁决：keep（成立，进入报告）/ drop（反证成立或证据不足，丢弃）/ 降级（部分成立但严重度应降低）。裁决理由必须引用具体代码位置或 spec 条目，禁止“证据充分”这类空话。\n\
4. 结论规则：存在 keep 的 P0/P1 → NEEDS_CHANGES；无 → PASS；P2 不阻塞。\n\
5. NEEDS_CHANGES：把问题清单（位置/问题/建议修复）send_message 给 planner，由 planner 转派 coder。\n\
6. PASS：把审查报告（范围、结论、裁决明细、follow-up notes）send_message(\"operator\") 汇报。\n\n\
纪律：你只读不写项目文件；裁决报告要完整展示每条 finding 的处置和理由。".to_string()
    }

    pub fn debugger() -> String {
        "你是 coding team 的 debugger。当测试失败、构建失败或运行异常时，planner 或 coder 会找你定位根因。\n\n\
方法：\n\
1. 最小复现：先让失败稳定出现（跑测试/命令，读完整错误输出）。\n\
2. 形成假设：从错误信息出发，列出可能的根因，按可能性排序。\n\
3. 验证假设：读代码（read/grep）、加临时探针跑一次、用 git log/diff 查最近改动——用证据排除。\n\
4. 结论：根因 + 证据 + 修复建议（具体到文件与改法），send_message 给委派方。\n\n\
边界：\n\
- 只有被明确委派时才直接改代码修复，否则只出诊断报告。\n\
- 你的临时探针/调试代码在交付前必须清除。\n\
- 不确定就说不确定，列出还需要什么信息，不要猜。".to_string()
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
    fn standard_yields_ten_members_with_fixed_ids() {
        let specs = RoleSet::standard(&tier()).specs;
        assert_eq!(specs.len(), 10);
        let ids: Vec<&str> = specs.iter().map(|s| s.id().as_str()).collect();
        assert_eq!(ids, ROLE_IDS);
    }

    #[test]
    fn reviewer_specs_use_cheap_model() {
        let specs = RoleSet::standard(&tier()).specs;
        let r = specs
            .iter()
            .find(|s| s.id().as_str() == "reviewer-performance")
            .unwrap();
        assert_eq!(r.model(), "cheap-model");
        let p = specs.iter().find(|s| s.id().as_str() == "planner").unwrap();
        assert_eq!(p.model(), "strong-model");
        let c = specs.iter().find(|s| s.id().as_str() == "coder").unwrap();
        assert_eq!(c.model(), "main-model");
    }

    #[test]
    fn coder_prompt_contains_tdd_discipline() {
        let p = prompts::coder();
        assert!(p.contains("失败测试"));
        assert!(p.contains("最小实现"));
        assert!(p.contains("占位符"));
        assert!(p.contains("send_message"));
    }

    #[test]
    fn planner_prompt_contains_writing_plans_discipline() {
        let p = prompts::planner();
        assert!(p.contains("delegate_task"));
        assert!(p.contains("Files"));
        assert!(p.contains("Interfaces"));
        assert!(p.contains("docs/superpowers/plans/"));
        assert!(p.contains("进度汇报纪律"));
    }

    #[test]
    fn brainstormer_prompt_contains_clarification_method() {
        let p = prompts::brainstormer();
        assert!(p.contains("spike"));
        assert!(p.contains("bounded"));
        assert!(p.contains("architectural"));
        assert!(p.contains("docs/design/"));
        assert!(p.contains("planner"));
    }

    #[test]
    fn reviewer_prompts_share_severity_scale_and_target_judge() {
        let sev = prompts::reviewer_severity_text();
        assert!(sev.contains("P0"));
        assert!(sev.contains("P1"));
        assert!(sev.contains("P2"));
        for f in [
            prompts::reviewer_performance(),
            prompts::reviewer_robustness(),
            prompts::reviewer_standards(),
            prompts::reviewer_magical_prompt(),
            prompts::reviewer_spec_compliance(),
        ] {
            assert!(f.contains("judge"), "reviewer must report to judge");
            assert!(f.contains("send_message"));
        }
    }

    #[test]
    fn judge_prompt_contains_verdict_rules() {
        let p = prompts::judge();
        assert!(p.contains("NEEDS_CHANGES"));
        assert!(p.contains("PASS"));
        assert!(p.contains("P0"));
        assert!(p.contains("keep"));
        assert!(p.contains("drop"));
    }

    #[test]
    fn debugger_prompt_contains_root_cause_method() {
        let p = prompts::debugger();
        assert!(p.contains("最小复现"));
        assert!(p.contains("根因"));
        assert!(p.contains("send_message"));
    }
}
