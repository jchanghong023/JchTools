---
type: 工程导航
title: 工程入口与任务导航
description: 从已批准需求和协作权限出发，定位 Windows 工程入口，并按修改任务进入架构、工作流、验证与交付专题。
tags: [quickstart, navigation, requirements, windows]
verified:
  - by: openwiki/0.0.0-unknown
    at: 2026-11-01T04:18:51.401Z
sources:
  - id: openwiki-source-8037e2358a2c4f9b2c722a11
    resource: repo://AGENTS.md
  - id: openwiki-source-651d1fb6c9e49916a916ab51
    resource: repo://Cargo.toml
  - id: openwiki-source-63b78807a76d5ae0bf99febc
    resource: repo://e2e.md
  - id: openwiki-source-23775c3de52f3ab95a13cb8b
    resource: repo://README.md
  - id: openwiki-source-b55a21a31ede1b56cd31a6a6
    resource: repo://src/main.rs
generated: { by: "openwiki/0.0.0-unknown", at: "2026-11-01T04:18:51.401Z" }
---

# 工程入口与任务导航

## 先确定权威与权限

1. 先读根目录 [AGENTS.md](../AGENTS.md)：它规定协作、需求维护、临时文件、验证和授权纪律。按任务读取 [已批准规格](../openspec/specs/) 中的功能域，同时补读[应用全局与共用约束](../openspec/specs/application-contract/spec.md)及交叉引用；[README 的需求入口](../README.md#需求与工程入口)列出各域。
2. `openspec/specs/` 是唯一已批准需求基线。`openspec/changes/` 中的提案和需求增量须核对批准状态；提案存在、代码已实现、验证通过是不同事实，彼此不能代替。新需求先按协作纪律形成提案并确认批准；缺陷修复先写修复前会失败的回归测试。
3. 本 Wiki 是导航，不授权运行构建、测试、桌面驱动或发布。纯文档修改按实际影响检查内容、差异、引用和需求保留，不运行产品构建或测试。功能验证应按风险和影响选测并如实报告范围、结果与未验证项。

## Windows 工程入口

仓库根 `Cargo.toml` 默认运行 `JchTools`，默认启用 `gui`；根 [src/main.rs](../src/main.rs) 是薄入口：普通启动进入 GUI，`--xberg-broker` 和 `--codeagent-service` 是内部后台模式。GUI 装配与工具注册分别从 `crates/gui/src/gui.rs` 和 `crates/gui/src/registry.rs` 进入，界面源为 `ui/app.slint`；新增 GUI 工具须注册并接入真实页面。独立 CLI `jchocr` 在 `crates/jchocr/`，不是新 GUI 工具。

Windows PowerShell 中的 GUI 启动入口是 `cargo run --bin JchTools`；开发构建为 `cargo build`。这些是入口而非本次执行指令。构建前需要 Rust 1.98.1 或更新版本、`x86_64-pc-windows-msvc` 工具链、VS C++ Build Tools 和 Windows SDK（含 `rc.exe`）。验证和打包还有各自依赖与授权前提，见下方专题。

## 按任务进入

| 修改或排查任务 | 下一站 |
| --- | --- |
| GUI 装配、工具注册、导航及后台回调 | [应用装配、GUI 状态与后台任务](architecture/application-and-gui.md) |
| 解压或目录整理及共用路径安全、任务持久化 | [文件任务的安全边界与持久状态](concepts/file-task-safety.md)；[递归解压](workflows/recursive-extraction.md)；[目录整理](workflows/directory-organization.md) |
| MD 合并拆分、转换或 Git 发布行为 | [Markdown 整理](workflows/markdown-organization.md)；[转 Markdown](workflows/markdown-conversion.md)；[Git 逐文件发布](workflows/git-file-publishing.md) |
| 共享推理、截图及 CLI OCR | [共享 Xberg 协议与代理生命周期](integrations/shared-xberg.md)；[截图后台与 jchocr](workflows/screenshot-and-cli-ocr.md) |
| CodeAgent 凭据维护、应用设置或诊断 | [CodeAgent 本地 Token 自动维护](workflows/codeagent-maintenance.md)；[应用设置、状态目录与诊断日志](operations/state-and-diagnostics.md) |
| 选测、验收证据与桌面边界 | [验证入口、影响选测与证据边界](testing/verification.md)；[e2e.md](../e2e.md) |
| 构建资产、安装包或便携包 | [构建、资产装配与 Windows 交付](operations/build-and-delivery.md) |

## 验证与交付边界

`fastcheck` 是静态、格式及编译检查，不运行测试；`fulltest` 是离线的影响范围功能门，不证明打包；`slowtest` 另覆盖适用的在线准备与交付验证。后两者每次均需当次明确授权，`--authorized` 本身不创造授权。局部通过不等于完整验收；CI 只编译、打包和发布，不运行功能测试或质量门。

真实窗口、桌面驱动及全局鼠标或焦点抢占不属于三级门，须当次另行明确授权；抢占用户鼠标的流程还须用户当次明确要求使用 [e2e.md](../e2e.md)。无头 MCP 也不等于真实桌面验收，且必须按该文档隔离状态、凭据路径和注册表。真实发布须另行明确授权；未运行或缺少依赖的项目不得报告为通过。
