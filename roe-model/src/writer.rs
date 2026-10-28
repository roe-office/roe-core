use serde::{Deserialize, Serialize};
use slotmap::new_key_type;
use crate::style::StyleRegistry;
use crate::common::{StyleRef, ImageData};
pub use crate::common::{Table, TableRow, TableCell, CellContent};

new_key_type! { pub struct ParagraphId; }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriterDocument {
    pub styles:    StyleRegistry,
    pub body:      Vec<BodyElement>,
    pub metadata:  DocumentMeta,
    /// 页面尺寸（宽, 高），单位 pt。默认 A4：595.28 × 841.89
    pub page_size: (f32, f32),
    /// 页眉内容（WROOT-004：从纯文本升级为 default/even/first 三态段落集合）
    #[serde(default)] pub header: HeaderFooterSet,
    /// 页脚内容（语义同 header）
    #[serde(default)] pub footer: HeaderFooterSet,
    /// 对应 docx sectPr 的 w:titlePg：是否首页使用独立的 header.first/footer.first
    #[serde(default)] pub title_page: bool,
    /// 对应 docx settings.xml 的 w:evenAndOddHeaders：是否奇偶页使用不同的 header/footer
    #[serde(default)] pub even_and_odd_headers: bool,
    /// 页边距（上, 下, 左, 右），单位 pt。默认 A4 常用边距
    #[serde(default)] pub margin_top:    f32,
    #[serde(default)] pub margin_bottom: f32,
    #[serde(default)] pub margin_left:   f32,
    #[serde(default)] pub margin_right:  f32,
    /// 分栏（对应 docx sectPr 的 w:cols）。默认单栏（num_cols=1），
    /// 此时 col_widths/col_spacing 无意义、保持为空 Vec。
    #[serde(default)] pub columns: DocumentColumns,
    /// STEP6-STAGE-B：对应 docx sectPr 里 w:endnotePr/w:numFmt 的 w:val。
    /// 驱动正文里 note-ref 上标编号的数字格式化（如 lowerRoman 显示为
    /// i/ii/iii）。默认 Arabic（docx 规范里 numFmt 缺省即阿拉伯数字）。
    /// 只在 WriterDocument 顶层存一份（不按 section 拆分）：多节各自不同
    /// numFmt 是极端边缘情况，当前样本集没有覆盖，先不支持，等真实样本
    /// 出现再扩展成按 section 存储（参照 margin_top 等字段现有的简化处理）。
    #[serde(default)] pub endnote_num_fmt: NumFormat,
    /// 语义同 endnote_num_fmt，对应 w:footnotePr/w:numFmt。
    #[serde(default)] pub footnote_num_fmt: NumFormat,
    /// WROOT任务B第一步新增：文档按 sectPr 切分出的各节数据。为空表示
    /// "单节文档"或"解析器尚未产出分节数据"，此时应使用上面顶层字段
    /// 作为唯一依据（顶层字段语义等价于"最后一节"，与此前多sectPr场景
    /// 下的历史行为一致，保证不引入回归）。
    /// 阶段性限制（本次未完成，留给后续任务）：目前 parser 只把每节的
    /// header/footer/title_page 引用数据正确填充进了各个 Section，但
    /// Section.content 恒为空 Vec——body 尚未按 sectPr 边界真正切分，
    /// 仍然是拍平存放。渲染层也尚未读取这个字段。
    #[serde(default)] pub sections: Vec<Section>,
    /// 文档级脚注集合（docx word/footnotes.xml），按 Note.id 供 Run.footnote_ref 查找。
    #[serde(default)] pub footnotes: Vec<Note>,
    /// 文档级尾注集合（docx word/endnotes.xml），按 Note.id 供 Run.endnote_ref 查找。
    #[serde(default)] pub endnotes: Vec<Note>,
    /// 文档级批注集合（docx word/comments.xml），按 Comment.id 供
    /// Run.comment_range_start/comment_range_end/comment_ref 查找。
    #[serde(default)] pub comments: Vec<Comment>,
}

impl Default for WriterDocument {
    fn default() -> Self {
        Self {
            styles:    StyleRegistry::default(),
            body:      Vec::new(),
            metadata:  DocumentMeta::default(),
            page_size: (595.28, 841.89), // A4
            header: HeaderFooterSet::default(),
            footer: HeaderFooterSet::default(),
            title_page: false,
            even_and_odd_headers: false,
            margin_top:    56.69, // 2cm
            margin_bottom: 56.69,
            margin_left:   70.87, // 2.5cm
            margin_right:  70.87,
            columns: DocumentColumns::default(),
            sections: Vec::new(),
            footnotes: Vec::new(),
            endnotes: Vec::new(),
            comments: Vec::new(),
            endnote_num_fmt: NumFormat::Arabic,
            footnote_num_fmt: NumFormat::Arabic,
        }
    }
}

/// 分栏定义，对应 docx sectPr 的 w:cols。
/// - num_cols=1（默认）：单栏，无需分栏，col_widths/col_spacing 保持空。
/// - equalWidth=1（或省略，OOXML 默认值）：col_widths/col_spacing 留空，
///   排版层按 num_cols 对内容宽度均分即可，间距统一用 uniform_spacing。
/// - equalWidth=0：col_widths/col_spacing 逐栏填充，长度均应等于 num_cols；
///   col_spacing 的语义是"该栏之后的间距"，最后一栏通常为 0（docx里常省略）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentColumns {
    #[serde(default = "default_num_cols")] pub num_cols: usize,
    /// equalWidth=1 时统一栏间距（单位 pt）；equalWidth=0 时不使用，见 col_spacing。
    #[serde(default)] pub uniform_spacing: f32,
    /// equalWidth=0 时逐栏宽度（单位 pt），为空表示 equalWidth=1（或单栏）。
    #[serde(default)] pub col_widths: Vec<f32>,
    /// equalWidth=0 时逐栏（该栏之后的）间距（单位 pt），为空表示 equalWidth=1（或单栏）。
    #[serde(default)] pub col_spacing: Vec<f32>,
}

fn default_num_cols() -> usize { 1 }

impl Default for DocumentColumns {
    fn default() -> Self {
        Self {
            num_cols: 1,
            uniform_spacing: 0.0,
            col_widths: Vec::new(),
            col_spacing: Vec::new(),
        }
    }
}

