//! CSV インポート・エクスポート。取込は「追加と更新」のみで、CSV にない行は削除しない。
//! 行はコード（ユーザーはメールアドレス）で既存のデータと結びつける。

use std::collections::{HashMap, HashSet};

use axum::extract::State;
use axum::extract::multipart::{Multipart, MultipartRejection};
use axum::response::Response;
use mysql_async::prelude::*;

use super::subject::{SUBJECT_SELECT, Subject, SubjectRequest, find_subject, validate_subject_request};
use super::tree::{TreeKind, TreeNode};
use super::unit::{UNIT_SELECT, Unit, UnitRequest, find_unit, validate_unit_request};
use super::user::{UNUSABLE_PASSWORD_HASH, find_user, validate_slack_id, validate_user_fields};
use super::{MAX_NAME_LEN, same_parent};
use crate::auth::{AdminUser, CurrentUser, normalize_email};
use crate::csvio::{self, ImportResult, RowErrors};
use crate::db::{self, Tx};
use crate::http::{Query, Validator};
use crate::{AppResult, AppState, audit, codes};

// --- 組織・セグメント ---

const TREE_COLUMNS: [&str; 4] = ["code", "name", "parent_code", "sort_order"];

/// GET /api/{organizations|segments}/export。親が子より先に来る順で書き出す。
pub async fn export_tree(k: &'static TreeKind, State(st): State<AppState>, _: CurrentUser) -> AppResult<Response> {
    let t = k.table;
    let mut conn = st.db.get_conn().await?;
    let rows: Vec<(String, String, String, i32)> = conn
        .exec(
            format!(
                "SELECT n.code, n.name, COALESCE(p.code, ''), n.sort_order
                 FROM {t} n LEFT JOIN {t} p ON p.id = n.parent_id
                 ORDER BY n.level, n.sort_order, n.id"
            ),
            (),
        )
        .await?;
    let out: Vec<Vec<String>> = rows.into_iter().map(|(c, n, p, s)| vec![c, n, p, s.to_string()]).collect();
    csvio::write_csv(t, &TREE_COLUMNS, &out)
}

#[derive(Clone)]
struct TreeImportItem {
    line: usize,
    code: String,
    name: String,
    parent_code: String,
    sort_order: i32,
}

