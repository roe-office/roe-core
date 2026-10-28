//! FODT（ODF Flat Text）解析器

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use base64::Engine;

use crate::ParseError;
use roe_model::common::{StyleRef};
use roe_model::style::{Alignment, CharStyle, ParagraphStyle};
use roe_model::common::Color;
use roe_model::writer::ScriptPosition;
use roe_model::writer::{
    BodyElement, ListInfo, Paragraph, Run, Table, TableCell, TableRow, WriterDocument,
};

pub struct FodtParser;

impl FodtParser {
    pub fn parse(xml: &str) -> Result<WriterDocument, ParseError> {
        let mut ctx = ParseCtx::default();
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(false);
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) => ctx.on_start(e)?,
                Ok(Event::Empty(ref e)) => ctx.on_empty(e)?,
                Ok(Event::End(ref e))   => ctx.on_end(e.local_name().as_ref()),
                Ok(Event::Text(ref e))  => {
                    ctx.on_text(e.unescape().unwrap_or_default().into_owned());
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(ParseError::Xml(e)),
                _ => {}
            }
            buf.clear();
        }
        Ok(ctx.finish())
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn attr(e: &BytesStart, name: &[u8]) -> Option<String> {
    e.attributes()
        .filter_map(|a| a.ok())
        .find(|a| a.key.as_ref() == name)
        .and_then(|a| String::from_utf8(a.value.to_vec()).ok())
}

fn attr_local(e: &BytesStart, local_name: &[u8]) -> Option<String> {
    e.attributes()
        .filter_map(|a| a.ok())
        .find(|a| a.key.local_name().as_ref() == local_name)
        .and_then(|a| String::from_utf8(a.value.to_vec()).ok())
}

fn parse_length_pt(s: &str) -> Option<f32> {
    let s = s.trim();
    if let Some(v) = s.strip_suffix("pt") { return v.trim().parse().ok(); }
    if let Some(v) = s.strip_suffix("cm") { return v.trim().parse::<f32>().ok().map(|x| x * 28.3465); }
    if let Some(v) = s.strip_suffix("mm") { return v.trim().parse::<f32>().ok().map(|x| x * 2.83465); }
    if let Some(v) = s.strip_suffix("in") { return v.trim().parse::<f32>().ok().map(|x| x * 72.0); }
    if let Some(v) = s.strip_suffix("px") { return v.trim().parse::<f32>().ok().map(|x| x * 0.75); }
    // 纯数字当 pt
    s.parse().ok()
}

fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 { return None; }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color { r, g, b, a: 255 })
}

fn parse_script_position(s: &str) -> ScriptPosition {
    let s = s.trim();
    if s.starts_with("super") { ScriptPosition::Superscript }
    else if s.starts_with("sub") { ScriptPosition::Subscript }
    else { ScriptPosition::Normal }
}


// ── state ────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct ParseCtx {
    doc: WriterDocument,

    in_styles: bool,
    in_body:   bool,
    in_meta:   bool,
    meta_field: MetaField,

    // 当前正在构建的样式
    cur_para_style: Option<ParagraphStyle>,
    cur_char_style: Option<CharStyle>,

    // 段落 / run 栈
    para_stack:  Vec<ParaBuilder>,
    cur_run:     Option<RunBuilder>,

    // 表格栈
    table_stack: Vec<TableBuilder>,
    // 图片解析状态
    in_draw_frame: bool,
    in_draw_image: bool,
    in_binary_data: bool,
    cur_image_mime: String,
    cur_image_w_pt: f32,
    cur_image_h_pt: f32,
    cur_image_x_pt: f32,
    cur_image_y_pt: f32,
    cur_frame_is_float: bool,
    cur_binary_data: String,

    // 列表样式解析状态（text:list-style / text:list-level-style-*）
    cur_list_style: Option<roe_model::writer::ListStyle>,
    cur_list_level_num: Option<u8>,
    cur_list_level_indent: roe_model::writer::ListIndent,
    cur_list_level_prefix: String,
    cur_list_level_suffix: String,
    // 最外层 <text:list> 的 text:style-name，作为真正的 list 实例标识，
    // 同时 Vec 长度即为当前嵌套深度，决定段落的 level。
    cur_list_id_stack: Vec<String>,
}

