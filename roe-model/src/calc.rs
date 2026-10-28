use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use crate::style::StyleRegistry;
use crate::common::{ShapeGeometry, FillStyle};
use crate::writer::Paragraph;

/// ROOT-019: 公式解析方言来源。formualizer-parse 默认Excel方言，无法正确解析
/// ODF/OpenFormula约定的 `[.A2]` 方括号引用、`COM.MICROSOFT.`/`ORG.OPENOFFICE.`
/// 命名空间函数前缀。workbook需要记住自己来自哪种格式，求值时roe-calc才能
/// 选对 formualizer_parse::FormulaDialect。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[derive(Default)]
pub enum FormulaDialectSource {
    /// xlsx / csv 等 OOXML 及无方言概念的格式
    #[default]
    Excel,
    /// ods / fods 等 ODF 格式，公式使用 OpenFormula（ODFF）语法
    OpenFormula,
}


/// optimalRowHeight（ROOT-082）：行高状态。
///
/// 对应 xlsx `<row customHeight="...">`：
/// - `customHeight="true"`：用户/原文件明确设置的高度，是最终权威值，
///   不做任何最小值/合理性校验（Manual 分支故意不夹紧，尊重用户故意
///   设很矮的表头美化/隐藏行等场景）。
/// - `customHeight="false"` 或该属性缺失：`ht` 只是"上次保存的建议值"，
///   不应直接采信，需要按内容自动计算（见 roe-core::tile 的懒加载
///   测量逻辑，方案A：Layout 阶段 `&mut Sheet` 按需测量回写，Paint/
///   Query 阶段 `&Sheet` 纯函数读取，两阶段职责分离）。
///
/// 用枚举而非并行数组（`Vec<f32>` + `Vec<bool>`）表达互斥状态，避免
/// 长度不同步风险，交给编译器做穷尽匹配校验。
/// 设计依据：optimalRowHeight_design_v1.md 三、2节。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum RowHeight {
    /// customHeight="true"：直接使用，权威值。
    Manual(f32),
    /// customHeight="false" 或缺失：需要自动计算。
    /// current：当前可用高度，初始为 default_height 占位估算值，
    ///          视口驱动测量完成后更新为真实测量值。
    /// is_dirty：是否需要（重新）计算。初始 true；测量完成后置
    ///          false；未来编辑功能接入后内容/样式/列宽变化应重新
    ///          置 true（见 invalidate_row_height，本次只预留钩子，
    ///          不实现触发逻辑）。
    Auto { current: f32, is_dirty: bool },
}

impl RowHeight {
    /// 当前可用高度数值（Manual 直接返回；Auto 返回 current，
    /// 可能是尚未测量的估算值）。不触发任何测量副作用，纯读取。
    #[inline]
    pub fn current(&self) -> f32 {
        match self {
            RowHeight::Manual(h) => *h,
            RowHeight::Auto { current, .. } => *current,
        }
    }

    /// 构造一个待自动计算的行高，初始为估算值。
    #[inline]
    pub fn new_auto(default_height: f32) -> Self {
        RowHeight::Auto { current: default_height, is_dirty: true }
    }
}

impl Sheet {
    /// 在按索引展开的 Vec 中于 idx 处插入 fill;idx 越界(>= len)表示该位置
    /// 及之后本来就是"缺省值",无需处理,避免凭空撑长。
    fn vec_insert_at<T>(v: &mut Vec<T>, idx: usize, fill: T) {
        if idx < v.len() {
            v.insert(idx, fill);
        }
    }

    /// 同上,删除 idx 处元素;越界则忽略。
    fn vec_remove_at<T>(v: &mut Vec<T>, idx: usize) {
        if idx < v.len() {
            v.remove(idx);
        }
    }

    /// 在 row 处插入一行后,平移"按行号索引"但 cells/row_heights/merged_cells
    /// 之外的字段:cell_formats、row_hidden、row_outline_level。
    /// (cells/row_heights/merged_cells 由 insert_row_impl 自己处理。)
    /// 已知未处理(遗留): cond_formats / data_validations / auto_filter /
    /// floating_* / tables / 公式引用。
    pub fn shift_rows_for_insert(&mut self, row: u32) {
        if !self.cell_formats.is_empty() {
            let old = std::mem::take(&mut self.cell_formats);
            self.cell_formats = old
                .into_iter()
                .map(|((r, c), f)| (if r >= row { (r + 1, c) } else { (r, c) }, f))
                .collect();
        }
        Self::vec_insert_at(&mut self.row_hidden, row as usize, false);
        Self::vec_insert_at(&mut self.row_outline_level, row as usize, 0u8);
    }

    /// 批量删除行后,补齐 row_hidden / row_outline_level
    /// (delete_rows_batch 已处理 cells/row_heights/merged_cells/cell_formats)。
    /// to_delete 必须已排序去重。
    pub fn shift_rows_for_delete(&mut self, sorted_deleted: &[u32]) {
        let del: std::collections::HashSet<u32> = sorted_deleted.iter().copied().collect();
        let keep = |i: usize| !del.contains(&(i as u32));
        if !self.row_hidden.is_empty() {
            let old = std::mem::take(&mut self.row_hidden);
            self.row_hidden = old.into_iter().enumerate().filter(|(i, _)| keep(*i)).map(|(_, v)| v).collect();
        }
        if !self.row_outline_level.is_empty() {
            let old = std::mem::take(&mut self.row_outline_level);
            self.row_outline_level = old.into_iter().enumerate().filter(|(i, _)| keep(*i)).map(|(_, v)| v).collect();
        }
    }

    /// 在 col 处插入一列后,平移 cell_formats 及按列索引展开的
    /// col_style / col_hidden / col_outline_level。
    /// (cells.cols / col_widths / merged_cells 由 insert_column 闭包自己处理。)
    pub fn shift_cols_for_insert(&mut self, col: u32) {
        if !self.cell_formats.is_empty() {
            let old = std::mem::take(&mut self.cell_formats);
            self.cell_formats = old
                .into_iter()
                .map(|((r, c), f)| (if c >= col { (r, c + 1) } else { (r, c) }, f))
                .collect();
        }
        Self::vec_insert_at(&mut self.col_style, col as usize, None);
        Self::vec_insert_at(&mut self.col_hidden, col as usize, false);
        Self::vec_insert_at(&mut self.col_outline_level, col as usize, 0u8);
    }

    /// 删除 col 列后,平移 cell_formats(该列格式丢弃、右侧左移)及
    /// col_style / col_hidden / col_outline_level。
    pub fn shift_cols_for_delete(&mut self, col: u32) {
        if !self.cell_formats.is_empty() {
            let old = std::mem::take(&mut self.cell_formats);
            self.cell_formats = old
                .into_iter()
                .filter(|((_, c), _)| *c != col)
                .map(|((r, c), f)| (if c > col { (r, c - 1) } else { (r, c) }, f))
                .collect();
        }
        Self::vec_remove_at(&mut self.col_style, col as usize);
        Self::vec_remove_at(&mut self.col_hidden, col as usize);
        Self::vec_remove_at(&mut self.col_outline_level, col as usize);
    }

    /// 把指定行的自动行高置为待重测(is_dirty=true)。
    /// - 仅作用于 `RowHeight::Auto`;`Manual`(customHeight,用户/文件指定的
    ///   权威值)不动。
    /// - 保留 `current` 作为重测前的占位值,避免行高瞬间塌成 0。
    /// - `row_heights` 可能比行数短;越界即该行本来就是"缺失=待测量"
    ///   (见 ensure_row_height_measured_single 的 None => true),无需处理。
    /// 调用时机:该行内容、单元格格式(wrap_text/字号等)、所涉列宽变化后。
    pub fn invalidate_row_height(&mut self, row: u32) {
        if let Some(RowHeight::Auto { is_dirty, .. }) = self.row_heights.get_mut(row as usize) {
            *is_dirty = true;
        }
    }

    /// 本表生效的默认行高:sheet 自带值(>0)优先,否则 15.0。
    /// 测量下限、补行等处统一用它,避免各处各写一份字面量。
    pub fn effective_default_row_height(&self) -> f32 {
        if self.default_row_height > 0.0 { self.default_row_height } else { 15.0 }
    }
}

