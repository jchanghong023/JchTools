# 截图 OCR 同图对照

这里保留两张无敏感信息的合成截图、HTML 原稿、参考文本、渲染参数、冻结 Python 输出，以及 CER／布局比较工具。`normal-1920x1080.png` 与 `4k-3840x2160.png` 的 SHA-256 分别为 `1765f8d581a4b6c68d42ba18e1640d8f23e776d2e2bd8741c250b2e567e3f9df`、`6875c22dc1d78d7dda77eb4db4b39a11bc35fc67a9c09715848ed65a4934853d`。固定模型和字典的身份见 `docs/requirements/SNAP2TEXT.md` 附录 A。

## 冻结对照资料与报告工具

以下工具入口仍存在，但这里只说明用途，本次未执行。历史报告不是当前共享
Xberg 链路的验收结论（XB-02），冻结 TextSnap 及旧独立推理 runtime 的数值
不能证明当前模型身份、质量、速度或常驻。

`python scripts/make_tmp.py ocr-compare --model-root <含两个模型及 inference.yml 的目录> --font <固定字体> --force` 仅生成旧版资产 bundle，不执行 OCR，也不生成当前共享 Xberg 的报告。旧版报告曾由冻结 TextSnap 仓库的 `scripts/evaluate_ocr_fixture.py` 生成；Rust 报告曾由 worker 的 `ocr_compare` example 直连 ONNX Runtime 生成。该 example 已随旧直连推理路径退役，当前仓库没有此运行入口，不能再使用旧 `cargo run ... --example ocr_compare` 命令复跑。这里不提供未经实现的替代报告生成命令；冻结输出可继续作为比较输入，当前共享引擎的同图报告生成与对照仍未验证。产物和中间文件只写在 `.tmp/`。

对已经取得且来源明确的报告，可运行 `python tests/ocr_fixtures/compare.py --expected tests/ocr_fixtures/controlled-corpus.expected.txt --manifest tests/ocr_fixtures/corpus-manifest.json --python <旧版报告> --rust <待比较报告> --out .tmp/ocr-assets/compare.json`。该脚本只比较 JSON，不启动 worker 或 Xberg；`--rust` 参数名不证明报告来自当前生产链路。重复报告可多次传入 `--python`／`--rust`。报告列出紧凑 CER、含空白 CER、行数、空行、布局锚点的行列偏差和加载／识别耗时；速度结论应使用同机交替复测。锚点列按 East Asian Width 半角单元计（W/F 计 2、组合字符计 0，与合同附录 B 的 Rust 侧口径一致）。退出码约定：0 = Rust 紧凑 CER 与锚点行列偏差均在阈值内（默认 `--max-cer 0.02`、`--max-anchor-delta 1`，即附录 C 的「正文字符错误率 ≤ 2%」与「偏差 ≤ 一个半角单元或一行」；阈值只作用于 Rust，Python 是冻结参照只记录数值，含空白的 exact CER 同样只记录——冻结基线自身约 2.2%，以其设门会把逐字符一致的健康结果误判失败）；1 = 超阈，JSON 的 `failures` 逐条标注原因；2 = 命令行缺参；3 = 脚本执行异常（崩溃诊断，不产出 JSON）。`--self-test` 运行内嵌自测（锚点列宽度口径、阈值评估与输出路径守卫），不读写报告文件。比较退出码 0 只证明所提供报告的指标满足该门，不能替代真实模型、桌面或交付验收。

`--out` 与 `derive_dict.py` 的输出路径都必须位于仓库根的 `.tmp/` 之下（AGENTS.md §2），仓库外同名 `.tmp` 会被拒绝。

性能报告中的加载及识别耗时必须为有限非负数，且至少记录一次识别；`NaN`、无穷值、负耗时及空识别耗时列表按无效报告拒绝，不得产出通过结论或含非标准数值的 JSON。该输入回归包含在 `compare.py --self-test` 中。

从原 `PP-OCRv6_small_rec/inference.yml` 派生字典可运行 `python tests/ocr_fixtures/derive_dict.py <inference.yml> .tmp/ocr-assets/dict-check.txt`；脚本核对 18708 字、74947 字节和固定 SHA-256，输出同样锚定仓库根 `.tmp/`。需有 PyYAML。若将来删除冻结 Python 仓库，须另行保存其可运行源码和锁定依赖；这里保存的冻结输出只支持对已取得报告作既有质量基线比较，不能重新测量已删除程序的速度。

## 当前源码链路与现有验收入口（未运行）

- worker 的 `src/service.rs` 经 `SharedXbergClient` 识别；`src/shared_xberg.rs`
  将内存 PNG 的 base64 作为 `ocr_snapshot` 请求交给复用的 Xberg crate
  `crates/xberg/src/xberg_runtime.rs`／Windows 命名管道代理，不在 worker 内直连 det/rec
  runtime。启动预热使用 1×1 白图，随后要求 `snapshot_state` 为 `ready`。
  这是源码接线与就绪判据，不是当前真实模型推理或常驻的实测证据。
- `tests/xberg_shared_process.rs` 使用真实 JchTools 代理及命名管道，但
  `prepare` 安装的是 `tests/fixtures/shared_xberg.rs` 模拟引擎。其同 PID、
  请求归属、并用与取消断言只覆盖协议／进程控制；mock 同 PID 或一次
  `ready` 不证明真实模型未卸载重载。XB 第 5.2 节要求模型加载次数或等价
  观测，还要求真实长文档转换期间经热键／托盘完成截图及正文结果断言。