#[derive(Default, PartialEq)]
enum MetaField { #[default] None, Title, Author, Created, Modified }

#[derive(Default)]
struct ParaBuilder {
    style_name: String,
    runs: Vec<Run>,
    list_id: Option<String>,
    list_level: Option<u8>,
    list_style_name: Option<String>,
}

#[derive(Default)]
struct RunBuilder {
    text: String,
    style_name: Option<String>,
}

#[derive(Default)]
struct TableBuilder {
    rows: Vec<TableRow>,
    col_widths: Vec<f32>,
    cur_row: Option<Vec<TableCell>>,
    cur_cell: Option<CellBuilder>,
}

#[derive(Default)]
struct CellBuilder {
    content: Vec<BodyElement>,
    col_span: u8,
    row_span: u8,
    style: Option<String>,
}

// ── event handlers ───────────────────────────────────────────────────────────

impl ParseCtx {
    fn on_start(&mut self, e: &BytesStart) -> Result<(), ParseError> {
        // local_name() 已去掉命名空间前缀
        let tag = std::str::from_utf8(e.local_name().as_ref())
            .unwrap_or("").to_string();

        // DEBUG
        match tag.as_str() {
            // 区域
            "styles" | "automatic-styles" | "master-styles" => {
                self.in_styles = true;
            }
            "body" => {
                self.in_styles = false;
                self.in_body = true;
            }
            "meta" => {
                self.in_meta = true;
            }

            // 元信息字段
            "title"         if self.in_meta => self.meta_field = MetaField::Title,
            "creator"       if self.in_meta => self.meta_field = MetaField::Author,
            "date"          if self.in_meta => self.meta_field = MetaField::Modified,
            "creation-date" if self.in_meta => self.meta_field = MetaField::Created,

            // 样式定义
            "style" if self.in_styles => {
                let name   = attr(e, b"style:name").unwrap_or_default();
                let family = attr(e, b"style:family").unwrap_or_default();
                let parent = attr(e, b"style:parent-style-name");
                if family == "paragraph" {
                    let mut s = ParagraphStyle::default();
                    s.name = name;
                    s.based_on = parent;
                    s.list_style_name = attr(e, b"style:list-style-name");
                    s.list_level = attr(e, b"style:list-level").and_then(|v| v.parse::<u8>().ok()).map(|v| v.saturating_sub(1));
                    self.cur_para_style = Some(s);
                } else if family == "text" {
                    let mut s = CharStyle::default();
                    s.name = name;
                    s.based_on = parent;
                    self.cur_char_style = Some(s);
                }
            }

            // 页面尺寸（fo:page-width / fo:page-height，单位 cm，转换为 pt）
            "page-layout-properties" => {
                fn cm_to_pt(s: &str) -> Option<f32> {
                    let s = s.trim();
                    if let Some(v) = s.strip_suffix("cm") {
                        v.trim().parse::<f32>().ok().map(|x| x * 28.3465)
                    } else if let Some(v) = s.strip_suffix("pt") {
                        v.trim().parse::<f32>().ok()
                    } else if let Some(v) = s.strip_suffix("mm") {
                        v.trim().parse::<f32>().ok().map(|x| x * 2.83465)
                    } else if let Some(v) = s.strip_suffix("in") {
                        v.trim().parse::<f32>().ok().map(|x| x * 72.0)
                    } else {
                        None
                    }
                }
                if let (Some(w), Some(h)) = (
                    attr(e, b"fo:page-width").as_deref().and_then(cm_to_pt),
                    attr(e, b"fo:page-height").as_deref().and_then(cm_to_pt),
                ) {
                    self.doc.page_size = (w, h);
                }
                // 页边距
                if let Some(v) = attr(e, b"fo:margin-top")    .as_deref().and_then(cm_to_pt) { self.doc.margin_top    = v; }
                if let Some(v) = attr(e, b"fo:margin-bottom")  .as_deref().and_then(cm_to_pt) { self.doc.margin_bottom = v; }
                if let Some(v) = attr(e, b"fo:margin-left")    .as_deref().and_then(cm_to_pt) { self.doc.margin_left   = v; }
                if let Some(v) = attr(e, b"fo:margin-right")   .as_deref().and_then(cm_to_pt) { self.doc.margin_right  = v; }
            }
            // 段落格式属性
            "paragraph-properties" if self.cur_para_style.is_some() => {
                let s = self.cur_para_style.as_mut().unwrap();
                if let Some(v) = attr(e, b"fo:text-align") {
                    s.alignment = Some(match v.as_str() {
                        "center"        => Alignment::Center,
                        "end" | "right" => Alignment::Right,
                        "justify"       => Alignment::Justify,
                        _               => Alignment::Left,
                    });
                }
                if let Some(v) = attr(e, b"fo:line-height")    { s.line_height  = parse_length_pt(&v); }
                if let Some(v) = attr(e, b"fo:margin-top")     { s.space_before = parse_length_pt(&v); }
                if let Some(v) = attr(e, b"fo:margin-bottom")  { s.space_after  = parse_length_pt(&v); }
                if let Some(v) = attr(e, b"fo:margin-left")    { s.indent_left  = parse_length_pt(&v); }
                if let Some(v) = attr(e, b"fo:text-indent")    { s.indent_first = parse_length_pt(&v); }
            }

            // 文字格式属性（段落样式）
            "text-properties" if self.cur_para_style.is_some() => {
                let s = self.cur_para_style.as_mut().unwrap();
                if let Some(v) = attr(e, b"fo:font-size") {
                    s.font_size = parse_length_pt(&v);
                }
                if let Some(v) = attr(e, b"fo:font-family")
                    .or_else(|| attr(e, b"style:font-name")) {
                    s.font_family = Some(v);
                }
                if attr(e, b"fo:font-weight").as_deref() == Some("bold")   { s.bold   = true; }
                if attr(e, b"fo:font-style").as_deref()  == Some("italic") { s.italic = true; }
            }

            // 文字格式属性（char style / text:span 对应）
            "text-properties" if self.cur_char_style.is_some() => {
                let s = self.cur_char_style.as_mut().unwrap();
                if let Some(v) = attr(e, b"fo:font-size") {
                    s.font_size = parse_length_pt(&v);
                }
                if let Some(v) = attr(e, b"fo:font-family")
                    .or_else(|| attr(e, b"style:font-name")) {
                    s.font_family = Some(v);
                }
                if attr(e, b"fo:font-weight").as_deref() == Some("bold")   { s.bold   = true; }
                if attr(e, b"fo:font-style").as_deref()  == Some("italic") { s.italic = true; }
                if let Some(v) = attr(e, b"fo:color") {
                    s.color = parse_color(&v);
                }
                if let Some(v) = attr(e, b"fo:background-color") {
                    s.background = parse_color(&v);
                }
                if let Some(v) = attr(e, b"style:text-underline-style") {
                    s.underline = v != "none";
                }
                if let Some(v) = attr(e, b"style:text-line-through-style") {
                    s.strikethrough = Some(v != "none");
                }
                if let Some(v) = attr(e, b"style:text-position") {
                    s.script = Some(parse_script_position(&v));
                }
            }

            // 段落 / 标题
            "p" | "h" if self.in_body => {
                let style_name = attr(e, b"text:style-name").unwrap_or_default();
                self.para_stack.push(ParaBuilder {
                    style_name,
                    runs: Vec::new(),
                    list_id: None,
                    list_level: None,
                    list_style_name: None,
                });
            }

            // span
            "span" if self.in_body => {
                self.cur_run = Some(RunBuilder {
                    text: String::new(),
                    style_name: attr(e, b"text:style-name"),
                });
            }

            // 列表
            "frame" if self.in_body => {
                self.in_draw_frame = true;
                // svg:width/svg:height/svg:x/svg:y（带命名空间前缀，attr_local 取本地名）
                let w_str = attr_local(e, b"width").unwrap_or_default();
                let h_str = attr_local(e, b"height").unwrap_or_default();
                let x_str = attr_local(e, b"x").unwrap_or_default();
                let y_str = attr_local(e, b"y").unwrap_or_default();
                self.cur_image_w_pt = parse_length_pt(&w_str).unwrap_or(100.0);
                self.cur_image_h_pt = parse_length_pt(&h_str).unwrap_or(50.0);
                self.cur_image_x_pt = parse_length_pt(&x_str).unwrap_or(0.0);
                self.cur_image_y_pt = parse_length_pt(&y_str).unwrap_or(0.0);
                // anchor-type="char"/"paragraph"/"frame" 视为浮动；
                // 也兜底：有明确 x/y 坐标也视为浮动
                let anchor = attr_local(e, b"anchor-type").unwrap_or_default();
                self.cur_frame_is_float = matches!(anchor.as_str(), "char" | "paragraph" | "frame")
                    || !x_str.is_empty() || !y_str.is_empty();
            }
            "image" if self.in_draw_image || self.in_draw_frame => {
                self.in_draw_image = true;
                self.cur_image_mime = attr(e, b"mime-type").unwrap_or_else(|| "image/png".to_string());
                self.cur_binary_data.clear();
            }
            "binary-data" if self.in_draw_image => {
                self.in_binary_data = true;
            }
            "list" if self.in_body => {
                let style_name = attr(e, b"text:style-name");
                // 最外层 <text:list> 的 style-name 作为整个列表实例的标识，
                // 嵌套的 <text:list>（子级）可能不重复携带 style-name，
                // 此时沿用最外层的；level 由嵌套深度决定（0-based）。
                let id = style_name.clone().unwrap_or_else(|| {
                    self.cur_list_id_stack.last().cloned().unwrap_or_default()
                });
                self.cur_list_id_stack.push(id);
                // 后续 on_empty/on_start 处理 <text:list-item> 内的 <text:p>
                // 时，用 list_nest_depth - 1 作为 level（最外层 list 对应 level 0）
            }
            "list-item" if self.in_body => {
                // list-item 本身不产生段落，仅作为容器；level 在 list 标签
                // 嵌套时已确定，这里不需要额外动作。
            }
            "list-style" => {
                let name = attr(e, b"style:name").unwrap_or_default();
                self.cur_list_style = Some(roe_model::writer::ListStyle { name, levels: Vec::new() });
            }
            "list-level-style-number" if self.cur_list_style.is_some() => {
                self.cur_list_level_num = attr(e, b"text:level").and_then(|v| v.parse::<u8>().ok());
                self.cur_list_level_prefix = attr(e, b"style:num-prefix").unwrap_or_default();
                self.cur_list_level_suffix = attr(e, b"style:num-suffix").unwrap_or_else(|| ".".to_string());
                self.cur_list_level_indent = roe_model::writer::ListIndent::default();
                let fmt = match attr(e, b"style:num-format").as_deref() {
                    Some("1") => roe_model::writer::NumFormat::Arabic,
                    Some("a") => roe_model::writer::NumFormat::LowerAlpha,
                    Some("A") => roe_model::writer::NumFormat::UpperAlpha,
                    Some("i") => roe_model::writer::NumFormat::LowerRoman,
                    Some("I") => roe_model::writer::NumFormat::UpperRoman,
                    _ => roe_model::writer::NumFormat::None,
                };
                let start_value = attr(e, b"text:start-value").and_then(|v| v.parse().ok()).unwrap_or(1);
                let display_levels = attr(e, b"text:display-levels").and_then(|v| v.parse().ok()).unwrap_or(1);
                if let Some(ls) = self.cur_list_style.as_mut() {
                    let level = roe_model::writer::NumberListLevel {
                        format: fmt,
                        prefix: self.cur_list_level_prefix.clone(),
                        suffix: self.cur_list_level_suffix.clone(),
                        start_value,
                        display_levels,
                        indent: self.cur_list_level_indent,
                    };
                    ls.levels.push(roe_model::writer::ListLevelStyle::Number(level));
                }
            }
            "list-level-style-bullet" if self.cur_list_style.is_some() => {
                self.cur_list_level_num = attr(e, b"text:level").and_then(|v| v.parse::<u8>().ok());
                let bullet_char = attr(e, b"text:bullet-char").unwrap_or_else(|| "•".to_string());
                let prefix = attr(e, b"style:num-prefix").unwrap_or_default();
                let suffix = attr(e, b"style:num-suffix").unwrap_or_default();
                if let Some(ls) = self.cur_list_style.as_mut() {
                    let level = roe_model::writer::BulletListLevel {
                        bullet_char,
                        prefix,
                        suffix,
                        indent: roe_model::writer::ListIndent::default(),
                    };
                    ls.levels.push(roe_model::writer::ListLevelStyle::Bullet(level));
                }
            }
            "list-level-properties" if self.cur_list_style.is_some() => {
                // 缩进信息合并进最后一个刚 push 的 level
                let margin_left = attr(e, b"fo:margin-left").as_deref().and_then(parse_length_pt);
                let text_indent = attr(e, b"fo:text-indent").as_deref().and_then(parse_length_pt);
                let min_label_width = attr(e, b"text:min-label-width").as_deref().and_then(parse_length_pt);
                if let Some(ls) = self.cur_list_style.as_mut() {
                    if let Some(last) = ls.levels.last_mut() {
                        let indent = match last {
                            roe_model::writer::ListLevelStyle::Number(n) => &mut n.indent,
                            roe_model::writer::ListLevelStyle::Bullet(b) => &mut b.indent,
                        };
                        if let Some(v) = margin_left { indent.margin_left = v; }
                        if let Some(v) = text_indent { indent.text_indent = v; }
                        if let Some(v) = min_label_width { indent.min_label_width = v; }
                    }
                }
            }

            // 表格
            "table" if self.in_body => {
                self.table_stack.push(TableBuilder::default());
            }
            "table-column" if !self.table_stack.is_empty() => {
                if let Some(tb) = self.table_stack.last_mut() {
                    tb.col_widths.push(60.0);
                }
            }
            "table-row" if !self.table_stack.is_empty() => {
                if let Some(tb) = self.table_stack.last_mut() {
                    tb.cur_row = Some(Vec::new());
                }
            }
            "table-cell" | "covered-table-cell" if !self.table_stack.is_empty() => {
                let col_span = attr(e, b"table:number-columns-spanned")
                    .and_then(|v| v.parse().ok()).unwrap_or(1);
                let row_span = attr(e, b"table:number-rows-spanned")
                    .and_then(|v| v.parse().ok()).unwrap_or(1);
                let style = attr(e, b"table:style-name");
                if let Some(tb) = self.table_stack.last_mut() {
                    tb.cur_cell = Some(CellBuilder { content: Vec::new(), col_span, row_span, style });
                }
            }

            _ => {}
        }
        Ok(())
    }

