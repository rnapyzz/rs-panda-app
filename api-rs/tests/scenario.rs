//! シナリオ API の統合テスト。Go 版の scenario_test.go・cycle_test.go・comments_test.go・plan_values_test.go・target_test.go。
//! お知らせ・レポートの API を使う確認は、それらを移す段階で足す（お知らせは notifications の行で確かめる）。

mod common;

use chrono::{Duration, Local};
use common::*;
use reqwest::StatusCode;
use serde_json::{Value, json};

#[tokio::test]
async fn scenario_management() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let (status, _) = f.manager1.post("/api/scenarios", json!({"name": "x", "fiscal_year": 2026})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "マネージャーのシナリオ作成");
    for (name, body, field) in [
        ("名称重複", json!({"name": "2026年度 当初予算", "fiscal_year": 2026}), "name"),
        ("エイリアス不正", json!({"name": "x", "fiscal_year": 2026, "plan_role": "budget"}), "plan_role"),
        (
            "決算確定月が年度外",
            json!({"name": "x", "fiscal_year": 2026, "actual_through": "2027-04"}),
            "actual_through",
        ),
        ("年度不正", json!({"name": "x", "fiscal_year": 26}), "fiscal_year"),
        (
            "年度違いの複製元",
            json!({"name": "x", "fiscal_year": 2027, "base_scenario_id": f.budget}),
            "base_scenario_id",
        ),
    ] {
        let (status, res) = f.admin.post("/api/scenarios", body).await;
        assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&res, field).is_empty(), "{name}: {res}");
    }

    let path = format!("/api/scenarios/{}", f.budget);
    let (status, body) = f.admin.put(&path, json!({"name": "2026年度 予算"})).await;
    assert!(status == StatusCode::OK && body["name"] == "2026年度 予算", "名称変更: {body}");

    // ロック / ロック解除
    let (status, body) = f.admin.post(&format!("{path}/lock"), Value::Null).await;
    assert!(status == StatusCode::OK && body["is_locked"] == json!(true), "ロック: {body}");
    assert_eq!(f.admin.post(&format!("{path}/lock"), Value::Null).await.0, StatusCode::CONFLICT, "二重ロック");
    let (status, body) = f.admin.post(&format!("{path}/unlock"), Value::Null).await;
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "reason").is_empty(),
        "理由なしのロック解除: {body}"
    );
    assert_eq!(f.admin.post(&format!("{path}/unlock"), json!({"reason": "修正漏れの反映"})).await.0, StatusCode::OK);
    assert_eq!(f.manager1.post(&format!("{path}/lock"), Value::Null).await.0, StatusCode::FORBIDDEN);

    f.admin.must_create("/api/scenarios", json!({"name": "2027年度 予算", "fiscal_year": 2027})).await;
    let list = f.viewer.must_get("/api/scenarios?fiscal_year=2027").await;
    assert!(items(&list).len() == 1 && items(&list)[0]["name"] == "2027年度 予算", "年度での絞り込み = {list}");
    assert_eq!(f.viewer.get("/api/scenarios?fiscal_year=x").await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn driver_values_recalculate_amounts() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let path = format!("{}/driver-values", f.values_path(f.budget, f.formula_act));

    let (status, body) = f
        .member
        .put(
            &path,
            json!({"reason": "初回入力", "values": [
                {"driver_id": f.price_id, "target_month": "2026-10", "value": 1000},
                {"driver_id": f.volume_id, "target_month": "2026-10", "value": 3},
                {"driver_id": f.price_id, "target_month": "2026-11", "value": "1000.5"},
                {"driver_id": f.volume_id, "target_month": "2026-11", "value": 3, "is_provisional": true, "provisional_reason": "商談中の件数"},
                {"driver_id": f.price_id, "target_month": "2026-12", "value": 1000},
            ]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "ドライバー値の登録: {body}");
    // 1000 * 3 * 0.5 = 1500、1000.5 * 3 * 0.5 = 1500.75 → 1501（四捨五入）
    assert_eq!(line_amount_of(&body, f.sales_line, "2026-10").0, "1500");
    let (got, cell) = line_amount_of(&body, f.sales_line, "2026-11");
    assert!(got == "1501" && cell["source"] == "formula" && cell["is_provisional"] == json!(true), "2026-11 = {cell}");
    assert_eq!(line_amount_of(&body, f.sales_line, "2026-12").0, "", "件数が未入力");
    assert_eq!(body["editable"], json!(true));
    // ドライバー値は書かれたまま（末尾の 0 なし）で返る
    let price = body["drivers"].as_array().unwrap().iter().find(|d| d["code"] == "unit_price").unwrap();
    assert_eq!(serde_json::to_string(&price["values"][1]["value"]).unwrap(), "1000.5");

    // 値の削除で金額も消え、値の変更で金額も変わる
    let (status, body) = f
        .member
        .put(
            &path,
            json!({"reason": "見直し", "values": [
                {"driver_id": f.volume_id, "target_month": "2026-10", "value": null},
                {"driver_id": f.volume_id, "target_month": "2026-11", "value": 4},
            ]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "更新: {body}");
    assert_eq!(line_amount_of(&body, f.sales_line, "2026-10").0, "");
    let (got, cell) = line_amount_of(&body, f.sales_line, "2026-11");
    assert!(got == "2001" && cell["is_provisional"] == json!(false), "2026-11 = {cell}");

    // 金額は満額で持つので、確度の段階を変えても金額は変わらない（加重は集計時に行う）
    let update = activity_body(
        f.unit1,
        "SAAS-1",
        json!({"owner_user_id": f.member_id, "confidence_level": "B", "reason": "内示を受けた"}),
    );
    let (status, body) = f.member.put(&format!("/api/activities/{}", f.formula_act), update).await;
    assert_eq!(status, StatusCode::OK, "確度の変更: {body}");
    let body = f.member.must_get(&f.values_path(f.budget, f.formula_act)).await;
    assert_eq!(line_amount_of(&body, f.sales_line, "2026-11").0, "2001");

    // 変更は変更セット（シナリオ付き）と監査ログに残る
    let n: i64 = f
        .env
        .query_one(
            "SELECT COUNT(*) FROM audit_logs a JOIN change_sets c ON c.id = a.change_set_id
             WHERE a.table_name = 'budget_facts' AND c.scenario_id = ? AND c.reason = '見直し'",
            (f.budget,),
        )
        .await;
    assert_eq!(n, 2, "「見直し」での金額の監査ログ（削除1・更新1）");
}

#[tokio::test]
async fn driver_value_validation() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let path = format!("{}/driver-values", f.values_path(f.budget, f.formula_act));
    let other_act = f.admin.must_create("/api/activities", activity_body(f.unit1, "OTHER", json!({}))).await;
    let other_driver = f
        .admin
        .must_create(
            &format!("/api/activities/{other_act}/drivers"),
            json!({"code": "x", "name": "x", "driver_kind": "kpi"}),
        )
        .await;

    for (name, value, field) in [
        (
            "年度外の月",
            json!({"driver_id": f.price_id, "target_month": "2027-04", "value": 1}),
            "values[0].target_month",
        ),
        (
            "月の形式",
            json!({"driver_id": f.price_id, "target_month": "2026-10-01", "value": 1}),
            "values[0].target_month",
        ),
        (
            "他施策のドライバー",
            json!({"driver_id": other_driver, "target_month": "2026-10", "value": 1}),
            "values[0].driver_id",
        ),
        (
            "小数7桁",
            json!({"driver_id": f.price_id, "target_month": "2026-10", "value": "0.1234567"}),
            "values[0].value",
        ),
        (
            "仮の値に理由なし",
            json!({"driver_id": f.price_id, "target_month": "2026-10", "value": 1, "is_provisional": true}),
            "values[0].provisional_reason",
        ),
    ] {
        let (status, body) = f.member.put(&path, json!({"reason": "r", "values": [value]})).await;
        assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, field).is_empty(), "{name}: {body}");
    }
    // 数値として読めない文字列は JSON の形式の誤り（Go の json.Number と同じ）
    let bad = json!({"reason": "r", "values": [{"driver_id": f.price_id, "target_month": "2026-10", "value": "1/0"}]});
    assert_eq!(f.member.put(&path, bad).await.0, StatusCode::BAD_REQUEST);
    let dup = json!([{"driver_id": f.price_id, "target_month": "2026-10", "value": 1}, {"driver_id": f.price_id, "target_month": "2026-10", "value": 2}]);
    let (status, body) = f.member.put(&path, json!({"reason": "r", "values": dup})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "values[1]").is_empty(), "重複: {body}");
    let (status, body) = f.member.put(&path, json!({"reason": "r", "values": []})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "values").is_empty(), "空: {body}");
}

