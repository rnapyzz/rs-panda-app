//! ドライバーから金額を算出する計算式を解析・評価する。
//!
//! 使える要素は、数値リテラル（例: 1000, 0.25）、識別子（ドライバーの code）、
//! 四則演算（+ - * /）、単項マイナス、括弧のみ。
//! 計算は有理数（BigRational）で行い、浮動小数点の誤差を出さない。
//!
//! ```text
//! unit_price * volume
//! (headcount * monthly_cost) + fixed_cost
//! ```

use std::collections::{BTreeSet, HashMap};
use std::fmt;

use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{Signed, Zero};

/// 有理数。
pub type Rat = BigRational;

/// 評価のエラー。
#[derive(Debug, PartialEq, Eq)]
pub enum EvalError {
    /// 0 で割った
    DivisionByZero,
    /// 識別子の値がない
    MissingValue(String),
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EvalError::DivisionByZero => write!(f, "0 で割ることはできません"),
            EvalError::MissingValue(name) => write!(f, "{name} の値がありません"),
        }
    }
}

impl std::error::Error for EvalError {}

#[derive(Clone, Debug)]
enum Node {
    Num(Rat),
    Ident(String),
    Neg(Box<Node>),
    Bin(u8, Box<Node>, Box<Node>),
}

impl Node {
    fn eval(&self, vars: &HashMap<String, Rat>) -> Result<Rat, EvalError> {
        match self {
            Node::Num(v) => Ok(v.clone()),
            Node::Ident(name) => vars.get(name).cloned().ok_or_else(|| EvalError::MissingValue(name.clone())),
            Node::Neg(x) => Ok(-x.eval(vars)?),
            Node::Bin(op, l, r) => {
                let l = l.eval(vars)?;
                let r = r.eval(vars)?;
                Ok(match op {
                    b'+' => l + r,
                    b'-' => l - r,
                    b'*' => l * r,
                    _ => {
                        if r.is_zero() {
                            return Err(EvalError::DivisionByZero);
                        }
                        l / r
                    }
                })
            }
        }
    }

    fn idents(&self, set: &mut BTreeSet<String>) {
        match self {
            Node::Num(_) => {}
            Node::Ident(n) => {
                set.insert(n.clone());
            }
            Node::Neg(x) => x.idents(set),
            Node::Bin(_, l, r) => {
                l.idents(set);
                r.idents(set);
            }
        }
    }
}

/// 解析済みの計算式。
#[derive(Clone, Debug)]
pub struct Expr {
    root: Node,
}

impl Expr {
    /// 計算式を解析する。構文エラーは位置を含むメッセージで返す。
    pub fn parse(src: &str) -> Result<Expr, String> {
        let mut p = Parser { src, pos: 0, tok: Token { kind: Kind::Eof, text: String::new(), pos: 0 } };
        p.next();
        if p.tok.kind == Kind::Eof {
            return Err("計算式が空です".into());
        }
        let root = p.expr()?;
        if p.tok.kind != Kind::Eof {
            return Err(p.error(&format!("不要な {:?} があります", p.tok.text)));
        }
        Ok(Expr { root })
    }

    /// 式に含まれる識別子を名前順で返す。
    pub fn idents(&self) -> Vec<String> {
        let mut set = BTreeSet::new();
        self.root.idents(&mut set);
        set.into_iter().collect()
    }

    /// 識別子に値を割り当てて式を評価する。
    pub fn eval(&self, vars: &HashMap<String, Rat>) -> Result<Rat, EvalError> {
        self.root.eval(vars)
    }
}

