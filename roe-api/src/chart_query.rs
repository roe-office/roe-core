//! 图表查询/修改 C-ABI 接口（office-rs2 图表格式侧边栏 panel 需求，P0）
//!
//! 依赖 ChartModel.id（自增，同 sheet 内唯一，0 为无效哨兵值）。
//! 定位方式统一为 (sheet_idx, chart_id)，不使用数组下标，避免删除/并发
//! 编辑导致下标错位。
//!
//! 与 calc_geometry.rs 的 roe_calc_* 系列是姊妹接口，但服务对象是
//! floating_charts 而非单元格网格，因此使用独立的 roe_chart_* 前缀。

use std::ffi::CStr;
use std::os::raw::c_char;

use roe_model::calc::Sheet;
use roe_model::impress::ChartModel;
use roe_parser::RoeDocument;

use crate::error::set_last_error;
use crate::handle;

/// 取出指定句柄、指定 sheet_idx 的 Sheet 只读引用，供查询类接口使用。
fn with_sheet<R>(
    handle: u64,
    sheet_idx: u32,
    fn_name: &str,
    f: impl FnOnce(&Sheet) -> Result<R, String>,
) -> Option<R> {
    let result = handle::with_entry(handle, |entry| {
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

/// 取出指定句柄、指定 sheet_idx 的 Sheet 可写引用，供修改类接口使用。
fn with_sheet_mut<R>(
    handle: u64,
    sheet_idx: u32,
    fn_name: &str,
    f: impl FnOnce(&mut Sheet) -> Result<R, String>,
) -> Option<R> {
    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Calc(workbook) = &mut entry.doc else {
            return Err("非 Calc 文档".to_string());
        };
        let sheet = workbook
            .sheets
            .get_mut(sheet_idx as usize)
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

fn find_chart(sheet: &Sheet, chart_id: u64) -> Option<&ChartModel> {
    sheet.floating_charts.iter().find(|c| c.id == chart_id)
}

fn find_chart_mut(sheet: &mut Sheet, chart_id: u64) -> Option<&mut ChartModel> {
    sheet.floating_charts.iter_mut().find(|c| c.id == chart_id)
}

/// 按 id 取图表矩形(普通图或统计图;两类共用 id 空间)。
fn chart_rect(sheet: &Sheet, id: u64) -> Option<(f32, f32, f32, f32)> {
    if let Some(c) = sheet.floating_charts.iter().find(|c| c.id == id) {
        return Some((c.x_pt, c.y_pt, c.width_pt, c.height_pt));
    }
    sheet.floating_stat_charts.iter().find(|c| c.id == id)
        .map(|c| (c.x_pt, c.y_pt, c.width_pt, c.height_pt))
}

/// 按 id 取图表矩形的可变引用(x, y, w, h);只管几何,不碰模型内容。
fn chart_rect_mut(sheet: &mut Sheet, id: u64) -> Option<(&mut f32, &mut f32, &mut f32, &mut f32)> {
    if let Some(c) = sheet.floating_charts.iter_mut().find(|c| c.id == id) {
        return Some((&mut c.x_pt, &mut c.y_pt, &mut c.width_pt, &mut c.height_pt));
    }
    if let Some(c) = sheet.floating_stat_charts.iter_mut().find(|c| c.id == id) {
        return Some((&mut c.x_pt, &mut c.y_pt, &mut c.width_pt, &mut c.height_pt));
    }
    None
}

fn color_to_hex(c: &roe_model::common::Color) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
}

fn parse_hex_color(s: &str) -> Option<roe_model::common::Color> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 { return None; }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(roe_model::common::Color { r, g, b, a: 255 })
}

fn json_to_cstring(json: String, fn_name: &str) -> *mut c_char {
    match std::ffi::CString::new(json) {
        Ok(cs) => cs.into_raw(),
        Err(e) => {
            set_last_error(format!("{fn_name}: {e}"));
            std::ptr::null_mut()
        }
    }
}

/// 查询图表当前位置/尺寸。返回 JSON `{"x_pt":..,"y_pt":..,"width_pt":..,"height_pt":..}`，
/// 失败（无效句柄/非 Calc 文档/sheet_idx 越界/chart_id 不存在）返回 null，
/// 详情见 roe_last_error_message。
#[no_mangle]
pub extern "C" fn roe_get_chart_rect(handle: u64, sheet_idx: u32, chart_id: u64) -> *mut c_char {
    let json = with_sheet(handle, sheet_idx, "roe_get_chart_rect", |sheet| {
        let (rx, ry, rw, rh) = chart_rect(sheet, chart_id)
            .ok_or_else(|| format!("chart_id={chart_id} 不存在"))?;
        Ok(format!(
            "{{\"x_pt\":{:.2},\"y_pt\":{:.2},\"width_pt\":{:.2},\"height_pt\":{:.2}}}",
            rx, ry, rw, rh
        ))
    });
    match json {
        Some(j) => json_to_cstring(j, "roe_get_chart_rect"),
        None => std::ptr::null_mut(),
    }
}

/// 查询图表完整配置（含格式化字段），供侧边栏面板初始化显示用。
/// 返回 JSON，失败返回 null。
#[no_mangle]
pub extern "C" fn roe_get_chart_config(handle: u64, sheet_idx: u32, chart_id: u64) -> *mut c_char {
    let json = with_sheet(handle, sheet_idx, "roe_get_chart_config", |sheet| {
        // 统计图(chartEx)只暴露与普通图共有的三项;"stat":true 供前台置灰其余控件
        if let Some(sc) = sheet.floating_stat_charts.iter().find(|c| c.id == chart_id) {
            let m = &sc.model;
            let title = match &m.title {
                Some(t) => serde_json::to_string(t).unwrap_or_else(|_| "null".to_string()),
                None => "null".to_string(),
            };
            return Ok(format!(
                "{{\"id\":{},\"stat\":true,\"kind\":\"{:?}\",\"title\":{},\"show_legend\":{},\"show_grid\":{}}}",
                sc.id, m.kind, title, m.show_legend, m.show_grid
            ));
        }
        let chart = find_chart(sheet, chart_id)
            .ok_or_else(|| format!("chart_id={chart_id} 不存在"))?;
        let kind_str = match chart.kind {
            roe_model::impress::ChartKind::Bar => "Bar",
            roe_model::impress::ChartKind::HorizontalBar => "HorizontalBar",
            roe_model::impress::ChartKind::Line => "Line",
            roe_model::impress::ChartKind::Pie => "Pie",
            roe_model::impress::ChartKind::Scatter => "Scatter",
            roe_model::impress::ChartKind::Radar => "Radar",
            roe_model::impress::ChartKind::Area => "Area",
            roe_model::impress::ChartKind::Doughnut => "Doughnut",
        };
        let opt_f32 = |o: Option<f32>| o.map(|v| format!("{:.2}", v)).unwrap_or_else(|| "null".to_string());
        #[allow(unreachable_patterns)]
        let grouping_str = match chart.grouping {
            roe_model::impress::ChartGrouping::Standard => "Standard",
            roe_model::impress::ChartGrouping::Stacked => "Stacked",
            roe_model::impress::ChartGrouping::PercentStacked => "PercentStacked",
            _ => "Standard",
        };
        let title_json = serde_json::to_string(&chart.title).unwrap_or_else(|_| "null".to_string());
        let x_label_json = serde_json::to_string(&chart.x_axis_label).unwrap_or_else(|_| "\"\"".to_string());
        let y_label_json = serde_json::to_string(&chart.y_axis_label).unwrap_or_else(|_| "\"\"".to_string());
        Ok(format!(
            "{{\"id\":{},\"kind\":\"{}\",\"title\":{},\"x_pt\":{:.2},\"y_pt\":{:.2},\"width_pt\":{:.2},\"height_pt\":{:.2},\"show_legend\":{},\"show_grid\":{},\"x_axis_label\":{},\"y_axis_label\":{},\"border_color\":\"{}\",\"border_width\":{:.2},\"grouping\":\"{}\",\"show_val\":{},\"gap_width_pct\":{},\"overlap_pct\":{},\"hole_size_pct\":{}}}",
            chart.id, kind_str, title_json, chart.x_pt, chart.y_pt, chart.width_pt, chart.height_pt,
            chart.show_legend, chart.show_grid, x_label_json, y_label_json,
            color_to_hex(&chart.border_color), chart.border_width,
            grouping_str, chart.data_labels.show_val,
            opt_f32(chart.gap_width_pct), opt_f32(chart.overlap_pct), opt_f32(chart.hole_size_pct)
        ))
    });
    match json {
        Some(j) => json_to_cstring(j, "roe_get_chart_config"),
        None => std::ptr::null_mut(),
    }
}

/// 修改图表尺寸。成功返回 true，失败（含 chart_id 不存在）返回 false。
#[no_mangle]
pub extern "C" fn roe_resize_chart(handle: u64, sheet_idx: u32, chart_id: u64, width_pt: f32, height_pt: f32) -> bool {
    let result = with_sheet_mut(handle, sheet_idx, "roe_resize_chart", |sheet| {
        let (_, _, w, h) = chart_rect_mut(sheet, chart_id)
            .ok_or_else(|| format!("chart_id={chart_id} 不存在"))?;
        *w = width_pt;
        *h = height_pt;
        Ok(())
    });
    result.is_some()
}

/// 修改图表位置。成功返回 true，失败返回 false。
#[no_mangle]
pub extern "C" fn roe_move_chart(handle: u64, sheet_idx: u32, chart_id: u64, x_pt: f32, y_pt: f32) -> bool {
    let result = with_sheet_mut(handle, sheet_idx, "roe_move_chart", |sheet| {
        let (x, y, _, _) = chart_rect_mut(sheet, chart_id)
            .ok_or_else(|| format!("chart_id={chart_id} 不存在"))?;
        *x = x_pt;
        *y = y_pt;
        Ok(())
    });
    result.is_some()
}

/// 原子设置图表完整矩形（位置+尺寸一次提交）。成功返回 true，失败返回 false。
/// 用于前端8方向缩放句柄（nw/n/ne/w/e/sw/s/se），避免 resize+move 两次调用
/// 之间的中间不一致窗口，调用方直接传入拖拽结束后的最终矩形即可。
#[no_mangle]
pub extern "C" fn roe_set_chart_rect(
    handle: u64,
    sheet_idx: u32,
    chart_id: u64,
    x_pt: f32,
    y_pt: f32,
    width_pt: f32,
    height_pt: f32,
) -> bool {
    let result = with_sheet_mut(handle, sheet_idx, "roe_set_chart_rect", |sheet| {
        let (x, y, w, h) = chart_rect_mut(sheet, chart_id)
            .ok_or_else(|| format!("chart_id={chart_id} 不存在"))?;
        *x = x_pt;
        *y = y_pt;
        *w = width_pt;
        *h = height_pt;
        Ok(())
    });
    result.is_some()
}

/// 在 Calc 指定 sheet 插入浮动图片。source 以 "base64:" 或 "data:image/" 开头按 base64,否则按本地路径;
/// 按文件头识别 PNG/JPEG,上限 10 MB。x/y/w/h 单位 pt;w、h 传 0 按原比例(都为 0 时宽 200pt)。
/// 成功返回新对象 id,失败返回 -1(详情见 roe_last_error_message)。
#[no_mangle]
pub extern "C" fn roe_calc_insert_image(
    handle: u64,
    sheet_idx: u32,
    source: *const c_char,
    x_pt: f32,
    y_pt: f32,
    width_pt: f32,
    height_pt: f32,
) -> i64 {
    if source.is_null() {
        set_last_error("roe_calc_insert_image: source 为空指针");
        return -1;
    }
    let src = match unsafe { CStr::from_ptr(source) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => {
            set_last_error("roe_calc_insert_image: source 不是 UTF-8");
            return -1;
        }
    };
    let r = with_sheet_mut(handle, sheet_idx, "roe_calc_insert_image", |sheet| {
        roe_model::object_edit::insert_image(sheet, &src, x_pt, y_pt, width_pt, height_pt)
    });
    match r {
        Some(id) => id as i64,
        None => -1,
    }
}

fn calc_c_str_arg(p: *const c_char) -> Option<String> {
    if p.is_null() { return None; }
    unsafe { CStr::from_ptr(p) }.to_str().ok().map(|s| s.to_string())
}

/// 在 Calc 指定 sheet 插入浮动图形。geometry 为 rect/ellipse/line 或 OOXML 预设名(如 heart);
/// fill 为 "#rrggbb",传 null 或空串表示无填充。x/y/w/h 单位 pt。
/// 成功返回新对象 id,失败返回 -1(详情见 roe_last_error_message)。
#[no_mangle]
pub extern "C" fn roe_calc_insert_shape(
    handle: u64,
    sheet_idx: u32,
    geometry: *const c_char,
    x_pt: f32,
    y_pt: f32,
    width_pt: f32,
    height_pt: f32,
    fill: *const c_char,
) -> i64 {
    let Some(geo) = calc_c_str_arg(geometry) else {
        set_last_error("roe_calc_insert_shape: geometry 为空指针或不是 UTF-8");
        return -1;
    };
    let fill_s = calc_c_str_arg(fill).unwrap_or_default();
    let r = with_sheet_mut(handle, sheet_idx, "roe_calc_insert_shape", |sheet| {
        roe_model::object_edit::insert_shape(sheet, &geo, x_pt, y_pt, width_pt, height_pt, &fill_s)
    });
    match r { Some(id) => id as i64, None => -1 }
}

/// 在 Calc 指定 sheet 插入文本框(无填充矩形,\n 分段)。成功返回新对象 id,失败返回 -1。
#[no_mangle]
pub extern "C" fn roe_calc_insert_textbox(
    handle: u64,
    sheet_idx: u32,
    x_pt: f32,
    y_pt: f32,
    width_pt: f32,
    height_pt: f32,
    text: *const c_char,
) -> i64 {
    let Some(t) = calc_c_str_arg(text) else {
        set_last_error("roe_calc_insert_textbox: text 为空指针或不是 UTF-8");
        return -1;
    };
    let r = with_sheet_mut(handle, sheet_idx, "roe_calc_insert_textbox", |sheet| {
        roe_model::object_edit::insert_textbox(sheet, x_pt, y_pt, width_pt, height_pt, &t)
    });
    match r { Some(id) => id as i64, None => -1 }
}

/// 整体替换图形/文本框的文字(\n 分段)。成功返回 true;id 不存在、对象不是图形、文字过长返回 false。
#[no_mangle]
pub extern "C" fn roe_calc_set_object_text(handle: u64, sheet_idx: u32, object_id: u64, text: *const c_char) -> bool {
    let Some(t) = calc_c_str_arg(text) else {
        set_last_error("roe_calc_set_object_text: text 为空指针或不是 UTF-8");
        return false;
    };
    let r = with_sheet_mut(handle, sheet_idx, "roe_calc_set_object_text", |sheet| {
        roe_model::object_edit::set_object_text(sheet, object_id, &t)
    });
    r.is_some()
}

/// 删除任意浮动对象(图表/统计图/图片/图形,共用 id 空间)。成功返回 true;
/// id 不存在返回 false(详情见 roe_last_error_message)。删除后 id 不复用。
#[no_mangle]
pub extern "C" fn roe_calc_delete_object(handle: u64, sheet_idx: u32, object_id: u64) -> bool {
    let result = with_sheet_mut(handle, sheet_idx, "roe_calc_delete_object", |sheet| {
        if roe_model::object_edit::delete_object(sheet, object_id) {
            Ok(())
        } else {
            Err(format!("object_id={object_id} 不存在"))
        }
    });
    result.is_some()
}

/// 原子设置任意浮动对象(图表/统计图/图片/图形,共用 id 空间)的位置与尺寸,单位 pt。
/// 旋转角不变。成功返回 true;id 不存在或参数非法返回 false(详情见 roe_last_error_message)。
#[no_mangle]
pub extern "C" fn roe_calc_set_object_rect(
    handle: u64,
    sheet_idx: u32,
    object_id: u64,
    x_pt: f32,
    y_pt: f32,
    width_pt: f32,
    height_pt: f32,
) -> bool {
    let result = with_sheet_mut(handle, sheet_idx, "roe_calc_set_object_rect", |sheet| {
        roe_model::object_edit::set_object_rect(sheet, object_id, x_pt, y_pt, width_pt, height_pt)
    });
    result.is_some()
}

/// 设置图形/文本框填充色。fill 为 #rrggbb;空串或 none 表示无填充。只对图形/文本框有效。
/// 成功返回 true;id 不存在、对象不是图形或参数非法返回 false(详情见 roe_last_error_message)。
#[no_mangle]
pub extern "C" fn roe_calc_set_object_fill(handle: u64, sheet_idx: u32, object_id: u64, fill: *const c_char) -> bool {
    if fill.is_null() {
        set_last_error("roe_calc_set_object_fill: fill 为空指针".to_string());
        return false;
    }
    let fill = match unsafe { CStr::from_ptr(fill) }.to_str() {
        Ok(s) => s.to_string(),
        Err(e) => {
            set_last_error(format!("roe_calc_set_object_fill: utf8 error: {e}"));
            return false;
        }
    };
    let result = with_sheet_mut(handle, sheet_idx, "roe_calc_set_object_fill", |sheet| {
        roe_model::object_edit::set_object_fill(sheet, object_id, &fill)
    });
    result.is_some()
}

/// 设置图形/文本框边框。color 为 #rrggbb;空串或 none 表示无边框。width_pt 范围 (0,100]。
/// 成功返回 true;id 不存在、对象不是图形或参数非法返回 false(详情见 roe_last_error_message)。
#[no_mangle]
pub extern "C" fn roe_calc_set_object_stroke(handle: u64, sheet_idx: u32, object_id: u64, color: *const c_char, width_pt: f32) -> bool {
    if color.is_null() {
        set_last_error("roe_calc_set_object_stroke: color 为空指针".to_string());
        return false;
    }
    let color = match unsafe { CStr::from_ptr(color) }.to_str() {
        Ok(s) => s.to_string(),
        Err(e) => {
            set_last_error(format!("roe_calc_set_object_stroke: utf8 error: {e}"));
            return false;
        }
    };
    let result = with_sheet_mut(handle, sheet_idx, "roe_calc_set_object_stroke", |sheet| {
        roe_model::object_edit::set_object_stroke(sheet, object_id, &color, width_pt)
    });
    result.is_some()
}

/// 修改图表格式化字段（局部 patch，未提供的字段保持原值不变）。
/// style_json 支持字段：kind/title(可为null)/show_legend/show_grid/x_axis_label/y_axis_label/
/// border_color(hex字符串"#rrggbb")/border_width/
/// grouping("Standard"|"Stacked"|"PercentStacked")/show_val(数据标签)/
/// gap_width_pct/overlap_pct/hole_size_pct(数字,传 null 表示清除)，均为可选。
/// 成功返回 true，失败（json 解析失败/chart_id 不存在）返回 false。
#[no_mangle]
pub extern "C" fn roe_set_chart_style(handle: u64, sheet_idx: u32, chart_id: u64, style_json: *const c_char) -> bool {
    let json_str = match unsafe { CStr::from_ptr(style_json) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!("roe_set_chart_style: utf8 error: {e}"));
            return false;
        }
    };
    let val: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => {
            set_last_error(format!("roe_set_chart_style: json parse error: {e}"));
            return false;
        }
    };

    let result = with_sheet_mut(handle, sheet_idx, "roe_set_chart_style", |sheet| {
        // 校验与应用在 roe_core::chart_style(与 Rhai set_chart_style 共用);
        // C-ABI 不拒绝未知键,字段值非法则整体不生效并返回 false。
        if let Some(sc) = sheet.floating_stat_charts.iter_mut().find(|c| c.id == chart_id) {
            return roe_core::chart_style::apply_stat_chart_style(&mut sc.model, &val, false);
        }
        let chart = find_chart_mut(sheet, chart_id)
            .ok_or_else(|| format!("chart_id={chart_id} 不存在"))?;
        roe_core::chart_style::apply_chart_style(chart, &val, false)
    });
    result.is_some()
}

