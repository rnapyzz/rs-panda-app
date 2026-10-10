//! 未割当の一覧と割当、再割当（docs/plan.md「2.12」）。

use std::collections::{BTreeSet, HashMap};

use axum::extract::State;
use axum::response::Response;
use chrono::NaiveDateTime;
use mysql_async::prelude::*;
use num_bigint::BigInt;
use num_traits::{Signed, Zero};
use serde::{Deserialize, Serialize};

use super::accounts::{as_field_error, find_account, load_accounts};
use super::allocate::{BY_MANUAL, load_allocator};
use super::closings::{NOT_CLOSED_CONDITION, closed_months, closed_years};
use super::facts::{entry_months, sync_facts};
use super::rules::{AllocationRule, insert_rule, is_duplicate_rule};
use super::{ActivityRef, FactCounts, find_activity, is_year_month, month_args, placeholders};
use crate::activity::EXTERNAL_CODE_PATTERN;
use crate::audit::Recorder;
use crate::auth::AdminUser;
use crate::db::{self, Params, Tx, Value};
use crate::error::{bad_request, conflict, validation1};
use crate::http::{self, Json, Query, Validator};
use crate::{AppResult, AppState, audit, calc, jst};

/// 未割当の明細のまとまり。箱の ID がある行は箱の ID ごと、ない行は会計科目 × 部門ごと。
#[derive(Serialize)]
struct UnallocatedGroup {
    key: String,
    box_code: Option<String>,
    /// 箱の ID がないまとまりのみ
    gl_account_id: Option<i64>,
    department_code: Option<String>,
    /// 会計科目（「コード 名前」）
    accounts: Vec<String>,
    months: Vec<String>,
    count: i64,
    revenue: String,
    expense: String,
    /// 摘要の例
    description: String,
    #[serde(skip)]
    revenue_sum: BigInt,
    #[serde(skip)]
    expense_sum: BigInt,
}

#[derive(Serialize)]
struct UnallocatedResponse {
    items: Vec<UnallocatedGroup>,
    count: i64,
    revenue: String,
    expense: String,
}

type UnallocatedRow = (Option<String>, i64, String, String, Option<String>, String, String, i64, String, String);

