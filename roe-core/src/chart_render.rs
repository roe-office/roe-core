//! 图表渲染公共实现：ChartModel → SVG → RGBA/PNG。
//!
//! 此前 Writer(roe-impress/src/chart.rs)、Impress(slide_renderer.rs)、
//! Calc(tile_render_helpers.rs) 各自复制了一份 `match ChartKind` 渲染逻辑，
//! 且三份行为已产生分叉（标题缺省值、系列颜色、gapWidth、字体加载、边框）。
//! 本模块是唯一实现，三处调用方逐步迁移到这里。roe-core 是三者的共同下游，
//! 且已依赖 charts-rs / resvg，不引入新依赖、不产生循环依赖。

use charts_rs::{
    BarChart, HorizontalBarChart, LineChart, PieChart, RadarChart, ScatterChart, Series,
};
use roe_model::impress::{ChartGrouping, ChartKind, ChartModel};

/// Office 2013+ 默认主题 accent1~6，作为系列颜色缺省值（模型里没有显式颜色时使用）。
pub(crate) const OFFICE_PALETTE: [(u8, u8, u8); 6] = [
    (0x5B, 0x9B, 0xD5), // accent1 蓝
    (0xED, 0x7D, 0x31), // accent2 橙
    (0xA5, 0xA5, 0xA5), // accent3 灰
    (0xFF, 0xC0, 0x00), // accent4 金黄
    (0x44, 0x72, 0xC4), // accent5 深蓝
    (0x70, 0xAD, 0x47), // accent6 绿
];

/// 多环环形图的中心孔系数。Word 的 holeSize 对单环是"中心孔占外径的比例"；
/// 多环时若照搬会让每环只剩 (1-hole)/n 的厚度而过细，按经验缩小孔径为此系数倍。
/// 数值需对照 Word 实际渲染调整。
const MULTI_RING_HOLE_FACTOR: f32 = 0.7;
/// 多环环形图中心孔占外径的上限，保证每环仍有可见厚度。
const MULTI_RING_HOLE_MAX: f32 = 0.8;

/// 扇区描边色：模型里有就用（Word 为 lt1 白色分隔线），没有则不描边（旧文档/Calc 行为不变）。
// ROE-FIX(unified-fonts): 普通图字号与统计图同口径(标题16不加粗,图例/刻度11)
macro_rules! apply_unified_fonts {
    ($c:expr) => {
        $c.title_font_size = 16.0;
        $c.title_font_weight = None;
        $c.legend_font_size = 11.0;
        $c.x_axis_font_size = 11.0;
        for cfg in $c.y_axis_configs.iter_mut() {
            cfg.axis_font_size = 11.0;
        }
    };
}

fn slice_border_color(model: &ChartModel) -> Option<charts_rs::Color> {
    model
        .slice_border_color
        .map(|c| charts_rs::Color { r: c.r, g: c.g, b: c.b, a: c.a })
}

