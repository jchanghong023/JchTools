# Proposal

## Why

用户要求基于 Cargo 与 cargo-nextest 缓存单个 Rust 测试用例的结果，减少输入未变时的重复执行；不得将整轮门或静态检查阶段当作缓存对象，不清除现有 Cargo 或 coverage 缓存。

## What Changes

- 缓存单位是 Cargo 与 cargo-nextest 运行的单个 Rust 测试用例，不缓存或跳过整个 fastcheck、fulltest、slowtest 或非测试检查阶段；fastcheck 不运行测试，不虚构测试缓存。
- 当修改文件 A 时，按 cargo-affected 与 Cargo 的依赖证据仅使依赖 A 的测试用例结果失效；无关用例继续复用。依赖无法确定、新增、失败或环境失效的用例必须保守重跑，不能伪造缓存命中。
- `--full` 保持全量实际执行语义；真实外部状态与交付须现场核验，每轮分别报告实际运行与缓存命中数量、失败及阶段耗时。
- 修改 P-12 当前禁止结果缓存及每轮实际执行的约束；保留全量覆盖、原生选测、安全隔离与授权边界。

## Capabilities

### New Capabilities

无。

### Modified Capabilities

- `application-contract`：Rust 逐用例结果缓存、失效和可观察结果。

## Impact

本地门的 Cargo / cargo-nextest 执行与逐用例结果记录、原生选测及开发说明；不改变 CI、真实桌面或发布授权。用户已明确要求修改需求并立即实现；实现及验证状态另行报告。
