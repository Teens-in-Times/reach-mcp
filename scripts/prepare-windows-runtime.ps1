param([Parameter(Mandatory=$true)][string]$Destination)
$ErrorActionPreference='Stop'
# Official Microsoft redistributable, pinned and signature-checked. No system installation.
$version='1.25.260930003'
$archiveHash='02B07B349AF66D801159BDF9E440D4A1CE78BB951F37FC8609731665AFDAE7EE'
$cache=Join-Path ([IO.Path]::GetTempPath()) "reach-conpty-$version"
New-Item -ItemType Directory -Force -Path $cache | Out-Null
$archive=Join-Path $cache 'package.zip'
if (-not (Test-Path -LiteralPath $archive)) {
    Invoke-WebRequest -Uri "https://api.nuget.org/v3-flatcontainer/microsoft.windows.console.conpty/$version/microsoft.windows.console.conpty.$version.nupkg" -OutFile $archive
}
if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ne $archiveHash) {throw 'ConPTY package hash mismatch'}
$package=Join-Path $cache 'package'
if (-not (Test-Path -LiteralPath $package)) {Expand-Archive -LiteralPath $archive -DestinationPath $package}
$files=@(
    @{Source='runtimes\win-x64\native\conpty.dll';Target='conpty.dll';Hash='FEEEF341D891643C62D30B6B07800BC70F0BC148F44CB8C3BEE84AA557AE805A'},
    @{Source='build\native\runtimes\x64\OpenConsole.exe';Target='x64\OpenConsole.exe';Hash='3D66B23D0A71BB8EED2B77EDC8B9DF9BF54CE6C8FB74C863A30E760997F80586'}
)
$destinationPath=[IO.Path]::GetFullPath($Destination)
foreach($entry in $files) {
    $source=Join-Path $package $entry.Source
    if ((Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash -ne $entry.Hash) {throw "ConPTY file hash mismatch: $($entry.Target)"}
    $signature=Get-AuthenticodeSignature -LiteralPath $source
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch '^CN=Microsoft Corporation,') {throw "Invalid Microsoft signature: $($entry.Target)"}
    $target=Join-Path $destinationPath $entry.Target
    New-Item -ItemType Directory -Force -Path (Split-Path $target) | Out-Null
    Copy-Item -LiteralPath $source -Destination $target -Force
}
Write-Output "Prepared Microsoft ConPTY $version (x64) in $destinationPath"