    fn on_empty(&mut self, e: &BytesStart) -> Result<(), ParseError> {
        let tag = std::str::from_utf8(e.local_name().as_ref())
            .unwrap_or("").to_string();
        // 样式属性标签通常是 Empty，转发给 on_start 统一处理
        if matches!(tag.as_str(),
            "text-properties" | "paragraph-properties" | "style"
            | "list-style" | "list-level-style-number" | "list-level-style-bullet"
            | "list-level-properties"
        ) {
            self.on_start(e)?;
            // Empty 标签没有对应的 End 事件，list-style 和自闭合的
            // <style:style .../>（无 paragraph-properties 子元素时）
            // 需要在这里立即收尾，否则永远不会被提交进 doc.styles。
            if tag == "list-style" {
                self.on_end(b"list-style");
            }
            if tag == "style" {
                self.on_end(b"style");
            }
            return Ok(());
        }
        match tag.as_str() {
            "tab"        if self.in_body => self.push_text("\t".to_string()),
            "line-break" if self.in_body => self.push_text("\n".to_string()),
            "s" if self.in_body => {
                let n: usize = attr(e, b"text:c")
                    .and_then(|v| v.parse().ok()).unwrap_or(1);
                self.push_text(" ".repeat(n));
            }
            _ => {}
        }
        Ok(())
    }