/// ChartModel → SVG 字符串。width/height 为目标像素尺寸。
pub fn chart_model_to_svg(model: &ChartModel, width: u32, height: u32) -> Option<String> {
    // 百分比堆积：charts-rs 没有百分比模式，这里按 x 位置把各系列归一化为占比(0~100)。
    let stacked = model.grouping != ChartGrouping::Standard
        && matches!(
            model.kind,
            ChartKind::Bar | ChartKind::HorizontalBar | ChartKind::Line | ChartKind::Area
        );
    let percent = stacked && model.grouping == ChartGrouping::PercentStacked;
    let col_sum = |i: usize| -> f64 {
        model.series.iter().map(|s| s.data.get(i).copied().unwrap_or(0.0)).sum()
    };
    // Word/OOXML 饼图语义：只画第一个 series，其每个数据点是一个扇区，图例显示类目名。
    // charts-rs 是"每个 Series 一个扇区"，所以把第一个 series 展开成多个 Series。
    // 仅当第一个 series 有多个数据点时才展开；每 series 一个数据点（Calc 旧语义）保持原样。
    // 多环环形图：Doughnut 且有多个 series，每个 series 都有多个数据点 -> 每个 series 一环。
    if matches!(model.kind, ChartKind::Doughnut)
        && model.series.len() > 1
        && model.series.iter().all(|s| s.data.len() > 1)
    {
        return chart_model_doughnut_rings_to_svg(model, width, height);
    }

    let pie_expand = matches!(model.kind, ChartKind::Pie | ChartKind::Doughnut)
        && model.series.first().map(|s| s.data.len() > 1).unwrap_or(false);
    let pie_slice_count = model.series.first().map(|s| s.data.len()).unwrap_or(0);
    if pie_expand {
        let first = &model.series[0];
        let slices: Vec<Series> = first
            .data
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                let name = model
                    .x_labels
                    .get(i)
                    .filter(|l| !l.is_empty())
                    .cloned()
                    .unwrap_or_else(|| format!("{}", i + 1));
                Series::new(name, vec![v as f32])
            })
            .collect();
        return chart_model_pie_to_svg(model, slices, pie_slice_count, width, height);
    }

    let series: Vec<Series> = model
        .series
        .iter()
        .map(|s| {
            let data: Vec<f32> = s
                .data
                .iter()
                .enumerate()
                .map(|(i, &v)| {
                    if percent {
                        let t = col_sum(i);
                        if t.abs() > f64::EPSILON { (v / t * 100.0) as f32 } else { 0.0 }
                    } else {
                        v as f32
                    }
                })
                .collect();
            let mut ser = Series::new(s.name.clone(), data);
            // 数据标签：百分比堆积的数据已被归一化，显示的不是原值，故不开
            ser.label_show = model.data_labels.show_val && !percent;
            // 线性趋势线（折线/面积图；柱形图库里不支持）
            if s.trendline && matches!(model.kind, ChartKind::Line | ChartKind::Area) {
                ser.mark_lines = vec![charts_rs::MarkLine {
                    category: charts_rs::MarkLineCategory::Trend,
                    label_show: false,
                }];
            }
            // 组合图：柱形图里属于 lineChart 的系列按折线画，且不参与堆积
            let combo_line = s.as_line && matches!(model.kind, ChartKind::Bar);
            if combo_line {
                ser.category = Some(charts_rs::SeriesCategory::Line);
            }
            if stacked && !combo_line {
                ser.stack = Some("stack".to_string());
            }
            if let Some(c) = s.color {
                ser.colors = Some(vec![
                    Some(charts_rs::Color { r: c.r, g: c.g, b: c.b, a: c.a });
                    s.data.len()
                ]);
            }
            // ROE-FIX(scatter-x): 库的散点图按 [x,y,x,y,...] 交替取数，
            // 文档里 X 为文字或缺失时，X 取 1 起的序号（与 Word/WPS 一致）
            if matches!(model.kind, ChartKind::Scatter) {
                let ys = std::mem::take(&mut ser.data);
                ser.data = ys
                    .into_iter()
                    .enumerate()
                    .flat_map(|(i, y)| [Some((i + 1) as f32), y])
                    .collect();
            }
            ser
        })
        .collect();

    // 图例取色读的是图表级 series_colors（不是逐点的 Series.colors），
    // 必须同步写入，否则图例颜色与图形颜色不一致。
    // 模型里没有显式颜色的系列，按序号取 Office 调色板兜底。
    let series_colors: Vec<charts_rs::Color> = model
        .series
        .iter()
        .enumerate()
        .map(|(i, s)| match s.color {
            Some(c) => charts_rs::Color { r: c.r, g: c.g, b: c.b, a: c.a },
            None => {
                let (r, g, b) = OFFICE_PALETTE[i % OFFICE_PALETTE.len()];
                charts_rs::Color { r, g, b, a: 255 }
            }
        })
        .collect();

    let grid_width = if model.show_grid { 1.0 } else { 0.0 };
    let w = width as f32;
    let h = height as f32;

    // 各图表类型共用的公共字段设置
    macro_rules! apply_common {
        ($c:expr) => {
            if let Some(t) = &model.title {
                $c.title_text = t.clone();
            }
            $c.width = w;
            $c.height = h;
            $c.legend_show = Some(model.show_legend);
            $c.stack_title_legend = true;
            $c.legend_category = charts_rs::LegendCategory::RoundRect;
            apply_unified_fonts!($c);
            $c.grid_stroke_width = grid_width;
            $c.series_colors = series_colors.clone();
        };
    }

    // 坐标轴标题（竖直柱/折线/面积/散点；条形图轴向相反，暂不传）
    // ROE-FIX(percent-axis): 百分比堆积图 Y 轴固定 0%~100%，每 10% 一档
    macro_rules! apply_percent_axis {
        ($c:expr) => {
            if percent {
                if let Some(cfg) = $c.y_axis_configs.first_mut() {
                    cfg.axis_min = Some(0.0);
                    cfg.axis_max = Some(100.0);
                    cfg.axis_split_number = 10;
                    cfg.axis_formatter = Some("{c}%".to_string());
                }
            } else if let Some((mx, n)) = model_data_max(model, stacked).and_then(nice_axis) {
                if let Some(cfg) = $c.y_axis_configs.first_mut() {
                    cfg.axis_min = Some(0.0);
                    cfg.axis_max = Some(mx);
                    cfg.axis_split_number = n;
                }
            }
        };
    }

    macro_rules! apply_axis_titles {
        ($c:expr) => {
            $c.x_axis_title_text = model.x_axis_label.clone();
            $c.y_axis_title_text = model.y_axis_label.clone();
        };
    }

    let svg = match model.kind {
        ChartKind::Bar => {
            let mut c = BarChart::new(series, model.x_labels.clone());
            apply_common!(c);
            apply_axis_titles!(c);
            // Word dLblPos=ctr：标签居中于柱段（堆积图默认如此）
            c.label_center = model.data_labels.position
                == roe_model::impress::DataLabelPos::Center;
            // Word/PPTX 的 gapWidth 以【单根柱宽】为基准，charts-rs 的 bar_chart_margin_pct
            // 以【簇宽】为基准，需换算：pct = gapWidth / (n + (n-1)*s)，
            // 其中 n 为簇内柱数，s 为柱间距占柱宽的比例（overlap 为负时 = -overlap/100）。
            // 堆积图所有系列叠成一根柱，等价于簇内只有 1 根柱。
            // 组合图：折线系列不占柱位，n 只数柱系列。
            let combo = model.series.iter().any(|s| s.as_line);
            let bar_n = model.series.iter().filter(|s| !s.as_line).count().max(1) as f32;
            let n = if stacked { 1.0 } else { bar_n };
            let s = if stacked {
                0.0
            } else {
                model.overlap_pct.map(|o| (-o).max(0.0) / 100.0).unwrap_or(0.0)
            };
            // 组合图文档未写 gapWidth 时，Word 默认 150
            c.bar_chart_margin_pct = model
                .gap_width_pct
                .or(Some(150.0))
                .map(|g| g / (n + (n - 1.0) * s));
            // 组合图折线加粗（该字段只影响折线，柱形不用）
            if combo {
                c.series_stroke_width = 3.5;
                c.series_symbol = None;
            }
            // overlap 为负 => 簇内柱间有空隙，对应 charts-rs 的 bar_chart_gap_pct（占柱宽百分比）
            c.bar_chart_gap_pct = if stacked {
                None
            } else {
                model.overlap_pct.filter(|o| *o < 0.0).map(|o| -o)
            };
            apply_percent_axis!(c);
            c.svg().ok()?
        }
        ChartKind::HorizontalBar => {
            let mut c = HorizontalBarChart::new(series, model.x_labels.clone());
            apply_common!(c);
            // gapWidth 以单根柱宽为基准：类目高度 = n*柱高 + g*柱高（g = gapWidth/100）。
            // 留白占类目高度的比例 = g / (n + g)；堆积图等价于 n = 1。
            if let Some(gw) = Some(model.gap_width_pct.unwrap_or(150.0)) {
                let g = gw / 100.0;
                let n = if stacked { 1.0 } else { model.series.len().max(1) as f32 };
                c.bar_chart_margin_pct = Some(g / (n + g));
            }
            apply_percent_axis!(c);
            c.svg().ok()?
        }
        ChartKind::Line => {
            let mut c = LineChart::new(series, model.x_labels.clone());
            apply_common!(c);
            apply_axis_titles!(c);
            c.trend_follow_series_color = true;
            c.series_stroke_width = 3.5;
            c.series_symbol = None;
            apply_percent_axis!(c);
            c.svg().ok()?
        }
        ChartKind::Area => {
            let mut c = LineChart::new(series, model.x_labels.clone());
            apply_common!(c);
            apply_axis_titles!(c);
            c.series_fill = true;
            c.trend_follow_series_color = true;
            c.series_symbol = None;
            c.x_boundary_gap = Some(false);
            c.edge_label_align = true; // ROE-PATCH(edge-label)
            apply_percent_axis!(c);
            c.svg().ok()?
        }
        ChartKind::Pie => {
            let mut c = PieChart::new(series);
            apply_common!(c);
            // charts-rs 默认 rose_type=true、inner_radius=40，普通饼图需显式关闭
            c.rose_type = Some(false);
            c.border_radius = Some(0.0);
            c.slice_stroke_color = slice_border_color(model);
            c.inner_radius = 0.0;
            c.svg().ok()?
        }
        ChartKind::Doughnut => {
            let mut c = PieChart::new(series);
            apply_common!(c);
            c.rose_type = Some(false);
            c.border_radius = Some(0.0);
            c.slice_stroke_color = slice_border_color(model);
            // 复现 charts-rs 内部实际半径公式，使内圈按比例落在真实半径上
            let rd = w.min(h) * 0.8;
            let r = (rd / 2.0).min(150.0);
            c.radius = r;
            c.inner_radius = r * 0.6;
            c.svg().ok()?
        }
        ChartKind::Scatter => {
            let mut c = ScatterChart::new(series);
            apply_common!(c);
            c.x_axis_config.axis_font_size = 11.0;
            apply_axis_titles!(c);
            c.series_symbols = model
                .series
                .iter()
                .map(|_| charts_rs::Symbol::Circle(0.0, None))
                .collect();
            // ROE-FIX(scatter-xmax): X 轴范围 0~点数+1,整数刻度,与 Word/WPS 一致
            let scatter_pts = model.series.iter().map(|s| s.data.len()).max().unwrap_or(0);
            if scatter_pts > 0 {
                c.x_axis_config.axis_min = Some(0.0);
                c.x_axis_config.axis_max = Some((scatter_pts + 1) as f32);
                c.x_axis_config.axis_split_number = scatter_pts + 1;
            }
            apply_percent_axis!(c);
            c.svg().ok()?
        }
        ChartKind::Radar => {
            let radar_data_max = model
                .series
                .iter()
                .flat_map(|s| s.data.iter().copied())
                .fold(0.0_f64, f64::max);
            let radar_max = ((radar_data_max / 5.0).ceil() * 5.0).max(5.0) as f32;
            let mut c = RadarChart::new(
                series,
                model
                    .x_labels
                    .iter()
                    .map(|s| charts_rs::RadarIndicator { name: s.clone(), max: radar_max })
                    .collect(),
            );
            apply_common!(c);
            c.svg().ok()?
        }
    };
    Some(svg)
}

