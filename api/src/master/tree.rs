//! 階層構造を持つマスタ（組織・セグメント）の API。
//!
//! ユニット（units）は末端ノードにのみ所属できるため、次を守る:
//!   - ユニットが所属しているノードの下には子ノードを作れない（移動先にもできない）
//!   - 子ノードを持つノードにはユニットを所属させられない（units 側で検証）

use std::collections::HashSet;

use axum::Router;
use axum::extract::State;
use axum::response::Response;
use axum::routing::{get, post};
use chrono::NaiveDateTime;
use mysql_async::prelude::*;
use serde::{Deserialize, Serialize};

use super::{MAX_NAME_LEN, delete_error, not_found_label, same_parent, transfer, validate_code};
use crate::auth::{AdminUser, CurrentUser};
use crate::db::{self, Tx};
use crate::error::{conflict, validation1};
use crate::http::{self, Id, Json, Validator};
use crate::{AppResult, AppState, audit, codes, jst};

/// 組織・セグメントの違い。
pub struct TreeKind {
    /// organizations / segments
    pub table: &'static str,
    /// 組織 / セグメント
    pub label: &'static str,
    /// units テーブルでこのマスタを参照する列
    pub unit_column: &'static str,
    /// 自動採番するコードの接頭辞（ORG- / SEG-）
    pub code_prefix: &'static str,
}

pub static ORGANIZATIONS: TreeKind =
    TreeKind { table: "organizations", label: "組織", unit_column: "organization_id", code_prefix: "ORG-" };
pub static SEGMENTS: TreeKind =
    TreeKind { table: "segments", label: "セグメント", unit_column: "segment_id", code_prefix: "SEG-" };

#[derive(Clone, Debug, Serialize)]
pub struct TreeNode {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub code: String,
    pub name: String,
    pub level: i32,
    pub sort_order: i32,
    #[serde(serialize_with = "jst::ser")]
    pub created_at: NaiveDateTime,
    #[serde(serialize_with = "jst::ser")]
    pub updated_at: NaiveDateTime,
}
from_row!(TreeNode { id, parent_id, code, name, level, sort_order, created_at, updated_at });

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct TreeRequest {
    parent_id: Option<i64>,
    /// 作成時は空なら自動採番、更新時は空なら変更しない
    code: String,
    name: String,
    sort_order: i32,
    reason: String,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ReasonRequest {
    pub reason: String,
}

pub fn routes(r: Router<AppState>, path: &str, k: &'static TreeKind) -> Router<AppState> {
    r.route(
        &format!("/api/{path}"),
        get(move |st: State<AppState>, u: CurrentUser| list(k, st, u))
            .post(move |st: State<AppState>, u: AdminUser, b: Json<TreeRequest>| create(k, st, u, b)),
    )
    .route(
        &format!("/api/{path}/export"),
        get(move |st: State<AppState>, u: CurrentUser| transfer::export_tree(k, st, u)),
    )
    .route(
        &format!("/api/{path}/import"),
        post(move |st: State<AppState>, u: AdminUser, q: http::Query, mp| transfer::import_tree(k, st, u, q, mp)),
    )
    .route(
        &format!("/api/{path}/{{id}}"),
        get(move |st: State<AppState>, u: CurrentUser, id: Id| get_one(k, st, u, id))
            .put(move |st: State<AppState>, u: AdminUser, id: Id, b: Json<TreeRequest>| update(k, st, u, id, b))
            .delete(move |st: State<AppState>, u: AdminUser, id: Id, b: Json<ReasonRequest>| delete(k, st, u, id, b)),
    )
}

impl TreeKind {
    pub fn select_sql(&self) -> String {
        format!("SELECT id, parent_id, code, name, level, sort_order, created_at, updated_at FROM {}", self.table)
    }

    pub async fn find<Q: Queryable>(&self, q: &mut Q, id: i64, lock: &str) -> AppResult<TreeNode> {
        let n: Option<TreeNode> = q.exec_first(format!("{} WHERE id = ?{lock}", self.select_sql()), (id,)).await?;
        n.ok_or_else(|| not_found_label(self.label))
    }

    /// 親にするノードを行ロック付きで取得し、ユニットが所属していないことを確認する。
    async fn lock_parent(&self, tx: &mut Tx, parent_id: i64) -> AppResult<TreeNode> {
        let parent = match self.find(tx, parent_id, " FOR UPDATE").await {
            Err(e) if e.is_not_found() => {
                return Err(validation1("parent_id", format!("親の{}が見つかりません", self.label)));
            }
            r => r?,
        };
        let n =
            db::count(tx, &format!("SELECT COUNT(*) FROM units WHERE {} = ?", self.unit_column), p![parent_id]).await?;
        if n > 0 {
            return Err(validation1(
                "parent_id",
                format!("ユニットが所属している{}の下には追加できません", self.label),
            ));
        }
        Ok(parent)
    }

    /// id の配下（子孫）のノード ID を返す。
    async fn descendant_ids(&self, tx: &mut Tx, id: i64) -> AppResult<HashSet<i64>> {
        let t = self.table;
        let ids: Vec<i64> = tx
            .exec(
                format!(
                    "WITH RECURSIVE sub AS (
                        SELECT id FROM {t} WHERE parent_id = ?
                        UNION ALL
                        SELECT c.id FROM {t} c JOIN sub ON c.parent_id = sub.id
                    )
                    SELECT id FROM sub"
                ),
                (id,),
            )
            .await?;
        Ok(ids.into_iter().collect())
    }
}

