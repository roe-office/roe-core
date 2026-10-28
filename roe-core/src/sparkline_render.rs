//! Sparkline（迷你图）渲染：Line / Column 两种类型，第一版范围。
//! 参照 cond_format_render.rs 的既有模式：独立文件，被 tile.rs 在
//! "数据条/色阶渲染"之后、"单元格文字渲染"之前调用。
//!
//! 第一版范围（见 ROE_Sparkline_Design_v1.md 第5节）：
//! - 仅 Line / Column 两种类型（Stacked 留待后续）
//! - markers/high/low/first/last/negative 高亮点：仅用简单圆点标记
//! - display_empty_cells_as：仅正确处理 Gap（跳过该点），Span/Zero 按 0 处理
//! - date_axis / min_axis_type=Custom / right_to_left：忽略，记录技术债

use roe_model::calc::{CellValue, Sheet, SparklineGroup, SparklineType, DisplayEmptyCellsAs};
use crate::tile_geometry::{CalcRenderConfig, col_x_px, row_y_px, col_width_at_px, row_height_at_px, PT_TO_PX};
use tiny_skia::{PixmapMut, Paint, PathBuilder, Rect, Transform, Stroke, LineCap};

fn parse_hex_color(s: &str, fallback: [u8; 4]) -> [u8; 4] {
    let s = s.trim_start_matches('#');
    if s.len() != 6 {
        return fallback;
    }
    let r = u8::from_str_radix(&s[0..2], 16).unwrap_or(fallback[0]);
    let g = u8::from_str_radix(&s[2..4], 16).unwrap_or(fallback[1]);
    let b = u8::from_str_radix(&s[4..6], 16).unwrap_or(fallback[2]);
    [r, g, b, 0xff]
}

/// 从单个 sparkline 的 input_range 里取出数值序列。
/// 支持多段 range（通常只有一段），按行优先展开成一维序列。
/// 空单元格按 DisplayEmptyCellsAs 处理：Gap → None（跳过），
/// Span/Zero → Some(0.0)（第一版简化，未做真正的跨空值连线）。
fn collect_values(sheet: &Sheet, group: &SparklineGroup, spark_idx: usize) -> Vec<Option<f64>> {
    let sparkline = &group.sparklines[spark_idx];
    let mut values = Vec::new();
    for range in &sparkline.input_range {
        let (r1, c1) = range.start;
        let (r2, c2) = range.end;
        for row in r1..=r2 {
            for col in c1..=c2 {
                let cell = sheet.cells.get(row, col);
                let v = match cell {
                    CellValue::Number(n) => Some(n),
                    CellValue::Formula { cached, .. } => match *cached {
                        CellValue::Number(n) => Some(n),
                        _ => None,
                    },
                    CellValue::Empty => match group.display_empty_cells_as {
                        DisplayEmptyCellsAs::Gap => None,
                        DisplayEmptyCellsAs::Span | DisplayEmptyCellsAs::Zero => Some(0.0),
                    },
                    _ => None,
                };
                values.push(v);
            }
        }
    }
    values
}

