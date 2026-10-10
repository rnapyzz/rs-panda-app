//! 施策の CSV インポート・エクスポート。取込は「追加と更新」のみで、CSV にない施策は削除しない。
//! code が空の行は新しい施策として追加し、施策コードを自動採番する。
//! external_codes は空白区切りの外部コード。記載した外部コードを施策に追加する（記載のない外部コードは外さない）。

use std::collections::{HashMap, HashSet};

use axum::extract::State;
use axum::extract::multipart::{Multipart, MultipartRejection};
use axum::response::Response;
use mysql_async::prelude::*;

use super::external::{self, EXTERNAL_CODE_PATTERN};
use super::{
    ACTIVITY_SELECT, Activity, ActivityInput, ActivityRequest, find_activity, insert_activity, next_activity_code,
    validate_activity_with,
};
use crate::audit::Recorder;
use crate::auth::{CurrentUser, normalize_email};
use crate::csvio::{self, ImportResult, RowErrors};
use crate::db::{self, Tx};
use crate::error::forbidden;
use crate::http::{Query, Validator};
use crate::{AppResult, AppState, audit};

const ACTIVITY_COLUMNS: [&str; 11] = [
    "code",
    "name",
    "unit_code",
    "activity_type",
    "status",
    "start_date",
    "end_date",
    "owner_email",
    "confidence_level",
    "assumptions",
    "external_codes",
];

/// GET /api/activities/export
pub async fn export(State(st): State<AppState>, _: CurrentUser) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let rows: Vec<mysql_async::Row> = conn
        .exec(
            "SELECT a.code, a.name, un.code, a.activity_type, a.status,
                    COALESCE(DATE_FORMAT(a.start_date, '%Y-%m-%d'), ''), COALESCE(DATE_FORMAT(a.end_date, '%Y-%m-%d'), ''),
                    COALESCE(usr.email, ''), a.confidence_level, COALESCE(a.assumptions, ''),
                    COALESCE((SELECT GROUP_CONCAT(e.code ORDER BY e.code SEPARATOR ' ') FROM activity_external_codes e WHERE e.activity_id = a.id), '')
             FROM activities a
             JOIN units un ON un.id = a.unit_id
             LEFT JOIN users usr ON usr.id = a.owner_user_id
             ORDER BY a.code",
            (),
        )
        .await?;
    let out: Vec<Vec<String>> = rows
        .into_iter()
        .map(|mut r| (0..ACTIVITY_COLUMNS.len()).map(|i| r.take::<String, _>(i).unwrap_or_default()).collect())
        .collect();
    csvio::write_csv("activities", &ACTIVITY_COLUMNS, &out)
}

struct ImportItem {
    input: ActivityInput,
    externals: Vec<String>,
}

