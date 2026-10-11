//! マスタ管理 API の統合テスト。Go 版の master_test.go・transfer_test.go（マスタの部分）・confidence_test.go。

mod common;

use common::*;
use reqwest::StatusCode;
use serde_json::{Value, json};

#[tokio::test]
async fn master_permissions() {
    let Some(env) = TestEnv::start().await else { return };
    env.create_user("viewer@example.com", "viewer").await;
    env.create_user("manager@example.com", "manager").await;

    assert_eq!(env.client().get("/api/organizations").await.0, StatusCode::UNAUTHORIZED, "未ログインの GET");

    for email in ["viewer@example.com", "manager@example.com"] {
        let c = env.client();
        c.login(email).await;
        for path in [
            "/api/organizations",
            "/api/segments",
            "/api/units",
            "/api/subjects",
            "/api/users",
            "/api/confidence-levels",
        ] {
            assert_eq!(c.get(path).await.0, StatusCode::OK, "{email} GET {path}");
            assert_eq!(c.post(path, json!({})).await.0, StatusCode::FORBIDDEN, "{email} POST {path}");
        }
    }
}

#[tokio::test]
async fn tree_hierarchy() {
    for path in ["/api/organizations", "/api/segments"] {
        let Some((c, _env)) = TestEnv::admin_client().await else { return };

        let root = c.must_create(path, json!({"name": "本部"})).await;
        let dept = c.must_create(path, json!({"name": "部", "parent_id": root})).await;
        let sect = c.must_create(path, json!({"name": "課", "parent_id": dept})).await;
        let other = c.must_create(path, json!({"name": "別本部"})).await;

        assert_eq!(c.must_get(&format!("{path}/{sect}")).await["level"], json!(3));

        // 部を別本部の下に移動すると、配下の課の level も変わらない（同じ深さ）
        let (status, body) = c.put(&format!("{path}/{dept}"), json!({"name": "部", "parent_id": other})).await;
        assert_eq!(status, StatusCode::OK, "移動: {body}");
        // 部をルートに移動すると、部は 1、課は 2 になる
        let (status, body) = c.put(&format!("{path}/{dept}"), json!({"name": "部", "parent_id": null})).await;
        assert_eq!(status, StatusCode::OK, "ルートへ移動: {body}");
        assert_eq!(c.must_get(&format!("{path}/{dept}")).await["level"], json!(1));
        assert_eq!(c.must_get(&format!("{path}/{sect}")).await["level"], json!(2));

        // 配下のノードを親にはできない（循環）
        let (status, body) = c.put(&format!("{path}/{dept}"), json!({"name": "部", "parent_id": sect})).await;
        assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "parent_id").is_empty(), "循環: {body}");
        // 自分自身も親にできない
        let (status, _) = c.put(&format!("{path}/{dept}"), json!({"name": "部", "parent_id": dept})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "自己参照");
        // 存在しない親
        assert_eq!(c.post(path, json!({"name": "x", "parent_id": 99999})).await.0, StatusCode::UNPROCESSABLE_ENTITY);
        // 名称は必須
        let (status, body) = c.post(path, json!({"name": "  "})).await;
        assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "name").is_empty(), "名称なし: {body}");

        // 子がいるノードは削除できない
        assert_eq!(c.delete(&format!("{path}/{dept}")).await.0, StatusCode::CONFLICT);
        assert_eq!(c.delete(&format!("{path}/{sect}")).await.0, StatusCode::NO_CONTENT);
        assert_eq!(c.get(&format!("{path}/{sect}")).await.0, StatusCode::NOT_FOUND);
        // ID の形が正しくなければ 404
        assert_eq!(c.get(&format!("{path}/abc")).await.0, StatusCode::NOT_FOUND);

        assert_eq!(items(&c.must_get(path).await).len(), 3);
    }
}

