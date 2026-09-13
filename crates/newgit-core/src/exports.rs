use std::collections::BTreeMap;

/// The minimal template variable set for exports and action commands:
/// `{{ports.<name>}}`, `{{branch.name}}`, `{{branch.slug}}`, `{{workspace}}`,
/// `{{scripts}}`.
///
/// Checkpoint and restore commands additionally see `{{exports.<name>}}`,
/// `{{snapshot.path}}` (the staging dir for `into_tracker` deposits), and
/// `{{state_ref}}` (the checkpointed state reference). Those stay `None`
/// everywhere else — action and export rendering is deliberately minimal.
#[derive(Debug, Clone, Default)]
pub struct RenderContext<'a> {
    pub branch_name: &'a str,
    pub branch_slug: &'a str,
    pub workspace: &'a str,
    /// The store's `.newgit/scripts/`, so a command can shell out to a script
    /// that lives beside the definition calling it instead of in the
    /// workspace, where it would be subject to source materialization.
    pub scripts: &'a str,
    pub ports: Option<&'a BTreeMap<String, u16>>,
    pub exports: Option<&'a BTreeMap<String, String>>,
    pub snapshot_path: Option<&'a str>,
    pub state_ref: Option<&'a str>,
}

/// The placeholder a checkpointed state reference renders into. Named because
/// definition validation has to look for it in a template it will not render.
pub const STATE_REF_PLACEHOLDER: &str = "{{state_ref}}";

pub fn render(template: &str, context: &RenderContext) -> String {
    let mut rendered = template
        .replace("{{branch.name}}", context.branch_name)
        .replace("{{branch.slug}}", context.branch_slug)
        .replace("{{workspace}}", context.workspace)
        .replace("{{scripts}}", context.scripts);
    for (name, port) in context.ports.into_iter().flatten() {
        rendered = rendered.replace(&format!("{{{{ports.{name}}}}}"), &port.to_string());
    }
    for (name, value) in context.exports.into_iter().flatten() {
        rendered = rendered.replace(&format!("{{{{exports.{name}}}}}"), value);
    }
    if let Some(path) = context.snapshot_path {
        rendered = rendered.replace("{{snapshot.path}}", path);
    }
    if let Some(state_ref) = context.state_ref {
        rendered = rendered.replace(STATE_REF_PLACEHOLDER, state_ref);
    }
    rendered
}

/// The first `{{...}}` a render left behind, if any.
///
/// Rendering deliberately leaves unknown variables verbatim so a
/// misconfigured template is visible rather than silently emptied. That is
/// the right default for a command the user watches run, but destructive
/// hooks (cleanup) must refuse instead: `cloudctl preview delete
/// {{state_ref}}` with no state ref is not a no-op, it is a wrong argument.
/// Every `{{exports.<name>}}` a template refers to, in order of appearance.
///
/// Rendering replaces these; the graph reads them instead, to learn which
/// resource a template needs a value *from*. A name is bounded by the closing
/// `}}` and never spans one, so `{{exports.A}}/{{exports.B}}` is two names and
/// a stray `{{exports.` with no close is none.
pub fn export_placeholders(template: &str) -> Vec<&str> {
    const OPEN: &str = "{{exports.";
    let mut names = Vec::new();
    // Every opener is considered, rather than resuming after each closer: in
    // `{{exports.{{exports.A}}` the first opener is not a placeholder and the
    // second one is, and skipping past the `}}` would lose the real
    // reference along with the false one.
    for (start, _) in template.match_indices(OPEN) {
        let rest = &template[start + OPEN.len()..];
        let Some(end) = rest.find("}}") else {
            continue;
        };
        let name = &rest[..end];
        // `{{exports.}}` names nothing, and a `{{` inside means this opener
        // was never a placeholder — the one nested in it may still be.
        if !name.is_empty() && !name.contains("{{") {
            names.push(name);
        }
    }
    names
}

pub fn unresolved_placeholder(rendered: &str) -> Option<&str> {
    let start = rendered.find("{{")?;
    let rest = &rendered[start..];
    let end = rest.find("}}")? + 2;
    Some(&rest[..end])
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{RenderContext, export_placeholders, render, unresolved_placeholder};

    #[test]
    fn export_placeholders_names_every_reference_and_nothing_else() {
        assert_eq!(
            export_placeholders("{{exports.A}}/x/{{exports.B}}"),
            vec!["A", "B"]
        );
        // Other placeholders are not export references.
        assert_eq!(
            export_placeholders("http://127.0.0.1:{{ports.app}}/{{branch.slug}}"),
            Vec::<&str>::new()
        );
        // Malformed references name nothing rather than half a name.
        assert_eq!(export_placeholders("{{exports.}}"), Vec::<&str>::new());
        assert_eq!(export_placeholders("{{exports.A"), Vec::<&str>::new());
        assert_eq!(export_placeholders("{{exports.{{exports.A}}"), vec!["A"]);
    }

    #[test]
    fn renders_ports_and_branch_vars() {
        let ports = BTreeMap::from([("app".to_owned(), 3107)]);
        let context = RenderContext {
            branch_name: "feature/a",
            branch_slug: "feature-a",
            workspace: "/ws",
            ports: Some(&ports),
            ..RenderContext::default()
        };
        assert_eq!(
            render("http://127.0.0.1:{{ports.app}}/{{branch.slug}}", &context),
            "http://127.0.0.1:3107/feature-a"
        );
    }

    #[test]
    fn renders_checkpoint_vars_only_when_provided() {
        let exports = BTreeMap::from([("PREVIEW_ID".to_owned(), "pv_9".to_owned())]);
        let context = RenderContext {
            branch_name: "a",
            branch_slug: "a",
            workspace: "/ws",
            exports: Some(&exports),
            snapshot_path: Some("/stage"),
            state_ref: Some("/snap/db.sql"),
            ..RenderContext::default()
        };
        assert_eq!(
            render(
                "{{exports.PREVIEW_ID}} {{snapshot.path}}/db.sql < {{state_ref}}",
                &context
            ),
            "pv_9 /stage/db.sql < /snap/db.sql"
        );
        // Absent variables are left verbatim, so a misconfigured template is
        // visible in the command instead of silently emptied.
        assert_eq!(
            render("{{state_ref}}", &RenderContext::default()),
            "{{state_ref}}"
        );
    }

    #[test]
    fn unresolved_placeholders_are_reported_for_refusal() {
        assert_eq!(
            unresolved_placeholder("delete {{state_ref}} --force"),
            Some("{{state_ref}}")
        );
        assert_eq!(unresolved_placeholder("delete pv_9"), None);
        // An unterminated brace pair is not a placeholder newgit can name.
        assert_eq!(unresolved_placeholder("echo {{oops"), None);
    }
}
