# Proposal

## Why

现有本地测试门依赖实验性的 coverage 驱动逐用例选测及成功结果复用，增加工具和状态维护复杂度，也使增量通过不能代表本轮完整执行。用户本次明确批准以 Cargo Workspace 和 cargo-nextest 的简单稳定执行方式替代该策略。

## What Changes

- fulltest 和 slowtest 每轮实际执行所有适用测试，保持根 workspace 默认成员及两个 optional 包的三个顺序 nextest 范围，文档测试单独运行；局部测试不得冒报完整验收。
- 移除 `--full` 接口、coverage 选测工具及状态、成功用例结果复用和两轮零选中或加速验收要求；保留 Cargo 构建增量缓存。
- 本次用户指令替代 `cache-three-gate-results`、`remove-local-build-orchestration`、`optimize-fulltest-throughput` 中 coverage 选测、成功结果复用及两轮提速验收部分；其他隔离、并发和安全条款保留。

## Capabilities

### New Capabilities

无。

### Modified Capabilities

- `application-contract`：更新 P-12 测试门执行及证据要求，保留现有覆盖、授权及安全边界。

## Impact

影响本地测试门、工具配置和使用说明，不改变产品行为、测试断言或生产构建维度。fastcheck、fulltest、slowtest 分层和独立授权不变；真实桌面、真实模型、CI 和发布仍须单独授权。本次未获 fulltest / slowtest 授权，完整门未运行；不得提前宣称验收通过。无需新增能力或遗留兼容接口。无待决问题。
