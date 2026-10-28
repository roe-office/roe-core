//! v222: ChartModel -> DrawingML 图表部件（chartN.xml）与幻灯片里的图表框
//! 不嵌入工作簿，只写缓存（numCache/strCache），PowerPoint/WPS 可显示。
use roe_model::common::Color;
use roe_model::impress::{ChartGrouping, ChartKind, ChartModel, ChartSeries, Shape};

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
fn hex(c: &Color) -> String {
    format!("{:02X}{:02X}{:02X}", c.r, c.g, c.b)
}
fn col(i: usize) -> char {
    (b'B' + (i.min(24) as u8)) as char
}

/// 图表数据引用方式。
/// Sheet1:写 <c:f>Sheet1!...(历史行为,pptx/docx 沿用);
/// Literal:写 strLit/numLit 字面量,不带 <c:f>(xlsx 用,避免引用到错误的 sheet)。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Refs {
    Sheet1,
    Literal,
}

pub fn content_type_overrides(n: usize) -> String {
    let mut s = String::new();
    for i in 1..=n {
        s.push_str(&format!(
            "<Override PartName=\"/ppt/charts/chart{i}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.drawingml.chart+xml\"/>"
        ));
    }
    s.push_str("</Types>");
    s
}

pub fn graphic_frame_xml(shape: &Shape, rid: &str) -> String {
    let x = (shape.position.x() * 12700.0).round() as i64;
    let y = (shape.position.y() * 12700.0).round() as i64;
    let w = (shape.position.w() * 12700.0).round() as i64;
    let h = (shape.position.h() * 12700.0).round() as i64;
    format!(
        "<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id=\"{id}\" name=\"Chart {id}\"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr>\
<p:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{w}\" cy=\"{h}\"/></p:xfrm>\
<a:graphic><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/chart\">\
<c:chart xmlns:c=\"http://schemas.openxmlformats.org/drawingml/2006/chart\" r:id=\"{rid}\"/>\
</a:graphicData></a:graphic></p:graphicFrame>\n",
        id = shape.id
    )
}

fn labels(m: &ChartModel) -> Vec<String> {
    let n = m.series.iter().map(|s| s.data.len()).max().unwrap_or(0).max(m.x_labels.len());
    (0..n)
        .map(|i| m.x_labels.get(i).cloned().unwrap_or_else(|| (i + 1).to_string()))
        .collect()
}

fn tx_xml(i: usize, s: &ChartSeries, r: Refs) -> String {
    if r == Refs::Literal {
        return format!("<c:tx><c:v>{}</c:v></c:tx>", esc(&s.name));
    }
    format!(
        "<c:tx><c:strRef><c:f>Sheet1!${c}$1</c:f><c:strCache><c:ptCount val=\"1\"/><c:pt idx=\"0\"><c:v>{n}</c:v></c:pt></c:strCache></c:strRef></c:tx>",
        c = col(i),
        n = esc(&s.name)
    )
}

fn cat_xml(labs: &[String], r: Refs) -> String {
    let n = labs.len();
    let mut pts = String::new();
    for (i, l) in labs.iter().enumerate() {
        pts.push_str(&format!("<c:pt idx=\"{i}\"><c:v>{}</c:v></c:pt>", esc(l)));
    }
    if r == Refs::Literal {
        return format!("<c:cat><c:strLit><c:ptCount val=\"{n}\"/>{pts}</c:strLit></c:cat>");
    }
    format!(
        "<c:cat><c:strRef><c:f>Sheet1!$A$2:$A${last}</c:f><c:strCache><c:ptCount val=\"{n}\"/>{pts}</c:strCache></c:strRef></c:cat>",
        last = n + 1
    )
}

fn num_xml(tag: &str, f: Option<&str>, vals: &[f64]) -> String {
    let n = vals.len();
    let mut pts = String::new();
    for (i, v) in vals.iter().enumerate() {
        let v = if v.is_finite() { *v } else { 0.0 };
        pts.push_str(&format!("<c:pt idx=\"{i}\"><c:v>{v}</c:v></c:pt>"));
    }
    let Some(f) = f else {
        return format!(
            "<c:{tag}><c:numLit><c:formatCode>General</c:formatCode><c:ptCount val=\"{n}\"/>{pts}</c:numLit></c:{tag}>"
        );
    };
    format!(
        "<c:{tag}><c:numRef><c:f>{f}</c:f><c:numCache><c:formatCode>General</c:formatCode><c:ptCount val=\"{n}\"/>{pts}</c:numCache></c:numRef></c:{tag}>"
    )
}

fn val_f(i: usize, n: usize, r: Refs) -> Option<String> {
    if r == Refs::Literal {
        return None;
    }
    Some(format!("Sheet1!${c}$2:${c}${last}", c = col(i), last = n + 1))
}

