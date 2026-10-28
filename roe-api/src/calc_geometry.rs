//! Calc 几何查询接口（office-rs2 需求3，2026-06-21 落地）
//!
//! 对称 hit_test.rs / layout_query.rs 的 Writer 版本，但服务 Calc 文档。
//! 与 roe_hit_test / roe_query_layout_rect 不是同一套接口——参数和返回
//! 结构均不同（Writer: para_index/char_offset；Calc: row/col），命名也
//! 因此区分为 roe_calc_ 前缀，避免与已有 Writer 接口混淆。
//!
//! 坐标系约定：全部为文档坐标系（pt，未经 scroll/zoom 变换），与
//! roe_get_document_size / roe_get_render_page_size 既有约定一致。
//! 缩放滚动由前端自行处理，本模块不参与。
//!
//! 底层坐标计算复用 roe_core::tile 的 col_x/row_y/col_at_x/row_at_y/
//! col_width_at/row_height_at，与 CalcTileRenderer 渲染路径共用同一套
//! 逻辑，保证"画出来的格子"与"查询返回的坐标"始终一致。
//!
//! 合并单元格：roe_calc_hit_test 返回实际点击到的格子坐标（不强制归一化
//! 为合并区域左上角），同时通过 in_merged_range 字段附带完整合并区域范围，
//! 两者都给，调用方按场景自行选用（office-rs2 需求确认过的边界场景）。
//!
//! 冻结区域坐标：统一返回 sheet 级别文档坐标，不做"相对冻结面板局部坐标"
//! 的特殊处理——col_x/row_y 本身就是从第0列/行累加的全局坐标，天然满足
//! 这条约定，冻结面板的裁切渲染由前端自行处理。
//!
//! 超出已用数据范围的查询：col_width_at/row_height_at 对未设置宽高的
//! 列/行 fallback 到默认值，因此远超当前数据区域的格子仍能正确推算坐标，
//! 服务大表格虚拟渲染/滚动定位场景。

use std::ffi::CString;
use std::os::raw::c_char;

use roe_core::tile::{col_at_x, col_width_at, col_x, row_at_y, row_height_at, row_y, CalcRenderConfig};
use roe_model::calc::{MergedCell, Sheet};
use roe_parser::RoeDocument;

use crate::error::set_last_error;
use crate::handle;

/// 数值类查询失败时的哨兵返回值（列宽/行高不可能为负）。
const FAILURE_SENTINEL: f32 = -1.0;

/// 返回 sheet 实际有数据的最大行数（0-based，不含空行）
fn used_rows(sheet: &Sheet) -> u32 {
    sheet.cells.cols.values()
        .flat_map(|sc| sc.blocks.iter())
        .map(|b| b.start_row + b.values.len() as u32)
        .max()
        .unwrap_or(0)
}

/// 返回 sheet 实际有数据的最大列数（0-based，不含空列）
fn used_cols(sheet: &Sheet) -> u32 {
    sheet.cells.cols.keys().copied().max().map(|c| c + 1).unwrap_or(0)
}

/// 浮动图表(普通图 + 统计图)的右/下边界(pt)再加 200pt 缓冲;无图表返回 (0, 0)。
/// 用于把滚动范围撑到能看到所有图表。
fn extent_of_rects(rects: impl Iterator<Item = (f32, f32, f32, f32)>) -> (f32, f32) {
    let mut any = false;
    let (mut right, mut bottom) = (0.0f32, 0.0f32);
    for (x, y, w, h) in rects {
        any = true;
        right = right.max(x + w);
        bottom = bottom.max(y + h);
    }
    if any { (right + 200.0, bottom + 200.0) } else { (0.0, 0.0) }
}

fn floating_extent(sheet: &Sheet) -> (f32, f32) {
    extent_of_rects(
        sheet.floating_charts.iter().map(|c| (c.x_pt, c.y_pt, c.width_pt, c.height_pt))
            .chain(sheet.floating_stat_charts.iter().map(|c| (c.x_pt, c.y_pt, c.width_pt, c.height_pt))),
    )
}

