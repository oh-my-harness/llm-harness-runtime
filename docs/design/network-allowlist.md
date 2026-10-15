# 沙箱网络 allowlist 设计

## 1. 背景与目标

Linux gateway 当前使用 Bwrap 的默认断网模式。`net_allowlist` 为空时可以阻止普通 outbound 流量；非空时 Bwrap 只有“打开整个 host 网络”和“完全断网”两种能力，因此 gateway 会拒绝非空 allowlist。

这个设计要解决的边界是：沙箱内运行的 shell、代码或 guest agent 都是**不可信进程**。网络策略必须由宿主侧强制执行，不能依赖进程读取环境变量、遵守 HTTP proxy 或调用 gateway 提供的 SDK。

## 2. 结论

- 空网 allowlist 继续等于默认拒绝，并保持 Bwrap `--unshare-net`。
- 非空 allowlist 使用**每个 sandbox 独立的 Linux network namespace**，不打开 host network namespace。
- namespace 内使用 nftables 默认拒绝，只放行明确授权的目标 IP、CIDR 和端口。
- 宿主通过 veth 把网络交给 sandbox；gateway 不复用 veth、namespace 或 network identity。
- DNS 作为一等策略处理，不把“允许 example.com”翻译成“允许任意解析结果”。
- 不支持精确允许域名当前返回的 DNS A/AAAA 记录时，必须拒绝该规则，不能降级为按第一个 IP 放行或打开全部网络。

## 3. 策略语义

`NetRule.host` 当前是字符串，需要明确解释：

| 配置值 | 含义 | 实现 |
|---|---|---|
| IPv4/IPv6 literal | 允许该精确地址 | 直接生成地址规则 |
| IPv4/IPv6 CIDR | 允许网段 | 生成网段规则 |
| 域名 | 允许该域名当前可解析的地址 | 在沙箱创建前由 gateway 解析，只放行解析结果；解析结果为空或包含私有/链路本地/保留地址时拒绝 |
| 端口范围 | 只作用于 IP/CIDR 目标端口 | 由 nftables 匹配 |

当前 v0 规则只有目标 host 和端口，没有协议字段。因此实现不得把规则解释为“任意协议、任意方式可达”，而应明确这是**目的地和端口过滤**。如果后续需要区分 TCP、UDP、ICMP，应先扩展 `NetRule`，不能复用现有字段做隐式升级。

DNS 有两个独立问题：

1. **解析控制**：namespace 内只应指向 gateway 部署者指定的 DNS resolver；不要继承 `/etc/resolv.conf`。
2. **出口目标控制**：域名规则必须在创建时解析并只放行解析结果。对 HTTP URL 中的 host，应用层必须在沙箱外解析；不允许 shell 自行解析后访问任意 IP。

如果业务需要长时间运行且 DNS 结果可能变化，v1 应使用受控 egress proxy；静态 netns 规则只适合“启动时目标集合已知”的场景。

## 4. Namespace 与身份隔离

每个非空网络沙箱获得：

1. 唯一 namespace 名；
2. 唯一 veth pair；
3. 独立网段，避免与其他 sandbox 冲突；
4. 独立 nftables table 和规则集；
5. 独立生命周期记录，绑定 sandbox id。

host 侧只把该 veth 的 peer 移入该 sandbox 的 namespace。进程只能访问自己的 namespace。即使沙箱内进程变成 UID 0，它也没有宿主的 `CAP_NET_ADMIN`，不能修改宿主接口或另一个 namespace 的规则。

部署方式有两种，推荐第二种：

- **方案 A**：gateway 自身持有 `CAP_NET_ADMIN`。实现简单，但 gateway 进程被攻破后影响面变大。
- **方案 B**：宿主上运行一个很小的 network manager，只接受 gateway 通过 Unix socket 发送的 create/delete 请求；gateway 保留最小身份和配额控制，netns/veth/nftables 由 manager 执行。生产部署优先选择方案 B。

manager 必须以 sandbox id 作为幂等键。创建、恢复、删除时都要清理旧接口和旧 nftables 规则；gateway 崩溃后不能把旧 namespace 当作新 sandbox 的网络身份。

## 5. 规则生成

对每个 sandbox：

