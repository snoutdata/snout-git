//! Checking out branches, tags and commits; creating and deleting branches.

use serde::{Deserialize, Serialize};

use crate::git::run_raw;
use crate::{Branch, BranchKind, Error, Repo, Result};

/// Something that can be checked out.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum CheckoutTarget {
    /// A local branch, e.g. `main`.
    Branch(String),
    /// A remote-tracking branch, e.g. `origin/feature`. Checks out the local branch of
    /// the same name, creating it (tracking the remote one) if needed.
    RemoteBranch(String),
    /// A tag. Results in a detached HEAD.
    Tag(String),
    /// Any commit-ish (`a1b2c3d`, `HEAD~2`, ...). Results in a detached HEAD.
    Commit(String),
}

impl CheckoutTarget {
    pub fn label(&self) -> String {
        match self {
            Self::Branch(n) | Self::RemoteBranch(n) => n.clone(),
            Self::Tag(t) => format!("tag {t}"),
            Self::Commit(c) => format!("commit {c}"),
        }
    }

    /// Whether checking this out leaves HEAD detached.
    pub fn detaches(&self) -> bool {
        matches!(self, Self::Tag(_) | Self::Commit(_))
    }
}

impl From<&Branch> for CheckoutTarget {
    fn from(b: &Branch) -> Self {
        match b.kind {
            BranchKind::Local => Self::Branch(b.name.clone()),
            BranchKind::Remote => Self::RemoteBranch(b.name.clone()),
        }
    }
}

/// What to do with uncommitted changes when checking out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalChanges {
    /// Carry them over; git refuses if they would be overwritten.
    #[default]
    Keep,
    /// Stash them (including untracked files), check out, then re-apply them.
    Stash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckoutOutcome {
    pub output: String,
    /// Set when the checkout succeeded but re-applying stashed changes conflicted.
    /// The stash is then kept, so nothing is lost.
    pub warning: Option<String>,
}

impl Repo {
    pub fn checkout(&self, target: &CheckoutTarget, local: LocalChanges) -> Result<CheckoutOutcome> {
        let args = self.checkout_args(target)?;
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let stashed = match local {
            LocalChanges::Keep => false,
            LocalChanges::Stash => self.stash_push(&format!("snout: before checking out {}", target.label()))?,
        };

        let out = match self.git(&args) {
            Ok(out) => out,
            Err(e) => {
                if stashed {
                    // Nothing moved, so the stash applies cleanly where it came from.
                    let _ = self.git(&["stash", "pop", "-q"]);
                }
                return Err(e);
            }
        };

        let mut outcome = CheckoutOutcome { output: out.text(), warning: None };
        if stashed {
            let pop = run_raw(self.root(), &["stash", "pop"], None)?;
            if !pop.success {
                outcome.warning = Some(format!(
                    "Checked out {}, but re-applying your local changes conflicted.\n\
                     Resolve the conflicts; your changes are still saved as stash@{{0}}.\n\n{}",
                    target.label(),
                    pop.text()
                ));
            }
        }
        Ok(outcome)
    }

    fn checkout_args(&self, target: &CheckoutTarget) -> Result<Vec<String>> {
        let v = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        Ok(match target {
            CheckoutTarget::Branch(name) => v(&["switch", name]),
            CheckoutTarget::RemoteBranch(name) => {
                let (_, local) =
                    name.split_once('/').ok_or_else(|| Error::Invalid(format!("`{name}` is not a remote branch")))?;
                if self.ref_exists(&format!("refs/heads/{local}"))? {
                    v(&["switch", local])
                } else {
                    v(&["switch", "--track", name])
                }
            }
            CheckoutTarget::Tag(tag) => v(&["switch", "--detach", &format!("refs/tags/{tag}")]),
            CheckoutTarget::Commit(rev) => v(&["switch", "--detach", &self.resolve_commit(rev)?]),
        })
    }

    /// Stash all local changes, including untracked files. Returns whether anything
    /// was stashed (there may have been nothing to stash).
    fn stash_push(&self, message: &str) -> Result<bool> {
        let before = self.stash_top()?;
        self.git(&["stash", "push", "--include-untracked", "-m", message])?;
        Ok(self.stash_top()? != before)
    }

    fn stash_top(&self) -> Result<Option<String>> {
        let out = run_raw(self.root(), &["rev-parse", "-q", "--verify", "refs/stash"], None)?;
        Ok(out.success.then(|| out.stdout_str().trim().to_owned()))
    }

    pub(crate) fn ref_exists(&self, full_ref: &str) -> Result<bool> {
        Ok(run_raw(self.root(), &["show-ref", "--verify", "-q", full_ref], None)?.success)
    }

    /// Full commit id for a commit-ish.
    pub fn resolve_commit(&self, rev: &str) -> Result<String> {
        let spec = format!("{}^{{commit}}", rev.trim());
        let out = run_raw(self.root(), &["rev-parse", "--verify", "-q", "--end-of-options", &spec], None)?;
        if out.success && !rev.trim().is_empty() {
            Ok(out.stdout_str().trim().to_owned())
        } else {
            Err(Error::Invalid(format!("`{}` is not a commit", rev.trim())))
        }
    }

