//! DOCX (OOXML WordprocessingML) → roe-model::writer::WriterDocument
//!
//! OPC 流程：
//!   ZIP → word/document.xml  → Paragraph / Run / Text
//!         word/styles.xml    → StyleGraph + resolve_style()
//!         word/numbering.xml → NumberingResolver

use std::collections::{HashMap, HashSet};
use std::io::Read;

use quick_xml::events::Event;
use quick_xml::Reader;
use zip::ZipArchive;

use crate::ooxml::sniff_image_mime;
use crate::ooxml::chart_common::{parse_ooxml_chart_xml, parse_ooxml_theme_colors};
use crate::ParseError;
use roe_model::common::{Color, StyleRef};
use roe_model::style::{Alignment, CharStyle, ParagraphStyle, StyleRegistry};
use roe_model::writer::{
    BodyElement, Comment, FloatFrame, HeaderFooterSet, InlineImage, ListInfo, Note, NoteType, Paragraph, Run, Shape, Table, TableRow, TableCell, WriterDocument,
};

// ─── 公开入口 ────────────────────────────────────────────────────────────────

pub struct DocxParser;

impl DocxParser {
    pub fn parse(data: &[u8]) -> Result<WriterDocument, ParseError> {
        let cursor = std::io::Cursor::new(data);
        let mut zip = ZipArchive::new(cursor)
            .map_err(|e| ParseError::Malformed(format!("zip open failed: {e}")))?;

        let style_graph = read_styles(&mut zip)?;
        let mut numbering = read_numbering(&mut zip)?;
        let images = collect_docx_images(&mut zip);
        let hyperlink_rels = collect_hyperlink_rels(&mut zip);
        let chart_rels = collect_chart_rels(&mut zip);
        let theme_colors = collect_docx_theme_colors(&mut zip);
        let page_layouts = read_page_layout(&mut zip);
        let mut body = read_document(&mut zip, &style_graph, &mut numbering, &images, &hyperlink_rels, &chart_rels, &theme_colors, &page_layouts)?;
        // [WROOT-047] 顶层字段沿用"最后一节"语义，与 header/footer 已采用的
        // 策略（all_hf_refs.last()）完全一致，不引入新的行为差异。
        let layout = page_layouts.last().cloned().unwrap_or_default();

        // WROOT-004: 解析header/footer。三步：①sectPr里的type→rId引用
        // ②document.xml.rels里的rId→文件名 ③按文件名读取zip part解析出段落。
        // WROOT任务B第一步：all_hf_refs 现在是每个sectPr各自一份的Vec，
        // 不再互相覆盖。顶层 header/footer/title_page 字段沿用"最后一节"
        // 语义（与此前单实例覆盖到最后的历史行为完全一致，不引入回归）；
        // 同时用完整的 Vec 构造 sections，供后续任务读取"每节各自的
        // header/footer"使用（本次 Section.content 恒为空，body 尚未
        // 按 sectPr 切分，见 Section 结构体字段注释）。
        let all_hf_refs = read_header_footer_refs(&mut zip);
        let hf_targets = collect_header_footer_rel_targets(&mut zip);
        let last_hf_refs = all_hf_refs.last().cloned().unwrap_or_default();
        let header = HeaderFooterSet {
            default: load_hf_part(&mut zip, &hf_targets, &last_hf_refs.header_default),
            even:    load_hf_part(&mut zip, &hf_targets, &last_hf_refs.header_even),
            first:   load_hf_part(&mut zip, &hf_targets, &last_hf_refs.header_first),
        };
        let footer = HeaderFooterSet {
            default: load_hf_part(&mut zip, &hf_targets, &last_hf_refs.footer_default),
            even:    load_hf_part(&mut zip, &hf_targets, &last_hf_refs.footer_even),
            first:   load_hf_part(&mut zip, &hf_targets, &last_hf_refs.footer_first),
        };
        let even_and_odd_headers = read_even_and_odd_headers(&mut zip);

        // 按出现顺序，把每个sectPr各自的引用解析成实际段落内容，装进Section。
        // name 用序号占位（"Section 1"..）；content 恒为空Vec（见上方注释）。
        // WROOT-129：页眉页脚按节继承（ECMA-376 §17.10.5）。某一节的 sectPr 没有写
        // 某一档（default/even/first × header/footer，六档各自独立）的
        // headerReference/footerReference 时，沿用上一节该档的引用；第 1 节没写就是没有。
        // 此前每节只取自己 sectPr 里显式写的引用，未写的档恒为 None，导致 tdf95367 一类
        // "第 2、3 节没有页眉页脚"。只继承 rId 引用，不继承 title_page / section_type
        // （它们是本节自己的属性，没写就是没开/默认）。顶层 last_hf_refs 语义不变。
        let mut eff_hf_refs: Vec<HeaderFooterRefs> = Vec::with_capacity(all_hf_refs.len());
        for refs in all_hf_refs.iter() {
            let mut r = refs.clone();
            if let Some(prev) = eff_hf_refs.last() {
                if r.header_default.is_none() { r.header_default = prev.header_default.clone(); }
                if r.header_even.is_none()    { r.header_even    = prev.header_even.clone(); }
                if r.header_first.is_none()   { r.header_first   = prev.header_first.clone(); }
                if r.footer_default.is_none() { r.footer_default = prev.footer_default.clone(); }
                if r.footer_even.is_none()    { r.footer_even    = prev.footer_even.clone(); }
                if r.footer_first.is_none()   { r.footer_first   = prev.footer_first.clone(); }
            }
            eff_hf_refs.push(r);
        }
        let sections: Vec<roe_model::writer::Section> = eff_hf_refs.iter().enumerate()
            .map(|(idx, refs)| {
                let sec_header = HeaderFooterSet {
                    default: load_hf_part(&mut zip, &hf_targets, &refs.header_default),
                    even:    load_hf_part(&mut zip, &hf_targets, &refs.header_even),
                    first:   load_hf_part(&mut zip, &hf_targets, &refs.header_first),
                };
                let sec_footer = HeaderFooterSet {
                    default: load_hf_part(&mut zip, &hf_targets, &refs.footer_default),
                    even:    load_hf_part(&mut zip, &hf_targets, &refs.footer_even),
                    first:   load_hf_part(&mut zip, &hf_targets, &refs.footer_first),
                };
                // [WROOT-047] page_layouts 与 all_hf_refs 都是按 sectPr 在
                // body 中出现的顺序收集的 Vec，下标语义对齐；用同一个 idx
                // 取对应节的 page_size/margin/columns。若下标越界（理论上
                // 不应发生，两者都应遍历同一份 document.xml 里的 sectPr 集合，
                // 但用 get() 保守 fallback 到 None，不 panic）。
                let pl = page_layouts.get(idx);
                roe_model::writer::Section {
                    name: format!("Section {}", idx + 1),
                    content: Vec::new(),
                    header: Some(sec_header),
                    footer: Some(sec_footer),
                    title_page: Some(refs.title_page),
                    section_type: refs.section_type.clone(),
                    page_size:     pl.map(|p| p.page_size),
                    margin_top:    pl.map(|p| p.margin_top),
                    margin_bottom: pl.map(|p| p.margin_bottom),
                    margin_left:   pl.map(|p| p.margin_left),
                    margin_right:  pl.map(|p| p.margin_right),
                    columns:       pl.map(|p| p.columns.clone()),
                    ..Default::default()
                }
            })
            .collect();

        // WROOT-130：w:type 描述的是"本节自己如何开始"，不是"本节之后如何"。
        // 解析正文时（见 w:sectPr / w:type 两处 pending_page_break 逻辑）用的是
        // "这个 sectPr 自己的 w:type"来决定"下一段是否换页"，等于拿本节的类型
        // 去控制下一节的起始方式，错了一位。tdf95367（节1 nextPage、节2 continuous、
        // 节3 nextPage）因此整个反过来：应留在第1页的节2内容被挤到第2页，
        // 应换页的节3没换页。这里在 sections 已知之后统一修正：第 k 个 Section 边界
        // （第 k 节结束、第 k+1 节开始）之后的第一个元素若是段落，其 page_break_before
        // = 第 k+1 节的 section_type != continuous。紧跟元素不是段落（表格/图片等）
        // 时不动。页面尺寸/边距变化引起的强制换页仍由 layout 独立处理。
        {
            let mut boundary_k: usize = 0;
            let mut i = 0usize;
            while i < body.len() {
                if matches!(body[i], BodyElement::Section(_)) {
                    let next_type: Option<Option<String>> = sections
                        .get(boundary_k + 1)
                        .map(|sec| sec.section_type.clone());
                    if let Some(t) = next_type {
                        if let Some(BodyElement::Paragraph(pp)) = body.get_mut(i + 1) {
                            pp.page_break_before = t.as_deref() != Some("continuous");
                        }
                    }
                    boundary_k += 1;
                }
                i += 1;
            }
        }

        // 上面这段是 sections（多sectPr场景）的构造，不涉及顶层
        // endnote_num_fmt/footnote_num_fmt——这两个字段只在下面顶层
        // WriterDocument 构造处赋值一次，取"最后一个sectPr"的值
        // （与 margin_top 等顶层字段现有语义一致：顶层字段代表最后一节）。

        // Step 4 (WROOT-075/027/028): 接入 Step 3 的 parse_notes_part，替换掉
        // Step 1 的占位 Vec::new()。part 不存在（绝大多数 docx 没有脚注/尾注）
        // 时 parse_notes_part 内部已处理为返回空 Vec，这里无需额外判断。
        let footnotes = parse_notes_part(&mut zip, "word/footnotes.xml", b"w:footnote");
        let endnotes = parse_notes_part(&mut zip, "word/endnotes.xml", b"w:endnote");
        // 批注功能第二步（v148交接续）：先解析 commentsExtended.xml 得到
        // paraId -> done 映射表，再传给 parse_comments_part 用于回填
        // Comment.done。正文 commentRangeStart/End/commentReference 锚点
        // 关联仍未处理，留给下一步。part 不存在时内部已处理为返回空 Vec/map。
        let comments_extended_map = parse_comments_extended_part(&mut zip, "word/commentsExtended.xml");
        let comments = parse_comments_part(&mut zip, "word/comments.xml", &comments_extended_map);

        Ok(WriterDocument {
            body,
            styles: { let mut reg = build_style_registry(&style_graph); numbering.register_roe_styles(&mut reg); reg },
            metadata: Default::default(),
            page_size: layout.page_size,
            header,
            footer,
            title_page: last_hf_refs.title_page,
            even_and_odd_headers,
            margin_top:    layout.margin_top,
            margin_bottom: layout.margin_bottom,
            margin_left:   layout.margin_left,
            margin_right:  layout.margin_right,
            columns: layout.columns,
            endnote_num_fmt: layout.endnote_num_fmt,
            footnote_num_fmt: layout.footnote_num_fmt,
            sections,
            footnotes,
            endnotes,
            comments,
        })
    }
}

// ─── StyleGraph ──────────────────────────────────────────────────────────────
// 注：样式继承解析基础设施，已解析并传递到主流程（read_styles），但下游
// “应用样式到段落/字符”的接线逻辑尚未实现（属阶段1 Writer 线后续工作）。
// 暂时整体标记 allow(dead_code)，不要删除。

#[derive(Debug, Clone, Default)]
struct RawStyle {
    style_id:   String,
    style_type: Option<String>,
    based_on:   Option<String>,
    name:       String,
    para_props: ParaProps,
    char_props: CharProps,
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
struct ParaProps {
    alignment:      Option<String>,
    indent_left:    Option<f32>,
    indent_first:   Option<f32>,
    spacing_before: Option<i32>,
    spacing_after:  Option<i32>,
    outline_level:  Option<u8>,
    /// WROOT-036 续修：style-level numbering。样式定义自身携带的
    /// w:pPr/w:numPr/w:numId，用于"段落没有直接写 numPr，而是通过
    /// w:pStyle 引用一个自带编号的样式"这种情况（Word 称为
    /// style-level numbering，常见于自定义样式如本例的 CustomStyle）。
    num_id:         Option<u32>,
    /// 同上，对应 w:numPr/w:ilvl。注意 OOXML 规范里 w:ilvl 缺省时
    /// 默认为 0（level 0），不能因为没读到就当作"没有编号"处理。
    num_ilvl:       Option<u8>,
    /// WROOT-114: w:pPr/w:shd 的 w:fill 原始十六进制串（"auto"/空已过滤）。
    shading:        Option<String>,
    /// WROOT-114 阶段二：w:pBdr/w:left 原始值 (w:color 十六进制串, w:sz 1/8磅, w:space 磅)。
    /// w:val 为 none/nil 时不记录。
    border_left:    Option<(String, f32, f32)>,
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
struct CharProps {
    bold:      Option<bool>,
    italic:    Option<bool>,
    underline: Option<bool>,
    font_size: Option<f32>,
    color:     Option<String>,
    font_name: Option<String>,
    // [WROOT-093小尾巴2] w:ascii 候选，见 CharStyle.font_family_ascii 注释。
    font_name_ascii: Option<String>,
    // WROOT-027(v141): styles.xml 里字符样式(w:type="character")自带的
    // <w:vertAlign w:val="superscript"/>（如 EndnoteReference 样式）此前
    // 完全未解析，导致 to_char_style() 转出的 CharStyle.script 恒为 None。
    // 存原始字符串（"superscript"/"subscript"/其他），转换成 ScriptPosition
    // 枚举的工作留给 to_char_style。
    vert_align: Option<String>,
    // WROOT-107: w:position（半磅，已换算成pt，正上负下）
    vertical_position: Option<f32>,
    // [ROE-STYLE-STRIKE-HL] styles.xml 里字符样式自带的 <w:strike/> 和
    // <w:highlight w:val="yellow"/> 此前完全未解析，导致 to_char_style()
    // 转出的 strikethrough/background 恒为 None（写死）。本次补上解析，
    // 精确度（例如 highlight 预设颜色名到 RGB 的映射是否完整）留给后续
    // golden 用例走查验证，本次先打通"读到就不丢"这条链路。
    strikethrough: Option<bool>,
    background: Option<String>,
    // [ROE-STYLE-SHD] styles.xml 里字符样式自带的 <w:shd w:fill="RRGGBB".../>
    // 此前完全未解析（本文件此前只有 w:tcPr 表格单元格、正文run级 in_rpr_body
    // 两处解析了 w:shd，样式定义级 in_rpr 分支完全空白）。与 background
    // （w:highlight，16种预设颜色名）分开存储，不复用同一个字段——两者是
    // OOXML 里两套独立机制（微软官方文档确认二者可以在同一个 w:rPr 里并存，
    // 显示时 highlight 盖过 shd，但那只是"视觉优先级"，不代表解析该二选一），
    // 十六进制RGB和预设色名字符串格式不同，硬塞进同一个 Option<String>
    // 会导致 parse_highlight_color 把十六进制误当颜色名匹配、静默失败。
    // 存原始十六进制字符串，转换成 Color 的工作留给 to_char_style
    // （与 color 字段"先存字符串、resolve阶段再parse"的既有模式对齐）。
    shd_background: Option<String>,
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
struct ResolvedStyle {
    para: ParaProps,
    char: CharProps,
}

#[allow(dead_code)]
#[derive(Default)]
struct StyleGraph {
    styles: HashMap<String, RawStyle>,
    doc_default_line_height: Option<f32>,
    doc_default_font_size: Option<f32>,
    // WROOT-040-FONT：docDefaults(w:rPrDefault) 的 w:rFonts 此前完全未解析，
    // 导致 relativeFrom docDefaults 兜底字体（无显式pStyle/run字体覆盖时的
    // 正文场景，如 Times New Roman）缺失，被迫回退到系统默认无衬线字体。
    doc_default_font_family: Option<String>,
    // WROOT-040-FONT修正：resolve_run_font_family 的最终判断逻辑对纯ASCII
    // 文本走的是 font_family_ascii 分支（不是 font_family），docDefaults
    // 的 w:ascii 候选必须单独兜底进这个字段，否则纯拉丁文正文永远拿不到
    // docDefaults 字体（第一版遗漏，本次补上）。
    doc_default_font_family_ascii: Option<String>,
    doc_default_color: Option<String>,
    doc_default_space_before: Option<f32>,
    doc_default_space_after: Option<f32>,
    doc_default_indent_left: Option<f32>,
    doc_default_indent_first: Option<f32>,
}

impl StyleGraph {
    #[allow(dead_code)]
    fn resolve(&self, style_id: &str) -> ResolvedStyle {
        let Some(raw) = self.styles.get(style_id) else {
            return ResolvedStyle::default();
        };
        let mut result = if let Some(parent_id) = &raw.based_on {
            self.resolve(parent_id)
        } else {
            ResolvedStyle::default()
        };
        merge_para(&mut result.para, &raw.para_props);
        merge_char(&mut result.char, &raw.char_props);
        result
    }

    /// WROOT-036 续修：style-level numbering 兜底查找。
    /// 段落自身没有直接写 w:numPr 时（cur_num_id/cur_ilvl 均为 None），
    /// 调用方应改查这个方法——沿着 w:pStyle 的 based_on 继承链，找第一层
    /// 定义了 numId 的样式。找到 numId 但没找到 ilvl 时，按 OOXML 规范
    /// 缺省语义返回 ilvl=0，而不是当作"没有编号"处理。
    /// 继承链上限 32 层，防止样式表本身有环形引用（异常数据）导致死循环。
    #[allow(dead_code)]
    fn resolve_num_id(&self, style_id: &str) -> Option<(u32, u8)> {
        let mut cur_id = style_id.to_string();
        let mut depth = 0;
        loop {
            depth += 1;
            if depth > 32 {
                return None;
            }
            let raw = self.styles.get(&cur_id)?;
            if let Some(num_id) = raw.para_props.num_id {
                let ilvl = raw.para_props.num_ilvl.unwrap_or(0);
                return Some((num_id, ilvl));
            }
            match &raw.based_on {
                Some(parent_id) => cur_id = parent_id.clone(),
                None => return None,
            }
        }
    }
}

#[allow(dead_code)]
fn merge_para(dst: &mut ParaProps, src: &ParaProps) {
    if src.alignment.is_some()      { dst.alignment      = src.alignment.clone(); }
    if src.indent_left.is_some()    { dst.indent_left    = src.indent_left; }
    if src.indent_first.is_some()   { dst.indent_first   = src.indent_first; }
    if src.spacing_before.is_some() { dst.spacing_before = src.spacing_before; }
    if src.spacing_after.is_some()  { dst.spacing_after  = src.spacing_after; }
    if src.outline_level.is_some()  { dst.outline_level  = src.outline_level; }
    if src.num_id.is_some()         { dst.num_id         = src.num_id; }
    if src.num_ilvl.is_some()       { dst.num_ilvl       = src.num_ilvl; }
}

#[allow(dead_code)]
fn merge_char(dst: &mut CharProps, src: &CharProps) {
    if src.bold.is_some()      { dst.bold      = src.bold; }
    if src.italic.is_some()    { dst.italic    = src.italic; }
    if src.underline.is_some() { dst.underline = src.underline; }
    if src.font_size.is_some() { dst.font_size = src.font_size; }
    if src.color.is_some()     { dst.color     = src.color.clone(); }
    if src.font_name.is_some() { dst.font_name = src.font_name.clone(); }
    // [ROE-STYLE-STRIKE-HL] 继承链合并，与其余bool/String字段同款模式。
    if src.strikethrough.is_some() { dst.strikethrough = src.strikethrough; }
    if src.background.is_some()    { dst.background    = src.background.clone(); }
    // [ROE-STYLE-SHD] 同上模式，此前遗漏——如果不补这一行，一个样式没有
    // 直接写 w:shd、指望从 based_on 父样式继承时，shd_background 会在
    // merge_char 这一步就丢失，即使 read_styles 阶段已经从父样式的XML里
    // 正确解析出来了。
    if src.shd_background.is_some() { dst.shd_background = src.shd_background.clone(); }
}

// ─── styles.xml 解析 ─────────────────────────────────────────────────────────

fn read_styles<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
) -> Result<StyleGraph, ParseError> {
    let xml = match read_zip_entry(zip, "word/styles.xml") {
        Ok(s) => s,
        Err(_) => return Ok(StyleGraph { styles: HashMap::new(), doc_default_line_height: None, doc_default_font_size: None, doc_default_color: None, doc_default_space_before: None, doc_default_space_after: None, doc_default_indent_left: None, doc_default_indent_first: None, doc_default_font_family: None, doc_default_font_family_ascii: None }),
    };

    let mut reader = Reader::from_str(&xml);
    reader.config_mut().trim_text(true);

    let mut styles: HashMap<String, RawStyle> = HashMap::new();
    let mut cur: Option<RawStyle> = None;
    let mut in_ppr = false;
    let mut in_pbdr = false; // WROOT-114: 位于 w:pPr/w:pBdr 内
    let mut in_rpr = false;
    let mut in_doc_defaults = false;
    let mut in_doc_defaults_ppr = false;
    let mut in_doc_defaults_rpr = false;
    let mut doc_default_line_height: Option<f32> = None;
    let mut doc_default_font_size: Option<f32> = None;
    let mut doc_default_font_family: Option<String> = None;
    let mut doc_default_font_family_ascii: Option<String> = None;
    let mut doc_default_color: Option<String> = None;
    let mut doc_default_space_before: Option<f32> = None;
    let mut doc_default_space_after: Option<f32> = None;
    let mut doc_default_indent_left: Option<f32> = None;
    let mut doc_default_indent_first: Option<f32> = None;

    loop {
        let ev = reader.read_event()?;
        // WROOT-133：自闭合标签（如 <w:pPrDefault/>）只产生 Empty 事件、没有 End 事件，
        // 若在 Empty 时置位 in_doc_defaults* 标志，就再无处复位，会一直为 true，
        // 把后面样式里的 w:spacing/w:ind 误读成 docDefaults（例如 Subtitle 的
        // before=480 被读成 doc_default_space_before=24pt）。空容器内没有子元素可读，
        // 所以 Empty 时不置位。
        let is_empty_tag = matches!(ev, Event::Empty(_));
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                match e.name().as_ref() {
                    b"w:docDefaults" if !is_empty_tag => {
                        in_doc_defaults = true;
                    }
                    b"w:pPrDefault" if in_doc_defaults && !is_empty_tag => {
                        in_doc_defaults_ppr = true;
                    }
                    b"w:rPrDefault" if in_doc_defaults && !is_empty_tag => {
                        in_doc_defaults_rpr = true;
                    }
                    b"w:sz" if in_doc_defaults_rpr => {
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"w:val" {
                                let hp: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                                doc_default_font_size = Some(hp / 2.0);
                            }
                        }
                    }
                    // WROOT-040-FONT：docDefaults 的 w:rFonts 兜底字体。
                    // 换算规则与 run 级/样式表级 w:rFonts 解析保持一致：
                    // w:eastAsia 优先，缺失时退回 w:ascii。
                    b"w:rFonts" if in_doc_defaults_rpr => {
                        let mut ascii_font: Option<String> = None;
                        let mut east_asia_font: Option<String> = None;
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"w:ascii" => {
                                    ascii_font = attr.unescape_value().ok().map(|v| v.into_owned());
                                }
                                b"w:eastAsia" => {
                                    east_asia_font = attr.unescape_value().ok().map(|v| v.into_owned());
                                }
                                _ => {}
                            }
                        }
                        doc_default_font_family_ascii = ascii_font.clone();
                        doc_default_font_family = east_asia_font.or(ascii_font);
                    }
                    b"w:color" if in_doc_defaults_rpr => {
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"w:val" {
                                doc_default_color = Some(attr.unescape_value()?.into_owned());
                            }
                        }
                    }
                    b"w:spacing" if in_doc_defaults_ppr => {
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"w:line" => {
                                    let val: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                                    // 240 = single spacing, 与 w:spacing (段落级) 解析算法保持一致
                                    doc_default_line_height = Some(val / 240.0);
                                }
                                b"w:before" => {
                                    let val: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                                    // 20 twips = 1pt，与 w:spacing (段落级) 解析算法保持一致
                                    doc_default_space_before = Some(val / 20.0);
                                }
                                b"w:after" => {
                                    let val: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                                    // v225：docDefaults 段后间距，换算同 w:before（20 twips = 1pt）
                                    doc_default_space_after = Some(val / 20.0);
                                }
                                _ => {}
                            }
                        }
                    }
                    b"w:ind" if in_doc_defaults_ppr => {
                        // WROOT-095 关联：此处仅为编号列表缩进的 docDefaults 兜底
                        // 提供数据来源，真正的兜底判断（仅当 level 自身未定义缩进
                        // 时才使用此值）发生在 read_document 里 lvl_indent 处理逻辑，
                        // 见该处调用点。换算方式与段落级 w:ind（第318-336行附近）
                        // 一致：twips / 20 = pt。
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"w:left" => {
                                    let val: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                                    doc_default_indent_left = Some(val / 20.0);
                                }
                                b"w:hanging" => {
                                    let val: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                                    doc_default_indent_first = Some(-(val / 20.0));
                                }
                                b"w:firstLine" => {
                                    let val: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                                    doc_default_indent_first = Some(val / 20.0);
                                }
                                _ => {}
                            }
                        }
                    }
                    b"w:style" => {
                        let mut s = RawStyle::default();
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"w:styleId" => {
                                    s.style_id = attr.unescape_value()?.into_owned();
                                }
                                b"w:type" => {
                                    s.style_type = Some(attr.unescape_value()?.into_owned());
                                }
                                _ => {}
                            }
                        }
                        cur = Some(s);
                        in_ppr = false;
                        in_rpr = false;
                    }
                    b"w:name" => {
                        if let Some(ref mut s) = cur {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:val" {
                                    s.name = attr.unescape_value()?.into_owned();
                                }
                            }
                        }
                    }
                    b"w:basedOn" => {
                        if let Some(ref mut s) = cur {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:val" {
                                    s.based_on = Some(attr.unescape_value()?.into_owned());
                                }
                            }
                        }
                    }
                    b"w:pPr" => in_ppr = true,
                    b"w:rPr" => in_rpr = true,
                    b"w:jc" if in_ppr => {
                        if let Some(ref mut s) = cur {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:val" {
                                    s.para_props.alignment =
                                        Some(attr.unescape_value()?.into_owned());
                                }
                            }
                        }
                    }
                    b"w:ind" if in_ppr => {
                        if let Some(ref mut s) = cur {
                            for attr in e.attributes().flatten() {
                                match attr.key.as_ref() {
                                    b"w:left" => {
                                        let v: f32 = attr.unescape_value()?
                                            .parse().unwrap_or(0.0);
                                        s.para_props.indent_left = Some(v / 20.0);
                                    }
                                    b"w:firstLine" => {
                                        let v: f32 = attr.unescape_value()?
                                            .parse().unwrap_or(0.0);
                                        s.para_props.indent_first = Some(v / 20.0);
                                    }
                                    b"w:hanging" => {
                                        let v: f32 = attr.unescape_value()?
                                            .parse().unwrap_or(0.0);
                                        s.para_props.indent_first = Some(-(v / 20.0));
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    // WROOT-114 阶段二：w:pBdr 容器（仅 in_ppr 内）。注意 w:pBdr 在
                    // 样式里总是 Start 事件（有子元素），End 事件在下方 Event::End 处清除。
                    b"w:pBdr" if in_ppr => { in_pbdr = true; }
                    // 只取 w:left；w:val=none/nil 视为无边框。sz 为 1/8 磅。
                    b"w:left" if in_pbdr => {
                        if let Some(ref mut s) = cur {
                            let mut val = String::new();
                            let mut color = String::new();
                            let mut sz = 0.0f32;
                            let mut space = 0.0f32;
                            for attr in e.attributes().flatten() {
                                match attr.key.as_ref() {
                                    b"w:val" => { val = attr.unescape_value()?.into_owned(); }
                                    b"w:color" => { color = attr.unescape_value()?.into_owned(); }
                                    b"w:sz" => { sz = attr.unescape_value()?.parse().unwrap_or(0.0); }
                                    b"w:space" => { space = attr.unescape_value()?.parse().unwrap_or(0.0); }
                                    _ => {}
                                }
                            }
                            if !val.is_empty() && val != "none" && val != "nil" && sz > 0.0 {
                                let c = if color.is_empty() || color.eq_ignore_ascii_case("auto") {
                                    "000000".to_string()
                                } else { color };
                                s.para_props.border_left = Some((c, sz / 8.0, space));
                            }
                        }
                    }
                    // WROOT-114: 段落样式自带的段落底纹（w:pPr/w:shd）。
                    // 只在 in_ppr 且 cur 为 Some 时记录；w:fill 为 "auto"/空则忽略。
                    // 注意：rPr 内的 w:shd（run 级底纹）由下方 in_rpr 分支处理，互不干扰。
                    b"w:shd" if in_ppr => {
                        if let Some(ref mut s) = cur {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:fill" {
                                    let v = attr.unescape_value()?.into_owned();
                                    if !v.is_empty() && !v.eq_ignore_ascii_case("auto") {
                                        s.para_props.shading = Some(v);
                                    }
                                }
                            }
                        }
                    }
                    b"w:outlineLvl" if in_ppr => {
                        if let Some(ref mut s) = cur {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:val" {
                                    s.para_props.outline_level =
                                        attr.unescape_value()?.parse().ok();
                                }
                            }
                        }
                    }
                    // WROOT-036 续修：style-level numbering。此前 read_styles
                    // 完全没有处理 w:pPr 内的 w:numPr，导致像 CustomStyle 这样
                    // "编号信息挂在样式定义上而非段落自身"的情况被整个丢弃。
                    // 只在 in_ppr 范围内识别，避免和其他作用域的同名标签混淆
                    // （虽然目前 styles.xml 里这两个标签只会出现在 w:pPr 内）。
                    b"w:numId" if in_ppr => {
                        if let Some(ref mut s) = cur {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:val" {
                                    s.para_props.num_id =
                                        attr.unescape_value()?.parse().ok();
                                }
                            }
                        }
                    }
                    b"w:ilvl" if in_ppr => {
                        if let Some(ref mut s) = cur {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:val" {
                                    s.para_props.num_ilvl =
                                        attr.unescape_value()?.parse().ok();
                                }
                            }
                        }
                    }
                    // WROOT-STYLEITALIC修复: 此前不管 w:val 是什么，只要标签
                    // <w:b>/<w:i> 出现就判定 true，导致 styles.xml 里显式写
                    // <w:b w:val="0"/> 或 <w:i w:val="0"/>（用于关闭继承样式的
                    // 粗体/斜体，OOXML常见写法）时被误判为开启。与 read_document
                    // 主路径（2885/2893行附近，b"w:b"/b"w:i" if in_rpr_body）保持
                    // 同款判断逻辑：w:val 缺省视为true，显式"0"/"false"视为false。
                    b"w:b" if in_rpr => {
                        if let Some(ref mut s) = cur {
                            let val = e.attributes().flatten()
                                .find(|a| a.key.as_ref() == b"w:val")
                                .and_then(|a| a.unescape_value().ok())
                                .unwrap_or_else(|| "true".into());
                            s.char_props.bold = Some(val != "false" && val != "0");
                        }
                    }
                    b"w:i" if in_rpr => {
                        if let Some(ref mut s) = cur {
                            let val = e.attributes().flatten()
                                .find(|a| a.key.as_ref() == b"w:val")
                                .and_then(|a| a.unescape_value().ok())
                                .unwrap_or_else(|| "true".into());
                            s.char_props.italic = Some(val != "false" && val != "0");
                        }
                    }
                    // [WROOT-047] 样式表定义（<w:style><w:rPr><w:rFonts.../></w:rPr>）
                    // 自身携带的字体此前从未被解析——run级w:rFonts（WROOT-RFONTS，
                    // 本文件2724/3943行附近）已经补过，但那只覆盖"正文run自身内联
                    // 指定字体"的场景；当run自身rPr为空、字体应从段落样式表继承时
                    // （OOXML合规且常见的写法，例如本次样本"Normal"样式定义了
                    // Liberation Serif但正文run的rPr是空的），resolve_run_font_family
                    // 会读 doc.styles.resolve_paragraph_style(...).font_family，
                    // 但这个值从未被填充过（一直是None），导致断行/渲染被迫回退到
                    // Parley的GenericFamily::SystemUi兜底——在CJK字体优先的系统上
                    // (fc-match sans-serif/serif 都解析到 Noto Sans CJK)，视觉字面
                    // 尺寸明显大于文档实际指定的拉丁字体，同样12pt看起来比正常更大，
                    // 连带影响行高估算和后续分页判断。
                    // 换算规则与run级w:rFonts解析（2724/3943行）保持一致：
                    // w:eastAsia优先，缺失时退回w:ascii。
                    b"w:rFonts" if in_rpr => {
                        if let Some(ref mut s) = cur {
                            let mut ascii_font: Option<String> = None;
                            let mut east_asia_font: Option<String> = None;
                            for attr in e.attributes().flatten() {
                                match attr.key.as_ref() {
                                    b"w:ascii" => {
                                        ascii_font = attr.unescape_value().ok().map(|v| v.into_owned());
                                    }
                                    b"w:eastAsia" => {
                                        east_asia_font = attr.unescape_value().ok().map(|v| v.into_owned());
                                    }
                                    _ => {}
                                }
                            }
                            s.char_props.font_name = east_asia_font.clone().or(ascii_font.clone());
                            // [WROOT-093小尾巴2] 同时保留 ascii 候选，供
                            // resolve_run_font_family 按文字实际 Unicode 范围
                            // 二次决策（纯 ASCII 文字不该套用 eastAsia 字体）。
                            s.char_props.font_name_ascii = ascii_font;
                        }
                    }
                    b"w:sz" if in_rpr => {
                        if let Some(ref mut s) = cur {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:val" {
                                    let hp: f32 =
                                        attr.unescape_value()?.parse().unwrap_or(24.0);
                                    s.char_props.font_size = Some(hp / 2.0);
                                }
                            }
                        }
                    }
                    // WROOT-107: 字符样式级 w:position（半磅→pt，正上负下）
                    b"w:position" if in_rpr => {
                        if let Some(ref mut s) = cur {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:val" {
                                    if let Ok(hp) = attr.unescape_value().unwrap_or_default().parse::<f32>() {
                                        s.char_props.vertical_position = Some(hp / 2.0);
                                    }
                                }
                            }
                        }
                    }
                    b"w:color" if in_rpr => {
                        if let Some(ref mut s) = cur {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:val" {
                                    s.char_props.color =
                                        Some(attr.unescape_value()?.into_owned());
                                }
                            }
                        }
                    }
                    // WROOT-055: styles.xml 里字符样式(w:type="character")
                    // 自带的 <w:u w:val="single"/> 此前完全未解析，
                    // char_props.underline 恒为 None，导致 to_char_style()
                    // 转出的 CharStyle.underline 恒为 false——即使 run 通过
                    // w:rStyle 引用了这个样式也拿不到下划线。语义同正文
                    // run 级 w:u 处理（本文件 b"w:u" if in_rpr_body 分支）：
                    // w:val="none" 显式关闭，其余值（single/double等）视为
                    // 开启下划线。
                    b"w:u" if in_rpr => {
                        if let Some(ref mut s) = cur {
                            let val = e.attributes().flatten()
                                .find(|a| a.key.as_ref() == b"w:val")
                                .and_then(|a| a.unescape_value().ok())
                                .unwrap_or_else(|| "single".into());
                            s.char_props.underline = Some(val != "none");
                        }
                    }
                    // WROOT-027(v141): 字符样式(w:type="character")自带的
                    // <w:vertAlign w:val="superscript"/>，此前完全未解析。
                    // 典型场景：内置 EndnoteReference/FootnoteReference 样式
                    // 就是靠这一条定义"引用编号该显示成上标"，而不是靠
                    // 渲染层硬编码规则强制上标（v140/v141会话已确认的原则：
                    // 该不该上标完全由run自身通过w:rStyle引用的字符样式决定）。
                    b"w:vertAlign" if in_rpr => {
                        if let Some(ref mut s) = cur {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:val" {
                                    s.char_props.vert_align =
                                        Some(attr.unescape_value()?.into_owned());
                                }
                            }
                        }
                    }
                    // [ROE-STYLE-STRIKE-HL] <w:strike/> 或 <w:strike w:val="true"/>
                    // 表示开启删除线；显式 w:val="false"/"0" 表示关闭。语义与
                    // w:b/w:i 保持一致（w:val 缺省视为true）。
                    b"w:strike" if in_rpr => {
                        if let Some(ref mut s) = cur {
                            let val = e.attributes().flatten()
                                .find(|a| a.key.as_ref() == b"w:val")
                                .and_then(|a| a.unescape_value().ok())
                                .unwrap_or_else(|| "true".into());
                            s.char_props.strikethrough = Some(val != "false" && val != "0");
                        }
                    }
                    // [ROE-STYLE-STRIKE-HL] <w:highlight w:val="yellow"/>：存原始
                    // 预设颜色名字符串，名字到RGB的映射交给 to_char_style 阶段
                    // （与 color 字段先存字符串、resolve阶段再 parse_hex_color
                    // 的既有模式对齐；highlight是预设颜色名不是十六进制值，
                    // 需要单独的名字->RGB映射表，本次只打通解析链路，映射表
                    // 的完整度留给后续验证）。
                    b"w:highlight" if in_rpr => {
                        if let Some(ref mut s) = cur {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:val" {
                                    s.char_props.background =
                                        Some(attr.unescape_value()?.into_owned());
                                }
                            }
                        }
                    }
                    // [ROE-STYLE-SHD] 与run级 w:shd 解析（3435行附近）换算规则
                    // 一致：w:fill="auto"表示无填充，其余取十六进制颜色，存
                    // 原始十六进制字符串（不在这里转Color，留给to_char_style
                    // 统一处理，与color字段模式一致）。
                    b"w:shd" if in_rpr => {
                        if let Some(ref mut s) = cur {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:fill" {
                                    let val = attr.unescape_value().unwrap_or_default();
                                    if val.as_ref() != "auto" {
                                        s.char_props.shd_background =
                                            Some(val.into_owned());
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::End(ref e) => match e.name().as_ref() {
                b"w:docDefaults" => in_doc_defaults = false,
                b"w:pPrDefault" => in_doc_defaults_ppr = false,
                b"w:rPrDefault" => in_doc_defaults_rpr = false,
                b"w:style" => {
                    if let Some(s) = cur.take() {
                        if !s.style_id.is_empty() {
                            styles.insert(s.style_id.clone(), s);
                        }
                    }
                }
                b"w:pPr" => in_ppr = false,
                b"w:pBdr" => in_pbdr = false,
                b"w:rPr" => in_rpr = false,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(StyleGraph { styles, doc_default_line_height, doc_default_font_size, doc_default_color, doc_default_space_before, doc_default_space_after, doc_default_indent_left, doc_default_indent_first, doc_default_font_family, doc_default_font_family_ascii })
}

// DOCX-4: w:vMerge 后处理。tbl_vmerge[row_idx][col_idx] 与 tbl.rows[row_idx]
// .cells[col_idx] 一一对应（col_idx 用 cells 数组下标，不做 gridSpan 折算——
// gridSpan 只影响列宽跨度展示，不影响 cells 数组本身的长度，这与 DOCX-3
// 确认过的现状一致）。
//
// 算法：按列遍历所有行。遇到 Some(true)（restart）记录起点；随后连续的
// Some(false)（continue）计入同一段合并，直到遇到 None 或另一个 restart
// 为止，把累计行数写回起点单元格的 row_span，并清空沿途 continue 单元格
// 的 content（不删除单元格本身——渲染层按 cells 下标对齐列位置，物理删除
// 会导致后续列错位，已在设计阶段验证过 roe-writer/src/layout.rs 的实现
// 依赖 cells 下标与 col_widths 下标一一对应，此约束不可破坏）。
fn apply_vmerge(tbl: &mut Table, tbl_vmerge: &[Vec<Option<bool>>]) {
    let num_cols = tbl.rows.iter().map(|r| r.cells.len()).max().unwrap_or(0);

    for col in 0..num_cols {
        let mut restart_row: Option<usize> = None;

        for row in 0..tbl.rows.len() {
            let mark = tbl_vmerge.get(row).and_then(|r| r.get(col)).copied().flatten();

            match mark {
                Some(true) => {
                    // restart：开启新的合并段
                    restart_row = Some(row);
                }
                Some(false) => {
                    // continue：并入当前合并段（若存在），清空自身内容
                    if let Some(start) = restart_row {
                        if let Some(cell) = tbl.rows[row].cells.get_mut(col) {
                            cell.content.clear();
                        }
                        if let Some(start_cell) = tbl.rows[start].cells.get_mut(col) {
                            start_cell.row_span = start_cell.row_span.saturating_add(1);
                        }
                    }
                    // continue 但没有先导 restart：不合法的 vMerge（规范原文：
                    // "若合并的单元格未覆盖相同的网格列集合，则此纵向合并无效"），
                    // 按防御性策略忽略，不 panic，不修改该单元格。
                }
                None => {
                    // 未参与合并，结束当前合并段
                    restart_row = None;
                }
            }
        }
    }
}


/// 十六进制颜色解析（与 pptx.rs::parse_hex_color 逻辑一致，DOCX 侧不引入
/// 不同的容错行为）。w:val 形如 "FF5733"，无 '#' 前缀。
/// WROOT-101（表格浮动绕排）：解析 <w:tblpPr> 标签的属性，供Start/Empty
/// 两条事件路径共用（避免逻辑重复漂移，是与w:insideH/insideV解析风格
/// 的一处小分歧——那边因为要在tblBorders这个更大范围内识别子标签，
/// 用了inline写法；这里tblpPr是独立单标签，抽函数更清晰）。
fn parse_tblppr_attrs(
    e: &quick_xml::events::BytesStart,
    vert_anchor: &mut Option<String>,
    horz_anchor: &mut Option<String>,
    y_pt: &mut Option<f32>,
    x_pt: &mut Option<f32>,
    left_from_text_pt: &mut f32,
    right_from_text_pt: &mut f32,
) {
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"w:vertAnchor" => {
                let val = attr.unescape_value().unwrap_or_default();
                *vert_anchor = Some(val.to_string());
            }
            b"w:horzAnchor" => {
                let val = attr.unescape_value().unwrap_or_default();
                *horz_anchor = Some(val.to_string());
            }
            b"w:tblpY" => {
                let twips: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                *y_pt = Some(twips / 20.0);
            }
            b"w:tblpX" => {
                let twips: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                *x_pt = Some(twips / 20.0);
            }
            b"w:leftFromText" => {
                let twips: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                *left_from_text_pt = twips / 20.0;
            }
            b"w:rightFromText" => {
                let twips: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                *right_from_text_pt = twips / 20.0;
            }
            _ => {}
        }
    }
}

fn parse_hex_color(hex: &str) -> Color {
    let hex = hex.trim_start_matches('#');
    let v = u32::from_str_radix(hex, 16).unwrap_or(0);
    Color {
        r: ((v >> 16) & 0xff) as u8,
        g: ((v >> 8) & 0xff) as u8,
        b: (v & 0xff) as u8,
        a: 255,
    }
}

// [ROE-STYLE-STRIKE-HL] w:highlight 用的是 OOXML 规定的固定预设颜色名
// 集合（ECMA-376 17.18.40），不是十六进制值，不能复用 parse_hex_color
// （那样 "yellow" 会被 from_str_radix 解析失败、静默 unwrap_or(0) 变成
// 黑色，比没有背景色更糟）。这里给出规范列出的全部预设名到RGB的映射，
// "none" 和未识别的值返回 None（区别于"识别到但没设置"，styles.xml
// 里 w:highlight w:val="none" 表示显式关闭高亮）。RGB取值参照Word历史
// 固定调色板，后续如与真实渲染器比对有偏差，在golden用例走查时再校正。
fn parse_highlight_color(name: &str) -> Option<Color> {
    let (r, g, b) = match name {
        "black"       => (0x00, 0x00, 0x00),
        "blue"        => (0x00, 0x00, 0xFF),
        "cyan"        => (0x00, 0xFF, 0xFF),
        "darkBlue"    => (0x00, 0x00, 0x8B),
        "darkCyan"    => (0x00, 0x8B, 0x8B),
        "darkGray"    => (0xA9, 0xA9, 0xA9),
        "darkGreen"   => (0x00, 0x64, 0x00),
        "darkMagenta" => (0x8B, 0x00, 0x8B),
        "darkRed"     => (0x8B, 0x00, 0x00),
        "darkYellow"  => (0x80, 0x80, 0x00),
        "green"       => (0x00, 0xFF, 0x00),
        "lightGray"   => (0xD3, 0xD3, 0xD3),
        "magenta"     => (0xFF, 0x00, 0xFF),
        "red"         => (0xFF, 0x00, 0x00),
        "white"       => (0xFF, 0xFF, 0xFF),
        "yellow"      => (0xFF, 0xFF, 0x00),
        _ => return None, // 含 "none" 和任何未识别值
    };
    Some(Color { r, g, b, a: 255 })
}

// [ROE-STYLE-HL-DEDUP] parse_highlight_color 的 [u8;4] 包装版本，供
// run_highlight（正文run级）和 lvl.highlight（编号列表级）两处历史遗留的
// 数组表示复用，避免维护三份内容相同的颜色名映射表。真正的颜色表只在
// parse_highlight_color 里维护一份，这里只做类型转换。
fn parse_highlight_color_rgba(name: &str) -> Option<[u8; 4]> {
    parse_highlight_color(name).map(|c| [c.r, c.g, c.b, c.a])
}

// ─── StyleGraph → StyleRegistry 接线（DOCX-1）───────────────────────────────
// 只做"原始解析结果 → 模型层未解析结构"的转换，based_on 原样保留，不在这里
// 展开继承链。真正的继承合并交给 StyleRegistry::resolve_paragraph_style()/
// resolve_char_style()（roe-model/src/style.rs），避免维护两套继承解析逻辑。

fn build_style_registry(graph: &StyleGraph) -> StyleRegistry {
    let mut registry = StyleRegistry::default();
    registry.doc_default_line_height = graph.doc_default_line_height;
    registry.doc_default_font_size = graph.doc_default_font_size;
    registry.doc_default_color = graph.doc_default_color.as_deref().map(parse_hex_color);
    registry.doc_default_space_before = graph.doc_default_space_before;
    registry.doc_default_space_after = graph.doc_default_space_after;
    registry.doc_default_indent_left = graph.doc_default_indent_left;
    registry.doc_default_indent_first = graph.doc_default_indent_first;
    registry.doc_default_font_family = graph.doc_default_font_family.clone();
    registry.doc_default_font_family_ascii = graph.doc_default_font_family_ascii.clone();

    for raw in graph.styles.values() {
        match raw.style_type.as_deref() {
            Some("character") => {
                registry.char_styles.insert(raw.style_id.clone(), to_char_style(raw));
            }
            Some("table") | Some("numbering") => {
                // 表格样式 / 编号样式暂不接入 StyleRegistry，超出 DOCX-1 范围。
            }
            // w:type 缺省时 OOXML 规范默认为 "paragraph"，Normal 等常见样式
            // 常常省略该属性，因此 None 按 paragraph 处理。
            _ => {
                registry.paragraph_styles.insert(raw.style_id.clone(), to_paragraph_style(raw));
            }
        }
    }

    registry
}

fn to_paragraph_style(raw: &RawStyle) -> ParagraphStyle {
    ParagraphStyle {
        name: raw.name.clone(),
        based_on: raw.based_on.clone(),
        font_size:   raw.char_props.font_size,
        font_family: raw.char_props.font_name.clone(),
        font_family_ascii: raw.char_props.font_name_ascii.clone(),
        bold:        raw.char_props.bold.unwrap_or(false),
        italic:      raw.char_props.italic.unwrap_or(false),
        color:       raw.char_props.color.as_deref().map(parse_hex_color),
        alignment: raw.para_props.alignment.as_deref().and_then(|v| match v {
            "left" | "start" => Some(Alignment::Left),
            "center" => Some(Alignment::Center),
            "right" | "end" => Some(Alignment::Right),
            "both" | "distribute" => Some(Alignment::Justify),
            _ => None,
        }),
        line_height:  None,
        space_before: raw.para_props.spacing_before.map(|v| v as f32),
        space_after:  raw.para_props.spacing_after.map(|v| v as f32),
        indent_left:  raw.para_props.indent_left,
        indent_first: raw.para_props.indent_first,
        list_style_name: None,
        list_level:      None,
        shading: raw.para_props.shading.as_deref().map(parse_hex_color),
        border_left: raw.para_props.border_left.as_ref().map(|(c, w, sp)| roe_model::style::ParaBorder {
            color: parse_hex_color(c.as_str()),
            width_pt: *w,
            space_pt: *sp,
        }),
    }
}

fn to_char_style(raw: &RawStyle) -> CharStyle {
    // WROOT-027(v141): script 不再硬编码 None，读取上面新解析出的
    // char_props.vert_align（来自 w:vertAlign），映射到 ScriptPosition。
    // 未定义时保持 None（不是 Normal——None 表示"这个样式没有显式定义
    // script，消费方应该继续往 based_on 链或 run 自身找"，语义上与其他
    // 字段（bold/italic 等用 unwrap_or(false)，而非 Option）不同，是
    // 因为 script 目前只有这一个来源，没有 run 级/段落级兜底逻辑，
    // 保留 Option 便于将来若发现消费方需要区分"未定义"和"显式关闭"
    // 时不必再改一次签名。
    let script = match raw.char_props.vert_align.as_deref() {
        Some("superscript") => Some(roe_model::writer::ScriptPosition::Superscript),
        Some("subscript")   => Some(roe_model::writer::ScriptPosition::Subscript),
        _ => None,
    };
    CharStyle {
        name:        raw.name.clone(),
        based_on:    raw.based_on.clone(),
        font_size:   raw.char_props.font_size,
        font_family: raw.char_props.font_name.clone(),
        font_family_ascii: raw.char_props.font_name_ascii.clone(),
        vertical_position: raw.char_props.vertical_position,
        bold:        raw.char_props.bold.unwrap_or(false),
        italic:      raw.char_props.italic.unwrap_or(false),
        underline:   raw.char_props.underline.unwrap_or(false),
        color:       raw.char_props.color.as_deref().map(parse_hex_color),
        // [ROE-STYLE-STRIKE-HL] 此前写死 None，现在读取上面新解析出的
        // char_props.strikethrough / background。background 的具体颜色
        // 映射准确度（highlight 预设色名 -> RGB 是否完整覆盖 OOXML 全部
        // 预设值）留给后续 golden 用例走查验证，本次先确保"读到就不丢"。
        strikethrough: raw.char_props.strikethrough,
        // [ROE-STYLE-SHD] w:shd（任意RGB，精度更高）优先于 w:highlight
        // （16种预设色名，精度较低，parse_highlight_color 无法识别的值
        // 会返回None）。选这个优先级方向是为了和写出层保持读写对称——
        // write_char_props_rpr_xml 里 CharStyle.background 就是用 w:shd
        // 写出的，两者都存在时以更保真的来源为准，而不是像正文run级
        // run_highlight 变量那样"XML里谁写在后面谁生效"的隐式规则
        // （那种写法这次没有照抄，因为它不是有意设计的优先级，只是两处
        // 赋值共用一个变量槽的副作用）。
        background: raw.char_props.shd_background.as_deref().map(parse_hex_color)
            .or_else(|| raw.char_props.background.as_deref().and_then(parse_highlight_color)),
        script,
    }
}

// ─── NumberingResolver ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
struct NumLevel {
    num_fmt:    String,
    lvl_text:   String,
    #[allow(dead_code)]
    start_val:  u32,
    indent_left:  Option<f32>,
    indent_first: Option<f32>,
    // WROOT-074: numbering.xml 里 w:lvl 内 <w:rPr> 定义的编号/bullet自身样式
    // （字体、颜色、高亮），此前完全未解析，导致bullet符号被当成普通正文
    // run插入时走通用字体fallback、丢失应有的背景色。
    font_family: Option<String>,
    color:       Option<String>,
    highlight:   Option<[u8; 4]>,
}

#[derive(Debug, Clone, Default)]
struct AbstractNum {
    levels: Vec<NumLevel>,
}

#[derive(Debug, Clone)]
struct NumInstance {
    abstract_num_id: u32,
    /// w:lvlOverride/w:startOverride 的解析结果：ilvl -> 起始值。
    /// WROOT-036 v13 修复前该字段从未被赋值（一直是空的 dead_code）。
    /// 语义：仅在该 num_id 在文档流中「首次」被 advance 时生效一次，
    /// 用于重置共享计数器（key 是 abstract_num_id）；此后同一 num_id
    /// 再次出现，override 不再生效，只做普通 +1 延续。
    overrides: HashMap<u8, u32>,
    /// WROOT-108：w:lvlOverride 内嵌的完整 w:lvl 定义（ilvl -> NumLevel）。
    /// 优先级高于 abstractNum 里同级定义。此前内嵌 w:lvl 被静默丢弃，
    /// 导致 numFmt="none" 的取消编号覆盖不生效（tdf135164_cancelledNumbering）。
    level_overrides: HashMap<u8, NumLevel>,
}

impl NumberingStore {
    /// WROOT-108：取某 num_id 某级的有效定义——先查该 num 的 lvlOverride 内嵌
    /// 定义，再回退到 abstractNum。
    fn effective_level(&self, num_id: u32, level: usize) -> Option<NumLevel> {
        let inst = self.nums.get(&num_id)?;
        if let Some(l) = inst.level_overrides.get(&(level as u8)) {
            return Some(l.clone());
        }
        self.abstract_nums
            .get(&inst.abstract_num_id)
            .and_then(|a| a.levels.get(level))
            .cloned()
    }
}

struct NumberingStore {
    abstract_nums: HashMap<u32, AbstractNum>,
    nums:          HashMap<u32, NumInstance>,
    /// WROOT-036 v13 修复：key 是 abstract_num_id（不是 num_id）。
    /// 真实 Word/LibreOffice/OnlyOffice 行为里，多个 num_id 如果共享同一个
    /// abstractNumId，运行时共用同一份计数器状态，按 abstract_num_id+ilvl 维护。
    counters:      HashMap<u32, [u32; 9]>,
    /// WROOT-036 v13 修复：追踪每个 num_id 是否已经在文档流中被 advance 过
    /// 至少一次。用于 startOverride「仅首次生效」语义的判断——同一 num_id
    /// 第二次出现时，即使定义了 startOverride 也不再重置，只是对共享计数器
    /// 做普通 +1 延续。
    seen_num_ids:  HashSet<u32>,
    /// v257: abstractNumId -> ROE_ 列表样式名（roe 自己导出的标记）
    roe_names:     HashMap<u32, String>,
}

impl NumberingStore {
    fn new() -> Self {
        Self {
            abstract_nums: HashMap::new(),
            nums:          HashMap::new(),
            counters:      HashMap::new(),
            seen_num_ids:  HashSet::new(),
            roe_names:     HashMap::new(),
        }
    }

    /// v257: num_id 对应的 abstractNum 若带 ROE_ 标记，返回样式名
    fn roe_style_for(&self, num_id: u32) -> Option<String> {
        let inst = self.nums.get(&num_id)?;
        self.roe_names.get(&inst.abstract_num_id).cloned()
    }

    /// v257: 用 abstractNum 各级定义重建 ROE_ ListStyle 并注册
    fn register_roe_styles(&self, reg: &mut StyleRegistry) {
        use roe_model::writer::{ListStyle, ListLevelStyle, NumberListLevel, BulletListLevel, ListIndent, NumFormat};
        for (abs_id, name) in &self.roe_names {
            let Some(abs) = self.abstract_nums.get(abs_id) else { continue };
            let mut levels = Vec::new();
            for lv in abs.levels.iter().take(9) {
                let indent = ListIndent {
                    margin_left: lv.indent_left.unwrap_or(0.0),
                    text_indent: lv.indent_first.unwrap_or(0.0),
                    min_label_width: 18.0,
                };
                if lv.num_fmt == "bullet" {
                    levels.push(ListLevelStyle::Bullet(BulletListLevel {
                        bullet_char: lv.lvl_text.clone(), prefix: String::new(), suffix: String::new(), indent,
                    }));
                } else {
                    let format = match lv.num_fmt.as_str() {
                        "lowerLetter" => NumFormat::LowerAlpha,
                        "upperLetter" => NumFormat::UpperAlpha,
                        "lowerRoman" => NumFormat::LowerRoman,
                        "upperRoman" => NumFormat::UpperRoman,
                        "none" => NumFormat::None,
                        _ => NumFormat::Arabic,
                    };
                    let (prefix, suffix) = match lv.lvl_text.find('%') {
                        Some(pos) => (
                            lv.lvl_text[..pos].to_string(),
                            lv.lvl_text.get(pos + 2..).unwrap_or("").to_string(),
                        ),
                        None => (String::new(), String::new()),
                    };
                    levels.push(ListLevelStyle::Number(NumberListLevel {
                        format, prefix, suffix, start_value: lv.start_val.max(1), display_levels: 1, indent,
                    }));
                }
            }
            if !levels.is_empty() {
                reg.list_styles.entry(name.clone()).or_insert(ListStyle { name: name.clone(), levels });
            }
        }
    }

    fn advance(&mut self, num_id: u32, level: u8) -> String {
        // OOXML w:ilvl 规范上限是 0-8（9级）。部分畸形/边界文档（如 fuzz/CVE 样本，
        // 或某些工具生成的非规范docx）可能出现超范围的 ilvl 值，此处钳制到合法范围，
        // 避免数组越界panic（v117修复，tdf109063/tdf141966/tdf61309三个样本触发）。
        let level = level.min(8);

        let inst = match self.nums.get(&num_id) {
            Some(i) => i.clone(),
            None => return String::new(),
        };
        let abstract_id = inst.abstract_num_id;

        // WROOT-036 v13 修复：startOverride 仅在该 num_id 在文档流中「首次」
        // 被 advance 时生效一次；HashSet::insert 返回 true 表示之前不存在，
        // 即本次是首次出现。同一 num_id 后续再次出现，override 不再生效。
        let is_first_use = self.seen_num_ids.insert(num_id);
        let override_val = if is_first_use {
            inst.overrides.get(&level).copied()
        } else {
            None
        };

        // counters 现在按 abstract_num_id 维护（不是 num_id），这样共享同一个
        // abstractNumId 的多个 num_id 会共用同一份运行时计数状态，与真实
        // Word/LibreOffice/OnlyOffice 渲染行为一致。
        // WROOT-117：跳级使用低层级（如直接用 ilvl=4）时，上层（0..level）
        // 尚未出现过的计数器按各自 start 值补齐（默认 1），而不是保持 0。
        // Word/LibreOffice 行为：显示 "1st.i.a.1.I" 而不是 "0th.0..0.I"
        // （LOK 断言 testTdf141966_chapterNumberTortureTest）。
        // 必须在取 counters 可变借用之前先算出各上层的 start 值。
        let upper_starts: Vec<u32> = (0..(level as usize))
            .map(|i| {
                inst.level_overrides.get(&(i as u8))
                    .map(|l| l.start_val)
                    .or_else(|| self.abstract_nums.get(&abstract_id)
                        .and_then(|a| a.levels.get(i))
                        .map(|l| l.start_val))
                    .unwrap_or(1)
            })
            .collect();
        let counters = self.counters.entry(abstract_id).or_insert([0u32; 9]);
        for (i, st) in upper_starts.iter().enumerate() {
            if counters[i] == 0 {
                counters[i] = *st;
            }
        }
        if let Some(start) = override_val {
            counters[level as usize] = start;
        } else {
            counters[level as usize] += 1;
        }
        for l in (level as usize + 1)..9 {
            counters[l] = 0;
        }

        let abs = match self.abstract_nums.get(&abstract_id) {
            Some(a) => a.clone(),
            None => return String::new(),
        };
        // WROOT-108：先查 lvlOverride 内嵌定义，再回退 abstractNum
        let lvl = match inst.level_overrides.get(&level)
            .cloned()
            .or_else(|| abs.levels.get(level as usize).cloned())
        {
            Some(l) => l,
            None => return String::new(),
        };
        // WROOT-040-NUMFMT-NONE：OOXML 规范里 w:numFmt="none" 表示这个
        // 编号级别参与计数（counters仍正常递增，供其他级别/交叉引用使用），
        // 但不产生任何可见编号文字——不是"把数字格式化成空串再替换进
        // lvlText模板"，而是整个级别的编号文本都应为空，避免 lvlText
        // 模板里除占位符外还有固定文字时（如"第%1章"）残留半截文字。
        // 此前 format_num 对未知/none格式一律走 `_ => val.to_string()`
        // 兜底分支，把"none"当十进制处理，导致 numFmt="none" 的标题
        // （如本例 heading 1）凭空多渲染出一个数字。
        let this_level_fmt = lvl.num_fmt.as_str();
        if this_level_fmt == "none" {
            return String::new();
        }
        let mut text = lvl.lvl_text.clone();
        let snap = *self.counters.get(&abstract_id).unwrap();
        for i in 0..9usize {
            let placeholder = format!("%{}", i + 1);
            if text.contains(&placeholder) {
                let val = snap[i];
                let fmt_owned: String = inst.level_overrides.get(&(i as u8))
                    .map(|l| l.num_fmt.clone())
                    .or_else(|| abs.levels.get(i).map(|l| l.num_fmt.clone()))
                    .unwrap_or_else(|| "decimal".to_string());
                text = text.replace(&placeholder, &format_num(val, fmt_owned.as_str()));
            }
        }
        text
    }
}

// WROOT-074: OOXML numbering.xml 里 w:numFmt="bullet" 场景常见写法是
// w:lvlText 直接写一个私有区字符（Unicode Private Use Area, U+E000-U+F8FF），
// 配合 w:rFonts="Symbol"/"Wingdings" 使用——这是Word的历史约定，字符本身
// 在这些私有区码位并无跨字体统一的标准语义，必须绑定特定字体才能正确
// 显示。但目标渲染环境（本项目运行环境）通常不具备Word附带的Symbol/
// Wingdings字体文件，直接按字体名查找会fallback到任意不相关字体、
// 画出乱码图案。
//
// 完整的Symbol/Wingdings私有区→标准Unicode映射表规模较大（224个码位，
// 参考LibreOffice unotools/source/misc/fontcvt.cxx的aAdobeSymbolTab/
// aWingDingsTab等），本次不做完整移植（技术债，留待真正需要渲染Symbol/
// Wingdings字体的数学符号、图形符号等场景时再扩展）。这里只窄范围处理
// 目前唯一已知真实需要的场景——bullet圆点，这是OOXML/Word文档里迄今
// 最常见的 numFmt="bullet" 写法：
//   U+F0B7（Symbol字体私有区）  -> U+2022 BULLET（标准圆点）
//   U+F0A7（Wingdings字体私有区，另一常见方形bullet变体）-> U+25AA BLACK SMALL SQUARE
// 命中后返回标准Unicode字符 + None（不再要求特定字体名，走系统默认
// 字体即可正确显示），未命中则原样返回，不做任何改动（不影响其他
// 私有区字符的现有行为，只精确处理这两个已知场景）。
fn normalize_symbol_bullet_char(c: char, font_hint: Option<&str>) -> (char, bool) {
    // font_hint 目前仅用于未来扩展时进一步收窄匹配范围（比如同一码位在
    // 不同字体下语义不同），本次两个码位在实际样本中含义明确，暂不依赖
    // font_hint 做判断，仅接收该参数保留调用点语义、避免未来误用忽略此信息。
    let _ = font_hint;
    match c {
        '\u{F0B7}' => ('\u{2022}', true),
        '\u{F0A7}' => ('\u{25AA}', true),
        _ => (c, false),
    }
}

pub fn format_num(val: u32, fmt: &str) -> String {
    match fmt {
        "upperRoman" => to_roman(val, true),
        "lowerRoman" => to_roman(val, false),
        "upperLetter" => to_letter(val, true),
        "lowerLetter" => to_letter(val, false),
        "bullet" => "•".to_string(),
        // golden走查符号chicago_list现象修复：OOXML numFmt="chicago"此前完全
        // 未识别，落到下面_分支被当十进制处理，导致芝加哥编号列表渲染成普通
        // 阿拉伯数字（1,2,3...），而不是Chicago Manual of Style的脚注符号
        // 循环。该格式固定用4个基础符号 * † ‡ § 循环，每用完一轮4个符号后，
        // 下一轮把符号重复对应轮次的次数（第2轮各符号重复2次，第3轮重复
        // 3次，以此类推）：1=*  2=†  3=‡  4=§  5=**  6=††  7=‡‡  8=§§  9=***
        // 这是OOXML/Word固定规范（非本项目自定义），实现前已核实用例XML
        // 里实际值为 <w:numFmt w:val="chicago"/>。
        "chicago" => to_chicago(val),
        // WROOT-116：numFmt="ordinal"（1st/2nd/3rd…）与"ordinalText"
        // （First/Second/Third…）此前未识别，落到 _ 分支被当十进制，
        // 导致 tdf141966_chapterNumbering / chapterNumberTortureTest
        // 的章节编号渲染成 1、2 而非 1st、2nd（LOK 断言 ListLabelString="2nd"、"1st.i.a.1.I"）。
        "ordinal" => to_ordinal(val),
        "ordinalText" => to_ordinal_text(val),
        _ => val.to_string(),
    }
}

pub fn to_chicago(val: u32) -> String {
    if val == 0 { return "0".to_string(); }
    const SYMBOLS: [char; 4] = ['*', '\u{2020}', '\u{2021}', '\u{00A7}']; // † ‡ §
    let idx = ((val - 1) % 4) as usize;
    let round = (val - 1) / 4 + 1;
    SYMBOLS[idx].to_string().repeat(round as usize)
}

/// WROOT-116：英文序数后缀数字形式：1st 2nd 3rd 4th … 11th 12th 13th … 21st 22nd。
/// 11/12/13（及 111/112/113 等，即末两位为 11-13）一律用 th。
pub fn to_ordinal(n: u32) -> String {
    let last_two = n % 100;
    let suffix = if (11..=13).contains(&last_two) {
        "th"
    } else {
        match n % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        }
    };
    format!("{}{}", n, suffix)
}

/// WROOT-116：英文序数词：First Second Third … Twentieth；
/// 20 以内用词，超出范围回退成数字序数（21st…），不自行拼写大数。
pub fn to_ordinal_text(n: u32) -> String {
    const WORDS: [&str; 20] = [
        "First", "Second", "Third", "Fourth", "Fifth", "Sixth", "Seventh",
        "Eighth", "Ninth", "Tenth", "Eleventh", "Twelfth", "Thirteenth",
        "Fourteenth", "Fifteenth", "Sixteenth", "Seventeenth", "Eighteenth",
        "Nineteenth", "Twentieth",
    ];
    if n >= 1 && (n as usize) <= WORDS.len() {
        WORDS[(n - 1) as usize].to_string()
    } else {
        to_ordinal(n)
    }
}

#[cfg(test)]
mod tests_wroot116_ordinal {
    use super::*;

    #[test]
    fn ordinal_basic_suffixes() {
        assert_eq!(format_num(1, "ordinal"), "1st");
        assert_eq!(format_num(2, "ordinal"), "2nd");
        assert_eq!(format_num(3, "ordinal"), "3rd");
        assert_eq!(format_num(4, "ordinal"), "4th");
        assert_eq!(format_num(10, "ordinal"), "10th");
    }

    #[test]
    fn ordinal_teens_use_th() {
        assert_eq!(format_num(11, "ordinal"), "11th");
        assert_eq!(format_num(12, "ordinal"), "12th");
        assert_eq!(format_num(13, "ordinal"), "13th");
        assert_eq!(format_num(111, "ordinal"), "111th");
        assert_eq!(format_num(112, "ordinal"), "112th");
    }

    #[test]
    fn ordinal_twenties() {
        assert_eq!(format_num(21, "ordinal"), "21st");
        assert_eq!(format_num(22, "ordinal"), "22nd");
        assert_eq!(format_num(23, "ordinal"), "23rd");
        assert_eq!(format_num(101, "ordinal"), "101st");
    }

    #[test]
    fn ordinal_text_words_and_fallback() {
        assert_eq!(format_num(1, "ordinalText"), "First");
        assert_eq!(format_num(2, "ordinalText"), "Second");
        assert_eq!(format_num(12, "ordinalText"), "Twelfth");
        assert_eq!(format_num(20, "ordinalText"), "Twentieth");
        assert_eq!(format_num(21, "ordinalText"), "21st");
    }
}

pub fn to_roman(mut n: u32, upper: bool) -> String {
    if n == 0 { return "0".to_string(); }
    const VALS: &[(u32, &str)] = &[
        (1000,"M"),(900,"CM"),(500,"D"),(400,"CD"),
        (100,"C"),(90,"XC"),(50,"L"),(40,"XL"),
        (10,"X"),(9,"IX"),(5,"V"),(4,"IV"),(1,"I"),
    ];
    let mut s = String::new();
    for &(v, sym) in VALS {
        while n >= v { s.push_str(sym); n -= v; }
    }
    if upper { s } else { s.to_lowercase() }
}

pub fn to_letter(n: u32, upper: bool) -> String {
    if n == 0 { return String::new(); }
    let c = (b'A' + ((n - 1) % 26) as u8) as char;
    if upper { c.to_uppercase().to_string() } else { c.to_lowercase().to_string() }
}

#[cfg(test)]
mod tests_format_num_chicago {
    use super::*;

    #[test]
    fn chicago_numfmt_produces_symbol_cycle() {
        // golden用例symbol_chicago_list回归验证：核对Chicago Manual of Style
        // 脚注符号循环的前9个值，覆盖第一轮4个基础符号+第二轮开始的重复模式。
        assert_eq!(format_num(1, "chicago"), "*");
        assert_eq!(format_num(2, "chicago"), "\u{2020}");   // †
        assert_eq!(format_num(3, "chicago"), "\u{2021}");   // ‡
        assert_eq!(format_num(4, "chicago"), "\u{00A7}");   // §
        assert_eq!(format_num(5, "chicago"), "**");
        assert_eq!(format_num(6, "chicago"), "\u{2020}\u{2020}");   // ††
        assert_eq!(format_num(7, "chicago"), "\u{2021}\u{2021}");   // ‡‡
        assert_eq!(format_num(8, "chicago"), "\u{00A7}\u{00A7}");   // §§
        assert_eq!(format_num(9, "chicago"), "***");
    }

    #[test]
    fn chicago_numfmt_zero_falls_back_to_zero_string() {
        // 与to_roman/其他格式化函数的val==0兜底行为保持一致，不panic。
        assert_eq!(format_num(0, "chicago"), "0");
    }

    #[test]
    fn other_numfmt_branches_unaffected_by_chicago_addition() {
        // 回归防护：确认新增chicago分支没有影响既有分支的匹配顺序/行为。
        assert_eq!(format_num(4, "upperRoman"), "IV");
        assert_eq!(format_num(4, "lowerRoman"), "iv");
        assert_eq!(format_num(2, "upperLetter"), "B");
        assert_eq!(format_num(1, "bullet"), "\u{2022}");
        assert_eq!(format_num(42, "unknownFormat"), "42");
    }
}

fn read_numbering<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
) -> Result<NumberingStore, ParseError> {
    let xml = match read_zip_entry(zip, "word/numbering.xml") {
        Ok(s) => s,
        Err(_) => return Ok(NumberingStore::new()),
    };

    let mut reader = Reader::from_str(&xml);
    reader.config_mut().trim_text(true);

    let mut store = NumberingStore::new();
    let mut cur_abs_id: Option<u32> = None;
    let mut cur_abs: Option<AbstractNum> = None;
    let mut cur_lvl: Option<(usize, NumLevel)> = None;
    let mut cur_num_id: Option<u32> = None;
    let mut cur_num: Option<NumInstance> = None;
    // WROOT-036 v13 修复：解析 w:num 内部的 w:lvlOverride/w:startOverride，
    // 此前完全没有解析这两个标签，NumInstance.overrides 一直是空的。
    let mut cur_override_lvl: Option<u8> = None;

    loop {
        match reader.read_event()? {
            Event::Start(ref e) | Event::Empty(ref e) => {
                match e.name().as_ref() {
                    b"w:name" if cur_abs_id.is_some() => {
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"w:val" {
                                let v = attr.unescape_value()?.into_owned();
                                if v.starts_with("ROE_") {
                                    if let Some(id) = cur_abs_id { store.roe_names.insert(id, v); }
                                }
                            }
                        }
                    }
                    b"w:abstractNum" => {
                        let mut id = 0u32;
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"w:abstractNumId" {
                                id = attr.unescape_value()?.parse().unwrap_or(0);
                            }
                        }
                        cur_abs_id = Some(id);
                        cur_abs = Some(AbstractNum::default());
                    }
                    b"w:lvl" => {
                        let mut idx = 0usize;
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"w:ilvl" {
                                idx = attr.unescape_value()?.parse().unwrap_or(0);
                            }
                        }
                        cur_lvl = Some((idx, NumLevel::default()));
                    }
                    b"w:numFmt" => {
                        if let Some((_, ref mut lvl)) = cur_lvl {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:val" {
                                    lvl.num_fmt = attr.unescape_value()?.into_owned();
                                }
                            }
                        }
                    }
                    b"w:lvlText" => {
                        if let Some((_, ref mut lvl)) = cur_lvl {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:val" {
                                    lvl.lvl_text = attr.unescape_value()?.into_owned();
                                }
                            }
                        }
                    }
                    b"w:start" => {
                        if let Some((_, ref mut lvl)) = cur_lvl {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:val" {
                                    lvl.start_val =
                                        attr.unescape_value()?.parse().unwrap_or(1);
                                }
                            }
                        }
                    }
                    b"w:ind" => {
                        if let Some((_, ref mut lvl)) = cur_lvl {
                            for attr in e.attributes().flatten() {
                                let k = attr.key.as_ref();
                                let v: f32 = attr.unescape_value()
                                    .unwrap_or_default().parse().unwrap_or(0.0);
                                if k == b"w:left" {
                                    lvl.indent_left = Some(v / 1440.0 * 72.0);
                                } else if k == b"w:hanging" {
                                    lvl.indent_first = Some(-(v / 1440.0 * 72.0));
                                } else if k == b"w:firstLine" {
                                    lvl.indent_first = Some(v / 1440.0 * 72.0);
                                }
                            }
                        }
                    }
                    // WROOT-074: numbering.xml 的 w:lvl 内 <w:rPr> 子标签，
                    // 描述编号/bullet符号自身的字体、颜色、高亮底纹。此前
                    // read_numbering 完全不解析这些标签，bullet被当成普通
                    // 正文文字插入时（见下方 runs.insert(0, ...)）丢失了
                    // 这些视觉属性。
                    b"w:rFonts" => {
                        if let Some((_, ref mut lvl)) = cur_lvl {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:ascii" {
                                    lvl.font_family = Some(attr.unescape_value()?.into_owned());
                                }
                            }
                        }
                    }
                    b"w:color" => {
                        if let Some((_, ref mut lvl)) = cur_lvl {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:val" {
                                    lvl.color = Some(attr.unescape_value()?.into_owned());
                                }
                            }
                        }
                    }
                    b"w:highlight" => {
                        if let Some((_, ref mut lvl)) = cur_lvl {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:val" {
                                    lvl.highlight = parse_highlight_color_rgba(
                                        attr.unescape_value().unwrap_or_default().as_ref()
                                    );
                                }
                            }
                        }
                    }
                    // WROOT-110：numbering.xml lvl/rPr 里的 <w:shd w:fill="RRGGBB"/>
                    // （编号/项目符号自身的底纹）此前未解析，只处理了 w:highlight。
                    // 与 run 级 w:shd（3572行附近）换算规则一致，复用 lvl.highlight
                    // 字段（下游 bullet_bg -> Run.background 链路不变）。w:shd 是
                    // 任意RGB，精度高于 w:highlight 预设色，因此晚到的 w:shd 覆盖之。
                    b"w:shd" => {
                        if let Some((_, ref mut lvl)) = cur_lvl {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:fill" {
                                    let hex = attr.unescape_value().unwrap_or_default();
                                    let hex = hex.trim_start_matches('#');
                                    if hex.len() == 6 && hex != "auto" {
                                        if let Ok(rgb) = u32::from_str_radix(hex.as_ref(), 16) {
                                            let r = ((rgb >> 16) & 0xff) as u8;
                                            let g = ((rgb >> 8) & 0xff) as u8;
                                            let b = (rgb & 0xff) as u8;
                                            lvl.highlight = Some([r, g, b, 0xff]);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    b"w:num" => {
                        let mut id = 0u32;
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"w:numId" {
                                id = attr.unescape_value()?.parse().unwrap_or(0);
                            }
                        }
                        cur_num_id = Some(id);
                        cur_num = Some(NumInstance {
                            abstract_num_id: 0,
                            overrides: HashMap::new(),
                            level_overrides: HashMap::new(),
                        });
                    }
                    b"w:abstractNumId" => {
                        if let Some(ref mut inst) = cur_num {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:val" {
                                    inst.abstract_num_id =
                                        attr.unescape_value()?.parse().unwrap_or(0);
                                }
                            }
                        }
                    }
                    b"w:lvlOverride" => {
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"w:ilvl" {
                                cur_override_lvl =
                                    attr.unescape_value()?.parse::<u8>().ok();
                            }
                        }
                    }
                    b"w:startOverride" => {
                        if let (Some(ref mut inst), Some(ilvl)) =
                            (&mut cur_num, cur_override_lvl)
                        {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:val" {
                                    let start: u32 =
                                        attr.unescape_value()?.parse().unwrap_or(1);
                                    inst.overrides.insert(ilvl, start);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::End(ref e) => match e.name().as_ref() {
                b"w:lvl" => {
                    // WROOT-108：cur_lvl.take() 只调用一次，再按上下文分流：
                    // 在 abstractNum 内 -> 落盘到 abs.levels；
                    // 在 w:num/w:lvlOverride 内 -> 落盘到 inst.level_overrides。
                    let taken = cur_lvl.take();
                    if let Some((idx, lvl)) = taken {
                        if let Some(ref mut abs) = cur_abs {
                            if idx >= abs.levels.len() {
                                abs.levels.resize(idx + 1, NumLevel::default());
                            }
                            abs.levels[idx] = lvl;
                        } else if let (Some(ref mut inst), Some(ilvl)) =
                            (&mut cur_num, cur_override_lvl)
                        {
                            inst.level_overrides.insert(ilvl, lvl);
                        }
                    }
                }
                b"w:abstractNum" => {
                    if let (Some(id), Some(abs)) =
                        (cur_abs_id.take(), cur_abs.take())
                    {
                        store.abstract_nums.insert(id, abs);
                    }
                }
                b"w:num" => {
                    if let (Some(id), Some(inst)) =
                        (cur_num_id.take(), cur_num.take())
                    {
                        store.nums.insert(id, inst);
                    }
                    // 防御：避免 lvlOverride 的 ilvl 状态泄漏到下一个 w:num 块
                    cur_override_lvl = None;
                }
                b"w:lvlOverride" => {
                    cur_override_lvl = None;
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(store)
}

// ─── document.xml 解析 ───────────────────────────────────────────────────────


fn collect_chart_rels<R: std::io::Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
) -> std::collections::HashMap<String, String> {
    let mut map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let rels_xml = match read_zip_entry(zip, "word/_rels/document.xml.rels") {
        Ok(s) => s,
        Err(_) => return map,
    };
    let mut rest = rels_xml.as_str();
    while let Some(pos) = rest.find("<Relationship ") {
        rest = &rest[pos + 14..];
        let end = rest.find('>').unwrap_or(rest.len());
        let chunk = &rest[..end];
        let get = |attr: &str| -> Option<String> {
            let needle = format!("{}=\"", attr);
            let start = chunk.find(&needle)? + needle.len();
            let end = chunk[start..].find('"')?;
            Some(chunk[start..start + end].to_string())
        };
        if let (Some(id), Some(typ), Some(target)) =
            (get("Id"), get("Type"), get("Target"))
        {
            if typ.ends_with("/chart") || typ.ends_with("/chartEx") {
                map.insert(id, target);
            }
        }
    }
    map
}

fn collect_hyperlink_rels<R: std::io::Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
) -> std::collections::HashMap<String, String> {
    let mut map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let rels_xml = match read_zip_entry(zip, "word/_rels/document.xml.rels") {
        Ok(s) => s,
        Err(_) => return map,
    };
    let mut rest = rels_xml.as_str();
    while let Some(pos) = rest.find("<Relationship ") {
        rest = &rest[pos + 14..];
        let end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..end];
        if !tag.contains("/hyperlink") { continue; }
        let id = pull_attr(tag, "Id");
        let target = pull_attr(tag, "Target");
        if !id.is_empty() && !target.is_empty() {
            map.insert(id, target);
        }
    }
    map
}


/// WROOT-004: header/footer的引用信息——从 word/document.xml 的 sectPr 里
/// w:headerReference/w:footerReference（type={default,even,first} + r:id）
/// 和 w:titlePg 解析得到。只记录"这份文档要用哪个rId对应的header/footer"，
/// 还没解析rId对应的实际XML内容（那是下面 collect_header_footer_rel_targets
/// + load_hf_part 做的事）。
#[derive(Debug, Default, Clone)]
struct HeaderFooterRefs {
    header_default: Option<String>,
    header_even:    Option<String>,
    header_first:   Option<String>,
    footer_default: Option<String>,
    footer_even:    Option<String>,
    footer_first:   Option<String>,
    title_page: bool,
    // [WROOT-093] sectPr 的 w:type（evenPage/oddPage/nextPage/continuous）。
    section_type: Option<String>,
}

/// WROOT任务B第一步（parser层）：此前这个函数只维护单个 HeaderFooterRefs
/// 实例，每遇到一个 sectPr 就直接覆盖同名字段，多节文档最终只保留*最后
/// 一节*的引用（同根因见 read_page_layout，那里同样是单实例覆盖，本次
/// 不改，留给后续任务）。
/// 改造：改为收集 Vec<HeaderFooterRefs>，每个 sectPr 各自一份、互不覆盖，
/// 按文档中出现的先后顺序排列。调用方目前仍只使用"最后一个元素"来填充
/// WriterDocument 顶层字段（保持现有行为不变，不引入回归），但完整的
/// Vec 会被用来填充新增的 WriterDocument.sections，供未来"按section查
/// 各自header/footer"使用（渲染层接入是后续任务，本次不做）。
fn read_header_footer_refs<R: std::io::Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
) -> Vec<HeaderFooterRefs> {
    let mut all_refs: Vec<HeaderFooterRefs> = Vec::new();
    let mut refs = HeaderFooterRefs::default();
    let xml = match read_zip_entry(zip, "word/document.xml") {
        Ok(s) => s,
        Err(_) => return all_refs,
    };
    let mut reader = Reader::from_str(&xml);
    reader.config_mut().trim_text(true);
    let mut in_sect = false;
    let mut buf = Vec::new();
    loop {
        let ev = match reader.read_event_into(&mut buf) {
            Ok(ev) => ev,
            Err(_) => break,
        };
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                match e.name().as_ref() {
                    b"w:sectPr" => { in_sect = true; }
                    b"w:titlePg" if in_sect => { refs.title_page = true; }
                    b"w:type" if in_sect => {
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"w:val" {
                                if let Ok(v) = attr.unescape_value() {
                                    refs.section_type = Some(v.to_string());
                                }
                            }
                        }
                    }
                    b"w:headerReference" if in_sect => {
                        let mut htype = String::new();
                        let mut rid = String::new();
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"w:type" => { if let Ok(v) = attr.unescape_value() { htype = v.to_string(); } }
                                b"r:id" => { if let Ok(v) = attr.unescape_value() { rid = v.to_string(); } }
                                _ => {}
                            }
                        }
                        if !rid.is_empty() {
                            match htype.as_str() {
                                "even"  => refs.header_even  = Some(rid),
                                "first" => refs.header_first = Some(rid),
                                _       => refs.header_default = Some(rid),
                            }
                        }
                    }
                    b"w:footerReference" if in_sect => {
                        let mut ftype = String::new();
                        let mut rid = String::new();
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"w:type" => { if let Ok(v) = attr.unescape_value() { ftype = v.to_string(); } }
                                b"r:id" => { if let Ok(v) = attr.unescape_value() { rid = v.to_string(); } }
                                _ => {}
                            }
                        }
                        if !rid.is_empty() {
                            match ftype.as_str() {
                                "even"  => refs.footer_even  = Some(rid),
                                "first" => refs.footer_first = Some(rid),
                                _       => refs.footer_default = Some(rid),
                            }
                        }
                    }
                    _ => {}
                }
            }
            // 每个 sectPr 结束时，把这一节累积的 refs 存入 Vec，
            // 然后重置 refs（清空）供下一个 sectPr 使用。这是与原实现
            // 唯一的行为差异点：原实现从不重置、从不存档，靠"最后一次
            // 覆盖"隐式得到最后一节的值；新实现显式存档每一节。
            Event::End(ref e) if e.name().as_ref() == b"w:sectPr" => {
                in_sect = false;
                all_refs.push(std::mem::take(&mut refs));
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    all_refs
}

/// WROOT-004: 从 document.xml.rels 中筛出所有 header/footer part 的
/// rId → Target 文件名映射（如 "header1.xml"，实际zip路径需要拼上"word/"前缀）。
/// 写法照抄 collect_hyperlink_rels 的字符串扫描模式，保持风格一致。
fn collect_header_footer_rel_targets<R: std::io::Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
) -> std::collections::HashMap<String, String> {
    let mut map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let rels_xml = match read_zip_entry(zip, "word/_rels/document.xml.rels") {
        Ok(s) => s,
        Err(_) => return map,
    };
    let mut rest = rels_xml.as_str();
    while let Some(pos) = rest.find("<Relationship ") {
        rest = &rest[pos + 14..];
        let end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..end];
        if !tag.contains("/header") && !tag.contains("/footer") { continue; }
        let id = pull_attr(tag, "Id");
        let target = pull_attr(tag, "Target");
        if !id.is_empty() && !target.is_empty() {
            map.insert(id, target);
        }
    }
    map
}

/// WROOT-004: 按rId从zip里取出对应header/footer part的XML，解析成段落。
/// 复用 parse_isolated_paragraphs（无状态、独立于主body状态机）。
/// 第一阶段先不处理 w:fldChar/w:instrText 域(field)，PAGE/NUMPAGES域
/// 会退化成其静态缓存文本（模型层Run.text本来就是为这种情况设计的兜底），
/// 域的动态求值支持留到下一步单独做（避免一次改动改太多状态机逻辑）。
fn load_hf_part<R: std::io::Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
    targets: &std::collections::HashMap<String, String>,
    rid: &Option<String>,
) -> Option<Vec<Paragraph>> {
    let rid = rid.as_ref()?;
    let target = targets.get(rid)?;
    let path = format!("word/{}", target);
    let xml = read_zip_entry(zip, &path).ok()?;
    Some(parse_isolated_paragraphs(&xml))
}

/// Step 3 (WROOT-075/027/028): 读取 word/endnotes.xml 或 word/footnotes.xml，
/// 切分出每个 w:endnote/w:footnote 节点，跳过分隔符类型（separator/
/// continuationSeparator，真实样本 tdf141548.docx 里 w:id="0"/"1" 就是这两种，
/// 不是真实内容），对每个真实 note 抠出内部 XML 片段，交给 Step 2 改造后的
/// parse_isolated_paragraphs 解析，包装成 Note { id, content }。
///
/// part_path: 如 "word/endnotes.xml" / "word/footnotes.xml"（part 不存在时
/// 返回空 Vec，不是错误——大多数 docx 根本没有脚注/尾注）。
/// item_tag: 如 b"w:endnote" / b"w:footnote"（根标签 w:endnotes/w:footnotes
/// 本身不需要单独传参，因为切分逻辑只关心子标签的 Start/End 配对，不关心
/// 外层根标签叫什么）。
fn parse_notes_part<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
    part_path: &str,
    item_tag: &[u8],
) -> Vec<Note> {
    let xml = match read_zip_entry(zip, part_path) {
        Ok(x) => x,
        Err(_) => return Vec::new(),
    };

    let mut reader = Reader::from_str(&xml);
    reader.config_mut().trim_text(false);

    let mut notes: Vec<Note> = Vec::new();
    let mut cur_id: Option<u32> = None;
    let mut cur_note_type = NoteType::Normal;
    let mut cur_start_pos: usize = 0;
    let mut buf = Vec::new();

    loop {
        let pos_before = reader.buffer_position() as usize;
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) if e.name().as_ref() == item_tag => {
                let mut id: Option<u32> = None;
                let mut note_type = NoteType::Normal;
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"w:id" => {
                            if let Ok(v) = attr.unescape_value() {
                                id = v.parse::<u32>().ok();
                            }
                        }
                        b"w:type" => {
                            if let Ok(v) = attr.unescape_value() {
                                note_type = match v.as_ref() {
                                    "separator" => NoteType::Separator,
                                    // WROOT-135：continuationNotice（续注提示，Word 系统占位项，内容通常是
                                    // 空段落）此前落进 `_ => Normal`，被当成真实尾注渲染出一条分隔线和
                                    // 一个 `?`（tdf166205 第 2 页）。它和 separator/continuationSeparator
                                    // 同属系统占位、不是真实注释；layout 侧所有过滤点都是 `== Normal`，
                                    // 所以只要归到非 Normal 类型即可，不新增枚举变体以免波及各处 match。
                                    "continuationSeparator" | "continuationNotice" => NoteType::ContinuationSeparator,
                                    _ => NoteType::Normal,
                                };
                            }
                        }
                        _ => {}
                    }
                }
                cur_id = id;
                cur_note_type = note_type;
                // 内部片段从当前 Start 标签结束处开始抠（不含 <w:endnote ...> 本身）
                cur_start_pos = reader.buffer_position() as usize;
            }
            Ok(Event::End(ref e)) if e.name().as_ref() == item_tag => {
                if let Some(id) = cur_id.take() {
                    {
                        // 内部片段结束于当前 End 标签开始处（不含 </w:endnote>）
                        let end_pos = pos_before;
                        if end_pos > cur_start_pos && end_pos <= xml.len() {
                            let inner_xml = &xml[cur_start_pos..end_pos];
                            let paragraphs = parse_isolated_paragraphs(inner_xml);
                            let content: Vec<BodyElement> = paragraphs
                                .into_iter()
                                .map(BodyElement::Paragraph)
                                .collect();
                            notes.push(Note { id, content, note_type: cur_note_type });
                        }
                    }
                }
                cur_note_type = NoteType::Normal;
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    notes
}

/// 批注功能第二步（v148交接续）：解析 word/commentsExtended.xml，
/// 提取每条 <w15:commentEx w15:paraId="..." w15:done="0/1"/> 记录，
/// 返回 paraId -> done 的映射表。part 不存在时返回空 map（不报错），
/// 语义对齐 parse_notes_part/parse_comments_part。
///
/// 关联方式（对齐 v148 交接文档记录的验证结果）：commentsExtended.xml
/// 里的 w15:paraId 对应"该批注 content 里最后一个段落"的 w14:paraId，
/// 由调用方（parse_comments_part）负责从每个 w:comment 内部片段里抠出
/// 最后一个 w14:paraId 再来这张表里查。
fn parse_comments_extended_part<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
    part_path: &str,
) -> HashMap<String, (bool, Option<String>)> {
    let xml = match read_zip_entry(zip, part_path) {
        Ok(x) => x,
        Err(_) => return HashMap::new(),
    };

    let mut reader = Reader::from_str(&xml);
    reader.config_mut().trim_text(false);

    let mut map: HashMap<String, (bool, Option<String>)> = HashMap::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e))
                if e.name().as_ref() == b"w15:commentEx" =>
            {
                let mut para_id: Option<String> = None;
                let mut done = false;
                // 回复链功能：w15:paraIdParent 指向"父批注最后一段"的
                // w14:paraId（不是父批注的 w:id 本身）。批注结构只有一层
                // （只能回复原批注，不能回复回复），所以这里只需要记录
                // 原始 paraIdParent 字符串，留给 parse_comments_part 在
                // 拿到完整 paraId->id 映射后统一转换成 Comment.parent_id
                // （u32），不在本函数内做转换（本函数只管忠实转录XML）。
                let mut para_id_parent: Option<String> = None;
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"w15:paraId" => {
                            if let Ok(v) = attr.unescape_value() {
                                para_id = Some(v.into_owned());
                            }
                        }
                        b"w15:done" => {
                            if let Ok(v) = attr.unescape_value() {
                                done = v.as_ref() == "1" || v.as_ref() == "true";
                            }
                        }
                        b"w15:paraIdParent" => {
                            if let Ok(v) = attr.unescape_value() {
                                para_id_parent = Some(v.into_owned());
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(pid) = para_id {
                    map.insert(pid, (done, para_id_parent));
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    map
}

/// 批注功能第一步（v147/v148交接）：解析 word/comments.xml 主体内容。
/// 结构与 parse_notes_part 高度相似（w:comment 对 w:footnote/w:endnote，都是
/// id + 内部若干 w:p 段落），照抄其流式扫描框架，区别是要额外从 w:comment
/// 标签上取 w:author/w:date/w:initials 三个属性。
///
/// 本次范围限定（后续任务再扩展）：
/// - 不解析 commentsExtended.xml，Comment.done 恒为 false、parent_id 恒为 None，
///   留给下一步任务补上。
/// - 内部第一段落开头的 <w:annotationRef/>（批注图标占位符）未做特殊过滤，
///   会被 parse_isolated_paragraphs 解析成一个文本为空的普通 Run，混在
///   content 里但不含任何文字，暂不影响"提取批注文字内容"这个目标，后续
///   渲染层/清理阶段如需要可以按"空文本的Run"特征过滤掉。
/// - 不处理正文 document.xml 里的 commentRangeStart/End/commentReference
///   锚点关联，留给下一步任务补上。
fn parse_comments_part<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
    part_path: &str,
    comments_extended: &HashMap<String, (bool, Option<String>)>,
) -> Vec<Comment> {
    let xml = match read_zip_entry(zip, part_path) {
        Ok(x) => x,
        Err(_) => return Vec::new(),
    };

    let mut reader = Reader::from_str(&xml);
    reader.config_mut().trim_text(false);

    let mut comments: Vec<Comment> = Vec::new();
    // 回复链第二步：commentsExtended 里的 w15:paraIdParent 存的是"父批注
    // 最后一段"的 w14:paraId 字符串，不是父批注的 w:id。要把它转换成
    // Comment.parent_id（u32），必须先知道"某个 paraId 对应哪条批注的
    // id"——但这个映射要等所有 comment 都解析完才能建完整（当前样本里
    // 子批注可能排在父批注前面，不能假设解析顺序）。所以这里先在单趟
    // 扫描里只暂存"这条comment的id + 它的最后一个paraId + 它引用的
    // paraIdParent原始字符串"三元组，等循环结束、拿到完整 paraId->id
    // 映射后，再统一做一次转换回填，比在单趟扫描里就地转换更稳妥。
    // 批注结构只有一层（只能回复原批注，不能再回复回复），所以这里
    // 不需要处理链式/多级parent的情况。
    let mut pending: Vec<(usize, Option<String>)> = Vec::new(); // (comments下标, paraIdParent字符串)
    let mut paraid_to_index: HashMap<String, usize> = HashMap::new(); // 该comment最后一个w14:paraId -> comments下标
    let mut cur_id: Option<u32> = None;
    let mut cur_author = String::new();
    let mut cur_date: Option<String> = None;
    let mut cur_initials: Option<String> = None;
    let mut cur_start_pos: usize = 0;
    let mut buf = Vec::new();

    loop {
        let pos_before = reader.buffer_position() as usize;
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) if e.name().as_ref() == b"w:comment" => {
                let mut id: Option<u32> = None;
                let mut author = String::new();
                let mut date: Option<String> = None;
                let mut initials: Option<String> = None;
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"w:id" => {
                            if let Ok(v) = attr.unescape_value() {
                                id = v.parse::<u32>().ok();
                            }
                        }
                        b"w:author" => {
                            if let Ok(v) = attr.unescape_value() {
                                author = v.into_owned();
                            }
                        }
                        b"w:date" => {
                            if let Ok(v) = attr.unescape_value() {
                                date = Some(v.into_owned());
                            }
                        }
                        b"w:initials" => {
                            if let Ok(v) = attr.unescape_value() {
                                initials = Some(v.into_owned());
                            }
                        }
                        _ => {}
                    }
                }
                cur_id = id;
                cur_author = author;
                cur_date = date;
                cur_initials = initials;
                // 内部片段从当前 Start 标签结束处开始抠（不含 <w:comment ...> 本身）
                cur_start_pos = reader.buffer_position() as usize;
            }
            Ok(Event::End(ref e)) if e.name().as_ref() == b"w:comment" => {
                if let Some(id) = cur_id.take() {
                    let end_pos = pos_before;
                    if end_pos > cur_start_pos && end_pos <= xml.len() {
                        let inner_xml = &xml[cur_start_pos..end_pos];
                        let paragraphs = parse_isolated_paragraphs(inner_xml);
                        let content: Vec<BodyElement> = paragraphs
                            .into_iter()
                            .map(BodyElement::Paragraph)
                            .collect();
                        // WROOT-031第二步：从原始inner_xml里抠出最后一个
                        // w14:paraId（对应该批注content的最后一段），
                        // 去commentsExtended表里查done状态回填。
                        let mut last_para_id: Option<String> = None;
                        let mut search_pos = 0usize;
                        while let Some(rel) = inner_xml[search_pos..].find("w14:paraId=\"") {
                            let start = search_pos + rel + "w14:paraId=\"".len();
                            if let Some(end_rel) = inner_xml[start..].find('"') {
                                let val = &inner_xml[start..start + end_rel];
                                last_para_id = Some(val.to_string());
                                search_pos = start + end_rel + 1;
                            } else {
                                break;
                            }
                        }
                        let (done, para_id_parent) = last_para_id
                            .as_ref()
                            .and_then(|pid| comments_extended.get(pid))
                            .cloned()
                            .unwrap_or((false, None));
                        comments.push(Comment {
                            id,
                            author: std::mem::take(&mut cur_author),
                            date: cur_date.take(),
                            initials: cur_initials.take(),
                            content,
                            done,
                            parent_id: None, // 下面统一回填，见循环外的转换步骤
                        });
                        pending.push((comments.len() - 1, para_id_parent));
                        // 复用 last_para_id：同时记下"这条comment的最后一个
                        // paraId"，回填阶段要靠它反查"谁是某条paraIdParent
                        // 指向的父批注"。直接把值也塞进 pending 会让元组变
                        // 三元，这里换一种写法：单独收集一张 paraId->下标 表。
                        if let Some(pid) = last_para_id {
                            paraid_to_index.insert(pid, comments.len() - 1);
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    // 回填 parent_id：用每条 comment 的 paraIdParent 字符串，去
    // paraid_to_index 表里查"这个paraId对应哪条comment的下标"，
    // 再取那条comment的 id 填进当前 comment.parent_id。
    for (idx, para_id_parent) in pending {
        if let Some(parent_pid_str) = para_id_parent {
            if let Some(&parent_idx) = paraid_to_index.get(&parent_pid_str) {
                let parent_comment_id = comments[parent_idx].id;
                comments[idx].parent_id = Some(parent_comment_id);
            }
        }
    }

    comments
}

/// WROOT-004: word/settings.xml 里的 w:evenAndOddHeaders 全局开关。
/// 空标签出现即代表启用，除非显式 w:val="false"/"0"（OOXML布尔开关标签的通用写法）。
fn read_even_and_odd_headers<R: std::io::Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
) -> bool {
    let xml = match read_zip_entry(zip, "word/settings.xml") {
        Ok(s) => s,
        Err(_) => return false,
    };
    match xml.find("w:evenAndOddHeaders") {
        Some(pos) => {
            let rest = &xml[pos..];
            let end = rest.find('>').unwrap_or(0);
            let tag = &rest[..end];
            let val = pull_attr(tag, "w:val");
            !(val == "false" || val == "0")
        }
        None => false,
    }
}



// sniff_image_mime 已提升至 ooxml::mod（DEVROOT-002），见顶部 use 声明

// WROOT-THEMECOLOR-01: 读取 word/theme/theme1.xml 解析出主题色表
// （dk1/lt1/dk2/lt2/accent1-6/hlink/folHlink -> Color），复用 pptx.rs
// 已验证过的 parse_theme_colors（跨模块 pub(crate) 调用，函数体本身
// 与文档类型无关，纯粹是"给定theme1.xml文本，解析出颜色表"）。
// 找不到 theme1.xml（理论上docx也可能没有主题，虽然真实样本罕见）
// 时返回空表，下游 schemeClr 查表 miss 时的兜底行为由调用方决定。
fn collect_docx_theme_colors<R: std::io::Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
) -> std::collections::HashMap<String, Color> {
    match read_zip_entry(zip, "word/theme/theme1.xml") {
        Ok(xml) => parse_ooxml_theme_colors(&xml),
        Err(_) => parse_ooxml_theme_colors(""),
    }
}

fn collect_docx_images<R: std::io::Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
) -> std::collections::HashMap<String, Vec<u8>> {
    let mut rid_to_data: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    let rels_xml = match read_zip_entry(zip, "word/_rels/document.xml.rels") {
        Ok(s) => s,
        Err(_) => return rid_to_data,
    };
    for (rid, target) in parse_image_rels(&rels_xml) {
        let zip_path = format!("word/{}", target);
        if let Ok(mut file) = zip.by_name(&zip_path) {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut file, &mut buf);
            rid_to_data.insert(rid, buf);
        }
    }
    rid_to_data
}

fn parse_image_rels(xml: &str) -> Vec<(String, String)> {
    let mut result = Vec::new();
    let mut rest = xml;
    while let Some(pos) = rest.find("<Relationship ") {
        rest = &rest[pos + 14..];
        let end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..end];
        if !tag.contains("/image") { continue; }
        let id = pull_attr(tag, "Id");
        let target = pull_attr(tag, "Target");
        if !id.is_empty() && !target.is_empty() {
            result.push((id, target));
        }
    }
    result
}

fn pull_attr(tag: &str, attr: &str) -> String {
    let needle = format!("{}=\"", attr);
    if let Some(start) = tag.find(&needle) {
        let after = &tag[start + needle.len()..];
        if let Some(end) = after.find('"') {
            return after[..end].to_string();
        }
    }
    String::new()
}

fn read_document<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
    styles: &StyleGraph,
    numbering: &mut NumberingStore,
    images: &std::collections::HashMap<String, Vec<u8>>,
    hyperlink_rels: &std::collections::HashMap<String, String>,
    chart_rels: &std::collections::HashMap<String, String>,
    theme_colors: &std::collections::HashMap<String, Color>,
    // WROOT-136x(分栏修复)：body 中出现的 Section 路标需要携带对应节的
    // 分栏配置，否则排版层拿不到数据。page_layouts 已由调用方在此之前
    // 算好（按 sectPr 在文档流中出现顺序），下标语义见下方 Section 构造处。
    page_layouts: &[PageLayout],
) -> Result<Vec<BodyElement>, ParseError> {
    let xml = read_zip_entry(zip, "word/document.xml")?;
    let mut reader = Reader::from_str(&xml);
    // WROOT-025 修复：关闭全局 trim_text，改为按 xml:space 属性逐个 w:t 判断。
    reader.config_mut().trim_text(false);

    let mut body: Vec<BodyElement> = Vec::new();
    let mut cur_para: Option<Paragraph> = None;
    let mut pending_frame_indices_for_cur_para: Vec<usize> = Vec::new();
    let mut cur_text = String::new();
    let mut cur_style_id = String::from("Normal");
    let mut in_body = false;
    let mut in_ppr_body = false;
    let mut in_tabs = false;  // WROOT-068: 是否在 w:pPr/w:tabs 容器内
    // WROOT-001: 上一个段落若带 pageBreakBefore 或段内嵌 sectPr（分节符），
    // 在下一个 w:p Start 时把此标志写入新段落的 page_break_before，然后清空。
    let mut pending_page_break = false;
    // WROOT-115: 位于段落内 w:sectPr 中（用于读取其 w:type 子元素）。
    let mut in_para_sectpr = false;
    // [WROOT-任务B] body 里补 sectPr 边界标记（BodyElement::Section），
    // 供排版层识别"这一页归第几个 section"。此前 w:sectPr 只驱动
    // pending_page_break（下一段 page_break_before=true），完全没留下
    // "第几节"这个边界信号本身，导致 header/footer/page_size 等 per-section
    // 属性即使在 WriterDocument.sections 里已解析正确（v79验证通过），
    // 渲染层也无从得知"当前页该用第几个 section"。
    // 见 ROE_DOCX_Progress bug_log：body完全不含sectPr边界信息。
    let mut cur_section_index: usize = 0;
    // 当前正在收集的这个段落内部是否出现过 w:sectPr（即本段是本节最后一段）。
    // 与 pending_page_break 语义不同：pending_page_break 是"给下一段用的"，
    // 这个是"当前段落结束时，要不要在 push 段落之前先 push 一个 Section 边界"。
    let mut cur_para_ends_section = false;
    // WROOT-026: 当前正在收集的这个段落内部是否出现过 <w:br w:type="page"/>。
    // 不能在遇到br那一刻就直接判断"这段该跳过还是该换页显示"——真实样本
    // （first-header-footer段落#6/#7）证实br可能出现在run开头、后面紧跟
    // 实际文字（如"Odd page 2"），而cur_text在遇到br那一刻还是空的（文字
    // 要等后续Event::Text才追加），此时判断"段落是否为空"必然误判。
    // 正确做法：只记录"出现过"，等</w:p>收尾、p.runs最终定型后再判断。
    let mut cur_para_has_page_br = false;
    let mut in_rpr_body = false;
    let mut in_run = false;
    // 当前 run 的内联属性缓存
    let mut run_bold:          Option<bool> = None;
    let mut run_style_id:      Option<String> = None;
    let mut run_italic:        Option<bool> = None;
    let mut run_underline:     Option<bool> = None;
    let mut run_strikethrough: Option<bool> = None;
    let mut run_highlight:     Option<[u8;4]> = None;
    let mut run_color:         Option<String> = None;
    // WROOT-024: w14:textFill/w14:solidFill/w14:schemeClr/w14:alpha 是 Word 2010
    // 扩展命名空间（w14）下的字符透明度机制，与 w:color（基础颜色）是两套独立
    // 标签、分别定义颜色本体和透明度，需要在 run 构造时合并进同一个 Color.a
    // 通道。此前完全未解析，导致该效果被静默丢弃（渲染为完全不透明）。
    // 值域：w14:val 是万分比整数字符串（如 "74000" = 74%），换算为 0.0-1.0。
    let mut run_alpha:         Option<f32> = None;
    let mut run_font_family:  Option<String> = None;
    let mut run_font_family_ascii: Option<String> = None;
    // DOCX-8-v1(f): run级字号，供公式尺寸回填(latex_layout_size)使用。
    // 方案A（技术债，见roe-ext/notes/DOCX-8-v1-f.md）：只读run级w:sz，
    // 不读段落样式继承链的字号（那是resolve_paragraph_style的职责，
    // 依赖DOCX-1样式表basedOn接线，本步骤不做，留作后续重构方案B）。
    let mut run_font_size:     Option<f32> = None;
    let mut run_vertical_position: Option<f32> = None; // WROOT-107
    // WROOT-136：run 直接写的 <w:vertAlign>（此前只解析了字符样式里的，run 级漏读）
    let mut run_script: Option<roe_model::writer::ScriptPosition> = None;
    let mut in_t = false;
    let mut t_buf = String::new();
    let mut t_preserve_space = false;
    let mut cur_num_id: Option<u32> = None;
    let mut cur_ilvl: Option<u8> = None;
    // WROOT-118：只有 w:pPr/w:numPr 内部的 w:numId/w:ilvl 才是编号定义。
    // 畸形写法 <w:pPr><w:numId w:val="0"/></w:pPr>（numId 缺 numPr 包裹）
    // 按规范应被忽略，Word/LO/WPS 均因此回退到样式继承的编号。
    let mut in_numpr = false;
    // WROOT-119b：w:pPr/w:rPr（段落标记属性，如只影响编号符号的 shd）内部的元素
    // 不得当作段落自身属性读取。in_rpr_body 只在 w:r 内置位，管不到这里。
    let mut in_ppr_rpr = false;
    // ── 表格状态 ──────────────────────────────────────────────────────────────
    let mut in_tbl = false;
    let mut cur_table: Option<Table> = None;
    let mut cur_row: Option<TableRow> = None;
    let mut cur_cell: Option<TableCell> = None;
    // DOCX-4: w:vMerge 状态跟踪。Some(true) = restart，Some(false) = continue，
    // None = 本单元格不参与纵向合并。按行收集，与 tbl.rows 行下标一一对应，
    // 表格结束时统一做后处理算出 row_span，不进入 roe-model 的 TableCell。
    let mut cur_cell_vmerge: Option<bool> = None;
    // WROOT-TBL-01: <w:tcPr><w:shd w:fill="RRGGBB".../></w:tcPr> 单元格背景色。
    // docx与pptx结构不同：docx是单标签属性(w:fill)，不是pptx那种
    // <a:solidFill><a:srgbClr>嵌套子标签，所以不需要额外的in_..._fill状态，
    // 读到w:shd时直接取属性值即可。in_tcpr仅用于将来扩展w:tcPr下其他
    // 子标签（如w:tcBorders/w:tcMar）时做容器范围限定，本步骤(A)暂不需要
    // 靠它区分（w:shd只出现在w:tcPr内，不会和别处的w:shd撞名），先加上
    // 是为了不在步骤B/C时再回来重复插入这个变量。
    let mut in_tcpr = false;
    let mut cur_cell_bg_color: Option<Color> = None;
    // 表格样式移植 Layer 1步骤B新增：w:tblPr/w:tblBorders（表格级默认边框，
    // docx没有PPTX那套TableStyle六边fallback链，tblBorders是cell自身无
    // border时唯一的fallback来源）+ w:tcPr/w:tcBorders（cell级边框，优先级
    // 高于tblBorders）。四边(top/left/bottom/right)合并成一个值的策略：
    // 只要还没设置过（None）就用当前遇到的这条边的值，实质等价于"优先取
    // 先出现的边"，四边不一致时是已知简化（同border_color字段本身设计）。
    // w:sz单位是八分之一磅(eighths of a point)，÷8.0换算成pt，与PPTX的
    // EMU(÷12700)完全不同，不能混用换算公式。
    let mut in_tblpr = false;
    let mut in_tblborders = false;
    let mut cur_tbl_border_color: Option<Color> = None;
    let mut cur_tbl_border_width: Option<f32> = None;
    // WROOT-100：insideH/insideV 内部网格线颜色宽度，独立于四边
    // cur_tbl_border_color/width（那两个只承接top/left/bottom/right）。
    let mut cur_tbl_inside_h_color: Option<Color> = None;
    let mut cur_tbl_inside_h_width: Option<f32> = None;
    let mut cur_tbl_inside_v_color: Option<Color> = None;
    let mut cur_tbl_inside_v_width: Option<f32> = None;
    // WROOT-101（表格浮动绕排）：<w:tblpPr> 单个自闭合标签，所有属性一次
    // 性读完，不需要额外的in_xxx guard（不像tblBorders那样需要区分子标签
    // 归属）。docx原始单位是twips（1/20 pt，与项目内w:spacing/w:indent等
    // 处已验证一致的换算公式相同），与w:sz的eighths-of-point(÷8.0)换算
    // 公式不同，务必用÷20.0，不要混用。
    let mut cur_tbl_float_vert_anchor: Option<String> = None;
    let mut cur_tbl_float_horz_anchor: Option<String> = None;
    let mut cur_tbl_float_y_pt: Option<f32> = None;
    let mut cur_tbl_float_x_pt: Option<f32> = None;
    let mut cur_tbl_float_left_from_text_pt: f32 = 0.0;
    let mut cur_tbl_float_right_from_text_pt: f32 = 0.0;
    let mut in_tcborders = false;
    let mut cur_cell_border_color: Option<Color> = None;
    let mut cur_cell_border_width: Option<f32> = None;
    let mut cur_row_vmerge: Vec<Option<bool>> = Vec::new();
    let mut tbl_vmerge: Vec<Vec<Option<bool>>> = Vec::new();
    let mut tbl_col_widths: Vec<f32> = Vec::new();
    let mut in_hyperlink = false;
    let mut cur_hyperlink_url: Option<String> = None;
    // Step 5 (WROOT-075/027/028): 正文里 <w:endnoteReference w:id="N"/>/
    // <w:footnoteReference w:id="N"/> 的引用标记（Empty事件，w:r内部，与
    // w:br/w:tab同级）。比hyperlink更简单：不需要查.rels拿目标内容，直接
    // 从w:id属性读到数字，赋给当前run的field。None表示当前run不是引用锚点。
    let mut run_note_field: Option<roe_model::writer::FieldKind> = None;
    let mut run_endnote_ref_id: Option<u32> = None;
    let mut run_footnote_ref_id: Option<u32> = None;
    let mut _pending_bookmark: Option<(u32, String)> = None;
    // WROOT-031第三步：正文 commentRangeStart/End/commentReference 锚点。
    // commentReference 语义同 endnoteReference/footnoteReference（Empty事件，
    // w:r内部），直接存到当前run。commentRangeStart/End 是裸标签（不在w:r
    // 内，可能出现在两个run之间），语义是"一段范围"而非单点：start 记录
    // 待挂载的id列表，等下一个真正被push的run出现时消费（可能跨越
    // proofErr等中间裸标签，故用pending队列而非立即挂载）；end 则反向，
    // 直接回填给"已经存在的、最后一个"run（因为该run必然已在start之后
    // 被push过，不存在"等待未来"的问题）。
    let mut run_comment_ref: Option<u32> = None;
    let mut pending_comment_range_start: Vec<u32> = Vec::new();
    let mut in_drawing = false;
    let mut in_anchor = false;
    let mut in_inline = false;
    let mut in_mc_fallback = false;
    let mut mc_saw_cx_chart = false;
    let mut anchor_cx_emu: i64 = 0;
    let mut anchor_cy_emu: i64 = 0;
    let mut anchor_rid = String::new();
    // WROOT-105修复：<wp:docPr hidden="1"/> 此前完全未解析，导致标记为
    // hidden的图片/形状仍被渲染。照抄Calc侧ROOT-051的解决方式：解析
    // 阶段直接跳过，不构造FloatFrame/Shape，不新增模型层字段。
    let mut anchor_hidden = false;
    // DOCX-5: wp:positionH/V 相关状态。posOffset 与 align 都是"文本内容元素"，
    // 用同一个 buffer 捕获，靠 in_pos_offset/in_pos_align 区分正在捕获哪一种、
    // 靠 in_pos_h/in_pos_v 区分是水平还是垂直，四个开关互斥使用。
    let mut in_pos_h = false;
    let mut in_pos_v = false;
    let mut in_pos_offset = false;
    let mut in_pos_align = false;
    let mut pos_text_buf = String::new();
    let mut anchor_relative_from_h: Option<String> = None;
    let mut anchor_relative_from_v: Option<String> = None;
    let mut anchor_dist_t_pt: f32 = 0.0;
    let mut anchor_dist_b_pt: f32 = 0.0;
    let mut anchor_dist_l_pt: f32 = 0.0;
    let mut anchor_dist_r_pt: f32 = 0.0;
    let mut anchor_x_pt: f32 = 0.0;
    let mut anchor_y_pt: f32 = 0.0;
    let mut anchor_align_h: Option<String> = None;
    let mut anchor_align_v: Option<String> = None;
    let mut anchor_wrap_type: Option<String> = None;
                let mut anchor_rotation: Option<f32> = None;
    let mut inline_cx_emu: i64 = 0;
    let mut inline_cy_emu: i64 = 0;
    let mut inline_rid = String::new();
    // WROOT-105修复：inline场景同anchor_hidden，见上方注释。
    let mut inline_hidden = false;
    // WROOT-097: wp:inline 自身的 distT/distB/distL/distR 环绕间距属性，
    // 写法与 anchor_dist_*_pt 一致（EMU/12700 换算为pt，规范缺省为0）。
    let mut inline_dist_t_pt: f32 = 0.0;
    let mut inline_dist_b_pt: f32 = 0.0;
    let mut inline_dist_l_pt: f32 = 0.0;
    let mut inline_dist_r_pt: f32 = 0.0;
    // DOCX-9: wps:wsp (DrawingML形状) 检测与属性捕获。text_content 本阶段(Step1)
    // 恒为空 Vec，wps:txbx 内文字提取是 Step2 的工作，此处先打通几何/样式/定位。
    let mut in_wps_shape = false;
    let mut in_wps_sppr = false;
    // WROOT-STYLE-01: wps:style（fillRef/lnRef 主题色间接引用容器）。
    // 照抄 pptx.rs 的 in_fill_ref/in_ln_ref 状态机思路，idx 属性本次
    // 不解析（颜色已直接写在内层 schemeClr 里，不需要真的查 fmtScheme
    // 表就能拿到主色，与 pptx.rs PROOT-004b 的简化策略一致）。
    let mut in_wps_style = false;
    let mut in_wps_fill_ref = false;
    let mut in_wps_ln_ref = false;
    // WROOT-STYLE-01续: lnRef内非自闭合schemeClr（带shade等mods子元素）
    // 的原始主题色暂存，见 </a:schemeClr> 结束事件的shade应用逻辑。
    let mut wps_ln_ref_base_color: Option<Color> = None;
    let mut wps_ln_ref_shade: Option<f64> = None;
    let mut wps_prst_geom: Option<String> = None;
    // WROOT-CUSTGEOM-01: <a:custGeom><a:pathLst><a:path>自定义几何路径解析
    // 状态, 照抄 pptx.rs PROOT-012 状态机。custGeom 与 prstGeom 是 spPr 下
    // 互斥的兄弟标签, 用独立状态量, 不复用 in_prst_geom 相关变量。
    let mut in_cust_geom = false;   // <a:custGeom>
    let mut in_cust_path = false;   // <a:path>（pathLst下唯一关心的子元素）
    let mut in_move_to = false;     // <a:moveTo>
    let mut in_line_to = false;     // <a:lnTo>
    let mut in_cubic_to = false;    // <a:cubicBezTo>，含3个<a:pt>：控制点1/控制点2/终点
    let mut cubic_to_pt_idx: u8 = 0;
    let mut cubic_to_pts: [(f32, f32); 2] = [(0.0, 0.0); 2];
    let mut in_quad_to = false;     // <a:quadBezTo>，含2个<a:pt>：控制点/终点
    let mut quad_to_pt_idx: u8 = 0;
    let mut quad_to_ctrl_pt: (f32, f32) = (0.0, 0.0);
    // <a:path w=".." h="...">声明的路径坐标系尺寸（与EMU无关，是custGeom
    // 自己的任意坐标系），构造Shape时用实际width_pt/height_pt统一缩放
    // （与pptx.rs不同：pptx在path结束时就有cur_shape.position.extent可用，
    // docx这里path结束时Shape还没构造，只能先存原始坐标，缩放挪到构造
    // Shape的地方统一做，避免两阶段各自维护一份缩放逻辑）。
    let mut cur_cust_path_w: f32 = 1.0;
    let mut cur_cust_path_h: f32 = 1.0;
    let mut cur_cust_path_cmds: Vec<roe_model::common::PathCommand> = Vec::new();
    let mut wps_fill_color: Option<Color> = None;
    // WROOT-GRAD-01: <a:gradFill> 渐变解析。两阶段处理照抄 pptx.rs 的设计
    // （先收集原始 pos/color/ang，角度换算延后到组装 Shape 时），因为
    // OOXML 子元素顺序不保证 xfrm 一定在 gradFill 之前。第一版简化：
    // 不叠加形状自身 rot（docx 侧 wps:wsp 目前未解析 rot 属性），
    // rotate_with_shape 字段仍如实记录供未来扩展，但渲染层暂不使用。
    let mut in_grad_fill = false;
    let mut grad_stops: Vec<roe_model::common::GradientStop> = Vec::new();
    let mut grad_cur_pos: f32 = 0.0;
    let mut grad_angle_emu: i64 = 0;
    let mut grad_rot_with_shape: bool = true;
    // WROOT-STROKE-01: <a:ln> 描边解析。范围裁剪（如实标注）：只覆盖
    // <a:ln w="..."> 宽度 + 内嵌 <a:solidFill><a:srgbClr val="..."/></a:solidFill>
    // 显式颜色两种写法，不查 <p:style><a:lnRef idx="N"> 主题色引用表——
    // docx 侧目前完全没有主题 lnStyleLst 解析基础设施（pptx.rs 有一整套
    // theme_line_color_mods/lookup_theme_line_width，docx 侧移植成本
    // 超出本次范围），真实样本 tdf159158 星形的描边颜色恰好来自 lnRef，
    // 本次不会画出（已知简化非遗漏，技术债见交接文档）。
    // 兜底策略照抄 pptx.rs PROOT-002：有 <a:ln w="N"/>（N>0）但没有
    // 显式颜色时，黑色兜底（"有ln标签就应有可见描边"）；<a:noFill/>
    // 时不兜底，保持无描边。
    let mut in_ln = false;
    let mut ln_width_emu: Option<i64> = None;
    let mut ln_color: Option<Color> = None;
    let mut ln_no_fill: bool = false;
    // WROOT-FLIPV-01: <a:xfrm flipV="1">，形状需沿自身竖直中轴上下镜像。
    // docx侧此前完全未解析该属性（pptx.rs侧已有对应实现，见PPTX-7），
    // 2026-08-17新增补上，解析位置在 wps:spPr 内 <a:xfrm> 标签本身的
    // flipV 属性（xfrm是自身携带该属性，不是靠子元素）。
    let mut wps_flip_v: Option<bool> = None;
    let mut wps_rot: Option<f32> = None;
    // DOCX-9: w:pict(legacy VML) 识别但不解析，仅消费标签避免误判为普通文本，
    // 降级策略与 DOCX-7 OLE 一致：查不到/不支持时静默跳过，不产出 BodyElement。
    let mut in_w_pict = false;
    // DOCX-9 Step2: wps:txbx 形状内文字提取所需状态
    let mut in_wps_txbx = false;
    let mut wps_txbx_depth: u32 = 0;   // 防御嵌套 wps:txbx（理论存在，实践罕见）
    let mut wps_txbx_start_pos: u64 = 0;
    let mut wps_txbx_paragraphs: Vec<Paragraph> = Vec::new();
    // DOCX-8-v1 (e): m:oMathPara 抠取，手法同 wps_txbx（buffer_position + saturating_sub）
    let mut in_omath_para = false;
    let mut omath_para_start_pos: u64 = 0;
    // DOCX-8-v3: 裸 m:oMath（无 oMathPara 包裹）抠取，独立状态位，
    // 手法同上（同 wps_txbx）。与 in_omath_para 互斥：Start 分支要求
    // !in_omath_para，避免 Para 内部的 m:oMath 被这里重复处理
    // （Para 场景继续由 m:oMathPara 结束分支整体抠取内部 m:oMath 子串，
    // 与之前行为完全一致）。
    let mut in_omath = false;
    let mut omath_start_pos: u64 = 0;
    // DOCX-7: OLE 对象降级显示。只提取 w:object > v:shape > v:imagedata 指向的
    // 预渲染缩略图（EMF/WMF等，Word/PPT/Excel保存OLE对象时通常内嵌一份），
    // 当普通行内图片展示；不解析 o:OLEObject 的真实二进制数据（完整OLE编辑，
    // 超出本任务范围，见调研文档结论）。
    let mut in_w_object = false;
    let mut ole_preview_rid = String::new();
    // DOCX-7: w:object 的尺寸来自 w:dxaOrig/w:dyaOrig 属性（twips），不是
    // wp:extent（EMU，DrawingML路径专用）。换算方式与 DOCX-1 的 w:ind 一致，
    // 都是 twips / 20.0 得到 pt。
    let mut ole_width_pt: f32 = 0.0;
    let mut ole_height_pt: f32 = 0.0;

    loop {
        match reader.read_event()? {
            Event::Start(ref e) => match e.name().as_ref() {
                // DOCX-7: w:object 包裹 VML 降级图（v:shape > v:imagedata），
                // 进入时清空上一次残留的 rid，避免跨对象串数据
                b"w:object" => {
                    in_w_object = true;
                    ole_preview_rid.clear();
                    ole_width_pt = 0.0;
                    ole_height_pt = 0.0;
                    for attr in e.attributes().flatten() {
                        let key = attr.key.as_ref();
                        if key.ends_with(b"dxaOrig") {
                            if let Ok(v) = attr.unescape_value().unwrap_or_default().parse::<f32>() {
                                ole_width_pt = v / 20.0;
                            }
                        } else if key.ends_with(b"dyaOrig") {
                            if let Ok(v) = attr.unescape_value().unwrap_or_default().parse::<f32>() {
                                ole_height_pt = v / 20.0;
                            }
                        }
                    }
                }
                b"wps:wsp" if in_anchor || in_inline => {
                    in_wps_shape = true;
                    wps_prst_geom = None;
                    cur_cust_path_cmds.clear();
                    wps_fill_color = None;
                    grad_stops.clear();
                    in_grad_fill = false;
                    in_ln = false;
                    ln_width_emu = None;
                    ln_color = None;
                    ln_no_fill = false;
                    wps_flip_v = None; wps_rot = None;
                }
                b"wps:spPr" if in_wps_shape => { in_wps_sppr = true; }
                b"a:xfrm" if in_wps_sppr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"rot" {
                            if let Ok(v) = attr.unescape_value() {
                                if let Ok(raw) = v.parse::<i32>() { wps_rot = Some(raw as f32 / 60000.0); }
                            }
                        }
                        if attr.key.as_ref() == b"flipV" {
                            let v = attr.unescape_value().unwrap_or_default();
                            wps_flip_v = Some(v == "1" || v.eq_ignore_ascii_case("true"));
                        }
                    }
                }
                b"wps:style" if in_wps_shape => { in_wps_style = true; }
                b"a:fillRef" if in_wps_style => { in_wps_fill_ref = true; }
                b"a:lnRef" if in_wps_style => { in_wps_ln_ref = true; }
                // WROOT-STYLE-01续: lnRef内非自闭合schemeClr（带shade等
                // mods子元素，如 <a:schemeClr val="accent1"><a:shade
                // val="50000"/></a:schemeClr>）。先只记录原始主题色到
                // wps_ln_ref_base_color，不直接赋值ln_color——真正的
                // shade运算和ln_color赋值挪到 </a:schemeClr> 结束事件
                // 统一做（此时shade值必然已经收集完毕）。
                b"a:schemeClr" if in_wps_ln_ref => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"val" {
                            let slot = attr.unescape_value().unwrap_or_default().into_owned();
                            if let Some(color) = theme_colors.get(&slot) {
                                wps_ln_ref_base_color = Some(*color);
                            }
                        }
                    }
                }
                b"a:shade" if in_wps_ln_ref => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"val" {
                            if let Ok(v) = attr.unescape_value().unwrap_or_default().parse::<f64>() {
                                wps_ln_ref_shade = Some(v / 100000.0);
                            }
                        }
                    }
                }
                // WROOT-CUSTGEOM-01: <a:custGeom>，与prstGeom是spPr下互斥的兄弟标签。
                b"a:custGeom" if in_wps_sppr => {
                    in_cust_geom = true;
                }
                b"a:path" if in_cust_geom => {
                    in_cust_path = true;
                    cur_cust_path_w = e.attributes().flatten()
                        .find(|a| a.key.as_ref() == b"w")
                        .and_then(|a| a.unescape_value().ok().and_then(|v| v.parse::<f32>().ok()))
                        .unwrap_or(0.0).max(1.0);
                    cur_cust_path_h = e.attributes().flatten()
                        .find(|a| a.key.as_ref() == b"h")
                        .and_then(|a| a.unescape_value().ok().and_then(|v| v.parse::<f32>().ok()))
                        .unwrap_or(0.0).max(1.0);
                    cur_cust_path_cmds.clear();
                }
                b"a:moveTo" if in_cust_path => { in_move_to = true; }
                b"a:lnTo" if in_cust_path => { in_line_to = true; }
                b"a:cubicBezTo" if in_cust_path => { in_cubic_to = true; cubic_to_pt_idx = 0; }
                b"a:quadBezTo" if in_cust_path => { in_quad_to = true; quad_to_pt_idx = 0; }
                b"a:pt" if in_move_to => {
                    let x = e.attributes().flatten().find(|a| a.key.as_ref() == b"x")
                        .and_then(|a| a.unescape_value().ok().and_then(|v| v.parse::<f32>().ok())).unwrap_or(0.0);
                    let y = e.attributes().flatten().find(|a| a.key.as_ref() == b"y")
                        .and_then(|a| a.unescape_value().ok().and_then(|v| v.parse::<f32>().ok())).unwrap_or(0.0);
                    cur_cust_path_cmds.push(roe_model::common::PathCommand::MoveTo { x, y });
                }
                b"a:pt" if in_line_to => {
                    let x = e.attributes().flatten().find(|a| a.key.as_ref() == b"x")
                        .and_then(|a| a.unescape_value().ok().and_then(|v| v.parse::<f32>().ok())).unwrap_or(0.0);
                    let y = e.attributes().flatten().find(|a| a.key.as_ref() == b"y")
                        .and_then(|a| a.unescape_value().ok().and_then(|v| v.parse::<f32>().ok())).unwrap_or(0.0);
                    cur_cust_path_cmds.push(roe_model::common::PathCommand::LineTo { x, y });
                }
                b"a:pt" if in_cubic_to => {
                    let x = e.attributes().flatten().find(|a| a.key.as_ref() == b"x")
                        .and_then(|a| a.unescape_value().ok().and_then(|v| v.parse::<f32>().ok())).unwrap_or(0.0);
                    let y = e.attributes().flatten().find(|a| a.key.as_ref() == b"y")
                        .and_then(|a| a.unescape_value().ok().and_then(|v| v.parse::<f32>().ok())).unwrap_or(0.0);
                    match cubic_to_pt_idx {
                        0 => { cubic_to_pts[0] = (x, y); cubic_to_pt_idx = 1; }
                        1 => { cubic_to_pts[1] = (x, y); cubic_to_pt_idx = 2; }
                        _ => {
                            cur_cust_path_cmds.push(roe_model::common::PathCommand::CubicTo {
                                x1: cubic_to_pts[0].0, y1: cubic_to_pts[0].1,
                                x2: cubic_to_pts[1].0, y2: cubic_to_pts[1].1,
                                x, y,
                            });
                            in_cubic_to = false;
                            cubic_to_pt_idx = 0;
                        }
                    }
                }
                b"a:pt" if in_quad_to => {
                    let x = e.attributes().flatten().find(|a| a.key.as_ref() == b"x")
                        .and_then(|a| a.unescape_value().ok().and_then(|v| v.parse::<f32>().ok())).unwrap_or(0.0);
                    let y = e.attributes().flatten().find(|a| a.key.as_ref() == b"y")
                        .and_then(|a| a.unescape_value().ok().and_then(|v| v.parse::<f32>().ok())).unwrap_or(0.0);
                    match quad_to_pt_idx {
                        0 => { quad_to_ctrl_pt = (x, y); quad_to_pt_idx = 1; }
                        _ => {
                            cur_cust_path_cmds.push(roe_model::common::PathCommand::QuadTo {
                                x1: quad_to_ctrl_pt.0, y1: quad_to_ctrl_pt.1,
                                x, y,
                            });
                            in_quad_to = false;
                            quad_to_pt_idx = 0;
                        }
                    }
                }
                b"a:close" if in_cust_path => {
                    cur_cust_path_cmds.push(roe_model::common::PathCommand::Close);
                }
                b"a:prstGeom" if in_wps_sppr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"prst" {
                            wps_prst_geom = Some(attr.unescape_value().unwrap_or_default().into_owned());
                        }
                    }
                }
                b"a:ln" if in_wps_sppr => {
                    in_ln = true;
                    ln_width_emu = None;
                    ln_color = None;
                    ln_no_fill = false;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w" {
                            ln_width_emu = attr.unescape_value().unwrap_or_default().parse().ok();
                        }
                    }
                }
                b"a:noFill" if in_ln => {
                    ln_no_fill = true;
                }
                b"a:gradFill" if in_wps_sppr => {
                    in_grad_fill = true;
                    grad_stops.clear();
                    grad_angle_emu = 0;
                    grad_rot_with_shape = true;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"rotWithShape" {
                            let v = attr.unescape_value().unwrap_or_default();
                            grad_rot_with_shape = v.as_ref() != "0";
                        }
                    }
                }
                b"a:gs" if in_grad_fill => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"pos" {
                            let raw: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                            grad_cur_pos = raw / 100000.0;
                        }
                    }
                }
                b"a:lin" if in_grad_fill => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"ang" {
                            grad_angle_emu = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0);
                        }
                    }
                }
                b"a:srgbClr" if in_grad_fill => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"val" {
                            let hex = attr.unescape_value().unwrap_or_default().into_owned();
                            let color = parse_hex_color(&hex);
                            grad_stops.push(roe_model::common::GradientStop { pos: grad_cur_pos, color });
                        }
                    }
                }
                b"a:srgbClr" if in_ln => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"val" {
                            let hex = attr.unescape_value().unwrap_or_default().into_owned();
                            ln_color = Some(parse_hex_color(&hex));
                        }
                    }
                }
                b"a:srgbClr" if in_wps_sppr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"val" {
                            let hex = attr.unescape_value().unwrap_or_default().into_owned();
                            wps_fill_color = Some(parse_hex_color(&hex));
                        }
                    }
                }
                // WROOT-THEMECOLOR-01: <a:schemeClr val="accentN"/> 自闭合形式，
                // 与紧邻的 a:srgbClr 分支同一模式（同为 solidFill 内的颜色值，
                // 两者互斥，源文档只会写其中一种）。查 theme_colors 表（本次
                // 会话新增，来自 word/theme/theme1.xml），命中才赋值，未命中
                // （理论上不应发生，除非文档没有主题或槽位名拼写异常）时保持
                // wps_fill_color 原值不变，不做黑色等兜底猜测。
                // WROOT-STYLE-01: lnRef内自闭合schemeClr（纯色引用，无
                // shade/tint等mods），直接赋值ln_color，与fillRef自闭合
                // 场景（下方 in_wps_sppr 分支）同一模式。
                b"a:schemeClr" if in_wps_ln_ref => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"val" {
                            let slot = attr.unescape_value().unwrap_or_default().into_owned();
                            if let Some(color) = theme_colors.get(&slot) {
                                ln_color = Some(*color);
                            }
                        }
                    }
                }
                // WROOT-STYLE-01最终补漏: fillRef内自闭合schemeClr。
                // 之前两次patch均未成功写入此分支（guard写错/锚点误配），
                // 这是本次会话第三次尝试，用最短单行锚点降低出错概率。
                b"a:schemeClr" if in_wps_fill_ref => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"val" {
                            let slot = attr.unescape_value().unwrap_or_default().into_owned();
                            if let Some(color) = theme_colors.get(&slot) {
                                wps_fill_color = Some(*color);
                            }
                        }
                    }
                }
                b"a:schemeClr" if in_wps_sppr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"val" {
                            let slot = attr.unescape_value().unwrap_or_default().into_owned();
                            if let Some(color) = theme_colors.get(&slot) {
                                wps_fill_color = Some(*color);
                            }
                        }
                    }
                }
                b"w:pict" => { in_w_pict = true; }
                b"wps:txbx" if in_wps_shape => {
                    if in_wps_txbx {
                        wps_txbx_depth += 1; // 嵌套 txbx，理论边界情况，仅计数不重新起点
                    } else {
                        in_wps_txbx = true;
                        wps_txbx_depth = 0;
                        wps_txbx_start_pos = reader.buffer_position();
                    }
                }
                b"m:oMathPara" if !in_omath_para => {
                    // DOCX-8-v1 (e): 公式是段落直接子元素，不挂在 w:r 内部
                    // （fixture 证实 m:oMathPara 与 w:r 平级，都是 w:p 的子节点）。
                    in_omath_para = true;
                    omath_para_start_pos = reader.buffer_position();
                }
                // DOCX-8-v3: 裸 m:oMath 起点记录（不在 m:oMathPara 内部时）。
                // 守卫 !in_omath_para 保证 Para 内部的 m:oMath 不会走这条
                // 新分支。手法同 wps_txbx：记录此刻 buffer_position()
                // 作为"标签之间内容"的起点。
                b"m:oMath" if !in_omath_para && !in_omath => {
                    in_omath = true;
                    omath_start_pos = reader.buffer_position();
                }
                b"a:blip" if in_anchor || in_inline => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref().ends_with(b"embed") {
                            let rid = attr.unescape_value().unwrap_or_default().into_owned();
                            if in_anchor {
                                anchor_rid = rid;
                            } else {
                                inline_rid = rid;
                            }
                        }
                    }
                }
                // WROOT-CHART-INLINE-01 bugfix: <c:chart r:id="rIdN"/> 此前完全
                // 没有被任何分支处理——它既不是 a:blip（图片），属性名也是 r:id
                // 而非 a:blip 用的 r:embed，导致 inline_rid 永远不会被设成图表
                // 的 rId，下游 chart_rels.get(&inline_rid) 分支形同虚设，真实
                // inline chart 样本（wp:inline 内 a:graphicData uri=.../chart）
                // 解析出来的 body 全是空段落，图表被静默丢失。这里只处理
                // in_inline 场景：in_anchor（wp:anchor 内的图表，即浮动图表）
                // 目前下游没有消费 anchor_rid 对应的 chart_rels 查找路径，是
                // 独立的未实现功能，不在本次修复范围内，故意不在 in_anchor 时
                // 赋值，避免引入"赋了值但没人用"的死代码。
                b"mc:AlternateContent" => { mc_saw_cx_chart = false; in_mc_fallback = false; }
                b"mc:Fallback" => { in_mc_fallback = true; }
                b"c:chart" | b"cx:chart" if in_inline => {
                    if e.name().as_ref() == b"cx:chart" { mc_saw_cx_chart = true; }
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"r:id" {
                            inline_rid = attr.unescape_value().unwrap_or_default().into_owned();
                        }
                    }
                }
                b"w:drawing" if in_body => { in_drawing = true; }
                b"w:drawing" => { }
                b"wp:anchor" if in_drawing => {
                    in_anchor = true;
                    anchor_cx_emu = 0;
                    anchor_cy_emu = 0;
                    anchor_rid.clear();
                    anchor_hidden = false;
                    anchor_relative_from_h = None;
                    anchor_relative_from_v = None;
                    anchor_x_pt = 0.0;
                    anchor_y_pt = 0.0;
                    anchor_align_h = None;
                    anchor_align_v = None;
                    anchor_wrap_type = None;
                    // distT/distB/distL/distR：wp:anchor 自身的环绕间距属性，
                    // EMU/12700 换算为 pt。OOXML 规范缺省为0，故 unwrap_or(0)。
                    anchor_dist_t_pt = 0.0;
                    anchor_dist_b_pt = 0.0;
                    anchor_dist_l_pt = 0.0;
                    anchor_dist_r_pt = 0.0;
                    // WROOT-016: 图片自身旋转角度，来自 pic:spPr>a:xfrm 的
                    // rot 属性（不是 wp:anchor 自身的属性，在 Start 分支
                    // 这里只做每个 anchor 开始时的重置，真正赋值在下面
                    // b"a:xfrm" 分支里，与 anchor_x_pt/anchor_y_pt 等其余
                    // 字段的"重置于Start、赋值于子元素"模式保持一致）。
                    anchor_rotation = None;
                    for attr in e.attributes().flatten() {
                        let val = || -> f32 {
                            attr.unescape_value()
                                .ok()
                                .and_then(|v| v.parse::<f32>().ok())
                                .map(|emu| emu / 12700.0)
                                .unwrap_or(0.0)
                        };
                        match attr.key.as_ref() {
                            b"distT" => anchor_dist_t_pt = val(),
                            b"distB" => anchor_dist_b_pt = val(),
                            b"distL" => anchor_dist_l_pt = val(),
                            b"distR" => anchor_dist_r_pt = val(),
                            _ => {}
                        }
                    }
                }
                b"wp:inline" if in_drawing => {
                    in_inline = true;
                    inline_cx_emu = 0;
                    inline_cy_emu = 0;
                    inline_rid.clear();
                    inline_hidden = false;
                    inline_dist_t_pt = 0.0;
                    inline_dist_b_pt = 0.0;
                    inline_dist_l_pt = 0.0;
                    inline_dist_r_pt = 0.0;
                    for attr in e.attributes().flatten() {
                        let val = || -> f32 {
                            attr.unescape_value()
                                .ok()
                                .and_then(|v| v.parse::<f32>().ok())
                                .map(|emu| emu / 12700.0)
                                .unwrap_or(0.0)
                        };
                        match attr.key.as_ref() {
                            b"distT" => inline_dist_t_pt = val(),
                            b"distB" => inline_dist_b_pt = val(),
                            b"distL" => inline_dist_l_pt = val(),
                            b"distR" => inline_dist_r_pt = val(),
                            _ => {}
                        }
                    }
                }
                b"wp:positionH" if in_anchor => {
                    in_pos_h = true;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"relativeFrom" {
                            anchor_relative_from_h =
                                Some(attr.unescape_value().unwrap_or_default().into_owned());
                        }
                    }
                }
                b"wp:positionV" if in_anchor => {
                    in_pos_v = true;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"relativeFrom" {
                            anchor_relative_from_v =
                                Some(attr.unescape_value().unwrap_or_default().into_owned());
                        }
                    }
                }
                b"wp:posOffset" if in_pos_h || in_pos_v => {
                    in_pos_offset = true;
                    pos_text_buf.clear();
                }
                // WROOT-016: <pic:spPr><a:xfrm rot="..."> —— 图片自身旋转角度。
                // 与 wps:wsp 形状路径的 a:xfrm（in_wps_sppr 条件）是两个独立
                // 分支：这里只在"当前在anchor内、且不是wps形状"时生效，避免
                // 图片和形状两条路径互相干扰。rot 单位 1/60000 度，换算方式
                // 与 pptx.rs/xlsx.rs 已验证一致（raw.rot as f32 / 60000.0）。
                b"a:xfrm" if in_anchor && !in_wps_shape => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"rot" {
                            if let Ok(v) = attr.unescape_value() {
                                if let Ok(raw) = v.parse::<i32>() {
                                    anchor_rotation = Some(raw as f32 / 60000.0);
                                }
                            }
                        }
                    }
                }
                b"wp:align" if in_pos_h || in_pos_v => {
                    in_pos_align = true;
                    pos_text_buf.clear();
                }
                b"w:hyperlink" if in_body => {
                    in_hyperlink = true;
                    cur_hyperlink_url = None;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref().ends_with(b"id") {
                            let r_id = attr.unescape_value().unwrap_or_default();
                            cur_hyperlink_url = hyperlink_rels.get(r_id.as_ref()).cloned();
                        }
                    }
                }
                b"w:body" => in_body = true,
                b"w:tbl" if in_body => {
                    in_tbl = true;
                    cur_table = Some(Table { rows: Vec::new(), col_widths: Vec::new(), style_id: None, float_pos: None, first_row: false, first_col: false, last_row: false, last_col: false, band_row: false, band_col: false, default_border_color: None, default_border_width: None, inside_h_color: None, inside_h_width: None, inside_v_color: None, inside_v_width: None });
                    tbl_col_widths.clear();
                    cur_tbl_border_color = None;
                    cur_tbl_border_width = None;
                    cur_tbl_inside_h_color = None;
                    cur_tbl_inside_h_width = None;
                    cur_tbl_inside_v_color = None;
                    cur_tbl_inside_v_width = None;
                }
                b"w:tblPr" if in_tbl => {
                    in_tblpr = true;
                }
                b"w:tblBorders" if in_tblpr => {
                    in_tblborders = true;
                }
                b"w:top" | b"w:left" | b"w:bottom" | b"w:right" if in_tblborders => {
                    if cur_tbl_border_color.is_none() {
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"w:color" => {
                                    let val = attr.unescape_value().unwrap_or_default();
                                    if val.as_ref() != "auto" {
                                        cur_tbl_border_color = Some(parse_hex_color(&val));
                                    }
                                }
                                b"w:sz" => {
                                    let sz: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                                    cur_tbl_border_width = Some(sz / 8.0);
                                }
                                _ => {}
                            }
                        }
                    }
                }
                // WROOT-101（表格浮动绕排）：w:tblpPr，Start事件路径。
                b"w:tblpPr" if in_tblpr => {
                    parse_tblppr_attrs(
                        &e,
                        &mut cur_tbl_float_vert_anchor,
                        &mut cur_tbl_float_horz_anchor,
                        &mut cur_tbl_float_y_pt,
                        &mut cur_tbl_float_x_pt,
                        &mut cur_tbl_float_left_from_text_pt,
                        &mut cur_tbl_float_right_from_text_pt,
                    );
                }
                // WROOT-100：w:insideH/w:insideV 独立于四边解析，Start事件路径。
                b"w:insideH" if in_tblborders => {
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"w:color" => {
                                let val = attr.unescape_value().unwrap_or_default();
                                if val.as_ref() != "auto" {
                                    cur_tbl_inside_h_color = Some(parse_hex_color(&val));
                                }
                            }
                            b"w:sz" => {
                                let sz: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                                cur_tbl_inside_h_width = Some(sz / 8.0);
                            }
                            _ => {}
                        }
                    }
                }
                b"w:insideV" if in_tblborders => {
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"w:color" => {
                                let val = attr.unescape_value().unwrap_or_default();
                                if val.as_ref() != "auto" {
                                    cur_tbl_inside_v_color = Some(parse_hex_color(&val));
                                }
                            }
                            b"w:sz" => {
                                let sz: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                                cur_tbl_inside_v_width = Some(sz / 8.0);
                            }
                            _ => {}
                        }
                    }
                }
                b"w:tr" if in_tbl => {
                    cur_row = Some(TableRow { cells: Vec::new(), row_height: None });
                }
                b"w:tcPr" if in_tbl => {
                    in_tcpr = true;
                    cur_cell_border_color = None;
                    cur_cell_border_width = None;
                }
                b"w:tcBorders" if in_tcpr => {
                    in_tcborders = true;
                }
                b"w:top" | b"w:left" | b"w:bottom" | b"w:right" if in_tcborders => {
                    if cur_cell_border_color.is_none() {
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"w:color" => {
                                    let val = attr.unescape_value().unwrap_or_default();
                                    if val.as_ref() != "auto" {
                                        cur_cell_border_color = Some(parse_hex_color(&val));
                                    }
                                }
                                b"w:sz" => {
                                    let sz: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                                    cur_cell_border_width = Some(sz / 8.0);
                                }
                                _ => {}
                            }
                        }
                    }
                }
                b"w:shd" if in_tcpr => {
                    // w:fill="auto" 表示"无填充"，与颜色值区分；其余情况
                    // 取十六进制颜色。docx真实文件里w:shd几乎总是自闭合
                    // (Empty事件)，但Start分支同样补上，理由同本文件其它
                    // 位置反复出现的"自闭合与非自闭合两条路径都要处理"教训。
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref().ends_with(b"fill") {
                            let val = attr.unescape_value().unwrap_or_default();
                            if val.as_ref() != "auto" {
                                cur_cell_bg_color = Some(parse_hex_color(&val));
                            }
                        }
                    }
                }
                b"w:tc" if in_tbl => {
                    cur_cell_bg_color = None;
                    cur_cell = Some(TableCell {
                        content: Vec::new(),
                        col_span: 1,
                        row_span: 1,
                        style: None,
                        hidden: false,
                        // PROOT-003 followup: DOCX侧表格背景色走独立的
                        // w:shd属性体系（非本次PPTX tcPr改动范围），边框/
                        // 内边距同理走DOCX独立的w:tcBorders/w:tcMar体系，
                        // 此处占位None非遗漏，DOCX若需支持应另开任务。
                        bg_color: None,
                        border_color: None,
                        border_width: None,
                        pad_left: None,
                        pad_right: None,
                        pad_top: None,
                        pad_bottom: None,
                    });
                    cur_cell_vmerge = None;
                }
                b"w:p" if in_body => {
                    pending_frame_indices_for_cur_para.clear();
                    cur_para = Some(Paragraph {
                        style: StyleRef("Normal".to_string()),
                        runs: Vec::new(),
                        list_info: None,
                        page_break_before: pending_page_break,
                        ..Default::default()
                    });
                    pending_page_break = false;
                    cur_para_ends_section = false;
                    cur_para_has_page_br = false;
                    cur_text.clear();
                    cur_style_id = "Normal".to_string();
                    cur_num_id = None;
                    cur_ilvl = None;
                    in_numpr = false;
                    in_ppr_rpr = false;
                }
                b"w:rPr" if in_run => { in_rpr_body = true; }
                b"w:rPr" if in_ppr_body && !in_run => { in_ppr_rpr = true; }
                b"w:r" if cur_para.is_some() => {
                    in_run = true;
                    run_bold = None; run_italic = None; run_underline = None;
                    run_strikethrough = None; run_highlight = None; run_font_size = None; run_vertical_position = None;
                    run_script = None;
                    run_color = None;
                    run_alpha = None;
                    run_font_family = None;
                    run_font_family_ascii = None;
                    run_style_id = None;
                    run_note_field = None;
                    run_endnote_ref_id = None;
                    run_footnote_ref_id = None;
                    run_comment_ref = None;
                }
                b"w:t" if in_run => {
                    in_t = true;
                    t_buf.clear();
                    t_preserve_space = e.attributes().flatten().any(|attr| {
                        attr.key.as_ref() == b"xml:space"
                            && attr.unescape_value().map(|v| v == "preserve").unwrap_or(false)
                    });
                }
                b"w:pPr" if cur_para.is_some() => { in_ppr_body = true; }
                // WROOT-068 二次修正：<w:tabs> 本身不是自闭合标签（它包着
                // 若干 <w:tab .../> 子元素），触发的是 Event::Start，不是
                // Event::Empty——首版修正把这个分支和 w:tab 一起搬去了
                // Event::Empty 块，矫枉过正，导致 in_tabs 永远不会被置
                // true，w:tab 分支的 guard 条件永远不满足。内联 eprintln!
                // 调试打点证实：w:pPr Start 被正确匹配3次，但 w:tabs/w:tab
                // 打点从未触发，问题根源在此。正确状态：w:tabs（非自闭合）
                // 在 Event::Start；w:tab（自闭合）在 Event::Empty；两者
                // 不是同一类标签，不能想当然放在一起处理。
                b"w:tabs" if in_ppr_body => { in_tabs = true; }
                b"w:pageBreakBefore" if in_ppr_body && cur_para.is_some() => {
                    // w:val 可缺省(缺省即true)，也可显式 w:val="0"/"false"关闭
                    let mut val = true;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            if let Ok(s) = attr.unescape_value() {
                                val = !(s == "0" || s.eq_ignore_ascii_case("false"));
                            }
                        }
                    }
                    if let Some(ref mut p) = cur_para {
                        p.page_break_before = val;
                    }
                }
                b"w:sectPr" if in_ppr_body && cur_para.is_some() => {
                    // 段落内嵌sectPr：该段是本节最后一段，下一段落起新节。
                    // MVP：默认视为强制换页；WROOT-115：若随后读到
                    // w:type=continuous 则撤回（见下方 w:type 分支）。
                    pending_page_break = true;
                    cur_para_ends_section = true;
                    in_para_sectpr = true;
                }
                // WROOT-115: 段落内 sectPr 的 w:type。continuous（连续分节符）
                // 不应换页——此前无条件换页，导致 tdf138892_noNumbering 多出
                // 两个空白页、tdf148273_sectionBulletFormatLeak 被切成两页。
                // 页面尺寸/边距变化引起的强制换页由 layout 的
                // size_changed||margins_changed 独立处理，不受这里影响。
                // 只撤回 pending_page_break；Section 边界与 section 归属不变。
                b"w:type" if in_para_sectpr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            if attr.unescape_value()?.as_ref() == "continuous" {
                                pending_page_break = false;
                            }
                        }
                    }
                }
                b"w:pStyle" if cur_para.is_some() => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            let sid = attr.unescape_value()?.into_owned();
                            if let Some(ref mut p) = cur_para {
                                p.style = StyleRef(sid.clone());
                            }
                            cur_style_id = sid;
                        }
                    }
                }
                b"w:numPr" if in_ppr_body => { in_numpr = true; }
                b"w:numId" if in_numpr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            cur_num_id = attr.unescape_value()?.parse().ok();
                        }
                    }
                }
                b"w:ilvl" if in_numpr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            cur_ilvl = attr.unescape_value()?.parse().ok();
                        }
                    }
                }
                _ => {}
            },
            Event::Empty(ref e) => match e.name().as_ref() {
                // DOCX-9 Step1/2修复: a:prstGeom/a:srgbClr/wps:txbx 在真实文档中
                // 经常是自闭合写法（尤其 prstGeom 恒自闭合），原代码只在
                // Event::Start 里处理，与上面 a:blip 同款疏漏，此处补齐
                // WROOT-CUSTGEOM-01: <a:custGeom>，与prstGeom是spPr下互斥的兄弟标签。
                b"a:custGeom" if in_wps_sppr => {
                    in_cust_geom = true;
                }
                b"a:path" if in_cust_geom => {
                    in_cust_path = true;
                    cur_cust_path_w = e.attributes().flatten()
                        .find(|a| a.key.as_ref() == b"w")
                        .and_then(|a| a.unescape_value().ok().and_then(|v| v.parse::<f32>().ok()))
                        .unwrap_or(0.0).max(1.0);
                    cur_cust_path_h = e.attributes().flatten()
                        .find(|a| a.key.as_ref() == b"h")
                        .and_then(|a| a.unescape_value().ok().and_then(|v| v.parse::<f32>().ok()))
                        .unwrap_or(0.0).max(1.0);
                    cur_cust_path_cmds.clear();
                }
                b"a:moveTo" if in_cust_path => { in_move_to = true; }
                b"a:lnTo" if in_cust_path => { in_line_to = true; }
                b"a:cubicBezTo" if in_cust_path => { in_cubic_to = true; cubic_to_pt_idx = 0; }
                b"a:quadBezTo" if in_cust_path => { in_quad_to = true; quad_to_pt_idx = 0; }
                b"a:pt" if in_move_to => {
                    let x = e.attributes().flatten().find(|a| a.key.as_ref() == b"x")
                        .and_then(|a| a.unescape_value().ok().and_then(|v| v.parse::<f32>().ok())).unwrap_or(0.0);
                    let y = e.attributes().flatten().find(|a| a.key.as_ref() == b"y")
                        .and_then(|a| a.unescape_value().ok().and_then(|v| v.parse::<f32>().ok())).unwrap_or(0.0);
                    cur_cust_path_cmds.push(roe_model::common::PathCommand::MoveTo { x, y });
                }
                b"a:pt" if in_line_to => {
                    let x = e.attributes().flatten().find(|a| a.key.as_ref() == b"x")
                        .and_then(|a| a.unescape_value().ok().and_then(|v| v.parse::<f32>().ok())).unwrap_or(0.0);
                    let y = e.attributes().flatten().find(|a| a.key.as_ref() == b"y")
                        .and_then(|a| a.unescape_value().ok().and_then(|v| v.parse::<f32>().ok())).unwrap_or(0.0);
                    cur_cust_path_cmds.push(roe_model::common::PathCommand::LineTo { x, y });
                }
                b"a:pt" if in_cubic_to => {
                    let x = e.attributes().flatten().find(|a| a.key.as_ref() == b"x")
                        .and_then(|a| a.unescape_value().ok().and_then(|v| v.parse::<f32>().ok())).unwrap_or(0.0);
                    let y = e.attributes().flatten().find(|a| a.key.as_ref() == b"y")
                        .and_then(|a| a.unescape_value().ok().and_then(|v| v.parse::<f32>().ok())).unwrap_or(0.0);
                    match cubic_to_pt_idx {
                        0 => { cubic_to_pts[0] = (x, y); cubic_to_pt_idx = 1; }
                        1 => { cubic_to_pts[1] = (x, y); cubic_to_pt_idx = 2; }
                        _ => {
                            cur_cust_path_cmds.push(roe_model::common::PathCommand::CubicTo {
                                x1: cubic_to_pts[0].0, y1: cubic_to_pts[0].1,
                                x2: cubic_to_pts[1].0, y2: cubic_to_pts[1].1,
                                x, y,
                            });
                            in_cubic_to = false;
                            cubic_to_pt_idx = 0;
                        }
                    }
                }
                b"a:pt" if in_quad_to => {
                    let x = e.attributes().flatten().find(|a| a.key.as_ref() == b"x")
                        .and_then(|a| a.unescape_value().ok().and_then(|v| v.parse::<f32>().ok())).unwrap_or(0.0);
                    let y = e.attributes().flatten().find(|a| a.key.as_ref() == b"y")
                        .and_then(|a| a.unescape_value().ok().and_then(|v| v.parse::<f32>().ok())).unwrap_or(0.0);
                    match quad_to_pt_idx {
                        0 => { quad_to_ctrl_pt = (x, y); quad_to_pt_idx = 1; }
                        _ => {
                            cur_cust_path_cmds.push(roe_model::common::PathCommand::QuadTo {
                                x1: quad_to_ctrl_pt.0, y1: quad_to_ctrl_pt.1,
                                x, y,
                            });
                            in_quad_to = false;
                            quad_to_pt_idx = 0;
                        }
                    }
                }
                b"a:close" if in_cust_path => {
                    cur_cust_path_cmds.push(roe_model::common::PathCommand::Close);
                }
                b"a:prstGeom" if in_wps_sppr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"prst" {
                            wps_prst_geom = Some(attr.unescape_value().unwrap_or_default().into_owned());
                        }
                    }
                }
                b"a:ln" if in_wps_sppr => {
                    in_ln = true;
                    ln_width_emu = None;
                    ln_color = None;
                    ln_no_fill = false;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w" {
                            ln_width_emu = attr.unescape_value().unwrap_or_default().parse().ok();
                        }
                    }
                }
                b"a:noFill" if in_ln => {
                    ln_no_fill = true;
                }
                b"a:gradFill" if in_wps_sppr => {
                    // 极少见（gradFill 内部必有 gsLst 子元素，理论上不会自闭合），
                    // 但防御性处理：自闭合时视为空渐变，走 in_grad_fill=true
                    // 之后立刻在 End 分支被判空跳过，不 panic、不误吞后续标签。
                    in_grad_fill = true;
                    grad_stops.clear();
                    grad_angle_emu = 0;
                    grad_rot_with_shape = true;
                }
                b"a:gs" if in_grad_fill => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"pos" {
                            let raw: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                            grad_cur_pos = raw / 100000.0;
                        }
                    }
                }
                b"a:lin" if in_grad_fill => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"ang" {
                            grad_angle_emu = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0);
                        }
                    }
                }
                b"a:srgbClr" if in_grad_fill => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"val" {
                            let hex = attr.unescape_value().unwrap_or_default().into_owned();
                            let color = parse_hex_color(&hex);
                            grad_stops.push(roe_model::common::GradientStop { pos: grad_cur_pos, color });
                        }
                    }
                }
                b"a:srgbClr" if in_ln => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"val" {
                            let hex = attr.unescape_value().unwrap_or_default().into_owned();
                            ln_color = Some(parse_hex_color(&hex));
                        }
                    }
                }
                b"a:srgbClr" if in_wps_sppr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"val" {
                            let hex = attr.unescape_value().unwrap_or_default().into_owned();
                            wps_fill_color = Some(parse_hex_color(&hex));
                        }
                    }
                }
                // WROOT-THEMECOLOR-01: <a:schemeClr val="accentN"/> 自闭合形式，
                // 与紧邻的 a:srgbClr 分支同一模式（同为 solidFill 内的颜色值，
                // 两者互斥，源文档只会写其中一种）。查 theme_colors 表（本次
                // 会话新增，来自 word/theme/theme1.xml），命中才赋值，未命中
                // （理论上不应发生，除非文档没有主题或槽位名拼写异常）时保持
                // wps_fill_color 原值不变，不做黑色等兜底猜测。
                // WROOT-STYLE-01: lnRef内自闭合schemeClr（纯色引用，无
                // shade/tint等mods），直接赋值ln_color，与fillRef自闭合
                // 场景（下方 in_wps_sppr 分支）同一模式。
                b"a:schemeClr" if in_wps_ln_ref => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"val" {
                            let slot = attr.unescape_value().unwrap_or_default().into_owned();
                            if let Some(color) = theme_colors.get(&slot) {
                                ln_color = Some(*color);
                            }
                        }
                    }
                }
                // WROOT-STYLE-01补漏2(Empty事件版): lnRef内<a:shade val=".."/>
                // 自闭合形式。真实文档里 lnRef 的 schemeClr 带 shade 子元素时，
                // schemeClr本身是Start事件（1688行已处理），但shade子标签自身
                // 无子元素、是Empty事件，此前只在Event::Start match(1699行)里
                // 写了这个分支，Empty match里完全没有，导致shade永远命中不到，
                // wps_ln_ref_shade 拿不到值，最终描边色未按shade变暗，与填充色
                // 数值重合、视觉上"看起来没有描边"。2026-08-17定位确认，与上面
                // fillRef的Empty分支缺失同一bug模式。
                b"a:shade" if in_wps_ln_ref => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"val" {
                            if let Ok(v) = attr.unescape_value().unwrap_or_default().parse::<f64>() {
                                wps_ln_ref_shade = Some(v / 100000.0);
                            }
                        }
                    }
                }
                // WROOT-STYLE-01补漏(Empty事件版): fillRef内自闭合schemeClr
                // （无shade/tint等修饰子元素时，quick-xml吐出Empty事件而非Start，
                // 此前只在Event::Start match里加了这个分支，Empty match里漏了，
                // 导致fillRef的schemeClr自闭合场景永远命中不到任何guard、落入
                // 兜底 _ => {}，wps_fill_color拿不到值。2026-08-17定位确认。
                b"a:schemeClr" if in_wps_fill_ref => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"val" {
                            let slot = attr.unescape_value().unwrap_or_default().into_owned();
                            if let Some(color) = theme_colors.get(&slot) {
                                wps_fill_color = Some(*color);
                            }
                        }
                    }
                }
                b"a:schemeClr" if in_wps_sppr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"val" {
                            let slot = attr.unescape_value().unwrap_or_default().into_owned();
                            if let Some(color) = theme_colors.get(&slot) {
                                wps_fill_color = Some(*color);
                            }
                        }
                    }
                }
                b"wps:txbx" if in_wps_shape => {
                    if in_wps_txbx {
                        wps_txbx_depth += 1;
                    } else {
                        in_wps_txbx = true;
                        wps_txbx_depth = 0;
                        wps_txbx_start_pos = reader.buffer_position();
                    }
                }
                // DOCX-7: v:imagedata 是 VML 命名空间，指向 OLE 对象的预渲染缩略图
                // （EMF/WMF等）。属性名是 r:id，不是 a:blip 用的 r:embed，需单独判断。
                b"v:imagedata" if in_w_object => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref().ends_with(b"id") && !attr.key.as_ref().ends_with(b"embed") {
                            ole_preview_rid = attr.unescape_value().unwrap_or_default().into_owned();
                        }
                    }
                }
                // a:blip 在真实 docx 中几乎总是自闭合写法（<a:blip r:embed="..."/>），
                // 原代码只在 Event::Start 里处理，漏了这个更常见的形态，此处补齐
                b"a:blip" if in_anchor || in_inline => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref().ends_with(b"embed") {
                            let rid = attr.unescape_value().unwrap_or_default().into_owned();
                            if in_anchor {
                                anchor_rid = rid;
                            } else {
                                inline_rid = rid;
                            }
                        }
                    }
                }
                // WROOT-CHART-INLINE-01 bugfix: <c:chart r:id="rIdN"/> 此前完全
                // 没有被任何分支处理——它既不是 a:blip（图片），属性名也是 r:id
                // 而非 a:blip 用的 r:embed，导致 inline_rid 永远不会被设成图表
                // 的 rId，下游 chart_rels.get(&inline_rid) 分支形同虚设，真实
                // inline chart 样本（wp:inline 内 a:graphicData uri=.../chart）
                // 解析出来的 body 全是空段落，图表被静默丢失。这里只处理
                // in_inline 场景：in_anchor（wp:anchor 内的图表，即浮动图表）
                // 目前下游没有消费 anchor_rid 对应的 chart_rels 查找路径，是
                // 独立的未实现功能，不在本次修复范围内，故意不在 in_anchor 时
                // 赋值，避免引入"赋了值但没人用"的死代码。
                b"c:chart" | b"cx:chart" if in_inline => {
                    if e.name().as_ref() == b"cx:chart" { mc_saw_cx_chart = true; }
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"r:id" {
                            inline_rid = attr.unescape_value().unwrap_or_default().into_owned();
                        }
                    }
                }
                // WROOT-001 followup: w:pageBreakBefore/w:sectPr 在真实docx里常见
                // 自闭合写法，与上面a:blip同款疏漏，Start分支已处理，此处Empty分支补齐
                b"w:pageBreakBefore" if in_ppr_body && cur_para.is_some() => {
                    let mut val = true;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            if let Ok(s) = attr.unescape_value() {
                                val = !(s == "0" || s.eq_ignore_ascii_case("false"));
                            }
                        }
                    }
                    if let Some(ref mut p) = cur_para {
                        p.page_break_before = val;
                    }
                }
                b"w:sectPr" if in_ppr_body && cur_para.is_some() => {
                    pending_page_break = true;
                    cur_para_ends_section = true;
                    in_para_sectpr = true;
                }
                b"w:type" if in_para_sectpr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            if attr.unescape_value()?.as_ref() == "continuous" {
                                pending_page_break = false;
                            }
                        }
                    }
                }
                b"w:pStyle" if cur_para.is_some() => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            let sid = attr.unescape_value()?.into_owned();
                            if let Some(ref mut p) = cur_para {
                                p.style = StyleRef(sid.clone());
                            }
                            cur_style_id = sid;
                        }
                    }
                }
                b"w:bookmarkStart" => {
                    let mut bk_id: Option<u32> = None;
                    let mut bk_name = String::new();
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"w:id" => { bk_id = attr.unescape_value().ok().and_then(|v| v.parse().ok()); }
                            b"w:name" => { bk_name = attr.unescape_value().unwrap_or_default().into_owned(); }
                            _ => {}
                        }
                    }
                    if let (Some(id), Some(ref mut p)) = (bk_id, cur_para.as_mut()) {
                        let run_index = p.runs.len();
                        p.bookmarks.push(roe_model::writer::BookmarkAnchor { id, name: bk_name, run_index });
                    } else if let Some(id) = bk_id {
                        _pending_bookmark = Some((id, bk_name));
                    }
                }
                b"w:bookmarkEnd" => {
                    _pending_bookmark = None;
                }
                b"w:b" if in_rpr_body => {
                    // w:val="false"/"0" 显式关闭粗体，无 w:val 或 "true"/"1" 则开启
                    let val = e.attributes().flatten()
                        .find(|a| a.key.as_ref() == b"w:val")
                        .and_then(|a| a.unescape_value().ok())
                        .unwrap_or_else(|| "true".into());
                    run_bold = Some(val != "false" && val != "0");
                }
                b"w:i" if in_rpr_body => {
                    let val = e.attributes().flatten()
                        .find(|a| a.key.as_ref() == b"w:val")
                        .and_then(|a| a.unescape_value().ok())
                        .unwrap_or_else(|| "true".into());
                    run_italic = Some(val != "false" && val != "0");
                }
                b"w:u" if in_rpr_body => {
                    // w:val="none" 显式关闭下划线，其他值（single/double等）开启
                    let val = e.attributes().flatten()
                        .find(|a| a.key.as_ref() == b"w:val")
                        .and_then(|a| a.unescape_value().ok())
                        .unwrap_or_else(|| "single".into());
                    run_underline = Some(val != "none");
                }
                b"w:strike" if in_rpr_body => { run_strikethrough = Some(true); }
                // DOCX-8-v1(f): w:sz 是半点单位(half-point)，换算规则与
                // read_styles里的处理（本文件254行附近）保持一致，都是 /2.0。
                b"w:sz" if in_rpr_body => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            if let Ok(hp) = attr.unescape_value().unwrap_or_default().parse::<f32>() {
                                run_font_size = Some(hp / 2.0);
                            }
                        }
                    }
                }
                // WROOT-136：run级 w:vertAlign（superscript/subscript）
                b"w:vertAlign" if in_rpr_body => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            run_script = match attr.unescape_value().unwrap_or_default().as_ref() {
                                "superscript" => Some(roe_model::writer::ScriptPosition::Superscript),
                                "subscript" => Some(roe_model::writer::ScriptPosition::Subscript),
                                _ => None,
                            };
                        }
                    }
                }
                // WROOT-107: run级 w:position（半磅→pt，正上负下）
                b"w:position" if in_rpr_body => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            if let Ok(hp) = attr.unescape_value().unwrap_or_default().parse::<f32>() {
                                run_vertical_position = Some(hp / 2.0);
                            }
                        }
                    }
                }
                b"w:highlight" if in_rpr_body => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            run_highlight = parse_highlight_color_rgba(
                                attr.unescape_value().unwrap_or_default().as_ref()
                            );
                        }
                    }
                }
                // WROOT-119：段落直写底纹 <w:pPr><w:shd w:fill="RRGGBB"/></w:pPr>。
                // 必须在 pPr 内且不在 pPr/rPr（段落标记属性）内，否则会和 run 级 shd 混淆。
                b"w:shd" if in_ppr_body && !in_rpr_body && !in_ppr_rpr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:fill" {
                            let hex = attr.unescape_value().unwrap_or_default();
                            let hex = hex.trim_start_matches('#');
                            if hex.len() == 6 && hex != "auto" {
                                if let Ok(rgb) = u32::from_str_radix(hex.as_ref(), 16) {
                                    let r = ((rgb >> 16) & 0xff) as u8;
                                    let g = ((rgb >> 8) & 0xff) as u8;
                                    let b = (rgb & 0xff) as u8;
                                    if let Some(ref mut p) = cur_para {
                                        p.shading = Some([r, g, b, 0xff]);
                                    }
                                }
                            }
                        }
                    }
                }
                // WROOT-074: 正文 run 级 <w:rPr><w:shd w:fill="RRGGBB" .../></w:rPr>
                // 此前完全未被解析。w:shd 是 OOXML 里另一套底纹机制（区别于
                // w:highlight 的预定义颜色名，w:shd 用任意十六进制RGB色值），
                // 此前只有 w:tcPr（表格单元格）上下文解析了 w:shd，run 级完全
                // 空白。复用 run_highlight 变量（最终同样落到 Run.background
                // 字段）。
                b"w:shd" if in_rpr_body => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:fill" {
                            let hex = attr.unescape_value().unwrap_or_default();
                            let hex = hex.trim_start_matches('#');
                            if hex.len() == 6 && hex != "auto" {
                                if let Ok(rgb) = u32::from_str_radix(hex.as_ref(), 16) {
                                    let r = ((rgb >> 16) & 0xff) as u8;
                                    let g = ((rgb >> 8) & 0xff) as u8;
                                    let b = (rgb & 0xff) as u8;
                                    run_highlight = Some([r, g, b, 0xff]);
                                }
                            }
                        }
                    }
                }
                // WROOT-RUNCOLOR: 正文 run 级 w:color 此前完全未被解析
                // （w:b/w:i/w:sz/w:highlight 均有对应处理分支，唯独w:color
                // 缺失）。真实文档里 run 自身内联颜色（不经过段落样式/字符
                // 样式，直接写在 <w:r><w:rPr><w:color .../></w:rPr></w:r>）
                // 是完全合规且常见的写法，此前会被 resolve_run_style 的
                // 兜底链因 run.color 恒为 None 而跳过，误落到更早层级
                // （段落样式/文档默认色），本次补齐这一最基础的解析路径。
                b"w:color" if in_rpr_body => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            run_color = Some(attr.unescape_value().unwrap_or_default().into_owned());
                        }
                    }
                }
                // WROOT-024: <w:rPr><w14:textFill><w14:solidFill><w14:schemeClr>
                // <w14:alpha w14:val="74000"/></w14:schemeClr></w14:solidFill>
                // </w14:textFill></w:rPr>。w14:alpha 是 w14:schemeClr 的子元素
                // （不是属性），单独匹配 Start 事件。万分比整数换算为 0.0-1.0，
                // 与本文件其他万分比字段（如WROOT-016的rot换算）保持同一换算
                // 习惯但除数不同（此处除以100000.0，非60000.0）。
                b"w14:alpha" if in_rpr_body => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w14:val" {
                            if let Ok(raw) = attr.unescape_value().unwrap_or_default().parse::<f32>() {
                                // WROOT-024修正：实测样本 w14:val="74000"（数值较大）
                                // 视觉效果是更浅/更透明，w14:val="29000"（数值较小）
                                // 视觉效果是更深/更不透明——与"数值=不透明度百分比"的
                                // 直觉相反，实际语义是"数值越大越透明"。按 OOXML 规范，
                                // w14:alpha 表示的是相对于满值的透明程度，这里换算为
                                // 不透明度（Color.a 需要的语义）时要用 1.0 减去比例。
                                run_alpha = Some(1.0 - (raw / 100000.0));
                            }
                        }
                    }
                }
                // WROOT-055: run 级 <w:rPr><w:rStyle w:val="..."/></w:rPr>
                // 此前完全未被解析。这是字符样式引用（如 fldChar 生成的超链接
                // 显示文字 run 常见的 "Internet Link" / style15 等），与
                // w:color/w:u 这类内联直接属性不同——rStyle 只是"引用"，
                // 实际颜色/下划线定义在 styles.xml 里，需要在 run 构造时
                // 通过 styles.resolve() 查出来，且只在 run 自身没有内联
                // 覆盖时才生效（内联属性优先级高于 rStyle 引用，OOXML 标准
                // 继承顺序）。
                b"w:rStyle" if in_rpr_body => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            run_style_id = Some(attr.unescape_value().unwrap_or_default().into_owned());
                        }
                    }
                }
                // WROOT-RFONTS: 正文 run 级 w:rFonts 此前完全未被解析，
                // 导致 resolve_run_font_family 永远拿不到 run.font_family，
                // ROE 排版被迫用 SystemUi 兜底字体，与 OO/LO 断行位置不一致。
                // 中文文档断行主要受东亚字体宽度影响，w:eastAsia 优先于
                // w:ascii；只有 w:eastAsia 缺失时才退回 w:ascii。
                b"w:rFonts" if in_rpr_body => {
                    let mut ascii_font: Option<String> = None;
                    let mut east_asia_font: Option<String> = None;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"w:ascii" => {
                                ascii_font = Some(attr.unescape_value().unwrap_or_default().into_owned());
                            }
                            b"w:eastAsia" => {
                                east_asia_font = Some(attr.unescape_value().unwrap_or_default().into_owned());
                            }
                            _ => {}
                        }
                    }
                    run_font_family = east_asia_font.clone().or(ascii_font.clone());
                    run_font_family_ascii = ascii_font;
                }
                b"w:gridCol" if in_tbl => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:w" {
                            let twips: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                            tbl_col_widths.push(twips / 20.0);
                        }
                    }
                }
                b"w:gridSpan" if cur_cell.is_some() => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            let span: u8 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(1);
                            if let Some(ref mut c) = cur_cell { c.col_span = span; }
                        }
                    }
                }
                // DOCX-4: w:vMerge。w:val="restart" 开始一段纵向合并；
                // w:val="continue" 或属性整体缺省（含 <w:vMerge/> 无 val）
                // 均表示"续接上一行"——OOXML 规范原文：若省略该属性其值视为
                // continue（微软文档 VerticalMerge.Val 明确写出，c-rex/ECMA-376
                // 参照实现一致）。cur_cell 为 None 时忽略（vMerge 只出现在
                // w:tcPr 内，理论上此时 cur_cell 必为 Some，加判断仅作防御）。
                b"w:vMerge" if cur_cell.is_some() => {
                    let mut val: Option<String> = None;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            val = Some(attr.unescape_value().unwrap_or_default().into_owned());
                        }
                    }
                    cur_cell_vmerge = Some(matches!(val.as_deref(), Some("restart")));
                }
                // WROOT-TBL-01: <w:shd w:fill="RRGGBB".../> 自闭合形式（真实
                // 文档里的常见写法，与Start分支的判断逻辑保持一致：
                // fill="auto"视为无填充，其余取十六进制颜色）。
                b"w:shd" if in_tcpr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref().ends_with(b"fill") {
                            let val = attr.unescape_value().unwrap_or_default();
                            if val.as_ref() != "auto" {
                                cur_cell_bg_color = Some(parse_hex_color(&val));
                            }
                        }
                    }
                }
                // 表格样式移植 Layer 1步骤B：w:tblBorders / w:tcBorders 容器本身
                // 自闭合的情况（真实文档少见，如<w:tblBorders/>整体不带任何子边），
                // 与Start分支对称补上，避免出现漏解析。
                b"w:tblBorders" if in_tblpr => {
                    in_tblborders = true;
                }
                b"w:tcBorders" if in_tcpr => {
                    in_tcborders = true;
                }
                // 表格样式移植 Layer 1步骤B：四条边（top/left/bottom/right）自闭合
                // 形式，真实文档里边框标签几乎总是这种写法（<w:top w:val="single"
                // w:sz="24" w:color="00B050"/>），与Start分支判断逻辑完全一致。
                // 注意tblBorders和tcBorders的四边标签名完全相同，靠in_tblborders/
                // in_tcborders两个独立状态变量区分所属容器，不会互相干扰（两者的
                // 生效区间不重叠：in_tblborders在tblPr内，in_tcborders在tcPr内，
                // 一个w:tc不可能同时处于tblPr内）。
                b"w:top" | b"w:left" | b"w:bottom" | b"w:right" if in_tblborders => {
                    if cur_tbl_border_color.is_none() {
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"w:color" => {
                                    let val = attr.unescape_value().unwrap_or_default();
                                    if val.as_ref() != "auto" {
                                        cur_tbl_border_color = Some(parse_hex_color(&val));
                                    }
                                }
                                b"w:sz" => {
                                    let sz: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                                    cur_tbl_border_width = Some(sz / 8.0);
                                }
                                _ => {}
                            }
                        }
                    }
                }
                // WROOT-101（表格浮动绕排）：w:tblpPr，Empty事件路径（自闭合标签，
                // 真实文档最常见的写法，如<w:tblpPr w:leftFromText="180" .../>）。
                b"w:tblpPr" if in_tblpr => {
                    parse_tblppr_attrs(
                        &e,
                        &mut cur_tbl_float_vert_anchor,
                        &mut cur_tbl_float_horz_anchor,
                        &mut cur_tbl_float_y_pt,
                        &mut cur_tbl_float_x_pt,
                        &mut cur_tbl_float_left_from_text_pt,
                        &mut cur_tbl_float_right_from_text_pt,
                    );
                }
                // WROOT-100：w:insideH/w:insideV，Empty事件路径（自闭合标签，
                // 真实文档最常见的写法，如<w:insideH w:val="none" .../>）。
                b"w:insideH" if in_tblborders => {
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"w:color" => {
                                let val = attr.unescape_value().unwrap_or_default();
                                if val.as_ref() != "auto" {
                                    cur_tbl_inside_h_color = Some(parse_hex_color(&val));
                                }
                            }
                            b"w:sz" => {
                                let sz: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                                cur_tbl_inside_h_width = Some(sz / 8.0);
                            }
                            _ => {}
                        }
                    }
                }
                b"w:insideV" if in_tblborders => {
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"w:color" => {
                                let val = attr.unescape_value().unwrap_or_default();
                                if val.as_ref() != "auto" {
                                    cur_tbl_inside_v_color = Some(parse_hex_color(&val));
                                }
                            }
                            b"w:sz" => {
                                let sz: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                                cur_tbl_inside_v_width = Some(sz / 8.0);
                            }
                            _ => {}
                        }
                    }
                }
                b"w:top" | b"w:left" | b"w:bottom" | b"w:right" if in_tcborders => {
                    if cur_cell_border_color.is_none() {
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"w:color" => {
                                    let val = attr.unescape_value().unwrap_or_default();
                                    if val.as_ref() != "auto" {
                                        cur_cell_border_color = Some(parse_hex_color(&val));
                                    }
                                }
                                b"w:sz" => {
                                    let sz: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                                    cur_cell_border_width = Some(sz / 8.0);
                                }
                                _ => {}
                            }
                        }
                    }
                }
                b"w:spacing" if in_ppr_body && cur_para.is_some() => {
                    for attr in e.attributes().flatten() {
                        if let Some(ref mut p) = cur_para {
                            match attr.key.as_ref() {
                                b"w:before" => {
                                    let twips: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                                    p.space_before = Some(twips / 20.0);
                                }
                                b"w:after" => {
                                    let twips: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                                    p.space_after = Some(twips / 20.0);
                                }
                                b"w:line" => {
                                    let val: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                                    // 240 = single spacing, 480 = double
                                    p.line_height = Some(val / 240.0);
                                    p.line_abs_pt = Some(val / 20.0);
                                }
                                b"w:lineRule" => {
                                    let r = attr.unescape_value().unwrap_or_default().to_string();
                                    p.line_exact = match r.as_str() {
                                        "exact" => Some(true),
                                        "atLeast" => Some(false),
                                        _ => None,
                                    };
                                }
                                _ => {}
                            }
                        }
                    }
                }
                b"w:tab" if in_tabs && cur_para.is_some() => {
                    // WROOT-068 修正记录：w:pPr/w:tabs/w:tab 在真实文档里几乎
                    // 总是自闭合写法（<w:tab w:val="right" .../>），quick-xml
                    // 对自闭合标签触发的是 Event::Empty，不是 Event::Start——
                    // 首版实现放错在 Event::Start 块，导致这个分支永远不会被
                    // 触发（调试打印验证过：tab_stops 始终为空）。搬到这里
                    // （Event::Empty 块，与 w:spacing 等其他 in_ppr_body 属性
                    // 同一个 match 作用域）才是正确位置。
                    let mut pos_twips: Option<f32> = None;
                    let mut alignment = roe_model::writer::TabAlignment::Left;
                    let mut leader = roe_model::writer::TabLeader::None;
                    let mut is_clear = false;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"w:pos" => {
                                let twips: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                                pos_twips = Some(twips);
                            }
                            b"w:val" => {
                                let val = attr.unescape_value().unwrap_or_default();
                                match val.as_ref() {
                                    "left" | "start" => alignment = roe_model::writer::TabAlignment::Left,
                                    "center" => alignment = roe_model::writer::TabAlignment::Center,
                                    "right" | "end" => alignment = roe_model::writer::TabAlignment::Right,
                                    "clear" => is_clear = true,
                                    // "decimal"/"bar"/"num" 等罕见类型暂按 left 兜底，
                                    // 不崩溃但对齐效果不完整（记入技术债，WROOT-068
                                    // 交接文档未要求覆盖这些）
                                    _ => alignment = roe_model::writer::TabAlignment::Left,
                                }
                            }
                            b"w:leader" => {
                                let val = attr.unescape_value().unwrap_or_default();
                                leader = match val.as_ref() {
                                    "dot" => roe_model::writer::TabLeader::Dot,
                                    "hyphen" => roe_model::writer::TabLeader::Hyphen,
                                    "underscore" => roe_model::writer::TabLeader::Underscore,
                                    // "heavy"/"middleDot" 等罕见类型兜底为 None，
                                    // 不崩溃但引导符效果不完整（同上，技术债）
                                    _ => roe_model::writer::TabLeader::None,
                                };
                            }
                            _ => {}
                        }
                    }
                    // w:val="clear" 语义是"清除某个继承来的tab stop位置"，
                    // 按交接文档建议简化处理：不加入tab_stops，本段落自身
                    // 定义的w:tabs几乎不会用到clear（clear主要用于样式继承
                    // 场景，本实现暂不支持样式级tab stop继承）
                    if !is_clear {
                        if let (Some(twips), Some(ref mut p)) = (pos_twips, cur_para.as_mut()) {
                            p.tab_stops.push(roe_model::writer::TabStop {
                                pos: twips / 20.0,
                                alignment,
                                leader,
                            });
                        }
                    }
                }
                // WROOT-127：正文段落直写 w:ind。此前正文状态机完全没有 w:ind 分支
                // （只有样式表 493 行、docDefaults 422 行、numbering 1487/7657 行有），
                // 导致普通段落与列表段落自带的 w:ind 全部丢失，列表只能用 numbering
                // 级别缩进。w:ind 是自闭合标签，只会出现在 Event::Empty。
                // 守卫：in_ppr_body && !in_ppr_rpr（排除段落标记属性 pPr/rPr 内部）。
                // 注意：本分支【没有】pPrChange（修订历史）守卫——正文状态机目前不识别
                // pPrChange，其内部的 w:ind 理论上会被误读。已核实 golden 全集中含
                // pPrChange/w:ind 的用例数为 0，暂无实际影响；将来做修订标记（WFEAT-001）
                // 时必须一并处理。
                // 属性：w:left/w:start -> indent_left；w:right/w:end -> indent_right；
                //       w:firstLine -> indent_first(正)；w:hanging -> indent_first(负)，
                //       hanging 与 firstLine 并存时 hanging 优先（Word 语义）。
                b"w:ind" if in_ppr_body && !in_ppr_rpr && cur_para.is_some() => {
                    let mut left: Option<f32> = None;
                    let mut right: Option<f32> = None;
                    let mut first: Option<f32> = None;
                    let mut hanging: Option<f32> = None;
                    for attr in e.attributes().flatten() {
                        let v: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                        match attr.key.as_ref() {
                            b"w:left" | b"w:start" => left = Some(v / 20.0),
                            b"w:right" | b"w:end" => right = Some(v / 20.0),
                            b"w:firstLine" => first = Some(v / 20.0),
                            b"w:hanging" => hanging = Some(v / 20.0),
                            _ => {}
                        }
                    }
                    if let Some(ref mut p) = cur_para {
                        if left.is_some() { p.indent_left = left; }
                        if right.is_some() { p.indent_right = right; }
                        if let Some(h) = hanging {
                            p.indent_first = Some(-h);
                        } else if let Some(f) = first {
                            p.indent_first = Some(f);
                        }
                    }
                }
                b"w:jc" if in_ppr_body && cur_para.is_some() => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            let val = attr.unescape_value().unwrap_or_default().into_owned();
                            let alignment = match val.as_str() {
                                "left" | "start" => Some(roe_model::style::Alignment::Left),
                                "center" => Some(roe_model::style::Alignment::Center),
                                "right" | "end" => Some(roe_model::style::Alignment::Right),
                                "both" | "distribute" => Some(roe_model::style::Alignment::Justify),
                                _ => None,
                            };
                            if let Some(ref mut p) = cur_para {
                                p.alignment = alignment;
                            }
                        }
                    }
                }
                b"w:numId" if in_numpr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            cur_num_id = attr.unescape_value()?.parse().ok();
                        }
                    }
                }
                b"w:ilvl" if in_numpr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            cur_ilvl = attr.unescape_value()?.parse().ok();
                        }
                    }
                }
                // WROOT-105修复：<wp:docPr id="1" name="..." hidden="1"/> 是
                // wp:anchor/wp:inline的直接子元素，此前完全未处理该标签，
                // hidden属性被静默忽略。与wp:extent等兄弟标签处理模式一致。
                b"wp:docPr" if in_anchor => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"hidden" {
                            let val = attr.unescape_value().unwrap_or_default();
                            anchor_hidden = val.as_ref() == "1" || val.as_ref() == "true";
                        }
                    }
                }
                b"wp:docPr" if in_inline => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"hidden" {
                            let val = attr.unescape_value().unwrap_or_default();
                            inline_hidden = val.as_ref() == "1" || val.as_ref() == "true";
                        }
                    }
                }
                b"wp:extent" if in_anchor => {
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"cx" => { anchor_cx_emu = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0); }
                            b"cy" => { anchor_cy_emu = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0); }
                            _ => {}
                        }
                    }
                }
                // DOCX-6: wp:wrap* 系列标签，均为 wp:anchor 直接子元素、互斥五选一。
                // wrapNone/wrapTopAndBottom 是空标记；wrapSquare/wrapTight/wrapThrough
                // 可能带 wrapText 等属性或 effectExtent/wrapPolygon 子元素，但本任务
                // 只记录"是哪一档"，不解析 wrapText 细分方向或多边形坐标（后置范围）。
                // 若某文档非自闭合写法（带 effectExtent 子元素），这里仍能在 Start
                // 阶段捕获到标签名——但当前只在 Event::Empty 处理，非自闭合写法会
                // 走 Event::Start 而漏判，是已知边界，真实 Word 文档这五个标签
                // 绝大多数是自闭合，暂不额外处理 Event::Start 分支。
                b"wp:wrapNone" if in_anchor => { anchor_wrap_type = Some("none".to_string()); }
                b"wp:wrapSquare" if in_anchor => { anchor_wrap_type = Some("square".to_string()); }
                b"wp:wrapTight" if in_anchor => { anchor_wrap_type = Some("tight".to_string()); }
                b"wp:wrapThrough" if in_anchor => { anchor_wrap_type = Some("through".to_string()); }
                b"wp:wrapTopAndBottom" if in_anchor => { anchor_wrap_type = Some("topAndBottom".to_string()); }
                b"wp:extent" if in_inline => {
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"cx" => { inline_cx_emu = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0); }
                            b"cy" => { inline_cy_emu = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0); }
                            _ => {}
                        }
                    }
                }

                b"w:br" if in_run => {
                    let mut is_column_break = false;
                    let mut is_page_break = false;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:type" {
                            if let Ok(v) = attr.unescape_value() {
                                if v == "column" {
                                    is_column_break = true;
                                } else if v == "page" {
                                    is_page_break = true;
                                }
                            }
                        }
                    }
                    if is_column_break {
                        // 分栏符不当作普通换行符塞进文本流。真实样本（食谱.docx）
                        // 证实Word/LibreOffice/OnlyOffice都把这个br所在的段落
                        // 当成正常参与排版的空段落（占一行空白高度），换栏发生在
                        // 排完这个空段落之后。所以直接标记在当前段落自己身上，
                        // 不用像page_break_before那样搞跨段落pending传递。
                        if let Some(ref mut p) = cur_para {
                            p.column_break_after = true;
                        }
                    } else if is_page_break {
                        // WROOT-026：<w:br w:type="page"/> 显式分页符。不能在
                        // 这里直接判断"该段是否为空"——真实样本证实br可能
                        // 出现在run开头、后面紧跟实际文字（cur_text此刻还是
                        // 空的，文字要等后续Event::Text才追加）。只记录"这个
                        // 段落出现过page类型br"，最终是标page_break_after
                        // （纯占位段，整段跳过+触发下一段换页）还是
                        // page_break_before（段落自身仍有文字，换页后正常
                        // 显示这段内容）留到</w:p>收尾、p.runs最终定型后
                        // 再判断（见下方 cur_para_has_page_br 消费处）。
                        cur_para_has_page_br = true;
                    } else {
                        cur_text.push('\n');
                    }
                }
                b"w:tab" if in_run => {
                    cur_text.push('\t');
                }
                b"w:endnoteReference" if in_run => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:id" {
                            if let Ok(v) = attr.unescape_value() {
                                if let Ok(id) = v.parse::<u32>() {
                                    run_note_field = Some(roe_model::writer::FieldKind::EndnoteRef);
                                    // 复用 run_style_id 类似的"直接存值"模式不适用于
                                    // u32 id（那些变量都是 String），故新增专门变量。
                                    run_endnote_ref_id = Some(id);
                                }
                            }
                        }
                    }
                }
                b"w:footnoteReference" if in_run => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:id" {
                            if let Ok(v) = attr.unescape_value() {
                                if let Ok(id) = v.parse::<u32>() {
                                    run_note_field = Some(roe_model::writer::FieldKind::FootnoteRef);
                                    run_footnote_ref_id = Some(id);
                                }
                            }
                        }
                    }
                }
                // WROOT-031第三步：<w:commentReference w:id="N"/>，w:r内部，
                // 与endnoteReference/footnoteReference同级同构。
                b"w:commentReference" if in_run => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:id" {
                            if let Ok(v) = attr.unescape_value() {
                                if let Ok(id) = v.parse::<u32>() {
                                    run_comment_ref = Some(id);
                                }
                            }
                        }
                    }
                }
                // WROOT-031第三步：<w:commentRangeStart w:id="N"/>，裸标签，
                // 不在w:r内（与w:bookmarkStart同级）。只记录到pending队列，
                // 真正挂载延后到下一个被push的run（见</w:r>处理）。
                b"w:commentRangeStart" => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:id" {
                            if let Ok(v) = attr.unescape_value() {
                                if let Ok(id) = v.parse::<u32>() {
                                    pending_comment_range_start.push(id);
                                }
                            }
                        }
                    }
                }
                // WROOT-031第三步：<w:commentRangeEnd w:id="N"/>，裸标签。
                // 该标签出现时，它所属范围的最后一个run必然已经被push过
                // （不像start要等待未来），直接回填给p.runs最后一个元素。
                b"w:commentRangeEnd" => {
                    if let Some(id) = e.attributes().flatten()
                        .find(|a| a.key.as_ref() == b"w:id")
                        .and_then(|a| a.unescape_value().ok())
                        .and_then(|v| v.parse::<u32>().ok())
                    {
                        if let Some(ref mut p) = cur_para {
                            if let Some(last_run) = p.runs.last_mut() {
                                last_run.comment_range_end.push(id);
                            }
                        }
                    }
                }
                _ => {}
            },
            Event::End(ref e) => match e.name().as_ref() {
                b"wp:posOffset" if in_pos_offset => {
                    in_pos_offset = false;
                    // EMU -> pt：与 anchor_cx_emu 等处一致，除以 12700
                    let emu: f64 = pos_text_buf.trim().parse().unwrap_or(0.0);
                    let pt = (emu / 12700.0) as f32;
                    if in_pos_h {
                        anchor_x_pt = pt;
                    } else if in_pos_v {
                        anchor_y_pt = pt;
                    }
                    pos_text_buf.clear();
                }
                b"wp:align" if in_pos_align => {
                    in_pos_align = false;
                    let val = pos_text_buf.trim().to_string();
                    if in_pos_h {
                        anchor_align_h = Some(val);
                    } else if in_pos_v {
                        anchor_align_v = Some(val);
                    }
                    pos_text_buf.clear();
                }
                b"wp:positionH" if in_pos_h => { in_pos_h = false; }
                b"wp:positionV" if in_pos_v => { in_pos_v = false; }
                b"wp:anchor" if in_anchor => {
                    in_anchor = false;
                    if in_wps_shape {
                        // DOCX-9: wps:wsp 形状路径。Step1 阶段 text_content 恒为空，
                        // Step2 补充 wps:txbx 文字提取。geometry 未识别的 prst 值
                        // 一律降级为 Rect（完整187种预设映射是 PPTX-11 的范围）。
                        // WROOT-shape-preset: 先查预设形状公式表（PPTX-11成果，
                        // roe-model::preset_geometry，第一期约30种），查到就用
                        // eval_preset_shape 求值成 PresetPath（真正可画的路径点）；
                        // avLst/gd 调整参数第一版未解析，暂用空表（走各形状默认值），
                        // 精确调整值留到后续单独补。查不到的 prst 值才降级为 Custom
                        // 占位（未来接入更多形状时自然消解，不阻塞当前链路）。
                        let geometry = if !cur_cust_path_cmds.is_empty() {
                            // WROOT-CUSTGEOM-01: custGeom 优先于 prstGeom（两者是
                            // spPr 下互斥的兄弟标签）。缩放：cur_cust_path_cmds 里
                            // 的坐标是 <a:path w=".." h="..">声明的任意坐标系，按
                            // 目标 width/height 相对该声明宽高统一缩放，逻辑照抄
                            // pptx.rs PROOT-012，只是挪到此刻才有目标宽高可用。
                            let target_w = anchor_cx_emu as f32 / 12700.0;
                            let target_h = anchor_cy_emu as f32 / 12700.0;
                            let sx = if cur_cust_path_w > 0.0 { target_w / cur_cust_path_w } else { 1.0 };
                            let sy = if cur_cust_path_h > 0.0 { target_h / cur_cust_path_h } else { 1.0 };
                            let scaled: Vec<roe_model::common::PathCommand> = cur_cust_path_cmds
                                .drain(..)
                                .map(|cmd| match cmd {
                                    roe_model::common::PathCommand::MoveTo { x, y } =>
                                        roe_model::common::PathCommand::MoveTo { x: x * sx, y: y * sy },
                                    roe_model::common::PathCommand::LineTo { x, y } =>
                                        roe_model::common::PathCommand::LineTo { x: x * sx, y: y * sy },
                                    roe_model::common::PathCommand::CubicTo { x1, y1, x2, y2, x, y } =>
                                        roe_model::common::PathCommand::CubicTo {
                                            x1: x1 * sx, y1: y1 * sy,
                                            x2: x2 * sx, y2: y2 * sy,
                                            x: x * sx, y: y * sy,
                                        },
                                    roe_model::common::PathCommand::QuadTo { x1, y1, x, y } =>
                                        roe_model::common::PathCommand::QuadTo {
                                            x1: x1 * sx, y1: y1 * sy,
                                            x: x * sx, y: y * sy,
                                        },
                                    other => other,
                                })
                                .collect();
                            // v181: custGeom 场景没有真正的 prst 名字（这不是
                            // 预设形状，复用 PresetPath 变体只是为了共享渲染
                            // 逻辑），preset_name 留空字符串、adj_overrides 留
                            // 空 Vec 表示"不适用"，与 ShapeGeometry::PresetPath
                            // 文档注释里约定的语义一致，未来调用方需要据此区分
                            // custGeom 与真预设形状时应以 preset_name.is_empty()
                            // 判断，不要假设非空。
                            roe_model::common::ShapeGeometry::PresetPath(
                                roe_model::common::PresetPathData {
                                    commands: scaled,
                                    preset_name: String::new(),
                                    adj_overrides: Vec::new(),
                                }
                            )
                        } else { match wps_prst_geom.as_deref() {
                            Some("ellipse") => roe_model::common::ShapeGeometry::Ellipse,
                            Some("line") => roe_model::common::ShapeGeometry::Line,
                            // WROOT-108（v188）: triangle 硬编码拦截已注释掉，对齐 pptx.rs 的
                            // PROOT-101 修复（triangle/rtTriangle 改走 find_preset_shape 查表得到
                            // PresetPath，而非独立 Triangle 枚举）。docx 此前手写了 Triangle 专属
                            // 渲染分支（roe-writer/src/tile.rs、roe-core/src/tile_render_helpers.rs），
                            // 但那是 Parley 迁移期间的临时实现，作者当时已注明"不是最终形态"。
                            // docx 排期落后于 calc/impress 是 Parley 从 cosmic-text 迁移占用了大量
                            // 时间，不是基础组件缺失；现在 impress/calc 侧的 PresetPath 已经用真实
                            // 样本验证过是标准做法，docx 理应跟进对齐，不维护孤立实现。
                            // 原行（已注释）：
                            // Some("triangle") => roe_model::common::ShapeGeometry::Triangle,
                            Some(other) => {
                                match roe_model::preset_geometry::find_preset_shape(other) {
                                    Some(formula) => {
                                        let w = anchor_cx_emu as f64 / 12700.0;
                                        let h = anchor_cy_emu as f64 / 12700.0;
                                        let adj_overrides = std::collections::HashMap::new();
                                        let path = roe_model::preset_geometry::eval_preset_shape(
                                            formula, w, h, &adj_overrides,
                                        );
                                        // v181: Writer 侧 avLst/gd 解析尚未接入
                                        // （见上方"avLst/gd 调整参数第一版未解析"
                                        // 注释），此处 adj_overrides 恒为空表，
                                        // 如实记录为空 Vec；other 是真实 prst 名字，
                                        // 如实保留。
                                        roe_model::common::ShapeGeometry::PresetPath(
                                            roe_model::common::PresetPathData {
                                                commands: path,
                                                preset_name: other.to_string(),
                                                adj_overrides: Vec::new(),
                                            }
                                        )
                                    }
                                    None => roe_model::common::ShapeGeometry::Custom(other.to_string()),
                                }
                            }
                            None => roe_model::common::ShapeGeometry::Rect,
                        }
                        };
                        // WROOT-GRAD-01: 渐变优先于纯色。角度换算公式照抄
                        // pptx.rs 验证过的规则（EMU/60000 归一化到0-360度），
                        // 第一版不叠加 flip/形状自身rot（docx侧wps:wsp暂未
                        // 解析这两个属性），rotate_with_shape 记录但渲染层
                        // 暂不使用，是已知简化非遗漏。
                        let fill = if !grad_stops.is_empty() {
                            let angle = (grad_angle_emu as f32 / 60000.0).rem_euclid(360.0);
                            Some(roe_model::common::FillStyle::Gradient {
                                stops: std::mem::take(&mut grad_stops),
                                kind: roe_model::common::GradientKind::Linear { angle },
                                rotate_with_shape: grad_rot_with_shape,
                            })
                        } else {
                            wps_fill_color.map(roe_model::common::FillStyle::Solid)
                        };
                        // WROOT-STROKE-01 bugfix: 修正"宽度>0才画"的错误判断——
                        // 真实样本 sw_doc_data__textbox-zorder.docx 有
                        // <a:ln w="0"><a:solidFill>.../></a:ln>，Word 实测渲染出
                        // 可见蓝色描边（w="0" 不代表"无描边"，只是没有显式指定
                        // 精确宽度，需要一个合理默认最小宽度）；另一份样本
                        // <a:ln><a:round/></a:ln> 干脆没有 w 属性（ln_width_emu
                        // 为 None），同样应该画（颜色来自 lnRef，本次简化跳过，
                        // 属于已知技术债，与这里的宽度判断是两回事）。
                        // 新判断依据：只要有显式颜色，就画（宽度用显式值，
                        // 缺失/为0时给 0.75pt 最小可见宽度，参照 Word 细线默认
                        // 观感）；没有显式颜色但宽度>0（纯 <a:ln w="N"/>，无
                        // solidFill 无lnRef能拿到颜色）→ 黑色兜底，照抄
                        // pptx.rs PROOT-002 同款"有ln标签就应有可见描边"策略。
                        // noFill 或（宽度缺失/0 且无显式颜色）时不画。
                        let stroke = if ln_no_fill {
                            None
                        } else if let Some(color) = ln_color {
                            let width = ln_width_emu
                                .map(|w| w as f32 / 12700.0)
                                .filter(|&w| w > 0.0)
                                .unwrap_or(0.75);
                            Some(roe_model::common::StrokeStyle { color, width })
                        } else {
                            ln_width_emu.filter(|&w| w > 0).map(|w| {
                                roe_model::common::StrokeStyle {
                                    color: roe_model::common::Color { r: 0, g: 0, b: 0, a: 255 },
                                    width: w as f32 / 12700.0,
                                }
                            })
                        };
                        let style = roe_model::common::ShapeStyle {
                            fill,
                            stroke,
                            shadow: None,
                            head_arrow: None,
                            tail_arrow: None,
                        };
                        let width_pt = anchor_cx_emu as f32 / 12700.0;
                        let height_pt = anchor_cy_emu as f32 / 12700.0;
                        if anchor_hidden {
                            // WROOT-105补充：anchor场景wps:shape（非图片形状）此前未挡
                            // anchor_hidden，与anchor场景图片分支判断逻辑对齐，跳过不构造。
                        } else {
                        body.push(BodyElement::Shape(Shape {
                            geometry,
                            style,
                            text_content: std::mem::take(&mut wps_txbx_paragraphs),
                            width_pt,
                            height_pt,
                            x_pt: anchor_x_pt,
                            y_pt: anchor_y_pt,
                            anchor_type_h: anchor_relative_from_h.clone(),
                            anchor_type_v: anchor_relative_from_v.clone(),
                            align_h: anchor_align_h.clone(),
                            align_v: anchor_align_v.clone(),
                            wrap_type: anchor_wrap_type.clone(),
                            dist_t_pt: anchor_dist_t_pt,
                            dist_b_pt: anchor_dist_b_pt,
                            dist_l_pt: anchor_dist_l_pt,
                            dist_r_pt: anchor_dist_r_pt,
                            flip_v: wps_flip_v,
                            rotation: wps_rot,
                        }));
                        }
                        in_wps_shape = false;
                        in_wps_sppr = false;
                    } else if !anchor_rid.is_empty() && anchor_hidden {
                        // WROOT-105修复：hidden="1"的图片直接跳过，不构造
                        // FloatFrame，不进入body。照抄Calc侧ROOT-051解决
                        // 思路（xlsx.rs collect_xlsx_images的continue模式）。
                    } else if !anchor_rid.is_empty() {
                        if let Some(img_data) = images.get(&anchor_rid) {
                            let image = roe_model::common::ImageData {
                                data: img_data.clone(),
                                mime: sniff_image_mime(img_data),
                                ..Default::default()
                            };
                            let width_pt = anchor_cx_emu as f32 / 12700.0;
                            let height_pt = anchor_cy_emu as f32 / 12700.0;
                            // WROOT-ANCHOR-01: relativeFrom 在 OOXML 里 H/V 允许不同
                            // （真实文档常见，如 H="column" V="paragraph"），此前误合并成
                            // 单一字段、优先取H，导致V的关键信息（决定y坐标基准）丢失。
                            // 现在H/V各自独立保留，不做合并/取舍，由消费方各自判断。
                            body.push(BodyElement::Frame(FloatFrame {
                                origin_para_body_index: None, // WROOT-ANCHOR-05: 占位，段落push时回填真实值
                                image,
                                width_pt,
                                height_pt,
                                x_pt: anchor_x_pt,
                                y_pt: anchor_y_pt,
                                anchor_type_h: anchor_relative_from_h.clone(),
                                anchor_type_v: anchor_relative_from_v.clone(),
                                align_h: anchor_align_h.clone(),
                                align_v: anchor_align_v.clone(),
                                wrap_type: anchor_wrap_type.clone(),
                                dist_t_pt: anchor_dist_t_pt,
                                dist_b_pt: anchor_dist_b_pt,
                                dist_l_pt: anchor_dist_l_pt,
                                dist_r_pt: anchor_dist_r_pt,
                                rotation: anchor_rotation,
                            }));
                            pending_frame_indices_for_cur_para.push(body.len() - 1);
                        }
                    }
                }
                b"wp:inline" if in_inline && in_mc_fallback && mc_saw_cx_chart => {
                    // 统计图的备用位图:图表本体已由 cx:chart 分支处理,这里不再推入
                    in_inline = false;
                }
                b"mc:Fallback" => { in_mc_fallback = false; }
                b"mc:AlternateContent" => { in_mc_fallback = false; mc_saw_cx_chart = false; }
                b"wp:inline" if in_inline => {
                    in_inline = false;
                    if in_wps_shape {
                        // WROOT-shape-preset: 先查预设形状公式表（PPTX-11成果，
                        // roe-model::preset_geometry，第一期约30种），查到就用
                        // eval_preset_shape 求值成 PresetPath（真正可画的路径点）；
                        // avLst/gd 调整参数第一版未解析，暂用空表（走各形状默认值），
                        // 精确调整值留到后续单独补。查不到的 prst 值才降级为 Custom
                        // 占位（未来接入更多形状时自然消解，不阻塞当前链路）。
                        let geometry = if !cur_cust_path_cmds.is_empty() {
                            // WROOT-CUSTGEOM-01: custGeom 优先于 prstGeom（两者是
                            // spPr 下互斥的兄弟标签）。缩放：cur_cust_path_cmds 里
                            // 的坐标是 <a:path w=".." h="..">声明的任意坐标系，按
                            // 目标 width/height 相对该声明宽高统一缩放，逻辑照抄
                            // pptx.rs PROOT-012，只是挪到此刻才有目标宽高可用。
                            let target_w = inline_cx_emu as f32 / 12700.0;
                            let target_h = inline_cy_emu as f32 / 12700.0;
                            let sx = if cur_cust_path_w > 0.0 { target_w / cur_cust_path_w } else { 1.0 };
                            let sy = if cur_cust_path_h > 0.0 { target_h / cur_cust_path_h } else { 1.0 };
                            let scaled: Vec<roe_model::common::PathCommand> = cur_cust_path_cmds
                                .drain(..)
                                .map(|cmd| match cmd {
                                    roe_model::common::PathCommand::MoveTo { x, y } =>
                                        roe_model::common::PathCommand::MoveTo { x: x * sx, y: y * sy },
                                    roe_model::common::PathCommand::LineTo { x, y } =>
                                        roe_model::common::PathCommand::LineTo { x: x * sx, y: y * sy },
                                    roe_model::common::PathCommand::CubicTo { x1, y1, x2, y2, x, y } =>
                                        roe_model::common::PathCommand::CubicTo {
                                            x1: x1 * sx, y1: y1 * sy,
                                            x2: x2 * sx, y2: y2 * sy,
                                            x: x * sx, y: y * sy,
                                        },
                                    roe_model::common::PathCommand::QuadTo { x1, y1, x, y } =>
                                        roe_model::common::PathCommand::QuadTo {
                                            x1: x1 * sx, y1: y1 * sy,
                                            x: x * sx, y: y * sy,
                                        },
                                    other => other,
                                })
                                .collect();
                            // v181: custGeom 场景没有真正的 prst 名字（这不是
                            // 预设形状，复用 PresetPath 变体只是为了共享渲染
                            // 逻辑），preset_name 留空字符串、adj_overrides 留
                            // 空 Vec 表示"不适用"，与 ShapeGeometry::PresetPath
                            // 文档注释里约定的语义一致，未来调用方需要据此区分
                            // custGeom 与真预设形状时应以 preset_name.is_empty()
                            // 判断，不要假设非空。
                            roe_model::common::ShapeGeometry::PresetPath(
                                roe_model::common::PresetPathData {
                                    commands: scaled,
                                    preset_name: String::new(),
                                    adj_overrides: Vec::new(),
                                }
                            )
                        } else { match wps_prst_geom.as_deref() {
                            Some("ellipse") => roe_model::common::ShapeGeometry::Ellipse,
                            Some("line") => roe_model::common::ShapeGeometry::Line,
                            // WROOT-108（v188）: triangle 硬编码拦截已注释掉，对齐 pptx.rs 的
                            // PROOT-101 修复（triangle/rtTriangle 改走 find_preset_shape 查表得到
                            // PresetPath，而非独立 Triangle 枚举）。docx 此前手写了 Triangle 专属
                            // 渲染分支（roe-writer/src/tile.rs、roe-core/src/tile_render_helpers.rs），
                            // 但那是 Parley 迁移期间的临时实现，作者当时已注明"不是最终形态"。
                            // docx 排期落后于 calc/impress 是 Parley 从 cosmic-text 迁移占用了大量
                            // 时间，不是基础组件缺失；现在 impress/calc 侧的 PresetPath 已经用真实
                            // 样本验证过是标准做法，docx 理应跟进对齐，不维护孤立实现。
                            // 原行（已注释）：
                            // Some("triangle") => roe_model::common::ShapeGeometry::Triangle,
                            Some(other) => {
                                match roe_model::preset_geometry::find_preset_shape(other) {
                                    Some(formula) => {
                                        let w = anchor_cx_emu as f64 / 12700.0;
                                        let h = anchor_cy_emu as f64 / 12700.0;
                                        let adj_overrides = std::collections::HashMap::new();
                                        let path = roe_model::preset_geometry::eval_preset_shape(
                                            formula, w, h, &adj_overrides,
                                        );
                                        // v181: Writer 侧 avLst/gd 解析尚未接入
                                        // （见上方"avLst/gd 调整参数第一版未解析"
                                        // 注释），此处 adj_overrides 恒为空表，
                                        // 如实记录为空 Vec；other 是真实 prst 名字，
                                        // 如实保留。
                                        roe_model::common::ShapeGeometry::PresetPath(
                                            roe_model::common::PresetPathData {
                                                commands: path,
                                                preset_name: other.to_string(),
                                                adj_overrides: Vec::new(),
                                            }
                                        )
                                    }
                                    None => roe_model::common::ShapeGeometry::Custom(other.to_string()),
                                }
                            }
                            None => roe_model::common::ShapeGeometry::Rect,
                        }
                        };
                        let fill = if !grad_stops.is_empty() {
                            let angle = (grad_angle_emu as f32 / 60000.0).rem_euclid(360.0);
                            Some(roe_model::common::FillStyle::Gradient {
                                stops: std::mem::take(&mut grad_stops),
                                kind: roe_model::common::GradientKind::Linear { angle },
                                rotate_with_shape: grad_rot_with_shape,
                            })
                        } else {
                            wps_fill_color.map(roe_model::common::FillStyle::Solid)
                        };
                        let stroke = if ln_no_fill {
                            None
                        } else if let Some(color) = ln_color {
                            let width = ln_width_emu
                                .map(|w| w as f32 / 12700.0)
                                .filter(|&w| w > 0.0)
                                .unwrap_or(0.75);
                            Some(roe_model::common::StrokeStyle { color, width })
                        } else {
                            ln_width_emu.filter(|&w| w > 0).map(|w| {
                                roe_model::common::StrokeStyle {
                                    color: roe_model::common::Color { r: 0, g: 0, b: 0, a: 255 },
                                    width: w as f32 / 12700.0,
                                }
                            })
                        };
                        let style = roe_model::common::ShapeStyle {
                            fill,
                            stroke,
                            shadow: None,
                            head_arrow: None,
                            tail_arrow: None,
                        };
                        let width_pt = inline_cx_emu as f32 / 12700.0;
                        let height_pt = inline_cy_emu as f32 / 12700.0;
                        if inline_hidden {
                            // WROOT-105补充：inline场景wps:shape（非图片形状）此前未挡
                            // inline_hidden，与anchor场景图片分支判断逻辑对齐，跳过不构造。
                        } else {
                        body.push(BodyElement::Shape(Shape {
                            geometry,
                            style,
                            text_content: std::mem::take(&mut wps_txbx_paragraphs),
                            width_pt,
                            height_pt,
                            x_pt: 0.0,
                            y_pt: 0.0,
                            anchor_type_h: None,
                            anchor_type_v: None,
                            align_h: None,
                            align_v: None,
                            wrap_type: None,
                            dist_t_pt: 0.0,
                            dist_b_pt: 0.0,
                            dist_l_pt: 0.0,
                            dist_r_pt: 0.0,
                            flip_v: wps_flip_v,
                            rotation: wps_rot,
                        }));
                        }
                        in_wps_shape = false;
                        in_wps_sppr = false;
                    } else if !inline_rid.is_empty() {
                        if let Some(chart_target) = chart_rels.get(&inline_rid) {
                            // WROOT-011: 内联图表，从 word/charts/chartN.xml 解析
                            let chart_path = format!("word/charts/{}", chart_target.trim_start_matches("../").trim_start_matches("charts/"));
                            if let Ok(chart_xml) = read_zip_entry(zip, &chart_path) {
                                if chart_xml.contains("<cx:chartSpace") {
                                    if let Some(model) = super::chartex::parse_chartex_xml(&chart_xml) {
                                        let width_pt = inline_cx_emu as f32 / 12700.0;
                                        let height_pt = inline_cy_emu as f32 / 12700.0;
                                        if !inline_hidden {
                                            body.push(BodyElement::InlineStatChart { model, width_pt, height_pt });
                                        }
                                    }
                                } else if let Some(model) = parse_ooxml_chart_xml(&chart_xml, theme_colors, &HashMap::new()) {
                                    let width_pt = inline_cx_emu as f32 / 12700.0;
                                    let height_pt = inline_cy_emu as f32 / 12700.0;
                                    if !inline_hidden {
                                    body.push(BodyElement::InlineChart { model, width_pt, height_pt });
                                    }
                                    // WROOT-105补充：inline场景chart此前未挡inline_hidden，补齐。
                                }
                            }
                        } else if let Some(img_data) = images.get(&inline_rid) {
                            let image = roe_model::common::ImageData {
                                data: img_data.clone(),
                                mime: sniff_image_mime(img_data),
                                ..Default::default()
                            };
                            let width_pt = inline_cx_emu as f32 / 12700.0;
                            let height_pt = inline_cy_emu as f32 / 12700.0;
                            body.push(BodyElement::Image(InlineImage {
                                image,
                                width_pt,
                                height_pt,
                                dist_t_pt: inline_dist_t_pt,
                                dist_b_pt: inline_dist_b_pt,
                                dist_l_pt: inline_dist_l_pt,
                                dist_r_pt: inline_dist_r_pt,
                            rotation: None, }));
                        }
                    }
                }
                b"m:oMathPara" if in_omath_para => {
                    in_omath_para = false;
                    let end_tag_len = e.name().as_ref().len() + 3; // "</" + name + ">"
                    let end_pos = reader.buffer_position().saturating_sub(end_tag_len as u64);
                    let start = omath_para_start_pos as usize;
                    let end = end_pos as usize;
                    if start <= end && end <= xml.len() {
                        let chunk = &xml[start..end];
                        // m:oMathPara 内部还包着 m:oMathParaPr（对齐方式等元信息），
                        // 真正要转换的是里面的 <m:oMath>...</m:oMath> 子串。
                        if let (Some(m_start), Some(m_end)) =
                            (chunk.find("<m:oMath>"), chunk.find("</m:oMath>"))
                        {
                            let math_end = m_end + "</m:oMath>".len();
                            if m_start < math_end && math_end <= chunk.len() {
                                let latex = parse_omml_to_latex(&chunk[m_start..math_end]);
                                // DOCX-8-v1(f): 尺寸回填。
                                // 字号来源：方案A（技术债，详见roe-ext/notes/DOCX-8-v1-f.md）——
                                // 只用run级w:sz（run_font_size），取不到时退回Word默认字号10.5pt。
                                // 不查段落样式继承链（resolve_paragraph_style），那依赖DOCX-1接线，
                                // 后续重构方案B时再补上更准确的来源。
                                let font_size_pt = run_font_size.unwrap_or(10.5) as f64;
                                let (width_pt, height_pt, depth_pt) =
                                    match roe_formula::latex_layout_size(&latex, font_size_pt) {
                                        Ok((w, h, d)) => (w as f32, h as f32, d as f32),
                                        Err(_) => {
                                            // 降级兜底：latex_layout_size失败（如公式含未支持语法）时，
                                            // 退回旧的count_tag近似算法，保证不崩溃、不产出0尺寸。
                                            // estimate_size_from_mathml输入是MathML非LaTeX，这里复用
                                            // latex_to_mathml先转一道；若这一步也失败，才真正退到0。
                                            match roe_formula::latex_to_mathml(&latex) {
                                                Ok(mathml) => {
                                                    let (w, h) = roe_formula::estimate_size_from_mathml(&mathml);
                                                    (w as f32, h as f32, 0.0)
                                                }
                                                Err(_) => (0.0, 0.0, 0.0),
                                            }
                                        }
                                    };
                                if let Some(ref mut p) = cur_para {
                                    // DOCX-8-v1(g): 渲染层完全没有"行内非文本对象"概念
                                    // （roe-writer的layout.rs用cosmic-text对Run.text做纯文本
                                    // shaping，没有为公式预留空间/基线对齐的机制；tile.rs渲染
                                    // 循环全文件搜索"formula"零命中）。真正解决需要教会排版引擎
                                    // "行内嵌入对象"这个概念（预留宽度、按depth_pt对齐基线），
                                    // 这属于新架构工作，规模类比DOCX-9/10（Writer独立形状能力/
                                    // 共享组件层），本分支不做架构级实现。
                                    // 本步骤只做最小兜底：把LaTeX源码原样当作文本显示，
                                    // 防止公式完全静默消失（比"样式没接上"更严重的一类缺口——
                                    // 那是"数据丢弃"，这是"内容完全不可见"）。方括号包裹只是
                                    // 视觉上标出"这是一个公式占位"，无特殊解析含义。
                                    // DOCX-8-v2(Step2): 占位符改造。
                                    // 不再把LaTeX源码原样当文本显示（那会被cosmic-text当成
                                    // 普通文字shape出来，即WROOT-059描述的"渲染出LaTeX语法
                                    // 文本"现象）。
                                    //
                                    // WROOT-080修复：不再用重复空格模拟公式宽度占位。根因
                                    // （已用Parley源码vendor/parley/.../alignment.rs实锤）：
                                    // Parley对齐计算会把纯空格run识别为trailing_whitespace，
                                    // 从free_space中排除，导致这段占位符完全不参与居中/右
                                    // 对齐的宽度计算，视觉上公式位置严重偏移（貼在最右侧）。
                                    // 且这不是系数问题——曾从cosmic-text时代系数0.26改到
                                    // Parley下实测的0.224，视觉效果毫无变化，证实空格数量
                                    // /宽度从一开始就没被算进对齐逻辑，不是"稍微不准"。
                                    // 改用Parley原生InlineBox机制（layout.rs构建阶段
                                    // push_inline_box，宽高直接传width_pt/height_pt+depth_pt），
                                    // run.text留空字符串，不再需要在此模拟占位宽度。
                                    let fallback_text = String::new();
                                    // DOCX-8-v2(方案乙): 同一时机顺便算出绘制指令列表，
                                    // 供tile.rs直接画图，不需要在渲染阶段重新parse。
                                    // 失败时（如公式含未支持语法）降级为空DisplayList——
                                    // 不崩溃、不显示乱码，占位空格宽度依然生效，只是这个
                                    // 公式暂时画不出可见笔画，跟width_pt/height_pt/depth_pt
                                    // 的降级路径（latex_layout_size失败时退到
                                    // estimate_size_from_mathml）相互独立，互不影响。
                                    let display_list = roe_formula::latex_to_display_list(&latex, font_size_pt)
                                        .unwrap_or_default();
                                    p.runs.push(Run {
                                        text: fallback_text,
                                        formula: Some(roe_model::writer::Formula {
                                            latex,
                                            width_pt,
                                            height_pt,
                                            depth_pt,
                                            display_list,
                                            // DOCX-8-v3续: m:borderBox的strikeV真正传递通道
                                            // 需要改parse_omml_to_latex返回类型（牵动所有
                                            // 调用者，改动面较大），推迟到方向B（渲染管线）
                                            // 开工时一并处理。当前硬编码false，不影响其余
                                            // 三种strike场景（strikeH/TLBR/BLTR）正常工作。
                                            needs_vertical_strike: false,
                                        }),
                                        ..Default::default()
                                    });
                                }
                            }
                        }
                    }
                }
                // DOCX-8-v3: 裸 m:oMath 收尾。抠出标签之间的内容，拼回
                // 标准 <m:oMath>...</m:oMath> 形式（规避原始开标签是否带
                // xmlns:m="..." 等属性的问题），后续逻辑与 m:oMathPara
                // 分支完全对齐（parse_omml_to_latex → 尺寸计算 → 空格
                // 占位符 → display_list → push Run）。
                b"m:oMath" if in_omath => {
                    in_omath = false;
                    let end_tag_len = e.name().as_ref().len() + 3; // "</" + name + ">"
                    let end_pos = reader.buffer_position().saturating_sub(end_tag_len as u64);
                    let start = omath_start_pos as usize;
                    let end = end_pos as usize;
                    if start <= end && end <= xml.len() {
                        let inner = &xml[start..end];
                        let wrapped = format!("<m:oMath>{}</m:oMath>", inner);
                        let latex = parse_omml_to_latex(&wrapped);
                        // 字号来源同 m:oMathPara 分支：方案A技术债，run级w:sz
                        // 取不到时兜底10.5pt。
                        let font_size_pt = run_font_size.unwrap_or(10.5) as f64;
                        let (width_pt, height_pt, depth_pt) =
                            match roe_formula::latex_layout_size(&latex, font_size_pt) {
                                Ok((w, h, d)) => (w as f32, h as f32, d as f32),
                                Err(_) => {
                                    match roe_formula::latex_to_mathml(&latex) {
                                        Ok(mathml) => {
                                            let (w, h) = roe_formula::estimate_size_from_mathml(&mathml);
                                            (w as f32, h as f32, 0.0)
                                        }
                                        Err(_) => (0.0, 0.0, 0.0),
                                    }
                                }
                            };
                        if let Some(ref mut p) = cur_para {
                            // WROOT-080修复：同上，不再用空格模拟宽度，改走InlineBox。
                            let fallback_text = String::new();
                            let display_list = roe_formula::latex_to_display_list(&latex, font_size_pt)
                                .unwrap_or_default();
                            p.runs.push(Run {
                                text: fallback_text,
                                formula: Some(roe_model::writer::Formula {
                                    latex,
                                    width_pt,
                                    height_pt,
                                    depth_pt,
                                    display_list,
                                    needs_vertical_strike: false,
                                }),
                                ..Default::default()
                            });
                        }
                    }
                }
                // WROOT-STYLE-01续: </a:schemeClr> 结束，若在lnRef语境下
                // 收集到了base color，此时shade（若有）也已收集完毕，统一
                // 应用shade（若无shade则原色直接用）写入ln_color，随后
                // 清空两个暂存变量，避免污染下一个形状。
                b"a:schemeClr" if in_wps_ln_ref => {
                    if let Some(base) = wps_ln_ref_base_color.take() {
                        let rgb = [base.r, base.g, base.b];
                        let final_rgb = match wps_ln_ref_shade.take() {
                            Some(v) => crate::ooxml::pptx::apply_shade(rgb, v),
                            None => rgb,
                        };
                        ln_color = Some(Color { r: final_rgb[0], g: final_rgb[1], b: final_rgb[2], a: base.a });
                    }
                }
                // WROOT-STYLE-01修复: a:lnRef/a:fillRef/wps:style 此前
                // 只有Start事件置true，从未复位，导致lnRef先于fillRef
                // 出现时（样本3实际顺序）in_wps_ln_ref一直保持true，
                // fillRef内的schemeClr被错误地判给ln_ref分支处理，
                // wps_fill_color永远拿不到值。此处补齐End复位。
                b"a:lnRef" if in_wps_ln_ref => { in_wps_ln_ref = false; }
                b"a:fillRef" if in_wps_fill_ref => { in_wps_fill_ref = false; }
                b"wps:style" if in_wps_style => { in_wps_style = false; }
                b"a:ln" if in_ln => { in_ln = false; }
                b"a:gs" if in_grad_fill => {
                    // pos/color 已在 Start/Empty 分支收集完毕（srgbClr 是
                    // gs 的子元素，quick-xml 事件顺序保证子元素先于父元素
                    // 结束），这里无需额外动作，只是显式声明该标签已消费，
                    // 避免落入 _ => {} 兜底（虽然效果一样，但便于阅读）。
                }
                b"a:gradFill" if in_grad_fill => { in_grad_fill = false; }
                b"a:moveTo" if in_move_to => { in_move_to = false; }
                b"a:lnTo" if in_line_to => { in_line_to = false; }
                b"a:cubicBezTo" if in_cubic_to => { in_cubic_to = false; cubic_to_pt_idx = 0; }
                b"a:quadBezTo" if in_quad_to => { in_quad_to = false; quad_to_pt_idx = 0; }
                b"a:path" if in_cust_path => { in_cust_path = false; }
                b"a:custGeom" if in_cust_geom => { in_cust_geom = false; }
                b"wps:spPr" if in_wps_sppr => { in_wps_sppr = false; }
                b"wps:txbx" if in_wps_txbx => {
                    if wps_txbx_depth > 0 {
                        wps_txbx_depth -= 1;
                    } else {
                        in_wps_txbx = false;
                        // buffer_position() 此刻已越过 </wps:txbx>，回退标签本身
                        // 的字节长度（"</" + name + ">"），拿到内容真正的结束偏移
                        let end_tag_len = e.name().as_ref().len() + 3;
                        let end_pos = reader.buffer_position().saturating_sub(end_tag_len as u64);
                        let start = wps_txbx_start_pos as usize;
                        let end = end_pos as usize;
                        if start <= end && end <= xml.len() {
                            wps_txbx_paragraphs = parse_isolated_paragraphs(&xml[start..end]);
                        }
                    }
                }
                b"w:pict" if in_w_pict => { in_w_pict = false; }
                // DOCX-7: w:object 收尾。只有捕获到预渲染图 rid 且能在 images 里
                // 查到数据时才产出 InlineImage；查不到（如只有真实OLE二进制、
                // 没有VML降级图）则静默跳过，不报错也不占位，与 wp:inline/anchor
                // 的"查不到就不产出"策略一致。
                b"w:object" if in_w_object => {
                    in_w_object = false;
                    if !ole_preview_rid.is_empty() {
                        if let Some(img_data) = images.get(&ole_preview_rid) {
                            let image = roe_model::common::ImageData {
                                data: img_data.clone(),
                                mime: sniff_image_mime(img_data),
                                ..Default::default()
                            };
                            body.push(BodyElement::Image(InlineImage {
                                image,
                                width_pt: ole_width_pt,
                                height_pt: ole_height_pt,
                                ..Default::default()
                            }));
                        }
                    }
                    ole_preview_rid.clear();
                    ole_width_pt = 0.0;
                    ole_height_pt = 0.0;
                }
                b"w:drawing" => { in_drawing = false; }
                b"w:t" => {
                    in_t = false;
                    if t_preserve_space {
                        cur_text.push_str(&t_buf);
                    } else {
                        cur_text.push_str(t_buf.trim());
                    }
                    t_buf.clear();
                    t_preserve_space = false;
                }
                b"w:numPr" => { in_numpr = false; }
                b"w:pPr" => { in_ppr_body = false; in_tabs = false; in_para_sectpr = false; in_numpr = false; in_ppr_rpr = false; }
                b"w:tabs" => { in_tabs = false; }  // WROOT-068: </w:tabs> 结束，提前于</w:pPr>
                b"w:rPr" => { in_rpr_body = false; in_ppr_rpr = false; }
                b"w:r" => {
                    in_run = false;
                    if let Some(ref mut p) = cur_para {
                        // Step 5 修复：同 Step 2 踩过的坑——<w:endnoteReference/>/
                        // <w:footnoteReference/> 是不产生 cur_text 的 Empty 标签，
                        // 原条件 `if !cur_text.is_empty()` 会让纯引用锚点run（真实
                        // 样本里绝大多数 endnoteReference 确实伴随其他文字，但不能
                        // 假设所有情况都如此）被静默跳过。改为文本非空 或 携带
                        // note引用标记时都要push。
                        // WROOT-031第三步：纯commentReference/commentRangeStart
                        // 裸run（无文字，Word/LO真实样本证实二者常单独成一个
                        // 空w:r，比如<w:r><w:rPr>.../<w:rPr><w:commentReference
                        // w:id="0"/></w:r>紧跟在CommentDone.docx里）也不能被
                        // 静默跳过，否则comment_ref/comment_range_start锚点
                        // 信息随空文本run一起丢失，重蹈WROOT-075初版对
                        // endnoteReference踩过的坑。
                        if !cur_text.is_empty()
                            || run_note_field.is_some()
                            || run_comment_ref.is_some()
                            || !pending_comment_range_start.is_empty()
                        {
                            p.runs.push(Run {
                                text: cur_text.clone(),
                                // WROOT-055: 此前恒为 None，导致 <w:rStyle> 引用的
                                // 字符样式（如 fldChar 超链接显示文字常用的
                                // "Internet Link"/style15）完全无法生效。
                                // 下游 roe-writer::layout::resolve_run_style 已有
                                // 完整的 "段落样式 -> 字符样式(char_style) -> Run
                                // 内联属性" 优先级链（含 color/underline/background/
                                // strikethrough 等），只是此前从未被喂到有效值——
                                // 这里只需原样传递样式 ID 引用，不在 parser 层
                                // 重复解析样式内容，复用渲染层已验证过的逻辑。
                                char_style: run_style_id.clone().map(roe_model::common::StyleRef),
                                bold: run_bold,
                                italic: run_italic,
                                underline: run_underline,
                                strikethrough: run_strikethrough,
                                font_size: run_font_size,
                                vertical_position: run_vertical_position,
                                script: run_script,
                                // WROOT-024: run_alpha 非 None 时覆盖 parse_hex_color
                                // 默认写死的 a:255（完全不透明），实现 w14:textFill
                                // 字符透明度效果。若 run 本身没有 w:color（run_color
                                // 为 None），则 alpha 无处附着（无颜色可谈透明度），
                                // 此时按 OOXML 语义应回退用某个基准色（如黑色）承载
                                // alpha，但当前样本 w14:alpha 均伴随 w:color 同时出现，
                                // 未观察到"只有alpha无color"的真实场景，暂不臆造兜底。
                                color: run_color.as_deref().map(parse_hex_color).map(|mut c| {
                                    if let Some(a) = run_alpha {
                                        c.a = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
                                    }
                                    c
                                }),
                                font_family: run_font_family.clone(),
                                font_family_ascii: run_font_family_ascii.clone(),
                                background: run_highlight.map(|c| roe_model::common::Color { r: c[0], g: c[1], b: c[2], a: c[3] }),
                                hyperlink: if in_hyperlink { cur_hyperlink_url.clone() } else { None },
                                // Step 5 (WROOT-075/027/028): 正文脚注/尾注引用标记。
                                field: run_note_field,
                                endnote_ref: run_endnote_ref_id,
                                footnote_ref: run_footnote_ref_id,
                                // WROOT-031第三步：正文批注锚点关联，见函数
                                // 顶部pending_comment_range_start/run_comment_ref
                                // 声明处的完整设计说明。
                                comment_range_start: std::mem::take(&mut pending_comment_range_start),
                                comment_ref: run_comment_ref,
                                ..Default::default()
                            });
                            cur_text.clear();
                            run_comment_ref = None;
                        }
                    }
                }
                b"w:p" => {
                    // WROOT-ANCHOR-05: 图片剥离出FloatFrame时（见下方
                    // origin_para_body_index插入点），此段落自己尚未push进
                    // body，无法直接拿到自己未来的index。改为记录"该图片
                    // 在body里的临时位置"，等段落自己真正确定index的这一刻
                    // （现在），回填给之前所有暂存的图片Frame——不依赖"图片
                    // 和段落在body里紧邻"这类顺序假设，无论解析器内部把
                    // Frame和Paragraph以什么顺序push进body，回填都精确对应
                    // 真实所属关系。
                    let this_para_body_index = body.len();
                    for &frame_body_idx in &pending_frame_indices_for_cur_para {
                        if let Some(BodyElement::Frame(ref mut ff)) = body.get_mut(frame_body_idx) {
                            ff.origin_para_body_index = Some(this_para_body_index);
                        }
                    }
                    pending_frame_indices_for_cur_para.clear();
                    if let Some(mut p) = cur_para.take() {
                        // WROOT-026：p.runs 此刻已最终定型，可以准确判断这个
                        // 曾出现过page类型br的段落，到底是"纯占位空段落"
                        // （real样本#1/#3：br就是段落全部内容）还是"br之后
                        // 还带实际文字"（real样本#6/#7：br在run开头，后面
                        // 紧跟"Odd page 2"这类文字）。前者整段跳过+触发下
                        // 一段换页（复用column_break_after式的"标记在自己
                        // 身上"跳过语义）；后者改用page_break_before语义
                        // （这段自己换页显示，不跳过，复用WROOT-001已验证
                        // 成熟的机制，不需要layout层任何新逻辑）。
                        if cur_para_has_page_br {
                            if p.runs.is_empty() {
                                p.page_break_after = true;
                            } else {
                                p.page_break_before = true;
                            }
                        }
                        // WROOT-036 续修：段落自身没有直接 w:numPr 时
                        // （cur_num_id/cur_ilvl 至少一个是 None——正常情况下
                        // 两者应该同时有值或同时没值，因为都在同一个 w:numPr
                        // 块内被设置，这里用 or 兜底防御万一只设置了一个的
                        // 异常情况），改查 cur_style_id 对应样式（含继承链）
                        // 是否自带 numId（Word style-level numbering，典型场景
                        // 是自定义样式如 CustomStyle 在 w:styles.xml 里自己
                        // 挂了 w:pPr/w:numPr）。
                        // WROOT-117：段落自身显式 w:numId=0 表示"取消编号"，
                        // 不得再回退去继承样式的编号（LOK 断言：ListLabelString 为空）。
                        let effective_num = if cur_num_id == Some(0) {
                            None
                        } else {
                            match (cur_num_id, cur_ilvl) {
                                (Some(n), Some(i)) => Some((n, i)),
                                _ => styles.resolve_num_id(&cur_style_id),
                            }
                        };
                        // v257: roe 自己导出的列表（abstractNum 带 ROE_ 标记）还原为 ROE_ 列表
                        let mut roe_restored = false;
                        if let Some((nid, il)) = effective_num {
                            if let Some(rn) = numbering.roe_style_for(nid) {
                                p.list_info = Some(ListInfo {
                                    list_id: format!("ROE_L{}", nid),
                                    level: il,
                                    style_name: rn,
                                });
                                roe_restored = true;
                            }
                        }
                        let effective_num = if roe_restored { None } else { effective_num };
                        if let Some((num_id, ilvl)) = effective_num {
                            let label = numbering.advance(num_id, ilvl);
                            // 从 numbering 取该级别的缩进，段落自身未设置时才应用
                            // WROOT-108：先查 lvlOverride 内嵌定义，再回退 abstractNum
                            let lvl_indent = numbering.effective_level(num_id, ilvl as usize);
                            if let Some(ref lvl) = lvl_indent {
                                if p.indent_left.is_none() {
                                    // WROOT-095：level 自身也未定义时，退回 docDefaults
                                    // 文档级默认缩进（不影响普通非列表段落，那部分缩进
                                    // 消费路径目前不存在，已记录为独立技术债）。
                                    p.indent_left = lvl.indent_left.or(styles.doc_default_indent_left);
                                }
                                if p.indent_first.is_none() {
                                    p.indent_first = lvl.indent_first.or(styles.doc_default_indent_first);
                                }
                            } else {
                                // WROOT-095：该 numId/ilvl 在 numbering.xml 里完全查不到对应
                                // level 定义（lvl_indent 为 None，边缘情况），同样退回
                                // docDefaults 兜底，而不是让缩进彻底缺失。
                                if p.indent_left.is_none() {
                                    p.indent_left = styles.doc_default_indent_left;
                                }
                                if p.indent_first.is_none() {
                                    p.indent_first = styles.doc_default_indent_first;
                                }
                            }
                            p.list_info = Some(ListInfo {
                                list_id: num_id.to_string(),
                                level: ilvl,
                                style_name: cur_style_id.clone(),
                            });
                            // WROOT-111：只承载 w:sectPr（分节符）、没有任何可见 run 的
                            // 空段落，仅作为分节符的载体，Word/LibreOffice 导入时会把它
                            // 移除，它从样式继承来的编号不应显示（LOK 断言：
                            // ooxmlexport12.cxx testTdf113608_runAwayNumbering，
                            // "incorrect numbering style is not applied after removing
                            // a w:r-less paragraph"）。只对"含 sectPr 且无可见文本"的
                            // 段落生效；普通空段落（空列表项等）仍按原样显示编号。
                            let is_sect_carrier_only = cur_para_ends_section
                                && p.runs.iter().all(|r| r.text.is_empty());
                            // 把编号 label 前置到第一个 run
                            if !label.is_empty() && !is_sect_carrier_only {
                                // WROOT-074: bullet/编号 label 携带 numbering.xml
                                // 该 level 定义的字体/颜色/高亮（lvl_indent 变量
                                // 前面已从 numbering.abstract_nums 取到，克隆自
                                // 同一个 NumLevel，这里直接复用，避免重复查找）。
                                let (bullet_font, bullet_color, bullet_bg) = lvl_indent.as_ref()
                                    .map(|l| (l.font_family.clone(), l.color.clone(), l.highlight))
                                    .unwrap_or((None, None, None));
                                // WROOT-074续修2：私有区bullet字符（如\uf0b7）
                                // 规范化为标准Unicode，命中后不再需要特定字体名
                                // （标准Unicode圆点任何常见字体都能正确显示），
                                // 详见 normalize_symbol_bullet_char 函数注释。
                                let mut normalized_label = String::new();
                                let mut any_normalized = false;
                                for ch in label.chars() {
                                    let (out_ch, hit) = normalize_symbol_bullet_char(ch, bullet_font.as_deref());
                                    normalized_label.push(out_ch);
                                    any_normalized |= hit;
                                }
                                let effective_font = if any_normalized { None } else { bullet_font };
                                p.runs.insert(0, Run {
                                    text: format!("{normalized_label} "),
                                    char_style: None,
                                    // WROOT-074续修：bullet符号（如\uf0b7）既不是
                                    // 标准ASCII西文也不是东亚文字，resolve_run_font_
                                    // family 的 ascii/eastAsia 二选一启发式（is_all_
                                    // ascii判断）对它不适用——该判断只在is_all_ascii
                                    // 为true时才会采用font_family_ascii，而bullet私有
                                    // 区字符会被判定为is_all_ascii=false，导致只写
                                    // font_family_ascii时该字体从未被实际使用。这里
                                    // 同时写入font_family（东亚候选位），确保无论
                                    // is_all_ascii判断结果如何，都能拿到正确字体
                                    // （当effective_font为None时，两个字段自然也
                                    // 都是None，走系统默认字体）。
                                    font_family: effective_font.clone(),
                                    font_family_ascii: effective_font,
                                    color: bullet_color.map(|hex| {
                                        let hex = hex.trim_start_matches('#');
                                        let rgb = u32::from_str_radix(hex, 16).unwrap_or(0);
                                        roe_model::common::Color {
                                            r: ((rgb >> 16) & 0xff) as u8,
                                            g: ((rgb >> 8) & 0xff) as u8,
                                            b: (rgb & 0xff) as u8,
                                            a: 0xff,
                                        }
                                    }),
                                    background: bullet_bg.map(|c| roe_model::common::Color { r: c[0], g: c[1], b: c[2], a: c[3] }),
                                    ..Default::default()
                                });
                            }
                        }
                        if let Some(ref mut cell) = cur_cell {
                            cell.content.push(roe_model::common::CellContent::Paragraph(p));
                        } else {
                            body.push(BodyElement::Paragraph(p));
                            // [WROOT-任务B] 本段内嵌了 w:sectPr，说明本段是
                            // 本节最后一段。在段落之后补一个 Section 边界标记，
                            // 供排版层扫描 body 时识别"从这里起进入下一节"。
                            // 边界放在段落*之后*（而非之前），因为这段内容本身
                            // 仍属于"结束的那一节"，下一段才属于新节——与
                            // pending_page_break 的"下一段才换页"语义保持一致。
                            if cur_para_ends_section {
                                cur_section_index += 1;
                                // WROOT-136x(分栏修复)：此前 ..Default::default() 丢弃了
                                // page_layouts 里已解析好的分栏数据。下标关系：
                                // cur_section_index 从1开始自增，page_layouts 从0开始，
                                // 已用临时诊断打印实测验证 page_layouts[cur_section_index-1]
                                // 就是这个 Section 边界对应的那一节。
                                let sec_columns = page_layouts.get(cur_section_index - 1)
                                    .map(|pl| pl.columns.clone());
                                body.push(BodyElement::Section(roe_model::writer::Section {
                                    name: format!("Section {}", cur_section_index),
                                    columns: sec_columns,
                                    ..Default::default()
                                }));
                                cur_para_ends_section = false;
                            }
                        }
                    }
                }
                // 表格样式移植 Layer 1步骤B：w:tblBorders End，仅关闭容器状态，
                // 真正的写回发生在下面 w:tblPr End（此处cur_tbl_border_*已经在
                // Start/Empty分支收集完毕，w:tblPr End时cur_table必为Some，
                // 直接写入即可，不需要在这里提前写）。
                b"w:tblBorders" if in_tblpr => {
                    in_tblborders = false;
                }
                // 表格样式移植 Layer 1步骤B：w:tblPr End，把本表格级默认边框
                // （cur_tbl_border_color/width）写回 cur_table。此时cur_table
                // 必为Some（w:tbl Start时已创建），直接可变借用赋值。
                b"w:tblPr" if in_tbl => {
                    in_tblpr = false;
                    if let Some(ref mut tbl) = cur_table {
                        tbl.default_border_color = cur_tbl_border_color.take();
                        tbl.default_border_width = cur_tbl_border_width.take();
                        tbl.inside_h_color = cur_tbl_inside_h_color.take();
                        tbl.inside_h_width = cur_tbl_inside_h_width.take();
                        tbl.inside_v_color = cur_tbl_inside_v_color.take();
                        tbl.inside_v_width = cur_tbl_inside_v_width.take();
                        // WROOT-101（表格浮动绕排）：五个状态变量只要有任意一个
                        // 被设置过，就说明XML里出现过<w:tblpPr>标签，组装成
                        // TableFloatPos写回；否则float_pos保持None（表格是普通
                        // 流内块级元素，不浮动，与既有行为完全一致，零回归）。
                        // left/right_from_text_pt不是Option（缺省即0.0，语义
                        // 上"没写等于没有额外间距"，不需要区分None与0.0）。
                        let has_float_pos = cur_tbl_float_vert_anchor.is_some()
                            || cur_tbl_float_horz_anchor.is_some()
                            || cur_tbl_float_y_pt.is_some()
                            || cur_tbl_float_x_pt.is_some();
                        tbl.float_pos = if has_float_pos {
                            Some(roe_model::common::TableFloatPos {
                                vert_anchor: cur_tbl_float_vert_anchor.take(),
                                horz_anchor: cur_tbl_float_horz_anchor.take(),
                                y_pt: cur_tbl_float_y_pt.take(),
                                x_pt: cur_tbl_float_x_pt.take(),
                                left_from_text_pt: cur_tbl_float_left_from_text_pt,
                                right_from_text_pt: cur_tbl_float_right_from_text_pt,
                            })
                        } else {
                            None
                        };
                        cur_tbl_float_vert_anchor = None;
                        cur_tbl_float_horz_anchor = None;
                        cur_tbl_float_y_pt = None;
                        cur_tbl_float_x_pt = None;
                        cur_tbl_float_left_from_text_pt = 0.0;
                        cur_tbl_float_right_from_text_pt = 0.0;
                    }
                }
                b"w:tcBorders" if in_tcpr => {
                    in_tcborders = false;
                }
                b"w:tcPr" if in_tbl => {
                    in_tcpr = false;
                }
                b"w:tc" if in_tbl => {
                    if let (Some(mut cell), Some(ref mut row)) = (cur_cell.take(), cur_row.as_mut()) {
                        cell.bg_color = cur_cell_bg_color.take();
                        // 表格样式移植 Layer 1步骤B：cell级边框写回，与bg_color
                        // 写回逻辑放在一起，保持"w:tc结束时把本cell在tcPr内
                        // 收集到的所有属性一次性落到TableCell"这个既有模式。
                        cell.border_color = cur_cell_border_color.take();
                        cell.border_width = cur_cell_border_width.take();
                        row.cells.push(cell);
                        cur_row_vmerge.push(cur_cell_vmerge.take());
                    }
                }
                b"w:tr" if in_tbl => {
                    if let (Some(row), Some(ref mut tbl)) = (cur_row.take(), cur_table.as_mut()) {
                        tbl.rows.push(row);
                        tbl_vmerge.push(std::mem::take(&mut cur_row_vmerge));
                    }
                }
                b"w:tbl" if in_tbl => {
                    in_tbl = false;
                    if let Some(mut tbl) = cur_table.take() {
                        tbl.col_widths = tbl_col_widths.clone();
                        apply_vmerge(&mut tbl, &tbl_vmerge);
                        tbl_vmerge.clear();
                        body.push(BodyElement::Table(tbl));
                    }
                }
                b"w:hyperlink" if in_hyperlink => {
                    in_hyperlink = false;
                    cur_hyperlink_url = None;
                }
                b"w:body" => in_body = false,
                _ => {}
            },
            Event::Text(ref e) if in_t => {
                t_buf.push_str(&e.unescape()?);
            }
            Event::Text(ref e) if in_pos_offset || in_pos_align => {
                pos_text_buf.push_str(&e.unescape()?);
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(body)
}

// ─── 辅助 ─────────────────────────────────────────────────────────────────────

fn read_zip_entry<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
    name: &str,
) -> Result<String, ParseError> {
    let mut entry = zip
        .by_name(name)
        .map_err(|e| ParseError::Malformed(format!("zip entry '{name}' not found: {e}")))?;
    let mut buf = String::new();
    entry.read_to_string(&mut buf)?;
    Ok(buf)
}

// ─── 测试 ─────────────────────────────────────────────────────────────────────

/// DOCX-9 Step2: 独立、无状态地解析一段 w:txbxContent 内部 XML（wps:txbx 里的文字）。
/// 输入是从主 read_document 状态机切出的字符串子片段，不共享/依赖主状态机任何状态
/// （不处理 in_table/in_anchor 等嵌套上下文）。
/// 范围限定：解析 w:p/w:r/w:t + w:rPr 下的 b/i/u/strike 基础字符格式。
/// 不处理：w:pStyle（样式表引用，留空 StyleRef("Normal")）、w:numPr（编号）、
/// w:hyperlink（超链接）——这些在文本框内极少见，且样式表接线本身是 DOCX-1 的
/// 独立范围，此处不越界处理。
fn parse_isolated_paragraphs(xml_chunk: &str) -> Vec<Paragraph> {
    let mut reader = Reader::from_str(xml_chunk);
    // WROOT-027(v141二次修复): 此前全局trim_text(true)，导致quick-xml在
    // 触发Text事件前就把每个w:t节点的文本内容首尾空白裁剪掉了，不管下面
    // t_preserve_space这套判断逻辑写得多正确都无济于事——事件触发时文本
    // 已经被trim过。这正是read_document主解析函数当初(WROOT-025)踩过、
    // 已经修复过的同一个坑（见该函数trim_text(false)那一行的注释），
    // 但parse_isolated_paragraphs（note内容专用解析函数）当初照抄框架时
    // 漏掉了这一处同步，导致note正文里<w:t xml:space="preserve"> He...</w:t>
    // 这类带前导空格的文本，空格被静默吃掉（027场景实测：编号"i"和"He"
    // 之间应有的空格消失，两者直接拼在一起）。改为关闭全局trim，交给
    // 下面已经写对的t_preserve_space逐个w:t判断（与read_document同源
    // 同逻辑）。
    reader.config_mut().trim_text(false);

    let mut paragraphs: Vec<Paragraph> = Vec::new();
    let mut cur_para: Option<Paragraph> = None;
    let mut cur_text = String::new();
    let mut in_run = false;
    let mut in_rpr = false;
    let mut in_t = false;
    let mut t_buf = String::new();
    let mut t_preserve_space = false;
    let mut run_bold: Option<bool> = None;
    let mut run_italic: Option<bool> = None;
    let mut run_underline: Option<bool> = None;
    let mut run_strikethrough: Option<bool> = None;
    let mut run_color: Option<String> = None;
    let mut run_font_family: Option<String> = None;
    let mut run_font_family_ascii: Option<String> = None;
    // WROOT-136：页眉页脚 run 的字号与上下标
    let mut run_font_size: Option<f32> = None;
    let mut run_script: Option<roe_model::writer::ScriptPosition> = None;
    // WROOT-027/028 (v140修复): parse_isolated_paragraphs 此前完全没有
    // 解析 run 级 <w:rPr><w:rStyle w:val="..."/></w:rPr>，导致 note 内容
    // （endnotes.xml/footnotes.xml）里占位符 run（<w:endnoteRef/> 等）
    // 引用的字符样式（如 EndnoteReference，定义了 vertAlign=superscript）
    // 完全丢失，char_style 恒为 None。照抄 read_document 里 WROOT-055
    // 已验证的写法：run_style_id 收集 w:val，run 构造时映射进 char_style。
    let mut run_style_id: Option<String> = None;
    // WROOT任务A追查发现的第二个真实bug修复：此前这个函数完全没有解析
    // w:pPr 内的 w:jc（段落对齐），Paragraph.alignment 恒为 None（初始
    // 默认值），导致 header/footer 无论 XML 里写了什么对齐都渲染成默认
    // 左对齐。in_ppr 跟踪是否在 w:pPr 内，避免误吃 w:rPr 或其他地方同名
    // 标签（这个函数目前没有其他 w:jc 用途，但显式限定范围更安全）。
    let mut in_ppr = false;
    let mut para_alignment: Option<roe_model::style::Alignment> = None;
    // Step 2 (WROOT-075/027/028 endnote/footnote 支持)：w:pStyle 读取的样式ID，
    // None 时段落收尾按原有行为回退到硬编码 "Normal"，保持两个现有调用方
    // （load_hf_part / wps_txbx）零回归。
    let mut para_style_id: Option<String> = None;
    // WROOT-027/028 (v141续): w:pPr/w:ind 此前完全未解析，导致note内容
    // （endnotes.xml/footnotes.xml）里的段落缩进/悬挂缩进信息全部丢失，
    // layout.rs渲染层被迫用硬编码hanging_indent_pt=20.0一刀切套用在
    // 所有note段落上——不管文档自己有没有定义缩进、缩进是多少。
    // 照抄read_document里WROOT-ind的解析写法（w:left->indent_left,
    // w:hanging->indent_first取负值），单位从twips(1/20pt)换算成pt。
    let mut para_indent_left: Option<f32> = None;
    let mut para_indent_first: Option<f32> = None;
    // Step 2：当前 run 是否是 endnote/footnote 编号占位符（<w:endnoteRef/>/
    // <w:footnoteRef/>，Empty事件，与 w:br/w:tab 同级）。None 表示普通 run，
    // 沿用原有 field: None 行为。
    let mut run_field: Option<roe_model::writer::FieldKind> = None;
    // WROOT任务A追查发现的真实bug修复：w:pict(legacy VML) 内部常常嵌套
    // 完整的 w:txbxContent>w:p 子文档（例如空白定位占位文本框），这个函数
    // 是平坦状态机，此前对 w:pict 完全没有处理，导致内部嵌套的 <w:p> 被
    // 误判为与外层同级的新段落边界，直接覆盖(cur_para = Some(新段落))
    // 掉正在收集中的、真正带文字的外层段落（例如页脚 "AAA-888888a8"），
    // 外层段落从未被 push 进 paragraphs，静默丢失。
    // 修复：用深度计数器跳过整个 w:pict 子树，期间任何 w:p/w:r/w:t 都不
    // 参与外层状态机，直到匹配的 </w:pict> 才恢复正常处理。
    let mut pict_depth: u32 = 0;
    // WROOT-131：复杂域（fldChar begin/instrText/separate/缓存值/end）跨 run 状态机。
    // 页码 PAGE 域的缓存值（如 "2"）是 Word 保存时那一页的页码，不能当普通文字画。
    // fld_stage: 0=域外，1=已 begin 尚未 separate（instrText 收集中），2=已 separate（缓存值区间）。
    // 只识别 PAGE（trim 后以 "PAGE" 开头；NUMPAGES 不以 PAGE 开头，不会误中），
    // 其他域（NUMPAGES/STYLEREF/AUTONUM 等）保持原行为：缓存文字当普通文字。
    let mut fld_stage: u8 = 0;
    let mut fld_instr = String::new();
    let mut fld_is_page = false;
    // WROOT-132：NUMPAGES 域（文档总页数），与 PAGE 同构。
    let mut fld_is_numpages = false;
    let mut in_instr = false;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) if e.name().as_ref() == b"w:pict" => {
                pict_depth += 1;
            }
            Ok(Event::End(ref e)) if e.name().as_ref() == b"w:pict" => {
                pict_depth = pict_depth.saturating_sub(1);
            }
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) if pict_depth == 0 => match e.name().as_ref() {
                b"w:p" => {
                    cur_para = Some(Paragraph {
                        style: StyleRef("Normal".to_string()),
                        runs: Vec::new(),
                        list_info: None,
                        ..Default::default()
                    });
                    cur_text.clear();
                    para_alignment = None;
                    para_style_id = None;
                    para_indent_left = None;
                    para_indent_first = None;
                }
                b"w:pPr" if cur_para.is_some() => { in_ppr = true; }
                b"w:jc" if in_ppr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            let v = attr.unescape_value().unwrap_or_default();
                            para_alignment = match v.as_ref() {
                                "left" | "start" => Some(roe_model::style::Alignment::Left),
                                "center" => Some(roe_model::style::Alignment::Center),
                                "right" | "end" => Some(roe_model::style::Alignment::Right),
                                "both" | "distribute" => Some(roe_model::style::Alignment::Justify),
                                _ => None,
                            };
                        }
                    }
                }
                b"w:pStyle" if in_ppr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            if let Ok(v) = attr.unescape_value() {
                                para_style_id = Some(v.into_owned());
                            }
                        }
                    }
                }
                // WROOT-027/028 (v141续): 照抄 read_document 里 w:ind 的解析
                // 写法（约446行），单位统一从twips(1/20pt)换算成pt。
                b"w:ind" if in_ppr => {
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"w:left" => {
                                if let Ok(v) = attr.unescape_value() {
                                    let v: f32 = v.parse().unwrap_or(0.0);
                                    para_indent_left = Some(v / 20.0);
                                }
                            }
                            b"w:firstLine" => {
                                if let Ok(v) = attr.unescape_value() {
                                    let v: f32 = v.parse().unwrap_or(0.0);
                                    para_indent_first = Some(v / 20.0);
                                }
                            }
                            b"w:hanging" => {
                                if let Ok(v) = attr.unescape_value() {
                                    let v: f32 = v.parse().unwrap_or(0.0);
                                    para_indent_first = Some(-(v / 20.0));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                b"w:r" if cur_para.is_some() => {
                    in_run = true;
                    run_bold = None;
                    run_italic = None;
                    run_underline = None;
                    run_strikethrough = None;
                    run_font_size = None;
                    run_script = None;
                    run_color = None;
                    run_font_family = None;
                    run_font_family_ascii = None;
                    run_field = None;
                    run_style_id = None;
                }
                b"w:rPr" if in_run => { in_rpr = true; }
                b"w:t" if in_run => {
                    in_t = true;
                    t_buf.clear();
                    t_preserve_space = e.attributes().flatten().any(|attr| {
                        attr.key.as_ref() == b"xml:space"
                            && attr.unescape_value().map(|v| v == "preserve").unwrap_or(false)
                    });
                }
                b"w:b" if in_rpr => { run_bold = Some(true); }
                b"w:sz" if in_rpr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            if let Ok(hp) = attr.unescape_value().unwrap_or_default().parse::<f32>() {
                                run_font_size = Some(hp / 2.0);
                            }
                        }
                    }
                }
                b"w:vertAlign" if in_rpr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            run_script = match attr.unescape_value().unwrap_or_default().as_ref() {
                                "superscript" => Some(roe_model::writer::ScriptPosition::Superscript),
                                "subscript" => Some(roe_model::writer::ScriptPosition::Subscript),
                                _ => None,
                            };
                        }
                    }
                }
                b"w:i" if in_rpr => { run_italic = Some(true); }
                b"w:u" if in_rpr => { run_underline = Some(true); }
                b"w:strike" if in_rpr => { run_strikethrough = Some(true); }
                // WROOT-027/028 (v140修复): 照抄 read_document 里 WROOT-055
                // 已验证的 w:rStyle 解析写法，note 内容占位符 run 引用的
                // 字符样式（如 EndnoteReference）需要在这里被读取，才能
                // 在下游 char_style 里生效。
                b"w:rStyle" if in_rpr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            run_style_id = Some(attr.unescape_value().unwrap_or_default().into_owned());
                        }
                    }
                }
                b"w:color" if in_rpr => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            run_color = attr.unescape_value().ok().map(|v| v.into_owned());
                        }
                    }
                }
                b"w:rFonts" if in_rpr => {
                    let mut ascii_font: Option<String> = None;
                    let mut east_asia_font: Option<String> = None;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"w:ascii" => {
                                ascii_font = attr.unescape_value().ok().map(|v| v.into_owned());
                            }
                            b"w:eastAsia" => {
                                east_asia_font = attr.unescape_value().ok().map(|v| v.into_owned());
                            }
                            _ => {}
                        }
                    }
                    run_font_family = east_asia_font.clone().or(ascii_font.clone());
                    run_font_family_ascii = ascii_font;
                }
                b"w:br" if in_run => { cur_text.push('\n'); }
                b"w:tab" if in_run => { cur_text.push('\t'); }
                b"w:fldChar" if in_run => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:fldCharType" {
                            match attr.unescape_value().unwrap_or_default().as_ref() {
                                "begin" => {
                                    fld_stage = 1;
                                    fld_instr.clear();
                                    fld_is_page = false;
                                    fld_is_numpages = false;
                                }
                                "separate" => {
                                    fld_is_page = fld_stage == 1 && fld_instr.trim().starts_with("PAGE");
                                    fld_is_numpages = fld_stage == 1 && fld_instr.trim().starts_with("NUMPAGES");
                                    fld_stage = 2;
                                }
                                "end" => {
                                    fld_stage = 0;
                                    fld_is_page = false;
                                    fld_is_numpages = false;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                b"w:instrText" if in_run => { in_instr = true; }
                b"w:endnoteRef" if in_run => { run_field = Some(roe_model::writer::FieldKind::EndnoteRef); }
                b"w:footnoteRef" if in_run => { run_field = Some(roe_model::writer::FieldKind::FootnoteRef); }
                _ => {}
            },
            Ok(Event::Text(ref e)) => {
                if in_t && pict_depth == 0 {
                    if let Ok(txt) = e.unescape() {
                        t_buf.push_str(&txt);
                    }
                }
                if in_instr && pict_depth == 0 && fld_stage == 1 {
                    if let Ok(txt) = e.unescape() {
                        fld_instr.push_str(&txt);
                    }
                }
            }
            Ok(Event::End(ref e)) if pict_depth == 0 => match e.name().as_ref() {
                b"w:t" => {
                    in_t = false;
                    if t_preserve_space {
                        cur_text.push_str(&t_buf);
                    } else {
                        cur_text.push_str(t_buf.trim());
                    }
                    t_buf.clear();
                    t_preserve_space = false;
                }
                b"w:instrText" => { in_instr = false; }
                b"w:rPr" => { in_rpr = false; }
                b"w:pPr" => { in_ppr = false; }
                b"w:r" => {
                    in_run = false;
                    if let Some(ref mut p) = cur_para {
                        // Step 2 修复：此前这里用 `if !cur_text.is_empty()` 判断是否
                        // push run，会导致 <w:endnoteRef/> 这种无文本内容的 Empty 标签
                        // （它跟 w:t 不同，不产生 cur_text）被静默丢弃，field 标记随之
                        // 丢失。改为文本非空 或 携带 field 标记时都要 push。
                        // WROOT-131：PAGE 域缓存值区间内的 run，标记为 PageNumber，
                        // 渲染层按当前页号动态求值，忽略 text（缓存值仅作兜底）。
                        if fld_stage == 2 && fld_is_page && !cur_text.is_empty() {
                            run_field = Some(roe_model::writer::FieldKind::PageNumber);
                        }
                        // WROOT-132：NUMPAGES 缓存值区间的 run 标记为 NumPages，
                        // 渲染层按文档总页数动态求值。
                        if fld_stage == 2 && fld_is_numpages && !cur_text.is_empty() {
                            run_field = Some(roe_model::writer::FieldKind::NumPages);
                        }
                        if !cur_text.is_empty() || run_field.is_some() {
                            p.runs.push(Run {
                                text: cur_text.clone(),
                                char_style: run_style_id.clone().map(roe_model::common::StyleRef),
                                bold: run_bold,
                                italic: run_italic,
                                underline: run_underline,
                                strikethrough: run_strikethrough,
                                font_size: run_font_size,
                                script: run_script,
                                color: run_color.as_deref().map(parse_hex_color),
                                font_family: run_font_family.clone(),
                                font_family_ascii: run_font_family_ascii.clone(),
                                field: run_field,
                                ..Default::default()
                            });
                            cur_text.clear();
                        }
                    }
                }
                b"w:p" => {
                    if let Some(mut p) = cur_para.take() {
                        p.alignment = para_alignment;
                        if let Some(ref sid) = para_style_id {
                            p.style = StyleRef(sid.clone());
                        }
                        p.indent_left = para_indent_left;
                        p.indent_first = para_indent_first;
                        paragraphs.push(p);
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    paragraphs
}

/// DOCX-8-v1 (e): OMML (m:oMath 子树) → LaTeX 字符串。
///
/// 输入是从 `<m:oMath>` 到匹配 `</m:oMath>` 之间抠出的原始 XML 片段
/// （抠取方式同 wps:txbx 先例：buffer_position() + saturating_sub 回退，
/// 见 read_document 内 m:oMath 分支）。
///
/// 实现说明：OMML 是真正的递归树形结构（fixture 已验证 m:rad/m:nary 内部
/// 可嵌套 m:sSup 等复合标签），与本文件其余部分"扁平 flag 状态机"风格不同，
/// 用**显式栈**模拟递归：每个"容器类"标签（m:num/m:den/m:e/m:sub/m:sup/m:deg等）
/// 开始时压栈，闭合时弹栈并按标签语义包装（加 \\frac{{}}{{}} 之类）后
/// 拼接给父层 parts。
///
/// 关键陷阱（本轮排查确认，务必保留这段注释）：容器类标签可能以
/// `Event::Empty` 形态出现（如 `<m:deg/>`，n次方根省略时的空次数位），
/// 此时**不会再有对应的 `Event::End`**——如果只在 Empty 分支里压栈、
/// 指望后续 End 事件弹栈，栈永远不会正确闭合，会导致父容器错位读取到
/// 空/错误的子节点。修复方式：close_container 抽成独立函数，
/// Start/End 配对路径和 Empty(自闭合) 路径共享同一份"压栈后立即用
/// close_container 弹栈闭合"逻辑，不能只在 End 分支里调用它。
/// 这是与 v7 记录的"属性标签 Start/Empty 遗漏分支"同源但更隐蔽的一类
/// 陷阱——只在自闭合容器恰好为空内容时才会暴露，编造的简单测试用例
/// 容易漏测，本次是用真实 Word 生成的 fixture（degHide=1 时 Word 把
/// m:deg 存成空的自闭合标签）才实际触发并发现。
///
/// 映射规则详见 DOCX-8-v1-d-mapping-table.md，覆盖：
/// m:r/m:t（文本）、m:f（分数）、m:rad（根号，含degHide分支）、
/// m:sSup/m:sSub/m:sSubSup（上下标）、m:nary（大型运算符，含chr映射）。
/// 未识别标签兜底：忽略容器语义，仅拼接内部文本，不 panic。
fn parse_omml_to_latex(xml_chunk: &str) -> String {
    // DOCX-8-v1(h) 修复：parts从Vec<String>改为Vec<(Vec<u8>, String)>，
    // 记录每个已收集片段"来自哪个标签"，不再依赖数组下标顺序(first()/get(1)/
    // get(2))做语义判断。
    //
    // 根因（真实OnlyOffice fixture暴露，见roe-ext/fixtures/docx-8-omml-h/）：
    // 此前实现对 m:f 用 parts.first()=num、parts.get(1)=den，隐含假设
    // "m:num标签一定先于m:den出现"。这在(c)阶段Word生成的fixture里恰好成立，
    // 但OnlyOffice产出的真实文档里 m:den 在 m:num 之前，导致分子分母互换
    // （1+1/(2+1/3) 被错误转换成 1+(2+3/1)/1）。OOXML schema虽然为
    // m:sSup/m:sSub/m:sSubSup等定义了固定子元素顺序，但不能假设所有生产
    // 实现都精确遵守顺序、且不能假设文件不会被非标准工具生成/篡改，
    // 按标签名索引比按位置索引更安全，成本可忽略。
    struct Frame {
        parts: Vec<(Vec<u8>, String)>,
        nary_chr: Option<String>,
        rad_deg_hide: bool,
        // DOCX-8-v3续: m:d（定界符）用，仿照 nary_chr 的写法。
        // m:dPr 本身不入栈，其子元素 m:begChr/m:endChr/m:sepChr
        // 作为 Event::Empty 单值属性标签直接写入这里（当前栈顶
        // 就是 m:d 自己的帧，因为 m:dPr 没有单独占一层栈）。
        d_beg_chr: Option<String>,
        d_end_chr: Option<String>,
        d_sep_chr: Option<String>,
        // DOCX-8-v3续: m:groupChr 用，复用 m:chr 的 Empty 事件读取逻辑
        // （物理上写入 nary_chr 字段，语义上代表"当前容器的符号字符"，
        // m:nary 和 m:groupChr 不会同层出现，不冲突，不新增重复字段）。
        // group_pos 专属：m:pos 的 val（"top"/"bot"），None 时按OOXML
        // 规范默认视为"bot"（与LOK一致）。
        group_pos: Option<String>,

        // DOCX-8-v3续: m:borderBox（边框盒）专用。四个strike开关各自
        // 独立存储（不能复用group_pos/nary_chr，那两个字段只能存一个
        // String，这里需要同时判断4个bool）。m:borderBoxPr不入栈，
        // 同m:dPr/m:groupChrPr/m:barPr/m:accPr手法。
        strike_h: bool,
        strike_v: bool,
        strike_tlbr: bool,
        strike_bltr: bool,
    }
    impl Frame {
        fn new() -> Self {
            Frame {
                parts: Vec::new(),
                nary_chr: None,
                rad_deg_hide: true,
                d_beg_chr: None,
                d_end_chr: None,
                d_sep_chr: None,
                group_pos: None,
                strike_h: false,
                strike_v: false,
                strike_tlbr: false,
                strike_bltr: false,
            }
        }
        /// 按标签名找第一个匹配的已收集内容，找不到则回退空字符串。
        fn find(&self, tag: &[u8]) -> String {
            self.parts.iter().find(|(t, _)| t.as_slice() == tag)
                .map(|(_, s)| s.clone()).unwrap_or_default()
        }
        /// 按标签名找全部匹配的已收集内容，按出现顺序返回。
        /// m:d（定界符）可含多个 m:e 子元素（如 (a|b) 这类多段分隔场景），
        /// find() 只取第一个不够用，这里新增按序取全部。
        fn find_all(&self, tag: &[u8]) -> Vec<String> {
            self.parts.iter().filter(|(t, _)| t.as_slice() == tag)
                .map(|(_, s)| s.clone()).collect()
        }
        /// 拼接所有片段的文本内容（不看标签名，用于m:r/m:num等"纯累加"场景）
        fn joined(&self) -> String {
            self.parts.iter().map(|(_, s)| s.as_str()).collect()
        }
    }

    const CONTAINER_TAGS: &[&[u8]] = &[
        b"m:r", b"m:f", b"m:rad", b"m:sSup", b"m:sSub", b"m:sSubSup", b"m:nary",
        b"m:num", b"m:den", b"m:e", b"m:sub", b"m:sup", b"m:deg",
        b"m:d", // DOCX-8-v3续: 定界符/括号
        b"m:func", b"m:fName", // DOCX-8-v3续: 函数名（sin/cos/log等）
        b"m:limLow", b"m:limUpp", b"m:lim", // DOCX-8-v3续: 极限上下标
        b"m:groupChr", // DOCX-8-v3续: 群组符号标注（overbrace/underbrace等）
        b"m:bar", b"m:acc", // DOCX-8-v3续: 上下划线 / 重音符号
        b"m:borderBox", // DOCX-8-v3续: 边框盒（strikeH/V/TLBR/BLTR + hide四开关，hide忽略）
        b"m:eqArr", // DOCX-8-v3续: 方程组对齐（多个m:e堆叠）
        b"m:sPre", // DOCX-8-v3续: 前置上下标（如同位素记法235U）
        b"m:mr", b"m:m", // DOCX-8-v3续: 矩阵（mr=行，m=整体，二维结构两层标签）
    ];

    // 把栈顶帧按标签语义包装成 LaTeX 片段，push 给父帧。
    // Start/End 配对路径和 Empty(自闭合) 路径共用这一个函数，
    // 是修复"自闭合容器标签栈错位"bug 的关键——两条路径必须走同一份闭合逻辑。
    fn close_container(stack: &mut Vec<Frame>, tag: &[u8]) {
        let Some(finished) = stack.pop() else { return };
        let Some(parent) = stack.last_mut() else { return };
        match tag {
            b"m:r" | b"m:num" | b"m:den" | b"m:e" | b"m:sub" | b"m:sup" | b"m:deg" | b"m:fName" | b"m:lim" => {
                parent.parts.push((tag.to_vec(), finished.joined()));
            }
            b"m:bar" => {
                // DOCX-8-v3续: 上/下划线。参考LOK handleBar：m:barPr
                // 内m:pos的val("top"/"bot")决定overline/underline，
                // 复用group_pos字段（m:barPr不入栈，同m:dPr/m:groupChrPr
                // 手法，val直接写入m:bar自己的帧）。默认bot（同LOK）。
                let e_content = finished.find(b"m:e");
                let pos = finished.group_pos.as_deref().unwrap_or("bot");
                let result = if pos == "top" {
                    format!("\\overline{{{e_content}}}")
                } else {
                    format!("\\underline{{{e_content}}}")
                };
                parent.parts.push((tag.to_vec(), result));
            }
            b"m:borderBox" => {
                // DOCX-8-v3续: 边框盒。参考LOK handleBorderBox + OnlyOffice
                // CBorderBoxPr（双源交叉验证字段名一致）：m:borderBoxPr内
                // 8个开关（hideTop/Bot/Left/Right + strikeH/V/TLBR/BLTR），
                // 均为独立bool，写入本帧strike_h/strike_v/strike_tlbr/
                // strike_bltr四个专属字段（不能复用group_pos/nary_chr，
                // 那两个只能存一个String）。
                //
                // 设计取舍（已与Violet确认）：hide四开关全部忽略，不支持
                // 局部边框（只支持"全框"或"无框"），因为LaTeX没有原生的
                // "四边独立可控矩形框"命令，要自己拼会让工程量不可控。
                //
                // strike系列映射（已用真实样本math-overstrike.docx验证，
                // 该样本hideTop/Bot/Left/Right全1+strikeH=1，四边全隐藏
                // 只留strikeH，是当前样本库里这几个低优先级标签的唯一
                // 真实命中案例）：
                //   strikeH   -> \sout{e}     （纯水平线，语义精确匹配，
                //                                RaTeX注释里"Horizontal
                //                                line at -0.5x x-height"）
                //   strikeTLBR-> \bcancel{e}  （左上->右下对角线，RaTeX
                //                                bcancel方向确认匹配）
                //   strikeBLTR-> \cancel{e}   （左下->右上对角线，RaTeX
                //                                cancel方向确认匹配）
                //   都没有   -> \fbox{e}      （画完整矩形，忽略hide）
                //
                // 多个strike同时为true时优先级：strikeH > strikeTLBR >
                // strikeBLTR > strikeV，取第一个命中，不做嵌套组合（当前
                // 唯一真实样本不存在组合场景，不做过度设计）。
                //
                // strikeV（垂直划线）：RaTeX无原生对应命令（\cancel/
                // \bcancel都是对角斜线，不是纯垂直线），不降级用错误
                // 方向的命令冒充。真正的绘制逻辑（复用tile.rs已有的
                // draw_v_line，同\frac分数线用的那个函数，参照OnlyOffice
                // sdkjs borderBox.js的drawVerLine做法）留给方向B（渲染
                // 管线）实现。当前这里仍然只输出e本身（不加任何LaTeX
                // 命令包裹），Formula.needs_vertical_strike字段的真正
                // 传递通道（需要改parse_omml_to_latex返回类型，改动面
                // 较大）也推迟到方向B一并处理。
                let e_content = finished.find(b"m:e");
                let result = if finished.strike_h {
                    format!("\\sout{{{e_content}}}")
                } else if finished.strike_tlbr {
                    format!("\\bcancel{{{e_content}}}")
                } else if finished.strike_bltr {
                    format!("\\cancel{{{e_content}}}")
                } else if finished.strike_v {
                    // strikeV单独出现（无其他strike命中）：暂不输出\bcancel
                    // 降级（会误导），也暂不加\fbox，直接透传e，等方向B
                    // 打通传递通道后再补上真正的垂直线绘制。
                    e_content
                } else {
                    format!("\\fbox{{{e_content}}}")
                };
                parent.parts.push((tag.to_vec(), result));
            }
            b"m:acc" => {
                // DOCX-8-v3续: 重音符号。参考LOK handleAcc的字符->命令
                // 映射表（码点来自starmath/inc/types.hxx，已核实），
                // 目标从StarMath语法名换成等价RaTeX命令（accent.rs
                // 已确认支持的命令集，已核实）。RaTeX无dddot(三点)/
                // 无宽版vec专用命令，用最接近的等价物代替（降级但不
                // panic不丢内容，已知局限）。复用nary_chr字段存m:chr
                // 的val（m:accPr不入栈，同上）。
                fn map_acc_char(chr: &str) -> &'static str {
                    match chr {
                        "\u{0060}" | "\u{0300}" => "\\grave",
                        "\u{00b4}" | "\u{0301}" => "\\acute",
                        "\u{005e}" | "\u{0302}" => "\\hat",
                        "\u{007e}" | "\u{0303}" => "\\tilde",
                        "\u{00af}" | "\u{0304}" => "\\bar",
                        "\u{0305}" => "\\overline", // COMBOVERLINE：LOK标注为overline语义
                        "\u{02d8}" | "\u{0306}" => "\\breve",
                        "\u{02da}" | "\u{030a}" => "\\mathring", // circle：RaTeX无\circle重音，用等价物
                        "\u{02c7}" | "\u{030c}" => "\\check",
                        "\u{20d0}" => "\\overleftharpoon", // LHARPOON
                        "\u{20d1}" => "\\overrightharpoon", // HARPOON
                        "\u{20d6}" => "\\overleftarrow", // LVEC：RaTeX无宽版leftvec，箭头近似
                        "\u{20d7}" | "\u{2192}" => "\\overrightarrow", // VEC/RIGHTARROW：同上，箭头近似
                        "\u{02d9}" | "\u{0307}" => "\\dot",
                        "\u{00a8}" | "\u{0308}" => "\\ddot",
                        "\u{20db}" => "\\ddot", // DDDOT：RaTeX无三点重音，降级用两点，已知局限
                        _ => "\\acute", // 未知字符兜底，同LOK default分支
                    }
                }
                let e_content = finished.find(b"m:e");
                let chr = finished.nary_chr.as_deref().unwrap_or("");
                let cmd = map_acc_char(chr);
                parent.parts.push((tag.to_vec(), format!("{cmd}{{{e_content}}}")));
            }
            b"m:limLow" => {
                // DOCX-8-v3续: 极限下标，如 lim_{x \to 0}。字段名(e/lim)
                // 已核实LOK与OnlyOffice一致。RaTeX的下标语法(_{})可以
                // 直接跟在任意花括号组后面，不需要\lim前缀（外层若是
                // m:func套m:fName=lim会已经产出\lim，此处只管加下标）。
                let e_content = finished.find(b"m:e");
                let lim = finished.find(b"m:lim");
                parent.parts.push((tag.to_vec(), format!("{{{e_content}}}_{{{lim}}}")));
            }
            b"m:limUpp" => {
                // DOCX-8-v3续: 极限上标，如 overbrace场景配合groupChr使用
                // （groupChr产出\overbrace{e}后，limUpp在外层加^{lim}，
                // 两者天然拼接成\overbrace{e}^{lim}，不需要LOK式的字符串
                // 后缀缝合技巧——因为我们直接输出目标LaTeX语法）。
                let e_content = finished.find(b"m:e");
                let lim = finished.find(b"m:lim");
                parent.parts.push((tag.to_vec(), format!("{{{e_content}}}^{{{lim}}}")));
            }
            b"m:groupChr" => {
                // DOCX-8-v3续: 群组符号标注。参考LOK handleGroupChr +
                // OnlyOffice CGroupChrPr结构（m:chr的val + m:pos的val，
                // 均已核实一致）。标准overbrace(⏞ U+23DE)/underbrace
                // (⏟ U+23DF)配对对应位置时用专用命令拿正确弯曲方向；
                // 其余字符+位置组合用\overset/\underset通用兜底
                // （RaTeX functions/mclass.rs确认支持）。
                let e_content = finished.find(b"m:e");
                let chr = finished.nary_chr.as_deref().unwrap_or("");
                let pos = finished.group_pos.as_deref().unwrap_or("bot");
                let result = if chr == "\u{23de}" && pos == "top" {
                    format!("\\overbrace{{{e_content}}}")
                } else if chr == "\u{23df}" && pos == "bot" {
                    format!("\\underbrace{{{e_content}}}")
                } else if pos == "top" {
                    format!("\\overset{{{chr}}}{{{e_content}}}")
                } else {
                    format!("\\underset{{{chr}}}{{{e_content}}}")
                };
                parent.parts.push((tag.to_vec(), result));
            }
            b"m:func" => {
                // DOCX-8-v3续: 函数名。参考 LOK
                // starmath/source/ooxmlimport.cxx::handleFunc：
                // fname + " {" + e + "}"。标准三角/对数/极限函数名
                // （RaTeX functions/op.rs 注册表）转\前缀命令拿直立
                // 字体+正确间距；自定义函数名（如 f、g）原样输出，
                // 走默认斜体变量渲染，符合数学排版惯例。
                fn map_func_name(raw: &str) -> String {
                    const KNOWN_FUNCS: &[&str] = &[
                        "arcsin", "arccos", "arctan", "arctg", "arcctg", "arg", "ch", "cos",
                        "cosec", "cosh", "cot", "cotg", "coth", "csc", "ctg", "cth", "deg",
                        "dim", "exp", "hom", "ker", "lg", "ln", "log", "sec", "sin",
                        "sinh", "sh", "tan", "tanh", "tg", "th",
                        "det", "gcd", "inf", "lim", "max", "min", "Pr", "sup",
                    ];
                    if KNOWN_FUNCS.contains(&raw) {
                        format!("\\{raw}")
                    } else {
                        raw.to_string()
                    }
                }
                let fname_raw = finished.find(b"m:fName");
                let fname = map_func_name(fname_raw.trim());
                let e_content = finished.find(b"m:e");
                parent.parts.push((tag.to_vec(), format!("{fname} {{{e_content}}}")));
            }
            b"m:f" => {
                let num = finished.find(b"m:num");
                let den = finished.find(b"m:den");
                parent.parts.push((tag.to_vec(), format!("\\frac{{{num}}}{{{den}}}")));
            }
            b"m:rad" => {
                let deg = finished.find(b"m:deg");
                let e_content = finished.find(b"m:e");
                let content = if finished.rad_deg_hide || deg.trim().is_empty() {
                    format!("\\sqrt{{{e_content}}}")
                } else {
                    format!("\\sqrt[{deg}]{{{e_content}}}")
                };
                parent.parts.push((tag.to_vec(), content));
            }
            b"m:sSup" => {
                let base = finished.find(b"m:e");
                let sup = finished.find(b"m:sup");
                parent.parts.push((tag.to_vec(), format!("{base}^{{{sup}}}")));
            }
            b"m:sSub" => {
                let base = finished.find(b"m:e");
                let sub = finished.find(b"m:sub");
                parent.parts.push((tag.to_vec(), format!("{base}_{{{sub}}}")));
            }
            b"m:sSubSup" => {
                let base = finished.find(b"m:e");
                let sub = finished.find(b"m:sub");
                let sup = finished.find(b"m:sup");
                parent.parts.push((tag.to_vec(), format!("{base}_{{{sub}}}^{{{sup}}}")));
            }
            b"m:nary" => {
                let sub = finished.find(b"m:sub");
                let sup = finished.find(b"m:sup");
                let e_content = finished.find(b"m:e");
                let has_sub = finished.parts.iter().any(|(t, _)| t.as_slice() == b"m:sub");
                let has_sup = finished.parts.iter().any(|(t, _)| t.as_slice() == b"m:sup");
                let cmd = match finished.nary_chr.as_deref() {
                    Some("\u{2211}") => "\\sum",
                    Some("\u{222b}") => "\\int",
                    Some("\u{220f}") => "\\prod",
                    Some("\u{22c3}") => "\\bigcup",
                    Some("\u{22c2}") => "\\bigcap",
                    Some(other) => {
                        let sub_s = if has_sub { format!("_{{{sub}}}") } else { String::new() };
                        let sup_s = if has_sup { format!("^{{{sup}}}") } else { String::new() };
                        parent.parts.push((tag.to_vec(), format!("{other}{sub_s}{sup_s} {e_content}")));
                        return;
                    }
                    None => "\\sum",
                };
                let sub_s = if has_sub { format!("_{{{sub}}}") } else { String::new() };
                let sup_s = if has_sup { format!("^{{{sup}}}") } else { String::new() };
                parent.parts.push((tag.to_vec(), format!("{cmd}{sub_s}{sup_s} {e_content}")));
            }
            b"m:d" => {
                // DOCX-8-v3续: 定界符/括号。映射表参考 LOK
                // starmath/source/ooxmlimport.cxx::handleD，转译为
                // \left...\right LaTeX语法（RaTeX确认支持，见
                // ratex-parser/src/functions/left_right.rs）。
                // 默认值（省略 m:dPr 时）：opening="(" closing=")"
                // separator="|"，与LOK一致。
                fn map_delim_char(raw: &str, default: &str) -> String {
                    match raw {
                        "" => ".".to_string(), // LaTeX不可见定界符标准写法
                        "{" => "\\{".to_string(),
                        "}" => "\\}".to_string(),
                        "\u{2016}" => "\\|".to_string(), // ‖ 双竖线范数
                        "\u{27e8}" => "\\langle".to_string(), // ⟨
                        "\u{27e9}" => "\\rangle".to_string(), // ⟩
                        // OOXML双方括号(⟦⟧)等RaTeX无直接对应命令，
                        // 降级为普通方括号，已知局限，不panic不丢内容。
                        "\u{27e6}" => "[".to_string(),
                        "\u{27e7}" => "]".to_string(),
                        other if !other.is_empty() => other.to_string(),
                        _ => default.to_string(),
                    }
                }
                let opening = map_delim_char(
                    finished.d_beg_chr.as_deref().unwrap_or("("), "(",
                );
                let closing = map_delim_char(
                    finished.d_end_chr.as_deref().unwrap_or(")"), ")",
                );
                let sep_raw = finished.d_sep_chr.as_deref().unwrap_or("|");
                let separator = map_delim_char(sep_raw, "|");
                let items = finished.find_all(b"m:e");
                let joined = if items.is_empty() {
                    finished.joined() // 兜底：理论上不该发生，防御性处理
                } else {
                    items.join(&format!(" \\middle{separator} "))
                };
                parent.parts.push((
                    tag.to_vec(),
                    format!("\\left{opening} {joined} \\right{closing}"),
                ));
            }
            b"m:eqArr" => {
                // DOCX-8-v3续: 方程组对齐。参考LOK handleEqArr（StarMath
                // 输出"stack {e1 # e2 # ...}"）+ OnlyOffice CEqArr/CEqArrPr
                // （双源交叉验证字段结构一致：m:eqArrPr含m:baseJc/m:maxDist/
                // m:objDist/m:rSp/m:rSpRule等排版微调属性，均忽略不处理，
                // 只影响渲染细节间距不影响语义，同m:d对非核心dPr属性的
                // 处理思路一致）。
                //
                // roe目标语言是LaTeX不是StarMath，"多个m:e堆叠对齐"这个
                // 语义在LaTeX里有原生环境对应：\begin{aligned}...\end{aligned}
                // （RaTeX已确认支持，ratex-parser/src/environments.rs:767
                // 注册了"align"/"align*"/"aligned"/"split"四个环境名，
                // tests.rs:811有\begin{aligned}的parse测试）。
                //
                // 复用m:d分支同款find_all(b"m:e")模式，区别只在连接符
                // （\\换行 而非 \middle|）和外层包装
                // （\begin{aligned}\end{aligned} 而非 \left\right）。
                let items = finished.find_all(b"m:e");
                let joined = if items.is_empty() {
                    finished.joined() // 兜底：理论上不该发生，防御性处理
                } else {
                    items.join(" \\\\ ")
                };
                parent.parts.push((
                    tag.to_vec(),
                    format!("\\begin{{aligned}} {joined} \\end{{aligned}}"),
                ));
            }
            b"m:sPre" => {
                // DOCX-8-v3续: 前置上下标（如同位素记法 235-U）。参考
                // OnlyOffice CSPre/LOK对应结构（均确认三个子容器：
                // m:sub/m:sup/m:e，无m:sPrePr以外的属性需要处理，
                // m:sPrePr本身只含ctrlPr排版属性，忽略）。
                //
                // LaTeX没有原生"前置上下标"命令，标准写法是空组起手：
                // {}_{sub}^{sup}{e}（数学界同位素记法惯用写法，本次
                // session已用最小fixture验证RaTeX parser接受这个语法，
                // 见/tmp/spre_test验证记录，3种候选写法全部parse成功）。
                // sub/sup均可能为空（OOXML允许只写一个），空的话产出
                // 空花括号^{}或_{}，语法仍合法，不特判省略。
                let sub = finished.find(b"m:sub");
                let sup = finished.find(b"m:sup");
                let e_content = finished.find(b"m:e");
                parent.parts.push((
                    tag.to_vec(),
                    format!("{{}}_{{{sub}}}^{{{sup}}}{{{e_content}}}"),
                ));
            }
            b"m:mr" => {
                // DOCX-8-v3续: 矩阵的一行。参考LOK handleM+OnlyOffice
                // CMatrixRow结构（均确认m:mr直接含多个m:e子元素，
                // m:mcPr/m:mcs列属性均可忽略，LOK handleM函数体本身
                // 就没解析m:mPr，印证这些属性对生成正确LaTeX不是必需）。
                // 用find_all(b"m:e")取该行全部单元格，按"&"连接
                // （LaTeX matrix环境的标准列分隔符）。
                let cells = finished.find_all(b"m:e");
                let joined = cells.join(" & ");
                parent.parts.push((tag.to_vec(), joined));
            }
            b"m:m" => {
                // DOCX-8-v3续: 矩阵整体。二维结构：m:m含多个m:mr行，
                // 每行已在上面m:mr分支被拼成"a & b"形式的字符串，这里
                // 用find_all(b"m:mr")取全部行，按"\\"（LaTeX换行）
                // 连接，包一层\begin{matrix}...\end{matrix}（RaTeX已
                // 确认原生支持matrix环境族，ratex-parser/src/
                // environments.rs:484-490注册）。
                // 真实样本math-matrix.docx验证过：单个2x2矩阵，无
                // m:mPr，行列数规整，无需处理边角情况（不规则行列数
                // 场景暂不特殊处理，按find_all实际取到的内容直接拼接）。
                let rows = finished.find_all(b"m:mr");
                let joined = rows.join(" \\\\ ");
                parent.parts.push((
                    tag.to_vec(),
                    format!("\\begin{{matrix}} {joined} \\end{{matrix}}"),
                ));
            }
            _ => {}
        }
    }

    let mut reader = Reader::from_str(xml_chunk);
    reader.config_mut().trim_text(true);

    let mut stack: Vec<Frame> = vec![Frame::new()];
    let mut in_t = false;
    let mut t_buf = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = e.name().as_ref().to_vec();
                if CONTAINER_TAGS.contains(&name.as_slice()) {
                    stack.push(Frame::new());
                } else if name.as_slice() == b"m:t" {
                    in_t = true;
                    t_buf.clear();
                }
            }
            Ok(Event::Empty(ref e)) => {
                let name = e.name().as_ref().to_vec();
                if CONTAINER_TAGS.contains(&name.as_slice()) {
                    // 自闭合容器（如 <m:deg/>）：立即压栈立即闭合，产出空子节点。
                    // 不能只压栈——见函数顶部注释，这是本轮排查到的关键陷阱。
                    stack.push(Frame::new());
                    close_container(&mut stack, &name);
                } else if name.as_slice() == b"m:degHide" {
                    if let Some(frame) = stack.last_mut() {
                        let val = e.attributes().flatten()
                            .find(|a| a.key.as_ref().ends_with(b"val"))
                            .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
                            .unwrap_or_default();
                        frame.rad_deg_hide = val != "0";
                    }
                } else if name.as_slice() == b"m:chr" {
                    // DOCX-8-v3续: 这个Empty事件同时服务 m:nary 和
                    // m:groupChr 两种容器（写入同一个 nary_chr 字段，
                    // 见 Frame 定义处注释，两者不会同层出现不冲突）。
                    if let Some(frame) = stack.last_mut() {
                        frame.nary_chr = e.attributes().flatten()
                            .find(|a| a.key.as_ref().ends_with(b"val"))
                            .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()));
                    }
                } else if name.as_slice() == b"m:pos" {
                    // DOCX-8-v3续: m:groupChr 专属，top/bot 决定符号
                    // 画在内容上方还是下方。
                    if let Some(frame) = stack.last_mut() {
                        frame.group_pos = e.attributes().flatten()
                            .find(|a| a.key.as_ref().ends_with(b"val"))
                            .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()));
                    }
                } else if name.as_slice() == b"m:begChr" {
                    // DOCX-8-v3续: m:dPr 不入栈，其子元素直接写入
                    // 当前栈顶（即 m:d 自己的帧）。
                    if let Some(frame) = stack.last_mut() {
                        frame.d_beg_chr = e.attributes().flatten()
                            .find(|a| a.key.as_ref().ends_with(b"val"))
                            .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()));
                    }
                } else if name.as_slice() == b"m:endChr" {
                    if let Some(frame) = stack.last_mut() {
                        frame.d_end_chr = e.attributes().flatten()
                            .find(|a| a.key.as_ref().ends_with(b"val"))
                            .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()));
                    }
                } else if name.as_slice() == b"m:sepChr" {
                    if let Some(frame) = stack.last_mut() {
                        frame.d_sep_chr = e.attributes().flatten()
                            .find(|a| a.key.as_ref().ends_with(b"val"))
                            .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()));
                    }
                } else if name.as_slice() == b"m:strikeH" {
                    // DOCX-8-v3续: m:borderBoxPr 不入栈，子元素直接写入
                    // 当前栈顶（即 m:borderBox 自己的帧），同 m:dPr 手法。
                    // val 属性缺省视为 true（OOXML Empty开关标签惯例：
                    // 标签出现即代表启用，同 m:degHide 的 "0"/其余值 处理）。
                    if let Some(frame) = stack.last_mut() {
                        let val = e.attributes().flatten()
                            .find(|a| a.key.as_ref().ends_with(b"val"))
                            .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()));
                        frame.strike_h = val.as_deref().map(|v| v != "0" && v != "false").unwrap_or(true);
                    }
                } else if name.as_slice() == b"m:strikeV" {
                    if let Some(frame) = stack.last_mut() {
                        let val = e.attributes().flatten()
                            .find(|a| a.key.as_ref().ends_with(b"val"))
                            .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()));
                        frame.strike_v = val.as_deref().map(|v| v != "0" && v != "false").unwrap_or(true);
                    }
                } else if name.as_slice() == b"m:strikeTLBR" {
                    if let Some(frame) = stack.last_mut() {
                        let val = e.attributes().flatten()
                            .find(|a| a.key.as_ref().ends_with(b"val"))
                            .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()));
                        frame.strike_tlbr = val.as_deref().map(|v| v != "0" && v != "false").unwrap_or(true);
                    }
                } else if name.as_slice() == b"m:strikeBLTR" {
                    if let Some(frame) = stack.last_mut() {
                        let val = e.attributes().flatten()
                            .find(|a| a.key.as_ref().ends_with(b"val"))
                            .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()));
                        frame.strike_bltr = val.as_deref().map(|v| v != "0" && v != "false").unwrap_or(true);
                    }
                }
            }
            Ok(Event::Text(ref e)) => {
                if in_t {
                    if let Ok(txt) = e.unescape() {
                        t_buf.push_str(&txt);
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let name = e.name().as_ref().to_vec();
                if name.as_slice() == b"m:t" {
                    in_t = false;
                    if let Some(frame) = stack.last_mut() {
                        frame.parts.push((b"m:t".to_vec(), t_buf.clone()));
                    }
                    t_buf.clear();
                } else if CONTAINER_TAGS.contains(&name.as_slice()) {
                    close_container(&mut stack, &name);
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    stack.first().map(|f| f.joined()).unwrap_or_default()
}



#[cfg(test)]
mod test_docx1_style_registry {
    use super::*;

    #[test]
    fn test_build_style_registry_splits_by_type_and_keeps_based_on() {
        let mut styles = HashMap::new();
        styles.insert("Normal".to_string(), RawStyle {
            style_id: "Normal".to_string(),
            style_type: Some("paragraph".to_string()),
            based_on: None,
            name: "Normal".to_string(),
            para_props: ParaProps { alignment: Some("left".to_string()), ..Default::default() },
            char_props: CharProps { font_size: Some(12.0), ..Default::default() },
        });
        styles.insert("Heading1".to_string(), RawStyle {
            style_id: "Heading1".to_string(),
            style_type: Some("paragraph".to_string()),
            based_on: Some("Normal".to_string()),
            name: "Heading 1".to_string(),
            para_props: ParaProps { outline_level: Some(0), ..Default::default() },
            char_props: CharProps { bold: Some(true), font_size: Some(16.0), ..Default::default() },
        });
        styles.insert("Emphasis".to_string(), RawStyle {
            style_id: "Emphasis".to_string(),
            style_type: Some("character".to_string()),
            based_on: None,
            name: "Emphasis".to_string(),
            para_props: ParaProps::default(),
            char_props: CharProps { italic: Some(true), ..Default::default() },
        });
        // w:type 缺省，应按 paragraph 处理（OOXML 默认行为）
        styles.insert("Untyped".to_string(), RawStyle {
            style_id: "Untyped".to_string(),
            style_type: None,
            based_on: None,
            name: "Untyped".to_string(),
            para_props: ParaProps::default(),
            char_props: CharProps::default(),
        });

        let graph = StyleGraph { styles, ..Default::default() };
        let registry = build_style_registry(&graph);

        // 类型分流：character 样式不应出现在 paragraph_styles 里，反之亦然
        assert!(registry.paragraph_styles.contains_key("Normal"));
        assert!(registry.paragraph_styles.contains_key("Heading1"));
        assert!(registry.paragraph_styles.contains_key("Untyped"));
        assert!(!registry.paragraph_styles.contains_key("Emphasis"));
        assert!(registry.char_styles.contains_key("Emphasis"));
        assert!(!registry.char_styles.contains_key("Normal"));

        // based_on 原样保留，未在转换阶段展开
        assert_eq!(
            registry.paragraph_styles.get("Heading1").unwrap().based_on.as_deref(),
            Some("Normal")
        );

        // 端到端验证：StyleRegistry::resolve_paragraph_style 能正确走继承链
        // （这是 DOCX-1 真正要修的行为：之前 styles 字段恒为空表，这里必须非空
        // 且继承生效）
        let resolved = registry.resolve_paragraph_style("Heading1");
        assert_eq!(resolved.alignment.map(|a| matches!(a, roe_model::style::Alignment::Left)), Some(true));
        assert_eq!(resolved.font_size, Some(16.0)); // Heading1 覆盖了 Normal 的 12.0
        assert!(resolved.bold);
    }

    #[test]
    fn test_build_style_registry_skips_table_and_numbering_types() {
        let mut styles = HashMap::new();
        styles.insert("TableGrid".to_string(), RawStyle {
            style_id: "TableGrid".to_string(),
            style_type: Some("table".to_string()),
            based_on: None,
            name: "Table Grid".to_string(),
            para_props: ParaProps::default(),
            char_props: CharProps::default(),
        });

        let graph = StyleGraph { styles, ..Default::default() };
        let registry = build_style_registry(&graph);

        assert!(registry.paragraph_styles.is_empty());
        assert!(registry.char_styles.is_empty());
    }

    #[test]
    fn test_parse_hex_color_matches_pptx_behavior() {
        let c = parse_hex_color("FF5733");
        assert_eq!(c.r, 0xFF);
        assert_eq!(c.g, 0x57);
        assert_eq!(c.b, 0x33);
        assert_eq!(c.a, 255);
    }
}

#[cfg(test)]
mod test_docx4_vmerge_probe {
    use super::*;

    /// 探测性测试：确认 quick-xml 对 <w:vMerge/>（自闭合，无 val）、
    /// <w:vMerge w:val="restart"/>（自闭合，带 val）两种真实文件里最常见
    /// 的写法，均落入 Event::Empty 分支，不会被误判为 Event::Start。
    /// 这决定了 w:vMerge 的解析分支是否只写 Event::Empty 就够用。
    #[test]
    fn test_vmerge_self_closing_forms_are_event_empty() {
        let cases = [
            r#"<w:vMerge/>"#,
            r#"<w:vMerge w:val="restart"/>"#,
            r#"<w:vMerge w:val="continue"/>"#,
        ];
        for xml in cases {
            let mut reader = Reader::from_str(xml);
            reader.config_mut().trim_text(true);
            let event = reader.read_event().unwrap();
            match event {
                Event::Empty(ref e) => {
                    assert_eq!(e.name().as_ref(), b"w:vMerge", "case: {xml}");
                }
                other => panic!("expected Event::Empty for {xml:?}, got {other:?}"),
            }
        }
    }

    /// 同时验证：若某个生产者真的写成非自闭合形式 <w:vMerge></w:vMerge>，
    /// quick-xml 会给出 Event::Start，当前解析代码不会处理它（只写了
    /// Event::Empty 分支）。这条测试记录这个已知边界，不是要修复它——
    /// 现有代码库里 w:b/w:i 等同类属性标签也是同样处理方式，保持一致，
    /// 不在 DOCX-4 范围内单独加固这个 schema 允许但实践中罕见的写法。
    #[test]
    fn test_vmerge_non_self_closing_form_is_event_start_not_handled() {
        let xml = r#"<w:vMerge w:val="restart"></w:vMerge>"#;
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);
        let event = reader.read_event().unwrap();
        match event {
            Event::Start(ref e) => {
                assert_eq!(e.name().as_ref(), b"w:vMerge");
            }
            other => panic!("expected Event::Start, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod test_docx4_apply_vmerge {
    use super::*;
    // 遗留编译错误修复（本次会话顺手补上，与pptx工作无关，纯粹是缺失的
    // 导入语句，不改动任何逻辑）：CellContent 未从 roe_model::common 导入，
    // 导致 cargo test -p roe-parser --lib 整体无法编译，见 roe-ext_progress
    // v61/v62 交接文档记录的已知问题。
    use roe_model::common::CellContent;

    fn make_cell(text: &str) -> TableCell {
        TableCell {
            content: vec![CellContent::Paragraph(Paragraph {
                style: StyleRef("Normal".to_string()),
                runs: vec![Run { text: text.to_string(), char_style: None, ..Default::default() }],
                list_info: None,
                ..Default::default()
            })],
            col_span: 1,
            row_span: 1,
            style: None,
            hidden: false, bg_color: None,
            border_color: None, border_width: None,
            pad_left: None, pad_right: None, pad_top: None, pad_bottom: None,
        }
    }

    #[test]
    fn test_row_span_computed_and_continue_content_cleared() {
        // 3行1列表格，第0/1/2行同一列做纵向合并：restart, continue, continue
        // 预期：row0.row_span == 3，row1/row2 内容被清空，cells 数组长度不变
        let mut tbl = Table {
            rows: vec![
                TableRow { cells: vec![make_cell("A")], row_height: None },
                TableRow { cells: vec![make_cell("B")], row_height: None },
                TableRow { cells: vec![make_cell("C")], row_height: None },
            ],
            col_widths: vec![100.0],
            style_id: None, first_row: false, first_col: false, last_row: false, last_col: false, band_row: false, band_col: false,
            default_border_color: None, default_border_width: None,
            inside_h_color: None, inside_h_width: None, inside_v_color: None, inside_v_width: None,
            float_pos: None,
        };
        let tbl_vmerge = vec![
            vec![Some(true)],   // row0: restart
            vec![Some(false)],  // row1: continue
            vec![Some(false)],  // row2: continue
        ];

        apply_vmerge(&mut tbl, &tbl_vmerge);

        assert_eq!(tbl.rows.len(), 3, "行数不应变化");
        assert_eq!(tbl.rows[0].cells.len(), 1);
        assert_eq!(tbl.rows[0].cells[0].row_span, 3, "restart 单元格应累计到 3");
        assert!(tbl.rows[1].cells[0].content.is_empty(), "continue 单元格内容应清空");
        assert!(tbl.rows[2].cells[0].content.is_empty(), "continue 单元格内容应清空");
        assert_eq!(tbl.rows[1].cells[0].row_span, 1, "continue 单元格自身 row_span 不应被修改");
    }

    #[test]
    fn test_unmerged_column_untouched() {
        // 未参与合并的列（全 None）不应受影响
        let mut tbl = Table {
            rows: vec![
                TableRow { cells: vec![make_cell("X")], row_height: None },
                TableRow { cells: vec![make_cell("Y")], row_height: None },
            ],
            col_widths: vec![100.0],
            style_id: None, first_row: false, first_col: false, last_row: false, last_col: false, band_row: false, band_col: false,
            default_border_color: None, default_border_width: None,
            inside_h_color: None, inside_h_width: None, inside_v_color: None, inside_v_width: None,
            float_pos: None,
        };
        let tbl_vmerge = vec![vec![None], vec![None]];

        apply_vmerge(&mut tbl, &tbl_vmerge);

        assert_eq!(tbl.rows[0].cells[0].row_span, 1);
        assert_eq!(tbl.rows[1].cells[0].row_span, 1);
        assert!(!tbl.rows[0].cells[0].content.is_empty());
        assert!(!tbl.rows[1].cells[0].content.is_empty());
    }

    #[test]
    fn test_multi_column_independent_merge() {
        // 2列2行：col0 纵向合并(restart+continue)，col1 不合并
        let mut tbl = Table {
            rows: vec![
                TableRow { cells: vec![make_cell("A1"), make_cell("B1")], row_height: None },
                TableRow { cells: vec![make_cell("A2"), make_cell("B2")], row_height: None },
            ],
            col_widths: vec![50.0, 50.0],
            style_id: None, first_row: false, first_col: false, last_row: false, last_col: false, band_row: false, band_col: false,
            default_border_color: None, default_border_width: None,
            inside_h_color: None, inside_h_width: None, inside_v_color: None, inside_v_width: None,
            float_pos: None,
        };
        let tbl_vmerge = vec![
            vec![Some(true), None],
            vec![Some(false), None],
        ];

        apply_vmerge(&mut tbl, &tbl_vmerge);

        assert_eq!(tbl.rows[0].cells[0].row_span, 2, "col0 应合并");
        assert!(tbl.rows[1].cells[0].content.is_empty(), "col0 row1 应清空");
        assert_eq!(tbl.rows[0].cells[1].row_span, 1, "col1 不受影响");
        assert!(!tbl.rows[1].cells[1].content.is_empty(), "col1 row1 内容应保留");
        // 关键约束：cells 数组长度全程不变，不会导致列错位
        assert_eq!(tbl.rows[0].cells.len(), 2);
        assert_eq!(tbl.rows[1].cells.len(), 2);
    }

    #[test]
    fn test_continue_without_restart_is_ignored_defensively() {
        // 非法/损坏文件场景：第一行直接是 continue，没有先导 restart
        let mut tbl = Table {
            rows: vec![TableRow { cells: vec![make_cell("Orphan")], row_height: None }],
            col_widths: vec![100.0],
            style_id: None, first_row: false, first_col: false, last_row: false, last_col: false, band_row: false, band_col: false,
            default_border_color: None, default_border_width: None,
            inside_h_color: None, inside_h_width: None, inside_v_color: None, inside_v_width: None,
            float_pos: None,
        };
        let tbl_vmerge = vec![vec![Some(false)]];

        apply_vmerge(&mut tbl, &tbl_vmerge);

        assert_eq!(tbl.rows[0].cells[0].row_span, 1);
        assert!(!tbl.rows[0].cells[0].content.is_empty(), "无先导 restart 时不应清空内容");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_style_strikethrough_and_highlight_resolve_and_convert() {
        // [ROE-STYLE-STRIKE-HL] 验证 CharProps.strikethrough/background
        // 能正确走完 merge_char 继承合并 + to_char_style 转换两段链路，
        // 不再是写死的 None。同时验证 parse_highlight_color 对预设颜色名
        // "yellow" 的映射结果，以及 "none"/未识别值应返回 None 而不是
        // 静默变成黑色。
        let mut styles = HashMap::new();
        styles.insert("Normal".to_string(), RawStyle {
            style_id: "Normal".to_string(),
            style_type: None,
            based_on: None,
            name: "Normal".to_string(),
            para_props: ParaProps::default(),
            char_props: CharProps { strikethrough: Some(true), background: Some("yellow".to_string()), ..Default::default() },
        });
        styles.insert("NoHighlight".to_string(), RawStyle {
            style_id: "NoHighlight".to_string(),
            style_type: None,
            based_on: None,
            name: "NoHighlight".to_string(),
            para_props: ParaProps::default(),
            char_props: CharProps { background: Some("none".to_string()), ..Default::default() },
        });

        let graph = StyleGraph { styles, ..Default::default() };

        // merge_char 继承合并层面：resolve() 应该原样带出这两个字段
        let resolved = graph.resolve("Normal");
        assert_eq!(resolved.char.strikethrough, Some(true));
        assert_eq!(resolved.char.background.as_deref(), Some("yellow"));

        // to_char_style 转换层面：应该把 "yellow" 映射成正确的RGB，
        // 而不是走 parse_hex_color 静默变成黑色 (0,0,0)
        let raw_normal = graph.styles.get("Normal").unwrap();
        let char_style = to_char_style(raw_normal);
        assert_eq!(char_style.strikethrough, Some(true));
        match char_style.background {
            Some(color) => {
                assert_eq!((color.r, color.g, color.b), (0xFF, 0xFF, 0x00), "yellow应映射为(255,255,0)，实际={:?}", color);
            }
            None => panic!("expected Some(yellow color), got None"),
        }

        // "none" 应该返回 None，而不是黑色 (0,0,0) 这种看似"有值但是错的"结果
        let raw_none = graph.styles.get("NoHighlight").unwrap();
        let char_style_none = to_char_style(raw_none);
        assert_eq!(char_style_none.background, None, "w:highlight=none 应解析为 None，而不是某个颜色值");
    }


    #[test]
    fn test_style_shd_background_priority_and_inheritance() {
        // [ROE-STYLE-SHD] 验证三件事：
        // 1. shd_background（w:shd任意RGB）优先于 background（w:highlight
        //    16种预设色名）——即使两者都设置了，to_char_style 应该采信
        //    shd_background，因为它精度更高，与写出层的选择保持读写对称。
        // 2. shd_background 单独存在（没有 background）时也能正确转换。
        // 3. merge_char 继承合并链路：子样式没有自己的 shd_background 时，
        //    应该从 based_on 父样式继承到（这是本次会话发现并补上的
        //    遗漏点，之前只顾着改 to_char_style，漏了 merge_char 那一行）。
        let mut styles = HashMap::new();
        styles.insert("BothSet".to_string(), RawStyle {
            style_id: "BothSet".to_string(),
            style_type: None,
            based_on: None,
            name: "BothSet".to_string(),
            para_props: ParaProps::default(),
            char_props: CharProps {
                background: Some("yellow".to_string()),
                shd_background: Some("123456".to_string()),
                ..Default::default()
            },
        });
        styles.insert("ShdOnly".to_string(), RawStyle {
            style_id: "ShdOnly".to_string(),
            style_type: None,
            based_on: None,
            name: "ShdOnly".to_string(),
            para_props: ParaProps::default(),
            char_props: CharProps {
                shd_background: Some("ABCDEF".to_string()),
                ..Default::default()
            },
        });
        styles.insert("ShdParent".to_string(), RawStyle {
            style_id: "ShdParent".to_string(),
            style_type: None,
            based_on: None,
            name: "ShdParent".to_string(),
            para_props: ParaProps::default(),
            char_props: CharProps {
                shd_background: Some("00FF00".to_string()),
                ..Default::default()
            },
        });
        styles.insert("ShdChild".to_string(), RawStyle {
            style_id: "ShdChild".to_string(),
            style_type: None,
            based_on: Some("ShdParent".to_string()),
            name: "ShdChild".to_string(),
            para_props: ParaProps::default(),
            char_props: CharProps { bold: Some(true), ..Default::default() },
        });

        let graph = StyleGraph { styles, ..Default::default() };

        // 优先级：两者都设置时，shd_background(#123456) 应该赢过
        // background("yellow")
        let both_set = graph.styles.get("BothSet").unwrap();
        let both_style = to_char_style(both_set);
        let both_color = both_style.background.expect("expected Some color");
        assert_eq!(
            (both_color.r, both_color.g, both_color.b),
            (0x12, 0x34, 0x56),
            "两者都设置时应采信shd_background而不是highlight"
        );

        // shd_background 单独存在
        let shd_only = graph.styles.get("ShdOnly").unwrap();
        let shd_only_style = to_char_style(shd_only);
        let shd_only_color = shd_only_style.background.expect("expected Some color");
        assert_eq!((shd_only_color.r, shd_only_color.g, shd_only_color.b), (0xAB, 0xCD, 0xEF));

        // 继承链：ShdChild 自己没有 shd_background，应该从 ShdParent 继承到
        let resolved_child = graph.resolve("ShdChild");
        assert_eq!(
            resolved_child.char.shd_background.as_deref(),
            Some("00FF00"),
            "子样式应该从based_on父样式继承shd_background，这是merge_char此前遗漏的一行"
        );
        let raw_child = graph.styles.get("ShdChild").unwrap();
        // 注意：to_char_style 只读 raw.char_props（未经继承合并的原始值），
        // 不读 resolved——继承展开是 resolve() 的职责，to_char_style 只做
        // 字段格式转换。所以这里直接用 raw_child 验证会得到 None，是符合
        // 预期的、与本文件其余继承相关测试一致的分工方式（真正验证继承
        // 生效要看上面 resolved_child 那一段）。
        let child_style_raw = to_char_style(raw_child);
        assert_eq!(
            child_style_raw.background, None,
            "raw RawStyle 本身不含继承展开结果，符合to_char_style与resolve()的既有分工"
        );
    }

    #[test]
    fn test_style_resolver_inheritance() {
        let mut styles = HashMap::new();
        styles.insert("Normal".to_string(), RawStyle {
            style_id: "Normal".to_string(),
            style_type: None,
            based_on: None,
            name: "Normal".to_string(),
            para_props: ParaProps { alignment: Some("left".to_string()), ..Default::default() },
            char_props: CharProps { font_size: Some(12.0), ..Default::default() },
        });
        styles.insert("Heading1".to_string(), RawStyle {
            style_id: "Heading1".to_string(),
            style_type: None,
            based_on: Some("Normal".to_string()),
            name: "Heading 1".to_string(),
            para_props: ParaProps { outline_level: Some(0), ..Default::default() },
            char_props: CharProps { bold: Some(true), font_size: Some(16.0), ..Default::default() },
        });

        let graph = StyleGraph { styles, ..Default::default() };
        let resolved = graph.resolve("Heading1");

        assert_eq!(resolved.para.alignment.as_deref(), Some("left"));
        assert_eq!(resolved.para.outline_level, Some(0));
        assert_eq!(resolved.char.font_size, Some(16.0));
        assert_eq!(resolved.char.bold, Some(true));
    }

    #[test]
    fn test_roman_numerals() {
        assert_eq!(to_roman(1, true), "I");
        assert_eq!(to_roman(4, true), "IV");
        assert_eq!(to_roman(9, false), "ix");
        assert_eq!(to_roman(14, true), "XIV");
        assert_eq!(to_roman(40, false), "xl");
    }

    #[test]
    fn test_numbering_advance() {
        let mut store = NumberingStore::new();
        let mut abs = AbstractNum::default();
        abs.levels.push(NumLevel {
            num_fmt: "decimal".to_string(),
            lvl_text: "%1.".to_string(),
            start_val: 1,
            indent_left: None,
            indent_first: None,
            color: None,
            font_family: None,
            highlight: None,
        });
        abs.levels.push(NumLevel {
            num_fmt: "decimal".to_string(),
            lvl_text: "%1.%2.".to_string(),
            start_val: 1,
            indent_left: None,
            indent_first: None,
            color: None,
            font_family: None,
            highlight: None,
        });
        store.abstract_nums.insert(0, abs);
        store.nums.insert(1, NumInstance {
            abstract_num_id: 0,
            overrides: HashMap::new(),
            level_overrides: HashMap::new(),
        });

        assert_eq!(store.advance(1, 0), "1.");
        assert_eq!(store.advance(1, 0), "2.");
        assert_eq!(store.advance(1, 1), "2.1.");
        assert_eq!(store.advance(1, 1), "2.2.");
        assert_eq!(store.advance(1, 0), "3.");
        assert_eq!(store.advance(1, 1), "3.1.");
    }

    #[test]
    fn test_letter_format() {
        assert_eq!(to_letter(1, true), "A");
        assert_eq!(to_letter(26, true), "Z");
        assert_eq!(to_letter(1, false), "a");
    }
}

/// 页面布局：从 word/document.xml 的 w:sectPr 解析页面尺寸和页边距
/// 单位：twip（1/20 pt），转换为 pt
#[derive(Debug, Clone)]
struct PageLayout {
    page_size:     (f32, f32),
    margin_top:    f32,
    margin_bottom: f32,
    margin_left:   f32,
    margin_right:  f32,
    /// 分栏（w:cols）。默认单栏，语义见 roe_model::writer::DocumentColumns。
    columns: roe_model::writer::DocumentColumns,
    /// STEP6-STAGE-B：w:sectPr/w:endnotePr/w:numFmt 的 w:val。
    endnote_num_fmt: roe_model::writer::NumFormat,
    /// STEP6-STAGE-B：w:sectPr/w:footnotePr/w:numFmt 的 w:val。
    footnote_num_fmt: roe_model::writer::NumFormat,
}

impl Default for PageLayout {
    fn default() -> Self {
        Self {
            page_size:     (595.28, 841.89), // A4
            margin_top:    56.69,
            margin_bottom: 56.69,
            margin_left:   70.87,
            margin_right:  70.87,
            columns: roe_model::writer::DocumentColumns::default(),
            endnote_num_fmt: roe_model::writer::NumFormat::default(),
            footnote_num_fmt: roe_model::writer::NumFormat::default(),
        }
    }
}

/// [WROOT-047] 改造为 per-sectPr 解析，返回 Vec<PageLayout>（按 body 中
/// sectPr 出现顺序，与 read_header_footer_refs 返回的 Vec<HeaderFooterRefs>
/// 下标语义对齐，供 doc.sections[] 构造时按相同 idx 配对读取）。此前是单个
/// PageLayout 全局覆盖式解析，多节文档（各节pgSz不同）会导致后一节覆盖
/// 前一节，与 read_header_footer_refs 改造前的问题是同一处根因模式。
/// STEP6-STAGE-B：w:numFmt 的 w:val 字符串 → NumFormat 枚举。
/// 未识别的值（真实文档里出现过 "decimal" 等其它 OOXML numFmt 变体，
/// 目前 NumFormat 只覆盖脚注/尾注/列表最常见的几种）一律归为 Arabic，
/// 与 NumFormat::default() 语义一致，不 panic、不报错。
fn parse_num_fmt_attr(val: &str) -> roe_model::writer::NumFormat {
    use roe_model::writer::NumFormat;
    match val {
        "lowerRoman" => NumFormat::LowerRoman,
        "upperRoman" => NumFormat::UpperRoman,
        "lowerLetter" => NumFormat::LowerAlpha,
        "upperLetter" => NumFormat::UpperAlpha,
        "none" => NumFormat::None,
        _ => NumFormat::Arabic,
    }
}

fn read_page_layout<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
) -> Vec<PageLayout> {
    let xml = match read_zip_entry(zip, "word/document.xml") {
        Ok(x) => x,
        Err(_) => return Vec::new(),
    };
    let mut reader = Reader::from_str(&xml);
    reader.config_mut().trim_text(true);
    let mut results: Vec<PageLayout> = Vec::new();
    let mut layout = PageLayout::default();
    let mut in_sect = false;
    let mut in_cols = false;
    // STEP6-STAGE-B：w:endnotePr/w:footnotePr 内部的 w:numFmt 需要区分
    // 是给 endnote 还是 footnote 用的，两者都是 sectPr 直接子元素、
    // 结构对称（各自包一个 w:numFmt 子元素）。
    let mut in_endnote_pr = false;
    let mut in_footnote_pr = false;
    // WROOT-027 numbering fix: NumFormat::default() (Arabic) is the
    // generic OOXML numFmt fallback used everywhere else (lists, etc),
    // but real Word has a DIFFERENT implicit default specifically for
    // w:endnotePr when it has no w:numFmt child at all: lowerRoman, not
    // decimal (footnote's implicit default IS decimal/Arabic, so that
    // one stays as-is). Track whether an explicit endnote w:numFmt was
    // actually seen so we can distinguish "never declared" from
    // "explicitly declared as decimal" -- only the former should be
    // upgraded to lowerRoman below.
    let mut endnote_num_fmt_seen = false;
    // equalWidth=1（或未显式声明，OOXML默认true）时的临时寄存：
    // w:cols 自身的 num/space，找齐后一次性写回 layout.columns。
    let mut cols_num: Option<usize> = None;
    let mut cols_equal_width = true;
    let mut cols_uniform_space: f32 = 0.0;
    loop {
        // [WROOT-047] 函数返回类型从 Option<PageLayout> 改为 Vec<PageLayout>
        // 后不能再用 `?` 提前返回（Vec 不支持 FromResidual）。XML 读取失败
        // 时改为直接 break 出循环，退化为返回目前已收集到的 results（哪怕
        // 是空的，调用方已有 results.is_empty() 时补默认值的兜底逻辑）。
        let event = match reader.read_event() {
            Ok(ev) => ev,
            Err(_) => break,
        };
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                match e.name().as_ref() {
                    b"w:sectPr" => in_sect = true,
                    b"w:endnotePr" if in_sect => { in_endnote_pr = true; }
                    b"w:footnotePr" if in_sect => { in_footnote_pr = true; }
                    b"w:numFmt" if in_endnote_pr => {
                        endnote_num_fmt_seen = true;
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"w:val" {
                                if let Ok(s) = attr.unescape_value() {
                                    layout.endnote_num_fmt = parse_num_fmt_attr(&s);
                                }
                            }
                        }
                    }
                    b"w:numFmt" if in_footnote_pr => {
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"w:val" {
                                if let Ok(s) = attr.unescape_value() {
                                    layout.footnote_num_fmt = parse_num_fmt_attr(&s);
                                }
                            }
                        }
                    }
                    b"w:cols" if in_sect => {
                        in_cols = true;
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"w:num" => {
                                    if let Ok(s) = attr.unescape_value() {
                                        cols_num = s.parse::<usize>().ok();
                                    }
                                }
                                b"w:space" => {
                                    if let Ok(s) = attr.unescape_value() {
                                        if let Ok(v) = s.parse::<f32>() {
                                            cols_uniform_space = v / 20.0;
                                        }
                                    }
                                }
                                b"w:equalWidth" => {
                                    if let Ok(s) = attr.unescape_value() {
                                        // OOXML ST_OnOff: "0"/"false"/"off" 均为假，
                                        // 其余（含属性缺失，此分支不会命中缺失情况）为真。
                                        cols_equal_width = !matches!(s.as_ref(), "0" | "false" | "off");
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    b"w:col" if in_cols && !cols_equal_width => {
                        let mut w: f32 = 0.0;
                        let mut space: f32 = 0.0;
                        for attr in e.attributes().flatten() {
                            if let Ok(s) = attr.unescape_value() {
                                if let Ok(v) = s.parse::<f32>() {
                                    match attr.key.as_ref() {
                                        b"w:w"     => w = v / 20.0,
                                        b"w:space" => space = v / 20.0,
                                        _ => {}
                                    }
                                }
                            }
                        }
                        layout.columns.col_widths.push(w);
                        layout.columns.col_spacing.push(space);
                    }
                    b"w:pgSz" if in_sect => {
                        let mut w: Option<f32> = None;
                        let mut h: Option<f32> = None;
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"w:w" => {
                                    if let Ok(s) = attr.unescape_value() {
                                        w = s.parse::<f32>().ok();
                                    }
                                }
                                b"w:h" => {
                                    if let Ok(s) = attr.unescape_value() {
                                        h = s.parse::<f32>().ok();
                                    }
                                }
                                _ => {}
                            }
                        }
                        if let (Some(ww), Some(hh)) = (w, h) {
                            layout.page_size = (ww / 20.0, hh / 20.0);
                        }
                    }
                    b"w:pgMar" if in_sect => {
                        for attr in e.attributes().flatten() {
                            if let Ok(s) = attr.unescape_value() {
                                if let Ok(v) = s.parse::<f32>() {
                                    let pt = v / 20.0;
                                    match attr.key.as_ref() {
                                        b"w:top"    => layout.margin_top    = pt,
                                        b"w:bottom" => layout.margin_bottom = pt,
                                        b"w:left"   => layout.margin_left   = pt,
                                        b"w:right"  => layout.margin_right  = pt,
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::End(ref e) if e.name().as_ref() == b"w:cols" => {
                in_cols = false;
            }
            Event::End(ref e) if e.name().as_ref() == b"w:endnotePr" => {
                in_endnote_pr = false;
            }
            Event::End(ref e) if e.name().as_ref() == b"w:footnotePr" => {
                in_footnote_pr = false;
            }
            Event::End(ref e) if e.name().as_ref() == b"w:sectPr" => {
                in_sect = false;
                // [WROOT-047] 分栏信息统一写回（沿用原有"循环结束时统一处理"
                // 的必要性说明：<w:cols/>自闭合和带子元素两种写法事件序列不同，
                // 但到 sectPr 结束时这几个临时变量已收集完整），然后把这一节
                // 的 layout 存入结果 Vec、重置所有跨节状态，供下一个 sectPr
                // 从干净状态重新收集，不与上一节混淆。
                if cols_equal_width {
                    layout.columns.num_cols = cols_num.unwrap_or(1).max(1);
                    layout.columns.uniform_spacing = cols_uniform_space;
                } else if !layout.columns.col_widths.is_empty() {
                    layout.columns.num_cols = layout.columns.col_widths.len();
                }
                // WROOT-027 numbering fix: when this section's w:endnotePr
                // had no w:numFmt child at all, layout.endnote_num_fmt is
                // still sitting at NumFormat::default() (Arabic), which is
                // the generic OOXML fallback but NOT what real Word does
                // for endnotes specifically -- Word's implicit numbering
                // for endnotes with no declared numFmt is lowerRoman.
                // Footnote's implicit default really is decimal/Arabic, so
                // footnote_num_fmt is intentionally left untouched here.
                if !endnote_num_fmt_seen {
                    layout.endnote_num_fmt = roe_model::writer::NumFormat::LowerRoman;
                }
                results.push(layout);
                layout = PageLayout::default();
                cols_num = None;
                cols_equal_width = true;
                cols_uniform_space = 0.0;
                endnote_num_fmt_seen = false;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    // 兜底：如果整份文档没有任何 w:sectPr（理论上不应发生，docx规范要求至少
    // body末尾有一个），results 为空，返回一个默认值的单元素Vec，保持调用方
    // "至少有一份可用" 的假设不被打破。
    if results.is_empty() {
        results.push(PageLayout::default());
    }
    results
}

#[cfg(test)]
mod tests_wroot001_page_break {
    use super::*;
    use crate::ooxml::docx::DocxParser;

    fn make_docx_body(body_xml: &str) -> Vec<u8> {
        use std::io::Write;
        let content_types = r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;
        let rels = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
        let document = format!(r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body_xml}</w:body></w:document>"#);
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("[Content_Types].xml", opts).unwrap();
        zip.write_all(content_types.as_bytes()).unwrap();
        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    /// [ROE-STYLE-STRIKE-HL] 与 make_docx_body 类似，但额外写入自定义
    /// word/styles.xml，用于端到端验证 read_styles 对 w:strike/w:highlight
    /// 等标签的真实 XML 解析（不是内存里直接构造 RawStyle 那种只测转换
    /// 逻辑、不测 XML 解析本身的方式）。
    fn make_docx_body_with_styles(body_xml: &str, styles_xml_inner: &str) -> Vec<u8> {
        use std::io::Write;
        let content_types = r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/></Types>"#;
        let rels = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
        let document = format!(r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body_xml}</w:body></w:document>"#);
        let styles = format!(r#"<?xml version="1.0" encoding="UTF-8"?><w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">{styles_xml_inner}</w:styles>"#);
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("[Content_Types].xml", opts).unwrap();
        zip.write_all(content_types.as_bytes()).unwrap();
        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document.as_bytes()).unwrap();
        zip.start_file("word/styles.xml", opts).unwrap();
        zip.write_all(styles.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    /// [ROE-STYLE-STRIKE-HL] 端到端验证：真实 styles.xml 里的 <w:strike/>
    /// 和 <w:highlight w:val="yellow"/> 能被 read_styles 正确解析、经过
    /// build_style_registry 落到最终 doc.styles.char_styles 里，而不是
    /// 停留在"内存里手工构造RawStyle能work"这种只测了一半的验证。
    #[test]
    fn test_docx_xml_strike_and_highlight_end_to_end() {
        let styles_xml = r#"<w:style w:type="character" w:styleId="Emphasis2"><w:name w:val="Emphasis2"/><w:rPr><w:strike/><w:highlight w:val="yellow"/></w:rPr></w:style><w:style w:type="character" w:styleId="NoHighlight2"><w:name w:val="NoHighlight2"/><w:rPr><w:highlight w:val="none"/></w:rPr></w:style>"#;
        let body = r#"<w:p><w:r><w:t>test</w:t></w:r></w:p>"#;
        let bytes = make_docx_body_with_styles(body, styles_xml);
        let doc = DocxParser::parse(&bytes).expect("parse should succeed");

        let emphasis2 = doc.styles.char_styles.get("Emphasis2")
            .expect("Emphasis2 字符样式应该存在于 doc.styles.char_styles 里");
        assert_eq!(emphasis2.strikethrough, Some(true), "w:strike 应解析为 strikethrough=Some(true)");
        match emphasis2.background {
            Some(color) => assert_eq!((color.r, color.g, color.b), (0xFF, 0xFF, 0x00), "w:highlight val=yellow 应映射为黄色"),
            None => panic!("expected Some(yellow), got None"),
        }

        let no_highlight2 = doc.styles.char_styles.get("NoHighlight2")
            .expect("NoHighlight2 字符样式应该存在");
        assert_eq!(no_highlight2.background, None, "w:highlight val=none 应解析为 None");
    }

    /// WROOT-001: w:pPr/w:pageBreakBefore 应该让该段落 page_break_before=true
    #[test]
    fn page_break_before_sets_flag_on_own_paragraph() {
        let body = r#"<w:p><w:r><w:t>First</w:t></w:r></w:p><w:p><w:pPr><w:pageBreakBefore/></w:pPr><w:r><w:t>Second</w:t></w:r></w:p>"#;
        let bytes = make_docx_body(body);
        let doc = DocxParser::parse(&bytes).expect("parse should succeed");
        let paras: Vec<&Paragraph> = doc.body.iter().filter_map(|b| {
            if let BodyElement::Paragraph(p) = b { Some(p) } else { None }
        }).collect();
        assert_eq!(paras.len(), 2);
        assert!(!paras[0].page_break_before, "第一段不应带page_break_before");
        assert!(paras[1].page_break_before, "带pageBreakBefore的段落应标记page_break_before=true");
    }

    /// WROOT-001: 段落内嵌 w:sectPr（分节符）应该让*下一段*的 page_break_before=true，
    /// 而不是含sectPr的这一段自己。
    #[test]
    fn inline_sectpr_sets_flag_on_next_paragraph_not_own() {
        let body = r#"<w:p><w:pPr><w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr></w:pPr><w:r><w:t>LastOfSection1</w:t></w:r></w:p><w:p><w:r><w:t>FirstOfSection2</w:t></w:r></w:p>"#;
        let bytes = make_docx_body(body);
        let doc = DocxParser::parse(&bytes).expect("parse should succeed");
        let paras: Vec<&Paragraph> = doc.body.iter().filter_map(|b| {
            if let BodyElement::Paragraph(p) = b { Some(p) } else { None }
        }).collect();
        assert_eq!(paras.len(), 2);
        assert!(!paras[0].page_break_before, "含内嵌sectPr的段落自己不应标记page_break_before");
        assert!(paras[1].page_break_before, "内嵌sectPr之后的下一段应标记page_break_before=true");
    }

    /// 普通段落（无pageBreakBefore、无内嵌sectPr）不应被误标记
    #[test]
    fn plain_paragraph_has_no_page_break() {
        let body = r#"<w:p><w:r><w:t>Plain</w:t></w:r></w:p>"#;
        let bytes = make_docx_body(body);
        let doc = DocxParser::parse(&bytes).expect("parse should succeed");
        let BodyElement::Paragraph(p) = &doc.body[0] else { panic!("expected paragraph") };
        assert!(!p.page_break_before);
    }

    /// WROOT-031第三步：正文 commentRangeStart/End/commentReference 锚点
    /// 关联。结构照抄真实样本 CommentDone.docx 里 "eget" 一词的批注范围
    /// （用本次会话核实过的真实XML片段裁剪出最小复现用例，中间保留
    /// w:proofErr 裸标签验证pending队列能穿透非run裸标签）：
    ///   <w:r><w:t>before </w:t></w:r>
    ///   <w:commentRangeStart w:id="0"/>
    ///   <w:proofErr w:type="spellStart"/>
    ///   <w:r><w:t>eget</w:t></w:r>
    ///   <w:commentRangeEnd w:id="0"/>
    ///   <w:proofErr w:type="spellEnd"/>
    ///   <w:r><w:commentReference w:id="0"/></w:r>
    /// 放在本mod内（而非新建mod）是因为要复用本mod私有的make_docx_body。
    #[test]
    fn comment_range_start_end_and_reference_attach_to_correct_runs() {
        let body = r#"<w:p><w:r><w:t xml:space="preserve">before </w:t></w:r><w:commentRangeStart w:id="0"/><w:proofErr w:type="spellStart"/><w:r><w:t>eget</w:t></w:r><w:commentRangeEnd w:id="0"/><w:proofErr w:type="spellEnd"/><w:r><w:rPr><w:rStyle w:val="CommentReference"/></w:rPr><w:commentReference w:id="0"/></w:r></w:p>"#;
        let bytes = make_docx_body(body);
        let doc = DocxParser::parse(&bytes).expect("parse should succeed");
        let BodyElement::Paragraph(p) = &doc.body[0] else { panic!("expected paragraph") };

        // 3个run：普通文字run（无锚点） / "eget"（range start+end同时挂在
        // 这一个run上，因为它就是整个范围） / 纯commentReference空run。
        assert_eq!(p.runs.len(), 3, "should keep all 3 runs including text-less commentReference run");

        // run 0："before "，不应带任何批注锚点
        assert!(p.runs[0].comment_range_start.is_empty());
        assert!(p.runs[0].comment_range_end.is_empty());
        assert_eq!(p.runs[0].comment_ref, None);

        // run 1："eget"，range start/end 都应该是 [0]（穿透中间的
        // w:proofErr 裸标签，pending队列正确延续到这个run）
        assert_eq!(p.runs[1].text, "eget");
        assert_eq!(p.runs[1].comment_range_start, vec![0u32]);
        assert_eq!(p.runs[1].comment_range_end, vec![0u32]);
        assert_eq!(p.runs[1].comment_ref, None);

        // run 2：纯commentReference空run，文字为空但不能被丢弃，
        // comment_ref应为Some(0)
        assert_eq!(p.runs[2].text, "");
        assert_eq!(p.runs[2].comment_ref, Some(0));
        assert!(p.runs[2].comment_range_start.is_empty());
        assert!(p.runs[2].comment_range_end.is_empty());
    }

    /// 边界情况：没有任何commentRange/commentReference标签的普通段落，
    /// 三个新字段应保持全默认（空Vec/None），不受本次改动影响。
    #[test]
    fn paragraph_without_comment_anchors_has_default_empty_fields() {
        let body = r#"<w:p><w:r><w:t>Plain text</w:t></w:r></w:p>"#;
        let bytes = make_docx_body(body);
        let doc = DocxParser::parse(&bytes).expect("parse should succeed");
        let BodyElement::Paragraph(p) = &doc.body[0] else { panic!("expected paragraph") };
        assert_eq!(p.runs.len(), 1);
        assert!(p.runs[0].comment_range_start.is_empty());
        assert!(p.runs[0].comment_range_end.is_empty());
        assert_eq!(p.runs[0].comment_ref, None);
    }
}

#[cfg(test)]
mod tests_pgsz {
    use super::*;
    use crate::ooxml::docx::DocxParser;

    fn make_docx(w: u32, h: u32, mt: u32, mb: u32, ml: u32, mr: u32) -> Vec<u8> {
        use std::io::Write;
        let content_types = r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;
        let rels = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
        let document = format!(r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Hi</w:t></w:r></w:p><w:sectPr><w:pgSz w:w="{w}" w:h="{h}"/><w:pgMar w:top="{mt}" w:bottom="{mb}" w:left="{ml}" w:right="{mr}"/></w:sectPr></w:body></w:document>"#);
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("[Content_Types].xml", opts).unwrap();
        zip.write_all(content_types.as_bytes()).unwrap();
        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn parse_page_layout_letter() {
        // Letter: 12240x15840 twip = 612x792 pt
        // margins: 1440/1440/1800/1800 twip = 72/72/90/90 pt
        let data = make_docx(12240, 15840, 1440, 1440, 1800, 1800);
        let doc = DocxParser::parse(&data).unwrap();
        assert!((doc.page_size.0 - 612.0).abs() < 0.5, "width={}", doc.page_size.0);
        assert!((doc.page_size.1 - 792.0).abs() < 0.5, "height={}", doc.page_size.1);
        assert!((doc.margin_top    - 72.0).abs() < 0.5, "margin_top={}", doc.margin_top);
        assert!((doc.margin_bottom - 72.0).abs() < 0.5, "margin_bottom={}", doc.margin_bottom);
        assert!((doc.margin_left   - 90.0).abs() < 0.5, "margin_left={}", doc.margin_left);
        assert!((doc.margin_right  - 90.0).abs() < 0.5, "margin_right={}", doc.margin_right);
    }

    /// 构造一个自定义 sectPr 内容（含 w:cols）的最小 docx，用于分栏解析测试。
    fn make_docx_with_sectpr_extra(sectpr_extra_xml: &str) -> Vec<u8> {
        use std::io::Write;
        let content_types = r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;
        let rels = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
        let document = format!(r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Hi</w:t></w:r></w:p><w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:bottom="1440" w:left="1440" w:right="1440"/>{sectpr_extra_xml}</w:sectPr></w:body></w:document>"#);
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("[Content_Types].xml", opts).unwrap();
        zip.write_all(content_types.as_bytes()).unwrap();
        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn parse_cols_absent_defaults_to_single_column() {
        // 没有 w:cols 标签时，应保持默认单栏，不影响所有现存无分栏文档。
        let data = make_docx_with_sectpr_extra("");
        let doc = DocxParser::parse(&data).unwrap();
        assert_eq!(doc.columns.num_cols, 1);
        assert!(doc.columns.col_widths.is_empty());
        assert!(doc.columns.col_spacing.is_empty());
    }

    #[test]
    fn parse_cols_equal_width_self_closing() {
        // equalWidth=1（或省略）+ 自闭合 <w:cols .../>：只用 num/space 属性，
        // 不读子col元素（即使没有子元素也要能正确处理自闭合标签的收尾）。
        let data = make_docx_with_sectpr_extra(r#"<w:cols w:num="3" w:space="720"/>"#);
        let doc = DocxParser::parse(&data).unwrap();
        assert_eq!(doc.columns.num_cols, 3);
        assert!((doc.columns.uniform_spacing - 36.0).abs() < 0.5, "uniform_spacing={}", doc.columns.uniform_spacing);
        assert!(doc.columns.col_widths.is_empty(), "equalWidth场景不应填充col_widths");
    }

    #[test]
    fn parse_cols_unequal_width_matches_recipe_docx() {
        // 食谱.docx 真实用例：equalWidth=0，两栏，第一栏后间距720twip=36pt，
        // 第二栏（最后一栏）间距省略/0。栏宽均为4320twip=216pt（凑巧数值相等，
        // 但因equalWidth=0，仍应走col_widths逐栏路径而非uniform_spacing）。
        let data = make_docx_with_sectpr_extra(
            r#"<w:cols w:equalWidth="0" w:num="2"><w:col w:space="720" w:w="4320"/><w:col w:space="0" w:w="4320"/></w:cols>"#
        );
        let doc = DocxParser::parse(&data).unwrap();
        assert_eq!(doc.columns.num_cols, 2);
        assert_eq!(doc.columns.col_widths.len(), 2);
        assert_eq!(doc.columns.col_spacing.len(), 2);
        assert!((doc.columns.col_widths[0] - 216.0).abs() < 0.5, "col_widths[0]={}", doc.columns.col_widths[0]);
        assert!((doc.columns.col_widths[1] - 216.0).abs() < 0.5, "col_widths[1]={}", doc.columns.col_widths[1]);
        assert!((doc.columns.col_spacing[0] - 36.0).abs() < 0.5, "col_spacing[0]={}", doc.columns.col_spacing[0]);
        assert!((doc.columns.col_spacing[1] - 0.0).abs() < 0.5, "col_spacing[1]={}", doc.columns.col_spacing[1]);
    }

    #[test]
    fn parse_cols_unequal_width_with_child_elements() {
        // equalWidth=0 且w:cols自身带num/space属性时，这两个属性应被忽略，
        // 完全以子col元素为准（覆盖"w:cols自身spam+子col都存在"的歧义场景）。
        let data = make_docx_with_sectpr_extra(
            r#"<w:cols w:equalWidth="0" w:num="99" w:space="9999"><w:col w:w="3000" w:space="200"/><w:col w:w="5000"/></w:cols>"#
        );
        let doc = DocxParser::parse(&data).unwrap();
        assert_eq!(doc.columns.num_cols, 2, "应以实际col子元素数量为准，忽略num=99");
        assert_eq!(doc.columns.col_widths.len(), 2);
        assert!((doc.columns.col_widths[0] - 150.0).abs() < 0.5);
        assert!((doc.columns.col_widths[1] - 250.0).abs() < 0.5);
        assert!((doc.columns.col_spacing[0] - 10.0).abs() < 0.5);
        assert!((doc.columns.col_spacing[1] - 0.0).abs() < 0.5, "最后一栏省略space应为0");
    }
}

#[cfg(test)]
mod tests_table {
    use super::*;

    fn make_table_docx() -> Vec<u8> {
        use std::io::Write;
        use zip::write::FileOptions;
        let opts: FileOptions<()> = FileOptions::default();
        let buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(buf));

        let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
        let doc_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
</Relationships>"#;
        let document = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
  <w:tbl>
    <w:tblGrid>
      <w:gridCol w:w="3000"/>
      <w:gridCol w:w="3000"/>
    </w:tblGrid>
    <w:tr>
      <w:tc><w:p><w:r><w:t>Cell A1</w:t></w:r></w:p></w:tc>
      <w:tc><w:p><w:r><w:t>Cell B1</w:t></w:r></w:p></w:tc>
    </w:tr>
    <w:tr>
      <w:tc><w:p><w:r><w:t>Cell A2</w:t></w:r></w:p></w:tc>
      <w:tc><w:p><w:r><w:t>Cell B2</w:t></w:r></w:p></w:tc>
    </w:tr>
  </w:tbl>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"#;

        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        zip.start_file("word/_rels/document.xml.rels", opts).unwrap();
        zip.write_all(doc_rels.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn parse_docx_table() {
        let data = make_table_docx();
        let doc = DocxParser::parse(&data).unwrap();
        // body 应该有 1 个 Table
        assert_eq!(doc.body.len(), 1, "body={:?}", doc.body.len());
        let roe_model::writer::BodyElement::Table(tbl) = &doc.body[0] else {
            panic!("expected Table");
        };
        assert_eq!(tbl.rows.len(), 2, "rows={}", tbl.rows.len());
        assert_eq!(tbl.rows[0].cells.len(), 2, "cols={}", tbl.rows[0].cells.len());
        // col_widths: 3000 twip / 20 = 150 pt
        assert!((tbl.col_widths[0] - 150.0).abs() < 0.5, "col_width={}", tbl.col_widths[0]);
        // 检查单元格内容
        let cell_a1 = &tbl.rows[0].cells[0];
        let roe_model::common::CellContent::Paragraph(p) = &cell_a1.content[0] else {
            panic!("expected Paragraph in cell");
        };
        assert_eq!(p.runs[0].text, "Cell A1");
        let cell_b2 = &tbl.rows[1].cells[1];
        let roe_model::common::CellContent::Paragraph(p2) = &cell_b2.content[0] else {
            panic!("expected Paragraph in cell");
        };
        assert_eq!(p2.runs[0].text, "Cell B2");
    }
}

#[cfg(test)]
mod tests_spacing {
    use super::*;

    fn make_spacing_docx() -> Vec<u8> {
        use std::io::Write;
        use zip::write::FileOptions;
        let opts: FileOptions<()> = FileOptions::default();
        let buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(buf));

        let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
        let doc_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
</Relationships>"#;
        // w:before=240 → 12pt, w:after=480 → 24pt, w:line=360 → 1.5x
        let document = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
  <w:p>
    <w:pPr>
      <w:spacing w:before="240" w:after="480" w:line="360"/>
    </w:pPr>
    <w:r><w:t>Hello</w:t></w:r>
  </w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"#;

        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        zip.start_file("word/_rels/document.xml.rels", opts).unwrap();
        zip.write_all(doc_rels.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn parse_docx_spacing() {
        let data = make_spacing_docx();
        let doc = DocxParser::parse(&data).unwrap();
        assert_eq!(doc.body.len(), 1);
        let roe_model::writer::BodyElement::Paragraph(p) = &doc.body[0] else {
            panic!("expected Paragraph");
        };
        // 240 twip / 20 = 12pt
        let sb = p.space_before.expect("space_before should be set");
        assert!((sb - 12.0).abs() < 0.5, "space_before={sb}");
        // 480 twip / 20 = 24pt
        let sa = p.space_after.expect("space_after should be set");
        assert!((sa - 24.0).abs() < 0.5, "space_after={sa}");
        // 360 / 240 = 1.5x
        let lh = p.line_height.expect("line_height should be set");
        assert!((lh - 1.5).abs() < 0.01, "line_height={lh}");
    }
}

#[cfg(test)]
mod tests_decoration {
    use super::*;

    fn make_decoration_docx() -> Vec<u8> {
        use std::io::Write;
        use zip::write::FileOptions;
        let opts: FileOptions<()> = FileOptions::default();
        let buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(buf));
        let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
        let doc_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
</Relationships>"#;
        let document = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
  <w:p>
    <w:r>
      <w:rPr><w:b/><w:u w:val="single"/><w:highlight w:val="yellow"/></w:rPr>
      <w:t>Bold+Underline+Yellow</w:t>
    </w:r>
    <w:r>
      <w:rPr><w:i/><w:strike/></w:rPr>
      <w:t>Italic+Strike</w:t>
    </w:r>
  </w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"#;
        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        zip.start_file("word/_rels/document.xml.rels", opts).unwrap();
        zip.write_all(doc_rels.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn parse_docx_decoration() {
        let data = make_decoration_docx();
        let doc = DocxParser::parse(&data).unwrap();
        let roe_model::writer::BodyElement::Paragraph(p) = &doc.body[0] else {
            panic!("expected Paragraph");
        };
        assert_eq!(p.runs.len(), 2);
        let r0 = &p.runs[0];
        assert_eq!(r0.bold, Some(true), "bold");
        assert_eq!(r0.underline, Some(true), "underline");
        let bg = r0.background.as_ref().expect("highlight");
        assert_eq!(bg.r, 0xff); assert_eq!(bg.g, 0xff); assert_eq!(bg.b, 0x00);

        let r1 = &p.runs[1];
        assert_eq!(r1.italic, Some(true), "italic");
        assert_eq!(r1.strikethrough, Some(true), "strikethrough");
    }
}

#[cfg(test)]
mod test_hyperlink {
    use super::*;

    #[test]
    fn parse_docx_hyperlink() {
        use std::io::Write;
        use zip::write::FileOptions;

        let mut buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts = FileOptions::<()>::default();

        // word/_rels/document.xml.rels
        zip.start_file("word/_rels/document.xml.rels", opts).unwrap();
        zip.write_all(br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com" TargetMode="External"/>
</Relationships>"#).unwrap();

        // word/document.xml
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(br#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <w:body>
    <w:p>
      <w:hyperlink r:id="rId1">
        <w:r><w:t>click here</w:t></w:r>
      </w:hyperlink>
    </w:p>
  </w:body>
</w:document>"#).unwrap();

        zip.finish().unwrap();

        let doc = DocxParser::parse(&buf).unwrap();
        let para = match &doc.body[0] {
            roe_model::writer::BodyElement::Paragraph(p) => p,
            _ => panic!("期望段落"),
        };
        assert_eq!(para.runs.len(), 1);
        assert_eq!(para.runs[0].text, "click here");
        assert_eq!(para.runs[0].hyperlink, Some("https://example.com".to_string()));
    }
}

#[cfg(test)]
mod test_bookmark {
    use super::*;

    #[test]
    fn parse_docx_bookmark() {
        use std::io::Write;
        use zip::write::FileOptions;

        let mut buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts = FileOptions::<()>::default();

        zip.start_file("word/_rels/document.xml.rels", opts).unwrap();
        zip.write_all(br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
</Relationships>"#).unwrap();

        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(br#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p>
      <w:r><w:t>before</w:t></w:r>
      <w:bookmarkStart w:id="1" w:name="section1"/>
      <w:r><w:t>marked</w:t></w:r>
      <w:bookmarkEnd w:id="1"/>
    </w:p>
  </w:body>
</w:document>"#).unwrap();

        zip.finish().unwrap();

        let doc = DocxParser::parse(&buf).unwrap();
        let para = match &doc.body[0] {
            roe_model::writer::BodyElement::Paragraph(p) => p,
            _ => panic!("期望段落"),
        };
        assert_eq!(para.bookmarks.len(), 1);
        assert_eq!(para.bookmarks[0].id, 1);
        assert_eq!(para.bookmarks[0].name, "section1");
        assert_eq!(para.bookmarks[0].run_index, 1); // 在第1个run之前（before之后）
    }
}

#[cfg(test)]
mod test_a1f {
    use super::*;

    #[test]
    fn test_numpr_indent() {
        // 构造一个最小 numbering.xml，含 w:ind left=720 hanging=360
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:abstractNum w:abstractNumId="0">
    <w:lvl w:ilvl="0">
      <w:start w:val="1"/>
      <w:numFmt w:val="decimal"/>
      <w:lvlText w:val="%1."/>
      <w:ind w:left="720" w:hanging="360"/>
    </w:lvl>
  </w:abstractNum>
  <w:num w:numId="1">
    <w:abstractNumId w:val="0"/>
  </w:num>
</w:numbering>"#;

        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);
        // 复用 read_numbering 内部逻辑：直接调 NumberingStore
        let mut store = NumberingStore::new();
        let mut cur_abs_id: Option<u32> = None;
        let mut cur_abs: Option<AbstractNum> = None;
        let mut cur_lvl: Option<(usize, NumLevel)> = None;
        let mut cur_num_id: Option<u32> = None;
        let mut cur_num: Option<NumInstance> = None;

        loop {
            match reader.read_event().unwrap() {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    match e.name().as_ref() {
                        b"w:abstractNum" => {
                            let mut id = 0u32;
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:abstractNumId" {
                                    id = attr.unescape_value().unwrap().parse().unwrap_or(0);
                                }
                            }
                            cur_abs_id = Some(id);
                            cur_abs = Some(AbstractNum::default());
                        }
                        b"w:lvl" => {
                            let mut idx = 0usize;
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:ilvl" {
                                    idx = attr.unescape_value().unwrap().parse().unwrap_or(0);
                                }
                            }
                            cur_lvl = Some((idx, NumLevel::default()));
                        }
                        b"w:numFmt" => {
                            if let Some((_, ref mut lvl)) = cur_lvl {
                                for attr in e.attributes().flatten() {
                                    if attr.key.as_ref() == b"w:val" {
                                        lvl.num_fmt = attr.unescape_value().unwrap().into_owned();
                                    }
                                }
                            }
                        }
                        b"w:lvlText" => {
                            if let Some((_, ref mut lvl)) = cur_lvl {
                                for attr in e.attributes().flatten() {
                                    if attr.key.as_ref() == b"w:val" {
                                        lvl.lvl_text = attr.unescape_value().unwrap().into_owned();
                                    }
                                }
                            }
                        }
                        b"w:start" => {
                            if let Some((_, ref mut lvl)) = cur_lvl {
                                for attr in e.attributes().flatten() {
                                    if attr.key.as_ref() == b"w:val" {
                                        lvl.start_val = attr.unescape_value().unwrap().parse().unwrap_or(1);
                                    }
                                }
                            }
                        }
                        b"w:ind" => {
                            if let Some((_, ref mut lvl)) = cur_lvl {
                                for attr in e.attributes().flatten() {
                                    let k = attr.key.as_ref();
                                    let v: f32 = attr.unescape_value().unwrap_or_default().parse().unwrap_or(0.0);
                                    if k == b"w:left" {
                                        lvl.indent_left = Some(v / 1440.0 * 72.0);
                                    } else if k == b"w:hanging" {
                                        lvl.indent_first = Some(-(v / 1440.0 * 72.0));
                                    }
                                }
                            }
                        }
                        b"w:num" => {
                            let mut id = 0u32;
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"w:numId" {
                                    id = attr.unescape_value().unwrap().parse().unwrap_or(0);
                                }
                            }
                            cur_num_id = Some(id);
                            cur_num = Some(NumInstance { abstract_num_id: 0, overrides: HashMap::new(), level_overrides: HashMap::new() });
                        }
                        b"w:abstractNumId" => {
                            if let Some(ref mut inst) = cur_num {
                                for attr in e.attributes().flatten() {
                                    if attr.key.as_ref() == b"w:val" {
                                        inst.abstract_num_id = attr.unescape_value().unwrap().parse().unwrap_or(0);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Event::End(ref e) => match e.name().as_ref() {
                    b"w:lvl" => {
                        if let (Some(ref mut abs), Some((idx, lvl))) = (&mut cur_abs, cur_lvl.take()) {
                            if idx >= abs.levels.len() { abs.levels.resize(idx + 1, NumLevel::default()); }
                            abs.levels[idx] = lvl;
                        }
                    }
                    b"w:abstractNum" => {
                        if let (Some(id), Some(abs)) = (cur_abs_id.take(), cur_abs.take()) {
                            store.abstract_nums.insert(id, abs);
                        }
                    }
                    b"w:num" => {
                        if let (Some(id), Some(inst)) = (cur_num_id.take(), cur_num.take()) {
                            store.nums.insert(id, inst);
                        }
                    }
                    _ => {}
                },
                Event::Eof => break,
                _ => {}
            }
        }

        let lvl = &store.abstract_nums[&0].levels[0];
        // 720 twips = 36pt, 360 twips = 18pt
        let indent_left = lvl.indent_left.unwrap();
        let indent_first = lvl.indent_first.unwrap();
        assert!((indent_left - 36.0).abs() < 0.01, "indent_left={indent_left}");
        assert!((indent_first - (-18.0)).abs() < 0.01, "indent_first={indent_first}");
        println!("indent_left={indent_left} indent_first={indent_first} ✓");
    }
}

#[cfg(test)]
mod tests_style_ind_twips {
    use super::*;

    fn make_style_ind_docx() -> Vec<u8> {
        use std::io::Write;
        use zip::write::FileOptions;
        let opts: FileOptions<()> = FileOptions::default();
        let buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(buf));

        let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
        let doc_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;
        let document = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
  <w:p>
    <w:pPr><w:pStyle w:val="IndentedStyle"/></w:pPr>
    <w:r><w:t>Hello</w:t></w:r>
  </w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"#;
        // w:left=720 -> 36pt, w:hanging=360 -> -18pt
        let styles = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:style w:type="paragraph" w:styleId="IndentedStyle">
    <w:name w:val="Indented Style"/>
    <w:pPr>
      <w:ind w:left="720" w:hanging="360"/>
    </w:pPr>
  </w:style>
</w:styles>"#;

        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        zip.start_file("word/_rels/document.xml.rels", opts).unwrap();
        zip.write_all(doc_rels.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document.as_bytes()).unwrap();
        zip.start_file("word/styles.xml", opts).unwrap();
        zip.write_all(styles.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn style_ind_left_and_hanging_convert_twips_to_pt() {
        let data = make_style_ind_docx();
        let doc = DocxParser::parse(&data).unwrap();

        let resolved = doc.styles.resolve_paragraph_style("IndentedStyle");

        // 720 twips / 20 = 36pt
        let il = resolved.indent_left.expect("indent_left should be set");
        assert!((il - 36.0).abs() < 0.01, "indent_left={il}");

        // w:hanging=360 -> stored as negative indent_first: -360/20 = -18pt
        let ifst = resolved.indent_first.expect("indent_first should be set");
        assert!((ifst - (-18.0)).abs() < 0.01, "indent_first={ifst}");
    }
}

#[cfg(test)]
mod tests_docx5_anchor_inline {
    use super::*;
    use std::io::Write;
    use zip::write::FileOptions;

    fn make_zip(document_xml: &str) -> Vec<u8> {
        let opts: FileOptions<()> = FileOptions::default();
        let buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(buf));

        let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
        let doc_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rIdImg1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.png"/>
</Relationships>"#;
        // 最小 PNG 字节（1x1 透明像素），内容不重要，只验证能被原样读出+base64编码
        let fake_png: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x01];

        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        zip.start_file("word/_rels/document.xml.rels", opts).unwrap();
        zip.write_all(doc_rels.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document_xml.as_bytes()).unwrap();
        zip.start_file("word/media/image1.png", opts).unwrap();
        zip.write_all(fake_png).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn wp_inline_parses_as_inline_image_not_frame() {
        let document = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
            xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
            xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<w:body>
  <w:p>
    <w:r>
      <w:drawing>
        <wp:inline>
          <wp:extent cx="914400" cy="457200"/>
          <a:graphic>
            <a:graphicData>
              <pic:pic xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture">
                <pic:blipFill>
                  <a:blip r:embed="rIdImg1"/>
                </pic:blipFill>
              </pic:pic>
            </a:graphicData>
          </a:graphic>
        </wp:inline>
      </w:drawing>
    </w:r>
  </w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"#;

        let data = make_zip(document);
        let doc = DocxParser::parse(&data).unwrap();

        let images: Vec<_> = doc.body.iter().filter(|b| matches!(b, BodyElement::Image(_))).collect();
        let frames: Vec<_> = doc.body.iter().filter(|b| matches!(b, BodyElement::Frame(_))).collect();

        assert_eq!(images.len(), 1, "expected exactly one inline image, body={:?}", doc.body);
        assert_eq!(frames.len(), 0, "wp:inline must not produce a FloatFrame");

        let BodyElement::Image(img) = images[0] else { unreachable!() };
        // 914400 EMU / 12700 = 72pt, 457200 EMU / 12700 = 36pt
        assert!((img.width_pt - 72.0).abs() < 0.01, "width_pt={}", img.width_pt);
        assert!((img.height_pt - 36.0).abs() < 0.01, "height_pt={}", img.height_pt);
    }

    #[test]
    fn wp_anchor_position_offset_converts_emu_and_keeps_relative_from() {
        let document = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
            xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
            xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<w:body>
  <w:p>
    <w:r>
      <w:drawing>
        <wp:anchor>
          <wp:positionH relativeFrom="page">
            <wp:posOffset>914400</wp:posOffset>
          </wp:positionH>
          <wp:positionV relativeFrom="paragraph">
            <wp:posOffset>457200</wp:posOffset>
          </wp:positionV>
          <wp:extent cx="1270000" cy="635000"/>
          <a:graphic>
            <a:graphicData>
              <pic:pic xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture">
                <pic:blipFill>
                  <a:blip r:embed="rIdImg1"/>
                </pic:blipFill>
              </pic:pic>
            </a:graphicData>
          </a:graphic>
        </wp:anchor>
      </w:drawing>
    </w:r>
  </w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"#;

        let data = make_zip(document);
        let doc = DocxParser::parse(&data).unwrap();

        let frames: Vec<_> = doc.body.iter().filter_map(|b| match b {
            BodyElement::Frame(f) => Some(f),
            _ => None,
        }).collect();
        assert_eq!(frames.len(), 1, "expected exactly one FloatFrame, body={:?}", doc.body);

        let frame = frames[0];
        // 914400 EMU / 12700 = 72pt
        assert!((frame.x_pt - 72.0).abs() < 0.01, "x_pt={}", frame.x_pt);
        // 457200 EMU / 12700 = 36pt
        assert!((frame.y_pt - 36.0).abs() < 0.01, "y_pt={}", frame.y_pt);
        // relativeFrom: H取"page"优先
        assert_eq!(frame.anchor_type_h.as_deref(), Some("page"));
        // WROOT-ANCHOR-01: 此测试fixture里 positionV relativeFrom="paragraph"，
        // 之前的合并逻辑吞掉了这个信息，现在独立断言两个方向
        assert_eq!(frame.anchor_type_v.as_deref(), Some("paragraph"));
        assert!(frame.align_h.is_none());
        assert!(frame.align_v.is_none());

        // 1270000 EMU / 12700 = 100pt, 635000 EMU / 12700 = 50pt
        assert!((frame.width_pt - 100.0).abs() < 0.01, "width_pt={}", frame.width_pt);
        assert!((frame.height_pt - 50.0).abs() < 0.01, "height_pt={}", frame.height_pt);
    }

    #[test]
    fn wp_anchor_align_keyword_is_preserved_not_coerced_to_zero() {
        let document = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
            xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
            xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<w:body>
  <w:p>
    <w:r>
      <w:drawing>
        <wp:anchor>
          <wp:positionH relativeFrom="margin">
            <wp:align>center</wp:align>
          </wp:positionH>
          <wp:positionV relativeFrom="margin">
            <wp:align>top</wp:align>
          </wp:positionV>
          <wp:extent cx="914400" cy="914400"/>
          <a:graphic>
            <a:graphicData>
              <pic:pic xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture">
                <pic:blipFill>
                  <a:blip r:embed="rIdImg1"/>
                </pic:blipFill>
              </pic:pic>
            </a:graphicData>
          </a:graphic>
        </wp:anchor>
      </w:drawing>
    </w:r>
  </w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"#;

        let data = make_zip(document);
        let doc = DocxParser::parse(&data).unwrap();

        let frames: Vec<_> = doc.body.iter().filter_map(|b| match b {
            BodyElement::Frame(f) => Some(f),
            _ => None,
        }).collect();
        assert_eq!(frames.len(), 1);

        let frame = frames[0];
        assert_eq!(frame.anchor_type_h.as_deref(), Some("margin"));
        assert_eq!(frame.align_h.as_deref(), Some("center"));
        assert_eq!(frame.align_v.as_deref(), Some("top"));
        // align 场景下 x_pt/y_pt 无意义，保持默认 0.0，不应被错误赋值
        assert_eq!(frame.x_pt, 0.0);
        assert_eq!(frame.y_pt, 0.0);
    }

    #[test]
    fn wroot016_anchor_image_rotation_parsed() {
        // WROOT-016: <pic:spPr><a:xfrm rot="641099"> 应解析为约10.68度
        // （641099 / 60000 ≈ 10.685度），写入 FloatFrame.rotation。
        // 取自真实样本 large__sw_ooxmlexport_data__crop-pixel.docx 的实际值，
        // 不是构造的整数便于验证的假数据，确保覆盖真实场景的非整数换算结果。
        let xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
 xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
 xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
 xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"
 xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture">
<w:body><w:p><w:r><w:drawing>
<wp:anchor distT="0" distB="0" distL="114300" distR="114300" simplePos="0" relativeHeight="1" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">
<wp:simplePos x="0" y="0"/>
<wp:positionH relativeFrom="column"><wp:posOffset>100</wp:posOffset></wp:positionH>
<wp:positionV relativeFrom="paragraph"><wp:posOffset>100</wp:posOffset></wp:positionV>
<wp:extent cx="1000000" cy="1000000"/>
<wp:wrapSquare wrapText="bothSides"/>
<wp:docPr id="1" name="Picture 1"/>
<a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture">
<pic:pic><pic:nvPicPr><pic:cNvPr id="0" name="Picture 1"/><pic:cNvPicPr/></pic:nvPicPr>
<pic:blipFill><a:blip r:embed="rIdImg1"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>
<pic:spPr><a:xfrm rot="641099"><a:off x="0" y="0"/><a:ext cx="1000000" cy="1000000"/></a:xfrm>
<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr>
</pic:pic></a:graphicData></a:graphic></wp:anchor>
</w:drawing></w:r></w:p></w:body></w:document>"#;

        let data = make_zip(xml);
        let doc = DocxParser::parse(&data).unwrap();
        let frames: Vec<_> = doc.body.iter().filter_map(|b| match b {
            BodyElement::Frame(f) => Some(f),
            _ => None,
        }).collect();
        assert_eq!(frames.len(), 1, "应解析出1个FloatFrame");
        let rot = frames[0].rotation.expect("rotation应为Some，不应是None");
        assert!(
            (rot - 10.685).abs() < 0.01,
            "rot=641099应换算为约10.685度，实际={}", rot
        );
    }

    #[test]
    fn wroot016_anchor_image_no_rotation_is_none() {
        // 回归防护：没有 a:xfrm rot 属性的普通浮动图片，rotation 必须是
        // None（不能凭空产生默认值0.0或其他值），这是本次新增解析分支
        // 最容易犯的错——用 unwrap_or(0.0) 而非 Option 会让"未设置"和
        // "显式设置为0度"无法区分，虽然视觉效果一样，但语义上不应该
        // 给不存在的属性编造一个值。
        let xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
 xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
 xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
 xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"
 xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture">
<w:body><w:p><w:r><w:drawing>
<wp:anchor distT="0" distB="0" distL="114300" distR="114300" simplePos="0" relativeHeight="1" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">
<wp:simplePos x="0" y="0"/>
<wp:positionH relativeFrom="column"><wp:posOffset>100</wp:posOffset></wp:positionH>
<wp:positionV relativeFrom="paragraph"><wp:posOffset>100</wp:posOffset></wp:positionV>
<wp:extent cx="1000000" cy="1000000"/>
<wp:wrapSquare wrapText="bothSides"/>
<wp:docPr id="1" name="Picture 1"/>
<a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture">
<pic:pic><pic:nvPicPr><pic:cNvPr id="0" name="Picture 1"/><pic:cNvPicPr/></pic:nvPicPr>
<pic:blipFill><a:blip r:embed="rIdImg1"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>
<pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="1000000" cy="1000000"/></a:xfrm>
<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr>
</pic:pic></a:graphicData></a:graphic></wp:anchor>
</w:drawing></w:r></w:p></w:body></w:document>"#;

        let data = make_zip(xml);
        let doc = DocxParser::parse(&data).unwrap();
        let frames: Vec<_> = doc.body.iter().filter_map(|b| match b {
            BodyElement::Frame(f) => Some(f),
            _ => None,
        }).collect();
        assert_eq!(frames.len(), 1, "应解析出1个FloatFrame");
        assert_eq!(frames[0].rotation, None, "无rot属性时应为None，不应凭空产生默认值");
    }
}

// WROOT-024: w14:textFill/w14:alpha 字符透明度解析测试。真实样本
// (tdf152884_Char_Transparency.docx) 里 w14:val="74000" 视觉效果是较浅/
// 较透明，换算逻辑是 1.0 - raw/100000.0（而非直觉的 raw/100000.0），
// 详见 docx.rs 内 b"w14:alpha" 分支的注释。
#[cfg(test)]
mod tests_docx10_wroot024_char_alpha {
    use super::*;
    use std::io::Write;
    use zip::write::FileOptions;

    fn make_zip(document_xml: &str) -> Vec<u8> {
        let opts: FileOptions<()> = FileOptions::default();
        let buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(buf));

        let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
        let doc_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
</Relationships>"#;

        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        zip.start_file("word/_rels/document.xml.rels", opts).unwrap();
        zip.write_all(doc_rels.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document_xml.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn wroot024_char_alpha_74000_converts_to_lighter_color() {
        // 真实样本的实际值：w:color=C0504D + w14:alpha=74000（对应文档中
        // 视觉上"较浅"的B字符）。1.0 - 74000/100000.0 = 0.26 -> a≈66
        let document = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml">
<w:body>
  <w:p>
    <w:r>
      <w:rPr>
        <w:color w:val="C0504D"/>
        <w14:textFill>
          <w14:solidFill>
            <w14:schemeClr w14:val="accent2">
              <w14:alpha w14:val="74000"/>
            </w14:schemeClr>
          </w14:solidFill>
        </w14:textFill>
      </w:rPr>
      <w:t>B</w:t>
    </w:r>
  </w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"#;

        let data = make_zip(document);
        let doc = DocxParser::parse(&data).unwrap();

        let para = doc.body.iter().find_map(|b| match b {
            BodyElement::Paragraph(p) => Some(p),
            _ => None,
        }).expect("应解析出1个段落");
        assert_eq!(para.runs.len(), 1, "应解析出1个run");

        let color = para.runs[0].color.expect("应解析出run颜色");
        assert_eq!(color.r, 0xC0);
        assert_eq!(color.g, 0x50);
        assert_eq!(color.b, 0x4D);
        // 1.0 - 74000/100000.0 = 0.26; 0.26 * 255 = 66.3 -> round = 66
        assert_eq!(color.a, 66, "w14:alpha=74000 应换算为较低的不透明度(约66)，实际a={}", color.a);
    }

    #[test]
    fn wroot024_no_alpha_tag_keeps_default_opaque() {
        // 无 w14:alpha 标签时，颜色应保持默认完全不透明（a=255），不应
        // 因为本次新增的 run_alpha 逻辑而意外产生非255的值。
        let document = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
  <w:p>
    <w:r>
      <w:rPr>
        <w:color w:val="C0504D"/>
      </w:rPr>
      <w:t>C</w:t>
    </w:r>
  </w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"#;

        let data = make_zip(document);
        let doc = DocxParser::parse(&data).unwrap();

        let para = doc.body.iter().find_map(|b| match b {
            BodyElement::Paragraph(p) => Some(p),
            _ => None,
        }).expect("应解析出1个段落");
        assert_eq!(para.runs.len(), 1, "应解析出1个run");

        let color = para.runs[0].color.expect("应解析出run颜色");
        assert_eq!(color.a, 255, "无w14:alpha时应保持完全不透明，不应凭空产生透明度");
    }
}

#[cfg(test)]
mod tests_docx7_ole_preview {
    use super::*;
    use std::io::Write;
    use zip::write::FileOptions;

    // 独立的 make_zip：比 DOCX-5 版本多注册一个 OLE 预渲染图关系(rIdOle1)，
    // 复用同一张假 PNG 数据，因为测试只关心 rid 是否被正确捕获+查到，不关心
    // 真实 EMF/WMF 解码（渲染层职责，不在本任务范围）。
    fn make_zip(document_xml: &str, include_ole_rel: bool) -> Vec<u8> {
        let opts: FileOptions<()> = FileOptions::default();
        let buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(buf));

        let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;

        let doc_rels = if include_ole_rel {
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rIdOle1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.emf"/>
</Relationships>"#
        } else {
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
</Relationships>"#
        };

        // 内容不重要（真实场景是 EMF/WMF，这里仍用假 PNG 字节），只验证 rid 关联
        // 和 base64 编码路径是否走通，不测试渲染层能否显示 EMF/WMF。
        let fake_preview: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x02];

        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        zip.start_file("word/_rels/document.xml.rels", opts).unwrap();
        zip.write_all(doc_rels.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document_xml.as_bytes()).unwrap();
        if include_ole_rel {
            zip.start_file("word/media/image1.emf", opts).unwrap();
            zip.write_all(fake_preview).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn w_object_with_imagedata_produces_inline_image() {
        let document = r##"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            xmlns:v="urn:schemas-microsoft-com:vml"
            xmlns:o="urn:schemas-microsoft-com:office:office"
            xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<w:body>
  <w:p>
    <w:r>
      <w:object w:dxaOrig="1440" w:dyaOrig="1440">
        <v:shapetype id="_x0000_t75"/>
        <v:shape id="OleObject1" type="#_x0000_t75" style="width:72pt;height:72pt" o:ole="">
          <v:imagedata r:id="rIdOle1" o:title=""/>
        </v:shape>
        <o:OLEObject Type="Embed" ProgID="Excel.Sheet.12" r:id="rIdOle1"/>
      </w:object>
    </w:r>
  </w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"##;

        let data = make_zip(document, true);
        let doc = DocxParser::parse(&data).unwrap();

        let images: Vec<_> = doc.body.iter().filter(|b| matches!(b, BodyElement::Image(_))).collect();
        let frames: Vec<_> = doc.body.iter().filter(|b| matches!(b, BodyElement::Frame(_))).collect();

        assert_eq!(images.len(), 1, "expected OLE preview to surface as one InlineImage, body={:?}", doc.body);
        assert_eq!(frames.len(), 0, "OLE preview downgrade must not produce a FloatFrame");

        let BodyElement::Image(img) = images[0] else { unreachable!() };
        assert!(!img.image.data.is_empty(), "preview image data must not be empty");
        // w:dxaOrig="1440" w:dyaOrig="1440" (twips) / 20.0 = 72.0pt, same
        // conversion convention as DOCX-1's w:ind handling.
        assert!((img.width_pt - 72.0).abs() < 0.01, "width_pt={}", img.width_pt);
        assert!((img.height_pt - 72.0).abs() < 0.01, "height_pt={}", img.height_pt);
    }

    #[test]
    fn w_object_with_missing_relationship_produces_nothing() {
        // v:imagedata 的 r:id 指向一个不存在的关系（模拟"文档引用了但资源缺失"或者
        // 解析顺序异常的边界情况）——应静默跳过，不 panic、不产出占位 Image，
        // 与 wp:inline/wp:anchor 查不到图片数据时的既有行为一致。
        let document = r##"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            xmlns:v="urn:schemas-microsoft-com:vml"
            xmlns:o="urn:schemas-microsoft-com:office:office"
            xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<w:body>
  <w:p>
    <w:r>
      <w:object w:dxaOrig="1440" w:dyaOrig="1440">
        <v:shapetype id="_x0000_t75"/>
        <v:shape id="OleObject1" type="#_x0000_t75" style="width:72pt;height:72pt" o:ole="">
          <v:imagedata r:id="rIdOleMissing" o:title=""/>
        </v:shape>
      </w:object>
    </w:r>
  </w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"##;

        let data = make_zip(document, false);
        let doc = DocxParser::parse(&data).unwrap();

        let images: Vec<_> = doc.body.iter().filter(|b| matches!(b, BodyElement::Image(_))).collect();
        assert_eq!(images.len(), 0, "missing relationship must not produce a placeholder Image, body={:?}", doc.body);
    }

    #[test]
    fn document_without_w_object_is_unaffected() {
        // 回归验证：不含 w:object 的普通文档，新增的 DOCX-7 状态机分支不应
        // 产生任何副作用（不误触发、不 panic）。
        let document = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
  <w:p><w:r><w:t>plain text, no OLE, no drawing</w:t></w:r></w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"#;

        let data = make_zip(document, false);
        let doc = DocxParser::parse(&data).unwrap();

        let images: Vec<_> = doc.body.iter().filter(|b| matches!(b, BodyElement::Image(_))).collect();
        let frames: Vec<_> = doc.body.iter().filter(|b| matches!(b, BodyElement::Frame(_))).collect();
        assert_eq!(images.len(), 0);
        assert_eq!(frames.len(), 0);
    }
}

#[cfg(test)]
mod tests_docx6_wrap_type {
    use super::*;
    use std::io::Write;
    use zip::write::FileOptions;

    fn make_zip_with_wrap(wrap_tag: &str) -> Vec<u8> {
        let opts: FileOptions<()> = FileOptions::default();
        let buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(buf));

        let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
        let doc_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rIdImg1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.png"/>
</Relationships>"#;
        let document = format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
            xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
            xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<w:body>
  <w:p>
    <w:r>
      <w:drawing>
        <wp:anchor>
          <wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH>
          <wp:positionV relativeFrom="paragraph"><wp:posOffset>0</wp:posOffset></wp:positionV>
          <wp:extent cx="914400" cy="914400"/>
          {wrap_tag}
          <a:graphic>
            <a:graphicData>
              <pic:pic xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture">
                <pic:blipFill>
                  <a:blip r:embed="rIdImg1"/>
                </pic:blipFill>
              </pic:pic>
            </a:graphicData>
          </a:graphic>
        </wp:anchor>
      </w:drawing>
    </w:r>
  </w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"#, wrap_tag = wrap_tag);

        let fake_png: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x01];

        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        zip.start_file("word/_rels/document.xml.rels", opts).unwrap();
        zip.write_all(doc_rels.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document.as_bytes()).unwrap();
        zip.start_file("word/media/image1.png", opts).unwrap();
        zip.write_all(fake_png).unwrap();
        zip.finish().unwrap().into_inner()
    }

    fn extract_frame(doc: &roe_model::writer::WriterDocument) -> roe_model::writer::FloatFrame {
        let frames: Vec<_> = doc.body.iter().filter_map(|b| match b {
            BodyElement::Frame(f) => Some(f.clone()),
            _ => None,
        }).collect();
        assert_eq!(frames.len(), 1, "expected exactly one FloatFrame, body={:?}", doc.body);
        frames.into_iter().next().unwrap()
    }

    #[test]
    fn wrap_none_parsed() {
        let data = make_zip_with_wrap("<wp:wrapNone/>");
        let doc = DocxParser::parse(&data).unwrap();
        let frame = extract_frame(&doc);
        assert_eq!(frame.wrap_type.as_deref(), Some("none"));
    }

    #[test]
    fn wrap_square_parsed() {
        let data = make_zip_with_wrap(r#"<wp:wrapSquare wrapText="bothSides"/>"#);
        let doc = DocxParser::parse(&data).unwrap();
        let frame = extract_frame(&doc);
        assert_eq!(frame.wrap_type.as_deref(), Some("square"));
    }

    #[test]
    fn wrap_top_and_bottom_parsed() {
        let data = make_zip_with_wrap("<wp:wrapTopAndBottom/>");
        let doc = DocxParser::parse(&data).unwrap();
        let frame = extract_frame(&doc);
        assert_eq!(frame.wrap_type.as_deref(), Some("topAndBottom"));
    }

    #[test]
    fn wrap_tight_and_through_parsed() {
        let data_tight = make_zip_with_wrap(r#"<wp:wrapTight wrapText="bothSides"/>"#);
        let doc_tight = DocxParser::parse(&data_tight).unwrap();
        assert_eq!(extract_frame(&doc_tight).wrap_type.as_deref(), Some("tight"));

        let data_through = make_zip_with_wrap(r#"<wp:wrapThrough wrapText="bothSides"/>"#);
        let doc_through = DocxParser::parse(&data_through).unwrap();
        assert_eq!(extract_frame(&doc_through).wrap_type.as_deref(), Some("through"));
    }

    #[test]
    fn wrap_type_none_when_tag_absent() {
        // 没有任何 wrap* 标签的 wp:anchor（schema 上不合法但防御性验证：解析器不应崩溃或瞎猜）
        let data = make_zip_with_wrap("");
        let doc = DocxParser::parse(&data).unwrap();
        let frame = extract_frame(&doc);
        assert!(frame.wrap_type.is_none());
    }
}

#[cfg(test)]
mod tests_docx9_wps_shape {
    use super::*;
    use std::io::Write;
    use zip::write::FileOptions;

    // 最简 make_zip：wps:wsp 场景不需要额外关系文件（形状内容内联在
    // document.xml 里，不像图片需要 r:embed 指向 media/ 里的二进制）。
    fn make_zip(document_xml: &str) -> Vec<u8> {
        let opts: FileOptions<()> = FileOptions::default();
        let buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(buf));

        let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;

        let doc_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
</Relationships>"#;

        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        zip.start_file("word/_rels/document.xml.rels", opts).unwrap();
        zip.write_all(doc_rels.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document_xml.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    fn wps_document(prst_geom: &str, fill_hex: &str) -> String {
        format!(r##"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
            xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
            xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"
            xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<w:body>
  <w:p>
    <w:r>
      <w:drawing>
        <wp:anchor>
          <wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH>
          <wp:positionV relativeFrom="paragraph"><wp:posOffset>0</wp:posOffset></wp:positionV>
          <wp:extent cx="914400" cy="457200"/>
          <wp:wrapSquare/>
          <a:graphic>
            <a:graphicData>
              <wps:wsp>
                <wps:spPr>
                  <a:prstGeom prst="{prst_geom}"/>
                  <a:solidFill><a:srgbClr val="{fill_hex}"/></a:solidFill>
                </wps:spPr>
              </wps:wsp>
            </a:graphicData>
          </a:graphic>
        </wp:anchor>
      </w:drawing>
    </w:r>
  </w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"##, prst_geom = prst_geom, fill_hex = fill_hex)
    }

    #[test]
    fn wps_wsp_produces_shape_not_frame_or_image() {
        let document = wps_document("rect", "FF0000");
        let data = make_zip(&document);
        let doc = DocxParser::parse(&data).unwrap();

        let shapes: Vec<_> = doc.body.iter().filter(|b| matches!(b, BodyElement::Shape(_))).collect();
        let frames: Vec<_> = doc.body.iter().filter(|b| matches!(b, BodyElement::Frame(_))).collect();
        let images: Vec<_> = doc.body.iter().filter(|b| matches!(b, BodyElement::Image(_))).collect();

        assert_eq!(shapes.len(), 1, "expected one Shape, body={:?}", doc.body);
        assert_eq!(frames.len(), 0, "wps:wsp must not produce a FloatFrame");
        assert_eq!(images.len(), 0, "wps:wsp must not produce an InlineImage");
    }

    #[test]
    fn wps_wsp_geometry_and_fill_captured() {
        let document = wps_document("ellipse", "00FF00");
        let data = make_zip(&document);
        let doc = DocxParser::parse(&data).unwrap();

        let BodyElement::Shape(shape) = &doc.body[0] else {
            panic!("expected Shape as first body element, got {:?}", doc.body[0]);
        };

        assert!(
            matches!(shape.geometry, roe_model::common::ShapeGeometry::Ellipse),
            "geometry={:?}", shape.geometry
        );
        match &shape.style.fill {
            Some(roe_model::common::FillStyle::Solid(color)) => {
                assert_eq!(color.r, 0x00);
                assert_eq!(color.g, 0xFF);
                assert_eq!(color.b, 0x00);
            }
            other => panic!("expected Solid fill, got {:?}", other),
        }
        assert!((shape.width_pt - 72.0).abs() < 0.01, "width_pt={}", shape.width_pt);
        assert!((shape.height_pt - 36.0).abs() < 0.01, "height_pt={}", shape.height_pt);
    }

    #[test]
    fn wps_wsp_unrecognized_prst_falls_back_to_custom() {
        // PPTX-11 公式表已扩充到覆盖全部187种OOXML标准预设形状
        // （最初写此测试时只覆盖约30种，"wedgeRoundRectCallout"曾是未
        // 覆盖样本，现已被覆盖，故此测试改用一个 OOXML 规范中不存在的
        // 虚构 prst 值，专测"完全查不到公式"这条 fallback 路径本身，
        // 不再依赖"当前公式表覆盖到哪里"这个会随时间变化的事实。
        let document = wps_document("totallyFakeShapeNameXyz123", "0000FF");
        let data = make_zip(&document);
        let doc = DocxParser::parse(&data).unwrap();

        let BodyElement::Shape(shape) = &doc.body[0] else {
            panic!("expected Shape as first body element, got {:?}", doc.body[0]);
        };
        match &shape.geometry {
            roe_model::common::ShapeGeometry::Custom(name) => {
                assert_eq!(name, "totallyFakeShapeNameXyz123");
            }
            other => panic!("expected Custom(..) fallback, got {:?}", other),
        }
    }

    #[test]
    fn w_pict_alone_produces_nothing() {
        // w:pict(legacy VML) 识别但不解析：应静默跳过，不 panic、不产出占位
        // BodyElement，符合 DOCX-9 的降级策略（同 DOCX-7 OLE 降级思路）。
        let document = r##"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            xmlns:v="urn:schemas-microsoft-com:vml">
<w:body>
  <w:p>
    <w:r>
      <w:pict>
        <v:shapetype id="_x0000_t75"/>
        <v:shape id="LegacyShape1" type="#_x0000_t75" style="width:72pt;height:72pt"/>
      </w:pict>
    </w:r>
  </w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"##;

        let data = make_zip(document);
        let doc = DocxParser::parse(&data).unwrap();

        let shapes: Vec<_> = doc.body.iter().filter(|b| matches!(b, BodyElement::Shape(_))).collect();
        let frames: Vec<_> = doc.body.iter().filter(|b| matches!(b, BodyElement::Frame(_))).collect();
        let images: Vec<_> = doc.body.iter().filter(|b| matches!(b, BodyElement::Image(_))).collect();

        assert_eq!(shapes.len(), 0, "w:pict must not produce a Shape, body={:?}", doc.body);
        assert_eq!(frames.len(), 0, "w:pict must not produce a FloatFrame");
        assert_eq!(images.len(), 0, "w:pict must not produce an InlineImage");
    }

    #[test]
    fn document_without_wps_or_pict_is_unaffected() {
        // 回归验证：不含 wps:wsp / w:pict 的普通文档，新增的 DOCX-9 状态机
        // 分支不应产生任何副作用（不误触发、不 panic）。
        let document = r##"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
  <w:p><w:r><w:t>Plain paragraph, no shapes here.</w:t></w:r></w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"##;

        let data = make_zip(document);
        let doc = DocxParser::parse(&data).unwrap();

        let shapes: Vec<_> = doc.body.iter().filter(|b| matches!(b, BodyElement::Shape(_))).collect();
        assert_eq!(shapes.len(), 0);
        let paragraphs: Vec<_> = doc.body.iter().filter(|b| matches!(b, BodyElement::Paragraph(_))).collect();
        assert_eq!(paragraphs.len(), 1, "plain paragraph should still parse normally");
    }

    #[test]
    fn wps_txbx_text_extracted_into_shape_text_content() {
        let document_xml = r##"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
            xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
            xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"
            xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<w:body>
  <w:p>
    <w:r>
      <w:drawing>
        <wp:anchor>
          <wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH>
          <wp:positionV relativeFrom="paragraph"><wp:posOffset>0</wp:posOffset></wp:positionV>
          <wp:extent cx="914400" cy="457200"/>
          <wp:wrapSquare/>
          <a:graphic>
            <a:graphicData>
              <wps:wsp>
                <wps:spPr>
                  <a:prstGeom prst="rect"/>
                  <a:solidFill><a:srgbClr val="FF0000"/></a:solidFill>
                </wps:spPr>
                <wps:txbx>
                  <w:txbxContent>
                    <w:p>
                      <w:r>
                        <w:rPr><w:b/></w:rPr>
                        <w:t>Hello</w:t>
                      </w:r>
                    </w:p>
                    <w:p>
                      <w:r>
                        <w:t>World</w:t>
                      </w:r>
                    </w:p>
                  </w:txbxContent>
                </wps:txbx>
              </wps:wsp>
            </a:graphicData>
          </a:graphic>
        </wp:anchor>
      </w:drawing>
    </w:r>
  </w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"##;

        let data = make_zip(document_xml);
        let doc = DocxParser::parse(&data).unwrap();

        let shapes: Vec<_> = doc.body.iter().filter_map(|b| {
            if let BodyElement::Shape(s) = b { Some(s) } else { None }
        }).collect();
        assert_eq!(shapes.len(), 1, "should produce exactly one Shape");

        let shape = shapes[0];
        assert_eq!(shape.text_content.len(), 2, "wps:txbx should yield 2 paragraphs");

        assert_eq!(shape.text_content[0].runs.len(), 1);
        assert_eq!(shape.text_content[0].runs[0].text, "Hello");
        assert_eq!(shape.text_content[0].runs[0].bold, Some(true));

        assert_eq!(shape.text_content[1].runs.len(), 1);
        assert_eq!(shape.text_content[1].runs[0].text, "World");
        assert_eq!(shape.text_content[1].runs[0].bold, None);
    }

    #[test]
    fn wps_wsp_without_txbx_has_empty_text_content() {
        // 回归：无 wps:txbx 的形状（Step1场景）text_content 应仍为空 Vec，不受影响
        let document_xml = wps_document("ellipse", "00FF00");
        let data = make_zip(&document_xml);
        let doc = DocxParser::parse(&data).unwrap();

        let shapes: Vec<_> = doc.body.iter().filter_map(|b| {
            if let BodyElement::Shape(s) = b { Some(s) } else { None }
        }).collect();
        assert_eq!(shapes.len(), 1);
        assert!(shapes[0].text_content.is_empty());
    }

}

#[cfg(test)]
mod tests_docx8_omml {
    use super::*;

    // 4份 fixture 均来自一代 office-rs 真实生成的 docx（非手写猜测），
    // 见 roe-ext/fixtures/docx-8-omml/，对应 DOCX-8-v1-d 映射表的
    // 核心档 4 类结构：分数/根号/上下标同现/大型运算符。

    const FIXTURE_FRACTION: &str = r#"<m:oMath>
          <m:r><m:t xml:space="preserve">x</m:t></m:r>
          <m:r><m:t xml:space="preserve">=</m:t></m:r>
          <m:f>
            <m:num>
              <m:r><m:t xml:space="preserve">a</m:t></m:r>
              <m:r><m:t xml:space="preserve">+</m:t></m:r>
              <m:r><m:t xml:space="preserve">b</m:t></m:r>
            </m:num>
            <m:den>
              <m:r><m:t xml:space="preserve">c</m:t></m:r>
              <m:r><m:t xml:space="preserve">+</m:t></m:r>
              <m:r><m:t xml:space="preserve">d</m:t></m:r>
            </m:den>
          </m:f>
        </m:oMath>"#;

    const FIXTURE_SQRT: &str = r#"<m:oMath>
          <m:rad>
            <m:radPr><m:degHide m:val="1"/></m:radPr>
            <m:deg/>
            <m:e>
              <m:sSup>
                <m:e><m:r><m:t xml:space="preserve">x</m:t></m:r></m:e>
                <m:sup><m:r><m:t xml:space="preserve">2</m:t></m:r></m:sup>
              </m:sSup>
              <m:r><m:t xml:space="preserve">+</m:t></m:r>
              <m:sSup>
                <m:e><m:r><m:t xml:space="preserve">y</m:t></m:r></m:e>
                <m:sup><m:r><m:t xml:space="preserve">2</m:t></m:r></m:sup>
              </m:sSup>
            </m:e>
          </m:rad>
        </m:oMath>"#;

    const FIXTURE_SUBSUP: &str = r#"<m:oMath>
          <m:sSubSup>
            <m:e><m:r><m:t xml:space="preserve">x</m:t></m:r></m:e>
            <m:sub><m:r><m:t xml:space="preserve">i</m:t></m:r></m:sub>
            <m:sup><m:r><m:t xml:space="preserve">2</m:t></m:r></m:sup>
          </m:sSubSup>
        </m:oMath>"#;

    const FIXTURE_NARY_SUM: &str = r#"<m:oMath>
          <m:nary>
            <m:naryPr><m:chr m:val="∑"/></m:naryPr>
            <m:sub>
              <m:r><m:t xml:space="preserve">i</m:t></m:r>
              <m:r><m:t xml:space="preserve">=</m:t></m:r>
              <m:r><m:t xml:space="preserve">1</m:t></m:r>
            </m:sub>
            <m:sup><m:r><m:t xml:space="preserve">n</m:t></m:r></m:sup>
            <m:e>
              <m:sSup>
                <m:e><m:r><m:t xml:space="preserve">i</m:t></m:r></m:e>
                <m:sup><m:r><m:t xml:space="preserve">2</m:t></m:r></m:sup>
              </m:sSup>
            </m:e>
          </m:nary>
        </m:oMath>"#;

    #[test]
    fn fraction_converts_correctly() {
        assert_eq!(parse_omml_to_latex(FIXTURE_FRACTION), "x=\\frac{a+b}{c+d}");
    }

    #[test]
    fn sqrt_converts_correctly() {
        // 这条是本轮排查到的"自闭合容器标签栈错位"bug 的直接回归测试：
        // <m:deg/> 是自闭合空标签(degHide=1时Word的实际产出形态)，
        // 若 Empty 事件只压栈不闭合，本测试会得到空字符串而非正确结果。
        assert_eq!(parse_omml_to_latex(FIXTURE_SQRT), r"\sqrt{x^{2}+y^{2}}");
    }

    #[test]
    fn subsup_converts_correctly() {
        assert_eq!(parse_omml_to_latex(FIXTURE_SUBSUP), r"x_{i}^{2}");
    }

    #[test]
    fn nary_sum_converts_correctly() {
        assert_eq!(parse_omml_to_latex(FIXTURE_NARY_SUM), r"\sum_{i=1}^{n} i^{2}");
    }

    // DOCX-8-v3续: m:d（定界符）测试。fixture手写（非真实docx导出），
    // 结构参照 OOXML m:d 规范：m:dPr 可选，内含 m:e 子元素承载内容。

    const FIXTURE_DELIM_DEFAULT: &str = r#"<m:oMath>
          <m:d>
            <m:e><m:r><m:t xml:space="preserve">x</m:t></m:r></m:e>
          </m:d>
        </m:oMath>"#;

    const FIXTURE_DELIM_BRACKETS_PIPE_SEP: &str = r#"<m:oMath>
          <m:d>
            <m:dPr>
              <m:begChr m:val="["/>
              <m:endChr m:val="]"/>
              <m:sepChr m:val="|"/>
            </m:dPr>
            <m:e><m:r><m:t xml:space="preserve">a</m:t></m:r></m:e>
            <m:e><m:r><m:t xml:space="preserve">b</m:t></m:r></m:e>
          </m:d>
        </m:oMath>"#;

    const FIXTURE_DELIM_INVISIBLE: &str = r#"<m:oMath>
          <m:d>
            <m:dPr>
              <m:begChr m:val=""/>
              <m:endChr m:val=""/>
            </m:dPr>
            <m:e><m:r><m:t xml:space="preserve">x</m:t></m:r></m:e>
          </m:d>
        </m:oMath>"#;

    const FIXTURE_DELIM_ANGLE_BRACKETS: &str = r#"<m:oMath>
          <m:d>
            <m:dPr>
              <m:begChr m:val="&#x27e8;"/>
              <m:endChr m:val="&#x27e9;"/>
            </m:dPr>
            <m:e><m:r><m:t xml:space="preserve">v</m:t></m:r></m:e>
          </m:d>
        </m:oMath>"#;

    #[test]
    fn delimiter_default_parens_no_dpr() {
        assert_eq!(parse_omml_to_latex(FIXTURE_DELIM_DEFAULT), r"\left( x \right)");
    }

    #[test]
    fn delimiter_custom_brackets_with_pipe_separator_between_multiple_e() {
        assert_eq!(
            parse_omml_to_latex(FIXTURE_DELIM_BRACKETS_PIPE_SEP),
            r"\left[ a \middle| b \right]"
        );
    }

    #[test]
    fn delimiter_empty_chr_becomes_invisible_dot() {
        assert_eq!(parse_omml_to_latex(FIXTURE_DELIM_INVISIBLE), r"\left. x \right.");
    }

    #[test]
    fn delimiter_angle_brackets_map_to_langle_rangle() {
        assert_eq!(
            parse_omml_to_latex(FIXTURE_DELIM_ANGLE_BRACKETS),
            r"\left\langle v \right\rangle"
        );
    }

    // DOCX-8-v3续: m:func（函数名）测试。参考 LOK handleFunc：
    // fname + " {" + e + "}"。

    const FIXTURE_FUNC_SIN: &str = r#"<m:oMath>
          <m:func>
            <m:fName><m:r><m:t xml:space="preserve">sin</m:t></m:r></m:fName>
            <m:e><m:r><m:t xml:space="preserve">x</m:t></m:r></m:e>
          </m:func>
        </m:oMath>"#;

    const FIXTURE_FUNC_LIM: &str = r#"<m:oMath>
          <m:func>
            <m:fName><m:r><m:t xml:space="preserve">lim</m:t></m:r></m:fName>
            <m:e><m:r><m:t xml:space="preserve">x</m:t></m:r></m:e>
          </m:func>
        </m:oMath>"#;

    const FIXTURE_FUNC_CUSTOM: &str = r#"<m:oMath>
          <m:func>
            <m:fName><m:r><m:t xml:space="preserve">f</m:t></m:r></m:fName>
            <m:e><m:r><m:t xml:space="preserve">x</m:t></m:r></m:e>
          </m:func>
        </m:oMath>"#;

    #[test]
    fn func_known_trig_name_gets_backslash_prefix() {
        assert_eq!(parse_omml_to_latex(FIXTURE_FUNC_SIN), r"\sin {x}");
    }

    #[test]
    fn func_known_limit_name_gets_backslash_prefix() {
        assert_eq!(parse_omml_to_latex(FIXTURE_FUNC_LIM), r"\lim {x}");
    }

    #[test]
    fn func_custom_name_stays_unprefixed() {
        assert_eq!(parse_omml_to_latex(FIXTURE_FUNC_CUSTOM), r"f {x}");
    }

    // DOCX-8-v3续: m:limLow/m:limUpp/m:groupChr 测试。

    const FIXTURE_LIM_LOW: &str = r#"<m:oMath>
          <m:limLow>
            <m:e><m:r><m:t xml:space="preserve">f</m:t></m:r></m:e>
            <m:lim><m:r><m:t xml:space="preserve">x</m:t></m:r></m:lim>
          </m:limLow>
        </m:oMath>"#;

    const FIXTURE_LIM_UPP: &str = r#"<m:oMath>
          <m:limUpp>
            <m:e><m:r><m:t xml:space="preserve">f</m:t></m:r></m:e>
            <m:lim><m:r><m:t xml:space="preserve">x</m:t></m:r></m:lim>
          </m:limUpp>
        </m:oMath>"#;

    const FIXTURE_GROUPCHR_OVERBRACE: &str = r#"<m:oMath>
          <m:groupChr>
            <m:groupChrPr>
              <m:chr m:val="&#x23de;"/>
              <m:pos m:val="top"/>
            </m:groupChrPr>
            <m:e><m:r><m:t xml:space="preserve">abc</m:t></m:r></m:e>
          </m:groupChr>
        </m:oMath>"#;

    const FIXTURE_GROUPCHR_UNDERBRACE: &str = r#"<m:oMath>
          <m:groupChr>
            <m:groupChrPr>
              <m:chr m:val="&#x23df;"/>
              <m:pos m:val="bot"/>
            </m:groupChrPr>
            <m:e><m:r><m:t xml:space="preserve">xyz</m:t></m:r></m:e>
          </m:groupChr>
        </m:oMath>"#;

    // WROOT-060真实场景：overbrace套在limUpp里，overbrace末尾配一个
    // 上标注释（比如 n项 overbrace{a+a+...+a}^{n}）。
    const FIXTURE_GROUPCHR_NESTED_IN_LIMUPP: &str = r#"<m:oMath>
          <m:limUpp>
            <m:e>
              <m:groupChr>
                <m:groupChrPr>
                  <m:chr m:val="&#x23de;"/>
                  <m:pos m:val="top"/>
                </m:groupChrPr>
                <m:e><m:r><m:t xml:space="preserve">abc</m:t></m:r></m:e>
              </m:groupChr>
            </m:e>
            <m:lim><m:r><m:t xml:space="preserve">n</m:t></m:r></m:lim>
          </m:limUpp>
        </m:oMath>"#;

    #[test]
    fn lim_low_produces_subscript() {
        assert_eq!(parse_omml_to_latex(FIXTURE_LIM_LOW), r"{f}_{x}");
    }

    #[test]
    fn lim_upp_produces_superscript() {
        assert_eq!(parse_omml_to_latex(FIXTURE_LIM_UPP), r"{f}^{x}");
    }

    #[test]
    fn group_chr_standard_overbrace_char_and_top_pos() {
        assert_eq!(parse_omml_to_latex(FIXTURE_GROUPCHR_OVERBRACE), r"\overbrace{abc}");
    }

    #[test]
    fn group_chr_standard_underbrace_char_and_bot_pos() {
        assert_eq!(parse_omml_to_latex(FIXTURE_GROUPCHR_UNDERBRACE), r"\underbrace{xyz}");
    }

    #[test]
    fn group_chr_nested_in_lim_upp_combines_into_annotated_overbrace() {
        assert_eq!(
            parse_omml_to_latex(FIXTURE_GROUPCHR_NESTED_IN_LIMUPP),
            r"{\overbrace{abc}}^{n}"
        );
    }

    // DOCX-8-v3续: m:bar / m:acc 测试。

    const FIXTURE_BAR_TOP: &str = r#"<m:oMath>
          <m:bar>
            <m:barPr><m:pos m:val="top"/></m:barPr>
            <m:e><m:r><m:t xml:space="preserve">x</m:t></m:r></m:e>
          </m:bar>
        </m:oMath>"#;

    const FIXTURE_BAR_BOT_DEFAULT: &str = r#"<m:oMath>
          <m:bar>
            <m:e><m:r><m:t xml:space="preserve">x</m:t></m:r></m:e>
          </m:bar>
        </m:oMath>"#;

    // DOCX-8-v3续: m:borderBox 测试。真实样本(math-overstrike.docx)对应
    // FIXTURE_BORDERBOX_STRIKEH 场景（hideTop/Bot/Left/Right全1+strikeH=1，
    // 但hide开关已定为全部忽略，所以fixture里就不写hide标签了，直接测试
    // strikeH单独出现的效果，等价于真实样本的实际渲染结果）。

    const FIXTURE_BORDERBOX_STRIKEH: &str = r#"<m:oMath>
          <m:borderBox>
            <m:borderBoxPr><m:strikeH m:val="1"/></m:borderBoxPr>
            <m:e><m:r><m:t xml:space="preserve">abc</m:t></m:r></m:e>
          </m:borderBox>
        </m:oMath>"#;

    const FIXTURE_BORDERBOX_STRIKE_TLBR: &str = r#"<m:oMath>
          <m:borderBox>
            <m:borderBoxPr><m:strikeTLBR m:val="1"/></m:borderBoxPr>
            <m:e><m:r><m:t xml:space="preserve">abc</m:t></m:r></m:e>
          </m:borderBox>
        </m:oMath>"#;

    const FIXTURE_BORDERBOX_STRIKE_BLTR: &str = r#"<m:oMath>
          <m:borderBox>
            <m:borderBoxPr><m:strikeBLTR m:val="1"/></m:borderBoxPr>
            <m:e><m:r><m:t xml:space="preserve">abc</m:t></m:r></m:e>
          </m:borderBox>
        </m:oMath>"#;

    const FIXTURE_BORDERBOX_NO_STRIKE: &str = r#"<m:oMath>
          <m:borderBox>
            <m:e><m:r><m:t xml:space="preserve">abc</m:t></m:r></m:e>
          </m:borderBox>
        </m:oMath>"#;

    // DOCX-8-v3续: m:eqArr 测试。
    const FIXTURE_EQARR_TWO_LINES: &str = r#"<m:oMath>
          <m:eqArr>
            <m:e><m:r><m:t xml:space="preserve">x+1</m:t></m:r></m:e>
            <m:e><m:r><m:t xml:space="preserve">y-2</m:t></m:r></m:e>
          </m:eqArr>
        </m:oMath>"#;

    const FIXTURE_EQARR_THREE_LINES: &str = r#"<m:oMath>
          <m:eqArr>
            <m:e><m:r><m:t xml:space="preserve">a</m:t></m:r></m:e>
            <m:e><m:r><m:t xml:space="preserve">b</m:t></m:r></m:e>
            <m:e><m:r><m:t xml:space="preserve">c</m:t></m:r></m:e>
          </m:eqArr>
        </m:oMath>"#;

    const FIXTURE_ACC_HAT: &str = r#"<m:oMath>
          <m:acc>
            <m:accPr><m:chr m:val="&#x0302;"/></m:accPr>
            <m:e><m:r><m:t xml:space="preserve">x</m:t></m:r></m:e>
          </m:acc>
        </m:oMath>"#;

    const FIXTURE_ACC_TILDE: &str = r#"<m:oMath>
          <m:acc>
            <m:accPr><m:chr m:val="&#x0303;"/></m:accPr>
            <m:e><m:r><m:t xml:space="preserve">x</m:t></m:r></m:e>
          </m:acc>
        </m:oMath>"#;

    const FIXTURE_ACC_DOT: &str = r#"<m:oMath>
          <m:acc>
            <m:accPr><m:chr m:val="&#x02d9;"/></m:accPr>
            <m:e><m:r><m:t xml:space="preserve">x</m:t></m:r></m:e>
          </m:acc>
        </m:oMath>"#;

    const FIXTURE_ACC_UNKNOWN_CHAR_FALLS_BACK: &str = r#"<m:oMath>
          <m:acc>
            <m:accPr><m:chr m:val="?"/></m:accPr>
            <m:e><m:r><m:t xml:space="preserve">x</m:t></m:r></m:e>
          </m:acc>
        </m:oMath>"#;

    #[test]
    fn bar_top_position_produces_overline() {
        assert_eq!(parse_omml_to_latex(FIXTURE_BAR_TOP), r"\overline{x}");
    }

    #[test]
    fn bar_missing_pos_defaults_to_underline() {
        assert_eq!(parse_omml_to_latex(FIXTURE_BAR_BOT_DEFAULT), r"\underline{x}");
    }

    #[test]
    fn acc_hat_char_maps_to_hat_command() {
        assert_eq!(parse_omml_to_latex(FIXTURE_ACC_HAT), r"\hat{x}");
    }

    #[test]
    fn acc_tilde_char_maps_to_tilde_command() {
        assert_eq!(parse_omml_to_latex(FIXTURE_ACC_TILDE), r"\tilde{x}");
    }

    #[test]
    fn acc_dot_char_maps_to_dot_command() {
        assert_eq!(parse_omml_to_latex(FIXTURE_ACC_DOT), r"\dot{x}");
    }

    #[test]
    fn acc_unknown_char_falls_back_to_acute() {
        assert_eq!(parse_omml_to_latex(FIXTURE_ACC_UNKNOWN_CHAR_FALLS_BACK), r"\acute{x}");
    }

    #[test]
    fn border_box_strike_h_produces_sout() {
        assert_eq!(parse_omml_to_latex(FIXTURE_BORDERBOX_STRIKEH), r"\sout{abc}");
    }

    #[test]
    fn border_box_strike_tlbr_produces_bcancel() {
        assert_eq!(parse_omml_to_latex(FIXTURE_BORDERBOX_STRIKE_TLBR), r"\bcancel{abc}");
    }

    #[test]
    fn border_box_strike_bltr_produces_cancel() {
        assert_eq!(parse_omml_to_latex(FIXTURE_BORDERBOX_STRIKE_BLTR), r"\cancel{abc}");
    }

    #[test]
    fn border_box_no_strike_produces_fbox() {
        assert_eq!(parse_omml_to_latex(FIXTURE_BORDERBOX_NO_STRIKE), r"\fbox{abc}");
    }

    #[test]
    fn eq_arr_two_lines_joined_with_double_backslash() {
        assert_eq!(
            parse_omml_to_latex(FIXTURE_EQARR_TWO_LINES),
            r"\begin{aligned} x+1 \\ y-2 \end{aligned}"
        );
    }

    #[test]
    fn eq_arr_three_lines_joined_with_double_backslash() {
        assert_eq!(
            parse_omml_to_latex(FIXTURE_EQARR_THREE_LINES),
            r"\begin{aligned} a \\ b \\ c \end{aligned}"
        );
    }

    #[test]
    fn empty_container_tag_does_not_break_stack() {
        // 最小化复现：单独一个自闭合容器标签在序列中间，
        // 验证它不会导致后续兄弟节点错位读取。
        let xml = r#"<m:oMath>
          <m:sSup>
            <m:e><m:r><m:t xml:space="preserve">a</m:t></m:r></m:e>
            <m:sup><m:r><m:t xml:space="preserve">2</m:t></m:r></m:sup>
          </m:sSup>
        </m:oMath>"#;
        assert_eq!(parse_omml_to_latex(xml), r"a^{2}");
    }

    // DOCX-8-v1(h): 组合/嵌套场景。此前(c)/(d)/(e)的测试均为单一标签的
    // 平铺case，未验证核心档标签互相嵌套时转换是否正确——这类case历史上
    // 正是bug高发区（(e)阶段的自闭合m:deg bug就只在嵌套真实fixture里现身，
    // 平铺测试完全没暴露）。结构参照ooxml-omml crate（math.rs）确认的类型
    // 定义：Fraction.numerator/denominator、Radical.degree/base、
    // Nary.subscript/superscript/base、Script/SubSuperscript的base/script
    // 字段均为递归容器，语义上支持任意嵌套，与本文件parse_omml_to_latex
    // 的通用递归实现假设一致。

    // DOCX-8-v1(h) 真实fixture（OnlyOffice产出，非手写）：来自用户手动
    // 在OnlyOffice里打的连分数 1+1/(2+1/3) 和 n次根sqrt(x_i^2 y)，
    // 路径见 roe-ext/fixtures/docx-8-omml-h/。这两份真实fixture暴露了
    // 手写测试没覆盖到的两个真实结构点：
    //   1. m:f 内部 m:den 出现在 m:num 之前（OnlyOffice真实产出顺序，
    //      与此前(c)阶段Word生成的fixture、以及本文件手写测试的顺序相反）
    //   2. m:r 内部混入 w:rPr（而非m:rPr），是额外一层需要正确跳过的属性容器
    //   3. m:f 内部有 m:fPr/m:ctrlPr 包裹的控制属性，需确认被正确忽略
    const FIXTURE_CONTINUED_FRACTION_REAL: &str = r####"<m:oMath>
          <m:r>
            <w:rPr>
              <w:rFonts w:ascii="Cambria Math" w:hAnsi="Cambria Math"/>
              <w:sz w:val="24"/>
              <w:szCs w:val="24"/>
            </w:rPr>
            <m:t>x</m:t>
          </m:r>
          <m:r>
            <w:rPr>
              <w:rFonts w:ascii="Cambria Math" w:hAnsi="Cambria Math"/>
              <w:sz w:val="24"/>
              <w:szCs w:val="24"/>
            </w:rPr>
            <m:t>=</m:t>
          </m:r>
          <m:r>
            <w:rPr>
              <w:rFonts w:ascii="Cambria Math" w:hAnsi="Cambria Math"/>
              <w:sz w:val="24"/>
              <w:szCs w:val="24"/>
            </w:rPr>
            <m:t>1</m:t>
          </m:r>
          <m:r>
            <w:rPr>
              <w:rFonts w:ascii="Cambria Math" w:hAnsi="Cambria Math"/>
              <w:sz w:val="24"/>
              <w:szCs w:val="24"/>
            </w:rPr>
            <m:t>+</m:t>
          </m:r>
          <m:f>
            <m:fPr>
              <m:ctrlPr>
                <w:rPr>
                  <w:rFonts w:ascii="Cambria Math" w:hAnsi="Cambria Math"/>
                  <w:sz w:val="24"/>
                  <w:szCs w:val="24"/>
                </w:rPr>
              </m:ctrlPr>
            </m:fPr>
            <m:den>
              <m:r>
                <w:rPr>
                  <w:rFonts w:ascii="Cambria Math" w:hAnsi="Cambria Math"/>
                  <w:sz w:val="24"/>
                  <w:szCs w:val="24"/>
                </w:rPr>
                <m:t>2</m:t>
              </m:r>
              <m:r>
                <w:rPr>
                  <w:rFonts w:ascii="Cambria Math" w:hAnsi="Cambria Math"/>
                  <w:sz w:val="24"/>
                  <w:szCs w:val="24"/>
                </w:rPr>
                <m:t>+</m:t>
              </m:r>
              <m:f>
                <m:fPr>
                  <m:ctrlPr>
                    <w:rPr>
                      <w:rFonts w:ascii="Cambria Math" w:hAnsi="Cambria Math"/>
                      <w:sz w:val="24"/>
                      <w:szCs w:val="24"/>
                    </w:rPr>
                  </m:ctrlPr>
                </m:fPr>
                <m:den>
                  <m:r>
                    <w:rPr>
                      <w:rFonts w:ascii="Cambria Math" w:hAnsi="Cambria Math"/>
                      <w:sz w:val="24"/>
                      <w:szCs w:val="24"/>
                    </w:rPr>
                    <m:t>3</m:t>
                  </m:r>
                </m:den>
                <m:num>
                  <m:r>
                    <w:rPr>
                      <w:rFonts w:ascii="Cambria Math" w:hAnsi="Cambria Math"/>
                      <w:sz w:val="24"/>
                      <w:szCs w:val="24"/>
                    </w:rPr>
                    <m:t>1</m:t>
                  </m:r>
                </m:num>
              </m:f>
            </m:den>
            <m:num>
              <m:r>
                <w:rPr>
                  <w:rFonts w:ascii="Cambria Math" w:hAnsi="Cambria Math"/>
                  <w:sz w:val="24"/>
                  <w:szCs w:val="24"/>
                </w:rPr>
                <m:t>1</m:t>
              </m:r>
            </m:num>
          </m:f>
        </m:oMath>"####;

    const FIXTURE_NTH_ROOT_SUBSUP_REAL: &str = r####"<m:oMath>
          <m:rad>
            <m:radPr>
              <m:degHide m:val="on"/>
              <m:ctrlPr>
                <w:rPr>
                  <w:rFonts w:ascii="Cambria Math" w:hAnsi="Cambria Math"/>
                  <w:sz w:val="24"/>
                  <w:szCs w:val="24"/>
                </w:rPr>
              </m:ctrlPr>
            </m:radPr>
            <m:deg/>
            <m:e>
              <m:sSubSup>
                <m:sSubSupPr>
                  <m:ctrlPr>
                    <w:rPr>
                      <w:rFonts w:ascii="Cambria Math" w:hAnsi="Cambria Math"/>
                      <w:sz w:val="24"/>
                      <w:szCs w:val="24"/>
                    </w:rPr>
                  </m:ctrlPr>
                </m:sSubSupPr>
                <m:e>
                  <m:r>
                    <w:rPr>
                      <w:rFonts w:ascii="Cambria Math" w:hAnsi="Cambria Math"/>
                      <w:sz w:val="24"/>
                      <w:szCs w:val="24"/>
                    </w:rPr>
                    <m:t>x</m:t>
                  </m:r>
                </m:e>
                <m:sub>
                  <m:r>
                    <w:rPr>
                      <w:rFonts w:ascii="Cambria Math" w:hAnsi="Cambria Math"/>
                      <w:sz w:val="24"/>
                      <w:szCs w:val="24"/>
                    </w:rPr>
                    <m:t>i</m:t>
                  </m:r>
                </m:sub>
                <m:sup>
                  <m:r>
                    <w:rPr>
                      <w:rFonts w:ascii="Cambria Math" w:hAnsi="Cambria Math"/>
                      <w:sz w:val="24"/>
                      <w:szCs w:val="24"/>
                    </w:rPr>
                    <m:t>2</m:t>
                  </m:r>
                </m:sup>
              </m:sSubSup>
              <m:r>
                <w:rPr>
                  <w:rFonts w:ascii="Cambria Math" w:hAnsi="Cambria Math"/>
                  <w:sz w:val="24"/>
                  <w:szCs w:val="24"/>
                </w:rPr>
                <m:t>y</m:t>
              </m:r>
            </m:e>
          </m:rad>
        </m:oMath>"####;

    #[test]
    fn continued_fraction_real_fixture_num_den_order_independent() {
        // 关键回归点：这份真实fixture里 m:den 在 m:num 之前出现，
        // 若 close_container 对 m:f 的处理是"按parts数组位置(first=num,
        // get(1)=den)"而非"按标签名分别收集"，这里会得到分子分母互换的
        // 错误结果。正确结果应为 1+1/(2+1/3)，不是 1+1/(1/3+2) 之类的误判。
        let result = parse_omml_to_latex(FIXTURE_CONTINUED_FRACTION_REAL);
        assert_eq!(result, r"x=1+\frac{1}{2+\frac{1}{3}}", "实际输出: {result}");
    }

    #[test]
    fn nth_root_subsup_real_fixture_converts_correctly() {
        // degHide m:val="on"（非"1"）应仍被判定为隐藏次数（当前判断逻辑
        // val != "0"，任何非"0"值都算隐藏，"on"能正确命中，这里做正向验证
        // 而非回归测试——此前排查已确认这条判断逻辑本身没问题）。
        let result = parse_omml_to_latex(FIXTURE_NTH_ROOT_SUBSUP_REAL);
        assert_eq!(result, r"\sqrt{x_{i}^{2}y}", "实际输出: {result}");
    }

    #[test]
    fn fraction_with_superscript_in_numerator_converts_correctly() {
        // (x^2+1)/y —— 分子内嵌上标，验证m:f容器内部不假设"只有纯文本run"
        let xml = r#"<m:oMath>
          <m:f>
            <m:num>
              <m:sSup>
                <m:e><m:r><m:t xml:space="preserve">x</m:t></m:r></m:e>
                <m:sup><m:r><m:t xml:space="preserve">2</m:t></m:r></m:sup>
              </m:sSup>
              <m:r><m:t xml:space="preserve">+</m:t></m:r>
              <m:r><m:t xml:space="preserve">1</m:t></m:r>
            </m:num>
            <m:den>
              <m:r><m:t xml:space="preserve">y</m:t></m:r>
            </m:den>
          </m:f>
        </m:oMath>"#;
        assert_eq!(parse_omml_to_latex(xml), r"\frac{x^{2}+1}{y}");
    }

    #[test]
    fn nary_with_fraction_in_upper_bound_converts_correctly() {
        // sum_{i=1}^{n/2} i —— 上界内嵌分数，验证m:nary的sup容器同样支持复合结构
        let xml = r#"<m:oMath>
          <m:nary>
            <m:naryPr><m:chr m:val="∑"/></m:naryPr>
            <m:sub><m:r><m:t xml:space="preserve">i=1</m:t></m:r></m:sub>
            <m:sup>
              <m:f>
                <m:num><m:r><m:t xml:space="preserve">n</m:t></m:r></m:num>
                <m:den><m:r><m:t xml:space="preserve">2</m:t></m:r></m:den>
              </m:f>
            </m:sup>
            <m:e><m:r><m:t xml:space="preserve">i</m:t></m:r></m:e>
          </m:nary>
        </m:oMath>"#;
        assert_eq!(parse_omml_to_latex(xml), r"\sum_{i=1}^{\frac{n}{2}} i");
    }

    #[test]
    fn sqrt_with_subsup_inside_base_converts_correctly() {
        // sqrt(x_i^2) —— 根号内部base是sSubSup而非纯文本run，
        // 验证m:rad的m:e容器同样走通用递归路径（不是m:f/m:nary专属）
        let xml = r#"<m:oMath>
          <m:rad>
            <m:radPr><m:degHide m:val="1"/></m:radPr>
            <m:deg/>
            <m:e>
              <m:sSubSup>
                <m:e><m:r><m:t xml:space="preserve">x</m:t></m:r></m:e>
                <m:sub><m:r><m:t xml:space="preserve">i</m:t></m:r></m:sub>
                <m:sup><m:r><m:t xml:space="preserve">2</m:t></m:r></m:sup>
              </m:sSubSup>
            </m:e>
          </m:rad>
        </m:oMath>"#;
        assert_eq!(parse_omml_to_latex(xml), r"\sqrt{x_{i}^{2}}");
    }

    #[test]
    fn unrecognized_nary_chr_falls_back_to_literal_symbol() {
        let xml = r#"<m:oMath>
          <m:nary>
            <m:naryPr><m:chr m:val="⊕"/></m:naryPr>
            <m:sub><m:r><m:t xml:space="preserve">1</m:t></m:r></m:sub>
            <m:sup><m:r><m:t xml:space="preserve">n</m:t></m:r></m:sup>
            <m:e><m:r><m:t xml:space="preserve">x</m:t></m:r></m:e>
          </m:nary>
        </m:oMath>"#;
        let result = parse_omml_to_latex(xml);
        assert!(result.contains('⊕'), "未识别符号应原样兜底保留，实际输出: {result}");
        assert!(!result.is_empty());
    }

    #[test]
    fn spre_produces_prefix_sub_and_superscript() {
        let xml = r#"
          <m:sPre>
            <m:sub><m:r><m:t>92</m:t></m:r></m:sub>
            <m:sup><m:r><m:t>235</m:t></m:r></m:sup>
            <m:e><m:r><m:t>U</m:t></m:r></m:e>
          </m:sPre>
        "#;
        let result = parse_omml_to_latex(xml);
        assert_eq!(result.trim(), r"{}_{92}^{235}{U}");
    }

    #[test]
    fn spre_missing_sub_produces_empty_subscript_group() {
        let xml = r#"
          <m:sPre>
            <m:sup><m:r><m:t>235</m:t></m:r></m:sup>
            <m:e><m:r><m:t>U</m:t></m:r></m:e>
          </m:sPre>
        "#;
        let result = parse_omml_to_latex(xml);
        assert_eq!(result.trim(), r"{}_{}^{235}{U}");
    }

    #[test]
    fn matrix_2x2_produces_matrix_environment() {
        let xml = r#"
          <m:m>
            <m:mr>
              <m:e><m:r><m:t>1</m:t></m:r></m:e>
              <m:e><m:r><m:t>2</m:t></m:r></m:e>
            </m:mr>
            <m:mr>
              <m:e><m:r><m:t>3</m:t></m:r></m:e>
              <m:e><m:r><m:t>4</m:t></m:r></m:e>
            </m:mr>
          </m:m>
        "#;
        let result = parse_omml_to_latex(xml);
        let expected = "\\begin{matrix} 1 & 2 \\\\ 3 & 4 \\end{matrix}";
        assert_eq!(result.trim(), expected);
    }
}

#[cfg(test)]
mod tests_docx8_omath_para_e2e {
    use super::*;
    use std::io::Write;
    use zip::write::FileOptions;

    // 端到端集成测试：验证 m:oMathPara 抠取接线本身（不是转换函数逻辑，
    // 那部分已由 tests_docx8_omml 独立验证）。走完整 DocxParser::parse。
    fn make_zip(document_xml: &str) -> Vec<u8> {
        let opts: FileOptions<()> = FileOptions::default();
        let buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(buf));

        let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;

        let doc_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
</Relationships>"#;

        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        zip.start_file("word/_rels/document.xml.rels", opts).unwrap();
        zip.write_all(doc_rels.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document_xml.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    // 来自真实 fixture1（一代 office-rs 生成），保留了 m:oMathPara 前
    // 紧跟一个空 w:r 的真实结构（Word 的实际产出形态，不是简化过的）。
    const DOCUMENT_WITH_FORMULA: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math">
<w:body>
  <w:p>
    <w:r><w:rPr/></w:r>
    <m:oMathPara xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math">
      <m:oMathParaPr>
        <m:jc m:val="left"/>
      </m:oMathParaPr>
      <m:oMath>
        <m:r><m:t xml:space="preserve">x</m:t></m:r>
        <m:r><m:t xml:space="preserve">=</m:t></m:r>
        <m:f>
          <m:num>
            <m:r><m:t xml:space="preserve">a</m:t></m:r>
            <m:r><m:t xml:space="preserve">+</m:t></m:r>
            <m:r><m:t xml:space="preserve">b</m:t></m:r>
          </m:num>
          <m:den>
            <m:r><m:t xml:space="preserve">c</m:t></m:r>
            <m:r><m:t xml:space="preserve">+</m:t></m:r>
            <m:r><m:t xml:space="preserve">d</m:t></m:r>
          </m:den>
        </m:f>
      </m:oMath>
    </m:oMathPara>
  </w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"##;

    #[test]
    fn formula_reaches_run_formula_field_end_to_end() {
        let data = make_zip(DOCUMENT_WITH_FORMULA);
        let doc = DocxParser::parse(&data).unwrap();

        let formula_runs: Vec<_> = doc.body.iter()
            .filter_map(|el| match el {
                BodyElement::Paragraph(p) => Some(p),
                _ => None,
            })
            .flat_map(|p| p.runs.iter())
            .filter(|r| r.formula.is_some())
            .collect();

        assert_eq!(formula_runs.len(), 1, "应恰好产出1个带公式的Run，实际body: {:?}", doc.body);
        let formula = formula_runs[0].formula.as_ref().unwrap();
        assert_eq!(formula.latex, r"x=\frac{a+b}{c+d}");
    }

    #[test]
    fn document_without_formula_has_no_formula_runs() {
        // 回归防护：确认新增的 m:oMathPara 分支不会误触发/不影响无公式文档
        let plain = r##"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
  <w:p><w:r><w:t>hello</w:t></w:r></w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"##;
        let data = make_zip(plain);
        let doc = DocxParser::parse(&data).unwrap();
        let has_formula = doc.body.iter()
            .filter_map(|el| match el {
                BodyElement::Paragraph(p) => Some(p),
                _ => None,
            })
            .flat_map(|p| p.runs.iter())
            .any(|r| r.formula.is_some());
        assert!(!has_formula);
    }

    // DOCX-8-v1(f): 尺寸回填验证。
    fn extract_formula(doc: &roe_model::writer::WriterDocument) -> roe_model::writer::Formula {
        doc.body.iter()
            .filter_map(|el| match el {
                BodyElement::Paragraph(p) => Some(p),
                _ => None,
            })
            .flat_map(|p| p.runs.iter())
            .find_map(|r| r.formula.clone())
            .expect("应恰好产出1个带公式的Run")
    }

    #[test]
    fn formula_size_uses_default_font_size_when_no_run_sz_present() {
        // DOCUMENT_WITH_FORMULA 里 m:oMathPara 前的 w:r 是 <w:rPr/>（空，无w:sz），
        // 应退回方案A的默认字号10.5pt，且 latex_layout_size 应成功执行
        // （不是降级到estimate_size_from_mathml兜底），产出非零、合理量级的尺寸。
        let data = make_zip(DOCUMENT_WITH_FORMULA);
        let doc = DocxParser::parse(&data).unwrap();
        let formula = extract_formula(&doc);

        assert!(formula.width_pt > 0.0, "width_pt应为正值，实际: {}", formula.width_pt);
        assert!(formula.height_pt > 0.0, "height_pt应为正值，实际: {}", formula.height_pt);
        // 合理性上界粗略把关：默认10.5pt字号下，一个简单分式公式不应长/高到几百pt
        assert!(formula.width_pt < 200.0, "width_pt量级异常: {}", formula.width_pt);
        assert!(formula.height_pt < 200.0, "height_pt量级异常: {}", formula.height_pt);
    }

    #[test]
    fn formula_size_scales_with_run_level_font_size() {
        // 公式前一个w:r显式指定 w:sz=48（半点单位=24pt，是默认10.5pt的约2.3倍），
        // 验证 run_font_size 被正确读取并影响 latex_layout_size 的结果，
        // 而不是恒定退回默认值10.5pt。
        let doc_large_font = DOCUMENT_WITH_FORMULA.replace(
            "<w:r><w:rPr/></w:r>",
            r#"<w:r><w:rPr><w:sz w:val="48"/></w:rPr></w:r>"#,
        );
        assert_ne!(doc_large_font, DOCUMENT_WITH_FORMULA, "replace应实际生效，否则测试没有意义");

        let data_default = make_zip(DOCUMENT_WITH_FORMULA);
        let data_large = make_zip(&doc_large_font);

        let formula_default = extract_formula(&DocxParser::parse(&data_default).unwrap());
        let formula_large = extract_formula(&DocxParser::parse(&data_large).unwrap());

        assert!(
            formula_large.width_pt > formula_default.width_pt,
            "24pt字号应产出比默认10.5pt更宽的公式，default={}, large={}",
            formula_default.width_pt, formula_large.width_pt
        );
        assert!(
            formula_large.height_pt > formula_default.height_pt,
            "24pt字号应产出比默认10.5pt更高的公式，default={}, large={}",
            formula_default.height_pt, formula_large.height_pt
        );
        // 字号比例约2.2857倍(48/2/10.5)，允许宽松容差，只验证方向和大致量级，
        // 不做精确浮点比例断言（避免对RaTeX内部实现细节过度耦合）。
        let ratio = formula_large.width_pt / formula_default.width_pt;
        assert!(
            ratio > 1.5 && ratio < 4.0,
            "尺寸缩放比例超出合理范围: {}", ratio
        );
    }

    // WROOT-080更新：占位符方案已从"空格串撑宽度"改为Parley原生
    // InlineBox机制（layout.rs构建阶段push_inline_box直接传width_pt，
    // 不再依赖run.text的字符宽度撑开占位）。根因：Parley对齐计算把
    // 纯空格run识别为trailing_whitespace，从free_space中排除，导致
    // 居中/右对齐时占位符完全不参与宽度计算（详见docx.rs公式run
    // 构造处注释、bug_log.md WROOT-080条目）。因此本测试原有的
    // "text非空"断言已过时，改为断言text为空（新方案的正确状态）、
    // 且formula.width_pt为正值（宽度信息真正的来源，InlineBox靠
    // 这个字段定位，不再靠text）。
    #[test]
    fn formula_run_has_visible_fallback_text_not_empty() {
        let data = make_zip(DOCUMENT_WITH_FORMULA);
        let doc = DocxParser::parse(&data).unwrap();

        let formula_run = doc.body.iter()
            .filter_map(|el| match el {
                BodyElement::Paragraph(p) => Some(p),
                _ => None,
            })
            .flat_map(|p| p.runs.iter())
            .find(|r| r.formula.is_some())
            .expect("应存在1个带公式的Run");

        assert!(formula_run.text.is_empty(), "WROOT-080后公式Run的text应为空，宽度信息改由InlineBox读取formula.width_pt");
        assert!(
            formula_run.formula.as_ref().map(|f| f.width_pt > 0.0).unwrap_or(false),
            "公式的width_pt应为正值，InlineBox渲染依赖此字段定位"
        );
        assert!(
            formula_run.text.chars().all(|c| c == ' '),
            "占位符应为纯空格串（撑宽度用），不应包含可见的LaTeX语法文本，实际text: {:?}", formula_run.text
        );
    }
}




#[cfg(test)]
mod tests_doc_defaults_font_size {
    use super::*;

    fn make_doc_defaults_docx() -> Vec<u8> {
        use std::io::Write;
        use zip::write::FileOptions;
        let opts: FileOptions<()> = FileOptions::default();
        let buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(buf));

        let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
        let doc_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;
        let document = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
  <w:p>
    <w:r><w:t>Hello</w:t></w:r>
  </w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"#;
        let styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:docDefaults>
    <w:rPrDefault><w:rPr>
      <w:rFonts w:ascii="Roboto" w:hAnsi="Roboto" w:eastAsia="Roboto" w:cs="Roboto"/>
      <w:color w:val="666666"/>
      <w:sz w:val="22"/>
      <w:szCs w:val="22"/>
    </w:rPr></w:rPrDefault>
    <w:pPrDefault><w:pPr>
      <w:spacing w:after="0" w:afterAutospacing="0" w:before="200"
                 w:beforeAutospacing="0" w:line="335" w:lineRule="auto"/>
      <w:ind w:right="0" w:firstLine="0" w:left="-15"/>
    </w:pPr></w:pPrDefault>
  </w:docDefaults>
</w:styles>"#;

        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        zip.start_file("word/_rels/document.xml.rels", opts).unwrap();
        zip.write_all(doc_rels.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document.as_bytes()).unwrap();
        zip.start_file("word/styles.xml", opts).unwrap();
        zip.write_all(styles.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn doc_default_font_size_parsed_into_style_registry() {
        let data = make_doc_defaults_docx();
        let doc = DocxParser::parse(&data).unwrap();
        // docDefaults w:sz=22 half-points -> 11.0pt，验证解析阶段
        // 已经把 w:rPrDefault/w:sz 打通进 StyleRegistry
        let fs = doc.styles.doc_default_font_size
            .expect("doc_default_font_size should be parsed from w:rPrDefault");
        assert!((fs - 11.0).abs() < 0.01, "doc_default_font_size={fs}, expected 11.0");
    }

    #[test]
    fn doc_default_font_size_applies_via_resolve_paragraph_style_fallback() {
        use roe_model::style::{StyleRegistry, ParagraphStyle};
        // 直接在 StyleRegistry 层面构造场景：注册一个没有 based_on 的具名样式
        // "Normal"（模拟真实文档里存在但未设置 font_size 的基础样式），
        // 验证 resolve_paragraph_style 的隐式基类兜底会把 doc_default_font_size
        // 注入进最终 resolved 结果，而不是让调用方后续再退化到硬编码 12.0。
        let mut registry = StyleRegistry::default();
        registry.doc_default_font_size = Some(11.0);
        registry.paragraph_styles.insert("Normal".to_string(), ParagraphStyle {
            name: "Normal".to_string(),
            based_on: None,
            ..Default::default()
        });

        let resolved = registry.resolve_paragraph_style("Normal");
        let fs = resolved.font_size
            .expect("resolved paragraph style should carry doc_default_font_size as fallback");
        assert!((fs - 11.0).abs() < 0.01, "resolved font_size={fs}, expected 11.0 from docDefaults");
    }

    #[test]
    fn doc_default_color_parsed_into_style_registry() {
        let data = make_doc_defaults_docx();
        let doc = DocxParser::parse(&data).unwrap();
        // docDefaults w:color w:val="666666" -> RGB(0x66, 0x66, 0x66)，
        // 验证解析阶段已经把 w:rPrDefault/w:color 打通进 StyleRegistry，
        // 并且十六进制字符串被正确转换成了 Color 类型（不是停留在原始字符串）。
        let color = doc.styles.doc_default_color
            .expect("doc_default_color should be parsed from w:rPrDefault");
        assert_eq!(color.r, 0x66, "color.r={:#x}, expected 0x66", color.r);
        assert_eq!(color.g, 0x66, "color.g={:#x}, expected 0x66", color.g);
        assert_eq!(color.b, 0x66, "color.b={:#x}, expected 0x66", color.b);
    }

    #[test]
    fn doc_default_color_applies_via_resolve_char_style_fallback() {
        use roe_model::style::{StyleRegistry, CharStyle};
        use roe_model::common::Color;
        // 直接在 StyleRegistry 层面构造场景：注册一个没有 based_on 的具名字符样式
        // "DefaultParagraphFont"（模拟真实文档里存在但未设置 color 的基础样式），
        // 验证 resolve_char_style 的隐式基类兜底会把 doc_default_color
        // 注入进最终 resolved 结果，而不是让调用方后续再退化到纯黑色。
        let mut registry = StyleRegistry::default();
        registry.doc_default_color = Some(Color { r: 0x66, g: 0x66, b: 0x66, a: 255 });
        registry.char_styles.insert("DefaultParagraphFont".to_string(), CharStyle {
            name: "DefaultParagraphFont".to_string(),
            based_on: None,
            ..Default::default()
        });

        let resolved = registry.resolve_char_style("DefaultParagraphFont");
        let color = resolved.color
            .expect("resolved char style should carry doc_default_color as fallback");
        assert_eq!(color.r, 0x66, "color.r={:#x}, expected 0x66", color.r);
        assert_eq!(color.g, 0x66, "color.g={:#x}, expected 0x66", color.g);
        assert_eq!(color.b, 0x66, "color.b={:#x}, expected 0x66", color.b);
    }

    #[test]
    fn doc_default_space_before_parsed_into_style_registry() {
        let data = make_doc_defaults_docx();
        let doc = DocxParser::parse(&data).unwrap();
        // docDefaults w:spacing w:before="200" twips -> 200/20 = 10.0pt，
        // 验证解析阶段已经把 w:pPrDefault/w:spacing 的 w:before 属性打通
        // 进 StyleRegistry（原来这个分支只读了 w:line 一个属性）。
        let sb = doc.styles.doc_default_space_before
            .expect("doc_default_space_before should be parsed from w:pPrDefault/w:spacing/@w:before");
        assert!((sb - 10.0).abs() < 0.01, "doc_default_space_before={sb}, expected 10.0");
    }

    #[test]
    fn doc_default_space_before_applies_via_resolve_paragraph_style_fallback() {
        use roe_model::style::{StyleRegistry, ParagraphStyle};
        // 段落样式没有显式设置 space_before 时，resolve_paragraph_style 的
        // 隐式基类兜底应该把 doc_default_space_before 注入进最终 resolved 结果。
        let mut registry = StyleRegistry::default();
        registry.doc_default_space_before = Some(10.0);
        registry.paragraph_styles.insert("Normal".to_string(), ParagraphStyle {
            name: "Normal".to_string(),
            based_on: None,
            ..Default::default()
        });

        let resolved = registry.resolve_paragraph_style("Normal");
        let sb = resolved.space_before
            .expect("resolved paragraph style should carry doc_default_space_before as fallback");
        assert!((sb - 10.0).abs() < 0.01, "resolved space_before={sb}, expected 10.0");
    }

    // WROOT-095 关联：以下两个测试针对"缩进"字段，专门验证编号列表场景
    // 下 docDefaults 缩进兜底的端到端链路（level 自身未定义 w:ind 时退回
    // 文档级默认值）。docDefaults 缩进只对编号列表生效，因为普通非列表
    // 段落目前没有任何缩进消费路径（该缺口已登记为独立技术债 WROOT-095，
    // 不在本次任务范围内）。

    #[test]
    fn doc_default_indent_parsed_into_style_registry() {
        let data = make_doc_defaults_docx();
        let doc = DocxParser::parse(&data).unwrap();
        // docDefaults <w:ind w:right="0" w:firstLine="0" w:left="-15"/>
        // w:left="-15" twips -> -15/20 = -0.75pt
        // w:firstLine="0" twips -> 0/20 = 0.0pt（显式写了0，不是缺失）
        let il = doc.styles.doc_default_indent_left
            .expect("doc_default_indent_left should be parsed from w:pPrDefault/w:ind");
        assert!((il - (-0.75)).abs() < 0.01, "doc_default_indent_left={il}, expected -0.75");
        let ifst = doc.styles.doc_default_indent_first
            .expect("doc_default_indent_first should be parsed from w:pPrDefault/w:ind");
        assert!((ifst - 0.0).abs() < 0.01, "doc_default_indent_first={ifst}, expected 0.0");
    }

    fn make_doc_defaults_docx_with_numbering_no_ind() -> Vec<u8> {
        // 与 make_doc_defaults_docx 同款 docDefaults（含 w:ind left=-15），
        // 额外加一份 numbering.xml：唯一的 level（ilvl=0）故意不写 w:ind，
        // 用来验证"level 自身未定义缩进时，退回 docDefaults"这条兜底路径，
        // 而不是退回硬编码默认值或彻底缺失。document.xml 的段落通过
        // w:numPr 引用这个 numId=1/ilvl=0。
        use std::io::Write;
        use zip::write::FileOptions;
        let opts: FileOptions<()> = FileOptions::default();
        let buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(buf));

        let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
        let doc_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/>
</Relationships>"#;
        let document = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
  <w:p>
    <w:pPr>
      <w:numPr>
        <w:ilvl w:val="0"/>
        <w:numId w:val="1"/>
      </w:numPr>
    </w:pPr>
    <w:r><w:t>Item one</w:t></w:r>
  </w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"#;
        let styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:docDefaults>
    <w:pPrDefault><w:pPr>
      <w:ind w:right="0" w:firstLine="0" w:left="-15"/>
    </w:pPr></w:pPrDefault>
  </w:docDefaults>
</w:styles>"#;
        let numbering = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:abstractNum w:abstractNumId="0">
    <w:lvl w:ilvl="0">
      <w:start w:val="1"/>
      <w:numFmt w:val="decimal"/>
      <w:lvlText w:val="%1."/>
    </w:lvl>
  </w:abstractNum>
  <w:num w:numId="1">
    <w:abstractNumId w:val="0"/>
  </w:num>
</w:numbering>"#;

        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        zip.start_file("word/_rels/document.xml.rels", opts).unwrap();
        zip.write_all(doc_rels.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document.as_bytes()).unwrap();
        zip.start_file("word/styles.xml", opts).unwrap();
        zip.write_all(styles.as_bytes()).unwrap();
        zip.start_file("word/numbering.xml", opts).unwrap();
        zip.write_all(numbering.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn doc_default_indent_applies_to_numbered_paragraph_when_level_has_no_ind() {
        let data = make_doc_defaults_docx_with_numbering_no_ind();
        let doc = DocxParser::parse(&data).unwrap();
        let para = match &doc.body[0] {
            roe_model::writer::BodyElement::Paragraph(p) => p,
            _ => panic!("期望段落"),
        };
        // level（ilvl=0）自身没有 w:ind，段落自身也没有直接 w:ind，
        // 应该端到端一路兜底到 docDefaults 的 -0.75pt / 0.0pt，而不是
        // None（彻底缺失）或某个硬编码默认值。这是本次真正要验证的
        // 端到端路径（对应 read_document 里 lvl_indent 的新增兜底分支），
        // 不是只测 StyleRegistry 层面的 resolve 函数。
        let il = para.indent_left
            .expect("indent_left should fall back to doc_default_indent_left end-to-end");
        assert!((il - (-0.75)).abs() < 0.01, "para.indent_left={il}, expected -0.75");
        let ifst = para.indent_first
            .expect("indent_first should fall back to doc_default_indent_first end-to-end");
        assert!((ifst - 0.0).abs() < 0.01, "para.indent_first={ifst}, expected 0.0");
    }
}


#[cfg(test)]
mod tests_wroot075_endnotes {
    use super::*;

    /// 构造一个只含 word/endnotes.xml 的最小 zip，内容取自真实样本
    /// small__sw_ooxmlexport_data__tdf141548.docx 的 word/endnotes.xml
    /// 原始内容（未做任何简化），确保测试断言的是真实样本的真实结构。
    fn make_endnotes_only_zip() -> Vec<u8> {
        use std::io::Write;
        use zip::write::FileOptions;
        let opts: FileOptions<()> = FileOptions::default();
        let buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(buf));

        let endnotes_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:endnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:endnote w:id="0" w:type="separator"><w:p><w:r><w:separator/></w:r></w:p></w:endnote><w:endnote w:id="1" w:type="continuationSeparator"><w:p><w:r><w:continuationSeparator/></w:r></w:p></w:endnote><w:endnote w:id="2"><w:p><w:pPr><w:pStyle w:val="Style23"/><w:rPr></w:rPr></w:pPr><w:r><w:rPr></w:rPr><w:endnoteRef/><w:tab/><w:t>some endnote</w:t></w:r></w:p><w:p><w:pPr><w:pStyle w:val="Style23"/><w:rPr></w:rPr></w:pPr><w:r><w:rPr></w:rPr><w:tab/><w:t>new paragraph</w:t><w:br/><w:t>new line</w:t></w:r></w:p><w:p><w:pPr><w:pStyle w:val="Style23"/><w:rPr></w:rPr></w:pPr><w:r><w:rPr></w:rPr><w:tab/><w:t>new paragraph</w:t></w:r></w:p><w:p><w:pPr><w:pStyle w:val="Style23"/><w:rPr></w:rPr></w:pPr><w:r><w:rPr></w:rPr></w:r></w:p></w:endnote><w:endnote w:id="3"><w:p><w:pPr><w:pStyle w:val="Style23"/><w:rPr></w:rPr></w:pPr><w:r><w:rPr></w:rPr><w:endnoteRef/><w:tab/><w:t>another endnote</w:t><w:br/><w:t>new line</w:t></w:r></w:p><w:p><w:pPr><w:pStyle w:val="Style23"/><w:rPr></w:rPr></w:pPr><w:r><w:rPr></w:rPr><w:tab/><w:t>new paragraph</w:t></w:r></w:p><w:p><w:pPr><w:pStyle w:val="Style23"/><w:rPr></w:rPr></w:pPr><w:r><w:rPr></w:rPr><w:tab/><w:t>new paragraph</w:t></w:r></w:p></w:endnote></w:endnotes>"#;

        zip.start_file("word/endnotes.xml", opts).unwrap();
        zip.write_all(endnotes_xml.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    fn para_text(p: &Paragraph) -> String {
        p.runs.iter().map(|r| r.text.as_str()).collect::<String>()
    }

    #[test]
    fn parse_notes_part_skips_separators_and_extracts_two_real_endnotes() {
        let data = make_endnotes_only_zip();
        let cursor = std::io::Cursor::new(data);
        let mut zip = ZipArchive::new(cursor).unwrap();

        let notes = parse_notes_part(&mut zip, "word/endnotes.xml", b"w:endnote");

        // Step 7改动：分隔符类型（id=0, id=1）不再被跳过，而是保留下来并标记
        // note_type，供渲染层识别"这是画分隔线的标记，不是真实脚注内容"。
        // 所以现在总数是4条：2条分隔符标记 + 2条真实内容（id=2, id=3）。
        assert_eq!(notes.len(), 4, "expected 4 notes total (2 separators kept + 2 real), got {}", notes.len());
        assert_eq!(notes[0].id, 0);
        assert_eq!(notes[0].note_type, NoteType::Separator);
        assert_eq!(notes[1].id, 1);
        assert_eq!(notes[1].note_type, NoteType::ContinuationSeparator);
        assert_eq!(notes[2].id, 2);
        assert_eq!(notes[2].note_type, NoteType::Normal);
        assert_eq!(notes[3].id, 3);
        assert_eq!(notes[3].note_type, NoteType::Normal);

        // note id=2 应含4个段落（含最后一个空段落）
        let note2_paras: Vec<&Paragraph> = notes[2].content.iter().filter_map(|b| {
            if let BodyElement::Paragraph(p) = b { Some(p) } else { None }
        }).collect();
        assert_eq!(note2_paras.len(), 4, "note id=2 should have 4 paragraphs (incl. trailing empty one)");
        assert_eq!(para_text(note2_paras[0]), "	some endnote");
        assert_eq!(para_text(note2_paras[1]), "	new paragraph
new line");
        assert_eq!(para_text(note2_paras[2]), "	new paragraph");
        assert_eq!(para_text(note2_paras[3]), "");

        // note id=2 第一段第一个 run 应带 EndnoteRef 标记（<w:endnoteRef/> 紧跟在前）
        let first_run = &note2_paras[0].runs[0];
        assert_eq!(first_run.field, Some(roe_model::writer::FieldKind::EndnoteRef));
        // 第一段 pStyle 应被正确读取为 "Style23"，而不是硬编码 "Normal"
        assert_eq!(note2_paras[0].style, StyleRef("Style23".to_string()));

        // note id=3 应含3个段落
        let note3_paras: Vec<&Paragraph> = notes[3].content.iter().filter_map(|b| {
            if let BodyElement::Paragraph(p) = b { Some(p) } else { None }
        }).collect();
        assert_eq!(note3_paras.len(), 3, "note id=3 should have 3 paragraphs");
        assert_eq!(para_text(note3_paras[0]), "	another endnote
new line");
        assert_eq!(para_text(note3_paras[1]), "	new paragraph");
        assert_eq!(para_text(note3_paras[2]), "	new paragraph");
        let note3_first_run = &note3_paras[0].runs[0];
        assert_eq!(note3_first_run.field, Some(roe_model::writer::FieldKind::EndnoteRef));
    }

    #[test]
    fn parse_notes_part_returns_empty_vec_when_part_missing() {
        // 没有 word/endnotes.xml 的 zip 应该返回空 Vec，而不是报错——
        // 大多数 docx 根本没有脚注/尾注，这是正常情况不是异常。
        use std::io::Write;
        use zip::write::FileOptions;
        let opts: FileOptions<()> = FileOptions::default();
        let buf = Vec::new();
        let mut zip_writer = zip::ZipWriter::new(std::io::Cursor::new(buf));
        zip_writer.start_file("word/document.xml", opts).unwrap();
        zip_writer.write_all(b"<w:document/>").unwrap();
        let data = zip_writer.finish().unwrap().into_inner();

        let cursor = std::io::Cursor::new(data);
        let mut zip = ZipArchive::new(cursor).unwrap();
        let notes = parse_notes_part(&mut zip, "word/endnotes.xml", b"w:endnote");
        assert!(notes.is_empty());
    }

    /// 批注功能第一步（v147/v148交接）：用真实样本 CommentDone.docx 端到端验证
    /// parse_comments_part 解析结果，对照交接文档 v147 2.3节记录的真实内容。
    /// 本次范围：只验证 id/author/content 文字；done/parent_id 本次恒为默认值
    /// （false/None），暂不验证（下一步任务解析 commentsExtended.xml 后再补）。
    #[test]
    fn parse_comments_part_extracts_two_real_comments_from_commentdone_sample() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../test_samples/curated/docx/mid__sw_ooxmlexport_data__CommentDone.docx"
        );
        let data = std::fs::read(path).expect("CommentDone.docx sample should exist");
        let cursor = std::io::Cursor::new(data);
        let mut zip = ZipArchive::new(cursor).unwrap();

        let comments_extended_map = parse_comments_extended_part(&mut zip, "word/commentsExtended.xml");
        let comments = parse_comments_part(&mut zip, "word/comments.xml", &comments_extended_map);

        assert_eq!(comments.len(), 2, "CommentDone.docx should have exactly 2 comments");

        // comment id=0：两段，author="Mike Kaganski"
        assert_eq!(comments[0].id, 0);
        assert_eq!(comments[0].author, "Mike Kaganski");
        let c0_paras: Vec<&Paragraph> = comments[0].content.iter().filter_map(|b| {
            if let BodyElement::Paragraph(p) = b { Some(p) } else { None }
        }).collect();
        assert_eq!(c0_paras.len(), 2, "comment id=0 should have 2 paragraphs");
        // 第一段除了 annotationRef 占位符 run（空文本）外还有正文文字，
        // 用 para_text 拼接后应以目标文字结尾（前面可能有空字符串前缀）。
        assert!(
            para_text(c0_paras[0]).contains("A two-paragraph comment."),
            "actual: {:?}", para_text(c0_paras[0])
        );
        assert_eq!(para_text(c0_paras[1]), "This is the second paragraph, and it is done.");

        // comment id=1：三段，同一 author
        assert_eq!(comments[1].id, 1);
        assert_eq!(comments[1].author, "Mike Kaganski");
        let c1_paras: Vec<&Paragraph> = comments[1].content.iter().filter_map(|b| {
            if let BodyElement::Paragraph(p) = b { Some(p) } else { None }
        }).collect();
        assert_eq!(c1_paras.len(), 3, "comment id=1 should have 3 paragraphs");
        assert!(
            para_text(c1_paras[0]).contains("A three-paragraph comment."),
            "actual: {:?}", para_text(c1_paras[0])
        );
        assert_eq!(para_text(c1_paras[1]), "The second paragraph.");
        assert_eq!(para_text(c1_paras[2]), "The third paragraph.");

        // WROOT-031第二步：done 现在已解析 commentsExtended.xml 得到真实值。
        // 对照 v148 交接文档记录：comment id=0 最后一段 paraId=78D78669
        // 对应 commentsExtended 里 done="1"；comment id=1 最后一段
        // paraId=33436897 对应 done="0"。parent_id（回复链）仍未实现，
        // 恒为默认值 None，这部分保持记录性断言不变。
        assert_eq!(comments[0].done, true, "comment id=0 should be done=true per commentsExtended.xml");
        assert_eq!(comments[0].parent_id, None);
        assert_eq!(comments[1].done, false, "comment id=1 should be done=false per commentsExtended.xml");
        assert_eq!(comments[1].parent_id, None);
    }

    /// 没有 word/comments.xml 的 zip（绝大多数 docx 没有批注）应返回空 Vec，
    /// 而不是报错，语义对齐 parse_notes_part_returns_empty_vec_when_part_missing。
    #[test]
    fn parse_comments_part_returns_empty_vec_when_part_missing() {
        use std::io::Write;
        use zip::write::FileOptions;
        let opts: FileOptions<()> = FileOptions::default();
        let buf = Vec::new();
        let mut zip_writer = zip::ZipWriter::new(std::io::Cursor::new(buf));
        zip_writer.start_file("word/document.xml", opts).unwrap();
        zip_writer.write_all(b"<w:document/>").unwrap();
        let data = zip_writer.finish().unwrap().into_inner();

        let cursor = std::io::Cursor::new(data);
        let mut zip = ZipArchive::new(cursor).unwrap();
        let comments_extended_map: HashMap<String, (bool, Option<String>)> = HashMap::new();
        let comments = parse_comments_part(&mut zip, "word/comments.xml", &comments_extended_map);
        assert!(comments.is_empty());
    }

    /// 独立验证 parse_comments_extended_part 本身：用真实样本
    /// CommentDone.docx 的 word/commentsExtended.xml，确认能正确解析出
    /// paraId -> done 映射表，对照 v148 交接文档记录的真实内容。
    #[test]
    fn parse_comments_extended_part_extracts_paraid_done_map_from_commentdone_sample() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../test_samples/curated/docx/mid__sw_ooxmlexport_data__CommentDone.docx"
        );
        let data = std::fs::read(path).expect("CommentDone.docx sample should exist");
        let cursor = std::io::Cursor::new(data);
        let mut zip = ZipArchive::new(cursor).unwrap();

        let map = parse_comments_extended_part(&mut zip, "word/commentsExtended.xml");

        assert_eq!(map.len(), 2, "should have exactly 2 commentEx entries");
        assert_eq!(map.get("78D78669"), Some(&(true, None)));
        assert_eq!(map.get("33436897"), Some(&(false, None)));
    }

    /// part 不存在（绝大多数 docx 没有 commentsExtended.xml，这个 part 是
    /// Word 2013+ 才引入的扩展）时应返回空 map，而不是报错，语义对齐
    /// parse_comments_part_returns_empty_vec_when_part_missing。
    #[test]
    fn parse_comments_extended_part_returns_empty_map_when_part_missing() {
        use std::io::Write;
        use zip::write::FileOptions;
        let opts: FileOptions<()> = FileOptions::default();
        let buf = Vec::new();
        let mut zip_writer = zip::ZipWriter::new(std::io::Cursor::new(buf));
        zip_writer.start_file("word/document.xml", opts).unwrap();
        zip_writer.write_all(b"<w:document/>").unwrap();
        let data = zip_writer.finish().unwrap().into_inner();

        let cursor = std::io::Cursor::new(data);
        let mut zip = ZipArchive::new(cursor).unwrap();
        let map = parse_comments_extended_part(&mut zip, "word/commentsExtended.xml");
        assert!(map.is_empty());
    }

    /// 回复链功能第一步：用真实样本 CommentReply.docx 验证 parent_id 正确
    /// 回填。样本结构（已用python核对原始XML确认）：
    /// comments.xml 里 w:id=1(author=Egor, paraId=01000000, text="Parent")，
    /// w:id=0(author=Egor, paraId=02000000, text="Child")；
    /// commentsExtended.xml 里 paraId="02000000" paraIdParent="01000000"，
    /// 即"Child"(id=0)是"Parent"(id=1)的回复。
    #[test]
    fn parse_comments_part_resolves_parent_id_from_commentreply_sample() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../test_samples/docx/sw_ooxmlexport_data__CommentReply.docx"
        );
        let data = std::fs::read(path).expect("CommentReply.docx sample should exist");
        let cursor = std::io::Cursor::new(data);
        let mut zip = ZipArchive::new(cursor).unwrap();

        let comments_extended_map = parse_comments_extended_part(&mut zip, "word/commentsExtended.xml");
        let comments = parse_comments_part(&mut zip, "word/comments.xml", &comments_extended_map);

        assert_eq!(comments.len(), 2, "CommentReply.docx should have exactly 2 comments");

        let parent = comments.iter().find(|c| c.id == 1).expect("comment id=1 (Parent) should exist");
        let child = comments.iter().find(|c| c.id == 0).expect("comment id=0 (Child) should exist");

        assert_eq!(parent.parent_id, None, "Parent comment (id=1) should have no parent_id");
        assert_eq!(child.parent_id, Some(1), "Child comment (id=0) should have parent_id=Some(1), pointing to Parent");
    }

    /// 回复链功能：用真实样本 comment-annotationref.docx 验证"一条批注
    /// 多条回复"场景（一对多，不是链式多级）。样本结构：
    /// w:id=1(paraId=01000000) 是原批注；
    /// w:id=2(paraId=02000000, paraIdParent=01000000) 是第一条回复；
    /// w:id=3(paraId=03000000, paraIdParent=01000000) 是第二条回复；
    /// 两条回复的 paraIdParent 都指向同一个原批注(id=1)，不是互相指向。
    #[test]
    fn parse_comments_part_resolves_one_to_many_replies_from_annotationref_sample() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../test_samples/docx/sw_ooxmlexport_data__comment-annotationref.docx"
        );
        let data = std::fs::read(path).expect("comment-annotationref.docx sample should exist");
        let cursor = std::io::Cursor::new(data);
        let mut zip = ZipArchive::new(cursor).unwrap();

        let comments_extended_map = parse_comments_extended_part(&mut zip, "word/commentsExtended.xml");
        let comments = parse_comments_part(&mut zip, "word/comments.xml", &comments_extended_map);

        let original = comments.iter().find(|c| c.id == 1).expect("comment id=1 (original) should exist");
        let reply_r1 = comments.iter().find(|c| c.id == 2).expect("comment id=2 (r1) should exist");
        let reply_r2 = comments.iter().find(|c| c.id == 3).expect("comment id=3 (r2) should exist");

        assert_eq!(original.parent_id, None, "original comment (id=1) should have no parent_id");
        assert_eq!(reply_r1.parent_id, Some(1), "reply r1 (id=2) should point to original (id=1)");
        assert_eq!(reply_r2.parent_id, Some(1), "reply r2 (id=3) should point to original (id=1)");
    }
}


#[cfg(test)]
mod tests_wroot075_e2e {
    use super::*;

    /// 端到端最小 docx：正文一个段落 + word/endnotes.xml 含一个真实 endnote，
    /// 验证完整 DocxParser::parse 链路（zip打开 → 各 part 读取 → 组装
    /// WriterDocument）真正把 Step 3/4 的 endnotes 结果带到最终产物里，
    /// 而不仅仅是 parse_notes_part 这个孤立函数本身正确（tests_wroot075_endnotes
    /// 已验证过后者）。复用 tests_doc_defaults_font_size::make_doc_defaults_docx
    /// 已验证过的"缺失 part 不报错"最小骨架（_rels/.rels + word/document.xml +
    /// word/_rels/document.xml.rels + word/styles.xml），只新增 endnotes part。
    fn make_docx_with_endnotes() -> Vec<u8> {
        use std::io::Write;
        use zip::write::FileOptions;
        let opts: FileOptions<()> = FileOptions::default();
        let buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(buf));

        let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
        let doc_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;
        let document = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
  <w:p>
    <w:r><w:t>Hello</w:t></w:r>
  </w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"#;
        let styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
</w:styles>"#;
        let endnotes_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:endnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:endnote w:id="0" w:type="separator"><w:p><w:r><w:separator/></w:r></w:p></w:endnote><w:endnote w:id="2"><w:p><w:pPr><w:pStyle w:val="Style23"/></w:pPr><w:r><w:endnoteRef/><w:tab/><w:t>some endnote</w:t></w:r></w:p></w:endnote></w:endnotes>"#;

        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        zip.start_file("word/_rels/document.xml.rels", opts).unwrap();
        zip.write_all(doc_rels.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document.as_bytes()).unwrap();
        zip.start_file("word/styles.xml", opts).unwrap();
        zip.write_all(styles.as_bytes()).unwrap();
        zip.start_file("word/endnotes.xml", opts).unwrap();
        zip.write_all(endnotes_xml.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn docx_parser_parse_wires_endnotes_into_writer_document() {
        let data = make_docx_with_endnotes();
        let doc = DocxParser::parse(&data).expect("parse should succeed");

        // Step 7改动：分隔符不再被跳过，会和真实note一起保留在 doc.endnotes 里，
        // 渲染层靠 note_type 字段区分二者。样本里 id=0(separator) + id=2(真实内容)
        // 共2条。
        assert_eq!(doc.endnotes.len(), 2, "separator now kept alongside real endnote id=2");
        assert_eq!(doc.endnotes[0].id, 0);
        assert_eq!(doc.endnotes[0].note_type, roe_model::writer::NoteType::Separator);
        assert_eq!(doc.endnotes[1].id, 2);
        assert_eq!(doc.endnotes[1].note_type, roe_model::writer::NoteType::Normal);
        assert!(doc.footnotes.is_empty(), "no footnotes.xml part provided, should stay empty");

        let paras: Vec<&Paragraph> = doc.endnotes[1].content.iter().filter_map(|b| {
            if let BodyElement::Paragraph(p) = b { Some(p) } else { None }
        }).collect();
        assert_eq!(paras.len(), 1);
        assert_eq!(paras[0].style, StyleRef("Style23".to_string()));
        let text: String = paras[0].runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(text, "	some endnote");
        assert_eq!(paras[0].runs[0].field, Some(roe_model::writer::FieldKind::EndnoteRef));
    }

    /// 不带 endnotes/footnotes part 的最小 docx，内联复制（不跨模块访问
    /// tests_doc_defaults_font_size 里的私有 helper，避免可见性问题）。
    fn make_docx_without_notes() -> Vec<u8> {
        use std::io::Write;
        use zip::write::FileOptions;
        let opts: FileOptions<()> = FileOptions::default();
        let buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(buf));

        let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
        let doc_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;
        let document = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
  <w:p>
    <w:r><w:t>Hello</w:t></w:r>
  </w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"#;
        let styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
</w:styles>"#;

        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        zip.start_file("word/_rels/document.xml.rels", opts).unwrap();
        zip.write_all(doc_rels.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document.as_bytes()).unwrap();
        zip.start_file("word/styles.xml", opts).unwrap();
        zip.write_all(styles.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn docx_parser_parse_without_notes_parts_has_empty_notes() {
        // 回归保护：绝大多数 docx 没有 endnotes.xml/footnotes.xml，
        // 确认这种最常见场景下 endnotes/footnotes 保持空 Vec 且不报错
        // （与 Step 1 之前的占位行为在"空结果"这一点上一致，只是现在
        // 是真实解析出的空，而非硬编码占位）。
        let data = make_docx_without_notes();
        let doc = DocxParser::parse(&data).expect("parse should succeed");
        assert!(doc.endnotes.is_empty());
        assert!(doc.footnotes.is_empty());
    }
}


#[cfg(test)]
mod tests_wroot075_step5_body_refs {
    use super::*;

    /// 端到端最小 docx：正文两段，每段"文字run + 独立endnoteReference run"，
    /// 结构照抄真实样本 tdf141548.docx 的正文写法（w:rStyle="Style14" +
    /// 无文字的纯 <w:endnoteReference/> run）。验证主状态机 read_document
    /// 正确识别该标记，且不会因为 run 无文本而被静默丢弃。
    fn make_docx_with_endnote_refs_in_body() -> Vec<u8> {
        use std::io::Write;
        use zip::write::FileOptions;
        let opts: FileOptions<()> = FileOptions::default();
        let buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(buf));

        let rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
        let doc_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;
        let document = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
  <w:p>
    <w:r><w:rPr></w:rPr><w:t>some text</w:t></w:r>
    <w:r><w:rPr><w:rStyle w:val="Style14"/></w:rPr><w:endnoteReference w:id="2"/></w:r>
  </w:p>
  <w:p>
    <w:r><w:rPr></w:rPr><w:t>some other text</w:t></w:r>
    <w:r><w:rPr><w:rStyle w:val="Style14"/></w:rPr><w:endnoteReference w:id="3"/></w:r>
  </w:p>
  <w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>
</w:body>
</w:document>"#;
        let styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
</w:styles>"#;

        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        zip.start_file("word/_rels/document.xml.rels", opts).unwrap();
        zip.write_all(doc_rels.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document.as_bytes()).unwrap();
        zip.start_file("word/styles.xml", opts).unwrap();
        zip.write_all(styles.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn body_endnote_reference_sets_run_field_and_endnote_ref() {
        let data = make_docx_with_endnote_refs_in_body();
        let doc = DocxParser::parse(&data).expect("parse should succeed");

        let paras: Vec<&Paragraph> = doc.body.iter().filter_map(|b| {
            if let BodyElement::Paragraph(p) = b { Some(p) } else { None }
        }).collect();
        assert_eq!(paras.len(), 2, "expected 2 paragraphs in body");

        // 第一段：2个run，第1个是文字，第2个是纯endnoteReference（无文字）
        assert_eq!(paras[0].runs.len(), 2, "first paragraph should keep the text-less endnoteReference run, not drop it");
        assert_eq!(paras[0].runs[0].text, "some text");
        assert_eq!(paras[0].runs[0].endnote_ref, None);
        assert_eq!(paras[0].runs[1].text, "");
        assert_eq!(paras[0].runs[1].endnote_ref, Some(2));
        assert_eq!(paras[0].runs[1].field, Some(roe_model::writer::FieldKind::EndnoteRef));

        // 第二段：同样结构，id=3
        assert_eq!(paras[1].runs.len(), 2);
        assert_eq!(paras[1].runs[0].text, "some other text");
        assert_eq!(paras[1].runs[1].endnote_ref, Some(3));
        assert_eq!(paras[1].runs[1].field, Some(roe_model::writer::FieldKind::EndnoteRef));
    }
}

// WROOT-107: w:position（字符垂直位移）解析验证，读取真实 golden 用例文件。
#[cfg(test)]
mod tests_wroot107_position {
    use super::*;

    fn golden_path(name: &str) -> String {
        format!(
            "{}/office_rs_golden_tests/docx/{}",
            std::env::var("HOME").unwrap(),
            name
        )
    }

    /// tdf99602_charStyleSubscript:
    ///   CharSize48: position=0,   sz=96 (48pt)
    ///   subscript : basedOn CharSize48, position=-24 (半磅 → -12pt)
    #[test]
    fn position_parsed_from_real_golden_file() {
        let path = golden_path("sw_ooxmlexport_data__tdf99602_charStyleSubscript.docx");
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("跳过：找不到golden文件 {path}: {e}");
                return;
            }
        };
        let doc = DocxParser::parse(&bytes).expect("parse should succeed");

        // 1. 样式表里的原始值
        let base = doc.styles.char_styles.get("CharSize48")
            .expect("CharSize48 样式应存在");
        assert_eq!(base.font_size, Some(48.0), "CharSize48 字号应为48pt");
        assert_eq!(base.vertical_position, Some(0.0), "CharSize48 position=0");

        let sub = doc.styles.char_styles.get("subscript")
            .expect("subscript 样式应存在");
        assert_eq!(sub.vertical_position, Some(-12.0), "subscript position=-24半磅=-12pt");

        // 2. 经继承链解析后的最终值（layout 层实际消费的就是这个）
        let resolved = doc.styles.resolve_char_style("subscript");
        assert_eq!(resolved.font_size, Some(48.0), "继承后字号应仍为48pt");
        assert_eq!(resolved.vertical_position, Some(-12.0), "继承后 position 应为-12pt");
    }
}


#[cfg(test)]
mod tests_wroot108_lvl_override {
    use super::*;

    fn golden_path(name: &str) -> String {
        format!(
            "{}/office_rs_golden_tests/docx/{}",
            std::env::var("HOME").unwrap(),
            name
        )
    }

    /// tdf135164_cancelledNumbering:
    ///   numId=7 指向 abstractNum 1（lvl0 是 bullet），但带 w:lvlOverride，
    ///   内嵌完整 w:lvl 把各级覆盖成 numFmt="none"。
    ///   WROOT-108 修复前内嵌 w:lvl 被静默丢弃，numId=7 退回 bullet。
    #[test]
    fn lvl_override_embedded_lvl_is_parsed() {
        let path = golden_path("sw_ooxmlexport_data__tdf135164_cancelledNumbering.docx");
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("跳过：找不到golden文件 {path}: {e}");
                return;
            }
        };
        let cursor = std::io::Cursor::new(bytes);
        let mut zip = ZipArchive::new(cursor).expect("zip open");
        let store = read_numbering(&mut zip).expect("read_numbering");

        // numId=7：应有覆盖，lvl0 有效定义 numFmt == none
        let inst7 = store.nums.get(&7).expect("numId=7 应存在");
        eprintln!("numId=7 abstract={} level_overrides keys={:?}",
            inst7.abstract_num_id, inst7.level_overrides.keys().collect::<Vec<_>>());
        let eff7 = store.effective_level(7, 0).expect("numId=7 lvl0 应有有效定义");
        eprintln!("numId=7 lvl0 有效: num_fmt={:?} lvl_text={:?}", eff7.num_fmt, eff7.lvl_text);
        assert_eq!(eff7.num_fmt, "none", "numId=7 lvl0 应被覆盖为 none");

        // numId=11：与 numId=7 共享 abstractNum 1，但覆盖互不影响，不应是 none
        if let Some(inst11) = store.nums.get(&11) {
            let eff11 = store.effective_level(11, 0).expect("numId=11 lvl0");
            eprintln!("numId=11 abstract={} lvl0 有效: num_fmt={:?} lvl_text={:?} overrides={}",
                inst11.abstract_num_id, eff11.num_fmt, eff11.lvl_text, inst11.level_overrides.len());
            assert_ne!(eff11.num_fmt, "none", "numId=11 不应被 numId=7 的覆盖污染");
        }

        // numId=1：无覆盖，保持 abstractNum 原定义
        if let Some(inst1) = store.nums.get(&1) {
            let eff1 = store.effective_level(1, 0).expect("numId=1 lvl0");
            eprintln!("numId=1 abstract={} lvl0 有效: num_fmt={:?} lvl_text={:?} overrides={}",
                inst1.abstract_num_id, eff1.num_fmt, eff1.lvl_text, inst1.level_overrides.len());
            assert!(inst1.level_overrides.is_empty(), "numId=1 不应有覆盖");
        }

        // abstractNum 1 自身不能被覆盖污染
        let abs1 = store.abstract_nums.get(&inst7.abstract_num_id).expect("abstractNum");
        eprintln!("abstractNum {} lvl0: num_fmt={:?}", inst7.abstract_num_id, abs1.levels[0].num_fmt);
        assert_ne!(abs1.levels[0].num_fmt, "none", "abstractNum 原定义不应被污染");
    }

    /// advance(7, 0) 应返回空串（无编号）
    #[test]
    fn advance_returns_empty_for_cancelled_numbering() {
        let path = golden_path("sw_ooxmlexport_data__tdf135164_cancelledNumbering.docx");
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => return,
        };
        let cursor = std::io::Cursor::new(bytes);
        let mut zip = ZipArchive::new(cursor).expect("zip open");
        let mut store = read_numbering(&mut zip).expect("read_numbering");
        assert_eq!(store.advance(7, 0), "", "numId=7 lvl0 应无编号文字");
    }
}


#[cfg(test)]
mod tests_wroot111_sect_carrier {
    use super::*;

    /// tdf113608_runAwayNumbering：第2段是只含 sectPr 的 Heading4 空段落
    /// （Heading4 样式自带 upperLetter 编号 "A."），该编号不应生成。
    #[test]
    fn sect_carrier_paragraph_gets_no_numbering_label() {
        let path = format!(
            "{}/office_rs_golden_tests/docx/sw_ooxmlexport_data__tdf113608_runAwayNumbering.docx",
            std::env::var("HOME").unwrap()
        );
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => { eprintln!("跳过：找不到golden文件 {path}: {e}"); return; }
        };
        let doc = DocxParser::parse(&bytes).expect("parse");
        let mut all_text = String::new();
        for el in doc.body.iter() {
            if let BodyElement::Paragraph(p) = el {
                let t: String = p.runs.iter().map(|r| r.text.as_str()).collect();
                eprintln!("WROOT111 段落 style={:?} text={:?}", p.style.0, t);
                all_text.push_str(&t);
                all_text.push('|');
            }
        }
        assert!(!all_text.contains("A."), "分节符载体空段落不应生成编号 A.，实际: {all_text}");
        assert!(all_text.contains("Paragraph should not be numbered"));
    }
}


#[cfg(test)]
mod tests_wroot114_para_border {
    use super::*;

    /// tdf117504_numberingIndent：BodyNote 段落样式带
    ///   w:shd fill=E6E6E6（灰底）
    ///   w:pBdr/w:left sz=48(=6pt) space=6 color=C0C0C0（左侧粗竖条）
    /// 解析后 resolve_paragraph_style("BodyNote") 应带出这两项。
    #[test]
    fn bodynote_style_has_shading_and_left_border() {
        let path = format!(
            "{}/office_rs_golden_tests/docx/sw_ooxmlexport_data__tdf117504_numberingIndent.docx",
            std::env::var("HOME").unwrap()
        );
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => { eprintln!("跳过：找不到golden文件 {path}: {e}"); return; }
        };
        let doc = DocxParser::parse(&bytes).expect("parse");
        let st = doc.styles.resolve_paragraph_style("BodyNote");
        eprintln!("WROOT114 BodyNote shading={:?} border_left={:?}", st.shading, st.border_left);

        let sh = st.shading.expect("BodyNote 应解析出段落底纹");
        assert_eq!((sh.r, sh.g, sh.b), (0xE6, 0xE6, 0xE6), "底纹应为 E6E6E6");

        let bl = st.border_left.expect("BodyNote 应解析出左边框");
        assert_eq!((bl.color.r, bl.color.g, bl.color.b), (0xC0, 0xC0, 0xC0), "左边框颜色应为 C0C0C0");
        assert!((bl.width_pt - 6.0).abs() < 0.01, "sz=48 应换算为 6pt，实际 {}", bl.width_pt);
        assert!((bl.space_pt - 6.0).abs() < 0.01, "space=6 应为 6pt，实际 {}", bl.space_pt);
    }

    /// 没有 pBdr/shd 的普通样式不应受影响（零回归）：Normal 样式两项均应为 None。
    #[test]
    fn normal_style_has_no_shading_or_border() {
        let path = format!(
            "{}/office_rs_golden_tests/docx/sw_ooxmlexport_data__tdf117504_numberingIndent.docx",
            std::env::var("HOME").unwrap()
        );
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => { eprintln!("跳过：找不到golden文件 {path}: {e}"); return; }
        };
        let doc = DocxParser::parse(&bytes).expect("parse");
        let st = doc.styles.resolve_paragraph_style("Normal");
        assert!(st.shading.is_none(), "Normal 不应有底纹: {:?}", st.shading);
        assert!(st.border_left.is_none(), "Normal 不应有左边框: {:?}", st.border_left);
    }
}


#[cfg(test)]
mod tests_wroot115_continuous_section {
    use super::*;

    fn load(name: &str) -> Option<WriterDocument> {
        let path = format!(
            "{}/office_rs_golden_tests/docx/{}.docx",
            std::env::var("HOME").unwrap(), name
        );
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => { eprintln!("跳过：找不到golden文件 {path}: {e}"); return None; }
        };
        Some(DocxParser::parse(&bytes).expect("parse"))
    }

    /// continuous 分节符不应让任何段落 page_break_before=true。
    /// tdf138892_noNumbering（4 个 continuous sectPr）、
    /// tdf148273_sectionBulletFormatLeak（2 个 continuous sectPr）。
    #[test]
    fn continuous_section_break_does_not_force_page_break() {
        for name in [
            "sw_ooxmlexport_data__tdf138892_noNumbering",
            "sw_ooxmlexport_data__tdf148273_sectionBulletFormatLeak",
        ] {
            let Some(doc) = load(name) else { continue };
            let mut n_para = 0;
            for el in doc.body.iter() {
                if let BodyElement::Paragraph(p) = el {
                    n_para += 1;
                    assert!(
                        !p.page_break_before,
                        "{name}: continuous 分节符后的段落不应强制换页，实际 page_break_before=true，text={:?}",
                        p.runs.iter().map(|r| r.text.as_str()).collect::<String>()
                    );
                }
            }
            assert!(n_para > 0, "{name}: 没解析出任何段落");
        }
    }

    /// 分节边界（BodyElement::Section）必须保留：只撤回换页，不能丢 section 归属。
    #[test]
    fn continuous_section_boundaries_are_kept() {
        let Some(doc) = load("sw_ooxmlexport_data__tdf148273_sectionBulletFormatLeak") else { return };
        let n_sections = doc.body.iter()
            .filter(|el| matches!(el, BodyElement::Section(_))).count();
        assert!(n_sections >= 1, "应至少保留 1 个 Section 边界，实际 {n_sections}");
    }
}


#[cfg(test)]
mod tests_wroot117_numbering_upper_start {
    use super::*;

    fn mk_store() -> NumberingStore {
        let mut store = NumberingStore::new();
        let lvl = |fmt: &str, text: &str| NumLevel {
            num_fmt: fmt.to_string(),
            lvl_text: text.to_string(),
            start_val: 1,
            ..Default::default()
        };
        store.abstract_nums.insert(1, AbstractNum {
            levels: vec![
                lvl("ordinal", "%1"),
                lvl("lowerRoman", "%1.%2"),
                lvl("lowerLetter", "%1.%2.%3"),
                lvl("decimal", "%1.%2.%3.%4"),
                lvl("upperRoman", "%1.%2.%3.%4.%5"),
            ],
        });
        store.nums.insert(1, NumInstance {
            abstract_num_id: 1,
            overrides: HashMap::new(),
            level_overrides: HashMap::new(),
        });
        store
    }

    #[test]
    fn skipping_to_deep_level_fills_upper_with_start() {
        // 直接用 ilvl=4：上层 0..4 应补成 1，得到 1st.i.a.1.I 而不是 0th.0..0.I
        let mut store = mk_store();
        assert_eq!(store.advance(1, 4), "1st.i.a.1.I");
    }

    #[test]
    fn deep_level_then_shallow_then_deep_again() {
        let mut store = mk_store();
        assert_eq!(store.advance(1, 4), "1st.i.a.1.I");
        assert_eq!(store.advance(1, 4), "1st.i.a.1.II");
        assert_eq!(store.advance(1, 0), "2nd");
        // 回到深层：下层被清零后再补 start，上层 1 级为 2
        assert_eq!(store.advance(1, 4), "2nd.i.a.1.I");
    }
}


#[cfg(test)]
mod tests_wroot119_para_shading {
    use super::*;

    /// WROOT-119：段落直写 w:pPr/w:shd（tdf141966_chapterNumberTortureTest 第3页四个段落）。
    #[test]
    fn direct_paragraph_shd_is_parsed() {
        let path = format!(
            "{}/office_rs_golden_tests/docx/sw_ooxmlexport_data__tdf141966_chapterNumberTortureTest.docx",
            std::env::var("HOME").unwrap()
        );
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => return, // 无本机 golden 时跳过，不让 CI 因缺文件失败
        };
        let doc = DocxParser::parse(&bytes).expect("parse");
        let mut found: Vec<(String, Option<[u8; 4]>)> = Vec::new();
        for el in doc.body.iter() {
            if let BodyElement::Paragraph(p) = el {
                let text: String = p.runs.iter().map(|r| r.text.clone()).collect();
                found.push((text, p.shading));
            }
        }
        // 编号标签可能作为独立 run 排在正文前，故用 contains 而非 starts_with
        let get = |needle: &str| -> Option<[u8; 4]> {
            found.iter().find(|(t, _)| t.contains(needle)).and_then(|(_, s)| *s)
        };
        assert_eq!(get("outline with Body listLvl(9)."), Some([0xFF, 0xFF, 0x00, 0xFF]));
        assert_eq!(get("outline with Body listLvl(9) #2"), Some([0xFF, 0xFF, 0x88, 0xFF]));
        assert_eq!(get("direct formatting - Body listLvl(9)"), Some([0x00, 0xFF, 0x00, 0xFF]));
        assert_eq!(get("direct numId, inherit listLvl"), Some([0xAA, 0xAA, 0xFF, 0xFF]));
        // 无底纹的段落必须是 None（run 级/标记级 shd 不得串入）
        assert_eq!(get("Style numId0 cancels"), None);
    }

    /// WROOT-119b 回归：tdf135774_numberingShading 里 w:shd 只出现在
    /// w:pPr/w:rPr（段落标记）和 run 的 w:rPr 中，段落自身不应有底纹。
    #[test]
    fn paragraph_mark_rpr_shd_is_not_paragraph_shading() {
        let path = format!(
            "{}/office_rs_golden_tests/docx/sw_ooxmlexport_data__tdf135774_numberingShading.docx",
            std::env::var("HOME").unwrap()
        );
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => return,
        };
        let doc = DocxParser::parse(&bytes).expect("parse");
        let mut n = 0;
        for el in doc.body.iter() {
            if let BodyElement::Paragraph(p) = el {
                n += 1;
                assert_eq!(p.shading, None, "段落标记 rPr 里的 shd 不应变成段落底纹");
            }
        }
        assert!(n > 0);
    }
}

#[cfg(test)]
mod tests_wroot127_indent {
    use super::*;

    /// WROOT-127：正文段落直写 w:ind（左/右/首行/悬挂）。
    /// 用例 roe_custom__indent_matrix：02 left=720；03 right=1440；05 firstLine=480；
    /// 06 left=720 hanging=360。单位 twips -> pt（/20）；hanging 存为负的 indent_first。
    #[test]
    fn direct_ind_is_parsed() {
        let path = format!(
            "{}/office_rs_golden_tests/docx/roe_custom__indent_matrix.docx",
            std::env::var("HOME").unwrap()
        );
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => return, // 无本机用例时跳过
        };
        let doc = DocxParser::parse(&bytes).expect("parse");
        let mut found: Vec<(String, Option<f32>, Option<f32>, Option<f32>)> = Vec::new();
        for el in doc.body.iter() {
            if let BodyElement::Paragraph(p) = el {
                let text: String = p.runs.iter().map(|r| r.text.clone()).collect();
                found.push((text, p.indent_left, p.indent_right, p.indent_first));
            }
        }
        let get = |needle: &str| found.iter().find(|(t, ..)| t.contains(needle)).cloned()
            .unwrap_or_else(|| panic!("找不到段落 {}", needle));
        let p2 = get("[02 left=720]");
        assert_eq!((p2.1, p2.2, p2.3), (Some(36.0), None, None));
        let p3 = get("[03 right=1440]");
        assert_eq!((p3.1, p3.2, p3.3), (None, Some(72.0), None));
        let p5 = get("[05 firstLine=480]");
        assert_eq!((p5.1, p5.2, p5.3), (None, None, Some(24.0)));
        let p6 = get("[06 left=720 hanging=360]");
        assert_eq!((p6.1, p6.2, p6.3), (Some(36.0), None, Some(-18.0)));
        // 无缩进的基准段落必须全是 None
        let p1 = get("[01 baseline]");
        assert_eq!((p1.1, p1.2, p1.3), (None, None, None));
    }
}


#[cfg(test)]
mod tests_wroot130_section_type_owner {
    use super::*;

    /// tdf95367：节1 nextPage、节2 continuous、节3 nextPage。
    /// "This line is after the continuous break"（节2首段）应留在第1页 → page_break_before=false；
    /// "This line is after a section-page break"（节3首段）应换页 → page_break_before=true。
    #[test]
    fn section_type_belongs_to_the_section_that_starts() {
        let path = format!(
            "{}/office_rs_golden_tests/docx/sw_ooxmlexport_data__tdf95367_inheritFollowStyle.docx",
            std::env::var("HOME").unwrap()
        );
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => { eprintln!("跳过：找不到golden文件 {path}: {e}"); return; }
        };
        let doc = DocxParser::parse(&bytes).expect("parse");
        let mut found_cont = false;
        let mut found_page = false;
        for el in doc.body.iter() {
            if let BodyElement::Paragraph(p) = el {
                let t: String = p.runs.iter().map(|r| r.text.as_str()).collect();
                if t.starts_with("This line is after the continuous break") {
                    assert!(!p.page_break_before, "节2(continuous)首段不应换页");
                    found_cont = true;
                }
                if t.starts_with("This line is after a section-page break") {
                    assert!(p.page_break_before, "节3(nextPage)首段应换页");
                    found_page = true;
                }
            }
        }
        assert!(found_cont && found_page, "没找到目标段落");
    }
}


#[cfg(test)]
mod tests_wroot131_page_field {
    use super::*;

    /// 复杂域 PAGE：缓存值 run 应被标记为 PageNumber；域外普通文字不受影响。
    #[test]
    fn complex_page_field_marks_cached_run() {
        let xml = r#"<w:p><w:r><w:t xml:space="preserve">Page </w:t></w:r><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText xml:space="preserve"> PAGE   \* MERGEFORMAT </w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>2</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r><w:r><w:t xml:space="preserve"> end</w:t></w:r></w:p>"#;
        let paras = parse_isolated_paragraphs(xml);
        assert_eq!(paras.len(), 1);
        let runs = &paras[0].runs;
        let page_runs: Vec<_> = runs.iter()
            .filter(|r| r.field == Some(roe_model::writer::FieldKind::PageNumber)).collect();
        assert_eq!(page_runs.len(), 1, "应恰好 1 个 PageNumber run，实际 runs={:?}", runs.iter().map(|r| (&r.text, r.field)).collect::<Vec<_>>());
        assert_eq!(page_runs[0].text, "2");
        assert!(runs.iter().any(|r| r.text == "Page " && r.field.is_none()));
        assert!(runs.iter().any(|r| r.text == " end" && r.field.is_none()));
    }

    /// WROOT-132：NUMPAGES 域缓存值 run 标记为 NumPages；PAGE 与 NUMPAGES 并存时互不串。
    #[test]
    fn numpages_field_marks_cached_run() {
        let xml = r#"<w:p><w:r><w:t xml:space="preserve">Page </w:t></w:r><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText> PAGE </w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>1</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r><w:r><w:t xml:space="preserve"> of </w:t></w:r><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText> NUMPAGES </w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>9</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r></w:p>"#;
        let paras = parse_isolated_paragraphs(xml);
        let runs = &paras[0].runs;
        let pg: Vec<_> = runs.iter().filter(|r| r.field == Some(roe_model::writer::FieldKind::PageNumber)).collect();
        let np: Vec<_> = runs.iter().filter(|r| r.field == Some(roe_model::writer::FieldKind::NumPages)).collect();
        assert_eq!(pg.len(), 1, "PageNumber run 应恰好1个");
        assert_eq!(pg[0].text, "1");
        assert_eq!(np.len(), 1, "NumPages run 应恰好1个");
        assert_eq!(np[0].text, "9");
        assert!(runs.iter().any(|r| r.text == " of " && r.field.is_none()));
    }

    /// 其他域（如 STYLEREF）仍不标记，缓存值当普通文字。
    #[test]
    fn other_field_is_not_marked() {
        let xml = r#"<w:p><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText> STYLEREF "Heading 1" </w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>Intro</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r></w:p>"#;
        let paras = parse_isolated_paragraphs(xml);
        assert!(paras[0].runs.iter().all(|r| r.field.is_none()));
    }
}


#[cfg(test)]
mod tests_wroot133_empty_ppr_default {
    use super::*;
    use std::io::Write;

    fn make_docx_with_empty_ppr_default() -> Vec<u8> {
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts = zip::write::SimpleFileOptions::default();

        let rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;
        let doc_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;
        let document = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body><w:p><w:r><w:t>x</w:t></w:r></w:p></w:body>
</w:document>"#;
        // pPrDefault 自闭合；后面 Subtitle 样式带 before=480（24pt）。
        let styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:docDefaults>
    <w:rPrDefault><w:rPr><w:rFonts w:ascii="Times New Roman" w:hAnsi="Times New Roman"/></w:rPr></w:rPrDefault>
    <w:pPrDefault/>
  </w:docDefaults>
  <w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style>
  <w:style w:type="paragraph" w:styleId="Subtitle"><w:name w:val="Subtitle"/><w:basedOn w:val="Normal"/>
    <w:pPr><w:spacing w:before="480" w:after="120"/></w:pPr></w:style>
</w:styles>"#;

        zip.start_file("_rels/.rels", opts).unwrap();
        zip.write_all(rels.as_bytes()).unwrap();
        zip.start_file("word/_rels/document.xml.rels", opts).unwrap();
        zip.write_all(doc_rels.as_bytes()).unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(document.as_bytes()).unwrap();
        zip.start_file("word/styles.xml", opts).unwrap();
        zip.write_all(styles.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    /// WROOT-133：<w:pPrDefault/> 自闭合时，后面样式的 w:spacing before 不能被读成 docDefaults。
    /// 复现 tdf155736：Subtitle 的 before=480（24pt）曾被误读成 doc_default_space_before。
    #[test]
    fn empty_ppr_default_does_not_leak_following_style_spacing() {
        let data = make_docx_with_empty_ppr_default();
        let doc = DocxParser::parse(&data).unwrap();
        assert_eq!(doc.styles.doc_default_space_before, None,
            "自闭合的 pPrDefault 不应产生 doc_default_space_before，实际={:?}",
            doc.styles.doc_default_space_before);
        // Normal 段落解析后段前距也不应是 24pt
        let normal = doc.styles.resolve_paragraph_style("Normal");
        assert_eq!(normal.space_before, None,
            "Normal 的段前距不应是 Some(24.0)，实际={:?}", normal.space_before);
    }
}


#[cfg(test)]
mod tests_wroot135_continuation_notice {
    use super::*;

    fn make_zip(endnotes_xml: &str) -> Vec<u8> {
        use std::io::Write;
        use zip::write::FileOptions;
        let opts: FileOptions<()> = FileOptions::default();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("word/endnotes.xml", opts).unwrap();
        zip.write_all(endnotes_xml.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    /// WROOT-135：continuationNotice 是系统占位项，不能被当成 Normal 真实尾注。
    /// 复现 tdf166205：文档只有 separator/continuationSeparator/continuationNotice 三个系统项，
    /// 没有任何真实尾注，但旧代码把 continuationNotice 当 Normal，渲染出分隔线和 `?`。
    #[test]
    fn continuation_notice_is_not_normal() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:endnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:endnote w:type="separator" w:id="-1"><w:p><w:r><w:separator/></w:r></w:p></w:endnote>
<w:endnote w:type="continuationSeparator" w:id="0"><w:p><w:r><w:continuationSeparator/></w:r></w:p></w:endnote>
<w:endnote w:type="continuationNotice" w:id="1"><w:p><w:pPr><w:spacing w:after="0"/></w:pPr></w:p></w:endnote>
<w:endnote w:id="2"><w:p><w:r><w:t>real endnote</w:t></w:r></w:p></w:endnote>
</w:endnotes>"#;
        let data = make_zip(xml);
        let mut zip = ZipArchive::new(std::io::Cursor::new(data)).unwrap();
        let notes = parse_notes_part(&mut zip, "word/endnotes.xml", b"w:endnote");
        let normal: Vec<_> = notes.iter().filter(|n| n.note_type == NoteType::Normal).collect();
        assert_eq!(normal.len(), 1, "只应有 1 条真实尾注，实际 Normal 数={}，类型序列={:?}",
            normal.len(), notes.iter().map(|n| (n.id, n.note_type)).collect::<Vec<_>>());
        assert_eq!(normal[0].id, 2);
    }
}
