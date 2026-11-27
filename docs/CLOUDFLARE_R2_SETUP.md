# Cloudflare R2 の準備手順 (`docs/CLOUDFLARE_R2_SETUP.md`)

Restic Tray はバックアップの保存先として **Cloudflare R2**（S3 互換オブジェクトストレージ）を使用します。
この文書では、Cloudflare ダッシュボードで R2 バケットと最小権限の API トークンを作成し、Restic Tray に設定するまでの手順を説明します。

> 画面の項目名や料金体系は Cloudflare 側の更新で変わることがあります。最新の仕様と料金は Cloudflare 公式ドキュメントを参照してください。
> - [Cloudflare R2 Pricing](https://developers.cloudflare.com/r2/pricing/)（エグレス［取り出し］料金無料・無料枠・従量課金の詳細）
> - [Create buckets (Cloudflare R2 Docs)](https://developers.cloudflare.com/r2/buckets/create-buckets/)
> - [Authentication / API tokens (Cloudflare R2 Docs)](https://developers.cloudflare.com/r2/api/tokens/)

---

## 1. 事前準備

1. Cloudflare アカウントを作成し、ダッシュボード（`https://dash.cloudflare.com/`）にログインします。
2. 左メニューの **R2 Object Storage** を開き、R2 を有効化します。
   - 無料枠の範囲内で使う場合でも、Cloudflare 側の仕様により支払い方法（クレジットカード等）の登録が必要です。
   - Cloudflare R2 は取り出し（エグレス）料金がかからないため、復元や Restic Tray の定期的な読み戻し確認（`restic check --read-data-subset`）による転送量課金を気にせず運用できます。料金の最新条件は [Cloudflare R2 Pricing](https://developers.cloudflare.com/r2/pricing/) を確認してください。

---

## 2. バケットを作成する

1. 左メニューから **R2 Object Storage** → **Overview** を開きます。
2. **Create bucket** をクリックします。
3. **Bucket name**（バケット名）を入力します。
   - 命名規則: 3〜63 文字の英小文字（`a-z`）、数字（`0-9`）、ハイフン（`-`）のみ使用可能。先頭・末尾にハイフンは使えません（例: `my-restic-backups`）。
4. **Location**（リージョンヒント）や **Default Storage Class**（通常は **Standard**）を確認し、**Create bucket** で作成します。
   - データの保存先を特定地域に固定したい場合のみ Jurisdiction を設定します。通常は既定（Automatic / 必要なら最寄り地域の Location Hint）で問題ありません。

---

## 3. アカウント ID を確認する

Restic Tray はアカウント ID から S3 互換エンドポイント（`https://<ACCOUNT_ID>.r2.cloudflarestorage.com`）を自動で組み立てます。

アカウント ID（32 文字の 16 進文字列）は次のいずれかで確認できます。

- R2 の **Overview** 画面右側にある **Account Details** の **Account ID**
- Cloudflare ダッシュボードの URL（`https://dash.cloudflare.com/<ACCOUNT_ID>/r2/...`）の `<ACCOUNT_ID>` 部分

---

## 4. R2 API トークンを作成する

S3 互換 API 経由で Restic から読み書きするための **Access Key ID** と **Secret Access Key** を発行します。

1. R2 の **Overview** 画面右側にある **Account Details** 内の **API Tokens** の横にある **Manage**（または **Manage R2 API Tokens**）をクリックします。
2. トークンの種別で **Account API tokens**（推奨）または **User API tokens** を選び、**Create Account API token**（または **Create User API token**）をクリックします。
   - **Account API tokens** はアカウントに紐づくため、ユーザー構成の変更に左右されにくく長期運用に向きます。
3. 各項目を設定します。
   - **Token name**: 用途が分かる名前を付けます（例: `restic-tray-main-pc`）。PC ごとにトークンを分けておくと、紛失時に該当 PC 分だけ失効できて安全です。
   - **Permissions（権限）**: 次の基準で選びます。

| 権限 | バケット限定 | できること | 推奨用途 |
| :--- | :--- | :--- | :--- |
| **Object Read & Write**（推奨） | 可能（**Specify bucket(s)**） | バックアップ、一覧取得、復元、整合性チェック、`forget --prune`、プレフィックス配下削除、バケット内の全オブジェクト削除（空化） | **通常のバックアップ運用（最小権限）**。バケット本体削除（`DeleteBucket`）のみ `403 AccessDenied` になるため、空化したあとに Cloudflare ダッシュボードからバケットを削除します。 |
| **Admin Read & Write** | 不可（アカウント内の全 R2 バケット） | 上記すべてに加え、Restic Tray からのバケット本体の一括削除 | アプリからバケット本体まで直接削除したい場合のみ（通常運用では非推奨）。 |

   - **Specify bucket(s)**（**Object Read & Write** 選択時）: **Apply to specific buckets only** を選び、手順 2 で作成したバックアップ専用バケットだけを指定します。
   - **TTL（有効期限）**: 運用方針に合わせて **Forever**（無期限）または任意の期限を選びます。期限を設けた場合は、期限切れ前にトークンを再発行して Restic Tray 側を更新してください。
4. **Create Account API Token**（または **Create User API Token**）をクリックします。
5. 完了画面に表示される以下の 2 つを控えます。
   - **Access Key ID**
   - **Secret Access Key**

> [!IMPORTANT]
> **Access Key ID** と **Secret Access Key** は作成直後の画面で **1 度しか表示されません**。画面を閉じた後に確認できなくなった場合は、新しくトークンを発行し、不要になった古いトークンを失効（Revoke / Delete）してください。

---

## 5. Restic Tray に入力して初期化する

1. Restic Tray を起動し、左メニューの **設定** を開きます。
2. 以下の対応表のとおりに入力します。

| Cloudflare 側の値 | Restic Tray（設定画面）の入力欄 | 保存先 |
| :--- | :--- | :--- |
| **Account ID**（32 文字） | 保存先 → **アカウント ID** | `config.json`（平文・非秘密） |
| 作成した **Bucket name** | 保存先 → **バケット** | `config.json`（平文・非秘密） |
| 任意（例: `workstation`、空欄ならバケット直下） | 保存先 → **バケット内のフォルダ** | `config.json`（平文・非秘密） |
| API トークンの **Access Key ID** | 認証情報 → **アクセスキー ID** | `config\credentials.dat`（Windows DPAPI 暗号化） |
| API トークンの **Secret Access Key** | 認証情報 → **シークレットアクセスキー** | `config\credentials.dat`（Windows DPAPI 暗号化） |
| 利用者が決める強力なパスワード | 認証情報 → **リポジトリのパスワード** | `config\credentials.dat`（Windows DPAPI 暗号化） |

> [!CAUTION]
> **リポジトリのパスワードは必ずアプリとは別の場所（パスワード管理ツールやオフラインの控え）にも保管してください。**
> `credentials.dat` は Windows DPAPI でその PC のその Windows ユーザーに紐づけて暗号化されるため、PC の故障・紛失や Windows の再インストール時には復号できません。R2 の API トークンは Cloudflare ダッシュボードで再発行できますが、**リポジトリのパスワードを失うと R2 上の暗号化バックアップは二度と復元できません**。

3. 画面下部の保存バーで **保存**（`Ctrl+S`）を押し、保存先設定を `config.json` に書き込みます。
4. 認証情報欄の **認証情報を保存** を押し、DPAPI で暗号化して保存します。
5. 新しいバケット／プレフィックスの場合は **リポジトリを作成** を実行します（すでに他の PC で初期化済みのリポジトリへ接続する場合は **接続を確認** を実行します）。
6. 接続が確認できたら、左メニューの **フォルダ** でバックアップ対象を追加し、**概要** 画面の **テスト実行**（Dry-Run）で除外対象を確認してから **今すぐバックアップ** を実行してください。

---

## 6. 補足: バケット内のフォルダ（プレフィックス）の使い分け

- **1 台だけで使う場合**: 空欄（バケット直下）でも、PC 名などのプレフィックス（例: `workstation`）を付けても構いません。プレフィックスを付けておくと、同じバケット内に別の用途のリポジトリを分けて置いたり、プレフィックス配下だけを削除したりできます。
- **複数 PC で共有リポジトリを使う場合**: 共有する PC すべてで **同じアカウント ID・バケット・プレフィックス・リポジトリのパスワード** を指定してください。API トークンは PC ごとに別々に発行して問題ありません。

---

## 7. API トークンが漏洩したときの対処

万が一 **Access Key ID** や **Secret Access Key** をログや公開 Issue 等に誤って貼り付けてしまった場合は、次の順で対処してください。

1. Cloudflare ダッシュボードの **R2 Object Storage** → **Manage R2 API Tokens** で該当トークンを直ちに **Revoke / Delete** します。
2. 新しい API トークン（**Object Read & Write**、対象バケット限定）を発行します。
3. Restic Tray の **設定 → 認証情報** に新しい **アクセスキー ID** と **シークレットアクセスキー** を入力して **認証情報を保存** します（リポジトリのパスワード欄を空欄のまま保存すれば、既存のリポジトリパスワードは維持されます）。
4. **接続を確認** を実行し、新しいトークンで接続できることを確認します。
