//! 统计图表(chartEx)渲染:StatChartModel + 宽高 → SVG → RGBA。
//! 目前实现直方图;其余类型返回 None,调用方回退到占位框。
//! 不走 charts-rs,直接拼 SVG,以便精确控制柱子紧贴、区间标签、白色细边。

use crate::chart_render::{nice_axis, svg_to_pixmap, OFFICE_PALETTE};
use roe_model::impress::DEFAULT_CHART_TITLE;
use roe_model::stat_chart::{StatChartKind, StatChartModel, StatData};
use roe_model::stats;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 整数不带小数点,其余最多两位小数并去掉末尾 0。
fn fmt_num(v: f64) -> String {
    if (v - v.round()).abs() < 1e-9 {
        format!("{}", v.round() as i64)
    } else {
        let s = format!("{:.2}", v);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// 区间标签。右闭:第一组 `[lo, hi]`,其余 `(lo, hi]`;左闭:最后一组 `[lo, hi]`,其余 `[lo, hi)`。
fn bin_labels(edges: &[f64], closed_right: bool) -> Vec<String> {
    let k = edges.len().saturating_sub(1);
    (0..k)
        .map(|i| {
            let lo = fmt_num(edges[i]);
            let hi = fmt_num(edges[i + 1]);
            if closed_right {
                if i == 0 { format!("[{}, {}]", lo, hi) } else { format!("({}, {}]", lo, hi) }
            } else if i == k - 1 {
                format!("[{}, {}]", lo, hi)
            } else {
                format!("[{}, {})", lo, hi)
            }
        })
        .collect()
}

fn histogram_svg(model: &StatChartModel, w: u32, h: u32, scale: f32) -> Option<String> {
    let s = model.series.first()?;
    let d = model
        .data
        .iter()
        .find(|d| d.id == s.data_id)
        .or_else(|| model.data.first())?;
    let vals: Vec<f64> = d.values.iter().filter_map(|v| *v).collect();
    let hist = stats::histogram(&vals, s.bin_width, s.bin_count, s.interval_closed_right)?;
    let k = hist.counts.len();
    if k == 0 {
        return None;
    }
    let max_count = *hist.counts.iter().max()? as f64;
    let (axis_max, n) = nice_axis(max_count)?;

    // 布局按"点"为单位,再由 viewBox 整体放大 scale 倍,字号与边距随页面缩放
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let wf = w as f32 / scale;
    let hf = h as f32 / scale;
    let title: Option<String> = if model.title_present {
        Some(match model.title.as_deref() {
            Some(t) if !t.trim().is_empty() => t.to_string(),
            _ => DEFAULT_CHART_TITLE.to_string(),
        })
    } else {
        None
    };
    let left = 44.0_f32;
    let right = 16.0_f32;
    let top = if title.is_some() { 44.0_f32 } else { 16.0_f32 };
    let bottom = 36.0_f32;
    let plot_w = wf - left - right;
    let plot_h = hf - top - bottom;
    if plot_w <= 0.0 || plot_h <= 0.0 {
        return None;
    }
    let y_of = |v: f32| top + plot_h * (1.0 - v / axis_max);
    let (r, g, b) = OFFICE_PALETTE[0];
    let fill = format!("#{:02X}{:02X}{:02X}", r, g, b);

    let mut svg = String::new();
    svg.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {wf:.2} {hf:.2}\">"
    ));
    svg.push_str(&format!("<rect width=\"{wf:.2}\" height=\"{hf:.2}\" fill=\"#FFFFFF\"/>"));
    // 图表外框:宽度按 1 个设备像素(1/scale 点)画,向内缩半个线宽避免被裁
    {
        let sw = 1.0 / scale;
        svg.push_str(&format!(
            "<rect x=\"{:.3}\" y=\"{:.3}\" width=\"{:.3}\" height=\"{:.3}\" fill=\"none\" stroke=\"#D9D9D9\" stroke-width=\"{:.3}\"/>",
            sw / 2.0, sw / 2.0, wf - sw, hf - sw, sw
        ));
    }

    if let Some(t) = &title {
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"27\" font-family=\"serif\" font-size=\"16\" fill=\"#595959\" text-anchor=\"middle\">{}</text>",
            wf / 2.0,
            esc(t)
        ));
    }

    // 网格线 + Y 轴刻度
    for i in 0..=n {
        let val = axis_max * i as f32 / n as f32;
        let y = y_of(val);
        if model.show_grid || i == 0 {
            let color = if i == 0 { "#BFBFBF" } else { "#D9D9D9" };
            svg.push_str(&format!(
                "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"{}\" stroke-width=\"1\"/>",
                left, y, left + plot_w, y, color
            ));
        }
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"serif\" font-size=\"11\" fill=\"#595959\" text-anchor=\"end\">{}</text>",
            left - 6.0,
            y + 4.0,
            fmt_num(val as f64)
        ));
    }

    // 柱子:gapWidth 是间距相对柱宽的比例(0 = 紧贴)
    let slot = plot_w / k as f32;
    let gap = model.gap_width.unwrap_or(0.0).max(0.0) as f32;
    let bw = slot / (1.0 + gap);
    let labels = bin_labels(&hist.edges, s.interval_closed_right);
    for (i, &c) in hist.counts.iter().enumerate() {
        let x = left + i as f32 * slot + (slot - bw) / 2.0;
        let bh = plot_h * c as f32 / axis_max;
        if bh > 0.0 {
            svg.push_str(&format!(
                "<rect class=\"bar\" x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\" fill=\"{}\" stroke=\"#FFFFFF\" stroke-width=\"1\"/>",
                x,
                y_of(c as f32),
                bw,
                bh,
                fill
            ));
        }
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"serif\" font-size=\"11\" fill=\"#595959\" text-anchor=\"middle\">{}</text>",
            left + i as f32 * slot + slot / 2.0,
            top + plot_h + 18.0,
            esc(&labels[i])
        ));
    }
    svg.push_str("</svg>");
    Some(svg)
}

