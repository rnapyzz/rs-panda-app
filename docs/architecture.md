# アーキテクチャ

## 技術スタック

| レイヤー       | 技術                                                                 |
| -------------- | -------------------------------------------------------------------- |
| リバースプロキシ | nginx                                                                |
| フロントエンド | React.js + TypeScript + TailwindCSS（Vite でビルド）                  |
| バックエンド   | Rust（tokio・axum・mysql_async などの定番の crate）                   |
| データベース   | MySQL 8.4                                                            |
| 実行環境       | Docker / Docker Compose                                              |

基本方針: **外部ライブラリは定番のものに限り、必要最小限にとどめる。** 追加する場合は理由を残す。OIDC と Excel の書き出しはライブラリを使わずに実装する。

## コンテナ構成

```
                ┌──────────────────────────────┐
  browser ───▶  │ nginx                        │
                │  /      → React の静的ファイル  │
                │  /api/* → api:8080           │
                └───────────────┬──────────────┘
                                │
                        ┌───────▼───────┐        ┌──────────┐
                        │ api (Rust)    │ ─────▶ │ db       │
                        │ axum          │        │ MySQL 8  │
                        └───────────────┘        └──────────┘
```

| サービス | 役割                                                                       |
| -------- | -------------------------------------------------------------------------- |
| `nginx`  | React のビルド成果物を配信し、`/api` を `api` コンテナに転送する            |
| `web`    | React アプリ（開発時は Vite dev server）                            |
| `api`    | Rust の API サーバー（同じイメージに `migrate`・`createuser` も入れる）      |
| `db`     | MySQL 8。データはボリュームに永続化する                                     |

## バックエンド（Rust）

- 言語・ツールチェーン: Rust（edition 2024）。バージョンは `api/rust-toolchain.toml` で固定する（CI も同じ）
- HTTP サーバー・ルーティング: `axum`（`tokio` の上で動く）。ログイン・権限の判定は抽出子（`CurrentUser`・`AdminUser`）で行う
- DB アクセス: `mysql_async`（コネクションプール）。接続の照合順序は `utf8mb4_0900_ai_ci` にそろえる（ビューの中の文字列リテラルと比べても照合順序が混ざらないように）
- JSON: `serde` / `serde_json`。未知の項目は受け付けない。数値は書かれた桁のまま扱う（`arbitrary_precision`）
- 金額・ドライバー値: 浮動小数点を使わず、`num-bigint`・`num-rational` の整数・有理数で計算する
- ログ: `tracing`（本番は JSON で標準出力へ）
- テスト: `cargo test`（単体テストは各モジュール、DB を使う統合テストは `api/tests/`。テストごとに DB を作る）
- 通知: Slack は Incoming Webhook に `reqwest` で送る（SDK は使わない）。定期の送信は API の中の tokio のタスクで行う（外部のスケジューラーは使わない）
- マイグレーション: `api/migrations/<連番>_<説明>.sql` をビルド時に埋め込み（`build.rs`）、自前のランナー（`api/src/migrate.rs`）で適用する。適用済みバージョンは `schema_migrations` に記録する。compose の `migrate` サービスが起動時に適用する
- 計算式の評価: ドライバー式（四則演算・括弧・数値・ドライバー code 参照）は自前の簡易パーサ（`api/src/formula.rs`）で評価する
- コマンド: `server`（API）、`migrate`（`up` / `status`）、`createuser`（最初のユーザーの作成）、`perfseed`（速さの計測用のデータ）。`api/src/bin/` に置く

## 認証

- 通常は **Google Workspace の SSO（OIDC）** でログインする。SSO を設定していない環境（開発環境など）は、メールアドレス＋パスワードでログインする
- ログイン後はセッション Cookie（`fpanda_session`）で認証する（SSO・パスワードで共通）
- パスワード: PBKDF2-HMAC-SHA256（600,000回、`pbkdf2` crate）でハッシュ化。12〜128文字
- セッション: ランダムな32バイトのトークンを Cookie で渡し、DB（`sessions`）には SHA-256 のみ保存する。有効期間は `SESSION_TTL`（デフォルト12時間）。期限切れは API サーバーが1時間ごとに削除する
- Cookie: `HttpOnly`・`SameSite=Lax`。HTTPS 環境では `COOKIE_SECURE=true` で `Secure` を付ける
- CSRF: 別オリジンからの更新系リクエストを拒否するミドルウェア（`Sec-Fetch-Site`、なければ `Origin` と `Host` を比べる）
- 権限: ハンドラーの抽出子 `CurrentUser`（ログイン必須、なければ 401）と `AdminUser`（FP&A のみ、ほかは 403）で制御する。施策のようにデータごとに権限が変わるものはハンドラー内で判定する
- 無効化（`users.is_active = false`）したユーザーはログインできず、既存のセッションも使えなくなる
- 最初の FP&A 管理者は `createuser` コマンドで作成する（`make create-user`、本番は ECS の単発のタスク）

### SSO（OIDC）

