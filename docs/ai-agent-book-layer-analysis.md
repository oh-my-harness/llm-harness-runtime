# 《AI Agents in Depth》分层梳理

> ## 工作模式（供后续 agent 继续）
>
> ### 目标
> 逐章分析 `../ai-agent-book/book/chapter{1-10}.md` 教程内容，将每个知识点归入三层：
> - **runtime 机制层**：harness 运行时必须提供的基础设施（trait 定义、执行引擎、循环机制、状态持久化、通信通道等）
> - **runtime 策略层**：runtime 提供机制、由框架/业务层注入策略的回调与可配置点（hooks、rules、judges、approval、budget 等）
> - **用户策略层**：面向最终用户/任务的可替换策略内容（prompt、skill、工具实现、知识库内容、评估标准等）
>
> ### 分工
> - **用户**：逐章提出观点（哪个知识点归属哪层），逐条与 AI 讨论
> - **AI**：对照教程原文 + runtime 源码校验观点是否正确，讨论至达成一致后记录
>
> ### 工作流程（每个知识点）
> 1. 用户提出观点（教程内容 → 归属层 + 理由）
> 2. AI 读教程原文对应段落，读 runtime 源码对应实现
> 3. AI 逐条校验：机制层是否已具备？策略层是否缺失？用户策略层是否有内容？
> 4. 如有分歧，讨论至达成一致
> 5. 记录到文档：用户观点表 + 校验记录详情
>
> ### 关键源码位置
> - 教程：`../ai-agent-book/book/chapter{N}.md`
> - llm-api-adapter（git rev `10fb9a8`）：`~/.cargo/git/checkouts/llm-api-adapter-ae83aa579a24123b/10fb9a8/src/`
> - runtime 类型：`crates/llm-harness-types/src/`（tool.rs, hooks.rs, messages.rs, content.rs, misc.rs, skills.rs）
> - runtime 引擎：`crates/llm-harness-runtime/src/`（builder.rs, rules/, spawn/, platform/）
> - runtime agent：`crates/llm-harness-agent/src/`（compaction.rs, skills.rs, harness/）
> - loop 引擎：`crates/llm-harness-loop/src/`（loop_fn.rs, type_bridge.rs）
>
> ### 核心发现（跨章一致结论）
> - **机制层大部分已具备**（hooks、traits、loop 引擎、workflow 引擎、compaction 基础设施）
> - **runtime 策略层系统性缺失**（默认策略、模板、约束 hook 行为的通用原则）
> - **例外**：压缩策略（观点 9）有 3 个机制层实质缺失（自定义压缩 prompt 通路、熔断器、标记消息为不可压缩）
> - **设计原则**：runtime 策略层能做的尽量不放在机制层（用户明确提出，观点 6 起贯彻）
> - **架构决策**：策略层代码放在独立 crate `llm-harness-strategy`（同仓库，不同 crate），只依赖机制层 public API。编译器强制边界——策略代码只能用 `Tool`/`BeforeToolCallHook`/`HarnessBuilder` 等公开类型。与现有 `llm-harness-runtime-tools`/`llm-harness-runtime-mcp` 模式一致（独立可选 crate）。trait 定义留机制层（types），默认实现放策略层。边界原则：如果实现需要改 loop_fn/session/dispatch 内部逻辑 → 机制层；如果实现只需注册 hook 或实现 trait → 策略层。
>
> ### 进度
> - [x] 第一章：6 个观点 + 护栏交叉校验
> - [x] 第二章：9 个观点（消息角色、KV Cache、缓存约束、提示工程、状态栏+TODO、提示注入防御、Skill 元数据注入、状态栏完整分析、压缩策略）
> - [ ] 第三章：用户记忆和知识库（待读）
> - [x] 第四章：14 个观点（10-23）
> - [x] 第五章：5 个观点（24-28）
> - [ ] 第六~十章：待读
>
> ### 文档结构
> - 每章有"本章核心内容索引"+"用户观点表"+"校验记录详情"
> - 用户观点表：`| # | 教程内容 | 归属层 | 用户理由 | 校验 |`
> - 校验记录：每个观点的教程依据、runtime 现状、三层归属、关键观察
>
> ### 注意事项
> - 观点编号跨章连续（第一章 1-6，第二章 7-9，...）
> - 修正早期观点时，在后续观点的"修正观点 N"中说明，不回改原文
> - 教程引用标注行号（如 "ch2 line 786-960"）
> - runtime 源码引用标注文件路径和关键符号
>
> ### 实施计划
>
> 基于架构决策（策略层独立 crate `llm-harness-strategy`），分三个阶段执行：
>
> #### 阶段一：提取已有策略代码到策略 crate
>
> > ✅ 已完成：3 项迁移（BudgetControlAdapter、ResourceInjector、format_skills_for_system_prompt）
>
> 将混杂在机制层 crate 中的策略层代码迁移到 `llm-harness-strategy`，只依赖机制层 public API。
>
> **判断原则**：不机械地执行机制/策略分离。runtime 自身必然集成的策略留机制层，用户可选注入的策略才迁移到 strategy crate。
>
> | 待迁移代码 | 当前位置 | 迁移目标 | 说明 |
> |-----------|---------|---------|------|
> | `BudgetControlAdapter` + `BudgetExceededHook` | `runtime/src/control/budget.rs` | strategy | ShouldStopHook 实现，仅在用户调用 `builder.budget()` 时注入 |
> | `ResourceInjector` | `runtime/src/platform/resource.rs` | strategy | 往 system_prompt 追加资源，用户可选调用 |
> | `format_skills_for_system_prompt` | `agent/src/skills.rs` | strategy | Skill 元数据格式化模板，无生产调用 |
>
> 以下代码原表列为迁移/保留，现修正为**留 runtime**（runtime 自身必然集成的策略或紧耦合机制）：
>
> | 代码 | 位置 | 留 runtime 理由 |
> |-----|------|----------------|
> | `RuleBasedApprovalHook` + `RuleChain`/`Rule`/`Predicate`/内置 predicates | `runtime/src/rules/` | 规则匹配框架是 runtime 基础设施，workflow gate 等机制层直接依赖；决定不迁移 |
> | `CostAccumulatorHook` | `runtime/src/control/cost_hook.rs` | builder.rs:618 注释"即使没有 pricing/budget 也注入"，是 runtime 必然集成的默认策略 |
> | `SUMMARIZATION_SYSTEM_PROMPT`/`SUMMARY_REQUEST` | `agent/src/compaction.rs` | `compact()` 硬编码使用，是 runtime 压缩流程的默认 prompt；阶段二第 1 项补可配置入口后再由 strategy crate 提供默认模板 |
> | `UsageLedger`/`CostAggregate` | `runtime/src/control/cost.rs` | 共享数据结构，机制层 |
> | `WorkflowGateHook` | `runtime/src/workflow/gate.rs` | 与 workflow 引擎紧耦合，机制层 |
>
> 迁移后 `HarnessBuilder` 的 `budget()` 方法改为从 strategy crate 注入 hook（或保留 builder 方法但委托到 strategy crate 的工厂函数）。`pricing()` 不动——`CostAccumulatorHook` 留 runtime。
>
> #### 阶段二：补齐机制层缺失
>
> 按 28 个 view 中标记为"机制层缺失"的项目，补齐机制层基础设施。这些需要改 loop_fn/session/dispatch 内部逻辑，不能仅靠 hook 实现。
>
> | # | 缺失项 | 涉及 view | 改动位置 | 说明 |
> |---|--------|---------|---------|------|
> | 1 | ~~自定义压缩 prompt 通路~~ ✅ 已完成 | view 9 | `compaction.rs` | CompactionPromptSpec + placeholder validation |
> | 2 | ~~压缩熔断器（连续失败计数+停止）~~ ✅ 已完成 | view 9, view 26 | `compaction_ops.rs` | 连续失败 N 次 block + reset_circuit_breaker() |
> | 3 | ~~标记消息为不可压缩~~ ✅ 已完成 | view 9 | `messages.rs`/`compaction.rs` | CompactionPolicy::Keep + `<no-compact>` tag |
> | 4 | ~~ContextMode 结构化来源标注~~ ✅ 已完成 | view 19 | `spawn_types.rs` | Inject(String) → Inject(Vec&lt;ContextBlock&gt;) 带 source 标签 |
> | 5 | ~~SpawnOptions 配置继承~~ ✅ 已完成 | view 3, view 12 | `spawn_types.rs`/`spawner.rs` | 继承 system_prompt/tools/thinking_level + per-spawn overrides |
> | 6 | ~~QuerySubagentTool 返回 role/description~~ ✅ 已完成 | view 12 | `spawn/tools.rs` | AgentStatus 增加 role/description 字段 |
> | 7 | ~~退避抖动（jitter）~~ ✅ 已完成 | view 26 | `config.rs` RetryConfig | delay_for 增加 full jitter（`random(0, base*2^attempt]`），retry_after 优先不抖动 |
> | 8 | ~~流式空闲看门狗~~ ✅ 已完成 | view 26 | `loop_fn.rs` | stream 事件间超时检测；idle watchdog hoisted Sleep + reset()，discarded MessageEnd 类型层强制 |
> | 9 | ~~轨迹完整性自动修复~~ ✅ 已完成 | view 26 | `loop_fn.rs`/`dispatch.rs` | 未注册 ToolUse→synthetic error；执行缺结果→backfill；保证无 orphan ToolUse 到达 provider |
> | 10 | ~~MCP 服务器版本锁定~~ ✅ 已完成 | view 17 | `runtime-mcp/version_lock.rs` | `McpLockFile` 锁文件存储 server_info.version + 工具定义 sha256 快照（description_hash + input_schema_hash 分开）；首次连接自动写入，后续比对：版本不匹配或工具定义变更时按 `VersionPolicy` 决策（fail_closed / allow_with_warning / allow_and_update）；`tools/list_changed` 通知不再静默刷新，走 verify_and_lock diff + 策略路径；四种变更类型检测：added / removed / modified_desc / modified_schema |
> | 11 | ~~MCP 最小权限凭证管理~~ ✅ 已完成 | view 17 | `runtime-mcp/credential.rs` + `strategy/credential_guard.rs` | `CredentialProvider` trait + `EnvCredentialProvider` 默认实现，支持 `${ENV:VAR}` / `${ENV:VAR:-default}` / `vault://` 引用语法；`ScopedCredential { value, allowed_tools }` 声明凭证作用域；`credentials.scoped_env` 与 env/headers 同 key 互斥校验；未解析引用 fail-closed 拒绝连接。strategy crate `McpCredentialGuardHook`（BeforeToolCallHook）默认告警模式，可选 Deny 模式 |
> | 12 | ~~截断后完整输出保存到临时文件~~ ✅ 已完成 | view 18 | `runtime-tools/truncation.rs` + `bash.rs` | truncate_and_persist 辅助模块 + BashTool 集成；双阈值（200行/10000字节）触发，头尾各50行，完整输出持久化到 OS 临时文件 |
> | ~~13~~ | ~~Sidecar 专用机制（与流式输出并行的轻量 LLM 门控）~~ | view 11 | ~~`loop_fn.rs`/dispatch~~ | **不做**——见下方"暂不做的项目" |
> | ~~14~~ | ~~幂等性机制（idempotency key + 预检-确认两段式）~~ | view 11 | ~~`types/tool.rs`~~ | **不做**——见下方"暂不做的项目" |
> | ~~15~~ | ~~前台/后台调用区分~~ | view 26 | ~~`compaction_ops.rs`/`loop_fn.rs`~~ | **已满足**——改进点 2（压缩熔断器）完成后已覆盖，详见 view 26 校验记录 |
> | ~~16~~ | ~~中间输出过滤 hook（待评估）~~ | view 5 | ~~`types/hooks.rs`~~ | **不需要**——教程说的输出侧护栏检查点是"响应返回用户之前"，`FinalAnswerValidator` 已覆盖；中间 Progress 文本不直接返回用户，进入上下文后可由 `TransformContextHook` 清洗。见下方"暂不做的项目" |
> #### 阶段三：补齐 runtime 策略层缺失
>
> 按 28 个 view 中标记为"runtime 策略层缺失"的项目，在 `llm-harness-strategy` crate 中实现默认策略。这些只需注册 hook 或实现 trait，不需改机制层内部逻辑。
>
> | # | 缺失项 | 涉及 view | 实现方式 | 优先级 |
> |---|--------|---------|---------|--------|
> | 1 | ~~默认危险命令黑名单 BeforeToolCallHook~~ ✅ 已完成 | view 11 | RuleBasedApprovalHook + 默认 RuleChain | 高 | `SafetyDefaultsPlugin`（strategy crate）集成 `RuleBasedApprovalHook` + 内置危险命令黑名单 RuleChain（`rm -rf /`、`dd if=/dev/zero` 等）。一行 install 接入 |
> | 2 | ~~默认路径遍历防护规则~~ ✅ 已完成 | view 11 | BeforeToolCallHook + canonicalize/boundary check | 高 | `SafetyDefaultsPlugin` 同时集成路径遍历防护规则，对文件工具参数做 canonicalize + boundary check |
> | 3 | ~~默认审计日志格式~~ ✅ 已完成 | view 11 | AfterToolCallHook + 结构化日志模板 | 中 | `ToolCallAuditHook`（strategy crate）实现 BeforeToolCallHook + AfterToolCallHook 双 hook，自动记录工具调用的审计条目（时间、参数、结果、耗时、执行者、上下文）到 `AuditSink`。`AuditPlugin` 包装为 Plugin 一行 install 接入。审计类型从 runtime 迁移到 types crate，新增 `duration_ms` 字段 |
> | 4 | ~~状态栏 XML 格式模板 + 四种内容收集器~~ ✅ 已完成 | view 5, 8 | TransformContextHook 默认实现 | 高 | `StatusPanelPlugin`（strategy crate）实现 `TransformContextHook`，注入 `<agent_status>` XML 到上下文末尾。四种内容收集器：TODO 列表、侧信道时间戳、工具调用计数器、环境状态。支持 Replace/Append 两种更新策略 |
> | 5 | ~~TODO write 工具 + 状态栏更新策略~~ ✅ 已完成 | view 5 | Tool trait 实现 + TransformContextHook | 中 | `todo_write` 工具（strategy crate）实现 Tool trait，提供 `rewrite_todo_list` 和 `update_todo_status` 功能。`StatusPanelPlugin` 集成 TODO store，工具写入后下一轮状态栏自动更新 |
> | 6 | ~~Skill 元数据注入（方式三）BeforeRunHook~~ ✅ 已完成 | view 7 | BeforeRunHook 默认实现 | 高 | `SkillMetadataInjectionHook`（strategy crate）实现 `BeforeRunHook`，在 run 开始时将 Skills 元数据（name + description）追加到上下文末尾（方式三），不注入 system prompt（方式一），保持 KV Cache 前缀稳定 |
> | 7 | ~~自动包裹来源标签策略~~ ✅ 已完成 | view 6 | AfterToolCallHook 默认实现 | 中 | `SourceTagHook`（strategy crate）实现 `AfterToolCallHook`，在配置的外部内容工具返回 `DataBlock::Text` 时自动包裹 `<external_content source="...">` 标签。同时包裹 `model_content` 和 `session_projection`（Projected/Ephemeral），确保来源标记持久化。已有标签先 strip 再重新包裹（防伪造 source 属性）。Label 校验 `[A-Za-z0-9 _-]`。Image/Document 块透传 |
> | 8 | ~~注入短语黑名单模板~~ ✅ 已完成 | view 6 | AfterToolCallHook + TransformContextHook 默认实现 | 低 | `InjectionFilterHook`（strategy crate）同时实现 `AfterToolCallHook`（在 session_message 拆分前过滤，防持久化绕过）和 `TransformContextHook`（过滤已累积上下文，纵深防御）。扫描工具结果文本块中的注入短语（默认覆盖中英文"忽略之前的指令"等），替换为 `[FILTERED]`。支持自定义 `InjectionPattern`（Replace/Remove 动作）。仅过滤工具结果，用户消息不过滤。过滤 model_content/Projected content/Ephemeral summary。文档标注"辅助手段"；CompactionSummary/BranchSummary/Custom 过滤 out of scope |
> | 9 | ~~状态栏内容来源约束~~ ✅ 已完成 | view 6 | TransformContextHook 校验逻辑 | 中 | item 4（StatusPanelPlugin）已实现，状态栏内容由框架代码维护（TODO store、工具计数器、环境状态），不来自外部可污染数据源。`StatusPanelPlugin` 的内容收集器均从 `RunContext`/`ToolContext` 等可信来源读取数据 |
> | 10 | ~~notify_user 内置工具 + NotificationChannel trait~~ ✅ 已完成 | view 13 | Tool trait 实现 + NotificationChannel trait（均放 strategy crate，不进机制层 types/） | 高 | `NotifyUserTool`（ToolLevel::User，非 BuiltIn）让 LLM 向用户发消息但不结束 run（terminate: false）。`NotificationChannel` trait 抽象通知渠道，用户实现具体渠道（IM/邮件/Slack）并通过 RunExtensions 注入。`NotifyPlugin` 一行 install 注册工具。未配置渠道时返回 Ok + delivered:false（不终止 run，不阻塞）；发送超时 10s，tokio::select! 响应 ctx.abort |
> | ~~12~~ | ~~命令语义解析~~ | view 24 | ~~BeforeToolCallHook + Shell 命令语义分析~~ | ~~中~~ | **不做**——见下方"暂不做的项目" |
> | 13 | ~~持久记忆跨会话防线~~ ✅ 已完成 | view 24 | BeforeToolCallHook/AfterToolCallHook 写入审查 | 中 | `MemoryDefensePlugin`（strategy crate）实现 `BeforeToolCallHook`，扫描写入持久记忆的内容中的注入短语，防止恶意指令潜伏在长期记忆中跨会话生效。覆盖两条写入路径：memory_write 工具（始终检查 args.content）；write/edit 工具（当 path basename 匹配 memory 文件集 MEMORY.md/SOUL.md/AGENTS.md/CLAUDE.md/.cursorrules 时检查）。可配置 action: Deny（默认）/Sanitize/Warn。复用 InjectionFilterHook 提取的 InjectionDetector |
> | 14 | ~~项目指令文件自动注入 BeforeRunHook~~ ✅ 已完成 | view 25 | BeforeRunHook 默认实现（扫描 CLAUDE.md 等） | 高 | `ProjectInstructionHook`（strategy crate）实现 `BeforeRunHook`，扫描 CLAUDE.md/AGENTS.md/.cursorrules/SOUL.md，从 working_dir 向上遍历，按子→父顺序合并后注入 system_prompt 尾部（KV Cache 友好的稳定前缀）。`ProjectInstructionConfig` 可配置 file_names + max_depth + max_bytes。XML 安全：body 经 escape_xml_text、attrs 经 escape_xml_attr 转义。大小守卫：max_bytes（默认 64 KiB）超限 skip + warn。所有 FS 失败降级为 tracing::warn + skip |
> | ~~15~~ | ~~重复调用指纹检测~~ ✅ 已完成 | view 26 | ShouldStopHook + BeforeToolCallHook + tool+params 指纹 | 中 | `RepetitionGuardHook`（strategy crate）实现 `BeforeToolCallHook`（指纹 Deny）+ `ShouldStopHook`。`compute_fingerprint` 用 `DefaultHasher` + canonical JSON（递归排序 key）。`LoopSafetyPlugin` 一行 install。per-fingerprint 累计计数（非"连续"，防止模型交替切换） |
> | ~~16~~ | ~~连续失败计数器~~ ✅ 已完成 | view 26 | ShouldStopHook + AfterToolCallHook + per-tool 计数器 | 中 | `FailureCircuitBreakerHook`（strategy crate）实现 `AfterToolCallHook`（Err→increment, Ok→reset，阻塞 lock 避免并行 batch 丢计数）+ `BeforeToolCallHook`（circuit open→Deny）+ `ShouldStopHook`。per-tool 独立计数，默认阈值 3 |
> | ~~17~~ | ~~输出触顶降级接续~~ ✅ 已完成 | view 26 | ShouldStopHook + TransformContextHook 续写指令 | 中 | `TruncationContinuationHook`（strategy crate）实现 `ShouldStopHook`（MaxTokens→continue，只读判断）+ `TransformContextHook`（追加续写指令时自增 continuation_count）。不改机制层——`StreamOptions` 无 max_tokens 字段，用续写指令替代。默认续写 2 次 |
> | ~~18~~ | ~~死亡螺旋防护 + 递归深度计数器~~ ✅ 已完成 | view 26 | PrepareNextTurnHook + ShouldStopHook 连续错误轮计数 | 中 | `DeathSpiralGuardHook`（strategy crate）实现 `PrepareNextTurnHook`（turn 边界聚合：本 turn 有任意 ToolFailure→increment，全 Ok→reset）+ `ShouldStopHook`（超阈值停止）。用 per-turn 聚合替代 per-tool-call（避免并行 batch 非确定性）。`AfterRunHook` 不存在于 runtime，用 per-turn 近似。默认阈值 5 |
> | ~~19~~ | ~~全局 max_turns 上限~~ ✅ 已完成 | view 26 | ShouldStopHook + PrepareNextTurnHook 轮数计数 | 高 | `MaxTurnsHook`（strategy crate）实现 `ShouldStopHook`（非工具轮 clean stop）+ `PrepareNextTurnHook`（工具轮 Err→break loop）。`turn_index + 1 >= max_turns` 触发。默认 50 轮 |
> | ~~20~~ | ~~批量 steer 消息编号+汇总~~ | view 22 | ~~TransformContextHook 默认实现~~ | ~~低~~ | **不做**——见下方"暂不做的项目" |
> | ~~21~~ | ~~discover_tools 元工具~~ | view 17, 23 | ~~Tool trait 实现~~ | ~~中~~ | **不做**——归用户策略层。runtime 机制层已提供完整动态工具能力（`set_tools()`/`set_active_tools()`/`NextTurnDirective.tools/active_tools`/`PrepareNextTurnHook`），业务层用这些机制自行实现工具发现策略。匹配算法、返回格式、交互流程都是业务决策，差异太大，strategy crate 提供默认实现无通用价值。与观点 1 原则一致："分类不落在 trait 上，机制层用不了"——工具发现策略同理 |
> | ~~22~~ | ~~工具层次化组织~~ | view 17, 23 | ~~工具分类+分组策略~~ | ~~中~~ | **不做**——归用户策略层。主要价值是帮 discover_tools 缩小搜索空间，discover_tools 不做了，独立价值只剩"在 system prompt 里列分类索引帮 LLM 定位工具"——这同样是上层业务怎么组织 prompt 的事。`McpManager` 已有 server_name→tool 物理层次（`ExposedTool.server_name` + `full_name`），语义分类（搜索/读取/解析/查询）是业务决策 |
> | ~~23~~ | ~~工具描述审查机制~~ | view 17 | ~~MCP adapter wrapper 审计 description~~ | ~~低~~ | **不做**——见下方"暂不做的项目" |
> | ~~24~~ | ~~事件过滤规则 + 事件载荷上下文模板~~ ✅ 已完成 | view 14 | EventStream wrapper + 默认模板 | 低 | `FilterStream`（predicate 过滤 wrapper，静默丢弃不匹配事件）+ `EventDetails`/`EventContext`（source/channel/event_id/timestamp/context 四维度 JSON schema）+ predicate 工厂 `by_source`/`by_channel`/`by_field`/`pass_all`。均在 `strategy/src/event_stream/` |
> | ~~25~~ | ~~Cron/Heartbeat 内置 EventStream 实现~~ ✅ 已完成 | view 14 | TimerStream/HeartbeatStream | 低 | `TimerStream`（`TimerKind::Once` 触发后返回 None + `TimerKind::Interval` 永不返回 None）+ `HeartbeatStream`（拆 `HeartbeatHandle`+`HeartbeatStream` 共享 `Arc<AtomicU64>`，超时未 `tick()` 触发）+ descriptor 序列化用于 `TaskStore` 崩溃恢复 |
> | 26 | 压缩 prompt 默认模板（上下文感知） | view 9 | 自定义压缩 prompt 的策略层默认实现 | 中 | ⚠️ 部分完成：`SUMMARIZATION_SYSTEM_PROMPT`/`SUMMARY_REQUEST` 硬编码在 `agent/src/compaction.rs`（机制层），`CompactionPromptSpec` 支持自定义替换。缺 strategy crate 的上下文感知默认模板 |
> | 27 | ~~保留优先级默认规则~~ ✅ 已完成 | view 9 | `session/types.rs` + `agent/src/compaction.rs` | `CompactionPolicy::Keep` + `<no-compact>` tag（阶段二第 3 项）；Keep 条目 verbatim 保留到 summary，LLM 不合规时 also preserved |
> | 28 | 工具结果投影默认策略 | view 9, 10 | ToolSessionProjection 默认大小阈值 | 低 | ⚠️ 部分完成：`ToolSessionProjection` enum (Full/Projected/Ephemeral) + `ToolProjectionKind` 已有（`types/tool.rs`），各工具自行选择投影策略。缺统一默认大小阈值 |
> | 29 | ~~BashTool 截断策略~~ ✅ 已完成 | view 18 | `runtime-tools/truncation.rs` + `bash.rs` | truncate_and_persist 双阈值（200行/10000字节），按行截断头尾各50行 |
> | 30 | ~~截断标注引导 Agent read 临时文件~~ ✅ 已完成 | view 18 | `runtime-tools/truncation.rs` | 中文标注含文件路径+read工具引导 |
> | 31 | grep + glob 工具补齐 | view 15 | llm-harness-runtime-tools 新增 | 高 |
> | 32 | 内置工具 description 改写（补充"什么时候用"+边界+示例+移除实现细节） | view 16 | 工具实现改进 | 高 |
> | 33 | 内置工具参数描述补充具体例子 | view 16 | 工具实现改进 | 中 |
> | 34 | 内置工具附带 1-5 个调用示例 | view 16 | 工具实现改进 | 中 |
> | ~~35~~ | ~~edit 工具 trailing newline 行为修正~~ ✅ 已完成 | view 16 | `runtime-tools/edit.rs` | 文档明确 POSIX trailing newline 行为：always rewritten as joined lines + single `\n`；callers 需 byte-exact 用 `write` |
> | 36 | 模型降级（主模型过载自动切备用模型） | view 26 | BeforeProviderRequestHook + fallback model | 中 |
> | 37 | 连续失败超阈值→人工升级 | view 26 | ShouldStopHook + 人工升级通知 | 中 |
> | ~~38~~ | ~~ContextMode::Inject 注入格式加来源标签~~ ✅ 已完成 | view 19 | `runtime/src/spawn/spawn_types.rs` + `spawner.rs` | `ContextBlock` + `ContextSource` enum (ParentConversation/File/UserInstruction/Custom) + `render_context_blocks` 输出 `[Context: <label>]` 格式。标签为 `Parent Conversation`/`File`/`User Instruction`/`Custom`，非教程建议的 `[FROM_MAIN_AGENT]`/`[FROM_USER]`/`[TOOL_RESULT]`，但功能等价 |
> | 39 | 后台任务监控 EventStream 默认实现（ShellMonitorStream） | view 14 | 基于 BashTool 后台进程 + stdout 行流 | 低 |
> | 40 | 外部通道 webhook 接收器默认实现 | view 14 | EventStream 默认实现 | 低 |
> | ~~41~~ | ~~事件结构化模板（来源/渠道/内容/上下文四维度默认 JSON schema）~~ ✅ 已完成 | view 14 | Event details 默认模板 | 低 | `EventDetails` + `EventContext`（`strategy/src/event_stream/template.rs`）：source/channel/event_id/timestamp/context 五字段，`with_label`/`with_reason`/`with_task_id` builder，`to_value`/`from_value` 序列化 |
> | ~~42~~ | ~~事件过滤机制（基于 Event.details 的 predicate 过滤）~~ ✅ 已完成 | view 14 | EventStream wrapper 过滤接口 | 低 | `FilterStream`（`strategy/src/event_stream/filter.rs`）：包裹内层 `Box<dyn EventStream>`，predicate 不匹配静默丢弃，内层 None 传播。predicate 工厂 `by_source`/`by_channel`/`by_field`/`pass_all` |
>
> 暂不做的项目（明确标记）：
> - 流式工具执行（view 27）— 实现复杂度高
> - 持久化终端会话（view 28）— 需要 PTY
> - 写入后自动 lint（view 28）— 暂不做
> - 动态 schema 追加 + KV Cache 优化（view 23）— 太复杂
> - mid-turn steer（view 21）— 明确不做
> - 优先级队列（view 20）— 太复杂
> - 多渠道通知（view 13/20）— 太复杂
> - 幂等性机制（view 11 改进点 14）— 场景不成立：(1) 工具执行是本地同步函数调用且每个 tool_use_id 仅执行一次，无机械重试循环，idempotency key 去重无消费者；(2) 先查询后变更是工具设计模式，Tool trait 的 execute 已给工具完全控制权；(3) 预检-确认两段式已有等价构建块（ToolProgress channel + EventStream + spawn_agent），拆成两工具或异步工具即可表达，无需新 Tool trait 变体；(4) 崩溃恢复策略（view 26 轨迹完整性修复）注入 synthetic error 而非重执行，不触发重复执行
> - 中间输出过滤 hook（view 5 改进点 16）— 不需要：教程输出侧护栏检查点是"响应返回用户之前"（ch1 line 425），只有 FinalAnswer 返回用户，FinalAnswerValidator 已覆盖；中间 Progress 文本不直接返回用户，进入上下文后可由 TransformContextHook 在下一轮清洗；新增 hook 的语义复杂度（拒绝 Progress 后做什么？）和性能开销不匹配收益
> - Sidecar 并行门控机制（view 11 改进点 13）— 优先级不高：收益仅限于 `BeforeToolCallHook` 内部做 LLM 审查的场景（数百 ms 延迟与剩余流式输出重叠），而当前无内置 LLM 审查 hook（`RuleBasedApprovalHook` 纯规则、`CostAccumulatorHook` 记账，均微秒级无并行收益）；LLM 审批工具本身不是高优先级。若后续实现 LLM 审查 hook 时再回来设计。`BeforeToolCallHook` 的输入隔离（只看 tool_name + args）已天然满足，届时只需补并行调度
> - discover_tools 元工具 + 工具层次化组织（view 17/23 改进点 21/22）— 归用户策略层：runtime 机制层已提供完整动态工具能力（`set_tools()`/`set_active_tools()`/`NextTurnDirective.tools/active_tools`/`PrepareNextTurnHook`），业务层自行实现工具发现策略。匹配算法、返回格式、交互流程都是业务决策，strategy crate 提供默认实现无通用价值。工具层次化组织的主要价值是帮 discover_tools 缩小搜索空间，discover_tools 不做了，独立价值只剩"在 system prompt 里列分类索引"——同样是上层业务怎么组织 prompt 的事
> - 命令语义解析（view 24 改进点 12）— 不做：(1) 完整语义解析等价于实现 shell 解释器，变量展开（`$VAR`/`$(...)`）需要运行时环境状态（env/cwd/shell 版本），而 `BeforeToolCallHook` 的输入隔离原则只给 hook 看 tool_name + args——hook 层拿不到 shell 环境状态，无法做真正的语义展开；(2) 策略 crate 提供默认语义解析器无通用价值，与 #21/#22（discover_tools、工具层次化）否决理由一致——"危险"判定是业务决策（`rm -rf node_modules` 在 build agent 合法，在只读审计 agent 危险），默认实现要么误杀要么形同虚设；(3) 现有两层检测（`DefaultCommandBlacklist`：raw pre-scan + per-sub-command tokenizer）已覆盖绝大多数真实场景，`find -exec rm` 可通过给 `find` 加 `DangerRule` 覆盖，不需要语义解析；`$(echo rm)` 这类动态构造攻击的真正防线是沙盒隔离（已实现 `BwrapSandbox`/网络出口/文件系统隔离），不是命令审查；(4) 扩展点已就位——`CommandSafetyPredicate` trait 供用户替换为完整语义解析器，通过 `SafetyHook::new(Arc(custom_pred), PathSafetyConfig::new())` 直接注册 hook 接入
> - 批量 steer 消息编号+汇总（view 22 改进点 20）— 不做：(1) 机制不干净支撑——steer 消息 push 后就是普通 `AgentMessage::User`，无元数据区分 steer 与常规用户消息，`TransformContextHook` 要识别"哪些是本轮 drain 的 steer 消息"只能靠启发式（连续 User 消息？）脆弱，可靠实现需给消息加 steer 标记（机制层改动，违背阶段三"只注册 hook、不改机制层"边界）；(2) 场景低频且优先级低——steer 是用户中途插话通常一次一条，教程自己承认"实践中只在真正紧急时才打断"；(3) 解决方案更适合用户策略层——教程方案 (1) "提示词告知全面考虑"是纯 prompt 工程，方案 (2) 状态栏事件计数可在现有 `StatusPanelPlugin`（#4 已完成）的内容收集器框架里加一个 collector，不需 strategy crate 提供默认实现；(4) 默认实现会强加格式约定——`[未处理事件 N/M]` 前缀改变 steer 消息语义，某些场景反而干扰；汇总需理解内容，简单字符串拼接不算汇总，LLM 辅助汇总超出 `TransformContextHook` 职责（它不该调 LLM）
> - 工具描述审查机制（view 17 改进点 23）— 不做：(1) 有效的描述审查是 LLM/人工任务不是正则任务——教程说的"审计"指把 description 当不可信输入**审视**，需语义理解（"传 SSH 私钥作为参数"是合法接口说明还是投毒取决于上下文），正则匹配要么漏检（自然语言变体 "prior to invoking this tool, kindly provide..."）要么误杀（合法接口文档 "this tool takes an SSH key parameter"），与 #12（命令语义解析）同理，需语义理解的任务策略 crate 提供默认实现无通用价值；(2) 版本锁定（阶段二 #10 已完成）+ 首次人工审查已覆盖供应链攻击路径——教程完整缓解方案是"接入前审查 + 锁定版本 + 拒绝静默更新"，后两项已完成，第一项是人工任务（调用方在首次连接时审查 description，`compute_tool_hashes` 对 description 做 sha256 + `diff_tool_snapshots` 检测 `modified_desc` 保证后续稳定），strategy crate 无法替代人工审查的语义判断；(3) LLM 审查 hook 是 Sidecar 机制（阶段二 #13 暂不做）的职责——若后续实现 Sidecar，description 审查会是其典型用例（独立轻量 LLM 审查每次工具调用的结构化输入，天然覆盖 description 投毒），在 Sidecar 不存在时做正则 hook 是劣化版本；(4) 实际防线已就位——版本锁定（description hash 检测变更）+ 沙盒隔离（`BwrapSandbox`/网络出口/文件系统隔离限制实际破坏范围）+ 最小权限凭证（阶段二 #11 `ScopedCredential` 限制凭证作用域，即使 LLM 被诱导传凭证也只能传给 `allowed_tools` 内工具）+ `McpManager::get_tools()` 暴露完整 description 供调用方审查

