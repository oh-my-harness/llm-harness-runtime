# Laya 浏览器作者方案：首轮实际复现

更新：2026-09-29 19:24 Asia/Shanghai。负责人：lqj。定位：作者方案的单组开发复现，不是速度与质量的 A/B 结论。

后续：已完成[同 16 任务的纯 Qwen / Laya+Qwen 本地对照](laya-browser-paired-control-20260929.md)。本页保留原样单组结果，不与新配对拼接。

## 结论与进展

按 lqj“以作者公布的方案为基准”的要求，撤回自行提出的无损输入路线，实际运行 `cklxx/laya-browser` v19s 发布的包装器、引擎补丁和 Suite A。原始 `verify.py` 通过，随后 **16 个公开任务各执行一次，作者验收通过 13/16（81.25%）**，其余 3 个错误保留在分母。

**可肯定的进展是作者方案已经在本机跑通，决策与生成分工实际发生；尚不能回答相对纯 Qwen 是否更快、质量是否相近。** 本轮没有同条件 Qwen 决策对照；旧 TypeScript 7 任务的 Qwen 7/7、Laya 基座 0/7 不能拿来拼成新 A/B。这也不是 TypeSafe Jev 原模型的复现成绩。

## 量化结果

| 指标 | 实测 |
| --- | ---: |
| 任务 / 重复 / 完成 / 未执行 | 16 / 每题 1 次 / 16 / 0 |
| 作者验收通过率 | 13/16，81.25% |
| 任务期 Laya systemone 请求 | 41 次 |
| Laya 实际 predict / GPU 前向 | 50 / 50 次 |
| predict 内实际问题数，包含分块重复 | 125 个 |
| 任务期 Qwen 实际 HTTP | 5 次，均 200；4 次字段文本、1 次下拉值选择 |
| Qwen 输入 / 输出 token | 3,316 / 38；5 次均返回 usage |
| 工具执行尝试 | 28 次，含 1 次 StalePage 错误后的恢复成本 |
| 全部任务用时合计 / 平均 / 中位数 / p95 | 67.762 / 4.235 / 4.484 / 9.137 秒 |
| 成功任务用时平均 / 中位数 / p95 | 5.020 / 5.079 / 9.137 秒 |
| 失败任务用时平均 / 中位数 / p95 | 0.835 / 0.359 / 1.810 秒 |
| 全部任务累计用时 / 成功数 | 5.212 秒/成功任务 |
| 整场记录墙钟 | 75.110 秒 |

任务用时采用作者 `suite.run` 的 `wall`：从 Agent 构造前开始，包含初始导航、决策、工具、正常路径的 1.5 秒等待、最终页面观测和 Agent 关闭；**在纯 Python 验收表达式执行之前结束**，不是严格到验收结束的计时。异常路径可能跳过等待与最终观测，因此失败得早不能当提速。p95 用 nearest-rank；中位数用标准偶数样本两项平均，作者命令行摘要原用上中位数。整场计时从模型加载前至清理后，不含 Python 导入及源码校验。

本轮模型加载 4.803 秒，任务前 3 次样例预热合计 0.347 秒；上述任务调用数不含预热。含预热共 44 次 systemone 调用、53 次前向。任务期 Laya 服务内响应中位数 32.055 ms，本机决策 HTTP 中位数 41.036 ms；Qwen 请求中位数 724.260 ms。它们输入、输出和职责不同，**不能把两者延迟相除当 Agent 加速比**。嵌套阶段计时也不能直接相加。

