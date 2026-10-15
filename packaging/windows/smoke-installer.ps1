# Installs the Bytescape installer silently, checks the result, and removes it
# again. Run it in PowerShell 7 on a Windows machine that may be changed (a CI
# runner, or a virtual machine), in an elevated session:
#
#   pwsh packaging/windows/smoke-installer.ps1 -Installer dist\bytescape-<version>-windows-x64-setup.exe -Version <version>
#
# It checks that both programs land under Program Files, that a standard user
# cannot write to the folder or replace the helper, that the interface starts
# and reports its version, that the right-click entries exist, and that the
# uninstaller removes the files and the registry entries.

param(
  [Parameter(Mandatory = $true)][string]$Installer,
  [Parameter(Mandatory = $true)][string]$Version
)

$ErrorActionPreference = 'Stop'
$app = Join-Path $env:ProgramFiles 'Bytescape'
$verbs = @(
  'HKLM:\Software\Classes\Directory\shell\AnalyzeWithBytescape',
  'HKLM:\Software\Classes\Directory\Background\shell\AnalyzeWithBytescape',
  'HKLM:\Software\Classes\Drive\shell\AnalyzeWithBytescape'
)

function Stop-Smoke([string]$Message) {
  Write-Host "::error title=Installer check failed::$Message"
  exit 1
}

function Run-Silently([string]$Path, [string[]]$Arguments) {
  $process = Start-Process -FilePath $Path -ArgumentList $Arguments -Wait -PassThru
  return $process.ExitCode
}

# A standard user must not be able to create or replace files in the folder, or
# change the helper itself, or the helper that runs with administrator rights
# could be swapped before it is started.
function Test-UsersCannotWrite([string]$Path) {
  $writers = @('BUILTIN\Users', 'NT AUTHORITY\Authenticated Users', 'Everyone')
  $bad = [System.Security.AccessControl.FileSystemRights]'WriteData,AppendData,Delete,DeleteSubdirectoriesAndFiles,WriteAttributes,ChangePermissions,TakeOwnership'
  foreach ($rule in (Get-Acl -Path $Path).Access) {
    if ($rule.AccessControlType -eq 'Allow' -and $writers -contains $rule.IdentityReference.Value -and ($rule.FileSystemRights -band $bad)) {
      Stop-Smoke "$Path can be written by $($rule.IdentityReference.Value): $($rule.FileSystemRights)"
    }
  }
}

if (Test-Path $app) { Stop-Smoke "$app exists before the install; run this on a clean machine" }

$code = Run-Silently $Installer @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/LOG=install.log')
if ($code -ne 0) {
  if (Test-Path install.log) { Get-Content install.log -Tail 40 }
  Stop-Smoke "the installer exited with $code"
}
foreach ($name in 'bytescape.exe', 'bytescape-helper.exe', 'unins000.exe', 'LICENSE-MIT', 'LICENSE-APACHE') {
  if (-not (Test-Path (Join-Path $app $name))) { Stop-Smoke "$name is missing from $app" }
}
Test-UsersCannotWrite $app
Test-UsersCannotWrite (Join-Path $app 'bytescape-helper.exe')
Test-UsersCannotWrite (Join-Path $app 'bytescape.exe')

$output = & (Join-Path $app 'bytescape.exe') --version 2>&1 | Out-String
if ($LASTEXITCODE -ne 0 -or $output -notmatch [regex]::Escape($Version)) {
  Stop-Smoke "bytescape.exe --version gave: $output (exit $LASTEXITCODE)"
}
Write-Host "version output: $output"

foreach ($verb in $verbs) {
  $command = (Get-ItemProperty -Path (Join-Path $verb 'command')).'(default)'
  if ($command -notmatch 'bytescape\.exe') { Stop-Smoke "$verb has no command for bytescape.exe" }
}

$code = Run-Silently (Join-Path $app 'unins000.exe') @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART')
if ($code -ne 0) { Stop-Smoke "the uninstaller exited with $code" }
Start-Sleep -Seconds 2
if (Test-Path (Join-Path $app 'bytescape.exe')) { Stop-Smoke 'bytescape.exe is still installed' }
foreach ($verb in $verbs) {
  if (Test-Path $verb) { Stop-Smoke "$verb is still in the registry" }
}
Write-Host 'the installer check passed'
