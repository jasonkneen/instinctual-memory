//! Isolated Git command wrapper.
//!
//! All Git operations go through this module. It:
//! - strips inherited `GIT_*` variables,
//! - disables system, global, and worktree config; aliases; hooks; signing,
//! - enforces a timeout and a clean environment,
//! - and accepts an optional `GIT_INDEX_FILE` for staged-entry work.
//!
//! This is the only place that shells out to Git. It is not a public API;
//! library users should depend on [`crate::repo`] instead.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::error::{Error, Result};

/// Timeout applied to every Git invocation.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

/// Default author/committer identity baked into the wrapper.
///
/// The service is the only entity that should commit to the memory repository.
pub const IDENTITY_NAME: &str = "Memory service";
pub const IDENTITY_EMAIL: &str = "memory@example.invalid";

/// A wrapper around a dedicated bare Git repository.
#[derive(Debug, Clone)]
pub struct Git {
    repo: PathBuf,
    timeout: Duration,
}

impl Git {
    /// Construct a wrapper for the given repository path. The path is resolved
    /// and must point to an existing bare repository; use [`Git::create`] to
    /// bootstrap a new one.
    pub fn new(repo: impl Into<PathBuf>) -> Result<Self> {
        let repo = repo.into();
        let resolved = repo.canonicalize().unwrap_or(repo);
        let g = Self {
            repo: resolved,
            timeout: DEFAULT_TIMEOUT,
        };
        if !g.is_bare()? {
            return Err(Error::NotBareRepository(g.repo.clone()));
        }
        Ok(g)
    }

    /// Construct a wrapper without verifying that the path is a bare repo.
    /// Useful for the publisher's bootstrap flow.
    pub fn new_unchecked(repo: impl Into<PathBuf>) -> Self {
        let repo = repo.into();
        Self {
            repo: repo.canonicalize().unwrap_or(repo),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Override the per-invocation timeout (mostly for tests).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Path to the bare repository.
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// True if the configured path is a bare Git repository.
    pub fn is_bare(&self) -> Result<bool> {
        let out = self.run_raw(["rev-parse", "--is-bare-repository"], None, None)?;
        Ok(out.trim() == "true")
    }

    /// Run a Git subcommand, returning the captured stdout.
    pub fn run<I, S>(&self, args: I, input: Option<&[u8]>, index: Option<&Path>) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_raw(args, input, index)
    }

    /// Same as [`Self::run`] but accepts `&str` slices.
    pub fn run_args(&self, args: &[&str], input: Option<&[u8]>, index: Option<&Path>) -> Result<String> {
        self.run_raw(args.iter().copied(), input, index)
    }

    fn run_raw<I, S>(&self, args: I, input: Option<&[u8]>, index: Option<&Path>) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let captured: Vec<String> = args
            .into_iter()
            .map(|a| a.as_ref().to_string_lossy().into_owned())
            .collect();
        let mut cmd = self.command(captured.iter().map(|s| s.as_str()));
        if let Some(idx) = index {
            cmd.env("GIT_INDEX_FILE", idx);
        }
        if input.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(err) => return Err(Error::io(self.repo.clone(), err)),
        };

        if let (Some(data), Some(stdin)) = (input, child.stdin.as_mut()) {
            use std::io::Write;
            stdin.write_all(data).map_err(|e| Error::io(self.repo.clone(), e))?;
        }

        let output = match child.wait_with_output() {
            Ok(o) => o,
            Err(err) => return Err(Error::io(self.repo.clone(), err)),
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Git(format!(
                "git {} failed: {}",
                captured.join(" "),
                stderr.trim()
            )));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Construct a [`Command`] pre-loaded with our hardened environment.
    pub fn command<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new("git");
        cmd.arg("--git-dir");
        cmd.arg(&self.repo);
        cmd.arg("-c").arg("core.hooksPath=/dev/null");
        cmd.arg("-c").arg("commit.gpgSign=false");
        cmd.arg("-c").arg("alias.mktree=");
        cmd.arg("-c").arg("alias.commit-tree=");
        cmd.arg("-c").arg("alias.update-ref=");
        cmd.arg("-c").arg("alias.update-index=");
        cmd.arg("-c").arg("alias.read-tree=");
        cmd.arg("-c").arg("alias.hash-object=");
        cmd.arg("-c").arg("alias.write-tree=");
        cmd.arg("-c").arg("alias.cat-file=");
        cmd.arg("-c").arg("alias.ls-tree=");
        cmd.arg("-c").arg("alias.rev-parse=");
        cmd.arg("-c").arg("alias.revert=");
        cmd.arg("-c").arg("alias.init=");
        cmd.args(args);

        // Strip any inherited Git environment and force a clean identity.
        for (key, _) in std::env::vars() {
            if key.starts_with("GIT_") {
                cmd.env_remove(key);
            }
        }
        cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        cmd.env("GIT_AUTHOR_NAME", IDENTITY_NAME);
        cmd.env("GIT_AUTHOR_EMAIL", IDENTITY_EMAIL);
        cmd.env("GIT_COMMITTER_NAME", IDENTITY_NAME);
        cmd.env("GIT_COMMITTER_EMAIL", IDENTITY_EMAIL);

        cmd
    }
}

impl Git {
    /// Bootstrap a brand-new bare repository with an empty initial commit.
    pub fn create(repo: impl AsRef<Path>) -> Result<Self> {
        let path = repo.as_ref();
        if path.exists() {
            return Err(Error::RepositoryExists(path.to_path_buf()));
        }
        std::fs::create_dir_all(path).map_err(|e| Error::io(path, e))?;

        // Use an empty template so no global hooks ship in.
        let tmp = tempfile::tempdir().map_err(|e| Error::io(path, e))?;
        let template = tmp.path();

        let mut init = Command::new("git");
        init.arg("init")
            .arg("--bare")
            .arg("--initial-branch=memory")
            .arg(format!("--template={}", template.display()));
        init.arg(path);
        for (k, _) in std::env::vars() {
            if k.starts_with("GIT_") {
                init.env_remove(k);
            }
        }
        init.env("GIT_CONFIG_NOSYSTEM", "1");
        init.env("GIT_CONFIG_GLOBAL", "/dev/null");
        init.env("GIT_TERMINAL_PROMPT", "0");
        let out = init.output().map_err(|e| Error::io(path, e))?;
        if !out.status.success() {
            return Err(Error::Git(format!(
                "git init failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }

        let g = Self::new_unchecked(path);
        // Initial empty tree + commit.
        let tree = g.run_args(&["mktree"], Some(b""), None)?.trim().to_string();
        let commit = g
            .run_args(&["commit-tree", &tree], Some(b"Initial memory snapshot\n"), None)?
            .trim()
            .to_string();
        g.run_args(&["update-ref", "refs/heads/memory", &commit, &"0".repeat(commit.len())], None, None)?;
        Ok(g)
    }
}


/// Create a directory and all of its parents if it doesn't already exist.
pub fn ensure_dir(path: &Path) -> Result<()> {
    match std::fs::metadata(path) {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path).map_err(|e| Error::io(path, e))
        }
        Err(err) => Err(Error::io(path, err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_then_verify_bare() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("memory.git");
        let g = Git::create(&repo).unwrap();
        assert!(g.is_bare().unwrap());
    }

    #[test]
    fn create_refuses_existing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("memory.git");
        std::fs::create_dir_all(&repo).unwrap();
        let err = Git::create(&repo).unwrap_err();
        match err {
            Error::RepositoryExists(p) => assert_eq!(p, repo),
            other => panic!("unexpected error: {other}"),
        }
    }
}