# 公开实现来源与本地改动

状态（2026-09-29）：本目录是此前基于 `browser-use/jev-ultrafast` 的通用适配与来源记录，不是新主线 `dennisonbertram/jev-browser` 的实现。仅保留作为已验证组件；当前准备与运行门槛以[复现计划](../../../docs/experiment-fast-model-agent-loop.md)为准，不沿用旧实验授权。

来源：[browser-use/jev-ultrafast](https://github.com/browser-use/jev-ultrafast/tree/1231850a0bf1a0c0341fe408ef1668dbbfdfac46)，固定提交 `1231850a0bf1a0c0341fe408ef1668dbbfdfac46`，核查日期 2026-09-29。保留原 MIT 许可证及 Browser Use 版权声明。

这是使用 Jev 的公开浏览器 Agent，不是 Jev 模型权重。这里不包含 TypeSafe 客户端、密钥或 Jev 推理实现，不声称 Laya 与 Jev 在接口性能、输出或质量上等价。

| 文件 | 复用与改动 |
| --- | --- |
| `snapshot.js` | 原样复用，仅统一换行；DOM 可见文本、控件和实际节点引用来自这里 |
| `questions.py` | 原样复用提示语和上游动作上限，仅统一换行 |
| `protocol.py` | 从 `model.py` 提取 `validate_choice`、`action_space`、`field_context`；`choose` 的请求构造部分改名 `build_request`，去掉 TypeSafe 模型名和网络请求，返回相同 state/questions |
| `browser.py` | 保留观测、执行、失效检测及遮挡检查；移除 Browser Harness 的共享浏览器连接和创建/关闭标签逻辑，注入独立 Playwright CDP session；JS 文件显式按 UTF-8 读取 |
| `LICENSE` | 原样保留 |

外部适配在同级 `browser_policy.py`、`isolated_browser.py`、`run_laya_browser.py`，不藏进此目录。区别包括：Laya 多问题 `predict` 代替 TypeSafe HTTP；Qwen 替代文本生成器；新增可单请求选择并生成的 Qwen 基线；独立上下文与网络白名单；模型无自动重试；检查输入裁剪；增加审计、任务时限与终末验收。上游 TYPE_TEXT 描述里的 “small LLM” 是保留的原文，本实验实际仍用已确认的 Qwen2.5-72B。

对齐的是职责和动作协议，不宣称逐字运行未修改的上游程序，也不将其演示速度归属于本实现。上游本身有可见文本 6000 字符、最多 250 个控件动作、近期 10 步决策历史/6 步文本历史等限制，两组共用。Laya 额外输入裁剪会拒绝运行并留痕；框架的观察限制与模型截断是不同层次，不混称“完整网页”。

## 离线核对

从运行时仓库执行，`--checkout` 指向该固定提交的上游克隆：

```powershell
python scripts/benchmarks/verify_jev_reference.py --checkout ../benchmark/sources/jev-ultrafast
```

脚本核对实际 Git blob、原样文件、函数 AST 和浏览器允许改动清单，不只检查仓库 HEAD 名称。测试不访问付费 API。