- 認可コードフロー（PKCE・`state`・`nonce` 付き）。OIDC のライブラリは使わず、HTTP（`reqwest`）・RSA の署名の検証（`rsa`）・JSON（`serde_json`）で実装する
  1. `GET /api/auth/oidc/login`: `state`・`nonce`・PKCE の検証値を短命の Cookie（10分、`HttpOnly`・`Secure`）に入れ、IdP の認可エンドポイントへリダイレクトする
  2. `GET /api/auth/oidc/callback`: `state` を確かめ、トークンエンドポイントで認可コードを ID トークンに交換する
  3. ID トークン（JWT、RS256）を検証する: IdP の公開鍵（JWKS、`kid` で選ぶ。1時間キャッシュ）で署名、`iss`・`aud`・`exp`・`iat`・`nonce`、`email_verified`、ドメイン（`hd` またはメールアドレスのドメインが `OIDC_ALLOWED_DOMAINS` に含まれる）
  4. メールアドレス（大文字小文字を区別しない）でアプリのユーザーを探す。未登録・無効なら、ログイン画面へ戻して「FP&A に登録を依頼してください」と表示する
  5. 初回は `sub` を `users.oidc_subject` に記録する。以後は `sub` も一致しなければ通さない（メールアドレスの付け替えによるなりすましを防ぐ）
  6. セッションを作り、`/` へリダイレクトする
- 設定（環境変数）: `OIDC_ISSUER`（既定 `https://accounts.google.com`）、`OIDC_CLIENT_ID`、`OIDC_CLIENT_SECRET`、`OIDC_REDIRECT_URL`（例 `https://fpanda.example.com/api/auth/oidc/callback`）、`OIDC_ALLOWED_DOMAINS`（カンマ区切り）。`OIDC_CLIENT_ID` が空なら SSO は無効
- IdP の設定（エンドポイント）は、起動時に `OIDC_ISSUER` の `/.well-known/openid-configuration` から読む
- `GET /api/auth/config`: ログイン画面用。SSO が有効か、パスワードでのログインができるロールを返す

### パスワードでのログイン

- SSO が有効な環境では、パスワードでログインできるのは FP&A（`fpa_admin`）だけ（IdP の障害に備えた非常用）。ログイン画面の「非常用のログイン」から入る。ほかのロールは 403
- ログイン失敗の制限: 同じメールアドレス、または同じ IP からの失敗が15分に5回に達したら、15分間ログインを止める（429）。失敗は `login_attempts` に記録し、成功したらそのメールアドレスの記録を消す。古い記録は1日で消す
- IP は、`TRUST_PROXY=true` のとき `X-Forwarded-For` の最後の値（ALB が付けたもの）、それ以外は接続元のアドレスを使う

| API                      | 内容                     |
| ------------------------ | ------------------------ |
| `POST /api/auth/login`   | ログイン                 |
| `POST /api/auth/logout`  | ログアウト               |
| `GET /api/auth/me`       | ログイン中のユーザー     |

## 本番環境（AWS）

手順は docs/deploy.md。

```
  browser ──HTTPS──▶ ALB（ACM の証明書。HTTP は HTTPS へ転送）
                       │
                       ▼
            ECS Fargate のタスク（1台以上）
            ┌───────────────────────────────┐
            │ nginx（ビルドした画面、/api を転送）│
            │ api（Rust）                    │──▶ RDS for MySQL 8
            └───────────────────────────────┘
                 │ ログ                     秘密情報
                 ▼                          ▲
            CloudWatch Logs           Secrets Manager
```

| 項目 | 内容 |
| ---- | ---- |
| イメージ | `web`（本番用のステージで画面をビルドし nginx に入れる）・`api`。ECR に置く |
| マイグレーション | リリースの前に、`api` イメージの `migrate up` を ECS の単発のタスクとして実行する |
| 自動の処理 | 通知・組織変更の予約は API 内の tokio のタスクで動く。複数台でも DB の一意制約・行ロックで二重に動かない |
| 秘密情報 | `DB_PASSWORD`・`SLACK_WEBHOOK_URL`・`OIDC_CLIENT_SECRET` は Secrets Manager からタスクの環境変数に渡す |
| 環境変数 | `COOKIE_SECURE=true`・`TRUST_PROXY=true`・`TZ=Asia/Tokyo`・`APP_BASE_URL`・OIDC の設定 |
| ヘルスチェック | ALB のターゲットのヘルスチェックは `GET /api/health`（DB に接続できなければ 503） |
| バックアップ | RDS の自動バックアップを 30日に設定する（毎日のスナップショットと、30日以内の任意の時点への復元）。復旧は新しい DB への復元と接続先の切り替え。四半期に1回、復旧を練習する |
| 監視 | CloudWatch のアラーム: ALB の 5xx の増加、ヘルスチェックの失敗、RDS の CPU・空き容量・接続数 |
| 監査ログ | 消さない。テーブルの大きさを監視する |
| リリースの自動化 | 今は手順書で行う。GitHub Actions などでの自動化は将来の課題 |

### HTTP のヘッダー（nginx）

本番用の nginx の設定は `web/nginx.prod.conf`（`web/Dockerfile` の `prod` ステージで画面と一緒にイメージにする）。HSTS は ALB から HTTPS で来たとき（`X-Forwarded-Proto: https`）だけ付ける。


- `Strict-Transport-Security: max-age=31536000; includeSubDomains`（HTTPS のときだけ）
- `X-Frame-Options: DENY`、`X-Content-Type-Options: nosniff`、`Referrer-Policy: strict-origin-when-cross-origin`
- `Content-Security-Policy: default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'`

## マスタ管理 API

組織・セグメント・ユニット・勘定科目・会計科目・割当ルール・ユーザー・確度の段階の CRUD。参照はログインユーザー全員、更新は FP&A（`fpa_admin`）のみ。

