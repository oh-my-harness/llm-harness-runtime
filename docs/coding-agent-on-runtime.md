# 基于 llm-harness-runtime 构建 Coding Agent

本文档分析在 runtime v0.2 完成后，业务层可用的功能，以及构建类似 Claude Code 的 coding agent 还需要开发哪些东西。

---

## 1. Runtime v0.2 完成后，业务层能用的功能

### 1.1 核心框架能力

| 维度 | 功能 | 说明 |
|------|------|------|
| **任务管理** | `TaskRunner` | 状态机（Pending→Running→Succeeded/Failed/AwaitingHumanInput），支持检查点恢复、重试、超时 |
| **工具体系** | `ToolRegistry` + `ToolSource` | 统一注册/发现工具，支持 MCP、本地工具混合；per-session 管理平面 + per-turn 激活平面 |
| **沙箱隔离** | `Sandbox` trait | 文件系统白名单、网络隔离、资源限制；三种后端：OsEnvSandbox（开发）/ BwrapSandbox（Linux）/ SeatbeltSandbox（macOS） |
| **可观测性** | `TraceExporter` | OTel span 采集（task→session→turn→llm/tool 四层嵌套）；MetadataOnly / Full 两档捕获 |
| **审计日志** | `AuditSink` | 独立审计日志，支持 hash 链防篡改；JSONL 文件格式 |
| **成本控制** | `PricingProvider` + `BudgetControlAdapter` | 按模型计价、超限自动停止；支持 soft-limit 告警 + hard-limit 停止 |
| **人工控制** | `HumanApprover` + `HumanApprovalWrapper` | 高危操作触发人工审批，超时策略可配（fail-secure 默认 Deny） |
| **认证** | `AuthHook` | 每次 LLM 调用都调用；默认实现：环境变量 + 配置文件 |
| **上下文注入** | `ResourceProvider` | 向 system prompt 尾部注入外部资源，KV cache 友好 |
| **提示模板** | `PromptSource` | MCP prompts → 编译为 core PromptTemplate，统一执行 |
| **Sub-agent** | `SubAgentSpawner` | 在工具调用中生成子 agent，独立工具集 + 资源配额 + 新鲜上下文窗口 |

### 1.2 默认可用组件

Runtime v0.2 包含以下 crate，开箱即用：

| Crate | 提供 | 场景 |
|-------|------|------|
| `llm-harness-runtime-sandbox-os` | 无隔离沙箱 | 本地开发 |
| `llm-harness-runtime-sandbox-bwrap` | Linux bubblewrap 沙箱 | Linux 生产 |
| `llm-harness-runtime-sandbox-seatbelt` | macOS sandbox 沙箱 | macOS 生产 |
| `llm-harness-runtime-mcp` | MCP client 适配 | MCP server 集成 |
| `llm-harness-runtime-trace-otel` | OTel exporter | 可观测性 |
| `llm-harness-runtime-audit-jsonl` | JSONL audit sink | 审计存储 |
| `llm-harness-runtime-auth` | EnvAuthHook + FileAuthHook | 认证 |

### 1.3 Hook 扩展点

业务层可通过 `HarnessHooks` 注入自定义逻辑（11 个 hook 点）：

```rust
// 核心 hook 及推荐注入点
HarnessHooks {
    before_provider_request,      // 采集 LLM 请求开始时刻 → TraceExporter
    after_provider_response,      // 累积成本、检查预算 → BudgetControlAdapter
    before_tool_call,             // 参数验证、人工审批 → HumanApprovalWrapper
    after_tool_call,              // 采集工具执行结果 → TraceExporter + AuditSink
    after_turn,                   // 采集 turn 完成 → TraceExporter
    prepare_next_turn,            // 动态调整活跃工具 → TaskPreparer
    should_stop,                  // 预算超限时阻断 → BudgetControlAdapter
    // ... 其他 4 个
}
```

---

## 2. 构建 Coding Agent 还需要开发的东西

### 2.1 工具集实现（最核心，估 3-4 周）

业务层需要实现以下工具族：

