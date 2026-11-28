# アーキテクチャと設計判断 (`docs/ARCHITECTURE.md`)

## 1. モジュール責務マップ

| ファイル | 役割 |
| :--- | :--- |
| [`src/main.rs`](../src/main.rs) | CLI 引数解析 (`--mcp`, `--minimized`, `--config`, `--tab`, `--light`, `--screenshot`, `--version`) とモード分岐 |
| [`src/lib.rs`](../src/lib.rs) | バックグラウンドスケジューラ・名前付きパイプ MCP ブリッジ・`eframe` ネイティブウィンドウの初期化 |
| [`src/config.rs`](../src/config.rs) | `config.json` のスキーマ定義、Cloudflare R2 リポジトリ URL 構築、複数 PC 設定 (`machineName`, `defaultProjectRoot`, `maintenanceRole`, `projectKey`, `writerState`, `schedule.maintenanceAt`, `agent.allowHandoff`)、廃止キー (`googleDrive`, `rcloneBinaryPath`) の読み込み互換、Pkl 設定 (`config.pkl`) 評価、および `dunce::canonicalize` 付きターゲット追加・削除・アトミック書き込み |
| [`src/secrets.rs`](../src/secrets.rs) | Windows DPAPI (`CryptProtectData` / `CryptUnprotectData`) + 二次エントロピー + `Zeroize` による `credentials.dat` 暗号化ストア（R2 キー・Restic パスワード）。macOS / Linux は OS キーチェーン（Keychain / Secret Service、`keyring` クレート）に保存 |
| [`src/fs_safety.rs`](../src/fs_safety.rs) | バックアップ対象ディレクトリの正規化検証・リストア先ディレクトリのシステムパス保護・未解決 Reparse Point 拒否・`.git` / `Temp\UnityLockfile` 検知・Windows パス ↔ Restic サブパス変換 |
| [`src/restic/sidecar.rs`](../src/restic/sidecar.rs) | `config.json` 指定パス → バイナリ隣 / `binaries/` → システム `PATH` の順での `restic.exe` 解決 |
| [`src/restic/runner.rs`](../src/restic/runner.rs) | シェル非経由 (`std::process::Command` + `CREATE_NO_WINDOW`) での `restic` 実行、環境変数経由での R2 認証注入、1 対象 = 1 スナップショットのバックアップ、`--host` / `--retry-lock`（終了コード 11 識別）/ `restore <id>:<subpath> --delete --exclude` / 複数 PC 対応 `forget --prune` / 安全な `unlock`、`--json` ストリーム解析 |
| [`src/rclone.rs`](../src/rclone.rs) | `rclone serve s3` ループバックアダプタ（`127.0.0.1` のみ・匿名・設定変更で再起動）|
| [`src/restic/ignore.rs`](../src/restic/ignore.rs) | `.gitignore`・カスタム無視ファイルの Restic 除外パターン変換 |
| [`src/r2_s3.rs`](../src/r2_s3.rs) | 純 Rust 製 Cloudflare R2 S3 互換 API クライアント（AWS SigV4 / Windows Schannel TLS）。プレフィックス配下のみの削除（`prefixOnly`）およびバケット全体の空化＋`DeleteBucket`（`entireBucket`）、未完了マルチパートアップロードの中止、Dry-Run 集計、進捗・中断制御、403 権限不足時の案内 |
| [`src/restic/excludes.rs`](../src/restic/excludes.rs) | バックアップから外すパスの決定。`--exclude` / `--exclude-if-present` 引数の組み立て、Unity プロジェクト直下のフォルダの検出、対象フォルダそのものに一致するパターンの拒否、引き継ぎ `restore --delete` 用の相対 `--exclude` 構築（Unity・マーカーフォルダ保護）、除外設定のハッシュ |
| [`src/db.rs`](../src/db.rs) | `rusqlite` (bundled, WAL モード) による `backup_runs` 履歴（`'deferred'` 延期記録を含む）、`audit_logs` 監査ログ、複数 PC 同期状態 (`project_sync_state`)、リポジトリ確認メタ (`sync_meta`) の永続化 |
| [`src/sync.rs`](../src/sync.rs) | 複数 PC 間の共有プロジェクト 5 状態分類（`local_latest` / `remote_newer` / `not_local` / `diverged` / `local_only`）、有効最新スナップショット選定（`handoff:safety-backup` 除外）・旧複合スナップショット候補抽出、`project_sync_state` 取り込み、引き継ぎ復元先提案 |
| [`src/controller.rs`](../src/controller.rs) | GUI・トレイ・スケジューラ・MCP 間で共有される `SharedController` と排他ジョブ制御 (`job_mutex`)。1 対象 = 1 スナップショット実行・バックアップ前リモート確認・引き渡し (`release_project`)・6 段階引き継ぎ (`takeover_project`)・安全 `unlock`・`forget_snapshot`・監査ログ記録。`restic version` の検出結果は 60 秒（未検出時は 5 秒）キャッシュし、状態取得のたびにプロセスを起動しない |
| [`src/scheduler.rs`](../src/scheduler.rs) | 定期バックアップの実行時刻の計算と、それを起動するバックグラウンドスレッド。0〜60 秒の起動ジッター、待機中のみ (`standby`) 時のスキップと未実施通知抑止、排他ロック延期 (`deferred`) 時の 15 分後再試行（最大 2 回）、`maintenanceRole: "primary"` の `maintenanceAt` 枠でのみ自動 `forget --prune` / 定期 `verify` を実行 |
| [`src/agent/protocol.rs`](../src/agent/protocol.rs) | `agent-bridge.json` 接続情報、256-bit CSPRNG 相互認証トークン、定数時間比較 (`token_matches`) |
| [`src/agent/bridge.rs`](../src/agent/bridge.rs) | 常駐アプリ側の Tokio 名前付きパイプサーバー (`\\.\pipe\restic-tray-agent-<hex>`) |
| [`src/agent/mcp.rs`](../src/agent/mcp.rs) | `--mcp` 起動時の stdio JSON-RPC 2.0 サーバー（22 ツール提供、名前付きパイプ中継 + ヘッドレス直接実行フォールバック） |
| [`src/tray.rs`](../src/tray.rs) | `tray-icon` による Windows タスクトレイ常駐と右クリックメニュー。ウィンドウをトレイに格納している間はフレーム処理（`logic` / `ui`）を止めているため、イベントはフレーム側で読まずにイベントハンドラで直接処理する |
| [`src/notify.rs`](../src/notify.rs) | Windows のトースト通知（macOS は `osascript`、Linux は `notify-send`）。ウィンドウを隠している間に起きた問題（復元確認の失敗、バックアップが長く成功していない）を知らせる。`windows` クレートの WinRT API を使い、`unsafe` は書かない |
| [`src/instance.rs`](../src/instance.rs) | 多重起動の防止。データフォルダごとの固定名の名前付きパイプを最初のプロセスが持ち、2 回目の起動はそこへ接続して既存ウィンドウを表示させてから終了する |
| [`src/window.rs`](../src/window.rs) | メインウィンドウの `HWND` を保持し、任意のスレッドから Win32 (`ShowWindow` / `SetForegroundWindow`) で表示・前面化する。DPAPI 以外で唯一 `unsafe` を許可しているモジュール |
| [`src/theme.rs`](../src/theme.rs) | ダーク / ライトのデザイントークン、Windows システムフォントの読み込み、egui スタイル。[`theme/icons.rs`](../src/theme/icons.rs) に `egui::Painter` のベクターアイコン (`VectorIcon`) とブランドマーク、[`theme/widgets.rs`](../src/theme/widgets.rs) に独自ウィジェット |
| [`src/app.rs`](../src/app.rs) | サイドバーと 5 ページ構成のメイン GUI（状態・バックグラウンド処理・トースト・確認ダイアログ・スクリーンショット自動出力）。各ページは [`src/app/`](../src/app/) の `overview` / `targets` / `snapshots` / `history` / `settings`、日時と容量の表示整形は `format` |

