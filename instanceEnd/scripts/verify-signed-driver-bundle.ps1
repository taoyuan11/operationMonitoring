param(
    [Parameter(Mandatory = $true)][string]$BundleDir,
    [Parameter(Mandatory = $true)][ValidateSet('x64', 'arm64')][string]$Architecture,
    [string]$AgentVersion,
    [string]$SigntoolPath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0

function Invoke-CheckedNative {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $true)][string[]]$Arguments
    )

    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$FilePath exited with status $LASTEXITCODE"
    }
}

function Test-NormalizedRelativePath {
    param([string]$Value)

    if ([string]::IsNullOrWhiteSpace($Value) -or $Value.Contains('\') -or $Value.Contains(':')) {
        return $false
    }
    $Parts = $Value.Split('/')
    return -not ($Parts | Where-Object { $_ -eq '' -or $_ -eq '.' -or $_ -eq '..' })
}

function Resolve-BundleFile {
    param([string]$Root, [string]$RelativePath)

    if (-not (Test-NormalizedRelativePath $RelativePath)) {
        throw "Driver bundle path is not a normalized forward-slash relative path: $RelativePath"
    }
    $Candidate = [IO.Path]::GetFullPath((Join-Path $Root ($RelativePath.Replace('/', [IO.Path]::DirectorySeparatorChar))))
    $RootPrefix = $Root.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    if (-not $Candidate.StartsWith($RootPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Driver bundle path escapes its root: $RelativePath"
    }
    return $Candidate
}

function Get-ArchitectureEntry {
    param([object]$Architectures, [string]$Name)

    $Property = $Architectures.PSObject.Properties[$Name]
    if ($null -eq $Property) { throw "Bundle does not contain architecture $Name" }
    return $Property.Value
}

function Assert-ObjectProperties {
    param(
        [object]$Value,
        [string[]]$Allowed,
        [string[]]$Required,
        [string]$Context
    )

    $Names = @($Value.PSObject.Properties.Name)
    foreach ($Name in $Names) {
        if (-not ($Allowed -ccontains $Name)) { throw "$Context contains unknown property '$Name'" }
    }
    foreach ($Name in $Required) {
        if (-not ($Names -ccontains $Name)) { throw "$Context is missing required property '$Name'" }
    }
}

function Get-InfOwnership {
    param([Parameter(Mandatory = $true)][string]$Text)

    $Sections = @{}
    $Pattern = '(?ms)^\s*\[(?<name>[^\]]+)\]\s*\r?\n(?<body>.*?)(?=^\s*\[|\z)'
    foreach ($Match in [regex]::Matches($Text, $Pattern)) {
        $Sections[$Match.Groups['name'].Value.Trim().ToLowerInvariant()] = $Match.Groups['body'].Value
    }
    if (-not $Sections.ContainsKey('version') -or -not $Sections.ContainsKey('manufacturer')) {
        throw 'INF is missing Version or Manufacturer section'
    }

    $VersionValues = @{}
    foreach ($RawLine in ($Sections['version'] -split '\r?\n')) {
        $Line = ($RawLine -replace ';.*$', '').Trim()
        if ($Line -match '^([^=]+)=(.*)$') {
            $VersionValues[$Matches[1].Trim().ToLowerInvariant()] = $Matches[2].Trim()
        }
    }

    $StringValues = @{}
    if (-not $Sections.ContainsKey('strings')) {
        throw 'INF is missing Strings section'
    }
    foreach ($RawLine in ($Sections['strings'] -split '\r?\n')) {
        $Line = ($RawLine -replace ';.*$', '').Trim()
        if ([string]::IsNullOrWhiteSpace($Line)) { continue }
        if ($Line -notmatch '^([^=]+)=(.*)$') { throw "Invalid INF Strings line: $Line" }
        $StringValue = $Matches[2].Trim()
        if ($StringValue.Length -ge 2 -and $StringValue[0] -eq '"' -and $StringValue[$StringValue.Length - 1] -eq '"') {
            $StringValue = $StringValue.Substring(1, $StringValue.Length - 2)
        }
        $StringValues[$Matches[1].Trim().ToLowerInvariant()] = $StringValue
    }

    $ManufacturerModels = [Collections.Generic.List[string]]::new()
    $ManufacturerDecorations = [Collections.Generic.List[string]]::new()
    foreach ($RawLine in ($Sections['manufacturer'] -split '\r?\n')) {
        $Line = ($RawLine -replace ';.*$', '').Trim()
        if ([string]::IsNullOrWhiteSpace($Line)) { continue }
        if ($Line -notmatch '^[^=]+=(.*)$') { throw "Invalid INF Manufacturer line: $Line" }
        $Parts = @($Matches[1].Split(',') | ForEach-Object { $_.Trim() })
        $ManufacturerModels.Add($Parts[0])
        if ($Parts.Count -gt 1) {
            foreach ($Decoration in $Parts[1..($Parts.Count - 1)]) {
                if (-not [string]::IsNullOrWhiteSpace($Decoration)) { $ManufacturerDecorations.Add($Decoration) }
            }
        }
    }

    $ModelSections = [Collections.Generic.List[string]]::new()
    $Models = [Collections.Generic.List[object]]::new()
    $HardwareIds = [Collections.Generic.List[string]]::new()
    foreach ($Entry in $Sections.GetEnumerator()) {
        if ($Entry.Key -cne 'models' -and -not $Entry.Key.StartsWith('models.', [StringComparison]::Ordinal)) {
            continue
        }
        $ModelSections.Add($Entry.Key)
        $SectionHardwareIds = [Collections.Generic.List[string]]::new()
        foreach ($RawLine in ($Entry.Value -split '\r?\n')) {
            $Line = ($RawLine -replace ';.*$', '').Trim()
            if ([string]::IsNullOrWhiteSpace($Line)) { continue }
            if ($Line -notmatch '^[^=]+=(.*)$') { throw "Invalid INF model line: $Line" }
            $Parts = @($Matches[1].Split(',') | ForEach-Object { $_.Trim() })
            if ($Parts.Count -lt 2) { throw "INF model has no hardware ID: $Line" }
            foreach ($HardwareId in $Parts[1..($Parts.Count - 1)]) {
                if ([string]::IsNullOrWhiteSpace($HardwareId)) { throw "INF model has an empty hardware ID: $Line" }
                $HardwareIds.Add($HardwareId)
                $SectionHardwareIds.Add($HardwareId)
            }
        }
        $Models.Add([pscustomobject]@{ Section = $Entry.Key; HardwareIds = @($SectionHardwareIds) })
    }

    $Provider = $VersionValues['provider']
    $ResolvedProvider = $Provider
    if ($Provider -match '^%([^%]+)%$') {
        $ProviderKey = $Matches[1].ToLowerInvariant()
        if (-not $StringValues.ContainsKey($ProviderKey)) {
            throw "INF Provider references undefined Strings value: $Provider"
        }
        $ResolvedProvider = $StringValues[$ProviderKey]
    } elseif ($Provider.Length -ge 2 -and $Provider[0] -eq '"' -and $Provider[$Provider.Length - 1] -eq '"') {
        $ResolvedProvider = $Provider.Substring(1, $Provider.Length - 2)
    }

    [pscustomobject]@{
        CatalogFile = $VersionValues['catalogfile']
        Provider = $ResolvedProvider
        DriverVersion = if ($VersionValues['driverver'] -match ',\s*([^,\s]+)\s*$') { $Matches[1] } else { $null }
        ManufacturerModels = @($ManufacturerModels)
        ManufacturerDecorations = @($ManufacturerDecorations)
        ModelSections = @($ModelSections)
        Models = @($Models)
        HardwareIds = @($HardwareIds)
    }
}

function Assert-PeMachine {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][ValidateSet('x64', 'arm64')][string]$Architecture
    )

    $Bytes = [IO.File]::ReadAllBytes($Path)
    if ($Bytes.Length -lt 0x40 -or $Bytes[0] -ne 0x4d -or $Bytes[1] -ne 0x5a) {
        throw "Driver payload is not a valid PE file: $Path"
    }
    $PeOffset = [BitConverter]::ToUInt32($Bytes, 0x3c)
    if ($PeOffset -gt ($Bytes.Length - 6) -or
        $Bytes[$PeOffset] -ne 0x50 -or $Bytes[$PeOffset + 1] -ne 0x45 -or
        $Bytes[$PeOffset + 2] -ne 0 -or $Bytes[$PeOffset + 3] -ne 0) {
        throw "Driver payload has an invalid PE header: $Path"
    }
    $Machine = [BitConverter]::ToUInt16($Bytes, $PeOffset + 4)
    $ExpectedMachine = if ($Architecture -ceq 'x64') { 0x8664 } else { 0xaa64 }
    if ($Machine -ne $ExpectedMachine) {
        throw ('Driver payload PE Machine 0x{0:x4} does not match {1}: {2}' -f $Machine, $Architecture, $Path)
    }
}