/// 删除图表。成功返回 true，失败（chart_id 不存在）返回 false。
#[no_mangle]
pub extern "C" fn roe_delete_chart(handle: u64, sheet_idx: u32, chart_id: u64) -> bool {
    let result = with_sheet_mut(handle, sheet_idx, "roe_delete_chart", |sheet| {
        let before = sheet.floating_charts.len() + sheet.floating_stat_charts.len();
        sheet.floating_charts.retain(|c| c.id != chart_id);
        sheet.floating_stat_charts.retain(|c| c.id != chart_id);
        if sheet.floating_charts.len() + sheet.floating_stat_charts.len() == before {
            return Err(format!("chart_id={chart_id} 不存在"));
        }
        Ok(())
    });
    result.is_some()
}

/// 在 Calc 指定 sheet 插入统计图(chartEx)。stat_json 为完整 StatChartModel JSON,
/// x/y/w/h 单位 pt(相对 sheet 内容区)。id 与普通图共用空间:两类合并 max+1。
/// 成功返回新图 id,失败返回 -1(详情见 roe_last_error_message)。
#[no_mangle]
pub extern "C" fn roe_calc_insert_stat_chart(
    handle: u64,
    sheet_idx: u32,
    stat_json: *const c_char,
    x_pt: f32,
    y_pt: f32,
    width_pt: f32,
    height_pt: f32,
) -> i64 {
    if stat_json.is_null() {
        set_last_error("roe_calc_insert_stat_chart: stat_json 为空指针");
        return -1;
    }
    let json = match unsafe { CStr::from_ptr(stat_json) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => {
            set_last_error("roe_calc_insert_stat_chart: stat_json 不是 UTF-8");
            return -1;
        }
    };
    let model = match serde_json::from_str::<roe_model::stat_chart::StatChartModel>(&json) {
        Ok(m) => m,
        Err(e) => {
            set_last_error(format!("roe_calc_insert_stat_chart: JSON 解析失败: {e}"));
            return -1;
        }
    };
    if !(width_pt > 0.0 && height_pt > 0.0) {
        set_last_error("roe_calc_insert_stat_chart: 宽高必须大于 0");
        return -1;
    }
    let r = with_sheet_mut(handle, sheet_idx, "roe_calc_insert_stat_chart", |sheet| {
        let new_id = roe_model::object_edit::alloc_object_id(sheet);
        sheet.floating_stat_charts.push(roe_model::calc::FloatingStatChart {
            id: new_id,
            x_pt,
            y_pt,
            width_pt,
            height_pt,
            model,
        });
        Ok(new_id)
    });
    match r {
        Some(id) => id as i64,
        None => -1,
    }
}

