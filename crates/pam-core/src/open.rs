use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result};

/// Open a file with the OS default handler (usually the slicer).
pub fn open_path(path: &Path) -> Result<()> {
    let status = if cfg!(target_os = "macos") {
        Command::new("open").arg(path).status()?
    } else {
        Command::new("xdg-open").arg(path).status()?
    };
    if status.success() {
        Ok(())
    } else {
        Err(Error::Message(format!(
            "failed to open {}: {status}",
            path.display()
        )))
    }
}

/// `{path}` in the template is replaced with the file path.
pub fn open_with_template(template: &str, path: &Path) -> Result<()> {
    let rendered = template.replace("{path}", &path.display().to_string());
    let mut parts = rendered.split_whitespace();
    let cmd = parts
        .next()
        .ok_or_else(|| Error::Message("empty open command".into()))?;
    let status = Command::new(cmd).args(parts).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Message(format!("open command failed: {status}")))
    }
}
