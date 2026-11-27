//! `restic-tray.exe --mcp`: Model Context Protocol (MCP) server over stdio.
//!
//! Speaks JSON-RPC 2.0 over stdin/stdout (one JSON message per line).
//! Connects to the running tray app via its Windows named pipe (`bridge.rs`)
//! when available; if the tray app is not currently running, falls back to
//! executing operations directly against [`SharedController`] so AI agents can
//! always query status, edit `config.json`, or run backups even in headless mode.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::agent::protocol::{
    BridgeRequest, BridgeResponse, ConnectionInfo, MAX_MESSAGE_BYTES, connection_file_path,
    token_matches,
};
use crate::config::{default_app_data_dir, default_config_path};
use crate::controller::SharedController;

const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Restic Tray (Cloudflare R2 + Restic Windows Backup Agent) MCP Server.\
1) Use `get_status` and `get_config` first to inspect `storageBackend` (`r2`), `machineName`, `maintenanceRole`, configured backup directories (`projectKey`, `writerState`), schedule (`maintenanceAt`), credential readiness, and restic binary status. Built-in basic excludes (dependency/build caches, temporary files, and `Build`/`Library`/`Temp` directly under each Unity project only) apply on top of `globalExcludes` and are listed in `get_status`.\
2) Use `update_storage_config` / `add_backup_target` / `remove_backup_target` / `update_schedule` / `update_retention_policy` to declaratively configure Cloudflare R2 (non-secret settings only), backup targets, and schedule in `config.json`. Pruning and prune-affecting MCP operations (`forget_prune`, `update_retention_policy`, enabling `autoPruneAfterBackup`, and setting or changing `maintenanceAt`) are disabled by default (`agent.allowPrune: false`).\
3) Use `init_repository` (first time on a new bucket), `check_repository`, and `run_backup` (1 target = 1 snapshot; pass `dryRun: true` first when verifying exclusions) to manage backups to the configured R2 repository, and `get_backup_history` or `list_snapshots` to verify results.\
4) For multi-PC shared projects (`projectKey` set, non-git), use `get_project_sync_status` to check which PC is latest, `release_project` to create a `handoff:released` snapshot and move this PC to `standby`, and `takeover_project` (disabled by default via `agent.allowHandoff: false`; always preview with `dryRun: true` first and pass `confirmProjectKey` on real takeover) to restore and claim ownership.\
5) Use `list_snapshot_files` and `diff_snapshots` to inspect snapshot contents before calling `restore_snapshot` (enabled by default via `agent.allowRestore: true`; restores into an explicit separate safe `targetDir` rather than overwriting the original folder).\
6) `delete_r2_bucket` is disabled by default (`agent.allowDeleteStorage: false`); when enabled, always preview with `dryRun: true` first and pass `confirmBucketName` matching the configured R2 bucket for real deletion.";

struct ToolDef {
    name: &'static str,
    op: &'static str,
    description: &'static str,
    input_schema: fn() -> Value,
}

