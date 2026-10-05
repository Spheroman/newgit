//! What actually differs between two deposited snapshots — the evidence a
//! `checkpoint --verify` mismatch shows instead of two opaque revs.
//!
//! Lane revs are content hashes, so two deposits with different revs always
//! differ in bytes; the question is *where*. Two lines out of three thousand,
//! both a random token, reads very differently from a dump missing half its
//! tables, and newgit is holding both directories already. The diff itself
//! is `git diff --no-index`: git is already a hard dependency, and the
//! command printed for the full diff is the same one used here.
//!
//! Bounded on purpose. A dump can be hundreds of megabytes, and one compared
//! against an empty restore changes every line of it. So: exactly two git
//! processes however many files changed — one `--numstat` for the counts,
//! one patch that is streamed and killed as soon as the excerpt is full —
//! and nothing here ever holds a whole file or a whole diff in memory.

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{NewgitError, Result};

/// Changed lines shown across the whole diff. Enough to recognise a nonce
/// or a timestamp; anything longer belongs in the full diff.
const EXCERPT_LINES: usize = 6;
/// Characters kept from each excerpt line. Dumps have very long lines.
const EXCERPT_WIDTH: usize = 120;
/// Bytes read from any one patch line; the rest of it is skipped unread
/// into memory. Comfortably more than [`EXCERPT_WIDTH`] characters.
const LINE_BYTES: usize = 4 * EXCERPT_WIDTH + 4;
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
    /// The first changed lines, each prefixed `-` or `+`, control
    /// characters escaped.
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

/// Compare two snapshot directories.
pub fn diff_deposits(before: &Utf8Path, after: &Utf8Path) -> Result<DepositDiff> {
    let mut files = Vec::new();
    let mut changed_lines = 0;
    for entry in numstat(before, after)? {
        let Some(path) = entry.before.clone().or_else(|| entry.after.clone()) else {
            continue;
        };
        let change = match (&entry.before, &entry.after, entry.counts) {
            (Some(_), None, counts) => {
                changed_lines += counts.map_or(0, |(added, removed)| added + removed);
                FileChange::OnlyBefore
            }
            (None, Some(_), counts) => {
                changed_lines += counts.map_or(0, |(added, removed)| added + removed);
                FileChange::OnlyAfter
            }
            (Some(_), Some(_), None) => FileChange::Binary,
            (Some(relative), Some(_), Some((added, removed))) => {
                changed_lines += added + removed;
                // Only the listed files are worth a pass over their bytes.
                let lines_before = if files.len() < LISTED_FILES {
                    line_count(&before.join(relative))?
                } else {
                    0
                };
                FileChange::Text {
                    removed,
                    added,
                    lines_before,
                }
            }
            (None, None, _) => continue,
        };
        files.push(FileDiff { path, change });
    }

    let excerpt = if changed_lines > 0 {
        excerpt(before, after)?
    } else {
        Vec::new()
    };
    let more_files = files.len().saturating_sub(LISTED_FILES);
    files.truncate(LISTED_FILES);
    Ok(DepositDiff {
        before: before.to_path_buf(),
        after: after.to_path_buf(),
        files,
        more_files,
        excerpt_omitted: changed_lines.saturating_sub(excerpt.len()),
        excerpt,
    })
}

/// Escape every control character except tab, so a row holding an ANSI or
/// OSC sequence prints as text instead of acting on the terminal showing it.
pub fn escape_control(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() && character != '\t' {
            if u32::from(character) < 0x80 {
                escaped.push_str(&format!("\\x{:02x}", u32::from(character)));
            } else {
                escaped.extend(character.escape_unicode());
            }
        } else {
            escaped.push(character);
        }
    }
    escaped
}

struct NumstatEntry {
    /// Relative to its snapshot root; `None` for `/dev/null`.
    before: Option<Utf8PathBuf>,
    after: Option<Utf8PathBuf>,
    /// `(added, removed)`, or `None` when git calls the file binary.
    counts: Option<(usize, usize)>,
}

fn git_diff_command(args: &[&str], before: &Utf8Path, after: &Utf8Path) -> Command {
    let mut command = Command::new("git");
    command
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
        .arg(after.as_str());
    command
}