/// POST /api/{organizations|segments}/import
pub async fn import_tree(
    k: &'static TreeKind,
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    q: Query,
    mp: Result<Multipart, MultipartRejection>,
) -> AppResult<Response> {
    let up = csvio::read_upload(&q, mp).await?;
    let rows = csvio::parse(&up.data, &TREE_COLUMNS)?;
    let mut result = ImportResult { dry_run: up.dry_run, rows: rows.len(), ..Default::default() };

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &up.reason).await?;
    let existing = lock_all(k, &mut tx).await?;
    let code_of: HashMap<i64, String> = existing.values().map(|n| (n.id, n.code.clone())).collect();

    // 行の検証
    let mut errs = RowErrors::default();
    let mut items: HashMap<String, TreeImportItem> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for row in &rows {
        let mut v = Validator::new();
        let mut it = TreeImportItem {
            line: row.line,
            code: row.get("code").to_string(),
            name: String::new(),
            parent_code: row.get("parent_code").to_string(),
            sort_order: 0,
        };
        it.name = v.text("name", "名称", row.get("name"), MAX_NAME_LEN);
        if !codes::PATTERN.is_match(&it.code) {
            v.add("code", format!("コード {:?}: {}", it.code, codes::PATTERN_MESSAGE));
        }
        let s = row.get("sort_order");
        if !s.is_empty() {
            match s.parse::<i32>() {
                Ok(n) => it.sort_order = n,
                Err(_) => v.add("sort_order", format!("表示順 {s:?} は整数で入力してください")),
            }
        }
        if it.parent_code == it.code && !it.code.is_empty() {
            v.add("parent_code", "自分自身を親にはできません");
        }
        if items.contains_key(&it.code) && !it.code.is_empty() {
            v.add("code", format!("コード {} が CSV 内で重複しています", it.code));
        }
        if let Err(e) = v.check() {
            errs.add_details(row.line, e);
            continue;
        }
        order.push(it.code.clone());
        items.insert(it.code.clone(), it);
    }
    errs.check()?;

    // 取込後の親子関係（コード → 親のコード）
    let mut parent_of: HashMap<String, String> = HashMap::new();
    for n in existing.values() {
        let p = n.parent_id.and_then(|p| code_of.get(&p).cloned()).unwrap_or_default();
        parent_of.insert(n.code.clone(), p);
    }
    for it in items.values() {
        parent_of.insert(it.code.clone(), it.parent_code.clone());
    }
    for c in &order {
        let it = &items[c];
        if !it.parent_code.is_empty() && !parent_of.contains_key(&it.parent_code) {
            errs.add(it.line, format!("親のコード {} が見つかりません", it.parent_code));
        }
    }
    errs.check()?;
    let (depth, cyclic) = depths(&parent_of);
    for c in &order {
        if cyclic.contains(c) {
            errs.add(items[c].line, format!("親子関係が循環しています（{c}）"));
        }
    }
    // ユニットが所属しているノードの下には子を置けない
    let unit_nodes = nodes_with_units(k, &mut tx).await?;
    for c in &order {
        let p = &items[c].parent_code;
        if !p.is_empty() && unit_nodes.contains(p) {
            errs.add(items[c].line, format!("ユニットが所属している{} {} の下には追加できません", k.label, p));
        }
    }
    errs.check()?;

    // 親が先になるよう、深さの順に保存する
    order.sort_by_key(|c| depth[c]);
    let mut id_of: HashMap<String, i64> = existing.values().map(|n| (n.code.clone(), n.id)).collect();
    for c in &order {
        let it = &items[c];
        let parent_id =
            if it.parent_code.is_empty() { None } else { Some(id_of.get(&it.parent_code).copied().unwrap_or(0)) };
        let level = depth[c] + 1;
        let Some(before) = existing.get(c) else {
            tx.exec_drop(
                format!("INSERT INTO {} (parent_id, code, name, level, sort_order) VALUES (?, ?, ?, ?, ?)", k.table),
                p![parent_id, c.clone(), it.name.clone(), level as i64, it.sort_order],
            )
            .await?;
            let id = db::last_id(&tx);
            id_of.insert(c.clone(), id);
            let created = k.find(&mut tx, id, "").await?;
            rec.insert(&mut tx, k.table, id, &created).await?;
            result.inserted += 1;
            continue;
        };
        if same_parent(before.parent_id, parent_id) && before.name == it.name && before.sort_order == it.sort_order {
            result.unchanged += 1;
            continue;
        }
        tx.exec_drop(
            format!("UPDATE {} SET parent_id = ?, name = ?, sort_order = ?, level = ? WHERE id = ?", k.table),
            p![parent_id, it.name.clone(), it.sort_order, level as i64, before.id],
        )
        .await?;
        let after = k.find(&mut tx, before.id, "").await?;
        rec.update(&mut tx, k.table, before.id, before, &after).await?;
        result.updated += 1;
    }
    // 移動したノードの配下（CSV に含まれない行）の階層レベルも合わせる
    recalc_levels(k, &mut tx).await?;
    csvio::finish(tx, result).await
}

/// 全ノードを行ロック付きで読み込み、コード → ノードで返す。
async fn lock_all(k: &TreeKind, tx: &mut Tx) -> AppResult<HashMap<String, TreeNode>> {
    let nodes: Vec<TreeNode> = tx.exec(format!("{} FOR UPDATE", k.select_sql()), ()).await?;
    Ok(nodes.into_iter().map(|n| (n.code.clone(), n)).collect())
}

/// ユニットが所属しているノードのコードを返す。
async fn nodes_with_units(k: &TreeKind, tx: &mut Tx) -> AppResult<HashSet<String>> {
    let codes: Vec<String> = tx
        .exec(format!("SELECT DISTINCT n.code FROM {} n JOIN units u ON u.{} = n.id", k.table, k.unit_column), ())
        .await?;
    Ok(codes.into_iter().collect())
}

