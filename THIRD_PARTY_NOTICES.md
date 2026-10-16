# Third-party notices

Original JchTools code is MIT-licensed. This does not relicense dependencies.

## Slint 1.18.1

Slint is separately licensed. This project intends the Slint Royalty-free Software License for a desktop application, not a claim that Slint itself is MIT. Its `AboutSlint` component is included visibly in the About page for attribution. Review the exact Slint license texts packaged from the resolved crate before redistributing, including all required attribution conditions.

Official project and license reference: https://github.com/slint-ui/slint/blob/master/LICENSE.md

Rust integration: https://docs.slint.dev/latest/docs/rust/slint/

Palette and widget reference: https://docs.slint.dev/latest/docs/slint/reference/std-widgets/globals/palette/

The main window uses Windows system fonts. The screenshot result font is embedded in the shipped worker; see its attribution below.

## 7-Zip 26.03

The source repository intentionally contains no 7-Zip binaries: engine executables are kept out of git and out of the delivered packages by policy (contract E-01), not because the build environment could not download them. `scripts/fetch-7zip.ps1` fetches the fixed official full x64 MSI and corresponding source on a connected build machine (upstream SHA-256 verification, official domains only). The portable application uses unmodified `7z.exe` and `7z.dll` as a separate local process. It does not use the limited `7za` as a substitute for full RAR support.

7-Zip uses LGPL-2.1-or-later for most code, with BSD portions and the unRAR restriction. The packaging script includes upstream license texts and the corresponding complete source archive alongside the engine. Publisher should retain this folder and the generated manifest. Users must not be prevented from exercising third-party license rights; the manifest integrity mechanism is open source and can be regenerated for a lawful replacement engine.

Official download: https://www.7-zip.org/download.html

Official license: https://www.7-zip.org/license.txt

Official source: https://github.com/ip7z/7zip/releases/download/26.03/7z2603-src.tar.xz

Full x64 MSI: https://github.com/ip7z/7zip/releases/download/26.03/7z2603-x64.msi

## Rust dependencies

`Cargo.toml` declares dependency constraints; the committed `Cargo.lock` pins the exact versions. `.cargo/config.toml` resolves registry sources from the repository's `vendor/`; root `[patch.crates-io]` entries resolve locally repaired packages as ordinary path dependencies, so Cargo tracks local source changes rather than treating them as immutable registry releases. `vendor/SOURCES.json` records original crate archive URLs and hashes, original authors, license declarations, upstream commits and retained notice files. Its `source` and `original_upstream_source` strings retain the upstream registry origin; `effective_source` distinguishes the current registry identity from a locally patched path identity. A path package intentionally has no registry source or archive checksum in `Cargo.lock`; its original archive hash is retained for upstream provenance, not relabeled as the local tree's hash. Local source repairs are identified in global and per-package `project_source_modifications` lists; repaired packages' `.cargo-checksum.json` file digests describe the local trees, while their `package` digest still identifies the original upstream archive. This provenance does not claim production recompilation, runtime acceptance or execution of every target-specific feature. Omitted upstream license material is stored separately with its exact origin and digest. The packaging script gathers notices from the resolved graph into `third-party-rust` and copies recorded supplemental material and `SOURCES.json`. Entries with neither packaged nor supplemental license files retain their upstream absence notes and require attention before redistribution; no author grant is invented. This collection is not a substitute for reviewing distribution obligations.

`cfg_aliases` 0.2.2 的 `NOTICES.md` 包含其复用的 `tectonic_cfg_support::target_cfg!` 代码的 MIT 许可告知，与该 crate 的 `LICENSE` 一并登记在 `vendor/SOURCES.json` 中。`dispatch` 0.2.0 与 `drm-fourcc` 2.2.0 在发布归档及清单记录的上游提交树中均无独立许可证文件；清单保留此缺口，不以其他项目的 MIT 文本冒充原作者授权。`libseat` 0.2.4 与 `libseat-sys` 0.2.0（PolyMeilex/libseat-rs）同属此缺口：两者发布归档均无许可证文件，其各自清单记录的上游提交树（00a7f14a 与 cfc67eeb）完整递归枚举亦无任何许可证类文件，仅在 `Cargo.toml` 声明 MIT；上游许可证确实不可得，按同一先例在清单保留缺口说明。

本地修复的全局记录以 `package`、`version` 定位包，并保存 `files` 数组；对应包条目的 `project_source_modifications` 保存相同的文件记录。每项 `path` 相对该包的 `directory`，`reason` 说明源码、回归、文档或校验清单更新，不是上游已合入声明。原作者、上游提交及原始发布归档摘要保持不变；具体补丁归属以该清单为准。

## ACP HTTP implementation

Axum 0.8.9 is MIT-licensed: https://github.com/tokio-rs/axum/tree/axum-v0.8.9 . Official Agent Client Protocol Rust SDK 3.3.0 is Apache-2.0-licensed: https://github.com/agentclientprotocol/rust-sdk/tree/v3.3.0 . Their exact published source trees and all locked dependency sources are under `vendor/`; per-package notices and upstream supplements are identified by `SOURCES.json`. JchTools implements the Rust adapter between these components. No vLLM Router inference implementation or Python/Node HTTP backend is bundled.

## Microsoft APIs

Windows operations use the `windows` and `windows-sys` bindings. Windows itself is not bundled.

IFileOperation flags: https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperation-setoperationflags

Administrative MSI extraction: https://learn.microsoft.com/en-us/windows/win32/msi/administrative-installation


## Inno Setup Chinese Simplified language file

installer/ChineseSimplified.isl is vendored from the official Inno Setup repository (jrsoftware/issrc, main branch, Files/Languages/ChineseSimplified.isl; messages for Inno Setup 6.5.0+). It is redistributed here so the installer builds reproducibly on machines whose Inno Setup installation does not ship this file. The file remains subject to the Inno Setup license: https://jrsoftware.org/files/is/license.txt

## 内置截图字体

© 2014–2021 Adobe (http://www.adobe.com/). Noto Sans Mono CJK SC Regular（Noto CJK Sans 2.004），SIL Open Font License 1.1。字体内置于 snap-ocr-worker.exe，许可证全文随安装包及便携 ZIP 的 resources/fonts/LICENSE-noto-ofl.txt 提供。来源：https://github.com/notofonts/noto-cjk/releases/tag/Sans2.004 。固定字节大小及 SHA-256 见 resources/snap-ocr-assets.json。