/// 取出指定句柄、指定 sheet_idx 的 Sheet 引用。
/// 统一了"非 Calc 文档" / "sheet_idx 越界"两类错误的处理，供本文件内
/// 各导出函数共用，避免每个函数重复写一遍同样的 match。
fn with_sheet<R>(
    handle: u64,
    sheet_idx: u32,
    fn_name: &str,
    f: impl FnOnce(&Sheet) -> Result<R, String>,
) -> Option<R> {
    let result = self::handle::with_entry(handle, |entry| {
        let RoeDocument::Calc(workbook) = &entry.doc else {
            return Err("非 Calc 文档".to_string());
        };
        let sheet = workbook
            .sheets
            .get(sheet_idx as usize)
            .ok_or_else(|| format!("sheet_idx={sheet_idx} 越界"))?;
        f(sheet)
    });

    match result {
        None => {
            set_last_error(format!("{fn_name}: 无效句柄"));
            None
        }
        Some(Err(msg)) => {
            set_last_error(format!("{fn_name}: {msg}"));
            None
        }
        Some(Ok(v)) => Some(v),
    }
}

/// 在 merged_cells 中查找包含 (row, col) 的合并区域（若有）。
fn find_merged_range(sheet: &Sheet, row: u32, col: u32) -> Option<&MergedCell> {
    sheet
        .merged_cells
        .iter()
        .find(|m| row >= m.r1 && row <= m.r2 && col >= m.c1 && col <= m.c2)
}

fn merged_range_json_field(m: Option<&MergedCell>) -> String {
    match m {
        Some(m) => format!(
            ",\"in_merged_range\":{{\"row_start\":{},\"col_start\":{},\"row_end\":{},\"col_end\":{}}}",
            m.r1, m.c1, m.r2, m.c2
        ),
        None => String::new(),
    }
}

fn json_to_cstring(json: String, fn_name: &str) -> *mut c_char {
    match CString::new(json) {
        Ok(cs) => cs.into_raw(),
        Err(e) => {
            set_last_error(format!("{fn_name}: {e}"));
            std::ptr::null_mut()
        }
    }
}

/// 给定文档坐标 (doc_x, doc_y)，反查命中的单元格 (row, col)。
///
/// 返回 JSON：`{"row":N,"col":M}`，若命中合并区域，附加
/// `"in_merged_range":{"row_start":..,"col_start":..,"row_end":..,"col_end":..}`。
/// 返回的是实际点击到的格子坐标，不强制归一化为合并区域左上角。
///
/// 坐标越界（负值/超大值）会被钳制到最近合法格子，不算错误（与
/// roe_hit_test 的钳制语义一致，鼠标点击场景的标准 UX 预期）。
/// 仅 sheet_idx 越界或非 Calc 文档时返回 null，调用 roe_last_error_message
/// 获取详情。调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_calc_hit_test(
    handle: u64,
    sheet_idx: u32,
    doc_x: f64,
    doc_y: f64,
) -> *mut c_char {
    let doc_x = doc_x as f32;
    let doc_y = doc_y as f32;

    let json = with_sheet(handle, sheet_idx, "roe_calc_hit_test", |sheet| {
        let cfg = CalcRenderConfig::default();
        let col = col_at_x(sheet, doc_x, &cfg);
        let row = row_at_y(sheet, doc_y, &cfg);
        let merged = find_merged_range(sheet, row, col);
        Ok(format!(
            "{{\"row\":{},\"col\":{}{}}}",
            row,
            col,
            merged_range_json_field(merged)
        ))
    });

    match json {
        Some(j) => json_to_cstring(j, "roe_calc_hit_test"),
        None => std::ptr::null_mut(),
    }
}

/// 给定单元格 (row, col)，查询其在文档坐标系下的 bounding rect。
///
/// 返回 JSON：`{"x":..,"y":..,"width":..,"height":..}`，若该格属于合并
/// 区域，附加 `"in_merged_range":{...}`（同 roe_calc_hit_test 语义）。
///
/// 跨视口查询：row/col 不要求落在当前可见区域内，超出已用数据范围的
/// 格子仍按默认列宽行高正确推算返回（大表格虚拟渲染/远端选区场景）。
/// 仅 sheet_idx 越界或非 Calc 文档时返回 null。
/// 调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_calc_query_layout_rect(
    handle: u64,
    sheet_idx: u32,
    row: u32,
    col: u32,
) -> *mut c_char {
    let json = with_sheet(handle, sheet_idx, "roe_calc_query_layout_rect", |sheet| {
        let cfg = CalcRenderConfig::default();
        let x = col_x(sheet, col, &cfg);
        let y = row_y(sheet, row, &cfg);
        let w = col_width_at(sheet, col, &cfg);
        let h = row_height_at(sheet, row, &cfg);
        let merged = find_merged_range(sheet, row, col);
        Ok(format!(
            "{{\"x\":{:.2},\"y\":{:.2},\"width\":{:.2},\"height\":{:.2}{}}}",
            x, y, w, h,
            merged_range_json_field(merged)
        ))
    });

    match json {
        Some(j) => json_to_cstring(j, "roe_calc_query_layout_rect"),
        None => std::ptr::null_mut(),
    }
}