impl Default for RowHeight {
    /// 默认落到 Auto+dirty：与"缺失 customHeight"语义一致，交给
    /// 懒加载测量流程处理，而不是默认当作用户手动设置的权威值。
    fn default() -> Self {
        RowHeight::Auto { current: 0.0, is_dirty: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workbook {
    pub sheets:       Vec<Sheet>,
    pub named_ranges: std::collections::HashMap<String, CellRange>,
    /// ROOT tdf164895: definedName 里的"公式类型"命名定义（如
    /// `<definedName name="Cnt">COUNTA($A:$A)</definedName>`），key=名字，
    /// value=原始公式文本（不含前导'='）。与 named_ranges 是互斥的两类：
    /// 一个 definedName 要么是区域引用（进 named_ranges），要么是公式表达式
    /// （进这里），解析期 parse_defined_name_ref 失败（没有'!'分隔符）时
    /// 才会尝试当作公式类收进这个字段。
    #[serde(default)]
    pub named_formulas: std::collections::HashMap<String, String>,
    pub styles:       StyleRegistry,
    /// ROOT-019: 公式解析方言来源，见 FormulaDialectSource 注释。
    #[serde(default)]
    pub formula_dialect_source: FormulaDialectSource,
}
/// Calc 浮动统计图表(chartEx)。位置/尺寸语义同 ChartModel:pt,相对 sheet 内容区。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FloatingStatChart {
    /// 稳定标识,0 为无效哨兵值;与同 sheet 的 floating_charts 共用 id 空间。
    #[serde(default)]
    pub id: u64,
    pub x_pt: f32,
    pub y_pt: f32,
    pub width_pt: f32,
    pub height_pt: f32,
    pub model: crate::stat_chart::StatChartModel,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Sheet {
    pub name:        String,
    pub cells:       SparseGrid,
    pub col_widths:  Vec<f32>,
    pub row_heights: Vec<RowHeight>,
    pub frozen_rows: u32,
    pub frozen_cols: u32,
    /// ROOT-041: workbook.xml <workbookPr date1904="1"/> 标记。
    /// true表示该workbook使用1904日期系统（Mac Excel传统，序列号1对应
    /// 1904-01-01），false（默认）为标准1900日期系统（序列号1对应
    /// 1900-01-01，序列号60起有著名的1900年虚构闰年偏移）。虽然date1904
    /// 是workbook级别的设置，但实际使用中同一workbook内所有sheet共享
    /// 同一个值，直接挂在Sheet上避免序列化模型层需要额外传递Workbook
    /// 引用到日期渲染函数（tile.rs三条渲染路径目前都只持有sheet，不持有
    /// workbook）。解析时由roe-parser从workbook.xml读取后广播到每个sheet。
    #[serde(default)]
    pub date1904: bool,
    /// sheetView zoomScale 属性（百分比，如 100 表示 100%），省略时 xlsx 默认视为 100
    pub zoom_scale: Option<u32>,
    /// sheetView showGridLines 属性，省略时 xlsx 默认视为 true
    pub show_grid_lines: Option<bool>,
    /// 单元格格式：(row, col) → CellFormat
    pub cell_formats:    std::collections::HashMap<(u32,u32), CellFormat>,
    /// 条件格式规则列表
    pub cond_formats:    Vec<ConditionalFormatRule>,
    /// 数据验证规则列表
    pub data_validations: Vec<DataValidationRule>,
    /// 自动筛选（可选）
    pub auto_filter:     Option<AutoFilter>,
    /// 合并单元格区域列表
    pub merged_cells:    Vec<MergedCell>,
    /// 透视表元数据（若此 sheet 是透视结果 sheet）
    pub pivot:           Option<PivotMeta>,
    /// xlsx表格(Table/ListObject)元数据列表。ROOT-011修复(v127)：
    /// 结构化引用(如 MyTable1[[#This Row],[列名]])求值需要这份元数据，
    /// 通过 roe-calc 的 formualizer engine.define_table() 注册。
    pub tables:          Vec<Table>,
    /// 浮动图表列表
    pub floating_charts: Vec<crate::impress::ChartModel>,
    /// 浮动统计图表(chartEx:直方/箱线/瀑布/漏斗/旭日/树状)。id 与 floating_charts 共用同一 id 空间。
    #[serde(default)]
    pub floating_stat_charts: Vec<FloatingStatChart>,
    pub floating_images:  Vec<FloatingImage>,
    /// ROOT-053/054：浮动形状/文本框列表
    #[serde(default)]
    pub floating_shapes: Vec<FloatingShape>,
    /// 浮动对象 id 单调计数器(只增不减,删除后不复用;撤销不回滚)。0 表示尚未初始化。
    #[serde(default)]
    pub next_object_id: u64,
    /// 默认列宽（pt），来自 sheetFormatPr defaultColWidth，空白列 fallback 用
    pub default_col_width: f32,
    /// 默认行高（pt），来自 sheetFormatPr defaultRowHeight，空白行 fallback 用
    pub default_row_height: f32,
    /// 列隐藏状态，按索引展开（同 col_widths 风格）。越界视为 false（未隐藏）。
    pub col_hidden: Vec<bool>,
    /// 行隐藏状态，按索引展开。越界视为 false（未隐藏）。
    pub row_hidden: Vec<bool>,
    /// 列分组大纲层级（outline level），按索引展开。越界视为 0（无分组）。
    /// 仅存数值，不做折叠渲染逻辑——outline 折叠交互是前端 UI 职责。
    pub col_outline_level: Vec<u8>,
    /// 行分组大纲层级（outline level），按索引展开。越界视为 0（无分组）。
    pub row_outline_level: Vec<u8>,
    /// 迷你图分组列表（xlsx x14:sparklineGroups / ODF calcext:sparkline-groups）
    #[serde(default)]
    pub sparkline_groups: Vec<SparklineGroup>,
    /// 列级默认样式（xlsx `<col style="N">`，解析期已从 cellXfs 下标展开为
    /// 完整 CellFormat），按索引展开，越界或 None 视为无列级默认样式。
    /// 用于该列没有任何单元格级 cell_formats 覆盖时的默认背景色/字体渲染
    /// （典型场景：列级默认样式覆盖整列，但该列没有任何实际单元格数据）。
    #[serde(default)]
    pub col_style: Vec<Option<CellFormat>>,
}

/// 迷你图类型。xlsx `type` 属性省略时默认为 Line。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum SparklineType { Line, Column, Stacked }

/// 空单元格显示方式。对应 xlsx displayEmptyCellsAs 属性。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum DisplayEmptyCellsAs { Span, Gap, Zero }

/// 坐标轴最大/最小值计算方式。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum SparklineAxisType { Individual, Group, Custom }

/// 一组共享属性的迷你图集合，对应 x14:sparklineGroup / calcext:sparkline-group。
/// 颜色统一用 "#RRGGBB"（与项目内其他颜色字段风格一致），None 表示使用渲染层默认色。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SparklineGroup {
    pub sparkline_type: SparklineType,
    pub manual_max: Option<f64>,
    pub manual_min: Option<f64>,
    pub line_weight: f32,
    pub date_axis: bool,
    pub display_empty_cells_as: DisplayEmptyCellsAs,
    pub markers: bool,
    pub high: bool,
    pub low: bool,
    pub first: bool,
    pub last: bool,
    pub negative: bool,
    pub display_x_axis: bool,
    pub display_hidden: bool,
    pub min_axis_type: SparklineAxisType,
    pub max_axis_type: SparklineAxisType,
    pub right_to_left: bool,

    pub color_series:   Option<String>,
    pub color_negative: Option<String>,
    pub color_axis:     Option<String>,
    pub color_markers:  Option<String>,
    pub color_first:    Option<String>,
    pub color_last:     Option<String>,
    pub color_high:     Option<String>,
    pub color_low:      Option<String>,

    pub sparklines: Vec<Sparkline>,
}

impl Default for SparklineGroup {
    fn default() -> Self {
        SparklineGroup {
            sparkline_type: SparklineType::Line,
            manual_max: None,
            manual_min: None,
            line_weight: 0.75,
            date_axis: false,
            display_empty_cells_as: DisplayEmptyCellsAs::Gap,
            markers: false,
            high: false,
            low: false,
            first: false,
            last: false,
            negative: false,
            display_x_axis: false,
            display_hidden: false,
            min_axis_type: SparklineAxisType::Individual,
            max_axis_type: SparklineAxisType::Individual,
            right_to_left: false,
            color_series: None,
            color_negative: None,
            color_axis: None,
            color_markers: None,
            color_first: None,
            color_last: None,
            color_high: None,
            color_low: None,
            sparklines: Vec::new(),
        }
    }
}

/// 单个迷你图实例：画在哪个单元格，数据源 range 是什么。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Sparkline {
    /// (row, col)，画在这个单元格里，0-based
    pub cell: (u32, u32),
    /// 数据源，支持多段 range（通常只有1段）
    pub input_range: Vec<CellRange>,
}

/// 稀疏网格存储，参考 MDDS 思路
/// 按列存储，每列是一个稀疏 Block 列表
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SparseGrid {
    pub cols: std::collections::HashMap<u32, SparseColumn>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SparseColumn {
    /// 每个 Block 覆盖连续的行区段
    pub blocks: SmallVec<[CellBlock; 4]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellBlock {
    pub start_row: u32,
    pub values:    SmallVec<[CellValue; 8]>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum CellValue {
    #[default]
    Empty,
    Number(f64),
    Text(String),
    /// ROOT-078：同一单元格内多个格式不同的文本片段（对应OOXML
    /// sharedStrings.xml `<si>`内多个`<r>`run的场景，如"前半句删除线+
    /// 后半句正常"）。只有`<si>`真正含≥2个`<r>`时才产生这个变体，单run
    /// 或纯文本`<si>`仍然走普通`Text(String)`，不影响绝大多数单元格的
    /// 现有处理路径。本次实现范围：仅支持`strikethrough`这一个run级
    /// 布尔属性（对齐实测用例`strike-through.xlsx`的实际需求），
    /// bold/italic/underline/color留空/false占位，后续有真实用例需要
    /// 时再扩展，不在本次一次性做满。
    RichText(Vec<TextRun>),
    /// 公式单元格：source 是公式原文（如 "=A1+A2"），cached 是上次求值
    /// 结果（求值前为 Box::new(CellValue::Empty)）。
    /// v104：参照 LOK ScFormulaCell（pCode + aResult 分离存储）的标准做法，
    /// 拆分前该字段是单一 String，重算结果写回会直接覆盖掉公式原文，
    /// 导致编辑器再也看不到公式、导出文件里公式标签丢失。
    Formula { source: String, cached: Box<CellValue> },
    Bool(bool),
    Error(CellError),
}

impl CellValue {
    /// 单元格"当前展示值"的文本形式（脚本 get_cell、list 数据验证候选/校验共用，保证一致）：
    /// 公式已求值返回计算结果，尚未求值（cached 仍是 Empty）退化返回公式原文。
    pub fn to_display_string(&self) -> String {
        match self {
            CellValue::Empty => String::new(),
            CellValue::Number(n) => n.to_string(),
            CellValue::Text(s) => s.clone(),
            CellValue::RichText(runs) => runs.iter().map(|r| r.text.as_str()).collect(),
            CellValue::Formula { source, cached } => match cached.as_ref() {
                CellValue::Empty => source.clone(),
                other => other.to_display_string(),
            },
            CellValue::Bool(b) => b.to_string(),
            CellValue::Error(e) => format!("{:?}", e),
        }
    }
}

/// 手写 PartialEq/Eq/Hash：不能靠 derive，因为 `Number(f64)` 内部是
/// f64，f64 本身不满足 Eq（NaN != NaN）。用于 delete_duplicates
/// 等需要"内容完全相同"判定的场景（v157新增），语义为严格位模式相等，
/// 不做近似容差比较——判重操作要求结果确定可复现，容差比较反而会引入
/// "浮点误差导致判重结果不稳定"的新问题。
///
/// Formula 变体按 `cached`（当前计算结果）判等，不比较 `source`
/// 公式原文——与 `cell_value_to_string`（get_cell 脚本读取路径）的
/// "读到的是当前展示值"语义保持一致，两个公式格只要算出的值相同就
/// 视为重复，即使公式原文不同（如 `=A1+A2` 和 `=SUM(A1:A2)`）。
impl PartialEq for CellValue {
    fn eq(&self, other: &Self) -> bool {
        use CellValue::*;
        match (self, other) {
            (Empty, Empty) => true,
            (Number(a), Number(b)) => a.to_bits() == b.to_bits(),
            (Text(a), Text(b)) => a == b,
            (RichText(a), RichText(b)) => a == b,
            (Formula { cached: a, .. }, Formula { cached: b, .. }) => a == b,
            (Bool(a), Bool(b)) => a == b,
            (Error(a), Error(b)) => a == b,
            // 不同 variant 之间（如 Formula 的 cached 恰好是 Number(5.0)
            // 与直接的 Number(5.0)）故意不视为相等：判重发生在同一列内，
            // 同一列的单元格通常要么全是公式要么全是字面量，跨 variant
            // 比较的实际收益低，且会让这段逻辑显著复杂化（需要先"展开"
            // 成统一的比较视图），本次不做。
            _ => false,
        }
    }
}

impl Eq for CellValue {}

impl std::hash::Hash for CellValue {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        use CellValue::*;
        std::mem::discriminant(self).hash(state);
        match self {
            Empty => {}
            Number(n) => n.to_bits().hash(state),
            Text(s) => s.hash(state),
            RichText(runs) => runs.hash(state),
            Formula { cached, .. } => cached.hash(state),
            Bool(b) => b.hash(state),
            Error(e) => e.hash(state),
        }
    }
}

/// ROOT-078：单元格富文本run——一段连续文字+该片段专属的格式覆盖。
/// 与`roe-writer::layout::ResolvedRunStyle`是完全独立的并行设计（writer
/// 那边有段落样式表/字符样式表多层继承链，服务于docx场景；这里是xlsx
/// 单元格场景，无样式表、无继承，run格式直接来自sharedStrings.xml的
/// `<rPr>`，更简单），故意不复用/不跨crate共享类型，避免为了适配对方的
/// 继承链概念引入不必要的耦合。
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TextRun {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CellError {
    Div0, Na, Name, Null, Num, Ref, Value, CircularRef,
}

fn default_image_mime() -> String {
    "image/png".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FloatingImage {
    /// 稳定标识,0 为未分配;与同 sheet 的图表共用 id 空间,由 ensure_object_ids 分配。
    #[serde(default)]
    pub id: u64,
    /// Base64 编码的图片数据
    pub data_base64: String,
    /// MIME 类型（如 "image/png"、"image/jpeg"）。DEVROOT-002修复：此前
    /// 完全未存储图片格式信息，导致ODF写出的 office:binary-data 缺少
    /// mime-type 属性——ODF标准要求消费者靠此属性判断如何解码base64，
    /// 缺失时真实LibreOffice会静默降级为占位符渲染（不报错，肉眼看是
    /// 一个纯色矩形，不是应有的图片内容）。默认兜底"image/png"以保持
    /// 旧数据/旧测试的serde反序列化兼容。
    #[serde(default = "default_image_mime")]
    pub mime: String,
    /// 宽度（pt）
    pub width_pt:  f32,
    /// 高度（pt）
    pub height_pt: f32,
    /// 左边距（pt，相对于内容区）
    pub x_pt: f32,
    /// 上边距（pt，相对于内容区）
    pub y_pt: f32,
    /// 旋转角度（度，顺时针）。ROOT-051修复：此前<xdr:pic><xdr:spPr><a:xfrm rot="...">
    /// 完全未解析，图片旋转恒为0。单位从OOXML的60000分之一度换算而来，与
    /// FloatingShape.rot保持同一语义（度数，非原始EMU角度单位）。
    #[serde(default)]
    pub rot: f32,
    /// 是否隐藏（对应<xdr:cNvPr hidden="1">）。ROOT-051修复：此前完全未解析
    /// cNvPr的hidden属性，导致标记为hidden的图片仍被渲染。true时渲染层应
    /// 跳过绘制。
    #[serde(default)]
    pub hidden: bool,
}

/// ROOT-053/054修复：xlsx浮动图形/文本框（<xdr:sp>）。此前drawing解析
/// 只处理<xdr:pic>（图片），<xdr:sp>完全没有对应分支，导致所有形状/
/// 文本框类型的浮动对象在解析层就被跳过（不是渲染层问题）。
/// 复用roe-model公共几何/填充/文字类型，画法照抄roe-writer::tile.rs
/// 的render_shape_frame（docx侧已验证过的成熟实现）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FloatingShape {
    /// 稳定标识,0 为未分配;与同 sheet 的图表共用 id 空间,由 ensure_object_ids 分配。
    #[serde(default)]
    pub id: u64,
    /// 左边距（pt，相对于内容区），来自xdr:spPr/a:xfrm/a:off的x（EMU→pt）
    pub x_pt: f32,
    /// 上边距（pt）
    pub y_pt: f32,
    /// 宽度（pt），来自a:xfrm/a:ext的cx
    pub width_pt: f32,
    /// 高度（pt），来自a:xfrm/a:ext的cy
    pub height_pt: f32,
    /// 旋转角度（普通角度制，0-360）。OOXML原始值是1/60000度单位，
    /// 解析层负责换算（rot / 60000.0），与pptx.rs raw_to_shape同一换算方式。
    #[serde(default)]
    pub rot: f32,
    /// <a:xfrm flipV="1">上下镜像
    #[serde(default)]
    pub flip_v: bool,
    /// 形状几何（prstGeom求值结果或基础几何变体）
    pub geometry: ShapeGeometry,
    /// 填充样式（a:solidFill等），None表示无填充
    #[serde(default)]
    pub fill: Option<FillStyle>,
    /// 边框(线条)样式,None 表示无边框。宽度单位 pt。
    #[serde(default)]
    pub stroke: Option<crate::common::StrokeStyle>,
    /// 形状内文字（a:txBody），复用roe_model::writer::Paragraph（通用段落类型）
    #[serde(default)]
    pub text_content: Vec<Paragraph>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CellRange {
    pub sheet: String,
    pub start: (u32, u32),
    pub end:   (u32, u32),
}

impl SparseGrid {
    /// 按坐标读取单元格值。坐标不存在任何数据时返回 CellValue::Empty
    /// （这是稀疏存储的正常情况，不是错误）。
    ///
    /// 实现说明：不假设 blocks 内部已排序或无重叠，线性扫描该列所有
    /// block 找第一个覆盖 row 的。这是保守实现，优先保证正确性；
    /// 如果未来发现是性能热点（比如大范围批量读取），可以改成基于
    /// 排序的二分查找，接口签名不需要变。
    pub fn get(&self, row: u32, col: u32) -> CellValue {
        let Some(column) = self.cols.get(&col) else {
            return CellValue::Empty;
        };
        for block in &column.blocks {
            let len = block.values.len() as u32;
            if row >= block.start_row && row < block.start_row + len {
                let offset = (row - block.start_row) as usize;
                return block.values[offset].clone();
            }
        }
        CellValue::Empty
    }

    /// ROOT tdf164895: 整列/整行开放式引用（如 $A:$A、$1:$1）支持所需的
    /// "已用区域边界"查询。0-based，语义等价于 Excel/LibreOffice 的
    /// "UsedRange"概念里的最大行/列（不含空白尾部）。
    ///
    /// 返回 None 表示整个 sheet 没有任何单元格数据（完全空白），调用方
    /// 需要能优雅处理这种情况（不能假设一定有至少一行/一列）。
    ///
    /// 实现说明：线性扫描所有列的所有 block，找 (start_row + len - 1) 的
    /// 最大值作为已用最大行；已用最大列直接是 cols 这个 HashMap 的最大 key
    /// （前提：set() 目前的实现里，一个 col 只要被写过哪怕是 Empty，也会
    /// 在 cols 里占一个 key——如果未来 set() 改成会主动清理纯 Empty 列，
    /// 这里的"列存在即已用"假设需要重新核实）。
    /// 成本 O(总 block 数)，跟 sheet 里已有单元格数量成正比，不是 O(总面积)，
    /// 对稀疏 sheet 友好；仅在整列/整行开放引用求值时调用一次，不是热路径。
    /// 已存储的单元格总数(各列各 Block 的 values 长度之和)。
    /// O(块数),不遍历单元格;用于大文档保护的规模估算。
    pub fn cell_count(&self) -> usize {
        self.cols
            .values()
            .map(|c| c.blocks.iter().map(|b| b.values.len()).sum::<usize>())
            .sum()
    }

    pub fn used_bounds(&self) -> Option<(u32, u32)> {
        let max_col = self.cols.keys().copied().max()?;
        let mut max_row: Option<u32> = None;
        for column in self.cols.values() {
            for block in &column.blocks {
                let len = block.values.len() as u32;
                if len == 0 {
                    continue;
                }
                let block_max_row = block.start_row + len - 1;
                max_row = Some(max_row.map_or(block_max_row, |m| m.max(block_max_row)));
            }
        }
        // cols 里存在 key 但该列所有 block 都是空 SmallVec（理论上 set() 不会
        // 产生这种情况，防御性处理）：退化为 max_row = 0，仍保证返回 Some。
        Some((max_row.unwrap_or(0), max_col))
    }

    /// 按坐标写入单元格值。
    /// - 坐标落在已有 block 覆盖范围内：原地替换该值。
    /// - 坐标不在任何 block 范围内：新建一个长度为 1 的 block 并追加到
    ///   该列（不尝试与相邻 block 合并——合并是可选的空间优化，正确性
    ///   不依赖它，先实现能跑通的版本）。
    pub fn set(&mut self, row: u32, col: u32, value: CellValue) {
        let column = self.cols.entry(col).or_default();
        for block in column.blocks.iter_mut() {
            let len = block.values.len() as u32;
            if row >= block.start_row && row < block.start_row + len {
                let offset = (row - block.start_row) as usize;
                block.values[offset] = value;
                return;
            }
        }
        // 没找到覆盖该行的 block，新建一个单值 block。
        column.blocks.push(CellBlock {
            start_row: row,
            values: smallvec::smallvec![value],
        });
    }
}

/// 单元格格式
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CellFormat {
    pub bold:         Option<bool>,
    pub italic:       Option<bool>,
    pub underline:    Option<bool>,
    pub font_size:    Option<f32>,
    pub font_family:  Option<String>,
    pub fg_color:     Option<String>,   // "#RRGGBB"
    pub bg_color:     Option<String>,   // "#RRGGBB"
    pub align_h:      Option<String>,   // "left"/"center"/"right"
    pub align_v:      Option<String>,   // "top"/"middle"/"bottom"
    pub wrap_text:    Option<bool>,
    pub num_format:   Option<String>,   // 数字格式字符串，如 "0.00"
    pub borders:      Option<CellBorders>,
}

/// 单元格四边边框（对角线暂不支持）
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CellBorders {
    pub left:   Option<BorderLine>,
    pub right:  Option<BorderLine>,
    pub top:    Option<BorderLine>,
    pub bottom: Option<BorderLine>,
}

/// 单边边框线。style 对齐 excelize styleBorders 命名：
/// "thin"/"medium"/"dashed"/"dotted"/"thick"/"double"/"hair"/
/// "mediumDashed"/"dashDot"/"mediumDashDot"/"dashDotDot"/
/// "mediumDashDotDot"/"slantDashDot"
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BorderLine {
    pub style: String,
    pub color: Option<String>,   // "#RRGGBB"
}

/// 条件格式规则（v2：支持普通规则/数据条/色阶/图标集四种类型）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConditionalFormatRule {
    pub range:      CellRange,
    pub kind:       ConditionalFormatKind,
    /// 对应 xlsx cfRule 的 stopIfTrue 属性：命中此规则后是否停止应用后续规则
    pub stop_if_true: bool,
}

/// 条件格式规则种类。参考 OOXML CT_CfRule schema 以及 excelize
/// ConditionalFormatOptions 的字段设计（数据条/色阶/图标集三种可视化类型
/// 结构差异很大，用 enum 区分，不能用一套平铺字段表达）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConditionalFormatKind {
    /// 普通规则：条件匹配 → 应用固定格式（原有唯一支持的类型，字段不变）
    CellFormat {
        condition: String,   // "gt:0" / "between:1:10" / "text_contains:foo"
        format: CellFormat,
    },
    /// 数据条：cell 内绘制渐变长条，长度随数值大小在 min~max 间线性插值
    DataBar {
        min: CfValue,
        max: CfValue,
        bar_color: String,            // "#RRGGBB"
        border_color: Option<String>, // None 表示无边框
        direction: BarDirection,
        bar_only: bool,   // true: 只画条形，不显示数字
        solid_fill: bool, // true: 实心条；false: 渐变条
        // ROOT-066(v128): 以下三个字段只有x14扩展dataBar（Excel 2010+）
        // 才提供，legacy dataBar(2007兼容格式)没有这些信息。
        // negative_fill_color: 负值条形颜色，None时退化为bar_color的反色/灰色兜底
        negative_fill_color: Option<String>,
        // axis_color: 0轴分隔线颜色（仅当数据同时含正负值时绘制该竖线）
        axis_color: Option<String>,
        // axis_position: "automatic"(数据含负值时画在中点)/"middle"(强制画在
        // 单元格正中)/"none"(不画轴线，正负都从单元格左边开始画，Excel较少见)
        axis_position: String,
    },
    /// 色阶：固定 2 档或 3 档连续渐变着色。mid 为 None 时是 2 色，
    /// 否则是 3 色（(阈值类型, 阈值, 颜色) 三元组）。
    ColorScale {
        min_type: String,
        min_value: Option<String>,
        min_color: String,
        mid: Option<(String, String, String)>,
        max_type: String,
        max_value: Option<String>,
        max_color: String,
    },
    /// 图标集：按阈值分档显示图标（箭头/交通灯等）
    IconSet {
        icon_style: String,        // Excel 预设名称，如 "3TrafficLights1"
        thresholds: Vec<CfValue>,  // 阈值点，数量与 icon_style 对应的档位数一致
        percent: bool,             // 阈值是否按百分比解释
        reverse: bool,             // 反转图标顺序
        icons_only: bool,          // true: 只显示图标，不显示数字
        // ROOT-075(v132后续会话): OOXML x14扩展"自定义混合图标集"
        // (custom="1" + 逐档<x14:cfIcon iconSet="X" iconId="N"/>)携带的
        // 每档图标来源覆盖信息。None = 非custom规则，所有档统一沿用
        // icon_style（legacy <iconSet>、fods写回等路径均为None）。
        // Some(vec) 时，vec[i] 对应第i档：
        //   None = 该档沿用外层icon_style的第i个图标（无override）
        //   Some((set, id)) = 该档改用set预设的第id个图标；
        //     set=="NoIcons" 表示该档不绘制任何图标
        tier_overrides: Option<Vec<Option<(String, u8)>>>,
    },
}

/// 条件格式阈值点（对应 OOXML 的 cfvo：Conditional Format Value Object）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CfValue {
    pub kind: String,   // "min"/"max"/"num"/"percent"/"percentile"/"formula"
    pub val:  String,   // 数值或公式字符串
    pub gte:  bool,     // true: 阈值比较用"大于等于"；false: 严格大于。默认 true。
}

/// 数据条方向
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BarDirection {
    LeftToRight,
    RightToLeft,
    Auto,   // 根据数据正负自动判断方向（Excel 默认行为）
}

/// 数据验证规则
/// kind 沿用 xlsx dataValidation type 语义："none"/"custom"/"date"/"decimal"/
/// "list"/"textLength"/"time"/"whole"（对应 excelize DataValidationType）。
/// operator 沿用 xlsx dataValidation operator 语义："between"/"equal"/
/// "greaterThan"/"greaterThanOrEqual"/"lessThan"/"lessThanOrEqual"/
/// "notBetween"/"notEqual"，仅当 kind 非 list/custom 时有意义，默认空串
/// 表示不适用（如 list 类型不需要 operator）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataValidationRule {
    pub range:       CellRange,
    pub kind:        String,   // "list" / "whole" / "decimal" / "date" / "time" / "textLength" / "custom" / "none"
    #[serde(default)]
    pub operator:    String,   // "between" / "equal" / "greaterThan" / ... 空串表示不适用
    pub formula1:    String,
    pub formula2:    Option<String>,
    #[serde(default)]
    pub allow_blank: bool,
    pub show_error:  bool,
    pub error_msg:   Option<String>,
}


/// 自动筛选设置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoFilter {
    pub header_row: u32,
    pub header_col: u32,
    // ROOT-005修复(v126)：xlsx <autoFilter ref="A1:K1"/> 的真实终止列，
    // 替代此前渲染层"扫描连续空单元格猜测边界"的错误做法——该猜测会被
    // 合并单元格内的Empty非主cell误判为表格结束，导致后续列的按钮漏画。
    pub end_col: u32,
    pub col_filters: std::collections::HashMap<u32, Vec<String>>,  // col_offset → 允许值列表
    // ROOT-005修复(v126)：xlsx <filterColumn showButton="0"/> 表示该列不显示
    // 筛选下拉按钮（如autofilterShowButton样本专门测试这个属性）。
    // 存col_offset集合，渲染时跳过这些列的按钮绘制。
    pub hidden_buttons: std::collections::HashSet<u32>,
}

