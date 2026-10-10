//! シナリオの切り替え（docs/plan.md「2.16」）。
//! 月次の見込を始める・新年度の期初計画を始める を、1つの変更セット（全部か無しか）で行う。
//! dry_run なら保存せず、変わる内容と注意を返す。注意があっても実行できる。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use mysql_async::prelude::*;
use serde::{Deserialize, Serialize};

use super::home::target_condition;
use super::{
    Scenario, ScenarioInsert, deactivate_current, find_scenario, insert_scenario, lock_scenario, month_date,
    scenario_where, take_role, validate_deadline,
};
use crate::audit::Recorder;
use crate::auth::{AdminUser, User};
use crate::db::{self, Tx, Value};
use crate::error::validation1;
use crate::http::{self, Json, Validator};
use crate::{AppResult, AppState, actual, audit, calc};

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct CycleRequest {
    name: String,
    /// 月次のみ（YYYY-MM）
    actual_through: String,
    /// 新年度のみ
    fiscal_year: i32,
    update_deadline: String,
    reason: String,
    dry_run: bool,
}

/// 変わる内容に出すシナリオ。
#[derive(Clone, Debug, Serialize)]
struct CycleRef {
    id: i64,
    name: String,
}

fn cycle_ref(s: &Scenario) -> Option<CycleRef> {
    Some(CycleRef { id: s.id, name: s.name.clone() })
}

#[derive(Serialize)]
struct CycleResult {
    dry_run: bool,
    /// ロックする（した）版。すでにロック済みなら None
    lock: Option<CycleRef>,
    /// 複製元（月次のみ）
    base: Option<CycleRef>,
    /// エイリアスを付け替えられる版
    role_from: Option<CycleRef>,
    /// 新しい版（dry run では作った前提の内容）
    scenario: Option<Scenario>,
    warnings: Vec<String>,
}

/// 作成中のシナリオを行ロック付きで返す（なければ None）。
async fn active_scenario(tx: &mut Tx) -> AppResult<Option<Scenario>> {
    scenario_where(tx, "WHERE is_active FOR UPDATE", p![]).await
}

/// その年度でエイリアス role を持つシナリオを返す（なければ None）。
async fn role_holder(tx: &mut Tx, fiscal_year: i32, role: &str) -> AppResult<Option<Scenario>> {
    scenario_where(tx, "WHERE fiscal_year = ? AND plan_role = ? FOR UPDATE", p![fiscal_year, role]).await
}

/// シナリオの更新の対象のうち、更新を完了にしていない施策の数（docs/plan.md「2.10」の未着手・入力中）。
async fn incomplete_count(tx: &mut Tx, scenario_id: i64) -> AppResult<i64> {
    db::count(
        tx,
        &format!(
            "SELECT COUNT(*) FROM activities a
             LEFT JOIN activity_scenario_notes n ON n.activity_id = a.id AND n.scenario_id = ?
             WHERE n.completed_at IS NULL AND {}",
            target_condition("a")
        ),
        p![scenario_id, scenario_id],
    )
    .await
}

/// 月（YYYY-MM）の年度（4月開始）。
pub fn fiscal_year_of(month: &str) -> i32 {
    let y: i32 = month[..4].parse().unwrap_or(0);
    let m: i32 = month[5..7].parse().unwrap_or(0);
    if m < 4 { y - 1 } else { y }
}

/// YYYY-MM の形式かを返す。
pub fn is_year_month(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 7 || b[4] != b'-' || !b.iter().enumerate().all(|(i, c)| i == 4 || c.is_ascii_digit()) {
        return false;
    }
    matches!(s[5..].parse::<u32>(), Ok(1..=12))
}