| リソース                       | API                                                                 |
| ------------------------------ | ------------------------------------------------------------------- |
| 組織 / セグメント              | `GET/POST /api/{organizations,segments}`、`GET/PUT/DELETE /api/{organizations,segments}/{id}` |
| ユニット                       | `GET/POST /api/units`、`GET/PUT/DELETE /api/units/{id}`。種別 `unit_type`（service / cost_center / corporate、省略時 service）。一覧は既定で廃止したユニットを除く（`include_archived=true` で含める）。`POST /api/units/{id}/merge`（`target_unit_id`・`reason` 必須。施策をすべて移して廃止する）、`POST /api/units/{id}/archive`・`/unarchive` |
| 勘定科目                       | `GET/POST /api/subjects`、`GET/PUT/DELETE /api/subjects/{id}`（`is_restricted` で閲覧制限） |
| 会計科目                       | `GET/POST /api/gl-accounts`、`PUT/DELETE /api/gl-accounts/{id}`（コード・名前・対応する科目 `subject_id`・対象外 `is_excluded`・明細を FP&A 以外に見せない `hide_details`。明細から参照されている会計科目は削除できない） |
| 割当ルール                     | `GET/POST /api/allocation-rules`、`PUT/DELETE /api/allocation-rules/{id}`（会計科目 `gl_account_id`・部門 `department_code`（空は全部門）・施策 `activity_id`。会計科目 × 部門で一意。変更は次の取込・再割当から反映） |
| ユーザー                       | `GET/POST /api/users`、`GET/PUT /api/users/{id}`、`PUT /api/users/{id}/password`。`GET /api/users/{id}/assignments`（担当している施策・所管ユニットの件数）、`POST /api/users/{id}/deactivate`（`successor_user_id`（null で担当者未設定）・`reason`。担当とマネージャーを付け替えてから無効にする）。Slack のメンバー ID（`slack_user_id`、U または W で始まる英大文字・数字、空で未登録）を持つ |
| 確度の段階                     | `GET/POST /api/confidence-levels`、`PUT/DELETE /api/confidence-levels/{id}`（名前・標準の確率 0〜1・判定基準・表示順。コードは作成後に変更できない。施策・内訳から参照されている段階は削除できない） |

- 一覧は `{"items": [...]}` で全件を返す（マスタは件数が少ないためページングしない）
- 組織・セグメント・ユニット・勘定科目はコードを持つ（一意）。組織・セグメント・ユニットは、作成時にコードが空なら `ORG-0001` / `SEG-0001` / `UNIT-0001` 形式で自動採番し、更新時に空なら変更しない
- 更新は `PUT`（全項目を送る）。更新系のボディには任意の `reason`（変更理由）を含められる。`DELETE` も JSON ボディで `reason` を送れる
- すべての更新は1トランザクションで変更セット（`change_sets`）と監査ログ（`audit_logs`）に記録する。ユーザーのパスワードハッシュは記録しない
- 階層（組織・セグメント）
  - `level` はサーバーが親から計算する。親を変更（移動）すると配下の `level` も更新する
  - 自分自身や配下のノードを親にはできない
  - ユニットが所属しているノードの下には子を作れない。ユニットは子を持たない末端ノードにのみ所属できる
  - 子ノードやユニットが残っているノードは削除できない（409）
- 勘定科目: コードは一意。親科目は同じ区分（収益/費用）のみ。子科目がある科目は削除・区分変更できない
- ユーザー: 削除はせず `is_active` で無効化する（無効化するとセッションも削除）。自分自身のロール変更・無効化はできない。パスワード再設定で対象ユーザーのセッションを削除する
- エラー: 入力エラーは 422（`details` にフィールドごとのメッセージ）、存在しない場合は 404、参照中などの矛盾は 409。ファイル取込の行ごとのエラーは `rows`（`line`・`message`）で返す

## 施策 API

| リソース       | API                                                                                     |
| -------------- | --------------------------------------------------------------------------------------- |
| 施策           | `GET/POST /api/activities`、`GET/PUT/DELETE /api/activities/{id}`                        |
| 外部コード     | `POST /api/activities/{id}/external-codes`、`DELETE /api/activities/{id}/external-codes/{eid}` |
| 実績の明細     | `GET /api/activities/{id}/actual-entries?fiscal_year=&month=&subject_id=&offset=&limit=`（既定はすべての月。新しい月から `limit`（既定 100、最大 500）件。件数 `total`・続き `has_more`・明細の合計と実績データの合計は絞り込んだ全件。`hide_details` の会計科目は、FP&A 以外には会計科目 × 科目の合計 `hidden` だけを返す）、`GET .../actual-entries/export`（同じ絞り込みの全件を CSV で。`hide_details` は月 × 会計科目 × 科目の合計の行） |
| マイルストーン | `POST /api/activities/{id}/milestones`、`PUT/DELETE /api/activities/{id}/milestones/{mid}` |
| ドライバー定義 | `POST /api/activities/{id}/drivers`、`PUT/DELETE /api/activities/{id}/drivers/{did}`     |
| 重点施策       | `PUT /api/activities/{id}/priority`（`is_priority`。施策の作成・削除の権限が必要。変更履歴に残す） |
| ウォッチ       | `PUT/DELETE /api/activities/{id}/watch`（ログインユーザー本人の印。施策の一覧・詳細に `is_watched` を返す） |
| 金額の内訳     | `POST /api/activities/{id}/lines`、`PUT/DELETE /api/activities/{id}/lines/{lid}`（施策 × 科目に複数。計算式で反映するか・確度の段階（`confidence_level`、空なら施策の段階）・見通しの種類（`outlook`: base / addon / downside）を内訳ごとに設定。計算式・反映の有無・段階・見通しの種類の変更は理由必須） |