/// 渲染 sheet.sparkline_groups 中所有落在本 tile 可见范围内的 sparkline。
/// 调用时机：与 render_databar_and_colorscale 相同（网格线之上、文字之下）。
#[allow(clippy::too_many_arguments)]
pub fn render_sparklines(
    pixmap: &mut PixmapMut,
    sheet: &Sheet,
    cfg: &CalcRenderConfig,
    tx: Transform,
    row_start: u32,
    row_end: u32,
    col_start: u32,
    col_end: u32,
) {
    for group in &sheet.sparkline_groups {
        let series_color = parse_hex_color(
            group.color_series.as_deref().unwrap_or(""),
            [0x37, 0x6f, 0xb8, 0xff],
        );
        let negative_color = group.color_negative.as_deref()
            .map(|s| parse_hex_color(s, series_color));
        let marker_color = group.color_markers.as_deref()
            .map(|s| parse_hex_color(s, series_color));
        let high_color = group.color_high.as_deref().map(|s| parse_hex_color(s, series_color));
        let low_color = group.color_low.as_deref().map(|s| parse_hex_color(s, series_color));
        let first_color = group.color_first.as_deref().map(|s| parse_hex_color(s, series_color));
        let last_color = group.color_last.as_deref().map(|s| parse_hex_color(s, series_color));

        for (spark_idx, sparkline) in group.sparklines.iter().enumerate() {
            let (row, col) = sparkline.cell;
            // 只处理与本 tile 有交叉的单元格
            if row < row_start || row >= row_end { continue; }
            if col < col_start || col >= col_end { continue; }

            let values = collect_values(sheet, group, spark_idx);
            if values.is_empty() { continue; }

            let x = col_x_px(sheet, col, cfg) + 2.0;
            let y = row_y_px(sheet, row, cfg) + 2.0;
            let w = (col_width_at_px(sheet, col, cfg) - 4.0).max(0.0);
            let h = (row_height_at_px(sheet, row, cfg) - 4.0).max(0.0);
            if w <= 0.0 || h <= 0.0 { continue; }

            let present: Vec<(usize, f64)> = values.iter().enumerate()
                .filter_map(|(i, v)| v.map(|val| (i, val)))
                .collect();
            if present.is_empty() { continue; }

            let (data_min, data_max) = present.iter().fold(
                (f64::INFINITY, f64::NEG_INFINITY),
                |(mn, mx), (_, v)| (mn.min(*v), mx.max(*v)),
            );
            let min = group.manual_min.unwrap_or(data_min);
            let max = group.manual_max.unwrap_or(data_max);
            let range = (max - min).max(1e-9);

            let n = values.len();
            let step = if n > 1 { w / (n - 1) as f32 } else { 0.0 };

            match group.sparkline_type {
                SparklineType::Line => {
                    // 0 轴基准线（display_x_axis 打开时画，且 0 必须落在 min..max 区间内才有意义）
                    if group.display_x_axis && min < 0.0 && max > 0.0 {
                        // 0轴线颜色：不采用文件自带 color_axis 值（Excel 默认写入纯黑，
                        // 但 LOK/OnlyOffice 实际渲染均改用接近网格线的浅灰色，本实现对齐
                        // 这一显示策略，而非还原 Excel 原始设置）。
                        let axis_color = [0xc8, 0xc8, 0xc8, 0xff];
                        let mut axis_paint = Paint::default();
                        axis_paint.set_color_rgba8(axis_color[0], axis_color[1], axis_color[2], axis_color[3]);
                        let zero_t = ((0.0 - min) / range) as f32;
                        let zero_y = y + h - zero_t * h;
                        let mut axis_pb = PathBuilder::new();
                        axis_pb.move_to(x, zero_y);
                        axis_pb.line_to(x + w, zero_y);
                        if let Some(axis_path) = axis_pb.finish() {
                            let axis_stroke = Stroke { width: 0.75 * PT_TO_PX, ..Default::default() };
                            pixmap.stroke_path(&axis_path, &axis_paint, &axis_stroke, tx, None);
                        }
                    }

                    let mut paint = Paint::default();
                    paint.set_color_rgba8(series_color[0], series_color[1], series_color[2], series_color[3]);
                    paint.anti_alias = true;

                    let mut pb = PathBuilder::new();
                    let mut started = false;
                    for (i, v) in values.iter().enumerate() {
                        let Some(v) = v else {
                            started = false; // Gap：断线
                            continue;
                        };
                        let px = x + step * i as f32;
                        let t = ((*v - min) / range) as f32;
                        let py = y + h - t * h;
                        if !started {
                            pb.move_to(px, py);
                            started = true;
                        } else {
                            pb.line_to(px, py);
                        }
                    }
                    if let Some(path) = pb.finish() {
                        let stroke = Stroke {
                            width: (group.line_weight * PT_TO_PX).max(0.5),
                            line_cap: LineCap::Round,
                            ..Default::default()
                        };
                        pixmap.stroke_path(&path, &paint, &stroke, tx, None);
                    }

                    if group.markers {
                        for (i, v) in values.iter().enumerate() {
                            let Some(v) = v else { continue };
                            let px = x + step * i as f32;
                            let t = ((*v - min) / range) as f32;
                            let py = y + h - t * h;
                            let c = marker_color.unwrap_or(series_color);
                            draw_marker(pixmap, px, py, c, tx);
                        }
                    }
                }
                SparklineType::Stacked => {
                    // win-loss 图：所有柱子等高，只按正负分两色，不按数值比例缩放高度。
                    // 0 值不画柱（既非胜也非负）。
                    let bar_w = (w / n as f32 * 0.7).max(1.0);
                    let gap = if n > 1 { w / n as f32 } else { w };
                    let half_h = h / 2.0;
                    for (i, v) in values.iter().enumerate() {
                        let Some(v) = v else { continue };
                        if *v == 0.0 { continue; }
                        let px = x + gap * i as f32 + (gap - bar_w) / 2.0;
                        let (top, height) = if *v > 0.0 {
                            (y, half_h)
                        } else {
                            (y + half_h, half_h)
                        };
                        let color = if *v < 0.0 {
                            negative_color.unwrap_or(series_color)
                        } else {
                            series_color
                        };
                        let mut paint = Paint::default();
                        paint.set_color_rgba8(color[0], color[1], color[2], color[3]);
                        if let Some(rect) = Rect::from_xywh(px, top, bar_w, height.max(1.0)) {
                            let path = PathBuilder::from_rect(rect);
                            pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, tx, None);
                        }
                    }
                }
                SparklineType::Column => {
                    let bar_w = (w / n as f32 * 0.7).max(1.0);
                    let gap = if n > 1 { w / n as f32 } else { w };
                    let zero_y = if min < 0.0 && max > 0.0 {
                        y + h - ((0.0 - min) / range) as f32 * h
                    } else if min >= 0.0 {
                        y + h
                    } else {
                        y
                    };

                    // high/low/first/last 索引（用于逐柱高亮判定，优先级：
                    // high > low > first > last > negative > 普通色，
                    // 与 Excel 实际显示顺序一致：极值优先于位置标记）
                    let present: Vec<(usize, f64)> = values.iter().enumerate()
                        .filter_map(|(i, v)| v.map(|val| (i, val)))
                        .collect();
                    let high_idx = if group.high {
                        present.iter().max_by(|a, b| a.1.partial_cmp(&b.1).unwrap()).map(|&(i, _)| i)
                    } else { None };
                    let low_idx = if group.low {
                        present.iter().min_by(|a, b| a.1.partial_cmp(&b.1).unwrap()).map(|&(i, _)| i)
                    } else { None };
                    let first_idx = if group.first { present.first().map(|&(i, _)| i) } else { None };
                    let last_idx = if group.last { present.last().map(|&(i, _)| i) } else { None };

                    for (i, v) in values.iter().enumerate() {
                        let Some(v) = v else { continue };
                        let px = x + gap * i as f32 + (gap - bar_w) / 2.0;
                        let t = ((*v - min) / range) as f32;
                        let bar_top_y = y + h - t * h;
                        let (top, height) = if *v >= 0.0 {
                            (bar_top_y.min(zero_y), (zero_y - bar_top_y).abs())
                        } else {
                            (zero_y.min(bar_top_y), (bar_top_y - zero_y).abs())
                        };
                        let color = if high_idx == Some(i) {
                            high_color.unwrap_or(series_color)
                        } else if low_idx == Some(i) {
                            low_color.unwrap_or(series_color)
                        } else if first_idx == Some(i) {
                            first_color.unwrap_or(series_color)
                        } else if last_idx == Some(i) {
                            last_color.unwrap_or(series_color)
                        } else if *v < 0.0 {
                            negative_color.unwrap_or(series_color)
                        } else {
                            series_color
                        };
                        let mut paint = Paint::default();
                        paint.set_color_rgba8(color[0], color[1], color[2], color[3]);
                        if let Some(rect) = Rect::from_xywh(px, top, bar_w, height.max(1.0)) {
                            let path = PathBuilder::from_rect(rect);
                            pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, tx, None);
                        }
                    }
                }
            }

            // high/low/first/last 高亮点（仅 Line 类型有意义；Column 第一版跳过）
            if matches!(group.sparkline_type, SparklineType::Line)
                && (group.high || group.low || group.first || group.last) {
                    highlight_points(pixmap, &values, x, y, w, h, min, range, step, group,
                        high_color, low_color, first_color, last_color, tx);
                }
        }
    }
}

