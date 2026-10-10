#!/usr/bin/env bash
# 速さの計測（docs/performance.md、I-15）。本番に近い量のデータを入れた専用の DB（perf_*）で API を起動し、主な API の時間を測る。
#
#   scripts/perf.sh              作成 → 計測 → DB を消す
#   KEEP=1 scripts/perf.sh       計測の後も DB を残す（もう一度測るときは SEED=0 で作り直しを省ける）
#   SEED=0 scripts/perf.sh       既存の perf_* の DB で計測だけする
#
# 接続先の MySQL は TEST_DB_* と同じ（既定は開発環境の 127.0.0.1:3307、root）。開発用の DB（fpanda）は使わない。
set -euo pipefail
cd "$(dirname "$0")/.."

export DB_HOST="${TEST_DB_HOST:-127.0.0.1}" DB_PORT="${TEST_DB_PORT:-3307}" DB_USER="${TEST_DB_USER:-root}" DB_PASSWORD="${TEST_DB_PASSWORD:-root}"
export DB_NAME="${PERF_DB:-perf_fpanda}"
export HTTP_ADDR=":${PERF_PORT:-18090}"
BASE="http://localhost:${PERF_PORT:-18090}"
EMAIL=perf-admin@example.com
PASSWORD=perf-admin-password
RUNS="${RUNS:-3}"
WORK="$(mktemp -d)"
trap 'kill "${SERVER_PID:-0}" 2>/dev/null || true; rm -rf "$WORK"; if [ "${KEEP:-}" != "1" ]; then mysql_exec "DROP DATABASE IF EXISTS \`$DB_NAME\`" || true; fi' EXIT

mysql_exec() { docker compose exec -T db mysql -u"$DB_USER" -p"$DB_PASSWORD" -e "$1" 2>/dev/null; }

cd api
# 本番と同じく、最適化したビルドで測る
cargo build --release --locked --bin server --bin createuser --bin perfseed
BIN=target/release
if [ "${SEED:-1}" = "1" ]; then
  mysql_exec "DROP DATABASE IF EXISTS \`$DB_NAME\`; CREATE DATABASE \`$DB_NAME\` CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci"
  echo "== データの作成"
  "$BIN/perfseed"
  printf '%s\n' "$PASSWORD" | "$BIN/createuser" -email "$EMAIL" -name "計測用 FP&A" -role fpa_admin >/dev/null
fi

"$BIN/server" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 30); do curl -fs "$BASE/api/health" >/dev/null 2>&1 && break; sleep 1; done

JAR="$WORK/cookies"
curl -fs -c "$JAR" -H 'Content-Type: application/json' -d "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}" "$BASE/api/auth/login" >/dev/null
get() { curl -fs -b "$JAR" "$BASE$1"; }
ACTIVE=$(get /api/scenarios/active | grep -oE '"id":[0-9]+' | head -1 | cut -d: -f2)
PREV=$((ACTIVE - 1))
INITIAL=$((ACTIVE - 6))
IDS=$(seq 1 200 | paste -sd, -)

# 1行: 名前、メソッド、パス、（POST の本文のファイル）
measure() {
  local name=$1 method=$2 path=$3 body=${4:-}
  local times=() out code t
  for _ in $(seq 1 "$RUNS"); do
    if [ "$method" = GET ]; then
      out=$(curl -s -o /dev/null -w '%{http_code} %{time_total}' -b "$JAR" "$BASE$path")
    else
      out=$(curl -s -o /dev/null -w '%{http_code} %{time_total}' -b "$JAR" -F "file=@$body" -F "reason=計測" "$BASE$path")
    fi
    code=${out%% *}
    t=${out#* }
    times+=("$t")
  done
  median=$(printf '%s\n' "${times[@]}" | sort -n | sed -n "$(( (RUNS + 1) / 2 ))p")
  # API の欄は、長いクエリを省く
  printf '| %s | `%s` | %s | %d |\n' "$name" "$(echo "$path" | sed -E 's/activity_ids=[0-9,]+/activity_ids=…/')" "$code" "$(awk "BEGIN { printf \"%d\", $median * 1000 }")"
}

# 実績の取込（確認のみ）の CSV: 20万行（2026年9月、施策コードで割り当たる）
awk 'BEGIN { print "target_month,account_code,department_code,box_code,amount,description"; for (i = 0; i < 200000; i++) printf "2026-09,%d,D%d,ACT-%05d,%d,計測\n", (i % 2 ? 4001 + i % 10 : 5001 + i % 20), i % 20, i % 2000 + 1, 1000 + i % 9000 }' >"$WORK/actuals.csv"

echo
echo "== 計測（${RUNS}回の中央値、ミリ秒）"
echo "| 画面・操作 | API | 状態 | ミリ秒 |"
echo "| --- | --- | --- | ---: |"
measure "ホーム（FP&A・全施策）" GET "/api/scenarios/$ACTIVE/activity-status?scope=all"
measure "ホームのマイルストーン（200施策）" GET "/api/scenarios/$ACTIVE/milestones?activity_ids=$IDS"
measure "数値入力（1施策）" GET "/api/scenarios/$ACTIVE/activities/1"
measure "施策の一覧" GET "/api/activities"
measure "予実比較（2版＋実績）" GET "/api/reports/comparison?scenario_ids=$INITIAL,$ACTIVE&include_actual=true"
measure "予実比較（加重見込）" GET "/api/reports/comparison?scenario_ids=$INITIAL,$ACTIVE&measure=weighted"
measure "リスク画面・施策の一覧の図（比較あり）" GET "/api/reports/risk?scenario_id=$ACTIVE&compare_id=$PREV"
measure "リスク画面（比較なし）" GET "/api/reports/risk?scenario_id=$ACTIVE"
measure "変更履歴（全体）" GET "/api/change-sets"
measure "変更履歴（1施策）" GET "/api/change-sets?activity_id=1"
measure "実績の修正の食い違い" GET "/api/scenarios/actual-drift"
measure "未割当の一覧" GET "/api/actuals/unallocated?fiscal_year=2026"
measure "計画値の CSV（金額）" GET "/api/scenarios/$ACTIVE/amounts/export"
measure "実績の取込の確認（20万行）" POST "/api/actuals/import?dry_run=true" "$WORK/actuals.csv"
