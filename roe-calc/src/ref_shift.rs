//! 结构编辑(插/删行列)后的公式引用改写。
//! 按 token 字节偏移替换引用文本,其余字符(空格、大小写、函数名)原样保留。
//! 语义对齐 GridCraft adjust.rs:map_index/map_span;绝对引用与相对引用同样平移。

use formualizer_parse::parser::{ASTNode, ASTNodeType, ReferenceType};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Row,
    Col,
}

const MAX_ROWS: u32 = 1_048_576;
const MAX_COLS: u32 = 16_384;

fn map_index(v: u32, at: u32, count: i64, max: u32) -> Option<u32> {
    if count >= 0 {
        if v >= at {
            let n = v as i64 + count;
            if n >= max as i64 { None } else { Some(n as u32) }
        } else {
            Some(v)
        }
    } else {
        let c = (-count) as u32;
        if v < at {
            Some(v)
        } else if v >= at.saturating_add(c) {
            Some(v - c)
        } else {
            None
        }
    }
}

fn map_span(lo: u32, hi: u32, at: u32, count: i64, max: u32) -> Option<(u32, u32)> {
    if count >= 0 {
        let lo2 = if lo >= at { lo as i64 + count } else { lo as i64 };
        let hi2 = if hi >= at { hi as i64 + count } else { hi as i64 };
        let hi2 = hi2.min(max as i64 - 1);
        if lo2 > hi2 { return None; }
        Some((lo2 as u32, hi2 as u32))
    } else {
        let c = (-count) as u32;
        let end = at.saturating_add(c);
        if lo >= at && hi < end { return None; }
        let lo2 = if lo < at { lo } else if lo >= end { lo - c } else { at };
        let hi2 = if hi < at { hi } else if hi >= end { hi - c } else { at.saturating_sub(1) };
        Some((lo2, hi2.max(lo2)))
    }
}