/// 图表类型名,与 roe_get_chart_config 的 kind 字符串保持一致。
fn chart_kind_name(k: &roe_model::impress::ChartKind) -> &'static str {
    use roe_model::impress::ChartKind as K;
    match k {
        K::Bar => "Bar",
        K::HorizontalBar => "HorizontalBar",
        K::Line => "Line",
        K::Pie => "Pie",
        K::Scatter => "Scatter",
        K::Radar => "Radar",
        K::Area => "Area",
        K::Doughnut => "Doughnut",
    }
}

/// 列出指定 sheet 的全部浮动图表(按 floating_charts 顺序,即底到顶的叠放序)。
/// 返回 JSON 数组 `[{"id":..,"kind":"Bar","x_pt":..,"y_pt":..,"width_pt":..,"height_pt":..}, ...]`,
/// 无图表返回 `[]`。失败(无效句柄/非 Calc 文档/sheet_idx 越界)返回 null,
/// 详情见 roe_last_error_message。返回的字符串由调用方 roe_free_string 释放。
#[no_mangle]
pub extern "C" fn roe_list_charts(handle: u64, sheet_idx: u32) -> *mut c_char {
    let json = with_sheet(handle, sheet_idx, "roe_list_charts", |sheet| {
        let items: Vec<String> = sheet
            .floating_charts
            .iter()
            .map(|c| {
                format!(
                    "{{\"id\":{},\"kind\":\"{}\",\"x_pt\":{:.2},\"y_pt\":{:.2},\"width_pt\":{:.2},\"height_pt\":{:.2}}}",
                    c.id, chart_kind_name(&c.kind), c.x_pt, c.y_pt, c.width_pt, c.height_pt
                )
            })
            .collect();
        // 统计图(chartEx)追加在普通图之后,带 "stat":true;普通图 JSON 格式保持不变
        let mut items = items;
        for c in &sheet.floating_stat_charts {
            items.push(format!(
                "{{\"id\":{},\"kind\":\"{:?}\",\"stat\":true,\"x_pt\":{:.2},\"y_pt\":{:.2},\"width_pt\":{:.2},\"height_pt\":{:.2}}}",
                c.id, c.model.kind, c.x_pt, c.y_pt, c.width_pt, c.height_pt
            ));
        }
        Ok(format!("[{}]", items.join(",")))
    });
    match json {
        Some(j) => json_to_cstring(j, "roe_list_charts"),
        None => std::ptr::null_mut(),
    }
}

