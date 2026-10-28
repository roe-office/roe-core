//! ParagraphFrame + Follow Chain
//! Arena 使用 slotmap，FrameId/PageId 在 roe-core::arena 中定义

use slotmap::SlotMap;
use roe_core::arena::{FrameId, PageId};

/// 页面四边距，独立记录 top/bottom/left/right，不假设四边相等。
/// 修复背景：docx 真实边距经常不对称（例如 top/bottom=2cm 但
/// left/right=2.5cm），此前用单一 f32 表示会丢失信息。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Margins {
    pub top:    f32,
    pub bottom: f32,
    pub left:   f32,
    pub right:  f32,
}

impl Margins {
    /// 兼容旧代码/测试：四边相等的场景一行构造。
    pub fn uniform(v: f32) -> Self {
        Self { top: v, bottom: v, left: v, right: v }
    }
}

impl Default for Margins {
    fn default() -> Self {
        Self::uniform(56.7)
    }
}



/// 一行内某个 run 片段在本行文字中的字节范围 + 该片段的已解析样式。
/// D2 新增：为 D3（多样式 shaping）/D4（line_shape 精确定位改造）准备数据结构，
/// 本块只新增字段，不改动任何现有排版/渲染逻辑。
/// 字段含义对齐 D1 的 crate::layout::ResolvedRunStyle（per-run 逐层样式解析结果）。
#[derive(Debug, Clone, Copy)]
pub struct LineRunSpan {
    /// 本行文字内的字节偏移起点（与 LineBox.text 的切片口径一致）
    pub start: usize,
    /// 本行文字内的字节偏移终点（不含）
    pub end:   usize,
    pub style: crate::layout::ResolvedRunStyle,
    /// [WROOT-footnote-budget] 本片段对应原始 para.runs 的下标，用于
    /// 反查 Run.footnote_ref（方案D逐行预算扣减需要知道"这一行是否
    /// 引入了新的footnote引用"）。None 表示这个 span 不对应正文里的
    /// 某个具体 run —— 目前只有 footnote/endnote 内容自身的编号标签行
    /// （note_label 那两处构造点）落在这个情况，它们是渲染器合成的
    /// 文字，不是对某个 para.runs 元素的引用。
    pub run_idx: Option<usize>,
    /// WROOT-080修复：公式run（style.is_formula=true）改用Parley
    /// InlineBox渲染，此字段存Parley align()后算好的最终盒子坐标
    /// （x/y相对本行所在frame的顶部/margin原点，pt单位，未乘scale）。
    /// 非公式run恒为None。有值时tile.rs应直接用这个坐标画公式，
    /// 不再通过GlyphRun反查+手动叠加align_off_px（那条路径已被
    /// WROOT-080证实在纯空格占位场景下计算错误）。
    pub formula_box: Option<FormulaBoxPosition>,
}

/// WROOT-080修复新增：Parley PositionedInlineBox的精简搬运，只留
/// tile.rs画公式需要的字段，避免frame.rs直接依赖parley类型。
#[derive(Debug, Clone, Copy, Default)]
pub struct FormulaBoxPosition {
    /// 盒子左上角x坐标，相对本行所在frame内容区左边界（pt）
    pub x: f32,
    /// 盒子左上角y坐标，相对本行顶部（pt）
    pub y: f32,
    /// 盒子基线，相对盒子顶部（pt）。None时使用height（盒子底边即基线）。
    pub baseline: Option<f32>,
}

/// WROOT-100（表格边框BorderStyle重构）：单条边框线的完整渲染信息，
/// 取代此前 border_top/bottom: bool + border_left/right: Option<f32>
/// 这种"只知道要不要画、颜色宽度另外全局共用一份"的粗糙设计。四条边
/// 各自独立 Option<BorderStyle>，None 表示这条边不画，与 roe-impress
/// draw_rect_stroke_sides 的分边设计保持同一语言（该函数注释：
/// "某些内置样式故意让 inside_v 保持 None 表示'不画竖线'，这个语义
/// 必须在渲染层真正体现出来，不能像旧实现那样四边套同一色"）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BorderStyle {
    pub color: roe_model::common::Color,
    /// 线宽，单位pt（与cell.border_width等既有字段单位一致，渲染时
    /// 由tile.rs乘scale换算成像素，tile.rs内部不做任何单位换算）。
    pub width: f32,
    /// true表示画灰色虚线兜底（0xA6A6A6，dash[4,3]，与roe-impress
    /// draw_placeholder_dashed_rect占位符虚线框视觉语言一致），
    /// 用于insideH/insideV未指定具体颜色宽度时的默认内部网格线；
    /// false表示按color/width画实线。
    pub dashed: bool,
}

/// WROOT-068: 一段 tab 引导线的绘制信息（x 区间 + 字符类型）。
/// 一行内可能有多个 tab stop，因此 LineBox 用 Vec 装多段。区间坐标
/// 与 LineBox.text_x_offset 同一坐标系（相对页面内容区左边界，pt）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TabLeaderSegment {
    /// 引导线起点 x（pt），即 tab 断点原本的位置
    pub x_start: f32,
    /// 引导线终点 x（pt），即调整后文字实际起笔位置（引导线画在两者之间）
    pub x_end: f32,
    pub leader: roe_model::writer::TabLeader,
}