fn col_letters(col1: u32) -> String {
    let mut n = col1;
    let mut s = Vec::new();
    while n > 0 {
        let r = ((n - 1) % 26) as u8;
        s.push(b'A' + r);
        n = (n - 1) / 26;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

fn fmt_col(col1: u32, abs: bool) -> String {
    format!("{}{}", if abs { "$" } else { "" }, col_letters(col1))
}
fn fmt_row(row1: u32, abs: bool) -> String {
    format!("{}{}", if abs { "$" } else { "" }, row1)
}

fn fmt_sheet_prefix(sheet: &Option<String>, orig_token: &str) -> String {
    match sheet {
        None => String::new(),
        Some(name) => {
            let quoted_in_src = orig_token.starts_with('\'');
            let needs = quoted_in_src
                || name.is_empty()
                || !name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.')
                || name.chars().next().map_or(false, |c| c.is_ascii_digit());
            if needs {
                format!("'{}'!", name.replace('\'', "''"))
            } else {
                format!("{name}!")
            }
        }
    }
}

/// 返回 None = 引用不变;Some(text) = 用 text 替换整个 token。
fn rewrite_ref(
    r: &ReferenceType,
    tok: &str,
    host: &str,
    edit_sheet: &str,
    axis: Axis,
    at: u32,
    count: i64,
) -> Option<String> {
    match r {
        ReferenceType::Cell { sheet, row, col, row_abs, col_abs } => {
            let target = sheet.as_deref().unwrap_or(host);
            if !target.eq_ignore_ascii_case(edit_sheet) { return None; }
            let (nr, nc) = match axis {
                Axis::Row => (map_index(row - 1, at, count, MAX_ROWS).map(|v| v + 1), Some(*col)),
                Axis::Col => (Some(*row), map_index(col - 1, at, count, MAX_COLS).map(|v| v + 1)),
            };
            match (nr, nc) {
                (Some(nr), Some(nc)) => {
                    if nr == *row && nc == *col { return None; }
                    Some(format!("{}{}{}", fmt_sheet_prefix(sheet, tok), fmt_col(nc, *col_abs), fmt_row(nr, *row_abs)))
                }
                _ => Some("#REF!".to_string()),
            }
        }
        ReferenceType::Range {
            sheet, start_row, start_col, end_row, end_col,
            start_row_abs, start_col_abs, end_row_abs, end_col_abs,
        } => {
            let target = sheet.as_deref().unwrap_or(host);
            if !target.eq_ignore_ascii_case(edit_sheet) { return None; }
            let (mut sr, mut sc, mut er, mut ec) = (*start_row, *start_col, *end_row, *end_col);
            match axis {
                Axis::Row => {
                    if let (Some(a), Some(b)) = (sr, er) {
                        match map_span(a.min(b) - 1, a.max(b) - 1, at, count, MAX_ROWS) {
                            Some((lo, hi)) => { sr = Some(lo + 1); er = Some(hi + 1); }
                            None => return Some("#REF!".to_string()),
                        }
                    }
                }
                Axis::Col => {
                    if let (Some(a), Some(b)) = (sc, ec) {
                        match map_span(a.min(b) - 1, a.max(b) - 1, at, count, MAX_COLS) {
                            Some((lo, hi)) => { sc = Some(lo + 1); ec = Some(hi + 1); }
                            None => return Some("#REF!".to_string()),
                        }
                    }
                }
            }
            if (sr, sc, er, ec) == (*start_row, *start_col, *end_row, *end_col) { return None; }
            let one = |c: Option<u32>, r: Option<u32>, ca: bool, ra: bool| -> String {
                match (c, r) {
                    (Some(c), Some(r)) => format!("{}{}", fmt_col(c, ca), fmt_row(r, ra)),
                    (Some(c), None) => fmt_col(c, ca),
                    (None, Some(r)) => fmt_row(r, ra),
                    (None, None) => String::new(),
                }
            };
            Some(format!(
                "{}{}:{}",
                fmt_sheet_prefix(sheet, tok),
                one(sc, sr, *start_col_abs, *start_row_abs),
                one(ec, er, *end_col_abs, *end_row_abs)
            ))
        }
        _ => None,
    }
}

fn collect(
    node: &ASTNode,
    out: &mut Vec<(usize, usize, ReferenceType)>,
) {
    match &node.node_type {
        ASTNodeType::Reference { reference, .. } => {
            if let Some(t) = &node.source_token {
                out.push((t.start, t.end, reference.clone()));
            }
        }
        ASTNodeType::UnaryOp { expr, .. } => collect(expr, out),
        ASTNodeType::BinaryOp { left, right, .. } => { collect(left, out); collect(right, out); }
        ASTNodeType::Function { args, .. } => for a in args { collect(a, out); },
        ASTNodeType::Call { callee, args } => { collect(callee, out); for a in args { collect(a, out); } }
        ASTNodeType::Array(rows) => for r in rows { for i in r { collect(i, out); } },
        _ => {}
    }
}

/// 对一条公式文本应用结构编辑。`at0` 为 0 基;`count>0` 插入,`<0` 删除。
/// 无法解析时原样返回。
pub fn shift_formula(src: &str, host_sheet: &str, edit_sheet: &str, axis: Axis, at0: u32, count: i64) -> String {
    if count == 0 { return src.to_string(); }
    let had_eq = src.starts_with('=');
    let full = if had_eq { src.to_string() } else { format!("={src}") };
    let Ok(ast) = formualizer_parse::parse_with_dialect(&full, formualizer_parse::FormulaDialect::Excel) else {
        return src.to_string();
    };
    if matches!(ast.node_type, ASTNodeType::Literal(_)) { return src.to_string(); }
    let mut refs = Vec::new();
    collect(&ast, &mut refs);
    refs.sort_by_key(|r| r.0);
    let mut out = full.clone();
    for (s, e, r) in refs.into_iter().rev() {
        let Some(tok) = full.get(s..e) else { continue };
        if let Some(new) = rewrite_ref(&r, tok, host_sheet, edit_sheet, axis, at0, count) {
            out.replace_range(s..e, &new);
        }
    }
    if had_eq { out } else { out[1..].to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ins_rows(f: &str) -> String { shift_formula(f, "Sheet1", "Sheet1", Axis::Row, 2, 2) }

    #[test]
    fn insert_rows() {
        assert_eq!(ins_rows("=A1+A3+SUM(A2:A5)+Other!A3"), "=A1+A5+SUM(A2:A7)+Other!A3");
    }
    #[test]
    fn delete_rows() {
        let f = |s: &str| shift_formula(s, "Sheet1", "Sheet1", Axis::Row, 2, -2);
        assert_eq!(f("=A1+A3+SUM(A2:A5)+A6"), "=A1+#REF!+SUM(A2:A3)+A4");
    }
    #[test]
    fn delete_cols() {
        let f = |s: &str, h: &str, t: &str| shift_formula(s, h, t, Axis::Col, 0, -1);
        assert_eq!(f("=SUM(A1:C1)+B2", "S", "S"), "=SUM(A1:B1)+A2");
        assert_eq!(f("=Sheet1!B2", "Other", "Sheet1"), "=Sheet1!A2");
        assert_eq!(f("=SUM(A:A)", "S", "S"), "=SUM(#REF!)");
    }
    #[test]
    fn absolute_refs_also_shift_and_format_kept() {
        assert_eq!(ins_rows("= $A$3 + sum(B$4:C5)"), "= $A$5 + sum(B$6:C7)");
    }
    #[test]
    fn insert_inside_range_expands() {
        assert_eq!(shift_formula("=SUM(A1:A2)", "S", "S", Axis::Row, 1, 1), "=SUM(A1:A3)");
    }
    #[test]
    fn no_leading_eq_roundtrip() {
        assert_eq!(shift_formula("SUM(A1:A2)", "S", "S", Axis::Row, 1, 1), "SUM(A1:A3)");
    }
    #[test]
    fn quoted_sheet_and_whole_col_rows_untouched() {
        assert_eq!(shift_formula("='My Sheet'!B2:C3+SUM(A:A)", "S", "My Sheet", Axis::Row, 0, 1), "='My Sheet'!B3:C4+SUM(A:A)");
    }
    #[test]
    fn unparsable_unchanged() {
        assert_eq!(shift_formula("=SUM(", "S", "S", Axis::Row, 0, 1), "=SUM(");
    }
}