/// 深一档的描边色(各通道 ×0.8)。
fn darker(c: (u8, u8, u8)) -> String {
    format!(
        "#{:02X}{:02X}{:02X}",
        (c.0 as f32 * 0.8) as u8,
        (c.1 as f32 * 0.8) as u8,
        (c.2 as f32 * 0.8) as u8
    )
}

/// 含负数的坐标轴取整:返回 (下限, 上限, 刻度间隔数)。步长取 1/2/5×10^e,间隔数不超过 8,范围必含 0。
fn nice_range(lo: f64, hi: f64) -> (f64, f64, usize) {
    nice_range_n(lo, hi, 8)
}

fn nice_range_n(lo: f64, hi: f64, max_n: usize) -> (f64, f64, usize) {
    let lo = lo.min(0.0);
    let hi = hi.max(0.0);
    let span = if hi - lo > 0.0 { hi - lo } else { 1.0 };
    let base = span.log10().floor() as i32 - 2;
    for e in base..(base + 6) {
        for m in [1.0_f64, 2.0, 5.0] {
            let step = m * 10f64.powi(e);
            let a = (lo / step).floor() * step;
            let b = (hi / step).ceil() * step;
            let n = ((b - a) / step).round() as usize;
            if (1..=max_n).contains(&n) {
                return (a, b, n);
            }
        }
    }
    (lo, hi.max(lo + 1.0), 1)
}

fn box_svg(model: &StatChartModel, w: u32, h: u32, scale: f32) -> Option<String> {
    if model.series.is_empty() {
        return None;
    }
    let first = model
        .data
        .iter()
        .find(|d| d.id == model.series[0].data_id)
        .or_else(|| model.data.first())?;
    let cat_of = |d: &StatData, i: usize| -> String {
        d.categories.first().and_then(|c| c.get(i)).cloned().unwrap_or_default()
    };
    // 分类顺序:第一个系列里首次出现的顺序
    let mut cats: Vec<String> = Vec::new();
    for i in 0..first.values.len() {
        let c = cat_of(first, i);
        if !cats.contains(&c) {
            cats.push(c);
        }
    }
    if cats.is_empty() {
        return None;
    }
    // all[系列][分类] = 箱线统计
    let mut all: Vec<Vec<Option<stats::BoxStats>>> = Vec::new();
    for s in &model.series {
        let d = model.data.iter().find(|d| d.id == s.data_id).or_else(|| model.data.first())?;
        let exclusive = s.quartile_exclusive;
        let row: Vec<Option<stats::BoxStats>> = cats
            .iter()
            .map(|c| {
                let vals: Vec<f64> = d
                    .values
                    .iter()
                    .enumerate()
                    .filter_map(|(i, v)| if cat_of(d, i) == *c { *v } else { None })
                    .collect();
                let m = if exclusive {
                    stats::QuartileMethod::Exclusive
                } else {
                    stats::QuartileMethod::Inclusive
                };
                stats::box_stats(&vals, m)
            })
            .collect();
        all.push(row);
    }
    // Y 轴范围:须线端点,显示离群点时含离群点
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for (si, row) in all.iter().enumerate() {
        for bs in row.iter().flatten() {
            lo = lo.min(bs.lower_whisker);
            hi = hi.max(bs.upper_whisker);
            if model.series[si].show_outliers {
                for o in &bs.outliers {
                    lo = lo.min(*o);
                    hi = hi.max(*o);
                }
            }
        }
    }
    if !lo.is_finite() || !hi.is_finite() {
        return None;
    }
    let (a, b, n) = nice_range(lo, hi);

    let scale = if scale > 0.0 { scale } else { 1.0 };
    let wf = w as f32 / scale;
    let hf = h as f32 / scale;
    let title: Option<String> = if model.title_present {
        Some(match model.title.as_deref() {
            Some(t) if !t.trim().is_empty() => t.to_string(),
            _ => DEFAULT_CHART_TITLE.to_string(),
        })
    } else {
        None
    };
    let left = 44.0_f32;
    let right = 16.0_f32;
    let top = if title.is_some() { 44.0_f32 } else { 16.0_f32 };
    let bottom = 36.0_f32;
    let plot_w = wf - left - right;
    let plot_h = hf - top - bottom;
    if plot_w <= 0.0 || plot_h <= 0.0 {
        return None;
    }
    let y_of = |v: f64| -> f32 { top + plot_h * (1.0 - ((v - a) / (b - a)) as f32) };

    let mut svg = String::new();
    svg.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {wf:.2} {hf:.2}\">"
    ));
    svg.push_str(&format!("<rect width=\"{wf:.2}\" height=\"{hf:.2}\" fill=\"#FFFFFF\"/>"));
    {
        let sw = 1.0 / scale;
        svg.push_str(&format!(
            "<rect x=\"{:.3}\" y=\"{:.3}\" width=\"{:.3}\" height=\"{:.3}\" fill=\"none\" stroke=\"#D9D9D9\" stroke-width=\"{:.3}\"/>",
            sw / 2.0, sw / 2.0, wf - sw, hf - sw, sw
        ));
    }
    if let Some(t) = &title {
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"27\" font-family=\"serif\" font-size=\"16\" fill=\"#595959\" text-anchor=\"middle\">{}</text>",
            wf / 2.0,
            esc(t)
        ));
    }
    // 网格线 + Y 刻度
    for i in 0..=n {
        let val = a + (b - a) * i as f64 / n as f64;
        let y = y_of(val);
        if model.show_grid || val.abs() < 1e-9 {
            let color = if val.abs() < 1e-9 { "#BFBFBF" } else { "#D9D9D9" };
            svg.push_str(&format!(
                "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"{}\" stroke-width=\"1\"/>",
                left, y, left + plot_w, y, color
            ));
        }
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"serif\" font-size=\"11\" fill=\"#595959\" text-anchor=\"end\">{}</text>",
            left - 6.0,
            y + 4.0,
            fmt_num(val)
        ));
    }
    // 箱体
    let nc = cats.len();
    let ns = model.series.len();
    let slot = plot_w / nc as f32;
    let gap = model.gap_width.unwrap_or(1.0).max(0.0) as f32;
    let group_w = slot / (1.0 + gap);
    let bw = group_w / ns as f32;
    let boxw = bw * 0.9;
    for ci in 0..nc {
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"serif\" font-size=\"11\" fill=\"#595959\" text-anchor=\"middle\">{}</text>",
            left + ci as f32 * slot + slot / 2.0,
            top + plot_h + 18.0,
            esc(&cats[ci])
        ));
        for si in 0..ns {
            let bs = match &all[si][ci] {
                Some(b) => b,
                None => continue,
            };
            let ser = &model.series[si];
            let cx = left + ci as f32 * slot + slot / 2.0 - group_w / 2.0 + (si as f32 + 0.5) * bw;
            let pal = OFFICE_PALETTE[si % OFFICE_PALETTE.len()];
            let fill = format!("#{:02X}{:02X}{:02X}", pal.0, pal.1, pal.2);
            let dk = darker(pal);
            let (yq1, yq3) = (y_of(bs.q1), y_of(bs.q3));
            let (x0, x1) = (cx - boxw / 2.0, cx + boxw / 2.0);
            // 须线 + 端帽
            let cap = boxw * 0.25;
            for (ya, yb, yc) in [(yq3, y_of(bs.upper_whisker), y_of(bs.upper_whisker)),
                                 (yq1, y_of(bs.lower_whisker), y_of(bs.lower_whisker))] {
                svg.push_str(&format!(
                    "<line x1=\"{cx:.2}\" y1=\"{ya:.2}\" x2=\"{cx:.2}\" y2=\"{yb:.2}\" stroke=\"{dk}\" stroke-width=\"1\"/>\
                     <line x1=\"{:.2}\" y1=\"{yc:.2}\" x2=\"{:.2}\" y2=\"{yc:.2}\" stroke=\"{dk}\" stroke-width=\"1\"/>",
                    cx - cap, cx + cap
                ));
            }
            svg.push_str(&format!(
                "<rect class=\"box\" x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\" fill=\"{}\" stroke=\"{}\" stroke-width=\"1\"/>",
                x0, yq3, boxw, (yq1 - yq3).max(0.5), fill, dk
            ));
            let ym = y_of(bs.median);
            svg.push_str(&format!(
                "<line x1=\"{x0:.2}\" y1=\"{ym:.2}\" x2=\"{x1:.2}\" y2=\"{ym:.2}\" stroke=\"{dk}\" stroke-width=\"1\"/>"
            ));
            if ser.mean_marker {
                let (my, r) = (y_of(bs.mean), 3.5_f32);
                svg.push_str(&format!(
                    "<path class=\"mean\" d=\"M{:.2} {:.2}L{:.2} {:.2}M{:.2} {:.2}L{:.2} {:.2}\" stroke=\"{dk}\" stroke-width=\"1\" fill=\"none\"/>",
                    cx - r, my - r, cx + r, my + r, cx - r, my + r, cx + r, my - r
                ));
            }
            if ser.show_outliers {
                for o in &bs.outliers {
                    svg.push_str(&format!(
                        "<circle class=\"outlier\" cx=\"{:.2}\" cy=\"{:.2}\" r=\"2\" fill=\"{}\" stroke=\"{}\" stroke-width=\"1\"/>",
                        cx, y_of(*o), dk, dk
                    ));
                }
            }
        }
    }
    svg.push_str("</svg>");
    Some(svg)
}

