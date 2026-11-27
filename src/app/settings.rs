//! Settings: storage, credentials, schedule, retention, MCP and app behavior.
//!
//! Non-secret fields edit a `SettingsForm` copy and are saved together from the save bar.
//! Credentials are saved separately because they go to the DPAPI store, not `config.json`.

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use egui::{Align, Frame, Key, Layout, Margin, Modifiers, RichText, Sense, Stroke, Ui, Vec2};

use super::{Confirm, Job, ResticTrayApp, SecretsDraft, SettingsForm, format, spinner_inline};
use crate::config::{MaintenanceRole, default_os_machine_name};
use crate::r2_s3::R2DeleteScope;
use crate::theme::{
    Btn, DesignTokens, VectorIcon, badge, body, disclosure, field_label, hairline, help_text,
    icon_button, input, mono, page_header, paint_status_dot, segmented, stepper, strong,
    toggle_row,
};

const INTERVAL_PRESETS: [(u64, &str); 6] = [
    (15, "15分"),
    (30, "30分"),
    (60, "1時間"),
    (360, "6時間"),
    (720, "12時間"),
    (1_440, "1日"),
];

/// Takes saved settings that changed outside the form (MCP, tray, reload) into an unedited
/// form. Without this, the stale form shows as unsaved and saving it reverts the change.
/// shortcut: a form with pending edits keeps all its values, not just the edited fields;
/// merge per field if edits racing outside changes become common.
fn sync_with_saved<T: PartialEq + Clone>(form: &mut T, base: &mut T, saved: &T) {
    if saved != base {
        if form == base {
            *form = saved.clone();
        }
        *base = saved.clone();
    }
}

impl ResticTrayApp {
    pub(super) fn reset_snapshot_cache(&mut self) {
        self.snapshots.clear();
        self.snapshots_loaded = false;
        self.selected_snapshot = None;
        self.compare_snapshot = None;
        self.snapshot_files = None;
        self.diff_report = None;
        self.sizes = None;
    }

