param(
    [ValidateSet('x64', 'arm64', 'all')][string]$Architecture = 'all',
    [ValidateSet('Debug', 'Release')][string]$Configuration = 'Release',
    [string]$OutputDirectory,
    [switch]$Submission,
    [string]$HlkPackage,
    [string]$VerifierEvidence
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0

function Invoke-CheckedNative {
    param([string]$FilePath, [string[]]$Arguments)
    Write-Host "Running $([IO.Path]::GetFileName($FilePath)) $($Arguments -join ' ')"
    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$FilePath exited with status $LASTEXITCODE" }
}

function Find-MSBuild {
    $Command = Get-Command msbuild.exe -ErrorAction SilentlyContinue
    if ($null -ne $Command) { return $Command.Source }
    $VsWhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path -LiteralPath $VsWhere -PathType Leaf)) { throw 'MSBuild not found; install Visual Studio 2022 with the WDK workload' }
    $Path = & $VsWhere -latest -products * -requires Microsoft.Component.MSBuild -find 'MSBuild\**\Bin\amd64\MSBuild.exe' | Select-Object -First 1
    if ([string]::IsNullOrWhiteSpace($Path)) {
        $Path = & $VsWhere -latest -products * -requires Microsoft.Component.MSBuild -find 'MSBuild\**\Bin\MSBuild.exe' | Select-Object -First 1
    }
    if ([string]::IsNullOrWhiteSpace($Path)) { throw 'MSBuild not found through vswhere' }
    return $Path
}

function Find-WdkTool {
    param([string]$Name)
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
    if ([string]::IsNullOrWhiteSpace($Candidate)) { throw "$Name not found in the Windows Driver Kit" }
    return $Candidate
}

function Copy-DriverPackage {
    param([string]$Platform, [string]$NativeArch, [string]$OutputRoot)

    $BuildRoot = Join-Path $DriverRoot "artifacts\$Platform\$Configuration"
    $ArchRoot = Join-Path $OutputRoot $NativeArch
    $DisplayRoot = Join-Path $ArchRoot 'display'
    $AudioRoot = Join-Path $ArchRoot 'audio'
    New-Item -ItemType Directory -Force -Path $DisplayRoot, $AudioRoot | Out-Null

    Copy-Item -LiteralPath (Join-Path $BuildRoot 'OmVirtualDisplay\OmVirtualDisplay.inf') -Destination $DisplayRoot -Force
    Copy-Item -LiteralPath (Join-Path $BuildRoot 'OmVirtualDisplay\OmVirtualDisplay.dll') -Destination $DisplayRoot -Force
    Copy-Item -LiteralPath (Join-Path $BuildRoot 'OmVirtualAudio\OmVirtualAudio.inf') -Destination $AudioRoot -Force
    Copy-Item -LiteralPath (Join-Path $BuildRoot 'OmVirtualAudio\OmVirtualAudio.sys') -Destination $AudioRoot -Force

    foreach ($PackageRoot in @($DisplayRoot, $AudioRoot)) {
        $InfFiles = @(Get-ChildItem -LiteralPath $PackageRoot -Filter '*.inf')
        if ($InfFiles.Count -ne 1) { throw "Expected exactly one INF in $PackageRoot" }
        $Inf = $InfFiles[0]
        Invoke-CheckedNative -FilePath $InfVerif -Arguments @('/v', $Inf.FullName)
        $OsTargets = if ($NativeArch -eq 'x64') { '10_X64,Server10_X64' } else { '10_ARM64,Server10_ARM64' }
        Invoke-CheckedNative -FilePath $Inf2Cat -Arguments @("/driver:$PackageRoot", "/os:$OsTargets", '/uselocaltime')
    }

    $DraftArchitectures = [ordered]@{}
    $DraftArchitectures[$NativeArch] = [ordered]@{
        packages = @(
            New-DraftPackage -Kind display -Root $OutputRoot -Arch $NativeArch -PackageRoot $DisplayRoot -PayloadName 'OmVirtualDisplay.dll'
            New-DraftPackage -Kind audio -Root $OutputRoot -Arch $NativeArch -PackageRoot $AudioRoot -PayloadName 'OmVirtualAudio.sys'
        )
    }
    $DraftLock = [ordered]@{
        schema_version = 1
        production_ready = $false
        bundle_version = '1.0.0'
        provider = 'Operation Monitoring'
        minimum_agent_version = '0.1.23'
        maximum_agent_version_exclusive = $null
        architectures = $DraftArchitectures
    }
    $DraftLock | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $OutputRoot "bundle-lock.$NativeArch.draft.json") -Encoding utf8
    New-SubmissionCab -ArchRoot $ArchRoot -OutputRoot $OutputRoot -Arch $NativeArch
}

