//! FODT（Flat ODF Text）写出器
//! roe_model::writer::WriterDocument → FODT XML 字符串
//!
//! 已知限制（与 fods_writer.rs 对称，详见文件尾测试）：
//! - paragraph_styles 是真实样式定义+引用，FODT parser 会读回（与 FODS 不同，
//!   FODT parser 本身就解析 office:styles）
//! - char_styles 按规范写 family="text"，但 parser 把 text/paragraph family
//!   同等处理，统一落进 paragraph_styles（不进 char_styles），且 parser 的
//!   ParagraphStyle 没有 underline/color 字段，所以这两个字段 roundtrip 后丢失，
//!   只有 font_size/font_family/bold/italic 能从 paragraph_styles 里查到
//! - Run 内联 bold/italic/font_size/font_family（未走 char_style 时）：自动生成
//!   family="text" 的 auto style 引用，效果同上
//! - Paragraph 内联 alignment/space_before/space_after：自动生成 family="paragraph"
//!   的 auto style，parent-style-name 指回原 style，可 roundtrip；
//!   indent_left/indent_first 写出但 parser 不读，单向
//! - hyperlink（text:a）、footnote（text:note）：单向，parser 完全不识别这两个标签，
//!   其内部文本会被当作普通文本"漏"进所在段落的 runs 里（不会丢、但不在原字段位置）
//! - Table：rows/cells/col_span/row_span/cell style 会 roundtrip；
//!   table-column 实际宽度不会——parser 固定回填 60.0（parser 自身限制）
//! - Section：单向，parser 不识别 text:section，内部段落仍会被当作顶层 body
//!   段落解析回来（包裹结构丢失，内容不丢）
//! - Image：完全单向，parser 不识别 draw:frame/draw:image
//! - header/footer：写入 master-page，parser 不读，单向
//! - CharStyle.color / CellStyle.bg_color / CellStyle.text_color：不写出，
//!   且这不是"待补"而是结构性限制（已核实 Color={r,g,b,a}，结构本身很
//!   简单，不存在猜错字段的问题）：FODT parser 把 family="text" 和
//!   family="paragraph" 统一落进 ParagraphStyle（见 fodt.rs
//!   text-properties 分支），而 ParagraphStyle 本身没有 color 字段——
//!   所以 CharStyle.color 即使写出去也没地方读回来；CellStyle 对应的
//!   table-cell 样式 FODT parser 完全不解析（只记录 style-name 引用，
//!   不解析样式内容），bg_color/text_color 同样无法 roundtrip。要解决
//!   需要先重构 parser 的样式归类逻辑（区分 text/paragraph family 分别
//!   落位，以及补上 table-cell 样式解析），不是这次小修补的范围，留给
//!   后续专项 Step（排在 Step 30 之后）。
//!
//! ⚠️ 关键坑：<text:p>...</text:p> 内部绝不能插入"美化排版"用的换行/缩进——
//! parser reader 配置了 trim_text(false)，标签之间的空白会被当成真实文本节点
//! push 进段落的 runs 里，污染内容。本文件内所有 run 级写入都用 write! 不用
//! writeln!，paragraph 的开合标签紧贴 run 内容，不留空白。块级元素之间
//! （段落与段落、表格与表格之间）的换行是安全的，因为那时 para_stack 是空的，
//! push_text 会静默丢弃。

use roe_model::style::{Alignment, CellStyle, CharStyle, ParagraphStyle, StyleRegistry};
use roe_model::writer::{
    BodyElement, DocumentMeta, InlineImage, Paragraph, Run, Section, Table, TableCell, WriterDocument,
};
use std::fmt::Write as _;
use base64::Engine;

pub struct FodtWriter;

#[derive(Default)]
struct AutoCounter {
    para: u32,
    text: u32,
}