/// 公共 SVG 头:白底 + 1 设备像素的浅灰外框。返回 (svg 开头, 点单位宽, 点单位高, 有效 scale)。
fn svg_head(w: u32, h: u32, scale: f32) -> (String, f32, f32, f32) {
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let wf = w as f32 / scale;
    let hf = h as f32 / scale;
    let sw = 1.0 / scale;
    let mut svg = String::new();
    svg.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {wf:.2} {hf:.2}\">"
    ));
    svg.push_str(&format!("<rect width=\"{wf:.2}\" height=\"{hf:.2}\" fill=\"#FFFFFF\"/>"));
    svg.push_str(&format!(
        "<rect x=\"{:.3}\" y=\"{:.3}\" width=\"{:.3}\" height=\"{:.3}\" fill=\"none\" stroke=\"#D9D9D9\" stroke-width=\"{:.3}\"/>",
        sw / 2.0, sw / 2.0, wf - sw, hf - sw, sw
    ));
    (svg, wf, hf, scale)
}

fn title_text(model: &StatChartModel) -> Option<String> {
    if !model.title_present {
        return None;
    }
    Some(match model.title.as_deref() {
        Some(t) if !t.trim().is_empty() => t.to_string(),
        _ => DEFAULT_CHART_TITLE.to_string(),
    })
}

fn title_svg(t: &str, wf: f32) -> String {
    format!(
        "<text x=\"{:.1}\" y=\"27\" font-family=\"serif\" font-size=\"16\" fill=\"#595959\" text-anchor=\"middle\">{}</text>",
        wf / 2.0,
        esc(t)
    )
}

fn cat_at(d: &StatData, i: usize) -> String {
    d.categories.first().and_then(|c| c.get(i)).cloned().unwrap_or_default()
}

fn hex(c: (u8, u8, u8)) -> String {
    format!("#{:02X}{:02X}{:02X}", c.0, c.1, c.2)
}

