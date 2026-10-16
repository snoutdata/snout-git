//! Pulling, and the two ways it commonly fails: branches that both moved on, and a
//! remote whose history was replaced.

use serde::{Deserialize, Serialize};

use crate::{Error, Repo, Result};

/// How `pull` combines remote commits with local ones.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullMode {
    /// Whatever the repository's configuration says (`pull.rebase`, `pull.ff`).
    #[default]
    Default,
    /// `git pull --no-rebase`: a merge commit when both sides moved on.
    Merge,
    /// `git pull --rebase`: replay local commits on top of the remote ones.
    Rebase,
}

/// Why a pull could not combine the branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PullProblem {
    /// Both sides have new commits and the configuration does not say how to combine them.
    Divergent,
    /// The branches share no commit, typically because the remote repository was
    /// recreated or its history rewritten.
    Unrelated,
}

impl PullProblem {
    /// Recognise the problem from git's error message (git runs with `LC_ALL=C`).
    pub fn from_error(message: &str) -> Option<Self> {
        if message.contains("refusing to merge unrelated histories") {
            Some(Self::Unrelated)
        } else if message.contains("divergent branches") {
            Some(Self::Divergent)
        } else {
            None
        }
    }
}

impl Repo {
    pub fn pull(&self) -> Result<String> {
        self.pull_with(PullMode::Default)
    }

    pub fn pull_with(&self, mode: PullMode) -> Result<String> {
        let args: &[&str] = match mode {
            PullMode::Default => &["pull"],
            PullMode::Merge => &["pull", "--no-rebase"],
            PullMode::Rebase => &["pull", "--rebase"],
        };
        Ok(self.git(args)?.text())
    }

    /// Make the current branch match its upstream exactly (`reset --hard @{upstream}`),
    /// first saving its tip as `backup/<branch>-<short id>` so no commit is lost.
    /// Refuses while tracked files have uncommitted changes; untracked files are kept.
    /// Returns the backup branch name.
    pub fn reset_to_upstream(&self) -> Result<String> {
        let status = self.status()?;
        let head = status.branch.head.clone().ok_or_else(|| Error::Invalid("HEAD is detached".into()))?;
        let upstream =
            status.branch.upstream.clone().ok_or_else(|| Error::Invalid(format!("`{head}` has no upstream")))?;
        if status.files.iter().any(|f| !f.is_untracked()) {
            return Err(Error::Invalid("Commit or stash your changes first; resetting would discard them.".into()));
        }
        let tip = self.git(&["rev-parse", "--short", "HEAD"])?.stdout_str().trim().to_owned();
        let backup = format!("backup/{head}-{tip}");
        if !self.ref_exists(&format!("refs/heads/{backup}"))? {
            self.git(&["branch", &backup, "HEAD"])?;
        }
        self.git(&["reset", "--hard", &format!("refs/remotes/{upstream}")])?;
        Ok(backup)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::run;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("snout-pull-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn clone(remote: &Path, dir: &Path) -> Repo {
        run(remote.parent().unwrap(), &["clone", "-q", &remote.to_string_lossy(), &dir.to_string_lossy()]).unwrap();
        for args in [
            ["config", "user.name", "T"],
            ["config", "user.email", "t@example.com"],
            ["config", "commit.gpgsign", "false"],
        ] {
            run(dir, &args).unwrap();
        }
        Repo::open(dir).unwrap()
    }

    fn commit(repo: &Repo, file: &str, text: &str) {
        fs::write(repo.root().join(file), text).unwrap();
        repo.stage(&[file.into()]).unwrap();
        repo.commit(&format!("edit {file}"), false).unwrap();
    }

    #[test]
    fn classifies_git_errors() {
        let unrelated = "`git pull` failed: fatal: refusing to merge unrelated histories";
        assert_eq!(PullProblem::from_error(unrelated), Some(PullProblem::Unrelated));
        let divergent = "hint: You have divergent branches and need to specify how to reconcile them.\n\
                         fatal: Need to specify how to reconcile divergent branches.";
        assert_eq!(PullProblem::from_error(divergent), Some(PullProblem::Divergent));
        assert_eq!(PullProblem::from_error("fatal: could not read from remote"), None);
    }

    #[test]
    fn merge_rebase_and_reset_to_a_replaced_remote() {
        let root = scratch("modes");
        let remote = root.join("remote.git");
        run(&root, &["init", "-q", "--bare", "-b", "main", &remote.to_string_lossy()]).unwrap();
        fs::create_dir_all(root.join("a")).unwrap();
        let a = Repo::init(root.join("a")).unwrap();
        for args in [
            ["config", "user.name", "T"],
            ["config", "user.email", "t@example.com"],
            ["config", "commit.gpgsign", "false"],
        ] {
            run(a.root(), &args).unwrap();
        }
        run(a.root(), &["checkout", "-q", "-b", "main"]).unwrap();
        commit(&a, "base.txt", "base\n");
        run(a.root(), &["remote", "add", "origin", &remote.to_string_lossy()]).unwrap();
        a.push().unwrap();
        let b = clone(&remote, &root.join("b"));

        // Both sides move on: merge, then rebase.
        commit(&a, "a.txt", "a\n");
        a.push().unwrap();
        commit(&b, "b.txt", "b\n");
        b.pull_with(PullMode::Merge).unwrap();
        assert!(b.root().join("a.txt").exists());
        b.push().unwrap();
        commit(&b, "b2.txt", "b2\n");
        commit(&a, "a2.txt", "a2\n");
        a.pull_with(PullMode::Rebase).unwrap();
        assert!(a.root().join("b.txt").exists());
        assert_eq!(a.status().unwrap().branch.ahead, 1, "local commit replayed on top");

        // The remote is replaced by an unrelated history.
        fs::create_dir_all(root.join("c")).unwrap();
        let c = Repo::init(root.join("c")).unwrap();
        for args in [
            ["config", "user.name", "T"],
            ["config", "user.email", "t@example.com"],
            ["config", "commit.gpgsign", "false"],
        ] {
            run(c.root(), &args).unwrap();
        }
        run(c.root(), &["checkout", "-q", "-b", "main"]).unwrap();
        commit(&c, "new.txt", "new history\n");
        run(c.root(), &["push", "-q", "--force", &remote.to_string_lossy(), "main"]).unwrap();

        b.fetch().unwrap();
        let err = b.pull_with(PullMode::Merge).unwrap_err().to_string();
        assert_eq!(PullProblem::from_error(&err), Some(PullProblem::Unrelated), "{err}");

        fs::write(b.root().join("b.txt"), "dirty\n").unwrap();
        assert!(b.reset_to_upstream().is_err(), "refuses with uncommitted changes");
        run(b.root(), &["checkout", "--", "b.txt"]).unwrap();
        let old_tip = b.resolve_commit("HEAD").unwrap();
        let backup = b.reset_to_upstream().unwrap();
        assert_eq!(b.resolve_commit(&backup).unwrap(), old_tip, "old commits kept");
        assert_eq!(b.resolve_commit("HEAD").unwrap(), b.resolve_commit("origin/main").unwrap());
        assert!(b.root().join("new.txt").exists() && !b.root().join("b.txt").exists());
        fs::remove_dir_all(&root).unwrap();
    }
}
