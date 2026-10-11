//! 速さの計測（docs/performance.md）のために、本番に近い量のデータを作る。
//!
//! 作るデータ（既定）: 施策 2,000、ユニット 40、シナリオ 31（3年度分の月次の見込）、金額 約150万行、
//! 実績の合計 12万行、ロック済みのシナリオに保存した実績、明細 20万行、監査ログ 50万行、マイルストーン 4,000。
//!
//! 誤って普段の DB に入れないよう、DB_NAME が "perf_" で始まり、施策が1件もない DB にだけ入れる。
//! マイグレーションを先に適用する。接続先は API と同じ環境変数（DB_HOST・DB_PORT・DB_USER・DB_PASSWORD・DB_NAME）。
//!
//!   perfseed [-activities 2000] [-units 40] [-entries 200000] [-audits 500000]

use std::time::Instant;

use anyhow::{anyhow, bail};
use fpanda_api::config::Config;
use fpanda_api::{db, migrate};
use mysql_async::prelude::*;

const USAGE: &str = "使い方: perfseed [-activities 2000] [-units 40] [-entries 200000] [-audits 500000]";

struct Params {
    /// 施策の数
    activities: u32,
    /// ユニットの数
    units: u32,
    /// 実績の明細の行数（2026年4〜9月に分ける）
    entries: u32,
    /// 監査ログの行数（金額の変更として作る）
    audits: u32,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("perfseed: {e:#}");
        std::process::exit(1);
    }
}

/// Go の flag パッケージと同じく、-name value・--name value・-name=value を受け付ける。
fn parse_args() -> anyhow::Result<Params> {
    let mut p = Params { activities: 2000, units: 40, entries: 200_000, audits: 500_000 };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let flag = arg.trim_start_matches('-');
        let (key, inline) = match flag.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (flag.to_string(), None),
        };
        let raw = inline.or_else(|| args.next()).ok_or_else(|| {
            anyhow!(
                "-{key} に値がありません
{USAGE}"
            )
        })?;
        let n: u32 = raw.parse().map_err(|_| {
            anyhow!(
                "-{key} は数値で指定してください
{USAGE}"
            )
        })?;
        match key.as_str() {
            "activities" => p.activities = n,
            "units" => p.units = n,
            "entries" => p.entries = n,
            "audits" => p.audits = n,
            _ => bail!(
                "不明な引数です: {arg}
{USAGE}"
            ),
        }
    }
    Ok(p)
}

