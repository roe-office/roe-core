//! v223: StatChartModel -> chartEx 部件（chartExN.xml）与幻灯片里的图表框。
//! 第一版不写 colors/style/嵌入工作簿，也不写 cx:f（无工作簿）。
use roe_model::impress::Shape;
use roe_model::stat_chart::{StatChartKind, StatChartModel, StatData, StatSeries};

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
fn b2s(b: bool) -> &'static str {
    if b { "1" } else { "0" }
}

pub fn content_type_overrides(n: usize) -> String {
    let mut s = String::new();
    for i in 1..=n {
        s.push_str(&format!(
            "<Override PartName=\"/ppt/charts/chartEx{i}.xml\" ContentType=\"application/vnd.ms-office.chartex+xml\"/>"
        ));
    }
    s.push_str("</Types>");
    s
}

pub fn graphic_frame_xml(shape: &Shape, rid: &str, fallback_rid: Option<&str>) -> String {
    let x = (shape.position.x() * 12700.0).round() as i64;
    let y = (shape.position.y() * 12700.0).round() as i64;
    let w = (shape.position.w() * 12700.0).round() as i64;
    let h = (shape.position.h() * 12700.0).round() as i64;
    let id = shape.id;
    let mut s = format!(
        "<mc:AlternateContent xmlns:mc=\"http://schemas.openxmlformats.org/markup-compatibility/2006\">\
<mc:Choice xmlns:cx1=\"http://schemas.microsoft.com/office/drawing/2015/9/8/chartex\" Requires=\"cx1\">\
<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id=\"{id}\" name=\"Chart {id}\"/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr>\
<p:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{w}\" cy=\"{h}\"/></p:xfrm>\
<a:graphic><a:graphicData uri=\"http://schemas.microsoft.com/office/drawing/2014/chartex\">\
<cx:chart xmlns:cx=\"http://schemas.microsoft.com/office/drawing/2014/chartex\" r:id=\"{rid}\"/>\
</a:graphicData></a:graphic></p:graphicFrame></mc:Choice>"
    );
    if let Some(fb) = fallback_rid {
        s.push_str(&format!(
            "<mc:Fallback><p:pic><p:nvPicPr><p:cNvPr id=\"{id}\" name=\"Chart {id}\"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr>\
<p:blipFill><a:blip r:embed=\"{fb}\"/><a:stretch><a:fillRect/></a:stretch></p:blipFill>\
<p:spPr><a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{w}\" cy=\"{h}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></p:spPr></p:pic></mc:Fallback>"
        ));
    }
    s.push_str("</mc:AlternateContent>\n");
    s
}

fn data_xml(d: &StatData) -> String {
    let mut s = format!("<cx:data id=\"{}\">", d.id);
    if !d.categories.is_empty() {
        s.push_str("<cx:strDim type=\"cat\">");
        for lvl in &d.categories {
            s.push_str(&format!("<cx:lvl ptCount=\"{}\">", lvl.len()));
            for (i, t) in lvl.iter().enumerate() {
                if !t.is_empty() {
                    s.push_str(&format!("<cx:pt idx=\"{i}\">{}</cx:pt>", esc(t)));
                }
            }
            s.push_str("</cx:lvl>");
        }
        s.push_str("</cx:strDim>");
    }
    let kind = if d.value_kind.is_empty() { "val" } else { d.value_kind.as_str() };
    s.push_str(&format!("<cx:numDim type=\"{kind}\"><cx:lvl ptCount=\"{}\" formatCode=\"General\">", d.values.len()));
    for (i, v) in d.values.iter().enumerate() {
        if let Some(v) = v {
            s.push_str(&format!("<cx:pt idx=\"{i}\">{v}</cx:pt>"));
        }
    }
    s.push_str("</cx:lvl></cx:numDim></cx:data>");
    s
}