/// 一行 shaped 文字（来自 cosmic-text layout_runs）
#[derive(Debug, Clone)]
#[derive(Default)]
pub struct LineBox {
    pub y_offset: f32,   // 相对于本 frame 顶部（pt）
    pub ascent:   f32,
    pub descent:  f32,
    pub line_height: f32,
    pub text:     String,  // 调试用
    /// D2 新增：本行由哪些 run 片段组成（每段字节范围 + 样式）。
    /// D2 阶段主循环仍是整段统一 Attrs（layout.rs 尚未在 D3 改造前填充真实
    /// 多段数据），此字段当前总是空 Vec 或单一覆盖全行的 span，D3 才会按
    /// 真实 run 边界切分填充。
    pub runs: Vec<LineRunSpan>,
    /// 表格 cell 用：本行在页面内容区的 x 偏移（pt），普通段落为 0.0
    pub x_offset: f32,
    /// 表格 cell 用：本行可用宽度（pt），0.0 表示使用页面内容宽度
    pub cell_width: f32,
    /// 表格 cell 用：文字实际绘制起点 x 坐标（pt），= x_offset + cell内边距。
    /// 与 x_offset 分开存放，避免 x_offset/cell_width 被内边距污染后
    /// 影响边框绘制坐标（WROOT-030 followup，边框应始终用未内缩的
    /// x_offset/cell_width，文字绘制才用这个字段）。普通段落路径下
    /// 等于 x_offset（无 cell 内边距概念）。
    pub text_x_offset: f32,
    /// 表格 cell 用：顶部边框线样式，None表示不画。WROOT-100重构：
    /// 此前是bool（只表达"要不要画"），颜色宽度全局共用一份
    /// border_color/width；现在每条边独立携带完整BorderStyle，
    /// 外框/内部线可以有不同颜色宽度虚实。
    pub border_top: Option<BorderStyle>,
    /// 表格 cell 用：底部边框线样式，None表示不画。
    pub border_bottom: Option<BorderStyle>,
    /// 表格 cell 用：左边框线样式，None表示不画。WROOT-100重构：此前
    /// 是Option<f32>（同时表达"要不要画"+"x坐标"），x坐标语义已剥离，
    /// 画线时改由tile.rs直接用x_offset现算，这里只保留样式信息。
    pub border_left: Option<BorderStyle>,
    /// 表格 cell 用：右边框线样式，None表示不画。
    pub border_right: Option<BorderStyle>,
    /// 列表 bullet/编号标签文本（仅段落首行 Some，其余行 None）
    pub list_label: Option<String>,
    /// label 起始 x 偏移（相对页面 margin，pt），仅当 list_label 为 Some 时有意义
    pub list_label_x: f32,
    /// WROOT-068: 本行需要绘制的 tab 引导线列表（dot/hyphen/underscore），
    /// tile.rs 在画完正文之后按此列表补画。空 Vec 表示本行没有需要画
    /// 引导线的 tab（要么没有 leader，要么行内根本没有 tab）。LineBox
    /// 本身不 derive Serialize/Deserialize（纯运行时渲染中间产物），
    /// 不需要 #[serde(skip)]。
    pub tab_leaders: Vec<TabLeaderSegment>,
    /// WROOT-068 渲染算法本体：本行内每个 tab 断点造成的"累积右移量"表，
    /// 按 byte_offset（行内相对字节偏移，与 LineBox.text 切片口径一致）
    /// 升序排列。画字时，某个 run 的起始字节偏移落在 [byte_offset_i,
    /// byte_offset_{i+1}) 区间内，就应该整体右移 cumulative_extra_i（pt，
    /// 未乘scale）。空 Vec 表示本行没有需要调整的 tab（不影响任何现有
    /// 渲染路径，向后兼容默认值）。
    pub tab_adjustments: Vec<TabAdjustment>,
    /// WROOT-027/028 (v141): note正文区（endnotes.xml/footnotes.xml）行首
    /// 编号占位符（如endnoteRef）自身通过w:rStyle引用的字符样式颜色。
    /// None表示该占位符没有引用样式（如WROOT-075），渲染时用现有默认
    /// 灰黑色兜底，不改变现有行为。[u8;4] = [r,g,b,a]，与tile.rs渲染
    /// 时直接使用的颜色数组格式保持一致，避免在tile.rs侧再做一次
    /// roe_model::common::Color -> [u8;4]转换。
    pub list_label_color: Option<[u8;4]>,
    /// 同上，编号占位符引用样式的字号相对于endnote正文默认字号的缩放
    /// 比例（不是绝对字号本身，因为list_label渲染路径的primary_font_size
    /// 是运行时算出来的，存缩放比例更方便tile.rs侧直接相乘）。
    /// 用Option<f32>而非裸f32：derive(Default)会给裸f32字段0.0默认值，
    /// 导致没有样式引用时字号被乘以0变得不可见，Option::None才能正确
    /// 表达"不缩放，用原字号"这个语义。
    pub list_label_font_scale: Option<f32>,
    /// 同上，编号占位符引用样式的vertAlign是否为superscript。bool（非
    /// Option）：没有rStyle引用时，"不是上标"本身就是明确、无歧义的
    /// 默认值，不需要Option区分"未设置"和"设置为false"。
    pub list_label_superscript: bool,
    /// v313：单元格行的来源（表格在body中的序号, 行, 列, 单元格内段落号）；正文行为None。
    /// 设计借鉴 WordCraft（Apache-2.0）的"每行带路径"思路。
    pub cell_ref: Option<(u32, u32, u32, u32)>,
}

