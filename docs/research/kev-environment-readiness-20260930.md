# Kev 环境准备与验收

更新：2026-09-30 19:09 Asia/Shanghai。负责人：lqj。状态：本地模型环境、跨系统接口及正式服务生命周期预检通过；后续正式 A/B 已完成，见[结果报告](kev-browser-flash-comparison-20260930.md)。以下保留各阶段预检结果，不把预检当成任务成绩。

## 范围

目标仍是决策模型承担动作/目标选择，Flash 按原分工生成文本和提供既有 helper，验证完整任务是否更快且保持相近通过率。本轮只做本地环境与协议预检，不发起 Flash 请求、不访问实验网站、不调整提示、候选、任务、预算或验收。

本次候选为 `jaredpalmer/kev-4b@qwen3`，基于 `Qwen/Qwen3-4B-Base` 的旧版 attention-only 模型。选择依据是先前讨论的部署简化方向，不是因看到本套任务上的成绩而选型。作者当前默认 `main` 是另一套 Qwen3.5 权重，不可省略 `@qwen3` 后冒称同一模型。

## 预先规定的验收项

- [x] 阅读作者模型卡、依赖、checkpoint loader 和 HTTP 服务实现。
- [x] 固定源码、权重和环境，独立安装，`pip check` 通过，确认实际 CUDA 计算。
- [x] 作者样例、16 个任务首个录制请求及最长录制请求通过。在保留作者原有有损预处理后，Kev 编码无新增截断、无候选/问题丢失，输出分布有限且归一。
- [x] 使用作者 `Checkpoint` / `Server` 路径，检查首访/热调用和 Windows HTTP 输出。保留快照温度，不加入日期规则；本旧版温度实际为 1.0，不能套用新版已拟合校准的说法。
- [x] 加载、调用端墙钟、作者模型服务段、真实前向、缓存、峰值显存分别记录；下载/安装另列，不将 `latency_ms` 当成含排队/网络的总耗时。
- [x] 原 Windows `jev_ultrafast.model.post_json` 和 `validate_choice` 访问 WSL 本地服务成功；仅监听回环地址，检查后已关闭服务。
- [x] 中文记录给出通过/未通过和未覆盖边界。
- [ ] 正式 A/B 的 Kev 组服务生命周期、预算中断、独立计数和失败路径接入及回归审计。尚不能把 `environment_ready=true` 写成 `formal_experiment_ready=true`。

不对“Kev 与旧 Laya/Decider 输出相同”作要求，它们是不同模型。部署数值检查不使用任意概率差阈值声称质量等价；精度和缓存路径差异单列，若导致选择不稳定或接口错误则先分析。

## 固定环境

| 项目 | 本次条件 |
| --- | --- |
| 系统、硬件 | Ubuntu 24.04 / WSL2，RTX 5070 Ti 16GB，Windows 驱动 591.86 |
| 依赖 | Python 3.12，torch 2.8.0+cu128，transformers 5.17.0，peft 0.21.0，Triton 3.4.0；按作者 `uv.lock` 冻结安装，完整版本见本地报告 |
| 决策模型 | `jaredpalmer/kev-4b@qwen3`，实际加载 4,023,779,328 参数；BF16，LoRA 按作者默认合并，指针头保留 FP32，无量化 |
| 权重修订 | adapter `c4bfa11b0dc07691884f2d97f1c4c4c05c92e416`；底座 `Qwen/Qwen3-4B-Base@906bfd4b4dc7f14ee4320094d8b41684abff8539`；按 Hub LFS SHA-256 校验 |
| GPU 路径 | 作者默认 SDPA；分析器记录真实 `fmha_cutlassF_bf16` memory-efficient attention 和 BF16 GEMM。此 attention-only 版本不走 hybrid 专用 FLA/融合/图捕获分支，不是因缺依赖回退 |
| 模型缓存 | 保留作者状态前缀缓存，4 项；不增加跨任务答案缓存，日期规则关闭，温度 1.0 |
| 浏览器包装器 | 保留原 `cklxx/laya-browser@645cf366` 的页面/候选整理、1200 字符页面文本、60 项分块；不改原作者源码 |
| 实际接口链 | Windows 原客户端 → 回环 HTTP → 原 `LayaBrowser.systemone` → 最小 predict 适配 → Kev 作者 `Server.answer` → 模型；外层 HTTP 是预检服务，不冒称完整采用 Kev FastAPI 服务 |

