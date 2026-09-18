# Third-party notices

Original JchTools code is MIT-licensed. This does not relicense dependencies.

## Slint 1.17.1

Slint is separately licensed. This project intends the Slint Royalty-free Software License for a desktop application, not a claim that Slint itself is MIT. Its `AboutSlint` component is included visibly in the About page for attribution. Review the exact Slint license texts packaged from the resolved crate before redistributing, including all required attribution conditions.

Official project and license reference: https://github.com/slint-ui/slint/blob/master/LICENSE.md

Rust integration: https://docs.slint.dev/latest/docs/rust/slint/

Palette and widget reference: https://docs.slint.dev/latest/docs/slint/reference/std-widgets/globals/palette/

No font files are redistributed; Windows system fonts are referenced by family name.

## 7-Zip 26.03

The source delivery contains **no 7-Zip binaries** because the build environment could not download them. `scripts/fetch-7zip.ps1` downloads the fixed official full x64 MSI and corresponding source. The portable application uses unmodified `7z.exe` and `7z.dll` as a separate local process. It does not use the limited `7za` as a substitute for full RAR support.

7-Zip uses LGPL-2.1-or-later for most code, with BSD portions and the unRAR restriction. The packaging script includes upstream license texts and the corresponding complete source archive alongside the engine. Publisher should retain this folder and the generated manifest. Users must not be prevented from exercising third-party license rights; the manifest integrity mechanism is open source and can be regenerated for a lawful replacement engine.

Official download: https://www.7-zip.org/download.html

Official license: https://www.7-zip.org/license.txt

Official source: https://github.com/ip7z/7zip/releases/download/26.03/7z2603-src.tar.xz

Full x64 MSI: https://github.com/ip7z/7zip/releases/download/26.03/7z2603-x64.msi

## Rust dependencies

`Cargo.toml` declares dependency constraints; the committed `Cargo.lock` pins the exact versions. The packaging script builds with `--locked`. It gathers LICENSE/LICENCE/COPYING/NOTICE/COPYRIGHT files from the exact Cargo metadata dependency graph into `third-party-rust`, with an index of package version, declared license and repository. This collection is an aid, not a substitute for reviewing platform-specific and transitive distribution obligations. An entry with zero collected license files requires attention before redistribution.

## Microsoft APIs

Windows operations use the `windows` and `windows-sys` bindings. Windows itself is not bundled.

IFileOperation flags: https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperation-setoperationflags

Administrative MSI extraction: https://learn.microsoft.com/en-us/windows/win32/msi/administrative-installation


## Inno Setup Chinese Simplified language file

installer/ChineseSimplified.isl is vendored from the official Inno Setup repository (jrsoftware/issrc, main branch, Files/Languages/ChineseSimplified.isl; messages for Inno Setup 6.5.0+). It is redistributed here so the installer builds reproducibly on machines whose Inno Setup installation does not ship this file. The file remains subject to the Inno Setup license: https://jrsoftware.org/files/is/license.txt
