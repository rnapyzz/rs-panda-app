//! 組織変更と異動（docs/plan.md「2.15 組織変更と異動」）。
//!
//! - 組織変更の予約: 施策の移動・ユニットの所属の変更・ユニットの統合・担当者の変更をまとめて登録し、有効日に自動で適用する。
//!   適用は全部か無しか。失敗したら予約を失敗にして理由を残し、FP&A にお知らせを送る
//! - ユニットの統合と廃止
//! - 無効化のときの後任者への付け替え
//!
//! 集計は常に今の組織で行う（組替え）ので、変更の後は過去の版も新しい組織で集計される。

mod plans;
mod units_users;

use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
use mysql_async::prelude::*;
use serde::{Deserialize, Serialize};

pub use plans::{apply_due, run};

use crate::audit::Recorder;
use crate::db::{self, Tx};
use crate::error::{AppError, conflict};
use crate::{AppResult, AppState};

// 変更の種類。
/// 施策のユニットの移動
pub const KIND_MOVE_ACTIVITY: &str = "move_activity";
/// ユニットの所属（セグメント・組織）の変更
pub const KIND_MOVE_UNIT: &str = "move_unit";
/// ユニットの統合
pub const KIND_MERGE_UNIT: &str = "merge_unit";
/// 施策の担当者・ユニットのマネージャーの変更
pub const KIND_CHANGE_OWNER: &str = "change_owner";

/// 予約の自動の適用を確かめる間隔。
const RUN_INTERVAL: Duration = Duration::from_secs(60);

/// ルート。すべて FP&A のみ（各ハンドラーの AdminUser で判定する）。
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/org-change-plans", get(plans::list).post(plans::create))
        .route("/api/org-change-plans/{id}", get(plans::get_one).put(plans::update).delete(plans::delete))
        .route("/api/org-change-plans/{id}/apply", post(plans::apply_now))
        .route("/api/org-change-plans/{id}/cancel", post(plans::cancel))
        .route("/api/units/{id}/merge", post(units_users::merge_unit))
        .route("/api/units/{id}/archive", post(units_users::archive_unit))
        .route("/api/units/{id}/unarchive", post(units_users::unarchive_unit))
        .route("/api/users/{id}/assignments", get(units_users::assignments))
        .route("/api/users/{id}/deactivate", post(units_users::deactivate))
}

/// 予約の1つの変更。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Item {
    pub id: i64,
    pub kind: String,
    pub activity_id: Option<i64>,
    pub unit_id: Option<i64>,
    pub target_unit_id: Option<i64>,
    pub segment_id: Option<i64>,
    pub organization_id: Option<i64>,
    pub owner_user_id: Option<i64>,
}
from_row!(Item { id, kind, activity_id, unit_id, target_unit_id, segment_id, organization_id, owner_user_id });

/// 監査ログに残す施策（変えた項目を含む一部）。
#[derive(Clone, Debug, Serialize)]
struct ActivityRow {
    id: i64,
    code: String,
    name: String,
    unit_id: i64,
    owner_user_id: Option<i64>,
}
from_row!(ActivityRow { id, code, name, unit_id, owner_user_id });

/// 監査ログに残すユニット（変えた項目を含む一部）。
#[derive(Clone, Debug, Serialize)]
struct UnitRow {
    id: i64,
    code: String,
    name: String,
    segment_id: i64,
    organization_id: i64,
    owner_user_id: Option<i64>,
    is_archived: bool,
}
from_row!(UnitRow { id, code, name, segment_id, organization_id, owner_user_id, is_archived });

/// API のエラーの、利用者向けのメッセージ（項目ごとの内容があれば最初のもの）。
fn message(e: &AppError) -> String {
    match e {
        AppError::Api(e) => e.details.values().next().cloned().unwrap_or_else(|| e.message.clone()),
        AppError::Internal(e) => e.to_string(),
    }
}