fn spawn_failed(source: std::io::Error) -> NewgitError {
    NewgitError::SourceCommand {
        command: "git diff --no-index".to_owned(),
        stderr: source.to_string(),
    }
}

/// One line per changed file, so its size is the file count, not the
/// content. `-z` keeps paths unquoted and unambiguous: each record is
/// `added\tremoved\t\0<before>\0<after>\0`, with `/dev/null` for a side the
/// file is missing from.
fn numstat(before: &Utf8Path, after: &Utf8Path) -> Result<Vec<NumstatEntry>> {
    let output = git_diff_command(&["--numstat", "-z"], before, after)
        .stdin(Stdio::null())
        .output()
        .map_err(spawn_failed)?;
    // `git diff --no-index` exits 1 when the inputs differ — the expected
    // case here — so only a code above 1 is a failure.
    if output.status.code().is_none_or(|code| code > 1) {
        return Err(NewgitError::SourceCommand {
            command: format!("git diff --no-index --numstat {before} {after}"),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut fields = stdout.split('\0');
    let mut entries = Vec::new();
    while let Some(stats) = fields.next() {
        if stats.is_empty() {
            break;
        }
        let mut counts = stats.split('\t');
        let added = counts.next().and_then(|count| count.parse().ok());
        let removed = counts.next().and_then(|count| count.parse().ok());
        let (Some(old), Some(new)) = (fields.next(), fields.next()) else {
            break;
        };
        entries.push(NumstatEntry {
            before: relative_to(old, before),
            after: relative_to(new, after),
            counts: added.zip(removed),
        });
    }
    Ok(entries)
}

fn relative_to(path: &str, root: &Utf8Path) -> Option<Utf8PathBuf> {
    Utf8Path::new(path)
        .strip_prefix(root)
        .ok()
        .map(Utf8Path::to_path_buf)
}

/// The first [`EXCERPT_LINES`] changed lines of a zero-context patch,
/// streamed, with git killed as soon as they are in hand.
fn excerpt(before: &Utf8Path, after: &Utf8Path) -> Result<Vec<String>> {
    let mut child = git_diff_command(&["-U0"], before, after)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(spawn_failed)?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut reader = BufReader::new(stdout);

    let mut lines = Vec::new();
    // Only lines inside a hunk are content: a removed SQL comment reads
    // `--- like this`, exactly as a file header does.
    let mut in_hunk = false;
    while lines.len() < EXCERPT_LINES {
        let Some(line) = read_bounded_line(&mut reader) else {
            break;
        };
        if line.starts_with(b"diff --git ") {
            in_hunk = false;
        } else if line.starts_with(b"@@") {
            in_hunk = true;
        } else if in_hunk && (line.starts_with(b"-") || line.starts_with(b"+")) {
            lines.push(clip(&String::from_utf8_lossy(&line)));
        }
    }
    // Whatever is left — possibly most of a 400 MB patch — is never read.
    let _ = child.kill();
    let _ = child.wait();
    Ok(lines)
}

/// One line without its newline, holding at most [`LINE_BYTES`] of it in
/// memory however long it is. `None` at the end of the stream.
fn read_bounded_line(reader: &mut impl BufRead) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    let mut any = false;
    loop {
        let buffer = reader.fill_buf().ok()?;
        if buffer.is_empty() {
            return any.then_some(line);
        }
        any = true;
        let (chunk, found_newline) = match buffer.iter().position(|byte| *byte == b'\n') {
            Some(end) => (&buffer[..end], true),
            None => (buffer, false),
        };
        let room = LINE_BYTES.saturating_sub(line.len());
        line.extend_from_slice(&chunk[..chunk.len().min(room)]);
        let consumed = chunk.len() + usize::from(found_newline);
        reader.consume(consumed);
        if found_newline {
            return Some(line);
        }
    }
}

/// What `wc -l` would say, plus a final line with no newline — counted in
/// fixed-size chunks rather than by reading the file into memory.
fn line_count(path: &Utf8Path) -> Result<usize> {
    let mut file = std::fs::File::open(path).map_err(|source| NewgitError::io(path, source))?;
    let mut buffer = [0u8; 64 * 1024];
    let mut lines = 0;
    let mut last = None;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| NewgitError::io(path, source))?;
        if read == 0 {
            break;
        }
        lines += buffer[..read].iter().filter(|byte| **byte == b'\n').count();
        last = Some(buffer[read - 1]);
    }
    Ok(lines + usize::from(last.is_some_and(|byte| byte != b'\n')))
}