/// WROOT-004: 页眉/页脚内容集合，对应 docx sectPr 的 headerReference/footerReference
/// type="default"/"even"/"first" 三种引用。default 对应奇数页/无区分时的通用内容；
/// even 仅当 WriterDocument.even_and_odd_headers=true 时生效；
/// first 仅当 WriterDocument.title_page=true 时生效。缺失的槽位在渲染时退化到 default。
/// 内容类型是 Vec<Paragraph>（复用 body 的段落/run 结构，而非纯字符串），
/// 这样才能承载 Run.field（页码等域）这种"每页动态计算"的内容。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HeaderFooterSet {
    pub default: Option<Vec<Paragraph>>,
    pub even:    Option<Vec<Paragraph>>,
    pub first:   Option<Vec<Paragraph>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BodyElement {
    Paragraph(Paragraph),
    Table(Table),
    Section(Section),
    Image(InlineImage),
    Frame(FloatFrame),
    Shape(Shape),
    InlineChart { model: crate::impress::ChartModel, width_pt: f32, height_pt: f32 },
    /// chartEx 统计图表(直方图/箱线图等),保存原始数据,渲染时再计算。
    InlineStatChart { model: crate::stat_chart::StatChartModel, width_pt: f32, height_pt: f32 },
}

/// 收窄转换：BodyElement -> CellContent，供表格单元格构造使用。
/// Section（分节符）和 Frame（浮动框）不是合法的单元格内容，转换失败。
/// InlineImage 转换时丢弃锚定/环绕相关字段，因为单元格内图片不存在这些语义。
impl TryFrom<BodyElement> for CellContent {
    type Error = &'static str;
    fn try_from(elem: BodyElement) -> Result<Self, Self::Error> {
        match elem {
            BodyElement::Paragraph(p) => Ok(CellContent::Paragraph(p)),
            BodyElement::Image(img) => Ok(CellContent::Image { image: img.image, width_pt: img.width_pt, height_pt: img.height_pt }),
            BodyElement::Table(t) => Ok(CellContent::Table(Box::new(t))),
            BodyElement::Section(_) => Err("Section 不是合法的单元格内容"),
            BodyElement::Frame(_) => Err("Frame（浮动框）不是合法的单元格内容"),
            BodyElement::Shape(_) => Err("Shape（形状）不是合法的单元格内容"),
            BodyElement::InlineChart { .. } => Err("InlineChart 不是合法的单元格内容"),
            BodyElement::InlineStatChart { .. } => Err("InlineStatChart 不是合法的单元格内容"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookmarkAnchor {
    pub id:        u32,
    pub name:      String,
    /// 书签在段落内的位置：插在第几个 run 之前（0 = 段落开头）
    pub run_index: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Paragraph {
    pub style:     StyleRef,
    pub runs:      Vec<Run>,
    pub list_info: Option<ListInfo>,
    /// PPTX 专用的项目符号/编号信息（OOXML <a:pPr> 内联 bullet 语义）。
    /// 与 list_info 是两套独立体系，不要混用——list_info 服务 docx/ODF
    /// 的命名列表样式，bullet 服务 PPTX 的内联 buChar/buAutoNum。docx
    /// 侧该字段恒为 None。详见 roe_model::impress::BulletInfo 的文档注释。
    #[serde(default)] pub bullet: Option<crate::impress::BulletInfo>,
    /// PROOT-tdf158512: bullet 是否完全来自 master txStyles 继承（段落自身
    /// <a:pPr> 未显式声明 buChar/buAutoNum/buNone）。用于 layout lstStyle
    /// <a:buNone/> 覆盖判断——layout buNone 应能覆盖 master 继承来的 bullet，
    /// 但不应覆盖段落自身显式设置的 bullet（后者优先级更高，理应保留）。
    /// docx/ODF 侧该字段恒为 false，不受影响。
    #[serde(default)] pub bullet_from_master: bool,
    /// PPTX 专用：段落缩进级别（OOXML <a:pPr lvl="n">，0-based，缺省视为0）。
    /// 用于 master <p:txStyles> 按 lvlNpPr 分级取字号/样式兜底时定位对应级别。
    /// docx/ODF 侧该字段恒为 None，与 bullet 字段同源同理由。
    #[serde(default)] pub lvl: Option<usize>,
    /// 内联段落格式（覆盖样式表）
    #[serde(default)] pub alignment:    Option<crate::style::Alignment>,
    #[serde(default)] pub indent_left:  Option<f32>,
    #[serde(default)] pub indent_first: Option<f32>,
    /// WROOT-127：段落直写右缩进（w:ind w:right / w:end），单位 pt。样式级右缩进另行处理。
    #[serde(default)] pub indent_right: Option<f32>,
    /// WROOT-119：段落直写底纹（w:pPr/w:shd 的 w:fill），RGBA。优先于段落样式的 shading。
    #[serde(default)] pub shading: Option<[u8; 4]>,
    #[serde(default)] pub space_before: Option<f32>,
    #[serde(default)] pub space_after:  Option<f32>,
    #[serde(default)] pub line_height:  Option<f32>,
    /// v271: w:lineRule=exact/atLeast 的绝对行高（pt，= w:line/20）。仅当 line_exact 为 Some 时生效。
    #[serde(default)] pub line_abs_pt:  Option<f32>,
    /// v271: Some(true)=exact 固定值，Some(false)=atLeast 最小值，None=auto（按 line_height 倍数）
    #[serde(default)] pub line_exact:   Option<bool>,
    /// 段落内书签锚点（保留 run_index 供未来精确跳转用）
    #[serde(default)] pub bookmarks: Vec<BookmarkAnchor>,
    /// 强制该段落从新的一页开始渲染。
    /// 来源：①docx段落属性 w:pPr/w:pageBreakBefore
    ///       ②docx段落内嵌 w:sectPr（分节符，代表"上一段结束、
    ///         此段落所在为新section"，MVP阶段一律视为强制换页，
    ///         不区分nextPage/evenPage/oddPage/continuous类型）
    /// 参见 WROOT-001（test_samples/curated_progress_docx/bug_log.md）
    #[serde(default)] pub page_break_before: bool,
    /// 强制在这一段排完之后换到下一栏（仅在文档启用多栏 w:cols 时有意义；
    /// 单栏文档该字段应恒为false）。注意与 page_break_before 语义方向相反：
    /// page_break_before 是"排这段之前先换页"，这个字段是"排完这段之后
    /// 再换栏"——因为真实样本（食谱.docx）证实 Word/LibreOffice/OnlyOffice
    /// 都把 <w:br w:type="column"/> 所在的段落当成一个正常参与排版的空段落
    /// （占一行空白高度），换栏发生在排完这个空段落之后，紧跟着的下一段
    /// 从新栏顶部开始。最初设计（v58 2.1节草案）曾尝试把该段落完全吞掉、
    /// 把标志转移给下一段，实测与参考渲染对比后发现会丢失这一行空白，
    /// 已改为本方案：该段落自己保留、自己标记 column_break_after=true。
    #[serde(default)] pub column_break_after: bool,

    /// 该段落自身承载一个 <w:br w:type="page"/> 显式分页符（区别于
    /// page_break_before 来自段落属性 w:pageBreakBefore 或段内嵌 sectPr，
    /// 这个字段来自段落内部 run 里的内联换页标记）。与 column_break_after
    /// 语义相反：真实渲染（LibreOffice截图确认）证实这个段落本身不产生
    /// 任何可见行——不是"占一行再换栏"，而是"完全不可见，直接换页"。
    /// layout层需要在空段落占位分支之前拦截这个标记，跳过整个段落
    /// （不产生LineBox/frame），并让下一段落从新页顶部开始。
    #[serde(default)] pub page_break_after: bool,

    /// WROOT-068: 本段落的 tab stop 定义列表（对应 w:pPr/w:tabs 下的
    /// 若干 w:tab）。与 space_before/indent_left 等其他"内联段落格式"
    /// 字段同样直接挂在 Paragraph 上、不走样式表继承链——真实文档里
    /// w:tabs 几乎总是段落自身的直接定义（TOC 场景尤其如此），继承链
    /// 场景（w:tabs 定义在具名段落样式上）暂不支持，留空 Vec 时渲染层
    /// 应回退到原有 72pt 栅格兜底逻辑，不产生回归。
    /// 保持按 pos 从小到大排序（解析层负责排序），渲染层按顺序查找
    /// "第一个 pos > 当前x" 的 tab stop。
    #[serde(default)] pub tab_stops: Vec<TabStop>,
}

/// WROOT-068: 段落级 tab stop 定义（对应 OOXML w:pPr/w:tabs/w:tab）。
/// 与 Run 内部 '\t' 字符本身是两回事——TabStop 描述"遇到 tab 字符时应该
/// 跳到哪个精确位置、按什么方式对齐、中间空白用什么符号填充"，后者只是
/// 一个普通字符，真正的定位/对齐/引导线渲染完全依赖这里的数据。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TabStop {
    /// 目标位置，单位 pt，相对页面内容区左边界（与 indent_left 等字段
    /// 同一坐标系）。解析时从 OOXML w:pos（twips）换算：pt = twips / 20.0
    pub pos: f32,
    pub alignment: TabAlignment,
    pub leader: TabLeader,
}

/// 对应 OOXML w:tab/@w:val。left 最常见（等价于普通制表位，文字从
/// pos 处开始往右排）；right 用于目录页码对齐等场景（文字右边缘对齐到
/// pos，需要先测量 tab 后文字宽度再反推起笔位置）；center 同理但双向
/// 各让一半。clear 表示"清除某个继承来的 tab stop"，不产生可见效果，
/// 解析时按 WROOT-068 交接文档建议简化处理：忽略，不计入 tab_stops。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TabAlignment {
    Left,
    Center,
    Right,
}

/// 对应 OOXML w:tab/@w:leader。tab 起点到目标位置之间的空白区域用什么
/// 符号填充。None 表示不填充（纯空白，此前 expand_tabs_to_spaces 兜底
/// 方案的效果）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TabLeader {
    None,
    Dot,
    Hyphen,
    Underscore,
}

/// 文本段，不存字形/BiDi——那是 Layout 层的事
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Run {
    pub text:        String,
    pub char_style:  Option<StyleRef>,
    /// 内联格式（覆盖段落样式）
    #[serde(default)] pub bold:        Option<bool>,
    #[serde(default)] pub italic:      Option<bool>,
    #[serde(default)] pub underline:   Option<bool>,
    /// 下划线样式（None 时若 underline=true，按 Single 处理，见 UnderlineStyle 枚举注释）
    #[serde(default)] pub underline_style: Option<UnderlineStyle>,
    #[serde(default)] pub font_size:   Option<f32>,
    #[serde(default)] pub font_family: Option<String>,
    #[serde(default)] pub font_family_ascii: Option<String>,
    /// 超链接 URL（非空时此 Run 是超链接）
    #[serde(default)] pub hyperlink: Option<String>,
    /// 脚注引用（非 None 时此 Run 是脚注引用锚点，值为对应 Note.id，
    /// 内容需去 WriterDocument.footnotes 里查）。
    #[serde(default)] pub footnote_ref: Option<u32>,
    /// 尾注引用（语义同 footnote_ref，值对应 WriterDocument.endnotes 里的 Note.id）。
    #[serde(default)] pub endnote_ref: Option<u32>,
    /// 批注范围起点（comment功能第一步，仅加字段不加解析逻辑）：此 Run 之前，
    /// 哪些批注范围从这里开始（对应 OOXML <w:commentRangeStart w:id="N"/>）。
    /// 一个 Vec 是因为同一位置可能有多个批注范围同时起始（嵌套/相邻批注）。
    #[serde(default)] pub comment_range_start: Vec<u32>,
    /// 批注范围终点，语义同上，对应 <w:commentRangeEnd w:id="N"/>。
    #[serde(default)] pub comment_range_end: Vec<u32>,
    /// 批注图标占位符：非 None 时此 Run 是批注引用锚点（对应 OOXML
    /// <w:commentReference w:id="N"/>），值为对应 Comment.id。
    #[serde(default)] pub comment_ref: Option<u32>,
    /// 字体颜色
    #[serde(default)] pub color:         Option<crate::common::Color>,
    /// 高亮/背景色
    #[serde(default)] pub background:    Option<crate::common::Color>,
    /// 删除线
    #[serde(default)] pub strikethrough: Option<bool>,
    /// 上标/下标
    #[serde(default)] pub script:        Option<ScriptPosition>,
    /// WROOT-107: 字符基线垂直位移（对应 OOXML w:position，单位pt；
    /// 正值上移，负值下移；与 script 是两套独立机制，可并存）。
    #[serde(default)] pub vertical_position: Option<f32>,
    /// 行内公式（非空时此 Run 携带公式，text 字段留空或存占位符）
    #[serde(default)] pub formula: Option<Formula>,
    /// 域（field）语义标记（WROOT-004新增）。非 None 时说明此 Run 是一个动态域
    /// （如页码），text 字段是 Word/解析时缓存的静态值，仅在渲染器无法计算
    /// 真实值时用作兜底显示；正常渲染应忽略 text，按 FieldKind 动态求值。
    #[serde(default)] pub field: Option<FieldKind>,
}

/// WROOT-004: Run 携带的域（field）类型，目前只覆盖页码相关的两种。
/// 对应 OOXML w:instrText 里的域代码（如 "PAGE"、"NUMPAGES"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FieldKind {
    /// 对应域代码 PAGE：当前页码
    PageNumber,
    /// 对应域代码 NUMPAGES：文档总页数
    NumPages,
    /// 对应 OOXML <w:footnoteRef/>：脚注内部的编号占位符（渲染时替换为该
    /// 脚注在文档中的实际序号，不是 Run.text 缓存的静态值）。
    FootnoteRef,
    /// 对应 OOXML <w:endnoteRef/>：尾注内部的编号占位符，语义同 FootnoteRef。
    EndnoteRef,
}

/// 脚注/尾注的内容（脚注 footnote 和尾注 endnote 结构完全一致，共用此类型）。
/// content 用 Vec<BodyElement> 而非 Vec<Paragraph>，因为 OOXML 规范及 LibreOffice
/// 官方测试（如 tdf141172：endnote 内嵌 w:tbl）证实脚注/尾注内容不限于纯段落，
/// 未来若样本覆盖到表格/图片场景，无需再改数据结构，只需扩展渲染分支。
/// 当前渲染层实现范围：仅处理 BodyElement::Paragraph 分支，其余分支暂不渲染。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Note {
    /// 对应 docx 里的 w:id，用于跟 Run.footnote_ref / Run.endnote_ref 关联
    pub id: u32,
    pub content: Vec<BodyElement>,
    /// note 的类型（Step 7新增）：区分"真实脚注/尾注内容"与"分隔线标记"。
    /// 缺省即 Normal（docx里绝大多数 w:endnote/w:footnote 都不带 w:type）。
    #[serde(default)]
    pub note_type: NoteType,
}

