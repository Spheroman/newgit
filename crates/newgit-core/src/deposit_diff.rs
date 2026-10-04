//! What actually differs between two deposited snapshots — the evidence a
//! `checkpoint --verify` mismatch shows instead of two opaque revs.
//!
//! Lane revs are content hashes, so two deposits with different revs always
//! differ in bytes; the question is *where*. Two lines out of three thousand,
//! both a random token, reads very differently from a dump missing half its
//! tables, and newgit is holding both directories already. The line diff
//! itself is `git diff --no-index`: git is already a hard dependency, and
//! the command printed for the full diff is the same one used here.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{NewgitError, Result};
use crate::tracker::collect_all_files;

/// Changed lines shown across the whole diff. Enough to recognise a nonce
/// or a timestamp; anything longer belongs in the full diff.
const EXCERPT_LINES: usize = 6;
/// Characters kept from each excerpt line. Dumps have very long lines.
const EXCERPT_WIDTH: usize = 120;
/// Files listed by name before the rest are summarised as a count.
const LISTED_FILES: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepositDiff {
    /// The two lane rev directories compared — still on disk, and named so
    /// someone can diff them in full.
    pub before: Utf8PathBuf,
    pub after: Utf8PathBuf,
    /// At most [`LISTED_FILES`] entries; `more_files` counts the rest.
    pub files: Vec<FileDiff>,
    pub more_files: usize,
    /// The first changed lines, each prefixed `-` or `+`.
    pub excerpt: Vec<String>,
    /// Changed lines that exist but were not put in `excerpt`.
    pub excerpt_omitted: usize,
}

impl DepositDiff {
    /// The command that shows this diff in full.
    pub fn full_diff_command(&self) -> String {
        format!("git diff --no-index {} {}", self.before, self.after)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// Relative to the snapshot root.
    pub path: Utf8PathBuf,
    pub change: FileChange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    OnlyBefore,
    OnlyAfter,
    Binary,
    Text {
        removed: usize,
        added: usize,
        /// Lines in the `before` copy, so a count reads as a proportion.
        lines_before: usize,
    },
}

/// Compare two snapshot directories file by file.
pub fn diff_deposits(before: &Utf8Path, after: &Utf8Path) -> Result<DepositDiff> {
    let before_files = collect_all_files(before)?;
    let after_files = collect_all_files(after)?;

    let mut all = Vec::new();
    let mut excerpt = Vec::new();
    let mut excerpt_omitted = 0;
    for (relative, before_path) in &before_files {
        let Some((_, after_path)) = after_files.iter().find(|(path, _)| path == relative) else {
            all.push(FileDiff {
                path: relative.clone(),
                change: FileChange::OnlyBefore,
            });
            continue;
        };
        let before_bytes = read(before_path)?;
        let after_bytes = read(after_path)?;
        if before_bytes == after_bytes {
            continue;
        }
        let Some((removed, added)) = numstat(before_path, after_path)? else {
            all.push(FileDiff {
                path: relative.clone(),
                change: FileChange::Binary,
            });
            continue;
        };
        for line in changed_lines(before_path, after_path)? {
            if excerpt.len() < EXCERPT_LINES {
                excerpt.push(clip(&line));
            } else {
                excerpt_omitted += 1;
            }
        }
        all.push(FileDiff {
            path: relative.clone(),
            change: FileChange::Text {
                removed,
                added,
                lines_before: line_count(&before_bytes),
            },
        });
    }
    for (relative, _) in &after_files {
        if !before_files.iter().any(|(path, _)| path == relative) {
            all.push(FileDiff {
                path: relative.clone(),
                change: FileChange::OnlyAfter,
            });
        }
    }

    let more_files = all.len().saturating_sub(LISTED_FILES);
    all.truncate(LISTED_FILES);
    Ok(DepositDiff {
        before: before.to_path_buf(),
        after: after.to_path_buf(),
        files: all,
        more_files,
        excerpt,
        excerpt_omitted,
    })
}

/// What `wc -l` would say, plus a final line with no newline.
fn line_count(bytes: &[u8]) -> usize {
    bytes.iter().filter(|byte| **byte == b'\n').count()
        + usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n"))
}

fn read(path: &Utf8Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|source| NewgitError::io(path, source))
}