/// WROOT-068：一行内某个 tab 断点开始，后续文字应整体右移的累积量。
/// 之所以是"累积"而非"单个tab独立右移量"：同一行可能有多个 tab（较少见，
/// 但不能排除，比如多列对齐的表格式段落），后一个 tab 的固定起点本身
/// 依赖前面所有 tab 撑大后的最终位置，所以从左到右处理时天然是累加关系，
/// 存储时直接存累加后的值，画字时不需要再反查前面的 adjustment 求和。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TabAdjustment {
    pub byte_offset: usize,
    pub cumulative_extra: f32,
}

/// WROOT-068：tab展开阶段（layout_impl内）记录的"占位tab在full_text里的
/// 绝对字节偏移"标记。只在para.tab_stops非空时产生（tab_stops为空的段落
/// 走原72pt栅格逻辑，不经过这条记录路径）。断行完成后，layout_impl按
/// byte_offset归属到具体某一行，结合run_idx/tab_seq查回para.tab_stops
/// 对应项，驱动LOK PostFormat阶段公式计算。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TabMarker {
    pub byte_offset: usize,
    pub run_idx: usize,
    /// 段落内第几个tab（从0计数，遇到的顺序），用于对应
    /// para.tab_stops[tab_seq.min(tab_stops.len()-1)]。
    pub tab_seq: usize,
}

/// 一个段落在某页上的帧
/// 一个段落可能因分页产生多个 ParagraphFrame，通过 follow/precede 链接
#[derive(Debug, Clone)]
/// 表格 cell（WROOT-030 结构性重构新增）：
/// 一行表格由若干个 TableCellFrame 组成，每个 cell 有独立的横向偏移、
/// 宽度、以及内部真正 shaping 出来的多行文字（不再是拍平的整段截断字符串）。
pub struct TableCellFrame {
    /// 在行内的横向偏移（pt，相对页面内容区左边界）
    pub x_offset: f32,
    /// cell 宽度（pt）
    pub cell_width: f32,
    /// cell 内部多行文字（真正 shaping 换行产出）
    pub lines: Vec<LineBox>,
    /// 顶部边框线样式，None表示不画（WROOT-100重构，语义同LineBox
    /// 同名字段）。
    pub border_top: Option<BorderStyle>,
    /// 底部边框线样式，None表示不画。
    pub border_bottom: Option<BorderStyle>,
    /// 左边框线样式，None表示不画（x坐标由tile.rs用x_offset现算）。
    pub border_left: Option<BorderStyle>,
    /// 右边框线样式，None表示不画。
    pub border_right: Option<BorderStyle>,
    /// BugB续修：本cell内实际产出过内容的段落数（跳过空段落后的计数），
    /// 用于 height() 精确复现 layout.rs 逐段落累积时叠加的
    /// CELL_PARAGRAPH_SPACING，否则段落越多 height() 算出来的高度
    /// 比实际排版占用高度差得越多，导致内容压穿下边框。
    pub paragraph_count: usize,
    /// v317e：各段段前段后之和（pt），计入 height()
    pub extra_spacing: f32,
    /// 表格样式移植 Layer 1步骤D：单元格背景色，来自 roe_model::common::
    /// TableCell.bg_color（docx.rs 已解析 w:tcPr/w:shd 写入该字段，见
    /// ROE_DOCX_Progress_v63.md Layer 1步骤A）。None 表示无背景色/透明，
    /// 渲染层不画填充矩形，与此前行为完全一致（零回归）。
    pub bg_color: Option<roe_model::common::Color>,
    /// 表格样式移植 Layer 1步骤B（渲染部分）：单元格边框颜色/宽度渲染用
    /// 最终值。由 layout.rs 构造处一次性合并 cell.border_color.or(table.
    /// default_border_color) 算出，不回写 model 层 TableCell（model层
    /// border_color 保持原始解析语义不被覆盖，见 ROE_DOCX_Progress_v64.md
    /// 二节"核心原则"）。None 表示最终也没有颜色/宽度可用，渲染层沿用
    /// 硬编码黑色/1.0px兜底（与此前行为一致，零回归）。
    pub border_color: Option<roe_model::common::Color>,
    pub border_width: Option<f32>,
}

impl TableCellFrame {
    /// BugB续修：必须和 layout.rs 里的 CELL_PARAGRAPH_SPACING 保持同一
    /// 数值——这是当前权宜写法（两处各自定义一份常量），更理想是提到
    /// 公共位置让两边共享同一定义，避免以后改一处忘了改另一处，此风险
    /// 记入已知技术债，留待后续重构。
    const CELL_PARAGRAPH_SPACING: f32 = 2.0;

