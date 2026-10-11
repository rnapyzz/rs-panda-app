# 本番環境の構築・リリース・復旧の手順（AWS）

構成と方針は [architecture.md](architecture.md) の「本番環境（AWS）」と「認証」を参照。この手順書では、AWS のコンソール（または CLI）で行う作業を順に書く。構成をコードで管理する仕組み（Terraform など）とリリースの自動化（GitHub Actions など）は将来の課題とする。

以降の例では、リージョンを `ap-northeast-1`、アプリの URL を `https://fpanda.example.com`、会社のドメインを `example.com` とする。

## 1. 事前の準備

### Google Workspace（SSO）

1. Google Cloud のコンソールでプロジェクトを作り、「OAuth 同意画面」をユーザーの種類「内部」で設定する（会社のアカウントだけが使える）。
2. 「認証情報」で OAuth クライアント ID（種類: ウェブ アプリケーション）を作る。
   - 承認済みのリダイレクト URI: `https://fpanda.example.com/api/auth/oidc/callback`
3. クライアント ID とクライアント シークレットを控える（シークレットは Secrets Manager に入れる）。

### ドメインと証明書

1. 本番のドメイン（例: `fpanda.example.com`）を決める。
2. ACM で証明書を発行する（DNS 検証）。

## 2. AWS の構成

| リソース | 設定 |
| -------- | ---- |
| VPC | パブリック サブネット（ALB）とプライベート サブネット（ECS・RDS）を2つの AZ に作る。ECS から外（Google・Slack）へ出るため NAT ゲートウェイを置く |
| RDS for MySQL 8.4 | プライベート サブネット。文字コードはパラメータ グループで `character_set_server = utf8mb4`・`collation_server = utf8mb4_0900_ai_ci`・`time_zone = Asia/Tokyo`。**自動バックアップの保持期間 30日**。削除保護を有効にする。マルチ AZ は可用性の要件に合わせる |
| ECR | `fpanda-api`・`fpanda-web` のリポジトリ |
| Secrets Manager | `DB_PASSWORD`・`OIDC_CLIENT_SECRET`・`SLACK_WEBHOOK_URL`（使う場合） |
| ECS（Fargate） | クラスター、タスク定義（下記）、サービス（タスク 1台以上、プライベート サブネット） |
| ALB | HTTPS（443、ACM の証明書）のリスナーでターゲット グループ（タスクの nginx、80番）へ転送する。HTTP（80）のリスナーは HTTPS へリダイレクトする。ヘルスチェックのパスは `/api/health` |
| CloudWatch Logs | ロググループ `/ecs/fpanda`（保持期間は運用に合わせる。監査ログは DB にあるので、ここは障害調査用） |
| Route 53 | `fpanda.example.com` を ALB に向ける |

### セキュリティ グループ

- ALB: インターネットから 443・80 を受ける
- ECS: ALB から 80 だけを受ける
- RDS: ECS から 3306 だけを受ける

### タスク定義

1つのタスクに2つのコンテナを置く（ネットワーク モード `awsvpc`。nginx は `127.0.0.1:8080` で API に転送する）。

| コンテナ | イメージ | ポート | 設定 |
| -------- | -------- | ------ | ---- |
| `web` | `fpanda-web`（`web/Dockerfile` の `prod` ステージ） | 80 | なし。ALB のターゲット |
| `api` | `fpanda-api`（`api/Dockerfile`） | 8080 | 下の環境変数 |

`api` コンテナの環境変数:

| 名前 | 値 |
| ---- | -- |
| `DB_HOST` / `DB_PORT` / `DB_USER` / `DB_NAME` | RDS のエンドポイントなど |
| `DB_PASSWORD` | Secrets Manager から |
| `TZ` | `Asia/Tokyo` |
| `COOKIE_SECURE` | `true` |
| `TRUST_PROXY` | `true`（ALB の後ろで動かすため） |
| `APP_BASE_URL` | `https://fpanda.example.com` |
| `OIDC_ISSUER` | `https://accounts.google.com` |
| `OIDC_CLIENT_ID` | Google の OAuth クライアント ID |
| `OIDC_CLIENT_SECRET` | Secrets Manager から |
| `OIDC_REDIRECT_URL` | `https://fpanda.example.com/api/auth/oidc/callback` |
| `OIDC_ALLOWED_DOMAINS` | `example.com`（複数ならカンマ区切り） |
| `SLACK_WEBHOOK_URL` | Secrets Manager から（使わなければ設定しない） |

`OIDC_CLIENT_ID` を設定すると SSO が有効になり、パスワードでログインできるのは FP&A だけになる。

## 3. リリース

