//! Modal dialogs.

use std::path::{Path, PathBuf};

use eframe::egui::{self, Key, RichText};
use snout_core::{Branch, BranchKind, CheckoutTarget, FileEntry, LocalChanges, PullMode, PullProblem, Tag};

use super::{Action, SnoutApp, repo_dir_name};

pub(super) enum Dialog {
    Error(String),
    Info {
        title: String,
        text: String,
    },
    ConfirmDiscard(Vec<FileEntry>),
    /// The working tree has changes; ask how to treat them before checking out.
    ConfirmCheckout(CheckoutTarget),
    Checkout(CheckoutPicker),
    NewBranch {
        name: String,
        start: Option<StartPoint>,
        checkout: bool,
    },
    DeleteBranch {
        name: String,
        force: bool,
    },
    /// A pull could not combine the local and remote branches.
    PullProblem {
        problem: PullProblem,
        details: String,
    },
    /// A folder that is not a repository was opened.
    InitRepo(PathBuf),
    Clone {
        url: String,
        parent: String,
    },
}

/// Where a new branch starts. `None` in [`Dialog::NewBranch`] means HEAD.
pub(super) struct StartPoint {
    pub label: String,
    pub rev: String,
}

/// State of the "Check out…" picker.
#[derive(Default)]
pub(super) struct CheckoutPicker {
    pub query: String,
    cursor: usize,
    local: LocalChanges,
    focused: bool,
    /// `describe_commit` result for the query it was computed for.
    preview: Option<(String, Option<String>)>,
}

struct PickItem {
    target: CheckoutTarget,
    label: String,
    detail: String,
    section: &'static str,
}

/// Candidates for the picker, filtered by `query`. Remote branches that already have a
/// local counterpart are left out: checking them out would just switch to the local one.
fn pick_items(branches: &[Branch], tags: &[Tag], query: &str, commit: Option<&str>) -> Vec<PickItem> {
    let q = query.to_lowercase();
    let matches = |s: &str| q.is_empty() || s.to_lowercase().contains(&q);
    let locals: Vec<&str> = branches.iter().filter(|b| b.kind == BranchKind::Local).map(|b| b.name.as_str()).collect();

    let mut items = Vec::new();
    for b in branches.iter().filter(|b| matches(&b.name)) {
        let (section, include) = match b.kind {
            BranchKind::Local => ("Local branches", !b.is_head),
            BranchKind::Remote => ("Remote branches", !locals.contains(&b.local_name())),
        };
        if include {
            items.push(PickItem {
                target: b.into(),
                label: b.name.clone(),
                detail: format!("{} {}", b.target, b.subject),
                section,
            });
        }
    }
    items.sort_by_key(|i| i.section != "Local branches");
    for t in tags.iter().filter(|t| matches(&t.name)) {
        items.push(PickItem {
            target: CheckoutTarget::Tag(t.name.clone()),
            label: t.name.clone(),
            detail: format!("{} {}", t.target, t.subject),
            section: "Tags",
        });
    }
    // A query that names a commit but nothing above exactly: offer the commit itself.
    if let Some(described) = commit
        && !items.iter().any(|i| i.label == query)
    {
        items.push(PickItem {
            target: CheckoutTarget::Commit(query.to_owned()),
            label: query.to_owned(),
            detail: described.to_owned(),
            section: "Commit",
        });
    }
    items
}