/// 手順ごとの SQL。
fn steps(p: &Params) -> Vec<(&'static str, Vec<String>)> {
    vec![
        (
            "連番の表",
            vec![
                r#"CREATE TABLE perf_seq (n INT NOT NULL PRIMARY KEY)"#.to_string(),
                r#"INSERT INTO perf_seq (n)
			 SELECT a.n + b.n * 10 + c.n * 100 + d.n * 1000 + e.n * 10000 + f.n * 100000
			 FROM (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) a,
			      (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) b,
			      (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) c,
			      (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) d,
			      (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) e,
			      (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) f"#.to_string(),
            ],
        ),
        (
            "マスタ",
            vec![
                r#"INSERT INTO segments (code, name, level, sort_order) SELECT CONCAT('SEG-', n + 1), CONCAT('事業', n + 1), 1, n FROM perf_seq WHERE n < 4"#.to_string(),
                r#"INSERT INTO organizations (code, name, level, sort_order) SELECT CONCAT('ORG-', n + 1), CONCAT('本部', n + 1), 1, n FROM perf_seq WHERE n < 4"#.to_string(),
                // ユーザー: マネージャー（ユニットの数）と担当者 160 人。パスワードなし（計測は FP&A で行う）
                format!(
                    r#"INSERT INTO users (name, email, password_hash, role) SELECT CONCAT('マネージャー', n + 1), CONCAT('mgr', n + 1, '@perf.example.com'), '', 'manager' FROM perf_seq WHERE n < {}"#,
                    p.units
                ),
                r#"INSERT INTO users (name, email, password_hash, role) SELECT CONCAT('担当', n + 1), CONCAT('member', n + 1, '@perf.example.com'), '', 'member' FROM perf_seq WHERE n < 160"#.to_string(),
                format!(
                    r#"INSERT INTO units (code, name, segment_id, organization_id, owner_user_id)
			 SELECT CONCAT('UNIT-', n + 1), CONCAT('ユニット', n + 1), n % 4 + 1, n % 4 + 1, (SELECT id FROM users WHERE email = CONCAT('mgr', n + 1, '@perf.example.com'))
			 FROM perf_seq WHERE n < {}"#,
                    p.units
                ),
                r#"INSERT INTO subjects (code, name, category, sort_order) SELECT CONCAT('4', LPAD(n + 1, 3, '0')), CONCAT('売上', n + 1), 'revenue', n FROM perf_seq WHERE n < 10"#.to_string(),
                r#"INSERT INTO subjects (code, name, category, sort_order) SELECT CONCAT('5', LPAD(n + 1, 3, '0')), CONCAT('費用', n + 1), 'expense', n FROM perf_seq WHERE n < 20"#.to_string(),
                r#"INSERT INTO gl_accounts (code, name, subject_id) SELECT code, name, id FROM subjects"#.to_string(),
            ],
        ),
        (
            "施策",
            vec![
                // 半分はプロジェクト型（期間あり）、3割は運用型、2割はコストプール型。1割は完了
                format!(
                    r#"INSERT INTO activities (unit_id, code, name, activity_type, status, start_date, end_date, owner_user_id, confidence_level)
			 SELECT n % {} + 1, CONCAT('ACT-', LPAD(n + 1, 5, '0')), CONCAT('施策', n + 1),
			        CASE WHEN n % 10 < 5 THEN 'project' WHEN n % 10 < 8 THEN 'recurring' ELSE 'cost_pool' END,
			        IF(n % 10 = 9, 'completed', 'in_progress'),
			        IF(n % 10 < 5, DATE_ADD('2024-04-01', INTERVAL n % 30 MONTH), NULL),
			        IF(n % 10 < 5, DATE_ADD('2024-04-01', INTERVAL n % 30 + 12 MONTH), NULL),
			        (SELECT id FROM users WHERE email = CONCAT('member', n % 160 + 1, '@perf.example.com')),
			        ELT(n % 5 + 1, 'A', 'B', 'C', 'D', 'E')
			 FROM perf_seq WHERE n < {}"#,
                    p.units, p.activities
                ),
                // 内訳: 売上の内訳を1つ（見通しの種類と段階もばらす）
                r#"INSERT INTO activity_lines (activity_id, subject_id, name, confidence_level, outlook)
			 SELECT a.id, (SELECT id FROM subjects WHERE category = 'revenue' ORDER BY id LIMIT 1 OFFSET 0) + a.id % 10, '本体',
			        IF(a.id % 7 = 0, 'D', NULL), IF(a.id % 11 = 0, 'downside', IF(a.id % 5 = 0, 'addon', 'base'))
			 FROM activities a"#.to_string(),
                r#"INSERT INTO activity_milestones (activity_id, name, due_date, status)
			 SELECT a.id, CONCAT('マイルストーン', s.n + 1), DATE_ADD('2026-04-01', INTERVAL (a.id * 7 + s.n * 90) % 360 DAY), IF((a.id + s.n) % 4 = 0, 'completed', 'in_progress')
			 FROM activities a JOIN perf_seq s ON s.n < 2"#.to_string(),
            ],
        ),
        (
            "シナリオ",
            vec![
                // 2024・2025年度は月次の見込を12版ずつ、2026年度は4〜10月の7版。最新の版を作成中（今回の見込）にする
                r#"INSERT INTO scenarios (name, fiscal_year, plan_role, actual_through, is_locked, created_by)
			 SELECT CONCAT(fy, '年度 ', IF(v = 0, '期初計画', CONCAT((v + 3) % 12 + 1, '月見込'))), fy,
			        IF(v = 0, 'initial', NULL), IF(v = 0, NULL, DATE_ADD(MAKEDATE(fy, 1), INTERVAL v + 2 MONTH)),
			        NOT (fy = 2026 AND v = 6), (SELECT MIN(id) FROM users)
			 FROM (SELECT 2024 fy UNION ALL SELECT 2025 UNION ALL SELECT 2026) y
			 JOIN (SELECT n v FROM perf_seq WHERE n < 12) m ON fy < 2026 OR v <= 6
			 ORDER BY fy, v"#.to_string(),
                // 複製元・前回見込は同じ年度の1つ前の版
                r#"UPDATE scenarios s JOIN scenarios p ON p.fiscal_year = s.fiscal_year AND p.id = s.id - 1
			 SET s.base_scenario_id = p.id, s.previous_scenario_id = p.id"#.to_string(),
                r#"UPDATE scenarios SET plan_role = 'latest' WHERE id IN (SELECT id FROM (SELECT MAX(id) id FROM scenarios GROUP BY fiscal_year) x)"#.to_string(),
                r#"UPDATE scenarios SET is_active = TRUE, update_deadline = '2026-10-16' WHERE fiscal_year = 2026 AND plan_role = 'latest'"#.to_string(),
            ],
        ),
        (
            "金額",
            vec![
                // 版 × 施策 × 12か月 × 2行（売上の内訳、費用の科目への直接入力）
                r#"INSERT INTO budget_facts (scenario_id, activity_id, subject_id, line_id, target_month, amount, source)
			 SELECT s.id, l.activity_id, l.subject_id, l.id, DATE_ADD(MAKEDATE(s.fiscal_year, 1), INTERVAL m.n + 3 MONTH),
			        IF(l.outlook = 'downside', -1, 1) * CAST((l.activity_id % 50 + 10) * 10000 + m.n * 1000 + s.id * 100 AS SIGNED), 'manual'
			 FROM scenarios s JOIN activity_lines l JOIN perf_seq m ON m.n < 12"#.to_string(),
                r#"INSERT INTO budget_facts (scenario_id, activity_id, subject_id, line_id, target_month, amount, source)
			 SELECT s.id, a.id, (SELECT MIN(id) FROM subjects WHERE category = 'expense') + a.id % 20, NULL, DATE_ADD(MAKEDATE(s.fiscal_year, 1), INTERVAL m.n + 3 MONTH),
			        (a.id % 30 + 5) * 10000 + m.n * 500, 'manual'
			 FROM scenarios s JOIN activities a JOIN perf_seq m ON m.n < 12"#.to_string(),
            ],
        ),
        (
            "実績",
            vec![
                // 2024年4月〜2026年9月の30か月 × 施策 × 2科目
                r#"INSERT INTO actual_facts (activity_id, subject_id, target_month, amount)
			 SELECT l.activity_id, l.subject_id, DATE_ADD('2024-04-01', INTERVAL m.n MONTH), (l.activity_id % 50 + 10) * 10000 + m.n * 900
			 FROM activity_lines l JOIN perf_seq m ON m.n < 30"#.to_string(),
                r#"INSERT INTO actual_facts (activity_id, subject_id, target_month, amount)
			 SELECT a.id, (SELECT MIN(id) FROM subjects WHERE category = 'expense') + a.id % 20, DATE_ADD('2024-04-01', INTERVAL m.n MONTH), (a.id % 30 + 5) * 10000
			 FROM activities a JOIN perf_seq m ON m.n < 30"#.to_string(),
                // 未割当の実績も少し（月ごと1行）
                r#"INSERT INTO actual_facts (activity_id, subject_id, target_month, amount)
			 SELECT NULL, (SELECT MIN(id) FROM subjects WHERE category = 'expense'), DATE_ADD('2024-04-01', INTERVAL m.n MONTH), 123000
			 FROM perf_seq m WHERE m.n < 30"#.to_string(),
                // ロック済みのシナリオに、ロックしたときの実績を保存する
                r#"INSERT INTO scenario_actuals (scenario_id, activity_id, subject_id, target_month, amount)
			 SELECT s.id, f.activity_id, f.subject_id, f.target_month, f.amount
			 FROM scenarios s JOIN actual_facts f
			   ON f.target_month BETWEEN DATE_ADD(MAKEDATE(s.fiscal_year, 1), INTERVAL 3 MONTH) AND s.actual_through
			 WHERE s.is_locked AND s.actual_through IS NOT NULL"#.to_string(),
            ],
        ),
        (
            "変更履歴と明細",
            vec![
                r#"INSERT INTO change_sets (user_id, scenario_id, reason) SELECT MIN(id), NULL, '計測用のデータ' FROM users"#.to_string(),
                format!(
                    r#"INSERT INTO actual_entries (target_month, line_no, gl_account_id, subject_id, department_code, box_code, amount, description, activity_id, allocated_by, change_set_id)
			 SELECT DATE_ADD('2026-04-01', INTERVAL s.n % 6 MONTH), s.n DIV 6 + 1, g.id, g.subject_id, CONCAT('D', s.n % 20), CONCAT('ACT-', LPAD(s.n % {} + 1, 5, '0')),
			        1000 + s.n % 9000, '計測用の明細', s.n % {} + 1, 'activity_code', (SELECT MAX(id) FROM change_sets)
			 FROM perf_seq s JOIN gl_accounts g ON g.id = s.n % 30 + 1 WHERE s.n < {}"#,
                    p.activities, p.activities, p.entries
                ),
                format!(
                    r#"INSERT INTO audit_logs (change_set_id, table_name, record_id, action, before_json, after_json)
			 SELECT (SELECT MAX(id) FROM change_sets), 'budget_facts', s.n + 1, 'update',
			        JSON_OBJECT('activity_id', s.n % {} + 1, 'subject_id', 1, 'amount', '100000'), JSON_OBJECT('activity_id', s.n % {} + 1, 'subject_id', 1, 'amount', '120000')
			 FROM perf_seq s WHERE s.n < {}"#,
                    p.activities, p.activities, p.audits
                ),
                // マイルストーンの期日の後ろ倒し（4件に1件）。リスク画面の「後ろ倒し」が監査ログを検索する
                r#"INSERT INTO audit_logs (change_set_id, table_name, record_id, action, before_json, after_json)
			 SELECT (SELECT MAX(id) FROM change_sets), 'activity_milestones', m.id, 'update',
			        JSON_OBJECT('activity_id', m.activity_id, 'due_date', DATE_FORMAT(DATE_SUB(m.due_date, INTERVAL 20 DAY), '%Y-%m-%d')),
			        JSON_OBJECT('activity_id', m.activity_id, 'due_date', DATE_FORMAT(m.due_date, '%Y-%m-%d'))
			 FROM activity_milestones m WHERE m.id % 4 = 0"#.to_string(),
            ],
        ),
        (
            "今回の見込の状態",
            vec![
                // 4割は入力中、2割は完了
                r#"INSERT INTO activity_scenario_notes (scenario_id, activity_id, explanation, last_edited_at, completed_at)
			 SELECT s.id, a.id, IF(a.id % 5 < 1, '計測用の説明', NULL), NOW(), IF(a.id % 5 < 1, NOW(), NULL)
			 FROM scenarios s JOIN activities a ON a.id % 5 < 3 WHERE s.is_active"#.to_string(),
            ],
        ),
        ("後片付け", vec![
            "DROP TABLE perf_seq".to_string(),
            "ANALYZE TABLE budget_facts, actual_facts, scenario_actuals, actual_entries, audit_logs, activities".to_string(),
        ]),
    ]
}

