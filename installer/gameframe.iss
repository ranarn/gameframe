; GameFrame installer (Inno Setup 7). Build with installer\build.ps1, which
; compiles the release exe and passes AppVersion from Cargo.toml.
;
; Per-user install: no admin prompt, lands in %LOCALAPPDATA%\Programs\GameFrame,
; registers in Settings > Apps for uninstall.
;
; "Start with Windows" is the HKCU Run value named "GameFrame" -- the same
; value src/autostart.rs reads and writes, so the installer's checkbox and
; the app's own toggles (tray menu, Settings) always agree.

#ifndef AppVersion
  #error AppVersion must be passed on the command line (/DAppVersion=x.y.z) -- use installer\build.ps1
#endif

#define AppName "GameFrame"
#define AppExe "gameframe.exe"
#define RunKey "Software\Microsoft\Windows\CurrentVersion\Run"
#define StartupApprovedKey "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run"

[Setup]
; Never change AppId: it's how an upgrade finds the existing install.
AppId={{5C28E192-0AB5-4FA5-8874-88144D010E3B}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=Jonas Ranerfors
AppPublisherURL=https://github.com/ranarn/gameframe
AppSupportURL=https://github.com/ranarn/gameframe/issues
AppUpdatesURL=https://github.com/ranarn/gameframe/releases
AppCopyright=Copyright (C) 2026 Jonas Ranerfors
PrivilegesRequired=lowest
DefaultDirName={autopf}\{#AppName}
DisableDirPage=auto
DisableProgramGroupPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; GameFrame is stopped explicitly in [Code] below -- it has no visible
; window for Restart Manager to ask to close.
CloseApplications=no
SetupIconFile=..\assets\icon.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
OutputDir=..\target\installer
OutputBaseFilename=GameFrame-Setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern

[Tasks]
Name: "startup"; Description: "Start {#AppName} with Windows"; GroupDescription: "Startup:"
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
; The startup task's initial state mirrors the real Run value on an upgrade
; (see InitializeWizard), so these entries only need to reflect its final state.
; Checked: point the Run value at the installed exe (same quoted-path format
; as autostart::set_enabled).
Root: HKCU; Subkey: "{#RunKey}"; ValueType: string; ValueName: "{#AppName}"; ValueData: """{app}\{#AppExe}"""; Tasks: startup
; Clear a Task Manager "Disabled" override so a newly chosen autostart actually
; runs -- but not on an upgrade where the user left the task alone.
Root: HKCU; Subkey: "{#StartupApprovedKey}"; ValueName: "{#AppName}"; ValueType: none; Flags: deletevalue dontcreatekey; Tasks: startup; Check: StartupChoiceChanged
; Unchecked: remove a registration (a no-op when the value is already absent).
Root: HKCU; Subkey: "{#RunKey}"; ValueName: "{#AppName}"; ValueType: none; Flags: deletevalue dontcreatekey; Tasks: not startup

[Run]
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent

[Code]
var
  // Captured when the wizard opens, before this run touches the registry.
  WasInstalled: Boolean;
  // The startup task's state when the user first sees it.
  InitialStartup: Boolean;

procedure StopGameFrame;
var
  ResultCode: Integer;
begin
  // A running gameframe.exe locks the file being replaced/removed.
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#AppExe}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

procedure SelectStartupFromRegistry;
begin
  if InitialStartup then
    WizardSelectTasks('startup')
  else
    WizardSelectTasks('!startup');
end;

procedure InitializeWizard;
begin
  // AppId from [Setup] (the "{{" there is Inno's escape for a literal "{").
  WasInstalled := RegKeyExists(HKEY_CURRENT_USER,
    'Software\Microsoft\Windows\CurrentVersion\Uninstall\{5C28E192-0AB5-4FA5-8874-88144D010E3B}_is1');
  if WasInstalled then
  begin
    // Upgrade: the checkbox shows what the app's toggle last set -- the real
    // Run value -- not the default or Inno's remembered task state.
    InitialStartup := RegValueExists(HKEY_CURRENT_USER, '{#RunKey}', '{#AppName}');
    SelectStartupFromRegistry;
  end
  else
    // Fresh install keeps the task's default (checked).
    InitialStartup := WizardIsTaskSelected('startup');
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  // Inno restores the remembered task selection for a silent run after
  // InitializeWizard, so re-apply the registry state here unless the caller
  // chose tasks explicitly with /TASKS.
  if WasInstalled and WizardSilent and (ExpandConstant('{param:TASKS|}') = '') then
    SelectStartupFromRegistry;
  StopGameFrame;
  Result := '';
end;

function StartupChoiceChanged: Boolean;
begin
  Result := (not WasInstalled) or (WizardIsTaskSelected('startup') <> InitialStartup);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
  begin
    StopGameFrame;
    // Remove autostart however it was enabled -- by this installer or by
    // the app's own tray/Settings toggle.
    RegDeleteValue(HKEY_CURRENT_USER, '{#RunKey}', '{#AppName}');
    RegDeleteValue(HKEY_CURRENT_USER, '{#StartupApprovedKey}', '{#AppName}');
  end;
end;
