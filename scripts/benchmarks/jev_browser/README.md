# jev-browser 研究入口

## Kev：先验收环境

2026-09-30，固定旧版 `jaredpalmer/kev-4b@qwen3` 的独立 WSL2 环境和原 Windows 客户端接口已通过预检，尚未运行正式 A/B。18 条历史输入共 72 次回放、96 次真实前向，保留原作者 `LayaBrowser` 预处理及候选分块；未调用 Flash 或访问实验网站。

入口为 `kev_preflight.py run --corpus CORPUS --output NEW_OUTPUT`；完成回放后，Windows 用现有浏览器环境执行 `python -X utf8 kev_preflight.py probe --output NEW_OUTPUT`。模型权重须提前缓存，执行时离线，输出目录必须新建。详细版本、计量口径、结果及仍待接入的整场预算见 [Kev 环境准备记录](../../../docs/research/kev-environment-readiness-20260930.md)。不得将预检用时当作正式任务提速，也不把旧版温度 1.0 冒称新版校准结果。

## Decider-2B 三组对照

2026-09-30 经 lqj 确认，`--with-decider` 在原受控 Flash 场次加入 C 组；A/B 原决策及 helper 路径不改。C 以 `Mapika/decider-2b@533964dae8be954c5b5e19fa4948e48408094c1e` v11 代替本地决策打分器，B/C 共用原作者页面/候选压缩和分块。模型原生分词和编码不同，不是参数量的因果消融。

先用 `decider_adapter.py --output logs/benchmarks/NEW_DECIDER_PREFLIGHT` 检验 CUDA、作者原生多候选数值一致性及真实单候选前向。固定权重 SHA-256 为 `acaef2228b134dcdc20cad4ee79219482c927ec819aa3687b9b8a575c338817f`，源码随 Hub 固定快照读取；运行中不联网更新模型。采用 BF16/CUDA eager，无编译/图捕获/schema 缓存；Windows 当前缺少 causal-conv1d 与 flash-linear-attention 的加速实现，参考 PyTorch 路径的速度限制须披露。

```powershell
$env:PYTHONUTF8='1'
python scripts/benchmarks/jev_browser/author_suite.py --paired --controlled --with-decider --text-backend deepseek-flash --repeats 2 --qwen-http-limit 237 --proxy http://127.0.0.1:7890 --output logs/benchmarks/NEW_DECIDER_RUN --chromium $env:CHROMIUM_EXECUTABLE
python scripts/benchmarks/jev_browser/audit_decider_run.py logs/benchmarks/NEW_DECIDER_RUN
```

共 16 题 × 3 组 × 2 轮，A/B 相对顺序不改，C 按题及轮次在首/中/末轮换。Laya 与 Decider 的 RPC、predict、真实前向分别计数；模型调用总用时仅相加 Flash HTTP 和本地 decision HTTP，不累加嵌套阶段。仍从 `DEEPSEEK_API_KEY` 环境变量读取凭据；不得出现在参数、日志或提交中。旧运行保持不变，三组审计使用 `audit_decider_run.py`，不能用旧双组审计漏掉 C。

当前按 lqj 要求以浏览器权重作者公布的方案为基准。来源、任务和边界见[中文计划](../../../docs/experiment-fast-model-agent-loop.md)。唯一目标仍是比较速度与完整任务通过率；旧 7 任务入口只作历史保留，不是当前默认运行路线。

## 当前：作者 Suite A

`author_suite.py` 直接加载 `cklxx/laya-browser@645cf366a2ae35f1086e8c20eff48f909bb49206` 的 `code/apps/laya_browser.py` 和 `browser_suite.py`，调用作者原始 16 项 `TASKS` / `run`。不带 `--paired` 时保留原始单组行为；生成模型仍为指定 Qwen2.5-72B，区别于作者 Qwen3-8B。