fn clip(line: &str) -> String {
    let clipped = match line.char_indices().nth(EXCERPT_WIDTH) {
        Some((cut, _)) => &line[..cut],
        None => line,
    };
    let escaped = escape_control(clipped);
    if clipped.len() < line.len() {
        format!("{escaped}…")
    } else {
        escaped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(files: &[(&str, &str)]) -> (tempfile::TempDir, Utf8PathBuf) {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonical"))
            .expect("utf8");
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
    fn missing_extra_and_unchanged_files_are_told_apart() {
        let many_before: String = (0..20).map(|n| format!("a{n}\n")).collect();
        let many_after: String = (0..20).map(|n| format!("b{n}\n")).collect();
        let (_a, before) = dir_with(&[
            ("gone.sql", "x\n"),
            ("rows.sql", &many_before),
            ("same.sql", "s\n"),
            ("blob.bin", "\0\x01"),
        ]);
        let (_b, after) = dir_with(&[
            ("new.sql", "y\n"),
            ("rows.sql", &many_after),
            ("same.sql", "s\n"),
            ("blob.bin", "\0\x02"),
        ]);

        let diff = diff_deposits(&before, &after).expect("diff");
        let mut changes: Vec<_> = diff
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.change.clone()))
            .collect();
        changes.sort_by(|a, b| a.0.cmp(b.0));
        assert_eq!(
            changes,
            vec![
                ("blob.bin", FileChange::Binary),
                ("gone.sql", FileChange::OnlyBefore),
                ("new.sql", FileChange::OnlyAfter),
                (
                    "rows.sql",
                    FileChange::Text {
                        removed: 20,
                        added: 20,
                        lines_before: 20
                    }
                ),
            ]
        );
        assert_eq!(diff.excerpt.len(), EXCERPT_LINES);
        // 1 gone + 1 new + 40 rows; the binary file has no lines to count.
        assert_eq!(diff.excerpt_omitted, 42 - EXCERPT_LINES);
    }

    /// A dump against an empty restore: every line changed. The excerpt
    /// stays six lines and the rest is counted, not collected.
    #[test]
    fn a_large_diff_is_counted_not_collected() {
        let rows = 200_000;
        let big: String = (0..rows).map(|n| format!("INSERT {n};\n")).collect();
        let (_a, before) = dir_with(&[("db.sql", &big)]);
        let (_b, after) = dir_with(&[("db.sql", "")]);

        let diff = diff_deposits(&before, &after).expect("diff");
        assert_eq!(diff.excerpt.len(), EXCERPT_LINES);
        assert_eq!(diff.excerpt_omitted, rows - EXCERPT_LINES);
        assert_eq!(
            diff.files[0].change,
            FileChange::Text {
                removed: rows,
                added: 0,
                lines_before: rows
            }
        );
    }

    #[test]
    fn one_enormous_line_is_read_only_as_far_as_the_excerpt_needs() {
        let mut reader = BufReader::with_capacity(
            16,
            std::io::Cursor::new(format!("{}\nnext\n", "x".repeat(1_000_000))),
        );
        let first = read_bounded_line(&mut reader).expect("a line");
        assert_eq!(first.len(), LINE_BYTES);
        assert_eq!(
            read_bounded_line(&mut reader).as_deref(),
            Some(&b"next"[..])
        );
        assert_eq!(read_bounded_line(&mut reader), None);
    }

    #[test]
    fn terminal_control_sequences_in_a_row_are_escaped() {
        assert_eq!(
            clip("+\x1b[31mred\x1b]0;title\x07\tok\r"),
            "+\\x1b[31mred\\x1b]0;title\\x07\tok\\x0d"
        );
        assert_eq!(escape_control("csi \u{9b}"), "csi \\u{9b}");
    }
}