fn fill_sppr(c: &Option<Color>, line: bool) -> String {
    match c {
        None => String::new(),
        Some(c) => {
            if line {
                format!("<c:spPr><a:ln w=\"28575\" cap=\"rnd\"><a:solidFill><a:srgbClr val=\"{}\"/></a:solidFill><a:round/></a:ln></c:spPr>", hex(c))
            } else {
                format!("<c:spPr><a:solidFill><a:srgbClr val=\"{}\"/></a:solidFill></c:spPr>", hex(c))
            }
        }
    }
}

/// v318h: 系列没有显式颜色时的默认配色（Office 经典色），
/// 避免 LibreOffice 对无 spPr 的系列不画填充。
fn default_series_color(idx: usize) -> Color {
    const P: [(u8, u8, u8); 6] = [
        (0x5B, 0x9B, 0xD5),
        (0xED, 0x7D, 0x31),
        (0xA5, 0xA5, 0xA5),
        (0xFF, 0xC0, 0x00),
        (0x44, 0x72, 0xC4),
        (0x70, 0xAD, 0x47),
    ];
    let (r, g, b) = P[idx % P.len()];
    Color { r, g, b, a: 255 }
}

fn ser_xml(tag: &str, idx: usize, s: &ChartSeries, labs: &[String], r: Refs) -> String {
    let mut x = format!("<c:ser><c:idx val=\"{idx}\"/><c:order val=\"{idx}\"/>{}", tx_xml(idx, s, r));
    let eff: Option<Color> = s.color.clone().or_else(|| {
        if matches!(tag, "bar" | "line" | "radar" | "scatter" | "area") {
            Some(default_series_color(idx))
        } else {
            None
        }
    });
    let n = s.data.len();
    match tag {
        "scatter" => {
            x.push_str("<c:spPr><a:ln w=\"19050\"><a:noFill/></a:ln></c:spPr>");
            x.push_str(&format!(
                "<c:marker><c:symbol val=\"circle\"/><c:size val=\"7\"/>{}</c:marker>",
                fill_sppr(&eff, false)
            ));
            let xs: Vec<f64> = (0..n)
                .map(|i| labs.get(i).and_then(|l| l.trim().parse::<f64>().ok()).unwrap_or((i + 1) as f64))
                .collect();
            let xf = (r == Refs::Sheet1).then(|| format!("Sheet1!$A$2:$A${}", n + 1));
            x.push_str(&num_xml("xVal", xf.as_deref(), &xs));
            x.push_str(&num_xml("yVal", val_f(idx, n, r).as_deref(), &s.data));
            x.push_str("<c:smooth val=\"0\"/>");
        }
        _ => {
            x.push_str(&fill_sppr(&eff, tag == "line" || tag == "radar"));
            // v318h: 饼图、圆环图按扇区写 c:dPt（顺序必须在 c:cat 之前）
            if tag == "pie" {
                for i in 0..n {
                    let c = default_series_color(i);
                    x.push_str(&format!(
                        "<c:dPt><c:idx val=\"{i}\"/><c:bubble3D val=\"0\"/><c:spPr><a:solidFill><a:srgbClr val=\"{}\"/></a:solidFill></c:spPr></c:dPt>",
                        hex(&c)
                    ));
                }
            }
            if tag == "bar" {
                x.push_str("<c:invertIfNegative val=\"0\"/>");
            }
            if tag == "line" {
                x.push_str("<c:marker><c:symbol val=\"circle\"/><c:size val=\"5\"/></c:marker>");
            }
            x.push_str(&cat_xml(labs, r));
            x.push_str(&num_xml("val", val_f(idx, n, r).as_deref(), &s.data));
            if tag == "line" {
                x.push_str("<c:smooth val=\"0\"/>");
            }
        }
    }
    x.push_str("</c:ser>");
    x
}

fn sers(tag: &str, items: &[(usize, &ChartSeries)], labs: &[String], r: Refs) -> String {
    items.iter().map(|(i, s)| ser_xml(tag, *i, s, labs, r)).collect::<String>()
}

fn axis_title(t: &str) -> String {
    if t.trim().is_empty() {
        return String::new();
    }
    format!(
        "<c:title><c:tx><c:rich><a:bodyPr/><a:p><a:r><a:t>{}</a:t></a:r></a:p></c:rich></c:tx><c:overlay val=\"0\"/></c:title>",
        esc(t)
    )
}

const AX1: &str = "500000001";
const AX2: &str = "500000002";

