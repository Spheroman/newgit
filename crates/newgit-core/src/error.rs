use camino::Utf8PathBuf;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, NewgitError>;

#[derive(Debug, Error)]
pub enum NewgitError {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: Utf8PathBuf,
        source: std::io::Error,
    },

    // The source is not interpolated here: toml renders a multi-line snippet
    // with the offending span, and anyhow already prints it as the cause.
    // Naming it twice turns every parse error into the same paragraph read
    // back to you.
    #[error("could not parse TOML at {path}")]
    TomlRead {
        path: Utf8PathBuf,
        source: toml::de::Error,
    },

    #[error("could not serialize TOML for {label}: {source}")]
    TomlWrite {
        label: String,
        source: toml::ser::Error,
    },

    #[error(
        "newgit is not initialized here (no .newgit found from {0} upward); run `newgit init` in the project root"
    )]
    MissingMetadata(Utf8PathBuf),

    #[error("already exists at {0}")]
    AlreadyExists(Utf8PathBuf),

    #[error(
        "invalid name `{0}`; use letters, numbers, dots, underscores, hyphens, or slashes, starting with a letter or number"
    )]
    InvalidName(String),

    #[error(
        "invalid name: {len} characters is over the {max}-character limit; names end up in \
         Docker container names, Supabase project ids, and hostnames, which break well before \
         a filesystem does"
    )]
    NameTooLong { len: usize, max: usize },

    #[error(
        "branch instance `{name}` collides with existing record {path}; instance names must be unique after slugging"
    )]
    BranchInstanceExists { name: String, path: Utf8PathBuf },

    #[error("no branch instance named `{0}`")]
    UnknownBranchInstance(String),

    #[error(
        "a newgit command (pid {pid}) is already running on `{instance}` and this one was \
         started from inside it — a hook cannot run a newgit command that changes its own \
         instance; it would wait for itself forever"
    )]
    NestedCommand { instance: String, pid: u32 },

    #[error("workspace directory already exists at {0}; remove it or pick another name")]
    WorkspaceExists(Utf8PathBuf),

    #[error("{0} is not a Git repository; newgit v1 drives source through Git")]
    NotAGitRepo(Utf8PathBuf),

    #[error("`{command}` failed: {stderr}")]
    SourceCommand { command: String, stderr: String },

    /// The newgit remote answered with an error. `message` is the remote's
    /// own explanation, which says why (a refused credential names the rule).
    #[error("the newgit remote refused ({status}): {message}")]
    Remote { status: u16, message: String },

    #[error("could not reach the newgit remote at {url}: {detail}")]
    RemoteUnreachable { url: String, detail: String },

    #[error("not logged in to {0}; run `newgit login`")]
    NotLoggedIn(String),

    #[error("this project has no [remote] in .newgit/config.toml; run `newgit remote add`")]
    NoRemote,

    #[error("{kind} `{name}` is invalid: {reason}")]
    InvalidDefinition {
        kind: &'static str,
        name: String,
        reason: String,
    },

    #[error(
        "trackers `{left}` and `{right}` both own `{path}`; give one of them a more specific path, or give both the same `role` if they are alternatives"
    )]
    TrackerPathConflict {
        left: String,
        right: String,
        path: camino::Utf8PathBuf,
    },

    #[error("no tracker named `{0}` is defined in .newgit/trackers/")]
    UnknownTracker(String),

    #[error("unknown starter template `{0}`")]
    UnknownTemplate(String),

    #[error("tracker `{tracker}` has no captured content at rev `{rev}`")]
    NoSnapshot { tracker: String, rev: String },

    #[error("tracker `{0}` owns no paths; there is nothing to capture from a workspace")]
    TrackerHasNoPaths(String),

    /// Capture found a symlink at a tracked path that points outside the
    /// directory being captured. Following it would copy a file from
    /// anywhere on the machine into a lane that may sync to a remote.
    #[error(
        "refusing to capture `{path}` into tracker `{tracker}`: it is a symlink to `{target}`, \
         which is outside the workspace. newgit never copies what a link points at; it keeps \
         a link only when it is relative and stays inside the workspace. Replace `{path}` with \
         a regular file, or with a link that stays inside"
    )]
    SymlinkLeavesWorkspace {
        tracker: String,
        path: Utf8PathBuf,
        target: Utf8PathBuf,
    },

    /// A tracked path lies under a directory that is a symlink, so what is
    /// "at" the path is really somewhere else.
    #[error(
        "refusing to read `{path}`: `{link}` is a symlink to `{target}`, so `{path}` is not in \
         the workspace but wherever that link leads. Replace `{link}` with a real directory"
    )]
    SymlinkedParent {
        path: Utf8PathBuf,
        link: Utf8PathBuf,
        target: Utf8PathBuf,
    },

    /// A lane holds a link whose target leaves the workspace. Written out, it
    /// would point the workspace at whatever the lane's author chose.
    #[error(
        "refusing to check out tracker `{tracker}` at `{rev}`: `{path}` is a symlink to \
         `{target}`, which is outside the workspace"
    )]
    LaneLinkLeavesWorkspace {
        tracker: String,
        rev: String,
        path: Utf8PathBuf,
        target: String,
    },

    #[error(
        "tracker `{tracker}` owns nothing on disk in the store repo, so there is nothing to \
         seed from: {paths}"
    )]
    NothingToSeed { tracker: String, paths: String },

    #[error("no resource named `{0}` is defined in .newgit/resources/")]
    UnknownResource(String),

    #[error("resource `{resource}` has no action named `{action}`")]
    UnknownAction { resource: String, action: String },

    #[error("resource `{resource}` is already running (pid {pid}); stop it first")]
    AlreadyRunning { resource: String, pid: u32 },

    #[error(
        "resource `{resource}` action `{action}` has workdir `{path}`, which does not exist; \
         if an earlier action creates it, run that first"
    )]
    MissingWorkdir {
        resource: String,
        action: String,
        path: Utf8PathBuf,
    },

    #[error("resource dependency cycle: {0:?}")]
    DependencyCycle(Vec<String>),

    #[error(
        "resource `{resource}` depends on `{dependency}`, which is neither a tracker nor a resource"
    )]
    MissingDependency {
        resource: String,
        dependency: String,
    },

    #[error(
        "environment variable `{name}` is declared more than once ({claimants}); a name may \
         have only one owner — {remedy}"
    )]
    EnvNameCollision {
        name: String,
        claimants: String,
        remedy: String,
    },

    #[error(
        "environment variable `{name}` is declared by {claimant}, but newgit sets `{name}` for \
         every command it runs and would shadow it; rename it"
    )]
    ReservedEnvName { name: String, claimant: String },

    #[error("no checkpoints exist for `{0}`; create one with `newgit checkpoint {0}`")]
    NoCheckpoints(String),

    #[error(
        "no checkpoint `{id}` for `{instance}`; list them with `newgit checkpoints {instance}`"
    )]
    UnknownCheckpoint { instance: String, id: String },

    #[error(
        "checkpoint command for resource `{resource}` exited with {code} (log: {log}); \
         the checkpoint was aborted — a checkpoint that missed a resource is not coherent"
    )]
    CheckpointCommandFailed {
        resource: String,
        code: i32,
        log: Utf8PathBuf,
    },

    #[error(
        "resource `{resource}` renders into `{path}`, but `{find}` occurs {found} time(s) there, \
         not {expected}; the committed file changed under the definition, or the string is not \
         unique — a multi-line `find` disambiguates, and `count` declares a repeat{hint}",
        hint = no_match_hint(*found)
    )]
    RenderMatchCount {
        resource: String,
        path: Utf8PathBuf,
        find: String,
        expected: usize,
        found: usize,
    },

    #[error(
        "resource `{resource}` renders `{value}` into `{path}`, which would then occur {found} \
         time(s) there, not {expected}; `newgit capture` could not reverse that unambiguously, \
         so include more context in `find` and `with`"
    )]
    RenderNotInvertible {
        resource: String,
        path: Utf8PathBuf,
        value: String,
        expected: usize,
        found: usize,
    },

    #[error(
        "resource `{resource}` leaves exports unresolved: {exports}; an export is rendered once \
         and handed to every later command as an environment variable, so a placeholder left \
         verbatim would surface in another process rather than here. None of this resource's \
         exports are stored"
    )]
    ExportUnresolved {
        resource: String,
        /// Every export that did not resolve, as ``` `NAME` ({{placeholder}}) ```
        /// — all of them, so fixing one does not just reveal the next.
        exports: String,
    },

    #[error(
        "resource `{resource}` renders into `{path}` with unresolved `{placeholder}`; a render \
         writes a file rather than a command line, so an unrendered placeholder would be \
         committed-looking content nobody wrote"
    )]
    RenderUnresolved {
        resource: String,
        path: Utf8PathBuf,
        placeholder: String,
    },

    #[error(
        "resource `{resource}` renders into `{path}`, where `{left}` and `{right}` claim \
         overlapping text; all finds are located in the committed content and applied at once, \
         so there is no order in which one of them wins"
    )]
    RenderOverlappingFinds {
        resource: String,
        path: Utf8PathBuf,
        left: String,
        right: String,
    },

    #[error(
        "resources `{left}` and `{right}` both claim the same text in `{path}` (`{left}` finds \
         {left_find:?}, `{right}` finds {right_find:?}); several resources may render one file, \
         but each piece of text has one owner. Keep the rule in one of them, and if the other \
         needs it, have it export the value and reach it with `{{{{exports.NAME}}}}`"
    )]
    RenderPathConflict {
        left: String,
        left_find: String,
        right: String,
        right_find: String,
        path: Utf8PathBuf,
    },

    #[error(
        "resource `{resource}` renders into `{path}`, which is neither tracked by Git nor owned \
         by a tracker; a render substitutes into committed content, so there has to be some"
    )]
    RenderPathNotCommitted { resource: String, path: Utf8PathBuf },

    #[error("{0}")]
    Unsupported(String),

    /// Work that may not leave this machine as it is: a secret tracker's
    /// file committed, or a path whose tracker the instance has not bound.
    #[error("refusing to send this work anywhere: {0}")]
    SplitRefused(String),

    #[error("path is not valid UTF-8: {0}")]
    NonUtf8Path(String),
}

impl NewgitError {
    pub fn io(path: impl Into<Utf8PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// When a `find` matches nothing, the likeliest cause is that the file has no
/// line for that setting at all (a `config.toml` without an `[inbucket]`
/// section). The tool then uses its own built-in default, and every instance
/// shares it, so say that rather than leaving "0 time(s)" to be decoded.
fn no_match_hint(found: usize) -> &'static str {
    if found == 0 {
        ". If the file has no line for this setting, the tool falls back to its own default, \
         which every instance shares: add the line to the committed file so there is one to \
         rewrite. Deleting the rule only hides the collision"
    } else {
        ""
    }
}
