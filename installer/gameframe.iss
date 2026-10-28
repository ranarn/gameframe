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
; Quit wait when no command window was found (build older than the command
; channel, so the request cannot be acted on): just long enough for the startup
; race where the window appears slightly after the mutex.
#define NoWindowWaitMs "2000"
; How long to wait for the process to disappear after a successful force-kill.
#define KillWaitMs "5000"
; After the mutex is gone, how long to wait for gameframe.exe to become writable.
#define ExitWaitMs "5000"
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

[InstallDelete]
; The same stray folders, so an upgrade cleans them up too.
Type: filesandordirs; Name: "{userappdata}\GameFrame Settings"
Type: filesandordirs; Name: "{userappdata}\GameFrame zone editor"

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

// MSG for the message pump below (32-bit layout: setup is a 32-bit process).
type
  TMsg = record
    Wnd: Longint;
    Message: Longint;
    WParam: Longint;
    LParam: Longint;
    Time: Longint;
    PtX: Longint;
    PtY: Longint;
  end;

// Win32 calls for the quit request. Setup is a 32-bit process, so window
// handles fit a Longint.
function FindWindowEx(Parent, ChildAfter: Longint; ClassName, WindowName: String): Longint;
  external 'FindWindowExW@user32.dll stdcall';
function RegisterWindowMessage(Name: String): Longint;
  external 'RegisterWindowMessageW@user32.dll stdcall';
function PostMessage(Wnd, Msg, WParam, LParam: Longint): Longint;
  external 'PostMessageW@user32.dll stdcall';
function GetWindowThreadProcessId(Wnd: Longint; var ProcessId: Longint): Longint;
  external 'GetWindowThreadProcessId@user32.dll stdcall';
function OpenProcess(Access, InheritHandle, ProcessId: Longint): Longint;
  external 'OpenProcess@kernel32.dll stdcall';
function CloseHandle(Handle: Longint): Longint;
  external 'CloseHandle@kernel32.dll stdcall';
function PeekMessage(var Msg: TMsg; Wnd, MsgFilterMin, MsgFilterMax, RemoveMsg: Longint): Longint;
  external 'PeekMessageW@user32.dll stdcall';
procedure PostQuitMessage(ExitCode: Longint);
  external 'PostQuitMessage@user32.dll stdcall';
function TranslateMessage(var Msg: TMsg): Longint;
  external 'TranslateMessage@user32.dll stdcall';
function DispatchMessage(var Msg: TMsg): Longint;
  external 'DispatchMessageW@user32.dll stdcall';
function CreateFile(FileName: String; Access, ShareMode, Security, Disposition, Flags, Template: Longint): Longint;
  external 'CreateFileW@kernel32.dll stdcall';

function GameFrameRunning: Boolean;
begin
  Result := CheckForMutexes('{#MutexName}');
end;

// Dispatches everything waiting in this thread's queue (Pascal Script has no
// Application.ProcessMessages), so the wizard repaints and the window stays
// responsive. Wizard buttons stay clickable meanwhile (no IsDialogMessage, so
// no Tab/Enter/Esc handling); the wait is short, and a Cancel click is handled
// by Inno's own confirmation, re-entrantly but modally.
procedure PumpMessages;
var
  Msg: TMsg;
begin
  while PeekMessage(Msg, 0, 0, 0, 1 { PM_REMOVE }) <> 0 do
  begin
    if Msg.Message = $0012 { WM_QUIT } then
    begin
      // Not ours to swallow: hand it back for the outer message loop.
      PostQuitMessage(Msg.WParam);
      Exit;
    end;
    TranslateMessage(Msg);
    DispatchMessage(Msg);
  end;
end;

// Sleeps for the given time in short slices, pumping the message queue between
// them so the wizard keeps repainting and answering the user.
procedure PumpSleep(Ms: Integer);
var
  Waited: Integer;
begin
  Waited := 0;
  while Waited < Ms do
  begin
    PumpMessages;
    Sleep(50);
    Waited := Waited + 50;
  end;
  PumpMessages;
end;

