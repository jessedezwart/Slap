; Per-user Windows installer, built by .github/workflows/release.yml:
;   iscc /DAppVersion=... /DNumericVersion=... /DBinDir=... /DBundleDir=... /DRepoDir=... [/DFull] slap.iss
; BinDir holds slap-windows-x86_64.exe, BundleDir holds yt-dlp.exe. With Full,
; BundleDir also holds deno.exe, the bgutil plugin zip and the bgutil server
; source, so YouTube PO tokens work without any setup.

#ifndef AppVersion
  #define AppVersion "0.0.0-dev"
#endif
#ifndef NumericVersion
  #define NumericVersion "0.0.0"
#endif
#ifndef BinDir
  #define BinDir "..\target\release"
#endif
#ifndef BundleDir
  #define BundleDir BinDir
#endif
#ifndef RepoDir
  #define RepoDir ".."
#endif

[Setup]
AppId={{6F3C2B1E-8A4D-4E7B-9C21-5D0A7F4B3E92}
AppName=Slap
AppVersion={#AppVersion}
VersionInfoVersion={#NumericVersion}
AppPublisher=Jesse de Zwart
AppPublisherURL=https://github.com/jessedezwart/Slap
; The bot reads .env, cache.redb and secrets/ from its working directory, so
; install per user into a writable folder (%LOCALAPPDATA%\Programs\Slap).
PrivilegesRequired=lowest
DefaultDirName={autopf}\Slap
DisableProgramGroupPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=force
RestartApplications=no
UninstallDisplayIcon={app}\slap.exe
OutputBaseFilename=slap-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern

[Tasks]
Name: "startup"; Description: "Run Slap when I sign in"; Flags: unchecked

[Files]
Source: "{#BinDir}\slap-windows-x86_64.exe"; DestDir: "{app}"; DestName: "slap.exe"; Flags: ignoreversion
; Next to slap.exe, so the default YTDLP_PATH finds it.
Source: "{#BundleDir}\yt-dlp.exe"; DestDir: "{app}"; Flags: ignoreversion
#ifdef Full
Source: "{#BundleDir}\deno.exe"; DestDir: "{app}"; Flags: ignoreversion
; yt-dlp loads plugins from yt-dlp-plugins next to its executable.
Source: "{#BundleDir}\bgutil-ytdlp-pot-provider.zip"; DestDir: "{app}\yt-dlp-plugins"; Flags: ignoreversion
Source: "{#BundleDir}\bgutil-server\*"; DestDir: "{app}\bgutil-server"; Flags: ignoreversion recursesubdirs createallsubdirs
#endif
; Never overwrite or remove the user's config.
Source: "{#RepoDir}\.env.example"; DestDir: "{app}"; DestName: ".env"; Flags: onlyifdoesntexist uninsneveruninstall

[Dirs]
Name: "{app}\secrets"

[Icons]
Name: "{autoprograms}\Slap"; Filename: "{app}\slap.exe"; WorkingDir: "{app}"
Name: "{autoprograms}\Slap configuration"; Filename: "notepad.exe"; Parameters: """{app}\.env"""
Name: "{userstartup}\Slap"; Filename: "{app}\slap.exe"; WorkingDir: "{app}"; Flags: runminimized; Tasks: startup

[Run]
#ifdef Full
; Deno lays node_modules out with junctions, which can't be packaged, so the
; provider's dependencies are fetched here instead of shipped. canvas's native
; build is optional for token generation, so no install scripts run.
Filename: "{app}\deno.exe"; Parameters: "install --frozen"; WorkingDir: "{app}\bgutil-server"; StatusMsg: "Installing the PO token provider..."; Flags: runhidden
#endif
Filename: "notepad.exe"; Parameters: """{app}\.env"""; Description: "Edit configuration (.env)"; Flags: postinstall nowait skipifsilent
Filename: "{app}\slap.exe"; WorkingDir: "{app}"; Description: "Launch Slap"; Flags: postinstall nowait skipifsilent unchecked

#ifdef Full
[UninstallDelete]
Type: files; Name: "{app}\yt-dlp.conf"
Type: filesandordirs; Name: "{app}\bgutil-server"

[Code]
// yt-dlp reads yt-dlp.conf next to its executable. Point it at the bundled
// Deno (also used for YouTube's JS challenges) and the bgutil script provider.
// Forward slashes because yt-dlp splits the config like a POSIX shell.
procedure CurStepChanged(CurStep: TSetupStep);
var
  App: String;
begin
  if CurStep = ssPostInstall then
  begin
    App := ExpandConstant('{app}');
    StringChangeEx(App, '\', '/', True);
    SaveStringToFile(ExpandConstant('{app}\yt-dlp.conf'),
      '--js-runtimes "deno:' + App + '/deno.exe"' + #13#10 +
      '--extractor-args "youtubepot-bgutilscript:server_home=' + App + '/bgutil-server"' + #13#10,
      False);
  end;
end;
#endif
