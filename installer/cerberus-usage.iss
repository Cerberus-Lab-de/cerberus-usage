; Inno Setup script for Cerberus Usage.
; Build:  ISCC.exe /DAppVersion=1.0.0 installer\cerberus-usage.iss   (release.ps1 does this)
; Output: dist\CerberusUsage-Setup-<version>.exe
; Optional: /DExeDir=<folder with cerberus-usage.exe>, default ..\target\release.

#ifndef AppVersion
  #error AppVersion is not defined. Pass /DAppVersion=x.y.z
#endif
#ifndef ExeDir
  #define ExeDir "..\target\release"
#endif

#define AppName "Cerberus Usage"
#define AppExe "cerberus-usage.exe"

[Setup]
; Fixed identity of the app. Never change it, or updates would install side by side.
AppId={{3C5CBFDA-3EC7-452A-9E39-6ADA8ACBBFFD}
AppName={#AppName}
AppVersion={#AppVersion}
VersionInfoVersion={#AppVersion}
AppPublisher=Cerberus Lab
DefaultDirName={localappdata}\Programs\CerberusUsage
DefaultGroupName=Cerberus Usage
DisableProgramGroupPage=yes
; Per-user install, no admin rights. The user cannot switch to an all-users install.
PrivilegesRequired=lowest
OutputDir=..\dist
OutputBaseFilename=CerberusUsage-Setup-{#AppVersion}
UninstallDisplayIcon={app}\{#AppExe}
SetupIconFile=..\assets\icon.ico
WizardStyle=modern
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; The app hides to the tray on window close, so it is ended explicitly in [Code].
CloseApplications=force
RestartApplications=no

[Languages]
Name: "german"; MessagesFile: "compiler:Languages\German.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[CustomMessages]
german.TaskAutostart=Cerberus Usage mit Windows starten
german.RunApp=Cerberus Usage starten
german.AskRemoveData=Einstellungen, Zählerstand und gespeicherte Keys ebenfalls entfernen?
english.TaskAutostart=Start Cerberus Usage with Windows
english.RunApp=Start Cerberus Usage
english.AskRemoveData=Also remove settings, balance data and stored keys?

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"
Name: "autostart"; Description: "{cm:TaskAutostart}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; Only the app. The signing tool (sign-release.exe) is never shipped.
Source: "{#ExeDir}\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Cerberus Usage"; Filename: "{app}\{#AppExe}"
; Not on updates, so a desktop icon the user deleted does not come back.
Name: "{autodesktop}\Cerberus Usage"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon; Check: not IsUpdate

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "CerberusUsage"; ValueData: """{app}\{#AppExe}"""; Flags: uninsdeletevalue; Tasks: autostart

[Run]
; Interactive install: finish-page checkbox.
Filename: "{app}\{#AppExe}"; Description: "{cm:RunApp}"; Flags: nowait postinstall skipifsilent; Check: not IsUpdate
; Silent update started by the app itself (/UPDATE=1): start the new version.
Filename: "{app}\{#AppExe}"; Flags: nowait; Check: IsUpdate

[Code]
function IsUpdate: Boolean;
begin
  Result := ExpandConstant('{param:UPDATE|0}') = '1';
end;

function IsAppRunning: Boolean;
var
  ResultCode: Integer;
begin
  // find sets errorlevel 0 when the image name is listed.
  Result := Exec(ExpandConstant('{cmd}'),
    '/C tasklist /FI "IMAGENAME eq {#AppExe}" /NH | find /I "{#AppExe}" >nul',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode) and (ResultCode = 0);
end;

// Ends cerberus-usage.exe (this exact image name only): polite first, force after 2 s.
procedure CloseApp;
var
  ResultCode, I: Integer;
begin
  if not IsAppRunning then
    Exit;
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/im {#AppExe}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  for I := 1 to 10 do
  begin
    if not IsAppRunning then
      Exit;
    Sleep(500);
  end;
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/f /im {#AppExe}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Sleep(500);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  CloseApp;
  Result := '';
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  ResultCode: Integer;
begin
  if CurUninstallStep = usUninstall then
  begin
    CloseApp;
    // Silent uninstalls keep the data (default answer: no).
    if not UninstallSilent then
    begin
      if MsgBox(ExpandConstant('{cm:AskRemoveData}'), mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
        Exec(ExpandConstant('{app}\{#AppExe}'), '--uninstall-cleanup', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    end;
  end;
end;