## 预检数据

输入事先按结构选取，不按任务成败挑选：作者样例、原 16 题各首个 C 请求、最长序列化请求，共 18 条、45 个原始问题。每条清空前缀缓存后调用一次，再连续调用三次；保留其候选顺序，不用答案做调参。

| 指标 | 结果 |
| --- | ---: |
| 录制输入完成 | 18/18；72 次 HTTP 回放 |
| 模型逻辑调用 / 实际底座前向 | 96 / 96，含作者宽候选分块；不含额外分析器和 Windows 探针 |
| 单候选问题覆盖 | 28 个模型问题行；真实执行前向，不添假候选 |
| 编码规模 | state 57–528 token；最大 packed 序列 3,763 token；分块后最多 59 个候选 |
| 加载时间 | 2.639 秒，缓存权重已在本机，不含下载和 SHA 校验 |
| 18 条首次调用累计 / 54 次热调用累计 | 7.887 / 20.079 秒 |
| 热调用中位数 / p95 | 0.279 / 0.759 秒，调用端墙钟 |
| 热调用范围 | 0.114–0.774 秒 |
| 作者模型服务段累计 | 26.471 秒，72 次回放中的全部底层调用之和；包含服务段 CPU 调度与同步，不是纯 CUDA kernel 时间 |
| 冷热比较 | 135 个问题比较，选择变化 0；最大概率绝对差 0.01085，不是逐位相同 |
| 前缀缓存命中 / 未命中 / OOM 重试 | 35 / 5 / 0；较短 state 不进入缓存，不能与总请求数直接相加对账 |
| PyTorch 峰值 allocated / reserved | 8.388 / 9.006 GiB，含额外分析器检查；不是整机总显存 |
| 原 Windows 客户端探针 | 0.388 秒，合法响应；与该样例本地热调用的概率差 0，选择相同 |
| Flash 请求 / 在线任务 | 0 / 0 |

这些时间是环境预检，不是浏览器任务端到端时间。首访指该输入没有前缀缓存，不代表每条都重新启动模型。未比较 BF16 与 FP32，也未证明所有未来轨迹都在显存/输入范围内；不得由此得出质量等价、比 Laya 快或比纯 Flash 快的结论。

## 复跑与归档

入口：[kev_preflight.py](../../scripts/benchmarks/jev_browser/kev_preflight.py)。在独立 Python 3.12 环境按固定 Kev 源码的 `uv.lock` 执行 `uv sync --frozen --extra serve`，先下载上述固定修订，再在离线模式运行。旧 Laya/Decider 环境不改。

```bash
python scripts/benchmarks/jev_browser/kev_preflight.py run \
  --corpus logs/benchmarks/decider-readiness-corpus-20260930-01/corpus.json \
  --output logs/benchmarks/NEW_KEV_PREFLIGHT
```

模型进程在完成计时后写出 `ready.json`，等待 Windows 探针最多 120 秒。此时使用已有浏览器依赖的 Windows Python 执行（须 `-X utf8`，本地连接绕过代理）：

```powershell
python -X utf8 scripts/benchmarks/jev_browser/kev_preflight.py probe --output logs/benchmarks/NEW_KEV_PREFLIGHT
```

