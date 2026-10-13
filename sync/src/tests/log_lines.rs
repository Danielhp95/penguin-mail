//! No log line carries an address, a password or a token. Logs end up in
//! the journal and in bug reports, so a line names an account by its id.
//! This reads every log macro in the workspace's own code, outside test
//! modules and `tests/` folders, and fails on one whose fields name any of
//! those. The message text is left out of the check, because the words
//! "refresh token" in a message reveal nothing.

use std::path::{Path, PathBuf};

const MACROS: [&str; 5] = ["trace!(", "debug!(", "info!(", "warn!(", "error!("];
const FORBIDDEN: [&str; 4] = ["email", "password", "token", "secret"];
const SKIPPED: [&str; 6] = ["target", "tests", "packaging", "node_modules", ".git", ".claude"];

#[test]
fn no_log_line_names_an_address_a_password_or_a_token() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    assert!(files.len() > 100, "found only {} source files under {}", files.len(), root.display());
    let found: Vec<String> = files
        .iter()
        .filter_map(|file| Some((file, std::fs::read_to_string(file).ok()?)))
        .flat_map(|(file, text)| offending(&text, &file.strip_prefix(&root).unwrap_or(file).display().to_string()))
        .collect();
    assert!(found.is_empty(), "log lines that name a secret or an address:\n{}", found.join("\n"));
}

#[test]
fn a_field_naming_an_address_is_found_and_the_message_is_not() {
    let text = "fn a() {\n    tracing::warn!(account = id, \"lost the refresh token\");\n    \
                tracing::info!(\n        account = %account.email,\n        \"no client\"\n    );\n}\n";
    assert_eq!(offending(text, "a.rs"), ["a.rs:3: info!( account = %account.email, \"\" )"]);
}

#[test]
fn a_test_module_is_skipped_and_the_code_after_it_is_not() {
    let text = "#[cfg(test)]\nmod tests {\n    fn b() { tracing::info!(%email, \"{}\", '}'); }\n}\n\
                fn c() { tracing::debug!(password = %p, \"é\"); }\n";
    assert_eq!(offending(text, "c.rs"), ["c.rs:5: debug!(password = %p, \"\")"]);
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if !SKIPPED.contains(&name.as_ref()) {
                rust_files(&path, out);
            }
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

/// The log calls in `text` outside its test modules whose fields name a
/// forbidden word, each as `shown:line: call` with the message emptied.
fn offending(text: &str, shown: &str) -> Vec<String> {
    let product = without_test_modules(text);
    let mut found = Vec::new();
    for name in MACROS {
        for (start, _) in product.match_indices(name) {
            let before = product[..start].chars().next_back();
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let open = start + name.len() - 1;
            let fields = without_strings(&product[open..closing(product.as_bytes(), open, b'(', b')')]);
            if FORBIDDEN.iter().any(|word| fields.to_lowercase().contains(word)) {
                let line = product[..start].matches('\n').count() + 1;
                let call = format!("{}{fields}", name.trim_end_matches('('));
                found.push(format!("{shown}:{line}: {}", call.split_whitespace().collect::<Vec<_>>().join(" ")));
            }
        }
    }
    found
}

/// `text` with each inline `#[cfg(test)] mod name { ... }` turned to
/// spaces. Newlines stay, so line numbers still match, and the byte length
/// stays, so every index into `text` holds. A file can hold product code
/// after a test module, so this does not stop at the first one.
fn without_test_modules(text: &str) -> String {
    let mut bytes = text.as_bytes().to_vec();
    for (start, marker) in text.match_indices("#[cfg(test)]\nmod ") {
        let header = start + marker.len();
        let Some(brace) = text[header..].find(['{', ';']).map(|i| header + i) else { continue };
        if bytes[brace] != b'{' {
            continue;
        }
        let end = closing(text.as_bytes(), brace, b'{', b'}');
        for byte in &mut bytes[start..end] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    // Only whole multi-byte characters were blanked, byte by byte, into
    // ASCII spaces, so the result is still UTF-8.
    String::from_utf8(bytes).unwrap_or_default()
}

/// The index just past the bracket that closes the one at `open`, counting
/// only brackets outside string and character literals.
fn closing(bytes: &[u8], open: usize, left: u8, right: u8) -> usize {
    let mut depth = 0;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => i = past_string(bytes, i),
            b'\'' if bytes.get(i + 2) == Some(&b'\'') => i += 2,
            b'\'' if bytes.get(i + 1) == Some(&b'\\') => i += 3,
            b if b == left => depth += 1,
            b if b == right => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}

/// The index of the quote that closes the string opening at `open`.
fn past_string(bytes: &[u8], open: usize) -> usize {
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'"' => return i,
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}

/// `call` with the contents of its string literals dropped.
fn without_strings(call: &str) -> String {
    let bytes = call.as_bytes();
    let mut out = String::with_capacity(call.len());
    let mut from = 0;
    while let Some(at) = call[from..].find('"') {
        let open = from + at;
        out.push_str(&call[from..open]);
        out.push_str("\"\"");
        from = (past_string(bytes, open) + 1).min(call.len());
    }
    out.push_str(&call[from..]);
    out
}
