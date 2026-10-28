//! ODF table:content-validation 的 condition 字符串解析/生成，以及
//! 单元格引用坐标 → 矩形 range 的合并算法。
//!
//! ODF 与 xlsx 的数据验证表达方式本质不同：
//!   xlsx: <dataValidation type="whole" operator="between" sqref="B1:B10">
//!         range 内联在规则标签上，type/operator 是独立属性
//!   ODF:  <table:content-validation table:name="val1"
//!             table:condition="oooc:cell-content-is-whole-number() and cell-content-is-between(1,100)">
//!         规则是文档级具名对象，单元格通过 table:validation-name="val1" 反向引用，
//!         range 需要从"哪些单元格引用了这个 name"反推出来。
//!
//! condition 字符串语法参考 LOK odf2uof_spreadsheet.xsl 的逐分支解析（未抄代码，
//! 语法家族照抄）：
//!   whole:      oooc:cell-content-is-whole-number() and cell-content-is-between(A,B)
//!               oooc:cell-content-is-whole-number() and cell-content()<op>N
//!   decimal:    oooc:cell-content-is-decimal-number() and cell-content-is-between(A,B)
//!               oooc:cell-content-is-decimal-number() and cell-content()<op>N
//!   textLength: oooc:cell-content-text-length()<op>N
//!               oooc:cell-content-text-length-is-between(A,B)
//!               oooc:cell-content-text-length-is-not-between(A,B)
//!   list:       oooc:cell-content-is-in-list("a";"b";"c")
//! operator 符号：<= >= < > != =
//! `oooc:` 前缀在写出时总是带上（兼容性最好），读入时两种都接受。

/// 从 ODF condition 字符串解析出 (kind, operator, formula1, formula2)。
/// 解析失败（未识别的语法）返回 None，调用方应跳过该规则。
pub fn parse_odf_condition(raw: &str) -> Option<(String, String, String, Option<String>)> {
    let s = raw.trim();
    // 去掉可能存在的 oooc: 前缀（只影响开头，函数内部的前缀在下面单独处理）
    let s = s.strip_prefix("oooc:").unwrap_or(s);

    // whole / decimal：形如 "cell-content-is-whole-number() and <rest>"
    for (kind, marker) in [
        ("whole", "cell-content-is-whole-number()"),
        ("decimal", "cell-content-is-decimal-number()"),
    ] {
        if let Some(rest) = s.strip_prefix(marker) {
            let rest = rest.trim_start().strip_prefix("and").unwrap_or(rest).trim_start();
            let rest = rest.strip_prefix("oooc:").unwrap_or(rest);
            if let Some((op, f1, f2)) = parse_cell_content_clause(rest) {
                return Some((kind.to_string(), op, f1, f2));
            }
            return None;
        }
    }

    // textLength：三种子语法
    if let Some(rest) = s.strip_prefix("cell-content-text-length-is-not-between(") {
        let (f1, f2) = split_two_args(rest)?;
        return Some(("textLength".to_string(), "notBetween".to_string(), f1, Some(f2)));
    }
    if let Some(rest) = s.strip_prefix("cell-content-text-length-is-between(") {
        let (f1, f2) = split_two_args(rest)?;
        return Some(("textLength".to_string(), "between".to_string(), f1, Some(f2)));
    }
    if let Some(rest) = s.strip_prefix("cell-content-text-length()") {
        let (op, f1) = parse_operator_value(rest)?;
        return Some(("textLength".to_string(), op, f1, None));
    }

    // list：字面量列表 cell-content-is-in-list("a";"b";"c")
    if let Some(rest) = s.strip_prefix("cell-content-is-in-list(") {
        let inner = rest.strip_suffix(')').unwrap_or(rest);
        return Some(("list".to_string(), String::new(), inner.to_string(), None));
    }

    // 顶层 between/not-between（不带 whole/decimal 前缀，泛型数值范围）
    if let Some(rest) = s.strip_prefix("cell-content-is-not-between(") {
        let (f1, f2) = split_two_args(rest)?;
        return Some(("decimal".to_string(), "notBetween".to_string(), f1, Some(f2)));
    }
    if let Some(rest) = s.strip_prefix("cell-content-is-between(") {
        let (f1, f2) = split_two_args(rest)?;
        return Some(("decimal".to_string(), "between".to_string(), f1, Some(f2)));
    }

    // 泛型单值比较 cell-content()<op>N（不带 kind 前缀，退化为 decimal）
    if let Some(rest) = s.strip_prefix("cell-content()") {
        let (op, f1) = parse_operator_value(rest)?;
        return Some(("decimal".to_string(), op, f1, None));
    }

    None
}

