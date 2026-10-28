use serde::{Deserialize, Serialize};

/// 通用颜色
// WROOT-100：补PartialEq/Eq（全字段是u8，比较无精度顾虑），供
// roe-writer::frame::BorderStyle derive(PartialEq)使用。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

/// 通用矩形（单位：EMU 或 pt，由上下文决定）
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// PPTX-7：形状变换（位置+尺寸+旋转+翻转），单位：pt（解析阶段已从 EMU 转换）。
/// 对应 OOXML <a:xfrm rot="..." flipH="..." flipV="..."><a:off/><a:ext/></a:xfrm>。
/// rotation/flip_h/flip_v 为 None 表示该维度未设置或来源格式（如当前 ODF）暂不提供此信息，
/// 不代表"确认为0/false"，调用方（渲染层）应按 unwrap_or 默认值处理。
/// rotation 单位为普通角度（度），非 OOXML 原始的 1/60000 度编码，解析层负责换算。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Transform {
    pub offset: (f32, f32),
    pub extent: (f32, f32),
    pub rotation: Option<f32>,
    pub flip_h: Option<bool>,
    pub flip_v: Option<bool>,
    pub shear_x: Option<f32>,
    pub shear_y: Option<f32>,
}

impl Transform {
    /// 从一个无旋转/翻转信息的矩形构造 Transform，供 ODF/生成方向/测试数据等
    /// 暂不提供旋转信息的调用点平滑过渡使用。
    pub fn from_rect(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self {
            offset: (x, y),
            extent: (w, h),
            rotation: None,
            flip_h: None,
            flip_v: None,
            shear_x: None,
            shear_y: None,
        }
    }

    #[inline] pub fn x(&self) -> f32 { self.offset.0 }
    #[inline] pub fn y(&self) -> f32 { self.offset.1 }
    #[inline] pub fn w(&self) -> f32 { self.extent.0 }
    #[inline] pub fn h(&self) -> f32 { self.extent.1 }
}

/// 样式引用
#[derive(Default, Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StyleRef(pub String);

/// 求值后的预设几何路径指令（绝对坐标，单位与 Shape.position 同一
/// 坐标系）。对应 OOXML `a:pathLst` 求值结果，由
/// `preset_geometry::eval_preset_shape` 产出，渲染层消费。
/// PPTX-11。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum PathCommand {
    MoveTo { x: f32, y: f32 },
    LineTo { x: f32, y: f32 },
    CubicTo { x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32 },
    /// 二次贝塞尔，对应 `a:quadBezTo`（单控制点）。PPTX-11批量脚本处理
    /// ellipseRibbon/ellipseRibbon2/smileyFace/swooshArrow/teardrop 五个
    /// 形状时新增，此前187个形状中未出现quadBezTo用例。
    QuadTo { x1: f32, y1: f32, x: f32, y: f32 },
    /// 圆心+半径+角度参数化表达（非笔画游标增量式），渲染层负责转换
    /// 成实际绘制调用。见 preset_geometry.rs 模块文档关于 ArcTo 当前
    /// 局限性（游标位置未追踪）的说明。
    ArcTo { cx: f32, cy: f32, rx: f32, ry: f32, start_deg: f32, sweep_deg: f32 },
    Close,
}

/// 求值后的形状连接点（本地坐标系，未经 Shape.position/Transform 映射为
/// 全局文档坐标）。对应 OOXML `a:cxnLst` 求值结果，由
/// `preset_geometry::get_connection_points` 产出。`idx` 即该连接点在
/// `a:cxnLst`（及 OnlyOffice `f.AddCnx` 调用）里的声明顺序，是 OOXML
/// `<a:cxn idx="N">` 引用该连接点时用的编号，语义上无隐式排序规律
/// （不同形状 idx=0 可能对应完全不同的方位），每个形状需按其自身定义
/// 解读，见 preset_geometry.rs 模块内 2026-09 的核实记录。
/// `angle_deg` 单位是 1/60000 度（与源数据 `ang` 属性同单位，未转换成
/// 常规角度制，消费方按需自行换算）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ConnectionPoint {
    pub idx: usize,
    pub angle_60000th_deg: f32,
    pub x: f32,
    pub y: f32,
}