function New-DraftPackage {
    param([string]$Kind, [string]$Root, [string]$Arch, [string]$PackageRoot, [string]$PayloadName)
    $TitleKind = (Get-Culture).TextInfo.ToTitleCase($Kind)
    $BaseName = "OmVirtual$TitleKind"
    $HardwareId = if ($Kind -eq 'display') { 'ROOT\OMVIRTUALDISPLAY' } else { 'ROOT\OMVIRTUALAUDIO' }
    $InfPath = Join-Path $PackageRoot "$BaseName.inf"
    $InfText = Get-Content -LiteralPath $InfPath -Raw
    if ($InfText -notmatch '(?im)^\s*DriverVer\s*=\s*[^,]+,\s*([0-9]+(?:\.[0-9]+){3})\s*$') {
        throw "Unable to read DriverVer from $InfPath"
    }
    $DriverVersion = $Matches[1]
    $Files = @("$BaseName.inf", "$BaseName.cat", $PayloadName) | ForEach-Object {
        $FilePath = Join-Path $PackageRoot $_
        [ordered]@{
            path = "$Arch/$Kind/$_"
            sha256 = (Get-FileHash -LiteralPath $FilePath -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    }
    return [ordered]@{
        kind = $Kind
        driver_version = $DriverVersion
        hardware_id = $HardwareId
        catalog_path = "$Arch/$Kind/$BaseName.cat"
        files = $Files
    }
}

function New-SubmissionCab {
    param([string]$ArchRoot, [string]$OutputRoot, [string]$Arch)
    $DdfPath = Join-Path $OutputRoot "submission-$Arch.ddf"
    $CabName = "om-windows-drivers-$Arch-unsigned.cab"
    $InfName = Join-Path $OutputRoot "submission-$Arch.inf"
    $ReportName = Join-Path $OutputRoot "submission-$Arch.rpt"
    $Lines = @(
        '.OPTION EXPLICIT',
        ".Set CabinetNameTemplate=$CabName",
        ".Set DiskDirectoryTemplate=`"$OutputRoot`"",
        ".Set InfFileName=`"$InfName`"",
        ".Set RptFileName=`"$ReportName`"",
        '.Set CompressionType=MSZIP',
        '.Set Cabinet=on',
        '.Set Compress=on'
    )
    foreach ($Kind in @('display', 'audio')) {
        $Lines += ".Set DestinationDir=$Arch\$Kind"
        Get-ChildItem -LiteralPath (Join-Path $ArchRoot $Kind) -File | Sort-Object Name | ForEach-Object {
            $Lines += "`"$($_.FullName)`""
        }
    }
    $Lines | Set-Content -LiteralPath $DdfPath -Encoding ascii
    Invoke-CheckedNative -FilePath $MakeCab -Arguments @('/F', $DdfPath)
}

function Assert-VirtualAudioProject {
    param([string]$DriverRoot)

    $AudioRoot = Join-Path $DriverRoot 'virtual-audio'
    $ProjectPath = Join-Path $AudioRoot 'OmVirtualAudio.vcxproj'
    [xml]$Project = Get-Content -LiteralPath $ProjectPath -Raw
    $Namespace = [Xml.XmlNamespaceManager]::new($Project.NameTable)
    $Namespace.AddNamespace('msb', 'http://schemas.microsoft.com/developer/msbuild/2003')
    $CompileItems = @($Project.SelectNodes('//msb:ClCompile[@Include]', $Namespace))
    if ($CompileItems.Count -eq 0) { throw 'Virtual audio project has no compiled sources' }
    $ProjectItems = @($CompileItems) + @($Project.SelectNodes('//msb:ClInclude[@Include]', $Namespace))
    foreach ($Item in $ProjectItems) {
        $RelativePath = "$($Item.Include)"
        if ($Item.LocalName -ceq 'ClCompile' -and $RelativePath -match '(?i)(mic|capture)') {
            throw "Virtual audio project must not compile capture source: $RelativePath"
        }
        $SourcePath = Join-Path $AudioRoot $RelativePath
        if (-not (Test-Path -LiteralPath $SourcePath -PathType Leaf)) {
            throw "Virtual audio project references missing source: $RelativePath"
        }
    }
    foreach ($ForbiddenSource in @('ToneGenerator.cpp', 'ToneGenerator.h', 'savedata.cpp', 'savedata.h')) {
        if (Test-Path -LiteralPath (Join-Path $AudioRoot "Source\Utilities\$ForbiddenSource")) {
            throw "Virtual audio source contains forbidden capture/debug component $ForbiddenSource"
        }
    }

    $IncludeRoots = @(
        (Join-Path $AudioRoot 'Source\Main'),
        (Join-Path $AudioRoot 'Source\Inc'),
        (Join-Path $AudioRoot 'Source\Filters'),
        (Join-Path $AudioRoot 'Source\Utilities')
    )
    foreach ($Item in $CompileItems) {
        $SourcePath = Join-Path $AudioRoot "$($Item.Include)"
        $Source = Get-Content -LiteralPath $SourcePath -Raw
        foreach ($Match in [regex]::Matches($Source, '(?m)^\s*#include\s+"([^"]+)"')) {
            $Include = $Match.Groups[1].Value
            $Candidates = @((Join-Path ([IO.Path]::GetDirectoryName($SourcePath)) $Include))
            $Candidates += @($IncludeRoots | ForEach-Object { Join-Path $_ $Include })
            if (@($Candidates | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf }).Count -eq 0) {
                throw "$($Item.Include) references missing local include $Include"
            }
        }
    }

    $LicensePath = Join-Path $AudioRoot 'LICENSE-MS-PL.txt'
    $ProvenancePath = Join-Path $AudioRoot 'UPSTREAM.md'
    if (-not (Test-Path -LiteralPath $LicensePath -PathType Leaf) -or
        -not (Get-Content -LiteralPath $LicensePath -Raw).Contains('The Microsoft Public License (MS-PL)')) {
        throw 'Virtual audio source must retain the complete Microsoft Public License'
    }
    if (-not (Test-Path -LiteralPath $ProvenancePath -PathType Leaf) -or
        -not (Get-Content -LiteralPath $ProvenancePath -Raw).Contains('26a27df80772dbcfd69e6449b671d5c29eb5aedc')) {
        throw 'Virtual audio source must retain its exact upstream commit'
    }

    $MiniPairs = Get-Content -LiteralPath (Join-Path $AudioRoot 'Source\Filters\minipairs.h') -Raw
    if ($MiniPairs -match '(?i)(g_CaptureEndpoints|MicArrayMiniports|CreateMicArray)') {
        throw 'Virtual audio endpoint table contains a capture miniport'
    }
    $Definitions = Get-Content -LiteralPath (Join-Path $AudioRoot 'Source\Inc\definitions.h') -Raw
    if ($Definitions -notmatch 'ExAllocatePool2\s*\(' -or $Definitions -notmatch 'ExAllocatePoolWithTag') {
        throw 'Virtual audio source must retain the Windows 10 1809 pool compatibility mapping'
    }
    $WaveTable = Get-Content -LiteralPath (Join-Path $AudioRoot 'Source\Filters\speakerwavtable.h') -Raw
    foreach ($Required in @(
        'SPEAKER_DEVICE_MAX_CHANNELS\s+2',
        'SPEAKER_HOST_MIN_BITS_PER_SAMPLE\s+16',
        'SPEAKER_HOST_MAX_BITS_PER_SAMPLE\s+16',
        'SPEAKER_HOST_MIN_SAMPLE_RATE\s+48000',
        'SPEAKER_HOST_MAX_SAMPLE_RATE\s+48000',
        'KSDATAFORMAT_SUBTYPE_PCM'
    )) {
        if ($WaveTable -notmatch $Required) { throw "Virtual audio fixed render format check failed: $Required" }
    }

    $Inf = Get-Content -LiteralPath (Join-Path $AudioRoot 'OmVirtualAudio.inf') -Raw
    if ($Inf -match '(?i)(KSCATEGORY_CAPTURE|microphone|KSNAME_.*Mic)') {
        throw 'Virtual audio INF must not register a capture or microphone interface'
    }
    foreach ($Required in @('KSCATEGORY_RENDER', 'WaveSpeaker', 'TopologySpeaker', 'ROOT\OMVIRTUALAUDIO')) {
        if (-not $Inf.Contains($Required)) { throw "Virtual audio INF is missing $Required" }
    }
}

$AgentRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$DriverRoot = Join-Path $AgentRoot 'windows-drivers'
$Solution = Join-Path $DriverRoot 'OmWindowsVirtualDevices.sln'
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path $DriverRoot 'artifacts\packages'
}
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
$OutputDirectory = (Resolve-Path -LiteralPath $OutputDirectory).Path

Assert-VirtualAudioProject -DriverRoot $DriverRoot

if ($Submission) {
    if ([string]::IsNullOrWhiteSpace($HlkPackage) -or -not (Test-Path -LiteralPath $HlkPackage -PathType Leaf)) {
        throw 'Submission builds require -HlkPackage pointing to the signed HLKX result from the test controller'
    }
    if ([string]::IsNullOrWhiteSpace($VerifierEvidence) -or -not (Test-Path -LiteralPath $VerifierEvidence -PathType Leaf)) {
        throw 'Submission builds require -VerifierEvidence from a disposable VM Driver Verifier run'
    }
}

$MSBuild = Find-MSBuild
$InfVerif = Find-WdkTool 'InfVerif.exe'
$Inf2Cat = Find-WdkTool 'Inf2Cat.exe'
$MakeCabCommand = Get-Command makecab.exe -ErrorAction SilentlyContinue
$MakeCab = if ($null -ne $MakeCabCommand) { $MakeCabCommand.Source } else { Find-WdkTool 'makecab.exe' }
$Platforms = if ($Architecture -eq 'all') {
    @([pscustomobject]@{ Platform = 'x64'; NativeArch = 'x64' }, [pscustomobject]@{ Platform = 'ARM64'; NativeArch = 'arm64' })
} elseif ($Architecture -eq 'x64') {
    @([pscustomobject]@{ Platform = 'x64'; NativeArch = 'x64' })
} else {
    @([pscustomobject]@{ Platform = 'ARM64'; NativeArch = 'arm64' })
}

foreach ($Target in $Platforms) {
    Invoke-CheckedNative -FilePath $MSBuild -Arguments @(
        $Solution,
        '/m',
        '/nr:false',
        '/t:Build',
        "/p:Configuration=$Configuration",
        "/p:Platform=$($Target.Platform)",
        '/p:Inf2CatUseLocalTime=true',
        '/p:SignMode=Off'
    )
    Copy-DriverPackage -Platform $Target.Platform -NativeArch $Target.NativeArch -OutputRoot $OutputDirectory
}

Write-Warning 'The generated catalogs and CABs are unsigned development artifacts.'
Write-Host "Driver artifacts: $OutputDirectory"
