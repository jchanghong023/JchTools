---
type: 运维指南
title: 构建、打包与诊断
description: 说明 JchTools 的 Cargo 特性开关、Windows GUI 资源打包、运行日志与性能追踪配置。提供构建和运行期故障的排查路径及安全边界。
tags: [build, packaging, logging, diagnostics, windows]
verified:
  - by: openwiki/0.0.0-unknown
    at: 2026-11-01T02:18:06.927Z
sources:
  - id: openwiki-source-36f054f1f38f2197a6af5bca
    resource: repo://build.rs
  - id: openwiki-source-651d1fb6c9e49916a916ab51
    resource: repo://Cargo.toml
  - id: openwiki-source-27a8c0a6cc7c3dc4748de841
    resource: repo://crates/foundation/src/logging.rs
  - id: openwiki-source-bc8ae0161e7a0f35a8fd16ad
    resource: repo://LOGGING.md
generated: { by: "openwiki/0.0.0-unknown", at: "2026-11-01T02:18:06.927Z" }
---

# 构建、打包与诊断

本文聚焦从 Cargo 配置到 Windows 产物、再到运行期诊断的运维链路。功能集成和用户设置入口另见[集成与 OCR](../integrations/xberg-and-ocr.md)、[设置与服务](settings-and-services.md)；一般启动见[快速开始](../quickstart.md)，验证范围见[验证](../testing/validation.md)。

## 构建特性与产物边界

工作区根 `Cargo.toml` 定义默认特性 `gui`，默认运行目标为 `JchTools`；该二进制声明 `required-features = ["gui"]`。因此关闭 GUI 后可以构建库或指定 crate，但不能把它当作完整桌面程序运行。`gui` 同时启用 GUI/UI crate 的默认路径及 Slint、文件对话框依赖，避免单独选包与工作区构建出现不同特性组合。

- `perf-tracing` 默认关闭；它将性能埋点特性转发到实际有埋点的各 crate，GUI 侧以可选依赖方式启用。
- `mcp-server` 是调试/测试用途，依赖 GUI，并编译 Slint MCP 能力。发布产物不要启用；运行期还需 `SLINT_MCP_PORT` 才会激活，相关无头测试见仓库根目录 `e2e.md`。
- `test-hooks` 为隔离测试暴露内部入口，不应作为生产特性。

最低 Rust 工具链是 `1.98.1`（`rust-version`）；CI 固定同版本。工作区含生产 crate、可选 OCR crate；本地修复依赖在 workspace 外以 path dependency 跟踪，不能仅凭 workspace 测试矩阵判断其独立验证状态。

常用命令：

```sh
cargo build --release
cargo test --workspace
cargo test --features test-hooks --lib
```

这些命令目标并不等价：第一个构建默认 GUI 发行配置，第二个覆盖工作区成员测试，第三个针对隔离测试入口；具体 CI 门禁与平台矩阵以 `.github/workflows` 为准。性能采集的诊断构建可加 `--features perf-tracing`，勿与正式包配置混淆。

## Windows GUI 打包

`build.rs` 仅在 `gui` 特性下装配 Windows 资源。对 Windows MSVC 目标，它为 `JchTools` 链接 `resources/windows.manifest`，并尝试把 `resources/app.ico` 编译嵌入 EXE。图标 `.rc` 源写为带 BOM 的 UTF-16LE，避免仓库路径含 CJK 字符时被 `rc.exe` 按 ANSI 误读。脚本优先从 Windows SDK 的版本目录中选 `rc.exe`，按数字版本排序；找不到才回退 PATH，并发出可信度警告。

若缺少 SDK、图标文件、或资源编译失败，构建脚本会警告且仍可生成不带嵌入图标的产物；运行时窗口图标仍由 Slint 设置。检查构建输出中的 `cargo:warning`。路径含空格时链接参数存在潜在问题，脚本会提示；优先将仓库放在简单路径并安装 Windows SDK。Windows manifest/icon 的改动会触发 Cargo 重新运行构建脚本。

