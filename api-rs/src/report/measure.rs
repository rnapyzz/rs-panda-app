//! 楽観・基準（加重見込）・悲観の算出（docs/plan.md「2.8」）。
//! 金額は満額で保存し、集計のときに内訳（なければ施策）の確度の段階と見通しの種類で算出する。実績の月はどれも実績の金額。
//!
//! - 楽観: ベース・アドオンはすべて計上、ダウンサイドは計上しない
//! - 基準: 金額 × 段階の標準の確率
//! - 悲観: ベース・アドオンは確度の高い段階（標準の確率が HIGH_RATE 以上。初期値では A・B）だけ計上、ダウンサイドは全額を計上

/// 「確度の高い段階」とみなす標準の確率の下限（初期値の A 確定 100%・B 高 80%）。
pub const HIGH_RATE: &str = "0.8";

/// 指標（full は満額）。
pub const MEASURES: [&str; 4] = ["full", "weighted", "optimistic", "pessimistic"];

/// scenario_amounts（別名 b）に、段階（cl）と内訳（l）を結びつける JOIN。
pub const JOINS: &str = "
    JOIN activities ma ON ma.id = b.activity_id
    LEFT JOIN activity_lines l ON l.id = b.line_id
    JOIN confidence_levels cl ON cl.code = COALESCE(l.confidence_level, ma.confidence_level)";

/// 指標の金額を合計する SQL の式（円未満は四捨五入）。
pub fn sum(measure: &str) -> String {
    match measure {
        "weighted" => {
            "CAST(ROUND(SUM(CASE WHEN b.kind = 'actual' THEN b.amount ELSE b.amount * cl.rate END)) AS CHAR)".into()
        }
        "optimistic" => "CAST(SUM(CASE WHEN b.kind = 'actual' THEN b.amount WHEN COALESCE(l.outlook, 'base') = 'downside' THEN 0 ELSE b.amount END) AS CHAR)".into(),
        "pessimistic" => format!(
            "CAST(SUM(CASE WHEN b.kind = 'actual' THEN b.amount WHEN COALESCE(l.outlook, 'base') = 'downside' THEN b.amount WHEN cl.rate >= {HIGH_RATE} THEN b.amount ELSE 0 END) AS CHAR)"
        ),
        _ => "CAST(SUM(b.amount) AS CHAR)".into(),
    }
}