#[tokio::test]
async fn units_require_leaf_nodes() {
    let Some((c, _env)) = TestEnv::admin_client().await else { return };

    let seg_root = c.must_create("/api/segments", json!({"name": "XXX事業"})).await;
    let seg_leaf = c.must_create("/api/segments", json!({"name": "YYYサービス", "parent_id": seg_root})).await;
    let org_root = c.must_create("/api/organizations", json!({"name": "AAA本部"})).await;
    let org_leaf = c.must_create("/api/organizations", json!({"name": "BBB部", "parent_id": org_root})).await;

    // 末端でないノードには所属できない
    let (status, body) =
        c.post("/api/units", json!({"name": "CCC課", "segment_id": seg_root, "organization_id": org_leaf})).await;
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "segment_id").is_empty(),
        "末端でないセグメント: {body}"
    );
    // 存在しない参照
    let (status, body) = c
        .post(
            "/api/units",
            json!({"name": "CCC課", "segment_id": 99999, "organization_id": org_leaf, "owner_user_id": 99999}),
        )
        .await;
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY
            && !detail(&body, "segment_id").is_empty()
            && !detail(&body, "owner_user_id").is_empty(),
        "存在しない参照: {body}"
    );

    let unit = c
        .must_create("/api/units", json!({"name": "CCC課", "segment_id": seg_leaf, "organization_id": org_leaf}))
        .await;

    // ユニットが所属しているノードの下には子を作れない
    let (status, body) = c.post("/api/segments", json!({"name": "子", "parent_id": seg_leaf})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "parent_id").is_empty(), "{body}");
    // ユニットが所属しているノードは削除できない
    assert_eq!(c.delete(&format!("/api/organizations/{org_leaf}")).await.0, StatusCode::CONFLICT);

    // 更新
    let (status, body) = c
        .put(
            &format!("/api/units/{unit}"),
            json!({"name": "CCC課（改）", "segment_id": seg_leaf, "organization_id": org_leaf}),
        )
        .await;
    assert!(status == StatusCode::OK && body["name"] == "CCC課（改）", "更新: {body}");
    assert_eq!(c.delete(&format!("/api/units/{unit}")).await.0, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn unit_with_activity_cannot_be_deleted() {
    let Some((c, env)) = TestEnv::admin_client().await else { return };
    let seg = c.must_create("/api/segments", json!({"name": "S"})).await;
    let org = c.must_create("/api/organizations", json!({"name": "O"})).await;
    let unit = c.must_create("/api/units", json!({"name": "F", "segment_id": seg, "organization_id": org})).await;
    env.exec(
        "INSERT INTO activities (unit_id, code, name, activity_type, status, confidence_level) VALUES (?, 'ACT-1', 'a', 'project', 'active', 'C')",
        (unit,),
    )
    .await;
    assert_eq!(c.delete(&format!("/api/units/{unit}")).await.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn subjects() {
    let Some((c, _env)) = TestEnv::admin_client().await else { return };

    let sales = c.must_create("/api/subjects", json!({"code": "4000", "name": "売上高", "category": "revenue"})).await;
    let child = c
        .must_create(
            "/api/subjects",
            json!({"code": "4110", "name": "受託売上", "category": "revenue", "parent_id": sales}),
        )
        .await;
    let cost = c.must_create("/api/subjects", json!({"code": "8110", "name": "外注費", "category": "expense"})).await;

    for (name, body, field) in [
        ("コード重複", json!({"code": "4000", "name": "x", "category": "revenue"}), "code"),
        ("区分が不正", json!({"code": "9999", "name": "x", "category": "asset"}), "category"),
        (
            "親と区分が違う",
            json!({"code": "9998", "name": "x", "category": "expense", "parent_id": sales}),
            "parent_id",
        ),
    ] {
        let (status, res) = c.post("/api/subjects", body).await;
        assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&res, field).is_empty(), "{name}: {res}");
    }

    // 循環: 売上高の親を子の受託売上にはできない
    let (status, body) = c
        .put(
            &format!("/api/subjects/{sales}"),
            json!({"code": "4000", "name": "売上高", "category": "revenue", "parent_id": child}),
        )
        .await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "parent_id").is_empty(), "循環: {body}");
    // 子科目がある科目は削除できない
    assert_eq!(c.delete(&format!("/api/subjects/{sales}")).await.0, StatusCode::CONFLICT);
    assert_eq!(c.delete(&format!("/api/subjects/{cost}")).await.0, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn users() {
    let Some((c, env)) = TestEnv::admin_client().await else { return };

    let (status, body) = c
        .post(
            "/api/users",
            json!({"name": "山田", "email": "Yamada@Example.com", "role": "member", "password": "member-pass-123"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "作成: {body}");
    assert_eq!(body["email"], "yamada@example.com", "小文字に正規化");
    assert!(body.get("password_hash").is_none(), "レスポンスに password_hash が含まれている");
    assert_eq!(body["has_password"], json!(true));
    assert!(body["created_at"].as_str().unwrap().ends_with("+09:00"), "日時は日本時間: {}", body["created_at"]);
    let id = body["id"].as_i64().unwrap();

    for (field, req) in [
        ("email", json!({"name": "x", "email": "yamada@example.com", "role": "member", "password": "member-pass-123"})),
        ("role", json!({"name": "x", "email": "x@example.com", "role": "owner", "password": "member-pass-123"})),
        ("password", json!({"name": "x", "email": "y@example.com", "role": "member", "password": "short"})),
        ("email", json!({"name": "x", "email": "a@b@example.com", "role": "member", "password": "member-pass-123"})),
    ] {
        let (status, body) = c.post("/api/users", req).await;
        assert!(
            status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, field).is_empty(),
            "{field} の検証: {body}"
        );
    }

    // 自分自身のロール変更・無効化はできない
    let me = format!("/api/users/{}", env.admin_id);
    let (status, _) =
        c.put(&me, json!({"name": "管理者", "email": "admin@example.com", "role": "viewer", "is_active": true})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "自分のロール変更");
    let (status, _) = c
        .put(&me, json!({"name": "管理者", "email": "admin@example.com", "role": "fpa_admin", "is_active": false}))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "自分の無効化");

    // パスワード再設定で既存セッションが無効になり、新しいパスワードでログインできる
    let member = env.client();
    let (status, _) =
        member.post("/api/auth/login", json!({"email": "yamada@example.com", "password": "member-pass-123"})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = c
        .put(
            &format!("/api/users/{id}/password"),
            json!({"password": "new-member-pass-456", "reason": "パスワード忘れ"}),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "パスワード再設定");
    assert_eq!(member.get("/api/auth/me").await.0, StatusCode::UNAUTHORIZED, "再設定後の旧セッション");
    let (status, _) =
        member.post("/api/auth/login", json!({"email": "yamada@example.com", "password": "new-member-pass-456"})).await;
    assert_eq!(status, StatusCode::OK, "新しいパスワードでのログイン");

    // 無効化するとセッションが切れる
    let (status, body) = c
        .put(
            &format!("/api/users/{id}"),
            json!({"name": "山田", "email": "yamada@example.com", "role": "member", "is_active": false}),
        )
        .await;
    assert!(status == StatusCode::OK && body["is_active"] == json!(false), "無効化: {body}");
    assert_eq!(member.get("/api/auth/me").await.0, StatusCode::UNAUTHORIZED, "無効化後のセッション");

    // 監査ログにパスワードハッシュが残らない
    let n: i64 = env
        .query_one(
            "SELECT COUNT(*) FROM audit_logs WHERE table_name = 'users' AND (JSON_SEARCH(after_json, 'one', 'pbkdf2%') IS NOT NULL OR JSON_CONTAINS_PATH(after_json, 'one', '$.password_hash'))",
            (),
        )
        .await;
    assert_eq!(n, 0, "監査ログにパスワードハッシュが含まれている");
}

#[tokio::test]
async fn master_changes_are_audited() {
    let Some((c, env)) = TestEnv::admin_client().await else { return };

    let id = c.must_create("/api/organizations", json!({"name": "経営企画部", "reason": "組織改編"})).await;
    assert_eq!(c.put(&format!("/api/organizations/{id}"), json!({"name": "FP&A部"})).await.0, StatusCode::OK);
    let (status, _) = c.do_req("DELETE", &format!("/api/organizations/{id}"), Some(json!({"reason": "廃止"}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let got: Vec<(String, i64, String, bool, bool, String)> = env
        .query(
            "SELECT a.action, cs.user_id, COALESCE(cs.reason, ''), a.before_json IS NULL, a.after_json IS NULL,
                    COALESCE(JSON_UNQUOTE(JSON_EXTRACT(a.after_json, '$.name')), '')
             FROM audit_logs a JOIN change_sets cs ON cs.id = a.change_set_id
             WHERE a.table_name = 'organizations' AND a.record_id = ?
             ORDER BY a.id",
            (id,),
        )
        .await;
    let a = env.admin_id;
    assert_eq!(
        got,
        vec![
            ("insert".into(), a, "組織改編".into(), true, false, "経営企画部".into()),
            ("update".into(), a, "".into(), false, false, "FP&A部".into()),
            ("delete".into(), a, "廃止".into(), false, true, "".into()),
        ]
    );
    // 監査ログの JSON は Go 版と同じ形（日時は日本時間の RFC 3339）
    let after: String = env
        .query_one(
            "SELECT CAST(after_json AS CHAR) FROM audit_logs WHERE table_name = 'organizations' AND action = 'insert'",
            (),
        )
        .await;
    let v: Value = serde_json::from_str(&after).unwrap();
    for key in ["id", "parent_id", "code", "name", "level", "sort_order", "created_at", "updated_at"] {
        assert!(v.get(key).is_some(), "監査ログに {key} がない: {after}");
    }
    assert!(v["created_at"].as_str().unwrap().ends_with("+09:00"));
}

#[tokio::test]
async fn master_rejects_malformed_json() {
    let Some((c, _env)) = TestEnv::admin_client().await else { return };
    let res = c
        .http
        .post(format!("{}/api/organizations", c.base))
        .header("Content-Type", "application/json")
        .body(r#"{"name":"x","unknown":1}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST, "未知のフィールド");
    let res = c.http.post(format!("{}/api/organizations", c.base)).body(r#"{"name":"x"} {}"#).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST, "複数の値");
    // null は値なしとして扱う（Go の encoding/json と同じ）
    let (status, body) = c.post("/api/organizations", json!({"name": "x", "code": null, "sort_order": null})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn unit_types() {
    let Some((c, _env)) = TestEnv::admin_client().await else { return };
    let seg = c.must_create("/api/segments", json!({"name": "S"})).await;
    let org = c.must_create("/api/organizations", json!({"name": "O"})).await;
    let body = |name: &str, unit_type: &str| {
        let mut b = json!({"name": name, "segment_id": seg, "organization_id": org});
        if !unit_type.is_empty() {
            b["unit_type"] = json!(unit_type);
        }
        b
    };

    // 省略時はサービス
    let id = c.must_create("/api/units", body("SaaSサービス", "")).await;
    assert_eq!(c.must_get(&format!("/api/units/{id}")).await["unit_type"], "service");
    let cc = c.must_create("/api/units", body("事業共通経費", "cost_center")).await;
    assert_eq!(c.must_get(&format!("/api/units/{cc}")).await["unit_type"], "cost_center");
    let (status, res) = c.post("/api/units", body("x", "profit")).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&res, "unit_type").is_empty(), "不正な種別: {res}");
    let (status, res) = c.put(&format!("/api/units/{cc}"), body("人事部", "corporate")).await;
    assert!(status == StatusCode::OK && res["unit_type"] == "corporate", "種別の変更: {res}");
}

#[tokio::test]
async fn master_codes() {
    let Some((c, _env)) = TestEnv::admin_client().await else { return };
    // 組織・セグメント・ユニットのコードは、空なら自動採番、指定もできる
    let org = c.must_create("/api/organizations", json!({"name": "本部"})).await;
    assert_eq!(c.must_get(&format!("/api/organizations/{org}")).await["code"], "ORG-0001");
    c.must_create("/api/organizations", json!({"name": "HR", "code": "HR-100"})).await;
    let seg = c.must_create("/api/segments", json!({"name": "事業"})).await;
    assert_eq!(c.must_get(&format!("/api/segments/{seg}")).await["code"], "SEG-0001");
    let unit = c.must_create("/api/units", json!({"name": "U", "segment_id": seg, "organization_id": org})).await;
    assert_eq!(c.must_get(&format!("/api/units/{unit}")).await["code"], "UNIT-0001");
    let org2 = c.must_create("/api/organizations", json!({"name": "本部2"})).await;
    assert_eq!(c.must_get(&format!("/api/organizations/{org2}")).await["code"], "ORG-0002");

    for (name, path, body) in [
        ("組織コードの重複", "/api/organizations", json!({"name": "x", "code": "HR-100"})),
        ("組織コードの形式", "/api/organizations", json!({"name": "x", "code": "人事"})),
        (
            "ユニットコードの重複",
            "/api/units",
            json!({"name": "x", "code": "UNIT-0001", "segment_id": seg, "organization_id": org}),
        ),
    ] {
        let (status, res) = c.post(path, body).await;
        assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&res, "code").is_empty(), "{name}: {res}");
    }

    // 更新でコードを省略すると今のコードのまま、指定すると変わる
    let (status, body) = c.put(&format!("/api/organizations/{org}"), json!({"name": "本部（改）"})).await;
    assert!(status == StatusCode::OK && body["code"] == "ORG-0001", "コード省略の更新: {body}");
    let (status, body) = c
        .put(
            &format!("/api/units/{unit}"),
            json!({"name": "U", "code": "SAAS", "segment_id": seg, "organization_id": org}),
        )
        .await;
    assert!(status == StatusCode::OK && body["code"] == "SAAS", "コードの変更: {body}");
}

#[tokio::test]
async fn confidence_levels() {
    let Some((admin, env)) = TestEnv::admin_client().await else { return };
    env.create_user("manager1@example.com", "manager").await;
    let manager = env.client();
    manager.login("manager1@example.com").await;

    // 初期値は A〜E
    let list = admin.must_get("/api/confidence-levels").await;
    let levels = items(&list);
    assert_eq!(levels.len(), 5, "初期の段階");
    let b = &levels[1];
    assert!(b["code"] == "B" && serde_json::to_string(&b["rate"]).unwrap() == "0.8" && b["criteria"] != "", "B = {b}");

    // 作成・更新は FP&A のみ
    let (status, _) = manager.post("/api/confidence-levels", json!({"code": "F", "name": "x", "rate": 0.1})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    for (name, body, field) in [
        ("コード形式", json!({"code": "a b", "name": "x", "rate": 0.1}), "code"),
        ("コード重複", json!({"code": "A", "name": "x", "rate": 0.1}), "code"),
        ("確率が範囲外", json!({"code": "F", "name": "x", "rate": 1.2}), "rate"),
        ("確率の桁数", json!({"code": "F", "name": "x", "rate": 0.12345}), "rate"),
        ("名前なし", json!({"code": "F", "rate": 0.1}), "name"),
    ] {
        let (status, res) = admin.post("/api/confidence-levels", body).await;
        assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&res, field).is_empty(), "{name}: {res}");
    }
    let id = admin
        .must_create(
            "/api/confidence-levels",
            json!({"code": "F", "name": "撤退検討", "rate": "0.05", "criteria": "撤退の検討を始めた", "sort_order": 6}),
        )
        .await;
    let path = format!("/api/confidence-levels/{id}");
    let (status, body) = admin
        .put(
            &path,
            json!({"code": "G", "name": "撤退検討", "rate": 0.1, "criteria": "撤退の検討を始めた", "sort_order": 6}),
        )
        .await;
    assert!(
        status == StatusCode::OK && body["code"] == "F" && serde_json::to_string(&body["rate"]).unwrap() == "0.1",
        "更新（コードは変わらない）: {body}"
    );
    assert_eq!(admin.delete(&path).await.0, StatusCode::NO_CONTENT);
}

// --- CSV ---

#[tokio::test]
async fn tree_import_export() {
    let Some((c, _env)) = TestEnv::admin_client().await else { return };
    c.must_create("/api/organizations", json!({"name": "本部", "code": "HQ"})).await;

    // 同じ CSV 内の新しい親の下に子を追加できる。既存の名称も更新できる
    let csv = "code,name,parent_code,sort_order\nDEPT-A,営業部,HQ,1\nSEC-A1,営業一課,DEPT-A,0\nHQ,本社,,0\n";
    let (status, body) = c.upload("/api/organizations/import?dry_run=true", csv, "").await;
    assert!(status == StatusCode::OK && result_counts(&body) == "inserted=2 updated=1 unchanged=0", "dry run: {body}");
    assert_eq!(items(&c.must_get("/api/organizations").await).len(), 1, "dry run なのに保存された");
    let (status, body) = c.upload("/api/organizations/import", csv, "組織の一括登録").await;
    assert!(status == StatusCode::OK && result_counts(&body) == "inserted=2 updated=1 unchanged=0", "取込: {body}");
    let levels = |list: &Value| -> Vec<(String, i64)> {
        let mut v: Vec<(String, i64)> = items(list)
            .iter()
            .map(|m| (m["code"].as_str().unwrap().to_string(), m["level"].as_i64().unwrap()))
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        levels(&c.must_get("/api/organizations").await),
        vec![("DEPT-A".into(), 2), ("HQ".into(), 1), ("SEC-A1".into(), 3)]
    );

    // エクスポート → そのまま取込で変更なし（往復できる）
    let exported = c.download("/api/organizations/export").await;
    assert!(exported.contains("SEC-A1,営業一課,DEPT-A,0\r\n"), "エクスポート = {exported:?}");
    let (status, body) = c.upload("/api/organizations/import", &exported, "再取込").await;
    assert!(status == StatusCode::OK && result_counts(&body) == "inserted=0 updated=0 unchanged=3", "往復: {body}");

    // 親を付け替えると配下の階層レベルも変わる（課は CSV に含めない）
    let (status, body) =
        c.upload("/api/organizations/import", "code,name,parent_code,sort_order\nDEPT-A,営業部,,1\n", "組織改編").await;
    assert!(status == StatusCode::OK && result_counts(&body) == "inserted=0 updated=1 unchanged=0", "付け替え: {body}");
    assert!(levels(&c.must_get("/api/organizations").await).contains(&("SEC-A1".into(), 2)));

    // エラー: 循環・存在しない親・コード形式・重複・ユニットが所属するノードへの子追加
    let seg = c.must_create("/api/segments", json!({"name": "S"})).await;
    let sec = items(&c.must_get("/api/organizations").await).iter().find(|m| m["code"] == "SEC-A1").unwrap()["id"]
        .as_i64()
        .unwrap();
    c.must_create("/api/units", json!({"name": "U", "segment_id": seg, "organization_id": sec})).await;
    let bad = "code,name,parent_code,sort_order\n\
               HQ,本社,SEC-A1,0\n\
               X1,x,NOPE,0\n\
               人事,x,,0\n\
               Y1,y,Y2,0\n\
               Y2,y,Y1,0\n\
               DEPT-A,営業部,DEPT-A,0\n";
    // 行の形式のエラーがあれば、構造（親・循環）の確認の前に止まる
    let (status, body) = c.upload("/api/organizations/import", bad, "r").await;
    let errs = row_errors(&body);
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && errs.len() == 2, "エラー: {errs:?}");
    let joined = errs.join("\n");
    for want in ["4:コード", "7:自分自身"] {
        assert!(joined.contains(want), "エラーに {want:?} がない: {errs:?}");
    }
    // 形式エラーを除いたものでも、存在しない親と循環は拒否される
    for (csv, want) in [
        ("code,name,parent_code,sort_order\nX1,x,NOPE,0\n", "見つかりません"),
        ("code,name,parent_code,sort_order\nY1,y,Y2,0\nY2,y,Y1,0\n", "循環"),
        ("code,name,parent_code,sort_order\nZ1,z,SEC-A1,0\n", "ユニットが所属"),
    ] {
        let (status, body) = c.upload("/api/organizations/import", csv, "r").await;
        assert!(
            status == StatusCode::UNPROCESSABLE_ENTITY && row_errors(&body).join("").contains(want),
            "{want}: {body}"
        );
    }
    // ヘッダー違い・理由なし
    assert_eq!(c.upload("/api/organizations/import", "code,name\nA,B\n", "r").await.0, StatusCode::BAD_REQUEST);
    let (status, body) = c.upload("/api/organizations/import", "code,name,parent_code,sort_order\nA,B,,0\n", "").await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "reason").is_empty(), "理由なし: {body}");
    // ファイルなし
    let (status, body) = c.upload("/api/organizations/import", "", "r").await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "file").is_empty(), "ファイルなし: {body}");
}