/// xlsx表格(Table/ListObject，如 `<table displayName="MyTable1" ref="A1:C5">`)元数据。
/// ROOT-011修复(v127)：结构化引用(`MyTable1[[#This Row],[列名]]`)求值的数据基础，
/// 参考LOK的TableModel(sc/source/filter/oox/tablebuffer.hxx)设计——range含表头，
/// header_row_count/totals_row_count默认语义与LOK/OOXML规范一致(默认header=1行,totals=0行)。
/// 求值本身依赖formualizer-eval 0.7+的Table trait/define_table API，不在此结构体内实现。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Table {
    /// 表名，即OOXML的displayName（结构化引用公式里用的名字，如"MyTable1"）
    pub name: String,
    /// 整表范围（0-based，含表头行/汇总行，即OOXML的ref属性）
    pub range: CellRange,
    /// 是否含表头行（OOXML headerRowCount，1=有，0=无；ROE只支持0/1，>1按1处理并忽略多出部分）
    pub has_header: bool,
    /// 是否含汇总行（OOXML totalsRowCount，>0视为true）
    pub has_totals: bool,
    /// 列名列表，顺序与range左到右的列一一对应
    pub columns: Vec<String>,
    /// ROOT-050修复：表格自带的自动筛选（OOXML <table>内嵌的<autoFilter ref="..."/>子标签）。
    /// 与sheet级别的独立<autoFilter>（roe_model::calc::Sheet::auto_filter字段）是两种不同的
    /// XML位置：普通sheet的筛选按钮写在<worksheet>下，而结构化表格(Table/ListObject)的筛选
    /// 按钮写在xl/tables/tableN.xml的<table>元素内部，此前parse_autofilter_from_xml只扫描
    /// sheet XML文本，完全没有解析这条路径，导致所有"表格自带autoFilter"场景筛选按钮不渲染
    /// （real-world样本统计：375个样本中34个table带显式<autoFilter>子标签，此前全部漏解析）。
    /// 目前只解析ref属性（真实样本里table内的autoFilter均只带ref，无filterColumn/filters
    /// 子标签），故col_filters/hidden_buttons恒为空，仅end_col/header_row/header_col有效。
    pub auto_filter: Option<AutoFilter>,
    /// 表格样式名(OOXML tableStyleInfo@name,如 "TableStyleMedium2"),无名为 None
    #[serde(default)]
    pub style_name: Option<String>,
    /// tableStyleInfo@showFirstColumn
    #[serde(default)]
    pub show_first_column: bool,
    /// tableStyleInfo@showLastColumn
    #[serde(default)]
    pub show_last_column: bool,
    /// tableStyleInfo@showRowStripes(OOXML 省略时视为 true)
    #[serde(default = "table_default_true")]
    pub show_row_stripes: bool,
    /// tableStyleInfo@showColumnStripes
    #[serde(default)]
    pub show_column_stripes: bool,
    /// 主题调色板,按 Excel theme 索引顺序 0=lt1 1=dk1 2=lt2 3=dk2 4~9=accent1~6;
    /// 空表示未知(渲染层回退 Office 默认色)。表格样式的 theme+tint 颜色据此还原。
    #[serde(default)]
    pub theme_palette: Vec<[u8; 3]>,
}

