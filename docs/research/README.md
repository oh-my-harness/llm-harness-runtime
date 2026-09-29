# 研究文档索引

- [Suite A 纯 Qwen 与 Laya+Qwen 开发对照](laya-browser-paired-control-20260929.md)：最新结果。32 次尝试全部完成，主要验收 A 10/16、B 12/16；9 对共同成功任务速度比 1.591，Qwen 全量请求 49 对 5。披露协议失败、站点/超时、计时资源重叠及弱验收，不宣称正式质量等价。

- [Laya 作者方案首轮实际复现](laya-browser-author-reproduction-20260929.md)：当前执行基准。按固定 v19s 权重、原始包装器/补丁/Suite A 完成 16 题，13/16 通过；Qwen 5 次 HTTP、Laya 50 次前向。原始失败、用时口径和环境差异完整披露；单组结果尚不能证明相对 Qwen 更快且质量相近。

- [Laya 决策契约与浏览器权重核查](jev-browser-laya-contract-audit-20260929.md)：历史离线审计，未发现 choice/policy 映射错误；自拟完整输入加候选分块提案已撤回。后续以作者实现为基准，不将输入完整性检查当模型成绩。

- [jev-browser 开发运行记录](jev-browser-development-20260929.md)：修复首轮编码故障后完成 14 次真实 A/B，Qwen 7/7、Laya+Qwen 0/7；全场 46 次 Qwen 请求，无有效成功配对，不支持当前适配保持相近质量。附环境、耗时、调用、逐题失败与证据局限。

- [jev-browser 初始输入审计与开发试跑提案](jev-browser-input-audit-20260929.md)：7 任务两轮真实初态分词审计，定位 Hacker News 原样超长，验证保留全部内容的紧凑编码；提案随后获 lqj 批准，审计本身不包含模型推理。
- [jev-browser 复现计划](../experiment-fast-model-agent-loop.md)：研究目标不变，当前转为社区浏览器权重作者发布的原始方案；旧 TypeScript 7 任务 A/B 与新 Suite A 单组结果分开保存，不拼接比较。
- [jev-browser 环境准备与来源核查](jev-browser-preparation-20260929.md)：公开 7 任务的同引擎适配、单次计量和独立验收已实现；新增 33 项 Node、8 项 Python 离线测试通过，保留上游工程验证记录。真实模型和 7 任务输入预算尚未验证，不是模型速度或质量结果。

2026-09-29 清理：按 lqj 要求撤下版本库中的 MiniWoB++ 代码、配置、测试和报告。本机忽略目录中的专用产物及源码副本删除被执行策略阻断，待清理路径见计划，不再启用。此前大小生成模型替换、规则模拟和 AppWorld 路线仍不启用；旧结果不作为 jev-browser 复现成果。历史提交不重写，旧进度仅留在归档中。
