//! 图表格式补丁的共享解析与应用(C-ABI roe_set_chart_style 与 Rhai set_chart_style 共用)。
//! 先把整个 JSON 解析成补丁,全部合法才写入,任一字段非法则一个字段都不改。

use roe_model::common::Color;
use roe_model::impress::{ChartGrouping, ChartKind, ChartModel};
use roe_model::stat_chart::StatChartModel;
use serde_json::{Map, Value};

pub const STYLE_KEYS: &[&str] = &[
    "kind", "title", "show_legend", "show_grid", "x_axis_label", "y_axis_label",
    "border_color", "border_width", "grouping", "show_val",
    "gap_width_pct", "overlap_pct", "hole_size_pct",
];
pub const STAT_STYLE_KEYS: &[&str] = &["title", "show_legend", "show_grid"];
pub const KINDS: &[&str] = &[
    "Bar", "HorizontalBar", "Line", "Pie", "Scatter", "Radar", "Area", "Doughnut",
];
pub const GROUPINGS: &[&str] = &["Standard", "Stacked", "PercentStacked"];

fn parse_hex(s: &str) -> Option<Color> {
    let h = s.strip_prefix('#').unwrap_or(s);
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) { return None; }
    let r = u8::from_str_radix(&h[0..2], 16).ok()?;
    let g = u8::from_str_radix(&h[2..4], 16).ok()?;
    let b = u8::from_str_radix(&h[4..6], 16).ok()?;
    Some(Color { r, g, b, a: 255 })
}

fn as_obj(v: &Value) -> Result<&Map<String, Value>, String> {
    v.as_object().ok_or_else(|| "style_json 必须是 JSON 对象".to_string())
}

fn check_keys(obj: &Map<String, Value>, allowed: &[&str], reject: bool) -> Result<(), String> {
    if reject {
        for k in obj.keys() {
            if !allowed.contains(&k.as_str()) {
                return Err(format!("未知键 {:?};合法键: {}", k, allowed.join(", ")));
            }
        }
    }
    Ok(())
}

fn get_bool(obj: &Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    match obj.get(key) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(o) => Err(format!("{key} 必须是 true/false,收到 {o}")),
    }
}

fn get_string(obj: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match obj.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(o) => Err(format!("{key} 必须是字符串,收到 {o}")),
    }
}

/// title: 缺省=不改;null=清除;字符串=设置
fn get_title(obj: &Map<String, Value>) -> Result<Option<Option<String>>, String> {
    match obj.get("title") {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(s)) => Ok(Some(Some(s.clone()))),
        Some(o) => Err(format!("title 必须是字符串或 null,收到 {o}")),
    }
}

/// 数字字段: 缺省=不改;null=清除;数字=设置
fn get_num_opt(obj: &Map<String, Value>, key: &str) -> Result<Option<Option<f32>>, String> {
    match obj.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(v) => match v.as_f64() {
            Some(n) if n.is_finite() => Ok(Some(Some(n as f32))),
            _ => Err(format!("{key} 必须是有限数字或 null,收到 {v}")),
        },
    }
}