fn table_default_true() -> bool { true }

/// 合并单元格区域
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergedCell {
    pub r1: u32,
    pub c1: u32,
    pub r2: u32,
    pub c2: u32,
}


// ════════════════════════════════════════════════════════════
// 透视表元数据
// ════════════════════════════════════════════════════════════

/// 透视表字段定义（字段名 + 当前筛选值）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PivotFieldDef {
    /// 字段名（value_fields 场景下是展示标签，如 "Sum of C"；
    /// filter/row/col 场景下就是数据源列名本身）
    pub name: String,
    /// 当前筛选值（空表示全部，多值 OR）
    pub selected_values: Vec<String>,
    /// 该值字段自身的聚合方式（对应 xlsx dataField subtotal 属性）。
    /// None 表示未指定，调用方应回退到 PivotMeta.agg 或默认 "sum"。
    /// 仅对 value_fields 有意义，filter/row/col 字段不使用此字段。
    pub agg: Option<String>,
    /// 真实数据源列名（仅 value_fields 场景使用）。
    /// 与 name（展示标签，如 "Sum of C"）解耦，避免依赖标签文本反推列名。
    /// None 表示未提供，调用方应回退到从 name 解析（不推荐，见 ROOT-057）。
    pub source_field: Option<String>,
}

impl PivotFieldDef {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), selected_values: vec![], agg: None, source_field: None }
    }
}

