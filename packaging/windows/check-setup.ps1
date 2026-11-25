# Install a built setup silently for the current user, check what it put in
# place, install it again over itself as an upgrade, then uninstall and check
# that nothing is left. Exits non-zero on the first thing out of place.
param([Parameter(Mandatory)] [string] $Setup)

$ErrorActionPreference = 'Stop'
$app = Join-Path $env:LOCALAPPDATA 'Programs\disktree'
$exe = Join-Path $app 'disktree.exe'
$start = Join-Path ([Environment]::GetFolderPath('Programs')) 'disktree.lnk'
$desktop = Join-Path ([Environment]::GetFolderPath('Desktop')) 'disktree.lnk'
# The AppId in disktree.iss, as Inno Setup names its uninstall key.
$key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\' +
    '{354830A5-B031-45DE-8595-4513BA422E5F}_is1'
$installed = @($exe, $start, $desktop, $key)

function Invoke-Quiet([string] $Path) {
    $p = Start-Process $Path -Wait -PassThru -ArgumentList `
        '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/CURRENTUSER'
    if ($p.ExitCode -ne 0) { throw "$Path exited $($p.ExitCode)" }
}

Invoke-Quiet $Setup
foreach ($path in $installed) {
    if (-not (Test-Path $path)) { throw "not installed: $path" }
}
Invoke-Quiet $Setup
$entries = @(Get-ChildItem 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall' |
    Where-Object PSChildName -Like '*354830A5-B031-45DE-8595-4513BA422E5F*')
if ($entries.Count -ne 1) { throw "upgrade left $($entries.Count) entries" }

Invoke-Quiet (Join-Path $app 'unins000.exe')
# The uninstaller hands the last deletions to a copy of itself that outlives
# the process waited on, so give it a moment before looking.
$deadline = (Get-Date).AddSeconds(30)
while ((Test-Path $app) -and (Get-Date) -lt $deadline) { Start-Sleep 1 }
foreach ($path in $installed + $app) {
    if (Test-Path $path) { throw "left after uninstall: $path" }
}
Write-Output "setup installs, upgrades and uninstalls cleanly: $Setup"