/// 列出指定 sheet 的全部浮动对象(图表/统计图/图片/图形,共用 id 空间)。
/// 返回 JSON 数组,每项含 id/kind(chart|stat_chart|image|shape)/sub/x_pt,y_pt,width_pt,height_pt/rot/hidden,
/// 以及 title(图表)/text(图形)/mime(图片)。失败返回 null,详情见 roe_last_error_message。
#[no_mangle]
pub extern "C" fn roe_calc_list_objects(handle: u64, sheet_idx: u32) -> *mut c_char {
    let json = with_sheet(handle, sheet_idx, "roe_calc_list_objects", |sheet| {
        serde_json::to_string(&roe_model::object_list::list_objects(sheet)).map_err(|e| e.to_string())
    });
    match json {
        Some(j) => json_to_cstring(j, "roe_calc_list_objects"),
        None => std::ptr::null_mut(),
    }
}

/// 查询单个浮动对象,字段同 roe_calc_list_objects 的一项。id 不存在返回 null。
#[no_mangle]
pub extern "C" fn roe_calc_get_object(handle: u64, sheet_idx: u32, object_id: u64) -> *mut c_char {
    let json = with_sheet(handle, sheet_idx, "roe_calc_get_object", |sheet| {
        let o = roe_model::object_list::list_objects(sheet)
            .into_iter()
            .find(|o| o.id == object_id)
            .ok_or_else(|| format!("object_id={object_id} 不存在"))?;
        serde_json::to_string(&o).map_err(|e| e.to_string())
    });
    match json {
        Some(j) => json_to_cstring(j, "roe_calc_get_object"),
        None => std::ptr::null_mut(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use roe_model::calc::{SparseGrid, Workbook};
    use roe_model::style::StyleRegistry;
    use crate::handle::{self, DocumentEntry, WriterOptions};

    fn make_calc_handle_with_chart() -> (u64, u64) {
        let chart = ChartModel {
            id: 0,
            kind: roe_model::impress::ChartKind::Bar,
            title: None,
            x_labels: vec![],
            series: vec![],
            x_pt: 10.0,
            y_pt: 10.0,
            width_pt: 100.0,
            height_pt: 80.0,
            show_legend: true,
            show_grid: true,
            x_axis_label: String::new(),
            y_axis_label: String::new(),
            border_color: roe_model::common::Color { r: 204, g: 204, b: 204, a: 255 },
            border_width: 1.0,
            gap_width_pct: None,
            overlap_pct: None,
            grouping: roe_model::impress::ChartGrouping::Standard,
            hole_size_pct: None,
            data_labels: Default::default(),
            slice_border_color: None,
        };
        let chart_id = 1u64;
        let mut chart = chart;
        chart.id = chart_id;

        let wb = Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                name: "Data".into(),
                cells: SparseGrid::default(),
                col_widths: vec![],
                row_heights: vec![],
                frozen_rows: 0,
                frozen_cols: 0,
                cell_formats: HashMap::new(),
                cond_formats: vec![],
                data_validations: vec![],
                auto_filter: None,
                merged_cells: vec![],
                pivot: None,
                floating_charts: vec![chart],
                ..Default::default()
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };

        let h = handle::insert(DocumentEntry {
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
        });
        (h, chart_id)
    }

    /// 需求场景1：单次 set_chart_rect 提交后，不依赖任何后续 move 调用，
    /// 位置和尺寸都应立即准确。
    #[test]
    fn test_set_chart_rect_single_call_is_accurate() {
        let (h, chart_id) = make_calc_handle_with_chart();
        let ok = roe_set_chart_rect(h, 0, chart_id, 5.0, 6.0, 50.0, 40.0);
        assert!(ok, "set_chart_rect should succeed");

        let got = with_sheet(h, 0, "test", |sheet| {
            let c = find_chart(sheet, chart_id).ok_or("not found".to_string())?;
            Ok((c.x_pt, c.y_pt, c.width_pt, c.height_pt))
        }).unwrap();
        assert_eq!(got, (5.0, 6.0, 50.0, 40.0));
    }

    /// 需求场景2：模拟 nw 句柄连续 resize 3-4 次（每次都同时改变 x/y/w/h），
    /// 每次都用最终矩形直接 set_chart_rect，验证不产生累积偏移
    /// （即最后一次调用的结果就是准确值，不受之前调用顺序影响）。
    #[test]
    fn test_set_chart_rect_repeated_nw_no_accumulated_drift() {
        let (h, chart_id) = make_calc_handle_with_chart();

        // 初始矩形 (10,10,100,80)，右下角固定在 (110, 90)。
        // 模拟连续4次nw拖拽，每次都提交"完整矩形"而不是增量。
        let rects = [
            (20.0_f32, 15.0_f32, 90.0_f32, 75.0_f32),  // 右下角仍是 (110,90)
            (30.0, 25.0, 80.0, 65.0),                   // 右下角仍是 (110,90)
            (15.0, 40.0, 95.0, 50.0),                   // 右下角仍是 (110,90)
            (25.0, 30.0, 85.0, 60.0),                   // 右下角仍是 (110,90)
        ];

        for (x, y, w, hh) in rects.iter() {
            let ok = roe_set_chart_rect(h, 0, chart_id, *x, *y, *w, *hh);
            assert!(ok);
            // 每次提交后立刻验证右下角始终不变，证明矩形是原子替换而非累加。
            let got = with_sheet(h, 0, "test", |sheet| {
                let c = find_chart(sheet, chart_id).ok_or("not found".to_string())?;
                Ok((c.x_pt, c.y_pt, c.width_pt, c.height_pt))
            }).unwrap();
            assert_eq!(got, (*x, *y, *w, *hh));
            let (gx, gy, gw, gh) = got;
            assert!((gx + gw - 110.0).abs() < 1e-4, "右下角x应保持110, got {}", gx + gw);
            assert!((gy + gh - 90.0).abs() < 1e-4, "右下角y应保持90, got {}", gy + gh);
        }
    }

    /// 需求场景3：交替使用不同"方向"（se 只变w/h，nw 同时变x/y/w/h），
    /// 验证任意组合下最终值都精确等于最后一次提交的矩形，无残留旧值污染。
    #[test]
    fn test_set_chart_rect_alternating_directions() {
        let (h, chart_id) = make_calc_handle_with_chart();

        // se: 左上角不变，只变宽高
        assert!(roe_set_chart_rect(h, 0, chart_id, 10.0, 10.0, 120.0, 100.0));
        // nw: 右下角不变，左上角和宽高一起变
        assert!(roe_set_chart_rect(h, 0, chart_id, 40.0, 30.0, 90.0, 80.0));
        // sw: 右上角不变，x和宽度变，y/高度不变
        assert!(roe_set_chart_rect(h, 0, chart_id, 20.0, 30.0, 110.0, 80.0));
        // n: 底边不变，只有y和高度变
        assert!(roe_set_chart_rect(h, 0, chart_id, 20.0, 50.0, 110.0, 60.0));

        let got = with_sheet(h, 0, "test", |sheet| {
            let c = find_chart(sheet, chart_id).ok_or("not found".to_string())?;
            Ok((c.x_pt, c.y_pt, c.width_pt, c.height_pt))
        }).unwrap();
        assert_eq!(got, (20.0, 50.0, 110.0, 60.0));
    }

    /// chart_id 不存在时应返回 false，不应 panic。
    fn take_json(p: *mut c_char) -> String {
        assert!(!p.is_null(), "expected non-null json");
        unsafe { std::ffi::CString::from_raw(p) }.into_string().unwrap()
    }

    #[test]
    fn test_list_charts_returns_id_kind_rect() {
        let (h, chart_id) = make_calc_handle_with_chart();
        let s = take_json(roe_list_charts(h, 0));
        assert_eq!(
            s,
            format!("[{{\"id\":{chart_id},\"kind\":\"Bar\",\"x_pt\":10.00,\"y_pt\":10.00,\"width_pt\":100.00,\"height_pt\":80.00}}]")
        );
    }

    #[test]
    fn test_list_charts_empty_after_delete() {
        let (h, chart_id) = make_calc_handle_with_chart();
        assert!(roe_delete_chart(h, 0, chart_id));
        assert_eq!(take_json(roe_list_charts(h, 0)), "[]");
    }

    #[test]
    fn test_list_charts_invalid_sheet_or_handle_returns_null() {
        let (h, _) = make_calc_handle_with_chart();
        assert!(roe_list_charts(h, 5).is_null());
        assert!(roe_list_charts(u64::MAX, 0).is_null());
    }

    fn stat_fixture_for_api(id: u64) -> roe_model::calc::FloatingStatChart {
        use roe_model::stat_chart::*;
        roe_model::calc::FloatingStatChart {
            id, x_pt: 200.0, y_pt: 20.0, width_pt: 120.0, height_pt: 90.0,
            model: StatChartModel {
                kind: StatChartKind::Funnel, title_present: false, title: None,
                show_legend: false, legend_pos: None, show_grid: false, gap_width: None,
                data: vec![], series: vec![],
            },
        }
    }

    #[test]
    fn test_stat_chart_list_move_resize_rect_delete() {
        let (h, chart_id) = make_calc_handle_with_chart();
        let stat_id = chart_id + 1;
        with_sheet_mut(h, 0, "test", |sheet| {
            sheet.floating_stat_charts.push(stat_fixture_for_api(stat_id));
            Ok(())
        }).unwrap();

        let list = take_json(roe_list_charts(h, 0));
        assert!(list.contains(&format!("\"id\":{stat_id},\"kind\":\"Funnel\",\"stat\":true")), "{list}");
        assert!(list.starts_with(&format!("[{{\"id\":{chart_id},\"kind\":\"Bar\",\"x_pt\"")), "普通图格式不变: {list}");

        assert!(roe_move_chart(h, 0, stat_id, 11.0, 12.0));
        assert!(roe_resize_chart(h, 0, stat_id, 33.0, 44.0));
        assert_eq!(
            take_json(roe_get_chart_rect(h, 0, stat_id)),
            "{\"x_pt\":11.00,\"y_pt\":12.00,\"width_pt\":33.00,\"height_pt\":44.00}"
        );
        assert!(roe_set_chart_rect(h, 0, stat_id, 5.0, 6.0, 70.0, 80.0));
        assert_eq!(
            take_json(roe_get_chart_rect(h, 0, stat_id)),
            "{\"x_pt\":5.00,\"y_pt\":6.00,\"width_pt\":70.00,\"height_pt\":80.00}"
        );
        // 普通图不受影响
        assert_eq!(
            take_json(roe_get_chart_rect(h, 0, chart_id)),
            "{\"x_pt\":10.00,\"y_pt\":10.00,\"width_pt\":100.00,\"height_pt\":80.00}"
        );
        // 统计图只暴露 title/show_legend/show_grid 三项,并带 stat 标志
        let cfg = take_json(roe_get_chart_config(h, 0, stat_id));
        assert!(cfg.contains("\"stat\":true"), "{cfg}");

        assert!(roe_delete_chart(h, 0, stat_id));
        let list = take_json(roe_list_charts(h, 0));
        assert!(!list.contains("\"stat\":true"), "{list}");
        assert!(list.contains(&format!("\"id\":{chart_id}")));
        assert!(!roe_delete_chart(h, 0, stat_id));
    }

    #[test]
    fn test_calc_insert_stat_chart() {
        let (h, chart_id) = make_calc_handle_with_chart();
        let json = serde_json::to_string(&stat_fixture_for_api(0).model).unwrap();
        let cj = std::ffi::CString::new(json).unwrap();

        let id1 = roe_calc_insert_stat_chart(h, 0, cj.as_ptr(), 20.0, 30.0, 200.0, 150.0);
        assert_eq!(id1 as u64, chart_id + 1);
        let id2 = roe_calc_insert_stat_chart(h, 0, cj.as_ptr(), 1.0, 2.0, 3.0, 4.0);
        assert_eq!(id2, id1 + 1);

        let list = take_json(roe_list_charts(h, 0));
        assert!(list.contains(&format!("\"id\":{id1},\"kind\":\"Funnel\",\"stat\":true")), "{list}");
        assert_eq!(
            take_json(roe_get_chart_rect(h, 0, id1 as u64)),
            "{\"x_pt\":20.00,\"y_pt\":30.00,\"width_pt\":200.00,\"height_pt\":150.00}"
        );

        // 非法输入
        let bad = std::ffi::CString::new("{not json").unwrap();
        assert_eq!(roe_calc_insert_stat_chart(h, 0, bad.as_ptr(), 0.0, 0.0, 10.0, 10.0), -1);
        assert_eq!(roe_calc_insert_stat_chart(h, 0, cj.as_ptr(), 0.0, 0.0, 0.0, 10.0), -1);
        assert_eq!(roe_calc_insert_stat_chart(h, 0, std::ptr::null(), 0.0, 0.0, 10.0, 10.0), -1);
        assert_eq!(roe_calc_insert_stat_chart(h, 9, cj.as_ptr(), 0.0, 0.0, 10.0, 10.0), -1);
    }

    #[test]
    fn test_set_chart_rect_invalid_chart_id_returns_false() {
        let (h, _chart_id) = make_calc_handle_with_chart();
        let ok = roe_set_chart_rect(h, 0, 9999, 1.0, 2.0, 3.0, 4.0);
        assert!(!ok);
    }
}


#[cfg(test)]
mod chart_config_parity_tests {
    use super::roe_get_chart_config;
    use std::ffi::CStr;

    fn take(p: *mut std::os::raw::c_char) -> String {
        assert!(!p.is_null(), "roe_get_chart_config 返回 null");
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
        crate::script::roe_free_string(p);
        s
    }

    /// C-ABI roe_get_chart_config 与 Rhai get_chart_config 的共有字段必须一致;
    /// 标题/轴标签含 DEL 字符时 C-ABI 输出仍须是合法 JSON(旧版 {:?} 会输出 \u{7f})。
    #[test]
    fn cabi_and_rhai_get_chart_config_agree() {
        let h = crate::document::new_document_impl(2);
        assert!(h != 0);
        let cj = r#"{"kind":"Bar","x_labels":["A"],"series":[{"name":"s","data":[1]}],"width_pt":100,"height_pt":60}"#;
        let run = |script: String| match crate::script::execute_script_impl(h, &script, None) {
            Some(Ok((_, _, ret))) => ret,
            other => panic!("unexpected: {:?}", other.map(|r| r.map(|(_, _, v)| v))),
        };
        let id = run(format!("insert_chart(0, 0, 0, `{cj}`)")).as_i64().unwrap();
        run(format!(
            r#"set_chart_style(0, {id}, `{{"title":"x\u007fy","x_axis_label":"t\u007f","show_legend":false}}`)"#));

        let c_raw = take(roe_get_chart_config(h, 0, id as u64));
        let c: serde_json::Value = serde_json::from_str(&c_raw)
            .unwrap_or_else(|e| panic!("C-ABI 输出不是合法 JSON: {e}\n{c_raw}"));
        let r_ret = run(format!("get_chart_config(0, {id})"));
        let r: serde_json::Value = serde_json::from_str(r_ret.as_str().unwrap()).unwrap();

        assert_eq!(c["title"], "x\u{7f}y", "{c_raw}");
        let mut keys: Vec<&str> = roe_core::chart_style::STYLE_KEYS.to_vec();
        keys.extend(["id", "x_pt", "y_pt", "width_pt", "height_pt"]);
        for k in keys {
            assert_eq!(c[k], r[k], "字段 {k} 两边不一致: C-ABI={} Rhai={}", c[k], r[k]);
        }
    }
}
