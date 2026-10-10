//! 組織変更の予約（org_change_plans）と、その適用。

use axum::extract::State;
use axum::response::Response;
use chrono::{NaiveDate, NaiveDateTime};
use mysql_async::prelude::*;
use mysql_async::{Conn, Pool};
use serde::{Deserialize, Serialize};

use super::{Item, KIND_CHANGE_OWNER, KIND_MERGE_UNIT, KIND_MOVE_ACTIVITY, KIND_MOVE_UNIT, RUN_INTERVAL, apply_items};
use crate::audit::Recorder;
use crate::auth::AdminUser;
use crate::db::{self, Tx};
use crate::error::{AppError, conflict, not_found, validation1};
use crate::http::{self, Id, Json, Validator};
use crate::{AppResult, AppState, audit, jst};

const MAX_ITEMS: usize = 500;

/// 組織変更の予約。
#[derive(Clone, Debug, Serialize)]
pub struct Plan {
    id: i64,
    name: String,
    effective_date: String,
    /// scheduled / applied / failed / cancelled
    status: String,
    created_by: i64,
    created_by_name: String,
    #[serde(serialize_with = "jst::ser_opt")]
    applied_at: Option<NaiveDateTime>,
    error: String,
    item_count: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    items: Vec<Item>,
    #[serde(serialize_with = "jst::ser")]
    created_at: NaiveDateTime,
    #[serde(serialize_with = "jst::ser")]
    updated_at: NaiveDateTime,
}

type PlanRow =
    (i64, String, String, String, i64, String, Option<NaiveDateTime>, String, i64, NaiveDateTime, NaiveDateTime);

const PLAN_SELECT: &str = "SELECT p.id, p.name, DATE_FORMAT(p.effective_date, '%Y-%m-%d'), p.status, p.created_by, u.name, p.applied_at,
           COALESCE(p.error, ''), (SELECT COUNT(*) FROM org_change_items i WHERE i.plan_id = p.id), p.created_at, p.updated_at
    FROM org_change_plans p JOIN users u ON u.id = p.created_by";

fn plan_of(r: PlanRow) -> Plan {
    let (
        id,
        name,
        effective_date,
        status,
        created_by,
        created_by_name,
        applied_at,
        error,
        item_count,
        created_at,
        updated_at,
    ) = r;
    Plan {
        id,
        name,
        effective_date,
        status,
        created_by,
        created_by_name,
        applied_at,
        error,
        item_count,
        items: Vec::new(),
        created_at,
        updated_at,
    }
}

/// 予約と変更を読む。lock は " FOR UPDATE" など。
async fn find_plan<Q: Queryable>(q: &mut Q, id: i64, lock: &str) -> AppResult<Plan> {
    let r: Option<PlanRow> = q.exec_first(format!("{PLAN_SELECT} WHERE p.id = ?{lock}"), (id,)).await?;
    let mut p = plan_of(r.ok_or_else(|| not_found("組織変更の予約が見つかりません"))?);
    p.items = q
        .exec(
            "SELECT id, kind, activity_id, unit_id, target_unit_id, segment_id, organization_id, owner_user_id
             FROM org_change_items WHERE plan_id = ? ORDER BY sort_order, id",
            (id,),
        )
        .await?;
    Ok(p)
}

/// GET /api/org-change-plans。有効日の新しい順。
pub async fn list(State(st): State<AppState>, _: AdminUser) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let rows: Vec<PlanRow> = conn.query(format!("{PLAN_SELECT} ORDER BY p.effective_date DESC, p.id DESC")).await?;
    Ok(http::list(&rows.into_iter().map(plan_of).collect::<Vec<_>>()))
}

/// GET /api/org-change-plans/{id}
pub async fn get_one(State(st): State<AppState>, _: AdminUser, Id(id): Id) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    Ok(http::ok(&find_plan(&mut conn, id, "").await?))
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct PlanRequest {
    name: String,
    effective_date: String,
    items: Vec<Item>,
    reason: String,
}

