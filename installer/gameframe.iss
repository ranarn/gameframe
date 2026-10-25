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
; Never change AppGuid: it is the AppId, i.e. how an upgrade finds the existing install.
#define AppGuid "5C28E192-0AB5-4FA5-8874-88144D010E3B"
#define UninstallKey "Software\Microsoft\Windows\CurrentVersion\Uninstall\{" + AppGuid + "}_is1"
; Must match src/single_instance.rs (mutex) and src/instance_channel.rs (class,
; restricted message, Quit's wParam code).
#define MutexName "Local\GameFrame-" + AppGuid
#define CommandClass "GameFrame.CommandChannel-" + AppGuid
#define QuitMessage "GameFrame.RestrictedCommand-" + AppGuid
#define QuitWParam "2"
; How long to wait for GameFrame to exit after the quit request before killing it.
#define QuitWaitMs "10000"
#define StartupApprovedKey "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run"

[Setup]
; Never change AppId: it's how an upgrade finds the existing install.
AppId={{{#AppGuid}}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=Jonas Ranerfors
AppPublisherURL=https://github.com/ranarn/gameframe
AppSupportURL=https://github.com/ranarn/gameframe/issues
AppUpdatesURL=https://github.com/ranarn/gameframe/releases
; One setup at a time: a second setup started while the first still runs reports
; "Setup is already running" and exits (silently under /SUPPRESSMSGBOXES).
SetupMutex=GameFrame-Setup-{#AppGuid}
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
Source: "..\target\installer\GameFrame-Third-Party-Notices.txt"; DestDir: "{app}"; DestName: "THIRD-PARTY-NOTICES.txt"; Flags: ignoreversion

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

[UninstallDelete]
; Earlier versions let eframe persist window geometry under
; %APPDATA%\<window title>\data; remove those stray folders.
Type: filesandordirs; Name: "{userappdata}\GameFrame Settings"
Type: filesandordirs; Name: "{userappdata}\GameFrame zone editor"

[Run]
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent
; An update started by GameFrame itself passes /RELAUNCH (src/update_install.rs) and has no
; wizard to tick the box above, so it starts the app here. A plain silent install
; leaves the app stopped.
Filename: "{app}\{#AppExe}"; Flags: nowait; Check: RelaunchRequested

[Code]
var
  // Captured when the wizard opens, before this run touches the registry.
  WasInstalled: Boolean;
  // The real Run value at that moment: what the app's toggle last set. It is
  // also the baseline StartupChoiceChanged compares the final task against.
  InitialStartup: Boolean;
  // The Tasks page applies InitialStartup only the first time it is shown, so
  // the user's own tick/untick survives going Back and Next.
  StartupApplied: Boolean;

// Win32 calls for the quit request. Setup is a 32-bit process, so window
// handles fit a Longint.
function FindWindowEx(Parent, ChildAfter: Longint; ClassName, WindowName: String): Longint;
  external 'FindWindowExW@user32.dll stdcall';
function RegisterWindowMessage(Name: String): Longint;
  external 'RegisterWindowMessageW@user32.dll stdcall';
function PostMessage(Wnd, Msg, WParam, LParam: Longint): Longint;
  external 'PostMessageW@user32.dll stdcall';

function GameFrameRunning: Boolean;
begin
  Result := CheckForMutexes('{#MutexName}');
end;

// Posts GameFrame's restricted Quit command (see src/instance_channel.rs) to
// its message-only window. A no-op for a build without that window.
procedure RequestGameFrameQuit;
var
  Wnd: Longint;
begin
  Wnd := FindWindowEx(-3 { HWND_MESSAGE }, 0, '{#CommandClass}', '');
  if Wnd = 0 then
    Log('quit request: no GameFrame command window found')
  else if PostMessage(Wnd, RegisterWindowMessage('{#QuitMessage}'), {#QuitWParam}, 0) = 0 then
    Log('quit request: could not post the quit message')
  else
    Log('quit request: posted');
end;

procedure StopGameFrame;
var
  ResultCode, Waited: Integer;
begin
  // A running gameframe.exe locks the file being replaced/removed. Ask it to
  // exit through its normal path (restores the taskbar and borderless
  // windows, removes the tray icon) and wait for its single-instance mutex to
  // go; force-kill only if it does not. Nothing to do when it is not running.
  if not GameFrameRunning then
    Exit;
  RequestGameFrameQuit;
  Waited := 0;
  while GameFrameRunning and (Waited < {#QuitWaitMs}) do
  begin
    Sleep(100);
    Waited := Waited + 100;
  end;
  if GameFrameRunning then
  begin
    Log('GameFrame did not quit in time; force-killing it');
    Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#AppExe}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  end
  else
    Log('GameFrame quit cleanly');
end;

procedure SelectStartupFromRegistry;
begin
  if InitialStartup then
    WizardSelectTasks('startup')
  else
    WizardSelectTasks('!startup');
  StartupApplied := True;
end;

procedure InitializeWizard;
begin
  WasInstalled := RegKeyExists(HKEY_CURRENT_USER, '{#UninstallKey}');
  // Upgrade: remember what the app's toggle last set -- the real Run value.
  // Inno restores its own remembered task selection after this point, so the
  // selection itself is applied later (CurPageChanged / PrepareToInstall).
  // A fresh install keeps the task's default (checked).
  InitialStartup := WasInstalled and RegValueExists(HKEY_CURRENT_USER, '{#RunKey}', '{#AppName}');
end;

// Upgrade, first application, and the caller did not choose tasks with /TASKS.
function ShouldApplyRegistryStartup: Boolean;
begin
  Result := WasInstalled and not StartupApplied and (ExpandConstant('{param:TASKS|}') = '');
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  // Upgrade: override Inno's remembered selection with the real
  // Run value, once, when the Tasks page first appears (Inno raises this for
  // the page even in silent runs, after its restore).
  if (CurPageID = wpSelectTasks) and ShouldApplyRegistryStartup then
    SelectStartupFromRegistry;
  Log(Format('startup-task: page=%d installed=%d registry=%d selected=%d applied=%d', [
    CurPageID, Ord(WasInstalled), Ord(InitialStartup), Ord(WizardIsTaskSelected('startup')), Ord(StartupApplied)]));
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  // Safety net in case the Tasks page was never raised.
  if ShouldApplyRegistryStartup then
    SelectStartupFromRegistry;
  Log(Format('startup-task: PrepareToInstall installed=%d registry=%d selected=%d applied=%d', [
    Ord(WasInstalled), Ord(InitialStartup), Ord(WizardIsTaskSelected('startup')), Ord(StartupApplied)]));
  StopGameFrame;
  Result := '';
end;

// Whether setup was started with the bare /RELAUNCH switch.
function RelaunchRequested: Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), '/RELAUNCH') = 0 then
      Result := True;
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
