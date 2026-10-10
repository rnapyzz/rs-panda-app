//! 実績の取込・割当・締めの統合テスト。Go 版の actuals_test.go・entries_test.go・corrections_test.go と、
//! external_code_test.go・visibility_test.go のうち実績の部分。
//! 予実比較（レポート）で未割当を確かめる部分は、レポートを移す段階で足す。

mod common;

use common::*;
use reqwest::StatusCode;
use serde_json::{Value, json};

const ACTUALS: &str = "/api/actuals/import";

fn allocation(body: &Value) -> String {
    let a = &body["allocation"];
    format!(
        "activity_code={} external_code={} rule={} unallocated={}",
        a["activity_code"], a["external_code"], a["rule"], a["unallocated"]
    )
}

fn row_message(body: &Value, i: usize) -> &str {
    body["error"]["rows"][i]["message"].as_str().unwrap_or("")
}

#[tokio::test]
async fn import_actuals() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let csv = "target_month,account_code,department_code,box_code,amount,description\n\
               2026-09,4110,D100,PRJ-1,1200000,A社 受託\n\
               2026-09,8110,D100,PRJ-1,-350000,外注 戻し\n\
               2026-09,8110,D100,PRJ-1,-50000,\n\
               2026-10,4110,,SAAS-1,300000,\n";

    // dry_run は保存せずに件数を返す（理由は不要）
    let (status, body) = f.admin.upload(&format!("{ACTUALS}?dry_run=true"), csv, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["dry_run"], json!(true));
    assert_eq!(counts(&body), "inserted=3 updated=0 deleted=0 unchanged=0");
    assert_eq!(f.actual_amount(f.manual_act, f.sales, "2026-09").await, "", "dry_run なのに保存された");

    // 取込（同じ施策・科目は合算 → -400000）
    let (status, body) = f.admin.upload(ACTUALS, csv, "2026-09・10 実績取込").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(counts(&body), "inserted=3 updated=0 deleted=0 unchanged=0");
    assert_eq!((body["rows"].clone(), body["facts"].clone()), (json!(4), json!(3)));
    assert_eq!(allocation(&body), "activity_code=4 external_code=0 rule=0 unallocated=0");
    let sep = &body["totals"][0];
    assert_eq!(
        (&sep["month"], &sep["revenue"], &sep["expense"], &sep["unallocated_revenue"]),
        (&json!("2026-09"), &json!("1200000"), &json!("-400000"), &json!("0")),
        "2026-09 の合計"
    );
    assert_eq!(f.actual_amount(f.manual_act, f.cost, "2026-09").await, "-400000");
    let months = f.viewer.must_get("/api/actuals/months?fiscal_year=2026").await;
    assert_eq!(months["months"], json!(["2026-09", "2026-10"]));

    // 同じファイルの再取込では合計は何も変わらない（監査ログも増えない）
    let (status, body) = f.admin.upload(ACTUALS, csv, "再取込").await;
    assert_eq!((status, counts(&body)), (StatusCode::OK, "inserted=0 updated=0 deleted=0 unchanged=3".into()));

    // 2026-09 だけを取り込み直すと、2026-09 は CSV の内容に置き換わり、2026-10 はそのまま。列は省略できる
    let (status, body) = f
        .admin
        .upload(ACTUALS, "target_month,account_code,box_code,amount\n2026-09,4110,PRJ-1,1250000\n", "2026-09 実績修正")
        .await;
    assert_eq!((status, counts(&body)), (StatusCode::OK, "inserted=0 updated=1 deleted=1 unchanged=0".into()));
    assert_eq!(f.actual_amount(f.manual_act, f.cost, "2026-09").await, "", "CSV にない 2026-09 の外注費");
    assert_eq!(f.actual_amount(f.formula_act, f.sales, "2026-10").await, "300000");
    let n: i64 = f.env.query_one("SELECT COUNT(*) FROM actual_entries WHERE target_month = '2026-09-01'", ()).await;
    assert_eq!(n, 1);

    // 取込は変更セット（理由付き、シナリオなし）と監査ログ（合計の変更）に残る
    let n: i64 = f
        .env
        .query_one(
            "SELECT COUNT(*) FROM audit_logs a JOIN change_sets c ON c.id = a.change_set_id
             WHERE c.scenario_id IS NULL AND c.reason = '2026-09 実績修正' AND a.table_name = 'actual_facts'",
            (),
        )
        .await;
    assert_eq!(n, 2, "監査ログ（更新1・削除1）");
}