/// 解析 "cell-content-is-between(A,B)" / "cell-content-is-not-between(A,B)" /
/// "cell-content()<op>N" 这几种紧跟在 whole/decimal 标记之后的从句。
fn parse_cell_content_clause(rest: &str) -> Option<(String, String, Option<String>)> {
    if let Some(inner) = rest.strip_prefix("cell-content-is-not-between(") {
        let (f1, f2) = split_two_args(inner)?;
        return Some(("notBetween".to_string(), f1, Some(f2)));
    }
    if let Some(inner) = rest.strip_prefix("cell-content-is-between(") {
        let (f1, f2) = split_two_args(inner)?;
        return Some(("between".to_string(), f1, Some(f2)));
    }
    if let Some(inner) = rest.strip_prefix("cell-content()") {
        let (op, f1) = parse_operator_value(inner)?;
        return Some((op, f1, None));
    }
    None
}

/// 从 "A,B)" 形式的字符串里拆出两个逗号分隔的参数（右括号已知紧跟第二个参数之后）
fn split_two_args(s: &str) -> Option<(String, String)> {
    let s = s.trim_end_matches(')');
    let idx = s.find(',')?;
    let a = s[..idx].trim().to_string();
    let b = s[idx + 1..].trim().to_string();
    Some((a, b))
}

/// 解析 "<=N" / ">=N" / "<N" / ">N" / "!=N" / "=N" → (operator名, N)
fn parse_operator_value(s: &str) -> Option<(String, String)> {
    let s = s.trim();
    for (sym, op) in [
        ("<=", "lessThanOrEqual"),
        (">=", "greaterThanOrEqual"),
        ("!=", "notEqual"),
        ("<", "lessThan"),
        (">", "greaterThan"),
        ("=", "equal"),
    ] {
        if let Some(val) = s.strip_prefix(sym) {
            return Some((op.to_string(), val.trim().to_string()));
        }
    }
    None
}

/// 把 (kind, operator, formula1, formula2) 生成 ODF condition 字符串（写出方向）。
/// list 类型 formula1 应已是 ODF 字面量列表格式（"a";"b";"c"）的内部字符串，
/// 调用方负责把 xlsx 风格的 "a,b,c" 转换成这个格式（由 fods_writer 处理）。
pub fn build_odf_condition(kind: &str, operator: &str, formula1: &str, formula2: Option<&str>) -> Option<String> {
    let op_symbol = |op: &str| -> Option<&'static str> {
        match op {
            "lessThanOrEqual" => Some("<="),
            "greaterThanOrEqual" => Some(">="),
            "notEqual" => Some("!="),
            "lessThan" => Some("<"),
            "greaterThan" => Some(">"),
            "equal" => Some("="),
            _ => None,
        }
    };

    match kind {
        "whole" | "decimal" => {
            let type_marker = if kind == "whole" {
                "oooc:cell-content-is-whole-number()"
            } else {
                "oooc:cell-content-is-decimal-number()"
            };
            match operator {
                "between" => {
                    let f2 = formula2?;
                    Some(format!("{} and cell-content-is-between({},{})", type_marker, formula1, f2))
                }
                "notBetween" => {
                    let f2 = formula2?;
                    Some(format!("{} and cell-content-is-not-between({},{})", type_marker, formula1, f2))
                }
                _ => {
                    let sym = op_symbol(operator)?;
                    Some(format!("{} and cell-content(){}{}", type_marker, sym, formula1))
                }
            }
        }
        "textLength" => match operator {
            "between" => {
                let f2 = formula2?;
                Some(format!("oooc:cell-content-text-length-is-between({},{})", formula1, f2))
            }
            "notBetween" => {
                let f2 = formula2?;
                Some(format!("oooc:cell-content-text-length-is-not-between({},{})", formula1, f2))
            }
            _ => {
                let sym = op_symbol(operator)?;
                Some(format!("oooc:cell-content-text-length(){}{}", sym, formula1))
            }
        },
        "list" => Some(format!("oooc:cell-content-is-in-list({})", formula1)),
        _ => None, // date/time/custom/none：不生成 condition，调用方应跳过整条规则
    }
}

