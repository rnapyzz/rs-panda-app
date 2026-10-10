//! 施策と、施策に紐づくマイルストーン・ドライバー定義・金額の内訳・外部コードの API。
//!
//! 権限:
//!   - 参照: ログインユーザー全員
//!   - 作成・削除: FP&A（全ユニット）、マネージャー（担当者になっているユニットの配下）
//!   - 編集: 上記に加え、施策の担当者（manager / member）
//!
//! 変更は変更セット・監査ログに記録する。確度・前提条件・期間の変更、内訳の計算式・反映の有無の変更、
//! マイルストーン期日の変更、および削除では変更理由（reason）を必須とする。

mod driver;
mod external;
mod lines;
mod milestone;
mod transfer;

use std::collections::HashSet;

use axum::Router;
use axum::extract::State;
use axum::response::Response;
use axum::routing::{get, post, put};
use chrono::{NaiveDate, NaiveDateTime};
use mysql_async::prelude::*;
use serde::{Deserialize, Serialize};

use crate::auth::{CurrentUser, ROLE_FPA_ADMIN, ROLE_MANAGER, ROLE_MEMBER, User};
use crate::db::{self, Tx, Value};
use crate::error::{bad_request, conflict, forbidden, not_found, validation1};
use crate::http::{self, Id, Json, Query, RawBody, Validator};
use crate::{AppResult, AppState, audit, codes, jst, visibility};

pub use external::EXTERNAL_CODE_PATTERN;
pub use lines::Line;

/// ルート。権限は各ハンドラーの中で施策・ユニットごとに判定する。
/// CSV のインポート・エクスポート（/export・/import）は /{id} より先に一致する。
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/activities/export", get(transfer::export))
        .route("/api/activities/import", post(transfer::import_csv))
        .route("/api/activities", get(list).post(create))
        .route("/api/activities/{id}", get(get_one).put(update).delete(delete))
        .route("/api/activities/{id}/priority", put(set_priority))
        .route("/api/activities/{id}/watch", put(watch).delete(unwatch))
        .route("/api/activities/{id}/external-codes", post(external::create))
        .route("/api/activities/{id}/external-codes/{eid}", axum::routing::delete(external::delete))
        .route("/api/activities/{id}/milestones", post(milestone::create))
        .route("/api/activities/{id}/milestones/{mid}", put(milestone::update).delete(milestone::delete))
        .route("/api/activities/{id}/drivers", post(driver::create))
        .route("/api/activities/{id}/drivers/order", put(driver::reorder))
        .route("/api/activities/{id}/drivers/{did}", put(driver::update).delete(driver::delete))
        .route("/api/activities/{id}/lines", post(lines::create))
        .route("/api/activities/{id}/lines/{lid}", put(lines::update).delete(lines::delete))
}

// 施策タイプ・ステータス
const ACTIVITY_TYPES: [&str; 3] = ["project", "recurring", "cost_pool"];
const STATUSES: [&str; 5] = ["planned", "in_progress", "completed", "on_hold", "cancelled"];

const MAX_NAME_LEN: usize = 200;
const MAX_ASSUMPTIONS_LEN: usize = 5000;

/// 施策。
#[derive(Clone, Debug, Serialize)]
pub struct Activity {
    pub id: i64,
    pub unit_id: i64,
    pub code: String,
    pub name: String,
    pub activity_type: String,
    pub status: String,
    #[serde(serialize_with = "jst::ser_date_opt")]
    pub start_date: Option<NaiveDate>,
    #[serde(serialize_with = "jst::ser_date_opt")]
    pub end_date: Option<NaiveDate>,
    pub owner_user_id: Option<i64>,
    /// 確度の段階のコード（confidence_levels.code）
    pub confidence_level: String,
    pub assumptions: String,
    /// 重点施策（共有の印）
    pub is_priority: bool,
    #[serde(serialize_with = "jst::ser")]
    pub created_at: NaiveDateTime,
    #[serde(serialize_with = "jst::ser")]
    pub updated_at: NaiveDateTime,
    /// 所属するユニットの担当者。権限判定に使う（レスポンス・監査ログには含めない）。
    #[serde(skip)]
    pub unit_owner_id: Option<i64>,
}
from_row!(Activity {
    id,
    unit_id,
    code,
    name,
    activity_type,
    status,
    start_date,
    end_date,
    owner_user_id,
    confidence_level,
    assumptions,
    is_priority,
    created_at,
    updated_at,
    unit_owner_id
});