// ROE-FIX(nice-axis): Word/Excel 风格刻度——步长取 1/2/5×10^k,分段不超过 10,留约 5% 余量
pub(crate) fn nice_axis(data_max: f64) -> Option<(f32, usize)> {
    if !data_max.is_finite() || data_max <= 0.0 {
        return None;
    }
    let top = data_max * 1.05;
    let base = top.log10().floor() as i32 - 2;
    for e in base..(base + 6) {
        for m in [1.0_f64, 2.0, 5.0] {
            let step = m * 10f64.powi(e);
            let n = (top / step).ceil() as usize;
            if (1..=10).contains(&n) {
                return Some(((step * n as f64) as f32, n));
            }
        }
    }
    None
}

/// 主 Y 轴数据最大值;堆积图取各类目叠加和;含负数返回 None(保持库的原行为)
fn model_data_max(model: &ChartModel, stacked: bool) -> Option<f64> {
    if model.series.iter().any(|s| s.data.iter().any(|v| *v < 0.0)) {
        return None;
    }
    let len = model.series.iter().map(|s| s.data.len()).max().unwrap_or(0);
    let mut mx = 0.0_f64;
    for s in model.series.iter().filter(|s| s.as_line || !stacked) {
        for v in &s.data {
            mx = mx.max(*v);
        }
    }
    if stacked {
        for i in 0..len {
            let sum: f64 = model
                .series
                .iter()
                .filter(|s| !s.as_line)
                .map(|s| s.data.get(i).copied().unwrap_or(0.0))
                .sum();
            mx = mx.max(sum);
        }
    }
    Some(mx)
}

