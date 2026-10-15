# jev-browser 准备与来源核查

更新：2026-09-29 18:00 Asia/Shanghai。范围：环境准备、来源审计及适配工程验证，不是模型实验报告。当前主线见[复现计划](../experiment-fast-model-agent-loop.md)。

## 结论

- lqj 已确认按当前公开的 7 个任务准备 Qwen/Laya 配对实验，明确标注适配复现。本轮不调用真实模型，不冒称原版 Jev 的 9 任务成绩复现。
- 锁定依赖、类型检查、Windows 构建和编译产物的真实 Chromium 观测检查通过。
- 原样上游工程测试为 190/191 通过；仅修复测试脚本的 Windows 进程检查后，完整 191/191 通过，首次失败记录保留。这不是任务准确率。
- 已实现 Qwen 同引擎基线、Laya worker 接口、单次计量和独立验收候选版本。新增 33 项 Node 与 8 项 Python 离线测试通过；4 个本地公开任务均在真实 Chromium 中验证了 A/B 执行路径，但使用的是脚本化测试回答，不是模型成绩。
- 已补充[7 任务两轮初态输入审计和具体开发提案](jev-browser-input-audit-20260929.md)：发现一题原样超长，无损紧凑编码可容纳，但余量很小；未调用模型，尚未覆盖完整轨迹。真实模型实测、整场调度与运行确认仍待完成，不能只填密钥就直接启动所需 A/B。

## 1. 固定环境

