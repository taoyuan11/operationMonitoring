use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::VerifyingKey;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
};

const PUBLIC_KEY_ENV: &str = "OM_UPDATE_PUBLIC_KEY";
const KEY_ID_ENV: &str = "OM_UPDATE_PUBLIC_KEY_ID";
const DRIVER_BUNDLE_ENV: &str = "OM_WINDOWS_DRIVER_BUNDLE_DIR";
const TEST_DRIVER_BUNDLE_ENV: &str = "OM_WINDOWS_TEST_DRIVER_BUNDLE_DIR";
const TEST_SIGNING_CERTIFICATE_ENV: &str = "OM_WINDOWS_TEST_SIGNING_CERTIFICATE_SHA1";
const SIGNTOOL_ENV: &str = "OM_SIGNTOOL_PATH";
const DRIVER_PROVIDER: &str = "Operation Monitoring";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DriverBundleMode {
    Production,
    Test,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DriverBundleLock {
    schema_version: u32,
    production_ready: bool,
    bundle_version: String,
    provider: String,
    minimum_agent_version: String,
    maximum_agent_version_exclusive: Option<String>,
    architectures: BTreeMap<String, DriverArchitecture>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DriverArchitecture {
    packages: Vec<DriverPackage>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DriverPackage {
    kind: String,
    driver_version: String,
    hardware_id: String,
    catalog_path: String,
    files: Vec<DriverFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DriverFile {
    path: String,
    sha256: String,
}

struct EmbeddedDriverPackage {
    kind: String,
    driver_version: String,
    hardware_id: String,
    catalog_path: String,
}

struct EmbeddedDriverFile {
    package: String,
    kind: String,
    relative_path: String,
    sha256: String,
    copied_path: PathBuf,
}

fn main() {
    println!("cargo:rerun-if-env-changed={PUBLIC_KEY_ENV}");
    println!("cargo:rerun-if-env-changed={KEY_ID_ENV}");
    println!("cargo:rerun-if-env-changed={DRIVER_BUNDLE_ENV}");
    println!("cargo:rerun-if-env-changed={TEST_DRIVER_BUNDLE_ENV}");
    println!("cargo:rerun-if-env-changed={TEST_SIGNING_CERTIFICATE_ENV}");
    println!("cargo:rerun-if-env-changed={SIGNTOOL_ENV}");

    let public_key = std::env::var(PUBLIC_KEY_ENV).ok();
    let key_id = std::env::var(KEY_ID_ENV).ok();
    match (public_key, key_id) {
        (None, None) => {}
        (Some(public_key), Some(key_id)) => validate_update_key(&public_key, &key_id),
        _ => panic!("{PUBLIC_KEY_ENV} and {KEY_ID_ENV} must be set together"),
    }

    generate_windows_driver_assets();
}

fn validate_update_key(encoded_key: &str, key_id: &str) {
    let key_id = key_id.trim();
    assert!(
        !key_id.is_empty()
            && key_id.len() <= 64
            && key_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        "{KEY_ID_ENV} is invalid"
    );

    let key_bytes = STANDARD
        .decode(encoded_key.trim())
        .unwrap_or_else(|_| panic!("{PUBLIC_KEY_ENV} must be valid Base64"));
    let key_bytes: [u8; 32] = key_bytes
        .try_into()
        .unwrap_or_else(|_| panic!("{PUBLIC_KEY_ENV} must decode to 32 bytes"));
    VerifyingKey::from_bytes(&key_bytes)
        .unwrap_or_else(|_| panic!("{PUBLIC_KEY_ENV} must be a valid Ed25519 public key"));
}

fn generate_windows_driver_assets() {
    let output_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is not set"));
    let generated_path = output_dir.join("windows_driver_assets.rs");

    let production_feature = std::env::var_os("CARGO_FEATURE_BUNDLED_WINDOWS_DRIVERS").is_some();
    let test_feature = std::env::var_os("CARGO_FEATURE_BUNDLED_WINDOWS_TEST_DRIVERS").is_some();
    assert!(
        !(production_feature && test_feature),
        "bundled-windows-drivers and bundled-windows-test-drivers are mutually exclusive"
    );
    let mode = match (production_feature, test_feature) {
        (false, false) => {
            fs::write(generated_path, empty_windows_driver_assets())
                .expect("failed to generate empty Windows driver assets module");
            return;
        }
        (true, false) => DriverBundleMode::Production,
        (false, true) => DriverBundleMode::Test,
        (true, true) => unreachable!(),
    };
    let feature_name = match mode {
        DriverBundleMode::Production => "bundled-windows-drivers",
        DriverBundleMode::Test => "bundled-windows-test-drivers",
    };
    let bundle_env = match mode {
        DriverBundleMode::Production => DRIVER_BUNDLE_ENV,
        DriverBundleMode::Test => TEST_DRIVER_BUNDLE_ENV,
    };

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    assert_eq!(
        target_os, "windows",
        "{feature_name} is only supported for Windows targets"
    );
    let architecture = match target_arch.as_str() {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => panic!("{feature_name} only supports Windows x86_64 and aarch64 targets"),
    };
    assert_eq!(
        std::env::consts::OS,
        "windows",
        "{feature_name} must be built on Windows so signtool can verify the driver catalogs"
    );

    let bundle_dir = PathBuf::from(
        std::env::var_os(bundle_env)
            .unwrap_or_else(|| panic!("{bundle_env} is required by {feature_name}")),
    )
    .canonicalize()
    .unwrap_or_else(|error| panic!("failed to resolve {bundle_env}: {error}"));
    let lock_path = bundle_dir.join("bundle-lock.json");
    println!("cargo:rerun-if-changed={}", lock_path.display());
    let lock_bytes = fs::read(&lock_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", lock_path.display()));
    let lock: DriverBundleLock = serde_json::from_slice(&lock_bytes)
        .unwrap_or_else(|error| panic!("invalid {}: {error}", lock_path.display()));
    validate_bundle_metadata(&lock, architecture, mode);

    let test_signing_certificate = match mode {
        DriverBundleMode::Production => None,
        DriverBundleMode::Test => {
            let thumbprint = std::env::var(TEST_SIGNING_CERTIFICATE_ENV).unwrap_or_else(|_| {
                panic!("{TEST_SIGNING_CERTIFICATE_ENV} is required by bundled-windows-test-drivers")
            });
            validate_certificate_thumbprint(&thumbprint);
            Some(thumbprint)
        }
    };

    let selected = lock
        .architectures
        .get(architecture)
        .expect("validated architecture is missing");
    let embedded_dir = output_dir.join("bundled-windows-drivers");
    fs::create_dir_all(&embedded_dir).expect("failed to create embedded driver output directory");
    let copied_lock = embedded_dir.join("bundle-lock.json");
    fs::write(&copied_lock, &lock_bytes).expect("failed to copy driver bundle lock");

    let mut packages = Vec::new();
    let mut files = Vec::new();
    let mut seen_paths = BTreeSet::new();
    for package in &selected.packages {
        validate_driver_package(package);
        let package_prefix = format!("{architecture}/{}/", package.kind);
        let mut has_catalog = false;
        let mut has_driver_payload = false;
        let mut inf_count = 0_usize;
        let mut catalog_members = Vec::new();
        for file in &package.files {
            validate_relative_path(&file.path);
            assert!(
                file.path.starts_with(&package_prefix),
                "{} package file must be under {package_prefix}: {}",
                package.kind,
                file.path
            );
            assert!(
                seen_paths.insert(file.path.to_ascii_lowercase()),
                "driver bundle file is listed more than once: {}",
                file.path
            );
            validate_sha256(&file.sha256, &file.path);
            let source_path = bundle_dir
                .join(&file.path)
                .canonicalize()
                .unwrap_or_else(|error| {
                    panic!(
                        "failed to resolve driver bundle file {}: {error}",
                        file.path
                    )
                });
            assert!(
                source_path.starts_with(&bundle_dir),
                "driver bundle file resolves outside its root: {}",
                file.path
            );
            println!("cargo:rerun-if-changed={}", source_path.display());
            let contents = fs::read(&source_path).unwrap_or_else(|error| {
                panic!(
                    "failed to read driver bundle file {}: {error}",
                    source_path.display()
                )
            });
            let actual = encode_lower_hex(Sha256::digest(&contents));
            assert!(
                actual.eq_ignore_ascii_case(&file.sha256),
                "SHA-256 mismatch for {}: expected {}, got {actual}",
                file.path,
                file.sha256
            );

            let kind = driver_file_kind(&file.path);
            if kind == "inf" {
                let expected_inf = expected_inf_name(&package.kind);
                assert_eq!(
                    Path::new(&file.path)
                        .file_name()
                        .and_then(|name| name.to_str()),
                    Some(expected_inf),
                    "{} package INF must retain its original file name {expected_inf}",
                    package.kind
                );
                validate_inf_package(&contents, package, architecture);
                catalog_members.push(source_path.clone());
            } else if kind == "driver" {
                validate_pe_machine(&contents, architecture, &file.path);
                has_driver_payload |= Path::new(&file.path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    == Some(expected_driver_payload_name(&package.kind));
                catalog_members.push(source_path.clone());
            }

            let copied_path = embedded_dir.join(&file.path);
            if let Some(parent) = copied_path.parent() {
                fs::create_dir_all(parent).expect("failed to create embedded driver directory");
            }
            fs::write(&copied_path, contents).unwrap_or_else(|error| {
                panic!("failed to copy driver bundle file {}: {error}", file.path)
            });

            has_catalog |= file.path.eq_ignore_ascii_case(&package.catalog_path);
            inf_count += usize::from(kind == "inf");
            files.push(EmbeddedDriverFile {
                package: package.kind.clone(),
                kind: kind.to_string(),
                relative_path: file.path.clone(),
                sha256: file.sha256.to_ascii_lowercase(),
                copied_path,
            });
        }
        assert_eq!(
            inf_count, 1,
            "{} driver package must contain exactly one INF file",
            package.kind
        );
        assert!(
            has_catalog,
            "{} driver catalog_path is not present in files",
            package.kind
        );
        assert!(
            has_driver_payload,
            "{} package must contain its {} driver payload",
            package.kind,
            expected_driver_payload_name(&package.kind)
        );
        let catalog_path = bundle_dir.join(&package.catalog_path);
        verify_catalog_signature(&catalog_path, mode, test_signing_certificate.as_deref());
        for member_path in catalog_members {
            verify_catalog_membership(
                &catalog_path,
                &member_path,
                mode,
                test_signing_certificate.as_deref(),
            );
        }
        packages.push(EmbeddedDriverPackage {
            kind: package.kind.clone(),
            driver_version: package.driver_version.clone(),
            hardware_id: package.hardware_id.clone(),
            catalog_path: package.catalog_path.clone(),
        });
    }

    let lock_sha256 = encode_lower_hex(Sha256::digest(&lock_bytes));
    let generated = render_windows_driver_assets(
        &lock.bundle_version,
        architecture,
        &lock_sha256,
        &copied_lock,
        &packages,
        &files,
    );
    fs::write(generated_path, generated)
        .expect("failed to generate bundled Windows driver assets module");
}

fn encode_lower_hex(bytes: impl AsRef<[u8]>) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    let bytes = bytes.as_ref();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(DIGITS[usize::from(byte >> 4)] as char);
        encoded.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

fn validate_bundle_metadata(lock: &DriverBundleLock, architecture: &str, mode: DriverBundleMode) {
    assert_eq!(lock.schema_version, 1, "unsupported driver bundle schema");
    match mode {
        DriverBundleMode::Production => assert!(
            lock.production_ready,
            "driver bundle is marked production_ready=false; development scaffold packages cannot be embedded by the production feature"
        ),
        DriverBundleMode::Test => assert!(
            !lock.production_ready,
            "the test-only feature refuses production_ready=true bundles"
        ),
    }
    assert_eq!(
        lock.provider, DRIVER_PROVIDER,
        "driver bundle provider must be {DRIVER_PROVIDER}"
    );
    assert!(
        lock.architectures
            .keys()
            .all(|key| matches!(key.as_str(), "x64" | "arm64")),
        "driver bundle contains an unsupported architecture"
    );
    let bundle_version =
        Version::parse(&lock.bundle_version).expect("driver bundle_version must be SemVer");
    assert!(
        bundle_version.pre.is_empty() && bundle_version.build.is_empty(),
        "driver bundle_version must be a stable SemVer"
    );
    let agent_version = Version::parse(env!("CARGO_PKG_VERSION")).expect("invalid agent version");
    assert!(
        agent_version.pre.is_empty() && agent_version.build.is_empty(),
        "bundled driver releases require a stable Agent version"
    );
    let minimum = Version::parse(&lock.minimum_agent_version)
        .expect("driver minimum_agent_version must be SemVer");
    assert!(
        minimum.pre.is_empty() && minimum.build.is_empty(),
        "driver minimum_agent_version must be stable"
    );
    assert!(
        agent_version >= minimum,
        "driver bundle requires agent {minimum} or newer"
    );
    if let Some(maximum) = &lock.maximum_agent_version_exclusive {
        let maximum =
            Version::parse(maximum).expect("driver maximum_agent_version_exclusive must be SemVer");
        assert!(
            maximum.pre.is_empty() && maximum.build.is_empty(),
            "driver maximum_agent_version_exclusive must be stable"
        );
        assert!(
            agent_version < maximum,
            "driver bundle is not compatible with agent {agent_version}"
        );
    }
    let selected = lock.architectures.get(architecture).unwrap_or_else(|| {
        panic!("driver bundle does not contain target architecture {architecture}")
    });
    let kinds = selected
        .packages
        .iter()
        .map(|package| package.kind.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        kinds,
        BTreeSet::from(["audio", "display"]),
        "driver bundle must contain exactly one display and one audio package"
    );
    assert_eq!(
        selected.packages.len(),
        2,
        "driver bundle must contain exactly one display and one audio package"
    );
}

fn validate_driver_package(package: &DriverPackage) {
    let expected_hardware_id = match package.kind.as_str() {
        "display" => r"ROOT\OMVIRTUALDISPLAY",
        "audio" => r"ROOT\OMVIRTUALAUDIO",
        other => panic!("unsupported driver package kind: {other}"),
    };
    assert!(
        package
            .hardware_id
            .eq_ignore_ascii_case(expected_hardware_id),
        "{} package hardware_id must be {expected_hardware_id}",
        package.kind
    );
    assert!(
        is_windows_driver_version(&package.driver_version),
        "{} driver_version must have four numeric components",
        package.kind
    );
    validate_relative_path(&package.catalog_path);
    assert!(
        package.catalog_path.to_ascii_lowercase().ends_with(".cat"),
        "{} catalog_path must name a .cat file",
        package.kind
    );
    assert!(
        !package.files.is_empty(),
        "{} package has no files",
        package.kind
    );
}

fn expected_inf_name(package_kind: &str) -> &'static str {
    match package_kind {
        "display" => "OmVirtualDisplay.inf",
        "audio" => "OmVirtualAudio.inf",
        other => panic!("unsupported driver package kind: {other}"),
    }
}

fn expected_driver_payload_name(package_kind: &str) -> &'static str {
    match package_kind {
        "display" => "OmVirtualDisplay.dll",
        "audio" => "OmVirtualAudio.sys",
        other => panic!("unsupported driver package kind: {other}"),
    }
}

fn validate_inf_package(contents: &[u8], package: &DriverPackage, architecture: &str) {
    let text = decode_inf(contents);
    let expected_decoration = match architecture {
        "x64" => "ntamd64",
        "arm64" => "ntarm64",
        other => panic!("unsupported driver architecture: {other}"),
    };
    let mut section = String::new();
    let mut catalog = None;
    let mut provider = None;
    let mut driver_version = None;
    let mut strings = BTreeMap::new();
    let mut manufacturer_models = Vec::new();
    let mut manufacturer_decorations = Vec::new();
    let mut model_sections = BTreeSet::new();
    let mut model_hardware_ids = Vec::new();
    let mut target_model_hardware_ids = Vec::new();

    for raw_line in text.lines() {
        let line = raw_line.split(';').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            section = name.trim().to_ascii_lowercase();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if section == "version" {
            match key.to_ascii_lowercase().as_str() {
                "catalogfile" => catalog = Some(value.to_string()),
                "provider" => provider = Some(value.to_string()),
                "driverver" => {
                    driver_version = value
                        .rsplit_once(',')
                        .map(|(_, version)| version.trim().to_string())
                }
                _ => {}
            }
        } else if section == "manufacturer" {
            let values = value.split(',').map(str::trim).collect::<Vec<_>>();
            if let Some(models) = values.first() {
                manufacturer_models.push(models.to_ascii_lowercase());
            }
            manufacturer_decorations.extend(
                values
                    .iter()
                    .skip(1)
                    .filter(|value| !value.is_empty())
                    .map(|value| value.to_ascii_lowercase()),
            );
        } else if section == "models" || section.starts_with("models.") {
            model_sections.insert(section.clone());
            let values = value.split(',').map(str::trim).collect::<Vec<_>>();
            let hardware_ids = values
                .iter()
                .skip(1)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_ascii_uppercase())
                .collect::<Vec<_>>();
            if section
                .strip_prefix("models.")
                .is_some_and(|decoration| inf_decoration_matches(decoration, expected_decoration))
            {
                target_model_hardware_ids.extend(hardware_ids.iter().cloned());
            }
            model_hardware_ids.extend(hardware_ids);
        } else if section == "strings" {
            strings.insert(key.to_ascii_lowercase(), unquote_inf_string(value));
        }
    }

    let expected_catalog = Path::new(&package.catalog_path)
        .file_name()
        .and_then(|value| value.to_str())
        .expect("validated catalog path has no file name");
    assert!(
        catalog.is_some_and(|value| value.eq_ignore_ascii_case(expected_catalog)),
        "{} INF CatalogFile does not match {}",
        package.kind,
        package.catalog_path
    );
    let provider = provider.expect("INF Version section must declare Provider");
    let resolved_provider = resolve_inf_string(&provider, &strings).unwrap_or_else(|| {
        panic!(
            "{} INF Provider references an undefined Strings value: {provider}",
            package.kind
        )
    });
    assert_eq!(
        resolved_provider, DRIVER_PROVIDER,
        "{} INF Provider must resolve to {DRIVER_PROVIDER}",
        package.kind
    );
    assert_eq!(
        driver_version.as_deref(),
        Some(package.driver_version.as_str()),
        "{} INF DriverVer does not match bundle lock",
        package.kind
    );
    assert!(
        !manufacturer_models.is_empty()
            && manufacturer_models.iter().all(|models| models == "models"),
        "{} INF Manufacturer must reference only the Models section",
        package.kind
    );
    assert!(
        manufacturer_decorations
            .iter()
            .any(|decoration| inf_decoration_matches(decoration, expected_decoration)),
        "{} INF Manufacturer does not declare a Models decoration for {architecture}",
        package.kind
    );
    assert!(
        model_sections.iter().any(|section| {
            section
                .strip_prefix("models.")
                .is_some_and(|decoration| inf_decoration_matches(decoration, expected_decoration))
        }),
        "{} INF has no Models section for {architecture}",
        package.kind
    );
    assert!(
        manufacturer_decorations
            .iter()
            .all(|decoration| { model_sections.contains(&format!("models.{decoration}")) }),
        "{} INF Manufacturer references a missing decorated Models section",
        package.kind
    );
    assert!(
        !model_hardware_ids.is_empty()
            && model_hardware_ids
                .iter()
                .all(|id| id.eq_ignore_ascii_case(&package.hardware_id)),
        "{} INF Models may bind only {}",
        package.kind,
        package.hardware_id
    );
    assert!(
        !target_model_hardware_ids.is_empty()
            && target_model_hardware_ids
                .iter()
                .all(|id| id.eq_ignore_ascii_case(&package.hardware_id)),
        "{} INF Models for {architecture} must bind {}",
        package.kind,
        package.hardware_id
    );
}

fn unquote_inf_string(value: &str) -> String {
    let value = value.trim();
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
        .to_string()
}

fn resolve_inf_string(value: &str, strings: &BTreeMap<String, String>) -> Option<String> {
    let value = value.trim();
    if let Some(name) = value
        .strip_prefix('%')
        .and_then(|value| value.strip_suffix('%'))
    {
        strings.get(&name.to_ascii_lowercase()).cloned()
    } else {
        Some(unquote_inf_string(value))
    }
}

fn inf_decoration_matches(value: &str, expected: &str) -> bool {
    value == expected || value.starts_with(&format!("{expected}."))
}

fn validate_pe_machine(contents: &[u8], architecture: &str, path: &str) {
    assert!(
        contents.len() >= 0x40 && contents.starts_with(b"MZ"),
        "driver payload is not a valid PE file: {path}"
    );
    let pe_offset = u32::from_le_bytes(
        contents[0x3c..0x40]
            .try_into()
            .expect("validated DOS header length"),
    ) as usize;
    assert!(
        pe_offset
            .checked_add(6)
            .is_some_and(|end| end <= contents.len())
            && contents.get(pe_offset..pe_offset + 4) == Some(b"PE\0\0"),
        "driver payload has an invalid PE header: {path}"
    );
    let machine = u16::from_le_bytes(
        contents[pe_offset + 4..pe_offset + 6]
            .try_into()
            .expect("validated PE header length"),
    );
    let expected_machine = match architecture {
        "x64" => 0x8664,
        "arm64" => 0xaa64,
        other => panic!("unsupported driver architecture: {other}"),
    };
    assert_eq!(
        machine, expected_machine,
        "driver payload PE Machine does not match {architecture}: {path}"
    );
}

fn decode_inf(contents: &[u8]) -> String {
    if let Some(bytes) = contents.strip_prefix(&[0xff, 0xfe]) {
        assert!(bytes.len() % 2 == 0, "INF UTF-16LE payload is truncated");
        let words = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        String::from_utf16(&words).expect("INF is not valid UTF-16LE")
    } else {
        String::from_utf8(contents.to_vec()).expect("INF must be UTF-8 or UTF-16LE")
    }
}

fn is_windows_driver_version(value: &str) -> bool {
    let parts = value.split('.').collect::<Vec<_>>();
    parts.len() == 4
        && parts.iter().all(|part| {
            !part.is_empty()
                && part
                    .parse::<u16>()
                    .is_ok_and(|number| number.to_string() == *part)
        })
}

fn validate_relative_path(value: &str) {
    let path = Path::new(value);
    assert!(
        !value.is_empty()
            && !value.contains('\\')
            && !value.contains(':')
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "driver bundle path must be a normalized forward-slash relative path: {value}"
    );
}

fn validate_sha256(value: &str, path: &str) {
    assert!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid SHA-256 for {path}"
    );
}

fn driver_file_kind(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "inf" => "inf",
        "cat" => "catalog",
        "dll" | "sys" => "driver",
        _ => "support",
    }
}

fn validate_certificate_thumbprint(value: &str) {
    assert!(
        value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{TEST_SIGNING_CERTIFICATE_ENV} must be a 40-digit SHA-1 certificate thumbprint"
    );
}

fn verify_catalog_signature(
    catalog_path: &Path,
    mode: DriverBundleMode,
    test_signing_certificate: Option<&str>,
) {
    let signtool = std::env::var_os(SIGNTOOL_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("signtool.exe"));
    let mut command = Command::new(&signtool);
    command.args(["verify"]);
    match mode {
        DriverBundleMode::Production => {
            command.args(["/kp", "/all", "/v"]);
        }
        DriverBundleMode::Test => {
            command.args([
                "/pa",
                "/all",
                "/v",
                "/sha1",
                test_signing_certificate.expect("test signing certificate is missing"),
            ]);
        }
    }
    let output = command.arg(catalog_path).output().unwrap_or_else(|error| {
        panic!(
            "failed to start {} for {}: {error}",
            signtool.display(),
            catalog_path.display()
        )
    });
    assert!(
        output.status.success(),
        "signtool rejected {} in {mode:?} mode: {}{}",
        catalog_path.display(),
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
}

fn verify_catalog_membership(
    catalog_path: &Path,
    member_path: &Path,
    mode: DriverBundleMode,
    test_signing_certificate: Option<&str>,
) {
    let signtool = std::env::var_os(SIGNTOOL_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("signtool.exe"));
    let mut command = Command::new(&signtool);
    command.args(["verify"]);
    match mode {
        DriverBundleMode::Production => {
            command.args(["/kp", "/v"]);
        }
        DriverBundleMode::Test => {
            command.args([
                "/pa",
                "/v",
                "/sha1",
                test_signing_certificate.expect("test signing certificate is missing"),
            ]);
        }
    }
    let output = command
        .arg("/c")
        .arg(catalog_path)
        .arg(member_path)
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "failed to start {} for catalog member {}: {error}",
                signtool.display(),
                member_path.display()
            )
        });
    assert!(
        output.status.success(),
        "signtool rejected {} as a member of {}: {}{}",
        member_path.display(),
        catalog_path.display(),
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
}