- 一覧は `unit_id` / `owner_user_id` / `activity_type` / `status` / `q`（コード・名称の部分一致）で絞り込める
- 詳細（`GET /api/activities/{id}`）はマイルストーン・ドライバー・内訳を含む。各施策に `can_edit`（ログインユーザーが編集できるか）を付ける
- 権限は施策ごとに判定する（docs/plan.md「4. ロール」）
- 値の形式
  - 施策コード: 半角英数字・`-`・`_`、50文字以内、全体で一意。作成時に空欄なら `ACT-0001` 形式で自動採番
  - 外部コード: 空白・カンマ・引用符を除く100文字以内、全体で一意。施策コードと同じ値は不可
  - ステータス: `planned` / `in_progress` / `completed` / `on_hold` / `cancelled`
  - 確度: 確度の段階のコード（`confidence_level`、例: `C`）。日付は `YYYY-MM-DD`。プロジェクト型は開始日・終了日が必須
  - ドライバー code: 英小文字で始まる英小文字・数字・`_`、施策内で一意
  - マイルストーンのステータス: `not_started` / `in_progress` / `completed` / `delayed`
- 計算式は `api/src/formula.rs` で解析・評価する（有理数による誤差のない計算、四捨五入は `round_half_up`）。登録時に構文と、未定義のドライバーを使っていないかを検証する
- 内訳の計算式（反映しない式を含む）で使われているドライバーは、code の変更・削除ができない。値が登録済みのドライバーも削除できない
- 金額・ドライバー値・シナリオ条件がある施策は削除できない（409）。削除できる場合は、マイルストーン・ドライバー・内訳も合わせて削除し、それぞれ監査ログに残す

## シナリオ・数値入力 API

| API                                                           | 内容                                         | 権限           |
| ------------------------------------------------------------- | -------------------------------------------- | -------------- |
| `GET /api/scenarios`（`fiscal_year` で絞り込み）              | 一覧（エイリアス・決算確定月・作成中を含む） | 全員           |
| `GET /api/scenarios/active`                                   | 作成中のシナリオ（未設定なら `null`）        | 全員           |
| `POST /api/scenarios`                                         | 作成（`base_scenario_id` 指定で複製。`plan_role`・`actual_through`・`previous_scenario_id`（前回見込、省略時は複製元）も指定できる） | FP&A           |
| `GET/PUT /api/scenarios/{id}`                                 | 取得・名称、エイリアス（`plan_role`）、前回見込（`previous_scenario_id`）、決算確定月（`actual_through`、変更は理由必須）、現場の更新の締切日（`update_deadline`、`YYYY-MM-DD`）の変更 | 全員 / FP&A    |
| `POST /api/scenarios/{id}/activate`                           | 作成中に指定（前の作成中は外れる）           | FP&A           |
| `POST /api/scenarios/{id}/lock`、`/unlock`                    | ロック（決算確定月以前の実績を `scenario_actuals` に保存し、作成中なら外す）・ロック解除（理由必須。保存した実績を外す） | FP&A           |
| `GET /api/scenarios/actual-drift`                             | ロック済みのシナリオごとに、保存した実績と今の実績が食い違う月（月・収益の差・費用の差）。食い違いのないシナリオは返さない（docs/plan.md「2.14」） | FP&A           |
| `POST /api/scenarios/{id}/refresh-actuals`                    | ロック済みのシナリオに保存した実績を、今の実績に入れ替える（理由必須。`dry_run: true` で月ごとの差だけを返す）。ロックは外れない | FP&A           |
| `GET /api/scenarios/{id}/activities/{aid}`                    | 施策の月別のドライバー値・金額・想定条件     | 全員           |
| `PUT .../activities/{aid}/driver-values`                      | ドライバー値の一括登録・更新・削除（理由必須） | 施策の編集権限 |
| `PUT .../activities/{aid}/amounts`                            | 金額の直接入力（理由必須）                   | 施策の編集権限 |
| `PUT .../activities/{aid}/condition`                          | 廃止（想定条件は今回の見込の説明に統合。docs/plan.md「2.18」）。410 を返す | － |
| `GET/PUT .../activities/{aid}/note`                           | 差異の説明・要因の分類（docs/plan.md「2.10」）  | 参照は全員、更新は数値の入力と同じ |
| `GET/POST .../activities/{aid}/comments`、`DELETE .../activities/{aid}/comments/{cid}` | 説明へのコメント（docs/plan.md「2.24」）。書いた順に返し、削除したものは本文を返さない（`deleted: true`） | 参照・投稿は全員（ロック済みのシナリオには投稿できない）、削除は書いた本人だけ |
| `POST/DELETE .../activities/{aid}/complete`                   | 更新を完了にする・完了を取り消す             | 数値の入力と同じ |
| `GET /api/scenarios/{id}/activity-status?scope=mine\|units\|all` | ホーム用。施策ごとの状態・差異の説明・要因の分類、重点施策・ウォッチ、今回・基準・期初計画・修正計画・前回見込の年間の収益・費用、新しく実績になった月の前回見込との差、コメントの件数（`comment_count`） | 全員（範囲はロールで決まる） |
| `GET /api/scenarios/{id}/milestones?activity_ids=` | ホームのマイルストーン（docs/plan.md「2.11」）。完了していないマイルストーンと、期日超過・遅延・期日が近い・後ろ倒し（前回見込の作成以降の回数と日数） | 全員 |

