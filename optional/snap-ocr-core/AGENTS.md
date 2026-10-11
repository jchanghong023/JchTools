# AGENTS.md · snap-ocr-core（O 分区规格参照物）

本文件只承载本子树（`optional/snap-ocr-core/`）专属信息；全局纪律与需求权威分工见仓库根 [AGENTS.md](../../AGENTS.md)，规格条目本体见 [SNAP2TEXT.md](../../docs/requirements/SNAP2TEXT.md)（O-23～O-28 与附录 B）。本文件不复制需求条目。

## 职责与状态

截图 OCR 纯逻辑核心：按 SNAP2TEXT 附录 B 的确定性定义，从冻结 TextSnap（Python）翻译瓦片、几何、接缝合并、去重、方向策略与文本布局；模块与冻结仓库 `src/textsnap/` 一一对应。纯 lib、无 bin、零外部依赖（East Asian Width / 组合类表内嵌 `src/ucd_tables.rs`）、`publish = false`。

**当前是规格参照物，不是被依赖的库**：识别已迁至共享 Xberg，workspace 内没有任何 crate 依赖本 crate；本 crate 与 Xberg 侧实现是同一冻结规格的两份译文。`tests/ocr_fixtures/` 保留同图对照夹具及比较工具，但其文档中的 worker `ocr_compare` example 已不存在；当前完整真实引擎对照入口尚未确认，不把本 crate 单测或旧报告当成当前 OCR 验收。

## 命令

以下命令从仓库根目录执行，需要根文档所列 Windows Rust 构建条件。本包是 workspace 非默认成员，默认根构建/测试不覆盖；本地 `acceptance.ps1` 显式执行这两类检查，CI 不运行功能测试。列出命令不表示本次已验证：

```powershell
cargo test --manifest-path optional/snap-ocr-core/Cargo.toml --all-targets --features test-hooks
cargo clippy --manifest-path optional/snap-ocr-core/Cargo.toml --all-targets --features test-hooks -- -D warnings
```

测试全部为模块内 `#[cfg(test)]` 内联单元测试（无独立 `tests/` 目录），不需要模型即可运行。

## 改动与删除前置

- 不得往本 crate 加推理实现：`pipeline` 模块的识别与图像后端是 trait 注入，O-23 推理本体在 Xberg 侧。
- 删除前须确认 O-23～O-28 的规格约束已有其他执法点承接。无 path 依赖不等于可跳过规格承接；删除时同步根 `Cargo.toml` workspace、`scripts/acceptance.ps1` 及 `scripts/local_gate_plan.py` 的本地覆盖、根 AGENTS.md 登记和相关文档引用。CI 当前不显式测试本包，若实际交付引用有变则另查。
- 本 crate 自带与主包同口径的独立 lint 硬门禁（`[lints]` 内 `warnings`、clippy pedantic、`unwrap_used` / `expect_used` / `todo` 等全 deny；workspace lints 不自动继承到成员），编辑须维持零告警。
