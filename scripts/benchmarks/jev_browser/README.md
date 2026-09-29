# jev-browser 同引擎适配

当前是准备组件和离线验证入口，不是已获准启动的模型实验。来源、任务和边界见[中文准备记录](../../../docs/research/jev-browser-preparation-20260929.md)。唯一目标仍是比较速度与完整任务通过率，不替换成规则策略研究。

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

## 真实运行前

准备清单的 `live_run.enabled` 保持关闭。没有提供绕过确认的 live CLI；不能用上游 `examples/journeys.ts` 代替本方案。

后续需核查 7 任务真实输入的完整性，冻结验收、模型/SDK 版本和预算，再实现已获批条件下的整场配对调度、预算执行、逐次落盘和环境记录。不要直接沿用旧预检的上下文长度或输出预算。Qwen 与 Laya 的真实服务能力仍需实测，不以替身测试证明。
