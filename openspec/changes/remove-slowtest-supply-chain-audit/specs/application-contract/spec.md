## ADDED Requirements

### Requirement: P-12 供应链审计移除

本地三级测试门 MUST NOT 执行原 `pyquality-pip-audit` 在线供应链审计；其余适用在线测试、发布构建与交付校验保持不变。测试门通过不得视为供应链漏洞审计通过。

#### Scenario: slowtest 发布交付门

- **WHEN** 执行 slowtest
- **THEN** 不运行 `pyquality-pip-audit`，继续按适用范围执行其他在线测试、发布构建和交付校验。
