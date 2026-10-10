//! 施策の実績の明細（docs/plan.md「2.12」の実績の明細）。
//!
//! 既定はすべての月で、新しい月から順に返す。年度・月・科目で絞り込める。画面は 100 件ずつ読み込み、
//! 合計（明細の合計・実績データの合計）は絞り込んだ全件で返す。エクスポートは絞り込みどおりの全件を CSV で返す。
//! hide_details の会計科目は、FP&A 以外には明細を出さず、会計科目 × 科目（CSV は月も）の合計だけを返す。
//! 閲覧制限のある科目（docs/plan.md「2.17」）は、明細も合計も出さない。

use std::collections::HashMap;

use axum::extract::State;
use axum::response::Response;
use mysql_async::prelude::*;
use mysql_async::{Conn, Params};
use serde::Serialize;

use super::is_year_month;
use crate::auth::{CurrentUser, ROLE_FPA_ADMIN, User};
use crate::db::Value;
use crate::error::{bad_request, not_found};
use crate::http::{self, Id, Query};
use crate::{AppResult, AppState, calc, csvio, visibility};

const ENTRIES_PAGE_SIZE: i64 = 100;
const ENTRIES_MAX_PAGE: i64 = 500;

#[derive(Clone, Debug, Serialize)]
struct EntryItem {
    id: i64,
    target_month: String,
    gl_account_code: String,
    gl_account_name: String,
    subject_id: i64,
    department_code: Option<String>,
    box_code: Option<String>,
    description: String,
    amount: String,
    allocated_by: String,
}
from_row!(EntryItem {
    id,
    target_month,
    gl_account_code,
    gl_account_name,
    subject_id,
    department_code,
    box_code,
    description,
    amount,
    allocated_by
});

/// 明細を見せない会計科目の、会計科目 × 科目の合計。
#[derive(Clone, Debug, Serialize)]
struct HiddenTotal {
    gl_account_code: String,
    gl_account_name: String,
    subject_id: i64,
    count: i64,
    amount: String,
}
from_row!(HiddenTotal { gl_account_code, gl_account_name, subject_id, count, amount });

#[derive(Serialize)]
struct EntriesResponse {
    /// この施策の実績（合計）がある月
    months: Vec<String>,
    /// 明細の1ページ（新しい月から）。total は絞り込んだ明細の件数（明細を見せない会計科目を除く）
    items: Vec<EntryItem>,
    total: i64,
    offset: i64,
    has_more: bool,
    /// 明細を見せない会計科目の合計（絞り込んだ範囲）
    hidden: Vec<HiddenTotal>,
    /// entries_total は明細の合計、fact_total は実績データ（actual_facts）の合計（絞り込んだ範囲）。
    /// 明細のない実績（明細を持たない移行前の実績）があると一致しない
    entries_total: String,
    fact_total: String,
    /// 閲覧制限のある科目を除いたか（docs/plan.md「2.17」）
    restricted_hidden: bool,
}

/// 明細の絞り込み。where_ は actual_entries（別名 e）、fact_where は actual_facts の条件。
struct EntriesFilter {
    activity_id: i64,
    activity_code: String,
    /// hide_details の会計科目の明細も見せるか（FP&A）
    show_details: bool,
    where_: String,
    fact_where: String,
    args: Vec<Value>,
}

impl EntriesFilter {
    /// 明細を行で見せる会計科目の条件（FP&A はすべて）。hidden が true なら見せない会計科目の条件。
    fn detail_cond(&self, hidden: bool) -> &'static str {
        match (self.show_details, hidden) {
            (true, true) => " AND FALSE",
            (true, false) => "",
            (false, true) => " AND g.hide_details",
            (false, false) => " AND NOT g.hide_details",
        }
    }

    fn params(&self) -> Params {
        Params::Positional(self.args.clone())
    }
}