/// POST /api/activities/import。FP&A のみ。変更理由が必須。
pub async fn import_csv(
    State(st): State<AppState>,
    CurrentUser(u): CurrentUser,
    q: Query,
    mp: Result<Multipart, MultipartRejection>,
) -> AppResult<Response> {
    if !u.is_admin() {
        return Err(forbidden());
    }
    let up = csvio::read_upload(&q, mp).await?;
    let rows = csvio::parse(&up.data, &ACTIVITY_COLUMNS)?;
    let mut result = ImportResult { dry_run: up.dry_run, rows: rows.len(), ..Default::default() };

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &up.reason).await?;
    // 廃止したユニットには所属させられない
    let units = id_index(&mut tx, "SELECT code, id FROM units WHERE NOT is_archived FOR SHARE").await?;
    let users = id_index(&mut tx, "SELECT email, id FROM users").await?;
    let activity_ids = id_index(&mut tx, "SELECT code, id FROM activities FOR UPDATE").await?;
    let external_owner = id_index(&mut tx, "SELECT code, activity_id FROM activity_external_codes FOR UPDATE").await?;
    let levels = id_index(&mut tx, "SELECT code, id FROM confidence_levels").await?;

    let mut errs = RowErrors::default();
    let mut items: Vec<ImportItem> = Vec::new();
    let mut seen_codes: HashSet<String> = HashSet::new();
    let mut seen_externals: HashMap<String, usize> = HashMap::new(); // 外部コード → CSV の行
    for row in &rows {
        let opt = |col: &str| Some(row.get(col).to_string()).filter(|s| !s.is_empty());
        let mut req = ActivityRequest {
            code: row.get("code").into(),
            name: row.get("name").into(),
            activity_type: row.get("activity_type").into(),
            status: row.get("status").into(),
            assumptions: row.get("assumptions").into(),
            start_date: opt("start_date"),
            end_date: opt("end_date"),
            confidence_level: row.get("confidence_level").into(),
            ..Default::default()
        };
        if req.status.is_empty() {
            req.status = "planned".into();
        }

        let mut v = Validator::new();
        match units.get(row.get("unit_code")) {
            Some(&id) => req.unit_id = id,
            None => v.add("unit_id", format!("ユニットのコード {:?} が見つかりません", row.get("unit_code"))),
        }
        let e = normalize_email(row.get("owner_email"));
        if !e.is_empty() {
            match users.get(&e) {
                Some(&id) => req.owner_user_id = Some(id),
                None => v.add("owner_user_id", format!("担当者のメールアドレス {e} のユーザーが見つかりません")),
            }
        }
        if !req.code.is_empty() {
            if !seen_codes.insert(req.code.clone()) {
                v.add("code", format!("施策コード {} が CSV 内で重複しています", req.code));
            }
            if external_owner.contains_key(&req.code) && !activity_ids.contains_key(&req.code) {
                v.add("code", format!("施策コード {} は外部コードとして使われています", req.code));
            }
        }
        // 項目の検証。ユニット・担当者はコードの解決で出したメッセージを優先する（同じ内容を2重に出さない）
        let mut fields = Validator::new();
        let input = validate_activity_with(&req, true, &mut fields);
        let fields_ok = fields.is_empty();
        for (field, msg) in fields.0 {
            v.add(&field, msg);
        }
        if fields_ok && !levels.contains_key(&input.confidence_level) {
            v.add("confidence_level", format!("確度の段階 {:?} は登録されていません", input.confidence_level));
        }
        let row_ok = v.is_empty();
        if let Err(e) = v.check() {
            errs.add_details(row.line, e);
        }

        // 外部コード
        let externals: Vec<String> = row.get("external_codes").split_whitespace().map(str::to_string).collect();
        for ext in &externals {
            if !EXTERNAL_CODE_PATTERN.is_match(ext) {
                errs.add(row.line, format!("外部コード {ext:?} に使えない文字が含まれています"));
            } else if let Some(&line) = seen_externals.get(ext).filter(|&&l| l > 0) {
                errs.add(row.line, format!("外部コード {ext} が CSV の {line} 行目と重複しています"));
            } else if activity_ids.contains_key(ext) || seen_codes.contains(ext) {
                errs.add(row.line, format!("外部コード {ext} は施策コードとして使われています"));
            } else if let Some(owner) = external_owner.get(ext)
                && (req.code.is_empty() || activity_ids.get(&req.code) != Some(owner))
            {
                errs.add(row.line, format!("外部コード {ext} は別の施策に登録されています"));
            }
            seen_externals.insert(ext.clone(), row.line);
        }
        if row_ok {
            items.push(ImportItem { input, externals });
        }
    }
    errs.check()?;

    for it in items {
        let (mut changed, created, id) = upsert_activity(&mut tx, &rec, it.input).await?;
        for ext in &it.externals {
            if external_owner.get(ext) == Some(&id) {
                continue;
            }
            tx.exec_drop("INSERT INTO activity_external_codes (activity_id, code) VALUES (?, ?)", (id, ext.clone()))
                .await?;
            let eid = db::last_id(&tx);
            let e = external::find_by_id(&mut tx, eid).await?;
            rec.insert(&mut tx, "activity_external_codes", eid, &e).await?;
            changed = true;
        }
        if created {
            result.inserted += 1;
        } else if changed {
            result.updated += 1;
        } else {
            result.unchanged += 1;
        }
    }
    csvio::finish(tx, result).await
}

/// 施策を追加または更新する。コードが空なら自動採番して追加する。(changed, created, id) を返す。
async fn upsert_activity(tx: &mut Tx, rec: &Recorder, mut input: ActivityInput) -> AppResult<(bool, bool, i64)> {
    let before: Option<Activity> = if input.code.is_empty() {
        None
    } else {
        tx.exec_first(format!("{ACTIVITY_SELECT} WHERE a.code = ? FOR UPDATE"), (input.code.clone(),)).await?
    };
    let Some(before) = before else {
        if input.code.is_empty() {
            input.code = next_activity_code(tx).await?;
        }
        insert_activity(tx, &input).await?;
        let id = db::last_id(tx);
        let created = find_activity(tx, id, "").await?;
        rec.insert(tx, "activities", id, &created).await?;
        return Ok((true, true, id));
    };

    let same = before.unit_id == input.unit_id
        && before.name == input.name
        && before.activity_type == input.activity_type
        && before.status == input.status
        && before.start_date == input.start_date
        && before.end_date == input.end_date
        && before.owner_user_id == input.owner_user_id
        && before.confidence_level == input.confidence_level
        && before.assumptions == input.assumptions;
    if same {
        return Ok((false, false, before.id));
    }
    tx.exec_drop(
        "UPDATE activities SET unit_id = ?, name = ?, activity_type = ?, status = ?, start_date = ?, end_date = ?,
                owner_user_id = ?, confidence_level = ?, assumptions = ?
         WHERE id = ?",
        p![
            input.unit_id,
            input.name.clone(),
            input.activity_type.clone(),
            input.status.clone(),
            input.start_date,
            input.end_date,
            input.owner_user_id,
            input.confidence_level.clone(),
            db::null_str(&input.assumptions),
            before.id
        ],
    )
    .await?;
    let after = find_activity(tx, before.id, "").await?;
    rec.update(tx, "activities", before.id, &before, &after).await?;
    Ok((true, false, before.id))
}

/// 「キー, ID」を返すクエリから対応表を作る。
async fn id_index(tx: &mut Tx, sql: &str) -> AppResult<HashMap<String, i64>> {
    let rows: Vec<(String, i64)> = tx.exec(sql, ()).await?;
    Ok(rows.into_iter().collect())
}
