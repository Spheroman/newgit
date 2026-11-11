//! One ownership rule, decided three ways that must agree (#426 review):
//! [`owner_of`] (what the commit gate, split's check, export, import, and
//! pull ask), the workspace's [`TrackerDefinition::owns`] (what capture,
//! checkout, and clear use, settled by `resolve_ownership`), and where split
//! actually puts a file a commit adds. Each seed generates a set of
//! definitions — mounts nested in mounts, trackers in and outside Git, both
//! ways of writing a claim (`mount` and relative `paths`, or project-relative
//! `paths` at `.`), `!` exclusions, role alternatives — and checks every
//! path against all three. Above all, no path a tracker outside Git owns is
//! ever let into a tracker in Git.
//!
//!     NEWGIT_OWNERSHIP_SEEDS=500 cargo test -p newgit-core --test ownership_agrees
//!     NEWGIT_OWNERSHIP_SEED=17 cargo test -p newgit-core --test ownership_agrees

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::compose::{self, SplitError, SplitOptions};
use newgit_core::tracker::{
    TrackerDefinition, owner_of, owners_of, resolve_ownership, unreached_claims,
};

const SEEDS: u64 = 40;

struct Random(u64);

impl Random {
    fn new(seed: u64) -> Self {
        Random(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, limit: usize) -> usize {
        (self.next() % limit as u64) as usize
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

fn git_env(dir: &Utf8Path, args: &[&str], env: &[(&str, String)]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args(args)
        .envs(env.iter().map(|(key, value)| (*key, value.as_str())))
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn git(dir: &Utf8Path, args: &[&str]) -> String {
    git_env(dir, args, &[])
}

const ROOT_TOML: &str =
    "audience = \"project-devs\"\nstorage = \"local\"\ngit = true\npaths = [\".\"]\n";
const ACCESS_TOML: &str = "audience = \"project-devs\"\nstorage = \"local\"\ngit = true\nmount = \".newgit/trackers\"\npaths = [\".\"]\nmaintainers = \"owner\"\n";

/// Where a tracker may be mounted, and the paths claims are made of.
const MOUNTS: [&str; 4] = [".", "apps", "apps/x", "apps/x/sec"];
const DIRECTORIES: [&str; 6] = [
    "apps",
    "apps/x",
    "apps/x/sec",
    "apps/x/sec/deep",
    "apps/y",
    "lib",
];

/// Every file a commit may add, one per directory and a few beside.
fn files() -> Vec<String> {
    let mut files: Vec<String> = DIRECTORIES
        .iter()
        .map(|directory| format!("{directory}/f.txt"))
        .collect();
    files.extend(
        ["top.txt", "apps/x/sec/g.txt", "apps/x/sec/deep/h.txt"]
            .iter()
            .map(|file| (*file).to_owned()),
    );
    files
}

/// One generated definition, as the file says it.
#[derive(Debug, Clone)]
struct Generated {
    name: String,
    git: bool,
    mount: String,
    claims: Vec<String>,
    role: Option<String>,
}

impl Generated {
    fn toml(&self) -> String {
        let mut text = format!(
            "audience = \"{}\"\nstorage = \"local\"\ngit = {}\n",
            if self.git { "public" } else { "user" },
            self.git
        );
        if self.mount != "." {
            text.push_str(&format!("mount = \"{}\"\n", self.mount));
        }
        let claims: Vec<String> = self.claims.iter().map(|c| format!("\"{c}\"")).collect();
        text.push_str(&format!("paths = [{}]\n", claims.join(", ")));
        if let Some(role) = &self.role {
            text.push_str(&format!("role = \"{role}\"\n"));
        }
        text
    }
}

/// A path under `mount`, relative to it: one of the directories or files
/// inside it, or the mount itself.
fn inside(rng: &mut Random, mount: &str) -> Option<String> {
    let mut choices: Vec<String> = DIRECTORIES
        .iter()
        .map(|d| (*d).to_owned())
        .chain(files())
        .filter_map(|path| {
            if mount == "." {
                return Some(path);
            }
            path.strip_prefix(&format!("{mount}/"))
                .map(ToOwned::to_owned)
        })
        .collect();
    if mount != "." {
        choices.push(".".to_owned());
    }
    choices.sort();
    choices.dedup();
    (!choices.is_empty()).then(|| choices[rng.below(choices.len())].clone())
}

fn generate(rng: &mut Random) -> Vec<Generated> {
    let count = 1 + rng.below(5);
    let mut out: Vec<Generated> = Vec::new();
    for index in 0..count {
        let name = format!("t{index}");
        // A role alternative of one outside Git, claiming just what it
        // claims, in or outside Git itself.
        if let Some(first) = out.iter().find(|d| !d.git && d.role.is_none()).cloned()
            && rng.chance(20)
        {
            let role = format!("r-{}", first.name);
            for definition in &mut out {
                if definition.name == first.name {
                    definition.role = Some(role.clone());
                }
            }
            out.push(Generated {
                name,
                git: rng.chance(50),
                role: Some(role),
                ..first
            });
            continue;
        }
        let git = rng.chance(50);
        let mount = MOUNTS[rng.below(MOUNTS.len())].to_owned();
        let mut claims = Vec::new();
        for _ in 0..1 + rng.below(2) {
            if let Some(claim) = inside(rng, &mount)
                && !(mount == "." && claim == ".")
            {
                claims.push(claim);
            }
        }
        if claims.is_empty() {
            continue;
        }
        // Some exclusion inside the first claim.
        if rng.chance(20) {
            let base = if claims[0] == "." {
                mount.clone()
            } else if mount == "." {
                claims[0].clone()
            } else {
                format!("{mount}/{}", claims[0])
            };
            if let Some(excluded) = inside(rng, &base)
                && excluded != "."
            {
                let excluded = if claims[0] == "." {
                    excluded
                } else {
                    format!("{}/{excluded}", claims[0])
                };
                claims.push(format!("!{excluded}"));
            }
        }
        // The same claims, written project-relative at `.`.
        let (mount, claims) = if mount != "." && rng.chance(40) {
            let claims = claims
                .iter()
                .map(|claim| {
                    let (bang, claim) = match claim.strip_prefix('!') {
                        Some(claim) => ("!", claim),
                        None => ("", claim.as_str()),
                    };
                    if claim == "." {
                        format!("{bang}{mount}")
                    } else {
                        format!("{bang}{mount}/{claim}")
                    }
                })
                .collect();
            (".".to_owned(), claims)
        } else {
            (mount, claims)
        };
        out.push(Generated {
            name,
            git,
            mount,
            claims,
            role: None,
        });
    }
    // Trackers in Git sharing a role would be forks, which split binds one
    // of: out of scope here.
    let mut seen_git_role = std::collections::BTreeSet::new();
    for definition in &mut out {
        if definition.git
            && let Some(role) = &definition.role
            && !seen_git_role.insert(role.clone())
        {
            definition.git = false;
        }
    }
    out
}

fn definitions_of(generated: &[Generated]) -> Option<Vec<TrackerDefinition>> {
    let mut all = vec![
        TrackerDefinition::from_text(
            "root",
            Utf8Path::new(".newgit/trackers/root.toml"),
            ROOT_TOML,
        )
        .ok()?,
        TrackerDefinition::from_text(
            "access",
            Utf8Path::new(".newgit/trackers/access.toml"),
            ACCESS_TOML,
        )
        .ok()?,
    ];
    for definition in generated {
        let path = Utf8PathBuf::from(format!(".newgit/trackers/{}.toml", definition.name));
        all.push(TrackerDefinition::from_text(&definition.name, &path, &definition.toml()).ok()?);
    }
    resolve_ownership(&mut all).ok()?;
    Some(all)
}

/// Where `path` lands in `outcome`: every tracker whose commit holds it,
/// judged at its mount.
fn landed_in(
    dir: &Utf8Path,
    outcome: &compose::SplitOutcome,
    all: &[TrackerDefinition],
    path: &str,
) -> Vec<String> {
    let mut found = Vec::new();
    for (tracker, commit) in &outcome.index {
        let Some(definition) = all.iter().find(|d| &d.name == tracker) else {
            continue;
        };
        let relative = if definition.mount.as_str() == "." {
            path.to_owned()
        } else {
            match path.strip_prefix(&format!("{}/", definition.mount)) {
                Some(relative) => relative.to_owned(),
                None => continue,
            }
        };
        let listed = git(dir, &["ls-tree", "--name-only", commit, "--", &relative]);
        if listed == relative {
            found.push(tracker.clone());
        }
    }
    found
}

fn check(seed: u64) -> bool {
    let mut rng = Random::new(seed);
    let generated = generate(&mut rng);
    let Some(all) = definitions_of(&generated) else {
        return false;
    };
    let context = || {
        let texts: Vec<String> = generated
            .iter()
            .map(|d| format!("{}: {}", d.name, d.toml().replace('\n', "; ")))
            .collect();
        format!("seed {seed}:\n  {}", texts.join("\n  "))
    };

    // The workspace's rule is `owner_of`'s, path by path.
    for path in files() {
        let path = Utf8Path::new(&path);
        let owners: Vec<&str> = owners_of(&all, path)
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        for definition in &all {
            assert_eq!(
                definition.owns(path),
                owners.contains(&definition.name.as_str()),
                "`{}` owns `{path}` in the workspace, but owners_of says {owners:?}\n{}",
                definition.name,
                context()
            );
        }
        if let Some(owner) = owner_of(&all, path) {
            assert!(owners.contains(&owner.name.as_str()), "{}", context());
        }
        // A claim `tracker list` calls unreached owns nothing under it.
        for definition in &all {
            for (claim, _) in unreached_claims(&all, definition) {
                if path.starts_with(claim) {
                    assert!(!definition.owns(path), "{}", context());
                }
            }
        }
    }

    // Split puts what a commit adds where `owner_of` says, or refuses it.
    let temp = tempfile::tempdir().expect("tempdir");
    let dir =
        Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonical")).expect("utf8");
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "commit.gpgSign", "false"]);
    let who = |time: i64| {
        vec![
            ("GIT_AUTHOR_NAME", "ana".to_owned()),
            ("GIT_AUTHOR_EMAIL", "ana@example.com".to_owned()),
            ("GIT_AUTHOR_DATE", format!("@{time} +0000")),
            ("GIT_COMMITTER_NAME", "ana".to_owned()),
            ("GIT_COMMITTER_EMAIL", "ana@example.com".to_owned()),
            ("GIT_COMMITTER_DATE", format!("@{time} +0000")),
        ]
    };
    std::fs::write(dir.join("README.md"), "hello\n").expect("write");
    git(&dir, &["add", "-A"]);
    git_env(&dir, &["commit", "-q", "-m", "start"], &who(1_000));
    let first = git(&dir, &["rev-parse", "HEAD"]);
    git(
        &dir,
        &["update-ref", "refs/newgit/trackers/root/main", &first],
    );
    std::fs::create_dir_all(dir.join(".newgit/trackers")).expect("mkdir");
    std::fs::write(dir.join(".newgit/trackers/root.toml"), ROOT_TOML).expect("write");
    std::fs::write(dir.join(".newgit/trackers/access.toml"), ACCESS_TOML).expect("write");
    for definition in &generated {
        std::fs::write(
            dir.join(format!(".newgit/trackers/{}.toml", definition.name)),
            definition.toml(),
        )
        .expect("write");
    }
    git(&dir, &["add", "-A"]);
    git_env(&dir, &["commit", "-q", "-m", "definitions"], &who(1_100));
    let defined = git(&dir, &["rev-parse", "HEAD"]);
    let bound = |_: &str| true;
    let audience = |_: &str| None;
    let options = SplitOptions {
        root: "root",
        bound: &bound,
        audience: &audience,
        newgit_made: None,
        need_history: &|_| Ok(()),
        forks_before: &[],
    };
    compose::split(&dir, &defined, &options)
        .unwrap_or_else(|error| panic!("split the definitions: {error}\n{}", context()));

    for (step, path) in files().iter().enumerate() {
        git(&dir, &["reset", "-q", "--hard", &defined]);
        let at = dir.join(path);
        std::fs::create_dir_all(at.parent().expect("parent")).expect("mkdir");
        std::fs::write(&at, format!("{path}\n")).expect("write");
        git(&dir, &["add", "-A"]);
        git_env(
            &dir,
            &["commit", "-q", "-m", &format!("add {path}")],
            &who(1_200 + step as i64),
        );
        let tip = git(&dir, &["rev-parse", "HEAD"]);
        let split = compose::split(&dir, &tip, &options);
        let owner = owner_of(&all, Utf8Path::new(path));
        let outside_git_owns = all.iter().any(|d| !d.git && d.owns(Utf8Path::new(path)));
        match (owner, split) {
            (Some(owner), Ok(outcome)) if owner.git => {
                assert!(!outside_git_owns, "{}", context());
                let landed = landed_in(&dir, &outcome, &all, path);
                assert_eq!(
                    landed,
                    std::slice::from_ref(&owner.name),
                    "`{path}` is `{}`'s, but split put it in {landed:?}\n{}",
                    owner.name,
                    context()
                );
            }
            (Some(owner), Err(SplitError::Refused(_))) if !owner.git => {}
            (None, Err(SplitError::Refused(_))) => {}
            (owner, split) => panic!(
                "`{path}`: owner_of says {:?}, split says {}\n{}",
                owner.map(|o| (&o.name, o.git)),
                match split {
                    Ok(outcome) => format!(
                        "ok, landing it in {:?}",
                        landed_in(&dir, &outcome, &all, path)
                    ),
                    Err(error) => error.to_string(),
                },
                context()
            ),
        }
    }
    true
}

#[test]
fn owner_of_the_workspace_and_split_agree() {
    if let Ok(seed) = std::env::var("NEWGIT_OWNERSHIP_SEED") {
        assert!(
            check(seed.parse().expect("a number")),
            "seed {seed} is not a valid set"
        );
        return;
    }
    let seeds = std::env::var("NEWGIT_OWNERSHIP_SEEDS")
        .ok()
        .and_then(|seeds| seeds.parse().ok())
        .unwrap_or(SEEDS);
    let checked = (0..seeds).filter(|seed| check(*seed)).count();
    assert!(
        checked * 2 >= seeds as usize,
        "only {checked} of {seeds} seeds made a valid set of definitions"
    );
}
