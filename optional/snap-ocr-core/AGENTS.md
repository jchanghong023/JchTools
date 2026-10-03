# AGENTS.md · snap-ocr-core（O 分区规格参照物）

本文件只承载本子树（`optional/snap-ocr-core/`）专属信息；全局纪律与需求权威分工见仓库根 [AGENTS.md](../../AGENTS.md)，规格条目本体见 [SNAP2TEXT.md](../../docs/requirements/SNAP2TEXT.md)（O-23～O-28 与附录 B）。本文件不复制需求条目。

## 职责与状态

截图 OCR 纯逻辑核心：按 SNAP2TEXT 附录 B 的确定性定义，从冻结 TextSnap（Python）翻译瓦片、几何、接缝合并、去重、方向策略与文本布局；模块与冻结仓库 `src/textsnap/` 一一对应。纯 lib、无 bin、零外部依赖（East Asian Width / 组合类表内嵌 `src/ucd_tables.rs`）、`publish = false`。

**当前是规格参照物，不是被依赖的库**：识别已按 XB-01 / XB-02 迁至 Xberg 推理组件，workspace 内没有任何 crate 依赖本 crate；本 crate 与 Xberg 侧实现是同一冻结规格的两份译文。行为对齐由 `tests/ocr_fixtures/` 的同图对照链路验收——该链路经 `optional/snap-ocr-worker` 的对照 example 运行，不经过本 crate 的测试。

## 命令

本包在 workspace `members` 内但不在 `default-members`，仓库根的 `cargo build` / `cargo test` 不构建、不测试本包（与 `check.yml`、`acceptance.ps1` 同口径，可选分发不等于可选验证）：

```powershell
cargo test --manifest-path optional/snap-ocr-core/Cargo.toml --all-targets
cargo clippy --manifest-path optional/snap-ocr-core/Cargo.toml --all-targets -- -D warnings
```

测试全部为模块内 `#[cfg(test)]` 内联单元测试（无独立 `tests/` 目录），不需要模型即可运行。

## 改动与删除前置

- 不得往本 crate 加推理实现：`pipeline` 模块的识别与图像后端是 trait 注入，O-23 推理本体在 Xberg 侧。
- 删除前须确认 O-23～O-28 的规格约束已有其他执法点承接（当前本 crate 是这些冻结条文唯一的纯逻辑对照译文）。代码层面删除是孤儿安全的（无 path 依赖），但必须同步四处：根 `Cargo.toml` 的 workspace `members`、`.github/workflows/check.yml` 的本包两步、`scripts/acceptance.ps1` 的可选组件循环、根 AGENTS.md 对本子树的描述。
- 本 crate 自带与主包同口径的独立 lint 硬门禁（`[lints]` 内 `warnings`、clippy pedantic、`unwrap_used` / `expect_used` / `todo` 等全 deny；workspace lints 不自动继承到成员），编辑须维持零告警。