| 项目 | 本轮实测条件 |
| --- | --- |
| 上游代码 | [dennisonbertram/jev-browser](https://github.com/dennisonbertram/jev-browser/tree/990005a1b9c21258682d6da8cced4c7a9a5bc2da)，提交 `990005a1b9c21258682d6da8cced4c7a9a5bc2da` |
| 本机目录 | 工作区 `benchmark/sources/jev-browser`，独立克隆 |
| 平台 | Windows x64，Node 24.14.1，pnpm 11.19.0 |
| 锁定依赖 | Playwright 1.63.0、TypeScript 5.9.3、tsx 4.23.15、Vitest 4.1.11 |
| Chromium | 153.0.8010.12，独立无头浏览器，不连接用户浏览器 |
| 安装方式 | `pnpm install --frozen-lockfile --ignore-scripts`，锁文件未改动 |
| 锁文件 SHA-256 | 统一 CRLF 为 LF 后：`b01237fdf942a9b32147e726fcbef97bc7a082d7aacfa2a2b65c28097d96ef0a`，避免跨平台行尾影响 |
| 登记的本地上游改动 | [Windows 测试补丁](../../configs/research/jev-browser-windows-tests.patch)只改测试进程枚举；新增[模型接口补丁](../../configs/research/jev-browser-model-hooks.patch)只在 `src/decide.ts` 与 `src/run.ts` 提取请求构造、注入模型接口和阶段计量、暴露原有循环预算，不重写观测和执行器 |
| 准备清单 | [jev-browser-preparation.json](../../configs/research/jev-browser-preparation.json)，7 个名称已通过 TypeScript AST 与固定示例逐项核对，真实运行关闭 |

准备清单不是可执行实验配置。次数、时限、动作预算、质量容忍度和成本仍留空，不自动继承旧实验参数，也不自动切换模型。准备阶段不加载 Laya 权重、不发送真实 Qwen/Jev 请求。

## 2. 原报告材料来自哪里

核查了完整克隆的 80 个可达提交、历史文件清单、当前源码与示例，以及上游公开分支、标签和 issue。当前远端只有 main，没有发布标签；未找到原三组入口、完整 9 任务清单或 81 次原始记录。

更明确的来源证据是：最早的[PROGRESS.md](https://github.com/dennisonbertram/jev-browser/blob/4caf5d36dc921078aae0f8b61c3eadac0a3734c3/PROGRESS.md) 将 81 次对照注明为 Partyline 仓库的 benchmark，并说明本库由该项目抽取而来。因此，当前示例不是原报告材料缺两道题的完整 runner，不能靠自行补题重建原成绩。

公开 [issue #1](https://github.com/dennisonbertram/jev-browser/issues/1) 链接到 `dennisonbertram/partyline-v2`。本轮 GitHub 仓库 API 返回 404；这只证明以当前访问条件无法取得，不能断言它已删除或必然私有。没有代用户向作者发消息或开 issue。

当前 `examples/journeys.ts` 从其[首次提交](https://github.com/dennisonbertram/jev-browser/blob/0dd28f8a9748bfd4a3fdb22789502ccf26b01567/examples/journeys.ts) 起就是 7 个任务。lqj 已确认以它们准备适配实验；原材料缺口不再阻塞这条准备路线，但仍限制与原报告的直接比较。

## 3. 已确认的 7 个任务

| 任务 ID | 来源 | 当前验收边界 |
| --- | --- | --- |
| shadow-dom | 本地 fixture | 表单提交结果中的邮箱 |
| nested-scroll | 本地 fixture | 指定行打开后的标题 |
| cross-origin-iframe | 本地 fixture | 跨源 frame 中的应用结果 |
| select | 本地 fixture | 原生下拉选项的结果文本 |
| wikipedia | 公开网站 | 候选严格检查要求 HTTPS、en.wikipedia.org 和精确 Ada_Lovelace 文章路径；另保留上游路径正则结果 |
| hacker-news | 公开网站 | 记录任务开始时第一条 story 的 ID，终末检查对应评论页；另保留上游任意评论页检查结果 |
| mdn | 公开网站 | 候选严格检查要求 MDN 主机和英文 Array.prototype.map 精确文档路径，允许尾斜杠/锚点；另保留上游宽泛正则结果 |

任务题目和本地 fixture 验收未改。上述公开站点验收是明确披露的适配差异，已实现但尚未冻结为真实运行协议；两组共用，答案不传给模型，不传给 `run.isDone`。保留 `upstream_pass` 和 `strict_pass` 两列，以后者作为候选研究判定，DONE 本身不保证通过。Hacker News 头条 ID 不同的双方成功配对不进入配对提速计算，并单列漂移数；其他公开页面的内容漂移与网络条件仍需在整场运行时补充记录。本轮未运行三个公共网站上的真实模型任务。

## 4. 接口与计量依据

依据固定版本 [run.ts](https://github.com/dennisonbertram/jev-browser/blob/990005a1b9c21258682d6da8cced4c7a9a5bc2da/src/run.ts) 和 [decide.ts](https://github.com/dennisonbertram/jev-browser/blob/990005a1b9c21258682d6da8cced4c7a9a5bc2da/src/decide.ts)：

1. 当前 `run` 直接调用 TypeSafe 决策函数，服务地址为常量；没有现成的可配置 Laya 接口或 Qwen 同引擎决策替换入口。需要最小接口适配，不另写一套浏览器循环。
2. 两组共用观测、候选和执行器。A 不强制拆开可合并的动作/文本输出；B 的 Laya 直接选操作与目标，Qwen 仅负责选定输入框的文本。
3. 当前 `decisions.length` 包含逻辑决策和缓存返回，不等于实际 HTTP/前向次数；决策无效答案重试、HTTP 重试都要在真正发送或前向处计数。
4. `usage.text_calls` 在收到可用非空文本后才增加，不能代表所有文本请求。文本请求的 token 未完整进入 run 的累计 usage；原有文本延迟也在完整响应体解析前计时，需另记完整请求墙钟。
5. `journeys.ts` 只将通过尝试的内部 elapsedMs 纳入中位数。需要保留所有失败耗时、重试与最终验收成本，并单列双方成功配对及覆盖率。
6. 本轮对象仍是示例所用的 `run`。当前库另有 `runTask` 的规划、事实提取与比较功能，不静默接入这条额外路径，以免改变已确认职责。

据此实现了[适配组件](../../scripts/benchmarks/jev_browser/README.md)，职责与计量如下：

- A 的一个 Qwen 请求允许直接返回操作、目标和文本；B 的 Laya 直接决策，只有实际进入选定字段的生成步骤才请求 Qwen。两组保留上游目标缓存和执行循环，不注入题目规则。A 为合并生成而在同一请求中携带可选字段的文本上下文，来自同一观测，不额外给正确答案。
- B 使用上游请求结构、多选择头验证及一次无效答案重试。A/Qwen 的 HTTP 与无效 JSON 重试次数由待冻结参数明确指定，不能假称两边已经冻结成相同重试策略。控制动作和 DONE 不缓存。
- HTTP 逐次记录开始、状态、重试序号、完整响应体解析与验证后的耗时，以及决策/混合/生成的互斥用途。`mixed` 表示请求允许合并生成，不等于最终一定选择填写。Qwen 未返回的概率不臆造，内部兼容占位值不作置信度研究指标。
- Laya 复用既有 SDK 输入完整性审计，在截断、候选折叠或设备回退时拒绝继续；实际 SDK 调用与前向分别记录。进程异常中断后，完整前向次数未知则记 `null`，另保留已观察到的前向开始次数；未知 token 同样不填零。
- 单次任务墙钟从初始页面导航前开始，到终末独立验收或错误结束，含观测、模型、执行、等待、重试；上下文建立、清理及 Laya 加载/预热另记。保留上游内部用时。阶段包含嵌套关系，不能直接相加当端到端用时。
- 汇总保留失败和超时分母，区分全量/成功/失败耗时；配对速度只取双方成功且头条身份可比的配对。当前累计尝试耗时/成功数包含上下文准备和清理，不含整场一次性模型准备；正式整场统计仍待实现。

这些测试证明接口契约与工程计量，不证明真实 SDK 能完整接收全部 7 任务输入，更不证明 Qwen/Laya 的真实质量。下一步核查输入需求，再冻结预算与运行条件。

## 5. 工程验证记录

| 检查 | 结果 |
| --- | --- |
| 原样上游全套测试 | 190 通过、1 失败，191 项；166.31 s |
| 首次失败原因 | `tests/browser.test.ts` 的进程残留检查调用 `ps -axo command`；本机 ps 不支持该参数。此前的启动时限断言已通过，不是模型失败 |
| Windows 测试补丁后全套 | 15 个文件、191 项全部通过；166.14 s |
| 模型接口补丁后回归 | 15 个文件、191 项全部通过；166.38 s；保留上游默认模型路径行为 |
| 新增 Node 适配测试 | 33/33 通过；包含 4 个本地公开任务 × A/B、错误 DONE、截止时间、缓存、重试、完整响应体计时、独立验收和 worker 异常 |
| 新增 Python worker 测试 | 8/8 通过；显式加载门、前向通知、输入拒绝零前向、错误脱敏；未加载权重 |
| 类型检查 | 原样及补丁后均通过 |
| 构建 | `tsc -p tsconfig.build.json` 加复制原样 snapshot 资源；避开原 build 脚本的 POSIX 删除/复制命令，不修改编译配置 |
| 编译产物检查 | 独立 Chromium 加载内存 HTML，observe 找到按钮；未调用模型 |
| 范围与来源核对 | 准备清单的 7 个任务名、锁文件 SHA-256、测试补丁反向适用检查通过 |
| 运行时仓库提交检查 | Rust fmt/clippy 通过；全仓测试仍在既有 `invalid_api_key_surfaces_as_error` 失败，本轮无 Rust 修改 |

上游三轮测试均使用测试替身和真实本地浏览器，运行进程移除了 API 密钥环境变量。新增 Node 测试封锁全局真实 fetch，HTTP 注入替身；Python 测试注入假模型工厂。用时只是工程测试墙钟，不能当 Agent 性能或质量指标。

原始输出位于运行时仓库忽略目录 `logs/benchmarks/setup/jev-browser-preparation-20260929/`：`upstream-tests.log/json`、`windows-tests.log/json`、`model-hooks-tests.log/json`、`runtime-workspace-tests.log`、`model-hooks-runtime-workspace-tests.log`。不覆盖首轮失败，不提交日志、依赖、浏览器或模型文件。运行时 fmt/clippy 再次通过，全仓测试仍在相同既有断言失败，未修改 Rust。

## 6. 环境复现命令

在固定的上游克隆内执行。两个补丁在本机已应用；其他新克隆须先检查，再各应用一次：

```powershell
pnpm install --frozen-lockfile --ignore-scripts
pnpm exec playwright install chromium
git apply --check ../../../llm-harness-runtime/configs/research/jev-browser-windows-tests.patch
git apply ../../../llm-harness-runtime/configs/research/jev-browser-windows-tests.patch
git apply --check ../../../llm-harness-runtime/configs/research/jev-browser-model-hooks.patch
git apply ../../../llm-harness-runtime/configs/research/jev-browser-model-hooks.patch
pnpm run types
pnpm exec tsc -p tsconfig.build.json
Copy-Item -LiteralPath src/snapshot-dom.js -Destination dist/snapshot-dom.js
Get-ChildItem Env: |
    Where-Object { $_.Name -match '(API_KEY|ACCESS_TOKEN|AUTH_TOKEN)$' } |
    ForEach-Object { [Environment]::SetEnvironmentVariable($_.Name, $null, 'Process') }
pnpm run test
```

然后在运行时仓库中执行 `node scripts/benchmarks/jev_browser/check.mjs`，核对构建、补丁、锁文件和 7 任务来源。新增测试命令见[组件说明](../../scripts/benchmarks/jev_browser/README.md)。

这些命令只用于工程准备。没有提供真实任务启动命令，不能把 `examples/journeys.ts` 的原版 Jev 入口直接当成已适配的 Qwen/Laya A/B。