---
## 第一章：AI Agent 入门

### 本章核心内容索引
1. Agent = LLM + 上下文 + 工具（核心公式；观察空间/动作空间）
2. 工具：Agent 的手脚（五类工具、工具调用四步流程、通用 vs 专用）
3. LLM：Agent 的大脑（内部思考、零样本/少样本、模型即 Agent、学习机制三层次）
4. 上下文：Agent 的眼睛（五部分构成、消融实验）
5. ReAct 循环（轨迹、静态前缀 + 动态轨迹）
6. Harness 工程（Agent = Model + Harness = LLM + [上下文 + 工具 + 约束 + 验证 + 纠正]）
7. 工程范式演进（提示工程 → 上下文工程 → Harness 工程 → Loop 工程 → Graph 工程）
8. Harness 五功能核心原则
9. 构建 Agent 的三个核心原则（保持简单、保持透明、ACI）
10. 模型选型
11. 编排模式（工作流 vs 自主 Agent）
12. 护栏与安全性（输入侧/执行侧/输出侧、人工干预）
13. 本书作为 Harness 工程实践指南（各章节在 Harness 框架下的定位）

### 用户观点

<!-- 用户在此填写逐条归类观点，AI 在下方校验栏标注对错与依据 -->

| # | 教程内容 | 归属层 | 用户理由 | 校验 |
|---|----------|--------|----------|------|
| 1 | 工具五类分类（感知/执行/协作/事件触发/用户沟通） | runtime 策略层 | 分类不落在 trait 上（机制层用不了），但应作为通用策略层的知识，供默认审批规则等策略使用 | ✅ 讨论后达成一致。教程 ch1 line 61-73 列出五类但说"完整分类体系和设计原则将在第四章展开"；归入策略层是用户的架构判断，与教程精神一致但不来自教程原文 |
| 2 | 工具风险评级/特权等级（低/中/高） | runtime 策略层+用户策略层（拆分） | 策略层提供按分类自动标注的通用规则；具体规则和阈值由业务决定 | ✅ 讨论后达成一致。机制层不缺失：BeforeToolCallHook（async）+ Allow/Modify/Deny 三档已足够支撑人工审批实现（hook 内部 await oneshot channel 等待人工结果，loop 引擎无感知）。不需要增加"需人工确认"档位——它是过程不是终态。拆分为两层：通用策略（按分类/参数自动标注风险等级的默认规则模板）→ runtime 策略层，当前缺失；具体规则和阈值 → 用户策略层 |
| 3 | 具体工具设计（通用 vs 专用、沙盒隔离、参数设计等） | 用户策略层 | 面向具体任务的设计决策 | ✅ 正确。教程 ch1 line 99-103 讨论的设计权衡是任务级决策，runtime 只提供 Tool trait 接口 |
| 4 | 约束（Constrain） | 三层拆分 | 机制层提供拦截点；策略层提供默认策略原则；用户策略层提供具体规则 | ✅ 讨论后达成一致。机制层已具备（BeforeToolCallHook + WorkflowGateHook）；runtime 策略层当前缺失（故障安全默认值、权限分类模板）；用户策略层为具体约束规则 |
| 5 | 验证（Verify） | 三层拆分 | 机制层提供拦截点；策略层提供通用验证原则；用户策略层提供具体验证逻辑 | ✅ 讨论后达成一致。机制层已具备（AfterToolCallHook + FinalAnswerValidator + StepTransitionJudge）；runtime 策略层当前缺失（输入隔离原则、通用验证规则模板）；用户策略层为具体验证内容。~~潜在缺口~~：~~FinalAnswerValidator 只覆盖最终答案提交点，非最终回复的中间输出无拦截 hook~~ **已确认不需要**——教程输出侧护栏检查点是"响应返回用户之前"，FinalAnswerValidator 已覆盖；中间 Progress 文本不直接返回用户，进入上下文后可由 TransformContextHook 清洗 |
| 6 | 纠正（Correct） | 三层拆分 | 机制层提供 retry/abort/should_stop；策略层提供通用纠正策略；用户策略层提供具体阈值和业务回滚 | ✅ 讨论后达成一致。机制层已具备（ToolFailure.terminate + run_step_with_policy + ShouldStopHook + CancellationToken）；runtime 策略层当前缺失（不暴露中间态原则、熔断器通用实现、回滚策略模板）；用户策略层为具体重试次数/超时阈值/工具级回滚 |

### 校验记录

#### 观点 1：工具五类分类 → runtime 策略层（讨论后修正）

- **原始观点**：工具五类分类 → runtime 机制层
- **修正过程**：
  - 用户认为不需要落在 trait 上——机制层（loop 引擎、hook 调度）不会根据分类做分支，分类是给策略用的（如"感知类默认放行，执行类需审批"）。
  - AI 确认：`Tool` trait 加 `category()` 返回枚举，机制层确实无法使用。
  - 用户进一步指出：当前 runtime 有策略框架（Rule+Predicate+RuleChain）和零散策略实现（CostAccumulatorHook、BudgetControlAdapter、fallback=Deny），但缺少系统性的通用策略层；有些通用策略应放在 runtime 里。
  - AI 确认：runtime 已有的策略实现是零散的，无系统性策略层抽象。此判断准确。
- **最终结论**：工具五类分类 → **runtime 策略层**。作为通用策略层的知识基础，供默认审批规则等策略使用，不落在 Tool trait 上。
- **教程依据**：ch1 line 61-73 列出五类但说"完整分类体系和设计原则将在第四章展开"；归入策略层是用户的架构判断，与教程精神一致但不来自教程原文。

#### 观点 2：工具风险评级/特权等级 → 两层拆分（讨论后二次修正）

- **原始观点**：工具风险评级/特权等级 → runtime 机制层
- **第一次修正**：拆分为三层——机制支撑（Decision 增加"需人工确认"档位）→ 机制层；通用策略 → 策略层；具体规则 → 用户策略层。
- **第二次修正（本次）**：讨论"是否需要增加人工确认档位"后，确认机制层不缺失。
- **讨论过程**：
  - `BeforeToolCallHook::on_call` 是 async（返回 `BoxFuture`）。业务层在 hook 内部 await oneshot channel 等待人工审批结果，拿到后返回 Allow 或 Deny——loop 引擎无感知。
  - "人工确认"不是终态，是到达终态前的中间过程，最终结果仍是 Allow 或 Deny。增加第四变体会混叠"过程"和"终态"两个语义层。
  - 增加变体还会引入连带问题：挂起期间能否被 abort？挂起状态要不要持久化？超时谁管？这些是不必要的机制层复杂度。
  - 机制和策略分离原则：runtime 的职责是提供拦截点（BeforeToolCallHook），不是定义拦截后的所有可能行为。人工审批是策略层的实现选择。
- **最终结论**：拆分为两层——
  1. **机制层**：`BeforeToolCallHook`（async）+ `BeforeToolCallDecision{Allow, Modify, Deny}` → **已具备**，不需要增加档位。
  2. **通用策略** → runtime 策略层（当前缺失）：按工具分类/参数自动标注风险等级的默认规则模板，业务层可直接用或覆盖。人工审批的通用策略模板（超时、通知渠道、fallback 行为）也可放在此处，但属可选便利。
  3. **具体规则和阈值** → 用户策略层：什么操作算高风险、多少钱以上需人工确认——业务决策。
- **教程依据**：ch1 line 425 "根据操作是否可逆、权限等级、财务影响，为每个工具标注风险等级（低/中/高），高风险操作需额外审查或人工确认"。
- **runtime 现状**：`BeforeToolCallDecision` 有 `Allow`/`Modify(args)`/`Deny(ToolFailure)` 三档；`BeforeToolCallHook::on_call` 是 async；`Rule`/`RuleBasedApprovalHook` 提供规则匹配框架；`Tool` trait 无 `risk_level()`（不需要，风险等级是策略层概念）。

#### 观点 3：具体工具设计 → 用户策略层

- **校验结果：正确。**
- 教程 ch1 line 99-103 讨论通用 vs 专用工具的权衡（沙盒隔离、参数清晰、权限受限、可审计），是面向具体任务的设计决策。
- runtime 只提供 `Tool` trait 这个接口机制，具体工具实现由业务层提供。

#### 观点 4：约束（Constrain）→ 三层拆分

- **教程依据**：ch1 line 250-274，约束是"围绕上下文和工具构建的安全边界"；ch1 line 308 "故障安全默认值：所有能力默认关闭，必须显式开放"。
- **机制层（已具备）**：
  - `BeforeToolCallHook` + `BeforeToolCallDecision{Allow, Modify, Deny}`（types/hooks.rs）
  - `WorkflowGateHook`（workflow/gate.rs，按 step 收窄工具集，LLM 物理上提不出越界 tool call）
  - `Rule` / `RuleBasedApprovalHook`（rules/，规则匹配框架，fallback=Deny 即 fail-secure）
- **runtime 策略层（当前缺失）**：
  - "故障安全默认值"通用策略原则的默认实现
  - 权限分类模板：哪些类工具默认放行、哪些默认拒绝、哪些需人工确认
- **用户策略层**：
  - 具体约束规则（哪个 workflow step 收窄到哪些工具、特定工具的 Allow/Deny 规则）

#### 观点 5：验证（Verify）→ 三层拆分

- **教程依据**：ch1 line 250-274，验证是"围绕工具执行结果构建的检查机制"；ch1 line 309 "输入隔离：安全检查只看结构化数据（如工具返回的 JSON 字段），而不是模型自由生成的文本"。
- **机制层（已具备）**：
  - `AfterToolCallHook` + `AfterToolCallDecision`（types/hooks.rs，工具执行后拦截）
  - `FinalAnswerValidator`（types/hooks.rs，最终答案提交前校验）
  - `StepTransitionJudge`（workflow/judge.rs，workflow step 间条件判断）
- **runtime 策略层（当前缺失）**：
  - "输入隔离"原则的通用实现（安全检查只看结构化数据，不看模型自由文本）
  - 通用验证规则模板（如 JSON schema 合规检查、工具返回值类型校验）
- **用户策略层**：
  - 具体验证什么、怎么判断对错（退款金额不超过订单金额、代码是否编译通过）

#### 观点 6：纠正（Correct）→ 三层拆分

- **教程依据**：ch1 line 250-274，纠正是"围绕工具调用失败构建的恢复机制"；ch1 line 310 "在确认无法恢复之前，不暴露中间态（例如工具调用失败时先静默重试，不将半成品结果展示给用户）"；ch1 line 283 "熔断器（Circuit Breaker）：当错误连续发生时自动'断电'停止重试"；ch1 line 284 "错误恢复机制：捕获异常、回滚到上一稳定状态、重试或交还给人类"。
- **机制层（已具备）**：
  - `ToolFailure.terminate`（types/tool.rs，false=建议重试，true=硬拒绝）
  - `WorkflowEngine::run_step_with_policy`（workflow/engine.rs，retry + backoff + timeout）
  - `ShouldStopHook`（types/hooks.rs，控制 loop 是否继续）
  - `CancellationToken`（abort 传播，engine step_abort → sub-agent handle → agent loop）
- **runtime 策略层（当前缺失）**：
  - "不暴露中间态"原则的通用实现（失败时先静默重试，不展示半成品）
  - 熔断器通用策略实现（连续失败 N 次后自动停止，可用 `ShouldStopHook` 实现，缺的是通用策略）
  - 回滚策略模板（什么条件触发回滚、回滚到哪个 checkpoint）
- **用户策略层**：
  - 具体重试次数、超时阈值
  - 工具级回滚（删除文件后恢复）— 业务逻辑，runtime 无法通用解决
  - 特定 workflow 的回滚目标

#### 关键观察

约束、验证、纠正三者的**机制层都已具备**——runtime 通过 hook 体系（BeforeToolCall / AfterToolCall / ShouldStop / FinalAnswerValidator）+ workflow 引擎（retry / gate / judge）提供了完整的拦截和恢复机制点。**当前缺失的主要是 runtime 策略层**——通用的默认策略原则和模板实现。这与观点 1、2 的结论一致：runtime 有机制、有零散策略实现、有策略框架，但缺少系统性的通用策略层。

#### 护栏段落交叉校验（ch1 line 409-447）

教程护栏段落对观点 2、4、5、6 做了交叉校验，结论一致，补充如下：

**观点 2 确认**：教程明确"执行侧护栏在工具调用时验证，其核心是工具风险评级"——风险评级是策略层内容，机制层由 `BeforeToolCallHook` 提供。无修正。

**观点 4 确认**：教程将输入侧护栏（相关性分类器、安全分类器、内容审核、基于规则的保护）列为约束的实现。runtime 对应机制：`TransformContextHook`（LLM 调用前转换上下文，可过滤输入）+ `BeforeProviderRequestHook`（provider 请求前拦截）。机制层已具备，具体分类器实现是用户策略层。无修正。

**观点 5 确认**：教程将输出侧护栏（PII 过滤器、输出验证）列为验证的实现。runtime 对应机制：`FinalAnswerValidator`（最终答案提交前校验）+ `AfterToolCallHook`（工具执行结果校验）。~~潜在缺口~~ **已确认不需要**：教程说的输出侧护栏检查点是"在响应返回用户之前检查"（ch1 line 425），本 runtime 只有 `FinalAnswer` 返回用户，`FinalAnswerValidator` 已覆盖此检查点。中间 Progress 文本不直接返回用户，进入上下文后可通过已有的 `TransformContextHook` 在下一轮 LLM 调用前清洗。新增拦截 hook 引入的语义复杂度（拒绝后做什么？丢弃？重试？修改？）和性能开销不匹配收益 |

**观点 6 确认**：教程人工干预部分明确两个触发条件——"超过失败阈值"对应 `ShouldStopHook` + retry 机制；"高风险操作"对应 `BeforeToolCallHook` 中人工审批（观点 2 已讨论）。无修正。

