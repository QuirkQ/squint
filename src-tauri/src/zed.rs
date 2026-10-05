//! Opens a pull request's worktree from the review workspace in Zed.

use std::path::{Component, Path};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::shell_env::ShellEnv;

/// File paths come from the UI and are joined onto the worktree, so they must
/// stay inside it.
pub fn check_path(path: &str) -> Result<()> {
    let inside = Path::new(path)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    if inside {
        Ok(())
    } else {
        Err(Error::msg(format!("Refusing to open {path:?}")))
    }
}

pub async fn launch(
    env: &ShellEnv,
    worktree: &Path,
    path: Option<&str>,
    line: Option<u32>,
) -> Result<()> {
    let file = path.map(|p| {
        let file = worktree.join(p).to_string_lossy().into_owned();
        match line {
            Some(line) => format!("{file}:{line}"),
            None => file,
        }
    });

    // Prefer the `zed` CLI; fall back to Launch Services when it isn't installed.
    let mut cmd = match env.find_program("zed") {
        Some(zed) => env.command(&zed),
        None if cfg!(target_os = "macos") => {
            let mut open = env.command(Path::new("/usr/bin/open"));
            open.args(["-a", "Zed"]);
            open
        }
        None => return Err(Error::msg("The `zed` command was not found on PATH.")),
    };
    cmd.arg(worktree);
    cmd.args(file);

    let output = tokio::time::timeout(Duration::from_secs(20), cmd.output())
        .await
        .map_err(|_| Error::msg("Zed did not respond"))??;
    if !output.status.success() {
        return Err(Error::msg(format!(
            "Could not open Zed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_paths_inside_the_worktree() {
        assert!(check_path("app/models/user.rb").is_ok());
        for bad in ["../x", "/etc/passwd", "a/../../b", "./a"] {
            assert!(check_path(bad).is_err(), "{bad}");
        }
    }
}