/// 親子関係から全ノードの階層レベルを計算し直す。
async fn recalc_levels(k: &TreeKind, tx: &mut Tx) -> AppResult<()> {
    let rows: Vec<(i64, Option<i64>, i64)> =
        tx.exec(format!("SELECT id, parent_id, level FROM {}", k.table), ()).await?;
    let nodes: HashMap<i64, (Option<i64>, i64)> = rows.into_iter().map(|(id, p, l)| (id, (p, l))).collect();
    fn level_of(nodes: &HashMap<i64, (Option<i64>, i64)>, id: i64, guard: usize) -> i64 {
        match nodes.get(&id).and_then(|n| n.0) {
            Some(p) if guard <= nodes.len() => level_of(nodes, p, guard + 1) + 1,
            _ => 1,
        }
    }
    for (&id, &(_, level)) in &nodes {
        let lv = level_of(&nodes, id, 0);
        if lv != level {
            tx.exec_drop(format!("UPDATE {} SET level = ? WHERE id = ?", k.table), p![lv, id]).await?;
        }
    }
    Ok(())
}

/// 親のコードの対応から、各コードの深さ（ルートが 0）と循環しているコードを返す。
fn depths(parent_of: &HashMap<String, String>) -> (HashMap<String, usize>, HashSet<String>) {
    let mut depth = HashMap::new();
    let mut cyclic = HashSet::new();
    for c in parent_of.keys() {
        let mut seen: HashSet<&str> = HashSet::from([c.as_str()]);
        let mut d = 0;
        let mut p = parent_of.get(c).map(String::as_str).unwrap_or("");
        while !p.is_empty() {
            if !seen.insert(p) {
                cyclic.insert(c.clone());
                break;
            }
            d += 1;
            p = parent_of.get(p).map(String::as_str).unwrap_or("");
        }
        depth.insert(c.clone(), d);
    }
    (depth, cyclic)
}

// --- ユニット ---

const UNIT_COLUMNS: [&str; 6] = ["code", "name", "unit_type", "segment_code", "organization_code", "owner_email"];

/// GET /api/units/export
pub async fn export_units(State(st): State<AppState>, _: CurrentUser) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let rows: Vec<(String, String, String, String, String, String)> = conn
        .exec(
            "SELECT u.code, u.name, u.unit_type, s.code, o.code, COALESCE(usr.email, '')
             FROM units u
             JOIN segments s ON s.id = u.segment_id
             JOIN organizations o ON o.id = u.organization_id
             LEFT JOIN users usr ON usr.id = u.owner_user_id
             ORDER BY u.code",
            (),
        )
        .await?;
    let out: Vec<Vec<String>> = rows.into_iter().map(|(a, b, c, d, e, f)| vec![a, b, c, d, e, f]).collect();
    csvio::write_csv("units", &UNIT_COLUMNS, &out)
}

struct NodeRef {
    id: i64,
    leaf: bool,
}

/// コード → （ID, 末端か）を返す。
async fn leaf_index(tx: &mut Tx, table: &str) -> AppResult<HashMap<String, NodeRef>> {
    let rows: Vec<(i64, String, bool)> = tx
        .exec(
            format!("SELECT n.id, n.code, NOT EXISTS (SELECT 1 FROM {table} c WHERE c.parent_id = n.id) FROM {table} n FOR SHARE"),
            (),
        )
        .await?;
    Ok(rows.into_iter().map(|(id, code, leaf)| (code, NodeRef { id, leaf })).collect())
}

async fn email_index(tx: &mut Tx) -> AppResult<HashMap<String, i64>> {
    let rows: Vec<(String, i64)> = tx.exec("SELECT email, id FROM users", ()).await?;
    Ok(rows.into_iter().collect())
}

