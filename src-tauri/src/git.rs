//! Runs `git` for the review workspace.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use base64::Engine;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::error::{Error, Result};
use crate::shell_env::ShellEnv;

#[derive(Clone)]
pub struct Git {
    program: PathBuf,
    path: OsString,
    auth_header: Option<String>,
    offline: bool,
}

impl Git {
    pub fn new(env: &ShellEnv, token: &str) -> Self {
        let program = env
            .find_program("git")
            .unwrap_or_else(|| "/usr/bin/git".into());
        Git::with_program(program, env.path().to_owned(), Some(token))
    }

    pub fn with_program(program: PathBuf, path: OsString, token: Option<&str>) -> Self {
        let auth_header = token.map(|token| {
            let basic =
                base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
            format!("AUTHORIZATION: basic {basic}")
        });
        Git {
            program,
            path,
            auth_header,
            offline: false,
        }
    }

    /// The same git, but a partial clone reports missing file contents
    /// instead of quietly downloading them one at a time.
    pub fn offline(&self) -> Git {
        Git {
            offline: true,
            ..self.clone()
        }
    }

    pub fn command(&self, dir: &Path) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.arg("-C")
            .arg(dir)
            .env("PATH", &self.path)
            // The workspace is the app's, not yours: your global hooks, URL
            // rewrites (https → ssh) and credential helpers stay out of it.
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        if self.offline {
            cmd.env("GIT_NO_LAZY_FETCH", "1");
        }
        // The token travels through the environment rather than argv (where any
        // process could read it) or a config file (where it would persist).
        if let Some(header) = &self.auth_header {
            cmd.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraheader")
                .env("GIT_CONFIG_VALUE_0", header);
        }
        cmd
    }

    /// Runs git and returns its stdout, or its stderr as the error.
    pub async fn run(&self, dir: &Path, args: &[&str]) -> Result<String> {
        let stdout = self.run_bytes(dir, args).await?;
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    }

    /// Like [`Git::run`], for output that may not be text.
    pub async fn run_bytes(&self, dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
        let output = self.command(dir).args(args).output().await?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(failure(args, &output.stderr))
        }
    }

    /// Like [`Git::run`], feeding `input` to git's stdin.
    pub async fn run_with_input(&self, dir: &Path, args: &[&str], input: &str) -> Result<String> {
        let mut child = self
            .command(dir)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input.as_bytes()).await?;
        }
        let output = child.wait_with_output().await?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(failure(args, &output.stderr))
        }
    }

    /// For yes/no commands such as `merge-base --is-ancestor`: exit code 0 is
    /// yes, 1 is no, anything else is an error.
    pub async fn check(&self, dir: &Path, args: &[&str]) -> Result<bool> {
        let output = self.command(dir).args(args).output().await?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(failure(args, &output.stderr)),
        }
    }
}

fn failure(args: &[&str], stderr: &[u8]) -> Error {
    let stderr = String::from_utf8_lossy(stderr);
    Error::msg(format!(
        "git {} failed: {}",
        args.first().unwrap_or(&""),
        stderr.trim()
    ))
}
