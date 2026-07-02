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
