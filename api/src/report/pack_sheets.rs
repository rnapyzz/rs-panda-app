//! 報告資料（docs/plan.md「2.23」）のシートの組み立て。DB を読まず、集めたデータ（PackInput）から作る。
//! 集計（科目体系・組織の階層の合計、利益、差）は画面の予実比較（web/src/lib/aggregate.ts）と同じ考え方で、整数で計算する。

use std::collections::{HashMap, HashSet};

use num_bigint::BigInt;
use num_traits::Signed;

use super::pack::{NoteRow, PackActivity, PackInput, PackNode, PackUnit, SubjectNode};
use crate::xlsx::{Cell, Merge, Sheet, Style, bold_indent, empty, indent, n, t};

/// 出せるシート（既定の順）。
pub const SHEETS: [&str; 3] = ["pl", "units", "notes"];

/// 全社 P/L の列の期間。
pub const GRAINS: [&str; 4] = ["month", "quarter", "half", "year"];

fn measure_label(m: &str) -> &'static str {
    match m {
        "weighted" => "加重見込",
        "optimistic" => "楽観",
        "pessimistic" => "悲観",
        _ => "満額",
    }
}

fn grain_label(g: &str) -> &'static str {
    match g {
        "quarter" => "四半期",
        "half" => "半期",
        "year" => "通期",
        _ => "月次",
    }
}

fn sheet_label(s: &str) -> &'static str {
    match s {
        "pl" => "全社 P/L",
        "units" => "ユニット別 P/L",
        _ => "変動の説明",
    }
}

fn note_status_label(s: &str) -> &'static str {
    match s {
        "completed" => "完了",
        "in_progress" => "入力中",
        _ => "未着手",
    }
}

fn note_cause_label(c: &str) -> &str {
    match c {
        "timing" => "時期のずれ",
        "volume" => "数量・単価の増減",
        "new" => "新規",
        "lost" => "失注・解約",
        "assumption" => "前提の変化",
        "other" => "その他",
        c => c,
    }
}

/// 期間（列の見出しと月）。
#[derive(Debug, PartialEq)]
struct Period<'a> {
    label: String,
    months: &'a [String],
}

/// 年度の月を期間に分ける。月次・四半期・半期のときは、最後に通期を付ける（web/src/lib/pl.ts の periodsOf と同じ）。
fn periods_of<'a>(months: &'a [String], grain: &str) -> Vec<Period<'a>> {
    let total = Period { label: "通期".into(), months };
    let mut out: Vec<Period> = match grain {
        "month" => months
            .chunks(1)
            .map(|m| Period { label: format!("{}月", m[0][5..7].parse::<u32>().unwrap_or(0)), months: m })
            .collect(),
        "quarter" => {
            months.chunks(3).enumerate().map(|(i, m)| Period { label: format!("Q{}", i + 1), months: m }).collect()
        }
        "half" => months
            .chunks(6)
            .enumerate()
            .map(|(i, m)| Period { label: if i == 0 { "上期" } else { "下期" }.into(), months: m })
            .collect(),
        _ => Vec::new(),
    };
    out.push(total);
    out
}

/// シートを組み立てる。
pub fn build(input: &PackInput) -> Vec<Sheet> {
    let mut sheets = vec![conditions_sheet(input)];
    for s in &input.sheets {
        sheets.push(match *s {
            "pl" => pl_sheet(input),
            "units" => units_sheet(input),
            _ => notes_sheet(input),
        });
    }
    sheets
}

fn conditions_sheet(input: &PackInput) -> Sheet {
    let mut rows: Vec<Vec<Cell>> = vec![vec![t("報告資料の条件", Style::Title)], vec![]];
    let mut add = |label: &str, value: String| rows.push(vec![t(label, Style::Bold), t(value, Style::Plain)]);
    add("年度", format!("{}年度", input.fiscal_year));
    for (i, s) in input.series.iter().enumerate() {
        let mut v = format!("{}. {}", i + 1, s.label);
        if i == 0 {
            v.push_str("（比較元）");
        }
        if let Some(through) = &s.actual_through {
            v.push_str(&format!("（{through} まで実績）"));
        }
        add(if i == 0 { "系列" } else { "" }, v);
    }
    add("金額", measure_label(&input.measure).into());
    add("期間", grain_label(&input.grain).into());
    add("範囲", input.scope_label.clone());
    add("シート", input.sheets.iter().map(|s| sheet_label(s)).collect::<Vec<_>>().join("・"));
    if input.sheets.contains(&"units") {
        add("ユニット別 P/L の施策", if input.with_activities { "含める" } else { "含めない" }.into());
    }
    add("単位", "円".into());
    add("出力した日時", input.now.format("%Y-%m-%d %H:%M").to_string());
    add("出力した人", input.user_name.clone());
    if input.restricted_hidden {
        rows.push(vec![]);
        rows.push(vec![t("閲覧制限のある科目を除いた金額です", Style::Muted)]);
    }
    Sheet { name: "条件".into(), rows, widths: vec![22.0, 60.0], ..Default::default() }
}

