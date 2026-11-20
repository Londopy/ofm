; ofm - Inno Setup script
;
; Built by the release workflow with
;   ISCC.exe /DAppVersion=1.0.0 installer\ofm.iss
; Run `cargo build --release` first so ..\target\release\ofm.exe exists.
;
; Output: installer\output\ofm-<version>-setup-x64.exe
;
; ofm is a console program, so the shortcuts open a terminal running it, and
; the install folder goes on PATH so client setup notes can just say "run ofm".

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

; Folder holding the built exe, relative to this script. CI passes
; /DBinDir=..\target\x86_64-pc-windows-msvc\release when it builds with --target.
#ifndef BinDir
  #define BinDir "..\target\release"
#endif

; VersionInfoVersion must be plain numeric (x.x.x[.x]); strip any prerelease
; suffix like "-rc1" so prerelease tags still compile.
#if Pos("-", AppVersion) > 0
  #define NumericVersion Copy(AppVersion, 1, Pos("-", AppVersion) - 1)
#else
  #define NumericVersion AppVersion
#endif

#define AppName      "ofm"
#define AppPublisher "Londopy"
#define AppURL       "https://github.com/Londopy/ofm"
#define AppExeName   "ofm.exe"

[Setup]
; Unique GUID - do NOT reuse this for a different app.
; The doubled opening brace is Inno's escape for a literal "{".
AppId={{E79B41E7-2EDD-4EC0-AB4A-3ADA8F884EEA}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}/issues
AppUpdatesURL={#AppURL}/releases

; Per-user install, no UAC prompt. The dialog still lets the user pick an
; all-users install if they want one.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog

; With lowest privileges {autopf} resolves to %LOCALAPPDATA%\Programs.
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
AllowNoIcons=yes

; ofm is MIT OR Apache-2.0. The release workflow writes license.rtf with both
; texts; a local build without it shows the MIT text.
#if FileExists(AddBackslash(SourcePath) + "license.rtf")
LicenseFile=license.rtf
#else
LicenseFile=..\LICENSE-MIT
#endif

OutputDir=output
OutputBaseFilename=ofm-{#AppVersion}-setup-x64
SetupIconFile=..\assets\ofm.ico

; Compression
Compression=lzma2/ultra64
SolidCompression=yes

; Windows 10+ only (64-bit)
MinVersion=10.0
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

WizardStyle=modern

; Restart Manager asks a running ofm to close before its exe is replaced.
; Unlike the tray apps there is no taskkill: killing the modem in the middle
; of a transmission can leave the radio keyed (a CM108 GPIO stays as it was).
CloseApplications=yes
CloseApplicationsFilter=*.exe
RestartApplications=no

; tell Explorer the PATH changed, so new terminals see it without a sign-out
ChangesEnvironment=yes

UninstallDisplayIcon={app}\{#AppExeName}
UninstallDisplayName={#AppName} {#AppVersion}
VersionInfoVersion={#NumericVersion}
VersionInfoDescription={#AppName} {#AppVersion} setup
VersionInfoProductName={#AppName}
VersionInfoCompany={#AppPublisher}

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; \
  Description: "{cm:CreateDesktopIcon}"; \
  GroupDescription: "{cm:AdditionalIcons}"
; checked by default, unlike the other Londopy apps' PATH task: Pat and other
; clients' setup notes tell people to run "ofm" from a terminal
Name: "addtopath"; \
  Description: "Add {#AppName} to the PATH"; \
  GroupDescription: "Environment:"

[Files]
; Main binary - built before running iscc
Source: "{#BinDir}\{#AppExeName}"; \
  DestDir: "{app}"; \
  Flags: ignoreversion

[Icons]
; cmd /k runs ofm and keeps the window open after it exits, so a startup
; error stays readable instead of closing with the window. The doubled quotes
; survive cmd's quote stripping when the path has spaces or parentheses.
Name: "{group}\{#AppName}"; \
  Filename: "{cmd}"; \
  Parameters: "/k """"{app}\{#AppExeName}"""""; \
  WorkingDir: "{app}"; \
  IconFilename: "{app}\{#AppExeName}"
Name: "{group}\{cm:UninstallProgram,{#AppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; \
  Filename: "{cmd}"; \
  Parameters: "/k """"{app}\{#AppExeName}"""""; \
  WorkingDir: "{app}"; \
  IconFilename: "{app}\{#AppExeName}"; \
  Tasks: desktopicon

[Run]
; Offer to start ofm in a terminal after install (user can uncheck)
Filename: "{cmd}"; \
  Parameters: "/k """"{app}\{#AppExeName}"""""; \
  WorkingDir: "{app}"; \
  Description: "{cm:LaunchProgram,{#StringChange(AppName, '&', '&&')}}"; \
  Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Clean up the config left in AppData
Type: filesandordirs; Name: "{userappdata}\{#AppName}"

[Code]
// PATH handling (from nexium): append or remove {app} in the user's PATH, or
// the machine's for an all-users install.
function EnvKey(): string;
begin
  if IsAdminInstallMode then
    Result := 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment'
  else
    Result := 'Environment';
end;

function EnvRoot(): Integer;
begin
  if IsAdminInstallMode then
    Result := HKEY_LOCAL_MACHINE
  else
    Result := HKEY_CURRENT_USER;
end;

procedure EnvAddPath(Dir: string);
var
  Paths: string;
begin
  if not RegQueryStringValue(EnvRoot(), EnvKey(), 'Path', Paths) then
    Paths := '';
  if Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';') > 0 then
    exit;
  if (Paths <> '') and (Paths[Length(Paths)] <> ';') then
    Paths := Paths + ';';
  Paths := Paths + Dir;
  RegWriteExpandStringValue(EnvRoot(), EnvKey(), 'Path', Paths);
end;

// Paths without the entry Dir (any case). Padding with ';' lets the first
// and last entries match like the rest.
function WithoutEntry(Paths, Dir: string): string;
var
  P: Integer;
begin
  Result := ';' + Paths + ';';
  P := Pos(';' + Uppercase(Dir) + ';', Uppercase(Result));
  if P = 0 then
  begin
    Result := Paths;
    exit;
  end;
  Delete(Result, P, Length(Dir) + 1);
  Result := Copy(Result, 2, Length(Result) - 2);
end;

procedure EnvRemovePath(Dir: string);
var
  Paths, Trimmed: string;
begin
  if not RegQueryStringValue(EnvRoot(), EnvKey(), 'Path', Paths) then
    exit;
  Trimmed := WithoutEntry(Paths, Dir);
  if Trimmed <> Paths then
    RegWriteExpandStringValue(EnvRoot(), EnvKey(), 'Path', Trimmed);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    if WizardIsTaskSelected('addtopath') then
      EnvAddPath(ExpandConstant('{app}'))
    else
      // unticked on an upgrade: take out what the earlier install added
      EnvRemovePath(ExpandConstant('{app}'));
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    EnvRemovePath(ExpandConstant('{app}'));
end;