| 任务 | 作者验收 | 用时（秒） | Qwen HTTP | Laya 请求 / 前向 | 工具尝试 |
| --- | --- | ---: | ---: | ---: | ---: |
| wiki-einstein | 通过 | 5.079 | 1 | 3 / 3 | 2 |
| wiki-search | 通过 | 9.137 | 1 | 3 / 4 | 2 |
| hn-new | 通过 | 3.593 | 0 | 2 / 4 | 1 |
| hn-login-page | 通过 | 2.501 | 0 | 2 / 3 | 1 |
| gh-issues | 通过 | 4.973 | 0 | 2 / 4 | 1 |
| py-downloads | 通过 | 5.680 | 0 | 2 / 2 | 1 |
| books-travel | 通过 | 5.591 | 0 | 2 / 2 | 1 |
| books-open-book | TypeError，失败 | 0.359 | 0 | 3 / 3 | 3 |
| internet-dropdown | 通过 | 5.952 | 1 | 2 / 2 | 1 |
| internet-checkbox | 通过；另核对实际选中 | 2.719 | 0 | 2 / 2 | 1 |
| books-page2 | TypeError，失败 | 0.335 | 0 | 3 / 3 | 3 |
| quotes-tag-love | 通过 | 3.995 | 0 | 2 / 2 | 1 |
| hn-past | 通过 | 2.764 | 0 | 2 / 4 | 1 |
| web-search | 通过 | 5.775 | 1 | 3 / 3 | 2 |
| arxiv-search | 通过 | 7.499 | 1 | 4 / 5 | 3 |
| flights | TypeError，失败 | 1.810 | 0 | 4 / 4 | 4 |

## 对齐依据与条件

