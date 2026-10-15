; Inno Setup script for the Bytescape installer.
;
; Why Inno Setup and not WiX: one short readable script, the silent switches
; winget, Chocolatey and the Microsoft Store use (/VERYSILENT /SUPPRESSMSGBOXES
; /NORESTART) built in, winget's `inno` installer type, and x64 and ARM64
; installers from the same file. WiX would mean writing an MSI and carrying a
; .NET toolchain for what is a two-file install.
;
; Build (see packaging/windows/build-windows.sh, which sets the variables):
;   BYTESCAPE_VERSION=0.1.0 BYTESCAPE_ARCH=x64 BYTESCAPE_STAGE=<folder> iscc bytescape.iss
; BYTESCAPE_ARCH is x64 or arm64. BYTESCAPE_STAGE holds bytescape.exe,
; bytescape-helper.exe and the two licence files. BYTESCAPE_OUT is the output
; folder (default: the current folder).

#define Version GetEnv("BYTESCAPE_VERSION")
#define Arch GetEnv("BYTESCAPE_ARCH")
#define Stage GetEnv("BYTESCAPE_STAGE")
#define OutDir GetEnv("BYTESCAPE_OUT")
#if Version == "" || Arch == "" || Stage == ""
  #error Set BYTESCAPE_VERSION, BYTESCAPE_ARCH and BYTESCAPE_STAGE.
#endif
#if OutDir == ""
  #define OutDir "."
#endif

[Setup]
AppId={{F6BB911E-9C99-4C17-B21F-9693793EBDE3}
AppName=Bytescape
AppVersion={#Version}
AppPublisher=Recursive AI, LLC
AppPublisherURL=https://github.com/recursive-ai-oss/bytescape
AppSupportURL=https://github.com/recursive-ai-oss/bytescape/issues
AppUpdatesURL=https://github.com/recursive-ai-oss/bytescape/releases
VersionInfoVersion={#Version}
; Per machine, into Program Files. Only administrators can write there, so a
; standard user cannot replace the helper that runs with administrator rights.
; The install page is not shown, so nobody picks a folder they can write to by
; accident. A person who starts the installer with /DIR= chooses that folder
; on purpose, as an administrator.
PrivilegesRequired=admin
DefaultDirName={autopf}\Bytescape
DisableProgramGroupPage=yes
DisableDirPage=yes
OutputDir={#OutDir}
OutputBaseFilename=bytescape-{#Version}-windows-{#Arch}-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
MinVersion=10.0
UninstallDisplayIcon={app}\bytescape.exe
UninstallDisplayName=Bytescape
CloseApplications=yes
RestartApplications=no
LicenseFile={#Stage}\LICENSE-MIT
#if Arch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
#endif

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "{#Stage}\bytescape.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\bytescape-helper.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\LICENSE-MIT"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\LICENSE-APACHE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Bytescape"; Filename: "{app}\bytescape.exe"

[Registry]
; "Analyze with Bytescape" in Explorer's right-click menu (on Windows 11, under
; "Show more options"), for a folder, for the empty space inside a folder, and
; for a drive. Each key is removed again when Bytescape is uninstalled.
;
; The path goes to the program as its only argument. A drive's path ends in a
; backslash, and a backslash just before the closing quote would swallow the
; quote, so the drive and background verbs add `\.` after the path.
Root: HKLM; Subkey: "Software\Classes\Directory\shell\AnalyzeWithBytescape"; ValueType: string; ValueName: ""; ValueData: "Analyze with Bytescape"; Flags: uninsdeletekey
Root: HKLM; Subkey: "Software\Classes\Directory\shell\AnalyzeWithBytescape"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\bytescape.exe"""
Root: HKLM; Subkey: "Software\Classes\Directory\shell\AnalyzeWithBytescape\command"; ValueType: string; ValueName: ""; ValueData: """{app}\bytescape.exe"" ""%1"""
Root: HKLM; Subkey: "Software\Classes\Directory\Background\shell\AnalyzeWithBytescape"; ValueType: string; ValueName: ""; ValueData: "Analyze with Bytescape"; Flags: uninsdeletekey
Root: HKLM; Subkey: "Software\Classes\Directory\Background\shell\AnalyzeWithBytescape"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\bytescape.exe"""
Root: HKLM; Subkey: "Software\Classes\Directory\Background\shell\AnalyzeWithBytescape\command"; ValueType: string; ValueName: ""; ValueData: """{app}\bytescape.exe"" ""%V\."""
Root: HKLM; Subkey: "Software\Classes\Drive\shell\AnalyzeWithBytescape"; ValueType: string; ValueName: ""; ValueData: "Analyze with Bytescape"; Flags: uninsdeletekey
Root: HKLM; Subkey: "Software\Classes\Drive\shell\AnalyzeWithBytescape"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\bytescape.exe"""
Root: HKLM; Subkey: "Software\Classes\Drive\shell\AnalyzeWithBytescape\command"; ValueType: string; ValueName: ""; ValueData: """{app}\bytescape.exe"" ""%1\."""

; There is no [Run] section on purpose. A "start Bytescape" entry at the end of
; setup would be started by an installer that is running with administrator
; rights, and Inno cannot always hand it back to the user's own account (for
; example when setup itself was started from an elevated prompt). The
; interface must never run elevated, so setup does not start it.
