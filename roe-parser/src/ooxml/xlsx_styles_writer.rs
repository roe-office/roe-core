//! roe-parser/src/ooxml/xlsx_styles_writer.rs
//! Workbook 内存模型 → xl/styles.xml + 各 sheet 的 <conditionalFormatting> 片段
//!
//! 与 xlsx_writer.rs 分文件维护（新导出代码不塞进已有大文件）。
//!
//! 参考：LOK sc/source/filter/excel/xestyle.cxx（XclExpXFBuffer/XclExpDxfs）
//! 顶层元素顺序遵循 OOXML CT_Stylesheet schema 强制顺序：
//!   numFmts, fonts, fills, borders, cellStyleXfs, cellXfs, cellStyles, dxfs, tableStyles
//! 顺序写错会导致 Excel 判定文件损坏，不能颠倒其中任何一节的相对顺序
//! （某一节内容为空时可以整节省略，但不能颠倒顺序）。
//!
//! dxf 内部子元素顺序：font, numFmt, fill, alignment, border, protection
//! （本版本只写 font + numFmt + fill，align/border/protection 暂不支持，
//! 因为 CellFormat 内存模型本身还没有边框字段——见 ROE_Progress_v106.md
//! "styles.go 对照"专项待办，本次不做）。
//!
//! condition 字符串 ←→ cfRule type/operator/formula 的映射与 xlsx.rs parser 的
//! build_cf_condition() 完全对称（反向操作），新增/修改需两边同步核对。

use std::collections::HashMap;

use roe_model::calc::{CellFormat, ConditionalFormatKind, ConditionalFormatRule, Workbook};

// ─── 本文件自用的小工具函数（与 xlsx_writer.rs 里的同名私有函数保持相同语义，
// 不跨文件导出私有函数，两边各自维护一份，遵循本项目一贯的模块解耦约定，
// 参照 icon_set_render.rs / cond_format_render.rs 之间 resolve_cfvo 的先例）───

fn xml_escape_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn xml_escape_attr(s: &str) -> String {
    xml_escape_text(s).replace('"', "&quot;")
}

fn col_to_letters(mut col: u32) -> String {
    let mut s = String::new();
    while col > 0 {
        let rem = (col - 1) % 26;
        s.insert(0, (b'A' + rem as u8) as char);
        col = (col - 1) / 26;
    }
    if s.is_empty() {
        s.push('A');
    }
    s
}

fn range_to_sqref(r: &roe_model::calc::CellRange) -> String {
    let (r1, c1) = r.start;
    let (r2, c2) = r.end;
    let a1 = format!("{}{}", col_to_letters(c1 + 1), r1 + 1);
    if r.start == r.end {
        a1
    } else {
        let a2 = format!("{}{}", col_to_letters(c2 + 1), r2 + 1);
        format!("{a1}:{a2}")
    }
}

// ─── 内置 numFmtId 正向映射（与 xlsx.rs parser 的 builtin_num_fmt 互逆） ──────

fn builtin_num_fmt_id(fmt: &str) -> Option<u32> {
    match fmt {
        "0" => Some(1),
        "0.00" => Some(2),
        "#,##0" => Some(3),
        "#,##0.00" => Some(4),
        "0%" => Some(9),
        "0.00%" => Some(10),
        "0.00E+00" => Some(11),
        "# ?/?" => Some(12),
        "# ??/??" => Some(13),
        "m/d/yyyy" => Some(14),
        "d-mmm-yy" => Some(15),
        "d-mmm" => Some(16),
        "mmm-yy" => Some(17),
        "h:mm AM/PM" => Some(18),
        "h:mm:ss AM/PM" => Some(19),
        "h:mm" => Some(20),
        "h:mm:ss" => Some(21),
        "m/d/yyyy h:mm" => Some(22),
        "#,##0 ;(#,##0)" => Some(37),
        "#,##0 ;[Red](#,##0)" => Some(38),
        "#,##0.00;(#,##0.00)" => Some(39),
        "#,##0.00;[Red](#,##0.00)" => Some(40),
        "mm:ss" => Some(45),
        "[h]:mm:ss" => Some(46),
        "mmss.0" => Some(47),
        "##0.0E+0" => Some(48),
        "@" => Some(49),
        _ => None,
    }
}

// ─── Fonts ────────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, Eq, Hash)]
struct FontKey {
    bold: bool,
    italic: bool,
    underline: bool,
    size_bits: u32,
    family: String,
    color: Option<String>,
}