    /// `a1b2c3d Subject line` for a commit-ish, for previews.
    pub fn describe_commit(&self, rev: &str) -> Result<String> {
        let id = self.resolve_commit(rev)?;
        Ok(self.git(&["log", "-1", "--format=%h %s", &id])?.stdout_str().trim().to_owned())
    }

    /// Create a branch at `start` (default HEAD), optionally checking it out. Starting
    /// from a remote branch sets it as the upstream.
    pub fn create_branch(&self, name: &str, start: Option<&str>, checkout: bool) -> Result<()> {
        let name = name.trim();
        crate::git::run(self.root(), &["check-ref-format", "--branch", name])
            .map_err(|_| Error::Invalid(format!("`{name}` is not a valid branch name")))?;
        let mut args = if checkout { vec!["switch", "-c", name] } else { vec!["branch", name] };
        if let Some(start) = start {
            args.push(start);
        }
        self.git(&args)?;
        Ok(())
    }

    /// Delete a local branch. Without `force`, git refuses if it is not fully merged.
    pub fn delete_branch(&self, name: &str, force: bool) -> Result<()> {
        self.git(&["branch", if force { "-D" } else { "-d" }, name])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::run;
    use std::fs;

    fn repo_with_branches(name: &str) -> Repo {
        let dir = std::env::temp_dir().join(format!("snout-checkout-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let repo = Repo::init(&dir).unwrap();
        for args in [
            ["config", "user.name", "Test"],
            ["config", "user.email", "t@example.com"],
            ["config", "commit.gpgsign", "false"],
            ["config", "tag.gpgsign", "false"],
        ] {
            run(&dir, &args).unwrap();
        }
        run(&dir, &["checkout", "-q", "-b", "main"]).unwrap();
        fs::write(dir.join("f.txt"), "base\n").unwrap();
        repo.stage(&["f.txt".into()]).unwrap();
        repo.commit("base", false).unwrap();
        run(&dir, &["tag", "-a", "v1", "-m", "Version 1"]).unwrap();
        repo.create_branch("other", None, true).unwrap();
        fs::write(dir.join("f.txt"), "other\n").unwrap();
        repo.stage(&["f.txt".into()]).unwrap();
        repo.commit("other change", false).unwrap();
        repo.checkout(&CheckoutTarget::Branch("main".into()), LocalChanges::Keep).unwrap();
        repo
    }

    fn head(repo: &Repo) -> Option<String> {
        repo.status().unwrap().branch.head
    }

    #[test]
    fn stash_mode_carries_conflicting_changes() {
        let repo = repo_with_branches("stash");
        let f = repo.root().join("f.txt");
        fs::write(&f, "base\nlocal edit\n").unwrap();
        fs::write(repo.root().join("new.txt"), "untracked\n").unwrap();

        // `other` changes f.txt too, so a plain checkout must refuse...
        let other = CheckoutTarget::Branch("other".into());
        assert!(repo.checkout(&other, LocalChanges::Keep).is_err());
        assert_eq!(head(&repo).as_deref(), Some("main"));
        assert_eq!(fs::read_to_string(&f).unwrap(), "base\nlocal edit\n");

        // ...while stash mode switches and re-applies (here: with a conflict, stash kept).
        let outcome = repo.checkout(&other, LocalChanges::Stash).unwrap();
        assert_eq!(head(&repo).as_deref(), Some("other"));
        assert!(outcome.warning.is_some(), "edit conflicts with `other`");
        assert!(repo.stash_top().unwrap().is_some(), "stash kept on conflict");
        fs::remove_dir_all(repo.root()).unwrap();
    }

    #[test]
    fn stash_mode_reapplies_cleanly() {
        let repo = repo_with_branches("clean");
        fs::write(repo.root().join("new.txt"), "untracked\n").unwrap();
        let outcome = repo.checkout(&CheckoutTarget::Branch("other".into()), LocalChanges::Stash).unwrap();
        assert_eq!(outcome.warning, None);
        assert!(repo.root().join("new.txt").exists());
        assert!(repo.stash_top().unwrap().is_none(), "stash popped");
        fs::remove_dir_all(repo.root()).unwrap();
    }

    #[test]
    fn tags_commits_and_branch_management() {
        let repo = repo_with_branches("tags");
        let tags = repo.tags().unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].subject, "Version 1");

        repo.checkout(&CheckoutTarget::Tag("v1".into()), LocalChanges::Keep).unwrap();
        assert_eq!(head(&repo), None, "detached at tag");
        assert!(repo.describe_commit("HEAD").unwrap().ends_with("base"));

        repo.checkout(&CheckoutTarget::Commit("other".into()), LocalChanges::Keep).unwrap();
        assert_eq!(head(&repo), None);
        assert!(repo.resolve_commit("no-such-rev").is_err());
        assert!(repo.resolve_commit("--help").is_err());

        repo.create_branch("from-tag", Some("v1"), false).unwrap();
        repo.checkout(&CheckoutTarget::Branch("main".into()), LocalChanges::Keep).unwrap();
        repo.delete_branch("from-tag", false).unwrap();
        assert!(repo.delete_branch("other", false).is_err(), "unmerged needs force");
        repo.delete_branch("other", true).unwrap();
        assert_eq!(repo.branches().unwrap().len(), 1);
        fs::remove_dir_all(repo.root()).unwrap();
    }
}
