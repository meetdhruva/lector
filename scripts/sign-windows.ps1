# Authenticode-signs (or verifies) Windows binaries and installers (docs/RELEASING.md).
#
#   pwsh scripts/sign-windows.ps1 -Files target\...\lector.exe, target\...\lector-cli.exe
#   pwsh scripts/sign-windows.ps1 -Files dist\Lector-0.7.0-windows-x64-setup.exe -Verify
#
# Provider, from the environment (the workflow maps GitHub secrets to these):
#   SIGN_PROVIDER=azure   Azure Artifact Signing (formerly Trusted Signing) through signtool + the
#                         Azure.CodeSigning.Dlib. Needs AZURE_TENANT_ID, AZURE_CLIENT_ID,
#                         AZURE_CLIENT_SECRET (a service principal with the "Artifact Signing
#                         Certificate Profile Signer" role), AZURE_SIGNING_ENDPOINT,
#                         AZURE_SIGNING_ACCOUNT, AZURE_SIGNING_PROFILE.
#                         ARTIFACT_SIGNING_CLIENT_VERSION overrides the pinned dlib NuGet version.
#   SIGN_PROVIDER=pfx     A certificate file: WINDOWS_CERTIFICATE_PFX_BASE64 + WINDOWS_CERTIFICATE_PASSWORD.
#   Both use SHA-256 and an RFC 3161 timestamp (WINDOWS_TIMESTAMP_URL, default per provider).
# -Verify runs `signtool verify /pa /v` on every file instead of signing.
#
# Secrets are never printed. Not runnable outside Windows (signtool).
param(
  [Parameter(Mandatory = $true)][string[]]$Files,
  [switch]$Verify
)
$ErrorActionPreference = "Stop"

$DefaultDlibVersion = "1.0.128"   # Microsoft.ArtifactSigning.Client; bump deliberately

function Find-SignTool {
  $bins = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
  $candidates = Get-ChildItem -Path $bins -Directory -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -match '^\d+\.\d+\.\d+\.\d+$' } |
    Sort-Object { [version]$_.Name } -Descending
  foreach ($dir in $candidates) {
    $exe = Join-Path $dir.FullName "x64\signtool.exe"
    if (Test-Path $exe) { return $exe }
  }
  throw "signtool.exe not found under $bins (install the Windows SDK)"
}

function Invoke-SignTool {
  param([string]$SignTool, [string[]]$Arguments, [int]$Attempts = 1)
  for ($i = 1; $i -le $Attempts; $i++) {
    & $SignTool @Arguments
    if ($LASTEXITCODE -eq 0) { return }
    if ($i -lt $Attempts) {
      Write-Warning "signtool failed ($LASTEXITCODE), retrying ($i/$Attempts)"
      Start-Sleep -Seconds (10 * $i)
    }
  }
  throw "signtool failed with exit code $LASTEXITCODE"
}

$signtool = Find-SignTool
Write-Host "signtool: $signtool"
$resolved = @()
foreach ($f in $Files) {
  if (-not (Test-Path -LiteralPath $f)) { throw "file not found: $f" }
  $resolved += (Resolve-Path -LiteralPath $f).Path
}

if ($Verify) {
  foreach ($f in $resolved) {
    Invoke-SignTool -SignTool $signtool -Arguments @("verify", "/pa", "/v", $f)
    $sig = Get-AuthenticodeSignature -LiteralPath $f
    Write-Host ("{0}: {1}, signer {2}" -f (Split-Path $f -Leaf), $sig.Status, $sig.SignerCertificate.Subject)
  }
  return
}

$provider = $env:SIGN_PROVIDER
$work = Join-Path $(if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }) "lector-sign"
New-Item -ItemType Directory -Force -Path $work | Out-Null

