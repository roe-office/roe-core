//! Calc 区域批量读写（C-ABI 层）：`roe_get_range_json` / `roe_set_range_json`。
//!
//! 背景（Gemini 缺口清单第1条剩余两项，ROE_Progress_v196.md §2.1）：
//! 与 find_replace.rs / auto_fit.rs 同一分层原则——本文件只负责 FFI
//! 边界（handle 解析、C 字符串、JSON 序列化/反序列化、错误码），
//! 不重新实现 Calc 核心数据结构或公式求值逻辑，全部复用
//! roe_model::calc 既有类型和 roe_calc::CalcEngine。
//!
//! 设计要点（本次会话与用户逐项讨论确认，非本文件自主决定）：
//! - get_range_json 支持 include_format 参数控制是否携带完整
//!   CellFormat；公式单元格同时返回 formula_source 和 cached_value；
//!   非左上角合并单元格带 merged_into 字段标注左上角坐标。
//! - set_range_json 支持部分字段更新（未传字段保持原样）；写入
//!   formula_source 时忽略输入中的 cached_value（cached 由
//!   CalcEngine 重算得出，不接受外部灌值）；写入合并单元格非左上角
//!   坐标时自动重定向到左上角，不报错也不丢弃；输入 JSON 中任意
//!   一个单元格非法（字段类型错误等）则整体失败、不写入任何改动
//!   （先校验后写入的两遍扫描，保证严格的"全有或全无"语义）。
//! - 公式重算：采用"方案A"（与用户/Gemini讨论后拍板，ROE_Progress_v196.md
//!   会话记录）——每次调用现建一个 roe_calc::CalcEngine::new()，
//!   在本文件内部直接遍历写入并手动收集 dirty_cells 后调用
//!   recalc_and_apply，不复用/不读写 entry.calc_engine 持久字段，
//!   也不经过 roe_script::calc_api::with_workbook（那是专为 Rhai
//!   脚本引擎设计的 thread-local 桥接机制，接口形状是 FnOnce()，
//!   不支持从外部按行传入 sheet 引用操作，写代码验证时才发现不适用，
//!   已改为本文件自行维护 dirty_cells vec 的写法）。
//! - 单元格写入统一走 SparseGrid::set（roe_model::calc 公开 API），
//!   不复用 roe-script::calc_api::set_cell_in_sheet——该函数是
//!   roe-script crate 内的私有函数，roe-api 访问不到，且其逻辑
//!   本身就是对 SparseGrid::set 的一层查找优化包装，直接用
//!   SparseGrid::set 语义等价，不需要跨 crate 改可见性。

use std::os::raw::c_char;

use roe_model::calc::{CellFormat, CellValue, MergedCell, Sheet};
use roe_parser::RoeDocument;
use serde::Deserialize;
use serde_json::Value;

use crate::error::{set_last_error, RoeErrorCode};
use crate::handle;

fn json_to_cstring(json: String, fn_name: &str) -> *mut c_char {
    match std::ffi::CString::new(json) {
        Ok(cs) => cs.into_raw(),
        Err(e) => {
            set_last_error(format!("{fn_name}: {e}"));
            std::ptr::null_mut()
        }
    }
}

/// 在 sheet.merged_cells 中查找 (row, col) 所属的合并区域（若存在）。
fn find_merge_for_cell(sheet: &Sheet, row: u32, col: u32) -> Option<&MergedCell> {
    sheet.merged_cells.iter().find(|mc| {
        row >= mc.r1 && row <= mc.r2 && col >= mc.c1 && col <= mc.c2
    })
}