/// 透视表表头节点（树状领域模型）
///
/// 行/列表头均表示为树：根节点 children 是外层字段各取值节点，
/// 每个取值节点的 children 是内层字段各取值节点，叶节点 children 为空。
/// cached_span 由 compute_spans() 自底向上填充，渲染层直接使用。
/// 透视表表头显示模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[derive(Default)]
pub enum PivotLayoutMode {
    /// 大纲模式：有子节点的父节点独占一行/列，子节点各自另起一行/列排列
    #[default]
    Outline,
    /// 表格模式：父节点与其第一个子节点共享同一行/列（视觉居中于子节点跨度）
    Tabular,
}


#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PivotHeaderNode {
    /// 该节点显示的值（如 "红"、"大"、"Sales Sum"；根节点为空字符串）
    pub value: String,
    /// 子节点列表（内层字段的取值）
    pub children: Vec<PivotHeaderNode>,
    /// 是否为小计/总计节点（对应 OOXML t="default"/t="grand"）
    pub is_subtotal: bool,
    /// 该节点在物理网格上的跨度（列表头=横向列数，行表头=纵向行数）
    /// 表格模式（Tabular）专用：非叶节点 = 子节点 span 之和，父子共享空间。
    /// 由 compute_spans() 计算后填充。
    pub cached_span: u32,
    /// 大纲模式（Outline）专用：非叶节点 = 1（自身独占标题行/列）+ 子节点
    /// outline_span 之和；叶节点 = 1。由 compute_outline_spans() 计算后填充。
    /// 与 cached_span 并存，互不影响，由渲染层按当前显示模式选择使用哪个字段。
    pub outline_span: u32,
}

