<#
.SYNOPSIS
  Download the Prism release this checkout is pinned to (speech-spec §4.3) and extract prism.dll.

.DESCRIPTION
  The pinned tag is read from the external/prism submodule (git describe). The matching
  prism-windows-x64.zip (or -arm64) is fetched from GitHub releases and the release prism.dll is
  placed in -Dest. Only prism.dll is needed beside loadstar.dll: the companion libraries in the
  zip are statically folded into the release build's exports we use. Requires `gh` (GitHub CLI)
  or falls back to a direct download URL.

.EXAMPLE
  tools/fetch-prism.ps1 -Dest vendor
  tools/fetch-prism.ps1 -Dest C:\Games\Wrath -Arch arm64
#>
param(
    [string]$Dest = "vendor",
    [ValidateSet("x64", "arm64")] [string]$Arch = "x64",
    [string]$Tag = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)

if ($Tag -eq "") {
    $Tag = (git -C (Join-Path $root "external/prism") describe --tags --exact-match 2>$null)
    if (-not $Tag) { throw "external/prism is not at a tag; pass -Tag vX.Y.Z or check the submodule out at a release" }
}

$asset = "prism-windows-$Arch.zip"
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) "loadstar-prism-$Tag-$Arch"
New-Item -ItemType Directory -Force $tmp | Out-Null
$zip = Join-Path $tmp $asset

Write-Host "Fetching Prism $Tag ($asset)..."
if (Get-Command gh -ErrorAction SilentlyContinue) {
    gh release download $Tag --repo ethindp/prism --pattern $asset --dir $tmp --clobber
} else {
    Invoke-WebRequest -Uri "https://github.com/ethindp/prism/releases/download/$Tag/$asset" -OutFile $zip
}

Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [System.IO.Compression.ZipFile]::OpenRead($zip)
try {
    $entry = $archive.Entries | Where-Object { $_.FullName -eq "dynamic/release/bin/prism.dll" }
    if (-not $entry) { throw "dynamic/release/bin/prism.dll not found in $asset" }
    New-Item -ItemType Directory -Force $Dest | Out-Null
    $out = Join-Path $Dest "prism.dll"
    [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $out, $true)
    $header = $archive.Entries | Where-Object { $_.FullName -eq "include/prism.h" }
    if ($header) {
        $tmpHeader = Join-Path $tmp "prism.h"
        [System.IO.Compression.ZipFileExtensions]::ExtractToFile($header, $tmpHeader, $true)
        $pinned = Join-Path $root "external/prism/include/prism.h"
        if ((Get-FileHash $tmpHeader).Hash -ne (Get-FileHash $pinned).Hash) {
            Write-Warning "The release header differs from the pinned submodule header - re-check crates/loadstar-speech/src/backends/prism.rs against it."
        }
    }
    Write-Host "Wrote $out ($((Get-Item $out).Length) bytes) from Prism $Tag"
} finally {
    $archive.Dispose()
}
