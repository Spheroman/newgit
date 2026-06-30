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

    #[error("newgit metadata has not been initialized at {0}")]
    MissingMetadata(Utf8PathBuf),

    #[error("resource already exists at {0}")]
    AlreadyExists(Utf8PathBuf),

    #[error("unknown starter template `{0}`")]
    UnknownTemplate(String),

    #[error("invalid name `{0}`; use letters, numbers, dots, underscores, or hyphens")]
    InvalidName(String),

    #[error("tracker dependency cycle detected: {0:?}")]
    DependencyCycle(Vec<String>),

    #[error("tracker `{tracker}` depends on missing tracker `{dependency}`")]
    MissingDependency { tracker: String, dependency: String },

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
