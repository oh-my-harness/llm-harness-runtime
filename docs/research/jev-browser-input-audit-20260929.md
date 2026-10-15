# jev-browser 初始输入审计

更新：2026-09-29 18:00 Asia/Shanghai。范围：真实浏览器观测和固定 SDK 分词，不是模型推理或任务通过率实验。

## 结论

**7 个公开任务的初始页面均已成功采集两次。当前原样表示不能完整容纳 Hacker News；仅将 state 编码为无损紧凑 JSON 后，两次采集的全部初态均可装入配置的 8,192-token 窗口。**

这是适配准备进展，不是模型速度或质量成果。没有调用 Qwen/Jev，没有加载 Laya 权重或执行前向，没有让模型推进任务。紧凑表示目前只在审计脚本中试验，尚未应用到真实模型接口；准备清单仍关闭真实运行。

Hacker News 余量仅 **43–67 token**。页面变化、历史记录和后续页面都可能再次超出预算，初态容纳成功不能证明完整任务始终可运行。建议明确采用无损格式适配后，进行有界开发 A/B，逐步审计并保留所有输入拒绝与失败，不能用截断或任务筛选美化结果。

## 1. 实测数据

表中 token 是同一请求最宽选择头的完整序列需求，包含指令、候选、状态及特殊标记；不是 Qwen token，也不是模型调用次数。两次采集分别始于北京时间 17:50:42 和 17:54:58。

| 任务 | 原样序列 token，第一 / 第二次 | 紧凑 state 后，第一 / 第二次 | 初始目标头最大候选数 | 结论 |
| --- | ---: | ---: | ---: | --- |
| shadow-dom | 431 / 431 | 404 / 404 | 1 | 可容纳 |
| nested-scroll | 621 / 621 | 560 / 560 | 8 | 可容纳 |
| cross-origin-iframe | 501 / 501 | 474 / 474 | 1 | 可容纳 |
| select | 542 / 542 | 494 / 494 | 3 | 可容纳 |
| wikipedia | 3,448 / 3,448 | 3,120 / 3,120 | 51 | 不能沿用小预算；可容纳 |
| hacker-news | 9,080 / 9,056 | 8,149 / 8,125 | 157 | 原样超出配置长度；紧凑后余量很小 |
| mdn | 1,334 / 1,334 | 1,187 / 1,187 | 21 | 不能沿用 1,024；可容纳 |

所有 14 次页面加载返回 HTTP 200；记录了标题、候选数、页面指纹和请求散列。Hacker News 的第二次请求已经发生内容变化，不能将公开网站当固定数据集。

固定 SDK 的候选描述另有 **每项 48 token** 截断上限，两次初态中均未触发；这不保证后续状态不触发。checkpoint 默认 `max_len=512 / head_max_len=192` 在 7 个初态均不完整，其中包含指令预算损失，不能直接拿默认设置做对照。紧凑候选方案的 `8192 / 4096` 在两次采集的全部选择头上通过完整性检查。

## 2. 核查方式与事实来源