impl PivotHeaderNode {
    pub fn new(value: impl Into<String>) -> Self {
        Self { value: value.into(), ..Default::default() }
    }

    /// 递归计算 cached_span：叶节点=leaf_multiplier，非叶节点=子节点 span 之和
    /// （表格模式 Tabular 用：父子共享同一段空间）
    /// leaf_multiplier：列表头多值字段场景下，每个叶节点要横向平铺 n_values 个
    /// 物理列（1个value时传1，等价于原逻辑）。row_tree 恒传1。
    ///
    /// TODO（小计功能，设计已完成，见 ROE_Subtotal_Design_v1.md，暂缓实现）：
    /// 小计不引入新树结构，复用已有的 is_subtotal 字段作为展开时的额外插入项。
    /// 接入后此函数需要在非叶子节点的子节点 span 之和基础上，为小计行/列本身
    /// 额外 +leaf_multiplier（小计也要按 n_values 横向平铺，复用同一套机制）。
    pub fn compute_spans(&mut self, leaf_multiplier: u32) {
        if self.children.is_empty() {
            self.cached_span = leaf_multiplier.max(1);
        } else {
            for child in &mut self.children {
                child.compute_spans(leaf_multiplier);
            }
            self.cached_span = self.children.iter().map(|c| c.cached_span).sum();
        }
    }

    /// 递归计算 outline_span：叶节点=1，非叶节点=1（自身标题行）+ 子节点
    /// outline_span 之和（大纲模式 Outline 用：父节点独占一行/列，
    /// 子节点各自另起一行/列排在其下方/右方，不与父节点共享空间）
    pub fn compute_outline_spans(&mut self) {
        if self.children.is_empty() {
            self.outline_span = 1;
        } else {
            for child in &mut self.children {
                child.compute_outline_spans();
            }
            self.outline_span = 1 + self.children.iter().map(|c| c.outline_span).sum::<u32>();
        }
    }

    /// 树的深度（叶节点=0）
    pub fn depth(&self) -> u32 {
        if self.children.is_empty() {
            0
        } else {
            1 + self.children.iter().map(|c| c.depth()).max().unwrap_or(0)
        }
    }
}

/// 透视表元数据（树状领域模型）
///
/// col_tree / row_tree 的根节点是虚拟根（value=""），
/// children 是外层字段各取值节点。
/// 渲染层通过遍历树 + cached_span 计算每个表头格子的坐标和跨度。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PivotMeta {
    /// 数据源 sheet 名称
    pub source_sheet: String,
    /// 聚合方式："sum" | "mean" | "count" | "min" | "max"
    pub agg: String,
    /// 筛选字段（page fields），顺序 = 用户选择顺序
    pub filter_fields: Vec<PivotFieldDef>,
    /// 行字段名列表（外→内顺序）
    pub row_field_names: Vec<String>,
    /// 列字段名列表（外→内顺序）
    pub col_field_names: Vec<String>,
    /// 值字段列表
    pub value_fields: Vec<PivotFieldDef>,
    /// 列表头树（虚拟根）
    pub col_tree: PivotHeaderNode,
    /// 行表头树（虚拟根）
    pub row_tree: PivotHeaderNode,
    /// 数据区起始行（筛选区行数 + 列表头层数，相对透视表自身，不含 base_row 偏移）
    pub data_row_start: u32,
    /// 数据区起始列（行字段列数，相对透视表自身，不含 base_col 偏移）
    pub data_col_start: u32,
    /// 透视表在 sheet 中的绝对起始行（对应 xlsx location ref 左上角，0-indexed）
    pub base_row: u32,
    /// 透视表在 sheet 中的绝对起始列（对应 xlsx location ref 左上角，0-indexed）
    pub base_col: u32,
    /// 透视表在 sheet 中的绝对结束行（对应 xlsx location ref 右下角，0-indexed，含）
    pub last_row: u32,
    /// 透视表在 sheet 中的绝对结束列（对应 xlsx location ref 右下角，0-indexed，含）
    pub last_col: u32,
    /// 表头显示模式（大纲/表格），默认 Outline
    pub layout_mode: PivotLayoutMode,
    /// 是否显示行方向汇总（对应xlsx colGrandTotals属性，默认true）
    pub row_grand_totals: bool,
    /// 是否显示列方向汇总（对应xlsx rowGrandTotals属性，默认true）
    pub col_grand_totals: bool,
    /// ROOT-060: 值字段(dataFields)是否展开在行方向（对应xlsx
    /// pivotTableDefinition的dataOnRows属性，默认false即展开在列方向）。
    /// xlsx标准语义：dataOnRows="1"时，rowFields里会有一个field x="-2"
    /// 占位（代表"值"这一维），真正的分组字段全部转移到colFields；
    /// dataOnRows缺省或="0"时反之。当前row_field_indices/col_field_indices
    /// 解析阶段会跳过x=-2（不当作普通分组字段处理，避免污染真实字段列表），
    /// 这个标志位是唯一保留"值字段方向"信息的地方，供渲染层判断表头如何铺排。
    /// 仅由xlsx解析路径（roe-parser/src/ooxml/pivot.rs）填充；
    /// pivot_sheet_into_workbook_v2（用户手动配置透视表路径）不涉及此概念，
    /// 构造PivotMeta时使用默认值false，不受影响。
    pub data_on_rows: bool,
    /// 数据区聚合结果表（现场计算，不依赖xlsx静态快照）
    /// key: (行叶子完整路径, 列叶子完整路径, 第几个值字段) -> 聚合值
    /// 小计/总计节点的路径中，对应层级用空字符串占位（约定，后续实现聚合函数时统一遵守）
    pub data: PivotDataMatrix,
    /// 物化进 sheet.cells 的真实包围盒 (r1, c1, r2, c2),含端点,绝对坐标。
    /// 由 pivot_layout::materialize_pivot 写入;None 表示尚未物化。
    /// 刷新时据此清旧区域;渲染遮罩以它为准(last_row/last_col 口径不含 base 偏移,不可靠)。
    #[serde(default)]
    pub cells_rect: Option<(u32, u32, u32, u32)>,
    /// 数据源范围(如 "A1:H201"),对应 cacheSource/worksheetSource 的 ref。
    /// None 表示未知(用户手动创建的透视表),写出 xlsx 时由源 sheet 已用范围推算。
    #[serde(default)]
    pub source_ref: Option<String>,
    /// 透视表样式名(如 "PivotStyleLight20"),对应 pivotTableStyleInfo 的 name。
    /// None 时写出默认 "PivotStyleLight16"。
    #[serde(default)]
    pub style_name: Option<String>,
    /// 透视表名称(如 "数据透视表1"),对应 pivotTableDefinition 的 name。
    /// None 时写出 "PivotTable{n}"。
    #[serde(default)]
    pub name: Option<String>,
    /// 按字段名的筛选(行/列/页字段通用):字段名 -> 允许值列表,空或缺省=不限制。
    /// 页字段的 selected_values 与此同步,仅用于显示。刷新时据此还原 PivotConfig.filters。
    #[serde(default)]
    pub field_filters: std::collections::HashMap<String, Vec<String>>,
    /// 行/列字段排序:(字段名, 是否升序);缺省=默认升序。
    #[serde(default)]
    pub sort: Vec<(String, bool)>,
}

/// 透视表数据区数值矩阵（方案1：现场聚合计算）
/// 用逻辑路径而非物理坐标做key，天然不受表头折叠/排序影响。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PivotDataMatrix {
    pub cells: std::collections::HashMap<(Vec<String>, Vec<String>, usize), f64>,
}

impl PivotDataMatrix {
    pub fn get(&self, row_path: &[String], col_path: &[String], value_idx: usize) -> Option<f64> {
        self.cells.get(&(row_path.to_vec(), col_path.to_vec(), value_idx)).copied()
    }