/// 批注（comment）内容。参照 Note（脚注/尾注）的结构模式：id 用于跟
/// Run.comment_range_start/comment_range_end/comment_ref 关联。
/// content 复用 BodyElement，与 Note.content 同构（docx里 w:comment 内部
/// 也是若干 w:p 段落，结构与 w:footnote/w:endnote 高度相似）。
/// 第一版范围（对齐 CommentDone.docx 用例）：author/date/initials/content/done。
/// parent_id 是回复链（reply chain）占位字段，本次不实现完整回复链逻辑，
/// 详见交接文档 ROE_Progress_v147 2.5节的设计取舍说明。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Comment {
    /// 对应 docx 里 w:comment 的 w:id
    pub id: u32,
    /// 对应 w:comment 的 w:author
    pub author: String,
    /// 对应 w:comment 的 w:date，原始字符串存储，暂不解析成结构化时间
    #[serde(default)]
    pub date: Option<String>,
    /// 对应 w:comment 的 w:initials
    #[serde(default)]
    pub initials: Option<String>,
    /// 批注正文内容
    pub content: Vec<BodyElement>,
    /// 完成状态，来自 commentsExtended.xml 的 w15:commentEx w15:done。
    /// 语义挂在"整条批注"上（对应LOK CommentProperties.hxx: bDone与
    /// sParaIdParent均"linked to the last paragraph of a comment"），
    /// 不按段落级别细分。
    #[serde(default)]
    pub done: bool,
    /// 回复链父批注ID占位字段（对应 commentsExtended.xml 的 w15:paraIdParent），
    /// 本次不实现完整回复链逻辑，仅预留字段。
    #[serde(default)]
    pub parent_id: Option<u32>,
}