fn no_args_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "get_status",
        op: "get_status",
        description: "常駐アプリの状態、PC 名（machineName）、メンテナンス担当ロール（maintenanceRole）、共有プロジェクト同期状態のキャッシュサマリ（projectSyncSummary）、保存先バックエンド（storageBackend: \"r2\" / storageConfigured）、DPAPI認証情報の準備状況、Resticバイナリ情報（resticBinary）、次回スケジュール時刻、実行中ジョブ進捗、適用中の基本除外パターンを返します。",
        input_schema: no_args_schema,
    },
    ToolDef {
        name: "get_config",
        op: "get_config",
        description: "現在の config.json（Cloudflare R2 設定 r2、バックアップ対象ディレクトリ一覧、除外パターン、スケジュール、保持ポリシー）を返します（アクセスキーやパスワードなどの秘密情報は含みません）。useBasicExcludes が true のときに加わる組み込みの基本除外パターンは、get_status の basicExcludes に出ます。",
        input_schema: no_args_schema,
    },
    ToolDef {
        name: "get_backup_history",
        op: "get_backup_history",
        description: "ローカルSQLiteに記録された直近のバックアップ実行履歴とMCP監査ログを返します。",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 25 }
                },
                "additionalProperties": false
            })
        },
    },
    ToolDef {
        name: "list_snapshots",
        op: "list_snapshots",
        description: "設定されたリポジトリ（Cloudflare R2）上の Restic スナップショット一覧を返します。",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": "ホスト名（PC 名）で絞り込む場合に指定" },
                    "projectKey": { "type": "string", "description": "共有プロジェクトの projectKey（project:<key> タグ）で絞り込む場合に指定" },
                    "tag": { "type": "string", "description": "タグで絞り込む場合に指定" },
                    "path": { "type": "string", "description": "対象パスで絞り込む場合に指定" }
                },
                "additionalProperties": false
            })
        },
    },
    ToolDef {
        name: "list_snapshot_files",
        op: "list_snapshot_files",
        description: "指定したスナップショット内のファイル・ディレクトリ一覧を返します。",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "snapshotId": { "type": "string", "minLength": 1, "description": "スナップショットIDまたは 'latest'" },
                    "prefix": { "type": "string", "description": "絞り込むサブパス" },
                    "maxEntries": { "type": "integer", "minimum": 1, "maximum": 2000, "default": 200 }
                },
                "required": ["snapshotId"],
                "additionalProperties": false
            })
        },
    },
    ToolDef {
        name: "get_size_breakdown",
        op: "get_size_breakdown",
        description: "スナップショットに入っているファイルの大きさを、バックアップ対象フォルダとその 2 階層下までのフォルダごとに合計し、大きい順に返します。何が容量を使っているか、除外すべきものが入っていないかを調べるために使います。ファイル数が多いと 1 分ほどかかります。",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "snapshotId": { "type": "string", "description": "スナップショット ID。省略時は最新 (latest)" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "返すフォルダ数 (既定 30)" }
                },
                "additionalProperties": false
            })
        },
    },
    ToolDef {
        name: "diff_snapshots",
        op: "diff_snapshots",
        description: "2つのスナップショット間の追加・削除・変更ファイル統計とパス一覧を返します。",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "snapshotA": { "type": "string", "minLength": 1 },
                    "snapshotB": { "type": "string", "minLength": 1 }
                },
                "required": ["snapshotA", "snapshotB"],
                "additionalProperties": false
            })
        },
    },
    ToolDef {
        name: "run_backup",
        op: "run_backup",
        description: "設定された保存先（Cloudflare R2）へ 1 ターゲット = 1 スナップショット形式でバックアップを実行します。dryRun: true でファイル転送なしのシミュレーションが可能です。",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "targetIds": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "特定のターゲットIDまたはパスのみ実行する場合に指定（省略時はこの PC で有効かつ active な全ターゲット）"
                    },
                    "projectKeys": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "特定の共有プロジェクト（projectKey）のみ実行する場合に指定"
                    },
                    "markHandoffReady": {
                        "type": "boolean",
                        "default": false,
                        "description": "true の場合、handoff:released タグを付与し、本番実行完了後に対象の writerState を standby に変更します（UnityLockfile / .git を検出した場合は拒否）"
                    },
                    "dryRun": { "type": "boolean", "default": false },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "スナップショットに付与する追加タグ"
                    }
                },
                "additionalProperties": false
            })
        },
    },
    ToolDef {
        name: "cancel_backup",
        op: "cancel_backup",
        description: "現在実行中のバックアップジョブを安全にキャンセルします。",
        input_schema: no_args_schema,
    },
    ToolDef {
        name: "init_repository",
        op: "init_repository",
        description: "設定された保存先（Cloudflare R2）上の Restic リポジトリを初期化 (`restic init`) します。",
        input_schema: no_args_schema,
    },
    ToolDef {
        name: "check_repository",
        op: "check_repository",
        description: "設定された保存先リポジトリ（Cloudflare R2）の整合性チェック (`restic check`) を実行します。`readDataPercent` を指定すると、保存データのその割合を実際に読み戻して復元できることまで確かめます（定期の復元確認と同じ処理。`get_status` の `lastVerify` に結果が残ります）。",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "readDataPercent": { "type": "integer", "minimum": 1, "maximum": 100, "description": "読み戻すデータの割合 (%)。省略時は構造の検査のみ" }
                },
                "additionalProperties": false
            })
        },
    },
    ToolDef {
        name: "update_storage_config",
        op: "update_storage_config",
        description: "保存先（Cloudflare R2、または Rclone serve s3 アダプタ）の非秘密設定（storageBackend、r2、rclone）を検証して config.json に保存します。秘密情報（アクセスキー・リポジトリパスワード）や実行バイナリパス（resticBinaryPath、rclone.binaryPath）は MCP からは変更できません。",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "storageBackend": {
                        "type": "string",
                        "enum": ["r2", "rclone"],
                        "description": "保存先バックエンド（\"r2\" = Cloudflare R2、\"rclone\" = rclone serve s3 ループバックアダプタ）"
                    },
                    "r2": {
                        "type": "object",
                        "properties": {
                            "accountId": { "type": "string" },
                            "bucket": { "type": "string" },
                            "prefix": { "type": "string" },
                            "endpointOverride": { "type": ["string", "null"] },
                            "region": { "type": "string" },
                            "maxConnections": { "type": "integer", "minimum": 1, "maximum": 32 }
                        },
                        "additionalProperties": false
                    },
                    "rclone": {
                        "type": "object",
                        "properties": {
                            "remote": { "type": "string", "description": "rclone リモート（例: \"mycrypt:\"、\"gdrive:backups\"）" },
                            "bucket": { "type": "string", "description": "S3 バケット名（リモート直下のフォルダ）" },
                            "port": { "type": ["integer", "null"], "minimum": 1, "maximum": 65535 },
                            "configPath": { "type": ["string", "null"] }
                        },
                        "additionalProperties": false
                    }
                },
                "additionalProperties": false
            })
        },
    },
    ToolDef {
        name: "add_backup_target",
        op: "add_backup_target",
        description: "バックアップ対象ディレクトリを検証・正規化して config.json に追加または更新します。",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "minLength": 1, "description": "バックアップする実在ディレクトリのパス" },
                    "label": { "type": "string", "description": "表示ラベル（省略時はフォルダ名）" },
                    "excludePatterns": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "このターゲット固有の除外パターン (例: ['*.log', 'cache/'])"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" }
                    },
                    "enabled": { "type": "boolean", "default": true },
                    "projectKey": {
                        "type": "string",
                        "pattern": "^[a-z0-9][a-z0-9._-]{0,63}$",
                        "description": "複数 PC 間で共有する論理プロジェクト ID（確定後は変更不可、.git を含むリポジトリには設定不可）"
                    },
                    "writerState": {
                        "type": "string",
                        "enum": ["active", "standby"],
                        "description": "共有プロジェクトに対するこの PC の作業権（active: 定期バックアップ対象、standby: 待機中）"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            })
        },
    },
    ToolDef {
        name: "remove_backup_target",
        op: "remove_backup_target",
        description: "指定した ID またはパスのバックアップ対象ディレクトリを config.json から削除します。",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "idOrPath": { "type": "string", "minLength": 1 }
                },
                "required": ["idOrPath"],
                "additionalProperties": false
            })
        },
    },
    ToolDef {
        name: "update_schedule",
        op: "update_schedule",
        description: "定期バックアップの有効/無効、実行間隔（分）または毎日の実行時刻（dailyAt、1日複数回指定可、設定すると間隔より優先）、起動時実行、バックアップ後の自動Prune設定を更新します。",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "enabled": { "type": "boolean" },
                    "intervalMinutes": { "type": "integer", "minimum": 1, "maximum": 10080 },
                    "dailyAt": {
                        "oneOf": [
                            {
                                "type": "array",
                                "items": {
                                    "type": "string",
                                    "pattern": "^([01][0-9]|2[0-3]):[0-5][0-9]$"
                                }
                            },
                            {
                                "type": "string",
                                "pattern": "^([01][0-9]|2[0-3]):[0-5][0-9]$"
                            },
                            { "type": "null" }
                        ],
                        "description": "毎日の実行時刻（ローカル時刻、24時間表記の HH:MM の配列または単一文字列、例: [\"05:00\", \"17:00\"]）。空配列または null で間隔指定に戻します"
                    },
                    "runOnStartup": { "type": "boolean" },
                    "autoPruneAfterBackup": { "type": "boolean" },
                    "verifyEveryDays": { "type": "integer", "minimum": 0, "maximum": 365, "description": "定期バックアップのあとに復元確認 (restic check --read-data-subset) を行う間隔 (日)。0 で無効。既定は 7" },
                    "verifyReadPercent": { "type": "integer", "minimum": 1, "maximum": 100, "description": "復元確認で読み戻す保存データの割合 (%)。既定は 5" },
                    "notifyOverdueHours": { "type": "integer", "minimum": 0, "maximum": 8760, "description": "この時間バックアップが成功しなければ Windows の通知を出す。0 で無効。既定は 48" },
                    "maintenanceAt": {
                        "oneOf": [
                            {
                                "type": "string",
                                "pattern": "^([01][0-9]|2[0-3]):[0-5][0-9]$"
                            },
                            { "type": "null" }
                        ],
                        "description": "maintenanceRole: 'primary' の PC が forget --prune と復元確認を実行する 1 日 1 回の独立メンテナンス時刻（ローカル 24 時間表記 HH:MM）。null で無効（agent.allowPrune: false のときは MCP からの新規設定・変更を拒否）"
                    }
                },
                "additionalProperties": false
            })
        },
    },
    ToolDef {
        name: "update_retention_policy",
        op: "update_retention_policy",
        description: "スナップショット保持ポリシー (keepLast / keepDaily / keepWeekly / keepMonthly) を更新します。既定で無効（agent.allowPrune: false）であり、MCP からの変更には config.json または設定画面での有効化が必要です。",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "keepLast": { "type": "integer", "minimum": 0, "maximum": 1000 },
                    "keepDaily": { "type": "integer", "minimum": 0, "maximum": 365 },
                    "keepWeekly": { "type": "integer", "minimum": 0, "maximum": 520 },
                    "keepMonthly": { "type": "integer", "minimum": 0, "maximum": 240 }
                },
                "additionalProperties": false
            })
        },
    },
    ToolDef {
        name: "restore_snapshot",
        op: "restore_snapshot",
        description: "指定スナップショットを元の場所とは別の安全な出力先ディレクトリ (`targetDir`) に復元します（既定で有効: agent.allowRestore: true。ドライブ直下やシステム領域等への復元は拒否されます）。",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "snapshotId": { "type": "string", "minLength": 1 },
                    "targetDir": { "type": "string", "minLength": 1, "description": "復元先の専用ディレクトリパス" },
                    "includePaths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "特定のサブパス・ファイルのみ復元する場合に指定"
                    }
                },
                "required": ["snapshotId", "targetDir"],
                "additionalProperties": false
            })
        },
    },
    ToolDef {
        name: "forget_prune",
        op: "forget_prune",
        description: "保持ポリシーに従い古いスナップショットを削除・Pruneします。既定で無効（agent.allowPrune: false）です。既定は dryRun: true です。本実行する場合は confirmPrune: true かつ dryRun: false を指定してください。",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "dryRun": { "type": "boolean", "default": true },
                    "confirmPrune": { "type": "boolean", "default": false }
                },
                "additionalProperties": false
            })
        },
    },
    ToolDef {
        name: "delete_r2_bucket",
        op: "delete_r2_bucket",
        description: "設定済み Cloudflare R2 のプレフィックス配下（scope: 'prefixOnly'）またはバケット全体（scope: 'entireBucket'）を削除します。既定で無効（agent.allowDeleteStorage: false）です。既定は dryRun: true で対象件数と容量のみ返します。実削除する場合は dryRun: false かつ confirmBucketName に r2.bucket と完全一致するバケット名を指定してください。Object Read & Write トークンで entireBucket を実行した場合は中身をすべて空にした後、バケット本体削除は 403 AccessDenied として案内を返します。",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "scope": {
                        "type": "string",
                        "enum": ["prefixOnly", "entireBucket"],
                        "description": "'prefixOnly': r2.prefix 配下のみ削除（プレフィックス必須）、'entireBucket': バケット内の全オブジェクトと未完了マルチパートを削除したうえで DeleteBucket を試行"
                    },
                    "dryRun": {
                        "type": "boolean",
                        "default": true,
                        "description": "true（既定）なら削除せず対象オブジェクト数と合計サイズのみ集計"
                    },
                    "confirmBucketName": {
                        "type": "string",
                        "description": "dryRun: false で実削除を行う場合に必須。config.json の r2.bucket と完全一致するバケット名"
                    }
                },
                "required": ["scope"],
                "additionalProperties": false
            })
        },
    },
    ToolDef {
        name: "get_project_sync_status",
        op: "get_project_sync_status",
        description: "複数 PC 間の共有プロジェクトごとの最新ホスト・時刻・スナップショット ID・handoffReady 有無・自 PC 状態（local_latest / remote_newer / not_local / diverged / local_only）を返します。refresh: true でリポジトリから最新スナップショットを取得してキャッシュを更新します。",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "projectKey": {
                        "type": "string",
                        "description": "特定の共有プロジェクトのみ確認する場合に指定"
                    },
                    "refresh": {
                        "type": "boolean",
                        "default": false,
                        "description": "true の場合 restic snapshots を実行して最新状態へキャッシュを更新"
                    },
                    "checkLocalChanges": {
                        "type": "boolean",
                        "default": false,
                        "description": "true の場合、他 PC が最新の共有プロジェクトについてローカル未バックアップ変更の有無も検査"
                    }
                },
                "additionalProperties": false
            })
        },
    },
    ToolDef {
        name: "release_project",
        op: "release_project",
        description: "指定した共有プロジェクトの作業を終了して他 PC へ引き渡します。UnityLockfile と .git の有無を確認し、project:<key> と handoff:released タグ付きでバックアップしたうえで自 PC の writerState を standby に変更します（ローカルファイルは削除しません）。",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "projectKey": {
                        "type": "string",
                        "minLength": 1,
                        "description": "引き渡す共有プロジェクトの projectKey"
                    },
                    "dryRun": {
                        "type": "boolean",
                        "default": false,
                        "description": "true の場合はアップロードや writerState 変更を行わずにシミュレーション"
                    }
                },
                "required": ["projectKey"],
                "additionalProperties": false
            })
        },
    },
    ToolDef {
        name: "takeover_project",
        op: "takeover_project",
        description: "他 PC の最新スナップショットをパス読み替え（--delete + 除外保護）で復元し、所有権スナップショット（handoff:claimed）を作成してこの PC で作業権を引き継ぎます。既定で無効（agent.allowHandoff: false）です。既定の dryRun: true では復元元スナップショット・相手側作業中の可能性・ローカル未バックアップ変更の有無のみ返します。実実行（dryRun: false）には confirmProjectKey の完全一致が必須です。",
        input_schema: || {
            json!({
                "type": "object",
                "properties": {
                    "projectKey": {
                        "type": "string",
                        "minLength": 1,
                        "description": "引き継ぐ共有プロジェクトの projectKey"
                    },
                    "localPath": {
                        "type": "string",
                        "description": "復元先フォルダ（未配置プロジェクトで defaultProjectRoot 未設定時は必須、省略時は既存ターゲットパスまたは <defaultProjectRoot>\\<フォルダ名>）"
                    },
                    "dryRun": {
                        "type": "boolean",
                        "default": true,
                        "description": "true（既定）ならローカル変更や復元を行わず、確認情報のみ返却"
                    },
                    "saveLocalBackupFirst": {
                        "type": "boolean",
                        "default": true,
                        "description": "受け側に未バックアップ変更がある場合、復元前に handoff:safety-backup タグ付き退避スナップショットを保存するか（既定 true）"
                    },
                    "confirmDiscardLocalChanges": {
                        "type": "boolean",
                        "default": false,
                        "description": "saveLocalBackupFirst: false で受け側の未バックアップ変更を退避せず破棄する場合に true を明示指定（既定 false）"
                    },
                    "forceWhileRemoteActive": {
                        "type": "boolean",
                        "default": false,
                        "description": "相手 PC の最新スナップショットに handoff:released が付いていない場合でも強制的に引き継ぐか（既定 false）"
                    },
                    "deleteExtra": {
                        "type": "boolean",
                        "default": true,
                        "description": "復元時に相手 PC で削除されたファイルを --delete で削除するか（Unity 除外フォルダは --exclude で保護、既定 true）"
                    },
                    "confirmProjectKey": {
                        "type": "string",
                        "description": "dryRun: false で実実行する場合に必須。projectKey と完全一致する文字列"
                    }
                },
                "required": ["projectKey"],
                "additionalProperties": false
            })
        },
    },
];