/// GET /api/actuals/unallocated?fiscal_year=。FP&A のみ。
/// 年度を省略するとすべての月。金額の大きいまとまりから返す。
pub async fn list(State(st): State<AppState>, _: AdminUser, q: Query) -> AppResult<Response> {
    let mut filter = "";
    let mut args: Vec<Value> = Vec::new();
    let s = q.get("fiscal_year");
    if !s.is_empty() {
        let fy: i32 = s.parse().map_err(|_| bad_request("fiscal_year は数値で指定してください"))?;
        let months = calc::fiscal_months(fy);
        filter = " AND e.target_month BETWEEN ? AND ?";
        args.extend([format!("{}-01", months[0]).into(), format!("{}-01", months[months.len() - 1]).into()]);
    }
    let mut conn = st.db.get_conn().await?;
    let rows: Vec<UnallocatedRow> = conn
        .exec(
            format!(
                "SELECT e.box_code, e.gl_account_id, g.code, g.name, e.department_code, DATE_FORMAT(e.target_month, '%Y-%m'), s.category,
                        COUNT(*), CAST(SUM(e.amount) AS CHAR), MIN(COALESCE(e.description, ''))
                 FROM actual_entries e JOIN gl_accounts g ON g.id = e.gl_account_id JOIN subjects s ON s.id = e.subject_id
                 WHERE e.activity_id IS NULL{filter}
                 GROUP BY e.box_code, e.gl_account_id, g.code, g.name, e.department_code, e.target_month, s.category
                 ORDER BY e.target_month"
            ),
            Params::Positional(args),
        )
        .await?;

    let mut index: HashMap<String, usize> = HashMap::new();
    let mut items: Vec<UnallocatedGroup> = Vec::new();
    let mut count = 0;
    let mut revenue = BigInt::zero();
    let mut expense = BigInt::zero();
    for (box_code, account_id, code, name, dept, month, category, n, amount, desc) in rows {
        let key = match &box_code {
            Some(b) => format!("box:{b}"),
            None => format!("account:{account_id}:{}", dept.as_deref().unwrap_or("")),
        };
        let i = *index.entry(key.clone()).or_insert_with(|| {
            let has_box = box_code.is_some();
            items.push(UnallocatedGroup {
                key,
                box_code: box_code.clone(),
                gl_account_id: (!has_box).then_some(account_id),
                department_code: if has_box { None } else { dept.clone() },
                accounts: Vec::new(),
                months: Vec::new(),
                count: 0,
                revenue: String::new(),
                expense: String::new(),
                description: String::new(),
                revenue_sum: BigInt::zero(),
                expense_sum: BigInt::zero(),
            });
            items.len() - 1
        });
        let g = &mut items[i];
        let label = format!("{code} {name}");
        if !g.accounts.contains(&label) {
            g.accounts.push(label);
        }
        if !g.months.contains(&month) {
            g.months.push(month);
        }
        if g.description.is_empty() {
            g.description = desc;
        }
        g.count += n;
        count += n;
        let v: BigInt = amount.parse().unwrap_or_default();
        if category == "revenue" {
            g.revenue_sum += &v;
            revenue += v;
        } else {
            g.expense_sum += &v;
            expense += v;
        }
    }
    items.sort_by_cached_key(|g| std::cmp::Reverse(g.revenue_sum.abs() + g.expense_sum.abs()));
    for g in &mut items {
        g.revenue = g.revenue_sum.to_string();
        g.expense = g.expense_sum.to_string();
    }
    Ok(http::ok(&UnallocatedResponse { items, count, revenue: revenue.to_string(), expense: expense.to_string() }))
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct AssignRequest {
    box_code: Option<String>,
    gl_account_id: Option<i64>,
    department_code: Option<String>,
    /// 会計科目 × 部門のまとまりを、部門を問わない割当ルール（全部門）にする
    all_departments: bool,
    activity_id: i64,
    reason: String,
}

#[derive(Serialize, Default)]
struct AssignResult {
    /// 割り当てた明細の行数
    assigned: u64,
    /// 締めた年度の月なので割り当てなかった未割当の行数（ルールは登録する）
    skipped_closed: i64,
    months: Vec<String>,
    /// 登録した外部コード、または追加した割当ルール（どちらか一方）
    #[serde(skip_serializing_if = "Option::is_none")]
    external_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rule: Option<AllocationRule>,
    #[serde(flatten)]
    counts: FactCounts,
}

/// POST /api/actuals/unallocated/assign。FP&A のみ。
///
/// 未割当のまとまりを施策に割り当て、翌月以降も同じ施策に入るようにルールとして残す。
///   - box_code: 箱の ID を施策の外部コードとして登録し、その箱の ID の未割当の明細（すべての月）を割り当てる
///   - gl_account_id（と department_code）: 割当ルールを追加し、その会計科目 × 部門の未割当の明細を割り当てる。
///     all_departments なら部門を問わないルールにする
pub async fn assign(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Json(req): Json<AssignRequest>,
) -> AppResult<Response> {
    let mut v = Validator::new();
    let reason = req.reason.trim();
    if reason.is_empty() {
        v.add("reason", "変更理由を入力してください");
    }
    if req.box_code.is_none() == req.gl_account_id.is_none() {
        v.add("box_code", "箱の ID か会計科目のどちらかを指定してください");
    }
    if req.activity_id == 0 {
        v.add("activity_id", "割り当てる施策を選んでください");
    }
    v.check()?;

    let mut result = AssignResult::default();
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, reason).await?;
    let activity = find_activity(&mut tx, req.activity_id).await?;
    let mut filter: String;
    let mut args: Vec<Value>;
    if let Some(code) = &req.box_code {
        let code = code.trim().to_string();
        register_external_code(&mut tx, &rec, &code, &activity).await?;
        filter = "box_code = ?".into();
        args = vec![code.clone().into()];
        result.external_code = Some(code);
    } else {
        let account_id = req.gl_account_id.unwrap_or(0);
        find_account(&mut tx, account_id, "")
            .await
            .map_err(|e| as_field_error(e, "gl_account_id", "会計科目が見つかりません"))?;
        let dept = req.department_code.as_deref().filter(|d| !req.all_departments && !d.is_empty());
        let rule = insert_rule(&mut tx, &rec, account_id, dept, req.activity_id).await.map_err(|e| {
            if is_duplicate_rule(&e) {
                conflict("この会計科目・部門の割当ルールは既にあります。再割当で今のルールを当て直してください")
            } else {
                e
            }
        })?;
        result.rule = Some(rule);
        // 割当の順番（箱の ID → 割当ルール）に合わせ、箱の ID で決まらなかった行はルールで割り当てる。
        // 部門が空のルールは全部門に当たるので、部門を問わずに割り当てる
        filter = "gl_account_id = ?".into();
        args = vec![account_id.into()];
        if let Some(d) = dept {
            filter.push_str(" AND department_code = ?");
            args.push(d.into());
        }
    }

    // 締めた年度の行は割り当てない（実績の数字を変えないため）
    result.skipped_closed = db::count(
        &mut tx,
        &format!(
            "SELECT COUNT(*) FROM actual_entries WHERE activity_id IS NULL AND {filter} AND NOT {NOT_CLOSED_CONDITION}"
        ),
        Params::Positional(args.clone()),
    )
    .await?;
    filter.push_str(&format!(" AND {NOT_CLOSED_CONDITION}"));
    let months: Vec<String> = tx
        .exec(
            format!(
                "SELECT DISTINCT DATE_FORMAT(target_month, '%Y-%m') FROM actual_entries
                 WHERE activity_id IS NULL AND {filter} ORDER BY 1 FOR UPDATE"
            ),
            Params::Positional(args.clone()),
        )
        .await?;
    let mut update_args: Vec<Value> = vec![req.activity_id.into(), BY_MANUAL.into()];
    update_args.extend(args);
    tx.exec_drop(
        format!("UPDATE actual_entries SET activity_id = ?, allocated_by = ? WHERE activity_id IS NULL AND {filter}"),
        Params::Positional(update_args),
    )
    .await?;
    result.assigned = tx.affected_rows();
    sync_facts(&mut tx, &rec, &months, &mut result.counts).await?;
    result.months = months;
    tx.commit().await?;
    Ok(http::ok(&result))
}

