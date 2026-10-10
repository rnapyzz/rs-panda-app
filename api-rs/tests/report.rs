//! 予実比較・リスク・報告資料の統合テスト。Go 版の report_test.go・risk_test.go・pack_test.go。

mod common;

use std::collections::HashMap;
use std::io::{Cursor, Read};

use chrono::{Duration, Local};
use common::*;
use regex::Regex;
use reqwest::StatusCode;
use serde_json::{Value, json};

const ACTUALS: &str = "/api/actuals/import";

fn amt(subject: i64, month: &str, amount: i64) -> Value {
    json!({"subject_id": subject, "target_month": month, "amount": amount})
}

async fn put_amounts(f: &ScenarioFixture, sid: i64, aid: i64, items: Vec<Value>) {
    let (status, body) =
        f.admin.put(&format!("{}/amounts", f.values_path(sid, aid)), json!({"reason": "r", "amounts": items})).await;
    assert_eq!(status, StatusCode::OK, "amounts: {body}");
}

/// comparison のレスポンスから、指定した行・系列の金額を取り出す（なければ ""）。activity_id の 0 は問わない。
fn report_value(
    body: &Value,
    unit_id: Option<i64>,
    activity_id: i64,
    subject_id: i64,
    month: &str,
    series: &str,
) -> String {
    for row in body["rows"].as_array().unwrap() {
        if row["unit_id"] != json!(unit_id) || row["subject_id"] != json!(subject_id) || row["month"] != month {
            continue;
        }
        if activity_id != 0 && row["activity_id"] != json!(activity_id) {
            continue;
        }
        return row["values"][series].as_str().unwrap_or("").to_string();
    }
    String::new()
}