本机报告：`logs/benchmarks/kev-readiness-20260930-05/report.json`，SHA-256 `d2e5bc9f54ff179aa464bea850464000e167f5705690f374ca3a22ca6c73f6f7`。冻结输入 SHA-256 `bdb4be337c9a770c0449def17d76c080a62c80a1ccdec067f7785e72781ac771`。权重、录制输入和原始日志不提交版本库；跨机器复跑需取得相同输入归档。独立环境与 HF 缓存位于 WSL 用户数据/缓存目录，不混入项目源代码。

保留 `-01` 至 `-04` 预检记录：前三次止于检查脚本的换行归一、下载范围和 Xet 缓存名校验问题；Xet blob 名不是文件 SHA，改为验证 Hub LFS 公布的 SHA 后权重一致。`-04` 已完成全部模型回放，未在等待窗口内完成 Windows 探针；`-05` 在 UTF-8 客户端下全部通过。它们不是任务失败样本，不纳入通过率。

## 验证与边界

- Kev 作者 15 项协议测试通过；新增 6 项检查测试通过；正确 Windows 环境下，本次提交包含的 Python 测试 107/107 通过（不混入尚未提交的 Decider 准备测试）。误用系统 Python 时因缺少 `laya` / `browser_harness` 失败，随后在既有浏览器虚拟环境重跑成功。
- Rust 格式检查通过。clippy / workspace test 仍被已有 Windows `std::os::unix` 编译错误及示例配置错误阻断；本轮没有修改 Rust，不宣称全仓 CI 通过。
- 当前证据足以说明该固定模型在本机可运行、能承接原客户端请求、可记录真实前向。下一步仅完成既有 A/B runner 的 Kev 组接入和计量验收，再冻结场次；正式实验仍比较纯 Flash 与 Kev+Flash，保留原任务、原 helper 和独立验收。

## 事实依据

补充（2026-09-30 18:50）：正式 runner 已增加 `--decision-backend kev`，B 使用上述固定模型，A 保持原 Flash。服务预检 `logs/benchmarks/kev-service-smoke-20260930-02/smoke.json` 通过，4 次有效推理、1 次预期 HTTP 500，缓存清理后选择一致且进程正常关闭；均无 Flash 请求和网站任务。新增独立服务账本、开始/结束计数、实际前向、模型身份审计及失联中止逻辑。每题前清空前缀缓存，清理在任务计时外；任务内使用作者默认缓存。服务与客户端耗时是嵌套关系，不能相加两遍。正式条件见[计划](../experiment-fast-model-agent-loop.md)的 Kev 正式接入与对照节。

- [Kev 仓库](https://github.com/jaredpalmer/kev)，本次下载源码 `0fe8fc97c2bcc247fa3efb6e5c32af4e99770e91`。
- [Qwen3 旧版模型卡](https://github.com/jaredpalmer/kev/blob/0fe8fc97c2bcc247fa3efb6e5c32af4e99770e91/docs/model-cards/kev-4b-qwen3.md)。不是官方 Jev 权重，分类结果不能代替浏览器实验。
- [依赖](https://github.com/jaredpalmer/kev/blob/0fe8fc97c2bcc247fa3efb6e5c32af4e99770e91/pyproject.toml)：Python 3.12/3.13，torch >=2.6,<2.9，transformers >=5.17,<6。旧 Decider 的 torch 2.11 环境不复用。
- [加载器](https://github.com/jaredpalmer/kev/blob/0fe8fc97c2bcc247fa3efb6e5c32af4e99770e91/kev/checkpoint.py)：hybrid 专用融合和图捕获只适用于 hybrid；本候选不是该结构，不能把不走 hybrid 分支误报为缺少优化内核。

## 进展

- 2026-09-30 15:14：确认 Ubuntu 24.04/WSL2 可见 RTX 5070 Ti 16GB，驱动 591.86；建立独立环境，原环境保留。源码已下载，模型加载和接口仍未验收。
- 2026-09-30 15:34：固定旧版 4B 权重、依赖和代码，18 条录制输入及 Windows 原客户端探针通过；完成中文环境记录，无 Flash 请求、无正式任务成绩。
