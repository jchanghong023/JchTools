# Spec Delta

## REMOVED Requirements

### Requirement: AH-01

**Reason**：用户要求删除整个模型服务，不再要求独立工具、GUI 或 Rust 服务装配。

**Migration**：系统 MUST 删除模型服务注册项、界面及专属实现，不保留隐藏入口。

#### Scenario: 不再提供模型服务工具
- **WHEN** 用户打开工具箱或切换导航
- **THEN** 不显示或打开模型服务，其他七个工具及设置、关于仍可独立使用。

### Requirement: AH-02

**Reason**：退役功能不再需要官方 ACP 客户端适配。

**Migration**：系统 MUST 删除 ACP SDK 接入及其专属依赖声明，不以自写协议替代。

#### Scenario: 不再初始化 ACP 客户端
- **WHEN** 新版本启动或使用任一保留工具
- **THEN** 不因原模型服务创建 ACP 客户端或进行 ACP 初始化。

### Requirement: AH-03

**Reason**：HTTP 到外部 ACP Agent 的请求链路整体取消。

**Migration**：系统 MUST 删除请求映射、角色包装和响应适配，不修改外部调用方或 Agent 产品。

#### Scenario: 无模型请求转发链路
- **WHEN** 新版本运行
- **THEN** 不接受并转发原模型服务请求至外部 Agent，也不保留该链路的备用实现。

### Requirement: AH-04

**Reason**：模型列表、Chat Completions、SSE 和会话续接接口不再属于产品能力。

**Migration**：系统 MUST 删除旧 HTTP 路由、DTO、流式及会话处理，不提供兼容端点或固定响应。

#### Scenario: 旧 HTTP 契约不再发布
- **WHEN** 调用方尝试使用新版本原有的 `/v1/models` 或 `/v1/chat/completions`
- **THEN** 新版本不提供这些接口，也不启动替代监听器来返回假模型或退役占位结果。

### Requirement: AH-05

**Reason**：模型服务退役后不再承接后端 Agent 工具决策或文本结果。

**Migration**：系统 MUST 删除服务专属工具事件、权限协作和文本适配，不转交其他工具承接。

#### Scenario: 不建立替代工具执行链
- **WHEN** 用户使用任一保留工具
- **THEN** 不经原模型服务执行 Agent 任务或建立 HTTP 调用方工具循环。

### Requirement: AH-06

**Reason**：独立后台、Agent 进程及工作目录管理不再需要。

**Migration**：系统 MUST 删除模型服务自动启动、连接、监督和工作目录创建逻辑。

#### Scenario: 启动应用不唤起旧服务
- **WHEN** 用户启动新版本，且旧数据库包含有效模型服务配置
- **THEN** 不读取该配置启动服务或 Agent，不创建 `acp-workspace`，其他工具正常装配。

### Requirement: AH-07

**Reason**：已移除能力不再具有关窗后持续接受模型请求的常驻承诺。

**Migration**：系统 MUST 删除模型服务常驻生命周期接入，不改变截图/Xberg 与 CodeAgent 的独立常驻行为。

#### Scenario: 关闭 GUI 不涉及已移除服务
- **WHEN** 用户关闭或终止新版本 GUI
- **THEN** 不存在由新版本模型服务承担的在途或后续 HTTP 请求，其他后台按各自规格运行。

### Requirement: AH-08

**Reason**：Agent 启动程序、参数和 HTTP 端口配置入口全部退役。

**Migration**：系统 MUST 删除对应字段、校验和服务错误展示，不把它们迁入设置页。

#### Scenario: 无替代配置页
- **WHEN** 用户查看设置、关于或任一工具页
- **THEN** 无模型服务启动程序、参数、端口或连接配置入口。

### Requirement: AH-09

**Reason**：已退役服务不再保存、恢复或应用配置。

**Migration**：系统 MUST 删除专属配置读写、待应用状态及重启逻辑；既有共用数据库和其他设置保持不变。

#### Scenario: 旧配置成为未消费数据
- **WHEN** 旧 `config.sqlite3` 含有 `acp_http_config`
- **THEN** 新版本不解析、更新或应用该记录，也不因其内容影响其他设置读取；不为退役新增数据清理逻辑。

