param(
    [Parameter(Position = 0)][string]$RustTarget = 'windows',
    [Parameter(Position = 1)][string]$NativeArchitecture
)

$ErrorActionPreference = 'Stop'
if (Test-Path Variable:PSNativeCommandUseErrorActionPreference) {
    $PSNativeCommandUseErrorActionPreference = $false
}
$SupportedTargets = @(
    [pscustomobject]@{ RustTarget = 'x86_64-unknown-linux-gnu'; OS = 'linux'; NativeArchitecture = 'x86_64' }
    [pscustomobject]@{ RustTarget = 'x86_64-unknown-linux-musl'; OS = 'linux'; NativeArchitecture = 'x86_64-musl' }
    [pscustomobject]@{ RustTarget = 'aarch64-unknown-linux-musl'; OS = 'linux'; NativeArchitecture = 'aarch64' }
    [pscustomobject]@{ RustTarget = 'armv7-unknown-linux-gnueabihf'; OS = 'linux'; NativeArchitecture = 'arm' }
    [pscustomobject]@{ RustTarget = 'i686-unknown-linux-gnu'; OS = 'linux'; NativeArchitecture = 'x86' }
    [pscustomobject]@{ RustTarget = 'x86_64-pc-windows-msvc'; OS = 'windows'; NativeArchitecture = 'x64' }
    [pscustomobject]@{ RustTarget = 'i686-pc-windows-msvc'; OS = 'windows'; NativeArchitecture = 'x86' }
    [pscustomobject]@{ RustTarget = 'aarch64-pc-windows-msvc'; OS = 'windows'; NativeArchitecture = 'arm64' }
    [pscustomobject]@{ RustTarget = 'aarch64-apple-darwin'; OS = 'macos'; NativeArchitecture = 'arm64' }
    [pscustomobject]@{ RustTarget = 'x86_64-apple-darwin'; OS = 'macos'; NativeArchitecture = 'x86_64' }
)

function Invoke-NativeCommand {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$ArgumentList = @(),
        [switch]$EchoOutput
    )

    Get-Command $FilePath -ErrorAction Stop | Out-Null
    $PreviousErrorActionPreference = $ErrorActionPreference
    try {
        # Windows PowerShell 5.1 wraps redirected native stderr in ErrorRecord
        # instances. Cargo writes normal progress to stderr, so keep those records
        # as build output and decide success from the native exit status instead.
        $ErrorActionPreference = 'Continue'
        $Output = @(
            & $FilePath @ArgumentList 2>&1 | ForEach-Object {
                $Line = if ($_ -is [System.Management.Automation.ErrorRecord]) {
                    $_.Exception.Message
                } else {
                    "$_"
                }
                if ($EchoOutput) { Write-Host $Line }
                $Line
            }
        )
        $ExitCode = if ($null -eq $LASTEXITCODE) { -1 } else { $LASTEXITCODE }
    } finally {
        $ErrorActionPreference = $PreviousErrorActionPreference
    }

    [pscustomobject]@{
        ExitCode = $ExitCode
        Output = $Output
    }
}

$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$CargoToml = Get-Content (Join-Path $Root 'Cargo.toml') -Raw
if ($CargoToml -notmatch '(?m)^version = "([^"]+)"') { throw 'Unable to read version from Cargo.toml' }
$Version = $Matches[1]

$RequestedBuilder = if ([string]::IsNullOrWhiteSpace($env:OM_STANDALONE_BUILDER)) {
    'auto'
} else {
    $env:OM_STANDALONE_BUILDER.ToLowerInvariant()
}
if ($RequestedBuilder -notin @('auto', 'cargo', 'zigbuild', 'xwin')) {
    throw 'OM_STANDALONE_BUILDER must be auto, cargo, zigbuild, or xwin'
}

$RustVersionResult = Invoke-NativeCommand -FilePath 'rustc' -ArgumentList @('-vV')
if ($RustVersionResult.ExitCode -ne 0) {
    throw "Unable to query rustc host target (exit status $($RustVersionResult.ExitCode))"
}
$RustVersion = @($RustVersionResult.Output)
$HostTargetLine = $RustVersion | Where-Object { "$_" -match '^host: ' } | Select-Object -First 1
if ($null -eq $HostTargetLine -or "$HostTargetLine" -notmatch '^host: (.+)$') {
    throw 'Unable to read the host target from rustc'
}
$HostTarget = $Matches[1]
$OutputDirectory = Join-Path $Root 'dist/standalone'
$MinimumGlibcVersion = '2.17'

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