**新发现（未归类为观点，标记待后续章节）**：
1. **误拒绝**（ch1 line 417）：教程说"护栏评估不能只测试'应当拒绝的请求是否被拦截'，还要测试'明确允许的请求是否能够正常完成'"。这是评估原则 → runtime 策略层（通用评估策略），待第六章（评估）展开。
2. **Constitutional Classifiers**（ch1 line 431）：规则驱动 + 上下文联合判断 + 两级筛查。这是具体分类器实现方案 → 用户策略层（甚至超出 runtime 范围，涉及模型层防御）。

---

## 第二章：上下文工程

### 本章核心内容索引

（待补充——本章内容较多，随用户观点逐步填充）

### 用户观点

| # | 教程内容 | 归属层 | 用户理由 | 校验 |
|---|----------|--------|----------|------|
| 1 | 四种消息角色（system/user/assistant/tool）+ tools 字段 | runtime 机制层 | 大模型 API 的核心结构，llm-api-adapter 已实现 | ✅ 正确。adapter 的 `Message` 枚举有 System/User/Assistant/Tool 四变体；`ChatRequest.tools: Vec<ToolDef>` 独立于 messages。runtime (harness) 在其上构建 `AgentMessage` 联合体，经 `ConvertToLlmHook` 转换为 adapter Message |
| 2 | KV Cache 友好的上下文设计 | 机制层+策略层（拆分） | 首先保证机制层有足够支撑，然后需要增加 runtime 策略层提供最佳默认实现 | ✅ 讨论后达成一致。机制层：system prompt 静态前缀✅、消息历史 append-only✅、tools 独立于 messages✅、adapter 支持 CacheHint/AnthropicCacheControl✅。runtime 不引入缓存概念（层次 B：只管前缀稳定性，缓存优化交给 adapter/provider）。策略层：缺少约束 hook 不往 system prompt 注入动态内容的通用原则 |
| 3 | 缓存作为架构约束（三约束）+ 平台无关 | 机制层+策略层（拆分） | runtime 必须抽象成平台无关 | ✅ 讨论后达成一致。三约束校验：约束 1（缓存边界）⚠️违背——system prompt 是单块字符串无边界分割；约束 2（子 agent 对齐）❌违背——子 agent 不继承父 agent 配置；约束 3（替换字符串冻结）✅基本满足——ToolSessionProjection 持久化。采用层次 B：runtime 只管前缀稳定性约束，不引入缓存概念，缓存优化交给 adapter 层 |
| 4 | 提示工程：优化系统提示词（5 个小节） | 用户策略层 | 语气风格、结构化提示、流程驱动 vs 规则堆砌、业务规则细化、Few-shot 示例——都是 prompt 内容编写 | ✅ 讨论后达成一致。前 5 个小节全部归用户策略层；第 6 个小节（工具定义的设计）留到第四章讨论 |
| 5 | Agent 状态栏 + TODO write | 机制层+runtime 策略层+用户策略层（拆分） | TODO write 要做成 runtime 策略层 | ✅ 讨论后达成一致。三层拆分：状态栏注入通道（上下文末尾追加框架生成的 user 消息）→ 机制层，当前缺失；TODO write 工具+状态栏 XML 格式模板 → runtime 策略层，当前缺失；具体 TODO 内容 → 用户策略层 |
| 6 | 提示注入防御 | runtime 策略层为主 | runtime 策略层能做的尽量不放在机制层 | ✅ 讨论后达成一致。机制层都不缺（除观点 5 的状态栏注入通道）：结构化角色已由 adapter 类型系统强制；来源标记可由工具实现在 content 中自行加标签；输入清洗拦截点已有（TransformContextHook/BeforeToolCallHook/AfterToolCallHook）。缺口主要在 runtime 策略层：自动包裹来源标签的默认策略、注入短语黑名单模板、状态栏内容来源约束 |
| 7 | Skill 元数据注入位置（方式三） | runtime 策略层 | 方式三：元数据动态注入 + 完整内容工具按需加载 | ✅ 讨论后达成一致。机制层不缺：BeforeRunHook 可追加消息，SkillReadTool 已实现按需加载。缺口在 runtime 策略层：需默认策略将元数据通过 BeforeRunHook 追加到上下文末尾（方式三），而非注入 system prompt（方式一）。format_skills_for_system_prompt 函数已存在但无生产调用，且函数名暗示方式一，应改造为方式三 |
| 8 | Agent 状态栏（完整分析） | runtime 策略层为主 | 机制层已有的 hook 体系足够支撑，缺口全在策略层 | ✅ 讨论后达成一致。修正观点 5 的"状态栏注入通道→机制层缺失"：TransformContextHook/PrepareNextTurnHook/BeforeRunHook/AfterToolCallHook 已能追加消息到末尾，机制层不缺。缺口全在 runtime 策略层：通道 XML 格式模板、四种内容类型的默认收集器、两种更新策略的选择规则、五种具体技术模板、时间感三轴"读数+操作策略"配对模板 |
| 9 | 上下文压缩策略 | 机制层+runtime 策略层 | 只讨论效果好的策略所需的机制 | ✅ 讨论后达成一致。聚焦效果最好的策略：上下文感知压缩（策略四）、熔断器（层5）、保留优先级强制。三个机制层缺失：自定义压缩 prompt 通路（当前 SUMMARIZATION_SYSTEM_PROMPT 硬编码）、连续失败计数+熔断、标记消息为不可压缩。自适应窗口化（策略六）和工具结果预算控制（层1）机制层已具备 |

### 校验记录

#### 观点 1：四种消息角色 + tools 字段 → runtime 机制层

- **校验结果：正确。**
- **教程依据**：ch2 描述大模型 API 核心——消息列表（messages）中每条消息有角色（role）：system / user / assistant / tool；工具定义（tools）作为请求独立字段，不是消息角色。"四种消息角色 + tools 字段"覆盖第一章所说的五个上下文组成部分。
- **llm-api-adapter 实现**（git rev `10fb9a8`）：
  - `Message` 枚举（types/message.rs）：`System(String)` / `User(Vec<RequestContent>)` / `Assistant(Vec<ResponseContent>)` / `Tool{invocation_id, content, is_error}` — 四变体与教程四角色精确对应
  - `ChatRequest`（types/request.rs）：`messages: Vec<Message>` + `tools: Vec<ToolDef>` 独立字段 — tools 不在 messages 中，与教程一致
  - `RequestContent`（User/Tool 可用）：Text / Image — 编译期阻止把 ToolInvocation 放进 User 消息
  - `ResponseContent`（Assistant 可用）：Text / ToolInvocation / Reasoning — 精确对应教程 ch1 line 144 的"reasoning + content + tool_calls"三部分
  - `Message::Tool.invocation_id` 必须匹配 `ToolInvocation.id` — 与教程的 `tool_call_id` 关联一致
  - Anthropic wire 格式（anthropic/schema.rs）：`AnthropicRequest.system` 是顶层字段（不在 messages 中），与教程一致；OpenAI 用 `role: "system"` 消息，adapter 在转换层处理
- **runtime (harness) 实现**：
  - `AgentMessage` 枚举（types/messages.rs）：User / Assistant / ToolResult + CompactionSummary / BranchSummary / Custom — 比教程四角色更丰富，后三者是 runtime 扩展，不直接发给 LLM
  - `AgentContext.system_prompt: Option<String>` 独立存放 → loop_fn.rs:244 `insert(0, AMsg::System)` — system 是独立字段
  - `ConvertToLlmHook` 把 `AgentMessage` 转换为 adapter `Message` — 扩展消息在此过滤
  - `tools_to_defs()`（type_bridge.rs:35）把 `Vec<Arc<dyn Tool>>` 转为 `Vec<ToolDef>` → `ChatRequest.builder().tools()` — tools 作为请求顶层字段
  - `AssistantMessage.content: Vec<ContentBlock>` 有 Text / Thinking / ToolUse 三变体 — 对应 adapter 的 ResponseContent 三变体
  - `ToolResultMessage.tool_use_id` 对应 `ContentBlock::ToolUse.id` — 与 adapter 的 invocation_id 关联一致
- **关键设计点**：adapter 层用类型系统区分 RequestContent（输入侧）和 ResponseContent（输出侧），编译期保证语义正确；runtime 层用 `ConvertToLlmHook` 作为扩展点，支持 CompactionSummary 等内部消息不入 LLM 上下文。

#### 观点 2：KV Cache 友好的上下文设计 → 机制层+策略层（拆分）

- **教程依据**：ch2 KV Cache 友好的上下文设计——前缀完全不变才能命中缓存，前缀里一个字符被改写则缓存全部作废。反例：在 system prompt 里加 `Current time: {{now}}` 导致每次请求 KV Cache 完全失效。

- **机制层现状**：
  - ✅ system prompt 是静态前缀：`AgentContext.system_prompt` 在 run 开始时设定（`BeforeRunHook` 可修改一次），之后每个 turn 中不变。`loop_fn.rs:244` 每次 `insert(0, AMsg::System(sp))`，内容相同。
  - ✅ 消息历史是 append-only：`Session::append_message()` 只追加不修改已有消息。`build_context()` 按序构建，不改写已有消息。轨迹只增不改。
  - ✅ tools 定义独立于 messages：`ChatRequest.tools` 不在消息列表中，tools 变化不影响消息前缀的 cache。
  - ✅ adapter 层支持 prompt cache：`AnthropicCacheControl`（schema.rs:125）、`CacheHint`（convert.rs:72）、`apply_cache_hints()`（convert.rs:576）提供 cache control 标记能力；`Usage` 有 `cached_input_tokens` / `cache_creation_input_tokens` 字段追踪缓存命中。
  - ❌ runtime/loop 层未对接 adapter 的 CacheHint 能力：`StreamOptions.cache_config` 是 `Option<serde_json::Value>`，注释说"需要 provider adapter 支持后才能透传"，但没有和 adapter 的 `CacheHint` / `AnthropicExt.cache_hint()` 对接。loop 没有途径自动在 system prompt 或 tools 定义末尾打 cache_control 标记。

- **机制层需要补的**：
  - 无（层次 B 下 runtime 不引入缓存概念，cache hint 通路是 adapter 层职责）。

- **runtime 策略层（当前缺失，需要补的）**：
  - 约束 hook 不往 system prompt 注入动态内容（如时间戳）的通用原则，或检测动态注入并告警。
  - `ResourceInjector` 把资源追加到 system_prompt 末尾（resource.rs:89-96）——如果资源内容在 run 间变化会导致前缀不稳定。通用策略应约束资源内容稳定性或把动态资源放到消息列表尾部而非 system prompt。
  - `BeforeRunHook` 可修改 system_prompt（hooks.rs:277）和 `TransformContextHook` 可每轮转换上下文（hooks.rs:69-76）——两者都有能力破坏前缀稳定性。通用策略应提供检测和约束。
  - `PrepareNextTurnHook` 可替换整个 `AgentContext`（hooks.rs:95, `NextTurnDirective.context`）——如果替换了 system_prompt 或重排消息，前缀失效。通用策略应约束。

- **用户策略层**：
  - 无（层次 B 下 cache_control 的位置和粒度由 adapter 层处理，不属于用户策略）。

- **关键观察**：层次 B 下 runtime 的职责是保持前缀稳定性（机制层提供 append-only 消息、静态 system prompt；runtime 策略层约束 hook 行为），缓存优化（cache_control 标记、缓存命中追踪）完全交给 adapter 层。这与第一章的结论一致：runtime 提供机制和通用策略，provider 特定优化在 adapter 层。


#### 观点 3：缓存作为架构约束 + 平台无关 → 机制层+策略层（拆分）

- **教程依据**：ch2 "缓存作为架构约束"——Claude Code 的三个设计决策：(1) 提示词结构由缓存边界决定，可缓存内容在前动态在后；(2) 子 Agent 必须与父 Agent 字节级对齐以命中 Prompt Cache；(3) 工具结果的替换字符串在首次出现时被冻结。

- **平台无关抽象的层次选择**：
  - 层次 A（主动抽象）：runtime 抽象出平台无关的缓存概念（CacheBoundary、CacheKey）。→ **未采用**。
  - 层次 B（被动抽象）：runtime 只管前缀稳定性约束，不引入缓存概念。缓存优化完全由 adapter 层处理，runtime 通过保持前缀稳定来间接支持。→ **采用**。
  - 用户决策：runtime 必须抽象成平台无关，选择层次 B。

- **三约束校验**：

  **约束 1：提示词结构由缓存边界决定 — ⚠️ 违背**
  - runtime 没有缓存边界概念。`AgentContext.system_prompt` 是单块 `Option<String>`。
  - adapter 层 `Message::System(String)` 也是单字符串。`AnthropicContentBlocks` 支持 Blocks 数组（可在中间打 cache_control 断点），但 runtime 传过去的是单一字符串，无法利用。
  - 没有"可缓存前缀"和"动态后缀"的类型层区分。
  - **机制层需要补的**：无（层次 B 下 runtime 不引入缓存概念）。
  - **runtime 策略层需要补的**：通用策略约束 system prompt 的组成方式——静态内容在前，动态内容（如时间戳、用户偏好）不放 system prompt 而放消息列表尾部。
  - **用户策略层**：具体 system prompt 的内容编写。

  **约束 2：子 Agent 与父 Agent 字节级对齐 — ❌ 违背**
  - `SpawnOptions`（spawn_types.rs:7-16）只有 `prompt` / `provider` / `cwd` / `context` 四个字段。
  - 子 agent 通过 `HarnessBuilder::new(&model)` 从零构建（spawner.rs:273），不自动继承父 agent 的 system_prompt / tools / thinking_level。
  - `builder_customize` 是可选闭包（默认 `None`），不设置则子 agent 没有 system prompt、没有 tools。
  - `ContextMode::Inject` 只把父 agent 上下文作为文本前缀注入 prompt，不是消息级别的对齐。
  - 子 agent 的消息历史从空开始，与父 agent 的轨迹完全不同。
  - **后果**：子 agent 的 API 请求前缀与父 agent 不一致，无法命中父 agent 的 Prompt Cache。
  - **机制层需要补的**：`SpawnOptions` 增加配置继承机制——让子 agent 能继承父 agent 的 system_prompt / tools / thinking_level / 消息前缀，而非从零构建。
  - **runtime 策略层需要补的**：子 agent 默认继承父 agent 配置的默认策略。
  - **用户策略层**：具体子 agent 覆盖哪些配置（如使用不同 model、收窄 tools 子集）。

  **约束 3：工具结果的替换字符串在首次出现时被冻结 — ✅ 基本满足**
  - `ToolSessionProjection`（types/tool.rs:79-146）有三种模式：`Full`（保留完整内容）、`Projected`（替换为摘要内容）、`Ephemeral`（只保留 summary）。
  - `Projected` 和 `Ephemeral` 的替换内容被持久化到 session log 中（`ToolResultMessage.content` + `projection`）。
  - session 恢复时用相同的持久化内容，不会重新生成。
  - 满足教程说的"替换字符串在首次出现时被冻结"。
  - **无需补**。

- **关键观察**：层次 B 的选择意味着 runtime 的职责边界清晰——只管前缀稳定性（机制层提供 append-only 消息、静态 system prompt、配置继承机制；策略层提供约束 hook 行为的通用原则），缓存优化（cache_control 标记、缓存命中追踪）完全交给 adapter 层。这与第一章的结论一致：runtime 提供机制和通用策略，provider 特定优化在 adapter 层。

#### 观点 4：提示工程：优化系统提示词 → 用户策略层

- **教程依据**：ch2 "提示工程：优化系统提示词"一节的 6 个小节。

- **逐小节归类**：

  1. **语气与风格（"人格"）** → 用户策略层。大写强调、简洁约束等写法技巧，是 system prompt 内容编写。教程 line 575。
  2. **结构化提示（"格式"）** → 用户策略层。XML 标签 + Markdown 协同，是 prompt 内容的组织方式原则。教程 line 577-581。
  3. **流程驱动 vs 规则堆砌（"组织方式"）** → 用户策略层。SOP 式流程优于零散规则，是 prompt 内容的组织方式原则。教程 line 583-610。
  4. **业务规则细化（"内容"）** → 用户策略层。教程明确说"这不是技术问题，而是产品设计问题，需要产品经理的深度参与"（line 614）。计费规则、金额计算等业务逻辑属于用户策略。教程 line 612-630。
  5. **Few-shot 示例** → 用户策略层。何时用示例、放哪里（system prompt vs 伪造 user/assistant 消息）、数量控制，都是 prompt 内容决策。教程 line 632-638。
  6. **工具定义的设计** → 留到第四章讨论。教程自己说"工具定义的设计原则和最佳实践将在第四章详细展开"（line 644）。

- **讨论点**：Few-shot 示例中"示例应保持字节级稳定"的前缀稳定性原则已在观点 2 中覆盖（runtime 策略层约束前缀稳定性），此处不重复。"用几个示例、放哪里"是用户策略层。

- **讨论点**：runtime 策略层在提示工程中是否有角色？教程说"提示词一般由产品经理来设计"（line 628），prompt 内容完全在用户策略层。runtime 策略层不碰 prompt 内容——只管前缀稳定性约束（观点 2 已覆盖）和机制支撑（system prompt 作为静态字段、append-only 消息历史等，观点 1 已覆盖）。

#### 观点 5：Agent 状态栏 + TODO write → 三层拆分

- **教程依据**：ch2 "Agent 状态栏"一节（line 786-960）。状态栏是在上下文末尾以 user 角色消息注入 `<agent_status>` 元信息的机制，包括任务规划（TODO 列表）、侧信道信息、环境状态、可用能力清单。TODO write 提供 `rewrite_todo_list` 和 `update_todo_status` 两个工具（line 930）。实验数据：启用 TODO 的 Agent 平均 15 次迭代完成任务，禁用时需要 21 次且经常遗漏子任务。

- **原始观点**：TODO write 功能要做成 runtime 策略层。

- **讨论后拆分为三层**：

  1. **状态栏注入通道 → runtime 机制层（当前缺失）**：
     - 在上下文末尾以 user 角色消息注入框架生成的 `<agent_status>` 内容的能力。
     - 教程明确：状态栏作为 user 消息追加到上下文末尾，不修改 system prompt（line 885-908）。
     - runtime 现有 `PrepareNextTurnHook`（可通过 `NextTurnDirective.context` 替换整个 AgentContext）和 `TransformContextHook`（每轮 LLM 调用前转换上下文）理论上可以做，但没有通用的"在上下文末尾追加框架生成的 user 消息"通道。
     - 当前实现（`DefaultPrepareNextTurn`、`WorkflowGateHook`、`StructuredPrepareNextTurnHook`）都没做状态栏注入。
     - **需要补的**：一条通用的"在上下文末尾追加框架生成的 user 消息"通路，不依赖业务层手动实现。

  2. **TODO write 工具 + 状态栏内容模板 → runtime 策略层（当前缺失）**：
     - `rewrite_todo_list` / `update_todo_status` 工具的实现。
     - TODO 项的数据结构（标识符、内容、状态 pending/in_progress/completed/cancelled、时间戳）。
     - 状态栏的 XML 格式模板（`<agent_status>` 标签包裹、键值对格式）。
     - 教程的三条经验（line 845-851）：状态栏用代码维护而非 LLM 维护、删原文前确认状态栏覆盖所有问题、状态栏准确率是一线生产指标。
     - 状态更新两种实现的选择策略（line 912-920）：每轮替换 vs 持久追加，按轨迹长度和状态消息大小取舍。
     - 这些是通用的，不绑定具体业务，适合 runtime 策略层提供默认实现。

  3. **具体 TODO 内容 → 用户策略层**：
     - 任务分解为哪些步骤、什么算完成，由具体任务决定。

- **runtime 现状**：没有 `todo_write` / `rewrite_todo_list` / `update_todo_status` 工具；没有 Agent 状态栏机制；没有在上下文末尾注入 `<agent_status>` 消息的通用通道。

- **关键观察**：Agent 状态栏的机制层需求（上下文末尾追加 user 消息）与 KV Cache 友好设计（观点 2-3）一致——追加到末尾不破坏前缀缓存。状态栏注入通道是观点 2 中"动态信息追加到末尾"原则的工程化落地。

#### 观点 6：提示注入防御 → runtime 策略层为主

- **教程依据**：ch2 "提示注入：上下文安全的核心威胁"（line 673-706）。三个防御手段：来源标记（`<external_content source="webpage">` 包裹外部内容）、结构化角色（严格用 system/user/assistant/tool 传递信息）、输入清洗（过滤注入短语）。另提到 Skills 安装审查和状态栏投毒防护。

- **设计原则**：runtime 策略层能做的尽量不放在机制层。

- **逐条归类**：

  **1. 来源标记**
  - 机制层：✅ 不缺。`Tool::execute` 返回 `ToolResult`，内容完全由工具实现控制，业务层可自行在 `content` 中加 XML 标签。不需要在 `ToolResultMessage` 或 `DataBlock` 上新增来源标记字段。
  - runtime 策略层：❌ 缺失。自动包裹来源标签的默认策略（如工具 wrapper 或 `AfterToolCallHook` 自动给外部内容工具的结果加 `<external_content source="...">` 标签）。
  - 用户策略层：具体标签格式和来源命名。

  **2. 结构化角色**
  - 机制层：✅ 已具备。adapter 类型系统（`RequestContent` vs `ResponseContent`）编译期强制角色分离，阻止把工具结果塞进 user 消息。观点 1 已确认。
  - runtime 策略层：无。
  - 用户策略层：无。

  **3. 输入清洗**
  - 机制层：✅ 不缺。`TransformContextHook`（每轮 LLM 调用前转换上下文）、`BeforeToolCallHook` / `AfterToolCallHook`（工具调用前后拦截）已提供拦截点。
  - runtime 策略层：❌ 缺失（低优先级）。注入短语黑名单、正则过滤规则模板。教程说"这层防御容易被措辞变体绕过，只能作为辅助手段"。
  - 用户策略层：具体清洗规则和阈值。

  **4. Skills 安装审查**
  - 机制层：✅ 不缺。教程说的是"审查其内容，如同审查将要执行的代码"，是内容审查而非签名验证。
  - runtime 策略层：无。
  - 用户策略层：❌ 审查具体 Skill 内容。

  **5. 状态栏投毒防护**
  - 与观点 5 关联。状态栏注入通道（机制层，观点 5 已标记缺失）是前提。
  - runtime 策略层：❌ 缺失。约束状态栏内容来源的通用原则——教程说"状态栏里的信息越是来自对真实世界的可靠观测越好，绝不能来自可被外部污染的数据源"。
  - 用户策略层：无。

- **关键观察**：提示注入防御的主要缺口在 runtime 策略层，而非机制层。机制层已有的拦截点（hook 体系 + adapter 类型系统）足够支撑防御实现，缺的是通用的默认策略。这与第一章护栏交叉校验的结论一致：机制层已具备，runtime 策略层缺失。

#### 观点 7：Skill 元数据注入位置 → runtime 策略层

- **教程依据**：ch2 "Skills 的实现方式与权衡"（line 741-757）。三种方式：方式一（注入 system prompt，指令遵循最好但每次加载 Skill 破坏 KV Cache）、方式二（作为 tool result 在上下文中间，不破坏缓存但指令遵循差）、方式三（元数据动态注入 + 完整内容通过专用工具按需加载，兼顾缓存和指令遵循）。教程推荐方式三，Claude Code 生产实现。

- **教程区分两个层次**（line 751-755）：
  - "元数据需要提前对模型可见" → 较稳定的机制设计
  - "用什么角色注入（user/system/`<system-reminder>`）" → 具体版本的实现方式

- **设计原则**：runtime 策略层能做的尽量不放在机制层。`BeforeRunHook` 已能在 run 开始时追加消息，`SkillReadTool` 已实现按需加载完整内容。已有的 hook 体系足够支撑方式三，机制层不需要新增任何东西。

