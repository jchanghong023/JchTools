---
type: 工作流程
title: CodeAgent 本地 Token 自动维护
description: 说明独立 CodeAgent 后台如何定时解密当前用户凭据、按变化更新注册表，以及状态隔离、维护退出和测试边界。
tags: [codeagent, credentials, dpapi, windows, background-service]
verified:
  - by: openwiki/0.0.0-unknown
    at: 2026-11-01T04:18:51.401Z
sources:
  - id: openwiki-source-f1075b77a9e50844d1165f8f
    resource: repo://crates/codeagent/src/codeagent.rs
  - id: openwiki-source-e857d702c8ecf092db489879
    resource: repo://crates/gui/src/gui/codeagent.rs
  - id: openwiki-source-a14532b5acb3ff7fcb78a865
    resource: repo://crates/gui/src/gui/wiring.rs
  - id: openwiki-source-d4885361a1b1a8461b274ed1
    resource: repo://installer/JchTools.iss
  - id: openwiki-source-951cdb57947e97e2204d9cda
    resource: repo://openspec/specs/codeagent-token-maintenance/spec.md
  - id: openwiki-source-b55a21a31ede1b56cd31a6a6
    resource: repo://src/main.rs
  - id: openwiki-source-d2f0bd32fe044e63be00f91d
    resource: repo://tests/codeagent.rs
  - id: openwiki-source-c2d431b50a315528427eb20b
    resource: repo://tests/codeagent/review_remediation.rs
generated: { by: "openwiki/0.0.0-unknown", at: "2026-11-01T04:18:51.401Z" }
---

# CodeAgent 本地 Token 自动维护

CodeAgent 是独立的 Windows 本地凭据维护后台，不是 Xberg 代理：它不负责代理请求，也不需要联网或调用 PowerShell。打开 GUI 的 CodeAgent 页面会请求启动或连接后台；后台使用独立进程，关闭 GUI 不会停止它。有关应用入口与界面框架见[应用与 GUI](../architecture/application-and-gui.md)。

## 检查与安全边界

```mermaid
flowchart TD
    Start["后台启动并取得单实例锁"] --> Tick["首次立即检查，之后按有效间隔检查"]
    Tick --> Read["读取当前用户凭据 JSON"]
    Read --> Decode["提取字段并 Base64 解码"]
    Decode --> DPAPI["CurrentUser DPAPI 解密并校验 UTF-8"]
    DPAPI --> Compare["比较当前用户注册表值"]
    Compare -->|相同| Same["不写入，记录未变化"]
    Compare -->|不同或不存在| Write["写入 REG_SZ，记录已更新"]
    Read -->|读取失败| Fail["记录静态失败分类，保留已有值"]
    Decode -->|格式或解码失败| Fail
    DPAPI -->|解密或 UTF-8 失败| Fail
    Compare -->|注册表访问失败| Fail
    Write -->|写入失败| Fail
    Same --> Status["发布去敏状态"]
    Write --> Status
    Fail --> Status
    Status --> Tick
```

图：定时检查只在值变化时写注册表，失败以去敏状态报告并在后续周期重试。

生产路径是 `%USERPROFILE%\.cac\.credentials.json`，读取 JSON 的 `idaasOAuth.xAuthToken`，通过 `CryptStringToBinaryW` 解码，再以无附加熵、禁止交互的 `CryptUnprotectData` 使用 CurrentUser DPAPI 解密，严格按 UTF-8 解析。DPAPI 分配的明文缓冲区在释放前清零；这不意味着所有后续字符串副本都被保证清零。

解密成功后才打开或创建 `HKCU\Environment`，目标值为 `JCH_CODEAGENT3_X_AUTH_TOKEN`。已有 `REG_SZ` 与包含终止符的 UTF-16 值一致时返回 `Unchanged` 而不写入；否则写入 `REG_SZ` 并返回 `Updated`。空字符串或内含 NUL 被拒绝。读取、格式、解密、UTF-8 或注册表访问失败不主动清除已有值；读取或解密失败甚至不会创建目标键。

