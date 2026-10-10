//! 施策 API の統合テスト。Go 版の activity_test.go・external_code_test.go・transfer_test.go（施策）・confidence_test.go（施策）。
//! シナリオ・実績・変更履歴を使う確認は、それらを移植する段階でこのファイルに足す。

mod common;

use common::*;
use reqwest::StatusCode;
use serde_json::{Value, json};

fn codes_of(body: &Value) -> Vec<String> {
    items(body).iter().map(|it| it["code"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn activity_permissions() {
    let Some(f) = ActivityFixture::new().await else { return };

    // 作成: FP&A はどこでも、マネージャーは担当ユニットのみ、担当者・閲覧者は不可
    f.admin.must_create("/api/activities", activity_body(f.unit2, "ACT-ADMIN", json!({}))).await;
    let id = f
        .manager1
        .must_create("/api/activities", activity_body(f.unit1, "ACT-1", json!({"owner_user_id": f.member_id})))
        .await;
    for (name, c, unit) in [
        ("他ユニットのマネージャー", &f.manager1, f.unit2),
        ("担当者", &f.member, f.unit1),
        ("閲覧者", &f.viewer, f.unit1),
    ] {
        let (status, _) = c.post("/api/activities", activity_body(unit, "ACT-X", json!({}))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{name} の作成");
    }
    // 担当者は入力が正しくなくても 403（権限の確認が先）
    let (status, _) = f.member.post("/api/activities", json!({"unknown": 1})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let path = format!("/api/activities/{id}");
    let update = activity_body(f.unit1, "ACT-1", json!({"owner_user_id": f.member_id, "name": "更新後"}));

    // 編集: 担当ユニットのマネージャー・施策の担当者は可、それ以外は不可
    for (name, c) in [("担当ユニットのマネージャー", &f.manager1), ("施策の担当者", &f.member)] {
        let (status, body) = c.put(&path, update.clone()).await;
        assert_eq!(status, StatusCode::OK, "{name} の編集: {body}");
    }
    for (name, c) in [("他ユニットのマネージャー", &f.manager2), ("担当外の担当者", &f.member2), ("閲覧者", &f.viewer)]
    {
        assert_eq!(c.put(&path, update.clone()).await.0, StatusCode::FORBIDDEN, "{name} の編集");
    }

    // can_edit は閲覧者ごとに変わる
    assert_eq!(f.member.must_get(&path).await["can_edit"], json!(true));
    assert_eq!(f.member2.must_get(&path).await["can_edit"], json!(false));

    // 別のユニットへの移動は、移動先で作成できる権限が必要
    let moved = activity_body(f.unit2, "ACT-1", json!({"owner_user_id": f.member_id}));
    for (name, c) in [("マネージャー", &f.manager1), ("担当者", &f.member)] {
        assert_eq!(c.put(&path, moved.clone()).await.0, StatusCode::FORBIDDEN, "{name} による他ユニットへの移動");
    }

    // 削除: 担当者は不可、担当ユニットのマネージャーは可
    let del = json!({"reason": "施策の中止"});
    assert_eq!(f.member.do_req("DELETE", &path, Some(del.clone())).await.0, StatusCode::FORBIDDEN);
    let (status, body) = f.manager1.do_req("DELETE", &path, Some(del)).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "マネージャーの削除: {body}");
}

#[tokio::test]
async fn activity_validation() {
    let Some(f) = ActivityFixture::new().await else { return };
    f.admin.must_create("/api/activities", activity_body(f.unit1, "ACT-1", json!({}))).await;

    for (name, overrides, code, field) in [
        ("コード重複", json!({}), "ACT-1", "code"),
        ("コードに使えない文字", json!({}), "ACT 2", "code"),
        ("施策タイプ不正", json!({"activity_type": "flow"}), "ACT-2", "activity_type"),
        ("ステータス不正", json!({"status": "done"}), "ACT-2", "status"),
        ("プロジェクト型の期間なし", json!({"activity_type": "project"}), "ACT-2", "start_date"),
        ("終了日が開始日より前", json!({"start_date": "2026-10-01", "end_date": "2026-09-30"}), "ACT-2", "end_date"),
        ("日付形式", json!({"start_date": "2026/10/01"}), "ACT-2", "start_date"),
        ("確度の段階が未登録", json!({"confidence_level": "Z"}), "ACT-2", "confidence_level"),
        ("存在しない担当者", json!({"owner_user_id": 99999}), "ACT-2", "owner_user_id"),
    ] {
        let (status, body) = f.admin.post("/api/activities", activity_body(f.unit1, code, overrides)).await;
        assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, field).is_empty(), "{name}: {body}");
    }

    // プロジェクト型は期間を入れれば作成できる
    let (status, body) = f
        .admin
        .post(
            "/api/activities",
            activity_body(
                f.unit1,
                "PRJ-1",
                json!({"activity_type": "project", "start_date": "2026-04-01", "end_date": "2027-03-31", "confidence_level": "B"}),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "プロジェクト型の作成: {body}");
    assert!(body["confidence_level"] == "B" && body["end_date"] == "2027-03-31", "作成結果 = {body}");
    assert_eq!(body["start_date"], "2026-04-01");
    assert_eq!(body["can_edit"], json!(true));
}

#[tokio::test]
async fn activity_changes_require_reason() {
    let Some(f) = ActivityFixture::new().await else { return };
    let id =
        f.admin.must_create("/api/activities", activity_body(f.unit1, "ACT-1", json!({"confidence_level": "C"}))).await;
    let path = format!("/api/activities/{id}");

    // 名称だけの変更は理由不要
    let (status, body) =
        f.admin.put(&path, activity_body(f.unit1, "ACT-1", json!({"confidence_level": "C", "name": "改名"}))).await;
    assert_eq!(status, StatusCode::OK, "名称変更: {body}");

    for (name, overrides) in [
        ("確度", json!({"confidence_level": "B"})),
        ("前提条件", json!({"confidence_level": "C", "assumptions": "大型案件の受注が前提"})),
        ("期間", json!({"confidence_level": "C", "start_date": "2026-10-01"})),
    ] {
        let mut body = activity_body(f.unit1, "ACT-1", overrides);
        body["name"] = json!("改名");
        let (status, res) = f.admin.put(&path, body.clone()).await;
        assert!(
            status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&res, "reason").is_empty(),
            "{name}（理由なし）: {res}"
        );
        body["reason"] = json!("受注見込みの上方修正");
        let (status, res) = f.admin.put(&path, body).await;
        assert_eq!(status, StatusCode::OK, "{name}（理由あり）: {res}");
        // 次のケースのために元に戻す
        f.admin
            .put(
                &path,
                activity_body(f.unit1, "ACT-1", json!({"confidence_level": "C", "name": "改名", "reason": "元に戻す"})),
            )
            .await;
    }

    // 削除は理由が必須
    let (status, body) = f.admin.delete(&path).await;
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "reason").is_empty(),
        "理由なしの削除: {body}"
    );
}

