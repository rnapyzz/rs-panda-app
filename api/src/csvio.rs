//! マスタ・施策の CSV インポート・エクスポートに共通する処理。
//!
//! インポートは「追加と更新」のみ（CSV にない行は削除しない）。1行でもエラーがあれば何も保存せず、
//! 行番号付きのエラーを返す。dry_run=true なら検証と件数の集計だけを行う。
//! エクスポートは Excel でそのまま開けるよう、BOM 付きの UTF-8 で書き出す。

use std::collections::HashMap;

use axum::extract::multipart::{Multipart, MultipartRejection};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Serialize;

use crate::error::{ApiError, AppError, RowError, bad_request, too_large, validation1};
use crate::http::{self, Query};
use crate::{AppResult, jst};

/// 取込ファイルの上限（20MB）。ルートの DefaultBodyLimit もこの値にする。
pub const MAX_UPLOAD_BYTES: usize = 20 << 20;
const MAX_ROWS: usize = 50_000;
const MAX_ROW_ERRORS: usize = 100;

const BOM: &[u8] = b"\xef\xbb\xbf";

/// 取込リクエストの内容。
#[derive(Default)]
pub struct Upload {
    pub data: Vec<u8>,
    pub reason: String,
    pub dry_run: bool,
}

/// multipart/form-data の file と reason を読み込む。本番の取込（dry run 以外）は理由が必須。
pub async fn read_upload(q: &Query, mp: Result<Multipart, MultipartRejection>) -> AppResult<Upload> {
    read_upload_with(q, mp, "ファイルは 20MB 以下にしてください", "変更理由を入力してください").await
}

/// read_upload と同じだが、大きすぎるときと理由がないときのメッセージを指定する（上限はルートの DefaultBodyLimit）。
pub async fn read_upload_with(
    q: &Query,
    mp: Result<Multipart, MultipartRejection>,
    too_large_msg: &str,
    reason_msg: &str,
) -> AppResult<Upload> {
    let mut up = Upload { dry_run: q.get("dry_run") == "true", ..Default::default() };
    let form_error = || bad_request("multipart/form-data で file と reason を送ってください");
    let mut mp = mp.map_err(|_| form_error())?;
    let mut file: Option<Vec<u8>> = None;
    let mut reason: Option<String> = None;
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                return Err(too_large(too_large_msg));
            }
            Err(_) => return Err(form_error()),
        };
        let name = field.name().unwrap_or("").to_string();
        let is_file = field.file_name().is_some();
        let bytes = match field.bytes().await {
            Ok(b) => b,
            Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                return Err(too_large(too_large_msg));
            }
            Err(_) => return Err(form_error()),
        };
        match name.as_str() {
            "file" if is_file && file.is_none() => file = Some(bytes.to_vec()),
            "reason" if !is_file && reason.is_none() => reason = Some(String::from_utf8_lossy(&bytes).into_owned()),
            _ => {}
        }
    }
    up.reason = reason.unwrap_or_default().trim().to_string();
    if up.reason.is_empty() && !up.dry_run {
        return Err(validation1("reason", reason_msg));
    }
    match file {
        Some(f) => up.data = f,
        None => return Err(validation1("file", "CSV ファイルを選択してください")),
    }
    Ok(up)
}

/// CSV の1データ行。
#[derive(Clone, Debug)]
pub struct Row {
    pub line: usize,
    values: HashMap<String, String>,
}

impl Row {
    /// 列がヘッダーにあるかを返す（省略できる列の判定に使う）。
    pub fn has(&self, column: &str) -> bool {
        self.values.contains_key(column)
    }

    /// 列の値を前後の空白を除いて返す。
    pub fn get(&self, column: &str) -> &str {
        self.values.get(column).map(|s| s.trim()).unwrap_or("")
    }
}

fn utf8_text(data: &[u8]) -> AppResult<&[u8]> {
    let data = data.strip_prefix(BOM).unwrap_or(data);
    if std::str::from_utf8(data).is_err() {
        return Err(bad_request("CSV の文字コードが UTF-8 ではありません。UTF-8 で保存してください"));
    }
    Ok(data)
}

fn reader(data: &[u8]) -> csv::Reader<&[u8]> {
    csv::ReaderBuilder::new().has_headers(false).flexible(true).from_reader(data)
}

/// CSV を読み込み、ヘッダーが columns と一致すること（順番は問わない）を確認して行を返す。
pub fn parse(data: &[u8], columns: &[&str]) -> AppResult<Vec<Row>> {
    parse_with_optional(data, columns, &[])
}

