# Laya 决策契约与浏览器权重核查

更新：2026-09-29 18:52 Asia/Shanghai。负责人：lqj。范围：离线审计，新增模型调用为 0。

## 结论先行

**本轮没有发现 choice 接口或选项键映射错误；现有证据不足以把 0/7 归为某个已确定的单一根因。** 原始 [A 7/7、B 0/7 结果](jev-browser-development-20260929.md) 保留。接口兼容、输入完整，只证明模型收到了合法请求，不证明它已经具备浏览器决策能力。

此前遗漏了固定 Laya 源码中已存在的浏览器微调文档，这是前期资料核查的不足。该文档直接讨论未专门训练的模型无法可靠完成浏览器选择，并引用社区浏览器权重。因此下一步优先核查这个相关权重，而不是把现有基座继续盲目扩样，或据此否定 Jev 的分工方式。

## 1. 已排查的接入问题

固定 SDK `9d955671415fc19f069b9cc998928075c1f255ec`：

- [`common.py`](https://github.com/NandhaKishorM/laya/blob/9d955671415fc19f069b9cc998928075c1f255ec/laya/common.py#L74) 明确支持字符串 state 和结构化 criteria，后者按 JSON 渲染；不能把本轮 dict 型选项描述直接认定为 API 误用。
- [`agent.py`](https://github.com/NandhaKishorM/laya/blob/9d955671415fc19f069b9cc998928075c1f255ec/laya/agent.py#L731) 将结构化 instructions 序列化；解码按 criteria 键顺序将概率最大项还原为原始键。`confidence` 与 `answer_confidence` 不是同一指标，均不是任务验收结果。
- 本地 `policy.mjs` 和固定上游 `decide.ts`/`run.ts` 没有根据低置信度把 B 的选择替换成 DONE/BLOCKED，也没有低置信度自动求助 Qwen。上游直接采用 operation 头，以及对应的 target 头。

新增只读脚本 [`audit-traces.mjs`](../../scripts/benchmarks/jev_browser/audit-traces.mjs)，从现有日志重算请求 SHA-256，检查输入审计记录、全部选择头的键集合/概率/argmax，再检查实际 policy 记录的 operation/target。它不加载权重、不联网、不重跑任务、不修改旧日志。

```powershell
node scripts/benchmarks/jev_browser/audit-traces.mjs logs/benchmarks/jev-browser-development-20260929-02
node --test scripts/tests/jev_browser/audit-traces.test.mjs
```

结果：**7 份 B 轨迹、12 次实际决策、40 个选择头全部通过上述核对**。针对错误映射、非最大概率选择、输入变化/截断、缺失结束事件等的 10 项测试通过。脚本范围是记录中的未缓存 B 决策，不声称重新执行了 DOM，也不从模型概率推导任务正确性。

| 实际失败现象 | 复核证据 |
| --- | --- |
| 下拉框选错 | 首次 SELECT 概率 0.8854；`1:1` 对应 Choose one、概率 0.6791，`1:3` 对应 VIP、概率 0.1411；policy 采用 `1:1`，并非把 VIP 错映射成占位项 |
| 提前结束 | 4 次 DONE、2 次 BLOCKED 均为模型 operation 头的最大概率项，并原样进入 policy；不是框架按阈值强行结束 |
| 未调用文本模型 | 12 次 operation 选择没有 TYPE_TEXT；所以未触发 Qwen 生成，不是填写请求丢失 |
| 输入或设备异常 | 这 12 次传输校验全部通过；日志中状态、指令、选项损失为 0，实际前向均为 CUDA，无 CPU 回退 |

这缩小了排查范围，但不能排除合法提示表达不适合当前权重、长上下文分布差异或模型能力不足。不能将“没有接口错误”写成“已经证明唯一根因是缺少微调”。

## 2. 查到的直接相关资料

[Laya 官方仓库的浏览器微调文档](https://github.com/NandhaKishorM/laya/blob/9d955671415fc19f069b9cc998928075c1f255ec/docs/finetune_browser_agent.md) 报告：在作者的 16 个浏览器任务、每题 3 次评测中，`typed-decisions` 零样本为 0%，浏览器微调后的 322M 版本为 62%。**这不是我们的 English 基座、不是同一任务集，也不是本地复现数据。** 它说明存在值得核查的专门适配路径，不能直接借用其提升幅度。

该文档指向 [`cklxx/laya-browser`](https://huggingface.co/cklxx/laya-browser/tree/645cf366a2ae35f1086e8c20eff48f909bb49206)。2026-09-29 查询公开 Hub API，固定快照为 `645cf366a2ae35f1086e8c20eff48f909bb49206`，版本 v19s；仓库公开、无需门控，作者标注 Apache-2.0。**这是官方文档引用的社区发布，不是 TypeSafe Jev 权重，也不是我们此前运行的 `convaiinnovations/laya`。**

[`rl_agent_config.json`](https://huggingface.co/cklxx/laya-browser/blob/645cf366a2ae35f1086e8c20eff48f909bb49206/rl_agent_config.json) 标明 mmBERT 基座、输入格式 v5、max_len=1024、head_max_len=768。[模型卡](https://huggingface.co/cklxx/laya-browser/blob/645cf366a2ae35f1086e8c20eff48f909bb49206/README.md) 自报真实多步 Suite C 约 20%–26%，不是可靠通用浏览器能力；它还明确说版本间提升有一部分来自框架改变。本轮只读取配置、代码和说明，未下载权重或执行 remote code。

## 3. 不能直接照搬的部分

[`laya_browser.py`](https://huggingface.co/cklxx/laya-browser/blob/645cf366a2ae35f1086e8c20eff48f909bb49206/laya_browser.py) 显示，匹配训练格式不等于只换模型名：它缩短页面文本至 1200 字符、压缩字段与选项描述，宽于 60 项的选择分块后再比较赢家。后者可能增加前向次数，必须计入成本；前者是信息删减，不能冒称本轮已批准的无损表示。仅把其权重塞进现有 8192/4096 输入，也不能声称忠实复现作者用法。

作者的 [`jev-ultrafast.patch`](https://huggingface.co/cklxx/laya-browser/blob/645cf366a2ae35f1086e8c20eff48f909bb49206/code/jev-ultrafast.patch) 面向另一个引擎版本，包含抑制重复操作的候选过滤、大模型选择下拉值，以及可选的升级/规划/终止检查路径。**不能把这些辅助行为连同权重无差别迁入，或把“代码里存在”当作作者每项评测都启用了。** 我们仍要求决策模型选择操作与目标，Qwen 只提供必要文本。

官方文档所列训练抓取包含 Wikipedia/Hacker News 等域名，与当前公开任务存在域名重叠。尚未完成样本级查重，不能宣称新权重对这 7 题是未见泛化。

## 4. 下一步提案与边界

lqj 已确认将浏览器专用固定快照列为下一版本候选，先完成输入兼容性核查，不自动引入辅助规则、大模型代选或信息删减。已下载固定快照的 tokenizer、配置及供审阅的包装源码；不含模型权重。**尚未将新权重写入运行配置，也未启动另轮 A/B**。

后续应先明确采用哪一种输入表示。若需要接近作者训练格式，必须列出相对原始 observation 的信息变化，并让 A/B 获取相同的可见任务信息和候选语义；不能先看得分再决定保留哪种表示。保留同一 Qwen、公开 7 任务、独立验收、失败分母及端到端/调用计量，不增加规则代选、不给 B 特供正确答案。

输入审计通过且版本条件确认后，再有界运行新的开发配对；原 0/7 不覆盖。即使新的 7 对改善，也只说明该适配值得进一步验证，不证明正式质量等价。研究目的始终是更快且相近质量，不变为“让某个 checkpoint 跑出正结果”。

本次代码检查：53 项 Node 测试通过；Rust fmt/clippy 通过。Rust 全仓仍在既有 `provider_and_error::invalid_api_key_surfaces_as_error` 失败，本轮没有修改 Rust，不声称全仓通过。