1. イメージを作って ECR に置く（`<タグ>` はコミットのハッシュなど）。

   ```bash
   docker build -t <アカウント>.dkr.ecr.ap-northeast-1.amazonaws.com/fpanda-api:<タグ> api
   docker build --target prod -t <アカウント>.dkr.ecr.ap-northeast-1.amazonaws.com/fpanda-web:<タグ> web
   docker push <アカウント>.dkr.ecr.ap-northeast-1.amazonaws.com/fpanda-api:<タグ>
   docker push <アカウント>.dkr.ecr.ap-northeast-1.amazonaws.com/fpanda-web:<タグ>
   ```

2. **マイグレーション**: 新しい `fpanda-api` のイメージで、コマンドを `migrate up` にした単発のタスクを実行し、正常に終わったことを CloudWatch Logs で確かめる。マイグレーションは追加のみで作っているので、適用後に古いタスクが動いていても壊れない。
3. タスク定義の新しいリビジョンを作り（イメージのタグを差し替え）、サービスを更新する。ローリング更新で入れ替わる。
4. アプリにログインし、ホーム・シナリオ・予実比較が開けることを確かめる。

### 最初だけ: FP&A の管理者を作る

`fpanda-api` のイメージで、コマンドを `createuser -email <メールアドレス> -name <氏名> -role fpa_admin` にした単発のタスクを実行する。パスワードは標準入力から読むので、ECS Exec でタスクに入って実行する。

SSO が有効でも、FP&A はこのパスワードで「非常用のログイン」から入れる。ほかのユーザーは FP&A がアプリで登録する（パスワードは不要。会社の Google アカウントのメールアドレスで登録する）。

### API を Go 版から Rust 版に切り替えるとき（1回だけ）

- DB のスキーマ・マイグレーション・データの形は変わらない。Rust 版の `migrate up` は、Go 版で適用済みの DB では何もしない（`schema_migrations` を共有する）。
- パスワードのハッシュ・セッション・Cookie の形も同じなので、切り替えた後もログインしたままで使える。
- 環境変数は Go 版と同じ（上の表）。ログは JSON で標準出力に出る（CloudWatch Logs のまま）。
- 切り戻すときは、ECS のサービスを Go 版のイメージのタグに戻す。DB は戻さなくてよい（Rust 版が書いたデータは Go 版でも読める）。

## 4. 監視

CloudWatch のアラームを作り、通知先（メール・Slack）に送る。

| 対象 | 条件の例 |
| ---- | -------- |
| ALB の 5xx（`HTTPCode_Target_5XX_Count`） | 5分間に10件以上 |
| ALB のヘルスチェック（`UnHealthyHostCount`） | 1以上が5分続く |
| RDS の CPU（`CPUUtilization`） | 80% 以上が15分続く |
| RDS の空き容量（`FreeStorageSpace`） | 10GB 未満 |
| RDS の接続数（`DatabaseConnections`） | タスク数 × 20 に近づいた |

監査ログ（`audit_logs`）は消さない。四半期ごとにテーブルの大きさを確認する。

```sql
SELECT table_name, ROUND((data_length + index_length) / 1024 / 1024) AS mb
FROM information_schema.tables WHERE table_schema = 'fpanda' ORDER BY mb DESC LIMIT 10;
```

## 5. バックアップと復旧

- RDS の自動バックアップ（保持期間 30日）で、毎日のスナップショットと、30日以内の任意の時点への復元ができる。
- 大きな変更（年度の締め、組織変更など）の前には、手動のスナップショットを取っておくとよい（手動のスナップショットは期限なく残る）。

### 復旧の手順

1. RDS のコンソールで「特定時点への復元」（または スナップショットから復元）を選び、**新しい DB インスタンス**として復元する（元のインスタンスは調査のため残す）。
2. 復元したインスタンスに、元と同じパラメータ グループ・セキュリティ グループを付ける。
3. タスク定義の `DB_HOST` を新しいエンドポイントに変え、サービスを更新する。
4. アプリにログインし、復元した時点のデータになっていることを確かめる。復元した時点より後の変更は失われるので、利用者に知らせ、変更履歴をもとに入力し直してもらう。

### 復旧の練習

四半期に1回、本番のスナップショットを別のインスタンスに復元し、検証用のタスクから接続して開けることを確かめる。かかった時間を記録する。終わったら、復元したインスタンスを削除する。

## 6. 開発環境との違い

| 項目 | 開発環境（`make up`） | 本番環境 |
| ---- | --------------------- | -------- |
| 画面 | Vite の開発サーバー | ビルドした画面を nginx で配信（`web/nginx.prod.conf`） |
| HTTPS | なし | ALB で終端。HSTS などのヘッダーを nginx で付ける |
| ログイン | パスワード | Google の SSO（パスワードは FP&A の非常用だけ） |
| DB | compose の MySQL（ボリューム） | RDS（自動バックアップ 30日） |
| マイグレーション | compose の `migrate` サービス | ECS の単発のタスク |