switch ($provider) {
  "azure" {
    foreach ($name in "AZURE_TENANT_ID", "AZURE_CLIENT_ID", "AZURE_CLIENT_SECRET", "AZURE_SIGNING_ENDPOINT", "AZURE_SIGNING_ACCOUNT", "AZURE_SIGNING_PROFILE") {
      if (-not [Environment]::GetEnvironmentVariable($name)) { throw "$name is not set" }
    }
    $dlib = Get-ChildItem -Path $work -Recurse -Filter "Azure.CodeSigning.Dlib.dll" -ErrorAction SilentlyContinue |
      Where-Object { $_.FullName -match '[\\/]x64[\\/]' } | Select-Object -First 1
    if (-not $dlib) {
      $version = if ($env:ARTIFACT_SIGNING_CLIENT_VERSION) { $env:ARTIFACT_SIGNING_CLIENT_VERSION } else { $DefaultDlibVersion }
      $pkg = Join-Path $work "client.zip"
      Invoke-WebRequest -UseBasicParsing -Uri "https://www.nuget.org/api/v2/package/Microsoft.ArtifactSigning.Client/$version" -OutFile $pkg
      Expand-Archive -Path $pkg -DestinationPath (Join-Path $work "client") -Force
      $dlib = Get-ChildItem -Path (Join-Path $work "client") -Recurse -Filter "Azure.CodeSigning.Dlib.dll" |
        Where-Object { $_.FullName -match '[\\/]x64[\\/]' } | Select-Object -First 1
      if (-not $dlib) { throw "Azure.CodeSigning.Dlib.dll (x64) not found in Microsoft.ArtifactSigning.Client $version" }
    }
    $metadata = Join-Path $work "metadata.json"
    @{
      Endpoint               = $env:AZURE_SIGNING_ENDPOINT
      CodeSigningAccountName = $env:AZURE_SIGNING_ACCOUNT
      CertificateProfileName = $env:AZURE_SIGNING_PROFILE
    } | ConvertTo-Json | Set-Content -Path $metadata -Encoding utf8
    # Artifact Signing certificates live three days: the timestamp is what keeps signatures valid.
    $timestamp = if ($env:WINDOWS_TIMESTAMP_URL) { $env:WINDOWS_TIMESTAMP_URL } else { "http://timestamp.acs.microsoft.com" }
    foreach ($f in $resolved) {
      Write-Host "signing $(Split-Path $f -Leaf) (Azure Artifact Signing)"
      Invoke-SignTool -SignTool $signtool -Attempts 3 -Arguments @(
        "sign", "/v", "/fd", "SHA256", "/tr", $timestamp, "/td", "SHA256",
        "/dlib", $dlib.FullName, "/dmdf", $metadata, $f)
    }
  }
  "pfx" {
    if (-not $env:WINDOWS_CERTIFICATE_PFX_BASE64) { throw "WINDOWS_CERTIFICATE_PFX_BASE64 is not set" }
    if (-not $env:WINDOWS_CERTIFICATE_PASSWORD) { throw "WINDOWS_CERTIFICATE_PASSWORD is not set" }
    $pfx = Join-Path $work "certificate.pfx"
    [IO.File]::WriteAllBytes($pfx, [Convert]::FromBase64String($env:WINDOWS_CERTIFICATE_PFX_BASE64))
    $timestamp = if ($env:WINDOWS_TIMESTAMP_URL) { $env:WINDOWS_TIMESTAMP_URL } else { "http://timestamp.digicert.com" }
    try {
      foreach ($f in $resolved) {
        Write-Host "signing $(Split-Path $f -Leaf) (certificate file)"
        Invoke-SignTool -SignTool $signtool -Attempts 3 -Arguments @(
          "sign", "/v", "/fd", "SHA256", "/tr", $timestamp, "/td", "SHA256",
          "/f", $pfx, "/p", $env:WINDOWS_CERTIFICATE_PASSWORD, $f)
      }
    } finally {
      Remove-Item -Force -ErrorAction SilentlyContinue $pfx
    }
  }
  default { throw "SIGN_PROVIDER must be 'azure' or 'pfx' (got '$provider')" }
}