/// レスポンス用。ログインユーザーが編集できるかを含める。
#[derive(Serialize)]
pub struct ActivityView {
    #[serde(flatten)]
    activity: Activity,
    can_edit: bool,
    /// 施策の作成・削除・重点施策の設定ができるか
    can_manage: bool,
    /// ログインユーザーがウォッチしているか（本人にだけ見える）
    is_watched: bool,
}

fn view_of(u: &User, a: Activity, watched: bool) -> ActivityView {
    ActivityView {
        can_edit: can_edit(u, &a),
        can_manage: can_manage_unit(u, a.unit_owner_id),
        is_watched: watched,
        activity: a,
    }
}

/// 施策の詳細（マイルストーン・ドライバー・計算式を含む）。
#[derive(Serialize)]
struct ActivityDetail {
    #[serde(flatten)]
    view: ActivityView,
    external_codes: Vec<external::ExternalCode>,
    milestones: Vec<milestone::Milestone>,
    drivers: Vec<driver::Driver>,
    lines: Vec<Line>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct ActivityRequest {
    pub unit_id: i64,
    pub code: String,
    pub name: String,
    pub activity_type: String,
    pub status: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub owner_user_id: Option<i64>,
    /// 確度の段階のコード。空なら施策タイプの既定（プロジェクト型は C、それ以外は A）
    pub confidence_level: String,
    pub assumptions: String,
    pub reason: String,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ReasonRequest {
    pub reason: String,
}

const ACTIVITY_SELECT: &str = "
    SELECT a.id, a.unit_id, a.code, a.name, a.activity_type, a.status, a.start_date, a.end_date,
           a.owner_user_id, a.confidence_level, COALESCE(a.assumptions, ''), a.is_priority, a.created_at, a.updated_at,
           un.owner_user_id
    FROM activities a JOIN units un ON un.id = a.unit_id";

pub async fn find_activity<Q: Queryable>(q: &mut Q, id: i64, lock: &str) -> AppResult<Activity> {
    let a: Option<Activity> = q.exec_first(format!("{ACTIVITY_SELECT} WHERE a.id = ?{lock}"), (id,)).await?;
    a.ok_or_else(|| not_found("施策が見つかりません"))
}

async fn is_watched<Q: Queryable>(q: &mut Q, user_id: i64, activity_id: i64) -> AppResult<bool> {
    let n = db::count(
        q,
        "SELECT COUNT(*) FROM activity_watches WHERE user_id = ? AND activity_id = ?",
        p![user_id, activity_id],
    )
    .await?;
    Ok(n > 0)
}

// --- 権限 ---

/// ユニットの配下で施策を作成・削除できるかを返す。
pub fn can_manage_unit(u: &User, unit_owner_id: Option<i64>) -> bool {
    match u.role.as_str() {
        ROLE_FPA_ADMIN => true,
        ROLE_MANAGER => unit_owner_id == Some(u.id),
        _ => false,
    }
}

/// 施策を編集できるかを返す。
pub fn can_edit(u: &User, a: &Activity) -> bool {
    if can_manage_unit(u, a.unit_owner_id) {
        return true;
    }
    a.owner_user_id == Some(u.id) && (u.role == ROLE_MANAGER || u.role == ROLE_MEMBER)
}

/// 施策を行ロック付きで取得し、編集権限を確認する。
pub async fn lock_editable(tx: &mut Tx, u: &User, id: i64) -> AppResult<Activity> {
    let a = find_activity(tx, id, " FOR UPDATE").await?;
    if !can_edit(u, &a) {
        return Err(forbidden());
    }
    Ok(a)
}

/// ユニットを行ロック付きで取得し、担当者を返す。
/// 廃止したユニット（docs/plan.md「2.15」）には施策を所属させられない。
async fn lock_unit(tx: &mut Tx, id: i64) -> AppResult<Option<i64>> {
    let row: Option<(Option<i64>, bool)> =
        tx.exec_first("SELECT owner_user_id, is_archived FROM units WHERE id = ? FOR SHARE", (id,)).await?;
    match row {
        None => Err(validation1("unit_id", "ユニットが見つかりません")),
        Some((_, true)) => Err(validation1("unit_id", "廃止したユニットには施策を所属させられません")),
        Some((owner, false)) => Ok(owner),
    }
}

/// 他のモジュール（シナリオの値入力など）が使う施策の要約。
#[derive(Clone, Debug, Serialize)]
pub struct Summary {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub confidence_level: String,
    pub can_edit: bool,
}

fn summarize(u: &User, a: &Activity) -> Summary {
    Summary {
        id: a.id,
        code: a.code.clone(),
        name: a.name.clone(),
        confidence_level: a.confidence_level.clone(),
        can_edit: can_edit(u, a),
    }
}

/// 施策の要約を返す。存在しなければ 404。
pub async fn load<Q: Queryable>(q: &mut Q, u: &User, id: i64) -> AppResult<Summary> {
    Ok(summarize(u, &find_activity(q, id, "").await?))
}

/// 施策を行ロック付きで取得し、編集権限がなければ 403 を返す。
pub async fn lock_for_edit(tx: &mut Tx, u: &User, id: i64) -> AppResult<Summary> {
    Ok(summarize(u, &lock_editable(tx, u, id).await?))
}

/// 変更理由が空ならエラーを返す。
pub fn require_reason(reason: &str) -> AppResult<()> {
    if reason.trim().is_empty() {
        return Err(validation1("reason", "この変更には変更理由の入力が必要です"));
    }
    Ok(())
}

// --- 施策 ---

/// GET /api/activities。
/// クエリパラメーター unit_id / owner_user_id / activity_type / status / q（コード・名称の部分一致）、
/// priority=true（重点施策）/ watched=true（自分がウォッチしている施策）で絞り込める。
async fn list(State(st): State<AppState>, CurrentUser(u): CurrentUser, q: Query) -> AppResult<Response> {
    let mut filters: Vec<&str> = Vec::new();
    let mut args: Vec<Value> = Vec::new();
    for (f, cond) in [("unit_id", "a.unit_id = ?"), ("owner_user_id", "a.owner_user_id = ?")] {
        let s = q.get(f);
        if !s.is_empty() {
            let id: i64 = s.parse().map_err(|_| bad_request(format!("{f} は数値で指定してください")))?;
            filters.push(cond);
            args.push(id.into());
        }
    }
    for (f, cond) in [("activity_type", "a.activity_type = ?"), ("status", "a.status = ?")] {
        let s = q.get(f);
        if !s.is_empty() {
            filters.push(cond);
            args.push(s.into());
        }
    }
    if q.get("priority") == "true" {
        filters.push("a.is_priority");
    }
    if q.get("watched") == "true" {
        filters.push("EXISTS (SELECT 1 FROM activity_watches w WHERE w.activity_id = a.id AND w.user_id = ?)");
        args.push(u.id.into());
    }
    let kw = q.get("q").trim();
    if !kw.is_empty() {
        let like = format!("%{}%", escape_like(kw));
        filters.push(
            "(a.code LIKE ? OR a.name LIKE ? OR EXISTS (SELECT 1 FROM activity_external_codes e WHERE e.activity_id = a.id AND e.code LIKE ?))",
        );
        args.extend([like.clone().into(), like.clone().into(), like.into()]);
    }
    let mut sql = ACTIVITY_SELECT.to_string();
    if !filters.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&filters.join(" AND "));
    }
    sql.push_str(" ORDER BY a.code");

