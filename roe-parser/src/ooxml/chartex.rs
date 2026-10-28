//! chartEx(Office 2016+ 统计图表)解析:直方图/箱线图/瀑布/漏斗/旭日/树状。
//! 不依赖 XML 库,只做本格式需要的最小扫描。

use roe_model::stat_chart::*;

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// 取出所有 `<tag ...>...</tag>`(或自闭合)块。标签名后必须是空白、'>' 或 '/'。
fn blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(rel) = xml[pos..].find(&open) {
        let s = pos + rel;
        let after = s + open.len();
        let ok = matches!(xml[after..].chars().next(), Some(' ') | Some('>') | Some('/') | Some('\n') | Some('\t') | Some('\r'));
        if !ok {
            pos = after;
            continue;
        }
        let gt = match xml[s..].find('>') {
            Some(g) => s + g,
            None => break,
        };
        if xml[..gt].ends_with('/') {
            out.push(&xml[s..=gt]);
            pos = gt + 1;
            continue;
        }
        match xml[gt..].find(&close) {
            Some(c) => {
                let e = gt + c + close.len();
                out.push(&xml[s..e]);
                pos = e;
            }
            None => break,
        }
    }
    out
}

fn first_block<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    blocks(xml, tag).into_iter().next()
}

/// 块的开始标签(含属性)。
fn head(block: &str) -> &str {
    match block.find('>') {
        Some(g) => &block[..=g],
        None => block,
    }
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!(" {}=\"", name);
    let s = tag.find(&needle)? + needle.len();
    let e = tag[s..].find('"')?;
    Some(unescape(&tag[s..s + e]))
}

/// 块的内部文本。
fn inner(block: &str) -> &str {
    let s = match block.find('>') {
        Some(g) => g + 1,
        None => return "",
    };
    if block[..s].ends_with("/>") {
        return "";
    }
    let e = block.rfind("</").unwrap_or(block.len());
    if e < s { "" } else { &block[s..e] }
}

fn read_lvl(lvl: &str) -> Vec<String> {
    let n = attr(head(lvl), "ptCount").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    let mut out = vec![String::new(); n.min(1_000_000)];
    for pt in blocks(lvl, "cx:pt") {
        if let Some(i) = attr(head(pt), "idx").and_then(|v| v.parse::<usize>().ok()) {
            if i >= 1_000_000 {
                continue;
            }
            if i >= out.len() {
                out.resize(i + 1, String::new());
            }
            out[i] = unescape(inner(pt).trim());
        }
    }
    out
}

fn parse_data(block: &str) -> StatData {
    let id = attr(head(block), "id").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut categories = Vec::new();
    for sd in blocks(block, "cx:strDim") {
        for lvl in blocks(sd, "cx:lvl") {
            categories.push(read_lvl(lvl));
        }
    }
    let mut values = Vec::new();
    let mut value_kind = String::new();
    if let Some(nd) = first_block(block, "cx:numDim") {
        value_kind = attr(head(nd), "type").unwrap_or_default();
        if let Some(lvl) = first_block(nd, "cx:lvl") {
            values = read_lvl(lvl).iter().map(|s| s.trim().parse::<f64>().ok()).collect();
        }
    }
    StatData { id, categories, values, value_kind }
}

fn flag(tag: Option<&str>, name: &str, default: bool) -> bool {
    tag.and_then(|t| attr(head(t), name))
        .map(|s| s == "1" || s == "true")
        .unwrap_or(default)
}

