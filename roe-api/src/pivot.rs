//! 透视表 C-ABI 接口
//!
//! - roe_pivot_sheet：对指定 sheet 执行透视，结果追加为新 sheet
//! - roe_get_pivot_unique_values：取指定列的唯一值列表（供筛选下拉菜单）
//! - roe_sheet_preview：取指定 sheet 前 N 行预览 JSON

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use roe_parser::RoeDocument;

use crate::error::set_last_error;
use crate::handle;

/// 对 sheet_idx 执行透视，结果作为新 sheet 追加到 workbook。
///
/// config_json 格式：
/// {"rows":["A"],"cols":["B"],"values":["C"],"agg":"sum","filters":{"D":["v1"]}}
/// rows/cols/values/filters 均可独立配置，任意组合。
///
/// 返回新 sheet 的索引（u32），失败返回 u32::MAX，详情见 roe_last_error_message。
#[no_mangle]
pub extern "C" fn roe_pivot_sheet(
    handle: u64,
    sheet_idx: u32,
    config_json: *const c_char,
    result_sheet_name: *const c_char,
) -> u32 {
    let config_json = match unsafe { CStr::from_ptr(config_json) }.to_str() {
        Ok(s) => s.to_string(),
        Err(e) => { set_last_error(format!("roe_pivot_sheet: config_json utf8 error: {e}")); return u32::MAX; }
    };
    let result_name = match unsafe { CStr::from_ptr(result_sheet_name) }.to_str() {
        Ok(s) => s.to_string(),
        Err(e) => { set_last_error(format!("roe_pivot_sheet: sheet_name utf8 error: {e}")); return u32::MAX; }
    };

    let cfg: roe_polars_bridge::PivotConfig = match serde_json::from_str(&config_json) {
        Ok(c) => c,
        Err(e) => { set_last_error(format!("roe_pivot_sheet: config parse error: {e}")); return u32::MAX; }
    };

    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Calc(wb) = &mut entry.doc else {
            set_last_error("roe_pivot_sheet: 非 Calc 文档");
            return u32::MAX;
        };
        match roe_polars_bridge::pivot_sheet_into_workbook_v2(wb, sheet_idx as usize, &cfg, result_name) {
            Ok(idx) => idx as u32,
            Err(e) => { set_last_error(format!("roe_pivot_sheet: {e}")); u32::MAX }
        }
    });

    result.unwrap_or_else(|| { set_last_error("roe_pivot_sheet: 无效句柄"); u32::MAX })
}

