//! 施策のマイルストーン。

use axum::extract::State;
use axum::response::Response;
use chrono::{NaiveDate, NaiveDateTime};
use mysql_async::prelude::*;
use serde::{Deserialize, Serialize};

use super::{MAX_NAME_LEN, ReasonRequest, lock_editable, parse_date, require_reason};
use crate::auth::CurrentUser;
use crate::db::{self, Tx};
use crate::error::not_found;
use crate::http::{self, Json, PathIds, Validator};
use crate::{AppResult, AppState, audit, jst};

const MILESTONE_STATUSES: [&str; 4] = ["not_started", "in_progress", "completed", "delayed"];

#[derive(Clone, Debug, Serialize)]
pub struct Milestone {
    pub id: i64,
    pub activity_id: i64,
    pub name: String,
    #[serde(serialize_with = "jst::ser_date")]
    pub due_date: NaiveDate,
    pub status: String,
    #[serde(serialize_with = "jst::ser")]
    pub created_at: NaiveDateTime,
    #[serde(serialize_with = "jst::ser")]
    pub updated_at: NaiveDateTime,
}
from_row!(Milestone { id, activity_id, name, due_date, status, created_at, updated_at });

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct MilestoneRequest {
    name: String,
    due_date: String,
    status: String,
    reason: String,
}

const MILESTONE_SELECT: &str =
    "SELECT id, activity_id, name, due_date, status, created_at, updated_at FROM activity_milestones";

pub async fn list<Q: Queryable>(q: &mut Q, activity_id: i64) -> AppResult<Vec<Milestone>> {
    Ok(q.exec(format!("{MILESTONE_SELECT} WHERE activity_id = ? ORDER BY due_date, id"), (activity_id,)).await?)
}

async fn find(tx: &mut Tx, activity_id: i64, id: i64) -> AppResult<Milestone> {
    let m: Option<Milestone> = tx
        .exec_first(format!("{MILESTONE_SELECT} WHERE id = ? AND activity_id = ? FOR UPDATE"), (id, activity_id))
        .await?;
    m.ok_or_else(|| not_found("マイルストーンが見つかりません"))
}

fn validate(req: &MilestoneRequest) -> AppResult<(String, NaiveDate)> {
    let mut v = Validator::new();
    let name = v.text("name", "マイルストーン名", &req.name, MAX_NAME_LEN);
    let (due, ok) = parse_date(&mut v, "due_date", "期日", Some(&req.due_date));
    if ok && due.is_none() {
        v.add("due_date", "期日を入力してください");
    }
    if !MILESTONE_STATUSES.contains(&req.status.as_str()) {
        v.add("status", format!("ステータスは {} のいずれかを指定してください", MILESTONE_STATUSES.join(" / ")));
    }
    v.check()?;
    Ok((name, due.unwrap_or_default()))
}

/// POST /api/activities/{id}/milestones
pub async fn create(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    ids: PathIds,
    Json(mut req): Json<MilestoneRequest>,
) -> AppResult<Response> {
    let activity_id = ids.get("id")?;
    if req.status.is_empty() {
        req.status = "not_started".into();
    }
    let (name, due) = validate(&req)?;

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    lock_editable(&mut tx, &u, activity_id).await?;
    tx.exec_drop(
        "INSERT INTO activity_milestones (activity_id, name, due_date, status) VALUES (?, ?, ?, ?)",
        p![activity_id, name, due, req.status.clone()],
    )
    .await?;
    let id = db::last_id(&tx);
    let created = find(&mut tx, activity_id, id).await?;
    rec.insert(&mut tx, "activity_milestones", id, &created).await?;
    tx.commit().await?;
    Ok(http::created(&created))
}

/// PUT /api/activities/{id}/milestones/{mid}。期日を変更する場合は変更理由が必須。
pub async fn update(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    ids: PathIds,
    Json(req): Json<MilestoneRequest>,
) -> AppResult<Response> {
    let activity_id = ids.get("id")?;
    let id = ids.get("mid")?;
    let (name, due) = validate(&req)?;

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    lock_editable(&mut tx, &u, activity_id).await?;
    let before = find(&mut tx, activity_id, id).await?;
    if before.due_date != due {
        require_reason(&req.reason)?;
    }
    tx.exec_drop(
        "UPDATE activity_milestones SET name = ?, due_date = ?, status = ? WHERE id = ?",
        p![name, due, req.status.clone(), id],
    )
    .await?;
    let updated = find(&mut tx, activity_id, id).await?;
    rec.update(&mut tx, "activity_milestones", id, &before, &updated).await?;
    tx.commit().await?;
    Ok(http::ok(&updated))
}

/// DELETE /api/activities/{id}/milestones/{mid}。変更理由が必須。
pub async fn delete(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    ids: PathIds,
    Json(req): Json<ReasonRequest>,
) -> AppResult<Response> {
    let activity_id = ids.get("id")?;
    let id = ids.get("mid")?;
    require_reason(&req.reason)?;

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    lock_editable(&mut tx, &u, activity_id).await?;
    let before = find(&mut tx, activity_id, id).await?;
    tx.exec_drop("DELETE FROM activity_milestones WHERE id = ?", (id,)).await?;
    rec.delete(&mut tx, "activity_milestones", id, &before).await?;
    tx.commit().await?;
    Ok(http::no_content())
}
