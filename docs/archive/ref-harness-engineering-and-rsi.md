# Harness Engineering and Recursive Self-Improvement

> 参考文档。原文为 Lilian Weng 关于 harness engineering 与 RSI（Recursive Self-Improvement）的研究综述。
> 本文整理自原文逐段内容，保留核心论点、方法描述、实验数据和关键结论，供 runtime 开发参考。
> 相关分析见 [harness-engineering-and-rsi-implications.md](./harness-engineering-and-rsi-implications.md)。

---

## 1. RSI 与 Harness 的定位

### 1.1 RSI 的起源

Recursive Self-Improvement (RSI) 的概念可追溯到 I. J. Good (1965)，他定义了 "ultraintelligent machine"——一个能在所有智力活动中超越人类、并能设计更好机器来改进自身的系统。Yudkowsky (2008) 用 "recursive self-improvement" 描述一个特定反馈循环：AI 用当前智能改进产生自身智能的认知机制。

在现代 AI 中，这一反馈循环可能指模型直接重写自身权重，也可能更广泛地指模型改进 training pipeline 和 deployment system，从而产出在 economically valuable tasks 上表现更好的后继模型。AI 研究发展的速度在前沿实验室（Anthropic、OpenAI）已显著加速。

### 1.2 Deployment System 的重要性

作者明确强调 "deployment system"——raw model 和 real-world context 之间的层与模型原始智能（即 pretraining 后的 evals）同等重要。Harness 是 AI deployment 的重要组成部分，如 Claude Code 和 Codex 等成功的 coding agent 产品所示。

**Harness 定义**：围绕 base model 的系统，负责 orchestrate execution、决定 model 如何 think and plan、call tools and act、perceive and manage context、store artifacts、evaluate results。

### 1.3 文章 Scope

本文聚焦 harness engineering 研究及其对 RSI 的贡献。近期关于 auto-research、self-improving agents、evolutionary program search 的工作都可围绕这一问题组织。self-play、synthetic data、test-time training 和更广泛的 continual learning 主题（如 Yuan et al. 2024, Chen et al. 2024, Zhao et al. 2025, Choi et al. 2026）虽符合 RSI 愿景，但不在本文 scope 内。

---

## 2. Harness 的设计哲学

### 2.1 从 Agent 框架到 Harness

相比早期 agent 框架 "`agent = LLM + memory + tools + planning + action`"，harness engineering 额外包含：

- **workflow design**（loop engineering）
- **evaluation**
- **permission controls**
- **persistent state management**

本质从 prompt templates 转向 runtime / software system design：关注模型如何 observe、act、memorize、check itself、improve。

### 2.2 简单与通用

设计应刻意简单和通用以支持泛化，参考已有软件工程实践以受益于 pretraining knowledge。Harness 与操作系统有强类比：类似 OS，harness 应封装复杂逻辑同时保持接口简单。Configs、tool interfaces 和其他 protocols 可能会逐渐跨行业标准化。

### 2.3 Harness Layer vs Core Intelligence

RSI 近期路径不太可能从模型直接重写权重开始。作者预测的近期实践路径：

1. **Harness engineering 向 meta-methodology 演化**：即改进产出好答案的机制，而非只改进答案本身。Harness 系统自身成为优化目标，更少 heuristic rules，更多 general mechanisms。
2. **成熟 harness 使能 auto-research / self-improvement loop**，更聪明的模型反过来防止 harness 过度工程化，保持系统可持续。
3. **最终许多 harness 改进会被内化到 core model behavior**，但与外部 context/tools 的接口会保留。这与 prompt engineering 的软化版本类似：手工 prompt trick 随 instruction tuning 和 model reasoning 提升而边缘化，但指定目标、约束、上下文和评估的需求不会消失。

### 2.4 优化对象演进链

Harness 系统中优化对象的演进大致为：

```
instruction prompts → structured context → workflow → harness code → optimizer code
```

模型越强，优化目标越复杂、越通用。

---

## 3. 关键设计模式

### 3.1 Workflow as Goal-Oriented Loop

定义一个模型可以在其中 operate、test、iterate 的 workflow 是自动化的关键设计。Karpathy 的 autoresearch repo 是一个简洁范例。通用 workflow 遵循 goal-oriented loop：`plan → execute → observe/test → improve → execute again`，直到目标达成。流程中可能主动向用户请求澄清（任务规格或执行偏好）。

