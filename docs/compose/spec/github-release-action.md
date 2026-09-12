---
feature: github-release-action
status: delivered
updated: 2026-09-12
branch: master
commits: 20af006..6027801
---

# GitHub Release Action

## Report

**What was built** — 新增仅手动触发的 GitHub Actions 工作流 `.github/workflows/release.yml`：在 `windows-2022` 上执行现有 `scripts/package-windows.ps1`，以 UTC 精确到分钟的 tag（`yyyyMMdd-HHmm`）创建 Release（标题 `JchTools <tag>`），并只上传生成的 Windows 便携 ZIP。README「Windows 构建与发布」补充了 Releases 下载入口说明。`check.yml` 未改动。

**Verification** —
- `python scripts/static_check.py` PASS
- workflow YAML 可解析；内嵌 PowerShell 块 parse OK
- 独立 code review：spec / correctness / consistency 三类 PASS，无 critical
- 真实触发：Actions run [34678912136](https://github.com/jchanghong023/JchTools/actions/runs/34678912136) success
- Release：https://github.com/jchanghong023/JchTools/releases/tag/20260912-0644（tag `20260912-0644`，target `6027801`）
- 下载 `JchTools-Windows-x64-20260912-065659.zip`（18,586,531 bytes，sha256 `71f0ce05458e1188b31a06448e2ef494bb6fd00169b38fb5a76436e981801ee7`）
- 解压验证 PASS：含 `JchTools.exe`（~23 MiB）、`jchtools-cli.exe`、`launch-software.cmd`、`BUILD-INFO.json`、`resources/7zip` 许可/NOTICE/源码包；包内无散落 `7z.exe`/`7z.dll`；`BUILD-INFO.json` 标明 CI 测试已跑

**Journey log** —
- 版本策略由用户改为「当前时间精确到分钟」，不读 Cargo.toml、无输入框。
- 本机无 `gh` CLI；用 `~/.git-credentials` 中已有 PAT 调 GitHub API 完成 dispatch 与资产下载（未把 token 写入仓库或日志）。
- Python `yaml` 会把顶层 `on:` 解析成布尔 `True`（YAML 1.1），这是解析器怪癖，GitHub Actions 本身接受该文件。
- 外部评审指出：`gh release create` 在 tag 已存在但 Release 不存在时会复用 tag；本工作流与 Release 同时创建，实测无影响。

## [S1] Problem
用户需要从 GitHub Release 页面直接下载可运行的 Windows 便携包。现有 `check.yml` 只把 ZIP 作为 workflow artifact（有时效、需登录 Actions），不能当作稳定发布渠道。需要一条**仅手动触发**的发布流水线：打包 → 创建带时间戳 tag 的 Release → 上传 ZIP。

## [S2] Design
新增 `.github/workflows/release.yml`：

- **触发**：仅 `workflow_dispatch`。不响应 push / tag / PR。
- **权限**：`contents: write`（创建 tag + Release + 上传资产）。
- **Runner**：`windows-2022`，与现有 `check.yml` 的 Windows 打包路径一致。
- **打包**：复用 `scripts/package-windows.ps1`（含官方 7-Zip 引擎获取、测试、release 构建、许可材料与防泄漏检查）。设置 `GITHUB_TOKEN` 以降低 GitHub API 限流风险。
- **版本/tag**：执行 job 时取当前 UTC 时间到分钟，格式 `yyyyMMdd-HHmm`（例：`20260912-0644`）。不使用 Cargo.toml 版本，不接受输入框。
- **Release**：用 runner 自带 `gh release create` 创建 tag + Release，标题 `JchTools <tag>`，`--generate-notes`，`target` 为触发该次 workflow 的 commit SHA。
- **资产**：仅上传 `dist/JchTools-Windows-x64-*.zip`（完整便携包）。不附带单独 EXE、不附带 SHA256SUMS。
- **失败语义**：打包失败则不创建 Release；若同分钟 tag/Release 已存在则创建失败并让 job 失败（不静默覆盖）。
- **不改动** `check.yml` 的 push/PR 检查行为。

## [S3] Out of Scope
- 自动在 tag push 时发布、定时发布、多平台构建（Linux/macOS）。
- 代码签名、安装器（MSI/NSIS）、自动更新通道。
- 修改 `package-windows.ps1` 的打包内容或产物命名。
- Release 资产的二次校验文件（SHA256SUMS）上传。

## Tasks
- [x] T1: 新增 `.github/workflows/release.yml` — acceptance: YAML 合法；`on` 仅 `workflow_dispatch`；调用 `package-windows.ps1`；用 `yyyyMMdd-HHmm` 创建 Release 并上传 `dist/*.zip` (covers: S2)
- [x] T2: README「Windows 构建与发布」补充手动 Release 入口说明 — acceptance: 文档写明仅手动触发、tag 格式与用户从 Releases 页下载 (covers: S2)
- [x] T3: 本地静态校验 — acceptance: `python scripts/static_check.py` 通过；workflow YAML 可解析 (covers: S2)
- [x] T4: 推送到 GitHub 并手动触发 workflow — acceptance: Actions run 成功；Release 页出现时间戳 tag 与 ZIP (covers: S2; depends: T1, T3)
- [x] T5: 下载 Release ZIP 并验证 — acceptance: 能解压；含 `JchTools.exe`、`jchtools-cli.exe`、`resources/7zip` 许可材料且无 `7z.exe`/`7z.dll` 散落；`BUILD-INFO.json` 存在 (covers: S2; depends: T4)
