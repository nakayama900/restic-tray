//! Snapshots: browse stored snapshots, compare two points in time, restore safely.

use std::collections::BTreeSet;

use chrono::{DateTime, Local};
use egui::{Align, Align2, Layout, Pos2, Rect, RichText, Sense, Ui, Vec2};

use super::{
    Confirm, Job, ResticTrayApp, SnapshotOp, cell, cell_right, format, list_row, spinner_inline,
};
use crate::restic::types::ResticSnapshot;
use crate::theme::{
    Btn, DesignTokens, VectorIcon, badge, body, chip, display, elided_start, empty_state,
    field_label, figure, hairline, help_text, icon_button, input, mono, page_header, paint_icon,
    paint_row_hover, row_hover_t, section_title, strong,
};

impl ResticTrayApp {
    pub(super) fn page_snapshots(&mut self, ui: &mut Ui) {
        let t = self.tokens;
        let loading = self.snapshot_op == Some(SnapshotOp::List);
        let subtitle = if self.snapshots_loaded {
            format!(
                "R2 に {} 件のスナップショットがあります",
                self.snapshots.len()
            )
        } else {
            "R2 に保存したバックアップを確認して、比較や復元を行います".to_owned()
        };
        let mut fetch = false;
        let load_btn_label = "R2 から読み込む";
        page_header(ui, &t, "スナップショット", &subtitle, |ui| {
            let label = if self.snapshots_loaded {
                "再読み込み"
            } else {
                load_btn_label
            };
            fetch = Btn::secondary(label)
                .icon(VectorIcon::Refresh)
                .busy(loading)
                .show(ui, &t)
                .clicked();
        });
        if fetch {
            self.fetch_snapshots();
        }
        ui.add_space(28.0);

        if !self.snapshots_loaded {
            if loading {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| spinner_inline(ui, &t, "R2 から一覧を取得しています"));
                return;
            }
            empty_state(
                ui,
                &t,
                VectorIcon::Snapshots,
                "まだ読み込んでいません",
                "R2 と通信するため、必要なときだけ一覧を取得します。",
            );
            ui.vertical_centered(|ui| {
                if Btn::primary(load_btn_label)
                    .icon(VectorIcon::Download)
                    .show(ui, &t)
                    .clicked()
                {
                    self.fetch_snapshots();
                }
            });
            return;
        }
        if self.snapshots.is_empty() {
            empty_state(
                ui,
                &t,
                VectorIcon::Snapshots,
                "スナップショットはまだありません",
                "バックアップが完了すると、ここに並びます。",
            );
            return;
        }

        self.snapshot_filters(ui);
        ui.add_space(16.0);

