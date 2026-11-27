//! History: every backup run, and the operation log written by the GUI, tray and MCP.

use chrono::{DateTime, Local};
use egui::{Color32, Pos2, RichText, Ui, Vec2};

use super::{
    HistoryView, ResticTrayApp, cell, cell_right, format, list_row, place, run_status_color,
};
use crate::db::BackupRunRecord;
use crate::theme::{
    DesignTokens, VectorIcon, badge, body, display, empty_state, hairline, icon_button, mono,
    page_header, paint_row_hover, row_hover_t, segmented, strong,
};

impl ResticTrayApp {
    pub(super) fn page_history(&mut self, ui: &mut Ui) {
        let t = self.tokens;
        let mut refresh = false;
        page_header(
            ui,
            &t,
            "履歴",
            "バックアップの結果と、アプリや MCP から行った操作の記録",
            |ui| {
                refresh = icon_button(ui, &t, VectorIcon::Refresh, "最新の状態に更新").clicked();
            },
        );
        if refresh {
            self.refresh_telemetry_async();
        }
        ui.add_space(20.0);
        segmented(
            ui,
            &t,
            &mut self.history_view,
            &[
                (HistoryView::Runs, "バックアップ"),
                (HistoryView::Operations, "操作ログ"),
            ],
        );
        ui.add_space(20.0);

        let now = Local::now();
        match self.history_view {
            HistoryView::Runs if self.runs.is_empty() => empty_state(
                ui,
                &t,
                VectorIcon::History,
                "まだ実行していません",
                "バックアップを実行すると、結果がここに記録されます。",
            ),
            HistoryView::Runs => {
                let runs: Vec<BackupRunRecord> = self.runs.iter().take(200).cloned().collect();
                self.runs_table(ui, &runs, now, true);
            }
            HistoryView::Operations if self.audits.is_empty() => empty_state(
                ui,
                &t,
                VectorIcon::Terminal,
                "記録された操作はありません",
                "認証情報の保存や復元、MCP からの操作がここに残ります。",
            ),
            HistoryView::Operations => self.operations_table(ui, now),
        }
    }

    /// Run list shared by the overview (compact) and the history page (with error lines).
    pub(super) fn runs_table(
        &self,
        ui: &mut Ui,
        runs: &[BackupRunRecord],
        now: DateTime<Local>,
        show_errors: bool,
    ) {
        let t = self.tokens;
        let cols = RunColumns::new(ui.available_width());
        let (head, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), egui::Sense::hover());
        let y = head.center().y;
        let small = body(12.0);
        cell(
            ui,
            Pos2::new(head.left() + cols.when, y),
            140.0,
            "日時",
            small.clone(),
            t.text_3,
        );
        cell(
            ui,
            Pos2::new(head.left() + cols.source, y),
            80.0,
            "実行元",
            small.clone(),
            t.text_3,
        );
        cell(
            ui,
            Pos2::new(head.left() + cols.targets, y),
            cols.targets_w,
            "対象",
            small.clone(),
            t.text_3,
        );
        cell_right(
            ui,
            head.left() + cols.changes_r,
            y,
            "新規 / 変更",
            small.clone(),
            t.text_3,
        );
        cell_right(
            ui,
            head.left() + cols.delta_r,
            y,
            "増減",
            small.clone(),
            t.text_3,
        );
        cell_right(
            ui,
            head.left() + cols.added_r,
            y,
            "保存量",
            small.clone(),
            t.text_3,
        );
        cell_right(
            ui,
            head.left() + cols.duration_r,
            y,
            "所要時間",
            small,
            t.text_3,
        );
        hairline(ui, &t);
        ui.spacing_mut().item_spacing.y = 0.0;

