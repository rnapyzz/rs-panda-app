//! 実績 CSV（会計の明細）の取込（docs/plan.md「6.1」）。

use std::collections::{BTreeMap, BTreeSet, HashMap};

use axum::extract::State;
use axum::extract::multipart::{Multipart, MultipartRejection};
use axum::response::Response;
use mysql_async::prelude::*;
use num_bigint::BigInt;
use serde::Serialize;

use super::accounts::{GlAccount, load_accounts};
use super::allocate::{ALLOCATION_KINDS, Allocator, load_allocator};
use super::closings::{closed_years, fiscal_year_of};
use super::drift::{ScenarioDrift, load_drift};
use super::facts::{MonthTotals, monthly_totals, sync_facts};
use super::rules::DEPARTMENT_PATTERN;
use super::{FactCounts, amount_fits, is_year_month, month_args, parse_amount, placeholders};
use crate::auth::{AdminUser, CurrentUser};
use crate::csvio::{self, RowErrors};
use crate::db::{self, Params, Tx, Value};
use crate::error::bad_request;
use crate::http::{self, Query};
use crate::{AppResult, AppState, audit, calc};

/// 50MB（nginx の client_max_body_size と合わせる）。
pub const MAX_IMPORT_BYTES: usize = 50 << 20;
const MAX_IMPORT_ROWS: usize = 200_000;
/// 1回の INSERT の行数（11列 × 2,000 = 22,000 個のプレースホルダー。上限は 65,535）
const INSERT_BATCH: usize = 2000;

const REQUIRED_COLUMNS: [&str; 3] = ["target_month", "account_code", "amount"];
const OPTIONAL_COLUMNS: [&str; 3] = ["department_code", "box_code", "description"];

/// CSV の1データ行。
#[derive(Clone, Debug)]
pub struct EntryRow {
    pub line: usize,
    pub month: String,
    pub account: String,
    pub dept: String,
    pub box_code: String,
    pub description: String,
    pub amount: BigInt,
}

/// 実績 CSV の形式（ヘッダー・列数・年月・金額・長さ）を検証して行を返す。
/// target_month・account_code・amount は必須の列。department_code・box_code・description は列ごと省略できる。
pub fn parse_entries_csv(data: &[u8]) -> AppResult<Vec<EntryRow>> {
    let mut out = Vec::new();
    let mut errs = RowErrors::default();
    let rows = csvio::read_rows(data, &REQUIRED_COLUMNS, &OPTIONAL_COLUMNS, MAX_IMPORT_ROWS, &mut errs, |r, errs| {
        out.push(entry_row(r, errs));
    })?;
    errs.check()?;
    if rows.is_empty() {
        return Err(bad_request("CSV にデータ行がありません"));
    }
    Ok(out)
}

/// 1行の値を検証する。
fn entry_row(r: &csvio::Row, errs: &mut RowErrors) -> EntryRow {
    let line = r.line;
    let month = r.get("target_month").to_string();
    let account = r.get("account_code").to_string();
    let dept = r.get("department_code").to_string();
    let box_code = r.get("box_code").to_string();
    let description = r.get("description").to_string();
    if !is_year_month(&month) {
        errs.add(line, format!("対象年月 {month:?} は YYYY-MM 形式で入力してください"));
    }
    if account.is_empty() {
        errs.add(line, "会計科目コードが空です");
    }
    if !dept.is_empty() && !DEPARTMENT_PATTERN.is_match(&dept) {
        errs.add(line, format!("部門コード {dept:?} は空白・カンマ・引用符を含まない50文字以内にしてください"));
    }
    if box_code.chars().count() > 100 {
        errs.add(line, "箱の ID は100文字以内にしてください");
    }
    if description.chars().count() > 200 {
        errs.add(line, "摘要は200文字以内にしてください");
    }
    let raw = r.get("amount");
    let amount = match parse_amount(raw) {
        None => {
            errs.add(line, format!("金額 {raw:?} は円単位の整数で入力してください"));
            BigInt::default()
        }
        Some(v) if !amount_fits(&v) => {
            errs.add(line, "金額が大きすぎます");
            BigInt::default()
        }
        Some(v) => v,
    };
    EntryRow { line, month, account, dept, box_code, description, amount }
}

/// 保存する明細。
struct Entry<'a> {
    row: &'a EntryRow,
    account_id: i64,
    subject_id: i64,
    activity_id: Option<i64>,
    by: &'static str,
}