$ResolvedBundle = (Resolve-Path -LiteralPath $BundleDir).Path
$LockPath = Join-Path $ResolvedBundle 'bundle-lock.json'
if (-not (Test-Path -LiteralPath $LockPath -PathType Leaf)) {
    throw "Driver bundle lock not found: $LockPath"
}
$LockJson = Get-Content -LiteralPath $LockPath -Raw
$Lock = $LockJson | ConvertFrom-Json
Assert-ObjectProperties -Value $Lock `
    -Allowed @('schema_version', 'production_ready', 'bundle_version', 'provider', 'minimum_agent_version', 'maximum_agent_version_exclusive', 'architectures') `
    -Required @('schema_version', 'production_ready', 'bundle_version', 'provider', 'minimum_agent_version', 'architectures') `
    -Context 'bundle lock'
Assert-ObjectProperties -Value $Lock.architectures `
    -Allowed @('x64', 'arm64') -Required @() -Context 'architectures'

if ($Lock.schema_version -ne 1) { throw 'Unsupported driver bundle schema_version' }
if ($Lock.production_ready -ne $true) {
    throw 'Driver bundle is production_ready=false; development scaffold packages cannot be embedded'
}
if ($Lock.provider -cne 'Operation Monitoring') {
    throw 'Driver bundle provider must be exactly Operation Monitoring'
}
if ($Lock.bundle_version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') {
    throw 'Driver bundle_version must be a stable numeric SemVer'
}

