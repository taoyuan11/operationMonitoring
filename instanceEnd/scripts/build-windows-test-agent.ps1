param(
    [ValidateSet('x64', 'arm64')][string]$Architecture = 'x64',
    [switch]$SkipDriverBuild
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0

function Write-Utf8NoBom {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Contents
    )

    [IO.File]::WriteAllText($Path, $Contents, [Text.UTF8Encoding]::new($false))
}

function Invoke-CheckedNative {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $true)][string[]]$Arguments
    )

    Write-Host "Running $([IO.Path]::GetFileName($FilePath)) $($Arguments -join ' ')"
    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$FilePath exited with status $LASTEXITCODE"
    }
}

function Find-WdkTool {
    param([Parameter(Mandatory = $true)][string]$Name)

    $Command = Get-Command $Name -ErrorAction SilentlyContinue
    if ($null -ne $Command) { return $Command.Source }
    $KitsRoot = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    $ToolsRoot = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\Tools'
    $SearchRoots = @($KitsRoot, $ToolsRoot) | Where-Object {
        Test-Path -LiteralPath $_ -PathType Container
    }
    $Candidates = foreach ($SearchRoot in $SearchRoots) {
        Get-ChildItem -LiteralPath $SearchRoot -Recurse -Filter $Name -File -ErrorAction SilentlyContinue |
            ForEach-Object {
                $ArchitectureRank = if ($_.FullName -match '\\x64\\') {
                    0
                } elseif ($_.FullName -match '\\x86\\') {
                    1
                } else {
                    2
                }
                $WdkVersion = [version]'0.0.0.0'
                if ($_.FullName -match '\\(?<wdkVersion>\d+(?:\.\d+){3})\\') {
                    $WdkVersion = [version]$Matches['wdkVersion']
                }
                [pscustomobject]@{
                    Path = $_.FullName
                    ArchitectureRank = $ArchitectureRank
                    WdkVersion = $WdkVersion
                }
            }
    }
    $Candidate = $Candidates |
        Sort-Object `
            ArchitectureRank, `
            @{ Expression = { $_.WdkVersion }; Descending = $true }, `
            Path |
        Select-Object -First 1 |
        Select-Object -ExpandProperty Path
    if ([string]::IsNullOrWhiteSpace($Candidate)) {
        throw "$Name was not found in the Windows Driver Kit"
    }
    return $Candidate
}

function Remove-OwnedDirectory {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$OwnerRoot
    )

    if (-not (Test-Path -LiteralPath $Path)) { return }
    $ResolvedPath = (Resolve-Path -LiteralPath $Path).Path
    $ResolvedRoot = (Resolve-Path -LiteralPath $OwnerRoot).Path
    $Prefix = $ResolvedRoot.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    if (-not $ResolvedPath.StartsWith($Prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to remove a path outside the owned artifact root: $ResolvedPath"
    }
    Remove-Item -LiteralPath $ResolvedPath -Recurse -Force
}

function Get-ProcessEnvironment {
    param([Parameter(Mandatory = $true)][string[]]$Names)

    $Values = @{}
    foreach ($Name in $Names) {
        $Values[$Name] = [Environment]::GetEnvironmentVariable($Name, 'Process')
    }
    return $Values
}

function Restore-ProcessEnvironment {
    param([Parameter(Mandatory = $true)][hashtable]$Values)

    foreach ($Entry in $Values.GetEnumerator()) {
        if ($null -eq $Entry.Value) {
            Remove-Item -LiteralPath "Env:$($Entry.Key)" -ErrorAction SilentlyContinue
        } else {
            Set-Item -LiteralPath "Env:$($Entry.Key)" -Value $Entry.Value
        }
    }
}

function Remove-TestCertificate {
    param(
        [Parameter(Mandatory = $true)][string]$Thumbprint,
        [Parameter(Mandatory = $true)][string]$Subject
    )

    $Stores = @('My', 'TrustedPeople', 'Root', 'TrustedPublisher')
    foreach ($Store in $Stores) {
        $Matches = @(Get-ChildItem -LiteralPath "Cert:\CurrentUser\$Store" | Where-Object Thumbprint -eq $Thumbprint)
        foreach ($Certificate in $Matches) {
            if ($Certificate.Subject -cne $Subject) {
                throw "Refusing to remove an unrelated certificate with thumbprint $Thumbprint from $Store"
            }
            Invoke-CheckedNative -FilePath 'certutil.exe' -Arguments @(
                '-user', '-delstore', $Store, $Thumbprint
            )
        }
    }
}

$AgentRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$DriverRoot = Join-Path $AgentRoot 'windows-drivers'
$ArtifactsRoot = Join-Path $DriverRoot 'artifacts'
$PackagesRoot = Join-Path $ArtifactsRoot 'packages'
$TestBundleRoot = Join-Path $ArtifactsRoot "test-bundles\om-windows-drivers-$Architecture-local-test"
$OutputRoot = Join-Path $AgentRoot 'dist\standalone'
$BuildDriversScript = Join-Path $AgentRoot 'scripts\build-windows-drivers.ps1'
$BuildAgentScript = Join-Path $AgentRoot 'scripts\build-standalone.ps1'
$SignTool = Find-WdkTool 'signtool.exe'
$Target = if ($Architecture -ceq 'x64') { 'x86_64-pc-windows-msvc' } else { 'aarch64-pc-windows-msvc' }
$CargoToml = Get-Content -LiteralPath (Join-Path $AgentRoot 'Cargo.toml') -Raw
if ($CargoToml -notmatch '(?m)^version = "([^"]+)"') {
    throw 'Unable to read Agent version from Cargo.toml'
}
$Version = $Matches[1]

if (-not $SkipDriverBuild) {
    & $BuildDriversScript -Architecture $Architecture
}

$SourceArchitectureRoot = Join-Path $PackagesRoot $Architecture
if (-not (Test-Path -LiteralPath $SourceArchitectureRoot -PathType Container)) {
    throw "Driver package root not found: $SourceArchitectureRoot"
}
New-Item -ItemType Directory -Force -Path $ArtifactsRoot, $OutputRoot | Out-Null
Remove-OwnedDirectory -Path $TestBundleRoot -OwnerRoot $ArtifactsRoot
New-Item -ItemType Directory -Force -Path $TestBundleRoot | Out-Null

$DraftLockPath = Join-Path $PackagesRoot "bundle-lock.$Architecture.draft.json"
if (-not (Test-Path -LiteralPath $DraftLockPath -PathType Leaf)) {
    throw "Driver bundle draft lock not found: $DraftLockPath"
}
$Lock = Get-Content -LiteralPath $DraftLockPath -Raw | ConvertFrom-Json
if ($Lock.production_ready -ne $false) {
    throw 'The local test bundle must remain production_ready=false'
}

$Subject = "CN=Operation Monitoring Local Driver Test $([guid]::NewGuid().ToString('N'))"
$Certificate = $null
$Thumbprint = $null
$CertificateOutput = Join-Path $OutputRoot "om-agent_${Version}_windows_${Architecture}_test-only.cer"
$EnvironmentNames = @(
    'OM_WINDOWS_TEST_DRIVER_BUNDLE_DIR',
    'OM_WINDOWS_TEST_SIGNING_CERTIFICATE_SHA1',
    'OM_SIGNTOOL_PATH',
    'OM_WINDOWS_DRIVER_BUNDLE_DIR',
    'OM_WINDOWS_SIGNING_CERTIFICATE_SHA1',
    'OM_WINDOWS_TIMESTAMP_URL'
)
$PreviousEnvironment = Get-ProcessEnvironment -Names $EnvironmentNames

try {
    $Certificate = New-SelfSignedCertificate `
        -Type CodeSigningCert `
        -Subject $Subject `
        -FriendlyName 'Operation Monitoring local Windows driver test certificate' `
        -CertStoreLocation 'Cert:\CurrentUser\My' `
        -KeyAlgorithm RSA `
        -KeyLength 3072 `
        -HashAlgorithm SHA256 `
        -NotAfter (Get-Date).AddYears(2)
    $Thumbprint = $Certificate.Thumbprint.Replace(' ', '').ToUpperInvariant()
    if ($Thumbprint -notmatch '^[0-9A-F]{40}$') {
        throw "New-SelfSignedCertificate returned an invalid thumbprint: $Thumbprint"
    }

    Export-Certificate -Cert $Certificate -FilePath $CertificateOutput -Type CERT -Force | Out-Null
    # certutil's forced per-user import is non-interactive. This is a temporary
    # Current User trust change; the finally block removes it from every store.
    foreach ($StoreName in @('Root', 'TrustedPublisher')) {
        Invoke-CheckedNative -FilePath 'certutil.exe' -Arguments @(
            '-user', '-addstore', '-f', $StoreName, $CertificateOutput
        )
    }

    foreach ($Kind in @('display', 'audio')) {
        $SourcePackage = Join-Path $SourceArchitectureRoot $Kind
        $DestinationPackage = Join-Path $TestBundleRoot "$Architecture\$Kind"
        New-Item -ItemType Directory -Force -Path $DestinationPackage | Out-Null
        $ExpectedFiles = if ($Kind -ceq 'display') {
            @('OmVirtualDisplay.inf', 'OmVirtualDisplay.cat', 'OmVirtualDisplay.dll')
        } else {
            @('OmVirtualAudio.inf', 'OmVirtualAudio.cat', 'OmVirtualAudio.sys')
        }
        foreach ($FileName in $ExpectedFiles) {
            $SourceFile = Join-Path $SourcePackage $FileName
            if (-not (Test-Path -LiteralPath $SourceFile -PathType Leaf)) {
                throw "Expected driver package file not found: $SourceFile"
            }
            Copy-Item -LiteralPath $SourceFile -Destination (Join-Path $DestinationPackage $FileName) -Force
        }
    }

    foreach ($Catalog in (Get-ChildItem -LiteralPath $TestBundleRoot -Recurse -Filter '*.cat' -File)) {
        Invoke-CheckedNative -FilePath $SignTool -Arguments @(
            'sign', '/sha1', $Thumbprint, '/fd', 'SHA256', '/v', $Catalog.FullName
        )
        Invoke-CheckedNative -FilePath $SignTool -Arguments @(
            'verify', '/pa', '/all', '/v', '/sha1', $Thumbprint, $Catalog.FullName
        )
    }

    foreach ($Package in @($Lock.architectures.$Architecture.packages)) {
        foreach ($File in @($Package.files)) {
            $FilePath = Join-Path $TestBundleRoot ($File.path.Replace('/', [IO.Path]::DirectorySeparatorChar))
            if (-not (Test-Path -LiteralPath $FilePath -PathType Leaf)) {
                throw "Bundle file not found after staging: $($File.path)"
            }
            $File.sha256 = (Get-FileHash -LiteralPath $FilePath -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    }
    $LockPath = Join-Path $TestBundleRoot 'bundle-lock.json'
    Write-Utf8NoBom -Path $LockPath -Contents ($Lock | ConvertTo-Json -Depth 10)

    $env:OM_WINDOWS_TEST_DRIVER_BUNDLE_DIR = $TestBundleRoot
    $env:OM_WINDOWS_TEST_SIGNING_CERTIFICATE_SHA1 = $Thumbprint
    $env:OM_SIGNTOOL_PATH = $SignTool
    Remove-Item -LiteralPath 'Env:OM_WINDOWS_DRIVER_BUNDLE_DIR' -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath 'Env:OM_WINDOWS_SIGNING_CERTIFICATE_SHA1' -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath 'Env:OM_WINDOWS_TIMESTAMP_URL' -ErrorAction SilentlyContinue
    & $BuildAgentScript `
        -RustTarget $Target `
        -NativeArchitecture $Architecture `
        -TestWindowsDrivers

    $ArtifactName = "om-agent_${Version}_windows_${Architecture}_test-only.exe"
    $ArtifactPath = Join-Path $OutputRoot $ArtifactName
    if (-not (Test-Path -LiteralPath $ArtifactPath -PathType Leaf)) {
        throw "Test-only Agent artifact not found: $ArtifactPath"
    }
    $Manifest = [ordered]@{
        package_type = 'standalone-test-only'
        agent = $ArtifactName
        architecture = $Architecture
        driver_bundle = (Split-Path -Leaf $TestBundleRoot)
        production_ready = $false
        driver_catalog_signer = [IO.Path]::GetFileName($CertificateOutput)
        requires_windows_testsigning = $true
        requires_reboot_after_testsigning_change = $true
        warning = 'For local disposable test machines only; never deploy to production or Secure Boot hosts.'
    }
    Write-Utf8NoBom `
        -Path (Join-Path $OutputRoot "${ArtifactName}.manifest.json") `
        -Contents ($Manifest | ConvertTo-Json -Depth 5)
    Write-Host "Created test-only Agent: $ArtifactPath"
    Write-Host "Created test certificate: $CertificateOutput"
} finally {
    try {
        if ($null -ne $Thumbprint) {
            Remove-TestCertificate -Thumbprint $Thumbprint -Subject $Subject
        }
    } finally {
        Restore-ProcessEnvironment -Values $PreviousEnvironment
    }
}