        for run in runs {
            let ok = run.status == "success";
            let notes = if show_errors {
                run_notes(&t, run)
            } else {
                Vec::new()
            };
            let height = 46.0 + 20.0 * notes.len() as f32;
            let (rect, response) = list_row(ui, height, ("run", run.id), egui::Sense::hover());
            let h = row_hover_t(ui, &response);
            paint_row_hover(ui.painter(), &t, rect, h, false);
            let y = rect.top() + 23.0;
            let color = run_status_color(&t, run);
            ui.painter()
                .circle_filled(Pos2::new(rect.left() + 12.0, y), 4.0, color);
            let when = format::parse_local(&run.finished_at)
                .map(|at| format::day_time(at, now))
                .unwrap_or_else(|| run.finished_at.clone());
            cell(
                ui,
                Pos2::new(rect.left() + cols.when, y),
                140.0,
                &when,
                body(13.5),
                t.text,
            );
            let mut source_x = rect.left() + cols.source;
            let source = cell(
                ui,
                Pos2::new(source_x, y),
                80.0,
                format::actor(&run.trigger_source),
                body(13.0),
                t.text_2,
            );
            source_x = source.right() + 6.0;
            if run.dry_run {
                let g = ui
                    .painter()
                    .layout_no_wrap("テスト".to_owned(), strong(11.0), t.text_2);
                let r = egui::Rect::from_min_size(
                    Pos2::new(source_x, y - 9.0),
                    g.size() + Vec2::new(10.0, 4.0),
                );
                ui.painter()
                    .rect_filled(r, egui::CornerRadius::same(4), t.surface_hover);
                ui.painter()
                    .galley(r.min + Vec2::new(5.0, 2.0), g, t.text_2);
            }
            cell(
                ui,
                Pos2::new(rect.left() + cols.targets, y),
                cols.targets_w,
                &run.target_summary,
                body(13.5),
                t.text,
            );
            let changes = if ok {
                format!(
                    "{} / {}",
                    format::count(run.files_new),
                    format::count(run.files_changed)
                )
            } else {
                "—".to_owned()
            };
            cell_right(
                ui,
                rect.left() + cols.changes_r,
                y,
                &changes,
                display(14.0),
                t.text_2,
            );
            let delta = run
                .bytes_delta
                .filter(|_| ok)
                .map_or_else(|| "—".to_owned(), format::signed_bytes);
            cell_right(
                ui,
                rect.left() + cols.delta_r,
                y,
                &delta,
                display(14.0),
                t.text_2,
            );
            let added = if ok {
                format::bytes(run.data_added_bytes)
            } else {
                "—".to_owned()
            };
            cell_right(
                ui,
                rect.left() + cols.added_r,
                y,
                &added,
                display(14.0),
                t.text_2,
            );
            let deferred = run.status == "deferred";
            let duration = if ok {
                format::duration_ms(run.duration_ms)
            } else if deferred {
                "延期".to_owned()
            } else {
                "失敗".to_owned()
            };
            // Mixed digit + Japanese duration labels ("3.2秒", "3分12秒") use `body`
            // rather than `display` (Bahnschrift), whose ascent and digit advances do
            // not align cleanly with the CJK fallback face.
            cell_right(
                ui,
                rect.left() + cols.duration_r,
                y,
                &duration,
                body(14.0),
                if ok {
                    t.text_2
                } else if deferred {
                    t.warn
                } else {
                    t.err
                },
            );
            let note_w = rect.width() - cols.when - 12.0;
            for (i, (note, color)) in notes.iter().enumerate() {
                cell(
                    ui,
                    Pos2::new(rect.left() + cols.when, rect.top() + 48.0 + 20.0 * i as f32),
                    note_w,
                    note,
                    mono(12.0),
                    *color,
                );
            }
            let files = run
                .files_delta
                .filter(|_| ok)
                .map(|d| format!("前回比ファイル数: {} 件", format::signed_count(d)));
            if !notes.is_empty() || files.is_some() {
                response.on_hover_ui(|ui| {
                    ui.set_max_width(520.0);
                    let lines = notes
                        .iter()
                        .map(|(n, _)| n.as_str())
                        .chain(files.as_deref());
                    for line in lines {
                        ui.label(RichText::new(line).font(mono(12.0)).color(t.text));
                    }
                });
            }
        }
    }

    fn operations_table(&self, ui: &mut Ui, now: DateTime<Local>) {
        let t = self.tokens;
        let w = ui.available_width();
        let (when_x, actor_x, op_x, detail_x) = (12.0, 160.0, 240.0, 420.0);
        let (head, _) = ui.allocate_exact_size(Vec2::new(w, 26.0), egui::Sense::hover());
        let y = head.center().y;
        let small = body(12.0);
        cell(
            ui,
            Pos2::new(head.left() + when_x, y),
            140.0,
            "日時",
            small.clone(),
            t.text_3,
        );
        cell(
            ui,
            Pos2::new(head.left() + actor_x, y),
            70.0,
            "実行元",
            small.clone(),
            t.text_3,
        );
        cell(
            ui,
            Pos2::new(head.left() + op_x, y),
            170.0,
            "操作",
            small.clone(),
            t.text_3,
        );
        cell(
            ui,
            Pos2::new(head.left() + detail_x, y),
            w - detail_x,
            "内容",
            small,
            t.text_3,
        );
        hairline(ui, &t);
        ui.spacing_mut().item_spacing.y = 0.0;
        for log in &self.audits {
            let (rect, response) = list_row(ui, 44.0, ("audit", log.id), egui::Sense::hover());
            let h = row_hover_t(ui, &response);
            paint_row_hover(ui.painter(), &t, rect, h, false);
            let y = rect.center().y;
            let when = format::parse_local(&log.timestamp)
                .map(|at| format::day_time(at, now))
                .unwrap_or_else(|| log.timestamp.clone());
            cell(
                ui,
                Pos2::new(rect.left() + when_x, y),
                140.0,
                &when,
                body(13.5),
                t.text,
            );
            let actor_color = if log.actor == "mcp" {
                t.accent_text
            } else {
                t.text_2
            };
            place(
                ui,
                egui::Rect::from_min_size(
                    Pos2::new(rect.left() + actor_x, y - 11.0),
                    Vec2::new(70.0, 22.0),
                ),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| badge(ui, format::actor(&log.actor), actor_color),
            );
            let failed = log.operation.ends_with("_failed");
            cell(
                ui,
                Pos2::new(rect.left() + op_x, y),
                170.0,
                format::operation(&log.operation),
                strong(13.5),
                if failed { t.err } else { t.text },
            );
            cell(
                ui,
                Pos2::new(rect.left() + detail_x, y),
                rect.width() - detail_x - 12.0,
                &log.detail,
                mono(12.0),
                t.text_3,
            );
            if !log.detail.is_empty() {
                response.on_hover_ui(|ui| {
                    ui.set_max_width(520.0);
                    ui.label(
                        RichText::new(&log.operation)
                            .font(mono(12.0))
                            .color(t.text_2),
                    );
                    ui.label(RichText::new(&log.detail).font(mono(12.0)).color(t.text));
                });
            }
        }
    }
}