凭据不进入 SQLite、GUI 或日志；可传播的检查结果仅为 `Waiting`、`Updated`、`Unchanged`、`ReadFailed`、`DecodeFailed`、`DecryptFailed`、`InvalidUtf8`、`RegistryFailed` 这些静态分类，不传播解析器错误正文。SQLite 保存间隔和去敏状态，状态包括运行标志、PID、有效间隔、最近检查时间和结果；日志使用静态事件与分类。注册表本身是明文目标，应避免在诊断中展示其内容。

## 进程、间隔与观察

`src/main.rs` 将内部 `--codeagent-service` 分派给 `serve()`，与 `--xberg-broker` 分支分开。GUI 的 `ensure_started()` 先观察锁，必要时以无窗口、脱离父进程的方式启动当前可执行文件，再有限等待可观察的运行状态和 PID。真正的单实例裁决在后台独占持有的 `codeagent-service.lock`，重复启动在锁争用时正常退出；锁一直持有到进程实际结束，包括日志刷盘。运行状态以锁探测为准，而不是盲信 SQLite 中可能过期的 `running` 或 PID。

默认间隔为 60 秒；GUI 保存的 `codeagent_interval_seconds` 必须是正整数秒，存入共享 `app_settings`。后台每秒重新读取配置，以自上次检查经过的时间决定是否到期，因此改变间隔无需重启，也不会每秒重复解密。后台使用零等待 SQLite 连接：首次读取失败暂用 60 秒，此后读取失败保留上次有效间隔；状态发布失败不停止检查或释放锁，恢复后重试发布最新去敏状态。启动请求可能因读不到状态而报错，即使独立后台已经启动并继续维护。

GUI 的观察线程处理启动和保存请求，并约每 500 毫秒刷新状态；展示运行情况、PID、最近检查、结果和后台有效间隔。已保存间隔与后台当前有效间隔分开读取，编辑中的草稿不会被刷新覆盖；保存失败单独显示，不停止后台。状态读取失败显示状态未知，不应据此断定后台已经退出。更多共享状态和诊断背景见[状态与诊断](../operations/state-and-diagnostics.md)。

## 安装与卸载的维护退出

普通窗口关闭不发送停止请求。安装升级替换文件之前、确认卸载删除文件之前，安装器调用内部 `--codeagent-service --stop`；它不是普通用户操作入口。控制锁串行化启动清理与停止请求，防止握手丢失。停止方不依赖 SQLite，也不强杀进程：读取持锁后台发布的 PID，发送停止文件，请求自然退出，等待进程句柄及单实例锁释放；等待失败返回错误。后台检查到请求后尽力发布停止状态，但不会等待数据库可写。

升级时安装器使用本次安装包携带的临时维护控制器，不运行可能忽略停止参数的旧版程序；卸载时使用已安装的程序。控制器失败会阻止替换或卸载，而不是声称成功；该协议不删除源凭据或已有注册表 Token。安装交付背景见[构建与交付](../operations/build-and-delivery.md)。

## 验证范围与隔离

Windows 测试使用合成数据和真实 CurrentUser DPAPI，断言解密、隔离注册表的 `REG_SZ` 更新与未变化结果，以及错误密文、无效 UTF-8、无效 JSON 和读取失败时保留旧值。测试还覆盖间隔校验、单实例、短命启动宿主退出后后台继续、SQLite 独占争用期间继续检查及恢复、状态和日志不含所用合成值。争用测试通过锁和注册表变化观察后台，而不是依赖受争用阻塞的状态读取。

`test-hooks` 隔离要求同时配置绝对路径 `JCHTOOLS_TEST_CODEAGENT_CREDENTIALS`、绝对路径 `JCHTOOLS_TEST_STATE_DIR` 和位于 `Software\JchTools\Tests\CodeAgent\` 下的 `JCHTOOLS_TEST_CODEAGENT_REGISTRY`；部分配置或无效注册表变量会拒绝回退真实位置。相关回归还覆盖保存与生效间隔的区别、维护退出及安装升级和卸载入口。上述是测试代码的断言范围，不等同于本次已执行测试或真实桌面验收；参见[验证](../testing/verification.md)。