fn parse_series(s: &str) -> StatSeries {
    let h = head(s);
    let layout_id = attr(h, "layoutId").unwrap_or_default();
    let format_idx = attr(h, "formatIdx").and_then(|v| v.parse().ok()).unwrap_or(0);
    let name = first_block(s, "cx:tx")
        .and_then(|tx| first_block(tx, "cx:v"))
        .map(|v| unescape(inner(v).trim()))
        .unwrap_or_default();
    let data_id = first_block(s, "cx:dataId")
        .and_then(|t| attr(head(t), "val"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let lp = first_block(s, "cx:layoutPr").unwrap_or("");

    let (interval_closed_right, bin_width, bin_count) = match first_block(lp, "cx:binning") {
        Some(b) => {
            let right = attr(head(b), "intervalClosed").map(|v| v != "l").unwrap_or(true);
            let w = first_block(b, "cx:binSize")
                .and_then(|t| attr(head(t), "val"))
                .and_then(|v| v.parse::<f64>().ok());
            let c = first_block(b, "cx:binCount")
                .and_then(|t| attr(head(t), "val"))
                .and_then(|v| v.parse::<usize>().ok());
            (right, w, c)
        }
        None => (true, None, None),
    };

    let quartile_exclusive = first_block(lp, "cx:statistics")
        .and_then(|t| attr(head(t), "quartileMethod"))
        .map(|v| v != "inclusive")
        .unwrap_or(true);

    let vis = first_block(lp, "cx:visibility");
    let subtotals: Vec<usize> = first_block(lp, "cx:subtotals")
        .map(|st| {
            blocks(st, "cx:idx")
                .into_iter()
                .filter_map(|t| attr(head(t), "val").and_then(|v| v.parse::<usize>().ok()))
                .collect()
        })
        .unwrap_or_default();

    let data_labels = first_block(s, "cx:dataLabels").map(|dl| {
        let v = first_block(dl, "cx:visibility");
        StatDataLabels {
            pos: attr(head(dl), "pos").unwrap_or_default(),
            show_value: flag(v, "value", false),
            show_category: flag(v, "categoryName", false),
            show_series: flag(v, "seriesName", false),
        }
    });

    StatSeries {
        name,
        layout_id,
        format_idx,
        data_id,
        bin_width,
        bin_count,
        interval_closed_right,
        quartile_exclusive,
        mean_line: flag(vis, "meanLine", false),
        mean_marker: flag(vis, "meanMarker", true),
        show_outliers: flag(vis, "outliers", true),
        show_non_outliers: flag(vis, "nonoutliers", false),
        subtotals,
        data_labels,
    }
}

/// 解析 chartEx XML。不认识的图表种类(地图等)返回 None。
pub fn parse_chartex_xml(xml: &str) -> Option<StatChartModel> {
    let data: Vec<StatData> = blocks(xml, "cx:data").into_iter().map(parse_data).collect();
    let series: Vec<StatSeries> = blocks(xml, "cx:series").into_iter().map(parse_series).collect();
    let kind = {
        let first = series.first()?;
        match first.layout_id.as_str() {
            "clusteredColumn" => StatChartKind::Histogram,
            "boxWhisker" => StatChartKind::BoxWhisker,
            "waterfall" => StatChartKind::Waterfall,
            "funnel" => StatChartKind::Funnel,
            "sunburst" => StatChartKind::Sunburst,
            "treemap" => StatChartKind::Treemap,
            _ => return None,
        }
    };

    let chart = first_block(xml, "cx:chart").unwrap_or("");
    let pre = &chart[..chart.find("<cx:plotArea").unwrap_or(chart.len())];
    let title_blk = first_block(pre, "cx:title");
    let title_present = title_blk.is_some();
    let title = title_blk
        .map(|t| blocks(t, "a:t").iter().map(|x| unescape(inner(x))).collect::<String>())
        .filter(|s| !s.is_empty());
    let legend = first_block(chart, "cx:legend");
    let gap_width = first_block(xml, "cx:catScaling")
        .and_then(|t| attr(head(t), "gapWidth"))
        .and_then(|v| v.parse::<f64>().ok());

    Some(StatChartModel {
        kind,
        title_present,
        title,
        show_legend: legend.is_some(),
        legend_pos: legend.and_then(|l| attr(head(l), "pos")),
        show_grid: chart.contains("<cx:majorGridlines"),
        gap_width,
        data,
        series,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = r##"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:cx="http://schemas.microsoft.com/office/drawing/2014/chartex""##;

    fn wrap(data: &str, chart: &str) -> String {
        format!(r##"<?xml version="1.0"?><cx:chartSpace {NS}><cx:chartData><cx:externalData r:id="rId1"/>{data}</cx:chartData><cx:chart>{chart}</cx:chart></cx:chartSpace>"##)
    }

    #[test]
    fn histogram_basic() {
        let data = r##"<cx:data id="0"><cx:numDim type="val"><cx:f>Sheet1!$A$2:$A$6</cx:f><cx:lvl ptCount="5" formatCode="G"><cx:pt idx="0">1</cx:pt><cx:pt idx="1">3</cx:pt><cx:pt idx="2">3</cx:pt><cx:pt idx="3">5</cx:pt><cx:pt idx="4">24</cx:pt></cx:lvl></cx:numDim></cx:data>"##;
        let chart = r##"<cx:title pos="t" align="ctr" overlay="0"/><cx:plotArea><cx:plotAreaRegion><cx:series layoutId="clusteredColumn" uniqueId="{x}" formatIdx="0"><cx:tx><cx:txData><cx:f>Sheet1!$A$1</cx:f><cx:v>系列 1</cx:v></cx:txData></cx:tx><cx:dataId val="0"/><cx:layoutPr><cx:binning intervalClosed="r"><cx:binSize val="4"/></cx:binning></cx:layoutPr></cx:series></cx:plotAreaRegion><cx:axis id="0"><cx:catScaling gapWidth="0"/><cx:tickLabels/></cx:axis><cx:axis id="1"><cx:valScaling/><cx:majorGridlines/><cx:tickLabels/></cx:axis></cx:plotArea>"##;
        let m = parse_chartex_xml(&wrap(data, chart)).unwrap();
        assert_eq!(m.kind, StatChartKind::Histogram);
        assert!(m.title_present && m.title.is_none());
        assert!(!m.show_legend);
        assert!(m.show_grid);
        assert_eq!(m.gap_width, Some(0.0));
        assert_eq!(m.data.len(), 1);
        assert_eq!(m.data[0].values, vec![Some(1.0), Some(3.0), Some(3.0), Some(5.0), Some(24.0)]);
        let s = &m.series[0];
        assert_eq!(s.name, "系列 1");
        assert!(s.interval_closed_right);
        assert_eq!(s.bin_width, Some(4.0));
        assert_eq!(s.bin_count, None);
    }

    #[test]
    fn histogram_auto_binning_has_no_width() {
        let chart = r##"<cx:plotArea><cx:plotAreaRegion><cx:series layoutId="clusteredColumn" formatIdx="0"><cx:dataId val="0"/><cx:layoutPr><cx:binning intervalClosed="r"></cx:binning></cx:layoutPr></cx:series></cx:plotAreaRegion></cx:plotArea>"##;
        let m = parse_chartex_xml(&wrap("", chart)).unwrap();
        assert_eq!(m.series[0].bin_width, None);
        assert_eq!(m.series[0].bin_count, None);
        assert!(!m.title_present);
    }

    #[test]
    fn box_whisker_two_series() {
        let one = |id: u32, col: &str| {
            format!(r##"<cx:data id="{id}"><cx:strDim type="cat"><cx:f>Sheet1!$A$2:$A$4</cx:f><cx:lvl ptCount="3"><cx:pt idx="0">类别1</cx:pt><cx:pt idx="1">类别1</cx:pt><cx:pt idx="2">类别2</cx:pt></cx:lvl></cx:strDim><cx:numDim type="val"><cx:f>Sheet1!${col}$2:${col}$4</cx:f><cx:lvl ptCount="3"><cx:pt idx="0">-7</cx:pt><cx:pt idx="2">47</cx:pt></cx:lvl></cx:numDim></cx:data>"##)
        };
        let ser = |id: u32| {
            format!(r##"<cx:series layoutId="boxWhisker" formatIdx="{id}"><cx:dataId val="{id}"/><cx:layoutPr><cx:visibility meanLine="0" meanMarker="1" nonoutliers="0" outliers="1"/><cx:statistics quartileMethod="exclusive"/></cx:layoutPr></cx:series>"##)
        };
        let data = format!("{}{}", one(0, "B"), one(1, "C"));
        let chart = format!(r##"<cx:plotArea><cx:plotAreaRegion>{}{}</cx:plotAreaRegion><cx:axis id="0"><cx:catScaling gapWidth="1"/></cx:axis></cx:plotArea>"##, ser(0), ser(1));
        let m = parse_chartex_xml(&wrap(&data, &chart)).unwrap();
        assert_eq!(m.kind, StatChartKind::BoxWhisker);
        assert_eq!(m.series.len(), 2);
        assert_eq!(m.series[1].data_id, 1);
        assert!(m.series[0].quartile_exclusive);
        assert!(m.series[0].mean_marker && !m.series[0].mean_line);
        assert!(m.series[0].show_outliers && !m.series[0].show_non_outliers);
        assert_eq!(m.data[0].categories.len(), 1);
        assert_eq!(m.data[0].categories[0], vec!["类别1", "类别1", "类别2"]);
        assert_eq!(m.data[0].values, vec![Some(-7.0), None, Some(47.0)]);
        assert_eq!(m.gap_width, Some(1.0));
    }

    #[test]
    fn box_whisker_inclusive() {
        let chart = r##"<cx:plotArea><cx:plotAreaRegion><cx:series layoutId="boxWhisker" formatIdx="0"><cx:dataId val="0"/><cx:layoutPr><cx:statistics quartileMethod="inclusive"/></cx:layoutPr></cx:series></cx:plotAreaRegion></cx:plotArea>"##;
        let m = parse_chartex_xml(&wrap("", chart)).unwrap();
        assert!(!m.series[0].quartile_exclusive);
    }

    #[test]
    fn waterfall_subtotals_labels_legend() {
        let chart = r##"<cx:title pos="t" align="ctr" overlay="0"/><cx:plotArea><cx:plotAreaRegion><cx:series layoutId="waterfall" formatIdx="0"><cx:dataPt idx="4"/><cx:dataPt idx="7"/><cx:dataLabels pos="outEnd"><cx:visibility seriesName="0" categoryName="0" value="1"/></cx:dataLabels><cx:dataId val="0"/><cx:layoutPr><cx:subtotals><cx:idx val="7"/><cx:idx val="4"/></cx:subtotals></cx:layoutPr></cx:series></cx:plotAreaRegion><cx:axis id="0"><cx:catScaling gapWidth="0.5"/></cx:axis></cx:plotArea><cx:legend pos="t" align="ctr" overlay="0"/>"##;
        let m = parse_chartex_xml(&wrap("", chart)).unwrap();
        assert_eq!(m.kind, StatChartKind::Waterfall);
        assert_eq!(m.series[0].subtotals, vec![7, 4]);
        let dl = m.series[0].data_labels.as_ref().unwrap();
        assert_eq!(dl.pos, "outEnd");
        assert!(dl.show_value && !dl.show_category && !dl.show_series);
        assert!(m.show_legend);
        assert_eq!(m.legend_pos.as_deref(), Some("t"));
        assert_eq!(m.gap_width, Some(0.5));
    }

    #[test]
    fn sunburst_multi_level() {
        let data = r##"<cx:data id="0"><cx:strDim type="cat"><cx:f>Sheet1!$A$2:$C$3</cx:f><cx:lvl ptCount="2"><cx:pt idx="0">叶子 1</cx:pt><cx:pt idx="1">叶子 2</cx:pt></cx:lvl><cx:lvl ptCount="2"><cx:pt idx="0">茎 1</cx:pt><cx:pt idx="1">茎 1</cx:pt></cx:lvl><cx:lvl ptCount="2"><cx:pt idx="0">分支 1</cx:pt><cx:pt idx="1">分支 1</cx:pt></cx:lvl></cx:strDim><cx:numDim type="size"><cx:f>Sheet1!$D$2:$D$3</cx:f><cx:lvl ptCount="2"><cx:pt idx="0">17</cx:pt><cx:pt idx="1">8</cx:pt></cx:lvl></cx:numDim></cx:data>"##;
        let chart = r##"<cx:plotArea><cx:plotAreaRegion><cx:series layoutId="sunburst" formatIdx="0"><cx:dataId val="0"/></cx:series></cx:plotAreaRegion></cx:plotArea>"##;
        let m = parse_chartex_xml(&wrap(data, chart)).unwrap();
        assert_eq!(m.kind, StatChartKind::Sunburst);
        assert_eq!(m.data[0].categories.len(), 3);
        assert_eq!(m.data[0].value_kind, "size");
        assert_eq!(m.data[0].values, vec![Some(17.0), Some(8.0)]);
    }

    #[test]
    fn escaped_text_and_unknown_kind() {
        let data = r##"<cx:data id="0"><cx:strDim type="cat"><cx:lvl ptCount="1"><cx:pt idx="0">A&amp;B</cx:pt></cx:lvl></cx:strDim></cx:data>"##;
        let chart = r##"<cx:plotArea><cx:plotAreaRegion><cx:series layoutId="funnel" formatIdx="0"><cx:dataId val="0"/></cx:series></cx:plotAreaRegion></cx:plotArea>"##;
        let m = parse_chartex_xml(&wrap(data, chart)).unwrap();
        assert_eq!(m.data[0].categories[0][0], "A&B");
        let bad = r##"<cx:plotArea><cx:plotAreaRegion><cx:series layoutId="regionMap" formatIdx="0"/></cx:plotAreaRegion></cx:plotArea>"##;
        assert!(parse_chartex_xml(&wrap("", bad)).is_none());
        assert!(parse_chartex_xml("<not-xml/>").is_none());
    }
}
