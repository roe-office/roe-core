// OOXML chart 解析统一实现（ROE_Chart_Parser_Unify_设计文档_v1）。
//
// 背景：xlsx.rs/pptx.rs/docx.rs 三处曾各自维护一份几乎相同的
// parse_pptx_chart_xml + 主题色解析函数，本文件是合并后的唯一实现。
// 三种宿主格式（xlsx/pptx/docx）通过各自的 relationship 指向同一种
// OOXML chart part（chartN.xml），解析这个 part 的代码理应独立于任何
// 一个宿主格式模块，故设为独立文件而非塞进某一个宿主文件里。
//
// theme_colors 类型统一为 HashMap<String, Color>（原 xlsx.rs 用
// [u8;3]，pptx.rs/docx.rs 用 Color，现以 Color 为准）。
//
// sysClr 分支采用 xlsx.rs 原有的 ROOT-069/v130 修复版本（val 属性
// 优先，lastClr 仅在 val 是未知关键字时兜底），修正 pptx.rs 原有的
// "无条件用 lastClr"落后逻辑（该逻辑与 OnlyOffice/WPS/LibreOffice
// 三方参考渲染不一致）。docx.rs 因为原本就是调用 pptx.rs 的
// parse_theme_colors，此次一并修复。

use std::collections::HashMap;
use quick_xml::events::Event;
use quick_xml::Reader;
use roe_model::common::Color;
use roe_model::impress::{ChartModel, ChartKind, ChartSeries};

// ─── 小工具（原 pptx.rs 6406/6416/6839 行迁移） ────────────────────────────

pub(crate) fn attr_str(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        if a.key.local_name().as_ref() == key {
            a.unescape_value().ok().map(|v| v.to_string())
        } else {
            None
        }
    })
}

pub(crate) fn attr_f32(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<f32> {
    attr_str(e, key)?.parse().ok()
}

/// clrMap 槽位归一化：把 <c:ser><c:spPr><a:schemeClr val="bg1"/> 里的
/// "逻辑槽位名"（bg1/tx1/bg2/tx2/accent1-6/hlink/folHlink）转换成
/// theme_colors 表里实际存储用的"物理槽位名"（dk1/lt1/dk2/lt2/...）。
/// clr_map 来自宿主 slideMaster/slideLayout 的 <p:clrMap>（pptx 特有
/// 概念），xlsx 场景没有这个概念，调用时传空表，此函数在 miss 时走
/// 下方 4 条硬编码默认对应关系兜底，不会 panic、不会返回 None。
pub(crate) fn normalize_scheme_color_slot<'a>(slot: &'a str, clr_map: &HashMap<String, String>) -> &'a str {
    match clr_map.get(slot) {
        Some(mapped) => default_clr_map_target(mapped),
        None => match slot {
            "bg1" => "lt1",
            "tx1" => "dk1",
            "bg2" => "lt2",
            "tx2" => "dk2",
            other => other,
        },
    }
}

/// normalize_scheme_color_slot 的辅助函数：把 clr_map 里存的目标槽位名
/// （字符串）转译成 'static 生命周期的字符串 slice。clrMap 的合法目标值
/// 只有 dk1/lt1/dk2/lt2/accent1-6/hlink/folHlink 这固定 12 种
/// （OOXML ST_ColorSchemeIndex 规范）。
fn default_clr_map_target(mapped: &str) -> &'static str {
    match mapped {
        "dk1" => "dk1",
        "lt1" => "lt1",
        "dk2" => "dk2",
        "lt2" => "lt2",
        "accent1" => "accent1",
        "accent2" => "accent2",
        "accent3" => "accent3",
        "accent4" => "accent4",
        "accent5" => "accent5",
        "accent6" => "accent6",
        "hlink" => "hlink",
        "folHlink" => "folHlink",
        _ => "lt1",
    }
}

/// 解析6位十六进制颜色字符串（不带#前缀）→ Color（a固定255），失败返回黑色
fn parse_hex_color(s: &str) -> Color {
    let s = s.trim();
    if s.len() >= 6 {
        let r = u8::from_str_radix(&s[0..2], 16).unwrap_or(0);
        let g = u8::from_str_radix(&s[2..4], 16).unwrap_or(0);
        let b = u8::from_str_radix(&s[4..6], 16).unwrap_or(0);
        Color { r, g, b, a: 255 }
    } else {
        Color { r: 0, g: 0, b: 0, a: 255 }
    }
}

// ─── 统一后的主题色解析 ─────────────────────────────────────────────────

