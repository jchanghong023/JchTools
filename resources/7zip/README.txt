The official full 7-Zip engine is placed here by scripts/fetch-7zip.ps1.
This source delivery does not include downloaded executable or DLL files.

Expected generated layout:
  7z.exe
  7z.dll
  manifest.json
  NOTICE.txt
  licenses/
  7z2603-src.tar.xz

The end-user portable application receives this directory automatically when
scripts/package-windows.ps1 builds and packages the application.
Never substitute an unknown system PATH executable or the limited 7za binary.