/// parse と同じだが、optional の列はヘッダーになくてもよい（値は空として読む）。
pub fn parse_with_optional(data: &[u8], columns: &[&str], optional: &[&str]) -> AppResult<Vec<Row>> {
    parse_limited(data, columns, optional, MAX_ROWS)
}

/// parse_with_optional と同じだが、データ行の上限を指定する。
pub fn parse_limited(data: &[u8], columns: &[&str], optional: &[&str], max_rows: usize) -> AppResult<Vec<Row>> {
    let mut errs = RowErrors::default();
    let rows = read_rows(data, columns, optional, max_rows, &mut errs, |_, _| {})?;
    errs.check()?;
    if rows.is_empty() {
        return Err(bad_request("CSV にデータ行がありません"));
    }
    Ok(rows)
}

/// CSV を読み、ヘッダーを確かめて行を返す。行の形式のエラーは errs に集め、読めた行ごとに check を呼ぶ
/// （値の検証のエラーも行の順に並ぶ）。ヘッダーのエラーだけは Err で返す。errs の確認と、行がないときの扱いは呼び出し側で行う。
pub fn read_rows(
    data: &[u8],
    columns: &[&str],
    optional: &[&str],
    max_rows: usize,
    errs: &mut RowErrors,
    mut check: impl FnMut(&Row, &mut RowErrors),
) -> AppResult<Vec<Row>> {
    let data = utf8_text(data)?;
    let mut r = reader(data);
    let mut records = r.records();

    let header = match records.next() {
        None => return Err(bad_request("CSV が空です")),
        Some(Err(e)) => return Err(bad_request(format!("CSV のヘッダー行を読み込めません: {e}"))),
        Some(Ok(h)) => h,
    };
    let mut index: HashMap<String, usize> = HashMap::new();
    for (i, h) in header.iter().enumerate() {
        index.insert(h.trim().to_lowercase(), i);
    }
    let header_error = if optional.is_empty() {
        format!("CSV のヘッダー行は {} にしてください（順番は自由）", columns.join(","))
    } else {
        format!(
            "CSV のヘッダー行は {}（必須）と {}（省略可）にしてください（順番は自由）",
            columns.join(","),
            optional.join(",")
        )
    };
    let mut present = 0;
    for c in columns {
        if !index.contains_key(*c) {
            return Err(bad_request(header_error));
        }
        present += 1;
    }
    present += optional.iter().filter(|c| index.contains_key(**c)).count();
    if header.len() != present || index.len() != header.len() {
        return Err(bad_request(header_error));
    }
    let all: Vec<&str> = columns.iter().chain(optional.iter()).copied().collect();

    let mut rows = Vec::new();
    for rec in records {
        let rec = match rec {
            Ok(r) => r,
            Err(e) => {
                let line = e.position().map(|p| line_at(data, p.byte() as usize)).unwrap_or(0);
                errs.add(line, format!("CSV の形式が正しくありません: {e}"));
                break;
            }
        };
        let line = rec.position().map(|p| line_at(data, p.byte() as usize)).unwrap_or(0);
        // 空行は読み飛ばす（Go の encoding/csv と同じ）
        if rec.len() == 1 && rec.get(0) == Some("") {
            continue;
        }
        if rec.len() != header.len() {
            errs.add(line, format!("列の数が {} ではありません", header.len()));
            continue;
        }
        if rows.len() >= max_rows {
            errs.add(line, format!("データ行は {max_rows} 行までです"));
            break;
        }
        let values = all
            .iter()
            .filter_map(|c| index.get(*c).map(|&i| (c.to_string(), rec.get(i).unwrap_or("").trim_start().to_string())))
            .collect();
        let row = Row { line, values };
        check(&row, errs);
        rows.push(row);
    }
    Ok(rows)
}

/// 記録の始まりのバイト位置から、1始まりの行番号を求める（csv crate の行番号は CRLF や空行でずれるため）。
fn line_at(data: &[u8], byte: usize) -> usize {
    let mut start = byte.min(data.len());
    while start < data.len() && (data[start] == b'\r' || data[start] == b'\n') {
        start += 1;
    }
    1 + data[..start].iter().filter(|&&b| b == b'\n').count()
}

/// 行ごとのエラーを上限まで集める。
#[derive(Default, Debug)]
pub struct RowErrors {
    items: Vec<RowError>,
    total: usize,
}

impl RowErrors {
    /// 行のエラーを追加する。
    pub fn add(&mut self, line: usize, msg: impl Into<String>) {
        self.total += 1;
        if self.items.len() < MAX_ROW_ERRORS {
            self.items.push(RowError { line, message: msg.into() });
        }
    }