/// 查询第 col 列的列宽（pt）。该 sheet 未设置该列宽度时返回默认列宽
/// （不是错误，稀疏数据的正常情况）。
/// 失败（无效句柄/非 Calc 文档/sheet_idx 越界）返回 -1.0，调用
/// roe_last_error_message 获取详情。
#[no_mangle]
pub extern "C" fn roe_get_column_width(handle: u64, sheet_idx: u32, col: u32) -> f32 {
    with_sheet(handle, sheet_idx, "roe_get_column_width", |sheet| {
        let cfg = CalcRenderConfig::default();
        Ok(col_width_at(sheet, col, &cfg))
    })
    .unwrap_or(FAILURE_SENTINEL)
}

/// 查询第 row 行的行高（pt）。语义同 roe_get_column_width。
#[no_mangle]
pub extern "C" fn roe_get_row_height(handle: u64, sheet_idx: u32, row: u32) -> f32 {
    with_sheet(handle, sheet_idx, "roe_get_row_height", |sheet| {
        let cfg = CalcRenderConfig::default();
        Ok(row_height_at(sheet, row, &cfg))
    })
    .unwrap_or(FAILURE_SENTINEL)
}

/// 查询冻结行列范围。
/// 返回 JSON：`{"frozen_rows":N,"frozen_cols":M}`。
/// 失败返回 null，调用 roe_last_error_message 获取详情。
/// 调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_get_frozen_panes(handle: u64, sheet_idx: u32) -> *mut c_char {
    let json = with_sheet(handle, sheet_idx, "roe_get_frozen_panes", |sheet| {
        Ok(format!(
            "{{\"frozen_rows\":{},\"frozen_cols\":{}}}",
            sheet.frozen_rows, sheet.frozen_cols
        ))
    });

    match json {
        Some(j) => json_to_cstring(j, "roe_get_frozen_panes"),
        None => std::ptr::null_mut(),
    }
}

/// 给定 viewport（文档坐标系下的矩形，x/y/width/height），反查其覆盖的
/// 行列范围。
///
/// 返回 JSON：`{"row_start":..,"row_end":..,"col_start":..,"col_end":..}`
/// （均为闭区间起点、开区间终点语义：[row_start, row_end)，
/// [col_start, col_end)，与 CalcTileRenderer::paint_tile 内部 col_end/
/// row_end 的语义一致）。
/// 失败返回 null，调用 roe_last_error_message 获取详情。
/// 调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_get_visible_range(
    handle: u64,
    sheet_idx: u32,
    viewport_x: f64,
    viewport_y: f64,
    viewport_w: f64,
    viewport_h: f64,
) -> *mut c_char {
    let viewport_x = viewport_x as f32;
    let viewport_y = viewport_y as f32;
    let viewport_w = viewport_w as f32;
    let viewport_h = viewport_h as f32;

    let json = with_sheet(handle, sheet_idx, "roe_get_visible_range", |sheet| {
        let cfg = CalcRenderConfig::default();
        let col_start = col_at_x(sheet, viewport_x, &cfg);
        let col_end = col_at_x(sheet, viewport_x + viewport_w, &cfg) + 1;
        let row_start = row_at_y(sheet, viewport_y, &cfg);
        let row_end = row_at_y(sheet, viewport_y + viewport_h, &cfg) + 1;
        Ok(format!(
            "{{\"row_start\":{},\"row_end\":{},\"col_start\":{},\"col_end\":{}}}",
            row_start, row_end, col_start, col_end
        ))
    });

    match json {
        Some(j) => json_to_cstring(j, "roe_get_visible_range"),
        None => std::ptr::null_mut(),
    }
}


