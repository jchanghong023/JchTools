# 转 Markdown 回归样例

本目录供转 Markdown 验收（`scripts/markdown_acceptance.py`）与 GUI 冒烟使用，
同时保留从零构造的合成夹具和既有公开／结构样本，不能统称为「全部合成」。
样本仅用于开发验收，不随 JchTools 安装包或便携包发布。

T-26 要求公开测试使用从零构造、无业务含义的合成夹具，禁止把内部文档裁剪、
匿名化或重打包后公开。`ALL2MARKDOWN.md` 附录 A 的默认范围说明及第 24 项
明确保留既有公开 MP4，用于真实中文转录、时间戳及停止验收；这项具体例外
不把其余历史残留自动视为符合 T-26，也不把公开文件认定为内部私密资料。
以下按生成方式与既有保留说明区分来源；保留说明不等于已完成逐文件来源、
许可、元数据或等价再合成能力的审计。

## 合成范围与既有残留说明（T-26 / 附录 A）

- 本目录的 `generate_synthetic.py` 从零重建以下受管夹具（`--check` 只做结构
  校验，不重写文件）：`test_hello_world.png`、`large_210_pages.pdf`（常规模式
  回归，低于引擎 `auto_fast_pages=500` 阈值）、`large_501_pages.pdf`（超过
  阈值，用于断言 `auto_mode` 降级披露）、`scanned_hello.pdf`（单页图像型扫描
  形态）、`matrix/format_sweep/sample.jsonl`（纯合成 JSONL）、
  `matrix/jbig2_standalone.jb2`（320×96 白底黑字，Pillow Group4 MMR 加完整
  JBIG2 单页顺序段流）、`matrix/format_sweep/sample.hwp`（无压缩 HWP 5.0.0.0，
  CFB v3 + miniFAT，正文为 `HWP SYNTH CHECK`）。JBIG2 以独立 MuPDF 渲染逐像素
  核对，不能用引擎自身解码结果当唯一真值。旧 JBIG2 样本声明存在页数却缺少
  页数字段，不再冒充健康输入。
  - HWP 按[Hancom 官方格式说明](https://tech.hancom.com/python-hwp-parsing-2/)构造
    `BodyText/Section0`：22 字节 `PARA_HEADER`（5.0.0.0 无后续版本扩展）、
    UTF-16LE `PARA_TEXT`、字形引用、行布局、区段定义及 A4 页面定义；DocInfo
    含文档属性、ID 映射、七语言字体、字形、制表位、段落样式和命名样式。
    所有字体/样式引用均有对应定义，无下载样本、旧正文或元数据。
  - 独立读取使用 `olefile 0.47` 与 `pyhwp 0.1b15`：CFB 流可枚举，版本为
    `(5, 0, 0, 0)`、flags=0，DocInfo 与正文模型完整解析，无残留字节；
    `ParaText` 独立解出区段控制、`HWP SYNTH CHECK` 和段落结束符。受管 `--check`
    对确定性容器、miniFAT、记录和正文逐字节断言，不以 Xberg 能接受为有效性定义。
    CFB 另按 [MS-CFB §2.6.4](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-cfb/d30e462c-5f8a-435b-9c4c-cc0b9ea89956)
    校验名称先按 UTF-16 长度、再按大写码点排序及红黑树性质；根子树顺序为
    `DocInfo < PrvText < BodyText < FileHeader`，并非普通字符串字典序。
    `BodyText` 黑节点左右为 `DocInfo` 黑节点和 `FileHeader` 黑节点；
    `DocInfo` 的右节点 `PrvText` 为红，所有路径黑高相等、无连续红节点。
  - 旧 HWP 虽有 Section0，却使用 zlib 包装压缩、零长度段落头和错误的
    `0x50/0x51` 段落/文本标签（规范为 `0x42/0x43`），不能作为健康格式样本。
    2026-10-05 替换前原件保留在 `.tmp/hwp-fixture-evidence/original-invalid.hwp`；
    原件 SHA-256 为 `73a47ed24329cca8a26936ba95501580ee127cdf9f63becf87d9057ec0738194`。
    仅替换格式输入，不改变 A25 判定或将失败重新分类。
- `matrix/` 内的 Office/PDF 构造文件（如 `docx_all_sources.docx`、
  `docx_emf_wmf_raster.docx`、`pdf_repeat_softmask.pdf`）及
  `matrix/format_sweep/` 的格式清点样本，不全在上述生成器的受管清单中。
  验收脚本的 `_synth_office` 会保留既有 OOXML 部件并改写内容类型；
  `_build_pptx` 也以 `merged_table.pptx` 为骨架构造变体。因此「构造／合成
  变体」不证明其整个容器从零生成，不能由此推断全目录满足 T-26。
- 既有残留样本及原保留原因（未在本次重新验证生成能力）：
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
  - `video-to-notes-intro-zh.mp4`：既有公开中文语音样本，A24 的真实转录、
    时间戳及停止链路使用它；这是附录 A 明确保留的 MP4 例外，不是合成
    语音，也不是内部私密输入。默认验收不要求安装 FFmpeg 或合成 M4A、
    无音轨等扩展场景，不能因无离线 TTS 而删除唯一样例；
  - `matrix/jpeg2000/`：JPEG 2000 家族公开样本，requirements-dev 无对应
    编码库。

## 当前入口与证据边界

`scripts/markdown_acceptance.py` 默认 `--profile common` 验收新老 Office、
PDF、MP4；`--profile full` 保留扩展矩阵，`--list` 仅列条目。脚本经 GUI
驱动核对转换结果，安装版／便携版条目为 C08／C09；夹具存在、生成器结构
检查或历史通过记录均不能证明当前真实 Xberg、GUI 或两种交付形态通过。
缺资产或未执行的必验条目（含 `--only` 排除项）为 `NOT RUN`，保留的
`OPTIONAL` 不计为通过。本次仅修订说明，未运行生成、结构检查或任何验收。

回归判据同时锁定夹具自身和产物：A02 的组形状与图片形状使用唯一 ID；
A17 的 P4 位图按最高位优先编码并与 P1 逐像素一致；A08 使用标准 DrawingML
`a:p` 文本段及合法字段 ID，字段值固定为 `7`，不能用任意非空字符冒充字段
保留。所有条目检查每个顶层输入最多一份结果，且最终正文非空；媒体链接按
Markdown URL 转义还原文件名后核对本地落盘，绝对路径（含转义后的 UNC）
在路径解析前拒绝。媒体 E2E 准备器的环境 JSON 不得覆盖其媒体、来源收据、
下载归档或已校验工具。

## 历史接线记录（非当前验收结论）

以下记录迁自转 Markdown 需求文档，保留原有信息；本次文档整理未复跑该矩阵，也未核验所引临时报告。当前需求以 [ALL2MARKDOWN.md](../../docs/requirements/ALL2MARKDOWN.md) 及其引用的 XB 分区为准；临时报告可能已清理，历史通过记录不能替代当前提交的真实链路验收。

2026-09-29 接线记录（用户跨仓收尾授权）：文档转换清单与双侧推理组件清单已更新至 Xberg 发布 `v2026.9.29-0212-run49.1`；该出厂构建不含 layout-detection，`resources/markdown-xberg.json` 的 `layout` 配置块随之移除（出厂对未编译配置字段明确报错），OCR 与图片 OCR 行为不变，验收矩阵 A 组 15/15 可执行项全部通过（A08 p:p 段落提取、A17 PNM 六表示修复后全绿，见 `.tmp/markdown-acceptance-A2.json`）。