#[tokio::test]
async fn comparison_report() {
    let Some(f) = ScenarioFixture::new().await else { return };
    // 予算: 4月・10月に受託売上、見込: 10月だけ増額し、決算確定月を 9月にする。実績: 4月・10月
    put_amounts(
        &f,
        f.budget,
        f.manual_act,
        vec![amt(f.sales, "2026-04", 1000), amt(f.sales, "2026-10", 2000), amt(f.cost, "2026-04", 300)],
    )
    .await;
    let forecast = f
        .admin
        .must_create("/api/scenarios", json!({"name": "10月見込", "fiscal_year": 2026, "base_scenario_id": f.budget}))
        .await;
    put_amounts(&f, forecast, f.manual_act, vec![amt(f.sales, "2026-10", 2500)]).await;
    // 同じユニットの別施策にも金額を入れ、ユニット単位では合算されることを確認する
    put_amounts(&f, f.budget, f.formula_act, vec![amt(f.cost, "2026-04", 50)]).await;
    let (status, body) = f
        .admin
        .upload(
            ACTUALS,
            "target_month,box_code,account_code,amount\n2026-04,PRJ-1,4110,900\n2026-10,PRJ-1,4110,9999\n",
            "実績取込",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = f
        .admin
        .put(
            &format!("/api/scenarios/{forecast}"),
            json!({"name": "10月見込", "actual_through": "2026-09", "reason": "9月決算確定"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let body = f
        .viewer
        .must_get(&format!("/api/reports/comparison?scenario_ids={},{forecast}&include_actual=true", f.budget))
        .await;
    let series = body["series"].as_array().unwrap();
    assert_eq!(series.len(), 3, "{body}");
    assert_eq!((&series[1]["actual_through"], &series[2]["key"]), (&json!("2026-09"), &json!("actual")));
    assert!(series[2].get("scenario_id").is_none(), "実績の系列に scenario_id がある");
    let u = Some(f.unit1);
    for (name, subject, month, s, want) in [
        ("予算 4月 売上", f.sales, "2026-04", "s1", "1000"),
        ("見込 10月 売上", f.sales, "2026-10", "s2", "2500"),
        ("ユニット単位で2施策の費用を合算", f.cost, "2026-04", "s1", "350"),
        ("見込: 決算確定月以前は実績", f.sales, "2026-04", "s2", "900"),
        ("見込: 実績の月の計画値（費用）は使わない", f.cost, "2026-04", "s2", ""),
        ("実績 10月", f.sales, "2026-10", "actual", "9999"),
    ] {
        assert_eq!(report_value(&body, u, 0, subject, month, s), want, "{name}");
    }

    // ユニットを指定すると施策ごとに集計する
    let body =
        f.viewer.must_get(&format!("/api/reports/comparison?scenario_ids={}&unit_id={}", f.budget, f.unit1)).await;
    assert_eq!(report_value(&body, u, f.manual_act, f.cost, "2026-04", "s1"), "300", "施策別の費用");
    assert_eq!(report_value(&body, u, f.formula_act, f.cost, "2026-04", "s1"), "50", "施策別の費用（別施策）");
}

#[tokio::test]
async fn comparison_report_validation() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let other = f.admin.must_create("/api/scenarios", json!({"name": "2027予算", "fiscal_year": 2027})).await;
    for (name, query) in [
        ("指定なし", String::new()),
        ("年度違い", format!("scenario_ids={},{other}", f.budget)),
        ("存在しない", "scenario_ids=99999".into()),
        ("5つ以上", format!("scenario_ids={},1,2,3,4", f.budget)),
    ] {
        let (status, body) = f.viewer.get(&format!("/api/reports/comparison?{query}")).await;
        assert!(
            status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "scenario_ids").is_empty(),
            "{name}: {body}"
        );
    }
    for query in ["scenario_ids=x", "scenario_ids=1&measure=x", "scenario_ids=1&unit_id=x"] {
        let (status, _) = f.viewer.get(&format!("/api/reports/comparison?{query}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
    }
}

fn risk_activity<'a>(body: &'a Value, code: &str) -> &'a Value {
    body["activities"].as_array().unwrap().iter().find(|a| a["code"] == code).unwrap()
}

fn pl_of(a: &Value, key: &str) -> String {
    format!("{}/{}", a[key]["revenue"].as_str().unwrap(), a[key]["expense"].as_str().unwrap())
}

/// docs/plan.md「2.8」の例（楽観・基準・悲観）。
#[tokio::test]
async fn risk_measures() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let a = &f.admin;
    let line = |act: i64, name: &'static str, level: &'static str, outlook: &'static str| async move {
        a.must_create(
            &format!("/api/activities/{act}/lines"),
            json!({"subject_id": f.sales, "name": name, "confidence_level": level, "outlook": outlook, "reason": "r"}),
        )
        .await
    };
    let item = |line_id: i64, amount: i64| json!({"subject_id": f.sales, "line_id": line_id, "target_month": "2026-10", "amount": amount});
    // フロー型（PRJ-1）: 案件 1,000（C・ベース）、追加要件 200（D・アドオン）
    let l1 = line(f.manual_act, "案件", "C", "base").await;
    let l2 = line(f.manual_act, "追加要件", "D", "addon").await;
    put_amounts(&f, f.budget, f.manual_act, vec![item(l1, 1000), item(l2, 200)]).await;
    // ストック型: ベース 500（A）、アドオン 100（C）、ダウンサイド −50（D）
    let stock =
        a.must_create("/api/activities", activity_body(f.unit1, "STOCK-1", json!({"confidence_level": "A"}))).await;
    let s1 = line(stock, "既存顧客", "A", "base").await;
    let s2 = line(stock, "新規", "C", "addon").await;
    let s3 = line(stock, "解約", "D", "downside").await;
    put_amounts(&f, f.budget, stock, vec![item(s1, 500), item(s2, 100), item(s3, -50)]).await;

    let body = a.must_get(&format!("/api/reports/risk?scenario_id={}", f.budget)).await;
    let flow = risk_activity(&body, "PRJ-1");
    let measures =
        |x: &Value| format!("{} {} {}", pl_of(x, "optimistic"), pl_of(x, "weighted"), pl_of(x, "pessimistic"));
    assert_eq!(measures(flow), "1200/0 540/0 0/0", "フロー型 楽観・基準・悲観");
    let st = risk_activity(&body, "STOCK-1");
    assert_eq!(measures(st), "600/0 540/0 450/0", "ストック型 楽観・基準・悲観");
    assert_eq!(st["revenue_by_level"], json!({"A": "500", "C": "100", "downside": "-50"}));
    assert_eq!(st["revenue_by_month"], json!({"2026-10": {"A": "500", "C": "100", "downside": "-50"}}));
    let lines = st["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 3, "{st}");
    assert_eq!((&lines[2]["outlook"], &lines[2]["amount"]), (&json!("downside"), &json!("-50")));
    // 段階の一覧（標準の確率は末尾の 0 を除いた数値）
    let levels = body["levels"].as_array().unwrap();
    let b = levels.iter().find(|l| l["code"] == "B").unwrap();
    assert_eq!((&b["rate"], &b["high"]), (&json!(0.8), &json!(true)), "{levels:?}");

    // 予実比較の measure
    let cmp = a
        .must_get(&format!("/api/reports/comparison?scenario_ids={}&unit_id={}&measure=weighted", f.budget, f.unit1))
        .await;
    let weighted: i64 = cmp["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["values"]["s1"].as_str().unwrap().parse::<i64>().unwrap())
        .sum();
    assert_eq!((weighted, cmp["measure"].clone()), (1080, json!("weighted")), "予実比較の加重見込の合計");

    // 実績の月はどの指標でも実績の金額。期間「残り」なら実績の月を除く
    a.upload(ACTUALS, "target_month,box_code,account_code,amount\n2026-04,PRJ-1,4110,300\n", "4月実績").await;
    a.put(
        &format!("/api/scenarios/{}", f.budget),
        json!({"name": "2026年度 当初予算", "plan_role": "initial", "actual_through": "2026-04", "reason": "4月確定"}),
    )
    .await;
    let body = a.must_get(&format!("/api/reports/risk?scenario_id={}", f.budget)).await;
    let flow = risk_activity(&body, "PRJ-1");
    assert_eq!((pl_of(flow, "pessimistic"), pl_of(flow, "actual")), ("300/0".into(), "300/0".into()));
    assert_eq!(flow["revenue_by_level"]["actual"], json!("300"));
    let body = a.must_get(&format!("/api/reports/risk?scenario_id={}&period=remaining", f.budget)).await;
    assert_eq!(pl_of(risk_activity(&body, "PRJ-1"), "pessimistic"), "0/0");
    assert_eq!(body["months"].as_array().unwrap().len(), 11);
}

/// docs/plan.md「2.8」の客観的なシグナル（警告）。
#[tokio::test]
async fn risk_warnings() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let a = &f.admin;
    let put = |sid: i64, month: &'static str, amount: i64| {
        put_amounts(&f, sid, f.manual_act, vec![amt(f.sales, month, amount)])
    };
    // PRJ-1 を確度 A にする（「段階に対して状況が悪い」の対象）
    let (status, body) = a
        .put(
            &format!("/api/activities/{}", f.manual_act),
            activity_body(
                f.unit1,
                "PRJ-1",
                json!({"owner_user_id": f.member_id, "confidence_level": "A", "reason": "受注"}),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // 見込の推移: 期初 1,000,000 → 9月見込 800,000 → 10月見込 500,000（2回続けて下がる）。4月の計画 400,000 に対し実績 200,000
    put(f.budget, "2026-04", 400_000).await;
    put(f.budget, "2026-10", 600_000).await;
    let sep = a
        .must_create("/api/scenarios", json!({"name": "9月見込", "fiscal_year": 2026, "base_scenario_id": f.budget}))
        .await;
    put(sep, "2026-10", 400_000).await;
    let day = |d: i64| (Local::now().date_naive() + Duration::days(d)).format("%Y-%m-%d").to_string();
    let ms = a
        .must_create(
            &format!("/api/activities/{}/milestones", f.manual_act),
            json!({"name": "検収", "due_date": day(10)}),
        )
        .await;
    let oct = a
        .must_create("/api/scenarios", json!({"name": "10月見込", "fiscal_year": 2026, "base_scenario_id": sep}))
        .await;
    put(oct, "2026-10", 100_000).await;
    a.upload(ACTUALS, "target_month,box_code,account_code,amount\n2026-04,PRJ-1,4110,200000\n", "4月実績").await;
    a.put(
        &format!("/api/scenarios/{oct}"),
        json!({"name": "10月見込", "actual_through": "2026-04", "reason": "4月確定"}),
    )
    .await;
    // 比較シナリオの作成後に、マイルストーンの期日を後ろにずらす
    let (status, body) = a
        .put(
            &format!("/api/activities/{}/milestones/{ms}", f.manual_act),
            json!({"name": "検収", "due_date": day(40), "status": "in_progress", "reason": "先方都合"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let body = a.must_get(&format!("/api/reports/risk?scenario_id={oct}&compare_id={sep}")).await;
    let p = risk_activity(&body, "PRJ-1");
    let w = &p["warnings"];
    assert_eq!(w["downward"]["diff"], json!("-500000"), "下方修正: {w}");
    assert_eq!(w["consecutive"], json!(true), "連続の下方修正");
    assert_eq!(w["accuracy"], json!(50), "当たり具合");
    assert_eq!(w["postponed"], json!({"count": 1, "days": 30}), "後ろ倒し");
    assert_eq!((&p["warning_count"], &p["bad_for_level"]), (&json!(4), &json!(true)));
    assert_eq!(p["compare"]["revenue"], json!("800000"), "比較シナリオの加重見込");
    assert_eq!(w["milestones"], json!([]), "40日後の期日は期日が近いに入らない");
    assert_eq!(body["compare"]["base_scenario_id"], json!(f.budget));

    // 比較シナリオなしでは、比較に基づく警告は出ない
    let body = a.must_get(&format!("/api/reports/risk?scenario_id={oct}")).await;
    let p = risk_activity(&body, "PRJ-1");
    assert_eq!((&p["compare"], &p["warnings"]["downward"]), (&Value::Null, &Value::Null), "{p}");
    let (status, body) = a.get(&format!("/api/reports/risk?scenario_id={oct}&period=x")).await;
    assert!(status == StatusCode::BAD_REQUEST && body.to_string().contains("period"), "{body}");
    let (status, body) = a.get("/api/reports/risk").await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "scenario_id").is_empty(), "{body}");
    let other = a.must_create("/api/scenarios", json!({"name": "2027予算", "fiscal_year": 2027})).await;
    let (status, body) = a.get(&format!("/api/reports/risk?scenario_id={oct}&compare_id={other}")).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "compare_id").is_empty(), "{body}");
}

/// 見込の当たり具合で、比較シナリオでも実績の月は比べない。
#[tokio::test]
async fn risk_accuracy_compared_months() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let a = &f.admin;
    // 計画は 4月・5月とも 100,000。実績も同じ。9月見込は 4月まで実績、10月見込は 5月まで実績
    put_amounts(&f, f.budget, f.manual_act, vec![amt(f.sales, "2026-04", 100_000), amt(f.sales, "2026-05", 100_000)])
        .await;
    a.upload(
        ACTUALS,
        "target_month,box_code,account_code,amount\n2026-04,PRJ-1,4110,100000\n2026-05,PRJ-1,4110,100000\n",
        "4・5月実績",
    )
    .await;
    let sep = a
        .must_create(
            "/api/scenarios",
            json!({"name": "9月見込", "fiscal_year": 2026, "base_scenario_id": f.budget, "actual_through": "2026-04"}),
        )
        .await;
    let oct = a
        .must_create(
            "/api/scenarios",
            json!({"name": "10月見込", "fiscal_year": 2026, "base_scenario_id": sep, "actual_through": "2026-05"}),
        )
        .await;
    let path = format!("/api/reports/risk?scenario_id={oct}&compare_id={sep}");
    let body = a.must_get(&path).await;
    assert_eq!(risk_activity(&body, "PRJ-1")["warnings"]["accuracy"], Value::Null, "計画どおりの実績");
    // 5月（9月見込では計画値の月）の実績が 150,000 なら、差は 50%
    a.upload(ACTUALS, "target_month,box_code,account_code,amount\n2026-05,PRJ-1,4110,150000\n", "5月実績の修正").await;
    let body = a.must_get(&path).await;
    assert_eq!(risk_activity(&body, "PRJ-1")["warnings"]["accuracy"], json!(50));
}