前置条件：完整快照位于工作区 `benchmark/models/huggingface/hub/` 标准缓存目录；`benchmark/sources/jev-ultrafast` 为 `1231850a0bf1a0c0341fe408ef1668dbbfdfac46`，已应用快照内 `code/jev-ultrafast.patch`；Python 环境安装 Laya、torch、browser-harness、`httpx[http2]`。入口校验固定源码/模型散列，不下载或更新它们。作者工程声明 Python `<3.13`；本机使用既有 3.13.9 的差异已披露。

从已授权的 `SILICONFLOW_API_KEY` 环境变量读取密钥，禁止写入命令参数、日志或版本库。运行前启用 UTF-8，并通过 `--chromium` 指定实际可启动的 headless shell；输出目录必须是新目录：

```powershell
$env:PYTHONUTF8='1'
python scripts/benchmarks/jev_browser/author_suite.py --output logs/benchmarks/NEW_AUTHOR_RUN --chromium $env:CHROMIUM_EXECUTABLE
python -m unittest discover -s scripts/tests -p test_jev_browser_author_suite.py -v
```

依次完成加载、原始样例三次预热和 Suite A 原顺序各一次；总 Qwen HTTP 上限 120，原任务停止条件不改。记录 `manifest.json`、`events.jsonl`、`attempts.jsonl`、`report.json`；字段 `pass` 保留作者判定，checkbox 另记独立核查值。Laya 请求、predict、前向分开计数，宽候选多阶段不能只算一次。结果与已知作者框架/验收局限见[中文报告](../../../docs/research/laya-browser-author-reproduction-20260929.md)。

## 本地扩展：纯 Qwen 对照

已按 lqj 要求新增 `--paired`。它不是作者发布的现成对照：`author_control.py` 接入相同 Agent 循环，A 为纯 Qwen、B 为作者 Laya+Qwen。A 一次输出动作、目标及必要文本；选择下拉值不额外询问一次 Qwen。目标只允许映射到当前观测候选，不能生成选择器或代码。A 的 one-hot 概率仅用于满足执行器接口，不是模型置信度。

两组共用一处 `undo=None` 判空修复，不改变已有重复动作过滤；以运行时子类覆盖，不修改上游副本或作者缓存文件。16 题原顺序按 A/B、B/A 交替，各一次；每次使用独立浏览器上下文，保留原始目标（包括已过期的航班日期）。共享原始步骤/决策预算，180 秒协作式边界检查，整场 Qwen HTTP 上限 120。

```powershell
python scripts/benchmarks/jev_browser/author_suite.py --paired --output logs/benchmarks/NEW_PAIRED_RUN --chromium $env:CHROMIUM_EXECUTABLE
python -m unittest discover -s scripts/tests -p 'test_jev_browser_author*.py' -v
```

`author_pass` 保留原验收，主要 `pass` 同时排除运行错误并修正 checkbox 字符串假通过。严格墙钟含最终验收，另存作者 wall；新建/销毁隔离上下文单列。A 使用框架完整可见观测，B 保留作者模型内部压缩，属于两种完整方案比较，不声称输入编码完全相同。运行源文件、散列、初态散列、实际调用、前向和异常栈位置均保存；模型 DONE 不单独作为通过依据。

首轮实测见[中文对照报告](../../../docs/research/laya-browser-paired-control-20260929.md)。`audit_author_run.py RUN_DIRECTORY` 只读重算逐次计数、配对统计、按组 token 和敏感性结果，不加载模型、不调用远端 API、不修改原始日志。工程测试必须在计时实验之前完成，不与真实实验同时运行。

### 固定条件的三轮重跑

`--controlled --repeats 3 --proxy http://127.0.0.1:7890` 在同一 16 题上运行 48 对、96 次尝试，轮次翻转先后顺序；请求上限为 360。浏览器显式使用本地代理，Qwen 使用显式 HTTPX transport，本地模型与 CDP 绕过代理。两组全部 Qwen 请求统一 temperature=0、top_p=1、max_tokens=1024 和 JSON 输出；不改变作者 Laya 预处理或 helper 提示。