- 现有 worker 资产根探测入口是
  `python tests/ocr_fixtures/check_worker_root.py --asset-root <完整资产缓存> --worker-version <缓存中的 worker 版本>`。
  它要求 `JCHTOOLS_TEST_XBERG_DIR` 指定隔离 Xberg 测试目录及支持隔离覆盖的
  `test-hooks` 产物，将资产复制到 `.tmp/` 并写入隔离 SQLite 后启动服务，
  断言模型状态为 `ready`；可用 `--without-font-cache` 检查不依赖字体缓存。
  源码收尾调用 broker 停止并终结、等待本轮 worker，不是旧说明中的
  `shutdown` 协议。这是会注册全局热键的独立桌面探测，须当次明确授权；
  状态就绪不证明两张夹具的识别质量、模型常驻或完整桌面链路。
  服务启动前先读取 `--capabilities` 的 `test_hooks: true`；缺字段或生产构建
  一律拒绝，不保留旧生产管道探测回退。此守卫的安全回归入口为
  `python -m unittest discover -s tests/ocr_fixtures -p test_worker_root_isolation.py -v`，
  回归用合成能力响应证明拒绝发生在 `--service` 启动前，不注册热键。
  隔离服务的自启读写仅访问 `HKCU\Software\JchTools\Tests\SnapOcr\<资产根哈希>`，
  不读取或修改生产 Run 项；worker 的注册表 sentinel 回归为
  `cargo test --manifest-path optional/snap-ocr-worker/Cargo.toml --features test-hooks --lib service::tests::isolated_service_autostart_preserves_existing_sentinel -- --exact`。
- `check_gui_worker_connection.py --pid <主程序 PID>` 是现有真实窗口检查：
  切到截图 OCR 页，不点击连接按钮，等待当前后台连接／模型就绪文案，
  并断言旧按钮和文案不在场。它会设置焦点并点击侧栏，须当次独立桌面
  授权；不包含热键／托盘框选、识别正文、结果复制或交付目录断言。

本次只核对源码并修订说明，未运行上述工具、测试、真实模型、桌面或发布包。
当前共享引擎的同图质量／性能、模型加载与常驻、关闭／重开 GUI 和系统重启、
跨场景真实并用及取消、热键／托盘完整链路、多 DPI／多屏、Setup 实际安装与
便携 ZIP 运行均未验证；具体验收仍按 SNAP2TEXT 附录 C 与 XB 第 5.2／7.2 节。

历史资产根反例也仅适用于旧独立推理链路：v0.1.0 worker 返回
`{"model":"uninitialized"}`，当时主程序装在 `JchTools/data/snap-ocr`，
worker 却从 `JchTools/snap-ocr` 读取；v0.1.1 改为从 worker 自身安装位置
回溯资产根。该历史修复不能证明当前共享 Xberg 配置、模型加载或安装包通过。

## 历史记录：2026-09-27 同图结果（旧独立推理链路）

环境：Windows 11 专业版 10.0.26200、AMD Ryzen 9 9950X、Python 3.13.15、Rust 1.98.1；两端使用相同 PP-OCRv6 small FP32 ONNX 模型、18708 字字典和 ONNX Runtime 1.28.0。此机型不同于合同 O-04 所列 i7-13700 首轮目标。

| 截图 | Python / Rust 紧凑 CER | 最终布局文本 | Python / Rust 加载中位数 | Python / Rust 识别中位数 |
|---|---:|---|---:|---:|
| 1920×1080 | 0.324% / 0.324% | 逐字符一致；行序、缩进、空行、双栏位置一致 | 1.232s / 0.167s | 0.767s / 0.752s |
| 3840×2160 | 0.486% / 0.486% | 逐字符一致；行序、缩进、空行、双栏位置一致 | 1.263s / 0.178s | 1.382s / 1.322s |

1920×1080 使用 5 组 Python→Rust 交替进程复测；3840×2160 使用 3 次 Python 独立进程与 Rust 单进程连续 3 次识别。首轮 1080p 数值曾受运行波动影响，交替复测后未观察到速度退化。这是当时旧独立推理链路的记录，仅覆盖两张合成截图与无头链路；本次未复测，不证明迁移后的共享 Xberg 质量、性能或模型常驻，也不替代桌面、安装、DPI 和多屏验收。

## 历史记录：旧版真实桌面补充验证

2026-09-27，在 Windows 11 单显示器、窗口 DPI 144（150%）下，v0.1.1 worker 与修复后的主程序完成热键 `Ctrl+Shift+F12` → 框选合成文字窗口 → 识别 → 结果窗「复制全部」；剪贴板为 `HELLO OCR 2026\nSECOND LINE 12345`。主界面的「截图识别」进入框选后按 Esc，框选窗口关闭且服务返回 `idle`。默认 `Ctrl+Alt+O` 在本机与现有全局快捷键冲突时，主界面明确报告启动失败；改用空闲热键后服务成功启动。`check_gui_worker_connection.py --pid <主程序 PID>` 的真实窗口回归在修复前报告 `os error 231`，修复后报告 `PASS ... 模型 ready`；原因是单实例管道在前一次响应断开期间，下一次连接必须短暂等待。（该段记录的是当时「手动重连」版脚本的行为。）

2026-10 起主界面改为自动连接后台（XB-22，设置页仅保留「重试后台连接」按钮），`check_gui_worker_connection.py` 已改写为上节所述当前状态检查。源码存在及文案同步不表示该入口本次已运行，更不表示上述 2026-09-27 桌面结果已在当前共享引擎链路复现。

当时桌面验证未覆盖多显示器、其他 DPI 或 Setup 实际安装；当时 Setup 仅做完整性检查与解包内容比对，ZIP 与 Setup 干净目录内容相同、重资产依清单按需下载的描述也是历史记录。本次未核验这些产物；包内容相同不能证明当前安装版或便携版运行通过。
