# Windows virtual-device drivers

This directory is the source and packaging scaffold for the Operation Monitoring
headless Windows fallback devices:

- `OmVirtualDisplay` is a UMDF 2 / IddCx indirect display adapter. It declares
  one EDID-less monitor with 1024x768, 1280x720, 1600x900, and 1920x1080 at
  60 Hz; 1920x1080 is preferred.
- `OmVirtualAudio` is a KMDF/PortCls WaveRT driver exposing one render-only
  speaker endpoint fixed at 48 kHz, 16-bit stereo PCM.

## Important release boundary

The source implementations are suitable for WDK/VM development, not an
assertion that they have passed HLK. The audio driver is a render-only
adaptation of Microsoft's `audio/simpleaudiosample`; it registers only the
speaker topology and deliberately contains no microphone or capture endpoint.
Its complete MS-PL license and exact upstream commit are recorded under
`virtual-audio/`.

Nothing built from this directory is production-signed. `Inf2Cat` creates an
unsigned catalog. A production package must be submitted through Hardware
Partner Center and the returned Microsoft-signed catalog must pass
`verify-signed-driver-bundle.ps1`, including `signtool verify /kp`. Do not set
`production_ready` in a bundle lock until the implementation, Driver Verifier,
HLK, Secure Boot, and HVCI gates have actually passed.

The adapted IddCx structure follows Microsoft's Windows Driver Samples
`video/IndirectDisplay` sample, which is distributed under the MIT license.
Review the upstream sample and current IddCx documentation when updating the WDK.

## Local WDK build

Use Visual Studio 2022 with the Windows Driver Kit installed:

```powershell
.\scripts\build-windows-drivers.ps1 -Architecture x64
.\scripts\build-windows-drivers.ps1 -Architecture arm64
```

The script first validates that every audio project source exists, the endpoint
table and INF contain no capture registration, and the speaker format remains
48 kHz/16-bit/stereo PCM. It then builds, runs `InfVerif`, creates unsigned
catalogs with `Inf2Cat`, and creates draft CAB files under
`windows-drivers/artifacts/`. HLK and Driver Verifier require dedicated
disposable Windows test VMs; the script records the expected evidence paths but
cannot replace those environment-specific tests.

After Microsoft signing, arrange the returned files using
`bundle-lock.example.json`, calculate every SHA-256, set `production_ready` only
after the release gates pass, and verify each architecture:

```powershell
.\scripts\verify-signed-driver-bundle.ps1 `
  -BundleDir C:\release\om-windows-drivers-1.0.0 `
  -Architecture x64
```

Formal Agent builds opt in with Cargo feature `bundled-windows-drivers` and
`OM_WINDOWS_DRIVER_BUNDLE_DIR`. Ordinary builds remain physical-device-only and
do not need the WDK or a driver bundle.
