//! FODS (Flat ODF Spreadsheet) 解析器
//! 解析 FODS XML → roe_model::calc::Workbook

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use roe_model::calc::{CellBlock, CellError, CellValue, FormulaDialectSource, Sheet, SparseColumn, SparseGrid, Workbook};
use roe_model::style::StyleRegistry;
use std::collections::HashMap;

use smallvec::SmallVec;
use crate::ParseError;
use super::xml_tree::parse_tree;

pub struct FodsParser;

impl FodsParser {
    /// 解析 FODS XML 字符串，返回 Workbook
    pub fn parse(xml: &str) -> Result<Workbook, ParseError> {
        let mut parser = FodsParserInner::new();
        parser.run(xml)?;
        let mut workbook = parser.finish();

        // sparkline 独立二次扫描（不侵入主状态机），按 sheet 名回填
        let sparkline_map = parse_sparkline_groups_from_fods_xml(xml);
        for sheet in workbook.sheets.iter_mut() {
            if let Some(groups) = sparkline_map.get(&sheet.name) {
                sheet.sparkline_groups = groups.clone();
            }
        }

        Ok(workbook)
    }
}

/// 解析 fods 全文里所有 <calcext:sparkline-groups> 块，按所属 <table:table table:name="..."> 分组。
/// 独立二次扫描整份 xml，不复用主状态机 FodsParserInner，避免侵入 800+ 行的核心解析循环。
/// 标签风格为 kebab-case + calcext: 前缀，与 xlsx 侧 x14: 驼峰命名同构但拼写不同，故不复用
/// xlsx 侧的 parse_sparkline_groups_from_xml，独立实现。
fn parse_sparkline_groups_from_fods_xml(xml: &str) -> std::collections::HashMap<String, Vec<roe_model::calc::SparklineGroup>> {
    use roe_model::calc::{SparklineGroup, Sparkline, SparklineType, DisplayEmptyCellsAs, SparklineAxisType};
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut result: std::collections::HashMap<String, Vec<SparklineGroup>> = std::collections::HashMap::new();

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut current_table_name = String::new();
    let mut cur_group: Option<SparklineGroup> = None;
    let mut buf = Vec::new();

    // "Sheet1.A2" → (row, col)，剥离 "SheetName." 前缀后复用 parse_odf_cell_ref
    fn strip_sheet_prefix(addr: &str) -> &str {
        match addr.find('.') {
            Some(idx) => &addr[idx + 1..],
            None => addr,
        }
    }

    // "Sheet1.B1:Sheet1.M1" → CellRange，剥离每一半的 sheet 前缀后复用 parse_odf_cell_ref
    fn parse_fods_data_range(raw: &str) -> Option<roe_model::calc::CellRange> {
        let mut parts = raw.splitn(2, ':');
        let first = parts.next()?;
        let second = parts.next().unwrap_or(first);
        let (r1, c1) = parse_odf_cell_ref(strip_sheet_prefix(first))?;
        let (r2, c2) = parse_odf_cell_ref(strip_sheet_prefix(second))?;
        Some(roe_model::calc::CellRange {
            sheet: String::new(),
            start: (r1, c1),
            end: (r2, c2),
        })
    }

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                match e.local_name().as_ref() {
                    b"table" => {
                        for attr in e.attributes().flatten() {
                            if attr.key.local_name().as_ref() == b"name" {
                                if let Ok(v) = attr.unescape_value() {
                                    current_table_name = v.into_owned();
                                }
                            }
                        }
                    }
                    b"sparkline-group" => {
                        let mut g = SparklineGroup::default();
                        for attr in e.attributes().flatten() {
                            let key = attr.key.local_name();
                            let key = key.as_ref();
                            let val = match attr.unescape_value() { Ok(v) => v.into_owned(), Err(_) => continue };
                            match key {
                                b"type" => {
                                    g.sparkline_type = match val.as_str() {
                                        "column" => SparklineType::Column,
                                        "stacked" => SparklineType::Stacked,
                                        _ => SparklineType::Line,
                                    };
                                }
                                b"line-width" => { g.line_weight = parse_measure(&val); }
                                b"manual-max" => { g.manual_max = val.parse().ok(); }
                                b"manual-min" => { g.manual_min = val.parse().ok(); }
                                b"date-axis" => { g.date_axis = val == "true"; }
                                b"display-empty-cells-as" => {
                                    g.display_empty_cells_as = match val.as_str() {
                                        "span" => DisplayEmptyCellsAs::Span,
                                        "zero" => DisplayEmptyCellsAs::Zero,
                                        _ => DisplayEmptyCellsAs::Gap,
                                    };
                                }
                                b"markers" => { g.markers = val == "true"; }
                                b"high" => { g.high = val == "true"; }
                                b"low" => { g.low = val == "true"; }
                                b"first" => { g.first = val == "true"; }
                                b"last" => { g.last = val == "true"; }
                                b"negative" => { g.negative = val == "true"; }
                                b"display-x-axis" => { g.display_x_axis = val == "true"; }
                                b"display-hidden" => { g.display_hidden = val == "true"; }
                                b"min-axis-type" => {
                                    g.min_axis_type = match val.as_str() {
                                        "group" => SparklineAxisType::Group,
                                        "custom" => SparklineAxisType::Custom,
                                        _ => SparklineAxisType::Individual,
                                    };
                                }
                                b"max-axis-type" => {
                                    g.max_axis_type = match val.as_str() {
                                        "group" => SparklineAxisType::Group,
                                        "custom" => SparklineAxisType::Custom,
                                        _ => SparklineAxisType::Individual,
                                    };
                                }
                                b"right-to-left" => { g.right_to_left = val == "true"; }
                                b"color-series" => { g.color_series = Some(val); }
                                b"color-negative" => { g.color_negative = Some(val); }
                                b"color-axis" => { g.color_axis = Some(val); }
                                b"color-markers" => { g.color_markers = Some(val); }
                                b"color-first" => { g.color_first = Some(val); }
                                b"color-last" => { g.color_last = Some(val); }
                                b"color-high" => { g.color_high = Some(val); }
                                b"color-low" => { g.color_low = Some(val); }
                                _ => {}
                            }
                        }
                        cur_group = Some(g);
                    }
                    b"sparkline" => {
                        if let Some(g) = cur_group.as_mut() {
                            let mut cell_addr: Option<String> = None;
                            let mut data_range: Option<String> = None;
                            for attr in e.attributes().flatten() {
                                let key = attr.key.local_name();
                                let val = match attr.unescape_value() { Ok(v) => v.into_owned(), Err(_) => continue };
                                match key.as_ref() {
                                    b"cell-address" => cell_addr = Some(val),
                                    b"data-range" => data_range = Some(val),
                                    _ => {}
                                }
                            }
                            if let Some(addr) = cell_addr {
                                if let Some((row, col)) = parse_odf_cell_ref(strip_sheet_prefix(&addr)) {
                                    let input_range = data_range
                                        .as_deref()
                                        .and_then(parse_fods_data_range)
                                        .into_iter()
                                        .collect();
                                    g.sparklines.push(Sparkline { cell: (row, col), input_range });
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                if e.local_name().as_ref() == b"sparkline-group" {
                    if let Some(g) = cur_group.take() {
                        result.entry(current_table_name.clone()).or_default().push(g);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    result
}

// ──────────────────────────────────────────────
// 内部状态机
// ──────────────────────────────────────────────

/// 当前所在的 XML 上下文
#[derive(Debug, PartialEq, Clone)]
enum Context {
    Root,
    Styles,
    Body,
    Spreadsheet,
    Table,
    Row,
    Cell,
    Shapes,
    Frame,
    ContentValidations,
    ContentValidation,
    DvHelpMessage,
    DvErrorMessage,
}

struct FodsParserInner {
    ctx_stack: Vec<Context>,

    // 正在构建的数据
    sheets: Vec<Sheet>,
    named_ranges: HashMap<String, roe_model::calc::CellRange>,
    current_sheet_name: String,
    shapes_xml_buf: String,  // 收集 table-shapes 原始 XML
    frame_xml_buf: String,   // 收集 cell 内 draw:frame 原始 XML
    pending_images: Vec<roe_model::calc::FloatingImage>, // 当前 table 待提交图片
    current_rows: Vec<(u32, u32, CellValue)>, // (row, col, value)

    current_row: u32,
    current_col: u32,

    // 当前 cell 的属性
    cell_value_type: Option<String>,
    cell_formula: Option<String>,
    cell_string_value: Option<String>, // office:string-value 属性
    cell_date_value: Option<String>,
    cell_bool_value: Option<String>,
    cell_num_value: Option<String>,    // office:value 属性（数字直接从属性读）
    cell_error: bool,

    // repeat 属性
    row_repeat: u32,
    col_repeat: u32,

    // 是否在读 text:p（cell 文本内容）
    in_text_p: bool,
    text_p_depth: usize,
    text_buf: String,

    // column width / row height（简单记录）
    col_widths: Vec<f32>,
    row_heights: Vec<f32>,

    // style:style style-name -> 宽度/高度（pt），来自 automatic-styles 里的
    // <style:table-column-properties style:column-width="..."/> 和
    // <style:table-row-properties style:row-height="..."/>。
    // table:table-column/table-row 只带 table:style-name 引用，实际宽高定义
    // 在这里，之前完全没有解析这层关联，导致列宽/行高永远走默认值。
    col_style_widths: HashMap<String, f32>,
    row_style_heights: HashMap<String, f32>,
    // 当前正在解析的 style:style 的 style-name（仅 family=table-column/table-row 时有意义）
    current_col_row_style_name: Option<String>,

    // ODF 数据验证（table:content-validations，文档级具名规则 + 单元格反向引用）
    // name -> (kind, operator, formula1, formula2, allow_blank, show_error, error_msg)
    content_validations: HashMap<String, (String, String, String, Option<String>, bool, bool, Option<String>)>,
    // 当前正在解析的 content-validation 规则（name + 属性 + 收集中的 error-message 字段）
    dv_current_name: String,
    dv_current_condition: String,
    dv_current_allow_blank: bool,
    dv_current_show_error: bool,     // table:error-message table:display
    dv_current_error_msg: Option<String>, // table:error-message 的 text:p 内容
    dv_in_error_message_p: bool,
    // 当前 cell 的 validation-name 引用（本轮单元格属性读取用）
    cell_validation_name: Option<String>,
    // 当前 sheet 内，所有 (row, col, validation_name) 引用（sheet 提交时消化为 data_validations）
    cell_validation_refs: Vec<(u32, u32, String)>,
}

impl FodsParserInner {
    fn new() -> Self {
        Self {
            named_ranges: HashMap::new(),
            ctx_stack: vec![Context::Root],
            sheets: Vec::new(),
            current_sheet_name: String::new(),
            current_rows: Vec::new(),
            current_row: 0,
            current_col: 0,
            cell_value_type: None,
            cell_formula: None,
            cell_string_value: None,
            cell_date_value: None,
            cell_bool_value: None,
            cell_num_value: None,
            cell_error: false,
            row_repeat: 1,
            col_repeat: 1,
            in_text_p: false,
            text_p_depth: 0,
            text_buf: String::new(),
            col_widths: Vec::new(),
            row_heights: Vec::new(),
            col_style_widths: HashMap::new(),
            row_style_heights: HashMap::new(),
            current_col_row_style_name: None,
            shapes_xml_buf: String::new(),
            frame_xml_buf: String::new(),
            pending_images: Vec::new(),
            content_validations: HashMap::new(),
            dv_current_name: String::new(),
            dv_current_condition: String::new(),
            dv_current_allow_blank: true,
            dv_current_show_error: false,
            dv_current_error_msg: None,
            dv_in_error_message_p: false,
            cell_validation_name: None,
            cell_validation_refs: Vec::new(),
        }
    }

    fn ctx(&self) -> &Context {
        self.ctx_stack.last().unwrap_or(&Context::Root)
    }

    fn push(&mut self, c: Context) {
        self.ctx_stack.push(c);
    }

    fn pop(&mut self) {
        self.ctx_stack.pop();
    }

    fn run(&mut self, xml: &str) -> Result<(), ParseError> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(false);

        let mut buf = Vec::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) => {
                    let name_bytes = e.name().as_ref().to_vec();
                    let local = local_name_owned(&name_bytes);
                    self.handle_start(&local, e)?;
                }
                Ok(Event::Empty(ref e)) => {
                    let name_bytes = e.name().as_ref().to_vec();
                    let local = local_name_owned(&name_bytes);
                    if matches!(self.ctx(), Context::Frame) {
                        let raw = String::from_utf8_lossy(e.as_ref()).into_owned();
                        self.frame_xml_buf.push('<');
                        self.frame_xml_buf.push_str(&raw);
                        self.frame_xml_buf.push_str("/>");
                    } else if matches!(self.ctx(), Context::Shapes) && local != "shapes" {
                        let raw = String::from_utf8_lossy(e.as_ref()).into_owned();
                        self.shapes_xml_buf.push('<');
                        self.shapes_xml_buf.push_str(&raw);
                        self.shapes_xml_buf.push_str("/>");
                    }
                    self.handle_start(&local, e)?;
                    self.handle_end(&local)?;
                }
                Ok(Event::End(ref e)) => {
                    let name_bytes = e.name().as_ref().to_vec();
                    let local = local_name_owned(&name_bytes);
                    if matches!(self.ctx(), Context::Frame) {
                        let full = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                        self.frame_xml_buf.push_str("</");
                        self.frame_xml_buf.push_str(&full);
                        self.frame_xml_buf.push('>');
                    } else if matches!(self.ctx(), Context::Shapes) && local != "shapes" {
                        // DEVROOT-001: 必须用完整标签名（带命名空间前缀），
                        // 不能用 local（已剥离前缀），否则拼出的结束标签
                        // 与开始标签不匹配（如 <draw:frame> 对应 </frame>），
                        // parse_tree 二次解析时树节点 tag 字段值不符预期，
                        // 导致 children_tag("draw:frame") 等查找静默返回空。
                        let full = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                        self.shapes_xml_buf.push_str("</");
                        self.shapes_xml_buf.push_str(&full);
                        self.shapes_xml_buf.push('>');
                    }
                    self.handle_end(&local)?;
                }
                Ok(Event::Text(ref e)) => {
                    let s = e.unescape().unwrap_or_default();
                    if matches!(self.ctx(), Context::Frame) {
                        self.frame_xml_buf.push_str(&s);
                    } else if matches!(self.ctx(), Context::Shapes) {
                        self.shapes_xml_buf.push_str(&s);
                    }
                    // text_buf 同时服务两种场景：普通 cell 内 text:p（in_text_p）
                    // 和 error-message 内 text:p（dv_in_error_message_p）。两者在
                    // 状态机语境上互斥（不会同时为 true），共用同一个缓冲区安全。
                    if self.in_text_p || self.dv_in_error_message_p {
                        self.text_buf.push_str(&s);
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(ParseError::Xml(e)),
                _ => {}
            }
            buf.clear();
        }
        Ok(())
    }

    fn handle_start(
        &mut self,
        local: &str,
        e: &quick_xml::events::BytesStart,
    ) -> Result<(), ParseError> {
        match local {
            "document-styles" | "document" => {
                // 顶层元素，不需要特殊处理
            }
            "styles" | "automatic-styles" | "master-styles" => {
                self.push(Context::Styles);
            }
            "style" if matches!(self.ctx(), Context::Styles) => {
                // 只关心 family = table-column / table-row 的 style:style，
                // 记录其 style-name，供紧随其后的 table-column-properties /
                // table-row-properties 子标签使用。其余 family（table-cell 等）
                // 不在本次范围内，current_col_row_style_name 置 None。
                let mut name: Option<String> = None;
                let mut family: Option<String> = None;
                for attr in e.attributes().flatten() {
                    let key = local_name(attr.key.as_ref());
                    match key {
                        "name" => name = Some(String::from_utf8_lossy(&attr.value).into_owned()),
                        "family" => family = Some(String::from_utf8_lossy(&attr.value).into_owned()),
                        _ => {}
                    }
                }
                self.current_col_row_style_name = match family.as_deref() {
                    Some("table-column") | Some("table-row") => name,
                    _ => None,
                };
            }
            "table-column-properties" if matches!(self.ctx(), Context::Styles) => {
                if let Some(style_name) = self.current_col_row_style_name.clone() {
                    for attr in e.attributes().flatten() {
                        let key = local_name(attr.key.as_ref());
                        if key == "column-width" {
                            let width = parse_measure(&String::from_utf8_lossy(&attr.value));
                            self.col_style_widths.insert(style_name.clone(), width);
                        }
                    }
                }
            }
            "table-row-properties" if matches!(self.ctx(), Context::Styles) => {
                if let Some(style_name) = self.current_col_row_style_name.clone() {
                    for attr in e.attributes().flatten() {
                        let key = local_name(attr.key.as_ref());
                        if key == "row-height" {
                            let height = parse_measure(&String::from_utf8_lossy(&attr.value));
                            self.row_style_heights.insert(style_name.clone(), height);
                        }
                    }
                }
            }
            "body" => {
                self.push(Context::Body);
            }
            "spreadsheet" => {
                self.push(Context::Spreadsheet);
            }
            "named-range" => {
                let mut name = String::new();
                let mut range_addr = String::new();
                for attr in e.attributes().flatten() {
                    let key = local_name(attr.key.as_ref());
                    match key {
                        "name" => name = String::from_utf8_lossy(&attr.value).into_owned(),
                        "cell-range-address" => {
                            range_addr = String::from_utf8_lossy(&attr.value).into_owned();
                        }
                        _ => {}
                    }
                }
                if !name.is_empty() {
                    if let Some(range) = parse_odf_named_range_address(&range_addr) {
                        self.named_ranges.insert(name, range);
                    }
                }
            }
            "content-validations" if matches!(self.ctx(), Context::Spreadsheet) => {
                self.push(Context::ContentValidations);
            }
            "content-validation" if matches!(self.ctx(), Context::ContentValidations) => {
                self.push(Context::ContentValidation);
                self.dv_current_name.clear();
                self.dv_current_condition.clear();
                self.dv_current_allow_blank = true;
                self.dv_current_show_error = false;
                self.dv_current_error_msg = None;
                for attr in e.attributes().flatten() {
                    let key = local_name(attr.key.as_ref());
                    match key {
                        "name" => {
                            self.dv_current_name =
                                String::from_utf8_lossy(&attr.value).into_owned();
                        }
                        "condition" => {
                            // table:condition 里可能含 &lt;/&gt;/&amp; 等转义实体
                            // （比如 cell-content()&lt;100），必须解码后再交给
                            // parse_odf_condition，否则 <>符号比较语法会全部解析失败。
                            self.dv_current_condition = attr
                                .unescape_value()
                                .map(|c| c.into_owned())
                                .unwrap_or_else(|_| {
                                    String::from_utf8_lossy(&attr.value).into_owned()
                                });
                        }
                        "allow-empty-cell" => {
                            let val = String::from_utf8_lossy(&attr.value).into_owned();
                            self.dv_current_allow_blank = val == "true";
                        }
                        _ => {}
                    }
                }
            }
            "help-message" if matches!(self.ctx(), Context::ContentValidation) => {
                self.push(Context::DvHelpMessage);
            }
            "error-message" if matches!(self.ctx(), Context::ContentValidation) => {
                self.push(Context::DvErrorMessage);
                for attr in e.attributes().flatten() {
                    let key = local_name(attr.key.as_ref());
                    let val = String::from_utf8_lossy(&attr.value).into_owned();
                    if key == "display" {
                        self.dv_current_show_error = val == "true";
                    }
                }
            }
            "p" if matches!(self.ctx(), Context::DvErrorMessage) => {
                self.dv_in_error_message_p = true;
                self.text_buf.clear();
            }
            "table" if matches!(self.ctx(), Context::Spreadsheet) => {
                self.push(Context::Table);
                // 读取 table:name
                for attr in e.attributes().flatten() {
                    let key = local_name(attr.key.as_ref());
                    if key == "name" {
                        self.current_sheet_name =
                            String::from_utf8_lossy(&attr.value).into_owned();
                    }
                }
                self.current_row = 0;
                self.current_col = 0;
                self.current_rows.clear();
                self.col_widths.clear();
                self.row_heights.clear();
            }
            "table-column" if matches!(self.ctx(), Context::Table) => {
                // 列宽优先级：本标签直接写的 column-width 属性（ODF 允许但罕见）
                // > table:style-name 关联到 automatic-styles 里的
                // style:table-column-properties/column-width（最常见情形，
                // 之前完全没做这层关联，永远落到下面的默认值）> 默认值。
                let mut width: Option<f32> = None;
                let mut style_name: Option<String> = None;
                let mut repeat = 1u32;
                for attr in e.attributes().flatten() {
                    let key = local_name(attr.key.as_ref());
                    match key {
                        "column-width" => {
                            width = Some(parse_measure(&String::from_utf8_lossy(&attr.value)));
                        }
                        "style-name" => {
                            style_name = Some(String::from_utf8_lossy(&attr.value).into_owned());
                        }
                        "number-columns-repeated" => {
                            repeat = String::from_utf8_lossy(&attr.value)
                                .parse()
                                .unwrap_or(1);
                        }
                        _ => {}
                    }
                }
                let width = width
                    .or_else(|| style_name.as_ref().and_then(|n| self.col_style_widths.get(n).copied()))
                    .unwrap_or(64.0); // 默认约 2.267cm 换算成 pt（64.27pt），此前误写成裸厘米数 2.267
                for _ in 0..repeat {
                    self.col_widths.push(width);
                }
            }
            "table-row" if matches!(self.ctx(), Context::Table) => {
                self.push(Context::Row);
                self.current_col = 0;
                self.row_repeat = 1;
                // 行高优先级同列宽：本标签直接写的 row-height 属性
                // > style-name 关联到 automatic-styles 的 row-height（最常见）
                // > 默认值。
                let mut height: Option<f32> = None;
                let mut style_name: Option<String> = None;
                for attr in e.attributes().flatten() {
                    let key = local_name(attr.key.as_ref());
                    match key {
                        "number-rows-repeated" => {
                            self.row_repeat = String::from_utf8_lossy(&attr.value)
                                .parse()
                                .unwrap_or(1);
                        }
                        "row-height" => {
                            height = Some(parse_measure(&String::from_utf8_lossy(&attr.value)));
                        }
                        "style-name" => {
                            style_name = Some(String::from_utf8_lossy(&attr.value).into_owned());
                        }
                        _ => {}
                    }
                }
                let height = height
                    .or_else(|| style_name.as_ref().and_then(|n| self.row_style_heights.get(n).copied()))
                    .unwrap_or(12.8); // 默认约 0.453cm 换算成 pt（12.84pt），此前误写成裸厘米数 0.453
                self.row_heights.push(height);
            }
            "table-cell" | "covered-table-cell"
                if matches!(self.ctx(), Context::Row) =>
            {
                self.push(Context::Cell);
                self.cell_value_type = None;
                self.cell_formula = None;
                self.cell_string_value = None;
                self.cell_date_value = None;
                self.cell_bool_value = None;
                self.cell_num_value = None;
                self.cell_error = false;
                self.col_repeat = 1;
                self.text_buf.clear();
                self.cell_validation_name = None;

                for attr in e.attributes().flatten() {
                    let key = local_name(attr.key.as_ref());
                    let val = String::from_utf8_lossy(&attr.value).into_owned();
                    match key {
                        "value-type" => {
                            self.cell_value_type = Some(val);
                        }
                        "validation-name" => {
                            self.cell_validation_name = Some(val);
                        }
                        "formula" => {
                            // ROOT-020: 属性值可能含 XML 实体（如 &quot;），
                            // 之前用 from_utf8_lossy 未反转义，导致公式里的
                            // 字符串字面量带着 &quot; 喂给 formualizer_parse。
                            let unescaped = attr
                                .unescape_value()
                                .map(|c| c.into_owned())
                                .unwrap_or_else(|_| val.clone());
                            // 去掉 "of:" 或 "oooc:" 前缀
                            let f = unescaped
                                .strip_prefix("of:")
                                .or_else(|| unescaped.strip_prefix("oooc:"))
                                .unwrap_or(&unescaped)
                                .to_string();
                            self.cell_formula = Some(f);
                        }
                        "value" => {
                            self.cell_num_value = Some(val);
                        }
                        "string-value" => {
                            // ROOT-020: 同样需要反转义（如 cell 内容本身
                            // 含 &amp;/&quot; 等）
                            let unescaped = attr
                                .unescape_value()
                                .map(|c| c.into_owned())
                                .unwrap_or_else(|_| val.clone());
                            self.cell_string_value = Some(unescaped);
                        }
                        "date-value" => {
                            self.cell_date_value = Some(val);
                        }
                        "boolean-value" => {
                            self.cell_bool_value = Some(val);
                        }
                        "number-columns-repeated" => {
                            self.col_repeat = val.parse().unwrap_or(1);
                        }
                        _ => {}
                    }
                }
            }
            "frame" if matches!(self.ctx(), Context::Cell) => {
                self.push(Context::Frame);
                self.frame_xml_buf.clear();
                let raw = String::from_utf8_lossy(e.as_ref()).into_owned();
                self.frame_xml_buf.push('<');
                self.frame_xml_buf.push_str(&raw);
                self.frame_xml_buf.push('>');
            }
            _ if matches!(self.ctx(), Context::Frame) => {
                let raw = String::from_utf8_lossy(e.as_ref()).into_owned();
                self.frame_xml_buf.push('<');
                self.frame_xml_buf.push_str(&raw);
                self.frame_xml_buf.push('>');
            }
            "shapes" if matches!(self.ctx(), Context::Table) => {
                self.push(Context::Shapes);
                self.shapes_xml_buf.clear();
                self.shapes_xml_buf.push_str("<shapes>");
            }
            _ if matches!(self.ctx(), Context::Shapes) => {
                // 收集 shapes 内所有原始标签（简单文本拼接）
                let raw = String::from_utf8_lossy(e.as_ref()).into_owned();
                self.shapes_xml_buf.push('<');
                self.shapes_xml_buf.push_str(&raw);
                self.shapes_xml_buf.push('>');
            }
            "p" if matches!(self.ctx(), Context::Cell) => {
                self.in_text_p = true;
                self.text_p_depth = 1;
            }
            "p" if self.in_text_p => {
                self.text_p_depth += 1;
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_end(&mut self, local: &str) -> Result<(), ParseError> {
        match local {
            "styles" | "automatic-styles" | "master-styles"
                if matches!(self.ctx(), Context::Styles) =>
            {
                self.pop();
            }
            "body" if matches!(self.ctx(), Context::Body) => {
                self.pop();
            }
            "spreadsheet" if matches!(self.ctx(), Context::Spreadsheet) => {
                self.pop();
            }
            "shapes" if matches!(self.ctx(), Context::Shapes) => {
                self.shapes_xml_buf.push_str("</shapes>");
                self.pop();
                self.parse_shapes_charts();
            }
            "content-validations" if matches!(self.ctx(), Context::ContentValidations) => {
                self.pop();
            }
            "content-validation" if matches!(self.ctx(), Context::ContentValidation) => {
                self.pop();
                if !self.dv_current_name.is_empty() {
                    if let Some((kind, operator, formula1, formula2)) =
                        super::odf_data_validation::parse_odf_condition(&self.dv_current_condition)
                    {
                        self.content_validations.insert(
                            std::mem::take(&mut self.dv_current_name),
                            (
                                kind, operator, formula1, formula2,
                                self.dv_current_allow_blank,
                                self.dv_current_show_error,
                                std::mem::take(&mut self.dv_current_error_msg),
                            ),
                        );
                    }
                }
            }
            "help-message" if matches!(self.ctx(), Context::DvHelpMessage) => {
                self.pop();
            }
            "error-message" if matches!(self.ctx(), Context::DvErrorMessage) => {
                self.pop();
            }
            "p" if matches!(self.ctx(), Context::DvErrorMessage) && self.dv_in_error_message_p => {
                self.dv_in_error_message_p = false;
                if !self.text_buf.is_empty() {
                    self.dv_current_error_msg = Some(std::mem::take(&mut self.text_buf));
                }
            }
            "table" if matches!(self.ctx(), Context::Table) => {
                self.pop();
                self.commit_sheet();
            }
            "frame" if matches!(self.ctx(), Context::Frame) => {
                self.frame_xml_buf.push_str("</draw:frame>");
                self.pop();
                self.parse_frame_image();
            }
            "table-row" if matches!(self.ctx(), Context::Row) => {
                self.pop();
                self.current_row += self.row_repeat;
            }
            "table-cell" | "covered-table-cell"
                if matches!(self.ctx(), Context::Cell) =>
            {
                self.pop();
                self.commit_cell();
                self.current_col += self.col_repeat;
            }
            "p" if self.in_text_p => {
                self.text_p_depth -= 1;
                if self.text_p_depth == 0 {
                    self.in_text_p = false;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn commit_cell(&mut self) {
        let value = self.build_cell_value();
        // validation-name 引用需要记录，即使单元格值本身为空
        // （数据验证规则常挂在空单元格上，等待用户输入）
        if let Some(ref name) = self.cell_validation_name {
            for i in 0..self.col_repeat {
                self.cell_validation_refs.push((
                    self.current_row,
                    self.current_col + i,
                    name.clone(),
                ));
            }
        }
        // 空单元格跳过（节省内存）
        if matches!(value, CellValue::Empty) && self.col_repeat < 64 {
            return;
        }
        // repeat 的空单元格直接跳过
        if matches!(value, CellValue::Empty) {
            return;
        }
        for i in 0..self.col_repeat {
            self.current_rows.push((
                self.current_row,
                self.current_col + i,
                value.clone(),
            ));
        }
    }

    fn build_cell_value(&self) -> CellValue {
        // 公式优先
        if let Some(ref f) = self.cell_formula {
            return CellValue::Formula { source: f.clone(), cached: Box::new(CellValue::Empty) };
        }

        match self.cell_value_type.as_deref() {
            Some("float") | Some("currency") | Some("percentage") => {
                if let Some(ref v) = self.cell_num_value {
                    if let Ok(n) = v.parse::<f64>() {
                        return CellValue::Number(n);
                    }
                }
                // fallback: 尝试从文本内容解析
                if let Ok(n) = self.text_buf.trim().parse::<f64>() {
                    return CellValue::Number(n);
                }
                CellValue::Empty
            }
            Some("string") => {
                // string-value 属性优先，其次 text:p 内容
                let s = self
                    .cell_string_value
                    .as_deref()
                    .unwrap_or(self.text_buf.trim());
                if s.is_empty() {
                    CellValue::Empty
                } else {
                    CellValue::Text(s.to_string())
                }
            }
            Some("boolean") => {
                let v = self.cell_bool_value.as_deref().unwrap_or("false");
                CellValue::Bool(v == "true" || v == "TRUE" || v == "1")
            }
            Some("date") | Some("time") => {
                // 日期/时间：存为文本（后续可扩展为专用类型）
                let s = self
                    .cell_date_value
                    .as_deref()
                    .unwrap_or(self.text_buf.trim());
                if s.is_empty() {
                    CellValue::Empty
                } else {
                    CellValue::Text(s.to_string())
                }
            }
            Some("error") => CellValue::Error(CellError::Value),
            None => {
                // 无类型：尝试文本内容
                let t = self.text_buf.trim();
                if t.is_empty() {
                    CellValue::Empty
                } else {
                    CellValue::Text(t.to_string())
                }
            }
            _ => CellValue::Empty,
        }
    }

    fn parse_length_pt(s: &str) -> Option<f32> {
        let s = s.trim();
        if let Some(v) = s.strip_suffix("cm") {
            return v.trim().parse::<f32>().ok().map(|x| x * 28.3465);
        }
        if let Some(v) = s.strip_suffix("mm") {
            return v.trim().parse::<f32>().ok().map(|x| x * 2.83465);
        }
        if let Some(v) = s.strip_suffix("in") {
            return v.trim().parse::<f32>().ok().map(|x| x * 72.0);
        }
        if let Some(v) = s.strip_suffix("pt") {
            return v.trim().parse::<f32>().ok();
        }
        if let Some(v) = s.strip_suffix("px") {
            return v.trim().parse::<f32>().ok().map(|x| x * 0.75);
        }
        s.parse::<f32>().ok()
    }

    fn parse_frame_image(&mut self) {
        // parse_tree は namespace prefix を落とすので、正規表現で直接抽出する
        let buf = &self.frame_xml_buf;

        // svg:width/height/x/y を frame タグの属性から抽出
        let w_pt = Self::extract_attr(buf, "svg:width").and_then(|v| Self::parse_length_pt(&v)).unwrap_or(100.0);
        let h_pt = Self::extract_attr(buf, "svg:height").and_then(|v| Self::parse_length_pt(&v)).unwrap_or(100.0);
        let x_pt = Self::extract_attr(buf, "svg:x").and_then(|v| Self::parse_length_pt(&v)).unwrap_or(0.0);
        let y_pt = Self::extract_attr(buf, "svg:y").and_then(|v| Self::parse_length_pt(&v)).unwrap_or(0.0);

        // office:binary-data タグの中身を抽出
        let data_base64 = Self::extract_between(buf, "office:binary-data")
            .map(|s| s.split_whitespace().collect::<String>())
            .unwrap_or_default();
        // DEVROOT-002修复：读回 mime-type 属性（写出端 fods_writer.rs
        // 已同步补上写出，这里对应读回，形成完整闭环）。找不到时兜底
        // "image/png"，与 fodt.rs 既有的同名属性读取兜底策略一致。
        let mime = Self::extract_attr(buf, "mime-type").unwrap_or_else(|| "image/png".to_string());

        if !data_base64.is_empty() {
            let img = roe_model::calc::FloatingImage {
                id: 0,
                data_base64,
                mime,
                width_pt: w_pt,
                height_pt: h_pt,
                x_pt,
                y_pt,
                // ODF(fods)格式暂无对应rot/hidden语义来源，给默认值。
                rot: 0.0,
                hidden: false,
            };
            self.pending_images.push(img);
        }
    }

    fn extract_attr(xml: &str, attr: &str) -> Option<String> {
        let needle = format!("{}=\"", attr);
        let start = xml.find(&needle)? + needle.len();
        let end = xml[start..].find('"')? + start;
        Some(xml[start..end].to_string())
    }

    fn extract_between(xml: &str, tag: &str) -> Option<String> {
        let open = format!("<{}>", tag);
        let close = format!("</{}>", tag);
        let start = xml.find(&open)? + open.len();
        let end = xml.find(&close)?;
        if end > start { Some(xml[start..end].to_string()) } else { None }
    }


    fn parse_shapes_charts(&mut self) {
        use super::fodp::parse_chart_from_xml;
        let xml = self.shapes_xml_buf.clone();
        // DEVROOT-001: shapes_xml_buf 本身是完整的 "<shapes>...</shapes>"
        // 一整段，parse_tree 后 root(#root虚拟根) 的直接孩子是 "shapes"
        // 节点本身，draw:frame 是 shapes 节点的孩子（孙子层），不是
        // root 的直接孩子。此前 root.children_tag("draw:frame") 找错了
        // 层级，即使标签名拼接问题修复后依然返回空。
        if let Ok(root) = parse_tree(&xml) {
            let shapes_node = root.first_child_tag("shapes");
            let frames: Vec<&super::xml_tree::XNode> = shapes_node
                .map(|s| s.children_tag("draw:frame"))
                .unwrap_or_default();
            for frame in frames {
                // 图表
                // DEVROOT-001 关联: 这里同样是 self.sheets.last_mut() 误用，
                // 且 commit_sheet() 里 floating_charts 目前硬编码 vec![]，
                // 未接 pending 字段，图表本轮不在范围内（v146/接口缺口分析
                // 均明确图表写出/解析拆到后续任务），暂不动，仅记录不修。
                if let Some(obj) = frame.first_child_tag("draw:object") {
                    if let Some(chart) = parse_chart_from_xml(obj) {
                        if let Some(sheet) = self.sheets.last_mut() {
                            let mut chart = chart;
                            chart.id = sheet.floating_charts.iter().map(|c| c.id).max().unwrap_or(0) + 1;
                            sheet.floating_charts.push(chart);
                        }
                    }
                }
                // 图片
                // DEVROOT-001 修复: 此前用 self.sheets.last_mut() 在 sheet
                // commit 之前（table 结束标签触发 commit_sheet，晚于
                // table:shapes 结束标签）永远拿不到当前构建中的 sheet，
                // 数据静默丢弃。改为与 parse_frame_image 路径（941行）一致，
                // 写入 self.pending_images，由 commit_sheet() 统一收编。
                if let Some(img_node) = frame.first_child_tag("draw:image") {
                    if let Some(bin_node) = img_node.first_child_tag("office:binary-data") {
                        let data_base64 = bin_node.all_text().split_whitespace().collect::<String>();
                        if !data_base64.is_empty() {
                            let w_str = frame.attr("svg:width").unwrap_or("0");
                            let h_str = frame.attr("svg:height").unwrap_or("0");
                            let x_str = frame.attr("svg:x").unwrap_or("0");
                            let y_str = frame.attr("svg:y").unwrap_or("0");
                            // DEVROOT-002修复：mime-type 属性挂在 office:binary-data
                            // 标签上（与 fods_writer.rs 写出位置一致），bin_node 上读取。
                            let mime = bin_node.attr("mime-type").unwrap_or("image/png").to_string();
                            let img = roe_model::calc::FloatingImage {
                                id: 0,
                                data_base64,
                                mime,
                                width_pt:  Self::parse_length_pt(w_str).unwrap_or(100.0),
                                height_pt: Self::parse_length_pt(h_str).unwrap_or(100.0),
                                x_pt:      Self::parse_length_pt(x_str).unwrap_or(0.0),
                                y_pt:      Self::parse_length_pt(y_str).unwrap_or(0.0),
                                // ODF(fods)格式暂无对应rot/hidden语义来源，给默认值。
                                rot: 0.0,
                                hidden: false,
                            };
                            self.pending_images.push(img);
                        }
                    }
                }
            }
        }
    }

    fn commit_sheet(&mut self) {
        let grid = build_sparse_grid(std::mem::take(&mut self.current_rows));

        // 把 cell_validation_refs 按 validation_name 分组，矩形合并，生成 DataValidationRule
        let mut by_name: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
        for (row, col, name) in std::mem::take(&mut self.cell_validation_refs) {
            by_name.entry(name).or_default().push((row, col));
        }
        let mut data_validations = Vec::new();
        for (name, coords) in by_name {
            let def = match self.content_validations.get(&name) {
                Some(d) => d,
                None => continue, // condition 解析失败或规则未定义，跳过
            };
            let rects = super::odf_data_validation::merge_coords_to_rects(coords);
            for (r1, c1, r2, c2) in rects {
                data_validations.push(roe_model::calc::DataValidationRule {
                    range: roe_model::calc::CellRange {
                        sheet: String::new(),
                        start: (r1, c1),
                        end: (r2, c2),
                    },
                    kind: def.0.clone(),
                    operator: def.1.clone(),
                    formula1: def.2.clone(),
                    formula2: def.3.clone(),
                    allow_blank: def.4,
                    show_error: def.5,
                    error_msg: def.6.clone(),
                });
            }
        }

        let sheet = Sheet {
            col_style: vec![],
            tables: vec![],
            name: std::mem::take(&mut self.current_sheet_name),
            cells: grid,
            col_widths: std::mem::take(&mut self.col_widths),
            row_heights: std::mem::take(&mut self.row_heights)
                .into_iter()
                .map(roe_model::calc::RowHeight::Manual)
                .collect(),
            frozen_rows: 0,
            frozen_cols: 0,
        date1904: false, // ROOT-041: csv/odf场景无workbookPr概念，固定为1900系统
            zoom_scale: None,
            show_grid_lines: None,
            col_hidden: vec![],
            row_hidden: vec![],
            col_outline_level: vec![],
            row_outline_level: vec![],
            sparkline_groups: vec![],
            cell_formats:     std::collections::HashMap::new(),
            cond_formats:     vec![],
            data_validations,
            auto_filter:      None,
            merged_cells:     vec![],
            pivot:            None,
            floating_charts:  vec![],
            floating_stat_charts: vec![],
            floating_images:  std::mem::take(&mut self.pending_images),
            floating_shapes: vec![],
            next_object_id: 0,
            default_col_width: 0.0,
            default_row_height: 0.0,
        };
        self.sheets.push(sheet);
    }

    fn finish(self) -> Workbook {
        Workbook {
            sheets: self.sheets,
            named_ranges: self.named_ranges,
            named_formulas: std::collections::HashMap::new(),
            styles: StyleRegistry::default(),
            formula_dialect_source: FormulaDialectSource::OpenFormula,
        }
    }
}

// ──────────────────────────────────────────────
// SparseGrid 构建
// ──────────────────────────────────────────────

/// 把 (row, col, value) 列表转成 SparseGrid（列存 + Block 合并）
fn build_sparse_grid(mut cells: Vec<(u32, u32, CellValue)>) -> SparseGrid {
    // 按 col, row 排序
    cells.sort_by_key(|&(r, c, _)| (c, r));

    let mut grid = SparseGrid::default();

    let mut i = 0;
    while i < cells.len() {
        let col = cells[i].1;
        // 收集同一列的所有 cell
        let mut col_cells: Vec<(u32, CellValue)> = Vec::new();
        while i < cells.len() && cells[i].1 == col {
            col_cells.push((cells[i].0, cells[i].2.clone()));
            i += 1;
        }
        // 构建 SparseColumn：把连续行合并为 Block
        let sparse_col = build_sparse_column(col_cells);
        grid.cols.insert(col, sparse_col);
    }
    grid
}

fn build_sparse_column(cells: Vec<(u32, CellValue)>) -> SparseColumn {
    let mut blocks: SmallVec<[CellBlock; 4]> = SmallVec::new();

    for (row, value) in cells {
        // 尝试合并到最后一个 block
        if let Some(last) = blocks.last_mut() {
            let next_row = last.start_row + last.values.len() as u32;
            if row == next_row {
                last.values.push(value);
                continue;
            }
        }
        // 新建 block
        let mut values = SmallVec::new();
        values.push(value);
        blocks.push(CellBlock { start_row: row, values });
    }

    SparseColumn { blocks }
}

// ──────────────────────────────────────────────
// 辅助函数
// ──────────────────────────────────────────────

/// 去掉命名空间前缀，取本地名（借用版）
fn local_name(name: &[u8]) -> &str {
    let s = std::str::from_utf8(name).unwrap_or("");
    if let Some(pos) = s.rfind(':') {
        &s[pos + 1..]
    } else {
        s
    }
}

/// 去掉命名空间前缀，返回拥有所有权的 String（用于 run() 里的 event 处理）
fn local_name_owned(name: &[u8]) -> String {
    local_name(name).to_string()
}

/// 解析 ODF 度量值，统一转为 pt（f32）
/// 支持: cm, mm, in, pt, px
/// （2026-06-18 修复：原先统一转cm，与fodt.rs/fods_writer.rs假设的pt不一致，
///  现与 fodp.rs / fodt.rs 统一为 pt 语义）
/// 解析 ODF table:cell-range-address 属性值，格式如
/// "$Sheet1.$B$2:$Sheet1.$C$10" 或不带 $ 的 "Sheet1.B2:Sheet1.C10"。
/// ODF 用 "." 分隔 sheet 名和单元格坐标（不是 xlsx 的 "!"），且
/// range 两端各自带独立的 sheet 前缀（理论上应该相同，这里只取第一个）。
fn parse_odf_named_range_address(raw: &str) -> Option<roe_model::calc::CellRange> {
    let cleaned = raw.replace('$', "");
    let (start_part, end_part) = cleaned.split_once(':').unwrap_or((cleaned.as_str(), cleaned.as_str()));

    let (sheet, start_cell) = start_part.split_once('.')?;
    let end_cell = end_part.split_once('.').map(|(_, c)| c).unwrap_or(end_part);

    let (r1, c1) = parse_odf_cell_ref(start_cell)?;
    let (r2, c2) = parse_odf_cell_ref(end_cell)?;

    Some(roe_model::calc::CellRange {
        sheet: sheet.to_string(),
        start: (r1, c1),
        end: (r2, c2),
    })
}

/// 解析 "B2" → (row, col)，0-based
fn parse_odf_cell_ref(addr: &str) -> Option<(u32, u32)> {
    let col_str: String = addr.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    let row_str: String = addr.chars().skip_while(|c| c.is_ascii_alphabetic()).collect();
    if col_str.is_empty() || row_str.is_empty() {
        return None;
    }
    let mut col = 0u32;
    for ch in col_str.chars() {
        col = col * 26 + (ch.to_ascii_uppercase() as u32 - 'A' as u32 + 1);
    }
    let row: u32 = row_str.parse().ok()?;
    Some((row.saturating_sub(1), col.saturating_sub(1)))
}

fn parse_measure(s: &str) -> f32 {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("cm") {
        n.trim().parse::<f32>().unwrap_or(0.0) * 28.3465
    } else if let Some(n) = s.strip_suffix("mm") {
        n.trim().parse::<f32>().unwrap_or(0.0) * 2.83465
    } else if let Some(n) = s.strip_suffix("in") {
        n.trim().parse::<f32>().unwrap_or(0.0) * 72.0
    } else if let Some(n) = s.strip_suffix("pt") {
        n.trim().parse::<f32>().unwrap_or(0.0)
    } else if let Some(n) = s.strip_suffix("px") {
        n.trim().parse::<f32>().unwrap_or(0.0) * 0.75
    } else {
        0.0
    }
}

// ──────────────────────────────────────────────
// 测试
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE_FODS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
                 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
                 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
                 xmlns:of="urn:oasis:names:tc:opendocument:xmlns:of:1.2"
                 office:mimetype="application/vnd.oasis.opendocument.spreadsheet">
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Sheet1">
        <table:table-column table:number-columns-repeated="3"/>
        <table:table-row>
          <table:table-cell office:value-type="string">
            <text:p>Hello</text:p>
          </table:table-cell>
          <table:table-cell office:value-type="float" office:value="42">
            <text:p>42</text:p>
          </table:table-cell>
          <table:table-cell office:value-type="boolean" office:boolean-value="true">
            <text:p>TRUE</text:p>
          </table:table-cell>
        </table:table-row>
        <table:table-row>
          <table:table-cell table:formula="of:=B1*2" office:value-type="float" office:value="84">
            <text:p>84</text:p>
          </table:table-cell>
          <table:table-cell/>
          <table:table-cell office:value-type="float" office:value="-3.14">
            <text:p>-3.14</text:p>
          </table:table-cell>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document>"#;

    #[test]
    fn test_parse_simple() {
        let wb = FodsParser::parse(SIMPLE_FODS).unwrap();
        assert_eq!(wb.sheets.len(), 1);
        let sheet = &wb.sheets[0];
        assert_eq!(sheet.name, "Sheet1");

        // R0C0 = "Hello"
        let v = get_cell(sheet, 0, 0);
        assert!(matches!(v, Some(CellValue::Text(s)) if s == "Hello"), "R0C0={:?}", v);

        // R0C1 = 42.0
        let v = get_cell(sheet, 0, 1);
        assert!(matches!(v, Some(CellValue::Number(n)) if (*n - 42.0).abs() < 1e-9), "R0C1={:?}", v);

        // R0C2 = true
        let v = get_cell(sheet, 0, 2);
        assert!(matches!(v, Some(CellValue::Bool(true))), "R0C2={:?}", v);

        // R1C0 = Formula("=B1*2")
        let v = get_cell(sheet, 1, 0);
        assert!(matches!(v, Some(CellValue::Formula { .. })), "R1C0={:?}", v);

        // R1C1 = Empty（跳过，不在 grid 里）
        let v = get_cell(sheet, 1, 1);
        assert!(v.is_none() || matches!(v, Some(CellValue::Empty)), "R1C1={:?}", v);

        // R1C2 = -3.14
        let v = get_cell(sheet, 1, 2);
        assert!(matches!(v, Some(CellValue::Number(n)) if (*n - (-3.14)).abs() < 1e-6), "R1C2={:?}", v);
    }

    #[test]
    fn test_named_range_basic_and_cross_reference() {
        let xml = r#"<?xml version="1.0"?>
<office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
                 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
                 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
                 office:mimetype="application/vnd.oasis.opendocument.spreadsheet">
  <office:body>
    <office:spreadsheet>
      <table:named-range table:name="SalesTotal"
                          table:base-cell-address="$Sheet1.$A$1"
                          table:cell-range-address="$Sheet1.$B$2:$Sheet1.$C$10"/>
      <table:named-range table:name="TaxRate"
                          table:base-cell-address="$Sheet1.$A$1"
                          table:cell-range-address="$Sheet1.$D$1"/>
      <table:table table:name="Sheet1">
        <table:table-row>
          <table:table-cell office:value-type="float" office:value="1"><text:p>1</text:p></table:table-cell>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document>"#;
        let wb = FodsParser::parse(xml).unwrap();
        assert_eq!(wb.named_ranges.len(), 2);

        let sales = wb.named_ranges.get("SalesTotal").expect("SalesTotal should be parsed");
        assert_eq!(sales.sheet, "Sheet1");
        assert_eq!(sales.start, (1, 1)); // B2 0-based -> row1,col1
        assert_eq!(sales.end, (9, 2));   // C10 0-based -> row9,col2

        let tax = wb.named_ranges.get("TaxRate").expect("TaxRate should be parsed");
        assert_eq!(tax.sheet, "Sheet1");
        assert_eq!(tax.start, (0, 3)); // D1 0-based -> row0,col3
        assert_eq!(tax.end, (0, 3));

        println!("test_named_range_basic_and_cross_reference ✓");
    }

    #[test]
    fn test_multi_sheet() {
        let xml = r#"<?xml version="1.0"?>
<office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
                 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
                 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
                 office:mimetype="application/vnd.oasis.opendocument.spreadsheet">
  <office:body>
    <office:spreadsheet>
      <table:table table:name="Alpha">
        <table:table-row>
          <table:table-cell office:value-type="float" office:value="1"><text:p>1</text:p></table:table-cell>
        </table:table-row>
      </table:table>
      <table:table table:name="Beta">
        <table:table-row>
          <table:table-cell office:value-type="string"><text:p>world</text:p></table:table-cell>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document>"#;
        let wb = FodsParser::parse(xml).unwrap();
        assert_eq!(wb.sheets.len(), 2);
        assert_eq!(wb.sheets[0].name, "Alpha");
        assert_eq!(wb.sheets[1].name, "Beta");
    }

    /// 辅助：从 Sheet 里取某个 (row, col) 的值（返回引用）
    fn get_cell(sheet: &Sheet, row: u32, col: u32) -> Option<&CellValue> {
        let sc = sheet.cells.cols.get(&col)?;
        for block in &sc.blocks {
            if row >= block.start_row {
                let idx = (row - block.start_row) as usize;
                if idx < block.values.len() {
                    return Some(&block.values[idx]);
                }
            }
        }
        None
    }

    // ──────────────────────────────────────────────
    // ODF dataValidation 读方向测试（v101 补测）
    // ──────────────────────────────────────────────

    fn dv_xml(content_validations_block: &str, cell_extra: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
                 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
                 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
                 xmlns:of="urn:oasis:names:tc:opendocument:xmlns:of:1.2"
                 office:mimetype="application/vnd.oasis.opendocument.spreadsheet">
  <office:body>
    <office:spreadsheet>
      {cv_block}
      <table:table table:name="Sheet1">
        <table:table-row>
          <table:table-cell {cell_extra} office:value-type="float" office:value="1">
            <text:p>1</text:p>
          </table:table-cell>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document>"#,
            cv_block = content_validations_block,
            cell_extra = cell_extra,
        )
    }

    #[test]
    fn test_dv_whole_between() {
        let cv = r#"<table:content-validations>
        <table:content-validation table:name="dv_whole" table:condition="oooc:cell-content-is-whole-number() and cell-content-is-between(1,10)" table:allow-empty-cell="true">
          <table:error-message table:display="true"><text:p>Enter 1-10</text:p></table:error-message>
        </table:content-validation>
      </table:content-validations>"#;
        let xml = dv_xml(cv, r#"table:validation-name="dv_whole""#);
        let wb = FodsParser::parse(&xml).unwrap();
        let sheet = &wb.sheets[0];
        assert_eq!(sheet.data_validations.len(), 1, "dvs={:?}", sheet.data_validations);
        let rule = &sheet.data_validations[0];
        assert_eq!(rule.kind, "whole");
        assert_eq!(rule.operator, "between");
        assert_eq!(rule.formula1, "1");
        assert_eq!(rule.formula2.as_deref(), Some("10"));
        assert!(rule.allow_blank);
        assert!(rule.show_error);
        assert_eq!(rule.error_msg.as_deref(), Some("Enter 1-10"));
        assert_eq!(rule.range.start, (0, 0));
        assert_eq!(rule.range.end, (0, 0));
    }

    #[test]
    fn test_dv_decimal_less_than() {
        let cv = r#"<table:content-validations>
        <table:content-validation table:name="dv_dec" table:condition="oooc:cell-content-is-decimal-number() and cell-content()&lt;100" table:allow-empty-cell="false">
        </table:content-validation>
      </table:content-validations>"#;
        let xml = dv_xml(cv, r#"table:validation-name="dv_dec""#);
        let wb = FodsParser::parse(&xml).unwrap();
        let sheet = &wb.sheets[0];
        assert_eq!(sheet.data_validations.len(), 1, "dvs={:?}", sheet.data_validations);
        let rule = &sheet.data_validations[0];
        assert_eq!(rule.kind, "decimal");
        assert_eq!(rule.operator, "lessThan");
        assert_eq!(rule.formula1, "100");
        assert!(!rule.allow_blank);
        assert!(!rule.show_error);
    }

    #[test]
    fn test_dv_text_length_between() {
        let cv = r#"<table:content-validations>
        <table:content-validation table:name="dv_len" table:condition="oooc:cell-content-text-length-is-between(2,5)" table:allow-empty-cell="true">
        </table:content-validation>
      </table:content-validations>"#;
        let xml = dv_xml(cv, r#"table:validation-name="dv_len""#);
        let wb = FodsParser::parse(&xml).unwrap();
        let sheet = &wb.sheets[0];
        assert_eq!(sheet.data_validations.len(), 1, "dvs={:?}", sheet.data_validations);
        let rule = &sheet.data_validations[0];
        assert_eq!(rule.kind, "textLength");
        assert_eq!(rule.operator, "between");
        assert_eq!(rule.formula1, "2");
        assert_eq!(rule.formula2.as_deref(), Some("5"));
    }

    #[test]
    fn test_dv_list() {
        let cv = r#"<table:content-validations>
        <table:content-validation table:name="dv_list" table:condition="oooc:cell-content-is-in-list(&quot;a&quot;;&quot;b&quot;;&quot;c&quot;)" table:allow-empty-cell="true">
        </table:content-validation>
      </table:content-validations>"#;
        let xml = dv_xml(cv, r#"table:validation-name="dv_list""#);
        let wb = FodsParser::parse(&xml).unwrap();
        let sheet = &wb.sheets[0];
        assert_eq!(sheet.data_validations.len(), 1, "dvs={:?}", sheet.data_validations);
        let rule = &sheet.data_validations[0];
        assert_eq!(rule.kind, "list");
    }

    #[test]
    fn test_dv_empty_cell_still_gets_rule() {
        // 规则挂在空单元格上（无 value-type，无内容），验证 commit_cell 的修复生效：
        // 即使单元格值为空，只要有 validation-name 引用也要记录进 data_validations。
        let cv = r#"<table:content-validations>
        <table:content-validation table:name="dv_empty" table:condition="oooc:cell-content-is-whole-number() and cell-content()&gt;=0" table:allow-empty-cell="true">
        </table:content-validation>
      </table:content-validations>"#;
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
                 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
                 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
                 office:mimetype="application/vnd.oasis.opendocument.spreadsheet">
  <office:body>
    <office:spreadsheet>
      {cv}
      <table:table table:name="Sheet1">
        <table:table-row>
          <table:table-cell table:validation-name="dv_empty"/>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document>"#,
            cv = cv,
        );
        let wb = FodsParser::parse(&xml).unwrap();
        let sheet = &wb.sheets[0];
        assert_eq!(sheet.data_validations.len(), 1, "dvs={:?}", sheet.data_validations);
        assert_eq!(sheet.data_validations[0].kind, "whole");
        assert_eq!(sheet.data_validations[0].operator, "greaterThanOrEqual");
    }

    #[test]
    fn test_dv_rect_merge_adjacent_cells() {
        // 同一 validation-name 挂在相邻两行同列的单元格上，应合并为一个矩形 range
        let cv = r#"<table:content-validations>
        <table:content-validation table:name="dv_rect" table:condition="oooc:cell-content-is-whole-number() and cell-content-is-between(1,5)" table:allow-empty-cell="true">
        </table:content-validation>
      </table:content-validations>"#;
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
                 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
                 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
                 office:mimetype="application/vnd.oasis.opendocument.spreadsheet">
  <office:body>
    <office:spreadsheet>
      {cv}
      <table:table table:name="Sheet1">
        <table:table-row>
          <table:table-cell table:validation-name="dv_rect" office:value-type="float" office:value="1"><text:p>1</text:p></table:table-cell>
        </table:table-row>
        <table:table-row>
          <table:table-cell table:validation-name="dv_rect" office:value-type="float" office:value="2"><text:p>2</text:p></table:table-cell>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document>"#,
            cv = cv,
        );
        let wb = FodsParser::parse(&xml).unwrap();
        let sheet = &wb.sheets[0];
        assert_eq!(sheet.data_validations.len(), 1, "expected merge into 1 rect, got: {:?}", sheet.data_validations);
        let rule = &sheet.data_validations[0];
        assert_eq!(rule.range.start, (0, 0));
        assert_eq!(rule.range.end, (1, 0));
    }
}

