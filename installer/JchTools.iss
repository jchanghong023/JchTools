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
#if !FileExists(SourceDir + "\jchocr.exe")
#error "SourceDir 必须包含本次 Cargo 构建的 jchocr.exe"
#endif
; P3-29：真实安装/卸载回归专用预处理隔离；普通构建完全保留生产目标。
#ifdef ReviewRemediationRoot
#ifndef ReviewRemediationAppId
#error "隔离安装回归必须同时提供随机 ReviewRemediationAppId"
#endif
#endif

[Setup]
#ifdef ReviewRemediationRoot
AppId={#ReviewRemediationAppId}
#else
AppId={{6F4A2B9C-8D3E-4C71-9A5B-0E2D7C8F1A34}
#endif
AppName=JchTools
AppVersion={#Version}
AppPublisher=JchTools
AppPublisherURL=https://example.invalid/jchtools
#ifdef ReviewRemediationRoot
DefaultDirName={#ReviewRemediationRoot}\installed
#else
DefaultDirName={localappdata}\Programs\JchTools
#endif
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

; 简体中文语言文件 vendor 自 Inno Setup 官方仓库（jrsoftware/issrc main 分支
; Files/Languages/ChineseSimplified.isl，适用于 Inno Setup 6.5.0+；来源与
; 许可见 THIRD_PARTY_NOTICES.md）。不同安装来源的 Inno 不一定自带该文件，
; 随仓库分发并按脚本相对路径引用：文件缺失即编译失败（fail-closed）。
[Languages]
Name: "chinesesimplified"; MessagesFile: "ChineseSimplified.isl"

[Tasks]
; P-05：桌面快捷方式默认创建（用户可取消勾选）。
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"
Name: "quicklaunchicon"; Description: "添加到快速启动栏"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; 升级控制器仅从本次 setup 解压到临时目录，不安装第二份 EXE。
; 独特名称防止 ExtractTemporaryFile 匹配到普通安装载荷中的 JchTools.exe。
Source: "{#SourceDir}\JchTools.exe"; DestDir: "{tmp}"; DestName: "JchTools-maintenance.exe"; Flags: dontcopy
; 整个便携目录原样安装：EXE（内嵌 7-Zip 引擎）、resources 许可证材料、第三方清单等。
Source: "{#SourceDir}\*"; DestDir: "{app}"; Excludes: "jchocr.exe"; Flags: ignoreversion recursesubdirs createallsubdirs
; O-33：复制到 Windows 已有的用户 PATH 目录，不修改环境变量，无需重启终端。
#ifdef ReviewRemediationRoot
Source: "{#SourceDir}\jchocr.exe"; DestDir: "{#ReviewRemediationRoot}\WindowsApps"; Flags: ignoreversion
#else
Source: "{#SourceDir}\jchocr.exe"; DestDir: "{localappdata}\Microsoft\WindowsApps"; Flags: ignoreversion
#endif

[Icons]
Name: "{group}\JchTools"; Filename: "{app}\JchTools.exe"
Name: "{group}\卸载 JchTools"; Filename: "{uninstallexe}"
Name: "{autodesktop}\JchTools"; Filename: "{app}\JchTools.exe"; Tasks: desktopicon
Name: "{userappdata}\Microsoft\Internet Explorer\Quick Launch\JchTools"; Filename: "{app}\JchTools.exe"; Tasks: quicklaunchicon

[Run]
Filename: "{app}\JchTools.exe"; Description: "立即运行 JchTools"; Flags: nowait postinstall skipifsilent

[InstallDelete]
; 升级时移除旧版安装目录里的命令副本，只保留已有 PATH 目录中的新入口。
Type: files; Name: "{app}\jchocr.exe"

[UninstallDelete]
; 仅清理程序自身生成的空壳；用户数据（%LOCALAPPDATA%\JchTools）不动。
Type: filesandordirs; Name: "{app}\resources\7zip"


[Code]
// CA-08 / P3-29：替换与删除前真实调用内部维护入口；自然退出并检查退出码。
// 不使用任务名匹配/强杀，不更改普通 GUI 关闭后的 CA-05 常驻行为。
// 控制器只改变镜像来源；工作目录与继承的应用状态目录配置保持不变。
function StopCodeAgentForMaintenance(const Executable: String): Boolean;
var
  ResultCode: Integer;
begin
  Result := Exec(Executable, '--codeagent-service --stop', ExpandConstant('{app}'),
    SW_HIDE, ewWaitUntilTerminated, ResultCode);
  if Result then
    Result := ResultCode = 0;
  if not Result then
    Log('CodeAgent maintenance did not complete; refusing to replace or delete program files.');
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  if not FileExists(ExpandConstant('{app}\JchTools.exe')) then
    Exit;
  // 旧版可能忽略 --stop，绝不能运行它探测能力；只运行本次 setup 携带的新镜像。
  try
    ExtractTemporaryFile('JchTools-maintenance.exe');
    if StopCodeAgentForMaintenance(ExpandConstant('{tmp}\JchTools-maintenance.exe')) then
      Exit;
  except
    Log('Cannot prepare CodeAgent maintenance controller: ' + GetExceptionMessage);
  end;
  Result := '无法等待 CodeAgent 后台自然退出，已停止安装。请稍后重试。';
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  // 确认卸载后、实际删除前才停止后台；取消确认不影响凭据维护。
  if CurUninstallStep <> usUninstall then
    Exit;
  // 卸载器没有 setup 的临时载荷；本版本已安装的主程序已实现 stop 协议。
  if not FileExists(ExpandConstant('{app}\JchTools.exe')) then
    Exit;
  if not StopCodeAgentForMaintenance(ExpandConstant('{app}\JchTools.exe')) then
    RaiseException('无法等待 CodeAgent 后台自然退出，已停止卸载。请稍后重试。');
end;