#### 2.1.1 文件操作（4 个）
- `read_file(path)` — 读取文件内容
- `write_file(path, content)` — 完整覆盖文件
- `edit_file(path, old_text, new_text)` — 原位替换
- `list_dir(path)` — 递归列出目录树

**实现考量**：
- 需要利用 `ExecutionEnv` 或 `Sandbox` 获取文件系统访问权
- 考虑大文件（>1MB）的流式读取
- 编码问题处理（UTF-8 vs 二进制）

#### 2.1.2 代码搜索（3 个）
- `grep(pattern, path)` — 正则搜索
- `find(path_pattern, type)` — 按路径模式和类型查找
- `semantic_search(query, language)` — 语义搜索（可选，需要 embedding 模型）

**实现考量**：
- grep 应支持正则和文本搜索两种
- find 应限制返回数量（避免大仓库爆炸）
- semantic_search 需要后端服务或本地模型

#### 2.1.3 Git 操作（5 个）
- `git_diff(path)` — 查看 diff
- `git_status()` — 查看当前状态
- `git_commit(message)` — 提交更改
- `git_log(path, limit)` — 查看历史
- `git_branch(operation)` — 分支操作

**实现考量**：
- 需要在沙箱内执行 git 命令（权限、repo 初始化）
- 提交消息格式检查（可选）
- 冲突检测和处理

#### 2.1.4 测试运行（3 个）
- `run_tests(path, filters)` — 运行单元测试
- `run_test_file(path)` — 运行指定文件的测试
- `coverage_report(language)` — 生成覆盖率报告

**实现考量**：
- 需要知道项目的测试框架（pytest, jest, go test 等）
- 超时控制（长运行的测试不能卡住 agent）
- 失败日志收集和解析

#### 2.1.5 Shell 执行（2 个）
- `bash_run(command)` — 执行 bash 命令
- `python_eval(code)` — 执行 Python 代码片段

**实现考量**：
- 必须在沙箱内执行（防止 LLM 的恶意代码）
- stdout/stderr 分离采集
- 超时和资源限制（CPU、内存、磁盘）

#### 2.1.6 代码质量（2 个，可选）
- `lint(path, language)` — 代码检查
- `format(path, language)` — 代码格式化

**实现考量**：
- 可能需要在沙箱内安装工具链
- 配置文件管理（.eslintrc, .flake8 等）

#### 2.1.7 总结

工具集设计原则：
```
工具数量目标：< 15 个（agent 工具选择错误率与数量正相关）
  注：此处"< 15"是工具的实现总数（管理平面上限），不是每个 session 的激活数。
  每个 session 的激活工具集通过 PrepareNextTurnHook 动态控制，目标 5-12 个
  （见设计文档 §8.2.2）。两个维度相互独立：注册数量多不影响 session 激活数。
风险分层：
  - T0（读取）：read_file, grep, find, git_log, git_status
  - T1（可逆写入）：write_file, edit_file, git_commit
  - T2（外部副作用）：bash_run, run_tests, lint
  - T3（特权操作）：删除文件、部署（本 v0 不做）
```

---

### 2.2 Agent 策略层（2-3 周）

#### 2.2.1 工具选择策略

**a) 阶段过滤（via `PrepareNextTurnHook`）**

```rust
// 伪代码示例
impl PrepareNextTurnHook for AgentPhaseManager {
    fn prepare_next_turn(&self, ctx: &...) -> NextTurnDirective {
        let phase = detect_current_phase(&ctx);
        match phase {
            Phase::Planning => {
                // 只开放读取工具
                active_tools = vec!["read_file", "grep", "find", "git_status"];
            }
            Phase::Implementation => {
                // 开放读 + 写
                active_tools = vec![
                    "read_file", "write_file", "edit_file",
                    "git_diff", "git_status",
                ];
            }
            Phase::Verification => {
                // 开放读 + 测试
                active_tools = vec!["read_file", "run_tests", "coverage_report"];
            }
        }
        return NextTurnDirective {
            tools: Some(active_tools),
            // `..` 展开为 NextTurnDirective 其他字段的默认值（即 passthrough 语义）：
            //   system_prompt: None   → 不修改系统提示
            //   max_tokens: None      → 使用 AgentHarness 配置的默认值
            //   stop_sequences: None  → 不覆盖停止序列
            // 若需修改其他字段，显式赋值即可（如 `system_prompt: Some(...)`）
            ..NextTurnDirective::default()
        };
    }
}
```

