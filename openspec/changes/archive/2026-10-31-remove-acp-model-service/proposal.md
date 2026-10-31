# Proposal

## Why

用户明确要求删除「模型服务」的全部代码、依赖、测试和文档。当前该功能不仅是侧栏页面，还包含独立 ACP / HTTP 后台、外部 Agent 管理、发现协议、安装维护及验收链路；仅隐藏入口不能满足完整移除要求。

现行 `acp-model-service` 的 AH-01～AH-17 仍要求交付该功能，`application-contract` 和 `shared-inference` 也保留对应例外、交付及生命周期约束。本变更明确提出撤销这些要求，避免删除实现后仍与有效需求冲突。本次只形成待评审的需求产物，不修改实现或已批准主规格，不代表已完成移除或软件验收。

## What Changes

- **BREAKING**：彻底退役「模型服务」，移除注册表项、导航、Slint 页面、字段、回调、确认框、观察线程与启动装配；应用保留其余七个工具及设置、关于入口，不保留隐藏页、功能开关或空占位。
- **BREAKING**：删除 ACP / OpenAI 风格 HTTP 中间层及独立后台，不再提供 `/v1/models`、`/v1/chat/completions`、SSE、会话续接、控制管道 `Discover` 或 Agent 文件/终端回调；删除 `--acp-http-service` 及其 `--stop` 分支、安装器 ACP 维护调用和相关提示。不保留兼容协议、替代服务、废弃别名或模拟响应。
- 删除模型服务专属配置读写、状态、日志角色、环境变量处理和工作目录创建逻辑，不再消费旧 `acp_http_config` 或创建 `acp-workspace`；保留其他工具共用的 SQLite、日志及后台基础设施。
- 删除 `crates/acp-api` 及其所有调用和 Cargo 接入；清除服务专属的直接依赖、feature 和仅由该功能引入的传递依赖，以及对应 vendor 源码、来源清单和许可登记。仍被其余生产、构建或测试目标使用的依赖与许可必须保留，不按「曾被 ACP 使用」批量删除。
- 删除全部模型服务单元、GUI、HTTP、回调、发现、后台进程和真实 OpenCode 验收测试，以及专属夹具、二进制目标、环境配置、nextest 过滤项、门阶段及基线记录；保留混合测试和脚本中其余能力的有效覆盖，不用 ignore、空实现或伪造 PASS 代替删除。
- 删除模型服务专属文档及现有文档中的相关内容，包括 README、日志约定、E2E 指引、合规说明和仓库内审查交接文档；移除现行规格中的 AH 要求、验收矩阵、联网/API 例外和交叉引用。退役变更自身用于说明移除范围与依据，不作为继续提供该功能的文档。
- 截图 OCR、文档/媒体转换、共享 Xberg 和 CodeAgent 是独立能力，不因移除名称中的「模型」或独立后台而被误删；其模型资产、凭据、配置、运行及退出语义不变。

## Capabilities

### New Capabilities

无。

### Modified Capabilities

- `acp-model-service`：移除 AH-01～AH-17 全部要求及该能力的附属验收、说明；不保留继续交付模型服务的规范性义务。
- `application-contract`：更新 P-02、P-03、P-05、P-06、P-10、P-14、U-09，并约束模型服务代码、依赖、测试和文档的完整退役及其他能力保护。
- `shared-inference`：更新 XB-23，删除对已退役模型服务的退出范围交叉引用，保留截图与共享 Xberg 的现有生命周期。

## Impact

### 已观察到的影响面

- 生产与界面：`crates/acp-api/`、`src/main.rs`、`crates/gui/Cargo.toml`、`crates/gui/src/registry.rs`、`crates/gui/src/gui.rs` 及其 `gui/` 内 ACP 装配、状态、回调和测试，`ui/app.slint`、`ui/pages/acp.slint`、`ui/pages/about.slint` 与服务专属图标。
- 共享基础：`crates/foundation/src/xberg_settings.rs`、`crates/foundation/src/logging/` 的 ACP 专属角色、测试或注释。`app_settings` 是共用表，不能随模型服务一起删除。
- 依赖与交付：根及 GUI Cargo 清单、`Cargo.lock`、`vendor/`、`vendor/SOURCES.json`、`vendor/_upstream-licenses/`、`THIRD_PARTY_NOTICES.md`、`installer/JchTools.iss` 及打包所用清单。根清单明确引用官方 `agent-client-protocol`、Axum、futures、Tokio 与 tokio-util；实际删除范围以移除后的全部剩余目标需求为准，不预设这些包及所有相关 feature 均可删除。
- 测试与工具：`tests/acp_*.rs`、对应测试子目录、`tests/fixtures/acp_api/`、`tests/fixtures/acp_opencode.rs`、混合 GUI/日志测试、`.config/nextest.toml`、`scripts/local_gate_plan.py`、`scripts/gui_smoke_impl/`、`scripts/static_check_impl/`、`scripts/test-baseline.json` 及需求引用清单。
- 文档与规格：`README.md`、`LOGGING.md`、`e2e.md`、`docs/` 的相关内容，以及本提案列出的三个能力。CodeAgent 规格 Purpose 中的模型服务对比文字属于同步删除的文档内容，其 CA-01～CA-08 行为不变，不另造功能 delta。

### 验收边界

- 交付的 GUI 不再列出或打开模型服务，其他七个工具与设置、关于仍有真实入口；启动、导航和关闭应用均不启动模型服务或外部 ACP Agent。
- 新版本不再实现或发布旧 HTTP、发现、后台及维护入口，也不保留服务配置解析和后台连接逻辑。仅取消注册、禁用执行或保留不可达实现不算完成。
- 源码、构建依赖与 vendor、测试目标与夹具、门及桌面驱动、基线与需求映射、产品文档及合规材料均与移除后的功能集合一致；其他工具使用的共同依赖、测试断言及许可完整保留。
- 本次不运行构建、测试、桌面、模型或发布验证；上述条目是后续实施的验收要求，不是本次通过声明。

### 范围约定与兼容性

- 范围限于当前 JchTools 仓库及其后续产品交付；不修改外部 OMP/OpenCode 仓库或用户已安装的 Agent，不删除用户账号、凭据、模型资产、历史日志、共用数据库或用户工作区，不清空缓存或重写 Git 历史。
- 旧数据库可能保留不再使用的服务配置，旧工作目录可能存在；本次不新增运行期清理、数据迁移或兼容代码。新版本不得因这些残留恢复旧功能，也不得影响其他设置。
- 完全移除不承诺新版本代为停止仍运行的旧版模型服务。升级前应通过旧版自身入口正常退出旧服务；不能为了继续兼容旧 ACP 后台保留本次要求删除的协议实现，也不能强杀用户进程或绕过文件占用继续覆盖。
- 未决问题：无。本变更按「彻底移除、无替代功能、保留其余能力」拟定，不包含实施设计、任务清单、主规格同步或归档。