/// POST /api/units/import。所属先のセグメント・組織は末端のノードであること。
pub async fn import_units(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    q: Query,
    mp: Result<Multipart, MultipartRejection>,
) -> AppResult<Response> {
    let up = csvio::read_upload(&q, mp).await?;
    let rows = csvio::parse(&up.data, &UNIT_COLUMNS)?;
    let mut result = ImportResult { dry_run: up.dry_run, rows: rows.len(), ..Default::default() };

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &up.reason).await?;
    let segments = leaf_index(&mut tx, "segments").await?;
    let orgs = leaf_index(&mut tx, "organizations").await?;
    let users = email_index(&mut tx).await?;

    let mut errs = RowErrors::default();
    let mut items: Vec<(UnitRequest, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for row in &rows {
        let mut req = UnitRequest {
            code: row.get("code").into(),
            name: row.get("name").into(),
            unit_type: row.get("unit_type").into(),
            ..Default::default()
        };
        let mut v = Validator::new();
        let seg_code = row.get("segment_code");
        match segments.get(seg_code) {
            None => v.add("segment_id", format!("セグメントのコード {seg_code:?} が見つかりません")),
            Some(s) if !s.leaf => v.add(
                "segment_id",
                format!("セグメント {seg_code} は末端ではありません（ユニットは末端のセグメントに所属します）"),
            ),
            Some(s) => req.segment_id = s.id,
        }
        let org_code = row.get("organization_code");
        match orgs.get(org_code) {
            None => v.add("organization_id", format!("組織のコード {org_code:?} が見つかりません")),
            Some(o) if !o.leaf => v.add(
                "organization_id",
                format!("組織 {org_code} は末端ではありません（ユニットは末端の組織に所属します）"),
            ),
            Some(o) => req.organization_id = o.id,
        }
        let e = normalize_email(row.get("owner_email"));
        if !e.is_empty() {
            match users.get(&e) {
                Some(&id) => req.owner_user_id = Some(id),
                None => v.add("owner_user_id", format!("担当者のメールアドレス {e} のユーザーが見つかりません")),
            }
        }
        if !seen.insert(req.code.clone()) {
            v.add("code", format!("コード {} が CSV 内で重複しています", req.code));
        }
        if let Err(e) = v.check() {
            errs.add_details(row.line, e);
            continue;
        }
        match validate_unit_request(&mut req, false) {
            Ok(name) => items.push((req, name)),
            Err(e) => errs.add_details(row.line, e),
        }
    }
    errs.check()?;

    for (req, name) in &items {
        let before: Option<Unit> =
            tx.exec_first(format!("{UNIT_SELECT} WHERE code = ? FOR UPDATE"), (req.code.clone(),)).await?;
        let Some(before) = before else {
            tx.exec_drop(
                "INSERT INTO units (code, name, unit_type, segment_id, organization_id, owner_user_id) VALUES (?, ?, ?, ?, ?, ?)",
                p![req.code.clone(), name.clone(), req.unit_type.clone(), req.segment_id, req.organization_id, req.owner_user_id],
            )
            .await?;
            let id = db::last_id(&tx);
            let created = find_unit(&mut tx, id, "").await?;
            rec.insert(&mut tx, "units", id, &created).await?;
            result.inserted += 1;
            continue;
        };
        if before.name == *name
            && before.unit_type == req.unit_type
            && before.segment_id == req.segment_id
            && before.organization_id == req.organization_id
            && before.owner_user_id == req.owner_user_id
        {
            result.unchanged += 1;
            continue;
        }
        tx.exec_drop(
            "UPDATE units SET name = ?, unit_type = ?, segment_id = ?, organization_id = ?, owner_user_id = ? WHERE id = ?",
            p![name.clone(), req.unit_type.clone(), req.segment_id, req.organization_id, req.owner_user_id, before.id],
        )
        .await?;
        let after = find_unit(&mut tx, before.id, "").await?;
        rec.update(&mut tx, "units", before.id, &before, &after).await?;
        result.updated += 1;
    }
    csvio::finish(tx, result).await
}

// --- 勘定科目 ---

const SUBJECT_COLUMNS: [&str; 6] = ["code", "name", "category", "parent_code", "sort_order", "is_restricted"];
/// インポートで必須の列。is_restricted は省略できる（省略した行は閲覧制限を変えない）
const SUBJECT_REQUIRED: [&str; 5] = ["code", "name", "category", "parent_code", "sort_order"];

/// GET /api/subjects/export。親が子より先に来る順で書き出す。
pub async fn export_subjects(State(st): State<AppState>, _: CurrentUser) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let rows: Vec<(String, String, String, String, i32, bool)> = conn
        .exec(
            "SELECT s.code, s.name, s.category, COALESCE(p.code, ''), s.sort_order, s.is_restricted
             FROM subjects s LEFT JOIN subjects p ON p.id = s.parent_id
             ORDER BY s.parent_id IS NOT NULL, s.category, s.sort_order, s.code",
            (),
        )
        .await?;
    let out: Vec<Vec<String>> =
        rows.into_iter().map(|(a, b, c, d, s, r)| vec![a, b, c, d, s.to_string(), r.to_string()]).collect();
    csvio::write_csv("subjects", &SUBJECT_COLUMNS, &out)
}