async fn run() -> anyhow::Result<()> {
    let p = parse_args()?;
    let cfg = Config::load();
    if !cfg.db.name.starts_with("perf_") {
        bail!("DB_NAME は perf_ で始まる名前にしてください（今は {:?}）。普段の DB には入れません", cfg.db.name);
    }
    let mut conn = mysql_async::Conn::new(db::opts(&cfg.db, Some(&cfg.db.name))).await?;
    migrate::up(&mut conn).await?;
    let n: i64 = conn.query_first("SELECT COUNT(*) FROM activities").await?.unwrap_or(0);
    if n > 0 {
        bail!("施策がすでにある DB です。空の DB を指定してください");
    }
    for (name, sqls) in steps(&p) {
        let start = Instant::now();
        for q in sqls {
            conn.query_drop(&q).await.map_err(|e| {
                anyhow!(
                    "{name}: {e}
{q}"
                )
            })?;
        }
        println!("{name:<16} {:6.1}s", start.elapsed().as_secs_f64());
    }
    for t in
        ["activities", "scenarios", "budget_facts", "actual_facts", "scenario_actuals", "actual_entries", "audit_logs"]
    {
        let c: i64 = conn.query_first(format!("SELECT COUNT(*) FROM {t}")).await?.unwrap_or(0);
        println!("{t:<18} {c:>9} 行");
    }
    conn.disconnect().await?;
    Ok(())
}