Workflow graph 还强调模型分析自身 trajectory 和 failure case，然后通过 "agent runtime" 而非 static prompt template 迭代改进。

### 3.2 File System as Persistent Memory

长周期 agent 系统中，harness 不应在 context 中携带整个 workflow 和所有 log，而应将 durable state 存文件。长周期 rollout 中，artifacts（实验日志、代码 diff、论文摘要、错误堆栈、历史轨迹）往往远超模型训练时的 context window。

学习读写编辑文件系统（通常通过 bash）是 LLM 的基础能力，因此以文件形式管理持久记忆能自然受益于核心模型能力的提升。

### 3.3 Sub-agent and Backend Jobs

Harness 可 spawn 多个 subagent 并行执行并监控 backend jobs。适用于主 agent 需要搜索多假设、并发跑实验、或委派隔离子任务而不污染主 context。父 agent 需要一个小型 process manager：launch jobs、inspect logs、cancel failed runs、merge results back。

**关键设计选择：并行要 explicit 且 inspectable。** 如果 subagent 输出只存在于 transient chat context，很快会过时和隐藏。如果存为文件、日志和状态记录，模型能在中断后恢复并推理自身执行历史。

### 3.4 Coding Agent Harness 工具栈

主流 coding agent（Claude Code、Codex、OpenCode、Cursor）的核心接口已趋稳定：

| 分组 | 工具 |
|---|---|
| File system | glob, grep, ls, read, read_many, write, edit, multi_edit, apply_patch |
| Shell execution | bash, PowerShell |
| IO | lsp, git_status, git_diff, git_commit |
| External context | MCP tools, Skills |
| Web search | web_search, web_fetch, browser tools |
| Artifacts | read docs/images, generate HTML/images |
| Backend processes | CronCreate, CronDelete, CronList |
| Agent delegation | spawn_agent, resume_agent, wait_agent, list_agents, close_agent, interrupt_agent |

---

## 4. Context Engineering

### 4.1 问题

简单地将所有 tool response 和 model generation append 到 context 中，随 agentic job horizon 增长会迅速失控。Context management 是一层为 LLM 构造更结构化、更精简的 context 并管理持久状态的机制。长上下文研究会持续进步，但长上下文能力和 context engineering 目前交织。

### 4.2 ACE（Agentic Context Engineering; Zhang et al. 2025）

ACE 把 context 视为 **evolving playbook** 而非不断变长的 prompt。维护一个 bullet points 列表，每条有 identifier 和 description。三个组件：

- **Generator**：参考 bullet points 产出任务 trajectory。
- **Reflector**：从成功和失败 trajectory 中提炼 insights，有 iterative refinement 自循环。
- **Curator**：用增量、条目化方式更新结构化 context。

**数据流**：

```
Query + Playbook → Generator → Trajectory
                                        ↓
                               Reflector ←(Iterative Refinement)
                                        ↓
                                   Insights
                                        ↓
Playbook ──(update)──→ Curator ← Insights
                            ↓
                    Delta Context Items
                            ↓
                      Playbook (更新，下一轮复用)
```

**防退化设计**：为防止 iterative rewrite 中的 context collapse 和 brevity bias，Curator 不重写完整 prompt blob，而是输出结构化、条目化的 bullets `(identifier, description)`，用确定性逻辑合并进 context logbook，定期 refine 和 deduplicate。

### 4.3 MCE（Meta Context Engineering; Ye et al. 2026）

ACE 从 rollout 学习 insight 走向 self-managed memory，但 update rules 和 workflow 仍是手工设计的。

MCE 把 "how to manage context"（机制）和 "what is in context"（内容）分离，在 meta-optimization 层做 skill evolution，在 base level 做 context optimization。

**形式化定义**：MCE skill $s \in \mathcal{S}$ 定义 context function $c_s = (\rho_s, F_s)$，把输入 $x$ 映射到 context $c = F_s(x; \rho_s)$：
- $\rho_s = \{\rho_1,\dots,\rho_m\}$：静态组件（prompts、知识库、代码库）。
- $F_s = \{F_1,\dots,F_k\}$：动态算子（搜索、选择、过滤、格式化）。