impl SnoutApp {
    pub(super) fn dialogs(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let has_changes = !self.status.files.is_empty();
        let has_tracked_changes = self.has_tracked_changes();
        let branch = self.status.branch.head.clone().unwrap_or_else(|| "HEAD".into());
        let upstream = self.status.branch.upstream.clone().unwrap_or_else(|| "its upstream".into());
        let Some(dialog) = &mut self.dialog else { return };
        let (branches, tags, repo) = (&self.branches, &self.tags, self.repo.as_ref());
        let mut close = false;

        let modal = egui::Modal::new(egui::Id::unique("dialog")).show(ctx, |ui| {
            ui.set_max_width(600.0);
            match dialog {
                Dialog::Error(msg) => {
                    ui.heading("Error");
                    egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                        ui.label(RichText::new(msg.as_str()).monospace());
                    });
                    ui.add_space(8.0);
                    close = ui.button("OK").clicked() || ui.input(|i| i.key_pressed(Key::Enter));
                }
                Dialog::PullProblem { problem, details } => {
                    match problem {
                        PullProblem::Divergent => {
                            ui.heading("Both sides have new commits");
                            ui.label(format!(
                                "{branch} and {upstream} have each moved on, and this repository's settings do not say \
                                 how to combine them."
                            ));
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                if ui
                                    .button("Merge")
                                    .on_hover_text("git pull --no-rebase: adds a merge commit")
                                    .clicked()
                                {
                                    actions.push(Action::Pull(PullMode::Merge));
                                    close = true;
                                }
                                if ui
                                    .button("Rebase")
                                    .on_hover_text("git pull --rebase: replays your commits on top of the remote ones")
                                    .clicked()
                                {
                                    actions.push(Action::Pull(PullMode::Rebase));
                                    close = true;
                                }
                                close |= ui.button("Cancel").clicked();
                            });
                        }
                        PullProblem::Unrelated => {
                            ui.heading("No shared history");
                            ui.label(format!(
                                "{branch} and {upstream} have no commit in common, so they cannot be merged. This \
                                 usually means the remote repository was recreated or its history was rewritten."
                            ));
                            ui.label(format!(
                                "Resetting makes {branch} match {upstream} exactly. Your current commits stay on a \
                                 backup branch, so nothing is lost."
                            ));
                            if has_tracked_changes {
                                ui.colored_label(
                                    ui.visuals().warn_fg_color,
                                    "Commit or stash your uncommitted changes first; resetting would discard them.",
                                );
                            }
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                let reset = ui.add_enabled(
                                    !has_tracked_changes,
                                    egui::Button::new(format!("Reset to {upstream}")),
                                );
                                if reset.clicked() {
                                    actions.push(Action::ResetToUpstream);
                                    close = true;
                                }
                                close |= ui.button("Cancel").clicked();
                            });
                        }
                    }
                    ui.add_space(4.0);
                    egui::CollapsingHeader::new("Details from git").show(ui, |ui| {
                        ui.label(RichText::new(details.as_str()).monospace().small());
                    });
                }
                Dialog::Info { title, text } => {
                    ui.heading(title.as_str());
                    egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                        ui.label(text.as_str());
                    });
                    ui.add_space(8.0);
                    close = ui.button("OK").clicked() || ui.input(|i| i.key_pressed(Key::Enter));
                }
                Dialog::ConfirmDiscard(entries) => {
                    ui.heading("Discard changes?");
                    ui.label("These files will be reverted to HEAD. Untracked files will be deleted.");
                    egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
                        for e in entries.iter() {
                            ui.label(RichText::new(format!("{}  ({})", e.path, e.state_label())).monospace());
                        }
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Discard").color(ui.visuals().error_fg_color)).clicked() {
                            actions.push(Action::Discard(entries.clone()));
                            close = true;
                        }
                        close |= ui.button("Cancel").clicked();
                    });
                }
                Dialog::ConfirmCheckout(target) => {
                    ui.heading(format!("Check out {}?", target.label()));
                    ui.label("You have uncommitted changes.");
                    if target.detaches() {
                        detach_note(ui);
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button("Stash, check out, re-apply")
                            .on_hover_text("Works even if the changes touch files that differ on the target")
                            .clicked()
                        {
                            actions.push(Action::Checkout(target.clone(), LocalChanges::Stash));
                            close = true;
                        }
                        if ui
                            .button("Check out, keep changes")
                            .on_hover_text("git refuses if the changes would be overwritten")
                            .clicked()
                        {
                            actions.push(Action::Checkout(target.clone(), LocalChanges::Keep));
                            close = true;
                        }
                        close |= ui.button("Cancel").clicked();
                    });
                }
                Dialog::Checkout(picker) => {
                    if let Some(action) = checkout_picker(ui, picker, branches, tags, repo, has_changes, &mut close) {
                        actions.push(action);
                    }
                }
                Dialog::NewBranch { name, start, checkout } => {
                    ui.heading("New branch");
                    let from = start.as_ref().map_or("HEAD", |s| s.label.as_str());
                    ui.label(format!("Starting at: {from}"));
                    let edit =
                        ui.add(egui::TextEdit::singleline(name).hint_text("branch name").desired_width(f32::INFINITY));
                    edit.request_focus();
                    ui.checkbox(checkout, "Check out the new branch");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let ok = !name.trim().is_empty();
                        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        if ui.add_enabled(ok, egui::Button::new("Create")).clicked() || (ok && enter) {
                            actions.push(Action::CreateBranch {
                                name: name.trim().to_owned(),
                                start: start.as_ref().map(|s| s.rev.clone()),
                                checkout: *checkout,
                            });
                            close = true;
                        }
                        close |= ui.button("Cancel").clicked();
                    });
                }
                Dialog::DeleteBranch { name, force } => {
                    ui.heading(format!("Delete branch “{name}”?"));
                    ui.checkbox(force, "Delete even if it is not fully merged");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Delete").color(ui.visuals().error_fg_color)).clicked() {
                            actions.push(Action::DeleteBranch { name: name.clone(), force: *force });
                            close = true;
                        }
                        close |= ui.button("Cancel").clicked();
                    });
                }
                Dialog::InitRepo(path) => {
                    ui.heading("Not a Git repository");
                    ui.label(RichText::new(path.display().to_string()).monospace());
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Find repositories inside").clicked() {
                            actions.push(Action::Scan(path.clone()));
                            close = true;
                        }
                        if ui.button("Initialize a repository here").clicked() {
                            actions.push(Action::InitRepo(path.clone()));
                            close = true;
                        }
                        close |= ui.button("Cancel").clicked();
                    });
                }
                Dialog::Clone { url, parent } => {
                    ui.heading("Clone repository");
                    egui::Grid::new("clone_grid").num_columns(2).spacing([8.0, 8.0]).show(ui, |ui| {
                        ui.label("URL");
                        ui.add(
                            egui::TextEdit::singleline(url)
                                .hint_text("https://github.com/owner/repo.git")
                                .desired_width(380.0),
                        );
                        ui.end_row();
                        ui.label("Into folder");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(parent).desired_width(300.0));
                            if ui.button("Choose…").clicked()
                                && let Some(dir) = rfd::FileDialog::new().pick_folder()
                            {
                                *parent = dir.display().to_string();
                            }
                        });
                        ui.end_row();
                    });
                    let dest = repo_dir_name(url).map(|name| Path::new(parent.as_str()).join(name));
                    if let Some(dest) = &dest {
                        ui.weak(format!("→ {}", dest.display()));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let ok = dest.is_some() && !parent.trim().is_empty();
                        if ui.add_enabled(ok, egui::Button::new("Clone")).clicked() {
                            actions.push(Action::Clone { url: url.trim().to_owned(), dest: dest.clone().unwrap() });
                            close = true;
                        }
                        close |= ui.button("Cancel").clicked();
                    });
                }
            }
        });
        if close || modal.should_close() {
            self.dialog = None;
        }
    }
}