/// `git diff --no-index` exits 1 when the files differ — the expected case
/// here — so only a code above 1 is a failure.
fn git_diff(args: &[&str], before: &Utf8Path, after: &Utf8Path) -> Result<String> {
    let output = Command::new("git")
        .args([
            "diff",
            "--no-index",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
        ])
        .args(args)
        .arg("--")
        .arg(before.as_str())
        .arg(after.as_str())
        .output()
        .map_err(|source| NewgitError::SourceCommand {
            command: "git diff --no-index".to_owned(),
            stderr: source.to_string(),
        })?;
    if output.status.code().is_none_or(|code| code > 1) {
        return Err(NewgitError::SourceCommand {
            command: format!("git diff --no-index {before} {after}"),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `(removed, added)` line counts, or `None` when git calls it binary.
fn numstat(before: &Utf8Path, after: &Utf8Path) -> Result<Option<(usize, usize)>> {
    let stdout = git_diff(&["--numstat"], before, after)?;
    let mut fields = stdout.split('\t');
    let added = fields.next().and_then(|field| field.trim().parse().ok());
    let removed = fields.next().and_then(|field| field.trim().parse().ok());
    Ok(added.zip(removed).map(|(added, removed)| (removed, added)))
}

/// Every `-`/`+` line of a zero-context diff, headers skipped.
fn changed_lines(before: &Utf8Path, after: &Utf8Path) -> Result<Vec<String>> {
    let stdout = git_diff(&["-U0"], before, after)?;
    Ok(stdout
        .lines()
        .skip_while(|line| !line.starts_with("@@"))
        .filter(|line| line.starts_with('-') || line.starts_with('+'))
        .map(ToOwned::to_owned)
        .collect())
}

fn clip(line: &str) -> String {
    match line.char_indices().nth(EXCERPT_WIDTH) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(files: &[(&str, &str)]) -> (tempfile::TempDir, Utf8PathBuf) {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf8");
        for (name, contents) in files {
            std::fs::write(root.join(name), contents).expect("write");
        }
        (temp, root)
    }

    #[test]
    fn a_one_line_nonce_is_reported_as_one_line_of_many() {
        let body: String = (0..50).map(|n| format!("row {n}\n")).collect();
        let (_a, before) = dir_with(&[("db.sql", &format!("\\restrict aaa\n{body}"))]);
        let (_b, after) = dir_with(&[("db.sql", &format!("\\restrict bbb\n{body}"))]);

        let diff = diff_deposits(&before, &after).expect("diff");
        assert_eq!(
            diff.files,
            vec![FileDiff {
                path: "db.sql".into(),
                change: FileChange::Text {
                    removed: 1,
                    added: 1,
                    lines_before: 51
                },
            }]
        );
        assert_eq!(diff.excerpt, vec!["-\\restrict aaa", "+\\restrict bbb"]);
        assert_eq!(diff.excerpt_omitted, 0);
    }

    #[test]
    fn missing_and_extra_files_are_named_and_the_excerpt_is_bounded() {
        let many_before: String = (0..20).map(|n| format!("a{n}\n")).collect();
        let many_after: String = (0..20).map(|n| format!("b{n}\n")).collect();
        let (_a, before) = dir_with(&[("gone.sql", "x\n"), ("rows.sql", &many_before)]);
        let (_b, after) = dir_with(&[("new.sql", "y\n"), ("rows.sql", &many_after)]);

        let diff = diff_deposits(&before, &after).expect("diff");
        let changes: Vec<_> = diff
            .files
            .iter()
            .map(|file| (file.path.as_str(), &file.change))
            .collect();
        assert_eq!(
            changes,
            vec![
                ("gone.sql", &FileChange::OnlyBefore),
                (
                    "rows.sql",
                    &FileChange::Text {
                        removed: 20,
                        added: 20,
                        lines_before: 20
                    }
                ),
                ("new.sql", &FileChange::OnlyAfter),
            ]
        );
        assert_eq!(diff.excerpt.len(), EXCERPT_LINES);
        assert_eq!(diff.excerpt_omitted, 40 - EXCERPT_LINES);
    }
}
