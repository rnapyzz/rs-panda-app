# CLAUDE.md

## プロジェクト概要

活動（施策）ベースの予実管理・ローリングフォアキャストを支援する FP&A 向けアプリ。

- 要件・ドメイン設計: [docs/plan.md](docs/plan.md)
- アーキテクチャ・技術スタック: [docs/architecture.md](docs/architecture.md)
- 運用上の課題（俯瞰チェック）: [docs/issues.md](docs/issues.md)
- 利用者向けの手引き（アプリの「使い方」）: [web/src/manual/](web/src/manual/)
- 本番環境の構築・リリース・復旧の手順（AWS）: [docs/deploy.md](docs/deploy.md)
- 速さの計測の結果と測り方: [docs/performance.md](docs/performance.md)
- 研修・試用の環境（デモのデータ）: [docs/demo.md](docs/demo.md)

## 技術スタック

nginx + React.js + TailwindCSS + Rust（axum）+ MySQL 8。すべて Docker コンテナで動かす。

## 用語（画面とコードの対応）

| 画面での呼び方 | コード・DB・API での名前 |
| -------------- | ------------------------ |
| 施策           | `activities`             |
| ユニット       | `units`（`unit_id`）。旧称「機能」（`functions` / `function_id`）。改称前の監査ログには旧名が残っている |
| セグメント / 組織 | `segments` / `organizations` |
| シナリオ       | `scenarios`              |
| 変更セット / 監査ログ | `change_sets` / `audit_logs` |

画面の文言では「ユニット」を使い、「機能」とは書かない。
画面では現場向けの言葉を使う（docs/plan.md「2.18」）: 作成中のシナリオ →「今回の見込」、基準 →「目標」、前回見込 →「前回の見込」、決算確定月 →「実績の月」、リスクの基準 →「加重見込」。画面で「基準」とは書かない。

## よく使うコマンド

- 起動: `make up`（http://localhost:8080）／停止: `make down`
- マイグレーション: `make migrate`、`make migrate-status`、DB 作り直し: `make reset-db`
- テスト: `make test`（API の fmt・clippy・テスト、フロントの lint・ビルド・単体テスト）、E2E: `make e2e`（専用スタックで Playwright）、速さの計測: `make perf`、研修・試用の環境: `make demo`（本番に近い量のデータ。集計の仕組みを変えたら測り直す）、開発環境をデモのデータで作り直す: `make dev-demo`（開発環境のデータはすべて消える）
- 画面の変更では、利用者向けの手引き（`web/src/manual/`。docs/plan.md「2.21」）も同じ PR で更新する。手引きの画像に写る画面を変えたら `make manual-screenshots` で撮り直す
- 画面の変更では、関係する E2E（`web/e2e/`）も更新する。E2E のデータは実行ごとに一意な名前で作る（`uniq()`）
- マイグレーションは `api/migrations/` に `<4桁の連番>_<説明>.sql` で追加し、適用済みファイルは変更しない

## 開発フロー

作業ごとに main からブランチを切り、コミット → プッシュ → PR 作成まで行う。

## 開発方針

- **依存は定番の crate に限り、最小限にする。** API は `tokio`・`axum`・`mysql_async`・`serde`・`tracing` などを使う（一覧は `api/Cargo.toml`、方針は docs/architecture.md）。新しい crate の追加が必要なときは理由を示して確認をとる。OIDC と Excel の書き出しはライブラリを使わずに実装している。
- API の変更では `cargo fmt`・`cargo clippy --all-targets -- -D warnings`・`cargo test` を通す。Rust のバージョンは `api/rust-toolchain.toml` で固定している。
- 金額・ドライバー値は `DECIMAL` で扱い、浮動小数点で計算しない（API では整数・有理数で計算する）。
- 年月（`target_month`）は月初日の `DATE` で保持する。
- 画面・図・Excel の金額のマイナスは「▲1,000」で表示する（`web/src/lib/format.ts` の `formatYen`・`formatSignedYen` を使う）。CSV は「-」のまま。
- 確認・報告の画面の金額は、画面ごとの「金額の単位」（百万円が既定・千円・円）で表示する。`AmountUnitSwitch` と `AmountUnitProvider` を置き、部品は `useAmountUnit()` の `fmt`・`signed` を使う。数値の入力・明細・変更履歴は円のまま（docs/plan.md「7. 前提・決定事項」）。
- 値の変更は変更セット（`change_sets`）単位で行い、変更理由を必須にする。変更前後の値は `audit_logs` に残す。
- ロック済みシナリオと、シナリオの決算確定月以前の月（実績）は画面から編集できないようにする。現場が入力できるのは作成中のシナリオだけ。
- ドキュメント・UI 文言は日本語で書く。
