; simonaircast installer (Inno Setup 6). Build with tools\build-installer.ps1.
;
; Design: everything that needs administrator rights happens here, once —
; Program Files install and firewall rules (port rule for the speaker's timing
; packets + program rules so Windows never shows "allow access?"). The app
; itself runs as the normal user and never asks for elevation.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#define AppName "simonaircast"
#define TrayExe "simonaircast-tray.exe"
#define CliExe "simonaircast.exe"

[Setup]
AppId={{6F1D2A9E-3B7C-4E8A-9D21-5C4B7A8E1F03}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppName}
AppComments=Stream Windows audio to AirPlay speakers
DefaultDirName={autopf}\{#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
DisableReadyPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=..\dist
OutputBaseFilename=simonaircast-setup-{#AppVersion}
SetupIconFile=..\assets\simonaircast.ico
UninstallDisplayIcon={app}\{#TrayExe}
UninstallDisplayName={#AppName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; We stop the tray ourselves (it has no window for Restart Manager to close).
CloseApplications=no
SetupLogging=yes
VersionInfoVersion={#AppVersion}
VersionInfoProductName={#AppName}
VersionInfoDescription={#AppName} setup

[Tasks]
Name: autostart; Description: "Start {#AppName} when I sign in"

[Files]
Source: "..\target\release\{#TrayExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\{#CliExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#TrayExe}"; Comment: "Stream Windows audio to AirPlay speakers"

[Registry]
; Per-user autostart (the tray's "Start with Windows" item edits the same value).
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#AppName}"; ValueData: """{app}\{#TrayExe}"""; Tasks: autostart; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueName: "{#AppName}"; ValueType: none; Tasks: not autostart; Flags: deletevalue

[Run]
Filename: "{app}\{#CliExe}"; Parameters: "setup-firewall"; StatusMsg: "Configuring Windows Firewall..."; Flags: runhidden waituntilterminated
Filename: "{app}\{#TrayExe}"; Description: "Start {#AppName} now"; Flags: nowait postinstall skipifsilent runasoriginaluser

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/im {#TrayExe} /f"; Flags: runhidden waituntilterminated; RunOnceId: "StopTray"
Filename: "{app}\{#CliExe}"; Parameters: "remove-firewall"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveFirewall"

[Code]
procedure StopTray();
var
  ResultCode: Integer;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/im {#TrayExe} /f', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  // Upgrades: the running tray locks its exe.
  StopTray();
  // Development builds were installed per user; remove them so there's one copy.
  DelTree(ExpandConstant('{localappdata}\Programs\{#AppName}'), True, True, True);
  DeleteFile(ExpandConstant('{userprograms}\{#AppName}.lnk'));
  Result := '';
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if (CurUninstallStep = usPostUninstall) and not UninstallSilent then
  begin
    if MsgBox('Also remove your {#AppName} settings and logs?', mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
    begin
      DelTree(ExpandConstant('{userappdata}\{#AppName}'), True, True, True);
      DelTree(ExpandConstant('{localappdata}\{#AppName}'), True, True, True);
    end;
  end;
end;