    fn on_end(&mut self, local: &[u8]) {
        let tag = std::str::from_utf8(local).unwrap_or("");
        match tag {
            "styles" | "automatic-styles" | "master-styles" => {
                self.in_styles = false;
            }
            "list" => {
                self.cur_list_id_stack.pop();
            }
            "list-style" => {
                if let Some(ls) = self.cur_list_style.take() {
                    self.doc.styles.list_styles.insert(ls.name.clone(), ls);
                }
            }
            "meta" => {
                self.in_meta = false;
                self.meta_field = MetaField::None;
            }
            "title" | "creator" | "date" | "creation-date" => {
                self.meta_field = MetaField::None;
            }

            // 提交样式
            "style" => {
                if let Some(s) = self.cur_para_style.take() {
                    self.doc.styles.paragraph_styles.insert(s.name.clone(), s);
                }
                if let Some(s) = self.cur_char_style.take() {
                    self.doc.styles.char_styles.insert(s.name.clone(), s);
                }
            }

            // 提交 span
            "span" => {
                if let Some(rb) = self.cur_run.take() {
                    if !rb.text.is_empty() {
                        let run = Run { text: rb.text, char_style: rb.style_name.map(StyleRef), ..Default::default() };
                        if let Some(pb) = self.para_stack.last_mut() {
                            pb.runs.push(run);
                        }
                    }
                }
            }

            // 提交段落
            "p" | "h" => {
                self.flush_run();
                if let Some(pb) = self.para_stack.pop() {
                    // level = 当前 <text:list> 嵌套深度 - 1（最外层 list 对应 level 0）。
                    // 只有段落确实处于某个 <text:list> 内部时才生成 list_info。
                    let list_info = if !self.cur_list_id_stack.is_empty() {
                        let id = self.cur_list_id_stack.last().cloned().unwrap_or_default();
                        let level = (self.cur_list_id_stack.len() as u8).saturating_sub(1);
                        Some(ListInfo {
                            list_id: id.clone(),
                            level,
                            style_name: id,
                        })
                    } else {
                        // 不在 <text:list> 内：检查段落样式是否自带
                        // style:list-style-name（outline/heading 自动编号场景）
                        self.doc.styles.paragraph_styles.get(&pb.style_name).and_then(|ps| {
                            ps.list_style_name.as_ref().map(|sn| ListInfo {
                                list_id: sn.clone(),
                                level: ps.list_level.unwrap_or(0),
                                style_name: sn.clone(),
                            })
                        })
                    };
                    let _ = (pb.list_id, pb.list_level, pb.list_style_name);
                    let para = Paragraph {
                        style: StyleRef(pb.style_name),
                        runs: pb.runs,
                        list_info,
                        ..Default::default()
                    };
                    let be = BodyElement::Paragraph(para);
                    // 在 table cell 里？
                    if let Some(tb) = self.table_stack.last_mut() {
                        if let Some(cell) = tb.cur_cell.as_mut() {
                            cell.content.push(be);
                            return;
                        }
                    }
                    self.doc.body.push(be);
                }
            }

            // 提交 cell
            "table-cell" | "covered-table-cell" => {
                if let Some(tb) = self.table_stack.last_mut() {
                    if let Some(cb) = tb.cur_cell.take() {
                        let cell = TableCell {
                            content: cb.content.into_iter()
                                .filter_map(|e| e.try_into().ok())
                                .collect(),
                            col_span: cb.col_span,
                            row_span: cb.row_span,
                            style: cb.style.map(StyleRef),
                            hidden: false,
                            // PROOT-003 followup: ODT侧表格背景色/边框/内边距
                            // 暂未解析，本次改动范围限定PPTX，此处占位None非遗漏。
                            bg_color: None,
                            border_color: None,
                            border_width: None,
                            pad_left: None,
                            pad_right: None,
                            pad_top: None,
                            pad_bottom: None,
                        };
                        if let Some(row) = tb.cur_row.as_mut() {
                            row.push(cell);
                        }
                    }
                }
            }

            // 提交行
            "table-row" => {
                if let Some(tb) = self.table_stack.last_mut() {
                    if let Some(row_cells) = tb.cur_row.take() {
                        tb.rows.push(TableRow { cells: row_cells, row_height: None });
                    }
                }
            }

            // 提交表格
            "table" => {
                if let Some(tb) = self.table_stack.pop() {
                    self.doc.body.push(BodyElement::Table(Table {
                        float_pos: None,
                        rows: tb.rows,
                        col_widths: tb.col_widths,
                        style_id: None,
                        first_row: false,
                        first_col: false,
                        last_row: false,
                        last_col: false,
                        band_row: false,
                        band_col: false,
                        default_border_color: None,
                        default_border_width: None,
                        inside_h_color: None,
                        inside_h_width: None,
                        inside_v_color: None,
                        inside_v_width: None,
                    }));
                }
            }

            "binary-data" => {
                self.in_binary_data = false;
            }
            "image" => {
                self.in_draw_image = false;
            }
            "frame" => {
                self.in_draw_frame = false;
                if !self.cur_binary_data.is_empty() {
                    let decoded = base64::engine::general_purpose::STANDARD.decode(
                        self.cur_binary_data.replace([' ', '\n', '\r'], "").as_bytes()
                    );
                    if let Ok(bytes) = decoded {
                        let image = roe_model::common::ImageData {
                            data: bytes,
                            mime: self.cur_image_mime.clone(),
                            ..Default::default()
                        };
                        if self.cur_frame_is_float {
                            let frame = roe_model::writer::FloatFrame {
                                image,
                                width_pt:    self.cur_image_w_pt,
                                height_pt:   self.cur_image_h_pt,
                                x_pt:        self.cur_image_x_pt,
                                y_pt:        self.cur_image_y_pt,
                                ..Default::default()
                            };
                            self.doc.body.push(BodyElement::Frame(frame));
                        } else {
                            let img = roe_model::writer::InlineImage {
                                image,
                                width_pt:    self.cur_image_w_pt,
                                height_pt:   self.cur_image_h_pt,
                                ..Default::default()
                            };
                            self.doc.body.push(BodyElement::Image(img));
                        }
                    }
                    self.cur_binary_data.clear();
                }
            }
            _ => {}
        }
    }