**b) 上下文感知工具选择**

根据前几个 turn 的工具调用历史，动态优化工具集：
- 编辑 Python 文件 → 自动加入 `python_eval`, `lint` 工具
- 发现测试失败 → 自动降级回 Phase::Verification
- 工具调用频繁出错 → 向工具集添加诊断工具

**c) 风险分层与门控**

```rust
struct TieredApprover {
    // T0 工具无需审批
    t0_tools: HashSet<String> = {
        "read_file", "grep", "find", "git_log", "git_status"
    },
    // T1 工具可选审批（开发环境省略）
    t1_tools: HashSet<String> = {
        "write_file", "edit_file", "git_commit"
    },
    // T2 工具推荐审批
    t2_tools: HashSet<String> = {
        "bash_run", "run_tests"
    },
    // T3 工具必须审批
    t3_tools: HashSet<String> = {
        // deploy, db_write 等
    },
}

impl HumanApprover for TieredApprover {
    async fn ask(&self, request: &ApprovalRequest) -> Option<ApprovalDecision> {
        let tool_name = &request.tool_name;
        
        // T0 直接放行
        if self.t0_tools.contains(tool_name) {
            return Some(ApprovalDecision::Approve);
        }
        
        // T2/T3 需要人工
        if self.t2_tools.contains(tool_name) || self.t3_tools.contains(tool_name) {
            // 通过 CLI/Web 弹窗询问用户
            return self.query_user(request).await;
        }
        
        // T1 取决于环境
        return Some(ApprovalDecision::Approve);  // 默认放行
    }
}
```

#### 2.2.2 Agent loop + TaskVerifier

**a) 迭代决策逻辑（核心 loop）**

```rust
pub struct CodingAgentTaskVerifier;

impl TaskVerifier for CodingAgentTaskVerifier {
    async fn verify(&self, trace: &TaskTrace) -> TaskVerdict {
        // 检查 1：工作是否进行中？
        let has_edits = trace.checkpoints.iter().any(|cp| {
            cp.payload.get("edited_files").is_some()
        });
        if !has_edits {
            return TaskVerdict::Fail { 
                reason: "no code changes made".into() 
            };
        }
        
        // 检查 2：最终状态是否表示成功？
        match trace.final_state.get("completion_status") {
            Some(Value::String(status)) if status == "complete" => {
                return TaskVerdict::Pass;
            }
            _ => {}
        }
        
        // 检查 3：需要人工判断吗？
        if trace.checkpoints.last().map(|cp| {
            cp.payload.get("requires_manual_review").as_bool().unwrap_or(false)
        }).unwrap_or(false) {
            return TaskVerdict::Indeterminate {
                reason: "needs manual code review".into()
            };
        }
        
        return TaskVerdict::Fail { 
            reason: "unknown failure reason".into() 
        };
    }
}
```

**b) 失败重试策略**

```rust
// TaskRunner 中的重试配置
pub struct RetryPolicy {
    pub max_retries: usize,
    pub base_delay_ms: u64,
    pub backoff_multiplier: f64,
}

// 业务层决定每次失败后的重试策略
impl TaskRunner {
    pub async fn run_with_recovery(&self, task_id: &str) -> Result<()> {
        let mut attempt = 0;
        loop {
            match self.run_task(task_id).await {
                Ok(_) => return Ok(()),
                Err(TaskError::ToolError { ref tool_name, .. }) if attempt < max_retries => {
                    // 不同工具的失败有不同的处理
                    let next_action = match tool_name.as_str() {
                        "run_tests" => {
                            // 测试失败 → 回到编辑阶段
                            return Err(TaskError::TestFailure);
                        }
                        "git_commit" => {
                            // 提交冲突 → 需要人工介入
                            return Err(TaskError::ConflictRequiresHumanIntervention);
                        }
                        _ => {
                            // 其他工具失败 → 重试或调整工具集
                            attempt += 1;
                            continue;
                        }
                    };
                }
                Err(e) => return Err(e),
            }
        }
    }
}
```

