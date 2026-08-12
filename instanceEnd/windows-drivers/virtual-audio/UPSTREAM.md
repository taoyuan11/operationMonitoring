# Upstream source provenance

The implementation under `Source/` is derived from Microsoft's
`Windows-driver-samples/audio/simpleaudiosample` at commit
`26a27df80772dbcfd69e6449b671d5c29eb5aedc` (2026-08-07).

Upstream repository: <https://github.com/microsoft/Windows-driver-samples>

The source is used under the Microsoft Public License (MS-PL). The complete
license is retained in `LICENSE-MS-PL.txt`, and the copyright notices in the
adapted source files are preserved.

Operation Monitoring adaptations remove the microphone endpoint and capture
miniport registration, retain only the speaker topology, fix its single host
format at 48 kHz/16-bit/stereo PCM, use the product hardware ID and names, and
map the newer pool allocation API to its Windows 10 1809-compatible equivalent.