    /// 该 cell 的高度：内部所有行高之和 + 段落间距累计
    /// （与 layout.rs 构造 cell_lines 时 cell_y_cursor 的累积公式一致：
    /// 每个非空段落之后都追加一次 CELL_PARAGRAPH_SPACING，含最后一段）
    pub fn height(&self) -> f32 {
        self.lines.iter().map(|l| l.line_height).sum::<f32>()
            + self.paragraph_count as f32 * Self::CELL_PARAGRAPH_SPACING
            + self.extra_spacing
    }
}

/// ParagraphFrame 装载的实际内容（WROOT-030 结构性重构新增）：
/// - Paragraph：普通段落的多行文字，行为与重构前完全一致
/// - TableRow：表格的一行，内含多个独立几何的 cell
pub enum FrameContent {
    Paragraph(Vec<LineBox>),
    TableRow(Vec<TableCellFrame>),
    /// 通用水平分隔线（Step 7新增）：用于脚注/尾注分隔线，命名不专属
    /// endnote，为将来复用给段落级横线等场景留空间。只描述"线本身"：
    /// 线上方留白 margin_before，线下方留白由调用方在推进排版游标时
    /// 另行处理，不在这个variant里存 margin_after。
    HorizontalRule {
        width_pt: f32,
        thickness_pt: f32,
        color: roe_model::common::Color,
        margin_before: f32,
    },
}

pub struct ParagraphFrame {
    pub id:       FrameId,
    pub page:     PageId,
    /// 后续帧（此帧内容溢出，续到下一页）
    pub follow:   Option<FrameId>,
    /// 前驱帧
    pub precede:  Option<FrameId>,
    /// 该帧装载的内容：普通段落多行文字，或表格一行的多个 cell
    pub content:  FrameContent,
    /// 该帧顶部在页面内容区的 y 偏移（pt）
    pub y_on_page: f32,
    /// 该帧所在栏的序号（0-based）。单栏文档恒为0。
    pub column_index: usize,
    /// 对应内存模型中段落的索引（用于回溯）
    pub para_index: usize,
    /// space_before（只在链头帧有效，即 precede 为 None）
    pub space_before: f32,
    /// space_after（只在链尾帧有效）
    pub space_after: f32,
    /// WROOT-101：该frame是否为浮动内容（当前只有浮动表格行会置true）。
    /// 浮动内容脱离文档纵向堆叠流——它有真实几何高度（渲染需要），
    /// 但不应该推进used_height()，否则后续段落的起始y会被推到浮动内容
    /// 底部之后，绕排逻辑形同虚设（本次实测触发：文字完全跑到表格下方
    /// 才开始排，而不是环绕在表格右侧）。图片走的是另一套模式（浮动图片
    /// content填空Vec，height()天然为0，靠渲染阶段另查doc.body原始数据
    /// 画图），表格行content必须是真实cells（没有旁路可查），故用独立
    /// 标记位跳过used_height统计，是更直接的方案。默认false，不影响
    /// 任何既有非浮动内容的行为。
    pub is_floating: bool,
}

impl ParagraphFrame {
    /// 该帧的总高度：
    /// - Paragraph：所有行高之和（不变）
    /// - TableRow：行高 = 最高的那个 cell 的高度
    pub fn height(&self) -> f32 {
        match &self.content {
            FrameContent::Paragraph(lines) => lines.iter().map(|l| l.line_height).sum(),
            FrameContent::TableRow(cells) => cells.iter()
                .map(|c| c.height())
                .fold(0.0, f32::max),
            FrameContent::HorizontalRule { thickness_pt, margin_before, .. } => thickness_pt + margin_before,
        }
    }

    /// 返回该帧的纯段落行（Paragraph variant）。TableRow 帧目前
    /// 不参与 hit_test/layout_query 光标定位（已知限制：表格行内
    /// 没有"字节偏移"这个线性语义，光标落点/命中测试暂不支持，
    /// 与 split_frame 对 TableRow 返回 None 的简化范围一致），
    /// 返回空切片，使调用方的字节计数/行遍历逻辑自然跳过整个 frame，
    /// 不需要在每个调用点做 match 分支。
    pub fn text_lines(&self) -> &[LineBox] {
        match &self.content {
            FrameContent::Paragraph(lines) => lines,
            FrameContent::TableRow(_) => &[],
            FrameContent::HorizontalRule { .. } => &[],
        }
    }

    /// 是否是链头（没有前驱）
    pub fn is_chain_head(&self) -> bool {
        self.precede.is_none()
    }

    /// 是否是链尾（没有后续）
    pub fn is_chain_tail(&self) -> bool {
        self.follow.is_none()
    }
}