#### 2.2.3 多 turn 协调

**a) 文件修改的全局视图**

在 checkpoint 中维护已修改文件列表：

```rust
// 每个 turn 后记录 checkpoint
struct CodeChangeCheckpoint {
    pub turn_index: u32,
    pub modified_files: Vec<String>,
    pub added_files: Vec<String>,
    pub deleted_files: Vec<String>,
    pub test_status: TestResult,
    pub syntax_errors: Vec<String>,
}

// Agent 在每个 turn 前可以查询历史修改
if context.checkpoint_history.has_conflict() {
    // 检测并报告冲突
}
```

**b) 上下文窗口管理**

利用 core 的 compaction 机制：

```rust
// 当上下文变大时，压缩历史 turn
let compaction_trigger = context.tokens_used > context.max_tokens / 2;
if compaction_trigger {
    // core 的 compact() 会自动提取摘要
    // 业务层可在 CompactionHook 中定制行为
    harness.compact_context().await?;
}
```

---

### 2.3 UI 层（1-2 周）

#### 2.3.1 命令行界面

基本框架：

```bash
$ cargo run --release -- "implement a new feature"
[Task #1] Initializing...
  ├─ Setting up sandbox...
  ├─ Loading tools...
  └─ Starting agent session

[Turn 1/50] Planning phase
  📖 Reading repository structure...
  Task description: implement a new feature
  Tools used: read_file, grep
  
[Human approval] bash_run: "npm install"
  Do you approve this operation? [y/n/always]: y

[Turn 2/50] Implementation phase
  ✏️  Modifying src/main.rs
  ✏️  Creating tests/integration.rs
  ...

[Turn 15/50] Verification phase
  ✅ All tests passed (42 tests, 2.3s)
  ✅ Code coverage: 87% → 91%

[Result] ✅ Task completed in 2m 34s
  Cost: $0.23 (1.2M input tokens, 340K output tokens)
  Files modified: 5
  Tests run: 42 (all passed)
  
Summary:
  - Fixed the issue in src/main.rs
  - Added integration tests
  - Coverage improved from 87% to 91%
```

实现要点：

1. **进度显示** — 每个 turn 的工具调用、token 用量实时显示
2. **人工审批 UI** — 高风险工具触发交互式确认（支持 allow-once / allow-always）
3. **结果展示** — 最终摘要 + 工作统计

#### 2.3.2 实现人工审批接口

```rust
pub struct CliHumanApprover {
    stdin: BufReader<Stdin>,
    allow_once_cache: Mutex<HashSet<String>>,
    allow_always: Mutex<HashSet<String>>,
}

#[async_trait]
impl HumanApprover for CliHumanApprover {
    async fn ask(&self, request: &ApprovalRequest) -> Option<ApprovalDecision> {
        let tool_name = &request.tool_name;
        let request_id = &request.request_id;
        
        // 检查缓存
        if self.allow_always.lock().unwrap().contains(tool_name) {
            return Some(ApprovalDecision::Approve);
        }
        
        // 显示待审批请求
        println!("\n[Human approval] {}: {}", tool_name, request.context);
        println!("  Arguments: {}", serde_json::to_string(&request.arguments).unwrap());
        println!("\n  Options: [y]es, [n]o, [a]lways, [v]iew details");
        
        // 等待用户输入（with timeout）
        match tokio::time::timeout(Duration::from_secs(300), self.read_input()).await {
            Ok(Some('y')) => Some(ApprovalDecision::Approve),
            Ok(Some('n')) => Some(ApprovalDecision::Deny {
                reason: "User denied".into()
            }),
            Ok(Some('a')) => {
                self.allow_always.lock().unwrap().insert(tool_name.clone());
                Some(ApprovalDecision::Approve)
            }
            Ok(Some('v')) => {
                // 显示更多上下文
                self.show_details(&request);
                self.ask(request).await  // 递归问一遍
            }
            Ok(_) => self.ask(request).await,  // 重新问
            Err(_) => {
                println!("\n⏱️  Approval timeout, denying...");
                Some(ApprovalDecision::Deny {
                    reason: "Timeout (5 min)".into()
                })
            }
        }
    }
}
```