/// 对普通图应用格式补丁。reject_unknown=true 时未知键报错。
pub fn apply_chart_style(m: &mut ChartModel, v: &Value, reject_unknown: bool) -> Result<(), String> {
    let obj = as_obj(v)?;
    check_keys(obj, STYLE_KEYS, reject_unknown)?;

    let kind = match obj.get("kind") {
        None => None,
        Some(Value::String(s)) => Some(match s.as_str() {
            "Bar" => ChartKind::Bar,
            "HorizontalBar" => ChartKind::HorizontalBar,
            "Line" => ChartKind::Line,
            "Pie" => ChartKind::Pie,
            "Scatter" => ChartKind::Scatter,
            "Radar" => ChartKind::Radar,
            "Area" => ChartKind::Area,
            "Doughnut" => ChartKind::Doughnut,
            other => return Err(format!(
                "kind={:?} 不合法,合法值(区分大小写): {}", other, KINDS.join(", "))),
        }),
        Some(o) => return Err(format!("kind 必须是字符串,收到 {o}")),
    };
    let title = get_title(obj)?;
    let show_legend = get_bool(obj, "show_legend")?;
    let show_grid = get_bool(obj, "show_grid")?;
    let x_axis_label = get_string(obj, "x_axis_label")?;
    let y_axis_label = get_string(obj, "y_axis_label")?;
    let border_color = match get_string(obj, "border_color")? {
        None => None,
        Some(s) => Some(parse_hex(&s).ok_or_else(|| format!(
            "border_color={:?} 不合法,须为 \"#RRGGBB\"", s))?),
    };
    let border_width = match obj.get("border_width") {
        None => None,
        Some(v) => match v.as_f64() {
            Some(n) if n.is_finite() && n >= 0.0 => Some(n as f32),
            _ => return Err(format!("border_width 必须是非负数字,收到 {v}")),
        },
    };
    let grouping = match get_string(obj, "grouping")? {
        None => None,
        Some(s) => Some(match s.as_str() {
            "Standard" => ChartGrouping::Standard,
            "Stacked" => ChartGrouping::Stacked,
            "PercentStacked" => ChartGrouping::PercentStacked,
            other => return Err(format!(
                "grouping={:?} 不合法,合法值: {}", other, GROUPINGS.join(", "))),
        }),
    };
    let show_val = get_bool(obj, "show_val")?;
    let gap = get_num_opt(obj, "gap_width_pct")?;
    let overlap = get_num_opt(obj, "overlap_pct")?;
    let hole = get_num_opt(obj, "hole_size_pct")?;

    // 全部校验通过,开始写入
    if let Some(k) = kind { m.kind = k; }
    if let Some(t) = title { m.title = t; }
    if let Some(b) = show_legend { m.show_legend = b; }
    if let Some(b) = show_grid { m.show_grid = b; }
    if let Some(s) = x_axis_label { m.x_axis_label = s; }
    if let Some(s) = y_axis_label { m.y_axis_label = s; }
    if let Some(c) = border_color { m.border_color = c; }
    if let Some(w) = border_width { m.border_width = w; }
    if let Some(g) = grouping { m.grouping = g; }
    if let Some(b) = show_val { m.data_labels.show_val = b; }
    if let Some(x) = gap { m.gap_width_pct = x; }
    if let Some(x) = overlap { m.overlap_pct = x; }
    if let Some(x) = hole { m.hole_size_pct = x; }
    Ok(())
}

