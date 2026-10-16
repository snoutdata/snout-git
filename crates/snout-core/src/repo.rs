use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::git::{run, run_raw, run_with_stdin};
use crate::status::FileEntry;
use crate::{Branch, Commit, DiffBase, Error, FileDiff, Result, Status, Tag};
use crate::{branches, diff, log, status, tags};

/// A Git working tree. Cheap to clone and `Send`, so it can be moved to worker threads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Repo {
    root: PathBuf,
}

impl Repo {
    /// Open the working tree containing `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.is_dir() {
            return Err(Error::NotARepo(path.to_owned()));
        }
        let out = run_raw(path, &["rev-parse", "--show-toplevel"], None)?;
        if !out.success {
            return Err(Error::NotARepo(path.to_owned()));
        }
        Ok(Self { root: PathBuf::from(out.stdout_str().trim_end()) })
    }

    /// Create a new repository in the existing directory `path`.
    pub fn init(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.is_dir() {
            return Err(Error::Invalid(format!("{} is not a folder", path.display())));
        }
        run(path, &["init", "-q"])?;
        Self::open(path)
    }

    /// Clone `url` into `dest` (which must not exist or be empty). Network operation.
    pub fn clone_from(url: &str, dest: &Path) -> Result<(Self, String)> {
        let parent = dest.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let dest_str = dest.to_string_lossy();
        let out = run(parent, &["clone", "--progress", url, &dest_str])?;
        Ok((Self::open(dest)?, out.text()))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn name(&self) -> String {
        self.root.file_name().map_or_else(|| self.root.display().to_string(), |n| n.to_string_lossy().into_owned())
    }

    pub(crate) fn git(&self, args: &[&str]) -> Result<crate::git::Output> {
        run(&self.root, args)
    }

    // ----- Queries ---------------------------------------------------------------------

    pub fn status(&self) -> Result<Status> {
        let out = self.git(&["status", "--porcelain=v2", "-z", "--branch", "--untracked-files=all"])?;
        status::parse(&out.stdout)
    }

    pub fn branches(&self) -> Result<Vec<Branch>> {
        let out = self.git(&["for-each-ref", branches::FORMAT, "refs/heads", "refs/remotes"])?;
        Ok(branches::parse(&out.stdout_str()))
    }

    /// Tags, newest first.
    pub fn tags(&self) -> Result<Vec<Tag>> {
        let out = self.git(&["for-each-ref", "--sort=-creatordate", tags::FORMAT, "refs/tags"])?;
        Ok(tags::parse(&out.stdout_str()))
    }

    /// Most recent commits reachable from HEAD, newest first.
    pub fn log(&self, limit: usize) -> Result<Vec<Commit>> {
        let n = format!("-n{limit}");
        let out = run_raw(&self.root, &["log", &n, log::FORMAT, log::DATE, "HEAD"], None)?;
        if !out.success {
            // An unborn branch has no history; that is not an error.
            return if self.has_head()? { Err(git_error("log", &out)) } else { Ok(Vec::new()) };
        }
        let mut commits = log::parse(&out.stdout_str());

        let unpushed = run_raw(&self.root, &["rev-list", "@{upstream}..HEAD"], None)?;
        if unpushed.success {
            let ids = unpushed.stdout_str();
            let ids: std::collections::HashSet<&str> = ids.lines().collect();
            for c in &mut commits {
                c.unpushed = ids.contains(c.id.as_str());
            }
        } else {
            // No upstream: everything on this branch is unpushed.
            commits.iter_mut().for_each(|c| c.unpushed = true);
        }
        Ok(commits)
    }

    /// Every directory of the working tree that holds a tracked or untracked (not ignored)
    /// file, as repository-relative paths with `/` separators, sorted. The root is not
    /// included.
    pub fn directories(&self) -> Result<Vec<String>> {
        let out = self.git(&["ls-files", "-z", "--cached", "--others", "--exclude-standard"])?;
        let mut dirs = std::collections::BTreeSet::new();
        for path in out.stdout_str().split('\0') {
            let mut end = path.len();
            while let Some(slash) = path[..end].rfind('/') {
                if !dirs.insert(path[..slash].to_owned()) {
                    break; // its ancestors are in already
                }
                end = slash;
            }
        }
        Ok(dirs.into_iter().collect())
    }

    pub fn remotes(&self) -> Result<Vec<(String, String)>> {
        let out = self.git(&["remote", "-v"])?;
        let mut remotes: Vec<(String, String)> = out
            .stdout_str()
            .lines()
            .filter(|l| l.ends_with("(fetch)"))
            .filter_map(|l| {
                let mut parts = l.split_whitespace();
                Some((parts.next()?.to_owned(), parts.next()?.to_owned()))
            })
            .collect();
        remotes.dedup();
        Ok(remotes)
    }

    /// The GitHub web page for the `origin` remote (or the first GitHub remote).
    pub fn github_url(&self) -> Option<String> {
        let remotes = self.remotes().ok()?;
        let origin = remotes.iter().find(|(name, _)| name == "origin");
        origin.into_iter().chain(remotes.iter()).find_map(|(_, url)| crate::github_web_url(url))
    }

    pub fn last_commit_message(&self) -> Result<String> {
        Ok(self.git(&["log", "-1", "--format=%B"])?.stdout_str().trim_end().to_owned())
    }

    /// Full-file diff of one path. For renames, pass the entry so the old side uses the
    /// original path.
    pub fn file_diff(&self, entry: &FileEntry, base: DiffBase) -> Result<FileDiff> {
        let old_path = entry.orig_path.as_deref().unwrap_or(&entry.path);
        let (old, new) = match base {
            DiffBase::HeadToWorktree => (self.blob("HEAD", old_path)?, self.worktree_file(&entry.path)?),
            DiffBase::IndexToWorktree => (self.blob("", &entry.path)?, self.worktree_file(&entry.path)?),
            DiffBase::HeadToIndex => (self.blob("HEAD", old_path)?, self.blob("", &entry.path)?),
        };
        Ok(diff::compute(entry.path.clone(), base, &old, &new))
    }

    /// Contents of `path` at `rev` (`""` = index). Missing objects yield empty content.
    fn blob(&self, rev: &str, path: &str) -> Result<Vec<u8>> {
        let spec = format!("{rev}:{path}");
        let out = run_raw(&self.root, &["cat-file", "blob", &spec], None)?;
        Ok(if out.success { out.stdout } else { Vec::new() })
    }

    fn worktree_file(&self, path: &str) -> Result<Vec<u8>> {
        let full = self.root.join(path);
        match std::fs::metadata(&full) {
            Ok(m) if m.is_file() && m.len() as usize > diff::MAX_BYTES => {
                // Report as too large without reading it all into memory.
                Ok(vec![b' '; diff::MAX_BYTES + 1])
            }
            Ok(m) if m.is_file() => Ok(std::fs::read(full)?),
            Ok(_) => Ok(Vec::new()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    pub(crate) fn has_head(&self) -> Result<bool> {
        Ok(run_raw(&self.root, &["rev-parse", "--verify", "-q", "HEAD"], None)?.success)
    }

    // ----- Working tree operations -----------------------------------------------------

    pub fn stage(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        self.git(&with_paths(&["add", "-A"], paths))?;
        Ok(())
    }

    pub fn unstage(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        if self.has_head()? {
            self.git(&with_paths(&["restore", "--staged"], paths))?;
        } else {
            self.git(&with_paths(&["rm", "--cached", "-r", "-q"], paths))?;
        }
        Ok(())
    }

    /// Revert entries to their HEAD state. Untracked files are deleted; files that are
    /// only added to the index are un-added (and so become untracked) rather than deleted.
    pub fn discard(&self, entries: &[FileEntry]) -> Result<()> {
        let mut restore = Vec::new();
        let mut unadd = Vec::new();
        for e in entries {
            if e.is_untracked() {
                std::fs::remove_file(self.root.join(&e.path))?;
                continue;
            }
            match e.index {
                Some(crate::Change::Added) => unadd.push(e.path.clone()),
                Some(crate::Change::Renamed | crate::Change::Copied) => {
                    unadd.push(e.path.clone());
                    if let Some(orig) = &e.orig_path {
                        restore.push(orig.clone());
                    }
                }
                _ => restore.push(e.path.clone()),
            }
        }
        if !unadd.is_empty() {
            self.git(&with_paths(&["rm", "--cached", "-q", "-f"], &unadd))?;
        }
        if !restore.is_empty() {
            self.git(&with_paths(&["restore", "--source=HEAD", "--staged", "--worktree"], &restore))?;
        }
        Ok(())
    }

    /// Commit the index. Returns git's summary output.
    pub fn commit(&self, message: &str, amend: bool) -> Result<String> {
        if message.trim().is_empty() {
            return Err(Error::Invalid("commit message is empty".into()));
        }
        let mut args = vec!["commit", "-q", "-F", "-"];
        if amend {
            args.push("--amend");
        }
        run_with_stdin(&self.root, &args, Some(message.as_bytes()))?;
        Ok(self.git(&["log", "-1", "--format=%h %s"])?.stdout_str().trim_end().to_owned())
    }

    // ----- Network operations (slow; call off the UI thread) ---------------------------

    pub fn fetch(&self) -> Result<String> {
        Ok(self.git(&["fetch", "--all", "--prune"])?.text())
    }

    /// Push the current branch, setting the upstream on first push.
    pub fn push(&self) -> Result<String> {
        let status = self.status()?;
        let Some(head) = status.branch.head else {
            return Err(Error::Invalid("HEAD is detached; check out a branch to push".into()));
        };
        if status.branch.upstream.is_some() {
            return Ok(self.git(&["push"])?.text());
        }
        let remotes = self.remotes()?;
        let remote = remotes
            .iter()
            .find(|(n, _)| n == "origin")
            .or(remotes.first())
            .map(|(n, _)| n.clone())
            .ok_or_else(|| Error::Invalid("no remote configured".into()))?;
        Ok(self.git(&["push", "-u", &remote, &head])?.text())
    }
}

fn with_paths<'a>(args: &[&'a str], paths: &'a [String]) -> Vec<&'a str> {
    let mut v = args.to_vec();
    v.push("--");
    v.extend(paths.iter().map(String::as_str));
    v
}

fn git_error(args: &str, out: &crate::git::Output) -> Error {
    Error::Git { args: args.into(), message: out.text() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch_repo(name: &str) -> Repo {
        let dir = std::env::temp_dir().join(format!("snout-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        run(&dir, &["init", "-q", "-b", "main"]).unwrap();
        run(&dir, &["config", "user.name", "Test"]).unwrap();
        run(&dir, &["config", "user.email", "test@example.com"]).unwrap();
        run(&dir, &["config", "commit.gpgsign", "false"]).unwrap();
        Repo::open(&dir).unwrap()
    }

    #[test]
    fn full_local_workflow() {
        let repo = scratch_repo("workflow");
        assert!(repo.log(10).unwrap().is_empty(), "unborn branch has empty log");

        fs::write(repo.root().join("a.txt"), "one\n").unwrap();
        let st = repo.status().unwrap();
        assert!(st.branch.is_unborn());
        assert_eq!(st.files.len(), 1);
        assert!(st.files[0].is_untracked());

        repo.stage(&["a.txt".into()]).unwrap();
        repo.unstage(&["a.txt".into()]).unwrap(); // unborn path: rm --cached
        repo.stage(&["a.txt".into()]).unwrap();
        assert_eq!(repo.status().unwrap().staged_count(), 1);

        let summary = repo.commit("first", false).unwrap();
        assert!(summary.ends_with("first"));
        assert!(repo.status().unwrap().files.is_empty());

        fs::write(repo.root().join("a.txt"), "one\ntwo\n").unwrap();
        let st = repo.status().unwrap();
        let d = repo.file_diff(&st.files[0], DiffBase::HeadToWorktree).unwrap();
        assert_eq!((d.additions, d.deletions), (1, 0));
        assert!(repo.file_diff(&st.files[0], DiffBase::HeadToIndex).unwrap().is_identical());

        repo.discard(&st.files).unwrap();
        assert!(repo.status().unwrap().files.is_empty());
        assert_eq!(fs::read_to_string(repo.root().join("a.txt")).unwrap(), "one\n");

        fs::create_dir_all(repo.root().join("src/deep/er")).unwrap();
        fs::write(repo.root().join("src/deep/er/x.rs"), "").unwrap();
        fs::write(repo.root().join(".gitignore"), "ignored/\n").unwrap();
        fs::create_dir_all(repo.root().join("ignored")).unwrap();
        fs::write(repo.root().join("ignored/y"), "").unwrap();
        assert_eq!(repo.directories().unwrap(), ["src", "src/deep", "src/deep/er"]);
        fs::remove_dir_all(repo.root().join("src")).unwrap();
        fs::remove_file(repo.root().join(".gitignore")).unwrap();
        fs::remove_dir_all(repo.root().join("ignored")).unwrap();

        repo.create_branch("feature/x", None, true).unwrap();
        let branches = repo.branches().unwrap();
        assert!(branches.iter().any(|b| b.name == "feature/x" && b.is_head));
        let main = branches.iter().find(|b| b.name == "main").unwrap();
        repo.checkout(&main.into(), crate::LocalChanges::Keep).unwrap();
        assert_eq!(repo.status().unwrap().branch.head.as_deref(), Some("main"));

        let log = repo.log(10).unwrap();
        assert_eq!(log.len(), 1);
        assert!(log[0].unpushed, "no upstream means unpushed");
        assert!(serde_json::to_string(&log).is_ok());

        fs::remove_dir_all(repo.root()).unwrap();
    }

    #[test]
    fn push_sets_upstream_then_fetch_and_pull() {
        let repo = scratch_repo("push");
        let remote = repo.root().with_extension("remote.git");
        let _ = fs::remove_dir_all(&remote);
        run(repo.root(), &["init", "-q", "--bare", "-b", "main", &remote.to_string_lossy()]).unwrap();
        run(repo.root(), &["remote", "add", "origin", &remote.to_string_lossy()]).unwrap();

        fs::write(repo.root().join("a.txt"), "one\n").unwrap();
        repo.stage(&["a.txt".into()]).unwrap();
        repo.commit("first", false).unwrap();
        assert_eq!(repo.status().unwrap().branch.upstream, None);

        repo.push().unwrap();
        let st = repo.status().unwrap();
        assert_eq!(st.branch.upstream.as_deref(), Some("origin/main"));
        assert!(repo.log(10).unwrap().iter().all(|c| !c.unpushed));

        repo.fetch().unwrap();
        repo.pull().unwrap();
        let remote_branch = repo.branches().unwrap().into_iter().find(|b| b.kind == crate::BranchKind::Remote).unwrap();
        assert_eq!(remote_branch.name, "origin/main");

        fs::remove_dir_all(repo.root()).unwrap();
        fs::remove_dir_all(&remote).unwrap();
    }
}