**Bi-Level Optimization**：

$$\text{Inner}:\; c_s^* = \arg\max_{c_s} J_{\mathrm{train}}(c_s; s) \quad \text{Outer}:\; s^* = \arg\max_{s\in\mathcal{S}} J_{\mathrm{val}}(c_s^*)$$

Skill 数据库跟踪历史 $\mathcal{H}_{k-1} = \{(s_i, c_i, J_i^{\mathrm{train}}, J_i^{\mathrm{val}})\}$。Meta-level agent 做 agentic crossover 产生新 skill：

$$s_k = \mathrm{crossover}(\tau, \mathcal{H}_{k-1})$$

Base-level context engineer 执行 skill 并从 rollout feedback 学习 context function：

$$c_k = \mathrm{engineer}(\tau, s_k, c_{k-1}^*, \mathcal{R}_k)$$

**实现**：context function $F_s$ 实例化为一个目录下的文件集合，包括静态（`skill.md`）和动态（context、data rollouts）组件。两层优化都在标准 coding 环境中执行，工具集 $\mathcal{T} = \{\text{Read, Write, Edit, Bash, Glob, Grep, TodoWrite}\}$。

MCE 不像 ACE 那样用 heuristic 规则结构化 context，而是用 free-form skills 存储任务最重要的知识，并迭代地共同演化 skill 和 skill-conditioned context。

---

## 5. Meta-Harness：优化 Harness 本身

### 5.1 核心思想

Meta-Harness（Lee et al. 2026）把优化对象再深一层：优化决定"什么信息该存、该取、该呈现给模型"的代码本身。"Meta" 意为 harness for optimizing harnesses。

### 5.2 算法

```
输入：任务集 X、LLM M、proposer P、迭代次数 N
初始化：harness 种群 H、文件系统 D（存 code/scores/traces）

第一阶段：评估初始种群
  for each H in H:
    E_H = Evaluate(H, M, X)
    D ∪= {(H, E_H)}

第二阶段：迭代演化
  for t = 1..N:
    Proposer P 查询文件系统 D（用 grep/cat 读历史）
    P 提出 k 个新 harness
    for each new H:
      if H passes interface validation:
        D ∪= {(H, Evaluate(H, M, X))}

输出：D 中的 Pareto frontier
```

**关键设计**：
- Proposer 本身是 coding agent，与被优化的 harness 同构。
- 文件系统是唯一记忆接口——proposer 用 `grep`/`cat` 读历史，不把所有东西塞进一个 prompt。
- 每个 harness 是文件系统里的自包含字典（源代码 + scores + rollout trajectories + state updates）。
- Interface validation 门——新提出的 harness 必须通过接口校验才被评估。
- Pareto frontier 选择——多目标优化。
- 只有合格的 harness 被保留。

### 5.3 核心结论

> Once harness design becomes an executable search space, a strong coding agent can exploit the same design space human engineers use.

---

## 6. Workflow Design 实践

### 6.1 手工设计

Workflow design 可由领域专家手工设计。

**AI Scientist（Lu et al. 2026）**：线性 pipeline——propose research ideas → write code → run experiments → analyze results → write manuscript → peer review。

**ScientistOne（Meng et al. 2026）**：把 verifiability 作为中心设计约束，每个 claim（citation、numerical、methodological、conclusion）必须追溯到 evidence source，由 Chain-of-Evidence checks 审计。

### 6.2 AI Scientist 流水线详解

分为 Ideation → Experimentation → Write-up 三大阶段：

**Ideation**：
1. LLM idea proposal：大模型提出研究想法
2. Novelty checking：核验创新度
3. Scoring and archiving：打分评级并归档

**Experimentation**（4 个串行步骤，每步都做 [Write to log] + [Best] 筛选）：
1. Preliminary investigation：初步可行性验证
2. Hyperparameter tuning：超参调优
3. Research execution：正式实验运行
4. Ablation studies：消融实验

**Write-up**：
1. Plotting and feedback：基于实验结果绘图并接收反馈迭代
2. Paper template：套用论文模板
3. Paper：生成完整论文初稿
4. Paper AI review：AI 评审论文并最终修订

