use crate::error::{NewgitError, Result};
use crate::tracker::TrackerDefinition;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StarterTemplate {
    pub name: &'static str,
    pub description: &'static str,
    pub contents: &'static str,
}

const ENV_FILE: &str = r#"
name = "__TRACKER_NAME__"
kind = "env-file"
ownership = "branch"
propagation = "pin"

[materialize]
copy_from = ".newgit/templates/base.env"
to = ".env.local"

[capture]
mode = "copy"
paths = [".env.local"]

[restore]
mode = "copy"

[exports]
env_file = ".env.local"
"#;

const PNPM: &str = r#"
name = "__TRACKER_NAME__"
kind = "command"
ownership = "workspace"
propagation = "recompute"
depends_on = []

[identity]
paths = ["package.json", "pnpm-lock.yaml"]

[actions.prepare]
command = "pnpm install --frozen-lockfile"

[capture]
mode = "hash"
paths = ["package.json", "pnpm-lock.yaml"]

[restore]
mode = "recompute"
action = "prepare"
"#;

const PROCESS: &str = r#"
name = "__TRACKER_NAME__"
kind = "process"
ownership = "branch"
propagation = "pin"
depends_on = []

[ports.app]
start = 3100
env = "PORT"

[actions.start]
command = "pnpm dev"
long_running = true

[actions.stop]
signal = "term"

[exports]
APP_URL = "http://127.0.0.1:{{ports.app}}"
"#;

const FILE_SNAPSHOT: &str = r#"
name = "__TRACKER_NAME__"
kind = "file-snapshot"
ownership = "workspace"
propagation = "manual"

[capture]
mode = "copy"
paths = ["src/generated"]

[restore]
mode = "copy"
"#;

const SQLITE: &str = r#"
name = "__TRACKER_NAME__"
kind = "sqlite"
ownership = "branch"
propagation = "manual"

[capture]
mode = "copy"
paths = ["data/dev.sqlite"]

[restore]
mode = "copy"

[exports]
DATABASE_URL = "sqlite://data/dev.sqlite"
"#;

const COMMAND_SNAPSHOT: &str = r#"
name = "__TRACKER_NAME__"
kind = "command-snapshot"
ownership = "branch"
propagation = "manual"

[actions.prepare]
command = "createdb {{branch.slug}} || true"

[actions.migrate]
command = "pnpm db:migrate"

[capture]
mode = "command"
command = "pg_dump {{branch.slug}} > {{snapshot.path}}/db.sql && echo {{snapshot.path}}/db.sql"

[restore]
mode = "command"
command = "dropdb {{branch.slug}} --if-exists && createdb {{branch.slug}} && psql {{branch.slug}} < {{state_ref}}"

[exports]
DATABASE_URL = "postgres://localhost/{{branch.slug}}"
"#;

const EXTERNAL: &str = r#"
name = "__TRACKER_NAME__"
kind = "external"
ownership = "external"
propagation = "manual"

[actions.prepare]
command = "cloudctl preview create --branch {{branch.name}} --json"
captures = ["PREVIEW_ID", "PREVIEW_URL"]

[capture]
mode = "external"
state_ref = "{{exports.PREVIEW_ID}}"

[cleanup]
command = "cloudctl preview delete {{state_ref}}"
"#;

pub fn starter_templates() -> &'static [StarterTemplate] {
    &[
        StarterTemplate {
            name: "env-file",
            description: "Branch-local env file exported to commands",
            contents: ENV_FILE,
        },
        StarterTemplate {
            name: "pnpm",
            description: "Workspace-local dependency preparation through pnpm",
            contents: PNPM,
        },
        StarterTemplate {
            name: "process",
            description: "Branch-local long-running process with a port export",
            contents: PROCESS,
        },
        StarterTemplate {
            name: "file-snapshot",
            description: "Generic branch/workspace file snapshot",
            contents: FILE_SNAPSHOT,
        },
        StarterTemplate {
            name: "sqlite",
            description: "SQLite database represented as a file snapshot",
            contents: SQLITE,
        },
        StarterTemplate {
            name: "command-snapshot",
            description: "State captured and restored through user commands",
            contents: COMMAND_SNAPSHOT,
        },
        StarterTemplate {
            name: "external",
            description: "Opaque external resource handle",
            contents: EXTERNAL,
        },
    ]
}

pub fn starter_template(template_name: &str, tracker_name: &str) -> Result<TrackerDefinition> {
    let template = starter_templates()
        .iter()
        .find(|candidate| candidate.name == template_name)
        .ok_or_else(|| NewgitError::UnknownTemplate(template_name.to_owned()))?;
    let rendered = template.contents.replace("__TRACKER_NAME__", tracker_name);
    let definition =
        toml::from_str::<TrackerDefinition>(&rendered).map_err(|source| NewgitError::TomlRead {
            path: format!("<template:{template_name}>").into(),
            source,
        })?;
    definition.validate()?;
    Ok(definition)
}

#[cfg(test)]
mod tests {
    use super::{starter_template, starter_templates};

    #[test]
    fn all_starter_templates_parse_as_tracker_definitions() {
        for template in starter_templates() {
            let definition = starter_template(template.name, "runtime-env")
                .expect("template should parse into a tracker definition");
            assert_eq!(definition.name, "runtime-env");
            assert!(!definition.kind.is_empty());
        }
    }
}