- 上游固定为 [jev-browser 990005a](https://github.com/dennisonbertram/jev-browser/tree/990005a1b9c21258682d6da8cced4c7a9a5bc2da)。用其实际 `observe` 和 `buildDecisionRequest` 生成初态请求，不调用 `run`、模型或验收器，不执行解题动作。
- Laya 权重版本仅用于定位 tokenizer/config：`convaiinnovations/laya@55cf4c4ebb4ebe31b2550e8bdf3bd21b99753851`。读取的 ModernBERT-large encoder 配置和 tokenizer 均标注长度 8,192；本轮以配置值作审计边界，**未尝试扩大位置配置，也未证明超出该值一定能/不能推理**。
- 固定 [Laya SDK 9d95567](https://github.com/NandhaKishorM/laya/tree/9d955671415fc19f069b9cc998928075c1f255ec)。已将安装版 `agent.py`、`common.py`、`__init__.py` 与该提交逐字节核对（仅统一行尾）。[序列构造](https://github.com/NandhaKishorM/laya/blob/9d955671415fc19f069b9cc998928075c1f255ec/laya/common.py)包含候选 48-token 限制、head 预算裁剪与 state 裁剪，不能只估算总字符数。
- 审计实际调用该 SDK 的静态问题验证、serializer、tokenizer 与 `build_sequence`；校验实际序列长度，复用运行时的完整性检查。报告保留分头的指令长度、候选长度、丢失量、marker 数、候选可区分数和序列散列。
- 原样 `state` 为 dict，SDK 以含分隔空格的 JSON 编码。候选方案只将同一 dict 编成 `json.dumps(..., ensure_ascii=False, separators=(",", ":"))` 字符串，利用 SDK 对字符串 state 的现有支持。不删除任何字段、值、历史或候选；逐条检查 JSON 还原相等，questions 完全不变。**不删除字符串值本身的空白。**
- 此处“完整”指相对于上游提供的观测没有新增 token 裁剪；不是完整网页 DOM。上游自身的页面文本和名称有长度限制，两组共用，不把这些已有界限隐藏掉。

环境：Windows x64；Node 24.14.1；Chromium 153.0.8010.12；视口 1120×780；每次使用独立无头上下文。Laya 包 0.3.21、Transformers 5.17.0、Tokenizers 0.23.2。tokenizer 在 CPU 上执行；未做 GPU 推理。仅查询到本机 RTX 5070 Ti、16,303 MiB 显存、驱动 591.86，不能据此声称长输入显存验证已通过。

## 3. 下一次开发试跑提案

**待 lqj 确认，未启动。** 研究目标、Qwen/Laya 模型选择、公开 7 任务和独立验收职责不变。不把无损格式适配称为原版 Jev 成绩复现。

| 条件 | 提案 |
| --- | --- |
| 范围 | 7 个任务，每题 A/B 各 1 次，共 14 次；只有 7 个独立任务，不作质量等价检验 |
| 输入与模型 | Laya 使用已缓存固定版本、BF16、无损紧凑 state，`max_len=8192 / head_max_len=4096`；Qwen 仍为既定 72B，禁止静默换模型或 CPU 回退 |
| 调度与时限 | 单并发；按任务索引交替 AB/BA；每次最多 180 秒、30 动作、60 决策；浏览器单次操作限 15 秒，整场最多 60 分钟 |
| Qwen 预算 | temperature 0，输出上限 1,024 token，请求时限 45 秒，每个逻辑请求至多 2 次实际尝试；整场最多 120 次实际请求，含重试和最多 2 次部署预检。该上限是请求量约束，不是人民币费用承诺 |
| Laya 预检 | 加载限 180 秒，单次预测限 45 秒；一次短预热后，用最多 7 个已采集初态各检查一次真实前向、输入和显存。预检失败停止，不自动放宽输入审计 |
| 质量判定 | 四个本地 checker 保持上游原样；公开站点使用已披露的严格终末检查并保留原判定。验收不提供运行中答案，DONE 不算通过 |
| 超限与异常 | 逐步完整性审计失败、超时、动作耗尽保留在已执行尝试分母；不裁剪候选、不换题、不删除失败。预算耗尽后未启动项单列，不能伪装成已运行失败 |
| 分析边界 | 报告用时、调用次数、通过率和配对差异；不设虚假的“相近质量已成立”结论，正式容忍度与样本量另行预注册 |

先实现获批的格式适配、整场预算与落盘，再执行真实预检；预检通过才进入这 14 次开发尝试。既有运行时逐步输入拒绝不能关闭。后续完整轨迹仍可能超长，这是需要如实观察的适配边界，不是提前保证全部能跑通。

端到端用时必须包含格式转换和逐步审计成本；预检、模型加载与预热单列，不混入任务成绩，也不隐藏任务中发生的额外开销。

## 4. 复核与留档

实现入口：[采集脚本](../../scripts/benchmarks/jev_browser/capture-inputs.mjs)、[分词审计](../../scripts/benchmarks/jev_browser/audit_inputs.py)。两轮原始文件均在忽略目录 `logs/benchmarks/setup/jev-browser-input-audit-20260929-01` 和 `...-02`，没有覆盖首轮结果，不提交网页内容、权重、日志或凭据。

| 原始文件 | SHA-256 |
| --- | --- |
| 01/captures.json | `10962c046d2ac6bf5b35807786de5b6f8ab89c33dbee50c4c9c5adce69c1a849` |
| 01/input-audit-with-compact.json | `596649bd26887becdcb3e063dae7b1df8570004ac1944aba5112c024904c9681` |
| 02/captures.json | `2492b25ee705af6760aa71d3a85c4297de4035b8813e89bda4c27a506dfc2512` |
| 02/input-audit-with-compact.json | `a732d817dc1eb3640630cd9f0c1dbb2756acef844127db2a7c0004da8ce6899c` |

新增 2 项 Node 采集测试、6 项 Python 审计测试通过；完整相关测试为 Node 35/35、固定 Python 环境 56/56，无跳过。测试覆盖保持内容、48-token 截断、状态超长、指令预算不足和禁止加载模型等路径。它们仍是工程验证，不计入模型任务通过率。

运行时 `cargo fmt` / `cargo clippy --all-targets --all-features` 通过；`cargo test --workspace` 仍在既有 `provider_and_error::invalid_api_key_surfaces_as_error` 断言失败。本轮没有修改 Rust，输出留在 `01/runtime-workspace-tests.log`，不声称全仓测试通过。

```powershell
node scripts/benchmarks/jev_browser/capture-inputs.mjs --output logs/benchmarks/setup/NEW_INPUT_AUDIT
& ../benchmark/tools/laya-venv/Scripts/python.exe scripts/benchmarks/jev_browser/audit_inputs.py --captures logs/benchmarks/setup/NEW_INPUT_AUDIT/captures.json --output logs/benchmarks/setup/NEW_INPUT_AUDIT/input-audit.json
```

输出目录和结果文件必须不存在；已采集内容的请求散列若改变，审计会拒绝。复核公开页面得到不同 token 数是可能的，应保留新结果与采集时间，不覆盖旧值。