/// 取 sheet_idx 中指定列的唯一值列表，返回 JSON 数组字符串。
/// 用于透视表筛选字段的下拉菜单候选值。
///
/// 返回 JSON 数组如 ["East","West"]，失败返回 null。
/// 调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_get_pivot_unique_values(
    handle: u64,
    sheet_idx: u32,
    field: *const c_char,
) -> *mut c_char {
    let field = match unsafe { CStr::from_ptr(field) }.to_str() {
        Ok(s) => s.to_string(),
        Err(e) => { set_last_error(format!("roe_get_pivot_unique_values: utf8 error: {e}")); return std::ptr::null_mut(); }
    };

    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Calc(wb) = &entry.doc else {
            set_last_error("roe_get_pivot_unique_values: 非 Calc 文档");
            return None;
        };
        let sheet = match wb.sheets.get(sheet_idx as usize) {
            Some(s) => s,
            None => { set_last_error(format!("roe_get_pivot_unique_values: sheet_idx={sheet_idx} 越界")); return None; }
        };
        let df = match roe_polars_bridge::sheet_to_dataframe(sheet) {
            Ok(df) => df,
            Err(e) => { set_last_error(format!("roe_get_pivot_unique_values: {e}")); return None; }
        };
        match roe_polars_bridge::df_unique_values(&df, &field) {
            Ok(json) => Some(json),
            Err(e) => { set_last_error(format!("roe_get_pivot_unique_values: {e}")); None }
        }
    });

    match result {
        Some(Some(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        _ => std::ptr::null_mut(),
    }
}

/// 取 sheet_idx 的前 n_rows 行预览，返回 JSON。
/// 格式：{"columns":[...],"rows":[[...],...],"total_rows":N}
/// 调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_sheet_preview(
    handle: u64,
    sheet_idx: u32,
    n_rows: u32,
) -> *mut c_char {
    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Calc(wb) = &entry.doc else {
            set_last_error("roe_sheet_preview: 非 Calc 文档");
            return None;
        };
        match roe_polars_bridge::sheet_preview(wb, sheet_idx as usize, n_rows as usize) {
            Ok(json) => Some(json),
            Err(e) => { set_last_error(format!("roe_sheet_preview: {e}")); None }
        }
    });

    match result {
        Some(Some(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        _ => { set_last_error("roe_sheet_preview: 无效句柄"); std::ptr::null_mut() }
    }
}

/// 取 sheet_idx 透视表头单元格坐标列表，返回 JSON。
/// 格式：[{"row":0,"col":0,"field_name":"Region","kind":"FilterField"}, ...]
/// kind 值：FilterField / FilterValue / DataHeader
/// 供前端建热区（点击区域映射）。调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_get_pivot_header_cells(
    handle: u64,
    sheet_idx: u32,
) -> *mut c_char {
    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Calc(wb) = &entry.doc else {
            set_last_error("roe_get_pivot_header_cells: 非 Calc 文档");
            return None;
        };
        let sheet = match wb.sheets.get(sheet_idx as usize) {
            Some(s) => s,
            None => { set_last_error(format!("roe_get_pivot_header_cells: sheet_idx={sheet_idx} 越界")); return None; }
        };
        let pivot = match &sheet.pivot {
            Some(p) => p,
            None => { set_last_error(format!("roe_get_pivot_header_cells: sheet_idx={sheet_idx} 无透视表元数据")); return None; }
        };
        #[derive(serde::Serialize)]
        struct CellJson {
            row: u32,
            col: u32,
            field_name: String,
            kind: String,
            has_dropdown: bool,
        }

        let mut out: Vec<CellJson> = Vec::new();

        for (fi, ff) in pivot.filter_fields.iter().enumerate() {
            out.push(CellJson {
                row: fi as u32,
                col: 0,
                field_name: ff.name.clone(),
                kind: "FilterField".to_string(),
                has_dropdown: true,
            });
        }

        if pivot.data_row_start > 0 {
            let label_row = pivot.data_row_start - 1;
            for (i, name) in pivot.row_field_names.iter().enumerate() {
                let col = i as u32;
                if col >= pivot.data_col_start { break; }
                out.push(CellJson {
                    row: label_row,
                    col,
                    field_name: name.clone(),
                    kind: "RowField".to_string(),
                    has_dropdown: true,
                });
            }
        }

        if pivot.data_col_start > 0 {
            let label_row = pivot.filter_fields.len() as u32;
            for (i, name) in pivot.col_field_names.iter().enumerate() {
                let col = pivot.data_col_start + i as u32;
                out.push(CellJson {
                    row: label_row,
                    col,
                    field_name: name.clone(),
                    kind: "ColField".to_string(),
                    has_dropdown: true,
                });
            }
        }
        match serde_json::to_string(&out) {
            Ok(json) => Some(json),
            Err(e) => { set_last_error(format!("roe_get_pivot_header_cells: serialize error: {e}")); None }
        }
    });

    match result {
        Some(Some(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        _ => std::ptr::null_mut(),
    }
}

fn read_cstr_arg(p: *const c_char, what: &str) -> Result<String, String> {
    if p.is_null() {
        return Err(format!("{what} 为空指针"));
    }
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map(|s| s.to_string())
        .map_err(|e| format!("{what} utf8 错误: {e}"))
}

/// 设置透视表某字段(页/行/列字段)的筛选值并就地刷新。
/// values_json 为 JSON 字符串数组,如 ["East","West"];空数组=清除该字段筛选。
/// 不做撤销快照,调用方用 roe_begin_edit/roe_commit_edit 包住。
/// 返回 0=成功,-1=失败(见 roe_last_error_message)。失败时 sheet 不变。
#[no_mangle]
pub extern "C" fn roe_pivot_set_filter(
    handle: u64,
    sheet_idx: u32,
    field: *const c_char,
    values_json: *const c_char,
) -> i32 {
    let field = match read_cstr_arg(field, "field") {
        Ok(v) => v,
        Err(e) => { set_last_error(format!("roe_pivot_set_filter: {e}")); return -1; }
    };
    let vj = match read_cstr_arg(values_json, "values_json") {
        Ok(v) => v,
        Err(e) => { set_last_error(format!("roe_pivot_set_filter: {e}")); return -1; }
    };
    let values: Vec<String> = match serde_json::from_str(&vj) {
        Ok(v) => v,
        Err(e) => { set_last_error(format!("roe_pivot_set_filter: values_json 解析失败: {e}")); return -1; }
    };
    let r = handle::with_entry(handle, |entry| -> Result<(), String> {
        let RoeDocument::Calc(wb) = &mut entry.doc else { return Err("非 Calc 文档".into()); };
        let idx = sheet_idx as usize;
        roe_polars_bridge::pivot_set_filter_in_place(wb, idx, &field, values.clone())
            .map_err(|e| format!("{e}"))
    });
    match r {
        Some(Ok(())) => 0,
        Some(Err(e)) => { set_last_error(format!("roe_pivot_set_filter: {e}")); -1 }
        None => { set_last_error("roe_pivot_set_filter: 无效句柄"); -1 }
    }
}

/// 设置透视表某行/列字段的排序并就地刷新。asc!=0 升序(默认,删除排序条目),asc==0 降序。
/// 不做撤销快照,调用方用 roe_begin_edit/roe_commit_edit 包住。
/// 返回 0=成功,-1=失败(见 roe_last_error_message)。失败时 sheet 不变。
#[no_mangle]
pub extern "C" fn roe_pivot_set_sort(
    handle: u64,
    sheet_idx: u32,
    field: *const c_char,
    asc: i32,
) -> i32 {
    let field = match read_cstr_arg(field, "field") {
        Ok(v) => v,
        Err(e) => { set_last_error(format!("roe_pivot_set_sort: {e}")); return -1; }
    };
    let r = handle::with_entry(handle, |entry| -> Result<(), String> {
        let RoeDocument::Calc(wb) = &mut entry.doc else { return Err("非 Calc 文档".into()); };
        let idx = sheet_idx as usize;
        roe_polars_bridge::pivot_set_sort_in_place(wb, idx, &field, asc != 0)
            .map_err(|e| format!("{e}"))
    });
    match r {
        Some(Ok(())) => 0,
        Some(Err(e)) => { set_last_error(format!("roe_pivot_set_sort: {e}")); -1 }
        None => { set_last_error("roe_pivot_set_sort: 无效句柄"); -1 }
    }
}

/// 返回透视表当前配置 JSON:
/// {"rows":[..],"cols":[..],"values":[..],"filters":[..],"source_idx":N,"fields":[数据源全部列名]}
/// values 为值字段的真实源列名;source_idx 找不到为 -1。失败返回 null。
/// 调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_get_pivot_config(handle: u64, sheet_idx: u32) -> *mut c_char {
    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Calc(wb) = &entry.doc else {
            set_last_error("roe_get_pivot_config: 非 Calc 文档");
            return None;
        };
        match roe_polars_bridge::pivot_config_value(wb, sheet_idx as usize) {
            Ok(v) => Some(v.to_string()),
            Err(e) => { set_last_error(format!("roe_get_pivot_config: {e}")); None }
        }
    });
    match result {
        Some(Some(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        _ => std::ptr::null_mut(),
    }
}

/// 就地整体设置透视表的行/列/值/页字段并刷新。
/// config_json: {"rows":[],"cols":[],"values":[],"filters":[]},缺省键视为空。
/// 不做撤销快照,调用方用 roe_begin_edit/roe_commit_edit 包住。
/// 返回 0=成功,-1=失败(见 roe_last_error_message)。失败时 sheet 不变。
#[no_mangle]
pub extern "C" fn roe_pivot_set_config(handle: u64, sheet_idx: u32, config_json: *const c_char) -> i32 {
    let cj = match read_cstr_arg(config_json, "config_json") {
        Ok(v) => v,
        Err(e) => { set_last_error(format!("roe_pivot_set_config: {e}")); return -1; }
    };
    #[derive(serde::Deserialize)]
    struct Cfg {
        #[serde(default)] rows: Vec<String>,
        #[serde(default)] cols: Vec<String>,
        #[serde(default)] values: Vec<String>,
        #[serde(default)] filters: Vec<String>,
    }
    let cfg: Cfg = match serde_json::from_str(&cj) {
        Ok(c) => c,
        Err(e) => { set_last_error(format!("roe_pivot_set_config: config_json 解析失败: {e}")); return -1; }
    };
    let r = handle::with_entry(handle, |entry| -> Result<(), String> {
        let RoeDocument::Calc(wb) = &mut entry.doc else { return Err("非 Calc 文档".into()); };
        roe_polars_bridge::set_pivot_config_in_place(
            wb, sheet_idx as usize, cfg.rows, cfg.cols, cfg.values, cfg.filters,
        ).map_err(|e| format!("{e}"))
    });
    match r {
        Some(Ok(())) => 0,
        Some(Err(e)) => { set_last_error(format!("roe_pivot_set_config: {e}")); -1 }
        None => { set_last_error("roe_pivot_set_config: 无效句柄"); -1 }
    }
}

/// 返回透视表字段按钮的单元格绝对坐标(0-indexed),与 tile.rs 实际绘制位置一致。
/// JSON 数组,元素 {kind, field_name, row, col, filtered}:
/// - FilterField: row=序号(与 base_row 无关), col=base_col+1, filtered=selected_values 非空
/// - RowField: row=base_row+data_row_start-1, col=base_col+i(i<data_col_start)
/// - ColField: row=base_row+filter_fields.len(), col=base_col+data_col_start+i
/// 按钮像素矩形由前台按单元格矩形算(btn_size=clamp(min(ch-2,cw-2),4,16),靠单元格右侧)。
/// 与 roe_get_pivot_header_cells 并存,后者坐标不含偏移,不改。
/// 调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_get_pivot_buttons(handle: u64, sheet_idx: u32) -> *mut c_char {
    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Calc(wb) = &entry.doc else {
            set_last_error("roe_get_pivot_buttons: 非 Calc 文档");
            return None;
        };
        let sheet = match wb.sheets.get(sheet_idx as usize) {
            Some(s) => s,
            None => { set_last_error(format!("roe_get_pivot_buttons: sheet_idx={sheet_idx} 越界")); return None; }
        };
        let pivot = match &sheet.pivot {
            Some(p) => p,
            None => { set_last_error(format!("roe_get_pivot_buttons: sheet_idx={sheet_idx} 无透视表元数据")); return None; }
        };
        #[derive(serde::Serialize)]
        struct BtnJson {
            kind: &'static str,
            field_name: String,
            row: u32,
            col: u32,
            filtered: bool,
            selected: Vec<String>,
            source_idx: i64,
        }
        let source_idx: i64 = wb.sheets.iter().position(|s| s.name == pivot.source_sheet).map(|i| i as i64).unwrap_or(-1);
        let mut out: Vec<BtnJson> = Vec::new();
        for (fi, ff) in pivot.filter_fields.iter().enumerate() {
            out.push(BtnJson {
                kind: "FilterField",
                field_name: ff.name.clone(),
                row: fi as u32,
                col: pivot.base_col + 1,
                selected: pivot.field_filters.get(&ff.name).cloned().unwrap_or_else(|| ff.selected_values.clone()),
                source_idx,
                filtered: !ff.selected_values.is_empty(),
            });
        }
        if pivot.data_row_start > 0 {
            let label_row = pivot.base_row + pivot.data_row_start - 1;
            for (i, name) in pivot.row_field_names.iter().enumerate() {
                if i as u32 >= pivot.data_col_start { break; }
                out.push(BtnJson {
                    kind: "RowField",
                    field_name: name.clone(),
                    row: label_row,
                    col: pivot.base_col + i as u32,
                    selected: pivot.field_filters.get(name).cloned().unwrap_or_default(),
                    source_idx,
                    filtered: pivot.field_filters.get(name).map_or(false, |v| !v.is_empty()),
                });
            }
        }
        if pivot.data_col_start > 0 {
            let label_row = pivot.base_row + pivot.filter_fields.len() as u32;
            for (i, name) in pivot.col_field_names.iter().enumerate() {
                out.push(BtnJson {
                    kind: "ColField",
                    field_name: name.clone(),
                    row: label_row,
                    col: pivot.base_col + pivot.data_col_start + i as u32,
                    selected: pivot.field_filters.get(name).cloned().unwrap_or_default(),
                    source_idx,
                    filtered: pivot.field_filters.get(name).map_or(false, |v| !v.is_empty()),
                });
            }
        }
        match serde_json::to_string(&out) {
            Ok(json) => Some(json),
            Err(e) => { set_last_error(format!("roe_get_pivot_buttons: serialize error: {e}")); None }
        }
    });
    match result {
        Some(Some(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        _ => std::ptr::null_mut(),
    }
}

/// 查询 sheet_idx 是否为透视结果表(sheet.pivot 为 Some)。
/// 返回 1=是,0=否,-1=句柄无效/非 Calc/索引越界。
/// 前台点击判定、打开文档后的面板联动应读此状态,而不是前台自己记的创建流程状态。
#[no_mangle]
pub extern "C" fn roe_sheet_is_pivot(handle: u64, sheet_idx: u32) -> i32 {
    handle::with_entry(handle, |entry| {
        let RoeDocument::Calc(wb) = &entry.doc else { return -1; };
        match wb.sheets.get(sheet_idx as usize) {
            Some(s) => if s.pivot.is_some() { 1 } else { 0 },
            None => -1,
        }
    }).unwrap_or(-1)
}

#[cfg(test)]
mod pivot_set_filter_sort_tests {
    use super::*;

    fn make_pivot() -> (u64, u32) {
        let h = crate::document::new_document_impl(2);
        assert!(h != 0);
        let script = CString::new(
            r#"set_cell(0,0,0,"R"); set_cell(0,0,1,"V");
               set_cell(0,1,0,"a"); set_cell(0,1,1,"1");
               set_cell(0,2,0,"b"); set_cell(0,2,1,"2");"#).unwrap();
        let p = crate::script::roe_execute_script(h, script.as_ptr());
        if !p.is_null() { crate::script::roe_free_string(p); }
        let cfg = CString::new(r#"{"rows":["R"],"cols":[],"values":["V"],"agg":"sum","filters":{}}"#).unwrap();
        let name = CString::new("P").unwrap();
        let idx = roe_pivot_sheet(h, 0, cfg.as_ptr(), name.as_ptr());
        assert!(idx != u32::MAX, "pivot failed");
        (h, idx)
    }

    fn rows(h: u64, idx: u32) -> Vec<String> {
        handle::with_entry(h, |e| {
            let RoeDocument::Calc(wb) = &e.doc else { panic!("非 Calc") };
            wb.sheets[idx as usize].pivot.as_ref().unwrap()
                .row_tree.children.iter().map(|c| c.value.clone()).collect()
        }).unwrap()
    }

    #[test]
    fn set_filter_applies_and_clears() {
        let (h, idx) = make_pivot();
        assert_eq!(rows(h, idx), vec!["a".to_string(), "b".to_string()]);
        let f = CString::new("R").unwrap();
        let v = CString::new(r#"["a"]"#).unwrap();
        assert_eq!(roe_pivot_set_filter(h, idx, f.as_ptr(), v.as_ptr()), 0);
        assert_eq!(rows(h, idx), vec!["a".to_string()]);
        let has = handle::with_entry(h, |e| {
            let RoeDocument::Calc(wb) = &e.doc else { panic!() };
            wb.sheets[idx as usize].pivot.as_ref().unwrap().field_filters.get("R").cloned()
        }).unwrap();
        assert_eq!(has, Some(vec!["a".to_string()]));
        let empty = CString::new("[]").unwrap();
        assert_eq!(roe_pivot_set_filter(h, idx, f.as_ptr(), empty.as_ptr()), 0);
        assert_eq!(rows(h, idx), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn set_sort_desc_then_asc() {
        let (h, idx) = make_pivot();
        let f = CString::new("R").unwrap();
        assert_eq!(roe_pivot_set_sort(h, idx, f.as_ptr(), 0), 0);
        assert_eq!(rows(h, idx), vec!["b".to_string(), "a".to_string()]);
        assert_eq!(roe_pivot_set_sort(h, idx, f.as_ptr(), 1), 0);
        assert_eq!(rows(h, idx), vec!["a".to_string(), "b".to_string()]);
        let n = handle::with_entry(h, |e| {
            let RoeDocument::Calc(wb) = &e.doc else { panic!() };
            wb.sheets[idx as usize].pivot.as_ref().unwrap().sort.len()
        }).unwrap();
        assert_eq!(n, 0, "升序应删除排序条目");
    }

    #[test]
    fn unknown_field_rejected_and_meta_unchanged() {
        let (h, idx) = make_pivot();
        let f = CString::new("NoSuch").unwrap();
        let v = CString::new(r#"["a"]"#).unwrap();
        assert_eq!(roe_pivot_set_filter(h, idx, f.as_ptr(), v.as_ptr()), -1);
        assert_eq!(roe_pivot_set_sort(h, idx, f.as_ptr(), 0), -1);
        assert_eq!(rows(h, idx), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn invalid_handle_returns_minus_one() {
        let f = CString::new("R").unwrap();
        let v = CString::new("[]").unwrap();
        assert_eq!(roe_pivot_set_filter(0, 0, f.as_ptr(), v.as_ptr()), -1);
        assert_eq!(roe_pivot_set_sort(0, 0, f.as_ptr(), 1), -1);
    }
}

#[cfg(test)]
mod pivot_config_tests {
    use super::*;

    fn make_pivot() -> (u64, u32) {
        let h = crate::document::new_document_impl(2);
        assert!(h != 0);
        let script = CString::new(
            r#"set_cell(0,0,0,"R"); set_cell(0,0,1,"K"); set_cell(0,0,2,"V");
               set_cell(0,1,0,"a"); set_cell(0,1,1,"x"); set_cell(0,1,2,"1");
               set_cell(0,2,0,"b"); set_cell(0,2,1,"y"); set_cell(0,2,2,"2");"#).unwrap();
        let p = crate::script::roe_execute_script(h, script.as_ptr());
        if !p.is_null() { crate::script::roe_free_string(p); }
        let cfg = CString::new(r#"{"rows":["R"],"cols":[],"values":["V"],"agg":"sum","filters":{}}"#).unwrap();
        let name = CString::new("P").unwrap();
        let idx = roe_pivot_sheet(h, 0, cfg.as_ptr(), name.as_ptr());
        assert!(idx != u32::MAX, "pivot failed");
        (h, idx)
    }

    #[test]
    fn get_config_reports_fields() {
        let (h, idx) = make_pivot();
        let p = roe_get_pivot_config(h, idx);
        assert!(!p.is_null());
        let txt = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
        crate::script::roe_free_string(p);
        let v: serde_json::Value = serde_json::from_str(&txt).unwrap();
        assert_eq!(v["rows"], serde_json::json!(["R"]));
        assert_eq!(v["values"], serde_json::json!(["V"]));
        assert_eq!(v["source_idx"], 0);
        assert_eq!(v["fields"], serde_json::json!(["R", "K", "V"]));
    }

    #[test]
    fn set_config_applies_and_rejects() {
        let (h, idx) = make_pivot();
        let ok = CString::new(r#"{"rows":["K"],"cols":[],"values":["V"],"filters":[]}"#).unwrap();
        assert_eq!(roe_pivot_set_config(h, idx, ok.as_ptr()), 0);
        let rows: Vec<String> = handle::with_entry(h, |e| {
            let RoeDocument::Calc(wb) = &e.doc else { panic!() };
            wb.sheets[idx as usize].pivot.as_ref().unwrap().row_field_names.clone()
        }).unwrap();
        assert_eq!(rows, vec!["K".to_string()]);
        let bad = CString::new(r#"{"rows":["Nope"],"values":["V"]}"#).unwrap();
        assert_eq!(roe_pivot_set_config(h, idx, bad.as_ptr()), -1);
        assert_eq!(roe_pivot_set_config(0, 0, ok.as_ptr()), -1);
    }

    #[test]
    fn non_pivot_sheet_returns_null() {
        let (h, _) = make_pivot();
        assert!(roe_get_pivot_config(h, 0).is_null());
    }
}

#[cfg(test)]
mod pivot_buttons_tests {
    use super::*;

    fn make_pivot(cfg_json: &str) -> (u64, u32) {
        let h = crate::document::new_document_impl(2);
        assert!(h != 0);
        let script = CString::new(
            r#"set_cell(0,0,0,"R"); set_cell(0,0,1,"V");
               set_cell(0,1,0,"a"); set_cell(0,1,1,"1");
               set_cell(0,2,0,"b"); set_cell(0,2,1,"2");"#).unwrap();
        let p = crate::script::roe_execute_script(h, script.as_ptr());
        if !p.is_null() { crate::script::roe_free_string(p); }
        let cfg = CString::new(cfg_json).unwrap();
        let name = CString::new("P").unwrap();
        let idx = roe_pivot_sheet(h, 0, cfg.as_ptr(), name.as_ptr());
        assert!(idx != u32::MAX, "pivot failed");
        (h, idx)
    }

    fn buttons(h: u64, idx: u32) -> serde_json::Value {
        let p = roe_get_pivot_buttons(h, idx);
        assert!(!p.is_null(), "roe_get_pivot_buttons 返回空指针");
        let txt = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
        crate::script::roe_free_string(p);
        serde_json::from_str(&txt).unwrap()
    }

    #[test]
    fn row_field_button_absolute_coords() {
        let (h, idx) = make_pivot(r#"{"rows":["R"],"cols":[],"values":["V"],"agg":"sum","filters":{}}"#);
        let v = buttons(h, idx);
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["kind"], "RowField");
        assert_eq!(arr[0]["field_name"], "R");
        // base_row=2, data_row_start=1 => 行 2;base_col=0 => 列 0
        assert_eq!(arr[0]["row"], 2);
        assert_eq!(arr[0]["col"], 0);
        assert_eq!(arr[0]["source_idx"], 0);
        assert_eq!(arr[0]["selected"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn filter_field_button_row_ignores_base_row() {
        let (h, idx) = make_pivot(r#"{"rows":["R"],"cols":[],"values":["V"],"agg":"sum","filters":{"R":["a"]}}"#);
        let v = buttons(h, idx);
        let arr = v.as_array().unwrap();
        let f = arr.iter().find(|x| x["kind"] == "FilterField").expect("应有页字段按钮");
        assert_eq!(f["row"], 0);
        assert_eq!(f["col"], 1);
        // 页字段存在时行字段按钮下移:data_row_start=2 => 行 base_row+2-1=3
        let r = arr.iter().find(|x| x["kind"] == "RowField").unwrap();
        assert_eq!(r["row"], 3);
    }

    #[test]
    fn invalid_handle_returns_null() {
        assert!(roe_get_pivot_buttons(0, 0).is_null());
    }
}

#[cfg(test)]
mod sheet_is_pivot_tests {
    use super::*;

    #[test]
    fn invalid_handle_and_out_of_range() {
        assert_eq!(roe_sheet_is_pivot(0, 0), -1);
        let h = crate::document::new_document_impl(2);
        assert!(h != 0);
        assert_eq!(roe_sheet_is_pivot(h, 0), 0);
        assert_eq!(roe_sheet_is_pivot(h, 99), -1);
    }

    #[test]
    fn pivot_result_sheet_is_detected() {
        let h = crate::document::new_document_impl(2);
        assert!(h != 0);
        let script = CString::new(
            r#"set_cell(0,0,0,"R"); set_cell(0,0,1,"V");
               set_cell(0,1,0,"a"); set_cell(0,1,1,"1");
               set_cell(0,2,0,"b"); set_cell(0,2,1,"2");"#).unwrap();
        let p = crate::script::roe_execute_script(h, script.as_ptr());
        if !p.is_null() { crate::script::roe_free_string(p); }
        let cfg = CString::new(r#"{"rows":["R"],"cols":[],"values":["V"],"agg":"sum","filters":{}}"#).unwrap();
        let name = CString::new("P").unwrap();
        let idx = roe_pivot_sheet(h, 0, cfg.as_ptr(), name.as_ptr());
        assert!(idx != u32::MAX, "pivot failed");
        assert_eq!(roe_sheet_is_pivot(h, idx), 1);
        assert_eq!(roe_sheet_is_pivot(h, 0), 0);
    }
}