/// 对统计图(chartEx)应用格式补丁:只认 title/show_legend/show_grid。
pub fn apply_stat_chart_style(m: &mut StatChartModel, v: &Value, reject_unknown: bool) -> Result<(), String> {
    let obj = as_obj(v)?;
    if reject_unknown {
        for k in obj.keys() {
            if !STAT_STYLE_KEYS.contains(&k.as_str()) {
                return Err(format!(
                    "统计图(直方/箱线/瀑布/漏斗/旭日/树状)只支持 {},不支持 {:?}",
                    STAT_STYLE_KEYS.join(", "), k));
            }
        }
    }
    let title = get_title(obj)?;
    let show_legend = get_bool(obj, "show_legend")?;
    let show_grid = get_bool(obj, "show_grid")?;
    if let Some(t) = title {
        m.title_present = t.is_some();
        m.title = t;
    }
    if let Some(b) = show_legend { m.show_legend = b; }
    if let Some(b) = show_grid { m.show_grid = b; }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::impress::DataLabels;

    fn model() -> ChartModel {
        ChartModel {
            id: 1, kind: ChartKind::Bar, title: None, x_labels: vec![], series: vec![],
            x_pt: 0.0, y_pt: 0.0, width_pt: 300.0, height_pt: 300.0,
            show_legend: true, show_grid: true,
            x_axis_label: String::new(), y_axis_label: String::new(),
            border_color: Color { r: 0xcc, g: 0xcc, b: 0xcc, a: 255 }, border_width: 1.0,
            gap_width_pct: None, overlap_pct: None, grouping: ChartGrouping::Standard,
            hole_size_pct: None, data_labels: DataLabels::default(), slice_border_color: None,
        }
    }
    fn j(s: &str) -> Value { serde_json::from_str(s).unwrap() }

    #[test]
    fn applies_all_fields() {
        let mut m = model();
        apply_chart_style(&mut m, &j(r##"{"kind":"Line","title":"T","show_legend":false,"show_grid":false,
            "x_axis_label":"X","y_axis_label":"Y","border_color":"#ff0000","border_width":2.5,
            "grouping":"Stacked","show_val":true,"gap_width_pct":40,"overlap_pct":-10,"hole_size_pct":55}"##), true).unwrap();
        assert!(matches!(m.kind, ChartKind::Line));
        assert_eq!(m.title.as_deref(), Some("T"));
        assert!(!m.show_legend && !m.show_grid && m.data_labels.show_val);
        assert_eq!((m.x_axis_label.as_str(), m.y_axis_label.as_str()), ("X", "Y"));
        assert_eq!(m.border_color, Color { r: 255, g: 0, b: 0, a: 255 });
        assert_eq!(m.border_width, 2.5);
        assert_eq!(m.grouping, ChartGrouping::Stacked);
        assert_eq!((m.gap_width_pct, m.overlap_pct, m.hole_size_pct), (Some(40.0), Some(-10.0), Some(55.0)));
    }

    #[test]
    fn null_clears_title_and_numbers() {
        let mut m = model();
        m.title = Some("old".into());
        m.gap_width_pct = Some(10.0);
        apply_chart_style(&mut m, &j(r#"{"title":null,"gap_width_pct":null}"#), true).unwrap();
        assert!(m.title.is_none() && m.gap_width_pct.is_none());
    }

    #[test]
    fn invalid_field_changes_nothing() {
        let mut m = model();
        let e = apply_chart_style(&mut m, &j(r#"{"title":"X","show_legend":false,"kind":"Nope"}"#), true).unwrap_err();
        assert!(e.contains("Nope") && e.contains("HorizontalBar"), "got: {e}");
        assert!(m.title.is_none() && m.show_legend, "任一字段非法,整体不生效");
    }

    #[test]
    fn kind_is_case_sensitive() {
        let mut m = model();
        assert!(apply_chart_style(&mut m, &j(r#"{"kind":"bar"}"#), false).is_err());
    }

    #[test]
    fn type_errors_are_reported() {
        for (s, key) in [
            (r#"{"show_legend":"yes"}"#, "show_legend"),
            (r#"{"border_width":"2"}"#, "border_width"),
            (r#"{"border_width":-1}"#, "border_width"),
            (r#"{"x_axis_label":5}"#, "x_axis_label"),
            (r#"{"title":5}"#, "title"),
            (r#"{"gap_width_pct":"a"}"#, "gap_width_pct"),
        ] {
            let mut m = model();
            let e = apply_chart_style(&mut m, &j(s), false).unwrap_err();
            assert!(e.contains(key), "{s} -> {e}");
        }
    }

    #[test]
    fn bad_color_and_grouping() {
        let mut m = model();
        assert!(apply_chart_style(&mut m, &j(r##"{"border_color":"red"}"##), false).is_err());
        assert!(apply_chart_style(&mut m, &j(r##"{"border_color":"#12345"}"##), false).is_err());
        assert!(apply_chart_style(&mut m, &j(r##"{"border_color":"#中文中文"}"##), false).is_err());
        let e = apply_chart_style(&mut m, &j(r#"{"grouping":"stacked"}"#), false).unwrap_err();
        assert!(e.contains("PercentStacked"), "got: {e}");
    }

    #[test]
    fn unknown_keys_rejected_only_in_strict_mode() {
        let mut m = model();
        let e = apply_chart_style(&mut m, &j(r#"{"xlabel":"a"}"#), true).unwrap_err();
        assert!(e.contains("xlabel") && e.contains("x_axis_label"), "got: {e}");
        assert!(apply_chart_style(&mut m, &j(r#"{"xlabel":"a"}"#), false).is_ok());
    }

    #[test]
    fn non_object_rejected() {
        let mut m = model();
        assert!(apply_chart_style(&mut m, &j("[1]"), false).is_err());
    }
}