#[tokio::test]
async fn drivers_and_lines() {
    let Some(f) = ActivityFixture::new().await else { return };
    let sales =
        f.admin.must_create("/api/subjects", json!({"code": "4110", "name": "受託売上", "category": "revenue"})).await;
    let id = f
        .manager1
        .must_create("/api/activities", activity_body(f.unit1, "ACT-1", json!({"owner_user_id": f.member_id})))
        .await;
    let base = format!("/api/activities/{id}");

    // ドライバーは担当者も追加できる
    let price = f
        .member
        .must_create(
            &format!("{base}/drivers"),
            json!({"code": "unit_price", "name": "単価", "driver_kind": "value", "unit": "円"}),
        )
        .await;
    f.member
        .must_create(
            &format!("{base}/drivers"),
            json!({"code": "volume", "name": "件数", "driver_kind": "kpi", "unit": "件"}),
        )
        .await;

    for (name, body, field) in [
        ("コード重複", json!({"code": "volume", "name": "x", "driver_kind": "kpi"}), "code"),
        ("コード形式", json!({"code": "Volume", "name": "x", "driver_kind": "kpi"}), "code"),
        ("種別不正", json!({"code": "x", "name": "x", "driver_kind": "money"}), "driver_kind"),
    ] {
        let (status, res) = f.member.post(&format!("{base}/drivers"), body).await;
        assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&res, field).is_empty(), "{name}: {res}");
    }

    let lbase = format!("{base}/lines");
    for (name, body, field) in [
        (
            "理由なし",
            json!({"subject_id": sales, "name": "利用料", "expression": "unit_price * volume", "formula_enabled": true}),
            "reason",
        ),
        ("科目なし", json!({"name": "利用料", "reason": "r"}), "subject_id"),
        ("名前なし", json!({"subject_id": sales, "reason": "r"}), "name"),
        (
            "反映するのに式なし",
            json!({"subject_id": sales, "name": "利用料", "formula_enabled": true, "reason": "r"}),
            "expression",
        ),
        (
            "構文エラー",
            json!({"subject_id": sales, "name": "利用料", "expression": "unit_price *", "reason": "r"}),
            "expression",
        ),
        (
            "未定義のドライバー",
            json!({"subject_id": sales, "name": "利用料", "expression": "unit_price * qty", "reason": "r"}),
            "expression",
        ),
    ] {
        let (status, res) = f.member.post(&lbase, body).await;
        assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&res, field).is_empty(), "内訳 {name}: {res}");
    }

    // 直接入力の内訳は理由なしで作れる。計算式で反映する内訳は理由が必須
    f.member.must_create(&lbase, json!({"subject_id": sales, "name": "スポット"})).await;
    let lid = f
        .member
        .must_create(
            &lbase,
            json!({"subject_id": sales, "name": "利用料", "expression": "unit_price * volume * 2", "formula_enabled": true, "reason": "式で算出する"}),
        )
        .await;
    let (status, body) = f.member.post(&lbase, json!({"subject_id": sales, "name": "利用料"})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "name").is_empty(), "同名の内訳: {body}");
    let lpath = format!("{lbase}/{lid}");
    let (status, body) = f
        .member
        .put(&lpath, json!({"name": "月額利用料", "expression": "unit_price * volume * 2", "formula_enabled": true}))
        .await;
    assert_eq!(status, StatusCode::OK, "名前だけの変更（理由なし）: {body}");
    let (status, body) = f
        .member
        .put(&lpath, json!({"name": "月額利用料", "expression": "unit_price * volume", "formula_enabled": true}))
        .await;
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "reason").is_empty(),
        "式の変更（理由なし）: {body}"
    );
    let (status, _) = f
        .member
        .put(&lpath, json!({"name": "月額利用料", "expression": "unit_price * volume", "formula_enabled": true, "reason": "確度は別で管理"}))
        .await;
    assert_eq!(status, StatusCode::OK, "式の変更");
    assert_eq!(
        f.member.put(&format!("{lbase}/99999"), json!({"name": "x", "reason": "r"})).await.0,
        StatusCode::NOT_FOUND
    );

    // 計算式で使われているドライバーは、コード変更も削除もできない
    let dpath = format!("{base}/drivers/{price}");
    let (status, body) = f.member.put(&dpath, json!({"code": "price", "name": "単価", "driver_kind": "value"})).await;
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "code").is_empty(),
        "使用中のコード変更: {body}"
    );
    let (status, _) =
        f.member.put(&dpath, json!({"code": "unit_price", "name": "販売単価", "driver_kind": "value"})).await;
    assert_eq!(status, StatusCode::OK, "名称のみの変更");
    assert_eq!(f.member.delete(&dpath).await.0, StatusCode::CONFLICT, "使用中の削除");

    // 詳細にドライバー・内訳が含まれる
    let detail_body = f.viewer.must_get(&base).await;
    assert_eq!(detail_body["drivers"].as_array().unwrap().len(), 2);
    assert_eq!(detail_body["external_codes"], json!([]));
    assert_eq!(detail_body["milestones"], json!([]));
    let lines = detail_body["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 2, "{lines:?}");
    for l in lines {
        if l["name"] == "月額利用料" {
            assert!(l["expression"] == "unit_price * volume" && l["formula_enabled"] == json!(true), "内訳 = {l}");
        }
    }

    // 計算式を使う内訳を消せばドライバーも消せる（反映しない式も参照に数える）
    let (status, _) = f
        .member
        .put(&lpath, json!({"name": "月額利用料", "expression": "unit_price * volume", "formula_enabled": false, "reason": "直接入力に切り替え"}))
        .await;
    assert_eq!(status, StatusCode::OK, "反映をやめる");
    assert_eq!(f.member.delete(&dpath).await.0, StatusCode::CONFLICT, "反映しない式で使用中の削除");
    assert_eq!(f.member.delete(&lpath).await.0, StatusCode::UNPROCESSABLE_ENTITY, "理由なしの内訳削除");
    assert_eq!(
        f.member.do_req("DELETE", &lpath, Some(json!({"reason": "内訳の整理"}))).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(f.member.delete(&dpath).await.0, StatusCode::NO_CONTENT, "ドライバーの削除");

    // 担当外のユーザーはドライバーを追加できない
    let (status, _) =
        f.member2.post(&format!("{base}/drivers"), json!({"code": "x", "name": "x", "driver_kind": "kpi"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn driver_order() {
    let Some(f) = ActivityFixture::new().await else { return };
    let id = f.admin.must_create("/api/activities", activity_body(f.unit1, "ACT-1", json!({}))).await;
    let base = format!("/api/activities/{id}/drivers");
    let a = f.admin.must_create(&base, json!({"code": "a", "name": "A", "driver_kind": "kpi"})).await;
    let b = f.admin.must_create(&base, json!({"code": "b", "name": "B", "driver_kind": "kpi"})).await;
    let c = f.admin.must_create(&base, json!({"code": "c", "name": "C", "driver_kind": "kpi"})).await;

    let (status, body) = f.admin.put(&format!("{base}/order"), json!({"ids": [c, a, b]})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(codes_of(&body), vec!["c", "a", "b"]);
    for (name, ids) in [("重複", json!([c, c, a])), ("足りない", json!([c, a])), ("他の施策", json!([c, a, 99999]))]
    {
        let (status, body) = f.admin.put(&format!("{base}/order"), json!({ "ids": ids })).await;
        assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "ids").is_empty(), "{name}: {body}");
    }
    // 並べ替えたドライバーだけが監査ログに残る（c: 3→1, a: 1→2, b: 2→3）
    let n: i64 = f
        .env
        .query_one("SELECT COUNT(*) FROM audit_logs WHERE table_name = 'activity_drivers' AND action = 'update'", ())
        .await;
    assert_eq!(n, 3);
}

#[tokio::test]
async fn milestones() {
    let Some(f) = ActivityFixture::new().await else { return };
    let id = f
        .admin
        .must_create("/api/activities", activity_body(f.unit1, "ACT-1", json!({"owner_user_id": f.member_id})))
        .await;
    let base = format!("/api/activities/{id}/milestones");

    let (status, body) = f.member.post(&base, json!({"name": "要件定義完了", "due_date": "2026-11-30"})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body["due_date"] == "2026-11-30" && body["status"] == "not_started", "{body}");
    let mid = body["id"].as_i64().unwrap();
    let mpath = format!("{base}/{mid}");

    let (status, body) = f.member.post(&base, json!({"name": "x", "due_date": ""})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "due_date").is_empty(), "期日なし: {body}");
    // ステータスだけの変更は理由不要、期日の変更は理由が必須
    let (status, body) =
        f.member.put(&mpath, json!({"name": "要件定義完了", "due_date": "2026-11-30", "status": "in_progress"})).await;
    assert_eq!(status, StatusCode::OK, "ステータス変更: {body}");
    let (status, body) =
        f.member.put(&mpath, json!({"name": "要件定義完了", "due_date": "2026-12-15", "status": "delayed"})).await;
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "reason").is_empty(),
        "理由なしの期日変更: {body}"
    );
    let (status, _) = f
        .member
        .put(&mpath, json!({"name": "要件定義完了", "due_date": "2026-12-15", "status": "delayed", "reason": "顧客レビューの遅れ"}))
        .await;
    assert_eq!(status, StatusCode::OK, "理由ありの期日変更");
    // 別の施策のマイルストーンとしては扱えない
    let other = f.admin.must_create("/api/activities", activity_body(f.unit1, "ACT-2", json!({}))).await;
    let (status, _) = f
        .admin
        .put(
            &format!("/api/activities/{other}/milestones/{mid}"),
            json!({"name": "x", "due_date": "2026-12-15", "status": "delayed"}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "他施策のマイルストーン");
    assert_eq!(
        f.member.do_req("DELETE", &mpath, Some(json!({"reason": "計画見直し"}))).await.0,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn delete_activity() {
    let Some(f) = ActivityFixture::new().await else { return };
    let sales =
        f.admin.must_create("/api/subjects", json!({"code": "4110", "name": "受託売上", "category": "revenue"})).await;

    // 子データ（マイルストーン・ドライバー・内訳）は施策と一緒に削除され、監査ログに残る
    let id = f.admin.must_create("/api/activities", activity_body(f.unit1, "ACT-1", json!({}))).await;
    let base = format!("/api/activities/{id}");
    f.admin.must_create(&format!("{base}/milestones"), json!({"name": "m", "due_date": "2026-12-01"})).await;
    f.admin
        .must_create(&format!("{base}/drivers"), json!({"code": "volume", "name": "件数", "driver_kind": "kpi"}))
        .await;
    f.admin
        .must_create(
            &format!("{base}/lines"),
            json!({"subject_id": sales, "name": "売上", "expression": "volume * 1000", "formula_enabled": true, "reason": "r"}),
        )
        .await;
    let (status, body) = f.admin.do_req("DELETE", &base, Some(json!({"reason": "施策の統合"}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "削除: {body}");
    let n: i64 = f
        .env
        .query_one(
            "SELECT COUNT(*) FROM audit_logs a JOIN change_sets c ON c.id = a.change_set_id WHERE a.action = 'delete' AND c.reason = '施策の統合'",
            (),
        )
        .await;
    assert_eq!(n, 4, "削除の監査ログ（施策・マイルストーン・ドライバー・計算式）");

    // 金額データがある施策は削除できない
    let id2 = f.admin.must_create("/api/activities", activity_body(f.unit1, "ACT-2", json!({}))).await;
    f.env
        .exec("INSERT INTO scenarios (name, fiscal_year, created_by) VALUES ('2026予算', 2026, ?)", (f.env.admin_id,))
        .await;
    let scenario_id: i64 = f.env.query_one("SELECT MAX(id) FROM scenarios", ()).await;
    f.env
        .exec(
            "INSERT INTO budget_facts (scenario_id, activity_id, subject_id, target_month, amount, source) VALUES (?, ?, ?, '2026-10-01', 1000, 'manual')",
            (scenario_id, id2, sales),
        )
        .await;
    let (status, _) = f.admin.do_req("DELETE", &format!("/api/activities/{id2}"), Some(json!({"reason": "r"}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "金額ありの削除");
}

#[tokio::test]
async fn list_activities() {
    let Some(f) = ActivityFixture::new().await else { return };
    f.admin.must_create("/api/activities", activity_body(f.unit1, "SAAS-1", json!({"name": "SaaS 月額課金"}))).await;
    f.admin
        .must_create(
            "/api/activities",
            activity_body(
                f.unit1,
                "PRJ-1",
                json!({"name": "受託開発 A社", "activity_type": "project", "start_date": "2026-04-01", "end_date": "2026-12-31"}),
            ),
        )
        .await;
    f.admin
        .must_create(
            "/api/activities",
            activity_body(f.unit2, "COST-1", json!({"name": "情シス運用", "activity_type": "cost_pool"})),
        )
        .await;

    for (path, want) in [
        ("/api/activities".to_string(), vec!["COST-1", "PRJ-1", "SAAS-1"]),
        ("/api/activities?activity_type=project".to_string(), vec!["PRJ-1"]),
        (format!("/api/activities?unit_id={}", f.unit2), vec!["COST-1"]),
        ("/api/activities?q=%E5%8F%97%E8%A8%97".to_string(), vec!["PRJ-1"]), // q=受託
        ("/api/activities?q=100%25".to_string(), vec![]),                    // % は文字として扱う
    ] {
        assert_eq!(codes_of(&f.viewer.must_get(&path).await), want, "GET {path}");
    }
    assert_eq!(f.viewer.get("/api/activities?unit_id=abc").await.0, StatusCode::BAD_REQUEST, "不正な unit_id");
}

#[tokio::test]
async fn priority_and_watch() {
    let Some(f) = ActivityFixture::new().await else { return };
    let id = f
        .manager1
        .must_create("/api/activities", activity_body(f.unit1, "ACT-1", json!({"owner_user_id": f.member_id})))
        .await;
    let base = format!("/api/activities/{id}");

    // 重点施策: 所管ユニットのマネージャー・FP&A が設定できる。担当者・他ユニットのマネージャーは不可
    for (name, c) in [("担当者", &f.member), ("他ユニットのマネージャー", &f.manager2), ("閲覧者", &f.viewer)]
    {
        assert_eq!(
            c.put(&format!("{base}/priority"), json!({"is_priority": true})).await.0,
            StatusCode::FORBIDDEN,
            "{name}"
        );
    }
    let (status, body) = f.manager1.put(&format!("{base}/priority"), json!({"is_priority": true})).await;
    assert!(status == StatusCode::OK && body["is_priority"] == json!(true), "重点施策の設定: {body}");
    let got = f.viewer.must_get(&base).await;
    assert!(got["is_priority"] == json!(true) && got["can_manage"] == json!(false), "{got}");
    assert_eq!(items(&f.viewer.must_get("/api/activities?priority=true").await).len(), 1);

    // ウォッチ: だれでも付けられ、本人にだけ見える
    let (status, body) = f.viewer.put(&format!("{base}/watch"), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"activity_id": id, "is_watched": true}));
    assert_eq!(f.viewer.must_get(&base).await["is_watched"], json!(true));
    assert_eq!(f.member.must_get(&base).await["is_watched"], json!(false));
    assert_eq!(items(&f.viewer.must_get("/api/activities?watched=true").await).len(), 1);
    f.viewer.put(&format!("{base}/watch"), Value::Null).await; // 2回付けても1件
    assert_eq!(f.viewer.delete(&format!("{base}/watch")).await.0, StatusCode::OK);
    assert_eq!(items(&f.viewer.must_get("/api/activities?watched=true").await).len(), 0);
    assert_eq!(f.viewer.put("/api/activities/99999/watch", Value::Null).await.0, StatusCode::NOT_FOUND);

    // 重点施策の変更は変更履歴に残る
    let n: i64 = f
        .env
        .query_one(
            "SELECT COUNT(*) FROM audit_logs WHERE table_name = 'activities' AND record_id = ? AND action = 'update'",
            (id,),
        )
        .await;
    assert_eq!(n, 1);
}

#[tokio::test]
async fn line_confidence_and_outlook() {
    let Some(f) = ActivityFixture::new().await else { return };
    let sales =
        f.admin.must_create("/api/subjects", json!({"code": "4110", "name": "売上", "category": "revenue"})).await;
    let id = f
        .admin
        .must_create("/api/activities", activity_body(f.unit1, "ACT-1", json!({"owner_user_id": f.member_id})))
        .await;
    let lbase = format!("/api/activities/{id}/lines");

    // 既定: 段階は施策の段階（null）、見通しの種類はベース
    let lid = f.member.must_create(&lbase, json!({"subject_id": sales, "name": "既存顧客"})).await;
    let detail_body = f.viewer.must_get(&format!("/api/activities/{id}")).await;
    let line = detail_body["lines"].as_array().unwrap().iter().find(|l| l["id"] == json!(lid)).unwrap().clone();
    assert!(line["confidence_level"].is_null() && line["outlook"] == "base", "既定 = {line}");

    for (name, body, field) in [
        ("未登録の段階", json!({"subject_id": sales, "name": "x", "confidence_level": "Z"}), "confidence_level"),
        ("見通しの種類が不正", json!({"subject_id": sales, "name": "x", "outlook": "upside"}), "outlook"),
    ] {
        let (status, res) = f.member.post(&lbase, body).await;
        assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&res, field).is_empty(), "{name}: {res}");
    }
    f.member
        .must_create(
            &lbase,
            json!({"subject_id": sales, "name": "解約リスク", "confidence_level": "D", "outlook": "downside"}),
        )
        .await;

    // 段階・見通しの種類の変更は理由が必須
    let lpath = format!("{lbase}/{lid}");
    let mut change = json!({"name": "既存顧客", "confidence_level": "B", "outlook": "addon"});
    let (status, body) = f.member.put(&lpath, change.clone()).await;
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "reason").is_empty(),
        "理由なしの変更: {body}"
    );
    change["reason"] = json!("追加発注の内示");
    let (status, body) = f.member.put(&lpath, change).await;
    assert!(status == StatusCode::OK && body["confidence_level"] == "B" && body["outlook"] == "addon", "変更: {body}");

    // 内訳から使われている段階は削除できない
    let level_id: i64 = f.env.query_one("SELECT id FROM confidence_levels WHERE code = 'D'", ()).await;
    assert_eq!(f.admin.delete(&format!("/api/confidence-levels/{level_id}")).await.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn restricted_subject_lines() {
    let Some(f) = ActivityFixture::new().await else { return };
    let secret = f
        .admin
        .must_create(
            "/api/subjects",
            json!({"code": "9000", "name": "役員報酬", "category": "expense", "is_restricted": true}),
        )
        .await;
    let id = f
        .admin
        .must_create("/api/activities", activity_body(f.unit1, "ACT-1", json!({"owner_user_id": f.member_id})))
        .await;
    let lbase = format!("/api/activities/{id}/lines");
    // 閲覧制限のある科目の内訳は FP&A だけが作れ、担当者には詳細でも見えない
    assert_eq!(f.member.post(&lbase, json!({"subject_id": secret, "name": "x"})).await.0, StatusCode::FORBIDDEN);
    f.admin.must_create(&lbase, json!({"subject_id": secret, "name": "x"})).await;
    let path = format!("/api/activities/{id}");
    assert_eq!(f.member.must_get(&path).await["lines"], json!([]));
    assert_eq!(f.viewer.must_get(&path).await["lines"].as_array().unwrap().len(), 1, "閲覧者（経営陣）は見られる");
}

#[tokio::test]
async fn activity_code_auto_numbering() {
    let Some(f) = ActivityFixture::new().await else { return };
    let create = |code: &'static str| {
        let body = activity_body(f.unit1, code, json!({}));
        let admin = &f.admin;
        async move {
            let (status, body) = admin.post("/api/activities", body).await;
            assert_eq!(status, StatusCode::CREATED, "作成: {body}");
            body["code"].as_str().unwrap().to_string()
        }
    };
    assert_eq!(create("").await, "ACT-0001");
    assert_eq!(create("").await, "ACT-0002");
    // 手入力のコードは自由。ACT-数字 の形なら続きから採番する
    create("ACT-0010").await;
    create("X-99").await;
    assert_eq!(create("").await, "ACT-0011");
    // 外部コードと同じ番号は飛ばす
    let first: i64 = f.env.query_one("SELECT MIN(id) FROM activities", ()).await;
    f.admin.must_create(&format!("/api/activities/{first}/external-codes"), json!({"code": "ACT-0012"})).await;
    assert_eq!(create("").await, "ACT-0013");
    // 自動採番しない場合（更新）は空欄を許さない
    let (status, body) = f.admin.put(&format!("/api/activities/{first}"), activity_body(f.unit1, "", json!({}))).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "code").is_empty(), "更新で空欄: {body}");
}

#[tokio::test]
async fn external_codes() {
    let Some(f) = ActivityFixture::new().await else { return };
    let manual = f
        .admin
        .must_create("/api/activities", activity_body(f.unit1, "PRJ-1", json!({"owner_user_id": f.member_id})))
        .await;
    let other = f.admin.must_create("/api/activities", activity_body(f.unit1, "SAAS-1", json!({}))).await;
    let base = format!("/api/activities/{manual}/external-codes");

    // 施策に複数の外部コードを付けられる。担当者も付けられる
    let p1 = f.member.must_create(&base, json!({"code": "P-1001", "note": "A社 受託"})).await;
    f.member.must_create(&base, json!({"code": "P-1002"})).await;
    let detail_body = f.viewer.must_get(&format!("/api/activities/{manual}")).await;
    assert_eq!(detail_body["external_codes"].as_array().unwrap().len(), 2);
    assert_eq!(detail_body["external_codes"][0]["note"], "A社 受託");

    for (name, path, code, want) in [
        (
            "他の施策に登録済み",
            format!("/api/activities/{other}/external-codes"),
            "P-1001",
            "施策「施策 PRJ-1」に登録されています",
        ),
        ("同じ施策に登録済み", base.clone(), "P-1002", "この施策に登録済みです"),
        ("施策コードと同じ", base.clone(), "SAAS-1", "施策コードと同じです"),
        ("空白を含む", base.clone(), "P 1003", "空白"),
        ("カンマを含む", base.clone(), "P,1003", "カンマ"),
    ] {
        let (status, body) = f.admin.post(&path, json!({ "code": code })).await;
        assert!(status == StatusCode::UNPROCESSABLE_ENTITY && detail(&body, "code").contains(want), "{name}: {body}");
    }
    // 担当外は付けられない
    assert_eq!(f.member2.post(&base, json!({"code": "P-2000"})).await.0, StatusCode::FORBIDDEN);

    // 施策コードを外部コードと同じにはできない（作成・更新とも）
    let (status, body) = f.admin.post("/api/activities", activity_body(f.unit1, "P-1002", json!({}))).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "code").is_empty(), "作成: {body}");
    let (status, body) = f
        .admin
        .put(
            &format!("/api/activities/{manual}"),
            activity_body(f.unit1, "P-1001", json!({"owner_user_id": f.member_id})),
        )
        .await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "code").is_empty(), "更新: {body}");

    // 一覧のキーワード検索は外部コードにも一致する
    assert_eq!(codes_of(&f.viewer.must_get("/api/activities?q=P-1002").await), vec!["PRJ-1"]);

    // 外部コードを外せる（理由は変更履歴に残る）。外した外部コードは別の施策に付け替えられる
    let (status, _) =
        f.member.do_req("DELETE", &format!("{base}/{p1}"), Some(json!({"reason": "案件番号の付け替え"}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(f.member.delete(&format!("{base}/{p1}")).await.0, StatusCode::NOT_FOUND);
    f.admin.must_create(&format!("/api/activities/{other}/external-codes"), json!({"code": "P-1001"})).await;
}

#[tokio::test]
async fn activity_import_export() {
    let Some(f) = ActivityFixture::new().await else { return };
    let a = &f.admin;
    let units = a.download("/api/units/export").await;
    let unit_code = units.split("\r\n").nth(1).unwrap().split(',').next().unwrap().to_string();
    let header = "code,name,unit_code,activity_type,status,start_date,end_date,owner_email,confidence_level,assumptions,external_codes\n";

    // code が空なら自動採番して追加。外部コードも登録
    let csv = format!(
        "{header},新規受託 3件想定,{unit_code},project,planned,2026-04-01,2027-03-31,member@example.com,C,下期に2件受注が前提,P-100 P-101\n\
         SAAS-X,SaaS X,{unit_code},recurring,in_progress,,,,,,\n"
    );
    let (status, body) = a.upload("/api/activities/import", &csv, "期初計画の登録").await;
    assert!(status == StatusCode::OK && result_counts(&body) == "inserted=2 updated=0 unchanged=0", "取込: {body}");
    let exported = a.download("/api/activities/export").await;
    assert!(
        exported.contains(&format!(
            "ACT-0001,新規受託 3件想定,{unit_code},project,planned,2026-04-01,2027-03-31,member@example.com,C,下期に2件受注が前提,P-100 P-101"
        )),
        "エクスポート = {exported:?}"
    );
    // 往復で変更なし
    let (status, body) = a.upload("/api/activities/import", &exported, "再取込").await;
    assert!(status == StatusCode::OK && result_counts(&body) == "inserted=0 updated=0 unchanged=2", "往復: {body}");
    // 確度の更新と外部コードの追加（記載のない外部コードは外さない）
    let csv = format!(
        "{header}ACT-0001,新規受託 3件想定,{unit_code},project,in_progress,2026-04-01,2027-03-31,member@example.com,B,下期に2件受注が前提,P-102\n"
    );
    let (status, body) = a.upload("/api/activities/import", &csv, "受注見込みの更新").await;
    assert!(status == StatusCode::OK && result_counts(&body) == "inserted=0 updated=1 unchanged=0", "更新: {body}");
    let list = a.must_get("/api/activities?q=ACT-0001").await;
    let id = items(&list)[0]["id"].as_i64().unwrap();
    let d = a.must_get(&format!("/api/activities/{id}")).await;
    assert!(d["confidence_level"] == "B" && d["external_codes"].as_array().unwrap().len() == 3, "更新後 = {d}");

    // エラー: 存在しないユニット・担当者、外部コードの衝突、タイプ不正
    let bad = format!(
        "{header}X-1,x,NOPE,recurring,planned,,,,,,\n\
         X-2,x,{unit_code},recurring,planned,,,nobody@example.com,,,\n\
         SAAS-X,SaaS X,{unit_code},recurring,in_progress,,,,,,P-100\n\
         X-3,x,{unit_code},flow,planned,,,,,,\n\
         X-4,x,{unit_code},recurring,planned,,,,,,SAAS-X\n\
         X-5,x,{unit_code},recurring,planned,,,,Z,,\n"
    );
    let (status, body) = a.upload("/api/activities/import", &bad, "r").await;
    let errs = row_errors(&body);
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && errs.len() == 6, "エラー: {errs:?}");
    assert!(errs.join("\n").contains("4:外部コード P-100 は別の施策に登録されています"), "{errs:?}");
    // FP&A 以外は取込できない（マネージャーでも）
    assert_eq!(f.manager1.upload("/api/activities/import", &csv, "r").await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn confidence_level_in_use() {
    let Some(f) = ActivityFixture::new().await else { return };
    let id = f
        .admin
        .must_create(
            "/api/confidence-levels",
            json!({"code": "F", "name": "撤退検討", "rate": "0.05", "sort_order": 6}),
        )
        .await;
    let path = format!("/api/confidence-levels/{id}");

    // 施策に段階を付ける。省略時は施策タイプの既定（運用型は A）
    let act = f.admin.must_create("/api/activities", activity_body(f.unit1, "ACT-1", json!({}))).await;
    assert_eq!(f.viewer.must_get(&format!("/api/activities/{act}")).await["confidence_level"], "A");
    f.admin
        .put(
            &format!("/api/activities/{act}"),
            activity_body(f.unit1, "ACT-1", json!({"confidence_level": "F", "reason": "撤退の検討"})),
        )
        .await;
    // 施策から参照されている段階は削除できない
    assert_eq!(f.admin.delete(&path).await.0, StatusCode::CONFLICT, "使用中の段階の削除");
    f.admin
        .put(
            &format!("/api/activities/{act}"),
            activity_body(f.unit1, "ACT-1", json!({"confidence_level": "A", "reason": "継続"})),
        )
        .await;
    assert_eq!(f.admin.delete(&path).await.0, StatusCode::NO_CONTENT, "段階の削除");
}

#[tokio::test]
async fn formula_lines_recalculate_amounts() {
    let Some(f) = ActivityFixture::new().await else { return };
    let sales =
        f.admin.must_create("/api/subjects", json!({"code": "4110", "name": "売上", "category": "revenue"})).await;
    let id = f.admin.must_create("/api/activities", activity_body(f.unit1, "ACT-1", json!({}))).await;
    let base = format!("/api/activities/{id}");
    let price = f
        .admin
        .must_create(&format!("{base}/drivers"), json!({"code": "price", "name": "単価", "driver_kind": "value"}))
        .await;
    let qty = f
        .admin
        .must_create(&format!("{base}/drivers"), json!({"code": "qty", "name": "件数", "driver_kind": "kpi"}))
        .await;
    // シナリオとドライバー値は、シナリオの API を移すまで DB に直接入れる
    f.env
        .exec(
            "INSERT INTO scenarios (name, fiscal_year, created_by, actual_through) VALUES ('2026予算', 2026, ?, '2026-04-01')",
            (f.env.admin_id,),
        )
        .await;
    let scenario: i64 = f.env.query_one("SELECT MAX(id) FROM scenarios", ()).await;
    for (driver, month, value, provisional) in [
        (price, "2026-04-01", "1000", false), // 実績の月は計算しない
        (qty, "2026-04-01", "1", false),
        (price, "2026-05-01", "1000.5", false),
        (qty, "2026-05-01", "3", true),
        (price, "2026-06-01", "1000", false), // 件数がない月は金額なし
    ] {
        f.env
            .exec(
                "INSERT INTO driver_values (scenario_id, activity_driver_id, target_month, value, is_provisional) VALUES (?, ?, ?, ?, ?)",
                (scenario, driver, month, value, provisional),
            )
            .await;
    }
    f.admin
        .must_create(
            &format!("{base}/lines"),
            json!({"subject_id": sales, "name": "売上", "expression": "price * qty", "formula_enabled": true, "reason": "式で算出"}),
        )
        .await;
    let facts: Vec<(String, String, String, bool, String)> = f
        .env
        .query(
            "SELECT DATE_FORMAT(target_month, '%Y-%m'), CAST(amount AS CHAR), source, is_provisional, COALESCE(provisional_reason, '')
             FROM budget_facts WHERE scenario_id = ? ORDER BY target_month",
            (scenario,),
        )
        .await;
    assert_eq!(
        facts,
        vec![(
            "2026-05".into(),
            "3002".into(), // 1000.5 × 3 = 3001.5 → 四捨五入
            "formula".into(),
            true,
            "仮の値のドライバーを含む: qty".into()
        )]
    );
    // 監査ログの金額の JSON は Go 版と同じ形
    let after: String = f
        .env
        .query_one(
            "SELECT CAST(after_json AS CHAR) FROM audit_logs WHERE table_name = 'budget_facts' AND action = 'insert'",
            (),
        )
        .await;
    let v: Value = serde_json::from_str(&after).unwrap();
    assert_eq!(v["target_month"], "2026-05");
    assert_eq!(v["amount"], "3002");
    assert!(v["line_id"].is_i64());
}