fn waterfall_svg(model: &StatChartModel, w: u32, h: u32, scale: f32) -> Option<String> {
    let ser = model.series.first()?;
    let d = model.data.iter().find(|d| d.id == ser.data_id).or_else(|| model.data.first())?;
    let n = d.values.len();
    if n == 0 {
        return None;
    }
    // (起点, 终点, 类型 0增加 1减少 2汇总, 数据值)
    let mut bars: Vec<(f64, f64, u8, f64)> = Vec::new();
    let mut run = 0.0_f64;
    for i in 0..n {
        let v = d.values[i].unwrap_or(0.0);
        if ser.subtotals.contains(&i) {
            bars.push((0.0, v, 2, v));
            run = v;
        } else {
            let e = run + v;
            bars.push((run, e, if v >= 0.0 { 0 } else { 1 }, v));
            run = e;
        }
    }
    let lo = bars.iter().map(|b| b.0.min(b.1)).fold(0.0, f64::min);
    let hi = bars.iter().map(|b| b.0.max(b.1)).fold(0.0, f64::max);
    let (a, b, nt) = nice_range_n(lo, hi, 10);

    let (mut svg, wf, hf, _scale) = svg_head(w, h, scale);
    let title = title_text(model);
    let legend = model.show_legend;
    let left = 44.0_f32;
    let right = 16.0_f32;
    let mut top = if title.is_some() { 44.0_f32 } else { 16.0_f32 };
    if legend {
        top += 20.0;
    }
    let bottom = 36.0_f32;
    let plot_w = wf - left - right;
    let plot_h = hf - top - bottom;
    if plot_w <= 0.0 || plot_h <= 0.0 {
        return None;
    }
    let y_of = |v: f64| -> f32 { top + plot_h * (1.0 - ((v - a) / (b - a)) as f32) };
    if let Some(t) = &title {
        svg.push_str(&title_svg(t, wf));
    }
    // 图例:增加/减少/汇总
    let pal_idx = [0usize, 1, 3];
    if legend {
        let names = ["增加", "减少", "汇总"];
        let item_w: Vec<f32> = names.iter().map(|n| 14.0 + n.chars().count() as f32 * 11.0).collect();
        let total: f32 = item_w.iter().sum::<f32>() + 10.0 * 2.0;
        let mut x = (wf - total) / 2.0;
        let ly = if title.is_some() { 46.0_f32 } else { 18.0_f32 };
        for k in 0..3 {
            svg.push_str(&format!(
                "<rect class=\"legend\" x=\"{:.1}\" y=\"{:.1}\" width=\"10\" height=\"10\" fill=\"{}\"/>\
                 <text x=\"{:.1}\" y=\"{:.1}\" font-family=\"serif\" font-size=\"11\" fill=\"#595959\">{}</text>",
                x, ly - 5.0, hex(OFFICE_PALETTE[pal_idx[k]]), x + 14.0, ly + 4.0, names[k]
            ));
            x += item_w[k] + 10.0;
        }
    }
    // 网格线 + Y 刻度
    for i in 0..=nt {
        let val = a + (b - a) * i as f64 / nt as f64;
        let y = y_of(val);
        if model.show_grid || val.abs() < 1e-9 {
            let color = if val.abs() < 1e-9 { "#BFBFBF" } else { "#D9D9D9" };
            svg.push_str(&format!(
                "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"{}\" stroke-width=\"1\"/>",
                left, y, left + plot_w, y, color
            ));
        }
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"serif\" font-size=\"11\" fill=\"#595959\" text-anchor=\"end\">{}</text>",
            left - 6.0, y + 4.0, fmt_num(val)
        ));
    }
    // 柱子、连接线、数据标签、类别名
    let slot = plot_w / n as f32;
    let gap = model.gap_width.unwrap_or(0.5).max(0.0) as f32;
    let bw = slot / (1.0 + gap);
    let show_val = ser.data_labels.as_ref().map(|l| l.show_value).unwrap_or(false);
    for (i, bar) in bars.iter().enumerate() {
        let x = left + i as f32 * slot + (slot - bw) / 2.0;
        let (ys, ye) = (y_of(bar.0), y_of(bar.1));
        let (ytop, bh) = (ys.min(ye), (ys - ye).abs().max(0.5));
        svg.push_str(&format!(
            "<rect class=\"bar k{}\" x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\" fill=\"{}\" stroke=\"#FFFFFF\" stroke-width=\"0.5\"/>",
            bar.2, x, ytop, bw, bh, hex(OFFICE_PALETTE[pal_idx[bar.2 as usize]])
        ));
        if i + 1 < n {
            let nx = left + (i + 1) as f32 * slot + (slot - bw) / 2.0;
            svg.push_str(&format!(
                "<line x1=\"{:.2}\" y1=\"{:.2}\" x2=\"{:.2}\" y2=\"{:.2}\" stroke=\"#BFBFBF\" stroke-width=\"1\"/>",
                x + bw, ye, nx, ye
            ));
        }
        if show_val {
            let out_y = if bar.3 >= 0.0 { ytop - 4.0 } else { ytop + bh + 13.0 };
            // 柱外标签会越过绘图区上/下沿时,挪到柱内靠顶/靠底(柱高不足一个字高则不挪)
            let overflow = if bar.3 >= 0.0 { out_y - 11.0 < top } else { out_y > top + plot_h + 4.0 };
            let ly = if overflow && bh > 16.0 {
                if bar.3 >= 0.0 { ytop + 13.0 } else { ytop + bh - 4.0 }
            } else {
                out_y
            };
            svg.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"serif\" font-size=\"11\" fill=\"#595959\" text-anchor=\"middle\">{}</text>",
                x + bw / 2.0, ly, fmt_num(bar.3)
            ));
        }
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"serif\" font-size=\"11\" fill=\"#595959\" text-anchor=\"middle\">{}</text>",
            left + i as f32 * slot + slot / 2.0, top + plot_h + 18.0, esc(&cat_at(d, i))
        ));
    }
    svg.push_str("</svg>");
    Some(svg)
}