/// Lines under a run on the history page: its error or unreadable files, a changed exclude
/// setting that explains jumps in new files, and the folders that stored the most data.
fn run_notes(t: &DesignTokens, run: &BackupRunRecord) -> Vec<(String, Color32)> {
    let ok = run.status == "success";
    let deferred = run.status == "deferred";
    let mut notes = Vec::new();
    if let Some(err) = &run.error_message {
        notes.push((err.clone(), if ok || deferred { t.warn } else { t.err }));
    }
    if ok && run.excludes_changed {
        notes.push((
            "除外設定が前回から変更されたため、新規・増減には設定変更の影響が含まれます".to_owned(),
            t.text_3,
        ));
    }
    if !run.top_added.is_empty() {
        let folders: Vec<String> = run
            .top_added
            .iter()
            .map(|f| format!("{} {}", f.path, format::bytes(f.bytes)))
            .collect();
        notes.push((
            format!("保存量の多いフォルダ: {}", folders.join("  ·  ")),
            t.text_3,
        ));
    }
    notes
}

/// Column offsets for the run table. Numbers are right-aligned so digits line up.
struct RunColumns {
    when: f32,
    source: f32,
    targets: f32,
    targets_w: f32,
    changes_r: f32,
    delta_r: f32,
    added_r: f32,
    duration_r: f32,
}

impl RunColumns {
    fn new(width: f32) -> Self {
        let duration_r = width - 12.0;
        let added_r = duration_r - 96.0;
        let delta_r = added_r - 96.0;
        let changes_r = delta_r - 104.0;
        let targets = 312.0;
        Self {
            when: 32.0,
            source: 180.0,
            targets,
            targets_w: (changes_r - 110.0 - targets).max(60.0),
            changes_r,
            delta_r,
            added_r,
            duration_r,
        }
    }
}
