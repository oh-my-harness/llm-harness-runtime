# jev-browser 研究入口

当前按 lqj 要求以浏览器权重作者公布的方案为基准。来源、任务和边界见[中文计划](../../../docs/experiment-fast-model-agent-loop.md)。唯一目标仍是比较速度与完整任务通过率；旧 7 任务入口只作历史保留，不是当前默认运行路线。

## 当前：作者 Suite A

`author_suite.py` 直接加载 `cklxx/laya-browser@645cf366a2ae35f1086e8c20eff48f909bb49206` 的 `code/apps/laya_browser.py` 和 `browser_suite.py`，调用作者原始 16 项 `TASKS` / `run`。保留作者默认预处理、候选处理和 helper 分工，只适配本机服务与计量。生成模型仍为指定 Qwen2.5-72B，区别于作者 Qwen3-8B；这是单组复现，还没有同条件 Qwen-only 结果。

前置条件：完整快照位于工作区 `benchmark/models/huggingface/hub/` 标准缓存目录；`benchmark/sources/jev-ultrafast` 为 `1231850a0bf1a0c0341fe408ef1668dbbfdfac46`，已应用快照内 `code/jev-ultrafast.patch`；Python 环境安装 Laya、torch、browser-harness、`httpx[http2]`。入口校验固定源码/模型散列，不下载或更新它们。作者工程声明 Python `<3.13`；本机使用既有 3.13.9 的差异已披露。

从已授权的 `SILICONFLOW_API_KEY` 环境变量读取密钥，禁止写入命令参数、日志或版本库。运行前启用 UTF-8，并通过 `--chromium` 指定实际可启动的 headless shell；输出目录必须是新目录：

```powershell
$env:PYTHONUTF8='1'
python scripts/benchmarks/jev_browser/author_suite.py --output logs/benchmarks/NEW_AUTHOR_RUN --chromium $env:CHROMIUM_EXECUTABLE
python -m unittest discover -s scripts/tests -p test_jev_browser_author_suite.py -v
```

依次完成加载、原始样例三次预热和 Suite A 原顺序各一次；总 Qwen HTTP 上限 120，原任务停止条件不改。记录 `manifest.json`、`events.jsonl`、`attempts.jsonl`、`report.json`；字段 `pass` 保留作者判定，checkbox 另记独立核查值。Laya 请求、predict、前向分开计数，宽候选多阶段不能只算一次。结果与已知作者框架/验收局限见[中文报告](../../../docs/research/laya-browser-author-reproduction-20260929.md)。

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