    pub(super) fn settings_save_bar(&mut self, ui: &mut Ui) {
        let t = self.tokens;
        let saved = SettingsForm::from_config(&self.controller.get_config());
        sync_with_saved(&mut self.settings, &mut self.settings_base, &saved);
        self.settings_dirty = self.settings != saved;
        let mut save =
            self.settings_dirty && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::S));
        let mut revert = false;
        let mut shown = self.settings_dirty;
        egui::Panel::bottom("settings-save-bar")
            .frame(
                Frame::new()
                    .fill(t.surface)
                    .inner_margin(Margin::symmetric(44, 14)),
            )
            .show_collapsible(ui, &mut shown, |ui| {
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                    paint_status_dot(ui, r.center(), t.warn, false);
                    ui.label(
                        RichText::new("保存していない変更があります")
                            .font(strong(14.0))
                            .color(t.text),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        save |= Btn::primary("保存")
                            .show(ui, &t)
                            .on_hover_text("Ctrl+S")
                            .clicked();
                        revert = Btn::ghost("元に戻す").show(ui, &t).clicked();
                    });
                });
            });
        if revert {
            self.settings = saved;
        } else if save {
            let form = self.settings.clone();
            match self.controller.update_config(|cfg| {
                form.validate()?;
                form.apply(cfg);
                Ok(())
            }) {
                Ok(cfg) => {
                    self.settings = SettingsForm::from_config(&cfg);
                    self.toast_ok("設定を保存しました");
                    self.refresh_telemetry_async();
                }
                Err(err) => self.toast_err(err.to_string()),
            }
        }
    }

    pub(super) fn page_settings(&mut self, ui: &mut Ui) {
        let t = self.tokens;
        let mut open_file = false;
        let mut reload = false;
        page_header(
            ui,
            &t,
            "設定",
            "設定は config.json に、認証情報は暗号化して別のファイルに保存します",
            |ui| {
                open_file = Btn::ghost("config.json を開く")
                    .icon(VectorIcon::File)
                    .show(ui, &t)
                    .clicked();
                reload = icon_button(ui, &t, VectorIcon::Refresh, "config.json から再読み込み")
                    .clicked();
            },
        );
        if open_file && let Err(err) = open::that(&self.controller.config_path) {
            self.toast_err(format!("config.json を開けませんでした: {err}"));
        }
        if reload {
            match self.controller.reload_config_from_disk() {
                Ok(cfg) => {
                    self.settings = SettingsForm::from_config(&cfg);
                    self.toast_ok("config.json から再読み込みしました");
                    self.refresh_telemetry_async();
                }
                Err(err) => self.toast_err(err.to_string()),
            }
        }
        ui.add_space(24.0);

        section(
            ui,
            &t,
            "保存先",
            "バックアップは暗号化したうえで Cloudflare R2 に保存します。",
            |ui| {
                self.r2_storage_fields(ui);
            },
        );
        section(
            ui,
            &t,
            "認証情報",
            "Windows の DPAPI で暗号化し、この PC のこのユーザーだけが読める形で保存します。config.json には書きません。",
            |ui| self.credential_fields(ui),
        );
        section(
            ui,
            &t,
            "複数 PC 共有",
            "同じリポジトリを複数 PC で共有するときの PC 名、既定の復元先親フォルダ、メンテナンス担当を設定します。",
            |ui| self.multi_pc_fields(ui),
        );
        section(
            ui,
            &t,
            "スケジュール",
            "アプリがトレイにある間、決まった間隔でバックアップします。",
            |ui| {
                self.schedule_fields(ui);
            },
        );
        section(
            ui,
            &t,
            "保持ルール",
            "古いスナップショットを整理するとき、どれだけ残すかを決めます。0 にした項目は使いません。",
            |ui| self.retention_fields(ui),
        );
        section(
            ui,
            &t,
            "AI エージェント連携",
            "Claude Code などの MCP クライアントから、状態の確認やバックアップの実行ができます。",
            |ui| self.agent_fields(ui),
        );
        section(
            ui,
            &t,
            "アプリ",
            "起動時の動作と外観を設定します。",
            |ui| self.app_fields(ui),
        );
    }

    fn r2_storage_fields(&mut self, ui: &mut Ui) {
        let t = self.tokens;
        let s = &mut self.settings;
        field_label(ui, &t, "アカウント ID");
        ui.add(
            input(
                &t,
                &mut s.account_id,
                "Cloudflare ダッシュボードの右側に表示される 32 文字",
            )
            .font(mono(13.0))
            .desired_width(f32::INFINITY),
        );
        ui.add_space(12.0);
        let half = (ui.available_width() - 16.0) / 2.0;
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(half);
                field_label(ui, &t, "バケット");
                ui.add(input(&t, &mut s.bucket, "my-backups").desired_width(half));
            });
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.set_width(half);
                field_label(ui, &t, "バケット内のフォルダ");
                ui.add(input(&t, &mut s.prefix, "main-pc").desired_width(half));
            });
        });
        let mut preview = self.controller.get_config();
        s.apply(&mut preview);
        if let Ok(url) = preview.r2.repository_url() {
            ui.add_space(8.0);
            ui.label(RichText::new(url).font(mono(12.0)).color(t.text_3));
        }

        ui.add_space(14.0);
        disclosure(ui, &t, "詳細設定", &mut self.show_advanced_r2);
        if self.show_advanced_r2 {
            let s = &mut self.settings;
            ui.add_space(8.0);
            field_label(ui, &t, "エンドポイント");
            ui.add(
                input(
                    &t,
                    &mut s.endpoint,
                    "空欄なら https://<アカウント ID>.r2.cloudflarestorage.com",
                )
                .font(mono(13.0))
                .desired_width(f32::INFINITY),
            );
            ui.add_space(12.0);
            field_label(ui, &t, "同時接続数");
            stepper(ui, &t, &mut s.max_connections, 1..=32, "");
            help_text(
                ui,
                &t,
                "restic の s3.connections。回線が細い場合は小さくします",
            );
            ui.add_space(12.0);
            binary_path_picker(
                ui,
                &t,
                "restic.exe の場所",
                &mut s.restic_path,
                "空欄なら同梱版、なければ PATH から探します",
                "restic",
            );
        }

        ui.add_space(14.0);
        self.r2_danger_zone(ui);
    }

    fn r2_danger_zone(&mut self, ui: &mut Ui) {
        let t = self.tokens;
        let active_delete = self
            .status
            .active_r2_delete
            .as_ref()
            .filter(|p| p.running && !p.dry_run)
            .cloned();
        let deleting = self.job_running(Job::DeleteR2) || active_delete.is_some();

        disclosure(
            ui,
            &t,
            "バケット・データの削除（危険な操作）",
            &mut self.show_danger_r2,
        );
        if !self.show_danger_r2 && !deleting && self.last_r2_delete_note.is_none() {
            return;
        }

        let saved_cfg = self.controller.get_config();
        let r2_matches = self.settings.r2_matches_saved(&saved_cfg);
        let r2_configured = saved_cfg.r2.is_configured();
        let ready = self.status.secrets_readiness;
        let has_r2_keys = ready.has_access_key_id && ready.has_secret_access_key;
        let saved_prefix = saved_cfg.r2.normalized_prefix();
        let has_prefix = !saved_prefix.is_empty();

        let can_delete_bucket = r2_matches && r2_configured && has_r2_keys && !deleting;
        let can_delete_prefix = can_delete_bucket && has_prefix;

        let bucket_tip = if deleting {
            "削除処理を実行中です".to_owned()
        } else if !r2_matches {
            "R2 設定の未保存の変更を先に「保存」してください".to_owned()
        } else if !r2_configured {
            "先にアカウント ID とバケット名を保存してください".to_owned()
        } else if !has_r2_keys {
            "先に R2 のアクセスキー ID とシークレットアクセスキーを保存してください".to_owned()
        } else {
            format!(
                "バケット「{}」の全オブジェクトを削除したうえでバケット本体の削除を試行します",
                saved_cfg.r2.normalized_bucket()
            )
        };
        let prefix_tip = if !can_delete_bucket {
            bucket_tip.clone()
        } else if !has_prefix {
            "バケット内のフォルダ（プレフィックス）が未設定のため使用できません".to_owned()
        } else {
            format!(
                "{}/{saved_prefix}/ 配下のオブジェクトのみ削除します（バケット本体は残ります）",
                saved_cfg.r2.normalized_bucket()
            )
        };

        let mut open_prefix_confirm = false;
        let mut open_bucket_confirm = false;
        let mut cancel_delete = false;
        let mut dismiss_note = false;

        ui.add_space(8.0);
        Frame::new()
            .fill(t.err.gamma_multiply(0.05))
            .stroke(Stroke::new(1.0_f32, t.err.gamma_multiply(0.35)))
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(Margin::same(14))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                help_text(
                    ui,
                    &t,
                    "Cloudflare R2 の S3 互換 API でオブジェクトやバケット本体を削除します。削除したデータは復元できません。Object Read & Write トークンではバケット内を空にするまで実行し、本体削除は Cloudflare ダッシュボードから行えます（Admin Read & Write トークンなら本体まで一括削除できます）。",
                );

                if deleting {
                    ui.add_space(10.0);
                    let progress_label = if let Some(p) = &active_delete {
                        format!(
                            "{}（確認 {} 件 / 削除 {} 件・{}）",
                            p.phase,
                            format::count(p.objects_scanned),
                            format::count(p.objects_deleted),
                            format::bytes(p.bytes_deleted),
                        )
                    } else {
                        "R2 の削除処理を実行中…".to_owned()
                    };
                    ui.horizontal_wrapped(|ui| {
                        spinner_inline(ui, &t, &progress_label);
                        if Btn::ghost("中止")
                            .icon(VectorIcon::Stop)
                            .show(ui, &t)
                            .clicked()
                        {
                            cancel_delete = true;
                        }
                    });
                }

                ui.add_space(12.0);
                ui.horizontal_wrapped(|ui| {
                    if Btn::danger_ghost("プレフィックス配下を削除")
                        .icon(VectorIcon::Trash)
                        .busy(deleting)
                        .enabled(can_delete_prefix)
                        .show(ui, &t)
                        .on_hover_text(prefix_tip)
                        .clicked()
                    {
                        open_prefix_confirm = true;
                    }
                    if Btn::danger("バケット全体を削除")
                        .icon(VectorIcon::Trash)
                        .busy(deleting)
                        .enabled(can_delete_bucket)
                        .show(ui, &t)
                        .on_hover_text(bucket_tip)
                        .clicked()
                    {
                        open_bucket_confirm = true;
                    }
                });

                if let Some(note) = &self.last_r2_delete_note {
                    ui.add_space(12.0);
                    Frame::new()
                        .fill(t.bg)
                        .stroke(Stroke::new(1.0_f32, t.line_strong))
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(Margin::symmetric(12, 10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal_top(|ui| {
                                ui.vertical(|ui| {
                                    ui.set_width((ui.available_width() - 36.0).max(160.0));
                                    ui.label(
                                        RichText::new(note).font(body(12.5)).color(t.text),
                                    );
                                });
                                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                                    if icon_button(ui, &t, VectorIcon::Close, "閉じる").clicked() {
                                        dismiss_note = true;
                                    }
                                });
                            });
                        });
                }
            });

        if cancel_delete {
            self.controller.cancel_flag.store(true, Ordering::Relaxed);
            self.toast_ok("R2 削除の中止を要求しました");
        }
        if dismiss_note {
            self.last_r2_delete_note = None;
        }
        if open_prefix_confirm {
            self.open_delete_r2_confirm(R2DeleteScope::PrefixOnly);
        } else if open_bucket_confirm {
            self.open_delete_r2_confirm(R2DeleteScope::EntireBucket);
        }
    }

    fn credential_fields(&mut self, ui: &mut Ui) {
        let t = self.tokens;
        let ready = self.status.secrets_readiness;
        let reveal = self.reveal_secrets;

        if ready.vault_unreadable {
            let vault_path = crate::secrets::credentials_path(&self.controller.base_dir);
            Frame::new()
                .fill(t.err.gamma_multiply(0.08))
                .stroke(Stroke::new(1.0_f32, t.err.gamma_multiply(0.45)))
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(Margin::symmetric(12, 10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new(
                            "認証情報ファイル (credentials.dat) を復号・読み込みできませんでした。",
                        )
                        .font(strong(13.0))
                        .color(t.err),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(format!(
                            "元の認証情報（特にリポジトリのパスワード）を誤って上書きしないよう、通常の保存を停止しています。\
                             別の Windows ユーザーで作成したファイル、またはファイル破損の可能性があります。\
                             元のユーザーで開くか、バックアップを取ったうえで {} を移動・削除してから全項目を再入力してください。",
                            vault_path.display()
                        ))
                        .font(body(12.5))
                        .color(t.text),
                    );
                    if let Some(ref detail) = self.status.secrets_error {
                        ui.add_space(4.0);
                        ui.label(RichText::new(detail).font(mono(11.5)).color(t.text_3));
                    }
                });
            ui.add_space(12.0);
        }

        let rows: [(&str, bool, &mut String); 3] = [
            (
                "アクセスキー ID",
                ready.has_access_key_id,
                &mut self.secrets_draft.access_key_id,
            ),
            (
                "シークレットアクセスキー",
                ready.has_secret_access_key,
                &mut self.secrets_draft.secret_access_key,
            ),
            (
                "リポジトリのパスワード",
                ready.has_restic_password,
                &mut self.secrets_draft.restic_password,
            ),
        ];
        for (label, saved, value) in rows {
            secret_input_row(
                ui,
                &t,
                label,
                saved,
                value,
                if ready.vault_unreadable {
                    "復号エラー（元ファイルを保護中）"
                } else if saved {
                    "変更時のみ入力"
                } else {
                    "未設定"
                },
                reveal,
            );
            ui.add_space(10.0);
        }

        help_text(
            ui,
            &t,
            "パスワードを紛失するとバックアップを復元できません。パスワード管理ツールなどにも控えてください。",
        );
        ui.add_space(14.0);

        let dirty = self.settings_dirty;
        let check_tip = if dirty {
            "未保存の設定変更を先に「保存」してください"
        } else {
            "restic check で R2 への接続とリポジトリの整合性を確認します"
        };
        let init_tip = if dirty {
            "未保存の設定変更を先に「保存」してください"
        } else {
            "新しいバケットで初回のみ実行します"
        };
        let save_tip = if ready.vault_unreadable {
            "credentials.dat が復号できない状態のため、元ファイル保護のため上書き保存を停止しています"
        } else {
            "入力した項目のみ暗号化して更新します（空欄の項目は既存の保存値を維持します）"
        };
        let mut save = false;
        let mut check = false;
        let mut init = false;
        ui.horizontal_wrapped(|ui| {
            save = Btn::primary("認証情報を保存")
                .icon(VectorIcon::Key)
                .enabled(!self.secrets_draft.is_empty() && !ready.vault_unreadable)
                .show(ui, &t)
                .on_hover_text(save_tip)
                .clicked();
            check = Btn::secondary("接続を確認")
                .icon(VectorIcon::ShieldCheck)
                .busy(self.job_running(Job::CheckRepo))
                .enabled(!dirty)
                .show(ui, &t)
                .on_hover_text(check_tip)
                .clicked();
            init = Btn::ghost("リポジトリを作成")
                .icon(VectorIcon::Plus)
                .busy(self.job_running(Job::InitRepo))
                .enabled(!dirty)
                .show(ui, &t)
                .on_hover_text(init_tip)
                .clicked();
            let (icon, label) = if reveal {
                (VectorIcon::EyeOff, "入力内容を隠す")
            } else {
                (VectorIcon::Eye, "入力内容を表示")
            };
            if icon_button(ui, &t, icon, label).clicked() {
                self.reveal_secrets = !reveal;
            }
        });
        if save {
            self.save_secrets();
        }
        if check {
            self.start_repo_job(Job::CheckRepo);
        }
        if init {
            self.confirm = Some(Confirm::InitRepo);
        }
    }

    fn save_secrets(&mut self) {
        let draft = &self.secrets_draft;
        let access_key =
            (!draft.access_key_id.trim().is_empty()).then_some(draft.access_key_id.trim());
        let secret_key =
            (!draft.secret_access_key.trim().is_empty()).then_some(draft.secret_access_key.trim());
        let password =
            (!draft.restic_password.trim().is_empty()).then_some(draft.restic_password.as_str());
        match self
            .controller
            .update_secrets_partial(access_key, secret_key, password)
        {
            Ok(()) => {
                self.secrets_draft.wipe();
                self.secrets_draft = SecretsDraft::default();
                self.toast_ok("認証情報を暗号化して保存しました");
                self.refresh_telemetry_async();
            }
            Err(err) => self.toast_err(err.to_string()),
        }
    }

    fn multi_pc_fields(&mut self, ui: &mut Ui) {
        let t = self.tokens;
        let os_host = default_os_machine_name();
        {
            let s = &mut self.settings;
            field_label(ui, &t, "PC 名（machineName）");
            ui.add(
                input(
                    &t,
                    &mut s.machine_name,
                    &format!("空欄なら OS のホスト名（{os_host}）を使用"),
                )
                .font(mono(13.0))
                .desired_width(f32::INFINITY),
            );
            help_text(
                ui,
                &t,
                "全バックアップの --host に小文字で記録され、どの PC が最新かの判定に使われます",
            );

            ui.add_space(12.0);
            field_label(
                ui,
                &t,
                "既定のプロジェクト親フォルダ（defaultProjectRoot・任意）",
            );
            let mut browse_root = false;
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 110.0).max(160.0);
                ui.add(
                    input(&t, &mut s.default_project_root, r"例: D:\Projects")
                        .font(mono(13.0))
                        .desired_width(w),
                );
                browse_root = Btn::secondary("参照")
                    .icon(VectorIcon::Folder)
                    .show(ui, &t)
                    .clicked();
            });
            if browse_root
                && let Some(dir) = rfd::FileDialog::new()
                    .set_title("既定のプロジェクト親フォルダを選択")
                    .pick_folder()
            {
                s.default_project_root = dir.to_string_lossy().into_owned();
            }
            help_text(
                ui,
                &t,
                "この PC に未配置の共有プロジェクトを引き継ぐ際、このフォルダ配下を復元先候補として提案します",
            );

            ui.add_space(14.0);
            field_label(ui, &t, "メンテナンス担当（maintenanceRole）");
            segmented(
                ui,
                &t,
                &mut s.maintenance_role,
                &[
                    (MaintenanceRole::Primary, "メンテナンス担当 (primary)"),
                    (MaintenanceRole::None, "担当しない (none)"),
                ],
            );
            ui.add_space(4.0);
            help_text(
                ui,
                &t,
                match s.maintenance_role {
                    MaintenanceRole::Primary => {
                        "自動 forget --prune と定期復元確認（check）を実行します。共有リポジトリでは常時稼働に近い 1 台のみ primary にしてください。"
                    }
                    MaintenanceRole::None => {
                        "この PC では自動 forget --prune と定期復元確認を実行しません（ノート PC 等の 2 台目推奨）。手動実行は確認付きで行えます。"
                    }
                },
            );
        }

        ui.add_space(14.0);
        hairline(ui, &t);
        ui.add_space(12.0);
        field_label(ui, &t, "手動メンテナンスとロック復旧");
        let dirty = self.settings_dirty;
        let non_primary = self.settings.maintenance_role == MaintenanceRole::None;
        let mut open_unlock = false;
        let mut open_prune_dry = false;
        let mut open_prune_real = false;
        ui.horizontal_wrapped(|ui| {
            open_unlock = Btn::secondary("古いロックを解除 (unlock)")
                .icon(VectorIcon::Key)
                .busy(self.job_running(Job::UnlockRepo))
                .enabled(!dirty)
                .show(ui, &t)
                .on_hover_text("30 分以上更新されていない失効ロックのみを安全に解除します（--remove-all は使用しません）")
                .clicked();
            open_prune_dry = Btn::ghost("整理対象を確認 (Dry-Run)")
                .busy(self.job_running(Job::ForgetPrune))
                .enabled(!dirty)
                .show(ui, &t)
                .clicked();
            open_prune_real = Btn::ghost("古い世代を整理 (forget --prune)")
                .busy(self.job_running(Job::ForgetPrune))
                .enabled(!dirty)
                .show(ui, &t)
                .clicked();
        });
        if open_unlock {
            self.confirm = Some(Confirm::UnlockRepo);
        } else if open_prune_dry {
            self.confirm = Some(Confirm::ForgetPrune {
                dry_run: true,
                non_primary_warning: false,
            });
        } else if open_prune_real {
            self.confirm = Some(Confirm::ForgetPrune {
                dry_run: false,
                non_primary_warning: non_primary,
            });
        }
        if let Some(preview_text) = self.forget_prune_preview.clone() {
            ui.add_space(10.0);
            let mut clear_preview = false;
            Frame::new()
                .fill(t.bg)
                .stroke(Stroke::new(1.0_f32, t.line_strong))
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(Margin::symmetric(12, 10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical()
                        .id_salt("forget_prune_preview_scroll")
                        .max_height(220.0)
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&preview_text).font(mono(12.0)).color(t.text),
                                )
                                .wrap(),
                            );
                        });
                    ui.add_space(6.0);
                    if crate::theme::link_button(ui, &t, "確認結果を閉じる").clicked() {
                        clear_preview = true;
                    }
                });
            if clear_preview {
                self.forget_prune_preview = None;
            }
        }
    }

    fn schedule_fields(&mut self, ui: &mut Ui) {
        let t = self.tokens;
        let s = &mut self.settings;
        toggle_row(
            ui,
            &t,
            &mut s.schedule_enabled,
            "自動でバックアップする",
            None,
        );
        if s.schedule_enabled {
            ui.add_space(6.0);
            field_label(ui, &t, "実行タイミング");
            segmented(
                ui,
                &t,
                &mut s.daily,
                &[(true, "毎日決まった時刻"), (false, "一定の間隔")],
            );
            ui.add_space(8.0);
            if s.daily {
                let can_remove = s.daily_times.len() > 1;
                let mut remove_idx = None;
                for (i, (hour, minute)) in s.daily_times.iter_mut().enumerate() {
                    if i > 0 {
                        ui.add_space(6.0);
                    }
                    ui.push_id(("daily-slot", i), |ui| {
                        ui.horizontal(|ui| {
                            stepper(ui, &t, hour, 0..=23, " 時");
                            ui.add_space(8.0);
                            stepper(ui, &t, minute, 0..=59, " 分");
                            if can_remove {
                                ui.add_space(4.0);
                                if icon_button(ui, &t, VectorIcon::Close, "この時刻を削除")
                                    .clicked()
                                {
                                    remove_idx = Some(i);
                                }
                            }
                        });
                    });
                }
                if let Some(idx) = remove_idx {
                    s.daily_times.remove(idx);
                }
                ui.add_space(8.0);
                if Btn::secondary("時刻を追加")
                    .icon(VectorIcon::Plus)
                    .show(ui, &t)
                    .clicked()
                {
                    let next = next_suggested_daily_time(&s.daily_times);
                    s.daily_times.push(next);
                }
                ui.add_space(4.0);
                let times_label = s
                    .daily_times
                    .iter()
                    .map(|(h, m)| format!("{}:{:02}", (*h).min(23), (*m).min(59)))
                    .collect::<Vec<_>>()
                    .join("・");
                help_text(
                    ui,
                    &t,
                    &format!(
                        "毎日 {times_label} に実行します。その時刻にスリープ中なら復帰直後、電源オフなら次回起動の 2 分後に実行します",
                    ),
                );
            } else {
                ui.horizontal_wrapped(|ui| {
                    segmented(ui, &t, &mut s.interval_minutes, &INTERVAL_PRESETS);
                    ui.add_space(8.0);
                    ui.add(
                        egui::DragValue::new(&mut s.interval_minutes)
                            .range(1..=10_080)
                            .suffix(" 分")
                            .speed(1.0),
                    )
                    .on_hover_text("その他の間隔は分単位で指定します");
                });
                help_text(ui, &t, &format::interval(s.interval_minutes));
            }
            ui.add_space(6.0);
        }
        hairline(ui, &t);
        toggle_row(
            ui,
            &t,
            &mut s.run_on_startup,
            "アプリの起動時にも実行する",
            Some("次の予定を待たず、起動直後に 1 回実行します"),
        );
        hairline(ui, &t);
        if s.maintenance_role == MaintenanceRole::None {
            ui.add_space(6.0);
            Frame::new()
                .fill(t.warn.gamma_multiply(0.08))
                .stroke(Stroke::new(1.0_f32, t.warn.gamma_multiply(0.40)))
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new(
                            "この PC はメンテナンス担当ではない（maintenanceRole: none）ため、自動 forget --prune と定期復元確認は無効になります。",
                        )
                        .font(body(12.5))
                        .color(t.warn),
                    );
                });
            ui.add_space(6.0);
        } else {
            toggle_row(
                ui,
                &t,
                &mut s.maintenance_slot_enabled,
                "独立したメンテナンス枠で 1 日 1 回整理する（maintenanceAt）",
                Some(
                    "指定時刻以降の最初の定期バックアップ後にのみ forget --prune と復元確認を実行し、他 PC との排他ロック競合を避けます",
                ),
            );
            if s.maintenance_slot_enabled {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    stepper(ui, &t, &mut s.maintenance_hour, 0..=23, " 時");
                    ui.add_space(8.0);
                    stepper(ui, &t, &mut s.maintenance_minute, 0..=59, " 分");
                });
                ui.add_space(6.0);
            } else {
                toggle_row(
                    ui,
                    &t,
                    &mut s.auto_prune,
                    "成功のたびに古いスナップショットを整理する",
                    Some(
                        "保持ルールに従って restic forget --prune を実行します（複数 PC 共有時は上の独立メンテナンス枠を推奨）",
                    ),
                );
            }
        }
        hairline(ui, &t);
        let last_verify = match &self.status.last_verify {
            Some(v) => format!(
                "前回は {} に実施し、{}",
                format::parse_local(&v.at)
                    .map(|at| format::day_time(at, chrono::Local::now()))
                    .unwrap_or_default(),
                if v.ok {
                    "問題はありませんでした"
                } else {
                    "問題が見つかりました"
                }
            ),
            None => "まだ実行していません".to_owned(),
        };
        toggle_row(
            ui,
            &t,
            &mut s.verify_enabled,
            "復元できるか定期的に確認する",
            Some(&format!(
                "{} 日に 1 回、定期バックアップのあとに、保存したデータの {}% を読み戻して確かめます。{last_verify}",
                s.verify_every_days, s.verify_read_percent
            )),
        );
        hairline(ui, &t);
        toggle_row(
            ui,
            &t,
            &mut s.notify_enabled,
            "バックアップが成功しない状態が続いたら通知する",
            Some(&format!(
                "バックアップが {} 時間以上成功していないときに、Windows の通知で知らせます",
                s.notify_overdue_hours
            )),
        );
    }

    fn retention_fields(&mut self, ui: &mut Ui) {
        let t = self.tokens;
        let s = &mut self.settings;
        let fields: [(&str, &mut u32, u32, &str); 4] = [
            ("最新", &mut s.keep_last, 500, " 件"),
            ("日ごと", &mut s.keep_daily, 365, " 日"),
            ("週ごと", &mut s.keep_weekly, 260, " 週"),
            ("月ごと", &mut s.keep_monthly, 120, " か月"),
        ];
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 28.0;
            for (label, value, max, suffix) in fields {
                ui.vertical(|ui| {
                    field_label(ui, &t, label);
                    stepper(ui, &t, value, 0..=max, suffix);
                });
            }
        });
        ui.add_space(10.0);
        let parts: Vec<String> = [
            (s.keep_last, "最新の {} 件"),
            (s.keep_daily, "直近 {} 日は 1 日 1 件"),
            (s.keep_weekly, "直近 {} 週は週 1 件"),
            (s.keep_monthly, "直近 {} か月は月 1 件"),
        ]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, tpl)| tpl.replace("{}", &n.to_string()))
        .collect();
        let summary = if parts.is_empty() {
            "すべて 0 のため、整理してもスナップショットは削除されません".to_owned()
        } else {
            format!("{}を残します", parts.join("、"))
        };
        ui.label(RichText::new(summary).font(body(13.5)).color(t.text_2));
    }

    fn agent_fields(&mut self, ui: &mut Ui) {
        let t = self.tokens;
        let s = &mut self.settings;
        toggle_row(
            ui,
            &t,
            &mut s.agent_enabled,
            "MCP サーバーを有効にする",
            Some("無効にすると、エージェントからの操作をすべて拒否します"),
        );
        hairline(ui, &t);
        toggle_row(
            ui,
            &t,
            &mut s.allow_restore,
            "エージェントによる復元を許可する",
            None,
        );
        hairline(ui, &t);
        toggle_row(
            ui,
            &t,
            &mut s.allow_prune,
            "エージェントによる古いスナップショットの削除を許可する",
            None,
        );
        hairline(ui, &t);
        toggle_row(
            ui,
            &t,
            &mut s.allow_delete_storage,
            "エージェントによる R2 バケット・プレフィックスの削除を許可する",
            Some("既定は無効です。有効にしても、実削除にはバケット名の完全一致確認が必要です"),
        );
        hairline(ui, &t);
        toggle_row(
            ui,
            &t,
            &mut s.allow_handoff,
            "エージェントによるプロジェクト引き継ぎ（takeover_project）を許可する",
            Some(
                "既定は無効です。有効にしても、既定は dryRun: true で、実実行には confirmProjectKey の完全一致確認が必要です",
            ),
        );
        ui.add_space(16.0);

        let exe = std::env::current_exe()
            .unwrap_or_else(|_| PathBuf::from("restic-tray.exe"))
            .to_string_lossy()
            .into_owned();
        let command = format!("claude mcp add --scope user restic-tray -- \"{exe}\" --mcp");
        field_label(ui, &t, "Claude Code への登録コマンド");
        let mut copy = false;
        Frame::new()
            .fill(t.bg)
            .stroke(Stroke::new(1.0_f32, t.line))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(Margin {
                left: 12,
                right: 4,
                top: 4,
                bottom: 4,
            })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let w = ui.available_width() - 40.0;
                    ui.add_sized(
                        Vec2::new(w, 30.0),
                        egui::Label::new(RichText::new(&command).font(mono(12.5)).color(t.text))
                            .truncate(),
                    );
                    copy = icon_button(ui, &t, VectorIcon::Copy, "コマンドをコピー").clicked();
                });
            });
        if copy {
            ui.ctx().copy_text(command);
            self.toast_ok("登録コマンドをコピーしました");
        }
    }

    fn app_fields(&mut self, ui: &mut Ui) {
        let t = self.tokens;
        toggle_row(
            ui,
            &t,
            &mut self.settings.start_minimized,
            "起動時にトレイに格納する",
            Some(
                "ウィンドウを開かずに常駐します。ウィンドウを最小化・閉じたときもトレイに格納されます",
            ),
        );
        hairline(ui, &t);
        ui.add_space(14.0);
        field_label(ui, &t, "テーマ");
        let mut dark = t.is_dark;
        if segmented(ui, &t, &mut dark, &[(true, "ダーク"), (false, "ライト")]).changed() {
            self.set_theme(dark, ui.ctx());
        }
        ui.add_space(22.0);
        if Btn::danger_ghost("アプリを終了")
            .icon(VectorIcon::Power)
            .show(ui, &t)
            .clicked()
        {
            self.confirm = Some(Confirm::Quit);
        }
    }
}