// Posts GameFrame's restricted Quit command (see src/instance_channel.rs) to
// its message-only window. False when the window is not there (a build older
// than the command channel) or the post failed -- nothing will act on the request.
function RequestGameFrameQuit: Boolean;
var
  Wnd: Longint;
begin
  Result := False;
  Wnd := FindWindowEx(-3 { HWND_MESSAGE }, 0, '{#CommandClass}', '');
  if Wnd = 0 then
    Log('quit request: no GameFrame command window found')
  else if PostMessage(Wnd, RegisterWindowMessage('{#QuitMessage}'), {#QuitWParam}, 0) = 0 then
    Log('quit request: could not post the quit message')
  else
  begin
    Log('quit request: posted');
    Result := True;
  end;
end;

// True when the running GameFrame is one this (unelevated) setup cannot
// terminate: it runs at a higher integrity level (#69 elevated relaunch), so
// its quit message is blocked by UIPI and taskkill is refused. Probed by asking
// for PROCESS_TERMINATE on the command window's owner; only ERROR_ACCESS_DENIED
// counts (any other failure, e.g. the process just exited, is not elevation).
// False when the window is not found (older build) -- the taskkill outcome
// decides then.
function GameFrameRunsElevated: Boolean;
var
  Wnd, Pid, Process: Longint;
begin
  Result := False;
  Wnd := FindWindowEx(-3 { HWND_MESSAGE }, 0, '{#CommandClass}', '');
  if Wnd = 0 then
    Exit;
  GetWindowThreadProcessId(Wnd, Pid);
  if Pid = 0 then
    Exit;
  Process := OpenProcess(1 { PROCESS_TERMINATE }, 0, Pid);
  if Process = 0 then
    // Inno stores GetLastError after every external call.
    Result := DLLGetLastError = 5 { ERROR_ACCESS_DENIED }
  else
    CloseHandle(Process);
end;

// True while something still holds gameframe.exe open without write sharing,
// i.e. the process image is still mapped. The mutex disappears while the
// process is still shutting down, so it does not prove the file is replaceable.
// Only a sharing violation counts as locked; any other failure (missing file on
// a fresh install, access denied on a read-only or ACL-protected exe) is
// treated as unlocked on purpose -- waiting would not help, and the wait is
// only an optimisation before Inno's own copy retry.
function ExeLocked: Boolean;
var
  Handle: Longint;
begin
  Handle := CreateFile(ExpandConstant('{app}\{#AppExe}'), $40000000 { GENERIC_WRITE },
                       0 { no sharing }, 0, 3 { OPEN_EXISTING }, 0, 0);
  if Handle = -1 { INVALID_HANDLE_VALUE } then
    Result := DLLGetLastError = 32 { ERROR_SHARING_VIOLATION }
  else
  begin
    Result := False;
    CloseHandle(Handle);
  end;
end;

// Waits (capped) until gameframe.exe can be written, so the file copy / removal
// that follows does not hit the lock of a process that is still exiting.
procedure WaitForExeUnlocked;
var
  Waited: Integer;
  Locked: Boolean;
begin
  Waited := 0;
  Locked := ExeLocked;
  while Locked and (Waited < {#ExitWaitMs}) do
  begin
    PumpSleep(100);
    Waited := Waited + 100;
    Locked := ExeLocked;
  end;
  if Locked then
    Log('gameframe.exe is still locked; continuing (the file copy retries)')
  else
    Log(Format('gameframe.exe is writable after %d ms', [Waited]));
end;

// Tries to get GameFrame out of the way. False when it is still running and
// setup cannot stop it (typically because it is elevated).
function StopGameFrame: Boolean;
var
  ResultCode, Waited, QuitWait: Integer;
begin
  // A running gameframe.exe locks the file being replaced/removed. Ask it to
  // exit through its normal path (restores the taskbar and borderless
  // windows, removes the tray icon) and wait for its single-instance mutex to
  // go; force-kill only if it does not. Nothing to do when it is not running.
  Result := True;
  if not GameFrameRunning then
  begin
    // The mutex goes before the process has fully exited (e.g. a retry after
    // quitting from the tray), so the exe can still be locked.
    WaitForExeUnlocked;
    Exit;
  end;
  if GameFrameRunsElevated then
  begin
    Log('GameFrame runs elevated; setup cannot stop it');
    Result := False;
    Exit;
  end;
  if RequestGameFrameQuit then
    QuitWait := {#QuitWaitMs}
  else
    // Older build: nothing will act on the request, so only allow for the
    // startup race where the window appears just after the mutex.
    QuitWait := {#NoWindowWaitMs};
  Waited := 0;
  while GameFrameRunning and (Waited < QuitWait) do
  begin
    PumpSleep(100);
    Waited := Waited + 100;
  end;
  if GameFrameRunning then
  begin
    Log('GameFrame did not quit in time; force-killing it');
    Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#AppExe}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    Log(Format('taskkill exit code %d', [ResultCode]));
    if (ResultCode <> 0) and GameFrameRunning then
    begin
      // taskkill was refused (access denied, exit 1) or failed for another
      // reason (other session, policy); an elevated instance whose command
      // window could not be probed is the usual cause.
      Log('taskkill failed and GameFrame is still running');
      Result := False;
      Exit;
    end;
    // taskkill succeeded: a slow exit is not elevation, so keep waiting.
    Waited := 0;
    while GameFrameRunning and (Waited < {#KillWaitMs}) do
    begin
      PumpSleep(100);
      Waited := Waited + 100;
    end;
    if GameFrameRunning then
      Log('GameFrame is still running after a successful taskkill; continuing');
  end
  else
    Log('GameFrame quit cleanly');
  WaitForExeUnlocked;
end;

// StopGameFrame, with the "cannot stop it" case handed to the user: an
// interactive run offers Retry (after quitting GameFrame from the tray) or
// Cancel; a silent run just fails. True when GameFrame is gone.
function EnsureGameFrameStopped(Silent: Boolean): Boolean;
begin
  Result := StopGameFrame;
  while not Result do
  begin
    if Silent then
    begin
      Log('GameFrame is still running and cannot be stopped; failing (silent run)');
      Exit;
    end;
    if MsgBox('GameFrame is still running and setup cannot close it. It may be running as ' +
              'administrator or in another session.'#13#10#13#10 +
              'Quit GameFrame from its tray icon, then press Retry.',
              mbError, MB_RETRYCANCEL) <> IDRETRY then
    begin
      Log('User cancelled while GameFrame could not be stopped');
      Exit;
    end;
    Result := StopGameFrame;
  end;
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
  // Only per-user installs are supported (PrivilegesRequired=lowest, so setup
  // never installs for all users and never writes HKLM). HKLM's Uninstall key
  // is therefore deliberately not looked up: an admin-mode (HKLM) install
  // would count as fresh here.
  WasInstalled := RegKeyExists(HKEY_CURRENT_USER, '{#UninstallKey}');
  // Upgrade: remember what the app's toggle last set -- the real Run value.
  // Inno restores its own remembered task selection after this point, so the
  // selection itself is applied later (CurPageChanged / PrepareToInstall).
  // A fresh install keeps the task's default (checked).
  InitialStartup := WasInstalled and RegValueExists(HKEY_CURRENT_USER, '{#RunKey}', '{#AppName}');
end;

// The value of command-line switch Name ("/NAME=value", or "-NAME=value"), read
// from ParamStr rather than {param:}: {param:} returns '' both for a missing
// switch and for an empty "/NAME=", so it can't tell them apart. Found is True
// when the switch is present at all (including with an empty or no value).
function CmdLineSwitch(Name: String; var Found: Boolean): String;
var
  I, P: Integer;
  Arg: String;
begin
  Result := '';
  Found := False;
  for I := 1 to ParamCount do
  begin
    Arg := ParamStr(I);
    if (Length(Arg) > Length(Name) + 1) and ((Arg[1] = '/') or (Arg[1] = '-')) and
       (CompareText(Copy(Arg, 2, Length(Name)), Name) = 0) and
       ((Length(Arg) = Length(Name) + 1) or (Arg[Length(Name) + 2] = '=')) then
    begin
      Found := True;
      P := Length(Name) + 3;
      Result := Copy(Arg, P, Length(Arg));
    end;
  end;
  // Also catch the bare "/NAME" (length equals Length(Name) + 1).
  if not Found then
    for I := 1 to ParamCount do
    begin
      Arg := ParamStr(I);
      if (Length(Arg) = Length(Name) + 1) and ((Arg[1] = '/') or (Arg[1] = '-')) and
         (CompareText(Copy(Arg, 2, Length(Name)), Name) = 0) then
        Found := True;
    end;
end;

// Whether a comma-separated task list (as in /TASKS or /MERGETASKS) names the
// startup task, with or without a leading "!".
function TaskListNamesStartup(List: String): Boolean;
var
  Item: String;
  P: Integer;
begin
  Result := False;
  List := List + ',';
  repeat
    P := Pos(',', List);
    Item := Trim(Copy(List, 1, P - 1));
    Delete(List, 1, P);
    if (Length(Item) > 0) and (Item[1] = '!') then
      Delete(Item, 1, 1);
    if CompareText(Trim(Item), 'startup') = 0 then
      Result := True;
  until List = '';
end;

// Upgrade, first application, and the caller made no explicit task choice:
//  - /TASKS (any value, including an explicit empty "/TASKS=" meaning "no
//    tasks") is honoured as given;
//  - /MERGETASKS naming startup ("startup" or "!startup") wins over the
//    registry, so a silent upgrade can force the task either way.
// Only the HKCU Uninstall key is looked up (see InitializeWizard).
function ShouldApplyRegistryStartup: Boolean;
var
  TasksGiven, MergeGiven: Boolean;
  MergeValue: String;
begin
  CmdLineSwitch('TASKS', TasksGiven);
  MergeValue := CmdLineSwitch('MERGETASKS', MergeGiven);
  Result := WasInstalled and not StartupApplied and not TasksGiven and
            not (MergeGiven and TaskListNamesStartup(MergeValue));
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
  if EnsureGameFrameStopped(WizardSilent) then
    Result := ''
  else
    // Non-empty aborts before any file is touched (non-zero exit when silent).
    Result := 'GameFrame is still running and could not be closed, so it was not installed. ' +
              'Quit GameFrame from its tray icon and run setup again.';
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

// Uninstall: stop GameFrame before anything is removed; False cancels the
// uninstall untouched (non-zero exit).
function InitializeUninstall: Boolean;
begin
  Result := EnsureGameFrameStopped(UninstallSilent);
  if not Result then
    Log('Uninstall aborted: GameFrame still running');
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
  begin
    // Backstop: GameFrame may have been started after InitializeUninstall.
    // Source: jrsoftware/issrc Projects/Src/Setup.Uninstall.pas calls
    // CurUninstallStepChanged(usUninstall, HandleException=False) immediately
    // before UninstLog.PerformUninstall (the file removal); with False the
    // script's exception is re-raised ("fatal"), so Abort (EAbort) ends the
    // uninstall before any file is removed. It takes the same exit path as
    // InitializeUninstall returning False (that also calls Abort); the exact
    // exit code is not documented and not run here (it would need a throwaway
    // machine).
    if not StopGameFrame then
    begin
      Log('Uninstall aborted by the backstop: GameFrame is running and cannot be stopped');
      if not UninstallSilent then
        MsgBox('GameFrame was started again and setup cannot close it. It may be running as ' +
               'administrator or in another session.'#13#10#13#10 +
               'Quit GameFrame from its tray icon and run the uninstall again.', mbError, MB_OK);
      Abort;
    end;
    // Remove autostart however it was enabled -- by this installer or by
    // the app's own tray/Settings toggle.
    RegDeleteValue(HKEY_CURRENT_USER, '{#RunKey}', '{#AppName}');
    RegDeleteValue(HKEY_CURRENT_USER, '{#StartupApprovedKey}', '{#AppName}');
  end;
end;
