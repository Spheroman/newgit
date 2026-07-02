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