pub fn run_stdio(config_override: Option<PathBuf>) -> i32 {
    let base_dir = default_app_data_dir();
    let cfg_path = config_override.unwrap_or_else(default_config_path);
    let mut fallback_controller: Option<Arc<SharedController>> = None;

    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let mut reader = BufReader::new(stdin.lock());
    let mut next_req_id = 1_u64;

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => return 1,
        }
        // Windows PowerShell 5.1 puts a byte order mark in front of text piped to a program.
        let trimmed = line.trim().trim_start_matches('\u{feff}');
        if trimmed.is_empty() {
            continue;
        }

        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(err) => {
                let _ = write_json_rpc_error(
                    &mut stdout,
                    Value::Null,
                    -32700,
                    &format!("Parse error: {err}"),
                );
                continue;
            }
        };

        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            continue;
        };
        let id = msg.get("id").cloned();

        // Notifications have no `id`
        let Some(id) = id else {
            continue;
        };

        match method {
            "initialize" => {
                let requested = msg
                    .get("params")
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(Value::as_str)
                    .unwrap_or(PROTOCOL_VERSIONS[0]);
                let negotiated = if PROTOCOL_VERSIONS.contains(&requested) {
                    requested
                } else {
                    PROTOCOL_VERSIONS[0]
                };
                let result = json!({
                    "protocolVersion": negotiated,
                    "capabilities": {
                        "tools": { "listChanged": false }
                    },
                    "serverInfo": {
                        "name": "restic-tray",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "instructions": INSTRUCTIONS
                });
                let _ = write_json_rpc_result(&mut stdout, id, result);
            }
            "ping" => {
                let _ = write_json_rpc_result(&mut stdout, id, json!({}));
            }
            "tools/list" => {
                let tools_json: Vec<Value> = TOOLS
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "description": t.description,
                            "inputSchema": (t.input_schema)()
                        })
                    })
                    .collect();
                let _ = write_json_rpc_result(&mut stdout, id, json!({ "tools": tools_json }));
            }
            "tools/call" => {
                let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
                let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));

                let Some(tool) = TOOLS.iter().find(|t| t.name == tool_name) else {
                    let _ = write_json_rpc_error(
                        &mut stdout,
                        id,
                        -32602,
                        &format!("Unknown tool: {tool_name}"),
                    );
                    continue;
                };

                let req_id = next_req_id;
                next_req_id += 1;

                let dispatch_res = dispatch_tool_call(
                    &base_dir,
                    &cfg_path,
                    &mut fallback_controller,
                    req_id,
                    tool.op,
                    &args,
                );

                match dispatch_res {
                    Ok(val) => {
                        let text = serde_json::to_string_pretty(&val).unwrap_or_default();
                        let _ = write_json_rpc_result(
                            &mut stdout,
                            id,
                            json!({
                                "content": [{ "type": "text", "text": text }],
                                "isError": false
                            }),
                        );
                    }
                    Err(err_msg) => {
                        let _ = write_json_rpc_result(
                            &mut stdout,
                            id,
                            json!({
                                "content": [{ "type": "text", "text": err_msg }],
                                "isError": true
                            }),
                        );
                    }
                }
            }
            _ => {
                let _ = write_json_rpc_error(
                    &mut stdout,
                    id,
                    -32601,
                    &format!("Method not found: {method}"),
                );
            }
        }
    }

    0
}