/// POST /api/scenarios/start-monthly。月次の見込を始める。
pub async fn start_monthly(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Json(req): Json<CycleRequest>,
) -> AppResult<Response> {
    let mut v = Validator::new();
    let name = v.text("name", "シナリオ名", &req.name, 100);
    let through = req.actual_through.trim().to_string();
    if !is_year_month(&through) {
        v.add("actual_through", "決算確定月は YYYY-MM の形式で指定してください");
    }
    let deadline = validate_deadline(&mut v, &req.update_deadline);
    v.check()?;
    let fy = fiscal_year_of(&through);

    let (mut tx, rec, mut res) = begin_cycle(&st, &u, &req).await?;
    // 複製元: 作成中の版。なければその年度の「最新見込」の版
    let mut base = match active_scenario(&mut tx).await? {
        Some(b) if b.fiscal_year != fy => {
            return Err(validation1(
                "actual_through",
                format!(
                    "作成中の版（{}年度）と同じ年度の月を指定してください。年度の切り替えは「新年度の期初計画を始める」で行います",
                    b.fiscal_year
                ),
            ));
        }
        Some(b) => b,
        None => role_holder(&mut tx, fy, "latest").await?.ok_or_else(|| {
            validation1(
                "actual_through",
                format!("{fy}年度に、作成中の版も「最新見込」の版もありません。シナリオを作成してから始めてください"),
            )
        })?,
    };
    if let Some(bt) = &base.actual_through
        && through < *bt
    {
        return Err(validation1(
            "actual_through",
            format!("決算確定月は、複製元の決算確定月（{bt}）以降にしてください"),
        ));
    }
    res.base = cycle_ref(&base);

    // 注意
    let months = calc::fiscal_months(fy);
    let mut missing = Vec::new();
    for m in months.iter().take_while(|m| **m <= through) {
        let one: Option<i64> =
            tx.exec_first("SELECT 1 FROM actual_facts WHERE target_month = ? LIMIT 1", (format!("{m}-01"),)).await?;
        if one.is_none() {
            missing.push(m.clone());
        }
    }
    if !missing.is_empty() {
        res.warnings.push(format!("決算確定月までに、実績を取り込んでいない月があります: {}", missing.join("、")));
    }
    let unallocated = db::count(
        &mut tx,
        "SELECT COUNT(*) FROM actual_facts WHERE activity_id IS NULL AND target_month BETWEEN ? AND ?",
        p![format!("{}-01", months[0]), format!("{}-01", months[11])],
    )
    .await?;
    if unallocated > 0 {
        res.warnings.push(format!("{fy}年度に未割当の実績があります（{unallocated} 件）。実績の割当で解消できます"));
    }
    if base.is_active {
        let n = incomplete_count(&mut tx, base.id).await?;
        if n > 0 {
            res.warnings
                .push(format!("今の作成中の版「{}」で、更新を完了にしていない施策が {n} 件あります", base.name));
        }
    }
    let drifted = actual::drifted_scenarios(&mut tx, fy).await?;
    if !drifted.is_empty() {
        res.warnings.push(format!("ロック済みの版に「実績の修正あり」があります: {}", drifted.join("、")));
    }

    // 1. 複製元をロックする
    if !base.is_locked {
        res.lock = cycle_ref(&base);
        base = lock_scenario(&mut tx, &rec, &base).await?;
    }
    // 4. エイリアス「最新見込」を付け替える
    if let Some(holder) = role_holder(&mut tx, fy, "latest").await? {
        res.role_from = cycle_ref(&holder);
    }
    take_role(&mut tx, &rec, fy, "latest", 0).await?;
    // 2・3・5. 複製して決算確定月・前回見込・締切を設定する
    let created = insert_scenario(
        &mut tx,
        &rec,
        ScenarioInsert {
            name,
            fiscal_year: fy,
            role: "latest".into(),
            through: month_date(&through).into(),
            base: Some(base.id),
            previous: Some(base.id),
            deadline,
            created_by: u.id,
        },
    )
    .await?;
    calc::recalculate_scenario(&mut tx, &rec, created.id).await?;
    finish_cycle(&st, tx, &rec, created, res).await
}

