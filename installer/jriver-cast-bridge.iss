; Inno Setup script for jriver-cast-bridge. Build with installer\build.ps1,
; which compiles the release binaries and passes AppVersion.
;
; Installs for the current user only (no admin rights):
;   program   %LOCALAPPDATA%\Programs\jriver-cast-bridge
;   config    %APPDATA%\jriver-cast-bridge\config.toml (created from the
;             example if missing; kept on uninstall, like the logs and the
;             Jellyfin login in Windows Credential Manager)

#define AppName "JRiver Cast Bridge"
#define AppExe "jriver-cast-bridge.exe"
#define CliExe "bridge-cli.exe"
#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef BinDir
  #define BinDir "..\target\release"
#endif

[Setup]
AppId={{FBFC9B81-F8DD-40E1-9453-9F3960022233}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=jriver-cast-bridge
DefaultDirName={localappdata}\Programs\jriver-cast-bridge
DisableDirPage=auto
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\target\installer
OutputBaseFilename=jriver-cast-bridge-setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}

[Tasks]
Name: "autostart"; Description: "Start the bridge when I log in"; GroupDescription: "Startup:"

[Files]
Source: "{#BinDir}\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\{#CliExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\crates\jriver-cast-bridge\config.example.toml"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
; The user's config: only created, never overwritten or removed.
Source: "..\crates\jriver-cast-bridge\config.example.toml"; DestDir: "{userappdata}\jriver-cast-bridge"; DestName: "config.toml"; Flags: onlyifdoesntexist uninsneveruninstall

[Icons]
Name: "{userprograms}\{#AppName}\{#AppName}"; Filename: "{app}\{#AppExe}"; WorkingDir: "{app}"
Name: "{userprograms}\{#AppName}\Settings"; Filename: "{app}\{#AppExe}"; Parameters: "--settings"; WorkingDir: "{app}"
Name: "{userprograms}\{#AppName}\Uninstall {#AppName}"; Filename: "{uninstallexe}"
Name: "{userstartup}\{#AppName}"; Filename: "{app}\{#AppExe}"; WorkingDir: "{app}"; Tasks: autostart

[InstallDelete]
; Replaced by the Settings window (0.5.0).
Type: files; Name: "{userprograms}\{#AppName}\Edit config.lnk"

[Run]
Filename: "{app}\{#AppExe}"; WorkingDir: "{app}"; Description: "Start the bridge now (it asks for the Jellyfin login the first time)"; Flags: postinstall nowait skipifsilent

[UninstallRun]
; Ask the bridge to quit (it stops a playing film and reports it), then make
; sure it's gone.
Filename: "{app}\{#AppExe}"; Parameters: "--quit"; Flags: runhidden waituntilterminated; RunOnceId: "QuitBridge"
Filename: "{sys}\taskkill.exe"; Parameters: "/f /im {#AppExe}"; Flags: runhidden; RunOnceId: "StopBridge"

[Code]
{ A running bridge locks its exe; stop it before files are replaced. The new
  exe asks it to quit over its local pipe (stopping a playing film and
  reporting it to Jellyfin) and waits up to 25 s. taskkill then stops a
  bridge that didn't quit in time, or an older one without the pipe; that
  can't report the stop, and the next start reports it instead. }
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  ExtractTemporaryFile('{#AppExe}');
  Exec(ExpandConstant('{tmp}\{#AppExe}'), '--quit', '', SW_HIDE,
    ewWaitUntilTerminated, ResultCode);
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/f /im {#AppExe}', '', SW_HIDE,
    ewWaitUntilTerminated, ResultCode);
  Sleep(500);
  Result := '';
end;