/// 多环环形图：每个 series 一环（最内环 = 第一个 series），各环按同一套类目名/颜色切分。
fn chart_model_doughnut_rings_to_svg(model: &ChartModel, width: u32, height: u32) -> Option<String> {
    let slice_count = model.series.iter().map(|s| s.data.len()).max().unwrap_or(0);
    let make_ring = |data: &Vec<f64>| -> Vec<Series> {
        (0..slice_count)
            .map(|i| {
                let name = model
                    .x_labels
                    .get(i)
                    .filter(|l| !l.is_empty())
                    .cloned()
                    .unwrap_or_else(|| format!("{}", i + 1));
                Series::new(name, vec![data.get(i).copied().unwrap_or(0.0) as f32])
            })
            .collect()
    };
    let mut rings: Vec<Vec<Series>> = model.series.iter().map(|s| make_ring(&s.data)).collect();
    let first = rings.remove(0);

    let series_colors: Vec<charts_rs::Color> = (0..slice_count)
        .map(|i| {
            let (r, g, b) = OFFICE_PALETTE[i % OFFICE_PALETTE.len()];
            charts_rs::Color { r, g, b, a: 255 }
        })
        .collect();
    let w = width as f32;
    let h = height as f32;

    let mut c = PieChart::new(first);
    if let Some(t) = &model.title {
        c.title_text = t.clone();
    }
    c.width = w;
    c.height = h;
    c.legend_show = Some(model.show_legend);
    c.stack_title_legend = true;
    c.legend_category = charts_rs::LegendCategory::RoundRect;
    apply_unified_fonts!(c);
    c.series_colors = series_colors;
    c.rose_type = Some(false);
    c.border_radius = Some(0.0);
    c.slice_stroke_color = slice_border_color(model);
    // 与单环一致：复现 charts-rs 内部半径公式，内圈按比例留出中心空洞
    let rd = w.min(h) * 0.8;
    let r = (rd / 2.0).min(150.0);
    c.radius = r;
    let hole = model
        .hole_size_pct
        .map(|h| (h / 100.0) * MULTI_RING_HOLE_FACTOR)
        .unwrap_or(0.35)
        .clamp(0.0, MULTI_RING_HOLE_MAX);
    c.inner_radius = r * hole;
    c.extra_rings = rings;
    c.svg().ok()
}

