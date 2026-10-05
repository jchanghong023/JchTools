# Xberg 本地联调记录（非需求、非发布结论）

## 对照与边界

- 发布对照：`v2026.10.4-2214-run57.1`，使用已缓存的官方发布物与其模型/DLL，不重复下载。`E:\xberg` 仅本次经用户授权本地构建；未发布、未触发 CI。
- 本地标准 feature 集：`formats-no-heic,core-cli,analysis,ocr,paddle-ocr,transcription,api`。不得把本地修补二进制冒充官方发布物。
- 未修改 Xberg 的金标准、阈值或基线。2026-10-05 用户明确删除独立断网测试；该项已从 JchTools 验收清单删除，不是改判为通过。产品的离线功能与隐私约束不变。

## 真实引擎问题

1. **健康的独立 JBIG2 无法进入完整图像链路。** 合成输入是完整单页 JBIG2，320×96，文字 `JBIG2 SYNTH CHECK`，SHA-256 `dff2c7ab69b032183726f41563994ee40c1c5c645d1732c6d639a8d87d53eeea`。用独立 MuPDF 解码，逐像素与合成位图核对，不使用 Xberg 输出自证。发布对照的真实 CLI 报 `Could not determine image format`。原因是元数据入口仅覆盖通用图像解码器；默认 PNG 重编码也未接入 JBIG2。修补补齐带安全限制的 JBIG2 解码、元数据与 PNG 重编码入口。
2. **文件入口绕过大文档自动模式。** `large_501_pages.pdf` 超过 `auto_fast_pages=500`，发布对照的 GUI/worker 产物未披露 `auto_mode`。修补在文件入口按原始内容长度与真实 PDF 页数选择模式，命中时保留可观察的自动降级警告；缓存键隔离切换前的结果。显式 normal 不套用自动降级。
3. **格式清单宣告未编译的 HEIF/HEIC/AVIF 家族。** 标准 `formats-no-heic` 构建没有这些解码器，实际转换报 `UnsupportedFormat`，不属于健康语料的内容质量失败。修补从已编译解码能力登记 MIME/扩展名，CLI 与 worker 使用同一登记结果；JchTools 固定清单同步移除这七项，不将未编译功能算作可支持格式。
4. **省略 images 配置时，PNG 缺省值未生效。** 真正的 worker 回复保留原始 `JBIG2` 图像；公开文件入口回归修补前失败：`left: "JBIG2", right: "png"`。原因是输出处理只在 `config.images=Some(...)` 时执行。修补 async/sync 两条管线，使省略配置与默认图像配置都生成 PNG，显式 native 保留源字节。worker 缺省使用引擎 Markdown，而非交互式 CLI 的纯文本展示缺省值，以保留真实图片引用。
5. **无 EXIF 的格式被误判为元数据损坏。** 健康 JBIG2 回复含 `EXIF metadata extraction failed: failed to read media source: unsupported media format`。修补按 `nom-exif` 的类型分类区分 `UnsupportedFormat` / `ExifNotFound` 与损坏、截断、I/O 错误，不吞掉损坏 EXIF；真实 JPEG 的无 EXIF / 损坏 EXIF 回归通过。
6. **JPEG 2000 已能提取，但 PNG 输出被错误提前排除。** 真实 A25 GUI 对 `sample.jpg2` / `sample.jpc` 披露 `image_encoder：cannot re-encode format 'JPEG2000': no decoder available`。输出处理的 `is_untranslatable` 无条件排除了 JPEG 2000；其余别名只进入通用解码器。修补复用既有带安全限制的 JP2/J2K 解码器，覆盖魔数与别名；native 保留、损坏输入和解码/编码峰值限制仍有四项回归。

## 夹具纠正（不是引擎问题）

原 `matrix/jbig2_standalone.jb2` 的头声明存在页数，却缺少页数字段，不能作为健康输入。以 `tests/markdown_fixtures/generate_synthetic.py` 从零生成的健康单页 JBIG2 代替。损坏输入仍须明确失败；没有将原损坏文件改判为成功。

原 `sample.hwp` 虽含 BodyText/Section0，但正文使用带 zlib 头的数据、空段落头和错误的文字记录标签（0x51 而非 PARA_TEXT=0x43），不能作为健康 HWP5。现从零生成无压缩 HWP5，遵循 MS-CFB 长度优先的目录排序和红黑树规则，由独立 olefile / pyhwp 读取完整记录，并由未修改的真实引擎提取 `HWP SYNTH CHECK`。HWP 引擎的 Windows 分隔符假设没有被实测支持，不据此改代码。

## 当前验证证据