impl FodtWriter {
    pub fn write(doc: &WriterDocument) -> String {
        let mut auto_styles = String::new();
        let mut body_xml = String::new();
        let mut counter = AutoCounter::default();

        write_named_styles(&mut auto_styles, &doc.styles);

        for el in &doc.body {
            write_body_element(&mut body_xml, el, &mut auto_styles, &mut counter);
        }

        let mut out = String::with_capacity(4096 + body_xml.len());
        out.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
        out.push('\n');
        out.push_str(
            r#"<office:document
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0"
    xmlns:xlink="http://www.w3.org/1999/xlink"
    xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"
    xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0"
    office:mimetype="application/vnd.oasis.opendocument.text">
"#,
        );

        out.push_str("  <office:meta>\n");
        write_meta(&mut out, &doc.metadata);
        out.push_str("  </office:meta>\n");

        out.push_str("  <office:automatic-styles>\n");
        write_page_layout(&mut out, doc);
        out.push_str(&auto_styles);
        out.push_str("  </office:automatic-styles>\n");

        out.push_str("  <office:master-styles>\n");
        write_master_page(&mut out, doc);
        out.push_str("  </office:master-styles>\n");

        out.push_str("  <office:body>\n    <office:text>\n");
        out.push_str(&body_xml);
        out.push_str("    </office:text>\n  </office:body>\n</office:document>\n");

        out
    }
}

// ── meta ─────────────────────────────────────────────────────

fn write_meta(out: &mut String, meta: &DocumentMeta) {
    if let Some(ref t) = meta.title {
        let _ = writeln!(out, "    <dc:title>{}</dc:title>", xml_escape_text(t));
    }
    if let Some(ref a) = meta.author {
        let _ = writeln!(out, "    <dc:creator>{}</dc:creator>", xml_escape_text(a));
    }
    if let Some(ref c) = meta.created {
        let _ = writeln!(out, "    <meta:creation-date>{}</meta:creation-date>", xml_escape_text(c));
    }
    if let Some(ref m) = meta.modified {
        let _ = writeln!(out, "    <dc:date>{}</dc:date>", xml_escape_text(m));
    }
}

// ── page layout / master page ───────────────────────────────