struct SubjectItem {
    line: usize,
    name: String,
    category: String,
    parent_code: String,
    sort_order: i32,
    /// 閲覧制限（None は列がなく、今の設定を変えない）
    restricted: Option<bool>,
}

/// POST /api/subjects/import。親科目は同じ区分であること。
pub async fn import_subjects(
    State(st): State<AppState>,
    AdminUser(u): AdminUser,
    q: Query,
    mp: Result<Multipart, MultipartRejection>,
) -> AppResult<Response> {
    let up = csvio::read_upload(&q, mp).await?;
    let rows = csvio::parse_with_optional(&up.data, &SUBJECT_REQUIRED, &["is_restricted"])?;
    let mut result = ImportResult { dry_run: up.dry_run, rows: rows.len(), ..Default::default() };

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, u.id, None, &up.reason).await?;
    let list: Vec<Subject> = tx.exec(format!("{SUBJECT_SELECT} FOR UPDATE"), ()).await?;
    let code_of: HashMap<i64, String> = list.iter().map(|s| (s.id, s.code.clone())).collect();
    let existing: HashMap<String, Subject> = list.into_iter().map(|s| (s.code.clone(), s)).collect();

    let mut errs = RowErrors::default();
    let mut items: HashMap<String, SubjectItem> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for row in &rows {
        let req = SubjectRequest {
            code: row.get("code").into(),
            name: row.get("name").into(),
            category: row.get("category").into(),
            ..Default::default()
        };
        let (code, name) = match validate_subject_request(&req) {
            Ok(v) => v,
            Err(e) => {
                errs.add_details(row.line, e);
                continue;
            }
        };
        let mut it = SubjectItem {
            line: row.line,
            name,
            category: req.category,
            parent_code: row.get("parent_code").into(),
            sort_order: 0,
            restricted: None,
        };
        let s = row.get("sort_order");
        if !s.is_empty() {
            match s.parse::<i32>() {
                Ok(n) => it.sort_order = n,
                Err(_) => {
                    errs.add(row.line, format!("表示順 {s:?} は整数で入力してください"));
                    continue;
                }
            }
        }
        if row.has("is_restricted") {
            let s = row.get("is_restricted");
            let v = if s.is_empty() {
                false
            } else {
                match parse_bool(s) {
                    Some(v) => v,
                    None => {
                        errs.add(row.line, format!("閲覧制限 {s:?} は true か false で入力してください"));
                        continue;
                    }
                }
            };
            it.restricted = Some(v);
        }
        if items.contains_key(&code) {
            errs.add(row.line, format!("コード {code} が CSV 内で重複しています"));
            continue;
        }
        order.push(code.clone());
        items.insert(code, it);
    }
    errs.check()?;

    // 取込後の親子関係と区分
    let mut parent_of: HashMap<String, String> = HashMap::new();
    let mut category_of: HashMap<String, String> = HashMap::new();
    for (c, s) in &existing {
        parent_of.insert(c.clone(), s.parent_id.and_then(|p| code_of.get(&p).cloned()).unwrap_or_default());
        category_of.insert(c.clone(), s.category.clone());
    }
    for (c, it) in &items {
        parent_of.insert(c.clone(), it.parent_code.clone());
        category_of.insert(c.clone(), it.category.clone());
    }
    for c in &order {
        let it = &items[c];
        if !it.parent_code.is_empty() && !parent_of.contains_key(&it.parent_code) {
            errs.add(it.line, format!("親科目のコード {} が見つかりません", it.parent_code));
        }
    }
    errs.check()?;
    let (depth, cyclic) = depths(&parent_of);
    for c in &order {
        if cyclic.contains(c) {
            errs.add(items[c].line, format!("親子関係が循環しています（{c}）"));
        }
    }
    // 区分は親と同じであること（既存の子科目との関係も含めて確認する）
    for (c, p) in &parent_of {
        if p.is_empty() || category_of.get(c) == category_of.get(p) {
            continue;
        }
        let line = items.get(c).or_else(|| items.get(p)).map(|it| it.line).unwrap_or(0);
        if line > 0 {
            errs.add(line, format!("科目 {c} と親科目 {p} の区分が異なります"));
        }
    }
    errs.check()?;

    order.sort_by_key(|c| depth[c]);
    let mut id_of: HashMap<String, i64> = existing.iter().map(|(c, s)| (c.clone(), s.id)).collect();
    for c in &order {
        let it = &items[c];
        let parent_id =
            if it.parent_code.is_empty() { None } else { Some(id_of.get(&it.parent_code).copied().unwrap_or(0)) };
        let before = existing.get(c);
        let restricted = it.restricted.unwrap_or(before.is_some_and(|b| b.is_restricted));
        let Some(before) = before else {
            tx.exec_drop(
                "INSERT INTO subjects (parent_id, code, name, category, sort_order, is_restricted) VALUES (?, ?, ?, ?, ?, ?)",
                p![parent_id, c.clone(), it.name.clone(), it.category.clone(), it.sort_order, restricted],
            )
            .await?;
            let id = db::last_id(&tx);
            id_of.insert(c.clone(), id);
            let created = find_subject(&mut tx, id, "").await?;
            rec.insert(&mut tx, "subjects", id, &created).await?;
            result.inserted += 1;
            continue;
        };
        if same_parent(before.parent_id, parent_id)
            && before.name == it.name
            && before.category == it.category
            && before.sort_order == it.sort_order
            && before.is_restricted == restricted
        {
            result.unchanged += 1;
            continue;
        }
        tx.exec_drop(
            "UPDATE subjects SET parent_id = ?, name = ?, category = ?, sort_order = ?, is_restricted = ? WHERE id = ?",
            p![parent_id, it.name.clone(), it.category.clone(), it.sort_order, restricted, before.id],
        )
        .await?;
        let after = find_subject(&mut tx, before.id, "").await?;
        rec.update(&mut tx, "subjects", before.id, before, &after).await?;
        result.updated += 1;
    }
    csvio::finish(tx, result).await
}