## 2. Cloudflare R2 と Restic の接続方式

Restic は S3 互換バックエンドをネイティブサポートしているため、Cloudflare R2 に対して以下のリポジトリ URL と環境変数で接続します：

- `RESTIC_REPOSITORY`: `s3:https://<accountId>.r2.cloudflarestorage.com/<bucket>[/<prefix>]`
- `AWS_ACCESS_KEY_ID`: DPAPI ストアから復号した R2 S3 Access Key ID
- `AWS_SECRET_ACCESS_KEY`: DPAPI ストアから復号した R2 S3 Secret Access Key
- `AWS_DEFAULT_REGION`: `auto`（既定）
- `RESTIC_PASSWORD`: DPAPI ストアから復号した Restic 暗号化パスワード
- 追加オプション: `-o s3.connections=<maxConnections>`（既定 10）
- **バケット／プレフィックス削除 (`src/r2_s3.rs`)**: Restic にはバケット削除コマンドがないため、内蔵の純 Rust S3 互換 API クライアント（`ureq` + Windows Schannel TLS + AWS SigV4、派生署名鍵は `Zeroize`）で `ListObjectsV2` / `DeleteObjects`（最大 1,000 件一括）/ `ListMultipartUploads` / `AbortMultipartUpload` / `DeleteBucket` を実行します。`prefixOnly` は末尾 `/` 付きで対象プレフィックス配下のみ削除し（プレフィックス空欄は拒否）、`entireBucket` は全オブジェクトと未完了マルチパートを削除したうえで `DeleteBucket` を試行します。Object Read & Write トークンで `DeleteBucket` のみ `403 AccessDenied` になった場合は空化済みの結果とダッシュボード削除の案内を返します。

