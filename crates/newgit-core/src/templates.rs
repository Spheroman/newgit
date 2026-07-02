/// Starter tracker templates. Conveniences over the tracker primitive, not
/// privileged subsystems — the generated TOML is meant to be edited.
#[derive(Debug, Clone, Copy)]
pub struct TrackerTemplate {
    pub name: &'static str,
    pub description: &'static str,
    pub contents: &'static str,
}

pub const TRACKER_TEMPLATES: &[TrackerTemplate] = &[
    TrackerTemplate {
        name: "env-file",
        description: "branch-local env file, pinned per branch",
        contents: r#"kind = "env-file"
audience = "user"
storage = "local"
propagation = "pin"
paths = [".env.local"]

[materialize]
copy_from = ".newgit/templates/base.env"
to = ".env.local"

[exports]
env_file = ".env.local"
"#,
    },
    TrackerTemplate {
        name: "file-snapshot",
        description: "any branch-local files that checkpoint/restore as content",
        contents: r#"kind = "file-snapshot"
audience = "project-devs"
storage = "local"
propagation = "manual"
# List the workspace paths this tracker owns, e.g. ["src/generated"].
paths = []
"#,
    },
    TrackerTemplate {
        name: "sqlite",
        description: "a SQLite database file, pinned per branch",
        contents: r#"kind = "sqlite"
audience = "project-devs"
storage = "local"
propagation = "pin"
paths = ["data/dev.sqlite"]

[exports]
DATABASE_URL = "sqlite://data/dev.sqlite"
"#,
    },
];

pub fn tracker_template(name: &str) -> Option<&'static TrackerTemplate> {
    TRACKER_TEMPLATES
        .iter()
        .find(|template| template.name == name)
}
