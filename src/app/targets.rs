//! Targets: folders to back up, per-folder settings and the shared exclude list.

use std::path::Path;

use chrono::Local;
use egui::{Align, Align2, Layout, Pos2, Rect, RichText, Sense, Ui, Vec2};

use super::{Confirm, Job, ResticTrayApp, TargetDraft, cell, format, place};
use crate::config::{
    BASIC_EXCLUDE_IF_PRESENT, BASIC_EXCLUDES, BASIC_UNITY_PROJECT_EXCLUDES, BackupTarget,
    WriterState, suggest_project_key,
};
use crate::fs_safety::is_git_repository;
use crate::sync::{ProjectSyncItem, ProjectSyncStateKind};
use crate::theme::{
    Btn, DesignTokens, VectorIcon, badge, body, chip, elided_start, empty_state, field_label,
    hairline, help_text, icon_button, input, mono, page_header, paint_row_hover, panel_frame,
    row_hover_t, section_title, strong, switch, toggle_row,
};

const CHIP_SPACING: Vec2 = Vec2::new(6.0, 6.0);

/// Read-only pattern chips, wrapped to the available width.
fn chips<'a>(ui: &mut Ui, t: &DesignTokens, items: impl Iterator<Item = &'a str>) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = CHIP_SPACING;
        for item in items {
            chip(ui, t, item, true, false);
        }
    });
}

impl ResticTrayApp {
    pub(super) fn page_targets(&mut self, ui: &mut Ui) {
        let t = self.tokens;
        let now = Local::now();
        let cfg = self.controller.get_config();
        let active = cfg
            .targets
            .iter()
            .filter(|x| x.is_active_for_backup())
            .count();
        let checked_suffix = self
            .status
            .project_sync_summary
            .last_checked_at
            .as_deref()
            .and_then(format::parse_local)
            .map(|at| format!("　最終確認: {}", format::relative(at, now)))
            .unwrap_or_default();
        let subtitle = if cfg.targets.is_empty() {
            format!("バックアップするフォルダを登録します{checked_suffix}")
        } else if active == cfg.targets.len() {
            format!(
                "{} 個のフォルダをバックアップします。ウィンドウへのドロップでも追加できます{checked_suffix}",
                active
            )
        } else {
            format!(
                "{} 個のうち {} 個がこの PC で作業中（自動バックアップ対象）です{checked_suffix}",
                cfg.targets.len(),
                active
            )
        };
        let mut pick = false;
        let mut refresh_sync = false;
        page_header(ui, &t, "フォルダ", &subtitle, |ui| {
            pick = Btn::primary("フォルダを追加")
                .icon(VectorIcon::Plus)
                .show(ui, &t)
                .clicked();
            refresh_sync = Btn::secondary("リポジトリの最新状態を確認")
                .icon(VectorIcon::Refresh)
                .busy(self.sync_loading)
                .show(ui, &t)
                .clicked();
        });
        if pick {
            self.pick_and_add_folder();
        }
        if refresh_sync {
            self.refresh_sync_status_async(true);
        }
        ui.add_space(28.0);

        let sync_items: Vec<ProjectSyncItem> = self
            .sync_report
            .as_ref()
            .map(|r| r.projects.clone())
            .unwrap_or_default();

        if cfg.targets.is_empty() {
            empty_state(
                ui,
                &t,
                VectorIcon::Folder,
                "フォルダがまだありません",
                "「フォルダを追加」を押すか、エクスプローラーからこのウィンドウへドロップしてください。",
            );
            ui.vertical_centered(|ui| {
                if Btn::secondary("フォルダを追加")
                    .icon(VectorIcon::Folder)
                    .show(ui, &t)
                    .clicked()
                {
                    self.pick_and_add_folder();
                }
            });
        } else {
            hairline(ui, &t);
            for target in &cfg.targets {
                let sync_item = sync_items
                    .iter()
                    .find(|item| item.local_target_id.as_deref() == Some(target.id.as_str()));
                self.target_row(ui, target, sync_item, now);
                if self.editing_target.as_deref() == Some(target.id.as_str()) {
                    self.target_editor(ui, target);
                }
                hairline(ui, &t);
            }
        }

        let not_local: Vec<&ProjectSyncItem> = sync_items
            .iter()
            .filter(|item| item.state == ProjectSyncStateKind::NotLocal)
            .collect();
        if !not_local.is_empty() {
            ui.add_space(36.0);
            self.not_local_projects_section(ui, &not_local, now);
        }

        ui.add_space(44.0);
        self.basic_excludes(ui, cfg.use_basic_excludes);
        ui.add_space(36.0);
        self.global_excludes(ui, &cfg.global_excludes);
        ui.add_space(36.0);
        self.size_breakdown(ui);
    }

