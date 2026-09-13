/// Starter resource templates. `companions` are additional resource
/// definitions the template depends on, and `companion_trackers` are lanes
/// it deposits into; both are created alongside it when absent (e.g. the
/// user-owned `pnpm-store` that `deps` depends on, or the `db-snapshots`
/// lane a Postgres dump lands in).
#[derive(Debug, Clone, Copy)]
pub struct ResourceTemplate {
    pub name: &'static str,
    pub description: &'static str,
    pub contents: &'static str,
    pub companions: &'static [CompanionFile],
    pub companion_trackers: &'static [CompanionTracker],
}

#[derive(Debug, Clone, Copy)]
pub struct CompanionFile {
    pub name: &'static str,
    pub contents: &'static str,
}

/// A tracker a template's checkpoint deposits into. Created through the same
/// CLI path as `newgit tracker create`, so trackers stay CLI-managed and
/// templates never hand-write tracker TOML.
#[derive(Debug, Clone, Copy)]
pub struct CompanionTracker {
    pub name: &'static str,
    pub audience: &'static str,
    pub merge_with_source: bool,
}

pub const RESOURCE_TEMPLATES: &[ResourceTemplate] = &[
    ResourceTemplate {
        name: "process",
        description: "a branch-local long-running process with its own port",
        companions: &[],
        companion_trackers: &[],
        contents: r#"ownership = "branch"
# Trackers or resources that must be ready first, e.g. ["deps", "runtime-env"].
depends_on = []

# In a monorepo, every action below likely runs from one package, not the
# workspace root; uncomment and every command runs from there instead.
# workdir = "packages/app"

[ports]
# `env` is the variable your dev server reads its port from. Every resource's
# environment lands in one process environment, so the name has to be unique
# across the project — it is named after this resource for that reason. If
# this is your only service and your tool insists on plain `PORT`, rename it.
app = { start = 3100, env = "RESOURCE_PORT" }

[actions.start]
# Edit to your dev command, e.g. "pnpm dev" or "bin/rails server".
command = "npm run dev"
long_running = true

[actions.stop]
signal = "term"

[exports]
RESOURCE_URL = "http://127.0.0.1:{{ports.app}}"

# Most tools read their port from a committed config file rather than argv.
# Uncomment and point this at yours. There is no template file: `find` names
# the project's working default, so a clone without newgit still starts on it.
#
# `find` is literal, never a regex, and must match exactly once — which is
# also the drift detector. When the default changes upstream, the bind fails
# naming the file and the string instead of quietly doing nothing.
#
# [[render]]
# path = "vite.config.ts"
# replace = [
#   { find = "port: 3000", with = "port: {{ports.app}}" },
# ]
"#,
    },
    ResourceTemplate {
        name: "pnpm",
        description: "dependency install via pnpm; recomputed per instance, hardlinked from one store",
        // The shared package store is user-owned: newgit must never delete
        // or rewrite it. Created alongside so `depends_on` resolves.
        companions: &[CompanionFile {
            name: "pnpm-store",
            contents: r#"ownership = "user"

[identity]
paths = ["pnpm-lock.yaml"]

[checkpoint]
mode = "hash"

[restore]
mode = "none"
"#,
        }],
        companion_trackers: &[],
        contents: r#"# Every instance installs its own dependencies: two branches with
# different lockfiles must not share a tree, or one branch's install
# rewrites the other's. What that costs is your package manager's call.
# pnpm hardlinks from one shared store, so instance ten adds directory
# entries, not gigabytes; `npm ci` expands a full copy every time.
ownership = "workspace"
depends_on = ["pnpm-store"]

[identity]
paths = ["package.json", "pnpm-lock.yaml"]

[actions.prepare]
command = "pnpm install --frozen-lockfile"

# Hashes `[identity] paths` above: what the install is derived from is
# declared once, and a `recompute` restore skips when it has not moved.
[checkpoint]
mode = "hash"

[restore]
mode = "recompute"
action = "prepare"
"#,
    },
    ResourceTemplate {
        name: "command-snapshot",
        description: "a daemon-owned database captured through the daemon into a tracker",
        companions: &[],
        // The dump has to land somewhere versioned; this is the one seam
        // between the two primitives, so the lane ships with the template.
        companion_trackers: &[CompanionTracker {
            name: "db-snapshots",
            audience: "project-devs",
            merge_with_source: false,
        }],
        contents: r#"ownership = "branch"

# A branch-local database, named after the instance so instances never share
# one. Edit the commands for your database; the shape is what matters:
# checkpoint emits a dump into the lane, restore reads it back.

[actions.prepare]
command = "createdb {{branch.slug}} || true"

# A convenience command, not a lifecycle hook: no stage runs `migrate`, and
# nothing but you ever will — `newgit action <this resource>.migrate`. Only
# `prepare` is run for you (by `spawn`, and by a `recompute` restore).
[actions.migrate]
command = "npm run db:migrate"

[checkpoint]
mode = "command"
command = "pg_dump {{branch.slug}} > {{snapshot.path}}/db.sql && echo {{snapshot.path}}/db.sql"
into_tracker = "db-snapshots"

[restore]
mode = "command"
command = "dropdb {{branch.slug}} --if-exists && createdb {{branch.slug}} && psql --quiet {{branch.slug}} < {{state_ref}}"

[cleanup]
command = "dropdb {{branch.slug}} --if-exists"

[exports]
RESOURCE_URL = "postgres://localhost/{{branch.slug}}"
"#,
    },
    ResourceTemplate {
        name: "external",
        description: "a resource another system owns; newgit holds only a handle",
        companions: &[],
        companion_trackers: &[],
        contents: r#"ownership = "external"

# newgit does not own this resource, so it never assumes deletion semantics
# it did not author: `ownership = "external"` means cleanup runs exactly the
# command below and nothing else.
#
# `captures` reads names out of the prepare command's stdout — either a flat
# JSON object or KEY=VALUE lines — and publishes them as this resource's
# exports, so `newgit run` and the hooks below can use them.
#
# When `captures` is set, stdout belongs to newgit: send anything else the
# command prints to stderr, or a chatty CLI's progress output will be
# interleaved with the values and they will not parse. A declared name that
# never turns up is reported as a warning, not an error.

[actions.prepare]
# Edit to your provisioning command.
command = "cloudctl preview create --branch {{branch.name}} --json"
captures = ["RESOURCE_ID", "RESOURCE_URL"]

[checkpoint]
mode = "external"
state_ref = "{{exports.RESOURCE_ID}}"

[restore]
# The handle is recorded, not re-created: an undo does not rewind another
# system. Change to `mode = "command"` if yours can be rewound.
mode = "external"

[cleanup]
command = "cloudctl preview delete {{state_ref}}"
"#,
    },
];