### 6.3 Autodata Agent（Kulikov et al. 2026）

设计为数据科学家 agent，用于生成训练和评估数据。Main Agent 管理四个角色：

- **Challenger**：出题
- **Weak Solver**：弱模型解题
- **Strong Solver**：强模型解题
- **Verifier/Judge**：验证

目标：合成 "just right" 难度的数据——强模型能解，弱模型不能解。Challenger prompt 根据 solver 和 verifier 反馈迭代更新。

**架构**：
```
                    Main Agent
                    (核心调度器，接收 grounding data，输出 training data)
                       │
            ┌──────────┼──────────┐
            ▼          ▼          ▼
      Strong Solver  Weak Solver  Challenger LLM
            │          │          (返回困难样例反哺)
            └────┬─────┘
                 ▼
          Verifier/Judge ──(reward signal)──→ Main Agent
```

**RSI 局限**：合成数据只 fine-tune 弱 solver，不改进强 solver。如果 loop 不能迭代改进强模型，更像在生成 prompt 分布上做间接蒸馏，RSI 味道不足。

---

## 7. 搜索式 Harness 优化

### 7.1 ADAS（Automated Design of Agentic Systems; Hu et al. 2025）

把 agent design 本身建模为优化问题——"meta-agent search"：

1. 初始化 archive 为简单 agent（CoT、self-refine）。
2. Meta-agent 编程新 agent（全用代码），受 archive 中已有方案启发。先生成 high-level description，再实现为代码。
3. 草稿经两轮 self-refine（生成反馈→基于反馈改进），检查 novelty。
4. 评估新候选，成功的加入 archive。
5. 重复直到最大迭代次数。

### 7.2 AFlow（Zhang et al. 2025）

把 agentic workflow 表示为图，节点是 LLM-invoking actions，边是代码实现的逻辑操作。用 MCTS 优化：

1. 初始化模板 workflow 为树根。
2. Select：用 score + uniform exploration 的 soft mixture 选节点。
3. Expand：LLM 基于评估性能修改 workflow。
4. Execute + Evaluate：跑新 workflow 并打分。
5. Add：预算 B 轮内有改进则加入树。
6. 重复，直到 top-K 平均分收敛或预算耗尽。

### 7.3 STOP（Self-Taught Optimizer; Zelikman et al. 2023）

递归 scaffolding 改进的早期示例。Seed improver $I_t$ 接收初始解 $S_t$、utility function $U$、black-box language model $M$，返回改进解 $S_{t+1}$。STOP 的目标不是直接改进 $S$，而是改进 improver $I$ 本身。

Meta-utility = 给定 improver 在下游任务集上的平均 utility：

$$\bar{J}(I) = \mathbb{E}_{\tau \sim \mathcal{T}} [J(I(\tau))]$$

递归 self-improvement update：

$$I_{t+1} = f(I_t, S_t, U, M)$$

实验发现改进的 improver 发现了遗传算法、分解改进、多臂 prompt bandit、模拟退火、变温度、beam/tree search 等策略。

**关键结论**：STOP 在 GPT-4 上跨迭代改进了平均下游性能，但在 GPT-3.5 和 Mixtral 等较弱模型上退化。递归结构本身不够，base model 必须足够强才能改进机制。Harness 改进能释放模型能力，但智能本身是核心。

### 7.4 Self-Harness（Zhang et al. 2026）

LLM agent 通过 propose-evaluate-accept loop 改进自身 harness，三阶段循环：

**1. Weakness mining**：聚类失败为 verifier-grounded failure patterns。当前 harness $H_t$ 在任务上评估，收集执行轨迹。失败记录需包含丰富信息：terminal verifier-level cause、relevant agent behavior 的 causal status、trace 暴露的 abstract agent mechanism——因为表面相同的错误可能有不同根因。

**2. Harness proposal**：基于失败模式提议 bounded harness edits。同一模型在 $H_t$ 下作为 proposer。提供 bounded proposal context：(1) 当前 harness 的 editable surfaces，(2) verifier-grounded failure patterns，(3) 应保留的 passing behaviors，(4) 之前尝试过的 edits 摘要。优先 recurrent + addressable 的模式。候选应 distinct and diverse。