    fn on_text(&mut self, text: String) {
        if self.in_meta {
            match self.meta_field {
                MetaField::Title    => self.doc.metadata.title    = Some(text),
                MetaField::Author   => self.doc.metadata.author   = Some(text),
                MetaField::Created  => self.doc.metadata.created  = Some(text),
                MetaField::Modified => self.doc.metadata.modified = Some(text),
                MetaField::None     => {}
            }
            return;
        }
        if !self.in_body { return; }
        self.push_text(text);
    }

    fn push_text(&mut self, text: String) {
        if self.in_binary_data {
            self.cur_binary_data.push_str(&text);
            return;
        }
        if text.is_empty() { return; }
        if let Some(rb) = self.cur_run.as_mut() {
            rb.text.push_str(&text);
        } else if let Some(pb) = self.para_stack.last_mut() {
            pb.runs.push(Run { text, char_style: None, ..Default::default() });
        }
    }

    fn flush_run(&mut self) {
        if let Some(rb) = self.cur_run.take() {
            if !rb.text.is_empty() {
                let run = Run { text: rb.text, char_style: rb.style_name.map(StyleRef), ..Default::default() };
                if let Some(pb) = self.para_stack.last_mut() {
                    pb.runs.push(run);
                }
            }
        }
    }

    fn finish(self) -> WriterDocument { self.doc }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE_FODT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
    xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0"
    office:mimetype="application/vnd.oasis.opendocument.text">
  <office:meta>
    <dc:title>测试文档</dc:title>
    <dc:creator>ROE Test</dc:creator>
  </office:meta>
  <office:styles>
    <style:style style:name="Default" style:family="paragraph">
      <style:text-properties fo:font-size="12pt" fo:font-family="Liberation Serif"/>
    </style:style>
    <style:style style:name="Heading1" style:family="paragraph"
                 style:parent-style-name="Default">
      <style:text-properties fo:font-size="18pt" fo:font-weight="bold"/>
      <style:paragraph-properties fo:margin-top="12pt" fo:margin-bottom="6pt"/>
    </style:style>
  </office:styles>
  <office:body>
    <office:text>
      <text:h text:style-name="Heading1">Hello ROE</text:h>
      <text:p text:style-name="Default">这是一段<text:span text:style-name="Default">普通文字</text:span>。</text:p>
      <text:p text:style-name="Default">第二段。</text:p>
    </office:text>
  </office:body>
</office:document>"#;