    pub fn insert(&mut self, row_path: Vec<String>, col_path: Vec<String>, value_idx: usize, value: f64) {
        self.cells.insert((row_path, col_path, value_idx), value);
    }
}

impl PivotMeta {
    /// 计算 col_tree 和 row_tree 的所有 cached_span（表格模式）和
    /// outline_span（大纲模式），渲染前调用一次。
    pub fn compute_spans(&mut self) {
        let n_values = self.value_fields.len().max(1) as u32;
        // ROOT-060: dataOnRows=true 时，值字段(dataFields)纵向展开在数据区自身的行，
        // 不依附 col_tree 或 row_tree 的叶子横向/纵向扩展，故两棵树的 leaf_multiplier
        // 都应为 1（不因 value_fields 数量而横向/纵向膨胀）。
        // dataOnRows=false（原有逻辑不变）：值字段横向依附 col_tree 叶子扩展。
        if self.data_on_rows {
            self.col_tree.compute_spans(1);
            self.row_tree.compute_spans(1);
        } else {
            self.col_tree.compute_spans(n_values);
            self.row_tree.compute_spans(1);
        }
        self.col_tree.compute_outline_spans();
        self.row_tree.compute_outline_spans();
    }

    /// 列表头物理行数
    pub fn col_header_rows(&self) -> u32 {
        let value_layer = if self.value_fields.len() > 1 { 1 } else { 0 };
        self.col_field_names.len() as u32 + value_layer
    }

    /// 行表头物理列数
    pub fn row_header_cols(&self) -> u32 {
        self.row_field_names.len() as u32
    }
}

// ══════════ 数据验证：校验逻辑（从 roe-core 渲染层下沉到 model，供渲染与脚本层共用） ══════════
// 校验逻辑参考 univer `base-data-validator.ts` 骨架抄写，kind 覆盖 whole/decimal/textLength；
// list/date/time/custom/none 不做校验（视为合法）。

