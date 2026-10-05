# 截图 OCR 同图对照

这里保留两张无敏感信息的合成截图、HTML 原稿、参考文本、渲染参数、冻结 Python 输出，以及 CER／布局比较工具。`normal-1920x1080.png` 与 `4k-3840x2160.png` 的 SHA-256 分别为 `1765f8d581a4b6c68d42ba18e1640d8f23e776d2e2bd8741c250b2e567e3f9df`、`6875c22dc1d78d7dda77eb4db4b39a11bc35fc67a9c09715848ed65a4934853d`。固定模型和字典的身份见 `docs/requirements/SNAP2TEXT.md` 附录 A。

用 `python scripts/make_tmp.py ocr-compare --model-root <含两个模型及 inference.yml 的目录> --font <固定字体> --force` 生成旧版测试 bundle。旧版报告由冻结 TextSnap 仓库的 `scripts/evaluate_ocr_fixture.py` 针对这里的 PNG 与 `controlled-corpus.expected.txt` 生成；本仓库的 Rust 报告由 `cargo run --release --manifest-path optional/snap-ocr-worker/Cargo.toml --example ocr_compare -- <det.onnx> <rec.onnx> <dict.txt> <image.png> .tmp/ocr-assets/rust.json 3` 生成，运行前设置 `ORT_DYLIB_PATH` 指向固定的 ONNX Runtime DLL。两者产物和中间文件只写在 `.tmp/`。

随后运行 `python tests/ocr_fixtures/compare.py --expected tests/ocr_fixtures/controlled-corpus.expected.txt --manifest tests/ocr_fixtures/corpus-manifest.json --python <旧版报告> --rust <新版报告> --out .tmp/ocr-assets/compare.json`。重复报告可多次传入 `--python`／`--rust`。报告列出紧凑 CER、含空白 CER、行数、空行、布局锚点的行列偏差和加载／识别耗时；速度结论应使用同机交替复测。锚点列按 East Asian Width 半角单元计（W/F 计 2、组合字符计 0，与合同附录 B 的 Rust 侧口径一致）。退出码约定：0 = Rust 紧凑 CER 与锚点行列偏差均在阈值内（默认 `--max-cer 0.02`、`--max-anchor-delta 1`，即附录 C 的「正文字符错误率 ≤ 2%」与「偏差 ≤ 一个半角单元或一行」；阈值只作用于 Rust，Python 是冻结参照只记录数值，含空白的 exact CER 同样只记录——冻结基线自身约 2.2%，以其设门会把逐字符一致的健康结果误判失败）；1 = 超阈，JSON 的 `failures` 逐条标注原因；2 = 命令行缺参；3 = 脚本执行异常（崩溃诊断，不产出 JSON）。`--self-test` 运行内嵌自测（锚点列宽度口径、阈值评估与输出路径守卫），不读写报告文件。

`--out` 与 `derive_dict.py` 的输出路径都必须位于仓库根的 `.tmp/` 之下（AGENTS.md §2），仓库外同名 `.tmp` 会被拒绝。

从原 `PP-OCRv6_small_rec/inference.yml` 派生字典可运行 `python tests/ocr_fixtures/derive_dict.py <inference.yml> .tmp/ocr-assets/dict-check.txt`；脚本核对 18708 字、74947 字节和固定 SHA-256，输出同样锚定仓库根 `.tmp/`。需有 PyYAML。若将来删除冻结 Python 仓库，须另行保存其可运行源码和锁定依赖；这里保存的冻结输出只支持继续复跑 Rust 与既有质量基线的比较，不能重新测量已删除程序的速度。

worker 资产根回归可运行 `python tests/ocr_fixtures/check_worker_root.py --asset-root <完整资产缓存> --worker-version v0.1.1`。脚本把资产复制到 `.tmp/`，用独立用户名和命名管道启动真实 worker，要求模型进入 `ready`，结束时通过 `shutdown` 退出。修复前的 v0.1.0 worker 返回 `{"model":"uninitialized"}`：主程序装在 `JchTools/data/snap-ocr`，旧 worker 却从 `JchTools/snap-ocr` 读取；v0.1.1 改为从 worker 自身安装位置回溯资产根。

## 2026-09-27 同图结果

环境：Windows 11 专业版 10.0.26200、AMD Ryzen 9 9950X、Python 3.13.15、Rust 1.98.1；两端使用相同 PP-OCRv6 small FP32 ONNX 模型、18708 字字典和 ONNX Runtime 1.28.0。此机型不同于合同 O-04 所列 i7-13700 首轮目标。

| 截图 | Python / Rust 紧凑 CER | 最终布局文本 | Python / Rust 加载中位数 | Python / Rust 识别中位数 |
|---|---:|---|---:|---:|
| 1920×1080 | 0.324% / 0.324% | 逐字符一致；行序、缩进、空行、双栏位置一致 | 1.232s / 0.167s | 0.767s / 0.752s |
| 3840×2160 | 0.486% / 0.486% | 逐字符一致；行序、缩进、空行、双栏位置一致 | 1.263s / 0.178s | 1.382s / 1.322s |

1920×1080 使用 5 组 Python→Rust 交替进程复测；3840×2160 使用 3 次 Python 独立进程与 Rust 单进程连续 3 次识别。首轮 1080p 数值曾受运行波动影响，交替复测后未观察到速度退化。上述结果只覆盖这两张合成截图与无头链路，不替代桌面、安装、DPI 和多屏验收。

## 真实桌面补充验证

2026-09-27，在 Windows 11 单显示器、窗口 DPI 144（150%）下，v0.1.1 worker 与修复后的主程序完成热键 `Ctrl+Shift+F12` → 框选合成文字窗口 → 识别 → 结果窗「复制全部」；剪贴板为 `HELLO OCR 2026\nSECOND LINE 12345`。主界面的「截图识别」进入框选后按 Esc，框选窗口关闭且服务返回 `idle`。默认 `Ctrl+Alt+O` 在本机与现有全局快捷键冲突时，主界面明确报告启动失败；改用空闲热键后服务成功启动。`check_gui_worker_connection.py --pid <主程序 PID>` 的真实窗口回归在修复前报告 `os error 231`，修复后报告 `PASS ... 模型 ready`；原因是单实例管道在前一次响应断开期间，下一次连接必须短暂等待。（该段记录的是当时「手动重连」版脚本的行为。）

2026-10 起主界面改为自动连接后台（XB-22，设置页仅保留「重试后台连接」按钮），`check_gui_worker_connection.py` 已同步改写为现状口径：脚本切到侧栏「截图 OCR」页后不点击任何连接按钮，等待「后台已连接 · 托盘和热键独立运行」与「模型：就绪」出现即 PASS；同时断言旧「启动 / 重连」「刷新连接」按钮与旧「截图服务已连接」文案不在场，脚本期望与 `ui/app.slint` 当前文案保持一致。

此桌面验证尚未覆盖多显示器、其他 DPI 或 Setup 实际安装；Setup 仅做完整性检查与解包内容比对。ZIP 与 Setup 的干净目录内容相同，重资产依清单按需下载。
