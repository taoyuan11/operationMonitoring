param(
    [Parameter(Mandatory = $true, Position = 0)][string]$RustTarget,
    [Parameter(Position = 1)][ValidateSet('x64', 'arm64', 'x86')][string]$NativeArchitecture
)

$ErrorActionPreference = 'Stop'
if (Test-Path Variable:PSNativeCommandUseErrorActionPreference) {
    $PSNativeCommandUseErrorActionPreference = $false
}
$SupportedTargets = @(
    [pscustomobject]@{ RustTarget = 'x86_64-unknown-linux-gnu'; OS = 'linux'; NativeArchitecture = 'x86_64' }
    [pscustomobject]@{ RustTarget = 'aarch64-unknown-linux-musl'; OS = 'linux'; NativeArchitecture = 'aarch64' }
    [pscustomobject]@{ RustTarget = 'armv7-unknown-linux-gnueabihf'; OS = 'linux'; NativeArchitecture = 'arm' }
    [pscustomobject]@{ RustTarget = 'i686-unknown-linux-gnu'; OS = 'linux'; NativeArchitecture = 'x86' }
    [pscustomobject]@{ RustTarget = 'x86_64-pc-windows-msvc'; OS = 'windows'; NativeArchitecture = 'x64' }
    [pscustomobject]@{ RustTarget = 'aarch64-pc-windows-msvc'; OS = 'windows'; NativeArchitecture = 'arm64' }
    [pscustomobject]@{ RustTarget = 'i686-pc-windows-msvc'; OS = 'windows'; NativeArchitecture = 'x86' }
    [pscustomobject]@{ RustTarget = 'aarch64-apple-darwin'; OS = 'macos'; NativeArchitecture = 'arm64' }
    [pscustomobject]@{ RustTarget = 'x86_64-apple-darwin'; OS = 'macos'; NativeArchitecture = 'x86_64' }
)

$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$CargoToml = Get-Content (Join-Path $Root 'Cargo.toml') -Raw
if ($CargoToml -notmatch '(?m)^version = "([^"]+)"') { throw 'Unable to read version from Cargo.toml' }
$Version = $Matches[1]

$RequestedBuilder = if ([string]::IsNullOrWhiteSpace($env:OM_STANDALONE_BUILDER)) {
    'auto'
} else {
    $env:OM_STANDALONE_BUILDER.ToLowerInvariant()
}
if ($RequestedBuilder -notin @('auto', 'cargo', 'zigbuild')) {
    throw 'OM_STANDALONE_BUILDER must be auto, cargo, or zigbuild'
}

$RustVersion = @(& rustc -vV 2>&1)
if ($LASTEXITCODE -ne 0) { throw "Unable to query rustc host target (exit status $LASTEXITCODE)" }
$HostTargetLine = $RustVersion | Where-Object { "$_" -match '^host: ' } | Select-Object -First 1
if ($null -eq $HostTargetLine -or "$HostTargetLine" -notmatch '^host: (.+)$') {
    throw 'Unable to read the host target from rustc'
}
$HostTarget = $Matches[1]
$OutputDirectory = Join-Path $Root 'dist/standalone'