if ([string]::IsNullOrWhiteSpace($AgentVersion)) {
    $CargoToml = Get-Content (Join-Path $PSScriptRoot '../Cargo.toml') -Raw
    if ($CargoToml -notmatch '(?m)^version = "([^"]+)"') {
        throw 'Unable to determine Agent version from Cargo.toml'
    }
    $AgentVersion = $Matches[1]
}
foreach ($VersionValue in @($AgentVersion, "$($Lock.minimum_agent_version)")) {
    if ($VersionValue -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') {
        throw "Agent compatibility versions must be stable numeric SemVer: $VersionValue"
    }
}
$ParsedAgentVersion = [version]$AgentVersion
if ($ParsedAgentVersion -lt [version]$Lock.minimum_agent_version) {
    throw "Driver bundle requires Agent $($Lock.minimum_agent_version) or newer"
}
$MaximumVersionProperty = $Lock.PSObject.Properties['maximum_agent_version_exclusive']
if ($null -ne $MaximumVersionProperty -and
    $null -ne $MaximumVersionProperty.Value -and
    -not [string]::IsNullOrWhiteSpace("$($MaximumVersionProperty.Value)")) {
    if ("$($MaximumVersionProperty.Value)" -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') {
        throw 'maximum_agent_version_exclusive must be a stable numeric SemVer'
    }
    if ($ParsedAgentVersion -ge [version]$MaximumVersionProperty.Value) {
        throw "Driver bundle is not compatible with Agent $AgentVersion"
    }
}

$ArchitectureEntry = Get-ArchitectureEntry -Architectures $Lock.architectures -Name $Architecture
Assert-ObjectProperties -Value $ArchitectureEntry -Allowed @('packages') -Required @('packages') -Context "$Architecture architecture"
$Packages = @($ArchitectureEntry.packages)
if ($Packages.Count -ne 2) { throw "$Architecture bundle must contain exactly two packages" }
$Kinds = @($Packages | ForEach-Object { $_.kind } | Sort-Object -Unique)
if ($Kinds.Count -ne 2 -or $Kinds[0] -cne 'audio' -or $Kinds[1] -cne 'display') {
    throw "$Architecture bundle must contain exactly one audio and one display package"
}

