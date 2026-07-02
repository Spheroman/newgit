pub mod branch;
pub mod config;
pub mod error;
pub mod manager;
pub mod materializer;
pub mod source;
pub mod store;

pub use branch::{BranchInstance, InstanceStatus};
pub use config::{ProjectConfig, SourceSubstrate};
pub use error::{NewgitError, Result};
pub use manager::BranchManager;
pub use store::{Context, MetadataStore};