A 的协议明确合法操作/目标，并仅允许整数目标按同值字符串正规化，仍禁止猜测和越界目标。完整请求体、响应原文及异常栈在本地日志记录，凭据脱敏。相同任务的不同轮次分别配对；只读审计同时检查公共参数和 Qwen 路由没有漂移。工程测试须先全部结束再运行：

受控模式的 429 重试在两组统一等待 30/60 秒，最多三次 HTTP；有效 Retry-After 可延长至最多 120 秒，等待全部计入任务墙钟并记录为 `provider_backoff`。其他 503/529 沿用作者 0.5/1 秒重试。可用 `--qwen-http-limit` 显式收紧预算；本次在保留限流中止记录后，第二次完整运行只剩 237 次 HTTP 预算，不能静默扩额。

```powershell
python scripts/benchmarks/jev_browser/author_suite.py --paired --controlled --repeats 3 --proxy http://127.0.0.1:7890 --output logs/benchmarks/NEW_CONTROLLED_RUN --chromium $env:CHROMIUM_EXECUTABLE
```

这仍是作者方案与合理纯 Qwen 基线的整体部署比较，不是最终输入完全相同的模型消融；原任务和旧结果均不覆盖。

实测范围及结论见[固定条件重跑报告](../../../docs/research/laya-browser-controlled-comparison-20260929.md)：当前 89/96 次，预算停止。只读审计输出 `round_analysis`，按任务覆盖度提取完整轮次，并列给出全部成功配对与无 HTTP 失败成功配对，不从原始文件删除记录。

### DeepSeek Flash 同条件替换

Kev 对照可在该命令增加 `--decision-backend kev`，替换 B 的 Laya，不增加 C 组。须先按 [Kev 环境记录](../../../docs/research/kev-environment-readiness-20260930.md)完成固定 WSL 环境，将 `KEV_WSL_PYTHON` 与 `KEV_HF_HOME` 设为 WSL 中已验收的绝对路径；`KEV_WSL_DISTRO` 默认为 Ubuntu-24.04。运行前可用 `python -X utf8 scripts/benchmarks/jev_browser/kev_service.py --smoke --output logs/benchmarks/NEW_KEV_SMOKE --port 18792` 检查原 Windows 客户端、错误计量和进程关闭，不调用 Flash。正式入口自动启动/关闭本地服务并保存 `kev-events.jsonl`；`audit_author_run.py` 对账独立服务日志和任务计数。失联停止场次，失败与未执行项不删除。`qwen_*` 仍是 Flash 的历史计数字段；Kev 单独使用 `kev_rpc/predict/forward`，不会计成 Laya。

按 lqj 授权，以 `--text-backend deepseek-flash` 只替换两组的远端文本模型，保持上述作者模型、任务、提示、执行器和验收不变。密钥从 `DEEPSEEK_API_KEY` 读取；仅使用官方 `https://api.deepseek.com`，两组均显式 `thinking.type=disabled`，其余受控请求参数、代理和重试不变。原 Qwen 模式仍是默认值。

```powershell
python scripts/benchmarks/jev_browser/author_suite.py --paired --controlled --text-backend deepseek-flash --repeats 2 --qwen-http-limit 237 --proxy http://127.0.0.1:7890 --output logs/benchmarks/NEW_FLASH_RUN --chromium $env:CHROMIUM_EXECUTABLE
```

为避免修改旧分析和原始文件，`qwen_http`、`qwen_*_tokens`、`A_qwen` 等历史计数字段暂沿用，含义为所选文本模型的请求/用量；`text_model`、`text_api_base` 与实际请求体才标识本轮模型，不能在报告中将 Flash 请求写成 Qwen。只读审计同时核对模型名与模式，旧 Qwen 数据仍可复算；计数不含连通性诊断，模型预热另列。

以下章节为上一轮 TypeScript 7 任务适配的历史用法。

## 已接入的职责