- **三层归属**：

  **机制层**：✅ 不缺。
  - `BeforeRunHook`：可在 run 开始时向 `additional_messages` 追加消息，元数据通过此 hook 注入上下文末尾。
  - `SkillReadTool`（`skill_read`）：已实现方式三的"完整内容按需加载"部分——支持读取 SKILL.md 主文件和子文件，有路径穿越防护（拒绝 `../../../etc/passwd`）、大文件截断（256KB）、XML 标签转义。
  - `Skill` 类型：有 name / description / source / base_dir / disable_model_invocation，元数据字段完整。

  **runtime 策略层**：❌ 缺失。
  - 需要一个默认策略：在 run 开始时将 Skills 元数据（name + description）通过 `BeforeRunHook` 追加到上下文末尾（方式三），而非注入 system prompt（方式一）。
  - `format_skills_for_system_prompt` 函数已存在（生成 `<available-skills>` XML），但**没有生产代码调用**——元数据当前根本没被注入。且函数名暗示方式一（注入 system prompt），应改造为方式三（通过 hook 追加到上下文末尾）。
  - Skills 元数据属于低频静态内容（仅在安装/卸载 Skill 时变），注入一次即可，不需每轮更新——与状态栏的动态内容（TODO、环境状态）不同。

  **用户策略层**：具体每个 Skill 的 description 写法。教程强调"Use when / Don't use when" + 反例是路由准确率的关键——"缺少反例的 Skill 描述会让路由准确率明显下降"。

- **关键观察**：`format_skills_for_system_prompt` 函数存在但未被调用，说明 Skills 元数据注入功能从未接线。函数名和设计意图是方式一（system prompt），应整体改造为方式三。这与观点 2-3 的 KV Cache 约束一致——不往 system prompt 注入动态内容。

#### 观点 8：Agent 状态栏 → runtime 策略层为主

- **教程依据**：ch2 "Agent 状态栏：通过元信息增强 Agent 轨迹管理"（line 786-960）。状态栏是 Agent 框架在上下文末尾以 user 角色消息注入 `<agent_status>` 元信息的通用通道。包含四种内容类型、两种更新实现、五种具体技术、时间感三轴。

- **修正观点 5**：观点 5 曾说"状态栏注入通道 → 机制层，当前缺失"。逐个确认后发现 runtime 已有的 hook 体系足够支撑追加消息到上下文末尾，机制层不缺。观点 5 中 TODO write 工具+XML 格式模板仍属 runtime 策略层缺失，但"通道本身缺失"的判断需修正。

- **逐项三层归属**：

  **1. 通道本身**（上下文末尾追加 user-role meta 消息）
  - 机制层：✅ 不缺。`TransformContextHook` 接收并返回 `AgentContext`，可追加消息；`PrepareNextTurnHook` 通过 `NextTurnDirective.context` 可替换上下文；`BeforeRunHook` 可追加 `additional_messages`。三个 hook 都能实现"追加消息到末尾"。
  - runtime 策略层：❌ 缺失。`<agent_status>` 标签包裹的 XML 格式模板。
  - 用户策略层：无。

  **2. 理论基础**（上下文蒸馏）
  - 三层都不归属。教程解释"为什么有效"的理论，不是 runtime 要实现的东西。

  **3. 四种内容类型**

  - **3a. 侧信道信息**（时间戳前缀到用户消息和工具响应）
    - 机制层：✅ 不缺。`TransformContextHook` 可给用户消息加前缀；`AfterToolCallHook` 可给工具结果加前缀。
    - runtime 策略层：❌ 缺失。默认时间戳格式模板。
    - 用户策略层：哪些事件需要侧信道信息。

  - **3b. 可用能力清单**（Skills 元数据）
    - 观点 7 已讨论。机制层不缺，策略层缺失。

  - **3c. 任务规划**（TODO）
    - 观点 5 已讨论。TODO write 工具 + XML 格式模板 → runtime 策略层缺失。

  - **3d. 环境状态**（系统时间、工作目录、工具调用计数、详细错误信息）
    - 机制层：✅ 不缺。`AfterToolCallHook` 可统计工具调用次数；`BeforeRunHook` 可注入系统时间、工作目录等。
    - runtime 策略层：❌ 缺失。默认的环境状态收集器、工具调用计数器、详细错误信息模板。
    - 用户策略层：哪些环境信息需要注入。

  **4. 两种更新实现**（每轮替换 vs 持久追加）
  - 机制层：✅ 不缺。`TransformContextHook` 可过滤旧消息再追加（实现一）或纯追加（实现二）。
  - runtime 策略层：❌ 缺失。根据内容类型选择更新策略的默认规则——低频静态内容（Skills 元数据、侧信道）用持久追加；高频动态内容（TODO、环境状态）按轨迹长度和状态大小选择。
  - 用户策略层：无。

  **5. 五种具体技术**

  - **5a. 时间戳跟踪**：机制层 ✅（TransformContextHook / AfterToolCallHook）；策略层 ❌ 缺失默认格式模板。
  - **5b. 工具调用计数器**：机制层 ✅（BeforeToolCallHook / AfterToolCallHook）；策略层 ❌ 缺失默认计数器实现 + 格式模板。
  - **5c. TODO 列表管理**：观点 5 已讨论。策略层 ❌ 缺失。
  - **5d. 详细错误信息**：机制层 ✅（AfterToolCallHook 可修改 ToolFailure 内容）；策略层 ❌ 缺失四层错误信息模板（错误类型、参数 JSON、调用栈、修复建议）。
  - **5e. 系统状态感知**：机制层 ✅（BeforeRunHook 可注入系统时间、cwd 等）；策略层 ❌ 缺失默认系统状态收集器（时间、cwd、OS、Shell）。

  **6. 时间感三轴**（紧迫度、坚持度、警觉度）
  - 机制层：✅ 不缺。读数和操作策略都是文本，通过同一通道注入，机制上无区别。
  - runtime 策略层：❌ 缺失。默认的"读数 + 操作策略"配对模板——教程核心发现：光给读数不够（只给原始时间戳≈什么都不给），必须把读数和操作策略成对给出（时间戳+操作手册通过率从一成拉到四五成，+19 到 +49 个百分点）。
  - 用户策略层：具体操作策略的措辞和阈值——什么算"慢"、什么算"撞墙次数够了"。教程给了三轴框架，但具体阈值由业务决定。

- **关键观察**：状态栏是第二章 runtime 策略层缺失最密集的区域——6 个方面中每一个的策略层都缺失。但机制层全面不缺，已有的 hook 体系（TransformContextHook / PrepareNextTurnHook / BeforeRunHook / BeforeToolCallHook / AfterToolCallHook）足以支撑所有状态栏功能。这与全章一致结论相同：机制层已具备，runtime 策略层系统性缺失。

#### 观点 9：上下文压缩策略 → 机制层+runtime 策略层

- **教程依据**：ch2 "上下文压缩策略"（line 961-1086）。六种压缩策略实验（策略四上下文感知压缩效果最佳：7 次迭代、40,157 token、压缩率 3.0%）+ 五层分层压缩机制。只讨论效果好的策略所需的机制。

- **runtime 现有压缩机制**：
  - `compact()` 方法：全量压缩，将 cut point 之前所有消息合并为一份 LLM 摘要
  - `compute_cut_point()`：按 token 阈值算切点（`reserve_tokens` + `keep_recent_tokens`）
  - `try_auto_compact`：阈值触发，在 `prompt_with_messages` 入口检查
  - `CompactionEntry`：记录 `first_kept_entry`，`live_region_start()` 跳过已压缩区（防重复标记）
  - `BeforeCompactHook`：决策拦截（Proceed / Skip / Compact / Override）
  - `SUMMARIZATION_SYSTEM_PROMPT` + `SUMMARY_REQUEST`：硬编码的摘要 prompt 常量
  - `ToolSessionProjection`（Full / Projected / Ephemeral）：工具结果投影，`PersistedToolProjection` 持久化（替换冻结）
  - `CompactionModelConfig`：可配置独立的压缩模型

- **逐项三层归属**：

  **1. 上下文感知压缩（策略四，效果最佳）**
  - 教程核心：压缩 prompt 中包含当前查询意图（`Given the search query: {query}`）和已积累信息（`Current context: {context}`），引导模型生成有针对性的摘要。
  - 机制层：❌ 缺失。`SUMMARIZATION_SYSTEM_PROMPT` 和 `SUMMARY_REQUEST` 是硬编码常量，没有让调用方传入查询意图或自定义 prompt 的途径。`BeforeCompactHook` 的 `Override(CompactionResult)` 可以完全替换摘要结果，但要求 hook 自己调 LLM——等于绕过框架压缩流程，不是"在框架压缩中注入上下文"。需要：自定义压缩 prompt 通路。
  - runtime 策略层：❌ 缺失。默认的上下文感知压缩 prompt 模板（包含查询意图和当前上下文）。
  - 用户策略层：具体保留哪些信息。

  **2. 熔断器（层 5）**
  - 教程：连续失败达到一定次数后自动停止重试。生产数据表明大量会话被困在反复压缩失败循环中。
  - 机制层：❌ 缺失。`compact()` 失败后直接返回错误，没有连续失败计数和熔断。需要：连续失败计数 + 停止重试机制。
  - runtime 策略层：❌ 缺失。默认熔断阈值（连续失败 N 次后停止）。
  - 用户策略层：无。

  **3. 保留优先级强制（教程 line 1070-1078）**
  - 教程：压缩最容易丢失的是架构决策、约束理由、失败路径。建议显式定义保留优先级——架构决策不得摘要、文件列表完整保留、验证状态必须保留、未解决 TODO 必须保留、工具输出可删除。标识符（UUID/hash/IP/URL/文件名）必须原样保留。
  - 机制层：❌ 缺失。`SUMMARIZATION_SYSTEM_PROMPT` 有 "preserve key decisions" 文字建议但无强制力——LLM 可能不遵守。`compute_cut_point` 按 token 阈值算切点，不区分消息类型，没有"保护特定消息不参与压缩"的机制。需要：标记消息为不可压缩，压缩时跳过。
  - runtime 策略层：❌ 缺失。默认保留优先级规则——哪些消息类型自动标记为不可压缩。
  - 用户策略层：具体哪些消息需要强制保留。

  **4. 自适应窗口化（策略六）**
  - 机制层：✅ 不缺。阈值触发（`try_auto_compact`）、批量压缩（`compact()` 一次性压缩）、防重复标记（`CompactionEntry` + `live_region_start()`）。
  - runtime 策略层：✅ 不缺。阈值默认值已有（`reserve_tokens=16384`、`keep_recent_tokens=20000`）。
  - 用户策略层：无。

  **5. 工具结果预算控制（层 1）**
  - 机制层：✅ 不缺。`ToolSessionProjection`（Full / Projected / Ephemeral）+ `PersistedToolProjection` 持久化（替换冻结）。
  - runtime 策略层：❌ 缺失（低优先级）。默认的工具结果投影策略——什么大小自动用 Projected/Ephemeral。
  - 用户策略层：具体工具的投影策略。

- **关键观察**：压缩策略是第二章中机制层有实质缺失的少数领域——三个机制层缺失（自定义压缩 prompt 通路、熔断器、标记消息为不可压缩），不能仅靠策略层弥补。这与状态栏（观点 8，机制层全面不缺）形成对比。根因：现有 `compact()` 设计为单一全量压缩流程，不支持上下文感知、选择性保留、失败熔断等精细化控制。

---

## 第三章：用户记忆和知识库

（待读）

---

## 第四章：工具

### 讨论框架

每个工具/工具类别讨论时，区分四个维度：

**维度 1：工具归属层**
- **runtime 内置工具**（`ToolLevel::BuiltIn`，名字保留）：框架运行必须的、与 loop 引擎紧耦合的工具。当前 11 个：`final_answer`、`wait_for_external_event`、`spawn_agent`、`submit_workflow`、`skill_read`、`message_subagent`/`await_subagent_reply`/`query_subagent`/`abort_subagent`、`message_main`/`await_main_message`
- **`llm-harness-runtime-tools` 常用工具**（`ToolLevel::User`，可选 crate）：通用但非框架必须的工具。当前 4 个：`bash`、`read`、`write`、`edit`
- **用户自定义工具**（`ToolLevel::User`，业务层）：面向特定任务的工具

**维度 2-4：三层归属**（与前章一致）
- **runtime 机制层**：trait 定义、执行引擎、hook 体系、安全拦截点等基础设施
- **runtime 策略层**：默认策略、模板、约束 hook 行为的通用原则
- **用户策略层**：具体 prompt、工具实现、参数设计、业务规则

### 用户观点

| # | 教程内容 | 归属层 | 用户理由 | 校验 |
|---|----------|--------|----------|------|
| 10 | 感知工具的输出信息量控制 | 不上升为统一策略层 | 各工具输出特征差异大，统一阈值要么太宽要么太窄；截断位置和格式依赖工具自身语义 | ✅ 讨论后达成一致。机制层不缺：ToolSessionProjection（Full/Projected/Ephemeral）+ AfterToolCallHook + 工具实现的 offset/limit。当前各工具各自实现截断（ReadTool DEFAULT_MAX_LINES=300、SkillReadTool 256KB），BashTool 无截断。输出控制留在具体工具实现，不上升为统一策略层。唯一通用原则"截断必须显式标注"是设计准则非 runtime 功能 |
| 11 | 执行工具的安全约束 | 机制层+runtime 策略层 | 机制层大部分已具备，但有两个实质缺失 | ✅ 讨论后达成一致。逐条对照教程 9 项安全机制：机制层已具备 5 项（权限控制 SandboxConfig、事前审批 BeforeToolCallHook async+Allow/Deny/Modify、自动验证 AfterToolCallHook、沙盒隔离 Sandbox trait+bwrap/seatbelt、取消 CancellationToken）；机制层缺失 2 项：Sidecar 专用机制（与流式输出并行的轻量 LLM 门控——BeforeToolCallHook 上下文已天然满足输入隔离，但非"与流式输出并行"）、幂等性机制（idempotency key/预检-确认两段式）；策略层缺失 3 项：默认危险命令黑名单、默认路径遍历防护规则、默认审计日志格式 |
| 12 | 协作工具 | 机制层基本完备+runtime 策略层缺失 | 7 个内置工具覆盖完整生命周期，但 HITL 和 Agent 发现有缺口 | ✅ 讨论后达成一致。机制层：7 个 BuiltIn 工具（spawn_agent/message_subagent/await_subagent_reply/query_subagent/abort_subagent/message_main/await_main_message）覆盖启动/取消/消息传递/发现/查询。机制层缺失 2 项：QuerySubagentTool 不返回职责描述（只有 agent_id+status）、子 Agent 配置继承（观点 3 已标记——SpawnOptions 不继承 system_prompt/tools/thinking_level）。runtime 策略层缺失 2 项：HITL 内置工具+超时默认行为（request_human_approval/request_human_input 无 BuiltIn 工具，机制层 BeforeToolCallHook async+steer/follow_up channel 可实现但无开箱即用工具）、HITL 超时降级默认策略 |
| 13 | 用户沟通工具 | runtime 策略层 | 需要内置工具+渠道抽象 trait | ✅ 讨论后达成一致，后修正分类。当前只有 final_answer（结束 run），缺"向用户发消息但不结束 run"的内置工具。**修正**：原记"机制层缺失 NotificationChannel trait"有误——NotificationChannel 无机制层消费者（loop_fn/dispatch/session 不发通知），notify_user 工具只需实现 Tool trait 不改 loop 内部逻辑，符合策略层判定标准（观点 6 起的设计原则：策略层能做的尽量不放在机制层）。NotificationChannel trait + notify_user 工具均放 `llm-harness-strategy` crate，不进机制层 `types/`。ExecutionEnv 类比不成立：ExecutionEnv 有多个机制层消费者（read/write/bash/SkillReadTool/compaction），NotificationChannel 零个。runtime 策略层缺失：NotificationChannel trait + 内置 notify_user 工具（LLM 可调用，通过 NotificationChannel 发送，不结束 run）、未配置渠道时 fallback 到 final_answer/steer、超时降级、多渠道选择策略（后者暂不做） |
| 14 | 事件触发工具 | 机制层基本完备+runtime 策略层缺失 | EventStream trait+WaitForExternalEventTool+steer/follow_up/abort 覆盖完整链路 | ✅ 讨论后达成一致。机制层：EventStream trait（业务层实现）+WaitForExternalEventTool（BuiltIn，阻塞等待事件，事件到达后 tool 返回 loop 续跑）+TaskStore（崩溃恢复）+steer（运行中注入消息）+follow_up（should_stop 后注入）+abort（CancellationToken）。机制层基本不缺。runtime 策略层缺失：三类常见事件触发工具（定时器 set_timer/后台监控 monitor_shell/外部通道 connect_channel）无内置实现，都要求业务层自己实现 EventStream |
| 15 | 通用性设计：grep、glob 与 code_interpreter | llm-harness-runtime-tools 补齐 | bash+Sandbox=code_interpreter，不需要单独内置；grep+glob 需补齐 | ✅ 讨论后达成一致。code_interpreter 不需要在 runtime-tools 中内置——bash 是更通用的执行器（python -c/script.py 即可），沙盒由 Sandbox trait+bwrap/seatbelt 解决，预装库是部署配置。grep 需要补齐——跨平台语法差异（BSD vs GNU）让 Agent 自由发挥容易出错。glob 需要补齐——跨平台 glob 行为差异（shell 展开 vs 程序内匹配），Agent 用 bash 的 find 命令会遇到平台差异问题。当前 runtime-tools 有 read/write/edit/bash 但缺 grep/glob。三者均为 llm-harness-runtime-tools 常用工具 |
| 16 | 工具描述艺术+参数保真性+工具演进 | 工具实现改进点 | 对照教程三节检查所有工具实现 | ✅ 讨论后达成一致。工具描述：所有工具 description 只说"能做什么"不说"什么时候用"和"做不到什么"；参数描述无具体例子；无调用示例（教程说附带示例可使准确率从 72%→90%）；read/edit 暴露实现细节（hashline/snapshot store）。参数保真性：edit 静默添加 trailing newline——与原文件内容有系统性偏差。工具演进：示例驱动调用缺失（同工具描述）、动态工具发现留到主动工具发现一节、代码编排执行留到第五章 |
| 17 | 工具生态：MCP 与工具选择 | 机制层+runtime 策略层 | MCP 协议适配完备，缺信任管理和上下文开销管理 | ✅ 讨论后达成一致。机制层基本完备：McpServerConfig 支持 Stdio/HttpOrSse 传输、McpServerCapabilities 有 tools/resources/prompts 三类原语分离、McpToolAdapter 适配为 Tool trait、McpPromptRegistry 发现和调用提示模板、McpResourceInjectionHook 按策略注入资源、McpToolCache 工具定义缓存+持久化、sanitize_name 加 mcp__ 前缀防同名遮蔽、SSE validate_same_origin 防恶意重定向、重连熔断器。机制层缺失 2 项：服务器版本锁定、最小权限凭证管理。runtime 策略层缺失 3 项：工具搜索工具（discover_tools，按需加载不全量注入 schema）、工具层次化组织（按信息源性质分类）、工具描述审查机制（把 description 当不可信输入审计） |
| 18 | 长输出的截断与持久化 | 机制层+runtime 策略层 | 截断机制已有但未应用到 Agent 工具，缺"保存到临时文件" | ✅ 讨论后达成一致。机制层：TruncationStrategy（Head/Tail/HeadTail）+ truncate_utf8() 已在 workflow ShellExecutor 中实现，HeadTail 保留头尾+中间标注 `... [truncated N bytes] ...`。ToolSessionProjection 有 Full/Projected/Ephemeral 三种投影，Projected 可用截断内容替换 model_content。机制层缺失：截断后完整输出保存到临时文件——当前 Projected 投影后原始内容丢弃，不保存到文件，教程要求持久化到文件让 Agent 按需读取。runtime 策略层缺失：BashTool 直接返回 stdout+stderr 全量，无截断（workflow ShellExecutor 有截断但 Agent 用的 BashTool 没有）；截断标注中引导 Agent 用 read 工具读取临时文件 |
| 19 | 子 Agent 设计哲学+协作机制（深化） | 机制层+runtime 策略层 | view 12 已覆盖三组原语；本次深化提示词要素和上下文来源标注 | ✅ 讨论后达成一致。view 12 已确认三组原语（启动取消/消息传递/发现）机制层基本完备，四种协作形态（同步/异步/流式/多轮）全覆盖。本次新增：机制层缺失：ContextMode 需结构化上下文来源标注——当前 Inject(String) 是无结构文本，主 Agent 任务指令/用户补充/工具结果混在一起，教程要求区分 [FROM_MAIN_AGENT]/[FROM_USER]/[TOOL_RESULT] 防混淆和提示注入攻击。runtime 策略层缺失：ContextMode::Inject 注入格式无来源标注——当前格式为 "Context:\n{ctx}\n\n---\n\n{prompt}"，无来源标签。用户策略层：角色定义、任务边界、输出格式保持自由文本由用户策略层决定，不强制 JSON |
| 20 | HITL 超时与降级策略 | runtime 策略层 | view 12 已标记 HITL 工具缺失；本次深化超时参数和降级默认行为 | ✅ 讨论后达成一致。机制层：BeforeToolCallHook::on_call 是 async，可 tokio::time::timeout 包裹等待；BeforeToolCallDecision 有 Allow/Modify/Deny 三态，超时后可返回 Deny（保守策略）或 Modify（安全默认值）。机制可拼装但无内置支持。runtime 策略层缺失：HITL 内置工具需带 timeout 参数（如 AwaitSubagentReplyTool 的 timeout 模式）；超时降级默认行为——超时自动返回 Deny（保守策略），可选配置为 Modify（安全默认值）。优先级队列和多渠道通知暂不做（太复杂） |
| 21 | 事件驱动的异步 Agent | 机制层（设计选择确认） | 同步 loop+异步工具模型；mid-turn steer 明确不做 | ✅ 讨论后达成一致。runtime 采用"同步 loop+异步工具"模型，非事件循环。agent loop 按 turn 推进：LLM 回复→tool 执行→结果回填→下一轮。turn 间可中断（steer drain/follow_up 注入/abort 终止），turn 内不可中断（mid-turn steer 明确不做）。四类输入未统一为事件流：用户消息走 steer/follow_up、工具返回走 loop 内回填、外部事件走 EventStream+WaitForExternalEventTool、定时触发由业务层自实现。OpenClaw 三机制中 Hooks 有对应（BeforeRunHook/AfterToolCallHook 等），Cron/Heartbeat 无内置（view 14 已标记）。Channel 机制对应 EventStream+WaitForExternalEventTool，但事件只在 LLM 主动调用 tool 时才能接收，非实时推送——这是同步 loop 模型的固有限制。外部事件实时推送（事件到达即触发）是改进点，但当前可通过 SpawnAgentTool spawn 专门的事件监听子 agent 替代 |
| 22 | 同步模型支持异步打断+abort 早停与接续 | runtime 策略层+机制层确认 | 规则 1/2/5 符合；规则 3/4 不做（mid-turn steer）；abort 早停+接续完备 | ✅ 讨论后达成一致。教程五条规则对照：规则 1（assistant message 立即记录）✅ loop_fn.rs:428-430；规则 2（tool result 完成时记录）✅ loop_fn.rs:564-588；规则 3（工具执行中打断需占位符）❌ 不做（view 21 mid-turn steer 明确不做）；规则 4（LLM 思考中打断丢弃）❌ 不做（同上）；规则 5（非打断事件队列批处理）✅ steer turn 间批量 drain。异步工具接口设计（initiate+事件通知解耦）✅ SpawnAgentTool 已是此模式。abort 早停：CancellationToken 在 turn 间（loop_fn.rs:205）、retry sleep（loop_fn.rs:337）、tool 内部（ToolContext.abort）三处检查；AbortCascadeHook 级联取消 sub-agent；abort 后 phase_guard RAII drop 恢复 Idle；已完成 turn 已 flush，未完成 turn 清空 pending_session_writes。abort 后接续：调 prompt()/continue_run() 从 session 当前状态继续，build_context() 重建上下文。改进点：批量 steer 消息编号+汇总（drain 多条 steer 时添加 [未处理事件 N/M] 标记和末尾汇总） |
| 23 | 主动工具发现 | runtime 策略层 | view 17 已标记 discover_tools+层次化组织；动态 schema 追加+KV Cache 优化暂不做 | ✅ 讨论后达成一致。当前 runtime 全量注入工具 schema 到 API tools 参数（MCP builder.rs:225-227）。runtime 有动态工具能力：set_tools() 运行中替换工具列表、set_active_tools() 控制激活子集、NextTurnDirective.tools 在 PrepareNextTurnHook 中替换工具。但无 discover_tools 元工具（view 17 已标记）、无层次化匹配（view 17 已标记）、无动态 schema 追加到上下文末尾（教程要求 schema 作为对话历史追加，非 API tools 参数，保持静态前缀+KV Cache 复用）。改进点 3（动态 schema 追加到上下文末尾）和 4（schema 固定在首次注入位置）暂不做——太复杂，当前全量注入模式对几十个工具足够用 |

