# jev-browser 开发运行记录

更新：2026-09-29 18:22 Asia/Shanghai。负责人：lqj。

## 当前结论

**已实际启动，但首轮止于模型预检，尚无 A/B 速度或任务通过率结果。** 这是公开 7 任务的 Qwen/Laya 适配，不是原版 Jev 成绩。研究目标不变：决策模型直接选择操作与目标，大模型仅在必要时生成文本，验证是否更快且保持相近完整任务通过率。

首轮遇到的是执行器的 Windows 字符编码错误，不是证据表明 Laya 判断错误、显存不足或研究设想无效。问题已离线复现并修复；未通过预检就没有进入 14 次任务实验，未开始项不记为失败任务。

## 量化状态

运行目录：`logs/benchmarks/jev-browser-development-20260929-01`。开始代码：`5a8a64b`。首轮结束于 2026-09-29 18:19:25 Asia/Shanghai。

| 指标 | 首轮实际记录 |
| --- | --- |
| 整场墙钟，含加载与清理 | 7.556 秒 |
| Laya 权重加载 | 826.951 毫秒；另有 Python/依赖初始化成本 |
| Laya 预热 | 1 次，322.454 毫秒，1 次前向 |
| 初态模型检查 | 前 3 个完成，第 4 个 select 在输入处理阶段报 `UnicodeEncodeError` |
| 已完成初态检查的实际前向 | 3 次；不含上述预热 |
| Qwen 实际 HTTP 请求 | 0 |
| 已执行 / 未开始 A/B 尝试 | 0 / 14 |
| 任务通过率、配对加速比 | 不适用，不填 0%，不宣称质量等价 |

第 4 次 RPC 的历史日志把 SDK 调用/前向记为未知，观测到的前向启动为 0；保留原始值，不改写历史记录。代码路径与离线复现将故障定位在模型推理之前；新版在进入 SDK 前明确初始化计数，并验证传输校验和。

## 条件与边界

- 上游固定 `990005a1b9c21258682d6da8cced4c7a9a5bc2da`，公开任务仍为 shadow-dom、nested-scroll、cross-origin-iframe、select、wikipedia、hacker-news、mdn。只登记同引擎注入与 Windows 工程测试两个补丁，不替换任务。
- A 使用 SiliconFlow 的 `Qwen/Qwen2.5-72B-Instruct` 一次选择操作、目标及必要文本；B 使用 Laya 选择，Qwen 仅为已选择的字段生成文本。Qwen temperature=0、max_tokens=1024、每个逻辑请求最多 2 次 HTTP 尝试、45 秒超时。
- Laya 权重 `convaiinnovations/laya@55cf4c4ebb4ebe31b2550e8bdf3bd21b99753851`，SDK `9d955671415fc19f069b9cc998928075c1f255ec`；无损紧凑 state，max_len=8192、head_max_len=4096，每次输入完整性审计，禁止 CPU 前向和自动换模型。
- 本机 Windows、RTX 5070 Ti（16 GiB）、PyTorch 2.11.0+cu128、CUDA 12.8、BF16；前三次初态检查的峰值已分配显存约 2.30 GiB，**最长输入尚未真实验证，不能据此宣称显存预检全部通过**。服务端硬件未知。
- 批准矩阵为 7 任务各 1 对、共 14 次；串行、交替 A/B 与 B/A、新浏览器上下文 1120×780；单次最多 180 秒、30 动作、60 决策，整场最多 3600 秒。
- Qwen 总请求上限 120，含预检和重试，是请求次数而非金额上限。当前消费为 0。真实预检使用第二轮冻结初态，不把预检选项与任务执行准确率混为一谈。
- 终末验收不反馈给模型，保留上游判定，同时核对目标 URL/头条 ID 和主文档 HTTP 非错误；DONE 本身不是成功。小样本仅描述，不证明相近质量。

## 故障复核与修复

Python 原管道实际为 `gbk`、`surrogateescape`，Node 写入为 UTF-8。无模型离线复现中，前三个初态都是可原样往返的 ASCII 内容；select、wikipedia、hacker-news 则无法原样往返，其中 select 产生 7 个代理码点，随后 UTF-8 编码失败。

修复显式指定 Python UTF-8 管道并严格解码；每个真实预测请求附原始 JSON 字符串 SHA-256，Python 先校验再解析、序列化和推理。校验不通过禁止前向。新增真实 Python 子进程测试覆盖中文、符号、重音字符及全部 7 个冻结初态，不加载权重；此类测试只证明通信正确，不算模型成绩。

下一步：已向 lqj 申请修复后重做一次真实预检（额外一次预热及最多 7 次初态推理），通过后继续原定 A/B，不增加 Qwen 总上限、不改实验目标。首次失败完整保留，不隐去工程成本。

## 留档

原始文件在本机忽略目录，网页内容、密钥、模型权重与完整日志不提交。

| 文件 | SHA-256 |
| --- | --- |
| `01/report.json` | `dee93e608143b274ea6d99028f4e8c66d00df5ef253256cb27c5f6ce235af087` |
| `01/setup-events.jsonl` | `d195383f2e86e2506f98e64199b7b4243bc8abfceaf172308c2e209cd69ed54a` |

运行入口提交前 41 项 Node、57 项 Python 测试通过；编码修复后完整回归为 **43/43 Node、58/58 Python**，无跳过，覆盖真实 Python IPC 和校验和拒绝。Rust fmt/clippy 通过；Rust 全仓仍在既有 `provider_and_error::invalid_api_key_surfaces_as_error` 失败，本轮不修改无关 Rust 行为。首轮结果不作覆盖重跑。