fn cat_ax(pos: &str, title: &str) -> String {
    format!(
        "<c:catAx><c:axId val=\"{AX1}\"/><c:scaling><c:orientation val=\"minMax\"/></c:scaling><c:delete val=\"0\"/><c:axPos val=\"{pos}\"/>{t}<c:majorTickMark val=\"out\"/><c:minorTickMark val=\"none\"/><c:tickLblPos val=\"nextTo\"/><c:crossAx val=\"{AX2}\"/><c:crosses val=\"autoZero\"/><c:auto val=\"1\"/><c:lblAlgn val=\"ctr\"/><c:lblOffset val=\"100\"/><c:noMultiLvlLbl val=\"0\"/></c:catAx>",
        t = axis_title(title)
    )
}

fn val_ax(id: &str, cross: &str, pos: &str, title: &str, grid: bool, pct: bool) -> String {
    format!(
        "<c:valAx><c:axId val=\"{id}\"/><c:scaling><c:orientation val=\"minMax\"/></c:scaling><c:delete val=\"0\"/><c:axPos val=\"{pos}\"/>{g}{t}{nf}<c:majorTickMark val=\"out\"/><c:minorTickMark val=\"none\"/><c:tickLblPos val=\"nextTo\"/><c:crossAx val=\"{cross}\"/><c:crosses val=\"autoZero\"/><c:crossBetween val=\"between\"/></c:valAx>",
        g = if grid { "<c:majorGridlines/>" } else { "" },
        t = axis_title(title),
        nf = if pct { "<c:numFmt formatCode=\"0%\" sourceLinked=\"0\"/>" } else { "" }
    )
}

/// pptx/docx 使用:数据引用写 Sheet1!(只写缓存,无工作簿可引用,历史行为)。
pub fn chart_to_xml(m: &ChartModel) -> String {
    chart_to_xml_impl(m, Refs::Sheet1)
}

/// xlsx 使用:数据写成 strLit/numLit 字面量,不写 <c:f>。
/// TODO(技术债):
/// 1. xlsx 里的图表目前不绑定单元格,改单元格后图表不会联动(数据本来就是
///    insert_chart 传入的字面量)。二期方案:给 ChartModel 增加真实数据范围
///    (sheet + 区域),写 <c:f> 时用真实引用(sheet 名需用单引号包裹并转义 ')。
/// 2. WPS / OnlyOffice 对 numLit/strLit 与 <c:tx><c:v> 的兼容性尚未真机验证,
///    若显示异常再回头评估。
pub fn chart_to_xml_literal(m: &ChartModel) -> String {
    chart_to_xml_impl(m, Refs::Literal)
}