function Ensure-RustTargets {
    param([object[]]$TargetDefinitions)

    $InstalledResult = Invoke-NativeCommand `
        -FilePath 'rustup' `
        -ArgumentList @('target', 'list', '--installed')
    if ($InstalledResult.ExitCode -ne 0) {
        throw "Unable to query installed Rust targets (exit status $($InstalledResult.ExitCode))"
    }

    $InstalledTargets = @($InstalledResult.Output | ForEach-Object { "$($_)".Trim() })
    $MissingTargets = @(
        $TargetDefinitions |
            Where-Object { $_.RustTarget -notin $InstalledTargets } |
            ForEach-Object { $_.RustTarget }
    )
    if ($MissingTargets.Count -eq 0) { return }

    Write-Host "Installing missing Rust targets: $($MissingTargets -join ', ')"
    $InstallResult = Invoke-NativeCommand `
        -FilePath 'rustup' `
        -ArgumentList (@('target', 'add') + $MissingTargets) `
        -EchoOutput
    if ($InstallResult.ExitCode -ne 0) {
        $Reason = Get-BuildFailureReason `
            -Output $InstallResult.Output `
            -ExitCode $InstallResult.ExitCode
        throw "Failed to install Rust targets: $Reason"
    }
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
        $HasXWin = $null -ne (Get-Command cargo-xwin -ErrorAction SilentlyContinue)
        if ($Target -eq 'aarch64-pc-windows-msvc' -and $HasXWin) {
            $Builder = 'xwin'
        } elseif ($Target -like '*-linux-gnu*' -and $HasZigBuild -and $HasZig) {
            $Builder = 'zigbuild'
        } elseif ($Target -like '*-linux-gnu*') {
            throw "cargo-zigbuild and Zig are required to build $Target against glibc $MinimumGlibcVersion"
        } elseif ($Target -ne $HostTarget -and $Target -like '*-linux-*' -and $HasZigBuild -and $HasZig) {
            $Builder = 'zigbuild'
        } else {
            $Builder = 'cargo'
        }
    }
    if ($Target -like '*-linux-gnu*' -and $Builder -ne 'zigbuild') {
        throw "$Target must be built with cargo-zigbuild to enforce the glibc $MinimumGlibcVersion baseline"
    }

    if ($Builder -eq 'xwin') {
        if ($null -eq (Get-Command cargo-xwin -ErrorAction SilentlyContinue)) {
            throw 'cargo-xwin is required; install it with: cargo install --locked cargo-xwin'
        }
        if ($null -eq (Get-Command clang -ErrorAction SilentlyContinue)) {
            throw 'Clang is required by cargo-xwin; install LLVM and make it available in PATH'
        }
        if ($null -eq (Get-Command lld-link -ErrorAction SilentlyContinue)) {
            throw 'lld-link is required by cargo-xwin; install LLVM and make it available in PATH'
        }
        $CargoArguments = @('xwin', 'build')
    } elseif ($Builder -eq 'zigbuild') {
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
    $CargoTarget = if ($Builder -eq 'zigbuild' -and $Target -like '*-linux-gnu*') {
        "$Target.$MinimumGlibcVersion"
    } else {
        $Target
    }
    $CargoArguments += @('--locked', '--release', '--target', $CargoTarget, '--bin', 'om-agent')

    Write-Host "Building $OS/$Architecture ($CargoTarget) with $Builder"
    $SetXWinCompiler =
        $Builder -eq 'xwin' -and
        [Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT -and
        [string]::IsNullOrWhiteSpace($env:XWIN_CROSS_COMPILER)
    if ($SetXWinCompiler) {
        # The clang-cl backend needs symlink privileges while preparing its SDK
        # cache on Windows. The clang sysroot backend works for standard users.
        $env:XWIN_CROSS_COMPILER = 'clang'
    }
    Push-Location $Root
    try {
        $BuildResult = Invoke-NativeCommand `
            -FilePath 'cargo' `
            -ArgumentList $CargoArguments `
            -EchoOutput
        $BuildOutput = @($BuildResult.Output)
        $BuildExitCode = $BuildResult.ExitCode
    } finally {
        Pop-Location
        if ($SetXWinCompiler) {
            Remove-Item Env:XWIN_CROSS_COMPILER -ErrorAction SilentlyContinue
        }
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

if ($RustTarget -ieq 'all' -or $RustTarget -ieq 'windows') {
    if (-not [string]::IsNullOrWhiteSpace($NativeArchitecture)) {
        throw 'NativeArchitecture must be omitted when RustTarget is all or windows'
    }

    $TargetDefinitions = if ($RustTarget -ieq 'windows') {
        @($SupportedTargets | Where-Object { $_.OS -eq 'windows' })
    } else {
        @($SupportedTargets)
    }
    if ($RustTarget -ieq 'windows') {
        Ensure-RustTargets -TargetDefinitions $TargetDefinitions
    }

    $Failures = [Collections.Generic.List[object]]::new()
    foreach ($TargetDefinition in $TargetDefinitions) {
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

    $TargetSet = if ($RustTarget -ieq 'windows') { 'Windows' } else { 'supported platform' }
    Write-Host "`nAll $($TargetDefinitions.Count) $TargetSet builds succeeded."
} else {
    $RequestedTarget = if ($RustTarget -ieq 'native') { $HostTarget } else { $RustTarget }
    $TargetDefinition = $SupportedTargets |
        Where-Object { $_.RustTarget -ieq $RequestedTarget } |
        Select-Object -First 1
    if ($null -eq $TargetDefinition) {
        $TargetList = ($SupportedTargets.RustTarget -join ', ')
        if ($RustTarget -ieq 'native') {
            throw "Native RustTarget '$HostTarget' is not supported. Supported targets: $TargetList"
        }
        throw "Unsupported RustTarget '$RequestedTarget'. Supported targets: $TargetList"
    }
    if (
        -not [string]::IsNullOrWhiteSpace($NativeArchitecture) -and
        $NativeArchitecture -ine $TargetDefinition.NativeArchitecture
    ) {
        throw "NativeArchitecture '$NativeArchitecture' does not match RustTarget '$RequestedTarget'; expected '$($TargetDefinition.NativeArchitecture)'"
    }

    Build-StandaloneTarget `
        -Target $TargetDefinition.RustTarget `
        -OS $TargetDefinition.OS `
        -Architecture $TargetDefinition.NativeArchitecture
}