/// 変更の種類ごとに必要な項目があるかを確かめる。
fn check_shape(it: &Item) -> Option<&'static str> {
    match it.kind.as_str() {
        KIND_MOVE_ACTIVITY if it.activity_id.is_none() || it.target_unit_id.is_none() => {
            Some("施策と移動先のユニットを選んでください")
        }
        KIND_MOVE_UNIT if it.unit_id.is_none() || it.segment_id.is_none() || it.organization_id.is_none() => {
            Some("ユニットと、所属先のセグメント・組織を選んでください")
        }
        KIND_MERGE_UNIT if it.unit_id.is_none() || it.target_unit_id.is_none() => {
            Some("ユニットと統合先のユニットを選んでください")
        }
        KIND_CHANGE_OWNER if it.activity_id.is_none() == it.unit_id.is_none() => {
            Some("施策かユニットのどちらかを選んでください")
        }
        KIND_MOVE_ACTIVITY | KIND_MOVE_UNIT | KIND_MERGE_UNIT | KIND_CHANGE_OWNER => None,
        _ => Some("変更の種類は move_activity / move_unit / merge_unit / change_owner のいずれかです"),
    }
}

/// 入力の形を確かめ、保存する名前と有効日を返す（対象があるか・適用できるかは validate_applicable で確かめる）。
fn validate_plan(req: &PlanRequest, today: NaiveDate) -> AppResult<(String, NaiveDate)> {
    let mut v = Validator::new();
    let name = v.text("name", "名前", &req.name, 100);
    let date = jst::parse_date(req.effective_date.trim());
    match date {
        None => v.add("effective_date", "有効日は YYYY-MM-DD の形式で入力してください"),
        Some(d) if d <= today => v.add(
            "effective_date",
            "有効日は明日以降の日付にしてください（今日から有効にするには、作ってから「今すぐ適用」します）",
        ),
        Some(_) => {}
    }
    if req.items.is_empty() {
        v.add("items", "変更を1つ以上登録してください");
    } else if req.items.len() > MAX_ITEMS {
        v.add("items", format!("変更は {MAX_ITEMS} 件までです"));
    }
    if let Some((i, msg)) = req.items.iter().enumerate().find_map(|(i, it)| check_shape(it).map(|m| (i, m))) {
        v.add("items", format!("{} 件目の変更: {msg}", i + 1));
    }
    v.check()?;
    Ok((name, date.unwrap_or(today)))
}

/// 今の状態で予約を適用できるかを、同じトランザクションの中で適用してセーブポイントまで戻して確かめる
/// （確かめてから保存するまでの間に、ほかの変更が入らないように）。
async fn validate_applicable(tx: &mut Tx, rec: &Recorder, items: &[Item]) -> AppResult<()> {
    tx.query_drop("SAVEPOINT validate_plan").await?;
    if let Some(msg) = apply_items(tx, rec, items).await? {
        return Err(validation1("items", msg));
    }
    tx.query_drop("ROLLBACK TO SAVEPOINT validate_plan").await?;
    Ok(())
}

async fn save_items(tx: &mut Tx, plan_id: i64, items: &[Item]) -> AppResult<()> {
    tx.exec_drop("DELETE FROM org_change_items WHERE plan_id = ?", (plan_id,)).await?;
    for (i, it) in items.iter().enumerate() {
        tx.exec_drop(
            "INSERT INTO org_change_items (plan_id, sort_order, kind, activity_id, unit_id, target_unit_id, segment_id, organization_id, owner_user_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            p![plan_id, i, it.kind.clone(), it.activity_id, it.unit_id, it.target_unit_id, it.segment_id, it.organization_id, it.owner_user_id],
        )
        .await?;
    }
    Ok(())
}

/// POST /api/org-change-plans。今の状態で適用できない予約は作れない。
pub async fn create(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Json(req): Json<PlanRequest>,
) -> AppResult<Response> {
    let (name, date) = validate_plan(&req, jst::today())?;
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    validate_applicable(&mut tx, &rec, &req.items).await?;
    tx.exec_drop(
        "INSERT INTO org_change_plans (name, effective_date, created_by) VALUES (?, ?, ?)",
        (name, date, u.id),
    )
    .await?;
    let id = db::last_id(&tx);
    save_items(&mut tx, id, &req.items).await?;
    let created = find_plan(&mut tx, id, "").await?;
    rec.insert(&mut tx, "org_change_plans", id, &created).await?;
    tx.commit().await?;
    Ok(http::created(&created))
}