/// 解析 themeN.xml → HashMap<槽位名, Color>。合并自原 pptx.rs::parse_theme_colors
/// 与 xlsx.rs::read_xlsx_theme_colors，sysClr 分支采用 xlsx.rs 现有修复版本。
pub(crate) fn parse_ooxml_theme_colors(xml: &str) -> HashMap<String, Color> {
    let mut map: HashMap<String, Color> = HashMap::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut current_slot: Option<String> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let local = e.local_name();
                let name = std::str::from_utf8(local.as_ref()).unwrap_or("");
                match name {
                    "dk1" | "lt1" | "dk2" | "lt2"
                    | "accent1" | "accent2" | "accent3"
                    | "accent4" | "accent5" | "accent6"
                    | "hlink" | "folHlink" => {
                        current_slot = Some(name.to_string());
                    }
                    "srgbClr" => {
                        if let Some(slot) = &current_slot {
                            if let Some(val) = attr_str(e, b"val") {
                                map.insert(slot.clone(), parse_hex_color(&val));
                            }
                        }
                    }
                    "sysClr" => {
                        // ROOT-069(v130) 修复版本（原 xlsx.rs 逻辑，本次反向
                        // 同步给 pptx/docx）：sysClr 的 val 属性
                        // ("windowText"/"window") 才是权威语义，分别对应
                        // 黑色/白色；lastClr 只是历史缓存值，仅当 val 是
                        // 未知关键字时才退化使用。经三方参考渲染
                        // （OnlyOffice/WPS/LibreOffice）比对确认此行为正确。
                        if let Some(slot) = &current_slot {
                            let mut val_attr: Option<String> = None;
                            let mut last_clr: Option<Color> = None;
                            for attr in e.attributes().flatten() {
                                match attr.key.as_ref() {
                                    b"val" => {
                                        val_attr = attr.unescape_value().ok().map(|v| v.into_owned());
                                    }
                                    b"lastClr" => {
                                        if let Ok(v) = attr.unescape_value() {
                                            last_clr = Some(parse_hex_color(&v));
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            let resolved = match val_attr.as_deref() {
                                Some("windowText") => Some(Color { r: 0x00, g: 0x00, b: 0x00, a: 255 }),
                                Some("window") => Some(Color { r: 0xFF, g: 0xFF, b: 0xFF, a: 255 }),
                                _ => last_clr,
                            };
                            if let Some(c) = resolved {
                                map.insert(slot.clone(), c);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                let local = e.local_name();
                let name = std::str::from_utf8(local.as_ref()).unwrap_or("");
                if matches!(name, "dk1" | "lt1" | "dk2" | "lt2"
                    | "accent1" | "accent2" | "accent3"
                    | "accent4" | "accent5" | "accent6"
                    | "hlink" | "folHlink")
                {
                    current_slot = None;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    // Office默认主题色补全缺失槽位
    let defaults: &[(&str, (u8, u8, u8))] = &[
        ("dk1",      (0x00, 0x00, 0x00)),
        ("lt1",      (0xFF, 0xFF, 0xFF)),
        ("dk2",      (0x1F, 0x49, 0x7D)),
        ("lt2",      (0xEE, 0xEC, 0xE1)),
        ("accent1",  (0x4F, 0x81, 0xBD)),
        ("accent2",  (0xC0, 0x50, 0x4D)),
        ("accent3",  (0x9B, 0xBB, 0x59)),
        ("accent4",  (0x80, 0x64, 0xA2)),
        ("accent5",  (0x4B, 0xAC, 0xC6)),
        ("accent6",  (0xF7, 0x96, 0x46)),
        ("hlink",    (0x00, 0x00, 0xFF)),
        ("folHlink", (0x80, 0x00, 0x80)),
    ];
    for (slot, (r, g, b)) in defaults {
        map.entry(slot.to_string()).or_insert(Color { r: *r, g: *g, b: *b, a: 255 });
    }
    map
}

// ─── 统一后的 chart XML 解析（以原 pptx.rs::parse_pptx_chart_xml 为基准） ──

/// 解析 chartN.xml → ChartModel。合并自 xlsx.rs/pptx.rs/docx.rs 三处
/// 几乎相同的 parse_pptx_chart_xml，以 pptx.rs 版本（含系列颜色解析、
/// gap_width_pct 解析）为基准。xlsx/docx 调用时 clr_map 传空表即可，
/// normalize_scheme_color_slot 对空表有安全兜底，不会 panic。
pub(crate) fn parse_ooxml_chart_xml(
    xml: &str,
    theme_colors: &HashMap<String, Color>,
    clr_map: &HashMap<String, String>,
) -> Option<ChartModel> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut kind = ChartKind::Bar;
    let mut title = None::<String>;
    let mut x_labels: Vec<String> = Vec::new();
    let mut series: Vec<ChartSeries> = Vec::new();

    let mut in_title = false;
    let mut in_title_t = false;
    let mut title_buf = String::new();
    let mut in_ser = false;
    let mut in_ser_tx = false;
    let mut in_ser_tx_t = false;
    let mut ser_name_buf = String::new();
    let mut in_cat = false;
    let mut in_cat_pt = false;
    let mut cat_buf = String::new();
    let mut in_val = false;
    let mut in_val_pt = false;
    let mut val_buf = String::new();
    let mut cur_ser_name = String::new();
    let mut cur_x_labels: Vec<String> = Vec::new();
    let mut cur_vals: Vec<f64> = Vec::new();
    let mut found_chart = false;
    let mut in_line_chart = false;
    let mut cur_ser_trend = false;
    let mut has_legend = false;
    let mut has_grid = false;
    let mut x_axis_label = String::new();
    let mut y_axis_label = String::new();
    let mut in_cat_ax = false;
    let mut in_val_ax = false;
    let mut in_ser_sppr = false;
    let mut cur_ser_scheme_color: Option<String> = None;
    let mut cur_ser_rgb: Option<Color> = None;
    let mut in_axis_title = false;
    let mut in_axis_title_t = false;
    let mut cat_ax_title_buf = String::new();
    let mut val_ax_title_buf = String::new();
    let mut gap_width_pct: Option<f32> = None;
    let mut overlap_pct: Option<f32> = None;
    let mut grouping = roe_model::impress::ChartGrouping::Standard;
    let mut hole_size_pct: Option<f32> = None;
    // 扇区描边色：取第一个 <c:dPt><c:spPr><a:ln> 里的第一个颜色
    let mut in_dpt = false;
    let mut in_dpt_ln = false;
    let mut slice_border_color: Option<Color> = None;
    // 数据标签：任一 <c:dLbls> 内出现 showVal=1 即视为显示；位置取第一个 dLblPos
    // 标题占位：有 <c:title> 块但没有文字时，用默认文字
    let mut title_block_seen = false;
    let mut x_axis_title_block = false;
    let mut y_axis_title_block = false;
    let mut auto_title_deleted = false;
    let mut in_dlbls = false;
    let mut data_labels = roe_model::impress::DataLabels::default();
    let mut dlbl_pos_set = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                match e.local_name().as_ref() {
                    b"barChart" => { if !found_chart { kind = ChartKind::Bar; } found_chart = true; }
                    b"lineChart" => { if !found_chart { kind = ChartKind::Line; } found_chart = true; in_line_chart = true; }
                    b"pieChart" => { if !found_chart { kind = ChartKind::Pie; } found_chart = true; }
                    b"scatterChart" => { if !found_chart { kind = ChartKind::Scatter; } found_chart = true; }
                    b"radarChart" => { if !found_chart { kind = ChartKind::Radar; } found_chart = true; }
                    b"areaChart" => { if !found_chart { kind = ChartKind::Area; } found_chart = true; }
                    b"doughnutChart" => { if !found_chart { kind = ChartKind::Doughnut; } found_chart = true; }
                    b"legend" => { has_legend = true; }
                    b"majorGridlines" => { has_grid = true; }
                    b"trendline" if in_ser => { cur_ser_trend = true; }
                    b"dPt" => { in_dpt = true; }
                    b"dLbls" => { in_dlbls = true; data_labels.explicit = true; }
                    b"ln" if in_dpt => { in_dpt_ln = true; }
                    b"catAx" => { in_cat_ax = true; }
                    b"valAx" => { in_val_ax = true; }
                    b"title" if in_cat_ax || in_val_ax => {
                        in_axis_title = true;
                        if in_cat_ax { x_axis_title_block = true; } else { y_axis_title_block = true; }
                        if in_cat_ax { cat_ax_title_buf.clear(); } else { val_ax_title_buf.clear(); }
                    }
                    b"t" if in_axis_title => { in_axis_title_t = true; }
                    b"title" if !in_ser => { in_title = true; title_block_seen = true; title_buf.clear(); }
                    b"t" if in_title => { in_title_t = true; }
                    b"ser" => {
                        in_ser = true;
                        cur_ser_name.clear();
                        cur_x_labels.clear();
                        cur_vals.clear();
                        cur_ser_scheme_color = None;
                        cur_ser_rgb = None;
                        cur_ser_trend = false;
                    }
                    b"spPr" if in_ser => { in_ser_sppr = true; }
                    b"tx" if in_ser => { in_ser_tx = true; ser_name_buf.clear(); }
                    b"t" if in_ser_tx => { in_ser_tx_t = true; }
                    b"v" if in_ser_tx && !in_cat && !in_val => { in_ser_tx_t = true; }
                    b"cat" | b"xVal" if in_ser => { in_cat = true; }
                    b"pt" if in_cat => { in_cat_pt = true; cat_buf.clear(); }
                    b"v" if in_cat_pt => {}
                    b"val" | b"yVal" if in_ser => { in_val = true; }
                    b"pt" if in_val => { in_val_pt = true; val_buf.clear(); }
                    b"v" if in_val_pt => {}
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                match e.local_name().as_ref() {
                    b"trendlineType" if in_ser => {
                        if attr_str(e, b"val").as_deref() != Some("linear") { cur_ser_trend = false; }
                    }
                    b"barDir" => {
                        if let Some(v) = attr_str(e, b"val") {
                            if v == "bar" { kind = ChartKind::HorizontalBar; }
                        }
                    }
                    b"legend" => { has_legend = true; }
                    b"majorGridlines" => { has_grid = true; }
                    b"autoTitleDeleted" => {
                        auto_title_deleted = attr_str(e, b"val").as_deref() == Some("1");
                    }
                    b"gapWidth" => {
                        gap_width_pct = attr_f32(e, b"val");
                    }
                    b"overlap" => {
                        overlap_pct = attr_f32(e, b"val");
                    }
                    b"holeSize" => {
                        hole_size_pct = attr_f32(e, b"val");
                    }
                    b"showVal" if in_dlbls => {
                        if attr_str(e, b"val").as_deref() == Some("1") {
                            data_labels.show_val = true;
                        }
                    }
                    b"dLblPos" if in_dlbls && !dlbl_pos_set => {
                        use roe_model::impress::DataLabelPos as P;
                        let pos = match attr_str(e, b"val").as_deref() {
                            Some("ctr") => Some(P::Center),
                            Some("inEnd") => Some(P::InsideEnd),
                            Some("inBase") => Some(P::InsideBase),
                            Some("outEnd") => Some(P::OutsideEnd),
                            Some("bestFit") => Some(P::BestFit),
                            Some("l") => Some(P::Left),
                            Some("r") => Some(P::Right),
                            Some("t") => Some(P::Top),
                            Some("b") => Some(P::Bottom),
                            _ => None,
                        };
                        if let Some(p) = pos {
                            data_labels.position = p;
                            dlbl_pos_set = true;
                        }
                    }
                    b"grouping" => {
                        // 只取第一个 grouping（复合图表暂不区分）
                        if grouping == roe_model::impress::ChartGrouping::Standard {
                            grouping = match attr_str(e, b"val").as_deref() {
                                Some("stacked") => roe_model::impress::ChartGrouping::Stacked,
                                Some("percentStacked") => roe_model::impress::ChartGrouping::PercentStacked,
                                _ => roe_model::impress::ChartGrouping::Standard,
                            };
                        }
                    }
                    b"schemeClr" if in_dpt_ln && slice_border_color.is_none() => {
                        slice_border_color = attr_str(e, b"val")
                            .as_deref()
                            .and_then(|slot| theme_colors.get(normalize_scheme_color_slot(slot, clr_map)))
                            .copied();
                    }
                    b"srgbClr" if in_dpt_ln && slice_border_color.is_none() => {
                        if let Some(hex) = attr_str(e, b"val") {
                            if hex.len() == 6 {
                                if let (Ok(r), Ok(g), Ok(b)) = (
                                    u8::from_str_radix(&hex[0..2], 16),
                                    u8::from_str_radix(&hex[2..4], 16),
                                    u8::from_str_radix(&hex[4..6], 16),
                                ) {
                                    slice_border_color = Some(Color { r, g, b, a: 255 });
                                }
                            }
                        }
                    }
                    b"srgbClr" if in_ser_sppr && cur_ser_rgb.is_none() => {
                        if let Some(hex) = attr_str(e, b"val") {
                            if hex.len() == 6 {
                                if let (Ok(r), Ok(g), Ok(b)) = (
                                    u8::from_str_radix(&hex[0..2], 16),
                                    u8::from_str_radix(&hex[2..4], 16),
                                    u8::from_str_radix(&hex[4..6], 16),
                                ) {
                                    cur_ser_rgb = Some(Color { r, g, b, a: 255 });
                                }
                            }
                        }
                    }
                    b"schemeClr" if in_ser_sppr && cur_ser_scheme_color.is_none() => {
                        cur_ser_scheme_color = attr_str(e, b"val");
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                match e.local_name().as_ref() {
                    b"ln" if in_dpt_ln => { in_dpt_ln = false; }
                    b"dLbls" if in_dlbls => { in_dlbls = false; }
                    b"dPt" if in_dpt => { in_dpt = false; in_dpt_ln = false; }
                    b"title" if in_axis_title => {
                        in_axis_title = false;
                    }
                    b"t" if in_axis_title_t => { in_axis_title_t = false; }
                    b"catAx" if in_cat_ax => {
                        in_cat_ax = false;
                        let t = cat_ax_title_buf.trim().to_string();
                        if !t.is_empty() { x_axis_label = t; }
                    }
                    b"valAx" if in_val_ax => {
                        in_val_ax = false;
                        let t = val_ax_title_buf.trim().to_string();
                        if !t.is_empty() { y_axis_label = t; }
                    }
                    b"title" if in_title => {
                        in_title = false;
                        let t = title_buf.trim().to_string();
                        if !t.is_empty() { title = Some(t); }
                    }
                    b"t" if in_title_t => { in_title_t = false; }
                    b"tx" if in_ser_tx => {
                        in_ser_tx = false;
                        cur_ser_name = ser_name_buf.trim().to_string();
                    }
                    b"t" if in_ser_tx_t => { in_ser_tx_t = false; }
                    b"v" if in_ser_tx_t && !in_cat_pt && !in_val_pt => { in_ser_tx_t = false; }
                    b"pt" if in_cat_pt => {
                        in_cat_pt = false;
                        cur_x_labels.push(cat_buf.trim().to_string());
                    }
                    b"cat" | b"xVal" if in_cat => { in_cat = false; }
                    b"pt" if in_val_pt => {
                        in_val_pt = false;
                        let v = val_buf.trim().parse::<f64>().unwrap_or(0.0);
                        cur_vals.push(v);
                    }
                    b"val" | b"yVal" if in_val => { in_val = false; }
                    b"spPr" if in_ser_sppr => { in_ser_sppr = false; }
                    b"lineChart" => { in_line_chart = false; }
                    b"ser" if in_ser => {
                        in_ser = false;
                        in_ser_tx = false;
                        if x_labels.is_empty() {
                            x_labels = cur_x_labels.drain(..).collect();
                        }
                        let resolved_color = cur_ser_scheme_color.as_deref()
                            .and_then(|slot| theme_colors.get(normalize_scheme_color_slot(slot, clr_map)))
                            .copied();
                        series.push(ChartSeries {
                            name: cur_ser_name.clone(),
                            data: cur_vals.drain(..).collect(),
                            color: cur_ser_rgb.take().or(resolved_color),
                            as_line: in_line_chart,
                            trendline: cur_ser_trend,
                        });
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(ref e)) => {
                let t = e.unescape().unwrap_or_default();
                if in_title_t { title_buf.push_str(&t); }
                if in_axis_title_t {
                    if in_cat_ax { cat_ax_title_buf.push_str(&t); }
                    else if in_val_ax { val_ax_title_buf.push_str(&t); }
                }
                if in_ser_tx_t { ser_name_buf.push_str(&t); }
                if in_cat_pt { cat_buf.push_str(&t); }
                if in_val_pt { val_buf.push_str(&t); }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    if !found_chart { return None; }
    if title.is_none() && title_block_seen && !auto_title_deleted {
        title = Some(roe_model::impress::DEFAULT_CHART_TITLE.to_string());
    }
    if x_axis_label.is_empty() && x_axis_title_block {
        x_axis_label = roe_model::impress::DEFAULT_X_AXIS_TITLE.to_string();
    }
    if y_axis_label.is_empty() && y_axis_title_block {
        y_axis_label = roe_model::impress::DEFAULT_Y_AXIS_TITLE.to_string();
    }
    Some(ChartModel {
        id: 0,
        kind, title, x_labels, series, x_pt: 0.0, y_pt: 0.0, width_pt: 300.0, height_pt: 300.0,
        show_legend: has_legend, show_grid: has_grid,
        x_axis_label, y_axis_label,
        border_color: Color { r: 0xcc, g: 0xcc, b: 0xcc, a: 255 },
        border_width: 1.0,
        gap_width_pct,
        overlap_pct,
        grouping,
        hole_size_pct,
        slice_border_color,
        data_labels,
    })
}
