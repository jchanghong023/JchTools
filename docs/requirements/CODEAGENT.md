# CodeAgent Token 自动维护（CA）

本功能是独立的本地凭据维护工具，遵守 [CONTRACT.md](CONTRACT.md) 的共用安全、界面与日志要求；不调用模型服务或 PowerShell，不联网。

## 行为

- **CA-01** 注册独立「CodeAgent」GUI 页面，归「开发」，复用现有设计令牌和控件。
- **CA-02** Rust 读取 `%USERPROFILE%\.cac\.credentials.json` 的 `idaasOAuth.xAuthToken`，Base64 解码后调用 Windows `CryptUnprotectData`，附加熵 NULL、CurrentUser 作用域，解密结果按 UTF-8 解析。
- **CA-03** 写入当前用户 `HKCU\Environment\JCH_CODEAGENT3_X_AUTH_TOKEN`，类型为 `REG_SZ`；与当前值一致时不写入，变化时立即更新。
- **CA-04** 默认每 60 秒检查；GUI 可修改正整数秒间隔，持久保存，后台采用新间隔。
- **CA-05** 后台独立运行且保持单实例；GUI 关闭后继续检查，不受 GUI 任务停止逻辑影响。打开 CodeAgent 页面启动或连接该后台，不要求另行运行命令。
- **CA-06** 页面显示后台运行状态、最近检查时间和结果，不显示 Token；日志、控制台、状态记录及错误信息禁止包含 Token 明文。
- **CA-07** 读取或解密失败只报告去敏错误，保留已有注册表 Token，后续按间隔继续检查。

## 轻量验收

- Windows CurrentUser DPAPI 合成密文能解密为原 UTF-8；错误密文失败且已有注册表值不变。
- 隔离注册表用例证明 `REG_SZ` 更新与相同 Token 不写入。
- 真实内部后台入口证明单实例、持久间隔及宿主 GUI 退出后继续检查；使用隔离测试数据，不打印 Token。
- 页面验证显示安全状态、检查时间与结果，保存间隔可由后台观察；未执行真实桌面验收时如实标注。