fn binary_path_picker(
    ui: &mut Ui,
    t: &DesignTokens,
    label: &str,
    path: &mut String,
    hint: &str,
    filter_name: &str,
) {
    field_label(ui, t, label);
    let mut browse = false;
    ui.horizontal(|ui| {
        let w = (ui.available_width() - 110.0).max(160.0);
        ui.add(input(t, path, hint).font(mono(13.0)).desired_width(w));
        browse = Btn::secondary("参照")
            .icon(VectorIcon::File)
            .show(ui, t)
            .clicked();
    });
    if browse
        && let Some(file) = rfd::FileDialog::new()
            .set_title(format!("{filter_name}.exe を選択"))
            .add_filter(filter_name, &["exe"])
            .pick_file()
    {
        *path = file.to_string_lossy().into_owned();
    }
}

fn secret_input_row(
    ui: &mut Ui,
    t: &DesignTokens,
    label: &str,
    saved: bool,
    value: &mut String,
    empty_hint: &str,
    reveal: bool,
) {
    ui.horizontal(|ui| {
        field_label(ui, t, label);
        if saved {
            badge(ui, "保存済み", t.ok());
        }
    });
    ui.add(
        input(t, value, empty_hint)
            .password(!reveal)
            .font(mono(13.0))
            .desired_width(f32::INFINITY),
    );
}

