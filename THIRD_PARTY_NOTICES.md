# Third-party notices

Original JchTools code is MIT-licensed. This does not relicense dependencies.

## Slint 1.17.1

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

`Cargo.toml` declares dependency constraints; the committed `Cargo.lock` pins the exact versions. `.cargo/config.toml` resolves registry sources from the repository's `vendor/`, including direct, transitive, build and Rust test dependencies. `vendor/SOURCES.json` records crate archive URLs and hashes, original authors, license declarations, upstream commits and retained notice files. Published archives are unmodified; omitted upstream license material is stored separately with its exact origin and digest. The packaging script gathers notices from the resolved graph into `third-party-rust` and copies recorded supplemental material and `SOURCES.json`. Entries with neither packaged nor supplemental license files retain their upstream absence notes and require attention before redistribution; no author grant is invented. This collection is not a substitute for reviewing distribution obligations.

## ACP HTTP implementation

Axum 0.8.9 is MIT-licensed: https://github.com/tokio-rs/axum/tree/axum-v0.8.9 . Official Agent Client Protocol Rust SDK 3.2.0 is Apache-2.0-licensed: https://github.com/agentclientprotocol/rust-sdk/tree/v3.2.0 . Their exact published source trees and all locked dependency sources are under `vendor/`; per-package notices and upstream supplements are identified by `SOURCES.json`. JchTools implements the Rust adapter between these components. No vLLM Router inference implementation or Python/Node HTTP backend is bundled.

## Microsoft APIs

Windows operations use the `windows` and `windows-sys` bindings. Windows itself is not bundled.

IFileOperation flags: https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperation-setoperationflags

Administrative MSI extraction: https://learn.microsoft.com/en-us/windows/win32/msi/administrative-installation


## Inno Setup Chinese Simplified language file

installer/ChineseSimplified.isl is vendored from the official Inno Setup repository (jrsoftware/issrc, main branch, Files/Languages/ChineseSimplified.isl; messages for Inno Setup 6.5.0+). It is redistributed here so the installer builds reproducibly on machines whose Inno Setup installation does not ship this file. The file remains subject to the Inno Setup license: https://jrsoftware.org/files/is/license.txt

## 内置截图字体

© 2014–2021 Adobe (http://www.adobe.com/). Noto Sans Mono CJK SC Regular（Noto CJK Sans 2.004），SIL Open Font License 1.1。字体内置于 snap-ocr-worker.exe，许可证全文随安装包及便携 ZIP 的 resources/fonts/LICENSE-noto-ofl.txt 提供。来源：https://github.com/notofonts/noto-cjk/releases/tag/Sans2.004 。固定字节大小及 SHA-256 见 resources/snap-ocr-assets.json。

## Portable Git for Windows

The installer and portable ZIP include the complete official Portable Git 2.56.0.windows.2 runtime under resources/git, including its upstream LICENSE.txt and component license material. The exact release URL, archive size, and official GitHub SHA-256 digest are recorded in resources/git/JCHTOOLS-BUNDLE.json. Upstream source: https://github.com/git-for-windows/git/tree/v2.56.0.windows.2 ; build and component sources: https://github.com/git-for-windows/build-extra . Original Git and component licenses remain in force.