安装包和依赖部署脚本位于 `installer/`、`scripts/`；构建 EXE 不自动等于完整安装包。发布时须按对应 Windows workflow 核对安装器载荷、目标架构、第三方许可及实际依赖，不能假设所有外部工具、模型或 OCR 引擎均已静态捆绑。对外分发还需检查 `THIRD_PARTY_NOTICES.md`。

## 运行日志：配置、位置与安全

Rust 诊断日志由共享 `jchtools_foundation` logging 基础设施初始化，主程序及 OCR worker 复用同一套 tracing API；不是每个业务模块各建 subscriber。主落点为状态目录下 `logs/`，主目录不可用时生产环境回退到当前用户 LocalAppData 的 `JchTools/Logs`；测试构建回退至隔离状态目录的 `logs-fallback`。两处均失败时仅向 stderr 报告去敏错误，不应导致业务退出。主程序启动/代理入口在服务工作前初始化并持有 Guard，正常退出路径显式刷盘。

日志级别由 `JCH_LOG_LEVEL` 控制：去除空白后不区分大小写支持 `debug`、`info`、`warning`、`error`；未设置、空值、非法值或非 Unicode 均回到 INFO。`warn`、`trace` 不属于有效输入；`RUST_LOG` 不接管该诊断配置。阈值过滤低等级事件，但 INFO 级关联 span 仍保留。提升至 DEBUG 会增加记录量，不采样。

日志是 UTC 微秒时间戳的单行结构化 key=value，包含级别、PID、线程、组件、事件及 span 上下文。敏感信息规则优先于排障便利：不要记录请求/响应正文、文件、OCR/模型内容、认证信息、无限制子进程输出；URL 与诊断文本仅能使用安全摘要。panic 仅记录类型和源码位置等元数据，不持久化 payload 或堆栈。

诊断文件按进程启动标识隔离并按 UTC 日期/大小分段；单文件上限 1 MiB，自有文件最多 10 个，最长保留 14 天，容量/数量可能提前淘汰旧文件。写入、创建与清理由落点级文件锁串行化，排他创建避免续写旧进程文件。文件系统错误、队列过载、异常退出都意味着不能承诺每条事件必然落盘；性能日志也不受普通日志保留规则约束。

## 性能追踪

编译时启用 `perf-tracing` 才有相应性能埋点，运行日志级别不能替代该编译开关。性能事件与常规日志共用 tracing registry，但性能输出独立于 `logs/`，位于状态目录 `perf-logs/` 的运行隔离目录中；Xberg 引擎使用单独启动目录。关闭特性时不要期待性能时间线。启用后依据实际配置检查该目录与埋点覆盖，而不是仅看 GUI 日志中是否有事件。

## 排障顺序

1. **命令或二进制不可用**：确认工作目录、Rust 版本及 `gui` 特性；`JchTools` 要求 GUI。确认构建目标与运行目标一致。
2. **Windows 资源警告**：查看 `rc.exe` 是否来自 Windows SDK、目标是否为 MSVC；核对 `resources/app.ico` 与 manifest 存在。资源警告通常说明图标未嵌入，不等于业务编译失败；检查路径空格警告。
3. **日志目录无新文件**：先确认应用实际状态目录与权限，再检查 LocalAppData 回退位置、启动 stderr 及 `JCH_LOG_LEVEL` 拼写。排查日志不能依赖 `RUST_LOG`。
4. **日志缺项或文件轮转**：过滤阈值可能抑制低等级事件；单文件/文件数上限和 14 天保留会淘汰历史，多个进程受同一保留策略约束。不要手工把任意相似命名文件当作产品日志删除。
5. **无性能数据**：确认构建启用了 `perf-tracing`，检查 `perf-logs/` 的当前启动目录，并核实具体操作路径确实有埋点。性能日志与诊断日志目录、开关不同。
6. **安装后功能缺失**：区分 EXE、安装器及运行时可选依赖；核查安装器文件清单和目标机器所需的引擎/模型/系统组件，不要以本地开发机环境推断已随安装包提供。

若需判断问题是否为代码回归，应使用[验证指南](../testing/validation.md)对应的平台与范围复现；构建成功仅证明该构建路径通过，不代表安装、运行时服务、模型或外部依赖均已验收。
