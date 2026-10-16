//! Git operations for snout.
//!
//! Everything here shells out to the `git` CLI so that the user's own configuration
//! (credential helpers, SSH agent, hooks, `pull.rebase`, ...) applies unchanged.
//!
//! The API is deliberately front-end agnostic: all results are plain `serde`-serializable
//! values, so the same calls can back the desktop GUI and an MCP server.

mod branches;
mod checkout;
mod diff;
mod discover;
mod error;
mod git;
mod github;
mod log;
mod pull;
mod repo;
mod status;
mod tags;

pub use branches::{Branch, BranchKind};
pub use checkout::{CheckoutOutcome, CheckoutTarget, LocalChanges};
pub use diff::{DiffBase, DiffRow, FileDiff, RowKind};
pub use discover::find_repos;
pub use error::{Error, Result};
pub use github::github_web_url;
pub use log::Commit;
pub use pull::{PullMode, PullProblem};
pub use repo::Repo;
pub use status::{BranchInfo, Change, FileEntry, Status};
pub use tags::Tag;
