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

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

use crate::error::{NewgitError, Result};
use crate::exports::{RenderContext, render as render_template};

/// One file a resource renders per-instance values into.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

/// Where one rule matched in the committed content.
struct Located {
    start: usize,
    end: usize,
    rule: usize,
}

/// Every occurrence of every `find`, located in one pass over the *same*
/// string. Returned sorted by position.
fn locate(content: &str, finds: &[&str]) -> Vec<Located> {
    let mut found = Vec::new();
    for (rule, find) in finds.iter().enumerate() {
        let mut from = 0;
        while let Some(offset) = content[from..].find(find) {
            let start = from + offset;
            found.push(Located {
                start,
                end: start + find.len(),
                rule,
            });
            // Overlapping occurrences of one `find` are not matches Git or a
            // human would count; advance past this one.
            from = start + find.len();
        }
    }
    found.sort_by_key(|located| (located.start, located.end));
    found
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
/// **Every `find` is located in the committed content, and all replacements
/// apply as one batch. A replacement's output is never a match target.**
/// Rewriting in declaration order, re-matching each rule against the
/// partially-rewritten text, would make a rule mean different things
/// depending on what ran before it: a `find` that happens to equal an earlier
/// rule's output would either report a spurious second match or silently
/// rewrite that output. Declaration order then stops being cosmetic, which is
/// not a property a config file should have.
pub fn apply(
    resource: &str,
    spec: &RenderSpec,
    committed: &str,
    context: &RenderContext,
) -> Result<(String, Vec<AppliedReplacement>)> {
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
        applied.push(AppliedReplacement {
            find: replacement.find.clone(),
            value,
            count: replacement.count,
        });
    }

    let content = substitute(resource, &spec.path, committed, &applied)?;

    // The inverse has to be as unambiguous as the forward pass, or
    // `newgit capture` on a tracker-owned file would rewrite the wrong
    // occurrence. Checked against the finished render, and here rather than
    // at capture, so the failure lands where the definition is in front of
    // you.
    for replacement in &applied {
        let back = content.matches(replacement.value.as_str()).count();
        if back != replacement.count {
            return Err(NewgitError::RenderNotInvertible {
                resource: resource.to_owned(),
                path: spec.path.clone(),
                value: replacement.value.clone(),
                expected: replacement.count,
                found: back,
            });
        }
    }

    Ok((content, applied))
}

/// Apply already-resolved replacements as one simultaneous batch, enforcing
/// each rule's declared match count against the input.
///
/// Also the recomputation behind drift detection: the expected on-disk
/// content of a rendered file is exactly this, run over the same committed
/// content with the replacements the binding record remembers.
pub fn substitute(
    resource: &str,
    path: &Utf8Path,
    committed: &str,
    applied: &[AppliedReplacement],
) -> Result<String> {
    let finds: Vec<&str> = applied
        .iter()
        .map(|replacement| replacement.find.as_str())
        .collect();
    let located = locate(committed, &finds);

    for (rule, replacement) in applied.iter().enumerate() {
        let found = located.iter().filter(|hit| hit.rule == rule).count();
        if found != replacement.count {
            return Err(NewgitError::RenderMatchCount {
                resource: resource.to_owned(),
                path: path.to_path_buf(),
                find: replacement.find.clone(),
                expected: replacement.count,
                found,
            });
        }
    }

    // Two rules claiming overlapping text have no batch answer — whichever
    // won would be an accident of declaration order, the thing simultaneous
    // application exists to remove.
    let mut output = String::with_capacity(committed.len());
    let mut cursor = 0;
    for hit in &located {
        if hit.start < cursor {
            return Err(NewgitError::RenderOverlappingFinds {
                resource: resource.to_owned(),
                path: path.to_path_buf(),
                left: applied[hit.rule].find.clone(),
                right: located
                    .iter()
                    .find(|other| other.end > hit.start && other.rule != hit.rule)
                    .map(|other| applied[other.rule].find.clone())
                    .unwrap_or_else(|| applied[hit.rule].find.clone()),
            });
        }
        output.push_str(&committed[cursor..hit.start]);
        output.push_str(&applied[hit.rule].value);
        cursor = hit.end;
    }
    output.push_str(&committed[cursor..]);
    Ok(output)
}

/// Undo a render: rewrite this instance's values back to the committed ones.
///
/// This is what lets a tracker-owned file be rendered at all. The lane is
/// shared by every instance, so `newgit capture` reverses the substitution
/// before recording — a key you add to `.env.local` reaches the lane and this
/// instance's port does not. Only literal substitution can be run backwards;
/// a whole-file template could not.
///
/// Simultaneous for the same reason [`apply`] is, and lenient where `apply`
/// is strict: this runs against a file someone may have edited, so a value
/// that no longer appears the declared number of times is not an error here.
/// Whether those edits survive is [`crate::manager`]'s question, not this
/// function's.
pub fn reverse(rendered: &str, applied: &[AppliedReplacement]) -> String {
    let values: Vec<&str> = applied
        .iter()
        .map(|replacement| replacement.value.as_str())
        .collect();
    let located = locate(rendered, &values);

    let mut output = String::with_capacity(rendered.len());
    let mut cursor = 0;
    for hit in &located {
        // Leftmost wins where two values overlap; nothing to decide between
        // them, and the alternative is dropping text.
        if hit.start < cursor {
            continue;
        }
        output.push_str(&rendered[cursor..hit.start]);
        output.push_str(&applied[hit.rule].find);
        cursor = hit.end;
    }
    output.push_str(&rendered[cursor..]);
    output
}