    let mut conn = st.db.get_conn().await?;
    let watched = watched_set(&mut conn, u.id).await?;
    let rows: Vec<Activity> = conn.exec(sql, db::Params::Positional(args)).await?;
    let items: Vec<ActivityView> = rows
        .into_iter()
        .map(|a| {
            let w = watched.contains(&a.id);
            view_of(&u, a, w)
        })
        .collect();
    Ok(http::list(&items))
}

/// GET /api/activities/{id}。マイルストーン・ドライバー・計算式を含めて返す。
async fn get_one(State(st): State<AppState>, CurrentUser(u): CurrentUser, Id(id): Id) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let a = find_activity(&mut conn, id, "").await?;
    let watched = is_watched(&mut conn, u.id, id).await?;
    let d = ActivityDetail {
        milestones: milestone::list(&mut conn, id).await?,
        drivers: driver::list(&mut conn, id).await?,
        external_codes: external::list(&mut conn, id).await?,
        lines: lines::list(&mut conn, id, &visibility::subject_filter(&u, "subject_id")).await?,
        view: view_of(&u, a, watched),
    };
    Ok(http::ok(&d))
}

/// POST /api/activities。担当者・閲覧者は 403（入力の検証より先に判定する）。
async fn create(State(st): State<AppState>, CurrentUser(u): CurrentUser, body: RawBody) -> AppResult<Response> {
    if u.role != ROLE_FPA_ADMIN && u.role != ROLE_MANAGER {
        return Err(forbidden());
    }
    let mut req: ActivityRequest = body.decode()?;
    if req.status.is_empty() {
        req.status = "planned".into();
    }
    // 施策コードが空なら自動で採番する
    let auto_code = req.code.trim().is_empty();
    let mut input = validate_activity(&req, auto_code)?;

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let unit_owner = lock_unit(&mut tx, input.unit_id).await?;
    if !can_manage_unit(&u, unit_owner) {
        return Err(forbidden());
    }
    check_owner(&mut tx, input.owner_user_id).await?;
    check_confidence_level(&mut tx, &input.confidence_level).await?;
    let result = if auto_code {
        // 同時に採番された場合に備えて、重複したら次の番号で再試行する
        let mut attempt = 0;
        loop {
            input.code = next_activity_code(&mut tx).await?;
            let r = insert_activity(&mut tx, &input).await;
            if !db::is_dup(&r) || attempt >= 4 {
                break r;
            }
            attempt += 1;
        }
    } else {
        external::check_code_not_external(&mut tx, &input.code).await?;
        insert_activity(&mut tx, &input).await
    };
    if db::is_dup(&result) {
        return Err(validation1("code", "この施策コードは既に使われています"));
    }
    result?;
    let id = db::last_id(&tx);
    let created = find_activity(&mut tx, id, "").await?;
    rec.insert(&mut tx, "activities", id, &created).await?;
    tx.commit().await?;
    Ok(http::created(&view_of(&u, created, false)))
}

