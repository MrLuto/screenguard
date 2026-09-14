#ifndef AppVersion
  #define AppVersion "0.10.9"
#endif
[Setup]
AppId={{F3D7CF47-4B51-45B5-B1E7-5110760B290F}
AppName=ScreenGuard
AppVersion={#AppVersion}
DefaultDirName={autopf}\ScreenGuard
DisableDirPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.22000
OutputBaseFilename=screenguard-windows-x86_64-setup
OutputDir=output
Compression=lzma2
SolidCompression=yes
Uninstallable=no
[Files]
Source: "package\*"; DestDir: "{tmp}\ScreenGuardPackage"; Flags: ignoreversion recursesubdirs
[Code]
var
  ConnectionPage: TInputOptionWizardPage;
  ServerPage: TInputQueryWizardPage;
  CloudPage: TInputQueryWizardPage;

procedure InitializeWizard;
begin
  ConnectionPage := CreateInputOptionPage(wpWelcome, 'Connect to ScreenGuard',
    'How should this computer find your server?',
    'An existing installation keeps its configuration.', True, False);
  ConnectionPage.Add('Discover the server on my local network (mDNS)');
  ConnectionPage.Add('Use a server address');
  ConnectionPage.Add('Use a cloud account (experimental)');
  ConnectionPage.SelectedValueIndex := 0;
  ServerPage := CreateInputQueryPage(ConnectionPage.ID, 'Server address',
    'Enter your ScreenGuard server address', '');
  ServerPage.Add('Server URL (for example https://server.example):', False);
  ServerPage.Add('Administration website URL (optional):', False);
  CloudPage := CreateInputQueryPage(ServerPage.ID, 'Cloud account',
    'Enter your ScreenGuard account email', 'Cloud mode is experimental.');
  CloudPage.Add('Email:', False);
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := ((PageID = ServerPage.ID) and (ConnectionPage.SelectedValueIndex <> 1)) or
    ((PageID = CloudPage.ID) and (ConnectionPage.SelectedValueIndex <> 2));
end;

function SafeArgument(Value: String): Boolean;
begin
  Result := (Pos('"', Value) = 0) and (Pos(#13, Value) = 0) and (Pos(#10, Value) = 0);
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if CurPageID = ServerPage.ID then
    Result := SafeArgument(ServerPage.Values[0]) and SafeArgument(ServerPage.Values[1]) and
      ((Pos('http://', ServerPage.Values[0]) = 1) or (Pos('https://', ServerPage.Values[0]) = 1) or
       (Pos('ws://', ServerPage.Values[0]) = 1) or (Pos('wss://', ServerPage.Values[0]) = 1));
  if CurPageID = CloudPage.ID then
    Result := SafeArgument(CloudPage.Values[0]) and (Pos('@', CloudPage.Values[0]) > 1);
  if not Result then MsgBox('Please enter a valid server URL or account email.', mbError, MB_OK);
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Arguments: String;
  ExitCode: Integer;
begin
  if CurStep = ssPostInstall then begin
    Arguments := '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' +
      ExpandConstant('{tmp}\ScreenGuardPackage\install.ps1') + '"';
    if ConnectionPage.SelectedValueIndex = 1 then
      Arguments := Arguments + ' -ServerUrl "' + ServerPage.Values[0] + '" -WebUiUrl "' + ServerPage.Values[1] + '"';
    if ConnectionPage.SelectedValueIndex = 2 then
      Arguments := Arguments + ' -CloudAccount "' + CloudPage.Values[0] + '"';
    if not Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'), Arguments,
      '', SW_SHOW, ewWaitUntilTerminated, ExitCode) then
      RaiseException('Could not start the ScreenGuard installer.');
    if ExitCode <> 0 then RaiseException('ScreenGuard service installation failed. Exit code: ' + IntToStr(ExitCode));
  end;
end;