/// 页面信息
#[derive(Debug, Clone)]
pub struct PageFrame {
    pub id:             PageId,
    pub width:          f32,
    pub height:         f32,
    pub margin_top:     f32,
    pub margin_bottom:  f32,
    pub margin_left:    f32,
    pub margin_right:   f32,
    /// 该页上按顺序排列的 frame id 列表
    pub frames:         Vec<FrameId>,
    /// 该页的分栏配置。num_cols=1 时退化为无分栏（现有行为）。
    pub columns:        ColumnLayout,
}

/// 某一页内的分栏几何参数（由 WriterDocument.columns 在建页时换算得到）。
#[derive(Debug, Clone)]
pub struct ColumnLayout {
    pub num_cols: usize,
    /// 每栏的 x 偏移（相对页面内容区左边界，pt），长度 = num_cols
    pub col_x_offsets: Vec<f32>,
    /// 每栏的可用宽度（pt），长度 = num_cols
    pub col_widths: Vec<f32>,
}

impl ColumnLayout {
    /// 单栏（无分栏）时的默认值，content_w 是页面内容区总宽度
    pub fn single(content_w: f32) -> Self {
        Self { num_cols: 1, col_x_offsets: vec![0.0], col_widths: vec![content_w] }
    }
}

impl PageFrame {
    pub fn content_width(&self) -> f32 {
        self.width - self.margin_left - self.margin_right
    }

    pub fn content_height(&self) -> f32 {
        self.height - self.margin_top - self.margin_bottom
    }
}

/// 整个文档的 Frame Arena
/// 所有 ParagraphFrame 和 PageFrame 都在这里统一管理
pub struct FrameArena {
    pub frames: SlotMap<FrameId, ParagraphFrame>,
    pub pages:  SlotMap<PageId, PageFrame>,
    /// 页面顺序（按文档顺序）
    pub page_order: Vec<PageId>,
    /// 每页在全局坐标系下的起始 y（pt），页间距 PAGE_GAP_PT
    pub page_global_y: std::collections::HashMap<PageId, f32>,
    /// [WROOT-任务B] 每页归属第几个 section（0-based，对应 WriterDocument.sections
    /// 的下标）。未写入的页（key不存在）表示"排版未识别到任何 section 边界"，
    /// 查询时应 fallback 到 0 或顶层字段，不应 panic。与 page_global_y 同款
    /// 设计：事后按 PageId 写入的 map，不改 new_page 签名，避免影响 split.rs/
    /// frame.rs 测试里已有的11处调用点。
    pub page_section_index: std::collections::HashMap<PageId, usize>,
    /// [WROOT-hf-parity] 每页在"所属 section 内"的相对页码（1-based）。
    /// 与 page_section_index 同款设计：事后按 PageId 写入的 map，key 不存在
    /// 时 fallback 到 1（即"当成本节第一页"，语义上是最保守的默认值，
    /// 不会导致 panic，也不会比"完全不处理"更差）。
    /// 引入原因：resolve_hf_for_page 原先用 section.title_page（节级别
    /// "是否启用首页格式"开关）直接当"这一页是不是首页"用，导致同一节
    /// 内只要 title_page=true，每一页都被当成首页；奇偶判断也错误地用了
    /// 全局物理页码而非节内页码。这个字段就是缺失的"节内第几页"信息。
    pub page_index_in_section: std::collections::HashMap<PageId, usize>,
    /// 文档级分栏配置（分栏排版设计文档v1 三、3.3节）。默认单栏，
    /// layout_impl 拿到 WriterDocument 后会重新赋值；new_page 建页时
    /// 读取这里换算出该页的 ColumnLayout。不改 new() 签名以避免影响
    /// 现有9处测试调用点，改为建 arena 后单独赋值这一个字段。
    pub doc_columns: roe_model::writer::DocumentColumns,
}

impl FrameArena {
    pub fn new() -> Self {
        Self {
            frames: SlotMap::with_key(),
            pages:  SlotMap::with_key(),
            page_order: Vec::new(),
            page_global_y: std::collections::HashMap::new(),
            page_section_index: std::collections::HashMap::new(),
            page_index_in_section: std::collections::HashMap::new(),
            doc_columns: roe_model::writer::DocumentColumns::default(),
        }
    }

    /// 页间距（pt），页与页之间的空白
    pub const PAGE_GAP_PT: f32 = 16.0;

    /// 按文档级分栏配置（DocumentColumns）+ 该页内容区宽度，换算出
    /// 具体的 ColumnLayout（每栏x偏移/宽度）。分栏排版设计文档v1 三、3.3节
    /// 三种换算规则：num_cols=1；equalWidth=1(col_widths为空，均分)；
    /// equalWidth=0(col_widths非空，逐栏累加)。
    fn compute_column_layout(doc_columns: &roe_model::writer::DocumentColumns, content_w: f32) -> ColumnLayout {
        if doc_columns.num_cols <= 1 {
            return ColumnLayout::single(content_w);
        }
        let num_cols = doc_columns.num_cols;
        if doc_columns.col_widths.is_empty() {
            // equalWidth=1：均分，栏间距用 uniform_spacing
            let spacing = doc_columns.uniform_spacing;
            let col_w = (content_w - spacing * (num_cols as f32 - 1.0)) / num_cols as f32;
            let mut col_x_offsets = Vec::with_capacity(num_cols);
            let mut col_widths = Vec::with_capacity(num_cols);
            for i in 0..num_cols {
                col_x_offsets.push(i as f32 * (col_w + spacing));
                col_widths.push(col_w);
            }
            ColumnLayout { num_cols, col_x_offsets, col_widths }
        } else {
            // equalWidth=0：逐栏显式宽度，累加算x偏移
            let col_widths = doc_columns.col_widths.clone();
            let mut col_x_offsets = Vec::with_capacity(col_widths.len());
            let mut acc = 0.0;
            for (i, &w) in col_widths.iter().enumerate() {
                col_x_offsets.push(acc);
                acc += w;
                if i < doc_columns.col_spacing.len() {
                    acc += doc_columns.col_spacing[i];
                }
            }
            ColumnLayout { num_cols: col_widths.len(), col_x_offsets, col_widths }
        }
    }

