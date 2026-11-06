; Inno Setup script for Lector (https://jrsoftware.org/isinfo.php).
; Built by scripts/package-windows.ps1:
;   ISCC.exe /DAppVersion=0.1.0 /DArch=x64 /DSourceDir=<staging> /DOutputDir=<out> lector.iss

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef Arch
  #define Arch "x64"
#endif

[Setup]
AppId={{7C1E4B7E-2B7A-4C5B-9C0E-6A1F3D2B9E41}
AppName=Lector
AppVersion={#AppVersion}
AppVerName=Lector {#AppVersion}
AppPublisher=Lector contributors
AppPublisherURL=https://github.com/meetdhruva/lector
AppSupportURL=https://github.com/meetdhruva/lector/issues
AppUpdatesURL=https://github.com/meetdhruva/lector/releases
DefaultDirName={autopf}\Lector
DefaultGroupName=Lector
DisableProgramGroupPage=yes
LicenseFile={#SourceDir}\LICENSE-MIT
OutputDir={#OutputDir}
OutputBaseFilename=Lector-{#AppVersion}-windows-{#Arch}-setup
SetupIconFile={#SourceDir}\lector.ico
UninstallDisplayIcon={app}\lector.exe
UninstallDisplayName=Lector {#AppVersion}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ChangesEnvironment=yes
ChangesAssociations=yes
; Per-user install by default; the user may choose an all-users install.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
#if Arch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
#endif
MinVersion=10.0.17763

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "addtopath"; Description: "Add Lector to PATH (use ""lector"" in terminals)"; GroupDescription: "Integration:"
Name: "contextmenu"; Description: "Add ""Open with Lector"" to the Explorer context menu"; GroupDescription: "Integration:"

[Files]
Source: "{#SourceDir}\lector.exe"; DestDir: "{app}"; Flags: ignoreversion
; Console build of the command line: `lector` in a terminal resolves lector.com before lector.exe.
Source: "{#SourceDir}\lector-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\lector.com"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\*.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\LICENSE-*"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\third-party\*"; DestDir: "{app}\third-party"; Flags: ignoreversion recursesubdirs

[Icons]
Name: "{autoprograms}\Lector"; Filename: "{app}\lector.exe"; Comment: "Reading-first code viewer"
Name: "{autodesktop}\Lector"; Filename: "{app}\lector.exe"; Tasks: desktopicon

[Registry]
; "Open with Lector" for folders, folder backgrounds and files.
Root: HKA; Subkey: "Software\Classes\Directory\shell\Lector"; ValueType: string; ValueData: "Open with Lector"; Tasks: contextmenu; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Directory\shell\Lector"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\lector.exe"; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\shell\Lector\command"; ValueType: string; ValueData: """{app}\lector.exe"" ""%1"""; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\Lector"; ValueType: string; ValueData: "Open with Lector"; Tasks: contextmenu; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\Lector"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\lector.exe"; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\Lector\command"; ValueType: string; ValueData: """{app}\lector.exe"" ""%V"""; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\*\shell\Lector"; ValueType: string; ValueData: "Open with Lector"; Tasks: contextmenu; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\*\shell\Lector"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\lector.exe"; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\*\shell\Lector\command"; ValueType: string; ValueData: """{app}\lector.exe"" ""%1"""; Tasks: contextmenu
; Register for "Open with" lists.
Root: HKA; Subkey: "Software\Classes\Applications\lector.exe"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "Lector"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Applications\lector.exe\shell\open\command"; ValueType: string; ValueData: """{app}\lector.exe"" ""%1"""

[Run]
Filename: "{app}\lector.exe"; Description: "{cm:LaunchProgram,Lector}"; Flags: nowait postinstall skipifsilent

[Code]
const
  UserEnvKey = 'Environment';
  SystemEnvKey = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment';

function EnvRoot(): Integer;
begin
  if IsAdminInstallMode then Result := HKEY_LOCAL_MACHINE else Result := HKEY_CURRENT_USER;
end;

function EnvKey(): String;
begin
  if IsAdminInstallMode then Result := SystemEnvKey else Result := UserEnvKey;
end;

procedure AddToPath(Dir: String);
var
  Paths: String;
begin
  if not RegQueryStringValue(EnvRoot(), EnvKey(), 'Path', Paths) then Paths := '';
  if Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';') > 0 then exit;
  if (Paths <> '') and (Copy(Paths, Length(Paths), 1) <> ';') then Paths := Paths + ';';
  RegWriteExpandStringValue(EnvRoot(), EnvKey(), 'Path', Paths + Dir);
end;

procedure RemoveFromPath(Dir: String);
var
  Paths: String;
  P: Integer;
begin
  if not RegQueryStringValue(EnvRoot(), EnvKey(), 'Path', Paths) then exit;
  P := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';');
  if P = 0 then exit;
  if P = 1 then
    Delete(Paths, 1, Length(Dir))
  else
    Delete(Paths, P - 1, Length(Dir) + 1);
  if Copy(Paths, 1, 1) = ';' then Delete(Paths, 1, 1);
  RegWriteExpandStringValue(EnvRoot(), EnvKey(), 'Path', Paths);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then
    AddToPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RemoveFromPath(ExpandConstant('{app}'));
end;