1. host 链默认 `drop`；
2. 只放行 loopback 的必要 resolver/本地代理流量；
3. 对每条授权规则生成目标地址和端口匹配；
4. 拒绝重用到本机服务、其他 sandbox 网段、link-local、 multicast、未指定地址和私有地址，除非该网段被显式授权；
5. 用一次 nftables transaction 原子应用全部规则。

nftables 按目的地址过滤可以防 redirect、proxy 误配和大部分 namespace 逃逸，但有两个边界必须写清：

- TLS SNI 和 HTTP Host 不由 nftables 校验。对域名做应用层校验需要后续加 egress proxy，v1 不应宣称已做。
- 如果允许了宽 CIDR 或公网 DNS IP，DNS 响应可以把流量引导到已放行 IP 之外；由于 nftables 仍按目的地址拒绝，攻击者不能借此打开其他目标。

## 6. IPv6

IPv6 与 IPv4 同等级，不接受“IPv6 稍后处理”：

- IPv6 literal/CIDR 生成 `ip6` 规则；
- 域名解析必须检查 AAAA 记录；
- namespace 内禁用 router advertisement 或使用受控 resolver，防止宿主路由自动把流量带出策略；
- 缺少 IPv6 支持时必须 fail-closed，而不是静默只配置 IPv4。

## 7. Redirect 与代理

kernel 层规则防不了应用层 redirect：

- 允许目标端口后，对端仍可能返回 302；
- HTTP Host 和 TLS SNI 仍可能指向另一个域名；
- shell 里的任意协议也不能被 HTTP proxy 环境变量约束。

因此文档和 API 响应必须区分：

- **network allowlist**：按 IP/CIDR/端口控制可达性；
- **HTTP egress proxy**：控制 HTTP/HTTPS 语义、域名、SNI、重定向和认证；
- **应用层域名策略**：必须由 proxy 强制，不能伪装成 netns 规则。

第一版可以只交付 netns + nftables。HTTP 级域名策略作为后续能力，不得用设置 `HTTPS_PROXY` 冒充强制执行。

## 8. 生命周期

创建：

1. gateway 校验 principal、quota、网络规则和目标地址；
2. 调用 manager 创建 netns/veth；
3. 应用 nftables 规则；
4. 以 `--share-net` 启动 Bwrap，并注入该 namespace；
5. 失败时按相反顺序清理。

运行：

- gateway 记录 netns、veth 和 nftables 资源标识；
- sandbox 删除、超时、进程退出或 gateway 停止时销毁 namespace；
- registry 恢复时若找不到或无法验证 netns，记录进入 unavailable/quarantine，不得自动当作 Running。

重启：

1. 枚举本 gateway 的 namespace；
2. 逐一检查记录、接口和规则是否完整；
3. 不能恢复的记录清理网络资源，并保留审计事件；
4. 不允许把一个 sandbox 的网络资源改绑给另一个 sandbox。

## 9. 测试

至少需要覆盖：

- 空 allowlist 时 DNS、TCP、UDP、ICMP 都不可达；
- 非 literal 域名规则解析失败时拒绝创建；
- 域名规则只放行 A/AAAA 解析结果，不自动放行宿主网络；
- IPv4 和 IPv6 的 allowed/denied 目标分别测试；
- 端口范围只作用于授权地址；
- 沙箱内进程不能读取或修改 host namespace；
- 两个 sandbox 不能互相访问对方网段或复用接口；
- 进程崩溃、gateway 崩溃、manager 重启后的资源回收；
- `--share-net` 下默认链仍为 deny。

需要真实 Linux namespace 的集成测试应默认跳过，CI 单元测试不能把静态配置检查当成流量验证。

## 10. 交付拆分

1. **manager 协议与生命周期模型**：定义 create/get/delete/reconcile，不接入 Bwrap；
2. **netns + nftables manager**：实现幂等创建和清理；
3. **Bwrap 接入**：非空规则走专用 namespace，空规则保持当前断网；
4. **gateway 配置与审计**：记录 principal 授权、解析结果、规则版本和资源释放；
5. **E2E**：在 Linux host 上跑允许/拒绝、IPv6、崩溃恢复测试。

在 1–4 完成前，`net_allowlist` 继续拒绝非空规则。这个设计不把 nftables 缺失、权限不足或 DNS 失败降级成“全网络放行”。