    /// 新建一页，返回 PageId。自动计算该页在全局坐标系下的起始 y。
    pub fn new_page(&mut self, width: f32, height: f32, margins: Margins) -> PageId {
        // 全局 y = 前面所有页的高度之和 + 页间距
        let global_y: f32 = self.page_order.iter().map(|&pid| {
            self.pages[pid].height + Self::PAGE_GAP_PT
        }).sum();
        let content_w = width - margins.left - margins.right;
        let columns = Self::compute_column_layout(&self.doc_columns, content_w);
        let page = PageFrame {
            id: PageId::default(), // 占位，insert 后用实际 id 更新
            width,
            height,
            margin_top:    margins.top,
            margin_bottom: margins.bottom,
            margin_left:   margins.left,
            margin_right:  margins.right,
            frames: Vec::new(),
            columns,
        };
        let pid = self.pages.insert(page);
        self.pages[pid].id = pid;
        self.page_order.push(pid);
        self.page_global_y.insert(pid, global_y);
        pid
    }

    /// 获取指定页的全局起始 y（不含 margin_top）
    pub fn page_global_y(&self, pid: PageId) -> f32 {
        *self.page_global_y.get(&pid).unwrap_or(&0.0)
    }

    /// [WROOT-任务B] 获取指定页归属第几个 section（0-based）。
    /// 查不到时 fallback 到 0（即"沿用顶层字段"的语义，与 Section 结构体
    /// 里各字段 None 表示 fallback 到 WriterDocument 顶层值是同一套约定）。
    pub fn page_section_index(&self, pid: PageId) -> usize {
        *self.page_section_index.get(&pid).unwrap_or(&0)
    }

    /// [WROOT-hf-parity] 获取指定页在所属 section 内的相对页码（1-based）。
    /// 查不到时 fallback 到 1（见字段注释）。
    pub fn page_index_in_section(&self, pid: PageId) -> usize {
        *self.page_index_in_section.get(&pid).unwrap_or(&1)
    }

    /// 获取指定页内容区的全局起始 y（含 margin_top）
    pub fn page_content_global_y(&self, pid: PageId) -> f32 {
        self.page_global_y(pid) + self.pages[pid].margin_top
    }

    /// 文档总高度（所有页高度之和 + 页间距，末页不加间距）
    pub fn total_height(&self) -> f32 {
        if self.page_order.is_empty() { return 0.0; }
        self.page_order.iter().map(|&pid| self.pages[pid].height).sum::<f32>()
            + Self::PAGE_GAP_PT * (self.page_order.len() as f32 - 1.0)
    }

    /// 在指定页上插入一个新 frame，返回 FrameId
    /// 插入一个普通段落帧（沿用重构前的语义与签名，内部包装为
    /// FrameContent::Paragraph，调用方无需感知本次重构）。
    pub fn insert_frame(
        &mut self,
        page: PageId,
        para_index: usize,
        lines: Vec<LineBox>,
        y_on_page: f32,
        space_before: f32,
        space_after: f32,
        column_index: usize,
    ) -> FrameId {
        let frame = ParagraphFrame {
            id: FrameId::default(),
            page,
            follow:  None,
            precede: None,
            content: FrameContent::Paragraph(lines),
            y_on_page,
            column_index,
            para_index,
            space_before,
            space_after,
            is_floating: false,
        };
        let fid = self.frames.insert(frame);
        self.frames[fid].id = fid;
        self.pages[page].frames.push(fid);
        fid
    }

    /// 插入一个表格行帧（WROOT-030 结构性重构新增）。
    /// 与 insert_frame 平行存在：段落走 insert_frame，表格行走这个方法，
    /// 两者共享同一个 FrameArena / SlotMap，follow/precede/分页机制完全复用。
    pub fn insert_table_row_frame(
        &mut self,
        page: PageId,
        para_index: usize,
        cells: Vec<TableCellFrame>,
        y_on_page: f32,
        space_before: f32,
        space_after: f32,
        is_floating: bool,
    ) -> FrameId {
        let frame = ParagraphFrame {
            id: FrameId::default(),
            page,
            follow:  None,
            precede: None,
            content: FrameContent::TableRow(cells),
            y_on_page,
            column_index: 0,  // 表格行不参与分栏（已知限制，见设计文档4.3节）
            para_index,
            space_before,
            space_after,
            is_floating,
        };
        let fid = self.frames.insert(frame);
        self.frames[fid].id = fid;
        self.pages[page].frames.push(fid);
        fid
    }