### 校验记录

#### 观点 10：感知工具的输出信息量控制 → 不上升为统一策略层

- **教程依据**：ch4 "感知工具"（line 150-180）。感知工具面临返回信息量远超 Agent 处理能力的挑战。应对手段：上下文感知压缩（观点 9 已讨论）、分页/游标、offset/limit + 截断策略、多模态输出形态选择。

- **设计问题**：runtime 在机制和策略层需要做通用的粒度权衡和输出信息量控制功能吗？

- **粒度权衡**：不需要。粒度是具体工具的设计决策（`read_document` vs `extract_pdf_text`），runtime 提供不了通用的"应该整合还是分离"的判断。纯用户策略层。

- **输出信息量控制**：不上升为统一策略层。各工具输出特征差异太大——文件有行号、搜索结果有条目、bash 有 stdout/stderr 分离、网页有 DOM 结构。统一阈值（如"所有工具输出 256KB 截断"）要么对某些工具太宽、要么对另一些太窄。截断位置和格式都依赖工具自身语义。

- **runtime 现状（各工具各自实现）**：
  - `ReadTool`：`DEFAULT_MAX_LINES=300`、`READ_MAX_BYTES`、目录 `DIR_MAX_ENTRIES=12`、offset/limit 参数、截断时标注行号范围
  - `SkillReadTool`：`SKILL_READ_MAX_BYTES=256KB`，截断时标注 `(truncated, N bytes omitted)`
  - `BashTool`：无输出截断（只有超时限制 `MAX_TIMEOUT_SECS=3600`）
  - `ToolSessionProjection`（Full/Projected/Ephemeral）：机制层通用投影机制，但无默认策略

- **三层归属**：

  **机制层**：✅ 不缺。`ToolSessionProjection`（Full/Projected/Ephemeral）+ `AfterToolCallHook`（可修改工具返回内容）+ 工具实现的 offset/limit。

  **runtime 策略层**：不上升为统一策略层。唯一通用原则"截断必须显式标注"是设计准则，不是需要 runtime 实现的功能——`AfterToolCallHook` 已经能在工具返回后检查并补充标注。

  **用户策略层**：具体工具的 offset/limit 参数设计、分页/游标接口、截断格式。

  **工具归属层**：感知工具（`read`、`grep_file`、`find_file`、`fetch_url`、`web_search`、`knowledge_base_search`）→ `llm-harness-runtime-tools` 或用户自定义。`read` 已在 runtime-tools 中；`BashTool` 无截断是该工具自身的缺失，不是 runtime 策略层的问题。

#### 观点 11：执行工具的安全约束 → 机制层+runtime 策略层

- **教程依据**：ch4 "执行工具"（line 182-285）。九项安全机制：输入验证、权限控制、提议者-审核者（事前审批+事后验证）、Sidecar 机制、自动验证与反馈闭环、长输出截断与持久化、沙盒隔离、可观测性、幂等性与取消语义。

- **逐条对照**：

  **1. 输入验证**（路径遍历、命令注入、参数校验、快速失败）
  - 机制层：✅ 拦截点有。`BeforeToolCallHook` + `RuleBasedApprovalHook`（RuleChain + predicates: Contains/NumberRangeField/RateLimit/RegexField）。
  - ⚠️ 工具自身无路径遍历防护——`OsEnv` 直接 `working_dir.join(path)`，无 canonicalize 或 boundary check。`SkillReadTool` 有路径穿越防护（拒绝 `../../../etc/passwd`），但 `ReadTool`/`WriteTool`/`BashTool` 没有。
  - runtime 策略层：❌ 缺失。默认的路径遍历防护规则。

  **2. 权限控制**（工作目录限制、命令黑名单、API 配额/速率限制）
  - 机制层：✅ 已具备。`SandboxConfig`（fs_allowlist/denylist, net_allowlist, ResourceLimits）+ `RuleChain`（RegexField 做命令黑名单）+ `RateLimit` predicate。
  - runtime 策略层：❌ 缺失。默认的危险命令黑名单（`rm -rf /`、`dd if=/dev/zero` 等）。

  **3. 提议者-审核者：事前审批**（独立模型双签）
  - 机制层：✅ 已具备。`BeforeToolCallHook` 是 async，hook 内部可调 LLM 审批。`BeforeToolCallDecision::Allow/Deny/Modify` 三档。教程说的"审批失败后将拒绝理由加入轨迹"= `Deny(ToolFailure)` 返回给 LLM。
  - runtime 策略层：无。是否双签、用哪些模型，是业务决策。

  **4. 提议者-审核者：事后验证**（模态切换验证）
  - 机制层：✅ 拦截点有。`AfterToolCallHook` 可在工具执行后验证结果。
  - runtime 策略层：无。模态切换（渲染后检查、沙盒运行验证）是具体工具的验证策略。

  **5. Sidecar 机制**（与主模型流式输出并行的轻量 LLM 安全审查，只看结构化数据，门控单次工具调用）
  - 机制层：❌ 缺失专用机制。教程描述的 Sidecar 是"与主模型流式输出并行运行，主模型发出工具调用后还在继续生成文本时 Sidecar 已同步开始"。当前 `BeforeToolCallHook` 在工具执行前同步拦截，不是与流式输出并行的。但有两个有利点：(a) `BeforeToolCallHook` 是 async 的，理论上可以 `tokio::spawn` 并行 LLM 调用再 await；(b) `BeforeToolCallCtx` 只包含结构化的 tool_name + args + assistant message，天然满足"只看结构化数据不看主模型自由文本"的输入隔离。缺的是"与流式输出并行"的专用调度——当前 hook 在 LLM 输出完整后才被调用，不是在工具调用 emit 时即触发。
  - runtime 策略层：无。Sidecar 用什么模型、判断什么风险，是业务决策。

  **6. 自动验证与反馈闭环**（写文件后立即跑 linter）
  - 机制层：✅ 已具备。`AfterToolCallHook` 可在工具执行后触发验证（如调 linter），将结果加入返回值。
  - runtime 策略层：无。具体验证什么（Python linter、JS linter）是工具自身行为。

  **7. 长输出截断与持久化**
  - 观点 10 已讨论。不上升为统一策略层。

  **8. 沙盒隔离**（OS 级 / 容器 / microVM / 资源配额）
  - 机制层：✅ 已具备。`Sandbox` trait + `SandboxConfig`（fs_allowlist/denylist, net_allowlist, ResourceLimits: max_cpus/max_memory_mb/max_disk_mb/timeout）+ `OsEnvSandbox`（开发用）+ `sandbox-bwrap`（Linux）/ `sandbox-seatbelt`（macOS）（生产用）。
  - runtime 策略层：无。隔离层级选择是部署决策。

  **9. 可观测性**（日志、审计追踪、性能指标、告警）
  - 机制层：✅ 拦截点有。`AfterToolCallHook` / `AfterProviderResponseHook` / `AfterTurnHook` 可观测工具调用和 LLM 请求。
  - runtime 策略层：❌ 缺失。默认的审计日志格式（每次调用的时间、参数、结果、耗时）、性能指标（调用频率、成功率、平均耗时）、告警机制（频繁失败、超时通知）。

  **10. 幂等性与取消语义**（idempotency key / 先查询后变更 / 预检-确认两段式）
  - 机制层：~~❌ 缺失~~ **不做**。经讨论确认场景不成立：(1) 工具执行是本地同步函数调用，每个 tool_use_id 仅执行一次（`execute_tool_batch` 无重试循环），idempotency key 去重无消费者；(2) 工具失败返回 LLM 决策重试（新 tool call 新 ID），非机械重试；(3) 先查询后变更是工具设计模式，Tool trait 的 execute 已给工具完全控制权；(4) 预检-确认两段式已有等价构建块（ToolProgress channel + EventStream + spawn_agent），拆成两工具或异步工具即可表达；(5) 崩溃恢复（view 26 轨迹完整性修复）注入 synthetic error 而非重执行，不触发重复执行。
  - runtime 策略层：无。

- **缺失总结**：

  **机制层缺失（2 项）**：
  1. Sidecar 专用机制——与流式输出并行的轻量 LLM 门控。`BeforeToolCallHook` 的输入隔离已天然满足，但缺"在工具调用 emit 时即触发、与流式输出并行"的调度。
  2. ~~幂等性机制~~——**不做**（场景不成立，详见上方第 10 条）。

  **runtime 策略层缺失（3 项）**：
  1. 默认的危险命令黑名单（`rm -rf /`、`dd`等）。
  2. 默认的路径遍历防护规则（canonicalize + boundary check）。
  3. 默认的审计日志格式和告警机制。

- **工具归属层**：执行工具（`bash`、`write`、`edit`、`code_interpreter`、`send_email`）→ `llm-harness-runtime-tools` 或用户自定义。`bash`/`write`/`edit` 已在 runtime-tools 中；`code_interpreter`/`send_email` 属用户自定义。

#### 观点 12：协作工具 → 机制层基本完备 + runtime 策略层缺失

- **教程依据**：ch4 "协作工具"（line 287-332）。三组原语：启动与取消（spawn_subagent/cancel_subagent）、消息传递（send_message_to_subagent + 子→主反向）、发现（list_agents 列出 Agent 及其职责描述和运行状态）。HITL：request_human_approval/request_human_input、超时降级、多渠道通知、反馈循环。四种协作形态：同步调用、异步调用、流式协作、多轮交互。

- **逐条对照**：

  **1. 启动与取消**
  - `spawn_subagent` → `SpawnAgentTool`（BuiltIn，async fire-and-forget）。✅
  - `cancel_subagent` → `AbortSubagentTool`（BuiltIn）。✅

  **2. 消息传递**
  - `send_message_to_subagent`（主→子）→ `MessageSubagentTool`（BuiltIn，fire-and-forget）。✅
  - 子→主反向消息 → `MessageMainTool`（BuiltIn）。✅
  - 主等待子回复 → `AwaitSubagentReplyTool`（BuiltIn，阻塞等待）。✅
  - 子等待主消息 → `AwaitMainMessageTool`（BuiltIn，阻塞等待）。✅
  - 四种协作形态：同步调用 = spawn + await；异步调用 = spawn fire-and-forget + await_reply；流式协作和多轮交互 = 双向消息（MessageSubagentTool / MessageMainTool）。✅

  **3. 发现**
  - `list_agents` → `QuerySubagentTool`（BuiltIn，省略 agent_id 时列出全部）。✅ 但只返回 agent_id + status（running/done/aborted），**不返回职责描述**。教程说 list_agents 应列出"Agent 及其职责描述和运行状态"。
  - 机制层缺失：`QuerySubagentTool` 的返回缺少 role/description 字段。`AgentStatus` 枚举只有 Running/Done/Aborted，没有职责信息。

  **4. 子 Agent 配置继承**（观点 3 约束 2 已标记）
  - `SpawnOptions` 有 `ProviderSelection::Inherit`（继承 client + model）和 `cwd: None`（继承工作目录）。✅
  - ❌ 不继承 `system_prompt`——子 agent 从零构建（`HarnessBuilder::new(&model)`）。
  - ❌ 不继承 `tools`——子 agent 的 tools 由 `sub_plugin_factory` 决定。
  - ❌ 不继承 `thinking_level` / 消息前缀。
  - `ContextMode::Inject` 只把上下文文本作为前缀注入 prompt，非消息级对齐——破坏 KV Cache 对齐（观点 3 约束 2）。

  **5. HITL（人工介入）**
  - `request_human_approval`：无 BuiltIn 工具。机制层 `BeforeToolCallHook` async 可实现人工审批（观点 2 已确认——hook 内部 await channel 等待人工结果）。但无开箱即用的工具。
  - `request_human_input`：无 BuiltIn 工具。`steer()` / `follow_up_message()` 可以注入用户消息，但这是 API 层而非 LLM 可调用的工具。
  - 超时降级：教程说"5 分钟内没有响应，采用保守策略"。无通用机制——`BeforeToolCallHook` 内部可实现超时，但无默认行为。
  - 多渠道通知（IM/邮件/Slack）：属用户自定义工具。
  - 反馈循环：教程说"人类批准/拒绝及理由构成带证据的反馈数据"，属第八章持续进化范畴。

- **缺失总结**：

  **机制层缺失（2 项）**：
  1. `QuerySubagentTool` 不返回职责描述——`AgentStatus` 缺少 role/description 字段。教程说 list_agents 应列出"Agent 及其职责描述和运行状态"。
  2. 子 Agent 配置继承（观点 3 已标记）——`SpawnOptions` 不继承 system_prompt / tools / thinking_level，破坏 KV Cache 对齐。

  **runtime 策略层缺失（2 项）**：
  1. HITL 内置工具 + 超时默认行为——`request_human_approval` / `request_human_input` 无 BuiltIn 工具。机制层（`BeforeToolCallHook` async + steer/follow_up channel）可以实现，但没有开箱即用的工具。需要 runtime 策略层提供内置的 HITL 工具和超时降级默认策略（如超时后采用保守策略——Deny 或回退到安全默认值）。
  2. HITL 超时降级默认策略——教程说"5 分钟内没有响应，采用保守策略"，当前无通用实现。

  **用户策略层**：具体超时阈值、多渠道通知工具实现、子 Agent 提示词内容（角色定义、上下文来源标注、任务边界、输出格式）。

- **工具归属层**：协作工具的 7 个内置工具 → runtime 内置（`ToolLevel::BuiltIn`）。HITL 工具（`request_human_approval` / `request_human_input`）→ 应为 runtime 策略层提供的内置工具。多渠道通知（`send_im_notification` / `send_email_notification` / `send_slack_message`）→ 用户自定义。

#### 观点 13：用户沟通工具 → runtime 策略层（修正分类）

- **教程依据**：ch4 "用户沟通工具"（line 386-400）。reply_to_user、send_card_to_user、send_user_notification。当 Agent 与用户的沟通从单一 session 内的一问一答，扩展到多渠道的异步消息时，"说话"本身也需要成为显式的工具调用。教程还提到多渠道召回（IM/短信/邮件/电话/推送，按紧急程度/用户状态/内容性质选择渠道）和异步消息模式（用户不一定在线，已读/未读追踪）。

- **核心问题**：当前 runtime 的 agent→用户通信只有一个出口：`final_answer`（BuiltIn，结束 run 并返回最终答案）。教程说的场景是异步通知——agent 在长时间运行中主动通知用户、用户召回、多渠道触达。`final_answer` 结束 run，不能用于中间通知。

- **修正记录**：原记"机制层缺失：需要 `NotificationChannel` trait（类似 `ExecutionEnv` 抽象文件系统/shell）"。经讨论后修正为纯策略层缺失，理由如下：

  1. **ExecutionEnv 类比不成立**。`ExecutionEnv` 之所以是机制层 trait，是因为机制层自身有大量消费者（read/write/bash/SkillReadTool/compaction 落盘）。`NotificationChannel` 的机制层消费者数量为零——`loop_fn`/`dispatch`/`session` 内部从不发通知。抽象一个机制层自身不依赖的 trait 违反 trait 放机制层的必要条件。
  2. **违反"策略层能做的尽量不放在机制层"原则**（观点 6 起贯彻）。`notify_user` 工具只需实现 `Tool` trait 并注册到 builder——不改 `loop_fn`/`session`/`dispatch` 内部逻辑。符合架构决策的边界判定标准："如果实现只需注册 hook 或实现 trait → 策略层"。
  3. **notify_user 不需要 BuiltIn 级别**。`final_answer` 是 `BuiltIn` 因为 loop 引擎对它有特殊处理（`loop_fn.rs:596-619` 检测 final_answer tool call 并当作终止信号）。`notify_user` 不需要任何 loop 特殊处理——它就是一个普通工具：发消息、返回 `ToolResult { terminate: false }`。没有理由进 `BUILTIN_TOOL_NAMES`。
  4. **trait 放机制层的真实成本**。如果 `NotificationChannel` 放 `crates/llm-harness-types/src/`，所有下游 crate 多一个类型依赖（即使不用通知功能也要编译），trait 定义一旦发布就很难改（跨 crate ABI），与 YAGNI 相悖。
  5. **教程依据再审视**。教程 ch4 line 386-400 说的是"说话需要成为显式工具调用"，强调的是工具化，不是"需要 runtime 提供通道抽象 trait"。教程的 `reply_to_user`/`send_card_to_user`/`send_user_notification` 都是具体工具，不是 trait。

- **修正后三层归属**：

  **机制层**：✅ 不缺失。`Tool` trait（策略层实现 notify_user 工具的接口）+ `ToolContext`（工具执行上下文，已含 `RunExtensions` typed slot 可传递通道实例）+ `BUILTIN_TOOL_NAMES` 保留名机制（notify_user 不需要进）。机制层无需新增任何类型或改任何内部逻辑。

  **runtime 策略层**：❌ 缺失。以下均放 `llm-harness-strategy` crate：
  - `NotificationChannel` trait——抽象通知渠道（`Send + Sync`，`send(&self, msg: NotificationMessage) -> BoxFuture<Result<(), NotificationError>>`），用户实现具体渠道。
  - 内置的 `notify_user` 工具（`ToolLevel::User`，非 BuiltIn）——LLM 可调用，通过 `NotificationChannel` trait 发送消息，不结束 run（`terminate: false`）。
  - 默认行为：未配置渠道时返回成功但标注未投递（`delivered: false`），不终止 run。~~fallback 到 final_answer 或 steer~~（修正：final_answer 的 terminate=true 与 notify_user 的 terminate=false 矛盾；steer 机制 ToolContext 不暴露。当前保守行为是合理工程选择，fallback 通路为可选增强，暂不实现）。
  - 超时降级：通知发送超时后的保守策略。
  - 多渠道选择策略：按紧急程度/用户状态/内容性质选择渠道的默认规则（暂不做，太复杂）。

  **用户策略层**：具体渠道实现（IM/邮件/Slack/推送，实现 `NotificationChannel` trait）、推送内容措辞、用户偏好。

- **工具归属层**：`notify_user` + `NotificationChannel` trait → `llm-harness-strategy` crate（`ToolLevel::User`，非 BuiltIn）。具体渠道实现（`send_im_notification`/`send_email_notification`/`send_slack_message`）→ 用户自定义（实现 `NotificationChannel` trait）。

- **与协作工具的边界**（教程 line 394）：同样是"发通知"，通知对象是审批者或协作者 → 协作工具（观点 12 的 HITL）；通知对象是最终用户本人 → 用户沟通工具（本观点）。区别不在渠道，而在"通知谁、为什么通知"。

#### 观点 14：事件触发工具 → 机制层基本完备 + runtime 策略层部分完成

- **教程依据**：ch4 "事件触发工具"（line 374-384）。三类：定时器（set_timer，一次性+循环）、后台任务监控（monitor_shell，监控长命令新增输出）、外部事件通道（connect_channel，新邮件/API 回调/IM 消息）。两个时刻：注册时 Agent 主动调用工具声明关心什么事件，触发时外部事件异步回调唤醒 Agent。教程还提到事件结构化建模（来源/渠道/内容/上下文四维度）和事件处理三种策略（取消式/队列式/并行式）。

- **逐条对照**：

  **1. 外部事件通道（connect_channel）**
  - 机制层：✅ 已具备。`EventStream` trait（业务层实现，`next()` 阻塞等待）+ `WaitForExternalEventTool`（BuiltIn，LLM 调用后阻塞等待，事件到达后 tool 返回 `ToolResult`，loop 自然续跑）。设计巧妙——无需手动 `resume()`，事件到达 → tool 返回 → loop 进入下一个 turn。

  **2. 定时器（set_timer）**
  - 机制层：✅ 可支撑。`WaitForExternalEventTool` 有 timeout（默认 300s），但语义是"等待超时返回提示文本"而非"定时唤醒"。业务层可用 `EventStream` 实现定时器（`tokio::time::sleep`），但无内置工具。

  **3. 后台任务监控（monitor_shell）**
  - 机制层：✅ 可支撑。`BashTool` 是同步阻塞等待命令完成，无"监控新增输出"能力。但可通过 `EventStream` + 后台 shell 进程实现，业务层自行包装。无内置工具。

  **4. 事件结构化建模**
  - 机制层：✅ 基本具备。`Event` 有 `content: Vec<DataBlock>`（给 LLM）+ `details: Value`（给程序）。教程说的来源/渠道/内容/上下文四维度可通过 `details` JSON 传递，但无结构化模板。

  **5. 运行中消息注入（steer）**
  - 机制层：✅ 已具备。`steer()` / `steer_message()` 在 loop 运行中向上下文追加消息（turn 间 drain），对应教程的"队列式处理"。

  **6. should_stop 后注入（follow_up）**
  - 机制层：✅ 已具备。`follow_up_message()` 在 should_stop 后注入消息触发新 turn。

  **7. 打断（abort）**
  - 机制层：✅ 已具备。`CancellationToken`，loop 中 `select!` 监听，对应教程的"取消式处理"。

  **8. 崩溃恢复**
  - 机制层：✅ 已具备。`TaskStore` 保存 `wait_descriptor`，重启后业务层读出重建 `EventStream`。

- **缺失总结**：

  **机制层**：基本不缺。`EventStream` trait + `WaitForExternalEventTool` + `steer`/`follow_up`/`abort` 覆盖了"注册→等待→触发→处理"的完整链路。比教程描述的"事件队列 + 事件循环"更轻量但功能等价——tool 阻塞等待代替显式事件队列，steer/follow_up 代替队列 drain，abort 代替取消式处理。

  **runtime 策略层**：⚠️ 部分完成。定时器、事件结构化模板、事件过滤已实现；后台监控和外部通道仍缺。
  - ✅ 定时器：`TimerStream`（`strategy/src/event_stream/timer.rs`）——`TimerKind::Once`（到点触发后返回 None）+ `TimerKind::Interval`（按 interval 持续触发，永不返回 None）+ `descriptor()`/`from_descriptor()` 支持 `TaskStore` 崩溃恢复
  - ❌ 后台监控：基于 `BashTool` 后台进程 + stdout 行流的 `ShellMonitorStream`（item 39，未实现）
  - ❌ 外部通道：webhook 接收器的默认实现（item 40，未实现）
  - ✅ 事件结构化模板：`EventDetails` + `EventContext`（`strategy/src/event_stream/template.rs`）——source/channel/event_id/timestamp/context 五字段 JSON schema
  - ✅ 事件过滤规则：`FilterStream`（`strategy/src/event_stream/filter.rs`）——predicate wrapper 静默丢弃不匹配事件，predicate 工厂 `by_source`/`by_channel`/`by_field`/`pass_all`
  - ✅ 事件载荷上下文：`EventDetails` 的 `with_label`/`with_reason`/`with_task_id` builder 提供上下文字段
  - ✅ 心跳守护：`HeartbeatStream`（`strategy/src/event_stream/heartbeat.rs`）——拆 `HeartbeatHandle`（Clone+Send+Sync，`tick()` 重置）+ `HeartbeatStream`（impl EventStream）共享 `Arc<AtomicU64>`，超时未活动触发

  **用户策略层**：具体事件源的接入（邮件 webhook、GitHub PR 事件、IM 消息等）。

- **工具归属层**：`WaitForExternalEventTool` → runtime 内置（`ToolLevel::BuiltIn`）。三类常见 `EventStream` 实现（定时器/后台监控/外部通道）→ runtime 策略层提供的默认实现。具体事件源接入 → 用户自定义。

- **关键观察**：runtime 的事件驱动设计比教程描述的更轻量——用"tool 阻塞等待"代替显式事件队列，用 steer/follow_up 代替队列 drain。教程的"并行式处理"（独立轻量查询另起循环）当前无专用机制，但可通过 `SpawnAgentTool`（观点 12）spawn 一个并行子 agent 实现。

#### 观点 15：通用性设计——grep、glob 与 code_interpreter → llm-harness-runtime-tools 补齐

- **教程依据**：ch4 "工具的通用性设计"（line 64-70）。通用工具优于专用工具，除非存在明确的安全、权限或性能理由。code_interpreter 比十几个专用计算器更省 token、更灵活。但通用性有边界——跨平台 grep 语法差异，专用 grep 工具比让 Agent 自由发挥更好。