#[tokio::test]
async fn unit_subject_user_import() {
    let Some((a, env)) = TestEnv::admin_client().await else { return };
    env.create_user("manager1@example.com", "manager").await;
    env.create_user("member2@example.com", "member").await;
    env.create_user("viewer@example.com", "viewer").await;
    let member2 = env.client();
    member2.login("member2@example.com").await;
    let viewer = env.client();
    viewer.login("viewer@example.com").await;
    a.must_create("/api/segments", json!({"name": "S"})).await;
    a.must_create("/api/organizations", json!({"name": "O"})).await;

    // ユニット: セグメント・組織・担当者をコード・メールアドレスで指定
    let segs = a.download("/api/segments/export").await;
    let orgs = a.download("/api/organizations/export").await;
    let first_code = |s: &str| s.split("\r\n").nth(1).unwrap().split(',').next().unwrap().to_string();
    let (seg_code, org_code) = (first_code(&segs), first_code(&orgs));
    let csv = format!(
        "code,name,unit_type,segment_code,organization_code,owner_email\nCC-1,共通経費,cost_center,{seg_code},{org_code},manager1@example.com\n"
    );
    let (status, body) = a.upload("/api/units/import", &csv, "ユニット追加").await;
    assert!(
        status == StatusCode::OK && result_counts(&body) == "inserted=1 updated=0 unchanged=0",
        "ユニット取込: {body}"
    );
    let exported = a.download("/api/units/export").await;
    assert!(
        exported.contains(&format!("CC-1,共通経費,cost_center,{seg_code},{org_code},manager1@example.com")),
        "{exported:?}"
    );
    let (status, body) = a.upload("/api/units/import", &exported, "再取込").await;
    assert!(
        status == StatusCode::OK && body["updated"] == json!(0) && body["inserted"] == json!(0),
        "ユニットの往復: {body}"
    );
    let bad = format!(
        "code,name,unit_type,segment_code,organization_code,owner_email\n\
         CC-2,x,service,NOPE,{org_code},\n\
         CC-3,x,service,{seg_code},{org_code},nobody@example.com\n\
         CC-4,x,profit,{seg_code},{org_code},\n"
    );
    let (status, body) = a.upload("/api/units/import", &bad, "r").await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && row_errors(&body).len() == 3, "ユニットのエラー: {body}");

    // 勘定科目: 親と区分が違えばエラー
    let csv = "code,name,category,parent_code,sort_order\n4000,売上高,revenue,,1\n4100,受託売上,revenue,4000,1\n8000,外注費,expense,4000,2\n";
    let (status, body) = a.upload("/api/subjects/import", csv, "科目登録").await;
    let errs = row_errors(&body);
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY && errs.len() == 1 && errs[0].contains("4:"),
        "区分違い: {errs:?}"
    );
    let csv = "code,name,category,parent_code,sort_order\n4100,受託売上,revenue,4000,1\n4000,売上高,revenue,,1\n";
    let (status, body) = a.upload("/api/subjects/import", csv, "科目登録").await;
    assert!(
        status == StatusCode::OK && result_counts(&body) == "inserted=2 updated=0 unchanged=0",
        "子が先の行でも可: {body}"
    );
    let exported = a.download("/api/subjects/export").await;
    let (status, body) = a.upload("/api/subjects/import", &exported, "再取込").await;
    assert!(status == StatusCode::OK && body["unchanged"] == json!(2), "科目の往復: {body}");

    // ユーザー: 追加したユーザーはパスワード未設定でログインできない
    let csv = "email,name,role,is_active\nnew@example.com,新規 太郎,member,true\nmember2@example.com,担当 二郎,member,false\n";
    let (status, body) = a.upload("/api/users/import", csv, "ユーザー登録").await;
    assert!(
        status == StatusCode::OK && result_counts(&body) == "inserted=1 updated=1 unchanged=0",
        "ユーザー取込: {body}"
    );
    let users = a.must_get("/api/users").await;
    let created = items(&users).iter().find(|m| m["email"] == "new@example.com").expect("追加したユーザー").clone();
    assert_eq!(created["has_password"], json!(false));
    let anon = env.client();
    let (status, _) = anon.post("/api/auth/login", json!({"email": "new@example.com", "password": "!"})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "パスワード未設定のログイン");
    // 無効化したユーザーのセッションは切れる
    assert_eq!(member2.get("/api/auth/me").await.0, StatusCode::UNAUTHORIZED);
    // パスワードを設定すればログインできる
    a.put(&format!("/api/users/{}/password", created["id"]), json!({"password": "new-user-pass-123"})).await;
    let (status, _) =
        anon.post("/api/auth/login", json!({"email": "new@example.com", "password": "new-user-pass-123"})).await;
    assert_eq!(status, StatusCode::OK, "パスワード設定後のログイン");
    // 自分自身の無効化はできない
    let (status, body) = a
        .upload("/api/users/import", "email,name,role,is_active\nadmin@example.com,管理者,fpa_admin,false\n", "r")
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "自分の無効化: {body}");
    // 閲覧者はエクスポートできるが、取込はできない
    viewer.download("/api/users/export").await;
    assert_eq!(viewer.upload("/api/users/import", csv, "r").await.0, StatusCode::FORBIDDEN);
}