/// 科目 ID → 区分（revenue / expense）。
fn categories(nodes: &[SubjectNode], out: &mut HashMap<i64, String>) {
    for s in nodes {
        out.insert(s.id, s.category.clone());
        categories(&s.children, out);
    }
}

fn category_map(input: &PackInput) -> HashMap<i64, String> {
    let mut out = HashMap::new();
    categories(&input.subjects, &mut out);
    out
}

/// 系列ごとの、（科目, 月）→ 金額（ユニット・施策を合算）。
type SubjectMonths = HashMap<(i64, String), BigInt>;

fn subject_months(input: &PackInput) -> Vec<SubjectMonths> {
    input
        .values
        .iter()
        .map(|vals| {
            let mut m: SubjectMonths = HashMap::new();
            for ((_, _, subject, month), v) in vals {
                *m.entry((*subject, month.clone())).or_default() += v;
            }
            m
        })
        .collect()
}

/// 系列 i の、期間（月）の金額。
type PeriodValue<'a> = &'a dyn Fn(usize, &[String]) -> BigInt;

fn pl_sheet(input: &PackInput) -> Sheet {
    let periods = periods_of(&input.months, &input.grain);
    let n_series = input.series.len();
    let per = 2 * n_series - 1; // 期間ごとの列: 系列と、2つ目以降の系列の比較元との差
    let by_subject = subject_months(input);

    let title = format!("P/L（{}・{}・単位: 円）", input.scope_label, measure_label(&input.measure));
    let mut h1 = vec![t("科目", Style::Header)];
    let mut h2 = vec![empty(Style::Header)];
    let mut merges = vec![Merge { row: 1, col: 0, rows: 2, cols: 1 }];
    let mut widths = vec![30.0];
    for (pi, p) in periods.iter().enumerate() {
        h1.push(t(p.label.clone(), Style::Header));
        h1.extend((1..per).map(|_| empty(Style::Header)));
        if per > 1 {
            merges.push(Merge { row: 1, col: 1 + pi * per, rows: 1, cols: per });
        }
        h2.extend(input.series.iter().map(|s| t(s.label.clone(), Style::Header)));
        h2.extend(input.series[1..].iter().map(|s| t(format!("差（{}）", s.label), Style::Header)));
        widths.extend((0..per).map(|_| 15.0));
    }
    let mut rows = vec![vec![t(title, Style::Title)], h1, h2];

    // 金額の行。value(i, months) は系列 i の期間の金額
    let line = |label: Cell, style: Style, value: PeriodValue| -> Vec<Cell> {
        let mut row = vec![label];
        for p in &periods {
            let vs: Vec<BigInt> = (0..n_series).map(|i| value(i, p.months)).collect();
            row.extend(vs.iter().map(|v| n(v.clone(), style)));
            row.extend(vs[1..].iter().map(|v| n(v - &vs[0], style)));
        }
        row
    };
    fn subtree(node: &SubjectNode, by: &SubjectMonths, months: &[String]) -> BigInt {
        let own: BigInt = months.iter().filter_map(|m| by.get(&(node.id, m.clone()))).sum();
        own + node.children.iter().map(|c| subtree(c, by, months)).sum::<BigInt>()
    }
    // 区分の合計は、科目ごとの区分で合計する（子の科目の区分が親と違っても、画面の集計と同じになるように）
    let category_of = category_map(input);
    let month_set = |months: &[String]| months.iter().cloned().collect::<HashSet<String>>();
    let category_total = |category: &str, i: usize, months: &[String]| -> BigInt {
        let ms = month_set(months);
        by_subject[i]
            .iter()
            .filter(|((s, m), _)| category_of.get(s).is_some_and(|c| c == category) && ms.contains(m))
            .map(|(_, v)| v)
            .sum()
    };

    fn emit(
        node: &SubjectNode,
        level: usize,
        rows: &mut Vec<Vec<Cell>>,
        line: &dyn Fn(Cell, Style, PeriodValue) -> Vec<Cell>,
        by_subject: &[SubjectMonths],
    ) {
        rows.push(line(indent(node.name.clone(), level), Style::Number, &|i, months| {
            subtree(node, &by_subject[i], months)
        }));
        for c in &node.children {
            emit(c, level + 1, rows, line, by_subject);
        }
    }
    for (category, label, total) in [("revenue", "収益", "収益計"), ("expense", "費用", "費用計")] {
        rows.push(vec![t(label, Style::Bold)]);
        for s in input.subjects.iter().filter(|s| s.category == category) {
            emit(s, 1, &mut rows, &line, &by_subject);
        }
        rows.push(line(t(total, Style::LabelSum), Style::NumberSum, &|i, months| category_total(category, i, months)));
        rows.push(vec![]);
    }
    rows.push(line(t("利益", Style::LabelSum), Style::NumberSum, &|i, months| {
        category_total("revenue", i, months) - category_total("expense", i, months)
    }));
    Sheet { name: "全社 P／L".into(), rows, widths, freeze_rows: 3, freeze_cols: 1, merges }
}