#[tokio::test]
async fn division_by_zero_rolls_back() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let base = format!("/api/activities/{}", f.formula_act);
    f.admin
        .must_create(
            &format!("{base}/lines"),
            json!({"subject_id": f.cost, "name": "外注費", "expression": "unit_price / volume", "formula_enabled": true, "reason": "r"}),
        )
        .await;
    let (status, body) = f
        .member
        .put(
            &format!("{}/driver-values", f.values_path(f.budget, f.formula_act)),
            json!({"reason": "r", "values": [
                {"driver_id": f.price_id, "target_month": "2026-10", "value": 1000},
                {"driver_id": f.volume_id, "target_month": "2026-10", "value": 0},
            ]}),
        )
        .await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "values").is_empty(), "0除算: {body}");
    // ロールバックされ、ドライバー値も保存されていない
    let body = f.member.must_get(&f.values_path(f.budget, f.formula_act)).await;
    for d in body["drivers"].as_array().unwrap() {
        assert_eq!(d["values"], json!([]), "0除算の後にドライバー値が残っている");
    }
}

#[tokio::test]
async fn formula_change_recalculates_only_unlocked_scenarios() {
    let Some(f) = ScenarioFixture::new().await else { return };
    f.member
        .put(
            &format!("{}/driver-values", f.values_path(f.budget, f.formula_act)),
            json!({"reason": "r", "values": [
                {"driver_id": f.price_id, "target_month": "2026-10", "value": 1000},
                {"driver_id": f.volume_id, "target_month": "2026-10", "value": 2},
            ]}),
        )
        .await;
    // 予算を複製して見込を作り、予算はロックする
    let forecast = f
        .admin
        .must_create(
            "/api/scenarios",
            json!({"name": "2026-10時点見込", "fiscal_year": 2026, "base_scenario_id": f.budget}),
        )
        .await;
    assert_eq!(f.admin.post(&format!("/api/scenarios/{}/lock", f.budget), Value::Null).await.0, StatusCode::OK);

    // 計算式を変更（確度を掛けない）
    let (status, body) = f
        .member
        .put(
            &format!("/api/activities/{}/lines/{}", f.formula_act, f.sales_line),
            json!({"name": "利用料", "expression": "unit_price * volume", "formula_enabled": true, "reason": "確度は別管理にする"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "計算式の変更: {body}");
    let budget = f.viewer.must_get(&f.values_path(f.budget, f.formula_act)).await;
    assert_eq!(line_amount_of(&budget, f.sales_line, "2026-10").0, "1000", "ロック済み予算は変わらない");
    let fc = f.viewer.must_get(&f.values_path(forecast, f.formula_act)).await;
    assert_eq!(line_amount_of(&fc, f.sales_line, "2026-10").0, "2000", "見込は再計算される");
}

#[tokio::test]
async fn scenario_copy() {
    let Some(f) = ScenarioFixture::new().await else { return };
    f.member
        .put(
            &format!("{}/driver-values", f.values_path(f.budget, f.formula_act)),
            json!({"reason": "r", "values": [{"driver_id": f.price_id, "target_month": "2026-10", "value": 1000}]}),
        )
        .await;
    f.member
        .put(
            &format!("{}/amounts", f.values_path(f.budget, f.manual_act)),
            json!({"reason": "r", "amounts": [{"subject_id": f.cost, "target_month": "2026-10", "amount": -500000}]}),
        )
        .await;

    let copy_id = f
        .admin
        .must_create("/api/scenarios", json!({"name": "複製", "fiscal_year": 2026, "base_scenario_id": f.budget}))
        .await;
    let body = f.viewer.must_get(&f.values_path(copy_id, f.manual_act)).await;
    assert_eq!(amount_of(&body, f.cost, "2026-10").0, "-500000");
    let drivers = f.viewer.must_get(&f.values_path(copy_id, f.formula_act)).await;
    let v = &drivers["drivers"][0]["values"];
    assert!(v.as_array().unwrap().len() == 1 && num_string(&v[0]["value"]) == "1000", "複製されたドライバー値 = {v}");
    // 複製した件数は監査ログに残る
    let after: String = f
        .env
        .query_one("SELECT CAST(after_json AS CHAR) FROM audit_logs WHERE table_name = 'scenarios' AND record_id = ? AND action = 'insert'", (copy_id,))
        .await;
    let after: Value = serde_json::from_str(&after).unwrap();
    assert_eq!(after["copied"], json!({"budget_facts": 1, "driver_values": 1}));
    assert_eq!(after["name"], "複製");
}

#[tokio::test]
async fn manual_amounts() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let path = format!("{}/amounts", f.values_path(f.budget, f.manual_act));

    let (status, body) = f
        .member
        .put(
            &path,
            json!({"reason": "初回入力", "amounts": [
                {"subject_id": f.sales, "target_month": "2026-04", "amount": 12000000},
                {"subject_id": f.cost, "target_month": "2026-04", "amount": -8000000, "is_provisional": true, "provisional_reason": "見積待ち"},
            ]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "金額の登録: {body}");
    let (got, cell) = amount_of(&body, f.cost, "2026-04");
    assert!(
        got == "-8000000" && cell["source"] == "manual" && cell["provisional_reason"] == "見積待ち",
        "外注費 = {cell}"
    );

    for (name, amount, field) in [
        ("小数", json!({"subject_id": f.sales, "target_month": "2026-05", "amount": 100.5}), "amounts[0].amount"),
        (
            "桁あふれ",
            json!({"subject_id": f.sales, "target_month": "2026-05", "amount": "1000000000000000000"}),
            "amounts[0].amount",
        ),
        (
            "存在しない科目",
            json!({"subject_id": 99999, "target_month": "2026-05", "amount": 1}),
            "amounts[0].subject_id",
        ),
    ] {
        let (status, body) = f.member.put(&path, json!({"reason": "r", "amounts": [amount]})).await;
        assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, field).is_empty(), "{name}: {body}");
    }

    // 計算式で反映する内訳には直接入力できないが、同じ科目への直接入力はできる
    let fpath = format!("{}/amounts", f.values_path(f.budget, f.formula_act));
    for (name, amount) in [
        (
            "計算式の内訳への直接入力",
            json!({"subject_id": f.sales, "line_id": f.sales_line, "target_month": "2026-05", "amount": 1}),
        ),
        (
            "別科目の内訳の指定",
            json!({"subject_id": f.cost, "line_id": f.sales_line, "target_month": "2026-05", "amount": 1}),
        ),
    ] {
        let (status, body) = f.member.put(&fpath, json!({"reason": "r", "amounts": [amount]})).await;
        assert!(
            status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "amounts[0].line_id").is_empty(),
            "{name}: {body}"
        );
    }
    let (status, body) = f
        .member
        .put(
            &fpath,
            json!({"reason": "r", "amounts": [{"subject_id": f.sales, "target_month": "2026-05", "amount": 1}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "計算式の内訳がある科目への直接入力: {body}");

    // null で削除
    let (status, body) = f
        .member
        .put(
            &path,
            json!({"reason": "取消", "amounts": [{"subject_id": f.sales, "target_month": "2026-04", "amount": null}]}),
        )
        .await;
    assert!(status == StatusCode::OK && amount_of(&body, f.sales, "2026-04").0.is_empty(), "削除: {body}");
}

#[tokio::test]
async fn value_editing_restrictions() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let values = json!([{"subject_id": f.sales, "target_month": "2026-04", "amount": 1}]);
    let amounts = |s: i64| format!("{}/amounts", f.values_path(s, f.manual_act));

    // 担当外・閲覧者は編集できない。閲覧者は参照できるが editable は false
    for (name, c) in [("担当外", &f.member2), ("閲覧者", &f.viewer)] {
        let (status, _) = c.put(&amounts(f.budget), json!({"reason": "r", "amounts": values})).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{name} の金額入力");
    }
    assert_eq!(f.viewer.must_get(&f.values_path(f.budget, f.manual_act)).await["editable"], json!(false));

    // 作成中ではないシナリオは FP&A のみ、ロック済みのシナリオはだれも編集できない
    let other = f.admin.must_create("/api/scenarios", json!({"name": "2026年度 楽観", "fiscal_year": 2026})).await;
    assert_eq!(f.member.put(&amounts(other), json!({"reason": "r", "amounts": values})).await.0, StatusCode::CONFLICT);
    assert_eq!(f.member.must_get(&f.values_path(other, f.manual_act)).await["editable"], json!(false));
    let (status, body) = f.admin.put(&amounts(other), json!({"reason": "r", "amounts": values})).await;
    assert_eq!(status, StatusCode::OK, "作成中ではないシナリオへの FP&A の入力: {body}");
    f.admin.post(&format!("/api/scenarios/{}/lock", f.budget), Value::Null).await;
    for (name, c) in [("担当者", &f.member), ("FP&A", &f.admin)] {
        assert_eq!(
            c.put(&amounts(f.budget), json!({"reason": "r", "amounts": values})).await.0,
            StatusCode::CONFLICT,
            "{name}"
        );
        assert_eq!(c.must_get(&f.values_path(f.budget, f.manual_act)).await["editable"], json!(false), "{name}");
    }
}

#[tokio::test]
async fn restricted_subject_amounts() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let secret = f
        .admin
        .must_create(
            "/api/subjects",
            json!({"code": "9000", "name": "役員報酬", "category": "expense", "is_restricted": true}),
        )
        .await;
    let path = format!("{}/amounts", f.values_path(f.budget, f.manual_act));
    let body = json!({"reason": "r", "amounts": [{"subject_id": secret, "target_month": "2026-10", "amount": 100}]});
    assert_eq!(
        f.member.put(&path, body.clone()).await.0,
        StatusCode::FORBIDDEN,
        "担当者は閲覧制限のある科目を入力できない"
    );
    assert_eq!(f.admin.put(&path, body).await.0, StatusCode::OK);
    // 担当者には見えず、restricted_hidden が付く
    let m = f.member.must_get(&f.values_path(f.budget, f.manual_act)).await;
    assert!(amount_of(&m, secret, "2026-10").0.is_empty() && m["restricted_hidden"] == json!(true), "{m}");
    let v = f.viewer.must_get(&f.values_path(f.budget, f.manual_act)).await;
    assert!(amount_of(&v, secret, "2026-10").0 == "100" && v["restricted_hidden"] == json!(false), "{v}");
}

#[tokio::test]
async fn default_reason() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let amounts = json!([{"subject_id": f.cost, "target_month": "2026-10", "amount": 100}]);
    let (status, body) =
        f.member.put(&format!("{}/amounts", f.values_path(f.budget, f.manual_act)), json!({"amounts": amounts})).await;
    assert_eq!(status, StatusCode::OK, "理由なし（作成中）: {body}");
    let reason: String = f.env.query_one("SELECT reason FROM change_sets ORDER BY id DESC LIMIT 1", ()).await;
    assert_eq!(reason, "2026年度 当初予算の見込更新");
    let other = f.admin.must_create("/api/scenarios", json!({"name": "比較用", "fiscal_year": 2026})).await;
    let (status, body) =
        f.admin.put(&format!("{}/amounts", f.values_path(other, f.manual_act)), json!({"amounts": amounts})).await;
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "reason").is_empty(),
        "理由なし（作成中以外）: {body}"
    );
    let (status, _) = f
        .member
        .put(&format!("{}/condition", f.values_path(f.budget, f.manual_act)), json!({"description": "x"}))
        .await;
    assert_eq!(status, StatusCode::GONE, "想定条件の API");
}

