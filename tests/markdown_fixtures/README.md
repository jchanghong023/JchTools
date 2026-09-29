# 转 Markdown 回归样例

本目录的 16 个公开测试文件逐字节迁自旧项目 `all2markdown` 的
`tests/test_example/`（源提交 `118957872982e44b08ab430c20147f74cf3ef494`）。
用于 PDF、Office、图片 OCR、嵌入对象和媒体转录的回归验证，
不随 JchTools 安装包或便携包发布。`58325_db.xlsx` 展开后可能占用数 GB 内存，
不纳入日常快速测试。

## 历史接线记录（非当前验收结论）

以下记录迁自转 Markdown 需求文档，保留原有信息；本次文档整理未复跑该矩阵，也未核验所引临时报告。当前需求以 [ALL2MARKDOWN.md](../../docs/requirements/ALL2MARKDOWN.md) 及其引用的 XB 分区为准；临时报告可能已清理，历史通过记录不能替代当前提交的真实链路验收。

2026-09-29 接线记录（用户跨仓收尾授权）：文档转换清单与双侧推理组件清单已更新至 Xberg 发布 `v2026.9.29-0212-run49.1`；该出厂构建不含 layout-detection，`resources/markdown-xberg.json` 的 `layout` 配置块随之移除（出厂对未编译配置字段明确报错），OCR 与图片 OCR 行为不变，验收矩阵 A 组 15/15 可执行项全部通过（A08 p:p 段落提取、A17 PNM 六表示修复后全绿，见 `.tmp/markdown-acceptance-A2.json`）。
