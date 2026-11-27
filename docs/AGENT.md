# AI エージェント連携（MCP）ガイド (`docs/AGENT.md`)

`restic-tray.exe --mcp` は [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) の stdio サーバーとして動作します。

## 通信の仕組み

```text
MCP クライアント ──stdio (JSON-RPC 2.0)──▶ restic-tray.exe --mcp ──名前付きパイプ (相互トークン認証)──▶ 常駐トレイアプリ
```

- トレイ常駐アプリが起動中の場合、`--mcp` プロセスは `%LOCALAPPDATA%\io.github.nayuengin.restic-tray\agent-bridge.json` に書き出された一時パイプ名と 256-bit トークンを用いて常駐アプリへ要求を中継します。操作結果は即座に GUI 画面と SQLite 監査ログ (`audit_logs`) に反映されます。
- トレイ常駐アプリが起動していない場合でも、`--mcp` プロセスが自動的にローカルコントローラへフォールバックし、設定編集やバックアップ実行を完遂できます。

## 提供ツール一覧（全 22 ツール）

| ツール名 | 引数 | 説明 |
| :--- | :--- | :--- |
| `get_status` | なし | 常駐状態、PC 名 (`machineName`)・保守担当 (`maintenanceRole`: `"primary"` \| `"none"` / `maintenanceAt`)・アクティブ対象件数 (`activeTargetCount`)・複数 PC 同期サマリー (`projectSyncSummary` / `consecutiveDeferredCount` / `observedRemoteHosts`)、保存先バックエンド (`storageBackend`: `"r2"` | `"rclone"` / `storageConfigured`)、R2・DPAPI 準備状況、Restic バイナリ情報 (`resticBinary`)、次回実行予定、実行中ジョブの進捗、適用中の基本除外パターン (`basicExcludes` / `basicExcludeIfPresent` / Unity プロジェクトの直下だけに適用する `basicUnityProjectExcludes`) を取得 |
| `get_config` | なし | 現在の `config.json`（`machineName`、`defaultProjectRoot`、`maintenanceRole`、Cloudflare R2 設定 `r2`、`resticBinaryPath`、対象ディレクトリ、`projectKey` / `writerState`、除外設定、スケジュール・`maintenanceAt`、保持ポリシー、`agent` ポリシー等）を取得 |
| `get_backup_history` | `limit?: number` | SQLite に保存されたバックアップ履歴 (`runs`、排他ロック延期時は `status: "deferred"`) と監査ログ (`auditLogs`) を取得。各 run の `topAdded` は、リポジトリへの保存量が多いフォルダの上位 5 件。`bytesDelta` / `filesDelta` は同じ対象フォルダの前回の本番実行と比べた対象サイズとファイル数の差（除外の追加や削除で負になる）、`excludesChanged` は除外設定が前回から変わったこと |
| `list_snapshots` | `tag?: string`, `path?: string`, `projectKey?: string`, `host?: string` | 設定されたリポジトリ（Cloudflare R2）上のスナップショット一覧を取得。`projectKey`（`project:<key>` タグ絞り込み）および `host`（小文字ホスト名絞り込み）に対応 |
| `list_snapshot_files` | `snapshotId: string`, `prefix?: string`, `maxEntries?: number` | 指定スナップショット内のファイル一覧を取得 |
| `diff_snapshots` | `snapshotA: string`, `snapshotB: string` | 2 つのスナップショット間の差分統計と変更パス一覧を取得（異なる PC・絶対パスのスナップショット同士ではツリー全体が削除＋追加として集計される） |
| `get_size_breakdown` | `snapshotId?: string`, `limit?: number` | スナップショット内のファイルの大きさを、バックアップ対象フォルダとその 2 階層下までのフォルダごとに合計して大きい順に返す。`snapshotId` 省略時は最新、`limit` は既定 30。応答は `totalBytes` / `folderCount` / `folders[{path, bytes}]` |
| `run_backup` | `targetIds?: string[]`, `projectKeys?: string[]`, `dryRun?: boolean`, `tags?: string[]`, `markHandoffReady?: boolean` | 設定された保存先（Cloudflare R2）へ対象ごと（1 対象 = 1 スナップショット）にバックアップを実行（`dryRun: true` で転送なし検証が可能）。`projectKeys` で共有プロジェクトを指定でき、`markHandoffReady: true` で `handoff:released` タグ付与と `writerState: "standby"` への切り替えも行う |
| `cancel_backup` | なし | 実行中のバックアップ／引き継ぎ復元ジョブをキャンセル（`--retry-lock` 待機中も即時中断） |
| `init_repository` | なし | 設定された保存先リポジトリを `restic init` で初期化 |
| `check_repository` | `readDataPercent?: number` | `restic check` でリポジトリ整合性を検査。`readDataPercent` (1〜100) を指定すると、保存データのその割合を読み戻して復元できることまで確かめる（結果は `get_status` の `lastVerify`） |
| `update_storage_config` | `storageBackend?: "r2" \| "rclone"`, `r2?: object`, `rclone?: object` | 保存先（Cloudflare R2 または Rclone `serve s3` アダプタ）の非秘密設定（`r2`、`rclone` の `remote` / `bucket` / `port` / `configPath` 等）を検証して `config.json` に保存（秘密情報や実行バイナリパス `resticBinaryPath` / `rclone.binaryPath` は MCP からは変更不可）。`storageBackend: "googleDrive"` は廃止を伝えるエラーを返す |
| `add_backup_target` | `path: string`, `label?: string`, `excludePatterns?: string[]`, `tags?: string[]`, `enabled?: boolean`, `projectKey?: string \| null`, `writerState?: "active" \| "standby"` | バックアップ対象フォルダを検証・正規化して `config.json` に追加または更新。`projectKey` は `^[a-z0-9][a-z0-9._-]{0,63}$` の範囲で一意かつ設定後の変更は不可。`.git` を含むリポジトリへの `projectKey` 付与は拒否する |
| `remove_backup_target` | `idOrPath: string` | 指定 ID またはパスのバックアップ対象を `config.json` から削除 |
| `update_schedule` | `enabled?: boolean`, `intervalMinutes?: number`, `dailyAt?: string[] \| string \| null`, `maintenanceAt?: string \| null`, `runOnStartup?: boolean`, `autoPruneAfterBackup?: boolean`, `verifyEveryDays?: number`, `verifyReadPercent?: number`, `notifyOverdueHours?: number` | 定期実行スケジュール設定を更新。`verifyEveryDays`（既定 7、0 で無効）は復元確認の間隔、`verifyReadPercent`（既定 5）はそのとき読み戻す割合、`notifyOverdueHours`（既定 48、0 で無効）は成功が途絶えたときに通知するまでの時間。`dailyAt`（`["05:00", "17:00"]` や `"05:00"` のようなローカル時刻）を指定すると毎日その時刻に実行し、空配列または `null` で間隔指定に戻す。`maintenanceAt`（`"05:00"` 等、`null` で解除）は `maintenanceRole: "primary"` の PC が自動 `forget --prune` と定期 `verify` を行う独立枠。`agent.allowPrune: false`（既定）のときは、MCP からの `autoPruneAfterBackup: true`、`maintenanceAt` の新規設定・変更、および `autoPruneAfterBackup: true` 状態での `maintenanceAt: null` 解除を拒否する |
| `update_retention_policy` | `keepLast?: number`, `keepDaily?: number`, `keepWeekly?: number`, `keepMonthly?: number` | スナップショット保持世代ポリシーを更新。既定で無効（`agent.allowPrune: false` のため MCP からの変更には `agent.allowPrune: true` のオプトインが必要）。未知のキー・型違い・範囲外（`keepLast: 0..=1000`, `keepDaily: 0..=365`, `keepWeekly: 0..=520`, `keepMonthly: 0..=240`）はエラーとして拒否する |
| `restore_snapshot` | `snapshotId: string`, `targetDir: string`, `includePaths?: string[]` | 指定スナップショットを元の場所とは別の安全な出力先フォルダ (`targetDir`) に復元（既定で有効: `agent.allowRestore: true`。ドライブ直下やシステム領域等への復元は `src/fs_safety.rs` で拒否） |
| `forget_prune` | `dryRun?: boolean`, `confirmPrune?: boolean` | 古いスナップショットを保持ポリシーに従い削除・Prune。既定で無効（`agent.allowPrune: false` のオプトインが必要）。既定は `dryRun: true` で、本実行時は `dryRun: false` と `confirmPrune: true` の両方の明示が必要。`agent.allowPrune: false` のときは `forget_prune`・`update_retention_policy`・MCP からの `autoPruneAfterBackup` 有効化および `maintenanceAt` の新規設定／変更・MCP 起点のバックアップ後自動 Prune をすべて抑止。共有プロジェクトは `project:<key>` ごとに `--group-by ''` で全 PC を合算管理しつつ `handoff:safety-backup` を保護し、旧形式・ローカル専用スナップショットは `--group-by host,paths` で保護管理する |
| `delete_r2_bucket` | `scope: "prefixOnly" \| "entireBucket"`, `dryRun?: boolean`, `confirmBucketName?: string` | 設定済み Cloudflare R2 のプレフィックス配下またはバケット全体を削除。既定無効（`agent.allowDeleteStorage: true` のオプトインが必要）。既定は `dryRun: true` で対象件数・容量のみ返却し、実削除時は `dryRun: false` かつ `confirmBucketName` に `r2.bucket` の完全一致が必要 |
| `get_project_sync_status` | `projectKey?: string`, `refresh?: boolean`, `checkLocalChanges?: boolean` | 複数 PC 間の共有プロジェクトごとの最新ホスト・時刻・スナップショット ID・`handoffReady` 有無・自 PC 状態（`local_latest` / `remote_newer` / `not_local` / `diverged` / `local_only` / `standby` / `unchecked`）を返す。`refresh: true` で `restic snapshots` を実行してキャッシュを更新、`checkLocalChanges: true` で他 PC 最新プロジェクトのローカル未バックアップ変更も検査 |
| `release_project` | `projectKey: string`, `dryRun?: boolean` | 指定した共有プロジェクトの作業を終了して他 PC へ引き渡す。`.git` と `Temp\UnityLockfile` を検査し、`project:<key>` + `handoff:released` タグ付きで単体バックアップしたうえで自 PC の `writerState` を `standby` に変更する（ローカルファイルは削除しない） |
| `takeover_project` | `projectKey: string`, `localPath?: string`, `dryRun?: boolean`, `saveLocalBackupFirst?: boolean`, `confirmDiscardLocalChanges?: boolean`, `forceWhileRemoteActive?: boolean`, `deleteExtra?: boolean`, `confirmProjectKey?: string` | 他 PC の最新スナップショットをパス読み替え（`restore <id>:<subpath> --target <localPath>`、既定 `--delete` + Unity/マーカー除外保護）で復元し、`handoff:claimed` スナップショットを作成して自 PC を `active` にする。既定で無効（`agent.allowHandoff: false` のオプトインが必要）。既定は `dryRun: true` で確認情報のみ返却し、実実行時は `dryRun: false` かつ `confirmProjectKey` に `projectKey` の完全一致（前後空白不可）が必要。未バックアップ変更を退避せず破棄する場合は `saveLocalBackupFirst: false` と `confirmDiscardLocalChanges: true` の両方が必要 |