/// 把单个单元格 (row, col) 序列化为 JSON。include_format=true 时携带
/// 完整 CellFormat。非左上角合并单元格额外带 merged_into: [top_row, top_col]。
fn cell_to_json(sheet: &Sheet, row: u32, col: u32, include_format: bool) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("row".to_string(), Value::from(row));
    obj.insert("col".to_string(), Value::from(col));

    if let Some(merge) = find_merge_for_cell(sheet, row, col) {
        if merge.r1 != row || merge.c1 != col {
            obj.insert(
                "merged_into".to_string(),
                Value::Array(vec![Value::from(merge.r1), Value::from(merge.c1)]),
            );
            return Value::Object(obj);
        }
    }

    let value = sheet.cells.get(row, col);
    match &value {
        CellValue::Empty => {}
        CellValue::Number(n) => {
            obj.insert("value".to_string(), serde_json::json!(n));
        }
        CellValue::Text(s) => {
            obj.insert("value".to_string(), Value::from(s.clone()));
        }
        CellValue::RichText(runs) => {
            let text: String = runs.iter().map(|r| r.text.as_str()).collect();
            obj.insert("value".to_string(), Value::from(text));
        }
        CellValue::Bool(b) => {
            obj.insert("value".to_string(), Value::from(*b));
        }
        CellValue::Error(e) => {
            obj.insert("error".to_string(), Value::from(format!("{e:?}")));
        }
        CellValue::Formula { source, cached } => {
            obj.insert("formula_source".to_string(), Value::from(source.clone()));
            let cached_val = match cached.as_ref() {
                CellValue::Empty => Value::Null,
                CellValue::Number(n) => serde_json::json!(n),
                CellValue::Text(s) => Value::from(s.clone()),
                CellValue::RichText(runs) => {
                    Value::from(runs.iter().map(|r| r.text.as_str()).collect::<String>())
                }
                CellValue::Bool(b) => Value::from(*b),
                CellValue::Error(e) => Value::from(format!("{e:?}")),
                CellValue::Formula { .. } => Value::Null,
            };
            obj.insert("cached_value".to_string(), cached_val);
        }
    }

    if include_format {
        if let Some(fmt) = sheet.cell_formats.get(&(row, col)) {
            if let Ok(fmt_json) = serde_json::to_value(fmt) {
                obj.insert("format".to_string(), fmt_json);
            }
        }
    }

    Value::Object(obj)
}

/// 读取指定矩形区域 [start_row, end_row] x [start_col, end_col]（闭区间）。
/// 越界的行/列按空单元格处理，不报错。
#[no_mangle]
pub extern "C" fn roe_get_range_json(
    handle: u64,
    sheet_idx: u32,
    start_row: u32,
    start_col: u32,
    end_row: u32,
    end_col: u32,
    include_format: bool,
) -> *mut c_char {
    if start_row > end_row || start_col > end_col {
        set_last_error("roe_get_range_json: start 不能大于 end");
        return std::ptr::null_mut();
    }

    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Calc(wb) = &entry.doc else {
            return Err("非 Calc 文档".to_string());
        };
        let Some(sheet) = wb.sheets.get(sheet_idx as usize) else {
            return Err("sheet_idx 越界".to_string());
        };

        let mut rows_json = Vec::with_capacity((end_row - start_row + 1) as usize);
        for row in start_row..=end_row {
            let mut row_json = Vec::with_capacity((end_col - start_col + 1) as usize);
            for col in start_col..=end_col {
                row_json.push(cell_to_json(sheet, row, col, include_format));
            }
            rows_json.push(Value::Array(row_json));
        }

        let out = serde_json::json!({ "rows": rows_json });
        serde_json::to_string(&out).map_err(|e| format!("JSON 序列化失败: {e}"))
    });

    match result {
        Some(Ok(json)) => json_to_cstring(json, "roe_get_range_json"),
        Some(Err(msg)) => {
            set_last_error(format!("roe_get_range_json: {msg}"));
            std::ptr::null_mut()
        }
        None => {
            set_last_error("roe_get_range_json: 无效句柄");
            std::ptr::null_mut()
        }
    }
}

// ─── set_range_json ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CellUpdate {
    row: u32,
    col: u32,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    formula_source: Option<String>,
    #[serde(default)]
    format: Option<CellFormat>,
}