- **code_interpreter**：不需要在 runtime-tools 中内置。
  - `bash` 是比 `code_interpreter` 更通用的执行器——`python -c "..."` 或 `python script.py` 就能做 code_interpreter 做的事。
  - 教程说 code_interpreter 的价值是"沙盒环境 + 预装库"——沙盒由 `Sandbox` trait + `sandbox-bwrap`/`sandbox-seatbelt` 解决，预装库是部署配置，不是 runtime 的职责。
  - code_interpreter 与 bash 的区别在于"隔离级别"，但 runtime 的 `ExecutionEnv` + `Sandbox` 已抽象了隔离层级，`bash` 可以在 sandbox 中运行。
  - 结论：`bash` + `Sandbox` = `code_interpreter`，不需要单独内置。如果某个部署需要更严格的 Python 专用沙盒，那是用户自定义工具。

- **grep**：需要补齐。
  - 跨平台 grep 语法差异（macOS BSD grep vs Linux GNU grep），让 Agent 自由发挥容易出错。
  - 当前 runtime-tools 有 `read`/`write`/`edit`/`bash` 但缺 `grep`。
  - 归属：`llm-harness-runtime-tools`（`ToolLevel::User`，可选 crate）。

- **glob（搜索文件名）**：需要补齐。
  - 跨平台 glob 行为差异（shell 展开 vs 程序内匹配），Agent 用 `bash` 的 `find` 命令会遇到平台差异问题。
  - 教程说 glob 工具通过模式匹配快速定位文件（如 `**/*.py`），是感知工具的基础设施。
  - 当前 runtime-tools 缺 `glob`/`find`。
  - 归属：`llm-harness-runtime-tools`（`ToolLevel::User`，可选 crate）。

- **三层归属**：
  - 机制层：✅ 不缺。`Tool` trait + `ExecutionEnv`（提供 `read_text_lines` 等原语）+ `Sandbox` trait。
  - runtime 策略层：无。通用性设计原则是工具作者的决策指南，不是 runtime 要实现的功能。
  - 用户策略层：具体工具的通用 vs 专用选择（三维决策框架：参数复杂度/变更频率/模型能力）。

- **工具归属层**：`grep`/`find` → `llm-harness-runtime-tools` 补齐。`code_interpreter` → 不内置，`bash` + `Sandbox` 替代。

#### 观点 16：工具描述艺术 + 参数保真性 + 工具演进 → 工具实现改进点

- **教程依据**：ch4 三节——"工具描述的艺术"（line 72-86）、"参数传递的保真性"（line 88-98）、"工具设计的演进"（line 100-106）。

- **一、工具描述的艺术**

  教程原则：(1) 描述"什么时候用"而非"能做什么"；(2) 列出边界——"做不到什么"比"能做什么"更重要；(3) 参数用具体例子；(4) 返回值描述清楚；(5) 附带 1-5 个真实调用示例（准确率从 72%→90%）。

  逐个工具检查：

  | 工具 | description | 问题 |
  |------|------------|------|
  | `bash` | "Execute a shell command and return its merged stdout/stderr output." | ❌ 只说"能做什么"，无"什么时候用"、无边界、无示例 |
  | `read` | "Read a file (with hashline anchoring when a snapshot store is attached) or list a directory." | ❌ 暴露实现细节（hashline anchoring/snapshot store）对 LLM 无意义，无"什么时候用"、无边界、无示例 |
  | `write` | "Overwrite a file with the given content, creating parent directories as needed." | ❌ 无边界（不说"会覆盖已有文件"）、无示例 |
  | `edit` | "Apply line-level swap/insert/delete operations to a file, with hashline tag verification." | ❌ 暴露实现细节（hashline tag verification）、无示例 |
  | `spawn_agent` | "派发一个 sub-agent 处理子任务..." | ⚠️ 较好——说了"什么时候用"和后续通信方式，但无示例 |
  | `skill_read` | （见 skills.rs） | ❌ 无边界、无示例 |
  | `wait_for_external_event` | "阻塞等待一个外部事件..." | ❌ 无边界、无示例 |

  参数描述检查：
  - `bash` `command`: "The shell command to execute." — ❌ 无示例
  - `read` `offset`: "1-indexed starting line..." — ⚠️ 有范围说明但无示例
  - `edit` `operations` — ⚠️ schema 详尽但无完整调用示例
  - `spawn_agent` `prompt`: "给 sub-agent 的任务指令" — ❌ 无示例

  **改进点**：
  1. 所有工具的 description 需补充"什么时候用"和"做不到什么"（边界条件）。
  2. 所有工具的参数描述需补充具体例子（如 `bash` 的 `command` 参数应附 `"ls -la"` 或 `"git status"` 示例）。
  3. 所有工具需附带 1-5 个真实调用示例（教程说准确率从 72%→90%）。
  4. `read`/`edit` 的 description 需移除实现细节（hashline/snapshot store），这些对 LLM 无意义。

- **二、参数传递的保真性**

  教程原则：模型感知的世界与工具操作的世界之间不能有系统性偏差。不得静默修改输入/输出。

  逐个工具检查：
  - `bash`：✅ 无静默转换。命令原样传递给 `execute_shell`。
  - `read`：✅ 文件内容原样返回。`replace('\\', '/')` 只用于 display path header，不影响文件内容。
  - `write`：✅ 内容原样写入。
  - `edit`：❌ **保真性问题**。`edit.rs:63-67` 说"file is always rewritten as joined lines plus a single trailing `\n`"。如果原文件没有 trailing newline，edit 会引入一个。模型读取文件看到的内容（无 trailing newline）与 edit 后的文件内容（有 trailing newline）有系统性偏差——正是教程说的"静默修改"。
  - `SkillReadTool`：✅ 内容原样返回，只在截断时标注。`parse_frontmatter` 的 `replace("\r\n", "\n")` 是 frontmatter 解析用的，不影响 body。
  - `format_skill_markup`：✅ `escape_skill_markup` 和 `escape_xml_attr` 是安全转义（防 XML 注入），不是静默修改——只影响注入到上下文的格式，不影响 Skill 原始内容。

  **改进点**：
  5. `edit` 工具静默添加 trailing newline——应在工具描述中明确说明此行为，或改为保留原文件的 trailing newline 状态。

- **三、工具设计的演进**

  教程说第三代优化的三个方向：
  1. 工具如何被准确调用——示例驱动（前文已讨论，当前缺失，改进点 3 覆盖）
  2. 工具如何被发现——动态工具发现（留到"主动工具发现"一节讨论）
  3. 工具如何被串联——代码编排执行（第五章话题，本章不讨论）

  **改进点**：无新增（示例驱动已被改进点 3 覆盖，动态发现和代码编排留到后续章节）。

- **改进点汇总**：

  | # | 改进点 | 归属 | 影响工具 |
  |---|--------|------|---------|
  | 1 | description 补充"什么时候用"和边界条件 | 工具实现 | bash/read/write/edit/spawn_agent/skill_read/wait_for_external_event |
  | 2 | 参数描述补充具体例子 | 工具实现 | bash/read/edit/spawn_agent |
  | 3 | 附带 1-5 个调用示例 | 工具实现 | 所有工具 |
  | 4 | description 移除实现细节 | 工具实现 | read/edit |
  | 5 | edit 静默添加 trailing newline | 工具实现 | edit |

  全部为 `llm-harness-runtime-tools` 和 runtime 内置工具的工具实现层面改进，不涉及机制层或策略层新增。

#### 观点 17：工具生态——MCP 与工具选择的挑战 → 机制层基本完备+runtime 策略层缺失

- **教程依据**：ch4 "工具生态：MCP 与工具选择的挑战"（line 108-148）。MCP 协议带来三方面挑战：(1) 协议本身——标准化工具描述格式、传输层灵活性、资源与工具分离、提示模板；(2) 上下文开销管理——5 个 MCP 服务器可能引入 55K token，需要渐进式披露；(3) 信任模型与安全风险——工具描述投毒、恶意/被劫持服务器、同名工具遮蔽、凭证管理。

- **一、MCP 协议适配**

  逐项检查 `llm-harness-runtime-mcp`：

  | 教程要求 | runtime 实现 | 状态 |
  |---------|------------|------|
  | 标准化工具描述格式 | `McpToolAdapter` 将 `tool_def.description` + `input_schema` 适配为 `Tool` trait | ✅ |
  | 传输层灵活性（stdio/HTTP/SSE） | `McpServerConfig` 支持 Stdio / HttpOrSse；transport/ 有 stdio.rs / http.rs / sse.rs | ✅ |
  | 资源与工具分离 | `McpServerCapabilities` 有 tools / resources / prompts 三类原语；`McpResourceInjectionHook` 按策略注入资源 | ✅ |
  | 提示模板 | `McpPromptRegistry` 发现和调用 MCP prompts，适配为 `PromptSource` trait | ✅ |
  | 工具定义缓存 | `McpToolCache` 缓存到磁盘，启动时先暴露缓存的 deferred 工具 | ✅ |

  **结论**：MCP 协议适配机制层完备。

- **二、上下文开销管理**

  教程说 5 个 MCP 服务器可能引入 55K token，需要渐进式披露（progressive disclosure）。Cursor 将工具描述同步到文件夹，Agent 默认只看到工具名索引，需要时再查询完整定义。

  逐项检查：

  | 教程要求 | runtime 实现 | 状态 |
  |---------|------------|------|
  | 启动时工具暴露 | `ExposedTool.deferred: bool`——启动超时时用缓存工具标记为 deferred，不阻塞启动 | ✅ |
  | 工具名命名空间隔离 | `sanitize_name` 加 `mcp__{server}_{tool}` 前缀 | ✅ |
  | 工具搜索工具（按需加载 schema） | 无——所有 MCP 工具的完整 schema 全量注入上下文 | ❌ 缺失 |
  | 工具层次化组织 | 无——工具是扁平列表，未按信息源性质分类（搜索/读取/解析/查询） | ❌ 缺失 |

  **改进点 1**：工具搜索工具（`discover_tools`）——MCP 工具按需加载，不全量注入 schema。归属 runtime 策略层。

  **改进点 2**：工具层次化组织——按信息源性质分类。归属 runtime 策略层。

- **三、信任模型与安全风险**

  教程说四类风险：工具描述投毒、恶意/被劫持服务器、同名工具遮蔽、凭证管理。

  逐项检查：

  | 教程要求 | runtime 实现 | 状态 |
  |---------|------------|------|
  | 同名工具遮蔽防护 | `sanitize_name` 加 `mcp__{server}_` 前缀 | ✅ |
  | 恶意重定向防护 | SSE transport 有 `validate_same_origin` | ✅ |
  | 资源注入限制 | `McpResourceInjectionPolicy` 有 `strict` 模式 + `max_resources` 限制 | ✅ |
  | 重连保护 | 熔断器（`RECONNECT_WINDOW`=30s + `MAX_RECONNECTS_IN_WINDOW`=5） | ✅ |
  | 工具描述审查（防投毒） | 无——`McpToolAdapter` 直接将 `tool_def.description` 传给 LLM，无审查/过滤 | ❌ 缺失 |
  | 服务器版本锁定 | 无——不锁定服务器版本，可能静默更新 | ❌ 缺失 |
  | 最小权限凭证管理 | 无——`McpServerConfig` 只有 `headers` 和 `env`，无凭证作用域管理 | ❌ 缺失 |

  **改进点 3**：工具描述审查机制——把 description 当不可信输入审计。归属 runtime 策略层。

  **改进点 4**：服务器版本锁定——拒绝静默更新。归属机制层。

  **改进点 5**：最小权限凭证管理——凭证作用域。归属机制层。

- **改进点汇总**：

  | # | 改进点 | 归属 | 教程依据 |
  |---|--------|------|---------|
  | 1 | 工具搜索工具（discover_tools）——按需加载 schema | runtime 策略层 | line 126-128 |
  | 2 | 工具层次化组织——按信息源性质分类 | runtime 策略层 | line 133-139 |
  | 3 | 工具描述审查机制——description 当不可信输入 | runtime 策略层 | line 144-148 |
  | 4 | 服务器版本锁定——拒绝静默更新 | 机制层 | line 148 |
  | 5 | 最小权限凭证管理——凭证作用域 | 机制层 | line 148 |

  MCP 协议适配机制层完备；上下文开销管理和信任模型各有缺失。机制层缺 2 项（版本锁定、凭证管理），runtime 策略层缺 3 项（工具搜索、层次化组织、描述审查）。

#### 观点 18：长输出的截断与持久化 → 机制层部分完备+runtime 策略层缺失

- **教程依据**：ch4 "长输出的截断与持久化"（line 242-249）。执行工具常常会产生复杂冗长的输出。当检测到输出超过阈值（如 200 行或 10000 个字符）时，工具只将头尾各若干行返回到上下文中，完整的结果则保存到临时文件：头部保留前 50 行，尾部保留后 50 行，中间提示省略了多少行及完整输出文件路径，引导 Agent 用 read_file 工具读取。

- **一、截断机制**

  逐项检查 runtime：

  | 教程要素 | runtime 实现 | 状态 |
  |---------|------------|------|
  | 截断策略 head/tail/head_tail | `TruncationStrategy` enum（Head/Tail/HeadTail），`truncate_utf8()` 在 workflow `ShellExecutor` 中实现 | ✅ 机制已有（仅 workflow executor） |
  | 头尾保留+中间标注 | `HeadTail` 策略保留两端 + `... [truncated N bytes] ...` 标记 | ✅ 机制已有 |
  | ReadTool 截断 | `DEFAULT_MAX_LINES=300` + offset/limit 分页 + 截断标注 `... (truncated)` | ✅ 已有 |
  | BashTool 截断 | 无——`BashTool` 直接返回 `stdout`+`stderr` 全量，无任何截断 | ❌ 缺失 |

  **关键发现**：workflow `ShellExecutor` 已实现完善的 head_tail 截断，但 Agent 直接调用的 `BashTool` 完全不截断。机制已有，但未应用到 Agent 工具上。

  **改进点 1**：BashTool 需要截断策略——复用 `TruncationStrategy`，默认 `HeadTail`。归属 runtime 策略层（工具实现）。

- **二、截断后完整输出持久化到临时文件**

  教程核心思想：截断后的完整输出不丢弃，而是持久化到临时文件让 Agent 按需读取。

  逐项检查 runtime：

  | 教程要素 | runtime 实现 | 状态 |
  |---------|------------|------|
  | 完整输出保存到临时文件 | 无——`ToolSessionProjection::Projected` 可替换 `model_content` 为截断版，但原始内容丢弃 | ❌ 缺失 |
  | 文件路径引导 | 无 | ❌ 缺失 |
  | `PersistedEnvelope` 可扩展 | 有结构化信封机制（namespace+schema_version+value），但无文件路径信封 | ⚠️ 可扩展 |

  当前 `ToolSessionProjection` 的设计是"投影后原始内容丢弃"。教程要求"投影后原始内容持久化到文件"。`Projected` variant 用截断内容替换 `model_content`，但完整输出不保存到任何地方——Agent 无法回溯。

  `Ephemeral` variant 更极端：只存摘要字符串，原始内容完全丢弃。

  `PersistedEnvelope` 有 `namespace`+`schema_version`+`value` 结构，可以扩展一个 `output_file` namespace 存文件路径。但当前没有这个约定。

  **改进点 2**：截断后完整输出保存到临时文件——扩展 `ToolSessionProjection` 或 `PersistedEnvelope` 新增文件路径信封。归属机制层。

  **改进点 3**：截断标注中引导 Agent 用 `read` 工具读取临时文件——如 `... [省略 8523 行，完整输出已保存至 /tmp/execution_output.txt，可用 read 工具读取] ...`。归属 runtime 策略层。

- **改进点汇总**：

  | # | 改进点 | 归属 | 教程依据 |
  |---|--------|------|---------|
  | 1 | BashTool 需要截断策略——复用 TruncationStrategy，默认 HeadTail | runtime 策略层 | line 244-248 |
  | 2 | 截断后完整输出保存到临时文件——ToolSessionProjection 扩展或 PersistedEnvelope 新增文件路径信封 | 机制层 | line 244-249 |
  | 3 | 截断标注中引导 Agent 用 read 工具读取临时文件 | runtime 策略层 | line 249 |

  截断机制本身已有（workflow ShellExecutor），但未应用到 Agent 工具（BashTool）。核心 gap 是"保存到临时文件"——当前投影后原始内容丢弃，教程要求持久化到文件让 Agent 按需读取。

#### 观点 19：子 Agent 设计哲学 + 协作机制（深化） → 机制层+runtime 策略层

- **教程依据**：ch4 "子 Agent 的设计哲学"+"子 Agent 提示词的关键要素"+"Agent 间的协作机制"（line 287-332）。子 Agent 核心价值在于专业化分工。提示词四要素：角色定义清晰、上下文来源明确标注、任务边界明确界定、输出格式标准化。三组协作原语：启动与取消（spawn_subagent/cancel_subagent）、消息传递（send_message_to_subagent + 子→主反向）、发现（list_agents 含职责描述和运行状态）。四种协作形态：同步调用、异步调用、流式协作、多轮交互。

- **与 view 12 的关系**：view 12 已覆盖三组原语和四种协作形态的逐条对照，结论是机制层基本完备（7 个 BuiltIn 工具覆盖全生命周期）。本次深化 view 12 未展开的提示词要素和上下文来源标注。

- **一、协作原语与协作形态**（view 12 已确认，不重复）

  三组原语 ✅：spawn_subagent → SpawnAgentTool、cancel_subagent → AbortSubagentTool、send_message_to_subagent → MessageSubagentTool、子→主 → MessageMainTool、list_agents → QuerySubagentTool（缺职责描述，view 12 已标记）。

  四种协作形态 ✅：同步调用 = spawn + await、异步调用 = spawn fire-and-forget + event channel、流式协作 = 双向消息、多轮交互 = AwaitMainMessageTool + MessageSubagentTool。

- **二、子 Agent 提示词四要素**

  | 教程要素 | runtime 实现 | 状态 |
  |---------|------------|------|
  | 角色定义清晰 | `SpawnAgentTool` 的 `prompt` 参数由 LLM 自由填写，runtime 不强制结构 | 用户策略层 |
  | 上下文来源标注 | `ContextMode::Inject` 格式为 `"Context:\n{ctx}\n\n---\n\n{prompt}"`——无来源标注 | ❌ 缺失 |
  | 任务边界明确 | 同上，由 LLM 自由填写 | 用户策略层 |
  | 输出格式标准化 | `SubAgentResult.final_answer: String`——自由文本，不强制 JSON | 用户策略层（保持自由文本） |

  **角色定义和任务边界**：教程说"开门见山说明'你是专门负责 XXX 的助手 Agent'"和"什么在职责范围内，什么需要转交或上报"。这两项是提示词内容层面，归属用户策略层——runtime 不应强制结构，由调用方在 prompt 中组织。

  **输出格式**：教程说"统一的 JSON 结构降低了主 Agent 的解析负担"。讨论后决定保持自由文本——子 Agent 的 `final_answer` 是自由文本，强制 JSON 过度约束。主 Agent 如需结构化数据，在 prompt 中要求子 Agent 按 JSON 格式输出即可。归属用户策略层。

  **上下文来源标注**：教程说"提示中应该明确区分各个来源：'[FROM_MAIN_AGENT] 是主协调 Agent 给你的任务指令；[FROM_USER] 是用户直接补充的信息；[TOOL_RESULT] 是你调用工具后的返回结果'。这种标注可以防止子 Agent 混淆信息来源，避免提示注入攻击。"

  ~~当前 `ContextMode::Inject(String)` 的格式：~~ **已实现**：`ContextMode::Inject(Vec<ContextBlock>)`，每个 block 带 `ContextSource` 标签（`ParentConversation`/`File`/`UserInstruction`/`Custom`），渲染为 `[Context: {label}]\n{content}\n\n`。

  ~~问题：~~ **已解决**：
  1. ~~`{ctx}` 是无结构大块文本，来源不可区分~~ → 每个 block 独立标注来源
  2. ~~无 `[FROM_MAIN_AGENT]`/`[FROM_USER]`/`[TOOL_RESULT]` 标签~~ → `[Context: Parent Conversation]`/`[Context: User Instruction]` 等标签
  3. ~~攻击者可通过 `ctx` 中的内容注入指令~~ → 来源标签让子 Agent 可区分数据来源

  ~~**改进点 1**：`ContextMode` 需结构化上下文来源标注。~~ ✅ 已实现：`Inject(Vec<ContextBlock>)`，每个 block 带 source 标签。

  ~~**改进点 2**：`ContextMode::Inject` 的注入格式需加来源标签。~~ ✅ 已实现：`render_context_blocks` 按 source 分段标注。

- **三、子 Agent 配置继承**（view 3/12 已标记，不重复）

  `SpawnOptions` 继承情况：✅ provider（`ProviderSelection::Inherit`）、✅ cwd（`None` 继承）、❌ system_prompt（不继承）、❌ tools（不继承）、❌ thinking_level（不继承）、⚠️ context（`Inject` 作为前缀注入，非消息级对齐，破坏 KV Cache 对齐）。view 12 已标记，不重复。

- **改进点汇总**：

  | # | 改进点 | 归属 | 教程依据 | 与 view 12 关系 |
  |---|--------|------|---------|---------------|
  | 1 | ContextMode 需结构化上下文来源标注（Inject(String) → Inject(Vec<ContextBlock>)） | 机制层 | line 296-300 | 新增 |
  | 2 | ContextMode::Inject 注入格式需加来源标签 | runtime 策略层 | line 296-300 | 新增 |

  view 12 已标记的 4 项不重复（QuerySubagentTool 缺 role/description、配置继承缺 system_prompt/tools/thinking_level、HITL 内置工具、HITL 超时降级）。角色定义、任务边界、输出格式保持自由文本由用户策略层决定。

#### 观点 20：HITL 超时与降级策略 → runtime 策略层

- **教程依据**：ch4 "超时和降级策略"（HITL 请求的超时阈值+默认行为+优先级队列）。HITL 请求可能不会立即得到响应，需要设置超时阈值和默认行为："如果 5 分钟内没有响应，采用保守策略"。还需要引入优先级队列："紧急请求通过多渠道通知，普通请求只发邮件"。

- **与 view 12/13 的关系**：view 12 已标记 HITL 内置工具缺失+超时降级默认策略缺失。view 13 已标记 NotificationChannel trait + notify_user 工具缺失（均归 runtime 策略层，不进机制层——见观点 13 修正记录）+多渠道选择策略缺失。本次深化超时参数和降级默认行为的具体机制，并评估优先级队列。

- **一、超时阈值**

  教程说"5 分钟内没有响应"。

  runtime 现状：`BeforeToolCallHook::on_call` 是 `async fn`，hook 实现者可以 `tokio::time::timeout` 包裹等待。但 runtime 不提供超时参数或默认超时——hook 实现者每次都要自己写 timeout 逻辑。

  对比：`AwaitSubagentReplyTool` 有 `timeout` 参数（默认 120s），LLM 可以指定。HITL 工具如果存在，也应带 `timeout` 参数让 LLM 或调用方指定。

  **改进点 1**：HITL 内置工具需带 `timeout` 参数（复用 `AwaitSubagentReplyTool` 的 timeout 模式）。归属 runtime 策略层。view 12 已标记 HITL 工具缺失，本次深化：工具需带 timeout 参数。

- **二、默认行为（降级策略）**

  教程说"采用保守策略"。

  runtime 现状：`BeforeToolCallDecision` 有三态：
  - `Allow`——允许工具按原参数执行
  - `Modify(Value)`——以修改后的参数执行（可用于"安全默认值"降级）
  - `Deny(ToolFailure)`——拒绝执行（最保守策略）

  超时后 hook 可以返回 `Deny`（保守策略）或 `Modify` 为安全参数后 `Allow`。机制可覆盖降级语义。但无"超时→自动 Deny"的默认行为，需 hook 实现者手动处理。

  **改进点 2**：超时降级默认行为——超时自动返回 `Deny`（保守策略），可选配置为 `Modify`（安全默认值）。归属 runtime 策略层。view 12 已标记超时降级缺失，本次深化：默认行为是 Deny，可选配置为 Modify。