fn dispatch_tool_call(
    base_dir: &Path,
    cfg_path: &Path,
    fallback_controller: &mut Option<Arc<SharedController>>,
    req_id: u64,
    op: &str,
    args: &Value,
) -> Result<Value, String> {
    // 1. Try named pipe to the resident tray app first so GUI state stays in sync
    if let Some(res) = try_call_named_pipe(base_dir, req_id, op, args) {
        return res;
    }

    // 2. Fallback: direct execution via SharedController if tray app is not running
    if let Some(ctrl) = fallback_controller.as_ref() {
        ctrl.reload_config_from_disk().map_err(|e| e.to_string())?;
        if !ctrl.get_config().agent.enabled {
            return Err("MCP agent integration is disabled in config.json".to_owned());
        }
        return ctrl
            .handle_operation("mcp", op, args)
            .map_err(|e| e.to_string());
    }

    let ctrl = SharedController::new(base_dir.to_path_buf(), cfg_path.to_path_buf())
        .map_err(|e| e.to_string())?;
    let enabled = ctrl.get_config().agent.enabled;
    let res = if enabled {
        ctrl.handle_operation("mcp", op, args)
            .map_err(|e| e.to_string())
    } else {
        Err("MCP agent integration is disabled in config.json".to_owned())
    };
    *fallback_controller = Some(ctrl);
    res
}

