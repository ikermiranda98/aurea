; Aurea for Windows — per-user installer (Inno Setup 6). Internal names
; (zeron.exe, %LOCALAPPDATA%\Zeron, the AppId) stay Zeron's; what a person
; sees says Aurea.
;
; Built by scripts/package-windows.ps1, which passes the version, the package
; architecture, and the staged portable directory:
;   ISCC.exe /DAppVersion=0.2.97 /DArch=x86_64 /DPackageDir=<stage> /DOutputDir=<out> zeron.iss
;
; Installs into %LOCALAPPDATA%\Programs\Aurea without elevation, like VS
; Code's user setup: the directory stays writable by its user, so the in-app
; updater (crates/update/src/windows.rs) can replace zeron.exe in place. The
; staged directory already carries zeron-update.json, which marks the install
; as update-managed. Re-running a newer installer upgrades in place; user data
; lives in %LOCALAPPDATA%\Zeron and is never touched here.

#ifndef AppVersion
  #error AppVersion must be defined (/DAppVersion=x.y.z)
#endif
#ifndef Arch
  #error Arch must be defined (/DArch=x86_64 or /DArch=aarch64)
#endif
#ifndef PackageDir
  #error PackageDir must be defined (/DPackageDir=<staged package directory>)
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif
; The zgx app (scripts/package-windows.ps1 -ZgxVersion, /DZgx=1) brings its
; version's folder, beta\<version>, stable or beta. The file version must be
; numeric; a beta's AppVersion (x.y.z-beta.n) isn't.
#ifndef FileVersion
  #define FileVersion AppVersion
#endif

#if Arch == "aarch64"
  #define ArchAllowed "arm64"
#else
  #define ArchAllowed "x64compatible"
#endif

[Setup]
; Never change AppId: it identifies the installation across upgrades, and
; crates/update/src/windows.rs refreshes DisplayVersion under this key after
; in-app updates.
AppId={{AD5DEC34-E254-467B-8F24-8127EBAF4DA6}
AppName=Aurea
AppVersion={#AppVersion}
AppVerName=Aurea {#AppVersion}
AppPublisher=Aurea
AppPublisherURL=https://github.com/ikermiranda98/aurea
AppSupportURL=https://github.com/ikermiranda98/aurea/issues
AppUpdatesURL=https://github.com/ikermiranda98/aurea/releases
VersionInfoVersion={#FileVersion}
PrivilegesRequired=lowest
DefaultDirName={autopf}\Aurea
DisableProgramGroupPage=yes
DisableDirPage=auto
DisableReadyPage=yes
ArchitecturesAllowed={#ArchAllowed}
ArchitecturesInstallIn64BitMode={#ArchAllowed}
MinVersion=10.0
OutputDir={#OutputDir}
OutputBaseFilename=aurea-{#AppVersion}-windows-{#Arch}-setup
SetupIconFile=zeron.ico
UninstallDisplayIcon={app}\zeron.exe
UninstallDisplayName=Aurea
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
; A running Aurea is closed through the Restart Manager before its files are
; replaced; the updated app starts again from the finish page. `force` matters:
; Aurea runs its engine as a child `zeron.exe engine` that has no window of its
; own, and the Restart Manager cannot ask that one to close — with a plain
; `yes` Setup gives up ("unable to automatically close all applications") and
; the install never happens.
CloseApplications=force
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#PackageDir}\zeron.exe"; DestDir: "{app}"; Flags: ignoreversion
; The Visual C++ runtime zeron.exe imports (app-local, see package-windows.ps1).
Source: "{#PackageDir}\msvcp140.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PackageDir}\msvcp140_1.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PackageDir}\vcruntime140.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PackageDir}\vcruntime140_1.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PackageDir}\zeron-update.json"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PackageDir}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PackageDir}\THIRD_PARTY_NOTICES.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PackageDir}\licenses\*"; DestDir: "{app}\licenses"; Flags: ignoreversion recursesubdirs createallsubdirs
#ifdef Zgx
; The zgx app's scripts, plugins and engine, by version.
Source: "{#PackageDir}\beta\*"; DestDir: "{app}\beta"; Flags: ignoreversion recursesubdirs createallsubdirs
#endif

[Icons]
Name: "{autoprograms}\Aurea"; Filename: "{app}\zeron.exe"
Name: "{autodesktop}\Aurea"; Filename: "{app}\zeron.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\zeron.exe"; Description: "{cm:LaunchProgram,Aurea}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Leftovers of in-app updates (crates/update/src/windows.rs).
Type: files; Name: "{app}\zeron.exe.old"
Type: files; Name: "{app}\.zeron-update-incoming.exe"
Type: filesandordirs; Name: "{app}\.zeron-update-*"
Type: files; Name: "{app}\.zeron-completing.lock"
Type: files; Name: "{app}\zeron-version"
; The zgx app's folder, whichever versions an update brought.
Type: filesandordirs; Name: "{app}\beta"