/// 设置第 col 列的列宽（pt）。
/// col_widths 不足时自动用默认列宽补齐到 col+1 长度。
/// 失败（无效句柄/非 Calc 文档/sheet_idx 越界）返回 false。
#[no_mangle]
pub extern "C" fn roe_set_column_width(
    handle: u64,
    sheet_idx: u32,
    col: u32,
    width_pt: f32,
) -> bool {
    let result = crate::handle::with_entry(handle, |entry| {
        let roe_parser::RoeDocument::Calc(workbook) = &mut entry.doc else {
            set_last_error("roe_set_column_width: 非 Calc 文档");
            return false;
        };
        let Some(sheet) = workbook.sheets.get_mut(sheet_idx as usize) else {
            set_last_error(format!("roe_set_column_width: sheet_idx={sheet_idx} 越界"));
            return false;
        };
        let col = col as usize;
        let cfg = CalcRenderConfig::default();
        if sheet.col_widths.len() <= col {
            sheet.col_widths.resize(col, cfg.default_col_width);
            sheet.col_widths.push(width_pt);
        } else {
            sheet.col_widths[col] = width_pt;
        }
        true
    });
    result.unwrap_or_else(|| {
        set_last_error("roe_set_column_width: 无效句柄");
        false
    })
}

/// 设置第 row 行的行高（pt）。语义同 roe_set_column_width。
#[no_mangle]
pub extern "C" fn roe_set_row_height(
    handle: u64,
    sheet_idx: u32,
    row: u32,
    height_pt: f32,
) -> bool {
    let result = crate::handle::with_entry(handle, |entry| {
        let roe_parser::RoeDocument::Calc(workbook) = &mut entry.doc else {
            set_last_error("roe_set_row_height: 非 Calc 文档");
            return false;
        };
        let Some(sheet) = workbook.sheets.get_mut(sheet_idx as usize) else {
            set_last_error(format!("roe_set_row_height: sheet_idx={sheet_idx} 越界"));
            return false;
        };
        let row = row as usize;
        // ROOT-082: C-ABI 显式调用设置行高 = 用户/前端主动指定的权威值，
        // 语义上是 Manual（不受后续自动计算覆盖），与 xlsx customHeight="true"
        // 走同一分支。resize 补齐的空缺行沿用默认值包装为 Manual，保持与
        // 改动前"直接采信数值"的行为等价（这批空缺行本就不是本次
        // API 调用的对象，不强行标记为 Auto）。
        let rh = roe_model::calc::RowHeight::Manual(height_pt);
        if sheet.row_heights.len() <= row {
            // 补齐的缺失行不是本次调用的对象,保持"待自动测量"语义(Auto),不固化为 Manual。
            let dflt = sheet.effective_default_row_height();
            sheet.row_heights.resize(row, roe_model::calc::RowHeight::Auto { current: dflt, is_dirty: true });
            sheet.row_heights.push(rh);
        } else {
            sheet.row_heights[row] = rh;
        }
        true
    });
    result.unwrap_or_else(|| {
        set_last_error("roe_set_row_height: 无效句柄");
        false
    })
}

/// 返回第 row 行的起始 y 坐标（pt，从第0行累加）。
/// 用于前端虚拟视窗精确对齐行标题。越界返回 -1.0。
#[no_mangle]
pub extern "C" fn roe_get_row_offset(handle: u64, sheet_idx: u32, row: u32) -> f32 {
    with_sheet(handle, sheet_idx, "roe_get_row_offset", |sheet| {
        let cfg = CalcRenderConfig::default();
        Ok(roe_core::tile::row_y(sheet, row, &cfg))
    })
    .unwrap_or(-1.0)
}

/// 返回第 col 列的起始 x 坐标（pt，从第0列累加）。
/// 用于前端虚拟视窗精确对齐列标题。越界返回 -1.0。
#[no_mangle]
pub extern "C" fn roe_get_col_offset(handle: u64, sheet_idx: u32, col: u32) -> f32 {
    with_sheet(handle, sheet_idx, "roe_get_col_offset", |sheet| {
        let cfg = CalcRenderConfig::default();
        Ok(roe_core::tile::col_x(sheet, col, &cfg))
    })
    .unwrap_or(-1.0)
}