/// 一条批注讨论串：一条原始批注 + 按顺序排列的回复。
/// "回复"定义为 parent_id 指向该原始批注 id 的其他 Comment。
/// 当前批注结构只有一层（只能回复原批注，不能再回复回复），
/// 所以 replies 内部不会再嵌套 CommentThread。
///
/// 用 &'a Comment 借用而非拷贝：Comment.content 可能较大
/// （Vec<BodyElement>），讨论串只是一种"读视图"，不需要拥有数据。
/// 这也意味着本结构体不能跨越C-ABI/Slint等外部语言边界直接传递
/// （生命周期'a无法导出）；未来做C-ABI坐标接口时，应另外提供一个
/// 只含id的平坦结构（例如 CommentThreadIdGroup），不要尝试把这个
/// 结构体本身暴露出去。
#[derive(Debug, Clone)]
pub struct CommentThread<'a> {
    pub root: &'a Comment,
    pub replies: Vec<&'a Comment>,
}

/// 把一条 Comment 的富文本内容（Vec<BodyElement>）拍平成纯文本。
///
/// C-ABI/Slint 批注面板第一阶段（ROE_Progress_v157.md 第三节"忠实转发"
/// 原则的第一步实现）：本次只需要纯文本跑通整套查询链路，BodyElement
/// 的样式信息（加粗/背景色等）本次不处理，下次设计富文本中间表示时
/// 再扩展。这不是"简化成纯文本就是最终方案"的决定，只是分阶段落地。
///
/// 规则：
/// - BodyElement::Paragraph 取其 runs，拼接每个 Run.text；
/// - 多个 Paragraph 之间用 "\n" 连接（对应批注里的换行/多段落场景）；
/// - Table/Image/Section/Frame/Shape/InlineChart 等其他变体直接跳过——
///   批注正文实践中几乎不会包含这些，跳过而非 panic 是保守的容错策略。
pub fn comment_plain_text(comment: &Comment) -> String {
    comment
        .content
        .iter()
        .filter_map(|elem| match elem {
            BodyElement::Paragraph(p) => {
                let text: String = p.runs.iter().map(|r| r.text.as_str()).collect();
                Some(text)
            }
            _ => None,
        })
        .collect::<Vec<String>>()
        .join("\n")
}

impl WriterDocument {
    /// 按 parent_id 把所有批注归组成讨论串，用于渲染回复区、以及任何
    /// 需要"这条批注有没有回复、有几条"的查询场景（包括脚本/Agent对
    /// 内存模型的直接查询——ROE的AI原生设计要求批注/回复数据能被完整
    /// 读取，不只是给内部渲染代码用）。
    ///
    /// 分组规则：
    /// - parent_id 为 None 的 Comment 是一条讨论串的 root。
    /// - parent_id 为 Some(pid) 且 pid 确实指向 self.comments 里某条
    ///   Comment 的 id，归入该 Comment 对应讨论串的 replies。
    /// - parent_id 指向一个不存在于 self.comments 的 id（数据异常/
    ///   docx损坏等情况）：降级为独立讨论串（root是它自己，replies为
    ///   空），不静默丢弃——保证任何一条Comment都会出现在某个
    ///   CommentThread里，不会在这个视图里"凭空消失"。
    ///
    /// 排序：
    /// - 返回的 Vec<CommentThread> 顺序 = root 在 self.comments 中的
    ///   原始出现顺序。
    /// - 每条串内部的 replies，优先按 Comment.date 排序（日期是docx
    ///   w:date 属性，标准格式为 "YYYY-MM-DDTHH:MM:SSZ"，这个格式定长
    ///   且字典序等价于时间序，直接用字符串比较排序即可，不需要引入
    ///   时间解析库）。date 缺失，或者格式不像标准ISO8601（长度不为
    ///   20、不以'Z'结尾、第11个字符不是'T'）的 reply，一律 fallback
    ///   为"保持在 self.comments 中的原始相对顺序"——用 stable sort
    ///   实现：先给每条 reply 打上"能否参与日期排序"的标记，能参与的
    ///   按日期字符串比较，不能参与的所有比较都视为"相等"，
    ///   sort_by的稳定性会让这些"相等"元素保持原始相对顺序不变。
    pub fn comment_threads(&self) -> Vec<CommentThread<'_>> {
        fn looks_like_iso8601(s: &str) -> bool {
            s.len() == 20 && s.ends_with('Z') && s.as_bytes().get(10) == Some(&b'T')
        }