// --- ユーザー ---

const USER_COLUMNS: [&str; 5] = ["email", "name", "role", "is_active", "slack_user_id"];
/// インポートで必須の列。slack_user_id は省略できる（省略した行は Slack のメンバー ID を変えない）
const USER_REQUIRED: [&str; 4] = ["email", "name", "role", "is_active"];

/// GET /api/users/export。パスワードは含めない。
pub async fn export_users(State(st): State<AppState>, _: CurrentUser) -> AppResult<Response> {
    let mut conn = st.db.get_conn().await?;
    let rows: Vec<(String, String, String, bool, String)> = conn
        .exec("SELECT email, name, role, is_active, COALESCE(slack_user_id, '') FROM users ORDER BY id", ())
        .await?;
    let out: Vec<Vec<String>> = rows.into_iter().map(|(e, n, r, a, s)| vec![e, n, r, a.to_string(), s]).collect();
    csvio::write_csv("users", &USER_COLUMNS, &out)
}

struct UserItem {
    name: String,
    email: String,
    role: String,
    active: bool,
    slack_id: String,
    /// slack_user_id の列がないので、既存の値を変えない
    keep_slack: bool,
}

/// POST /api/users/import。パスワードは扱わない。
/// 追加したユーザーはパスワード未設定（ログインできない）で作成し、FP&A が画面で設定する。
/// 自分自身のロール変更・無効化はできない。
pub async fn import_users(
    State(st): State<AppState>,
    AdminUser(me): AdminUser,
    q: Query,
    mp: Result<Multipart, MultipartRejection>,
) -> AppResult<Response> {
    let up = csvio::read_upload(&q, mp).await?;
    let rows = csvio::parse_with_optional(&up.data, &USER_REQUIRED, &["slack_user_id"])?;
    let mut result = ImportResult { dry_run: up.dry_run, rows: rows.len(), ..Default::default() };

    let mut tx = db::begin(&st.db).await?;
    let rec = audit::begin(&mut tx, me.id, None, &up.reason).await?;

    let mut errs = RowErrors::default();
    let mut items: Vec<UserItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for row in &rows {
        let mut v = Validator::new();
        let role = row.get("role").to_string();
        let (name, email) = validate_user_fields(&mut v, row.get("name"), row.get("email"), &role);
        let slack_id = validate_slack_id(&mut v, row.get("slack_user_id"));
        let active = parse_bool(row.get("is_active"));
        if active.is_none() {
            v.add("is_active", "is_active は true / false で入力してください");
        }
        let active = active.unwrap_or(false);
        if email == me.email && (role != me.role || !active) {
            v.add("role", "自分自身のロール変更・無効化はできません");
        }
        if !seen.insert(email.clone()) {
            v.add("email", format!("メールアドレス {email} が CSV 内で重複しています"));
        }
        if let Err(e) = v.check() {
            errs.add_details(row.line, e);
            continue;
        }
        items.push(UserItem { name, email, role, active, slack_id, keep_slack: !row.has("slack_user_id") });
    }
    errs.check()?;

    for it in &mut items {
        let id: Option<i64> =
            tx.exec_first("SELECT id FROM users WHERE email = ? FOR UPDATE", (it.email.clone(),)).await?;
        let Some(id) = id else {
            tx.exec_drop(
                "INSERT INTO users (name, email, password_hash, role, is_active, slack_user_id) VALUES (?, ?, ?, ?, ?, ?)",
                p![it.name.clone(), it.email.clone(), UNUSABLE_PASSWORD_HASH, it.role.clone(), it.active, db::null_str(&it.slack_id)],
            )
            .await?;
            let id = db::last_id(&tx);
            let created = find_user(&mut tx, id, "").await?;
            rec.insert(&mut tx, "users", id, &created).await?;
            result.inserted += 1;
            continue;
        };
        let before = find_user(&mut tx, id, "").await?;
        if it.keep_slack {
            it.slack_id = before.slack_user_id.clone();
        }
        if before.name == it.name
            && before.role == it.role
            && before.is_active == it.active
            && before.slack_user_id == it.slack_id
        {
            result.unchanged += 1;
            continue;
        }
        tx.exec_drop(
            "UPDATE users SET name = ?, role = ?, is_active = ?, slack_user_id = ? WHERE id = ?",
            p![it.name.clone(), it.role.clone(), it.active, db::null_str(&it.slack_id), id],
        )
        .await?;
        if !it.active {
            tx.exec_drop("DELETE FROM sessions WHERE user_id = ?", (id,)).await?;
        }
        let after = find_user(&mut tx, id, "").await?;
        rec.update(&mut tx, "users", id, &before, &after).await?;
        result.updated += 1;
    }
    // 無効にしたユーザーに担当が残っていれば知らせる（CSV では付け替えない。docs/plan.md「2.15」）
    for it in items.iter().filter(|it| !it.active) {
        let row: Option<(String, i64, i64)> = tx
            .exec_first(
                "SELECT u.name,
                        (SELECT COUNT(*) FROM activities WHERE owner_user_id = u.id),
                        (SELECT COUNT(*) FROM units WHERE owner_user_id = u.id AND NOT is_archived)
                 FROM users u WHERE u.email = ?",
                (it.email.clone(),),
            )
            .await?;
        if let Some((name, acts, units)) = row
            && acts + units > 0
        {
            result.warnings.push(format!(
                    "無効なユーザー「{name}」に、担当の施策 {acts} 件・所管ユニット {units} 件が残っています。ユーザーの画面で後任者に付け替えてください"
                ));
        }
    }
    csvio::finish(tx, result).await
}

/// true / false（1 / 0、有効 / 無効も可）を読む。空欄は true とする。
pub fn parse_bool(s: &str) -> Option<bool> {
    match s.to_lowercase().as_str() {
        "" | "true" | "1" | "yes" | "有効" => Some(true),
        "false" | "0" | "no" | "無効" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depths_and_cycles() {
        let m: HashMap<String, String> = [("A", ""), ("B", "A"), ("C", "B"), ("X", "Y"), ("Y", "X")]
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        let (d, cyc) = depths(&m);
        assert_eq!((d["A"], d["B"], d["C"]), (0, 1, 2));
        assert!(cyc.contains("X") && cyc.contains("Y") && !cyc.contains("C"));
    }
}
