//! Every Git newgit starts comes from `newgit_core::git::command()`, which
//! runs it in the C locale so newgit can read its messages. A Git started any
//! other way reads them in the developer's language, and the retries that
//! match them stop running (see `src/git.rs`).
//!
//! Merges cannot catch a new `Command::new("git")`: it is a new line, and
//! new lines merge cleanly. This test reads the source of the three crates
//! and fails on any `Command::new(...)` naming Git outside `src/git.rs` and
//! outside test code. A call that keeps the developer's locale on purpose
//! says why in a `// git-locale:` comment just above it.
//!
//! New tests live in their own file per AGENTS.md's "Working in parallel".

use std::path::{Path, PathBuf};

const CRATES: [&str; 3] = ["newgit", "newgit-core", "newgit-serve"];

/// How many lines above a call its `// git-locale:` reason may start.
const REASON_REACH: usize = 6;

fn rust_files(dir: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_files(&path, into);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            into.push(path);
        }
    }
}

/// Which bytes of `source` are code: not in a comment, string or char.
fn code_mask(source: &[u8]) -> Vec<bool> {
    let mut mask = vec![true; source.len()];
    let mut i = 0;
    while i < source.len() {
        let rest = &source[i..];
        let start = i;
        if rest.starts_with(b"//") {
            while i < source.len() && source[i] != b'\n' {
                i += 1;
            }
        } else if rest.starts_with(b"/*") {
            let mut depth = 0;
            while i < source.len() {
                if source[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if source[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else if (rest.starts_with(b"r#") || rest.starts_with(b"r\"") || rest.starts_with(b"br"))
            && (i == 0 || !(source[i - 1].is_ascii_alphanumeric() || source[i - 1] == b'_'))
        {
            let mut j = i + if rest[0] == b'b' { 2 } else { 1 };
            let hashes = source[j..].iter().take_while(|&&b| b == b'#').count();
            j += hashes;
            if source.get(j) != Some(&b'"') {
                i += 1;
                continue;
            }
            let mut close = vec![b'"'];
            close.extend(std::iter::repeat_n(b'#', hashes));
            j += 1;
            while j < source.len() && !source[j..].starts_with(&close) {
                j += 1;
            }
            i = (j + close.len()).min(source.len());
        } else if rest[0] == b'"' {
            i += 1;
            while i < source.len() && source[i] != b'"' {
                i += if source[i] == b'\\' { 2 } else { 1 };
            }
            i = (i + 1).min(source.len());
        } else if rest[0] == b'\'' && (rest.get(2) == Some(&b'\'') || rest.get(1) == Some(&b'\\')) {
            // A char literal ('{', '\n'), not a lifetime ('a).
            i += 1;
            while i < source.len() && source[i] != b'\'' {
                i += if source[i] == b'\\' { 2 } else { 1 };
            }
            i = (i + 1).min(source.len());
        } else {
            i += 1;
            continue;
        }
        for flag in &mut mask[start..i.min(source.len())] {
            *flag = false;
        }
    }
    mask
}

/// Mark as not code every item `#[cfg(test)]` gates: up to its first `;`,
/// or through the block its first `{` opens.
fn drop_test_items(source: &[u8], mask: &mut [bool]) {
    let marker = b"#[cfg(test)]";
    let mut from = 0;
    while let Some(at) = find(source, mask, marker, from) {
        let mut i = at + marker.len();
        let mut depth = 0usize;
        while i < source.len() {
            if mask[i] {
                match source[i] {
                    b';' if depth == 0 => break,
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        let end = (i + 1).min(source.len());
        for flag in &mut mask[at..end] {
            *flag = false;
        }
        from = end;
    }
}

/// The next `needle` at or after `from` that starts in code.
fn find(source: &[u8], mask: &[bool], needle: &[u8], from: usize) -> Option<usize> {
    (from..source.len().saturating_sub(needle.len() - 1))
        .find(|&i| mask[i] && source[i..].starts_with(needle))
}

/// `(line, call)` for every `Command::new(...)` naming Git in `source`'s
/// non-test code without a `// git-locale:` reason above it.
fn raw_gits(source: &str) -> Vec<(usize, String)> {
    let bytes = source.as_bytes();
    let mut mask = code_mask(bytes);
    drop_test_items(bytes, &mut mask);
    let lines: Vec<&str> = source.lines().collect();
    let needle = b"Command::new(";
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = find(bytes, &mask, needle, from) {
        from = at + needle.len();
        let close = source[from..]
            .find(')')
            .map_or(source.len(), |end| from + end);
        let argument = source[from..close].trim();
        if !argument.to_ascii_lowercase().contains("git") {
            continue;
        }
        let line = source[..at].matches('\n').count();
        let reasoned = lines[line.saturating_sub(REASON_REACH)..line]
            .iter()
            .any(|above| above.trim_start().starts_with("// git-locale:"));
        if !reasoned {
            found.push((line + 1, lines[line].trim().to_owned()));
        }
    }
    found
}

#[test]
fn every_git_newgit_starts_comes_from_git_command() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir");
    let helper = crates.join("newgit-core/src/git.rs");
    let mut files = Vec::new();
    for name in CRATES {
        rust_files(&crates.join(name).join("src"), &mut files);
        files.push(crates.join(name).join("build.rs"));
    }
    let mut scanned = 0;
    let mut misses = Vec::new();
    for file in files {
        let test_only = file
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with("_tests.rs"));
        if file == helper || test_only {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(&file) else {
            continue;
        };
        scanned += 1;
        for (line, call) in raw_gits(&source) {
            let shown = file.strip_prefix(crates).unwrap_or(&file).display();
            misses.push(format!("crates/{shown}:{line}: {call}"));
        }
    }
    assert!(scanned > 50, "found the sources: {scanned} files");
    assert!(
        misses.is_empty(),
        "Git started without newgit_core::git::command(), so in the developer's locale; use \
         it, or say why not in a `// git-locale:` comment above the call:\n{}",
        misses.join("\n")
    );
}

#[test]
fn the_sweep_sees_through_strings_comments_and_test_code() {
    let source = r##"
fn real() {
    let _ = std::process::Command::new("git").arg("status");
    let _ = tokio::process::Command::new(git_program);
    let _ = "Command::new(\"git\")"; // Command::new("git")
    let _ = r#"Command::new("git")"#;
    let _ = '{';
    // git-locale: for the developer to read.
    let _ = Command::new("git").arg("log");
    let _ = Command::new("sh");
}

#[cfg(test)]
mod tests {
    fn fixture() { let _ = Command::new("git"); let _ = "}"; }
}

fn after_tests() {
    let _ = Command::new("git");
}
"##;
    let lines: Vec<usize> = raw_gits(source).into_iter().map(|(line, _)| line).collect();
    assert_eq!(lines, [3, 4, 19]);
}