        let mut roots: Vec<CommentThread<'_>> = Vec::new();
        // id -> roots下标，用于O(1)定位某条parent_id该挂到哪个thread
        let mut id_to_root_idx: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();

        // 第一趟：先把所有"parent_id为None"的Comment建成root。
        for c in &self.comments {
            if c.parent_id.is_none() {
                id_to_root_idx.insert(c.id, roots.len());
                roots.push(CommentThread { root: c, replies: Vec::new() });
            }
        }

        // 第二趟：处理所有"parent_id为Some"的Comment，挂到对应root下；
        // 若parent_id指向的id不在id_to_root_idx里（孤儿回复），降级为
        // 独立thread（root是它自己）。
        for c in &self.comments {
            if let Some(pid) = c.parent_id {
                if let Some(&root_idx) = id_to_root_idx.get(&pid) {
                    roots[root_idx].replies.push(c);
                    continue;
                }
            }
            if c.parent_id.is_some() && !id_to_root_idx.contains_key(&c.id) {
                // 孤儿回复：它引用的parent不存在，且它自己尚未作为root
                // 出现过（第一趟只收了parent_id=None的，这条parent_id
                // 是Some但目标缺失，属于既不在第一趟、也没能在上面挂
                // 成功的情况）——降级为独立thread。
                id_to_root_idx.insert(c.id, roots.len());
                roots.push(CommentThread { root: c, replies: Vec::new() });
            }
        }

        // 每条thread内部对replies排序：能解析出合规日期的按日期字符串
        // 排序，不合规的保持原始相对顺序（stable sort + "不合规视为相等"）。
        for thread in &mut roots {
            thread.replies.sort_by(|a, b| {
                match (&a.date, &b.date) {
                    (Some(da), Some(db)) if looks_like_iso8601(da) && looks_like_iso8601(db) => {
                        da.cmp(db)
                    }
                    _ => std::cmp::Ordering::Equal, // 不参与排序，stable sort保持原顺序
                }
            });
        }

        roots
    }
}

/// 脚注/尾注的类型（对应 OOXML w:endnote/w:footnote 的 w:type 属性）。
/// Separator/ContinuationSeparator 这两种不是真实脚注内容，而是文档定义的
/// "分隔线"标记本身（渲染时应转换成画一条横线，而不是当文字段落排版）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum NoteType {
    #[default]
    Normal,
    Separator,
    ContinuationSeparator,
}

/// 行内数学公式（来源 OOXML m:oMath）
///
/// width_pt/height_pt/depth_pt 来自 RaTeX 排版引擎的真实计算结果
/// （见 roe-formula::latex_layout_size），不是字符串估算值。
/// depth 是排版学概念：基线以下的部分（如分数线以下的分母），
/// 渲染时用于正确对齐同一行内公式与普通文字的基线。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Formula {
    pub latex: String,
    pub width_pt: f32,
    pub height_pt: f32,
    pub depth_pt: f32,
    /// DOCX-8-v2(方案乙): 解析阶段(docx.rs)已经算好的绘制指令列表，
    /// 单位是em（相对公式自身原点），tile.rs绘制时按当次实际字号
    /// 换算成pt/像素后直接遍历这些指令画图，不需要重新parse LaTeX。
    /// 选这个方案是为了避免render_viewport每次重绘（滚动/缩放都会
    /// 触发）都重新跑一遍ratex_parser::parse，那样是隐藏的性能债。
    /// 代价：同一公式在文档里重复出现时会重复存储，可接受（公式数量
    /// 通常远小于会导致体积问题的规模）。
    pub display_list: ratex_types::DisplayList,
    /// DOCX-8-v3续: m:borderBox的strikeV（垂直划线）预留字段。
    /// RaTeX无原生垂直线命令（\cancel/\bcancel都是对角斜线），
    /// 不降级用错误方向的命令冒充（会误导：明明是竖线画成斜线），
    /// 真正的绘制逻辑推迟到方向B（渲染管线tile.rs实现时）：
    /// 届时在此公式包围盒中线位置，复用tile.rs已有的draw_v_line
    /// （同\frac分数线用的那个函数）直接叠加一条真正的垂直线，
    /// 参照OnlyOffice sdkjs borderBox.js的drawVerLine做法。
    /// 在此之前，这个字段只做记录，不产出任何视觉效果。
    pub needs_vertical_strike: bool,
}

/// 上标/下标位置
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ScriptPosition {
    #[default]
    Normal,
    Superscript,
    Subscript,
}

/// 下划线样式（本轮范围：单线/双线/波浪线/无 四选一，其余 UNO 枚举值如点线/
/// 虚线不做，如后续需要可参照批注渲染里已有的点线画法）。
/// 与 Run.underline: Option<bool> 是两个独立字段：underline 管"有没有下划线"
/// 这个开关，underline_style 管"画哪种线"。underline_style 为 None 且
/// underline 为 true 时，渲染按 Single（单线）处理，保证旧文档/旧脚本行为不变。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum UnderlineStyle {
    #[default]
    Single,
    Double,
    Wave,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListInfo {
    pub list_id:    String,
    pub level:      u8,
    pub style_name: String,
}

/// text:list-style，按 level（0-based）索引各级样式。
/// ODF 是 1-based（text:level="1".."10"），parser 负责转换成 0-based。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListStyle {
    pub name:   String,
    pub levels: Vec<ListLevelStyle>,
}

impl ListStyle {
    pub fn level(&self, level: u8) -> Option<&ListLevelStyle> {
        self.levels.get(level as usize)
    }
}

/// 单一 level 的列表样式：编号 / 项目符号 二选一。
/// image bullet（text:list-level-style-image）暂不支持，留作后续扩展。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ListLevelStyle {
    Number(NumberListLevel),
    Bullet(BulletListLevel),
}

impl Default for ListLevelStyle {
    fn default() -> Self {
        ListLevelStyle::Bullet(BulletListLevel::default())
    }
}

/// text:list-level-style-number
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NumberListLevel {
    pub format:          NumFormat,
    /// num:prefix（如 "("）
    pub prefix:          String,
    /// num:suffix（如 "." 或 ")"）
    pub suffix:          String,
    /// text:start-value，起始编号，默认 1
    pub start_value:     u32,
    /// text:display-levels，联合显示几级（如 "1.1.1" 为 3）
    pub display_levels:  u8,
    /// 缩进相关（来自 style:list-level-properties / style:list-level-label-alignment）
    pub indent:          ListIndent,
}

