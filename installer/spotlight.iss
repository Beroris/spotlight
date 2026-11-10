; Spotlight's installer: a normal Windows setup wizard, for the current user, with no admin rights.
;
; Build it with scripts/package-windows.ps1, which passes the version and the built exe:
;   ISCC.exe /DAppVersion=1.0.0 /DSourceExe=..\target\release\Spotlight.exe installer\spotlight.iss

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\target\release\Spotlight.exe"
#endif
; Tests build Setup with another AppId and name, so they can't touch a real install.
#ifndef AppGuid
  #define AppGuid "A87CA1A7-A57E-4C0B-8B55-1346E73C3AAB"
#endif
#ifndef AppName
  #define AppName "Spotlight"
#endif
#define AppExe "Spotlight.exe"

[Setup]
; Never change this: Windows knows the installed app by it, for upgrades and uninstalling.
AppId={{{#AppGuid}}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=Beroris
AppPublisherURL=https://github.com/Beroris/spotlight
AppSupportURL=https://github.com/Beroris/spotlight/issues
AppComments=A small, fast Spotify player.
VersionInfoVersion={#AppVersion}
VersionInfoDescription={#AppName} Setup
; Per user: no admin prompt.
PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\{#AppName}
DisableDirPage=yes
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableWelcomePage=no
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
SetupIconFile=..\assets\spotlight.ico
WizardStyle=modern
WizardSizePercent=100
WizardSmallImageFile=wizard-small.png
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
Compression=lzma2/max
SolidCompression=yes
OutputDir=..\dist
OutputBaseFilename=Spotlight-{#AppVersion}-Setup
; Spotlight is asked to quit by the code below. If one too old to understand that is running,
; Windows closes it instead.
CloseApplications=force
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "autostart"; Description: "Start {#AppName} when I sign in"; GroupDescription: "Options:"
Name: "desktopicon"; Description: "Put a shortcut on the desktop"; GroupDescription: "Options:"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "{#AppExe}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"; Comment: "Play Spotify"
; An update leaves the desktop as the user has it.
Name: "{userdesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Comment: "Play Spotify"; Tasks: desktopicon; Check: not IsUpdate

[Registry]
; The same entry Spotlight writes itself, which lists it under Startup apps in Task Manager.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#AppName}"; ValueData: """{app}\{#AppExe}"""; Flags: uninsdeletevalue
; Task Manager's switch for it is set in the code below, from the sign-in option.

[Run]
Filename: "{app}\{#AppExe}"; Description: "Start {#AppName} now"; Flags: postinstall nowait skipifsilent
; Spotlight started this update and quit for it, so it comes back when it's done.
Filename: "{app}\{#AppExe}"; Flags: nowait; Check: IsUpdate

[UninstallDelete]
Type: dirifempty; Name: "{app}"

[Code]
const
  RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';
  ApprovedKey = 'Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run';

var
  SignInOptionSet: Boolean;

{ Spotlight runs Setup with /UPDATE when it updates itself: Setup then changes nothing but the
  app, and starts it again at the end. }
function IsUpdate(): Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), '/UPDATE') = 0 then
      Result := True;
end;

{ Whether the command line chose the options, as /TASKS= or /MERGETASKS= do. }
function OptionsGiven(): Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if (CompareText(Copy(ParamStr(I), 1, 7), '/TASKS=') = 0) or (CompareText(Copy(ParamStr(I), 1, 12), '/MERGETASKS=') = 0) then
      Result := True;
end;

{ On a reinstall, the sign-in option starts as Task Manager's switch is now: the user may have
  changed it there since. An odd first byte means off. Options given on the command line win.
  This runs in silent installs too. }
procedure CurPageChanged(CurPageID: Integer);
var
  Switch: AnsiString;
begin
  if (CurPageID = wpSelectTasks) and not SignInOptionSet and not OptionsGiven() then
  begin
    SignInOptionSet := True;
    if RegQueryBinaryValue(HKCU, ApprovedKey, '{#AppName}', Switch) and (Length(Switch) > 0) then
    begin
      if (Ord(Switch[1]) and 1) = 0 then
        WizardSelectTasks('autostart')
      else
        WizardSelectTasks('!autostart');
    end;
  end;
end;

{ Asks a running Spotlight to quit, so its exe can be replaced or removed. It waits until
  Spotlight has gone. Only when one is running: a copy from before --quit would start up. }
procedure QuitSpotlight();
var
  Code: Integer;
  Exe: String;
begin
  Exe := ExpandConstant('{app}\{#AppExe}');
  if FileExists(Exe) and (FindWindowByWindowName('{#AppName}') <> 0) then
  begin
    Exec(Exe, '--quit', '', SW_HIDE, ewWaitUntilTerminated, Code);
    Sleep(300);
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  QuitSpotlight();
  Result := '';
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Switch: AnsiString;
begin
  { Task Manager's switch follows the sign-in option. An update leaves it as the user set it. }
  if (CurStep = ssPostInstall) and not IsUpdate() then
  begin
    if WizardIsTaskSelected('autostart') then
      Switch := #2#0#0#0#0#0#0#0#0#0#0#0
    else
      Switch := #3#0#0#0#0#0#0#0#0#0#0#0;
    RegWriteBinaryValue(HKCU, ApprovedKey, '{#AppName}', Switch);
  end;
end;

function SettingsFolder(): String;
begin
  Result := ExpandConstant('{localappdata}\{#AppName}');
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
  begin
    QuitSpotlight();
    RegDeleteValue(HKCU, RunKey, '{#AppName}');
    RegDeleteValue(HKCU, ApprovedKey, '{#AppName}');
  end;
  if (CurUninstallStep = usPostUninstall) and DirExists(SettingsFolder()) and not UninstallSilent() then
  begin
    if MsgBox('Also remove your Spotlight settings, sign-in and saved covers?' #13#10#13#10 'Keep them if you might install Spotlight again.', mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
      DelTree(SettingsFolder(), True, True, True);
  end;
end;