## 3. MCP サーバーの信頼境界と防御前提

- **信頼する主体**: 同一 Windows ユーザーアカウント上で動作し、`%LOCALAPPDATA%\io.github.nayuengin.restic-tray\agent-bridge.json` を読み取れるローカル MCP クライアントを受け入れます。ネットワーク越しのリモートクライアントは名前付きパイプの `reject_remote_clients(true)` と相互トークン認証（定数時間比較 `token_matches`）で拒否します。
- **想定する脅威と防御**:
  - **任意コマンド実行・オプション注入の防止**: `restic.exe` はシェル非経由（`std::process::Command` + `CREATE_NO_WINDOW`）で引数配列のみを渡し、スナップショット ID を `latest` または 4〜64 桁の 16 進文字列に検証したうえで位置引数の前に `--` を置きます。また、認証情報を受け取る実行ファイル (`resticBinaryPath`) の変更は MCP からは拒否し、ローカルの GUI 設定または `config.json` 編集のみに限定します。
  - **秘密情報の隔離**: R2 キーとリポジトリパスワードは Windows DPAPI（`src/secrets.rs`）で `credentials.dat` に分離保存し、MCP 応答や設定更新 API には一切露出・受理しません。既存の `credentials.dat` が復号不能（別ユーザーや破損）な場合、部分保存で元ファイルを上書きしないよう明確なエラーで停止します。
  - **破壊的操作の多段ゲート**: `agent.enabled` による全体停止に加え、`agent.allowRestore`（既定 `true`。元の場所とは別の検証済みフォルダ `targetDir` へ復元する操作） / `agent.allowPrune`（既定 `false`） / `agent.allowDeleteStorage`（既定 `false`） / `agent.allowHandoff`（既定 `false`）で操作別に制御します。`forget_prune` は `dryRun: false` と `confirmPrune: true` の明示両方、`delete_r2_bucket` は `confirmBucketName` 完全一致、`takeover_project` は `confirmProjectKey` 完全一致を要求します。`agent.allowPrune: false` の場合は直接の `forget_prune` だけでなく、MCP からの `update_retention_policy` による保持ポリシー変更、`update_schedule` による `autoPruneAfterBackup` 有効化・`maintenanceAt` の新規設定／変更・自動 Prune 発動につながる `maintenanceAt` 解除、および MCP 起点バックアップ後の自動 Prune もすべて抑止します。`update_retention_policy` は未知キー・型違い・範囲外（`keepLast: 0..=1000`, `keepDaily: 0..=365`, `keepWeekly: 0..=520`, `keepMonthly: 0..=240`）も厳密に拒否します。
  - **選択範囲の誤拡大・パス境界防御**: `includePaths`・`targetIds`・`projectKeys` の空配列や不正型は「省略」と区別して即座に拒否し、部分復元が全体復元へ広がるのを防ぎます。対象フォルダと復元先は `src/fs_safety.rs`（`dunce::canonicalize`）で正規化し、`..` トラバーサル・未解決リパースポイント・ドライブ直下・実行環境の Windows システム領域（`SystemRoot` / `ProgramFiles` 等）への復元を拒否します。

## 4. 複数 PC 共有プロジェクトと引き継ぎの概要

