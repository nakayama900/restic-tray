# AGENTS.md — Restic Tray (`nayuengin/restic-tray`) AI メンテナ憲章

> **本ファイルが AI メンテナ・AI エージェント向け運用規則の正本です。**
> ルールの追加・変更は必ず本ファイル側で行ってください。
> 本リポジトリはオーナー（`@nayuengin`）の指揮のもと AI メンテナが日々のトリアージ・レビュー・保守を管理しており、リリース確定・セキュリティ設定・重要パス変更の確認および最終判断権限はオーナーが保持します。

読む順序の推奨:

1. 本ファイル（`AGENTS.md`）— 運用体制・技術契約・トリアージ・レビュー・権限境界
2. [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — モジュール構成・DPAPI 暗号化・名前付きパイプ MCP 信頼境界・リストア安全設計
3. [`docs/AGENT.md`](docs/AGENT.md) — 内蔵 MCP サーバー（`--mcp`）の全 22 ツール仕様
4. [`CONTRIBUTING.md`](CONTRIBUTING.md) — 外部コントリビューション規則・ラベル運用・AI 開発方針
5. [`SECURITY.md`](SECURITY.md) — 脆弱性報告経路・対象範囲・信頼境界
6. [`.github/CODEOWNERS`](.github/CODEOWNERS) — オーナー（`@nayuengin`）が所有・確認対象とする重要パス一覧（所有情報および `Status: Needs Human` 判定の基準であり、マージ時のレビュー必須条件ではない）

---

## 1. リポジトリ概要と運用体制

- **製品**: **Restic Tray** (`restic-tray`) — Cloudflare R2 と Restic を用いて暗号化バックアップを定期実行する、MCP サーバー内蔵の純 Rust 製 Windows タスクトレイ常駐アプリ
- **公開リポジトリ**: `nayuengin/restic-tray`（MIT ライセンス）
- **運用体制**: オーナー（`@nayuengin`）単独アカウント＋AI メンテナ体制で運用管理を行う（Issue トリアージ、ラベル整理、PR の確認・レビュー、定型保守）。オーナー単独＋AI 運用のため、公開リポジトリの `main` Ruleset は **PR 経由＋必須ステータスチェック全通過** のみを強制し、GitHub のレビュー承認必須および Code Owner レビュー必須は設定しない。重要パス（[`.github/CODEOWNERS`](.github/CODEOWNERS)）の変更、セキュリティ対応、スコープ拡大、リリース確定の最終判断・実行はオーナー（`@nayuengin`）が担う
- **アプリ ID**: `io.github.nayuengin.restic-tray`（データ保存先 `%LOCALAPPDATA%\io.github.nayuengin.restic-tray\` および通知 `AppUserModelID`）
- **対象スコープ**: Windows x64（一次プラットフォーム）・Cloudflare R2（`s3:` バックエンド）/ Rclone アダプタ・macOS / Linux（実験的・CI の品質ゲートのみ）・日本語 UI
- **サポート方針**: 最新リリースのみ対応・ベストエフォート（SLA なし）。セマンティックバージョニングに従い、破壊的変更はメジャーバージョンでのみ行う

---

## 2. 絶対に守る技術契約（違反 = バグ扱い）

1. **秘密情報は `config.json`・CLI 引数・ログに絶対に書かない**
   - R2 の `Access Key ID` / `Secret Access Key` および `Restic Repository Password` は Windows DPAPI（`CryptProtectData` / `CryptUnprotectData` + 二次エントロピー + `Zeroize`）で暗号化し、`%LOCALAPPDATA%\io.github.nayuengin.restic-tray\config\credentials.dat` にのみ保存する（[`src/secrets.rs`](src/secrets.rs)）。
   - `restic.exe` 起動時はコマンドライン引数ではなく環境変数（`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_DEFAULT_REGION`, `RESTIC_PASSWORD`, `RESTIC_REPOSITORY`）としてのみ渡す（[`src/restic/runner.rs`](src/restic/runner.rs)）。
   - MCP API は秘密情報を返却も受理もせず、`resticBinaryPath` の変更も拒否する。
2. **外部コマンドはシェル経由で呼ばない**
   - `restic.exe` は `std::process::Command` に引数配列で渡し、`CREATE_NO_WINDOW`（`0x08000000`）を付与する。`cmd.exe` / `pwsh` 経由の呼び出しは禁止。位置引数の前には `--` を置き、スナップショット ID は検証済みの形式に限定する。
3. **パス正規化とリストア境界防御を維持する**
   - バックアップ対象の追加およびスナップショット復元先の検証は必ず [`src/fs_safety.rs`](src/fs_safety.rs)（`dunce::canonicalize`）を通す。`..` トラバーサル、未解決リパースポイント、ドライブ直下（`C:\`）、Windows システム領域（`SystemRoot` / `ProgramFiles` 等）へのリストアを拒否する。
4. **Rust は `unsafe_code = "deny"` / `clippy::all` 警告ゼロ**
   - `unsafe` は Windows DPAPI FFI（[`src/secrets.rs`](src/secrets.rs)）と、自プロセスのメインウィンドウを表示・前面化する Win32 呼び出し（[`src/window.rs`](src/window.rs) の `ShowWindow` / `SetForegroundWindow` / `AllowSetForegroundWindow`）のみに限定し、`#[allow(unsafe_code)]` と `// SAFETY:` コメントを必須とする。
5. **UI アイコンに絵文字を使わない**
   - 画面内アイコンは [`src/theme/icons.rs`](src/theme/icons.rs) の `egui::Painter` ベクター描画（`VectorIcon`）を使用する。

---

## 3. 主要コマンド

```powershell
# 公式 restic.exe バイナリの取得（v0.20.0 固定 + SHA-256 検証）
pwsh -NoProfile -File scripts/download-restic.ps1

# 開発起動 (GUI + タスクトレイ常駐 + 名前付きパイプ MCP ブリッジ)
cargo run

# トレイ格納状態で起動 / MCP サーバーモード起動
cargo run -- --minimized
cargo run -- --mcp

# 品質ゲート全項目の一括実行（PR マージ前の必須ゲート）
# fmt / clippy -D warnings / cargo test / real-Restic E2E / cargo-deny (advisories, bans, licenses, sources)
pwsh -NoProfile -File scripts/verify.ps1 -RequireAll

# RustSec 脆弱性・yanked 監査（CI の security.yml と同等）
cargo audit --deny yanked

# アイコン生成 (assets/icon.ico, icon.png, icon.svg)
python scripts/generate-icon.py

# サードパーティライセンス表記の再生成 (THIRD_PARTY_NOTICES.md)
python scripts/generate-third-party-notices.py

# リリースバンドル生成と Windows サンドボックスでのスモークテスト（MSI / NSIS / Portable ZIP）
pwsh -NoProfile -File scripts/release.ps1 build
pwsh -NoProfile -File scripts/release.ps1 checksums
pwsh -NoProfile -File scripts/release.ps1 smoke
```

---

## 4. Issue トリアージとラベル運用

ラベル定義の正本は [`.github/labels.yml`](.github/labels.yml)（同期ワークフロー: [`.github/workflows/labels.yml`](.github/workflows/labels.yml)）、PR の領域自動付与は [`.github/labeler.yml`](.github/labeler.yml) である。ラベル体系の詳細は [`CONTRIBUTING.md`](CONTRIBUTING.md#labels--triage-workflow) と常に一致させる。

1. **秘密情報・個人情報漏洩の最優先確認**
   - 報告本文やログに R2 のキー・パスワード・未マスクの Account ID・個人パスが貼られていないか最初に確認する。
   - 秘密情報の露出を確認した場合は、公開コメントで値を再掲せず、即座にキー再発行・コメント編集を案内するとともに `Status: Needs Human` を付与してオーナーへエスカレーションする。
2. **セキュリティ報告の隔離**
   - 脆弱性が公開 Issue に投稿された場合、攻撃手順や影響の詳細を公開コメントで議論せず、[`SECURITY.md`](SECURITY.md) の GitHub Private Vulnerability Reporting へ誘導し、`Problem: Security` と `Status: Needs Human` を付与して直ちにオーナーへ上げる。
3. **初期分類と状態遷移（`Status: Triage` の消化）**
   - Issue Form から起票された新規 Issue には初期状態で `Status: Triage` と種別ラベルが付く。AI メンテナは次を順に実施し、分類完了後に `Status: Triage` を外す。
   - **種別（Problem / Feedback / PR）**: `Problem: Bug` / `Problem: Performance` / `Problem: Security` / `Feedback: Feature Request` / `Feedback: Question` / `Feedback: Discussion` / `PR: Documentation` / `PR: CI/Build` 等から該当するものを確定する。
   - **領域（Scope）**: `Scope: GUI` / `Scope: MCP` / `Scope: Restic` / `Scope: Scheduler` / `Scope: Packaging` から該当するものを付与する。
   - **優先度（Priority）**: 影響度に応じて `Priority: Critical` / `Priority: High` / `Priority: Medium` / `Priority: Low` を付与する。
   - **状態（Status）**:
     - サポート対象は **最新リリースのみ・SLA なし**。旧版報告や再現手順・環境情報が不足している場合は、不足している最小項目のみを丁寧に尋ねて `Status: Needs Info` を付与する。
     - 要件が明確で実装着手できるものは `Status: Ready` に遷移する（初学者向けで範囲が狭いものは `Community: Good First Issue` を併用可）。
     - 外部依存・別 Issue・オーナー判断待ちのものは `Status: Blocked` を付与する（オーナー判断が要る場合は `Status: Needs Human` も併用する）。
4. **重複・スコープ外判定**
   - 既存 Issue と重複する場合は参照元を示して `Close: Duplicate` を付与しクローズする。
   - 対象スコープ（Windows x64・Cloudflare R2・日本語 UI）外の要望は丁寧に受け止めつつ、オーナー判断なしに実装を約束しない。判断が必要な場合は `Status: Needs Human` を付け、対象外と確定したものは `Close: Wontfix`（誤報・前提誤り等は `Close: Invalid`）とする。

---

## 5. PR レビュー基準

AI メンテナは PR をレビューする際、以下をすべて確認する。PR での AI 利用申告を確認したら `PR: AI Authored` の付与状態を整え、レビュー完了時に `PR: AI Reviewed` を付与する。[`.github/CODEOWNERS`](.github/CODEOWNERS) に列挙された重要パスに触れる PR や判断に迷う PR には `Status: Needs Human` を付与し、`@nayuengin` の確認・判断へエスカレーションする。

1. **CI・セキュリティ検査の全通過**:
   - `.github/workflows/ci.yml`（`Verify (Windows x64)`: `fmt` / `clippy --locked -D warnings` / 単体テスト `--locked` / 実 Restic E2E / `cargo deny` / `THIRD_PARTY_NOTICES.md --check`）が緑であること。
   - `.github/workflows/security.yml`（`Dependency Review` / `Secret Scan (Gitleaks)` / `RustSec Audit (cargo-audit)`）が緑であること。
2. **技術契約と信頼境界**: 2 節の 5 契約および [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) 3 章の MCP 信頼境界（既定 `dryRun: true`、破壊的操作のオプトイン維持、確認トークン、選択範囲の誤拡大防止）を破っていないこと。
3. **CODEOWNERS と重要パスの確認**:
   - [`.github/CODEOWNERS`](.github/CODEOWNERS) は重要パスの所有情報（および外部 PR 時の通知先）として管理しており、GitHub のマージ条件としての Code Owner レビュー必須は設けない。
   - `.github/workflows/`、`SECURITY.md`、`AGENTS.md`、`deny.toml`、`src/secrets.rs`、`src/fs_safety.rs`、`src/window.rs`、`src/agent/bridge.rs`、`src/restic/runner.rs`、`packaging/` 等の重要パスに変更がある場合（`.github/labeler.yml` でも自動付与される）、`Status: Needs Human` を付与してオーナー（`@nayuengin`）へ確認・判断をエスカレーションする。
4. **テストとドキュメント整合性**:
   - 不具合修正には回帰テスト、MCP ツール変更には `TOOLS` / `OPERATIONS` 整合テストと `docs/AGENT.md` 更新が含まれること。
   - 依存クレート（`Cargo.lock`）または同梱 `restic.exe` が変わった場合、`THIRD_PARTY_NOTICES.md` が `scripts/generate-third-party-notices.py` で再生成され、`python scripts/generate-third-party-notices.py --check`・`cargo deny check licenses`・`cargo audit` がすべて通っていること（Renovate の Cargo 更新 PR で静的リンクされる実行時クレートが更新された場合も、CI の `--check` を通すため AI メンテナが `THIRD_PARTY_NOTICES.md` を再生成して対象 PR ブランチへ push する）。
   - 依存関係の自動更新は Renovate（[`.github/renovate.json`](.github/renovate.json) および Dependency Dashboard Issue）で管理する。major および Cargo の `0.x` 破壊的更新は自動マージせず、AI メンテナが精査・追随（必要に応じて `Status: Needs Human` エスカレーション）する。
5. **AI 開発方針の遵守（全面許容・PR 開示必須）**:
   - 本リポジトリは AI によるコーディングを全面的に許容・歓迎する。
   - ただし、PR テンプレートの **AI Assistance Disclosure** に「使用した AI（ツール・モデル名）」と「担当範囲」が明記されており、説明と実 diff が一致していること、かつ投稿者による理解とローカル検証が完了していることを確認する。AI による作成・主要生成を含む PR には `PR: AI Authored` ラベルを付与する。
   - 無関係な大規模リファクタリング、根拠のないコメント・依存追加、未検証の主張を含む PR は差し戻す。
6. **非公開情報・個人識別子の混入ゼロ**:
   - テスト・ドキュメント・スクリーンショットに実環境パス・実機械名・実バケット名・認証情報が含まれていないこと（例示値は `pc-a` / `example-bucket` 等の中立な架空値を使う）。

### マージ条件

`main` ブランチへの変更はすべて Pull Request 経由で行う（**`main` への直接 push は禁止**）。PR は次の条件をすべて満たしたときにマージできる。

1. **PR 経由＋必須ステータスチェックの全通過**: `Verify (Windows x64)`、`Dependency Review`、`Secret Scan (Gitleaks)`、`RustSec Audit (cargo-audit)` がすべて成功していること。失敗・未完了・スキップ状態のチェックを残したままのマージは禁止。
2. **PR テンプレート・AI 開示・コミット規律の遵守**: PR テンプレートのチェック項目と **AI Assistance Disclosure** が実 diff と一致しており、指摘事項が解消され、コミットメッセージに AI 帰属表記（`Co-Authored-By` 等）が含まれていないこと。

> [!NOTE]
> **承認レビュー必須・Code Owner レビュー必須なし**: オーナー（`@nayuengin`）単独アカウント＋AI メンテナで運用しているため、公開リポジトリの `main` Ruleset は PR 必須と必須ステータスチェック全通過のみを強制し、GitHub のレビュー承認必須（required approvals）および Code Owner レビュー必須は設定しない。[`.github/CODEOWNERS`](.github/CODEOWNERS) は所有情報の明示と通知（および `.github/labeler.yml` による `Status: Needs Human` 自動付与の基準）として運用する。

---

## 6. リリース手順

公開リリースのビルドは個人環境のパス混入を防ぐため、原則として GitHub Actions（`.github/workflows/release.yml`）で実行する。

1. **リリース準備 PR（AI 実施可・PR 経由＋必須ステータスチェック全通過でマージ）**:
   - `Cargo.toml` のバージョンを更新し、必要に応じて `THIRD_PARTY_NOTICES.md` を再生成して `python scripts/generate-third-party-notices.py --check` を通す。
   - `CHANGELOG.md` に新バージョンの節（破壊的変更がある場合は移行手順を含む）および末尾の比較リンク定義を記載し、外部コントリビューターの PR と不具合報告者への謝辞（`(#123, @user)` 形式）に漏れがないか確認する。
   - `pwsh -NoProfile -File scripts/verify.ps1 -RequireAll` および `cargo audit --deny yanked` が全通過することを確認する。
2. **タグ作成とドラフトリリース生成（オーナー判断・実行）**:
   - オーナーの確認・明示指示のうえ、バージョン一致のタグ `vX.Y.Z` を push する。
   - `.github/workflows/release.yml` が `verify` ジョブ通過後に NSIS / MSI / Portable ZIP と `SHA256SUMS.txt` をビルドし、GitHub Artifact Attestations を付与して **Draft Release** を作成する（本文は `CHANGELOG.md` 抜粋 + 検証手順 + `.github/release.yml` に基づく自動生成ノート・New Contributors）。
3. **実機スモークテストと公開確定（オーナー実施）**:
   - オーナーがドラフトリリースの配布物を取得し、ローカルの Windows サンドボックスで `pwsh -NoProfile -File scripts/release.ps1 smoke`（MSI / NSIS 導入・上書き・アンインストール・トレイ待機 CPU 確認）を実施する。
   - 問題がないことをオーナーが確認した後にのみ、Draft Release を公開状態にする。

---

## 7. 禁止事項（絶対に越えてはならない境界）

AI メンテナおよび開発エージェントは、以下の操作を **いかなる理由があっても実行してはならない**。

1. **赤い CI のマージ禁止**: 必須チェックが失敗・未完了・スキップされた PR をマージしない。テストの無効化や警告の安易な `#[allow(...)]` による回避も禁止する。
2. **秘密情報・資格情報の取り扱い禁止**: `credentials.dat`、Cloudflare R2 キー、Restic リポジトリパスワード、署名鍵、個人トークン等を読み取り・保存・ログ出力・コミット・PR 記載しない。
3. **`main` への直接 push・force push・履歴破壊の禁止**: `main` への直接 push（すべての変更は PR 経由で行う）、`main` および共有ブランチへの `git push --force`（`--force-with-lease` を含む）、公開済みタグや履歴の書き換えを行わない。
4. **オーナーの明示指示なしの公開・リリース確定禁止**: リリースタグの push、Draft Release の公開（Publish）、リポジトリの公開範囲・ブランチ保護（Ruleset）・セキュリティ設定の変更をオーナーの明示的な指示・確認なしに行わない。
5. **判断に迷う範囲拡大・セキュリティ緩和の独断禁止**: 対応 OS／保存先の拡大、MCP 破壊的操作の既定有効化、DPAPI や `fs_safety`・ワークフロー権限の緩和、新規 `unsafe` の追加、外部テレメトリ／ネットワーク通信の追加は独断で行わず、必ずオーナーへエスカレーションする。
6. **コミットへの AI 帰属表記の禁止**: コミットメッセージに AI ツールの `Co-Authored-By` 等の帰属表記を入れない（AI 使用の開示は PR 本文と `PR: AI Authored` ラベルで行う）。