async fn insert_activity(tx: &mut Tx, i: &ActivityInput) -> Result<(), mysql_async::Error> {
    tx.exec_drop(
        "INSERT INTO activities (unit_id, code, name, activity_type, status, start_date, end_date,
                                 owner_user_id, confidence_level, assumptions)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        p![
            i.unit_id,
            i.code.clone(),
            i.name.clone(),
            i.activity_type.clone(),
            i.status.clone(),
            i.start_date,
            i.end_date,
            i.owner_user_id,
            i.confidence_level.clone(),
            db::null_str(&i.assumptions)
        ],
    )
    .await
}

/// PUT /api/activities/{id}
async fn update(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    Id(id): Id,
    Json(req): Json<ActivityRequest>,
) -> AppResult<Response> {
    let input = validate_activity(&req, false)?;

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let before = lock_editable(&mut tx, &u, id).await?;
    // 別のユニットへ移す場合は、移動先のユニットで施策を作成できる権限が必要。
    if input.unit_id != before.unit_id {
        let owner = lock_unit(&mut tx, input.unit_id).await?;
        if !can_manage_unit(&u, owner) {
            return Err(forbidden());
        }
    }
    if input.code != before.code {
        external::check_code_not_external(&mut tx, &input.code).await?;
    }
    if needs_reason(&before, &input) {
        require_reason(&req.reason)?;
    }
    check_owner(&mut tx, input.owner_user_id).await?;
    check_confidence_level(&mut tx, &input.confidence_level).await?;
    let r = tx
        .exec_drop(
            "UPDATE activities SET unit_id = ?, code = ?, name = ?, activity_type = ?, status = ?,
                    start_date = ?, end_date = ?, owner_user_id = ?, confidence_level = ?, assumptions = ?
             WHERE id = ?",
            p![
                input.unit_id,
                input.code.clone(),
                input.name.clone(),
                input.activity_type.clone(),
                input.status.clone(),
                input.start_date,
                input.end_date,
                input.owner_user_id,
                input.confidence_level.clone(),
                db::null_str(&input.assumptions),
                id
            ],
        )
        .await;
    if db::is_dup(&r) {
        return Err(validation1("code", "この施策コードは既に使われています"));
    }
    r?;
    let updated = find_activity(&mut tx, id, "").await?;
    rec.update(&mut tx, "activities", id, &before, &updated).await?;
    tx.commit().await?;
    let mut conn = st.db.get_conn().await?;
    let watched = is_watched(&mut conn, u.id, id).await?;
    Ok(http::ok(&view_of(&u, updated, watched)))
}