/// GET /api/{organizations|segments}。階層レベル・表示順の順に全件を返す。
async fn list(k: &'static TreeKind, State(st): State<AppState>, _: CurrentUser) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let items: Vec<TreeNode> = conn.exec(format!("{} ORDER BY level, sort_order, id", k.select_sql()), ()).await?;
    Ok(http::list(&items))
}

/// GET /api/{organizations|segments}/{id}
async fn get_one(k: &'static TreeKind, State(st): State<AppState>, _: CurrentUser, Id(id): Id) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    Ok(http::ok(&k.find(&mut conn, id, "").await?))
}

/// POST /api/{organizations|segments}
async fn create(
    k: &'static TreeKind,
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Json(req): Json<TreeRequest>,
) -> AppResult<Response> {
    let mut v = Validator::new();
    let name = v.text("name", "名称", &req.name, MAX_NAME_LEN);
    let mut code = validate_code(&mut v, &req.code, true);
    v.check()?;

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let mut level = 1;
    if let Some(pid) = req.parent_id {
        level = k.lock_parent(&mut tx, pid).await?.level + 1;
    }
    if code.is_empty() {
        code = codes::next(&mut tx, k.table, k.code_prefix, None).await?;
    }
    let r = tx
        .exec_drop(
            format!("INSERT INTO {} (parent_id, code, name, level, sort_order) VALUES (?, ?, ?, ?, ?)", k.table),
            p![req.parent_id, code, name, level, req.sort_order],
        )
        .await;
    if db::is_dup(&r) {
        return Err(validation1("code", "このコードは既に使われています"));
    }
    r?;
    let id = db::last_id(&tx);
    let created = k.find(&mut tx, id, "").await?;
    rec.insert(&mut tx, k.table, id, &created).await?;
    tx.commit().await?;
    Ok(http::created(&created))
}

/// PUT /api/{organizations|segments}/{id}。名称・表示順・親（移動）を更新する。
async fn update(
    k: &'static TreeKind,
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(req): Json<TreeRequest>,
) -> AppResult<Response> {
    let mut v = Validator::new();
    let name = v.text("name", "名称", &req.name, MAX_NAME_LEN);
    let mut code = validate_code(&mut v, &req.code, true); // 空なら今のコードのまま
    if req.parent_id == Some(id) {
        v.add("parent_id", "自分自身を親にはできません");
    }
    v.check()?;

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let before = k.find(&mut tx, id, " FOR UPDATE").await?;

    let mut level = before.level;
    if !same_parent(before.parent_id, req.parent_id) {
        level = 1;
        if let Some(pid) = req.parent_id {
            if k.descendant_ids(&mut tx, id).await?.contains(&pid) {
                return Err(validation1("parent_id", format!("配下の{}を親にはできません", k.label)));
            }
            level = k.lock_parent(&mut tx, pid).await?.level + 1;
        }
    }
    if code.is_empty() {
        code = before.code.clone();
    }
    let r = tx
        .exec_drop(
            format!("UPDATE {} SET parent_id = ?, code = ?, name = ?, sort_order = ?, level = ? WHERE id = ?", k.table),
            p![req.parent_id, code, name, req.sort_order, level, id],
        )
        .await;
    if db::is_dup(&r) {
        return Err(validation1("code", "このコードは既に使われています"));
    }
    r?;
    // 移動した場合は配下のノードの階層レベルも合わせてずらす。
    let delta = level - before.level;
    if delta != 0 {
        let t = k.table;
        tx.exec_drop(
            format!(
                "WITH RECURSIVE sub AS (
                    SELECT id FROM {t} WHERE parent_id = ?
                    UNION ALL
                    SELECT c.id FROM {t} c JOIN sub ON c.parent_id = sub.id
                )
                UPDATE {t} t JOIN sub ON t.id = sub.id SET t.level = t.level + ?"
            ),
            p![id, delta],
        )
        .await?;
    }
    let updated = k.find(&mut tx, id, "").await?;
    rec.update(&mut tx, k.table, id, &before, &updated).await?;
    tx.commit().await?;
    Ok(http::ok(&updated))
}

/// DELETE /api/{organizations|segments}/{id}。子ノードやユニットから参照されている場合は削除できない。
async fn delete(
    k: &'static TreeKind,
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    Id(id): Id,
    Json(req): Json<ReasonRequest>,
) -> AppResult<Response> {
    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &req.reason).await?;
    let before = k.find(&mut tx, id, " FOR UPDATE").await?;
    if db::count(&mut tx, &format!("SELECT COUNT(*) FROM {} WHERE parent_id = ?", k.table), p![id]).await? > 0 {
        return Err(conflict(format!("配下に{}があるため削除できません", k.label)));
    }
    if db::count(&mut tx, &format!("SELECT COUNT(*) FROM units WHERE {} = ?", k.unit_column), p![id]).await? > 0 {
        return Err(conflict("ユニットが所属しているため削除できません"));
    }
    let r = tx.exec_drop(format!("DELETE FROM {} WHERE id = ?", k.table), (id,)).await;
    delete_error(r, k.label)?;
    rec.delete(&mut tx, k.table, id, &before).await?;
    tx.commit().await?;
    Ok(http::no_content())
}
