pub mod branch;
pub mod config;
pub mod error;
pub mod lane;
pub mod manager;
pub mod materializer;
pub mod source;
pub mod store;
pub mod templates;
pub mod tracker;

pub use branch::{BranchInstance, InstanceStatus, TrackerBinding};
pub use config::{ProjectConfig, SourceSubstrate};
pub use error::{NewgitError, Result};
pub use manager::BranchManager;
pub use store::{Context, MetadataStore};