    /// 插入一个水平分隔线帧（Step 7新增）。与 insert_frame/
    /// insert_table_row_frame 平行存在：分隔线走这个方法，共享同一个
    /// FrameArena / SlotMap，follow/precede/分页机制完全复用。
    pub fn insert_horizontal_rule_frame(
        &mut self,
        page: PageId,
        para_index: usize,
        width_pt: f32,
        thickness_pt: f32,
        color: roe_model::common::Color,
        margin_before: f32,
        y_on_page: f32,
        space_before: f32,
        space_after: f32,
    ) -> FrameId {
        let frame = ParagraphFrame {
            id: FrameId::default(),
            page,
            follow:  None,
            precede: None,
            content: FrameContent::HorizontalRule { width_pt, thickness_pt, color, margin_before },
            y_on_page,
            column_index: 0,  // 分隔线不参与分栏，恒为0（与表格行同样的简化）
            para_index,
            space_before,
            space_after,
            is_floating: false,
        };
        let fid = self.frames.insert(frame);
        self.frames[fid].id = fid;
        self.pages[page].frames.push(fid);
        fid
    }

    /// 把 follow_id 设为 head_id 的后续帧（双向链接）
    pub fn link_follow(&mut self, head_id: FrameId, follow_id: FrameId) {
        self.frames[head_id].follow  = Some(follow_id);
        self.frames[follow_id].precede = Some(head_id);
    }

    /// 遍历从 start 开始的整条 follow chain，返回 FrameId 列表
    pub fn follow_chain(&self, start: FrameId) -> Vec<FrameId> {
        let mut chain = Vec::new();
        let mut cur = Some(start);
        while let Some(id) = cur {
            chain.push(id);
            cur = self.frames[id].follow;
        }
        chain
    }

    /// 当前最后一页
    pub fn last_page(&self) -> Option<PageId> {
        self.page_order.last().copied()
    }

    /// 指定页当前已用高度（所有 frame 高度 + space_after 之和）
    pub fn used_height(&self, page_id: PageId) -> f32 {
        let page = &self.pages[page_id];
        page.frames.iter().fold(0.0, |acc, &fid| {
            let f = &self.frames[fid];
            // WROOT-101：浮动内容（当前只有浮动表格行）脱离纵向堆叠流，
            // 不参与used_height统计，见结构体字段注释。
            if f.is_floating {
                return acc;
            }
            // 只有链尾帧才加 space_after
            let sb = if f.precede.is_none() { f.space_before } else { 0.0 };
            let sa = if f.is_chain_tail() { f.space_after } else { 0.0 };
            acc + sb + f.height() + sa
        })
    }

    /// 按 (页, 栏序号) 统计已用高度。单栏文档（column_index恒为0）时
    /// 与 used_height 结果完全一致。分栏排版设计文档 v1 三、3.2节。
    pub fn used_height_in_column(&self, page_id: PageId, column_index: usize) -> f32 {
        let page = &self.pages[page_id];
        page.frames.iter()
            .filter(|&&fid| self.frames[fid].column_index == column_index)
            .fold(0.0, |acc, &fid| {
                let f = &self.frames[fid];
                // WROOT-101：见 used_height 同款注释。
                if f.is_floating {
                    return acc;
                }
                let sb = if f.precede.is_none() { f.space_before } else { 0.0 };
                let sa = if f.is_chain_tail() { f.space_after } else { 0.0 };
                acc + sb + f.height() + sa
            })
    }

    /// 根据 para_index 找到对应的 frame 链（按文档顺序的链头开始，
    /// follow_chain 展开后即为完整链，可能跨多页）。
    /// 找不到该 para_index 时返回空 Vec（不是 panic，调用方据此判断
    /// "段落索引越界"）。
    /// 实现方式：遍历所有 frame，找到该 para_index 且 is_chain_head()
    /// 的那个作为链头，再展开 follow_chain。
    /// 注：当前是线性扫描，复杂度 O(frame总数)。如果以后文档段落数很大
    /// 且这个查询变成性能热点，可以在 layout() 阶段顺手建一个
    /// HashMap<para_index, FrameId>（链头）做 O(1) 查找——B 块先用线性
    /// 扫描走通功能，是否需要优化以后再看。
    pub fn find_chain_by_para_index(&self, para_index: usize) -> Vec<FrameId> {
        let head = self.frames.iter().find_map(|(fid, f)| {
            if f.para_index == para_index && f.is_chain_head() {
                Some(fid)
            } else {
                None
            }
        });
        match head {
            Some(fid) => self.follow_chain(fid),
            None => Vec::new(),
        }
    }
}

impl Default for FrameArena {
    fn default() -> Self { Self::new() }
}


#[cfg(test)]
mod tests_find_chain_by_para_index {
    use super::*;