    #[test]
    fn test_parse_simple_fodt() {
        let doc = FodtParser::parse(SIMPLE_FODT).expect("parse failed");
        assert_eq!(doc.metadata.title.as_deref(),  Some("测试文档"));
        assert_eq!(doc.metadata.author.as_deref(), Some("ROE Test"));
        assert_eq!(doc.body.len(), 3);
        assert!(doc.styles.paragraph_styles.contains_key("Heading1"));
        assert!(doc.styles.paragraph_styles.contains_key("Default"));
    }

    #[test]
    fn test_style_inheritance() {
        let doc = FodtParser::parse(SIMPLE_FODT).expect("parse failed");
        let resolved = doc.styles.resolve_paragraph_style("Heading1");
        assert_eq!(resolved.font_family.as_deref(), Some("Liberation Serif"));
        assert_eq!(resolved.font_size, Some(18.0));
        assert!(resolved.bold);
        assert_eq!(resolved.space_before, Some(12.0));
    }

    #[test]
    fn test_paragraph_runs() {
        let doc = FodtParser::parse(SIMPLE_FODT).expect("parse failed");
        if let BodyElement::Paragraph(p) = &doc.body[1] {
            let all_text: String = p.runs.iter().map(|r| r.text.as_str()).collect();
            assert!(all_text.contains("普通文字"), "got: {}", all_text);
        } else {
            panic!("expected paragraph at index 1");
        }
    }

