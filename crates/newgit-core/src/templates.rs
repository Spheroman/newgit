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

/// Starter resource templates. `companions` are additional definitions the
/// template depends on, created alongside it when absent (e.g. the
/// user-owned `pnpm-store` that `deps` depends on).
#[derive(Debug, Clone, Copy)]
pub struct ResourceTemplate {
    pub name: &'static str,
    pub description: &'static str,
    pub contents: &'static str,
    pub companions: &'static [CompanionFile],
}

#[derive(Debug, Clone, Copy)]
pub struct CompanionFile {
    pub name: &'static str,
    pub contents: &'static str,
}

pub const RESOURCE_TEMPLATES: &[ResourceTemplate] = &[
    ResourceTemplate {
        name: "process",
        description: "a branch-local long-running process with its own port",
        companions: &[],
        contents: r#"kind = "process"
ownership = "branch"
# Trackers or resources that must be ready first, e.g. ["deps", "runtime-env"].
depends_on = []

[ports]
app = { start = 3100, env = "PORT" }

[actions.start]
# Edit to your dev command, e.g. "pnpm dev" or "bin/rails server".
command = "npm run dev"
long_running = true

[actions.stop]
signal = "term"

[exports]
APP_URL = "http://127.0.0.1:{{ports.app}}"
"#,
    },
    ResourceTemplate {
        name: "pnpm",
        description: "dependency install via pnpm; recomputed, never copied",
        // The shared package store is user-owned: newgit must never delete
        // or rewrite it. Created alongside so `depends_on` resolves.
        companions: &[CompanionFile {
            name: "pnpm-store",
            contents: r#"kind = "external-store"
ownership = "user"

[checkpoint]
mode = "hash"
paths = ["pnpm-lock.yaml"]

[restore]
mode = "none"
"#,
        }],
        contents: r#"kind = "command"
ownership = "workspace"
depends_on = ["pnpm-store"]

[identity]
paths = ["package.json", "pnpm-lock.yaml"]

[actions.prepare]
command = "pnpm install --frozen-lockfile"

[checkpoint]
mode = "hash"
paths = ["package.json", "pnpm-lock.yaml"]

[restore]
mode = "recompute"
action = "prepare"
"#,
    },
];

pub fn resource_template(name: &str) -> Option<&'static ResourceTemplate> {
    RESOURCE_TEMPLATES
        .iter()
        .find(|template| template.name == name)
}
