//! 图表 C-ABI 接口
//!
//! - roe_render_chart_to_png：根据 JSON 数据直接渲染图表，返回 PNG bytes

use std::ffi::CStr;
use std::os::raw::{c_char, c_uchar};
use crate::error::set_last_error;
use roe_model::impress::{ChartModel, ChartKind, ChartSeries};

/// 根据 chart_json 渲染图表为 PNG，返回指向 PNG bytes 的指针，长度写入 out_len。
/// 调用方负责用 roe_free_bytes 释放。
///
/// chart_json 格式：
/// {
///   "kind": "Bar",          // Bar / HorizontalBar / Line / Pie / Scatter / Radar
///   "title": "Sales",       // 可选
///   "width": 600,
///   "height": 400,
///   "x_labels": ["Mon","Tue","Wed"],
///   "series": [
///     {"name":"A","data":[120.0,200.0,150.0]},
///     {"name":"B","data":[80.0,160.0,90.0]}
///   ]
/// }
///
/// 失败返回 null，out_len 设为 0，详情见 roe_last_error_message。
#[no_mangle]
pub extern "C" fn roe_render_chart_to_png(
    chart_json: *const c_char,
    out_len: *mut u32,
) -> *mut c_uchar {
    unsafe { *out_len = 0; }

    let json_str = match unsafe { CStr::from_ptr(chart_json) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!("roe_render_chart_to_png: utf8 error: {e}"));
            return std::ptr::null_mut();
        }
    };

    #[derive(serde::Deserialize)]
    struct ChartRequest {
        kind: String,
        title: Option<String>,
        width: u32,
        height: u32,
        x_labels: Vec<String>,
        series: Vec<ChartSeries>,
        #[serde(default = "default_true")]
        show_legend: bool,
        #[serde(default = "default_true")]
        show_grid: bool,
        #[serde(default)]
        x_axis_label: String,
        #[serde(default)]
        y_axis_label: String,
        #[serde(default)]
        border_color: Option<String>,
        #[serde(default = "default_border_width")]
        border_width: f32,
        #[serde(default)]
        grouping: Option<String>,
        #[serde(default)]
        show_val: bool,
        #[serde(default)]
        hole_size_pct: Option<f32>,
        #[serde(default)]
        gap_width_pct: Option<f32>,
        #[serde(default)]
        overlap_pct: Option<f32>,
    }
    fn default_true() -> bool { true }
    fn default_border_width() -> f32 { 1.0 }

    fn parse_hex_color(s: &str) -> Option<roe_model::common::Color> {
        let s = s.trim_start_matches('#');
        if s.len() != 6 { return None; }
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some(roe_model::common::Color { r, g, b, a: 255 })
    }

    let req: ChartRequest = match serde_json::from_str(json_str) {
        Ok(r) => r,
        Err(e) => {
            set_last_error(format!("roe_render_chart_to_png: json parse error: {e}"));
            return std::ptr::null_mut();
        }
    };

    let kind = match req.kind.as_str() {
        "Bar"           => ChartKind::Bar,
        "HorizontalBar" => ChartKind::HorizontalBar,
        "Line"          => ChartKind::Line,
        "Pie"           => ChartKind::Pie,
        "Scatter"       => ChartKind::Scatter,
        "Radar"         => ChartKind::Radar,
        "Area"          => ChartKind::Area,
        "Doughnut"      => ChartKind::Doughnut,
        other => {
            set_last_error(format!("roe_render_chart_to_png: unknown kind: {other}"));
            return std::ptr::null_mut();
        }
    };

    let border_color = req.border_color
        .as_deref()
        .and_then(parse_hex_color)
        .unwrap_or(roe_model::common::Color { r: 0xcc, g: 0xcc, b: 0xcc, a: 255 });

    let model = ChartModel {
        id: 0,
        kind,
        title: req.title,
        x_labels: req.x_labels,
        series: req.series,
        x_pt: 0.0,
        y_pt: 0.0,
        width_pt: req.width as f32,
        height_pt: req.height as f32,
        show_legend: req.show_legend,
        show_grid: req.show_grid,
        x_axis_label: req.x_axis_label,
        y_axis_label: req.y_axis_label,
        border_color,
        border_width: req.border_width,
        gap_width_pct: req.gap_width_pct,
        overlap_pct: req.overlap_pct,
        grouping: match req.grouping.as_deref() {
            Some("Stacked") => roe_model::impress::ChartGrouping::Stacked,
            Some("PercentStacked") => roe_model::impress::ChartGrouping::PercentStacked,
            _ => roe_model::impress::ChartGrouping::Standard,
        },
        hole_size_pct: req.hole_size_pct,
        data_labels: {
            let mut d = roe_model::impress::DataLabels::default();
            if req.show_val { d.show_val = true; }
            d
        },
        slice_border_color: None,
    };

    let png = roe_impress::chart::render_chart_to_png(&model, req.width, req.height);
    let png = match png {
        Some(b) => b,
        None => {
            set_last_error("roe_render_chart_to_png: render failed");
            return std::ptr::null_mut();
        }
    };

    let len = png.len() as u32;
    let mut boxed = png.into_boxed_slice();
    let ptr = boxed.as_mut_ptr();
    std::mem::forget(boxed);
    unsafe { *out_len = len; }
    ptr
}

/// 释放 roe_render_chart_to_png 返回的 PNG bytes
#[no_mangle]
pub extern "C" fn roe_free_bytes(ptr: *mut c_uchar, len: u32) {
    if ptr.is_null() { return; }
    unsafe {
        let _ = Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len as usize));
    }
}

/// v223: 统计图（StatChartModel JSON）渲染为 RGBA 像素，长度 w*h*4，写入 out_len。
/// 调用方用 roe_free_bytes 释放。失败返回 null。内部 catch_unwind，不会让 FFI 调用方 abort。
#[no_mangle]
pub extern "C" fn roe_render_stat_chart_to_rgba(
    json: *const c_char,
    width: u32,
    height: u32,
    out_len: *mut u32,
) -> *mut c_uchar {
    if out_len.is_null() { return std::ptr::null_mut(); }
    unsafe { *out_len = 0; }
    if json.is_null() {
        set_last_error("roe_render_stat_chart_to_rgba: null json");
        return std::ptr::null_mut();
    }
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Option<Vec<u8>> {
        let s = unsafe { CStr::from_ptr(json) }.to_str().ok()?;
        let model: roe_model::stat_chart::StatChartModel = serde_json::from_str(s).ok()?;
        roe_core::stat_chart_render::render_stat_chart_to_rgba(&model, width, height, 1.0)
    }));
    match r {
        Ok(Some(buf)) => {
            let len = buf.len() as u32;
            let mut boxed = buf.into_boxed_slice();
            let ptr = boxed.as_mut_ptr();
            std::mem::forget(boxed);
            unsafe { *out_len = len; }
            ptr
        }
        _ => {
            set_last_error("roe_render_stat_chart_to_rgba: render failed");
            std::ptr::null_mut()
        }
    }
}
