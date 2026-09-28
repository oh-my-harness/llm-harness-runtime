# 大小模型 Agent 循环速度与准确率实验

## 实验目的

在相同 ReAct 循环、任务、提示词、工具、预算和独立验收标准下，测量本地小模型比云端大模型快多少，以及准确率是否下降。

- 大模型：硅基流动托管的 Qwen2.5-72B-Instruct。
- 小模型：RTX 5070 Ti 上通过 Ollama 运行的 Qwen3.5-4B。

记录任务总耗时、模型和工具的单次及累计耗时、调用次数、验收准确率和失败情况。所有失败均计入准确率分母，同时区分成功与失败的耗时。

本实验比较实际部署方案，网络、量化和推理后端也会影响结果，不能把全部差异归因于模型大小。

## 文档与入口

- [最新 20 题实验报告](research/2026-09-29-paired-files-results.md)：完整环境、参数、任务条件、速度与准确率、失败原因、数据及复现命令。
- [通用口径与早期实验记录](research/2026-09-28-local-decision-benchmark.md)：指标定义及历史三题结果。
- 运行脚本：`scripts/benchmark_jev_real.mjs`。
- 固定任务集：`scripts/benchmarks/tasks-v1.json`。
- 数据汇总：`scripts/analyze_agent_benchmark.mjs`。

模拟结果不作为本实验真实模型速度或准确率的证据。
