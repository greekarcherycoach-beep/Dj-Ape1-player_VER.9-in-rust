#define AppVersion GetEnv("APP_VERSION")
#if AppVersion == ""
  #define AppVersion "1.0.0"
#endif

[Setup]
AppId={{B7A3F0C2-5D1E-4C8A-9E21-6F4D2A8C1B90}
AppName=Dj Ape1
AppVersion={#AppVersion}
AppPublisher=Dj Ape1
DefaultDirName={autopf}\Dj Ape1
DefaultGroupName=Dj Ape1
DisableProgramGroupPage=yes
OutputDir=..\dist
OutputBaseFilename=DjApe1-Setup-{#AppVersion}
SetupIconFile=..\assets\icon.ico
UninstallDisplayIcon={app}\dj-ape1.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
MinVersion=10.0
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
CloseApplications=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\target\release\dj-ape1.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Dj Ape1"; Filename: "{app}\dj-ape1.exe"
Name: "{autodesktop}\Dj Ape1"; Filename: "{app}\dj-ape1.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\dj-ape1.exe"; Description: "{cm:LaunchProgram,Dj Ape1}"; Flags: nowait postinstall skipifsilent

[Code]
const
  WV2 = 'SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}';

function WebView2Installed: Boolean;
var v: String;
begin
  Result := RegQueryStringValue(HKLM32, WV2, 'pv', v) or
            RegQueryStringValue(HKLM64, WV2, 'pv', v) or
            RegQueryStringValue(HKCU, WV2, 'pv', v);
  if Result then Result := (v <> '') and (v <> '0.0.0.0');
end;

procedure CurStepChanged(CurStep: TSetupStep);
var rc: Integer; tmp: String;
begin
  if (CurStep = ssPostInstall) and not WebView2Installed then
  begin
    tmp := ExpandConstant('{tmp}\wv2setup.exe');
    if Exec('powershell.exe',
         '-NoProfile -ExecutionPolicy Bypass -Command "[Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12; Invoke-WebRequest -UseBasicParsing -Uri ''https://go.microsoft.com/fwlink/p/?LinkId=2124703'' -OutFile ''' + tmp + '''"',
         '', SW_HIDE, ewWaitUntilTerminated, rc) and (rc = 0) then
      Exec(tmp, '/silent /install', '', SW_HIDE, ewWaitUntilTerminated, rc)
    else
      MsgBox('Δεν ήταν δυνατή η λήψη του Microsoft WebView2 Runtime. Εγκατέστησέ το χειροκίνητα από microsoft.com/edge/webview2 αλλιώς η εφαρμογή δεν θα ανοίξει.', mbError, MB_OK);
  end;
end;