fn series_xml(i: usize, se: &StatSeries) -> String {
    let mut s = format!(
        "<cx:series layoutId=\"{}\" uniqueId=\"{{00000000-0000-4000-8000-{:012}}}\" formatIdx=\"{}\">",
        esc(&se.layout_id), i + 1, se.format_idx
    );
    s.push_str(&format!("<cx:tx><cx:txData><cx:v>{}</cx:v></cx:txData></cx:tx>", esc(&se.name)));
    if se.layout_id == "waterfall" {
        for idx in &se.subtotals {
            s.push_str(&format!("<cx:dataPt idx=\"{idx}\"/>"));
        }
    }
    if let Some(dl) = &se.data_labels {
        s.push_str(&format!(
            "<cx:dataLabels pos=\"{}\"><cx:visibility seriesName=\"{}\" categoryName=\"{}\" value=\"{}\"/></cx:dataLabels>",
            esc(&dl.pos), b2s(dl.show_series), b2s(dl.show_category), b2s(dl.show_value)
        ));
    }
    s.push_str(&format!("<cx:dataId val=\"{}\"/>", se.data_id));
    match se.layout_id.as_str() {
        "clusteredColumn" => {
            let closed = if se.interval_closed_right { "r" } else { "l" };
            if let Some(w) = se.bin_width {
                s.push_str(&format!("<cx:layoutPr><cx:binning intervalClosed=\"{closed}\"><cx:binSize val=\"{w}\"/></cx:binning></cx:layoutPr>"));
            } else if let Some(c) = se.bin_count {
                s.push_str(&format!("<cx:layoutPr><cx:binning intervalClosed=\"{closed}\"><cx:binCount val=\"{c}\"/></cx:binning></cx:layoutPr>"));
            } else {
                s.push_str(&format!("<cx:layoutPr><cx:binning intervalClosed=\"{closed}\"/></cx:layoutPr>"));
            }
        }
        "boxWhisker" => {
            s.push_str(&format!(
                "<cx:layoutPr><cx:visibility meanLine=\"{}\" meanMarker=\"{}\" nonoutliers=\"{}\" outliers=\"{}\"/><cx:statistics quartileMethod=\"{}\"/></cx:layoutPr>",
                b2s(se.mean_line), b2s(se.mean_marker), b2s(se.show_non_outliers), b2s(se.show_outliers),
                if se.quartile_exclusive { "exclusive" } else { "inclusive" }
            ));
        }
        "waterfall" if !se.subtotals.is_empty() => {
            s.push_str("<cx:layoutPr><cx:subtotals>");
            for idx in &se.subtotals {
                s.push_str(&format!("<cx:idx val=\"{idx}\"/>"));
            }
            s.push_str("</cx:subtotals></cx:layoutPr>");
        }
        "treemap" => s.push_str("<cx:layoutPr><cx:parentLabelLayout val=\"overlapping\"/></cx:layoutPr>"),
        _ => {}
    }
    s.push_str("</cx:series>");
    s
}

pub fn chartex_to_xml(m: &StatChartModel) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<cx:chartSpace xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" xmlns:cx=\"http://schemas.microsoft.com/office/drawing/2014/chartex\"><cx:chartData>",
    );
    for d in &m.data {
        s.push_str(&data_xml(d));
    }
    s.push_str("</cx:chartData><cx:chart>");
    if m.title_present {
        match m.title.as_deref().filter(|t| !t.is_empty()) {
            Some(t) => s.push_str(&format!(
                "<cx:title pos=\"t\" align=\"ctr\" overlay=\"0\"><cx:tx><cx:rich><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang=\"zh-CN\"/><a:t>{}</a:t></a:r></a:p></cx:rich></cx:tx></cx:title>",
                esc(t)
            )),
            None => s.push_str("<cx:title pos=\"t\" align=\"ctr\" overlay=\"0\"/>"),
        }
    }
    s.push_str("<cx:plotArea><cx:plotAreaRegion>");
    for (i, se) in m.series.iter().enumerate() {
        s.push_str(&series_xml(i, se));
    }
    s.push_str("</cx:plotAreaRegion>");
    let default_gap = match m.kind {
        StatChartKind::Histogram => 0.0,
        StatChartKind::BoxWhisker => 1.0,
        StatChartKind::Waterfall => 0.5,
        _ => 0.06,
    };
    let gap = m.gap_width.unwrap_or(default_gap);
    let grid = if m.show_grid { "<cx:majorGridlines/>" } else { "" };
    match m.kind {
        StatChartKind::Waterfall | StatChartKind::Histogram | StatChartKind::BoxWhisker => s.push_str(&format!(
            "<cx:axis id=\"0\"><cx:catScaling gapWidth=\"{gap}\"/><cx:tickLabels/></cx:axis><cx:axis id=\"1\"><cx:valScaling/>{grid}<cx:tickLabels/></cx:axis>"
        )),
        StatChartKind::Funnel => s.push_str(&format!(
            "<cx:axis id=\"1\"><cx:catScaling gapWidth=\"{gap}\"/><cx:tickLabels/></cx:axis>"
        )),
        _ => {}
    }
    s.push_str("</cx:plotArea>");
    if m.show_legend {
        let pos = m.legend_pos.as_deref().unwrap_or("t");
        s.push_str(&format!("<cx:legend pos=\"{}\" align=\"ctr\" overlay=\"0\"/>", esc(pos)));
    }
    s.push_str("</cx:chart></cx:chartSpace>");
    s
}