- **三、优先级队列**（暂缓）

  教程说"紧急请求通过多渠道通知，普通请求只发邮件"。

  runtime 现状：`BeforeToolCallHook` 和 `steer()`/`follow_up()` 都是无优先级的 FIFO 通道，无优先级队列概念。

  **暂缓**：优先级队列+多渠道通知+选择策略复杂度高，暂不做。view 13 已标记 NotificationChannel trait + notify_user 工具缺失（归 runtime 策略层）和多渠道选择策略缺失，留待后续评估。

- **改进点汇总**：

  | # | 改进点 | 归属 | 与已有 view 关系 |
  |---|--------|------|---------------|
  | 1 | HITL 内置工具需带 timeout 参数 | runtime 策略层 | view 12 深化 |
  | 2 | 超时降级默认行为——超时自动 Deny，可选 Modify | runtime 策略层 | view 12 深化 |
  | 3 | 优先级队列 | — | 暂缓（太复杂） |
  | 4 | 多渠道通知+选择策略 | — | 暂缓（view 13 已标记，太复杂） |

  机制层（BeforeToolCallHook async + Allow/Modify/Deny）可拼装 HITL 超时降级，但无开箱即用的内置工具和默认行为。优先级队列和多渠道通知暂不做。

#### 观点 21：事件驱动的异步 Agent → 机制层（设计选择确认）

- **教程依据**：ch4 "事件驱动的异步 Agent"（line 334-370）。教程描述"训练同步/部署异步"的根本矛盾：LLM 训练假设同步（工具调用后下一条消息必须是工具结果），真实部署要求异步（用户随时打断、多任务并发、外部事件随时抵达）。教程提出事件驱动架构：所有输入（用户消息/工具返回/外部回调/定时触发）统一为事件流，通过事件循环驱动 Agent。还介绍 OpenClaw 的三种自动化机制（Hooks/Cron/Heartbeat）和 PineClaw 的 Channel 机制。

- **一、runtime 的异步模型**

  runtime 采用"同步 loop + 异步工具"模型，不是事件循环：

  - agent loop 按 turn 推进：LLM 回复 → tool 执行 → 结果回填 → 下一轮 LLM 回复
  - `steer` 在 turn 间 drain（`loop_fn.rs:210-216`，`while let Ok(msg) = rx.try_recv()`），非 mid-turn 中断
  - `follow_up` 在 should_stop 后注入（`loop_fn.rs:718-727`），触发新 turn
  - `abort` 在 `select!` 中监听（`loop_fn.rs:205-208,335-340`），终止 loop
  - `WaitForExternalEventTool` 在 tool 内 `stream.next().await` 阻塞等待事件，事件到达后 tool 返回 loop 续跑

  turn 间可中断：steer drain / follow_up 注入 / abort 终止。
  turn 内不可中断：LLM 流式响应期间和 tool 执行期间，steer 无法注入。**mid-turn steer 明确不做**——这是有意的设计选择，同步 loop 模型更简单、更匹配 LLM 的训练范式。

- **二、教程三种异步能力对照**

  | 教程能力 | runtime 实现 | 状态 |
  |---------|------------|------|
  | 异步执行是常态——长任务不阻塞用户交互 | WaitForExternalEventTool 阻塞当前 turn；steer 在 turn 间 drain | ⚠️ turn 间可交互，turn 内阻塞 |
  | 事件优先级动态判断——取消/排队/并行 | 取消=abort、排队=steer(FIFO)、并行=SpawnAgentTool；无优先级队列 | ⚠️ 三种策略有但无优先级（view 20 已暂缓） |
  | 中断和恢复流畅性 | steer turn 间注入、follow_up should_stop 后注入、abort 终止；mid-turn 不可中断 | ⚠️ turn 间可恢复，mid-turn 明确不做 |

- **三、事件驱动架构要素对照**

  | 教程要素 | runtime 实现 | 状态 |
  |---------|------------|------|
  | 所有输入统一为事件流 | 未统一——四类输入走不同路径 | ❌ 设计选择 |
  | 事件队列 | 无显式队列——steer_rx 是 mpsc channel，EventStream 是 trait，tool result 是同步回填 | ❌ 设计选择 |
  | 事件循环驱动 Agent | agent loop 是 turn 循环，不是事件循环 | ❌ 设计选择 |

  这些不是缺陷，是有意的设计选择。runtime 用"同步 loop + 异步工具"避免事件循环的复杂性，对大多数 LLM Agent 场景足够。

- **四、OpenClaw 三机制对照**

  | 教程机制 | runtime 实现 | 状态 |
  |---------|------------|------|
  | Hooks（事件钩子） | BeforeRunHook/AfterToolCallHook/ShouldStopHook/BeforeProviderRequestHook/AfterProviderResponseHook | ✅ 机制已有 |
  | Cron（定时调度器） | `TimerStream`（`TimerKind::Once` + `TimerKind::Interval`），`strategy/src/event_stream/timer.rs` | ✅ 已完成 |
  | Heartbeat（心跳守护） | `HeartbeatStream`（`HeartbeatHandle` + `HeartbeatStream` 共享 `Arc<AtomicU64>`），`strategy/src/event_stream/heartbeat.rs` | ✅ 已完成 |

- **五、Channel 机制对照**

  教程说 PineClaw 引入 Channel——实时事件通道，关键事件即时推送。

  runtime 的 `EventStream` + `WaitForExternalEventTool` 对应此机制，但有固有限制：事件只在 LLM 主动调用 `WaitForExternalEventTool` 时才能接收，非"事件到达即触发"。如果 LLM 正在做其他事情（执行其他工具或生成回复），事件堆积在 `EventStream` 的 channel 中，直到 LLM 再次调用此 tool 才能消费。

  这是同步 loop 模型的固有限制。替代方案：通过 `SpawnAgentTool` spawn 专门的事件监听子 agent，子 agent 阻塞等待事件，事件到达后通过 `MessageMainTool` 通知主 agent。

- **六、改进点**

  | # | 改进点 | 归属 | 状态 |
  |---|--------|------|------|
  | 1 | mid-turn steer——LLM 流式响应期间或 tool 执行期间注入 steer | 机制层 | **明确不做** |
  | 2 | 外部事件实时推送——事件到达即触发，不依赖 LLM 主动调用 WaitForExternalEventTool | 机制层 | 可通过 SpawnAgentTool 替代，暂不上升为机制层改进 |
  | ~~3~~ | ~~Cron/Heartbeat 内置实现~~ ✅ 已完成 | runtime 策略层 | `TimerStream` + `HeartbeatStream`，`strategy/src/event_stream/` |
  | 4 | 事件优先级队列 | — | view 20 已暂缓 |

  核心结论：runtime 的"同步 loop + 异步工具"模型是有意的设计选择，不追求教程描述的完整事件驱动架构。mid-turn steer 明确不做。外部事件实时推送可通过 SpawnAgentTool 替代。

#### 观点 22：同步模型支持异步打断 + abort 早停与接续 → runtime 策略层+机制层确认

- **教程依据**：ch4 "工程实现：如何让同步模型支持异步打断"（line 372-420）。教程给出五条规则让同步 ReAct 循环容纳异步打断：规则 1（LLM 输出时立即记录 assistant message）、规则 2（工具完成时才记录 tool result）、规则 3（工具执行中打断需占位符）、规则 4（LLM 思考中打断丢弃当前思考）、规则 5（非打断事件队列批处理）。还讨论异步工具接口设计（initiate+事件通知解耦）和队列式处理注意力分散问题。

- **一、五条规则对照**

  | 规则 | runtime 实现 | 状态 |
  |------|------------|------|
  | 规则 1：LLM 输出时立即记录 assistant message | `loop_fn.rs:428-430`——LLM 流式响应结束后 assistant_msg 立即 push 到 ctx.messages | ✅ |
  | 规则 2：工具完成时才记录 tool result | `loop_fn.rs:564-588`——tool batch 完成后才 push ToolResult | ✅ |
  | 规则 3：工具执行中打断需占位符 | 不支持——工具执行期间 steer 不 drain，无占位符 tool result 机制 | ❌ 不做（view 21） |
  | 规则 4：LLM 思考中打断丢弃当前思考 | 不支持——LLM 流式响应期间不 drain steer，abort 只终止 loop | ❌ 不做（view 21） |
  | 规则 5：非打断事件队列批处理 | `loop_fn.rs:210-216`——steer turn 间 `while let Ok(msg) = rx.try_recv()` 批量 drain | ✅ |

  runtime 实现规则 1、2、5（常态同步轨迹+队列式批处理），不实现规则 3、4（占位符打断+丢弃思考）。与 view 21 设计选择一致——"同步 loop+异步工具"模型，mid-turn 不中断。

  教程说"常态下 LLM 看到完美同步轨迹，只在打断时引入占位符"——runtime 更进一步：**从不引入占位符**，所有事件都在 turn 间安全点处理。代价是工具执行期间用户消息必须等待。教程承认"实践中只在真正紧急时才打断"——runtime 的队列式处理正是这个实践的体现。

- **二、异步工具接口设计**

  教程说将"启动"和"完成"解耦：`initiate_phone_call` 立即返回 task ID，完成后通过事件通知。

  runtime：`SpawnAgentTool` 已是此模式——fire-and-forget，立即返回 `agent_id`，完成后通过 `SubAgentEvent::Done` 事件通知。`WaitForExternalEventTool` 也是——阻塞等待事件，事件到达后 tool 返回。✅ 机制已支持。

  教程说"工具名称和描述要传达异步语义"——属 view 16 工具描述改进点，不重复。

- **三、队列式处理注意力分散问题**

  教程说批量事件处理时模型只关注最后一个事件，建议：(1) 提示词层面告知"全面考虑所有信息"；(2) 状态栏标记 `[未处理事件 1/4]`...`[未处理事件 4/4]` + 末尾汇总。

  runtime：steer drain 时（`loop_fn.rs:210-216`）直接 `ctx.messages.push(msg)`，每条 steer 消息作为独立 `AgentMessage::User` 追加。无编号标记，无汇总。

  **改进点 1**：批量 steer 消息编号+汇总——drain 多条 steer 时添加 `[未处理事件 N/M]` 标记和末尾汇总。归属 runtime 策略层。

- **四、abort 早停与接续**

  用户问：当前 runtime agent loop abort 能否支持早停，早停后能否接续？

  **早停**：✅ 支持。
  - `abort()` 方法（`core.rs:846-869`）：先取消 Idle watcher → 级联取消 sub-agent（`AbortCascadeHook`）→ cancel `CancellationToken`。
  - loop 中三处检查点：turn 间（`loop_fn.rs:205`）、retry sleep 的 `select!`（`loop_fn.rs:337`）、tool 内部（`ToolContext.abort` 传入 tool）。
  - abort 后 `drive_loop`（`loop_driver.rs:465-472`）匹配 `AgentError::Aborted` → emit `AgentHarnessEvent::Aborted`（terminal sentinel）→ break。不设置 `error_message`（abort 是信号不是错误）。
  - 清空 `pending_session_writes`——未完成的 turn 消息不持久化。
  - `phase_guard` RAII drop 恢复 `HarnessPhase::Idle`。

  **接续**：✅ 支持。
  - abort 后 phase 回 Idle，调 `prompt()` 或 `continue_run()` 即可从 session 当前状态继续。
  - `build_context()`（`loop_driver.rs:308`）从 session 读取已持久化的消息重建上下文。
  - 已完成 turn 在 `TurnEnd` 时 flush（`loop_driver.rs:444-448`），abort 不影响已持久化的数据。

  **队列状态**：abort 后 steer/follow_up 队列不自动清空。`clear_all_queues()` 可手动清空（Idle 时调用）。abort 期间排队的 steer 消息会在下次 run 的 turn 间 drain 时被消费——可能有意（保留用户 abort 期间发的消息），也可能意外（abort 后应丢弃所有上下文）。取决于使用场景。

  | 能力 | 状态 |
  |------|------|
  | abort 早停 | ✅ CancellationToken 三处检查 |
  | 级联取消 sub-agent | ✅ AbortCascadeHook |
  | abort 后 phase 恢复 Idle | ✅ phase_guard RAII |
  | abort 后 session 完整性 | ✅ 已完成 turn 已 flush，未完成 turn 清空 |
  | abort 后接续运行 | ✅ prompt()/continue_run() 从 session 继续 |
  | abort 后队列清理 | ⚠️ 需手动 clear_all_queues()，不自动清空 |

  abort 早停与接续机制完备，无需改进。

- **改进点汇总**：

  | # | 改进点 | 归属 | 教程依据 |
  |---|--------|------|---------|
  | 1 | 批量 steer 消息编号+汇总——drain 多条 steer 时添加 [未处理事件 N/M] 标记和末尾汇总 | runtime 策略层 | "队列式处理中的注意力分散问题" |

  规则 3、4 不做（view 21 已确认）。异步工具接口机制已支持。abort 早停与接续机制完备。

#### 观点 23：主动工具发现 → runtime 策略层

- **教程依据**：ch4 "主动工具发现"（line 422-460）。三种方法：(1) 检索式预筛选——按语义相似度一次性匹配候选工具；(2) 主动声明（MCP-Zero）——LLM 在思考中声明能力缺口，系统动态匹配注入；(3) 工具搜索工具——system prompt 只保留基础工具+discover_tools 元工具，LLM 主动检索加载。层次化匹配：MCP 工具按服务器分组，先定位服务器再匹配工具。动态加载与 KV Cache：工具 schema 追加到上下文末尾，保持静态前缀稳定，schema 固定在首次注入位置，后续轮次作为普通历史命中缓存。

- **一、现有工具发现方法**

  | 教程方法 | runtime 实现 | 状态 |
  |---------|------------|------|
  | 全量注入 schema | MCP builder（`builder.rs:225-227`）将所有 MCP 工具 schema 全量注入 HarnessBuilder | ✅ 当前模式 |
  | 检索式预筛选 | 无——无嵌入索引，无语义相似度匹配 | ❌ 缺失 |
  | 主动声明（MCP-Zero） | 无——LLM 无法在思考中声明能力缺口 | ❌ 缺失 |
  | 工具搜索工具（discover_tools） | 无——无 BuiltIn discover_tools 元工具 | ❌ view 17 已标记 |

  当前 runtime 是教程说的"传统做法"——全量注入所有工具 schema。对几十个工具足够用，上百个工具时上下文被工具说明书塞满，模型选择精度下降。

- **二、层次化匹配**

  教程说 MCP 工具按服务器分组，匹配分两层：先定位服务器，再在服务器内匹配工具。

  runtime：`McpManager` 有服务器→工具的层次结构（`ExposedTool.server_name`+`full_name`），但无两层语义搜索。`sanitize_name` 加 `mcp__{server}_{tool}` 前缀只是命名空间隔离，不是搜索机制。view 17 改进点 2 已标记。

- **三、动态加载与 KV Cache**

  教程说动态加载工具 schema 到上下文末尾，保持静态前缀稳定，KV Cache 复用。schema 固定在首次注入位置，后续轮次作为普通历史。

  runtime：工具 schema 通过 `build_loop_config` 注入到 LLM 请求的 `tools` 参数（API 层面），不是作为对话历史中的 user message 追加。每轮请求的工具 schema 是当前轮的完整列表——不存在"schema 散落在轨迹各处"的模式。

  `ExposedTool.deferred` 字段只标记工具来自缓存而非实时连接——不控制 schema 注入方式。

  | 教程要求 | runtime 实现 | 状态 |
  |---------|------------|------|
  | 动态加载工具 schema | `set_tools()` 可运行中替换工具列表 | ⚠️ 替换非追加 |
  | schema 追加到上下文末尾 | 无——schema 在 API tools 参数中，非对话历史 | ❌ 缺失 |
  | 静态前缀稳定+KV Cache 复用 | 不适用——工具 schema 不在前缀中 | ❌ 缺失 |
  | schema 固定在首次注入位置 | 不适用——每轮重新发送当前工具列表 | ❌ 缺失 |

- **四、runtime 已有的动态工具能力**

  runtime 不是完全没有动态工具能力：

  1. `set_tools()`（`core.rs:449-452`）：运行中替换整个工具列表，下一轮生效。可在 `BeforeTurnHook` 或 `PrepareNextTurnHook` 中调用。
  2. `set_active_tools()`（`core.rs:484-493`）：运行中控制激活工具子集。
  3. `NextTurnDirective.tools`（`hooks.rs:104`）：`PrepareNextTurnHook` 返回指令替换下一轮工具列表。
  4. `NextTurnDirective.active_tools`（`hooks.hs:106`）：控制下一轮激活工具子集。

  这些机制可实现"按需切换工具集"——例如 `PrepareNextTurnHook` 根据上一轮 LLM 回复判断需要哪些工具，返回 `NextTurnDirective { tools: Some(new_set) }`。但与教程的区别：替换 vs 追加、API 参数 vs 对话历史、无元工具。

- **五、改进点**

  | # | 改进点 | 归属 | 状态 |
  |---|--------|------|------|
  | 1 | discover_tools 元工具——LLM 主动声明能力缺口 | runtime 策略层 | view 17 改进点 1 已标记 |
  | 2 | 工具层次化组织——按服务器分组两层匹配 | runtime 策略层 | view 17 改进点 2 已标记 |
  | 3 | 动态加载工具 schema 追加到上下文末尾（非 API tools 参数替换） | 机制层 | **暂不做**（太复杂） |
  | 4 | schema 固定在首次注入位置，后续轮次作为普通历史 | 机制层 | **暂不做**（太复杂） |

  改进点 1、2 是 view 17 已标记内容的深化。改进点 3、4 暂不做——当前全量注入 API tools 参数模式对几十个工具足够用。

---

## 第五章：Coding Agent 与代码生成

### 用户观点

