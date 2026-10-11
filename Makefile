SHELL := /bin/bash

.PHONY: up down logs migrate migrate-status create-user test test-api test-web e2e manual-screenshots perf demo demo-down demo-reset dev-demo reset-db

up: ## 開発環境を起動する
	docker compose up --build -d

down: ## 開発環境を停止する
	docker compose down

logs: ## ログを表示する
	docker compose logs -f

migrate: ## マイグレーションを適用する
	docker compose run --rm migrate migrate up

migrate-status: ## マイグレーションの適用状況を表示する
	docker compose run --rm migrate migrate status

create-user: ## ユーザーを作成する（例: make create-user EMAIL=admin@example.com NAME=管理者 ROLE=fpa_admin）
	@test -n "$(EMAIL)" -a -n "$(NAME)" || (echo "EMAIL と NAME を指定してください" && exit 1)
	@read -rs -p "パスワード（12文字以上）: " pw; echo; \
	printf '%s\n' "$$pw" | docker compose run --rm -T migrate createuser -email "$(EMAIL)" -name "$(NAME)" -role "$(or $(ROLE),fpa_admin)"

test: test-api test-web ## テストを実行する（E2E を除く）

test-web: ## フロントの lint・ビルド・単体テストを実行する
	cd web && npm run lint && npm run build && npm test

e2e: ## E2E テストを専用のスタック（http://localhost:18080）で実行する
	scripts/e2e.sh

demo: ## 研修・試用の環境を起動する（http://localhost:18100、初回はデモのデータを入れる。docs/demo.md）
	scripts/demo.sh

demo-down: ## 研修・試用の環境を止めて、データを消す
	scripts/demo.sh down

demo-reset: ## 研修・試用の環境を、デモのデータの初期の状態に戻す
	scripts/demo.sh reset

dev-demo: ## 開発環境（8080）のデータをすべて消して、デモのデータを入れる（確認あり。FORCE=1 で省略）
	scripts/dev-demo.sh

perf: ## 本番に近い量のデータで、主な API の速さを測る（docs/performance.md）
	scripts/perf.sh

manual-screenshots: ## 手引きの画像（web/public/manual/）を、専用のスタックにデモのデータを入れて撮り直す
	scripts/e2e.sh --config=playwright.screenshots.config.ts

test-api: ## API のテスト（Rust の fmt・clippy・単体・統合テスト）を実行する（DB を使うテストは compose の db に接続する）
	cd api && cargo fmt --check && cargo clippy --all-targets --locked -- -D warnings \
	  && TEST_DB_HOST=127.0.0.1 TEST_DB_PORT=$${DB_PORT_HOST:-3307} TEST_DB_USER=root TEST_DB_PASSWORD=$${MYSQL_ROOT_PASSWORD:-root} cargo test --locked

reset-db: ## DB のデータを削除して作り直す
	docker compose down -v
	docker compose up --build -d