### Requirement: AH-10

**Reason**：模型服务专属退出入口、确认框及停止状态不再存在。

**Migration**：系统 MUST 删除该退出链路，不并入截图/Xberg 托盘，也不扩大其他后台退出职责。

#### Scenario: 保留后台的退出范围不扩张
- **WHEN** 用户主动退出截图/Xberg 后台或操作 CodeAgent
- **THEN** 按原能力的范围执行，不处理已移除模型服务的生命周期。

### Requirement: AH-11

**Reason**：不再提供 OpenAI 到 ACP 的 HTTP 后端。

**Migration**：系统 MUST 删除 Axum 服务装配，清除仅为该功能保留的直接及传递依赖，不另换 HTTP 框架。

#### Scenario: 无遗留 HTTP 后端
- **WHEN** 生成新版本构建与交付内容
- **THEN** 不包含模型服务 HTTP 实现；剩余目标仍需的依赖不因名称或历史归属被误删。

### Requirement: AH-12

**Reason**：单 Agent 多会话并发、队列和容量限制随服务整体退役。

**Migration**：系统 MUST 删除模型服务 Agent 管理、会话调度、排队及容量状态，不保留不可达调度器。

#### Scenario: 无模型服务请求队列
- **WHEN** 新版本运行多个保留工具
- **THEN** 不创建原模型服务的 Agent、会话或请求队列，也不改变其他任务调度。

### Requirement: AH-13

**Reason**：ACP 权限、文件和终端回调以及断连取消、异常恢复仅服务于退役链路。

**Migration**：系统 MUST 删除这些专属回调、终端资源和恢复代码，不删除其他工具仍使用的 Windows 或进程基础能力。

#### Scenario: 无遗留 Agent 回调执行
- **WHEN** 新版本运行
- **THEN** 不通过旧 ACP 文件、终端、权限或交互回调读写用户资料、运行命令或恢复 Agent。

### Requirement: AH-14

**Reason**：模型服务及其专属依赖源码不再属于仓库和产品的保留范围。

**Migration**：系统 MUST 删除服务专属源码、测试与资源，并按剩余依赖闭包同步裁减 vendor、来源登记和合规材料。

#### Scenario: 依赖清理保留实际消费者
- **WHEN** 一个原模型服务使用的依赖仍被其他生产、构建或测试目标需要
- **THEN** 保留该依赖的必要源码、版本、来源和许可；仅无剩余消费者的专属依赖退出清单与交付。

### Requirement: AH-15

**Reason**：新版本不再提供模型服务发现协议。

**Migration**：系统 MUST 删除控制管道及 `Discover` 处理，不保留只读发现兼容层。

#### Scenario: 不再发布可发现模型端点
- **WHEN** 外部调用方尝试发现新版本模型服务
- **THEN** 新版本不发布 `jchtools-acp-http` 实例、HTTP 地址或模型服务能力，也不因此启动后台。

### Requirement: AH-16

**Reason**：不再启动后端 ACP Agent，无需服务专属递归发现保护。

**Migration**：系统 MUST 删除 `OMP_JCHTOOLS_DISCOVERY=0` 的模型服务子进程注入逻辑，不修改外部 OMP 配置或用户环境。

#### Scenario: 无遗留发现环境处理
- **WHEN** 新版本启动或运行保留工具
- **THEN** 不因原模型服务设置、持久化或清除外部 OMP 的发现环境与配置。

### Requirement: AH-17

**Reason**：ACP 后台和维护停止协议随能力全部移除。

**Migration**：系统 MUST 删除 `--acp-http-service` 及其停止分支、安装器对应调用和提示；CodeAgent 的 CA-08 维护退出不变。

#### Scenario: 安装维护不调用旧协议
- **WHEN** 新版本安装器升级或卸载
- **THEN** 不执行 ACP 维护命令，仍按 CA-08 等现有保留约束执行维护；不强杀旧版服务或用户进程，不绕过文件占用继续覆盖或删除。