    // ── A4-a⑤：嵌套列表 level 推算（迁移自探针程序） ─────────────────
    const NESTED_LIST_FODT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
    xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
    office:mimetype="application/vnd.oasis.opendocument.text">
  <office:styles>
    <style:style style:name="Default" style:family="paragraph">
      <style:text-properties fo:font-size="12pt"/>
    </style:style>
    <text:list-style style:name="L1">
      <text:list-level-style-bullet text:level="1" text:bullet-char="•">
        <style:list-level-properties fo:margin-left="0.6cm" fo:text-indent="-0.6cm" text:min-label-width="0.6cm"/>
      </text:list-level-style-bullet>
      <text:list-level-style-number text:level="2" style:num-format="1" style:num-suffix=".">
        <style:list-level-properties fo:margin-left="1.2cm" fo:text-indent="-0.6cm" text:min-label-width="0.6cm"/>
      </text:list-level-style-number>
    </text:list-style>
  </office:styles>
  <office:body><office:text>
    <text:list text:style-name="L1">
      <text:list-item><text:p text:style-name="Default">第一项</text:p></text:list-item>
      <text:list-item><text:p text:style-name="Default">第二项</text:p>
        <text:list text:style-name="L1">
          <text:list-item><text:p text:style-name="Default">嵌套子项</text:p></text:list-item>
        </text:list>
      </text:list-item>
    </text:list>
    <text:p text:style-name="Default">普通段落</text:p>
  </office:text></office:body>
</office:document>"#;

