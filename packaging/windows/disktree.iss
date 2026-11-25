; The Windows installer, compiled by Inno Setup 7 from the repo root:
;
;   ISCC.exe /DAppVersion=0.11.0 /DArch=x86_64 packaging\windows\disktree.iss
;
; after `cargo build --release`. It writes
; target\bundle\disktree-<version>-<arch>-windows-setup.exe.

#ifndef AppVersion
  #error Pass the version: /DAppVersion=<version>
#endif

; One installer per release build, named like the zip. The setup program
; itself is x64 either way; on Arm64 Windows 11 it runs emulated and
; installs the native arm64 binary.
#ifndef Arch
  #error Pass the architecture: /DArch=x86_64 or /DArch=aarch64
#elif Arch == "x86_64"
  #define Archs "x64compatible"
#elif Arch == "aarch64"
  #define Archs "arm64"
#else
  #error Arch must be x86_64 or aarch64
#endif

[Setup]
; Upgrades and the uninstaller find an existing install by this id, so it
; must never change; a new one would install disktree a second time.
AppId={{354830A5-B031-45DE-8595-4513BA422E5F}
AppName=disktree
AppVersion={#AppVersion}
AppPublisherURL=https://github.com/tobi/disktree
AppSupportURL=https://github.com/tobi/disktree/issues
AppUpdatesURL=https://github.com/tobi/disktree/releases
; Paths below are relative to the repo root, not to this script.
SourceDir=..\..
OutputDir=target\bundle
OutputBaseFilename=disktree-{#AppVersion}-{#Arch}-windows-setup
SetupIconFile=assets\disktree.ico
UninstallDisplayIcon={app}\disktree.exe
SetupArchitecture=x64
ArchitecturesAllowed={#Archs}
ArchitecturesInstallIn64BitMode={#Archs}
; For the current user by default, with no UAC prompt, as `make install`
; puts it under ~/.local; the first page offers all users instead.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
DefaultDirName={autopf}\disktree
DisableProgramGroupPage=yes
MinVersion=10.0
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes

[Tasks]
Name: desktopicon; Description: "{cm:CreateDesktopIcon}"

[Files]
Source: "target\release\disktree.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "README.md"; DestDir: "{app}"
Source: "LICENSE"; DestDir: "{app}"

[Icons]
Name: "{autoprograms}\disktree"; Filename: "{app}\disktree.exe"
Name: "{autodesktop}\disktree"; Filename: "{app}\disktree.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\disktree.exe"; Description: "{cm:LaunchProgram,disktree}"; Flags: postinstall nowait skipifsilent
