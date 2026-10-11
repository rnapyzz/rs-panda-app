# F-Panda

[![CI](https://github.com/rnapyzz/rs-panda-app/actions/workflows/ci.yml/badge.svg)](https://github.com/rnapyzz/rs-panda-app/actions/workflows/ci.yml)

活動（施策）ベースの予実管理・ローリングフォアキャスト支援アプリ。

## 構成

| ディレクトリ | 内容                                        |
| ------------ | ------------------------------------------- |
| `api/`       | Rust の API サーバーとマイグレーション      |
| `web/`       | React + TailwindCSS（Vite）                 |
| `nginx/`     | リバースプロキシ設定                        |

## 開発環境

必要なもの: Docker（Docker Compose v2）

```bash
cp .env.example .env   # 任意。未作成でもデフォルト値で起動する
make up                # docker compose up --build -d
```

- アプリ: http://localhost:8080
- API: http://localhost:8080/api/health
- MySQL: `localhost:3307`（ユーザー / パスワード: `fpanda` / `fpanda`）

起動時に `migrate` コンテナが未適用のマイグレーションを自動で適用する。

API をコンテナの外で動かす・テストするには、Rust（`api/rust-toolchain.toml` のバージョン。`rustup` が自動で入れる）が必要。

### 最初のユーザーを作成する

```bash
make create-user EMAIL=admin@example.com NAME=管理者 ROLE=fpa_admin
```

パスワード（12文字以上）の入力を求められる。`ROLE` を省略すると `fpa_admin` になる。

### 画面

| URL                                   | 画面                                                         |
| ------------------------------------- | ------------------------------------------------------------ |
| `/activities`                         | 施策一覧                                                     |
| `/activities/{id}`                    | 施策の詳細（マイルストーン・ドライバー・計算式）             |
| `/scenarios`                          | シナリオ一覧・作成（複製）                                   |
| `/scenarios/{id}`                     | シナリオの詳細（ロック・実績 CSV 取込・施策の一覧）          |
| `/scenarios/{id}/activities/{aid}`    | 数値の入力（月別のドライバー値・金額・想定条件）             |
| `/reports`                            | 予実比較（シナリオ・着地見込の比較、階層でのドリルダウン）   |
| `/risks`                              | リスク（確度・仮の値・楽観/悲観の振れ幅・マイルストーン）   |
| `/history`                            | 変更履歴（誰が・いつ・なぜ・何を変えたか）                   |
| `/masters/...`                        | マスタ管理（組織・セグメント・ユニット・勘定科目・ユーザー）     |

### よく使うコマンド

| コマンド              | 内容                                   |
| --------------------- | -------------------------------------- |
| `make up`             | 起動（ビルドを含む）                    |
| `make down`           | 停止                                   |
| `make logs`           | ログを表示                             |
| `make migrate`        | マイグレーションを適用                  |
| `make migrate-status` | マイグレーションの適用状況を表示        |
| `make reset-db`       | DB のデータを削除して作り直す           |
| `make create-user`    | ユーザーを作成                          |
| `make test`           | API のテスト、フロントの lint・ビルド・単体テスト |
| `make test-api`       | API の fmt・clippy・テスト（DB を使うテストを含む） |
| `make test-web`       | フロントの lint・ビルド・単体テスト     |
| `make e2e`            | E2E テスト（専用のスタックで実行）      |

### マイグレーション

`api/migrations/` に `<4桁の連番>_<説明>.sql` の形式で追加する。適用済みのファイルは変更せず、変更は新しいファイルで行う。

## CI

GitHub Actions（`.github/workflows/ci.yml`）で、PR と main への push ごとに以下を実行する。

| ジョブ       | 内容                                                                 |
| ------------ | -------------------------------------------------------------------- |
| `api`        | `cargo fmt --check`・`cargo clippy`（警告はエラー）、MySQL 8.4 に対してマイグレーションを適用・再実行で変更がないこと、`cargo test`（単体・DB を使う統合テスト） |
| `web`        | `npm run lint`・`npm run build`（型チェックを含む）・`npm test`      |
| `e2e`        | 専用のスタックを起動し、Playwright で E2E テスト（失敗時はレポートを保存） |

### テスト

DB を使うテストは、環境変数 `TEST_DB_HOST` が設定されているときだけ実行される（未設定ならスキップ）。テストごとに一時的なデータベースを作成・削除するため、DB を作成できるユーザー（root）で接続する。`make test-api` は compose の db に接続して実行する。

### 画面のテスト

- **単体テスト**（`web/src/**/*.test.ts`）: 集計・書式・ルーター・階層などのロジックを Node 標準の `node:test` で実行する（`npm test`）。追加の依存なし
- **E2E テスト**（`web/e2e/*.spec.ts`）: Playwright で、ログイン・施策の作成と変更理由・数値入力と計算式・実績 CSV 取込・予実比較などの主要な流れを実際のブラウザで確認する
  - `make e2e`（= `scripts/e2e.sh`）は、開発環境とは別のスタック（プロジェクト名 `fpanda-e2e`、http://localhost:18080）を起動し、テスト用の管理者を作ってからテストを実行し、終わったらデータごと削除する。開発用の DB には影響しない
  - `KEEP=1 scripts/e2e.sh` でテスト後もスタックを残す。`scripts/e2e.sh --headed values.spec.ts` のように Playwright の引数を渡せる
  - 初回は `cd web && npx playwright install chromium` でブラウザを入れる
