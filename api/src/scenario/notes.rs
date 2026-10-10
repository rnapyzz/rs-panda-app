//! 施策 × シナリオの差異の説明と更新の完了（docs/plan.md「2.10 現場担当の動線」）。
//!
//! 状態は、完了にしていれば「完了」、数値や説明を変更していれば「入力中」、どちらもなければ「未着手」。
//! 数値（金額・ドライバー値）や説明を変更すると、変更日時を記録して完了を取り消す（入力中に戻す）。

use axum::extract::State;
use axum::response::Response;
use chrono::NaiveDateTime;
use mysql_async::prelude::*;
use serde::{Deserialize, Serialize};

use super::values::{begin_edit, path_ids, respond_values};
use super::{ReasonRequest, find_scenario};
use crate::audit::Recorder;
use crate::auth::CurrentUser;
use crate::db::{self, Tx};
use crate::http::{self, Json, PathIds, Validator};
use crate::{AppResult, AppState, jst};

/// 要因の分類。
pub const NOTE_CAUSES: [&str; 6] = ["timing", "volume", "new", "lost", "assumption", "other"];

pub const STATUS_NOT_STARTED: &str = "not_started";
pub const STATUS_IN_PROGRESS: &str = "in_progress";
pub const STATUS_COMPLETED: &str = "completed";

/// 施策 × シナリオの差異の説明と状態。
#[derive(Clone, Debug, Serialize)]
pub struct Note {
    #[serde(skip)]
    pub id: i64,
    pub scenario_id: i64,
    pub activity_id: i64,
    pub explanation: String,
    pub causes: Vec<String>,
    /// not_started / in_progress / completed
    pub status: String,
    #[serde(serialize_with = "jst::ser_opt")]
    pub completed_at: Option<NaiveDateTime>,
    pub completed_by: Option<i64>,
    pub completed_by_name: String,
    #[serde(serialize_with = "jst::ser_opt")]
    pub last_edited_at: Option<NaiveDateTime>,
}

/// 完了・変更の日時から状態を決める。
pub fn status_of(completed_at: Option<NaiveDateTime>, last_edited_at: Option<NaiveDateTime>) -> &'static str {
    if completed_at.is_some() {
        STATUS_COMPLETED
    } else if last_edited_at.is_some() {
        STATUS_IN_PROGRESS
    } else {
        STATUS_NOT_STARTED
    }
}

/// カンマ区切りの要因を一覧にする。
pub fn split_causes(s: &str) -> Vec<String> {
    if s.is_empty() { Vec::new() } else { s.split(',').map(str::to_string).collect() }
}

type NoteRow = (i64, Option<String>, String, Option<NaiveDateTime>, Option<i64>, Option<String>, Option<NaiveDateTime>);

/// 施策 × シナリオの記録を読む。なければ未着手の空の記録を返す。lock は " FOR UPDATE" など。2つ目の値はあったかどうか。
pub async fn load_note<Q: Queryable>(
    q: &mut Q,
    scenario_id: i64,
    activity_id: i64,
    lock: &str,
) -> AppResult<(Note, bool)> {
    let row: Option<NoteRow> = q
        .exec_first(
            format!(
                "SELECT n.id, n.explanation, n.causes, n.completed_at, n.completed_by, u.name, n.last_edited_at
                 FROM activity_scenario_notes n LEFT JOIN users u ON u.id = n.completed_by
                 WHERE n.scenario_id = ? AND n.activity_id = ?{lock}"
            ),
            (scenario_id, activity_id),
        )
        .await?;
    let mut n = Note {
        id: 0,
        scenario_id,
        activity_id,
        explanation: String::new(),
        causes: Vec::new(),
        status: STATUS_NOT_STARTED.into(),
        completed_at: None,
        completed_by: None,
        completed_by_name: String::new(),
        last_edited_at: None,
    };
    let Some((id, explanation, causes, completed_at, completed_by, completed_by_name, last_edited_at)) = row else {
        return Ok((n, false));
    };
    n.id = id;
    n.explanation = explanation.unwrap_or_default();
    n.causes = split_causes(&causes);
    n.completed_at = completed_at;
    n.completed_by = completed_by;
    n.completed_by_name = completed_by_name.unwrap_or_default();
    n.last_edited_at = last_edited_at;
    n.status = status_of(completed_at, last_edited_at).into();
    Ok((n, true))
}

/// 監査ログに残す記録（表示用の名前・状態は含めない）。
#[derive(Clone, Debug, Serialize)]
struct NoteRecord {
    scenario_id: i64,
    activity_id: i64,
    explanation: String,
    causes: Vec<String>,
    #[serde(serialize_with = "jst::ser_opt")]
    completed_at: Option<NaiveDateTime>,
    completed_by: Option<i64>,
}

fn record_of(n: &Note) -> NoteRecord {
    NoteRecord {
        scenario_id: n.scenario_id,
        activity_id: n.activity_id,
        explanation: n.explanation.clone(),
        causes: n.causes.clone(),
        completed_at: n.completed_at,
        completed_by: n.completed_by,
    }
}