/// 報告資料の Excel を読み、シート名 → 行 → 列の文字列（数値も文字列）を返す。
fn xlsx_sheets(b: &[u8]) -> HashMap<String, Vec<Vec<String>>> {
    let mut zr = zip::ZipArchive::new(Cursor::new(b.to_vec())).expect("zip");
    let mut read = |name: &str| {
        let mut s = String::new();
        zr.by_name(name).unwrap_or_else(|_| panic!("{name} がない")).read_to_string(&mut s).unwrap();
        s
    };
    let unescape = |s: &str| s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&amp;", "&");
    let names: Vec<String> = Regex::new(r#"<sheet name="([^"]*)""#)
        .unwrap()
        .captures_iter(&read("xl/workbook.xml"))
        .map(|c| unescape(&c[1]))
        .collect();
    let row_re = Regex::new(r"(?s)<row [^>]*>(.*?)</row>").unwrap();
    let cell_re = Regex::new(r#"(?s)<c r="([A-Z]+)\d+"[^>]*?(?:/>|>(.*?)</c>)"#).unwrap();
    let value_re = Regex::new(r"(?s)<v>(.*?)</v>|<t[^>]*>(.*?)</t>").unwrap();
    let mut out = HashMap::new();
    for (i, name) in names.into_iter().enumerate() {
        let xml = read(&format!("xl/worksheets/sheet{}.xml", i + 1));
        let mut rows = Vec::new();
        for r in row_re.captures_iter(&xml) {
            let mut row: Vec<String> = Vec::new();
            for c in cell_re.captures_iter(&r[1]) {
                let col = c[1].bytes().fold(0usize, |acc, ch| acc * 26 + (ch - b'A' + 1) as usize);
                let v = c
                    .get(2)
                    .and_then(|inner| value_re.captures(inner.as_str()))
                    .map_or(String::new(), |v| unescape(v.get(1).or(v.get(2)).map_or("", |m| m.as_str())));
                row.resize(row.len().max(col), String::new());
                row[col - 1] = v;
            }
            rows.push(row);
        }
        out.insert(name, rows);
    }
    out
}

/// 先頭の列が label の行（字下げは無視）。
fn find_row<'a>(rows: &'a [Vec<String>], label: &str) -> Option<&'a Vec<String>> {
    rows.iter().find(|r| r.first().is_some_and(|c| c == label))
}