$SeenPaths = @{}
$VerifiedPackages = [Collections.Generic.List[object]]::new()
foreach ($Package in $Packages) {
    Assert-ObjectProperties -Value $Package `
        -Allowed @('kind', 'driver_version', 'hardware_id', 'catalog_path', 'files') `
        -Required @('kind', 'driver_version', 'hardware_id', 'catalog_path', 'files') `
        -Context "$Architecture package"
    $ExpectedHardwareId = if ($Package.kind -ceq 'display') {
        'ROOT\OMVIRTUALDISPLAY'
    } elseif ($Package.kind -ceq 'audio') {
        'ROOT\OMVIRTUALAUDIO'
    } else {
        throw "Unsupported package kind: $($Package.kind)"
    }
    $ExpectedInfName = if ($Package.kind -ceq 'display') { 'OmVirtualDisplay.inf' } else { 'OmVirtualAudio.inf' }
    $ExpectedPayloadName = if ($Package.kind -ceq 'display') { 'OmVirtualDisplay.dll' } else { 'OmVirtualAudio.sys' }
    if (-not "$($Package.hardware_id)".Equals($ExpectedHardwareId, [StringComparison]::OrdinalIgnoreCase)) {
        throw "$($Package.kind) hardware_id must be $ExpectedHardwareId"
    }
    if ("$($Package.driver_version)" -notmatch '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$') {
        throw "$($Package.kind) driver_version must have four numeric components"
    }
    if (-not (Test-NormalizedRelativePath "$($Package.catalog_path)") -or
        -not "$($Package.catalog_path)".EndsWith('.cat', [StringComparison]::OrdinalIgnoreCase)) {
        throw "$($Package.kind) catalog_path is invalid"
    }

    $Files = @($Package.files)
    if ($Files.Count -lt 3) { throw "$($Package.kind) package must contain INF, catalog, and driver payload" }
    $InfCount = 0
    $HasPayload = $false
    $HasCatalog = $false
    $CatalogPath = $null
    $CatalogMembers = [Collections.Generic.List[string]]::new()
    foreach ($File in $Files) {
        Assert-ObjectProperties -Value $File -Allowed @('path', 'sha256') -Required @('path', 'sha256') -Context "$($Package.kind) file"
        $RelativePath = "$($File.path)"
        $ExpectedPrefix = "$Architecture/$($Package.kind)/"
        if (-not $RelativePath.StartsWith($ExpectedPrefix, [StringComparison]::Ordinal)) {
            throw "$($Package.kind) package file must be under ${ExpectedPrefix}: $RelativePath"
        }
        $Key = $RelativePath.ToLowerInvariant()
        if ($SeenPaths.ContainsKey($Key)) { throw "Bundle file is listed more than once: $RelativePath" }
        $SeenPaths[$Key] = $true
        if ("$($File.sha256)" -notmatch '^[0-9A-Fa-f]{64}$') {
            throw "Invalid SHA-256 for $RelativePath"
        }
        $FilePath = Resolve-BundleFile -Root $ResolvedBundle -RelativePath $RelativePath
        if (-not (Test-Path -LiteralPath $FilePath -PathType Leaf)) { throw "Bundle file not found: $RelativePath" }
        $ActualHash = (Get-FileHash -LiteralPath $FilePath -Algorithm SHA256).Hash
        if (-not $ActualHash.Equals("$($File.sha256)", [StringComparison]::OrdinalIgnoreCase)) {
            throw "SHA-256 mismatch for $RelativePath"
        }
        $Extension = [IO.Path]::GetExtension($RelativePath)
        if ($Extension.Equals('.inf', [StringComparison]::OrdinalIgnoreCase)) {
            $InfCount += 1
            if ([IO.Path]::GetFileName($RelativePath) -cne $ExpectedInfName) {
                throw "$($Package.kind) package INF must retain its original file name $ExpectedInfName"
            }
            $CatalogMembers.Add($FilePath)
        }
        if ($Extension.Equals('.sys', [StringComparison]::OrdinalIgnoreCase) -or
            $Extension.Equals('.dll', [StringComparison]::OrdinalIgnoreCase)) {
            Assert-PeMachine -Path $FilePath -Architecture $Architecture
            $HasPayload = $HasPayload -or ([IO.Path]::GetFileName($RelativePath) -ceq $ExpectedPayloadName)
            $CatalogMembers.Add($FilePath)
        }
        if ($RelativePath.Equals("$($Package.catalog_path)", [StringComparison]::OrdinalIgnoreCase)) {
            $HasCatalog = $true
            $CatalogPath = $FilePath
        }
        if ($Extension.Equals('.inf', [StringComparison]::OrdinalIgnoreCase)) {
            $InfText = Get-Content -LiteralPath $FilePath -Raw
            $Ownership = Get-InfOwnership -Text $InfText
            $CatalogName = [IO.Path]::GetFileName("$($Package.catalog_path)")
            if (-not "$($Ownership.CatalogFile)".Equals($CatalogName, [StringComparison]::OrdinalIgnoreCase) -or
                "$($Ownership.Provider)" -cne 'Operation Monitoring' -or
                "$($Ownership.DriverVersion)" -cne "$($Package.driver_version)" -or
                $Ownership.ManufacturerModels.Count -eq 0 -or
                @($Ownership.ManufacturerModels | Where-Object { $_ -cne 'Models' }).Count -ne 0 -or
                $Ownership.HardwareIds.Count -eq 0 -or
                @($Ownership.HardwareIds | Where-Object { -not $_.Equals($ExpectedHardwareId, [StringComparison]::OrdinalIgnoreCase) }).Count -ne 0) {
                throw "$RelativePath does not bind the declared provider, hardware ID, and catalog"
            }
            $ExpectedDecoration = if ($Architecture -ceq 'x64') { 'NTamd64' } else { 'NTarm64' }
            $MatchingDecorations = @($Ownership.ManufacturerDecorations | Where-Object {
                $_.Equals($ExpectedDecoration, [StringComparison]::OrdinalIgnoreCase) -or
                $_.StartsWith("$ExpectedDecoration.", [StringComparison]::OrdinalIgnoreCase)
            })
            $MatchingModels = @($Ownership.Models | Where-Object {
                $_.Section.Equals("models.$ExpectedDecoration", [StringComparison]::OrdinalIgnoreCase) -or
                $_.Section.StartsWith("models.$ExpectedDecoration.", [StringComparison]::OrdinalIgnoreCase)
            })
            if ($MatchingDecorations.Count -eq 0 -or $MatchingModels.Count -eq 0 -or
                @($MatchingModels | Where-Object {
                    $_.HardwareIds.Count -eq 0 -or
                    @($_.HardwareIds | Where-Object { -not $_.Equals($ExpectedHardwareId, [StringComparison]::OrdinalIgnoreCase) }).Count -ne 0
                }).Count -ne 0) {
                throw "$RelativePath does not declare decorated Models for $Architecture"
            }
            foreach ($Decoration in $Ownership.ManufacturerDecorations) {
                $ExpectedSection = "models.$Decoration"
                if (@($Ownership.ModelSections | Where-Object { $_.Equals($ExpectedSection, [StringComparison]::OrdinalIgnoreCase) }).Count -eq 0) {
                    throw "$RelativePath Manufacturer references missing section $ExpectedSection"
                }
            }
        }
    }
    if ($InfCount -ne 1 -or -not $HasPayload -or -not $HasCatalog) {
        throw "$($Package.kind) package must include exactly one INF, its declared catalog, and $ExpectedPayloadName"
    }
    $VerifiedPackages.Add([pscustomobject]@{ Catalog = $CatalogPath; Members = @($CatalogMembers) })
}

if ([string]::IsNullOrWhiteSpace($SigntoolPath)) {
    $SigntoolPath = if ([string]::IsNullOrWhiteSpace($env:OM_SIGNTOOL_PATH)) {
        'signtool.exe'
    } else {
        $env:OM_SIGNTOOL_PATH
    }
}
Get-Command $SigntoolPath -ErrorAction Stop | Out-Null
foreach ($VerifiedPackage in $VerifiedPackages) {
    Invoke-CheckedNative -FilePath $SigntoolPath -Arguments @('verify', '/kp', '/all', '/v', $VerifiedPackage.Catalog)
    foreach ($Member in $VerifiedPackage.Members) {
        Invoke-CheckedNative -FilePath $SigntoolPath -Arguments @('verify', '/kp', '/v', '/c', $VerifiedPackage.Catalog, $Member)
    }
}

[pscustomobject]@{
    BundleVersion = "$($Lock.bundle_version)"
    Architecture = $Architecture
    LockSha256 = (Get-FileHash -LiteralPath $LockPath -Algorithm SHA256).Hash.ToLowerInvariant()
    FilesVerified = $SeenPaths.Count
    CatalogsVerified = $VerifiedPackages.Count
}