/// Word 语义饼图/环形图：slices 已是"每扇区一个 Series"。扇区颜色按序号取 Office 调色板。
fn chart_model_pie_to_svg(
    model: &ChartModel,
    slices: Vec<Series>,
    slice_count: usize,
    width: u32,
    height: u32,
) -> Option<String> {
    let series_colors: Vec<charts_rs::Color> = (0..slice_count)
        .map(|i| {
            let (r, g, b) = OFFICE_PALETTE[i % OFFICE_PALETTE.len()];
            charts_rs::Color { r, g, b, a: 255 }
        })
        .collect();
    let w = width as f32;
    let h = height as f32;
    let grid_width = if model.show_grid { 1.0 } else { 0.0 };

    macro_rules! apply_pie_common {
        ($c:expr) => {
            // Word 饼图/环形图标签只显示数值
            if model.data_labels.show_val {
                $c.series_label_formatter = "{c}".to_string();
            }
            // 文档明确写了 dLbls 却不显示数值（如 Word 里没有标签的饼图）：隐藏标签
            $c.hide_labels = model.data_labels.explicit && !model.data_labels.show_val;
            if let Some(t) = &model.title {
                $c.title_text = t.clone();
            }
            $c.width = w;
            $c.height = h;
            $c.legend_show = Some(model.show_legend);
            $c.stack_title_legend = true;
            $c.legend_category = charts_rs::LegendCategory::RoundRect;
            apply_unified_fonts!($c);
            $c.grid_stroke_width = grid_width;
            $c.series_colors = series_colors.clone();
        };
    }

    let svg = match model.kind {
        ChartKind::Doughnut => {
            let mut c = PieChart::new(slices);
            apply_pie_common!(c);
            c.rose_type = Some(false);
            c.border_radius = Some(0.0);
            c.slice_stroke_color = slice_border_color(model);
            let rd = w.min(h) * 0.8;
            let r = (rd / 2.0).min(150.0);
            c.radius = r;
            // Word 的 holeSize 是中心孔占外径的百分比；缺省沿用 60%
            c.inner_radius = r * model.hole_size_pct.map(|h| h / 100.0).unwrap_or(0.6).clamp(0.0, 0.95);
            c.svg().ok()?
        }
        _ => {
            let mut c = PieChart::new(slices);
            apply_pie_common!(c);
            c.rose_type = Some(false);
            c.border_radius = Some(0.0);
            c.slice_stroke_color = slice_border_color(model);
            c.inner_radius = 0.0;
            c.svg().ok()?
        }
    };
    Some(svg)
}

