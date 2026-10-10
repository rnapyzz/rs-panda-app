//! 変更履歴の統合テスト。Go 版の history_test.go と、visibility_test.go の変更履歴の部分。

mod common;

use common::*;
use reqwest::StatusCode;
use serde_json::{Value, json};

fn reasons(body: &Value) -> Vec<String> {
    items(body).iter().map(|it| it["reason"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn change_history() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let activity_path = format!("/api/activities/{}", f.manual_act);

    // 施策の確度を変更（理由あり）
    let update = activity_body(
        f.unit1,
        "PRJ-1",
        json!({"owner_user_id": f.member_id, "confidence_level": "B", "reason": "受注確度の見直し"}),
    );
    let (status, body) = f.member.put(&activity_path, update).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // 金額の入力（理由あり）
    let (status, body) = f
        .member
        .put(
            &format!("{}/amounts", f.values_path(f.budget, f.manual_act)),
            json!({"reason": "予算の初回入力", "amounts": [{"subject_id": f.sales, "target_month": "2026-10", "amount": 1000}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // 別の施策のドライバー値（理由あり）
    let (status, body) = f
        .member
        .put(
            &format!("{}/driver-values", f.values_path(f.budget, f.formula_act)),
            json!({"reason": "単価の入力", "values": [{"driver_id": f.price_id, "target_month": "2026-10", "value": 500}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // マスタの変更（理由なし）
    f.admin.must_create("/api/subjects", json!({"code": "9999", "name": "雑費", "category": "expense"})).await;

    // 一覧は新しい順。閲覧者も参照できる
    let body = f.viewer.must_get("/api/change-sets?limit=4").await;
    assert_eq!(reasons(&body), ["", "単価の入力", "予算の初回入力", "受注確度の見直し"]);
    assert_eq!(body["has_more"], json!(true), "フィクスチャの作成分がまだある");
    let latest = &items(&body)[2];
    assert_eq!(latest["activities"], json!([{"id": f.manual_act, "code": "PRJ-1", "name": "施策 PRJ-1"}]), "{latest}");
    assert_eq!((&latest["tables"]["budget_facts"], &latest["more_activities"]), (&json!(1), &json!(0)));
    assert_eq!((&latest["user"]["id"], &latest["scenario"]["id"]), (&json!(f.member_id), &json!(f.budget)));
    // ドライバー値の変更は、ドライバー経由で施策に結びつく
    assert_eq!(items(&body)[1]["activities"][0]["code"], json!("SAAS-1"));

    // 続きの取得
    let last_id = items(&body)[3]["id"].as_i64().unwrap();
    let more = f.viewer.must_get(&format!("/api/change-sets?limit=4&before_id={last_id}")).await;
    assert!(items(&more).iter().all(|it| it["id"].as_i64().unwrap() < last_id), "{more}");

    // 施策で絞り込むと、施策自体・金額の変更が含まれ、別施策やマスタの変更は含まれない
    let body = f.viewer.must_get(&format!("/api/change-sets?activity_id={}", f.manual_act)).await;
    assert_eq!(reasons(&body), ["予算の初回入力", "受注確度の見直し", ""], "最後の空は作成時");
    let body = f.viewer.must_get(&format!("/api/change-sets?activity_id={}&reason=with", f.formula_act)).await;
    assert_eq!(reasons(&body), ["単価の入力", "算出式の設定"]);

    // シナリオ・理由の有無・ユーザーで絞り込み
    let body = f.viewer.must_get(&format!("/api/change-sets?scenario_id={}", f.budget)).await;
    assert_eq!(reasons(&body), ["単価の入力", "予算の初回入力", ""], "最後の空は作成中の指定");
    let body = f.viewer.must_get("/api/change-sets?reason=without&limit=1").await;
    assert_eq!(reasons(&body), [""]);
    let body = f.viewer.must_get(&format!("/api/change-sets?user_id={}", f.member_id)).await;
    assert_eq!(reasons(&body), ["単価の入力", "予算の初回入力", "受注確度の見直し"]);

    // 詳細: 対象が人の読める名前になる。tables・activities は一覧だけ
    let id = latest["id"].as_i64().unwrap();
    let detail = f.viewer.must_get(&format!("/api/change-sets/{id}")).await;
    let logs = detail["logs"].as_array().unwrap();
    assert_eq!(logs.len(), 1, "{detail}");
    assert_eq!((&logs[0]["label"], &logs[0]["action"]), (&json!("施策 PRJ-1 / 受託売上 / 2026-10"), &json!("insert")));
    assert_eq!((&logs[0]["after"]["amount"], &logs[0]["before"]), (&json!("1000"), &Value::Null));
    assert_eq!((&detail["change_set"]["changes"], &detail["change_set"]["tables"]), (&json!(1), &Value::Null));
    // ドライバー値のラベル
    let id = items(&f.viewer.must_get("/api/change-sets?limit=4").await)[1]["id"].as_i64().unwrap();
    let detail = f.viewer.must_get(&format!("/api/change-sets/{id}")).await;
    let labels: Vec<&str> = detail["logs"].as_array().unwrap().iter().map(|l| l["label"].as_str().unwrap()).collect();
    assert!(labels.contains(&"施策 SAAS-1 / 単価 / 2026-10"), "{labels:?}");

    // 期間の絞り込み（未来の日付からは何も出ない。今日を含む）
    assert!(items(&f.viewer.must_get("/api/change-sets?from=2999-01-01").await).is_empty());
    let today = fpanda_api::jst::today();
    assert!(!items(&f.viewer.must_get(&format!("/api/change-sets?from={today}&to={today}")).await).is_empty());
    let (status, _) = f.viewer.get("/api/change-sets/99999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for q in ["reason=maybe", "from=2026/01/01", "activity_id=x", "limit=0", "user_id=x"] {
        let (status, _) = f.viewer.get(&format!("/api/change-sets?{q}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{q}");
    }
}

/// 変更履歴の閲覧の範囲（docs/plan.md「2.17」）: 現場は編集できる施策だけ。閲覧制限のある科目の変更は出さない。
#[tokio::test]
async fn change_history_scope() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let a = &f.admin;
    let personnel = a
        .must_create(
            "/api/subjects",
            json!({"code": "8210", "name": "人件費", "category": "expense", "is_restricted": true}),
        )
        .await;
    let amounts = format!("{}/amounts", f.values_path(f.budget, f.manual_act));
    a.put(&amounts, json!({"reason": "人件費の配賦", "amounts": [{"subject_id": personnel, "target_month": "2026-10", "amount": 500}]}))
        .await;
    a.put(
        &amounts,
        json!({"reason": "売上", "amounts": [{"subject_id": f.sales, "target_month": "2026-10", "amount": 100}]}),
    )
    .await;
    let m = &f.member;

    let (status, _) = m.get("/api/change-sets").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "現場の変更履歴（全体）");
    let (status, _) = f.manager2.get(&format!("/api/change-sets?activity_id={}", f.manual_act)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "編集できない施策の変更履歴");
    let (status, _) = m.get("/api/change-sets?activity_id=999999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let path = format!("/api/change-sets?activity_id={}", f.manual_act);
    let all = reasons(&a.must_get(&path).await);
    assert!(all.contains(&"人件費の配賦".to_string()), "{all:?}");
    let mine = reasons(&m.must_get(&path).await);
    assert!(!mine.contains(&"人件費の配賦".to_string()) && mine.contains(&"売上".to_string()), "{mine:?}");

    let body = a.must_get(&path).await;
    let id_of = |reason: &str| items(&body).iter().find(|it| it["reason"] == reason).unwrap()["id"].as_i64().unwrap();
    let detail = format!("/api/change-sets/{}", id_of("人件費の配賦"));
    let (status, _) = m.get(&format!("{detail}?activity_id={}", f.manual_act)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "現場の人件費の変更の詳細");
    let (status, _) = f.viewer.get(&detail).await;
    assert_eq!(status, StatusCode::OK, "経営陣の変更履歴の詳細");
    let (status, _) = m.get(&format!("/api/change-sets/{}?activity_id={}", id_of("売上"), f.manual_act)).await;
    assert_eq!(status, StatusCode::OK, "現場の売上の変更の詳細");
}
