//! MySQL を扱う小さな共通処理（Go 版の dbx）。

use mysql_async::prelude::*;
use mysql_async::{Opts, OptsBuilder, PoolConstraints, PoolOpts, TxOpts};

use crate::AppResult;
use crate::config::DbConfig;

pub use mysql_async::{Params, Pool, Row, Value};

/// トランザクション（コネクションを所有する）。commit せずに drop するとロールバックする。
pub type Tx = mysql_async::Transaction<'static>;

/// MySQL のエラー番号
pub const ERR_DUPLICATE_ENTRY: u16 = 1062;
pub const ERR_ROW_IS_REFERENCED: u16 = 1451;
/// 参照先の行がない（外部キー）
pub const ERR_NO_REFERENCED_ROW: u16 = 1452;

/// 位置パラメーター。`p![a, b, c]`。
#[macro_export]
macro_rules! p {
    () => { $crate::db::Params::Empty };
    ($($v:expr),+ $(,)?) => {
        $crate::db::Params::Positional(vec![$($crate::db::Value::from($v)),+])
    };
}

/// 列の順に取り出して構造体にする FromRow を実装する。`from_row!(Type { a, b, c });`
#[macro_export]
macro_rules! from_row {
    ($t:ty { $($field:ident),* $(,)? }) => {
        impl mysql_async::prelude::FromRow for $t {
            #[allow(unused_assignments)]
            fn from_row_opt(mut row: mysql_async::Row) -> Result<Self, mysql_async::FromRowError> {
                let mut i = 0usize;
                $(
                    let $field = match row.take_opt(i) {
                        Some(Ok(v)) => v,
                        _ => return Err(mysql_async::FromRowError(row)),
                    };
                    i += 1;
                )*
                Ok(Self { $($field),* })
            }
        }
    };
}

/// 接続の文字コードと照合順序。Go 版（go-sql-driver の charset=utf8mb4）と同じく、MySQL 8 の既定の照合順序にする。
/// ビュー（scenario_amounts など）の中の文字列リテラルは作ったときの接続の照合順序を持つので、
/// Go 版が作った DB を Rust 版で読むとき（またはその逆）に、比較で照合順序が混ざらないようにそろえる。
const SET_NAMES: &str = "SET NAMES utf8mb4 COLLATE utf8mb4_0900_ai_ci";

/// 接続の設定。
pub fn opts(c: &DbConfig, db_name: Option<&str>) -> Opts {
    OptsBuilder::default()
        // 新しい接続と、プールに戻した接続のリセットのたびに実行する
        .setup(vec![SET_NAMES])
        .ip_or_hostname(c.host.clone())
        .tcp_port(c.port)
        .user(Some(c.user.clone()))
        .pass(Some(c.password.clone()))
        .db_name(db_name.map(str::to_string))
        .prefer_socket(false)
        .pool_opts(PoolOpts::default().with_constraints(PoolConstraints::new(0, 20).unwrap()))
        .into()
}

/// アプリ用のコネクションプール。
pub fn pool(c: &DbConfig) -> Pool {
    Pool::new(opts(c, Some(&c.name)))
}

/// トランザクションを始める。
pub async fn begin(pool: &Pool) -> AppResult<Tx> {
    Ok(pool.start_transaction(TxOpts::default()).await?)
}

/// MySQL のエラー番号を返す。MySQL のエラーでなければ 0。
pub fn err_no(err: &mysql_async::Error) -> u16 {
    match err {
        mysql_async::Error::Server(e) => e.code,
        _ => 0,
    }
}

/// 一意制約違反か。
pub fn is_dup<T>(r: &Result<T, mysql_async::Error>) -> bool {
    matches!(r, Err(e) if err_no(e) == ERR_DUPLICATE_ENTRY)
}

/// 参照されている行の削除か。
pub fn is_referenced<T>(r: &Result<T, mysql_async::Error>) -> bool {
    matches!(r, Err(e) if err_no(e) == ERR_ROW_IS_REFERENCED)
}

/// COUNT(*) を返すクエリを実行する。
pub async fn count<Q: Queryable>(q: &mut Q, sql: &str, params: Params) -> AppResult<i64> {
    Ok(q.exec_first::<i64, _, _>(sql, params).await?.unwrap_or(0))
}

/// table に id のレコードが存在するかを返す。tx 内で FOR SHARE ロックを取る。
pub async fn exists(tx: &mut Tx, table: &str, id: i64) -> AppResult<bool> {
    let one: Option<i64> = tx.exec_first(format!("SELECT 1 FROM {table} WHERE id = ? FOR SHARE"), (id,)).await?;
    Ok(one.is_some())
}

/// 直前の INSERT の ID。
pub fn last_id(tx: &Tx) -> i64 {
    tx.last_insert_id().unwrap_or(0) as i64
}

/// 空文字列を NULL として扱う。
pub fn null_str(s: &str) -> Value {
    if s.is_empty() { Value::NULL } else { Value::from(s) }
}