- 月は `YYYY-MM`。値を `null` にすると削除。1リクエスト1,000件まで
- ドライバー値は小数点以下6桁まで、金額は円単位の整数（マイナス可）
- 変更理由（`reason`）: 作成中のシナリオでは省略でき、省略すると「<シナリオ名>の見込更新」を記録する。それ以外のシナリオは必須（docs/plan.md「2.18」）
- 「仮の値」（`is_provisional: true`）には理由（`provisional_reason`）が必須
- 更新 API のレスポンスは、更新後の `GET .../activities/{aid}` と同じ形
- 金額は、決算確定月以前の月は実績（ロック済みなら `scenario_actuals`、それ以外は `actual_facts`）、それより後の月は計画値（`budget_facts`）。月ごとに実績かどうか（`actual_months`）を返す
- 入力できるのは、ロックされていないシナリオの決算確定月より後の月で、作成中のシナリオは施策の編集権限、それ以外は FP&A のみ。それ以外は 409。入力の可否はレスポンスの `editable` で分かる
- エイリアスを付けると、同じ年度で同じエイリアスを持つシナリオからは外れる
- 金額の再計算は `api/src/calc.rs` が行い、変更した金額は同じ変更セットの監査ログに残す

## シナリオの切り替え API

シナリオの切り替えの仕様は docs/plan.md「2.16」。すべて FP&A のみ。

| API | 内容 |
| --- | ---- |
| `POST /api/scenarios/start-monthly` | 月次の見込を始める（`name`・`actual_through`・`update_deadline`、`reason` 任意） |
| `POST /api/scenarios/start-fiscal-year` | 新年度の期初計画を始める（`fiscal_year`・`name`・`update_deadline`） |

- どちらも `dry_run: true` で保存せず、変わる内容（ロックする版、複製元、新しい版、付け替えるエイリアス、前回見込）と注意（`warnings`: 種類と件数・月）を返す。注意があっても実行できる
- 実行は1つの変更セット（全部か無しか）。ロック・複製・エイリアスの付け替え・作成中の指定は、それぞれの API と同じ処理（`scenario` パッケージ）を使い、監査ログに残す
- 締切が入っていれば、保存の後に「更新の開始」を送る（`notify`）
- レスポンスは新しい版（`Scenario`）と、ロックした版の ID

## 組織変更の予約 API

組織変更と異動の仕様は docs/plan.md「2.15」。すべて FP&A のみ。

| API | 内容 |
| --- | ---- |
| `GET /api/org-change-plans` | 予約の一覧（状態・有効日・変更の件数） |
| `POST /api/org-change-plans`、`GET/PUT/DELETE /api/org-change-plans/{id}` | 予約の作成・取得・変更（名前・有効日・変更の一覧 `items` をまとめて送る）・削除（予約中・失敗のみ） |
| `POST /api/org-change-plans/{id}/apply` | 今すぐ適用する（予約中・失敗のみ。失敗なら 422 で理由を返し、予約を失敗にする） |
| `POST /api/org-change-plans/{id}/cancel` | 取り消す（予約中のみ） |

- 変更（`items[]`）の種類: `move_activity`（`activity_id`・`target_unit_id`）、`move_unit`（`unit_id`・`segment_id`・`organization_id`）、`merge_unit`（`unit_id`・`target_unit_id`）、`change_owner`（`activity_id` または `unit_id` と `owner_user_id`、null で未設定）
- 適用: 予約を作った FP&A の変更セットとして、1トランザクションで変更を順に適用する。検証はマスタ・施策の更新と同じ（末端ノード、廃止したユニット、無効なユーザーなど）。1つでもエラーなら全体をロールバックし、予約を失敗（`error` に理由）にして FP&A にお知らせを送る
- 自動の適用: 通知と同じく API 内の tokio のタスク（1分ごと）で、有効日が今日以前の予約中の予約を適用する（`api/src/orgchange/`）。予約の行ロックで二重の適用を防ぐ
- 予約の作成・変更のときに、今の状態で適用できるかを確かめる（適用してロールバックする）。適用できない予約は 422（`details.items` に何件目の変更か）
- ユニットの廃止（`/archive`）は施策が所属していないときだけ。施策があるときは統合を使う

## 年度の締め API

| API | 内容 | 権限 |
| --- | ---- | ---- |
| `GET /api/fiscal-years/closings` | 締めた年度の一覧（年度・締めた日時・人） | 全員 |
| `POST /api/fiscal-years/{fy}/close` | 年度を締める（理由は任意） | FP&A |
| `POST /api/fiscal-years/{fy}/reopen` | 締めを解除する（理由必須） | FP&A |

- 締めた年度の月は、実績の取込（422、行エラー）・再割当（422）ができない。未割当の割当は、ルールを登録し、締めた年度の行は割り当てない（結果に割り当てなかった行数 `skipped_closed` を返す）
- 締め・解除は変更セットと監査ログ（`fiscal_year_closings`）に残す

## 実績取込 API

`POST /api/actuals/import`（FP&A のみ）

- `multipart/form-data` で `file`（CSV）と `reason`（変更理由）を送る。形式は docs/plan.md「6.1 実績 CSV の形式（会計の明細）」
- 各行を docs/plan.md「2.12」の順番（施策コード・外部コード → 割当ルール → 未割当）で施策に割り当て、明細（`actual_entries`）を対象月ごとに置き換え、合計（`actual_facts`）を差分で更新する。シナリオは指定しない
- `?dry_run=true` を付けると、検証と集計だけを行い保存しない（理由は不要）
- レスポンス: 取り込んだ月、データ行数、対象外の行数、割当の根拠ごと（`activity_code` / `external_code` / `rule` / `unallocated`）の件数と金額、合計の追加・更新・削除・変更なしの件数、月別の収益・費用の合計（会計システムとの突合用。未割当を含む）、ロック済みのシナリオと食い違う月（`locked_drift`: シナリオごとの月・収益の差・費用の差）
- CSV にエラーがあれば 422（`code: invalid_csv`）で、`error.rows` に行番号とメッセージを返す（最大100件）。未登録の会計科目は、コードごとに1件（最初の行番号と行数）にまとめて返す。1件もエラーがなければ保存する
- `GET /api/actuals/months?fiscal_year=`: 実績を取り込み済みの月（決算確定月の既定値に使う）