impl FontKey {
    fn from_format(fmt: &CellFormat) -> Self {
        FontKey {
            bold: fmt.bold.unwrap_or(false),
            italic: fmt.italic.unwrap_or(false),
            underline: fmt.underline.unwrap_or(false),
            size_bits: fmt.font_size.unwrap_or(11.0).to_bits(),
            family: fmt.font_family.clone().unwrap_or_else(|| "Calibri".to_string()),
            color: fmt.fg_color.clone(),
        }
    }

    fn size(&self) -> f32 {
        f32::from_bits(self.size_bits)
    }
}

fn write_font_xml(out: &mut String, key: &FontKey) {
    out.push_str("<font>");
    if key.bold {
        out.push_str("<b/>");
    }
    if key.italic {
        out.push_str("<i/>");
    }
    if key.underline {
        out.push_str("<u/>");
    }
    out.push_str(&format!(r#"<sz val="{}"/>"#, key.size()));
    if let Some(c) = &key.color {
        out.push_str(&format!(r#"<color rgb="{}"/>"#, argb_from_hex(c)));
    }
    out.push_str(&format!(r#"<name val="{}"/>"#, xml_escape_attr(&key.family)));
    out.push_str("</font>");
}

fn argb_from_hex(s: &str) -> String {
    let hex = s.trim_start_matches('#');
    if hex.len() == 8 {
        hex.to_uppercase()
    } else {
        format!("FF{}", hex.to_uppercase())
    }
}

// ─── Fills ────────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, Eq, Hash)]
struct FillKey {
    bg_color: Option<String>,
}

fn write_fill_xml(out: &mut String, key: &FillKey) {
    out.push_str("<fill><patternFill patternType=\"solid\">");
    if let Some(c) = &key.bg_color {
        out.push_str(&format!(r#"<fgColor rgb="{}"/>"#, argb_from_hex(c)));
    }
    out.push_str("</patternFill></fill>");
}

fn write_placeholder_fills(out: &mut String) {
    out.push_str(r#"<fill><patternFill patternType="none"/></fill>"#);
    out.push_str(r#"<fill><patternFill patternType="gray125"/></fill>"#);
}

// ─── CellBorders ────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, Eq, Hash)]
struct BorderLineKey {
    style: String,
    color: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct BorderKey {
    left: Option<BorderLineKey>,
    right: Option<BorderLineKey>,
    top: Option<BorderLineKey>,
    bottom: Option<BorderLineKey>,
}

impl BorderKey {
    fn from_format(fmt: &CellFormat) -> Self {
        match &fmt.borders {
            None => BorderKey { left: None, right: None, top: None, bottom: None },
            Some(b) => BorderKey {
                left: b.left.as_ref().map(|l| BorderLineKey { style: l.style.clone(), color: l.color.clone() }),
                right: b.right.as_ref().map(|l| BorderLineKey { style: l.style.clone(), color: l.color.clone() }),
                top: b.top.as_ref().map(|l| BorderLineKey { style: l.style.clone(), color: l.color.clone() }),
                bottom: b.bottom.as_ref().map(|l| BorderLineKey { style: l.style.clone(), color: l.color.clone() }),
            },
        }
    }

    fn is_empty(&self) -> bool {
        self.left.is_none() && self.right.is_none() && self.top.is_none() && self.bottom.is_none()
    }
}

fn write_border_line_xml(out: &mut String, tag: &str, line: &Option<BorderLineKey>) {
    match line {
        None => out.push_str(&format!("<{}/>", tag)),
        Some(l) => {
            out.push_str(&format!(r#"<{} style="{}">"#, tag, xml_escape_attr(&l.style)));
            if let Some(c) = &l.color {
                out.push_str(&format!(r#"<color rgb="{}"/>"#, argb_from_hex(c)));
            }
            out.push_str(&format!("</{}>", tag));
        }
    }
}

fn write_border_xml(out: &mut String, key: &BorderKey) {
    out.push_str("<border>");
    write_border_line_xml(out, "left", &key.left);
    write_border_line_xml(out, "right", &key.right);
    write_border_line_xml(out, "top", &key.top);
    write_border_line_xml(out, "bottom", &key.bottom);
    out.push_str("<diagonal/>");
    out.push_str("</border>");
}

fn write_placeholder_border(out: &mut String) {
    out.push_str("<border><left/><right/><top/><bottom/><diagonal/></border>");
}

// ─── CellXf ───────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, Eq, Hash)]
struct XfKey {
    font_id: usize,
    fill_id: usize,
    border_id: usize,
    num_fmt_id: u32,
    align_h: Option<String>,
    align_v: Option<String>,
    wrap_text: bool,
}

fn write_xf_xml(out: &mut String, key: &XfKey) {
    let mut attrs = format!(
        r#"numFmtId="{}" fontId="{}" fillId="{}" borderId="{}""#,
        key.num_fmt_id, key.font_id, key.fill_id, key.border_id
    );
    if key.num_fmt_id != 0 {
        attrs.push_str(r#" applyNumberFormat="1""#);
    }
    if key.font_id != 0 {
        attrs.push_str(r#" applyFont="1""#);
    }
    if key.fill_id != 0 {
        attrs.push_str(r#" applyFill="1""#);
    }
    if key.border_id != 0 {
        attrs.push_str(r#" applyBorder="1""#);
    }
    let has_align = key.align_h.is_some() || key.align_v.is_some() || key.wrap_text;
    if has_align {
        attrs.push_str(r#" applyAlignment="1""#);
    }
    out.push_str(&format!("<xf {}>", attrs));
    if has_align {
        let h = key
            .align_h
            .as_deref()
            .map(|s| format!(r#" horizontal="{}""#, xml_escape_attr(s)))
            .unwrap_or_default();
        let v = key
            .align_v
            .as_deref()
            .map(|s| format!(r#" vertical="{}""#, xml_escape_attr(map_align_v(s))))
            .unwrap_or_default();
        let wrap = if key.wrap_text { r#" wrapText="1""# } else { "" };
        out.push_str(&format!("<alignment{}{}{}/>", h, v, wrap));
    }
    out.push_str("</xf>");
}

fn map_align_v(s: &str) -> &str {
    match s {
        "middle" => "center",
        other => other,
    }
}

// ─── StylesCollector：遍历 workbook，收集去重后的 fonts/fills/numFmts/cellXfs ──

pub struct StylesCollector {
    fonts: Vec<FontKey>,
    font_index: HashMap<FontKey, usize>,
    fills: Vec<FillKey>,
    fill_index: HashMap<FillKey, usize>,
    borders: Vec<BorderKey>,
    border_index: HashMap<BorderKey, usize>,
    custom_num_fmts: Vec<String>,
    num_fmt_index: HashMap<String, u32>,
    xfs: Vec<XfKey>,
    xf_index: HashMap<XfKey, usize>,
    pub cell_xf: HashMap<(usize, u32, u32), usize>,
    dxfs: Vec<CellFormat>,
    dxf_index: HashMap<DxfKeyProxy, usize>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct DxfKeyProxy(String);

fn dxf_key_of(fmt: &CellFormat) -> DxfKeyProxy {
    DxfKeyProxy(format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        fmt.bold, fmt.italic, fmt.underline, fmt.fg_color, fmt.bg_color, fmt.num_format, fmt.font_family
    ))
}

impl StylesCollector {
    pub fn new() -> Self {
        StylesCollector {
            fonts: Vec::new(),
            font_index: HashMap::new(),
            fills: Vec::new(),
            fill_index: HashMap::new(),
            borders: Vec::new(),
            border_index: HashMap::new(),
            custom_num_fmts: Vec::new(),
            num_fmt_index: HashMap::new(),
            xfs: Vec::new(),
            xf_index: HashMap::new(),
            cell_xf: HashMap::new(),
            dxfs: Vec::new(),
            dxf_index: HashMap::new(),
        }
    }

    fn intern_font(&mut self, fmt: &CellFormat) -> usize {
        let key = FontKey::from_format(fmt);
        if let Some(&idx) = self.font_index.get(&key) {
            return idx;
        }
        let idx = self.fonts.len();
        self.fonts.push(key.clone());
        self.font_index.insert(key, idx);
        idx
    }

    /// 返回最终写入 xf 的 fillId:无背景色 -> 0(占位 none fill),
    /// 有背景色 -> 在 fills 中的下标 + 2(前两个是 none/gray125 占位)。
    fn intern_fill(&mut self, fmt: &CellFormat) -> usize {
        if fmt.bg_color.is_none() {
            return 0;
        }
        let key = FillKey { bg_color: fmt.bg_color.clone() };
        if let Some(&idx) = self.fill_index.get(&key) {
            return idx + 2;
        }
        let idx = self.fills.len();
        self.fills.push(key.clone());
        self.fill_index.insert(key, idx);
        idx + 2
    }

    fn intern_border(&mut self, fmt: &CellFormat) -> usize {
        let key = BorderKey::from_format(fmt);
        if key.is_empty() {
            return 0;
        }
        if let Some(&idx) = self.border_index.get(&key) {
            return idx;
        }
        // 下标0 预留给占位空边框（同 fill 的 +2 占位思路，但边框只需 1 个占位）
        let idx = self.borders.len() + 1;
        self.borders.push(key.clone());
        self.border_index.insert(key, idx);
        idx
    }

    fn intern_num_fmt(&mut self, fmt: &CellFormat) -> u32 {
        match &fmt.num_format {
            None => 0,
            Some(s) => {
                if let Some(id) = builtin_num_fmt_id(s) {
                    return id;
                }
                if let Some(&id) = self.num_fmt_index.get(s) {
                    return id;
                }
                let id = 164 + self.custom_num_fmts.len() as u32;
                self.custom_num_fmts.push(s.clone());
                self.num_fmt_index.insert(s.clone(), id);
                id
            }
        }
    }

    fn intern_xf(&mut self, fmt: &CellFormat) -> usize {
        let font_id = self.intern_font(fmt);
        let fill_id = self.intern_fill(fmt);
        let border_id = self.intern_border(fmt);
        let num_fmt_id = self.intern_num_fmt(fmt);
        let key = XfKey {
            font_id,
            fill_id,
            border_id,
            num_fmt_id,
            align_h: fmt.align_h.clone(),
            align_v: fmt.align_v.clone(),
            wrap_text: fmt.wrap_text.unwrap_or(false),
        };
        if let Some(&idx) = self.xf_index.get(&key) {
            return idx;
        }
        let idx = self.xfs.len();
        self.xfs.push(key.clone());
        self.xf_index.insert(key, idx);
        idx
    }

    pub fn collect(&mut self, workbook: &Workbook) {
        let default_fmt = CellFormat::default();
        let _ = self.intern_xf(&default_fmt);

        for (sheet_idx, sheet) in workbook.sheets.iter().enumerate() {
            for (&(row, col), fmt) in sheet.cell_formats.iter() {
                let xf_idx = self.intern_xf(fmt);
                self.cell_xf.insert((sheet_idx, row, col), xf_idx);
            }
            for rule in &sheet.cond_formats {
                self.collect_dxf_from_rule(rule);
            }
        }
    }

    fn collect_dxf_from_rule(&mut self, rule: &ConditionalFormatRule) {
        if let ConditionalFormatKind::CellFormat { format, .. } = &rule.kind {
            let key = dxf_key_of(format);
            if !self.dxf_index.contains_key(&key) {
                let idx = self.dxfs.len();
                self.dxfs.push(format.clone());
                self.dxf_index.insert(key, idx);
            }
        }
    }

    pub fn dxf_id_for(&self, format: &CellFormat) -> u32 {
        let key = dxf_key_of(format);
        self.dxf_index.get(&key).copied().unwrap_or(0) as u32
    }

    pub fn write_styles_xml(&self) -> String {
        let mut body = String::new();

        if !self.custom_num_fmts.is_empty() {
            body.push_str(&format!(r#"<numFmts count="{}">"#, self.custom_num_fmts.len()));
            for (i, fmt) in self.custom_num_fmts.iter().enumerate() {
                let id = 164 + i as u32;
                body.push_str(&format!(
                    r#"<numFmt numFmtId="{}" formatCode="{}"/>"#,
                    id,
                    xml_escape_attr(fmt)
                ));
            }
            body.push_str("</numFmts>");
        }

        let font_count = self.fonts.len().max(1);
        body.push_str(&format!(r#"<fonts count="{}">"#, font_count));
        if self.fonts.is_empty() {
            body.push_str(r#"<font><sz val="11"/><name val="Calibri"/></font>"#);
        } else {
            for f in &self.fonts {
                write_font_xml(&mut body, f);
            }
        }
        body.push_str("</fonts>");

        body.push_str(&format!(r#"<fills count="{}">"#, self.fills.len() + 2));
        write_placeholder_fills(&mut body);
        for f in &self.fills {
            write_fill_xml(&mut body, f);
        }
        body.push_str("</fills>");

        body.push_str(&format!(r#"<borders count="{}">"#, self.borders.len() + 1));
        write_placeholder_border(&mut body);
        for b in &self.borders {
            write_border_xml(&mut body, b);
        }
        body.push_str("</borders>");

        let xf_count = self.xfs.len().max(1);
        body.push_str(&format!(r#"<cellXfs count="{}">"#, xf_count));
        if self.xfs.is_empty() {
            body.push_str(r#"<xf numFmtId="0" fontId="0" fillId="0" borderId="0"/>"#);
        } else {
            for xf in &self.xfs {
                write_xf_xml(&mut body, xf);
            }
        }
        body.push_str("</cellXfs>");

        if !self.dxfs.is_empty() {
            body.push_str(&format!(r#"<dxfs count="{}">"#, self.dxfs.len()));
            for fmt in &self.dxfs {
                write_dxf_xml(&mut body, fmt);
            }
            body.push_str("</dxfs>");
        }

        format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">{body}</styleSheet>"#
        )
    }
}

fn write_dxf_xml(out: &mut String, fmt: &CellFormat) {
    out.push_str("<dxf>");
    let has_font =
        fmt.bold.is_some() || fmt.italic.is_some() || fmt.underline.is_some() || fmt.fg_color.is_some();
    if has_font {
        out.push_str("<font>");
        if fmt.bold.unwrap_or(false) {
            out.push_str("<b/>");
        }
        if fmt.italic.unwrap_or(false) {
            out.push_str("<i/>");
        }
        if fmt.underline.unwrap_or(false) {
            out.push_str("<u/>");
        }
        if let Some(c) = &fmt.fg_color {
            out.push_str(&format!(r#"<color rgb="{}"/>"#, argb_from_hex(c)));
        }
        out.push_str("</font>");
    }
    if let Some(nf) = &fmt.num_format {
        if builtin_num_fmt_id(nf).is_none() {
            out.push_str(&format!(
                r#"<numFmt numFmtId="164" formatCode="{}"/>"#,
                xml_escape_attr(nf)
            ));
        }
    }
    if let Some(bg) = &fmt.bg_color {
        out.push_str(&format!(
            r#"<fill><patternFill><bgColor rgb="{}"/></patternFill></fill>"#,
            argb_from_hex(bg)
        ));
    }
    out.push_str("</dxf>");
}

// ─── conditionalFormatting 容器 + cfRule 写出 ─────────────────────────────────

pub fn write_conditional_formatting_xml(
    rules: &[ConditionalFormatRule],
    styles: &StylesCollector,
) -> String {
    let mut out = String::new();
    for (i, rule) in rules.iter().enumerate() {
        let sqref = range_to_sqref(&rule.range);
        out.push_str(&format!(r#"<conditionalFormatting sqref="{}">"#, xml_escape_attr(&sqref)));
        let priority = (i + 1) as u32;
        write_cf_rule_xml(&mut out, rule, priority, styles, i);
        out.push_str("</conditionalFormatting>");
    }
    out
}

fn write_cf_rule_xml(
    out: &mut String,
    rule: &ConditionalFormatRule,
    priority: u32,
    styles: &StylesCollector,
    rule_idx: usize,
) {
    let stop_attr = if rule.stop_if_true { r#" stopIfTrue="1""# } else { "" };
    match &rule.kind {
        ConditionalFormatKind::CellFormat { condition, format } => {
            let (cf_type, operator, formulas) = condition_to_cf_type(condition);
            let dxf_id = styles.dxf_id_for(format);
            out.push_str(&format!(
                r#"<cfRule type="{}" dxfId="{}" priority="{}"{}"#,
                cf_type, dxf_id, priority, stop_attr
            ));
            if let Some(op) = operator {
                out.push_str(&format!(r#" operator="{}""#, op));
            }
            out.push('>');
            for f in &formulas {
                if !f.is_empty() {
                    out.push_str(&format!("<formula>{}</formula>", xml_escape_text(f)));
                }
            }
            out.push_str("</cfRule>");
        }
        ConditionalFormatKind::ColorScale { min_type, min_value, min_color, mid, max_type, max_value, max_color } => {
            out.push_str(&format!(
                r#"<cfRule type="colorScale" priority="{}"{}><colorScale>"#,
                priority, stop_attr
            ));
            write_cfvo(out, min_type, min_value.as_deref());
            if let Some((mid_type, mid_value, _)) = mid {
                write_cfvo(out, mid_type, Some(mid_value.as_str()));
            }
            write_cfvo(out, max_type, max_value.as_deref());
            out.push_str(&format!(r#"<color rgb="{}"/>"#, argb_from_hex(min_color)));
            if let Some((_, _, mid_color)) = mid {
                out.push_str(&format!(r#"<color rgb="{}"/>"#, argb_from_hex(mid_color)));
            }
            out.push_str(&format!(r#"<color rgb="{}"/>"#, argb_from_hex(max_color)));
            out.push_str("</colorScale></cfRule>");
        }
        ConditionalFormatKind::DataBar { min, max, bar_color, border_color: _, direction: _, bar_only, solid_fill: _, negative_fill_color: _, axis_color: _, axis_position: _ } => {
            out.push_str(&format!(
                r#"<cfRule type="dataBar" priority="{}"{}><dataBar showValue="{}">"#,
                priority,
                stop_attr,
                if *bar_only { "0" } else { "1" }
            ));
            // autoMin/autoMax 只在 x14 扩展里合法,legacy dataBar 降级为 min/max,
            // 否则 Excel 会判文件损坏。完整保真需写 x14 扩展(遗留项)。
            fn legacy_kind(k: &str) -> &str { match k { "autoMin" => "min", "autoMax" => "max", o => o } }
            write_cfvo(out, legacy_kind(&min.kind), Some(min.val.as_str()));
            write_cfvo(out, legacy_kind(&max.kind), Some(max.val.as_str()));
            out.push_str(&format!(r#"<color rgb="{}"/>"#, argb_from_hex(bar_color)));
            out.push_str("</dataBar>");
            out.push_str(&format!(
                r#"<extLst><ext uri="{{B025F937-C7B1-47D3-B67F-A62EFF666E3E}}" xmlns:x14="http://schemas.microsoft.com/office/spreadsheetml/2009/9/main"><x14:id>{}</x14:id></ext></extLst></cfRule>"#,
                databar_guid(rule_idx)
            ));
            // 不写 extLst（Excel 2010+ GUID 扩展块）：核心字段对齐标准即可，
            // 不绑定微软私有扩展——见 ROE_Progress_v106.md 决策记录。
        }
        ConditionalFormatKind::IconSet { icon_style, thresholds, percent: _, reverse, icons_only, tier_overrides: _ } => {
            out.push_str(&format!(
                r#"<cfRule type="iconSet" priority="{}"{}><iconSet iconSet="{}""#,
                priority, stop_attr, xml_escape_attr(icon_style)
            ));
            if *icons_only {
                out.push_str(r#" showValue="0""#);
            }
            if *reverse {
                out.push_str(r#" reverse="1""#);
            }
            out.push('>');
            for t in thresholds {
                write_cfvo(out, &t.kind, Some(t.val.as_str()));
            }
            out.push_str("</iconSet></cfRule>");
        }
    }
}

/// 按规则序号确定性生成 GUID(输出可复现,不引入随机依赖)。
pub fn databar_guid(rule_idx: usize) -> String {
    format!("{{00000000-0000-0000-0000-{:012X}}}", rule_idx + 1)
}

fn x14_cfvo(kind: &str, val: &str) -> String {
    match kind {
        "num" | "percent" | "percentile" | "formula" => format!(
            r#"<x14:cfvo type="{}"><xm:f>{}</xm:f></x14:cfvo>"#,
            xml_escape_attr(kind), xml_escape_text(val)
        ),
        k => format!(r#"<x14:cfvo type="{}"/>"#, xml_escape_attr(k)),
    }
}

/// 生成 <ext uri="{78C0D931-...}"> 块(x14:conditionalFormattings),
/// 只含 dataBar 规则;没有 dataBar 时返回空串。与 legacy cfRule 靠 GUID 关联。
pub fn write_x14_databar_ext_xml(rules: &[ConditionalFormatRule]) -> String {
    let mut body = String::new();
    for (i, rule) in rules.iter().enumerate() {
        let ConditionalFormatKind::DataBar {
            min, max, bar_color: _, border_color, direction, bar_only, solid_fill,
            negative_fill_color, axis_color, axis_position,
        } = &rule.kind else { continue };

        let mut attrs = String::from(r#" minLength="0" maxLength="100""#);
        if border_color.is_some() { attrs.push_str(r#" border="1""#); }
        if *solid_fill { attrs.push_str(r#" gradient="0""#); }
        match direction {
            roe_model::calc::BarDirection::LeftToRight => attrs.push_str(r#" direction="leftToRight""#),
            roe_model::calc::BarDirection::RightToLeft => attrs.push_str(r#" direction="rightToLeft""#),
            roe_model::calc::BarDirection::Auto => {}
        }
        if *bar_only { attrs.push_str(r#" showValue="0""#); }
        if !axis_position.is_empty() && axis_position != "automatic" {
            attrs.push_str(&format!(r#" axisPosition="{}""#, xml_escape_attr(axis_position)));
        }
        let mut inner = String::new();
        inner.push_str(&x14_cfvo(&min.kind, &min.val));
        inner.push_str(&x14_cfvo(&max.kind, &max.val));
        if let Some(c) = border_color { inner.push_str(&format!(r#"<x14:borderColor rgb="{}"/>"#, argb_from_hex(c))); }
        if let Some(c) = negative_fill_color { inner.push_str(&format!(r#"<x14:negativeFillColor rgb="{}"/>"#, argb_from_hex(c))); }
        if let Some(c) = axis_color { inner.push_str(&format!(r#"<x14:axisColor rgb="{}"/>"#, argb_from_hex(c))); }

        let sqref = range_to_sqref(&rule.range);
        body.push_str(&format!(
            r#"<x14:conditionalFormatting xmlns:xm="http://schemas.microsoft.com/office/excel/2006/main"><x14:cfRule type="dataBar" id="{}"><x14:dataBar{}>{}</x14:dataBar></x14:cfRule><xm:sqref>{}</xm:sqref></x14:conditionalFormatting>"#,
            databar_guid(i), attrs, inner, xml_escape_text(&sqref)
        ));
    }
    if body.is_empty() { return String::new(); }
    format!(
        r#"<ext uri="{{78C0D931-6437-407d-A8EE-F0AAD7539E65}}" xmlns:x14="http://schemas.microsoft.com/office/spreadsheetml/2009/9/main"><x14:conditionalFormattings>{}</x14:conditionalFormattings></ext>"#,
        body
    )
}

fn write_cfvo(out: &mut String, kind: &str, val: Option<&str>) {
    let val_attr = val.map(|v| format!(r#" val="{}""#, xml_escape_attr(v))).unwrap_or_default();
    out.push_str(&format!(r#"<cfvo type="{}"{}/>"#, kind, val_attr));
}

/// condition 字符串 → (cfRule type, operator, formula 列表)。
/// 与 xlsx.rs::build_cf_condition() 完全对称的反向映射。
fn condition_to_cf_type(condition: &str) -> (&'static str, Option<&'static str>, Vec<String>) {
    let mut parts = condition.splitn(3, ':');
    let head = parts.next().unwrap_or("");
    let a = parts.next().unwrap_or("").to_string();
    let b = parts.next().unwrap_or("").to_string();
    match head {
        "gt" => ("cellIs", Some("greaterThan"), vec![a]),
        "gte" => ("cellIs", Some("greaterThanOrEqual"), vec![a]),
        "lt" => ("cellIs", Some("lessThan"), vec![a]),
        "lte" => ("cellIs", Some("lessThanOrEqual"), vec![a]),
        "eq" => ("cellIs", Some("equal"), vec![a]),
        "ne" => ("cellIs", Some("notEqual"), vec![a]),
        "between" => ("cellIs", Some("between"), vec![a, b]),
        "not_between" => ("cellIs", Some("notBetween"), vec![a, b]),
        "text_contains" => ("containsText", None, vec![a]),
        "text_not_contains" => ("notContainsText", None, vec![a]),
        "begins_with" => ("beginsWith", None, vec![a]),
        "ends_with" => ("endsWith", None, vec![a]),
        "expr" => ("expression", None, vec![a]),
        "duplicate" => ("duplicateValues", None, vec![]),
        "unique" => ("uniqueValues", None, vec![]),
        "top10" => ("top10", None, vec![a]),
        "above_average" => ("aboveAverage", None, vec![]),
        "time_period" => ("timePeriod", None, vec![a]),
        _ => ("expression", None, vec![condition.to_string()]),
    }
}
