The official full 7-Zip engine is placed here by scripts/fetch-7zip.ps1.
This source delivery does not include downloaded executable or DLL files.

Expected generated layout:
  7z.exe        (Windows engine binary)
  7z.dll        (Windows engine library)
  7zz           (Linux engine binary; not fetched, not listed in manifest.json,
                 not embedded — build.rs verifies and embeds 7z.exe/7z.dll only;
                 manually placing 7zz here fails the build because manifest.json
                 has no entry for it)
  manifest.json
  NOTICE.txt
  licenses/
  7z2603-src.tar.xz

These engine binaries exist ONLY on the build machine. build.rs verifies each
file's SHA-256 against manifest.json, compresses them, and embeds them into the
EXE. The end-user release ZIP does NOT contain the engine executables or DLLs;
at runtime the app extracts the embedded copy into the user data directory
(with SHA-256 verification), or uses a user-supplied engine placed in
resources/7zip next to the EXE (LGPL replaceability).

Never substitute an unknown system PATH executable or the limited 7za binary.