fn write_page_layout(out: &mut String, doc: &WriterDocument) {
    let _ = writeln!(out, r#"    <style:page-layout style:name="PM1">"#);
    // 注意：page-layout-properties 必须用显式 Start/End，不能自闭合——
    // parser 的 on_empty() 不转发这个标签名，自闭合会导致 page_size/margin 读不回来。
    let _ = writeln!(
        out,
        r#"      <style:page-layout-properties fo:page-width="{:.2}pt" fo:page-height="{:.2}pt" fo:margin-top="{:.2}pt" fo:margin-bottom="{:.2}pt" fo:margin-left="{:.2}pt" fo:margin-right="{:.2}pt"></style:page-layout-properties>"#,
        doc.page_size.0, doc.page_size.1, doc.margin_top, doc.margin_bottom, doc.margin_left, doc.margin_right
    );
    out.push_str("    </style:page-layout>\n");
}

/// WROOT-004: header/footer 现在是 Vec<Paragraph>，ODF 导出这条路径本身
/// 只支持纯文本 header/footer（.fodt 的 master-page 不是本次重点），
/// 因此拍平成纯文本，只取 default 槽位（even/first 在 ODF 导出场景下降级丢弃，
/// 这是本次改动前就有的行为的延续，不是新引入的信息损失）。
fn paragraphs_to_plain_text(paras: &[Paragraph]) -> String {
    paras.iter()
        .map(|p| p.runs.iter().map(|r| r.text.as_str()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn write_master_page(out: &mut String, doc: &WriterDocument) {
    let has_hf = doc.header.default.is_some() || doc.footer.default.is_some();
    if has_hf {
        let _ = writeln!(out, r#"    <style:master-page style:name="Standard" style:page-layout-name="PM1">"#);
        if let Some(ref h) = doc.header.default {
            let text = paragraphs_to_plain_text(h);
            let _ = writeln!(out, "      <style:header><text:p>{}</text:p></style:header>", xml_escape_text(&text));
        }
        if let Some(ref f) = doc.footer.default {
            let text = paragraphs_to_plain_text(f);
            let _ = writeln!(out, "      <style:footer><text:p>{}</text:p></style:footer>", xml_escape_text(&text));
        }
        out.push_str("    </style:master-page>\n");
    } else {
        let _ = writeln!(out, r#"    <style:master-page style:name="Standard" style:page-layout-name="PM1"></style:master-page>"#);
    }
}

// ── 已命名样式（StyleRegistry） ──────────────────────────────

fn write_named_styles(out: &mut String, styles: &StyleRegistry) {
    for (name, s) in &styles.paragraph_styles {
        write_paragraph_style_def(out, name, s);
    }
    for (name, s) in &styles.char_styles {
        write_char_style_def(out, name, s);
    }
    for (name, s) in &styles.cell_styles {
        write_cell_style_def(out, name, s);
    }
}

fn write_paragraph_style_def(out: &mut String, name: &str, s: &ParagraphStyle) {
    let parent_attr = s
        .based_on
        .as_ref()
        .map(|p| format!(r#" style:parent-style-name="{}""#, xml_escape_attr(p)))
        .unwrap_or_default();
    let _ = writeln!(
        out,
        r#"    <style:style style:name="{}" style:family="paragraph"{}>"#,
        xml_escape_attr(name), parent_attr
    );
    write_text_props_common(out, s.font_size, s.font_family.as_deref(), s.bold, s.italic, None);
    let mut pp = String::new();
    if let Some(a) = s.alignment {
        let _ = write!(pp, r#" fo:text-align="{}""#, alignment_to_str(a));
    }
    if let Some(lh) = s.line_height {
        let _ = write!(pp, r#" fo:line-height="{:.2}pt""#, lh);
    }
    if let Some(sb) = s.space_before {
        let _ = write!(pp, r#" fo:margin-top="{:.2}pt""#, sb);
    }
    if let Some(sa) = s.space_after {
        let _ = write!(pp, r#" fo:margin-bottom="{:.2}pt""#, sa);
    }
    if !pp.is_empty() {
        let _ = writeln!(out, "      <style:paragraph-properties{}></style:paragraph-properties>", pp);
    }
    out.push_str("    </style:style>\n");
}

fn write_char_style_def(out: &mut String, name: &str, s: &CharStyle) {
    let parent_attr = s
        .based_on
        .as_ref()
        .map(|p| format!(r#" style:parent-style-name="{}""#, xml_escape_attr(p)))
        .unwrap_or_default();
    let _ = writeln!(
        out,
        r#"    <style:style style:name="{}" style:family="text"{}>"#,
        xml_escape_attr(name), parent_attr
    );
    write_text_props_common(out, s.font_size, s.font_family.as_deref(), s.bold, s.italic, Some(s.underline));
    out.push_str("    </style:style>\n");
}

fn write_cell_style_def(out: &mut String, name: &str, s: &CellStyle) {
    // family="table-cell"：FODT parser 完全不读这个 family，纯单向，仅供外部软件参考
    let _ = writeln!(
        out,
        r#"    <style:style style:name="{}" style:family="table-cell">"#,
        xml_escape_attr(name)
    );
    let mut tp = String::new();
    if let Some(sz) = s.font_size {
        let _ = write!(tp, r#" fo:font-size="{:.1}pt""#, sz);
    }
    if s.bold {
        tp.push_str(r#" fo:font-weight="bold""#);
    }
    if !tp.is_empty() {
        let _ = writeln!(out, "      <style:text-properties{}></style:text-properties>", tp);
    }
    if let Some(a) = s.alignment {
        let _ = writeln!(out, r#"      <style:paragraph-properties fo:text-align="{}"></style:paragraph-properties>"#, alignment_to_str(a));
    }
    out.push_str("    </style:style>\n");
}

fn write_text_props_common(
    out: &mut String,
    font_size: Option<f32>,
    font_family: Option<&str>,
    bold: bool,
    italic: bool,
    underline: Option<bool>,
) {
    let mut tp = String::new();
    if let Some(sz) = font_size {
        let _ = write!(tp, r#" fo:font-size="{:.1}pt""#, sz);
    }
    if let Some(ff) = font_family {
        let _ = write!(tp, r#" fo:font-family="{}" style:font-name="{}""#, xml_escape_attr(ff), xml_escape_attr(ff));
    }
    if bold {
        tp.push_str(r#" fo:font-weight="bold""#);
    }
    if italic {
        tp.push_str(r#" fo:font-style="italic""#);
    }
    if let Some(u) = underline {
        let _ = write!(tp, r#" style:text-underline-style="{}""#, if u { "solid" } else { "none" });
    }
    if !tp.is_empty() {
        let _ = writeln!(out, "      <style:text-properties{}></style:text-properties>", tp);
    }
}

fn alignment_to_str(a: Alignment) -> &'static str {
    match a {
        Alignment::Left => "left",
        Alignment::Center => "center",
        Alignment::Right => "right",
        Alignment::Justify => "justify",
    }
}

// ── body ─────────────────────────────────────────────────────

fn write_body_element(out: &mut String, el: &BodyElement, auto: &mut String, counter: &mut AutoCounter) {
    match el {
        BodyElement::Paragraph(p) => write_paragraph(out, p, auto, counter),
        BodyElement::Table(t) => write_table(out, t, auto, counter),
        BodyElement::Section(s) => write_section(out, s, auto, counter),
        BodyElement::Image(img) => write_image(out, img),
        BodyElement::Frame(_) => {}, // 浮动框暂不写出
        BodyElement::Shape(_) => {},
        BodyElement::InlineStatChart { .. } => {}, // 统计图暂不导出
        BodyElement::InlineChart { .. } => {}, // DOCX-9：形状暂不写出（ODT导出未覆盖，仅打通导入模型层）
    }
}

fn write_paragraph(out: &mut String, p: &Paragraph, auto: &mut String, counter: &mut AutoCounter) {
    let has_override = p.alignment.is_some()
        || p.indent_left.is_some()
        || p.indent_first.is_some()
        || p.space_before.is_some()
        || p.space_after.is_some();

    let style_name = if has_override {
        let name = format!("P_auto_{}", counter.para);
        counter.para += 1;
        let parent_attr = if p.style.0.is_empty() {
            String::new()
        } else {
            format!(r#" style:parent-style-name="{}""#, xml_escape_attr(&p.style.0))
        };
        let _ = writeln!(auto, r#"    <style:style style:name="{}" style:family="paragraph"{}>"#, name, parent_attr);
        let mut pp = String::new();
        if let Some(a) = p.alignment {
            let _ = write!(pp, r#" fo:text-align="{}""#, alignment_to_str(a));
        }
        if let Some(il) = p.indent_left {
            let _ = write!(pp, r#" fo:margin-left="{:.2}pt""#, il); // 单向，parser 不读
        }
        if let Some(ifst) = p.indent_first {
            let _ = write!(pp, r#" fo:text-indent="{:.2}pt""#, ifst); // 单向，parser 不读
        }
        if let Some(sb) = p.space_before {
            let _ = write!(pp, r#" fo:margin-top="{:.2}pt""#, sb);
        }
        if let Some(sa) = p.space_after {
            let _ = write!(pp, r#" fo:margin-bottom="{:.2}pt""#, sa);
        }
        if !pp.is_empty() {
            let _ = writeln!(auto, "      <style:paragraph-properties{}></style:paragraph-properties>", pp);
        }
        auto.push_str("    </style:style>\n");
        name
    } else {
        p.style.0.clone()
    };

    // ⚠️ 不用 writeln!，标签和内容之间不留换行/空白——见文件头注释
    let _ = write!(out, r#"      <text:p text:style-name="{}">"#, xml_escape_attr(&style_name));
    for r in &p.runs {
        write_run(out, r, auto, counter);
    }
    out.push_str("</text:p>\n");
}

fn write_run(out: &mut String, r: &Run, auto: &mut String, counter: &mut AutoCounter) {
    let inline_fmt = r.bold.is_some() || r.italic.is_some() || r.underline.is_some()
        || r.font_size.is_some() || r.font_family.is_some();

    let style_name: Option<String> = if let Some(ref cs) = r.char_style {
        Some(cs.0.clone())
    } else if inline_fmt {
        let name = format!("T_auto_{}", counter.text);
        counter.text += 1;
        let _ = writeln!(auto, r#"    <style:style style:name="{}" style:family="text">"#, name);
        write_text_props_common(
            auto,
            r.font_size,
            r.font_family.as_deref(),
            r.bold.unwrap_or(false),
            r.italic.unwrap_or(false),
            r.underline,
        );
        auto.push_str("    </style:style>\n");
        Some(name)
    } else {
        None
    };

    let text = xml_escape_text(&r.text);

    if let Some(ref href) = r.hyperlink {
        let style_attr = style_name
            .as_ref()
            .map(|n| format!(r#" text:style-name="{}""#, n))
            .unwrap_or_default();
        let _ = write!(out, r#"<text:a xlink:href="{}"{}>{}</text:a>"#, xml_escape_attr(href), style_attr, text);
    } else if let Some(ref name) = style_name {
        let _ = write!(out, r#"<text:span text:style-name="{}">{}</text:span>"#, name, text);
    } else {
        out.push_str(&text);
    }
}

fn write_table(out: &mut String, t: &Table, auto: &mut String, counter: &mut AutoCounter) {
    out.push_str("      <table:table>\n");
    for w in &t.col_widths {
        // parser 端宽度固定回填 60.0，这里写真实值仅供外部软件用，单向
        let _ = writeln!(out, r#"        <table:table-column table:column-width="{:.2}pt"></table:table-column>"#, w);
    }
    for row in &t.rows {
        out.push_str("        <table:table-row>\n");
        for cell in &row.cells {
            write_table_cell(out, cell, auto, counter);
        }
        out.push_str("        </table:table-row>\n");
    }
    out.push_str("      </table:table>\n");
}

fn write_table_cell(out: &mut String, cell: &TableCell, auto: &mut String, counter: &mut AutoCounter) {
    let mut span = String::new();
    if cell.col_span > 1 {
        let _ = write!(span, r#" table:number-columns-spanned="{}""#, cell.col_span);
    }
    if cell.row_span > 1 {
        let _ = write!(span, r#" table:number-rows-spanned="{}""#, cell.row_span);
    }
    let style_attr = cell
        .style
        .as_ref()
        .map(|s| format!(r#" table:style-name="{}""#, xml_escape_attr(&s.0)))
        .unwrap_or_default();

    let _ = writeln!(out, r#"          <table:table-cell{}{}>"#, span, style_attr);
    for el in &cell.content {
        write_cell_content(out, el, auto, counter);
    }
    out.push_str("          </table:table-cell>\n");
}

/// 写出单元格内容（CellContent：段落/图片/嵌套表格三种，收窄自 BodyElement）
fn write_cell_content(out: &mut String, el: &roe_model::common::CellContent, auto: &mut String, counter: &mut AutoCounter) {
    match el {
        roe_model::common::CellContent::Paragraph(p) => write_paragraph(out, p, auto, counter),
        roe_model::common::CellContent::Image { image, width_pt, height_pt } => write_image(out, &InlineImage { image: image.clone(), width_pt: *width_pt, height_pt: *height_pt, ..Default::default() }),
        roe_model::common::CellContent::Table(t) => write_table(out, t, auto, counter),
    }
}

fn write_section(out: &mut String, s: &Section, auto: &mut String, counter: &mut AutoCounter) {
    let _ = writeln!(out, r#"      <text:section text:name="{}">"#, xml_escape_attr(&s.name));
    for el in &s.content {
        write_body_element(out, el, auto, counter);
    }
    out.push_str("      </text:section>\n");
}

fn write_image(out: &mut String, img: &InlineImage) {
    let _ = writeln!(
        out,
        r#"      <draw:frame svg:width="{:.2}pt" svg:height="{:.2}pt"><draw:image><office:binary-data>{}</office:binary-data></draw:image></draw:frame>"#,
        img.width_pt, img.height_pt,
        base64::engine::general_purpose::STANDARD.encode(&img.image.data)
    );
}

// ── XML 转义 ─────────────────────────────────────────────────

fn xml_escape_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn xml_escape_attr(s: &str) -> String {
    xml_escape_text(s).replace('"', "&quot;")
}

// ── 测试 ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::odf::fodt::FodtParser;
    use roe_model::writer::TableRow;
    use roe_model::common::{StyleRef, CellContent};
    use roe_model::style::ParagraphStyle;

    #[test]
    fn test_roundtrip_basic_doc() {
        let mut doc = WriterDocument::default();
        doc.metadata.title = Some("测试文档".into());
        doc.metadata.author = Some("ROE".into());
        doc.body.push(BodyElement::Paragraph(Paragraph {
            style: StyleRef("Default".into()),
            runs: vec![Run { text: "Hello ROE".into(), ..Default::default() }],
            ..Default::default()
        }));

        let xml = FodtWriter::write(&doc);
        let parsed = FodtParser::parse(&xml).expect("roundtrip parse failed");

        assert_eq!(parsed.metadata.title.as_deref(), Some("测试文档"));
        assert_eq!(parsed.metadata.author.as_deref(), Some("ROE"));
        assert_eq!(parsed.page_size.0 as i32, doc.page_size.0 as i32);
        assert_eq!(parsed.margin_top as i32, doc.margin_top as i32);
        assert_eq!(parsed.body.len(), 1);
        if let BodyElement::Paragraph(p) = &parsed.body[0] {
            let text: String = p.runs.iter().map(|r| r.text.as_str()).collect();
            assert_eq!(text, "Hello ROE");
        } else {
            panic!("expected paragraph");
        }
    }

    #[test]
    fn test_roundtrip_named_paragraph_style() {
        let mut doc = WriterDocument::default();
        doc.styles.paragraph_styles.insert(
            "Heading1".into(),
            ParagraphStyle { name: "Heading1".into(), font_size: Some(18.0), bold: true, ..Default::default() },
        );
        doc.body.push(BodyElement::Paragraph(Paragraph {
            style: StyleRef("Heading1".into()),
            runs: vec![Run { text: "标题".into(), ..Default::default() }],
            ..Default::default()
        }));

        let xml = FodtWriter::write(&doc);
        let parsed = FodtParser::parse(&xml).expect("roundtrip parse failed");
        let resolved = parsed.styles.resolve_paragraph_style("Heading1");
        assert_eq!(resolved.font_size, Some(18.0));
        assert!(resolved.bold);
    }

    #[test]
    fn test_roundtrip_run_inline_formatting() {
        let mut doc = WriterDocument::default();
        doc.body.push(BodyElement::Paragraph(Paragraph {
            style: StyleRef("Default".into()),
            runs: vec![Run { text: "粗体文字".into(), bold: Some(true), font_size: Some(14.0), ..Default::default() }],
            ..Default::default()
        }));

        let xml = FodtWriter::write(&doc);
        assert!(xml.contains(r#"style:family="text""#));
        assert!(xml.contains(r#"fo:font-weight="bold""#));

        let parsed = FodtParser::parse(&xml).expect("roundtrip parse failed");
        let BodyElement::Paragraph(p) = &parsed.body[0] else { panic!("expected paragraph") };
        let style_name = p.runs[0].char_style.as_ref().expect("char_style should be set").0.clone();
        let resolved = parsed.styles.resolve_char_style(&style_name);
        assert!(resolved.bold);
        assert_eq!(resolved.font_size, Some(14.0));
    }

    #[test]
    fn test_roundtrip_table() {
        let mut doc = WriterDocument::default();
        doc.body.push(BodyElement::Table(Table {
            col_widths: vec![100.0, 100.0],
            style_id: None, first_row: false, first_col: false, last_row: false, last_col: false, band_row: false, band_col: false,
            default_border_color: None, default_border_width: None,
            inside_h_color: None, inside_h_width: None, inside_v_color: None, inside_v_width: None,
            float_pos: None,
            rows: vec![TableRow {
                row_height: None,
                cells: vec![
                    TableCell {
                        content: vec![CellContent::Paragraph(Paragraph {
                            style: StyleRef("Default".into()),
                            runs: vec![Run { text: "A1".into(), ..Default::default() }],
                            ..Default::default()
                        })],
                        col_span: 1, row_span: 1, style: None, hidden: false, bg_color: None,
                        border_color: None, border_width: None,
                        pad_left: None, pad_right: None, pad_top: None, pad_bottom: None,
                    },
                    TableCell {
                        content: vec![CellContent::Paragraph(Paragraph {
                            style: StyleRef("Default".into()),
                            runs: vec![Run { text: "B1".into(), ..Default::default() }],
                            ..Default::default()
                        })],
                        col_span: 2, row_span: 1, style: None, hidden: false, bg_color: None,
                        border_color: None, border_width: None,
                        pad_left: None, pad_right: None, pad_top: None, pad_bottom: None,
                    },
                ],
            }],
        }));

        let xml = FodtWriter::write(&doc);
        assert!(xml.contains(r#"table:number-columns-spanned="2""#));

        let parsed = FodtParser::parse(&xml).expect("roundtrip parse failed");
        assert_eq!(parsed.body.len(), 1);
        if let BodyElement::Table(t) = &parsed.body[0] {
            assert_eq!(t.rows.len(), 1);
            assert_eq!(t.rows[0].cells.len(), 2);
            assert_eq!(t.rows[0].cells[1].col_span, 2);
        } else {
            panic!("expected table");
        }
    }

    #[test]
    fn test_hyperlink_footnote_section_image_one_way() {
        let mut doc = WriterDocument::default();
        doc.body.push(BodyElement::Paragraph(Paragraph {
            style: StyleRef("Default".into()),
            runs: vec![Run {
                text: "link".into(),
                hyperlink: Some("https://example.com".into()),
                ..Default::default()
            }],
            ..Default::default()
        }));
        doc.body.push(BodyElement::Section(Section {
            name: "Sec1".into(),
            content: vec![BodyElement::Paragraph(Paragraph {
                style: StyleRef("Default".into()),
                runs: vec![Run { text: "in section".into(), ..Default::default() }],
                ..Default::default()
            })],
            ..Default::default()
        }));
        doc.body.push(BodyElement::Image(InlineImage { image: roe_model::common::ImageData { data: vec![65, 66, 67], mime: "image/png".to_string(), ..Default::default() }, width_pt: 50.0, height_pt: 50.0, ..Default::default() }));

        let xml = FodtWriter::write(&doc);
        assert!(xml.contains(r#"<text:a xlink:href="https://example.com""#));
        assert!(xml.contains(r#"<text:section text:name="Sec1">"#));
        assert!(xml.contains("draw:frame"));
        assert!(xml.contains("QUJD"));

        let parsed = FodtParser::parse(&xml).expect("roundtrip parse failed");
        let all_text: String = parsed.body.iter().filter_map(|b| {
            if let BodyElement::Paragraph(p) = b { Some(p.runs.iter().map(|r| r.text.as_str()).collect::<String>()) } else { None }
        }).collect();
        assert!(all_text.contains("in section"));
    }

    // ─── 真实文件 round-trip 测试（ODF §5.2 阶段三，v186后续）────────────

    /// 比较两个 BodyElement 序列在 writer 支持范围内的字段：Paragraph 比
    /// style + runs[].text；Table 递归比较 cell 内容（Paragraph 部分），
    /// 不比较 col_widths（parser 固定回填 60.0，已知限制，见文件头注释）；
    /// Section 展开比较内部 content（parser 会拍扁包裹结构，只验证内容
    /// 不丢，不验证 Section 包裹本身被保留）；Frame/Shape/InlineChart 不
    /// 比较内容，只确认"被跳过"符合预期（writer 完全不写出，已用代码
    /// 交叉核实，见 scan_odt_roundtrip_candidates.rs 头部注释）。
    fn assert_odt_body_match_within_writer_scope(
        expected: &[BodyElement],
        actual: &[BodyElement],
        path: &str,
    ) {
        fn flatten_paragraphs<'a>(body: &'a [BodyElement], out: &mut Vec<&'a Paragraph>) {
            for elem in body {
                match elem {
                    BodyElement::Paragraph(p) => out.push(p),
                    BodyElement::Section(s) => flatten_paragraphs(&s.content, out),
                    BodyElement::Table(t) => {
                        for row in &t.rows {
                            for cell in &row.cells {
                                for c in &cell.content {
                                    if let CellContent::Paragraph(p) = c {
                                        out.push(p);
                                    }
                                }
                            }
                        }
                    }
                    // Frame/Shape/InlineChart/Image: 不贡献 Paragraph，跳过
                    _ => {}
                }
            }
        }

        let mut exp_paras = Vec::new();
        flatten_paragraphs(expected, &mut exp_paras);
        let mut act_paras = Vec::new();
        flatten_paragraphs(actual, &mut act_paras);

        assert_eq!(
            exp_paras.len(), act_paras.len(),
            "{path}: 展开后的 paragraph 数量不一致（含 Table/Section 内嵌段落）"
        );

        for (i, (exp, act)) in exp_paras.iter().zip(act_paras.iter()).enumerate() {
            let p_path = format!("{path} paragraph[{i}]");
            assert_eq!(
                exp.style.0, act.style.0,
                "{p_path}: style 不一致 exp={:?} act={:?}", exp.style.0, act.style.0
            );
            let exp_text: String = exp.runs.iter().map(|r| r.text.as_str()).collect();
            let act_text: String = act.runs.iter().map(|r| r.text.as_str()).collect();
            assert_eq!(
                exp_text.trim(), act_text.trim(),
                "{p_path}: 拼接后的文字内容不一致\nexp={exp_text:?}\nact={act_text:?}"
            );
        }

        // Table 数量应一致（顶层 + Section 内，不含嵌套表格递归展开，
        // 简化起见只统计顶层 body 里的 Table 数）
        let exp_table_count = expected.iter().filter(|e| matches!(e, BodyElement::Table(_))).count();
        let act_table_count = actual.iter().filter(|e| matches!(e, BodyElement::Table(_))).count();
        assert_eq!(
            exp_table_count, act_table_count,
            "{path}: 顶层 Table 数量不一致 exp={exp_table_count} act={act_table_count}"
        );
    }

    fn run_real_file_roundtrip_odt(path: &str) {
        let data = std::fs::read(path)
            .unwrap_or_else(|e| panic!("读取样本文件失败 {path}: {e}"));

        let expected = if path.ends_with(".fodt") {
            crate::RoeDocument::open_fodt_bytes(&data)
        } else {
            crate::RoeDocument::open_odf_bytes(&data)
        }
        .unwrap_or_else(|e| panic!("首次 parse 失败 {path}: {e:?}"));
        let expected = match expected {
            crate::RoeDocument::Writer(w) => w,
            other => panic!("{path}: 不是 Writer 文档 ({other:?} variant)"),
        };

        let xml = FodtWriter::write(&expected);
        let actual = FodtParser::parse(&xml)
            .unwrap_or_else(|e| panic!("write→reparse 失败 {path}: {e:?}"));

        assert_odt_body_match_within_writer_scope(&expected.body, &actual.body, path);
    }

    #[test]
    fn test_real_file_roundtrip_tdf142847() {
        // 探测脚本 scan_odt_roundtrip_candidates 挑选：1 paragraph / 41
        // runs，全 clean（无 table/section/frame/shape/chart），高 run
        // 密度样本，主要验证大量 run 拼接场景。
        run_real_file_roundtrip_odt(
            "/home/yong/Projects/roe/test_samples/curated/odt/small__sw_uitest_data__tdf142847.fodt",
        );
    }

    #[test]
    fn test_real_file_roundtrip_tdf119081() {
        // 探测脚本挑选：1 paragraph + 2 tables，覆盖 Table 写出路径
        // （writer 有真实实现，本测试验证 cell 内 paragraph 内容可
        // roundtrip，不验证 col_widths，因 parser 已知固定回填 60.0）。
        run_real_file_roundtrip_odt(
            "/home/yong/Projects/roe/test_samples/curated/odt/supp_qa_core_unocore_data__tdf119081.odt",
        );
    }
}