#[derive(Debug, Deserialize)]
struct SetRangeInput {
    cells: Vec<CellUpdate>,
}

/// 把 serde_json::Value 转换为 CellValue（用于 value 字段，非公式）。
fn json_value_to_cell_value(v: &Value) -> Result<CellValue, String> {
    match v {
        Value::Null => Ok(CellValue::Empty),
        Value::Bool(b) => Ok(CellValue::Bool(*b)),
        Value::Number(n) => n
            .as_f64()
            .map(CellValue::Number)
            .ok_or_else(|| "数值超出 f64 表示范围".to_string()),
        Value::String(s) => Ok(CellValue::Text(s.clone())),
        Value::Array(_) | Value::Object(_) => {
            Err("value 字段不支持数组/对象类型".to_string())
        }
    }
}

/// 校验单个 CellUpdate：目前只校验 value 字段的类型合法性。
fn validate_cell_update(update: &CellUpdate) -> Result<(), String> {
    if let Some(v) = &update.value {
        json_value_to_cell_value(v)?;
    }
    Ok(())
}

/// 写入单个已校验通过的 CellUpdate。合并单元格非左上角坐标自动
/// 重定向到左上角。返回实际写入生效的 (row, col)。
fn apply_cell_update(sheet: &mut Sheet, update: &CellUpdate) -> (u32, u32) {
    let (target_row, target_col) = match find_merge_for_cell(sheet, update.row, update.col) {
        Some(merge) => (merge.r1, merge.c1),
        None => (update.row, update.col),
    };

    if let Some(formula) = &update.formula_source {
        sheet.cells.set(
            target_row,
            target_col,
            CellValue::Formula {
                source: formula.clone(),
                cached: Box::new(CellValue::Empty),
            },
        );
    } else if let Some(v) = &update.value {
        let cv = json_value_to_cell_value(v).unwrap();
        sheet.cells.set(target_row, target_col, cv);
    }

    if let Some(fmt) = &update.format {
        sheet.cell_formats.insert((target_row, target_col), fmt.clone());
    }

    sheet.invalidate_row_height(target_row);

    (target_row, target_col)
}