/// 監査ログに残す外部コード（施策 API と同じ形）。
#[derive(Serialize)]
struct ExternalCodeRecord {
    id: i64,
    activity_id: i64,
    code: String,
    note: String,
    #[serde(serialize_with = "jst::ser")]
    created_at: NaiveDateTime,
    #[serde(serialize_with = "jst::ser")]
    updated_at: NaiveDateTime,
}
from_row!(ExternalCodeRecord { id, activity_id, code, note, created_at, updated_at });

/// 箱の ID を施策の外部コードとして登録する（施策 API の外部コードの登録と同じ検証）。
async fn register_external_code(tx: &mut Tx, rec: &Recorder, code: &str, activity: &ActivityRef) -> AppResult<()> {
    if !EXTERNAL_CODE_PATTERN.is_match(code) {
        return Err(validation1("box_code", "外部コードに空白・カンマ・引用符は使えません（100文字以内）"));
    }
    let name: Option<String> = tx.exec_first("SELECT name FROM activities WHERE code = ?", (code,)).await?;
    if let Some(name) = name {
        return Err(conflict(format!(
            "箱の ID {code:?} は施策「{name}」の施策コードです。再割当で今のルールを当て直してください"
        )));
    }
    let name: Option<String> = tx
        .exec_first(
            "SELECT a.name FROM activity_external_codes e JOIN activities a ON a.id = e.activity_id WHERE e.code = ?",
            (code,),
        )
        .await?;
    if let Some(name) = name {
        return Err(conflict(format!(
            "箱の ID {code:?} は施策「{name}」の外部コードとして登録済みです。再割当で今のルールを当て直してください"
        )));
    }
    tx.exec_drop(
        "INSERT INTO activity_external_codes (activity_id, code, note) VALUES (?, ?, ?)",
        (activity.id, code, "未割当の一覧から登録"),
    )
    .await?;
    let id = db::last_id(tx);
    let created: Option<ExternalCodeRecord> = tx
        .exec_first(
            "SELECT id, activity_id, code, COALESCE(note, ''), created_at, updated_at FROM activity_external_codes WHERE id = ?",
            (id,),
        )
        .await?;
    let created = created.ok_or_else(|| anyhow::anyhow!("external code {id} not found"))?;
    rec.insert(tx, "activity_external_codes", id, &created).await
}

// --- 再割当 ---

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ReallocateRequest {
    months: Vec<String>,
    reason: String,
    dry_run: bool,
}

/// 再割当による施策ごとの増減（収益・費用）。施策が None は未割当。
#[derive(Serialize)]
struct ActivityChange {
    activity: Option<ActivityRef>,
    revenue: String,
    expense: String,
}

#[derive(Serialize)]
struct ReallocateResult {
    dry_run: bool,
    months: Vec<String>,
    /// 対象の明細の行数
    entries: usize,
    /// 割当が変わった行数
    changed: usize,
    /// 対象外になった会計科目の行数
    removed: usize,
    changes: Vec<ActivityChange>,
    #[serde(flatten)]
    counts: FactCounts,
}

type EntryRow = (i64, i64, i64, String, String, String, Option<i64>, String);

