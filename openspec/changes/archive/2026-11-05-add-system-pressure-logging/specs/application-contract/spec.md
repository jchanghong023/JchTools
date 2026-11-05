# Spec Delta

## MODIFIED Requirements

### Requirement: P-02

JchTools MUST 遵守本条行为：工具经统一注册表（id / 名称 / 类别 / 摘要）接入，侧栏按注册表显示真实工具，不设空占位分类。

#### Scenario: P-02 完整行为与适用边界
- **WHEN** 用户启动或使用 JchTools、执行文件任务或进行相应交付验收，且涉及本条规定的条件
- **THEN** 工具经统一注册表（id / 名称 / 类别 / 摘要）接入，侧栏按注册表显示真实工具，不设空占位分类。保留的九个工具为递归解压、目录整理、MD 整理、转 Markdown、Git 工具、截图 OCR、CodeAgent、定时任务、系统压力记录；递归解压和目录整理归「文件」，MD 整理、转 Markdown 和截图 OCR 归「文档」，Git 工具、CodeAgent、定时任务和系统压力记录归「开发」。各工具保持独立，既有六工具关系按 H-03，CodeAgent 按 CA 分区，定时任务按 ST 分区，系统压力记录按 SPL 分区；转换、截图的功能边界分别由 T / O 分区维护，共享组件的独立「设置」入口按 XB-20 管理，不将内部后台进程作为另一套 GUI 产品。