    /// 検証エラー（項目 → メッセージ）を行のエラーとして追加する。
    pub fn add_details(&mut self, line: usize, err: AppError) {
        match err.details() {
            Some(d) => {
                for msg in d.values() {
                    self.add(line, msg.clone());
                }
            }
            None => match err {
                AppError::Api(e) => self.add(line, e.message),
                AppError::Internal(e) => self.add(line, e.to_string()),
            },
        }
    }

    /// エラーがないかを返す。
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// エラーがあれば 422（code: invalid_csv）を返す。
    pub fn check(&mut self) -> AppResult<()> {
        if self.total == 0 {
            return Ok(());
        }
        let mut msg = format!("CSV に {} 件のエラーがあるため取り込みませんでした", self.total);
        if self.total > self.items.len() {
            msg.push_str(&format!("（先頭の {} 件を表示）", self.items.len()));
        }
        Err(AppError::Api(ApiError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "invalid_csv".into(),
            message: msg,
            details: Default::default(),
            rows: std::mem::take(&mut self.items),
        }))
    }
}

/// 取込の結果。
#[derive(Default, Debug, Serialize)]
pub struct ImportResult {
    pub dry_run: bool,
    pub rows: usize,
    pub inserted: usize,
    pub updated: usize,
    pub unchanged: usize,
    /// 取り込んだうえでの注意（例: 担当が残っている無効なユーザー）
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// 取込の結果を確定する。dry run ならロールバックし、そうでなければコミットして結果を返す。
pub async fn finish(tx: crate::db::Tx, result: ImportResult) -> AppResult<Response> {
    if result.dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(http::ok(&result))
}

/// CSV の本文（BOM 付き UTF-8、CRLF）を作る。
pub fn csv_bytes(header: &[&str], rows: &[Vec<String>]) -> AppResult<Vec<u8>> {
    let mut w = csv::WriterBuilder::new().terminator(csv::Terminator::CRLF).from_writer(BOM.to_vec());
    w.write_record(header)?;
    for r in rows {
        w.write_record(r)?;
    }
    Ok(w.into_inner().map_err(|e| anyhow::anyhow!("csv: {e}"))?)
}

/// BOM 付き UTF-8 の CSV を添付ファイルとして書き出す。ファイル名は "<name>-YYYYMMDD.csv"。
pub fn write_csv(name: &str, header: &[&str], rows: &[Vec<String>]) -> AppResult<Response> {
    let filename = format!("{name}-{}.csv", jst::now().format("%Y%m%d"));
    Ok(http::attachment("text/csv; charset=utf-8", &filename, csv_bytes(header, rows)?))
}

/// CSV のヘッダー行の列名（小文字・前後の空白なし）を返す。列によって形式を判定するときに使う。
pub fn header(data: &[u8]) -> AppResult<Vec<String>> {
    let data = utf8_text(data)?;
    match reader(data).records().next() {
        None => Err(bad_request("CSV が空です")),
        Some(Err(e)) => Err(bad_request(format!("CSV のヘッダー行を読み込めません: {e}"))),
        Some(Ok(h)) => Ok(h.iter().map(|s| s.trim().to_lowercase()).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rows_with_line_numbers() {
        let data = "\u{feff}Code, name\r\nA,あ\r\n\r\nB,\"い,う\"\r\nC,\"x\r\ny\"\r\nD,d".as_bytes();
        let rows = parse(data, &["code", "name"]).unwrap();
        let got: Vec<(usize, &str)> = rows.iter().map(|r| (r.line, r.get("code"))).collect();
        assert_eq!(got, vec![(2, "A"), (4, "B"), (5, "C"), (7, "D")]);
        assert_eq!(rows[1].get("name"), "い,う");
        let lf = parse(b"code,name\nA,a\n\nB,b\n", &["code", "name"]).unwrap();
        assert_eq!(lf.iter().map(|r| r.line).collect::<Vec<_>>(), vec![2, 4]);
    }

    #[test]
    fn rejects_bad_header_and_field_count() {
        assert!(parse(b"code\nA\n", &["code", "name"]).is_err());
        let err = parse(b"code,name\nA\nB,b\n", &["code", "name"]).unwrap_err();
        match err {
            AppError::Api(e) => {
                assert_eq!(e.rows, vec![RowError { line: 2, message: "列の数が 2 ではありません".into() }])
            }
            _ => panic!(),
        }
    }

    #[test]
    fn writes_bom_and_crlf() {
        let b = csv_bytes(&["a", "b"], &[vec!["1".into(), "x,y".into()]]).unwrap();
        assert_eq!(b, "\u{feff}a,b\r\n1,\"x,y\"\r\n".as_bytes());
    }
}