/// 収益と費用。
#[derive(Clone, Debug, Default)]
struct PackPl {
    revenue: BigInt,
    expense: BigInt,
}

impl PackPl {
    fn profit(&self) -> BigInt {
        &self.revenue - &self.expense
    }

    fn add(&mut self, o: &PackPl) {
        self.revenue += &o.revenue;
        self.expense += &o.expense;
    }

    fn add_amount(&mut self, category: &str, v: &BigInt) {
        if category == "revenue" {
            self.revenue += v;
        } else {
            self.expense += v;
        }
    }
}

/// ユニット別 P/L の1行: 系列ごとの売上・利益と、2つ目以降の系列の利益の差。
fn units_line(n_series: usize, label: Cell, style: Style, value: &dyn Fn(usize) -> PackPl) -> Vec<Cell> {
    let ps: Vec<PackPl> = (0..n_series).map(value).collect();
    let mut row = vec![label];
    for p in &ps {
        row.extend([n(p.revenue.clone(), style), n(p.profit(), style)]);
    }
    row.extend(ps[1..].iter().map(|p| n(p.profit() - ps[0].profit(), style)));
    row
}

/// ユニット別 P/L の組み立てに使う集計。
struct UnitsView<'a> {
    input: &'a PackInput,
    /// 系列ごとの、ユニット・施策の年間（通期）の収益・費用
    by_unit: Vec<HashMap<i64, PackPl>>,
    by_activity: Vec<HashMap<i64, PackPl>>,
    /// セグメント → 出すユニット（範囲のユニットのうち、廃止していないか金額のあるもの）
    units_by_segment: HashMap<i64, Vec<&'a PackUnit>>,
    /// ユニット → 金額のある施策
    activities_by_unit: HashMap<i64, Vec<&'a PackActivity>>,
    /// 親 → 子のセグメント（0 はルート）
    children: HashMap<i64, Vec<&'a PackNode>>,
}

