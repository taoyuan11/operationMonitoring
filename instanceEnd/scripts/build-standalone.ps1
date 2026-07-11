param(
    [Parameter(Mandatory = $true)][string]$RustTarget,
    [Parameter(Mandatory = $true)][ValidateSet('x64', 'arm64', 'x86')][string]$NativeArchitecture
)
$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$CargoToml = Get-Content (Join-Path $Root 'Cargo.toml') -Raw
if ($CargoToml -notmatch '(?m)^version = "([^"]+)"') { throw 'Unable to read version from Cargo.toml' }
$Version = $Matches[1]
Push-Location $Root
try { cargo build --locked --release --target $RustTarget --bin om-agent } finally { Pop-Location }
$Source = Join-Path $Root "target/$RustTarget/release/om-agent.exe"
if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) { throw "Built executable not found: $Source" }
$OutputDirectory = Join-Path $Root 'dist/standalone'
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
$Artifact = Join-Path $OutputDirectory "om-agent_${Version}_windows_${NativeArchitecture}.exe"
Copy-Item -LiteralPath $Source -Destination $Artifact -Force
$Hash = (Get-FileHash -LiteralPath $Artifact -Algorithm SHA256).Hash.ToLowerInvariant()
"$Hash  $([IO.Path]::GetFileName($Artifact))" | Set-Content -LiteralPath "$Artifact.sha256" -Encoding ascii
Write-Host "Created $Artifact"
Write-Host "Created $Artifact.sha256"