/// PPTX-11 followup（连接点查询前置重构，roe-ext_progress_v181）：
/// `ShapeGeometry::PresetPath` 携带的完整数据，取代此前裸 `Vec<PathCommand>`。
///
/// 背景：此前 `PresetPath(Vec<PathCommand>)` 只存求值后的绝对坐标路径，
/// 不保留"这是哪个预设形状（prst名字）""用了什么adj覆盖值"。这两个信息
/// 在解析阶段（如 roe-parser/pptx.rs::raw_to_shape）其实是拿到过的
/// （RawShape.geometry: Option<String> + RawShape.adj_overrides），但
/// 求值出 Vec<PathCommand> 之后就地丢弃，没有传递到最终 Shape，导致
/// 运行时给定一个已解析的 Shape，无法倒查出它对应哪个
/// `preset_geometry::PresetShapeFormula`、用了什么 adj 值，也就无法
/// 重新调用 `preset_geometry::get_connection_points()` 求出连接点全局
/// 坐标（这是 `roe_impress_get_connection_points` C-ABI 的前置阻塞点，
/// 已实测确认：roe-model::impress::Shape 求值前完全不含此信息）。
///
/// 参照 LibreOffice `EnhancedCustomShape2d`（`~/openlib/LOK/core/
/// include/svx/EnhancedCustomShape2d.hxx`）交叉验证：该类同时持有
/// `m_seqAdjustmentValues`（adj覆盖值）和 `m_seqGluePoints`（连接点，
/// 未求值的表达式对）两份"声明"数据，按需调用 `GetPoint()` 现场求值，
/// 而不是只保留求值后的死路径。"预设几何 = 声明(name+adj) + 缓存渲染
/// 路径"的双份数据设计是工业级 Office 实现的常规做法，不是过度设计。
///
/// 已知技术债（不阻塞本次重构，留给未来处理 ODF 侧预设几何时一并核实，
/// 完整讨论见 roe-ext_progress_v181.md）：LO 求值连接点时对 OOXML 来源
/// 与原生 ODF 来源的形状使用不同的坐标缩放参数（
/// `EnhancedCustomShape2d.cxx` 的 `GetPoint(pair, !m_bOOXMLShape, true)`，
/// 即 bScale 取决于形状来源）。ROE 当前 `preset_geometry::
/// get_connection_points` 对两种来源一视同仁，未做区分。目前 ROE 只从
/// OOXML（pptx/xlsx）解析预设形状产出 PresetPath，ODF 侧尚未接通这条
/// 链路，暂不构成实际 bug；但未来若 ODF 侧也开始产出 PresetPath，需要
/// 重新核实此处是否需要区分缩放逻辑，不要想当然认为两者一致。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetPathData {
    /// 已求值的绝对坐标路径命令（原 `PresetPath(Vec<PathCommand>)` 存的
    /// 就是这份数据，渲染层等只关心路径的调用点应继续只用这个字段）。
    pub commands: Vec<PathCommand>,
    /// OOXML prst 名字（如 "rect"/"heart"/"bentConnector3"），用于运行时
    /// 反查 `preset_geometry::PresetShapeFormula` 重新求值（如连接点
    /// 查询）。custGeom（自定义路径，非预设形状）场景没有真正的 prst
    /// 名字，此时为空字符串——custGeom 复用 PresetPath 变体只是为了共享
    /// 渲染逻辑，语义上不是"预设形状"，调用方若需要区分应检查此字段
    /// 是否为空，而不是假设非空。
    pub preset_name: String,
    /// `<a:avLst><a:gd name="adjN" fmla="val N"/>` 覆盖值。求值 `commands`
    /// 时已经用过一次，这里额外保留一份供未来重新求值使用（如连接点
    /// 查询需要同一套 adj_overrides 才能得到与渲染路径一致的坐标系）。
    /// 空 Vec 表示该形状未自定义任何 adj（用全部默认值求值），与
    /// custGeom 场景（同样是空 Vec，但含义是"不适用"而非"未覆盖"）在
    /// 数据形态上无法区分，如需精确区分请结合 `preset_name` 是否为空
    /// 判断。
    pub adj_overrides: Vec<(String, f64)>,
}

