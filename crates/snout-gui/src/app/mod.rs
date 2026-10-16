use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use eframe::egui::{self, Color32, Key, RichText, Sense};
use egui_extras::{Column, TableBuilder};
use serde::{Deserialize, Serialize};
use snout_core::{Branch, BranchKind, CheckoutTarget, Commit, DiffBase, FileEntry, LocalChanges, Repo, Status, Tag};

use crate::diff_view::DiffView;
use console::{Console, ConsoleEvent};
use dialogs::{CheckoutPicker, Dialog, StartPoint};

mod console;
mod dialogs;
#[cfg(test)]
mod tests;

const LOG_LIMIT: usize = 300;
/// How deep "Find repositories in folder…" searches.
const SCAN_DEPTH: usize = 4;

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    repos: Vec<PathBuf>,
    current: Option<PathBuf>,
    diff_base: DiffBase,
    side_by_side: bool,
    compact: bool,
    /// The Claude Code console is showing.
    console_open: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            repos: Vec::new(),
            current: None,
            diff_base: DiffBase::default(),
            side_by_side: true,
            compact: true,
            console_open: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortColumn {
    Name,
    State,
    Directory,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BottomTab {
    Journal,
    Output,
}

enum Action {
    Refresh,
    /// Open a repository from the list.
    OpenRepo(PathBuf),
    /// Ask for a folder, then [`Action::OpenPath`].
    PickFolder,
    /// Open a picked or dropped path; offers to initialize or search it if it is not a repository.
    OpenPath(PathBuf),
    InitRepo(PathBuf),
    PickScanFolder,
    Scan(PathBuf),
    RemoveRepo(PathBuf),
    ShowClone,
    Clone {
        url: String,
        dest: PathBuf,
    },
    Stage(Vec<String>),
    Unstage(Vec<String>),
    AskDiscard(Vec<FileEntry>),
    Discard(Vec<FileEntry>),
    Commit,
    ShowCheckout,
    /// Check out, asking first what to do with local changes if there are any.
    RequestCheckout(CheckoutTarget),
    Checkout(CheckoutTarget, LocalChanges),
    ShowNewBranch(Option<StartPoint>),
    CreateBranch {
        name: String,
        start: Option<String>,
        checkout: bool,
    },
    AskDeleteBranch(String),
    DeleteBranch {
        name: String,
        force: bool,
    },
    Fetch,
    Pull,
    Push,
    OpenGithub,
    ToggleConsole,
    Reveal(PathBuf),
    Copy(String),
}

enum JobOutcome {
    Done(String),
    Cloned(Repo, String),
    Found { root: PathBuf, repos: Vec<PathBuf> },
}

struct Job {
    label: String,
    rx: mpsc::Receiver<Result<JobOutcome, String>>,
}

struct OutputLine {
    text: String,
    error: bool,
}

pub struct SnoutApp {
    settings: Settings,
    repo: Option<Repo>,
    status: Status,
    branches: Vec<Branch>,
    tags: Vec<Tag>,
    log: Vec<Commit>,
    github_url: Option<String>,
    /// Working-tree directories, for the tree under the repository.
    dirs: Vec<String>,
    /// Directory picked in the tree; the Files pane shows only what is under it.
    dir_filter: Option<String>,

    repo_filter: String,
    selected: BTreeSet<String>,
    anchor: Option<String>,
    filter: String,
    sort: (SortColumn, bool),

    diff_key: Option<(String, DiffBase, bool)>,
    diff: Option<Result<DiffView, String>>,

    commit_msg: String,
    amend: bool,

    bottom_tab: BottomTab,
    output: Vec<OutputLine>,
    message: String,
    job: Option<Job>,
    dialog: Option<Dialog>,
    was_focused: bool,
    title: String,
    console: Console,
    /// The repository list and current repository as last written to disk.
    persisted: (Vec<PathBuf>, Option<PathBuf>),
}

impl SnoutApp {
    pub fn new(cc: &eframe::CreationContext<'_>, open_arg: Option<PathBuf>) -> Self {
        install_fonts(&cc.egui_ctx);
        let settings: Settings = cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
        let mut app = Self {
            settings,
            repo: None,
            status: Status::default(),
            branches: Vec::new(),
            tags: Vec::new(),
            log: Vec::new(),
            github_url: None,
            dirs: Vec::new(),
            dir_filter: None,
            repo_filter: String::new(),
            selected: BTreeSet::new(),
            anchor: None,
            filter: String::new(),
            sort: (SortColumn::Name, true),
            diff_key: None,
            diff: None,
            commit_msg: String::new(),
            amend: false,
            bottom_tab: BottomTab::Journal,
            output: Vec::new(),
            message: "Ready".into(),
            job: None,
            dialog: None,
            was_focused: true,
            title: String::new(),
            console: Console::default(),
            persisted: (Vec::new(), None),
        };
        app.persisted = (app.settings.repos.clone(), app.settings.current.clone());
        if let Some(path) = open_arg {
            app.open_path(&path);
        } else if let Some(path) = app.settings.current.clone() {
            app.open_repo(&path);
        }
        app
    }

    // ----- State ---------------------------------------------------------------------

    fn open_repo(&mut self, path: &Path) {
        match Repo::open(path) {
            Ok(repo) => {
                let root = repo.root().to_owned();
                self.remember(&root);
                self.settings.current = Some(root);
                self.github_url = repo.github_url();
                self.repo = Some(repo);
                self.selected.clear();
                self.anchor = None;
                self.dir_filter = None;
                self.commit_msg.clear();
                self.amend = false;
                self.refresh();
                self.message = format!("Opened {}", path.display());
            }
            Err(e) => self.fail(e.to_string()),
        }
    }

    /// Open a path chosen by the user. If it is not inside a repository, offer to
    /// initialize one there or to search it for repositories.
    fn open_path(&mut self, path: &Path) {
        let dir = if path.is_file() { path.parent().unwrap_or(path) } else { path };
        if Repo::open(dir).is_ok() {
            self.open_repo(dir);
        } else if dir.is_dir() {
            self.dialog = Some(Dialog::InitRepo(dir.to_owned()));
        } else {
            self.fail(format!("{} does not exist", dir.display()));
        }
    }

    fn remember(&mut self, root: &Path) {
        if !self.settings.repos.iter().any(|p| p == root) {
            self.settings.repos.push(root.to_owned());
        }
    }

    fn refresh(&mut self) {
        self.diff_key = None;
        let Some(repo) = self.repo.clone() else {
            self.status = Status::default();
            self.branches.clear();
            self.tags.clear();
            self.log.clear();
            self.dirs.clear();
            return;
        };
        match repo.status() {
            Ok(status) => {
                let present: BTreeSet<&String> = status.files.iter().map(|f| &f.path).collect();
                self.selected.retain(|p| present.contains(p));
                self.status = status;
            }
            Err(e) => return self.fail(e.to_string()),
        }
        self.branches = repo.branches().unwrap_or_else(|e| {
            self.log_output(e.to_string(), true);
            Vec::new()
        });
        self.tags = repo.tags().unwrap_or_else(|e| {
            self.log_output(e.to_string(), true);
            Vec::new()
        });
        self.log = repo.log(LOG_LIMIT).unwrap_or_else(|e| {
            self.log_output(e.to_string(), true);
            Vec::new()
        });
        self.dirs = repo.directories().unwrap_or_else(|e| {
            self.log_output(e.to_string(), true);
            Vec::new()
        });
        if self.dir_filter.as_ref().is_some_and(|d| self.dirs.binary_search(d).is_err()) {
            self.dir_filter = None;
        }
    }

    fn selected_entries(&self) -> Vec<FileEntry> {
        self.status.files.iter().filter(|f| self.selected.contains(&f.path)).cloned().collect()
    }

    /// Uncommitted changes to tracked files (untracked files never block a checkout
    /// unless the target contains the same path, which git reports anyway).
    fn has_tracked_changes(&self) -> bool {
        self.status.files.iter().any(|f| !f.is_untracked())
    }

    fn head_label(&self) -> String {
        let b = &self.status.branch;
        match (&b.head, &b.oid) {
            (Some(head), _) => head.clone(),
            (None, Some(oid)) => format!("detached at {}", &oid[..oid.len().min(7)]),
            (None, None) => "detached HEAD".into(),
        }
    }

    fn log_output(&mut self, text: String, error: bool) {
        if !text.trim().is_empty() {
            self.output.push(OutputLine { text, error });
        }
    }

    fn fail(&mut self, message: String) {
        self.message = message.lines().next().unwrap_or_default().to_owned();
        self.log_output(message.clone(), true);
        self.dialog = Some(Dialog::Error(message));
    }

    fn done(&mut self, summary: impl Into<String>) {
        self.message = summary.into();
        self.log_output(self.message.clone(), false);
    }

    /// Run a slow operation (network, filesystem scan) on a worker thread.
    fn spawn(
        &mut self,
        ctx: &egui::Context,
        label: &str,
        f: impl FnOnce() -> snout_core::Result<JobOutcome> + Send + 'static,
    ) {
        if self.job.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(f().map_err(|e| e.to_string()));
            ctx.request_repaint();
        });
        self.message = format!("{label}…");
        self.job = Some(Job { label: label.to_owned(), rx });
    }

    fn poll_job(&mut self) {
        let Some(job) = &self.job else { return };
        let Ok(result) = job.rx.try_recv() else { return };
        let label = job.label.clone();
        self.job = None;
        match result {
            Ok(JobOutcome::Done(text)) => {
                self.log_output(text, false);
                self.message = format!("{label} finished");
            }
            Ok(JobOutcome::Cloned(repo, text)) => {
                self.log_output(text, false);
                let root = repo.root().to_owned();
                self.open_repo(&root);
                self.message = format!("Cloned into {}", root.display());
            }
            Ok(JobOutcome::Found { root, repos }) => self.add_found(&root, repos),
            Err(e) => self.fail(format!("{label} failed:\n{e}")),
        }
        self.refresh();
    }

    fn add_found(&mut self, root: &Path, repos: Vec<PathBuf>) {
        if repos.is_empty() {
            self.dialog = Some(Dialog::Info {
                title: "No repositories found".into(),
                text: format!("No Git repositories within {SCAN_DEPTH} levels of\n{}", root.display()),
            });
            return;
        }
        let before = self.settings.repos.len();
        for r in &repos {
            self.remember(r);
        }
        let added = self.settings.repos.len() - before;
        if self.repo.is_none() {
            self.open_repo(&repos[0]);
        }
        self.done(format!("Found {} repositories in {} ({added} new)", repos.len(), root.display()));
    }

    fn handle(&mut self, action: Action, ctx: &egui::Context) {
        let repo = self.repo.clone();
        // Local operations run synchronously; they are fast and the result is needed
        // immediately for the next frame anyway.
        let local = |app: &mut Self, label: &str, r: snout_core::Result<()>| {
            match r {
                Ok(()) => app.done(label),
                Err(e) => app.fail(e.to_string()),
            }
            app.refresh();
        };
        match action {
            Action::Refresh => {
                self.refresh();
                self.message = "Refreshed".into();
            }
            Action::OpenRepo(path) => self.open_repo(&path),
            Action::PickFolder => {
                if let Some(dir) = rfd::FileDialog::new().set_title("Open local repository").pick_folder() {
                    self.open_path(&dir);
                }
            }
            Action::OpenPath(path) => self.open_path(&path),
            Action::InitRepo(path) => match Repo::init(&path) {
                Ok(repo) => {
                    let root = repo.root().to_owned();
                    self.open_repo(&root);
                    self.done(format!("Initialized repository in {}", root.display()));
                }
                Err(e) => self.fail(e.to_string()),
            },
            Action::PickScanFolder => {
                if let Some(dir) = rfd::FileDialog::new().set_title("Find repositories in folder").pick_folder() {
                    self.handle(Action::Scan(dir), ctx);
                }
            }
            Action::Scan(root) => self.spawn(ctx, "Searching for repositories", move || {
                let repos = snout_core::find_repos(&root, SCAN_DEPTH);
                Ok(JobOutcome::Found { root, repos })
            }),
            Action::RemoveRepo(path) => {
                self.settings.repos.retain(|p| p != &path);
                if self.settings.current.as_ref() == Some(&path) {
                    self.settings.current = None;
                    self.repo = None;
                    self.github_url = None;
                    self.refresh();
                }
            }
            Action::ShowClone => {
                let parent = self
                    .settings
                    .current
                    .as_ref()
                    .and_then(|p| p.parent())
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                self.dialog = Some(Dialog::Clone { url: String::new(), parent });
            }
            Action::Clone { url, dest } => self.spawn(ctx, "Clone", move || {
                Repo::clone_from(&url, &dest).map(|(repo, text)| JobOutcome::Cloned(repo, text))
            }),
            _ if repo.is_none() => {}
            Action::Stage(paths) => {
                let n = paths.len();
                local(self, &format!("Staged {n} file(s)"), repo.unwrap().stage(&paths));
            }
            Action::Unstage(paths) => {
                local(self, "Unstaged", repo.unwrap().unstage(&paths));
            }
            Action::AskDiscard(entries) => self.dialog = Some(Dialog::ConfirmDiscard(entries)),
            Action::Discard(entries) => {
                let n = entries.len();
                local(self, &format!("Discarded {n} file(s)"), repo.unwrap().discard(&entries));
            }
            Action::Commit => match repo.unwrap().commit(&self.commit_msg, self.amend) {
                Ok(summary) => {
                    self.done(format!("Committed {summary}"));
                    self.commit_msg.clear();
                    self.amend = false;
                    self.refresh();
                }
                Err(e) => self.fail(e.to_string()),
            },
            Action::ShowCheckout => self.dialog = Some(Dialog::Checkout(CheckoutPicker::default())),
            Action::RequestCheckout(target) => {
                if self.has_tracked_changes() {
                    self.dialog = Some(Dialog::ConfirmCheckout(target));
                } else {
                    self.handle(Action::Checkout(target, LocalChanges::Keep), ctx);
                }
            }
            Action::Checkout(target, mode) => {
                match repo.unwrap().checkout(&target, mode) {
                    Ok(outcome) => {
                        self.log_output(outcome.output, false);
                        self.done(format!("Checked out {}", target.label()));
                        if let Some(warning) = outcome.warning {
                            self.log_output(warning.clone(), true);
                            self.dialog =
                                Some(Dialog::Info { title: "Local changes conflicted".into(), text: warning });
                        }
                    }
                    Err(e) => self.fail(e.to_string()),
                }
                self.refresh();
            }
            Action::ShowNewBranch(start) => {
                self.dialog = Some(Dialog::NewBranch { name: String::new(), start, checkout: true });
            }
            Action::CreateBranch { name, start, checkout } => {
                let r = repo.unwrap().create_branch(&name, start.as_deref(), checkout);
                local(self, &format!("Created branch {name}"), r);
            }
            Action::AskDeleteBranch(name) => self.dialog = Some(Dialog::DeleteBranch { name, force: false }),
            Action::DeleteBranch { name, force } => {
                local(self, &format!("Deleted branch {name}"), repo.unwrap().delete_branch(&name, force));
            }
            Action::Fetch => {
                let repo = repo.unwrap();
                self.spawn(ctx, "Fetch", move || repo.fetch().map(JobOutcome::Done));
            }
            Action::Pull => {
                let repo = repo.unwrap();
                self.spawn(ctx, "Pull", move || repo.pull().map(JobOutcome::Done));
            }
            Action::Push => {
                let repo = repo.unwrap();
                self.spawn(ctx, "Push", move || repo.push().map(JobOutcome::Done));
            }
            Action::OpenGithub => {
                if let Some(url) = &self.github_url {
                    ctx.open_url(egui::OpenUrl::new_tab(url));
                }
            }
            Action::Reveal(path) => {
                let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
            }
            Action::Copy(text) => ctx.copy_text(text),
            Action::ToggleConsole => {
                self.settings.console_open = !self.settings.console_open;
                if self.settings.console_open {
                    self.console.focus();
                }
            }
        }
    }

    // ----- Frame-level behaviour -----------------------------------------------------

    /// Re-read the repository when the window regains focus, so changes made elsewhere
    /// (terminal, editor, an agent) show up without pressing F5.
    fn auto_refresh(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        if focused && !self.was_focused && self.job.is_none() {
            self.refresh();
        }
        self.was_focused = focused;
    }

    fn shortcuts(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        if self.dialog.is_some() {
            return;
        }
        let (refresh, open, checkout, console) = ctx.input(|i| {
            let cmd = i.modifiers.command;
            (
                i.key_pressed(Key::F5) || (cmd && i.key_pressed(Key::R)),
                cmd && !i.modifiers.shift && i.key_pressed(Key::O),
                cmd && i.modifiers.shift && i.key_pressed(Key::O),
                cmd && i.key_pressed(Key::J),
            )
        });
        if console {
            actions.push(Action::ToggleConsole);
        }
        if refresh {
            actions.push(Action::Refresh);
        }
        if open {
            actions.push(Action::PickFolder);
        }
        if checkout && self.repo.is_some() {
            actions.push(Action::ShowCheckout);
        }
    }

    /// Folders dropped onto the window are opened; show a hint while hovering.
    fn drag_and_drop(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let (hovering, dropped) = ctx.input(|i| {
            let dropped: Vec<PathBuf> = i
                .raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect();
            (!i.raw.hovered_files.is_empty(), dropped)
        });
        actions.extend(dropped.into_iter().map(Action::OpenPath));
        if hovering {
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("drop")));
            let rect = ctx.content_rect();
            painter.rect_filled(rect, 0.0, Color32::from_black_alpha(160));
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Drop a folder to open it",
                egui::FontId::proportional(24.0),
                Color32::WHITE,
            );
        }
    }

    fn update_diff(&mut self) {
        let entry = (self.selected.len() == 1)
            .then(|| self.status.files.iter().find(|f| self.selected.contains(&f.path)))
            .flatten();
        let Some(entry) = entry else {
            self.diff_key = None;
            self.diff = None;
            return;
        };
        let key = (entry.path.clone(), self.settings.diff_base, self.settings.compact);
        if self.diff_key.as_ref() == Some(&key) {
            return;
        }
        let Some(repo) = &self.repo else { return };
        self.diff = Some(
            repo.file_diff(entry, self.settings.diff_base)
                .map(|d| DiffView::new(d, self.settings.compact))
                .map_err(|e| e.to_string()),
        );
        self.diff_key = Some(key);
    }

    /// Write the settings as soon as the repository list or the current repository
    /// changes, instead of waiting for eframe's periodic save, so a crash or a forced
    /// quit cannot lose them.
    fn persist_repos(&mut self, frame: &mut eframe::Frame) {
        if self.persisted.0 == self.settings.repos && self.persisted.1 == self.settings.current {
            return;
        }
        if let Some(storage) = frame.storage_mut() {
            eframe::set_value(storage, eframe::APP_KEY, &self.settings);
            storage.flush();
        }
        self.persisted = (self.settings.repos.clone(), self.settings.current.clone());
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let title = match &self.repo {
            Some(repo) => format!("{} ({}) · snout", repo.name(), self.head_label()),
            None => "snout".into(),
        };
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    // ----- Panes ---------------------------------------------------------------------

    fn toolbar(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let has_repo = self.repo.is_some();
        let idle = self.job.is_none();
        let entries = self.selected_entries();
        let can_stage = entries.iter().any(FileEntry::has_unstaged);
        let can_unstage = entries.iter().any(FileEntry::has_staged);

        ui.horizontal(|ui| {
            ui.spacing_mut().button_padding = egui::vec2(10.0, 6.0);
            if ui.button("📂 Open").on_hover_text("Open local repository (⌘O)").clicked() {
                actions.push(Action::PickFolder);
            }
            ui.separator();
            let net = has_repo && idle;
            if ui.add_enabled(net, egui::Button::new("⬇ Pull")).on_hover_text("git pull").clicked() {
                actions.push(Action::Pull);
            }
            if ui.add_enabled(net, egui::Button::new("⟲ Fetch")).on_hover_text("git fetch --all --prune").clicked() {
                actions.push(Action::Fetch);
            }
            let push_label = match self.status.branch.ahead {
                0 => "⬆ Push".to_owned(),
                n => format!("⬆ Push ({n})"),
            };
            if ui
                .add_enabled(net, egui::Button::new(push_label))
                .on_hover_text("git push (sets upstream on first push)")
                .clicked()
            {
                actions.push(Action::Push);
            }
            ui.separator();
            if ui.add_enabled(can_stage, egui::Button::new("➕ Stage")).clicked() {
                actions.push(Action::Stage(stage_paths(&entries)));
            }
            if ui.add_enabled(can_unstage, egui::Button::new("➖ Unstage")).clicked() {
                actions.push(Action::Unstage(unstage_paths(&entries)));
            }
            if ui.add_enabled(!entries.is_empty(), egui::Button::new("↺ Discard")).clicked() {
                actions.push(Action::AskDiscard(entries.clone()));
            }
            ui.separator();
            if ui.add_enabled(has_repo, egui::Button::new("⟳ Refresh")).on_hover_text("F5 / ⌘R").clicked() {
                actions.push(Action::Refresh);
            }
            if ui
                .add_enabled(has_repo, egui::Button::new("🔀 Check Out…"))
                .on_hover_text("Branch, tag or commit (⇧⌘O)")
                .clicked()
            {
                actions.push(Action::ShowCheckout);
            }
            if ui.add_enabled(has_repo, egui::Button::new("🌿 New Branch")).clicked() {
                actions.push(Action::ShowNewBranch(None));
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let claude = ui
                    .add(egui::Button::selectable(self.settings.console_open, "✳ Claude"))
                    .on_hover_text("Claude Code console in this repository (⌘J)");
                if claude.clicked() {
                    actions.push(Action::ToggleConsole);
                }
                let gh = ui
                    .add_enabled(self.github_url.is_some(), egui::Button::new("🌐 GitHub"))
                    .on_hover_text(self.github_url.as_deref().unwrap_or("No GitHub remote"));
                if gh.clicked() {
                    actions.push(Action::OpenGithub);
                }
            });
        });
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.job.is_some() {
                ui.spinner();
            }
            ui.label(&self.message);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let b = &self.status.branch;
                if self.repo.is_some() {
                    let mut s = self.head_label();
                    if let Some(up) = &b.upstream {
                        s += &format!(" → {up}");
                        if b.ahead > 0 {
                            s += &format!("  ↑{}", b.ahead);
                        }
                        if b.behind > 0 {
                            s += &format!("  ↓{}", b.behind);
                        }
                    } else if b.head.is_some() {
                        s += "  (no upstream)";
                    }
                    ui.weak(s);
                }
            });
        });
    }

    fn repos_ui(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            pane_title(ui, "Repositories");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button("☰", |ui| open_menu_items(ui, actions));
            });
        });
        ui.separator();
        if self.settings.repos.len() > 6 {
            ui.add(
                egui::TextEdit::singleline(&mut self.repo_filter).hint_text("🔍 Filter").desired_width(f32::INFINITY),
            );
        }
        let needle = self.repo_filter.to_lowercase();
        let mut repos: Vec<(String, PathBuf)> = self
            .settings
            .repos
            .iter()
            .map(|p| (dir_name(p), p.clone()))
            .filter(|(name, _)| needle.is_empty() || name.to_lowercase().contains(&needle))
            .collect();
        repos.sort_by_key(|(name, _)| name.to_lowercase());

        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            if self.settings.repos.is_empty() {
                ui.weak("No repositories yet.\nUse ☰, ⌘O, or drop a folder here.");
            }
            for (name, path) in repos {
                let is_current = self.settings.current.as_ref() == Some(&path);
                let text = if is_current {
                    RichText::new(format!("{name} ({})", self.head_label())).strong()
                } else if path.is_dir() {
                    RichText::new(name)
                } else {
                    RichText::new(format!("{name} (missing)")).weak()
                };
                if is_current {
                    self.repo_tree(ui, &path, text, actions);
                    continue;
                }
                let resp = ui.selectable_label(false, text).on_hover_text(path.display().to_string());
                if resp.clicked() {
                    actions.push(Action::OpenRepo(path.clone()));
                }
                resp.context_menu(|ui| {
                    if ui.button("Reveal in Finder").clicked() {
                        actions.push(Action::Reveal(path.clone()));
                    }
                    if ui.button("Copy path").clicked() {
                        actions.push(Action::Copy(path.display().to_string()));
                    }
                    if ui.button("Remove from list").clicked() {
                        actions.push(Action::RemoveRepo(path.clone()));
                    }
                });
            }
        });
    }

    /// The open repository, expandable into its directory tree. Picking a directory
    /// narrows the Files pane to it; picking the repository shows everything again.
    fn repo_tree(&mut self, ui: &mut egui::Ui, root: &Path, text: RichText, actions: &mut Vec<Action>) {
        let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for dir in &self.dirs {
            let parent = dir.rfind('/').map_or("", |i| &dir[..i]);
            children.entry(parent).or_default().push(dir);
        }
        let mut changed: HashMap<&str, usize> = HashMap::new();
        for f in &self.status.files {
            let mut end = f.path.len();
            while let Some(slash) = f.path[..end].rfind('/') {
                *changed.entry(&f.path[..slash]).or_default() += 1;
                end = slash;
            }
        }
        let tree = DirTree { root, children: &children, changed: &changed, selected: self.dir_filter.as_deref() };
        let mut picked: Option<Option<String>> = None;

        let id = ui.make_persistent_id(("repo-tree", root));
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true)
            .show_header(ui, |ui| {
                let resp =
                    ui.selectable_label(self.dir_filter.is_none(), text).on_hover_text(root.display().to_string());
                if resp.clicked() {
                    picked = Some(None);
                }
                resp.context_menu(|ui| {
                    reveal_items(ui, root, actions);
                    if ui.button("Remove from list").clicked() {
                        actions.push(Action::RemoveRepo(root.to_owned()));
                    }
                });
            })
            .body(|ui| tree.show(ui, "", &mut picked, actions));
        if let Some(dir) = picked {
            self.dir_filter = dir;
            self.anchor = None;
        }
    }

    fn branches_ui(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            pane_title(ui, "Branches");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let has_repo = self.repo.is_some();
                if ui
                    .add_enabled(has_repo && self.job.is_none(), egui::Button::new("⟲").frame(false))
                    .on_hover_text("Fetch")
                    .clicked()
                {
                    actions.push(Action::Fetch);
                }
                if ui.add_enabled(has_repo, egui::Button::new("➕").frame(false)).on_hover_text("New branch").clicked()
                {
                    actions.push(Action::ShowNewBranch(None));
                }
                if ui
                    .add_enabled(has_repo, egui::Button::new("🔀").frame(false))
                    .on_hover_text("Check out… (⇧⌘O)")
                    .clicked()
                {
                    actions.push(Action::ShowCheckout);
                }
            });
        });
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            // Keep the full area even when empty, or the panel shrinks to fit and stays small.
            if self.repo.is_none() {
                ui.weak("No repository open");
                return;
            }
            if self.status.branch.head.is_none() {
                ui.horizontal(|ui| {
                    ui.add_sized([12.0, ui.spacing().interact_size.y], egui::Label::new("●"));
                    ui.label(RichText::new(format!("HEAD ({})", self.head_label())).strong())
                        .on_hover_text("Not on a branch. Create a branch here to keep new commits.");
                });
            }
            let local: Vec<&Branch> = self.branches.iter().filter(|b| b.kind == BranchKind::Local).collect();
            egui::CollapsingHeader::new("Local Branches").default_open(true).show(ui, |ui| {
                for b in local {
                    branch_row(ui, b, actions);
                }
            });
            let mut remotes: Vec<&str> = self.branches.iter().filter_map(|b| b.remote.as_deref()).collect();
            remotes.dedup();
            for remote in remotes {
                egui::CollapsingHeader::new(remote).default_open(false).show(ui, |ui| {
                    for b in self.branches.iter().filter(|b| b.remote.as_deref() == Some(remote)) {
                        branch_row(ui, b, actions);
                    }
                });
            }
            if !self.tags.is_empty() {
                egui::CollapsingHeader::new(format!("Tags ({})", self.tags.len())).default_open(false).show(ui, |ui| {
                    for t in &self.tags {
                        tag_row(ui, t, actions);
                    }
                });
            }
        });
    }

    /// Shown in place of the working-tree panes when no repository is open.
    fn welcome_ui(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.18).max(20.0));
            ui.heading(RichText::new("snout").size(32.0));
            ui.weak("Open a repository to get started");
            ui.add_space(20.0);
            let size = egui::vec2(300.0, 34.0);
            if ui.add_sized(size, egui::Button::new("📂 Open local repository…   ⌘O")).clicked() {
                actions.push(Action::PickFolder);
            }
            if ui.add_sized(size, egui::Button::new("🔍 Find repositories in folder…")).clicked() {
                actions.push(Action::PickScanFolder);
            }
            if ui.add_sized(size, egui::Button::new("⬇ Clone from GitHub…")).clicked() {
                actions.push(Action::ShowClone);
            }
            ui.add_space(8.0);
            ui.weak("or drop a folder onto this window");

            if !self.settings.repos.is_empty() {
                ui.add_space(24.0);
                ui.strong("Recent");
                for path in self.settings.repos.iter().rev().take(8) {
                    let resp = ui.link(dir_name(path)).on_hover_text(path.display().to_string());
                    if resp.clicked() {
                        actions.push(Action::OpenRepo(path.clone()));
                    }
                }
            }
        });
    }

    fn files_ui(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            pane_title(ui, "Files");
            ui.weak(format!("{} changed, {} staged", self.status.files.len(), self.status.staged_count()));
            if let Some(dir) = &self.dir_filter {
                let chip = ui.add(egui::Button::new(format!("in {dir}/  ✕")).small()).on_hover_text("Show all files");
                if chip.clicked() {
                    self.dir_filter = None;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("🔍 File filter").desired_width(220.0));
            });
        });
        ui.separator();

        // Visible rows: filtered, then sorted.
        let needle = self.filter.to_lowercase();
        let mut rows: Vec<&FileEntry> = self
            .status
            .files
            .iter()
            .filter(|f| needle.is_empty() || f.path.to_lowercase().contains(&needle))
            .filter(|f| {
                self.dir_filter
                    .as_ref()
                    .is_none_or(|d| f.path.strip_prefix(d.as_str()).is_some_and(|rest| rest.starts_with('/')))
            })
            .collect();
        let (col, asc) = self.sort;
        rows.sort_by(|a, b| {
            let ord = match col {
                SortColumn::Name => a.file_name().cmp(b.file_name()).then(a.path.cmp(&b.path)),
                SortColumn::State => a.state_label().cmp(&b.state_label()).then(a.path.cmp(&b.path)),
                SortColumn::Directory => a.directory().cmp(b.directory()).then(a.file_name().cmp(b.file_name())),
            };
            if asc { ord } else { ord.reverse() }
        });

        // Cmd+A selects all visible files when no text field has focus.
        let select_all =
            ui.input(|i| i.modifiers.command && i.key_pressed(Key::A)) && ui.ctx().memory(|m| m.focused().is_none());
        if select_all {
            self.selected = rows.iter().map(|f| f.path.clone()).collect();
        }

        let modifiers = ui.input(|i| i.modifiers);
        let mut clicked: Option<usize> = None;
        let mut sort_clicked: Option<SortColumn> = None;
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        // Selectable labels would swallow clicks meant for selecting the row.
        ui.style_mut().interaction.selectable_labels = false;

        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .auto_shrink(false)
            .min_scrolled_height(0.0)
            .max_scroll_height(f32::INFINITY)
            .column(Column::initial(260.0).at_least(80.0).clip(true))
            .column(Column::initial(170.0).at_least(60.0).clip(true))
            .column(Column::remainder().clip(true))
            .header(row_h, |mut header| {
                for (label, c) in [
                    ("Name", SortColumn::Name),
                    ("State", SortColumn::State),
                    ("Relative Directory", SortColumn::Directory),
                ] {
                    header.col(|ui| {
                        let arrow = match (self.sort.0 == c, self.sort.1) {
                            (true, true) => " ⏶",
                            (true, false) => " ⏷",
                            _ => "",
                        };
                        let r = ui.add(
                            egui::Label::new(RichText::new(format!("{label}{arrow}")).strong()).sense(Sense::click()),
                        );
                        if r.clicked() {
                            sort_clicked = Some(c);
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, rows.len(), |mut row| {
                    let f = rows[row.index()];
                    row.set_selected(self.selected.contains(&f.path));
                    row.col(|ui| {
                        ui.label(f.file_name());
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(f.state_label()).color(state_color(f, ui.visuals())));
                    });
                    row.col(|ui| {
                        ui.weak(f.directory());
                    });
                    let resp = row.response();
                    if resp.clicked() || resp.secondary_clicked() && !self.selected.contains(&f.path) {
                        clicked = Some(row.index());
                    }
                    resp.context_menu(|ui| {
                        let entries =
                            if self.selected.contains(&f.path) { self.selected_entries() } else { vec![f.clone()] };
                        if ui
                            .add_enabled(entries.iter().any(FileEntry::has_unstaged), egui::Button::new("Stage"))
                            .clicked()
                        {
                            actions.push(Action::Stage(stage_paths(&entries)));
                        }
                        if ui
                            .add_enabled(entries.iter().any(FileEntry::has_staged), egui::Button::new("Unstage"))
                            .clicked()
                        {
                            actions.push(Action::Unstage(unstage_paths(&entries)));
                        }
                        if ui.button("Discard…").clicked() {
                            actions.push(Action::AskDiscard(entries.clone()));
                        }
                        ui.separator();
                        if let Some(repo) = &self.repo
                            && ui.button("Reveal in Finder").clicked()
                        {
                            actions.push(Action::Reveal(repo.root().join(&f.path)));
                        }
                        if ui.button("Copy path").clicked() {
                            actions.push(Action::Copy(f.path.clone()));
                        }
                    });
                });
            });

        if let Some(c) = sort_clicked {
            self.sort = if self.sort.0 == c { (c, !self.sort.1) } else { (c, true) };
        }
        if let Some(i) = clicked {
            let path = rows[i].path.clone();
            if modifiers.shift {
                let anchor = self.anchor.as_ref().and_then(|a| rows.iter().position(|f| &f.path == a)).unwrap_or(i);
                let (lo, hi) = (anchor.min(i), anchor.max(i));
                if !modifiers.command {
                    self.selected.clear();
                }
                self.selected.extend(rows[lo..=hi].iter().map(|f| f.path.clone()));
            } else if modifiers.command {
                if !self.selected.remove(&path) {
                    self.selected.insert(path.clone());
                }
                self.anchor = Some(path);
            } else {
                self.selected = BTreeSet::from([path.clone()]);
                self.anchor = Some(path);
            }
        }
    }

    fn changes_ui(&mut self, ui: &mut egui::Ui) {
        self.update_diff();
        ui.horizontal(|ui| {
            pane_title(ui, "Changes");
            if let Some(Ok(view)) = &self.diff {
                let d = &view.diff;
                ui.label(RichText::new(&d.path).monospace());
                if !d.binary && !d.too_large {
                    ui.label(RichText::new(format!("+{}", d.additions)).color(Color32::from_rgb(60, 160, 80)));
                    ui.label(RichText::new(format!("−{}", d.deletions)).color(Color32::from_rgb(200, 70, 70)));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.toggle_value(&mut self.settings.compact, "Compact")
                    .on_hover_text("Hide unchanged lines away from changes");
                ui.separator();
                ui.selectable_value(&mut self.settings.side_by_side, true, "Side by Side");
                ui.selectable_value(&mut self.settings.side_by_side, false, "Unified");
                ui.separator();
                egui::ComboBox::from_label("Compare").selected_text(self.settings.diff_base.label()).show_ui(
                    ui,
                    |ui| {
                        for base in DiffBase::ALL {
                            ui.selectable_value(&mut self.settings.diff_base, base, base.label());
                        }
                    },
                );
            });
        });
        ui.separator();

        match &self.diff {
            None if self.selected.len() > 1 => centered_note(ui, &format!("{} files selected", self.selected.len())),
            None => centered_note(ui, "Select a file to see its changes"),
            Some(Err(e)) => centered_note(ui, e),
            Some(Ok(view)) => {
                let d = &view.diff;
                if d.binary {
                    centered_note(ui, "Binary file differs");
                } else if d.too_large {
                    centered_note(ui, "File too large to display");
                } else if d.is_identical() {
                    let (l, r) = d.base.sides();
                    centered_note(ui, &format!("No differences between {l} and {r}"));
                } else {
                    if self.settings.side_by_side {
                        let (l, r) = d.base.sides();
                        ui.columns(2, |cols| {
                            cols[0].weak(l);
                            cols[1].weak(r);
                        });
                    }
                    view.show(ui, self.settings.side_by_side);
                }
            }
        }
    }

    fn commit_ui(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        pane_title(ui, "Commit");
        ui.separator();
        let staged = self.status.staged_count();
        let can_commit = self.repo.is_some() && !self.commit_msg.trim().is_empty() && (staged > 0 || self.amend);

        egui::Panel::bottom("commit_actions").show(ui, |ui| {
            ui.horizontal(|ui| {
                let had_amend = self.amend;
                ui.add_enabled(!self.log.is_empty(), egui::Checkbox::new(&mut self.amend, "Amend last commit"));
                if self.amend
                    && !had_amend
                    && self.commit_msg.trim().is_empty()
                    && let Some(repo) = &self.repo
                {
                    self.commit_msg = repo.last_commit_message().unwrap_or_default();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let hint = if staged == 0 && !self.amend { "Nothing staged" } else { "⌘↩" };
                    if ui.add_enabled(can_commit, egui::Button::new("Commit")).on_hover_text(hint).clicked() {
                        actions.push(Action::Commit);
                    }
                    if staged == 0 && !self.amend {
                        ui.weak("nothing staged");
                    }
                });
            });
        });
        egui::CentralPanel::default().show(ui, |ui| {
            let edit = ui.add_sized(
                ui.available_size(),
                egui::TextEdit::multiline(&mut self.commit_msg).hint_text("Commit message"),
            );
            if edit.has_focus() && can_commit && ui.input(|i| i.modifiers.command && i.key_pressed(Key::Enter)) {
                actions.push(Action::Commit);
            }
        });
    }

    fn journal_ui(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.bottom_tab, BottomTab::Journal, RichText::new("Journal").strong());
            ui.selectable_value(&mut self.bottom_tab, BottomTab::Output, RichText::new("Output").strong());
            if self.bottom_tab == BottomTab::Output {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Clear").clicked() {
                        self.output.clear();
                    }
                });
            }
        });
        ui.separator();
        match self.bottom_tab {
            BottomTab::Journal => self.journal_table(ui, actions),
            BottomTab::Output => {
                egui::ScrollArea::vertical().auto_shrink(false).stick_to_bottom(true).show(ui, |ui| {
                    for line in &self.output {
                        let text = RichText::new(&line.text).monospace();
                        ui.label(if line.error { text.color(ui.visuals().error_fg_color) } else { text });
                    }
                });
            }
        }
    }

    fn journal_table(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
        ui.style_mut().interaction.selectable_labels = false;
        TableBuilder::new(ui)
            .id_salt("journal")
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .auto_shrink(false)
            .min_scrolled_height(0.0)
            .max_scroll_height(f32::INFINITY)
            .column(Column::exact(18.0))
            .column(Column::remainder().at_least(200.0).clip(true))
            .column(Column::initial(160.0).clip(true))
            .column(Column::initial(130.0))
            .column(Column::initial(80.0))
            .header(row_h, |mut h| {
                h.col(|_| {});
                for t in ["Message", "Author", "Date", "SHA"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, self.log.len(), |mut row| {
                    let c = &self.log[row.index()];
                    row.col(|ui| {
                        if c.unpushed {
                            ui.label(RichText::new("↑").color(Color32::from_rgb(70, 140, 220)))
                                .on_hover_text("Not pushed yet");
                        }
                    });
                    row.col(|ui| {
                        ui.horizontal(|ui| {
                            for r in &c.refs {
                                ref_badge(ui, r);
                            }
                            let text = RichText::new(&c.summary);
                            ui.label(if c.unpushed { text.strong() } else { text });
                        });
                    });
                    row.col(|ui| {
                        ui.label(&c.author).on_hover_text(&c.email);
                    });
                    row.col(|ui| {
                        ui.weak(&c.date);
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(&c.short_id).monospace()).on_hover_text(&c.id);
                    });
                    row.response().context_menu(|ui| {
                        if ui.button("Check out this commit (detached)").clicked() {
                            actions.push(Action::RequestCheckout(CheckoutTarget::Commit(c.id.clone())));
                        }
                        if ui.button("Create branch here…").clicked() {
                            let start =
                                StartPoint { label: format!("{} {}", c.short_id, c.summary), rev: c.id.clone() };
                            actions.push(Action::ShowNewBranch(Some(start)));
                        }
                        ui.separator();
                        if ui.button("Copy SHA").clicked() {
                            actions.push(Action::Copy(c.id.clone()));
                        }
                        if ui.button("Copy message").clicked() {
                            actions.push(Action::Copy(c.summary.clone()));
                        }
                    });
                });
            });
    }
}

