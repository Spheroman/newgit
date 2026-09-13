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

    #[error("could not parse TOML at {path}: {source}")]
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
        "branch instance `{name}` collides with existing record {path}; instance names must be unique after slugging"
    )]
    BranchInstanceExists { name: String, path: Utf8PathBuf },

    #[error("no branch instance named `{0}`")]
    UnknownBranchInstance(String),

    #[error("workspace directory already exists at {0}; remove it or pick another name")]
    WorkspaceExists(Utf8PathBuf),

    #[error("{0} is not a Git repository; newgit v1 drives source through Git")]
    NotAGitRepo(Utf8PathBuf),

    #[error("`{command}` failed: {stderr}")]
    SourceCommand { command: String, stderr: String },

    #[error("tracker `{tracker}` is invalid: {reason}")]
    InvalidDefinition { tracker: String, reason: String },

    #[error("trackers `{left}` and `{right}` both own `{path}`; content lanes must be disjoint")]
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

    #[error("resource dependency cycle: {0:?}")]
    DependencyCycle(Vec<String>),

    #[error(
        "resource `{resource}` depends on `{dependency}`, which is neither a tracker nor a resource"
    )]
    MissingDependency {
        resource: String,
        dependency: String,
    },

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

    #[error("{0}")]
    Unsupported(String),

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