#### 2.3.3 Web/IDE 集成（可选，4+ 周）

如果需要支持 IDE（VS Code）或 Web 界面：

1. **VS Code extension** — 通过 JSON-RPC 与 agent 通信
2. **Web UI** — 基于 WebSocket 的实时进度流，附文件树视图
3. **编辑器联动** — agent 修改文件时自动更新编辑器视图

---

### 2.4 Hook 和编排（1-2 周）

#### 2.4.1 实现 PrepareNextTurnHook

动态调整活跃工具、制定下一步策略：

```rust
pub struct CodingAgentPhaseManager {
    history: Arc<Mutex<Vec<TurnRecord>>>,
}

#[async_trait]
impl PrepareNextTurnHook for CodingAgentPhaseManager {
    async fn prepare_next_turn(&self, ctx: &...) -> NextTurnDirective {
        let history = self.history.lock().unwrap();
        let phase = self.detect_phase(&history);
        
        let next_tools = match phase {
            Phase::Planning => vec!["read_file", "grep", "git_status"],
            Phase::Implementation => vec![
                "read_file", "write_file", "edit_file",
                "bash_run", "git_diff"
            ],
            Phase::Testing => vec!["run_tests", "read_file"],
            Phase::Cleanup => vec!["git_commit", "git_log"],
        };
        
        NextTurnDirective {
            tools: Some(next_tools),
            system_prompt_update: Some(format!(
                "Current phase: {}. Focus on: {}",
                phase, phase.focus()
            )),
            ..Default::default()
        }
    }
}

#[async_trait]
impl AfterToolCallHook for CodingAgentPhaseManager {
    async fn after_tool_call(&self, info: &ToolCallInfo) {
        let mut history = self.history.lock().unwrap();
        history.push(TurnRecord {
            tool_name: info.tool_name.clone(),
            success: info.result.is_ok(),
            output_tokens: info.output_tokens,
            timestamp: Utc::now(),
        });
        
        // 智能决定何时切换阶段
        if self.should_move_to_testing(&history) {
            // PrepareNextTurnHook 会在下一个 turn 自动切换工具集
        }
    }
}
```

#### 2.4.2 自定义 TaskVerifier

定义什么时候认为任务成功：

```rust
pub struct CodingTaskVerifier;

impl TaskVerifier for CodingTaskVerifier {
    async fn verify(&self, trace: &TaskTrace) -> TaskVerdict {
        // 判断规则链
        
        // Rule 1: 有代码改动吗？
        if !self.has_meaningful_changes(trace) {
            return TaskVerdict::Fail {
                reason: "no code changes made".into()
            };
        }
        
        // Rule 2: 所有测试都通过吗？
        match self.check_test_results(trace).await {
            Ok(true) => {},  // 通过，继续检查
            Ok(false) => return TaskVerdict::Fail {
                reason: "tests did not pass".into()
            },
            Err(e) => return TaskVerdict::Indeterminate {
                reason: format!("could not run tests: {}", e)
            },
        }
        
        // Rule 3: 代码质量是否改进？
        if !self.code_quality_improved(trace).await? {
            // 并非失败，但可能需要人工检查
            return TaskVerdict::Indeterminate {
                reason: "code quality did not improve".into()
            };
        }
        
        return TaskVerdict::Pass;
    }
}
```

---

### 2.5 数据持久化 & 集成（可选）

#### 2.5.1 结果存储