/// month=YYYY-MM・fiscal_year=YYYY・subject_id= を読む。施策がなければ 404。
async fn parse_filter(conn: &mut Conn, u: &User, id: i64, q: &Query) -> AppResult<EntriesFilter> {
    let code: Option<String> = conn.exec_first("SELECT code FROM activities WHERE id = ?", (id,)).await?;
    let activity_code = code.ok_or_else(|| not_found("施策が見つかりません"))?;
    let mut f = EntriesFilter {
        activity_id: id,
        activity_code,
        show_details: u.role == ROLE_FPA_ADMIN,
        where_: format!("e.activity_id = ?{}", visibility::subject_filter(u, "e.subject_id")),
        fact_where: format!("activity_id = ?{}", visibility::subject_filter(u, "subject_id")),
        args: vec![id.into()],
    };
    let mut add = |cond: &str, fact_cond: &str, args: Vec<Value>| {
        f.where_.push_str(&format!(" AND {cond}"));
        f.fact_where.push_str(&format!(" AND {fact_cond}"));
        f.args.extend(args);
    };
    let m = q.get("month");
    if !m.is_empty() {
        if !is_year_month(m) {
            return Err(bad_request("month は YYYY-MM 形式で指定してください"));
        }
        add("e.target_month = ?", "target_month = ?", vec![format!("{m}-01").into()]);
    }
    let s = q.get("fiscal_year");
    if !s.is_empty() {
        let fy = match s.parse::<i32>() {
            Ok(fy) if (2000..=2999).contains(&fy) => fy,
            _ => return Err(bad_request("fiscal_year は年度（例: 2026）で指定してください")),
        };
        let months = calc::fiscal_months(fy);
        add(
            "e.target_month BETWEEN ? AND ?",
            "target_month BETWEEN ? AND ?",
            vec![format!("{}-01", months[0]).into(), format!("{}-01", months[months.len() - 1]).into()],
        );
    }
    let s = q.get("subject_id");
    if !s.is_empty() {
        let n: i64 = s.parse().map_err(|_| bad_request("subject_id は数値で指定してください"))?;
        add("e.subject_id = ?", "subject_id = ?", vec![n.into()]);
    }
    Ok(f)
}

const ENTRY_SELECT: &str = "SELECT e.id, DATE_FORMAT(e.target_month, '%Y-%m'), g.code, g.name, e.subject_id,
           e.department_code, e.box_code, COALESCE(e.description, ''), CAST(e.amount AS CHAR), e.allocated_by
    FROM actual_entries e JOIN gl_accounts g ON g.id = e.gl_account_id";

/// GET /api/activities/{id}/actual-entries?month=&fiscal_year=&subject_id=&offset=&limit=
pub async fn list(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    Id(id): Id,
    q: Query,
) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let f = parse_filter(&mut conn, &u, id, &q).await?;
    let mut offset = 0;
    let mut limit = ENTRIES_PAGE_SIZE;
    if q.has("offset") && !q.get("offset").is_empty() {
        offset = match q.get("offset").parse::<i64>() {
            Ok(n) if n >= 0 => n,
            _ => return Err(bad_request("offset は 0 以上の数値で指定してください")),
        };
    }
    if q.has("limit") && !q.get("limit").is_empty() {
        limit = match q.get("limit").parse::<i64>() {
            Ok(n) if (1..=ENTRIES_MAX_PAGE).contains(&n) => n,
            _ => return Err(bad_request(format!("limit は 1〜{ENTRIES_MAX_PAGE} で指定してください"))),
        };
    }

    let restricted_hidden = visibility::hidden(&mut conn, &u).await?;
    let months: Vec<String> = conn
        .exec(
            format!(
                "SELECT DISTINCT DATE_FORMAT(target_month, '%Y-%m') FROM actual_facts WHERE activity_id = ?{} ORDER BY 1",
                visibility::subject_filter(&u, "subject_id")
            ),
            (f.activity_id,),
        )
        .await?;

    // 件数と合計（絞り込んだ全件）
    let (total, entries_total): (i64, String) = conn
        .exec_first(
            format!(
                "SELECT COUNT(CASE WHEN TRUE{} THEN 1 END), CAST(COALESCE(SUM(e.amount), 0) AS CHAR)
                 FROM actual_entries e JOIN gl_accounts g ON g.id = e.gl_account_id WHERE {}",
                f.detail_cond(false),
                f.where_
            ),
            f.params(),
        )
        .await?
        .unwrap_or_default();
    let fact_total: String = conn
        .exec_first(
            format!("SELECT CAST(COALESCE(SUM(amount), 0) AS CHAR) FROM actual_facts WHERE {}", f.fact_where),
            f.params(),
        )
        .await?
        .unwrap_or_default();

    // 明細を見せない会計科目の合計
    let hidden: Vec<HiddenTotal> = conn
        .exec(
            format!(
                "SELECT g.code, g.name, e.subject_id, COUNT(*), CAST(SUM(e.amount) AS CHAR)
                 FROM actual_entries e JOIN gl_accounts g ON g.id = e.gl_account_id
                 WHERE {}{}
                 GROUP BY g.code, g.name, e.subject_id ORDER BY g.code, e.subject_id",
                f.where_,
                f.detail_cond(true)
            ),
            f.params(),
        )
        .await?;

    // 明細の1ページ（新しい月から）
    let mut args = f.args.clone();
    args.extend([limit.into(), offset.into()]);
    let items: Vec<EntryItem> = conn
        .exec(
            format!(
                "{ENTRY_SELECT} WHERE {}{} ORDER BY e.target_month DESC, g.code, e.line_no, e.id LIMIT ? OFFSET ?",
                f.where_,
                f.detail_cond(false)
            ),
            Params::Positional(args),
        )
        .await?;
    let has_more = offset + (items.len() as i64) < total;
    Ok(http::ok(&EntriesResponse {
        months,
        items,
        total,
        offset,
        has_more,
        hidden,
        entries_total,
        fact_total,
        restricted_hidden,
    }))
}

