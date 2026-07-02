use std::collections::BTreeMap;

use camino::Utf8Path;

use crate::error::{NewgitError, Result};

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

/// Dotenv-lite: `KEY=VALUE` lines, `#` comments, optional `export ` prefix,
/// matching single or double quotes stripped. No interpolation.
pub fn parse_env_file(path: &Utf8Path) -> Result<Vec<(String, String)>> {
    let contents = std::fs::read_to_string(path).map_err(|source| NewgitError::io(path, source))?;
    let mut vars = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = value.trim();
        let value = strip_quotes(value);
        vars.push((key.to_owned(), value.to_owned()));
    }
    Ok(vars)
}

fn strip_quotes(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &value[1..value.len() - 1];
        }
    }
    value
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