    fn not_local_projects_section(
        &mut self,
        ui: &mut Ui,
        items: &[&ProjectSyncItem],
        now: chrono::DateTime<Local>,
    ) {
        let t = self.tokens;
        section_title(ui, &t, "他の PC にある共有プロジェクト（この PC 未配置）");
        ui.label(
            RichText::new("リポジトリ上に存在する共有プロジェクトです。「この PC に取得して引き継ぐ」で復元し、この PC での作業を開始できます。")
                .font(body(13.5))
                .color(t.text_2),
        );
        ui.add_space(12.0);
        hairline(ui, &t);
        let handoff_busy =
            self.job_running(Job::TakeoverProject) || self.job_running(Job::ReleaseProject);
        for item in items {
            let Some(key) = item.project_key.as_deref() else {
                continue;
            };
            let host = item.latest_host.as_deref().unwrap_or("他 PC");
            let when = item
                .latest_time
                .as_deref()
                .and_then(format::parse_local)
                .map(|at| format::relative(at, now))
                .unwrap_or_else(|| "時刻不明".to_owned());
            let ready_note = if item.handoff_ready {
                "引き渡し済み"
            } else {
                "作業中の可能性"
            };
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 68.0), Sense::hover());
            cell(
                ui,
                Pos2::new(rect.left() + 8.0, rect.center().y - 11.0),
                rect.width() - 250.0,
                &format!("{}  (project:{key})", item.label),
                strong(14.5),
                t.text,
            );
            cell(
                ui,
                Pos2::new(rect.left() + 8.0, rect.center().y + 11.0),
                rect.width() - 250.0,
                &format!("最新: {host}・{when}（{ready_note}）"),
                body(12.5),
                t.text_2,
            );
            let btn_rect =
                Rect::from_min_max(Pos2::new(rect.right() - 236.0, rect.top()), rect.max);
            let clicked = place(ui, btn_rect, Layout::right_to_left(Align::Center), |ui| {
                Btn::secondary("この PC に取得して引き継ぐ")
                    .icon(VectorIcon::Download)
                    .enabled(!handoff_busy)
                    .show(ui, &t)
                    .clicked()
            });
            if clicked {
                self.open_takeover_confirm(
                    key.to_owned(),
                    item.label.clone(),
                    item.suggested_local_path.clone().unwrap_or_default(),
                );
            }
            hairline(ui, &t);
        }
    }

    fn size_breakdown(&mut self, ui: &mut Ui) {
        const SHOWN: usize = 20;
        let t = self.tokens;
        section_title(ui, &t, "容量の内訳");
        ui.label(
            RichText::new("最新のスナップショットに含まれるファイルのサイズを、フォルダごとに集計します。不要なファイルが含まれていないかを確認できます。")
                .font(body(13.5))
                .color(t.text_2),
        );
        ui.add_space(14.0);
        let mut load = false;
        panel_frame(&t).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let label = if self.sizes.is_some() {
                "もう一度集計する"
            } else {
                "集計する"
            };
            load = Btn::secondary(label)
                .busy(self.sizes_loading)
                .show(ui, &t)
                .clicked();
            if self.sizes_loading {
                ui.add_space(8.0);
                help_text(
                    ui,
                    &t,
                    "集計しています。ファイル数が多いと 1 分ほどかかる場合があります",
                );
            }
            let Some(folders) = &self.sizes else {
                return;
            };
            let total: u64 = folders.iter().map(|f| f.bytes).sum();
            ui.add_space(12.0);
            help_text(
                ui,
                &t,
                &format!(
                    "合計 {}。{} 個のフォルダのうち、サイズの大きい順に {} 件を表示しています",
                    format::bytes(total),
                    format::count(folders.len() as u64),
                    folders.len().min(SHOWN)
                ),
            );
            ui.add_space(6.0);
            for folder in folders.iter().take(SHOWN) {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&folder.path).font(mono(12.5)).color(t.text));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format::bytes(folder.bytes))
                                .font(body(13.0))
                                .color(t.text_2),
                        );
                    });
                });
            }
        });
        if load {
            self.fetch_sizes();
        }
    }

    fn basic_excludes(&mut self, ui: &mut Ui, enabled: bool) {
        let t = self.tokens;
        section_title(ui, &t, "基本の除外パターン");
        ui.label(
            RichText::new("ビルド成果物やキャッシュなど、再生成できるものを除外します。対象項目はアプリの更新で追加されることがあります。")
                .font(body(13.5))
                .color(t.text_2),
        );
        ui.add_space(14.0);
        let mut on = enabled;
        let mut toggled = false;
        panel_frame(&t).show(ui, |ui| {
            ui.set_width(ui.available_width());
            toggled = toggle_row(ui, &t, &mut on, "基本の除外パターンを有効にする", None).changed();
            if on {
                ui.add_space(12.0);
                chips(ui, &t, BASIC_EXCLUDES.iter().copied());
                ui.add_space(12.0);
                help_text(
                    ui,
                    &t,
                    "Unity プロジェクト（ProjectSettings があるフォルダ）直下の次のフォルダも除外します。パッケージやアセット内の同名フォルダはバックアップされます。",
                );
                ui.add_space(6.0);
                chips(ui, &t, BASIC_UNITY_PROJECT_EXCLUDES.iter().copied());
                ui.add_space(12.0);
                help_text(
                    ui,
                    &t,
                    "次のファイルが存在するフォルダは、中身ごと除外します",
                );
                ui.add_space(6.0);
                chips(
                    ui,
                    &t,
                    BASIC_EXCLUDE_IF_PRESENT
                        .iter()
                        .filter_map(|m| m.split(':').next()),
                );
            }
        });
        if toggled
            && let Err(err) = self.controller.update_config(|c| {
                c.use_basic_excludes = on;
                Ok(())
            })
        {
            self.toast_err(err.to_string());
        }
    }

    fn pick_and_add_folder(&mut self) {
        let Some(folder) = rfd::FileDialog::new()
            .set_title("バックアップするフォルダを選択")
            .pick_folder()
        else {
            return;
        };
        match self.upsert_target(&folder, None, None, None, Some(true)) {
            Ok(label) => {
                self.toast_ok(format!("「{label}」をバックアップ対象に追加しました"));
                self.refresh_telemetry_async();
            }
            Err(err) => self.toast_err(err),
        }
    }

    fn target_row(
        &mut self,
        ui: &mut Ui,
        target: &BackupTarget,
        sync_item: Option<&ProjectSyncItem>,
        now: chrono::DateTime<Local>,
    ) {
        let t = self.tokens;
        let is_shared = target.project_key.is_some();
        let row_h = if is_shared { 92.0 } else { 74.0 };
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), row_h), Sense::hover());
        let editing = self.editing_target.as_deref() == Some(target.id.as_str());
        let actions_w = if is_shared { 224.0 } else { 96.0 };
        let body_rect = Rect::from_min_max(
            Pos2::new(rect.left() + 56.0, rect.top()),
            Pos2::new(rect.right() - actions_w, rect.bottom()),
        );
        let response = ui.interact(
            body_rect,
            ui.scope_id().with(("target", &target.id)),
            Sense::click(),
        );
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::Role::Button,
                true,
                format!("{} の除外とタグを編集", target.label),
            )
        });
        let h = row_hover_t(ui, &response);
        paint_row_hover(ui.painter(), &t, rect, h * 0.7, editing);

        // Switch
        let mut on = target.enabled;
        let sw_rect = Rect::from_center_size(
            Pos2::new(rect.left() + 30.0, rect.center().y),
            Vec2::new(40.0, 22.0),
        );
        let toggled = place(ui, sw_rect, Layout::left_to_right(Align::Center), |ui| {
            switch(ui, &t, &mut on, &target.label).changed()
        });
        if toggled {
            let id = target.id.clone();
            match self.controller.update_config(|c| {
                if let Some(x) = c.targets.iter_mut().find(|x| x.id == id) {
                    x.enabled = on;
                }
                Ok(())
            }) {
                Ok(_) => self.refresh_telemetry_async(),
                Err(err) => self.toast_err(err.to_string()),
            }
        }

        // Label, path and meta
        let missing = !Path::new(&target.path).is_dir();
        let is_git = !missing && is_git_repository(Path::new(&target.path));
        let text_color = if target.is_active_for_backup() {
            t.text
        } else {
            t.text_3
        };
        let text_x = body_rect.left();
        let meta_w = 210.0_f32.min(body_rect.width() * 0.38);
        let text_w = (body_rect.width() - meta_w - 16.0).max(120.0);
        let top_y = if is_shared {
            rect.top() + 24.0
        } else {
            rect.center().y - 11.0
        };
        cell(
            ui,
            Pos2::new(text_x, top_y),
            text_w,
            &target.label,
            strong(15.0),
            text_color,
        );
        let path_y = if is_shared {
            rect.top() + 45.0
        } else {
            rect.center().y + 12.0
        };
        let path_g = elided_start(ui, &target.path, mono(12.5), t.text_3, text_w);
        let path_size = path_g.size();
        ui.painter().galley(
            Pos2::new(text_x, path_y - path_size.y / 2.0),
            path_g,
            t.text_3,
        );
        if missing {
            ui.painter().text(
                Pos2::new(text_x + path_size.x + 10.0, path_y),
                Align2::LEFT_CENTER,
                "フォルダが見つかりません",
                body(12.0),
                t.warn,
            );
        }

        if let Some(ref key) = target.project_key {
            let (status_text, status_color) = sync_status_label(target, sync_item, now, &t);
            let sync_line = format!("project:{key}  ·  {status_text}");
            cell(
                ui,
                Pos2::new(text_x, rect.top() + 69.0),
                body_rect.width() - 8.0,
                &sync_line,
                body(12.5),
                status_color,
            );
        }

        let mut meta = Vec::new();
        if is_git {
            meta.push("Git（バックアップ専用）".to_owned());
        }
        if !target.tags.is_empty() {
            meta.push(target.tags.join(" · "));
        }
        if !target.exclude_patterns.is_empty() {
            meta.push(format!("除外 {}", target.exclude_patterns.len()));
        }
        if !target.enabled {
            meta.push("一時停止中".to_owned());
        }
        if !meta.is_empty() {
            let galley = crate::theme::elided(ui, &meta.join("　"), body(12.5), t.text_2, meta_w);
            ui.painter().galley(
                Pos2::new(
                    body_rect.right() - galley.size().x,
                    top_y - galley.size().y / 2.0,
                ),
                galley,
                t.text_2,
            );
        }

        // Actions (edit/remove + handoff button for shared projects)
        let actions = Rect::from_min_max(
            Pos2::new(rect.right() - actions_w + 8.0, rect.top()),
            rect.max,
        );
        let handoff_busy = self.job_running(Job::ReleaseProject)
            || self.job_running(Job::TakeoverProject)
            || self.backup_active();
        let mut trigger_release = false;
        let mut trigger_takeover = false;
        let (edit, remove) = place(ui, actions, Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(4.0);
            let remove = icon_button(ui, &t, VectorIcon::Trash, "対象から外す").clicked();
            let icon = if editing {
                VectorIcon::ChevronDown
            } else {
                VectorIcon::Edit
            };
            let edit = icon_button(ui, &t, icon, "除外とタグを編集").clicked();
            if target.project_key.is_some() {
                ui.add_space(4.0);
                let state = sync_item
                    .map(|s| s.state)
                    .unwrap_or(ProjectSyncStateKind::LocalLatest);
                let interrupted_release =
                    sync_item.is_some_and(|s| s.handoff_status == "releasing");
                let interrupted_takeover = sync_item
                    .is_some_and(|s| s.handoff_status != "idle" && s.handoff_status != "releasing");
                if interrupted_release {
                    trigger_release = Btn::danger_ghost("引き渡しを再開")
                        .icon(VectorIcon::Upload)
                        .enabled(!handoff_busy && !missing)
                        .show(ui, &t)
                        .clicked();
                } else if state == ProjectSyncStateKind::Diverged || interrupted_takeover {
                    trigger_takeover = Btn::danger_ghost("解決する")
                        .enabled(!handoff_busy)
                        .show(ui, &t)
                        .clicked();
                } else if state == ProjectSyncStateKind::RemoteNewer
                    || target.writer_state == WriterState::Standby
                {
                    trigger_takeover = Btn::secondary("引き継ぐ")
                        .icon(VectorIcon::Download)
                        .enabled(!handoff_busy)
                        .show(ui, &t)
                        .clicked();
                } else {
                    trigger_release = Btn::ghost("引き渡す")
                        .icon(VectorIcon::Upload)
                        .enabled(!handoff_busy && !missing)
                        .show(ui, &t)
                        .clicked();
                }
            }
            (edit, remove)
        });

        if trigger_release && let Some(key) = target.project_key.clone() {
            self.confirm = Some(Confirm::ReleaseProject {
                project_key: key,
                label: target.label.clone(),
                path: target.path.clone(),
            });
        }
        if trigger_takeover && let Some(key) = target.project_key.clone() {
            self.open_takeover_confirm(key, target.label.clone(), target.path.clone());
        }

        if edit || response.clicked() {
            if editing {
                self.editing_target = None;
            } else {
                self.editing_target = Some(target.id.clone());
                self.target_draft = TargetDraft {
                    label: target.label.clone(),
                    excludes: target.exclude_patterns.join(", "),
                    tags: target.tags.join(", "),
                    project_key: target.project_key.clone().unwrap_or_default(),
                };
            }
        }
        if remove {
            self.confirm = Some(Confirm::RemoveTarget {
                id: target.id.clone(),
                label: target.label.clone(),
            });
        }
    }

    fn target_editor(&mut self, ui: &mut Ui, target: &BackupTarget) {
        let t = self.tokens;
        let is_git = is_git_repository(Path::new(&target.path));
        let mut save = false;
        let mut cancel = false;
        egui::Frame::new()
            .inner_margin(egui::Margin {
                left: 56,
                right: 16,
                top: 4,
                bottom: 22,
            })
            .show(ui, |ui| {
                let half = (ui.available_width() - 16.0) / 2.0;
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(half);
                        field_label(ui, &t, "表示名");
                        ui.add(
                            input(&t, &mut self.target_draft.label, "フォルダ名を使う")
                                .desired_width(half),
                        );
                    });
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.set_width(half);
                        field_label(ui, &t, "タグ");
                        ui.add(
                            input(&t, &mut self.target_draft.tags, "work, docs")
                                .desired_width(half),
                        );
                    });
                });
                ui.add_space(10.0);
                field_label(ui, &t, "複数 PC 共有キー（projectKey・任意）");
                if let Some(ref existing_key) = target.project_key {
                    ui.horizontal_wrapped(|ui| {
                        chip(ui, &t, &format!("project:{existing_key}"), true, false);
                        let ws_label = match target.writer_state {
                            WriterState::Active => "作業権: この PC (active)",
                            WriterState::Standby => "作業権: 待機中 (standby)",
                        };
                        badge(
                            ui,
                            ws_label,
                            if target.writer_state == WriterState::Active {
                                t.ok()
                            } else {
                                t.text_2
                            },
                        );
                    });
                    help_text(
                        ui,
                        &t,
                        "確定済みの projectKey はスナップショット履歴の連続性を守るため変更できません",
                    );
                } else if is_git {
                    help_text(
                        ui,
                        &t,
                        "Git 管理リポジトリ（.git を検出）のためバックアップ専用です。PC 間の受け渡しは GitHub を利用してください",
                    );
                } else {
                    ui.horizontal(|ui| {
                        let w = (ui.available_width() - 130.0).max(160.0);
                        ui.add(
                            input(
                                &t,
                                &mut self.target_draft.project_key,
                                "空欄ならこの PC 専用（例: project-alpha）",
                            )
                            .font(mono(13.0))
                            .desired_width(w),
                        );
                        if Btn::ghost("候補を入力").show(ui, &t).clicked() {
                            self.target_draft.project_key = suggest_project_key(&target.label);
                        }
                    });
                    help_text(
                        ui,
                        &t,
                        "英小文字・数字・ドット・ハイフン・アンダースコア（最大 64 文字）。確定後は変更不可になります",
                    );
                }
                ui.add_space(10.0);
                field_label(ui, &t, "このフォルダの除外パターン");
                ui.add(
                    input(&t, &mut self.target_draft.excludes, "*.log, cache/")
                        .font(mono(13.0))
                        .desired_width(f32::INFINITY),
                );
                help_text(
                    ui,
                    &t,
                    "カンマで区切ります。共通の除外パターンと合わせて適用されます",
                );
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    save = Btn::primary("保存").show(ui, &t).clicked();
                    cancel = Btn::ghost("キャンセル").show(ui, &t).clicked();
                });
            });
        if cancel {
            self.editing_target = None;
        }
        if save {
            let draft = &self.target_draft;
            let label = draft.label.trim().to_owned();
            let project_key_update = if target.project_key.is_some() {
                None
            } else {
                let pk = draft.project_key.trim();
                (!pk.is_empty()).then(|| Some(pk.to_owned()))
            };
            let result = self.upsert_target_full(
                Path::new(&target.path),
                (!label.is_empty()).then_some(label),
                Some(format::split_list(&draft.excludes)),
                Some(format::split_list(&draft.tags)),
                None,
                project_key_update,
                None,
            );
            match result {
                Ok(label) => {
                    self.toast_ok(format!("「{label}」の設定を保存しました"));
                    self.editing_target = None;
                    self.refresh_telemetry_async();
                }
                Err(err) => self.toast_err(err),
            }
        }
    }

    fn global_excludes(&mut self, ui: &mut Ui, excludes: &[String]) {
        let t = self.tokens;
        section_title(ui, &t, "共通の除外パターン");
        ui.label(
            RichText::new("基本の除外パターンに加え、すべてのフォルダに適用されます。大文字と小文字は区別されます。")
                .font(body(13.5))
                .color(t.text_2),
        );
        ui.add_space(14.0);
        let mut next = excludes.to_vec();
        let mut changed = false;
        panel_frame(&t).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if excludes.is_empty() {
                help_text(ui, &t, "まだありません");
            } else {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = CHIP_SPACING;
                    for pattern in excludes {
                        if chip(ui, &t, pattern, true, true) {
                            next.retain(|p| p != pattern);
                            changed = true;
                        }
                    }
                });
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let field = ui.add(
                    input(&t, &mut self.new_exclude, "パターンを追加（例: *.iso）")
                        .font(mono(13.0))
                        .desired_width(280.0),
                );
                let submitted = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let clicked = Btn::secondary("追加")
                    .enabled(!self.new_exclude.trim().is_empty())
                    .show(ui, &t)
                    .clicked();
                if submitted || clicked {
                    for pattern in format::split_list(&self.new_exclude) {
                        if !next.contains(&pattern) {
                            next.push(pattern);
                            changed = true;
                        }
                    }
                    self.new_exclude.clear();
                    if submitted {
                        field.request_focus();
                    }
                }
            });
        });
        if changed
            && let Err(err) = self.controller.update_config(|c| {
                c.global_excludes = next;
                Ok(())
            })
        {
            self.toast_err(err.to_string());
        }
    }
}