fn draw_marker(pixmap: &mut PixmapMut, cx: f32, cy: f32, color: [u8; 4], tx: Transform) {
    let mut paint = Paint::default();
    paint.set_color_rgba8(color[0], color[1], color[2], color[3]);
    paint.anti_alias = true;
    let r = 1.5;
    if let Some(rect) = Rect::from_xywh(cx - r, cy - r, r * 2.0, r * 2.0) {
        let path = PathBuilder::from_rect(rect);
        pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, tx, None);
    }
}

#[allow(clippy::too_many_arguments)]
fn highlight_points(
    pixmap: &mut PixmapMut,
    values: &[Option<f64>],
    x: f32, y: f32, _w: f32, h: f32,
    min: f64, range: f64, step: f32,
    group: &SparklineGroup,
    high_color: Option<[u8; 4]>,
    low_color: Option<[u8; 4]>,
    first_color: Option<[u8; 4]>,
    last_color: Option<[u8; 4]>,
    tx: Transform,
) {
    let present: Vec<(usize, f64)> = values.iter().enumerate()
        .filter_map(|(i, v)| v.map(|val| (i, val)))
        .collect();
    if present.is_empty() { return; }

    let point_xy = |i: usize, v: f64| -> (f32, f32) {
        let px = x + step * i as f32;
        let t = ((v - min) / range) as f32;
        let py = y + h - t * h;
        (px, py)
    };

    if group.high {
        if let Some(&(i, v)) = present.iter().max_by(|a, b| a.1.partial_cmp(&b.1).unwrap()) {
            let (px, py) = point_xy(i, v);
            draw_marker(pixmap, px, py, high_color.unwrap_or([0xd0, 0x00, 0x00, 0xff]), tx);
        }
    }
    if group.low {
        if let Some(&(i, v)) = present.iter().min_by(|a, b| a.1.partial_cmp(&b.1).unwrap()) {
            let (px, py) = point_xy(i, v);
            draw_marker(pixmap, px, py, low_color.unwrap_or([0x00, 0x00, 0xd0, 0xff]), tx);
        }
    }
    if group.first {
        let &(i, v) = present.first().unwrap();
        let (px, py) = point_xy(i, v);
        draw_marker(pixmap, px, py, first_color.unwrap_or([0x00, 0x99, 0x00, 0xff]), tx);
    }
    if group.last {
        let &(i, v) = present.last().unwrap();
        let (px, py) = point_xy(i, v);
        draw_marker(pixmap, px, py, last_color.unwrap_or([0x99, 0x00, 0x99, 0xff]), tx);
    }
}
