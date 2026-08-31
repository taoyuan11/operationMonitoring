use std::{fs, path::Path};

#[test]
fn signtool_verify_does_not_use_sign_only_certificate_selector() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut verification_count = 0;

    let build_script = fs::read_to_string(root.join("build.rs")).expect("failed to read build.rs");
    verification_count +=
        assert_verify_commands_exclude_sha1("build.rs", rust_string_literals(&build_script));

    let scripts = root.join("scripts");
    for entry in fs::read_dir(&scripts).expect("failed to read scripts directory") {
        let path = entry.expect("failed to read script entry").path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("ps1") {
            continue;
        }
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        verification_count += assert_verify_commands_exclude_sha1(
            path.file_name()
                .and_then(|name| name.to_str())
                .expect("PowerShell script name is not UTF-8"),
            powershell_string_literals(&source),
        );
    }

    assert!(
        verification_count >= 6,
        "expected to inspect every Windows signature verification command"
    );
}

fn assert_verify_commands_exclude_sha1(file: &str, literals: Vec<String>) -> usize {
    let mut count = 0;
    let mut inspecting_verify = false;
    for literal in literals {
        match literal.as_str() {
            "verify" => {
                count += 1;
                inspecting_verify = true;
            }
            "sign" => inspecting_verify = false,
            "/sha1" if inspecting_verify => {
                panic!("{file}: signtool verify must not use the sign-only /sha1 option")
            }
            _ => {}
        }
    }
    count
}

fn rust_string_literals(source: &str) -> Vec<String> {
    quoted_literals(source, '"', true)
}

fn powershell_string_literals(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|line| line.split('#').next())
        .flat_map(|line| quoted_literals(line, '\'', false))
        .collect()
}

fn quoted_literals(source: &str, quote: char, backslash_escapes: bool) -> Vec<String> {
    let mut literals = Vec::new();
    let mut characters = source.chars().peekable();
    while let Some(character) = characters.next() {
        if character != quote {
            continue;
        }
        let mut literal = String::new();
        while let Some(character) = characters.next() {
            if backslash_escapes && character == '\\' {
                if let Some(escaped) = characters.next() {
                    literal.push(escaped);
                }
            } else if character == quote {
                if !backslash_escapes && characters.peek() == Some(&quote) {
                    literal.push(quote);
                    characters.next();
                } else {
                    break;
                }
            } else {
                literal.push(character);
            }
        }
        literals.push(literal);
    }
    literals
}