/// POST /api/scenarios/start-fiscal-year。新年度の期初計画を始める。
pub async fn start_fiscal_year(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Json(req): Json<CycleRequest>,
) -> AppResult<Response> {
    let mut v = Validator::new();
    let name = v.text("name", "シナリオ名", &req.name, 100);
    if !(2000..=2100).contains(&req.fiscal_year) {
        v.add("fiscal_year", "年度を正しく入力してください（例: 2027）");
    }
    let deadline = validate_deadline(&mut v, &req.update_deadline);
    v.check()?;
    let fy = req.fiscal_year;

    let (mut tx, rec, mut res) = begin_cycle(&st, &u, &req).await?;
    let closed =
        db::count(&mut tx, "SELECT COUNT(*) FROM fiscal_year_closings WHERE fiscal_year = ?", p![fy - 1]).await?;
    if closed == 0 {
        res.warnings.push(format!(
            "前の年度（{}年度）を締めていません。会計の決算が確定したら、シナリオ管理で締めてください",
            fy - 1
        ));
    }
    // 1. 今の作成中の版をロックする
    if let Some(cur) = active_scenario(&mut tx).await? {
        let n = incomplete_count(&mut tx, cur.id).await?;
        if n > 0 {
            res.warnings.push(format!("今の作成中の版「{}」で、更新を完了にしていない施策が {n} 件あります", cur.name));
        }
        res.lock = cycle_ref(&cur);
        lock_scenario(&mut tx, &rec, &cur).await?;
    }
    // 2. 空の版を「期初計画」として作る
    if let Some(holder) = role_holder(&mut tx, fy, "initial").await? {
        res.role_from = cycle_ref(&holder);
        res.warnings.push(format!("{fy}年度の「期初計画」のエイリアスを「{}」から付け替えます", holder.name));
    }
    take_role(&mut tx, &rec, fy, "initial", 0).await?;
    let created = insert_scenario(
        &mut tx,
        &rec,
        ScenarioInsert {
            name,
            fiscal_year: fy,
            role: "initial".into(),
            through: Value::NULL,
            base: None,
            previous: None,
            deadline,
            created_by: u.id,
        },
    )
    .await?;
    finish_cycle(&st, tx, &rec, created, res).await
}

/// 切り替えを1つの変更セットで始める。
async fn begin_cycle(st: &AppState, u: &User, req: &CycleRequest) -> AppResult<(Tx, Recorder, CycleResult)> {
    let mut reason = req.reason.trim().to_string();
    if reason.is_empty() {
        reason = format!("シナリオの切り替え（{}）", req.name.trim());
    }
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &reason).await?;
    let res = CycleResult {
        dry_run: req.dry_run,
        lock: None,
        base: None,
        role_from: None,
        scenario: None,
        warnings: Vec::new(),
    };
    Ok((tx, rec, res))
}

/// 新しい版を作成中にして、保存（dry run ならロールバック）する。締切があれば、保存の後に「更新の開始」を送る。
async fn finish_cycle(
    st: &AppState,
    mut tx: Tx,
    rec: &Recorder,
    created: Scenario,
    mut res: CycleResult,
) -> AppResult<Response> {
    // 6. 新しい版を作成中に指定する（前の作成中はロックで外れている）
    deactivate_current(&mut tx, rec).await?;
    tx.exec_drop("UPDATE scenarios SET is_active = TRUE WHERE id = ?", (created.id,)).await?;
    let active = find_scenario(&mut tx, created.id, "").await?;
    rec.update(&mut tx, "scenarios", created.id, &created, &active).await?;
    let deadline = active.update_deadline.is_some();
    let id = active.id;
    res.scenario = Some(active);
    if res.dry_run {
        tx.rollback().await?;
        return Ok(http::ok(&res));
    }
    tx.commit().await?;
    // 7. 締切があれば「更新の開始」を送る
    if deadline {
        st.notify.update_started(id).await;
    }
    Ok(http::json(StatusCode::CREATED, &res))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months() {
        assert!(is_year_month("2026-09"));
        assert!(!is_year_month("2026-13"));
        assert!(!is_year_month("2026/09"));
        assert_eq!(fiscal_year_of("2027-03"), 2026);
        assert_eq!(fiscal_year_of("2026-04"), 2026);
    }
}