fn funnel_svg(model: &StatChartModel, w: u32, h: u32, scale: f32) -> Option<String> {
    let ser = model.series.first()?;
    let d = model.data.iter().find(|d| d.id == ser.data_id).or_else(|| model.data.first())?;
    let n = d.values.len();
    if n == 0 {
        return None;
    }
    let vals: Vec<f64> = d.values.iter().map(|v| v.unwrap_or(0.0).max(0.0)).collect();
    let vmax = vals.iter().cloned().fold(0.0, f64::max);
    if vmax <= 0.0 {
        return None;
    }
    let cats: Vec<String> = (0..n).map(|i| cat_at(d, i)).collect();
    let maxc = cats.iter().map(|c| c.chars().count()).max().unwrap_or(0) as f32;
    let (mut svg, wf, hf, _scale) = svg_head(w, h, scale);
    let title = title_text(model);
    let left = (maxc * 11.0 + 14.0).max(30.0);
    let right = 16.0_f32;
    let top = if title.is_some() { 44.0_f32 } else { 16.0_f32 };
    let bottom = 16.0_f32;
    let plot_w = wf - left - right;
    let plot_h = hf - top - bottom;
    if plot_w <= 0.0 || plot_h <= 0.0 {
        return None;
    }
    if let Some(t) = &title {
        svg.push_str(&title_svg(t, wf));
    }
    svg.push_str(&format!(
        "<line x1=\"{l:.1}\" y1=\"{:.1}\" x2=\"{l:.1}\" y2=\"{:.1}\" stroke=\"#D9D9D9\" stroke-width=\"1\"/>",
        top, top + plot_h, l = left
    ));
    let slot = plot_h / n as f32;
    let gap = model.gap_width.unwrap_or(0.06).max(0.0) as f32;
    let bh = slot / (1.0 + gap);
    let fill = hex(OFFICE_PALETTE[0]);
    let show_val = ser.data_labels.as_ref().map(|l| l.show_value).unwrap_or(false);
    for i in 0..n {
        let bwid = plot_w * (vals[i] / vmax) as f32;
        let x = left + (plot_w - bwid) / 2.0;
        let y = top + i as f32 * slot + (slot - bh) / 2.0;
        let cy = top + i as f32 * slot + slot / 2.0;
        svg.push_str(&format!(
            "<rect class=\"bar\" x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\" fill=\"{}\" stroke=\"#FFFFFF\" stroke-width=\"0.5\"/>",
            x, y, bwid, bh, fill
        ));
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"serif\" font-size=\"11\" fill=\"#595959\" text-anchor=\"end\">{}</text>",
            left - 6.0, cy + 4.0, esc(&cats[i])
        ));
        if show_val {
            svg.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"serif\" font-size=\"11\" fill=\"#FFFFFF\" text-anchor=\"middle\">{}</text>",
                left + plot_w / 2.0, cy + 4.0, fmt_num(vals[i])
            ));
        }
    }
    svg.push_str("</svg>");
    Some(svg)
}

// ---------- 层级图(树状图 / 旭日图)共用 ----------

struct Node {
    name: String,
    own: f64,
    value: f64,
    children: Vec<Node>,
}

/// 每行数据是一条路径:categories 最后一层是最外层;空名字跳过(缺项行直接挂到上一级)。
fn build_tree(d: &StatData) -> Node {
    let mut root = Node { name: String::new(), own: 0.0, value: 0.0, children: vec![] };
    for i in 0..d.values.len() {
        let v = match d.values[i] {
            Some(v) if v > 0.0 => v,
            _ => continue,
        };
        let path: Vec<String> = d
            .categories
            .iter()
            .rev()
            .map(|l| l.get(i).cloned().unwrap_or_default())
            .filter(|s| !s.is_empty())
            .collect();
        if path.is_empty() {
            continue;
        }
        let mut cur = &mut root;
        for name in path {
            let pos = match cur.children.iter().position(|c| c.name == name) {
                Some(p) => p,
                None => {
                    cur.children.push(Node { name, own: 0.0, value: 0.0, children: vec![] });
                    cur.children.len() - 1
                }
            };
            cur = &mut cur.children[pos];
        }
        cur.own += v;
    }
    fn finish(n: &mut Node) {
        for c in n.children.iter_mut() {
            finish(c);
        }
        n.value = n.own + n.children.iter().map(|c| c.value).sum::<f64>();
        n.children
            .sort_by(|a, b| b.value.partial_cmp(&a.value).unwrap_or(std::cmp::Ordering::Equal));
    }
    finish(&mut root);
    root
}

fn tree_height(n: &Node) -> usize {
    n.children.iter().map(tree_height).max().unwrap_or(0) + 1
}

/// 顶层分支的取色顺序:蓝、橙、金黄(与 WPS 一致),之后深蓝、绿、灰(后三个为推测)。
fn tree_color(i: usize) -> String {
    const ORDER: [usize; 6] = [0, 1, 3, 4, 5, 2];
    hex(OFFICE_PALETTE[ORDER[i % 6]])
}

/// 粗略估算文字宽度(点):ASCII 约 0.55 个字号,其他按 1 个字号。
fn text_w(s: &str, fs: f32) -> f32 {
    s.chars().map(|c| if c.is_ascii() { fs * 0.55 } else { fs }).sum()
}

fn tree_show_label(model: &StatChartModel) -> bool {
    model
        .series
        .first()
        .and_then(|s| s.data_labels.as_ref())
        .map(|l| l.show_category)
        .unwrap_or(false)
}

/// squarified 树状布局。values 需已按从大到小排序且都大于 0,返回 [x, y, w, h]。
fn squarify(values: &[f64], x: f32, y: f32, w: f32, h: f32) -> Vec<[f32; 4]> {
    let mut out = vec![[x, y, 0.0, 0.0]; values.len()];
    let total: f64 = values.iter().sum();
    if total <= 0.0 || w <= 0.0 || h <= 0.0 {
        return out;
    }
    let k = (w as f64 * h as f64) / total;
    let (mut x, mut y, mut w, mut h) = (x as f64, y as f64, w as f64, h as f64);
    let mut i = 0;
    while i < values.len() {
        let short = w.min(h);
        if short <= 1e-9 {
            break;
        }
        let mut j = i;
        let mut sum = 0.0_f64;
        let mut best = f64::INFINITY;
        while j < values.len() {
            let s2 = sum + values[j] * k;
            let mx = values[i] * k;
            let mn = (values[j] * k).max(1e-12);
            let worst = ((short * short * mx) / (s2 * s2)).max((s2 * s2) / (short * short * mn));
            if j > i && worst > best * 1.1 {
                break;
            }
            best = worst;
            sum = s2;
            j += 1;
        }
        let t = sum / short;
        if w >= h {
            let mut cy = y;
            for m in i..j {
                let hh = values[m] * k / t;
                out[m] = [x as f32, cy as f32, t as f32, hh as f32];
                cy += hh;
            }
            x += t;
            w -= t;
        } else {
            let mut cx = x;
            for m in i..j {
                let ww = values[m] * k / t;
                out[m] = [cx as f32, y as f32, ww as f32, t as f32];
                cx += ww;
            }
            y += t;
            h -= t;
        }
        i = j;
    }
    out
}