/// 10進数の文字列（例: "1200", "-0.25", "1.0000"）を有理数にする。
pub fn parse_decimal(s: &str) -> Option<Rat> {
    let s = s.trim();
    let (neg, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    if (int.is_empty() && frac.is_empty()) || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let num: BigInt = format!("{int}{frac}").parse().unwrap_or_default();
    let den = num_traits::pow(BigInt::from(10), frac.len());
    let r = Rat::new(num, den);
    Some(if neg { -r } else { r })
}

/// x を小数点以下 0 桁に四捨五入（0 から遠い方へ）した整数を返す。
pub fn round_half_up(x: &Rat) -> BigInt {
    let num = x.numer().abs();
    let den = x.denom().clone();
    // (|num| * 2 + den) / (den * 2) で四捨五入
    let q: BigInt = (num * 2 + &den) / (den * 2);
    if x.is_negative() { -q } else { q }
}

/// 有理数を小数点以下 scale 桁の10進数の文字列にする（四捨五入）。
pub fn format_decimal(x: &Rat, scale: usize) -> String {
    let factor = num_traits::pow(BigInt::from(10), scale);
    let scaled = round_half_up(&(x * Rat::from_integer(factor)));
    if scale == 0 {
        return scaled.to_string();
    }
    let neg = scaled.is_negative();
    let digits = scaled.abs().to_string();
    let digits = format!("{digits:0>width$}", width = scale + 1);
    let (i, f) = digits.split_at(digits.len() - scale);
    format!("{}{i}.{f}", if neg { "-" } else { "" })
}

// --- 字句解析・構文解析 ---

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Eof,
    Num,
    Ident,
    Op,
}

#[derive(Clone, Debug)]
struct Token {
    kind: Kind,
    text: String,
    pos: usize,
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
    tok: Token,
}

fn is_ident_start(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphabetic()
}

fn is_ident_part(c: u8) -> bool {
    is_ident_start(c) || c.is_ascii_digit()
}