fn chart_to_xml_impl(m: &ChartModel, r: Refs) -> String {
    let labs = labels(m);
    let all: Vec<(usize, &ChartSeries)> = m.series.iter().enumerate().collect();
    let dl = if m.data_labels.show_val {
        "<c:dLbls><c:showLegendKey val=\"0\"/><c:showVal val=\"1\"/><c:showCatName val=\"0\"/><c:showSerName val=\"0\"/><c:showPercent val=\"0\"/><c:showBubbleSize val=\"0\"/></c:dLbls>"
    } else {
        ""
    };
    let ids = format!("<c:axId val=\"{AX1}\"/><c:axId val=\"{AX2}\"/>");
    let pct = m.grouping == ChartGrouping::PercentStacked;
    let stacked = m.grouping != ChartGrouping::Standard;
    let g_bar = match m.grouping {
        ChartGrouping::Standard => "clustered",
        ChartGrouping::Stacked => "stacked",
        ChartGrouping::PercentStacked => "percentStacked",
    };
    let g_line = match m.grouping {
        ChartGrouping::Standard => "standard",
        ChartGrouping::Stacked => "stacked",
        ChartGrouping::PercentStacked => "percentStacked",
    };

    let mut plot = String::from("<c:layout/>");
    let mut axes = String::new();
    let (xl, yl) = (m.x_axis_label.as_str(), m.y_axis_label.as_str());

    match &m.kind {
        ChartKind::Bar | ChartKind::HorizontalBar => {
            let hbar = matches!(m.kind, ChartKind::HorizontalBar);
            let (lines, bars): (Vec<_>, Vec<_>) = if hbar {
                (Vec::new(), all.clone())
            } else {
                all.iter().copied().partition(|&(_, s)| s.as_line)
            };
            let gap = m.gap_width_pct.unwrap_or(150.0).round() as i32;
            let overlap = if stacked { 100 } else { m.overlap_pct.unwrap_or(0.0).round() as i32 };
            plot.push_str(&format!(
                "<c:barChart><c:barDir val=\"{}\"/><c:grouping val=\"{g_bar}\"/><c:varyColors val=\"0\"/>{}{dl}<c:gapWidth val=\"{gap}\"/><c:overlap val=\"{overlap}\"/>{ids}</c:barChart>",
                if hbar { "bar" } else { "col" },
                sers("bar", &bars, &labs, r)
            ));
            if !lines.is_empty() {
                plot.push_str(&format!(
                    "<c:lineChart><c:grouping val=\"standard\"/><c:varyColors val=\"0\"/>{}<c:marker val=\"1\"/>{ids}</c:lineChart>",
                    sers("line", &lines, &labs, r)
                ));
            }
            if hbar {
                axes.push_str(&cat_ax("l", xl));
                axes.push_str(&val_ax(AX2, AX1, "b", yl, m.show_grid, pct));
            } else {
                axes.push_str(&cat_ax("b", xl));
                axes.push_str(&val_ax(AX2, AX1, "l", yl, m.show_grid, pct));
            }
        }
        ChartKind::Line => {
            plot.push_str(&format!(
                "<c:lineChart><c:grouping val=\"{g_line}\"/><c:varyColors val=\"0\"/>{}{dl}<c:marker val=\"1\"/>{ids}</c:lineChart>",
                sers("line", &all, &labs, r)
            ));
            axes.push_str(&cat_ax("b", xl));
            axes.push_str(&val_ax(AX2, AX1, "l", yl, m.show_grid, pct));
        }
        ChartKind::Area => {
            plot.push_str(&format!(
                "<c:areaChart><c:grouping val=\"{g_line}\"/><c:varyColors val=\"0\"/>{}{dl}{ids}</c:areaChart>",
                sers("area", &all, &labs, r)
            ));
            axes.push_str(&cat_ax("b", xl));
            axes.push_str(&val_ax(AX2, AX1, "l", yl, m.show_grid, pct));
        }
        ChartKind::Pie => {
            let first: Vec<_> = all.iter().copied().take(1).collect();
            plot.push_str(&format!(
                "<c:pieChart><c:varyColors val=\"1\"/>{}{dl}<c:firstSliceAng val=\"0\"/></c:pieChart>",
                sers("pie", &first, &labs, r)
            ));
        }
        ChartKind::Doughnut => {
            let first: Vec<_> = all.clone(); // 环形图支持多环,写出全部系列
            let hole = m.hole_size_pct.unwrap_or(50.0).round().clamp(10.0, 90.0) as i32;
            plot.push_str(&format!(
                "<c:doughnutChart><c:varyColors val=\"1\"/>{}{dl}<c:firstSliceAng val=\"0\"/><c:holeSize val=\"{hole}\"/></c:doughnutChart>",
                sers("pie", &first, &labs, r)
            ));
        }
        ChartKind::Scatter => {
            plot.push_str(&format!(
                "<c:scatterChart><c:scatterStyle val=\"lineMarker\"/><c:varyColors val=\"0\"/>{}{dl}{ids}</c:scatterChart>",
                sers("scatter", &all, &labs, r)
            ));
            axes.push_str(&val_ax(AX1, AX2, "b", xl, false, false));
            axes.push_str(&val_ax(AX2, AX1, "l", yl, m.show_grid, false));
        }
        ChartKind::Radar => {
            plot.push_str(&format!(
                "<c:radarChart><c:radarStyle val=\"marker\"/><c:varyColors val=\"0\"/>{}{dl}{ids}</c:radarChart>",
                sers("radar", &all, &labs, r)
            ));
            axes.push_str(&cat_ax("b", ""));
            axes.push_str(&val_ax(AX2, AX1, "l", "", true, false));
        }
    }
    plot.push_str(&axes);

    let title = match &m.title {
        Some(t) if !t.trim().is_empty() => format!(
            "<c:title><c:tx><c:rich><a:bodyPr/><a:p><a:r><a:t>{}</a:t></a:r></a:p></c:rich></c:tx><c:overlay val=\"0\"/></c:title><c:autoTitleDeleted val=\"0\"/>",
            esc(t)
        ),
        _ => "<c:autoTitleDeleted val=\"1\"/>".to_string(),
    };
    let legend = if m.show_legend {
        let pos = if matches!(m.kind, ChartKind::Pie | ChartKind::Doughnut) { "r" } else { "b" };
        format!("<c:legend><c:legendPos val=\"{pos}\"/><c:overlay val=\"0\"/></c:legend>")
    } else {
        String::new()
    };
    let border = format!(
        "<c:spPr><a:solidFill><a:srgbClr val=\"FFFFFF\"/></a:solidFill><a:ln w=\"{}\"><a:solidFill><a:srgbClr val=\"{}\"/></a:solidFill></a:ln></c:spPr>",
        (m.border_width.max(0.0) * 12700.0).round() as i64,
        hex(&m.border_color)
    );

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<c:chartSpace xmlns:c=\"http://schemas.openxmlformats.org/drawingml/2006/chart\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
<c:roundedCorners val=\"0\"/><c:chart>{title}<c:plotArea>{plot}</c:plotArea>{legend}<c:plotVisOnly val=\"1\"/><c:dispBlanksAs val=\"gap\"/></c:chart>{border}</c:chartSpace>"
    )
}