#[cfg(windows)]
fn try_call_named_pipe(
    base_dir: &Path,
    req_id: u64,
    op: &str,
    args: &Value,
) -> Option<Result<Value, String>> {
    use crate::agent::protocol::{ERROR_PIPE_BUSY, open_pipe};

    let conn_path = connection_file_path(Some(base_dir));
    let raw = std::fs::read_to_string(&conn_path).ok()?;
    let info: ConnectionInfo = serde_json::from_str(&raw).ok()?;
    if !info.is_well_formed() {
        return None;
    }

    let mut pipe = loop {
        match open_pipe(&info.pipe_name) {
            Ok(f) => break f,
            Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    };

    let req = BridgeRequest {
        token: info.token.clone(),
        id: req_id,
        op: op.to_owned(),
        args: args.clone(),
    };
    let Ok(mut line) = serde_json::to_vec(&req) else {
        return Some(Err("failed to serialize bridge request".to_owned()));
    };
    line.push(b'\n');
    if let Err(err) = pipe.write_all(&line).and_then(|()| pipe.flush()) {
        return Some(Err(format!("failed to write bridge request: {err}")));
    }

    let mut buf = Vec::new();
    let mut reader = BufReader::new(pipe).take((MAX_MESSAGE_BYTES + 1) as u64);
    if let Err(err) = reader.read_until(b'\n', &mut buf) {
        return Some(Err(format!("failed to read bridge response: {err}")));
    }
    if buf.is_empty() || buf.len() > MAX_MESSAGE_BYTES {
        return Some(Err(
            "bridge response invalid or exceeded maximum size".to_owned()
        ));
    }

    let resp: BridgeResponse = match serde_json::from_slice(&buf) {
        Ok(r) => r,
        Err(err) => return Some(Err(format!("invalid bridge response JSON: {err}"))),
    };
    let Some(server_tok) = resp.server.as_deref() else {
        return Some(Err("bridge response missing server token".to_owned()));
    };
    if !token_matches(&info.server_token, server_tok) {
        return Some(Err("bridge server token verification failed".to_owned()));
    }

    if resp.ok {
        Some(Ok(resp.result.unwrap_or_else(|| json!({}))))
    } else {
        Some(Err(resp
            .error
            .unwrap_or_else(|| "unknown bridge error".to_owned())))
    }
}

#[cfg(not(windows))]
fn try_call_named_pipe(
    _base_dir: &Path,
    _req_id: u64,
    _op: &str,
    _args: &Value,
) -> Option<Result<Value, String>> {
    None
}

fn write_json_rpc_result(out: &mut impl Write, id: Value, result: Value) -> io::Result<()> {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    });
    let mut bytes = serde_json::to_vec(&payload)?;
    bytes.push(b'\n');
    out.write_all(&bytes)?;
    out.flush()
}

