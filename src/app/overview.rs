//! Overview: is my data protected, and what happens next.

use std::collections::HashMap;

use chrono::{DateTime, Datelike, Local, Timelike};
use egui::{
    Align, Align2, CornerRadius, Frame, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke, Ui,
    Vec2,
};

use super::{AppTab, Job, ResticTrayApp, cancel_backup, cell, format, place, run_status_color};
use crate::db::BackupRunRecord;
use crate::theme::{
    Btn, DesignTokens, VectorIcon, body, display, figure, hairline, link_button, paint_icon,
    paint_status_dot, progress_bar, section_title, strong,
};

/// Runs that finished within one hour cell of the coverage grid.
type HourRuns<'a> = Vec<(&'a BackupRunRecord, DateTime<Local>)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    Running,
    Setup,
    Standby,
    Failed,
    Stale,
    Protected,
    Never,
}

impl ResticTrayApp {
    pub(super) fn page_overview(&mut self, ui: &mut Ui) {
        let now = Local::now();
        let remaining = self.setup_steps().iter().filter(|s| !s.done).count();
        let last_real = self.runs.iter().find(|r| !r.dry_run).cloned();
        let last_non_deferred = self
            .runs
            .iter()
            .find(|r| !r.dry_run && r.status != "deferred")
            .cloned();
        let last_success = self
            .runs
            .iter()
            .find(|r| !r.dry_run && r.status == "success")
            .cloned();
        let last_success_at = last_success
            .as_ref()
            .and_then(|r| format::parse_local(&r.finished_at));
        let standby_only = remaining == 0
            && self.status.active_target_count == 0
            && (self.status.total_target_count > 0
                || self.status.project_sync_summary.not_local > 0);

        let health = if self.backup_active() {
            Health::Running
        } else if remaining > 0 {
            Health::Setup
        } else if standby_only {
            Health::Standby
        } else if last_non_deferred
            .as_ref()
            .is_some_and(|r| r.status != "success")
        {
            Health::Failed
        } else if let Some(at) = last_success_at {
            let limit = if self.status.schedule_enabled {
                (self.status.schedule_period_minutes() * 2).max(120)
            } else {
                7 * 24 * 60
            };
            if (now - at).num_minutes() > limit as i64 {
                Health::Stale
            } else {
                Health::Protected
            }
        } else {
            Health::Never
        };

        let width = ui.available_width();
        let actions_w = 228.0;
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(width - actions_w - 40.0, 0.0),
                Layout::top_down(Align::Min),
                |ui| {
                    // allocate_ui_with_layout only advances by the content width; pin it so
                    // the action column lands on the right edge.
                    ui.set_min_width(width - actions_w - 40.0);
                    self.hero(ui, health, remaining, last_success.as_ref(), now);
                },
            );
            ui.add_space(24.0);
            ui.allocate_ui_with_layout(
                Vec2::new(actions_w, 0.0),
                Layout::top_down(Align::Max),
                |ui| {
                    ui.add_space(28.0);
                    self.hero_actions(
                        ui,
                        health,
                        remaining == 0 && self.status.active_target_count > 0,
                    );
                },
            );
        });

        if health == Health::Failed
            && let Some(run) = &last_non_deferred
        {
            ui.add_space(20.0);
            self.failure_note(ui, run, now);
        }
        if self.status.consecutive_deferred_count >= 3 {
            ui.add_space(20.0);
            self.alert_note_with_target(
                ui,
                &format!(
                    "リポジトリがロックされたままです（{} 回連続で延期）",
                    self.status.consecutive_deferred_count
                ),
                "他 PC の整理処理が継続しているか、古いロックが残っている可能性があります。設定画面の「古いロックを解除」を確認してください。",
                "設定画面を開く",
                AppTab::Settings,
            );
        } else if last_real.as_ref().is_some_and(|r| r.status == "deferred") {
            ui.add_space(16.0);
            let t = self.tokens;
            Frame::new()
                .fill(t.warn.gamma_multiply(0.08))
                .stroke(Stroke::new(1.0_f32, t.warn.gamma_multiply(0.40)))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::symmetric(16, 10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new("直近の定期バックアップは他 PC がリポジトリを整理中だったため延期されました（自動再試行されます）")
                            .font(body(13.0))
                            .color(t.warn),
                    );
                });
        }
        let diverged = self.status.project_sync_summary.diverged;
        if diverged > 0 {
            ui.add_space(20.0);
            self.alert_note_with_target(
                ui,
                &format!("競合または引き継ぎ中断の共有プロジェクトが {diverged} 件あります"),
                "他 PC の最新スナップショットとローカル未保存変更が競合しているか、前回の引き継ぎが途中で中断しています。自動バックアップは一時停止されています。",
                "フォルダ画面で確認する",
                AppTab::Targets,
            );
        }
        if let Some(verify) = self.status.last_verify.clone().filter(|v| !v.ok) {
            let when = format::parse_local(&verify.at)
                .map(|at| format::day_time(at, now))
                .unwrap_or_default();
            ui.add_space(20.0);
            self.alert_note(
                ui,
                &format!("{when} の復元確認で問題が見つかりました"),
                &verify.detail,
            );
        }
        if health == Health::Setup {
            ui.add_space(28.0);
            self.setup_checklist(ui);
        }

        ui.add_space(36.0);
        self.facts(ui, now);
        self.multi_pc_summary_strip(ui, now);
        ui.add_space(40.0);
        self.coverage(ui, now);
        ui.add_space(40.0);
        self.recent(ui, now);
    }

    fn hero(
        &self,
        ui: &mut Ui,
        health: Health,
        remaining: usize,
        last_success: Option<&BackupRunRecord>,
        now: DateTime<Local>,
    ) {
        let t = self.tokens;
        let p = &self.status.active_progress;
        let dry = self.job_running(Job::DryRun);
        let (dot, eyebrow) = match health {
            Health::Running if p.waiting_for_lock => (t.warn, "ロック解放を待機中"),
            Health::Running if self.job_running(Job::TakeoverProject) => {
                (t.ok(), "プロジェクトを引き継ぎ中")
            }
            Health::Running if self.job_running(Job::ReleaseProject) => {
                (t.ok(), "プロジェクトを引き渡し中")
            }
            Health::Running if dry => (t.ok(), "テスト実行中"),
            Health::Running => (t.ok(), "バックアップ中"),
            Health::Setup => (t.warn, "セットアップ"),
            Health::Standby => (t.text_2, "待機中"),
            Health::Failed => (t.err, "前回のバックアップに失敗しました"),
            Health::Stale => (t.warn, "しばらくバックアップされていません"),
            Health::Protected => (t.ok(), "保護されています"),
            Health::Never => (t.text_3, "まだバックアップしていません"),
        };
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
            paint_status_dot(ui, r.center(), dot, health == Health::Running);
            ui.label(RichText::new(eyebrow).font(strong(14.0)).color(t.text));
        });
        ui.add_space(14.0);

        let caption = |ui: &mut Ui, text: &str| {
            ui.label(RichText::new(text).font(body(14.0)).color(t.text_2));
        };
        match health {
            Health::Running => {
                let known =
                    p.running && !p.waiting_for_lock && (p.percent_done > 0.0 || p.total_files > 0);
                caption(
                    ui,
                    if p.target_label.is_empty() {
                        "対象フォルダ"
                    } else {
                        &p.target_label
                    },
                );
                if p.waiting_for_lock {
                    ui.label(
                        RichText::new(
                            p.status_note
                                .as_deref()
                                .unwrap_or("他 PC がリポジトリを整理中のため待機しています"),
                        )
                        .font(strong(26.0))
                        .color(t.warn),
                    );
                } else if known {
                    figure(ui, &t, &format!("{:.0}", p.percent_done * 100.0), "%", 88.0);
                } else {
                    ui.label(
                        RichText::new(p.status_note.as_deref().unwrap_or("準備しています"))
                            .font(strong(34.0))
                            .color(t.text),
                    );
                }
                ui.add_space(10.0);
                progress_bar(ui, &t, known.then_some(p.percent_done as f32), 6.0);
                ui.add_space(10.0);
                if known {
                    caption(
                        ui,
                        &format!(
                            "{} / {} ファイル　{} / {}",
                            format::count(p.files_done),
                            format::count(p.total_files),
                            format::bytes(p.bytes_done),
                            format::bytes(p.total_bytes),
                        ),
                    );
                }
                if !p.current_file.is_empty() {
                    let g = crate::theme::elided_start(
                        ui,
                        &p.current_file,
                        crate::theme::mono(12.5),
                        t.text_3,
                        ui.available_width(),
                    );
                    let (r, _) = ui.allocate_exact_size(g.size(), Sense::hover());
                    ui.painter().galley(r.min, g, t.text_3);
                }
            }
            Health::Setup => {
                caption(ui, "準備が整うまで");
                figure(ui, &t, &remaining.to_string(), "ステップ", 88.0);
            }
            Health::Standby => {
                ui.label(
                    RichText::new("この PC で作業中のフォルダはありません（他 PC が最新）")
                        .font(strong(28.0))
                        .color(t.text),
                );
                ui.add_space(6.0);
                caption(
                    ui,
                    "フォルダ画面から共有プロジェクトを「この PC で引き継ぐ」と、自動バックアップ対象になります",
                );
            }
            Health::Failed | Health::Stale | Health::Protected => match last_success
                .and_then(|r| format::parse_local(&r.finished_at).map(|at| (r, at)))
            {
                Some((run, at)) => {
                    caption(ui, "最後に成功したバックアップ");
                    let (n, unit) = format::age_parts(at, now);
                    if unit.is_empty() {
                        ui.label(RichText::new(n).font(strong(52.0)).color(t.text));
                    } else {
                        figure(ui, &t, &n, unit, 88.0);
                    }
                    ui.add_space(6.0);
                    let delta = run
                        .bytes_delta
                        .map(|d| format!("  ·  増減 {}", format::signed_bytes(d)))
                        .unwrap_or_default();
                    caption(
                        ui,
                        &format!(
                            "{}  ·  {}  ·  新規 {} / 変更 {}{delta}  ·  保存 {}",
                            format::day_time(at, now),
                            run.target_summary,
                            format::count(run.files_new),
                            format::count(run.files_changed),
                            format::bytes(run.data_added_bytes),
                        ),
                    );
                }
                None => {
                    ui.label(
                        RichText::new("成功したバックアップはまだありません")
                            .font(strong(30.0))
                            .color(t.text),
                    );
                }
            },
            Health::Never => {
                ui.label(
                    RichText::new("準備ができました")
                        .font(strong(44.0))
                        .color(t.text),
                );
                ui.add_space(4.0);
                caption(
                    ui,
                    "「今すぐバックアップ」で最初のスナップショットを作成します",
                );
            }
        }
    }

    fn hero_actions(&mut self, ui: &mut Ui, health: Health, ready: bool) {
        let t = self.tokens;
        if health == Health::Running {
            if Btn::danger_ghost("中止する")
                .icon(VectorIcon::Stop)
                .large()
                .min_width(ACTION_W)
                .enabled(self.status.active_progress.running)
                .show(ui, &t)
                .clicked()
            {
                cancel_backup(&self.controller);
                self.toast_ok("中止を要求しました。処理中のファイルが完了しだい停止します");
            }
            return;
        }
        let primary = Btn::primary("今すぐバックアップ")
            .icon(VectorIcon::Upload)
            .large()
            .min_width(ACTION_W)
            .enabled(ready)
            .show(ui, &t);
        let primary = if ready {
            primary
        } else {
            primary.on_hover_text("セットアップを終えると実行できます")
        };
        if primary.clicked() {
            self.start_backup(false);
        }
        ui.add_space(6.0);
        if Btn::ghost("テスト実行")
            .icon(VectorIcon::Play)
            .min_width(ACTION_W)
            .enabled(ready)
            .show(ui, &t)
            .on_hover_text("ファイルを送信せずに、対象と除外の設定を確認します")
            .clicked()
        {
            self.start_backup(true);
        }
    }

    fn failure_note(&mut self, ui: &mut Ui, run: &BackupRunRecord, now: DateTime<Local>) {
        let when = format::parse_local(&run.finished_at)
            .map(|at| format::day_time(at, now))
            .unwrap_or_default();
        let message = run
            .error_message
            .as_deref()
            .unwrap_or("理由は記録されていません");
        let title = format!(
            "{when}（{}）の実行でエラーが発生しました",
            format::actor(&run.trigger_source)
        );
        self.alert_note(ui, &title, message);
    }

    /// A red-bordered note with a title, the first part of `message`, and a link to History.
    fn alert_note(&mut self, ui: &mut Ui, title: &str, message: &str) {
        self.alert_note_with_target(ui, title, message, "履歴で詳細を見る", AppTab::History);
    }

    fn alert_note_with_target(
        &mut self,
        ui: &mut Ui,
        title: &str,
        message: &str,
        link_label: &str,
        target_tab: AppTab,
    ) {
        let t = self.tokens;
        let mut open_tab = false;
        Frame::new()
            .fill(t.err.gamma_multiply(0.07))
            .stroke(Stroke::new(1.0_f32, t.err.gamma_multiply(0.45)))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin::symmetric(18, 14))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_top(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::hover());
                    paint_icon(ui.painter(), r, VectorIcon::Alert, t.err);
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new(title).font(strong(14.0)).color(t.text));
                        let short = format::summarize_error_for_ui(message, 280);
                        ui.label(
                            RichText::new(short)
                                .font(crate::theme::mono(12.5))
                                .color(t.text_2),
                        );
                        ui.add_space(2.0);
                        open_tab = link_button(ui, &t, link_label).clicked();
                    });
                });
            });
        if open_tab {
            self.go(target_tab);
        }
    }

    fn multi_pc_summary_strip(&mut self, ui: &mut Ui, now: DateTime<Local>) {
        let s = &self.status.project_sync_summary;
        let shared_total =
            s.local_latest + s.remote_newer + s.diverged + s.not_local + s.standby + s.unchecked;
        if shared_total == 0 && self.status.observed_remote_hosts.is_empty() {
            return;
        }
        let t = self.tokens;
        ui.add_space(14.0);
        let checked_label = s
            .last_checked_at
            .as_deref()
            .and_then(format::parse_local)
            .map(|at| format!("最終確認: {}", format::relative(at, now)))
            .unwrap_or_else(|| "未確認".to_owned());
        let mut summary_parts = vec![
            format!("この PC で作業中 {} 件", self.status.active_target_count),
            format!("他 PC が最新 {} 件", s.remote_newer),
        ];
        if s.standby > 0 {
            summary_parts.push(format!("引き渡し済み待機 {} 件", s.standby));
        }
        if s.unchecked > 0 {
            summary_parts.push(format!("リモート未確認 {} 件", s.unchecked));
        }
        if s.not_local > 0 {
            summary_parts.push(format!("未取得 {} 件", s.not_local));
        }
        if s.diverged > 0 {
            summary_parts.push(format!("要注意 {} 件", s.diverged));
        }
        let mut open_targets = false;
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(format!(
                    "複数 PC 共有 ({})  ·  {}  ·  {checked_label}",
                    self.status.machine_name,
                    summary_parts.join(" / ")
                ))
                .font(body(13.0))
                .color(t.text_2),
            );
            open_targets = link_button(ui, &t, "フォルダ画面で引き継ぎ").clicked();
        });
        if open_targets {
            self.go(AppTab::Targets);
        }
    }

    fn setup_checklist(&mut self, ui: &mut Ui) {
        let t = self.tokens;
        let steps = self.setup_steps();
        let first_open = steps.iter().position(|s| !s.done);
        let mut go_to = None;
        hairline(ui, &t);
        for (i, step) in steps.iter().enumerate() {
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 68.0), Sense::hover());
            let painter = ui.painter();
            let badge = Rect::from_center_size(
                Pos2::new(rect.left() + 16.0, rect.center().y),
                Vec2::splat(28.0),
            );
            if step.done {
                painter.circle_filled(badge.center(), 14.0, t.ok().gamma_multiply(0.16));
                paint_icon(painter, badge.shrink(7.0), VectorIcon::Check, t.ok());
            } else {
                painter.circle_stroke(badge.center(), 13.5, Stroke::new(1.0_f32, t.line_strong));
                painter.text(
                    badge.center(),
                    Align2::CENTER_CENTER,
                    (i + 1).to_string(),
                    display(14.0),
                    t.text_2,
                );
            }
            let text_x = rect.left() + 48.0;
            let title_color = if step.done { t.text_3 } else { t.text };
            painter.text(
                Pos2::new(text_x, rect.center().y - 10.0),
                Align2::LEFT_CENTER,
                step.title,
                strong(15.0),
                title_color,
            );
            painter.text(
                Pos2::new(text_x, rect.center().y + 12.0),
                Align2::LEFT_CENTER,
                if step.done { "完了" } else { step.detail },
                body(13.0),
                t.text_3,
            );
            if !step.done {
                let label = if step.page == AppTab::Targets {
                    "フォルダを追加"
                } else {
                    "設定を開く"
                };
                let btn_rect =
                    Rect::from_min_max(Pos2::new(rect.right() - 200.0, rect.top()), rect.max);
                place(ui, btn_rect, Layout::right_to_left(Align::Center), |ui| {
                    let btn = if Some(i) == first_open {
                        Btn::secondary(label).icon(VectorIcon::ArrowRight)
                    } else {
                        Btn::ghost(label)
                    };
                    if btn.show(ui, &t).clicked() {
                        go_to = Some(step.page);
                    }
                });
            }
            hairline(ui, &t);
        }
        if let Some(page) = go_to {
            self.go(page);
        }
    }

    fn facts(&self, ui: &mut Ui, now: DateTime<Local>) {
        let t = self.tokens;
        let rep = &self.status;
        let next = rep
            .next_scheduled_at
            .as_deref()
            .and_then(format::parse_local)
            .filter(|_| rep.schedule_enabled);
        let (sched_value, sched_sub) = match next {
            Some(next) => (
                format::clock(next),
                format!(
                    "{}・{}",
                    format::until(next, now),
                    format::schedule(&rep.daily_at, rep.interval_minutes)
                ),
            ),
            None => ("オフ".to_owned(), "設定でオンにできます".to_owned()),
        };
        let paused = rep.total_target_count - rep.enabled_target_count;
        let (target_value, target_sub) = if rep.total_target_count == 0 {
            ("0".to_owned(), "フォルダを追加してください".to_owned())
        } else if paused > 0 {
            (
                format!("{} / {}", rep.enabled_target_count, rep.total_target_count),
                format!("{paused} 個は一時停止中"),
            )
        } else {
            (
                rep.enabled_target_count.to_string(),
                "すべて有効".to_owned(),
            )
        };
        let cfg = self.controller.get_config();
        let (store_value, store_sub, store_alert) = if cfg.r2.bucket.trim().is_empty() {
            (
                "未設定".to_owned(),
                "R2 のバケットを指定してください".to_owned(),
                true,
            )
        } else {
            let prefix = cfg.r2.prefix.trim().trim_matches('/');
            (
                cfg.r2.bucket.trim().to_owned(),
                if prefix.is_empty() {
                    "バケット直下".to_owned()
                } else {
                    format!("/{prefix}")
                },
                false,
            )
        };
        let bin = &rep.restic_binary;
        let (restic_value, restic_sub) = if bin.available {
            (
                bin.version_output
                    .as_deref()
                    .and_then(format::restic_version)
                    .unwrap_or("検出済み")
                    .to_owned(),
                match bin.source.as_str() {
                    "sidecar" => "同梱の restic",
                    "config_override" => "指定したファイル",
                    _ => "PATH 上の restic",
                }
                .to_owned(),
            )
        } else {
            ("なし".to_owned(), "restic.exe が見つかりません".to_owned())
        };
        let facts = [
            ("次の自動実行", sched_value, sched_sub, false),
            ("対象フォルダ", target_value, target_sub, false),
            ("保存先バケット", store_value, store_sub, store_alert),
            ("restic", restic_value, restic_sub, !bin.available),
        ];

        hairline(ui, &t);
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 104.0), Sense::hover());
        let col_w = rect.width() / facts.len() as f32;
        for (i, (label, value, sub, alert)) in facts.iter().enumerate() {
            let x = rect.left() + col_w * i as f32;
            let pad = if i == 0 { 0.0 } else { 22.0 };
            if i > 0 {
                ui.painter().vline(
                    x,
                    rect.top() + 20.0..=rect.bottom() - 20.0,
                    Stroke::new(1.0_f32, t.line),
                );
            }
            let w = col_w - pad - 14.0;
            cell(
                ui,
                Pos2::new(x + pad, rect.top() + 26.0),
                w,
                label,
                body(12.5),
                t.text_3,
            );
            let value_color = if *alert { t.warn } else { t.text };
            cell(
                ui,
                Pos2::new(x + pad, rect.top() + 55.0),
                w,
                value,
                display(24.0),
                value_color,
            );
            cell(
                ui,
                Pos2::new(x + pad, rect.top() + 82.0),
                w,
                sub,
                body(12.5),
                t.text_2,
            );
        }
        hairline(ui, &t);
    }

    fn coverage(&self, ui: &mut Ui, now: DateTime<Local>) {
        let t = self.tokens;
        ui.horizontal(|ui| {
            section_title(ui, &t, "直近 7 日間");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                for (label, kind) in [
                    ("なし", 0),
                    ("テスト", 3),
                    ("延期", 5),
                    ("失敗", 2),
                    ("成功", 1),
                ] {
                    ui.label(RichText::new(label).font(body(12.5)).color(t.text_2));
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                    paint_cell(ui, &t, r, kind, 1.0);
                    ui.add_space(8.0);
                }
            });
        });
        ui.add_space(12.0);

        // Bucket runs by (days ago, hour). Runs are newest first, so stop at the first one
        // older than the grid.
        let today = now.date_naive();
        let mut cells: HashMap<(i64, u32), HourRuns<'_>> = HashMap::new();
        for run in &self.runs {
            let Some(at) = format::parse_local(&run.finished_at) else {
                continue;
            };
            let days_ago = (today - at.date_naive()).num_days();
            if days_ago > 6 {
                break;
            }
            cells
                .entry((days_ago, at.hour()))
                .or_default()
                .push((run, at));
        }
        let next_slot = self
            .status
            .next_scheduled_at
            .as_deref()
            .and_then(format::parse_local)
            .filter(|_| self.status.schedule_enabled)
            .filter(|n| n.date_naive() == today)
            .map(|n| n.hour());

        let since = (ui.input(|i| i.time) - self.page_shown_at) as f32;
        let label_w = 78.0;
        let row_h = 22.0;
        let slot_w = (ui.available_width() - label_w) / 24.0;
        let (grid, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), row_h * 7.0), Sense::hover());
        for day in 0..7_i64 {
            let row = Rect::from_min_size(
                Pos2::new(grid.left(), grid.top() + row_h * day as f32),
                Vec2::new(grid.width(), row_h),
            );
            let date = today - chrono::Days::new(day as u64);
            let label = match day {
                0 => "今日".to_owned(),
                1 => "昨日".to_owned(),
                _ => format!("{}/{} {}", date.month(), date.day(), format::weekday(&date)),
            };
            ui.painter().text(
                Pos2::new(row.left(), row.center().y),
                Align2::LEFT_CENTER,
                label,
                body(12.5),
                if day == 0 { t.text } else { t.text_2 },
            );
            for hour in 0..24_u32 {
                let x = row.left() + label_w + hour as f32 * slot_w;
                let r = Rect::from_min_size(Pos2::new(x, row.top()), Vec2::new(slot_w, row_h));
                let runs = cells.get(&(day, hour));
                let future = day == 0 && hour > now.hour();
                let kind = match runs {
                    Some(runs)
                        if runs
                            .iter()
                            .any(|(r, _)| r.status != "success" && r.status != "deferred") =>
                    {
                        2
                    }
                    Some(runs) if runs.iter().any(|(r, _)| r.status == "deferred") => 5,
                    Some(runs) if runs.iter().any(|(r, _)| !r.dry_run) => 1,
                    Some(_) => 3,
                    None if future => 4,
                    None => 0,
                };
                // Sweep in along the time axis when the page opens.
                let delay = hour as f32 * 0.014 + day as f32 * 0.03;
                let k = ((since - 0.08 - delay) / 0.22).clamp(0.0, 1.0);
                if k < 1.0 {
                    ui.ctx().request_repaint();
                }
                paint_cell(ui, &t, r, kind, 1.0 - (1.0 - k).powi(3));
                if next_slot == Some(hour) && day == 0 {
                    ui.painter().circle_stroke(
                        r.center(),
                        DOT_R + 2.5,
                        Stroke::new(1.5_f32, t.accent_text),
                    );
                }
                let response =
                    ui.interact(r, ui.scope_id().with(("cov", day, hour)), Sense::hover());
                if response.hovered() {
                    ui.painter().circle_stroke(
                        r.center(),
                        DOT_R + 4.0,
                        Stroke::new(1.5_f32, t.text),
                    );
                }
                response.on_hover_ui(|ui| {
                    ui.label(
                        RichText::new(format!("{}月{}日 {hour}時台", date.month(), date.day()))
                            .font(strong(13.0))
                            .color(t.text),
                    );
                    match runs {
                        None if next_slot == Some(hour) && day == 0 => {
                            ui.label(RichText::new("次の自動実行の予定").color(t.text_2));
                        }
                        None => {
                            ui.label(RichText::new("実行なし").color(t.text_3));
                        }
                        Some(runs) => {
                            for (run, at) in runs {
                                let status = match (run.status.as_str(), run.dry_run) {
                                    ("deferred", _) => "延期",
                                    ("success", true) => "テスト",
                                    ("success", false) => "成功",
                                    _ => "失敗",
                                };
                                ui.label(
                                    RichText::new(format!(
                                        "{}  {status}  {}  保存 {}",
                                        format::clock(*at),
                                        format::actor(&run.trigger_source),
                                        format::bytes(run.data_added_bytes)
                                    ))
                                    .color(run_status_color(&t, run)),
                                );
                            }
                        }
                    }
                });
            }
        }
        ui.add_space(2.0);
        let (axis, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 16.0), Sense::hover());
        for hour in [0_u32, 6, 12, 18, 23] {
            let x = axis.left() + label_w + (hour as f32 + 0.5) * slot_w;
            ui.painter().text(
                Pos2::new(x, axis.center().y),
                Align2::CENTER_CENTER,
                format!("{hour}時"),
                body(11.5),
                t.text_3,
            );
        }
    }

    fn recent(&mut self, ui: &mut Ui, now: DateTime<Local>) {
        let t = self.tokens;
        let mut open = false;
        ui.horizontal(|ui| {
            section_title(ui, &t, "最近の実行");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if !self.runs.is_empty() {
                    open = link_button(ui, &t, "すべて表示").clicked();
                }
            });
        });
        ui.add_space(8.0);
        if self.runs.is_empty() {
            ui.label(
                RichText::new(
                    "まだ実行していません。最初のバックアップが終わると、ここに結果が並びます。",
                )
                .font(body(13.5))
                .color(t.text_3),
            );
        } else {
            let runs: Vec<BackupRunRecord> = self.runs.iter().take(6).cloned().collect();
            self.runs_table(ui, &runs, now, false);
        }
        if open {
            self.go(AppTab::History);
        }
    }
}

const DOT_R: f32 = 5.0;
const ACTION_W: f32 = 212.0;

/// Dot-matrix cell. 0 = no run, 1 = success, 2 = failure, 3 = dry run only, 4 = future hour, 5 = deferred.
/// `reveal` (0..=1) grows and fades the dot in for the entrance sweep.
fn paint_cell(ui: &Ui, t: &DesignTokens, rect: Rect, kind: u8, reveal: f32) {
    let c = rect.center();
    let grow = 0.35 + 0.65 * reveal;
    let r = DOT_R.min(rect.width().min(rect.height()) / 2.0) * grow;
    let painter = ui.painter();
    match kind {
        1 => painter.circle_filled(c, r, t.ok().gamma_multiply(reveal)),
        2 => painter.circle_filled(c, r, t.err.gamma_multiply(reveal)),
        3 => painter.circle_stroke(
            c,
            r - 0.6,
            Stroke::new(1.2_f32, t.text_3.gamma_multiply(reveal)),
        ),
        4 => return,
        5 => painter.circle_filled(c, r, t.warn.gamma_multiply(reveal)),
        _ => painter.circle_filled(c, 1.8, t.line_strong.gamma_multiply(reveal)),
    };
}