impl UnitsView<'_> {
    fn pl(m: &HashMap<i64, PackPl>, id: i64) -> PackPl {
        m.get(&id).cloned().unwrap_or_default()
    }

    /// セグメントの下のユニット（子孫を含む）。
    fn units_under(&self, id: i64) -> Vec<&PackUnit> {
        let mut out: Vec<&PackUnit> = self.units_by_segment.get(&id).cloned().unwrap_or_default();
        for c in self.children.get(&id).into_iter().flatten() {
            out.extend(self.units_under(c.id));
        }
        out
    }

    /// セグメントの小計、子のセグメント、直下のユニット（と施策）の順に行を出す。ユニットのないセグメントは出さない。
    fn emit_segment(&self, s: &PackNode, level: usize, rows: &mut Vec<Vec<Cell>>) {
        let units = self.units_under(s.id);
        if units.is_empty() {
            return;
        }
        let n_series = self.input.series.len();
        rows.push(units_line(n_series, bold_indent(s.name.clone(), level), Style::NumberBold, &|i| {
            let mut sum = PackPl::default();
            for un in &units {
                sum.add(&Self::pl(&self.by_unit[i], un.id));
            }
            sum
        }));
        for c in self.children.get(&s.id).into_iter().flatten() {
            self.emit_segment(c, level + 1, rows);
        }
        for un in self.units_by_segment.get(&s.id).into_iter().flatten() {
            rows.push(units_line(n_series, indent(un.name.clone(), level + 1), Style::Number, &|i| {
                Self::pl(&self.by_unit[i], un.id)
            }));
            if !self.input.with_activities {
                continue;
            }
            for a in self.activities_by_unit.get(&un.id).into_iter().flatten() {
                rows.push(units_line(
                    n_series,
                    indent(format!("{} {}", a.code, a.name), level + 2),
                    Style::Number,
                    &|i| Self::pl(&self.by_activity[i], a.id),
                ));
            }
        }
    }
}

fn units_sheet(input: &PackInput) -> Sheet {
    let n_series = input.series.len();
    let category_of = category_map(input);

    let mut by_unit: Vec<HashMap<i64, PackPl>> = vec![HashMap::new(); n_series];
    let mut by_activity: Vec<HashMap<i64, PackPl>> = vec![HashMap::new(); n_series];
    let mut units_with_data: HashSet<i64> = HashSet::new();
    let mut activities_with_data: HashSet<i64> = HashSet::new();
    for (i, vals) in input.values.iter().enumerate() {
        for ((unit, activity, subject, _), v) in vals {
            let Some(c) = category_of.get(subject) else { continue };
            by_unit[i].entry(*unit).or_default().add_amount(c, v);
            units_with_data.insert(*unit);
            if *activity != 0 {
                by_activity[i].entry(*activity).or_default().add_amount(c, v);
                activities_with_data.insert(*activity);
            }
        }
    }
    let mut units_by_segment: HashMap<i64, Vec<&PackUnit>> = HashMap::new();
    for un in input.units.iter().filter(|un| !un.archived || units_with_data.contains(&un.id)) {
        units_by_segment.entry(un.segment_id).or_default().push(un);
    }
    let mut activities_by_unit: HashMap<i64, Vec<&PackActivity>> = HashMap::new();
    for a in input.activities.iter().filter(|a| activities_with_data.contains(&a.id)) {
        activities_by_unit.entry(a.unit_id).or_default().push(a);
    }
    let mut children: HashMap<i64, Vec<&PackNode>> = HashMap::new();
    for s in &input.segments {
        children.entry(s.parent_id).or_default().push(s);
    }
    let view = UnitsView { input, by_unit, by_activity, units_by_segment, activities_by_unit, children };

    let title = format!("ユニット別 P/L（{}・{}・通期・単位: 円）", input.scope_label, measure_label(&input.measure));
    let mut h1 = vec![t("セグメント / ユニット / 施策", Style::Header)];
    let mut h2 = vec![empty(Style::Header)];
    let mut merges = vec![Merge { row: 1, col: 0, rows: 2, cols: 1 }];
    let mut widths = vec![36.0];
    for (i, s) in input.series.iter().enumerate() {
        h1.extend([t(s.label.clone(), Style::Header), empty(Style::Header)]);
        h2.extend([t("売上", Style::Header), t("利益", Style::Header)]);
        merges.push(Merge { row: 1, col: 1 + 2 * i, rows: 1, cols: 2 });
        widths.extend([15.0, 15.0]);
    }
    if n_series > 1 {
        h1.push(t(format!("利益の差（比較元: {}）", input.series[0].label), Style::Header));
        h1.extend((2..n_series).map(|_| empty(Style::Header)));
        if n_series > 2 {
            merges.push(Merge { row: 1, col: 1 + 2 * n_series, rows: 1, cols: n_series - 1 });
        }
        for s in &input.series[1..] {
            h2.push(t(s.label.clone(), Style::Header));
            widths.push(15.0);
        }
    }
    let mut rows = vec![vec![t(title, Style::Title)], h1, h2];
    for s in view.children.get(&0).into_iter().flatten() {
        view.emit_segment(s, 0, &mut rows);
    }
    if input.all_units && units_with_data.contains(&0) {
        rows.push(units_line(n_series, t("未割当の実績", Style::Bold), Style::NumberBold, &|i| {
            UnitsView::pl(&view.by_unit[i], 0)
        }));
    }
    rows.push(units_line(n_series, t("合計", Style::LabelSum), Style::NumberSum, &|i| {
        let mut sum = PackPl::default();
        for p in view.by_unit[i].values() {
            sum.add(p);
        }
        sum
    }));
    Sheet { name: "ユニット別 P／L".into(), rows, widths, freeze_rows: 3, freeze_cols: 1, merges }
}

