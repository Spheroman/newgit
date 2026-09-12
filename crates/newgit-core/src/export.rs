use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{NewgitError, Result};
use crate::tracker::{TrackerDefinition, collect_files};

/// Path-level export filtering. Tracker audience is the default filter and
/// flags are the override; there is no hunk privacy, no AST rewriting, and
/// no concealment claim. The filter decides which *files* are copied out of
/// a workspace — nothing more.
///
/// The default is `public`-only, and it fails closed on purpose: a tracker
/// created without `--audience` is `project-devs`, so anything narrower than
/// the repo stays behind unless a flag names it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportFilter {
    /// Workspace-relative paths to include regardless of audience.
    pub includes: Vec<Utf8PathBuf>,
    /// Workspace-relative paths to drop, applied after everything else.
    pub excludes: Vec<Utf8PathBuf>,
}

/// Why one file is in or out of an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Git tracks it: source's audience is everyone.
    Source,
    /// Owned by a tracker whose audience is `public`.
    PublicTracker,
    /// Named by `--include`, overriding a narrower audience.
    Forced,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedFile {
    /// Workspace-relative path, which is also its path in the export.
    pub path: Utf8PathBuf,
    pub reason: Reason,
    /// The tracker that owns it, for anything but plain source.
    pub tracker: Option<String>,
}

/// One tracker's disposition, so the CLI can say what was left behind and
/// how to change its mind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerDisposition {
    pub name: String,
    pub audience: String,
    pub included: usize,
    /// Files held back by audience, with no `--include` covering them.
    pub withheld: Vec<Utf8PathBuf>,
}