impl eframe::App for SnoutApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_job();
        self.auto_refresh(&ctx);
        let mut actions = Vec::new();
        self.shortcuts(&ctx, &mut actions);

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui, &mut actions);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("statusbar").show(ui, |ui| self.status_bar(ui));
        if self.settings.console_open {
            let root = self.repo.as_ref().map(|r| r.root().to_owned());
            let event = egui::Panel::right("console")
                .resizable(true)
                .default_size(680.0)
                .min_size(360.0)
                .show(ui, |ui| self.console.ui(ui, root.as_deref()))
                .inner;
            match event {
                Some(ConsoleEvent::Activity) if self.job.is_none() => self.refresh(),
                Some(ConsoleEvent::Close) => self.settings.console_open = false,
                _ => {}
            }
        }
        egui::Panel::left("left").resizable(true).default_size(260.0).show(ui, |ui| {
            egui::Panel::bottom("branches")
                .resizable(true)
                .default_size(360.0)
                .show(ui, |ui| self.branches_ui(ui, &mut actions));
            egui::CentralPanel::default().show(ui, |ui| self.repos_ui(ui, &mut actions));
        });
        egui::CentralPanel::default().show(ui, |ui| {
            if self.repo.is_none() {
                self.welcome_ui(ui, &mut actions);
                return;
            }
            egui::Panel::bottom("journal")
                .resizable(true)
                .default_size(170.0)
                .show(ui, |ui| self.journal_ui(ui, &mut actions));
            egui::Panel::bottom("changes").resizable(true).default_size(340.0).show(ui, |ui| self.changes_ui(ui));
            // Capped so the file list keeps room when the console narrows the window.
            let commit_max = (ui.available_width() * 0.45).max(220.0);
            egui::Panel::right("commit")
                .resizable(true)
                .default_size(380.0)
                .max_size(commit_max)
                .show(ui, |ui| self.commit_ui(ui, &mut actions));
            egui::CentralPanel::default().show(ui, |ui| self.files_ui(ui, &mut actions));
        });

        self.dialogs(&ctx, &mut actions);
        self.drag_and_drop(&ctx, &mut actions);
        for action in actions {
            self.handle(action, &ctx);
        }
        self.update_title(&ctx);
        self.persist_repos(frame);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.persisted = (self.settings.repos.clone(), self.settings.current.clone());
        eframe::set_value(storage, eframe::APP_KEY, &self.settings);
    }
}

