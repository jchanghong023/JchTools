# Proposal

## Why

用户要求 fulltest 总墙钟至少降低 30%，同时保留有效验证。当前同口径基线为 90 项 worker 测试全部通过、总计约 57 秒；整个 worker 包被单并发分组，独立内存图像和私有 window station 用例也被串行化。P-12 的「worker 既有隔离不变」需要明确资源隔离与调度分组的区别。

## What Changes

- 允许经调用路径审核、仅使用进程内或测试专属资源的 worker 用例按显式白名单并行；名单外及新增用例默认串行。
- 真实共享引擎、后台控制、Job、会话锁、桌面、剪贴板和注册表隔离保持不变；不缩短等待、不修改断言、不删测、不复用旧成功结果。
- 前后必须使用相同原生选包、阶段、工具链、profile、features、target 和等价缓存状态，以本轮完整门的实际聚合数量、状态及墙钟证明收益。
- 本变更依据当前会话用户对完整优化、仓库任意文件修改及自主决策的明确授权执行；不声称用户逐字审阅了本提案。

## Capabilities

### New Capabilities

无。

### Modified Capabilities

- `application-contract`：明确 P-12 的 worker 资源隔离和显式名单并发边界。

## Impact

影响 `.config/nextest.toml`、验证说明及 P-12，不修改产品行为、全局线程上限、Cargo jobs 或选测算法。不扩大默认桌面、真实模型、网络或发布范围；30% 为待实测验收目标，不因配置变更自动视为达成。