/// The token every template uses where an environment variable name has to be
/// unique across the project. Replaced with the resource's own name when the
/// template is instantiated.
///
/// Templates cannot ship conventional names like `PORT` or `DATABASE_URL`:
/// every resource's environment lands in one process environment, so the
/// second `newgit resource add --template process` would claim a name the
/// first already owns and refuse the whole graph. The one thing newgit knows
/// is unique is the resource name, which is the file it is writing.
const NAME_TOKEN: &str = "RESOURCE_";

/// Instantiate a template for a resource of this name: `RESOURCE_PORT`
/// becomes `WEB_PORT` for a resource called `web`.
pub fn instantiate(contents: &str, resource_name: &str) -> String {
    contents.replace(NAME_TOKEN, &format!("{}_", env_prefix(resource_name)))
}

/// A resource name as an environment variable name fragment: `dev-db` ->
/// `DEV_DB`. Resource names are already validated to a conservative
/// character set, so uppercasing and swapping `-` is enough.
fn env_prefix(resource_name: &str) -> String {
    resource_name.to_uppercase().replace('-', "_")
}

pub fn resource_template(name: &str) -> Option<&'static ResourceTemplate> {
    RESOURCE_TEMPLATES
        .iter()
        .find(|template| template.name == name)
}

/// Names of every starter template, in listing order — for error messages
/// that need to name the valid options (e.g. `resource templates --show`
/// given a name that doesn't exist).
pub fn resource_template_names() -> Vec<&'static str> {
    RESOURCE_TEMPLATES
        .iter()
        .map(|template| template.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two of the same template is the canonical setup (a web and an api),
    /// and it must not produce two resources fighting over one name.
    #[test]
    fn instantiating_one_template_twice_yields_disjoint_env_names() {
        let process = resource_template("process").expect("process template");
        let web = instantiate(process.contents, "web");
        let api = instantiate(process.contents, "api");

        assert!(web.contains(r#"env = "WEB_PORT""#) && web.contains("WEB_URL ="));
        assert!(api.contains(r#"env = "API_PORT""#) && api.contains("API_URL ="));
        assert!(
            !web.contains(NAME_TOKEN) && !api.contains(NAME_TOKEN),
            "no token survives instantiation"
        );
    }

    /// A hyphen is legal in a resource name and illegal in most shells'
    /// variable names.
    #[test]
    fn a_hyphenated_resource_name_becomes_a_usable_variable_name() {
        assert_eq!(
            instantiate("env = \"RESOURCE_PORT\"", "dev-db"),
            "env = \"DEV_DB_PORT\""
        );
    }

    /// Every shipped template has to survive its own instantiation, or
    /// `resource add` writes a file that will not parse.
    #[test]
    fn every_template_still_parses_after_instantiation() {
        for template in RESOURCE_TEMPLATES {
            let contents = instantiate(template.contents, "sample");
            toml::from_str::<toml::Table>(&contents)
                .unwrap_or_else(|e| panic!("template `{}` does not parse: {e}", template.name));
            for companion in template.companions {
                let contents = instantiate(companion.contents, companion.name);
                toml::from_str::<toml::Table>(&contents).unwrap_or_else(|e| {
                    panic!("companion `{}` does not parse: {e}", companion.name)
                });
            }
        }
    }

    #[test]
    fn resource_template_finds_every_listed_name() {
        for name in resource_template_names() {
            assert!(
                resource_template(name).is_some(),
                "`{name}` is listed but resource_template() can't find it"
            );
        }
    }

    #[test]
    fn resource_template_rejects_unknown_names() {
        assert!(resource_template("does-not-exist").is_none());
    }

    #[test]
    fn resource_template_names_matches_listing_order() {
        let expected: Vec<&str> = RESOURCE_TEMPLATES.iter().map(|t| t.name).collect();
        assert_eq!(resource_template_names(), expected);
    }
}