/// 会計科目を確かめ、各行を施策に割り当てる。対象外の会計科目の行は除く（件数を返す）。
/// 未登録の会計科目は、コードごとにまとめてエラーにする。
fn resolve_entries<'a>(
    rows: &'a [EntryRow],
    accounts: &HashMap<String, GlAccount>,
    al: &Allocator,
) -> AppResult<(Vec<Entry<'a>>, usize)> {
    let mut missing: HashMap<&str, (usize, usize)> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    let mut excluded = 0;
    for row in rows {
        let Some(a) = accounts.get(&row.account) else {
            let m = missing.entry(&row.account).or_insert_with(|| {
                order.push(&row.account);
                (row.line, 0)
            });
            m.1 += 1;
            continue;
        };
        if a.is_excluded {
            excluded += 1;
            continue;
        }
        let (activity_id, by) = al.allocate(a.id, &row.dept, &row.box_code);
        out.push(Entry { row, account_id: a.id, subject_id: a.subject_id.unwrap_or(0), activity_id, by });
    }
    if !order.is_empty() {
        let mut errs = RowErrors::default();
        for code in order {
            let (first, count) = missing[code];
            errs.add(
                first,
                format!(
                    "会計科目 {code:?} は登録されていません（{count} 行）。会計科目のマスタに登録してから取り込んでください"
                ),
            );
        }
        errs.check()?;
    }
    Ok((out, excluded))
}

/// 取込の結果。
#[derive(Debug, Default, Serialize)]
struct ImportResult {
    dry_run: bool,
    months: Vec<String>,
    /// CSV のデータ行数
    rows: usize,
    /// 対象外の会計科目の行数
    excluded: usize,
    allocation: BTreeMap<&'static str, usize>,
    #[serde(flatten)]
    counts: FactCounts,
    totals: Vec<MonthTotals>,
    /// 取り込むとロック済みのシナリオの実績と食い違う月（docs/plan.md「2.14」）
    locked_drift: Vec<ScenarioDrift>,
}

/// POST /api/actuals/import。FP&A のみ。
///
/// multipart/form-data で file（CSV）と reason（変更理由、必須）を送る。
/// CSV に含まれる月の明細を置き換え、各行を施策に割り当て、施策 × 科目 × 月の合計（actual_facts）を差分で更新する。
/// ロック済みのシナリオは、ロック時に保存した実績を使うため影響を受けない。
/// クエリ dry_run=true を付けると、検証と集計だけを行い保存しない。
pub async fn import_actuals(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    q: Query,
    mp: Result<Multipart, MultipartRejection>,
) -> AppResult<Response> {
    let up = csvio::read_upload_with(
        &q,
        mp,
        "ファイルは 50MB 以下にしてください",
        "変更理由を入力してください（例: 2026-09 実績取込）",
    )
    .await?;
    let rows = parse_entries_csv(&up.data)?;

    let mut result = ImportResult {
        dry_run: up.dry_run,
        rows: rows.len(),
        allocation: ALLOCATION_KINDS.iter().map(|k| (*k, 0)).collect(),
        ..Default::default()
    };
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &up.reason).await?;
    check_closed_rows(&mut tx, &rows).await?;
    let accounts: HashMap<String, GlAccount> =
        load_accounts(&mut tx).await?.into_values().map(|a| (a.code.clone(), a)).collect();
    let al = load_allocator(&mut tx).await?;
    let (entries, excluded) = resolve_entries(&rows, &accounts, &al)?;
    result.excluded = excluded;
    for e in &entries {
        *result.allocation.entry(e.by).or_default() += 1;
    }

    // 対象の月は CSV に含まれる月（対象外の行だけの月も、その月の明細を置き換える）
    result.months = rows.iter().map(|r| r.month.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    tx.exec_drop(
        format!("DELETE FROM actual_entries WHERE target_month IN ({})", placeholders(result.months.len())),
        Params::Positional(month_args(&result.months)),
    )
    .await?;
    insert_entries(&mut tx, rec.change_set_id, &entries).await?;
    sync_facts(&mut tx, &rec, &result.months, &mut result.counts).await?;
    result.totals = monthly_totals(&mut tx, &result.months).await?;
    result.locked_drift = load_drift(&mut tx, 0, &result.months).await?;
    if up.dry_run {
        // 集計まで行ってロールバックする
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(http::ok(&result))
}

/// 締めた年度の月の行があれば、月ごとにまとめてエラーにする。
async fn check_closed_rows(tx: &mut Tx, rows: &[EntryRow]) -> AppResult<()> {
    let closed = closed_years(tx).await?;
    if closed.is_empty() {
        return Ok(());
    }
    let mut by_month: HashMap<&str, (usize, usize)> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    for row in rows {
        if !closed.contains(&fiscal_year_of(&row.month)) {
            continue;
        }
        let m = by_month.entry(&row.month).or_insert_with(|| {
            order.push(&row.month);
            (row.line, 0)
        });
        m.1 += 1;
    }
    let mut errs = RowErrors::default();
    for m in order {
        let (first, count) = by_month[m];
        errs.add(
            first,
            format!(
                "{m} は締めた年度（{}年度）の月なので取り込めません（{count} 行）。シナリオ管理で締めを解除してから取り込んでください",
                fiscal_year_of(m)
            ),
        );
    }
    errs.check()
}

/// 明細をまとめて登録する。
async fn insert_entries(tx: &mut Tx, change_set_id: i64, entries: &[Entry<'_>]) -> AppResult<()> {
    const COLS: usize = 11;
    for batch in entries.chunks(INSERT_BATCH) {
        let mut sql = String::from(
            "INSERT INTO actual_entries (target_month, line_no, gl_account_id, subject_id, department_code, box_code,
             amount, description, activity_id, allocated_by, change_set_id) VALUES ",
        );
        let mut args: Vec<Value> = Vec::with_capacity(batch.len() * COLS);
        let row = format!("({})", placeholders(COLS));
        for (i, e) in batch.iter().enumerate() {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&row);
            args.extend([
                format!("{}-01", e.row.month).into(),
                (e.row.line as i64).into(),
                e.account_id.into(),
                e.subject_id.into(),
                db::null_str(&e.row.dept),
                db::null_str(&e.row.box_code),
                e.row.amount.to_string().into(),
                db::null_str(&e.row.description),
                e.activity_id.into(),
                e.by.into(),
                change_set_id.into(),
            ]);
        }
        tx.exec_drop(sql, Params::Positional(args)).await?;
    }
    Ok(())
}

