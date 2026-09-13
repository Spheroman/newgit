//! Per-instance values substituted into a file the project commits.
//!
//! Ports reach a command as `{{ports.x}}` and as an env var. Most tools do
//! not take them that way: Supabase reads `supabase/config.toml`, Expo reads
//! `.env`, Compose reads `compose.yaml`. Without this, every project that
//! hits it writes the same config rewriter inside its `prepare` hook.
//!
//! There is no template file. `port = 54321` is not a placeholder, it is the
//! project's working default — a clone without newgit still starts on it.
//! newgit substitutes into the committed content and writes the result into
//! one workspace. See *Render* in newgit-v1-mvp.md.

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

use crate::error::{NewgitError, Result};
use crate::exports::{RenderContext, render as render_template};

/// One file a resource renders per-instance values into.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RenderSpec {
    /// Workspace-relative path to the committed file.
    pub path: Utf8PathBuf,
    #[serde(default)]
    pub replace: Vec<Replacement>,
}

/// A literal substitution. `find` is never a regex: a pattern language
/// reintroduces the "did it match what I meant" doubt that [`Replacement::count`]
/// exists to remove, and it would leave the inverse undefined.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Replacement {
    pub find: String,
    pub with: String,
    /// How many times `find` is expected to occur. Exactly one by default.
    ///
    /// Not an `all` flag: a declared number keeps failing when a file changes
    /// from two occurrences to three, which is the property worth protecting.
    #[serde(default = "one")]
    pub count: usize,
}

fn one() -> usize {
    1
}

/// What a render actually did, recorded on the binding record so capture can
/// reverse it without re-deriving the values from a definition that may have
/// been edited since.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RenderRecord {
    pub path: Utf8PathBuf,
    /// The tracker owning this path, if any. Decides where committed content
    /// is read from, and whether capture must reverse the substitution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracker: Option<String>,
    pub applied: Vec<AppliedReplacement>,
}

/// A replacement with its template already rendered, so the inverse is exact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppliedReplacement {
    pub find: String,
    /// `with` after `{{ports.*}}` and friends were resolved.
    pub value: String,
    pub count: usize,
}

/// Substitute into `committed`, the file's committed content — never what is
/// currently on disk.
///
/// That is what makes a render idempotent: reading the working file would
/// mean the second render looks for `port = 54321`, finds `port = 54400`, and
/// fails. Reading committed content means `undo` re-renders off the binding
/// record with no extra machinery, a `pull` that moves a lane head re-renders
/// from the new content, and values can never compound.
///
/// Replacements apply in declaration order, each checked against the content
/// as it stands at that step.
pub fn apply(
    resource: &str,
    spec: &RenderSpec,
    committed: &str,
    context: &RenderContext,
) -> Result<(String, Vec<AppliedReplacement>)> {
    let mut content = committed.to_owned();
    let mut applied = Vec::with_capacity(spec.replace.len());

    for replacement in &spec.replace {
        let value = render_template(&replacement.with, context);
        if let Some(unresolved) = crate::exports::unresolved_placeholder(&value) {
            return Err(NewgitError::RenderUnresolved {
                resource: resource.to_owned(),
                path: spec.path.clone(),
                placeholder: unresolved.to_owned(),
            });
        }

        let found = content.matches(&replacement.find).count();
        if found != replacement.count {
            return Err(NewgitError::RenderMatchCount {
                resource: resource.to_owned(),
                path: spec.path.clone(),
                find: replacement.find.clone(),
                expected: replacement.count,
                found,
            });
        }

        content = content.replace(&replacement.find, &value);

        // The inverse has to be as unambiguous as the forward pass, or
        // `newgit capture` on a tracker-owned file would rewrite the wrong
        // occurrence. Checked here so the failure lands at bind, where the
        // definition is in front of you, rather than at capture.
        let back = content.matches(value.as_str()).count();
        if back != replacement.count {
            return Err(NewgitError::RenderNotInvertible {
                resource: resource.to_owned(),
                path: spec.path.clone(),
                value,
                expected: replacement.count,
                found: back,
            });
        }

        applied.push(AppliedReplacement {
            find: replacement.find.clone(),
            value,
            count: replacement.count,
        });
    }

    Ok((content, applied))
}

