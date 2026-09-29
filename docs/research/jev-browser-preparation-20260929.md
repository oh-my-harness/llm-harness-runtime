# jev-browser 准备与来源核查

更新：2026-09-29 16:57 Asia/Shanghai。范围：环境准备和公开材料审计，不是模型实验报告。当前主线见[复现计划](../experiment-fast-model-agent-loop.md)。

## 结论

- lqj 已确认按当前公开的 7 个任务准备 Qwen/Laya 配对实验，明确标注适配复现。本轮不调用真实模型，不冒称原版 Jev 的 9 任务成绩复现。
- 锁定依赖、类型检查、Windows 构建和编译产物的真实 Chromium 观测检查通过。
- 原样上游工程测试为 190/191 通过；仅修复测试脚本的 Windows 进程检查后，完整 191/191 通过，首次失败记录保留。这不是任务准确率。
- 尚未完成 Qwen 同引擎基线、Laya 决策接入、统一计量和真实运行参数冻结；当前不能只填密钥就直接启动所需 A/B。

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
| 唯一本地上游改动 | [Windows 测试补丁](../../configs/research/jev-browser-windows-tests.patch)，只改 `tests/browser.test.ts` 中的进程枚举，不改生产代码或断言 |
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
| wikipedia | 公开网站 | 目标文章 URL |
| hacker-news | 公开网站 | 上游只检查是否为评论页，不能充分验证“头条”身份 |
| mdn | 公开网站 | 上游 URL 正则较宽，不能充分验证目标 API 文档 |

本轮没有改题、改验收或把验收答案传给模型。实施时需解决后两项验收不足，明确记录与上游的差异，再冻结 A/B 共用判定；不能以宽松检查的成功代替任务真正完成。公开站点的内容与网络变化也须记录。

## 4. 实施前必须解决的接口与计量

依据固定版本 [run.ts](https://github.com/dennisonbertram/jev-browser/blob/990005a1b9c21258682d6da8cced4c7a9a5bc2da/src/run.ts) 和 [decide.ts](https://github.com/dennisonbertram/jev-browser/blob/990005a1b9c21258682d6da8cced4c7a9a5bc2da/src/decide.ts)：

1. 当前 `run` 直接调用 TypeSafe 决策函数，服务地址为常量；没有现成的可配置 Laya 接口或 Qwen 同引擎决策替换入口。需要最小接口适配，不另写一套浏览器循环。
2. 两组共用观测、候选和执行器。A 不强制拆开可合并的动作/文本输出；B 的 Laya 直接选操作与目标，Qwen 仅负责选定输入框的文本。
3. 当前 `decisions.length` 包含逻辑决策和缓存返回，不等于实际 HTTP/前向次数；决策无效答案重试、HTTP 重试都要在真正发送或前向处计数。
4. `usage.text_calls` 在收到可用非空文本后才增加，不能代表所有文本请求。文本请求的 token 未完整进入 run 的累计 usage；原有文本延迟也在完整响应体解析前计时，需另记完整请求墙钟。
5. `journeys.ts` 只将通过尝试的内部 elapsedMs 纳入中位数。需要保留所有失败耗时、重试与最终验收成本，并单列双方成功配对及覆盖率。
6. 本轮对象仍是示例所用的 `run`。当前库另有 `runTask` 的规划、事实提取与比较功能，不静默接入这条额外路径，以免改变已确认职责。

上述是待实施项，不声称适配或计量已经完成。先完成接口契约、输入完整性与离线测试，再冻结具体运行条件。

## 5. 工程验证记录

| 检查 | 结果 |
| --- | --- |
| 原样上游全套测试 | 190 通过、1 失败，191 项；166.31 s |
| 首次失败原因 | `tests/browser.test.ts` 的进程残留检查调用 `ps -axo command`；本机 ps 不支持该参数。此前的启动时限断言已通过，不是模型失败 |
| Windows 测试补丁后全套 | 15 个文件、191 项全部通过；166.14 s |
| 类型检查 | 原样及补丁后均通过 |
| 构建 | `tsc -p tsconfig.build.json` 加复制原样 snapshot 资源；避开原 build 脚本的 POSIX 删除/复制命令，不修改编译配置 |
| 编译产物检查 | 独立 Chromium 加载内存 HTML，observe 找到按钮；未调用模型 |
| 范围与来源核对 | 准备清单的 7 个任务名、锁文件 SHA-256、测试补丁反向适用检查通过 |
| 运行时仓库提交检查 | Rust fmt/clippy 通过；全仓测试仍在既有 `invalid_api_key_surfaces_as_error` 失败，本轮无 Rust 修改 |

两轮测试均使用上游测试替身和真实本地浏览器，运行进程移除了 API 密钥环境变量。用时只是工程测试墙钟，不能当 Agent 性能或质量指标。

原始输出位于运行时仓库忽略目录 `logs/benchmarks/setup/jev-browser-preparation-20260929/`：`upstream-tests.log/json`、`windows-tests.log/json`、`runtime-workspace-tests.log`。不覆盖首轮失败，不提交日志、依赖、浏览器或模型文件。

## 6. 环境复现命令

在固定的上游克隆内执行。Windows 测试补丁在本机已应用；其他新克隆须先检查，再仅应用一次：

```powershell
pnpm install --frozen-lockfile --ignore-scripts
pnpm exec playwright install chromium
git apply --check ../../../llm-harness-runtime/configs/research/jev-browser-windows-tests.patch
git apply ../../../llm-harness-runtime/configs/research/jev-browser-windows-tests.patch
pnpm run types
pnpm exec tsc -p tsconfig.build.json
Copy-Item -LiteralPath src/snapshot-dom.js -Destination dist/snapshot-dom.js
Get-ChildItem Env: |
    Where-Object { $_.Name -match '(API_KEY|ACCESS_TOKEN|AUTH_TOKEN)$' } |
    ForEach-Object { [Environment]::SetEnvironmentVariable($_.Name, $null, 'Process') }
pnpm run test
```

这些命令只用于工程准备。没有提供真实任务启动命令，不能把 `examples/journeys.ts` 的原版 Jev 入口直接当成已适配的 Qwen/Laya A/B。
