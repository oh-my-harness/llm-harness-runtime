# jev-browser 同引擎适配

当前提供已获 lqj 批准的开发运行入口，以及独立的离线验证入口。来源、任务和边界见[中文计划](../../../docs/experiment-fast-model-agent-loop.md)。唯一目标仍是比较速度与完整任务通过率，不替换成规则策略研究。

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

## 获批的真实运行

已批准公开 7 任务各一对，共 14 次；顺序按任务交替 A/B、B/A，单次 180 秒、30 动作、60 决策，整场 3,600 秒。Qwen 全场实际 HTTP 上限 120，含最多 2 次预检请求与全部重试，这不是金额上限。只作开发描述，不声称质量等价。

从环境变量 `SILICONFLOW_API_KEY` 读取用户已授权的凭据，不放进参数、配置、日志或提交。输出目录必须不存在：

```powershell
node scripts/benchmarks/jev_browser/session.mjs --live --output logs/benchmarks/NEW_DEVELOPMENT_RUN
```

入口先验证固定源代码和批准参数，再加载本地 Laya、预热一次、测试已冻结的 7 个初态和 Qwen 连通性；预检任一失败即停止，不进入任务矩阵。worker 使用与输入审计相同的无损紧凑序列化，8192/4096，每次真实决策前仍审计全部输入，不删候选。CPU 前向被禁止；SDK 内部尝试设备回退也会记录为错误。

`manifest.json` 记录代码散列、环境、模型、顺序和预算；`setup-events.jsonl` 单列加载、预热、预检；每次尝试单独事件文件，`attempts.jsonl` 即时落盘；`report.json` 保留失败分母、未开始项、逐题数据、双成功速度及调用量。任务计时包含初始导航、输入转换、审计、最终验收，上下文准备及清理另列；整场墙钟包含全部成本。未知 token/前向保持未知。

开发结果不能直接视作原版 Jev 成绩；初态能容纳也不保证完整轨迹始终能容纳。不能用上游 `examples/journeys.ts` 代替本入口，更不能用旧 MiniWoB 产物补成绩。