fn sync_status_label(
    target: &BackupTarget,
    sync_item: Option<&ProjectSyncItem>,
    now: chrono::DateTime<Local>,
    t: &DesignTokens,
) -> (String, egui::Color32) {
    let Some(item) = sync_item else {
        return match target.writer_state {
            WriterState::Active => ("この PC で作業中".to_owned(), t.ok()),
            WriterState::Standby => ("待機中（standby）".to_owned(), t.text_3),
        };
    };
    if item.handoff_status == "releasing" {
        return (
            "前回の引き渡しが途中で中断しました（引き渡しを再実行してください）".to_owned(),
            t.err,
        );
    }
    if item.handoff_status != "idle" {
        return (
            "前回の引き継ぎが途中で中断しました（再実行してください）".to_owned(),
            t.err,
        );
    }
    let when = item
        .latest_time
        .as_deref()
        .and_then(format::parse_local)
        .map(|at| format!("・{}", format::relative(at, now)))
        .unwrap_or_default();
    let fp_note = if item.exclude_fingerprint_mismatch {
        "  [除外設定に差異あり]"
    } else {
        ""
    };
    match item.state {
        ProjectSyncStateKind::Diverged => {
            let host = item.latest_host.as_deref().unwrap_or("他 PC");
            (
                format!(
                    "競合・要注意: {host}{when} とローカル変更が競合（自動バックアップ停止中）{fp_note}"
                ),
                t.err,
            )
        }
        ProjectSyncStateKind::RemoteNewer => {
            let host = item.latest_host.as_deref().unwrap_or("他 PC");
            let ready = if item.handoff_ready {
                "引き渡し済み"
            } else {
                "作業中の可能性"
            };
            (format!("最新: {host}{when}（{ready}）{fp_note}"), t.warn)
        }
        ProjectSyncStateKind::Unchecked => {
            let role = if target.writer_state == WriterState::Standby {
                "待機中（standby）"
            } else {
                "この PC で作業中"
            };
            (
                format!("{role}（リモート未確認・最新状態を確認してください）{fp_note}"),
                t.text_2,
            )
        }
        ProjectSyncStateKind::Standby => {
            (format!("引き渡し済み（待機中）{when}{fp_note}"), t.text_2)
        }
        ProjectSyncStateKind::LocalLatest => {
            if item.latest_snapshot_id.is_some() {
                (format!("最新: この PC{when}{fp_note}"), t.ok())
            } else {
                (
                    format!("この PC で作業中（未バックアップ）{fp_note}"),
                    t.text_2,
                )
            }
        }
        ProjectSyncStateKind::NotLocal => ("他 PC にあり".to_owned(), t.warn),
        ProjectSyncStateKind::LocalOnly => ("この PC 専用".to_owned(), t.text_3),
    }
}