// ----- Helpers -------------------------------------------------------------------------

/// System fonts used as fallbacks, if present: the symbols Claude Code draws (✻ ⎿ ✓ …)
/// are not in egui's bundled fonts.
const SYSTEM_FALLBACKS: &[(&str, &str)] = &[
    ("apple-symbols", "/System/Library/Fonts/Apple Symbols.ttf"),
    ("menlo", "/System/Library/Fonts/Menlo.ttc"),
    ("dejavu-mono", "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"),
    ("segoe-symbol", "C:\\Windows\\Fonts\\seguisym.ttf"),
];

/// The proportional font lacks arrows (↑ ↓ →); let it fall back to the monospace font,
/// then to the system fallbacks.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mono = fonts.families.get(&egui::FontFamily::Monospace).and_then(|f| f.first()).cloned();
    if let Some(mono) = mono {
        fonts.families.entry(egui::FontFamily::Proportional).or_default().push(mono);
    }
    for (name, path) in SYSTEM_FALLBACKS {
        let Ok(bytes) = std::fs::read(path) else { continue };
        fonts.font_data.insert((*name).to_owned(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
        for family in [egui::FontFamily::Monospace, egui::FontFamily::Proportional] {
            fonts.families.entry(family).or_default().push((*name).to_owned());
        }
    }
    ctx.set_fonts(fonts);
}

/// The directory tree of the open repository, for [`SnoutApp::repo_tree`].
struct DirTree<'a> {
    root: &'a Path,
    children: &'a BTreeMap<&'a str, Vec<&'a str>>,
    /// Changed files under each directory, recursively.
    changed: &'a HashMap<&'a str, usize>,
    selected: Option<&'a str>,
}