**3. Proposal validation**：验证并合并合格 edits。held-in 回归测试（弱点是否解决）+ held-out 测试（是否引入新问题）。无回归才 accept。Accepted edits 合并更新 harness，rejected 候选记录但不改变 active harness。

实验：MiniMax M2.5、Qwen3.5-35B-A3B、GLM-5 在 Terminal-Bench-2 上，Self-Harness 学到了针对不同 base model 弱点的 model-specific harness instructions，提升了 held-out pass rate。

**安全警告**：如果程序被允许编辑 OS 系统，抽象边界会被打破。Editable surface 必须被正确设计，permission control 和 security 层必须在这个 loop 之外。Reward hacking 挑战仍然存在。

---

## 8. Evolutionary Search

### 8.1 适用性

进化搜索受自然选择启发，通过变异种群并只保留高 fitness 个体来演化。适用于：(1) 搜索空间大或形状奇特；(2) 难以梯度优化但容易评估。Harness search 是一个很好的匹配。

### 8.2 Prompt 进化

- **Promptbreeder**（Fernando et al. 2023）：通过丰富变异操作优化 task-specific prompts，变异 prompt 本身也被进化。
- **GEPA**（Agrawal et al. 2025）：结合反思式 prompt 和进化搜索，用自然语言 reflection over trajectories 提议 prompt 更新。

### 8.3 AlphaEvolve（Novikov et al. 2025）

Coding-agent 进化搜索系统，存储候选程序池，prompt 冻结的 LLM 生成 diff 改进。反复评估子程序并保留成功的。

**关键设计**：
- Prompt 包含 parent programs、results、instructions，有时含 meta information。
- Coding agent 可访问完整 repo，但可改进的代码区域用 `# EVOLVE-BLOCK-START` 和 `# EVOLVE-BLOCK-END` 显式标记。
- Meta-prompt 与 instructions 和 context 共同演化。
- Ablation 验证了进化过程、prompt 中的 context、meta-prompts、full-file evolution 和更强 LLM 的价值。

### 8.4 变体

- **ThetaEvolve**（Wang et al. 2025）：结合进化搜索与 RL + in-context learning。
- **ShinkaEvolve**（Lange et al. 2025）：三个新组件提升 LLM 采样效率：
  1. Parent sampling 平衡 performance rank 和 offspring count。
  2. Code-novelty rejection sampling——基于 embedding cosine similarity 丢弃与现有种群过于相似的候选。
  3. Meta-scratchpad 识别成功解中的好模式以指导未来变异。

### 8.5 Darwin Gödel Machine（DGM; Zhang et al. 2025）

明确目标：用 LLM coding agent 进化 editable harness-code repository。Agent 被允许修改自己的 harness。

1. 从 pool 中的一个 coding agent 开始。
2. 每次迭代，按概率 ∝ performance 且 ∝ 1/children_count 选一个 parent，修改并分支产生新 agent。
3. 选中的 parent agent 检查自己的 benchmark 评估 log，提议改进自己的 harness codebase。代码编辑用两个基础工具：`bash` 和 `editor (view/create/edit)`。
4. 评估新 coding agent，只有性能足够高的才加入 pool。
5. 重复直到停止条件。

DGM 是固定模型下的 harness evolution。实验：Claude 3.5 Sonnet 作为 base LLM，简单初始 harness 配置，DGM 发现的 agent 在 SWE-bench Verified（20%→50%）和 Polyglot（14.2%→30.7%）上可比或超越手工 agent。

**适用边界**：当候选解可自动评估且 fitness 容易量化时效果好（矩阵乘法、GPU kernel 优化、算法竞赛、数据中心调度）。在评估慢、模糊或主要靠 heuristic 的领域效果差。计算效率和有效性也是关注点。

### 8.6 Hyperagents（Zhang et al. 2026）

引入 meta-agent 控制 how to modify existing task agents to create new ones。

---

## 9. Joint Optimization with Model Weights

Harness evolution 改变模型周围的非参数系统。为实现完全 self-improvement，模型也可以同时更新自身权重——通过改进 training pipeline 或 test-time continual learning。

### SIA（Hebbar et al. 2026）

结合 harness improvement 和 model-parameter updates 的早期尝试，三个组件：