/// 批量写入指定区域内的单元格（值/公式/格式，均支持部分更新）。
/// 严格校验语义：任意一格非法则整体失败，不写入任何改动。
#[no_mangle]
pub extern "C" fn roe_set_range_json(
    handle: u64,
    sheet_idx: u32,
    json: *const c_char,
) -> i32 {
    if json.is_null() {
        set_last_error("roe_set_range_json: json 不能为空指针");
        return RoeErrorCode::NullPointer as i32;
    }
    let json_str = match unsafe { std::ffi::CStr::from_ptr(json) }.to_str() {
        Ok(s) => s,
        Err(_) => {
            set_last_error("roe_set_range_json: json 不是合法 UTF-8");
            return RoeErrorCode::Utf8Error as i32;
        }
    };

    let input: SetRangeInput = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => {
            set_last_error(format!("roe_set_range_json: JSON 解析失败: {e}"));
            return RoeErrorCode::ParseError as i32;
        }
    };

    let result = handle::with_entry(handle, |entry| {
        {
            let RoeDocument::Calc(wb) = &entry.doc else {
                return Err("非 Calc 文档".to_string());
            };
            if wb.sheets.get(sheet_idx as usize).is_none() {
                return Err("sheet_idx 越界".to_string());
            }
        }

        for update in &input.cells {
            validate_cell_update(update)
                .map_err(|e| format!("单元格 ({}, {}) 校验失败: {e}", update.row, update.col))?;
        }

        entry.snapshot = Some(entry.doc.clone());

        let RoeDocument::Calc(wb) = &mut entry.doc else {
            unreachable!("上面已确认是 Calc 文档");
        };

        let mut dirty_cells: Vec<(usize, u32, u32)> = Vec::new();
        {
            let sheet = &mut wb.sheets[sheet_idx as usize];
            for update in &input.cells {
                let (r, c) = apply_cell_update(sheet, update);
                dirty_cells.push((sheet_idx as usize, r, c));
            }
        }

        let mut engine = roe_calc::CalcEngine::new();
        let _applied = engine.recalc_and_apply(wb, &dirty_cells);

        Ok(dirty_cells.len() as i32)
    });

    match result {
        Some(Ok(n)) => {
            if n > 0 {
                let prev = handle::with_entry(handle, |entry| entry.snapshot.take()).flatten();
                if let Some(prev) = prev {
                    handle::with_entry(handle, |entry| {
                        let ctx = handle::EditCursorContext::range(0, 0, 0);
                        entry.push_undo_snapshot(prev, ctx);
                    });
                }
            } else {
                handle::with_entry(handle, |entry| {
                    entry.snapshot = None;
                });
            }
            n
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_set_range_json: {msg}"));
            RoeErrorCode::ScriptError as i32
        }
        None => {
            set_last_error("roe_set_range_json: 无效句柄");
            RoeErrorCode::InvalidHandle as i32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::roe_last_error_message;
    use crate::handle::WriterOptions;
    use roe_model::calc::{MergedCell, Sheet, Workbook};
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use std::ffi::CString;

    /// 构造一个最小 Calc handle：单 sheet，可选预置若干单元格。
    /// 与 auto_fit.rs 测试用的构造模式一致（直接手搭 Workbook，不走
    /// parser，因为本文件测试的是 C-ABI 层逻辑，不涉及文件格式解析）。
    fn open_calc_with_cells(cells: Vec<(u32, u32, CellValue)>) -> u64 {
        let mut sheet = Sheet { name: "Sheet1".into(), ..Default::default() };
        for (row, col, value) in cells {
            sheet.cells.set(row, col, value);
        }
        let wb = Workbook {
            sheets: vec![sheet],
            named_ranges: std::collections::HashMap::new(),
            named_formulas: std::collections::HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };
        handle::insert(handle::DocumentEntry {
            doc: RoeDocument::Calc(wb),
            snapshot: None,
            is_shadow: false,
            events: VecDeque::new(),
            animation: HashMap::new(),
            writer_layout_cache: None,
            writer_font_registry: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            writer_options: WriterOptions::default(),
            impress_font_registry: None,
            writer_parley_font_context: None,
            calc_engine: None,
            pending_comment_hits: Vec::new(),
        })
    }

    fn cstr(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn get_range_str(h: u64, sheet: u32, r1: u32, c1: u32, r2: u32, c2: u32, fmt: bool) -> String {
        let ptr = roe_get_range_json(h, sheet, r1, c1, r2, c2, fmt);
        assert!(!ptr.is_null(), "roe_get_range_json returned null unexpectedly");
        let s = unsafe { std::ffi::CStr::from_ptr(ptr) }.to_str().unwrap().to_string();
        unsafe { drop(CString::from_raw(ptr)) };
        s
    }

    #[test]
    fn get_range_returns_plain_values_without_format() {
        let h = open_calc_with_cells(vec![
            (0, 0, CellValue::Text("hello".to_string())),
            (0, 1, CellValue::Number(42.0)),
        ]);
        let json = get_range_str(h, 0, 0, 0, 0, 1, false);
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["rows"][0][0]["value"], Value::from("hello"));
        assert_eq!(v["rows"][0][1]["value"], serde_json::json!(42.0));
        // include_format=false 时不应出现 format 字段
        assert!(v["rows"][0][0].get("format").is_none());
    }

    #[test]
    fn get_range_includes_full_format_when_requested() {
        let h = open_calc_with_cells(vec![(0, 0, CellValue::Text("x".to_string()))]);
        handle::with_entry(h, |entry| {
            let RoeDocument::Calc(wb) = &mut entry.doc else { unreachable!() };
            wb.sheets[0].cell_formats.insert((0, 0), CellFormat {
                bold: Some(true),
                font_size: Some(14.0),
                bg_color: Some("#FFFF00".to_string()),
                ..Default::default()
            });
        });
        let json = get_range_str(h, 0, 0, 0, 0, 0, true);
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["rows"][0][0]["format"]["bold"], Value::from(true));
        assert_eq!(v["rows"][0][0]["format"]["font_size"], serde_json::json!(14.0));
        assert_eq!(v["rows"][0][0]["format"]["bg_color"], Value::from("#FFFF00"));
    }

    #[test]
    fn get_range_empty_cell_has_no_value_field() {
        let h = open_calc_with_cells(vec![]);
        let json = get_range_str(h, 0, 0, 0, 0, 0, false);
        let v: Value = serde_json::from_str(&json).unwrap();
        assert!(v["rows"][0][0].get("value").is_none());
    }

    #[test]
    fn get_range_out_of_bounds_treated_as_empty_not_error() {
        let h = open_calc_with_cells(vec![]);
        let json = get_range_str(h, 0, 100, 100, 101, 101, false);
        let v: Value = serde_json::from_str(&json).unwrap();
        assert!(v["rows"][0][0].get("value").is_none());
    }

    #[test]
    fn get_range_merged_non_top_left_has_merged_into() {
        let h = open_calc_with_cells(vec![(0, 0, CellValue::Text("merged".to_string()))]);
        handle::with_entry(h, |entry| {
            let RoeDocument::Calc(wb) = &mut entry.doc else { unreachable!() };
            wb.sheets[0].merged_cells.push(MergedCell { r1: 0, c1: 0, r2: 0, c2: 2 });
        });
        let json = get_range_str(h, 0, 0, 0, 0, 2, false);
        let v: Value = serde_json::from_str(&json).unwrap();
        // 左上角本身：无 merged_into，有 value
        assert!(v["rows"][0][0].get("merged_into").is_none());
        assert_eq!(v["rows"][0][0]["value"], Value::from("merged"));
        // 非左上角：有 merged_into 指向 [0,0]，无 value
        assert_eq!(v["rows"][0][1]["merged_into"], serde_json::json!([0, 0]));
        assert!(v["rows"][0][1].get("value").is_none());
        assert_eq!(v["rows"][0][2]["merged_into"], serde_json::json!([0, 0]));
    }

    #[test]
    fn get_range_invalid_handle_returns_null() {
        let json = roe_get_range_json(0, 0, 0, 0, 0, 0, false);
        assert!(json.is_null());
        let msg_ptr = roe_last_error_message();
        assert!(!msg_ptr.is_null());
    }

    #[test]
    fn get_range_sheet_idx_out_of_range_returns_null() {
        let h = open_calc_with_cells(vec![]);
        let json = roe_get_range_json(h, 99, 0, 0, 0, 0, false);
        assert!(json.is_null());
    }

    #[test]
    fn get_range_start_greater_than_end_returns_null() {
        let h = open_calc_with_cells(vec![]);
        let json = roe_get_range_json(h, 0, 5, 0, 0, 0, false);
        assert!(json.is_null());
    }

    // ─── set_range_json ─────────────────────────────────────────

    fn set_range(h: u64, sheet: u32, json: &str) -> i32 {
        let c = cstr(json);
        roe_set_range_json(h, sheet, c.as_ptr())
    }

    #[test]
    fn set_range_writes_plain_value() {
        let h = open_calc_with_cells(vec![]);
        let n = set_range(h, 0, r#"{"cells":[{"row":0,"col":0,"value":"hi"}]}"#);
        assert_eq!(n, 1);
        let json = get_range_str(h, 0, 0, 0, 0, 0, false);
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["rows"][0][0]["value"], Value::from("hi"));
    }

    #[test]
    fn set_range_partial_update_preserves_untouched_fields() {
        let h = open_calc_with_cells(vec![(0, 0, CellValue::Text("original".to_string()))]);
        handle::with_entry(h, |entry| {
            let RoeDocument::Calc(wb) = &mut entry.doc else { unreachable!() };
            wb.sheets[0].cell_formats.insert((0, 0), CellFormat {
                bold: Some(true),
                ..Default::default()
            });
        });
        // 只传 value，不传 format —— 原格式应保持不变。
        let n = set_range(h, 0, r#"{"cells":[{"row":0,"col":0,"value":"changed"}]}"#);
        assert_eq!(n, 1);
        let json = get_range_str(h, 0, 0, 0, 0, 0, true);
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["rows"][0][0]["value"], Value::from("changed"));
        assert_eq!(v["rows"][0][0]["format"]["bold"], Value::from(true));
    }

    #[test]
    fn set_range_formula_triggers_recalc_and_cached_value_is_fresh() {
        let h = open_calc_with_cells(vec![(0, 0, CellValue::Number(10.0))]);
        // 写入 B1 = A1 + 5，期望 cached_value 立即算出 15。
        let n = set_range(h, 0, r#"{"cells":[{"row":0,"col":1,"formula_source":"=A1+5"}]}"#);
        assert_eq!(n, 1);
        let json = get_range_str(h, 0, 0, 1, 0, 1, false);
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["rows"][0][0]["formula_source"], Value::from("=A1+5"));
        assert_eq!(v["rows"][0][0]["cached_value"], serde_json::json!(15.0));
    }

    #[test]
    fn set_range_merged_non_top_left_redirects_to_top_left() {
        let h = open_calc_with_cells(vec![]);
        handle::with_entry(h, |entry| {
            let RoeDocument::Calc(wb) = &mut entry.doc else { unreachable!() };
            wb.sheets[0].merged_cells.push(MergedCell { r1: 0, c1: 0, r2: 0, c2: 2 });
        });
        // 写入非左上角坐标 (0,2)，应该实际落到 (0,0)。
        let n = set_range(h, 0, r#"{"cells":[{"row":0,"col":2,"value":"redirected"}]}"#);
        assert_eq!(n, 1);
        let json = get_range_str(h, 0, 0, 0, 0, 0, false);
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["rows"][0][0]["value"], Value::from("redirected"));
    }

    #[test]
    fn set_range_strict_validation_rejects_all_on_one_bad_cell() {
        let h = open_calc_with_cells(vec![(0, 0, CellValue::Text("untouched".to_string()))]);
        // 第二个单元格的 value 是数组，非法类型；第一个单元格本身合法。
        let json_in = r#"{"cells":[{"row":1,"col":0,"value":"ok"},{"row":2,"col":0,"value":[1,2,3]}]}"#;
        let n = set_range(h, 0, json_in);
        assert!(n < 0, "expected negative error code, got {n}");
        // 校验第一个单元格（合法的那个）也没有被写入 —— 全有或全无。
        let json = get_range_str(h, 0, 1, 0, 1, 0, false);
        let v: Value = serde_json::from_str(&json).unwrap();
        assert!(v["rows"][0][0].get("value").is_none(), "partial write leaked despite strict validation");
    }

    #[test]
    fn set_range_invalid_handle_returns_invalid_handle_code() {
        let n = set_range(0, 0, r#"{"cells":[]}"#);
        assert_eq!(n, RoeErrorCode::InvalidHandle as i32);
    }

    #[test]
    fn set_range_sheet_idx_out_of_range_returns_error() {
        let h = open_calc_with_cells(vec![]);
        let n = set_range(h, 99, r#"{"cells":[]}"#);
        assert!(n < 0);
    }

    #[test]
    fn set_range_null_json_pointer_returns_null_pointer_code() {
        let h = open_calc_with_cells(vec![]);
        let n = roe_set_range_json(h, 0, std::ptr::null());
        assert_eq!(n, RoeErrorCode::NullPointer as i32);
    }

    #[test]
    fn set_range_malformed_json_returns_parse_error_code() {
        let h = open_calc_with_cells(vec![]);
        let n = set_range(h, 0, "not valid json{{{");
        assert_eq!(n, RoeErrorCode::ParseError as i32);
    }
}
