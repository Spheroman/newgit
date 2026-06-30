use crate::config::SourceSubstrate;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceStatus {
    pub substrate: SourceSubstrate,
    pub revision: Option<String>,
    pub dirty: bool,
}

pub trait SourceTracker {
    fn create_branch_instance(&self, name: &str, base: Option<&str>) -> crate::Result<()>;
    fn snapshot(&self, message: &str) -> crate::Result<String>;
    fn current_revision(&self) -> crate::Result<Option<String>>;
    fn restore(&self, revision: &str) -> crate::Result<()>;
    fn status(&self) -> crate::Result<SourceStatus>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellSourceTracker {
    substrate: SourceSubstrate,
}

impl ShellSourceTracker {
    pub fn new(substrate: SourceSubstrate) -> Self {
        Self { substrate }
    }

    pub fn substrate(&self) -> &SourceSubstrate {
        &self.substrate
    }
}