| API | 内容 | 権限 |
| --- | ---- | ---- |
| `GET /api/actuals/unallocated?fiscal_year=` | 未割当の一覧。箱の ID がある行は箱の ID ごと、ない行は会計科目 × 部門ごとにまとめ、月・件数・金額・摘要の例を返す | FP&A |
| `POST /api/actuals/unallocated/assign` | 未割当のまとまり（`box_code`、または `gl_account_id` と `department_code`）を施策（`activity_id`）に割り当てる。箱の ID は外部コードとして、会計科目 × 部門は割当ルールとして登録し、同じまとまりの未割当の行（すべての月）を割り当てる。`all_departments: true` なら部門を問わないルールにする（部門のない行のまとまりも全部門のルールになる）。ルールで割り当てるときは、割当の順番に合わせ、その会計科目（・部門）の未割当の行を箱の ID の有無を問わず割り当てる。登録済みの箱の ID・ルールは 409（再割当で当て直す）。`reason` 必須 | FP&A |
| `POST /api/actuals/reallocate` | 指定した月（`months`）の明細に、今の外部コード・割当ルールを当て直す。`reason` 必須。`dry_run: true` で施策ごとの増減だけを返す | FP&A |

- 明細の行は変更セットに紐づけて保存し、行ごとの監査ログは残さない。監査ログには合計（`actual_facts`）の変更と、外部コード・割当ルールの追加を残す
- 未割当は `actual_facts.activity_id` が NULL の行として持つ。ロック時の `scenario_actuals` にも含める

## 通知 API

締切と通知の仕様は docs/plan.md「2.13」。

| API | 内容 | 権限 |
| --- | ---- | ---- |
| `GET /api/notifications?limit=` | 自分宛てのお知らせ（新しい順、既定 30 件）と未読の数（`unread`） | 全員 |
| `POST /api/notifications/{id}/read`、`POST /api/notifications/read-all` | 既読にする | 全員（自分宛てのみ） |
| `GET/PUT /api/notification-settings` | 通知の設定（種類ごとの有効・無効 `enabled_kinds`、締切の前の日数 `reminder_days`、送信時刻 `send_time`）。Slack が設定されているか（`slack_configured`）も返す | 参照は全員、更新は FP&A |
| `POST /api/notification-settings/test` | Slack にテスト送信する | FP&A |
| `GET /api/notification-runs?limit=` | 送信の記録（日時・種類・シナリオ・宛先の数・Slack の結果） | FP&A |
| `POST /api/scenarios/{id}/reminders` | 催促（`user_id`）。作成中のシナリオの、その人の未完了の施策の一覧を、お知らせと Slack で送る（docs/plan.md「2.20」）。今日すでに送っていれば 409、未完了の施策がなければ 422 | FP&A |
| `GET /api/scenarios/{id}/reminders` | 今日（日本時間）催促した人（`user_ids`） | FP&A |

- コメントのお知らせ（`comment`）は、投稿の API の中で宛先（担当者、いなければユニットのマネージャーと、そのやり取りにすでに書いた人）ごとに作る。Slack には送らず、送信の記録（`notification_runs`）にも残さない
- 通知の組み立てと送信は `api/src/notify/`。宛先の決定（未完了の施策・担当者・マネージャー）はホームの更新の状態（`activity-status`）と同じ判定を使う
- 定期の送信: API の起動時に tokio のタスクを1つ起動し、1分ごとに送信時刻を過ぎたかを確かめる。送信の記録（`notification_runs`）の一意制約で、同じ通知を同じ日に二重に送らない（複数台・再起動でも安全）
- 即時の通知: 締切の設定（「更新の開始」）と決算確定月の変更（「実績の反映」）は、保存のトランザクションが終わった後に送る。送信の失敗は保存を失敗させない
- Slack: 環境変数 `SLACK_WEBHOOK_URL`（Incoming Webhook）に `reqwest` で JSON を POST する。本文のリンクには環境変数 `APP_BASE_URL`（アプリの URL。compose の既定は `http://localhost:8080`）を使う。未設定なら送らない。メンションは `<@メンバーID>`。タイムアウトは 10 秒。失敗は `notification_runs` に記録し、同じ日のうちに最大3回まで送り直す
- 時刻は日本時間（`Asia/Tokyo`）で判定する
- FP&A のホームの「今月の作業」（docs/plan.md「2.20」）は、既存の API（`activity-status`・`actuals/months`・`actuals/unallocated`・`scenarios`・`scenarios/actual-drift`）から画面で組み立てる。集計用の API は追加しない

## 予実比較 API

`GET /api/reports/comparison`（ログインユーザー全員）

- `scenario_ids`: 比較するシナリオ（カンマ区切り、最大4つ）。先頭が差異の基準
- 各シナリオの金額は、決算確定月以前は実績、それより後は計画値（着地見込の指定は廃止）
- `include_actual=true`: 実績データ（取込済みの月）を系列に加える
- `unit_id`: 指定するとそのユニットの施策ごと、指定しなければユニットごとに集計する。ユニットを指定しないときは、未割当の実績を `unit_id: null` の行として返す（全社の合計を会計と一致させるため）
- `measure`: `full`（満額、既定）/ `weighted`（加重見込）/ `optimistic`（楽観）/ `pessimistic`（悲観）。確度の段階と見通しの種類による算出は docs/plan.md「2.8」。実績の月はどれも実績の金額
- レスポンス: 系列（`series`）と、ユニット（または施策）× 科目 × 月の金額（`rows[].values` に系列ごとの金額を文字列で）
- すべての系列は同じ年度のシナリオであること
- セグメント・組織の階層での集計、収益・費用・利益の計算（利益 = 収益 − 費用）、差異の計算は画面側（`web/src/lib/aggregate.ts`、BigInt で計算）で行う