fn detach_note(ui: &mut egui::Ui) {
    ui.weak("This leaves HEAD detached. Create a branch if you want to keep new commits.");
}

/// The searchable branch / tag / commit picker. Returns the checkout to perform.
fn checkout_picker(
    ui: &mut egui::Ui,
    p: &mut CheckoutPicker,
    branches: &[Branch],
    tags: &[Tag],
    repo: Option<&snout_core::Repo>,
    has_changes: bool,
    close: &mut bool,
) -> Option<Action> {
    ui.heading("Check out");
    let edit = ui.add(
        egui::TextEdit::singleline(&mut p.query)
            .hint_text("Filter branches and tags, or enter a commit (SHA, HEAD~2, …)")
            .desired_width(f32::INFINITY),
    );
    if !p.focused {
        edit.request_focus();
        p.focused = true;
    }
    if edit.changed() {
        p.cursor = 0;
    }

    let query = p.query.trim().to_owned();
    if p.preview.as_ref().map(|(q, _)| q) != Some(&query) {
        let described = repo.filter(|_| !query.is_empty()).and_then(|r| r.describe_commit(&query).ok());
        p.preview = Some((query.clone(), described));
    }
    let commit = p.preview.as_ref().and_then(|(_, d)| d.as_deref());
    let items = pick_items(branches, tags, &query, commit);

    let (down, up, enter) =
        ui.input(|i| (i.key_pressed(Key::ArrowDown), i.key_pressed(Key::ArrowUp), i.key_pressed(Key::Enter)));
    if down {
        p.cursor += 1;
    }
    if up {
        p.cursor = p.cursor.saturating_sub(1);
    }
    p.cursor = p.cursor.min(items.len().saturating_sub(1));

    let mut chosen = enter.then_some(p.cursor).filter(|_| !items.is_empty());
    egui::ScrollArea::vertical().max_height(320.0).min_scrolled_height(320.0).auto_shrink([false, false]).show(
        ui,
        |ui| {
            let mut section = "";
            for (i, item) in items.iter().enumerate() {
                if item.section != section {
                    section = item.section;
                    ui.add_space(4.0);
                    ui.weak(section);
                }
                let r = ui
                    .horizontal(|ui| {
                        let r = ui.selectable_label(i == p.cursor, &item.label);
                        ui.weak(&item.detail);
                        r
                    })
                    .inner;
                if (up || down) && i == p.cursor {
                    r.scroll_to_me(None);
                }
                if r.clicked() {
                    p.cursor = i;
                }
                if r.double_clicked() {
                    chosen = Some(i);
                }
            }
            if items.is_empty() {
                ui.weak("No matching branch, tag, or commit");
            }
        },
    );

    ui.separator();
    if has_changes {
        ui.label("Uncommitted changes:");
        ui.radio_value(&mut p.local, LocalChanges::Keep, "Keep them (git refuses if they would be overwritten)");
        ui.radio_value(&mut p.local, LocalChanges::Stash, "Stash, check out, then re-apply");
    }
    if items.get(p.cursor).is_some_and(|i| i.target.detaches()) {
        detach_note(ui);
    }
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.add_enabled(!items.is_empty(), egui::Button::new("Check Out")).clicked() {
            chosen = Some(p.cursor);
        }
        *close |= ui.button("Cancel").clicked();
    });

    let item = items.into_iter().nth(chosen?)?;
    *close = true;
    Some(Action::Checkout(item.target, p.local))
}
