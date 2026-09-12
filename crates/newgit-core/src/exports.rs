use std::collections::BTreeMap;

/// The minimal template variable set for exports and action commands:
/// `{{ports.<name>}}`, `{{branch.name}}`, `{{branch.slug}}`, `{{workspace}}`.
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
    pub ports: Option<&'a BTreeMap<String, u16>>,
    pub exports: Option<&'a BTreeMap<String, String>>,
    pub snapshot_path: Option<&'a str>,
    pub state_ref: Option<&'a str>,
}

pub fn render(template: &str, context: &RenderContext) -> String {
    let mut rendered = template
        .replace("{{branch.name}}", context.branch_name)
        .replace("{{branch.slug}}", context.branch_slug)
        .replace("{{workspace}}", context.workspace);
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
        rendered = rendered.replace("{{state_ref}}", state_ref);
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
pub fn unresolved_placeholder(rendered: &str) -> Option<&str> {
    let start = rendered.find("{{")?;
    let rest = &rendered[start..];
    let end = rest.find("}}")? + 2;
    Some(&rest[..end])
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{RenderContext, render, unresolved_placeholder};

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
