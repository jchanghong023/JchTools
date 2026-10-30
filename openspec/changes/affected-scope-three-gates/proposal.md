# Proposal

## Why

用户本次明确要求所有三级测试尽量只运行受代码改动影响的测试，以缩短验证时间。现有 `use-cargo-workspace-nextest` 规定每轮覆盖全部范围，当前实现也依次运行根 workspace 默认成员及两个 optional 包；本变更替代其每轮全范围要求，而不是声称现有实现已具备影响选测。

## What Changes

- fastcheck、fulltest、slowtest 默认依据可信基线、工作区差异和依赖关系选择可证明的受影响范围；无法证明安全排除时保守扩大或全量。fastcheck 仍不运行测试，全局或不可局部化的检查不省略。
- 采用 cargo-delta 处理 Git 差异、文件归属和依赖影响选包，Cargo / nextest 负责执行；优先上游正式发布版本，不维护私有 fork 或补丁，不自研第二套影响算法，仅保留必要接入和全局输入、失败及未知影响的保守兜底。
- fulltest 与 slowtest 每轮实际执行选中 crate 的全部适用测试，保留相关外部引擎、媒体与隔离 GUI；slowtest 只执行一次相同受影响功能覆盖，并保留在线供应链审计、完整发布构建和 EXE / ZIP 交付检查。显式完整验收或保守兜底才全量，完整验收矩阵继续独立存在。
- 不恢复 coverage 插桩、逐用例成功结果复用、自研结果账本或 cargo-affected；Cargo 编译缓存可保留。不弱化测试断言、选中范围的 feature / profile / target、授权、CI、并发、资源隔离或失败报告要求。
- 报告基线、范围选择及排除和扩大原因、聚合数量及耗时；无受影响测试须有证据并明确报告，不冒报整仓或其他独立验收通过。

## Capabilities

### New Capabilities

无。

### Modified Capabilities

- `application-contract`：调整 P-12 的三级门影响范围及验收证据、P-15 的专项测试选择，保留完整交付验证。

## Impact

影响本地质量门计划与测试选择的实现、测试及工程说明，不改变产品功能或公开 API。用户已授权引入 cargo-delta 并连续运行两次 fulltest；结果须待实际运行后如实报告，不能提前宣称通过。历史 change 保留。基线可信性、依赖及动态输入无法证明时须保守扩大范围，未获独立授权的验收仍不得执行或冒报通过。没有待用户决定的范围问题。
