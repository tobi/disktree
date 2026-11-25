# Install the pinned Inno Setup into -Dir for the current user and put it on
# the GitHub Actions PATH. Pinned and checked rather than the runner's own:
# the x64 and Arm64 images carry different, older versions.
param([Parameter(Mandatory)] [string] $Dir)

$ErrorActionPreference = 'Stop'
$url = 'https://github.com/jrsoftware/issrc/releases/download/is-7_1_0/innosetup-7.1.0-x64.exe'
$sha256 = '0362a383ed217d4c4239b5933866dd96d3eb2102737da92f80f6057a4b40df2f'

$setup = Join-Path ([IO.Path]::GetTempPath()) 'innosetup-7.1.0-x64.exe'
Invoke-WebRequest $url -OutFile $setup
$hash = (Get-FileHash $setup -Algorithm SHA256).Hash.ToLower()
if ($hash -ne $sha256) { throw "Inno Setup download has SHA256 $hash" }

$p = Start-Process $setup -Wait -PassThru -ArgumentList `
    '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-',
    '/CURRENTUSER', "/DIR=$Dir"
if ($p.ExitCode -ne 0) { throw "Inno Setup install exited $($p.ExitCode)" }
if (-not (Test-Path (Join-Path $Dir 'ISCC.exe'))) {
    throw "ISCC.exe is missing from $Dir"
}
if ($env:GITHUB_PATH) { $Dir >> $env:GITHUB_PATH }