/// 施策 × シナリオを変更したことを記録し、完了していれば取り消す（入力中に戻す）。
/// 数値・説明を変更したトランザクションの中で呼ぶ。
pub async fn mark_edited(tx: &mut Tx, rec: &Recorder, scenario_id: i64, activity_id: i64) -> AppResult<()> {
    let (before, found) = load_note(tx, scenario_id, activity_id, " FOR UPDATE").await?;
    if !found {
        tx.exec_drop(
            "INSERT INTO activity_scenario_notes (scenario_id, activity_id, last_edited_at) VALUES (?, ?, NOW())",
            (scenario_id, activity_id),
        )
        .await?;
        return Ok(());
    }
    tx.exec_drop(
        "UPDATE activity_scenario_notes SET last_edited_at = NOW(), completed_at = NULL, completed_by = NULL WHERE id = ?",
        (before.id,),
    )
    .await?;
    if before.completed_at.is_none() {
        return Ok(());
    }
    // 完了の取り消しは、変更履歴に残す
    let mut after = record_of(&before);
    after.completed_at = None;
    after.completed_by = None;
    rec.update(tx, "activity_scenario_notes", before.id, &record_of(&before), &after).await
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct NoteRequest {
    explanation: String,
    causes: Vec<String>,
    reason: String,
}

/// PUT /api/scenarios/{id}/activities/{aid}/note。差異の説明と要因の分類を保存する。
/// 権限は数値の入力と同じ。変更すると入力中に戻る。
pub async fn put_note(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    ids: PathIds,
    Json(req): Json<NoteRequest>,
) -> AppResult<Response> {
    let (scenario_id, activity_id) = path_ids(&ids)?;
    let mut v = Validator::new();
    let explanation = v.optional_text("explanation", "差異の説明", &req.explanation, 5000);
    // 定義の順にそろえる
    let causes: Vec<String> =
        NOTE_CAUSES.iter().filter(|c| req.causes.iter().any(|r| r == *c)).map(|c| c.to_string()).collect();
    if req.causes.iter().any(|c| !NOTE_CAUSES.contains(&c.as_str())) {
        v.add("causes", format!("要因の分類は {} から選んでください", NOTE_CAUSES.join(" / ")));
    }
    v.check()?;

    let (mut tx, rec, _, _) = begin_edit(&st, &u, scenario_id, activity_id, &req.reason).await?;
    let (before, found) = load_note(&mut tx, scenario_id, activity_id, " FOR UPDATE").await?;
    if !(found && before.explanation == explanation && before.causes == causes) {
        mark_edited(&mut tx, &rec, scenario_id, activity_id).await?;
        tx.exec_drop(
            "UPDATE activity_scenario_notes SET explanation = ?, causes = ? WHERE scenario_id = ? AND activity_id = ?",
            p![db::null_str(&explanation), causes.join(","), scenario_id, activity_id],
        )
        .await?;
        let (after, _) = load_note(&mut tx, scenario_id, activity_id, "").await?;
        let mut b = record_of(&before);
        // 完了の取り消しは mark_edited で記録済み
        b.completed_at = None;
        b.completed_by = None;
        rec.update(&mut tx, "activity_scenario_notes", after.id, &b, &record_of(&after)).await?;
    }
    tx.commit().await?;
    respond_values(&st, &u, scenario_id, activity_id).await
}

/// POST /api/scenarios/{id}/activities/{aid}/complete。更新を完了にする（承認ではなく状態の記録）。
pub async fn complete(
    st: State<AppState>,
    u: CurrentUser,
    ids: PathIds,
    req: Json<ReasonRequest>,
) -> AppResult<Response> {
    set_completed(st, u, ids, req, true).await
}

/// DELETE /api/scenarios/{id}/activities/{aid}/complete。完了を取り消して入力中に戻す。
pub async fn uncomplete(
    st: State<AppState>,
    u: CurrentUser,
    ids: PathIds,
    req: Json<ReasonRequest>,
) -> AppResult<Response> {
    set_completed(st, u, ids, req, false).await
}

async fn set_completed(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    ids: PathIds,
    Json(req): Json<ReasonRequest>,
    completed: bool,
) -> AppResult<Response> {
    let (scenario_id, activity_id) = path_ids(&ids)?;
    let (mut tx, rec, _, _) = begin_edit(&st, &u, scenario_id, activity_id, &req.reason).await?;
    let (mut before, found) = load_note(&mut tx, scenario_id, activity_id, " FOR UPDATE").await?;
    // すでにその状態なら何もしない
    if before.completed_at.is_some() != completed {
        if completed {
            if !found {
                // 何も変更せずに完了にする（見込を変えなかった）場合
                tx.exec_drop(
                    "INSERT INTO activity_scenario_notes (scenario_id, activity_id, last_edited_at) VALUES (?, ?, NOW())",
                    (scenario_id, activity_id),
                )
                .await?;
                before = load_note(&mut tx, scenario_id, activity_id, " FOR UPDATE").await?.0;
            }
            tx.exec_drop(
                "UPDATE activity_scenario_notes SET completed_at = NOW(), completed_by = ? WHERE id = ?",
                (u.id, before.id),
            )
            .await?;
        } else {
            tx.exec_drop(
                "UPDATE activity_scenario_notes SET completed_at = NULL, completed_by = NULL WHERE id = ?",
                (before.id,),
            )
            .await?;
        }
        let (after, _) = load_note(&mut tx, scenario_id, activity_id, "").await?;
        rec.update(&mut tx, "activity_scenario_notes", after.id, &record_of(&before), &record_of(&after)).await?;
    }
    tx.commit().await?;
    respond_values(&st, &u, scenario_id, activity_id).await
}

/// GET /api/scenarios/{id}/activities/{aid}/note
pub async fn get_note(State(st): State<AppState>, _: CurrentUser, ids: PathIds) -> AppResult<Response> {
    let (scenario_id, activity_id) = path_ids(&ids)?;
    let mut conn = st.db.get_conn().await?;
    find_scenario(&mut conn, scenario_id, "").await?;
    Ok(http::ok(&load_note(&mut conn, scenario_id, activity_id, "").await?.0))
}