    #[test]
    fn test_nested_list_level_inference() {
        let doc = FodtParser::parse(NESTED_LIST_FODT).expect("parse failed");
        assert!(doc.styles.list_styles.contains_key("L1"), "L1 列表样式应被解析");

        let list_infos: Vec<Option<&roe_model::writer::ListInfo>> = doc.body.iter()
            .filter_map(|e| if let BodyElement::Paragraph(p) = e { Some(p.list_info.as_ref()) } else { None })
            .collect();

        assert_eq!(list_infos.len(), 4, "应有4个段落：第一项/第二项/嵌套子项/普通段落");
        assert_eq!(list_infos[0].expect("第一项应有list_info").level, 0, "第一项应为 level 0");
        assert_eq!(list_infos[1].expect("第二项应有list_info").level, 0, "第二项应为 level 0");
        assert_eq!(list_infos[2].expect("嵌套子项应有list_info").level, 1, "嵌套子项应为 level 1（0-based）");
        assert!(list_infos[3].is_none(), "普通段落不应带 list_info");

        // 校验 level 0 是 bullet，level 1 是 number，且缩进单位换算正确
        let l1 = doc.styles.list_styles.get("L1").unwrap();
        match l1.level(0) {
            Some(roe_model::writer::ListLevelStyle::Bullet(b)) => {
                assert_eq!(b.bullet_char, "•");
                // 0.6cm = 0.6 * 28.3465 ≈ 17.0079 pt
                assert!((b.indent.margin_left - 17.0079).abs() < 0.01, "got {}", b.indent.margin_left);
            }
            other => panic!("level 0 应为 Bullet，实际: {:?}", other),
        }
        match l1.level(1) {
            Some(roe_model::writer::ListLevelStyle::Number(n)) => {
                assert_eq!(n.format, roe_model::writer::NumFormat::Arabic);
                assert_eq!(n.suffix, ".");
            }
            other => panic!("level 1 应为 Number，实际: {:?}", other),
        }
    }

    // ── A4-a⑤：段落样式自带列表（style:list-style-name，无 <text:list> 包裹） ──
    const STYLE_OWNED_LIST_FODT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
    xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
    office:mimetype="application/vnd.oasis.opendocument.text">
  <office:styles>
    <style:style style:name="Default" style:family="paragraph">
      <style:text-properties fo:font-size="12pt"/>
    </style:style>
    <style:style style:name="Heading1" style:family="paragraph"
                 style:list-style-name="L2" style:list-level="1"/>
    <text:list-style style:name="L2">
      <text:list-level-style-number text:level="1" style:num-format="1" style:num-suffix=")"/>
    </text:list-style>
  </office:styles>
  <office:body><office:text>
    <text:h text:style-name="Heading1">自带编号的标题</text:h>
    <text:p text:style-name="Default">普通段落，不受影响</text:p>
  </office:text></office:body>
</office:document>"#;

    #[test]
    fn test_paragraph_style_owned_list() {
        let doc = FodtParser::parse(STYLE_OWNED_LIST_FODT).expect("parse failed");

        let heading_style = doc.styles.paragraph_styles.get("Heading1")
            .expect("自闭合 style:style 也应被解析进 paragraph_styles（on_empty 修复验证）");
        assert_eq!(heading_style.list_style_name.as_deref(), Some("L2"));
        assert_eq!(heading_style.list_level, Some(0), "ODF 1-based level=1 应转换为 0-based");

        let heading_list_info = doc.body.iter().find_map(|e| {
            if let BodyElement::Paragraph(p) = e {
                if p.style.0 == "Heading1" { return p.list_info.clone(); }
            }
            None
        });
        assert!(heading_list_info.is_some(), "标题段落应从 list_style_name 生成 list_info");
        assert_eq!(heading_list_info.unwrap().level, 0);

        let normal_list_info = doc.body.iter().find_map(|e| {
            if let BodyElement::Paragraph(p) = e {
                if p.style.0 == "Default" { return Some(p.list_info.clone()); }
            }
            None
        }).flatten();
        assert!(normal_list_info.is_none(), "普通段落不应受 list_style_name 影响");
    }
}

    #[test]
    fn test_image_parse() {
        let data = std::fs::read("/home/yong/test_image.fodt").unwrap();
        let xml = std::str::from_utf8(&data).unwrap();
        let doc = FodtParser::parse(xml).unwrap();
        println!("body elements: {}", doc.body.len());
        for (i, elem) in doc.body.iter().enumerate() {
            match elem {
                roe_model::writer::BodyElement::Image(img) => {
                    println!("[{i}] Image {}x{} pt, data_len={}", img.width_pt, img.height_pt, img.image.data.len());
                }
                roe_model::writer::BodyElement::Paragraph(p) => {
                    println!("[{i}] Para: {:?}", p.runs.iter().map(|r| r.text.as_str()).collect::<Vec<_>>());
                }
                _ => println!("[{i}] Other"),
            }
        }
    }