impl DirTree<'_> {
    fn show(&self, ui: &mut egui::Ui, parent: &str, picked: &mut Option<Option<String>>, actions: &mut Vec<Action>) {
        let Some(dirs) = self.children.get(parent) else { return };
        for &dir in dirs {
            let name = dir.rsplit('/').next().unwrap_or(dir);
            let changed = self.changed.get(dir).copied().unwrap_or(0);
            let label = |ui: &mut egui::Ui, picked: &mut Option<Option<String>>, actions: &mut Vec<Action>| {
                let mut text = RichText::new(format!("📁 {name}"));
                if changed > 0 {
                    text = text.color(ui.visuals().warn_fg_color);
                }
                let mut resp = ui.selectable_label(self.selected == Some(dir), text);
                if changed > 0 {
                    resp = resp.on_hover_text(format!("{changed} changed"));
                }
                if resp.clicked() {
                    *picked = Some(Some(dir.to_owned()));
                }
                resp.context_menu(|ui| reveal_items(ui, &self.root.join(dir), actions));
            };
            if self.children.contains_key(dir) {
                let id = ui.make_persistent_id(("dir", self.root, dir));
                egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
                    .show_header(ui, |ui| label(ui, picked, actions))
                    .body(|ui| self.show(ui, dir, picked, actions));
            } else {
                ui.horizontal(|ui| {
                    ui.add_space(ui.spacing().indent);
                    label(ui, picked, actions);
                });
            }
        }
    }
}

