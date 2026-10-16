# snout-git

A small, fast desktop Git client in Rust. It focuses on the everyday loop: see what changed, read the diff, stage, commit, switch branches, push. It also runs Claude Code in a docked console, so an agent can work in the repository while you watch the changes appear.

![snout-git main window](docs/screenshot.png)

## Features

- **Repositories:** a list of your repositories, with a filter. The open repository expands into its directory tree; pick a folder to see only the files under it.
- **Opening:** ⌘O, the welcome screen, or drag a folder onto the window. A folder that is not a repository can be searched for repositories (4 levels deep) or initialized. Clone from a URL.
- **Files:** working tree status (staged, partly staged, untracked, conflicted), with a filter, sortable columns, multi-select (⌘-click, ⇧-click, ⌘A) and a context menu.
- **Changes:** whole-file diff, side by side or unified, with an optional compact view (3 lines of context). Compare HEAD with the working tree, the index with the working tree, or HEAD with the index.
- **Commit:** message and amend (pre-fills the last message); ⌘↩ commits.
- **Check out:** a ⇧⌘O picker over local branches, remote branches and tags, or any commit you type (a SHA, `HEAD~2`), with a live preview. With uncommitted changes you choose to keep them or to stash, check out and re-apply. If re-applying conflicts, the stash is kept so nothing is lost.
- **Branches:** local branches, branches per remote, and tags, with ahead/behind counts and a detached HEAD indicator. Create a branch from any branch, tag or commit; delete one (with a force option).
- **Journal:** recent commits with branch and tag badges and unpushed markers. Right-click a commit to check it out or start a branch there.
- **Remote:** pull, fetch (all remotes, with prune) and push. The first push sets the upstream. When a pull cannot combine the branches, snout says why and offers a way forward: merge or rebase when both sides have new commits, or reset to the remote (keeping your commits on a backup branch) when the remote's history was replaced.
- **GitHub:** opens the repository on github.com.
- **Claude Code console:** see below.

The view re-reads the repository when the window regains focus, on F5 or ⌘R, and while the console's agent is working.

### Claude Code console

![Claude Code in the console](docs/claude-console.png)

Press ⌘J or click **✳ Claude** to run [Claude Code](https://docs.claude.com/en/docs/claude-code) in the open repository, docked on the right.

- There is one session per repository. Sessions keep running when you switch repositories or hide the panel.
- **New** starts a fresh conversation, **Continue** resumes the latest one in that folder (`claude --continue`), and **End** stops the session.
- The Git panes refresh once the agent goes quiet, and every 3 seconds while it keeps working.
- The console is a real pseudo-terminal, so Claude Code behaves as it does in your terminal: Shift+Tab cycles modes, Esc interrupts, Shift+Enter or Option+Enter adds a newline, Ctrl+V pastes an image, ⌘V pastes text, and you can drag to select and press ⌘C to copy.

snout looks for `claude` on `PATH`, then in `~/.local/bin`, `~/.claude/local`, `/opt/homebrew/bin` and `/usr/local/bin`. Claude Code itself is installed and signed in separately.

## Building

### Requirements

- **Git** on `PATH`. Push, pull and fetch use your normal Git credentials (the osxkeychain helper, an SSH agent, `gh auth setup-git`, and so on).
- **Rust 1.95 or newer.** Install it with [rustup](https://rustup.rs):

  ```sh
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

- **Platform:** developed and tested on macOS. Linux and Windows should build, since every dependency supports them, but they are untested. On Linux you need the system libraries that [eframe](https://github.com/emilk/egui/tree/main/crates/eframe) requires (X11 or Wayland, xkbcommon, OpenGL).
- **Optional:** [Claude Code](https://docs.claude.com/en/docs/claude-code), for the console.

### Build and run

```sh
git clone https://github.com/snoutdata/snout-git.git
cd snout-git
cargo build --release
./target/release/snout                 # reopen what you had last time
./target/release/snout ~/dev/my-repo   # open a repository (any folder inside it works)
```

To put `snout` on your `PATH`:

```sh
cargo install --path crates/snout-gui
```

### The launcher script

On macOS and Linux, `snout.sh` rebuilds the release binary when the sources have changed and starts the app in the background:

```sh
./snout.sh                  # reopen what you had last time
./snout.sh ~/dev/my-repo    # open a repository
./snout.sh ~/dev            # not a repository: find repositories inside it, or git init
./snout.sh --pick           # choose a repository with a folder dialog (macOS)
./snout.sh --fg ...         # stay in the foreground, with logs in the terminal
```

In the background, logs go to `$TMPDIR/snout.log`.

## Keyboard shortcuts

| Shortcut | Action |
|---|---|
| ⌘O | Open a local repository |
| ⇧⌘O | Check out a branch, tag or commit |
| ⌘J | Show or hide the Claude Code console |
| F5 or ⌘R | Refresh |
| ⌘A | Select all files (when no text field has focus) |
| ⌘↩ | Commit (in the message field) |

On Windows and Linux, use Ctrl in place of ⌘.

## How it is built

```
crates/
  snout-core/   Git operations: no UI, every result is serde::Serialize
  snout-gui/    the eframe/egui desktop app (binary: snout)
```

`snout-core` runs the `git` CLI instead of using libgit2, so authentication, hooks and settings such as `pull.rebase` behave exactly as they do in a terminal. Read-only calls set `GIT_OPTIONAL_LOCKS=0` so they don't contend with other processes for `index.lock`, and `GIT_TERMINAL_PROMPT=0` so nothing waits on a hidden prompt. Network operations run on a worker thread.

The console is `portable-pty` plus the `vt100` parser, painted with egui. It answers the start-up queries terminal apps send (cursor position, device attributes, background colour). Symbols missing from egui's bundled fonts fall back to the system's (Apple Symbols and Menlo on macOS, DejaVu Sans Mono on Linux, Segoe UI Symbol on Windows).

## Roadmap: MCP

`snout-core` is meant to be driven by agents too. A planned `snout-mcp` crate (stdio) will map MCP tools one to one onto `snout_core::Repo`:

| Tool | Core call |
|---|---|
| `status` | `Repo::status` |
| `diff` | `Repo::file_diff` |
| `stage`, `unstage`, `discard` | same names |
| `commit` | `Repo::commit` |
| `branches`, `tags`, `checkout`, `create_branch`, `delete_branch` | same names |
| `log` | `Repo::log` |
| `fetch`, `pull`, `push`, `reset_to_upstream` | same names (`pull_with` takes merge or rebase) |

Since the GUI re-reads state from Git rather than caching it, changes an agent makes show up in the GUI. Console sessions will get these tools through `claude --mcp-config`.

Also on the list: staging individual hunks, a file watcher, and merge, rebase, stash and blame.

## Tests

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all --check
```

- The core tests run against real scratch repositories, including a bare "remote" for push, fetch and pull.
- The GUI tests render the app headlessly with `egui_kittest` and drive it with clicks and key presses. Set `SNOUT_SCREENSHOT=/tmp/shot.png` to save PNGs of the renders.
- One test checks that every character used in the UI exists in the fonts.
- The console tests run a real process on a pseudo-terminal. An opt-in test renders the real Claude Code without sending a prompt:

  ```sh
  SNOUT_REAL_CLAUDE=1 cargo test -p snout-gui real_claude -- --ignored
  ```

## Contributing

Issues and pull requests are welcome. Please run the three commands under [Tests](#tests) before opening a pull request.

## License

[MIT](LICENSE), copyright (c) 2026 SNOUTDATA INC.