/// DELETE /api/activities/{id}。
/// 金額・ドライバー値・シナリオ条件が登録済みの施策は、履歴を失わないよう削除できない。
/// 外部コード・マイルストーン・ドライバー定義・計算式は合わせて削除する。
async fn delete(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    Id(id): Id,
    Json(req): Json<ReasonRequest>,
) -> AppResult<Response> {
    require_reason(&req.reason)?;

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let before = find_activity(&mut tx, id, " FOR UPDATE").await?;
    if !can_manage_unit(&u, before.unit_owner_id) {
        return Err(forbidden());
    }
    for (sql, msg) in [
        ("SELECT COUNT(*) FROM budget_facts WHERE activity_id = ?", "金額データが登録されているため削除できません"),
        (
            "SELECT COUNT(*) FROM driver_values v JOIN activity_drivers d ON d.id = v.activity_driver_id WHERE d.activity_id = ?",
            "ドライバー値が登録されているため削除できません",
        ),
        (
            "SELECT COUNT(*) FROM scenario_conditions WHERE activity_id = ?",
            "シナリオの想定条件が登録されているため削除できません",
        ),
        (
            "SELECT COUNT(*) FROM activity_scenario_notes WHERE activity_id = ? AND (explanation IS NOT NULL OR causes <> '' OR completed_at IS NOT NULL)",
            "差異の説明・更新の完了が記録されているため削除できません",
        ),
    ] {
        if db::count(&mut tx, sql, p![id]).await? > 0 {
            return Err(conflict(msg));
        }
    }

    for e in external::list(&mut tx, id).await? {
        rec.delete(&mut tx, "activity_external_codes", e.id, &e).await?;
    }
    for l in lines::list(&mut tx, id, "").await? {
        rec.delete(&mut tx, "activity_lines", l.id, &l).await?;
    }
    for d in driver::list(&mut tx, id).await? {
        rec.delete(&mut tx, "activity_drivers", d.id, &d).await?;
    }
    for m in milestone::list(&mut tx, id).await? {
        rec.delete(&mut tx, "activity_milestones", m.id, &m).await?;
    }
    // 残っている施策 × シナリオの記録は、変更した日時だけ（説明・完了なし）なので一緒に消す
    for table in [
        "activity_external_codes",
        "activity_lines",
        "activity_drivers",
        "activity_milestones",
        "activity_scenario_notes",
        "activity_watches",
    ] {
        tx.exec_drop(format!("DELETE FROM {table} WHERE activity_id = ?"), (id,)).await?;
    }
    tx.exec_drop("DELETE FROM activities WHERE id = ?", (id,)).await?;
    rec.delete(&mut tx, "activities", id, &before).await?;
    tx.commit().await?;
    Ok(http::no_content())
}