async fn find_activity(tx: &mut Tx, id: i64) -> AppResult<ActivityRow> {
    let a: Option<ActivityRow> = tx
        .exec_first("SELECT id, code, name, unit_id, owner_user_id FROM activities WHERE id = ? FOR UPDATE", (id,))
        .await?;
    a.ok_or_else(|| conflict(format!("施策 #{id} が見つかりません（削除された可能性があります）")))
}

async fn find_unit(tx: &mut Tx, id: i64) -> AppResult<UnitRow> {
    let u: Option<UnitRow> = tx
        .exec_first(
            "SELECT id, code, name, segment_id, organization_id, owner_user_id, is_archived FROM units WHERE id = ? FOR UPDATE",
            (id,),
        )
        .await?;
    u.ok_or_else(|| conflict(format!("ユニット #{id} が見つかりません（削除された可能性があります）")))
}

/// 施策を所属させられる（廃止していない）ユニット。
async fn active_unit(tx: &mut Tx, id: i64) -> AppResult<UnitRow> {
    let u = find_unit(tx, id).await?;
    if u.is_archived {
        return Err(conflict(format!("ユニット「{}」は廃止されています", u.name)));
    }
    Ok(u)
}

/// ノード（セグメント・組織）があり、子を持たない末端であることを確かめる。
async fn check_leaf(tx: &mut Tx, table: &str, label: &str, id: i64) -> AppResult<()> {
    let name: Option<String> = tx.exec_first(format!("SELECT name FROM {table} WHERE id = ? FOR SHARE"), (id,)).await?;
    let Some(name) = name else {
        return Err(conflict(format!("{label} #{id} が見つかりません")));
    };
    if db::count(tx, &format!("SELECT COUNT(*) FROM {table} WHERE parent_id = ?"), p![id]).await? > 0 {
        return Err(conflict(format!(
            "{label}「{name}」は子を持つため、ユニットを所属させられません（末端のノードを選んでください）"
        )));
    }
    Ok(())
}

/// 担当者にするユーザーが有効であることを確かめる。None（未設定）はよい。
async fn check_active_user(tx: &mut Tx, id: Option<i64>) -> AppResult<()> {
    let Some(id) = id else { return Ok(()) };
    let u: Option<(String, bool)> =
        tx.exec_first("SELECT name, is_active FROM users WHERE id = ? FOR SHARE", (id,)).await?;
    match u {
        None => Err(conflict(format!("ユーザー #{id} が見つかりません"))),
        Some((name, false)) => Err(conflict(format!("ユーザー「{name}」は無効です"))),
        Some(_) => Ok(()),
    }
}

/// 施策を別のユニットへ移す。
async fn move_activity(tx: &mut Tx, rec: &Recorder, activity_id: i64, unit_id: i64) -> AppResult<()> {
    let before = find_activity(tx, activity_id).await?;
    active_unit(tx, unit_id).await?;
    if before.unit_id == unit_id {
        return Ok(());
    }
    tx.exec_drop("UPDATE activities SET unit_id = ? WHERE id = ?", (unit_id, activity_id)).await?;
    let after = ActivityRow { unit_id, ..before.clone() };
    rec.update(tx, "activities", activity_id, &before, &after).await
}

/// ユニットの所属（セグメント・組織の末端ノード）を変える。
async fn move_unit(tx: &mut Tx, rec: &Recorder, unit_id: i64, segment_id: i64, organization_id: i64) -> AppResult<()> {
    let before = active_unit(tx, unit_id).await?;
    check_leaf(tx, "segments", "セグメント", segment_id).await?;
    check_leaf(tx, "organizations", "組織", organization_id).await?;
    if before.segment_id == segment_id && before.organization_id == organization_id {
        return Ok(());
    }
    tx.exec_drop(
        "UPDATE units SET segment_id = ?, organization_id = ? WHERE id = ?",
        (segment_id, organization_id, unit_id),
    )
    .await?;
    let after = UnitRow { segment_id, organization_id, ..before.clone() };
    rec.update(tx, "units", unit_id, &before, &after).await
}