#[tokio::test]
async fn import_actuals_rejects_invalid_data() {
    let Some(f) = ScenarioFixture::new().await else { return };

    // 1行でもエラーがあれば全件取り込まない
    let csv = "target_month,account_code,box_code,amount\n2026-09,4110,PRJ-1,1200000\n2026-13,4110,PRJ-1,1\n";
    let (status, body) = f.admin.upload(ACTUALS, csv, "取込").await;
    assert_eq!((status, error_code(&body)), (StatusCode::UNPROCESSABLE_ENTITY, "invalid_csv"), "{body}");
    assert_eq!(f.actual_amount(f.manual_act, f.sales, "2026-09").await, "");

    // 未登録の会計科目はコードごとにまとめて返す。登録されていない箱の ID はエラーにしない
    let csv = "target_month,account_code,box_code,amount\n2026-09,9999,NOPE,1\n2026-09,9999,,2\n2026-09,4110,NOPE,3\n";
    let (status, body) = f.admin.upload(ACTUALS, csv, "取込").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(row_errors(&body).len(), 1, "{body}");
    assert!(row_message(&body, 0).contains("9999") && row_message(&body, 0).contains("2 行"), "{body}");

    let valid = "target_month,account_code,amount\n2026-09,4110,1\n";
    let (status, body) = f.admin.upload(ACTUALS, valid, "").await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "reason").is_empty(), "理由なし: {body}");
    let (status, body) = f.admin.upload(ACTUALS, "", "取込").await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "file").is_empty(), "ファイルなし: {body}");
    let (status, _) = f.manager1.upload(ACTUALS, valid, "取込").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "マネージャーの取込");
}