function Get-BuildFailureReason {
    param(
        [object[]]$Output,
        [int]$ExitCode
    )

    $Lines = @($Output | ForEach-Object { "$_" } | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    $Reason = $Lines | Where-Object { $_ -match '^\s*error(\[[^]]+\])?:' } | Select-Object -First 1
    if ([string]::IsNullOrWhiteSpace($Reason) -and $Lines.Count -gt 0) {
        $Reason = $Lines[-1]
    }
    if ([string]::IsNullOrWhiteSpace($Reason)) {
        return "build command exited with status $ExitCode"
    }
    return "$Reason (exit status $ExitCode)"
}

function Build-StandaloneTarget {
    param(
        [string]$Target,
        [ValidateSet('linux', 'windows', 'macos')][string]$OS,
        [string]$Architecture
    )

    $Builder = $RequestedBuilder
    if ($Builder -eq 'auto') {
        $HasZigBuild = $null -ne (Get-Command cargo-zigbuild -ErrorAction SilentlyContinue)
        $HasZig = $null -ne (Get-Command zig -ErrorAction SilentlyContinue)
        if ($Target -ne $HostTarget -and $Target -like '*-linux-*' -and $HasZigBuild -and $HasZig) {
            $Builder = 'zigbuild'
        } else {
            $Builder = 'cargo'
        }
    }

    if ($Builder -eq 'zigbuild') {
        if ($null -eq (Get-Command cargo-zigbuild -ErrorAction SilentlyContinue)) {
            throw 'cargo-zigbuild is required; install it with: cargo install cargo-zigbuild'
        }
        if ($null -eq (Get-Command zig -ErrorAction SilentlyContinue)) {
            throw 'Zig is required by cargo-zigbuild; install Zig and make it available in PATH'
        }
        $CargoArguments = @('zigbuild')
    } else {
        $CargoArguments = @('build')
    }
    $CargoArguments += @('--locked', '--release', '--target', $Target, '--bin', 'om-agent')

    Write-Host "Building $OS/$Architecture ($Target) with $Builder"
    Push-Location $Root
    try {
        $BuildOutput = @()
        & cargo @CargoArguments 2>&1 |
            Tee-Object -Variable BuildOutput |
            ForEach-Object { Write-Host "$_" }
        $BuildExitCode = $LASTEXITCODE
    } finally {
        Pop-Location
    }
    if ($BuildExitCode -ne 0) {
        $Reason = Get-BuildFailureReason -Output $BuildOutput -ExitCode $BuildExitCode
        throw $Reason
    }

    if ($OS -eq 'windows') {
        $ExecutableName = 'om-agent.exe'
        $Extension = 'exe'
    } else {
        $ExecutableName = 'om-agent'
        $Extension = 'bin'
    }
    $Source = Join-Path $Root "target/$Target/release/$ExecutableName"
    if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) { throw "Built executable not found: $Source" }

    New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
    $Artifact = Join-Path $OutputDirectory "om-agent_${Version}_${OS}_${Architecture}.$Extension"
    Copy-Item -LiteralPath $Source -Destination $Artifact -Force
    $Hash = (Get-FileHash -LiteralPath $Artifact -Algorithm SHA256).Hash.ToLowerInvariant()
    "$Hash  $([IO.Path]::GetFileName($Artifact))" | Set-Content -LiteralPath "$Artifact.sha256" -Encoding ascii
    Write-Host "Created $Artifact"
    Write-Host "Created $Artifact.sha256"
}

if ($RustTarget -ieq 'all') {
    if (-not [string]::IsNullOrWhiteSpace($NativeArchitecture)) {
        throw 'NativeArchitecture must be omitted when RustTarget is all'
    }

    $Failures = [Collections.Generic.List[object]]::new()
    foreach ($TargetDefinition in $SupportedTargets) {
        $Platform = "$($TargetDefinition.OS)/$($TargetDefinition.NativeArchitecture) ($($TargetDefinition.RustTarget))"
        Write-Host "`n=== $Platform ==="
        try {
            Build-StandaloneTarget `
                -Target $TargetDefinition.RustTarget `
                -OS $TargetDefinition.OS `
                -Architecture $TargetDefinition.NativeArchitecture
            Write-Host "Succeeded: $Platform"
        } catch {
            $Reason = $_.Exception.Message
            $Failures.Add([pscustomobject]@{ Platform = $Platform; Reason = $Reason })
            Write-Warning "Failed: ${Platform}: $Reason"
        }
    }

    if ($Failures.Count -gt 0) {
        Write-Host "`nBuild failures ($($Failures.Count)):" -ForegroundColor Red
        foreach ($Failure in $Failures) {
            Write-Host "  - $($Failure.Platform): $($Failure.Reason)" -ForegroundColor Red
        }
        exit 1
    }

    Write-Host "`nAll $($SupportedTargets.Count) supported platform builds succeeded."
} else {
    if ([string]::IsNullOrWhiteSpace($NativeArchitecture)) {
        throw 'NativeArchitecture is required unless RustTarget is all'
    }
    Build-StandaloneTarget -Target $RustTarget -OS 'windows' -Architecture $NativeArchitecture
}
