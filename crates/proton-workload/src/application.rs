//! Configuration for an independently installed application in the guest share.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Application {
    pub executable: String,
    #[serde(default)]
    pub arguments: Vec<String>,
    pub dll_overrides: String,
}

impl Application {
    pub fn resolve(&self, root: &Path) -> Result<PathBuf> {
        let relative = Path::new(&self.executable);
        ensure!(
            !self.executable.is_empty()
                && relative
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
                && relative
                    .extension()
                    .is_some_and(|v| v.eq_ignore_ascii_case("exe")),
            "application executable must be a relative .exe path"
        );
        ensure!(
            self.arguments.len() <= 64
                && self
                    .arguments
                    .iter()
                    .all(|v| v.len() <= 4096 && !v.contains('\0'))
                && self.dll_overrides.len() <= 4096
                && !self.dll_overrides.contains('\0'),
            "invalid application arguments or DLL overrides"
        );
        let root = root
            .canonicalize()
            .context("locate shared application directory")?;
        let executable = root
            .join(&self.executable)
            .canonicalize()
            .context("locate application executable")?;
        ensure!(
            executable.starts_with(root) && executable.is_file(),
            "application executable escapes its share or is not a file"
        );
        Ok(executable)
    }
}

#[cfg(test)]
#[path = "../tests/unit/application.rs"]
mod tests;