/// docs/plan.md「2.12 実績の割当」の流れ。
#[tokio::test]
async fn actual_allocation() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let a = &f.admin;

    // 受け皿の施策と、会計科目（減価償却費・給与・対象外の預金）
    let pool =
        a.must_create("/api/activities", activity_body(f.unit1, "POOL-1", json!({"activity_type": "cost_pool"}))).await;
    let depreciation =
        a.must_create("/api/gl-accounts", json!({"code": "6210", "name": "減価償却費", "subject_id": f.cost})).await;
    let salary = a
        .must_create(
            "/api/gl-accounts",
            json!({"code": "6110", "name": "給与", "subject_id": f.cost, "hide_details": true}),
        )
        .await;
    a.must_create("/api/gl-accounts", json!({"code": "1110", "name": "普通預金", "is_excluded": true})).await;
    let (status, body) = a.post("/api/gl-accounts", json!({"code": "1120", "name": "当座預金"})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "subject_id").is_empty(), "{body}");
    // 割当ルール: 給与は全部門で受け皿へ。減価償却費は D200 だけ SAAS-1 へ
    a.must_create("/api/allocation-rules", json!({"gl_account_id": salary, "activity_id": pool})).await;
    a.must_create(
        "/api/allocation-rules",
        json!({"gl_account_id": depreciation, "department_code": "D200", "activity_id": f.formula_act}),
    )
    .await;
    let (status, body) = a
        .post(
            "/api/allocation-rules",
            json!({"gl_account_id": depreciation, "department_code": "D200", "activity_id": pool}),
        )
        .await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "department_code").is_empty(), "{body}");

    let csv = "target_month,account_code,department_code,box_code,amount,description\n\
               2026-04,4110,D100,PRJ-1,1000,施策コード\n\
               2026-04,4110,D100,P-9001,500,未登録の箱\n\
               2026-04,6110,D100,,300,山田 給与\n\
               2026-04,6210,D200,,200,サーバー\n\
               2026-04,6210,D300,,70,PC\n\
               2026-05,6210,D300,,30,PC\n\
               2026-04,1110,D100,,99999,預金\n";
    let (status, body) = a.upload(ACTUALS, csv, "4・5月実績").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(allocation(&body), "activity_code=1 external_code=0 rule=2 unallocated=3");
    assert_eq!(body["excluded"], json!(1));
    let apr = &body["totals"][0];
    assert_eq!(
        [&apr["revenue"], &apr["expense"], &apr["unallocated_revenue"], &apr["unallocated_expense"]],
        [&json!("1500"), &json!("570"), &json!("500"), &json!("70")],
        "4月の合計（未割当を含む）"
    );
    assert_eq!(f.actual_amount(0, f.sales, "2026-04").await, "500", "未割当の売上");
    assert_eq!(f.actual_amount(pool, f.cost, "2026-04").await, "300", "受け皿の給与");

    // 未割当の一覧: 箱の ID ごと・会計科目 × 部門ごと
    let list = a.must_get("/api/actuals/unallocated?fiscal_year=2026").await;
    assert_eq!((list["count"].clone(), items(&list).len()), (json!(3), 2), "{list}");
    // 金額の大きいまとまりから（箱の ID 500 → 減価償却費 × D300 の 100）
    assert_eq!(items(&list)[0]["key"], json!("box:P-9001"));
    assert_eq!(items(&list)[1]["months"], json!(["2026-04", "2026-05"]));
    let (status, _) = f.manager1.get("/api/actuals/unallocated").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // 箱の ID を施策に割り当てると、外部コードとして登録される
    let assign = "/api/actuals/unallocated/assign";
    let (status, body) =
        a.post(assign, json!({"box_code": "P-9001", "activity_id": f.manual_act, "reason": "新規案件"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!((body["assigned"].clone(), body["external_code"].clone()), (json!(1), json!("P-9001")));
    assert!(body.get("rule").is_none(), "{body}");
    assert_eq!(f.actual_amount(f.manual_act, f.sales, "2026-04").await, "1500");
    assert_eq!(f.actual_amount(0, f.sales, "2026-04").await, "");
    let act = a.must_get(&format!("/api/activities/{}", f.manual_act)).await;
    assert_eq!(act["external_codes"].as_array().unwrap().len(), 1);
    assert_eq!(act["external_codes"][0]["code"], json!("P-9001"));
    // 同じ箱の ID はもう登録できない
    let (status, _) = a.post(assign, json!({"box_code": "P-9001", "activity_id": pool, "reason": "再"})).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // 会計科目 × 部門を割り当てると、割当ルールが追加され、すべての月の未割当が割り当たる
    let (status, body) = a
        .post(
            assign,
            json!({"gl_account_id": depreciation, "department_code": "D300", "activity_id": pool, "reason": "PC は共通費"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!((body["assigned"].clone(), body["months"].clone()), (json!(2), json!(["2026-04", "2026-05"])));
    assert_eq!(body["rule"]["department_code"], json!("D300"));
    assert_eq!(f.actual_amount(pool, f.cost, "2026-05").await, "30");
    assert_eq!(a.must_get("/api/actuals/unallocated").await["count"], json!(0));
    // 同じルールはもう追加できない
    let (status, _) = a
        .post(
            assign,
            json!({"gl_account_id": depreciation, "department_code": "D300", "activity_id": pool, "reason": "x"}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // 取り込み直しても同じ施策に入る（ルールとして残っている）
    let (status, body) = a.upload(ACTUALS, csv, "4・5月実績 再取込").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(allocation(&body), "activity_code=1 external_code=1 rule=4 unallocated=0");

    // ルールを変えても取込済みの明細は変わらない。再割当で当て直す（dry run で増減を確認できる）
    let rules = a.must_get("/api/allocation-rules").await;
    let salary_rule = items(&rules).iter().find(|r| r["gl_account_code"] == "6110").unwrap()["id"].as_i64().unwrap();
    // 会計科目の順。同じ会計科目では全部門のルールが先
    let depts: Vec<Value> = items(&rules).iter().map(|r| r["department_code"].clone()).collect();
    assert_eq!(depts, [json!(null), json!("D200"), json!("D300")]);
    let (status, _) = a
        .put(
            &format!("/api/allocation-rules/{salary_rule}"),
            json!({"gl_account_id": salary, "activity_id": f.formula_act, "reason": "付け替え"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(f.actual_amount(pool, f.cost, "2026-04").await, "370", "ルール変更後（再割当前）の受け皿");
    let (status, body) = a.post("/api/actuals/reallocate", json!({"months": ["2026-04"], "dry_run": true})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], json!(1));
    // 施策コードの順（POOL-1 → SAAS-1）
    let changes = body["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 2, "{body}");
    assert_eq!(
        (
            &changes[0]["activity"]["code"],
            &changes[0]["expense"],
            &changes[1]["activity"]["code"],
            &changes[1]["expense"]
        ),
        (&json!("POOL-1"), &json!("-300"), &json!("SAAS-1"), &json!("300"))
    );
    assert_eq!(f.actual_amount(pool, f.cost, "2026-04").await, "370", "dry run なのに変わった");
    let (status, body) = a.post("/api/actuals/reallocate", json!({"months": ["2026-04"]})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "reason").is_empty(), "{body}");
    let (status, body) = a.post("/api/actuals/reallocate", json!({"months": ["2026-06"], "reason": "x"})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "months").is_empty(), "{body}");
    let (status, body) =
        a.post("/api/actuals/reallocate", json!({"months": ["2026-04"], "reason": "給与の付け替え"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(f.actual_amount(pool, f.cost, "2026-04").await, "70", "再割当後の受け皿");

    // 施策の明細: 給与（hide_details）は FP&A 以外には合計だけ
    let path = format!("/api/activities/{}/actual-entries?month=2026-04&subject_id={}", f.formula_act, f.cost);
    let view = f.member.must_get(&path).await;
    assert_eq!(items(&view).len(), 1, "{view}");
    assert_eq!((view["entries_total"].clone(), view["fact_total"].clone()), (json!("500"), json!("500")));
    let hidden = view["hidden"].as_array().unwrap();
    assert_eq!(hidden.len(), 1, "{view}");
    assert_eq!(
        (&hidden[0]["gl_account_code"], &hidden[0]["amount"], &hidden[0]["count"]),
        (&json!("6110"), &json!("300"), &json!(1))
    );
    assert!(!view.to_string().contains("山田"), "給与の摘要が見えている: {view}");
    let view = a.must_get(&path).await;
    assert_eq!((items(&view).len(), view["hidden"].as_array().unwrap().len()), (2, 0), "{view}");

    // ロックすると未割当も含めて実績を固定する
    a.post(assign, json!({"gl_account_id": depreciation, "activity_id": f.manual_act, "reason": "x"})).await; // 全部門のルールだけ追加
    a.upload(ACTUALS, "target_month,account_code,box_code,amount\n2026-05,4110,UNKNOWN,40\n", "5月 再取込").await;
    assert_eq!(f.actual_amount(0, f.sales, "2026-05").await, "40", "5月の未割当");
    a.put(
        &format!("/api/scenarios/{}", f.budget),
        json!({"name": "2026年度 当初予算", "plan_role": "initial", "actual_through": "2026-05", "reason": "5月確定"}),
    )
    .await;
    let (status, body) = a.post(&format!("/api/scenarios/{}/lock", f.budget), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let frozen: String = f
        .env
        .query_one("SELECT CAST(SUM(amount) AS CHAR) FROM scenario_unallocated WHERE scenario_id = ?", (f.budget,))
        .await;
    assert_eq!(frozen, "40", "ロックしたシナリオの未割当");
}

#[tokio::test]
async fn gl_accounts_and_rules_csv() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let a = &f.admin;
    let (status, body) = a
        .upload(
            "/api/gl-accounts/import",
            "code,name,subject_code,hide_details\n4110,受託売上（改）,4110,false\n6110,給与,8110,true\n1110,普通預金,,\n",
            "会計科目",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(result_counts(&body), "inserted=2 updated=1 unchanged=0");
    let exported = a.download("/api/gl-accounts/export").await;
    assert!(exported.contains("1110,普通預金,,false") && exported.contains("6110,給与,8110,true"), "{exported}");
    let accounts = a.must_get("/api/gl-accounts").await;
    let deposit = items(&accounts).iter().find(|x| x["code"] == "1110").unwrap();
    assert_eq!((&deposit["is_excluded"], &deposit["subject_id"]), (&json!(true), &json!(null)));

    let (status, body) = a
        .upload(
            "/api/allocation-rules/import",
            "account_code,department_code,activity_code\n6110,,PRJ-1\n6110,D1,SAAS-1\n",
            "ルール",
        )
        .await;
    assert_eq!((status, body["inserted"].clone()), (StatusCode::OK, json!(2)), "{body}");
    let (status, body) = a
        .upload(
            "/api/allocation-rules/import",
            "account_code,department_code,activity_code\n6110,,SAAS-1\n6110,D1,SAAS-1\n",
            "ルール",
        )
        .await;
    assert_eq!((status, result_counts(&body)), (StatusCode::OK, "inserted=0 updated=1 unchanged=1".into()));
    let got = a.download("/api/allocation-rules/export").await;
    assert!(got.contains("6110,,SAAS-1") && got.contains("6110,D1,SAAS-1"), "{got}");
    let (status, body) = a
        .upload("/api/allocation-rules/import", "account_code,department_code,activity_code\n9999,,NOPE\n", "ルール")
        .await;
    assert_eq!((status, row_errors(&body).len()), (StatusCode::UNPROCESSABLE_ENTITY, 2), "{body}");

    // 参照されている会計科目は削除できない。参照のない会計科目は削除できる
    let account = items(&accounts).iter().find(|x| x["code"] == "6110").unwrap()["id"].as_i64().unwrap();
    let (status, _) = a.do_req("DELETE", &format!("/api/gl-accounts/{account}"), Some(json!({"reason": "x"}))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let deposit = deposit["id"].as_i64().unwrap();
    let (status, _) = a.do_req("DELETE", &format!("/api/gl-accounts/{deposit}"), Some(json!({"reason": "x"}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = f.manager1.post("/api/gl-accounts", json!({"code": "9", "name": "x", "is_excluded": true})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// 施策の実績の明細（すべての月・100件ずつ・エクスポート）。
#[tokio::test]
async fn actual_entries_all() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let a = &f.admin;
    a.must_create(
        "/api/gl-accounts",
        json!({"code": "6110", "name": "給与", "subject_id": f.cost, "hide_details": true}),
    )
    .await;
    let mut csv = String::from("target_month,account_code,box_code,amount,description\n");
    for i in 0..150 {
        csv.push_str(&format!("2026-04,4110,PRJ-1,1,4月の請求 {}\n", i + 1));
    }
    csv.push_str("2026-05,4110,PRJ-1,100,5月の請求\n2026-06,6110,PRJ-1,300,山田 給与\n");
    let (status, body) = a.upload(ACTUALS, &csv, "実績取込").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let path = format!("/api/activities/{}/actual-entries", f.manual_act);

    // 既定はすべての月。新しい月から 100 件。合計は全件。給与は担当者には合計だけ
    let v = f.member.must_get(&path).await;
    assert_eq!(v["months"], json!(["2026-04", "2026-05", "2026-06"]));
    assert_eq!((v["total"].clone(), items(&v).len(), v["has_more"].clone()), (json!(151), 100, json!(true)));
    assert_eq!((v["entries_total"].clone(), v["fact_total"].clone()), (json!("550"), json!("550")));
    assert_eq!(items(&v)[0]["target_month"], json!("2026-05"), "新しい月から。給与の明細は出さない");
    assert_eq!(v["hidden"][0]["amount"], json!("300"));
    let v = f.member.must_get(&format!("{path}?offset=100")).await;
    assert_eq!((items(&v).len(), v["has_more"].clone()), (51, json!(false)));
    // 絞り込み
    let v = f.member.must_get(&format!("{path}?month=2026-05")).await;
    assert_eq!((v["total"].clone(), v["entries_total"].clone()), (json!(1), json!("100")));
    let v = f.member.must_get(&format!("{path}?fiscal_year=2027")).await;
    assert_eq!((v["total"].clone(), v["entries_total"].clone()), (json!(0), json!("0")));
    let v = a.must_get(&format!("{path}?subject_id={}", f.cost)).await;
    assert_eq!((v["total"].clone(), v["hidden"].as_array().unwrap().len()), (json!(1), 0));
    for q in ["?month=2026", "?fiscal_year=abc", "?limit=1000", "?offset=-1", "?subject_id=x"] {
        let (status, _) = f.member.get(&format!("{path}{q}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{q}");
    }
    let (status, _) = f.member.get("/api/activities/999999/actual-entries").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // エクスポート: 絞り込みどおりの全件。給与は担当者には月 × 会計科目の合計の行
    let csv = f.member.download(&format!("{path}/export")).await;
    let lines: Vec<&str> = csv.trim().split("\r\n").collect();
    assert_eq!(lines.len(), 1 + 151 + 1);
    assert!(lines[0].starts_with("target_month,subject_code,"));
    assert!(
        lines[1].starts_with("2026-06,8110,外注費,6110,給与,,,明細は FP&A のみ（1 行の合計）,300,"),
        "{}",
        lines[1]
    );
    assert!(!csv.contains("山田"));
    assert!(lines[2].contains("2026-05,4110,受託売上,4110,受託売上,,PRJ-1,5月の請求,100,施策コード"), "{}", lines[2]);
    let csv = a.download(&format!("{path}/export?month=2026-06")).await;
    assert!(csv.contains("山田 給与") && csv.trim().split("\r\n").count() == 2, "{csv}");
}

/// docs/plan.md「2.14 締めた後の実績の修正と年度の締め」。
#[tokio::test]
async fn actual_corrections() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let a = &f.admin;
    let header = "target_month,box_code,account_code,amount\n";
    let (status, body) =
        a.upload(ACTUALS, &format!("{header}2026-04,PRJ-1,4110,1000\n2026-05,PRJ-1,4110,500\n"), "4・5月実績").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // 5月まで確定した版をロックする
    let path = format!("/api/scenarios/{}", f.budget);
    a.put(
        &path,
        json!({"name": "2026年度 当初予算", "plan_role": "initial", "actual_through": "2026-05", "reason": "5月確定"}),
    )
    .await;
    let (status, body) = a.post(&format!("{path}/lock"), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(items(&a.must_get("/api/scenarios/actual-drift").await).is_empty(), "ロック直後の食い違い");

    // 4月の実績を修正して取り込むと、確認の結果にロック済みのシナリオとの食い違いが出る（取り込みは止めない）
    let fix = format!("{header}2026-04,PRJ-1,4110,1200\n2026-04,PRJ-1,8110,30\n");
    let (status, body) = a.upload(&format!("{ACTUALS}?dry_run=true"), &fix, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let drift = body["locked_drift"].as_array().unwrap();
    assert_eq!(drift.len(), 1, "{body}");
    assert_eq!(drift[0]["months"][0], json!({"month": "2026-04", "revenue": "200", "expense": "30", "changed": 2}));
    a.upload(ACTUALS, &fix, "4月実績の修正").await;

    // ロック済みのシナリオは固定のまま。一覧に食い違いが出る
    let vpath = f.values_path(f.budget, f.manual_act);
    assert_eq!(amount_of(&a.must_get(&vpath).await, f.sales, "2026-04").0, "1000", "ロック済みの4月");
    let list = a.must_get("/api/scenarios/actual-drift").await;
    assert_eq!(items(&list).len(), 1, "{list}");
    assert_eq!(items(&list)[0]["scenario_id"], json!(f.budget));
    let (status, _) = f.manager1.get("/api/scenarios/actual-drift").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // 実績を最新にする: dry run で差を確認し、理由付きで入れ替える。ロックは外れない
    let refresh = format!("{path}/refresh-actuals");
    let (status, body) = a.post(&refresh, json!({"dry_run": true})).await;
    assert!(status == StatusCode::OK && body["drift"]["months"].is_array(), "{body}");
    let (status, body) = a.post(&refresh, json!({})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "reason").is_empty(), "{body}");
    let (status, body) = a.post(&refresh, json!({"reason": "4月の決算整理を反映"})).await;
    assert_eq!((status, body["saved"].clone()), (StatusCode::OK, json!(3)), "{body}");
    let view = a.must_get(&vpath).await;
    assert_eq!(amount_of(&view, f.sales, "2026-04").0, "1200");
    assert_eq!(view["scenario"]["is_locked"], json!(true));
    assert!(items(&a.must_get("/api/scenarios/actual-drift").await).is_empty());
    let (status, _) = a.post(&refresh, json!({"reason": "再"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "食い違いがないのに最新にした");
    // 監査ログには入れ替えた件数と月ごとの差が残る
    let after: String = f
        .env
        .query_one(
            "SELECT CAST(after_json AS CHAR) FROM audit_logs WHERE table_name = 'scenarios' AND record_id = ? AND before_json IS NULL ORDER BY id DESC LIMIT 1",
            (f.budget,),
        )
        .await;
    let after: Value = serde_json::from_str(&after).unwrap();
    assert_eq!((&after["refreshed_actuals"], &after["actual_drift"][0]["revenue"]), (&json!(3), &json!("200")));

    // 年度を締めると、その年度の月は取り込めず、再割当もできない
    let (status, body) = a.post("/api/fiscal-years/2026/close", json!({"reason": "2026年度決算確定"})).await;
    assert_eq!((status, body["fiscal_year"].clone()), (StatusCode::OK, json!(2026)), "{body}");
    let (status, _) = a.post("/api/fiscal-years/2026/close", Value::Null).await;
    assert_eq!(status, StatusCode::CONFLICT, "二重の締め");
    let (status, _) = a.post("/api/fiscal-years/26/close", Value::Null).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = a
        .upload(
            ACTUALS,
            &format!("{header}2026-04,PRJ-1,4110,9\n2026-04,PRJ-1,4110,9\n2027-04,PRJ-1,4110,9\n"),
            "締めた年度",
        )
        .await;
    assert_eq!((status, row_errors(&body).len()), (StatusCode::UNPROCESSABLE_ENTITY, 1), "{body}");
    assert!(row_message(&body, 0).contains("2026年度") && row_message(&body, 0).contains("2 行"), "{body}");
    let (status, body) = a.post("/api/actuals/reallocate", json!({"months": ["2026-04"], "reason": "x"})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "months").is_empty(), "{body}");
    let closings = f.viewer.must_get("/api/fiscal-years/closings").await;
    assert_eq!(items(&closings).len(), 1);
    assert_eq!(items(&closings)[0]["closed_by_name"], json!("テスト fpa_admin"), "{closings}");

    // 締めの解除は理由が必須。解除すれば取り込める
    let (status, body) = a.post("/api/fiscal-years/2026/reopen", json!({})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "reason").is_empty(), "{body}");
    let (status, _) = a.post("/api/fiscal-years/2026/reopen", json!({"reason": "修正の取込"})).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = a.post("/api/fiscal-years/2026/reopen", json!({"reason": "再"})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, body) = a.upload(ACTUALS, &format!("{header}2026-04,PRJ-1,4110,1300\n"), "締め解除後").await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// 未割当の割当で、締めた年度の行は割り当てずにルールだけを登録する。
#[tokio::test]
async fn assign_skips_closed_year() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let a = &f.admin;
    a.upload(ACTUALS, "target_month,box_code,account_code,amount\n2026-04,P-1,4110,100\n2027-04,P-1,4110,50\n", "取込")
        .await;
    a.post("/api/fiscal-years/2026/close", Value::Null).await;
    let (status, body) = a
        .post(
            "/api/actuals/unallocated/assign",
            json!({"box_code": "P-1", "activity_id": f.manual_act, "reason": "新規案件"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        (&body["assigned"], &body["skipped_closed"], &body["external_code"]),
        (&json!(1), &json!(1), &json!("P-1"))
    );
    assert_eq!(f.actual_amount(0, f.sales, "2026-04").await, "100", "締めた年度の未割当（そのまま）");
    assert_eq!(f.actual_amount(f.manual_act, f.sales, "2027-04").await, "50");
}

/// 実績 CSV は外部コードでも取り込め、同じ施策の行は合算される（Go 版の TestExternalCodes の実績の部分）。
#[tokio::test]
async fn import_by_external_codes() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let base = format!("/api/activities/{}/external-codes", f.manual_act);
    f.member.must_create(&base, json!({"code": "P-1001"})).await;
    f.member.must_create(&base, json!({"code": "P-1002"})).await;
    let csv = "target_month,box_code,account_code,amount\n2026-09,P-1001,4110,1000\n2026-09,P-1002,4110,250\n2026-09,PRJ-1,4110,5\n";
    let (status, body) = f.admin.upload(ACTUALS, csv, "外部コードで取込").await;
    assert_eq!((status, body["facts"].clone()), (StatusCode::OK, json!(1)), "{body}");
    assert_eq!(allocation(&body), "activity_code=1 external_code=2 rule=0 unallocated=0");
    assert_eq!(f.actual_amount(f.manual_act, f.sales, "2026-09").await, "1255");
}

/// 実績の明細: 現場には閲覧制限のある科目の明細も合計も出さない（Go 版の TestRestrictedSubjects の実績の部分）。
#[tokio::test]
async fn entries_hide_restricted_subjects() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let a = &f.admin;
    let personnel = a
        .must_create(
            "/api/subjects",
            json!({"code": "8210", "name": "人件費", "category": "expense", "is_restricted": true}),
        )
        .await;
    a.must_create("/api/gl-accounts", json!({"code": "8210", "name": "人件費", "subject_id": personnel})).await;
    a.upload(
        ACTUALS,
        "target_month,box_code,account_code,amount\n2026-04,PRJ-1,4110,700\n2026-04,PRJ-1,8210,500\n",
        "4月実績",
    )
    .await;
    let path = format!("/api/activities/{}/actual-entries?month=2026-04", f.manual_act);
    let e = a.must_get(&path).await;
    assert_eq!((e["fact_total"].clone(), e["restricted_hidden"].clone()), (json!("1200"), json!(false)));
    let e = f.member.must_get(&path).await;
    assert_eq!(
        (e["fact_total"].clone(), items(&e).len(), e["restricted_hidden"].clone()),
        (json!("700"), 1, json!(true)),
        "{e}"
    );
}