1 対象 = 1 スナップショットを原則とし、共有プロジェクトには安定した `projectKey`（`project:<key>` タグ）を付与して単一ライター (`writerState`: `active` / `standby`) で運用します。`.git` を含むリポジトリは共有引き継ぎ対象外とし、自動 `forget --prune` と定期復元確認は `maintenanceRole: "primary"` の 1 台のみが `schedule.maintenanceAt` 枠で担当します。

### 同期状態 (`ProjectSyncStateKind`)

| 状態 | 意味 |
| :--- | :--- |
| `local_latest` | この PC が `active` であり、リポジトリ確認済みで最新スナップショットと整合している |
| `remote_newer` | 他 PC がより新しい有効スナップショットを公開しており、この PC に未バックアップ変更はない |
| `not_local` | リポジトリ上に共有プロジェクトが存在するが、この PC には未配置 |
| `diverged` | 他 PC が新しい一方でローカルにも未バックアップ変更がある、または前回の引き継ぎが中断している |
| `local_only` | `projectKey` を持たないローカル専用バックアップ対象 |
| `standby` | この PC で引き渡し済み（待機中）であり、自動バックアップ対象から外れている |
| `unchecked` | `projectKey` 登録後にまだリポジトリ確認（`refresh`）を行っていない |

### 引き継ぎ用タグ (`handoff:*`) と中断チェックポイント (`handoff_status`)

- `handoff:staging`: 引き渡し・所有権登録時の仮スナップショット（`project:<key>` なしで作成し、終了コード 0 を確認後に `restic tag` で公開）。終了コード 3 の不完全スナップショットを他 PC から不可視にします。
- `handoff:released`: `release_project` により明示的に作業終了・引き渡しされた最新スナップショット。
- `handoff:claimed` / `handoff:from:<sourceId>`: `takeover_project` による所有権取得スナップショット。引き継ぎ元 ID を記録することで、PC 間の時計ずれがあっても二重取得を検知します。
- `handoff:safety-backup`: 復元前にローカル未バックアップ変更を退避したスナップショット（自動 `forget` の保持枠を消費せず保護対象）。
- `handoff_status`（SQLite `project_sync_state`）: `idle` / `releasing` / `restoring` / `claiming` の 4 状態で中断点を記録し、再実行時に安全に収束・再開します。

### 引き渡し (`release_project`) と 6 段階引き継ぎ (`takeover_project`)

- **引き渡し (`release_project`)**: `.git`・`Temp\UnityLockfile`・中断状態・最新リモート状態を検査 → `handoff:staging` でバックアップ → `handoff_status = "releasing"` 保存とローカル `writerState = standby` 化 → `restic tag` で `project:<key>` + `handoff:released` へ昇格・確認 → `idle` へ復帰。
- **引き継ぎ (`takeover_project`) の 6 段階**:
  1. **Stage 1（リモート最新特定）**: `handoff:safety-backup` / `staging` を除外して最新有効スナップショット（または旧複合スナップショット候補）を特定し、`handoff:from:` 二重取得や旧スナップショット内の `.git` を検査。
  2. **Stage 2（復元先検証）**: 既存対象パス・指定 `localPath`・`defaultProjectRoot` から復元先を決定し、システムパス保護・既存対象との重複・`.git`・`UnityLockfile` を検証。
  3. **Stage 3（ローカル変更検知と再開判定）**: 中断中の `restoring` / `claiming` の再開可否を厳密一致で判定し、既存フォルダに未バックアップ変更があるかを検査（`dryRun: true` はここで確認結果を返却）。
  4. **Stage 4（保護ガード・退避）**: 相手 PC 未引き渡し時の `forceWhileRemoteActive` 確認、ローカル変更時の `handoff:safety-backup` 退避（または明示破棄確認 `confirmDiscardLocalChanges`）。
  5. **Stage 5（サブパス復元）**: `handoff_status = "restoring"` を記録し、復元直前のシンボリックリンク／ジャンクション再検証のうえ `restic restore --target <localPath> [--delete] [--exclude ...] -- <id>:<subpath>` を実行（Unity フォルダ・除外マーカーを保護）。
  6. **Stage 6（競合再確認・所有権公開）**: 復元中の他 PC 競合と保存先変更を再検証 → 対象を `active` 登録し `handoff_status = "claiming"` を記録 → `handoff:staging` 作成 → `handoff:claimed` + `handoff:from:<sourceId>` へ昇格・最終線形化検証（競合検知時は自 PC を `standby` へ戻して中断）。