impl Default for NumberListLevel {
    fn default() -> Self {
        Self {
            format: NumFormat::Arabic,
            prefix: String::new(),
            suffix: String::from("."),
            start_value: 1,
            display_levels: 1,
            indent: ListIndent::default(),
        }
    }
}

/// text:list-level-style-bullet
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulletListLevel {
    /// text:bullet-char（如 "•"）
    pub bullet_char: String,
    pub prefix:      String,
    pub suffix:      String,
    pub indent:      ListIndent,
}

impl Default for BulletListLevel {
    fn default() -> Self {
        Self {
            bullet_char: String::from("•"),
            prefix: String::new(),
            suffix: String::new(),
            indent: ListIndent::default(),
        }
    }
}

/// num:format 取值映射（ODF 标准：1/a/A/i/I/空）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum NumFormat {
    /// "1" → 阿拉伯数字（docx numFmt 缺省值）
    #[default]
    Arabic,
    /// "a" → 小写字母
    LowerAlpha,
    /// "A" → 大写字母
    UpperAlpha,
    /// "i" → 小写罗马数字
    LowerRoman,
    /// "I" → 大写罗马数字
    UpperRoman,
    /// 空（无编号，仅 prefix/suffix，常见于纯符号场景）
    None,
}

/// 缩进/对齐，来自 style:list-level-properties 与
/// style:list-level-label-alignment 两层属性的合并结果（pt 单位）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ListIndent {
    /// fo:margin-left，本级文字整体左缩进
    pub margin_left:  f32,
    /// fo:text-indent，标签（bullet/编号）相对 margin_left 的偏移（通常为负）
    pub text_indent:  f32,
    /// text:min-label-width，标签到正文之间的最小距离
    pub min_label_width: f32,
}

impl Default for ListIndent {
    fn default() -> Self {
        // 经验默认值：每级缩进 18pt（约 0.25in），标签宽度 18pt，
        // 与常见 Word/LO 默认列表样式量级一致。
        Self {
            margin_left: 18.0,
            text_indent: -18.0,
            min_label_width: 18.0,
        }
    }
}


#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InlineImage {
    /// 图片数据
    pub image: ImageData,
    /// 宽度（pt）
    pub width_pt: f32,
    /// 高度（pt）
    pub height_pt: f32,
    /// WROOT-097: wp:inline 自身的 distT/distB/distL/distR 环绕间距属性
    /// （EMU/12700 换算为 pt）。此前完全未解析、硬编码为0，导致内嵌图片
    /// 与上下文字之间没有 Word 规定的最小留白，渲染时图片贴着/压着相邻
    /// 段落文字。目前排版层（layout.rs）只用 dist_t_pt/dist_b_pt 转换为
    /// space_before/space_after；dist_l_pt/dist_r_pt 暂未使用（内嵌图片
    /// 目前没有左右环绕概念），保留字段是为了与 FloatFrame/Shape 的
    /// dist_*_pt 四字段模型保持一致，避免未来需要时再次改模型层。
    /// #[serde(default)]：旧版序列化数据没有这些字段，反序列化时按0处理，
    /// 与此前"无间距"的既有行为一致，不会导致旧数据加载失败。
    #[serde(default)]
    pub dist_t_pt: f32,
    #[serde(default)]
    pub dist_b_pt: f32,
    #[serde(default)]
    pub dist_l_pt: f32,
    #[serde(default)]
    pub dist_r_pt: f32,
    /// v308: 图片旋转角（度，顺时针）
    #[serde(default)]
    pub rotation: Option<f32>,
}

/// 浮动图片框（文字绕排）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FloatFrame {
    /// WROOT-ANCHOR-04: 图片在原始文档里所属的段落，在 body 数组中
    /// "即将占据"的索引（图片剥离出FloatFrame时，该段落尚未push进body，
    /// 此值等于剥离那一刻的 body.len()）。用于 relativeFrom="paragraph"/
    /// "character" 时精确定位锚点段落，取代此前"body里紧邻Frame之后的
    /// 第一个Paragraph"这条方向性猜错的启发式规则——真实文档常见图片
    /// 是某段落内第二个run（紧跟文字run之后），此时锚点应该是"图片所属
    /// 的这个段落"（向前找），而不是"文档流里下一个段落"（向后找）。
    /// None 表示解析时不在任何段落内部（理论上罕见，兜底回退旧逻辑）。
    #[serde(default)]
    pub origin_para_body_index: Option<usize>,
    /// 图片数据
    pub image: ImageData,
    /// 宽度（pt）
    pub width_pt:  f32,
    /// 高度（pt）
    pub height_pt: f32,
    /// 页面内容区 x 偏移（pt），从左边距内侧算起。仅当来源为 posOffset 时有意义，
    /// 若为 align 相对对齐则此值无意义，请优先看 align_h
    pub x_pt: f32,
    /// 页面内容区 y 偏移（pt），从上边距内侧算起。语义同 x_pt，优先看 align_v
    pub y_pt: f32,
    /// 水平方向锚定定位基准：对应 OOXML wp:positionH 的 relativeFrom 属性
    /// 原始值，如 "page"/"margin"/"column"/"character" 等；ODF 来源留空。
    /// H/V 在 OOXML 里允许不同（真实文档常见，如 H="column" V="paragraph"），
    /// 此前误合并成单一字段、优先取H，导致V的信息丢失，故拆分为两个独立
    /// 字段，各自反映各自方向真实的定位基准，不做任何合并/取舍。
    pub anchor_type_h: Option<String>,
    /// 垂直方向锚定定位基准：对应 OOXML wp:positionV 的 relativeFrom 属性
    /// 原始值，如 "page"/"margin"/"paragraph"/"line" 等；ODF 来源留空。
    pub anchor_type_v: Option<String>,
    /// 水平相对对齐关键字（如 "center"/"left"/"right"/"inside"/"outside"），
    /// 与 x_pt 二选一语义：有值时应优先于 x_pt 使用
    pub align_h: Option<String>,
    /// 垂直相对对齐关键字（如 "top"/"bottom"/"center"/"inside"/"outside"），
    /// 与 y_pt 二选一语义：有值时应优先于 y_pt 使用
    pub align_v: Option<String>,
    /// 文字环绕方式：对应 OOXML wp:wrap* 系列标签，原样存标签本地名
    /// （"none"/"square"/"tight"/"through"/"topAndBottom"），未识别或缺失时为 None。
    /// 渲染层自行决定支持哪几档，未支持的档位建议降级为 square 或 none 处理。
    pub wrap_type: Option<String>,
    /// 环绕间距-上（pt）：对应 OOXML wp:anchor 的 distT 属性，EMU/12700 换算。
    /// 缺省（未解析到）时为 0.0，语义上等同 OOXML 规范里的默认值。
    pub dist_t_pt: f32,
    /// 环绕间距-下（pt）：对应 distB 属性，语义同上。
    pub dist_b_pt: f32,
    /// 环绕间距-左（pt）：对应 distL 属性，语义同上。
    pub dist_l_pt: f32,
    /// 环绕间距-右（pt）：对应 distR 属性，语义同上。
    pub dist_r_pt: f32,
    /// WROOT-016: 图片自身旋转角度（度，顺时针），对应 OOXML
    /// <pic:spPr><a:xfrm rot="..."> 属性（原始单位 1/60000 度，解析层
    /// 换算为普通角度制，与 roe-model::common::Transform.rotation 同一
    /// 单位约定）。None 表示未设置该属性（等同不旋转），docx 侧此前完全
    /// 未解析图片的 xfrm rot（仅 wps:wsp 形状路径解析过，图片路径是
    /// 独立的、此前遗漏的解析空白），本次新增。
    #[serde(default)]
    pub rotation: Option<f32>,
}

