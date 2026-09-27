# 截图 OCR 同图对照

这里保留两张无敏感信息的合成截图、HTML 原稿、参考文本、渲染参数、冻结 Python 输出，以及 CER／布局比较工具。`normal-1920x1080.png` 与 `4k-3840x2160.png` 的 SHA-256 分别为 `1765f8d581a4b6c68d42ba18e1640d8f23e776d2e2bd8741c250b2e567e3f9df`、`6875c22dc1d78d7dda77eb4db4b39a11bc35fc67a9c09715848ed65a4934853d`。固定模型和字典的身份见 `docs/requirements/SNAP2TEXT.md` 附录 A。

用 `python scripts/make_tmp.py ocr-compare --model-root <含两个模型及 inference.yml 的目录> --font <固定字体> --force` 生成旧版测试 bundle。旧版报告由冻结 TextSnap 仓库的 `scripts/evaluate_ocr_fixture.py` 针对这里的 PNG 与 `controlled-corpus.expected.txt` 生成；本仓库的 Rust 报告由 `cargo run --release --manifest-path optional/snap-ocr-worker/Cargo.toml --example ocr_compare -- <det.onnx> <rec.onnx> <dict.txt> <image.png> .tmp/ocr-assets/rust.json 3` 生成，运行前设置 `ORT_DYLIB_PATH` 指向固定的 ONNX Runtime DLL。两者产物和中间文件只写在 `.tmp/`。

随后运行 `python tests/ocr_fixtures/compare.py --expected tests/ocr_fixtures/controlled-corpus.expected.txt --manifest tests/ocr_fixtures/corpus-manifest.json --python <旧版报告> --rust <新版报告> --out .tmp/ocr-assets/compare.json`。重复报告可多次传入 `--python`／`--rust`。报告列出紧凑 CER、含空白 CER、行数、空行、布局锚点的行列偏差和加载／识别耗时；速度结论应使用同机交替复测。

从原 `PP-OCRv6_small_rec/inference.yml` 派生字典可运行 `python tests/ocr_fixtures/derive_dict.py <inference.yml> .tmp/ocr-assets/dict-check.txt`；脚本核对 18708 字、74947 字节和固定 SHA-256。需有 PyYAML。若将来删除冻结 Python 仓库，须另行保存其可运行源码和锁定依赖；这里保存的冻结输出只支持继续复跑 Rust 与既有质量基线的比较，不能重新测量已删除程序的速度。

worker 资产根回归可运行 `python tests/ocr_fixtures/check_worker_root.py --asset-root <完整资产缓存> --worker-version v0.1.1`。脚本把资产复制到 `.tmp/`，用独立用户名和命名管道启动真实 worker，要求模型进入 `ready`，结束时通过 `shutdown` 退出。修复前的 v0.1.0 worker 返回 `{"model":"uninitialized"}`：主程序装在 `JchTools/data/snap-ocr`，旧 worker 却从 `JchTools/snap-ocr` 读取；v0.1.1 改为从 worker 自身安装位置回溯资产根。

## 2026-09-27 同图结果

环境：Windows 11 专业版 10.0.26200、AMD Ryzen 9 9950X、Python 3.13.15、Rust 1.98.1；两端使用相同 PP-OCRv6 small FP32 ONNX 模型、18708 字字典和 ONNX Runtime 1.28.0。此机型不同于合同 O-04 所列 i7-13700 首轮目标。

| 截图 | Python / Rust 紧凑 CER | 最终布局文本 | Python / Rust 加载中位数 | Python / Rust 识别中位数 |
|---|---:|---|---:|---:|
| 1920×1080 | 0.324% / 0.324% | 逐字符一致；行序、缩进、空行、双栏位置一致 | 1.232s / 0.167s | 0.767s / 0.752s |
| 3840×2160 | 0.486% / 0.486% | 逐字符一致；行序、缩进、空行、双栏位置一致 | 1.263s / 0.178s | 1.382s / 1.322s |

1920×1080 使用 5 组 Python→Rust 交替进程复测；3840×2160 使用 3 次 Python 独立进程与 Rust 单进程连续 3 次识别。首轮 1080p 数值曾受运行波动影响，交替复测后未观察到速度退化。上述结果只覆盖这两张合成截图与无头链路，不替代桌面、安装、DPI 和多屏验收。
