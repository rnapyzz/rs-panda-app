//! 組織変更と異動の統合テスト。Go 版の orgchange_test.go。

mod common;

use chrono::{Duration, NaiveDate};
use common::*;
use fpanda_api::{jst, orgchange};
use reqwest::StatusCode;
use serde_json::{Value, json};

async fn unit_of(f: &ActivityFixture, activity_id: i64) -> i64 {
    f.env.query_one("SELECT unit_id FROM activities WHERE id = ?", (activity_id,)).await
}

fn tomorrow() -> NaiveDate {
    jst::today() + Duration::days(1)
}

/// docs/plan.md「2.15」の組織変更の予約。
#[tokio::test]
async fn org_change_plan() {
    let Some(f) = ActivityFixture::new().await else { return };
    let a = &f.admin;
    let act1 =
        a.must_create("/api/activities", activity_body(f.unit1, "ACT-A", json!({"owner_user_id": f.member_id}))).await;
    let act2 = a.must_create("/api/activities", activity_body(f.unit1, "ACT-B", json!({}))).await;
    let seg2 = a.must_create("/api/segments", json!({"name": "新事業"})).await;
    let org: i64 = f.env.query_one("SELECT organization_id FROM units WHERE id = ?", (f.unit2,)).await;
    let items = json!([
        {"kind": "move_activity", "activity_id": act1, "target_unit_id": f.unit2},
        {"kind": "move_unit", "unit_id": f.unit2, "segment_id": seg2, "organization_id": org},
        {"kind": "change_owner", "activity_id": act2, "owner_user_id": f.member_id},
    ]);
    // 有効日は明日以降。今の状態で適用できない予約は作れない（何も変えない）
    let (status, body) = a
        .post(
            "/api/org-change-plans",
            json!({"name": "10月の組織変更", "effective_date": jst::today().to_string(), "items": items}),
        )
        .await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "effective_date").is_empty(), "{body}");
    let (status, body) = a
        .post(
            "/api/org-change-plans",
            json!({"name": "x", "effective_date": tomorrow().to_string(), "items": [
                {"kind": "change_owner", "activity_id": act2, "owner_user_id": f.member_id},
                {"kind": "move_activity", "activity_id": act1, "target_unit_id": 99999},
            ]}),
        )
        .await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && detail(&body, "items").contains("2 件目"), "{body}");
    let owner: Option<i64> = f.env.query_one("SELECT owner_user_id FROM activities WHERE id = ?", (act2,)).await;
    assert_eq!(owner, None, "確かめただけで担当者が変わった");
    let (status, body) = a
        .post(
            "/api/org-change-plans",
            json!({"name": "x", "effective_date": "2099-01-01", "items": [{"kind": "rename"}]}),
        )
        .await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && detail(&body, "items").contains("1 件目"), "{body}");
    let (status, _) = f.manager1.get("/api/org-change-plans").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let plan = a
        .must_create(
            "/api/org-change-plans",
            json!({"name": "10月の組織変更", "effective_date": tomorrow().to_string(), "items": items}),
        )
        .await;
    // 作っただけでは変わらない
    assert_eq!(unit_of(&f, act1).await, f.unit1, "予約しただけで施策が移った");
    let got = a.must_get(&format!("/api/org-change-plans/{plan}")).await;
    assert_eq!(
        (&got["status"], &got["item_count"], got["items"].as_array().map(Vec::len)),
        (&json!("scheduled"), &json!(3), Some(3))
    );
    let list = a.must_get("/api/org-change-plans").await;
    assert!(items_of(&list)[0].get("items").is_none(), "一覧には変更を含めない: {list}");

    // 有効日が来ると自動で適用する（今日の分はまだ）
    orgchange::apply_due(&f.env.db, jst::today()).await.unwrap();
    assert_eq!(unit_of(&f, act1).await, f.unit1);
    orgchange::apply_due(&f.env.db, tomorrow()).await.unwrap();
    let got = a.must_get(&format!("/api/org-change-plans/{plan}")).await;
    assert_eq!(got["status"], json!("applied"), "{got}");
    assert!(got["applied_at"].is_string());
    assert_eq!(unit_of(&f, act1).await, f.unit2);
    let seg: i64 = f.env.query_one("SELECT segment_id FROM units WHERE id = ?", (f.unit2,)).await;
    assert_eq!(seg, seg2, "ユニットの所属");
    // 自動の適用は予約を作った人の変更セットとして残る
    let n: i64 = f
        .env
        .query_one(
            "SELECT COUNT(*) FROM change_sets WHERE reason = '組織変更の予約「10月の組織変更」を適用' AND user_id = ?",
            (f.env.admin_id,),
        )
        .await;
    assert_eq!(n, 1);
    // 適用済みは変更・削除・取り消しできない
    let (status, _) = a.do_req("DELETE", &format!("/api/org-change-plans/{plan}"), None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = a
        .put(
            &format!("/api/org-change-plans/{plan}"),
            json!({"name": "x", "effective_date": "2099-01-01", "items": items}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = a.post(&format!("/api/org-change-plans/{plan}/cancel"), Value::Null).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

fn items_of(body: &Value) -> &Vec<Value> {
    body["items"].as_array().unwrap()
}

#[tokio::test]
async fn org_change_plan_failure() {
    let Some(f) = ActivityFixture::new().await else { return };
    let a = &f.admin;
    let act = a.must_create("/api/activities", activity_body(f.unit1, "ACT-A", json!({}))).await;
    let plan = a
        .must_create(
            "/api/org-change-plans",
            json!({"name": "統合の予約", "effective_date": tomorrow().to_string(), "items": [
                {"kind": "move_activity", "activity_id": act, "target_unit_id": f.unit2},
            ]}),
        )
        .await;
    // 予約の後に移動先が廃止された（unit2 の施策はないので廃止できる）
    let (status, body) = a.post(&format!("/api/units/{}/archive", f.unit2), Value::Null).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = a.post(&format!("/api/org-change-plans/{plan}/apply"), Value::Null).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && detail(&body, "items").contains("廃止"), "{body}");
    let got = a.must_get(&format!("/api/org-change-plans/{plan}")).await;
    assert_eq!(got["status"], json!("failed"), "{got}");
    assert!(got["error"].as_str().unwrap().contains("廃止"));
    assert_eq!(unit_of(&f, act).await, f.unit1, "失敗したら何も変えない");
    // FP&A にお知らせが届く
    let list = a.must_get("/api/notifications").await;
    let notes = items_of(&list);
    assert!(notes.len() == 1 && notes[0]["title"].as_str().unwrap().contains("統合の予約"), "{list}");
    assert_eq!(notes[0]["link"], json!("/admin/org-changes"));
    // 直して（廃止を取り消して）今すぐ適用する。適用は押した人の変更セットとして残る
    a.post(&format!("/api/units/{}/unarchive", f.unit2), Value::Null).await;
    let (status, body) = a.post(&format!("/api/org-change-plans/{plan}/apply"), Value::Null).await;
    assert_eq!((status, body["status"].clone()), (StatusCode::OK, json!("applied")), "{body}");
    assert_eq!(unit_of(&f, act).await, f.unit2);
    let (status, _) = a.post(&format!("/api/org-change-plans/{plan}/apply"), Value::Null).await;
    assert_eq!(status, StatusCode::CONFLICT, "二重の適用");
    let (status, _) = a.post("/api/org-change-plans/999999/apply", Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // 予約中の予約は変更・取り消し・削除できる
    let plan2 = a
        .must_create(
            "/api/org-change-plans",
            json!({"name": "戻す", "effective_date": tomorrow().to_string(), "items": [
                {"kind": "move_activity", "activity_id": act, "target_unit_id": f.unit1},
            ]}),
        )
        .await;
    let (status, body) = a
        .put(
            &format!("/api/org-change-plans/{plan2}"),
            json!({"name": "戻す（改）", "effective_date": "2099-04-01", "items": [
                {"kind": "change_owner", "unit_id": f.unit1, "owner_user_id": null},
            ], "reason": "見直し"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        (&body["name"], &body["effective_date"], &body["items"][0]["kind"]),
        (&json!("戻す（改）"), &json!("2099-04-01"), &json!("change_owner"))
    );
    let (status, body) = a.post(&format!("/api/org-change-plans/{plan2}/cancel"), json!({"reason": "中止"})).await;
    assert_eq!((status, body["status"].clone()), (StatusCode::OK, json!("cancelled")), "{body}");
    let (status, _) =
        a.do_req("DELETE", &format!("/api/org-change-plans/{plan2}"), Some(json!({"reason": "整理"}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn merge_unit_and_deactivate() {
    let Some(f) = ActivityFixture::new().await else { return };
    let a = &f.admin;
    let act =
        a.must_create("/api/activities", activity_body(f.unit1, "ACT-A", json!({"owner_user_id": f.member_id}))).await;

    // 施策が所属しているユニットは廃止できない。統合なら施策を移して廃止にする
    let (status, _) = a.post(&format!("/api/units/{}/archive", f.unit1), Value::Null).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = a.post("/api/units/999999/archive", Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let merge = format!("/api/units/{}/merge", f.unit1);
    let (status, body) = a.post(&merge, json!({"target_unit_id": f.unit2})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "reason").is_empty(), "{body}");
    let (status, _) = a.post(&merge, json!({"target_unit_id": f.unit1, "reason": "x"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "自分自身への統合");
    let (status, body) = a.post(&merge, json!({"target_unit_id": f.unit2, "reason": "課の統合"})).await;
    assert_eq!((status, body["moved"].clone()), (StatusCode::OK, json!(1)), "{body}");
    assert_eq!(unit_of(&f, act).await, f.unit2);
    // 廃止したユニットは一覧に出さず、施策を所属させられない
    assert_eq!(items_of(&a.must_get("/api/units").await).len(), 1);
    assert_eq!(items_of(&a.must_get("/api/units?include_archived=true").await).len(), 2);
    let (status, body) = a.post("/api/activities", activity_body(f.unit1, "ACT-X", json!({}))).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && detail(&body, "unit_id").contains("廃止"), "{body}");

    // 無効化のときの後任者: 担当の施策と所管ユニットをまとめて付け替えてから無効にする
    let counts = a.must_get(&format!("/api/users/{}/assignments", f.member_id)).await;
    assert_eq!(counts, json!({"activities": 1, "units": 0}));
    let (status, _) = a.get("/api/users/999999/assignments").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let deactivate = format!("/api/users/{}/deactivate", f.member_id);
    let (status, body) = a.post(&deactivate, json!({"successor_user_id": f.member_id})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "successor_user_id").is_empty(), "{body}");
    let (status, body) = a.post(&format!("/api/users/{}/deactivate", f.env.admin_id), json!({})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "is_active").is_empty(), "{body}");
    let (status, body) = a.post(&deactivate, json!({"successor_user_id": f.manager1_id, "reason": "異動"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["moved"], json!({"activities": 1, "units": 0}));
    let owner: i64 = f.env.query_one("SELECT owner_user_id FROM activities WHERE id = ?", (act,)).await;
    let active: bool = f.env.query_one("SELECT is_active FROM users WHERE id = ?", (f.member_id,)).await;
    assert_eq!((owner, active), (f.manager1_id, false));
    let (status, _) = f.member.get("/api/auth/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "無効にしたユーザーのセッション");
    // 無効なユーザーは後任者にできない
    let (status, body) =
        a.post(&format!("/api/users/{}/deactivate", f.manager1_id), json!({"successor_user_id": f.member_id})).await;
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY && detail(&body, "successor_user_id").contains("無効"),
        "{body}"
    );

    // CSV で無効にしたユーザーに担当が残っていれば知らせる
    let (status, body) = a
        .upload(
            "/api/users/import",
            "email,name,role,is_active\nmanager1@example.com,マネージャー1,manager,false\n",
            "異動",
        )
        .await;
    let warnings = body["warnings"].as_array().cloned().unwrap_or_default();
    assert!(
        status == StatusCode::OK && warnings.len() == 1 && warnings[0].as_str().unwrap().contains("担当の施策 1 件"),
        "{body}"
    );
}