/// 给定 y_pt（视口滚动偏移），返回该位置对应的行号（0-based）。
/// 找不到（空表）返回 0。
#[no_mangle]
pub extern "C" fn roe_get_row_at_offset(handle: u64, sheet_idx: u32, y_pt: f32) -> i32 {
    with_sheet(handle, sheet_idx, "roe_get_row_at_offset", |sheet| {
        let cfg = CalcRenderConfig::default();
        let mut acc = 0.0f32;
        let used = used_rows(sheet);
        for r in 0..=(used + 50) {
            let h = roe_core::tile::row_height_at(sheet, r, &cfg);
            if acc + h > y_pt {
                return Ok(r as i32);
            }
            acc += h;
        }
        Ok(used as i32)
    })
    .unwrap_or(-1)
}

/// 给定 x_pt（视口滚动偏移），返回该位置对应的列号（0-based）。
#[no_mangle]
pub extern "C" fn roe_get_col_at_offset(handle: u64, sheet_idx: u32, x_pt: f32) -> i32 {
    with_sheet(handle, sheet_idx, "roe_get_col_at_offset", |sheet| {
        let cfg = CalcRenderConfig::default();
        let mut acc = 0.0f32;
        let used = used_cols(sheet);
        for c in 0..=(used + 50) {
            let w = roe_core::tile::col_width_at(sheet, c, &cfg);
            if acc + w > x_pt {
                return Ok(c as i32);
            }
            acc += w;
        }
        Ok(used as i32)
    })
    .unwrap_or(-1)
}

/// 返回 sheet 实际使用的行列数（max_row, max_col，0-based，不含空白区域）。
/// 通过 max_row/max_col 指针返回，成功返回 1，失败返回 0。
#[no_mangle]
pub extern "C" fn roe_get_used_range(
    handle: u64,
    sheet_idx: u32,
    max_row: *mut u32,
    max_col: *mut u32,
) -> i32 {
    let result = with_sheet(handle, sheet_idx, "roe_get_used_range", |sheet| {
        Ok((used_rows(sheet), used_cols(sheet)))
    });
    match result {
        Some((r, c)) => {
            if !max_row.is_null() { unsafe { *max_row = r; } }
            if !max_col.is_null() { unsafe { *max_col = c; } }
            1
        }
        None => 0,
    }
}

/// 返回 sheet 全部行的总高度（pt）。用于前端设置 Flickable content-height。
#[no_mangle]
pub extern "C" fn roe_get_sheet_total_height(handle: u64, sheet_idx: u32) -> f32 {
    with_sheet(handle, sheet_idx, "roe_get_sheet_total_height", |sheet| {
        let cfg = CalcRenderConfig::default();
        let rows = used_rows(sheet) + 50; // 加50行空白缓冲
        let cells_h = (0..rows).map(|r| roe_core::tile::row_height_at(sheet, r, &cfg)).sum::<f32>();
        Ok(cells_h.max(floating_extent(sheet).1))
    })
    .unwrap_or(-1.0)
}

/// 返回 sheet 全部列的总宽度（pt）。用于前端设置 Flickable content-width。
#[no_mangle]
pub extern "C" fn roe_get_sheet_total_width(handle: u64, sheet_idx: u32) -> f32 {
    with_sheet(handle, sheet_idx, "roe_get_sheet_total_width", |sheet| {
        let cfg = CalcRenderConfig::default();
        let cols = used_cols(sheet) + 20; // 加20列空白缓冲
        let cells_w = (0..cols).map(|c| roe_core::tile::col_width_at(sheet, c, &cfg)).sum::<f32>();
        Ok(cells_w.max(floating_extent(sheet).0))
    })
    .unwrap_or(-1.0)
}

// ─────────────────────────────────────────────────────────────────────────────
// Sheet 元数据查询 / 增删改（office-rs2 需求 2026-06-29）
// ─────────────────────────────────────────────────────────────────────────────

/// 返回当前文档的 Sheet 数量。非 Calc 文档或无效句柄返回 -1。
#[no_mangle]
pub extern "C" fn roe_get_sheet_count(handle: u64) -> i32 {
    let result = crate::handle::with_entry(handle, |entry| {
        match &entry.doc {
            RoeDocument::Calc(wb) => wb.sheets.len() as i32,
            _ => {
                set_last_error("roe_get_sheet_count: 非 Calc 文档");
                -1
            }
        }
    });
    result.unwrap_or_else(|| {
        set_last_error("roe_get_sheet_count: 无效句柄");
        -1
    })
}

