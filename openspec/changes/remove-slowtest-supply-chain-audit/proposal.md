# Proposal

## Why

用户明确要求从 slowtest 删除供应链检查；现有 P-12 要求执行在线 pip-audit，需要同步调整。

## What Changes

- slowtest 不再执行 Python 依赖在线漏洞审计，移除仅为该阶段使用的质量工具依赖。
- 其他在线测试、发布构建、打包与产物校验保持不变；三级门通过不代表供应链漏洞审计通过。

## Capabilities

### New Capabilities

无。

### Modified Capabilities

- `application-contract`：取消 slowtest 的在线供应链审计要求。

## Impact

影响本地门计划及其文档，不改变 Rust 产品功能。批准依据：用户本次明确要求删除供应链检查。