#[tokio::test]
async fn amount_lines() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let base = format!("/api/activities/{}", f.formula_act);
    let vpath = f.values_path(f.budget, f.formula_act);
    let lpath = format!("{base}/lines/{}", f.sales_line);
    let setup_line =
        f.member.must_create(&format!("{base}/lines"), json!({"subject_id": f.sales, "name": "初期導入費"})).await;

    // 同じ科目に、計算式の内訳・直接入力の内訳・科目への直接入力が並ぶ
    f.member
        .put(
            &format!("{vpath}/driver-values"),
            json!({"reason": "r", "values": [
                {"driver_id": f.price_id, "target_month": "2026-10", "value": 1000},
                {"driver_id": f.volume_id, "target_month": "2026-10", "value": 3},
            ]}),
        )
        .await;
    let (status, body) = f
        .member
        .put(
            &format!("{vpath}/amounts"),
            json!({"reason": "r", "amounts": [
                {"subject_id": f.sales, "line_id": setup_line, "target_month": "2026-10", "amount": 200000},
                {"subject_id": f.sales, "target_month": "2026-10", "amount": 1000},
            ]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "金額の入力: {body}");
    assert_eq!(line_amount_of(&body, f.sales_line, "2026-10").0, "1500");
    assert_eq!(line_amount_of(&body, setup_line, "2026-10").0, "200000");
    assert_eq!(amount_of(&body, f.sales, "2026-10").0, "1000");
    // 再計算しても、ほかの内訳・直接入力の金額は消えない
    f.member
        .put(
            &format!("{vpath}/driver-values"),
            json!({"reason": "r", "values": [{"driver_id": f.volume_id, "target_month": "2026-10", "value": 4}]}),
        )
        .await;
    let body = f.member.must_get(&vpath).await;
    assert_eq!(
        (
            line_amount_of(&body, f.sales_line, "2026-10").0,
            line_amount_of(&body, setup_line, "2026-10").0,
            amount_of(&body, f.sales, "2026-10").0
        ),
        ("2000".into(), "200000".into(), "1000".into())
    );

    // 反映をやめると金額は残り、直接入力できるようになる
    let (status, body) = f
        .member
        .put(&lpath, json!({"name": "利用料", "expression": "unit_price * volume * 0.5", "formula_enabled": false, "reason": "値引き交渉中のため手入力"}))
        .await;
    assert_eq!(status, StatusCode::OK, "反映をやめる: {body}");
    let (status, body) = f
        .member
        .put(
            &format!("{vpath}/amounts"),
            json!({"reason": "値引き", "amounts": [{"subject_id": f.sales, "line_id": f.sales_line, "target_month": "2026-10", "amount": 1800}]}),
        )
        .await;
    let (got, cell) = line_amount_of(&body, f.sales_line, "2026-10");
    assert!(status == StatusCode::OK && got == "1800" && cell["source"] == "manual", "反映しない内訳への入力: {cell}");
    // 反映に戻すと再計算される
    let (status, _) = f
        .member
        .put(&lpath, json!({"name": "利用料", "expression": "unit_price * volume * 0.5", "formula_enabled": true, "reason": "式に戻す"}))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (got, cell) = line_amount_of(&f.member.must_get(&vpath).await, f.sales_line, "2026-10");
    assert!(got == "2000" && cell["source"] == "formula", "反映に戻した後 = {cell}");

    // 複製したシナリオにも内訳の金額が引き継がれる
    let forecast = f
        .admin
        .must_create("/api/scenarios", json!({"name": "見込", "fiscal_year": 2026, "base_scenario_id": f.budget}))
        .await;
    let body = f.viewer.must_get(&f.values_path(forecast, f.formula_act)).await;
    assert_eq!(
        (line_amount_of(&body, f.sales_line, "2026-10").0, line_amount_of(&body, setup_line, "2026-10").0),
        ("2000".into(), "200000".into())
    );

    // ロック済みシナリオに金額がある内訳は削除できない。ロックがなければ金額ごと削除できる
    f.admin.post(&format!("/api/scenarios/{}/lock", f.budget), Value::Null).await;
    let setup_path = format!("{base}/lines/{setup_line}");
    assert_eq!(f.member.do_req("DELETE", &setup_path, Some(json!({"reason": "r"}))).await.0, StatusCode::CONFLICT);
    f.admin.post(&format!("/api/scenarios/{}/unlock", f.budget), json!({"reason": "r"})).await;
    let (status, body) = f.member.do_req("DELETE", &setup_path, Some(json!({"reason": "内訳の整理"}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "内訳の削除: {body}");
    assert_eq!(
        amount_of(&f.viewer.must_get(&f.values_path(forecast, f.formula_act)).await, f.sales, "2026-10").0,
        "1000"
    );
    let n: i64 = f.env.query_one("SELECT COUNT(*) FROM budget_facts WHERE line_id = ?", (setup_line,)).await;
    assert_eq!(n, 0, "削除した内訳の金額が残っている");
}

#[tokio::test]
async fn scenario_roles_and_active() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let scenario = async |id: i64| f.viewer.must_get(&format!("/api/scenarios/{id}")).await;

    // エイリアスは年度ごとに1つ。別のシナリオに付けると前のシナリオからは外れる
    let r1 = f
        .admin
        .must_create("/api/scenarios", json!({"name": "修正計画A", "fiscal_year": 2026, "plan_role": "revised"}))
        .await;
    let r2 = f
        .admin
        .must_create("/api/scenarios", json!({"name": "修正計画B", "fiscal_year": 2026, "plan_role": "revised"}))
        .await;
    assert!(scenario(r1).await["plan_role"].is_null(), "付け替え後の修正計画A");
    assert_eq!(scenario(r2).await["plan_role"], "revised");
    // 別の年度なら同じエイリアスを付けられる
    let next = f
        .admin
        .must_create("/api/scenarios", json!({"name": "2027期初", "fiscal_year": 2027, "plan_role": "initial"}))
        .await;
    assert_eq!(scenario(f.budget).await["plan_role"], "initial");
    // PUT でも付け替えられる
    let (status, body) =
        f.admin.put(&format!("/api/scenarios/{r1}"), json!({"name": "修正計画A", "plan_role": "revised"})).await;
    assert!(status == StatusCode::OK && body["plan_role"] == "revised", "PUT での付け替え: {body}");
    assert!(scenario(r2).await["plan_role"].is_null());

    // 作成中はアプリ全体で1つ
    assert_eq!(f.viewer.must_get("/api/scenarios/active").await["id"], json!(f.budget));
    let (status, body) = f.admin.post(&format!("/api/scenarios/{next}/activate"), Value::Null).await;
    assert!(status == StatusCode::OK && body["is_active"] == json!(true), "作成中に指定: {body}");
    assert_eq!(scenario(f.budget).await["is_active"], json!(false));
    assert_eq!(
        f.manager1.post(&format!("/api/scenarios/{}/activate", f.budget), Value::Null).await.0,
        StatusCode::FORBIDDEN
    );
    // 作成中のシナリオをロックすると作成中は外れ、ロック済みは作成中にできない
    let (status, body) = f.admin.post(&format!("/api/scenarios/{next}/lock"), Value::Null).await;
    assert!(status == StatusCode::OK && body["is_active"] == json!(false), "作成中のロック: {body}");
    let (status, body) = f.viewer.get("/api/scenarios/active").await;
    assert!(status == StatusCode::OK && body.is_null(), "ロック後の作成中 = {body}");
    assert_eq!(f.admin.post(&format!("/api/scenarios/{next}/activate"), Value::Null).await.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn scenario_actual_through() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let path = format!("/api/scenarios/{}", f.budget);
    let vpath = f.values_path(f.budget, f.manual_act);

    // 計画値: 4〜6月。実績: 4・5月
    f.member
        .put(
            &format!("{vpath}/amounts"),
            json!({"reason": "計画", "amounts": [
                {"subject_id": f.sales, "target_month": "2026-04", "amount": 5000},
                {"subject_id": f.sales, "target_month": "2026-05", "amount": 6000},
                {"subject_id": f.sales, "target_month": "2026-06", "amount": 7000},
            ]}),
        )
        .await;
    f.import_actual("2026-04", 1000).await;
    f.import_actual("2026-05", 900).await;

    // 決算確定月の変更は理由が必須
    let (status, body) = f
        .admin
        .put(&path, json!({"name": "2026年度 当初予算", "plan_role": "initial", "actual_through": "2026-05"}))
        .await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "reason").is_empty(), "{body}");
    let (status, body) = f
        .admin
        .put(&path, json!({"name": "2026年度 当初予算", "plan_role": "initial", "actual_through": "2026-05", "reason": "5月決算確定"}))
        .await;
    assert!(status == StatusCode::OK && body["actual_through"] == "2026-05", "決算確定月の変更: {body}");

    // 決算確定月以前は実績、それより後は計画値
    let body = f.member.must_get(&vpath).await;
    let (got, cell) = amount_of(&body, f.sales, "2026-04");
    assert!(got == "1000" && cell["source"] == "actual", "4月 = {cell}");
    assert_eq!(amount_of(&body, f.sales, "2026-06").0, "7000");
    assert_eq!(body["actual_months"], json!(["2026-04", "2026-05"]));
    // 実績の月は入力できない（金額・ドライバー値とも）
    let (status, res) = f
        .member
        .put(
            &format!("{vpath}/amounts"),
            json!({"reason": "r", "amounts": [{"subject_id": f.sales, "target_month": "2026-05", "amount": 1}]}),
        )
        .await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&res, "amounts[0].target_month").is_empty(), "{res}");
    let (status, res) = f
        .member
        .put(
            &format!("{}/driver-values", f.values_path(f.budget, f.formula_act)),
            json!({"reason": "r", "values": [{"driver_id": f.price_id, "target_month": "2026-04", "value": 1}]}),
        )
        .await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&res, "values[0].target_month").is_empty(), "{res}");

    // ロックされていない間は実績を参照するので、取り込み直すと反映される
    f.import_actual("2026-04", 1100).await;
    assert_eq!(amount_of(&f.member.must_get(&vpath).await, f.sales, "2026-04").0, "1100");
    // ロックすると実績を保存して固定する。解除すると再び参照する
    let (_, locked) = f.admin.post(&format!("{path}/lock"), Value::Null).await;
    assert_eq!(locked["is_locked"], json!(true));
    let frozen: String = f
        .env
        .query_one("SELECT CAST(after_json AS CHAR) FROM audit_logs WHERE table_name = 'scenarios' AND record_id = ? ORDER BY id DESC LIMIT 1", (f.budget,))
        .await;
    assert_eq!(serde_json::from_str::<Value>(&frozen).unwrap()["frozen_actuals"], json!(2));
    f.import_actual("2026-04", 1200).await;
    assert_eq!(amount_of(&f.member.must_get(&vpath).await, f.sales, "2026-04").0, "1100", "ロック後の4月（固定）");
    let (status, _) =
        f.admin.put(&path, json!({"name": "2026年度 当初予算", "actual_through": "2026-06", "reason": "r"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "ロック済みの決算確定月の変更");
    f.admin.post(&format!("{path}/unlock"), json!({"reason": "修正"})).await;
    assert_eq!(amount_of(&f.member.must_get(&vpath).await, f.sales, "2026-04").0, "1200", "ロック解除後の4月");
}

#[tokio::test]
async fn recalculate_only_plan_months() {
    let Some(f) = ScenarioFixture::new().await else { return };
    f.member
        .put(
            &format!("{}/driver-values", f.values_path(f.budget, f.formula_act)),
            json!({"reason": "r", "values": [
                {"driver_id": f.price_id, "target_month": "2026-04", "value": 1000},
                {"driver_id": f.volume_id, "target_month": "2026-04", "value": 2},
                {"driver_id": f.price_id, "target_month": "2026-05", "value": 1000},
                {"driver_id": f.volume_id, "target_month": "2026-05", "value": 2},
            ]}),
        )
        .await;
    // 4月を実績の月にしてから計算式を変えると、計算式の金額は5月だけ再計算される
    f.admin
        .put(
            &format!("/api/scenarios/{}", f.budget),
            json!({"name": "2026年度 当初予算", "actual_through": "2026-04", "reason": "4月決算確定"}),
        )
        .await;
    let (status, body) = f
        .member
        .put(
            &format!("/api/activities/{}/lines/{}", f.formula_act, f.sales_line),
            json!({"name": "利用料", "expression": "unit_price * volume", "formula_enabled": true, "reason": "式の見直し"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "計算式の変更: {body}");
    let amount = async |month: &str| -> String {
        f.env
            .query_one(
                "SELECT CAST(amount AS CHAR) FROM budget_facts WHERE scenario_id = ? AND line_id = ? AND target_month = ?",
                (f.budget, f.sales_line, format!("{month}-01")),
            )
            .await
    };
    assert_eq!((amount("2026-04").await, amount("2026-05").await), ("1000".into(), "2000".into()));
}

#[tokio::test]
async fn reorder_drivers_in_values() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let base = format!("/api/activities/{}", f.formula_act);
    let extra = f
        .admin
        .must_create(&format!("{base}/drivers"), json!({"code": "discount", "name": "値引き", "driver_kind": "value"}))
        .await;
    let codes = async || -> Vec<String> {
        let body = f.viewer.must_get(&f.values_path(f.budget, f.formula_act)).await;
        body["drivers"].as_array().unwrap().iter().map(|d| d["code"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(codes().await, vec!["unit_price", "volume", "discount"], "初期の順（作成順）");
    let (status, body) =
        f.member.put(&format!("{base}/drivers/order"), json!({"ids": [extra, f.price_id, f.volume_id]})).await;
    assert!(status == StatusCode::OK && items(&body).len() == 3, "並び替え: {body}");
    assert_eq!(codes().await, vec!["discount", "unit_price", "volume"]);
}

#[tokio::test]
async fn driver_values_dry_run() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let path = format!("{}/driver-values", f.values_path(f.budget, f.formula_act));
    let values = json!([
        {"driver_id": f.price_id, "target_month": "2026-10", "value": 1000},
        {"driver_id": f.volume_id, "target_month": "2026-10", "value": 3},
    ]);
    let before: i64 = f.env.query_one("SELECT COUNT(*) FROM change_sets", ()).await;

    // 試算は理由なしで、計算式の金額を返す（1000 * 3 * 0.5 = 1500）
    let (status, body) = f.member.put(&format!("{path}?dry_run=true"), json!({"values": values})).await;
    assert_eq!(status, StatusCode::OK, "試算: {body}");
    assert_eq!(line_amount_of(&body, f.sales_line, "2026-10").0, "1500");
    // 何も保存されず、変更履歴も残らない
    assert_eq!(
        line_amount_of(&f.member.must_get(&f.values_path(f.budget, f.formula_act)).await, f.sales_line, "2026-10").0,
        ""
    );
    let after: i64 = f.env.query_one("SELECT COUNT(*) FROM change_sets", ()).await;
    assert_eq!(after, before, "変更セットが増えた");
    // 試算でも検証は行う。編集できないユーザーは試算もできない
    let bad = json!([{"driver_id": f.price_id, "target_month": "2026-10", "value": "0.1234567"}]);
    let (status, body) = f.member.put(&format!("{path}?dry_run=true"), json!({"values": bad})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "values[0].value").is_empty(), "{body}");
    assert_eq!(
        f.member2.put(&format!("{path}?dry_run=true"), json!({"values": values})).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn previous_scenario() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let get = async |id: i64| f.viewer.must_get(&format!("/api/scenarios/{id}")).await;

    // 複製して作ると、前回見込は複製元
    let oct = f
        .admin
        .must_create("/api/scenarios", json!({"name": "10月見込", "fiscal_year": 2026, "base_scenario_id": f.budget}))
        .await;
    assert_eq!(get(oct).await["previous_scenario_id"], json!(f.budget));
    // 明示すれば複製元と別でもよい。複製しなければ未設定
    let nov = f
        .admin
        .must_create(
            "/api/scenarios",
            json!({"name": "11月見込", "fiscal_year": 2026, "base_scenario_id": f.budget, "previous_scenario_id": oct}),
        )
        .await;
    assert_eq!(get(nov).await["previous_scenario_id"], json!(oct));
    assert!(get(f.budget).await["previous_scenario_id"].is_null());

    // 変更・解除
    let path = format!("/api/scenarios/{nov}");
    let (status, body) = f.admin.put(&path, json!({"name": "11月見込", "previous_scenario_id": f.budget})).await;
    assert!(status == StatusCode::OK && body["previous_scenario_id"] == json!(f.budget), "変更: {body}");
    let (status, body) = f.admin.put(&path, json!({"name": "11月見込", "previous_scenario_id": null})).await;
    assert!(status == StatusCode::OK && body["previous_scenario_id"].is_null(), "解除: {body}");

    let other = f.admin.must_create("/api/scenarios", json!({"name": "2027予算", "fiscal_year": 2027})).await;
    for (name, prev) in [("自分自身", nov), ("年度違い", other), ("存在しない", 99999)] {
        let (status, body) = f.admin.put(&path, json!({"name": "11月見込", "previous_scenario_id": prev})).await;
        assert!(
            status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "previous_scenario_id").is_empty(),
            "{name}: {body}"
        );
    }
}

#[tokio::test]
async fn note_and_completion() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let vpath = f.values_path(f.budget, f.manual_act);
    let status = async || f.viewer.must_get(&vpath).await["note"]["status"].as_str().unwrap().to_string();
    assert_eq!(status().await, "not_started");

    // 数値を変えると入力中
    f.member
        .put(&format!("{vpath}/amounts"), json!({"reason": "見込更新", "amounts": [{"subject_id": f.sales, "target_month": "2026-10", "amount": 1000}]}))
        .await;
    assert_eq!(status().await, "in_progress");

    // 差異の説明と要因の分類
    let (st, body) = f.member.put(&format!("{vpath}/note"), json!({"explanation": "x", "causes": ["weather"]})).await;
    assert!(st == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "causes").is_empty(), "要因の分類が不正: {body}");
    let (st, body) = f
        .member
        .put(&format!("{vpath}/note"), json!({"explanation": "A社の受注が11月にずれた", "causes": ["new", "timing"]}))
        .await;
    let n = &body["note"];
    assert!(
        st == StatusCode::OK
            && n["explanation"] == "A社の受注が11月にずれた"
            && n["causes"] == json!(["timing", "new"]),
        "説明の保存: {n}"
    );

    // 完了にすると、完了した人と日時が残る
    let (st, body) = f.member.post(&format!("{vpath}/complete"), Value::Null).await;
    let n = &body["note"];
    assert!(
        st == StatusCode::OK
            && n["status"] == "completed"
            && n["completed_by"] == json!(f.member_id)
            && !n["completed_at"].is_null(),
        "完了: {n}"
    );
    let note = f.viewer.must_get(&format!("{vpath}/note")).await;
    assert!(note["completed_by_name"] == "テスト member" && note.get("id").is_none(), "{note}");
    // 完了の後に数値を変えると入力中に戻る（試算では戻らない）
    f.member
        .put(
            &format!("{}/driver-values?dry_run=true", f.values_path(f.budget, f.formula_act)),
            json!({"values": [{"driver_id": f.price_id, "target_month": "2026-10", "value": 1}]}),
        )
        .await;
    assert_eq!(status().await, "completed", "試算では戻らない");
    f.member
        .put(
            &format!("{vpath}/amounts"),
            json!({"reason": "追加", "amounts": [{"subject_id": f.sales, "target_month": "2026-11", "amount": 500}]}),
        )
        .await;
    assert_eq!(status().await, "in_progress", "完了後の数値の変更");
    // 説明を変えても入力中に戻る。完了の取り消しもできる
    f.member.post(&format!("{vpath}/complete"), Value::Null).await;
    f.member
        .put(&format!("{vpath}/note"), json!({"explanation": "A社の受注が12月にずれた", "causes": ["timing"]}))
        .await;
    assert_eq!(status().await, "in_progress", "完了後の説明の変更");
    f.member.post(&format!("{vpath}/complete"), Value::Null).await;
    let (st, body) = f.member.delete(&format!("{vpath}/complete")).await;
    assert!(st == StatusCode::OK && body["note"]["status"] == "in_progress", "完了の取り消し: {body}");

    // 権限は数値の入力と同じ
    assert_eq!(f.member2.put(&format!("{vpath}/note"), json!({"explanation": "x"})).await.0, StatusCode::FORBIDDEN);
    assert_eq!(f.viewer.post(&format!("{vpath}/complete"), Value::Null).await.0, StatusCode::FORBIDDEN);
    // 何も変えずに完了にもできる（見込を変えなかった施策）
    let (st, body) = f.member.post(&format!("{}/complete", f.values_path(f.budget, f.formula_act)), Value::Null).await;
    assert!(st == StatusCode::OK && body["note"]["status"] == "completed", "変更なしで完了: {body}");
    // ロック済みでは完了の操作もできない
    f.admin.post(&format!("/api/scenarios/{}/lock", f.budget), Value::Null).await;
    assert_eq!(f.member.delete(&format!("{vpath}/complete")).await.0, StatusCode::CONFLICT);
    // 変更履歴に残る
    let logs: i64 =
        f.env.query_one("SELECT COUNT(*) FROM audit_logs WHERE table_name = 'activity_scenario_notes'", ()).await;
    assert!(logs > 0, "差異の説明・完了の監査ログがない");
}

#[tokio::test]
async fn activity_status() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let amount = async |sid: i64, aid: i64, month: &str, v: i64| {
        let (status, body) = f
            .admin
            .put(
                &format!("{}/amounts", f.values_path(sid, aid)),
                json!({"reason": "r", "amounts": [{"subject_id": f.sales, "target_month": month, "amount": v}]}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "amounts: {body}");
    };
    // 基準（期初計画）: 4月 1000・5月 1000 → 前回見込: 4月を 1200 に → 今回: 前回見込を複製
    amount(f.budget, f.manual_act, "2026-04", 1000).await;
    amount(f.budget, f.manual_act, "2026-05", 1000).await;
    let prev = f
        .admin
        .must_create("/api/scenarios", json!({"name": "9月見込", "fiscal_year": 2026, "base_scenario_id": f.budget}))
        .await;
    amount(prev, f.manual_act, "2026-04", 1200).await;
    let cur = f
        .admin
        .must_create("/api/scenarios", json!({"name": "10月見込", "fiscal_year": 2026, "base_scenario_id": prev}))
        .await;
    f.admin.post(&format!("/api/scenarios/{cur}/activate"), Value::Null).await;
    // 4月の実績 900 を入れ、今回の決算確定月を 4月にする
    f.import_actual("2026-04", 900).await;
    f.admin
        .put(
            &format!("/api/scenarios/{cur}"),
            json!({"name": "10月見込", "actual_through": "2026-04", "previous_scenario_id": prev, "reason": "4月決算確定"}),
        )
        .await;
    // 担当者が見込を更新し、別の施策は完了にする
    f.member
        .put(
            &format!("{}/amounts", f.values_path(cur, f.manual_act)),
            json!({"reason": "上積み", "amounts": [{"subject_id": f.sales, "target_month": "2026-05", "amount": 1500}]}),
        )
        .await;
    f.member.post(&format!("{}/complete", f.values_path(cur, f.formula_act)), Value::Null).await;

    let path = format!("/api/scenarios/{cur}/activity-status");
    let body = f.member.must_get(&path).await;
    assert!(body["scope"] == "mine" && body["new_actual_months"] == json!(["2026-04"]), "{body}");
    assert_eq!(body["base"]["id"], json!(f.budget), "基準は期初計画");
    let item = |code: &str| items(&body).iter().find(|m| m["code"] == code).cloned().unwrap();
    assert_eq!(items(&body).len(), 2, "担当施策");
    let prj = item("PRJ-1");
    assert!(
        prj["status"] == "in_progress"
            && prj["current"]["revenue"] == "2400"
            && prj["base"]["revenue"] == "2000"
            && prj["previous"]["revenue"] == "2200",
        "PRJ-1 = {prj}"
    );
    // 4月: 前回見込 1200 に対して実績 900 → 差 25%
    let acc = &prj["accuracy"];
    assert!(
        acc["large"] == json!(true)
            && serde_json::to_string(&acc["rate"]).unwrap() == "25"
            && acc["actual"]["revenue"] == "900",
        "{acc}"
    );
    assert_eq!(item("SAAS-1")["status"], "completed");
    // 期初計画（budget）の合計と、修正計画がないこと
    assert!(
        prj["initial"]["revenue"] == "2000" && prj["revised"].is_null() && body["initial"]["id"] == json!(f.budget),
        "{prj}"
    );

    // 範囲の既定: マネージャーは所管ユニット、閲覧者は全体、担当外の担当者は0件
    let m = f.manager1.must_get(&path).await;
    assert!(m["scope"] == "units" && items(&m).len() == 2, "{m}");
    assert_eq!(f.viewer.must_get(&path).await["scope"], "all");
    assert_eq!(items(&f.member2.must_get(&path).await).len(), 0);
    assert_eq!(f.member.get(&format!("{path}?scope=team")).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn home_milestones() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let ms = format!("/api/activities/{}/milestones", f.manual_act);
    let day = |d: i64| (Local::now().date_naive() + Duration::days(d)).format("%Y-%m-%d").to_string();
    let late = f.member.must_create(&ms, json!({"name": "要件定義", "due_date": "2020-01-01"})).await;
    f.member.must_create(&ms, json!({"name": "設計", "due_date": day(10)})).await;
    let moved = f.member.must_create(&ms, json!({"name": "リリース", "due_date": day(90)})).await;
    let done = f.member.must_create(&ms, json!({"name": "キックオフ", "due_date": "2020-01-01"})).await;
    f.member
        .put(&format!("{ms}/{done}"), json!({"name": "キックオフ", "due_date": "2020-01-01", "status": "completed"}))
        .await;

    // 前回見込（予算）の作成以降に、リリースを2回後ろ倒し（+10日・+5日）、1回前倒し（数えない）
    let cur = f
        .admin
        .must_create("/api/scenarios", json!({"name": "見込", "fiscal_year": 2026, "base_scenario_id": f.budget}))
        .await;
    for d in [100, 105, 102] {
        let (status, body) = f
            .member
            .put(
                &format!("{ms}/{moved}"),
                json!({"name": "リリース", "due_date": day(d), "status": "in_progress", "reason": "顧客都合"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "期日の変更: {body}");
    }

    let body = f
        .viewer
        .must_get(&format!("/api/scenarios/{cur}/milestones?activity_ids={},{}", f.manual_act, f.formula_act))
        .await;
    let item = |name: &str| items(&body).iter().find(|m| m["name"] == name).cloned().unwrap();
    assert_eq!(items(&body).len(), 3, "完了していないマイルストーン: {body}");
    assert!(item("要件定義")["overdue"] == json!(true) && item("要件定義")["id"] == json!(late));
    assert!(item("設計")["upcoming"] == json!(true) && item("設計")["overdue"] == json!(false));
    assert_eq!(item("リリース")["postponed"], json!({"count": 2, "days": 15}));
    assert!(body["since"].as_str().unwrap().ends_with("+09:00"));
    assert_eq!(f.viewer.must_get(&format!("/api/scenarios/{cur}/milestones")).await["items"], json!([]));
    assert_eq!(
        f.viewer.get(&format!("/api/scenarios/{cur}/milestones?activity_ids=x")).await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn start_monthly() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let a = &f.admin;
    // 作成中の版（期初計画）に 9月の計画値を入れ、4月の実績を入れる
    let (status, body) = a
        .put(
            &format!("{}/amounts", f.values_path(f.budget, f.manual_act)),
            json!({"reason": "r", "amounts": [{"subject_id": f.sales, "target_month": "2026-09", "amount": 700}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    f.import_actual("2026-04", 900).await;

    let req = json!({"name": "2026年5月見込", "actual_through": "2026-05", "update_deadline": "2026-06-10"});
    // dry run: 変わる内容と注意（5月の実績が未取込、未完了の施策）を返し、何も変えない
    let mut dry = req.clone();
    dry["dry_run"] = json!(true);
    let (status, body) = a.post("/api/scenarios/start-monthly", dry).await;
    assert!(status == StatusCode::OK && body["lock"]["name"] == "2026年度 当初予算", "dry run: {body}");
    let warnings = body["warnings"].to_string();
    assert!(warnings.contains("2026-05") && warnings.contains("完了にしていない施策が 2 件"), "注意 = {warnings}");
    assert_eq!(items(&a.must_get("/api/scenarios").await).len(), 1, "dry run なのにシナリオが増えた");

    // 実行: 前の版をロック → 複製 → 決算確定月・最新見込・前回見込・作成中・締切
    let (status, body) = a.post("/api/scenarios/start-monthly", req.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "実行: {body}");
    let s = &body["scenario"];
    assert!(
        s["actual_through"] == "2026-05"
            && s["plan_role"] == "latest"
            && s["is_active"] == json!(true)
            && s["update_deadline"] == "2026-06-10"
            && s["previous_scenario_id"] == json!(f.budget),
        "新しい版 = {s}"
    );
    let new_id = s["id"].as_i64().unwrap();
    let old = a.must_get(&format!("/api/scenarios/{}", f.budget)).await;
    assert!(
        old["is_locked"] == json!(true) && old["is_active"] == json!(false) && old["plan_role"] == "initial",
        "前の版 = {old}"
    );
    // 計画値が複製され、4月は実績になっている
    let view = a.must_get(&f.values_path(new_id, f.manual_act)).await;
    assert_eq!(amount_of(&view, f.sales, "2026-09").0, "700");
    assert_eq!(amount_of(&view, f.sales, "2026-04").0, "900");

    // 翌月: 決算確定月は前の版以降。最新見込は付け替わる
    let (status, body) =
        a.post("/api/scenarios/start-monthly", json!({"name": "戻す", "actual_through": "2026-04"})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "actual_through").is_empty(), "{body}");
    let (status, body) =
        a.post("/api/scenarios/start-monthly", json!({"name": "別年度", "actual_through": "2027-04"})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && detail(&body, "actual_through").contains("新年度"), "{body}");
    let (status, body) =
        a.post("/api/scenarios/start-monthly", json!({"name": "2026年6月見込", "actual_through": "2026-06"})).await;
    assert!(status == StatusCode::CREATED && body["role_from"]["name"] == "2026年5月見込", "翌月: {body}");
    let prev = a.must_get(&format!("/api/scenarios/{new_id}")).await;
    assert!(prev["plan_role"].is_null() && prev["is_locked"] == json!(true), "{prev}");
    assert_eq!(f.manager1.post("/api/scenarios/start-monthly", req).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn start_monthly_warns_drifted_actuals() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let a = &f.admin;
    f.import_actual("2026-04", 900).await;
    let (status, body) =
        a.post("/api/scenarios/start-monthly", json!({"name": "5月見込", "actual_through": "2026-05"})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    a.post("/api/scenarios/start-monthly", json!({"name": "6月見込", "actual_through": "2026-06"})).await;
    // ロック済みの「当初予算」「5月見込」に保存した4月の実績と、今の実績が食い違う（作成中の「6月見込」はまだロックしていない）
    f.import_actual("2026-04", 950).await;
    let (_, body) = a
        .post("/api/scenarios/start-monthly", json!({"name": "7月見込", "actual_through": "2026-07", "dry_run": true}))
        .await;
    assert!(body["warnings"].to_string().contains("実績の修正あり」があります: 5月見込\""), "{body}");
}

#[tokio::test]
async fn start_fiscal_year() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let a = &f.admin;
    let (status, body) = a
        .post(
            "/api/scenarios/start-fiscal-year",
            json!({"fiscal_year": 2027, "name": "2027年度 期初計画", "dry_run": true}),
        )
        .await;
    assert!(
        status == StatusCode::OK && body["warnings"].to_string().contains("2026年度）を締めていません"),
        "dry run: {body}"
    );
    let (status, body) =
        a.post("/api/scenarios/start-fiscal-year", json!({"fiscal_year": 2027, "name": "2027年度 期初計画"})).await;
    let s = &body["scenario"];
    assert!(
        status == StatusCode::CREATED
            && s["fiscal_year"] == json!(2027)
            && s["plan_role"] == "initial"
            && s["actual_through"].is_null()
            && s["is_active"] == json!(true),
        "新年度: {body}"
    );
    assert_eq!(a.must_get(&format!("/api/scenarios/{}", f.budget)).await["is_locked"], json!(true));
    // 新しい版は空
    let view = a.must_get(&f.values_path(s["id"].as_i64().unwrap(), f.manual_act)).await;
    assert_eq!(amount_of(&view, f.sales, "2027-04").0, "");
}

#[tokio::test]
async fn comments() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let path = format!("{}/comments", f.values_path(f.budget, f.manual_act));
    let notices = async |user_id: i64| -> Vec<(String, String)> {
        f.env
            .query(
                "SELECT title, link FROM notifications WHERE user_id = ? AND kind = 'comment' ORDER BY id",
                (user_id,),
            )
            .await
    };

    // マネージャーが書く → 担当者（member）にお知らせ。書いた本人には送らない
    let (status, body) = f.manager1.post(&path, json!({"body": "単価を据え置いた理由も書いてください"})).await;
    assert_eq!(status, StatusCode::CREATED, "投稿: {body}");
    let n = notices(f.member_id).await;
    assert!(n.len() == 1 && n[0].0.contains("コメントしました"), "担当者へのお知らせ = {n:?}");
    assert_eq!(n[0].1, format!("/activities/{}?tab=update&scenario={}", f.manual_act, f.budget));
    assert!(notices(f.manager1_id).await.is_empty(), "書いた本人へのお知らせ");

    // 担当者が返信 → すでに書いたマネージャーにお知らせ。閲覧者（経営陣）も書ける
    assert_eq!(f.member.post(&path, json!({"body": "値上げは来期の予定のためです"})).await.0, StatusCode::CREATED);
    assert_eq!(notices(f.manager1_id).await.len(), 1);
    let (status, body) = f.viewer.post(&path, json!({"body": "了解です"})).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(items(&body).len() == 3 && body["can_post"] == json!(true), "一覧 = {body}");
    assert_eq!(notices(f.member_id).await.len(), 2, "担当者へのお知らせ（マネージャー・閲覧者）");

    // コメントは更新の状態を変えない
    let note = f.member.must_get(&format!("{}/note", f.values_path(f.budget, f.manual_act))).await;
    assert_eq!(note["status"], "not_started");

    // 入力の誤り
    for b in ["  ".to_string(), "あ".repeat(1001)] {
        let (status, body) = f.member.post(&path, json!({ "body": b })).await;
        assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "body").is_empty(), "{body}");
    }

    // 消せるのは書いた本人だけ。消したものは本文を返さない
    let first = items(&body)[0].clone();
    let cid = first["id"].as_i64().unwrap();
    assert_eq!(f.member.delete(&format!("{path}/{cid}")).await.0, StatusCode::FORBIDDEN, "他人のコメントの削除");
    let (status, body) = f.manager1.delete(&format!("{path}/{cid}")).await;
    assert_eq!(status, StatusCode::OK, "削除: {body}");
    let c = &items(&body)[0];
    assert!(c["deleted"] == json!(true) && c["body"] == "" && c["can_delete"] == json!(false), "消したコメント = {c}");

    // 件数（消したものを除く）: ホーム
    let home = f.admin.must_get(&format!("/api/scenarios/{}/activity-status?scope=all", f.budget)).await;
    let prj = items(&home).iter().find(|m| m["code"] == "PRJ-1").unwrap();
    assert_eq!(prj["comment_count"], json!(2));

    // ロック済みのシナリオには書けない・消せない（読める）
    let (status, body) = f.admin.post(&format!("/api/scenarios/{}/lock", f.budget), json!({"reason": "確定"})).await;
    assert_eq!(status, StatusCode::OK, "lock: {body}");
    assert_eq!(f.member.post(&path, json!({"body": "追記"})).await.0, StatusCode::CONFLICT);
    let list = f.member.must_get(&path).await;
    let second_id = items(&list)[1]["id"].as_i64().unwrap();
    assert_eq!(f.member.delete(&format!("{path}/{second_id}")).await.0, StatusCode::CONFLICT);
    assert!(list["can_post"] == json!(false) && items(&list).len() == 3, "ロック後の一覧 = {list}");
}

#[tokio::test]
async fn plan_values_csv() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let a = &f.admin;
    let import_path = format!("/api/scenarios/{}/plan-values/import", f.budget);
    let amounts_path = format!("/api/scenarios/{}/amounts/export", f.budget);
    let drivers_path = format!("/api/scenarios/{}/driver-values/export", f.budget);
    a.must_create(
        &format!("/api/activities/{}/lines", f.manual_act),
        json!({"subject_id": f.sales, "name": "基本", "reason": "r"}),
    )
    .await;
    a.must_create("/api/activities", activity_body(f.unit1, "OTHER-1", json!({}))).await; // 担当者なし（member は編集できない）

    // 確認（dry run）は保存しない
    let csv = "activity_code,subject_code,line_name,2026-04,2026-05\nPRJ-1,4110,,1000,2000\nPRJ-1,4110,基本,500,\nPRJ-1,8110,,300,0\n";
    let (status, body) = a.upload(&format!("{import_path}?dry_run=true"), csv, "").await;
    assert!(
        status == StatusCode::OK
            && counts(&body) == "inserted=5 updated=0 deleted=0 unchanged=0"
            && body["kind"] == "amounts",
        "確認: {body}"
    );
    assert!(
        body["activities"].as_array().unwrap().len() == 1 && body["activities"][0]["changed"] == json!(5),
        "{body}"
    );
    assert_eq!(a.upload(&import_path, csv, "").await.0, StatusCode::UNPROCESSABLE_ENTITY, "理由なし");
    let (status, body) = a.upload(&import_path, csv, "期初計画の入力").await;
    assert!(status == StatusCode::OK && counts(&body) == "inserted=5 updated=0 deleted=0 unchanged=0", "取込: {body}");

    // 出力（月を横に並べる）と、出力した CSV の取込は「変更なし」
    let exported = a.download(&amounts_path).await;
    for want in [
        "activity_code,activity_name,subject_code,subject_name,line_name,line_type,2026-04,2026-05,2026-06",
        "PRJ-1,施策 PRJ-1,4110,受託売上,,manual,1000,2000,,",
        "PRJ-1,施策 PRJ-1,4110,受託売上,基本,manual,500,,",
        "PRJ-1,施策 PRJ-1,8110,外注費,,manual,300,0,",
        "SAAS-1,施策 SAAS-1,4110,受託売上,利用料,formula,,",
    ] {
        assert!(exported.contains(want), "金額の出力に {want:?} がない:\n{exported}");
    }
    let (status, body) = a.upload(&import_path, &exported, "再取込").await;
    assert!(
        status == StatusCode::OK
            && counts(&body) == "inserted=0 updated=0 deleted=0 unchanged=5"
            && body["warnings"] == json!([]),
        "出力した CSV の取込: {body}"
    );

    // ドライバー値の取込で、計算式の金額を計算し直す
    let csv = "activity_code,driver_code,2026-04\nSAAS-1,unit_price,100\nSAAS-1,volume,10.5\n";
    let (status, body) = a.upload(&import_path, csv, "ドライバー").await;
    assert!(
        status == StatusCode::OK
            && body["kind"] == "driver_values"
            && counts(&body) == "inserted=2 updated=0 deleted=0 unchanged=0",
        "ドライバー値: {body}"
    );
    let exported = a.download(&amounts_path).await;
    assert!(exported.contains("SAAS-1,施策 SAAS-1,4110,受託売上,利用料,formula,525,"), "再計算の結果:\n{exported}");
    let drivers = a.download(&drivers_path).await;
    assert!(drivers.contains("SAAS-1,施策 SAAS-1,volume,件数,,10.5,"), "ドライバー値の出力:\n{drivers}");
    let (status, body) = a.upload(&import_path, &drivers, "再取込").await;
    assert!(status == StatusCode::OK && counts(&body) == "inserted=0 updated=0 deleted=0 unchanged=2", "{body}");

    // 「-」で消す。計算式の内訳の値は取り込まず、注意を返す
    let csv = "activity_code,subject_code,line_name,2026-04,2026-05\nPRJ-1,4110,,1500,-\nSAAS-1,4110,利用料,999,\n";
    let (status, body) = a.upload(&import_path, csv, "見直し").await;
    assert!(
        status == StatusCode::OK && counts(&body) == "inserted=0 updated=1 deleted=1 unchanged=0",
        "更新・削除: {body}"
    );
    assert_eq!(body["warnings"], json!([{"kind": "formula_line", "count": 1}]));
    assert!(a.download(&amounts_path).await.contains("PRJ-1,施策 PRJ-1,4110,受託売上,,manual,1500,,"));

    // エラーが1件でもあれば何も取り込まない
    let csv = "activity_code,subject_code,line_name,2026-04\nPRJ-1,4110,,1\nNONE,4110,,1\nPRJ-1,4110,なし,1\nPRJ-1,8110,,1.5\nPRJ-1,4110,,2\n";
    let (status, body) = a.upload(&import_path, csv, "誤り").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let msgs = row_errors(&body).join("\n");
    for want in [
        "3:施策コード「NONE」",
        "4:施策「PRJ-1」の科目「4110」に内訳「なし」",
        "5:2026-04 列: 金額は円単位の整数",
        "6:2 行目と同じ",
    ] {
        assert!(msgs.contains(want), "エラーに {want:?} がない:\n{msgs}");
    }
    let (status, _) =
        a.upload(&import_path, "activity_code,subject_code,line_name,2025-04\nPRJ-1,4110,,1\n", "年度違い").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "年度の違う月の列");

    // 現場担当: 権限のない施策の行は、値が変わるときだけエラー
    let m = &f.member;
    a.upload(&import_path, "activity_code,subject_code,line_name,2026-06\nOTHER-1,4110,,5\n", "他の施策").await;
    let csv = "activity_code,subject_code,line_name,2026-06\nPRJ-1,4110,,700\nOTHER-1,4110,,1\n";
    let (status, body) = m.upload(&import_path, csv, "担当分").await;
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY
            && row_errors(&body).join("").contains("3:施策「OTHER-1」の数値を入力する権限がありません"),
        "権限のない施策: {body}"
    );
    let all = a.download(&amounts_path).await;
    let (status, body) = m.upload(&import_path, &all, "全施策の再取込").await;
    assert!(status == StatusCode::OK && body["inserted"] == json!(0), "変更のない全施策の CSV: {body}");
    let (status, body) =
        m.upload(&import_path, "activity_code,subject_code,line_name,2026-06\nPRJ-1,4110,,700\n", "担当分").await;
    assert!(status == StatusCode::OK && body["inserted"] == json!(1), "担当の施策: {body}");

    // 実績の月の値は取り込まず、注意を返す
    a.put(
        &format!("/api/scenarios/{}", f.budget),
        json!({"name": "2026年度 当初予算", "plan_role": "initial", "actual_through": "2026-04", "reason": "4月確定"}),
    )
    .await;
    let (status, body) = a
        .upload(&import_path, "activity_code,subject_code,line_name,2026-04,2026-05\nPRJ-1,4110,,1,2\n", "実績の月")
        .await;
    assert!(
        status == StatusCode::OK
            && counts(&body) == "inserted=1 updated=0 deleted=0 unchanged=0"
            && body["warnings"] == json!([{"kind": "actual_month", "count": 1}]),
        "実績の月: {body}"
    );

    // 作成中ではないシナリオは FP&A のみ。ロック済みには取り込めない
    let other = a.must_create("/api/scenarios", json!({"name": "比較用", "fiscal_year": 2026})).await;
    let other_path = format!("/api/scenarios/{other}/plan-values/import");
    let one = "activity_code,subject_code,line_name,2026-06\nPRJ-1,4110,,1\n";
    assert_eq!(m.upload(&other_path, one, "r").await.0, StatusCode::CONFLICT, "作成中ではないシナリオ（現場）");
    a.post(&format!("/api/scenarios/{other}/lock"), json!({"reason": "確定"})).await;
    assert_eq!(a.upload(&other_path, one, "r").await.0, StatusCode::CONFLICT, "ロック済み");
}

#[tokio::test]
async fn update_targets() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let a = &f.admin;
    let set_status = async |id: i64, code: &str, status: &str| {
        let act = a.must_get(&format!("/api/activities/{id}")).await;
        let body = activity_body(
            f.unit1,
            code,
            json!({"owner_user_id": f.member_id, "status": status, "name": act["name"], "confidence_level": act["confidence_level"], "reason": "r"}),
        );
        let (st, res) = a.put(&format!("/api/activities/{id}"), body).await;
        assert_eq!(st, StatusCode::OK, "status: {res}");
    };
    let codes = async || -> String {
        let body = a.must_get(&format!("/api/scenarios/{}/activity-status?scope=all", f.budget)).await;
        items(&body).iter().map(|it| it["code"].as_str().unwrap().to_string()).collect::<Vec<_>>().join(",")
    };
    // PRJ-1 は中止で金額なし（対象外）、SAAS-1 は完了だが 10月に金額が残っている（対象）、DONE-1 は完了で金額なし（対象外）
    set_status(f.manual_act, "PRJ-1", "cancelled").await;
    set_status(f.formula_act, "SAAS-1", "completed").await;
    let (status, body) = a
        .put(
            &format!("{}/amounts", f.values_path(f.budget, f.formula_act)),
            json!({"reason": "r", "amounts": [{"subject_id": f.cost, "target_month": "2026-10", "amount": 100}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    a.must_create(
        "/api/activities",
        activity_body(f.unit1, "DONE-1", json!({"owner_user_id": f.member_id, "status": "completed"})),
    )
    .await;
    a.must_create(
        "/api/activities",
        activity_body(f.unit1, "NEW-1", json!({"owner_user_id": f.member_id, "status": "planned"})),
    )
    .await;
    assert_eq!(codes().await, "NEW-1,SAAS-1");

    // 切り替えの注意も同じ対象（未完了 2 件）
    let (_, body) = a
        .post("/api/scenarios/start-monthly", json!({"name": "5月見込", "actual_through": "2026-05", "dry_run": true}))
        .await;
    assert!(body["warnings"].to_string().contains("完了にしていない施策が 2 件"), "{body}");
}
