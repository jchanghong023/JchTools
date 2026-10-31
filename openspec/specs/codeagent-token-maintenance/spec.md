# CodeAgent 本地 Token 维护

## Purpose

定义 JchTools 的CodeAgent 本地 Token 维护及其可观察结果、约束、例外和验收边界。独立本地凭据维护工具，同时遵守全局安全、界面和日志要求；不调用 PowerShell，不联网。

各能力同时遵守[应用全局与共用规格](../application-contract/spec.md)，工程纪律见[AGENTS.md](../../../AGENTS.md)。本文为已批准需求，不记录实现状态或测试通过状态；未实现不撤销要求，历史授权不授予当前执行权限。

## Requirements

<a id="行为"></a>

**行为**

### Requirement: CA-01

JchTools MUST 遵守本条行为：注册独立「CodeAgent」GUI 页面，归「开发」，复用现有设计令牌和控件。

#### Scenario: CA-01 完整行为与适用边界
- **WHEN** 用户打开 CodeAgent 页面、后台检查凭据或安装器执行维护退出，且涉及本条规定的条件
- **THEN** 注册独立「CodeAgent」GUI 页面，归「开发」，复用现有设计令牌和控件。

### Requirement: CA-02

JchTools MUST 遵守本条行为：Rust 读取 `%USERPROFILE%\.cac\.credentials.json` 的 `idaasOAuth.xAuthToken`，Base64 解码后调用 Windows `CryptUnprotectData`，附加熵 NULL、CurrentUser 作用域，解密结果按 UTF-8 解析。

#### Scenario: CA-02 完整行为与适用边界
- **WHEN** 用户打开 CodeAgent 页面、后台检查凭据或安装器执行维护退出，且涉及本条规定的条件
- **THEN** Rust 读取 `%USERPROFILE%\.cac\.credentials.json` 的 `idaasOAuth.xAuthToken`，Base64 解码后调用 Windows `CryptUnprotectData`，附加熵 NULL、CurrentUser 作用域，解密结果按 UTF-8 解析。

### Requirement: CA-03

JchTools MUST 遵守本条行为：写入当前用户 `HKCU\Environment\JCH_CODEAGENT3_X_AUTH_TOKEN`，类型为 `REG_SZ`；与当前值一致时不写入，变化时立即更新。

#### Scenario: CA-03 完整行为与适用边界
- **WHEN** 用户打开 CodeAgent 页面、后台检查凭据或安装器执行维护退出，且涉及本条规定的条件
- **THEN** 写入当前用户 `HKCU\Environment\JCH_CODEAGENT3_X_AUTH_TOKEN`，类型为 `REG_SZ`；与当前值一致时不写入，变化时立即更新。

### Requirement: CA-04

JchTools MUST 遵守本条行为：默认每 60 秒检查；GUI 可修改正整数秒间隔，持久保存，后台采用新间隔。

#### Scenario: CA-04 完整行为与适用边界
- **WHEN** 用户打开 CodeAgent 页面、后台检查凭据或安装器执行维护退出，且涉及本条规定的条件
- **THEN** 默认每 60 秒检查；GUI 可修改正整数秒间隔，持久保存，后台采用新间隔。

### Requirement: CA-05

JchTools MUST 遵守本条行为：后台独立运行且保持单实例；GUI 关闭后继续检查，不受 GUI 任务停止逻辑影响。

#### Scenario: CA-05 完整行为与适用边界
- **WHEN** 用户打开 CodeAgent 页面、后台检查凭据或安装器执行维护退出，且涉及本条规定的条件
- **THEN** 后台独立运行且保持单实例；GUI 关闭后继续检查，不受 GUI 任务停止逻辑影响。打开 CodeAgent 页面启动或连接该后台，不要求另行运行命令。共享 SQLite 暂时不可用时保留最后有效间隔并继续维护凭据，不释放单实例锁；存储恢复后补写最新安全状态。

### Requirement: CA-06

JchTools MUST 遵守本条行为：页面显示后台运行状态、最近检查时间和结果，不显示 Token；日志、控制台、状态记录及错误信息禁止包含 Token 明文。

#### Scenario: CA-06 完整行为与适用边界
- **WHEN** 用户打开 CodeAgent 页面、后台检查凭据或安装器执行维护退出，且涉及本条规定的条件
- **THEN** 页面显示后台运行状态、最近检查时间和结果，不显示 Token；日志、控制台、状态记录及错误信息禁止包含 Token 明文。

### Requirement: CA-07

JchTools MUST 遵守本条行为：读取或解密失败只报告去敏错误，保留已有注册表 Token，后续按间隔继续检查。

#### Scenario: CA-07 完整行为与适用边界
- **WHEN** 用户打开 CodeAgent 页面、后台检查凭据或安装器执行维护退出，且涉及本条规定的条件
- **THEN** 读取或解密失败只报告去敏错误，保留已有注册表 Token，后续按间隔继续检查。

### Requirement: CA-08

JchTools MUST 遵守本条行为：安装升级替换文件前、卸载删除文件前，安装器通过内部维护入口请求本应用的 CodeAgent 后台自然退出并等待释放运行镜像与单实例锁；未能完成时明确阻止该次替换或卸载，不强杀其他进程。

#### Scenario: CA-08 完整行为与适用边界
- **WHEN** 用户打开 CodeAgent 页面、后台检查凭据或安装器执行维护退出，且涉及本条规定的条件
- **THEN** 安装升级替换文件前、卸载删除文件前，安装器通过内部维护入口请求本应用的 CodeAgent 后台自然退出并等待释放运行镜像与单实例锁；未能完成时明确阻止该次替换或卸载，不强杀其他进程。此入口不是用户 CLI，不改变 CA-05 的普通 GUI 关闭后继续维护行为，不删除凭据或已有注册表 Token。

<a id="轻量验收"></a>

**轻量验收**

- Windows CurrentUser DPAPI 合成密文能解密为原 UTF-8；错误密文失败且已有注册表值不变。
- 隔离注册表用例证明 `REG_SZ` 更新与相同 Token 不写入。
- 真实内部后台入口证明单实例、持久间隔及宿主 GUI 退出后继续检查；并发状态查询不妨碍后台首次启动，共享 SQLite 暂时争用期间保持同一实例并在恢复后继续检查；使用隔离测试数据，不打印 Token。
- 页面验证显示安全状态、检查时间与结果，保存间隔可由后台观察；未执行真实桌面验收时如实标注。
- 隔离安装位置及凭据证明维护退出后旧镜像可替换或删除；普通 GUI 退出仍不停止后台，维护失败不得冒称安装或卸载完成。
