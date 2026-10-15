# 旧 Office 公开合成夹具

三个文件从空白文档生成，只含公开测试文字与数字，不含用户数据。

- `legacy.doc`：正文含 `JCHTOOLS-LEGACY-DOC` 与“公开合成文档：测试正文 123”。
- `legacy.xls`：Fixture 工作表含 `JCHTOOLS-LEGACY-XLS`、“公开合成表格”及数字 123。
- `legacy.ppt`：一页演示含 `JCHTOOLS-LEGACY-PPT` 与“公开合成演示：测试正文 123”。

2026-10-08 由 python-docx、openpyxl、python-pptx 创建现代 Office 容器，再经官方 LibreOffice 26.8.0 的 Word 97、Excel 97、PowerPoint 97 导出过滤器生成。生成工具只用于开发夹具，产品及验收运行不依赖这些工具。验收 A25 经真实 GUI 和 Xberg 验证三类正文与源文件不变。