/// 検証・正規化済みの入力。
#[derive(Clone, Debug)]
pub struct ActivityInput {
    pub unit_id: i64,
    pub code: String,
    pub name: String,
    pub activity_type: String,
    pub status: String,
    pub start_date: Option<NaiveDate>,
    pub end_date: Option<NaiveDate>,
    pub owner_user_id: Option<i64>,
    pub confidence_level: String,
    pub assumptions: String,
}

/// 施策の入力を検証する。allow_empty_code なら施策コードの空欄を許す（自動採番する）。
/// エラーのときも、検証した入力を返す（CSV の取込で確度の段階の確認に使う）。
pub fn validate_activity_with(req: &ActivityRequest, allow_empty_code: bool, v: &mut Validator) -> ActivityInput {
    let mut input = ActivityInput {
        unit_id: req.unit_id,
        code: req.code.trim().to_string(),
        name: v.text("name", "施策名", &req.name, MAX_NAME_LEN),
        activity_type: req.activity_type.clone(),
        status: req.status.clone(),
        start_date: None,
        end_date: None,
        owner_user_id: req.owner_user_id,
        assumptions: v.optional_text("assumptions", "前提条件", &req.assumptions, MAX_ASSUMPTIONS_LEN),
        confidence_level: req.confidence_level.trim().to_string(),
    };
    if input.unit_id <= 0 {
        v.add("unit_id", "ユニットを選択してください");
    }
    let code_ok = (allow_empty_code && input.code.is_empty()) || codes::PATTERN.is_match(&input.code);
    if !code_ok {
        v.add("code", "施策コードは半角英数字・ハイフン・アンダースコアの50文字以内で入力してください");
    }
    if !ACTIVITY_TYPES.contains(&input.activity_type.as_str()) {
        v.add("activity_type", "施策タイプは project / recurring / cost_pool のいずれかを指定してください");
    }
    if !STATUSES.contains(&input.status.as_str()) {
        v.add("status", format!("ステータスは {} のいずれかを指定してください", STATUSES.join(" / ")));
    }

    let (start, start_ok) = parse_date(v, "start_date", "開始日", req.start_date.as_deref());
    let (end, end_ok) = parse_date(v, "end_date", "終了日", req.end_date.as_deref());
    input.start_date = start;
    input.end_date = end;
    if let (true, true, Some(s), Some(e)) = (start_ok, end_ok, start, end)
        && s > e
    {
        v.add("end_date", "終了日は開始日以降にしてください");
    }
    if input.activity_type == "project" {
        if start.is_none() && start_ok {
            v.add("start_date", "プロジェクト型の施策は開始日を入力してください");
        }
        if end.is_none() && end_ok {
            v.add("end_date", "プロジェクト型の施策は終了日を入力してください");
        }
    }
    if input.confidence_level.is_empty() {
        input.confidence_level = default_confidence_level(&input.activity_type).into();
    }
    input
}

/// 施策の入力を検証する。allow_empty_code なら施策コードの空欄を許す（自動採番する）。
pub fn validate_activity(req: &ActivityRequest, allow_empty_code: bool) -> AppResult<ActivityInput> {
    let mut v = Validator::new();
    let input = validate_activity_with(req, allow_empty_code, &mut v);
    v.check()?;
    Ok(input)
}

/// 確度の段階を指定しなかったときの既定（docs/plan.md「10. 既存データの移行」と同じ）。
pub fn default_confidence_level(activity_type: &str) -> &'static str {
    if activity_type == "project" { "C" } else { "A" }
}

/// 確度の段階がマスタにあることを確認する。
async fn check_confidence_level(tx: &mut Tx, code: &str) -> AppResult<()> {
    if db::count(tx, "SELECT COUNT(*) FROM confidence_levels WHERE code = ?", p![code]).await? == 0 {
        return Err(validation1("confidence_level", format!("確度の段階「{code}」は登録されていません")));
    }
    Ok(())
}

/// 変更理由が必須になる項目（確度の段階・前提条件・期間）が変わるかを返す。
fn needs_reason(before: &Activity, input: &ActivityInput) -> bool {
    before.confidence_level != input.confidence_level
        || before.assumptions != input.assumptions
        || before.start_date != input.start_date
        || before.end_date != input.end_date
}

