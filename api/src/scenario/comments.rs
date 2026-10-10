//! 説明へのコメント（docs/plan.md「2.24」）。施策 × シナリオの今回の見込の説明に、関係者がコメントを書いてやり取りする。
//!
//! 施策を見られる人（ログインユーザー全員）が書ける。ロック済みのシナリオには書けない・消せない。
//! コメントは値ではないので、変更セット・監査ログには残さず、更新の状態（未着手・入力中・完了）も変えない。
//! 書くと、施策の担当者（いなければユニットのマネージャー）と、そのやり取りにすでに書いた人にアプリ内のお知らせを送る。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use chrono::NaiveDateTime;
use mysql_async::prelude::*;
use serde::{Deserialize, Serialize};

use super::find_scenario;
use super::values::path_ids;
use crate::auth::{CurrentUser, User};
use crate::error::{conflict, forbidden, not_found};
use crate::http::{self, Json, PathIds, Validator};
use crate::{AppResult, AppState, activity, db, jst};

const COMMENT_MAX_LEN: usize = 1000;

/// コメントの1件。消したコメントは本文を返さない。
#[derive(Serialize)]
struct Comment {
    id: i64,
    user_id: i64,
    user_name: String,
    body: String,
    deleted: bool,
    #[serde(serialize_with = "jst::ser")]
    created_at: NaiveDateTime,
    /// ログインユーザーが消せるか（書いた本人で、シナリオがロックされていない）
    can_delete: bool,
}

#[derive(Serialize)]
struct CommentList {
    items: Vec<Comment>,
    /// コメントを書けるか（シナリオがロックされていない）
    can_post: bool,
}

/// GET /api/scenarios/{id}/activities/{aid}/comments
pub async fn list(State(st): State<AppState>, CurrentUser(u): CurrentUser, ids: PathIds) -> AppResult<Response> {
    let (scenario_id, activity_id) = path_ids(&ids)?;
    respond_comments(&st, &u, scenario_id, activity_id, StatusCode::OK).await
}

async fn respond_comments(
    st: &AppState,
    u: &User,
    scenario_id: i64,
    activity_id: i64,
    status: StatusCode,
) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let s = find_scenario(&mut conn, scenario_id, "").await?;
    activity::load(&mut conn, u, activity_id).await?;
    let rows: Vec<(i64, i64, String, String, bool, NaiveDateTime)> = conn
        .exec(
            "SELECT c.id, c.user_id, u.name, c.body, c.deleted_at IS NOT NULL, c.created_at
             FROM activity_scenario_comments c JOIN users u ON u.id = c.user_id
             WHERE c.scenario_id = ? AND c.activity_id = ?
             ORDER BY c.id",
            (scenario_id, activity_id),
        )
        .await?;
    let items = rows
        .into_iter()
        .map(|(id, user_id, user_name, body, deleted, created_at)| Comment {
            id,
            user_id,
            user_name,
            body: if deleted { String::new() } else { body },
            deleted,
            created_at,
            can_delete: !deleted && !s.is_locked && user_id == u.id,
        })
        .collect();
    Ok(http::json(status, &CommentList { items, can_post: !s.is_locked }))
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct CommentRequest {
    body: String,
}

/// POST /api/scenarios/{id}/activities/{aid}/comments
pub async fn post(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    ids: PathIds,
    Json(req): Json<CommentRequest>,
) -> AppResult<Response> {
    let (scenario_id, activity_id) = path_ids(&ids)?;
    let mut v = Validator::new();
    let body = v.text("body", "コメント", &req.body, COMMENT_MAX_LEN);
    v.check()?;

    let mut tx = db::begin(&st.db).await?;
    let s = find_scenario(&mut tx, scenario_id, " FOR SHARE").await?;
    if s.is_locked {
        return Err(conflict("ロックされたシナリオにはコメントを書けません"));
    }
    let a = activity::load(&mut tx, &u, activity_id).await?;
    // お知らせの宛先: 担当者（いなければユニットのマネージャー）と、すでに書いた人。書いた本人と無効なユーザーは除く
    let recipients: Vec<i64> = tx
        .exec(
            "SELECT DISTINCT r.user_id FROM (
                 SELECT COALESCE(a.owner_user_id, un.owner_user_id) AS user_id
                 FROM activities a JOIN units un ON un.id = a.unit_id WHERE a.id = ?
                 UNION
                 SELECT user_id FROM activity_scenario_comments WHERE scenario_id = ? AND activity_id = ?
             ) r JOIN users us ON us.id = r.user_id
             WHERE us.is_active AND r.user_id <> ?
             ORDER BY r.user_id",
            (activity_id, scenario_id, activity_id, u.id),
        )
        .await?;
    tx.exec_drop(
        "INSERT INTO activity_scenario_comments (scenario_id, activity_id, user_id, body) VALUES (?, ?, ?, ?)",
        (scenario_id, activity_id, u.id, body.clone()),
    )
    .await?;
    let title = format!("「{}」の説明に{}さんがコメントしました", a.name, u.name);
    let text = format!("{}（{}）\n{}", a.name, s.name, excerpt(&body, 200));
    let link = format!("/activities/{activity_id}?tab=update&scenario={scenario_id}");
    for id in recipients {
        tx.exec_drop(
            "INSERT INTO notifications (user_id, kind, scenario_id, title, body, link) VALUES (?, 'comment', ?, ?, ?, ?)",
            (id, scenario_id, title.clone(), text.clone(), link.clone()),
        )
        .await?;
    }
    tx.commit().await?;
    respond_comments(&st, &u, scenario_id, activity_id, StatusCode::CREATED).await
}

/// DELETE /api/scenarios/{id}/activities/{aid}/comments/{cid}。書いた本人だけが消せる。
pub async fn delete(State(st): State<AppState>, CurrentUser(u): CurrentUser, ids: PathIds) -> AppResult<Response> {
    let (scenario_id, activity_id) = path_ids(&ids)?;
    let comment_id = ids.get("cid")?;
    let mut tx = db::begin(&st.db).await?;
    let s = find_scenario(&mut tx, scenario_id, " FOR SHARE").await?;
    if s.is_locked {
        return Err(conflict("ロックされたシナリオのコメントは消せません"));
    }
    let row: Option<(i64, bool)> = tx
        .exec_first(
            "SELECT user_id, deleted_at IS NOT NULL FROM activity_scenario_comments WHERE id = ? AND scenario_id = ? AND activity_id = ? FOR UPDATE",
            (comment_id, scenario_id, activity_id),
        )
        .await?;
    let (author_id, deleted) = row.ok_or_else(|| not_found("コメントが見つかりません"))?;
    if author_id != u.id {
        return Err(forbidden());
    }
    if !deleted {
        tx.exec_drop("UPDATE activity_scenario_comments SET body = '', deleted_at = NOW() WHERE id = ?", (comment_id,))
            .await?;
    }
    tx.commit().await?;
    respond_comments(&st, &u, scenario_id, activity_id, StatusCode::OK).await
}

/// 先頭 n 文字（超えたら「…」を付ける）。改行は空白にする。
pub fn excerpt(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= n {
        return s;
    }
    let mut out: String = s.chars().take(n).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpts() {
        assert_eq!(excerpt("a\n b", 10), "a b");
        assert_eq!(excerpt("あいうえお", 3), "あいう…");
    }
}
