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

/// Move a file to the system Trash (recoverable), never a hard delete.
pub fn move_to_trash(path: &Path) -> Result<()> {
    #[allow(unused_mut)]
    let mut ctx = trash::TrashContext::default();
    #[cfg(target_os = "macos")]
    {
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        // The default Finder method needs Automation permission to script Finder.
        // NSFileManager loses Trash "Put Back", which is fine: not needed.
        ctx.set_delete_method(DeleteMethod::NsFileManager);
    }
    ctx.delete(path)
        .map_err(|e| Error::Message(format!("failed to move {} to Trash: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn empty_template_errors() {
        let err = open_with_template("   ", Path::new("a.stl")).unwrap_err();
        assert!(err.to_string().contains("empty open command"));
    }

    #[test]
    fn true_template_succeeds() {
        open_with_template("true {path}", Path::new("/tmp/model.stl")).unwrap();
    }

    #[test]
    fn false_template_fails() {
        assert!(open_with_template("false {path}", Path::new("a.stl")).is_err());
    }
}