/// 返回指定 Sheet 的名称（JSON：{"name":"Sheet1"}）。
/// 失败返回 null，调用 roe_last_error_message 获取详情。
/// 返回值需调用 roe_free_string 释放。
#[no_mangle]
pub extern "C" fn roe_get_sheet_name(handle: u64, sheet_idx: u32) -> *mut c_char {
    use std::ffi::CString;
    let result = crate::handle::with_entry(handle, |entry| {
        let RoeDocument::Calc(wb) = &entry.doc else {
            set_last_error("roe_get_sheet_name: 非 Calc 文档");
            return None;
        };
        let sheet = wb.sheets.get(sheet_idx as usize)?;
        Some(format!(r#"{{"name":"{}"}}"#, sheet.name))
    });
    match result {
        Some(Some(json)) => CString::new(json).map(|cs| cs.into_raw()).unwrap_or(std::ptr::null_mut()),
        Some(None) => std::ptr::null_mut(), // sheet_idx 越界，错误已在闭包内设置
        None => {
            set_last_error("roe_get_sheet_name: 无效句柄");
            std::ptr::null_mut()
        }
    }
}

/// 返回指定 Sheet 有数据的最大行数。失败返回 -1。
#[no_mangle]
pub extern "C" fn roe_get_row_count(handle: u64, sheet_idx: u32) -> i32 {
    with_sheet(handle, sheet_idx, "roe_get_row_count", |sheet| {
        Ok(used_rows(sheet) as i32)
    }).unwrap_or(-1)
}

/// 返回指定 Sheet 有数据的最大列数。失败返回 -1。
#[no_mangle]
pub extern "C" fn roe_get_col_count(handle: u64, sheet_idx: u32) -> i32 {
    with_sheet(handle, sheet_idx, "roe_get_col_count", |sheet| {
        Ok(used_cols(sheet) as i32)
    }).unwrap_or(-1)
}

/// 在末尾新增一个 Sheet，返回新 Sheet 的 idx（0-based）。失败返回 -1。
#[no_mangle]
pub extern "C" fn roe_add_sheet(handle: u64, name: *const std::os::raw::c_char) -> i32 {
    use std::ffi::CStr;
    let name_str = if name.is_null() {
        set_last_error("roe_add_sheet: name is null");
        return -1;
    } else {
        match unsafe { CStr::from_ptr(name) }.to_str() {
            Ok(s) => s.to_owned(),
            Err(_) => {
                set_last_error("roe_add_sheet: invalid utf8");
                return -1;
            }
        }
    };

    let result = crate::handle::with_entry(handle, |entry| {
        let RoeDocument::Calc(wb) = &mut entry.doc else {
            set_last_error("roe_add_sheet: 非 Calc 文档");
            return -1;
        };
        let new_sheet = roe_model::calc::Sheet {
            col_style: vec![],
            name: name_str.clone(),
            default_col_width: 48.99,
            default_row_height: 14.25,
            ..Default::default()
        };
        wb.sheets.push(new_sheet);
        (wb.sheets.len() - 1) as i32
    });
    result.unwrap_or_else(|| {
        set_last_error("roe_add_sheet: 无效句柄");
        -1
    })
}

/// 删除指定 Sheet。不能删除最后一个 Sheet。成功返回 0，失败返回非 0。
#[no_mangle]
pub extern "C" fn roe_remove_sheet(handle: u64, sheet_idx: u32) -> i32 {
    let result = crate::handle::with_entry(handle, |entry| {
        let RoeDocument::Calc(wb) = &mut entry.doc else {
            set_last_error("roe_remove_sheet: 非 Calc 文档");
            return 1;
        };
        if wb.sheets.len() <= 1 {
            set_last_error("roe_remove_sheet: 不能删除最后一个 Sheet");
            return 1;
        }
        if sheet_idx as usize >= wb.sheets.len() {
            set_last_error("roe_remove_sheet: sheet_idx 越界");
            return 1;
        }
        wb.sheets.remove(sheet_idx as usize);
        0
    });
    result.unwrap_or_else(|| {
        set_last_error("roe_remove_sheet: 无效句柄");
        1
    })
}

/// 重命名指定 Sheet。成功返回 0，失败返回非 0。
#[no_mangle]
pub extern "C" fn roe_rename_sheet(handle: u64, sheet_idx: u32, name: *const std::os::raw::c_char) -> i32 {
    use std::ffi::CStr;
    let name_str = if name.is_null() {
        set_last_error("roe_rename_sheet: name is null");
        return 1;
    } else {
        match unsafe { CStr::from_ptr(name) }.to_str() {
            Ok(s) => s.to_owned(),
            Err(_) => {
                set_last_error("roe_rename_sheet: invalid utf8");
                return 1;
            }
        }
    };

    let result = crate::handle::with_entry(handle, |entry| {
        let RoeDocument::Calc(wb) = &mut entry.doc else {
            set_last_error("roe_rename_sheet: 非 Calc 文档");
            return 1;
        };
        match wb.sheets.get_mut(sheet_idx as usize) {
            Some(sheet) => { sheet.name = name_str.clone(); 0 }
            None => {
                set_last_error("roe_rename_sheet: sheet_idx 越界");
                1
            }
        }
    });
    result.unwrap_or_else(|| {
        set_last_error("roe_rename_sheet: 无效句柄");
        1
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::WriterOptions;
    use roe_model::calc::{MergedCell, Sheet, SparseGrid, Workbook};
    use std::collections::{HashMap, VecDeque};

    fn make_calc_handle() -> u64 {
        let sheet = Sheet {
            col_style: vec![],
            name: "Sheet1".into(),
            cells: SparseGrid::default(),
            col_widths: vec![50.0, 200.0, 30.0],
            row_heights: vec![roe_model::calc::RowHeight::Manual(20.0), roe_model::calc::RowHeight::Manual(40.0)],
            frozen_rows: 1,
            frozen_cols: 2,
            cell_formats: HashMap::new(),
            cond_formats: vec![],
            data_validations: vec![],
            auto_filter: None,
            merged_cells: vec![MergedCell { r1: 0, c1: 0, r2: 1, c2: 1 }],
            pivot:        None,
            ..Default::default()
        };
        let wb = Workbook {
            sheets: vec![sheet],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
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

    fn json_from_ptr(ptr: *mut c_char) -> String {
        assert!(!ptr.is_null());
        unsafe { CString::from_raw(ptr) }.into_string().unwrap()
    }

    #[test]
    fn test_get_column_width_real_value() {
        let h = make_calc_handle();
        assert_eq!(roe_get_column_width(h, 0, 0), 50.0);
        assert_eq!(roe_get_column_width(h, 0, 1), 200.0);
        // 越界列：fallback 默认值
        assert_eq!(roe_get_column_width(h, 0, 99), CalcRenderConfig::default().default_col_width);
        handle::remove(h);
    }

    #[test]
    fn test_get_row_height_real_value() {
        let h = make_calc_handle();
        assert_eq!(roe_get_row_height(h, 0, 0), 20.0);
        assert_eq!(roe_get_row_height(h, 0, 1), 40.0);
        handle::remove(h);
    }

    #[test]
    fn test_get_column_width_invalid_sheet_idx_returns_sentinel() {
        let h = make_calc_handle();
        assert_eq!(roe_get_column_width(h, 99, 0), FAILURE_SENTINEL);
        handle::remove(h);
    }

    #[test]
    fn test_get_column_width_invalid_handle_returns_sentinel() {
        assert_eq!(roe_get_column_width(999_999, 0, 0), FAILURE_SENTINEL);
    }

    #[test]
    fn test_hit_test_basic() {
        let h = make_calc_handle();
        // doc_x=0, doc_y=0 应命中 row=0, col=0
        let ptr = roe_calc_hit_test(h, 0, 0.0, 0.0);
        let json = json_from_ptr(ptr);
        assert!(json.contains("\"row\":0"));
        assert!(json.contains("\"col\":0"));
        handle::remove(h);
    }

    #[test]
    fn test_hit_test_reports_merged_range() {
        let h = make_calc_handle();
        // (row=0, col=0) 落在 merged_cells 的 (0,0)-(1,1) 范围内
        let ptr = roe_calc_hit_test(h, 0, 0.0, 0.0);
        let json = json_from_ptr(ptr);
        assert!(json.contains("in_merged_range"), "应包含合并区域信息: {json}");
        assert!(json.contains("\"row_end\":1"));
        assert!(json.contains("\"col_end\":1"));
        handle::remove(h);
    }

    #[test]
    fn test_hit_test_outside_merged_range_no_field() {
        let h = make_calc_handle();
        // col_widths累加: col0=0..50, col1=50..250, col2=250..280
        // x=260 落在 col2（不在合并区域内）
        let ptr = roe_calc_hit_test(h, 0, 260.0, 0.0);
        let json = json_from_ptr(ptr);
        assert!(!json.contains("in_merged_range"), "不应包含合并区域信息: {json}");
        handle::remove(h);
    }

    #[test]
    fn test_query_layout_rect_roundtrip_with_hit_test() {
        let h = make_calc_handle();
        // 查 row=0,col=1 的矩形
        let rect_ptr = roe_calc_query_layout_rect(h, 0, 0, 1);
        let rect_json = json_from_ptr(rect_ptr);
        assert!(rect_json.contains("\"x\":50.00"), "col1 起点应为 50.0: {rect_json}");
        assert!(rect_json.contains("\"width\":200.00"));
        handle::remove(h);
    }

    #[test]
    fn test_query_layout_rect_invalid_sheet_idx_returns_null() {
        let h = make_calc_handle();
        let ptr = roe_calc_query_layout_rect(h, 99, 0, 0);
        assert!(ptr.is_null());
        handle::remove(h);
    }

    #[test]
    fn test_frozen_panes() {
        let h = make_calc_handle();
        let ptr = roe_get_frozen_panes(h, 0);
        let json = json_from_ptr(ptr);
        assert!(json.contains("\"frozen_rows\":1"));
        assert!(json.contains("\"frozen_cols\":2"));
        handle::remove(h);
    }

    #[test]
    fn test_frozen_panes_invalid_handle_returns_null() {
        let ptr = roe_get_frozen_panes(999_999, 0);
        assert!(ptr.is_null());
    }

    #[test]
    fn test_visible_range_basic() {
        let h = make_calc_handle();
        // viewport 覆盖 x:[0,60) y:[0,25) -> 应覆盖 col0(0-50),col1起点(50+)，row0(0-20),row1起点(20+)
        let ptr = roe_get_visible_range(h, 0, 0.0, 0.0, 60.0, 25.0);
        let json = json_from_ptr(ptr);
        assert!(json.contains("\"col_start\":0"));
        assert!(json.contains("\"row_start\":0"));
        handle::remove(h);
    }

    #[test]
    fn test_visible_range_far_beyond_data_still_computes() {
        // 超出已用数据范围（col_widths 只有3个元素）的远端查询，仍应正确推算
        let h = make_calc_handle();
        let ptr = roe_get_visible_range(h, 0, 100000.0, 100000.0, 500.0, 500.0);
        assert!(!ptr.is_null(), "远超数据范围的 viewport 查询不应失败");
        handle::remove(h);
    }

    #[test]
    fn test_non_calc_document_returns_failure() {
        use roe_parser::odf::fodt::FodtParser;
        const SAMPLE_FODT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
    office:mimetype="application/vnd.oasis.opendocument.text">
  <office:body><office:text><text:p>x</text:p></office:text></office:body>
</office:document>"#;
        let doc = FodtParser::parse(SAMPLE_FODT).expect("parse failed");
        let h = handle::insert(handle::DocumentEntry {
            doc: RoeDocument::Writer(doc),
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
        });
        assert_eq!(roe_get_column_width(h, 0, 0), FAILURE_SENTINEL);
        assert!(roe_get_frozen_panes(h, 0).is_null());
        handle::remove(h);
    }
}

#[cfg(test)]
mod floating_extent_tests {
    use super::extent_of_rects;

    #[test]
    fn empty_is_zero() {
        assert_eq!(extent_of_rects(std::iter::empty()), (0.0, 0.0));
    }

    #[test]
    fn takes_max_right_bottom_plus_buffer() {
        let rects = vec![(10.0, 20.0, 100.0, 50.0), (300.0, 400.0, 200.0, 100.0)];
        assert_eq!(extent_of_rects(rects.into_iter()), (700.0, 700.0));
    }
}