```rust
pub struct SessionStore {
    base_dir: PathBuf,
}

impl SessionStore {
    pub async fn save_session(&self, session_id: &str, trace: &TaskTrace) -> Result<()> {
        // 保存：
        // 1. trace.json — 完整执行轨迹
        // 2. audit.jsonl — 审计日志（来自 AuditSink）
        // 3. span.jsonl — OTel span（来自 TraceExporter）
        // 4. summary.md — 人类可读的总结
        
        let session_dir = self.base_dir.join(session_id);
        tokio::fs::create_dir_all(&session_dir).await?;
        
        tokio::fs::write(
            session_dir.join("trace.json"),
            serde_json::to_string_pretty(trace)?
        ).await?;
        
        Ok(())
    }
}
```

#### 2.5.2 外部平台集成（可选）

```rust
#[async_trait]
pub trait TaskIntegration: Send + Sync {
    async fn on_task_start(&self, task_id: &str, description: &str) -> Result<()>;
    async fn on_task_end(&self, task_id: &str, result: &TaskResult) -> Result<()>;
}

// GitHub integration 示例
pub struct GitHubIntegration {
    client: Octocat,
}

#[async_trait]
impl TaskIntegration for GitHubIntegration {
    async fn on_task_end(&self, task_id: &str, result: &TaskResult) -> Result<()> {
        if let Some(pr_number) = extract_pr_number(task_id) {
            // 向 PR 写入评论，总结工作
            self.client.post_pr_comment(pr_number, &format!(
                "✅ Agent completed the task\n\n{}",
                result.summary
            )).await?;
        }
        Ok(())
    }
}
```

---

## 3. 新增内容工作量估计

### 3.1 工作量矩阵

| 优先级 | 功能 | 类型 | 工作量 | 关键路径 |
|--------|------|------|--------|---------|
| 🔴 必须 | 工具集实现（10+ 工具） | 业务代码 | 3-4 周 | ✅ |
| 🔴 必须 | Agent loop + 阶段管理 | 业务代码 | 2-3 周 | ✅ |
| 🔴 必须 | TaskVerifier 实现 | 业务代码 | 1 周 | ✅ |
| 🔴 必须 | 基础 CLI UI | 业务代码 | 1 周 | ✅ |
| 🟡 强烈推荐 | 人工审批 UI（CliHumanApprover） | 业务代码 | 1-2 周 | ✅ |
| 🟡 强烈推荐 | 动态工具调度（PrepareNextTurnHook） | 业务代码 | 1 周 | ✅ |
| 🟡 强烈推荐 | 失败恢复和重试策略 | 业务代码 | 1 周 | |
| 🟢 可选 | 结果存储和复盘 | 业务代码 | 1 周 | |
| 🟢 可选 | VS Code 插件 | UI 代码 | 3-4 周 | |
| 🟢 可选 | Web IDE 集成 | UI 代码 | 4-6 周 | |
| 🟢 可选 | GitHub/Jira 集成 | 业务代码 | 1-2 周/平台 | |

**关键路径（MVP 交付）**：工具集 + Agent loop + TaskVerifier + CLI UI = 7-8 周

**完整功能（生产就绪）** ：加上人工审批 + 动态调度 + 重试 = 10-12 周

### 3.2 依赖关系

```
工具集（week 1-4）
    ↓
Agent loop + TaskVerifier（week 3-5，并行 1-2 周）
    ↓
CLI UI（week 5-6）
    ↓
人工审批 UI（week 6-7，并行可提前）
    ↓
动态工具调度（week 7-8）
    ↓
完整端到端测试（week 8-9）
```

---

## 4. 与 Runtime 的分工明细

### 4.1 Runtime 负责（已/将完成）

✅ **已有** — v0.2 完成
- 任务状态机和生命周期管理
- 沙箱隔离和权限控制
- Tool registry 和 MCP 适配
- Hook 系统和编排基础设施
- 追踪（OTel）和审计（JSONL）
- 成本聚合和预算控制

🔄 **业务层决定**
- 价格表（`PricingProvider`）— 来自 LLM provider 的官方定价
- 工具的"安全性标注"（T0/T1/T2/T3）— 业务流程决策
- Human approval 的 UI 实现 — 依赖交互方式

### 4.2 业务层负责