## 報告資料 API（docs/plan.md「2.23」）

`GET /api/reports/pack.xlsx`（ログインユーザー全員）

- 条件は予実比較と同じ（`scenario_ids`・`include_actual`・`measure`）に、`grain`（`month` / `quarter` / `half` / `year`）、範囲（`segment_id` / `organization_id` / `unit_id`）、シート（`sheets=pl,units,notes`）、ユニット別 P/L に施策を含めるか（`activities=true`）を加える
- 金額は予実比較と同じ集計（`api/src/report/` の読み込み）を使い、科目体系・組織の階層の合計、利益、差をサーバー側で計算する（`api/src/report/` の `pack.rs`・`pack_sheets.rs`。整数）。画面の集計（`web/src/lib/aggregate.ts`）と同じ結果になることをテストで確かめる
- Excel は `api/src/xlsx.rs` が書く（ZIP は `zip` crate、中の XML は Office Open XML の最小限: ワークブック・シート・スタイル。文字列はセルに直接入れる）。Excel のライブラリは使わない。シート名に `/` は使えないため、「全社 P／L」のように全角にする
- レスポンスのファイル名は `filename*`（UTF-8）で `報告資料_<年度>_<日付>.xlsx`、`filename` は ASCII の `report_<年度>_<日付>.xlsx`
- 閲覧制限のある科目は、予実比較と同じく見られる人にだけ含める
- 図の画像（PNG）は画面で作る（`web/src/lib/domImage.ts`: 要素を複製して計算済みのスタイルを書き込み、SVG の foreignObject に入れて canvas に描く。`data-no-capture` の要素は除く）。API は使わない

## 閲覧制限のある科目（docs/plan.md「2.17」）

- FP&A と経営陣・レビュアー以外（`manager`・`member`）には、`subjects.is_restricted` の科目の金額を返さない。合計・差異・利益なども、その科目を除いて計算する
- 金額を返す API（数値入力、施策の P/L、予実比較、リスク、ホーム、計画値の CSV、実績の明細・未割当、変更履歴）は、ユーザーのロールから「見られる科目」の条件を作る共通処理（`api/src/visibility.rs`）を通して読む
- 制限のある科目を除いたレスポンスには `restricted_hidden: true` を付け、画面は「閲覧制限のある科目を除いた金額です」と表示する
- 制限のある科目の内訳の作成・金額の入力は FP&A のみ（それ以外は 403、計画値の CSV では行のエラー）

## CSV インポート・エクスポート API

| API | 内容 | 権限 |
| --- | ---- | ---- |
| `GET /api/{organizations,segments,units,subjects,gl-accounts,allocation-rules,users,activities}/export` | CSV（BOM 付き UTF-8）をダウンロード | 全員 |
| `POST /api/{organizations,segments,units,subjects,gl-accounts,allocation-rules,users,activities}/import` | CSV を取込（`multipart/form-data` の `file` と `reason`、`?dry_run=true` で確認のみ） | FP&A |

- 形式と取込のルールは docs/plan.md「6.2」
- 省略できる列（ユーザーの `slack_user_id`）は `csvio.ParseWithOptional` で読む
- 取込の結果の `warnings` に注意を返す（ユーザーの取込では、担当している施策・所管ユニットが残っている無効なユーザー）
- 共通処理（アップロードの読込、ヘッダーと行の検証、行エラー、結果、CSV の書き出し）は `api/src/csvio.rs`
- 結果: `{"dry_run", "rows", "inserted", "updated", "unchanged"}`。エラーは 422（`code: invalid_csv`、`error.rows` に行番号とメッセージ）

### 計画値の CSV（docs/plan.md「6.3」）

| API | 内容 | 権限 |
| --- | ---- | ---- |
| `GET /api/scenarios/{id}/amounts/export?unit_id=` | 金額の CSV（施策 × 科目 × 内訳、月を横に12列）をダウンロード | 全員 |
| `GET /api/scenarios/{id}/driver-values/export?unit_id=` | ドライバー値の CSV（施策 × ドライバー、月を横に12列）をダウンロード | 全員 |
| `POST /api/scenarios/{id}/plan-values/import` | 金額かドライバー値の CSV を取込（ヘッダーで判定。`multipart/form-data` の `file` と `reason`、`?dry_run=true` で確認のみ） | 数値の入力と同じ（施策ごとに判定） |

- 取込の結果: `{"dry_run", "kind": "amounts"|"driver_values", "rows", "inserted", "updated", "deleted", "unchanged", "activities": [{"activity_id", "code", "name", "changed"}], "warnings": [{"kind": "actual_month"|"formula_line", "count"}]}`（件数はセルの数）
- 値の書き込みは、数値入力 API（`PUT .../amounts`・`.../driver-values`）と同じ処理を使う（権限・入力できる月の判定、監査ログ、`api/src/calc.rs` の再計算、仮の値、更新の状態）

## リスク API

`GET /api/reports/risk?scenario_id=&compare_id=&period=`（ログインユーザー全員）。画面の仕様は docs/plan.md「2.9 リスク画面」。