impl std::ops::Deref for PresetPathData {
    type Target = Vec<PathCommand>;
    fn deref(&self) -> &Vec<PathCommand> {
        &self.commands
    }
}

/// 形状几何类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ShapeGeometry {
    Rect, RoundRect { radius: f32 },
    Ellipse, Triangle, Line,
    /// PPTX-11: 已求值的预设几何形状路径（架构接入，第一期覆盖约30种
    /// 高频形状，其余仍落入 Custom 分支）。数据来源：解析层持有187种
    /// （目标）预设形状的公式模板（`preset_geometry::PresetShapeFormula`
    /// 静态表），读到具体 shape 的 prstGeom/avLst 后调用
    /// `preset_geometry::eval_preset_shape` 求值产出路径，连同 prst 名字
    /// /adj覆盖值一并打包进 `PresetPathData`（见其文档注释了解为什么
    /// 不能只存路径）。
    PresetPath(PresetPathData),
    Custom(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShapeStyle {
    pub fill:   Option<FillStyle>,
    pub stroke: Option<StrokeStyle>,
    pub shadow: Option<ShadowStyle>,
    /// PROOT-002: 对应 OOXML <a:ln><a:headEnd type="..."/></a:ln>，
    /// 线条起点箭头/装饰样式。None 表示未设置该标签（渲染层按"无箭头"
    /// 处理），与 ArrowType::None（显式 type="none"）语义相同，两者
    /// 目前不做区分。仅对 Connector 或普通 Line 类几何生效，普通填充
    /// 形状的 stroke 不含端点箭头，此字段留空即可。
    #[serde(default)]
    pub head_arrow: Option<ArrowType>,
    /// PROOT-002: 对应 OOXML <a:ln><a:tailEnd type="..."/></a:ln>，
    /// 线条终点箭头/装饰样式。语义同 head_arrow。
    #[serde(default)]
    pub tail_arrow: Option<ArrowType>,
}

/// PROOT-002: 线条端点装饰类型，对应 OOXML <a:headEnd/tailEnd type="...">。
/// 第一期只覆盖 tdf114845/connectors.pptx 等已验证样本里出现的类型
/// （arrow/triangle/none），OOXML 规范里还有 stealth/diamond/oval 等
/// 类型未覆盖，遇到未知字符串时解析层应 fallback 到 None（而非报错），
/// 渲染层对 None 不画任何端点装饰。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArrowType {
    /// type="arrow"：开放式箭头（两条斜线，不闭合）
    Arrow,
    /// type="triangle"：实心三角形箭头
    Triangle,
    /// type="oval"：实心圆形端点。此前 str_to_arrow_type 把所有未识别/
    /// 未显式处理的 type 值（含 "oval"）一律映射成 None（不画任何装饰），
    /// 导致连接线的圆点端点在ROE里完全消失（真实样本 candy.pptx slide11
    /// 的8处 headEnd type="oval" 复现此问题）。新增此变体，让 "oval" 走
    /// 独立分支而不是落进兜底 None。
    Oval,
    /// type="none" 或未识别的类型字符串，不画任何装饰
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FillStyle {
    Solid(Color),
    /// PPTX-12: 多色标渐变，替代原 `{start,end,angle}` 二色线性设计。
    /// 参照 LOK (`GradientFillProperties`) + OnlyOffice (`CGradFill`)
    /// 交叉验证的字段设计：色标列表 + 类型分支互斥（lin/path 二选一），
    /// 而非把所有可能字段揉进一个 struct。
    Gradient {
        /// 色标位置 0.0-1.0（原始 OOXML `pos` 属性精度为十万分之一，
        /// 解析层负责归一化，此处只存归一化后的比例）+ 颜色。
        /// 使用 Vec 而非旧版 start/end 两个固定字段，因为 OOXML
        /// `gsLst` 本身允许任意数量色标（常见2-3个，理论无上限）。
        stops: Vec<GradientStop>,
        kind: GradientKind,
        /// PPTX-12: 对应 OOXML `<a:gradFill rotWithShape="...">`。
        /// true（默认，两方参照实现同一默认值）：渐变角度随形状旋转
        /// 一并旋转；false：渐变角度保持独立，不随形状旋转变化。
        /// 第一期实现：解析层始终读取该值，渲染层第一期按 true 处理
        /// （即恒叠加形状旋转），false 场景留待后续按需支持，因为
        /// LOK/OO 均确认这是较少使用的边缘选项。
        rotate_with_shape: bool,
    },
    /// PROOT-005: 幻灯片/母版/布局背景图片填充，对应 OOXML
    /// `<p:bg><p:bgPr><a:blipFill><a:blip r:embed="..."/></a:blipFill></p:bgPr></p:bg>`。
    /// 复用现成的 `ImageData`（与 `ShapeContent::Image` 同款字段），不新增图片承载类型。
    /// 裁剪字段（crop_l/t/r/b）在背景图场景下恒为默认值0（OOXML `<p:bgPr>`
    /// 下的 `blipFill` 理论上也允许 `<a:srcRect>` 裁剪，但目前未发现真实样本
    /// 使用此写法，解析层暂不读取，是已知简化非遗漏）。
    Picture(ImageData),
    /// ROE-TILE-001: 平铺图片背景，对应 OOXML
    /// `<a:blipFill><a:blip.../><a:tile tx="..." ty="..." sx="..." sy="..." algn="tl"/></a:blipFill>`。
    /// 与 `Picture`（单图裁剪+拉伸铺满）是完全不同的绘制语义，故用独立
    /// 变体而非在 `ImageData` 里加 tile 相关字段（避免"裁剪参数"和
    /// "平铺参数"同时存在于同一结构造成无效组合，两者互不叠加）。
    /// 本次实现只覆盖 `algn="tl"`（左上角锚点，向右下方向铺满整个
    /// 填充区域），真实样本 tdf152070 / 交易所竞争的新范式.pptx 均为
    /// 此锚点。其余8种锚点（t/tr/l/ctr/r/bl/b/br，语义见 LOK
    /// fillproperties.cxx lclGetRectanglePoint）解析层遇到时如实记录
    /// 原始字符串（见下方 tile_align 字段），渲染层退化按 tl 处理，
    /// 是已知简化非遗漏，等有真实样本再补精确的9种锚点几何算法。
    /// `<a:srcRect>` 裁剪与 `<a:tile>` 平铺同时出现的场景，两个真实
    /// 样本均未见，本次不处理叠加（如出现则忽略裁剪只做平铺），同理
    /// 是已知简化。
    PictureTiled {
        /// 复用现成的 ImageData 承载原图二进制+mime；crop_l/t/r/b
        /// 在此场景恒为0（未见真实样本在 tile 模式下使用 srcRect 裁剪）。
        image: ImageData,
        /// 对应 `tx` 属性，第一块瓦片相对 algn 锚点的 x 偏移，单位 pt
        /// （原始 EMU，解析层已换算：pt = emu / 12700）。
        tile_offset_x: f32,
        /// 对应 `ty` 属性，单位 pt，换算方式同上。
        tile_offset_y: f32,
        /// 对应 `sx` 属性，单个瓦片相对原图的缩放比例，归一化 0.0-1.0+
        /// （原始 ST_Percentage 千分之一精度已换算：ratio = raw/100000）。
        /// 例如 0.6 表示瓦片宽度 = 原图宽度 * 60%。
        tile_scale_x: f32,
        /// 对应 `sy` 属性，同上，纵向缩放比例。
        tile_scale_y: f32,
        /// 对应 `algn` 属性原始字符串（"tl"/"t"/"tr"/"l"/"ctr"/"r"/
        /// "bl"/"b"/"br"），本次渲染层只实现 "tl"，其余字符串如实
        /// 保留供未来扩展，不在解析层就丢弃信息。
        tile_align: String,
    },
    None,
}

/// PPTX-12: 单个渐变色标。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GradientStop {
    /// 0.0-1.0，对应 OOXML `<a:gs pos="...">`（原始千分之一精度已在
    /// 解析层归一化，见 pptx.rs `gs` 元素处理注释）。
    pub pos: f32,
    pub color: Color,
}