/// Undo a render: rewrite this instance's values back to the committed ones.
///
/// This is what lets a tracker-owned file be rendered at all. The lane is
/// shared by every instance, so `newgit capture` reverses the substitution
/// before recording — a key you add to `.env.local` reaches the lane and this
/// instance's port does not. Only literal substitution can be run backwards;
/// a whole-file template could not.
///
/// Reverses in the opposite order to [`apply`], so a chain of replacements
/// unwinds the way it was built.
pub fn reverse(rendered: &str, applied: &[AppliedReplacement]) -> String {
    let mut content = rendered.to_owned();
    for replacement in applied.iter().rev() {
        content = content.replace(&replacement.value, &replacement.find);
    }
    content
}

/// Two resources rendering the same path is a config error, not a merge:
/// they would race, and the second would render over the first's output and
/// fail its own match check for reasons nothing in the definition explains.
pub fn validate_disjoint(specs: &[(&str, &RenderSpec)]) -> Result<()> {
    for (index, (left, left_spec)) in specs.iter().enumerate() {
        for (right, right_spec) in &specs[index + 1..] {
            if left_spec.path == right_spec.path {
                return Err(NewgitError::RenderPathConflict {
                    left: (*left).to_owned(),
                    right: (*right).to_owned(),
                    path: left_spec.path.clone(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{RenderSpec, Replacement, apply, reverse, validate_disjoint};
    use crate::error::NewgitError;
    use crate::exports::RenderContext;

    fn ports() -> BTreeMap<String, u16> {
        BTreeMap::from([("api".to_owned(), 54400), ("db".to_owned(), 54500)])
    }

    fn spec(replace: Vec<Replacement>) -> RenderSpec {
        RenderSpec {
            path: "supabase/config.toml".into(),
            replace,
        }
    }

    fn replacement(find: &str, with: &str) -> Replacement {
        Replacement {
            find: find.to_owned(),
            with: with.to_owned(),
            count: 1,
        }
    }

    #[test]
    fn substitutes_into_committed_content() {
        let ports = ports();
        let context = RenderContext {
            branch_slug: "feature-a",
            ports: Some(&ports),
            ..RenderContext::default()
        };
        let committed = "[api]\nport = 54321\n[db]\nport = 54322\n";
        let spec = spec(vec![
            replacement("port = 54321", "port = {{ports.api}}"),
            replacement("port = 54322", "port = {{ports.db}}"),
        ]);

        let (rendered, applied) = apply("supabase", &spec, committed, &context).expect("renders");
        assert_eq!(rendered, "[api]\nport = 54400\n[db]\nport = 54500\n");
        assert_eq!(applied.len(), 2);
    }

    #[test]
    fn rendering_is_idempotent_because_it_reads_committed_content() {
        let ports = ports();
        let context = RenderContext {
            ports: Some(&ports),
            ..RenderContext::default()
        };
        let committed = "port = 54321\n";
        let spec = spec(vec![replacement("port = 54321", "port = {{ports.api}}")]);

        let (once, _) = apply("supabase", &spec, committed, &context).expect("renders");
        let (twice, _) = apply("supabase", &spec, committed, &context).expect("renders");
        assert_eq!(once, twice);
    }

    #[test]
    fn a_find_that_matches_twice_is_refused() {
        let ports = ports();
        let context = RenderContext {
            ports: Some(&ports),
            ..RenderContext::default()
        };
        let committed = "port = 54321\nport = 54321\n";
        let spec = spec(vec![replacement("port = 54321", "port = {{ports.api}}")]);

        assert!(matches!(
            apply("supabase", &spec, committed, &context),
            Err(NewgitError::RenderMatchCount {
                expected: 1,
                found: 2,
                ..
            })
        ));
    }

    /// The drift detector: upstream bumps its default and bind fails naming
    /// the string, rather than silently doing nothing.
    #[test]
    fn a_find_that_stopped_matching_is_refused() {
        let ports = ports();
        let context = RenderContext {
            ports: Some(&ports),
            ..RenderContext::default()
        };
        let committed = "port = 55555\n";
        let spec = spec(vec![replacement("port = 54321", "port = {{ports.api}}")]);

        assert!(matches!(
            apply("supabase", &spec, committed, &context),
            Err(NewgitError::RenderMatchCount { found: 0, .. })
        ));
    }

    #[test]
    fn a_declared_count_permits_exactly_that_many() {
        let ports = ports();
        let context = RenderContext {
            ports: Some(&ports),
            ..RenderContext::default()
        };
        let committed = "- \"3000:3000\"\n- \"3000:3000\"\n";
        let spec = spec(vec![Replacement {
            find: "\"3000:3000\"".to_owned(),
            with: "\"{{ports.api}}:3000\"".to_owned(),
            count: 2,
        }]);

        let (rendered, _) = apply("app", &spec, committed, &context).expect("renders");
        assert_eq!(rendered, "- \"54400:3000\"\n- \"54400:3000\"\n");
    }

    #[test]
    fn a_declared_count_still_fails_when_the_file_gains_one() {
        let ports = ports();
        let context = RenderContext {
            ports: Some(&ports),
            ..RenderContext::default()
        };
        let committed = "x\nx\nx\n";
        let spec = spec(vec![Replacement {
            find: "x".to_owned(),
            with: "{{ports.api}}".to_owned(),
            count: 2,
        }]);

        assert!(matches!(
            apply("app", &spec, committed, &context),
            Err(NewgitError::RenderMatchCount {
                expected: 2,
                found: 3,
                ..
            })
        ));
    }

    /// Multi-line `find` is how two sections sharing a default disambiguate,
    /// without newgit ever learning what TOML is.
    #[test]
    fn multiline_find_disambiguates_identical_defaults() {
        let ports = ports();
        let context = RenderContext {
            ports: Some(&ports),
            ..RenderContext::default()
        };
        let committed = "[api]\nport = 54321\n\n[studio]\nport = 54321\n";
        let spec = spec(vec![replacement(
            "[api]\nport = 54321",
            "[api]\nport = {{ports.api}}",
        )]);

        let (rendered, _) = apply("supabase", &spec, committed, &context).expect("renders");
        assert_eq!(rendered, "[api]\nport = 54400\n\n[studio]\nport = 54321\n");
    }

    #[test]
    fn round_trips_through_reverse() {
        let ports = ports();
        let context = RenderContext {
            branch_slug: "feature-a",
            ports: Some(&ports),
            ..RenderContext::default()
        };
        let committed = "SUPABASE_URL=http://127.0.0.1:54321\nAPI_KEY=local\n";
        let spec = spec(vec![replacement(
            "SUPABASE_URL=http://127.0.0.1:54321",
            "SUPABASE_URL=http://127.0.0.1:{{ports.api}}",
        )]);

        let (rendered, applied) = apply("supabase", &spec, committed, &context).expect("renders");
        assert_eq!(reverse(&rendered, &applied), committed);
    }

    /// The point of reversing: edits made alongside the rendered value still
    /// reach the lane.
    #[test]
    fn reverse_keeps_edits_made_beside_the_rendered_value() {
        let ports = ports();
        let context = RenderContext {
            ports: Some(&ports),
            ..RenderContext::default()
        };
        let committed = "SUPABASE_URL=http://127.0.0.1:54321\n";
        let spec = spec(vec![replacement(
            "SUPABASE_URL=http://127.0.0.1:54321",
            "SUPABASE_URL=http://127.0.0.1:{{ports.api}}",
        )]);

        let (rendered, applied) = apply("supabase", &spec, committed, &context).expect("renders");
        let edited = format!("{rendered}STRIPE_KEY=sk_test_123\n");
        assert_eq!(
            reverse(&edited, &applied),
            "SUPABASE_URL=http://127.0.0.1:54321\nSTRIPE_KEY=sk_test_123\n"
        );
    }

    #[test]
    fn a_value_that_cannot_be_reversed_unambiguously_is_refused() {
        let ports = ports();
        let context = RenderContext {
            ports: Some(&ports),
            ..RenderContext::default()
        };
        // Rendering `54400` here would leave two occurrences of it, so a
        // later capture could not know which one to put back.
        let committed = "port = 54321\nother = 54400\n";
        let spec = spec(vec![replacement("54321", "{{ports.api}}")]);

        assert!(matches!(
            apply("supabase", &spec, committed, &context),
            Err(NewgitError::RenderNotInvertible { .. })
        ));
    }

    #[test]
    fn an_unresolved_placeholder_is_refused() {
        let context = RenderContext::default();
        let spec = spec(vec![replacement("port = 54321", "port = {{ports.api}}")]);

        assert!(matches!(
            apply("supabase", &spec, "port = 54321\n", &context),
            Err(NewgitError::RenderUnresolved { .. })
        ));
    }

    #[test]
    fn two_resources_rendering_one_path_is_refused() {
        let left = spec(vec![]);
        let right = spec(vec![]);
        assert!(matches!(
            validate_disjoint(&[("supabase", &left), ("app", &right)]),
            Err(NewgitError::RenderPathConflict { .. })
        ));
    }
}
