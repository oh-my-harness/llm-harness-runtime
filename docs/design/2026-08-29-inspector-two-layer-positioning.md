# Inspector 两层定位决策（ADR）

> 日期：2026-08-29
> 状态：已采纳
> 关联：`2026-08-22-inspector-web-api-design.md`（原始定位）

## 背景

Inspector 的原始定位（2026-08-22 设计）是：开发者调试工具、只提供 Web API、
不绑定前端实现、任何基于 runtime 的 agent 应用都能复用。

此后它事实上演化出三个部分：

1. **观测面**（events WS / session / config / usage）——通用组件定位成立；
2. **控制面**（`/api/inject`、`/api/abort`、`/api/prompt`）——已越过"纯只读观测"；
3. **UI 托管**（static UI 含状态逻辑：History 回放、agent 列表轮询、agent 切换）。

当 bench、coding-team 等上层应用都需要复用它时，"整箱复用"模式不可持续：
上层功能要么写进 inspector（污染定位），要么 fork（失去共享演进）。

## 决策

Inspector 定位为**平台层**，上层应用**挂载**而非修改它：

- **平台层（`llm-harness-inspector`）**：harness registry、events WS 推送、
  session/config/usage 端点、inject/abort/prompt 控制面、静态 UI 托管、
  token 鉴权。不包含任何领域逻辑。
- **应用层（如 `llm-harness-coding-team`，后续可能拆到独立仓库）**：
  领域数据（项目/角色/issue）与领域路由，通过扩展点挂进 inspector server。

### 扩展点（inspector 唯一需要的改动）

- `InspectorBuilder::extra_routes(Router)`：上层应用把自己的 axum `Router`
  合并进 inspector server（受同一 token 鉴权保护）。
- `InspectorBuilder::extra_static(前缀, 目录)`：上层应用的静态页面托管。

### UI 层

前端同样分两层：inspector 的 static UI 提供 agent 观测基础面板；
上层应用在其上扩展自己的页面/面板（如 coding-team 的项目列表、
对话面板、issue 确认页），但前端代码不进 inspector crate。

## 推论

- coding-team、bench 的领域功能（对话 API、issue 流转、报告面板）**永不写入**
  inspector crate。
- inspector 的 API 兼容性以平台层接口为承诺边界；扩展点本身属于平台层契约。
- 应用层 crate（coding-team）在本仓库实现，成熟后拆分到独立仓库；
  拆分时只带走挂载关系，不动平台层。