- **Meta-Agent**：提议初始 harness。
- **Task-Specific Agent**：执行任务。
- **Feedback-Agent**：根据近期 trajectory 选择更新 harness 还是模型权重。

实验存在混淆因素（task-specific agent 用的模型 gpt-oss-120b 远弱于 Meta-Agent 和 Feedback-Agent 用的 Claude Sonnet 4.6，baseline 太弱）。方向有趣但证据 provisional。训练稳定性、Goodhart 效应等挑战仍然开放。

---

## 10. Future Challenges

### 10.1 实验中的失败模式

Trehan & Chopra (2026) 测试 LLM 能否在最小 scaffolding 下从研究想法到论文（工具：read_file、write_file、llm_search、list_files）。三个领域（world models、multi-agent RL、AI safety & alignment），每个领域 45-50 篇种子文档。仅 4 个想法被人类专家选中跑完整 pipeline，仅 1 个完整执行成论文。六个反复出现的失败模式：

1. **偏向训练数据默认值**：用旧库、过时命令、标准格式，或假设未基于实际 repo/dataset。
2. **执行压力下的实现漂移**：实现变得复杂时，模型趋向常见简单方案而非提议方法。
3. **记忆和上下文退化**：长周期项目丢失关键细节，除非日志写成持久 artifact。
4. **过度乐观**：实验失败或噪声大却宣称成功（"numerical duct tape"、"p-hacking and eureka-ing"）。
5. **领域智能不足**：缺乏隐性工艺知识（预测实现复杂度、判断结果合理性、知道哪些 baseline 重要）。
6. **科学品味不足**：实验可执行但没回答对的问题。

### 10.2 七大瓶颈

1. **Weak and fuzzy evaluators**：很多研究 claim 和真实任务没有快速精确的 verifier。当前 self-improvement loop 在指标可测且客观时效果最好（类似 RL）。Research taste、novelty 和 long-term scientific value 难以量化。

2. **Context and memory lifecycle**：随 agent 自主性增长，记忆膨胀。有用的 harness 需要管理 context 和 memory 以补充长上下文生成的局限，同时最大化长周期任务成功率。Context engineering 应成为 intelligence 的核心部分，而非停留在 software system 层。

3. **Negative results**：文献偏向成功案例。LLM 可能不擅长判断何时放弃假设、报告负面结果或承认失败。Research harness 应让失败尝试易于保留——从失败中学习是裁剪搜索空间的最佳方式。

4. **Diversity collapse**：进化/RL 倾向利用已知高 reward 模式。需要机制防止种群塌缩为同一解的变体。这对开放式研究尤其关键——最好的路径在当前 evaluator 下可能初始看起来更差。

5. **Reward hacking**：self-improvement loop 优化任何给定信号。单元测试→过拟合测试；judge model→学 judge 特定的 hacking 技巧；benchmark scores→利用 benchmark artifacts。Evaluator 和 permission control 应在 evolution loop 之外，配 held-out tests、trace audits 和关键决策点的人工 review。

6. **Long-term success**：外层优化作用于个体 rollout 之外的可模拟 reward。Coding agent 已提升日常生产力，但很多优化目标过于短期。Sandbox-based RLVR 训练难以捕获 maintainability、ownership boundaries、migration cost、backwards compatibility、future debugging burden 等长期 repo health。

7. **The role of humans**：人类应沿栈上移，而非被移除。在正确的时间、正确的抽象层级提供 oversight。系统设计应考虑何时和如何设置 touch points。许多上述挑战需要人类反馈和引导。我们是在为人类更好的未来构建技术，而非相反。

---

## 参考文献

[1] Good, I. J. "Speculations Concerning the First Ultraintelligent Machine." Advances in Computers, 6:31–88, 1965.

[2] Yudkowsky, Eliezer. "Recursive Self-Improvement." LessWrong, 2008.

[3] Choi, et al. "Anchored Self-Play for Code Repair." ICML 2026.

[4] Zhao, et al. "Absolute Zero: Reinforced Self-play Reasoning with Zero Data." arXiv preprint arXiv:2505.03335, 2025.

[5] Yuan, et al. "Self-Rewarding Language Models." arXiv preprint arXiv:2401.10020, 2024.

[6] Chen, et al. "Self-Play Fine-Tuning Converts Weak Language Models to Strong Language Models." ICML 2024.