impl TrackerDisposition {
    pub fn is_public(&self) -> bool {
        is_public(&self.audience)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportPlan {
    pub files: Vec<ExportedFile>,
    pub trackers: Vec<TrackerDisposition>,
    /// Paths `--exclude` removed, whatever their origin.
    pub excluded: Vec<Utf8PathBuf>,
}

impl ExportPlan {
    pub fn count(&self, reason: Reason) -> usize {
        self.files
            .iter()
            .filter(|file| file.reason == reason)
            .count()
    }
}

/// Audience is a string so `user:<name>` stays expressible; only the exact
/// value `public` means "everyone may read this".
pub fn is_public(audience: &str) -> bool {
    audience == "public"
}

/// Decide what leaves the workspace. Nothing is copied here — a plan is
/// worth having on its own, because `--dry-run` and the failure message both
/// need it before any file moves.
pub fn plan(
    workspace: &Utf8Path,
    source_files: &[Utf8PathBuf],
    trackers: &[TrackerDefinition],
    filter: &ExportFilter,
) -> Result<ExportPlan> {
    let mut files: Vec<ExportedFile> = Vec::new();
    let mut excluded: Vec<Utf8PathBuf> = Vec::new();

    for path in source_files {
        // `ls-files` lists the index; a file deleted but not yet committed is
        // listed and simply is not there to copy.
        if !workspace.join(path).is_file() {
            continue;
        }
        if covers(&filter.excludes, path) {
            excluded.push(path.clone());
            continue;
        }
        files.push(ExportedFile {
            path: path.clone(),
            reason: Reason::Source,
            tracker: None,
        });
    }

    let mut dispositions = Vec::new();
    for tracker in trackers {
        let public = is_public(&tracker.audience);
        let mut included = 0;
        let mut withheld = Vec::new();

        for (relative, _) in collect_files(workspace, &tracker.paths)? {
            if covers(&filter.excludes, &relative) {
                excluded.push(relative);
                continue;
            }
            let forced = covers(&filter.includes, &relative);
            if !public && !forced {
                withheld.push(relative);
                continue;
            }
            included += 1;
            files.push(ExportedFile {
                path: relative,
                reason: if public {
                    Reason::PublicTracker
                } else {
                    Reason::Forced
                },
                tracker: Some(tracker.name.clone()),
            });
        }

        dispositions.push(TrackerDisposition {
            name: tracker.name.clone(),
            audience: tracker.audience.clone(),
            included,
            withheld,
        });
    }

    // An `--include` may also name something no tracker owns and Git does not
    // track — a build output, say. Honoring it keeps the flag meaning one
    // thing: "this path ships".
    for include in &filter.includes {
        for (relative, _) in collect_files(workspace, std::slice::from_ref(include))? {
            if covers(&filter.excludes, &relative) || files.iter().any(|f| f.path == relative) {
                continue;
            }
            files.push(ExportedFile {
                path: relative,
                reason: Reason::Forced,
                tracker: None,
            });
        }
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));
    excluded.sort();
    excluded.dedup();

    Ok(ExportPlan {
        files,
        trackers: dispositions,
        excluded,
    })
}

/// Whether any of `paths` is `candidate` or one of its ancestor directories.
fn covers(paths: &[Utf8PathBuf], candidate: &Utf8Path) -> bool {
    paths.iter().any(|path| candidate.starts_with(path))
}

/// A destination must be absent or empty — export writes a fresh repository,
/// and quietly merging into someone's existing directory is the kind of
/// surprise that loses work.
pub fn prepare_destination(destination: &Utf8Path) -> Result<()> {
    if !destination.exists() {
        return Ok(());
    }
    if !destination.is_dir() {
        return Err(NewgitError::Unsupported(format!(
            "{destination} exists and is not a directory"
        )));
    }
    let mut entries =
        std::fs::read_dir(destination).map_err(|source| NewgitError::io(destination, source))?;
    if entries.next().is_some() {
        return Err(NewgitError::Unsupported(format!(
            "{destination} is not empty; export writes a fresh repository, so pass an empty or \
             nonexistent path"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;

    use super::*;
    use crate::tracker::Storage;

    fn tracker(name: &str, audience: &str, paths: &[&str]) -> TrackerDefinition {
        TrackerDefinition {
            name: name.to_owned(),
            audience: audience.to_owned(),
            storage: Storage::Local,
            merge_with_source: false,
            paths: paths.iter().map(Utf8PathBuf::from).collect(),
            definition_rev: "sha256:000000000000".to_owned(),
        }
    }

    /// A workspace with one source file and two tracker-owned files.
    fn workspace() -> (tempfile::TempDir, Utf8PathBuf) {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf8");
        std::fs::write(root.join("main.rs"), "fn main() {}").expect("write");
        std::fs::write(root.join(".env.local"), "SECRET=1").expect("write");
        std::fs::create_dir_all(root.join("src/generated")).expect("mkdir");
        std::fs::write(root.join("src/generated/api.ts"), "export {}").expect("write");
        (temp, root)
    }

    #[test]
    fn audience_withholds_by_default_and_flags_override() {
        let (_guard, root) = workspace();
        let source = vec![Utf8PathBuf::from("main.rs")];
        let trackers = [
            tracker("runtime-env", "user", &[".env.local"]),
            tracker("generated-sdk", "public", &["src/generated"]),
        ];

        let default = plan(&root, &source, &trackers, &ExportFilter::default()).expect("plan");
        let paths: Vec<&str> = default.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["main.rs", "src/generated/api.ts"]);
        assert_eq!(default.count(Reason::Source), 1);
        assert_eq!(default.count(Reason::PublicTracker), 1);

        let env = default
            .trackers
            .iter()
            .find(|t| t.name == "runtime-env")
            .expect("runtime-env");
        assert_eq!(env.withheld, [Utf8PathBuf::from(".env.local")]);
        assert_eq!(env.included, 0);

        // --include overrides the audience for exactly the named path.
        let forced = plan(
            &root,
            &source,
            &trackers,
            &ExportFilter {
                includes: vec![Utf8PathBuf::from(".env.local")],
                excludes: Vec::new(),
            },
        )
        .expect("plan");
        assert_eq!(forced.count(Reason::Forced), 1);
        assert!(
            forced
                .trackers
                .iter()
                .find(|t| t.name == "runtime-env")
                .expect("runtime-env")
                .withheld
                .is_empty()
        );
    }

    #[test]
    fn exclude_wins_over_source_and_include() {
        let (_guard, root) = workspace();
        let source = vec![Utf8PathBuf::from("main.rs")];
        let trackers = [tracker("generated-sdk", "public", &["src/generated"])];

        let filtered = plan(
            &root,
            &source,
            &trackers,
            &ExportFilter {
                includes: vec![Utf8PathBuf::from("src/generated")],
                excludes: vec![
                    Utf8PathBuf::from("main.rs"),
                    Utf8PathBuf::from("src/generated"),
                ],
            },
        )
        .expect("plan");

        assert!(
            filtered.files.is_empty(),
            "exclude is applied last and wins"
        );
        assert_eq!(
            filtered.excluded,
            [
                Utf8PathBuf::from("main.rs"),
                Utf8PathBuf::from("src/generated/api.ts")
            ]
        );
    }

    #[test]
    fn destination_must_be_absent_or_empty() {
        let (_guard, root) = workspace();
        assert!(prepare_destination(&root.join("fresh")).is_ok());
        std::fs::create_dir_all(root.join("empty")).expect("mkdir");
        assert!(prepare_destination(&root.join("empty")).is_ok());
        assert!(matches!(
            prepare_destination(&root),
            Err(NewgitError::Unsupported(_))
        ));
    }
}
