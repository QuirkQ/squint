//! Apps opened from Finder inherit launchd's bare environment
//! (`PATH=/usr/bin:/bin:/usr/sbin:/sbin`), so a Homebrew `zed` or `git` would be
//! invisible. This module asks the user's login shell for the environment a
//! terminal would have.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// Variables taken from the login shell when the app's own environment lacks them.
const IMPORTED: &[&str] = &["PR_REVIEWS_HOME", "XDG_DATA_HOME"];

pub struct ShellEnv {
    path: OsString,
    vars: HashMap<String, String>,
}

impl ShellEnv {
    pub async fn capture() -> Self {
        let own_path = std::env::var_os("PATH").unwrap_or_default();
        let mut vars: HashMap<String, String> = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .filter(|(k, _)| k != "PATH")
            .collect();
        let login = login_shell_vars().await.unwrap_or_default();

        for &key in IMPORTED {
            if let Some(value) = login.get(key)
                && !vars.contains_key(key)
            {
                vars.insert(key.to_string(), value.clone());
            }
        }
        let login_path = login.get("PATH").map(String::as_str).unwrap_or_default();
        let dirs = std::env::split_paths(login_path).chain(std::env::split_paths(&own_path));
        let path = std::env::join_paths(dirs).unwrap_or(own_path);
        ShellEnv { path, vars }
    }

    pub fn var(&self, key: &str) -> Option<&str> {
        self.vars
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    pub fn home(&self) -> Option<PathBuf> {
        self.var("HOME").map(PathBuf::from)
    }

    pub fn path(&self) -> &OsStr {
        &self.path
    }

    pub fn find_program(&self, name: &str) -> Option<PathBuf> {
        std::env::split_paths(&self.path)
            .map(|dir| dir.join(name))
            .find(|candidate| is_executable(candidate))
    }

    /// A command that runs with the login shell's `PATH`.
    pub fn command(&self, program: &Path) -> Command {
        let mut cmd = Command::new(program);
        cmd.env("PATH", &self.path)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        cmd
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Runs `script` the way a terminal would: in an interactive login shell, so
/// the PATH set up by the user's profile applies.
async fn login_shell_output(script: &str, timeout: Duration) -> Option<String> {
    // Interactive profiles may print banners, so fence the output we care about.
    const FENCE: &str = "__SQUINT_OUTPUT__";
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let script = format!("printf '{FENCE}'; {script}; printf '{FENCE}'");

    let output = Command::new(shell)
        .args(["-i", "-l", "-c", &script])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(timeout, output).await.ok()?.ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.split(FENCE).nth(1).map(str::to_string)
}

async fn login_shell_vars() -> Option<HashMap<String, String>> {
    let env = login_shell_output("env", Duration::from_secs(5)).await?;
    Some(
        env.lines()
            .filter_map(|line| line.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
}