💼 **核心业务逻辑**
- 工具集的定义和实现（什么工具，怎么调用）
- Agent 策略（什么时候用什么工具）
- 任务完成标准（TaskVerifier）
- 失败恢复策略（重试、降级、报警）

🎨 **用户界面**
- CLI / Web / IDE 的具体 UI 实现
- 进度显示和交互反馈
- 审批流程的前端

📊 **可观测性定制**
- 哪些 span 需要采集（MetadataOnly 还是 Full）
- 敏感信息的脱敏规则（`SpanRedactor`）
- 审计日志的长期存储和查询

### 4.3 集成示意图

```
┌─────────────────────────────────────────────────┐
│         Coding Agent Application                │
│  (CLI/Web/IDE, tool impls, hook adapters)       │
└────────┬──────────────────────────────┬─────────┘
         │                              │
         ├─ 注入工具集                  ├─ 实现 Hook
         │  tools: [read, write, test]  │  - PrepareNextTurnHook
         │                              │  - AfterToolCallHook
         ├─ 实现 HumanApprover          └─ 定制 TaskVerifier
         │  (CLI or Web callback)
         │
┌────────v──────────────────────────────v─────────┐
│    llm-harness-runtime (v0.2 platform layer)    │
│                                                  │
│  ┌─────────────────────────────────────────┐   │
│  │ Core trait 定义                         │   │
│  │ - Sandbox, ToolRegistry, TaskRunner     │   │
│  │ - TraceExporter, AuditSink              │   │
│  │ - HumanApprover, etc.                   │   │
│  └─────────────────────────────────────────┘   │
│                                                  │
│  ┌─────────────────────────────────────────┐   │
│  │ 默认实现                                │   │
│  │ - OsEnvSandbox/BwrapSandbox             │   │
│  │ - OTel exporter, JSONL audit sink       │   │
│  │ - EnvAuthHook, MCP adapter              │   │
│  └─────────────────────────────────────────┘   │
│                                                  │
│  ┌─────────────────────────────────────────┐   │
│  │ 编排核心                                │   │
│  │ - TaskRunner + checkpoint               │   │
│  │ - BudgetControlAdapter                  │   │
│  │ - HumanApprovalWrapper                  │   │
│  │ - TracingHookAdapter                    │   │
│  └─────────────────────────────────────────┘   │
└──────────────────────────────────────────────────┘
         ↓
    llm-harness-core
         ↓
    llm_adapter (Provider abstraction)
```

---

## 5. 参考：Claude Code 对标

Claude Code 的核心功能在 runtime 视角下的映射：

| Claude Code 功能 | 实现位置 | 负责方 |
|-----------------|--------|--------|
| 代码编辑、测试、git | 工具集 | 业务层 |
| Coding agent 的智能 | Hook + TaskVerifier | 业务层 |
| 用户交互（CLI/Web） | UI 层 | 业务层 |
| 上下文管理、compaction | core（已有）| Runtime |
| 人工审批流程 | HumanApprover impl | 业务层 |
| Token 计费和预算 | PricingProvider + BudgetControlAdapter | 业务层 + Runtime |
| 追踪和审计 | TraceExporter + AuditSink | Runtime（机制）+ 业务层（策略） |
| 任务重试和容错 | TaskRunner + 重试策略 | Runtime（机制）+ 业务层（策略） |

---

## 6. 推荐的实现顺序

### Phase 1（第 1-2 周）— 最小可运行

```
1. 实现 5 个核心工具：read_file, write_file, bash_run, run_tests, git_status
2. 简单的 TaskVerifier（检查：有改动 + 测试通过）
3. 最小 CLI UI（显示进度和最终结果）
4. 不需要人工审批（直接运行）
```

**交付物** — 能跑一个端到端的 task（create file + edit + test）

### Phase 2（第 3-4 周）— 添加工具集和阶段管理

```
1. 添加 6 个补充工具：grep, find, edit_file, git_diff, git_commit, lint
2. 实现 PrepareNextTurnHook（阶段检测 + 工具切换）
3. 改进 TaskVerifier（更复杂的成功条件）
4. CLI 显示当前阶段和活跃工具集
```

