//! 報告資料（docs/plan.md「2.23」）の Excel ファイル（Office Open XML、.xlsx）を書く。
//!
//! ZIP は zip crate で書き、中の XML は必要な最小限（ワークブック・シート・スタイル）だけを文字列で組み立てる。
//! 文字列はセルの中に直接書き（inlineStr）、共有文字列の表は持たない。スタイルは決まった種類（Style）から選ぶ。

use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::{Cursor, Write};

use num_bigint::BigInt;
use zip::write::SimpleFileOptions;

/// セルの書式。styles.xml の cellXfs の並びと同じ順番。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    /// 標準
    Plain = 0,
    /// 太字
    Bold,
    /// 見出し: 太字・薄い灰色の塗り・下罫線・中央
    Header,
    /// 金額: 3桁区切り、マイナスは赤の ▲
    Number,
    /// 合計の金額: 太字・上罫線
    NumberSum,
    /// 合計の見出し: 太字・上罫線
    LabelSum,
    Indent1,
    Indent2,
    Indent3,
    /// 折り返し・上揃え（長い説明）
    Wrap,
    /// シートの題: 太字・大きめ
    Title,
    /// 補足: 灰色の文字
    Muted,
    /// 小計の金額: 太字
    NumberBold,
    /// 小計の見出し: 太字・字下げ 1
    BoldIndent1,
    /// 小計の見出し: 太字・字下げ 2
    BoldIndent2,
}

/// セルの値。
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Empty,
    Text(String),
    Number(BigInt),
}

/// 1つのセル。
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub value: Value,
    pub style: Style,
}

/// 文字列のセル。
pub fn t(text: impl Into<String>, style: Style) -> Cell {
    Cell { value: Value::Text(text.into()), style }
}

/// 数値のセル。
pub fn n(v: BigInt, style: Style) -> Cell {
    Cell { value: Value::Number(v), style }
}

/// 値のないセル（罫線などの書式だけ）。
pub fn empty(style: Style) -> Cell {
    Cell { value: Value::Empty, style }
}

/// 字下げした文字列のセル（level は 0〜3。0 は字下げなし、3 より深いときは 3）。
pub fn indent(text: impl Into<String>, level: usize) -> Cell {
    let style = match level {
        0 => Style::Plain,
        1 => Style::Indent1,
        2 => Style::Indent2,
        _ => Style::Indent3,
    };
    t(text, style)
}

/// 字下げした太字の見出し（小計の行）。level は 0〜2（それより深いときは 2）。
pub fn bold_indent(text: impl Into<String>, level: usize) -> Cell {
    let style = match level {
        0 => Style::Bold,
        1 => Style::BoldIndent1,
        _ => Style::BoldIndent2,
    };
    t(text, style)
}

/// 結合するセルの範囲（0 始まりの行・列）。
#[derive(Clone, Copy, Debug)]
pub struct Merge {
    pub row: usize,
    pub col: usize,
    pub rows: usize,
    pub cols: usize,
}

/// 1枚のシート。
#[derive(Clone, Debug, Default)]
pub struct Sheet {
    pub name: String,
    pub rows: Vec<Vec<Cell>>,
    /// 列の幅（文字数）。0 は既定
    pub widths: Vec<f64>,
    /// 上・左から固定する行・列の数
    pub freeze_rows: usize,
    pub freeze_cols: usize,
    pub merges: Vec<Merge>,
}

/// シートを .xlsx にする。
pub fn write(sheets: &[Sheet]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(!sheets.is_empty(), "xlsx: シートがありません");
    let names = sheet_names(sheets);
    let mut files: Vec<(String, String)> = vec![
        ("[Content_Types].xml".into(), content_types(sheets.len())),
        ("_rels/.rels".into(), ROOT_RELS.into()),
        ("xl/workbook.xml".into(), workbook(&names)),
        ("xl/_rels/workbook.xml.rels".into(), workbook_rels(sheets.len())),
        ("xl/styles.xml".into(), STYLES.into()),
    ];
    for (i, s) in sheets.iter().enumerate() {
        files.push((format!("xl/worksheets/sheet{}.xml", i + 1), sheet_xml(s)));
    }
    let mut zw = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, body) in files {
        zw.start_file(name, opts)?;
        zw.write_all(body.as_bytes())?;
    }
    Ok(zw.finish()?.into_inner())
}

const XML_HEADER: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";

const ROOT_RELS: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n",
    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>"#,
    "</Relationships>"
);

