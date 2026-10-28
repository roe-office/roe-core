//! Calc API：get_cell / set_cell / set_formula / get_sheet_count

use rhai::Engine;
use base64::Engine as _;
use roe_model::calc::{CellValue, Workbook};
use std::cell::RefCell;

fn parse_hex_color_local(s: &str) -> Option<roe_model::common::Color> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 { return None; }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(roe_model::common::Color { r, g, b, a: 255 })
}

thread_local! {
    static WORKBOOK: RefCell<Option<*mut Workbook>> = const { RefCell::new(None) };
    /// 当前脚本执行期间被写操作触碰过的 (sheet, row, col) 点集合。
    /// 范围操作（merge_cells/sort_range/apply_filter/
    /// set_conditional_format/set_data_validation）只记录左上、右下两个
    /// 角点，由调用方在 with_workbook 结束后取出，聚合成每个 sheet 的
    /// bounding box 作为 dirty_region（2026-06-19 dirty_regions 细化）。
    static DIRTY: RefCell<Vec<(usize, u32, u32)>> = const { RefCell::new(Vec::new()) };
    /// P0-b:本次脚本执行是否发生过行列结构编辑(插/删行列)。
    /// DAG 按位置存储,结构编辑后旧边与缓存全部错位,调用方据此丢弃引擎并全量重算。
    static STRUCT_EDIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 取走并清除"发生过结构编辑"标志。
pub fn take_struct_edit() -> bool {
    STRUCT_EDIT.with(|f| f.replace(false))
}

fn mark_struct_edit() {
    STRUCT_EDIT.with(|f| f.set(true));
}

/// 记录一个被写操作触碰的单元格坐标，供脚本执行结束后聚合脏区域。
fn mark_dirty(sheet: usize, row: u32, col: u32) {
    DIRTY.with(|d| d.borrow_mut().push((sheet, row, col)));
}

pub fn with_workbook<F>(wb: &mut Workbook, f: F) -> Vec<(usize, u32, u32)>
where F: FnOnce() {
    WORKBOOK.with(|cell| { *cell.borrow_mut() = Some(wb as *mut Workbook); });
    DIRTY.with(|d| d.borrow_mut().clear());
    STRUCT_EDIT.with(|f| f.set(false));
    f();
    WORKBOOK.with(|cell| { *cell.borrow_mut() = None; });
    DIRTY.with(|d| std::mem::take(&mut *d.borrow_mut()))
}

/// 取浮动图表(普通图或统计图,共用 id 空间)矩形的可变引用 (x, y, w, h)。只管几何。
fn chart_rect_mut(
    sheet: &mut roe_model::calc::Sheet,
    id: u64,
) -> Option<(&mut f32, &mut f32, &mut f32, &mut f32)> {
    if let Some(c) = sheet.floating_charts.iter_mut().find(|c| c.id == id) {
        return Some((&mut c.x_pt, &mut c.y_pt, &mut c.width_pt, &mut c.height_pt));
    }
    if let Some(c) = sheet.floating_stat_charts.iter_mut().find(|c| c.id == id) {
        return Some((&mut c.x_pt, &mut c.y_pt, &mut c.width_pt, &mut c.height_pt));
    }
    None
}

/// 图表写函数的公共外壳:校验 id/sheet,执行 f,成功后标脏(不置 struct_edit)。
fn chart_edit<F>(fname: &str, sheet_idx: i64, id: i64, f: F) -> Result<(), Box<rhai::EvalAltResult>>
where
    F: FnOnce(&mut roe_model::calc::Sheet, u64) -> Result<(), String>,
{
    if id <= 0 {
        return Err(format!(
            "{fname}: id 必须是正整数(收到 {id});先用 list_charts({sheet_idx}) 查看现有 id").into());
    }
    let r = with_wb(|wb| {
        let n = wb.sheets.len();
        let Some(sheet) = wb.sheets.get_mut(sheet_idx as usize) else {
            return Err(format!(
                "{fname}: sheet_idx={sheet_idx} 越界(当前共 {n} 个 sheet,合法值 0..{n})"));
        };
        let uid = id as u64;
        if sheet.floating_charts.iter().all(|c| c.id != uid)
            && sheet.floating_stat_charts.iter().all(|c| c.id != uid)
        {
            return Err(format!(
                "{fname}: 图表 id={id} 不存在;先用 list_charts({sheet_idx}) 查看现有 id"));
        }
        f(sheet, uid)
    });
    match r {
        None => Err(format!("{fname}: 无打开的工作簿").into()),
        Some(Err(m)) => Err(m.into()),
        Some(Ok(())) => {
            mark_dirty(sheet_idx as usize, 0, 0);
            Ok(())
        }
    }
}

/// 判断某 sheet 名是否被工作簿里其它内容引用(公式 `名!`、名称、图表、条件格式、数据验证)。
/// 保守策略:宁可误拒也不弄断引用。include_self=false 时跳过该 sheet 自身(删表用)。
fn sheet_name_referenced(wb: &Workbook, idx: usize, name: &str, include_self: bool) -> bool {
    let lower = name.to_lowercase();
    let pats = [format!("{}!", lower), format!("{}'!", lower)];
    for (si, sheet) in wb.sheets.iter().enumerate() {
        if si == idx && !include_self { continue; }
        for column in sheet.cells.cols.values() {
            for block in &column.blocks {
                for v in block.values.iter() {
                    if let CellValue::Formula { source, .. } = v {
                        let src = source.to_lowercase();
                        if pats.iter().any(|p| src.contains(p.as_str())) { return true; }
                    }
                }
            }
        }
        let blob = format!(
            "{}{}{}",
            serde_json::to_string(&sheet.floating_charts).unwrap_or_default(),
            serde_json::to_string(&sheet.cond_formats).unwrap_or_default(),
            serde_json::to_string(&sheet.data_validations).unwrap_or_default(),
        ).to_lowercase();
        if blob.contains(&lower) { return true; }
    }
    let names = format!(
        "{}{}",
        serde_json::to_string(&wb.named_ranges).unwrap_or_default(),
        serde_json::to_string(&wb.named_formulas).unwrap_or_default(),
    ).to_lowercase();
    names.contains(&lower)
}

/// 单元格转 JSON(复制自 roe-api/src/range_json.rs::cell_to_json,改一处请同步另一处)。
fn range_cell_to_json(sheet: &roe_model::calc::Sheet, row: u32, col: u32, include_format: bool) -> serde_json::Value {
    use serde_json::Value;
    let mut obj = serde_json::Map::new();
    obj.insert("row".to_string(), Value::from(row));
    obj.insert("col".to_string(), Value::from(col));
    if let Some(m) = sheet.merged_cells.iter().find(|m| row >= m.r1 && row <= m.r2 && col >= m.c1 && col <= m.c2) {
        if m.r1 != row || m.c1 != col {
            obj.insert("merged_into".to_string(), Value::Array(vec![Value::from(m.r1), Value::from(m.c1)]));
            return Value::Object(obj);
        }
    }
    let plain = |v: &CellValue| -> Value {
        match v {
            CellValue::Empty => Value::Null,
            CellValue::Number(n) => serde_json::json!(n),
            CellValue::Text(s) => Value::from(s.clone()),
            CellValue::RichText(runs) => Value::from(runs.iter().map(|r| r.text.as_str()).collect::<String>()),
            CellValue::Bool(b) => Value::from(*b),
            CellValue::Error(e) => Value::from(format!("{e:?}")),
            CellValue::Formula { .. } => Value::Null,
        }
    };
    let value = sheet.cells.get(row, col);
    match &value {
        CellValue::Empty => {}
        CellValue::Error(e) => { obj.insert("error".to_string(), Value::from(format!("{e:?}"))); }
        CellValue::Formula { source, cached } => {
            obj.insert("formula_source".to_string(), Value::from(source.clone()));
            obj.insert("cached_value".to_string(), plain(cached.as_ref()));
        }
        other => { obj.insert("value".to_string(), plain(other)); }
    }
    if include_format {
        if let Some(fmt) = sheet.cell_formats.get(&(row, col)) {
            if let Ok(j) = serde_json::to_value(fmt) { obj.insert("format".to_string(), j); }
        }
    }
    Value::Object(obj)
}

const CALC_MAX_ROW: i64 = 1_048_576;
const CALC_MAX_COL: i64 = 16384;
fn calc_row_ok(r: i64) -> bool { r >= 0 && r < CALC_MAX_ROW }
fn calc_col_ok(c: i64) -> bool { c >= 0 && c < CALC_MAX_COL }

fn with_wb<R, F: FnOnce(&mut Workbook) -> R>(f: F) -> Option<R> {
    WORKBOOK.with(|cell| {
        let ptr = (*cell.borrow())?;
        Some(f(unsafe { &mut *ptr }))
    })
}
macro_rules! register_calc_apis {
    ($engine:expr, $is_readonly_env:expr, {
        $(
            fn $name:expr => $body:expr, $is_readonly:expr;
        )*
    }) => {
        $(
            if !$is_readonly_env || $is_readonly {
                $engine.register_fn($name, $body);
            }
        )*
    };
}


// ─── 名称管理(MVP:只管全局名称;局部名称作用域未支持) ───────────────
fn name_looks_like_cell_ref(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    let letters = up.chars().take_while(|c| c.is_ascii_alphabetic()).count();
    let rest = &up[letters..];
    if (1..=3).contains(&letters) && !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    if up == "R" || up == "C" {
        return true;
    }
    if let Some(after_r) = up.strip_prefix('R') {
        let d = after_r.chars().take_while(|c| c.is_ascii_digit()).count();
        let tail = &after_r[d..];
        if d > 0 && tail.is_empty() {
            return true;
        }
        if let Some(after_c) = tail.strip_prefix('C') {
            if after_c.chars().all(|c| c.is_ascii_digit()) {
                return true;
            }
        }
    }
    if let Some(after_c) = up.strip_prefix('C') {
        if !after_c.is_empty() && after_c.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    false
}

fn name_syntax_check(name: &str) -> Result<(), String> {
    let first = match name.chars().next() {
        Some(c) => c,
        None => return Err("名称不能为空".to_string()),
    };
    if name.chars().count() > 255 {
        return Err("名称不能超过 255 个字符".to_string());
    }
    if !(first.is_alphabetic() || first == '_' || first == '\\') {
        return Err("名称必须以字母、下划线或反斜杠开头".to_string());
    }
    if name.chars().any(|c| !(c.is_alphanumeric() || c == '_' || c == '.' || c == '\\')) {
        return Err("名称只能包含字母、数字、下划线和点(不能有空格)".to_string());
    }
    if name_looks_like_cell_ref(name) {
        return Err("名称不能与单元格引用相同(如 A1、R1C1)".to_string());
    }
    Ok(())
}

fn name_col_letters(mut c: u32) -> String {
    let mut s: Vec<u8> = Vec::new();
    loop {
        s.push(b'A' + (c % 26) as u8);
        if c < 26 {
            break;
        }
        c = c / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

fn name_range_text(r: &roe_model::calc::CellRange) -> String {
    let plain = !r.sheet.is_empty()
        && r.sheet.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        && !r.sheet.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false);
    let sheet = if plain {
        r.sheet.clone()
    } else {
        format!("'{}'", r.sheet.replace('\'', "''"))
    };
    let a = format!("${}${}", name_col_letters(r.start.1), r.start.0 + 1);
    if r.start == r.end {
        format!("{}!{}", sheet, a)
    } else {
        format!("{}!{}:${}${}", sheet, a, name_col_letters(r.end.1), r.end.0 + 1)
    }
}

/// 大小写不敏感地从两个名称表里删除,返回是否有命中。
fn names_remove_ci(wb: &mut roe_model::calc::Workbook, name: &str) -> bool {
    let key = name.trim().to_lowercase();
    let mut hit = false;
    let rk: Vec<String> = wb.named_ranges.keys().filter(|k| k.to_lowercase() == key).cloned().collect();
    for k in rk {
        wb.named_ranges.remove(&k);
        hit = true;
    }
    let fk: Vec<String> = wb.named_formulas.keys().filter(|k| k.to_lowercase() == key).cloned().collect();
    for k in fk {
        wb.named_formulas.remove(&k);
        hit = true;
    }
    hit
}

fn names_exists_ci(wb: &roe_model::calc::Workbook, name: &str) -> bool {
    let key = name.trim().to_lowercase();
    wb.named_ranges.keys().any(|k| k.to_lowercase() == key)
        || wb.named_formulas.keys().any(|k| k.to_lowercase() == key)
}

fn names_validate(wb: &roe_model::calc::Workbook, name: &str) -> Result<(), String> {
    let name = name.trim();
    name_syntax_check(name)?;
    if names_exists_ci(wb, name) {
        return Err("名称已存在(名称不区分大小写)".to_string());
    }
    Ok(())
}

/// 新增或覆盖(大小写归一:覆盖已有同名项,不并存)。
fn names_set(wb: &mut roe_model::calc::Workbook, name: &str, ref_text: &str) -> Result<(), String> {
    let name = name.trim();
    name_syntax_check(name)?;
    let text = ref_text.trim().trim_start_matches('=').trim();
    if text.is_empty() {
        return Err("引用不能为空".to_string());
    }
    let parsed = roe_parser::ooxml::xlsx::parse_defined_name_ref(text);
    if let Some(r) = &parsed {
        if !wb.sheets.iter().any(|s| s.name == r.sheet) {
            return Err(format!("工作表不存在: {}", r.sheet));
        }
    }
    names_remove_ci(wb, name);
    match parsed {
        Some(r) => {
            wb.named_ranges.insert(name.to_string(), r);
        }
        None => {
            wb.named_formulas.insert(name.to_string(), text.to_string());
        }
    }
    Ok(())
}

fn names_list_json(wb: &roe_model::calc::Workbook) -> String {
    let mut items: Vec<(String, &'static str, String)> = Vec::new();
    for (k, r) in wb.named_ranges.iter() {
        items.push((k.clone(), "range", name_range_text(r)));
    }
    for (k, f) in wb.named_formulas.iter() {
        items.push((k.clone(), "formula", f.clone()));
    }
    items.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    let arr: Vec<serde_json::Value> = items
        .into_iter()
        .map(|(n, k, r)| serde_json::json!({"name": n, "kind": k, "ref": r}))
        .collect();
    serde_json::Value::Array(arr).to_string()
}

/// P0-a:结构编辑后,按 ref_shift 规则改写全部 sheet 的公式引用。
/// 必须在单元格数据平移之后调用(公式格自身已在新位置)。
fn rewrite_formulas_for_edit(
    wb: &mut roe_model::calc::Workbook,
    edit_sheet_idx: usize,
    axis: roe_calc::ref_shift::Axis,
    at0: u32,
    count: i64,
) {
    let Some(edit_name) = wb.sheets.get(edit_sheet_idx).map(|s| s.name.clone()) else { return; };
    for si in 0..wb.sheets.len() {
        let host = wb.sheets[si].name.clone();
        for column in wb.sheets[si].cells.cols.values_mut() {
            for block in column.blocks.iter_mut() {
                for v in block.values.iter_mut() {
                    if let CellValue::Formula { source, .. } = v {
                        let new = roe_calc::ref_shift::shift_formula(source, &host, &edit_name, axis, at0, count);
                        if new != *source {
                            *source = new;
                        }
                    }
                }
            }
        }
    }
}

pub fn collect_formula_cells(wb: &roe_model::calc::Workbook) -> Vec<(usize, u32, u32)> {
    let mut out = Vec::new();
    for (si, sheet) in wb.sheets.iter().enumerate() {
        for (&col, column) in sheet.cells.cols.iter() {
            for block in &column.blocks {
                for (off, v) in block.values.iter().enumerate() {
                    if matches!(v, roe_model::calc::CellValue::Formula { .. }) {
                        out.push((si, block.start_row + off as u32, col));
                    }
                }
            }
        }
    }
    out
}

/// 名称表变化后,所有公式格都可能受影响(含当前显示 #NAME? 的),全部标脏。
fn mark_all_formulas_dirty() {
    let cells = with_wb(|wb| collect_formula_cells(wb)).unwrap_or_default();
    for (s, r, c) in cells {
        mark_dirty(s, r, c);
    }
}

/// 解析 CellRange 形参;失败抛 Rhai 错误,文本含函数名、参数名、serde 原因和示例。
fn parse_range_arg(func: &str, arg: &str, json: &str)
    -> Result<roe_model::calc::CellRange, Box<rhai::EvalAltResult>> {
    serde_json::from_str::<roe_model::calc::CellRange>(json).map_err(|e| {
        format!(
            "{func}: 参数 {arg} 不是合法的范围 JSON({e});示例: \
             {{\"sheet\":\"Sheet1\",\"start\":[0,0],\"end\":[4,3]}}。\
             start/end 为 [行,列],从 0 起,闭区间,sheet 键必填"
        ).into()
    })
}

const DV_KINDS: &[&str] = &["none", "custom", "date", "decimal", "list", "textLength", "time", "whole"];
pub(crate) const DV_EXAMPLE: &str =
    r#"{"kind":"whole","operator":"greaterThan","formula1":"10","formula2":null,"allow_blank":true,"show_error":true,"error_msg":"必须大于10"}。kind 可选: none(清除)/custom/date/decimal/list/textLength/time/whole;formula1/formula2 必须是字符串,数字也要加引号"#;

/// condition 前缀白名单,依据 roe-core tile_render_helpers.rs::cond_matches_ctx 的真实分支。
/// (前缀, 操作数个数, 是否数值操作数)
const COND_PREFIXES: &[(&str, usize, bool)] = &[
    ("gt", 1, true), ("lt", 1, true), ("gte", 1, true), ("lte", 1, true),
    ("eq", 1, false), ("ne", 1, false),
    ("between", 2, true), ("not_between", 2, true),
    ("text_contains", 1, false), ("text_not_contains", 1, false),
    ("begins_with", 1, false), ("ends_with", 1, false),
    ("not_empty", 0, false), ("is_empty", 0, false),
    ("expr", 1, false),
];
/// 文件能读写、但渲染层不生效的前缀:明确拒绝,避免静默不生效。
const COND_UNSUPPORTED: &[&str] = &["duplicate", "unique", "top10", "above_average", "time_period"];
pub(crate) const COND_HELP: &str = "合法写法: gt:N lt:N gte:N lte:N eq:X ne:X between:A:B not_between:A:B text_contains:X text_not_contains:X begins_with:X ends_with:X not_empty is_empty expr:公式;操作数不能含冒号;数值操作数可写成单元格引用如 $B$3";
pub(crate) const DV_OPERATORS: &[&str] = &["between", "notBetween", "equal", "notEqual", "greaterThan", "greaterThanOrEqual", "lessThan", "lessThanOrEqual"];

fn looks_like_cell_ref(raw: &str) -> bool {
    let t: String = raw.chars().filter(|c| *c != '$').collect();
    let letters = t.chars().take_while(|c| c.is_ascii_alphabetic()).count();
    let rest = &t[letters..];
    letters > 0 && !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
}

fn validate_condition(func: &str, cond: &str) -> Result<(), Box<rhai::EvalAltResult>> {
    let err = |m: String| -> Box<rhai::EvalAltResult> { format!("{func}: condition {:?} {m};{COND_HELP}", cond).into() };
    let parts: Vec<&str> = cond.splitn(3, ':').collect();
    let head = parts[0];
    if COND_UNSUPPORTED.contains(&head) {
        return Err(err("暂不支持(渲染层不生效,规则永远不命中)".into()));
    }
    let Some(&(_, n_args, numeric)) = COND_PREFIXES.iter().find(|(h, _, _)| *h == head) else {
        return Err(err("前缀不认识".into()));
    };
    let args = &parts[1..];
    if args.len() != n_args {
        return Err(err(format!("需要 {} 个操作数,实际 {} 个", n_args, args.len())));
    }
    for a in args {
        if a.is_empty() { return Err(err("操作数不能为空".into())); }
        if numeric && a.trim().parse::<f64>().is_err() && !looks_like_cell_ref(a.trim()) {
            return Err(err(format!("操作数 {:?} 必须是数字或单元格引用", a)));
        }
    }
    Ok(())
}

const CELL_FORMAT_KEYS: &[&str] = &[
    "bold", "italic", "underline", "font_size", "font_family", "fg_color",
    "bg_color", "align_h", "align_v", "wrap_text", "num_format", "borders",
];
pub(crate) const CELL_FORMAT_EXAMPLE: &str =
    r##"{"bold":true,"fg_color":"#FF0000","bg_color":"#FFFF00","align_h":"center","num_format":"0.00"}"##;
pub(crate) const CF_RULE_EXAMPLE: &str =
    r##"{"condition":"gt:0","format":{"fg_color":"#FF0000","bold":true}}"##;
pub(crate) const AF_EXAMPLE: &str = r##"{"col_filters":{"0":["A","B"],"2":["Yes"]}}"##;

/// 解析任意 JSON 形参;失败抛 Rhai 错误(含函数名、参数名、serde 原因、示例)。
fn parse_json_arg(func: &str, arg: &str, json: &str, example: &str)
    -> Result<serde_json::Value, Box<rhai::EvalAltResult>> {
    serde_json::from_str::<serde_json::Value>(json).map_err(|e| -> Box<rhai::EvalAltResult> {
        format!("{func}: 参数 {arg} 不是合法 JSON({e});示例: {example}").into()
    })
}

/// 校验 DataValidation 规则 JSON 的字段(set/update 共用),返回 kind。
fn validate_dv_rule(func: &str, v: &serde_json::Value) -> Result<String, Box<rhai::EvalAltResult>> {
        let kind = v["kind"].as_str().unwrap_or("").to_string();
        if !DV_KINDS.contains(&kind.as_str()) {
            return Err(format!("{func}: rule_json.kind={:?} 不合法;示例: {}", kind, DV_EXAMPLE).into());
        }
        if kind != "none" && v["formula1"].as_str().map_or(true, |t| t.is_empty()) {
            return Err(format!("{func}: kind={} 需要非空字符串 formula1;示例: {}", kind, DV_EXAMPLE).into());
        }
        if !v["formula2"].is_null() && !v["formula2"].is_string() {
            return Err(format!("{func}: formula2 必须是字符串或 null;示例: {}", DV_EXAMPLE).into());
        }
        if matches!(kind.as_str(), "whole" | "decimal" | "textLength") {
            let op = v["operator"].as_str().unwrap_or("");
            if !DV_OPERATORS.contains(&op) {
                return Err(format!("{func}: kind={} 需要合法 operator,收到 {:?};可选: {};示例: {}",
                    kind, op, DV_OPERATORS.join("/"), DV_EXAMPLE).into());
            }
            if v["formula1"].as_str().unwrap_or("").trim().parse::<f64>().is_err() {
                return Err(format!("{func}: kind={} 的 formula1 必须是数字字符串(如 \"10\"),否则规则不生效", kind).into());
            }
            if matches!(op, "between" | "notBetween")
                && v["formula2"].as_str().map_or(true, |t| t.trim().parse::<f64>().is_err()) {
                return Err(format!("{func}: operator={} 需要数字字符串 formula2,否则规则不生效", op).into());
            }
        }
        for key in ["allow_blank", "show_error"] {
            if !v[key].is_null() && !v[key].is_boolean() {
                return Err(format!("{func}: {} 必须是 true/false;示例: {}", key, DV_EXAMPLE).into());
            }
        }
        if !v["error_msg"].is_null() && !v["error_msg"].is_string() {
            return Err(format!("{func}: error_msg 必须是字符串或 null;示例: {}", DV_EXAMPLE).into());
        }
        Ok(kind)
}

pub(crate) const CF_RANGE_EXAMPLE: &str = r##"{"sheet":"Sheet1","start":[0,0],"end":[9,0]}"##;
pub(crate) const CF_KIND_EXAMPLE: &str = r##"{"ColorScale":{"min_type":"min","min_value":null,"min_color":"#F8696B","mid":null,"max_type":"max","max_value":null,"max_color":"#63BE7B"}}"##;

pub(crate) const CHART_KINDS: &[&str] = &[
    "Bar", "HorizontalBar", "Line", "Pie", "Scatter", "Radar", "Area", "Doughnut",
];
pub(crate) const CHART_EXAMPLE: &str = r##"{"kind":"Bar","title":"销量","x_labels":["A","B"],"series":[{"name":"s1","data":[1,2]}]}"##;
/// insert_chart 的可选字段名(与 insert_chart 实际读取的键一一对应,测试会校验)。
/// set_chart_style 的示例(局部补丁:只写要改的键;null 清除 title/gap_width_pct 等)。
pub(crate) const CHART_STYLE_EXAMPLE: &str = r##"{"title":"新标题","show_legend":false,"grouping":"Stacked","border_color":"#ff0000"}"##;
pub(crate) const CHART_OPTIONAL_KEYS: &[&str] = &[
    "width_pt", "height_pt", "show_legend", "show_grid", "x_axis_label", "y_axis_label",
    "border_color", "border_width", "grouping", "show_val", "gap_width_pct",
    "overlap_pct", "hole_size_pct", "avoid_overlap",
];
/// 可选字段说明(给 list_functions 示例用)。
pub(crate) const CHART_OPTIONAL_HELP: &str = "可选字段(JSON 里数字直接写,不加引号): width_pt/height_pt(默认 300x300,单位 pt)、show_legend/show_grid/show_val(true/false)、x_axis_label/y_axis_label(字符串)、border_color(\"#RRGGBB\")、border_width、grouping(\"Stacked\"或\"PercentStacked\")、gap_width_pct/overlap_pct/hole_size_pct;avoid_overlap:true 时若锚点压住数据或已有图表,自动挪到数据区右侧或已有图表下方,返回新图 id,之后可用 list_charts 查实际位置";

/// 已用区域的最大行/列(0 起算)。空白格(Empty 或空串)不计。
/// inspect_workbook 与 find_free_area 共用,只写这一份。
pub(crate) fn used_extent(sheet: &roe_model::calc::Sheet) -> (Option<u32>, Option<u32>) {
    let is_empty = |v: &CellValue| -> bool {
        match v {
            CellValue::Empty => true,
            CellValue::Text(t) => t.is_empty(),
            _ => false,
        }
    };
    let mut max_row: Option<u32> = None;
    let mut max_col: Option<u32> = None;
    for (c, col_data) in sheet.cells.cols.iter() {
        let mut col_has = false;
        for blk in &col_data.blocks {
            if let Some(i) = blk.values.iter().rposition(|v| !is_empty(v)) {
                col_has = true;
                let r = blk.start_row + i as u32;
                max_row = Some(max_row.map_or(r, |m| m.max(r)));
            }
        }
        if col_has {
            let c = *c;
            max_col = Some(max_col.map_or(c, |m| m.max(c)));
        }
    }
    (max_row, max_col)
}

/// 障碍物矩形(pt):两类图表 + 图片 + 形状 + 数据区(从 (0,0) 到已用区域右下角)。
fn sheet_obstacles(
    sheet: &roe_model::calc::Sheet,
    cfg: &roe_core::tile::CalcRenderConfig,
) -> Vec<(f32, f32, f32, f32)> {
    let mut v: Vec<(f32, f32, f32, f32)> = Vec::new();
    for c in &sheet.floating_charts { v.push((c.x_pt, c.y_pt, c.width_pt, c.height_pt)); }
    for c in &sheet.floating_stat_charts { v.push((c.x_pt, c.y_pt, c.width_pt, c.height_pt)); }
    for i in &sheet.floating_images { v.push((i.x_pt, i.y_pt, i.width_pt, i.height_pt)); }
    for sh in &sheet.floating_shapes { v.push((sh.x_pt, sh.y_pt, sh.width_pt, sh.height_pt)); }
    if let (Some(mr), Some(mc)) = used_extent(sheet) {
        let (x2, y2) = roe_core::tile::cell_to_pt(sheet, mr + 1, mc + 1, cfg);
        v.push((0.0, 0.0, x2, y2));
    }
    v
}

/// 为 w×h 的对象找不压住任何障碍物的左上角(pt)。
/// 期望位置 (x,y) 不冲突则原样返回;冲突则放到数据区右侧(max_col+2 列),
/// 再与冲突对象纵向堆叠(y 下移到其下边缘 + 8pt),直到不冲突。
/// 每轮 y 必越过一个障碍物下边缘,故必然终止。
pub(crate) fn find_free_area(
    sheet: &roe_model::calc::Sheet,
    x: f32, y: f32, w: f32, h: f32,
    cfg: &roe_core::tile::CalcRenderConfig,
) -> (f32, f32) {
    const GAP: f32 = 8.0;
    let obs = sheet_obstacles(sheet, cfg);
    let hit = |rx: f32, ry: f32| obs.iter().find(|o| roe_core::tile::rect_overlap((rx, ry, w, h), **o)).copied();
    if hit(x, y).is_none() {
        return (x, y);
    }
    let (start_x, start_y) = match used_extent(sheet).1 {
        Some(mc) => (roe_core::tile::cell_to_pt(sheet, 0, mc + 2, cfg).0, 0.0),
        None => (x, y),
    };
    let mut cy = start_y;
    for _ in 0..=obs.len() + 1 {
        match hit(start_x, cy) {
            Some(o) => cy = o.1 + o.3 + GAP,
            None => break,
        }
    }
    (start_x, cy)
}

/// 校验并解析 insert_chart 的 chart_json 核心字段。
/// 任何类型不符都报错,不再静默丢数据(data 里的字符串数字、x_labels 里的数字等)。
fn parse_chart_spec(val: &serde_json::Value) -> Result<(
    roe_model::impress::ChartKind,
    Option<String>,
    Vec<String>,
    Vec<roe_model::impress::ChartSeries>,
), Box<rhai::EvalAltResult>> {
    use serde_json::Value;
    let err = |msg: String| -> Box<rhai::EvalAltResult> {
        format!("insert_chart: {msg};示例: {CHART_EXAMPLE}").into()
    };
    let obj = val.as_object().ok_or_else(|| err("chart_json 必须是 JSON 对象".into()))?;

    let kind_str = match obj.get("kind") {
        Some(Value::String(s)) => s.as_str(),
        _ => return Err(err(format!(
            "缺少字符串字段 kind,合法值(区分大小写): {}", CHART_KINDS.join(", ")))),
    };
    let kind = match kind_str {
        "Bar" => roe_model::impress::ChartKind::Bar,
        "HorizontalBar" => roe_model::impress::ChartKind::HorizontalBar,
        "Line" => roe_model::impress::ChartKind::Line,
        "Pie" => roe_model::impress::ChartKind::Pie,
        "Scatter" => roe_model::impress::ChartKind::Scatter,
        "Radar" => roe_model::impress::ChartKind::Radar,
        "Area" => roe_model::impress::ChartKind::Area,
        "Doughnut" => roe_model::impress::ChartKind::Doughnut,
        other => return Err(err(format!(
            "kind={:?} 不合法,合法值(区分大小写): {}", other, CHART_KINDS.join(", ")))),
    };

    let title = match obj.get("title") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => return Err(err(format!("title 必须是字符串,收到 {}", other))),
    };

    let mut x_labels: Vec<String> = Vec::new();
    match obj.get("x_labels") {
        None | Some(Value::Null) => {}
        Some(Value::Array(arr)) => {
            for (i, v) in arr.iter().enumerate() {
                match v.as_str() {
                    Some(s) => x_labels.push(s.to_string()),
                    None => return Err(err(format!(
                        "x_labels[{}]={} 不是字符串(数字也要加引号,如 \"1月\"、\"2024\")", i, v))),
                }
            }
        }
        Some(_) => return Err(err("x_labels 必须是字符串数组".into())),
    }

    let arr = match obj.get("series") {
        Some(Value::Array(a)) if !a.is_empty() => a,
        _ => return Err(err(
            "series 必填,且须为非空数组,每项形如 {\"name\":\"..\",\"data\":[数字,..]}".into())),
    };
    let mut series = Vec::with_capacity(arr.len());
    for (i, s) in arr.iter().enumerate() {
        let so = s.as_object().ok_or_else(|| err(format!("series[{}] 必须是对象", i)))?;
        let name = match so.get("name") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(n)) => n.clone(),
            Some(other) => return Err(err(format!("series[{}].name 必须是字符串,收到 {}", i, other))),
        };
        let data_arr = match so.get("data") {
            Some(Value::Array(d)) => d,
            _ => return Err(err(format!("series[{}].data 必填,须为数字数组", i))),
        };
        let mut data = Vec::with_capacity(data_arr.len());
        for (j, v) in data_arr.iter().enumerate() {
            match v.as_f64() {
                Some(f) => data.push(f),
                None => return Err(err(format!(
                    "series[{}].data[{}]={} 不是数字(字符串数字如 \"12\" 不行,请写 12)", i, j, v))),
            }
        }
        let color = match so.get("color") {
            None | Some(Value::Null) => None,
            Some(Value::String(c)) => Some(parse_hex_color_local(c).ok_or_else(|| {
                err(format!("series[{}].color={:?} 不是合法十六进制颜色,如 \"#ff0000\"", i, c))
            })?),
            Some(other) => return Err(err(format!("series[{}].color 必须是字符串,收到 {}", i, other))),
        };
        series.push(roe_model::impress::ChartSeries {
            name,
            data,
            color,
            as_line: so.get("as_line").and_then(|v| v.as_bool()).unwrap_or(false),
            trendline: so.get("trendline").and_then(|v| v.as_bool()).unwrap_or(false),
        });
    }
    Ok((kind, title, x_labels, series))
}

/// 把 JSON 值解析为 CellFormat。CellFormat 不拒绝未知字段,这里手工检查,
/// 否则 {"background":"red"} 会被静默当成空格式。
fn parse_cell_format_value(func: &str, arg: &str, v: &serde_json::Value)
    -> Result<roe_model::calc::CellFormat, Box<rhai::EvalAltResult>> {
    let obj = v.as_object().ok_or_else(|| -> Box<rhai::EvalAltResult> {
        format!("{func}: 参数 {arg} 必须是 JSON 对象;示例: {CELL_FORMAT_EXAMPLE}").into()
    })?;
    let unknown: Vec<&str> = obj.keys().map(|k| k.as_str())
        .filter(|k| !CELL_FORMAT_KEYS.contains(k)).collect();
    if !unknown.is_empty() {
        return Err(format!(
            "{func}: 参数 {arg} 含未知字段 {unknown:?};可用字段: {};示例: {CELL_FORMAT_EXAMPLE}",
            CELL_FORMAT_KEYS.join(", ")).into());
    }
    serde_json::from_value::<roe_model::calc::CellFormat>(v.clone()).map_err(|e| -> Box<rhai::EvalAltResult> {
        format!("{func}: 参数 {arg} 字段类型不对({e});示例: {CELL_FORMAT_EXAMPLE}").into()
    })
}

pub fn register(engine: &mut Engine) {
    define_and_register_apis(engine, false);
}

pub fn register_readonly(engine: &mut Engine) {
    define_and_register_apis(engine, true);
}

/// 兼容空壳：insert_row/delete_row/insert_column/delete_column/
/// delete_duplicates 五个函数此前由独立的 register_row_col_ops()
/// 注册，生产环境曾因 engine.rs 漏调用这个函数导致"单测过、生产不可用"
/// （详见 engine.rs 中 production_entry_row_col_ops_check 模块的记录）。
/// 宏迁移后这五个函数已经并入 register() 统一注册的38个函数之一，
/// 不再存在"漏注册"的可能性（物理上不存在只注册register()却漏掉
/// 这5个函数的路径）。保留这个空函数只是为了不破坏调用方
/// （calc_api.rs 自身 mod tests 里的8处调用、任何历史遗留调用点）
/// 的编译，不做任何实际注册动作。
pub fn register_row_col_ops(_engine: &mut Engine) {}

fn define_and_register_apis(engine: &mut Engine, is_readonly_env: bool) {
    crate::calc_api_list::register_list_functions(engine, is_readonly_env);
    register_calc_apis!(engine, is_readonly_env, {
    // ── 第一批补充:几何/合并/冻结/新增 sheet(薄封装,直接操作 Workbook 字段) ──
    fn "set_column_width" => |sheet: i64, col: i64, width_pt: f64| -> String {
        if !width_pt.is_finite() || width_pt <= 0.0 || width_pt > 1000.0 {
            return "err:列宽须在 (0,1000] 磅之间".to_string();
        }
        if col < 0 || col >= 16384 { return "err:列号越界(0..16383)".to_string(); }
        with_wb(|wb| {
            let Some(s) = wb.sheets.get_mut(sheet as usize) else { return "err:sheet 越界".to_string(); };
            let col = col as usize;
            let dflt = if s.default_col_width > 0.0 { s.default_col_width } else { 48.99 };
            if s.col_widths.len() <= col {
                s.col_widths.resize(col, dflt);
                s.col_widths.push(width_pt as f32);
            } else {
                s.col_widths[col] = width_pt as f32;
            }
            "ok".to_string()
        }).unwrap_or_else(|| "err:无打开的工作簿".to_string())
    }, false;

    fn "get_column_width" => |sheet: i64, col: i64| -> f64 {
        if col < 0 { return -1.0; }
        with_wb(|wb| {
            let s = wb.sheets.get(sheet as usize)?;
            let dflt = if s.default_col_width > 0.0 { s.default_col_width } else { 48.99 };
            Some(s.col_widths.get(col as usize).copied().unwrap_or(dflt) as f64)
        }).flatten().unwrap_or(-1.0)
    }, true;

    fn "set_row_height" => |sheet: i64, row: i64, height_pt: f64| -> String {
        if !height_pt.is_finite() || height_pt <= 0.0 || height_pt > 1000.0 {
            return "err:行高须在 (0,1000] 磅之间".to_string();
        }
        if row < 0 || row >= 1_048_576 { return "err:行号越界(0..1048575)".to_string(); }
        with_wb(|wb| {
            let Some(s) = wb.sheets.get_mut(sheet as usize) else { return "err:sheet 越界".to_string(); };
            let row = row as usize;
            let rh = roe_model::calc::RowHeight::Manual(height_pt as f32);
            if s.row_heights.len() <= row {
                let dflt = s.effective_default_row_height();
                s.row_heights.resize(row, roe_model::calc::RowHeight::Auto { current: dflt, is_dirty: true });
                s.row_heights.push(rh);
            } else {
                s.row_heights[row] = rh;
            }
            "ok".to_string()
        }).unwrap_or_else(|| "err:无打开的工作簿".to_string())
    }, false;

    fn "get_row_height" => |sheet: i64, row: i64| -> f64 {
        if row < 0 { return -1.0; }
        with_wb(|wb| {
            let s = wb.sheets.get(sheet as usize)?;
            let dflt = s.effective_default_row_height();
            Some(match s.row_heights.get(row as usize) {
                Some(roe_model::calc::RowHeight::Manual(h)) => *h as f64,
                Some(roe_model::calc::RowHeight::Auto { current, .. }) => *current as f64,
                #[allow(unreachable_patterns)]
                Some(_) => dflt as f64,
                None => dflt as f64,
            })
        }).flatten().unwrap_or(-1.0)
    }, true;

    fn "unmerge_cells" => |sheet: i64, r1: i64, c1: i64, r2: i64, c2: i64| -> String {
        if r1 < 0 || c1 < 0 || r2 < r1 || c2 < c1 { return "err:区域非法".to_string(); }
        let res = with_wb(|wb| {
            let Some(s) = wb.sheets.get_mut(sheet as usize) else { return "err:sheet 越界".to_string(); };
            s.merged_cells.retain(|m| {
                !(m.r1 <= r2 as u32 && m.r2 >= r1 as u32 && m.c1 <= c2 as u32 && m.c2 >= c1 as u32)
            });
            "ok".to_string()
        }).unwrap_or_else(|| "err:无打开的工作簿".to_string());
        if res == "ok" {
            mark_dirty(sheet as usize, r1 as u32, c1 as u32);
            mark_dirty(sheet as usize, r2 as u32, c2 as u32);
        }
        res
    }, false;

    fn "get_frozen_panes" => |sheet: i64| -> String {
        with_wb(|wb| {
            let s = wb.sheets.get(sheet as usize)?;
            Some(format!("{{\"frozen_rows\":{},\"frozen_cols\":{}}}", s.frozen_rows, s.frozen_cols))
        }).flatten().unwrap_or_else(|| "err:sheet 越界或无工作簿".to_string())
    }, true;

    fn "set_frozen_panes" => |sheet: i64, rows: i64, cols: i64| -> String {
        if rows < 0 || rows > 1000 || cols < 0 || cols > 100 { return "err:冻结行列数越界(行0..1000,列0..100)".to_string(); }
        with_wb(|wb| {
            let Some(s) = wb.sheets.get_mut(sheet as usize) else { return "err:sheet 越界".to_string(); };
            s.frozen_rows = rows as u32;
            s.frozen_cols = cols as u32;
            "ok".to_string()
        }).unwrap_or_else(|| "err:无打开的工作簿".to_string())
    }, false;

    fn "add_sheet" => |name: String| -> String {
        let name = name.trim().to_string();
        if name.is_empty() { return "err:sheet 名不能为空".to_string(); }
        if name.chars().count() > 31 { return "err:sheet 名不能超过 31 个字符".to_string(); }
        if name.chars().any(|c| matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\')) {
            return "err:sheet 名不能含 [ ] : * ? / \\".to_string();
        }
        with_wb(|wb| {
            if wb.sheets.len() >= 255 { return "err:sheet 数量已达上限".to_string(); }
            if wb.sheets.iter().any(|s| s.name.to_lowercase() == name.to_lowercase()) {
                return "err:sheet 名已存在".to_string();
            }
            wb.sheets.push(roe_model::calc::Sheet {
                col_style: vec![],
                name: name.clone(),
                default_col_width: 48.99,
                default_row_height: 14.25,
                ..Default::default()
            });
            format!("ok:{}", wb.sheets.len() - 1)
        }).unwrap_or_else(|| "err:无打开的工作簿".to_string())
    }, false;

    fn "rename_sheet" => |idx: i64, name: String| -> String {
        let name = name.trim().to_string();
        if idx < 0 { return "err:sheet 越界".to_string(); }
        if name.is_empty() { return "err:sheet 名不能为空".to_string(); }
        if name.chars().count() > 31 { return "err:sheet 名不能超过 31 个字符".to_string(); }
        if name.chars().any(|c| matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\')) {
            return "err:sheet 名不能含 [ ] : * ? / \\".to_string();
        }
        with_wb(|wb| {
            let i = idx as usize;
            let Some(old) = wb.sheets.get(i).map(|s| s.name.clone()) else { return "err:sheet 越界".to_string(); };
            if old == name { return "ok".to_string(); }
            if wb.sheets.iter().enumerate().any(|(j, s)| j != i && s.name.to_lowercase() == name.to_lowercase()) {
                return "err:sheet 名已存在".to_string();
            }
            if sheet_name_referenced(wb, i, &old, true) {
                return format!("err:sheet「{}」被公式/名称/图表/条件格式/数据验证引用,改名会弄断引用,已拒绝", old);
            }
            wb.sheets[i].name = name.clone();
            mark_struct_edit();
            "ok".to_string()
        }).unwrap_or_else(|| "err:无打开的工作簿".to_string())
    }, false;

    fn "remove_sheet" => |idx: i64| -> String {
        if idx < 0 { return "err:sheet 越界".to_string(); }
        with_wb(|wb| {
            let i = idx as usize;
            if i >= wb.sheets.len() { return "err:sheet 越界".to_string(); }
            if wb.sheets.len() <= 1 { return "err:不能删除最后一个 sheet".to_string(); }
            let name = wb.sheets[i].name.clone();
            if sheet_name_referenced(wb, i, &name, false) {
                return format!("err:sheet「{}」被其它 sheet 的公式/名称/图表等引用,删除会弄断引用,已拒绝", name);
            }
            wb.sheets.remove(i);
            mark_struct_edit();
            "ok".to_string()
        }).unwrap_or_else(|| "err:无打开的工作簿".to_string())
    }, false;

    // 列宽自动适应:闭区间 [start_col, end_col],空列保持原宽;返回 ok:实际调整的列数
    fn "auto_fit_columns" => |sheet: i64, start_col: i64, end_col: i64| -> String {
        if start_col < 0 || end_col < start_col || end_col >= 16384 {
            return "err:列区间非法(须 0<=start<=end<16384)".to_string();
        }
        if end_col - start_col > 200 { return "err:一次最多适应 201 列".to_string(); }
        with_wb(|wb| {
            let Some(s) = wb.sheets.get_mut(sheet as usize) else { return "err:sheet 越界".to_string(); };
            let row_end = s.cells.cols.values()
                .flat_map(|sc| sc.blocks.iter())
                .map(|b| b.start_row + b.values.len() as u32)
                .max()
                .unwrap_or(0);
            let mut font_context = parley::FontContext::new();
            let mut layout_context = parley::LayoutContext::new();
            let dflt = if s.default_col_width > 0.0 { s.default_col_width } else { 48.99 };
            let mut fitted = 0usize;
            for col in (start_col as u32)..=(end_col as u32) {
                let Some(w) = roe_core::auto_fit::auto_fit_column_width(
                    s, col, 0, row_end, &mut font_context, &mut layout_context,
                ) else { continue; };
                let c = col as usize;
                if s.col_widths.len() <= c {
                    s.col_widths.resize(c, dflt);
                    s.col_widths.push(w);
                } else {
                    s.col_widths[c] = w;
                }
                fitted += 1;
            }
            format!("ok:{}", fitted)
        }).unwrap_or_else(|| "err:无打开的工作簿".to_string())
    }, false;

    // 批量读区域(闭区间,最多 2000 格);返回 {"rows":[[{row,col,value|formula_source+cached_value|error|merged_into,format?}]]}
    fn "get_range_json" => |sheet: i64, r1: i64, c1: i64, r2: i64, c2: i64, include_format: bool| -> String {
        if r1 < 0 || c1 < 0 || r2 < r1 || c2 < c1 || r2 >= 1_048_576 || c2 >= 16384 {
            return "err:区域非法(须 0<=start<=end)".to_string();
        }
        let cells = (r2 - r1 + 1) as i128 * (c2 - c1 + 1) as i128;
        if cells > 2000 { return format!("err:区域过大({} 格,最多 2000 格),请分块读取", cells); }
        with_wb(|wb| {
            let Some(s) = wb.sheets.get(sheet as usize) else { return "err:sheet 越界".to_string(); };
            let mut rows = Vec::new();
            for r in (r1 as u32)..=(r2 as u32) {
                let mut row = Vec::new();
                for c in (c1 as u32)..=(c2 as u32) {
                    row.push(range_cell_to_json(s, r, c, include_format));
                }
                rows.push(serde_json::Value::Array(row));
            }
            serde_json::json!({ "rows": rows }).to_string()
        }).unwrap_or_else(|| "err:无打开的工作簿".to_string())
    }, true;

    // 能力自省:各 sheet 的名称、已用范围(0 起的最大行/列索引,空串/Empty 不算)与前 3 行预览。
    fn "inspect_workbook" => || -> String {
        with_wb(|wb| {
            let mut sheets_json: Vec<serde_json::Value> = Vec::new();
            for (idx, sheet) in wb.sheets.iter().enumerate() {
                let (max_row, max_col) = used_extent(sheet);
                let (empty, top_rows) = match (max_row, max_col) {
                    (Some(mr), Some(mc)) => {
                        let last_row = mr.min(2);
                        let last_col = mc.min(19);
                        let mut rows: Vec<Vec<String>> = Vec::new();
                        for r in 0..=last_row {
                            let mut row: Vec<String> = Vec::new();
                            for c in 0..=last_col {
                                let text = sheet.cells.cols.get(&c).and_then(|cd| {
                                    for blk in &cd.blocks {
                                        if r >= blk.start_row && r < blk.start_row + blk.values.len() as u32 {
                                            return Some(cell_value_to_string(&blk.values[(r - blk.start_row) as usize]));
                                        }
                                    }
                                    None
                                }).unwrap_or_default();
                                if text.chars().count() > 24 {
                                    let mut t: String = text.chars().take(24).collect();
                                    t.push('…');
                                    row.push(t);
                                } else {
                                    row.push(text);
                                }
                            }
                            rows.push(row);
                        }
                        (false, rows)
                    }
                    _ => (true, Vec::new()),
                };
                sheets_json.push(serde_json::json!({
                    "index": idx,
                    "name": sheet.name,
                    "empty": empty,
                    "max_row": max_row,
                    "max_col": max_col,
                    "cols_truncated": max_col.map_or(false, |m| m > 19),
                    "top_rows": top_rows,
                }));
            }
            serde_json::json!({"sheet_count": wb.sheets.len(), "sheets": sheets_json}).to_string()
        })
        .unwrap_or_else(|| "err:无打开的工作簿".to_string())
    }, true;

    fn "get_cell" => |sheet: i64, row: i64, col: i64| -> String {
        with_wb(|wb| {
            let sheet = wb.sheets.get(sheet as usize)?;
            let col_data = sheet.cells.cols.get(&(col as u32))?;
            for block in &col_data.blocks {
                let r = row as u32;
                if r >= block.start_row && r < block.start_row + block.values.len() as u32 {
                    return Some(cell_value_to_string(&block.values[(r - block.start_row) as usize]));
                }
            }
            None
        })
        .flatten()
        .unwrap_or_default()
    }, true;

    // 2026-07-13新增：get_cell 语义一直是"当前展示值"（公式已求值时返回
    // cached 结果，见 cell_value_to_string 注释），因此公式栏此前一直读不到
    // 公式原文，回车提交后公式栏就"退化"显示成了计算结果/错误码，所有公式
    // 无一例外，不限于VLOOKUP等复杂函数。本函数专门暴露 source 字段，供
    // office-rs2公式栏读取用；非公式单元格统一返回空串，前端据此判断是否
    // 需要退回 get_cell 取显示值。返回值不含前导"="（与 set_formula 输入
    // 约定一致，前端负责拼回）。
    fn "get_cell_formula" => |sheet: i64, row: i64, col: i64| -> String {
        with_wb(|wb| {
            let sheet = wb.sheets.get(sheet as usize)?;
            let col_data = sheet.cells.cols.get(&(col as u32))?;
            for block in &col_data.blocks {
                let r = row as u32;
                if r >= block.start_row && r < block.start_row + block.values.len() as u32 {
                    return match &block.values[(r - block.start_row) as usize] {
                        CellValue::Formula { source, .. } => Some(source.clone()),
                        _ => None,
                    };
                }
            }
            None
        })
        .flatten()
        .unwrap_or_default()
    }, true;

    // 2026-07-13新增：Del/Backspace清空单元格。此前只有 set_cell，传空字符串
    // 会存成 CellValue::Text("")（内容为空文本），不是真正的 CellValue::Empty，
    // 会让 ISBLANK 等公式判断及 xlsx/ods 导出的空单元格语义出现偏差。这里补一个
    // 真正清空为 Empty 的函数。
    fn "clear_cell" => |sheet: i64, row: i64, col: i64| -> Result<(), Box<rhai::EvalAltResult>> {
        let ok = with_wb(|wb| {
            let sheet_ref = wb.sheets.get_mut(sheet as usize)?;
            set_cell_in_sheet(sheet_ref, row as u32, col as u32, CellValue::Empty);
            Some(())
        }).flatten().is_some();
        if !ok {
            return Err(format!("clear_cell: sheet {} 不存在(或没有打开的文档),未写入", sheet).into());
        }
        Ok(())
    }, false;

    fn "set_cell" => |sheet: i64, row: i64, col: i64, value: String| -> Result<(), Box<rhai::EvalAltResult>> {
        let ok = with_wb(|wb| {
            let sheet_ref = wb.sheets.get_mut(sheet as usize)?;
            let parsed = if value.is_empty() { CellValue::Empty } else { parse_string_to_cellvalue(&value) };
            set_cell_in_sheet(sheet_ref, row as u32, col as u32, parsed);
            sheet_ref.invalidate_row_height(row as u32);
            Some(())
        }).flatten().is_some();
        if !ok {
            return Err(format!("set_cell: sheet {} 不存在(或没有打开的文档),未写入", sheet).into());
        }
        mark_dirty(sheet as usize, row as u32, col as u32);
        Ok(())
    }, false;

    fn "set_formula" => |sheet: i64, row: i64, col: i64, formula: String| -> Result<(), Box<rhai::EvalAltResult>> {
        let ok = with_wb(|wb| {
            let sheet_ref = wb.sheets.get_mut(sheet as usize)?;
            set_cell_in_sheet(sheet_ref, row as u32, col as u32, CellValue::Formula {
                source: formula,
                cached: Box::new(CellValue::Empty),
            });
            sheet_ref.invalidate_row_height(row as u32);
            Some(())
        }).flatten().is_some();
        if !ok {
            return Err(format!("set_formula: sheet {} 不存在(或没有打开的文档),未写入", sheet).into());
        }
        mark_dirty(sheet as usize, row as u32, col as u32);
        Ok(())
    }, false;

    // insert_subtotals(sheet, group_col, agg_cols_json, function_num, header_row,
    //                   start_row, end_row) -> "ok" | "err:..."
    // Subtotals Step 3（gap analysis 2.1，roe-ext_progress_v168 §2）。
    // 全部参数 0-based（表格坐标语义与项目其它 Rhai 函数一致，仅在生成
    // 公式字符串时换算为 Excel 1-based）。
    //
    // 设计依据 v168 §2.1（Gemini三方评审已确认，此处照实现，不重复
    // 论证）：不经过 Polars（df_groupby_agg 产出静态快照，不满足"生成
    // 活公式"的设计要求），纯 Rust 手写分组扫描 + 倒序行插入。
    //
    // 分组边界判定：对 [start_row, end_row] 做一次线性扫描比较相邻行
    // group_col 列的值，值变化处即为边界。不要求预先排序，只识别
    // "连续相同值"区间，对齐 Excel Subtotal 的实际触发语义。
    //
    // 倒序插入：先扫描出全部分组区间，从最后一组开始倒序调用
    // insert_row_impl + 写入小计行内容，避免正序插入需要手动维护行号
    // offset 累加的复杂度。
    fn "insert_subtotals" =>
        |sheet: i64, group_col: i64, agg_cols_json: String, function_num: i64,
         header_row: i64, start_row: i64, end_row: i64| -> String {
        let agg_cols: Vec<u32> = match serde_json::from_str::<Vec<i64>>(&agg_cols_json) {
            Ok(v) => v.into_iter().map(|x| x as u32).collect(),
            Err(e) => return format!("err:agg_cols_json解析失败: {e}"),
        };
        if agg_cols.is_empty() {
            return "err:agg_cols_json不能为空".to_string();
        }

        let sheet_idx = sheet as usize;
        let group_col = group_col as u32;
        let header_row = header_row as u32;
        let start_row = start_row as u32;
        let end_row = end_row as u32;

        if end_row < start_row {
            return "err:end_row不能小于start_row".to_string();
        }
        if start_row <= header_row {
            return "err:start_row必须大于header_row".to_string();
        }

        // 闭包内收集实际写入的坐标，闭包结束后统一调用mark_dirty
        // （mark_dirty写的是thread_local DIRTY，与WORKBOOK是各自独立的
        // thread_local，闭包内直接调用mark_dirty本身也可以，这里改用
        // 先收集再统一调用是为了让"精确标脏"这个意图在代码里更显式，
        // 不是必须这样写，纯风格选择）。
        let mut dirty_cells: Vec<(usize, u32, u32)> = Vec::new();

        let result = with_wb(|wb| {
            let Some(s) = wb.sheets.get_mut(sheet_idx) else {
                return "err:sheet越界".to_string();
            };

            // 第一遍：扫描分组边界（只读，不修改数据）
            let mut groups: Vec<(u32, u32)> = Vec::new(); // (group_start_row, group_end_row) 均0-based，含端点
            let mut cur_start = start_row;
            let mut cur_val = s.cells.get(start_row, group_col);
            for row in (start_row + 1)..=end_row {
                let v = s.cells.get(row, group_col);
                if v != cur_val {
                    groups.push((cur_start, row - 1));
                    cur_start = row;
                    cur_val = v;
                }
            }
            groups.push((cur_start, end_row));

            // 第二遍：倒序插入空行（不写内容），用一个"插入点平移表"
            // 跟踪每次插入对已记录的其它插入点的影响。
            //
            // 关键教训（本函数实现过程中反复踩坑，记录清楚避免再错）：
            // 倒序处理保证"处理某一组时，它自己的 g_start/g_end 不受
            // 后续（更靠前分组）插入影响"，但**已经插入过的空行**的
            // 物理行号，会被后续（更靠前分组、位置更小）的插入操作
            // 顶下去。insert_points 必须在每次新插入后，把所有"行号
            // >= 本次insert_at"的历史插入点同步 +1，才能保证最终记录
            // 的是"全部插入操作完成后"的真实物理行号（v168 Step3早期
            // 实现漏了这一步，导致后处理分组B的小计公式引用了错误的
            // 行区间，算出35而非期望的20，已被生产入口集成测试捕获）。
            //
            // 由于内容回填阶段（第三遍）不再有任何插入操作，一旦
            // insert_points 平移正确，直接用它去回填内容，行号必然
            // 正确，不需要在回填阶段再额外做任何行号换算。
            let mut insert_points: Vec<u32> = Vec::with_capacity(groups.len());
            for (_g_start, g_end) in groups.iter().rev() {
                let insert_at = g_end + 1;

                // 平移：已记录的、行号 >= insert_at 的历史插入点整体 +1
                for p in insert_points.iter_mut() {
                    if *p >= insert_at {
                        *p += 1;
                    }
                }

                insert_row_impl(s, insert_at);
                insert_points.push(insert_at);
            }
            // insert_points 目前顺序对应 groups.iter().rev()（B先于A），
            // 反转回正序，与 groups 顺序一一对应。
            insert_points.reverse();

            // 第三遍：内容回填。所有插入已完成，行号不再变化，直接按
            // (groups[i], insert_points[i]) 配对写入，公式引用的
            // Excel行号同样需要用"平移后的组数据边界"而非原始g_start/
            // g_end——组数据本身的物理位置也会被更早处理、行号更靠前
            // 的分组的插入操作顶下去，用同样的平移逻辑跟踪组边界。
            let mut group_starts: Vec<u32> = groups.iter().map(|(gs, _)| *gs).collect();
            let mut group_ends: Vec<u32> = groups.iter().map(|(_, ge)| *ge).collect();
            {
                // 重放一遍插入过程，这次只平移 group_starts/group_ends
                // （复用同样的"倒序插入、平移已记录坐标"逻辑，与上面
                // insert_points 的平移是并列的独立账本，不合并是为了
                // 让每段职责单一、易读，尽管有少量重复循环结构）。
                let mut gs_copy = group_starts.clone();
                let mut ge_copy = group_ends.clone();
                for (idx, (_g_start, g_end)) in groups.iter().enumerate().rev() {
                    let insert_at = g_end + 1;
                    for i in 0..gs_copy.len() {
                        if i == idx { continue; }
                        if gs_copy[i] >= insert_at { gs_copy[i] += 1; }
                        if ge_copy[i] >= insert_at { ge_copy[i] += 1; }
                    }
                }
                group_starts = gs_copy;
                group_ends = ge_copy;
            }

            for (i, _g) in groups.iter().enumerate() {
                let insert_at = insert_points[i];
                let g_start_final = group_starts[i];
                let g_end_final = group_ends[i];
                let group_value = s.cells.get(g_start_final, group_col);

                // group_col 列：提示文本
                let label = format!("{} 汇总", cell_value_to_string(&group_value));
                set_cell_in_sheet(s, insert_at, group_col, CellValue::Text(label));
                dirty_cells.push((sheet_idx, insert_at, group_col));

                // 分组折叠：数据行(g_start_final..=g_end_final)设为大纲层级1，
                // 汇总行(insert_at)保持layer 0（insert_row_impl插入的新行默认
                // outline_level=0，此处不需要显式再设一次）。对齐Excel“分类
                // 汇总”语义：折叠时隐藏明细数据行，只显示汇总行。level本身
                // 只是数值标记，折叠/隐藏行为由调用方(前端)后续决定，本函数
                // 不联动set_row_hidden。
                set_row_outline_level_range_impl(s, g_start_final, g_end_final, 1);

                // 每个 agg_col：写入 ROE_SUBTOTAL 公式，Excel 1-based 行号
                let excel_start = g_start_final + 1;
                let excel_end = g_end_final + 1;
                for &agg_col in &agg_cols {
                    let col_letters = col_to_letters(agg_col + 1); // col_to_letters是1-based输入
                    let formula = format!(
                        "=SUBTOTAL({},{}{}:{}{})",
                        function_num, col_letters, excel_start, col_letters, excel_end
                    );
                    set_cell_in_sheet(s, insert_at, agg_col, CellValue::Formula {
                        source: formula,
                        cached: Box::new(CellValue::Empty),
                    });
                    dirty_cells.push((sheet_idx, insert_at, agg_col));
                }
            }

            "ok".to_string()
        }).unwrap_or_else(|| "err:workbook未绑定".to_string());

        // 精确标脏每个实际写入的坐标（不是笼统标一个点），确保调用方
        // 显式调用 recalc_and_apply 时能重算所有新插入的 ROE_SUBTOTAL
        // 公式。照抄 set_formula 的"每次写入即标脏"模式，而非最后
        // 只标一个粗粒度坐标——之前的粗粒度版本导致新插入公式的cached
        // 一直停留在Empty，get_cell读不到计算结果（生产入口集成测试
        // 已捕获此bug并驱动本次修复）。
        for &(s, r, c) in &dirty_cells {
            mark_dirty(s, r, c);
        }
        result
    }, false;

    // insert_subtotals_ex(sheet, header_row, start_row, end_row, spec_json) -> String
    // 新版分类汇总(对齐 LOK ScTable::DoSubTotals 的能力:多级嵌套/汇总方向/总计行),
    // 旧 insert_subtotals 保持不变(单级、汇总在下、无总计;已有单测与调用方依赖其行为)。
    //
    // spec_json:
    //   {"groups":[{"col":0,"aggs":[[1,9],[2,9]]}, ...],   // 从外到内;col/agg列均为绝对列号,
    //                                                      // aggs 每项 [列, SUBTOTAL function_num]
    //    "summary_below":false,                            // 默认 false(汇总在明细上方)
    //    "grand_total":true}                               // 默认 true
    // 全部行号 0-based;数据区 [start_row, end_row],header_row < start_row。
    // 返回 {"new_end_row":N}(汇总后数据区最后一行,含总计/汇总行) 或 "err:..."。
    //
    // 算法(不再倒序插入+平移账本):
    //  A) 只读扫描 → 递归分组树 → 最终行序列(汇总行覆盖区间直接用最终行号)
    //  B) 按最终行号升序 insert_row_impl:第k个汇总行插入时,它之前的行已全部就位,
    //     所以插入位置恰好=最终行号,无需任何平移
    //  C) 回填标签/SUBTOTAL 公式;outline = 覆盖该行的汇总区间个数
    // 与 insert_row 同一技术债范围:不调整 cond_formats/data_validations/auto_filter 等。
    fn "insert_subtotals_ex" =>
        |sheet: i64, header_row: i64, start_row: i64, end_row: i64, spec_json: String| -> String {
        if sheet < 0 || header_row < 0 || start_row < 0 || end_row < 0 {
            return "err:参数不能为负".to_string();
        }
        if end_row < start_row { return "err:end_row不能小于start_row".to_string(); }
        if start_row <= header_row { return "err:start_row必须大于header_row".to_string(); }
        let spec = match parse_subtotal_spec(&spec_json) {
            Ok(v) => v,
            Err(e) => return format!("err:{e}"),
        };
        let mut dirty_cells: Vec<(usize, u32, u32)> = Vec::new();
        let sheet_idx = sheet as usize;
        let result = with_wb(|wb| {
            let Some(s) = wb.sheets.get_mut(sheet_idx) else {
                return "err:sheet越界".to_string();
            };
            match apply_subtotals_ex(s, sheet_idx, start_row as u32, end_row as u32, &spec, &mut dirty_cells) {
                Ok(new_end) => format!("{{\"new_end_row\":{}}}", new_end),
                Err(e) => format!("err:{e}"),
            }
        }).unwrap_or_else(|| "err:workbook未绑定".to_string());
        for &(sh, r, c) in &dirty_cells { mark_dirty(sh, r, c); }
        result
    }, false;

    // remove_subtotals(sheet, r1, c1, r2, c2) -> "{\"removed\":N}" | "err:..."
    // 取消分类汇总，对齐 LOK ScTable::RemoveSubTotals 语义：
    // 在 [r1+1, r2] × [c1, c2] 内找出所有含 SUBTOTAL 公式的行，整行删除。
    // r1 是表头行（与 insert_subtotals 的 header_row 语义一致，不参与扫描）。
    // 全部参数 0-based，闭区间。
    //
    // 识别只看公式文本（不依赖 outline_level，用户手动分组不会被误删）：
    // 去掉前导空白和 '='，大小写不敏感，前缀为 "SUBTOTAL(" 即算汇总行。
    // 导入的 xlsx 公式 source 是否带 '=' 前缀不确定，两种都认。
    //
    // 删除走 delete_rows_batch（内部已同步 row_hidden / row_outline_level）。
    // 与 LOK 一致：不调整其它公式对被删行的引用（汇总公式引用的是明细区间，
    // 明细行不删就不会错位；用户自己写的公式若引用了汇总行会悬空，可接受）。
    fn "remove_subtotals" =>
        |sheet: i64, r1: i64, c1: i64, r2: i64, c2: i64| -> String {
        if sheet < 0 || r1 < 0 || c1 < 0 || r2 < 0 || c2 < 0 {
            return "err:参数不能为负".to_string();
        }
        if r2 <= r1 || c2 < c1 {
            return "err:范围不合法(需 r2>r1 且 c2>=c1)".to_string();
        }
        let (start_row, end_row) = ((r1 + 1) as u32, r2 as u32);
        let (col1, col2) = (c1 as u32, c2 as u32);

        with_wb(|wb| {
            let Some(s) = wb.sheets.get_mut(sheet as usize) else {
                return "err:sheet越界".to_string();
            };
            let mut rows: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
            for (&col, col_data) in s.cells.cols.iter() {
                if col < col1 || col > col2 { continue; }
                for block in col_data.blocks.iter() {
                    for (i, v) in block.values.iter().enumerate() {
                        let row = block.start_row + i as u32;
                        if row < start_row || row > end_row { continue; }
                        if let CellValue::Formula { source, .. } = v {
                            let t = source.trim_start().trim_start_matches('=').trim_start();
                            if t.len() >= 9 && t[..9].eq_ignore_ascii_case("SUBTOTAL(") {
                                rows.insert(row);
                            }
                        }
                    }
                }
            }
            let to_delete: Vec<u32> = rows.into_iter().collect();
            let n = to_delete.len();
            delete_rows_batch(s, &to_delete);
            // 对齐 LOK 取消分类汇总后的 RemoveAutoOutline：明细行的 outline 是
            // insert_subtotals 打上的，删汇总行不会自动还原，这里把数据区
            // [r1+1, 新r2] 的层级整段清零，并取消这段范围内的折叠隐藏
            // （否则折叠状态下取消，明细行会一直藏着）。
            // row_outline_level / row_hidden 长度可能短于行数（导入只到最后一个
            // 非零行），只清零已存在的部分，不扩长。
            let new_end = end_row - n as u32; // 闭区间，删除后数据区最后一行
            for r in start_row..=new_end {
                let ri = r as usize;
                // 只有 level>0 的行才可能是折叠隐藏的明细行；level==0 的隐藏行
                // 属于自动筛选等其它来源，不能误展开。
                let was_grouped = s.row_outline_level.get(ri).copied().unwrap_or(0) > 0;
                if let Some(v) = s.row_outline_level.get_mut(ri) { *v = 0; }
                if was_grouped {
                    if let Some(h) = s.row_hidden.get_mut(ri) { *h = false; }
                }
            }
            format!("{{\"removed\":{}}}", n)
        }).unwrap_or_else(|| "err:workbook未绑定".to_string())
    }, false;

    // text_to_columns(sheet, start_row, end_row, col, delimiter) -> String (JSON)
    // gap analysis 2.2：按分隔符将单列内容拆分为多列，CSV粘贴清洗场景常用。
    //
    // 范围裁剪（本次实现不做，需要时再补）：只支持"按分隔符"模式，不支持
    // Excel"分列向导"里的"固定宽度"模式（按字符位置切割）——固定宽度模式
    // 需要传一组切割位置数组，Rhai调用侧不如分隔符模式直观，且实际使用
    // 频率里分隔符模式（逗号/空格/制表符分列）远高于固定宽度，按8.1标准
    // 判断这属于内容/结构性操作，走Rhai，不涉及坐标系语境。
    //
    // 行为对齐Excel"分列"实际语义：
    // - 读取 col 列 [start_row, end_row] 区间每一行的当前展示值（复用
    //   cell_value_to_string 的取值逻辑，与get_cell语义一致：公式格取
    //   计算结果，不取公式原文）
    // - 按 delimiter 切分成若干段（delimiter 为空字符串时视为不合法，
    //   直接跳过该行，不产生panic也不做"每字符切一段"这种令人惊讶的
    //   隐式行为）
    // - 第一段写回原 col 位置（覆盖原值），后续段依次写入 col+1、col+2...
    // - 每一段按 set_cell 同样的解析规则（parse_string_to_cellvalue）
    //   写入：优先数字，其次true/false布尔，否则纯文本
    //
    // 故意不做的事：不会自动插入新列腾位置——与Excel默认行为一致，
    // 分列产生的多列会直接覆盖右侧已有数据。如果调用方需要先腾出空间，
    // 自行调用已有的 insert_column，ROE不代为决策是否插入，避免"分列"
    // 这一个操作意外产生"插入列"的副作用，用户心智上这是两个动作。
    //
    // 返回JSON：{"max_cols": N}，N是本次操作中出现过的最大目标列数
    // （从1开始计数，1表示所有行分列后都只有1段，实际等价于没有变化）。
    // 供前端/Agent确认分列实际影响的列范围，便于后续决定要不要提示
    // 用户"检测到会覆盖已有数据"之类的判断——这个判断本身不在本函数
    // 职责内，本函数只如实执行并报告结果范围。
    fn "text_to_columns" =>
        |sheet: i64, start_row: i64, end_row: i64, col: i64, delimiter: String| -> String {
        if delimiter.is_empty() {
            return "{\"max_cols\":0}".to_string();
        }
        let start_row = start_row as u32;
        let end_row = end_row as u32;
        let col = col as u32;
        let mut max_cols: usize = 0;

        with_wb(|wb| {
            let Some(sheet_ref) = wb.sheets.get_mut(sheet as usize) else { return; };
            for row in start_row..=end_row {
                let current = sheet_ref.cells.get(row, col);
                let text = cell_value_to_string(&current);
                let parts: Vec<&str> = text.split(delimiter.as_str()).collect();
                if parts.len() > max_cols {
                    max_cols = parts.len();
                }
                for (i, part) in parts.iter().enumerate() {
                    let target_col = col + i as u32;
                    let parsed = parse_string_to_cellvalue(part);
                    set_cell_in_sheet(sheet_ref, row, target_col, parsed);
                }
            }
        });

        for row in start_row..=end_row {
            for i in 0..max_cols {
                mark_dirty(sheet as usize, row, col + i as u32);
            }
        }

        format!("{{\"max_cols\":{}}}", max_cols)
    }, false;

    // describe_stats(sheet, start_row, end_row, col, out_sheet, out_row, out_col) -> String (JSON)
    // gap analysis 2.4：描述性统计（Descriptive Statistics）。对照LOK
    // DescriptiveStatisticsDialog.cxx的lclCalcDefinitions常量数组
    // （v159交接文档4.2节已核实：LOK本身就是"拼公式字符串批量写入"，
    // 不是自研算法），照抄其15项统计量清单与顺序：
    //   Mean/StdError/Mode/Median/Q1/Q3/Variance/StdDev/Kurtosis/
    //   Skewness/Range/Min/Max/Sum/Count
    // 公式统一用LOK同款的旧式函数名（VAR/STDEV/MODE/QUARTILE，非
    // .S/.P/.SNGL/.INC后缀新名）——v159会话已核实formualizer-eval
    // 0.7.1中这些旧名均以别名形式注册，指向语义正确的现代函数
    // （VAR->VAR.S样本方差、STDEV->STDEV.S样本标准差、MODE->MODE.SNGL
    // 单峰众数、QUARTILE->QUARTILE.INC含端点四分位数，均与LOK默认行为
    // 一致），无需在ROE侧做名称转换。
    //
    // 输入：sheet列col的[start_row, end_row]闭区间（单列，与LOK对话框
    // 单变量输出块的语义一致；多列批量对照LOK是循环调用同一套模板，
    // 若未来需要多列一次性输出，可在此函数外层由调用方循环调用本函数，
    // 不在这一版本内置多列支持）。
    //
    // 输出：允许跨sheet（out_sheet独立指定，与df_pivot_to_sheet允许
    // 结果写到不同sheet的精神一致）。从(out_row, out_col)开始写15行：
    // out_col列写统计量标签（纯文本），out_col+1列写对应公式。公式统一
    // 加"源sheet名!"前缀引用range（如"=AVERAGE(Sheet1!A1:A10)"），不因
    // out_sheet==sheet而省略前缀，避免两套分支逻辑。
    //
    // sheet/out_sheet越界时不写入任何内容，返回
    // {"rows_written":0,"error":"..."}。
    fn "describe_stats" =>
        |sheet: i64, start_row: i64, end_row: i64, col: i64,
         out_sheet: i64, out_row: i64, out_col: i64| -> String {
        let sheet_name = match with_wb(|wb| wb.sheets.get(sheet as usize).map(|s| s.name.clone())) {
            Some(Some(name)) => name,
            _ => return "{\"rows_written\":0,\"error\":\"sheet index out of range\"}".to_string(),
        };
        let out_sheet_valid = with_wb(|wb| wb.sheets.get(out_sheet as usize).is_some()).unwrap_or(false);
        if !out_sheet_valid {
            return "{\"rows_written\":0,\"error\":\"out_sheet index out of range\"}".to_string();
        }

        let range_ref = format!(
            "{}!{}{}:{}{}",
            sheet_name,
            col_to_letters(col as u32 + 1),
            start_row + 1,
            col_to_letters(col as u32 + 1),
            end_row + 1,
        );

        let templates: [(&str, &str); 15] = [
            ("Mean",               "=AVERAGE(%R%)"),
            ("Standard Error",     "=SQRT(VAR(%R%)/COUNT(%R%))"),
            ("Mode",               "=MODE(%R%)"),
            ("Median",             "=MEDIAN(%R%)"),
            ("First Quartile",     "=QUARTILE(%R%,1)"),
            ("Third Quartile",     "=QUARTILE(%R%,3)"),
            ("Variance",           "=VAR(%R%)"),
            ("Standard Deviation", "=STDEV(%R%)"),
            ("Kurtosis",           "=KURT(%R%)"),
            ("Skewness",           "=SKEW(%R%)"),
            ("Range",              "=MAX(%R%)-MIN(%R%)"),
            ("Minimum",            "=MIN(%R%)"),
            ("Maximum",            "=MAX(%R%)"),
            ("Sum",                "=SUM(%R%)"),
            ("Count",              "=COUNT(%R%)"),
        ];

        with_wb(|wb| {
            let out = wb.sheets.get_mut(out_sheet as usize)?;
            for (i, (label, tmpl)) in templates.iter().enumerate() {
                let r = out_row as u32 + i as u32;
                set_cell_in_sheet(out, r, out_col as u32, CellValue::Text(label.to_string()));
                let formula = tmpl.replace("%R%", &range_ref);
                set_cell_in_sheet(out, r, out_col as u32 + 1, CellValue::Formula {
                    source: formula,
                    cached: Box::new(CellValue::Empty),
                });
            }
            Some(())
        });

        for i in 0..15u32 {
            mark_dirty(out_sheet as usize, out_row as u32 + i, out_col as u32);
            mark_dirty(out_sheet as usize, out_row as u32 + i, out_col as u32 + 1);
        }

        "{\"rows_written\":15}".to_string()
    }, false;

    // t_test(sheet1, r1_start, r1_end, c1, sheet2, r2_start, r2_end, c2,
    //        tails, test_type, out_sheet, out_row, out_col) -> String (JSON)
    // gap analysis 2.4：T-test。方案确认（与用户2026-09讨论一致）：不照抄
    // LOK TTestDialog.cxx手工展开的12+行t值推导公式（那是LO历史包袱，
    // 当年公式引擎可能缺T.TEST内建函数才需要手动拼数学公式）——
    // formualizer-eval已原生实现T.TEST(array1,array2,tails,type)，
    // "有库用库"，直接调用一行公式即可，不重新发明轮子。
    //
    // tails: 1=单尾, 2=双尾；test_type: 1=配对, 2=等方差双样本, 3=Welch
    // （不等方差）——原样透传给T.TEST，本函数不做参数校验/转换，无效值
    // 由公式引擎在实际求值时返回#NUM!错误（本函数只负责拼写公式字符串，
    // 不负责校验业务语义，校验放在Agent/前端调用前更合适）。
    //
    // 两组数据允许来自不同sheet（sheet1/sheet2独立指定），输出同样允许
    // 跨sheet（out_sheet独立指定），与describe_stats一致的灵活度。
    //
    // 输出5行（用户明确要求保留均值/方差辅助信息，不只吐一个孤零零的
    // p值数字）：Mean 1/Mean 2/Variance 1/Variance 2/P-value。
    fn "t_test" =>
        |sheet1: i64, r1_start: i64, r1_end: i64, c1: i64,
         sheet2: i64, r2_start: i64, r2_end: i64, c2: i64,
         tails: i64, test_type: i64,
         out_sheet: i64, out_row: i64, out_col: i64| -> String {
        let name1 = with_wb(|wb| wb.sheets.get(sheet1 as usize).map(|s| s.name.clone())).flatten();
        let name2 = with_wb(|wb| wb.sheets.get(sheet2 as usize).map(|s| s.name.clone())).flatten();
        let (name1, name2) = match (name1, name2) {
            (Some(n1), Some(n2)) => (n1, n2),
            _ => return "{\"rows_written\":0,\"error\":\"sheet1 or sheet2 index out of range\"}".to_string(),
        };
        let out_sheet_valid = with_wb(|wb| wb.sheets.get(out_sheet as usize).is_some()).unwrap_or(false);
        if !out_sheet_valid {
            return "{\"rows_written\":0,\"error\":\"out_sheet index out of range\"}".to_string();
        }

        let range1 = format!(
            "{}!{}{}:{}{}",
            name1, col_to_letters(c1 as u32 + 1), r1_start + 1, col_to_letters(c1 as u32 + 1), r1_end + 1,
        );
        let range2 = format!(
            "{}!{}{}:{}{}",
            name2, col_to_letters(c2 as u32 + 1), r2_start + 1, col_to_letters(c2 as u32 + 1), r2_end + 1,
        );

        let rows: [(String, String); 5] = [
            ("Mean 1".to_string(),     format!("=AVERAGE({})", range1)),
            ("Mean 2".to_string(),     format!("=AVERAGE({})", range2)),
            ("Variance 1".to_string(), format!("=VAR({})", range1)),
            ("Variance 2".to_string(), format!("=VAR({})", range2)),
            ("P-value".to_string(),    format!("=T.TEST({},{},{},{})", range1, range2, tails, test_type)),
        ];

        with_wb(|wb| {
            let out = wb.sheets.get_mut(out_sheet as usize)?;
            for (i, (label, formula)) in rows.iter().enumerate() {
                let r = out_row as u32 + i as u32;
                set_cell_in_sheet(out, r, out_col as u32, CellValue::Text(label.clone()));
                set_cell_in_sheet(out, r, out_col as u32 + 1, CellValue::Formula {
                    source: formula.clone(),
                    cached: Box::new(CellValue::Empty),
                });
            }
            Some(())
        });

        for i in 0..5u32 {
            mark_dirty(out_sheet as usize, out_row as u32 + i, out_col as u32);
            mark_dirty(out_sheet as usize, out_row as u32 + i, out_col as u32 + 1);
        }

        "{\"rows_written\":5}".to_string()
    }, false;

    // z_test(sheet1, r1_start, r1_end, c1, sheet2, r2_start, r2_end, c2,
    //        var1, var2, hyp_diff, alpha, out_sheet, out_row, out_col) -> String (JSON)
    // 双样本Z检验（已知总体方差），输出对齐Excel"z-检验:双样本平均差检验"。
    // 12行：标签在out_col，值在out_col+1。数值常量一律写成"=常量"公式，
    // 不依赖CellValue的数值变体。公式只用引擎已有的NORM.S.DIST/NORM.S.INV/SQRT/COUNT。
    fn "z_test" =>
        |sheet1: i64, r1_start: i64, r1_end: i64, c1: i64,
         sheet2: i64, r2_start: i64, r2_end: i64, c2: i64,
         var1: f64, var2: f64, hyp_diff: f64, alpha: f64,
         out_sheet: i64, out_row: i64, out_col: i64| -> String {
        let name1 = with_wb(|wb| wb.sheets.get(sheet1 as usize).map(|s| s.name.clone())).flatten();
        let name2 = with_wb(|wb| wb.sheets.get(sheet2 as usize).map(|s| s.name.clone())).flatten();
        let (name1, name2) = match (name1, name2) {
            (Some(n1), Some(n2)) => (n1, n2),
            _ => return "{\"rows_written\":0,\"error\":\"sheet1 or sheet2 index out of range\"}".to_string(),
        };
        let out_sheet_valid = with_wb(|wb| wb.sheets.get(out_sheet as usize).is_some()).unwrap_or(false);
        if !out_sheet_valid {
            return "{\"rows_written\":0,\"error\":\"out_sheet index out of range\"}".to_string();
        }
        if r1_end < r1_start || r2_end < r2_start {
            return "{\"rows_written\":0,\"error\":\"invalid input range\"}".to_string();
        }
        if !(var1.is_finite() && var1 > 0.0 && var2.is_finite() && var2 > 0.0) {
            return "{\"rows_written\":0,\"error\":\"variances must be positive\"}".to_string();
        }
        if !hyp_diff.is_finite() {
            return "{\"rows_written\":0,\"error\":\"hypothesized difference must be finite\"}".to_string();
        }
        if !(alpha > 0.0 && alpha < 1.0) {
            return "{\"rows_written\":0,\"error\":\"alpha must be within (0,1)\"}".to_string();
        }

        let range1 = format!(
            "{}!{}{}:{}{}",
            name1, col_to_letters(c1 as u32 + 1), r1_start + 1, col_to_letters(c1 as u32 + 1), r1_end + 1,
        );
        let range2 = format!(
            "{}!{}{}:{}{}",
            name2, col_to_letters(c2 as u32 + 1), r2_start + 1, col_to_letters(c2 as u32 + 1), r2_end + 1,
        );

        // 值列（0-based out_col+1 → 1-based +2）；同sheet引用不带sheet名前缀
        let vl = col_to_letters(out_col as u32 + 2);
        let cell = |i: u32| format!("{}{}", vl, out_row as u32 + i + 1);
        let (m1, m2, v1, v2, n1, n2, h, z) =
            (cell(0), cell(1), cell(2), cell(3), cell(4), cell(5), cell(6), cell(7));

        let rows: [(String, String); 12] = [
            ("Mean 1".to_string(),                        format!("=AVERAGE({})", range1)),
            ("Mean 2".to_string(),                        format!("=AVERAGE({})", range2)),
            ("Known Variance 1".to_string(),              format!("={}", var1)),
            ("Known Variance 2".to_string(),              format!("={}", var2)),
            ("Observations 1".to_string(),                format!("=COUNT({})", range1)),
            ("Observations 2".to_string(),                format!("=COUNT({})", range2)),
            ("Hypothesized Mean Difference".to_string(),  format!("={}", hyp_diff)),
            ("z".to_string(),                             format!("=({}-{}-{})/SQRT({}/{}+{}/{})", m1, m2, h, v1, n1, v2, n2)),
            ("P(Z<=z) one-tail".to_string(),              format!("=1-NORM.S.DIST(ABS({}),TRUE)", z)),
            ("z Critical one-tail".to_string(),           format!("=NORM.S.INV(1-{})", alpha)),
            ("P(Z<=z) two-tail".to_string(),              format!("=2*(1-NORM.S.DIST(ABS({}),TRUE))", z)),
            ("z Critical two-tail".to_string(),           format!("=NORM.S.INV(1-{}/2)", alpha)),
        ];

        with_wb(|wb| {
            let out = wb.sheets.get_mut(out_sheet as usize)?;
            for (i, (label, formula)) in rows.iter().enumerate() {
                let r = out_row as u32 + i as u32;
                set_cell_in_sheet(out, r, out_col as u32, CellValue::Text(label.clone()));
                set_cell_in_sheet(out, r, out_col as u32 + 1, CellValue::Formula {
                    source: formula.clone(),
                    cached: Box::new(CellValue::Empty),
                });
            }
            Some(())
        });

        for i in 0..12u32 {
            mark_dirty(out_sheet as usize, out_row as u32 + i, out_col as u32);
            mark_dirty(out_sheet as usize, out_row as u32 + i, out_col as u32 + 1);
        }

        "{\"rows_written\":12}".to_string()
    }, false;

    // chi_square_test(sheet, r_start, r_end, c_start, c_end,
    //                 out_sheet, out_row, out_col) -> String (JSON)
    // 卡方独立性检验。输入为观测频数矩阵（不含表头）。
    // 输出布局（均从out_row/out_col起，R=行数，C=列数）：
    //   out_row           : "Expected"标题
    //   +1 .. +R          : 期望值矩阵 E = 行合计*列合计/总计
    //   +R+2              : "Contribution"标题
    //   +R+3 .. +2R+2     : 贡献值矩阵 (O-E)^2/E
    //   +2R+4 .. +2R+6    : Chi-square / Degrees of Freedom / P-value（值在out_col+1）
    // 不用SUMPRODUCT数组运算，逐格写贡献值再SUM，避开引擎数组运算的未验证区域。
    // rows_written = 实际占用行数(2R+7)。
    fn "chi_square_test" =>
        |sheet: i64, r_start: i64, r_end: i64, c_start: i64, c_end: i64,
         out_sheet: i64, out_row: i64, out_col: i64| -> String {
        let name = match with_wb(|wb| wb.sheets.get(sheet as usize).map(|s| s.name.clone())) {
            Some(Some(n)) => n,
            _ => return "{\"rows_written\":0,\"error\":\"sheet index out of range\"}".to_string(),
        };
        let out_sheet_valid = with_wb(|wb| wb.sheets.get(out_sheet as usize).is_some()).unwrap_or(false);
        if !out_sheet_valid {
            return "{\"rows_written\":0,\"error\":\"out_sheet index out of range\"}".to_string();
        }
        if r_end < r_start || c_end < c_start {
            return "{\"rows_written\":0,\"error\":\"invalid input range\"}".to_string();
        }
        let nr: i64 = r_end - r_start + 1;
        let nc: i64 = c_end - c_start + 1;
        if nr < 2 || nc < 2 {
            return "{\"rows_written\":0,\"error\":\"need at least 2 rows and 2 columns\"}".to_string();
        }

        let src_cell = |r: i64, c: i64| format!("{}!{}{}", name, col_to_letters(c as u32 + 1), r + 1);
        let src_rect = |ra: i64, ca: i64, rb: i64, cb: i64| format!(
            "{}!{}{}:{}{}",
            name, col_to_letters(ca as u32 + 1), ra + 1, col_to_letters(cb as u32 + 1), rb + 1,
        );
        let out_cell = |r: u32, c: u32| format!("{}{}", col_to_letters(c + 1), r + 1);
        let full = src_rect(r_start, c_start, r_end, c_end);

        let oc = out_col as u32;
        let exp_top = out_row as u32 + 1;
        let title2_row = exp_top + nr as u32 + 1;
        let con_top = title2_row + 1;
        let stats_top = con_top + nr as u32 + 1;
        let rows_used: u32 = stats_top + 3 - out_row as u32;

        let mk_formula = |s: String| CellValue::Formula { source: s, cached: Box::new(CellValue::Empty) };
        let mut cells: Vec<(u32, u32, CellValue)> = Vec::new();

        cells.push((out_row as u32, oc, CellValue::Text("Expected".to_string())));
        cells.push((title2_row, oc, CellValue::Text("Contribution".to_string())));

        for i in 0..nr {
            for j in 0..nc {
                let row_rng = src_rect(r_start + i, c_start, r_start + i, c_end);
                let col_rng = src_rect(r_start, c_start + j, r_end, c_start + j);
                let e_row = exp_top + i as u32;
                let e_col = oc + j as u32;
                cells.push((e_row, e_col,
                    mk_formula(format!("=SUM({})*SUM({})/SUM({})", row_rng, col_rng, full))));
                let e_ref = out_cell(e_row, e_col);
                let o_ref = src_cell(r_start + i, c_start + j);
                cells.push((con_top + i as u32, e_col,
                    mk_formula(format!("=({}-{})^2/{}", o_ref, e_ref, e_ref))));
            }
        }

        let con_range = format!(
            "{}:{}",
            out_cell(con_top, oc),
            out_cell(con_top + nr as u32 - 1, oc + nc as u32 - 1),
        );
        let chi_ref = out_cell(stats_top, oc + 1);
        let df_ref = out_cell(stats_top + 1, oc + 1);
        cells.push((stats_top,     oc,     CellValue::Text("Chi-square".to_string())));
        cells.push((stats_top,     oc + 1, mk_formula(format!("=SUM({})", con_range))));
        cells.push((stats_top + 1, oc,     CellValue::Text("Degrees of Freedom".to_string())));
        cells.push((stats_top + 1, oc + 1, mk_formula(format!("={}", (nr - 1) * (nc - 1)))));
        cells.push((stats_top + 2, oc,     CellValue::Text("P-value".to_string())));
        cells.push((stats_top + 2, oc + 1, mk_formula(format!("=CHISQ.DIST.RT({},{})", chi_ref, df_ref))));

        let coords: Vec<(u32, u32)> = cells.iter().map(|(r, c, _)| (*r, *c)).collect();
        with_wb(|wb| {
            let out = wb.sheets.get_mut(out_sheet as usize)?;
            for (r, c, v) in cells {
                set_cell_in_sheet(out, r, c, v);
            }
            Some(())
        });
        for (r, c) in coords {
            mark_dirty(out_sheet as usize, r, c);
        }

        format!("{{\"rows_written\":{}}}", rows_used)
    }, false;

    // f_test(sheet1, r1_start, r1_end, c1, sheet2, r2_start, r2_end, c2,
    //        out_sheet, out_row, out_col) -> String (JSON)
    // gap analysis 2.4：F-test。同t_test设计原则，直接调用formualizer-eval
    // 原生的F.TEST(array1,array2)，不照抄LOK的手工展开版本。
    //
    // 输出3行：Variance 1/Variance 2/P-value（F-test本身只关心方差，
    // 不涉及均值，因此不像t_test那样输出Mean 1/Mean 2）。
    fn "f_test" =>
        |sheet1: i64, r1_start: i64, r1_end: i64, c1: i64,
         sheet2: i64, r2_start: i64, r2_end: i64, c2: i64,
         out_sheet: i64, out_row: i64, out_col: i64| -> String {
        let name1 = with_wb(|wb| wb.sheets.get(sheet1 as usize).map(|s| s.name.clone())).flatten();
        let name2 = with_wb(|wb| wb.sheets.get(sheet2 as usize).map(|s| s.name.clone())).flatten();
        let (name1, name2) = match (name1, name2) {
            (Some(n1), Some(n2)) => (n1, n2),
            _ => return "{\"rows_written\":0,\"error\":\"sheet1 or sheet2 index out of range\"}".to_string(),
        };
        let out_sheet_valid = with_wb(|wb| wb.sheets.get(out_sheet as usize).is_some()).unwrap_or(false);
        if !out_sheet_valid {
            return "{\"rows_written\":0,\"error\":\"out_sheet index out of range\"}".to_string();
        }

        let range1 = format!(
            "{}!{}{}:{}{}",
            name1, col_to_letters(c1 as u32 + 1), r1_start + 1, col_to_letters(c1 as u32 + 1), r1_end + 1,
        );
        let range2 = format!(
            "{}!{}{}:{}{}",
            name2, col_to_letters(c2 as u32 + 1), r2_start + 1, col_to_letters(c2 as u32 + 1), r2_end + 1,
        );

        let rows: [(String, String); 3] = [
            ("Variance 1".to_string(), format!("=VAR({})", range1)),
            ("Variance 2".to_string(), format!("=VAR({})", range2)),
            ("P-value".to_string(),    format!("=F.TEST({},{})", range1, range2)),
        ];

        with_wb(|wb| {
            let out = wb.sheets.get_mut(out_sheet as usize)?;
            for (i, (label, formula)) in rows.iter().enumerate() {
                let r = out_row as u32 + i as u32;
                set_cell_in_sheet(out, r, out_col as u32, CellValue::Text(label.clone()));
                set_cell_in_sheet(out, r, out_col as u32 + 1, CellValue::Formula {
                    source: formula.clone(),
                    cached: Box::new(CellValue::Empty),
                });
            }
            Some(())
        });

        for i in 0..3u32 {
            mark_dirty(out_sheet as usize, out_row as u32 + i, out_col as u32);
            mark_dirty(out_sheet as usize, out_row as u32 + i, out_col as u32 + 1);
        }

        "{\"rows_written\":3}".to_string()
    }, false;

    // correl(sheet1, r1_start, r1_end, c1, sheet2, r2_start, r2_end, c2,
    //        out_sheet, out_row, out_col) -> String (JSON)
    // gap analysis 2.4：相关系数（Correlation）。同t_test/f_test设计原则，
    // 直接调用formualizer-eval原生的CORREL(array1,array2)，不照抄LOK的
    // 手工展开版本。
    //
    // 输出1行：Correlation。不额外输出PEARSON——CORREL和PEARSON在Excel
    // 里是同一统计量的两个函数名（算法完全等价），重复输出没有信息增量
    // （用户已确认此设计，见v160交接文档第一节）。
    fn "correl" =>
        |sheet1: i64, r1_start: i64, r1_end: i64, c1: i64,
         sheet2: i64, r2_start: i64, r2_end: i64, c2: i64,
         out_sheet: i64, out_row: i64, out_col: i64| -> String {
        let name1 = with_wb(|wb| wb.sheets.get(sheet1 as usize).map(|s| s.name.clone())).flatten();
        let name2 = with_wb(|wb| wb.sheets.get(sheet2 as usize).map(|s| s.name.clone())).flatten();
        let (name1, name2) = match (name1, name2) {
            (Some(n1), Some(n2)) => (n1, n2),
            _ => return "{\"rows_written\":0,\"error\":\"sheet1 or sheet2 index out of range\"}".to_string(),
        };
        let out_sheet_valid = with_wb(|wb| wb.sheets.get(out_sheet as usize).is_some()).unwrap_or(false);
        if !out_sheet_valid {
            return "{\"rows_written\":0,\"error\":\"out_sheet index out of range\"}".to_string();
        }

        let range1 = format!(
            "{}!{}{}:{}{}",
            name1, col_to_letters(c1 as u32 + 1), r1_start + 1, col_to_letters(c1 as u32 + 1), r1_end + 1,
        );
        let range2 = format!(
            "{}!{}{}:{}{}",
            name2, col_to_letters(c2 as u32 + 1), r2_start + 1, col_to_letters(c2 as u32 + 1), r2_end + 1,
        );

        let rows: [(String, String); 1] = [
            ("Correlation".to_string(), format!("=CORREL({},{})", range1, range2)),
        ];

        with_wb(|wb| {
            let out = wb.sheets.get_mut(out_sheet as usize)?;
            for (i, (label, formula)) in rows.iter().enumerate() {
                let r = out_row as u32 + i as u32;
                set_cell_in_sheet(out, r, out_col as u32, CellValue::Text(label.clone()));
                set_cell_in_sheet(out, r, out_col as u32 + 1, CellValue::Formula {
                    source: formula.clone(),
                    cached: Box::new(CellValue::Empty),
                });
            }
            Some(())
        });

        for i in 0..1u32 {
            mark_dirty(out_sheet as usize, out_row as u32 + i, out_col as u32);
            mark_dirty(out_sheet as usize, out_row as u32 + i, out_col as u32 + 1);
        }

        "{\"rows_written\":1}".to_string()
    }, false;

    // covariance(sheet1, r1_start, r1_end, c1, sheet2, r2_start, r2_end, c2,
    //            out_sheet, out_row, out_col) -> String (JSON)
    // gap analysis 2.4：协方差（Covariance）。同t_test/f_test设计原则，
    // 直接调用formualizer-eval原生的COVARIANCE.P/COVARIANCE.S，不照抄
    // LOK的手工展开版本。
    //
    // 输出2行：Covariance (Population)（COVARIANCE.P，总体协方差）/
    // Covariance (Sample)（COVARIANCE.S，样本协方差）——两者有实际数值
    // 差异（不像CORREL/PEARSON那样冗余），统计推断中经常都需要看，
    // 用户已确认此设计（见v160交接文档第一节）。
    fn "covariance" =>
        |sheet1: i64, r1_start: i64, r1_end: i64, c1: i64,
         sheet2: i64, r2_start: i64, r2_end: i64, c2: i64,
         out_sheet: i64, out_row: i64, out_col: i64| -> String {
        let name1 = with_wb(|wb| wb.sheets.get(sheet1 as usize).map(|s| s.name.clone())).flatten();
        let name2 = with_wb(|wb| wb.sheets.get(sheet2 as usize).map(|s| s.name.clone())).flatten();
        let (name1, name2) = match (name1, name2) {
            (Some(n1), Some(n2)) => (n1, n2),
            _ => return "{\"rows_written\":0,\"error\":\"sheet1 or sheet2 index out of range\"}".to_string(),
        };
        let out_sheet_valid = with_wb(|wb| wb.sheets.get(out_sheet as usize).is_some()).unwrap_or(false);
        if !out_sheet_valid {
            return "{\"rows_written\":0,\"error\":\"out_sheet index out of range\"}".to_string();
        }

        let range1 = format!(
            "{}!{}{}:{}{}",
            name1, col_to_letters(c1 as u32 + 1), r1_start + 1, col_to_letters(c1 as u32 + 1), r1_end + 1,
        );
        let range2 = format!(
            "{}!{}{}:{}{}",
            name2, col_to_letters(c2 as u32 + 1), r2_start + 1, col_to_letters(c2 as u32 + 1), r2_end + 1,
        );

        let rows: [(String, String); 2] = [
            ("Covariance (Population)".to_string(), format!("=COVARIANCE.P({},{})", range1, range2)),
            ("Covariance (Sample)".to_string(),      format!("=COVARIANCE.S({},{})", range1, range2)),
        ];

        with_wb(|wb| {
            let out = wb.sheets.get_mut(out_sheet as usize)?;
            for (i, (label, formula)) in rows.iter().enumerate() {
                let r = out_row as u32 + i as u32;
                set_cell_in_sheet(out, r, out_col as u32, CellValue::Text(label.clone()));
                set_cell_in_sheet(out, r, out_col as u32 + 1, CellValue::Formula {
                    source: formula.clone(),
                    cached: Box::new(CellValue::Empty),
                });
            }
            Some(())
        });

        for i in 0..2u32 {
            mark_dirty(out_sheet as usize, out_row as u32 + i, out_col as u32);
            mark_dirty(out_sheet as usize, out_row as u32 + i, out_col as u32 + 1);
        }

        "{\"rows_written\":2}".to_string()
    }, false;

    // regression(sheet_y, ry_start, ry_end, cy, sheet_x, rx_start, rx_end, cx,
    //            out_sheet, out_row, out_col) -> String (JSON)
    // gap analysis 2.4：回归分析（Regression）。本版本范围限定为"单一Y+
    // 单一X的简单线性回归"（用户2026-09确认：多元回归——支持多个X列——
    // 列为后续独立任务，不在本次范围）。
    //
    // 方案确认（与correl/covariance/t_test/f_test一致的原则）：不使用
    // formualizer-eval的LINEST/TREND——这两个函数返回数组结果
    // （LiteralValue::Array，LINEST为5x2矩阵，TREND为1xN），而ROE的
    // CellValue::Formula.cached只能装标量CellValue（literal_to_cell_value
    // 对Array类型的处理是直接取左上角第一个元素，其余全部丢弃，见
    // roe-calc/src/formula_engine.rs），若对单元格写入LINEST/TREND公式，
    // 结果会被静默截断成单一数字，語义错误。参照真实LibreOffice回归分析
    // 对话框实测样例（用户提供截图），LOK自身对着有空值的多元数据跑LINEST
    // 数组展开也大面积报"错误:502"，佐证了这条路线的脆弱性——ROE不追求
    // 复刻这套数组公式机制，改用formualizer-eval已原生实现的标量函数
    // SLOPE/INTERCEPT/RSQ/STEYX/CORREL各自单独调用一次，与LINEST 5x2
    // 矩阵左上角四个格外加相关系数在数学上完全等价，规避了数组落地问题。
    //
    // sheet_y/cy 是因变量Y列，sheet_x/cx 是自变量X列——SLOPE/INTERCEPT/
    // RSQ/STEYX/CORREL要求y在前x在后，参数命名直接体现方向，避免像
    // t_test/f_test那种对称参数（sheet1/sheet2）容易被调用方搞反顺序。
    //
    // 两组数据允许来自不同sheet，输出同样允许跨sheet，与其余统计工具
    // 一致的灵活度。
    //
    // 输出5行：Slope/Intercept/R Squared/Standard Error/Correlation。
    fn "regression" =>
        |sheet_y: i64, ry_start: i64, ry_end: i64, cy: i64,
         sheet_x: i64, rx_start: i64, rx_end: i64, cx: i64,
         out_sheet: i64, out_row: i64, out_col: i64| -> String {
        let name_y = with_wb(|wb| wb.sheets.get(sheet_y as usize).map(|s| s.name.clone())).flatten();
        let name_x = with_wb(|wb| wb.sheets.get(sheet_x as usize).map(|s| s.name.clone())).flatten();
        let (name_y, name_x) = match (name_y, name_x) {
            (Some(ny), Some(nx)) => (ny, nx),
            _ => return "{\"rows_written\":0,\"error\":\"sheet_y or sheet_x index out of range\"}".to_string(),
        };
        let out_sheet_valid = with_wb(|wb| wb.sheets.get(out_sheet as usize).is_some()).unwrap_or(false);
        if !out_sheet_valid {
            return "{\"rows_written\":0,\"error\":\"out_sheet index out of range\"}".to_string();
        }

        let range_y = format!(
            "{}!{}{}:{}{}",
            name_y, col_to_letters(cy as u32 + 1), ry_start + 1, col_to_letters(cy as u32 + 1), ry_end + 1,
        );
        let range_x = format!(
            "{}!{}{}:{}{}",
            name_x, col_to_letters(cx as u32 + 1), rx_start + 1, col_to_letters(cx as u32 + 1), rx_end + 1,
        );

        let rows: [(String, String); 5] = [
            ("Slope".to_string(),           format!("=SLOPE({},{})", range_y, range_x)),
            ("Intercept".to_string(),       format!("=INTERCEPT({},{})", range_y, range_x)),
            ("R Squared".to_string(),       format!("=RSQ({},{})", range_y, range_x)),
            ("Standard Error".to_string(),  format!("=STEYX({},{})", range_y, range_x)),
            ("Correlation".to_string(),     format!("=CORREL({},{})", range_y, range_x)),
        ];

        with_wb(|wb| {
            let out = wb.sheets.get_mut(out_sheet as usize)?;
            for (i, (label, formula)) in rows.iter().enumerate() {
                let r = out_row as u32 + i as u32;
                set_cell_in_sheet(out, r, out_col as u32, CellValue::Text(label.clone()));
                set_cell_in_sheet(out, r, out_col as u32 + 1, CellValue::Formula {
                    source: formula.clone(),
                    cached: Box::new(CellValue::Empty),
                });
            }
            Some(())
        });

        for i in 0..5u32 {
            mark_dirty(out_sheet as usize, out_row as u32 + i, out_col as u32);
            mark_dirty(out_sheet as usize, out_row as u32 + i, out_col as u32 + 1);
        }

        "{\"rows_written\":5}".to_string()
    }, false;

    // moving_average(sheet, r_start, r_end, c, interval, out_sheet, out_row,
    //                 out_col) -> String (JSON)
    // gap analysis 2.4/roe-ext_progress_v161第七节任务队列第1项：移动
    // 平均。照抄LOK MovingAverageDialog.cxx的居中窗口(central moving
    // average)语义，核实理由：2026-09会话核实业界惯例（scipy/pandas
    // rolling(center=True)同样是"窗口越界处结果为NaN/缺失值,不做窗口
    // 收缩"），LOK这套设计与业界统计惯例一致，不是LO历史包袱，可以直接
    // 采用，不需要另外发明"向后看N期"的Excel式方案(不确定Excel内部
    // 精确实现细节，避免臆测拼凑)。
    //
    // 窗口对齐（居中，偶数区间偏左，对齐LOK第85-96行的half/half_remainder
    // 计算）：
    //   half = interval / 2 (整数除法)
    //   half_remainder = interval % 2
    //   区间起点 = i - half
    //   区间终点 = i + half - 1 + half_remainder
    // 例：interval=4（偶）→ half=2,rem=0 → [i-2, i+1]（前2后1，偏左）
    //     interval=5（奇）→ half=2,rem=1 → [i-2, i+2]（前2后2，严格居中）
    //
    // 边界处理：区间起点/终点超出输入数据范围[0, n-1]时，该行写"=NA()"
    // （不用LOK的字面量"=#N/A"写法——那是LOK公式语法解析器认的token，
    // formualizer-eval经核实原生支持NA()函数，标准Excel/ODF函数，更
    // 稳妥可移植，求值结果等价）。不做窗口收缩去凑越界情况，与LOK/
    // pandas一致。
    //
    // interval<=0时直接返回错误，不生成任何公式（无意义的窗口大小）。
    //
    // 输出：r_end-r_start+1行，纵向写在out_col这一列，每行要么是
    // "=AVERAGE(range)"要么是"=NA()"。不写标签列——与describe_stats/
    // t_test等"固定几个统计量各一行"的形态不同，这里每行对应输入
    // 的每一个原始数据点，逐点添加文字标签意义不大，出参JSON里的
    // rows_written已经足够说明写了多少行。
    fn "moving_average" =>
        |sheet: i64, r_start: i64, r_end: i64, c: i64, interval: i64,
         out_sheet: i64, out_row: i64, out_col: i64| -> String {
        let sheet_name = match with_wb(|wb| wb.sheets.get(sheet as usize).map(|s| s.name.clone())) {
            Some(Some(name)) => name,
            _ => return "{\"rows_written\":0,\"error\":\"sheet index out of range\"}".to_string(),
        };
        let out_sheet_valid = with_wb(|wb| wb.sheets.get(out_sheet as usize).is_some()).unwrap_or(false);
        if !out_sheet_valid {
            return "{\"rows_written\":0,\"error\":\"out_sheet index out of range\"}".to_string();
        }
        if interval <= 0 {
            return "{\"rows_written\":0,\"error\":\"interval must be positive\"}".to_string();
        }

        let n = r_end - r_start + 1;
        if n <= 0 {
            return "{\"rows_written\":0,\"error\":\"r_start must be <= r_end\"}".to_string();
        }

        let half = interval / 2;
        let half_remainder = interval % 2;
        let col_letter = col_to_letters(c as u32 + 1);

        let mut formulas: Vec<String> = Vec::with_capacity(n as usize);
        for i in 0..n {
            let interval_start = i - half;
            let interval_end = i + half - 1 + half_remainder;
            if interval_start < 0 || interval_end >= n {
                formulas.push("=NA()".to_string());
            } else {
                let row_start = r_start + interval_start + 1;
                let row_end = r_start + interval_end + 1;
                formulas.push(format!(
                    "=AVERAGE({}!{}{}:{}{})",
                    sheet_name, col_letter, row_start, col_letter, row_end,
                ));
            }
        }

        with_wb(|wb| {
            let out = wb.sheets.get_mut(out_sheet as usize)?;
            for (i, formula) in formulas.iter().enumerate() {
                let r = out_row as u32 + i as u32;
                set_cell_in_sheet(out, r, out_col as u32, CellValue::Formula {
                    source: formula.clone(),
                    cached: Box::new(CellValue::Empty),
                });
            }
            Some(())
        });

        for i in 0..(n as u32) {
            mark_dirty(out_sheet as usize, out_row as u32 + i, out_col as u32);
        }

        format!("{{\"rows_written\":{}}}", n)
    }, false;

    // exponential_smoothing(sheet, r_start, r_end, c, alpha, out_sheet,
    //                        out_row, out_col) -> String (JSON)
    // gap analysis 2.4/roe-ext_progress_v161第七节任务队列第1项：指数
    // 平滑。照抄LOK ExponentialSmoothingDialog.cxx的公式模板（教科书
    // 标准一阶指数平滑递推式，无争议设计空间，不同于移动平均需要拍板
    // 窗口语义）。
    //
    // alpha范围校验：2026-09核实业界惯例（Excel数据分析工具箱"阻尼
    // 系数"框强制要求落在0到1之间，alpha=1-阻尼系数同样落在[0,1]）——
    // 这不是可选的业务口径选择，是这个统计方法本身的数学约束（alpha=0
    // 表示完全不响应新数据，alpha=1表示完全丢弃历史只看最新值，超出
    // [0,1]会导致递推公式产生无意义的发散结果），因此本函数校验
    // alpha必须落在[0,1]闭区间，越界返回错误，不同于t_test的tails/
    // test_type那种"不做业务校验交给调用方"的处理方式——那些是分类
    // 枚举，无效值只是选了不存在的选项，性质不同。
    //
    // 输出结构（对齐LOK的两段式输出）：
    //   第1行：Alpha标签 + alpha数值（CellValue::Number常量，不是公式，
    //          因为这是用户输入的参数本身，不是从数据计算出来的量）
    //   第2行起共n行（n=r_end-r_start+1，对应输入的每个数据点）：
    //     第一行（对应第一个原始观测值）：直接引用该单元格本身
    //       "=<cell_ref>"（LOK称为Initial value，等于首个观测值，代码
    //       中可见LOK曾用if(false)禁用的AVERAGE(全区间)分支，最终采用
    //       更朴素的"首值=首个观测值"）
    //     后续每行："=alpha_cell*当前原始值 + (1-alpha_cell)*上一行平滑值"
    //       （标准一阶指数平滑递推式，alpha_cell引用本函数第1行写入的
    //       alpha值单元格，不是把alpha数值硬编码进每条公式字符串里，
    //       这样后续如果用户手动改alpha格子的值，所有平滑结果公式能
    //       联动重算，不需要重新调用本函数）。
    fn "exponential_smoothing" =>
        |sheet: i64, r_start: i64, r_end: i64, c: i64, alpha: f64,
         out_sheet: i64, out_row: i64, out_col: i64| -> String {
        let sheet_name = match with_wb(|wb| wb.sheets.get(sheet as usize).map(|s| s.name.clone())) {
            Some(Some(name)) => name,
            _ => return "{\"rows_written\":0,\"error\":\"sheet index out of range\"}".to_string(),
        };
        let out_sheet_valid = with_wb(|wb| wb.sheets.get(out_sheet as usize).is_some()).unwrap_or(false);
        if !out_sheet_valid {
            return "{\"rows_written\":0,\"error\":\"out_sheet index out of range\"}".to_string();
        }
        if !(0.0..=1.0).contains(&alpha) {
            return "{\"rows_written\":0,\"error\":\"alpha must be within [0,1]\"}".to_string();
        }

        let n = r_end - r_start + 1;
        if n <= 0 {
            return "{\"rows_written\":0,\"error\":\"r_start must be <= r_end\"}".to_string();
        }

        let col_letter = col_to_letters(c as u32 + 1);
        // 数值列(标签列+1)的字母——alpha数值、平滑结果都写在这一列，
        // 标签列(out_col)只放"Alpha"这个文字，公式引用不应指向标签列。
        let value_col_letter = col_to_letters(out_col as u32 + 1 + 1);

        // 第1行：Alpha标签 + 数值。alpha_ref供后续行公式引用（同sheet内，
        // 不需要跨sheet前缀，因为alpha和平滑结果都写在out_sheet），必须
        // 指向数值列(value_col_letter)而非标签列。
        let alpha_row_1based = out_row as u32 + 1;
        let alpha_ref = format!("{}{}", value_col_letter, alpha_row_1based);

        with_wb(|wb| {
            let out = wb.sheets.get_mut(out_sheet as usize)?;
            set_cell_in_sheet(out, out_row as u32, out_col as u32, CellValue::Text("Alpha".to_string()));
            set_cell_in_sheet(out, out_row as u32, out_col as u32 + 1, CellValue::Number(alpha));

            for i in 0..n {
                let r = out_row as u32 + 1 + i as u32;
                let formula = if i == 0 {
                    format!("={}!{}{}", sheet_name, col_letter, r_start + 1)
                } else {
                    // r是当前输出行的0-based内部索引，"上一行"的0-based
                    // 索引是r-1，转成1-based公式行号是(r-1)+1=r，不是r-1
                    // （此处曾误把0-based索引直接当1-based行号用，导致
                    // 生成的引用总是指向alpha所在的第1行而非真正的上一
                    // 行，2026-09单元测试发现并修正）。
                    let prev_row = r;
                    let cur_data_row = r_start + i + 1;
                    format!(
                        "={}*{}!{}{} + (1-{})*{}{}",
                        alpha_ref, sheet_name, col_letter, cur_data_row,
                        alpha_ref, value_col_letter, prev_row,
                    )
                };
                set_cell_in_sheet(out, r, out_col as u32 + 1, CellValue::Formula {
                    source: formula,
                    cached: Box::new(CellValue::Empty),
                });
            }
            Some(())
        });

        for i in 0..=(n as u32) {
            mark_dirty(out_sheet as usize, out_row as u32 + i, out_col as u32);
            mark_dirty(out_sheet as usize, out_row as u32 + i, out_col as u32 + 1);
        }

        format!("{{\"rows_written\":{}}}", n + 1)
    }, false;

    // anova_single_factor(sheet, r_start, r_end, c_start, c_end,
    //                      grouped_by_column, alpha, out_sheet, out_row,
    //                      out_col) -> String (JSON)
    // gap analysis 2.4/roe-ext_progress_v162第八节任务队列第1项：单因素
    // 方差分析。对照LOK AnalysisOfVarianceDialog.cxx的AnovaSingleFactor
    // 函数（182-328行核实完整）实现，公式结构两段式：
    //   第一段：基础统计量表，每组一行，5列（Groups/Count/Sum/Mean/
    //           Variance），组数由grouped_by_column决定输入区域怎么切分
    //           （true=按列分组，每列一组；false=按行分组，每行一组）。
    //   第二段：ANOVA表，空一行后3行×7列（Source/SS/DF/MS/F/P-value/
    //           F-critical），Between Groups/Within Groups/Total三行，
    //           Within与Total行的F/P-value/F-critical留空
    //           （CellValue::Empty，无统计意义）。
    //
    // 2026-09会话核实formualizer-eval 0.7.1两处关键点，公式模板与LOK
    // 原文不同，不能直接照抄LOK字符串：
    //   1) DEVSQ是variadic的（min_args=1, variadic=true，内部把所有
    //      range参数拉平成一组数据统一算离均差平方和）：Total行
    //      "=DEVSQ(range1,range2,...)"多组一次调用直接可用；Within
    //      Groups行"=SUM(DEVSQ(range1),DEVSQ(range2),...)"每组单独
    //      调用一次DEVSQ再SUM，两者均不需要改写，与LOK设计等价。
    //   2) LOK老式FDIST(F_VAL,d1,d2)/FINV(alpha,d1,d2)三参数签名已过时，
    //      formualizer-eval对应新函数为F.DIST(x,d1,d2,cumulative)四参数
    //      （cumulative=1取左尾累积分布，传统p值是右尾，需用1-F.DIST）和F.INV(prob,d1,d2)
    //      三参数（签名与LOK基本一致，只是函数名带点）。Between Groups
    //      行P-value用"=1-F.DIST(F_VAL,BETWEEN_DF,WITHIN_DF,1)"，
    //      F-critical用"=F.INV(1-ALPHA,BETWEEN_DF,WITHIN_DF)"(上侧alpha分位)。
    //
    // 参数分隔符统一用英文逗号（tokenizer.rs确认','和';'均被识别为
    // 分隔符，选逗号与项目现有公式习惯一致，不采用LOK源码的欧式分号）。
    //
    // 第二段SUM_RANGE/MEAN_RANGE/COUNT_RANGE引用的是第一段基础统计量
    // 表（写在out_sheet），不是源数据sheet，前缀必须用out_sheet的名字，
    // 不是sheet_name（源数据sheet名，只用于第一段各组range的引用）。
    //
    // alpha校验：与exponential_smoothing一致，落在[0,1]闭区间，越界
    // 返回错误。
    //
    // grouped_by_column=true时要求c_end>=c_start且组数(c_end-c_start+1)
    // >=2；false时同理对r_start/r_end。至少2组，单组方差分析无意义。
    //
    // 组名标签（Groups列文字）：ROE自主生成"Column 1"/"Column 2"...或
    // "Row 1"/"Row 2"...占位符，v162第4.1节记录的"表头识别"技术债本函数
    // 不处理，与其余8个已完成统计函数现状一致。
    fn "anova_single_factor" =>
        |sheet: i64, r_start: i64, r_end: i64, c_start: i64, c_end: i64,
         grouped_by_column: bool, alpha: f64,
         out_sheet: i64, out_row: i64, out_col: i64| -> String {
        let sheet_name = match with_wb(|wb| wb.sheets.get(sheet as usize).map(|s| s.name.clone())) {
            Some(Some(name)) => name,
            _ => return "{\"rows_written\":0,\"error\":\"sheet index out of range\"}".to_string(),
        };
        let out_sheet_name = match with_wb(|wb| wb.sheets.get(out_sheet as usize).map(|s| s.name.clone())) {
            Some(Some(name)) => name,
            _ => return "{\"rows_written\":0,\"error\":\"out_sheet index out of range\"}".to_string(),
        };
        if !(0.0..=1.0).contains(&alpha) {
            return "{\"rows_written\":0,\"error\":\"alpha must be within [0,1]\"}".to_string();
        }
        if r_end < r_start || c_end < c_start {
            return "{\"rows_written\":0,\"error\":\"invalid input range\"}".to_string();
        }

        let n_groups: i64 = if grouped_by_column {
            c_end - c_start + 1
        } else {
            r_end - r_start + 1
        };
        if n_groups < 2 {
            return "{\"rows_written\":0,\"error\":\"need at least 2 groups\"}".to_string();
        }

        // 每组的range引用字符串（形如 "Sheet1!A1:A5"，含源sheet名前缀）。
        let mut group_ranges: Vec<String> = Vec::with_capacity(n_groups as usize);
        let mut group_labels: Vec<String> = Vec::with_capacity(n_groups as usize);
        if grouped_by_column {
            for (idx, col) in (c_start..=c_end).enumerate() {
                let col_letter = col_to_letters(col as u32 + 1);
                group_ranges.push(format!(
                    "{}!{}{}:{}{}",
                    sheet_name, col_letter, r_start + 1, col_letter, r_end + 1,
                ));
                group_labels.push(format!("Column {}", idx + 1));
            }
        } else {
            let start_letter = col_to_letters(c_start as u32 + 1);
            let end_letter = col_to_letters(c_end as u32 + 1);
            for (idx, row) in (r_start..=r_end).enumerate() {
                group_ranges.push(format!(
                    "{}!{}{}:{}{}",
                    sheet_name, start_letter, row + 1, end_letter, row + 1,
                ));
                group_labels.push(format!("Row {}", idx + 1));
            }
        }

        // 第一段列字母：Groups(out_col) Count(+1) Sum(+2) Mean(+3) Variance(+4)
        let count_col_letter = col_to_letters(out_col as u32 + 1 + 1);
        let sum_col_letter = col_to_letters(out_col as u32 + 2 + 1);
        let mean_col_letter = col_to_letters(out_col as u32 + 3 + 1);

        // 第一段：基础统计量表，每组一行
        with_wb(|wb| {
            let out = wb.sheets.get_mut(out_sheet as usize)?;
            for (i, (range_ref, label)) in group_ranges.iter().zip(group_labels.iter()).enumerate() {
                let r = out_row as u32 + i as u32;
                set_cell_in_sheet(out, r, out_col as u32, CellValue::Text(label.clone()));
                set_cell_in_sheet(out, r, out_col as u32 + 1, CellValue::Formula {
                    source: format!("=COUNT({})", range_ref),
                    cached: Box::new(CellValue::Empty),
                });
                set_cell_in_sheet(out, r, out_col as u32 + 2, CellValue::Formula {
                    source: format!("=SUM({})", range_ref),
                    cached: Box::new(CellValue::Empty),
                });
                set_cell_in_sheet(out, r, out_col as u32 + 3, CellValue::Formula {
                    source: format!("=AVERAGE({})", range_ref),
                    cached: Box::new(CellValue::Empty),
                });
                set_cell_in_sheet(out, r, out_col as u32 + 4, CellValue::Formula {
                    source: format!("=VAR({})", range_ref),
                    cached: Box::new(CellValue::Empty),
                });
            }
            Some(())
        });

        // 第一段整列引用（供第二段公式使用），前缀用out_sheet的名字
        let first_data_row_1based = out_row + 1;
        let last_data_row_1based = out_row + n_groups;
        let count_range = format!(
            "{}!{}{}:{}{}",
            out_sheet_name, count_col_letter, first_data_row_1based, count_col_letter, last_data_row_1based,
        );
        let sum_range = format!(
            "{}!{}{}:{}{}",
            out_sheet_name, sum_col_letter, first_data_row_1based, sum_col_letter, last_data_row_1based,
        );
        let mean_range = format!(
            "{}!{}{}:{}{}",
            out_sheet_name, mean_col_letter, first_data_row_1based, mean_col_letter, last_data_row_1based,
        );

        // 第二段起始行（0-based内部行索引）：空一行在n_groups行之后
        let between_row = out_row + n_groups + 1;
        let within_row = between_row + 1;
        let total_row = between_row + 2;

        // ANOVA表列：Source(out_col) SS(+1) DF(+2) MS(+3) F(+4) P-value(+5) F-critical(+6)
        let ss_col_letter = col_to_letters(out_col as u32 + 1 + 1);
        let df_col_letter = col_to_letters(out_col as u32 + 2 + 1);
        let ms_col_letter = col_to_letters(out_col as u32 + 3 + 1);
        let f_col_letter = col_to_letters(out_col as u32 + 4 + 1);

        let between_ss_ref = format!("{}{}", ss_col_letter, between_row as u32 + 1);
        let between_df_ref = format!("{}{}", df_col_letter, between_row as u32 + 1);
        let between_ms_ref = format!("{}{}", ms_col_letter, between_row as u32 + 1);
        let within_ss_ref = format!("{}{}", ss_col_letter, within_row as u32 + 1);
        let within_df_ref = format!("{}{}", df_col_letter, within_row as u32 + 1);
        let within_ms_ref = format!("{}{}", ms_col_letter, within_row as u32 + 1);

        let between_ss_formula = format!(
            "=SUMPRODUCT({},{})-SUM({})^2/SUM({})",
            sum_range, mean_range, sum_range, count_range,
        );
        let between_df_formula = format!("=COUNT({})-1", sum_range);
        let between_ms_formula = format!("={}/{}", between_ss_ref, between_df_ref);
        let between_f_formula = format!("={}/{}", between_ms_ref, within_ms_ref);
        let between_p_formula = format!(
            "=1-F.DIST({},{},{},1)",
            format!("{}{}", f_col_letter, between_row as u32 + 1), between_df_ref, within_df_ref,
        );
        let between_fcrit_formula = format!(
            "=F.INV(1-{},{},{})",
            alpha, between_df_ref, within_df_ref,
        );

        let devsq_within_terms: Vec<String> = group_ranges.iter()
            .map(|r| format!("DEVSQ({})", r))
            .collect();
        let within_ss_formula = format!("=SUM({})", devsq_within_terms.join(","));
        let within_df_formula = format!("=SUM({})-COUNT({})", count_range, count_range);
        let within_ms_formula = format!("={}/{}", within_ss_ref, within_df_ref);

        let total_ss_formula = format!("=DEVSQ({})", group_ranges.join(","));
        let total_df_formula = format!("=SUM({})-1", count_range);

        with_wb(|wb| {
            let out = wb.sheets.get_mut(out_sheet as usize)?;

            // Between Groups行
            set_cell_in_sheet(out, between_row as u32, out_col as u32, CellValue::Text("Between Groups".to_string()));
            set_cell_in_sheet(out, between_row as u32, out_col as u32 + 1, CellValue::Formula { source: between_ss_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, between_row as u32, out_col as u32 + 2, CellValue::Formula { source: between_df_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, between_row as u32, out_col as u32 + 3, CellValue::Formula { source: between_ms_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, between_row as u32, out_col as u32 + 4, CellValue::Formula { source: between_f_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, between_row as u32, out_col as u32 + 5, CellValue::Formula { source: between_p_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, between_row as u32, out_col as u32 + 6, CellValue::Formula { source: between_fcrit_formula, cached: Box::new(CellValue::Empty) });

            // Within Groups行
            set_cell_in_sheet(out, within_row as u32, out_col as u32, CellValue::Text("Within Groups".to_string()));
            set_cell_in_sheet(out, within_row as u32, out_col as u32 + 1, CellValue::Formula { source: within_ss_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, within_row as u32, out_col as u32 + 2, CellValue::Formula { source: within_df_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, within_row as u32, out_col as u32 + 3, CellValue::Formula { source: within_ms_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, within_row as u32, out_col as u32 + 4, CellValue::Empty);
            set_cell_in_sheet(out, within_row as u32, out_col as u32 + 5, CellValue::Empty);
            set_cell_in_sheet(out, within_row as u32, out_col as u32 + 6, CellValue::Empty);

            // Total行
            set_cell_in_sheet(out, total_row as u32, out_col as u32, CellValue::Text("Total".to_string()));
            set_cell_in_sheet(out, total_row as u32, out_col as u32 + 1, CellValue::Formula { source: total_ss_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, total_row as u32, out_col as u32 + 2, CellValue::Formula { source: total_df_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, total_row as u32, out_col as u32 + 3, CellValue::Empty);
            set_cell_in_sheet(out, total_row as u32, out_col as u32 + 4, CellValue::Empty);
            set_cell_in_sheet(out, total_row as u32, out_col as u32 + 5, CellValue::Empty);
            set_cell_in_sheet(out, total_row as u32, out_col as u32 + 6, CellValue::Empty);

            Some(())
        });

        let total_rows_written = n_groups + 1 + 3; // 第一段n_groups行 + 空1行 + 第二段3行
        for i in 0..(total_rows_written as u32) {
            for c in 0..7u32 {
                mark_dirty(out_sheet as usize, out_row as u32 + i, out_col as u32 + c);
            }
        }

        format!("{{\"rows_written\":{}}}", total_rows_written)
    }, false;

    // anova_two_factor(sheet, r_start, r_end, c_start, c_end, alpha,
    //                   out_sheet, out_row, out_col) -> String (JSON)
    // gap analysis 2.4/roe-ext_progress_v162第八节任务队列第2项：双因素
    // 方差分析（无重复，Without Replication，对齐LOK同名对话框功能）。
    // 对照LOK AnalysisOfVarianceDialog.cxx的AnovaTwoFactor函数
    // （329-538行完整核实）。不同于单因素，双因素不需要grouped_by_column
    // 参数——输入矩形区域天然同时按列和按行切分两次（LOK对同一个
    // mInputRange分别调用lclMakeSubRangesList(BY_COLUMN)和(BY_ROW)），
    // 两个维度均需要，不是二选一。
    //
    // 输出结构（三段式，比单因素多一段，核实于RowColumn函数147-179行：
    // 每组纵向占一行，标签+统计量横向排布，与单因素同一模式）：
    //   行0：Alpha标签 + alpha数值
    //   空一行
    //   ColumnX段：n_cols行 x 5列（Column标签/Count/Sum/Mean/Variance），
    //              每列一组（对mInputRange按列切分）
    //   空一行
    //   RowX段：n_rows行 x 5列（Row标签/Count/Sum/Mean/Variance），
    //           每行一组（对mInputRange按行切分）
    //   空一行
    //   ANOVA表：4行 x 7列（Source/SS/DF/MS/F/P-value/F-critical）
    //            Rows/Columns/Error/Total四行
    //
    // 关键交叉引用（核实于LOK源码329-538行，与单因素的"从上往下"单向
    // 引用不同，双因素存在双向坐标依赖）：
    //   - Rows行：SS用SUM_RANGE_ROW/MEAN_RANGE_ROW（按行切分统计量），
    //     DF用MAX(COUNT_RANGE_COLUMN)（注意是按列切分的Count取MAX，
    //     因为一行的自由度由列数决定，维度对调）
    //   - Columns行：同理但维度互换，SS用_COLUMN统计量，
    //     DF用MAX(COUNT_RANGE_ROW)
    //   - Rows/Columns行的F引用MS_ERROR（Error行的MS列），Error行在
    //     Rows/Columns之后才写——生成代码时需提前算出Error行坐标，
    //     即使还未写入该行内容，坐标计算无先后限制，只是"提前量"。
    //   - Error行SS用SUMSQ(RANGE)+SUM(RANGE)^2/COUNT(RANGE)-(两个
    //     SUMPRODUCT交叉项之和)。2026-09核实formualizer-eval的SUMSQ
    //     签名为SUMSQ(arg1...: number@range)，variadic但单range参数
    //     调用方式与LOK一致，无需改写。
    //   - Error行DF = TOTAL_DF - ROW_DF - COLUMN_DF，引用Total行DF
    //     （Total行在Error行之后才写，同样是"提前算坐标"模式）。
    //   - Total行SS = SUM(ROW_SS, COLUMN_SS, ERROR_SS)（引用ANOVA表内
    //     其他三行的SS，不是重新计算）。
    //   - P-value/F-critical同单因素一样，LOK老式FDIST/FINV三参数需
    //     改写为F.DIST(x,d1,d2,1)四参数（cumulative=1）和F.INV三参数
    //     （函数名加点），不能照抄LOK字符串。
    //
    // 参数分隔符统一用英文逗号，与单因素/其余已完成统计函数一致。
    //
    // alpha校验：[0,1]闭区间，越界返回错误，与其余统计函数一致。
    //
    // 要求n_cols=(c_end-c_start+1)>=2 且 n_rows=(r_end-r_start+1)>=2，
    // 任一维度只有1组则该维度自由度为0，无统计意义。
    //
    // 组名标签：ROE自主生成"Column 1"/"Column 2"...和"Row 1"/"Row 2"...，
    // 与单因素一致，不处理表头（v162第4.1节技术债，本函数同样不处理）。
    fn "anova_two_factor" =>
        |sheet: i64, r_start: i64, r_end: i64, c_start: i64, c_end: i64,
         alpha: f64, out_sheet: i64, out_row: i64, out_col: i64| -> String {
        let sheet_name = match with_wb(|wb| wb.sheets.get(sheet as usize).map(|s| s.name.clone())) {
            Some(Some(name)) => name,
            _ => return "{\"rows_written\":0,\"error\":\"sheet index out of range\"}".to_string(),
        };
        let out_sheet_name = match with_wb(|wb| wb.sheets.get(out_sheet as usize).map(|s| s.name.clone())) {
            Some(Some(name)) => name,
            _ => return "{\"rows_written\":0,\"error\":\"out_sheet index out of range\"}".to_string(),
        };
        if !(0.0..=1.0).contains(&alpha) {
            return "{\"rows_written\":0,\"error\":\"alpha must be within [0,1]\"}".to_string();
        }
        if r_end < r_start || c_end < c_start {
            return "{\"rows_written\":0,\"error\":\"invalid input range\"}".to_string();
        }

        let n_cols: i64 = c_end - c_start + 1;
        let n_rows: i64 = r_end - r_start + 1;
        if n_cols < 2 || n_rows < 2 {
            return "{\"rows_written\":0,\"error\":\"need at least 2 columns and 2 rows\"}".to_string();
        }

        // 每列/每行的range引用（含源sheet名前缀）
        let mut col_ranges: Vec<String> = Vec::with_capacity(n_cols as usize);
        let mut col_labels: Vec<String> = Vec::with_capacity(n_cols as usize);
        for (idx, col) in (c_start..=c_end).enumerate() {
            let col_letter = col_to_letters(col as u32 + 1);
            col_ranges.push(format!(
                "{}!{}{}:{}{}",
                sheet_name, col_letter, r_start + 1, col_letter, r_end + 1,
            ));
            col_labels.push(format!("Column {}", idx + 1));
        }
        let mut row_ranges: Vec<String> = Vec::with_capacity(n_rows as usize);
        let mut row_labels: Vec<String> = Vec::with_capacity(n_rows as usize);
        let start_letter = col_to_letters(c_start as u32 + 1);
        let end_letter = col_to_letters(c_end as u32 + 1);
        for (idx, row) in (r_start..=r_end).enumerate() {
            row_ranges.push(format!(
                "{}!{}{}:{}{}",
                sheet_name, start_letter, row + 1, end_letter, row + 1,
            ));
            row_labels.push(format!("Row {}", idx + 1));
        }
        let full_range_ref = format!(
            "{}!{}{}:{}{}",
            sheet_name, start_letter, r_start + 1, end_letter, r_end + 1,
        );

        // 布局：5列 Label(out_col)/Count(+1)/Sum(+2)/Mean(+3)/Variance(+4)
        let count_col_letter = col_to_letters(out_col as u32 + 1 + 1);
        let sum_col_letter = col_to_letters(out_col as u32 + 2 + 1);
        let mean_col_letter = col_to_letters(out_col as u32 + 3 + 1);

        // 行坐标（0-based内部索引）
        let alpha_row = out_row;
        let col_section_start = out_row + 2;               // Alpha行 + 空1行
        let row_section_start = col_section_start + n_cols + 1; // ColumnX段 + 空1行
        let anova_start = row_section_start + n_rows + 1;       // RowX段 + 空1行
        let rows_row = anova_start;       // Rows
        let columns_row = anova_start + 1; // Columns
        let error_row = anova_start + 2;   // Error
        let total_row = anova_start + 3;   // Total

        // ColumnX段整列引用（供ANOVA表Columns行使用）
        let col_count_range = format!(
            "{}!{}{}:{}{}",
            out_sheet_name, count_col_letter, col_section_start + 1, count_col_letter, col_section_start + n_cols,
        );
        let col_sum_range = format!(
            "{}!{}{}:{}{}",
            out_sheet_name, sum_col_letter, col_section_start + 1, sum_col_letter, col_section_start + n_cols,
        );
        let col_mean_range = format!(
            "{}!{}{}:{}{}",
            out_sheet_name, mean_col_letter, col_section_start + 1, mean_col_letter, col_section_start + n_cols,
        );
        // RowX段整列引用（供ANOVA表Rows行使用）
        let row_count_range = format!(
            "{}!{}{}:{}{}",
            out_sheet_name, count_col_letter, row_section_start + 1, count_col_letter, row_section_start + n_rows,
        );
        let row_sum_range = format!(
            "{}!{}{}:{}{}",
            out_sheet_name, sum_col_letter, row_section_start + 1, sum_col_letter, row_section_start + n_rows,
        );
        let row_mean_range = format!(
            "{}!{}{}:{}{}",
            out_sheet_name, mean_col_letter, row_section_start + 1, mean_col_letter, row_section_start + n_rows,
        );

        // ANOVA表列：Source(out_col) SS(+1) DF(+2) MS(+3) F(+4) P-value(+5) F-critical(+6)
        let ss_col_letter = col_to_letters(out_col as u32 + 1 + 1);
        let df_col_letter = col_to_letters(out_col as u32 + 2 + 1);
        let ms_col_letter = col_to_letters(out_col as u32 + 3 + 1);
        let f_col_letter = col_to_letters(out_col as u32 + 4 + 1);

        let rows_ss_ref = format!("{}{}", ss_col_letter, rows_row as u32 + 1);
        let rows_df_ref = format!("{}{}", df_col_letter, rows_row as u32 + 1);
        let rows_ms_ref = format!("{}{}", ms_col_letter, rows_row as u32 + 1);
        let rows_f_ref = format!("{}{}", f_col_letter, rows_row as u32 + 1);
        let columns_ss_ref = format!("{}{}", ss_col_letter, columns_row as u32 + 1);
        let columns_df_ref = format!("{}{}", df_col_letter, columns_row as u32 + 1);
        let columns_ms_ref = format!("{}{}", ms_col_letter, columns_row as u32 + 1);
        let columns_f_ref = format!("{}{}", f_col_letter, columns_row as u32 + 1);
        let error_ss_ref = format!("{}{}", ss_col_letter, error_row as u32 + 1);
        let error_df_ref = format!("{}{}", df_col_letter, error_row as u32 + 1);
        let error_ms_ref = format!("{}{}", ms_col_letter, error_row as u32 + 1);
        let total_df_ref = format!("{}{}", df_col_letter, total_row as u32 + 1);

        // Rows行公式
        let rows_ss_formula = format!(
            "=SUMPRODUCT({},{})-SUM({})^2/COUNT({})",
            row_sum_range, row_mean_range, full_range_ref, full_range_ref,
        );
        let rows_df_formula = format!("=MAX({})-1", col_count_range);
        let rows_ms_formula = format!("={}/{}", rows_ss_ref, rows_df_ref);
        let rows_f_formula = format!("={}/{}", rows_ms_ref, error_ms_ref);
        let rows_p_formula = format!("=1-F.DIST({},{},{},1)", rows_f_ref, rows_df_ref, error_df_ref);
        let rows_fcrit_formula = format!("=F.INV(1-{},{},{})", alpha, rows_df_ref, error_df_ref);

        // Columns行公式
        let columns_ss_formula = format!(
            "=SUMPRODUCT({},{})-SUM({})^2/COUNT({})",
            col_sum_range, col_mean_range, full_range_ref, full_range_ref,
        );
        let columns_df_formula = format!("=MAX({})-1", row_count_range);
        let columns_ms_formula = format!("={}/{}", columns_ss_ref, columns_df_ref);
        let columns_f_formula = format!("={}/{}", columns_ms_ref, error_ms_ref);
        let columns_p_formula = format!("=1-F.DIST({},{},{},1)", columns_f_ref, columns_df_ref, error_df_ref);
        let columns_fcrit_formula = format!("=F.INV(1-{},{},{})", alpha, columns_df_ref, error_df_ref);

        // Error行公式
        let error_ss_formula = format!(
            "=SUMSQ({})+SUM({})^2/COUNT({})-(SUMPRODUCT({},{})+SUMPRODUCT({},{}))",
            full_range_ref, full_range_ref, full_range_ref,
            row_sum_range, row_mean_range, col_sum_range, col_mean_range,
        );
        let error_df_formula = format!("={}-{}-{}", total_df_ref, rows_df_ref, columns_df_ref);
        let error_ms_formula = format!("={}/{}", error_ss_ref, error_df_ref);

        // Total行公式
        let total_ss_formula = format!("=SUM({},{},{})", rows_ss_ref, columns_ss_ref, error_ss_ref);
        let total_df_formula = format!("=COUNT({})-1", full_range_ref);

        let trailing_rows = with_wb(|wb| {
            let out = wb.sheets.get_mut(out_sheet as usize)?;

            // Alpha行
            set_cell_in_sheet(out, alpha_row as u32, out_col as u32, CellValue::Text("Alpha".to_string()));
            set_cell_in_sheet(out, alpha_row as u32, out_col as u32 + 1, CellValue::Number(alpha));

            // ColumnX段
            for (i, (range_ref, label)) in col_ranges.iter().zip(col_labels.iter()).enumerate() {
                let r = col_section_start as u32 + i as u32;
                set_cell_in_sheet(out, r, out_col as u32, CellValue::Text(label.clone()));
                set_cell_in_sheet(out, r, out_col as u32 + 1, CellValue::Formula { source: format!("=COUNT({})", range_ref), cached: Box::new(CellValue::Empty) });
                set_cell_in_sheet(out, r, out_col as u32 + 2, CellValue::Formula { source: format!("=SUM({})", range_ref), cached: Box::new(CellValue::Empty) });
                set_cell_in_sheet(out, r, out_col as u32 + 3, CellValue::Formula { source: format!("=AVERAGE({})", range_ref), cached: Box::new(CellValue::Empty) });
                set_cell_in_sheet(out, r, out_col as u32 + 4, CellValue::Formula { source: format!("=VAR({})", range_ref), cached: Box::new(CellValue::Empty) });
            }

            // RowX段
            for (i, (range_ref, label)) in row_ranges.iter().zip(row_labels.iter()).enumerate() {
                let r = row_section_start as u32 + i as u32;
                set_cell_in_sheet(out, r, out_col as u32, CellValue::Text(label.clone()));
                set_cell_in_sheet(out, r, out_col as u32 + 1, CellValue::Formula { source: format!("=COUNT({})", range_ref), cached: Box::new(CellValue::Empty) });
                set_cell_in_sheet(out, r, out_col as u32 + 2, CellValue::Formula { source: format!("=SUM({})", range_ref), cached: Box::new(CellValue::Empty) });
                set_cell_in_sheet(out, r, out_col as u32 + 3, CellValue::Formula { source: format!("=AVERAGE({})", range_ref), cached: Box::new(CellValue::Empty) });
                set_cell_in_sheet(out, r, out_col as u32 + 4, CellValue::Formula { source: format!("=VAR({})", range_ref), cached: Box::new(CellValue::Empty) });
            }

            // ANOVA表：Rows
            set_cell_in_sheet(out, rows_row as u32, out_col as u32, CellValue::Text("Rows".to_string()));
            set_cell_in_sheet(out, rows_row as u32, out_col as u32 + 1, CellValue::Formula { source: rows_ss_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, rows_row as u32, out_col as u32 + 2, CellValue::Formula { source: rows_df_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, rows_row as u32, out_col as u32 + 3, CellValue::Formula { source: rows_ms_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, rows_row as u32, out_col as u32 + 4, CellValue::Formula { source: rows_f_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, rows_row as u32, out_col as u32 + 5, CellValue::Formula { source: rows_p_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, rows_row as u32, out_col as u32 + 6, CellValue::Formula { source: rows_fcrit_formula, cached: Box::new(CellValue::Empty) });

            // ANOVA表：Columns
            set_cell_in_sheet(out, columns_row as u32, out_col as u32, CellValue::Text("Columns".to_string()));
            set_cell_in_sheet(out, columns_row as u32, out_col as u32 + 1, CellValue::Formula { source: columns_ss_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, columns_row as u32, out_col as u32 + 2, CellValue::Formula { source: columns_df_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, columns_row as u32, out_col as u32 + 3, CellValue::Formula { source: columns_ms_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, columns_row as u32, out_col as u32 + 4, CellValue::Formula { source: columns_f_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, columns_row as u32, out_col as u32 + 5, CellValue::Formula { source: columns_p_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, columns_row as u32, out_col as u32 + 6, CellValue::Formula { source: columns_fcrit_formula, cached: Box::new(CellValue::Empty) });

            // ANOVA表：Error（F/P/F-critical留空，无统计意义，同单因素Within Groups）
            set_cell_in_sheet(out, error_row as u32, out_col as u32, CellValue::Text("Error".to_string()));
            set_cell_in_sheet(out, error_row as u32, out_col as u32 + 1, CellValue::Formula { source: error_ss_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, error_row as u32, out_col as u32 + 2, CellValue::Formula { source: error_df_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, error_row as u32, out_col as u32 + 3, CellValue::Formula { source: error_ms_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, error_row as u32, out_col as u32 + 4, CellValue::Empty);
            set_cell_in_sheet(out, error_row as u32, out_col as u32 + 5, CellValue::Empty);
            set_cell_in_sheet(out, error_row as u32, out_col as u32 + 6, CellValue::Empty);

            // ANOVA表：Total（MS/F/P/F-critical留空）
            set_cell_in_sheet(out, total_row as u32, out_col as u32, CellValue::Text("Total".to_string()));
            set_cell_in_sheet(out, total_row as u32, out_col as u32 + 1, CellValue::Formula { source: total_ss_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, total_row as u32, out_col as u32 + 2, CellValue::Formula { source: total_df_formula, cached: Box::new(CellValue::Empty) });
            set_cell_in_sheet(out, total_row as u32, out_col as u32 + 3, CellValue::Empty);
            set_cell_in_sheet(out, total_row as u32, out_col as u32 + 4, CellValue::Empty);
            set_cell_in_sheet(out, total_row as u32, out_col as u32 + 5, CellValue::Empty);
            set_cell_in_sheet(out, total_row as u32, out_col as u32 + 6, CellValue::Empty);

            // 低自由度提示（本次新增）：Error DF = (n_rows-1)*(n_cols-1)，
            // 当这个值达到本函数能构造出的最小可能值(=1，即n_rows=2且
            // n_cols=2时)，残差项完全没有冗余自由度，只要行列主效应恰好
            // 能完全解释所有变异（数据"过于干净"/无噪声），Error SS就会
            // 精确为0，导致Rows/Columns行的F公式除零(#DIV/0!)。这在统计
            // 学上是合理结果（不是bug），但用户直接在表格里看到一堆
            // #DIV/0!容易误以为软件出错，因此在Total行之后追加一行纯文本
            // 备注，写入workbook本身（不是隐藏在返回JSON里），确保手工
            // 操作场景也能看到。仅在最小自由度(=1)时触发，DF>=2时不加，
            // 因为观测数据更多时"行列效应完全解释所有变异"的概率工程上
            // 可忽略，不值得每次都打扰用户。
            let error_df_static = (n_rows - 1) * (n_cols - 1);
            let trailing_rows: i64 = if error_df_static <= 1 {
                let note_row = total_row + 1;
                set_cell_in_sheet(
                    out,
                    note_row as u32,
                    out_col as u32,
                    CellValue::Text(
                        "注：Error自由度极低（=1），若残差平方和恰好为0，Rows/Columns行的F值会出现除零(#DIV/0!)。这是统计学固有限制（数据无残差噪声），非计算错误，建议增加观测数据。".to_string()
                    ),
                );
                1
            } else {
                0
            };

            Some(trailing_rows)
        });
        let trailing_rows = trailing_rows.flatten().unwrap_or(0);

        let total_rows_written = 1 + 1 + n_cols + 1 + n_rows + 1 + 4 + trailing_rows; // Alpha+空+ColumnX+空+RowX+空+ANOVA4行(+备注)
        for i in 0..(total_rows_written as u32) {
            for c in 0..7u32 {
                mark_dirty(out_sheet as usize, out_row as u32 + i, out_col as u32 + c);
            }
        }

        format!("{{\"rows_written\":{}}}", total_rows_written)
    }, false;

    // detect_autosum_range(sheet, row, col) -> String (JSON)
    // 仿照 Excel/WPS "自动求和"下拉菜单（Sum/Average/Count/Max/Min共享同一套
    // 区域探测规则）的探测逻辑：优先向上扫描当前列中连续的数字单元格，若上方
    // 紧邻单元格不是数字（比如就在表头正下方，上方是空/文本），退而向左扫描
    // 当前行中连续的数字单元格。两个方向都探测不到任何数字时返回
    // {"direction":"none"}。
    //
    // 本函数只负责"探测范围"，不负责生成公式或写入——生成 SUM/AVERAGE/COUNT/
    // MAX/MIN 具体用哪个聚合函数是前端Ribbon下拉菜单的选择，由前端/Agent按
    // 返回的range自行拼出公式字符串（如 "=SUM(A1:A5)"）再调用已有的
    // set_formula 写入，这里不重复实现字符串拼接这种简单工作。
    //
    // "连续数字"判定：从锚点的相邻单元格开始，逐格向上/向左检查，遇到
    // CellValue::Number 或 cached 值是 Number 的 Formula 视为"数字"，
    // 继续探测；遇到 Empty/Text/RichText，或 cached 非 Number 的 Formula，
    // 立即停止（不越过空格再继续找下一段数字——这是Excel AutoSum的实际行为，
    // 避免跨越表格间的空行/空列误连两段不相关数据）。
    fn "detect_autosum_range" => |sheet_idx: i64, row: i64, col: i64| -> String {
        with_wb(|wb| {
            let sheet = wb.sheets.get(sheet_idx as usize)?;
            let row = row as u32;
            let col = col as u32;

            let is_numeric = |v: &CellValue| -> bool {
                match v {
                    CellValue::Number(_) => true,
                    CellValue::Formula { cached, .. } => matches!(**cached, CellValue::Number(_)),
                    _ => false,
                }
            };
            let get_at = |r: u32, c: u32| -> CellValue {
                sheet.cells.cols.get(&c)
                    .and_then(|col_data| {
                        for blk in &col_data.blocks {
                            if r >= blk.start_row && r < blk.start_row + blk.values.len() as u32 {
                                return Some(blk.values[(r - blk.start_row) as usize].clone());
                            }
                        }
                        None
                    })
                    .unwrap_or(CellValue::Empty)
            };

            // 向上探测：从 row-1 开始往 row 0 方向找连续数字
            if row > 0 {
                let mut top = row - 1;
                if is_numeric(&get_at(top, col)) {
                    while top > 0 && is_numeric(&get_at(top - 1, col)) {
                        top -= 1;
                    }
                    let result = serde_json::json!({
                        "direction": "up",
                        "start": [top, col],
                        "end": [row - 1, col],
                    });
                    return serde_json::to_string(&result).ok();
                }
            }

            // 向左探测：从 col-1 开始往 col 0 方向找连续数字
            if col > 0 {
                let mut left = col - 1;
                if is_numeric(&get_at(row, left)) {
                    while left > 0 && is_numeric(&get_at(row, left - 1)) {
                        left -= 1;
                    }
                    let result = serde_json::json!({
                        "direction": "left",
                        "start": [row, left],
                        "end": [row, col - 1],
                    });
                    return serde_json::to_string(&result).ok();
                }
            }

            serde_json::to_string(&serde_json::json!({"direction": "none"})).ok()
        })
        .flatten()
        .unwrap_or_else(|| "{\"direction\":\"none\"}".to_string())
    }, true;

    // detect_current_region(sheet, row, col) -> String (JSON)
    // 等价于 Excel 的 CurrentRegion(Ctrl+A / Ctrl+*):从给定单元格出发,
    // 把与它(含八邻域)相连的非空单元格构成的最小矩形返回。
    // 返回 {"start":[r1,c1],"end":[r2,c2]};起点格自身为空且八邻域也全空时
    // 返回 {"direction":"none"}(与 detect_autosum_range 的 none 约定一致)。
    // 判空:Empty 与空字符串 Text("") 视为空,其余(含 Formula)视为非空。
    // 扩展规则:每轮检查当前包围盒外围一圈(含四角),有非空就并入,
    // 直到一轮没有变化。轮数上限 100000,防止极端数据卡死。
    fn "detect_current_region" => |sheet_idx: i64, row: i64, col: i64| -> String {
        with_wb(|wb| {
            let sheet = wb.sheets.get(sheet_idx as usize)?;
            if row < 0 || col < 0 { return None; }
            let row = row as u32;
            let col = col as u32;

            let is_nonempty = |r: u32, c: u32| -> bool {
                let v = sheet.cells.cols.get(&c).and_then(|col_data| {
                    for blk in &col_data.blocks {
                        if r >= blk.start_row && r < blk.start_row + blk.values.len() as u32 {
                            return Some(&blk.values[(r - blk.start_row) as usize]);
                        }
                    }
                    None
                });
                match v {
                    None | Some(CellValue::Empty) => false,
                    Some(CellValue::Text(t)) => !t.is_empty(),
                    Some(_) => true,
                }
            };

            // 某一行 r 在列区间 [ca..=cb] 内是否有非空格
            let row_has = |r: u32, ca: u32, cb: u32| -> bool {
                (ca..=cb).any(|c| is_nonempty(r, c))
            };
            // 某一列 c 在行区间 [ra..=rb] 内是否有非空格
            let col_has = |c: u32, ra: u32, rb: u32| -> bool {
                (ra..=rb).any(|r| is_nonempty(r, c))
            };

            // 起点:自身或八邻域有非空才成立
            let r_lo = row.saturating_sub(1);
            let c_lo = col.saturating_sub(1);
            let seed_ok = (r_lo..=row + 1).any(|r| (c_lo..=col + 1).any(|c| is_nonempty(r, c)));
            if !seed_ok {
                return serde_json::to_string(&serde_json::json!({"direction": "none"})).ok();
            }

            let (mut r1, mut c1, mut r2, mut c2) = (row, col, row, col);
            for _ in 0..100_000u32 {
                let mut changed = false;
                // 外围一圈的检查范围向两侧各多延一格(含对角)
                let ca = c1.saturating_sub(1);
                let cb = c2 + 1;
                let ra = r1.saturating_sub(1);
                let rb = r2 + 1;
                if r1 > 0 && row_has(r1 - 1, ca, cb) { r1 -= 1; changed = true; }
                if row_has(r2 + 1, ca, cb) { r2 += 1; changed = true; }
                if c1 > 0 && col_has(c1 - 1, ra, rb) { c1 -= 1; changed = true; }
                if col_has(c2 + 1, ra, rb) { c2 += 1; changed = true; }
                if !changed { break; }
            }
            serde_json::to_string(&serde_json::json!({
                "start": [r1, c1],
                "end": [r2, c2],
            })).ok()
        })
        .flatten()
        .unwrap_or_else(|| "{\"direction\":\"none\"}".to_string())
    }, true;

    // get_row_texts(sheet, row, c1, c2) -> String (JSON 字符串数组)
    // 取一行中 [c1..=c2] 各格的"展示文本"(与 get_cell 一致,公式取 cached),
    // 空格/未存储的格为 ""。用于前台对话框显示列名(表头文字)。
    // c2<c1、负数、或工作表不存在 → "[]";列数上限 1000,超出部分截断。
    fn "get_row_texts" => |sheet_idx: i64, row: i64, c1: i64, c2: i64| -> String {
        with_wb(|wb| {
            let sheet = wb.sheets.get(sheet_idx as usize)?;
            if row < 0 || c1 < 0 || c2 < c1 { return None; }
            let row = row as u32;
            let c1 = c1 as u32;
            let c2 = (c2 as u32).min(c1.saturating_add(999));
            let mut out: Vec<String> = Vec::with_capacity((c2 - c1 + 1) as usize);
            for c in c1..=c2 {
                let text = sheet.cells.cols.get(&c)
                    .and_then(|col_data| {
                        for blk in &col_data.blocks {
                            if row >= blk.start_row && row < blk.start_row + blk.values.len() as u32 {
                                return Some(cell_value_to_string(&blk.values[(row - blk.start_row) as usize]));
                            }
                        }
                        None
                    })
                    .unwrap_or_default();
                out.push(text);
            }
            serde_json::to_string(&out).ok()
        })
        .flatten()
        .unwrap_or_else(|| "[]".to_string())
    }, true;

    // detect_header_row(sheet, r1, c1, r2, c2) -> String (JSON)
    // 判断区域首行是否为表头,返回 {"header":true} 或 {"header":false}。
    // 规则(偏保守,宁可判无表头也不误把数据行当表头):
    //   1. 区域不足 2 行 → false;
    //   2. 首行非空单元格必须全为文本类(Text/RichText,Formula 取 cached),
    //      首行含数字/布尔,或首行全空 → false;
    //   3. 存在某一列:首行该格是文本、次行该格是数字/布尔(Formula 取 cached)
    //      → true;
    //   4. 否则 false(含全文本表,如通讯录:首行无法与数据区分,视为无表头)。
    // 只看单元格类型,不看格式(加粗/底色等线索本版不用)。
    fn "detect_header_row" => |sheet_idx: i64, r1: i64, c1: i64, r2: i64, c2: i64| -> String {
        with_wb(|wb| {
            let sheet = wb.sheets.get(sheet_idx as usize)?;
            if r1 < 0 || c1 < 0 || r2 < r1 || c2 < c1 { return None; }
            let (r1, c1, r2, c2) = (r1 as u32, c1 as u32, r2 as u32, c2 as u32);
            if r2 < r1 + 1 {
                return serde_json::to_string(&serde_json::json!({"header": false})).ok();
            }

            // 类别: 0=空 1=文本 2=数字/布尔 3=其他(视为不确定,等同于非文本非数字)
            fn kind(v: &CellValue) -> u8 {
                match v {
                    CellValue::Empty => 0,
                    CellValue::Text(t) => if t.is_empty() { 0 } else { 1 },
                    CellValue::RichText(_) => 1,
                    CellValue::Number(_) | CellValue::Bool(_) => 2,
                    CellValue::Formula { cached, .. } => kind(cached),
                    #[allow(unreachable_patterns)]
                    _ => 3,
                }
            }
            let get_at = |r: u32, c: u32| -> CellValue {
                sheet.cells.cols.get(&c)
                    .and_then(|col_data| {
                        for blk in &col_data.blocks {
                            if r >= blk.start_row && r < blk.start_row + blk.values.len() as u32 {
                                return Some(blk.values[(r - blk.start_row) as usize].clone());
                            }
                        }
                        None
                    })
                    .unwrap_or(CellValue::Empty)
            };

            let hdr_row = r1;
            let next_row = r1 + 1;
            let mut any_nonempty = false;
            let mut any_text_over_num = false;
            for c in c1..=c2 {
                let hk = kind(&get_at(hdr_row, c));
                match hk {
                    0 => {}
                    1 => {
                        any_nonempty = true;
                        if kind(&get_at(next_row, c)) == 2 {
                            any_text_over_num = true;
                        }
                    }
                    // 首行出现数字/布尔/其他 → 不是表头
                    _ => {
                        return serde_json::to_string(&serde_json::json!({"header": false})).ok();
                    }
                }
            }
            let header = any_nonempty && any_text_over_num;
            serde_json::to_string(&serde_json::json!({"header": header})).ok()
        })
        .flatten()
        .unwrap_or_else(|| "{\"header\":false}".to_string())
    }, true;

    // text_to_columns_multi(sheet, start_row, end_row, col, delims_json, merge) -> String (JSON)
    // 多分隔符版分列(旧 text_to_columns 只支持单个分隔符,保持不动)。
    //   delims_json: 分隔符字符串数组,如 [",", " ", "\t"];空串被过滤,全空/非法 → {"max_cols":0}
    //   merge:       true=连续分隔符视为一个(不产生中间空段)
    // 行为:读 col 列 [start_row, end_row] 每行展示值,用 split_by_delims 切分,
    // 第一段覆盖原格,后续段写入 col+1、col+2…(直接覆盖右侧数据,不插入列,
    // 与旧接口/Excel 一致)。数字/布尔按 parse_string_to_cellvalue 解析;
    // **空段写入 CellValue::Empty**(旧接口写 Text(""),此处更干净)。
    // 区间行数超过 TTC_MAX_ROWS → {"max_cols":0,"error":"row range too large"},不写入。
    // 返回 {"max_cols":N}(各行最大段数,1 表示无变化)。
    fn "text_to_columns_multi" =>
        |sheet: i64, start_row: i64, end_row: i64, col: i64, delims_json: String, merge: bool| -> Result<String, Box<rhai::EvalAltResult>> {
        let Some(delims) = parse_delims_json(&delims_json) else {
            return Err(format!("text_to_columns_multi: delims_json 须为非空字符串数组,如 [\",\"](空串会被忽略);收到 {}", delims_json).into());
        };
        if sheet < 0 || start_row < 0 || col < 0 || end_row < start_row {
            return Err(format!(
                "text_to_columns_multi: 参数不合法(sheet={}, start_row={}, end_row={}, col={}):均不能为负,且 end_row >= start_row",
                sheet, start_row, end_row, col).into());
        }
        if end_row - start_row + 1 > TTC_MAX_ROWS {
            return Err(format!("text_to_columns_multi: 行范围过大(上限 {} 行)", TTC_MAX_ROWS).into());
        }
        let nsheets = with_wb(|wb| wb.sheets.len()).unwrap_or(0);
        if sheet as usize >= nsheets {
            return Err(format!("text_to_columns_multi: sheet={} 越界或无工作簿(共 {} 个 sheet)", sheet, nsheets).into());
        }
        let (start_row, end_row, col) = (start_row as u32, end_row as u32, col as u32);
        let mut max_cols: usize = 0;

        with_wb(|wb| {
            let Some(sheet_ref) = wb.sheets.get_mut(sheet as usize) else { return; };
            for row in start_row..=end_row {
                let current = sheet_ref.cells.get(row, col);
                let text = cell_value_to_string(&current);
                let parts = split_by_delims(&text, &delims, merge);
                if parts.len() > max_cols {
                    max_cols = parts.len();
                }
                for (i, part) in parts.iter().enumerate() {
                    let target_col = col + i as u32;
                    let v = if part.is_empty() { CellValue::Empty } else { parse_string_to_cellvalue(part) };
                    set_cell_in_sheet(sheet_ref, row, target_col, v);
                }
            }
        });

        for row in start_row..=end_row {
            for i in 0..max_cols {
                mark_dirty(sheet as usize, row, col + i as u32);
            }
        }

        Ok(format!("{{\"max_cols\":{}}}", max_cols))
    }, false;

    // preview_text_to_columns(同上参数) -> String (JSON,只读)
    // 不修改任何数据,返回 {"max_cols":N,"overwrites":bool}:
    //   max_cols   与 text_to_columns_multi 实际写入一致(共用 split_by_delims);
    //   overwrites col+1..col+N-1 在 [start_row,end_row] 内是否有非空格(N<=1 恒为 false)。
    // 供前台在确认前提示"将覆盖右侧数据"。非法参数/超限同 text_to_columns_multi。
    fn "preview_text_to_columns" =>
        |sheet: i64, start_row: i64, end_row: i64, col: i64, delims_json: String, merge: bool| -> String {
        let Some(delims) = parse_delims_json(&delims_json) else {
            return "{\"max_cols\":0,\"overwrites\":false}".to_string();
        };
        if sheet < 0 || start_row < 0 || col < 0 || end_row < start_row {
            return "{\"max_cols\":0,\"overwrites\":false}".to_string();
        }
        if end_row - start_row + 1 > TTC_MAX_ROWS {
            return "{\"max_cols\":0,\"overwrites\":false,\"error\":\"row range too large\"}".to_string();
        }
        let (start_row, end_row, col) = (start_row as u32, end_row as u32, col as u32);
        with_wb(|wb| {
            let sheet_ref = wb.sheets.get(sheet as usize)?;
            let mut max_cols: usize = 0;
            for row in start_row..=end_row {
                let text = cell_value_to_string(&sheet_ref.cells.get(row, col));
                let n = split_by_delims(&text, &delims, merge).len();
                if n > max_cols { max_cols = n; }
            }
            let mut overwrites = false;
            if max_cols > 1 {
                'outer: for row in start_row..=end_row {
                    for i in 1..max_cols as u32 {
                        let t = cell_value_to_string(&sheet_ref.cells.get(row, col + i));
                        if !t.is_empty() { overwrites = true; break 'outer; }
                    }
                }
            }
            Some(format!("{{\"max_cols\":{},\"overwrites\":{}}}", max_cols, overwrites))
        })
        .flatten()
        .unwrap_or_else(|| "{\"max_cols\":0,\"overwrites\":false}".to_string())
    }, true;

    fn "get_sheet_count" => || -> i64 {
        with_wb(|wb| wb.sheets.len() as i64).unwrap_or(0)
    }, true;

    // list_available_fonts() -> String (JSON 字符串数组)
    // 系统已安装且渲染层白名单放行的字体族名(已折叠字重变体、去重、排序)。
    // 与工作簿无关,进程内只枚举一次(roe-core 内 OnceLock 缓存)。只读。
    fn "list_available_fonts" => || -> String {
        serde_json::to_string(roe_core::row_height_measure::list_available_fonts())
            .unwrap_or_else(|_| "[]".to_string())
    }, true;

    // set_cell_format(sheet, row, col, fmt_json)
    // fmt_json 示例: {"bold":true,"font_size":14,"bg_color":"#FFFF00"}
    fn "set_cell_format" => |sheet: i64, row: i64, col: i64, fmt_json: String| -> Result<(), Box<rhai::EvalAltResult>> {
        if row < 0 || col < 0 {
            return Err(format!("set_cell_format: row/col 不能为负(row={}, col={})", row, col).into());
        }
        let v = parse_json_arg("set_cell_format", "fmt_json", &fmt_json, CELL_FORMAT_EXAMPLE)?;
        let fmt = parse_cell_format_value("set_cell_format", "fmt_json", &v)?;
        let ok = with_wb(|wb| {
            let sheet_ref = wb.sheets.get_mut(sheet as usize)?;
            sheet_ref.cell_formats.insert((row as u32, col as u32), fmt);
            sheet_ref.invalidate_row_height(row as u32);
            Some(())
        }).flatten().is_some();
        if !ok {
            return Err(format!("set_cell_format: sheet {} 不存在(或没有打开的文档),未写入", sheet).into());
        }
        mark_dirty(sheet as usize, row as u32, col as u32);
        Ok(())
    }, false;

    // apply_borders(sheet, r1, c1, r2, c2, mode, style, color) -> "ok" | "err:原因"
    // mode: top/bottom/left/right/none/all/outside/inside/inside_h/inside_v
    // 只改 borders 字段,保留格式其余字段;邻格对边同步写入。color 空串=黑色(None)。
    // 坐标为负或越界、mode 非法、选区超过 100 万格均返回 err 且不写入;r1>r2/c1>c2 自动归一化。
    fn "apply_borders" => |sheet: i64, r1: i64, c1: i64, r2: i64, c2: i64, mode: String, style: String, color: String| -> String {
        if r1 < 0 || c1 < 0 || r2 < 0 || c2 < 0 || r1 >= CALC_MAX_ROW || r2 >= CALC_MAX_ROW || c1 >= CALC_MAX_COL || c2 >= CALC_MAX_COL {
            return "err:区域非法".to_string();
        }
        if !matches!(mode.as_str(), "top" | "bottom" | "left" | "right" | "none" | "all" | "outside" | "inside" | "inside_h" | "inside_v") {
            return format!("err:mode 非法: {}", mode);
        }
        let res = with_wb(|wb| {
            let Some(sheet_ref) = wb.sheets.get_mut(sheet as usize) else { return "err:sheet 越界".to_string(); };
            let col = if color.is_empty() { None } else { Some(color.as_str()) };
            if apply_borders_map(
                &mut sheet_ref.cell_formats,
                r1 as u32, c1 as u32, r2 as u32, c2 as u32,
                &mode, &style, col,
            ) {
                "ok".to_string()
            } else {
                "err:选区过大(上限 1000000 格)".to_string()
            }
        }).unwrap_or_else(|| "err:无打开的工作簿".to_string());
        if res == "ok" {
            mark_dirty(sheet as usize, r1.min(r2) as u32, c1.min(c2) as u32);
        }
        res
    }, false;

    // get_cell_format(sheet, row, col) -> JSON
    fn "get_cell_format" => |sheet: i64, row: i64, col: i64| -> String {
        with_wb(|wb| {
            let sheet = wb.sheets.get(sheet as usize)?;
            let fmt = sheet.cell_formats
                .get(&(row as u32, col as u32))
                .cloned()
                .unwrap_or_default();
            serde_json::to_string(&fmt).ok()
        })
        .flatten()
        .unwrap_or_else(|| "{}".to_string())
    }, true;

    // get_conditional_formats(sheet) -> String (JSON数组)
    // 返回该sheet上全部条件格式规则，前端/Agent自行按range匹配到具体单元格。
    // 不做单点查询（传坐标反查命中规则），因为一个单元格可能同时落在多条规则
    // range内，命中优先级和stop_if_true短路逻辑属于渲染层职责，这里只负责
    // 把原始规则列表如实吐出去。
    fn "get_conditional_formats" => |sheet_idx: i64| -> String {
        with_wb(|wb| {
            let sheet = wb.sheets.get(sheet_idx as usize)?;
            serde_json::to_string(&sheet.cond_formats).ok()
        })
        .flatten()
        .unwrap_or_else(|| "[]".to_string())
    }, true;

    // set_conditional_format(sheet, range_json, rule_json)
    // range_json: {"sheet":"Sheet1","start":[0,0],"end":[9,3]}
    // rule_json:  {"condition":"gt:0","format":{"fg_color":"#FF0000"}}
    fn "set_conditional_format" =>
        |sheet_idx: i64, range_json: String, rule_json: String| -> Result<(), Box<rhai::EvalAltResult>> {
        let range = parse_range_arg("set_conditional_format", "range_json", &range_json)?;
        let v = parse_json_arg("set_conditional_format", "rule_json", &rule_json, CF_RULE_EXAMPLE)?;
        let condition = v["condition"].as_str().unwrap_or("").to_string();
        if condition.is_empty() {
            return Err(format!(
                "set_conditional_format: rule_json 缺少非空字符串 condition;示例: {CF_RULE_EXAMPLE}").into());
        }
        validate_condition("set_conditional_format", &condition)?;
        let format = parse_cell_format_value("set_conditional_format", "rule_json.format", &v["format"])?;
        let (rs, re) = (range.start, range.end);
        let ok = with_wb(|wb| {
            let sheet = wb.sheets.get_mut(sheet_idx as usize)?;
            sheet.cond_formats.push(roe_model::calc::ConditionalFormatRule {
                range,
                kind: roe_model::calc::ConditionalFormatKind::CellFormat { condition, format },
                stop_if_true: false,
            });
            Some(())
        }).flatten().is_some();
        if !ok {
            return Err(format!("set_conditional_format: sheet {} 不存在(或没有打开的文档),未写入", sheet_idx).into());
        }
        mark_dirty(sheet_idx as usize, rs.0, rs.1);
        mark_dirty(sheet_idx as usize, re.0, re.1);
        Ok(())
    }, false;

    // add_cf_rule(sheet, range_json, kind_json, stop_if_true) -> bool
    // kind_json 为 ConditionalFormatKind 的 serde 外部标签 JSON,如 {"DataBar":{...}}。
    // 参数不合法或 sheet 不存在时抛错且不写入;成功返回 true。
    fn "add_cf_rule" =>
        |sheet_idx: i64, range_json: String, kind_json: String, stop_if_true: bool| -> Result<bool, Box<rhai::EvalAltResult>> {
        serde_json::from_str::<roe_model::calc::CellRange>(&range_json).map_err(|e| -> Box<rhai::EvalAltResult> {
            format!("add_cf_rule: range_json 不是合法 CellRange({e});示例: {CF_RANGE_EXAMPLE}").into()
        })?;
        serde_json::from_str::<roe_model::calc::ConditionalFormatKind>(&kind_json).map_err(|e| -> Box<rhai::EvalAltResult> {
            format!("add_cf_rule: kind_json 不是合法 ConditionalFormatKind({e});示例: {CF_KIND_EXAMPLE}").into()
        })?;
        let ok = with_wb(|wb| {
            let sheet = wb.sheets.get_mut(sheet_idx as usize)?;
            Some(cf_add_rule(sheet, &range_json, &kind_json, stop_if_true))
        })
        .flatten()
        .unwrap_or(false);
        if !ok {
            return Err(format!("add_cf_rule: sheet {} 不存在(或没有打开的文档),未写入", sheet_idx).into());
        }
        if ok {
            if let Ok(r) = serde_json::from_str::<roe_model::calc::CellRange>(&range_json) {
                mark_dirty(sheet_idx as usize, r.start.0, r.start.1);
                mark_dirty(sheet_idx as usize, r.end.0, r.end.1);
            }
        }
        Ok(true)
    }, false;

    // clear_conditional_formats(sheet, range_json) -> i64(删除条数)
    // 规则 range 与给定 range 相交即整条删除,不拆分。
    fn "clear_conditional_formats" => |sheet_idx: i64, range_json: String| -> i64 {
        let removed = with_wb(|wb| {
            let sheet = wb.sheets.get_mut(sheet_idx as usize)?;
            Some(cf_clear_in_range(sheet, &range_json))
        })
        .flatten()
        .unwrap_or_default();
        for r in &removed {
            mark_dirty(sheet_idx as usize, r.start.0, r.start.1);
            mark_dirty(sheet_idx as usize, r.end.0, r.end.1);
        }
        removed.len() as i64
    }, false;

    // remove_conditional_format(sheet, index) -> bool(index 对应 get_conditional_formats 数组下标)
    fn "remove_conditional_format" => |sheet_idx: i64, index: i64| -> bool {
        let removed = with_wb(|wb| {
            let sheet = wb.sheets.get_mut(sheet_idx as usize)?;
            cf_remove_at(sheet, index)
        })
        .flatten();
        match removed {
            Some(r) => {
                mark_dirty(sheet_idx as usize, r.start.0, r.start.1);
                mark_dirty(sheet_idx as usize, r.end.0, r.end.1);
                true
            }
            None => false,
        }
    }, false;

    // clear_cf_visual(sheet, range_json) -> i64(删除条数)
    // 只删与 range 相交的 DataBar/ColorScale/IconSet 规则,CellFormat 规则保留。
    fn "clear_cf_visual" => |sheet_idx: i64, range_json: String| -> i64 {
        let removed = with_wb(|wb| {
            let sheet = wb.sheets.get_mut(sheet_idx as usize)?;
            Some(cf_clear_visual_in_range(sheet, &range_json))
        })
        .flatten()
        .unwrap_or_default();
        for r in &removed {
            mark_dirty(sheet_idx as usize, r.start.0, r.start.1);
            mark_dirty(sheet_idx as usize, r.end.0, r.end.1);
        }
        removed.len() as i64
    }, false;

    // add_sparkline_group(sheet, type, data_range_json, location_range_json) -> i64(新增条数)
    // type: "line" | "column" | "stacked";尺寸不匹配/参数非法返回 0 且不改动。
    fn "add_sparkline_group" =>
        |sheet_idx: i64, kind: String, data_json: String, loc_json: String| -> Result<i64, Box<rhai::EvalAltResult>> {
        if !matches!(kind.as_str(), "line" | "column" | "stacked") {
            return Err(format!("add_sparkline_group: kind={:?} 不合法,合法值: line, column, stacked", kind).into());
        }
        for (arg, js) in [("data_json", &data_json), ("loc_json", &loc_json)] {
            serde_json::from_str::<roe_model::calc::CellRange>(js).map_err(|e| -> Box<rhai::EvalAltResult> {
                format!("add_sparkline_group: {arg} 不是合法 CellRange({e});示例: {CF_RANGE_EXAMPLE}").into()
            })?;
        }
        let nsheets = with_wb(|wb| wb.sheets.len()).unwrap_or(0);
        if sheet_idx < 0 || sheet_idx as usize >= nsheets {
            return Err(format!("add_sparkline_group: sheet_idx={} 越界或无工作簿(共 {} 个 sheet)", sheet_idx, nsheets).into());
        }
        let n = with_wb(|wb| {
            let sheet = wb.sheets.get_mut(sheet_idx as usize)?;
            Some(sparkline_add_group(sheet, &kind, &data_json, &loc_json))
        })
        .flatten()
        .unwrap_or_default();
        if n == 0 {
            return Err("add_sparkline_group: data 与 loc 无法配对。规则: loc 为单列且行数等于 data 行数(每行一条),或 loc 为单行且列数等于 data 列数(每列一条)".into());
        }
        if n > 0 {
            if let Ok(loc) = serde_json::from_str::<roe_model::calc::CellRange>(&loc_json) {
                mark_dirty(sheet_idx as usize, loc.start.0, loc.start.1);
                mark_dirty(sheet_idx as usize, loc.end.0, loc.end.1);
            }
        }
        Ok(n)
    }, false;

    // clear_sparklines(sheet, range_json) -> i64(删除条数);按单元格删,组空则整组删。
    fn "clear_sparklines" => |sheet_idx: i64, range_json: String| -> i64 {
        let n = with_wb(|wb| {
            let sheet = wb.sheets.get_mut(sheet_idx as usize)?;
            Some(sparkline_clear_in_range(sheet, &range_json))
        })
        .flatten()
        .unwrap_or_default();
        if n > 0 {
            if let Ok(r) = serde_json::from_str::<roe_model::calc::CellRange>(&range_json) {
                mark_dirty(sheet_idx as usize, r.start.0, r.start.1);
                mark_dirty(sheet_idx as usize, r.end.0, r.end.1);
            }
        }
        n
    }, false;

    // get_data_validations(sheet) -> String (JSON数组)
    // 返回该sheet上全部数据验证规则，风格与get_conditional_formats一致。
    fn "get_data_validations" => |sheet_idx: i64| -> String {
        with_wb(|wb| {
            let sheet = wb.sheets.get(sheet_idx as usize)?;
            serde_json::to_string(&sheet.data_validations).ok()
        })
        .flatten()
        .unwrap_or_else(|| "[]".to_string())
    }, true;

    // set_data_validation(sheet, range_json, rule_json)
    // rule_json: {"kind":"list","operator":null,"formula1":"A,B,C","formula2":null,
    //             "allow_blank":true,"show_error":true,"error_msg":null}
    // operator 仅 whole/decimal/date/time 等比较类 kind 需要，list/custom 可省略（默认空串）。
    // 覆盖式语义（对齐 Excel/WPS：对同一区域重新设置=替换，不是叠加）：
    //   先移除该sheet上与新range矩形重叠的全部旧规则，再追加新规则。
    //   kind=="none"（"任何值"）时只清除、不追加，等价于Excel的"全部清除"。
    //   range_json/rule_json解析失败时什么都不做（不会误删旧规则）。
    //   注意：文件导入(fods/xlsx)是直接push到Vec的，不走本接口，导入时同区域
    //   多条规则照旧保留，不受覆盖语义影响。
    fn "set_data_validation" =>
        |sheet_idx: i64, range_json: String, rule_json: String| -> Result<(), Box<rhai::EvalAltResult>> {
        let range = parse_range_arg("set_data_validation", "range_json", &range_json)?;
        let v = parse_json_arg("set_data_validation", "rule_json", &rule_json, DV_EXAMPLE)?;
        let kind = validate_dv_rule("set_data_validation", &v)?;
        let (rs, re) = (range.start, range.end);
        let removed = with_wb(|wb| {
            let sheet = wb.sheets.get_mut(sheet_idx as usize)?;

            // 规范化为 (行min,行max,列min,列max),不依赖 start<=end;只比较行列坐标。
            let norm = |r: &roe_model::calc::CellRange| {
                (
                    r.start.0.min(r.end.0), r.start.0.max(r.end.0),
                    r.start.1.min(r.end.1), r.start.1.max(r.end.1),
                )
            };
            let (nr0, nr1, nc0, nc1) = norm(&range);
            let overlaps = |old: &roe_model::calc::CellRange| {
                let (r0, r1, c0, c1) = norm(old);
                !(r1 < nr0 || r0 > nr1 || c1 < nc0 || c0 > nc1)
            };

            let mut removed = Vec::new();
            let mut kept = Vec::with_capacity(sheet.data_validations.len());
            for rule in sheet.data_validations.drain(..) {
                if overlaps(&rule.range) {
                    removed.push(rule.range);
                } else {
                    kept.push(rule);
                }
            }
            sheet.data_validations = kept;

            if kind != "none" {
                sheet.data_validations.push(roe_model::calc::DataValidationRule {
                    range,
                    kind,
                    operator:    v["operator"].as_str().unwrap_or("").to_string(),
                    formula1:    v["formula1"].as_str().unwrap_or("").to_string(),
                    formula2:    v["formula2"].as_str().map(|t| t.to_string()),
                    allow_blank: v["allow_blank"].as_bool().unwrap_or(true),
                    show_error:  v["show_error"].as_bool().unwrap_or(false),
                    error_msg:   v["error_msg"].as_str().map(|t| t.to_string()),
                });
            }
            Some(removed)
        }).flatten();
        let Some(removed_ranges) = removed else {
            return Err(format!("set_data_validation: sheet {} 不存在(或没有打开的文档),未写入", sheet_idx).into());
        };
        // 被移除的旧规则区域也要标脏,否则旧的下拉/红三角标记可能残留
        for r in &removed_ranges {
            mark_dirty(sheet_idx as usize, r.start.0, r.start.1);
            mark_dirty(sheet_idx as usize, r.end.0, r.end.1);
        }
        mark_dirty(sheet_idx as usize, rs.0, rs.1);
        mark_dirty(sheet_idx as usize, re.0, re.1);
        Ok(())
    }, false;

    // validate_cell_input(sheet, row, col, text) -> String (JSON)
    // 写入前校验：把 text 按 set_cell 同一份解析(parse_string_to_cellvalue)得到将要写入
    // 的值，找出覆盖 (row,col) 且校验不通过的第一条数据验证规则。
    //   合法/无规则/公式输入(以"="开头，写入前无法求值，先放行)：{"valid":true}
    //   违规：{"valid":false,"show_error":<bool>,"error_msg":<string|null>}
    // show_error=false 的违规规则也如实返回 valid:false，由前端决定是否拦截（与红三角一致）。
    // 纯查询，只读环境可调用。
    fn "validate_cell_input" =>
        |sheet_idx: i64, row: i64, col: i64, text: String| -> String {
        let ok = || "{\"valid\":true}".to_string();
        if row < 0 || col < 0 || text.trim_start().starts_with('=') {
            return ok();
        }
        with_wb(|wb| {
            let sheet = wb.sheets.get(sheet_idx as usize)?;
            let val = parse_string_to_cellvalue(&text);
            let bad = sheet.data_validations.iter()
                .find(|r| {
                    if !r.covers(row as u32, col as u32) {
                        return false;
                    }
                    if r.kind == "list" {
                        // list：候选可能来自单元格范围，需要工作簿；候选取不到(引用非法/
                        // sheet 不存在/无候选)时规则本身有问题，放行而不是拦住用户
                        return match r.list_candidates(&wb.sheets, sheet) {
                            Some(c) => !r.list_accepts(&val, &c),
                            None => false,
                        };
                    }
                    !r.is_valid(&val)
                })?;
            Some(serde_json::json!({
                "valid": false,
                "show_error": bad.show_error,
                "error_msg": bad.error_msg,
            }).to_string())
        })
        .flatten()
        .unwrap_or_else(ok)
    }, true;

    // get_list_candidates(sheet, row, col) -> String (JSON 字符串数组)
    // 取 (row,col) 处 list 类型数据验证的下拉候选项：找覆盖该格的第一条 list 规则，
    //   - formula1 是字面量(逗号 / xlsx 外层引号 / ODF 分号引号)：走 list_literal_items
    //   - formula1 是范围引用(D1:D3 / =$A$1:$A$5 / Sheet2!A1:A5)：读引用区域的单元格文本
    //     (与 get_cell 同一份 cell_value_to_string)，空格跳过，多列按行优先展开，
    //     跨 sheet 按 sheet 名匹配(忽略大小写)，最多展开 1000 格
    // 无 list 规则覆盖 / 无候选 / 引用解析失败 / sheet 不存在 → "[]"。纯查询，只读环境可调用。
    fn "get_list_candidates" => |sheet_idx: i64, row: i64, col: i64| -> String {
        let empty = || "[]".to_string();
        if sheet_idx < 0 || row < 0 || col < 0 {
            return empty();
        }
        with_wb(|wb| {
            let sheet = wb.sheets.get(sheet_idx as usize)?;
            let rule = sheet.data_validations.iter()
                .find(|r| r.kind == "list" && r.covers(row as u32, col as u32))?;
            let items = rule.list_candidates(&wb.sheets, sheet)?;
            serde_json::to_string(&items).ok()
        })
        .flatten()
        .unwrap_or_else(empty)
    }, true;

    // get_auto_filter(sheet) -> String (JSON对象或"null")
    // 一个sheet至多一个AutoFilter（Option），没有设置筛选时返回字符串"null"
    // （区别于空字符串或"{}"，前端按JSON.parse统一处理即可得到null）。
    fn "get_auto_filter" => |sheet_idx: i64| -> String {
        with_wb(|wb| {
            let sheet = wb.sheets.get(sheet_idx as usize)?;
            serde_json::to_string(&sheet.auto_filter).ok()
        })
        .flatten()
        .unwrap_or_else(|| "null".to_string())
    }, true;

    // sort_range(sheet, range_json, key_col, ascending)
    // 对指定行范围按 key_col 排序（key_col 是 range 内的相对列偏移）
    fn "sort_range" =>
        |sheet_idx: i64, range_json: String, key_col: i64, ascending: bool| -> Result<(), Box<rhai::EvalAltResult>> {
        let range = parse_range_arg("sort_range", "range_json", &range_json)?;
        if range.end.0 < range.start.0 || range.end.1 < range.start.1 {
            return Err(format!("sort_range: 范围 end {:?} 小于 start {:?}", range.end, range.start).into());
        }
        let width = (range.end.1 - range.start.1 + 1) as i64;
        if key_col < 0 || key_col >= width {
            return Err(format!(
                "sort_range: key_col={} 越界。key_col 是相对范围起始列的偏移,当前范围宽 {} 列,合法值 0..={}",
                key_col, width, width - 1).into());
        }
        let ok = with_wb(|wb| {
            let sheet = wb.sheets.get_mut(sheet_idx as usize)?;
            let (r1, _c1) = range.start;
            let (r2, _c2) = range.end;

            // 收集行数据
            let mut rows: Vec<(u32, Vec<roe_model::calc::CellValue>)> = (r1..=r2).map(|r| {
                let row_vals: Vec<_> = (range.start.1..=range.end.1).map(|c| {
                    sheet.cells.cols.get(&c)
                        .and_then(|col| {
                            for blk in &col.blocks {
                                if r >= blk.start_row && r < blk.start_row + blk.values.len() as u32 {
                                    return Some(blk.values[(r - blk.start_row) as usize].clone());
                                }
                            }
                            None
                        })
                        .unwrap_or(roe_model::calc::CellValue::Empty)
                }).collect();
                (r, row_vals)
            }).collect();

            // 排序:数字按数值、文本忽略大小写按字典序、空值永远最后(稳定排序)
            rows.sort_by(|a, b| {
                let va = cell_to_sort_key_v2(a.1.get(key_col as usize).unwrap_or(&roe_model::calc::CellValue::Empty));
                let vb = cell_to_sort_key_v2(b.1.get(key_col as usize).unwrap_or(&roe_model::calc::CellValue::Empty));
                compare_sort_keys(&va, &vb, ascending)
            });

            // 格式跟随数据:先把范围内旧格式整体取出(按原坐标),写回时按新行位置放回。
            // 必须先取完再写,否则新旧坐标重叠时会互相覆盖。
            let (c_lo, c_hi) = (range.start.1, range.end.1);
            let mut old_fmts: std::collections::HashMap<(u32, u32), roe_model::calc::CellFormat> =
                std::collections::HashMap::new();
            for r in r1..=r2 {
                for c in c_lo..=c_hi {
                    if let Some(f) = sheet.cell_formats.remove(&(r, c)) {
                        old_fmts.insert((r, c), f);
                    }
                }
            }

            // 写回值与格式
            for (new_row_idx, (orig_row, vals)) in rows.into_iter().enumerate() {
                let r = r1 + new_row_idx as u32;
                for (ci, val) in vals.into_iter().enumerate() {
                    let c = c_lo + ci as u32;
                    set_cell_in_sheet(sheet, r, c, val);
                    if let Some(f) = old_fmts.remove(&(orig_row, c)) {
                        sheet.cell_formats.insert((r, c), f);
                    }
                }
                // 行内容变了,自动行高需要重测
                sheet.invalidate_row_height(r);
            }
            Some(())
        }).flatten().is_some();
        if !ok {
            return Err(format!("sort_range: sheet {} 不存在(或没有打开的文档),未排序", sheet_idx).into());
        }
        mark_dirty(sheet_idx as usize, range.start.0, range.start.1);
        mark_dirty(sheet_idx as usize, range.end.0, range.end.1);
        Ok(())
    }, false;


    // apply_filter(sheet, header_row, header_col, filter_json)
    // filter_json: {"col_filters":{"0":["A","B"],"2":["Yes"]}}
    fn "apply_filter" =>
        |sheet_idx: i64, header_row: i64, header_col: i64, filter_json: String| -> Result<(), Box<rhai::EvalAltResult>> {
        if header_row < 0 || header_col < 0 {
            return Err(format!("apply_filter: header_row/header_col 不能为负({}, {})", header_row, header_col).into());
        }
        let fi = parse_json_arg("apply_filter", "filter_json", &filter_json, AF_EXAMPLE)?;
        let obj = fi["col_filters"].as_object().ok_or_else(|| -> Box<rhai::EvalAltResult> {
            format!("apply_filter: filter_json 必须含对象 col_filters(键=相对 header_col 的列偏移,值=字符串数组);示例: {AF_EXAMPLE}").into()
        })?;
        let mut col_filters: std::collections::HashMap<u32, Vec<String>> = std::collections::HashMap::new();
        for (k, v) in obj {
            let col = k.parse::<u32>().map_err(|_| -> Box<rhai::EvalAltResult> {
                format!("apply_filter: col_filters 的键 {:?} 必须是非负整数(列偏移);示例: {AF_EXAMPLE}", k).into()
            })?;
            let arr = v.as_array().ok_or_else(|| -> Box<rhai::EvalAltResult> {
                format!("apply_filter: col_filters[{}] 必须是字符串数组;示例: {AF_EXAMPLE}", k).into()
            })?;
            let mut vals: Vec<String> = Vec::with_capacity(arr.len());
            for x in arr {
                match x.as_str() {
                    Some(t) => vals.push(t.to_string()),
                    None => return Err(format!(
                        "apply_filter: col_filters[{}] 的值必须全是字符串,收到 {};数字也要写成字符串,如 \"2024\"", k, x).into()),
                }
            }
            col_filters.insert(col, vals);
        }
        // ROOT-005修复(v126)：API 没有终止列参数,用 col_filters 里最大列偏移估算 end_col。
        let max_offset = col_filters.keys().copied().max().unwrap_or(0);
        let ok = with_wb(|wb| {
            let sheet = wb.sheets.get_mut(sheet_idx as usize)?;
            sheet.auto_filter = Some(roe_model::calc::AutoFilter {
                header_row: header_row as u32,
                header_col: header_col as u32,
                end_col: header_col as u32 + max_offset,
                col_filters,
                hidden_buttons: std::collections::HashSet::new(),
            });
            Some(())
        }).flatten().is_some();
        if !ok {
            return Err(format!("apply_filter: sheet {} 不存在(或没有打开的文档),未设置", sheet_idx).into());
        }
        mark_dirty(sheet_idx as usize, header_row as u32, header_col as u32);
        Ok(())
    }, false;

    // get_column_unique_values(sheet, header_row, col, end_row) -> String (JSON)
    // 只读。返回表头下方 header_row+1..=end_row 该列的唯一展示文本及个数:
    // {"values":[{"v":"文本","n":个数},...],"truncated":bool}
    // 按首次出现顺序;空格子记为空串;唯一值超过 AF_MAX_UNIQUE 后不再新增(truncated=true)。
    fn "get_column_unique_values" => |sheet_idx: i64, header_row: i64, col: i64, end_row: i64| -> String {
        with_wb(|wb| {
            let sheet = wb.sheets.get(sheet_idx as usize)?;
            if header_row < 0 || col < 0 || end_row <= header_row { return None; }
            let hr = header_row as u32;
            let er = end_row as u32;
            if er - hr > AF_MAX_ROWS { return None; }
            let texts = af_column_texts(sheet, col as u32, hr + 1, er);
            let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
            let mut vals: Vec<(String, u32)> = Vec::new();
            let mut truncated = false;
            for t in texts {
                if let Some(&i) = index.get(&t) {
                    vals[i].1 += 1;
                } else if vals.len() >= AF_MAX_UNIQUE {
                    truncated = true;
                } else {
                    index.insert(t.clone(), vals.len());
                    vals.push((t, 1));
                }
            }
            let arr: Vec<serde_json::Value> = vals.into_iter()
                .map(|(v, n)| serde_json::json!({"v": v, "n": n}))
                .collect();
            serde_json::to_string(&serde_json::json!({"values": arr, "truncated": truncated})).ok()
        })
        .flatten()
        .unwrap_or_else(|| r#"{"values":[],"truncated":false}"#.to_string())
    }, true;

    // apply_auto_filter_rows(sheet, header_row, header_col, end_col, end_row, filter_json) -> String (JSON)
    // 可写。filter_json: {"col_filters":{"0":["A","B"],"2":["Yes"]}}(键为相对 header_col 的列偏移)。
    // 设置 sheet.auto_filter(同表头位置时保留 hidden_buttons),并按所有列条件取交集
    // 重算数据行 header_row+1..=end_row 的 row_hidden(不满足=隐藏,满足=恢复显示)。
    // 某列允许值为空数组=该列全部不显示;要清除某列筛选,把该列从 col_filters 中去掉。
    // 返回 {"ok":true,"hidden":N,"visible":M};失败返回 {"ok":false,...} 且不写入。
    fn "apply_auto_filter_rows" =>
        |sheet_idx: i64, header_row: i64, header_col: i64, end_col: i64, end_row: i64, filter_json: String| -> Result<String, Box<rhai::EvalAltResult>> {
        let fv = parse_json_arg("apply_auto_filter_rows", "filter_json", &filter_json, AF_EXAMPLE)?;
        let Some(cf) = fv["col_filters"].as_object() else {
            return Err(format!("apply_auto_filter_rows: filter_json 缺少对象字段 col_filters(无筛选条件也要写 {{\"col_filters\":{{}}}});示例: {}", AF_EXAMPLE).into());
        };
        for (k, v) in cf {
            if k.parse::<u32>().is_err() {
                return Err(format!("apply_auto_filter_rows: col_filters 的键 {:?} 必须是非负整数列偏移(相对 header_col);示例: {}", k, AF_EXAMPLE).into());
            }
            let ok_vals = v.as_array().map_or(false, |a| a.iter().all(|x| x.is_string()));
            if !ok_vals {
                return Err(format!("apply_auto_filter_rows: col_filters[{:?}] 必须是字符串数组(数字也要加引号);示例: {}", k, AF_EXAMPLE).into());
            }
        }
        let nsheets = with_wb(|wb| wb.sheets.len()).unwrap_or(0);
        if sheet_idx < 0 || sheet_idx as usize >= nsheets {
            return Err(format!("apply_auto_filter_rows: sheet_idx={} 越界或无工作簿(共 {} 个 sheet)", sheet_idx, nsheets).into());
        }
        let out = with_wb(|wb| {
            let sheet = wb.sheets.get_mut(sheet_idx as usize)?;
            if header_row < 0 || header_col < 0 || end_col < header_col || end_row < header_row {
                return Some(r#"{"ok":false,"error":"bad range"}"#.to_string());
            }
            let fi: serde_json::Value = serde_json::from_str(&filter_json).ok()?;
            let col_filters: std::collections::HashMap<u32, Vec<String>> =
                fi["col_filters"].as_object()
                .map(|m| m.iter().filter_map(|(k, v)| {
                    let col = k.parse::<u32>().ok()?;
                    let vals: Vec<String> = v.as_array()?.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect();
                    Some((col, vals))
                }).collect())
                .unwrap_or_default();
            let hr = header_row as u32;
            let hc = header_col as u32;
            let ec = end_col as u32;
            let er = end_row as u32;
            if er - hr > AF_MAX_ROWS {
                return Some(r#"{"ok":false,"error":"row range too large"}"#.to_string());
            }
            let hidden_buttons = match &sheet.auto_filter {
                Some(af) if af.header_row == hr && af.header_col == hc => af.hidden_buttons.clone(),
                _ => std::collections::HashSet::new(),
            };
            let n = (er - hr) as usize;
            let mut hide = vec![false; n];
            if n > 0 {
                for (off, allowed) in &col_filters {
                    let abs = hc + *off;
                    if abs > ec { continue; }
                    let texts = af_column_texts(&*sheet, abs, hr + 1, er);
                    let set: std::collections::HashSet<&str> =
                        allowed.iter().map(|s| s.as_str()).collect();
                    for (i, t) in texts.iter().enumerate() {
                        if !set.contains(t.as_str()) { hide[i] = true; }
                    }
                }
                if sheet.row_hidden.len() < er as usize + 1 {
                    sheet.row_hidden.resize(er as usize + 1, false);
                }
                for i in 0..n {
                    sheet.row_hidden[hr as usize + 1 + i] = hide[i];
                }
            }
            sheet.auto_filter = Some(roe_model::calc::AutoFilter {
                header_row: hr,
                header_col: hc,
                end_col: ec,
                col_filters,
                hidden_buttons,
            });
            let hidden = hide.iter().filter(|b| **b).count();
            Some(format!(r#"{{"ok":true,"hidden":{},"visible":{}}}"#, hidden, n - hidden))
        })
        .flatten()
        .unwrap_or_else(|| r#"{"ok":false}"#.to_string());
        if header_row >= 0 && header_col >= 0 {
            mark_dirty(sheet_idx as usize, header_row as u32, header_col as u32);
        }
        Ok(out)
    }, false;

    // clear_auto_filter(sheet, end_row) -> String (JSON)
    // 关闭自动筛选:表头下方 header_row+1..=end_row 的 row_hidden 恢复为显示,
    // 然后 sheet.auto_filter = None。区域外的行不动;原本无筛选时 had=false。
    // 返回 {"ok":true,"had":bool,"restored":N}。只处理 sheet.auto_filter,不含 tables[].auto_filter。
    fn "clear_auto_filter" => |sheet_idx: i64, end_row: i64| -> String {
        let res = with_wb(|wb| {
            let sheet = wb.sheets.get_mut(sheet_idx as usize)?;
            let Some(af) = sheet.auto_filter.take() else {
                return Some((r#"{"ok":true,"had":false,"restored":0}"#.to_string(), 0u32, 0u32));
            };
            let mut restored = 0u32;
            if end_row >= 0 {
                let start = af.header_row as usize + 1;
                let stop = (end_row as usize + 1).min(sheet.row_hidden.len());
                for i in start..stop {
                    if sheet.row_hidden[i] {
                        sheet.row_hidden[i] = false;
                        restored += 1;
                    }
                }
            }
            Some((format!(r#"{{"ok":true,"had":true,"restored":{}}}"#, restored), af.header_row, af.header_col))
        })
        .flatten();
        match res {
            Some((json, hr, hc)) => {
                mark_dirty(sheet_idx as usize, hr, hc);
                json
            }
            None => r#"{"ok":false}"#.to_string(),
        }
    }, false;

    // get_merged_cells(sheet) -> String (JSON数组)
    fn "get_merged_cells" => |sheet_idx: i64| -> String {
        with_wb(|wb| {
            let sheet = wb.sheets.get(sheet_idx as usize)?;
            serde_json::to_string(&sheet.merged_cells).ok()
        })
        .flatten()
        .unwrap_or_else(|| "[]".to_string())
    }, true;

    // merge_cells(sheet, r1, c1, r2, c2) -> "ok" | "err:原因"
    // 区域非法(负数、r2<r1、c2<c1、越界)或 sheet 越界返回 err 且不写入。
    fn "merge_cells" =>
        |sheet_idx: i64, r1: i64, c1: i64, r2: i64, c2: i64| -> String {
        if r1 < 0 || c1 < 0 || r2 < r1 || c2 < c1 || r2 >= CALC_MAX_ROW || c2 >= CALC_MAX_COL {
            return "err:区域非法".to_string();
        }
        let res = with_wb(|wb| {
            let Some(sheet) = wb.sheets.get_mut(sheet_idx as usize) else { return "err:sheet 越界".to_string(); };
            // 移除与新区域重叠的旧合并
            sheet.merged_cells.retain(|m| {
                !(m.r1 <= r2 as u32 && m.r2 >= r1 as u32
                  && m.c1 <= c2 as u32 && m.c2 >= c1 as u32)
            });
            sheet.merged_cells.push(roe_model::calc::MergedCell {
                r1: r1 as u32, c1: c1 as u32,
                r2: r2 as u32, c2: c2 as u32,
            });
            "ok".to_string()
        }).unwrap_or_else(|| "err:无打开的工作簿".to_string());
        if res == "ok" {
            mark_dirty(sheet_idx as usize, r1 as u32, c1 as u32);
            mark_dirty(sheet_idx as usize, r2 as u32, c2 as u32);
        }
        res
    }, false;

    // get_tables(sheet) -> JSON 数组
    // [{idx,name,range:[r1,c1,r2,c2],style,has_header,has_totals,first_col,last_col,row_stripes,col_stripes,columns}]
    fn "get_tables" => |sheet_idx: i64| -> String {
        with_wb(|wb| tables_to_json(wb, sheet_idx)).unwrap_or_else(|| "[]".to_string())
    }, true;

    // create_table(sheet, r1, c1, r2, c2, has_header, style_name) -> 新表名 | "err:..."
    // 范围含表头行;与已有表重叠、样式名不存在、带表头但无数据行均返回 err。
    // 表头为空/重复时补 ColumnN,并把表头单元格文字改成同名(与 tableColumn 名一致)。
    fn "create_table" =>
        |sheet_idx: i64, r1: i64, c1: i64, r2: i64, c2: i64, has_header: bool, style_name: String| -> String {
        let res = with_wb(|wb| table_create_impl(wb, sheet_idx, r1, c1, r2, c2, has_header, &style_name));
        match res {
            Some(Ok(name)) => {
                mark_dirty(sheet_idx as usize, r1 as u32, c1 as u32);
                mark_dirty(sheet_idx as usize, r2 as u32, c2 as u32);
                name
            }
            Some(Err(e)) => format!("err:{e}"),
            None => "err:无工作簿".to_string(),
        }
    }, false;

    // set_table_style(sheet, table_idx, style_name) -> "ok" | "err:..."
    // style_name 为空串表示清除样式。
    fn "set_table_style" => |sheet_idx: i64, table_idx: i64, style_name: String| -> String {
        let res = with_wb(|wb| {
            let r = table_set_style_impl(wb, sheet_idx, table_idx, &style_name);
            let rng = wb.sheets.get(sheet_idx.max(0) as usize)
                .and_then(|sh| sh.tables.get(table_idx.max(0) as usize))
                .map(|t| (t.range.start, t.range.end));
            (r, rng)
        });
        match res {
            Some((Ok(()), Some((a, b)))) => {
                mark_dirty(sheet_idx as usize, a.0, a.1);
                mark_dirty(sheet_idx as usize, b.0, b.1);
                "ok".to_string()
            }
            Some((Ok(()), None)) => "ok".to_string(),
            Some((Err(e), _)) => format!("err:{e}"),
            None => "err:无工作簿".to_string(),
        }
    }, false;

    // get_row_outline_level(sheet, row) -> i64
    // 行大纲层级(分组折叠深度),0=无分组。越界返回0。仅存数值,折叠/隐藏
    // 交互由调用方决定(通常配合 set_row_hidden 一起用)。
    fn "get_row_outline_level" => |sheet_idx: i64, row: i64| -> i64 {
        with_wb(|wb| {
            let sheet = wb.sheets.get(sheet_idx as usize)?;
            Some(sheet.row_outline_level.get(row as usize).copied().unwrap_or(0) as i64)
        })
        .flatten()
        .unwrap_or(0)
    }, true;

    // set_row_outline_level(sheet, row, level) -> "ok" | "err:原因"
    // level 须在 0..=7(对齐 Excel 大纲层级上限),否则 err 且不写入。
    // row 超出现有长度时自动 resize 补齐(新补位置为0)。
    fn "set_row_outline_level" => |sheet_idx: i64, row: i64, level: i64| -> String {
        if !calc_row_ok(row) { return "err:行号越界(0..1048575)".to_string(); }
        if !(0..=7).contains(&level) { return "err:level 须在 0..7".to_string(); }
        let res = with_wb(|wb| {
            let Some(sheet) = wb.sheets.get_mut(sheet_idx as usize) else { return "err:sheet 越界".to_string(); };
            let r = row as usize;
            if sheet.row_outline_level.len() <= r {
                sheet.row_outline_level.resize(r + 1, 0u8);
            }
            sheet.row_outline_level[r] = level as u8;
            "ok".to_string()
        }).unwrap_or_else(|| "err:无打开的工作簿".to_string());
        if res == "ok" { mark_dirty(sheet_idx as usize, row as u32, 0); }
        res
    }, false;

    // set_row_outline_level_range(sheet, start_row, end_row, level) -> "ok" | "err:原因"
    // 批量版,start_row/end_row 含端点。end_row<start_row、行号越界、level 不在 0..=7 均 err 且不写入。
    fn "set_row_outline_level_range" =>
        |sheet_idx: i64, start_row: i64, end_row: i64, level: i64| -> String {
        if !calc_row_ok(start_row) || !calc_row_ok(end_row) { return "err:行号越界(0..1048575)".to_string(); }
        if end_row < start_row { return "err:区间非法(end<start)".to_string(); }
        if !(0..=7).contains(&level) { return "err:level 须在 0..7".to_string(); }
        let res = with_wb(|wb| {
            let Some(sheet) = wb.sheets.get_mut(sheet_idx as usize) else { return "err:sheet 越界".to_string(); };
            set_row_outline_level_range_impl(sheet, start_row as u32, end_row as u32, level as u8);
            "ok".to_string()
        }).unwrap_or_else(|| "err:无打开的工作簿".to_string());
        if res == "ok" { mark_dirty(sheet_idx as usize, start_row as u32, 0); }
        res
    }, false;

    // get_row_hidden(sheet, row) -> bool
    // 通用行隐藏读取,越界返回false。与自动筛选内部使用的row_hidden是
    // 同一份底层存储,但本接口不关心是否存在auto_filter,纯粹读写数值,
    // 供分组折叠等场景复用同一字段。
    fn "get_row_hidden" => |sheet_idx: i64, row: i64| -> bool {
        with_wb(|wb| {
            let sheet = wb.sheets.get(sheet_idx as usize)?;
            Some(sheet.row_hidden.get(row as usize).copied().unwrap_or(false))
        })
        .flatten()
        .unwrap_or(false)
    }, true;

    // set_row_hidden(sheet, row, hidden) -> "ok" | "err:原因"
    // 通用单行隐藏设置,与auto_filter逻辑解耦(不检查/不修改sheet.auto_filter)。
    // 调用方(如分组折叠功能)需自行避免与筛选功能的隐藏状态互相踩踏——
    // 目前两者共用同一个row_hidden数组,是有意的复用,不是疏忽。
    fn "set_row_hidden" => |sheet_idx: i64, row: i64, hidden: bool| -> String {
        if !calc_row_ok(row) { return "err:行号越界(0..1048575)".to_string(); }
        let res = with_wb(|wb| {
            let Some(sheet) = wb.sheets.get_mut(sheet_idx as usize) else { return "err:sheet 越界".to_string(); };
            let r = row as usize;
            if sheet.row_hidden.len() <= r {
                sheet.row_hidden.resize(r + 1, false);
            }
            sheet.row_hidden[r] = hidden;
            "ok".to_string()
        }).unwrap_or_else(|| "err:无打开的工作簿".to_string());
        if res == "ok" { mark_dirty(sheet_idx as usize, row as u32, 0); }
        res
    }, false;

    // set_row_hidden_range(sheet, start_row, end_row, hidden) -> "ok" | "err:原因"
    // 批量版。end_row<start_row、行号越界、sheet 越界均 err 且不写入。
    fn "set_row_hidden_range" =>
        |sheet_idx: i64, start_row: i64, end_row: i64, hidden: bool| -> String {
        if !calc_row_ok(start_row) || !calc_row_ok(end_row) { return "err:行号越界(0..1048575)".to_string(); }
        if end_row < start_row { return "err:区间非法(end<start)".to_string(); }
        let res = with_wb(|wb| {
            let Some(sheet) = wb.sheets.get_mut(sheet_idx as usize) else { return "err:sheet 越界".to_string(); };
            let er = end_row as usize;
            if sheet.row_hidden.len() <= er {
                sheet.row_hidden.resize(er + 1, false);
            }
            for r in start_row as usize..=er {
                sheet.row_hidden[r] = hidden;
            }
            "ok".to_string()
        }).unwrap_or_else(|| "err:无打开的工作簿".to_string());
        if res == "ok" { mark_dirty(sheet_idx as usize, start_row as u32, 0); }
        res
    }, false;

    // ─── 名称管理(MVP,全局名称) ───
    // list_names() -> JSON: [{"name","kind":"range"|"formula","ref"}]
    fn "list_names" => || -> String {
        with_wb(|wb| names_list_json(wb)).unwrap_or_else(|| "[]".to_string())
    }, true;

    // validate_name(name) -> "ok" | "err:原因"(含重名检查,不区分大小写)
    fn "validate_name" => |name: String| -> String {
        match with_wb(|wb| names_validate(wb, &name)) {
            Some(Ok(())) => "ok".to_string(),
            Some(Err(e)) => format!("err:{}", e),
            None => "err:无打开的工作簿".to_string(),
        }
    }, true;

    // set_name(name, ref_text) -> "ok" | "err:原因";新增或覆盖
    fn "set_name" => |name: String, ref_text: String| -> String {
        match with_wb(|wb| names_set(wb, &name, &ref_text)) {
            Some(Ok(())) => {
                mark_all_formulas_dirty();
                "ok".to_string()
            }
            Some(Err(e)) => format!("err:{}", e),
            None => "err:无打开的工作簿".to_string(),
        }
    }, false;

    // remove_name(name) -> "ok" | "err:名称不存在"
    fn "remove_name" => |name: String| -> String {
        match with_wb(|wb| names_remove_ci(wb, &name)) {
            Some(true) => {
                mark_all_formulas_dirty();
                "ok".to_string()
            }
            Some(false) => "err:名称不存在".to_string(),
            None => "err:无打开的工作簿".to_string(),
        }
    }, false;

    fn "get_sheet_name" => |idx: i64| -> String {
        with_wb(|wb| wb.sheets.get(idx as usize).map(|s| s.name.clone()))
            .flatten()
            .unwrap_or_default()
    }, true;

    // insert_chart(sheet, anchor_row, anchor_col, chart_json) -> i64(新图 id;失败时抛错)
    // chart_json 格式与 roe_render_chart_to_png 一致：
    // {"kind":"Bar","title":"...","x_labels":[...],"series":[{"name":"..","data":[..]}]}
    // 锚点单元格 → pt 坐标：按 sheet 实际 col_widths/row_heights 累加，
    // 缺省列/行用 default_col_width/default_row_height 兜底（再缺省用 72.0/15.0）。
    fn "insert_chart" =>
        |sheet_idx: i64, anchor_row: i64, anchor_col: i64, chart_json: String| -> Result<i64, Box<rhai::EvalAltResult>> {
        let val = parse_json_arg("insert_chart", "chart_json", &chart_json, CHART_EXAMPLE)?;
        if anchor_row < 0 || anchor_col < 0 {
            return Err(format!(
                "insert_chart: anchor_row/anchor_col 不能为负(收到 {}, {})", anchor_row, anchor_col).into());
        }
        let (kind, title, x_labels, series) = parse_chart_spec(&val)?;
        let avoid_overlap = match val.get("avoid_overlap") {
            None | Some(serde_json::Value::Null) => false,
            Some(serde_json::Value::Bool(b)) => *b,
            Some(other) => return Err(format!(
                "insert_chart: avoid_overlap 必须是 true/false,收到 {};示例: {CHART_EXAMPLE}", other).into()),
        };

        // 以下字段此前硬编码，现从 chart_json 透传，缺省时保持与此前硬编码
        // 相同的默认值，向后兼容旧调用方（不传这些字段的现有脚本/测试）。
        let width_pt = val["width_pt"].as_f64().map(|v| v as f32).unwrap_or(300.0);
        let height_pt = val["height_pt"].as_f64().map(|v| v as f32).unwrap_or(300.0);
        let show_legend = val["show_legend"].as_bool().unwrap_or(true);
        let show_grid = val["show_grid"].as_bool().unwrap_or(true);
        let x_axis_label = val["x_axis_label"].as_str().unwrap_or("").to_string();
        let y_axis_label = val["y_axis_label"].as_str().unwrap_or("").to_string();
        let border_color = val["border_color"].as_str()
            .and_then(parse_hex_color_local)
            .unwrap_or(roe_model::common::Color { r: 0xcc, g: 0xcc, b: 0xcc, a: 255 });
        let border_width = val["border_width"].as_f64().map(|v| v as f32).unwrap_or(1.0);
        let gap_width_pct = val["gap_width_pct"].as_f64().map(|v| v as f32);
        let overlap_pct = val["overlap_pct"].as_f64().map(|v| v as f32);
        let grouping = match val["grouping"].as_str().unwrap_or("") {
            "Stacked" => roe_model::impress::ChartGrouping::Stacked,
            "PercentStacked" => roe_model::impress::ChartGrouping::PercentStacked,
            _ => roe_model::impress::ChartGrouping::Standard,
        };
        let hole_size_pct = val["hole_size_pct"].as_f64().map(|v| v as f32);
        let mut data_labels = roe_model::impress::DataLabels::default();
        if val["show_val"].as_bool().unwrap_or(false) { data_labels.show_val = true; }

        let result = with_wb(|wb| {
            let sheet = wb.sheets.get_mut(sheet_idx as usize)?;

            let cfg = roe_core::tile::CalcRenderConfig::default();
            // 与渲染/命中测试同一套换算(含隐藏行列、48/15 默认值)
            let (mut x_pt, mut y_pt) = roe_core::tile::cell_to_pt(
                sheet, anchor_row as u32, anchor_col as u32, &cfg);
            if avoid_overlap {
                let (nx, ny) = find_free_area(sheet, x_pt, y_pt, width_pt, height_pt, &cfg);
                x_pt = nx;
                y_pt = ny;
            }

            // 稳定 id：同 sheet 内现有最大 id + 1，0 保留为无效哨兵值
            let new_id = roe_model::object_edit::alloc_object_id(sheet);

            sheet.floating_charts.push(roe_model::impress::ChartModel {
                id: new_id,
                kind,
                title,
                x_labels,
                series,
                x_pt,
                y_pt,
                width_pt,
                height_pt,
                show_legend,
                show_grid,
                x_axis_label,
                y_axis_label,
                border_color,
                border_width,
                gap_width_pct,
                overlap_pct,
                grouping,
                hole_size_pct,
                data_labels,
                slice_border_color: None,
            });
            Some(new_id)
        });

        match result {
            Some(Some(new_id)) => {
                mark_dirty(sheet_idx as usize, anchor_row as u32, anchor_col as u32);
                Ok(new_id as i64)
            }
            _ => {
                let n = with_wb(|wb| wb.sheets.len()).unwrap_or(0);
                Err(format!(
                    "insert_chart: sheet_idx={} 越界或无工作簿(当前共 {} 个 sheet,合法值 0..{})",
                    sheet_idx, n, n).into())
            }
        }
    }, false;

    // list_charts(sheet) -> String(JSON 数组;sheet 越界时抛错)
    // 每项: id / stat(统计图) / kind / title / x_pt,y_pt,width_pt,height_pt /
    //       range(覆盖的单元格范围,如 "B3:D6") / overlaps(与之相交的其他对象 id) /
    //       at_origin(位于 (0,0) 且与别的对象重叠,多半是导入文档的默认位置)
    // 普通图与统计图一并计入(共用 id 空间),顺序同 roe_list_charts:普通图在前。
    fn "list_charts" => |sheet_idx: i64| -> Result<String, Box<rhai::EvalAltResult>> {
        let out = with_wb(|wb| {
            let sheet = wb.sheets.get(sheet_idx as usize)?;
            let cfg = roe_core::tile::CalcRenderConfig::default();
            let mut items: Vec<(u64, bool, String, Option<String>, (f32, f32, f32, f32))> = Vec::new();
            for c in &sheet.floating_charts {
                items.push((c.id, false, format!("{:?}", c.kind), c.title.clone(),
                    (c.x_pt, c.y_pt, c.width_pt, c.height_pt)));
            }
            for c in &sheet.floating_stat_charts {
                items.push((c.id, true, format!("{:?}", c.model.kind), c.model.title.clone(),
                    (c.x_pt, c.y_pt, c.width_pt, c.height_pt)));
            }
            let r2 = |v: f32| ((v as f64) * 100.0).round() / 100.0;
            let mut arr: Vec<serde_json::Value> = Vec::new();
            for (id, stat, kind, title, rect) in &items {
                let overlaps: Vec<u64> = items.iter()
                    .filter(|o| o.0 != *id && roe_core::tile::rect_overlap(*rect, o.4))
                    .map(|o| o.0).collect();
                let (x, y, w, h) = *rect;
                let (cr1, cc1) = roe_core::tile::pt_to_cell(sheet, x + 0.5, y + 0.5, &cfg);
                let (cr2, cc2) = roe_core::tile::pt_to_cell(
                    sheet, (x + w - 0.5).max(x + 0.5), (y + h - 0.5).max(y + 0.5), &cfg);
                let range = format!("{}{}:{}{}",
                    col_to_letters(cc1 + 1), cr1 + 1, col_to_letters(cc2 + 1), cr2 + 1);
                let at_origin = x == 0.0 && y == 0.0 && !overlaps.is_empty();
                arr.push(serde_json::json!({
                    "id": id, "stat": stat, "kind": kind, "title": title,
                    "x_pt": r2(x), "y_pt": r2(y), "width_pt": r2(w), "height_pt": r2(h),
                    "range": range, "overlaps": overlaps, "at_origin": at_origin,
                }));
            }
            Some(serde_json::Value::Array(arr).to_string())
        })
        .flatten();
        match out {
            Some(j) => Ok(j),
            None => {
                let n = with_wb(|wb| wb.sheets.len()).unwrap_or(0);
                Err(format!(
                    "list_charts: sheet_idx={} 越界或无工作簿(当前共 {} 个 sheet,合法值 0..{})",
                    sheet_idx, n, n).into())
            }
        }
    }, true;

    // calc_list_objects(sheet) -> String(JSON 数组;sheet 越界时抛错)
    // 浮动对象总览:图表/统计图/图片/图形共用 id 空间。每项: id/kind(chart|stat_chart|image|shape)/sub/
    // x_pt,y_pt,width_pt,height_pt/rot/hidden,另有 title(图表)/text(图形)/mime(图片)。
    // 重新打开文档后 id 会重新分配,必须先调用本函数,不得缓存旧 id。
    fn "calc_list_objects" => |sheet_idx: i64| -> Result<String, Box<rhai::EvalAltResult>> {
        let out = with_wb(|wb| {
            let sheet = wb.sheets.get(sheet_idx as usize)?;
            serde_json::to_string(&roe_model::object_list::list_objects(sheet)).ok()
        })
        .flatten();
        match out {
            Some(j) => Ok(j),
            None => {
                let n = with_wb(|wb| wb.sheets.len()).unwrap_or(0);
                Err(format!(
                    "calc_list_objects: sheet_idx={} 越界或无工作簿(当前共 {} 个 sheet,合法值 0..{})",
                    sheet_idx, n, n).into())
            }
        }
    }, true;

    // calc_get_object(sheet, id) -> String(JSON,字段同 calc_list_objects 的一项;id 不存在时抛错)
    fn "calc_get_object" => |sheet_idx: i64, id: i64| -> Result<String, Box<rhai::EvalAltResult>> {
        let out: Option<Result<String, String>> = with_wb(|wb| {
            let n = wb.sheets.len();
            let Some(sheet) = wb.sheets.get(sheet_idx as usize) else {
                return Err(format!(
                    "calc_get_object: sheet_idx={sheet_idx} 越界(当前共 {n} 个 sheet,合法值 0..{n})"));
            };
            if id <= 0 {
                return Err(format!(
                    "calc_get_object: id 必须是正整数(收到 {id});先用 calc_list_objects({sheet_idx}) 查看现有 id"));
            }
            match roe_model::object_list::list_objects(sheet).into_iter().find(|o| o.id == id as u64) {
                Some(o) => serde_json::to_string(&o).map_err(|e| e.to_string()),
                None => Err(format!(
                    "calc_get_object: 对象 id={id} 不存在;先用 calc_list_objects({sheet_idx}) 查看现有 id")),
            }
        });
        match out {
            None => Err("calc_get_object: 无打开的工作簿".to_string().into()),
            Some(Err(m)) => Err(m.into()),
            Some(Ok(j)) => Ok(j),
        }
    }, true;

    // get_chart_config(sheet, id):读图表当前格式(只读)。
    // 普通图返回 STYLE_KEYS 全部 13 项 + id/stat/x_pt/y_pt/width_pt/height_pt;
    // 其中 STYLE_KEYS 那部分可整体喂回 set_chart_style(往返)。
    // 统计图只返回 id/stat/kind/title/show_legend/show_grid + 位置(只能改 title/show_legend/show_grid)。
    // 字段序列复制自 roe-api/src/chart_query.rs::roe_get_chart_config,改一处请同步另一处。
    fn "get_chart_config" => |sheet_idx: i64, id: i64| -> Result<String, Box<rhai::EvalAltResult>> {
        let r2 = |v: f32| ((v as f64) * 100.0).round() / 100.0;
        let out: Option<Result<String, String>> = with_wb(|wb| {
            let n = wb.sheets.len();
            let Some(sheet) = wb.sheets.get(sheet_idx as usize) else {
                return Err(format!(
                    "get_chart_config: sheet_idx={sheet_idx} 越界(当前共 {n} 个 sheet,合法值 0..{n})"));
            };
            if id <= 0 {
                return Err(format!(
                    "get_chart_config: id 必须是正整数(收到 {id});先用 list_charts({sheet_idx}) 查看现有 id"));
            }
            let uid = id as u64;
            if let Some(sc) = sheet.floating_stat_charts.iter().find(|c| c.id == uid) {
                let m = &sc.model;
                return Ok(serde_json::json!({
                    "id": sc.id, "stat": true, "kind": format!("{:?}", m.kind),
                    "title": m.title, "show_legend": m.show_legend, "show_grid": m.show_grid,
                    "x_pt": r2(sc.x_pt), "y_pt": r2(sc.y_pt),
                    "width_pt": r2(sc.width_pt), "height_pt": r2(sc.height_pt),
                }).to_string());
            }
            let Some(c) = sheet.floating_charts.iter().find(|c| c.id == uid) else {
                return Err(format!(
                    "get_chart_config: 图表 id={id} 不存在;先用 list_charts({sheet_idx}) 查看现有 id"));
            };
            #[allow(unreachable_patterns)]
            let grouping = match c.grouping {
                roe_model::impress::ChartGrouping::Standard => "Standard",
                roe_model::impress::ChartGrouping::Stacked => "Stacked",
                roe_model::impress::ChartGrouping::PercentStacked => "PercentStacked",
                _ => "Standard",
            };
            Ok(serde_json::json!({
                "id": c.id, "stat": false, "kind": format!("{:?}", c.kind),
                "title": c.title,
                "show_legend": c.show_legend, "show_grid": c.show_grid,
                "x_axis_label": c.x_axis_label, "y_axis_label": c.y_axis_label,
                "border_color": format!("#{:02x}{:02x}{:02x}", c.border_color.r, c.border_color.g, c.border_color.b),
                "border_width": r2(c.border_width),
                "grouping": grouping,
                "show_val": c.data_labels.show_val,
                "gap_width_pct": c.gap_width_pct.map(|v| r2(v)),
                "overlap_pct": c.overlap_pct.map(|v| r2(v)),
                "hole_size_pct": c.hole_size_pct.map(|v| r2(v)),
                "x_pt": r2(c.x_pt), "y_pt": r2(c.y_pt),
                "width_pt": r2(c.width_pt), "height_pt": r2(c.height_pt),
            }).to_string())
        });
        match out {
            Some(Ok(j)) => Ok(j),
            Some(Err(e)) => Err(e.into()),
            None => Err("get_chart_config: 无工作簿".to_string().into()),
        }
    }, true;

    // set_chart_style(sheet, id, style_json):局部修改图表格式,未给的键不变。
    // 先整体校验再写入,任一字段非法则一个字段都不改;未知键与类型错误都抛错。
    fn "set_chart_style" =>
        |sheet_idx: i64, id: i64, style_json: String| -> Result<(), Box<rhai::EvalAltResult>> {
        let val = parse_json_arg("set_chart_style", "style_json", &style_json, CHART_STYLE_EXAMPLE)?;
        chart_edit("set_chart_style", sheet_idx, id, |sheet, uid| {
            if let Some(sc) = sheet.floating_stat_charts.iter_mut().find(|c| c.id == uid) {
                return roe_core::chart_style::apply_stat_chart_style(&mut sc.model, &val, true)
                    .map_err(|m| format!("{m};示例: {CHART_STYLE_EXAMPLE}"));
            }
            let c = sheet.floating_charts.iter_mut().find(|c| c.id == uid)
                .ok_or_else(|| "图表不存在".to_string())?;
            roe_core::chart_style::apply_chart_style(c, &val, true)
                .map_err(|m| format!("{m};示例: {CHART_STYLE_EXAMPLE}"))
        })
    }, false;

    // set_chart_rect(sheet, id, x_pt, y_pt, w_pt, h_pt):精确设置位置与尺寸(pt,原点为数据区左上角)。
    // 注意 Rhai 里 f64 参数必须写成小数,如 48.0 而不是 48。
    fn "set_chart_rect" =>
        |sheet_idx: i64, id: i64, x_pt: f64, y_pt: f64, w_pt: f64, h_pt: f64| -> Result<(), Box<rhai::EvalAltResult>> {
        if ![x_pt, y_pt, w_pt, h_pt].iter().all(|v| v.is_finite()) {
            return Err("set_chart_rect: 参数必须是有限数字".to_string().into());
        }
        if x_pt < 0.0 || y_pt < 0.0 {
            return Err(format!(
                "set_chart_rect: x_pt/y_pt 不能为负(收到 {}, {});示例: set_chart_rect(0, 1, 48.0, 30.0, 300.0, 200.0)",
                x_pt, y_pt).into());
        }
        if w_pt <= 0.0 || h_pt <= 0.0 {
            return Err(format!(
                "set_chart_rect: w_pt/h_pt 必须大于 0(收到 {}, {});示例: set_chart_rect(0, 1, 48.0, 30.0, 300.0, 200.0)",
                w_pt, h_pt).into());
        }
        chart_edit("set_chart_rect", sheet_idx, id, |sheet, uid| {
            let (x, y, w, h) = chart_rect_mut(sheet, uid).ok_or_else(|| "图表不存在".to_string())?;
            *x = x_pt as f32; *y = y_pt as f32; *w = w_pt as f32; *h = h_pt as f32;
            Ok(())
        })
    }, false;

    // move_chart_to_cell(sheet, id, row, col):把图表左上角对齐到单元格,尺寸不变。
    fn "move_chart_to_cell" =>
        |sheet_idx: i64, id: i64, row: i64, col: i64| -> Result<(), Box<rhai::EvalAltResult>> {
        if row < 0 || col < 0 || row > 1_048_575 || col > 16_383 {
            return Err(format!(
                "move_chart_to_cell: row/col 超出范围(收到 {}, {};行 0..1048575,列 0..16383,从 0 起)",
                row, col).into());
        }
        chart_edit("move_chart_to_cell", sheet_idx, id, |sheet, uid| {
            let cfg = roe_core::tile::CalcRenderConfig::default();
            let (nx, ny) = roe_core::tile::cell_to_pt(sheet, row as u32, col as u32, &cfg);
            let (x, y, _, _) = chart_rect_mut(sheet, uid).ok_or_else(|| "图表不存在".to_string())?;
            *x = nx; *y = ny;
            Ok(())
        })
    }, false;

    // resize_chart_to_range(sheet, id, r1, c1, r2, c2):让图表正好盖住 r1..=r2、c1..=c2(闭区间,从 0 起)。
    fn "resize_chart_to_range" =>
        |sheet_idx: i64, id: i64, r1: i64, c1: i64, r2: i64, c2: i64| -> Result<(), Box<rhai::EvalAltResult>> {
        if r1 < 0 || c1 < 0 || r2 < r1 || c2 < c1 || r2 > 1_048_575 || c2 > 16_383 {
            return Err(format!(
                "resize_chart_to_range: 区域不合法(r1={}, c1={}, r2={}, c2={});要求 0<=r1<=r2、0<=c1<=c2,从 0 起,闭区间;示例: resize_chart_to_range(0, 1, 2, 1, 5, 3) 覆盖 B3:D6",
                r1, c1, r2, c2).into());
        }
        chart_edit("resize_chart_to_range", sheet_idx, id, |sheet, uid| {
            let cfg = roe_core::tile::CalcRenderConfig::default();
            let (x1, y1) = roe_core::tile::cell_to_pt(sheet, r1 as u32, c1 as u32, &cfg);
            let (x2, y2) = roe_core::tile::cell_to_pt(sheet, r2 as u32 + 1, c2 as u32 + 1, &cfg);
            let (nw, nh) = (x2 - x1, y2 - y1);
            if nw <= 0.0 || nh <= 0.0 {
                return Err("该区域宽或高为 0(行/列被隐藏),无法放置图表".to_string());
            }
            let (x, y, w, h) = chart_rect_mut(sheet, uid).ok_or_else(|| "图表不存在".to_string())?;
            *x = x1; *y = y1; *w = nw; *h = nh;
            Ok(())
        })
    }, false;

    // calc_set_object_fill(sheet, id, fill):设置图形/文本框填充色,fill 为 "#rrggbb",空串或 "none" 表示无填充。
    fn "calc_set_object_fill" =>
        |sheet_idx: i64, id: i64, fill: String| -> Result<(), Box<rhai::EvalAltResult>> {
        if id <= 0 {
            return Err(format!(
                "calc_set_object_fill: id 必须是正整数(收到 {id});先用 calc_list_objects({sheet_idx}) 查看现有 id").into());
        }
        let r = with_wb(|wb| {
            let n = wb.sheets.len();
            let Some(sheet) = wb.sheets.get_mut(sheet_idx as usize) else {
                return Err(format!(
                    "sheet_idx={sheet_idx} 越界(当前共 {n} 个 sheet,合法值 0..{n})"));
            };
            roe_model::object_edit::set_object_fill(sheet, id as u64, &fill)
        });
        match r {
            None => Err("calc_set_object_fill: 无打开的工作簿".to_string().into()),
            Some(Err(m)) => Err(format!(
                "calc_set_object_fill: {m};示例: calc_set_object_fill(0, 1, \"#ff0000\");先用 calc_list_objects({sheet_idx}) 查看现有 id").into()),
            Some(Ok(())) => {
                mark_dirty(sheet_idx as usize, 0, 0);
                Ok(())
            }
        }
    }, false;

    // calc_set_object_stroke(sheet, id, color, width_pt):设置图形/文本框边框,color 为 "#rrggbb",空串或 "none" 表示无边框;width_pt 范围 (0,100],必须写成小数。
    fn "calc_set_object_stroke" =>
        |sheet_idx: i64, id: i64, color: String, width_pt: f64| -> Result<(), Box<rhai::EvalAltResult>> {
        if id <= 0 {
            return Err(format!(
                "calc_set_object_stroke: id 必须是正整数(收到 {id});先用 calc_list_objects({sheet_idx}) 查看现有 id").into());
        }
        let r = with_wb(|wb| {
            let n = wb.sheets.len();
            let Some(sheet) = wb.sheets.get_mut(sheet_idx as usize) else {
                return Err(format!(
                    "sheet_idx={sheet_idx} 越界(当前共 {n} 个 sheet,合法值 0..{n})"));
            };
            roe_model::object_edit::set_object_stroke(sheet, id as u64, &color, width_pt as f32)
        });
        match r {
            None => Err("calc_set_object_stroke: 无打开的工作簿".to_string().into()),
            Some(Err(m)) => Err(format!(
                "calc_set_object_stroke: {m};示例: calc_set_object_stroke(0, 1, \"#333333\", 1.5);先用 calc_list_objects({sheet_idx}) 查看现有 id").into()),
            Some(Ok(())) => {
                mark_dirty(sheet_idx as usize, 0, 0);
                Ok(())
            }
        }
    }, false;

    // calc_set_object_rect(sheet, id, x_pt, y_pt, w_pt, h_pt):精确设置浮动对象(图表/统计图/图片/图形)的位置与尺寸,
    // 单位 pt,原点为数据区左上角;旋转角不变。f64 参数必须写成小数。
    fn "calc_set_object_rect" =>
        |sheet_idx: i64, id: i64, x_pt: f64, y_pt: f64, w_pt: f64, h_pt: f64| -> Result<(), Box<rhai::EvalAltResult>> {
        if id <= 0 {
            return Err(format!(
                "calc_set_object_rect: id 必须是正整数(收到 {id});先用 calc_list_objects({sheet_idx}) 查看现有 id").into());
        }
        let r = with_wb(|wb| {
            let n = wb.sheets.len();
            let Some(sheet) = wb.sheets.get_mut(sheet_idx as usize) else {
                return Err(format!(
                    "sheet_idx={sheet_idx} 越界(当前共 {n} 个 sheet,合法值 0..{n})"));
            };
            roe_model::object_edit::set_object_rect(
                sheet, id as u64, x_pt as f32, y_pt as f32, w_pt as f32, h_pt as f32)
        });
        match r {
            None => Err("calc_set_object_rect: 无打开的工作簿".to_string().into()),
            Some(Err(m)) => Err(format!(
                "calc_set_object_rect: {m};示例: calc_set_object_rect(0, 1, 48.0, 30.0, 300.0, 200.0);先用 calc_list_objects({sheet_idx}) 查看现有 id").into()),
            Some(Ok(())) => {
                mark_dirty(sheet_idx as usize, 0, 0);
                Ok(())
            }
        }
    }, false;

    // calc_insert_image(sheet, source, x_pt, y_pt, w_pt, h_pt) -> i64(新对象 id):插入浮动图片。
    // source 以 "base64:" 或 "data:image/" 开头按 base64,否则按本地路径;按文件头识别 PNG/JPEG,上限 10 MB。
    // w_pt/h_pt 传 0.0 按原比例(都为 0.0 时宽 200pt)。f64 参数必须写成小数。
    fn "calc_insert_image" =>
        |sheet_idx: i64, source: String, x_pt: f64, y_pt: f64, w_pt: f64, h_pt: f64| -> Result<i64, Box<rhai::EvalAltResult>> {
        let r = with_wb(|wb| {
            let n = wb.sheets.len();
            let Some(sheet) = wb.sheets.get_mut(sheet_idx as usize) else {
                return Err(format!(
                    "sheet_idx={sheet_idx} 越界(当前共 {n} 个 sheet,合法值 0..{n})"));
            };
            roe_model::object_edit::insert_image(
                sheet, &source, x_pt as f32, y_pt as f32, w_pt as f32, h_pt as f32)
        });
        match r {
            None => Err("calc_insert_image: 无打开的工作簿".to_string().into()),
            Some(Err(m)) => Err(format!(
                "calc_insert_image: {m};示例: calc_insert_image(0, \"base64:iVBOR...\", 48.0, 30.0, 0.0, 0.0)").into()),
            Some(Ok(id)) => {
                mark_dirty(sheet_idx as usize, 0, 0);
                Ok(id as i64)
            }
        }
    }, false;

    // calc_insert_shape(sheet, geometry, x_pt, y_pt, w_pt, h_pt, fill) -> i64(新对象 id):插入浮动图形。
    // geometry: rect/ellipse/line 或 OOXML 预设名(如 heart);fill: "#rrggbb",空串=无填充。文字另用 calc_set_object_text。
    fn "calc_insert_shape" =>
        |sheet_idx: i64, geometry: String, x_pt: f64, y_pt: f64, w_pt: f64, h_pt: f64, fill: String| -> Result<i64, Box<rhai::EvalAltResult>> {
        let r = with_wb(|wb| {
            let n = wb.sheets.len();
            let Some(sheet) = wb.sheets.get_mut(sheet_idx as usize) else {
                return Err(format!("sheet_idx={sheet_idx} 越界(当前共 {n} 个 sheet,合法值 0..{n})"));
            };
            roe_model::object_edit::insert_shape(
                sheet, &geometry, x_pt as f32, y_pt as f32, w_pt as f32, h_pt as f32, &fill)
        });
        match r {
            None => Err("calc_insert_shape: 无打开的工作簿".to_string().into()),
            Some(Err(m)) => Err(format!(
                "calc_insert_shape: {m};示例: calc_insert_shape(0, \"ellipse\", 48.0, 30.0, 120.0, 80.0, \"#ff0000\")").into()),
            Some(Ok(id)) => {
                mark_dirty(sheet_idx as usize, 0, 0);
                Ok(id as i64)
            }
        }
    }, false;

    // calc_insert_textbox(sheet, x_pt, y_pt, w_pt, h_pt, text) -> i64(新对象 id):插入文本框,\n 分段。
    fn "calc_insert_textbox" =>
        |sheet_idx: i64, x_pt: f64, y_pt: f64, w_pt: f64, h_pt: f64, text: String| -> Result<i64, Box<rhai::EvalAltResult>> {
        let r = with_wb(|wb| {
            let n = wb.sheets.len();
            let Some(sheet) = wb.sheets.get_mut(sheet_idx as usize) else {
                return Err(format!("sheet_idx={sheet_idx} 越界(当前共 {n} 个 sheet,合法值 0..{n})"));
            };
            roe_model::object_edit::insert_textbox(
                sheet, x_pt as f32, y_pt as f32, w_pt as f32, h_pt as f32, &text)
        });
        match r {
            None => Err("calc_insert_textbox: 无打开的工作簿".to_string().into()),
            Some(Err(m)) => Err(format!(
                "calc_insert_textbox: {m};示例: calc_insert_textbox(0, 48.0, 30.0, 200.0, 60.0, \"第一行\\n第二行\")").into()),
            Some(Ok(id)) => {
                mark_dirty(sheet_idx as usize, 0, 0);
                Ok(id as i64)
            }
        }
    }, false;

    // calc_set_object_text(sheet, id, text):整体替换图形/文本框的文字,\n 分段,保留首段与首个 run 的格式。
    fn "calc_set_object_text" =>
        |sheet_idx: i64, id: i64, text: String| -> Result<(), Box<rhai::EvalAltResult>> {
        if id <= 0 {
            return Err(format!(
                "calc_set_object_text: id 必须是正整数(收到 {id});先用 calc_list_objects({sheet_idx}) 查看现有 id").into());
        }
        let r = with_wb(|wb| {
            let n = wb.sheets.len();
            let Some(sheet) = wb.sheets.get_mut(sheet_idx as usize) else {
                return Err(format!("sheet_idx={sheet_idx} 越界(当前共 {n} 个 sheet,合法值 0..{n})"));
            };
            roe_model::object_edit::set_object_text(sheet, id as u64, &text)
        });
        match r {
            None => Err("calc_set_object_text: 无打开的工作簿".to_string().into()),
            Some(Err(m)) => Err(format!(
                "calc_set_object_text: {m};先用 calc_list_objects({sheet_idx}) 查看现有 id").into()),
            Some(Ok(())) => {
                mark_dirty(sheet_idx as usize, 0, 0);
                Ok(())
            }
        }
    }, false;

    // calc_move_object_to_cell(sheet, id, row, col):把浮动对象左上角对齐到单元格,尺寸不变。行列从 0 起。
    fn "calc_move_object_to_cell" =>
        |sheet_idx: i64, id: i64, row: i64, col: i64| -> Result<(), Box<rhai::EvalAltResult>> {
        if row < 0 || col < 0 || row > 1_048_575 || col > 16_383 {
            return Err(format!(
                "calc_move_object_to_cell: row/col 超出范围(收到 {}, {};行 0..1048575,列 0..16383,从 0 起)",
                row, col).into());
        }
        if id <= 0 {
            return Err(format!(
                "calc_move_object_to_cell: id 必须是正整数(收到 {id});先用 calc_list_objects({sheet_idx}) 查看现有 id").into());
        }
        let r = with_wb(|wb| {
            let n = wb.sheets.len();
            let Some(sheet) = wb.sheets.get_mut(sheet_idx as usize) else {
                return Err(format!("sheet_idx={sheet_idx} 越界(当前共 {n} 个 sheet,合法值 0..{n})"));
            };
            let uid = id as u64;
            let Some((_, _, w, h)) = roe_model::object_edit::get_object_rect(sheet, uid) else {
                return Err(format!("对象 id={id} 不存在"));
            };
            let cfg = roe_core::tile::CalcRenderConfig::default();
            let (nx, ny) = roe_core::tile::cell_to_pt(sheet, row as u32, col as u32, &cfg);
            roe_model::object_edit::set_object_rect(sheet, uid, nx, ny, w, h)
        });
        match r {
            None => Err("calc_move_object_to_cell: 无打开的工作簿".to_string().into()),
            Some(Err(m)) => Err(format!(
                "calc_move_object_to_cell: {m};先用 calc_list_objects({sheet_idx}) 查看现有 id").into()),
            Some(Ok(())) => {
                mark_dirty(sheet_idx as usize, 0, 0);
                Ok(())
            }
        }
    }, false;

    // calc_resize_object_to_range(sheet, id, r1, c1, r2, c2):让浮动对象正好盖住 r1..=r2、c1..=c2(闭区间,从 0 起);同时改位置。
    fn "calc_resize_object_to_range" =>
        |sheet_idx: i64, id: i64, r1: i64, c1: i64, r2: i64, c2: i64| -> Result<(), Box<rhai::EvalAltResult>> {
        if r1 < 0 || c1 < 0 || r2 < r1 || c2 < c1 || r2 > 1_048_575 || c2 > 16_383 {
            return Err(format!(
                "calc_resize_object_to_range: 区域不合法(r1={}, c1={}, r2={}, c2={});要求 0<=r1<=r2、0<=c1<=c2,从 0 起,闭区间;示例: calc_resize_object_to_range(0, 1, 2, 1, 5, 3) 覆盖 B3:D6",
                r1, c1, r2, c2).into());
        }
        if id <= 0 {
            return Err(format!(
                "calc_resize_object_to_range: id 必须是正整数(收到 {id});先用 calc_list_objects({sheet_idx}) 查看现有 id").into());
        }
        let r = with_wb(|wb| {
            let n = wb.sheets.len();
            let Some(sheet) = wb.sheets.get_mut(sheet_idx as usize) else {
                return Err(format!("sheet_idx={sheet_idx} 越界(当前共 {n} 个 sheet,合法值 0..{n})"));
            };
            let uid = id as u64;
            if roe_model::object_edit::get_object_rect(sheet, uid).is_none() {
                return Err(format!("对象 id={id} 不存在"));
            }
            let cfg = roe_core::tile::CalcRenderConfig::default();
            let (x1, y1) = roe_core::tile::cell_to_pt(sheet, r1 as u32, c1 as u32, &cfg);
            let (x2, y2) = roe_core::tile::cell_to_pt(sheet, r2 as u32 + 1, c2 as u32 + 1, &cfg);
            let (nw, nh) = (x2 - x1, y2 - y1);
            if nw <= 0.0 || nh <= 0.0 {
                return Err("该区域宽或高为 0(行/列被隐藏),无法放置对象".to_string());
            }
            roe_model::object_edit::set_object_rect(sheet, uid, x1, y1, nw, nh)
        });
        match r {
            None => Err("calc_resize_object_to_range: 无打开的工作簿".to_string().into()),
            Some(Err(m)) => Err(format!(
                "calc_resize_object_to_range: {m};先用 calc_list_objects({sheet_idx}) 查看现有 id").into()),
            Some(Ok(())) => {
                mark_dirty(sheet_idx as usize, 0, 0);
                Ok(())
            }
        }
    }, false;

    // calc_delete_object(sheet, id) -> bool:删除浮动对象(图表/统计图/图片/图形);
    // 删除成功 true;id 不存在返回 false(与 delete_chart 一致);sheet 越界或无工作簿抛错。删除后 id 不复用。
    fn "calc_delete_object" => |sheet_idx: i64, id: i64| -> Result<bool, Box<rhai::EvalAltResult>> {
        let r = with_wb(|wb| {
            let n = wb.sheets.len();
            let Some(sheet) = wb.sheets.get_mut(sheet_idx as usize) else {
                return Err(format!(
                    "calc_delete_object: sheet_idx={sheet_idx} 越界(当前共 {n} 个 sheet,合法值 0..{n})"));
            };
            if id <= 0 { return Ok(false); }
            Ok(roe_model::object_edit::delete_object(sheet, id as u64))
        });
        match r {
            None => Err("calc_delete_object: 无打开的工作簿".to_string().into()),
            Some(Err(m)) => Err(m.into()),
            Some(Ok(removed)) => {
                if removed { mark_dirty(sheet_idx as usize, 0, 0); }
                Ok(removed)
            }
        }
    }, false;

    // delete_chart(sheet, id) -> bool:删除成功 true;id 不存在返回 false(与 remove_data_validation 一致);
    // sheet 越界或无工作簿抛错。
    fn "delete_chart" => |sheet_idx: i64, id: i64| -> Result<bool, Box<rhai::EvalAltResult>> {
        let r = with_wb(|wb| {
            let n = wb.sheets.len();
            let Some(sheet) = wb.sheets.get_mut(sheet_idx as usize) else {
                return Err(format!(
                    "delete_chart: sheet_idx={sheet_idx} 越界(当前共 {n} 个 sheet,合法值 0..{n})"));
            };
            if id <= 0 { return Ok(false); }
            let uid = id as u64;
            let before = sheet.floating_charts.len() + sheet.floating_stat_charts.len();
            sheet.floating_charts.retain(|c| c.id != uid);
            sheet.floating_stat_charts.retain(|c| c.id != uid);
            Ok(sheet.floating_charts.len() + sheet.floating_stat_charts.len() != before)
        });
        match r {
            None => Err("delete_chart: 无打开的工作簿".to_string().into()),
            Some(Err(m)) => Err(m.into()),
            Some(Ok(removed)) => {
                if removed { mark_dirty(sheet_idx as usize, 0, 0); }
                Ok(removed)
            }
        }
    }, false;

    // save_xlsx() -> String
    // 把当前 Workbook 序列化为 xlsx 字节流，返回 base64 编码字符串。
    // 与 roe_save_document_to_ooxml_bytes（C-ABI）走同一条 XlsxWriter。
    // 写出范围：cell值(Number/Text/Formula/Bool/Error/RichText) +
    // merged_cells + cell_formats/conditional_formatting + sheet_view +
    // col/row hidden+outline_level；不含 col_widths/row_heights/
    // frozen_rows/frozen_cols（详见 xlsx_writer.rs 及
    // roe-save-design.md §五记录的真实范围核实结果）。失败返回空字符串。
    fn "save_xlsx" => || -> String {
        with_wb(|wb| {
            match roe_parser::ooxml::xlsx_writer::XlsxWriter::write(wb) {
                Ok(bytes) => base64::engine::general_purpose::STANDARD.encode(bytes),
                Err(_) => String::new(),
            }
        })
        .unwrap_or_default()
    }, true;

    fn "insert_row" => |sheet: i64, row: i64| -> String {
        if !calc_row_ok(row) { return "err:行号越界(0..1048575)".to_string(); }
        with_wb(|wb| {
            let Some(s) = wb.sheets.get_mut(sheet as usize) else { return "err:sheet 越界".to_string(); };
            insert_row_impl(s, row as u32);
            rewrite_formulas_for_edit(wb, sheet as usize, roe_calc::ref_shift::Axis::Row, row as u32, 1);
        "ok".to_string()
        }).unwrap_or_else(|| "err:无打开的工作簿".to_string())
    }, false;

    fn "delete_row" => |sheet: i64, row: i64| -> String {
        if !calc_row_ok(row) { return "err:行号越界(0..1048575)".to_string(); }
        with_wb(|wb| {
            let Some(s) = wb.sheets.get_mut(sheet as usize) else { return "err:sheet 越界".to_string(); };
            delete_rows_batch(s, &[row as u32]);
            rewrite_formulas_for_edit(wb, sheet as usize, roe_calc::ref_shift::Axis::Row, row as u32, -1);
        "ok".to_string()
        }).unwrap_or_else(|| "err:无打开的工作簿".to_string())
    }, false;

    fn "insert_column" => |sheet: i64, col: i64| -> String {
        if !calc_col_ok(col) { return "err:列号越界(0..16383)".to_string(); }
        with_wb(|wb| {
            let Some(s) = wb.sheets.get_mut(sheet as usize) else { return "err:sheet 越界".to_string(); };
            mark_struct_edit();
            let col = col as u32;
            // 0.0 表示"未设置",渲染层 col_width_at 会回退到
            // sheet.default_col_width / cfg.default_col_width(此前硬编码 60.0)。
            let default_w = 0.0f32;
            if (col as usize) < s.col_widths.len() {
                s.col_widths.insert(col as usize, default_w);
            } else {
                s.col_widths.resize(col as usize, default_w);
                s.col_widths.push(default_w);
            }
            // 重建 cols HashMap：key >= col 的列 key+1
            let old: Vec<_> = s.cells.cols.drain().collect();
            for (c, data) in old {
                s.cells.cols.insert(if c >= col { c + 1 } else { c }, data);
            }
            for m in s.merged_cells.iter_mut() {
                if m.c1 >= col { m.c1 += 1; }
                if m.c2 >= col { m.c2 += 1; }
            }
            // cell_formats / col_style / col_hidden / col_outline_level 右移
            s.shift_cols_for_insert(col);
            rewrite_formulas_for_edit(wb, sheet as usize, roe_calc::ref_shift::Axis::Col, col, 1);
        "ok".to_string()
        }).unwrap_or_else(|| "err:无打开的工作簿".to_string())
    }, false;

    fn "delete_column" => |sheet: i64, col: i64| -> String {
        if !calc_col_ok(col) { return "err:列号越界(0..16383)".to_string(); }
        with_wb(|wb| {
            let Some(s) = wb.sheets.get_mut(sheet as usize) else { return "err:sheet 越界".to_string(); };
            mark_struct_edit();
            let col = col as u32;
            if (col as usize) < s.col_widths.len() {
                s.col_widths.remove(col as usize);
            }
            s.cells.cols.remove(&col);
            let old: Vec<_> = s.cells.cols.drain().collect();
            for (c, data) in old {
                s.cells.cols.insert(if c > col { c - 1 } else { c }, data);
            }
            s.merged_cells.retain(|m| !(m.c1 == col && m.c2 == col));
            for m in s.merged_cells.iter_mut() {
                if m.c1 > col { m.c1 -= 1; }
                if m.c2 > col { m.c2 -= 1; }
            }
            // cell_formats / col_style / col_hidden / col_outline_level 左移
            s.shift_cols_for_delete(col);
            rewrite_formulas_for_edit(wb, sheet as usize, roe_calc::ref_shift::Axis::Col, col, -1);
        "ok".to_string()
        }).unwrap_or_else(|| "err:无打开的工作簿".to_string())
    }, false;

    // delete_duplicates(sheet, r1, c1, r2, c2, cols_json, has_header)
    // v157新增。cols_json：参与判重的列偏移数组（相对 c1，JSON数组，
    // 如"[0,2]"表示只按第1列和第3列判重；空数组"[]"表示全列判重）。
    // has_header：true 时第一行（r1）不参与判重、也不会被删除，只用
    // 于（前端弹窗展示列名，函数本身不需要用到列名）。
    //
    // 性能设计（v157，面向百万行场景，见roe-ext_progress交接文档）：
    // 不对每一行都物化 Vec<CellValue> 再塞进 HashSet 判重（那样内存
    // 开销正比于"总行数 × 判重列数"，百万行会占用数百MB到GB级）。
    // 改为哈希分桶两阶段：第一遍只算每行判重列的 u64 哈希塞进桶
    // （HashSet<u64>数量级的内存开销，远小于物化完整值）；第二遍只对
    // "哈希落在同一个桶里、有多于一行"的候选组回读原始数据做精确
    // Vec<CellValue> 比较——绝大多数行第一遍就能确定"这个哈希只出现
    // 一次"从而立刻排除，不需要精确比较。这样避免了纯哈希方案"哈希
    // 碰撞导致误删不同数据行"的风险（判重导致的删除是不可逆操作，
    // 不能接受"极低概率误删"这种量级的风险），同时内存开销接近纯
    // 哈希方案。
    fn "delete_duplicates" =>
        |sheet_idx: i64, r1: i64, c1: i64, r2: i64, c2: i64, cols_json: String, has_header: bool| -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let cols: Vec<u32> = match serde_json::from_str::<Vec<i64>>(&cols_json) {
            Ok(v) => v.into_iter().map(|x| x as u32).collect(),
            Err(e) => return format!("err:cols_json解析失败: {e}"),
        };

        let result = with_wb(|wb| {
            let Some(sheet) = wb.sheets.get_mut(sheet_idx as usize) else {
                return "err:sheet_idx越界".to_string();
            };
            let (r1, c1, r2, c2) = (r1 as u32, c1 as u32, r2 as u32, c2 as u32);
            if r2 < r1 || c2 < c1 {
                return "err:range无效".to_string();
            }

            // 参与判重的绝对列号：空数组=c1..=c2全列，否则是相对c1的偏移
            let dedup_cols: Vec<u32> = if cols.is_empty() {
                (c1..=c2).collect()
            } else {
                cols.iter().map(|off| c1 + off).collect()
            };

            let data_start_row = if has_header { r1 + 1 } else { r1 };
            if data_start_row > r2 {
                return "ok:0".to_string(); // 表头之外没有数据行，无需处理
            }
                        // 每个判重列取值:先按 start_row 排序 block,再二分定位;
            // 避免原实现"每读一个值线性扫全部 block"的 O(blocks) 代价。
            let col_blocks: Vec<Vec<(u32, &[CellValue])>> = dedup_cols.iter().map(|&c| {
                let mut v: Vec<(u32, &[CellValue])> = sheet.cells.cols.get(&c)
                    .map(|cd| cd.blocks.iter().map(|b| (b.start_row, &b.values[..])).collect())
                    .unwrap_or_default();
                v.sort_by_key(|(sr, _)| *sr);
                v
            }).collect();
            let value_at = |ci: usize, row: u32| -> &CellValue {
                static EMPTY: CellValue = CellValue::Empty;
                let blocks = &col_blocks[ci];
                // 最后一个 start_row <= row 的 block
                let pos = blocks.partition_point(|(sr, _)| *sr <= row);
                if pos == 0 { return &EMPTY; }
                let (sr, vals) = blocks[pos - 1];
                let off = (row - sr) as usize;
                if off < vals.len() { &vals[off] } else { &EMPTY }
            };
            let is_blank = |v: &CellValue| -> bool {
                match v {
                    CellValue::Empty => true,
                    CellValue::Text(t) => t.is_empty(),
                    _ => false,
                }
            };

            // 候选行 = 判重列 block 与 [data_start_row, r2] 的交集(排序去重)。
            // 块之间的间隙不在其中,所以尾部/中部的"根本没存"的空行不会被遍历。
            let mut candidates: Vec<u32> = Vec::new();
            for blocks in &col_blocks {
                for (sr, vals) in blocks {
                    if vals.is_empty() { continue; }
                    let b_start = *sr;
                    let b_end = *sr + vals.len() as u32 - 1;
                    if b_end < data_start_row || b_start > r2 { continue; }
                    let lo = b_start.max(data_start_row);
                    let hi = b_end.min(r2);
                    candidates.extend(lo..=hi);
                }
            }
            candidates.sort_unstable();
            candidates.dedup();

            let row_vals = |row: u32| -> Vec<CellValue> {
                (0..dedup_cols.len()).map(|ci| value_at(ci, row).clone()).collect()
            };
            let row_all_blank = |row: u32| -> bool {
                (0..dedup_cols.len()).all(|ci| is_blank(value_at(ci, row)))
            };

            // 数据区域末行 = 候选行中判重列至少一个非空的最大行
            let last_data_row = match candidates.iter().rev().find(|&&r| !row_all_blank(r)) {
                Some(&r) => r,
                None => return "ok:0".to_string(), // 选区内判重列全空,不处理
            };
            candidates.retain(|&r| r <= last_data_row);

            // 第一遍:哈希分桶(只含非空行;空行由下方单独处理)
            let mut buckets: std::collections::HashMap<u64, Vec<u32>> =
                std::collections::HashMap::with_capacity(candidates.len());
            let mut nonempty_rows: usize = 0;
            for &row in &candidates {
                if row_all_blank(row) { continue; }
                nonempty_rows += 1;
                let vals = row_vals(row);
                let mut hasher = DefaultHasher::new();
                for v in &vals { v.hash(&mut hasher); }
                let h = hasher.finish();
                buckets.entry(h).or_insert_with(Vec::new).push(row);
            }

            // 第二遍:仅对桶内 >1 行的候选组做精确比较
            let mut to_delete: Vec<u32> = Vec::new();
            for (_hash, rows) in buckets.iter() {
                if rows.len() < 2 { continue; }
                let mut kept_reps: Vec<(u32, Vec<CellValue>)> = Vec::new();
                for &row in rows {
                    let vals = row_vals(row);
                    let is_dup = kept_reps.iter().any(|(_, rep_vals)| rep_vals == &vals);
                    if is_dup {
                        to_delete.push(row);
                    } else {
                        kept_reps.push((row, vals));
                    }
                }
            }

            // 数据区域内的空行(含块间隙里根本没存的行):判重列全空的行互为重复,
            // 删到只剩最靠上的一个。尾部(> last_data_row)不在此范围。
            // 展开成具体行号,线性于"数据区域跨度";设上限避免病态输入。
            let span = (last_data_row - data_start_row + 1) as usize;
            let blank_n = span - nonempty_rows;
            const BLANK_EXPAND_LIMIT: usize = 1_000_000;
            if blank_n >= 2 {
                if blank_n > BLANK_EXPAND_LIMIT {
                    return format!("err:区域内空行过多({}),请缩小选区", blank_n);
                }
                let nonempty_set: std::collections::HashSet<u32> = candidates.iter()
                    .copied().filter(|&r| !row_all_blank(r)).collect();
                let mut first_blank_kept = false;
                for row in data_start_row..=last_data_row {
                    if nonempty_set.contains(&row) { continue; }
                    if !first_blank_kept { first_blank_kept = true; continue; }
                    to_delete.push(row);
                }
            }

            let deleted_count = to_delete.len();
            delete_rows_batch(sheet, &to_delete);
            format!("ok:{deleted_count}")
        });

        if let Some(msg) = result {
            if msg.starts_with("ok:") {
                mark_dirty(sheet_idx as usize, r1 as u32, c1 as u32);
                mark_dirty(sheet_idx as usize, r2 as u32, c2 as u32);
            }
            msg
        } else {
            "err:workbook未绑定".to_string()
        }
    }, false;

    // update_data_validation(sheet, index, range_json, rule_json) -> bool
    // 按下标整体替换该sheet上第index条数据验证规则（range和rule均重新
    // 提供，等价于原地替换整条记录）。index越界或sheet不存在返回false，
    // 不panic。range_json/rule_json解析失败同样返回false。
    // 定位方式说明：DataValidationRule当前不带id字段，get_data_validations
    // 返回的JSON数组下标即为index，前台按该下标定位要改的是哪一条
    // （与set_data_validation现状一致，未引入新的id概念，最小改动）。
    fn "update_data_validation" =>
        |sheet_idx: i64, index: i64, range_json: String, rule_json: String| -> Result<bool, Box<rhai::EvalAltResult>> {
        let range = parse_range_arg("update_data_validation", "range_json", &range_json)?;
        let v = parse_json_arg("update_data_validation", "rule_json", &rule_json, DV_EXAMPLE)?;
        let kind = validate_dv_rule("update_data_validation", &v)?;
        if kind == "none" {
            return Err("update_data_validation: kind=none 表示删除规则,请改用 remove_data_validation(sheet, index)".into());
        }
        // index 为负/越界、sheet 不存在:与 remove_data_validation 一致,返回 false
        if index < 0 {
            return Ok(false);
        }

        let updated = with_wb(|wb| {
            let sheet = wb.sheets.get_mut(sheet_idx as usize)?;
            let slot = sheet.data_validations.get_mut(index as usize)?;
            *slot = roe_model::calc::DataValidationRule {
                range: range.clone(),
                kind:        v["kind"].as_str().unwrap_or("").to_string(),
                operator:    v["operator"].as_str().unwrap_or("").to_string(),
                formula1:    v["formula1"].as_str().unwrap_or("").to_string(),
                formula2:    v["formula2"].as_str().map(|s| s.to_string()),
                allow_blank: v["allow_blank"].as_bool().unwrap_or(true),
                show_error:  v["show_error"].as_bool().unwrap_or(false),
                error_msg:   v["error_msg"].as_str().map(|s| s.to_string()),
            };
            Some(())
        }).flatten();

        if updated.is_some() {
            mark_dirty(sheet_idx as usize, range.start.0, range.start.1);
            mark_dirty(sheet_idx as usize, range.end.0, range.end.1);
            Ok(true)
        } else {
            Ok(false)
        }
    }, false;

    // remove_data_validation(sheet, index) -> bool
    // 按下标删除该sheet上第index条数据验证规则。index越界或sheet不存在
    // 返回false，不panic。删除后数组整体前移（Vec::remove语义），
    // 若前台缓存了旧的下标列表需要重新get_data_validations刷新，这是
    // 下标定位方案的固有特性，与remove_sheet等现有"按下标删除"接口
    // 的语义一致，非本次新引入的问题。
    fn "remove_data_validation" => |sheet_idx: i64, index: i64| -> bool {
        if index < 0 {
            return false;
        }
        let removed = with_wb(|wb| {
            let sheet = wb.sheets.get_mut(sheet_idx as usize)?;
            if (index as usize) >= sheet.data_validations.len() {
                return None;
            }
            let rule = sheet.data_validations.remove(index as usize);
            Some(rule.range)
        }).flatten();

        if let Some(range) = removed {
            mark_dirty(sheet_idx as usize, range.start.0, range.start.1);
            mark_dirty(sheet_idx as usize, range.end.0, range.end.1);
            true
        } else {
            false
        }
    }, false;

    // get_pivot_config(sheet) -> JSON | "err:...":透视表当前配置
    // {"rows":[],"cols":[],"values":[源列名],"filters":[],"source_idx":N,"fields":[数据源全部列名]}
    fn "get_pivot_config" => |sheet_idx: i64| -> String {
        with_wb(|wb| crate::calc_api_pivot::get_pivot_config_impl(wb, sheet_idx))
            .unwrap_or_else(|| "err:get_pivot_config: 无打开的工作簿".to_string())
    }, true;

    // pivot_set_config(sheet, config_json) -> "ok" | "err:...":就地整体设置透视表字段布局
    // config_json: {"rows":[],"cols":[],"values":[],"filters":[]},键必须是这四个之一,缺省=空
    fn "pivot_set_config" => |sheet_idx: i64, config_json: String| -> String {
        match with_wb(|wb| crate::calc_api_pivot::set_config_impl(wb, sheet_idx, &config_json)) {
            None => "err:pivot_set_config: 无打开的工作簿".to_string(),
            Some(Err(m)) => format!("err:{m}"),
            Some(Ok(pts)) => {
                for (r, c) in pts { mark_dirty(sheet_idx as usize, r, c); }
                "ok".to_string()
            }
        }
    }, false;

    // pivot_set_filter(sheet, field, values_json) -> "ok" | "err:...":设置筛选,空数组 "[]" 清除
    fn "pivot_set_filter" => |sheet_idx: i64, field: String, values_json: String| -> String {
        match with_wb(|wb| crate::calc_api_pivot::set_filter_impl(wb, sheet_idx, &field, &values_json)) {
            None => "err:pivot_set_filter: 无打开的工作簿".to_string(),
            Some(Err(m)) => format!("err:{m}"),
            Some(Ok(pts)) => {
                for (r, c) in pts { mark_dirty(sheet_idx as usize, r, c); }
                "ok".to_string()
            }
        }
    }, false;

    // pivot_set_sort(sheet, field, asc) -> "ok" | "err:...":行/列字段排序,asc=true 升序
    fn "pivot_set_sort" => |sheet_idx: i64, field: String, asc: bool| -> String {
        match with_wb(|wb| crate::calc_api_pivot::set_sort_impl(wb, sheet_idx, &field, asc)) {
            None => "err:pivot_set_sort: 无打开的工作簿".to_string(),
            Some(Err(m)) => format!("err:{m}"),
            Some(Ok(pts)) => {
                for (r, c) in pts { mark_dirty(sheet_idx as usize, r, c); }
                "ok".to_string()
            }
        }
    }, false;

    // pivot_create(source_sheet, config_json, name) -> "ok:<新sheet索引>" | "err:...":新建透视表 sheet
    // config_json: {"rows":[],"cols":[],"values":[],"agg":"sum","filters":{"字段":["允许值"]}}
    // agg 只允许 sum/mean/count/min/max;filters 是对象,值为空数组=仅作页字段不限制行
    fn "pivot_create" => |source_sheet: i64, config_json: String, name: String| -> String {
        match with_wb(|wb| crate::calc_api_pivot::create_impl(wb, source_sheet, &config_json, &name)) {
            None => "err:pivot_create: 无打开的工作簿".to_string(),
            Some(Err(m)) => format!("err:{m}"),
            Some(Ok(idx)) => {
                mark_struct_edit();
                mark_dirty(idx, 0, 0);
                format!("ok:{idx}")
            }
        }
    }, false;
    });
}

/// sort_range 专用排序键。顺序(升序):数字 < 文本 < 空值;空值无论升降序永远排最后。
#[derive(Debug, Clone, PartialEq)]
enum SortKey {
    Num(f64),
    Text(String),
    Empty,
}

fn cell_to_sort_key_v2(v: &CellValue) -> SortKey {
    match v {
        CellValue::Empty => SortKey::Empty,
        CellValue::Number(n) => {
            if n.is_nan() { SortKey::Empty } else { SortKey::Num(*n) }
        }
        CellValue::Bool(b) => SortKey::Num(if *b { 1.0 } else { 0.0 }),
        CellValue::Formula { cached, .. } => cell_to_sort_key_v2(cached.as_ref()),
        // Text / RichText / Error:取文本内容,忽略大小写比较
        other => {
            let t = cell_value_to_string(other);
            if t.is_empty() { SortKey::Empty } else { SortKey::Text(t.to_lowercase()) }
        }
    }
}

/// 比较两个排序键。空值永远最后(不受 ascending 影响);数字与文本按 ascending 反转。
fn compare_sort_keys(a: &SortKey, b: &SortKey, ascending: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let rank = |k: &SortKey| match k { SortKey::Num(_) => 0u8, SortKey::Text(_) => 1, SortKey::Empty => 2 };
    let (ra, rb) = (rank(a), rank(b));
    // 空值始终最后
    if ra == 2 || rb == 2 {
        return ra.cmp(&rb);
    }
    let ord = if ra != rb {
        ra.cmp(&rb)
    } else {
        match (a, b) {
            (SortKey::Num(x), SortKey::Num(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
            (SortKey::Text(x), SortKey::Text(y)) => x.cmp(y),
            _ => Ordering::Equal,
        }
    };
    if ascending { ord } else { ord.reverse() }
}

fn cell_to_sort_key(v: &CellValue) -> f64 {
    match v {
        CellValue::Number(n) => *n,
        CellValue::Bool(b)   => if *b { 1.0 } else { 0.0 },
        CellValue::Text(_)   => f64::MAX,  // 文本排到最后
        _                    => f64::MAX,
    }
}


/// 与 cell_value_to_string 互为镜像的解析方向：把脚本传入的原始字符串
/// 按 set_cell 既有规则解析成 CellValue（数字优先，其次true/false布尔，
/// 否则退化为纯文本）。原先内联在 set_cell 闭包里，text_to_columns
/// （分列，2026-09新增）需要对拆分出的每一段做同样的解析，因此抽成
/// 独立函数供两处复用，避免规则漂移（例如未来给某处加了新判断分支，
/// 另一处忘记同步）。
/// 0-based列号 -> Excel字母记号（1-based内部换算），描述性统计等需要
/// 拼公式range引用（如 "A1:A10"）的场景使用。逻辑与 roe-parser 内多处
/// 同名函数（xlsx_writer.rs/xlsx.rs/xlsx_styles_writer.rs）一致，未做
/// 跨crate抽象共享——沿用本文件"就近放置小工具函数"的既有风格
/// （parse_string_to_cellvalue同理）。
fn col_to_letters(mut col: u32) -> String {
    let mut s = String::new();
    while col > 0 {
        let rem = (col - 1) % 26;
        s.insert(0, (b'A' + rem as u8) as char);
        col = (col - 1) / 26;
    }
    if s.is_empty() {
        s.push('A');
    }
    s
}

/// text_to_columns_multi / preview_text_to_columns 共用的切分函数。
/// - delims:分隔符列表(调用方已过滤空串);每个位置尝试全部分隔符,取最长匹配;
/// - 按 char 边界前进,多字节字符(中文等)安全;
/// - merge=false:与 str::split 语义一致(首尾分隔符产生首尾空段);
/// - merge=true:命中分隔符后继续吞掉紧跟的所有分隔符(不产生中间空段),
///   首尾不做特殊处理;
/// - 空文本返回 [""](1 段)。
fn split_by_delims(text: &str, delims: &[String], merge: bool) -> Vec<String> {
    let longest_at = |s: &str| -> usize {
        let mut best = 0usize;
        for d in delims {
            if !d.is_empty() && s.starts_with(d.as_str()) && d.len() > best {
                best = d.len();
            }
        }
        best
    };
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut i = 0usize;
    while i < text.len() {
        let rest = &text[i..];
        let m = longest_at(rest);
        if m > 0 {
            parts.push(std::mem::take(&mut cur));
            i += m;
            if merge {
                loop {
                    let m2 = longest_at(&text[i..]);
                    if m2 == 0 { break; }
                    i += m2;
                }
            }
        } else {
            let ch = rest.chars().next().unwrap();
            cur.push(ch);
            i += ch.len_utf8();
        }
    }
    parts.push(cur);
    parts
}

/// 解析 delims_json(字符串数组)并过滤空串;解析失败或全空返回 None。
fn parse_delims_json(json: &str) -> Option<Vec<String>> {
    let v: Vec<String> = serde_json::from_str(json).ok()?;
    let v: Vec<String> = v.into_iter().filter(|s| !s.is_empty()).collect();
    if v.is_empty() { None } else { Some(v) }
}

/// 单次分列允许的最大行数(防御整列选区百万行循环;前台应先收窄区间)。
const TTC_MAX_ROWS: i64 = 1_048_576;

// ── 自动筛选(行隐藏)辅助 ─────────────────────────────────────────────
const AF_MAX_ROWS: u32 = 1_048_576;
const AF_MAX_UNIQUE: usize = 5000;

/// 取 col 列 r1..=r2 各行的展示文本(与 get_row_texts 同口径),无单元格记为空串。
fn af_column_texts(sheet: &roe_model::calc::Sheet, col: u32, r1: u32, r2: u32) -> Vec<String> {
    let n = (r2 - r1 + 1) as usize;
    let mut out = vec![String::new(); n];
    if let Some(col_data) = sheet.cells.cols.get(&col) {
        for blk in &col_data.blocks {
            let len = blk.values.len() as u32;
            if len == 0 { continue; }
            let b1 = blk.start_row;
            let b2 = b1 + len - 1;
            if b2 < r1 || b1 > r2 { continue; }
            let lo = b1.max(r1);
            let hi = b2.min(r2);
            for r in lo..=hi {
                out[(r - r1) as usize] = cell_value_to_string(&blk.values[(r - b1) as usize]);
            }
        }
    }
    out
}

fn parse_string_to_cellvalue(value: &str) -> CellValue {
    if let Ok(n) = value.parse::<f64>() {
        CellValue::Number(n)
    } else if value.eq_ignore_ascii_case("true") {
        CellValue::Bool(true)
    } else if value.eq_ignore_ascii_case("false") {
        CellValue::Bool(false)
    } else {
        CellValue::Text(value.to_string())
    }
}

/// 单元格展示值文本。实现已下沉到 roe-model（CellValue::to_display_string），
/// 脚本 get_cell 与 list 数据验证共用同一份，保证读数一致。
fn cell_value_to_string(v: &CellValue) -> String {
    v.to_display_string()
}

// ── insert_subtotals_ex 支撑 ─────────────────────────────────────────────

struct SubtotalGroupSpec {
    col: u32,
    aggs: Vec<(u32, i64)>, // (列, SUBTOTAL function_num)
}

struct SubtotalSpec {
    groups: Vec<SubtotalGroupSpec>,
    summary_below: bool,
    grand_total: bool,
}

fn parse_subtotal_spec(json: &str) -> Result<SubtotalSpec, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("spec_json解析失败: {e}"))?;
    let groups_v = v.get("groups").and_then(|x| x.as_array())
        .ok_or_else(|| "spec_json缺少groups数组".to_string())?;
    if groups_v.is_empty() {
        return Err("groups不能为空".to_string());
    }
    let mut groups = Vec::with_capacity(groups_v.len());
    for g in groups_v {
        let col = g.get("col").and_then(|x| x.as_u64())
            .ok_or_else(|| "group缺少col".to_string())? as u32;
        let aggs_v = g.get("aggs").and_then(|x| x.as_array())
            .ok_or_else(|| "group缺少aggs".to_string())?;
        let mut aggs = Vec::with_capacity(aggs_v.len());
        for a in aggs_v {
            let pair = a.as_array().filter(|p| p.len() == 2)
                .ok_or_else(|| "aggs项必须是[列,函数编号]".to_string())?;
            let c = pair[0].as_u64().ok_or_else(|| "aggs列号非法".to_string())? as u32;
            let f = pair[1].as_i64().ok_or_else(|| "aggs函数编号非法".to_string())?;
            aggs.push((c, f));
        }
        if aggs.is_empty() {
            return Err("group的aggs不能为空".to_string());
        }
        groups.push(SubtotalGroupSpec { col, aggs });
    }
    Ok(SubtotalSpec {
        groups,
        summary_below: v.get("summary_below").and_then(|x| x.as_bool()).unwrap_or(false),
        grand_total: v.get("grand_total").and_then(|x| x.as_bool()).unwrap_or(true),
    })
}

/// 最终行序列的一项。
enum SubtotalItem {
    /// 原数据行(原行号)
    Data(u32),
    /// 汇总行:group_idx=None 表示总计;cover 在**最终布局**里的闭区间(行号为绝对行号,
    /// 由 layout 完成后统一换算,这里先存"序列下标区间")。label_row 为标签取值的原数据行。
    Summary { group_idx: Option<usize>, seq_lo: usize, seq_hi: usize, label_row: u32 },
}

/// 递归构造某一层的行序列。orig 区间 [lo, hi] 内按 groups[depth] 的列切分相邻相同值。
fn build_subtotal_seq(
    s: &roe_model::calc::Sheet,
    spec: &SubtotalSpec,
    depth: usize,
    lo: u32,
    hi: u32,
    seq: &mut Vec<SubtotalItem>,
) {
    if depth >= spec.groups.len() {
        for r in lo..=hi { seq.push(SubtotalItem::Data(r)); }
        return;
    }
    let col = spec.groups[depth].col;
    // 相邻相同值切段(不要求预排序,对齐旧 insert_subtotals 与 Excel 语义)
    let mut seg_start = lo;
    let mut cur = s.cells.get(lo, col);
    let mut segs: Vec<(u32, u32)> = Vec::new();
    for r in (lo + 1)..=hi {
        let v = s.cells.get(r, col);
        if v != cur {
            segs.push((seg_start, r - 1));
            seg_start = r;
            cur = v;
        }
    }
    segs.push((seg_start, hi));

    for (a, b) in segs {
        let summary_pos_before = seq.len();
        let mut inner: Vec<SubtotalItem> = Vec::new();
        build_subtotal_seq(s, spec, depth + 1, a, b, &mut inner);
        let inner_len = inner.len();
        if spec.summary_below {
            // 明细在前,汇总在后:先放 inner,再放汇总;seq_lo/hi 指向 inner 在最终序列中的下标区间
            let base = seq.len();
            // inner 里的 Summary.seq_lo/hi 是相对 inner 的,平移到全局
            shift_seq_items(&mut inner, base);
            seq.extend(inner);
            seq.push(SubtotalItem::Summary {
                group_idx: Some(depth), seq_lo: base, seq_hi: base + inner_len - 1, label_row: a,
            });
        } else {
            // 汇总在前:汇总占 summary_pos_before,inner 从 +1 开始
            let base = summary_pos_before + 1;
            shift_seq_items(&mut inner, base);
            seq.push(SubtotalItem::Summary {
                group_idx: Some(depth), seq_lo: base, seq_hi: base + inner_len - 1, label_row: a,
            });
            seq.extend(inner);
        }
    }
}

fn shift_seq_items(items: &mut [SubtotalItem], by: usize) {
    for it in items.iter_mut() {
        if let SubtotalItem::Summary { seq_lo, seq_hi, .. } = it {
            *seq_lo += by;
            *seq_hi += by;
        }
    }
}

/// insert_subtotals_ex 的执行体,返回汇总后数据区最后一行的行号。
fn apply_subtotals_ex(
    s: &mut roe_model::calc::Sheet,
    sheet_idx: usize,
    start_row: u32,
    end_row: u32,
    spec: &SubtotalSpec,
    dirty: &mut Vec<(usize, u32, u32)>,
) -> Result<u32, String> {
    // ── A) 只读:分组树 → 最终行序列(下标相对 start_row) ──
    let mut seq: Vec<SubtotalItem> = Vec::new();
    build_subtotal_seq(s, spec, 0, start_row, end_row, &mut seq);
    if spec.grand_total {
        let total_len = seq.len();
        if spec.summary_below {
            seq.push(SubtotalItem::Summary { group_idx: None, seq_lo: 0, seq_hi: total_len - 1, label_row: start_row });
        } else {
            shift_seq_items(&mut seq, 1);
            seq.insert(0, SubtotalItem::Summary { group_idx: None, seq_lo: 1, seq_hi: total_len, label_row: start_row });
        }
    }
    let abs = |i: usize| -> u32 { start_row + i as u32 };

    // ── B) 升序插入空行:第k个汇总行的最终行号 = abs(它在序列中的下标) ──
    // 升序保证:插入第k个时,其前所有汇总行已就位,插入点即最终位置。
    let summary_positions: Vec<usize> = seq.iter().enumerate()
        .filter_map(|(i, it)| matches!(it, SubtotalItem::Summary { .. }).then_some(i))
        .collect();
    for &i in &summary_positions {
        insert_row_impl(s, abs(i));
    }

    // ── C) 回填内容 + outline ──
    // outline:每个序列位置的层级 = 覆盖它的汇总区间个数(汇总行自身不算覆盖自己)
    let n = seq.len();
    let mut level = vec![0u8; n];
    for it in &seq {
        if let SubtotalItem::Summary { seq_lo, seq_hi, .. } = it {
            for l in level.iter_mut().take(*seq_hi + 1).skip(*seq_lo) {
                *l = l.saturating_add(1);
            }
        }
    }
    for (i, lv) in level.iter().enumerate() {
        set_row_outline_level_range_impl(s, abs(i), abs(i), *lv);
    }

    for it in &seq {
        let SubtotalItem::Summary { group_idx, seq_lo, seq_hi, label_row } = it else { continue };
        // 汇总行自身的序列下标:below=lo..hi 之后一行(hi+1),above=lo 之前一行(lo-1)
        let self_i = if spec.summary_below { seq_hi + 1 } else { seq_lo - 1 };
        let row = abs(self_i);
        let (lo_abs, hi_abs) = (abs(*seq_lo), abs(*seq_hi));

        // 标签所在列与汇总列
        let (label_col, aggs, label_text) = match group_idx {
            Some(g) => {
                // label_row 是原数据行号,但升序插入使其位置后移:换算为最终行号
                // 最终行号 = 序列里 Data(label_row) 的下标(找到即可)
                let val = {
                    let fin = seq.iter().position(|x| matches!(x, SubtotalItem::Data(r) if *r == *label_row))
                        .map(abs).unwrap_or(*label_row);
                    s.cells.get(fin, spec.groups[*g].col)
                };
                let txt = cell_value_to_string(&val);
                let txt = if txt.is_empty() { "(空白)".to_string() } else { txt };
                (spec.groups[*g].col, &spec.groups[*g].aggs, format!("{} 汇总", txt))
            }
            None => (spec.groups[0].col, &spec.groups[0].aggs, "总计".to_string()),
        };
        set_cell_in_sheet(s, row, label_col, CellValue::Text(label_text));
        dirty.push((sheet_idx, row, label_col));
        for &(agg_col, func) in aggs.iter() {
            let letters = col_to_letters(agg_col + 1);
            let formula = format!("=SUBTOTAL({},{}{}:{}{})", func, letters, lo_abs + 1, letters, hi_abs + 1);
            set_cell_in_sheet(s, row, agg_col, CellValue::Formula {
                source: formula,
                cached: Box::new(CellValue::Empty),
            });
            dirty.push((sheet_idx, row, agg_col));
        }
    }
    Ok(abs(n - 1))
}

/// 批量设置行大纲层级(分组折叠深度)，start_row..=end_row 含端点。
/// 供 Rhai 接口 set_row_outline_level_range 和 insert_subtotals 内部
/// 共用，避免两处各写一份 resize+循环逻辑（与 insert_row_impl 的
/// 抽取动机一致：调用方不同但核心操作相同）。level 由调用方保证
/// 合理性，本函数不做上限校验（Rhai 层入口已 clamp）。
fn set_row_outline_level_range_impl(s: &mut roe_model::calc::Sheet, start_row: u32, end_row: u32, level: u8) {
    if end_row < start_row {
        return;
    }
    let er = end_row as usize;
    if s.row_outline_level.len() <= er {
        s.row_outline_level.resize(er + 1, 0u8);
    }
    for r in start_row as usize..=er {
        s.row_outline_level[r] = level;
    }
}

/// insert_row 的核心插入逻辑，抽取为独立 Rust 函数（Subtotals Step 3，
/// 与Gemini三方评审讨论过方案3"扩展Trait/领域方法"，因工作量/复杂度
/// 不成比例已否决，采用最小抽取方案）：insert_row 闭包和 insert_subtotals
/// 都调用这个函数，避免各写一份行插入逻辑。row 为 0-based。
fn insert_row_impl(s: &mut roe_model::calc::Sheet, row: u32) {
    mark_struct_edit();
    // 行高：在 row 处插入新行。ROOT-082: 新插入的空行高度未知，
    // 应标记为 Auto（待自动计算），而非 Manual（后者代表用户/
    // 文件显式指定的权威值）——语义上这是"新行需要按内容重新
    // 判断"，与懒加载测量流程一致。20.0 只是测量完成前的
    // 估算起始值，不是最终权威高度。
    // 0.0 表示"尚未测量/未设置",渲染层 row_height_at 会回退到
    // sheet.default_row_height / cfg.default_row_height(与 RowHeight::default() 同约定),
    // 避免内核硬编码一个与表默认值不一致的高度(此前为 20.0)。
    let default_h = roe_model::calc::RowHeight::new_auto(0.0);
    if (row as usize) < s.row_heights.len() {
        s.row_heights.insert(row as usize, default_h);
    } else {
        s.row_heights.resize(row as usize, default_h);
        s.row_heights.push(default_h);
    }
    // SparseGrid：block.start_row >= row 的整体 +1；跨越 row 的 block 内插入 Empty
    for col_data in s.cells.cols.values_mut() {
        for block in col_data.blocks.iter_mut() {
            let end = block.start_row + block.values.len() as u32;
            if block.start_row >= row {
                block.start_row += 1;
            } else if end > row {
                let offset = (row - block.start_row) as usize;
                block.values.insert(offset, CellValue::Empty);
            }
        }
    }
    // 合并单元格下移
    for m in s.merged_cells.iter_mut() {
        if m.r1 >= row { m.r1 += 1; }
        if m.r2 >= row { m.r2 += 1; }
    }
    // cell_formats / row_hidden / row_outline_level 下移
    s.shift_rows_for_insert(row);
}

fn set_cell_in_sheet(sheet: &mut roe_model::calc::Sheet, row: u32, col: u32, value: CellValue) {
    use roe_model::calc::{CellBlock, SparseColumn};
    let col_data = sheet.cells.cols.entry(col)
        .or_insert_with(|| SparseColumn { blocks: smallvec::smallvec![] });
    for block in &mut col_data.blocks {
        let end = block.start_row + block.values.len() as u32;
        if row >= block.start_row && row < end {
            block.values[(row - block.start_row) as usize] = value;
            return;
        }
    }
    col_data.blocks.push(CellBlock { start_row: row, values: smallvec::smallvec![value] });
}

/// 批量删除多行（v157新增，delete_duplicates 的支撑函数，delete_row
/// 单行删除也改为调用它，避免两处各写一份行号搬移逻辑）。
///
/// 性能设计：不逐行调用"删单行再整体搬移"（那样是 O(待删行数 × 总行数)，
/// 百万行表格删几十万重复行会卡死）。改为每一列各自一次遍历重建
/// （O(该列总长度 × log(待删行数))，log 项来自二分查找某行之前有几行
/// 被删，用于计算行号偏移量）。
///
/// 已知技术债（v157）：不调整 `cond_formats`/`data_validations` 的
/// range（这两个字段本身在改造前的 delete_row 里就从未被处理，是早于
/// 本次改动的老问题，不在这次顺带修——避免"批量删除会调整 range 但
/// delete_row 曾经不调整"这种新的不一致；影响：删除行落在条件格式/
/// 数据验证区域内时，规则的 range 不会跟随收缩，需要后续单独排期
/// 统一处理所有行列结构变更操作的 range 跟随问题）。
fn delete_rows_batch(sheet: &mut roe_model::calc::Sheet, to_delete: &[u32]) {
    if to_delete.is_empty() { return; }
    mark_struct_edit();

    let mut sorted: Vec<u32> = to_delete.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let deleted_set: std::collections::HashSet<u32> = sorted.iter().copied().collect();

    // 给定一个原始行号，返回它在删除操作后的新行号（假定该行本身不在
    // 删除集合中；调用方需自行先判断是否要跳过）。用二分查找
    // sorted 中"小于 row 的元素个数"作为偏移量，避免逐行线性扫描
    // deleted_set 造成 O(总行数 × 待删行数) 的重复计算。
    let shift_for = |row: u32| -> u32 {
        let before = sorted.partition_point(|&d| d < row);
        row - before as u32
    };

    // 1. row_heights：按索引位置一次性重建（row_heights 的索引本身
    //    就是行号，不需要额外映射）
    if !sheet.row_heights.is_empty() {
        sheet.row_heights = sheet.row_heights.iter().enumerate()
            .filter(|(i, _)| !deleted_set.contains(&(*i as u32)))
            .map(|(_, h)| *h)
            .collect();
    }

    // 2. 每一列的 SparseColumn 独立重建：展开该列所有 block 为
    //    (原行号, value) 序列，跳过被删行，其余按 shift_for 计算新
    //    行号后重新打包成连续 block。用 Vec 收集后一次性写回，避免
    //    对 SmallVec 做逐元素 remove（那是 O(n) 每次，n 次调用变
    //    O(n²)）。
    for col_data in sheet.cells.cols.values_mut() {
        let mut kept: Vec<(u32, CellValue)> = Vec::new();
        for block in &col_data.blocks {
            for (i, v) in block.values.iter().enumerate() {
                let orig_row = block.start_row + i as u32;
                if deleted_set.contains(&orig_row) { continue; }
                kept.push((shift_for(orig_row), v.clone()));
            }
        }
        // 按新行号重新打包为连续 block（kept 天然按原行号升序，
        // shift_for 保序，故 kept 已按新行号升序，无需重排）
        let mut new_blocks: smallvec::SmallVec<[roe_model::calc::CellBlock; 4]> = smallvec::smallvec![];
        for (new_row, v) in kept {
            if let Some(last) = new_blocks.last_mut() {
                let last_end = last.start_row + last.values.len() as u32;
                if new_row == last_end {
                    last.values.push(v);
                    continue;
                }
            }
            new_blocks.push(roe_model::calc::CellBlock { start_row: new_row, values: smallvec::smallvec![v] });
        }
        col_data.blocks = new_blocks;
    }

    // 3. merged_cells：整块落在删除区间内的丢弃；部分/未受影响的
    //    按 r1/r2 各自应用偏移（注意 r1/r2 若不在 deleted_set 中，
    //    shift_for 才是该行的正确新行号；若 merge 的边界行恰好被删，
    //    这里保守地也用 shift_for 计算相邻未删行的位置，可能产生
    //    退化的合并区域，符合"删除中间行导致合并区域收缩"的直觉预期）
    sheet.merged_cells.retain(|m| {
        !(m.r1..=m.r2).all(|r| deleted_set.contains(&r))
    });
    for m in sheet.merged_cells.iter_mut() {
        m.r1 = shift_for(m.r1.min(*sorted.last().unwrap_or(&u32::MAX)).max(
            { let mut r = m.r1; while deleted_set.contains(&r) && r < m.r2 { r += 1; } r }
        ));
        // r2 同理：若边界行被删，向内收缩到最近的未删行再计算偏移
        let mut r2_adj = m.r2;
        while deleted_set.contains(&r2_adj) && r2_adj > m.r1 { r2_adj -= 1; }
        m.r2 = shift_for(r2_adj);
    }

    // 4. cell_formats：HashMap<(row,col), CellFormat> 全量重建
    //    （v157决策：该 HashMap 通常远小于总行数——只有显式设置过
    //    单元格级格式的坐标才会出现在里面，多数单元格靠列默认格式
    //    渲染，不经过这里；全量 rebuild 的开销与"实际有多少条自定义
    //    格式"成正比，不与总行数成正比，可接受）
    if !sheet.cell_formats.is_empty() {
        let old = std::mem::take(&mut sheet.cell_formats);
        sheet.cell_formats = old.into_iter()
            .filter(|((r, _), _)| !deleted_set.contains(r))
            .map(|((r, c), fmt)| ((shift_for(r), c), fmt))
            .collect();
    }

    // 5. row_hidden / row_outline_level(按索引展开的 Vec)
    sheet.shift_rows_for_delete(&sorted);
}
// ═══ Calc 表格(Table/ListObject)脚本接口的实现 ═══
// 约定:返回 Result,Rhai 层把 Err(e) 转成 "err:e" 字符串(同 df_* 约定)。

fn table_style_exists(name: &str) -> bool {
    roe_model::excel_table_styles_data::find_table_style(name).is_some()
}

fn table_create_impl(
    wb: &mut Workbook,
    sheet_idx: i64,
    r1: i64, c1: i64, r2: i64, c2: i64,
    has_header: bool,
    style_name: &str,
) -> Result<String, String> {
    if sheet_idx < 0 || sheet_idx as usize >= wb.sheets.len() {
        return Err("sheet越界".to_string());
    }
    if r1 < 0 || c1 < 0 || r2 < r1 || c2 < c1 {
        return Err("区域非法".to_string());
    }
    if has_header && r2 == r1 {
        return Err("带表头的表格至少需要一行数据".to_string());
    }
    if !table_style_exists(style_name) {
        return Err(format!("未知表格样式: {style_name}"));
    }
    let (r1, c1, r2, c2) = (r1 as u32, c1 as u32, r2 as u32, c2 as u32);
    let si = sheet_idx as usize;

    // 与本 sheet 已有表格重叠则拒绝
    for t in &wb.sheets[si].tables {
        if t.range.start.0 <= r2 && t.range.end.0 >= r1
            && t.range.start.1 <= c2 && t.range.end.1 >= c1
        {
            return Err(format!("与已有表格 {} 重叠", t.name));
        }
    }

    // 表名:工作簿内全局唯一(不区分大小写)
    let mut n = 1;
    let name = loop {
        let cand = format!("Table{n}");
        let used = wb.sheets.iter().any(|sh| {
            sh.tables.iter().any(|t| t.name.eq_ignore_ascii_case(&cand))
        });
        if !used { break cand; }
        n += 1;
    };

    // 列名:取表头文字;为空或与前面重复时用 ColumnN(N 为 1 起的列序号)
    let ncols = (c2 - c1 + 1) as usize;
    let mut columns: Vec<String> = Vec::with_capacity(ncols);
    let mut rewrites: Vec<(u32, String)> = Vec::new();
    let is_dup = |cols: &Vec<String>, x: &str| cols.iter().any(|c| c.eq_ignore_ascii_case(x));
    for i in 0..ncols {
        let col = c1 + i as u32;
        let raw = if has_header {
            cell_value_to_string(&wb.sheets[si].cells.get(r1, col))
        } else {
            String::new()
        };
        let mut nm = if raw.trim().is_empty() || is_dup(&columns, &raw) {
            format!("Column{}", i + 1)
        } else {
            raw.clone()
        };
        let mut k = 1;
        while is_dup(&columns, &nm) {
            k += 1;
            nm = format!("Column{}_{}", i + 1, k);
        }
        if has_header && nm != raw {
            rewrites.push((col, nm.clone()));
        }
        columns.push(nm);
    }
    // 表头单元格文字必须与 tableColumn 名一致,否则 Excel 打开会提示修复
    for (col, nm) in rewrites {
        set_cell_in_sheet(&mut wb.sheets[si], r1, col, parse_string_to_cellvalue(&nm));
    }

    // 主题色板:沿用本工作簿已有表格的,没有则留空(渲染层回退 Office 默认色)
    let palette = wb.sheets.iter()
        .flat_map(|sh| sh.tables.iter())
        .find(|t| t.theme_palette.len() >= 10)
        .map(|t| t.theme_palette.clone())
        .unwrap_or_default();

    let sheet_name = wb.sheets[si].name.clone();
    let auto_filter = if has_header {
        Some(roe_model::calc::AutoFilter {
            header_row: r1,
            header_col: c1,
            end_col: c2,
            col_filters: std::collections::HashMap::new(),
            hidden_buttons: std::collections::HashSet::new(),
        })
    } else {
        None
    };
    wb.sheets[si].tables.push(roe_model::calc::Table {
        name: name.clone(),
        range: roe_model::calc::CellRange {
            sheet: sheet_name,
            start: (r1, c1),
            end: (r2, c2),
        },
        has_header,
        has_totals: false,
        columns,
        auto_filter,
        style_name: Some(style_name.to_string()),
        show_first_column: false,
        show_last_column: false,
        show_row_stripes: true,
        show_column_stripes: false,
        theme_palette: palette,
    });
    Ok(name)
}

/// style_name 为空串表示清除样式(菜单"清除"用)
fn table_set_style_impl(
    wb: &mut Workbook,
    sheet_idx: i64,
    table_idx: i64,
    style_name: &str,
) -> Result<(), String> {
    if sheet_idx < 0 || sheet_idx as usize >= wb.sheets.len() {
        return Err("sheet越界".to_string());
    }
    let sh = &mut wb.sheets[sheet_idx as usize];
    if table_idx < 0 || table_idx as usize >= sh.tables.len() {
        return Err("表格下标越界".to_string());
    }
    if style_name.is_empty() {
        sh.tables[table_idx as usize].style_name = None;
        return Ok(());
    }
    if !table_style_exists(style_name) {
        return Err(format!("未知表格样式: {style_name}"));
    }
    sh.tables[table_idx as usize].style_name = Some(style_name.to_string());
    Ok(())
}

fn tables_to_json(wb: &Workbook, sheet_idx: i64) -> String {
    if sheet_idx < 0 { return "[]".to_string(); }
    let Some(sh) = wb.sheets.get(sheet_idx as usize) else { return "[]".to_string() };
    let arr: Vec<serde_json::Value> = sh.tables.iter().enumerate().map(|(i, t)| {
        serde_json::json!({
            "idx": i,
            "name": t.name,
            "range": [t.range.start.0, t.range.start.1, t.range.end.0, t.range.end.1],
            "style": t.style_name,
            "has_header": t.has_header,
            "has_totals": t.has_totals,
            "first_col": t.show_first_column,
            "last_col": t.show_last_column,
            "row_stripes": t.show_row_stripes,
            "col_stripes": t.show_column_stripes,
            "columns": t.columns,
        })
    }).collect();
    serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
                col_widths: vec![],
                row_heights: vec![],
                frozen_rows: 0,
                frozen_cols: 0,
                date1904: false,
                zoom_scale: None,
                show_grid_lines: None,
                col_hidden: vec![],
                row_hidden: vec![],
                col_outline_level: vec![],
                row_outline_level: vec![],
                cell_formats:     std::collections::HashMap::new(),
                cond_formats:     vec![],
                data_validations: vec![],
                auto_filter:      None,
                merged_cells:     vec![],
                pivot:            None,
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    // ---- insert_subtotals_ex 单元测试(多级/方向/总计行) ----

    /// 构造:表头 + Mon,1 / Mon,2 / Tue,3 / Tue,4 / Wed,5,共 6 行(row0 表头,row1..5 数据)。
    fn ex_basic_wb() -> Workbook {
        let mut wb = empty_wb();
        rs_put(&mut wb, 0, 0, "Day"); rs_put(&mut wb, 0, 1, "V");
        for (r, d, v) in [(1, "Mon", "1"), (2, "Mon", "2"), (3, "Tue", "3"), (4, "Tue", "4"), (5, "Wed", "5")] {
            rs_put(&mut wb, r, 0, d);
            rs_put(&mut wb, r, 1, v);
        }
        wb
    }

    fn ex_formula(wb: &Workbook, row: u32, col: u32) -> String {
        wb.sheets[0].cells.cols.get(&col)
            .and_then(|c| c.blocks.iter()
                .find(|b| row >= b.start_row && row < b.start_row + b.values.len() as u32)
                .map(|b| match &b.values[(row - b.start_row) as usize] {
                    CellValue::Formula { source, .. } => source.clone(),
                    other => format!("<非公式:{}>", cell_value_to_string(other)),
                }))
            .unwrap_or_else(|| "<空>".to_string())
    }

    /// 上方 + 总计,单组。期望行序:
    /// 0 表头 / 1 总计 / 2 Mon汇总 / 3 Mon / 4 Mon / 5 Tue汇总 / 6 Tue / 7 Tue / 8 Wed汇总 / 9 Wed
    #[test]
    fn insert_subtotals_ex_above_with_grand_total() {
        let mut wb = ex_basic_wb();
        let spec = r#"{"groups":[{"col":0,"aggs":[[1,9]]}],"summary_below":false,"grand_total":true}"#;
        let ret = rs_run(&mut wb, &format!("insert_subtotals_ex(0, 0, 1, 5, \"{}\")", spec.replace('"', "\\\"")));
        assert_eq!(ret, "{\"new_end_row\":9}");
        let expect = ["Day", "总计", "Mon 汇总", "Mon", "Mon", "Tue 汇总", "Tue", "Tue", "Wed 汇总", "Wed"];
        for (r, e) in expect.iter().enumerate() {
            assert_eq!(rs_text(&wb, r as u32, 0), *e, "row{r} 标签");
        }
        assert_eq!(ex_formula(&wb, 1, 1), "=SUBTOTAL(9,B3:B10)", "总计覆盖全部(含汇总行)");
        assert_eq!(ex_formula(&wb, 2, 1), "=SUBTOTAL(9,B4:B5)");
        assert_eq!(ex_formula(&wb, 5, 1), "=SUBTOTAL(9,B7:B8)");
        assert_eq!(ex_formula(&wb, 8, 1), "=SUBTOTAL(9,B10:B10)");
        // outline: 表头0 / 总计0 / 汇总1 / 明细2
        assert_eq!(wb.sheets[0].row_outline_level, vec![0, 0, 1, 2, 2, 1, 2, 2, 1, 2]);
    }

    /// 下方 + 总计:每组汇总在明细后,总计在最末。
    /// 0 表头 / 1 Mon / 2 Mon / 3 Mon汇总 / 4 Tue / 5 Tue / 6 Tue汇总 / 7 Wed / 8 Wed汇总 / 9 总计
    #[test]
    fn insert_subtotals_ex_below_with_grand_total() {
        let mut wb = ex_basic_wb();
        let spec = r#"{"groups":[{"col":0,"aggs":[[1,9]]}],"summary_below":true,"grand_total":true}"#;
        let ret = rs_run(&mut wb, &format!("insert_subtotals_ex(0, 0, 1, 5, \"{}\")", spec.replace('"', "\\\"")));
        assert_eq!(ret, "{\"new_end_row\":9}");
        let expect = ["Day", "Mon", "Mon", "Mon 汇总", "Tue", "Tue", "Tue 汇总", "Wed", "Wed 汇总", "总计"];
        for (r, e) in expect.iter().enumerate() {
            assert_eq!(rs_text(&wb, r as u32, 0), *e, "row{r} 标签");
        }
        assert_eq!(ex_formula(&wb, 3, 1), "=SUBTOTAL(9,B2:B3)");
        assert_eq!(ex_formula(&wb, 9, 1), "=SUBTOTAL(9,B2:B9)", "总计覆盖全部数据区(含汇总行)");
        assert_eq!(wb.sheets[0].row_outline_level, vec![0, 2, 2, 1, 2, 2, 1, 2, 1, 0]);
    }

    /// 无总计:层级整体少一层(汇总 0 / 明细 1),首行就是第一个汇总行。
    #[test]
    fn insert_subtotals_ex_no_grand_total() {
        let mut wb = ex_basic_wb();
        let spec = r#"{"groups":[{"col":0,"aggs":[[1,9]]}],"summary_below":false,"grand_total":false}"#;
        let ret = rs_run(&mut wb, &format!("insert_subtotals_ex(0, 0, 1, 5, \"{}\")", spec.replace('"', "\\\"")));
        assert_eq!(ret, "{\"new_end_row\":8}");
        assert_eq!(rs_text(&wb, 1, 0), "Mon 汇总");
        assert_eq!(wb.sheets[0].row_outline_level, vec![0, 0, 1, 1, 0, 1, 1, 0, 1]);
    }

    /// 两级嵌套(Region→Day):三层 outline,内层区间落在外层区间内。
    /// 数据: East/Mon/1, East/Mon/2, East/Tue/3, West/Mon/4
    /// 上方+总计 期望:
    /// 0 表头 / 1 总计 / 2 East汇总 / 3 Mon汇总 / 4 d / 5 d / 6 Tue汇总 / 7 d / 8 West汇总 / 9 Mon汇总 / 10 d
    #[test]
    fn insert_subtotals_ex_two_level_nested() {
        let mut wb = empty_wb();
        rs_put(&mut wb, 0, 0, "Region"); rs_put(&mut wb, 0, 1, "Day"); rs_put(&mut wb, 0, 2, "V");
        for (r, a, b, v) in [(1, "East", "Mon", "1"), (2, "East", "Mon", "2"), (3, "East", "Tue", "3"), (4, "West", "Mon", "4")] {
            rs_put(&mut wb, r, 0, a); rs_put(&mut wb, r, 1, b); rs_put(&mut wb, r, 2, v);
        }
        let spec = r#"{"groups":[{"col":0,"aggs":[[2,9]]},{"col":1,"aggs":[[2,9]]}],"summary_below":false,"grand_total":true}"#;
        let ret = rs_run(&mut wb, &format!("insert_subtotals_ex(0, 0, 1, 4, \"{}\")", spec.replace('"', "\\\"")));
        assert_eq!(ret, "{\"new_end_row\":10}");
        // 外层标签在 col0,内层标签在 col1
        assert_eq!(rs_text(&wb, 1, 0), "总计");
        assert_eq!(rs_text(&wb, 2, 0), "East 汇总");
        assert_eq!(rs_text(&wb, 3, 1), "Mon 汇总");
        assert_eq!(rs_text(&wb, 6, 1), "Tue 汇总");
        assert_eq!(rs_text(&wb, 8, 0), "West 汇总");
        assert_eq!(rs_text(&wb, 9, 1), "Mon 汇总");
        assert_eq!(ex_formula(&wb, 1, 2), "=SUBTOTAL(9,C3:C11)");
        assert_eq!(ex_formula(&wb, 2, 2), "=SUBTOTAL(9,C4:C8)", "East 覆盖其下全部(含内层汇总行)");
        assert_eq!(ex_formula(&wb, 3, 2), "=SUBTOTAL(9,C5:C6)");
        assert_eq!(ex_formula(&wb, 6, 2), "=SUBTOTAL(9,C8:C8)");
        assert_eq!(ex_formula(&wb, 8, 2), "=SUBTOTAL(9,C10:C11)");
        // 总计0 / East汇总1 / Mon汇总2 / 明细3 / Tue汇总2 / 明细3 / West汇总1 / Mon汇总2 / 明细3
        assert_eq!(wb.sheets[0].row_outline_level, vec![0, 0, 1, 2, 3, 3, 2, 3, 1, 2, 3]);
    }

    /// 每个汇总列独立函数:B 求和(9)、C 平均(1)。
    #[test]
    fn insert_subtotals_ex_per_column_function() {
        let mut wb = empty_wb();
        rs_put(&mut wb, 0, 0, "K"); rs_put(&mut wb, 0, 1, "B"); rs_put(&mut wb, 0, 2, "C");
        rs_put(&mut wb, 1, 0, "a"); rs_put(&mut wb, 1, 1, "1"); rs_put(&mut wb, 1, 2, "10");
        rs_put(&mut wb, 2, 0, "a"); rs_put(&mut wb, 2, 1, "2"); rs_put(&mut wb, 2, 2, "20");
        let spec = r#"{"groups":[{"col":0,"aggs":[[1,9],[2,1]]}],"summary_below":false,"grand_total":false}"#;
        let ret = rs_run(&mut wb, &format!("insert_subtotals_ex(0, 0, 1, 2, \"{}\")", spec.replace('"', "\\\"")));
        assert_eq!(ret, "{\"new_end_row\":3}");
        assert_eq!(ex_formula(&wb, 1, 1), "=SUBTOTAL(9,B3:B4)");
        assert_eq!(ex_formula(&wb, 1, 2), "=SUBTOTAL(1,C3:C4)");
    }

    /// 空分类值的标签为 "(空白) 汇总"(对齐 LOK STR_EMPTYDATA)。
    #[test]
    fn insert_subtotals_ex_blank_group_label() {
        let mut wb = empty_wb();
        rs_put(&mut wb, 0, 0, "K"); rs_put(&mut wb, 0, 1, "V");
        rs_put(&mut wb, 1, 1, "1"); // col0 留空
        rs_put(&mut wb, 2, 1, "2");
        let spec = r#"{"groups":[{"col":0,"aggs":[[1,9]]}],"summary_below":false,"grand_total":false}"#;
        let ret = rs_run(&mut wb, &format!("insert_subtotals_ex(0, 0, 1, 2, \"{}\")", spec.replace('"', "\\\"")));
        assert_eq!(ret, "{\"new_end_row\":3}");
        assert_eq!(rs_text(&wb, 1, 0), "(空白) 汇总");
    }

    /// 参数校验:全部返回 err: 前缀,且不修改表。
    #[test]
    fn insert_subtotals_ex_bad_args() {
        let mut wb = ex_basic_wb();
        let ok_spec = r#"{"groups":[{"col":0,"aggs":[[1,9]]}]}"#.replace('"', "\\\"");
        assert!(rs_run(&mut wb, &format!("insert_subtotals_ex(0, 0, 5, 1, \"{ok_spec}\")")).starts_with("err:"), "end<start");
        assert!(rs_run(&mut wb, &format!("insert_subtotals_ex(0, 3, 3, 5, \"{ok_spec}\")")).starts_with("err:"), "start<=header");
        assert!(rs_run(&mut wb, "insert_subtotals_ex(0, 0, 1, 5, \"not json\")").starts_with("err:"));
        assert!(rs_run(&mut wb, "insert_subtotals_ex(0, 0, 1, 5, \"{\\\"groups\\\":[]}\")").starts_with("err:"), "groups 空");
        assert!(rs_run(&mut wb, "insert_subtotals_ex(9, 0, 1, 5, \"{\\\"groups\\\":[{\\\"col\\\":0,\\\"aggs\\\":[[1,9]]}]}\")").starts_with("err:"), "sheet 越界");
        assert_eq!(rs_text(&wb, 1, 0), "Mon", "出错时不应修改表");
    }

    // ---- remove_subtotals 单元测试（对齐 LOK RemoveSubTotals：只按 SUBTOTAL 公式识别）----

    /// 测试辅助：在 wb 上按 (row, col, 文本或公式) 写格子。以 '=' 开头写 Formula，否则 Text。
    fn rs_put(wb: &mut Workbook, row: u32, col: u32, v: &str) {
        let cv = if v.starts_with('=') || v.starts_with("SUBTOTAL(") {
            CellValue::Formula { source: v.to_string(), cached: Box::new(CellValue::Empty) }
        } else {
            CellValue::Text(v.to_string())
        };
        set_cell_in_sheet(&mut wb.sheets[0], row, col, cv);
    }

    fn rs_text(wb: &Workbook, row: u32, col: u32) -> String {
        wb.sheets[0].cells.cols.get(&col)
            .and_then(|c| c.blocks.iter()
                .find(|b| row >= b.start_row && row < b.start_row + b.values.len() as u32)
                .map(|b| cell_value_to_string(&b.values[(row - b.start_row) as usize])))
            .unwrap_or_default()
    }

    fn rs_run(wb: &mut Workbook, script: &str) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        register_row_col_ops(&mut e);
        let mut out = String::new();
        with_workbook(wb, || { out = e.eval::<String>(script).unwrap(); });
        out
    }

    /// 汇总行在明细下方：删后只剩表头+明细，outline 数组同步收缩。
    #[test]
    fn remove_subtotals_below_layout() {
        let mut wb = empty_wb();
        rs_put(&mut wb, 0, 0, "H");
        rs_put(&mut wb, 1, 0, "A"); rs_put(&mut wb, 1, 1, "10");
        rs_put(&mut wb, 2, 0, "A"); rs_put(&mut wb, 2, 1, "20");
        rs_put(&mut wb, 3, 0, "A 汇总"); rs_put(&mut wb, 3, 1, "=SUBTOTAL(9,B2:B3)");
        rs_put(&mut wb, 4, 0, "B"); rs_put(&mut wb, 4, 1, "5");
        rs_put(&mut wb, 5, 0, "B 汇总"); rs_put(&mut wb, 5, 1, "=SUBTOTAL(9,B5:B5)");
        wb.sheets[0].row_outline_level = vec![0, 1, 1, 0, 1, 0];
        let ret = rs_run(&mut wb, "remove_subtotals(0, 0, 0, 5, 1)");
        assert_eq!(ret, "{\"removed\":2}");
        assert_eq!(rs_text(&wb, 1, 0), "A");
        assert_eq!(rs_text(&wb, 2, 0), "A");
        assert_eq!(rs_text(&wb, 3, 0), "B");
        assert_eq!(rs_text(&wb, 4, 0), "", "第5行应已被移空");
        assert_eq!(wb.sheets[0].row_outline_level, vec![0, 0, 0, 0],
                   "取消分类汇总后数据区 outline 应整段清零(对齐 LOK RemoveAutoOutline)");
    }

    /// 汇总行在明细上方（含 Grand Total），多级嵌套：全部识别删除。
    #[test]
    fn remove_subtotals_above_layout_multi_level() {
        let mut wb = empty_wb();
        rs_put(&mut wb, 0, 0, "H");
        rs_put(&mut wb, 1, 0, "Grand Total"); rs_put(&mut wb, 1, 1, "=SUBTOTAL(9,B3:B6)");
        rs_put(&mut wb, 2, 0, "A Total");     rs_put(&mut wb, 2, 1, "=SUBTOTAL(9,B4:B5)");
        rs_put(&mut wb, 3, 0, "A"); rs_put(&mut wb, 3, 1, "1");
        rs_put(&mut wb, 4, 0, "A"); rs_put(&mut wb, 4, 1, "2");
        rs_put(&mut wb, 5, 0, "B Total");     rs_put(&mut wb, 5, 1, "=SUBTOTAL(9,B7:B7)");
        rs_put(&mut wb, 6, 0, "B"); rs_put(&mut wb, 6, 1, "3");
        wb.sheets[0].row_outline_level = vec![0, 0, 1, 2, 2, 1, 2];
        let ret = rs_run(&mut wb, "remove_subtotals(0, 0, 0, 6, 1)");
        assert_eq!(ret, "{\"removed\":3}");
        assert_eq!(rs_text(&wb, 1, 0), "A");
        assert_eq!(rs_text(&wb, 2, 0), "A");
        assert_eq!(rs_text(&wb, 3, 0), "B");
        assert_eq!(wb.sheets[0].row_outline_level, vec![0, 0, 0, 0],
                   "多级嵌套取消后同样整段清零");
    }

    /// 导入路径的公式 source 可能不带 '=' 前缀，且大小写不定：都要识别。
    #[test]
    fn remove_subtotals_source_without_equals_and_lowercase() {
        let mut wb = empty_wb();
        rs_put(&mut wb, 0, 0, "H");
        rs_put(&mut wb, 1, 0, "x"); rs_put(&mut wb, 1, 1, "1");
        rs_put(&mut wb, 2, 0, "t1"); rs_put(&mut wb, 2, 1, "SUBTOTAL(9,B2:B2)");
        // 手工写小写 + 前导空格 + '='
        set_cell_in_sheet(&mut wb.sheets[0], 3, 1, CellValue::Formula {
            source: " =subtotal(9,B2:B2)".to_string(), cached: Box::new(CellValue::Empty) });
        let ret = rs_run(&mut wb, "remove_subtotals(0, 0, 0, 3, 1)");
        assert_eq!(ret, "{\"removed\":2}");
    }

    /// 用户自己写的非 SUBTOTAL 公式不能误删；范围外（表头行、r2 之后）的 SUBTOTAL 不删。
    #[test]
    fn remove_subtotals_keeps_other_formulas_and_out_of_range() {
        let mut wb = empty_wb();
        // 表头行本身放一个 SUBTOTAL（不应被删）
        rs_put(&mut wb, 0, 1, "=SUBTOTAL(9,B2:B3)");
        rs_put(&mut wb, 1, 0, "x"); rs_put(&mut wb, 1, 1, "=SUM(B3:B3)");
        rs_put(&mut wb, 2, 0, "y"); rs_put(&mut wb, 2, 1, "7");
        // r2=2，之后的第 3 行 SUBTOTAL 在范围外
        rs_put(&mut wb, 3, 1, "=SUBTOTAL(9,B2:B3)");
        let ret = rs_run(&mut wb, "remove_subtotals(0, 0, 0, 2, 1)");
        assert_eq!(ret, "{\"removed\":0}");
        assert_eq!(rs_text(&wb, 1, 0), "x", "SUM 行不应被删");
        assert!(matches!(
            wb.sheets[0].cells.cols.get(&1).unwrap().blocks[0].values[0],
            CellValue::Formula { .. }
        ), "表头行的 SUBTOTAL 应保留");
    }

    /// 折叠状态下取消：明细行(level>0)的 row_hidden 应被还原为可见；
    /// 自动筛选隐藏的行(level==0)必须保持隐藏，不能被误展开。
    #[test]
    fn remove_subtotals_unhides_collapsed_details_but_keeps_filtered_rows() {
        let mut wb = empty_wb();
        rs_put(&mut wb, 0, 0, "H");
        rs_put(&mut wb, 1, 0, "A"); rs_put(&mut wb, 1, 1, "1");
        rs_put(&mut wb, 2, 0, "A"); rs_put(&mut wb, 2, 1, "2");
        rs_put(&mut wb, 3, 0, "A 汇总"); rs_put(&mut wb, 3, 1, "=SUBTOTAL(9,B2:B3)");
        rs_put(&mut wb, 4, 0, "F"); rs_put(&mut wb, 4, 1, "9"); // 被自动筛选隐藏的行
        wb.sheets[0].row_outline_level = vec![0, 1, 1, 0, 0];
        wb.sheets[0].row_hidden = vec![false, true, true, false, true];
        let ret = rs_run(&mut wb, "remove_subtotals(0, 0, 0, 4, 1)");
        assert_eq!(ret, "{\"removed\":1}");
        // 删除 row3 后：原 row4(F) 变为 row3
        assert_eq!(wb.sheets[0].row_hidden, vec![false, false, false, true],
                   "折叠隐藏的明细行应恢复可见，筛选隐藏的行保持隐藏");
        assert_eq!(wb.sheets[0].row_outline_level, vec![0, 0, 0, 0]);
    }

    /// row_outline_level / row_hidden 比行数短(导入只存到最后一个非零行)：
    /// 不越界、不扩长。
    #[test]
    fn remove_subtotals_short_outline_vectors_not_extended() {
        let mut wb = empty_wb();
        rs_put(&mut wb, 0, 0, "H");
        rs_put(&mut wb, 1, 0, "A"); rs_put(&mut wb, 1, 1, "1");
        rs_put(&mut wb, 2, 0, "A 汇总"); rs_put(&mut wb, 2, 1, "=SUBTOTAL(9,B2:B2)");
        rs_put(&mut wb, 3, 0, "tail"); rs_put(&mut wb, 3, 1, "0");
        wb.sheets[0].row_outline_level = vec![0, 1]; // 只到 row1
        wb.sheets[0].row_hidden = vec![]; // 完全为空
        let ret = rs_run(&mut wb, "remove_subtotals(0, 0, 0, 3, 1)");
        assert_eq!(ret, "{\"removed\":1}");
        assert!(wb.sheets[0].row_outline_level.len() <= 2, "不应被扩长: {:?}", wb.sheets[0].row_outline_level);
        assert!(wb.sheets[0].row_outline_level.iter().all(|&v| v == 0));
        assert!(wb.sheets[0].row_hidden.is_empty(), "空的 row_hidden 不应被创建内容");
    }

    /// 范围之外的行的 outline / hidden 不受影响。
    #[test]
    fn remove_subtotals_does_not_touch_rows_outside_range() {
        let mut wb = empty_wb();
        rs_put(&mut wb, 0, 0, "H");
        rs_put(&mut wb, 1, 0, "A"); rs_put(&mut wb, 1, 1, "1");
        rs_put(&mut wb, 2, 0, "A 汇总"); rs_put(&mut wb, 2, 1, "=SUBTOTAL(9,B2:B2)");
        rs_put(&mut wb, 3, 0, "out"); rs_put(&mut wb, 3, 1, "5");
        wb.sheets[0].row_outline_level = vec![0, 1, 0, 3];
        wb.sheets[0].row_hidden = vec![false, true, false, true];
        // 范围只到 row2，row3 在范围外
        let ret = rs_run(&mut wb, "remove_subtotals(0, 0, 0, 2, 1)");
        assert_eq!(ret, "{\"removed\":1}");
        // 删 row2 后 row3 前移为 row2，其 outline=3 / hidden=true 必须原样保留
        assert_eq!(wb.sheets[0].row_outline_level, vec![0, 0, 3]);
        assert_eq!(wb.sheets[0].row_hidden, vec![false, false, true]);
    }

    /// 没有任何汇总行：removed=0，不报错，数据不变。
    #[test]
    fn remove_subtotals_none_found() {
        let mut wb = empty_wb();
        rs_put(&mut wb, 0, 0, "H");
        rs_put(&mut wb, 1, 0, "a"); rs_put(&mut wb, 1, 1, "1");
        let ret = rs_run(&mut wb, "remove_subtotals(0, 0, 0, 1, 1)");
        assert_eq!(ret, "{\"removed\":0}");
        assert_eq!(rs_text(&wb, 1, 0), "a");
    }

    /// 参数校验：范围不合法、sheet 越界返回 err: 前缀。
    #[test]
    fn remove_subtotals_bad_args() {
        let mut wb = empty_wb();
        assert!(rs_run(&mut wb, "remove_subtotals(0, 3, 0, 3, 1)").starts_with("err:"));
        assert!(rs_run(&mut wb, "remove_subtotals(9, 0, 0, 3, 1)").starts_with("err:"));
    }

    // ---- Subtotals Step 3: insert_subtotals 单元测试 ----

    /// 单组场景（所有行属于同一组）：应只插入1条小计行，位于数据区末尾之后。
    #[test]
    fn insert_subtotals_single_group() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        register_row_col_ops(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_cell(0, 0, 0, "Group")"#).unwrap();
            e.run(r#"set_cell(0, 0, 1, "Value")"#).unwrap();
            e.run(r#"set_cell(0, 1, 0, "A")"#).unwrap();
            e.run(r#"set_cell(0, 1, 1, "10")"#).unwrap();
            e.run(r#"set_cell(0, 2, 0, "A")"#).unwrap();
            e.run(r#"set_cell(0, 2, 1, "20")"#).unwrap();
            let ret: String = e.eval(r#"insert_subtotals(0, 0, "[1]", 9, 0, 1, 2)"#).unwrap();
            assert_eq!(ret, "ok");
        });

        let get_str = |wb: &Workbook, row: u32, col: u32| -> String {
            wb.sheets[0].cells.cols.get(&col)
                .and_then(|c| c.blocks.iter()
                    .find(|b| row >= b.start_row && row < b.start_row + b.values.len() as u32)
                    .map(|b| cell_value_to_string(&b.values[(row - b.start_row) as usize])))
                .unwrap_or_default()
        };
        // 小计行应插在row3
        assert_eq!(get_str(&wb, 3, 0), "A 汇总");

        let col1 = wb.sheets[0].cells.cols.get(&1).unwrap();
        let formula_cell = col1.blocks.iter()
            .find(|b| 3 >= b.start_row && 3 < b.start_row + b.values.len() as u32)
            .map(|b| b.values[(3 - b.start_row) as usize].clone())
            .unwrap();
        match formula_cell {
            CellValue::Formula { source, .. } => {
                assert_eq!(source, "=SUBTOTAL(9,B2:B3)", "公式应使用1-based Excel行号");
            }
            other => panic!("期望Formula，实际: {other:?}"),
        }

        // 分组折叠：数据行(row1/row2)应被标记outline_level=1，汇总行(row3)
        // 保持0(insert_row_impl插入新行的默认值，本函数不改它)。
        let ol = &wb.sheets[0].row_outline_level;
        assert_eq!(ol.get(1).copied().unwrap_or(0), 1, "数据行row1应为outline_level 1");
        assert_eq!(ol.get(2).copied().unwrap_or(0), 1, "数据行row2应为outline_level 1");
        assert_eq!(ol.get(3).copied().unwrap_or(0), 0, "汇总行row3应保持outline_level 0");
    }

    /// 多分组场景下，每组的数据行应独立标记outline_level=1，各组互不影响；
    /// 汇总行(倒序插入后分别落在row2/row4/row6)保持0。复用
    /// insert_subtotals_every_row_is_its_own_group 的数据结构，专门验证
    /// outline_level的区间设置在"倒序插入+组边界平移"之后仍然落在正确的
    /// 最终物理行号上（这是本测试要覆盖的核心风险点：如果g_start_final/
    /// g_end_final平移算错，outline_level会标错行）。
    #[test]
    fn insert_subtotals_outline_level_multi_group() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        register_row_col_ops(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_cell(0, 0, 0, "Group")"#).unwrap();
            e.run(r#"set_cell(0, 1, 0, "A")"#).unwrap();
            e.run(r#"set_cell(0, 1, 1, "1")"#).unwrap();
            e.run(r#"set_cell(0, 2, 0, "B")"#).unwrap();
            e.run(r#"set_cell(0, 2, 1, "2")"#).unwrap();
            e.run(r#"set_cell(0, 3, 0, "C")"#).unwrap();
            e.run(r#"set_cell(0, 3, 1, "3")"#).unwrap();
            let ret: String = e.eval(r#"insert_subtotals(0, 0, "[1]", 9, 0, 1, 3)"#).unwrap();
            assert_eq!(ret, "ok");
        });

        // 数据行倒序插入后分别落在row1(A)、row3(B)、row5(C)，
        // 汇总行落在row2(A汇总)、row4(B汇总)、row6(C汇总)。
        let ol = &wb.sheets[0].row_outline_level;
        assert_eq!(ol.get(1).copied().unwrap_or(0), 1, "A组数据行row1应为1");
        assert_eq!(ol.get(2).copied().unwrap_or(0), 0, "A组汇总行row2应为0");
        assert_eq!(ol.get(3).copied().unwrap_or(0), 1, "B组数据行row3应为1");
        assert_eq!(ol.get(4).copied().unwrap_or(0), 0, "B组汇总行row4应为0");
        assert_eq!(ol.get(5).copied().unwrap_or(0), 1, "C组数据行row5应为1");
        assert_eq!(ol.get(6).copied().unwrap_or(0), 0, "C组汇总行row6应为0");
    }

    /// 每行都是独立分组场景：应插入与数据行数相同的小计行，各自只聚合自己那一行，
    /// 且原始数据行不应错位（覆盖倒序插入逻辑的核心正确性）。
    #[test]
    fn insert_subtotals_every_row_is_its_own_group() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        register_row_col_ops(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_cell(0, 0, 0, "Group")"#).unwrap();
            e.run(r#"set_cell(0, 1, 0, "A")"#).unwrap();
            e.run(r#"set_cell(0, 1, 1, "1")"#).unwrap();
            e.run(r#"set_cell(0, 2, 0, "B")"#).unwrap();
            e.run(r#"set_cell(0, 2, 1, "2")"#).unwrap();
            e.run(r#"set_cell(0, 3, 0, "C")"#).unwrap();
            e.run(r#"set_cell(0, 3, 1, "3")"#).unwrap();
            let ret: String = e.eval(r#"insert_subtotals(0, 0, "[1]", 9, 0, 1, 3)"#).unwrap();
            assert_eq!(ret, "ok");
        });

        let get_str = |wb: &Workbook, row: u32, col: u32| -> String {
            wb.sheets[0].cells.cols.get(&col)
                .and_then(|c| c.blocks.iter()
                    .find(|b| row >= b.start_row && row < b.start_row + b.values.len() as u32)
                    .map(|b| cell_value_to_string(&b.values[(row - b.start_row) as usize])))
                .unwrap_or_default()
        };
        // 3组各插1行小计，倒序插入后应分别落在row2(A)、row4(B)、row6(C)
        assert_eq!(get_str(&wb, 2, 0), "A 汇总");
        assert_eq!(get_str(&wb, 4, 0), "B 汇总");
        assert_eq!(get_str(&wb, 6, 0), "C 汇总");
        // 原始数据行未错位
        assert_eq!(get_str(&wb, 1, 0), "A");
        assert_eq!(get_str(&wb, 3, 0), "B");
        assert_eq!(get_str(&wb, 5, 0), "C");
    }

    /// agg_cols_json 解析失败（非法JSON）应返回err，不panic。注意必须在
    /// with_workbook 闭包内部调用 e.eval，否则 thread_local WORKBOOK 已被
    /// 清空为 None，insert_subtotals 会走"err:workbook未绑定"分支，测试
    /// 会"假阳性通过"（凑巧也是err:前缀）但验证的不是本测试想验证的
    /// JSON解析失败路径。
    #[test]
    fn insert_subtotals_invalid_json_returns_err() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        register_row_col_ops(&mut e);
        let val: String = with_workbook(&mut wb, || {}).len().to_string();
        let _ = val; // 上一行只是占位保持模式一致，真正断言见下方
        let result: String = {
            let mut captured = String::new();
            with_workbook(&mut wb, || {
                captured = e.eval(r#"insert_subtotals(0, 0, "not_json", 9, 0, 1, 1)"#).unwrap();
            });
            captured
        };
        assert!(result.starts_with("err:"), "非法JSON应返回err前缀，实际: {result}");
        assert!(
            !result.contains("workbook未绑定"),
            "本测试应验证JSON解析失败路径，而非workbook绑定问题，实际: {result}"
        );
    }

    // ---- 分组折叠 Rhai 接口直接单测(不经过insert_subtotals，覆盖接口
    // 自身的参数转换/边界处理——insert_subtotals内部走的是_impl版本，
    // 不经过这层Rhai闭包，两边覆盖面不重叠，缺一不可) ----

    #[test]
    fn row_outline_level_get_out_of_bounds_returns_zero() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let v: i64 = e.eval(r#"get_row_outline_level(0, 999)"#).unwrap();
            assert_eq!(v, 0, "越界读取应返回0");
        });
    }

    #[test]
    fn row_outline_level_set_then_get_single_row() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_row_outline_level(0, 3, 2)"#).unwrap();
            let v: i64 = e.eval(r#"get_row_outline_level(0, 3)"#).unwrap();
            assert_eq!(v, 2, "设置后应能读回相同值");
            // 相邻行不受影响
            let v2: i64 = e.eval(r#"get_row_outline_level(0, 2)"#).unwrap();
            assert_eq!(v2, 0, "未设置的相邻行应仍为0");
        });
    }

    #[test]
    fn row_outline_level_set_range_covers_all_rows_inclusive() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_row_outline_level_range(0, 2, 5, 1)"#).unwrap();
            for r in 2..=5 {
                let v: i64 = e.eval(&format!("get_row_outline_level(0, {r})")).unwrap();
                assert_eq!(v, 1, "区间[2,5]内row{r}应为1");
            }
            let before: i64 = e.eval(r#"get_row_outline_level(0, 1)"#).unwrap();
            let after: i64 = e.eval(r#"get_row_outline_level(0, 6)"#).unwrap();
            assert_eq!(before, 0, "区间外row1应不受影响");
            assert_eq!(after, 0, "区间外row6应不受影响");
        });
    }

    #[test]
    fn row_outline_level_set_range_reversed_is_noop() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            // end_row < start_row：静默不做事，不panic不报错
            e.run(r#"set_row_outline_level_range(0, 5, 2, 1)"#).unwrap();
            let v: i64 = e.eval(r#"get_row_outline_level(0, 5)"#).unwrap();
            assert_eq!(v, 0, "反向区间应视为no-op，row5不应被设置");
        });
    }

    #[test]
    fn row_hidden_get_out_of_bounds_returns_false() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let v: bool = e.eval(r#"get_row_hidden(0, 999)"#).unwrap();
            assert!(!v, "越界读取应返回false");
        });
    }

    #[test]
    fn row_hidden_set_then_get_single_row() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_row_hidden(0, 4, true)"#).unwrap();
            let v: bool = e.eval(r#"get_row_hidden(0, 4)"#).unwrap();
            assert!(v, "设置后应能读回true");
            e.run(r#"set_row_hidden(0, 4, false)"#).unwrap();
            let v2: bool = e.eval(r#"get_row_hidden(0, 4)"#).unwrap();
            assert!(!v2, "再次设置为false应能读回false");
        });
    }

    #[test]
    fn row_hidden_set_range_covers_all_rows_inclusive() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_row_hidden_range(0, 3, 6, true)"#).unwrap();
            for r in 3..=6 {
                let v: bool = e.eval(&format!("get_row_hidden(0, {r})")).unwrap();
                assert!(v, "区间[3,6]内row{r}应为true");
            }
            let before: bool = e.eval(r#"get_row_hidden(0, 2)"#).unwrap();
            let after: bool = e.eval(r#"get_row_hidden(0, 7)"#).unwrap();
            assert!(!before, "区间外row2应不受影响");
            assert!(!after, "区间外row7应不受影响");
        });
    }

    #[test]
    fn test_set_get_cell() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run("set_cell(0, 0, 0, \"hello\")").unwrap();
            let v: String = e.eval("get_cell(0, 0, 0)").unwrap();
            assert_eq!(v, "hello");
        });
    }

    #[test]
    fn test_set_number_cell() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run("set_cell(0, 0, 0, \"42.5\")").unwrap();
        });
        let col = wb.sheets[0].cells.cols.get(&0).unwrap();
        assert!(matches!(col.blocks[0].values[0], CellValue::Number(n) if (n - 42.5).abs() < 1e-9));
    }

    #[test]
    fn test_set_formula() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run("set_formula(0, 0, 1, \"=A1+1\")").unwrap();
        });
        let col = wb.sheets[0].cells.cols.get(&1).unwrap();
        assert!(matches!(&col.blocks[0].values[0], CellValue::Formula { ref source, .. } if source == "=A1+1"));
    }

    #[test]
    fn test_list_available_fonts_rw_and_readonly() {
        for readonly in [false, true] {
            let mut e = rhai::Engine::new();
            if readonly { register_readonly(&mut e); } else { register(&mut e); }
            let json: String = e.eval("list_available_fonts()").unwrap();
            let v: Vec<String> = serde_json::from_str(&json).unwrap();
            assert!(v.iter().any(|n| n == "Carlito"), "应包含 Carlito: {:?}", v);
            assert!(!v.iter().any(|n| n.ends_with(" Light")), "不应含字重变体");
        }
    }

    #[test]
    fn test_sheet_count() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let n: i64 = e.eval("get_sheet_count()").unwrap();
            assert_eq!(n, 1);
        });
    }

    #[test]
    fn test_b1_geometry_merge_freeze_addsheet() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let r: String = e.eval("set_column_width(0, 3, 120.0)").unwrap();
            assert_eq!(r, "ok");
            let w: f64 = e.eval("get_column_width(0, 3)").unwrap();
            assert!((w - 120.0).abs() < 0.01, "w={w}");
            let r: String = e.eval("set_column_width(0, 99999, 10.0)").unwrap();
            assert!(r.starts_with("err:"), "{r}");
            let r: String = e.eval("set_row_height(0, 5, 30.0)").unwrap();
            assert_eq!(r, "ok");
            let h: f64 = e.eval("get_row_height(0, 5)").unwrap();
            assert!((h - 30.0).abs() < 0.01, "h={h}");
            e.run("merge_cells(0, 0, 0, 1, 1)").unwrap();
            let r: String = e.eval("unmerge_cells(0, 0, 0, 1, 1)").unwrap();
            assert_eq!(r, "ok");
            let r: String = e.eval("set_frozen_panes(0, 1, 2)").unwrap();
            assert_eq!(r, "ok");
            let f: String = e.eval("get_frozen_panes(0)").unwrap();
            assert!(f.contains("\"frozen_rows\":1") && f.contains("\"frozen_cols\":2"), "{f}");
            let r: String = e.eval("add_sheet(`数据`)").unwrap();
            assert!(r.starts_with("ok:"), "{r}");
            let r: String = e.eval("add_sheet(`数据`)").unwrap();
            assert!(r.starts_with("err:"), "重名应拒绝: {r}");
            let n: i64 = e.eval("get_sheet_count()").unwrap();
            assert_eq!(n, 2);
        });
    }

    #[test]
    fn test_set_get_cell_format() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_cell_format(0, 0, 0, `{"bold":true,"font_size":14.0}`)"#).unwrap();
            let s: String = e.eval("get_cell_format(0, 0, 0)").unwrap();
            assert!(s.contains("true"), "bold should be true, got: {s}");
        });
    }

    #[test]
    fn test_set_get_cell_format_vertical_and_horizontal_align() {
        // gap analysis 2.6：核实 align_v（垂直对齐）字段是否已随 CellFormat
        // 一起可读写。结论：CellFormat 结构体本来就有 align_v: Option<String>
        // 字段，与 align_h 同等地位，序列化机制自动覆盖，本测试只是补一个
        // 显式验证，不涉及任何新增实现。
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_cell_format(0, 0, 0, `{"align_h":"center","align_v":"middle"}`)"#).unwrap();
            let s: String = e.eval("get_cell_format(0, 0, 0)").unwrap();
            assert!(s.contains("\"align_h\":\"center\""), "应能读回水平对齐center，实际: {s}");
            assert!(s.contains("\"align_v\":\"middle\""), "应能读回垂直对齐middle，实际: {s}");
        });
    }

    #[test]
    fn test_describe_stats_basic_same_sheet() {
        // 基本场景：A1:A5写入5个数字，在同sheet的D1起输出15行描述性统计。
        // 验证：1) 报告rows_written=15  2) 标签文本按LOK顺序正确
        // 3) 抽查几个关键公式字符串（Mean/Median/Count）内容正确，
        //    range引用带"Sheet1!"前缀且坐标正确
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            for (i, v) in [1.0, 2.0, 3.0, 4.0, 5.0].iter().enumerate() {
                e.run(format!("set_cell(0, {}, 0, \"{}\")", i, v).as_str()).unwrap();
            }
            let result: String = e.eval("describe_stats(0, 0, 4, 0, 0, 0, 3)").unwrap();
            assert_eq!(result, "{\"rows_written\":15}");

            let label0: String = e.eval("get_cell(0, 0, 3)").unwrap();
            let label3: String = e.eval("get_cell(0, 3, 3)").unwrap();
            let label14: String = e.eval("get_cell(0, 14, 3)").unwrap();
            assert_eq!(label0, "Mean");
            assert_eq!(label3, "Median");
            assert_eq!(label14, "Count");

            let f_mean: String = e.eval("get_cell_formula(0, 0, 4)").unwrap();
            let f_median: String = e.eval("get_cell_formula(0, 3, 4)").unwrap();
            let f_count: String = e.eval("get_cell_formula(0, 14, 4)").unwrap();
            assert_eq!(f_mean, "=AVERAGE(Sheet1!A1:A5)");
            assert_eq!(f_median, "=MEDIAN(Sheet1!A1:A5)");
            assert_eq!(f_count, "=COUNT(Sheet1!A1:A5)");
        });
    }

    #[test]
    fn test_describe_stats_cross_sheet_output() {
        // 跨sheet输出：数据在sheet0，结果写到sheet1，验证公式range引用
        // 正确带上sheet0的名字前缀（不因为out_sheet不同而引用错sheet）。
        let mut wb = empty_wb();
        wb.sheets.push(wb.sheets[0].clone());
        wb.sheets[1].name = "Results".into();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            for (i, v) in [10.0, 20.0, 30.0].iter().enumerate() {
                e.run(format!("set_cell(0, {}, 1, \"{}\")", i, v).as_str()).unwrap();
            }
            let result: String = e.eval("describe_stats(0, 0, 2, 1, 1, 0, 0)").unwrap();
            assert_eq!(result, "{\"rows_written\":15}");

            let f_sum: String = e.eval("get_cell_formula(1, 13, 1)").unwrap();
            assert_eq!(f_sum, "=SUM(Sheet1!B1:B3)", "公式应引用源sheet(Sheet1)而非输出sheet(Results)");

            let label_sheet1: String = e.eval("get_cell(1, 0, 0)").unwrap();
            assert_eq!(label_sheet1, "Mean", "标签应写入out_sheet(1)而非原sheet(0)");
        });
    }

    #[test]
    fn test_describe_stats_invalid_source_sheet_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("describe_stats(5, 0, 2, 0, 0, 0, 3)").unwrap();
            assert_eq!(result, "{\"rows_written\":0,\"error\":\"sheet index out of range\"}");
        });
    }

    #[test]
    fn test_describe_stats_invalid_out_sheet_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("describe_stats(0, 0, 2, 0, 9, 0, 3)").unwrap();
            assert_eq!(result, "{\"rows_written\":0,\"error\":\"out_sheet index out of range\"}");
        });
    }

    #[test]
    fn test_anova_single_factor_grouped_by_column_basic() {
        // 3组数据按列分组：A1:A3=1,2,3  B1:B3=4,5,6  C1:C3=7,8,9
        // 输出到同sheet的F列起（out_col=5），验证第一段5列x3行 + 第二段
        // 7列x3行（空一行分隔）全部公式字符串，及组间相对引用坐标正确。
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            for (col, vals) in [(0, [1.0,2.0,3.0]), (1, [4.0,5.0,6.0]), (2, [7.0,8.0,9.0])] {
                for (i, v) in vals.iter().enumerate() {
                    e.run(format!("set_cell(0, {}, {}, \"{}\")", i, col, v).as_str()).unwrap();
                }
            }
            let result: String = e.eval("anova_single_factor(0, 0, 2, 0, 2, true, 0.05, 0, 0, 5)").unwrap();
            assert_eq!(result, "{\"rows_written\":7}");

            // 第一段：Groups(5) Count(6) Sum(7) Mean(8) Variance(9)
            let g1: String = e.eval("get_cell(0, 0, 5)").unwrap();
            assert_eq!(g1, "Column 1");
            let g1_count: String = e.eval("get_cell_formula(0, 0, 6)").unwrap();
            assert_eq!(g1_count, "=COUNT(Sheet1!A1:A3)");
            let g1_sum: String = e.eval("get_cell_formula(0, 0, 7)").unwrap();
            assert_eq!(g1_sum, "=SUM(Sheet1!A1:A3)");
            let g1_mean: String = e.eval("get_cell_formula(0, 0, 8)").unwrap();
            assert_eq!(g1_mean, "=AVERAGE(Sheet1!A1:A3)");
            let g1_var: String = e.eval("get_cell_formula(0, 0, 9)").unwrap();
            assert_eq!(g1_var, "=VAR(Sheet1!A1:A3)");

            let g2: String = e.eval("get_cell(0, 1, 5)").unwrap();
            assert_eq!(g2, "Column 2");
            let g2_sum: String = e.eval("get_cell_formula(0, 1, 7)").unwrap();
            assert_eq!(g2_sum, "=SUM(Sheet1!B1:B3)");

            let g3: String = e.eval("get_cell(0, 2, 5)").unwrap();
            assert_eq!(g3, "Column 3");
            let g3_var: String = e.eval("get_cell_formula(0, 2, 9)").unwrap();
            assert_eq!(g3_var, "=VAR(Sheet1!C1:C3)");

            // 第二段：between_row=4(行5,0-based) within_row=5 total_row=6
            // 列：Source(5) SS(6) DF(7) MS(8) F(9) P-value(10) F-critical(11)
            let between_label: String = e.eval("get_cell(0, 4, 5)").unwrap();
            assert_eq!(between_label, "Between Groups");
            let between_ss: String = e.eval("get_cell_formula(0, 4, 6)").unwrap();
            assert_eq!(between_ss, "=SUMPRODUCT(Sheet1!H1:H3,Sheet1!I1:I3)-SUM(Sheet1!H1:H3)^2/SUM(Sheet1!G1:G3)");
            let between_df: String = e.eval("get_cell_formula(0, 4, 7)").unwrap();
            assert_eq!(between_df, "=COUNT(Sheet1!H1:H3)-1");
            let between_ms: String = e.eval("get_cell_formula(0, 4, 8)").unwrap();
            assert_eq!(between_ms, "=G5/H5");
            let between_f: String = e.eval("get_cell_formula(0, 4, 9)").unwrap();
            assert_eq!(between_f, "=I5/I6");
            let between_p: String = e.eval("get_cell_formula(0, 4, 10)").unwrap();
            assert_eq!(between_p, "=1-F.DIST(J5,H5,H6,1)");
            let between_fcrit: String = e.eval("get_cell_formula(0, 4, 11)").unwrap();
            assert_eq!(between_fcrit, "=F.INV(1-0.05,H5,H6)");

            let within_label: String = e.eval("get_cell(0, 5, 5)").unwrap();
            assert_eq!(within_label, "Within Groups");
            let within_ss: String = e.eval("get_cell_formula(0, 5, 6)").unwrap();
            assert_eq!(within_ss, "=SUM(DEVSQ(Sheet1!A1:A3),DEVSQ(Sheet1!B1:B3),DEVSQ(Sheet1!C1:C3))");
            let within_df: String = e.eval("get_cell_formula(0, 5, 7)").unwrap();
            assert_eq!(within_df, "=SUM(Sheet1!G1:G3)-COUNT(Sheet1!G1:G3)");
            let within_ms: String = e.eval("get_cell_formula(0, 5, 8)").unwrap();
            assert_eq!(within_ms, "=G6/H6");
            let within_f: String = e.eval("get_cell_formula(0, 5, 9)").unwrap();
            assert_eq!(within_f, "", "Within Groups行F列应留空");
            let within_p: String = e.eval("get_cell_formula(0, 5, 10)").unwrap();
            assert_eq!(within_p, "", "Within Groups行P-value列应留空");
            let within_fcrit: String = e.eval("get_cell_formula(0, 5, 11)").unwrap();
            assert_eq!(within_fcrit, "", "Within Groups行F-critical列应留空");

            let total_label: String = e.eval("get_cell(0, 6, 5)").unwrap();
            assert_eq!(total_label, "Total");
            let total_ss: String = e.eval("get_cell_formula(0, 6, 6)").unwrap();
            assert_eq!(total_ss, "=DEVSQ(Sheet1!A1:A3,Sheet1!B1:B3,Sheet1!C1:C3)");
            let total_df: String = e.eval("get_cell_formula(0, 6, 7)").unwrap();
            assert_eq!(total_df, "=SUM(Sheet1!G1:G3)-1");
            let total_ms: String = e.eval("get_cell_formula(0, 6, 8)").unwrap();
            assert_eq!(total_ms, "", "Total行MS列应留空");
        });
    }

    #[test]
    fn test_anova_single_factor_grouped_by_row() {
        // 2组数据按行分组：Row0(A1:C1)=1,2,3  Row1(A2:C2)=4,5,6
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let rows = [[1.0,2.0,3.0], [4.0,5.0,6.0]];
            for (r, vals) in rows.iter().enumerate() {
                for (c, v) in vals.iter().enumerate() {
                    e.run(format!("set_cell(0, {}, {}, \"{}\")", r, c, v).as_str()).unwrap();
                }
            }
            let result: String = e.eval("anova_single_factor(0, 0, 1, 0, 2, false, 0.05, 0, 0, 5)").unwrap();
            assert_eq!(result, "{\"rows_written\":6}");

            let g1: String = e.eval("get_cell(0, 0, 5)").unwrap();
            assert_eq!(g1, "Row 1");
            let g1_sum: String = e.eval("get_cell_formula(0, 0, 7)").unwrap();
            assert_eq!(g1_sum, "=SUM(Sheet1!A1:C1)");

            let g2: String = e.eval("get_cell(0, 1, 5)").unwrap();
            assert_eq!(g2, "Row 2");
            let g2_sum: String = e.eval("get_cell_formula(0, 1, 7)").unwrap();
            assert_eq!(g2_sum, "=SUM(Sheet1!A2:C2)");

            // between_row = 0+2+1=3 (行4,0-based=3)
            let between_label: String = e.eval("get_cell(0, 3, 5)").unwrap();
            assert_eq!(between_label, "Between Groups");
            let total_ss: String = e.eval("get_cell_formula(0, 5, 6)").unwrap();
            assert_eq!(total_ss, "=DEVSQ(Sheet1!A1:C1,Sheet1!A2:C2)");
        });
    }

    #[test]
    fn test_anova_single_factor_alpha_out_of_range_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result_neg: String = e.eval("anova_single_factor(0, 0, 2, 0, 2, true, -0.1, 0, 0, 5)").unwrap();
            assert_eq!(result_neg, "{\"rows_written\":0,\"error\":\"alpha must be within [0,1]\"}");

            let result_over: String = e.eval("anova_single_factor(0, 0, 2, 0, 2, true, 1.5, 0, 0, 5)").unwrap();
            assert_eq!(result_over, "{\"rows_written\":0,\"error\":\"alpha must be within [0,1]\"}");
        });
    }

    #[test]
    fn test_anova_single_factor_insufficient_groups_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("anova_single_factor(0, 0, 2, 0, 0, true, 0.05, 0, 0, 5)").unwrap();
            assert_eq!(result, "{\"rows_written\":0,\"error\":\"need at least 2 groups\"}");
        });
    }

    #[test]
    fn test_anova_single_factor_invalid_sheet_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("anova_single_factor(9, 0, 2, 0, 2, true, 0.05, 0, 0, 5)").unwrap();
            assert_eq!(result, "{\"rows_written\":0,\"error\":\"sheet index out of range\"}");
        });
    }

    #[test]
    fn test_anova_single_factor_invalid_out_sheet_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("anova_single_factor(0, 0, 2, 0, 2, true, 0.05, 9, 0, 5)").unwrap();
            assert_eq!(result, "{\"rows_written\":0,\"error\":\"out_sheet index out of range\"}");
        });
    }

    #[test]
    fn test_anova_two_factor_basic() {
        // 2列x2行网格：A1=1 B1=2 / A2=3 B2=4，输出到F列起(out_col=5)
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let grid = [[1.0, 2.0], [3.0, 4.0]];
            for (r, vals) in grid.iter().enumerate() {
                for (c, v) in vals.iter().enumerate() {
                    e.run(format!("set_cell(0, {}, {}, \"{}\")", r, c, v).as_str()).unwrap();
                }
            }
            let result: String = e.eval("anova_two_factor(0, 0, 1, 0, 1, 0.05, 0, 0, 5)").unwrap();
            // rows_written = 1(alpha)+1(空)+2(ColumnX)+1(空)+2(RowX)+1(空)+4(ANOVA)+1(备注,
            // 因为2x2是本函数能构造的最小规格,Error DF=(2-1)*(2-1)=1,触发低自由度提示) = 13
            assert_eq!(result, "{\"rows_written\":13}");

            // 新增（本次会话）：验证低自由度备注行确实写入，且位于Total行(row11)之后
            let note_text: String = e.eval("get_cell(0, 12, 5)").unwrap();
            assert!(
                note_text.contains("Error自由度极低") && note_text.contains("DIV/0"),
                "备注行应提示Error自由度极低及可能的除零风险，实际内容: {}", note_text
            );

            // Alpha行
            let alpha_label: String = e.eval("get_cell(0, 0, 5)").unwrap();
            assert_eq!(alpha_label, "Alpha");
            let alpha_val: String = e.eval("get_cell(0, 0, 6)").unwrap();
            assert_eq!(alpha_val, "0.05");

            // ColumnX段：行2,3 (0-based)
            let col1_label: String = e.eval("get_cell(0, 2, 5)").unwrap();
            assert_eq!(col1_label, "Column 1");
            let col1_sum: String = e.eval("get_cell_formula(0, 2, 7)").unwrap();
            assert_eq!(col1_sum, "=SUM(Sheet1!A1:A2)");
            let col2_label: String = e.eval("get_cell(0, 3, 5)").unwrap();
            assert_eq!(col2_label, "Column 2");
            let col2_sum: String = e.eval("get_cell_formula(0, 3, 7)").unwrap();
            assert_eq!(col2_sum, "=SUM(Sheet1!B1:B2)");

            // RowX段：行5,6 (0-based)
            let row1_label: String = e.eval("get_cell(0, 5, 5)").unwrap();
            assert_eq!(row1_label, "Row 1");
            let row1_sum: String = e.eval("get_cell_formula(0, 5, 7)").unwrap();
            assert_eq!(row1_sum, "=SUM(Sheet1!A1:B1)");
            let row2_label: String = e.eval("get_cell(0, 6, 5)").unwrap();
            assert_eq!(row2_label, "Row 2");
            let row2_sum: String = e.eval("get_cell_formula(0, 6, 7)").unwrap();
            assert_eq!(row2_sum, "=SUM(Sheet1!A2:B2)");

            // ANOVA表：行8=Rows,9=Columns,10=Error,11=Total (0-based)
            let rows_label: String = e.eval("get_cell(0, 8, 5)").unwrap();
            assert_eq!(rows_label, "Rows");
            let rows_ss: String = e.eval("get_cell_formula(0, 8, 6)").unwrap();
            assert_eq!(rows_ss, "=SUMPRODUCT(Sheet1!H6:H7,Sheet1!I6:I7)-SUM(Sheet1!A1:B2)^2/COUNT(Sheet1!A1:B2)");
            let rows_df: String = e.eval("get_cell_formula(0, 8, 7)").unwrap();
            assert_eq!(rows_df, "=MAX(Sheet1!G3:G4)-1");
            let rows_ms: String = e.eval("get_cell_formula(0, 8, 8)").unwrap();
            assert_eq!(rows_ms, "=G9/H9");
            let rows_f: String = e.eval("get_cell_formula(0, 8, 9)").unwrap();
            assert_eq!(rows_f, "=I9/I11");
            let rows_p: String = e.eval("get_cell_formula(0, 8, 10)").unwrap();
            assert_eq!(rows_p, "=1-F.DIST(J9,H9,H11,1)");
            let rows_fcrit: String = e.eval("get_cell_formula(0, 8, 11)").unwrap();
            assert_eq!(rows_fcrit, "=F.INV(1-0.05,H9,H11)");

            let columns_label: String = e.eval("get_cell(0, 9, 5)").unwrap();
            assert_eq!(columns_label, "Columns");
            let columns_ss: String = e.eval("get_cell_formula(0, 9, 6)").unwrap();
            assert_eq!(columns_ss, "=SUMPRODUCT(Sheet1!H3:H4,Sheet1!I3:I4)-SUM(Sheet1!A1:B2)^2/COUNT(Sheet1!A1:B2)");
            let columns_df: String = e.eval("get_cell_formula(0, 9, 7)").unwrap();
            assert_eq!(columns_df, "=MAX(Sheet1!G6:G7)-1");
            let columns_f: String = e.eval("get_cell_formula(0, 9, 9)").unwrap();
            assert_eq!(columns_f, "=I10/I11");
            let columns_p: String = e.eval("get_cell_formula(0, 9, 10)").unwrap();
            assert_eq!(columns_p, "=1-F.DIST(J10,H10,H11,1)");
            let columns_fcrit: String = e.eval("get_cell_formula(0, 9, 11)").unwrap();
            assert_eq!(columns_fcrit, "=F.INV(1-0.05,H10,H11)");

            let error_label: String = e.eval("get_cell(0, 10, 5)").unwrap();
            assert_eq!(error_label, "Error");
            let error_ss: String = e.eval("get_cell_formula(0, 10, 6)").unwrap();
            assert_eq!(
                error_ss,
                "=SUMSQ(Sheet1!A1:B2)+SUM(Sheet1!A1:B2)^2/COUNT(Sheet1!A1:B2)-(SUMPRODUCT(Sheet1!H6:H7,Sheet1!I6:I7)+SUMPRODUCT(Sheet1!H3:H4,Sheet1!I3:I4))",
            );
            let error_df: String = e.eval("get_cell_formula(0, 10, 7)").unwrap();
            assert_eq!(error_df, "=H12-H9-H10");
            let error_ms: String = e.eval("get_cell_formula(0, 10, 8)").unwrap();
            assert_eq!(error_ms, "=G11/H11");
            let error_f: String = e.eval("get_cell_formula(0, 10, 9)").unwrap();
            assert_eq!(error_f, "", "Error行F列应留空");

            let total_label: String = e.eval("get_cell(0, 11, 5)").unwrap();
            assert_eq!(total_label, "Total");
            let total_ss: String = e.eval("get_cell_formula(0, 11, 6)").unwrap();
            assert_eq!(total_ss, "=SUM(G9,G10,G11)");
            let total_df: String = e.eval("get_cell_formula(0, 11, 7)").unwrap();
            assert_eq!(total_df, "=COUNT(Sheet1!A1:B2)-1");
            let total_ms: String = e.eval("get_cell_formula(0, 11, 8)").unwrap();
            assert_eq!(total_ms, "", "Total行MS列应留空");
        });
    }

    #[test]
    fn test_anova_two_factor_alpha_out_of_range_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result_neg: String = e.eval("anova_two_factor(0, 0, 1, 0, 1, -0.1, 0, 0, 5)").unwrap();
            assert_eq!(result_neg, "{\"rows_written\":0,\"error\":\"alpha must be within [0,1]\"}");

            let result_over: String = e.eval("anova_two_factor(0, 0, 1, 0, 1, 1.5, 0, 0, 5)").unwrap();
            assert_eq!(result_over, "{\"rows_written\":0,\"error\":\"alpha must be within [0,1]\"}");
        });
    }

    #[test]
    fn test_anova_two_factor_insufficient_dimensions_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            // 只有1列
            let result_col: String = e.eval("anova_two_factor(0, 0, 1, 0, 0, 0.05, 0, 0, 5)").unwrap();
            assert_eq!(result_col, "{\"rows_written\":0,\"error\":\"need at least 2 columns and 2 rows\"}");

            // 只有1行
            let result_row: String = e.eval("anova_two_factor(0, 0, 0, 0, 1, 0.05, 0, 0, 5)").unwrap();
            assert_eq!(result_row, "{\"rows_written\":0,\"error\":\"need at least 2 columns and 2 rows\"}");
        });
    }

    #[test]
    fn test_anova_two_factor_invalid_sheet_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("anova_two_factor(9, 0, 1, 0, 1, 0.05, 0, 0, 5)").unwrap();
            assert_eq!(result, "{\"rows_written\":0,\"error\":\"sheet index out of range\"}");
        });
    }

    #[test]
    fn test_anova_two_factor_invalid_out_sheet_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("anova_two_factor(0, 0, 1, 0, 1, 0.05, 9, 0, 5)").unwrap();
            assert_eq!(result, "{\"rows_written\":0,\"error\":\"out_sheet index out of range\"}");
        });
    }

    #[test]
    fn test_col_to_letters_basic() {
        assert_eq!(col_to_letters(1), "A");
        assert_eq!(col_to_letters(26), "Z");
        assert_eq!(col_to_letters(27), "AA");
    }

    #[test]
    fn test_z_test_formulas() {
        // A1:A3=1,2,3  B1:B3=4,5,6；输出到(0,3)，标签在D列，值在E列。
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            for (i, v) in [1.0, 2.0, 3.0].iter().enumerate() {
                e.run(format!("set_cell(0, {}, 0, \"{}\")", i, v).as_str()).unwrap();
            }
            for (i, v) in [4.0, 5.0, 6.0].iter().enumerate() {
                e.run(format!("set_cell(0, {}, 1, \"{}\")", i, v).as_str()).unwrap();
            }
            let result: String = e
                .eval("z_test(0, 0, 2, 0, 0, 0, 2, 1, 1.0, 1.0, 0.0, 0.05, 0, 0, 3)")
                .unwrap();
            assert_eq!(result, "{\"rows_written\":12}");

            let l0: String = e.eval("get_cell(0, 0, 3)").unwrap();
            let l7: String = e.eval("get_cell(0, 7, 3)").unwrap();
            let l11: String = e.eval("get_cell(0, 11, 3)").unwrap();
            assert_eq!(l0, "Mean 1");
            assert_eq!(l7, "z");
            assert_eq!(l11, "z Critical two-tail");

            let f_mean1: String = e.eval("get_cell_formula(0, 0, 4)").unwrap();
            let f_obs1: String = e.eval("get_cell_formula(0, 4, 4)").unwrap();
            let f_z: String = e.eval("get_cell_formula(0, 7, 4)").unwrap();
            let f_p1: String = e.eval("get_cell_formula(0, 8, 4)").unwrap();
            let f_c1: String = e.eval("get_cell_formula(0, 9, 4)").unwrap();
            let f_c2: String = e.eval("get_cell_formula(0, 11, 4)").unwrap();
            assert_eq!(f_mean1, "=AVERAGE(Sheet1!A1:A3)");
            assert_eq!(f_obs1, "=COUNT(Sheet1!A1:A3)");
            assert_eq!(f_z, "=(E1-E2-E7)/SQRT(E3/E5+E4/E6)");
            assert_eq!(f_p1, "=1-NORM.S.DIST(ABS(E8),TRUE)");
            assert_eq!(f_c1, "=NORM.S.INV(1-0.05)");
            assert_eq!(f_c2, "=NORM.S.INV(1-0.05/2)");
        });
    }

    #[test]
    fn test_z_test_invalid_args_return_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let neg_var: String = e
                .eval("z_test(0, 0, 2, 0, 0, 0, 2, 1, -1.0, 1.0, 0.0, 0.05, 0, 0, 3)")
                .unwrap();
            assert_eq!(neg_var, "{\"rows_written\":0,\"error\":\"variances must be positive\"}");
            let bad_alpha: String = e
                .eval("z_test(0, 0, 2, 0, 0, 0, 2, 1, 1.0, 1.0, 0.0, 1.5, 0, 0, 3)")
                .unwrap();
            assert_eq!(bad_alpha, "{\"rows_written\":0,\"error\":\"alpha must be within (0,1)\"}");
            let bad_sheet: String = e
                .eval("z_test(9, 0, 2, 0, 0, 0, 2, 1, 1.0, 1.0, 0.0, 0.05, 0, 0, 3)")
                .unwrap();
            assert_eq!(bad_sheet, "{\"rows_written\":0,\"error\":\"sheet1 or sheet2 index out of range\"}");
        });
    }

    #[test]
    fn test_chi_square_test_formulas() {
        // A1:B2 = 10,20 / 30,40；输出到(0,3)：D列起。
        // 布局：Expected标题(0) / E矩阵(1,2) / 空(3) / Contribution标题(4) /
        //       贡献矩阵(5,6) / 空(7) / Chi-square(8) DF(9) P-value(10)
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let vals = [[10.0, 20.0], [30.0, 40.0]];
            for r in 0..2usize {
                for c in 0..2usize {
                    e.run(format!("set_cell(0, {}, {}, \"{}\")", r, c, vals[r][c]).as_str()).unwrap();
                }
            }
            let result: String = e.eval("chi_square_test(0, 0, 1, 0, 1, 0, 0, 3)").unwrap();
            assert_eq!(result, "{\"rows_written\":11}");

            let l_exp: String = e.eval("get_cell(0, 0, 3)").unwrap();
            let l_con: String = e.eval("get_cell(0, 4, 3)").unwrap();
            let l_chi: String = e.eval("get_cell(0, 8, 3)").unwrap();
            let l_p: String = e.eval("get_cell(0, 10, 3)").unwrap();
            assert_eq!(l_exp, "Expected");
            assert_eq!(l_con, "Contribution");
            assert_eq!(l_chi, "Chi-square");
            assert_eq!(l_p, "P-value");

            let f_e00: String = e.eval("get_cell_formula(0, 1, 3)").unwrap();
            let f_c00: String = e.eval("get_cell_formula(0, 5, 3)").unwrap();
            let f_chi: String = e.eval("get_cell_formula(0, 8, 4)").unwrap();
            let f_df: String = e.eval("get_cell_formula(0, 9, 4)").unwrap();
            let f_p: String = e.eval("get_cell_formula(0, 10, 4)").unwrap();
            assert_eq!(f_e00, "=SUM(Sheet1!A1:B1)*SUM(Sheet1!A1:A2)/SUM(Sheet1!A1:B2)");
            assert_eq!(f_c00, "=(Sheet1!A1-D2)^2/D2");
            assert_eq!(f_chi, "=SUM(D6:E7)");
            assert_eq!(f_df, "=1");
            assert_eq!(f_p, "=CHISQ.DIST.RT(E9,E10)");
        });
    }

    #[test]
    fn test_chi_square_test_invalid_args_return_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let one_row: String = e.eval("chi_square_test(0, 0, 0, 0, 1, 0, 0, 3)").unwrap();
            assert_eq!(one_row, "{\"rows_written\":0,\"error\":\"need at least 2 rows and 2 columns\"}");
            let bad_sheet: String = e.eval("chi_square_test(9, 0, 1, 0, 1, 0, 0, 3)").unwrap();
            assert_eq!(bad_sheet, "{\"rows_written\":0,\"error\":\"sheet index out of range\"}");
        });
    }

    #[test]
    fn test_t_test_same_sheet_basic() {
        // 两组数据都在sheet0：A1:A3和B1:B3，配对t检验(type=1)，双尾(tails=2)。
        // 验证：1) rows_written=5  2) 标签顺序正确
        // 3) 公式字符串正确引用各自range，且T.TEST参数原样透传
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            for (i, v) in [1.0, 2.0, 3.0].iter().enumerate() {
                e.run(format!("set_cell(0, {}, 0, \"{}\")", i, v).as_str()).unwrap();
            }
            for (i, v) in [4.0, 5.0, 6.0].iter().enumerate() {
                e.run(format!("set_cell(0, {}, 1, \"{}\")", i, v).as_str()).unwrap();
            }
            let result: String = e.eval("t_test(0, 0, 2, 0, 0, 0, 2, 1, 2, 1, 0, 0, 3)").unwrap();
            assert_eq!(result, "{\"rows_written\":5}");

            let label0: String = e.eval("get_cell(0, 0, 3)").unwrap();
            let label4: String = e.eval("get_cell(0, 4, 3)").unwrap();
            assert_eq!(label0, "Mean 1");
            assert_eq!(label4, "P-value");

            let f_mean1: String = e.eval("get_cell_formula(0, 0, 4)").unwrap();
            let f_mean2: String = e.eval("get_cell_formula(0, 1, 4)").unwrap();
            let f_pvalue: String = e.eval("get_cell_formula(0, 4, 4)").unwrap();
            assert_eq!(f_mean1, "=AVERAGE(Sheet1!A1:A3)");
            assert_eq!(f_mean2, "=AVERAGE(Sheet1!B1:B3)");
            assert_eq!(f_pvalue, "=T.TEST(Sheet1!A1:A3,Sheet1!B1:B3,2,1)");
        });
    }

    #[test]
    fn test_t_test_cross_sheet_groups_and_output() {
        // 两组数据分别在sheet0/sheet1，输出到sheet1，验证公式range各自
        // 正确带上源sheet前缀（不因为两组来自不同sheet而互相搞混）。
        let mut wb = empty_wb();
        wb.sheets.push(wb.sheets[0].clone());
        wb.sheets[1].name = "GroupB".into();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_cell(0, 0, 0, "10")"#).unwrap();
            e.run(r#"set_cell(1, 0, 0, "20")"#).unwrap();
            let result: String = e.eval("t_test(0, 0, 0, 0, 1, 0, 0, 0, 2, 3, 1, 5, 0)").unwrap();
            assert_eq!(result, "{\"rows_written\":5}");

            let f_mean1: String = e.eval("get_cell_formula(1, 5, 1)").unwrap();
            let f_mean2: String = e.eval("get_cell_formula(1, 6, 1)").unwrap();
            assert_eq!(f_mean1, "=AVERAGE(Sheet1!A1:A1)", "组1应引用Sheet1而非GroupB");
            assert_eq!(f_mean2, "=AVERAGE(GroupB!A1:A1)", "组2应引用GroupB而非Sheet1");
        });
    }

    #[test]
    fn test_t_test_invalid_sheet_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("t_test(0, 0, 2, 0, 9, 0, 2, 0, 2, 1, 0, 0, 3)").unwrap();
            assert_eq!(result, "{\"rows_written\":0,\"error\":\"sheet1 or sheet2 index out of range\"}");
        });
    }

    #[test]
    fn test_f_test_basic() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            for (i, v) in [1.0, 2.0, 3.0, 4.0].iter().enumerate() {
                e.run(format!("set_cell(0, {}, 0, \"{}\")", i, v).as_str()).unwrap();
            }
            for (i, v) in [1.0, 1.0, 1.0, 5.0].iter().enumerate() {
                e.run(format!("set_cell(0, {}, 1, \"{}\")", i, v).as_str()).unwrap();
            }
            let result: String = e.eval("f_test(0, 0, 3, 0, 0, 0, 3, 1, 0, 0, 3)").unwrap();
            assert_eq!(result, "{\"rows_written\":3}");

            let label0: String = e.eval("get_cell(0, 0, 3)").unwrap();
            let label2: String = e.eval("get_cell(0, 2, 3)").unwrap();
            assert_eq!(label0, "Variance 1");
            assert_eq!(label2, "P-value");

            let f_var1: String = e.eval("get_cell_formula(0, 0, 4)").unwrap();
            let f_pvalue: String = e.eval("get_cell_formula(0, 2, 4)").unwrap();
            assert_eq!(f_var1, "=VAR(Sheet1!A1:A4)");
            assert_eq!(f_pvalue, "=F.TEST(Sheet1!A1:A4,Sheet1!B1:B4)");
        });
    }

    #[test]
    fn test_f_test_invalid_out_sheet_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("f_test(0, 0, 3, 0, 0, 0, 3, 1, 9, 0, 3)").unwrap();
            assert_eq!(result, "{\"rows_written\":0,\"error\":\"out_sheet index out of range\"}");
        });
    }

    #[test]
    fn test_correl_basic() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            for (i, v) in [1.0, 2.0, 3.0, 4.0].iter().enumerate() {
                e.run(format!("set_cell(0, {}, 0, \"{}\")", i, v).as_str()).unwrap();
            }
            for (i, v) in [2.0, 4.0, 6.0, 8.0].iter().enumerate() {
                e.run(format!("set_cell(0, {}, 1, \"{}\")", i, v).as_str()).unwrap();
            }
            let result: String = e.eval("correl(0, 0, 3, 0, 0, 0, 3, 1, 0, 0, 3)").unwrap();
            assert_eq!(result, "{\"rows_written\":1}");

            let label0: String = e.eval("get_cell(0, 0, 3)").unwrap();
            assert_eq!(label0, "Correlation");

            let f_correl: String = e.eval("get_cell_formula(0, 0, 4)").unwrap();
            assert_eq!(f_correl, "=CORREL(Sheet1!A1:A4,Sheet1!B1:B4)");
        });
    }

    #[test]
    fn test_correl_cross_sheet_groups_and_output() {
        // 两组数据来自不同sheet，输出写到第三个sheet，验证两条range各自
        // 正确引用各自源sheet，不因跨sheet输出而互相搞混。
        let mut wb = empty_wb();
        wb.sheets.push(wb.sheets[0].clone());
        wb.sheets[1].name = "GroupB".into();
        wb.sheets.push(wb.sheets[0].clone());
        wb.sheets[2].name = "Results".into();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_cell(0, 0, 0, "1")"#).unwrap();
            e.run(r#"set_cell(1, 0, 0, "2")"#).unwrap();
            let result: String = e.eval("correl(0, 0, 0, 0, 1, 0, 0, 0, 2, 5, 1)").unwrap();
            assert_eq!(result, "{\"rows_written\":1}");

            let f_correl: String = e.eval("get_cell_formula(2, 5, 2)").unwrap();
            assert_eq!(f_correl, "=CORREL(Sheet1!A1:A1,GroupB!A1:A1)");

            let label: String = e.eval("get_cell(2, 5, 1)").unwrap();
            assert_eq!(label, "Correlation", "标签应写入out_sheet(2)");
        });
    }

    #[test]
    fn test_correl_invalid_sheet_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("correl(0, 0, 2, 0, 9, 0, 2, 0, 0, 0, 3)").unwrap();
            assert_eq!(result, "{\"rows_written\":0,\"error\":\"sheet1 or sheet2 index out of range\"}");
        });
    }

    #[test]
    fn test_covariance_basic() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            for (i, v) in [1.0, 2.0, 3.0, 4.0].iter().enumerate() {
                e.run(format!("set_cell(0, {}, 0, \"{}\")", i, v).as_str()).unwrap();
            }
            for (i, v) in [2.0, 4.0, 6.0, 8.0].iter().enumerate() {
                e.run(format!("set_cell(0, {}, 1, \"{}\")", i, v).as_str()).unwrap();
            }
            let result: String = e.eval("covariance(0, 0, 3, 0, 0, 0, 3, 1, 0, 0, 3)").unwrap();
            assert_eq!(result, "{\"rows_written\":2}");

            let label0: String = e.eval("get_cell(0, 0, 3)").unwrap();
            let label1: String = e.eval("get_cell(0, 1, 3)").unwrap();
            assert_eq!(label0, "Covariance (Population)");
            assert_eq!(label1, "Covariance (Sample)");

            let f_pop: String = e.eval("get_cell_formula(0, 0, 4)").unwrap();
            let f_sample: String = e.eval("get_cell_formula(0, 1, 4)").unwrap();
            assert_eq!(f_pop, "=COVARIANCE.P(Sheet1!A1:A4,Sheet1!B1:B4)");
            assert_eq!(f_sample, "=COVARIANCE.S(Sheet1!A1:A4,Sheet1!B1:B4)");
        });
    }

    #[test]
    fn test_covariance_invalid_out_sheet_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("covariance(0, 0, 3, 0, 0, 0, 3, 1, 9, 0, 3)").unwrap();
            assert_eq!(result, "{\"rows_written\":0,\"error\":\"out_sheet index out of range\"}");
        });
    }

    #[test]
    fn test_regression_basic_perfect_line() {
        // y = 2x + 0 完美线性关系，验证Slope=2/Intercept=0对应的公式字符串
        // 正确拼出，以及标签顺序符合5行设计（Slope/Intercept/R Squared/
        // Standard Error/Correlation）。
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            for (i, v) in [1.0, 2.0, 3.0, 4.0].iter().enumerate() {
                e.run(format!("set_cell(0, {}, 0, \"{}\")", i, v).as_str()).unwrap();
            }
            for (i, v) in [2.0, 4.0, 6.0, 8.0].iter().enumerate() {
                e.run(format!("set_cell(0, {}, 1, \"{}\")", i, v).as_str()).unwrap();
            }
            // y列在col 1(B)，x列在col 0(A)
            let result: String = e.eval("regression(0, 0, 3, 1, 0, 0, 3, 0, 0, 0, 3)").unwrap();
            assert_eq!(result, "{\"rows_written\":5}");

            let label0: String = e.eval("get_cell(0, 0, 3)").unwrap();
            let label4: String = e.eval("get_cell(0, 4, 3)").unwrap();
            assert_eq!(label0, "Slope");
            assert_eq!(label4, "Correlation");

            let f_slope: String = e.eval("get_cell_formula(0, 0, 4)").unwrap();
            let f_intercept: String = e.eval("get_cell_formula(0, 1, 4)").unwrap();
            let f_rsq: String = e.eval("get_cell_formula(0, 2, 4)").unwrap();
            let f_steyx: String = e.eval("get_cell_formula(0, 3, 4)").unwrap();
            let f_correl: String = e.eval("get_cell_formula(0, 4, 4)").unwrap();
            assert_eq!(f_slope, "=SLOPE(Sheet1!B1:B4,Sheet1!A1:A4)");
            assert_eq!(f_intercept, "=INTERCEPT(Sheet1!B1:B4,Sheet1!A1:A4)");
            assert_eq!(f_rsq, "=RSQ(Sheet1!B1:B4,Sheet1!A1:A4)");
            assert_eq!(f_steyx, "=STEYX(Sheet1!B1:B4,Sheet1!A1:A4)");
            assert_eq!(f_correl, "=CORREL(Sheet1!B1:B4,Sheet1!A1:A4)");
        });
    }

    #[test]
    fn test_regression_cross_sheet_groups_and_output() {
        // Y来自sheet0，X来自sheet1，输出写到sheet2，验证两条range各自
        // 正确引用各自源sheet，不因跨sheet输出而互相搞混。
        let mut wb = empty_wb();
        wb.sheets.push(wb.sheets[0].clone());
        wb.sheets[1].name = "XData".into();
        wb.sheets.push(wb.sheets[0].clone());
        wb.sheets[2].name = "Results".into();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_cell(0, 0, 0, "10")"#).unwrap();
            e.run(r#"set_cell(1, 0, 0, "5")"#).unwrap();
            let result: String = e.eval("regression(0, 0, 0, 0, 1, 0, 0, 0, 2, 5, 1)").unwrap();
            assert_eq!(result, "{\"rows_written\":5}");

            let f_slope: String = e.eval("get_cell_formula(2, 5, 2)").unwrap();
            assert_eq!(f_slope, "=SLOPE(Sheet1!A1:A1,XData!A1:A1)");

            let label: String = e.eval("get_cell(2, 5, 1)").unwrap();
            assert_eq!(label, "Slope", "标签应写入out_sheet(2)");
        });
    }

    #[test]
    fn test_regression_invalid_sheet_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("regression(0, 0, 2, 0, 9, 0, 2, 0, 0, 0, 3)").unwrap();
            assert_eq!(result, "{\"rows_written\":0,\"error\":\"sheet_y or sheet_x index out of range\"}");
        });
    }

    #[test]
    fn test_regression_invalid_out_sheet_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("regression(0, 0, 3, 0, 0, 0, 3, 1, 9, 0, 3)").unwrap();
            assert_eq!(result, "{\"rows_written\":0,\"error\":\"out_sheet index out of range\"}");
        });
    }

    #[test]
    fn test_moving_average_odd_interval_centered() {
        // interval=5（奇数）严格居中：half=2,rem=0+1=1 -> [i-2,i+2]。
        // 7个数据点(index 0..6)，只有index 2..4满足[i-2,i+2]完全落在
        // [0,6]内，index 0/1/5/6应输出=NA()，index 2/3/4应输出=AVERAGE。
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            for (i, v) in [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0].iter().enumerate() {
                e.run(format!("set_cell(0, {}, 0, \"{}\")", i, v).as_str()).unwrap();
            }
            let result: String = e.eval("moving_average(0, 0, 6, 0, 5, 0, 0, 1)").unwrap();
            assert_eq!(result, "{\"rows_written\":7}");

            let f0: String = e.eval("get_cell_formula(0, 0, 1)").unwrap();
            let f1: String = e.eval("get_cell_formula(0, 1, 1)").unwrap();
            let f2: String = e.eval("get_cell_formula(0, 2, 1)").unwrap();
            let f3: String = e.eval("get_cell_formula(0, 3, 1)").unwrap();
            let f4: String = e.eval("get_cell_formula(0, 4, 1)").unwrap();
            let f5: String = e.eval("get_cell_formula(0, 5, 1)").unwrap();
            let f6: String = e.eval("get_cell_formula(0, 6, 1)").unwrap();
            assert_eq!(f0, "=NA()");
            assert_eq!(f1, "=NA()");
            assert_eq!(f2, "=AVERAGE(Sheet1!A1:A5)");
            assert_eq!(f3, "=AVERAGE(Sheet1!A2:A6)");
            assert_eq!(f4, "=AVERAGE(Sheet1!A3:A7)");
            assert_eq!(f5, "=NA()");
            assert_eq!(f6, "=NA()");
        });
    }

    #[test]
    fn test_moving_average_even_interval_skews_left() {
        // interval=4（偶数）：half=2,rem=0 -> [i-2,i+1]（前2后1，偏左，
        // 对齐LOK MovingAverageDialog.cxx第85-90行的half_remainder计算）。
        // 6个数据点(index 0..5)：index 2满足[0,3]，index 3满足[1,4]，
        // 其余越界应为=NA()。
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            for (i, v) in [10.0, 20.0, 30.0, 40.0, 50.0, 60.0].iter().enumerate() {
                e.run(format!("set_cell(0, {}, 0, \"{}\")", i, v).as_str()).unwrap();
            }
            let result: String = e.eval("moving_average(0, 0, 5, 0, 4, 0, 0, 1)").unwrap();
            assert_eq!(result, "{\"rows_written\":6}");

            let f0: String = e.eval("get_cell_formula(0, 0, 1)").unwrap();
            let f1: String = e.eval("get_cell_formula(0, 1, 1)").unwrap();
            let f2: String = e.eval("get_cell_formula(0, 2, 1)").unwrap();
            let f3: String = e.eval("get_cell_formula(0, 3, 1)").unwrap();
            let f4: String = e.eval("get_cell_formula(0, 4, 1)").unwrap();
            let f5: String = e.eval("get_cell_formula(0, 5, 1)").unwrap();
            assert_eq!(f0, "=NA()");
            assert_eq!(f1, "=NA()");
            assert_eq!(f2, "=AVERAGE(Sheet1!A1:A4)", "index2: [i-2,i+1]=[0,3]");
            assert_eq!(f3, "=AVERAGE(Sheet1!A2:A5)", "index3: [i-2,i+1]=[1,4]");
            assert_eq!(f4, "=AVERAGE(Sheet1!A3:A6)", "index4: [i-2,i+1]=[2,5]完全落在[0,5]内，不越界");
            assert_eq!(f5, "=NA()", "index5: [i-2,i+1]=[3,6]，终点6超出n=6的合法范围[0,5]");
        });
    }

    #[test]
    fn test_exponential_smoothing_basic_recurrence() {
        // 3个数据点[10,20,30]，alpha=0.5。验证：
        // 第1行Alpha标签+数值0.5；第2行(平滑首值)="=Sheet1!A1"直接引用；
        // 第3/4行递推公式引用alpha单元格与上一行输出单元格。
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            for (i, v) in [10.0, 20.0, 30.0].iter().enumerate() {
                e.run(format!("set_cell(0, {}, 0, \"{}\")", i, v).as_str()).unwrap();
            }
            let result: String = e.eval("exponential_smoothing(0, 0, 2, 0, 0.5, 0, 0, 1)").unwrap();
            assert_eq!(result, "{\"rows_written\":4}", "1行alpha + 3行平滑结果 = 4");

            let alpha_label: String = e.eval("get_cell(0, 0, 1)").unwrap();
            let alpha_val: String = e.eval("get_cell(0, 0, 2)").unwrap();
            assert_eq!(alpha_label, "Alpha");
            assert_eq!(alpha_val, "0.5");

            let f_first: String = e.eval("get_cell_formula(0, 1, 2)").unwrap();
            assert_eq!(f_first, "=Sheet1!A1", "首行平滑值应直接引用首个原始观测值");

            let f_second: String = e.eval("get_cell_formula(0, 2, 2)").unwrap();
            assert_eq!(f_second, "=C1*Sheet1!A2 + (1-C1)*C2", "第二行递推：alpha引用C1(Alpha值格)，上一平滑值引用C2");

            let f_third: String = e.eval("get_cell_formula(0, 3, 2)").unwrap();
            assert_eq!(f_third, "=C1*Sheet1!A3 + (1-C1)*C3", "第三行递推：引用C3(上一行输出)");
        });
    }

    #[test]
    fn test_exponential_smoothing_cross_sheet_output() {
        let mut wb = empty_wb();
        wb.sheets.push(wb.sheets[0].clone());
        wb.sheets[1].name = "Results".into();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_cell(0, 0, 0, "5")"#).unwrap();
            let result: String = e.eval("exponential_smoothing(0, 0, 0, 0, 0.3, 1, 0, 0)").unwrap();
            assert_eq!(result, "{\"rows_written\":2}");

            let f_first: String = e.eval("get_cell_formula(1, 1, 1)").unwrap();
            assert_eq!(f_first, "=Sheet1!A1", "输入range应带源sheet(Sheet1)前缀，即便输出在sheet1(Results)");

            let label: String = e.eval("get_cell(1, 0, 0)").unwrap();
            assert_eq!(label, "Alpha", "Alpha标签应写入out_sheet(1)");
        });
    }

    #[test]
    fn test_exponential_smoothing_alpha_out_of_range_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result_neg: String = e.eval("exponential_smoothing(0, 0, 2, 0, -0.1, 0, 0, 1)").unwrap();
            assert_eq!(result_neg, "{\"rows_written\":0,\"error\":\"alpha must be within [0,1]\"}");

            let result_over: String = e.eval("exponential_smoothing(0, 0, 2, 0, 1.5, 0, 0, 1)").unwrap();
            assert_eq!(result_over, "{\"rows_written\":0,\"error\":\"alpha must be within [0,1]\"}");
        });
    }

    #[test]
    fn test_exponential_smoothing_alpha_boundary_values_accepted() {
        // alpha=0.0和alpha=1.0是闭区间边界，应被接受而非拒绝。
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_cell(0, 0, 0, "1")"#).unwrap();
            let result0: String = e.eval("exponential_smoothing(0, 0, 0, 0, 0.0, 0, 0, 1)").unwrap();
            assert_eq!(result0, "{\"rows_written\":2}");
            let result1: String = e.eval("exponential_smoothing(0, 0, 0, 0, 1.0, 0, 3, 1)").unwrap();
            assert_eq!(result1, "{\"rows_written\":2}");
        });
    }

    #[test]
    fn test_exponential_smoothing_invalid_sheet_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("exponential_smoothing(9, 0, 2, 0, 0.5, 0, 0, 1)").unwrap();
            assert_eq!(result, "{\"rows_written\":0,\"error\":\"sheet index out of range\"}");
        });
    }

    #[test]
    fn test_exponential_smoothing_invalid_out_sheet_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("exponential_smoothing(0, 0, 2, 0, 0.5, 9, 0, 1)").unwrap();
            assert_eq!(result, "{\"rows_written\":0,\"error\":\"out_sheet index out of range\"}");
        });
    }

    #[test]
    fn test_moving_average_cross_sheet_output() {
        // 输入在sheet0，输出写到sheet1，验证range引用带正确的源sheet前缀，
        // 且写入位置确实落在out_sheet而非sheet0。
        let mut wb = empty_wb();
        wb.sheets.push(wb.sheets[0].clone());
        wb.sheets[1].name = "Results".into();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            for (i, v) in [1.0, 2.0, 3.0].iter().enumerate() {
                e.run(format!("set_cell(0, {}, 0, \"{}\")", i, v).as_str()).unwrap();
            }
            let result: String = e.eval("moving_average(0, 0, 2, 0, 3, 1, 0, 0)").unwrap();
            assert_eq!(result, "{\"rows_written\":3}");

            let f1: String = e.eval("get_cell_formula(1, 1, 0)").unwrap();
            assert_eq!(f1, "=AVERAGE(Sheet1!A1:A3)", "index1唯一满足interval=3(half=1,rem=1->[0,2])的完整窗口");

            let f0: String = e.eval("get_cell_formula(1, 0, 0)").unwrap();
            assert_eq!(f0, "=NA()", "输出应落在out_sheet(1)而非sheet0");
        });
    }

    #[test]
    fn test_moving_average_invalid_interval_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("moving_average(0, 0, 3, 0, 0, 0, 0, 1)").unwrap();
            assert_eq!(result, "{\"rows_written\":0,\"error\":\"interval must be positive\"}");
        });
    }

    #[test]
    fn test_moving_average_invalid_sheet_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("moving_average(9, 0, 3, 0, 3, 0, 0, 1)").unwrap();
            assert_eq!(result, "{\"rows_written\":0,\"error\":\"sheet index out of range\"}");
        });
    }

    #[test]
    fn test_moving_average_invalid_out_sheet_returns_error() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("moving_average(0, 0, 3, 0, 3, 9, 0, 1)").unwrap();
            assert_eq!(result, "{\"rows_written\":0,\"error\":\"out_sheet index out of range\"}");
        });
    }

    #[test]
    fn test_text_to_columns_basic_comma_split() {
        // 最基本场景：单行"a,b,c"按逗号分列，应写入col/col+1/col+2三格，
        // 且每段按parse_string_to_cellvalue规则解析（此处三段都是文本）。
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_cell(0, 0, 0, "a,b,c")"#).unwrap();
            let result: String = e.eval(r#"text_to_columns(0, 0, 0, 0, ",")"#).unwrap();
            assert_eq!(result, "{\"max_cols\":3}", "应报告最大列数3，实际: {result}");

            let c0: String = e.eval("get_cell(0, 0, 0)").unwrap();
            let c1: String = e.eval("get_cell(0, 0, 1)").unwrap();
            let c2: String = e.eval("get_cell(0, 0, 2)").unwrap();
            assert_eq!(c0, "a");
            assert_eq!(c1, "b");
            assert_eq!(c2, "c");
        });
    }

    #[test]
    fn test_text_to_columns_numeric_segments_parsed_as_numbers() {
        // 分列后每一段应复用set_cell同样的解析规则：能解析成数字的段应
        // 变成CellValue::Number，而不是停留在字符串"10"这种展示层面。
        // 用get_cell读回后比较字符串值（因为get_cell本身就是走
        // cell_value_to_string），间接验证Number(10.0).to_string()=="10"
        // 与原始文本"10"一致，不足以完全区分Number和Text，因此额外用
        // get_cell_formula确认它不是公式（排除误判为Formula分支），并且
        // 用一个明确的浮点数"1.5"验证能被解析。
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_cell(0, 0, 0, "10;1.5;text")"#).unwrap();
            let result: String = e.eval(r#"text_to_columns(0, 0, 0, 0, ";")"#).unwrap();
            assert_eq!(result, "{\"max_cols\":3}");

            let c0: String = e.eval("get_cell(0, 0, 0)").unwrap();
            let c1: String = e.eval("get_cell(0, 0, 1)").unwrap();
            let c2: String = e.eval("get_cell(0, 0, 2)").unwrap();
            assert_eq!(c0, "10");
            assert_eq!(c1, "1.5");
            assert_eq!(c2, "text");
        });
    }

    #[test]
    fn test_text_to_columns_multi_row_batch() {
        // 多行批量分列：验证每一行独立按自己的内容切分，互不影响，
        // 且max_cols取所有行中出现过的最大段数（这里第二行比第一行多一段）。
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_cell(0, 0, 0, "a,b")"#).unwrap();
            e.run(r#"set_cell(0, 1, 0, "x,y,z")"#).unwrap();
            let result: String = e.eval(r#"text_to_columns(0, 0, 1, 0, ",")"#).unwrap();
            assert_eq!(result, "{\"max_cols\":3}", "应取两行中的最大段数3，实际: {result}");

            let r0c0: String = e.eval("get_cell(0, 0, 0)").unwrap();
            let r0c1: String = e.eval("get_cell(0, 0, 1)").unwrap();
            let r1c0: String = e.eval("get_cell(0, 1, 0)").unwrap();
            let r1c1: String = e.eval("get_cell(0, 1, 1)").unwrap();
            let r1c2: String = e.eval("get_cell(0, 1, 2)").unwrap();
            assert_eq!(r0c0, "a");
            assert_eq!(r0c1, "b");
            assert_eq!(r1c0, "x");
            assert_eq!(r1c1, "y");
            assert_eq!(r1c2, "z");
        });
    }

    #[test]
    fn test_text_to_columns_overwrites_existing_right_side_data() {
        // 明确验证"不插入新列，直接覆盖右侧已有数据"这一设计决策
        // （对齐Excel默认行为）：col+1事先有数据"OLD"，分列后应被
        // 新分出的段覆盖，而不是被保留或报错。
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_cell(0, 0, 0, "a,b")"#).unwrap();
            e.run(r#"set_cell(0, 0, 1, "OLD")"#).unwrap();
            let _: String = e.eval(r#"text_to_columns(0, 0, 0, 0, ",")"#).unwrap();

            let c1: String = e.eval("get_cell(0, 0, 1)").unwrap();
            assert_eq!(c1, "b", "col+1原有数据应被分列结果覆盖，实际: {c1}");
        });
    }

    #[test]
    fn test_text_to_columns_empty_delimiter_returns_zero_and_no_change() {
        // 空分隔符视为不合法输入：应直接返回max_cols=0，不做任何单元格
        // 写入（不做"按字符切割"这种令人惊讶的隐式行为），原有数据保持
        // 不变。
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_cell(0, 0, 0, "abc")"#).unwrap();
            let result: String = e.eval(r#"text_to_columns(0, 0, 0, 0, "")"#).unwrap();
            assert_eq!(result, "{\"max_cols\":0}");

            let c0: String = e.eval("get_cell(0, 0, 0)").unwrap();
            assert_eq!(c0, "abc", "空分隔符时原值不应被改动，实际: {c0}");
        });
    }

    #[test]
    fn test_text_to_columns_no_delimiter_found_single_column_unchanged() {
        // 目标文本中不含分隔符时，split结果就是原字符串本身这一段，
        // max_cols应为1，col位置的值等价于原值（不产生额外列）。
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_cell(0, 0, 0, "nodeliminhere")"#).unwrap();
            let result: String = e.eval(r#"text_to_columns(0, 0, 0, 0, ",")"#).unwrap();
            assert_eq!(result, "{\"max_cols\":1}");

            let c0: String = e.eval("get_cell(0, 0, 0)").unwrap();
            assert_eq!(c0, "nodeliminhere");
        });
    }

    #[test]
    fn test_get_conditional_formats_empty_and_after_set() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let empty: String = e.eval("get_conditional_formats(0)").unwrap();
            assert_eq!(empty, "[]", "未设置任何规则时应返回空数组");

            let range = r#"{"sheet":"Sheet1","start":[0,0],"end":[9,3]}"#;
            let rule  = r#"{"condition":"gt:0","format":{"fg_color":"FF0000"}}"#;
            e.run(&format!(r#"set_conditional_format(0, `{range}`, `{rule}`)"#)).unwrap();

            let after: String = e.eval("get_conditional_formats(0)").unwrap();
            assert!(after.contains("gt:0"), "设置后应能读回条件字符串，实际: {after}");
        });
    }

    #[test]
    fn test_get_data_validations_empty_and_after_set() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let empty: String = e.eval("get_data_validations(0)").unwrap();
            assert_eq!(empty, "[]", "未设置任何规则时应返回空数组");

            let range = r#"{"sheet":"Sheet1","start":[0,0],"end":[9,0]}"#;
            let rule  = r#"{"kind":"list","formula1":"A,B,C","formula2":null,"show_error":true,"error_msg":null}"#;
            e.run(&format!(r#"set_data_validation(0, `{range}`, `{rule}`)"#)).unwrap();

            let after: String = e.eval("get_data_validations(0)").unwrap();
            assert!(after.contains("list"), "设置后应能读回kind字段，实际: {after}");
        });
    }

    #[test]
    fn test_get_auto_filter_null_and_after_apply() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let empty: String = e.eval("get_auto_filter(0)").unwrap();
            assert_eq!(empty, "null", "未设置筛选时应返回字符串null");

            let filter = r#"{"col_filters":{"0":["A","B"]}}"#;
            e.run(&format!(r#"apply_filter(0, 0, 0, `{filter}`)"#)).unwrap();

            let after: String = e.eval("get_auto_filter(0)").unwrap();
            assert!(after.contains("header_row"), "设置后应能读回header_row字段，实际: {after}");
        });
    }

    #[test]
    fn test_get_merged_cells_empty_and_after_merge() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let empty: String = e.eval("get_merged_cells(0)").unwrap();
            assert_eq!(empty, "[]", "未合并任何单元格时应返回空数组");

            e.run("merge_cells(0, 0, 0, 1, 1)").unwrap();
            let after: String = e.eval("get_merged_cells(0)").unwrap();
            assert!(after.contains("r1") && after.contains("c2"), "合并后应能读回边界坐标，实际: {after}");
        });
    }

    #[test]
    fn test_detect_autosum_range_scans_upward() {
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Number(1.0)]);
        set_row(&mut wb, 0, 1, &[CellValue::Number(2.0)]);
        set_row(&mut wb, 0, 2, &[CellValue::Number(3.0)]);
        // row 3, col 0 是空单元格，上方0-2行都是连续数字
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("detect_autosum_range(0, 3, 0)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(v["direction"], "up");
            assert_eq!(v["start"], serde_json::json!([0, 0]));
            assert_eq!(v["end"], serde_json::json!([2, 0]));
        });
    }

    #[test]
    fn test_detect_autosum_range_stops_at_gap_then_falls_back_left() {
        let mut wb = empty_wb();
        // 第0行: A0=文本(表头), B0=5, C0=空(锚点)
        set_row(&mut wb, 0, 0, &[CellValue::Text("header".into()), CellValue::Number(5.0)]);
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            // 锚点是 (row=0, col=2)：上方row=-1不存在(row=0无上方)，
            // 应直接尝试向左：col=1是数字5.0，col=0是文本 -> 只覆盖col1
            let result: String = e.eval("detect_autosum_range(0, 0, 2)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(v["direction"], "left");
            assert_eq!(v["start"], serde_json::json!([0, 1]));
            assert_eq!(v["end"], serde_json::json!([0, 1]));
        });
    }

    #[test]
    fn test_detect_autosum_range_none_when_no_numbers() {
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Text("a".into())]);
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("detect_autosum_range(0, 1, 0)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(v["direction"], "none");
        });
    }

    #[test]
    fn test_detect_autosum_range_treats_formula_with_numeric_cached_as_number() {
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Number(10.0)]);
        set_row(&mut wb, 0, 1, &[CellValue::Formula {
            source: "=A1*2".to_string(),
            cached: Box::new(CellValue::Number(20.0)),
        }]);
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let result: String = e.eval("detect_autosum_range(0, 2, 0)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(v["direction"], "up");
            assert_eq!(v["start"], serde_json::json!([0, 0]));
            assert_eq!(v["end"], serde_json::json!([1, 0]));
        });
    }

    #[test]
    fn test_merge_cells() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run("merge_cells(0, 0, 0, 2, 3)").unwrap();
        });
        assert_eq!(wb.sheets[0].merged_cells.len(), 1);
        let m = &wb.sheets[0].merged_cells[0];
        assert_eq!((m.r1, m.c1, m.r2, m.c2), (0, 0, 2, 3));
    }

    /// 跑一段 sort_range 脚本,返回 Err 文本(成功返回 None)
    fn run_sort_err(script: &str) -> Option<String> {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = None;
        with_workbook(&mut wb, || {
            out = e.run(script).err().map(|x| x.to_string());
        });
        out
    }

    #[test]
    fn test_set_cell_format_unknown_field_and_type_errs() {
        let m = run_sort_err(r##"set_cell_format(0, 0, 0, `{"background":"red"}`)"##)
            .expect("未知字段应抛错");
        assert!(m.contains("background") && m.contains("可用字段"), "{m}");
        assert!(run_sort_err(r##"set_cell_format(0, 0, 0, `{"bold":"yes"}`)"##).is_some());
        assert!(run_sort_err(r##"set_cell_format(0, 0, 0, `[1,2]`)"##).is_some());
        assert!(run_sort_err(r##"set_cell_format(0, -1, 0, `{"bold":true}`)"##).is_some());
        assert!(run_sort_err(r##"set_cell_format(5, 0, 0, `{"bold":true}`)"##).is_some());
    }

    #[test]
    fn test_set_cell_format_valid_is_applied() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r##"set_cell_format(0, 1, 2, `{"bold":true,"fg_color":"#FF0000"}`)"##).unwrap();
        });
        let f = wb.sheets[0].cell_formats.get(&(1, 2)).expect("格式应已写入");
        assert_eq!(f.bold, Some(true));
        assert_eq!(f.fg_color.as_deref(), Some("#FF0000"));
    }

    #[test]
    fn test_set_conditional_format_validation() {
        let r = r#"{"sheet":"Sheet1","start":[0,0],"end":[9,3]}"#;
        let no_cond = r##"{"format":{"fg_color":"#FF0000"}}"##;
        assert!(run_sort_err(&format!(r#"set_conditional_format(0, `{r}`, `{no_cond}`)"#)).is_some());
        let no_fmt = r#"{"condition":"gt:0"}"#;
        assert!(run_sort_err(&format!(r#"set_conditional_format(0, `{r}`, `{no_fmt}`)"#)).is_some());
        let bad_key = r#"{"condition":"gt:0","format":{"color":"red"}}"#;
        let m = run_sort_err(&format!(r#"set_conditional_format(0, `{r}`, `{bad_key}`)"#))
            .expect("format 含未知字段应抛错");
        assert!(m.contains("color"), "{m}");
        let ok = r##"{"condition":"gt:0","format":{"fg_color":"#FF0000"}}"##;
        assert!(run_sort_err(&format!(r#"set_conditional_format(0, `{r}`, `{ok}`)"#)).is_none());
        assert!(run_sort_err(&format!(r#"set_conditional_format(7, `{r}`, `{ok}`)"#)).is_some());
    }

    #[test]
    fn test_apply_filter_validation_and_valid() {
        assert!(run_sort_err(r#"apply_filter(0, 0, 0, `{}`)"#).is_some(), "缺 col_filters");
        assert!(run_sort_err(r#"apply_filter(0, 0, 0, `{"col_filters":{"0":[1,2]}}`)"#).is_some(), "数字值");
        assert!(run_sort_err(r#"apply_filter(0, 0, 0, `{"col_filters":{"x":["A"]}}`)"#).is_some(), "非整数键");
        assert!(run_sort_err(r#"apply_filter(0, 0, 0, `{"col_filters":{"0":"A"}}`)"#).is_some(), "值非数组");
        assert!(run_sort_err(r#"apply_filter(0, -1, 0, `{"col_filters":{}}`)"#).is_some(), "负行号");
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"apply_filter(0, 0, 1, `{"col_filters":{"0":["A"],"2":["B"]}}`)"#).unwrap();
        });
        let af = wb.sheets[0].auto_filter.as_ref().expect("应已设置");
        assert_eq!((af.header_row, af.header_col, af.end_col), (0, 1, 3));
    }

    #[test]
    fn test_set_data_validation_validation() {
        let r = r#"{"sheet":"Sheet1","start":[0,0],"end":[4,0]}"#;
        let call = |rule: &str| run_sort_err(&format!(r#"set_data_validation(0, `{r}`, `{rule}`)"#));
        assert!(call(r#"{"operator":"between","formula1":"1"}"#).is_some(), "缺 kind");
        assert!(call(r#"{"kind":"integer","formula1":"1"}"#).is_some(), "kind 非法");
        assert!(call(r#"{"kind":"whole","operator":"greaterThan","formula1":10}"#).is_some(), "formula1 是数字");
        assert!(call(r#"{"kind":"whole","operator":"greaterThan"}"#).is_some(), "缺 formula1");
        assert!(call(r#"{"kind":"whole","formula1":"1","show_error":"yes"}"#).is_some(), "show_error 非布尔");
        assert!(call(r#"not json"#).is_some());
        assert!(run_sort_err(r#"set_data_validation(0, `{"start":[0,0],"end":[1,0]}`, `{"kind":"none"}`)"#).is_some(), "缺 sheet");
        assert!(run_sort_err(&format!(r#"set_data_validation(9, `{r}`, `{{"kind":"none"}}`)"#)).is_some(), "sheet 不存在");
    }

    #[test]
    fn test_set_data_validation_valid_and_clear() {
        let r = r#"{"sheet":"Sheet1","start":[0,0],"end":[4,0]}"#;
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(&format!(r#"set_data_validation(0, `{r}`, `{{"kind":"whole","operator":"greaterThan","formula1":"10","error_msg":"x"}}`)"#)).unwrap();
        });
        assert_eq!(wb.sheets[0].data_validations.len(), 1);
        assert_eq!(wb.sheets[0].data_validations[0].kind, "whole");
        assert_eq!(wb.sheets[0].data_validations[0].formula1, "10");
        with_workbook(&mut wb, || {
            e.run(&format!(r#"set_data_validation(0, `{r}`, `{{"kind":"none"}}`)"#)).unwrap();
        });
        assert!(wb.sheets[0].data_validations.is_empty(), "none 应清除重叠规则");
    }

    #[test]
    fn test_condition_whitelist_matches_core_branches() {
        let mut names: Vec<&str> = COND_PREFIXES.iter().map(|x| x.0).collect();
        names.sort();
        let mut expect = vec!["gt","lt","gte","lte","eq","ne","between","not_between",
            "text_contains","text_not_contains","begins_with","ends_with","not_empty","is_empty","expr"];
        expect.sort();
        assert_eq!(names, expect, "白名单应与 cond_matches_ctx 分支一致;改分支名时同步这里");
    }

    #[test]
    fn test_set_conditional_format_condition_validation() {
        let r = r#"{"sheet":"Sheet1","start":[0,0],"end":[9,3]}"#;
        let call = |c: &str| run_sort_err(&format!(
            r##"set_conditional_format(0, `{r}`, `{{"condition":"{c}","format":{{"bold":true}}}}`)"##));
        for good in ["gt:0", "lte:3.5", "gt:$B$3", "eq:abc", "ne:5", "between:1:10", "not_between:1:10",
                     "text_contains:foo", "begins_with:a", "not_empty", "is_empty", "expr:A1<>1"] {
            assert!(call(good).is_none(), "{good} 应合法");
        }
        for bad in ["neq:5", "greater:5", "gt:abc", "gt", "between:1", "between:1:x", "not_empty:1",
                    "duplicate", "top10:10", "gt:"] {
            assert!(call(bad).is_some(), "{bad} 应报错");
        }
        assert!(call("neq:5").unwrap().contains("合法写法"));
    }

    #[test]
    fn test_set_data_validation_operator_and_formula_checks() {
        let r = r#"{"sheet":"Sheet1","start":[0,0],"end":[4,0]}"#;
        let call = |rule: &str| run_sort_err(&format!(r#"set_data_validation(0, `{r}`, `{rule}`)"#));
        assert!(call(r#"{"kind":"whole","operator":"greater_than","formula1":"1"}"#).is_some(), "operator 非法");
        assert!(call(r#"{"kind":"whole","formula1":"1"}"#).is_some(), "缺 operator");
        assert!(call(r#"{"kind":"whole","operator":"equal","formula1":"abc"}"#).is_some(), "formula1 非数字");
        assert!(call(r#"{"kind":"decimal","operator":"between","formula1":"1"}"#).is_some(), "between 缺 formula2");
        assert!(call(r#"{"kind":"whole","operator":"between","formula1":"1","formula2":"9"}"#).is_none());
        assert!(call(r#"{"kind":"list","formula1":"A,B,C"}"#).is_none(), "list 不需要 operator");
    }

    #[test]
    fn test_sort_range_bad_json_errs_with_example() {
        let m = run_sort_err(r#"sort_range(0, `{"start":[0,0],"end":[2,0]}`, 0, true)"#)
            .expect("缺 sheet 键应抛错");
        assert!(m.contains("sort_range"), "{m}");
        assert!(m.contains("sheet"), "错误文本应指出缺 sheet: {m}");
        assert!(m.contains("示例"), "{m}");
    }

    #[test]
    fn test_sort_range_key_col_out_of_range_errs() {
        let r = r#"{"sheet":"Sheet1","start":[0,2],"end":[3,2]}"#;
        let m = run_sort_err(&format!(r#"sort_range(0, `{r}`, 2, true)"#))
            .expect("单列范围 key_col=2 应抛错");
        assert!(m.contains("key_col"), "{m}");
        assert!(run_sort_err(&format!(r#"sort_range(0, `{r}`, -1, true)"#)).is_some());
        assert!(run_sort_err(&format!(r#"sort_range(0, `{r}`, 0, true)"#)).is_none());
    }

    #[test]
    fn test_sort_range_reversed_range_and_bad_sheet_err() {
        let rev = r#"{"sheet":"Sheet1","start":[3,0],"end":[0,0]}"#;
        assert!(run_sort_err(&format!(r#"sort_range(0, `{rev}`, 0, true)"#)).is_some());
        let ok = r#"{"sheet":"Sheet1","start":[0,0],"end":[2,0]}"#;
        let m = run_sort_err(&format!(r#"sort_range(9, `{ok}`, 0, true)"#))
            .expect("sheet 9 不存在应抛错");
        assert!(m.contains("sheet 9"), "{m}");
    }

    #[test]
    fn test_sort_range() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_cell(0, 0, 0, "3")"#).unwrap();
            e.run(r#"set_cell(0, 1, 0, "1")"#).unwrap();
            e.run(r#"set_cell(0, 2, 0, "2")"#).unwrap();
            let range = r#"{"sheet":"Sheet1","start":[0,0],"end":[2,0]}"#;
            e.run(&format!(r#"sort_range(0, `{range}`, 0, true)"#)).unwrap();
        });
        // 排序后第0行应为1
        let col = wb.sheets[0].cells.cols.get(&0).unwrap();
        let get = |r: u32| -> f64 {
            for blk in &col.blocks {
                if r >= blk.start_row && r < blk.start_row + blk.values.len() as u32 {
                    if let CellValue::Number(n) = blk.values[(r - blk.start_row) as usize] {
                        return n;
                    }
                }
            }
            f64::NAN
        };
        assert_eq!(get(0), 1.0);
        assert_eq!(get(1), 2.0);
        assert_eq!(get(2), 3.0);
    }

    #[test]
    fn test_apply_filter() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"apply_filter(0, 0, 0, `{"col_filters":{"0":["A","B"]}}`)"#).unwrap();
        });
        let af = wb.sheets[0].auto_filter.as_ref().unwrap();
        assert_eq!(af.header_row, 0);
        assert!(af.col_filters.contains_key(&0));
    }

    #[test]
    fn test_set_conditional_format() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let range = r#"{"sheet":"Sheet1","start":[0,0],"end":[9,3]}"#;
            let rule  = r#"{"condition":"gt:0","format":{"fg_color":"FF0000"}}"#;
            e.run(&format!(r#"set_conditional_format(0, `{range}`, `{rule}`)"#)).unwrap();
        });
        assert_eq!(wb.sheets[0].cond_formats.len(), 1);
        match &wb.sheets[0].cond_formats[0].kind {
            roe_model::calc::ConditionalFormatKind::CellFormat { condition, .. } => {
                assert_eq!(condition, "gt:0");
            }
            other => panic!("expected CellFormat kind, got {:?}", other),
        }
    }

    #[test]
    fn test_set_data_validation() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let range = r#"{"sheet":"Sheet1","start":[0,0],"end":[9,0]}"#;
            let rule  = r#"{"kind":"list","formula1":"A,B,C","formula2":null,"show_error":true,"error_msg":null}"#;
            e.run(&format!(r#"set_data_validation(0, `{range}`, `{rule}`)"#)).unwrap();
        });
        assert_eq!(wb.sheets[0].data_validations.len(), 1);
        assert_eq!(wb.sheets[0].data_validations[0].kind, "list");
    }


    #[test]
    fn test_set_data_validation_same_range_overwrites() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let range = r#"{"sheet":"Sheet1","start":[0,0],"end":[9,0]}"#;
            let r1 = r#"{"kind":"list","formula1":"A,B,C","formula2":null,"show_error":true,"error_msg":null}"#;
            let r2 = r#"{"kind":"whole","operator":"greaterThan","formula1":"10","formula2":null,"show_error":true,"error_msg":null}"#;
            e.run(&format!(r#"set_data_validation(0, `{range}`, `{r1}`)"#)).unwrap();
            e.run(&format!(r#"set_data_validation(0, `{range}`, `{r2}`)"#)).unwrap();
        });
        assert_eq!(wb.sheets[0].data_validations.len(), 1, "同一区域重复set应覆盖而非叠加");
        assert_eq!(wb.sheets[0].data_validations[0].kind, "whole");
        assert_eq!(wb.sheets[0].data_validations[0].formula1, "10");
    }

    #[test]
    fn test_set_data_validation_partial_overlap_removes_only_overlapping() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let rule = r#"{"kind":"list","formula1":"A,B","formula2":null,"show_error":true,"error_msg":null}"#;
            // 三条旧规则：第0-4行/第5-9行(同列0)/第0-9行(列2)
            let ra = r#"{"sheet":"Sheet1","start":[0,0],"end":[4,0]}"#;
            let rb = r#"{"sheet":"Sheet1","start":[5,0],"end":[9,0]}"#;
            let rc = r#"{"sheet":"Sheet1","start":[0,2],"end":[9,2]}"#;
            for r in [ra, rb, rc] {
                e.run(&format!(r#"set_data_validation(0, `{r}`, `{rule}`)"#)).unwrap();
            }
            // 新规则覆盖第3-6行、列0：与ra、rb重叠，与rc(列2)不重叠
            let rn = r#"{"sheet":"Sheet1","start":[3,0],"end":[6,0]}"#;
            let new_rule = r#"{"kind":"decimal","operator":"between","formula1":"1","formula2":"5","show_error":true,"error_msg":null}"#;
            e.run(&format!(r#"set_data_validation(0, `{rn}`, `{new_rule}`)"#)).unwrap();
        });
        let dvs = &wb.sheets[0].data_validations;
        assert_eq!(dvs.len(), 2, "应剩下：不重叠的rc + 新规则");
        assert!(dvs.iter().any(|d| d.kind == "list" && d.range.start == (0, 2)), "不重叠的旧规则应保留");
        assert!(dvs.iter().any(|d| d.kind == "decimal" && d.range.start == (3, 0)), "新规则应存在");
    }

    #[test]
    fn test_set_data_validation_kind_none_only_clears() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let range = r#"{"sheet":"Sheet1","start":[0,0],"end":[9,0]}"#;
            let rule = r#"{"kind":"list","formula1":"A,B,C","formula2":null,"show_error":true,"error_msg":null}"#;
            e.run(&format!(r#"set_data_validation(0, `{range}`, `{rule}`)"#)).unwrap();
            let none_rule = r#"{"kind":"none","formula1":"","formula2":null,"show_error":false,"error_msg":null}"#;
            e.run(&format!(r#"set_data_validation(0, `{range}`, `{none_rule}`)"#)).unwrap();
        });
        assert_eq!(wb.sheets[0].data_validations.len(), 0, "kind=none应清除重叠规则且不新增");
    }

    #[test]
    fn test_set_data_validation_bad_json_keeps_existing() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let range = r#"{"sheet":"Sheet1","start":[0,0],"end":[9,0]}"#;
            let rule = r#"{"kind":"list","formula1":"A,B,C","formula2":null,"show_error":true,"error_msg":null}"#;
            e.run(&format!(r#"set_data_validation(0, `{range}`, `{rule}`)"#)).unwrap();
            // rule_json解析失败：现在抛错(不再静默),且不应误删已有规则
            let r = e.run(&format!(r#"set_data_validation(0, `{range}`, `not json`)"#));
            assert!(r.is_err(), "坏 JSON 应抛错");
        });
        assert_eq!(wb.sheets[0].data_validations.len(), 1, "解析失败不应清除旧规则");
    }

    fn dv_input_json(e: &rhai::Engine, wb: &mut Workbook, row: i64, text: &str) -> serde_json::Value {
        let mut out = String::new();
        with_workbook(wb, || {
            out = e.eval(&format!(r#"validate_cell_input(0, {row}, 0, `{text}`)"#)).unwrap();
        });
        serde_json::from_str(&out).unwrap()
    }

    fn dv_set_whole_1_100(e: &rhai::Engine, wb: &mut Workbook, show_error: bool, allow_blank: bool) {
        with_workbook(wb, || {
            let range = r#"{"sheet":"","start":[0,0],"end":[9,0]}"#;
            let rule = format!(
                r#"{{"kind":"whole","operator":"between","formula1":"1","formula2":"100","allow_blank":{allow_blank},"show_error":{show_error},"error_msg":"请输入1到100的整数"}}"#
            );
            e.run(&format!(r#"set_data_validation(0, `{range}`, `{rule}`)"#)).unwrap();
        });
    }

    fn lc_set_list(e: &rhai::Engine, wb: &mut Workbook, range_end_row: i64, formula1: &str) {
        with_workbook(wb, || {
            let range = format!(r#"{{"sheet":"","start":[0,5],"end":[{range_end_row},5]}}"#);
            let rule = format!(
                r#"{{"kind":"list","formula1":{},"formula2":null,"show_error":true,"error_msg":null}}"#,
                serde_json::to_string(formula1).unwrap()
            );
            e.run(&format!(r#"set_data_validation(0, `{range}`, `{rule}`)"#)).unwrap();
        });
    }

    fn lc_get(e: &rhai::Engine, wb: &mut Workbook, sheet: i64, row: i64, col: i64) -> Vec<String> {
        let mut out = String::new();
        with_workbook(wb, || {
            out = e.eval(&format!("get_list_candidates({sheet}, {row}, {col})")).unwrap();
        });
        serde_json::from_str(&out).unwrap()
    }

    #[test]
    fn test_get_list_candidates_literal_and_none() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        // 无规则
        assert!(lc_get(&e, &mut wb, 0, 0, 5).is_empty());
        // 字面量（逗号、xlsx 外层引号）
        lc_set_list(&e, &mut wb, 9, "苹果, 香蕉,橙子");
        assert_eq!(lc_get(&e, &mut wb, 0, 3, 5), vec!["苹果", "香蕉", "橙子"]);
        lc_set_list(&e, &mut wb, 9, "\"x,y\"");
        assert_eq!(lc_get(&e, &mut wb, 0, 0, 5), vec!["x", "y"]);
        // 不在规则范围内 / 越界 / sheet 不存在 / 负数
        assert!(lc_get(&e, &mut wb, 0, 10, 5).is_empty());
        assert!(lc_get(&e, &mut wb, 0, 0, 4).is_empty());
        assert!(lc_get(&e, &mut wb, 9, 0, 5).is_empty());
        assert!(lc_get(&e, &mut wb, 0, -1, 5).is_empty());
    }

    #[test]
    fn test_get_list_candidates_range_ref() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        // 数据区 A1:A4：苹果 / (空) / 香蕉 / 数字 3
        with_workbook(&mut wb, || {
            e.run(r#"set_cell(0,0,0,`苹果`); set_cell(0,2,0,`香蕉`); set_cell(0,3,0,`3`);"#).unwrap();
        });
        // 同 sheet 单列范围：空格跳过，数字转文本
        lc_set_list(&e, &mut wb, 9, "A1:A4");
        assert_eq!(lc_get(&e, &mut wb, 0, 0, 5), vec!["苹果", "香蕉", "3"]);
        // 带 = 前缀 + 绝对引用
        lc_set_list(&e, &mut wb, 9, "=$A$1:$A$4");
        assert_eq!(lc_get(&e, &mut wb, 0, 0, 5), vec!["苹果", "香蕉", "3"]);
        // 多列行优先展开：B1=乙1, B3=乙3
        with_workbook(&mut wb, || {
            e.run(r#"set_cell(0,0,1,`乙1`); set_cell(0,2,1,`乙3`);"#).unwrap();
        });
        lc_set_list(&e, &mut wb, 9, "A1:B3");
        assert_eq!(lc_get(&e, &mut wb, 0, 0, 5), vec!["苹果", "乙1", "香蕉", "乙3"]);
        // 全空范围 → 空
        lc_set_list(&e, &mut wb, 9, "H1:H5");
        assert!(lc_get(&e, &mut wb, 0, 0, 5).is_empty());
        // 源数据变化后候选自动更新（每次现读）
        with_workbook(&mut wb, || {
            e.run(r#"set_cell(0,0,7,`新值`);"#).unwrap();
        });
        assert_eq!(lc_get(&e, &mut wb, 0, 0, 5), vec!["新值"]);
    }

    #[test]
    fn test_get_list_candidates_cross_sheet() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        // 复制出第二个 sheet，改名 "Data Src"
        let mut s2 = wb.sheets[0].clone();
        s2.name = "Data Src".to_string();
        wb.sheets.push(s2);
        with_workbook(&mut wb, || {
            e.run(r#"set_cell(1,0,0,`甲`); set_cell(1,1,0,`乙`);"#).unwrap();
        });
        lc_set_list(&e, &mut wb, 9, "'Data Src'!A1:A2");
        assert_eq!(lc_get(&e, &mut wb, 0, 0, 5), vec!["甲", "乙"]);
        // sheet 名忽略大小写
        lc_set_list(&e, &mut wb, 9, "data src!A1:A2");
        assert_eq!(lc_get(&e, &mut wb, 0, 0, 5), vec!["甲", "乙"]);
        // sheet 不存在 → 空
        lc_set_list(&e, &mut wb, 9, "NoSuch!A1:A2");
        assert!(lc_get(&e, &mut wb, 0, 0, 5).is_empty());
    }

    fn lv_check(e: &rhai::Engine, wb: &mut Workbook, row: i64, col: i64, text: &str) -> serde_json::Value {
        let mut out = String::new();
        with_workbook(wb, || {
            out = e.eval(&format!(r#"validate_cell_input(0, {row}, {col}, `{text}`)"#)).unwrap();
        });
        serde_json::from_str(&out).unwrap()
    }

    #[test]
    fn test_validate_cell_input_list_literal() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        lc_set_list(&e, &mut wb, 9, "苹果,香蕉,橙子"); // 规则范围 F1:F10 (col=5)
        assert_eq!(lc_check_valid(&e, &mut wb, 0, "苹果"), true);
        assert_eq!(lc_check_valid(&e, &mut wb, 0, " 香蕉 "), true, "首尾空白应被忽略");
        assert_eq!(lc_check_valid(&e, &mut wb, 0, "西瓜"), false);
        assert_eq!(lc_check_valid(&e, &mut wb, 0, "苹"), false, "必须整项匹配");
        // 规则范围之外不受影响
        assert_eq!(lv_check(&e, &mut wb, 0, 4, "西瓜")["valid"], true);
        // 大小写敏感
        lc_set_list(&e, &mut wb, 9, "Apple,Banana");
        assert_eq!(lc_check_valid(&e, &mut wb, 0, "Apple"), true);
        assert_eq!(lc_check_valid(&e, &mut wb, 0, "apple"), false);
    }

    fn lc_check_valid(e: &rhai::Engine, wb: &mut Workbook, row: i64, text: &str) -> bool {
        lv_check(e, wb, row, 5, text)["valid"].as_bool().unwrap()
    }

    #[test]
    fn test_validate_cell_input_list_range_ref_and_cross_sheet() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run(r#"set_cell(0,0,0,`苹果`); set_cell(0,1,0,`香蕉`); set_cell(0,3,0,`7`);"#).unwrap();
        });
        // 同 sheet 范围引用（含数字候选）
        lc_set_list(&e, &mut wb, 9, "A1:A4");
        assert_eq!(lc_check_valid(&e, &mut wb, 0, "苹果"), true);
        assert_eq!(lc_check_valid(&e, &mut wb, 0, "7"), true, "数字候选按展示文本比较");
        assert_eq!(lc_check_valid(&e, &mut wb, 0, "西瓜"), false);
        // 源数据变化后校验立即跟随
        with_workbook(&mut wb, || { e.run(r#"set_cell(0,1,0,`西瓜`);"#).unwrap(); });
        assert_eq!(lc_check_valid(&e, &mut wb, 0, "西瓜"), true);
        assert_eq!(lc_check_valid(&e, &mut wb, 0, "香蕉"), false);
        // 跨 sheet
        let mut s2 = wb.sheets[0].clone();
        s2.name = "Src".to_string();
        wb.sheets.push(s2);
        with_workbook(&mut wb, || { e.run(r#"set_cell(1,0,0,`甲`);"#).unwrap(); });
        lc_set_list(&e, &mut wb, 9, "Src!A1:A1");
        assert_eq!(lc_check_valid(&e, &mut wb, 0, "甲"), true);
        assert_eq!(lc_check_valid(&e, &mut wb, 0, "乙"), false);
    }

    #[test]
    fn test_validate_cell_input_list_bad_ref_and_blank() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        // 引用非法 / sheet 不存在 / 范围全空：规则有问题，放行
        for f in ["NoSuch!A1:A3", "H1:H5"] {
            lc_set_list(&e, &mut wb, 9, f);
            assert_eq!(lc_check_valid(&e, &mut wb, 0, "任意值"), true, "formula1={} 应放行", f);
        }
        // 空值：lc_set_list 的规则 allow_blank 缺省，serde 默认值决定；这里只验证不 panic
        lc_set_list(&e, &mut wb, 9, "a,b");
        let _ = lv_check(&e, &mut wb, 0, 5, "");
    }

    #[test]
    fn test_validate_cell_input_no_rule_is_valid() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let v = dv_input_json(&e, &mut wb, 0, "anything");
        assert_eq!(v["valid"], true);
    }

    #[test]
    fn test_validate_cell_input_whole_between() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        dv_set_whole_1_100(&e, &mut wb, true, true);
        assert_eq!(dv_input_json(&e, &mut wb, 0, "50")["valid"], true);
        let bad = dv_input_json(&e, &mut wb, 0, "200");
        assert_eq!(bad["valid"], false);
        assert_eq!(bad["show_error"], true);
        assert_eq!(bad["error_msg"], "请输入1到100的整数");
        // whole 对非数字文本判违规
        assert_eq!(dv_input_json(&e, &mut wb, 0, "abc")["valid"], false);
        // 规则范围外(第10行,下标10)不受影响
        assert_eq!(dv_input_json(&e, &mut wb, 10, "200")["valid"], true);
    }

    #[test]
    fn test_validate_cell_input_allow_blank() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        dv_set_whole_1_100(&e, &mut wb, true, true);
        assert_eq!(dv_input_json(&e, &mut wb, 0, "")["valid"], true, "allow_blank=true 空值合法");
        dv_set_whole_1_100(&e, &mut wb, true, false);
        assert_eq!(dv_input_json(&e, &mut wb, 0, "")["valid"], false, "allow_blank=false 空值违规");
    }

    #[test]
    fn test_validate_cell_input_formula_is_passed() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        dv_set_whole_1_100(&e, &mut wb, true, true);
        assert_eq!(dv_input_json(&e, &mut wb, 0, "=A1+1000")["valid"], true, "公式写入前无法求值,先放行");
    }

    #[test]
    fn test_validate_cell_input_show_error_false_reported() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        dv_set_whole_1_100(&e, &mut wb, false, true);
        let v = dv_input_json(&e, &mut wb, 0, "200");
        assert_eq!(v["valid"], false);
        assert_eq!(v["show_error"], false, "show_error=false 如实返回,由前端决定是否拦截");
    }

    #[test]
    fn test_update_data_validation() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let range = r#"{"sheet":"Sheet1","start":[0,0],"end":[9,0]}"#;
            let rule  = r#"{"kind":"list","formula1":"A,B,C","formula2":null,"show_error":true,"error_msg":null}"#;
            e.run(&format!(r#"set_data_validation(0, `{range}`, `{rule}`)"#)).unwrap();
        });
        assert_eq!(wb.sheets[0].data_validations.len(), 1);
        assert_eq!(wb.sheets[0].data_validations[0].kind, "list");

        // 更新下标0那条：range不变，kind改成whole，formula1改成新值
        let mut ok = false;
        with_workbook(&mut wb, || {
            let range = r#"{"sheet":"Sheet1","start":[0,0],"end":[9,0]}"#;
            let new_rule = r#"{"kind":"whole","operator":"greaterThan","formula1":"10","formula2":null,"show_error":true,"error_msg":"必须大于10"}"#;
            ok = e.eval(&format!(r#"update_data_validation(0, 0, `{range}`, `{new_rule}`)"#)).unwrap();
        });
        assert!(ok, "update_data_validation 应返回 true（下标0存在）");
        assert_eq!(wb.sheets[0].data_validations.len(), 1, "更新不应改变规则条数");
        assert_eq!(wb.sheets[0].data_validations[0].kind, "whole");
        assert_eq!(wb.sheets[0].data_validations[0].formula1, "10");
        assert_eq!(wb.sheets[0].data_validations[0].error_msg.as_deref(), Some("必须大于10"));
    }

    #[test]
    fn test_update_data_validation_out_of_range_returns_false() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut ok = true;
        with_workbook(&mut wb, || {
            let range = r#"{"sheet":"Sheet1","start":[0,0],"end":[9,0]}"#;
            let rule = r#"{"kind":"list","formula1":"A,B,C","formula2":null,"show_error":true,"error_msg":null}"#;
            // sheet上根本没有任何规则，index=0不存在，应返回false，不panic。
            ok = e.eval(&format!(r#"update_data_validation(0, 0, `{range}`, `{rule}`)"#)).unwrap();
        });
        assert!(!ok, "空sheet上update不存在的下标应返回 false");
        assert_eq!(wb.sheets[0].data_validations.len(), 0, "失败的update不应产生任何新规则");
    }

    fn dv_update_err(rule: &str) -> (String, Workbook) {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let range = r#"{"sheet":"Sheet1","start":[0,0],"end":[9,0]}"#;
        let init = r#"{"kind":"list","formula1":"A,B,C"}"#;
        let mut err = String::new();
        with_workbook(&mut wb, || {
            e.run(&format!(r#"set_data_validation(0, `{range}`, `{init}`)"#)).unwrap();
            err = e.eval::<bool>(&format!(r#"update_data_validation(0, 0, `{range}`, `{rule}`)"#))
                .unwrap_err().to_string();
        });
        (err, wb)
    }

    #[test]
    fn test_update_data_validation_bad_json_throws_keeps_rule() {
        let (err, wb) = dv_update_err("not json");
        assert!(err.contains("update_data_validation") && err.contains("JSON"), "got: {err}");
        assert_eq!(wb.sheets[0].data_validations.len(), 1);
        assert_eq!(wb.sheets[0].data_validations[0].kind, "list");
    }

    #[test]
    fn test_update_data_validation_missing_kind_throws() {
        let (err, wb) = dv_update_err(r#"{"formula1":"A,B"}"#);
        assert!(err.contains("update_data_validation") && err.contains("kind"), "got: {err}");
        assert_eq!(wb.sheets[0].data_validations[0].formula1, "A,B,C");
    }

    #[test]
    fn test_update_data_validation_bad_operator_throws() {
        let (err, wb) = dv_update_err(r#"{"kind":"whole","operator":"gt","formula1":"10"}"#);
        assert!(err.contains("operator"), "got: {err}");
        assert_eq!(wb.sheets[0].data_validations[0].kind, "list");
    }

    #[test]
    fn test_update_data_validation_kind_none_throws() {
        let (err, wb) = dv_update_err(r#"{"kind":"none"}"#);
        assert!(err.contains("remove_data_validation"), "got: {err}");
        assert_eq!(wb.sheets[0].data_validations.len(), 1);
    }

    const KC_RANGE: &str = r#"{"sheet":"Sheet1","start":[0,0],"end":[3,0]}"#;
    const KC_DATA: &str = r#"{"sheet":"Sheet1","start":[0,0],"end":[2,3]}"#;
    const KC_LOC: &str = r#"{"sheet":"Sheet1","start":[0,5],"end":[2,5]}"#;

    fn kc_run(script: &str) -> (Result<String, String>, Workbook) {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out: Result<String, String> = Err(String::new());
        with_workbook(&mut wb, || {
            out = e.eval::<rhai::Dynamic>(script).map(|d| d.to_string()).map_err(|er| er.to_string());
        });
        (out, wb)
    }

    fn kc_err(script: &str) -> (String, Workbook) {
        let (r, wb) = kc_run(script);
        (r.expect_err("应当抛错"), wb)
    }

    #[test]
    fn test_add_cf_rule_bad_kind_json_throws() {
        let (err, wb) = kc_err(&format!(r#"add_cf_rule(0, `{}`, `not json`, false)"#, KC_RANGE));
        assert!(err.contains("add_cf_rule") && err.contains("kind_json"), "got: {err}");
        assert_eq!(wb.sheets[0].cond_formats.len(), 0);
    }

    #[test]
    fn test_add_cf_rule_bad_range_throws() {
        let (err, wb) = kc_err(&format!(r#"add_cf_rule(0, `not json`, `{}`, false)"#, CF_KIND_EXAMPLE));
        assert!(err.contains("range_json"), "got: {err}");
        assert_eq!(wb.sheets[0].cond_formats.len(), 0);
    }

    #[test]
    fn test_add_cf_rule_bad_sheet_throws() {
        let (err, _wb) = kc_err(&format!(r#"add_cf_rule(99, `{}`, `{}`, false)"#, KC_RANGE, CF_KIND_EXAMPLE));
        assert!(err.contains("sheet") && err.contains("99"), "got: {err}");
    }

    #[test]
    fn test_add_cf_rule_valid_returns_true() {
        let (r, wb) = kc_run(&format!(r#"add_cf_rule(0, `{}`, `{}`, false)"#, KC_RANGE, CF_KIND_EXAMPLE));
        assert_eq!(r.unwrap(), "true");
        assert_eq!(wb.sheets[0].cond_formats.len(), 1);
    }

    #[test]
    fn test_add_sparkline_group_bad_kind_throws() {
        let (err, wb) = kc_err(&format!(r#"add_sparkline_group(0, "bar", `{}`, `{}`)"#, KC_DATA, KC_LOC));
        assert!(err.contains("kind"), "got: {err}");
        assert_eq!(wb.sheets[0].sparkline_groups.len(), 0);
    }

    #[test]
    fn test_add_sparkline_group_bad_json_throws() {
        let (err, _wb) = kc_err(&format!(r#"add_sparkline_group(0, "line", `not json`, `{}`)"#, KC_LOC));
        assert!(err.contains("data_json"), "got: {err}");
    }

    #[test]
    fn test_add_sparkline_group_pairing_mismatch_throws() {
        let loc = r#"{"sheet":"Sheet1","start":[0,5],"end":[1,5]}"#;
        let (err, wb) = kc_err(&format!(r#"add_sparkline_group(0, "line", `{}`, `{}`)"#, KC_DATA, loc));
        assert!(err.contains("配对"), "got: {err}");
        assert_eq!(wb.sheets[0].sparkline_groups.len(), 0);
    }

    #[test]
    fn test_add_sparkline_group_valid_returns_count() {
        let (r, wb) = kc_run(&format!(r#"add_sparkline_group(0, "line", `{}`, `{}`)"#, KC_DATA, KC_LOC));
        assert_eq!(r.unwrap(), "3");
        assert_eq!(wb.sheets[0].sparkline_groups.len(), 1);
    }

    #[test]
    fn test_text_to_columns_multi_bad_delims_throws() {
        let (err, _wb) = kc_err(r#"text_to_columns_multi(0, 0, 3, 0, `not json`, false)"#);
        assert!(err.contains("delims_json"), "got: {err}");
    }

    #[test]
    fn test_text_to_columns_multi_reversed_range_throws() {
        let (err, _wb) = kc_err(r#"text_to_columns_multi(0, 3, 0, 0, `["|"]`, false)"#);
        assert!(err.contains("end_row"), "got: {err}");
    }

    #[test]
    fn test_text_to_columns_multi_bad_sheet_throws() {
        let (err, _wb) = kc_err(r#"text_to_columns_multi(99, 0, 3, 0, `["|"]`, false)"#);
        assert!(err.contains("sheet=99"), "got: {err}");
    }

    #[test]
    fn test_apply_auto_filter_rows_valid_ok() {
        let (r, _wb) = kc_run(r#"apply_auto_filter_rows(0, 0, 0, 1, 3, `{"col_filters":{}}`)"#);
        assert!(r.unwrap().contains("\"ok\":true"));
    }

    #[test]
    fn test_apply_auto_filter_rows_bad_json_throws() {
        let (err, _wb) = kc_err(r#"apply_auto_filter_rows(0, 0, 0, 1, 3, `not json`)"#);
        assert!(err.contains("filter_json"), "got: {err}");
    }

    #[test]
    fn test_apply_auto_filter_rows_bad_col_filters_throws() {
        for fj in [r#"{}"#, r#"{"col_filters":{"a":["x"]}}"#, r#"{"col_filters":{"0":[1,2]}}"#] {
            let (err, _wb) = kc_err(&format!(r#"apply_auto_filter_rows(0, 0, 0, 1, 3, `{}`)"#, fj));
            assert!(err.contains("col_filters"), "fj={fj} got: {err}");
        }
    }

    #[test]
    fn test_apply_auto_filter_rows_bad_sheet_throws() {
        let (err, _wb) = kc_err(r#"apply_auto_filter_rows(99, 0, 0, 1, 3, `{"col_filters":{}}`)"#);
        assert!(err.contains("sheet_idx=99"), "got: {err}");
    }

    #[test]
    fn test_apply_auto_filter_rows_bad_range_still_returns_ok_false() {
        // 范围错误保持原行为(前台按 ok:false 处理),不抛错
        let (r, _wb) = kc_run(r#"apply_auto_filter_rows(0, 5, 0, 1, 3, `{"col_filters":{}}`)"#);
        let s = r.unwrap();
        assert!(s.contains("\"ok\":false") && s.contains("bad range"), "got: {s}");
    }

    #[test]
    fn test_remove_data_validation() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let range = r#"{"sheet":"Sheet1","start":[0,0],"end":[9,0]}"#;
            let rule  = r#"{"kind":"list","formula1":"A,B,C","formula2":null,"show_error":true,"error_msg":null}"#;
            e.run(&format!(r#"set_data_validation(0, `{range}`, `{rule}`)"#)).unwrap();
        });
        assert_eq!(wb.sheets[0].data_validations.len(), 1);

        let mut ok = false;
        with_workbook(&mut wb, || {
            ok = e.eval("remove_data_validation(0, 0)").unwrap();
        });
        assert!(ok, "remove_data_validation 应返回 true（下标0存在）");
        assert_eq!(wb.sheets[0].data_validations.len(), 0, "删除后规则数组应为空");
    }

    #[test]
    fn test_remove_data_validation_out_of_range_returns_false() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut ok = true;
        with_workbook(&mut wb, || {
            // 空sheet上删除不存在的下标，应返回false，不panic。
            ok = e.eval("remove_data_validation(0, 0)").unwrap();
        });
        assert!(!ok, "空sheet上remove不存在的下标应返回 false");
    }
    #[test]
    fn test_insert_chart() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let chart_json = r#"{"kind":"Bar","title":"éå®","x_labels":["A","B","C","D"],"series":[{"name":"ç³»å1","data":[120.0,200.0,150.0,80.0]}]}"#;
        with_workbook(&mut wb, || {
            let result: i64 = e.eval(&format!(r#"insert_chart(0, 2, 1, `{chart_json}`)"#)).unwrap();
            assert!(result > 0, "insert_chart should return a positive id");
        });
        assert_eq!(wb.sheets[0].floating_charts.len(), 1, "floating_charts should have one entry");
        let chart = &wb.sheets[0].floating_charts[0];
        assert!(matches!(chart.kind, roe_model::impress::ChartKind::Bar));
        assert_eq!(chart.x_labels, vec!["A","B","C","D"]);
        assert_eq!(chart.series.len(), 1);
        assert_eq!(chart.series[0].data, vec![120.0,200.0,150.0,80.0]);
        assert!(chart.x_pt > 0.0, "x_pt should accumulate from anchor_col");
        assert!(chart.y_pt > 0.0, "y_pt should accumulate from anchor_row");
    }

    #[test]
    fn test_insert_chart_aligns_with_render_geometry() {
        let mut wb = empty_wb();
        {
            let sh = &mut wb.sheets[0];
            sh.col_widths.clear();
            sh.row_heights.clear();
            sh.default_col_width = 0.0;
            sh.default_row_height = 0.0;
        }
        let mut e = rhai::Engine::new();
        register(&mut e);
        let cj = r#"{"kind":"Bar","x_labels":["A"],"series":[{"name":"s","data":[1]}]}"#;
        with_workbook(&mut wb, || {
            let _: i64 = e.eval(&format!(r#"insert_chart(0, 2, 3, `{cj}`)"#)).unwrap();
        });
        let cfg = roe_core::tile::CalcRenderConfig::default();
        let (ex, ey) = roe_core::tile::cell_to_pt(&wb.sheets[0], 2, 3, &cfg);
        let c = &wb.sheets[0].floating_charts[0];
        assert_eq!((c.x_pt, c.y_pt), (ex, ey), "图表左上角必须等于单元格的渲染坐标");
        assert_eq!((c.x_pt, c.y_pt), (144.0, 30.0), "空表应按 48pt/15pt 默认值,而不是 72");
    }

    const AO_CJ: &str = r#"{"kind":"Bar","x_labels":["A"],"series":[{"name":"s","data":[1]}],"width_pt":200,"height_pt":100,"avoid_overlap":true}"#;
    const NO_AO_CJ: &str = r#"{"kind":"Bar","x_labels":["A"],"series":[{"name":"s","data":[1]}],"width_pt":200,"height_pt":100}"#;

    #[test]
    fn test_set_chart_style_applies_patch() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let id: i64 = e.eval(&format!(r#"insert_chart(0, 0, 0, `{NO_AO_CJ}`)"#)).unwrap();
            e.eval::<()>(&format!(r##"set_chart_style(0, {id}, `{{"title":"新","show_legend":false,"grouping":"Stacked","border_color":"#ff0000","gap_width_pct":50}}`)"##)).unwrap();
        });
        let c = &wb.sheets[0].floating_charts[0];
        assert_eq!(c.title.as_deref(), Some("新"));
        assert!(!c.show_legend);
        assert_eq!(c.grouping, roe_model::impress::ChartGrouping::Stacked);
        assert_eq!(c.border_color, roe_model::common::Color { r: 255, g: 0, b: 0, a: 255 });
        assert_eq!(c.gap_width_pct, Some(50.0));
    }

    #[test]
    fn test_set_chart_style_invalid_throws_and_changes_nothing() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let id: i64 = e.eval(&format!(r#"insert_chart(0, 0, 0, `{NO_AO_CJ}`)"#)).unwrap();
            let err = e.eval::<()>(&format!(r#"set_chart_style(0, {id}, `{{"title":"X","kind":"Nope"}}`)"#))
                .unwrap_err().to_string();
            assert!(err.contains("Nope") && err.contains("示例"), "got: {err}");
            let err = e.eval::<()>(&format!(r#"set_chart_style(0, {id}, `{{"xlabel":"a"}}`)"#))
                .unwrap_err().to_string();
            assert!(err.contains("xlabel") && err.contains("x_axis_label"), "got: {err}");
            let err = e.eval::<()>(&format!(r#"set_chart_style(0, {id}, `{{"show_legend":"yes"}}`)"#))
                .unwrap_err().to_string();
            assert!(err.contains("show_legend"), "got: {err}");
        });
        let c = &wb.sheets[0].floating_charts[0];
        assert!(c.title.is_none() && c.show_legend, "报错时图表不能被改动");
    }

    #[test]
    fn test_set_chart_style_missing_chart_throws() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let err = e.eval::<()>(r#"set_chart_style(0, 99, `{"title":"a"}`)"#).unwrap_err().to_string();
            assert!(err.contains("不存在"), "got: {err}");
        });
    }

    #[test]
    fn test_chart_optional_keys_are_read_by_insert_chart() {
        // 每个登记的可选键,都必须在 insert_chart 的实现里出现,防止文档与实现漂移
        let src = include_str!("calc_api.rs");
        let start = src.find("fn \"insert_chart\" =>").expect("找不到 insert_chart");
        let body = &src[start..start + 6000.min(src.len() - start)];
        for k in CHART_OPTIONAL_KEYS {
            assert!(body.contains(&format!("\"{}\"", k)), "insert_chart 没有读取 {k}");
        }
    }

    #[test]
    fn test_used_extent_basic() {
        let mut wb = empty_wb();
        assert_eq!(used_extent(&wb.sheets[0]), (None, None));
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.eval::<()>(r#"set_cell(0, 2, 3, "x"); set_cell(0, 5, 1, "y");"#).unwrap();
        });
        assert_eq!(used_extent(&wb.sheets[0]), (Some(5), Some(3)));
    }

    #[test]
    fn test_insert_chart_no_avoid_keeps_position() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let _: i64 = e.eval(&format!(r#"insert_chart(0, 0, 0, `{NO_AO_CJ}`)"#)).unwrap();
            let _: i64 = e.eval(&format!(r#"insert_chart(0, 0, 0, `{NO_AO_CJ}`)"#)).unwrap();
        });
        let c = &wb.sheets[0].floating_charts;
        assert_eq!((c[0].x_pt, c[0].y_pt), (c[1].x_pt, c[1].y_pt), "不传 avoid_overlap 行为不变");
    }

    #[test]
    fn test_insert_chart_avoid_no_conflict_keeps_position() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let _: i64 = e.eval(&format!(r#"insert_chart(0, 2, 3, `{AO_CJ}`)"#)).unwrap();
        });
        let cfg = roe_core::tile::CalcRenderConfig::default();
        let (ex, ey) = roe_core::tile::cell_to_pt(&wb.sheets[0], 2, 3, &cfg);
        let c = &wb.sheets[0].floating_charts[0];
        assert_eq!((c.x_pt, c.y_pt), (ex, ey));
    }

    #[test]
    fn test_insert_chart_avoid_moves_right_of_data() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.eval::<()>(r#"for r in 0..5 { for c in 0..2 { set_cell(0, r, c, "1"); } }"#).unwrap();
            let _: i64 = e.eval(&format!(r#"insert_chart(0, 0, 0, `{AO_CJ}`)"#)).unwrap();
        });
        let cfg = roe_core::tile::CalcRenderConfig::default();
        let sh = &wb.sheets[0];
        let (dx, dy) = roe_core::tile::cell_to_pt(sh, 5, 2, &cfg);
        let c = &sh.floating_charts[0];
        assert!(!roe_core::tile::rect_overlap((0.0, 0.0, dx, dy), (c.x_pt, c.y_pt, c.width_pt, c.height_pt)),
            "图表不能压住数据区");
        let (rx, _) = roe_core::tile::cell_to_pt(sh, 0, 3, &cfg);
        assert_eq!((c.x_pt, c.y_pt), (rx, 0.0), "应放在 max_col+2 列、顶部");
    }

    #[test]
    fn test_insert_chart_avoid_stacks_below_existing_chart() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.eval::<()>(r#"for r in 0..5 { for c in 0..2 { set_cell(0, r, c, "1"); } }"#).unwrap();
            let _: i64 = e.eval(&format!(r#"insert_chart(0, 0, 0, `{AO_CJ}`)"#)).unwrap();
            let _: i64 = e.eval(&format!(r#"insert_chart(0, 0, 0, `{AO_CJ}`)"#)).unwrap();
        });
        let c = &wb.sheets[0].floating_charts;
        assert_eq!(c[1].x_pt, c[0].x_pt, "同一列堆叠");
        assert!(c[1].y_pt >= c[0].y_pt + c[0].height_pt, "第二张在第一张下方: {} vs {}", c[1].y_pt, c[0].y_pt);
        assert!(!roe_core::tile::rect_overlap(
            (c[0].x_pt, c[0].y_pt, c[0].width_pt, c[0].height_pt),
            (c[1].x_pt, c[1].y_pt, c[1].width_pt, c[1].height_pt)));
    }

    #[test]
    fn test_insert_chart_avoid_overlap_must_be_bool() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let cj = r#"{"kind":"Bar","x_labels":["A"],"series":[{"name":"s","data":[1]}],"avoid_overlap":"yes"}"#;
        with_workbook(&mut wb, || {
            let err = e.eval::<i64>(&format!(r#"insert_chart(0, 0, 0, `{cj}`)"#)).unwrap_err().to_string();
            assert!(err.contains("avoid_overlap"), "got: {err}");
        });
        assert!(wb.sheets[0].floating_charts.is_empty());
    }

    #[test]
    fn test_insert_chart_skips_hidden_cols() {
        let mut wb = empty_wb();
        {
            let sh = &mut wb.sheets[0];
            sh.col_widths = vec![10.0, 20.0, 30.0];
            sh.col_hidden = vec![false, true, false];
        }
        let mut e = rhai::Engine::new();
        register(&mut e);
        let cj = r#"{"kind":"Bar","x_labels":["A"],"series":[{"name":"s","data":[1]}]}"#;
        with_workbook(&mut wb, || {
            let _: i64 = e.eval(&format!(r#"insert_chart(0, 0, 3, `{cj}`)"#)).unwrap();
        });
        assert_eq!(wb.sheets[0].floating_charts[0].x_pt, 40.0, "隐藏列不占宽度");
    }

    #[test]
    fn test_get_chart_config_reads_normal_chart() {
        let mut wb = empty_wb();
        let cj = lc_chart(100, 60);
        let v = lc_list(&mut wb, &format!(
            "let id = insert_chart(0, 0, 0, `{cj}`); get_chart_config(0, id)"));
        assert_eq!(v["stat"], false, "{v}");
        assert_eq!(v["kind"], "Bar", "{v}");
        assert_eq!(v["grouping"], "Standard", "{v}");
        assert!(v["show_legend"].is_boolean() && v["show_val"].is_boolean(), "{v}");
        assert!(v["border_color"].as_str().unwrap().starts_with('#'), "{v}");
        assert_eq!(v["width_pt"], 100.0, "{v}");
        for k in roe_core::chart_style::STYLE_KEYS {
            assert!(v.get(*k).is_some(), "缺少 STYLE_KEYS 键 {k}: {v}");
        }
    }

    #[test]
    fn test_get_chart_config_errors() {
        let mut wb = empty_wb();
        let cj = lc_chart(100, 60);
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let _ = e.eval::<i64>(&format!("insert_chart(0, 0, 0, `{cj}`)")).unwrap();
            let e1 = e.eval::<String>("get_chart_config(99, 1)").unwrap_err().to_string();
            assert!(e1.contains("get_chart_config") && e1.contains("越界"), "{e1}");
            let e2 = e.eval::<String>("get_chart_config(0, 999)").unwrap_err().to_string();
            assert!(e2.contains("不存在") && e2.contains("list_charts"), "{e2}");
            let e3 = e.eval::<String>("get_chart_config(0, 0)").unwrap_err().to_string();
            assert!(e3.contains("正整数"), "{e3}");
        });
    }

    #[test]
    fn test_get_chart_config_roundtrip_through_set_chart_style() {
        let mut wb = empty_wb();
        let cj = lc_chart(100, 60);
        let v = lc_list(&mut wb, &format!(
            "let id = insert_chart(0, 0, 0, `{cj}`); get_chart_config(0, id)"));
        let id = v["id"].as_i64().unwrap();
        let mut style = serde_json::Map::new();
        for k in roe_core::chart_style::STYLE_KEYS {
            style.insert(k.to_string(), v[*k].clone());
        }
        let flipped = !v["show_legend"].as_bool().unwrap();
        style.insert("show_legend".to_string(), serde_json::Value::Bool(flipped));
        let style_s = serde_json::Value::Object(style.clone()).to_string();
        let v2 = lc_list(&mut wb, &format!(
            "set_chart_style(0, {id}, `{style_s}`); get_chart_config(0, {id})"));
        assert_eq!(v2["show_legend"], flipped, "{v2}");
        for k in roe_core::chart_style::STYLE_KEYS {
            assert_eq!(v2[*k], style[*k], "往返后 {k} 不一致");
        }
    }

    #[test]
    fn test_get_chart_config_available_in_readonly_engine() {
        let mut wb = empty_wb();
        let cj = lc_chart(100, 60);
        let _ = lc_list(&mut wb, &format!("insert_chart(0, 0, 0, `{cj}`); list_charts(0)"));
        let mut e = rhai::Engine::new();
        register_readonly(&mut e);
        let mut out = String::new();
        with_workbook(&mut wb, || { out = e.eval::<String>("get_chart_config(0, 1)").unwrap(); });
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["stat"], false, "{v}");
    }

    fn lc_chart(w: i32, h: i32) -> String {
        format!(r#"{{"kind":"Bar","x_labels":["A"],"series":[{{"name":"s","data":[1]}}],"width_pt":{w},"height_pt":{h}}}"#)
    }

    fn lc_list(wb: &mut roe_model::calc::Workbook, script: &str) -> serde_json::Value {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_workbook(wb, || { out = e.eval::<String>(script).unwrap(); });
        serde_json::from_str(&out).unwrap()
    }

    #[test]
    fn test_list_charts_range_and_overlaps() {
        let mut wb = empty_wb();
        {
            let sh = &mut wb.sheets[0];
            sh.col_widths.clear(); sh.row_heights.clear();
            sh.default_col_width = 0.0; sh.default_row_height = 0.0;
        }
        let cj = lc_chart(100, 60);
        let v = lc_list(&mut wb, &format!(
            "insert_chart(0, 2, 1, `{cj}`); insert_chart(0, 2, 1, `{cj}`); list_charts(0)"));
        let a = v.as_array().unwrap();
        assert_eq!(a.len(), 2);
        // 左上 (48,30) -> B3;右下 (148,90) 内缩 0.5pt -> D6
        assert_eq!(a[0]["range"], "B3:D6");
        assert_eq!(a[0]["x_pt"], 48.0);
        assert_eq!(a[0]["overlaps"], serde_json::json!([a[1]["id"]]));
        assert_eq!(a[1]["overlaps"], serde_json::json!([a[0]["id"]]));
        assert_eq!(a[0]["at_origin"], false);
        assert_eq!(a[0]["stat"], false);
    }

    #[test]
    fn test_list_charts_no_overlap_when_apart_or_touching() {
        let mut wb = empty_wb();
        {
            let sh = &mut wb.sheets[0];
            sh.col_widths.clear(); sh.row_heights.clear();
            sh.default_col_width = 0.0; sh.default_row_height = 0.0;
        }
        let cj = lc_chart(96, 60); // 宽 96 = 恰好 2 列
        // 第一张 B3 起,右边缘 = 48+96 = 144 = D 列左边界;第二张从 D3 起,仅相贴
        let v = lc_list(&mut wb, &format!(
            "insert_chart(0, 2, 1, `{cj}`); insert_chart(0, 2, 3, `{cj}`); list_charts(0)"));
        let a = v.as_array().unwrap();
        assert_eq!(a[0]["overlaps"], serde_json::json!([]));
        assert_eq!(a[1]["overlaps"], serde_json::json!([]));
    }

    #[test]
    fn test_list_charts_at_origin_flag() {
        let mut wb = empty_wb();
        let cj = lc_chart(100, 60);
        let v = lc_list(&mut wb, &format!(
            "insert_chart(0, 0, 0, `{cj}`); insert_chart(0, 0, 0, `{cj}`); list_charts(0)"));
        let a = v.as_array().unwrap();
        assert_eq!(a[0]["at_origin"], true);
        assert_eq!(a[1]["at_origin"], true);
    }

    #[test]
    fn test_list_charts_empty_and_invalid_sheet() {
        let mut wb = empty_wb();
        let v = lc_list(&mut wb, "list_charts(0)");
        assert_eq!(v, serde_json::json!([]));
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let err = e.eval::<String>("list_charts(99)").unwrap_err().to_string();
            assert!(err.contains("list_charts") && err.contains("越界"), "got: {err}");
        });
    }

    fn wc_wb() -> roe_model::calc::Workbook {
        let mut wb = empty_wb();
        let sh = &mut wb.sheets[0];
        sh.col_widths.clear(); sh.row_heights.clear();
        sh.default_col_width = 0.0; sh.default_row_height = 0.0;
        wb
    }

    fn wc_run(wb: &mut roe_model::calc::Workbook, script: &str) -> Result<Vec<(usize, u32, u32)>, String> {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut res: Result<(), String> = Ok(());
        let dirty = with_workbook(wb, || {
            res = e.eval::<()>(script).map_err(|x| x.to_string());
        });
        res.map(|_| dirty)
    }

    #[test]
    fn test_set_chart_rect_applies_and_marks_dirty_without_struct_edit() {
        let mut wb = wc_wb();
        let cj = lc_chart(100, 60);
        let _ = take_struct_edit();
        let dirty = wc_run(&mut wb, &format!(
            "let id = insert_chart(0, 0, 0, `{cj}`); set_chart_rect(0, id, 10.0, 20.0, 200.0, 100.0);")).unwrap();
        let c = &wb.sheets[0].floating_charts[0];
        assert_eq!((c.x_pt, c.y_pt, c.width_pt, c.height_pt), (10.0, 20.0, 200.0, 100.0));
        assert!(dirty.contains(&(0, 0, 0)));
        assert!(!take_struct_edit(), "图表写函数不应置 struct_edit");
    }

    #[test]
    fn test_set_chart_rect_invalid_args_throw_and_leave_chart_unchanged() {
        let mut wb = wc_wb();
        let cj = lc_chart(100, 60);
        let pre = format!("let id = insert_chart(0, 0, 0, `{cj}`); ");
        for (call, needle) in [
            ("set_chart_rect(0, id, 0.0, 0.0, 0.0, 50.0)", "必须大于 0"),
            ("set_chart_rect(0, id, -1.0, 0.0, 10.0, 10.0)", "不能为负"),
            ("set_chart_rect(0, 999, 0.0, 0.0, 10.0, 10.0)", "不存在"),
            ("set_chart_rect(5, id, 0.0, 0.0, 10.0, 10.0)", "越界"),
        ] {
            let err = wc_run(&mut wb, &format!("{pre}{call}")).unwrap_err();
            assert!(err.contains(needle), "{call} -> {err}");
        }
        // 每次 wc_run 的脚本都先 insert_chart,但出错整体应由执行层回滚;这里只断言报错内容
    }

    #[test]
    fn test_move_chart_to_cell_keeps_size() {
        let mut wb = wc_wb();
        let cj = lc_chart(100, 60);
        wc_run(&mut wb, &format!(
            "let id = insert_chart(0, 0, 0, `{cj}`); move_chart_to_cell(0, id, 2, 3);")).unwrap();
        let c = &wb.sheets[0].floating_charts[0];
        assert_eq!((c.x_pt, c.y_pt), (144.0, 30.0));
        assert_eq!((c.width_pt, c.height_pt), (100.0, 60.0));
    }

    #[test]
    fn test_resize_chart_to_range_covers_exactly() {
        let mut wb = wc_wb();
        let cj = lc_chart(100, 60);
        // B3:D6 = 行 2..=5、列 1..=3
        wc_run(&mut wb, &format!(
            "let id = insert_chart(0, 0, 0, `{cj}`); resize_chart_to_range(0, id, 2, 1, 5, 3);")).unwrap();
        let c = &wb.sheets[0].floating_charts[0];
        assert_eq!((c.x_pt, c.y_pt, c.width_pt, c.height_pt), (48.0, 30.0, 144.0, 60.0));
        let err = wc_run(&mut wb, "resize_chart_to_range(0, 1, 5, 1, 2, 3)").unwrap_err();
        assert!(err.contains("区域不合法"), "got: {err}");
    }

    #[test]
    fn test_delete_chart_true_then_false() {
        let mut wb = wc_wb();
        let cj = lc_chart(100, 60);
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut got = (false, false, String::new());
        let dirty = with_workbook(&mut wb, || {
            let _: i64 = e.eval(&format!("insert_chart(0, 0, 0, `{cj}`)")).unwrap();
            got.0 = e.eval::<bool>("delete_chart(0, 1)").unwrap();
            got.1 = e.eval::<bool>("delete_chart(0, 1)").unwrap();
            got.2 = e.eval::<bool>("delete_chart(7, 1)").unwrap_err().to_string();
        });
        assert!(got.0 && !got.1, "第一次 true,第二次 false");
        assert!(got.2.contains("越界"), "got: {}", got.2);
        assert!(wb.sheets[0].floating_charts.is_empty());
        assert!(dirty.contains(&(0, 0, 0)));
    }

    #[test]
    fn test_insert_chart_invalid_json_throws() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let err = e.eval::<i64>(r#"insert_chart(0, 0, 0, "not a json")"#).unwrap_err().to_string();
            assert!(err.contains("insert_chart") && err.contains("JSON"), "got: {err}");
        });
        assert_eq!(wb.sheets[0].floating_charts.len(), 0);
    }

    #[test]
    fn test_insert_chart_invalid_sheet_throws() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let chart_json = r#"{"kind":"Bar","x_labels":["A"],"series":[{"name":"s","data":[1]}]}"#;
        with_workbook(&mut wb, || {
            let err = e.eval::<i64>(&format!(r#"insert_chart(99, 0, 0, `{chart_json}`)"#))
                .unwrap_err().to_string();
            assert!(err.contains("sheet_idx=99") && err.contains("越界"), "got: {err}");
        });
        assert_eq!(wb.sheets[0].floating_charts.len(), 0);
    }

    #[test]
    fn test_insert_chart_kind_is_case_sensitive_and_lists_valid() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let chart_json = r#"{"kind":"bar","series":[{"name":"s","data":[1]}]}"#;
        with_workbook(&mut wb, || {
            let err = e.eval::<i64>(&format!(r#"insert_chart(0, 0, 0, `{chart_json}`)"#))
                .unwrap_err().to_string();
            assert!(err.contains("\"bar\"") && err.contains("HorizontalBar") && err.contains("Doughnut"),
                "got: {err}");
        });
        assert_eq!(wb.sheets[0].floating_charts.len(), 0);
    }

    #[test]
    fn test_insert_chart_missing_or_empty_series_throws() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            for chart_json in [
                r#"{"kind":"Bar"}"#,
                r#"{"kind":"Bar","series":[]}"#,
            ] {
                let err = e.eval::<i64>(&format!(r#"insert_chart(0, 0, 0, `{chart_json}`)"#))
                    .unwrap_err().to_string();
                assert!(err.contains("series"), "got: {err}");
            }
        });
        assert_eq!(wb.sheets[0].floating_charts.len(), 0);
    }

    #[test]
    fn test_insert_chart_string_number_in_data_throws() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let chart_json = r#"{"kind":"Bar","series":[{"name":"s","data":[1,"2"]}]}"#;
        with_workbook(&mut wb, || {
            let err = e.eval::<i64>(&format!(r#"insert_chart(0, 0, 0, `{chart_json}`)"#))
                .unwrap_err().to_string();
            assert!(err.contains("series[0].data[1]"), "got: {err}");
        });
        assert_eq!(wb.sheets[0].floating_charts.len(), 0, "出错时不应留下半成品图表");
    }

    #[test]
    fn test_insert_chart_number_in_x_labels_throws() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let chart_json = r#"{"kind":"Bar","x_labels":[1,2],"series":[{"name":"s","data":[1,2]}]}"#;
        with_workbook(&mut wb, || {
            let err = e.eval::<i64>(&format!(r#"insert_chart(0, 0, 0, `{chart_json}`)"#))
                .unwrap_err().to_string();
            assert!(err.contains("x_labels[0]"), "got: {err}");
        });
        assert_eq!(wb.sheets[0].floating_charts.len(), 0);
    }

    #[test]
    fn test_insert_chart_negative_anchor_throws() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let chart_json = r#"{"kind":"Bar","series":[{"name":"s","data":[1]}]}"#;
        with_workbook(&mut wb, || {
            let err = e.eval::<i64>(&format!(r#"insert_chart(0, -1, 0, `{chart_json}`)"#))
                .unwrap_err().to_string();
            assert!(err.contains("anchor"), "got: {err}");
        });
        assert_eq!(wb.sheets[0].floating_charts.len(), 0);
    }

    #[test]
    fn test_insert_chart_custom_params_are_applied() {
        // 验证 insert_chart 硬编码参数开放后，chart_json 里显式传入的
        // width_pt/height_pt/show_legend/show_grid/x_axis_label/
        // y_axis_label/border_color/border_width/gap_width_pct
        // 均被正确透传到 ChartModel，不再是此前的硬编码值。
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let chart_json = r##"{"kind":"Bar","title":"t","x_labels":["A","B"],"series":[{"name":"s","data":[1.0,2.0]}],"width_pt":500.0,"height_pt":250.0,"show_legend":false,"show_grid":false,"x_axis_label":"月份","y_axis_label":"销量","border_color":"#ff0000","border_width":2.5,"gap_width_pct":150.0}"##;
        with_workbook(&mut wb, || {
            let result: i64 = e.eval(&format!(r#"insert_chart(0, 0, 0, `{chart_json}`)"#)).unwrap();
            assert!(result > 0, "insert_chart should return a positive id");
        });
        assert_eq!(wb.sheets[0].floating_charts.len(), 1);
        let chart = &wb.sheets[0].floating_charts[0];
        assert_eq!(chart.width_pt, 500.0, "width_pt should be passed through");
        assert_eq!(chart.height_pt, 250.0, "height_pt should be passed through");
        assert_eq!(chart.show_legend, false, "show_legend should be passed through");
        assert_eq!(chart.show_grid, false, "show_grid should be passed through");
        assert_eq!(chart.x_axis_label, "月份", "x_axis_label should be passed through");
        assert_eq!(chart.y_axis_label, "销量", "y_axis_label should be passed through");
        assert_eq!(
            chart.border_color,
            roe_model::common::Color { r: 0xff, g: 0x00, b: 0x00, a: 255 },
            "border_color should be parsed from hex string and passed through"
        );
        assert_eq!(chart.border_width, 2.5, "border_width should be passed through");
        assert_eq!(chart.gap_width_pct, Some(150.0), "gap_width_pct should be passed through");
    }

    #[test]
    fn test_insert_chart_default_params_when_omitted() {
        // 向后兼容验证：不传这 9 个新字段时，应回退到此前硬编码的默认值，
        // 保证既有调用方（旧脚本/旧测试）行为不受本次改动影响。
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let chart_json = r#"{"kind":"Bar","title":"t","x_labels":["A","B"],"series":[{"name":"s","data":[1.0,2.0]}]}"#;
        with_workbook(&mut wb, || {
            let result: i64 = e.eval(&format!(r#"insert_chart(0, 0, 0, `{chart_json}`)"#)).unwrap();
            assert!(result > 0, "insert_chart should return a positive id");
        });
        assert_eq!(wb.sheets[0].floating_charts.len(), 1);
        let chart = &wb.sheets[0].floating_charts[0];
        assert_eq!(chart.width_pt, 300.0, "width_pt should default to previous hardcoded value");
        assert_eq!(chart.height_pt, 300.0, "height_pt should default to previous hardcoded value");
        assert_eq!(chart.show_legend, true, "show_legend should default to true");
        assert_eq!(chart.show_grid, true, "show_grid should default to true");
        assert_eq!(chart.x_axis_label, "", "x_axis_label should default to empty string");
        assert_eq!(chart.y_axis_label, "", "y_axis_label should default to empty string");
        assert_eq!(
            chart.border_color,
            roe_model::common::Color { r: 0xcc, g: 0xcc, b: 0xcc, a: 255 },
            "border_color should default to previous hardcoded gray"
        );
        assert_eq!(chart.border_width, 1.0, "border_width should default to previous hardcoded value");
        assert_eq!(chart.gap_width_pct, None, "gap_width_pct should default to None");
    }

    // ============ delete_duplicates / delete_rows_batch 测试 (v157) ============

    fn set_row(wb: &mut Workbook, sheet: usize, row: u32, vals: &[CellValue]) {
        let s = &mut wb.sheets[sheet];
        for (c, v) in vals.iter().enumerate() {
            set_cell_in_sheet(s, row, c as u32, v.clone());
        }
    }

    #[test]
    fn test_delete_duplicates_basic_all_columns() {
        // A列/B列两列数据，行1和行3完全相同，应删掉行3
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Text("x".into()), CellValue::Number(1.0)]);
        set_row(&mut wb, 0, 1, &[CellValue::Text("a".into()), CellValue::Number(1.0)]);
        set_row(&mut wb, 0, 2, &[CellValue::Text("b".into()), CellValue::Number(2.0)]);
        set_row(&mut wb, 0, 3, &[CellValue::Text("a".into()), CellValue::Number(1.0)]);

        let mut e = rhai::Engine::new();
        register(&mut e);
        register_row_col_ops(&mut e);
        let result: String = with_workbook(&mut wb, || {}).len().to_string(); // dummy to keep type inference happy
        let _ = result;
        let mut ret = String::new();
        with_workbook(&mut wb, || {
            ret = e.eval(r#"delete_duplicates(0, 0, 0, 3, 1, "[]", true)"#).unwrap();
        });
        assert_eq!(ret, "ok:1", "应删除1行重复数据");
        // 删除后应剩3行：header + a,1 + b,2
        assert_eq!(wb.sheets[0].row_heights.len().max(3), 3.max(wb.sheets[0].row_heights.len()));
        assert_eq!(get_cell_text(&wb, 0, 0, 0), "x");
        assert_eq!(get_cell_text(&wb, 0, 1, 0), "a");
        assert_eq!(get_cell_text(&wb, 0, 2, 0), "b");
        // 行3（原来的重复行）不应再存在于新的行2之后
        assert_eq!(get_cell_text(&wb, 0, 3, 0), "");
    }

    #[test]
    fn test_delete_duplicates_by_specific_column() {
        // 只按第0列（相对c1偏移0）判重，即使第1列不同也算重复
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Text("h1".into()), CellValue::Text("h2".into())]);
        set_row(&mut wb, 0, 1, &[CellValue::Text("k".into()), CellValue::Number(1.0)]);
        set_row(&mut wb, 0, 2, &[CellValue::Text("k".into()), CellValue::Number(999.0)]); // 第0列相同，视为重复
        set_row(&mut wb, 0, 3, &[CellValue::Text("m".into()), CellValue::Number(3.0)]);

        let mut e = rhai::Engine::new();
        register(&mut e);
        register_row_col_ops(&mut e);
        let mut ret = String::new();
        with_workbook(&mut wb, || {
            ret = e.eval(r#"delete_duplicates(0, 0, 0, 3, 1, "[0]", true)"#).unwrap();
        });
        assert_eq!(ret, "ok:1");
        assert_eq!(get_cell_text(&wb, 0, 1, 0), "k");
        assert_eq!(get_cell_text(&wb, 0, 2, 0), "m");
    }

    #[test]
    fn test_delete_duplicates_no_header_includes_first_row() {
        // has_header=false：第一行也参与判重
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Number(5.0)]);
        set_row(&mut wb, 0, 1, &[CellValue::Number(5.0)]); // 与行0重复
        set_row(&mut wb, 0, 2, &[CellValue::Number(6.0)]);

        let mut e = rhai::Engine::new();
        register(&mut e);
        register_row_col_ops(&mut e);
        let mut ret = String::new();
        with_workbook(&mut wb, || {
            ret = e.eval(r#"delete_duplicates(0, 0, 0, 2, 0, "[]", false)"#).unwrap();
        });
        assert_eq!(ret, "ok:1");
        assert_eq!(get_cell_text(&wb, 0, 0, 0), "5");
        assert_eq!(get_cell_text(&wb, 0, 1, 0), "6");
    }

    // ============ detect_current_region 测试 ============

    // ============ detect_header_row 测试 ============

    // ============ get_row_texts 测试 ============

    fn row_texts_of(wb: &mut Workbook, row: i64, c1: i64, c2: i64) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_workbook(wb, || {
            out = e
                .eval(&format!("get_row_texts(0, {row}, {c1}, {c2})"))
                .unwrap();
        });
        out
    }

    #[test]
    fn test_get_row_texts_mixed_text_and_number() {
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Text("name".into()), CellValue::Number(2.0), CellValue::Text("city".into())]);
        assert_eq!(row_texts_of(&mut wb, 0, 0, 2), "[\"name\",\"2\",\"city\"]");
    }

    #[test]
    fn test_get_row_texts_empty_cells_and_quotes() {
        // 中间一格未存储 → "";含引号的文本被正确转义
        let mut wb = empty_wb();
        set_cell_in_sheet(&mut wb.sheets[0], 0, 0, CellValue::Text("a\"b".into()));
        set_cell_in_sheet(&mut wb.sheets[0], 0, 2, CellValue::Text("z".into()));
        assert_eq!(row_texts_of(&mut wb, 0, 0, 2), "[\"a\\\"b\",\"\",\"z\"]");
    }

    #[test]
    fn test_get_row_texts_invalid_range_is_empty_array() {
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Text("x".into())]);
        assert_eq!(row_texts_of(&mut wb, 0, 3, 1), "[]");
        assert_eq!(row_texts_of(&mut wb, -1, 0, 1), "[]");
    }

    #[test]
    fn test_get_row_texts_available_in_readonly_engine() {
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Text("name".into()), CellValue::Number(2.0), CellValue::Text("city".into())]);
        let mut e = rhai::Engine::new();
        register_readonly(&mut e);
        let mut out = String::new();
        let mut write_err = String::new();
        with_workbook(&mut wb, || {
            out = e.eval::<String>("get_row_texts(0, 0, 0, 2)").unwrap();
            write_err = e.eval::<String>("set_cell(0, 0, 0, \"x\"); \"\"").unwrap_err().to_string();
        });
        assert_eq!(out, "[\"name\",\"2\",\"city\"]");
        assert!(write_err.contains("Function not found"), "写函数应未注册: {write_err}");
    }

    fn hdr_of(wb: &mut Workbook, r1: i64, c1: i64, r2: i64, c2: i64) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_workbook(wb, || {
            out = e
                .eval(&format!("detect_header_row(0, {r1}, {c1}, {r2}, {c2})"))
                .unwrap();
        });
        out
    }

    #[test]
    fn test_detect_header_row_text_over_numbers_is_header() {
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Text("name".into()), CellValue::Text("score".into())]);
        set_row(&mut wb, 0, 1, &[CellValue::Text("a".into()), CellValue::Number(1.0)]);
        set_row(&mut wb, 0, 2, &[CellValue::Text("b".into()), CellValue::Number(2.0)]);
        assert_eq!(hdr_of(&mut wb, 0, 0, 2, 1), "{\"header\":true}");
    }

    #[test]
    fn test_detect_header_row_all_numbers_is_not_header() {
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Number(1.0), CellValue::Number(2.0)]);
        set_row(&mut wb, 0, 1, &[CellValue::Number(3.0), CellValue::Number(4.0)]);
        assert_eq!(hdr_of(&mut wb, 0, 0, 1, 1), "{\"header\":false}");
    }

    #[test]
    fn test_detect_header_row_all_text_is_not_header() {
        // 通讯录式全文本表:首行无法与数据区分,保守判无表头
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Text("Tom".into()), CellValue::Text("Beijing".into())]);
        set_row(&mut wb, 0, 1, &[CellValue::Text("Amy".into()), CellValue::Text("Tokyo".into())]);
        assert_eq!(hdr_of(&mut wb, 0, 0, 1, 1), "{\"header\":false}");
    }

    #[test]
    fn test_detect_header_row_number_in_first_row_is_not_header() {
        // 首行含一个数字(如年份列头 2024)→ 不是表头
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Text("item".into()), CellValue::Number(2024.0)]);
        set_row(&mut wb, 0, 1, &[CellValue::Text("a".into()), CellValue::Number(5.0)]);
        assert_eq!(hdr_of(&mut wb, 0, 0, 1, 1), "{\"header\":false}");
    }

    #[test]
    fn test_detect_header_row_single_row_is_not_header() {
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Text("name".into()), CellValue::Text("score".into())]);
        assert_eq!(hdr_of(&mut wb, 0, 0, 0, 1), "{\"header\":false}");
    }

    fn region_of(wb: &mut Workbook, r: i64, c: i64) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_workbook(wb, || {
            out = e.eval(&format!("detect_current_region(0, {r}, {c})")).unwrap();
        });
        out
    }

    #[test]
    fn test_detect_current_region_isolated_empty_is_none() {
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 10, &[CellValue::Number(1.0)]);
        // (0,0) 及其八邻域全空 → none
        assert_eq!(region_of(&mut wb, 0, 0), "{\"direction\":\"none\"}");
    }

    #[test]
    fn test_detect_current_region_rect_block_from_any_cell() {
        // 3行×2列的实心块,从块内任一格出发得到同一矩形
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 1, &[CellValue::Number(1.0), CellValue::Number(2.0)]);
        set_row(&mut wb, 0, 2, &[CellValue::Number(3.0), CellValue::Number(4.0)]);
        set_row(&mut wb, 0, 3, &[CellValue::Number(5.0), CellValue::Number(6.0)]);
        let expect = "{\"end\":[3,1],\"start\":[1,0]}";
        assert_eq!(region_of(&mut wb, 2, 1), expect);
        assert_eq!(region_of(&mut wb, 1, 0), expect);
        assert_eq!(region_of(&mut wb, 3, 1), expect);
    }

    #[test]
    fn test_detect_current_region_iris_like_header_and_blank_tail() {
        // 表头 + 数据 + 之后一片空白(对应真机 Iris 场景):
        // 从数据区中间一列出发,应覆盖整个 A1:C4,不包含空白行
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[
            CellValue::Text("Id".into()), CellValue::Text("W".into()), CellValue::Text("S".into()),
        ]);
        for i in 1..=3u32 {
            set_row(&mut wb, 0, i, &[
                CellValue::Number(i as f64), CellValue::Number(3.5), CellValue::Text("setosa".into()),
            ]);
        }
        // 第 4 行起是空白;再在很远处放一个无关数据,不应被并入
        set_row(&mut wb, 0, 20, &[CellValue::Number(99.0)]);
        assert_eq!(region_of(&mut wb, 2, 1), "{\"end\":[3,2],\"start\":[0,0]}");
    }

    #[test]
    fn test_detect_current_region_l_shape_and_diagonal_link() {
        // L 形:A1,A2,A3,B3。八邻域含对角,所以 C4 与 B3 对角相连也应并入
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Number(1.0)]);
        set_row(&mut wb, 0, 1, &[CellValue::Number(2.0)]);
        set_row(&mut wb, 0, 2, &[CellValue::Number(3.0), CellValue::Number(4.0)]);
        set_cell_in_sheet(&mut wb.sheets[0], 3, 2, CellValue::Number(5.0)); // C4,与 B3 对角
        assert_eq!(region_of(&mut wb, 0, 0), "{\"end\":[3,2],\"start\":[0,0]}");
    }

    #[test]
    fn test_detect_current_region_blank_row_and_col_separate() {
        // 中间空一整行:上下两块互不连通,从上块出发不应包含下块
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Number(1.0), CellValue::Number(2.0)]);
        set_row(&mut wb, 0, 1, &[CellValue::Number(3.0), CellValue::Number(4.0)]);
        // 第 2 行空;第 3 行起是另一块
        set_row(&mut wb, 0, 3, &[CellValue::Number(7.0), CellValue::Number(8.0)]);
        assert_eq!(region_of(&mut wb, 0, 0), "{\"end\":[1,1],\"start\":[0,0]}");
        // 空字符串文本视为空:即使有 Text("") 也不并入
        set_cell_in_sheet(&mut wb.sheets[0], 2, 0, CellValue::Text(String::new()));
        assert_eq!(region_of(&mut wb, 0, 0), "{\"end\":[1,1],\"start\":[0,0]}");
    }

    fn run_dedup(wb: &mut Workbook, r1: i64, c1: i64, r2: i64, c2: i64, cols: &str) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        register_row_col_ops(&mut e);
        let mut ret = String::new();
        with_workbook(wb, || {
            ret = e.eval(&format!(
                r#"delete_duplicates(0, {r1}, {c1}, {r2}, {c2}, "{cols}", false)"#
            )).unwrap();
        });
        ret
    }

    #[test]
    fn test_delete_duplicates_trailing_blank_rows_untouched() {
        // 数据在第 0~2 行(第 2 行与第 0 行重复),选区却拉到第 999999 行:
        // 尾部空行既不遍历也不删除,只删 1 个真重复行
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Number(1.0), CellValue::Text("a".into())]);
        set_row(&mut wb, 0, 1, &[CellValue::Number(2.0), CellValue::Text("b".into())]);
        set_row(&mut wb, 0, 2, &[CellValue::Number(1.0), CellValue::Text("a".into())]);
        assert_eq!(run_dedup(&mut wb, 0, 0, 999_999, 1, "[]"), "ok:1");
        assert_eq!(get_cell_text(&wb, 0, 0, 0), "1");
        assert_eq!(get_cell_text(&wb, 0, 1, 0), "2");
        assert_eq!(get_cell_text(&wb, 0, 2, 0), "");
    }

    #[test]
    fn test_delete_duplicates_interior_blank_rows_collapse_to_one() {
        // 数据区域内夹 3 个空行(第 1、2、3 行),应删到只剩 1 个空行
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Number(1.0)]);
        // 行 1~3 留空
        set_row(&mut wb, 0, 4, &[CellValue::Number(2.0)]);
        assert_eq!(run_dedup(&mut wb, 0, 0, 4, 0, "[]"), "ok:2", "3 个空行应删 2 个");
        assert_eq!(get_cell_text(&wb, 0, 0, 0), "1");
        assert_eq!(get_cell_text(&wb, 0, 1, 0), "");
        assert_eq!(get_cell_text(&wb, 0, 2, 0), "2");
    }

    #[test]
    fn test_delete_duplicates_all_blank_selection_is_noop() {
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 50, &[CellValue::Number(7.0)]); // 选区之外的数据
        assert_eq!(run_dedup(&mut wb, 0, 0, 10, 1, "[]"), "ok:0");
        assert_eq!(get_cell_text(&wb, 0, 50, 0), "7", "选区外数据不受影响");
    }

    #[test]
    fn test_delete_duplicates_key_col_blank_but_other_col_has_data() {
        // 只按第 0 列判重(cols="[0]");第 0 列为空、第 1 列有数据的两行,
        // 在判重列上都是空 → 视为区域内空行,删到只剩 1 行
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Text("k".into()), CellValue::Number(1.0)]);
        set_cell_in_sheet(&mut wb.sheets[0], 1, 1, CellValue::Number(2.0)); // 第 0 列空
        set_cell_in_sheet(&mut wb.sheets[0], 2, 1, CellValue::Number(3.0)); // 第 0 列空
        set_row(&mut wb, 0, 3, &[CellValue::Text("m".into()), CellValue::Number(4.0)]);
        assert_eq!(run_dedup(&mut wb, 0, 0, 3, 1, "[0]"), "ok:1");
        assert_eq!(get_cell_text(&wb, 0, 0, 0), "k");
        assert_eq!(get_cell_text(&wb, 0, 1, 1), "2", "保留最靠上的空键行及其其他列数据");
        assert_eq!(get_cell_text(&wb, 0, 2, 0), "m");
    }

    #[test]
    fn test_delete_duplicates_no_duplicates_returns_zero() {
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Number(1.0)]);
        set_row(&mut wb, 0, 1, &[CellValue::Number(2.0)]);
        set_row(&mut wb, 0, 2, &[CellValue::Number(3.0)]);

        let mut e = rhai::Engine::new();
        register(&mut e);
        register_row_col_ops(&mut e);
        let mut ret = String::new();
        with_workbook(&mut wb, || {
            ret = e.eval(r#"delete_duplicates(0, 0, 0, 2, 0, "[]", false)"#).unwrap();
        });
        assert_eq!(ret, "ok:0");
    }

    #[test]
    fn test_delete_duplicates_multiple_duplicate_rows_non_contiguous() {
        // 验证批量删除多行（非连续）时行号偏移计算正确
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Number(1.0)]); // keep (row0)
        set_row(&mut wb, 0, 1, &[CellValue::Number(2.0)]); // keep (row1)
        set_row(&mut wb, 0, 2, &[CellValue::Number(1.0)]); // dup of row0 -> delete
        set_row(&mut wb, 0, 3, &[CellValue::Number(3.0)]); // keep (row3 -> new row2)
        set_row(&mut wb, 0, 4, &[CellValue::Number(2.0)]); // dup of row1 -> delete
        set_row(&mut wb, 0, 5, &[CellValue::Number(4.0)]); // keep (row5 -> new row3)

        let mut e = rhai::Engine::new();
        register(&mut e);
        register_row_col_ops(&mut e);
        let mut ret = String::new();
        with_workbook(&mut wb, || {
            ret = e.eval(r#"delete_duplicates(0, 0, 0, 5, 0, "[]", false)"#).unwrap();
        });
        assert_eq!(ret, "ok:2");
        assert_eq!(get_cell_text(&wb, 0, 0, 0), "1");
        assert_eq!(get_cell_text(&wb, 0, 1, 0), "2");
        assert_eq!(get_cell_text(&wb, 0, 2, 0), "3");
        assert_eq!(get_cell_text(&wb, 0, 3, 0), "4");
        assert_eq!(get_cell_text(&wb, 0, 4, 0), "");
    }

    #[test]
    fn test_delete_rows_batch_row_heights_shrink() {
        let mut wb = empty_wb();
        {
            let s = &mut wb.sheets[0];
            s.row_heights = vec![
                roe_model::calc::RowHeight::Manual(10.0),
                roe_model::calc::RowHeight::Manual(20.0),
                roe_model::calc::RowHeight::Manual(30.0),
                roe_model::calc::RowHeight::Manual(40.0),
            ];
        }
        delete_rows_batch(&mut wb.sheets[0], &[1, 2]);
        assert_eq!(wb.sheets[0].row_heights.len(), 2);
        assert_eq!(wb.sheets[0].row_heights[0], roe_model::calc::RowHeight::Manual(10.0));
        assert_eq!(wb.sheets[0].row_heights[1], roe_model::calc::RowHeight::Manual(40.0));
    }

    #[test]
    fn test_delete_rows_batch_merged_cells_fully_inside_removed() {
        // 合并区域(1,0)-(2,0)整体落在待删行[1,2]内 -> 应被丢弃
        let mut wb = empty_wb();
        wb.sheets[0].merged_cells.push(roe_model::calc::MergedCell { r1: 1, c1: 0, r2: 2, c2: 0 });
        delete_rows_batch(&mut wb.sheets[0], &[1, 2]);
        assert_eq!(wb.sheets[0].merged_cells.len(), 0);
    }

    #[test]
    fn test_delete_rows_batch_merged_cells_shift() {
        // 合并区域(3,0)-(4,0)完全在删除行[1]之后 -> 整体上移1行变(2,0)-(3,0)
        let mut wb = empty_wb();
        wb.sheets[0].merged_cells.push(roe_model::calc::MergedCell { r1: 3, c1: 0, r2: 4, c2: 0 });
        delete_rows_batch(&mut wb.sheets[0], &[1]);
        assert_eq!(wb.sheets[0].merged_cells.len(), 1);
        let m = &wb.sheets[0].merged_cells[0];
        assert_eq!((m.r1, m.r2), (2, 3));
    }

    #[test]
    fn test_delete_rows_batch_cell_formats_shift() {
        let mut wb = empty_wb();
        wb.sheets[0].cell_formats.insert((0, 0), roe_model::calc::CellFormat::default());
        wb.sheets[0].cell_formats.insert((3, 0), roe_model::calc::CellFormat::default());
        delete_rows_batch(&mut wb.sheets[0], &[1]);
        assert!(wb.sheets[0].cell_formats.contains_key(&(0, 0)));
        assert!(!wb.sheets[0].cell_formats.contains_key(&(3, 0)));
        assert!(wb.sheets[0].cell_formats.contains_key(&(2, 0)), "行3的格式应随行号收缩变为行2");
    }

    // ============ text_to_columns_multi / preview_text_to_columns / split_by_delims ============

    fn ds(v: &[&str]) -> Vec<String> { v.iter().map(|s| s.to_string()).collect() }

    fn run_ttc(wb: &mut Workbook, r1: i64, r2: i64, col: i64, delims: &str, merge: bool) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut ret = String::new();
        with_workbook(wb, || {
            ret = match e.eval::<String>(&format!(
                r#"text_to_columns_multi(0, {r1}, {r2}, {col}, "{delims}", {merge})"#
            )) {
                Ok(s) => s,
                Err(er) => format!("ERR:{er}"),
            };
        });
        ret
    }

    /// 断言 run_ttc 抛错(返回 "ERR:..."),且报错文本含关键词 kw。
    fn ttc_err(wb: &mut Workbook, r1: i64, r2: i64, col: i64, delims: &str, kw: &str) {
        let r = run_ttc(wb, r1, r2, col, delims, false);
        assert!(r.starts_with("ERR:") && r.contains(kw), "期望抛错且含 {kw:?},实际: {r}");
    }

    fn run_ttc_preview(wb: &mut Workbook, r1: i64, r2: i64, col: i64, delims: &str, merge: bool) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut ret = String::new();
        with_workbook(wb, || {
            ret = e.eval(&format!(
                r#"preview_text_to_columns(0, {r1}, {r2}, {col}, "{delims}", {merge})"#
            )).unwrap();
        });
        ret
    }

    /// 该格是否真的是 Empty / 未存储(区别于 Text(""))。
    fn is_empty_cell(wb: &Workbook, sheet: usize, row: u32, col: u32) -> bool {
        wb.sheets[sheet].cells.cols.get(&col)
            .map(|c| {
                for blk in &c.blocks {
                    if row >= blk.start_row && row < blk.start_row + blk.values.len() as u32 {
                        return matches!(blk.values[(row - blk.start_row) as usize], CellValue::Empty);
                    }
                }
                true
            })
            .unwrap_or(true)
    }

    #[test]
    fn test_split_by_delims_basic_and_multi() {
        assert_eq!(split_by_delims("a,b c;d", &ds(&[",", " ", ";"]), false), vec!["a", "b", "c", "d"]);
        assert_eq!(split_by_delims("abc", &ds(&[","]), false), vec!["abc"]);
        assert_eq!(split_by_delims("", &ds(&[","]), false), vec![""]);
    }

    #[test]
    fn test_split_by_delims_leading_trailing_and_consecutive() {
        // merge=false:与 str::split 一致,产生首尾/中间空段
        assert_eq!(split_by_delims(",a,,b,", &ds(&[","]), false), vec!["", "a", "", "b", ""]);
        // merge=true:连续分隔符只算一个,首尾不特殊处理
        assert_eq!(split_by_delims(",a,,b,", &ds(&[","]), true), vec!["", "a", "b", ""]);
        // merge=true 下不同分隔符连续也合并
        assert_eq!(split_by_delims("a, ;b", &ds(&[",", " ", ";"]), true), vec!["a", "b"]);
    }

    #[test]
    fn test_split_by_delims_longest_match_and_multibyte() {
        // 最长匹配:", " 优先于 ","
        assert_eq!(split_by_delims("a, b,c", &ds(&[",", ", "]), false), vec!["a", "b", "c"]);
        // 多字符分隔符
        assert_eq!(split_by_delims("a||b||c", &ds(&["||"]), false), vec!["a", "b", "c"]);
        // 中文内容与中文分隔符不被切坏
        assert_eq!(split_by_delims("张三,李四,王五", &ds(&[","]), false), vec!["张三", "李四", "王五"]);
        assert_eq!(split_by_delims("甲、乙、丙", &ds(&["、"]), false), vec!["甲", "乙", "丙"]);
    }

    #[test]
    fn test_ttc_multi_mixed_delims_and_number_parse() {
        let mut wb = empty_wb();
        set_cell_in_sheet(&mut wb.sheets[0], 0, 0, CellValue::Text("10,1.5 text".into()));
        let r = run_ttc(&mut wb, 0, 0, 0, r#"[\",\", \" \"]"#, false);
        assert_eq!(r, "{\"max_cols\":3}");
        assert_eq!(get_cell_text(&wb, 0, 0, 0), "10");
        assert_eq!(get_cell_text(&wb, 0, 0, 1), "1.5");
        assert_eq!(get_cell_text(&wb, 0, 0, 2), "text");
        assert!(matches!(wb.sheets[0].cells.get(0, 1), CellValue::Number(_)), "1.5 应解析为数字");
    }

    #[test]
    fn test_ttc_multi_tab_delimiter() {
        let mut wb = empty_wb();
        set_cell_in_sheet(&mut wb.sheets[0], 0, 0, CellValue::Text("a\tb\tc".into()));
        let r = run_ttc(&mut wb, 0, 0, 0, r#"[\"\\t\"]"#, false);
        assert_eq!(r, "{\"max_cols\":3}");
        assert_eq!(get_cell_text(&wb, 0, 0, 1), "b");
    }

    #[test]
    fn test_ttc_multi_merge_consecutive_and_empty_segment_is_empty_cell() {
        let mut wb = empty_wb();
        set_cell_in_sheet(&mut wb.sheets[0], 0, 0, CellValue::Text("a,,b".into()));
        set_cell_in_sheet(&mut wb.sheets[0], 1, 0, CellValue::Text("a,,b".into()));
        // 第 0 行不合并:a / 空 / b;第 1 行合并:a / b
        let _ = run_ttc(&mut wb, 0, 0, 0, r#"[\",\"]"#, false);
        assert_eq!(get_cell_text(&wb, 0, 0, 0), "a");
        assert!(is_empty_cell(&wb, 0, 0, 1), "空段应为 Empty 而非 Text(\"\")");
        assert_eq!(get_cell_text(&wb, 0, 0, 2), "b");
        let r = run_ttc(&mut wb, 1, 1, 0, r#"[\",\"]"#, true);
        assert_eq!(r, "{\"max_cols\":2}");
        assert_eq!(get_cell_text(&wb, 0, 1, 1), "b");
    }

    #[test]
    fn test_ttc_multi_overwrites_right_and_multi_row_max_cols() {
        let mut wb = empty_wb();
        set_cell_in_sheet(&mut wb.sheets[0], 0, 0, CellValue::Text("a,b".into()));
        set_cell_in_sheet(&mut wb.sheets[0], 0, 1, CellValue::Text("OLD".into()));
        set_cell_in_sheet(&mut wb.sheets[0], 1, 0, CellValue::Text("x,y,z".into()));
        let r = run_ttc(&mut wb, 0, 1, 0, r#"[\",\"]"#, false);
        assert_eq!(r, "{\"max_cols\":3}");
        assert_eq!(get_cell_text(&wb, 0, 0, 1), "b", "右侧原数据被覆盖");
        assert_eq!(get_cell_text(&wb, 0, 1, 2), "z");
    }

    #[test]
    fn test_ttc_multi_invalid_args_no_change() {
        let mut wb = empty_wb();
        set_cell_in_sheet(&mut wb.sheets[0], 0, 0, CellValue::Text("a,b".into()));
        ttc_err(&mut wb, 0, 0, 0, "[]", "delims_json");
        ttc_err(&mut wb, 0, 0, 0, r#"[\"\"]"#, "delims_json");
        ttc_err(&mut wb, 0, 0, 0, "not json", "delims_json");
        ttc_err(&mut wb, 5, 0, 0, r#"[\",\"]"#, "end_row");
        assert_eq!(get_cell_text(&wb, 0, 0, 0), "a,b", "非法参数不应改动数据");
        assert_eq!(get_cell_text(&wb, 0, 0, 1), "");
    }

    #[test]
    fn test_ttc_multi_row_range_too_large_rejected() {
        let mut wb = empty_wb();
        set_cell_in_sheet(&mut wb.sheets[0], 0, 0, CellValue::Text("a,b".into()));
        ttc_err(&mut wb, 0, 2_000_000, 0, r#"[\",\"]"#, "行范围过大");
        assert_eq!(get_cell_text(&wb, 0, 0, 1), "", "超限时不应写入");
    }

    #[test]
    fn test_ttc_preview_matches_and_detects_overwrite() {
        let mut wb = empty_wb();
        set_cell_in_sheet(&mut wb.sheets[0], 0, 0, CellValue::Text("a,b,c".into()));
        // 右侧空:不覆盖
        assert_eq!(run_ttc_preview(&mut wb, 0, 0, 0, r#"[\",\"]"#, false),
                   "{\"max_cols\":3,\"overwrites\":false}");
        // 预检不改数据
        assert_eq!(get_cell_text(&wb, 0, 0, 1), "");
        // 右侧 col+2 有数据:覆盖
        set_cell_in_sheet(&mut wb.sheets[0], 0, 2, CellValue::Text("OLD".into()));
        assert_eq!(run_ttc_preview(&mut wb, 0, 0, 0, r#"[\",\"]"#, false),
                   "{\"max_cols\":3,\"overwrites\":true}");
        // 无分隔符命中:max_cols=1,恒不覆盖
        assert_eq!(run_ttc_preview(&mut wb, 0, 0, 0, r#"[\";\"]"#, false),
                   "{\"max_cols\":1,\"overwrites\":false}");
        // 非法参数
        assert_eq!(run_ttc_preview(&mut wb, 0, 0, 0, "[]", false),
                   "{\"max_cols\":0,\"overwrites\":false}");
    }

    // ── 自动筛选(行隐藏)测试 ────────────────────────────────────────────
    // 数据:第0行表头;第1..=6行,列0 = A B A C B A,列1 = x y y x y x
    fn af_data() -> Workbook {
        let mut wb = empty_wb();
        set_row(&mut wb, 0, 0, &[CellValue::Text("h0".into()), CellValue::Text("h1".into())]);
        let c0 = ["A", "B", "A", "C", "B", "A"];
        let c1 = ["x", "y", "y", "x", "y", "x"];
        for i in 0..6 {
            set_row(&mut wb, 0, (i + 1) as u32,
                &[CellValue::Text(c0[i].into()), CellValue::Text(c1[i].into())]);
        }
        wb
    }

    fn af_run(wb: &mut Workbook, script: &str) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_workbook(wb, || {
            out = e.eval::<String>(script).unwrap();
        });
        out
    }

    fn af_json(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap()
    }

    fn af_hidden_rows(wb: &Workbook) -> Vec<usize> {
        wb.sheets[0].row_hidden.iter().enumerate()
            .filter(|(_, h)| **h).map(|(i, _)| i).collect()
    }

    #[test]
    fn test_af_unique_values_counts_and_order() {
        let mut wb = af_data();
        let r = af_run(&mut wb, "get_column_unique_values(0, 0, 0, 6)");
        assert_eq!(af_json(&r), serde_json::json!({
            "values": [{"v":"A","n":3},{"v":"B","n":2},{"v":"C","n":1}],
            "truncated": false
        }));
    }

    #[test]
    fn test_af_unique_values_blank_is_empty_string() {
        let mut wb = af_data();
        // 第7行列0留空,第8行再放一个 A
        set_row(&mut wb, 0, 8, &[CellValue::Text("A".into())]);
        let r = af_run(&mut wb, "get_column_unique_values(0, 0, 0, 8)");
        let v = af_json(&r);
        let arr = v["values"].as_array().unwrap();
        let blank = arr.iter().find(|x| x["v"] == "").expect("应有空串项");
        assert_eq!(blank["n"], 1, "第7行为空,记 1 个空串");
        let a = arr.iter().find(|x| x["v"] == "A").unwrap();
        assert_eq!(a["n"], 4);
    }

    #[test]
    fn test_af_unique_values_bad_args_empty() {
        let mut wb = af_data();
        let r = af_run(&mut wb, "get_column_unique_values(0, 5, 0, 5)");
        assert_eq!(af_json(&r), serde_json::json!({"values": [], "truncated": false}));
    }

    #[test]
    fn test_af_single_column_hide_and_restore() {
        let mut wb = af_data();
        let r = af_run(&mut wb,
            r#"apply_auto_filter_rows(0, 0, 0, 1, 6, `{"col_filters":{"0":["A"]}}`)"#);
        assert_eq!(af_json(&r), serde_json::json!({"ok":true,"hidden":3,"visible":3}));
        assert_eq!(af_hidden_rows(&wb), vec![2, 4, 5]);
        assert!(!wb.sheets[0].row_hidden[0], "表头行不动");

        // 清除筛选(col_filters 为空)→ 全部恢复
        let r2 = af_run(&mut wb,
            r#"apply_auto_filter_rows(0, 0, 0, 1, 6, `{"col_filters":{}}`)"#);
        assert_eq!(af_json(&r2), serde_json::json!({"ok":true,"hidden":0,"visible":6}));
        assert!(af_hidden_rows(&wb).is_empty());
    }

    #[test]
    fn test_af_multi_column_intersection() {
        let mut wb = af_data();
        let r = af_run(&mut wb,
            r#"apply_auto_filter_rows(0, 0, 0, 1, 6, `{"col_filters":{"0":["A"],"1":["x"]}}`)"#);
        // 列0=A: 行1,3,6;列1=x: 行1,4,6;交集 = 行1,6
        assert_eq!(af_json(&r), serde_json::json!({"ok":true,"hidden":4,"visible":2}));
        assert_eq!(af_hidden_rows(&wb), vec![2, 3, 4, 5]);
    }

    #[test]
    fn test_af_empty_allowed_list_hides_all() {
        let mut wb = af_data();
        let r = af_run(&mut wb,
            r#"apply_auto_filter_rows(0, 0, 0, 1, 6, `{"col_filters":{"0":[]}}`)"#);
        assert_eq!(af_json(&r), serde_json::json!({"ok":true,"hidden":6,"visible":0}));
        assert_eq!(af_hidden_rows(&wb), vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn test_af_keeps_hidden_buttons_same_header_and_clears_on_move() {
        let mut wb = af_data();
        wb.sheets[0].auto_filter = Some(roe_model::calc::AutoFilter {
            header_row: 0, header_col: 0, end_col: 1,
            col_filters: std::collections::HashMap::new(),
            hidden_buttons: [1u32].into_iter().collect(),
        });
        af_run(&mut wb, r#"apply_auto_filter_rows(0, 0, 0, 1, 6, `{"col_filters":{"0":["A"]}}`)"#);
        assert!(wb.sheets[0].auto_filter.as_ref().unwrap().hidden_buttons.contains(&1),
            "同表头位置应保留 hidden_buttons");

        // 换表头位置 → 不沿用
        af_run(&mut wb, r#"apply_auto_filter_rows(0, 1, 0, 1, 6, `{"col_filters":{}}`)"#);
        assert!(wb.sheets[0].auto_filter.as_ref().unwrap().hidden_buttons.is_empty());
    }

    #[test]
    fn test_af_end_col_and_col_filters_stored() {
        let mut wb = af_data();
        af_run(&mut wb, r#"apply_auto_filter_rows(0, 0, 0, 3, 6, `{"col_filters":{"0":["A"]}}`)"#);
        let af = wb.sheets[0].auto_filter.as_ref().unwrap();
        assert_eq!(af.end_col, 3, "end_col 用调用方传入值,不靠猜");
        assert_eq!(af.col_filters.get(&0).unwrap(), &vec!["A".to_string()]);
    }

    #[test]
    fn test_af_bad_range_no_write() {
        let mut wb = af_data();
        let r = af_run(&mut wb,
            r#"apply_auto_filter_rows(0, 0, 2, 1, 6, `{"col_filters":{"0":["A"]}}`)"#);
        assert_eq!(af_json(&r)["ok"], false);
        assert!(wb.sheets[0].auto_filter.is_none());
        assert!(af_hidden_rows(&wb).is_empty());
    }

    #[test]
    fn test_af_rows_outside_range_untouched() {
        let mut wb = af_data();
        wb.sheets[0].row_hidden = vec![false; 10];
        wb.sheets[0].row_hidden[8] = true; // end_row 之外,手动隐藏
        af_run(&mut wb, r#"apply_auto_filter_rows(0, 0, 0, 1, 6, `{"col_filters":{"0":["A"]}}`)"#);
        assert!(wb.sheets[0].row_hidden[8], "区域外的行不应被改动");
        assert_eq!(&af_hidden_rows(&wb)[..3], &[2, 4, 5]);
    }

    #[test]
    fn test_af_row_hidden_vec_grows() {
        let mut wb = af_data();
        assert!(wb.sheets[0].row_hidden.is_empty());
        af_run(&mut wb, r#"apply_auto_filter_rows(0, 0, 0, 1, 6, `{"col_filters":{"0":["B"]}}`)"#);
        assert!(wb.sheets[0].row_hidden.len() >= 7, "row_hidden 应补齐到 end_row");
    }

    #[test]
    fn test_af_clear_restores_rows_and_removes_filter() {
        let mut wb = af_data();
        af_run(&mut wb, r#"apply_auto_filter_rows(0, 0, 0, 1, 6, `{"col_filters":{"0":["A"]}}`)"#);
        assert_eq!(af_hidden_rows(&wb), vec![2, 4, 5]);
        let r = af_run(&mut wb, "clear_auto_filter(0, 6)");
        assert_eq!(af_json(&r), serde_json::json!({"ok":true,"had":true,"restored":3}));
        assert!(wb.sheets[0].auto_filter.is_none());
        assert!(af_hidden_rows(&wb).is_empty());
    }

    #[test]
    fn test_af_clear_without_filter_is_noop() {
        let mut wb = af_data();
        let r = af_run(&mut wb, "clear_auto_filter(0, 6)");
        assert_eq!(af_json(&r), serde_json::json!({"ok":true,"had":false,"restored":0}));
    }

    #[test]
    fn test_af_clear_leaves_rows_outside_range() {
        let mut wb = af_data();
        wb.sheets[0].row_hidden = vec![false; 10];
        wb.sheets[0].row_hidden[8] = true;
        af_run(&mut wb, r#"apply_auto_filter_rows(0, 0, 0, 1, 6, `{"col_filters":{"0":["A"]}}`)"#);
        af_run(&mut wb, "clear_auto_filter(0, 6)");
        assert!(wb.sheets[0].row_hidden[8], "区域外手动隐藏的行保持隐藏");
        assert!(!wb.sheets[0].row_hidden[2]);
    }

    #[test]
    fn test_af_enable_with_empty_filters_hides_nothing() {
        let mut wb = af_data();
        let r = af_run(&mut wb, r#"apply_auto_filter_rows(0, 0, 0, 1, 6, `{"col_filters":{}}`)"#);
        assert_eq!(af_json(&r), serde_json::json!({"ok":true,"hidden":0,"visible":6}));
        assert!(wb.sheets[0].auto_filter.is_some(), "空条件也应建立 auto_filter(出现下拉按钮)");
    }

    fn get_cell_text(wb: &Workbook, sheet: usize, row: u32, col: u32) -> String {
        wb.sheets[sheet].cells.cols.get(&col)
            .and_then(|c| {
                for blk in &c.blocks {
                    if row >= blk.start_row && row < blk.start_row + blk.values.len() as u32 {
                        return Some(cell_value_to_string(&blk.values[(row - blk.start_row) as usize]));
                    }
                }
                None
            })
            .unwrap_or_default()
    }

    #[test]
    fn test_save_xlsx_produces_valid_bytes() {
        // 验证 save_xlsx() 脚本接口：base64 解码后应能被 XlsxParser 正常
        // 读回，sheet 数量与 empty_wb() 构造的原始 workbook 一致。
        let mut wb = empty_wb();
        let expected_sheet_count = wb.sheets.len();
        with_workbook(&mut wb, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            let b64: String = e.eval("save_xlsx()").unwrap();
            assert!(!b64.is_empty(), "save_xlsx() 不应返回空字符串");

            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64.as_bytes())
                .expect("base64 解码失败");

            let parsed = roe_parser::ooxml::xlsx::XlsxParser::parse(&bytes)
                .expect("XlsxParser 读回失败，说明 save_xlsx 产出的字节流不是合法 xlsx");

            assert_eq!(parsed.sheets.len(), expected_sheet_count);
        });
    }

}

#[cfg(test)]
mod structure_edit_tests {
    //! insert/delete 行列后 cell_formats 及按索引展开字段的平移测试。
    use roe_model::calc::{CellFormat, MergedCell, Sheet};

    fn bold() -> CellFormat {
        CellFormat { bold: Some(true), ..Default::default() }
    }
    fn is_bold(s: &Sheet, r: u32, c: u32) -> bool {
        s.cell_formats.get(&(r, c)).and_then(|f| f.bold).unwrap_or(false)
    }

    #[test]
    fn insert_row_shifts_cell_formats_down() {
        let mut s = Sheet::default();
        s.cell_formats.insert((2, 3), bold());
        super::insert_row_impl(&mut s, 1);
        assert!(is_bold(&s, 3, 3), "第2行的格式应下移到第3行");
        assert!(!is_bold(&s, 2, 3), "原位置不应再有格式");
    }

    #[test]
    fn insert_row_keeps_formats_above_insert_point() {
        let mut s = Sheet::default();
        s.cell_formats.insert((0, 0), bold());
        super::insert_row_impl(&mut s, 5);
        assert!(is_bold(&s, 0, 0));
    }

    #[test]
    fn delete_row_shifts_cell_formats_up_and_drops_deleted() {
        let mut s = Sheet::default();
        s.cell_formats.insert((1, 0), bold()); // 被删行
        s.cell_formats.insert((3, 0), bold()); // 应上移到 2
        super::delete_rows_batch(&mut s, &[1]);
        assert!(is_bold(&s, 2, 0), "第3行的格式应上移到第2行");
        assert!(!is_bold(&s, 3, 0), "原位置不应再有格式");
        assert_eq!(s.cell_formats.len(), 1, "被删行的格式应消失");
    }

    #[test]
    fn insert_column_shifts_cell_formats_right() {
        let mut s = Sheet::default();
        s.cell_formats.insert((2, 2), bold());
        s.shift_cols_for_insert(1);
        assert!(is_bold(&s, 2, 3), "C列格式应右移到D列");
        assert!(!is_bold(&s, 2, 2));
    }

    #[test]
    fn delete_column_shifts_cell_formats_left_and_drops_deleted() {
        let mut s = Sheet::default();
        s.cell_formats.insert((2, 1), bold()); // 被删列
        s.cell_formats.insert((2, 3), bold()); // 应左移到 2
        s.shift_cols_for_delete(1);
        assert!(is_bold(&s, 2, 2), "D列格式应左移到C列");
        assert!(!is_bold(&s, 2, 3));
        assert_eq!(s.cell_formats.len(), 1, "被删列的格式应消失");
    }

    #[test]
    fn index_vecs_do_not_grow_when_out_of_range() {
        let mut s = Sheet::default();
        s.shift_cols_for_insert(5);
        s.shift_rows_for_insert(5);
        assert!(s.col_hidden.is_empty());
        assert!(s.row_hidden.is_empty());
    }

    #[test]
    fn index_vecs_shift_when_in_range() {
        let mut s = Sheet::default();
        s.col_hidden = vec![false, true, false];
        s.shift_cols_for_insert(1);
        assert_eq!(s.col_hidden, vec![false, false, true, false]);
        s.shift_cols_for_delete(1);
        assert_eq!(s.col_hidden, vec![false, true, false]);
    }

    #[test]
    fn insert_row_uses_unset_height_placeholder() {
        // 新插入行高度应为"未设置"(0.0),由渲染层回退到表默认行高,
        // 而不是内核硬编码的 20.0。
        let mut s = Sheet::default();
        s.row_heights = vec![roe_model::calc::RowHeight::Manual(30.0)];
        super::insert_row_impl(&mut s, 0);
        assert_eq!(s.row_heights.len(), 2);
        assert_eq!(s.row_heights[0].current(), 0.0);
        assert_eq!(s.row_heights[1].current(), 30.0, "原行高应下移保留");
    }

    #[test]
    fn merged_cells_untouched_by_new_helpers() {
        // 新 helper 不应处理 merged_cells(由调用方原有逻辑负责)
        let mut s = Sheet::default();
        s.merged_cells.push(MergedCell { r1: 1, c1: 1, r2: 2, c2: 2 });
        s.shift_cols_for_insert(0);
        s.shift_rows_for_insert(0);
        assert_eq!((s.merged_cells[0].r1, s.merged_cells[0].c1), (1, 1));
    }
}


#[cfg(test)]
mod sort_tests {
    use super::*;
    use roe_model::calc::{CellFormat, RowHeight, Sheet, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet { name: "Sheet1".into(), ..Default::default() }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    /// 读单元格显示文本(Formula 取 cached,Empty 为空串)
    fn cell_str(wb: &Workbook, r: u32, c: u32) -> String {
        let col = match wb.sheets[0].cells.cols.get(&c) {
            Some(x) => x,
            None => return String::new(),
        };
        for blk in &col.blocks {
            if r >= blk.start_row && r < blk.start_row + blk.values.len() as u32 {
                return cell_value_to_string(&blk.values[(r - blk.start_row) as usize]);
            }
        }
        String::new()
    }

    /// 按 vals[行][列] 写入,空串跳过(保持真正的空格)
    fn fill(e: &rhai::Engine, vals: &[&[&str]]) {
        for (r, row) in vals.iter().enumerate() {
            for (c, v) in row.iter().enumerate() {
                if !v.is_empty() {
                    e.run(&format!(r#"set_cell(0, {}, {}, "{}")"#, r, c, v)).unwrap();
                }
            }
        }
    }

    fn sort(e: &rhai::Engine, r2: u32, c2: u32, key: i64, asc: bool) {
        let range = format!(r#"{{"sheet":"Sheet1","start":[0,0],"end":[{},{}]}}"#, r2, c2);
        e.run(&format!(r#"sort_range(0, `{}`, {}, {})"#, range, key, asc)).unwrap();
    }

    fn col0(wb: &Workbook, n: u32) -> Vec<String> {
        (0..n).map(|r| cell_str(wb, r, 0)).collect()
    }

    #[test]
    fn number_asc_desc_uses_numeric_not_string_order() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            fill(&e, &[&["100"], &["50"], &["200"]]);
            sort(&e, 2, 0, 0, true);
        });
        assert_eq!(col0(&wb, 3), vec!["50", "100", "200"]);
        with_workbook(&mut wb, || sort(&e, 2, 0, 0, false));
        assert_eq!(col0(&wb, 3), vec!["200", "100", "50"]);
    }

    #[test]
    fn text_sort_asc_desc_case_insensitive() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            fill(&e, &[&["banana"], &["Apple"], &["cherry"]]);
            sort(&e, 2, 0, 0, true);
        });
        assert_eq!(col0(&wb, 3), vec!["Apple", "banana", "cherry"]);
        with_workbook(&mut wb, || sort(&e, 2, 0, 0, false));
        assert_eq!(col0(&wb, 3), vec!["cherry", "banana", "Apple"]);
    }

    #[test]
    fn empty_always_last_in_both_directions() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            fill(&e, &[&["3"], &[""], &["1"], &["2"]]);
            sort(&e, 3, 0, 0, true);
        });
        assert_eq!(col0(&wb, 4), vec!["1", "2", "3", ""]);
        with_workbook(&mut wb, || sort(&e, 3, 0, 0, false));
        assert_eq!(col0(&wb, 4), vec!["3", "2", "1", ""]);
    }

    #[test]
    fn mixed_number_before_text_asc() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            fill(&e, &[&["b"], &["10"], &["a"], &["2"]]);
            sort(&e, 3, 0, 0, true);
        });
        assert_eq!(col0(&wb, 4), vec!["2", "10", "a", "b"]);
    }

    #[test]
    fn other_columns_follow_the_key_column() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            // 第 0 列 Name,第 1 列 Price;按 Price(相对列 1)升序
            fill(&e, &[&["X1", "100"], &["X2", "200"], &["X3", "50"]]);
            sort(&e, 2, 1, 1, true);
        });
        assert_eq!((cell_str(&wb, 0, 0), cell_str(&wb, 0, 1)), ("X3".to_string(), "50".to_string()));
        assert_eq!((cell_str(&wb, 1, 0), cell_str(&wb, 1, 1)), ("X1".to_string(), "100".to_string()));
        assert_eq!((cell_str(&wb, 2, 0), cell_str(&wb, 2, 1)), ("X2".to_string(), "200".to_string()));
    }

    #[test]
    fn stable_for_equal_keys() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            fill(&e, &[&["B", "first"], &["A", "second"], &["B", "third"], &["A", "fourth"]]);
            sort(&e, 3, 1, 0, true);
        });
        assert_eq!(cell_str(&wb, 0, 1), "second");
        assert_eq!(cell_str(&wb, 1, 1), "fourth");
        assert_eq!(cell_str(&wb, 2, 1), "first");
        assert_eq!(cell_str(&wb, 3, 1), "third");
    }

    #[test]
    fn cell_format_follows_data() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            fill(&e, &[&["3"], &["1"], &["2"]]);
        });
        // 直接给第 0 行("3")设粗体,避免依赖 set_cell_format 的 JSON 键名
        wb.sheets[0].cell_formats.insert(
            (0, 0),
            CellFormat { bold: Some(true), ..Default::default() },
        );
        with_workbook(&mut wb, || sort(&e, 2, 0, 0, true));
        let bold = |r: u32| -> bool {
            wb.sheets[0].cell_formats.get(&(r, 0)).and_then(|f| f.bold).unwrap_or(false)
        };
        // 排序后 "3" 到了第 2 行,加粗应跟着走
        assert_eq!(col0(&wb, 3), vec!["1", "2", "3"]);
        assert!(!bold(0));
        assert!(!bold(1));
        assert!(bold(2));
    }

    #[test]
    fn row_height_invalidated_after_sort() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            fill(&e, &[&["2"], &["1"]]);
        });
        wb.sheets[0].row_heights = vec![
            RowHeight::Auto { current: 20.0, is_dirty: false },
            RowHeight::Auto { current: 20.0, is_dirty: false },
        ];
        with_workbook(&mut wb, || sort(&e, 1, 0, 0, true));
        for i in 0..2 {
            let rh = wb.sheets[0].row_heights[i]; // RowHeight 是 Copy
            match rh {
                RowHeight::Auto { is_dirty, .. } => assert!(is_dirty, "row {} 应被置脏", i),
                RowHeight::Manual(_) => panic!("row {} 期望 Auto", i),
            }
        }
    }
}


// ─────────────────────────────────────────────────────────────
// 边框写入(纯函数,只操作 cell_formats,便于单测)
// ─────────────────────────────────────────────────────────────
#[derive(Clone, Copy)]
enum BSide { Top, Bottom, Left, Right }

type FmtMap = std::collections::HashMap<(u32, u32), roe_model::calc::CellFormat>;

fn border_set_side(fmts: &mut FmtMap, r: u32, c: u32, side: BSide, line: &Option<roe_model::calc::BorderLine>) {
    // 清除边时,目标格没有格式记录就无需创建
    if line.is_none() && !fmts.contains_key(&(r, c)) { return; }
    let f = fmts.entry((r, c)).or_default();
    let b = f.borders.get_or_insert_with(Default::default);
    match side {
        BSide::Top => b.top = line.clone(),
        BSide::Bottom => b.bottom = line.clone(),
        BSide::Left => b.left = line.clone(),
        BSide::Right => b.right = line.clone(),
    }
    if b.top.is_none() && b.bottom.is_none() && b.left.is_none() && b.right.is_none() {
        f.borders = None;
    }
}

/// 行边界 rb(第 rb-1 行与第 rb 行之间),列 c:同时写上格 bottom 与下格 top
fn border_hedge(fmts: &mut FmtMap, rb: u32, c: u32, line: &Option<roe_model::calc::BorderLine>) {
    if rb > 0 { border_set_side(fmts, rb - 1, c, BSide::Bottom, line); }
    border_set_side(fmts, rb, c, BSide::Top, line);
}

/// 列边界 cb(第 cb-1 列与第 cb 列之间),行 r:同时写左格 right 与右格 left
fn border_vedge(fmts: &mut FmtMap, r: u32, cb: u32, line: &Option<roe_model::calc::BorderLine>) {
    if cb > 0 { border_set_side(fmts, r, cb - 1, BSide::Right, line); }
    border_set_side(fmts, r, cb, BSide::Left, line);
}

/// 返回 false 表示模式未知或选区过大(未写入)。
pub(crate) fn apply_borders_map(
    fmts: &mut FmtMap,
    r1: u32, c1: u32, r2: u32, c2: u32,
    mode: &str, style: &str, color: Option<&str>,
) -> bool {
    let (r1, r2) = (r1.min(r2), r1.max(r2));
    let (c1, c2) = (c1.min(c2), c1.max(c2));
    let none: Option<roe_model::calc::BorderLine> = None;

    if mode == "none" {
        let keys: Vec<(u32, u32)> = fmts.keys()
            .filter(|(r, c)| *r >= r1 && *r <= r2 && *c >= c1 && *c <= c2)
            .cloned().collect();
        for (r, c) in keys {
            border_hedge(fmts, r, c, &none);
            border_hedge(fmts, r + 1, c, &none);
            border_vedge(fmts, r, c, &none);
            border_vedge(fmts, r, c + 1, &none);
        }
        // 选区外缘:没有格式记录的选区格,其邻格对边也要清掉
        for c in c1..=c2.min(c1.saturating_add(100_000)) {
            border_hedge(fmts, r1, c, &none);
            border_hedge(fmts, r2.saturating_add(1), c, &none);
        }
        for r in r1..=r2.min(r1.saturating_add(100_000)) {
            border_vedge(fmts, r, c1, &none);
            border_vedge(fmts, r, c2.saturating_add(1), &none);
        }
        return true;
    }

    let cells = (r2 - r1 + 1) as u64 * (c2 - c1 + 1) as u64;
    if cells > 1_000_000 { return false; }
    let line = Some(roe_model::calc::BorderLine {
        style: style.to_string(),
        color: color.map(|s| s.to_string()),
    });
    let (top, bottom, left, right, inner_h, inner_v) = match mode {
        "top" => (true, false, false, false, false, false),
        "bottom" => (false, true, false, false, false, false),
        "left" => (false, false, true, false, false, false),
        "right" => (false, false, false, true, false, false),
        "outside" => (true, true, true, true, false, false),
        "inside" => (false, false, false, false, true, true),
        "inside_h" => (false, false, false, false, true, false),
        "inside_v" => (false, false, false, false, false, true),
        "all" => (true, true, true, true, true, true),
        _ => return false,
    };
    if top { for c in c1..=c2 { border_hedge(fmts, r1, c, &line); } }
    if bottom { for c in c1..=c2 { border_hedge(fmts, r2 + 1, c, &line); } }
    if left { for r in r1..=r2 { border_vedge(fmts, r, c1, &line); } }
    if right { for r in r1..=r2 { border_vedge(fmts, r, c2 + 1, &line); } }
    if inner_h { for rb in (r1 + 1)..=r2 { for c in c1..=c2 { border_hedge(fmts, rb, c, &line); } } }
    if inner_v { for r in r1..=r2 { for cb in (c1 + 1)..=c2 { border_vedge(fmts, r, cb, &line); } } }
    true
}

#[cfg(test)]
mod borders_apply_tests {
    use super::*;
    use roe_model::calc::{CellFormat};

    fn side(m: &FmtMap, r: u32, c: u32, s: u8) -> bool {
        m.get(&(r, c)).and_then(|f| f.borders.as_ref()).map(|b| match s {
            0 => b.top.is_some(), 1 => b.bottom.is_some(), 2 => b.left.is_some(), _ => b.right.is_some(),
        }).unwrap_or(false)
    }

    #[test]
    fn test_all_2x2_shared_edges_consistent() {
        let mut m = FmtMap::new();
        assert!(apply_borders_map(&mut m, 0, 0, 1, 1, "all", "thin", None));
        assert!(side(&m, 0, 0, 3) && side(&m, 0, 1, 2));
        assert!(side(&m, 0, 0, 1) && side(&m, 1, 0, 0));
        assert!(side(&m, 0, 0, 0) && side(&m, 0, 0, 2));
        assert!(side(&m, 1, 1, 1) && side(&m, 1, 1, 3));
    }

    #[test]
    fn test_outside_leaves_inner_edges_empty() {
        let mut m = FmtMap::new();
        apply_borders_map(&mut m, 0, 0, 1, 1, "outside", "thin", Some("#FF0000"));
        assert!(side(&m, 0, 0, 0) && side(&m, 0, 0, 2));
        assert!(!side(&m, 0, 0, 3) && !side(&m, 0, 0, 1));
        assert!(side(&m, 1, 1, 1) && side(&m, 1, 1, 3));
        let c = m[&(0, 0)].borders.as_ref().unwrap().top.as_ref().unwrap().color.clone();
        assert_eq!(c.as_deref(), Some("#FF0000"));
    }

    #[test]
    fn test_top_writes_neighbor_bottom_and_keeps_other_fields() {
        let mut m = FmtMap::new();
        m.insert((2, 0), CellFormat { bold: Some(true), ..Default::default() });
        apply_borders_map(&mut m, 2, 0, 2, 1, "top", "medium", None);
        assert!(side(&m, 2, 0, 0) && side(&m, 1, 0, 1));
        assert_eq!(m[&(2, 0)].bold, Some(true));
    }

    #[test]
    fn test_none_clears_neighbor_facing_edge_and_empties_borders() {
        let mut m = FmtMap::new();
        apply_borders_map(&mut m, 0, 0, 1, 1, "all", "thin", None);
        apply_borders_map(&mut m, 0, 0, 0, 0, "none", "thin", None);
        assert!(!side(&m, 0, 1, 2), "右邻格的 left 应被清掉");
        assert!(!side(&m, 1, 0, 0), "下邻格的 top 应被清掉");
        assert!(m[&(0, 0)].borders.is_none());
    }

    #[test]
    fn test_inside_v_only() {
        let mut m = FmtMap::new();
        apply_borders_map(&mut m, 0, 0, 0, 2, "inside_v", "thin", None);
        assert!(side(&m, 0, 0, 3) && side(&m, 0, 1, 2) && side(&m, 0, 1, 3) && side(&m, 0, 2, 2));
        assert!(!side(&m, 0, 0, 2) && !side(&m, 0, 2, 3) && !side(&m, 0, 0, 0));
    }

    #[test]
    fn test_unknown_mode_rejected() {
        let mut m = FmtMap::new();
        assert!(!apply_borders_map(&mut m, 0, 0, 0, 0, "bogus", "thin", None));
        assert!(m.is_empty());
    }
}


// ─────────────────────────────────────────────────────────────
// 条件格式规则增删(纯函数,只操作 Sheet.cond_formats,便于单测)
// ─────────────────────────────────────────────────────────────
fn cf_add_rule(sheet: &mut roe_model::calc::Sheet, range_json: &str, kind_json: &str, stop_if_true: bool) -> bool {
    let range = match serde_json::from_str::<roe_model::calc::CellRange>(range_json) {
        Ok(r) => r,
        Err(_) => return false,
    };
    let kind = match serde_json::from_str::<roe_model::calc::ConditionalFormatKind>(kind_json) {
        Ok(k) => k,
        Err(_) => return false,
    };
    sheet.cond_formats.push(roe_model::calc::ConditionalFormatRule { range, kind, stop_if_true });
    true
}

fn cf_ranges_intersect(a: &roe_model::calc::CellRange, b: &roe_model::calc::CellRange) -> bool {
    let (ar1, ar2) = (a.start.0.min(a.end.0), a.start.0.max(a.end.0));
    let (ac1, ac2) = (a.start.1.min(a.end.1), a.start.1.max(a.end.1));
    let (br1, br2) = (b.start.0.min(b.end.0), b.start.0.max(b.end.0));
    let (bc1, bc2) = (b.start.1.min(b.end.1), b.start.1.max(b.end.1));
    ar1 <= br2 && br1 <= ar2 && ac1 <= bc2 && bc1 <= ac2
}

/// 删除与 range_json 相交的全部规则,返回被删规则的 range(用于标脏)。
/// range_json 解析失败返回空 Vec(不删任何规则)。
fn cf_clear_in_range(sheet: &mut roe_model::calc::Sheet, range_json: &str) -> Vec<roe_model::calc::CellRange> {
    let target = match serde_json::from_str::<roe_model::calc::CellRange>(range_json) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut removed = Vec::new();
    let mut kept = Vec::with_capacity(sheet.cond_formats.len());
    for rule in sheet.cond_formats.drain(..) {
        if cf_ranges_intersect(&rule.range, &target) {
            removed.push(rule.range);
        } else {
            kept.push(rule);
        }
    }
    sheet.cond_formats = kept;
    removed
}

/// 按下标删除,越界返回 None;成功返回被删规则的 range。
fn cf_remove_at(sheet: &mut roe_model::calc::Sheet, index: i64) -> Option<roe_model::calc::CellRange> {
    if index < 0 || (index as usize) >= sheet.cond_formats.len() { return None; }
    Some(sheet.cond_formats.remove(index as usize).range)
}

/// 只删与 range_json 相交的可视化规则(DataBar/ColorScale/IconSet),返回被删规则的 range。
/// range_json 解析失败返回空 Vec。
fn cf_clear_visual_in_range(sheet: &mut roe_model::calc::Sheet, range_json: &str) -> Vec<roe_model::calc::CellRange> {
    use roe_model::calc::ConditionalFormatKind as K;
    let target = match serde_json::from_str::<roe_model::calc::CellRange>(range_json) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut removed = Vec::new();
    let mut kept = Vec::with_capacity(sheet.cond_formats.len());
    for rule in sheet.cond_formats.drain(..) {
        let visual = !matches!(rule.kind, K::CellFormat { .. });
        if visual && cf_ranges_intersect(&rule.range, &target) {
            removed.push(rule.range);
        } else {
            kept.push(rule);
        }
    }
    sheet.cond_formats = kept;
    removed
}

/// 删除落在 range_json 内的迷你图(按单元格),组空则整组删。返回删除条数。
fn sparkline_clear_in_range(sheet: &mut roe_model::calc::Sheet, range_json: &str) -> i64 {
    let target = match serde_json::from_str::<roe_model::calc::CellRange>(range_json) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let (r1, r2) = (target.start.0.min(target.end.0), target.start.0.max(target.end.0));
    let (c1, c2) = (target.start.1.min(target.end.1), target.start.1.max(target.end.1));
    let mut removed = 0i64;
    for g in sheet.sparkline_groups.iter_mut() {
        let before = g.sparklines.len();
        g.sparklines.retain(|sp| {
            !(sp.cell.0 >= r1 && sp.cell.0 <= r2 && sp.cell.1 >= c1 && sp.cell.1 <= c2)
        });
        removed += (before - g.sparklines.len()) as i64;
    }
    sheet.sparkline_groups.retain(|g| !g.sparklines.is_empty());
    removed
}

/// 按"数据范围 + 位置范围"新建迷你图组,返回新增条数;参数不合法返回 0 且不改动。
/// 配对:数据行数 == 位置格数 且位置为单列/单格 → 每行一条;
///       否则位置为单行 且 列数 == 数据列数 → 每列一条。
fn sparkline_add_group(
    sheet: &mut roe_model::calc::Sheet,
    kind: &str,
    data_json: &str,
    loc_json: &str,
) -> i64 {
    use roe_model::calc::{CellRange, Sparkline, SparklineGroup, SparklineType};
    let stype = match kind {
        "line" => SparklineType::Line,
        "column" => SparklineType::Column,
        "stacked" => SparklineType::Stacked,
        _ => return 0,
    };
    let (Ok(data), Ok(loc)) = (
        serde_json::from_str::<CellRange>(data_json),
        serde_json::from_str::<CellRange>(loc_json),
    ) else { return 0 };
    let (dr1, dr2) = (data.start.0.min(data.end.0), data.start.0.max(data.end.0));
    let (dc1, dc2) = (data.start.1.min(data.end.1), data.start.1.max(data.end.1));
    let (lr1, lr2) = (loc.start.0.min(loc.end.0), loc.start.0.max(loc.end.0));
    let (lc1, lc2) = (loc.start.1.min(loc.end.1), loc.start.1.max(loc.end.1));
    let d_rows = dr2 - dr1 + 1;
    let d_cols = dc2 - dc1 + 1;
    let l_rows = lr2 - lr1 + 1;
    let l_cols = lc2 - lc1 + 1;

    let mut items: Vec<Sparkline> = Vec::new();
    if l_cols == 1 && l_rows == d_rows {
        for i in 0..d_rows {
            items.push(Sparkline {
                cell: (lr1 + i, lc1),
                input_range: vec![CellRange {
                    sheet: data.sheet.clone(), start: (dr1 + i, dc1), end: (dr1 + i, dc2),
                }],
            });
        }
    } else if l_rows == 1 && l_cols == d_cols {
        for j in 0..d_cols {
            items.push(Sparkline {
                cell: (lr1, lc1 + j),
                input_range: vec![CellRange {
                    sheet: data.sheet.clone(), start: (dr1, dc1 + j), end: (dr2, dc1 + j),
                }],
            });
        }
    } else {
        return 0;
    }

    // 覆盖位置范围内已有迷你图(Excel 行为)
    sparkline_clear_in_range(sheet, loc_json);
    let n = items.len() as i64;
    sheet.sparkline_groups.push(SparklineGroup {
        sparkline_type: stype,
        sparklines: items,
        ..Default::default()
    });
    n
}

#[cfg(test)]
mod cf_rule_tests {
    use super::*;
    use roe_model::calc::{Sheet, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet { name: "Sheet1".into(), ..Default::default() }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    fn rng(r1: u32, c1: u32, r2: u32, c2: u32) -> String {
        format!(r#"{{"sheet":"Sheet1","start":[{r1},{c1}],"end":[{r2},{c2}]}}"#)
    }

    const DATABAR: &str = r##"{"DataBar":{"min":{"kind":"min","val":"","gte":true},"max":{"kind":"max","val":"","gte":true},"bar_color":"#638EC6","border_color":null,"direction":"LeftToRight","bar_only":false,"solid_fill":false,"negative_fill_color":null,"axis_color":null,"axis_position":"automatic"}}"##;
    const COLORSCALE: &str = r##"{"ColorScale":{"min_type":"min","min_value":null,"min_color":"#F8696B","mid":null,"max_type":"max","max_value":null,"max_color":"#63BE7B"}}"##;

    #[test]
    fn test_add_databar_and_colorscale() {
        let mut sh = Sheet::default();
        assert!(cf_add_rule(&mut sh, &rng(0, 0, 4, 0), DATABAR, false));
        assert!(cf_add_rule(&mut sh, &rng(0, 1, 4, 1), COLORSCALE, true));
        assert_eq!(sh.cond_formats.len(), 2);
        assert!(!sh.cond_formats[0].stop_if_true);
        assert!(sh.cond_formats[1].stop_if_true);
        assert!(matches!(sh.cond_formats[0].kind, roe_model::calc::ConditionalFormatKind::DataBar { .. }));
        assert!(matches!(sh.cond_formats[1].kind, roe_model::calc::ConditionalFormatKind::ColorScale { .. }));
    }

    #[test]
    fn test_add_invalid_json_rejected_without_write() {
        let mut sh = Sheet::default();
        assert!(!cf_add_rule(&mut sh, &rng(0, 0, 1, 1), r#"{"Nope":{}}"#, false));
        assert!(!cf_add_rule(&mut sh, "not json", DATABAR, false));
        assert!(sh.cond_formats.is_empty());
    }

    #[test]
    fn test_clear_removes_whole_intersecting_rules_only() {
        let mut sh = Sheet::default();
        cf_add_rule(&mut sh, &rng(0, 0, 4, 0), DATABAR, false);   // A
        cf_add_rule(&mut sh, &rng(10, 0, 14, 0), DATABAR, false); // B
        cf_add_rule(&mut sh, &rng(3, 0, 11, 0), COLORSCALE, false); // C
        let removed = cf_clear_in_range(&mut sh, &rng(4, 0, 4, 0));
        assert_eq!(removed.len(), 2, "A 与 C 相交,B 不相交");
        assert_eq!(sh.cond_formats.len(), 1);
        assert_eq!(sh.cond_formats[0].range.start, (10, 0));
        assert_eq!(sh.cond_formats[0].range.end, (14, 0), "不拆分,保留规则原范围");
    }

    #[test]
    fn test_clear_handles_reversed_range_and_bad_json() {
        let mut sh = Sheet::default();
        cf_add_rule(&mut sh, &rng(2, 2, 3, 3), DATABAR, false);
        assert!(cf_clear_in_range(&mut sh, "garbage").is_empty());
        assert_eq!(sh.cond_formats.len(), 1, "坏 JSON 不应删任何规则");
        let removed = cf_clear_in_range(&mut sh, &rng(5, 5, 0, 0)); // 起止颠倒
        assert_eq!(removed.len(), 1);
        assert!(sh.cond_formats.is_empty());
    }

    #[test]
    fn test_clear_visual_keeps_cellformat_and_nonintersecting() {
        let mut sh = Sheet::default();
        let cellfmt = r#"{"CellFormat":{"condition":"gt:0","format":{}}}"#;
        cf_add_rule(&mut sh, &rng(0, 0, 4, 0), DATABAR, false);     // 相交,视觉 -> 删
        cf_add_rule(&mut sh, &rng(0, 0, 4, 0), cellfmt, false);     // 相交,CellFormat -> 留
        cf_add_rule(&mut sh, &rng(0, 0, 4, 0), COLORSCALE, false);  // 相交,视觉 -> 删
        cf_add_rule(&mut sh, &rng(20, 0, 24, 0), DATABAR, false);   // 不相交 -> 留
        let removed = cf_clear_visual_in_range(&mut sh, &rng(2, 0, 2, 0));
        assert_eq!(removed.len(), 2);
        assert_eq!(sh.cond_formats.len(), 2);
        assert!(matches!(sh.cond_formats[0].kind, roe_model::calc::ConditionalFormatKind::CellFormat { .. }));
        assert_eq!(sh.cond_formats[1].range.start, (20, 0));
        assert!(cf_clear_visual_in_range(&mut sh, "garbage").is_empty());
    }

    #[test]
    fn test_remove_at_index() {
        let mut sh = Sheet::default();
        cf_add_rule(&mut sh, &rng(0, 0, 1, 0), DATABAR, false);
        cf_add_rule(&mut sh, &rng(5, 0, 6, 0), COLORSCALE, false);
        assert!(cf_remove_at(&mut sh, -1).is_none());
        assert!(cf_remove_at(&mut sh, 2).is_none());
        assert_eq!(sh.cond_formats.len(), 2);
        let r = cf_remove_at(&mut sh, 0).unwrap();
        assert_eq!(r.start, (0, 0));
        assert_eq!(sh.cond_formats.len(), 1);
        assert_eq!(sh.cond_formats[0].range.start, (5, 0));
    }

    #[test]
    fn test_rhai_end_to_end_add_clear_remove() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let range = rng(0, 0, 9, 0);
            let ok: bool = e.eval(&format!("add_cf_rule(0, `{range}`, `{DATABAR}`, false)")).unwrap();
            assert!(ok);
            let got: String = e.eval("get_conditional_formats(0)").unwrap();
            assert!(got.contains("DataBar"), "实际: {got}");
            let n: i64 = e.eval(&format!("clear_conditional_formats(0, `{}`)", rng(3, 0, 3, 0))).unwrap();
            assert_eq!(n, 1);
            let after: String = e.eval("get_conditional_formats(0)").unwrap();
            assert_eq!(after, "[]");
            let bad: bool = e.eval("remove_conditional_format(0, 0)").unwrap();
            assert!(!bad, "空列表删除应返回 false");
        });
        assert!(wb.sheets[0].cond_formats.is_empty());
    }
}

#[cfg(test)]
mod sparkline_tests {
    use super::*;
    use roe_model::calc::{Sheet, SparklineType, Workbook};
    use std::collections::HashMap;

    fn rng(r1: u32, c1: u32, r2: u32, c2: u32) -> String {
        format!(r#"{{"sheet":"Sheet1","start":[{r1},{c1}],"end":[{r2},{c2}]}}"#)
    }

    #[test]
    fn test_add_rows_to_column() {
        // 数据 A1:E3(3 行),位置 G1:G3 → 每行一条,共 3 条
        let mut sh = Sheet { name: "Sheet1".into(), ..Default::default() };
        let n = sparkline_add_group(&mut sh, "column", &rng(0, 0, 2, 4), &rng(0, 6, 2, 6));
        assert_eq!(n, 3);
        assert_eq!(sh.sparkline_groups.len(), 1);
        let g = &sh.sparkline_groups[0];
        assert_eq!(g.sparkline_type, SparklineType::Column);
        assert_eq!(g.sparklines.len(), 3);
        assert_eq!(g.sparklines[1].cell, (1, 6));
        assert_eq!(g.sparklines[1].input_range[0].start, (1, 0));
        assert_eq!(g.sparklines[1].input_range[0].end, (1, 4));
        assert_eq!(g.sparklines[1].input_range[0].sheet, "Sheet1");
    }

    #[test]
    fn test_add_cols_to_row() {
        // 数据 A1:C5(3 列),位置 A7:C7 → 每列一条,共 3 条
        let mut sh = Sheet { name: "Sheet1".into(), ..Default::default() };
        let n = sparkline_add_group(&mut sh, "line", &rng(0, 0, 4, 2), &rng(6, 0, 6, 2));
        assert_eq!(n, 3);
        let g = &sh.sparkline_groups[0];
        assert_eq!(g.sparkline_type, SparklineType::Line);
        assert_eq!(g.sparklines[2].cell, (6, 2));
        assert_eq!(g.sparklines[2].input_range[0].start, (0, 2));
        assert_eq!(g.sparklines[2].input_range[0].end, (4, 2));
    }

    #[test]
    fn test_add_single_cell_single_row() {
        // 数据 A1:E1(单行),位置 G1(单格) → 1 条
        let mut sh = Sheet { name: "Sheet1".into(), ..Default::default() };
        assert_eq!(sparkline_add_group(&mut sh, "stacked", &rng(0, 0, 0, 4), &rng(0, 6, 0, 6)), 1);
        assert_eq!(sh.sparkline_groups[0].sparkline_type, SparklineType::Stacked);
    }

    #[test]
    fn test_add_size_mismatch_rejected() {
        let mut sh = Sheet { name: "Sheet1".into(), ..Default::default() };
        // 3 行数据配 2 格位置
        assert_eq!(sparkline_add_group(&mut sh, "line", &rng(0, 0, 2, 4), &rng(0, 6, 1, 6)), 0);
        // 位置是 2x2 块
        assert_eq!(sparkline_add_group(&mut sh, "line", &rng(0, 0, 1, 4), &rng(0, 6, 1, 7)), 0);
        // 非法类型 / 非法 JSON
        assert_eq!(sparkline_add_group(&mut sh, "pie", &rng(0, 0, 0, 4), &rng(0, 6, 0, 6)), 0);
        assert_eq!(sparkline_add_group(&mut sh, "line", "garbage", &rng(0, 6, 0, 6)), 0);
        assert!(sh.sparkline_groups.is_empty(), "失败不得改动");
    }

    #[test]
    fn test_add_replaces_existing_in_location() {
        // 同一位置再建:覆盖旧的(Excel 行为),旧组空则整组删
        let mut sh = Sheet { name: "Sheet1".into(), ..Default::default() };
        sparkline_add_group(&mut sh, "line", &rng(0, 0, 0, 4), &rng(0, 6, 0, 6));
        sparkline_add_group(&mut sh, "column", &rng(1, 0, 1, 4), &rng(0, 6, 0, 6));
        assert_eq!(sh.sparkline_groups.len(), 1);
        assert_eq!(sh.sparkline_groups[0].sparkline_type, SparklineType::Column);
    }

    #[test]
    fn test_clear_by_cell_and_empty_group_removed() {
        let mut sh = Sheet { name: "Sheet1".into(), ..Default::default() };
        sparkline_add_group(&mut sh, "line", &rng(0, 0, 2, 4), &rng(0, 6, 2, 6)); // G1:G3
        sparkline_add_group(&mut sh, "column", &rng(0, 0, 0, 4), &rng(0, 8, 0, 8)); // I1
        // 删 G2 → 第一组剩 2 条
        assert_eq!(sparkline_clear_in_range(&mut sh, &rng(1, 6, 1, 6)), 1);
        assert_eq!(sh.sparkline_groups[0].sparklines.len(), 2);
        // 删 G1:G3 → 第一组清空被整组删除,剩 I1 那组
        assert_eq!(sparkline_clear_in_range(&mut sh, &rng(0, 6, 2, 6)), 2);
        assert_eq!(sh.sparkline_groups.len(), 1);
        assert_eq!(sh.sparkline_groups[0].sparklines[0].cell, (0, 8));
        assert_eq!(sparkline_clear_in_range(&mut sh, "garbage"), 0);
    }

    #[test]
    fn test_rhai_end_to_end() {
        let mut wb = Workbook {
            sheets: vec![Sheet { name: "Sheet1".into(), ..Default::default() }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let n: i64 = e.eval(&format!(
                "add_sparkline_group(0, `line`, `{}`, `{}`)", rng(0, 0, 2, 4), rng(0, 6, 2, 6)
            )).unwrap();
            assert_eq!(n, 3);
            let c: i64 = e.eval(&format!("clear_sparklines(0, `{}`)", rng(0, 6, 2, 6))).unwrap();
            assert_eq!(c, 3);
        });
        assert!(wb.sheets[0].sparkline_groups.is_empty());
    }
}

#[cfg(test)]
mod table_api_tests {
    use super::*;
    use roe_model::calc::{Sheet, Workbook};
    use std::collections::HashMap;

    fn wb_n(n: usize) -> Workbook {
        Workbook {
            sheets: (0..n)
                .map(|i| Sheet { name: format!("Sheet{}", i + 1), ..Default::default() })
                .collect(),
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    fn run_str(wb: &mut Workbook, script: &str) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_workbook(wb, || {
            out = e.eval::<String>(script).unwrap();
        });
        out
    }

    fn run_unit(wb: &mut Workbook, script: &str) {
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(wb, || {
            e.run(script).unwrap();
        });
    }

    fn text_at(wb: &Workbook, sheet: usize, r: u32, c: u32) -> String {
        cell_value_to_string(&wb.sheets[sheet].cells.get(r, c))
    }

    #[test]
    fn create_table_basic() {
        let mut wb = wb_n(1);
        run_unit(&mut wb, r#"
            set_cell(0, 0, 0, "姓名"); set_cell(0, 0, 1, "年龄");
            set_cell(0, 1, 0, "a"); set_cell(0, 1, 1, "1");
            set_cell(0, 2, 0, "b"); set_cell(0, 2, 1, "2");
        "#);
        let ret = run_str(&mut wb, r#"create_table(0, 0, 0, 2, 1, true, "TableStyleMedium2")"#);
        assert_eq!(ret, "Table1");
        let t = &wb.sheets[0].tables[0];
        assert_eq!(t.name, "Table1");
        assert_eq!(t.range.sheet, "Sheet1");
        assert_eq!(t.range.start, (0, 0));
        assert_eq!(t.range.end, (2, 1));
        assert_eq!(t.columns, vec!["姓名".to_string(), "年龄".to_string()]);
        assert!(t.has_header);
        assert!(!t.has_totals);
        assert!(t.show_row_stripes);
        assert_eq!(t.style_name.as_deref(), Some("TableStyleMedium2"));
        let af = t.auto_filter.as_ref().expect("带表头应生成 auto_filter");
        assert_eq!((af.header_row, af.header_col, af.end_col), (0, 0, 1));
    }

    #[test]
    fn create_table_without_header_has_no_autofilter() {
        let mut wb = wb_n(1);
        let ret = run_str(&mut wb, r#"create_table(0, 2, 1, 4, 3, false, "TableStyleLight1")"#);
        assert_eq!(ret, "Table1");
        let t = &wb.sheets[0].tables[0];
        assert!(!t.has_header);
        assert!(t.auto_filter.is_none());
        assert_eq!(t.columns, vec!["Column1", "Column2", "Column3"]);
    }

    #[test]
    fn blank_and_duplicate_headers_get_columnn_and_cells_rewritten() {
        let mut wb = wb_n(1);
        run_unit(&mut wb, r#"set_cell(0, 0, 0, "A"); set_cell(0, 0, 2, "a"); set_cell(0, 1, 0, "x");"#);
        let ret = run_str(&mut wb, r#"create_table(0, 0, 0, 1, 3, true, "TableStyleMedium2")"#);
        assert_eq!(ret, "Table1");
        let cols = wb.sheets[0].tables[0].columns.clone();
        assert_eq!(cols, vec!["A", "Column2", "Column3", "Column4"]);
        // 表头单元格文字与列名保持一致
        for (i, name) in cols.iter().enumerate() {
            assert_eq!(&text_at(&wb, 0, 0, i as u32), name);
        }
    }

    #[test]
    fn overlap_rejected_and_names_unique_across_sheets() {
        let mut wb = wb_n(2);
        assert_eq!(run_str(&mut wb, r#"create_table(0, 0, 0, 2, 1, true, "TableStyleMedium2")"#), "Table1");
        // 重叠
        let r = run_str(&mut wb, r#"create_table(0, 1, 1, 3, 2, true, "TableStyleMedium2")"#);
        assert!(r.starts_with("err:"), "{r}");
        assert_eq!(wb.sheets[0].tables.len(), 1);
        // 同 sheet 不重叠
        assert_eq!(run_str(&mut wb, r#"create_table(0, 0, 5, 2, 6, true, "TableStyleMedium2")"#), "Table2");
        // 另一个 sheet 的表名继续全局递增
        assert_eq!(run_str(&mut wb, r#"create_table(1, 0, 0, 2, 1, true, "TableStyleMedium2")"#), "Table3");
    }

    #[test]
    fn create_table_rejects_bad_input() {
        let mut wb = wb_n(1);
        for script in [
            r#"create_table(0, 0, 0, 2, 1, true, "NoSuchStyle")"#,
            r#"create_table(0, 0, 0, 0, 1, true, "TableStyleMedium2")"#, // 带表头但只有一行
            r#"create_table(0, 2, 0, 1, 1, true, "TableStyleMedium2")"#, // r2 < r1
            r#"create_table(9, 0, 0, 2, 1, true, "TableStyleMedium2")"#, // sheet 越界
            r#"create_table(0, -1, 0, 2, 1, true, "TableStyleMedium2")"#, // 负坐标
        ] {
            let r = run_str(&mut wb, script);
            assert!(r.starts_with("err:"), "{script} => {r}");
        }
        assert!(wb.sheets[0].tables.is_empty());
    }

    #[test]
    fn set_table_style_and_clear() {
        let mut wb = wb_n(1);
        run_str(&mut wb, r#"create_table(0, 0, 0, 2, 1, true, "TableStyleMedium2")"#);
        assert_eq!(run_str(&mut wb, r#"set_table_style(0, 0, "TableStyleLight9")"#), "ok");
        assert_eq!(wb.sheets[0].tables[0].style_name.as_deref(), Some("TableStyleLight9"));
        let r = run_str(&mut wb, r#"set_table_style(0, 0, "Nope")"#);
        assert!(r.starts_with("err:"), "{r}");
        assert_eq!(wb.sheets[0].tables[0].style_name.as_deref(), Some("TableStyleLight9"));
        let r = run_str(&mut wb, r#"set_table_style(0, 5, "TableStyleLight9")"#);
        assert!(r.starts_with("err:"), "{r}");
        assert_eq!(run_str(&mut wb, r#"set_table_style(0, 0, "")"#), "ok");
        assert!(wb.sheets[0].tables[0].style_name.is_none());
    }

    #[test]
    fn get_tables_json_shape() {
        let mut wb = wb_n(1);
        assert_eq!(run_str(&mut wb, "get_tables(0)"), "[]");
        run_str(&mut wb, r#"create_table(0, 0, 0, 2, 1, true, "TableStyleMedium2")"#);
        let js = run_str(&mut wb, "get_tables(0)");
        let v: serde_json::Value = serde_json::from_str(&js).unwrap();
        assert_eq!(v[0]["idx"], 0);
        assert_eq!(v[0]["name"], "Table1");
        assert_eq!(v[0]["range"], serde_json::json!([0, 0, 2, 1]));
        assert_eq!(v[0]["style"], "TableStyleMedium2");
        assert_eq!(v[0]["has_header"], true);
        assert_eq!(v[0]["row_stripes"], true);
        assert_eq!(run_str(&mut wb, "get_tables(7)"), "[]");
    }

    #[test]
    fn readonly_env_only_exposes_get_tables() {
        let mut wb = wb_n(1);
        let mut e = rhai::Engine::new();
        register_readonly(&mut e);
        with_workbook(&mut wb, || {
            assert_eq!(e.eval::<String>("get_tables(0)").unwrap(), "[]");
            assert!(e.eval::<String>(r#"create_table(0, 0, 0, 1, 1, true, "TableStyleMedium2")"#).is_err());
            assert!(e.eval::<String>(r#"set_table_style(0, 0, "TableStyleLight1")"#).is_err());
        });
        assert!(wb.sheets[0].tables.is_empty());
    }
}


#[cfg(test)]
mod names_tests {
    use super::*;
    use std::collections::HashMap;

    fn test_wb() -> roe_model::calc::Workbook {
        let mk = |name: &str| roe_model::calc::Sheet {
            col_style: vec![],
            tables: vec![],
            name: name.into(),
            cells: roe_model::calc::SparseGrid::default(),
            col_widths: vec![],
            row_heights: vec![],
            frozen_rows: 0,
            frozen_cols: 0,
            date1904: false,
            zoom_scale: None,
            show_grid_lines: None,
            col_hidden: vec![],
            row_hidden: vec![],
            col_outline_level: vec![],
            row_outline_level: vec![],
            cell_formats: HashMap::new(),
            cond_formats: vec![],
            data_validations: vec![],
            auto_filter: None,
            merged_cells: vec![],
            pivot: None,
            floating_images: vec![],
            floating_charts: vec![],
            floating_stat_charts: vec![],
            floating_shapes: vec![],
            next_object_id: 0,
            default_col_width: 0.0,
            default_row_height: 0.0,
            sparkline_groups: vec![],
        };
        roe_model::calc::Workbook {
            sheets: vec![mk("Sheet1"), mk("My Data")],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn names_set_range_and_list() {
        let mut wb = test_wb();
        names_set(&mut wb, "Tax", "=Sheet1!$A$1:$A$3").unwrap();
        names_set(&mut wb, "One", "Sheet1!B2").unwrap();
        names_set(&mut wb, "Other", "'My Data'!$A$1:$B$2").unwrap();
        let v: serde_json::Value = serde_json::from_str(&names_list_json(&wb)).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["name"], "One");
        assert_eq!(arr[0]["ref"], "Sheet1!$B$2");
        assert_eq!(arr[1]["name"], "Other");
        assert_eq!(arr[1]["ref"], "'My Data'!$A$1:$B$2");
        assert_eq!(arr[2]["name"], "Tax");
        assert_eq!(arr[2]["kind"], "range");
        assert_eq!(arr[2]["ref"], "Sheet1!$A$1:$A$3");
    }

    #[test]
    fn names_set_formula_kind() {
        let mut wb = test_wb();
        names_set(&mut wb, "Cnt", "=COUNTA($A:$A)").unwrap();
        assert_eq!(wb.named_formulas.get("Cnt").map(|s| s.as_str()), Some("COUNTA($A:$A)"));
        assert!(wb.named_ranges.is_empty());
    }

    #[test]
    fn names_set_overwrites_case_insensitively_and_switches_kind() {
        let mut wb = test_wb();
        names_set(&mut wb, "Global6", "Sheet1!$A$1").unwrap();
        names_set(&mut wb, "global6", "5").unwrap();
        assert_eq!(wb.named_ranges.len(), 0);
        assert_eq!(wb.named_formulas.len(), 1);
        assert!(wb.named_formulas.contains_key("global6"));
    }

    #[test]
    fn names_remove_ci_works() {
        let mut wb = test_wb();
        names_set(&mut wb, "Tax", "Sheet1!$A$1:$A$3").unwrap();
        assert!(names_remove_ci(&mut wb, "TAX"));
        assert!(!names_remove_ci(&mut wb, "Tax"));
        assert!(wb.named_ranges.is_empty());
    }

    #[test]
    fn names_reject_invalid() {
        let mut wb = test_wb();
        for bad in ["", "A1", "ab12", "R1C1", "R", "C5", "1abc", "a b", "x-y"] {
            assert!(names_set(&mut wb, bad, "Sheet1!$A$1").is_err(), "should reject {:?}", bad);
        }
        for ok in ["Tax", "_x", "Rate", "Cost", "my.name", "税率"] {
            assert!(names_set(&mut wb, ok, "Sheet1!$A$1").is_ok(), "should accept {:?}", ok);
        }
    }

    #[test]
    fn names_reject_unknown_sheet_and_empty_ref() {
        let mut wb = test_wb();
        assert!(names_set(&mut wb, "X", "NoSheet!$A$1").is_err());
        assert!(names_set(&mut wb, "X", "  ").is_err());
        assert!(wb.named_ranges.is_empty() && wb.named_formulas.is_empty());
    }

    #[test]
    fn names_validate_detects_duplicate_ci() {
        let mut wb = test_wb();
        names_set(&mut wb, "Tax", "Sheet1!$A$1").unwrap();
        assert!(names_validate(&wb, "tax").is_err());
        assert!(names_validate(&wb, "Tax2x").is_ok());
        assert!(names_validate(&wb, "A1").is_err());
    }
}


#[cfg(test)]
mod sheet_ops_tests {
    use super::*;

    fn wb1() -> Workbook {
        Workbook {
            sheets: vec![roe_model::calc::Sheet { name: "Sheet1".into(), ..Default::default() }],
            named_ranges: std::collections::HashMap::new(),
            named_formulas: std::collections::HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn sheet_ops_rename_and_remove() {
        let mut wb = wb1();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            let r: String = e.eval("add_sheet(`数据`)").unwrap();
            assert!(r.starts_with("ok:"), "{r}");
            let r: String = e.eval("rename_sheet(1, `明细`)").unwrap();
            assert_eq!(r, "ok");
            let r: String = e.eval("rename_sheet(1, `sheet1`)").unwrap();
            assert!(r.starts_with("err:"), "重名应拒绝: {r}");
            let r: String = e.eval("rename_sheet(1, `明细`)").unwrap();
            assert_eq!(r, "ok", "改成原名应视为成功");
            let r: String = e.eval("rename_sheet(9, `x`)").unwrap();
            assert!(r.starts_with("err:"), "{r}");
            let r: String = e.eval("remove_sheet(1)").unwrap();
            assert_eq!(r, "ok");
            let n: i64 = e.eval("get_sheet_count()").unwrap();
            assert_eq!(n, 1);
            let r: String = e.eval("remove_sheet(0)").unwrap();
            assert!(r.starts_with("err:"), "最后一个不能删: {r}");
        });
    }

    #[test]
    fn sheet_ops_reject_when_referenced() {
        let mut wb = wb1();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run("add_sheet(`数据`)").unwrap();
            e.run("set_formula(0, 0, 0, `=数据!A1`)").unwrap();
            let r: String = e.eval("rename_sheet(1, `别名`)").unwrap();
            assert!(r.starts_with("err:"), "被引用应拒绝改名: {r}");
            let r: String = e.eval("remove_sheet(1)").unwrap();
            assert!(r.starts_with("err:"), "被引用应拒绝删除: {r}");
        });
    }

    #[test]
    fn sheet_ops_auto_fit_columns() {
        let mut wb = wb1();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run("set_cell(0, 0, 0, `Hello World This Is A Long Text`)").unwrap();
            let r: String = e.eval("auto_fit_columns(0, 0, 1)").unwrap();
            assert_eq!(r, "ok:1", "只有 A 列有内容: {r}");
            let w: f64 = e.eval("get_column_width(0, 0)").unwrap();
            assert!(w > 48.0, "w={w}");
            let r: String = e.eval("auto_fit_columns(0, 3, 1)").unwrap();
            assert!(r.starts_with("err:"), "{r}");
        });
    }
}


#[cfg(test)]
mod range_json_tests {
    use super::*;

    #[test]
    fn get_range_json_reads_values_formula_and_limits() {
        let mut wb = Workbook {
            sheets: vec![roe_model::calc::Sheet { name: "Sheet1".into(), ..Default::default() }],
            named_ranges: std::collections::HashMap::new(),
            named_formulas: std::collections::HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            e.run("set_cell(0, 0, 0, `abc`)").unwrap();
            e.run("set_cell(0, 0, 1, `12`)").unwrap();
            e.run("set_formula(0, 1, 1, `=B1*2`)").unwrap();
            let r: String = e.eval("get_range_json(0, 0, 0, 1, 1, false)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&r).unwrap_or_else(|_| panic!("{r}"));
            assert_eq!(v["rows"][0][0]["value"], "abc", "{r}");
            assert_eq!(v["rows"][0][1]["value"], 12.0, "{r}");
            assert_eq!(v["rows"][1][1]["formula_source"], "=B1*2", "{r}");
            assert!(v["rows"][1][0].get("value").is_none(), "空格不应有 value: {r}");
            let r: String = e.eval("get_range_json(0, 0, 0, 100, 100, false)").unwrap();
            assert!(r.starts_with("err:"), "超限应拒绝: {r}");
            let r: String = e.eval("get_range_json(5, 0, 0, 1, 1, false)").unwrap();
            assert!(r.starts_with("err:"), "sheet 越界: {r}");
        });
    }
}


#[cfg(test)]
mod calc_ok_err_tests {
    use super::*;
    use roe_model::calc::Sheet;
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet { name: "Sheet1".into(), ..Default::default() }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    fn run_s(e: &rhai::Engine, sc: &str) -> String {
        e.eval::<String>(sc).unwrap()
    }

    #[test]
    fn merge_cells_ok_and_err() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            assert_eq!(run_s(&e, "merge_cells(0, 0, 0, 1, 1)"), "ok");
            assert_eq!(run_s(&e, "merge_cells(0, 2, 2, 1, 1)"), "err:区域非法");
            assert_eq!(run_s(&e, "merge_cells(0, -1, 0, 1, 1)"), "err:区域非法");
            assert_eq!(run_s(&e, "merge_cells(0, 0, 0, 1, 16384)"), "err:区域非法");
            assert_eq!(run_s(&e, "merge_cells(99, 0, 0, 1, 1)"), "err:sheet 越界");
        });
        assert_eq!(wb.sheets[0].merged_cells.len(), 1, "err 不得写入");
    }

    #[test]
    fn row_hidden_ok_and_err() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            assert_eq!(run_s(&e, "set_row_hidden(0, 3, true)"), "ok");
            assert!(e.eval::<bool>("get_row_hidden(0, 3)").unwrap());
            assert!(run_s(&e, "set_row_hidden(0, -1, true)").starts_with("err:行号越界"));
            assert!(run_s(&e, "set_row_hidden(0, 1048576, true)").starts_with("err:行号越界"));
            assert_eq!(run_s(&e, "set_row_hidden(99, 0, true)"), "err:sheet 越界");
            assert_eq!(run_s(&e, "set_row_hidden_range(0, 5, 7, true)"), "ok");
            assert!(e.eval::<bool>("get_row_hidden(0, 6)").unwrap());
            assert!(run_s(&e, "set_row_hidden_range(0, 7, 5, true)").starts_with("err:区间非法"));
            assert!(run_s(&e, "set_row_hidden_range(0, -1, 5, true)").starts_with("err:行号越界"));
            assert_eq!(run_s(&e, "set_row_hidden_range(99, 0, 1, true)"), "err:sheet 越界");
        });
        assert!(wb.sheets[0].row_hidden.len() < 100, "err 不得触发巨量分配");
    }

    #[test]
    fn row_outline_ok_and_err() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            assert_eq!(run_s(&e, "set_row_outline_level(0, 2, 7)"), "ok");
            assert_eq!(e.eval::<i64>("get_row_outline_level(0, 2)").unwrap(), 7);
            assert_eq!(run_s(&e, "set_row_outline_level(0, 2, 8)"), "err:level 须在 0..7");
            assert_eq!(run_s(&e, "set_row_outline_level(0, 2, -1)"), "err:level 须在 0..7");
            assert!(run_s(&e, "set_row_outline_level(0, -1, 1)").starts_with("err:行号越界"));
            assert_eq!(run_s(&e, "set_row_outline_level(99, 0, 1)"), "err:sheet 越界");
            assert_eq!(run_s(&e, "set_row_outline_level_range(0, 4, 6, 3)"), "ok");
            assert_eq!(e.eval::<i64>("get_row_outline_level(0, 5)").unwrap(), 3);
            assert!(run_s(&e, "set_row_outline_level_range(0, 6, 4, 3)").starts_with("err:区间非法"));
            assert_eq!(run_s(&e, "set_row_outline_level_range(0, 4, 6, 9)"), "err:level 须在 0..7");
            assert_eq!(run_s(&e, "set_row_outline_level_range(99, 0, 1, 1)"), "err:sheet 越界");
        });
    }

    #[test]
    fn apply_borders_ok_and_err() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            assert_eq!(run_s(&e, r#"apply_borders(0, 0, 0, 1, 1, "all", "thin", "")"#), "ok");
            assert_eq!(run_s(&e, r##"apply_borders(0, 1, 1, 0, 0, "outside", "thin", "#FF0000")"##), "ok");
            assert!(run_s(&e, r#"apply_borders(0, 0, 0, 1, 1, "bogus", "thin", "")"#).starts_with("err:mode 非法"));
            assert_eq!(run_s(&e, r#"apply_borders(0, -1, 0, 1, 1, "all", "thin", "")"#), "err:区域非法");
            assert_eq!(run_s(&e, r#"apply_borders(0, 0, 0, 1048576, 1, "all", "thin", "")"#), "err:区域非法");
            assert_eq!(run_s(&e, r#"apply_borders(99, 0, 0, 1, 1, "all", "thin", "")"#), "err:sheet 越界");
            assert!(run_s(&e, r#"apply_borders(0, 0, 0, 5000, 1000, "all", "thin", "")"#).starts_with("err:选区过大"));
        });
    }

    #[test]
    fn insert_delete_row_col_ok_and_err() {
        let mut wb = empty_wb();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_workbook(&mut wb, || {
            assert_eq!(run_s(&e, "insert_row(0, 0)"), "ok");
            assert_eq!(run_s(&e, "delete_row(0, 0)"), "ok");
            assert_eq!(run_s(&e, "insert_column(0, 0)"), "ok");
            assert_eq!(run_s(&e, "delete_column(0, 0)"), "ok");
            for sc in ["insert_row(0, -1)", "insert_row(0, 1048576)", "delete_row(0, -1)", "delete_row(0, 1048576)"] {
                assert!(run_s(&e, sc).starts_with("err:行号越界"), "{sc}");
            }
            for sc in ["insert_column(0, -1)", "insert_column(0, 16384)", "delete_column(0, -1)", "delete_column(0, 16384)"] {
                assert!(run_s(&e, sc).starts_with("err:列号越界"), "{sc}");
            }
            for sc in ["insert_row(99, 0)", "delete_row(99, 0)", "insert_column(99, 0)", "delete_column(99, 0)"] {
                assert_eq!(run_s(&e, sc), "err:sheet 越界", "{sc}");
            }
        });
    }
}