- `source.mjs` 验证固定提交、锁文件和两个登记补丁，构建上游引擎；只提取 7 个任务声明，不导入会启动原版 Jev 请求的示例主程序。
- `policy.mjs`：A 用 Qwen 一次返回操作、目标及必要文本；B 保留上游多选择头及验证，让 Laya 选操作与目标，Qwen 只生成已选字段文本。相同候选，不增加规划器或规则决策器。
- `qwen.mjs` 仅接受既定 Qwen 模型与 SiliconFlow 端点，从环境读取密钥，逐次记录真实 HTTP 尝试，禁止重定向携带凭据。
- `laya.mjs` / `laya_worker.py`：持久 Python worker，复用已核查的 Laya 输入审计与 CUDA 路径；加载、预热在任务外单列。模型代码和权重来自本地既有缓存，不自动下载或换模型。
- `attempt.mjs`：新浏览器上下文中的单次尝试；`acceptance.mjs` 独立验收，不向模型或上游 `isDone` 提供答案。调用方必须显式给出预算、已初始化模型及新的独立 Trace。
- `metrics.mjs`：完整响应体用时、失败与重试、未知 token/前向、工具派发、缓存和失败分母。嵌套阶段不能相加当墙钟。

## 离线验证

在运行时仓库根目录执行，依赖固定上游克隆已经安装。新克隆先按准备记录应用补丁。

```powershell
node scripts/benchmarks/jev_browser/check.mjs
node --test scripts/tests/jev_browser/*.test.mjs
python -m unittest discover -s scripts/tests -p test_jev_browser_worker.py -v
```

测试中的脚本化回答只用于检查调用契约；真实 Chromium 执行 4 个本地公开任务的两组路径。它们不加载 Laya 权重、不访问 Qwen 服务，不产生研究准确率或速度结论。

已完成运行的 B 轨迹可用 `node scripts/benchmarks/jev_browser/audit-traces.mjs RUN_DIRECTORY` 只读复核：请求散列、全部选择头的概率与键、输入审计记录及 policy 选择一致性。当前支持未缓存且成功返回的 B 决策；不加载模型，不重新判定 DOM 或任务成功。

浏览器专用候选的离线 tokenizer/格式检查入口为 `audit_browser_checkpoint.py`，具体命令与固定下载范围见[核查报告](../../../docs/research/jev-browser-laya-contract-audit-20260929.md)。它不改变下方 live 入口的基座权重与行为；不得把格式检查结果当作已加载新模型或已验证决策质量。

## 历史：已结束的 7 任务真实运行

已批准公开 7 任务各一对，共 14 次；顺序按任务交替 A/B、B/A，单次 180 秒、30 动作、60 决策，整场 3,600 秒。Qwen 全场实际 HTTP 上限 120，含最多 2 次预检请求与全部重试，这不是金额上限。只作开发描述，不声称质量等价。

从环境变量 `SILICONFLOW_API_KEY` 读取用户已授权的凭据，不放进参数、配置、日志或提交。输出目录必须不存在：

```powershell
node scripts/benchmarks/jev_browser/session.mjs --live --output logs/benchmarks/NEW_DEVELOPMENT_RUN
```

入口先验证固定源代码和批准参数，再加载本地 Laya、预热一次、测试已冻结的 7 个初态和 Qwen 连通性；预检任一失败即停止，不进入任务矩阵。worker 使用与输入审计相同的无损紧凑序列化，8192/4096，每次真实决策前仍审计全部输入，不删候选。CPU 前向被禁止；SDK 内部尝试设备回退也会记录为错误。

`manifest.json` 记录代码散列、环境、模型、顺序和预算；`setup-events.jsonl` 单列加载、预热、预检；每次尝试单独事件文件，`attempts.jsonl` 即时落盘；`report.json` 保留失败分母、未开始项、逐题数据、双成功速度及调用量。任务计时包含初始导航、输入转换、审计、最终验收，上下文准备及清理另列；整场墙钟包含全部成本。未知 token/前向保持未知。

开发结果不能直接视作原版 Jev 成绩；初态能容纳也不保证完整轨迹始终能容纳。不能用上游 `examples/journeys.ts` 代替本入口，更不能用旧 MiniWoB 产物补成绩。