/// PPTX-12: 渐变类型，对应 OOXML `gradFill` 内 `lin`/`path` 二选一。
/// 参照 LOK `moGradientPath`/`moShadeAngle` 的互斥设计与 OnlyOffice
/// `CGradFill.lin`/`CGradFill.path` 的字段命名。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum GradientKind {
    /// 线性渐变，对应 `<a:lin ang="..." scaled="...">`。
    /// `angle`: 换算后的最终角度（度，0-360），换算公式（叠加了
    /// flip 修正与形状自身旋转的复合运算，非单纯 EMU 角度÷60000）
    /// 见 pptx.rs `raw_to_shape` 处的详细注释，参照 LOK
    /// `fillproperties.cxx:554-566`。
    Linear { angle: f32 },
    /// 径向渐变，对应 `<a:path path="circle">`。中心点第一期固定为
    /// 形状几何中心（0.5, 0.5），不读取 `fillToRect` 算精确偏移中心
    /// ——LOK/OO 均确认这是更少见的自定义中心场景，第一期按最常见的
    /// "居中放射"简化处理，是已知简化非遗漏。
    Radial,
    /// 矩形渐变，对应 `<a:path path="rect">`（`path="shape"` 两方
    /// 参照实现均标注未真正实现，归并到 Rect 处理）。中心点同上，
    /// 第一期固定几何中心。
    Rect,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrokeStyle {
    pub color: Color,
    pub width: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowStyle {
    pub color:    Color,
    pub offset_x: f32,
    pub offset_y: f32,
    pub blur:     f32,
}

/// 图片二进制数据 + MIME 类型（Writer/Impress 共享）
///
/// PPTX-10: 新增 crop_l/t/r/b 四个裁剪字段，对应 OOXML `<a:srcRect>`
/// 的 l/t/r/b 属性，归一化为 0.0-1.0 浮点（原始 XML 属性是千分之一
/// 精度的百分比整数，ST_Percentage 规定 100000 = 100%，解析层负责
/// 换算，此处只存归一化后的比例）。默认 0.0 表示不裁剪，语义上不需要
/// Option 区分"未设置"与"设置为0"。
///
/// PROOT-008: 存入这四个字段的值不止来自 `<a:srcRect>`（正值 inset），
/// 也可能来自 `<a:stretch><a:fillRect t="-13000".../>`（背景图场景，
/// 全部为负值/零时表示"图片需要整体放大裁边"）——解析层用 LOK
/// fillproperties.cxx CropQuotientsFromFillRect 同款公式，把负值
/// fillRect 换算成等价的正值 inset 比例后存入同一套字段，渲染层无需
/// 区分来源。fillRect 含正值（真正的裁剪收窄）的场景本次未覆盖，
/// 遇到真实样本再扩展。
/// `#[serde(default)]` 保证旧序列化数据（不含这四个字段）反序列化兼容。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageData {
    pub data: Vec<u8>,
    pub mime: String,
    #[serde(default)]
    pub crop_l: f32,
    #[serde(default)]
    pub crop_t: f32,
    #[serde(default)]
    pub crop_r: f32,
    #[serde(default)]
    pub crop_b: f32,
}