fn empty_windows_driver_assets() -> &'static str {
    r#"// Generated by build.rs. Do not edit.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EmbeddedWindowsDriverFile {
    pub(crate) package: &'static str,
    pub(crate) kind: &'static str,
    pub(crate) relative_path: &'static str,
    pub(crate) sha256: &'static str,
    pub(crate) bytes: &'static [u8],
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EmbeddedWindowsDriverPackage {
    pub(crate) kind: &'static str,
    pub(crate) driver_version: &'static str,
    pub(crate) hardware_id: &'static str,
    pub(crate) catalog_path: &'static str,
}

pub(crate) const BUNDLED: bool = false;
pub(crate) const BUNDLE_VERSION: Option<&str> = None;
pub(crate) const ARCHITECTURE: Option<&str> = None;
pub(crate) const LOCK_SHA256: Option<&str> = None;
pub(crate) const LOCK_BYTES: &[u8] = &[];
pub(crate) const FILES: &[EmbeddedWindowsDriverFile] = &[];
pub(crate) const PACKAGES: &[EmbeddedWindowsDriverPackage] = &[];
"#
}

fn render_windows_driver_assets(
    bundle_version: &str,
    architecture: &str,
    lock_sha256: &str,
    lock_path: &Path,
    packages: &[EmbeddedDriverPackage],
    files: &[EmbeddedDriverFile],
) -> String {
    let mut output = empty_windows_driver_assets()
        .replace(
            "pub(crate) const BUNDLED: bool = false;",
            "pub(crate) const BUNDLED: bool = true;",
        )
        .replace(
            "pub(crate) const BUNDLE_VERSION: Option<&str> = None;",
            &format!("pub(crate) const BUNDLE_VERSION: Option<&str> = Some({bundle_version:?});"),
        )
        .replace(
            "pub(crate) const ARCHITECTURE: Option<&str> = None;",
            &format!("pub(crate) const ARCHITECTURE: Option<&str> = Some({architecture:?});"),
        )
        .replace(
            "pub(crate) const LOCK_SHA256: Option<&str> = None;",
            &format!("pub(crate) const LOCK_SHA256: Option<&str> = Some({lock_sha256:?});"),
        )
        .replace(
            "pub(crate) const LOCK_BYTES: &[u8] = &[];",
            &format!(
                "pub(crate) const LOCK_BYTES: &[u8] = include_bytes!({:?});",
                lock_path.to_string_lossy()
            ),
        );

    let package_values = packages
        .iter()
        .map(|package| {
            format!(
                "    EmbeddedWindowsDriverPackage {{ kind: {:?}, driver_version: {:?}, hardware_id: {:?}, catalog_path: {:?} }},",
                package.kind, package.driver_version, package.hardware_id, package.catalog_path
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    output = output.replace(
        "pub(crate) const PACKAGES: &[EmbeddedWindowsDriverPackage] = &[];",
        &format!(
            "pub(crate) const PACKAGES: &[EmbeddedWindowsDriverPackage] = &[\n{package_values}\n];"
        ),
    );

    let file_values = files
        .iter()
        .map(|file| {
            format!(
                "    EmbeddedWindowsDriverFile {{ package: {:?}, kind: {:?}, relative_path: {:?}, sha256: {:?}, bytes: include_bytes!({:?}) }},",
                file.package,
                file.kind,
                file.relative_path,
                file.sha256,
                file.copied_path.to_string_lossy()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    output.replace(
        "pub(crate) const FILES: &[EmbeddedWindowsDriverFile] = &[];",
        &format!("pub(crate) const FILES: &[EmbeddedWindowsDriverFile] = &[\n{file_values}\n];"),
    )
}