fn reveal_items(ui: &mut egui::Ui, path: &Path, actions: &mut Vec<Action>) {
    if ui.button("Reveal in Finder").clicked() {
        actions.push(Action::Reveal(path.to_owned()));
    }
    if ui.button("Copy path").clicked() {
        actions.push(Action::Copy(path.display().to_string()));
    }
}

fn pane_title(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).strong());
}

fn centered_note(ui: &mut egui::Ui, text: &str) {
    ui.centered_and_justified(|ui| {
        ui.weak(text);
    });
}

fn dir_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

fn open_menu_items(ui: &mut egui::Ui, actions: &mut Vec<Action>) {
    if ui.button("Open local repository…   ⌘O").clicked() {
        actions.push(Action::PickFolder);
    }
    if ui.button("Find repositories in folder…").clicked() {
        actions.push(Action::PickScanFolder);
    }
    if ui.button("Clone from GitHub…").clicked() {
        actions.push(Action::ShowClone);
    }
}

fn stage_paths(entries: &[FileEntry]) -> Vec<String> {
    entries.iter().filter(|e| e.has_unstaged()).map(|e| e.path.clone()).collect()
}

/// Paths to unstage; renames need both sides restored in the index.
fn unstage_paths(entries: &[FileEntry]) -> Vec<String> {
    entries
        .iter()
        .filter(|e| e.has_staged())
        .flat_map(|e| std::iter::once(e.path.clone()).chain(e.orig_path.clone()))
        .collect()
}