        let width = ui.available_width();
        if width < 760.0 {
            self.snapshot_list(ui, 360.0);
            ui.add_space(28.0);
            self.snapshot_detail(ui);
        } else {
            let list_w = (width * 0.4).clamp(300.0, 400.0);
            // ui.vertical keeps the full available height, which the nested scroll areas need.
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(list_w);
                    self.snapshot_list(ui, 640.0);
                });
                ui.add_space(28.0);
                ui.vertical(|ui| {
                    ui.set_width(width - list_w - 36.0);
                    self.snapshot_detail(ui);
                });
            });
        }
    }

    fn snapshot_filters(&mut self, ui: &mut Ui) {
        let t = self.tokens;
        let mut projects = BTreeSet::new();
        let mut hosts = BTreeSet::new();
        for snap in &self.snapshots {
            if let Some(pk) = snap.project_key() {
                projects.insert(pk.to_owned());
            }
            let h = snap.normalized_host();
            if !h.is_empty() {
                hosts.insert(h);
            }
        }
        if projects.is_empty() && hosts.len() <= 1 {
            return;
        }

        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new("プロジェクト")
                    .font(body(12.5))
                    .color(t.text_3),
            );
            let proj_label = match self.snapshot_filter_project.as_deref() {
                None => "すべて".to_owned(),
                Some("__local__") => "この PC 専用 / 旧形式".to_owned(),
                Some(k) => k.to_owned(),
            };
            egui::ComboBox::from_id_salt("snap-filter-project")
                .selected_text(RichText::new(proj_label).font(body(13.0)))
                .width(180.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.snapshot_filter_project, None, "すべて");
                    ui.selectable_value(
                        &mut self.snapshot_filter_project,
                        Some("__local__".to_owned()),
                        "この PC 専用 / 旧形式",
                    );
                    for pk in &projects {
                        ui.selectable_value(
                            &mut self.snapshot_filter_project,
                            Some(pk.clone()),
                            pk,
                        );
                    }
                });

            ui.add_space(12.0);
            ui.label(RichText::new("ホスト").font(body(12.5)).color(t.text_3));
            let host_label = self
                .snapshot_filter_host
                .clone()
                .unwrap_or_else(|| "すべて".to_owned());
            egui::ComboBox::from_id_salt("snap-filter-host")
                .selected_text(RichText::new(host_label).font(body(13.0)))
                .width(150.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.snapshot_filter_host, None, "すべて");
                    for h in &hosts {
                        ui.selectable_value(&mut self.snapshot_filter_host, Some(h.clone()), h);
                    }
                });

            if (self.snapshot_filter_project.is_some() || self.snapshot_filter_host.is_some())
                && Btn::ghost("絞り込みを解除").show(ui, &t).clicked()
            {
                self.snapshot_filter_project = None;
                self.snapshot_filter_host = None;
            }
        });
    }

    fn filtered_snapshots(&self) -> Vec<ResticSnapshot> {
        self.snapshots
            .iter()
            .filter(|snap| {
                let proj_ok = match self.snapshot_filter_project.as_deref() {
                    None => true,
                    Some("__local__") => snap.project_key().is_none(),
                    Some(k) => snap.project_key() == Some(k),
                };
                let host_ok = match self.snapshot_filter_host.as_deref() {
                    None => true,
                    Some(h) => snap.normalized_host().eq_ignore_ascii_case(h),
                };
                proj_ok && host_ok
            })
            .cloned()
            .collect()
    }

    fn snapshot_list(&mut self, ui: &mut Ui, max_height: f32) {
        let t = self.tokens;
        let now = Local::now();
        let filtered = self.filtered_snapshots();
        if filtered.is_empty() {
            help_text(ui, &t, "絞り込み条件に一致するスナップショットはありません");
            return;
        }
        let mut select = None;
        egui::ScrollArea::vertical()
            .id_salt("snapshot-list")
            .max_height(max_height)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for snap in &filtered {
                    let selected = self.selected_snapshot.as_deref() == Some(snap.id.as_str());
                    let (rect, response) = list_row(ui, 72.0, ("snap", &snap.id), Sense::click());
                    let h = row_hover_t(ui, &response);
                    paint_row_hover(ui.painter(), &t, rect, h, selected);
                    if selected {
                        ui.painter().rect_filled(
                            Rect::from_min_size(
                                Pos2::new(rect.left(), rect.top() + 14.0),
                                Vec2::new(3.0, rect.height() - 28.0),
                            ),
                            egui::CornerRadius::same(2),
                            t.accent_text,
                        );
                    }
                    let at = format::parse_local(&snap.time);
                    let title =
                        at.map_or_else(|| snap.time.clone(), |at| format::day_time(at, now));
                    response.widget_info(|| {
                        egui::WidgetInfo::selected(egui::Role::Button, true, selected, &title)
                    });
                    let x = rect.left() + 16.0;
                    let w = rect.width() - 32.0;
                    cell(
                        ui,
                        Pos2::new(x, rect.top() + 18.0),
                        w - 80.0,
                        &title,
                        strong(14.0),
                        t.text,
                    );
                    if let Some(at) = at {
                        cell_right(
                            ui,
                            rect.right() - 16.0,
                            rect.top() + 18.0,
                            &format::relative(at, now),
                            body(12.5),
                            t.text_3,
                        );
                    }
                    let id = short_id(snap);
                    let mut meta_parts: Vec<String> = vec![id.to_owned()];
                    if !snap.hostname.is_empty() {
                        meta_parts.push(snap.normalized_host());
                    }
                    if let Some(pk) = snap.project_key() {
                        meta_parts.push(pk.to_owned());
                    }
                    if snap.is_safety_backup() {
                        meta_parts.push("[退避]".to_owned());
                    } else if snap.is_handoff_released() {
                        meta_parts.push("[引渡済]".to_owned());
                    }
                    let meta_line = meta_parts.join(" · ");
                    let meta_color = if snap.is_safety_backup() {
                        t.warn
                    } else {
                        t.text_2
                    };
                    cell(
                        ui,
                        Pos2::new(x, rect.top() + 38.0),
                        w,
                        &meta_line,
                        mono(11.5),
                        meta_color,
                    );
                    let paths = elided_start(ui, &snap.paths.join(", "), body(12.0), t.text_3, w);
                    let paths_y = rect.top() + 56.0 - paths.size().y / 2.0;
                    ui.painter().galley(Pos2::new(x, paths_y), paths, t.text_3);
                    if response.clicked() {
                        select = Some(snap.id.clone());
                    }
                }
            });
        if let Some(id) = select
            && self.selected_snapshot.as_deref() != Some(id.as_str())
        {
            self.selected_snapshot = Some(id);
            self.snapshot_files = None;
            self.diff_report = None;
        }
    }

    fn snapshot_detail(&mut self, ui: &mut Ui) {
        let t = self.tokens;
        let now = Local::now();
        let Some(snap) = self
            .selected_snapshot
            .as_deref()
            .and_then(|id| self.snapshots.iter().find(|s| s.id == id))
            .cloned()
        else {
            help_text(ui, &t, "左の一覧からスナップショットを選んでください");
            return;
        };
        let at = format::parse_local(&snap.time);
        let title = at.map_or_else(|| snap.time.clone(), |at| format::day_time(at, now));

        ui.label(RichText::new(&title).font(strong(24.0)).color(t.text));
        ui.horizontal(|ui| {
            ui.label(RichText::new(&snap.id).font(mono(12.0)).color(t.text_3));
            if icon_button(ui, &t, VectorIcon::Copy, "ID をコピー").clicked() {
                ui.ctx().copy_text(snap.id.clone());
                self.toast_ok("スナップショット ID をコピーしました");
            }
        });
        ui.add_space(14.0);
        meta_row(ui, &t, "ホスト", &host_line(&snap));
        if let Some(pk) = snap.project_key() {
            meta_row(ui, &t, "プロジェクト", pk);
        }
        meta_row(ui, &t, "パス", &snap.paths.join("\n"));
        if !snap.tags.is_empty() {
            ui.horizontal_wrapped(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::new(72.0, 26.0), Sense::hover());
                ui.painter().text(
                    Pos2::new(r.left(), r.center().y),
                    Align2::LEFT_CENTER,
                    "タグ",
                    body(13.0),
                    t.text_3,
                );
                for tag in &snap.tags {
                    chip(ui, &t, tag, false, false);
                }
            });
        }
        if snap.is_safety_backup() {
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                badge(
                    ui,
                    "引き継ぎ前の退避スナップショット（自動整理から保護）",
                    t.warn,
                );
                if Btn::danger_ghost("この退避スナップショットを削除")
                    .icon(VectorIcon::Trash)
                    .busy(self.job_running(Job::ForgetSnapshot))
                    .show(ui, &t)
                    .clicked()
                {
                    self.confirm = Some(Confirm::ForgetSnapshot {
                        snapshot_id: snap.id.clone(),
                        snapshot_label: format!("{title}（{}）", short_id(&snap)),
                    });
                }
            });
        }

        ui.add_space(24.0);
        hairline(ui, &t);
        ui.add_space(20.0);
        self.snapshot_files_section(ui, &snap);
        ui.add_space(24.0);
        hairline(ui, &t);
        ui.add_space(20.0);
        self.snapshot_compare_section(ui, &snap, now);
        ui.add_space(24.0);
        hairline(ui, &t);
        ui.add_space(20.0);
        self.snapshot_restore_section(ui, &snap, &title);
    }

    fn snapshot_files_section(&mut self, ui: &mut Ui, snap: &ResticSnapshot) {
        let t = self.tokens;
        let mut load = false;
        ui.horizontal(|ui| {
            section_title(ui, &t, "内容");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                load = Btn::secondary("ファイル一覧を表示")
                    .icon(VectorIcon::File)
                    .busy(self.snapshot_op == Some(SnapshotOp::Files))
                    .enabled(self.snapshot_op.is_none())
                    .show(ui, &t)
                    .clicked();
            });
        });
        if load {
            self.fetch_snapshot_files(snap.id.clone());
        }
        let Some((id, files)) = &self.snapshot_files else {
            help_text(ui, &t, "先頭 250 件まで表示します");
            return;
        };
        if id != &snap.id {
            return;
        }
        ui.add_space(8.0);
        if files.is_empty() {
            help_text(ui, &t, "ファイルがありません");
            return;
        }
        egui::ScrollArea::vertical()
            .id_salt("snapshot-files")
            .max_height(280.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for (i, entry) in files.iter().enumerate() {
                    let (rect, response) = list_row(ui, 30.0, ("file", i), Sense::hover());
                    let h = row_hover_t(ui, &response);
                    paint_row_hover(ui.painter(), &t, rect, h, false);
                    let icon = if entry.kind == "dir" {
                        VectorIcon::Folder
                    } else {
                        VectorIcon::File
                    };
                    paint_icon(
                        ui.painter(),
                        Rect::from_center_size(
                            Pos2::new(rect.left() + 16.0, rect.center().y),
                            Vec2::splat(15.0),
                        ),
                        icon,
                        t.text_3,
                    );
                    let size = entry.size.map(format::bytes).unwrap_or_default();
                    let path =
                        elided_start(ui, &entry.path, mono(12.5), t.text, rect.width() - 140.0);
                    let y = rect.center().y - path.size().y / 2.0;
                    ui.painter()
                        .galley(Pos2::new(rect.left() + 34.0, y), path, t.text);
                    cell_right(
                        ui,
                        rect.right() - 10.0,
                        rect.center().y,
                        &size,
                        display(13.0),
                        t.text_3,
                    );
                }
            });
        if files.len() >= 250 {
            help_text(ui, &t, "先頭 250 件だけを表示しています");
        }
    }

    fn snapshot_compare_section(
        &mut self,
        ui: &mut Ui,
        snap: &ResticSnapshot,
        now: DateTime<Local>,
    ) {
        let t = self.tokens;
        section_title(ui, &t, "比較");
        help_text(
            ui,
            &t,
            "別のスナップショットと比較して、追加・削除・変更されたファイルを確認します",
        );
        ui.add_space(10.0);
        let others: Vec<&ResticSnapshot> =
            self.snapshots.iter().filter(|s| s.id != snap.id).collect();
        if others.is_empty() {
            help_text(ui, &t, "比較できるスナップショットが他にありません");
            return;
        }
        let label_of = |s: &ResticSnapshot| {
            let when = format::parse_local(&s.time)
                .map_or_else(|| s.time.clone(), |at| format::day_time(at, now));
            format!("{when}　{}", short_id(s))
        };
        let current = self
            .compare_snapshot
            .as_deref()
            .and_then(|id| others.iter().find(|s| s.id == id))
            .map_or_else(|| "比較するスナップショット".to_owned(), |s| label_of(s));
        let mut pick = None;
        let mut run = false;
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("compare-with")
                .selected_text(RichText::new(current).font(body(13.5)))
                .width(300.0)
                .height(320.0)
                .show_ui(ui, |ui| {
                    for s in &others {
                        let selected = self.compare_snapshot.as_deref() == Some(s.id.as_str());
                        if ui
                            .selectable_label(selected, RichText::new(label_of(s)).font(body(13.5)))
                            .clicked()
                        {
                            pick = Some(s.id.clone());
                        }
                    }
                });
            run = Btn::secondary("差分を表示")
                .icon(VectorIcon::Compare)
                .enabled(self.compare_snapshot.is_some() && self.snapshot_op.is_none())
                .busy(self.snapshot_op == Some(SnapshotOp::Diff))
                .show(ui, &t)
                .clicked();
        });
        if let Some(id) = pick {
            self.compare_snapshot = Some(id);
            self.diff_report = None;
        }
        if let Some(other) = self
            .compare_snapshot
            .as_deref()
            .and_then(|id| self.snapshots.iter().find(|s| s.id == id))
            && (other.normalized_host() != snap.normalized_host() || other.paths != snap.paths)
        {
            ui.add_space(6.0);
            help_text(
                ui,
                &t,
                "注意: ホストまたはルートパスが異なるスナップショット同士を比較すると、Restic の仕様によりツリー全体が削除＋追加として集計されます。",
            );
        }
        if run && let Some(other_id) = self.compare_snapshot.clone() {
            let other_time = self
                .snapshots
                .iter()
                .find(|s| s.id == other_id)
                .and_then(|s| format::parse_local(&s.time));
            let this_time = format::parse_local(&snap.time);
            // Always diff older -> newer so "added" means "appeared later".
            let (older, newer) = if other_time <= this_time {
                (other_id, snap.id.clone())
            } else {
                (snap.id.clone(), other_id)
            };
            self.fetch_diff(older, newer);
        }

        let Some(diff) = &self.diff_report else {
            return;
        };
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            for (label, value, sub, color) in [
                (
                    "追加",
                    diff.added_files,
                    format!("+{}", format::bytes(diff.added_bytes)),
                    t.ok(),
                ),
                (
                    "削除",
                    diff.removed_files,
                    format!("-{}", format::bytes(diff.removed_bytes)),
                    t.err,
                ),
                ("変更", diff.modified_files, String::new(), t.text_2),
            ] {
                ui.vertical(|ui| {
                    ui.set_width(150.0);
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                        ui.painter().circle_filled(r.center(), 3.5, color);
                        ui.label(RichText::new(label).font(body(12.5)).color(t.text_2));
                    });
                    figure(ui, &t, &format::count(value), "件", 34.0);
                    if !sub.is_empty() {
                        ui.label(RichText::new(sub).font(display(13.0)).color(t.text_3));
                    }
                });
            }
        });
        if !diff.changed_paths.is_empty() {
            ui.add_space(10.0);
            egui::ScrollArea::vertical()
                .id_salt("diff-paths")
                .max_height(200.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for path in diff.changed_paths.iter().take(200) {
                        ui.label(RichText::new(path).font(mono(12.0)).color(t.text_2));
                    }
                });
        }
    }

    fn snapshot_restore_section(&mut self, ui: &mut Ui, snap: &ResticSnapshot, title: &str) {
        let t = self.tokens;
        section_title(ui, &t, "復元");
        help_text(
            ui,
            &t,
            "Windows フォルダやドライブ直下などのシステム領域には復元できません",
        );
        ui.add_space(12.0);
        field_label(ui, &t, "復元先フォルダ");
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 110.0).max(160.0);
            ui.add(input(&t, &mut self.restore_dir, r"D:\restore\2026-11-07").desired_width(w));
            if Btn::secondary("参照")
                .icon(VectorIcon::Folder)
                .show(ui, &t)
                .clicked()
                && let Some(dir) = rfd::FileDialog::new()
                    .set_title("復元先フォルダを選択")
                    .pick_folder()
            {
                self.restore_dir = dir.to_string_lossy().into_owned();
            }
        });
        ui.add_space(10.0);
        field_label(ui, &t, "一部だけ復元する（任意）");
        ui.add(
            input(
                &t,
                &mut self.restore_includes,
                "/D/workspaces/handbook/notes",
            )
            .font(mono(13.0))
            .desired_width(f32::INFINITY),
        );
        help_text(
            ui,
            &t,
            "スナップショット内のパスをカンマで区切ります。空欄ならすべて復元します",
        );
        ui.add_space(14.0);
        let busy = self.job_running(Job::Restore);
        let clicked = Btn::primary("復元する")
            .icon(VectorIcon::Download)
            .busy(busy)
            .enabled(!self.restore_dir.trim().is_empty())
            .show(ui, &t)
            .clicked();
        if clicked {
            self.confirm = Some(Confirm::Restore {
                snapshot_id: snap.id.clone(),
                snapshot_label: format!("{title}（{}）", short_id(snap)),
                target_dir: self.restore_dir.trim().to_owned(),
                includes: format::split_list(&self.restore_includes),
            });
        }
    }
}

fn short_id(snap: &ResticSnapshot) -> &str {
    if snap.short_id.is_empty() {
        &snap.id[..snap.id.len().min(8)]
    } else {
        &snap.short_id
    }
}

fn host_line(snap: &ResticSnapshot) -> String {
    match (snap.hostname.is_empty(), snap.username.is_empty()) {
        (false, false) => format!("{}（{}）", snap.hostname, snap.username),
        (false, true) => snap.hostname.clone(),
        _ => "—".to_owned(),
    }
}

fn meta_row(ui: &mut Ui, t: &DesignTokens, label: &str, value: &str) {
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::new(72.0, 20.0), Sense::hover());
        ui.painter().text(
            Pos2::new(r.left(), r.center().y),
            Align2::LEFT_CENTER,
            label,
            body(13.0),
            t.text_3,
        );
        ui.label(RichText::new(value).font(body(13.5)).color(t.text));
    });
}