[7] Zhang, et al. "Agentic Context Engineering: Evolving Contexts for Self-Improving Language Models." ICLR 2026.

[8] Ye, et al. "Meta Context Engineering via Agentic Skill Evolution." arXiv preprint arXiv:2601.21557, 2026.

[9] Lee, et al. "Meta-Harness: End-to-End Optimization of Model Harnesses." arXiv preprint arXiv:2603.28052, 2026.

[10] Lu, et al. "Towards end-to-end automation of AI research." Nature, 651:914–919, 2026.

[11] Meng, et al. "ScientistOne: Towards Human-Level Autonomous Research via Chain-of-Evidence." arXiv preprint arXiv:2605.26340, 2026.

[12] Kulikov, et al. "Autodata: An agentic data scientist to create high quality synthetic data." arXiv preprint arXiv:2606.25996, 2026.

[13] Hu, Lu, and Clune. "Automated Design of Agentic Systems." ICLR 2025.

[14] Madaan, et al. "Self-Refine: Iterative Refinement with Self-Feedback." NeurIPS 2023.

[15] Zhang, et al. "AFlow: Automating Agentic Workflow Generation." ICLR 2025.

[16] Zelikman, et al. "Self-Taught Optimizer (STOP): Recursively Self-Improving Code Generation." COLM 2024.

[17] Zhang, et al. "Self-Harness: Harnesses That Improve Themselves." arXiv preprint arXiv:2606.09498, 2026.

[18] Fernando, et al. "Promptbreeder: Self-Referential Self-Improvement Via Prompt Evolution." arXiv preprint arXiv:2309.16797, 2023.

[19] Agrawal, A. et al. "GEPA: Reflective Prompt Evolution Can Outperform Reinforcement Learning." arXiv preprint arXiv:2507.19457, 2025.

[20] Novikov, et al. "AlphaEvolve: A coding agent for scientific and algorithmic discovery." arXiv preprint arXiv:2506.13131, 2025.

[21] Lange, Imajuku, and Cetin. "ShinkaEvolve: Towards Open-Ended And Sample-Efficient Program Evolution." arXiv preprint arXiv:2509.19349, 2025.

[22] Wang, et al. "ThetaEvolve: Test-time Learning on Open Problems." arXiv preprint arXiv:2511.23473, 2025.

[23] Zhang, et al. "Darwin Gödel Machine: Open-Ended Evolution of Self-Improving Agents." arXiv preprint arXiv:2505.22954, 2025.

[24] Zhang, et al. "Hyperagents." arXiv preprint arXiv:2603.19461, 2026.

[25] Yuksekgonul, et al. "Learning to Discover at Test Time." arXiv preprint arXiv:2601.16175, 2026.

[26] Riaz, et al. "Epistemic Uncertainty for Test-Time Discovery." arXiv preprint arXiv:2605.11328, 2026.

[27] Hebbar, et al. "SIA: Self Improving AI with Harness & Weight Updates." arXiv preprint arXiv:2605.27276, 2026.

[28] Trehan and Chopra. "Why LLMs Aren't Scientists Yet: Lessons from Four Autonomous Research Attempts." arXiv preprint arXiv:2601.03315, 2026.

[29] Bubeck, et al. "Early science acceleration experiments with GPT-5." arXiv preprint arXiv:2511.16072, 2025.

[30] Starace, et al. "PaperBench: Evaluating AI's Ability to Replicate AI Research." ICML 2025.

[31] Wijk, et al. "RE-Bench: Evaluating frontier AI R&D capabilities of language model agents against human experts." ICML 2025.

[32] Chan, et al. "MLE-bench: Evaluating Machine Learning Agents on Machine Learning Engineering." arXiv preprint arXiv:2410.07095, 2024.

[33] Chen, et al. "ScienceAgentBench: Toward Rigorous Assessment of Language Agents for Data-Driven Scientific Discovery." ICLR 2025.

[34] Siegel, et al. "CORE-Bench: Fostering the Credibility of Published Research Through a Computational Reproducibility Agent Benchmark." TMLR 2024.

[35] Ouyang, et al. "KernelBench: Can LLMs Write Efficient GPU Kernels?" arXiv preprint arXiv:2502.10517, 2025.