/// SVG → 像素缓冲（tiny-skia Pixmap）。带系统字体加载。
/// resvg 的 usvg::Options::default() 的 fontdb 是空库，找不到字体的 <text>
/// 会被静默跳过（图例/刻度/标题文字全部消失），所以必须显式加载系统字体。
/// charts-rs 生成的 SVG 硬编码 font-family="Roboto"，多数环境没有，
/// 故把 serif 通用族指向 Noto Sans CJK SC 作为回退。
pub fn svg_to_pixmap(svg: &str, width: u32, height: u32) -> Option<resvg::tiny_skia::Pixmap> {
    let mut opt = resvg::usvg::Options::default();
    opt.fontdb_mut().load_system_fonts();
    opt.fontdb_mut().set_serif_family("Noto Sans CJK SC");
    let tree = resvg::usvg::Tree::from_str(svg, &opt).ok()?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)?;
    resvg::render(&tree, resvg::tiny_skia::Transform::identity(), &mut pixmap.as_mut());
    Some(pixmap)
}

/// 图表外边框：charts-rs 没有"整图外框"概念，手动描边。border_width<=0 不画。
pub fn draw_chart_border(
    pixmap: &mut resvg::tiny_skia::Pixmap,
    width: u32,
    height: u32,
    model: &ChartModel,
) {
    if model.border_width <= 0.0 {
        return;
    }
    let bw = model.border_width;
    let half = bw / 2.0;
    if let Some(rect) =
        resvg::tiny_skia::Rect::from_ltrb(half, half, width as f32 - half, height as f32 - half)
    {
        let path = resvg::tiny_skia::PathBuilder::from_rect(rect);
        let bc = model.border_color;
        let mut paint = resvg::tiny_skia::Paint::default();
        paint.set_color_rgba8(bc.r, bc.g, bc.b, bc.a);
        paint.anti_alias = true;
        let stroke = resvg::tiny_skia::Stroke { width: bw, ..Default::default() };
        pixmap.stroke_path(
            &path,
            &paint,
            &stroke,
            resvg::tiny_skia::Transform::identity(),
            None,
        );
    }
}