- `scenario_id`: 基準シナリオ（必須）。`compare_id`: 比較シナリオ（任意、同じ年度）。`period`: `year`（通期、既定）/ `remaining`（決算確定月より後の月のみ）
- 施策ごとに次を返す
  - 確度の段階・前提条件・担当者・ユニット
  - 楽観・基準（加重見込）・悲観・満額の、期間の収益・費用（docs/plan.md「2.8」）
  - 段階別・見通しの種類別の売上（実績の月は「実績」として別に返す）
  - 内訳ごとの段階・見通しの種類・金額
  - 比較シナリオの加重見込の収益・費用
  - 月別・段階別の売上（`revenue_by_month`。施策の一覧の「確度の推移」で使う）
  - 警告: マイルストーンの遅れ（`overdue` / `delayed`、注意として `upcoming`）、後ろ倒し（回数・日数）、下方修正（幅）、連続の下方修正、当たり具合（差の率）。しきい値は docs/plan.md「2.8」の固定値
  - 今回の見込の説明（基準・比較）
  - 基準シナリオの説明へのコメントの件数（`comment_count`、docs/plan.md「2.24」）
- マイルストーンの日付は日本時間
- セグメント・組織での集計、並べ替えは画面側で行う

## 変更履歴 API

| API                         | 内容                                                                                   |
| --------------------------- | -------------------------------------------------------------------------------------- |
| `GET /api/change-sets`      | 変更セットの一覧（新しい順）。`scenario_id` / `activity_id` / `user_id` / `reason`（with・without）/ `from`・`to`（YYYY-MM-DD）で絞り込み、`before_id` と `limit` で続きを取得 |
| `GET /api/change-sets/{id}` | 変更セットと監査ログ（変更前後の値と、対象を人が読める形にした `label`）                |

- 参照は FP&A と経営陣・レビュアー（`viewer`）。現場マネージャー・現場担当は、`activity_id` で自分が編集できる施策に絞り込んだときだけ参照できる（それ以外は 403）。その場合、制限のある科目（`subjects.is_restricted`）の金額の監査ログは返さない（docs/plan.md「2.17」）
- 一覧の各行は、テーブル別の件数と、関係する施策（最大5件）を含む
- `activity_id` の絞り込みは、施策自体・施策に紐づくレコード（`activity_id` を持つもの）・ドライバー値（ドライバー経由）の変更を対象にする
- 変更のなかった操作（監査ログが0件の変更セット）は一覧に出さない

## フロントエンド（React）

- React + TypeScript + TailwindCSS v4、ビルドは Vite。依存ライブラリは React と Tailwind のみ
- 構成
  - `src/api/`: API クライアント（`client.ts`）と型（`types.ts`）。エラーは `ApiError`（`code`・`details`・`rows`）として投げる
  - `src/lib/`: ルーター（History API を使った最小実装）、認証（`AuthProvider`）、`useApi`（取得と再取得）、書式
  - `src/components/`: 共通 UI（ボタン・入力・表・ダイアログなど）、レイアウト、確認ダイアログ、変更理由ダイアログ
  - `src/pages/`: 画面
- 画面遷移は `lib/router.tsx` の `Link` / `navigate` を使う。ルートは `App.tsx` に定義する
- 画面は開いたときに読み込む（`App.tsx` の `page()`、`React.lazy`）。最初に読み込むのはログイン・ホーム・レイアウトだけ（約 274KB、gzip 85KB）。施策の一覧・詳細はログインの後、手が空いたときに先読みする。施策の一覧の図（`ActivityCharts`）も図を開いたときに読み込む。新しい画面を足すときも `page()` で登録する
- 施策の画面は `/activities/:id`（`?tab=update|overview|settings&scenario=:sid`。docs/plan.md「2.19」）。旧 URL `/scenarios/:sid/activities/:aid` は `App.tsx` で転送する。API は変えない（数値は `GET/PUT /api/scenarios/{id}/activities/{aid}...`、施策の情報は `/api/activities/{id}...`）
- 使い方（docs/plan.md「2.21」）: 手引きの Markdown（`src/manual/*.md`）を Vite の `?raw` で読み込み、`lib/markdown.ts`（見出し・段落・箇条書き・表・強調・コード・リンク・画像だけの小さな変換。外部ライブラリは使わない）で表示する。「使い方」の画面は `React.lazy` で開いたときだけ読み込む。画像は `public/manual/`、撮り直しは `make manual-screenshots`（Playwright のスクリプト `e2e/screenshots/`）
- 変更理由: `useReason().withReason(op)` で操作を実行すると、API が「変更理由が必要」（422 `details.reason`）を返したときに理由の入力ダイアログを出して再実行する
- 権限による表示の切り替え（編集ボタンを出すかなど）は画面で行うが、最終的な判定は API が行う
- テスト: ロジックは `node:test`（`src/**/*.test.ts`、Node の型ストリップで .ts をそのまま実行）、主要な操作の流れは Playwright の E2E（`e2e/`）。E2E は `scripts/e2e.sh` が専用の compose スタックを起動して実行する

## ディレクトリ構成（案）

```
.
├── compose.yaml
├── nginx/
│   └── default.conf
├── api/                  # Rust
│   ├── src/bin/          # コマンド（server・migrate・createuser・perfseed）
│   ├── src/              # ドメイン・ハンドラー（機能ごとのモジュール）
│   ├── tests/            # DB を使う統合テスト
│   └── migrations/       # SQL マイグレーション
├── web/                  # React + TailwindCSS
│   └── src/
└── docs/
    ├── plan.md           # 要件・ドメイン設計
    └── architecture.md   # 本ドキュメント
```
