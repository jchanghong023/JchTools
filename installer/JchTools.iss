; JchTools 安装脚本（Inno Setup 6，CONTRACT P-05/E-04）
; 交付形态：安装到当前用户目录（免管理员/UAC）+ 桌面快捷方式（默认创建）
; + 开始菜单入口 + 控制面板卸载项；不要求代码签名、不公开分发。
; 用法：ISCC.exe /DSourceDir=<发布目录> /DOutputDir=<输出目录> installer\JchTools.iss
; SourceDir 指向 package-windows.ps1 组装好的便携目录（含 EXE 与许可证材料）。

#ifndef SourceDir
#error "需要 /DSourceDir=<发布目录>（package-windows.ps1 组装的便携目录）"
#endif
#ifndef OutputDir
#error "需要 /DOutputDir=<输出目录>"
#endif
#ifndef Version
#define Version "0.1.0"
#endif

[Setup]
AppId={{6F4A2B9C-8D3E-4C71-9A5B-0E2D7C8F1A34}
AppName=JchTools
AppVersion={#Version}
AppPublisher=JchTools
AppPublisherURL=https://example.invalid/jchtools
DefaultDirName={localappdata}\Programs\JchTools
DefaultGroupName=JchTools
; P-05：个人使用，安装到当前用户目录，全程不弹 UAC。
PrivilegesRequired=lowest
DisableProgramGroupPage=yes
OutputDir={#OutputDir}
OutputBaseFilename=JchTools-Setup-x64
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesInstallIn64BitMode=x64compatible
; 卸载时一并清掉用户数据目录（任务库）由用户自行决定：默认保留，仅移除程序文件。
UninstallDisplayIcon={app}\JchTools.exe

; 简体中文是 Inno Setup 的非官方翻译：官方安装包不带，随安装来源分别位于
; Languages\ 或 Languages\Unofficial\。编译期探测两者取其一，都缺失即编译失败（
; fail-closed），避免静默产出英文安装包。
#if FileExists(AddBackslash(CompilerPath) + "Languages\ChineseSimplified.isl")
#define ChineseMessagesFile "compiler:Languages\ChineseSimplified.isl"
#elif FileExists(AddBackslash(CompilerPath) + "Languages\Unofficial\ChineseSimplified.isl")
#define ChineseMessagesFile "compiler:Languages\Unofficial\ChineseSimplified.isl"
#else
#error ChineseSimplified.isl not found under Inno Setup Languages
#endif

[Languages]
Name: "chinesesimplified"; MessagesFile: "{#ChineseMessagesFile}"

[Tasks]
; P-05：桌面快捷方式默认创建（用户可取消勾选）。
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"
Name: "quicklaunchicon"; Description: "添加到快速启动栏"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; 整个便携目录原样安装：EXE（内嵌 7-Zip 引擎）、resources 许可证材料、第三方清单等。
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\JchTools"; Filename: "{app}\JchTools.exe"
Name: "{group}\卸载 JchTools"; Filename: "{uninstallexe}"
Name: "{autodesktop}\JchTools"; Filename: "{app}\JchTools.exe"; Tasks: desktopicon
Name: "{userappdata}\Microsoft\Internet Explorer\Quick Launch\JchTools"; Filename: "{app}\JchTools.exe"; Tasks: quicklaunchicon

[Run]
Filename: "{app}\JchTools.exe"; Description: "立即运行 JchTools"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; 仅清理程序自身生成的空壳；用户数据（%LOCALAPPDATA%\JchTools）不动。
Type: filesandordirs; Name: "{app}\resources\7zip"
