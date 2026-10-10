//! 締切と通知・催促の統合テスト。Go 版の notify_test.go・reminder_test.go。

mod common;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::StatusCode as AxumStatus;
use axum::routing::post;
use chrono::NaiveDateTime;
use common::*;
use fpanda_api::notify::{Options, Service};
use reqwest::StatusCode;
use serde_json::{Value, json};

/// Slack の Incoming Webhook の代わり。受け取った本文を記録する。fail なら 500 を返す。
#[derive(Clone, Default)]
struct SlackStub {
    texts: Arc<Mutex<Vec<String>>>,
    fail: Arc<Mutex<bool>>,
}

impl SlackStub {
    async fn start(&self) -> String {
        let me = self.clone();
        let app = Router::new().route(
            "/",
            post(move |body: axum::Json<Value>| {
                let me = me.clone();
                async move {
                    if *me.fail.lock().unwrap() {
                        return (AxumStatus::INTERNAL_SERVER_ERROR, "invalid_token");
                    }
                    me.texts.lock().unwrap().push(body["text"].as_str().unwrap_or("").to_string());
                    (AxumStatus::OK, "ok")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/")
    }

    fn all(&self) -> Vec<String> {
        self.texts.lock().unwrap().clone()
    }
}

/// 時計を差し替えた通知のサービスを持つフィクスチャ。
struct NotifyFixture {
    f: ScenarioFixture,
    svc: Service,
    slack: SlackStub,
    clock: Arc<Mutex<NaiveDateTime>>,
}

impl NotifyFixture {
    async fn new() -> Option<NotifyFixture> {
        let f = ScenarioFixture::new().await?;
        let slack = SlackStub::default();
        let url = slack.start().await;
        let clock = Arc::new(Mutex::new(NaiveDateTime::default()));
        let c = clock.clone();
        let svc = Service::new(
            f.env.db.clone(),
            Options {
                slack_webhook_url: url,
                base_url: "https://fpanda.example.com".into(),
                now: Some(Box::new(move || *c.lock().unwrap())),
            },
        );
        // manual_act（PRJ-1）の担当は member、formula_act（SAAS-1）も member。unit1 のマネージャーは manager1
        f.env.exec("UPDATE users SET slack_user_id = 'U0MEMBER' WHERE id = ?", (f.member_id,)).await;
        Some(NotifyFixture { f, svc, slack, clock })
    }

    fn at(&self, s: &str) {
        *self.clock.lock().unwrap() = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
    }

    /// ユーザーのお知らせのタイトル（新しい順）。
    async fn notes(&self, user_id: i64) -> Vec<String> {
        self.f.env.query("SELECT title FROM notifications WHERE user_id = ? ORDER BY id DESC", (user_id,)).await
    }

    async fn set_deadline(&self, date: &str) {
        self.f.env.exec("UPDATE scenarios SET update_deadline = ? WHERE id = ?", (date, self.f.budget)).await;
    }
}

#[tokio::test]
async fn deadline_notifications() {
    let Some(n) = NotifyFixture::new().await else { return };
    let f = &n.f;
    // 締切は 2026-10-15（木）。既定の通知は 3日前（10/12 月）と前日（10/14 水）
    n.set_deadline("2026-10-15").await;

    // 更新の開始: 担当者ごとに1件。Slack はメンバー ID でメンションする。2回目は送らない
    n.at("2026-10-07 10:00");
    n.svc.update_started(f.budget).await;
    n.svc.update_started(f.budget).await;
    let got = n.notes(f.member_id).await;
    assert!(got.len() == 1 && got[0].contains("見込の更新が始まりました"), "{got:?}");
    let texts = n.slack.all();
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(
        texts[0].contains("<@U0MEMBER>（2件）")
            && texts[0].contains("10/15（木）")
            && texts[0].contains("<https://fpanda.example.com/|"),
        "{}",
        texts[0]
    );

    // 締切の前: 送信時刻（9:00）より前は送らない。3日前の 9:00 を過ぎたら、未完了の担当者にだけ送る
    f.member.post(&format!("{}/complete", f.values_path(f.budget, f.formula_act)), Value::Null).await; // SAAS-1 は完了
    n.at("2026-10-12 08:59");
    n.svc.run_due().await.unwrap();
    assert_eq!(n.slack.all().len(), 1, "送信時刻の前に送った");
    n.at("2026-10-12 09:01");
    n.svc.run_due().await.unwrap();
    n.svc.run_due().await.unwrap(); // 二重には送らない
    let texts = n.slack.all();
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(texts[1].contains("あと3日") && texts[1].contains("<@U0MEMBER>（1件）"), "{}", texts[1]);
    let got = n.notes(f.member_id).await;
    assert!(got.len() == 2 && got[0].contains("締切まであと3日です"), "{got:?}");
    // 通知の日ではない（10/13）は送らない。前日は「明日」
    n.at("2026-10-13 09:30");
    n.svc.run_due().await.unwrap();
    n.at("2026-10-14 09:30");
    n.svc.run_due().await.unwrap();
    let texts = n.slack.all();
    assert!(texts.len() == 3 && texts[2].contains("明日"), "{texts:?}");

    // 締切の超過: 担当者とユニットのマネージャーに、毎日。土日は送らない
    n.at("2026-10-16 09:00");
    n.svc.run_due().await.unwrap();
    let got = n.notes(f.manager1_id).await;
    assert!(got.len() == 1 && got[0].contains("締切を1日過ぎています"), "{got:?}");
    let body: String = f
        .env
        .query_one("SELECT body FROM notifications WHERE user_id = ? ORDER BY id DESC LIMIT 1", (f.manager1_id,))
        .await;
    assert!(body.contains("所管ユニットの未完了の施策（1件）") && body.contains("PRJ-1"), "{body}");
    n.at("2026-10-17 09:00"); // 土曜
    n.svc.run_due().await.unwrap();
    n.at("2026-10-19 09:00"); // 月曜
    n.svc.run_due().await.unwrap();
    let texts = n.slack.all();
    assert!(
        texts.len() == 5 && texts[4].contains("4日過ぎています") && texts[4].contains("ユニットのマネージャー"),
        "{texts:?}"
    );

    // すべて完了すると送らない
    f.member.post(&format!("{}/complete", f.values_path(f.budget, f.manual_act)), Value::Null).await;
    n.at("2026-10-20 09:00");
    n.svc.run_due().await.unwrap();
    assert_eq!(n.slack.all().len(), 5, "完了後に送った");
}

#[tokio::test]
async fn notification_slack_failure() {
    let Some(n) = NotifyFixture::new().await else { return };
    let f = &n.f;
    n.set_deadline("2026-10-15").await;
    *n.slack.fail.lock().unwrap() = true;
    n.at("2026-10-07 10:00");
    n.svc.update_started(f.budget).await;

    // Slack に失敗しても、アプリ内のお知らせは作る。送信の記録に失敗を残す
    assert_eq!(n.notes(f.member_id).await.len(), 1);
    let runs = f.admin.must_get("/api/notification-runs").await;
    let items = items(&runs);
    assert_eq!(items.len(), 1, "{runs}");
    assert_eq!((&items[0]["slack_status"], &items[0]["slack_attempts"]), (&json!("failed"), &json!(1)));
    assert!(items[0]["slack_error"].as_str().unwrap().contains("500"), "{runs}");
    let (status, _) = f.member.get("/api/notification-runs").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // 10分たてば、同じ日のうちに送り直す（DB の時計で測るので、記録の更新日時を戻す）
    *n.slack.fail.lock().unwrap() = false;
    n.svc.update_started(f.budget).await; // 更新の開始は1回だけなので、送り直しは締切の通知で確かめる
    n.at("2026-10-12 09:30");
    *n.slack.fail.lock().unwrap() = true;
    n.svc.run_due().await.unwrap();
    f.env
        .exec(
            "UPDATE notification_runs SET updated_at = NOW() - INTERVAL 11 MINUTE WHERE kind = 'deadline_reminder'",
            (),
        )
        .await;
    *n.slack.fail.lock().unwrap() = false;
    n.svc.run_due().await.unwrap();
    let (status, attempts): (String, i64) = f
        .env
        .query_one("SELECT slack_status, slack_attempts FROM notification_runs WHERE kind = 'deadline_reminder'", ())
        .await;
    assert_eq!((status.as_str(), attempts), ("sent", 2));
    assert_eq!(n.notes(f.member_id).await.len(), 2, "送り直しでお知らせは増えない");
}

#[tokio::test]
async fn notification_api() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let a = &f.admin;
    let path = format!("/api/scenarios/{}", f.budget);

    // 締切を入れると（作成中のシナリオ）、担当者に「更新の開始」のお知らせが届く
    let (status, body) = a
        .put(&path, json!({"name": "2026年度 当初予算", "plan_role": "initial", "update_deadline": "2026-10-15"}))
        .await;
    assert_eq!((status, body["update_deadline"].clone()), (StatusCode::OK, json!("2026-10-15")), "{body}");
    let (status, body) =
        a.put(&path, json!({"name": "2026年度 当初予算", "plan_role": "initial", "update_deadline": "10/15"})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "update_deadline").is_empty(), "{body}");
    let list = f.member.must_get("/api/notifications").await;
    assert_eq!(list["unread"], json!(1), "{list}");
    let first = &items(&list)[0];
    assert!(first["title"].as_str().unwrap().contains("見込の更新が始まりました"), "{list}");
    assert_eq!(
        (&first["kind"], &first["link"], &first["read_at"]),
        (&json!("update_started"), &json!("/"), &Value::Null)
    );
    let id = first["id"].as_i64().unwrap();
    // ほかの人のお知らせは既読にできない
    let (status, _) = f.viewer.post(&format!("/api/notifications/{id}/read"), Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = f.member.post(&format!("/api/notifications/{id}/read"), Value::Null).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let list = f.member.must_get("/api/notifications").await;
    assert_eq!(list["unread"], json!(0));
    assert!(items(&list)[0]["read_at"].is_string());
    let (status, _) = f.member.post("/api/notifications/read-all", Value::Null).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // 通知の設定
    let settings = f.viewer.must_get("/api/notification-settings").await;
    assert_eq!(
        (&settings["send_time"], &settings["reminder_days"], &settings["slack_configured"]),
        (&json!("09:00"), &json!([3, 1]), &json!(false)),
        "{settings}"
    );
    let (status, body) = a
        .put(
            "/api/notification-settings",
            json!({"enabled_kinds": {"deadline_overdue": false}, "reminder_days": [1, 5, 1], "send_time": "08:30", "reason": "朝に変更"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!((&body["reminder_days"], &body["enabled_kinds"]["deadline_overdue"]), (&json!([5, 1]), &json!(false)));
    assert_eq!(body["enabled_kinds"]["update_started"], json!(true));
    assert_eq!(f.viewer.must_get("/api/notification-settings").await["send_time"], json!("08:30"));
    let (status, body) =
        a.put("/api/notification-settings", json!({"reminder_days": [30], "send_time": "25:00"})).await;
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY
            && !detail(&body, "reminder_days").is_empty()
            && !detail(&body, "send_time").is_empty(),
        "{body}"
    );
    let (status, body) =
        a.put("/api/notification-settings", json!({"enabled_kinds": {"x": true}, "send_time": "09:00"})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "enabled_kinds").is_empty(), "{body}");
    let (status, _) = f.member.put("/api/notification-settings", json!({"send_time": "08:30"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = a.post("/api/notification-settings/test", Value::Null).await;
    assert_eq!(status, StatusCode::CONFLICT, "Slack 未設定のテスト送信");

    // ユーザーの Slack のメンバー ID
    let user = format!("/api/users/{}", f.member_id);
    let req = |slack: &str| json!({"name": "担当", "email": "member@example.com", "role": "member", "is_active": true, "slack_user_id": slack});
    let (status, body) = a.put(&user, req("bad id")).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "slack_user_id").is_empty(), "{body}");
    let (status, body) = a.put(&user, req("U012AB3CD")).await;
    assert_eq!((status, body["slack_user_id"].clone()), (StatusCode::OK, json!("U012AB3CD")), "{body}");
}

#[tokio::test]
async fn actuals_reflected_notification() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let a = &f.admin;
    // 前回見込: 4月 1,200。今回（作成中）は前回見込を複製し、4月の実績 900 を決算確定月で反映する（差 25%）
    let prev = a
        .must_create("/api/scenarios", json!({"name": "9月見込", "fiscal_year": 2026, "base_scenario_id": f.budget}))
        .await;
    let (status, body) = a
        .put(
            &format!("{}/amounts", f.values_path(prev, f.manual_act)),
            json!({"reason": "r", "amounts": [{"subject_id": f.sales, "target_month": "2026-04", "amount": 1200}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let cur = a
        .must_create("/api/scenarios", json!({"name": "10月見込", "fiscal_year": 2026, "base_scenario_id": prev}))
        .await;
    a.post(&format!("/api/scenarios/{cur}/activate"), Value::Null).await;
    a.upload("/api/actuals/import", "target_month,box_code,account_code,amount\n2026-04,PRJ-1,4110,900\n", "4月実績")
        .await;
    let (status, body) = a
        .put(
            &format!("/api/scenarios/{cur}"),
            json!({"name": "10月見込", "actual_through": "2026-04", "previous_scenario_id": prev, "reason": "4月決算確定"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let list = f.member.must_get("/api/notifications").await;
    let items = items(&list);
    assert_eq!(items.len(), 1, "{list}");
    let (title, body) = (items[0]["title"].as_str().unwrap(), items[0]["body"].as_str().unwrap());
    assert!(
        title.contains("4月の実績が反映されました") && body.contains("PRJ-1") && !body.contains("SAAS-1"),
        "{list}"
    );
}

/// FP&A のホームの「催促する」（docs/plan.md「2.20」）。
#[tokio::test]
async fn manual_reminder() {
    let Some(f) = ScenarioFixture::new().await else { return };
    let a = &f.admin;
    let path = format!("/api/scenarios/{}/reminders", f.budget);

    // member は PRJ-1・SAAS-1 の担当（どちらも未完了）
    let (status, body) = a.post(&path, json!({"user_id": f.member_id})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!((&body["activities"], &body["slack"]), (&json!(2), &json!("off")));
    let list = f.member.must_get("/api/notifications").await;
    let n = &items(&list)[0];
    assert!(
        n["title"].as_str().unwrap().contains("見込の更新をお願いします")
            && n["body"].as_str().unwrap().contains("PRJ-1"),
        "{n}"
    );
    assert_eq!(n["kind"], json!("manual_reminder"));
    // 同じ人には1日1回まで
    let (status, _) = a.post(&path, json!({"user_id": f.member_id})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(a.must_get(&path).await["user_ids"], json!([f.member_id]));
    // 担当する未完了の施策がない人
    let (status, body) = a.post(&path, json!({"user_id": f.manager1_id})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && !detail(&body, "user_id").is_empty(), "{body}");
    // FP&A のみ。作成中のシナリオだけ。存在しないシナリオは 404
    let (status, _) = f.member.post(&path, json!({"user_id": f.member_id})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let other = a.must_create("/api/scenarios", json!({"name": "比較用", "fiscal_year": 2026})).await;
    let (status, _) = a.post(&format!("/api/scenarios/{other}/reminders"), json!({"user_id": f.member_id})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = a.post("/api/scenarios/999999/reminders", json!({"user_id": f.member_id})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