fn joined(rows: &[Vec<String>], label: &str) -> String {
    find_row(rows, label).map(|r| r.join(",")).unwrap_or_default()
}

/// 報告資料の Excel（docs/plan.md「2.23」）。
#[tokio::test]
async fn report_pack() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let a = &f.admin;
    let personnel = a
        .must_create(
            "/api/subjects",
            json!({"code": "8210", "name": "人件費", "category": "expense", "is_restricted": true}),
        )
        .await;
    let other = a
        .must_create("/api/activities", activity_body(f.unit2, "OTHER-1", json!({"owner_user_id": f.member_id})))
        .await;

    // 目標（最初のシナリオ）と、今回の見込（作成中にする）
    put_amounts(
        &f,
        f.budget,
        f.manual_act,
        vec![
            amt(f.sales, "2026-04", 1000),
            amt(f.sales, "2026-10", 2000),
            amt(f.cost, "2026-04", 300),
            amt(personnel, "2026-05", 100),
        ],
    )
    .await;
    put_amounts(&f, f.budget, f.formula_act, vec![amt(f.cost, "2026-04", 50)]).await;
    put_amounts(&f, f.budget, other, vec![amt(f.sales, "2026-07", 700)]).await;
    let forecast = a
        .must_create("/api/scenarios", json!({"name": "10月見込", "fiscal_year": 2026, "base_scenario_id": f.budget}))
        .await;
    put_amounts(&f, forecast, f.manual_act, vec![amt(f.sales, "2026-10", 2500)]).await;
    let (status, body) = a.post(&format!("/api/scenarios/{forecast}/activate"), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = a
        .put(
            &format!("{}/note", f.values_path(forecast, f.manual_act)),
            json!({"explanation": "10月の受注が増えた", "causes": ["volume"]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let path = format!("/api/reports/pack.xlsx?scenario_ids={},{forecast}&grain=year&activities=true", f.budget);
    let (status, headers, b) = a.get_bytes(&path).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let cd = headers.get("content-disposition").unwrap().to_str().unwrap();
    assert!(
        cd.contains("attachment") && cd.contains("filename*=UTF-8''%E5%A0%B1%E5%91%8A%E8%B3%87%E6%96%99_2026_"),
        "{cd}"
    );
    assert_eq!(
        headers.get("content-type").unwrap(),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    );
    let sheets = xlsx_sheets(&b);
    for name in ["条件", "全社 P／L", "ユニット別 P／L", "変動の説明"] {
        assert!(sheets.contains_key(name), "シート {name} がない: {:?}", sheets.keys());
    }

    // 全社 P/L（通期）: 列は 目標・今回・差。予実比較と同じ集計
    let pl = &sheets["全社 P／L"];
    for want in
        ["受託売上,3700,4200,500", "人件費,100,100,0", "収益計,3700,4200,500", "費用計,450,450,0", "利益,3250,3750,500"]
    {
        let label = want.split(',').next().unwrap();
        assert_eq!(joined(pl, label), want, "全社 P/L {label}");
    }
    // ユニット別 P/L: セグメント → ユニット → 施策。列は系列ごとの売上・利益と利益の差
    let units = &sheets["ユニット別 P／L"];
    for want in [
        "XXX事業,3700,3250,4200,3750,500",
        "CCC課,3000,2550,3500,3050,500",
        "DDD課,700,700,700,700,0",
        "合計,3700,3250,4200,3750,500",
    ] {
        let label = want.split(',').next().unwrap();
        assert_eq!(joined(units, label), want, "ユニット別 P/L {label}");
    }
    let prj = units.iter().find(|r| r[0].starts_with("PRJ-1 ")).expect("施策の行がない");
    assert_eq!(prj[1..].join(","), "3000,2600,3500,3100,500");
    // 変動の説明: 今回の見込の施策ごと。差の大きい順
    let notes = &sheets["変動の説明"];
    let r = &notes[3];
    assert_eq!(
        (r[0].as_str(), r[4].as_str(), r[6].as_str(), r[8].as_str(), r[9].as_str()),
        ("PRJ-1", "入力中", "500", "数量・単価の増減", "10月の受注が増えた"),
        "{notes:?}"
    );

    // 範囲（ユニット）と、閲覧制限: 現場には人件費を出さず、条件のシートに注記する
    let (status, _, b) = f
        .member
        .get_bytes(&format!(
            "/api/reports/pack.xlsx?scenario_ids={}&grain=quarter&unit_id={}&sheets=pl",
            f.budget, f.unit1
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let sheets = xlsx_sheets(&b);
    assert!(!sheets.contains_key("ユニット別 P／L") && !sheets.contains_key("変動の説明"), "選んでいないシートがある");
    let pl = &sheets["全社 P／L"];
    assert!(find_row(pl, "人件費").is_none(), "現場に閲覧制限のある科目が出ている");
    // 四半期: Q1〜Q4 と通期（系列が1つなので差の列はない）
    assert_eq!(joined(pl, "収益計"), "収益計,1000,0,2000,0,3000");
    assert_eq!(joined(pl, "費用計"), "費用計,350,0,0,0,350");
    let cond = &sheets["条件"];
    assert!(find_row(cond, "閲覧制限のある科目を除いた金額です").is_some(), "{cond:?}");
    assert!(joined(cond, "範囲").contains("CCC課"), "{cond:?}");

    // 範囲（セグメント）
    let (status, _, b) =
        a.get_bytes(&format!("/api/reports/pack.xlsx?scenario_ids={}&grain=year&sheets=units", f.budget)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(find_row(&xlsx_sheets(&b)["ユニット別 P／L"], "XXX事業").is_some());

    // 条件の誤り
    for q in ["grain=week".to_string(), "sheets=pl,foo".into(), format!("unit_id={}&segment_id=1", f.unit1)] {
        let (status, _, b) = a.get_bytes(&format!("/api/reports/pack.xlsx?scenario_ids={}&{q}", f.budget)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{q}: {}", String::from_utf8_lossy(&b));
    }
    let (status, _, _) = a.get_bytes(&format!("/api/reports/pack.xlsx?scenario_ids={}&unit_id=99999", f.budget)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "存在しないユニット");
}

/// 接続の照合順序は Go 版と同じ（MySQL 8 の既定）。Go 版が作ったビューの文字列リテラルと比べても、照合順序が混ざらない。
#[tokio::test]
async fn connection_collation_matches_go() {
    let Some(env) = TestEnv::start().await else { return };
    let collation: String = env.query_one("SELECT @@collation_connection", ()).await;
    assert_eq!(collation, "utf8mb4_0900_ai_ci");
    let views: Vec<String> = env
        .query("SELECT COLLATION_CONNECTION FROM information_schema.VIEWS WHERE TABLE_SCHEMA = DATABASE()", ())
        .await;
    assert!(!views.is_empty() && views.iter().all(|c| c == "utf8mb4_0900_ai_ci"), "{views:?}");
}