/// GET /api/actuals/months?fiscal_year=。年度のうち、実績を取り込み済みの月を返す。
/// シナリオを作成するときの決算確定月の既定値（取込済みの最終月）に使う。
pub async fn actual_months(State(st): State<AppState>, _: CurrentUser, q: Query) -> AppResult<Response> {
    let fy: i32 = q.get("fiscal_year").parse().map_err(|_| bad_request("fiscal_year は数値で指定してください"))?;
    let months = calc::fiscal_months(fy);
    let mut conn = st.db.get_conn().await?;
    let out: Vec<String> = conn
        .exec(
            "SELECT DISTINCT DATE_FORMAT(target_month, '%Y-%m') FROM actual_facts
             WHERE target_month BETWEEN ? AND ? ORDER BY 1",
            (format!("{}-01", months[0]), format!("{}-01", months[months.len() - 1])),
        )
        .await?;
    Ok(http::ok(&serde_json::json!({"fiscal_year": fy, "months": out})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AppError;

    fn row_errors(e: AppError) -> (String, Vec<String>) {
        match e {
            AppError::Api(e) => (e.message, e.rows.iter().map(|r| format!("{}:{}", r.line, r.message)).collect()),
            AppError::Internal(e) => panic!("{e}"),
        }
    }

    #[test]
    fn parses_rows() {
        let data = "\u{feff}Target_Month, account_code,department_code,box_code,amount,description\r\n\
                    2026-09,4110,D100,ACT-0001,1200000,A社 受託\r\n\
                    \r\n\
                    2026-09,8110,,,-350000,\r\n";
        let rows = parse_entries_csv(data.as_bytes()).unwrap();
        assert_eq!(rows.len(), 2);
        let r = &rows[0];
        assert_eq!((r.dept.as_str(), r.box_code.as_str(), r.description.as_str()), ("D100", "ACT-0001", "A社 受託"));
        let r = &rows[1];
        assert_eq!((r.line, r.account.as_str(), r.dept.as_str()), (4, "8110", ""));
        assert_eq!(r.amount.to_string(), "-350000");
    }

    #[test]
    fn optional_columns() {
        // 省略できる列はなくてもよく、列の順番は自由
        let rows = parse_entries_csv(b"amount,account_code,target_month\n100,4110,2026-10\n").unwrap();
        let r = &rows[0];
        assert_eq!((r.month.as_str(), r.account.as_str(), r.box_code.as_str()), ("2026-10", "4110", ""));
        assert_eq!(r.amount.to_string(), "100");
    }

    #[test]
    fn errors() {
        let cases: [(&str, &[u8], &[&str]); 6] = [
            ("空", b"", &["空です"]),
            ("必須の列がない", b"target_month,account_code\n", &["ヘッダー行"]),
            ("知らない列", b"target_month,account_code,amount,memo\n", &["ヘッダー行"]),
            ("列の重複", b"target_month,account_code,amount,amount\n", &["ヘッダー行"]),
            ("データなし", b"target_month,account_code,amount\n", &["データ行がありません"]),
            ("Shift_JIS", b"target_month,account_code,amount\n2026-09,\x83\x65,1\n", &["UTF-8"]),
        ];
        for (name, data, want) in cases {
            let (msg, rows) = row_errors(parse_entries_csv(data).unwrap_err());
            assert!(rows.is_empty() && msg.contains(want[0]), "{name}: {msg}");
        }
        let data = "target_month,account_code,department_code,amount\n\
                    2026/09,4110,,1\n\
                    2026-09,,,1.5\n\
                    2026-09,4110,D 1,1\n\
                    2026-09,4110\n\
                    2026-09,4110,,1000000000000000000\n";
        let (_, rows) = row_errors(parse_entries_csv(data.as_bytes()).unwrap_err());
        let want = ["2:YYYY-MM", "3:会計科目コードが空", "3:円単位の整数", "4:部門コード", "5:列の数", "6:大きすぎ"];
        assert_eq!(rows.len(), want.len(), "{rows:?}");
        for (g, w) in rows.iter().zip(want) {
            let (line, substr) = w.split_once(':').unwrap();
            assert!(g.starts_with(&format!("{line}:")) && g.contains(substr), "{g} / {w}");
        }
    }
}