    fn make_line(text: &str) -> LineBox {
        LineBox { y_offset: 0.0, ascent: 10.0, descent: 2.0, line_height: 14.0, text: text.into(), runs: Vec::new(), ..Default::default() }
    }

    #[test]
    fn test_single_frame_no_split() {
        let mut arena = FrameArena::new();
        let page = arena.new_page(595.0, 842.0, Margins::uniform(56.7));
        let fid = arena.insert_frame(page, 0, vec![make_line("第一段")], 0.0, 0.0, 4.0, 0);

        let chain = arena.find_chain_by_para_index(0);
        assert_eq!(chain, vec![fid]);
    }

    #[test]
    fn test_split_across_two_pages() {
        let mut arena = FrameArena::new();
        let page1 = arena.new_page(595.0, 842.0, Margins::uniform(56.7));
        let page2 = arena.new_page(595.0, 842.0, Margins::uniform(56.7));
        let fid1 = arena.insert_frame(page1, 5, vec![make_line("第一节")], 0.0, 0.0, 0.0, 0);
        let fid2 = arena.insert_frame(page2, 5, vec![make_line("第二节")], 0.0, 0.0, 4.0, 0);
        arena.link_follow(fid1, fid2);

        let chain = arena.find_chain_by_para_index(5);
        assert_eq!(chain, vec![fid1, fid2]);
    }

    #[test]
    fn test_not_found_returns_empty() {
        let arena = FrameArena::new();
        let chain = arena.find_chain_by_para_index(999);
        assert!(chain.is_empty());
    }

    #[test]
    fn test_multiple_paragraphs_finds_correct_one() {
        let mut arena = FrameArena::new();
        let page = arena.new_page(595.0, 842.0, Margins::uniform(56.7));
        let _f0 = arena.insert_frame(page, 0, vec![make_line("段0")], 0.0, 0.0, 4.0, 0);
        let f1 = arena.insert_frame(page, 1, vec![make_line("段1")], 14.0, 0.0, 4.0, 0);
        let _f2 = arena.insert_frame(page, 2, vec![make_line("段2")], 28.0, 0.0, 4.0, 0);

        let chain = arena.find_chain_by_para_index(1);
        assert_eq!(chain, vec![f1]);
    }

    /// v77 补充：非对称边距（top≠bottom≠left≠right）场景下，new_page()
    /// 是否把 Margins 四个字段分别正确写入 PageFrame，而不是像旧的单一
    /// f32 margin 那样把某一个值重复填了四遍。这是本次 Margins 结构体
    /// 改动要解决的核心场景，此前38个测试全部用 Margins::uniform()
    /// （四边相等），完全没覆盖到这个场景，属于交接文档记录的验证缺口。
    #[test]
    fn test_new_page_asymmetric_margins() {
        let mut arena = FrameArena::new();
        let margins = Margins { top: 50.0, bottom: 30.0, left: 40.0, right: 60.0 };
        let page_width = 595.0;
        let page_height = 842.0;
        let pid = arena.new_page(page_width, page_height, margins);
        let page = &arena.pages[pid];

        // 四个 margin 字段必须分别对应，不能互相混淆或被压成同一个值。
        assert_eq!(page.margin_top,    50.0, "margin_top 应为 50.0，实际={}", page.margin_top);
        assert_eq!(page.margin_bottom, 30.0, "margin_bottom 应为 30.0，实际={}", page.margin_bottom);
        assert_eq!(page.margin_left,   40.0, "margin_left 应为 40.0，实际={}", page.margin_left);
        assert_eq!(page.margin_right,  60.0, "margin_right 应为 60.0，实际={}", page.margin_right);

        // 四个值必须互不相同，防止测试本身因为"四个字段恰好等于同一个值"
        // 而失去区分度（即使写错了赋值顺序，只要四值不同就一定会被抓出来）。
        let vals = [page.margin_top, page.margin_bottom, page.margin_left, page.margin_right];
        for i in 0..vals.len() {
            for j in (i + 1)..vals.len() {
                assert_ne!(vals[i], vals[j], "四个 margin 值不应相等，否则测试无法区分赋值顺序是否正确");
            }
        }

        // content_w/content_h 的非对称减法：new_page 内部用 content_w 来算
        // 分栏布局，这里通过 page.width/height 结合 margin 字段反推验证，
        // 等价于交接文档里 content_w = page_width - left - right、
        // content_h = page_height - top - bottom 这两条公式。
        let expected_content_w = page_width - margins.left - margins.right;   // 595 - 40 - 60 = 495
        let expected_content_h = page_height - margins.top - margins.bottom;  // 842 - 50 - 30 = 762
        let actual_content_w = page.width - page.margin_left - page.margin_right;
        let actual_content_h = page.height - page.margin_top - page.margin_bottom;
        assert!(
            (actual_content_w - expected_content_w).abs() < 0.01,
            "content_w 应为 {}，实际算出 {}", expected_content_w, actual_content_w
        );
        assert!(
            (actual_content_h - expected_content_h).abs() < 0.01,
            "content_h 应为 {}，实际算出 {}", expected_content_h, actual_content_h
        );
    }
}