- 作者发布：[cklxx/laya-browser 固定快照](https://huggingface.co/cklxx/laya-browser/tree/645cf366a2ae35f1086e8c20eff48f909bb49206)，v19s、mmBERT-base 322M；不再使用上一轮通用 Laya 基座。
- 引擎：[browser-use/jev-ultrafast@1231850](https://github.com/browser-use/jev-ultrafast/tree/1231850a0bf1a0c0341fe408ef1668dbbfdfac46)，原样应用快照内 [`code/jev-ultrafast.patch`](https://huggingface.co/cklxx/laya-browser/blob/645cf366a2ae35f1086e8c20eff48f909bb49206/code/jev-ultrafast.patch)。未修改作者决策源码或任务。
- 任务：直接导入并调用作者 [`browser_suite.py`](https://huggingface.co/cklxx/laya-browser/blob/645cf366a2ae35f1086e8c20eff48f909bb49206/code/apps/browser_suite.py) 的 `TASKS` 和 `run`，16 题原顺序、每题 1 次，`max_steps=20`。底层保留作者 120 决策预算；本地 Qwen HTTP 总上限 120，实际 5 次。
- 保留作者 1024/768 输入预算、页面文本裁剪、字段摘要、宽候选分块、重复动作过滤及下拉值 LLM 辅助。Suite A 默认不开 planner、escalate、verify_done、macros、SAFE；没有把可选机制全部启用，也没有把作者规则说成 Laya 自身能力。
- Laya 在本机 RTX 5070 Ti 16 GiB、驱动 591.86、CUDA 上运行，53 次前向均记录 `cuda:0`。精度沿用 checkpoint 的 BF16 autocast 配置，未另加量化。采用作者包装器支持的 `fast=False` 默认路径，未启用 TileLang、未修改 GPU 锁频；不宣称复现作者 4070 Ti SUPER 的内核速度数字。
- Windows、Python 3.13.9、torch 2.11.0+cu128、Laya 0.3.21、transformers 5.17.0、browser-harness 0.1.13，独立 Chromium headless shell 153.0.8010.12、隔离配置目录。Python 超出作者项目声明的 `<3.13` 范围，属于环境差异，不称完全同环境复现。
- 保留 lqj 指定的 SiliconFlow `Qwen/Qwen2.5-72B-Instruct`，**不同于作者本地 Qwen3-8B-AWQ**。沿用作者 helper 提示和生成配置；远端部署、量化及排队条件未知。返回 usage 报告 128 个缓存命中 token，未把服务端缓存视为关闭。

## 失败与证据边界

1. 三个失败任务均反复选择 `SCROLL_DOWN` 后报 `TypeError`；flights 另有一次可恢复的 `StalePage`。作者补丁 `_decision_page` 在 excluded 非空而 `_undo_guard()` 为 None 时调用 `undo(a)`，该错误可离线复现。但原 Suite A 吞掉异常堆栈，不能仅凭错误类别断言三次的唯一根因；未为提高分数临时修补或删掉失败题。
2. 作者 checkbox 验收用 `bool(checked)`，而页面可能返回字符串 `"false"`，存在假通过风险。本轮额外检查最终第一个 checkbox 的值为 true，结果仍通过；原始验收值与额外核查并列保留。其他验收按作者 URL、标题、文本或表单状态判定，强度有限，不把模型 DONE 当质量，也不声称已逐题证明全部语义要求。
3. flights 原题日期为 2026-09-28，运行日为 09-29，未擅自改题。这是任务时效性问题，不能由当前错误直接推导它造成了失败。Suite A 与训练域名有重叠，且只有一次重复，不能证明未见任务泛化或质量等价。
4. 清理时记录一次 PermissionError；随后核查专属浏览器、模型监听端口及 browser_harness 进程均已退出。未隐去该异常，未把它计入模型质量失败。

作者模型卡的 Suite A 是 39/48、每题 3 次。这里是 13/16、每题 1 次；即使比例相同，也不是同环境复现其统计稳定性的证据。

前置失败完整保留：`author-01` 缺 HTTP/2 依赖，任务未启动；`author-02` 完整 Chromium 启动报 WinError 14001，16 题未启动；`author-03` 因本地入口额外设置 SAFE 触发 TypeError，只有样例预热、0 次任务模型调用。第三轮 checkbox 被原验收误判通过，**该轮是执行配置错误，不作模型质量成绩**。修复为作者 Suite A 原始默认后才运行上述 `author-04`，不是筛选多个有效成绩后只报最好一次。

## 复核与下一步

入口与用法见 [`author_suite.py`](../../scripts/benchmarks/jev_browser/author_suite.py) 和[运行说明](../../scripts/benchmarks/jev_browser/README.md)。原始文件保留在忽略目录 `logs/benchmarks/laya-browser-author-20260929-04/`，不提交模型、页面日志或凭据。

| 原始文件 | SHA-256 |
| --- | --- |
| report.json | `05cd276483b85771b0a5e1d6dd5a4fd14ce89e65776231c99f3e2d5faf804122` |
| attempts.jsonl | `00f7f0b3943dfbc492514257e96c4b3983a395a1fd2d026e4ac9f397b635f43c` |
| events.jsonl | `bcebf6e7695aafe53f6a9c7b7ce8ae440bf3bdf1562a63343ed36808b1c47294` |
| manifest.json | `d13c85b2c8462100db2224cfda5eb0d46d86f002a8b5cb64745ffaf67972afe2` |

运行时本地入口 SHA-256 为 `c1dda84e11d769c708605152039ca9d32a76b3627dc977a9f62ff462ba879a2f`。运行后仅补全来源散列拒绝、未知 token 标记和严格到验收返回的外层计时；这些新字段没有事后补造进本轮原始结果。作者源码、模型散列在 manifest 内。原始 verify 三次样例调用成功，进程墙钟 7.109 秒，独立于本表任务成绩。

提交前工程验证：69/69 Python、53/53 Node 测试及 Rust fmt/clippy 通过。Rust 全仓仍在既有 `provider_and_error::invalid_api_key_surfaces_as_error` 失败；本轮未修改 Rust，不能声称全仓通过。工程测试不计入模型准确率。

下一步是在该作者基准上定位错误与验收缺陷，再做**同任务、同引擎的 Qwen 决策对照**，保留同等权限、预算和框架规则，避免把大模型可合并的请求人为拆开。作者 Suite A 并未提供这组 Qwen-only 成绩；需要明确标为本地对照扩展，不能冒称作者原对照。当前首先肯定复现链路和分工已经跑通，速度/相近质量假设仍待验证。