- Rust 公开边界与安全回归：2026-10-05，本地 Windows，公开 PNG/native 1、公开 auto_mode 4、探测边界 3、EXIF 分类 1、编译格式能力 1、图像安全入口审计 1，全部通过。JBIG2 解码与安全回归此前 11 项通过。
- 本地二进制 SHA-256：`4215e28ab087eab4f578cc13bf1e4aff3a3f20149e4ae8113fbd3848517bbe88`。真实零配置 worker 单进程完成健康 JBIG2 OCR、PNG 逐像素对照、Markdown 图片引用、501 页 auto/normal 隔离、损坏头失败与正常 shutdown；健康 JBIG2 无警告，命令退出码 0，耗时 48.30 秒。
- JPEG 2000 修补后的二进制 SHA-256：`a35f01338c4d679cc9fd2d4c93d3e9f7244ae118fd6b1c3844ef3ee9b60adf39`。真实 worker 对独立 4×3 RGB JP2/J2K 原始像素、PNG 元数据与 Markdown 引用逐项核对，同时验证健康 / 原损坏 HWP 和正常 shutdown；退出码 0，耗时 3.21 秒。
- 最终二进制的 `fulltest.py --keep-going --strict --deep` 使用本地编译 EXE 与已缓存发布资产：25/25 PASS（含 2 个音视频），WARN=0、FAIL=0，退出码 0，总墙钟 308.81 秒；报告内部计时 304.8 秒。实际 JSON 报告的基线对比新增 0、恶化 0。当前标准语料的 `_adversarial/` 目录不存在，不能将报告的 `adversarial_s=0` 冒充该三类历史对抗语料已执行；损坏 JBIG2/HWP 的本次 worker smoke 是独立证据。
- 历史基线的金标准 SHA 与当前金标准不同，脚本仍打印比较提示；未静默重设基线，不能将 42 项历史「已修复」计数全部归因于本次修补。上述 WARN=0 指质量报告计数，不指屏蔽所有终端提示。
- 删除断网测试前最后一轮 JchTools 完整门：822.1 秒，已执行项全部通过；Markdown 验收 44 PASS、0 FAIL、1 NOT RUN，唯一未执行项是独立断网测试，整门如实返回 `UNVERIFIED` / 退出码 1。按用户随后明确决定删除该项及两项专属 Python 测试后，重新运行完整门；旧结果不冒充新范围的全门通过。
- JchTools 本机 32 个逻辑核，未设 `RUST_TEST_THREADS` 时完整门的 Git 41 例中四个 30 秒阶段等待超时。`RUST_TEST_THREADS=8` 的单独 41 例曾全过（115.79 秒），但后续完整门仍有三例超时（38 PASS、3 FAIL、192.75 秒）；不能把单独通过当作原因已修复。等待窗口从整条任务开始计时，包含到达 add/fetch/merge/推送失败报告前的多条命令，不是只给目标命令计时。
- 使用标准库真实 `.cmd` + Git、每线程独立仓库、128 条命令的临时探针，拆分 spawn / 子进程执行 / 管道回收：D 盘 8 并发 spawn p95=0.568 秒、child p95=0.810 秒；4 并发分别 0.190/0.445 秒，管道回收均近零、无丢弃读线程。另一次系统临时目录对照的 8 并发 p95 又降低至 0.190/0.284 秒，说明启动/执行延迟有环境波动，不足以归因给具体 CPU、磁盘或防护软件。未观察到旧测试 wrapper 孤儿进程；没有凭猜测修改产品进程捕获。
- 最终完整门采用保守的 `RUST_TEST_THREADS=2` 并记录真实 Git Trace2 时间线，为整条命令链的 30 秒同步窗口留出资源裕量；不改断言、时间阈值、平台/feature 或用例数。
- 411.7 秒结束的完整门在 S13 记录「成功 0、失败 1、已有结果跳过 1」。真实原因是 `gui_smoke.py::start_conversion_and_wait_done` 看到「停止任务」后只 `break` 内层轮询，外层再次点击开始；首轮成功产物在第二轮被跳过。修补运行态分支立即等待本轮收尾，不改统计/产物断言。修补前上述真实 GUI 失败，修补后独立 S13 实际桌面通过（退出码 0、6.18 秒），既有 10 项 GUI 驱动 UT 全部通过；后续 822.1 秒完整门中的 S13 也通过。
- 删除独立断网测试后的完整门 632.6 秒：Markdown 实际 44 PASS、0 FAIL、0 NOT RUN，但 `acceptance.ps1` 将汇总文字 `NOT RUN 0` 误判成未执行。修补仅匹配真实条目状态行，真实未执行仍保持 PARTIAL。永久回归通过 PowerShell AST 执行脚本原分类分支：修补前零跳过用例明确失败（返回 PARTIAL 而非 PASS），修补后零跳过与真实跳过两例均通过。
- 后续完整门 672.9 秒：43 PASS、1 FAIL、0 NOT RUN；A24 遇到 UIA `COMError(-2147220991)`。原报告没有堆栈，不能确认实际抛出点；单独带堆栈诊断的 A24 正常通过，不把它当作修复证据。发现按钮等待循环的控件查询没有使用已有瞬态错误边界：向真实桌面 A24 首次按钮查询注入同类 COM 错误，修补前退出码 1；将幂等查询纳入原等待期限后，真实转录、停止、产物与失败隔离断言全部通过，退出码 0、21.63 秒。没有增加任务重试，也不重试已发出的启动/停止命令；原始异常是否确属此查询仍为推断。
- 最终 JchTools Windows `python scripts/test_gate.py fulltest --authorized`（显式本地 Xberg 运行目录、`RUST_TEST_THREADS=2`、外层整门 900 秒硬超时）：退出码 0，总墙钟 654.7 秒，13 个阶段全部 PASS。最终 JSON：Markdown 44 PASS、0 FAIL、0 NOT RUN；覆盖真实 GUI、引擎、截图组件、媒体转录及安装版/便携版公开入口。本次不触发 CI、不发布；通过结论仅限上述本地验证范围。

## 可复现入口

```powershell
python tests/markdown_fixtures/generate_synthetic.py --check
# 使用明确指定的发布对照或本地修补 xberg.exe，不使用 PATH 中不明来源的引擎。
xberg.exe extract tests/markdown_fixtures/matrix/jbig2_standalone.jb2 --format json
xberg.exe formats --format json
```

模式问题须以真实 `worker` 的 `extract` 请求省略 mode（auto）或携带 `mode:"normal"` 对同一 501 页夹具对照；检查产物中的 `processing_warnings`，不能以界面任务完成标记冒充披露证据。每轮完整 fulltest 的总墙钟硬上限保持 900 秒，超时终止整个所属进程树并判失败。