/// 表格单元格内容：收窄自 writer::BodyElement，仅保留单元格内合法的三种
/// （段落/图片/嵌套表格）。不直接复用 BodyElement 是为了避免 common 依赖
/// writer::BodyElement 导致的循环依赖，也避免 Impress 被迫支持 Writer
/// 专属语义（如 Section 分节符）。解析层仍产出 BodyElement，通过
/// TryFrom<BodyElement> for CellContent 转换后再放入 TableCell.content。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CellContent {
    Paragraph(crate::writer::Paragraph),
    Image { image: ImageData, width_pt: f32, height_pt: f32 },
    Table(Box<Table>),
}

/// WROOT-101（表格浮动绕排）：<w:tblPr><w:tblpPr> 解析结果。docx原始单位
/// 是twips（1/20 pt），解析层已换算成pt存入本结构体。
///
/// 字段与OOXML标准（ECMA-376 §17.4.66 tblpPr）对应关系：
/// - vert_anchor: w:vertAnchor（"text"/"margin"/"page"，缺省按OOXML规范
///   默认值处理，docx.rs解析层负责默认值兜底逻辑，本结构体只存实际生效值）
/// - horz_anchor: w:horzAnchor，同上
/// - y_pt / x_pt: w:tblpY / w:tblpX（绝对偏移，pt）。None表示原始XML未指定
///   该属性（常见于只用XSpec相对定位、不用绝对坐标的场景）
/// - left_from_text_pt / right_from_text_pt: w:leftFromText /
///   w:rightFromText（表格与周围文字的间距，pt），用于taffy
///   place_floated_box的containing_block_insets参数
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableFloatPos {
    #[serde(default)]
    pub vert_anchor: Option<String>,
    #[serde(default)]
    pub horz_anchor: Option<String>,
    #[serde(default)]
    pub y_pt: Option<f32>,
    #[serde(default)]
    pub x_pt: Option<f32>,
    #[serde(default)]
    pub left_from_text_pt: f32,
    #[serde(default)]
    pub right_from_text_pt: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Table {
    pub rows: Vec<TableRow>,
    pub col_widths: Vec<f32>,
    /// PROOT-031: <a:tblPr tableStyleId="{GUID}"> 引用的表格样式ID。可能
    /// 是PowerPoint内置74种样式的GUID之一（不在文档自带tableStyles.xml
    /// 里），也可能是文档自带 <a:tblStyleLst> 里的自定义样式ID。渲染层
    /// 按 style_id 查内置样式表（roe_model::table_styles::
    /// resolve_predefined_table_style）得到 TableStyle 后，结合下面几个
    /// bool 开关和 cell 在表格中的位置，为没有自身显式颜色/边框的 cell
    /// 提供 fallback。None 表示无 tableStyleId（渲染层保留纯默认全网格
    /// 线行为，即此前的既有行为，不因本次改动产生回归）。
    #[serde(default)]
    pub style_id: Option<String>,
    /// tblPr firstRow="1"：是否对第一行套用 TableStyle.first_row part。
    #[serde(default)]
    pub first_row: bool,
    /// tblPr firstCol="1"：是否对第一列套用 TableStyle.first_col part。
    #[serde(default)]
    pub first_col: bool,
    /// tblPr lastRow="1"：是否对最后一行套用 TableStyle.last_row part。
    #[serde(default)]
    pub last_row: bool,
    /// tblPr lastCol="1"：是否对最后一列套用 TableStyle.last_col part。
    #[serde(default)]
    pub last_col: bool,
    /// tblPr bandRow="1"：是否对奇偶行套用 TableStyle.band1_h/band2_h。
    #[serde(default)]
    pub band_row: bool,
    /// tblPr bandCol="1"：是否对奇偶列套用 TableStyle.band1_v/band2_v。
    #[serde(default)]
    pub band_col: bool,
    /// 表格样式移植 Layer 1步骤B新增（DOCX专属，PPTX无对应字段）：
    /// <w:tblPr><w:tblBorders> 表格级默认边框颜色。docx没有PPTX那套
    /// TableStyle六边fallback链机制，tblBorders就是docx"没有cell自己
    /// 边框时"的唯一fallback来源。四边（top/left/bottom/right）合并
    /// 成一个值的简化策略同TableCell.border_color：优先取w:top，
    /// 四边不一致时是已知简化。insideH/insideV（内部网格线）不在此
    /// 字段覆盖范围，属已知限制（真实样本目前遇到的都是外框，内部线
    /// 常为none，后续如有需要单独扩展）。
    #[serde(default)]
    pub default_border_color: Option<Color>,
    /// 表格样式移植 Layer 1步骤B新增：<w:tblPr><w:tblBorders> 表格级
    /// 默认边框宽度（pt），来自 w:sz（八分之一磅，÷8.0换算，与EMU
    /// 单位的PPTX完全不同）。None表示原始XML未指定。
    #[serde(default)]
    pub default_border_width: Option<f32>,
    /// WROOT-100：<w:tblBorders><w:insideH> 内部横线（行与行之间）颜色/
    /// 宽度。None 表示原始XML未指定具体颜色宽度（不区分"没写这个标签"
    /// 和"w:val=none显式关闭"，两种情况在docx.rs里都不写入具体值，统一
    /// 保持None）。渲染层看到None时不会不画线，而是画一条固定灰色虚线
    /// 兜底（0xA6A6A6，1.0px，dash[4,3]，与roe-impress占位符虚线框
    /// draw_placeholder_dashed_rect视觉语言保持一致），用户已确认这个
    /// 设计决策（不追求跟MS365/LibreOffice"insideH=none就完全不画线"
    /// 的行为完全一致，我们的产品希望内部线永远可见、给用户视觉引导）。
    #[serde(default)]
    pub inside_h_color: Option<Color>,
    #[serde(default)]
    pub inside_h_width: Option<f32>,
    /// WROOT-100：<w:tblBorders><w:insideV> 内部竖线（列与列之间）颜色/
    /// 宽度。语义同 inside_h_color/width。
    #[serde(default)]
    pub inside_v_color: Option<Color>,
    #[serde(default)]
    pub inside_v_width: Option<f32>,
    /// WROOT-101（表格浮动绕排）：<w:tblPr><w:tblpPr> 表格浮动定位属性。
    /// None 表示原始XML无此标签（表格是普通流内块级元素，不浮动，渲染层
    /// 保持既有行为——从页面内容区左边界铺满宽度排版，零回归）。
    /// Some 时渲染层应将表格视为浮动对象，注册进 taffy::FloatContext
    /// （与WROOT-093图片浮动绕排共用同一套框架，见roe-writer/layout.rs
    /// resolve_table_border_side附近的measure_table_height函数注释）。
    #[serde(default)]
    pub float_pos: Option<TableFloatPos>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableRow {
    pub cells: Vec<TableCell>,
    /// PROOT-003 followup: 行高（单位pt，来自 OOXML <a:tr h="..."> EMU值，
    /// 解析层换算：pt = emu / 12700）。None表示原始XML未指定，渲染层应按
    /// 内容自动测量高度作为兜底。此前该字段完全缺失，渲染层固定用
    /// MIN_ROW_H=18pt起步+按内容行数估算，与原始XML指定的行高（常见值
    /// 远小于18pt，如n80340实测14.4pt）不符，导致渲染行高明显偏高。
    #[serde(default)]
    pub row_height: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableCell {
    pub content:   Vec<CellContent>,
    pub col_span:  u8,
    pub row_span:  u8,
    pub style:     Option<StyleRef>,
    /// PPTX-6: 标记本单元格是否为合并单元格的"延续格"（被起始格的 col_span/
    /// row_span 覆盖吞掉，渲染时应跳过绘制）。
    ///
    /// 背景（DOCX 与 PPTX 表格合并模型的结构性差异，roe-ext 调研已确认）：
    /// DOCX 的 w:gridSpan/w:vMerge 合并单元格在 XML 层面"物理收窄"——被合并
    /// 覆盖的格子在 <w:tr> 里根本不存在对应的 <w:tc> 元素，cells 数组本身
    /// 就短一截，因此 DOCX 不需要这个字段，此处恒为 false。
    /// PPTX 的 a:gridSpan/a:rowSpan（写在起始格）与 a:hMerge/a:vMerge（写
    /// 在延续格，均为 <a:tc> 自身的布尔属性）不同：延续格是物理存在的
    /// <a:tc> 元素，可能带独立的 txBody/tcPr（参照 LOK oox/source/
    /// drawingml/table/tablecellcontext.cxx：解析阶段对 hMerge/vMerge 只是
    /// 读取布尔标记，不清空、不跳过任何内容，两者一视同仁地走完整解析路径；
    /// 真正的"跳过"发生在渲染阶段）。为了不丢失延续格可能携带的信息、也不
    /// 让 PPTX 的 cells 数组与实际网格列数脱节，PPTX 解析层保留延续格本身
    /// 作为一个 TableCell，仅用此字段标记"渲染时跳过"。
    #[serde(default)]
    pub hidden: bool,
    /// PROOT-003: 单元格自身背景色，对应 `<a:tcPr><a:solidFill>`。
    /// 支持 `<a:srgbClr val="RRGGBB"/>` 直接RGB写法，以及 `<a:schemeClr>`
    /// 主题色引用（PROOT-003 followup4起支持，见 border_color 同批实现，
    /// 复用 parse_table 新增的 theme_colors 参数）。
    #[serde(default)]
    pub bg_color: Option<Color>,
    /// PROOT-003 followup4: 单元格边框颜色，对应 `<a:tcPr><a:lnL/lnR/lnT/lnB>`
    /// 内的 `<a:solidFill>`。简化处理：不区分四条边各自独立的颜色（真实样本
    /// 目前四边总是一致），用一个字段覆盖全部四条边；四边中若有不一致，取
    /// 解析到的最后一个为准（真实样本从未出现四边不同色，此简化暂不构成
    /// 已知问题）。同 bg_color 一样支持 srgbClr 与 schemeClr。
    #[serde(default)]
    pub border_color: Option<Color>,
    /// PROOT-003 followup4: 单元格边框宽度（单位pt，来自 `<a:lnL w="...">`
    /// 等EMU值换算）。同 border_color 简化：四边共用一个宽度值。None表示
    /// 原始XML未指定，渲染层参照LOK oox/source/drawingml/linepropertiescontext.cxx
    /// 缺省值 12700 EMU(=1.0pt) 兜底。
    #[serde(default)]
    pub border_width: Option<f32>,
    /// PROOT-003 followup4: 单元格四向内边距（单位pt，来自 `<a:tcPr marL/
    /// marR/marT/marB="...">` EMU值换算）。None表示原始XML未指定，渲染层
    /// 参照LOK oox/source/drawingml/table/tablecellcontext.cxx 缺省值兜底
    /// （marL/marR缺省91440EMU≈7.2pt，marT/marB缺省45720EMU≈3.6pt——注意
    /// 这与此前渲染层硬编码的CELL_PAD_X=4.0/CELL_PAD_Y=3.0并不一致，本次
    /// 一并修正为遵循LOK缺省值）。
    #[serde(default)]
    pub pad_left: Option<f32>,
    #[serde(default)]
    pub pad_right: Option<f32>,
    #[serde(default)]
    pub pad_top: Option<f32>,
    #[serde(default)]
    pub pad_bottom: Option<f32>,
}
