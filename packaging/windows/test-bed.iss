; Test Bed for Windows: installs the bundle the Windows workflow builds.
; Per user, no administrator rights: the share service runs as the signed-in
; user anyway (an HKCU Run entry starts it at sign-in).
;
;   ISCC /DAppVersion=2.1.0 /DSourceDir=dist\TestBed /DOutputDir=. test-bed.iss

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\..\dist\TestBed"
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif

[Setup]
AppId={{6C1F6E0B-3B7E-4E52-9D0A-7F4C2B9E5A11}
AppName=Test Bed
AppVersion={#AppVersion}
AppPublisher=Pocket Studios LLC
AppPublisherURL=https://github.com/PocketStudiosLLC/Test-Bed
DefaultDirName={autopf}\Test Bed
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir={#OutputDir}
OutputBaseFilename=TestBed-Setup-{#AppVersion}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; The share service keeps its files open; close it to replace them.
CloseApplications=force
UninstallDisplayIcon={app}\bin\test-bed.exe
UninstallDisplayName=Test Bed

[Tasks]
Name: desktopicon; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: recursesubdirs createallsubdirs ignoreversion

[InstallDelete]
; Files from an older version that this one no longer ships.
Type: filesandordirs; Name: "{app}\bin"
Type: filesandordirs; Name: "{app}\lib"
Type: filesandordirs; Name: "{app}\share"

[Icons]
Name: "{autoprograms}\Test Bed"; Filename: "{app}\bin\test-bed.exe"; WorkingDir: "{app}\bin"
Name: "{autodesktop}\Test Bed"; Filename: "{app}\bin\test-bed.exe"; WorkingDir: "{app}\bin"; Tasks: desktopicon

[Run]
; Sharing was on before the upgrade: start it again from the new files.
Filename: "{app}\bin\test-bed-share.exe"; Parameters: "run"; Flags: nowait runhidden; Check: ShareWasOn
Filename: "{app}\bin\test-bed.exe"; Description: "Open Test Bed"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM test-bed-share.exe"; Flags: runhidden; RunOnceId: "StopShare"
Filename: "{sys}\reg.exe"; Parameters: "delete HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v ""Test Bed Share"" /f"; Flags: runhidden; RunOnceId: "RemoveSignIn"

[Code]
const
  RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';
var
  WasSharing: Boolean;

function ShareWasOn: Boolean;
begin
  Result := WasSharing;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  WasSharing := RegValueExists(HKEY_CURRENT_USER, RunKey, 'Test Bed Share');
  // Stop sharing so its files can be replaced.
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM test-bed-share.exe', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Result := '';
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  // Point the sign-in entry at this install (it may name an older unzipped copy).
  if (CurStep = ssPostInstall) and WasSharing then
    RegWriteStringValue(HKEY_CURRENT_USER, RunKey, 'Test Bed Share',
      '"' + ExpandConstant('{app}\bin\test-bed-share.exe') + '" run');
end;
