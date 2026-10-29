# Spec Delta

## MODIFIED Requirements

### Requirement: G-17

JchTools MUST 遵守本条行为：在同一 Windows 用户、同一仓库及同一继承环境下，使用系统 Git 原有的系统级、用户级、仓库级及 include / includeIf 配置和优先级，保留用户身份、认证助手、SSH、签名、hooks、过滤器与合并策略；Git HTTP(S) 网络操作优先遵守适用于当前目标的 Git 显式代理配置及继承环境中的代理或直连策略，未显式配置时使用 Windows 当前用户的手动系统代理。

#### Scenario: G-17 完整行为与适用边界
- **WHEN** 用户通过 Git 工具处理所选仓库的逐文件提交与推送，且涉及本条规定的条件
- **THEN** 配置与系统 Git 一致：在同一 Windows 用户、同一仓库及同一继承环境下，使用系统 Git 原有的系统级、用户级、仓库级及 include / includeIf 配置和优先级，保留用户身份、认证助手、SSH、签名、hooks、过滤器、合并策略及用户显式确定的代理行为。JchTools 不复制或维护独立 Git 配置，不写改用户 Git 配置，不通过命令行配置覆盖或增删配置、认证、代理相关环境变量改变用户已显式确定的有效配置；仅在下述条件满足时为本工具发起的 Git HTTP(S) 网络操作补充 Windows 手动系统代理，不改写 Windows 设置或宿主进程环境。Git 未安装时只提示缺失，不创建替代配置。原有配置继承约定于 2026-10-13 确认；系统代理缺省行为及优先级于 2026-10-29 用户确认。

#### Scenario: G-17 Git 显式代理及直连策略优先
- **WHEN** Git 原有配置或继承环境已为本次 HTTP(S) 目标显式指定代理或直连策略，包括系统级、用户级、仓库级、include / includeIf、URL 专用及 remote 代理配置、显式空代理值，或适用的代理环境变量和 `NO_PROXY` / `no_proxy` 绕过规则
- **THEN** JchTools MUST 按系统 Git 原有优先级保留该有效策略，不使用 Windows 系统代理覆盖它；只对当前目标生效的绕过规则不得导致其他目标也绕过代理。

#### Scenario: G-17 缺省使用 Windows 手动系统代理
- **WHEN** 本工具执行 fetch / push 所需的 HTTP(S) 网络访问，且 Git 原有配置与继承环境未对当前目标显式指定代理或直连策略
- **THEN** JchTools MUST 读取 Windows 当前用户 Internet 设置中的 `ProxyEnable`、`ProxyServer` 和 `ProxyOverride`；系统代理开启、存在可用端点且目标未命中例外表时，实际请求必须经过按目标协议选择的系统代理。支持整体和按协议（http / https / socks）配置，代理服务器支持域名、IPv4、IPv6 及实际配置的端口，不得写死 `127.0.0.1`、回环网段、`7890` 或任何固定地址和端口。PAC / WPAD 自动配置不在本次范围内。

#### Scenario: G-17 系统代理例外表及关闭状态
- **WHEN** 没有适用于当前目标的 Git / 环境显式代理或直连策略，且当前目标命中 `ProxyOverride` 的分号分隔条目、大小写不敏感主机匹配、`*` 通配或 `<local>` 本地地址规则，或者系统代理关闭、缺失或没有可用端点
- **THEN** 命中例外表的目标 MUST 直连；系统代理关闭、缺失或没有可用端点时 MUST 沿用系统 Git 原有行为，不补充代理。例外规则不得扩张到未命中的目标。

#### Scenario: G-17 非 HTTP 传输及代理失败
- **WHEN** Git 使用 SSH 等非 HTTP(S) 传输，或 HTTP(S) 代理连接失败
- **THEN** JchTools MUST 保留非 HTTP(S) 传输的原有配置和环境，不为它补充 Windows HTTP(S) 代理；代理失败时不得清除原有代理或系统代理后强制直连，仍遵守 G-09 的重试及 G-15 的停止行为。不得因使用系统代理而放宽 Git 的 TLS 证书校验，不扩大 P-03 的 upstream 网络边界。