fn tm_emit(n: &Node, rect: [f32; 4], depth: usize, ci: usize, show: bool, rects: &mut String, labels: &mut String) {
    let vals: Vec<f64> = n.children.iter().map(|c| c.value).collect();
    let rs = squarify(&vals, rect[0], rect[1], rect[2], rect[3]);
    for (k, c) in n.children.iter().enumerate() {
        let r = rs[k];
        let ci2 = if depth == 0 { k } else { ci };
        if c.children.is_empty() {
            rects.push_str(&format!(
                "<rect class=\"leaf\" x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\" fill=\"{}\" stroke=\"#FFFFFF\" stroke-width=\"1\"/>",
                r[0], r[1], r[2], r[3], tree_color(ci2)
            ));
            if show && r[2] > text_w(&c.name, 11.0) + 8.0 && r[3] > 16.0 {
                labels.push_str(&format!(
                    "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"serif\" font-size=\"11\" fill=\"#FFFFFF\">{}</text>",
                    r[0] + 4.0, r[1] + r[3] - 6.0, esc(&c.name)
                ));
            }
        } else {
            if depth == 0 && show && r[2] > text_w(&c.name, 11.0) + 8.0 && r[3] > 16.0 {
                labels.push_str(&format!(
                    "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"serif\" font-size=\"11\" fill=\"#FFFFFF\">{}</text>",
                    r[0] + 4.0, r[1] + 13.0, esc(&c.name)
                ));
            }
            tm_emit(c, r, depth + 1, ci2, show, rects, labels);
        }
    }
}

fn treemap_svg(model: &StatChartModel, w: u32, h: u32, scale: f32) -> Option<String> {
    let ser = model.series.first()?;
    let d = model.data.iter().find(|d| d.id == ser.data_id).or_else(|| model.data.first())?;
    let root = build_tree(d);
    if root.children.is_empty() || root.value <= 0.0 {
        return None;
    }
    let (mut svg, wf, hf, _sc) = svg_head(w, h, scale);
    let title = title_text(model);
    let mut top = if title.is_some() { 44.0_f32 } else { 12.0_f32 };
    if let Some(t) = &title {
        svg.push_str(&title_svg(t, wf));
    }
    if model.show_legend {
        let ws: Vec<f32> = root.children.iter().map(|c| 14.0 + text_w(&c.name, 11.0)).collect();
        let total: f32 = ws.iter().sum::<f32>() + 10.0 * root.children.len().saturating_sub(1) as f32;
        let mut lx = (wf - total) / 2.0;
        let ly = if title.is_some() { 46.0_f32 } else { 18.0_f32 };
        for (k, c) in root.children.iter().enumerate() {
            svg.push_str(&format!(
                "<rect class=\"legend\" x=\"{:.1}\" y=\"{:.1}\" width=\"10\" height=\"10\" fill=\"{}\"/>\
                 <text x=\"{:.1}\" y=\"{:.1}\" font-family=\"serif\" font-size=\"11\" fill=\"#595959\">{}</text>",
                lx, ly - 5.0, tree_color(k), lx + 14.0, ly + 4.0, esc(&c.name)
            ));
            lx += ws[k] + 10.0;
        }
        top += 12.0;
    }
    let (px, py) = (6.0_f32, top);
    let (pw, ph) = (wf - 12.0, hf - top - 6.0);
    if pw <= 0.0 || ph <= 0.0 {
        return None;
    }
    let mut rects = String::new();
    let mut labels = String::new();
    tm_emit(&root, [px, py, pw, ph], 0, 0, tree_show_label(model), &mut rects, &mut labels);
    svg.push_str(&rects);
    svg.push_str(&labels);
    svg.push_str("</svg>");
    Some(svg)
}

struct SunCtx {
    cx: f32,
    cy: f32,
    hole: f32,
    rw: f32,
    show: bool,
}

fn sun_pt(c: &SunCtx, r: f32, a: f64) -> (f32, f32) {
    (c.cx + r * a.cos() as f32, c.cy + r * a.sin() as f32)
}

fn sun_emit(n: &Node, depth: usize, a0: f64, a1: f64, ci: usize, c: &SunCtx, rects: &mut String, labels: &mut String) {
    if depth > 0 {
        let r0 = c.hole + (depth - 1) as f32 * c.rw;
        let r1 = r0 + c.rw;
        let span = (a1 - a0).min(std::f64::consts::TAU - 1e-4);
        let large = if span > std::f64::consts::PI { 1 } else { 0 };
        let (x0, y0) = sun_pt(c, r1, a0);
        let (x1, y1) = sun_pt(c, r1, a0 + span);
        let (x2, y2) = sun_pt(c, r0, a0 + span);
        let (x3, y3) = sun_pt(c, r0, a0);
        rects.push_str(&format!(
            "<path class=\"arc\" d=\"M{:.2} {:.2}A{r1:.2} {r1:.2} 0 {large} 1 {:.2} {:.2}L{:.2} {:.2}A{r0:.2} {r0:.2} 0 {large} 0 {:.2} {:.2}Z\" fill=\"{}\" stroke=\"#FFFFFF\" stroke-width=\"1.5\"/>",
            x0, y0, x1, y1, x2, y2, x3, y3, tree_color(ci), r0 = r0, r1 = r1, large = large
        ));
        let rm = (r0 + r1) / 2.0;
        let am = a0 + span / 2.0;
        let tw = text_w(&n.name, 11.0);
        let fits = tw * (am.cos().abs() as f32) + 4.0 <= c.rw
            && tw * (am.sin().abs() as f32) + 4.0 <= rm * span as f32;
        if c.show && c.rw > 14.0 && fits {
            let (tx, ty) = sun_pt(c, rm, am);
            labels.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" font-family=\"serif\" font-size=\"11\" fill=\"#FFFFFF\" text-anchor=\"middle\">{}</text>",
                tx, ty + 4.0, esc(&n.name)
            ));
        }
    }
    let mut a = a0;
    for (k, ch) in n.children.iter().enumerate() {
        let sub = (a1 - a0) * ch.value / n.value;
        sun_emit(ch, depth + 1, a, a + sub, if depth == 0 { k } else { ci }, c, rects, labels);
        a += sub;
    }
}

