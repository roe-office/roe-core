use serde::{Deserialize, Serialize};
use slotmap::new_key_type;
use crate::common::{Color, ImageData, Transform};
pub use crate::common::{ShapeGeometry, ShapeStyle, FillStyle, StrokeStyle, ShadowStyle, GradientStop, GradientKind};
use crate::style::StyleRegistry;

new_key_type! {
    pub struct SlideId;
    pub struct MasterSlideId;
    pub struct ShapeId;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Presentation {
    pub slide_size:    (f32, f32),
    pub slides:        Vec<Slide>,
    pub master_slides: Vec<MasterSlide>,
    pub styles:        StyleRegistry,
    /// 主题颜色表（来自 theme1.xml a:clrScheme）
    /// key: "dk1"/"lt1"/"dk2"/"lt2"/"accent1".."accent6"/"hlink"/"folHlink"
    pub theme_colors:  std::collections::HashMap<String, crate::common::Color>,
    /// PPTX-5：presentation.xml 里 <p:defaultTextStyle> 的文档级全局兜底样式
    /// （对应 docx 侧 w:docDefaults 概念，同构问题，命名不同但角色一致）。
    /// 只取 lvl1pPr/defRPr 的少量常用字段，作为占位符匹配链（layout/master 都未命中，
    /// 或命中但该字段本身是 None）时的最后一层兜底，None 表示文件未定义该层或该字段。
    pub default_text_style: DefaultTextStyle,
}

/// PPTX-5：presentation.xml <p:defaultTextStyle><a:lvl1pPr><a:defRPr .../> 对应的
/// 文档级默认文字样式，字段集合与占位符样式继承链（font_size/bold/italic/color/font_family）
/// 保持一致，便于同一套兜底逻辑复用。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DefaultTextStyle {
    pub font_size:   Option<f32>,
    pub bold:        Option<bool>,
    pub italic:      Option<bool>,
    pub color:       Option<crate::common::Color>,
    pub font_family: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Slide {
    /// v320a1: index into master.layouts of the owning master (None = unknown)
    #[serde(default)]
    pub layout_idx: Option<usize>,
    pub shapes:         Vec<Shape>,
    pub master_idx:     Option<usize>,
    pub animation_tree: AnimationNode,
    pub transition:     Option<SlideTransition>,
    pub notes:          Option<String>,
    /// PROOT-005: 本页最终生效的背景（对应 OOXML `<p:bg>`）。解析阶段已经
    /// 完成 slide 自身 → slideLayout → slideMaster 三层 fallback 查找
    /// （OOXML 规范继承顺序，自身有 `<p:bg>` 则用自身的，否则逐级向上找），
    /// 此字段存的是查找结果，渲染层不需要也不应该重新做继承判断。
    /// None 表示三层都没有定义背景（渲染层此时应使用默认白色背景，与
    /// 此前"背景填白"的兜底行为保持一致，不是"忘记解析"的信号）。
    #[serde(default)]
    pub background:     Option<crate::common::FillStyle>,
    /// PROOT-005 followup: slideLayout 里的非占位符装饰性形状（如 logo、
    /// 固定图片、装饰图案——对应 OOXML layout `<p:spTree>` 下没有 `<p:ph>`
    /// 标签的 `<p:sp>`/`<p:pic>` 等）。此前这类形状在 layout 解析时被
    /// `extract_ph_candidates` 完全过滤丢弃（该函数只收集占位符），
    /// 导致 layout 里的装饰图案永远不会出现在渲染结果里——这是独立于
    /// PROOT-005背景继承的另一个空白，真实样本 tdf158512 的 layout 右上角
    /// 太阳+小房子图案（`<p:pic>` id=7, userDrawn="1"）即是此问题的复现。
    /// 与 `background` 字段同样的设计：解析阶段一次性从 layout 提取好，
    /// 渲染层直接用，不重新做"层级查找"判断。空 Vec 表示 layout 没有
    /// 装饰性形状（或本 slide 没能关联到 layout），用 Vec 而非 Option
    /// 是因为"没有"和"空列表"语义上等价，不需要额外一层 Option 包装。
    /// 已知简化：只提取 layout 层，master 层的同类装饰形状暂未提取
    /// （master.shapes 本身已经是"全部形状"，渲染层早已在 render_slide
    /// 里遍历 master.shapes 并跳过占位符，master 层此问题不存在，
    /// 只有 layout 层缺失这条渲染路径）。
    #[serde(default)]
    pub layout_shapes:  Vec<Shape>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MasterSlide {
    /// v320b0: raw theme xml of this master, written back verbatim
    #[serde(default)]
    pub theme_xml: Option<String>,
    /// v320b0: raw <p:txStyles> element, written back verbatim
    #[serde(default)]
    pub tx_styles_xml: Option<String>,
    /// v320a1: real layout objects of this master
    #[serde(default)]
    pub layouts: Vec<SlideLayout>,
    /// v320a1: master clrMap (slot -> slot), None = not parsed
    #[serde(default)]
    pub color_map: Option<std::collections::HashMap<String, String>>,
    /// v320a1: format specific content kept verbatim
    #[serde(default)]
    pub raw_ext: Option<String>,
    pub name:   String,
    pub shapes: Vec<Shape>,
    /// PROOT-005: master 自身的 `<p:bg>`（若有），供 slide 三层 fallback
    /// 查找到 master 这一级时使用。None 表示 master 本身也没有定义背景。
    #[serde(default)]
    pub background: Option<crate::common::FillStyle>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TextAutofit {
    NoAutofit,
    SpAutoFit,
    NormAutofit { font_scale: Option<f32>, ln_spc_reduction: Option<f32> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TextAnchor { Top, Middle, Bottom }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextBodyProps {
    pub autofit: TextAutofit,
    pub wrap:    bool,
    pub anchor:  TextAnchor,
    pub ins_l:   Option<f32>,
    pub ins_r:   Option<f32>,
    pub ins_t:   Option<f32>,
    pub ins_b:   Option<f32>,
    /// PROOT-1xx (v99): 对应 OOXML <a:bodyPr vert="..."/>，文本框整体
    /// 排版方向。real样本核实（golden_progress_pptx bug_log.md PROOT-1xx
    /// 竖排系列）确认 vert/vert270/eaVert 三个取值在视觉上表现一致：
    /// 文字字形本身不旋转，仍是正常横排书写，但承载文字的整个文本框被
    /// 当成一个整体绕自身中心旋转90°（vert顺时针/vert270逆时针，eaVert
    /// 实测与vert270视觉一致，按同一机制处理），排版宽高相应对调。
    /// 与shape本身的xfrm rot（形状级旋转，含边框/填充一起转）是两个独立
    /// 维度：本字段只影响render_shape_text内部的文字排版和局部旋转变换，
    /// 不影响shape边框/填充渲染路径。
    #[serde(default)]
    pub vert:    TextVert,
}

/// PROOT-1xx (v99): OOXML <a:bodyPr vert="..."/> 取值。默认Horizontal
/// 对应vert属性缺失或显式"horz"（规范默认值，未强制区分"缺失"和"horz"，
/// 因为渲染行为相同）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TextVert {
    #[default]
    Horizontal,
    Vert,
    Vert270,
    EaVert,
}

impl Default for TextBodyProps {
    fn default() -> Self {
        Self {
            autofit: TextAutofit::NoAutofit,
            wrap:    true,
            anchor:  TextAnchor::Top,
            ins_l:   None,
            ins_r:   None,
            ins_t:   None,
            ins_b:   None,
            vert:    TextVert::Horizontal,
        }
    }
}

/// PROOT bullet: OOXML <a:pPr> 的项目符号/编号信息。挂在
/// roe_model::writer::Paragraph.bullet 字段上（pptx 复用 writer 的
/// Paragraph 类型，docx 侧该字段恒为 None，不受影响）。
///
/// 设计依据：本字段专为 OOXML PPTX 的内联 bullet 语义设计,
/// 不同于 writer::ListInfo（后者服务 docx/ODF 的"命名列表样式+
/// 外部注册表"体系）。两者字段形状不同，不应合并或复用，参见
/// roe-ext 进度文档 PROOT bullet 排查记录。
///
/// 继承规则（已用 onlyoffice 源码 core/OOXML/PPTXFormat 交叉验证,
/// 见 Shape::Merge / TextParagraphPr::Merge / SlideMaster::GetLevelUp）:
/// 按属性级别（field-level）合并，从粗到细依次覆盖：
///   1. master 的 <p:txStyles><p:bodyStyle|titleStyle|otherStyle>
///      （按占位符类型选择分类，按 lvl 索引 levels[10] 数组）
///   2. layout 对应占位符自己的 <p:txBody><a:lstStyle>（若非空）
///   3. slide 上实际 shape 自己的 <p:txBody><a:lstStyle>（若非空）
///   4. 段落自身 <a:pPr> 的属性（最终覆盖，优先级最高）
/// 不是"整体替换"，是"某一层有值就覆盖，没有就保留上一层的值"。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BulletInfo {
    pub kind:   BulletKind,
    /// 左边距，pt（对应 OOXML marL，EMU -> pt 已在解析层换算）
    pub mar_l:  Option<f32>,
    /// 悬挂缩进，pt，通常为负值（对应 OOXML indent，EMU -> pt 已换算）。
    /// bullet 符号绘制位置 = mar_l + indent；正文起始位置 = mar_l。
    pub indent: Option<f32>,
    /// PROOT bullet 颜色: 对应 <a:buClr><a:srgbClr val="..."/></a:buClr>
    /// （或 schemeClr 主题色引用）。与段落正文颜色是独立的两套颜色，
    /// 不应混用或共用 defRPr 的 color 字段——真实样本 bulletColor 证明
    /// bullet 符号颜色可以与正文颜色完全不同（红色bullet+黑色正文）。
    /// 此前渲染层无此字段来源，只能用正文首个run颜色兜底近似，见
    /// roe-ext 进度文档 v100 PROOT bullet-color 排查记录。
    pub color:  Option<crate::common::Color>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Default)]
pub enum BulletKind {
    /// 对应 <a:buNone/>：显式关闭 bullet（即使上层继承链有 bullet 定义，
    /// 这一级显式声明"这一段不要 bullet"，必须能覆盖上层，不能只用
    /// Option<BulletKind>::None 表达，否则无法区分"没写"和"显式关闭"）。
    #[default]
    None,
    /// 对应 <a:buChar char="..."/>。font 对应 <a:buFont typeface="..."/>，
    /// 渲染层若找不到该字体，需要有 fallback（多数 bullet 字符如 •
    /// 在常见字体里都有，不强依赖 buFont 指定的字体）。
    Char { ch: String, font: Option<String> },
    /// 对应 <a:buAutoNum type="..." startAt=".../>。scheme 保留 OOXML
    /// 原始 type 字符串（如 "arabicPeriod"、"romanUcPeriod"），格式化
    /// 规则交给渲染层决定，解析层不做转换。
    ///
    /// MVP 范围（已与用户确认，见 roe-ext 进度文档）：本次只实现"同一
    /// shape 内按出现顺序连续编号"的基础渲染，不实现多级列表编号在
    /// level 切换/中断时的重置规则——OOXML 规范本身对此场景定义模糊，
    /// 且当前样本库没有真实样本命中这个场景，无法验证任何具体规则的
    /// 正确性，属于已知缺口，留待有真实样本时再排查修复。
    AutoNum { scheme: String, start_at: Option<i32> },
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Shape {
    pub id:           u32,
    pub geometry:     ShapeGeometry,
    pub position:     Transform,
    pub style:        ShapeStyle,
    pub content:      ShapeContent,
    pub text_body:    TextBodyProps,
    /// 占位符身份信息（PPTX-3新增）。None 表示这是普通形状，不是占位符。
    /// 保留身份信息（而非像此前那样在解析阶段一次性完成继承后就丢弃），
    /// 用于：①未来占位符匹配算法重新执行时可以识别哪些形状带占位符身份，
    /// ②M2阶段AI通过Rhai脚本做语义化操作（如"把标题改成XXX"）时可以按占位符类型定位形状，
    /// ③空占位符（无实际内容，只是壳）未来可以正确识别并处理（而非被当成普通空文本框）。
    /// 本次只做"加字段保留身份"这一步，不改变现有"解析时一次性继承位置/样式"的逻辑。
    #[serde(default)]
    pub placeholder:  Option<PlaceholderInfo>,
    /// PROOT-002: 对应 OOXML <p:cxnSp><p:cNvCxnSpPr><a:stCxn id=".." idx=".."/>。
    /// (目标shape的id, 连接点idx)。仅记录语义信息，不参与渲染阶段的坐标计算——
    /// 渲染直接使用 position（xfrm）字段，因为 PowerPoint/LibreOffice 保存文件时
    /// 已经把 stCxn/endCxn 换算的最终坐标写入了 xfrm，第一期不重复这个计算。
    /// 这两个字段是为未来编辑场景（拖动shape后需要重新计算连接线端点）预留的
    /// 语义信息通道，None 表示该端点未绑定到任何shape（自由端点）或该shape
    /// 本身不是connector。参见 roe-ext_progress_v35 PROOT-002章节的技术债记录：
    /// idx→坐标的动态映射规则（含shape自定义连接点表、flipH/flipV重映射）
    /// 和 bentConnector/curvedConnector 自动路由算法均未实现。
    #[serde(default)]
    pub st_cxn:  Option<(u32, u32)>,
    /// PROOT-002: 对应 <a:endCxn id=".." idx=".."/>，语义同 st_cxn。
    #[serde(default)]
    pub end_cxn: Option<(u32, u32)>,
}

/// 占位符身份信息：类型 + 索引，对应 OOXML <p:ph type="..." idx="..."/>
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaceholderInfo {
    /// "title" / "body" / "ctrTitle" / "subTitle" / "obj" / ...
    /// None 时按 OOXML 规范默认视为 "body" 类型（<p:ph> 无 type 属性时的默认值）。
    pub ph_type: Option<String>,
    /// 对应 <p:ph idx="...">，用于同类型多个占位符消歧
    pub index:   Option<u32>,
    /// PROOT: 空占位符提示文案攻坚二期——该占位符从 layout/master 匹配到的
    /// 真实文字格式（字号/颜色/字重/字体/对齐/垂直锚点），供渲染层在
    /// 该占位符判定为空（无实际runs）时，构造提示文字使用同一份格式，
    /// 而非固定灰色兜底样式。None 表示未匹配到任何 layout/master 候选
    /// （理论上少见，此时渲染层继续走固定兜底样式）。
    /// 与 roe-parser 内部 LayoutPhStyle 字段一一对应，见 placeholder_hint_design
    /// 设计方案文档九、节。
    #[serde(default)]
    pub hint_style: Option<PlaceholderHintStyle>,
}

/// 占位符从 layout/master 继承到的真实文字格式，用于空占位符提示文字渲染。
/// 字段集合与 roe-parser::ooxml::pptx::LayoutPhStyle 一一对应（parser内部
/// 私有类型转换成model层公开类型时逐字段搬运，不合并/不新增语义）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlaceholderHintStyle {
    #[serde(default)]
    pub font_size:   Option<f32>,
    #[serde(default)]
    pub bold:        Option<bool>,
    #[serde(default)]
    pub italic:      Option<bool>,
    #[serde(default)]
    pub color:       Option<crate::common::Color>,
    #[serde(default)]
    pub font_family: Option<String>,
    #[serde(default)]
    pub algn:        Option<crate::style::Alignment>,
    #[serde(default)]
    pub anchor:      Option<TextAnchor>,
    /// PROOT-1xx (v99): layout/master bodyPr vert（文本框整体排版方向）
    #[serde(default)]
    pub vert:        Option<TextVert>,
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChartSeries {
    pub name: String,
    pub data: Vec<f64>,
    #[serde(default)]
    pub color: Option<crate::common::Color>,
    /// 组合图：该系列在 <c:lineChart> 中，需按折线绘制（柱+线组合图）。
    #[serde(default)]
    pub as_line: bool,
    /// 该系列带线性趋势线（<c:trendline> 且 trendlineType=linear）。
    #[serde(default)]
    pub trendline: bool,
}

/// OOXML `<c:grouping>`：系列的分组方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ChartGrouping {
    /// 标准/簇状（并排或独立绘制）
    #[default]
    Standard,
    /// 堆积
    Stacked,
    /// 百分比堆积
    PercentStacked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChartKind {
    Bar,
    HorizontalBar,
    Line,
    Pie,
    Scatter,
    Radar,
    Area,
    Doughnut,
}

/// OOXML `<c:dLblPos>`：数据标签位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DataLabelPos {
    /// 未指定，由渲染层按图表类型决定
    #[default]
    Auto,
    Center,
    InsideEnd,
    InsideBase,
    OutsideEnd,
    BestFit,
    Left,
    Right,
    Top,
    Bottom,
}

/// 数据标签设置（来自 `<c:dLbls>`）。目前只关心"是否显示数值"和位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DataLabels {
    #[serde(default)]
    pub show_val: bool,
    /// 文档里出现了 `<c:dLbls>`（区分"明确不显示"与"没写"，Calc 旧图表为 false）。
    #[serde(default)]
    pub explicit: bool,
    #[serde(default)]
    pub position: DataLabelPos,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChartModel {
    /// 图表稳定标识，自增，同一 sheet 内唯一。0 保留为"无效/未分配"哨兵值，
    /// 不会被 insert_chart 分配给真实图表。
    #[serde(default)]
    pub id: u64,
    pub kind: ChartKind,
    pub title: Option<String>,
    pub x_labels: Vec<String>,
    pub series: Vec<ChartSeries>,
    /// 锚点左边距（pt，相对于 sheet 内容区），浮动图表定位用。
    /// 历史 parser 路径（fods/xlsx 解析旧文档）暂未回填，默认 0.0。
    #[serde(default)]
    pub x_pt: f32,
    /// 锚点上边距（pt）
    #[serde(default)]
    pub y_pt: f32,
    /// 图表宽度（pt）
    #[serde(default = "default_chart_size")]
    pub width_pt: f32,
    /// 图表高度（pt）
    #[serde(default = "default_chart_size")]
    pub height_pt: f32,
    /// 是否显示图例，默认true
    #[serde(default = "default_true")]
    pub show_legend: bool,
    /// 是否显示网格线，默认true
    #[serde(default = "default_true")]
    pub show_grid: bool,
    /// X轴标题，默认空
    #[serde(default)]
    pub x_axis_label: String,
    /// Y轴标题，默认空
    #[serde(default)]
    pub y_axis_label: String,
    /// 图表外边框颜色，默认浅灰色 #cccccc
    #[serde(default = "default_chart_border_color")]
    pub border_color: crate::common::Color,
    /// 图表外边框宽度（pt），默认1.0
    #[serde(default = "default_chart_border_width")]
    pub border_width: f32,
    /// PPTX `<c:gapWidth val="..."/>`：柱间距占柱宽的百分比（如219表示
    /// 间距=柱宽的219%）。仅对Bar chart有意义，None时使用charts-rs默认
    /// 间距。历史文档/其他解析路径未提供时默认None，不影响现有渲染行为。
    #[serde(default)]
    pub gap_width_pct: Option<f32>,
    /// OOXML `<c:overlap>`：簇内柱子重叠百分比（-100~100，负数=柱间有空隙，100=完全重叠）。
    /// None 时按渲染层默认处理。
    #[serde(default)]
    pub overlap_pct: Option<f32>,
    /// OOXML `<c:grouping>`：标准 / 堆积 / 百分比堆积。旧数据默认 Standard。
    #[serde(default)]
    pub grouping: ChartGrouping,
    /// OOXML `<c:holeSize>`：环形图中心孔占外径的百分比（如 75）。None 时用渲染层默认值。
    #[serde(default)]
    pub hole_size_pct: Option<f32>,
    /// 饼图/环形图扇区描边色（来自 `<c:dPt><c:spPr><a:ln>`，Word 默认为白色分隔线）。
    #[serde(default)]
    pub slice_border_color: Option<crate::common::Color>,
    /// OOXML `<c:dLbls>`：数据标签（是否显示数值 + 位置）。旧数据默认不显示。
    #[serde(default)]
    pub data_labels: DataLabels,
}

fn default_chart_size() -> f32 { 300.0 }
/// 图表标题占位时的默认文字（文档有标题块但没写文字）。
pub const DEFAULT_CHART_TITLE: &str = "我的图表";
/// 坐标轴标题占位时的默认文字。
pub const DEFAULT_X_AXIS_TITLE: &str = "X轴";
pub const DEFAULT_Y_AXIS_TITLE: &str = "Y轴";

fn default_true() -> bool { true }
fn default_chart_border_color() -> crate::common::Color {
    crate::common::Color { r: 0xcc, g: 0xcc, b: 0xcc, a: 255 }
}
fn default_chart_border_width() -> f32 { 1.0 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ShapeContent {
    Text(Vec<crate::writer::Paragraph>),
    Image(ImageData),
    Table(crate::common::Table),
    Chart(ChartModel),
    /// v222: 统计类图表（直方/箱线/瀑布/漏斗/旭日/树状），对应 chartEx
    StatChart(crate::stat_chart::StatChartModel),
    /// PPTX-9：分组形状（对应 OOXML <p:grpSp>）。子 Shape 列表，可嵌套。
    /// 采用"内容互斥"设计（而非给 Shape 加独立 children 字段）：一个 Shape
    /// 要么是文字/图片/表格/图表/分组之一，改动集中在已有 match 分支上，
    /// 依据见《坐标变换绘制顺序图片表格.md》§2.4/§4.2。
    ///
    /// PROOT-D 子问题2 bugfix（roe-ext_progress_v62）：新增 ch_off/ch_ext，
    /// 对应 OOXML <p:grpSpPr><a:xfrm><a:chOff>/<a:chExt>（子坐标系原点/
    /// 尺寸）。此前这两个值在解析层被读出即丢弃（隐含假设子坐标系与父
    /// 坐标系 1:1 无缩放），真实样本 tdf141463_GroupTransform 的
    /// chExt(5760000x4320000) 与 ext(5760000x2880000) 不相等，证明这个
    /// 假设不成立——子 Shape 需要按 (ext/ch_ext) 比例缩放才能落到正确
    /// 位置/尺寸。None 表示该分组未显式给出 chOff/chExt（理论上 OOXML
    /// 规范要求 grpSpPr/xfrm 内 chOff/chExt 是必需属性，但保留 None 分支
    /// 是为了兼容非 PPTX 来源（如 ODF）未来复用这个 Group 变体时不强制
    /// 提供这两个值——渲染层应在 None 时按无缩放（1:1）处理，与此前行为
    /// 一致，不引入新的 panic 风险。
    Group {
        children: Vec<Shape>,
        ch_off: Option<(f32, f32)>,
        ch_ext: Option<(f32, f32)>,
    },
    Empty,
}

// ─── 动画树（文档层，持久化）────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AnimationNode {
    TimingRoot   { children: Vec<AnimationNode> },
    MainSequence { effects: Vec<AnimationEffect> },
    InteractiveSequence {
        trigger_shape_id: u32,
        effects: Vec<AnimationEffect>,
    },
    Parallel  { children: Vec<AnimationNode>, timing: TimingSpec },
    Sequence  { children: Vec<AnimationNode>, timing: TimingSpec },
    Animate          { target: u32, property: AnimProperty,    timing: TimingSpec },
    AnimateColor     { target: u32, from: Color, to: Color,   timing: TimingSpec },
    AnimateMotion    { target: u32, path: String,              timing: TimingSpec },
    AnimateTransform { target: u32, kind: TransformKind,       timing: TimingSpec },
    AnimateSet       { target: u32, property: AnimProperty, to: AnimValue, timing: TimingSpec },
    Audio            { source: String,                         timing: TimingSpec },
}

impl Default for AnimationNode {
    fn default() -> Self {
        AnimationNode::TimingRoot { children: vec![] }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimationEffect {
    pub node: AnimationNode,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[derive(Default)]
pub enum EasingFunction {
    #[default]
    Linear,
    EaseIn,
    EaseOut,
    EaseInOut,
    // 具体曲线类型
    CubicIn,
    CubicOut,
    CubicInOut,
    QuadIn,
    QuadOut,
    QuadInOut,
    SineIn,
    SineOut,
    SineInOut,
    BounceOut,
    ElasticIn,
    ElasticOut,
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimingSpec {
    pub begin:     BeginCondition,
    pub duration:  f64,           // 秒，-1 表示 indefinite
    pub repeat:    RepeatSpec,
    pub fill:      FillMode,
    pub node_type: EffectNodeType,
    pub easing:    EasingFunction,
}

/// ⚠️ LOK 已知 Bug：Sequence<Any>（多条件 OR）在 LOK generateEvent() 里
/// 直接 OSL_FAIL 丢弃。ROE 用 Multi 变体修掉这个坑。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BeginCondition {
    Offset(f64),                    // begin="0.5s"
    OnClick(u32),                   // spTrig → ON_CLICK，shape id
    AfterPrevious(f64),             // afterEffect + delay offset
    WithPrevious(f64),              // withEffect + delay offset
    OnNext,                         // ON_NEXT，用户点击推进
    OnBeginOf(u32),                 // BEGIN_EVENT，监听另一节点（node index）
    OnEndOf(u32),                   // END_EVENT
    Indefinite,                     // begin="indefinite"
    Multi(Vec<BeginCondition>),     // 多条件 OR，LOK 不支持，ROE 修复
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum EffectNodeType {
    TimingRoot, MainSequence, InteractiveSequence,
    OnClick, WithPrevious, AfterPrevious,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RepeatSpec {
    Count(f64), Indefinite, None,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum FillMode {
    Remove, Freeze, Hold, Transition, Auto, Default,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AnimProperty {
    Opacity, PositionX, PositionY,
    ScaleX, ScaleY, Rotation,
    FillColor, StrokeColor,
    Visibility, Custom(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AnimValue {
    Number(f64), Color(Color), Text(String), Bool(bool),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TransformKind {
    Scale, Rotate, Translate, SkewX, SkewY,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlideTransition {
    pub kind:     TransitionKind,
    pub duration: f64,
    pub auto_advance: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TransitionKind {
    None, Fade, Push, Wipe, Split, Zoom, Custom(String),
}

/// v320a1: layout kind (neutral naming, OOXML ST_SlideLayoutType subset)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SlideLayoutKind {
    Title,
    TitleAndContent,
    SectionHeader,
    TwoContent,
    TitleOnly,
    Blank,
    #[default]
    Custom,
}

/// v320a1: a real slide layout object. shapes include placeholders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlideLayout {
    pub name: String,
    #[serde(default)]
    pub kind: SlideLayoutKind,
    #[serde(default)]
    pub shapes: Vec<Shape>,
    #[serde(default)]
    pub background: Option<crate::common::FillStyle>,
    #[serde(default = "default_true")]
    pub show_master_shapes: bool,
    /// clrMapOvr override, None = inherit master
    #[serde(default)]
    pub color_map: Option<std::collections::HashMap<String, String>>,
    #[serde(default)]
    pub raw_ext: Option<String>,
}