impl Parser<'_> {
    fn error(&self, msg: &str) -> String {
        format!("{}文字目: {msg}", self.tok.pos + 1)
    }

    fn next(&mut self) {
        let b = self.src.as_bytes();
        while self.pos < b.len() && b" \t\r\n".contains(&b[self.pos]) {
            self.pos += 1;
        }
        let start = self.pos;
        if self.pos >= b.len() {
            self.tok = Token { kind: Kind::Eof, text: String::new(), pos: start };
            return;
        }
        let c = b[self.pos];
        let kind = if c.is_ascii_digit() || (c == b'.' && self.pos + 1 < b.len() && b[self.pos + 1].is_ascii_digit()) {
            while self.pos < b.len() && (b[self.pos].is_ascii_digit() || b[self.pos] == b'.') {
                self.pos += 1;
            }
            Kind::Num
        } else if is_ident_start(c) {
            while self.pos < b.len() && is_ident_part(b[self.pos]) {
                self.pos += 1;
            }
            Kind::Ident
        } else {
            // 演算子・括弧、または不正な文字（マルチバイト文字を含む）を1文字として扱う
            let ch = self.src[self.pos..].chars().next().unwrap();
            self.pos += ch.len_utf8();
            Kind::Op
        };
        self.tok = Token { kind, text: self.src[start..self.pos].to_string(), pos: start };
    }

    fn is_op(&self, ops: &[&str]) -> bool {
        self.tok.kind == Kind::Op && ops.contains(&self.tok.text.as_str())
    }

    // expr := term (('+' | '-') term)*
    fn expr(&mut self) -> Result<Node, String> {
        let mut l = self.term()?;
        while self.is_op(&["+", "-"]) {
            let op = self.tok.text.as_bytes()[0];
            self.next();
            let r = self.term()?;
            l = Node::Bin(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    // term := unary (('*' | '/') unary)*
    fn term(&mut self) -> Result<Node, String> {
        let mut l = self.unary()?;
        while self.is_op(&["*", "/"]) {
            let op = self.tok.text.as_bytes()[0];
            self.next();
            let r = self.unary()?;
            l = Node::Bin(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    // unary := '-' unary | primary
    fn unary(&mut self) -> Result<Node, String> {
        if self.is_op(&["-"]) {
            self.next();
            return Ok(Node::Neg(Box::new(self.unary()?)));
        }
        self.primary()
    }

    // primary := number | ident | '(' expr ')'
    fn primary(&mut self) -> Result<Node, String> {
        match self.tok.kind {
            Kind::Num => {
                let text = self.tok.text.clone();
                let v = if text.matches('.').count() > 1 { None } else { parse_decimal(&text) };
                let Some(v) = v else {
                    return Err(self.error(&format!("数値 {text:?} が正しくありません")));
                };
                self.next();
                Ok(Node::Num(v))
            }
            Kind::Ident => {
                let name = self.tok.text.clone();
                self.next();
                Ok(Node::Ident(name))
            }
            Kind::Op if self.tok.text == "(" => {
                self.next();
                let x = self.expr()?;
                if !self.is_op(&[")"]) {
                    return Err(self.error("「)」が必要です"));
                }
                self.next();
                Ok(x)
            }
            Kind::Op => Err(self.error(&format!("{:?} は使えません", self.tok.text))),
            Kind::Eof => Err(self.error("式が途中で終わっています")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rat(s: &str) -> Rat {
        if let Some((n, d)) = s.split_once('/') {
            return Rat::new(n.parse().unwrap(), d.parse().unwrap());
        }
        parse_decimal(s).unwrap()
    }

    #[test]
    fn eval() {
        let vars: HashMap<String, Rat> =
            [("unit_price", "1200"), ("volume", "3"), ("probability", "0.7"), ("a", "0.1"), ("b", "0.2")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), rat(v)))
                .collect();
        for (src, want) in [
            ("unit_price * volume * probability", "2520"),
            ("1 + 2 * 3", "7"),
            ("(1 + 2) * 3", "9"),
            ("10 - 4 - 3", "3"),  // 左結合
            ("100 / 4 / 5", "5"), // 左結合
            ("-3 * -2", "6"),     // 単項マイナス
            ("-(1 + 2)", "-3"),
            ("a + b", "3/10"),    // 浮動小数点なら 0.30000000000000004
            ("10 / 3 * 3", "10"), // 有理数で誤差なし
            (" .5 * 4 ", "2"),
        ] {
            let e = Expr::parse(src).unwrap_or_else(|e| panic!("parse({src:?}): {e}"));
            assert_eq!(e.eval(&vars).unwrap(), rat(want), "{src}");
        }
    }

    #[test]
    fn parse_errors() {
        for (src, want) in [
            ("", "空"),
            ("   ", "空"),
            ("1 +", "途中で終わって"),
            ("(1 + 2", ")"),
            ("1 + 2)", "不要な"),
            ("a b", "不要な"),
            ("1 ^ 2", "不要な"),
            ("売上 * 2", "使えません"),
            ("1.2.3", "正しくありません"),
            ("* 2", "使えません"),
        ] {
            let err = Expr::parse(src).expect_err(src);
            assert!(err.contains(want), "parse({src:?}) err = {err:?}, want {want:?}");
        }
        assert_eq!(Expr::parse("1 +").unwrap_err(), "4文字目: 式が途中で終わっています");
    }

    #[test]
    fn idents() {
        let e = Expr::parse("unit_price * volume + unit_price * 0.1 - fixed_cost").unwrap();
        assert_eq!(e.idents(), vec!["fixed_cost", "unit_price", "volume"]);
    }

    #[test]
    fn eval_errors() {
        let e = Expr::parse("a / b").unwrap();
        let vars: HashMap<String, Rat> = [("a".to_string(), rat("1")), ("b".to_string(), rat("0"))].into();
        assert_eq!(e.eval(&vars), Err(EvalError::DivisionByZero));
        let vars: HashMap<String, Rat> = [("a".to_string(), rat("1"))].into();
        assert!(e.eval(&vars).unwrap_err().to_string().contains('b'));
    }

    #[test]
    fn rounding() {
        for (input, want) in [
            ("0", "0"),
            ("100.4", "100"),
            ("100.5", "101"),
            ("100.6", "101"),
            ("-100.4", "-100"),
            ("-100.5", "-101"),
            ("2/3", "1"),
            ("1/3", "0"),
        ] {
            assert_eq!(round_half_up(&rat(input)).to_string(), want, "{input}");
        }
        assert_eq!(format_decimal(&rat("0.05"), 4), "0.0500");
        assert_eq!(format_decimal(&rat("-1/3"), 2), "-0.33");
        assert_eq!(format_decimal(&rat("12.5"), 0), "13");
        assert_eq!(parse_decimal("1.0000"), Some(rat("1")));
        assert_eq!(parse_decimal("abc"), None);
    }
}