/// POST /api/actuals/reallocate。FP&A のみ。
/// 指定した月の明細に、今の外部コード・割当ルール・会計科目の対応を当て直す。対象外になった会計科目の行は除く。
/// dry_run なら保存せず、施策ごとの増減だけを返す。
pub async fn reallocate(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Json(req): Json<ReallocateRequest>,
) -> AppResult<Response> {
    let mut v = Validator::new();
    let reason = req.reason.trim();
    if reason.is_empty() && !req.dry_run {
        v.add("reason", "変更理由を入力してください");
    }
    let mut month_set: BTreeSet<String> = BTreeSet::new();
    for m in &req.months {
        if !is_year_month(m) {
            v.add("months", "月は YYYY-MM 形式で指定してください");
            break;
        }
        month_set.insert(m.clone());
    }
    if month_set.is_empty() {
        v.add("months", "再割当する月を選んでください");
    }
    v.check()?;
    let months: Vec<String> = month_set.into_iter().collect();

    let mut result = ReallocateResult {
        dry_run: req.dry_run,
        months: months.clone(),
        entries: 0,
        changed: 0,
        removed: 0,
        changes: Vec::new(),
        counts: FactCounts::default(),
    };
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, reason).await?;
    let closed = closed_months(&months, &closed_years(&mut tx).await?);
    if !closed.is_empty() {
        return Err(validation1("months", format!("締めた年度の月は再割当できません: {}", closed.join("、"))));
    }
    let have = entry_months(&mut tx, &months).await?;
    let missing: Vec<&str> = months.iter().filter(|m| !have.contains(*m)).map(String::as_str).collect();
    if !missing.is_empty() {
        return Err(validation1("months", format!("明細のない月は再割当できません: {}", missing.join("、"))));
    }
    let accounts = load_accounts(&mut tx).await?;
    let al = load_allocator(&mut tx).await?;
    let rows: Vec<(i64, String)> = tx.query("SELECT id, category FROM subjects").await?;
    let categories: HashMap<i64, String> = rows.into_iter().collect();

    let list: Vec<EntryRow> = tx
        .exec(
            format!(
                "SELECT id, gl_account_id, subject_id, COALESCE(department_code, ''), COALESCE(box_code, ''), CAST(amount AS CHAR), activity_id, allocated_by
                 FROM actual_entries WHERE target_month IN ({}) FOR UPDATE",
                placeholders(months.len())
            ),
            Params::Positional(month_args(&months)),
        )
        .await?;
    result.entries = list.len();

    // 施策（0 は未割当）→（収益, 費用）の増減
    let mut changes: HashMap<i64, (BigInt, BigInt)> = HashMap::new();
    let mut moved = |activity_id: Option<i64>, subject_id: i64, amount: &BigInt, sign: i32| {
        let c = changes.entry(activity_id.unwrap_or(0)).or_default();
        let delta = amount * BigInt::from(sign);
        if categories.get(&subject_id).map(String::as_str) == Some("revenue") {
            c.0 += delta;
        } else {
            c.1 += delta;
        }
    };
    for (id, account_id, subject_id, dept, box_code, amount, activity_id, by) in list {
        let amount: BigInt = amount.parse().unwrap_or_default();
        let Some(acc) = accounts.get(&account_id) else { continue };
        if acc.is_excluded {
            tx.exec_drop("DELETE FROM actual_entries WHERE id = ?", (id,)).await?;
            moved(activity_id, subject_id, &amount, -1);
            result.removed += 1;
            continue;
        }
        let (new_activity, new_by) = al.allocate(account_id, &dept, &box_code);
        let new_subject = acc.subject_id.unwrap_or(0);
        if new_activity == activity_id && new_by == by && new_subject == subject_id {
            continue;
        }
        tx.exec_drop(
            "UPDATE actual_entries SET activity_id = ?, allocated_by = ?, subject_id = ? WHERE id = ?",
            (new_activity, new_by, new_subject, id),
        )
        .await?;
        result.changed += 1;
        if new_activity != activity_id || new_subject != subject_id {
            moved(activity_id, subject_id, &amount, -1);
            moved(new_activity, new_subject, &amount, 1);
        }
    }
    sync_facts(&mut tx, &rec, &months, &mut result.counts).await?;

    for (key, (revenue, expense)) in changes {
        if revenue.is_zero() && expense.is_zero() {
            continue;
        }
        let activity = if key == 0 { None } else { Some(find_activity(&mut tx, key).await?) };
        result.changes.push(ActivityChange { activity, revenue: revenue.to_string(), expense: expense.to_string() });
    }
    // 施策コードの順。未割当は最後
    result.changes.sort_by(|a, b| match (&a.activity, &b.activity) {
        (Some(x), Some(y)) => x.code.cmp(&y.code),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    if req.dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(http::ok(&result))
}