fn allocated_by_label(by: &str) -> &'static str {
    match by {
        "activity_code" => "施策コード",
        "external_code" => "外部コード",
        "rule" => "割当ルール",
        "manual" => "未割当の一覧から選択",
        "unallocated" => "未割当",
        _ => "",
    }
}

const EXPORT_HEADER: [&str; 10] = [
    "target_month",
    "subject_code",
    "subject_name",
    "account_code",
    "account_name",
    "department_code",
    "box_code",
    "description",
    "amount",
    "allocated_by",
];

/// GET /api/activities/{id}/actual-entries/export?month=&fiscal_year=&subject_id=。
/// 絞り込みどおりの明細を、件数の制限なしで CSV（BOM 付き UTF-8）にする。新しい月から並べる。
/// 明細を見せない会計科目は、月 × 会計科目 × 科目の合計の行にする（摘要に「明細は FP&A のみ」と書く）。
pub async fn export(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    Id(id): Id,
    q: Query,
) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let f = parse_filter(&mut conn, &u, id, &q).await?;
    let rows: Vec<(i64, String, String)> = conn.query("SELECT id, code, name FROM subjects").await?;
    let subjects: HashMap<i64, (String, String)> = rows.into_iter().map(|(id, c, n)| (id, (c, n))).collect();
    let subject = |id: i64| subjects.get(&id).cloned().unwrap_or_default();

    let mut lines: Vec<(String, Vec<String>)> = Vec::new();
    let items: Vec<EntryItem> = conn
        .exec(
            format!(
                "{ENTRY_SELECT} WHERE {}{} ORDER BY e.target_month DESC, g.code, e.line_no, e.id",
                f.where_,
                f.detail_cond(false)
            ),
            f.params(),
        )
        .await?;
    for e in items {
        let (sc, sn) = subject(e.subject_id);
        lines.push((
            e.target_month.clone(),
            vec![
                e.target_month,
                sc,
                sn,
                e.gl_account_code,
                e.gl_account_name,
                e.department_code.unwrap_or_default(),
                e.box_code.unwrap_or_default(),
                e.description,
                e.amount,
                allocated_by_label(&e.allocated_by).to_string(),
            ],
        ));
    }
    let totals: Vec<(String, String, String, i64, i64, String)> = conn
        .exec(
            format!(
                "SELECT DATE_FORMAT(e.target_month, '%Y-%m'), g.code, g.name, e.subject_id, COUNT(*), CAST(SUM(e.amount) AS CHAR)
                 FROM actual_entries e JOIN gl_accounts g ON g.id = e.gl_account_id
                 WHERE {}{}
                 GROUP BY e.target_month, g.code, g.name, e.subject_id ORDER BY e.target_month DESC, g.code",
                f.where_,
                f.detail_cond(true)
            ),
            f.params(),
        )
        .await?;
    for (month, code, name, subject_id, count, amount) in totals {
        let (sc, sn) = subject(subject_id);
        lines.push((
            month.clone(),
            vec![
                month,
                sc,
                sn,
                code,
                name,
                String::new(),
                String::new(),
                format!("明細は FP&A のみ（{count} 行の合計）"),
                amount,
                String::new(),
            ],
        ));
    }
    // 明細と合計の行を、新しい月の順にそろえる（同じ月の中では明細が先）
    lines.sort_by(|a, b| b.0.cmp(&a.0));
    let out: Vec<Vec<String>> = lines.into_iter().map(|(_, r)| r).collect();
    csvio::write_csv(&format!("actual_entries_{}", f.activity_code), &EXPORT_HEADER, &out)
}