/// 把一组 (row, col) 坐标（同一个 validation-name 引用的所有单元格）合并成尽量少的矩形。
/// 算法：先按行分组，同一行内连续列合并成 (row, col_start, col_end) 行段；
/// 再把行段按 (col_start, col_end) 分组，连续行号且列范围相同的行段合并成矩形。
/// 不追求全局最优（最少矩形数），只做常见规则形状（整行/整列/单矩形）的合并。
pub fn merge_coords_to_rects(mut coords: Vec<(u32, u32)>) -> Vec<(u32, u32, u32, u32)> {
    // (row_start, col_start, row_end, col_end)
    if coords.is_empty() {
        return Vec::new();
    }
    coords.sort_unstable();
    coords.dedup();

    // 第一步：按行分组，同一行内连续列合并
    let mut row_spans: Vec<(u32, u32, u32)> = Vec::new(); // (row, col_start, col_end)
    let mut i = 0;
    while i < coords.len() {
        let (row, col_start) = coords[i];
        let mut col_end = col_start;
        let mut j = i + 1;
        while j < coords.len() && coords[j].0 == row && coords[j].1 == col_end + 1 {
            col_end = coords[j].1;
            j += 1;
        }
        row_spans.push((row, col_start, col_end));
        i = j;
    }

    // 第二步：按 (col_start, col_end) 分组，连续行号的行段合并成矩形
    row_spans.sort_by_key(|&(row, cs, ce)| (cs, ce, row));
    let mut rects: Vec<(u32, u32, u32, u32)> = Vec::new();
    let mut i = 0;
    while i < row_spans.len() {
        let (row_start, col_start, col_end) = row_spans[i];
        let mut row_end = row_start;
        let mut j = i + 1;
        while j < row_spans.len()
            && row_spans[j].1 == col_start
            && row_spans[j].2 == col_end
            && row_spans[j].0 == row_end + 1
        {
            row_end = row_spans[j].0;
            j += 1;
        }
        rects.push((row_start, col_start, row_end, col_end));
        i = j;
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_whole_between() {
        let r = parse_odf_condition("oooc:cell-content-is-whole-number() and cell-content-is-between(1,100)");
        assert_eq!(r, Some(("whole".to_string(), "between".to_string(), "1".to_string(), Some("100".to_string()))));
    }

    #[test]
    fn test_parse_decimal_greater_than() {
        let r = parse_odf_condition("oooc:cell-content-is-decimal-number() and cell-content()>3.14");
        assert_eq!(r, Some(("decimal".to_string(), "greaterThan".to_string(), "3.14".to_string(), None)));
    }

    #[test]
    fn test_parse_text_length_lte() {
        let r = parse_odf_condition("oooc:cell-content-text-length()<=10");
        assert_eq!(r, Some(("textLength".to_string(), "lessThanOrEqual".to_string(), "10".to_string(), None)));
    }

    #[test]
    fn test_parse_text_length_between() {
        let r = parse_odf_condition("oooc:cell-content-text-length-is-between(2,5)");
        assert_eq!(r, Some(("textLength".to_string(), "between".to_string(), "2".to_string(), Some("5".to_string()))));
    }

    #[test]
    fn test_parse_list() {
        let r = parse_odf_condition(r#"oooc:cell-content-is-in-list("苹果";"香蕉";"橙子")"#);
        assert_eq!(r, Some(("list".to_string(), String::new(), r#""苹果";"香蕉";"橙子""#.to_string(), None)));
    }

    #[test]
    fn test_roundtrip_whole_between() {
        let cond = build_odf_condition("whole", "between", "1", Some("100")).unwrap();
        let parsed = parse_odf_condition(&cond).unwrap();
        assert_eq!(parsed, ("whole".to_string(), "between".to_string(), "1".to_string(), Some("100".to_string())));
    }

    #[test]
    fn test_roundtrip_text_length_not_between() {
        let cond = build_odf_condition("textLength", "notBetween", "2", Some("5")).unwrap();
        let parsed = parse_odf_condition(&cond).unwrap();
        assert_eq!(parsed, ("textLength".to_string(), "notBetween".to_string(), "2".to_string(), Some("5".to_string())));
    }

    #[test]
    fn test_merge_single_rect() {
        let coords = vec![(0, 0), (0, 1), (1, 0), (1, 1)];
        let rects = merge_coords_to_rects(coords);
        assert_eq!(rects, vec![(0, 0, 1, 1)]);
    }

    #[test]
    fn test_merge_single_cell() {
        let coords = vec![(5, 3)];
        let rects = merge_coords_to_rects(coords);
        assert_eq!(rects, vec![(5, 3, 5, 3)]);
    }

    #[test]
    fn test_merge_disjoint_cells_stay_separate() {
        let coords = vec![(0, 0), (5, 5)];
        let rects = merge_coords_to_rects(coords);
        assert_eq!(rects.len(), 2);
        assert!(rects.contains(&(0, 0, 0, 0)));
        assert!(rects.contains(&(5, 5, 5, 5)));
    }

    #[test]
    fn test_merge_column_range() {
        // A1:A5（col=0, row 0..=4）
        let coords: Vec<(u32, u32)> = (0..5).map(|r| (r, 0)).collect();
        let rects = merge_coords_to_rects(coords);
        assert_eq!(rects, vec![(0, 0, 4, 0)]);
    }
}