fn state_color(f: &FileEntry, v: &egui::Visuals) -> Color32 {
    if f.conflicted {
        v.error_fg_color
    } else if f.is_untracked() {
        v.weak_text_color()
    } else if f.has_staged() && !f.has_unstaged() {
        Color32::from_rgb(60, 160, 80)
    } else if f.has_staged() {
        Color32::from_rgb(200, 150, 40)
    } else {
        Color32::from_rgb(70, 130, 210)
    }
}

/// A selectable row with a fixed-width marker column, so names line up.
fn marked_row(ui: &mut egui::Ui, marker: &str, text: RichText, hover: String) -> egui::Response {
    ui.horizontal(|ui| {
        ui.add_sized([12.0, ui.spacing().interact_size.y], egui::Label::new(marker));
        ui.selectable_label(false, text).on_hover_text(hover)
    })
    .inner
}

fn branch_row(ui: &mut egui::Ui, b: &Branch, actions: &mut Vec<Action>) {
    let mut label = match b.kind {
        BranchKind::Local => b.name.clone(),
        BranchKind::Remote => b.local_name().to_owned(),
    };
    if b.ahead > 0 {
        label += &format!("  ↑{}", b.ahead);
    }
    if b.behind > 0 {
        label += &format!("  ↓{}", b.behind);
    }
    if b.upstream_gone {
        label += "  (gone)";
    }
    let text = if b.is_head { RichText::new(label).strong() } else { RichText::new(label) };
    let hover = format!(
        "{}{}\n{} {}\n\nDouble-click to check out",
        b.name,
        b.upstream.as_ref().map(|u| format!(" → {u}")).unwrap_or_default(),
        b.target,
        b.subject
    );
    let resp = marked_row(ui, if b.is_head { "●" } else { "" }, text, hover);
    if resp.double_clicked() && !b.is_head {
        actions.push(Action::RequestCheckout(b.into()));
    }
    resp.context_menu(|ui| {
        let checkout_label = match b.kind {
            BranchKind::Local => "Check out".to_owned(),
            BranchKind::Remote => format!("Check out as local “{}”", b.local_name()),
        };
        if ui.add_enabled(!b.is_head, egui::Button::new(checkout_label)).clicked() {
            actions.push(Action::RequestCheckout(b.into()));
        }
        if ui.button("Create branch from here…").clicked() {
            actions.push(Action::ShowNewBranch(Some(StartPoint { label: b.name.clone(), rev: b.name.clone() })));
        }
        if b.kind == BranchKind::Local && ui.add_enabled(!b.is_head, egui::Button::new("Delete…")).clicked() {
            actions.push(Action::AskDeleteBranch(b.name.clone()));
        }
        ui.separator();
        if ui.button("Copy name").clicked() {
            actions.push(Action::Copy(b.name.clone()));
        }
    });
}