fn content_types(n: usize) -> String {
    let mut b = String::from(XML_HEADER);
    b.push_str(r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">"#);
    b.push_str(r#"<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>"#);
    b.push_str(r#"<Default Extension="xml" ContentType="application/xml"/>"#);
    b.push_str(r#"<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>"#);
    b.push_str(r#"<Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>"#);
    for i in 1..=n {
        let _ = write!(
            b,
            r#"<Override PartName="/xl/worksheets/sheet{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>"#
        );
    }
    b.push_str("</Types>");
    b
}

fn workbook(names: &[String]) -> String {
    let mut b = String::from(XML_HEADER);
    b.push_str(r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>"#);
    for (i, name) in names.iter().enumerate() {
        let _ = write!(b, r#"<sheet name="{}" sheetId="{}" r:id="rId{}"/>"#, esc(name), i + 1, i + 1);
    }
    b.push_str("</sheets></workbook>");
    b
}

fn workbook_rels(n: usize) -> String {
    let mut b = String::from(XML_HEADER);
    b.push_str(r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#);
    for i in 1..=n {
        let _ = write!(
            b,
            r#"<Relationship Id="rId{i}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{i}.xml"/>"#
        );
    }
    let _ = write!(
        b,
        r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>"#,
        n + 1
    );
    b.push_str("</Relationships>");
    b
}

/// Style の順番どおりの cellXfs を持つ。金額の表示形式は 164 番（3桁区切り、マイナスは赤の ▲。値は数値のまま）。
const STYLES: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n",
    r#"<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
    r##"<numFmts count="1"><numFmt numFmtId="164" formatCode="#,##0;[Red]&quot;▲&quot;#,##0"/></numFmts>"##,
    r#"<fonts count="4">"#,
    r#"<font><sz val="11"/><name val="Yu Gothic"/><family val="2"/></font>"#,
    r#"<font><b/><sz val="11"/><name val="Yu Gothic"/><family val="2"/></font>"#,
    r#"<font><b/><sz val="14"/><name val="Yu Gothic"/><family val="2"/></font>"#,
    r#"<font><sz val="10"/><color rgb="FF6B7280"/><name val="Yu Gothic"/><family val="2"/></font>"#,
    "</fonts>",
    r#"<fills count="3"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill>"#,
    r#"<fill><patternFill patternType="solid"><fgColor rgb="FFF1F5F9"/><bgColor indexed="64"/></patternFill></fill></fills>"#,
    r#"<borders count="3">"#,
    "<border><left/><right/><top/><bottom/><diagonal/></border>",
    r#"<border><left/><right/><top/><bottom style="thin"><color rgb="FFCBD5E1"/></bottom><diagonal/></border>"#,
    r#"<border><left/><right/><top style="thin"><color rgb="FF64748B"/></top><bottom/><diagonal/></border>"#,
    "</borders>",
    r#"<cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>"#,
    r#"<cellXfs count="15">"#,
    r#"<xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/>"#,
    r#"<xf numFmtId="0" fontId="1" fillId="0" borderId="0" xfId="0" applyFont="1"/>"#,
    r#"<xf numFmtId="0" fontId="1" fillId="2" borderId="1" xfId="0" applyFont="1" applyFill="1" applyBorder="1" applyAlignment="1"><alignment horizontal="center" vertical="center" wrapText="1"/></xf>"#,
    r#"<xf numFmtId="164" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/>"#,
    r#"<xf numFmtId="164" fontId="1" fillId="0" borderId="2" xfId="0" applyNumberFormat="1" applyFont="1" applyBorder="1"/>"#,
    r#"<xf numFmtId="0" fontId="1" fillId="0" borderId="2" xfId="0" applyFont="1" applyBorder="1"/>"#,
    r#"<xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0" applyAlignment="1"><alignment indent="1"/></xf>"#,
    r#"<xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0" applyAlignment="1"><alignment indent="2"/></xf>"#,
    r#"<xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0" applyAlignment="1"><alignment indent="3"/></xf>"#,
    r#"<xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0" applyAlignment="1"><alignment vertical="top" wrapText="1"/></xf>"#,
    r#"<xf numFmtId="0" fontId="2" fillId="0" borderId="0" xfId="0" applyFont="1"/>"#,
    r#"<xf numFmtId="0" fontId="3" fillId="0" borderId="0" xfId="0" applyFont="1"/>"#,
    r#"<xf numFmtId="164" fontId="1" fillId="0" borderId="0" xfId="0" applyNumberFormat="1" applyFont="1"/>"#,
    r#"<xf numFmtId="0" fontId="1" fillId="0" borderId="0" xfId="0" applyFont="1" applyAlignment="1"><alignment indent="1"/></xf>"#,
    r#"<xf numFmtId="0" fontId="1" fillId="0" borderId="0" xfId="0" applyFont="1" applyAlignment="1"><alignment indent="2"/></xf>"#,
    "</cellXfs>",
    r#"<cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles>"#,
    "</styleSheet>"
);

fn sheet_xml(s: &Sheet) -> String {
    let mut b = String::from(XML_HEADER);
    b.push_str(r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#);
    // 要素の順番は仕様で決まっている: sheetViews → sheetFormatPr → cols → sheetData → mergeCells
    if s.freeze_rows > 0 || s.freeze_cols > 0 {
        let top = cell_ref(s.freeze_rows, s.freeze_cols);
        let (pane, active) = match (s.freeze_rows > 0, s.freeze_cols > 0) {
            (true, true) => (
                format!(
                    r#"<pane xSplit="{}" ySplit="{}" topLeftCell="{top}" activePane="bottomRight" state="frozen"/>"#,
                    s.freeze_cols, s.freeze_rows
                ),
                "bottomRight",
            ),
            (true, false) => (
                format!(
                    r#"<pane ySplit="{}" topLeftCell="{top}" activePane="bottomLeft" state="frozen"/>"#,
                    s.freeze_rows
                ),
                "bottomLeft",
            ),
            _ => (
                format!(
                    r#"<pane xSplit="{}" topLeftCell="{top}" activePane="topRight" state="frozen"/>"#,
                    s.freeze_cols
                ),
                "topRight",
            ),
        };
        let _ = write!(
            b,
            r#"<sheetViews><sheetView workbookViewId="0">{pane}<selection pane="{active}" activeCell="{top}" sqref="{top}"/></sheetView></sheetViews>"#
        );
    } else {
        b.push_str(r#"<sheetViews><sheetView workbookViewId="0"/></sheetViews>"#);
    }
    b.push_str(r#"<sheetFormatPr defaultRowHeight="18"/>"#);
    if s.widths.iter().any(|w| *w > 0.0) {
        b.push_str("<cols>");
        for (i, w) in s.widths.iter().enumerate() {
            if *w > 0.0 {
                let _ = write!(b, r#"<col min="{0}" max="{0}" width="{w:.1}" customWidth="1"/>"#, i + 1);
            }
        }
        b.push_str("</cols>");
    }
    b.push_str("<sheetData>");
    for (r, row) in s.rows.iter().enumerate() {
        let _ = write!(b, r#"<row r="{}">"#, r + 1);
        for (c, cell) in row.iter().enumerate() {
            let rf = cell_ref(r, c);
            let st = cell.style as u8;
            match &cell.value {
                Value::Number(v) => {
                    let _ = write!(b, r#"<c r="{rf}" s="{st}"><v>{v}</v></c>"#);
                }
                Value::Text(text) if !text.is_empty() => {
                    let _ = write!(
                        b,
                        r#"<c r="{rf}" s="{st}" t="inlineStr"><is><t xml:space="preserve">{}</t></is></c>"#,
                        esc(text)
                    );
                }
                _ if cell.style != Style::Plain => {
                    let _ = write!(b, r#"<c r="{rf}" s="{st}"/>"#);
                }
                _ => {}
            }
        }
        b.push_str("</row>");
    }
    b.push_str("</sheetData>");
    if !s.merges.is_empty() {
        let _ = write!(b, r#"<mergeCells count="{}">"#, s.merges.len());
        for m in &s.merges {
            let _ = write!(
                b,
                r#"<mergeCell ref="{}:{}"/>"#,
                cell_ref(m.row, m.col),
                cell_ref(m.row + m.rows - 1, m.col + m.cols - 1)
            );
        }
        b.push_str("</mergeCells>");
    }
    b.push_str(r#"<pageMargins left="0.7" right="0.7" top="0.75" bottom="0.75" header="0.3" footer="0.3"/>"#);
    b.push_str("</worksheet>");
    b
}

/// 0 始まりの行・列を「A1」の形にする。
pub fn cell_ref(row: usize, col: usize) -> String {
    let mut name = Vec::new();
    let mut c = col + 1;
    while c > 0 {
        name.push(b'A' + ((c - 1) % 26) as u8);
        c = (c - 1) / 26;
    }
    name.reverse();
    format!("{}{}", String::from_utf8(name).unwrap(), row + 1)
}

/// シート名を Excel で使える形にする: 使えない文字（[]:*?/\）を _ にし、前後の ' を除き、31文字までにする。
/// 空になった名前や、ほかのシートと（大文字・小文字を区別せずに）重なる名前には番号を付ける。
fn sheet_names(sheets: &[Sheet]) -> Vec<String> {
    let mut used: HashSet<String> = HashSet::new();
    sheets
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let cleaned: String = s.name.chars().map(|c| if r#"[]:*?/\"#.contains(c) { '_' } else { c }).collect();
            let base: String = cleaned.trim_matches('\'').chars().take(31).collect();
            let base = if base.is_empty() { format!("Sheet{}", i + 1) } else { base };
            let mut name = base.clone();
            let mut k = 2;
            while !used.insert(name.to_lowercase()) {
                let suffix = format!(" ({k})");
                let keep = 31 - suffix.chars().count();
                name = format!("{}{suffix}", base.chars().take(keep).collect::<String>());
                k += 1;
            }
            name
        })
        .collect()
}

/// XML の文字をエスケープする。XML 1.0 に入れられない文字（制御文字・U+FFFE・U+FFFF）は除く。
fn esc(s: &str) -> String {
    let mut b = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => b.push_str("&amp;"),
            '<' => b.push_str("&lt;"),
            '>' => b.push_str("&gt;"),
            '"' => b.push_str("&quot;"),
            '\t' | '\n' | '\r' => b.push(c),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => b.push(c),
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    #[test]
    fn cell_refs() {
        for (row, col, want) in
            [(0, 0, "A1"), (9, 25, "Z10"), (0, 26, "AA1"), (1, 27, "AB2"), (0, 701, "ZZ1"), (0, 702, "AAA1")]
        {
            assert_eq!(cell_ref(row, col), want);
        }
    }

    #[test]
    fn names() {
        let sheet = |name: &str| Sheet { name: name.into(), ..Default::default() };
        let names = sheet_names(&[
            sheet("条件/テスト:長い名前のシート名は31文字までに切り詰める必要がある"),
            sheet("A"),
            sheet("a"),
            sheet("''"),
        ]);
        assert_eq!(names[0], "条件_テスト_長い名前のシート名は31文字までに切り詰める必要");
        assert_eq!(names[1..], ["A", "a (2)", "Sheet4"]);
    }

    #[test]
    fn writes_parts() {
        let b = write(&[
            Sheet {
                name: "全社 P/L".into(),
                rows: vec![
                    vec![t("科目", Style::Header), t("A社 & <B>", Style::Header)],
                    vec![indent("売上", 1), n(BigInt::from(-1234567), Style::Number)],
                    vec![t("合計", Style::LabelSum), n(BigInt::from(0), Style::NumberSum)],
                ],
                widths: vec![20.0, 14.0],
                freeze_rows: 1,
                freeze_cols: 1,
                merges: vec![Merge { row: 0, col: 0, rows: 1, cols: 2 }],
            },
            Sheet {
                name: "条件".into(),
                rows: vec![vec![t("年度", Style::Bold), t("2026\u{1}年度", Style::Plain)]],
                ..Default::default()
            },
        ])
        .unwrap();
        let mut zr = zip::ZipArchive::new(Cursor::new(b)).unwrap();
        let mut read = |name: &str| {
            let mut s = String::new();
            zr.by_name(name).unwrap_or_else(|_| panic!("{name} がない")).read_to_string(&mut s).unwrap();
            s
        };
        for name in ["[Content_Types].xml", "_rels/.rels", "xl/_rels/workbook.xml.rels", "xl/styles.xml"] {
            read(name);
        }
        let s1 = read("xl/worksheets/sheet1.xml");
        for want in [
            r#"<c r="B2" s="3"><v>-1234567</v></c>"#,
            "A社 &amp; &lt;B&gt;",
            r#"<mergeCell ref="A1:B1"/>"#,
            r#"xSplit="1" ySplit="1" topLeftCell="B2""#,
            r#"<col min="1" max="1" width="20.0" customWidth="1"/>"#,
        ] {
            assert!(s1.contains(want), "sheet1 に {want} がない");
        }
        assert!(read("xl/workbook.xml").contains(r#"name="全社 P_L""#));
        assert!(!read("xl/worksheets/sheet2.xml").contains('\u{1}'));
    }
}
