# Packages a built Lector binary for Windows.
#
#   pwsh scripts/package-windows.ps1 -Binary target\x86_64-pc-windows-msvc\dist\lector.exe `
#        -Version 0.1.0 -Arch x64 -OutDir dist
#
# Produces in -OutDir:
#   Lector-<version>-windows-<arch>-portable.zip   portable build (settings stay next to the exe)
#   Lector-<version>-windows-<arch>-setup.exe      Inno Setup installer (per-user or all users)
#   Lector-<version>-windows-<arch>.msi            MSI for managed deployment (WiX v5)
param(
  [Parameter(Mandatory = $true)][string]$Binary,
  [Parameter(Mandatory = $true)][string]$Version,
  [Parameter(Mandatory = $true)][ValidateSet("x64", "arm64")][string]$Arch,
  [Parameter(Mandatory = $true)][string]$OutDir
)
$ErrorActionPreference = "Stop"
$Root = Resolve-Path (Join-Path $PSScriptRoot "..")
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = Resolve-Path $OutDir
$Work = Join-Path ([System.IO.Path]::GetTempPath()) ("lector-pkg-" + [guid]::NewGuid())
$Stage = Join-Path $Work "stage"
New-Item -ItemType Directory -Force -Path (Join-Path $Stage "third-party") | Out-Null

# --- staging ------------------------------------------------------------------------
Copy-Item $Binary (Join-Path $Stage "lector.exe")
Copy-Item (Join-Path $Root "assets\app-icon\lector.ico") $Stage
foreach ($f in "README.md", "CHANGELOG.md", "LICENSE-MIT", "LICENSE-APACHE") {
  Copy-Item (Join-Path $Root $f) $Stage
}
$tp = Join-Path $Stage "third-party"
Copy-Item (Join-Path $Root "assets\THIRD_PARTY.md") (Join-Path $tp "ASSETS.md")
Copy-Item (Join-Path $Root "crates\lector-icons\THIRD_PARTY.md") (Join-Path $tp "ICONS.md") -ErrorAction SilentlyContinue
Copy-Item (Join-Path $Root "crates\lector-syntax\GRAMMARS.md") (Join-Path $tp "GRAMMARS.md") -ErrorAction SilentlyContinue
Get-ChildItem (Join-Path $Root "assets\fonts") -Recurse -Filter "OFL*" | ForEach-Object {
  Copy-Item $_.FullName (Join-Path $tp ("{0}-OFL.txt" -f $_.Directory.Name))
}
Get-ChildItem (Join-Path $Root "crates\lector-icons\assets") -Recurse -Depth 1 -Filter "LICENSE*" | ForEach-Object {
  Copy-Item $_.FullName (Join-Path $tp ("{0}-LICENSE.txt" -f $_.Directory.Name))
}
$crates = Join-Path $Root "target\THIRD_PARTY_CRATES.md"
if (Test-Path $crates) { Copy-Item $crates (Join-Path $tp "CRATES.md") }

# RTF license for the MSI dialog.
$mit = (Get-Content (Join-Path $Root "LICENSE-MIT") -Raw).Replace('\', '\\').Replace('{', '\{').Replace('}', '\}')
$rtf = "{\rtf1\ansi\deff0{\fonttbl{\f0 Segoe UI;}}\fs18 " + ($mit -replace "`r?`n", "\par ") + "}"
Set-Content -Path (Join-Path $Stage "LICENSE.rtf") -Value $rtf -Encoding ascii

# --- portable zip ---------------------------------------------------------------------
$name = "Lector-$Version-windows-$Arch-portable"
$portable = Join-Path $Work $name
New-Item -ItemType Directory -Force -Path $portable | Out-Null
Get-ChildItem $Stage -Exclude "LICENSE.rtf", "lector.ico" | Copy-Item -Destination $portable -Recurse
# The presence of lector-data\ next to lector.exe enables portable mode.
New-Item -ItemType Directory -Force -Path (Join-Path $portable "lector-data") | Out-Null
Set-Content -Path (Join-Path $portable "lector-data\README.txt") -Encoding utf8 -Value @"
Portable mode: Lector keeps its settings and state in this folder.
Delete this folder to use the normal per-user locations instead.
"@
Compress-Archive -Path $portable -DestinationPath (Join-Path $OutDir "$name.zip") -Force
Write-Host "built $OutDir\$name.zip"

# --- Inno Setup installer -------------------------------------------------------------------
$iscc = (Get-Command ISCC.exe -ErrorAction SilentlyContinue).Source
if (-not $iscc) {
  foreach ($c in "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe") {
    if (Test-Path $c) { $iscc = $c; break }
  }
}
if ($iscc) {
  & $iscc /Q "/DAppVersion=$Version" "/DArch=$Arch" "/DSourceDir=$Stage" "/DOutputDir=$OutDir" (Join-Path $Root "packaging\windows\lector.iss")
  if ($LASTEXITCODE -ne 0) { throw "ISCC failed ($LASTEXITCODE)" }
  Write-Host "built $OutDir\Lector-$Version-windows-$Arch-setup.exe"
} else {
  Write-Warning "Inno Setup (ISCC.exe) not found; skipping setup.exe"
}

# --- MSI (WiX v5) -----------------------------------------------------------------------
if (Get-Command wix -ErrorAction SilentlyContinue) {
  $msi = Join-Path $OutDir "Lector-$Version-windows-$Arch.msi"
  wix build (Join-Path $Root "packaging\windows\lector.wxs") -arch $Arch -ext WixToolset.UI.wixext `
    -d "Version=$Version" -d "SourceDir=$Stage" -o $msi
  if ($LASTEXITCODE -ne 0) { throw "wix build failed ($LASTEXITCODE)" }
  Write-Host "built $msi"
} else {
  Write-Warning "WiX (wix) not found; skipping .msi"
}

Remove-Item -Recurse -Force $Work