| # | 教程内容 | 归属层 | 用户理由 | 校验 |
|----|---------|-------|---------|------|
| 24 | Coding Agent 安全增量：命令语义解析+持久记忆跨会话防线 | runtime 策略层 | view 11 标记的是关键字黑名单；教程要求语义解析（更高阶）；持久记忆审查是新增 | ✅ 讨论后达成一致。七个核心工具中 5 个已有（bash/read/write/edit/code_interpreter=bash+Sandbox），grep/glob 待补齐（view 15 已标记）。ExecutionEnv 提供完整文件系统操作（文件系统作为中枢 ✅）。Sessionless 设计由 session 持久化+Sandbox 生命周期管理支撑，机制已完备。安全增量：沙盒隔离+网络出口+文件系统隔离 ✅（Sandbox trait+BwrapSandbox+SandboxConfig）；资源限额 ⚠️（bwrap 对 max_cpus/max_memory_mb/max_disk_mb fail-closed，view 沙盒节已标记）。命令语义解析 ❌——BashTool 无命令解析，BeforeToolCallHook 可拦截但无语义解析，view 11 标记的是"默认危险命令黑名单"（runtime 策略层），教程要求语义解析（更高阶，理解命令参数类型和消费规则，识别嵌套危险操作如 find -exec rm）。持久记忆跨会话防线 ❌——写入 MEMORY.md 的内容无信任审查，恶意指令可潜伏在长期记忆中跨会话生效。忠诚度守则和信任边界下移属用户策略层 |
| 25 | Coding Agent 整体流程+项目指令文件自动注入 | runtime 策略层 | 工作流程是 Agent 行为策略，无需 runtime 支持；项目指令文件自动注入需 BeforeRunHook 默认实现 | ✅ 讨论后达成一致。教程五阶段流程（项目文档化→需求澄清→设计文档→代码实现与测试→文档同步）是 Agent 行为策略，由 LLM 在 ReAct 循环中自主执行——runtime 的 agent loop+工具集已支撑。教程也说"按需裁剪"——正是 runtime 的反应式迭代循环模式。项目指令文件（CLAUDE.md/AGENTS.md/.cursorrules）自动注入：BeforeRunHook 机制已支撑（hook 可读取文件注入 system_prompt），system_prompt 在请求前缀中天然稳定（KV Cache 友好 ✅），但无内置的"自动扫描并注入项目指令文件"默认实现。SOUL.md（Agent 身份）/MEMORY.md（跨会话经验）同理。改进点：BeforeRunHook 的默认实现——自动扫描 CLAUDE.md/AGENTS.md/.cursorrules 并注入上下文 |
| 26 | 故障与错误恢复：四层故障分类+检测+恢复+终止 | 机制层+runtime 策略层 | API 层重试已有（RetryConfig）；工具层部分（幻觉调用→synthetic error，但缺重复调用指纹+连续失败计数）；上下文层部分（压缩失败即终止，无重试）；控制流层缺失（无死循环检测、无死亡螺旋防护）。8 个改进点 | ✅ 讨论后达成一致。API 层：RetryConfig（指数退避+尊重 retry_after）+ is_retryable（RateLimit/Overloaded/Timeout 可重试）+ stream_opts.timeout_ms ✅。工具层：幻觉调用→synthetic ToolFailure("unknown_tool") ✅；参数畸形→ToolFailure ✅；重复调用指纹检测 ❌；连续失败计数 ❌。上下文层：try_auto_compact+CompactionError ✅；压缩失败即终止（无重试，过于激进）⚠️；轨迹完整性自动修复（unmatched tool 生成 synthetic error 防 400，但缺"工具调用缺结果消息"的自动修复）⚠️。控制流层：ShouldStopHook+BudgetControlAdapter（用户自定义停止+预算上限）✅；重复调用指纹检测 ❌；死亡螺旋防护 ❌；全局 max_turns ❌。恢复分级：静默重试 ✅（缺抖动）；工具错误变输入 ✅；输出触顶降级接续 ❌；模型降级 ❌。终止：API 层 max_retries=3 ✅；压缩失败即终止（无熔断阈值）⚠️；死亡螺旋防护 ❌；递归深度计数器 ❌；连续失败→人工升级 ❌ |
| 27 | 并行工具调用、流式执行与级联中止 | 机制层 | 并行执行已有（join_all+ToolExecutionMode）；级联中止不适用（agent loop 并行 batch 无依赖声明）；流式执行缺失但暂不做 | ✅ 讨论后达成一致。流式执行 ❌——loop_fn.rs 先消费整个 stream 到 MessageEnd，工具执行在模型输出完全结束后才开始，无重叠（改进点 1，暂不做）。并行执行 ✅——ToolExecutionMode::Parallel（默认）+ execute_tool_batch 用 join_all 并行执行同组工具 + group_by_sequential 分割子组。runtime 默认 Parallel 比教程的"默认为否，失败安全"更激进，是设计选择。级联中止——execute_tool_batch 每个工具独立执行，一个失败不影响其他（满足"不波及独立调用和父级"），但无"终止依赖该结果的调用"机制；agent loop 的并行 batch 无依赖声明（所有 Parallel 工具同时启动），级联中止不适用 |
| 28 | 实现技巧：上下文管理+环境感知+状态持久化+即时反馈 | 机制层+runtime 策略层 | 按行范围读取✅+行号标注✅+hashline锚定✅；长输出截断 view 18 已标记；环境感知 view 8 已标记；状态持久化❌（无持久终端会话）；即时反馈❌（无写入后自动 lint）但 AfterToolCallHook 可支撑。均暂不做 | ✅ 讨论后达成一致。上下文管理：ReadTool 支持 offset/limit 按行范围读取 ✅（read.rs:148-150, DEFAULT_MAX_LINES=300）；format_output 每行附加行号前缀 ✅（read.rs:350-352）；hashline 锚定 [PATH#TAG] ✅。长输出截断：TruncationStrategy（Head/Tail/HeadTail）存在于 ShellExecutor（workflow）但 BashTool 无截断 ❌——view 18 已标记。环境感知：TransformContextHook/PrepareNextTurnHook 状态栏注入 ✅——view 8 已标记。状态持久化 ❌——OsEnv::execute_shell 每次 sh -c 全新进程，不保留 cwd/env/会话状态；BashTool 通过 opts.cwd/opts.env 显式传递但非会话级持久化。即时反馈 ❌——WriteTool 写入后不运行 linter；AfterToolCallHook::Replace 可追加 linter 结果但无内置实现。改进点 1（持久化终端会话）和改进点 2（写入后自动 lint）均暂不做 |

### 校验记录

#### 观点 24：Coding Agent 安全增量 → runtime 策略层

- **教程依据**：ch5 "Coding Agent 的安全"（line 90-142）。教程在第四章防御体系基础上补充 Coding Agent 特有的三点增量：命令语义解析（Shell 命令组合爆炸使关键字黑名单形同虚设，需语义级理解）、沙盒隔离与网络出口控制（默认断网+白名单代理+文件系统只读挂载+凭证不挂载+资源限额+持久会话与隔离调和）、持久记忆的跨会话防线（写入长期记忆的内容需经信任审查）。还讨论忠诚度守则（系统提示词层面）和信任边界下移（数据层强制执行）。

- **一、七个核心工具**

  | 教程工具 | runtime 实现 | 状态 |
  |---------|------------|------|
  | Code Interpreter | bash+Sandbox 替代 | ✅ view 15 已确认 |
  | Bash Shell | `BashTool` | ✅ |
  | 读文件 | `ReadTool` | ✅ |
  | 写文件 | `WriteTool` | ✅ |
  | 编辑文件 | `EditTool`（hashline 锚定） | ✅ |
  | Glob（搜索文件名） | 无 | ❌ view 15 已标记补齐 |
  | Grep（搜索文件内容） | 无 | ❌ view 15 已标记补齐 |

  7 个工具中 5 个已有，2 个待补齐（grep/glob），view 15 已覆盖。code_interpreter 不需要新增。

- **二、文件系统作为中枢**

  `ExecutionEnv` trait 提供完整的文件系统操作（read/write/append/list_dir/exists/create_dir/remove/create_temp_dir/execute_shell）。`Session`（JSONL 持久化）+ `WriteTool`/`ReadTool` 实现记忆读写。机制完备，无需改进。

- **三、Sessionless 设计**

  runtime 提供 session 持久化（`Session`/`SessionRepo`/`JsonlSessionRepo`）+ `build_context()` 从 session 重建上下文 + `Sandbox` 生命周期管理（start/reset/shutdown）。Sessionless 是应用架构层面的组合，runtime 机制已支撑。不需要新增改进点。

- **四、安全增量对照**

  | 教程安全要求 | runtime 实现 | 状态 |
  |---------|------------|------|
  | 命令语义解析（非关键字黑名单） | 无——BashTool 无命令解析，BeforeToolCallHook 可拦截但无语义解析 | ❌ 缺失 |
  | 沙盒隔离 | Sandbox trait + BwrapSandbox（Linux）+ SeatbeltSandbox（macOS，未实现） | ✅ |
  | 网络出口控制 | SandboxConfig.net_allowlist + bwrap --unshare-all | ✅ |
  | 文件系统隔离范围 | SandboxConfig.fs_allowlist/fs_denylist + cap-std 防路径逃逸 | ✅ |
  | 资源限额 | ResourceLimits（max_cpus/max_memory_mb/max_disk_mb/timeout）——bwrap 对前三个 fail-closed | ⚠️ 沙盒节已标记 |
  | 持久会话与隔离调和 | 无持久终端会话机制 | ❌ 见实现技巧节 |
  | 持久记忆跨会话防线 | 无——MEMORY.md 的写入无信任审查 | ❌ 缺失 |
  | 忠诚度守则 | 无——系统提示词层面 | 用户策略层 |
  | 信任边界下移（数据层强制） | 无——属应用架构层面 | 用户策略层 |

  沙盒隔离、网络出口、文件系统隔离已完备（第四章沙盒节已确认）。资源限额部分缺失（bwrap fail-closed 拒绝不支持的配额）。

  **改进点 1**：命令语义解析——Shell 命令的语义级安全校验。教程说关键字黑名单无法应对 Shell 的组合爆炸（`$(echo rm) -rf /` 绕过 `rm` 禁令），需语义解析理解每个命令的参数类型和消费规则，识别嵌套危险操作（如 `find / -name '*.log' -exec rm {} \;` 通过合法 find 命令嵌入 rm）。view 11 标记的是"默认危险命令黑名单"（runtime 策略层），本改进点深化为语义解析——更高阶但也更复杂。归属 runtime 策略层。

  **改进点 2**：持久记忆跨会话防线——写入 MEMORY.md 的内容需经信任审查。教程说"攻击者可将看似无害的偏见或恶意指令写入 Agent 的长期记忆，跨会话潜伏，在合适的时机再触发"。当前 `WriteTool` 可写任意文件，无内容审查机制。runtime 策略层应提供默认的写入审查策略（如对记忆文件的内容进行提示注入检测）。归属 runtime 策略层。

  忠诚度守则属系统提示词层面（用户策略层）。信任边界下移（Permission-Embedded Data Objects）属应用架构层面，runtime 不涉及。

- **改进点汇总**：

  | # | 改进点 | 归属 | 教程依据 | 与已有 view 关系 |
  |---|--------|------|---------|---------------|
  | 1 | 命令语义解析——Shell 命令语义级安全校验（非关键字黑名单） | runtime 策略层 | line 122 | view 11 深化（黑名单→语义解析） |
  | 2 | 持久记忆跨会话防线——写入 MEMORY.md 的内容需信任审查 | runtime 策略层 | line 108 | 新增 |


#### 观点 25：Coding Agent 整体流程 + 项目指令文件自动注入 → runtime 策略层

- **教程依据**：ch5 "Coding Agent 的整体流程"（line 144-186）。教程描述五阶段工程化流程：项目文档化→任务理解与需求澄清→编写设计文档→代码实现与测试→文档同步与交付。还讨论项目指令文件（CLAUDE.md/AGENTS.md/.cursorrules）——每次会话开始时自动注入上下文，相当于项目级系统提示词，是 KV Cache 友好的稳定前缀。

- **一、工作流程**

  教程五阶段流程（项目文档化→需求澄清→设计文档→代码实现与测试→文档同步）是 Agent 行为策略，不是 runtime 机制。这些阶段由 LLM 在 ReAct 循环中自主执行——Agent 用 ReadTool 读文档、用 BashTool 跑测试、用 WriteTool 写设计文档。runtime 的 agent loop + 工具集已支撑。

  教程也说"现实中更多按反应式迭代循环工作，按需裁剪这套流程"——正是 runtime 的 agent loop 模式。

  教程还提到"何时停止收集信息、开始行动"的阈值是"模型学到的行为策略"，不是 Harness 的来源——Harness 的提示词、工具和预算能放大或抑制它。这进一步确认工作流程属用户策略层。

  **结论**：工作流程无需 runtime 改进。

- **二、项目指令文件自动注入**

  教程说 CLAUDE.md/AGENTS.md/.cursorrules 在"每次会话开始时自动注入上下文，相当于项目级的系统提示词"。与 README 不同，指令文件承载面向 Agent 的行为约定：构建与测试命令、代码风格、明确禁区。从上下文工程角度看，指令文件是最经济的稳定前缀——内容不随任务变化，天然对 KV Cache 友好。

  | 教程要求 | runtime 实现 | 状态 |
  |---------|------------|------|
  | 会话开始时自动注入项目指令文件 | `BeforeRunHook`（`hooks.rs`）可在 run 开始前修改 system_prompt 或注入消息。`HarnessBuilder` 有 `system_prompt()` 设置。但无自动扫描 CLAUDE.md/AGENTS.md 的内置机制 | ⚠️ 机制可支撑，无内置实现 |
  | 指令文件作为稳定前缀（KV Cache 友好） | `system_prompt` 在请求前缀中，天然稳定 | ✅ |
  | SOUL.md（Agent 身份）/ MEMORY.md（跨会话经验） | 无自动注入。可通过 BeforeRunHook 读取文件注入 | ⚠️ 同上 |

  `BeforeRunHook` 机制已支撑项目指令文件注入——hook 可读取 CLAUDE.md/AGENTS.md 并注入到 system_prompt 或上下文。但 runtime 策略层无内置的"自动扫描并注入项目指令文件"默认实现。

  **改进点 1**：自动扫描并注入项目指令文件（CLAUDE.md/AGENTS.md/.cursorrules）——`BeforeRunHook` 的默认实现。归属 runtime 策略层。

- **改进点汇总**：

  | # | 改进点 | 归属 | 教程依据 |
  |---|--------|------|---------|
  | 1 | 自动扫描并注入项目指令文件——BeforeRunHook 默认实现 | runtime 策略层 | line 160 |

#### 观点 26：故障与错误恢复 → 机制层+runtime 策略层

- **教程依据**：ch5 "故障与错误恢复"（line 233-266）。教程系统回答三个问题：生产级 Harness 会遇到哪些故障、如何检测与恢复、何时必须终止。四层故障分类（API/工具/上下文/控制流），检测（分类+计数+活性监控+完整性监控），恢复（分级升级：静默重试→降级接续→暴露用户），终止（每条恢复路径熔断上限+死亡螺旋防护+全局上限+人工升级）。

- **一、四层故障分类**

  | 教程故障层 | runtime 实现 | 状态 |
  |---------|------------|------|
  | API 层：429/过载/超时/断连/截断 | `RetryConfig`（指数退避+尊重 retry_after）+ `is_retryable`（RateLimit/Overloaded/Timeout）+ `stream_opts.timeout_ms` | ✅ |
  | 工具层：幻觉调用/参数畸形/重复错误 | 幻觉调用→synthetic `ToolFailure("unknown_tool")`；参数畸形→`ToolFailure`；重复错误检测 | ⚠️ 部分 |
  | 上下文层：溢出/压缩失败/轨迹损坏 | `try_auto_compact` + `CompactionError`；轨迹损坏无自动修复 | ⚠️ 部分 |
  | 控制流层：死循环/死亡螺旋 | `ShouldStopHook`（用户自定义）；`BudgetControlAdapter`（预算）；无重复调用指纹检测 | ❌ 缺失 |

- **二、检测：分类+计数**

  | 教程要求 | runtime 实现 | 状态 |
  |---------|------------|------|
  | 错误分类（可重试 vs 不可重试） | `is_retryable()` 区分 RateLimit/Overloaded/Timeout（可重试）vs 其他（不可重试） | ✅ |
  | 重复调用指纹（tool+params 指纹） | 无 | ❌ 缺失 |
  | 连续失败计数（per recovery path） | `RetryConfig.max_retries`（仅 API 层）；压缩失败无计数器 | ❌ 缺失 |
  | 活性监控（stream watchdog） | `stream_opts.timeout_ms` 仅覆盖初始连接+总时长；无空闲看门狗（数据流停止检测） | ❌ 缺失 |
  | 完整性监控（轨迹结构修复） | unmatched tool 生成 synthetic error（防 400）；但缺"工具调用缺结果消息"的自动修复 | ⚠️ 部分 |

  `stream_opts.timeout_ms` 是整体超时（`tokio::time::timeout`），覆盖从请求开始到完成的全部时间。但教程要求的是空闲看门狗——连接建立成功后数据流停止的检测，即"水管通着但不出水"。整体超时无法区分"正常的长生成"和"卡死的长等待"。

  unmatched tool 的 synthetic error 处理的是"工具调用有 ID 但无匹配工具名"的情况（`loop_fn.rs:552-562`）。教程说的是更广义的轨迹完整性——如工具调用缺少配对的结果消息（ToolUse block 无对应 ToolResult），这会导致 API 400 或模型困惑。

- **三、恢复：分级升级**

  | 教程要求 | runtime 实现 | 状态 |
  |---------|------------|------|
  | 静默重试+指数退避+抖动 | `RetryConfig`（指数退避）；`delay_for` 尊重 retry_after；无随机抖动 | ⚠️ 缺抖动 |
  | 前台/后台区分 | ✅ 已满足——compaction 是唯一后台 LLM 调用：`compact()` 零次重试（无 RetryConfig 包装），失败时 context 在窗口内则 `Ok(())` 静默吞掉不终止 run，连续失败由熔断器（`compaction_failure_count`/`compaction_max_failures`）独立放弃。改进点 2 完成后即已满足，此条为遗留标记 | ✅ |
  | 降级与接续（输出触顶） | `TruncationContinuationHook`（strategy crate）——`ShouldStopHook` MaxTokens→continue（只读判断）+ `TransformContextHook` 追加续写指令时自增 continuation_count。注：用续写指令替代 max_tokens 提升（`StreamOptions` 无此字段） | ✅ 已完成 |
  | 模型降级 | 无——主模型过载不自动切备用模型 | ❌ 缺失 |
  | 工具错误变输入（不终止会话） | ✅ `ToolFailure` 作为 ToolResult 进入上下文 | ✅ |

  `RetryConfig.delay_for` 的实现：指数退避（`base_delay_ms * 2^attempt`），尊重 `RateLimit.retry_after`/`Overloaded.retry_after`。但无随机抖动（jitter），大量客户端同时重试时可能造成二次拥塞。

  工具错误变输入是 runtime 的核心设计——`ToolFailure` 生成 `model_result_message` 作为 `ToolResult` 推入 `ctx.messages`（`loop_fn.rs:571-587`），模型在下一轮看到结构化错误自行纠正。这完全符合教程"错误处理的边界不是单次请求，而是整个恢复循环"原则。

- **四、终止：熔断+全局上限**

  | 教程要求 | runtime 实现 | 状态 |
  |---------|------------|------|
  | 每条恢复路径熔断上限 | API 层：`max_retries`（默认 3）；压缩层：失败即终止（无重试） | ⚠️ 压缩无重试即终止，过于激进 |
  | 死亡螺旋防护（错误路径禁调 LLM） | `DeathSpiralGuardHook`（strategy crate）——`PrepareNextTurnHook` turn 边界聚合（本 turn 有任意 ToolFailure→increment，全 Ok→reset）+ `ShouldStopHook` 超阈值停止。用 per-turn 聚合替代 per-tool-call（避免并行 batch 非确定性） | ✅ 已完成 |
  | 递归深度计数器 | `DeathSpiralGuardHook` 的 `consecutive_error_turns` per-turn 计数近似——真正的递归深度需机制层跟踪 sub-agent 调用栈 | ⚠️ 近似 |
  | 全局终止（max turns/budget） | `BudgetControlAdapter`（预算上限→ShouldStop）；`MaxTurnsHook`（strategy crate）——`ShouldStopHook` + `PrepareNextTurnHook` 轮数熔断，默认 50 轮 | ✅ 已完成 |
  | 连续失败超阈值→人工升级 | 无 | ❌ 缺失 |

  压缩失败处理（`compaction_ops.rs:196-202`）：`Err(e)` 直接 emit `CompactionEnd { error }` 并返回 `Err(e)`，终止整个 run。教程建议压缩应有独立熔断阈值（如连续 3 次失败才放弃），而非一次失败即终止。

  死亡螺旋场景：Agent 上下文溢出→触发 stop hook→hook 调用 LLM（如自动记忆提取）→再次溢出→再次触发 hook。runtime 的 `ShouldStopHook` 可在 stop 时调用 LLM。`DeathSpiralGuardHook`（strategy crate）通过 `PrepareNextTurnHook` per-turn 聚合检测连续错误轮（本 turn 有任意 ToolFailure→increment），超阈值（默认 5）后 `ShouldStopHook` 停止 loop。注：`AfterRunHook` 不存在于 runtime，用 per-turn 近似；真正的错误路径 LLM 禁用需机制层支持（error-mode flag），当前为策略层约定。

- **改进点汇总**：

  | # | 改进点 | 归属 | 教程依据 |
  |---|--------|------|---------|
  | ~~1~~ | ~~重复调用指纹检测~~ ✅ 已完成——`RepetitionGuardHook`（strategy crate），`BeforeToolCallHook` Deny + `ShouldStopHook`，canonical JSON 指纹，per-fingerprint 累计计数 | runtime 策略层 | line 248 |
  | ~~2~~ | ~~连续失败计数器~~ ✅ 已完成——`FailureCircuitBreakerHook`（strategy crate），`AfterToolCallHook` 计数（阻塞 lock）+ `BeforeToolCallHook` 熔断 Deny + `ShouldStopHook` | runtime 策略层 | line 248 |
  | 3 | 流式空闲看门狗——连接建立后无新输出超时判定为卡死，区别于连接超时 | 机制层 | line 250 |
  | 4 | 轨迹完整性自动修复——工具调用缺配对结果消息时自动修补，而非抛给模型/API | 机制层 | line 250 |
  | 5 | 退避抖动——指数退避叠加随机抖动，避免同步重试二次拥塞 | 机制层 | line 254 |
  | ~~7~~ | ~~死亡螺旋防护~~ ✅ 已完成——`DeathSpiralGuardHook`（strategy crate），`PrepareNextTurnHook` per-turn 聚合 + `ShouldStopHook` 超阈值停止。注：用 per-turn 计数近似递归深度（`AfterRunHook` 不存在于 runtime） | runtime 策略层 | line 264 |
  | ~~8~~ | ~~全局 max_turns 上限~~ ✅ 已完成——`MaxTurnsHook`（strategy crate），`ShouldStopHook` 非工具轮 stop + `PrepareNextTurnHook` 工具轮 Err→break | runtime 策略层 | line 264 |

#### 观点 27：并行工具调用、流式执行与级联中止 → 机制层

- **教程依据**：ch5 "Coding Agent 的实现技巧"——"并行工具调用、流式执行与级联中止"（line 272-278）。教程提三个点：(1) 流式执行——第一个工具调用的参数一经生成完整、通过校验，即可立即开始执行，无需等待模型生成后续工具调用；(2) 并行执行——彼此独立的调用之间并行执行而非排队等待；(3) 级联中止——每个工具声明是否支持并发（默认为否，失败安全），某调用失败时终止同一批并行启动、依赖该结果的其他调用，但不波及独立调用和父级操作。

- **一、流式执行（tool 参数一完整就启动，与后续 tool 生成重叠）**

  runtime 现状：**不做**。`loop_fn.rs:362-385` 先消费完整个 stream 到 `MessageEnd`，拿到完整的 `assistant_msg`，然后在 `loop_fn.rs:434-460` 从完整消息中提取 `ToolUse` blocks，最后调用 `execute_tool_batch`。工具执行在模型输出完全结束后才开始——无重叠。

  这是显著的延迟优化：模型一次输出 3 个工具调用时，第一个工具的执行时间可与后两个工具的生成时间重叠。但实现复杂度高：需要在 stream processing 中增量解析 `ToolUse` blocks（参数 JSON 增量构建），参数完整后立即启动工具执行，同时继续消费 stream。当前 `StreamingState::process` 消费 adapter 事件构建完整消息后才返回。

  **改进点 1**：流式工具执行——stream 中 ToolUse 参数一经完整即启动执行，与后续 tool 生成重叠。归属机制层。**暂不做**（短期内不实现，实现复杂度高）。

- **二、并行执行**

  runtime 现状：**已有**。`ToolExecutionMode::Parallel`（默认）+ `execute_tool_batch` 用 `join_all` 并行执行同组工具。`group_by_sequential` 按 `Sequential` 工具分割子组。✅

  教程说"默认为否，失败安全"，runtime 默认是 `Parallel`——比教程更激进。这是设计选择：agent loop 中的工具调用通常是独立的（读不同文件、跑不同命令），默认并行更合理。

  **无需改进**。

- **三、级联中止（某调用失败时终止依赖它的同批调用，不波及独立调用和父级）**

  runtime 现状：**部分满足**。`execute_tool_batch` 中每个工具独立执行（`join_all`），一个失败不影响其他——这满足"不波及独立调用和父级"。但没有"终止依赖该结果的调用"的级联中止机制。

  实际上 runtime 的并行工具之间没有依赖声明——所有 Parallel 工具同时启动，无法表达"A 依赖 B 的结果"。教程的场景（A 失败时中止依赖 A 的 B，但不影响独立的 C）需要工具间依赖关系，这更像 DAG 工具编排（workflow），不是 agent loop 的并行 batch。runtime 的故障边界控制（一个失败不取消其他）已满足教程核心要求。

  **无需改进**——agent loop 的并行 batch 模型无依赖声明，级联中止不适用。

- **改进点汇总**：

  | # | 改进点 | 归属 | 教程依据 | 状态 |
  |---|--------|------|---------|------|
  | 1 | 流式工具执行——stream 中 ToolUse 参数一经完整即启动执行，与后续 tool 生成重叠 | 机制层 | line 276 | **暂不做**（短期内不实现） |

#### 观点 28：实现技巧——上下文管理+环境感知+状态持久化+即时反馈 → 机制层+runtime 策略层

- **教程依据**：ch5 "Coding Agent 的实现技巧"（line 268-309）。五个实现技巧：(1) 并行与流式（view 27 已覆盖）；(2) 上下文的精细化管理——按行范围读取+行号标注+长输出截断持久化；(3) 环境信息的动态注入——Agent 状态栏注入工作目录/git 分支/最近提交/变更概览（view 8 已覆盖）；(4) 命令执行环境的状态持久化——维护持久化终端会话保留 cwd/env/会话状态；(5) 即时的语法反馈机制——文件写入后自动运行 linter 追加到工具返回值。

- **一、上下文的精细化管理**

  | 教程要求 | runtime 实现 | 状态 |
  |---------|------------|------|
  | 按行号范围读取特定片段 | `ReadTool` 支持 `offset`/`limit` 参数（`read.rs:148-150`），`DEFAULT_MAX_LINES=300` 限制默认读取量，大文件走 `read_file_windowed` 只读窗口+上下文行 | ✅ |
  | 返回内容附加行号标注 | `format_output`（`read.rs:332-358`）每行附加行号前缀（`{line_no}:{line}`），还附加 `[PATH#TAG]` hashline 锚定头 | ✅ |
  | 长输出截断与持久化 | `TruncationStrategy`（Head/Tail/HeadTail）存在于 `ShellExecutor`（workflow executor.rs:1093-1104）但 `BashTool` 无截断；无"保存到临时文件"机制 | ❌ view 18 已标记 |

  按行范围读取和行号标注已完备，甚至比教程要求更好（hashline 锚定）。长输出截断是 view 18 已标记的问题，不重复记录。

- **二、环境信息的动态注入**

  view 8 已覆盖。`TransformContextHook`/`PrepareNextTurnHook` 在每轮推理前注入动态信息到上下文末尾，不破坏 KV Cache。具体注入工作目录/git 分支/最近提交/变更概览是用户策略层。无需新增。

- **三、命令执行环境的状态持久化**

  runtime 现状：**无持久化终端会话**。`OsEnv::execute_shell`（`env.rs:322-367`）每次调用 `tokio::process::Command::new("sh").args(["-c", cmd])`——每次命令都是全新 shell 进程，不保留 `cd`、环境变量、虚拟环境激活等状态。`BashTool` 通过 `opts.cwd` 和 `opts.env` 每次显式传递工作目录和环境变量，但这不是会话级状态持久化。

  | 教程要求 | runtime 实现 | 状态 |
  |---------|------------|------|
  | 持久化终端会话（保留 cwd/env/会话状态） | 无——每次 `sh -c` 全新进程 | ❌ 缺失 |
  | 保留启动隔离终端的能力支持并行任务 | `BashTool` 每次独立 shell 天然支持并行 | ✅（无状态终端） |

  这是 view 24 安全增量中已提到的"持久会话与隔离调和"——当时标记"见实现技巧节"。现在确认是机制层缺失。需要 PTY 或长期 shell 进程管理。**暂不做**。

- **四、即时的语法反馈机制**

  runtime 现状：`WriteTool` 写入文件后直接返回成功/失败，不运行任何 linter。但 `AfterToolCallHook` 机制可以拦截工具结果并追加信息——`AfterToolCallDecision::Replace`（`hooks.rs:176-188`）可替换整个结果，追加 linter 输出。

  | 教程要求 | runtime 实现 | 状态 |
  |---------|------------|------|
  | 写入后自动运行 linter，结果附加到工具返回值 | `WriteTool` 无 linter；`AfterToolCallHook` 机制可支撑（hook 可在 write 后运行 linter 并追加结果） | ⚠️ 机制可支撑，无内置实现 |

  机制层已有（`AfterToolCallHook::Replace`），但 runtime 策略层无内置的"写入后自动 lint"默认实现。类似 view 25 项目指令文件自动注入——机制可支撑，缺默认实现。**暂不做**。

- **改进点汇总**：

  | # | 改进点 | 归属 | 教程依据 | 状态 |
  |---|--------|------|---------|------|
  | 1 | 持久化终端会话——维护长期运行的 shell 进程，保留 cwd/env/会话状态 | 机制层 | line 301-303 | **暂不做** |
  | 2 | 写入后自动 lint——AfterToolCallHook 默认实现，文件写入后自动运行 linter 追加结果 | runtime 策略层 | line 307 | **暂不做** |

---

## 第六章：Agent 的评估

（待读）

---

## 第七章：模型后训练

（待读）

---

## 第八章：Agent 的持续进化

（待读）

---

## 第九章：多模态与实时交互

（待读）

---

## 第十章：多 Agent 协作

（待读）