**交付物** — 支持多阶段任务（planning → implementation → testing）

### Phase 3（第 5-6 周）— 人工控制和可观测性

```
1. 实现 CliHumanApprover（人工审批 UI）
2. 工具分层标注（T0/T1/T2/T3）
3. 启用 TraceExporter（MetadataOnly）和 AuditSink
4. 添加结果存储（summary + 工作统计）
```

**交付物** — 支持人工审批和完整审计日志

### Phase 4（第 7-8 周）— 高级功能

```
1. 失败恢复和重试策略
2. 成本聚合和预算控制
3. 启用 TracingHookAdapter（Full 采集）+ 脱敏
4. 集成测试矩阵覆盖
```

**交付物** — 生产就绪的 coding agent

---

## 7. 常见集成问题 Q&A

**Q: 工具在沙箱中如何访问文件系统？**

A: 通过 `ExecutionEnv` 接口。业务层在实现工具时，接收 `ExecutionEnv` 参数，通过它调用 `read()`, `write()` 等方法。Sandbox 的配置（`fs_allowlist` 等）会约束这些操作的权限范围。

```rust
pub async fn read_file_tool(
    path: &str,
    env: Arc<dyn ExecutionEnv>,
) -> Result<String> {
    let content = env.read_file(path).await?;
    Ok(content)
}
```

**Q: 如何自定义成本计费？**

A: 实现 `PricingProvider` trait：

```rust
pub struct MyPricingProvider;

impl PricingProvider for MyPricingProvider {
    fn price_for(&self, model: &str, provider: &str) -> Option<TokenPrice> {
        match (model, provider) {
            ("claude-3-sonnet", "anthropic") => Some(TokenPrice {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cache_read_per_mtok: 0.3,
                cache_write_per_mtok: 3.75,
            }),
            _ => None,
        }
    }
}
```

传给 `BudgetControlAdapter::new()` 即可。

**Q: 如何处理长任务的上下文窗口爆炸？**

A: 利用 core 的 `compact()` 方法。业务层可在 `PrepareNextTurnHook` 中检查上下文大小，超过阈值时触发 compaction。

```rust
if harness.current_context_tokens() > MAX_TOKENS / 2 {
    harness.compact_context().await?;
}
```

**Q: 子 agent (SubAgentSpawner) 什么时候用？**

A: 当工具需要启动一个完全独立的、有新鲜上下文窗口的子 agent。示例：

- 主 agent 在编辑代码时，需要让子 agent 独立分析一个大文件
- 子 agent 有专用的工具集（如文档搜索）
- 子 agent 的结果（1-2K 摘要）返回给主 agent 决策

v0.2 阶段，业务层可选在 Tool handler 中实现 SubAgentSpawner，并存入 `ToolContext` 供调用。

**Q: 如何在 IDE（VS Code）中实时显示 agent 进度？**

A: 通过 WebSocket 或 JSON-RPC：

1. Agent 每个 turn 后，发送进度事件到 IDE（turn index, tool calls, token usage）
2. IDE extension 监听事件，更新 UI（进度条、文件编辑、日志）
3. 用户审批时，IDE 显示弹窗，将决策发回 agent

这涉及 IDE extension 开发，但 agent 端的改动很小（只需暴露事件流）。

---

## 8. 总结

Runtime v0.2 是 **execution engine 和 governance 基础设施**，提供：
- ✅ 任务编排和生命周期
- ✅ 工具管理和 MCP 适配
- ✅ 沙箱隔离和权限
- ✅ 可观测性和审计
- ✅ 成本控制和人工门控

业务层需要贡献 **策略和用户体验**：
- 🔨 工具集实现（什么工具，怎么调用）
- 🧠 Agent 策略（什么时候用什么工具）
- 🎨 UI 和交互（怎么展示给用户）
- 🔍 失败恢复（怎么重试和降级）

两层分工清晰，runtime 做**机制**，业务层做**策略**。这样的架构允许多样化的 agent（coding agent, data agent, browser agent）在同一个 runtime 上快速迭代。