fn tag_row(ui: &mut egui::Ui, t: &Tag, actions: &mut Vec<Action>) {
    let hover = format!("{} {}\n{}\n\nDouble-click to check out (detached)", t.target, t.subject, t.date);
    let resp = marked_row(ui, "", RichText::new(&t.name), hover);
    let target = CheckoutTarget::Tag(t.name.clone());
    if resp.double_clicked() {
        actions.push(Action::RequestCheckout(target.clone()));
    }
    resp.context_menu(|ui| {
        if ui.button("Check out (detached)").clicked() {
            actions.push(Action::RequestCheckout(target.clone()));
        }
        if ui.button("Create branch from tag…").clicked() {
            let rev = format!("refs/tags/{}", t.name);
            actions.push(Action::ShowNewBranch(Some(StartPoint { label: format!("tag {}", t.name), rev })));
        }
        ui.separator();
        if ui.button("Copy name").clicked() {
            actions.push(Action::Copy(t.name.clone()));
        }
    });
}

fn ref_badge(ui: &mut egui::Ui, r: &str) {
    let (text, color) = if let Some(branch) = r.strip_prefix("HEAD -> ") {
        (branch, Color32::from_rgb(60, 160, 80))
    } else if let Some(tag) = r.strip_prefix("tag: ") {
        (tag, Color32::from_rgb(200, 150, 40))
    } else if r == "HEAD" {
        (r, Color32::from_rgb(60, 160, 80))
    } else {
        (r, Color32::from_rgb(70, 130, 210))
    };
    egui::Frame::new()
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(3.0)
        .inner_margin(egui::Margin::symmetric(4, 0))
        .show(ui, |ui| {
            ui.label(RichText::new(text).small().color(color));
        });
}

/// `https://github.com/o/repo.git` -> `repo`.
fn repo_dir_name(url: &str) -> Option<String> {
    let name = url.trim().trim_end_matches('/').rsplit(['/', ':']).next()?.trim_end_matches(".git");
    (!name.is_empty()).then(|| name.to_owned())
}