fn sunburst_svg(model: &StatChartModel, w: u32, h: u32, scale: f32) -> Option<String> {
    let ser = model.series.first()?;
    let d = model.data.iter().find(|d| d.id == ser.data_id).or_else(|| model.data.first())?;
    let root = build_tree(d);
    if root.children.is_empty() || root.value <= 0.0 {
        return None;
    }
    let (mut svg, wf, hf, _sc) = svg_head(w, h, scale);
    let title = title_text(model);
    let top = if title.is_some() { 44.0_f32 } else { 12.0_f32 };
    if let Some(t) = &title {
        svg.push_str(&title_svg(t, wf));
    }
    let avail_h = hf - top - 12.0;
    let r_out = ((wf - 24.0).min(avail_h)) / 2.0;
    if r_out <= 10.0 {
        return None;
    }
    let depth = tree_height(&root) - 1;
    let hole = r_out * 0.26;
    let c = SunCtx {
        cx: wf / 2.0,
        cy: top + avail_h / 2.0,
        hole,
        rw: (r_out - hole) / depth as f32,
        show: tree_show_label(model),
    };
    let mut rects = String::new();
    let mut labels = String::new();
    sun_emit(&root, 0, -std::f64::consts::FRAC_PI_2, -std::f64::consts::FRAC_PI_2 + std::f64::consts::TAU, 0, &c, &mut rects, &mut labels);
    svg.push_str(&rects);
    svg.push_str(&labels);
    svg.push_str("</svg>");
    Some(svg)
}

/// StatChartModel → SVG。未实现的类型返回 None。
pub fn stat_chart_to_svg(model: &StatChartModel, w: u32, h: u32, scale: f32) -> Option<String> {
    if w == 0 || h == 0 {
        return None;
    }
    match model.kind {
        StatChartKind::Histogram => histogram_svg(model, w, h, scale),
        StatChartKind::BoxWhisker => box_svg(model, w, h, scale),
        StatChartKind::Waterfall => waterfall_svg(model, w, h, scale),
        StatChartKind::Funnel => funnel_svg(model, w, h, scale),
        StatChartKind::Treemap => treemap_svg(model, w, h, scale),
        StatChartKind::Sunburst => sunburst_svg(model, w, h, scale),
        _ => None,
    }
}

