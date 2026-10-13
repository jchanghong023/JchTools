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
; O-33：仅更新当前用户的 PATH，并通知 Explorer 等新进程环境消费者。
ChangesEnvironment=yes
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

[Code]
function ExpandEnvironmentStrings(Source, Destination: String; Size: Cardinal): Cardinal;
  external 'ExpandEnvironmentStringsW@kernel32.dll stdcall';

var
  RegistrationError: String;

function PathOwnerKey: String;
begin
  { 使用现有 AppId 隔离本次安装拥有的 PATH 条目。 }
  Result := 'Software\JchTools\Installer\' + ExpandConstant('{#SetupSetting("AppId")}');
end;

function NormalizePathEntry(Entry: String): String;
var
  Size: Cardinal;
  Expanded: String;
begin
  Entry := Trim(Entry);
  if (Length(Entry) >= 2) and (Entry[1] = '"') and
     (Entry[Length(Entry)] = '"') then
    Entry := Copy(Entry, 2, Length(Entry) - 2);
  Size := ExpandEnvironmentStrings(Entry, '', 0);
  if Size > 0 then begin
    SetLength(Expanded, Size);
    if ExpandEnvironmentStrings(Entry, Expanded, Size) = Size then begin
      SetLength(Expanded, Size - 1);
      Entry := Expanded;
    end;
  end;
  StringChangeEx(Entry, '/', '\', True);
  while (Length(Entry) > 3) and (Entry[Length(Entry)] = '\') do
    Delete(Entry, Length(Entry), 1);
  Result := Lowercase(Entry);
end;

function FindPathEntry(Path, Entry: String; Normalize: Boolean;
  var EntryStart, EntryLength: Integer): Boolean;
var
  Start, Stop: Integer;
  Candidate: String;
begin
  Result := False;
  if Entry = '' then Exit;
  if Normalize then Entry := NormalizePathEntry(Entry);
  Start := 1;
  while Start <= Length(Path) do begin
    Stop := Start;
    while (Stop <= Length(Path)) and (Path[Stop] <> ';') do Stop := Stop + 1;
    Candidate := Copy(Path, Start, Stop - Start);
    if Normalize then Candidate := NormalizePathEntry(Candidate);
    if Candidate = Entry then begin
      EntryStart := Start;
      EntryLength := Stop - Start;
      Result := True;
      Exit;
    end;
    Start := Stop + 1;
  end;
end;

function RemoveOwnedEntry(Path, OwnedEntry: String): String;
var
  Start, Count: Integer;
begin
  Result := Path;
  { 仅删除一个精确记录的自有条目，不删除其他规范化后相同的条目。 }
  if FindPathEntry(Path, OwnedEntry, False, Start, Count) then begin
    if Start + Count <= Length(Path) then Count := Count + 1
    else if Start > 1 then begin
      Start := Start - 1;
      Count := Count + 1;
    end;
    Delete(Result, Start, Count);
  end;
end;

function RegisterCliPath: String;
var
  PreviousPathExisted, PreviousOwnerExisted, OwnerWritten: Boolean;
  PreviousPath, PreviousOwner, RegisteredPath, RegisteredOwner, AppPath: String;
  Start, Count: Integer;
begin
  Result := '';
  PreviousPathExisted := RegValueExists(HKCU, 'Environment', 'Path');
  PreviousOwnerExisted := RegValueExists(HKCU, PathOwnerKey, 'PathEntry');
  PreviousPath := '';
  PreviousOwner := '';
  if PreviousPathExisted and
     not RegQueryStringValue(HKCU, 'Environment', 'Path', PreviousPath) then begin
    Result := '无法读取 HKCU\Environment\Path，命令行 PATH 注册失败。';
    Exit;
  end;
  if PreviousOwnerExisted and
     not RegQueryStringValue(HKCU, PathOwnerKey, 'PathEntry', PreviousOwner) then begin
    Result := '无法读取 JchTools 命令行 PATH 条目归属，注册失败。';
    Exit;
  end;
  AppPath := ExpandConstant('{app}');
  RegisteredPath := PreviousPath;
  RegisteredOwner := '';
  if FindPathEntry(PreviousPath, PreviousOwner, False, Start, Count) then begin
    if NormalizePathEntry(PreviousOwner) = NormalizePathEntry(AppPath) then
      RegisteredOwner := PreviousOwner
    else
      RegisteredPath := RemoveOwnedEntry(RegisteredPath, PreviousOwner);
  end;
  if not FindPathEntry(RegisteredPath, AppPath, True, Start, Count) then begin
    if RegisteredPath <> '' then RegisteredPath := RegisteredPath + ';';
    RegisteredPath := RegisteredPath + AppPath;
    RegisteredOwner := AppPath;
  end;
  { REG_EXPAND_SZ 保留原有 %变量%；不修改机器 PATH，也不使用 setx。 }
  if not RegWriteExpandStringValue(HKCU, 'Environment', 'Path', RegisteredPath) then begin
    Result := '无法写入 HKCU\Environment\Path，命令行 PATH 注册失败。';
    Exit;
  end;
  if RegisteredOwner <> '' then
    OwnerWritten := RegWriteStringValue(HKCU, PathOwnerKey, 'PathEntry', RegisteredOwner)
  else if PreviousOwnerExisted then
    OwnerWritten := RegDeleteValue(HKCU, PathOwnerKey, 'PathEntry')
  else
    OwnerWritten := True;
  if not OwnerWritten then begin
    Result := '无法保存 JchTools 命令行 PATH 条目归属，注册失败。';
    { 归属写入失败时，只撤销紧邻的 PATH 写入，避免留下无人管理的条目。 }
    if PreviousPathExisted then
      OwnerWritten := RegWriteExpandStringValue(HKCU, 'Environment', 'Path', PreviousPath)
    else
      OwnerWritten := RegDeleteValue(HKCU, 'Environment', 'Path');
    if not OwnerWritten then Result := Result + ' 无法恢复原有用户 PATH。';
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then begin
    RegistrationError := RegisterCliPath;
    if RegistrationError <> '' then
      SuppressibleMsgBox(RegistrationError, mbError, MB_OK, IDOK);
  end;
end;

function GetCustomSetupExitCode: Integer;
begin
  if RegistrationError <> '' then Result := 1
  else Result := 0;
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if (CurPageID = wpFinished) and (RegistrationError <> '') then begin
    WizardForm.FinishedHeadingLabel.Caption := 'JchTools 命令行 PATH 注册失败';
    WizardForm.FinishedLabel.Caption := RegistrationError;
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Path, OwnedEntry, NewPath: String;
begin
  if CurUninstallStep <> usUninstall then Exit;
  if not RegValueExists(HKCU, PathOwnerKey, 'PathEntry') then Exit;
  if not RegQueryStringValue(HKCU, PathOwnerKey, 'PathEntry', OwnedEntry) then
    RaiseException('无法读取 JchTools 命令行 PATH 条目归属。');
  if RegValueExists(HKCU, 'Environment', 'Path') then begin
    if not RegQueryStringValue(HKCU, 'Environment', 'Path', Path) then
      RaiseException('卸载命令行工具时无法读取 HKCU\Environment\Path。');
    NewPath := RemoveOwnedEntry(Path, OwnedEntry);
    if (NewPath <> Path) and
       not RegWriteExpandStringValue(HKCU, 'Environment', 'Path', NewPath) then
      RaiseException('无法移除 JchTools 自有的命令行 PATH 条目。');
  end;
  if not RegDeleteValue(HKCU, PathOwnerKey, 'PathEntry') then
    RaiseException('无法移除 JchTools 命令行 PATH 条目归属记录。');
  RegDeleteKeyIfEmpty(HKCU, PathOwnerKey);
end;
