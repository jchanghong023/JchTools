# 文件

- [CodeAgent 本地 Token 自动维护](codeagent-maintenance.md) - 说明独立 CodeAgent 后台如何定时解密当前用户凭据、按变化更新注册表，以及状态隔离、维护退出和测试边界。
- [目录整理：只读分析到确认执行](directory-organization.md) - 说明目录整理如何从扫描、清理候选和完整哈希生成可审核的去重与归类计划，并在确认后执行。涵盖勾选依赖重算、分页就绪门禁和失败边界。
- [Git 逐文件提交推送与停止恢复](git-file-publishing.md) - 说明系统 Git 驱动的串行逐文件发布、upstream 预检、暂存保护和冲突恢复，以及代理、重试和取消边界。
- [转 Markdown：扫描、推理与产物提交](markdown-conversion.md) - 说明 GUI 批次如何检查场景、扫描文件、通过共享 Xberg 转换并安全提交 Markdown，以及超时、取消和验收边界。
- [Markdown 合并、拆分与原文件保护](markdown-organization.md) - 说明 MD 整理的扫描排序、标题转换、UTF-8 无损拆分，以及冲突确认和输出提交边界。
- [递归解压：引擎、队列与失败隔离](recursive-extraction.md) - 说明确认后的递归解压如何扫描、懒加载 7-Zip、处理嵌套包与分卷，并安全落位和处置原包。介绍取消、隔离失败及 GUI 失败列表与任务库的关系。
- [截图后台与 jchocr 图片识别入口](screenshot-and-cli-ocr.md) - 说明 Windows 截图服务与独立 jchocr 命令如何汇合到共享 Xberg，以及交互、协议、输入输出和隐私边界。