/// One `find`'s result against a file's current content, independent of any
/// instance. What `render --check` reports, and nothing more: `with` is
/// never resolved here, because a `find` that fails to match fails whether
/// or not there is a port to substitute in yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindCheck {
    pub find: String,
    pub expected: usize,
    pub found: usize,
}

impl FindCheck {
    pub fn ok(&self) -> bool {
        self.found == self.expected
    }
}

/// Locate every `find` in `content` and report how many times each occurred
/// against how many the definition declares.
///
/// The dry-run counterpart of [`substitute`]'s match-count check: same
/// [`locate`] pass, same per-rule count, but run over whatever `content` is
/// handed rather than committed content, and without resolving `with` or
/// writing anything. That is the whole point — a render's input is always
/// `HEAD` or a tracker's bound rev (see [`apply`]), which is exactly right
/// for idempotence and exactly wrong for the moment you are editing the
/// defaults a `find` targets and have not committed yet. `render --check`
/// calls this against the working tree so that moment has a feedback loop
/// that costs neither a commit nor a spawn.
pub fn check(spec: &RenderSpec, content: &str) -> Vec<FindCheck> {
    let finds: Vec<&str> = spec
        .replace
        .iter()
        .map(|replacement| replacement.find.as_str())
        .collect();
    let located = locate(content, &finds);
    spec.replace
        .iter()
        .enumerate()
        .map(|(rule, replacement)| FindCheck {
            find: replacement.find.clone(),
            expected: replacement.count,
            found: located.iter().filter(|hit| hit.rule == rule).count(),
        })
        .collect()
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

    use super::{RenderSpec, Replacement, apply, check, reverse, validate_disjoint};
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

    /// The collision case. Sequential rewriting would locate rule two
    /// against text rule one had already produced: `port = 54400` would occur
    /// twice, and the exactly-once check would report a match count the
    /// committed file does not have. Locating everything up front makes
    /// declaration order cosmetic, which is what a config file should be.
    #[test]
    fn a_find_that_equals_an_earlier_rules_output_is_not_a_match_target() {
        let ports = ports();
        let context = RenderContext {
            ports: Some(&ports),
            ..RenderContext::default()
        };
        // The second rule's `find` is exactly what the first rule renders.
        // Sequentially, rule two would see two occurrences of it — one of
        // them rule one's own output — and fail the exactly-once check.
        let committed = "port = 54321\nport = 54400\n";
        let spec = spec(vec![
            replacement("port = 54321", "port = {{ports.api}}"),
            replacement("port = 54400", "port = {{ports.db}}"),
        ]);

        let (rendered, _) = apply("supabase", &spec, committed, &context).expect("renders");
        assert_eq!(rendered, "port = 54400\nport = 54500\n");
    }

    /// The same property stated the other way: reordering the rules cannot
    /// change the result.
    #[test]
    fn declaration_order_does_not_change_the_render() {
        let ports = ports();
        let context = RenderContext {
            ports: Some(&ports),
            ..RenderContext::default()
        };
        let committed = "port = 54321\nport = 54400\n";
        let forwards = spec(vec![
            replacement("port = 54321", "port = {{ports.api}}"),
            replacement("port = 54400", "port = {{ports.db}}"),
        ]);
        let backwards = spec(vec![
            replacement("port = 54400", "port = {{ports.db}}"),
            replacement("port = 54321", "port = {{ports.api}}"),
        ]);

        let (one, _) = apply("supabase", &forwards, committed, &context).expect("renders");
        let (two, _) = apply("supabase", &backwards, committed, &context).expect("renders");
        assert_eq!(one, two);
    }

    #[test]
    fn two_finds_claiming_overlapping_text_are_refused() {
        let ports = ports();
        let context = RenderContext {
            ports: Some(&ports),
            ..RenderContext::default()
        };
        let committed = "port = 54321\n";
        let spec = spec(vec![
            replacement("port = 54321", "port = {{ports.api}}"),
            replacement("= 54321", "= {{ports.db}}"),
        ]);

        assert!(matches!(
            apply("supabase", &spec, committed, &context),
            Err(NewgitError::RenderOverlappingFinds { .. })
        ));
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

    /// The case the dry run exists for: a `find` just added to the working
    /// tree, not yet committed, is still visible to `check`.
    #[test]
    fn check_reports_a_find_that_matches_in_the_given_content() {
        let spec = spec(vec![replacement("port = 54321", "port = {{ports.api}}")]);
        let results = check(&spec, "port = 54321\n");
        assert_eq!(results.len(), 1);
        assert!(results[0].ok());
        assert_eq!(results[0].found, 1);
        assert_eq!(results[0].expected, 1);
    }

    #[test]
    fn check_reports_a_find_that_does_not_match() {
        let spec = spec(vec![replacement("port = 54321", "port = {{ports.api}}")]);
        let results = check(&spec, "port = 55555\n");
        assert!(!results[0].ok());
        assert_eq!(results[0].found, 0);
    }

    #[test]
    fn check_reports_a_declared_count_that_does_not_match() {
        let spec = spec(vec![Replacement {
            find: "x".to_owned(),
            with: "{{ports.api}}".to_owned(),
            count: 2,
        }]);
        let results = check(&spec, "x\nx\nx\n");
        assert!(!results[0].ok());
        assert_eq!(results[0].found, 3);
        assert_eq!(results[0].expected, 2);
    }

    #[test]
    fn check_never_needs_a_render_context() {
        // No ports, no exports: `with` is never resolved by `check`, so a
        // placeholder that would fail `apply` does not stop the dry run from
        // reporting whether `find` matched.
        let spec = spec(vec![replacement("port = 54321", "port = {{ports.api}}")]);
        let results = check(&spec, "port = 54321\n");
        assert!(results[0].ok());
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
