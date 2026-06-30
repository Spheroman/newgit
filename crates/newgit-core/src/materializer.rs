use camino::Utf8Path;

use crate::branch::BranchInstance;
use crate::error::{NewgitError, Result};

pub trait Materializer {
    fn materialize(&self, branch: &BranchInstance) -> Result<()>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RealDirMaterializer;

impl Materializer for RealDirMaterializer {
    fn materialize(&self, branch: &BranchInstance) -> Result<()> {
        create_dir_all(&branch.workspace_path)
    }
}

pub fn create_dir_all(path: &Utf8Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|source| NewgitError::io(path.to_path_buf(), source))
}