/// PUT /api/org-change-plans/{id}。予約中・失敗の予約だけを変えられる（変えると予約中に戻る）。
pub async fn update(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(req): Json<PlanRequest>,
) -> AppResult<Response> {
    let (name, date) = validate_plan(&req, jst::today())?;
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let before = find_plan(&mut tx, id, " FOR UPDATE").await?;
    if before.status != "scheduled" && before.status != "failed" {
        return Err(conflict("適用済み・取り消した予約は変更できません"));
    }
    validate_applicable(&mut tx, &rec, &req.items).await?;
    tx.exec_drop(
        "UPDATE org_change_plans SET name = ?, effective_date = ?, status = 'scheduled', error = NULL WHERE id = ?",
        (name, date, id),
    )
    .await?;
    save_items(&mut tx, id, &req.items).await?;
    let updated = find_plan(&mut tx, id, "").await?;
    rec.update(&mut tx, "org_change_plans", id, &before, &updated).await?;
    tx.commit().await?;
    Ok(http::ok(&updated))
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ReasonRequest {
    pub(super) reason: String,
}

/// DELETE /api/org-change-plans/{id}。予約中・失敗・取り消しの予約を削除する（適用済みは履歴として残す）。
pub async fn delete(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(req): Json<ReasonRequest>,
) -> AppResult<Response> {
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let p = find_plan(&mut tx, id, " FOR UPDATE").await?;
    if p.status == "applied" {
        return Err(conflict("適用済みの予約は削除できません"));
    }
    tx.exec_drop("DELETE FROM org_change_plans WHERE id = ?", (id,)).await?;
    rec.delete(&mut tx, "org_change_plans", id, &p).await?;
    tx.commit().await?;
    Ok(http::no_content())
}

/// POST /api/org-change-plans/{id}/cancel。予約中・失敗の予約を取り消す。
pub async fn cancel(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(req): Json<ReasonRequest>,
) -> AppResult<Response> {
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let p = find_plan(&mut tx, id, " FOR UPDATE").await?;
    if p.status != "scheduled" && p.status != "failed" {
        return Err(conflict("取り消せるのは、予約中・失敗の予約だけです"));
    }
    tx.exec_drop("UPDATE org_change_plans SET status = 'cancelled' WHERE id = ?", (id,)).await?;
    let after = Plan { status: "cancelled".into(), ..p.clone() };
    rec.update(&mut tx, "org_change_plans", id, &p, &after).await?;
    tx.commit().await?;
    let mut conn = st.db.get_conn().await?;
    Ok(http::ok(&find_plan(&mut conn, id, "").await?))
}

/// POST /api/org-change-plans/{id}/apply。予約中・失敗の予約を今すぐ適用する。適用は押した人の変更セットとして残す。
/// 適用できなければ 422 で理由を返し、予約を失敗にする。
pub async fn apply_now(State(st): State<AppState>, AdminUser(u): AdminUser, Id(id): Id) -> AppResult<Response> {
    apply(&st.db, id, None, Some(u.id)).await?;
    let mut conn = st.db.get_conn().await?;
    Ok(http::ok(&find_plan(&mut conn, id, "").await?))
}

/// 予約を適用する。actor は変更セットに残す人（None なら予約を作った人。自動の適用）。
/// 失敗したら全体をロールバックし、予約を失敗にして理由を残し、FP&A 全員にお知らせを送る。
/// due に今日を渡すと、有効日が今日以前の予約中の予約だけを適用する（自動の適用。それ以外は何もしない）。
async fn apply(pool: &Pool, id: i64, due: Option<NaiveDate>, actor: Option<i64>) -> AppResult<()> {
    let mut conn = pool.get_conn().await?;
    let p = find_plan(&mut conn, id, "").await?;
    drop(conn);
    let reason = format!("組織変更の予約「{}」を適用", p.name);
    let mut tx = db::begin(pool).await?;
    let rec = audit::begin(&mut tx, actor.unwrap_or(p.created_by), None, &reason).await?;
    // 二重の適用を防ぐため、予約をロックしてから状態を確かめる
    let cur = find_plan(&mut tx, id, " FOR UPDATE").await?;
    if let Some(today) = due
        && (cur.status != "scheduled" || cur.effective_date > today.format("%Y-%m-%d").to_string())
    {
        tx.rollback().await?;
        return Ok(());
    }
    if cur.status != "scheduled" && cur.status != "failed" {
        return Err(conflict("適用できるのは、予約中・失敗の予約だけです"));
    }
    if let Some(msg) = apply_items(&mut tx, &rec, &cur.items).await? {
        // 予約の行のロックを先に外してから、失敗を記録する
        tx.rollback().await?;
        mark_failed(pool, &p, &msg).await?;
        return Err(validation1("items", msg));
    }
    tx.exec_drop(
        "UPDATE org_change_plans SET status = 'applied', applied_at = NOW(), error = NULL WHERE id = ?",
        (id,),
    )
    .await?;
    let after = Plan { status: "applied".into(), ..cur.clone() };
    rec.update(&mut tx, "org_change_plans", id, &cur, &after).await?;
    tx.commit().await?;
    Ok(())
}

/// 予約を失敗にし、FP&A 全員にアプリ内のお知らせを送る。
async fn mark_failed(pool: &Pool, p: &Plan, msg: &str) -> AppResult<()> {
    let mut conn: Conn = pool.get_conn().await?;
    conn.exec_drop("UPDATE org_change_plans SET status = 'failed', error = ? WHERE id = ?", (msg, p.id)).await?;
    conn.exec_drop(
        "INSERT INTO notifications (user_id, kind, scenario_id, title, body, link)
         SELECT id, 'org_change_failed', NULL, ?, ?, '/admin/org-changes' FROM users WHERE role = 'fpa_admin' AND is_active",
        (
            format!("組織変更の予約「{}」を適用できませんでした", p.name),
            format!(
                "有効日 {} の予約を適用できなかったため、何も変更していません。原因を直して「今すぐ適用」してください。\n{msg}",
                p.effective_date
            ),
        ),
    )
    .await?;
    Ok(())
}

/// 有効日が today 以前の予約中の予約を、有効日の順に適用する。適用できなかった予約は失敗にして次へ進む。
pub async fn apply_due(pool: &Pool, today: NaiveDate) -> AppResult<()> {
    let mut conn = pool.get_conn().await?;
    let ids: Vec<i64> = conn
        .exec(
            "SELECT id FROM org_change_plans WHERE status = 'scheduled' AND effective_date <= ? ORDER BY effective_date, id",
            (today,),
        )
        .await?;
    drop(conn);
    for id in ids {
        match apply(pool, id, Some(today), None).await {
            Ok(()) => {}
            Err(e @ AppError::Api(_)) => tracing::warn!(plan_id = id, error = ?e, "org change: plan failed"),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// 1分ごとに apply_due を呼ぶ（サーバーが止まるまで続ける）。
pub async fn run(pool: Pool) {
    let mut ticker = tokio::time::interval(RUN_INTERVAL);
    loop {
        ticker.tick().await;
        if let Err(e) = apply_due(&pool, jst::today()).await {
            tracing::error!(error = ?e, "org change: apply due");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        let item = |kind: &str| Item { kind: kind.into(), ..Default::default() };
        assert!(check_shape(&item("move_activity")).is_some());
        assert!(
            check_shape(&Item { activity_id: Some(1), target_unit_id: Some(2), ..item("move_activity") }).is_none()
        );
        // 担当者の変更は施策かユニットのどちらか一方
        assert!(check_shape(&Item { activity_id: Some(1), unit_id: Some(2), ..item("change_owner") }).is_some());
        assert!(check_shape(&Item { unit_id: Some(2), ..item("change_owner") }).is_none());
        assert!(check_shape(&item("rename")).is_some());
    }

    #[test]
    fn plan_dates() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 11).unwrap();
        let req = |date: &str| PlanRequest {
            name: "x".into(),
            effective_date: date.into(),
            items: vec![Item { kind: "change_owner".into(), unit_id: Some(1), ..Default::default() }],
            reason: String::new(),
        };
        assert!(validate_plan(&req("2026-10-11"), today).is_err());
        assert!(validate_plan(&req("2026/10/12"), today).is_err());
        assert_eq!(
            validate_plan(&req(" 2026-10-12 "), today).unwrap().1,
            NaiveDate::from_ymd_opt(2026, 10, 12).unwrap()
        );
    }
}