/// ユニットを統合する。施策をすべて統合先へ移し、元のユニットを廃止にする。移した施策の数を返す。
async fn merge_unit(tx: &mut Tx, rec: &Recorder, unit_id: i64, target_id: i64) -> AppResult<usize> {
    if unit_id == target_id {
        return Err(conflict("統合先に自分自身は選べません"));
    }
    let before = active_unit(tx, unit_id).await?;
    active_unit(tx, target_id).await?;
    let ids: Vec<i64> =
        tx.exec("SELECT id FROM activities WHERE unit_id = ? ORDER BY id FOR UPDATE", (unit_id,)).await?;
    for id in &ids {
        move_activity(tx, rec, *id, target_id).await?;
    }
    set_archived(tx, rec, &before, true).await?;
    Ok(ids.len())
}

async fn set_archived(tx: &mut Tx, rec: &Recorder, before: &UnitRow, archived: bool) -> AppResult<()> {
    tx.exec_drop("UPDATE units SET is_archived = ? WHERE id = ?", (archived, before.id)).await?;
    let after = UnitRow { is_archived: archived, ..before.clone() };
    rec.update(tx, "units", before.id, before, &after).await
}

/// 施策の担当者を変える（None は未設定）。
async fn change_activity_owner(tx: &mut Tx, rec: &Recorder, activity_id: i64, owner: Option<i64>) -> AppResult<()> {
    let before = find_activity(tx, activity_id).await?;
    check_active_user(tx, owner).await?;
    if before.owner_user_id == owner {
        return Ok(());
    }
    tx.exec_drop("UPDATE activities SET owner_user_id = ? WHERE id = ?", (owner, activity_id)).await?;
    let after = ActivityRow { owner_user_id: owner, ..before.clone() };
    rec.update(tx, "activities", activity_id, &before, &after).await
}

/// ユニットのマネージャーを変える（None は未設定）。
async fn change_unit_owner(tx: &mut Tx, rec: &Recorder, unit_id: i64, owner: Option<i64>) -> AppResult<()> {
    let before = find_unit(tx, unit_id).await?;
    check_active_user(tx, owner).await?;
    if before.owner_user_id == owner {
        return Ok(());
    }
    tx.exec_drop("UPDATE units SET owner_user_id = ? WHERE id = ?", (owner, unit_id)).await?;
    let after = UnitRow { owner_user_id: owner, ..before.clone() };
    rec.update(tx, "units", unit_id, &before, &after).await
}

/// 変更を順に適用する。適用できない変更があれば、何番目の変更かを含むメッセージを返す（呼び出し側でロールバックする）。
/// DB のエラーなど、変更の内容によらないエラーはそのまま返す。
async fn apply_items(tx: &mut Tx, rec: &Recorder, items: &[Item]) -> AppResult<Option<String>> {
    for (i, it) in items.iter().enumerate() {
        let id = |v: Option<i64>| v.unwrap_or(0);
        let r = match it.kind.as_str() {
            KIND_MOVE_ACTIVITY => move_activity(tx, rec, id(it.activity_id), id(it.target_unit_id)).await,
            KIND_MOVE_UNIT => move_unit(tx, rec, id(it.unit_id), id(it.segment_id), id(it.organization_id)).await,
            KIND_MERGE_UNIT => merge_unit(tx, rec, id(it.unit_id), id(it.target_unit_id)).await.map(|_| ()),
            KIND_CHANGE_OWNER => match it.activity_id {
                Some(a) => change_activity_owner(tx, rec, a, it.owner_user_id).await,
                None => change_unit_owner(tx, rec, id(it.unit_id), it.owner_user_id).await,
            },
            k => Err(conflict(format!("変更の種類 {k:?} はありません"))),
        };
        match r {
            Ok(()) => {}
            Err(e @ AppError::Api(_)) => return Ok(Some(format!("{} 件目の変更: {}", i + 1, message(&e)))),
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}