async fn check_owner(tx: &mut Tx, owner_id: Option<i64>) -> AppResult<()> {
    if let Some(id) = owner_id
        && !db::exists(tx, "users", id).await?
    {
        return Err(validation1("owner_user_id", "担当者が見つかりません"));
    }
    Ok(())
}

/// YYYY-MM-DD の日付を検証する。2つ目の値は形式が正しい（または未指定）かどうか。
fn parse_date(v: &mut Validator, field: &str, label: &str, s: Option<&str>) -> (Option<NaiveDate>, bool) {
    let s = s.unwrap_or("").trim();
    if s.is_empty() {
        return (None, true);
    }
    match jst::parse_date(s) {
        Some(d) => (Some(d), true),
        None => {
            v.add(field, format!("{label}は YYYY-MM-DD 形式で入力してください"));
            (None, false)
        }
    }
}

/// LIKE の特殊文字をエスケープする。
pub fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// 自動採番の次の施策コード（ACT-0001, ACT-0002, ...）を返す。
/// 外部コードと同じ値は避ける（CSV で施策を特定できなくなるため）。
async fn next_activity_code(tx: &mut Tx) -> AppResult<String> {
    codes::next(tx, "activities", external::AUTO_CODE_PREFIX, Some("activity_external_codes")).await
}

// --- 重点施策・ウォッチ（docs/plan.md「2.11」） ---

/// ユーザーがウォッチしている施策の ID を返す。
async fn watched_set<Q: Queryable>(q: &mut Q, user_id: i64) -> AppResult<HashSet<i64>> {
    let ids: Vec<i64> = q.exec("SELECT activity_id FROM activity_watches WHERE user_id = ?", (user_id,)).await?;
    Ok(ids.into_iter().collect())
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct PriorityRequest {
    is_priority: bool,
    reason: String,
}

/// PUT /api/activities/{id}/priority。重点施策の印を付け外しする。
/// 施策の作成・削除の権限（所管ユニットのマネージャー、FP&A）が必要。変更は変更履歴に残す。
async fn set_priority(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    Id(id): Id,
    Json(req): Json<PriorityRequest>,
) -> AppResult<Response> {
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let before = find_activity(&mut tx, id, " FOR UPDATE").await?;
    if !can_manage_unit(&u, before.unit_owner_id) {
        return Err(forbidden());
    }
    let updated = if before.is_priority == req.is_priority {
        before
    } else {
        tx.exec_drop("UPDATE activities SET is_priority = ? WHERE id = ?", (req.is_priority, id)).await?;
        let updated = find_activity(&mut tx, id, "").await?;
        rec.update(&mut tx, "activities", id, &before, &updated).await?;
        updated
    };
    tx.commit().await?;
    let mut conn = st.db.get_conn().await?;
    let watched = is_watched(&mut conn, u.id, id).await?;
    Ok(http::ok(&view_of(&u, updated, watched)))
}

/// PUT /api/activities/{id}/watch。ログインユーザー本人のウォッチ（気になる施策）に加える。
/// 本人にだけ見える印なので、変更履歴には残さない。
async fn watch(st: State<AppState>, u: CurrentUser, id: Id) -> AppResult<Response> {
    set_watch(st, u, id, true).await
}

/// DELETE /api/activities/{id}/watch
async fn unwatch(st: State<AppState>, u: CurrentUser, id: Id) -> AppResult<Response> {
    set_watch(st, u, id, false).await
}

async fn set_watch(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    Id(id): Id,
    on: bool,
) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    find_activity(&mut conn, id, "").await?;
    if on {
        conn.exec_drop("INSERT IGNORE INTO activity_watches (user_id, activity_id) VALUES (?, ?)", (u.id, id)).await?;
    } else {
        conn.exec_drop("DELETE FROM activity_watches WHERE user_id = ? AND activity_id = ?", (u.id, id)).await?;
    }
    Ok(http::ok(&serde_json::json!({"activity_id": id, "is_watched": on})))
}