fn notes_sheet(input: &PackInput) -> Sheet {
    let widths = vec![12.0, 28.0, 18.0, 14.0, 10.0, 15.0, 15.0, 15.0, 22.0, 60.0];
    let Some(notes) = &input.notes else {
        return Sheet {
            name: "変動の説明".into(),
            rows: vec![
                vec![t("変動の説明", Style::Title)],
                vec![t(format!("{}年度の作成中のシナリオ（今回の見込）がありません", input.fiscal_year), Style::Muted)],
            ],
            widths,
            ..Default::default()
        };
    };
    let or_none = |s: &str| if s.is_empty() { "なし".to_string() } else { s.to_string() };
    let sub = format!(
        "目標: {}／前回の見込: {}／金額は年間の利益（満額、円）／差の大きい順",
        or_none(&notes.base),
        or_none(&notes.previous)
    );
    let header: Vec<Cell> = [
        "施策コード",
        "施策",
        "ユニット",
        "担当者",
        "状態",
        "今回の見込",
        "目標との差",
        "前回の見込との差",
        "要因",
        "説明",
    ]
    .iter()
    .map(|h| t(*h, Style::Header))
    .collect();
    let mut rows = vec![
        vec![t(format!("変動の説明（今回の見込: {}）", notes.scenario), Style::Title)],
        vec![t(sub, Style::Muted)],
        header,
    ];
    for r in &notes.rows {
        let causes: Vec<&str> = r.causes.iter().map(|c| note_cause_label(c)).collect();
        rows.push(vec![
            t(r.code.clone(), Style::Wrap),
            t(r.name.clone(), Style::Wrap),
            t(r.unit.clone(), Style::Wrap),
            t(r.owner.clone(), Style::Wrap),
            t(note_status_label(r.status), Style::Wrap),
            n(r.current.clone(), Style::Number),
            n(r.base_diff.clone(), Style::Number),
            r.previous_diff.clone().map_or_else(|| empty(Style::Number), |v| n(v, Style::Number)),
            t(causes.join("・"), Style::Wrap),
            t(r.explanation.clone(), Style::Wrap),
        ]);
    }
    Sheet { name: "変動の説明".into(), rows, widths, freeze_rows: 3, freeze_cols: 2, ..Default::default() }
}

/// 差の大きい順（目標との差、次に前回の見込との差の絶対値）、同じならコードの順に並べる。
pub fn sort_notes(rows: &mut [NoteRow]) {
    let abs = |v: Option<&BigInt>| v.map(|v| v.abs()).unwrap_or_default();
    rows.sort_by(|a, b| {
        abs(Some(&b.base_diff))
            .cmp(&abs(Some(&a.base_diff)))
            .then_with(|| abs(b.previous_diff.as_ref()).cmp(&abs(a.previous_diff.as_ref())))
            .then_with(|| a.code.cmp(&b.code))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periods() {
        let months = crate::calc::fiscal_months(2026);
        let summary = |grain| {
            periods_of(&months, grain)
                .iter()
                .map(|p| format!("{}:{}", p.label, p.months.len()))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(
            summary("month"),
            "4月:1 5月:1 6月:1 7月:1 8月:1 9月:1 10月:1 11月:1 12月:1 1月:1 2月:1 3月:1 通期:12"
        );
        assert_eq!(summary("quarter"), "Q1:3 Q2:3 Q3:3 Q4:3 通期:12");
        assert_eq!(summary("half"), "上期:6 下期:6 通期:12");
        assert_eq!(summary("year"), "通期:12");
    }
}
