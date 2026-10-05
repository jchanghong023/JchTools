# 转 Markdown 回归样例

本目录是转 Markdown 验收（`scripts/markdown_acceptance.py`）与 GUI 冒烟的公开
合成夹具目录。全部样本按需求合同 T-26 从零构造、无业务含义，不包含内部源文件
名、正文、OCR 结果、可识别元数据或可反查源文档的哈希。样本不随 JchTools
安装包或便携包发布。

## 合成来源与残留说明（T-26）

- 本目录的 `generate_synthetic.py` 从零重建以下受管夹具（`--check` 只做结构
  校验，不重写文件）：`test_hello_world.png`、`large_210_pages.pdf`（常规模式
  回归，低于引擎 `auto_fast_pages=500` 阈值）、`large_501_pages.pdf`（超过
  阈值，用于断言 `auto_mode` 降级披露）、`scanned_hello.pdf`（单页图像型扫描
  形态）、`matrix/format_sweep/sample.jsonl`（纯合成 JSONL）。
- `matrix/` 内的 Office/PDF 构造文件（如 `docx_all_sources.docx`、
  `docx_emf_wmf_raster.docx`、`pdf_repeat_softmask.pdf`）为接线期就地合成的
  无来源样本；`matrix/format_sweep/` 其余样本为 A25 格式清点布置的无敏感内容
  样本。
- 尚无法等价再合成、按现状保留的残留样本及原因：
  - `1706.03762.pdf`、`single_paper.pdf`：公开论文 PDF（原生文字版式），
    requirements-dev 未提供文本型 PDF 生成库（Pillow 只能生成图像页）；
  - `mixed_native_scanned.pdf`：需要原生文字页与扫描页混排，同样受文本型
    PDF 生成能力限制；
  - `58325_db.xlsx`：图表语料工作簿（当前无验收条目引用），Excel 二进制
    结构无生成库；展开后可能占用数 GB 内存，不纳入日常快速测试；
  - `bug62513.pptx`：LibreOffice 公开缺陷样本，含真实 WMF 图形内容，无法
    等价再生成；
  - `chartex.docx`、`merged_cells.docx`、`merged_table.pptx`、
    `merged_header.xlsx`、`docx_with_embedded_office.docx`、
    `pptx_with_embedded_office.pptx`、`sample_with_images.docx`：承载嵌入
    对象、合并表格/单元格与合成容器骨架的 OOXML 结构夹具，验收的容器改写
    （`_synth_office`/`_build_pptx`）依赖其原始部件集，机械替换会改变矩阵
    期望；
  - `video-to-notes-intro-zh.mp4`：真实中文语音样本，A24 的真实转录链路
    必需，无离线 TTS 等价替代；
  - `matrix/jpeg2000/`：JPEG 2000 家族公开样本，requirements-dev 无对应
    编码库；
  - `matrix/jbig2_standalone.jb2`：JBIG2 位图编码样本（A25 格式清点在场），
    同样无对应编码库，按现状保留。

## 历史接线记录（非当前验收结论）

以下记录迁自转 Markdown 需求文档，保留原有信息；本次文档整理未复跑该矩阵，也未核验所引临时报告。当前需求以 [ALL2MARKDOWN.md](../../docs/requirements/ALL2MARKDOWN.md) 及其引用的 XB 分区为准；临时报告可能已清理，历史通过记录不能替代当前提交的真实链路验收。

2026-09-29 接线记录（用户跨仓收尾授权）：文档转换清单与双侧推理组件清单已更新至 Xberg 发布 `v2026.9.29-0212-run49.1`；该出厂构建不含 layout-detection，`resources/markdown-xberg.json` 的 `layout` 配置块随之移除（出厂对未编译配置字段明确报错），OCR 与图片 OCR 行为不变，验收矩阵 A 组 15/15 可执行项全部通过（A08 p:p 段落提取、A17 PNM 六表示修复后全绿，见 `.tmp/markdown-acceptance-A2.json`）。