fn write_json_rpc_error(
    out: &mut impl Write,
    id: Value,
    code: i32,
    message: &str,
) -> io::Result<()> {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    });
    let mut bytes = serde_json::to_vec(&payload)?;
    bytes.push(b'\n');
    out.write_all(&bytes)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::protocol::OPERATIONS;

    #[test]
    fn every_mcp_tool_is_allowed_by_named_pipe_protocol() {
        for tool in TOOLS {
            assert!(
                OPERATIONS.contains(&tool.op),
                "MCP tool {} (op={}) is missing from agent::protocol::OPERATIONS",
                tool.name,
                tool.op
            );
        }
        assert_eq!(TOOLS.len(), 22);
        assert_eq!(TOOLS.len(), OPERATIONS.len());
    }

    #[test]
    fn headless_mcp_propagates_google_drive_removal_and_config_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.json");
        std::fs::write(
            &cfg_path,
            r#"{"storageBackend":"googleDrive","googleDrive":{"folder":"restic-tray"}}"#,
        )
        .unwrap();

        let expected_err = crate::config::AppConfig::load_or_default(&cfg_path).unwrap_err();
        let (_, expected_gui_text) = crate::startup_error_dialog_content(&expected_err);

        let mut fallback = None;

        let err = dispatch_tool_call(
            dir.path(),
            &cfg_path,
            &mut fallback,
            1,
            "get_status",
            &json!({}),
        )
        .unwrap_err();
        assert_eq!(err, expected_gui_text);
        assert!(err.contains("Cloudflare R2"), "unexpected error: {err}");
        assert!(err.contains("storageBackend"), "unexpected error: {err}");
        assert!(
            !err.contains("v0.7.0"),
            "public error must not advise downgrading to unpublished v0.7.0: {err}"
        );

        // Once the user removes `storageBackend`, headless MCP recovers without restart.
        std::fs::write(
            &cfg_path,
            r#"{"r2":{"accountId":"acc","bucket":"bkt"},"googleDrive":{"folder":"restic-tray"}}"#,
        )
        .unwrap();
        let ok = dispatch_tool_call(
            dir.path(),
            &cfg_path,
            &mut fallback,
            2,
            "get_status",
            &json!({}),
        )
        .expect("should recover after config fix");
        assert_eq!(ok["storageBackend"], "r2");

        // Headless MCP rejects resticBinaryPath even when pointing to an existing executable.
        let existing_file = std::env::current_exe().expect("current_exe");
        let bin_err = dispatch_tool_call(
            dir.path(),
            &cfg_path,
            &mut fallback,
            3,
            "update_storage_config",
            &json!({ "resticBinaryPath": existing_file.to_string_lossy() }),
        )
        .unwrap_err();
        assert!(
            bin_err.contains("resticBinaryPath"),
            "unexpected error: {bin_err}"
        );

        // Subsequent config break on disk also surfaces the exact error rather than stale config.
        std::fs::write(&cfg_path, r#"{"storageBackend":"googleDrive"}"#).unwrap();
        let err_after_edit = dispatch_tool_call(
            dir.path(),
            &cfg_path,
            &mut fallback,
            4,
            "get_status",
            &json!({}),
        )
        .unwrap_err();
        assert_eq!(err_after_edit, expected_gui_text);
    }
}