/// 把 CellValue 转成用于数值比较的 f64，取不到返回 None
fn cell_value_as_f64(val: &CellValue) -> Option<f64> {
    match val {
        CellValue::Number(n) => Some(*n),
        CellValue::Formula { cached, .. } => cell_value_as_f64(cached),
        CellValue::Text(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

/// 把 CellValue 转成参与"文本长度"比较的字符数（textLength 用）
fn cell_value_as_text_len(val: &CellValue) -> Option<f64> {
    match val {
        CellValue::Text(s) => Some(s.chars().count() as f64),
        CellValue::Number(n) => Some(format_number_plain(*n).chars().count() as f64),
        CellValue::Formula { cached, .. } => cell_value_as_text_len(cached),
        _ => None,
    }
}

/// 数字转字符串时不带多余小数位（避免 "5" 变成 "5.0" 影响长度计数）
fn format_number_plain(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{}", n as i64)
    } else {
        format!("{}", n)
    }
}

/// 判断单元格是否为空（allow_blank 语义用）
fn is_empty_value(val: &CellValue) -> bool {
    matches!(val, CellValue::Empty) || matches!(val, CellValue::Text(s) if s.is_empty())
}

/// 8 种 operator 的数值比较，formula1/formula2 已解析为 f64
fn compare_by_operator(operator: &str, value: f64, formula1: Option<f64>, formula2: Option<f64>) -> bool {
    // formula 解析失败（None）时，跟 univer 行为一致：视为无法判断，放行（返回 true）
    let f1 = match formula1 {
        Some(v) => v,
        None => return true,
    };
    match operator {
        "between" => match formula2 {
            Some(f2) => {
                let (lo, hi) = if f1 <= f2 { (f1, f2) } else { (f2, f1) };
                value >= lo && value <= hi
            }
            None => true,
        },
        "notBetween" => match formula2 {
            Some(f2) => {
                let (lo, hi) = if f1 <= f2 { (f1, f2) } else { (f2, f1) };
                value < lo || value > hi
            }
            None => true,
        },
        "equal" => (value - f1).abs() < 1e-10,
        "notEqual" => (value - f1).abs() >= 1e-10,
        "greaterThan" => value > f1,
        "greaterThanOrEqual" => value >= f1,
        "lessThan" => value < f1,
        "lessThanOrEqual" => value <= f1,
        // 未知 operator：不判断，放行
        _ => true,
    }
}

/// 解析范围引用，返回 (可选 sheet 名, (起始行, 起始列), (结束行, 结束列))，全部 0-based，
/// 起止已规范化（start <= end）。支持两种来源的写法：
///   Excel/xlsx：`A1` / `A1:B5` / `$A$1:$A$5` / `=A1:B5`（可带 `=` 前缀）
///               `Sheet2!A1:A5` / `'my sheet'!A1:A5`（`''` 转义单引号）
///   ODF：`[$Sheet1.$A$1:.$A$3]`（区间第二段前导点）/ `[$Const.$A$2:$Const.$A$6]` /
///        `$Sheet1.$A$1`（外层方括号可省，sheet 名前的 `$` 可省，sheet 名可带 `'...'`）
/// 含 `!` 一律按 Excel 写法解析（sheet 名里允许出现 `.`）；不含 `!` 才尝试 ODF 写法。
/// 起止两端 sheet 不同（三维引用）、解析失败均返回 None。列最多 3 个字母（XFD），行从 1 起。
pub fn parse_a1_range(s: &str) -> Option<(Option<String>, (u32, u32), (u32, u32))> {
    let s = s.trim();
    let s = s.strip_prefix('=').unwrap_or(s).trim();
    if s.is_empty() {
        return None;
    }
    let parse_cell = |t: &str| -> Option<(u32, u32)> {
        let t = t.trim().trim_start_matches('$');
        let letters: String = t.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
        if letters.is_empty() || letters.len() > 3 {
            return None;
        }
        let rest = t[letters.len()..].trim_start_matches('$');
        if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let row1: u32 = rest.parse().ok()?;
        if row1 == 0 {
            return None;
        }
        let mut col: u32 = 0;
        for ch in letters.chars() {
            col = col * 26 + (ch.to_ascii_uppercase() as u32 - 'A' as u32 + 1);
        }
        Some((row1 - 1, col - 1))
    };
    let unquote = |name: &str| -> String {
        let name = name.trim();
        if name.len() >= 2 && name.starts_with('\'') && name.ends_with('\'') {
            name[1..name.len() - 1].replace("''", "'")
        } else {
            name.to_string()
        }
    };
    let norm = |sheet: Option<String>, a: (u32, u32), b: (u32, u32)| {
        Some((sheet, (a.0.min(b.0), a.1.min(b.1)), (a.0.max(b.0), a.1.max(b.1))))
    };

    if s.contains('!') {
        // Excel 写法：取最后一个 '!' 之前为 sheet 名
        let p = s.rfind('!')?;
        let name = unquote(&s[..p]);
        if name.is_empty() {
            return None;
        }
        let body = &s[p + 1..];
        let mut parts = body.split(':');
        let a = parse_cell(parts.next()?)?;
        let b = match parts.next() {
            Some(t) => parse_cell(t)?,
            None => a,
        };
        if parts.next().is_some() {
            return None;
        }
        return norm(Some(name), a, b);
    }

    // 无 '!'：Excel 同 sheet 写法或 ODF 写法。先剥 ODF 外层方括号
    let body = if s.starts_with('[') && s.ends_with(']') { &s[1..s.len() - 1] } else { s };
    // 每一段：可选 "[$]Sheet." 前缀 + 单元格。以最后一个 '.' 分割（单元格里不会有 '.'）
    let split_seg = |seg: &str| -> Option<(Option<String>, (u32, u32))> {
        let seg = seg.trim();
        match seg.rfind('.') {
            Some(p) => {
                let name_part = seg[..p].trim();
                let name_part = name_part.strip_prefix('$').unwrap_or(name_part);
                let cell = parse_cell(&seg[p + 1..])?;
                let name = if name_part.is_empty() { None } else { Some(unquote(name_part)) };
                Some((name, cell))
            }
            None => Some((None, parse_cell(seg)?)),
        }
    };
    let mut parts = body.split(':');
    let (sa, a) = split_seg(parts.next()?)?;
    let (sb, b) = match parts.next() {
        Some(t) => split_seg(t)?,
        None => (None, a),
    };
    if parts.next().is_some() {
        return None;
    }
    let sheet = match (sa, sb) {
        (Some(x), Some(y)) => {
            if x.eq_ignore_ascii_case(&y) { Some(x) } else { return None } // 三维引用不支持
        }
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    };
    norm(sheet, a, b)
}

impl DataValidationRule {
    /// list 规则的下拉候选项（内核唯一实现，get_list_candidates 与写入校验共用）。
    /// - formula1 是字面量：走 `list_literal_items`
    /// - formula1 是 A1 范围引用：读引用区域单元格文本（`to_display_string`），空格跳过，
    ///   多列按行优先展开，跨 sheet 在 `sheets` 里按名字匹配（忽略大小写），最多 1000 格；
    ///   无 sheet 前缀时取 `own`
    /// 不是 list / 无候选 / 引用解析失败 / 引用的 sheet 不存在 → None。
    /// `sheets` 传空切片表示"没有其它 sheet 可查"（渲染层只拿得到当前 sheet），
    /// 此时跨 sheet 引用返回 None。
    pub fn list_candidates(&self, sheets: &[Sheet], own: &Sheet) -> Option<Vec<String>> {
        const MAX_CELLS: usize = 1000;
        if self.kind != "list" {
            return None;
        }
        if let Some(lit) = self.list_literal_items() {
            return Some(lit);
        }
        let (sheet_name, (r0, c0), (r1, c1)) = parse_a1_range(&self.formula1)?;
        let src: &Sheet = match sheet_name {
            Some(n) => {
                if own.name.eq_ignore_ascii_case(&n) {
                    own
                } else {
                    sheets.iter().find(|s| s.name.eq_ignore_ascii_case(&n))?
                }
            }
            None => own,
        };
        let mut out = Vec::new();
        'outer: for r in r0..=r1 {
            for c in c0..=c1 {
                if out.len() >= MAX_CELLS {
                    break 'outer;
                }
                let t = src.cells.get(r, c).to_display_string();
                let t = t.trim();
                if !t.is_empty() {
                    out.push(t.to_string());
                }
            }
        }
        if out.is_empty() { None } else { Some(out) }
    }

    /// list 规则下，值是否在候选集内：空值按 allow_blank；非空值转展示文本 trim 后与候选
    /// 逐项做文本相等比较（区分大小写，对齐 Excel 字面量 list 的行为）。
    pub fn list_accepts(&self, val: &CellValue, candidates: &[String]) -> bool {
        if is_empty_value(val) {
            return self.allow_blank;
        }
        let t = val.to_display_string();
        let t = t.trim();
        candidates.iter().any(|c| c == t)
    }

    /// 解析 list 类型规则的"字面量候选项"。
    ///
    /// 返回值：
    /// - `Some(items)`：formula1 是字面量列表，items 已 trim 且去空
    /// - `None`：kind 不是 list / formula1 为空 / formula1 是单元格范围引用
    ///   （范围引用需要读单元格，由调用方处理）
    ///
    /// 支持的字面量格式（三种来源并存）：
    /// - 前端弹窗：`苹果,香蕉,橙子`（无引号，逗号分隔）
    /// - xlsx 导入：`"苹果,香蕉,橙子"`（一对外层引号，内部逗号分隔）
    /// - ODF 导入：`"苹果";"香蕉";"橙子"`（每项一对引号，分号分隔）
    /// 引号内的 `""` 视为转义的单个 `"`。
    pub fn list_literal_items(&self) -> Option<Vec<String>> {
        if self.kind != "list" {
            return None;
        }
        let f = self.formula1.trim();
        if f.is_empty() || f.starts_with('=') {
            return None;
        }

        // 字面量的引号结构：整串以 " 开头并以 " 结尾（xlsx "a,b" / ODF "a";"b"）。
        // 引号只是嵌在表达式里（如 ROW(INDIRECT("1:10")) ）不算字面量，交给 looks_like_reference。
        let quoted_literal = f.starts_with('"') && f.ends_with('"');
        let items: Vec<String> = if quoted_literal {
            // 提取所有 "..." 片段（"" 转义为 "）
            let chars: Vec<char> = f.chars().collect();
            let mut segs: Vec<String> = Vec::new();
            let mut i = 0;
            while i < chars.len() {
                if chars[i] == '"' {
                    let mut cur = String::new();
                    i += 1;
                    while i < chars.len() {
                        if chars[i] == '"' {
                            if i + 1 < chars.len() && chars[i + 1] == '"' {
                                cur.push('"');
                                i += 2;
                                continue;
                            }
                            break;
                        }
                        cur.push(chars[i]);
                        i += 1;
                    }
                    segs.push(cur);
                }
                i += 1;
            }
            if segs.len() == 1 {
                // xlsx 风格：一对外层引号，内部按逗号拆
                segs[0].split(',').map(|t| t.trim().to_string()).collect()
            } else {
                // ODF 风格：每对引号一项
                segs.into_iter().map(|t| t.trim().to_string()).collect()
            }
        } else if Self::looks_like_reference(f) {
            return None;
        } else {
            f.split(',').map(|t| t.trim().to_string()).collect()
        };

        let items: Vec<String> = items.into_iter().filter(|t| !t.is_empty()).collect();
        if items.is_empty() { None } else { Some(items) }
    }

    /// formula1（无引号）是否是"引用/表达式"而不是字面量列表。命中则 list_literal_items 返回 None，
    /// 交给引用解析；解析不了（命名区域、OFFSET(...) 等）候选取不到 → 无下拉、写入放行(安全降级)。
    /// - 含 `[ ] ( ) ! # $` 之一：引用或表达式（前端字面量几乎不会含这些字符）
    /// - 是合法范围引用（Excel 或 ODF 写法）
    /// - 命名引用形态：以字母/下划线开头，仅含 ASCII 字母数字/下划线/点（如 `nydim`）。
    ///   已知代价：纯英文单项字面量（如只填 `apple`）也会命中，得不到下拉；单项 list 无实际意义，接受。
    ///   含 CJK 的单项（如 `苹果`）不命中，仍按字面量处理。
    fn looks_like_reference(s: &str) -> bool {
        if s.chars().any(|c| matches!(c, '[' | ']' | '(' | ')' | '!' | '#' | '$')) {
            return true;
        }
        if parse_a1_range(s).is_some() {
            return true;
        }
        let mut chars = s.chars();
        match chars.next() {
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
            }
            _ => false,
        }
    }

    /// 规则范围是否覆盖单元格 (row, col)。用 min/max 规范化，不依赖 start<=end。
    pub fn covers(&self, row: u32, col: u32) -> bool {
        let (r0, r1) = (self.range.start.0.min(self.range.end.0), self.range.start.0.max(self.range.end.0));
        let (c0, c1) = (self.range.start.1.min(self.range.end.1), self.range.start.1.max(self.range.end.1));
        row >= r0 && row <= r1 && col >= c0 && col <= c1
    }

    /// 校验单个单元格的值是否满足给定的 DataValidationRule。
    /// 只覆盖 kind = whole/decimal/textLength；其余 kind 一律返回 true（不标红）。
    /// 返回 true 表示合法（或不在校验覆盖范围内），false 表示违反规则。
    pub fn is_valid(&self, val: &CellValue) -> bool {
        let rule = self;
        if is_empty_value(val) {
            return rule.allow_blank;
        }

        let compare_value = match rule.kind.as_str() {
            "whole" => match cell_value_as_f64(val) {
                Some(n) if n.fract() == 0.0 => n,
                _ => return false, // 不是整数（或不是数字）：whole 类型直接判违规
            },
            "decimal" => match cell_value_as_f64(val) {
                Some(n) => n,
                None => return false, // 不是数字：decimal 类型直接判违规
            },
            "textLength" => match cell_value_as_text_len(val) {
                Some(len) => len,
                None => return true, // 取不到长度（非文本非数字）：不判断
            },
            // list：候选集可能来自单元格范围引用，本函数拿不到工作簿，因此这里放行；
            // list 校验走带候选集的 list_candidates + list_accepts（validate_cell_input / 渲染层）。
            // date/time/custom/none：不校验，视为合法
            _ => return true,
        };

        if rule.operator.is_empty() {
            return true;
        }

        let formula1 = rule.formula1.trim().parse::<f64>().ok();
        let formula2 = rule.formula2.as_deref().and_then(|s| s.trim().parse::<f64>().ok());
        compare_by_operator(&rule.operator, compare_value, formula1, formula2)
    }

}


#[cfg(test)]
mod default_row_height_tests {
    use super::*;

    #[test]
    fn effective_default_falls_back_to_15() {
        let sh = Sheet::default();
        assert_eq!(sh.effective_default_row_height(), 15.0);
    }

    #[test]
    fn effective_default_uses_sheet_value() {
        let mut sh = Sheet::default();
        sh.default_row_height = 14.25;
        assert_eq!(sh.effective_default_row_height(), 14.25);
    }
}

#[cfg(test)]
mod pivot_meta_new_fields_tests {
    use super::*;

    #[test]
    fn new_fields_default_empty() {
        let m = PivotMeta::default();
        assert!(m.field_filters.is_empty() && m.sort.is_empty());
    }
}
