use std::collections::BTreeMap;

/// The minimal template variable set for exports and action commands:
/// `{{ports.<name>}}`, `{{branch.name}}`, `{{branch.slug}}`, `{{workspace}}`.
#[derive(Debug, Clone)]
pub struct RenderContext<'a> {
    pub branch_name: &'a str,
    pub branch_slug: &'a str,
    pub workspace: &'a str,
    pub ports: &'a BTreeMap<String, u16>,
}

pub fn render(template: &str, context: &RenderContext) -> String {
    let mut rendered = template
        .replace("{{branch.name}}", context.branch_name)
        .replace("{{branch.slug}}", context.branch_slug)
        .replace("{{workspace}}", context.workspace);
    for (name, port) in context.ports {
        rendered = rendered.replace(&format!("{{{{ports.{name}}}}}"), &port.to_string());
    }
    rendered
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{RenderContext, render};

    #[test]
    fn renders_ports_and_branch_vars() {
        let ports = BTreeMap::from([("app".to_owned(), 3107)]);
        let context = RenderContext {
            branch_name: "feature/a",
            branch_slug: "feature-a",
            workspace: "/ws",
            ports: &ports,
        };
        assert_eq!(
            render("http://127.0.0.1:{{ports.app}}/{{branch.slug}}", &context),
            "http://127.0.0.1:3107/feature-a"
        );
    }
}