/// SVG → 指定像素尺寸的 pixmap:按 SVG 自身尺寸缩放铺满(矢量放大,字号/线宽随之放大)。
pub fn svg_to_pixmap_fit(svg: &str, width: u32, height: u32) -> Option<resvg::tiny_skia::Pixmap> {
    let mut opt = resvg::usvg::Options::default();
    opt.fontdb_mut().load_system_fonts();
    opt.fontdb_mut().set_serif_family("Noto Sans CJK SC");
    let tree = resvg::usvg::Tree::from_str(svg, &opt).ok()?;
    let sz = tree.size();
    if sz.width() <= 0.0 || sz.height() <= 0.0 { return None; }
    let sx = width as f32 / sz.width();
    let sy = height as f32 / sz.height();
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)?;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(sx, sy), &mut pixmap.as_mut());
    Some(pixmap)
}

/// 与统计图同口径:布局按"点"(逻辑px)生成 SVG,再整体放大 scale 倍到 width x height 物理像素。
/// scale≈1 时等同 render_chart_to_rgba。
pub fn render_chart_to_rgba_scaled(model: &ChartModel, width: u32, height: u32, scale: f32) -> Option<Vec<u8>> {
    let s = if scale > 0.0 { scale } else { 1.0 };
    if (s - 1.0).abs() < 1e-3 {
        return render_chart_to_rgba(model, width, height);
    }
    let lw = ((width as f32 / s).round() as u32).max(1);
    let lh = ((height as f32 / s).round() as u32).max(1);
    let svg = chart_model_to_svg(model, lw, lh)?;
    let mut pixmap = svg_to_pixmap_fit(&svg, width, height)?;
    draw_chart_border(&mut pixmap, width, height, model);
    Some(pixmap.data().to_vec())
}

/// 一站式：ChartModel → 带边框的 RGBA 像素缓冲。
pub fn render_chart_to_rgba(model: &ChartModel, width: u32, height: u32) -> Option<Vec<u8>> {
    let svg = chart_model_to_svg(model, width, height)?;
    let mut pixmap = svg_to_pixmap(&svg, width, height)?;
    draw_chart_border(&mut pixmap, width, height, model);
    Some(pixmap.data().to_vec())
}


#[cfg(test)]
mod fit_tests {
    use super::*;

    #[test]
    fn fit_scales_svg_to_target_size() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50" viewBox="0 0 100 50"><rect x="0" y="0" width="100" height="50" fill="#FF0000"/></svg>"##;
        let p = svg_to_pixmap_fit(svg, 200, 100).expect("pixmap");
        assert_eq!((p.width(), p.height()), (200, 100));
        let d = p.data();
        let i = (99 * 200 + 199) * 4; // 右下角:放大后应被填满
        assert_eq!((d[i], d[i + 3]), (255, 255));
        // 对照:identity 渲染同一 SVG,右下角是空的
        let q = svg_to_pixmap(svg, 200, 100).expect("pixmap");
        assert_eq!(q.data()[i + 3], 0);
    }
}