/// Suggests the next daily time when adding a slot (e.g. 5:00 -> 17:00).
fn next_suggested_daily_time(existing: &[(u32, u32)]) -> (u32, u32) {
    if let Some(&(last_h, last_m)) = existing.last() {
        let candidate = ((last_h + 12) % 24, last_m);
        if !existing.contains(&candidate) {
            return candidate;
        }
    }
    for h in [5, 17, 12, 0, 9, 21].into_iter().chain(0..24) {
        if !existing.iter().any(|&(eh, _)| eh == h) {
            return (h, 0);
        }
    }
    (0, 0)
}

/// Two-column settings block: heading and explanation on the left, fields on the right.
fn section(ui: &mut Ui, t: &DesignTokens, title: &str, desc: &str, add: impl FnOnce(&mut Ui)) {
    hairline(ui, t);
    ui.add_space(30.0);
    let width = ui.available_width();
    if width < 720.0 {
        ui.label(RichText::new(title).font(strong(16.0)).color(t.text));
        ui.label(RichText::new(desc).font(body(13.0)).color(t.text_2));
        ui.add_space(14.0);
        add(ui);
    } else {
        let left = 248.0;
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(Vec2::new(left, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_min_width(left);
                ui.label(RichText::new(title).font(strong(16.0)).color(t.text));
                ui.add_space(2.0);
                ui.label(RichText::new(desc).font(body(13.0)).color(t.text_2));
            });
            ui.add_space(40.0);
            ui.allocate_ui_with_layout(
                Vec2::new(width - left - 56.0, 0.0),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_min_width(width - left - 56.0);
                    add(ui);
                },
            );
        });
    }
    ui.add_space(30.0);
}

#[cfg(test)]
mod tests {
    use super::sync_with_saved;

    #[test]
    fn sync_with_saved_takes_an_outside_change_into_an_unedited_form() {
        let (mut form, mut base) = (1, 1);
        sync_with_saved(&mut form, &mut base, &2);
        assert_eq!((form, base), (2, 2));
    }

    #[test]
    fn sync_with_saved_keeps_pending_edits_when_saved_settings_change() {
        let (mut form, mut base) = (5, 1);
        sync_with_saved(&mut form, &mut base, &2);
        assert_eq!((form, base), (5, 2));
    }
}
