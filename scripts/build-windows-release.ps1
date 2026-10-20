param([Parameter(Mandatory=$true)][string]$Destination)
$ErrorActionPreference='Stop'
$root=Split-Path $PSScriptRoot
$destinationPath=[IO.Path]::GetFullPath($Destination)
if(Test-Path -LiteralPath $destinationPath){throw 'Destination must be a new directory'}
Push-Location $root
try {
    & cargo build --locked --release
    if($LASTEXITCODE -ne 0){throw 'Release build failed'}
    $metadata=(& cargo metadata --no-deps --format-version 1 | ConvertFrom-Json)
    if($LASTEXITCODE -ne 0){throw 'Cargo metadata failed'}
    $package=$metadata.packages | Where-Object {$_.name -eq 'reach-mcp'}
    $version=$package.version
    $portable=Join-Path $destinationPath "reach-v$version-windows-x86_64"
    New-Item -ItemType Directory -Path $portable | Out-Null
    Copy-Item -LiteralPath (Join-Path $metadata.target_directory 'release\reach.exe') -Destination $portable
    foreach($name in @('README.md','LICENSE','THIRD_PARTY_NOTICES.md','config.example.json')) {
        Copy-Item -LiteralPath (Join-Path $root $name) -Destination $portable
    }
    $docs=Join-Path $portable 'docs'
    New-Item -ItemType Directory -Path $docs | Out-Null
    foreach($name in @('architecture.md','refactor-v0.3.md')) {
        Copy-Item -LiteralPath (Join-Path (Join-Path $root 'docs') $name) -Destination $docs
    }
    & (Join-Path $PSScriptRoot 'prepare-windows-runtime.ps1') -Destination $portable
    $zip=Join-Path $destinationPath "reach-v$version-windows-x86_64.zip"
    Compress-Archive -Path (Join-Path $portable '*') -DestinationPath $zip
    $lines=Get-ChildItem -LiteralPath $portable -Recurse -File | Sort-Object FullName | ForEach-Object {
        '{0}  {1}' -f (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLower(),$_.FullName.Substring($destinationPath.Length+1).Replace('\','/')
    }
    $lines+=('{0}  {1}' -f (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLower(),(Split-Path $zip -Leaf))
    [IO.File]::WriteAllLines((Join-Path $destinationPath 'SHA256SUMS.txt'),$lines,[Text.UTF8Encoding]::new($false))
    Write-Output "Built Reach $version portable package: $zip"
} finally {Pop-Location}