/// Writer 独立形状（自选图形/文本框），对应 OOXML w:pict(legacy VML) / wps:wsp(DrawingML)。
/// 本次(DOCX-9)仅覆盖 wps:wsp；w:pict 识别但不解析(降级跳过，参照DOCX-7 OLE降级思路)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Shape {
    pub geometry: crate::common::ShapeGeometry,
    pub style: crate::common::ShapeStyle,
    /// 形状内文字（文本框场景），无文字时为空 Vec
    pub text_content: Vec<Paragraph>,
    /// 宽度（pt）
    pub width_pt: f32,
    /// 高度（pt）
    pub height_pt: f32,
    // --- 以下锚定/环绕字段与 FloatFrame 同名同语义，直接复用而非另起类型 ---
    pub x_pt: f32,
    pub y_pt: f32,
    pub anchor_type_h: Option<String>,
    pub anchor_type_v: Option<String>,
    pub align_h: Option<String>,
    pub align_v: Option<String>,
    pub wrap_type: Option<String>,
    /// 环绕间距-上（pt）：对应 OOXML wp:anchor 的 distT 属性，EMU/12700 换算。
    /// 缺省（未解析到）时为 0.0，语义上等同 OOXML 规范里的默认值。
    pub dist_t_pt: f32,
    /// 环绕间距-下（pt）：对应 distB 属性，语义同上。
    pub dist_b_pt: f32,
    /// 环绕间距-左（pt）：对应 distL 属性，语义同上。
    pub dist_l_pt: f32,
    /// 环绕间距-右（pt）：对应 distR 属性，语义同上。
    pub dist_r_pt: f32,
    /// OOXML <a:xfrm flipV="1">：形状是否需要沿自身竖直中轴做上下镜像。
    /// None 表示未解析到该属性（等同 false，不翻转）；docx侧此前完全
    /// 未解析该属性（WROOT-FLIPV-01，2026-08-17新增），现补上。
    pub flip_v: Option<bool>,
    /// v308: 形状旋转角（度，顺时针），None 表示不旋转
    #[serde(default)]
    pub rotation: Option<f32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Section {
    pub name:    String,
    pub content: Vec<BodyElement>,
    // WROOT任务B第一步（模型层）：此前 Section 只有 name+content，
    // 完全没有承载"这一节自己的排版属性"（页面尺寸/边距/页眉页脚/
    // 首页独立设置/奇偶页不同/分栏）。docx 的 w:sectPr 本来就是"每节
    // 各自独立"的属性集合，此前 WriterDocument 顶层把这些属性做成了
    // "全文档单一一份"，导致多节文档解析时后一节覆盖前一节（同一处
    // 根因见 read_header_footer_refs 与 read_page_layout）。
    // 以下字段新增，用于承载"这一节自己的" sectPr 属性；WriterDocument
    // 顶层同名字段保留不删，作为"单节文档"或"未显式分节"场景的默认值，
    // 不破坏任何现有只读顶层字段的代码路径。
    /// 本节页面尺寸（宽, 高），单位 pt。None 表示沿用 WriterDocument 顶层值。
    #[serde(default)] pub page_size: Option<(f32, f32)>,
    #[serde(default)] pub margin_top:    Option<f32>,
    #[serde(default)] pub margin_bottom: Option<f32>,
    #[serde(default)] pub margin_left:   Option<f32>,
    #[serde(default)] pub margin_right:  Option<f32>,
    /// 本节页眉内容。None 表示沿用 WriterDocument 顶层值。
    #[serde(default)] pub header: Option<HeaderFooterSet>,
    /// 本节页脚内容。语义同 header。
    #[serde(default)] pub footer: Option<HeaderFooterSet>,
    /// 对应本节 sectPr 的 w:titlePg。
    #[serde(default)] pub title_page: Option<bool>,
    /// 对应本节 settings.xml 的 w:evenAndOddHeaders 在本节的取值。
    /// 注：evenAndOddHeaders 实际是 docx 全局开关（存于 settings.xml，
    /// 不是逐 sectPr 的），这里仍保留 per-section 字段位，供未来若发现
    /// 反例（某些生成器把它当节级属性处理）时使用；正常情况下应统一
    /// 读 WriterDocument 顶层的 even_and_odd_headers。
    #[serde(default)] pub even_and_odd_headers: Option<bool>,
    /// 对应本节 sectPr 的 w:type（<w:type w:val="evenPage|oddPage|nextPage|continuous"/>）。
    /// [WROOT-093] Word 用它强制声明"本节必须从偶数页/奇数页开始"，这个值
    /// 应该覆盖"节内页码奇偶"的自然判断，而不是靠 page_in_section % 2 推算——
    /// 当一个 section 恰好只含1页时，两者会给出矛盾结果（本节永远是"节内第1页"
    /// 即奇数，但 type 可能显式声明它是 evenPage）。None 表示未显式声明（默认
    /// nextPage 语义或本来就是 continuous），此时才退回 page_in_section 奇偶判断。
    #[serde(default)] pub section_type: Option<String>,
    /// 本节分栏设置（对应本节 sectPr 的 w:cols）。None 表示沿用顶层值。
    #[serde(default)] pub columns: Option<DocumentColumns>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DocumentMeta {
    pub title:    Option<String>,
    pub author:   Option<String>,
    pub created:  Option<String>,
    pub modified: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_default_has_no_formula() {
        let run = Run::default();
        assert!(run.formula.is_none());
    }

    #[test]
    fn run_can_carry_formula() {
        let run = Run {
            text: String::new(),
            formula: Some(Formula {
                latex: "\\frac{1}{2}".to_string(),
                width_pt: 12.5,
                height_pt: 8.0,
                depth_pt: 2.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let f = run.formula.expect("formula should be present");
        assert_eq!(f.latex, "\\frac{1}{2}");
        assert_eq!(f.width_pt, 12.5);
        assert_eq!(f.height_pt, 8.0);
        assert_eq!(f.depth_pt, 2.0);
    }

    #[test]
    fn formula_default_is_zeroed() {
        let f = Formula::default();
        assert_eq!(f.latex, "");
        assert_eq!(f.width_pt, 0.0);
        assert_eq!(f.height_pt, 0.0);
        assert_eq!(f.depth_pt, 0.0);
    }

    #[test]
    fn run_with_formula_clones_correctly() {
        // Clone 是模型层实际会用到的操作（构造/传递 WriterDocument 时），
        // 序列化格式(json/bincode等)正确性留给消费方 crate 测，
        // roe-model 本身不依赖具体格式，不为测试引入 serde_json。
        let run = Run {
            text: String::new(),
            formula: Some(Formula {
                latex: "x^2".to_string(),
                width_pt: 5.0,
                height_pt: 6.0,
                depth_pt: 0.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let cloned = run.clone();
        assert_eq!(cloned.formula.unwrap().latex, "x^2");
    }

    // ---- comment_threads() 相关测试 ----
    // Comment 没有 derive(Default)，用这个helper减少重复字段书写，
    // 测试只关心 id/date/parent_id 这三个跟分组/排序逻辑相关的字段，
    // 其余字段（author/initials/content/done）填最简单的占位值。
    fn make_comment(id: u32, date: Option<&str>, parent_id: Option<u32>) -> Comment {
        Comment {
            id,
            author: "tester".to_string(),
            date: date.map(|s| s.to_string()),
            initials: None,
            content: Vec::new(),
            done: false,
            parent_id,
        }
    }

    fn doc_with_comments(comments: Vec<Comment>) -> WriterDocument {
        WriterDocument { comments, ..Default::default() }
    }

    #[test]
    fn comment_threads_groups_one_to_one_reply() {
        // id=1是root，id=0的parent_id指向1，应归入1的replies
        let doc = doc_with_comments(vec![
            make_comment(1, None, None),
            make_comment(0, None, Some(1)),
        ]);
        let threads = doc.comment_threads();
        assert_eq!(threads.len(), 1, "只有一条root，应该只有一个thread");
        assert_eq!(threads[0].root.id, 1);
        assert_eq!(threads[0].replies.len(), 1);
        assert_eq!(threads[0].replies[0].id, 0);
    }

    #[test]
    fn comment_threads_groups_one_to_many_replies() {
        // id=1是root，id=2和id=3都回复给1（一对多，不是链式）
        let doc = doc_with_comments(vec![
            make_comment(1, None, None),
            make_comment(2, None, Some(1)),
            make_comment(3, None, Some(1)),
        ]);
        let threads = doc.comment_threads();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].root.id, 1);
        assert_eq!(threads[0].replies.len(), 2);
        let reply_ids: Vec<u32> = threads[0].replies.iter().map(|c| c.id).collect();
        assert!(reply_ids.contains(&2));
        assert!(reply_ids.contains(&3));
    }

    #[test]
    fn comment_threads_keeps_root_order_matching_comments_order() {
        // 多条独立root（都没有回复），thread顺序应该跟comments里出现顺序一致
        let doc = doc_with_comments(vec![
            make_comment(5, None, None),
            make_comment(2, None, None),
            make_comment(9, None, None),
        ]);
        let threads = doc.comment_threads();
        assert_eq!(threads.len(), 3);
        assert_eq!(threads[0].root.id, 5);
        assert_eq!(threads[1].root.id, 2);
        assert_eq!(threads[2].root.id, 9);
    }

    #[test]
    fn comment_threads_demotes_orphan_reply_to_independent_thread() {
        // id=0的parent_id=Some(999)，但999在comments里根本不存在（孤儿回复），
        // 应该降级为独立thread，而不是被静默丢弃。
        let doc = doc_with_comments(vec![
            make_comment(1, None, None),
            make_comment(0, None, Some(999)),
        ]);
        let threads = doc.comment_threads();
        assert_eq!(threads.len(), 2, "孤儿回复应该独立成一个thread，不能凭空消失");
        let ids: Vec<u32> = threads.iter().map(|t| t.root.id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&0));
        // 孤儿thread自己没有replies
        let orphan_thread = threads.iter().find(|t| t.root.id == 0).unwrap();
        assert!(orphan_thread.replies.is_empty());
    }

    #[test]
    fn comment_threads_sorts_replies_by_date_when_both_valid() {
        // replies乱序插入，日期格式都合规，应该按日期升序重排
        let doc = doc_with_comments(vec![
            make_comment(1, None, None),
            make_comment(3, Some("2024-06-01T10:00:00Z"), Some(1)),
            make_comment(2, Some("2024-01-01T10:00:00Z"), Some(1)),
        ]);
        let threads = doc.comment_threads();
        assert_eq!(threads[0].replies.len(), 2);
        assert_eq!(threads[0].replies[0].id, 2, "更早日期(2024-01)应该排前面");
        assert_eq!(threads[0].replies[1].id, 3, "更晚日期(2024-06)应该排后面");
    }

    #[test]
    fn comment_threads_falls_back_to_original_order_when_date_missing_or_invalid() {
        // reply id=3 日期缺失(None)，reply id=2 日期格式不合规("not-a-date")，
        // 两者都不能参与日期排序，应保持在comments里的原始相对顺序（3在前，2在后）
        let doc = doc_with_comments(vec![
            make_comment(1, None, None),
            make_comment(3, None, Some(1)),
            make_comment(2, Some("not-a-date"), Some(1)),
        ]);
        let threads = doc.comment_threads();
        assert_eq!(threads[0].replies.len(), 2);
        assert_eq!(threads[0].replies[0].id, 3, "日期缺失/不合规时应保持原始顺序");
        assert_eq!(threads[0].replies[1].id, 2);
    }

    #[test]
    fn comment_threads_mixed_valid_and_invalid_dates_stable_sort() {
        // 混合场景：id=2有合规日期，id=3日期缺失。
        // 按当前实现（不合规视为Equal参与stable sort），
        // 日期缺失的元素不会被强制排到某一端，只是不参与比较、
        // 保持它在插入序列中相对于"同样不参与比较的元素"的顺序。
        // 这里只验证不panic、两条都还在，且合规日期元素本身没有错乱。
        let doc = doc_with_comments(vec![
            make_comment(1, None, None),
            make_comment(2, Some("2024-01-01T10:00:00Z"), Some(1)),
            make_comment(3, None, Some(1)),
        ]);
        let threads = doc.comment_threads();
        assert_eq!(threads[0].replies.len(), 2);
        let ids: Vec<u32> = threads[0].replies.iter().map(|c| c.id).collect();
        assert!(ids.contains(&2));
        assert!(ids.contains(&3));
    }
}