/// 一站式:StatChartModel → RGBA 像素(长度 w*h*4)。None 表示暂不支持,调用方应回退到占位框。
pub fn render_stat_chart_to_rgba(model: &StatChartModel, w: u32, h: u32, scale: f32) -> Option<Vec<u8>> {
    let svg = stat_chart_to_svg(model, w, h, scale)?;
    let pixmap = svg_to_pixmap(&svg, w, h)?;
    Some(pixmap.data().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::stat_chart::{StatData, StatSeries};

    fn hist_model() -> StatChartModel {
        StatChartModel {
            kind: StatChartKind::Histogram,
            title_present: true,
            title: None,
            show_legend: false,
            legend_pos: None,
            show_grid: true,
            gap_width: Some(0.0),
            data: vec![StatData {
                id: 0,
                categories: vec![],
                values: (1..=25).map(|v| Some(v as f64)).collect(),
                value_kind: "val".to_string(),
            }],
            series: vec![StatSeries {
                name: "s".to_string(),
                layout_id: "clusteredColumn".to_string(),
                format_idx: 0,
                data_id: 0,
                bin_width: Some(4.0),
                bin_count: None,
                interval_closed_right: true,
                quartile_exclusive: false,
                mean_line: false,
                mean_marker: false,
                show_outliers: false,
                show_non_outliers: false,
                subtotals: vec![],
                data_labels: None,
            }],
        }
    }

    #[test]
    fn histogram_has_six_bars_and_labels() {
        let svg = stat_chart_to_svg(&hist_model(), 480, 300, 1.0).expect("svg");
        assert_eq!(svg.matches("class=\"bar\"").count(), 6);
        for l in ["[1, 5]", "(5, 9]", "(9, 13]", "(13, 17]", "(17, 21]", "(21, 25]"] {
            assert!(svg.contains(l), "缺少区间标签 {}", l);
        }
        assert!(svg.contains(DEFAULT_CHART_TITLE));
    }

    #[test]
    fn unsupported_kind_returns_none() {
        let mut m = hist_model();
        m.kind = StatChartKind::Treemap;
        assert!(stat_chart_to_svg(&m, 480, 300, 1.0).is_none());
    }

    #[test]
    fn rgba_has_expected_size() {
        let buf = render_stat_chart_to_rgba(&hist_model(), 480, 300, 1.0).expect("rgba");
        assert_eq!(buf.len(), 480 * 300 * 4);
        // 有蓝色柱子:至少存在一个与白底不同的像素
        assert!(buf.chunks(4).any(|p| p[0] != 255 || p[1] != 255 || p[2] != 255));
    }

    fn box_model() -> StatChartModel {
        // 分类 A 有 9 个值(含离群点 200),分类 B 有 9 个值;两个系列
        let mk = |id: u32, with_outlier: bool| -> StatData {
            let mut cats: Vec<String> = vec!["A".to_string(); 9];
            cats.extend(vec!["B".to_string(); 9]);
            let mut vals: Vec<Option<f64>> = (10..=17).map(|v| Some(v as f64)).collect();
            vals.push(Some(if with_outlier { 200.0 } else { 18.0 }));
            vals.extend((1..=9).map(|v| Some(v as f64)));
            StatData { id, categories: vec![cats], values: vals, value_kind: "val".to_string() }
        };
        let ser = |id: u32| StatSeries {
            name: "s".to_string(),
            layout_id: "boxWhisker".to_string(),
            format_idx: id,
            data_id: id,
            bin_width: None,
            bin_count: None,
            interval_closed_right: false,
            quartile_exclusive: true,
            mean_line: false,
            mean_marker: true,
            show_outliers: true,
            show_non_outliers: true,
            subtotals: vec![],
            data_labels: None,
        };
        StatChartModel {
            kind: StatChartKind::BoxWhisker,
            title_present: true,
            title: None,
            show_legend: false,
            legend_pos: None,
            show_grid: true,
            gap_width: Some(1.0),
            data: vec![mk(0, true), mk(1, false)],
            series: vec![ser(0), ser(1)],
        }
    }

    #[test]
    fn box_has_expected_shapes() {
        let svg = stat_chart_to_svg(&box_model(), 480, 300, 1.0).expect("svg");
        assert_eq!(svg.matches("class=\"box\"").count(), 4); // 2 分类 × 2 系列
        assert_eq!(svg.matches("class=\"mean\"").count(), 4);
        assert_eq!(svg.matches("class=\"outlier\"").count(), 1); // 只有 A 类第一个系列的 200
        assert!(svg.contains(">A<") && svg.contains(">B<"));
    }

    #[test]
    fn nice_range_matches_wps_sample() {
        assert_eq!(nice_range(-78.0, 128.0), (-100.0, 150.0, 5));
    }

    fn simple_model(kind: StatChartKind, layout: &str, vals: &[f64], subtotals: Vec<usize>, pos: &str) -> StatChartModel {
        use roe_model::stat_chart::StatDataLabels;
        StatChartModel {
            kind,
            title_present: true,
            title: None,
            show_legend: layout == "waterfall",
            legend_pos: None,
            show_grid: layout == "waterfall",
            gap_width: Some(0.5),
            data: vec![StatData {
                id: 0,
                categories: vec![(1..=vals.len()).map(|i| format!("类别{}", i)).collect()],
                values: vals.iter().map(|v| Some(*v)).collect(),
                value_kind: "val".to_string(),
            }],
            series: vec![StatSeries {
                name: "s".to_string(),
                layout_id: layout.to_string(),
                format_idx: 0,
                data_id: 0,
                bin_width: None,
                bin_count: None,
                interval_closed_right: false,
                quartile_exclusive: true,
                mean_line: false,
                mean_marker: false,
                show_outliers: false,
                show_non_outliers: false,
                subtotals,
                data_labels: Some(StatDataLabels {
                    pos: pos.to_string(),
                    show_value: true,
                    show_category: false,
                    show_series: false,
                }),
            }],
        }
    }

    #[test]
    fn waterfall_bars_colors_and_axis() {
        let m = simple_model(
            StatChartKind::Waterfall, "waterfall",
            &[100.0, 20.0, 50.0, -40.0, 130.0, -60.0, 70.0, 140.0], vec![7, 4], "outEnd",
        );
        let svg = stat_chart_to_svg(&m, 480, 300, 1.0).expect("svg");
        assert_eq!(svg.matches("class=\"bar k").count(), 8);
        assert_eq!(svg.matches("class=\"bar k0\"").count(), 4);
        assert_eq!(svg.matches("class=\"bar k1\"").count(), 2);
        assert_eq!(svg.matches("class=\"bar k2\"").count(), 2);
        assert_eq!(svg.matches("class=\"legend\"").count(), 3);
        assert!(svg.contains(">-40<") && svg.contains(">180<"));
        assert_eq!(nice_range_n(0.0, 170.0, 10), (0.0, 180.0, 9));
    }

    #[test]
    fn funnel_bars_and_labels() {
        let m = simple_model(
            StatChartKind::Funnel, "funnel",
            &[5000.0, 4000.0, 3000.0, 1000.0, 250.0], vec![], "ctr",
        );
        let svg = stat_chart_to_svg(&m, 480, 300, 1.0).expect("svg");
        assert_eq!(svg.matches("class=\"bar\"").count(), 5);
        assert!(svg.contains(">5000<") && svg.contains(">250<") && svg.contains(">类别5<"));
    }

    fn tree_model(kind: StatChartKind) -> StatChartModel {
        // 4 行:第 3 行叶子层为空(叶子名写在茎那一列)
        let cats = vec![
            vec!["a".to_string(), "b".to_string(), "".to_string(), "d".to_string()],
            vec!["s1".to_string(), "s1".to_string(), "c".to_string(), "s2".to_string()],
            vec!["B1".to_string(), "B1".to_string(), "B1".to_string(), "B2".to_string()],
        ];
        let mut m = simple_model(kind, "sunburst", &[3.0, 2.0, 4.0, 1.0], vec![], "ctr");
        m.data[0].categories = cats;
        m.show_legend = true;
        m.series[0].data_labels = Some(roe_model::stat_chart::StatDataLabels {
            pos: "ctr".to_string(),
            show_value: false,
            show_category: true,
            show_series: false,
        });
        m
    }

    #[test]
    fn tree_built_from_paths() {
        let m = tree_model(StatChartKind::Treemap);
        let root = build_tree(&m.data[0]);
        assert_eq!(root.value, 10.0);
        assert_eq!(root.children[0].name, "B1");
        assert_eq!(root.children[0].value, 9.0);
        assert_eq!(root.children[0].children[0].name, "s1"); // 5 > c 的 4
        assert_eq!(root.children[1].name, "B2");
    }

    #[test]
    fn squarify_fills_area() {
        let v = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0];
        let rs = squarify(&v, 0.0, 0.0, 6.0, 4.0);
        let area: f32 = rs.iter().map(|r| r[2] * r[3]).sum();
        assert!((area - 24.0).abs() < 0.01, "面积 {}", area);
        for r in rs {
            assert!(r[0] >= -0.001 && r[1] >= -0.001 && r[0] + r[2] <= 6.001 && r[1] + r[3] <= 4.001);
        }
    }

    #[test]
    fn treemap_leaves_and_legend() {
        let svg = stat_chart_to_svg(&tree_model(StatChartKind::Treemap), 600, 400, 1.0).expect("svg");
        assert_eq!(svg.matches("class=\"leaf\"").count(), 4); // a b c d
        assert_eq!(svg.matches("class=\"legend\"").count(), 2); // B1 B2
    }

    #[test]
    fn sunburst_arc_count() {
        let svg = stat_chart_to_svg(&tree_model(StatChartKind::Sunburst), 600, 400, 1.0).expect("svg");
        assert_eq!(svg.matches("class=\"arc\"").count(), 8); // B1 s1 a b c B2 s2 d
    }
}
