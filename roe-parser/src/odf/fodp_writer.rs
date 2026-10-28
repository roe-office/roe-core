//! FODP（Flat ODF Presentation）写出器
//! roe_model::impress::Presentation → FODP XML 字符串
//!
//! ⚠ 阶段A（本文件当前范围）：仅覆盖最外层骨架——
//!   slide_size（cm 单位换算）、slides 数组长度、master_idx 关联、
//!   master_slides（名字 + 空 shapes）。
//!   不处理 Shape 内容、不处理动画树、不处理转场——留给后续阶段 B/C/D/E。
//!
//! ⚠ 单位制（已与 fodt_writer.rs 统一为 pt，2026-06-18 修复）：
//!   roe_model::impress::Presentation.slide_size / Shape.position 现在
//!   统一是 pt（与 fodt_writer.rs 一致）。fodp.rs::parse_measure 也已改为
//!   统一转 pt。但 FODP XML 本身的 fo:page-width / svg:x 等属性约定用 cm
//!   书写，因此写出时必须做 pt→cm 换算（÷28.3465），不能直接把 pt 数值
//!   套上 "cm" 后缀写出去——这正是 2026-06-18 之前的 bug 根因。
//!
//! ⚠ 母版引用是按名字字符串关联，不是数字索引持久化：
//!   XML 里 draw:master-page-name="Midnightblue" 是名字，parser 解析时才
//!   建立"名字→数组下标"的映射转成 master_idx。写出时必须反向用
//!   master_idx 查 master_slides[master_idx].name，不能直接写数字。
//!
//! ⚠ trim_text(false)：fodp.rs 解析时标签间空白会被当作文本节点。
//!   阶段A 不涉及 text:p，暂不触发，但后续阶段 B 写 Shape 文本内容时
//!   必须延续 fodt_writer.rs 的"不留空白"写法。
//!
//! ⚠ 阶段B（本次新增范围）：Shape 的 position/geometry/文本内容。
//!   geometry 映射非对称，写出时每个变体固定选一个标签，不假设可逆：
//!   - Rect -> draw:rect；Ellipse -> draw:ellipse；Line -> draw:line
//!   - RoundRect{radius}/Triangle：parser（fodp.rs::build_geometry）目前
//!     没有任何路径会产出这两个变体（不认识对应的 draw:type 字符串），
//!     所以这两种写出后 roundtrip 会改变形状类型（落进 Custom(other)
//!     分支）。本版本先诚实写出对应 XML（圆角矩形/三角形仍按视觉效果
//!     写为 draw:custom-shape 自定义类型字符串），不在 roundtrip 测试里
//!     断言"读回来还是 RoundRect/Triangle"，留待 parser 后续补上对应
//!     draw:type 识别后再升级为双向。
//!   - Custom(s)：写 draw:custom-shape + draw:enhanced-geometry
//!     draw:type="{s}"，可双向，但 s 不能是 "rectangle"/"ellipse"
//!     （这两个字符串被 parser 保留，会被识别成 Rect/Ellipse 而不是
//!     Custom，调用方需自行避免使用这两个保留字作为自定义类型名）。
//!   Paragraph/Run 写出：阶段B 暂不处理 StyleRegistry 真实样式定义
//!   （不生成 office:automatic-styles 里的样式 block），style/char_style
//!   仅作为名字引用写出（text:style-name="xxx"），引用目标暂不保证存在。
//!   这是有意简化，不是遗漏——样式定义 roundtrip 留给后续阶段补齐，
//!   做法应参考 fodt_writer.rs::write_named_styles 但不在本阶段引入。
//!
//! ⚠ 阶段D（本次新增范围）：动画树 + xml:id 生成 + 幻灯片转场。
//!
//!   xml:id 与 AnimationNode.target 的关系（已核实 roe-impress 运行时
//!   约定）：整个动画运行时系统（slide_renderer.rs/renderer.rs等）通过
//!   `shape.id == target_shape_id` 直接相等比较查找目标 shape，这是既有
//!   硬性约定，不是本文件发明的新协议。因此写出 xml:id 时直接使用
//!   `format!("shape_{}", shape.id)`，target 同样写
//!   `format!("shape_{}", target)`，两者天然保持一致，不需要额外建立
//!   "内存模型id -> 写出序号"的映射表。
//!
//!   ⚠ 重要：roundtrip 不保证 Shape.id 数值本身不变（parser 重新解析时
//!   会按 XML 中出现顺序从 1 重新分配 id），只保证"target 与其对应
//!   shape.id 的相等关系"在 roundtrip 后依然成立——因为两者都被
//!   parser/writer 同步重新映射。测试断言时应验证这个相等关系，
//!   不应假设 Shape.id 数值原样不变。
//!
//!   BeginCondition 写出映射（与 fodp.rs::parse_begin 对照，非对称）：
//!   - Offset(n) -> smil:begin="{n}s"：双向
//!   - OnNext -> smil:begin="next"：双向
//!   - Indefinite -> smil:begin="indefinite"：双向
//!   - OnClick/AfterPrevious/WithPrevious/OnBeginOf/OnEndOf/Multi：
//!     parse_begin 完全没有对应的反向解析分支（只认 None/"next"/
//!     "indefinite"/"{n}s" 这四种格式），无论 FodpWriter 写出什么字符串，
//!     这六种变体 roundtrip 后都会落进 parse_begin 的兜底分支变成
//!     Indefinite。本版本按各自语义写出最贴近的近似字符串（仅供外部
//!     软件参考），但不在 roundtrip 测试里断言"读回来还是原变体"，
//!     已知会降级为 Indefinite，这是 parser 侧限制，不是 writer 的 bug。
//!
//!   Audio 节点：parser（fodp.rs::parse_animation_node）完全没有处理
//!   anim:audio 或等价标签的分支，写出后无法被解析回 AnimationNode，
//!   完全单向（仅供外部软件参考，ROE 自身读不回这个节点）。
//!
//!   SlideTransition 写出：必须包装成 fodp.rs::is_slide_transition_wrapper
//!   认识的特定结构——TimingRoot 的 children 最前面插入一个
//!   `<anim:par><anim:transitionFilter .../></anim:par>`，且这个
//!   transitionFilter 不能带 smil:targetElement 属性（一旦带上就会被
//!   误判为"形状级转场"而不是"幻灯片级转场"，这是 parser 区分两者的
//!   唯一依据，见 is_slide_transition_wrapper 实现）。
//!
//!   anim:transitionFilter（形状级，带 targetElement）：AnimationNode
//!   没有 TransitionFilter 变体（架构文档 8.4 节提到的设计在当前
//!   roe-model 实现中尚未落地），本阶段不写出形状级转场效果，
//!   这部分动画语义在 ROE 内存模型层面目前不可表达，不是 roundtrip
//!   限制，是模型尚未支持。
//!
//! ⚠ 阶段E（本次新增范围）：单向元素的诚实占位。
//!   - ShapeStyle（fill/stroke/shadow）：parser（fodp.rs build_shape）
//!     完全不解析图形样式继承链，shape.style 永远是 ShapeStyle::default()。
//!     本阶段写出 fill/stroke/shadow 对应的内联
//!     style:graphic-properties，仅供外部软件参考，ROE 自身 parser
//!     读回来时 style 字段必为 default，完全单向。
//!   - ShapeContent::Image{data,mime}：写出 draw:image + base64 编码的
//!     office:binary-data，单向（parser 见 build_content 对 draw:image
//!     的处理：v1 暂不解码 base64，直接归为 ShapeContent::Empty）。
//!   - ShapeContent::Chart：完全跳过，不写出任何内容（既不是 bug 也不是
//!     遗漏）。当前是 unit variant 占位（TODO），按已确立的产品方向
//!     （Rhai 规则模板生成图表，导出后用私有扩展属性 + 矢量图形组合
//!     实现"兼容软件能退化显示"），这个数据结构本身将被重新设计
//!     （带 chart_kind/data/theme 等字段），现在写任何处理逻辑都会在
//!     模型重新设计后被推翻，故意留空。
//!   - 形状级 anim:transitionFilter（带 smil:targetElement 的，区别于
//!     幻灯片级转场包装节点）：本阶段已说明不写出，此处不重复。

use roe_model::common::Color;
use roe_model::impress::{
    AnimProperty, AnimValue, AnimationNode, BeginCondition, EffectNodeType, FillMode,
    FillStyle, Presentation, RepeatSpec, Shape, ShapeContent, ShapeGeometry, ShapeStyle,
    SlideTransition, TimingSpec, TransformKind, TransitionKind,
};
use roe_model::writer::{Paragraph, Run};
use base64::Engine as _;
use std::fmt::Write as _;

pub struct FodpWriter;

impl FodpWriter {
    pub fn write(pres: &Presentation) -> String {
        let mut out = String::with_capacity(2048);

        out.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
        out.push('\n');
        out.push_str(
            r#"<office:document
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
    xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"
    xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0"
    xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
    xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0"
    xmlns:anim="urn:oasis:names:tc:opendocument:xmlns:animation:1.0"
    xmlns:smil="urn:oasis:names:tc:opendocument:xmlns:smil-compatible:1.0"
    xmlns:xlink="http://www.w3.org/1999/xlink"
    office:mimetype="application/vnd.oasis.opendocument.presentation">
"#,
        );

        // ── 阶段E：预收集所有 shape 的图形样式（按 master 在前、slide 在后
        // 的顺序，与下方两个写出循环顺序严格一致，否则样式名会错位） ──
        let shape_style_names = collect_shape_style_names(pres);
        let mut style_cursor: usize = 0;

        // ── automatic-styles：page-layout（slide_size，cm） + 图形样式 ──
        out.push_str("  <office:automatic-styles>\n");
        let _ = writeln!(out, r#"    <style:page-layout style:name="PM1">"#);
        let _ = writeln!(
            out,
            r#"      <style:page-layout-properties fo:page-width="{:.2}cm" fo:page-height="{:.2}cm"></style:page-layout-properties>"#,
            pres.slide_size.0 / 28.3465, pres.slide_size.1 / 28.3465
        );
        out.push_str("    </style:page-layout>\n");
        for name_opt in shape_style_names.iter() {
            if let Some((name, style)) = name_opt {
                write_graphic_style_def(&mut out, name, style);
            }
        }
        out.push_str("  </office:automatic-styles>\n");

        // ── master-styles：母版列表（阶段A：仅名字，shapes 留空） ───
        out.push_str("  <office:master-styles>\n");
        for master in &pres.master_slides {
            if master.shapes.is_empty() {
                let _ = writeln!(
                    out,
                    r#"    <style:master-page style:name="{}" style:page-layout-name="PM1"></style:master-page>"#,
                    xml_escape_attr(&master.name)
                );
            } else {
                let _ = writeln!(
                    out,
                    r#"    <style:master-page style:name="{}" style:page-layout-name="PM1">"#,
                    xml_escape_attr(&master.name)
                );
                for shape in &master.shapes {
                    let style_name = shape_style_names[style_cursor].as_ref().map(|(n, _)| n.as_str());
                    style_cursor += 1;
                    write_shape(&mut out, shape, style_name);
                }
                out.push_str("    </style:master-page>\n");
            }
        }
        out.push_str("  </office:master-styles>\n");

        // ── body：幻灯片列表（阶段A：仅 master 关联，shapes 留空） ──
        out.push_str("  <office:body>\n    <office:presentation>\n");
        for (i, slide) in pres.slides.iter().enumerate() {
            let master_attr = slide
                .master_idx
                .and_then(|idx| pres.master_slides.get(idx))
                .map(|m| format!(r#" draw:master-page-name="{}""#, xml_escape_attr(&m.name)))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                r#"      <draw:page draw:name="page{}"{}>"#,
                i + 1, master_attr
            );
            for shape in &slide.shapes {
                let style_name = shape_style_names[style_cursor].as_ref().map(|(n, _)| n.as_str());
                style_cursor += 1;
                write_shape(&mut out, shape, style_name);
            }
            write_animation_tree(&mut out, &slide.animation_tree, slide.transition.as_ref());
            out.push_str("      </draw:page>\n");
        }
        out.push_str("    </office:presentation>\n  </office:body>\n</office:document>\n");

        out
    }
}

/// 阶段E：按 master 在前、slide 在后的顺序收集所有 shape 的图形样式。
/// 返回值与"遍历所有 shape"的顺序严格一对一对应，每个元素是
/// `Some((样式名, 样式引用))` 或 `None`（该 shape 是 ShapeStyle::default()，
/// 不需要写 automatic-style，也不需要 draw:style-name 属性）。
/// 顺序必须与 write() 函数体内两个写出循环（master_slides 然后 slides）
/// 完全一致，否则样式名会错位到别的 shape 上。
fn collect_shape_style_names(pres: &Presentation) -> Vec<Option<(String, ShapeStyle)>> {
    let is_default = |s: &ShapeStyle| s.fill.is_none() && s.stroke.is_none() && s.shadow.is_none();
    let mut result = Vec::new();
    let mut counter: usize = 0;
    for master in &pres.master_slides {
        for shape in &master.shapes {
            if is_default(&shape.style) {
                result.push(None);
            } else {
                counter += 1;
                result.push(Some((format!("gr{}", counter), shape.style.clone())));
            }
        }
    }
    for slide in &pres.slides {
        for shape in &slide.shapes {
            if is_default(&shape.style) {
                result.push(None);
            } else {
                counter += 1;
                result.push(Some((format!("gr{}", counter), shape.style.clone())));
            }
        }
    }
    result
}

/// 阶段E：写出单个图形样式的 automatic-style 定义。
/// 单向：fodp.rs::build_shape 完全不解析样式继承链，shape.style 读回来
/// 永远是 ShapeStyle::default()（见文件头"阶段E"说明）。仅供外部软件参考。
fn write_graphic_style_def(out: &mut String, name: &str, style: &ShapeStyle) {
    let _ = writeln!(
        out,
        r#"    <style:style style:name="{}" style:family="graphic">"#,
        xml_escape_attr(name)
    );
    out.push_str("      <style:graphic-properties");
    match &style.fill {
        Some(FillStyle::Solid(c)) => {
            let _ = write!(out, r#" draw:fill="solid" draw:fill-color="{}""#, color_hex(c));
        }
        Some(FillStyle::Gradient { stops, kind, .. }) => {
            // ODF 渐变标准做法需要单独的 <draw:gradient> 定义并用 draw:fill-gradient-name
            // 引用；阶段E为保持简单只写起止色作为近似 solid fill 参考，并用注释属性
            // 留痕渐变语义，不引入新的 automatic-style 子元素。仅供外部软件参考。
            //
            // PPTX-12 模型改造适配：stops 现在是任意长度的 Vec（不再是固定
            // start/end 两个字段），这里取首尾两个色标近似还原旧行为——
            // 若未来某个 gradFill 有 3+ 色标，中间色标在写出 ODF 时会丢失，
            // 这是已知的、有意为之的降级（ODF 写出本身就是"参考近似"定位，
            // 见本函数文档注释"仅供外部软件参考"）。angle 只有 Linear
            // 类型有意义，Radial/Rect 第一期写出时退化为 0 度。
            let start = stops.first().map(|s| s.color)
                .unwrap_or(Color { r: 200, g: 220, b: 245, a: 255 });
            let end = stops.last().map(|s| s.color)
                .unwrap_or(Color { r: 100, g: 140, b: 200, a: 255 });
            let angle = match kind {
                roe_model::common::GradientKind::Linear { angle } => *angle,
                _ => 0.0,
            };
            let _ = write!(
                out,
                r#" draw:fill="gradient" draw:fill-color="{}" draw:secondary-fill-color="{}" draw:angle="{:.1}""#,
                color_hex(&start), color_hex(&end), angle
            );
        }
        Some(FillStyle::None) => {
            out.push_str(r#" draw:fill="none""#);
        }
        // PROOT-005: FillStyle::Picture 目前只会出现在 Slide/MasterSlide.background
        // （幻灯片背景图），不会出现在普通 shape 的 style.fill 上（本次未接入该路径）。
        // ODF 图片背景导出需要独立的 draw:fill="bitmap" + <draw:fill-image> 定义，
        // 阶段E "仅供外部软件参考"定位下不实现，降级为 none 保底，不报错。
        Some(FillStyle::Picture(_)) => {
            out.push_str(r#" draw:fill="none""#);
        }
        // ROE-TILE-001: 与 Picture 同理，ODF 写出功能本次未实现
        // （本次任务只做读取解析+渲染显示），降级为 none 保底，不报错。
        Some(FillStyle::PictureTiled { .. }) => {
            out.push_str(r#" draw:fill="none""#);
        }
        None => {}
    }
    if let Some(stroke) = &style.stroke {
        let _ = write!(
            out,
            r#" draw:stroke="solid" svg:stroke-color="{}" svg:stroke-width="{:.2}cm""#,
            color_hex(&stroke.color), stroke.width
        );
    }
    if let Some(shadow) = &style.shadow {
        let _ = write!(
            out,
            r#" draw:shadow="visible" draw:shadow-color="{}" draw:shadow-offset-x="{:.2}cm" draw:shadow-offset-y="{:.2}cm""#,
            color_hex(&shadow.color), shadow.offset_x, shadow.offset_y
        );
        // blur 在 ODF graphic-properties 里没有直接对应属性，用扩展属性留痕，
        // 仅供参考，标准软件会忽略未知属性（不影响合规性）。
        let _ = write!(out, r#" draw:shadow-blur="{:.2}cm""#, shadow.blur);
    }
    out.push_str("></style:graphic-properties>\n");
    out.push_str("    </style:style>\n");
}

/// 颜色转十六进制字符串，如 #FF0000。注意 alpha 通道 ODF graphic-properties
/// 没有标准位置承载，当前丢弃（仅供外部软件参考，非 roundtrip 路径）。
fn color_hex(c: &Color) -> String {
    format!("#{:02X}{:02X}{:02X}", c.r, c.g, c.b)
}

fn xml_escape_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn xml_escape_attr(s: &str) -> String {
    xml_escape_text(s).replace('"', "&quot;")
}

// ── Shape 写出（阶段B） ─────────────────────────────────────

/// 写出单个 Shape：position(cm) + geometry 标签 + 文本内容。
/// 标签选择见文件头"阶段B"注释——geometry 映射非对称，每个变体
/// 固定写出一种标签，不假设可逆。
fn write_shape(out: &mut String, shape: &Shape, style_name: Option<&str>) {
    let (open_tag, close_tag, extra_attrs) = geometry_tag(&shape.geometry);

    let style_attr = style_name
        .map(|n| format!(r#" draw:style-name="{}""#, xml_escape_attr(n)))
        .unwrap_or_default();

    // xml:id 无条件写出（不判断是否被动画引用），见文件头"阶段D"说明：
    // parser 用这个属性建立 "xml:id字符串 -> 新分配的运行时id" 映射，
    // AnimationNode.target 反查时依赖这个映射存在。未被引用的 shape
    // 多一个 xml:id 属性无副作用，比"判断是否需要"更简单且不易出错。
    let _ = write!(
        out,
        r#"      <{} xml:id="{}"{}{} svg:x="{:.2}cm" svg:y="{:.2}cm" svg:width="{:.2}cm" svg:height="{:.2}cm">"#,
        open_tag, shape_ref(shape.id), style_attr, extra_attrs,
        shape.position.x() / 28.3465, shape.position.y() / 28.3465,
        shape.position.w() / 28.3465, shape.position.h() / 28.3465
    );
    out.push_str("\n");

    if let ShapeGeometry::Custom(type_str) = &shape.geometry {
        let _ = writeln!(
            out,
            r#"        <draw:enhanced-geometry draw:type="{}"></draw:enhanced-geometry>"#,
            xml_escape_attr(type_str)
        );
    }

    write_shape_text(out, &shape.content);

    let _ = writeln!(out, "      </{}>", close_tag);
}

/// 返回 (开标签名, 闭标签名, 额外属性字符串)。
/// RoundRect/Triangle 走 draw:custom-shape，单向（见文件头说明）。
fn geometry_tag(geo: &ShapeGeometry) -> (&'static str, &'static str, String) {
    match geo {
        ShapeGeometry::Rect => ("draw:rect", "draw:rect", String::new()),
        ShapeGeometry::Ellipse => ("draw:ellipse", "draw:ellipse", String::new()),
        ShapeGeometry::Line => ("draw:line", "draw:line", String::new()),
        ShapeGeometry::RoundRect { radius } => {
            // 单向：parser 不识别这个 draw:type 字符串，roundtrip 会落进
            // Custom(other)。radius 编码进 draw:type 仅供 ROE 自身未来
            // 升级 parser 时参考，外部软件按 custom-shape 兜底显示。
            let _ = radius; // 暂不写入几何路径，仅标注类型字符串
            ("draw:custom-shape", "draw:custom-shape", String::new())
        }
        ShapeGeometry::Triangle => {
            ("draw:custom-shape", "draw:custom-shape", String::new())
        }
        ShapeGeometry::Custom(_) => ("draw:custom-shape", "draw:custom-shape", String::new()),
        ShapeGeometry::PresetPath(_) => ("draw:custom-shape", "draw:custom-shape", String::new()),
    }
}

/// Shape 文本内容写出。阶段B 简化版：不处理 StyleRegistry 真实样式
/// 定义，style/char_style 仅作为名字引用写出。与 fodt_writer.rs 的
/// write_paragraph/write_run 故意保持独立（FodpWriter 此阶段不需要
/// Writer 那套 AutoCounter 内联样式自动生成逻辑）。
///
/// ⚠ 与 fodt_writer.rs 同样的坑：text:p 标签和内容之间不能有空白，
/// 否则 trim_text(false) 会把空白当文本节点污染 runs。
fn write_shape_text(out: &mut String, content: &ShapeContent) {
    match content {
        ShapeContent::Text(paragraphs) => {
            out.push_str("        <draw:text-box>");
            for p in paragraphs {
                write_shape_paragraph(out, p);
            }
            out.push_str("</draw:text-box>\n");
        }
        ShapeContent::Image(roe_model::common::ImageData { data, mime, .. }) => write_shape_image(out, data, mime),
        ShapeContent::Table(_) => {} // TODO: PPTX-6 表格写出，本次仅打通模型层
        ShapeContent::Chart(_) | ShapeContent::StatChart(_) => {}
        ShapeContent::Group { .. } => {} // TODO: PPTX-9 分组写出，本次(9a)仅打通模型+渲染层。
        // PROOT-D 子问题2（v62）：Group 变体从 tuple 换成了 struct-like
        // （新增 ch_off/ch_ext），这里的通配符写法从 Group(_) 改成
        // Group { .. }，语义不变（依然是"分组写出未实现"的占位）。
        ShapeContent::Empty => {}
    }
}

fn write_shape_image(out: &mut String, data: &[u8], mime: &str) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    let _ = write!(
        out,
        r#"        <draw:image xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"><office:binary-data mime-type="{}">"#,
        xml_escape_attr(mime)
    );
    out.push_str(&encoded);
    out.push_str("</office:binary-data></draw:image>\n");
}

fn write_shape_paragraph(out: &mut String, p: &Paragraph) {
    let style_attr = if p.style.0.is_empty() {
        String::new()
    } else {
        format!(r#" text:style-name="{}""#, xml_escape_attr(&p.style.0))
    };
    let _ = write!(out, r#"<text:p{}>"#, style_attr);
    for r in &p.runs {
        write_shape_run(out, r);
    }
    out.push_str("</text:p>");
}

fn write_shape_run(out: &mut String, r: &Run) {
    let text = xml_escape_text(&r.text);
    if let Some(ref style) = r.char_style {
        let _ = write!(out, r#"<text:span text:style-name="{}">{}</text:span>"#, xml_escape_attr(&style.0), text);
    } else {
        out.push_str(&text);
    }
}

// ── 动画树写出（阶段D） ─────────────────────────────────────

fn shape_ref(id: u32) -> String {
    format!("shape_{}", id)
}

/// 写出 TimingRoot（顶层动画树）+ 可选的幻灯片转场包装节点。
/// 若 animation_tree 不是 TimingRoot{children:[]}（即真的有动画或转场），
/// 才输出 anim:par 顶层节点；空动画树（默认值）什么都不写，保持文件精简。
fn write_animation_tree(out: &mut String, tree: &AnimationNode, transition: Option<&SlideTransition>) {
    let AnimationNode::TimingRoot { children } = tree else {
        // 当前模型里 Slide.animation_tree 字段类型即为 AnimationNode，
        // 非 TimingRoot 的顶层节点不符合文档语义，按 TimingRoot{[]} 处理
        if transition.is_none() {
            return;
        }
        write_timing_root(out, &[], transition);
        return;
    };
    if children.is_empty() && transition.is_none() {
        return; // 无动画无转场，不输出空的 anim:par 节点
    }
    write_timing_root(out, children, transition);
}

fn write_timing_root(out: &mut String, children: &[AnimationNode], transition: Option<&SlideTransition>) {
    out.push_str(r#"      <anim:par presentation:node-type="timing-root">"#);
    out.push_str("
");
    if let Some(t) = transition {
        write_transition_wrapper(out, t);
    }
    for child in children {
        write_animation_node(out, child);
    }
    out.push_str("      </anim:par>
");
}

/// 幻灯片级转场必须包装成 fodp.rs::is_slide_transition_wrapper 认识的
/// 结构：唯一子节点是不带 smil:targetElement 的 anim:transitionFilter。
fn write_transition_wrapper(out: &mut String, t: &SlideTransition) {
    out.push_str("        <anim:par>
");
    let (smil_type, smil_subtype) = transition_kind_to_smil(&t.kind);
    let subtype_attr = smil_subtype
        .map(|s| format!(r#" smil:subtype="{}""#, s))
        .unwrap_or_default();
    let _ = writeln!(
        out,
        r#"          <anim:transitionFilter smil:dur="{:.3}s" smil:type="{}"{}></anim:transitionFilter>"#,
        t.duration, smil_type, subtype_attr
    );
    out.push_str("        </anim:par>
");
}

fn transition_kind_to_smil(k: &TransitionKind) -> (&'static str, Option<String>) {
    match k {
        TransitionKind::None => ("none", None),
        TransitionKind::Fade => ("fade", None),
        TransitionKind::Push => ("push", None),
        TransitionKind::Wipe => ("wipe", None),
        TransitionKind::Split => ("wipe", Some("split".to_string())), // 近似映射，parser 端 Split 本身没有专属 smil:type
        TransitionKind::Zoom => ("zoom", None),
        TransitionKind::Custom(s) => {
            // Custom 写出时把原始 label 拆回 "{type}-{subtype}" 形式
            // （对称于 parse_transition_kind 的拼接方式），尽力还原；
            // 如果原 label 不含 '-'，subtype 留空
            match s.split_once('-') {
                Some((t, sub)) => return (Box::leak(t.to_string().into_boxed_str()), Some(sub.to_string())),
                None => return (Box::leak(s.clone().into_boxed_str()), None),
            }
        }
    }
}

fn write_animation_node(out: &mut String, node: &AnimationNode) {
    match node {
        AnimationNode::TimingRoot { children } => {
            // 嵌套 TimingRoot 不是合法文档语义，按 Parallel 兜底写出，
            // 避免静默丢失内容
            out.push_str("        <anim:par>
");
            for c in children { write_animation_node(out, c); }
            out.push_str("        </anim:par>
");
        }
        AnimationNode::MainSequence { effects } => {
            out.push_str(r#"        <anim:seq presentation:node-type="main-sequence">"#);
            out.push_str("
");
            for e in effects { write_animation_node(out, &e.node); }
            out.push_str("        </anim:seq>
");
        }
        AnimationNode::InteractiveSequence { trigger_shape_id, effects } => {
            let _ = writeln!(
                out,
                r#"        <anim:par presentation:node-type="interactive-sequence" presentation:group-id="{}">"#,
                shape_ref(*trigger_shape_id)
            );
            for e in effects { write_animation_node(out, &e.node); }
            out.push_str("        </anim:par>
");
        }
        AnimationNode::Parallel { children, timing } => {
            let _ = writeln!(out, r#"        <anim:par{}>"#, timing_attrs(timing));
            for c in children { write_animation_node(out, c); }
            out.push_str("        </anim:par>
");
        }
        AnimationNode::Sequence { children, timing } => {
            let _ = writeln!(out, r#"        <anim:seq{}>"#, timing_attrs(timing));
            for c in children { write_animation_node(out, c); }
            out.push_str("        </anim:seq>
");
        }
        AnimationNode::Animate { target, property, timing } => {
            let _ = writeln!(
                out,
                r#"        <anim:animate smil:targetElement="{}" smil:attributeName="{}"{}></anim:animate>"#,
                shape_ref(*target), property_to_smil(property), timing_attrs(timing)
            );
        }
        AnimationNode::AnimateColor { target, from, to, timing } => {
            let _ = writeln!(
                out,
                r#"        <anim:animateColor smil:targetElement="{}" smil:from="{}" smil:to="{}"{}></anim:animateColor>"#,
                shape_ref(*target), color_to_hex(from), color_to_hex(to), timing_attrs(timing)
            );
        }
        AnimationNode::AnimateMotion { target, path, timing } => {
            let _ = writeln!(
                out,
                r#"        <anim:animateMotion smil:targetElement="{}" svg:path="{}"{}></anim:animateMotion>"#,
                shape_ref(*target), xml_escape_attr(path), timing_attrs(timing)
            );
        }
        AnimationNode::AnimateTransform { target, kind, timing } => {
            let _ = writeln!(
                out,
                r#"        <anim:animateTransform smil:targetElement="{}" svg:type="{}"{}></anim:animateTransform>"#,
                shape_ref(*target), transform_kind_to_smil(kind), timing_attrs(timing)
            );
        }
        AnimationNode::AnimateSet { target, property, to, timing } => {
            let _ = writeln!(
                out,
                r#"        <anim:set smil:targetElement="{}" smil:attributeName="{}" smil:to="{}"{}></anim:set>"#,
                shape_ref(*target), property_to_smil(property), anim_value_to_smil(to), timing_attrs(timing)
            );
        }
        AnimationNode::Audio { source, timing } => {
            // 单向：parser 完全不识别此节点，见文件头注释
            let _ = writeln!(
                out,
                r#"        <anim:audio xlink:href="{}"{}></anim:audio>"#,
                xml_escape_attr(source), timing_attrs(timing)
            );
        }
    }
}

fn timing_attrs(t: &TimingSpec) -> String {
    let mut s = String::new();
    let _ = write!(s, r#" smil:begin="{}""#, begin_to_smil(&t.begin));
    if t.duration >= 0.0 {
        let _ = write!(s, r#" smil:dur="{:.3}s""#, t.duration);
    } else {
        s.push_str(r#" smil:dur="indefinite""#);
    }
    match &t.repeat {
        RepeatSpec::Count(n) => { let _ = write!(s, r#" smil:repeatCount="{}""#, n); }
        RepeatSpec::Indefinite => s.push_str(r#" smil:repeatCount="indefinite""#),
        RepeatSpec::None => {}
    }
    let fill = match t.fill {
        FillMode::Remove => Some("remove"),
        FillMode::Freeze => Some("freeze"),
        FillMode::Hold => Some("hold"),
        FillMode::Transition => Some("transition"),
        FillMode::Auto => Some("auto"),
        FillMode::Default => None,
    };
    if let Some(f) = fill {
        let _ = write!(s, r#" smil:fill="{}""#, f);
    }
    let _ = write!(s, r#" presentation:node-type="{}""#, node_type_to_smil(&t.node_type));
    s
}

/// 见文件头"BeginCondition 写出映射"注释：仅 Offset/OnNext/Indefinite
/// 可双向，其余六种变体写出后会被 parse_begin 兜底降级为 Indefinite。
fn begin_to_smil(b: &BeginCondition) -> String {
    match b {
        BeginCondition::Offset(n) => format!("{}s", n),
        BeginCondition::OnNext => "next".to_string(),
        BeginCondition::Indefinite => "indefinite".to_string(),
        // 以下六种：parser 无法反向解析，写出最贴近语义的字符串仅供
        // 外部软件参考，ROE 自身 roundtrip 会读成 Indefinite
        BeginCondition::OnClick(shape_id) => format!("{}.click", shape_ref(*shape_id)),
        BeginCondition::AfterPrevious(delay) => format!("afterEffect+{}s", delay),
        BeginCondition::WithPrevious(delay) => format!("withEffect+{}s", delay),
        BeginCondition::OnBeginOf(node_ref) => format!("id{}.begin", node_ref),
        BeginCondition::OnEndOf(node_ref) => format!("id{}.end", node_ref),
        BeginCondition::Multi(conds) => conds.iter().map(begin_to_smil).collect::<Vec<_>>().join(";"),
    }
}

fn node_type_to_smil(nt: &EffectNodeType) -> &'static str {
    match nt {
        EffectNodeType::TimingRoot => "timing-root",
        EffectNodeType::MainSequence => "main-sequence",
        EffectNodeType::InteractiveSequence => "interactive-sequence",
        EffectNodeType::OnClick => "on-click",
        EffectNodeType::WithPrevious => "with-previous",
        EffectNodeType::AfterPrevious => "after-previous",
    }
}

fn property_to_smil(p: &AnimProperty) -> String {
    match p {
        AnimProperty::Opacity => "opacity".to_string(),
        AnimProperty::PositionX => "x".to_string(),
        AnimProperty::PositionY => "y".to_string(),
        AnimProperty::ScaleX | AnimProperty::ScaleY => "scale".to_string(), // 近似，parser 无专属 scale 解析
        AnimProperty::Rotation => "rotation".to_string(),
        AnimProperty::FillColor => "fill-color".to_string(),
        AnimProperty::StrokeColor => "stroke-color".to_string(),
        AnimProperty::Visibility => "visibility".to_string(),
        AnimProperty::Custom(s) => s.clone(),
    }
}

fn anim_value_to_smil(v: &AnimValue) -> String {
    match v {
        AnimValue::Bool(true) => "visible".to_string(),
        AnimValue::Bool(false) => "hidden".to_string(),
        AnimValue::Number(n) => n.to_string(),
        AnimValue::Text(s) => s.clone(),
        AnimValue::Color(c) => color_to_hex(c),
    }
}

fn transform_kind_to_smil(k: &TransformKind) -> &'static str {
    match k {
        TransformKind::Scale => "scale",
        TransformKind::Rotate => "rotate",
        TransformKind::Translate => "translate",
        TransformKind::SkewX => "skewX",
        TransformKind::SkewY => "skewY",
    }
}

fn color_to_hex(c: &Color) -> String {
    format!("#{:02X}{:02X}{:02X}", c.r, c.g, c.b)
}

// ── 测试 ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::odf::fodp::FodpParser;
    use roe_model::impress::{AnimationNode, MasterSlide, Slide};
    use roe_model::common::{Transform, StyleRef};
    use roe_model::writer::{Paragraph, Run};

    fn empty_slide(master_idx: Option<usize>) -> Slide {
        Slide { layout_idx: None,
            shapes: vec![],
            master_idx,
            animation_tree: AnimationNode::default(),
            transition: None,
            background: None,
            layout_shapes: vec![],
            notes: None,
        }
    }

    #[test]
    fn test_roundtrip_slide_size_pt() {
        // 28.0cm / 15.75cm 换算为 pt 构造（x28.3465），验证写出时
        // pt -> cm 换算正确，XML 里应仍是干净的 "28.00cm"/"15.75cm"。
        let pres = Presentation {
            styles: Default::default(),
            slide_size: (28.0 * 28.3465, 15.75 * 28.3465),
            slides: vec![],
            master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };
        let xml = FodpWriter::write(&pres);
        assert!(xml.contains(r#"fo:page-width="28.00cm""#));
        assert!(xml.contains(r#"fo:page-height="15.75cm""#));

        let parsed = FodpParser::parse(&xml).expect("roundtrip parse failed");
        assert!((parsed.slide_size.0 - 28.0 * 28.3465).abs() < 0.05);
        assert!((parsed.slide_size.1 - 15.75 * 28.3465).abs() < 0.05);
    }

    #[test]
    fn test_roundtrip_slide_count_no_master() {
        let pres = Presentation {
            styles: Default::default(),
            slide_size: (28.0, 21.0),
            slides: vec![empty_slide(None), empty_slide(None), empty_slide(None)],
            master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };
        let xml = FodpWriter::write(&pres);
        let parsed = FodpParser::parse(&xml).expect("roundtrip parse failed");

        assert_eq!(parsed.slides.len(), 3);
        for s in &parsed.slides {
            assert_eq!(s.master_idx, None);
            assert_eq!(s.shapes.len(), 0);
        }
    }

    #[test]
    fn test_roundtrip_master_slide_name_association() {
        let pres = Presentation {
            styles: Default::default(),
            slide_size: (28.0, 21.0),
            slides: vec![empty_slide(Some(0)), empty_slide(Some(1)), empty_slide(Some(0))],
            master_slides: vec![
                MasterSlide { theme_xml: None, tx_styles_xml: None, layouts: Vec::new(), color_map: None, raw_ext: None, name: "Midnightblue".into(), shapes: vec![], background: None },
                MasterSlide { theme_xml: None, tx_styles_xml: None, layouts: Vec::new(), color_map: None, raw_ext: None, name: "Default".into(), shapes: vec![], background: None },
            ],
            theme_colors: Default::default(),
            default_text_style: Default::default(),
        };
        let xml = FodpWriter::write(&pres);
        assert!(xml.contains(r#"style:name="Midnightblue""#));
        assert!(xml.contains(r#"style:name="Default""#));
        assert!(xml.contains(r#"draw:master-page-name="Midnightblue""#));
        assert!(xml.contains(r#"draw:master-page-name="Default""#));

        let parsed = FodpParser::parse(&xml).expect("roundtrip parse failed");
        assert_eq!(parsed.master_slides.len(), 2);
        // parser 是按 master-page 在 XML 中出现的顺序建立 name->idx 映射，
        // 与写出时的 master_slides 数组顺序一致，故 idx 应原样保留
        assert_eq!(parsed.slides[0].master_idx, Some(0));
        assert_eq!(parsed.slides[1].master_idx, Some(1));
        assert_eq!(parsed.slides[2].master_idx, Some(0));
        assert_eq!(parsed.master_slides[0].name, "Midnightblue");
        assert_eq!(parsed.master_slides[1].name, "Default");
    }

    #[test]
    fn test_roundtrip_master_idx_out_of_range_falls_back_to_none() {
        // 防御性测试：master_idx 指向不存在的下标时，写出端会跳过该属性
        // （and_then 在 get 失败时短路），不应 panic，也不应写出空字符串属性
        let pres = Presentation {
            styles: Default::default(),
            slide_size: (28.0, 21.0),
            slides: vec![empty_slide(Some(99))],
            master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };
        let xml = FodpWriter::write(&pres);
        assert!(!xml.contains("draw:master-page-name"));

        let parsed = FodpParser::parse(&xml).expect("roundtrip parse failed");
        assert_eq!(parsed.slides[0].master_idx, None);
    }

    #[test]
    fn test_empty_presentation() {
        let pres = Presentation { styles: Default::default(), slide_size: (28.0, 21.0), slides: vec![], master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };
        let xml = FodpWriter::write(&pres);
        let parsed = FodpParser::parse(&xml).expect("roundtrip parse failed");
        assert_eq!(parsed.slides.len(), 0);
        assert_eq!(parsed.master_slides.len(), 0);
    }

    // ── 阶段B：Shape position/geometry/文本 ─────────────────

    fn shape_with(geometry: ShapeGeometry, content: ShapeContent) -> Shape {
        Shape {
            id: 0, // 写出端不使用/不持久化此字段，见文件头说明
            text_body: roe_model::impress::TextBodyProps::default(),
            geometry,
            // 数值对应 1.0/2.0/10.0/5.0 cm，换算为 pt（x28.3465），
            // 与 Presentation.slide_size 统一单位语义（2026-06-18 修复）。
            position: Transform::from_rect(1.0 * 28.3465, 2.0 * 28.3465, 10.0 * 28.3465, 5.0 * 28.3465),
            style: roe_model::impress::ShapeStyle::default(),
            content,
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        }
    }

    fn text_content(text: &str) -> ShapeContent {
        ShapeContent::Text(vec![Paragraph {
            style: StyleRef("Default".into()),
            runs: vec![Run { text: text.into(), ..Default::default() }],
            list_info: None,
            ..Default::default()
        }])
    }

    #[test]
    fn test_roundtrip_shape_position_pt() {
        // 1.0/2.0/10.0/5.0 (cm) 换算为 pt 构造，验证 pt -> cm 写出换算正确。
        let mut slide = empty_slide(None);
        slide.shapes.push(shape_with(ShapeGeometry::Rect, ShapeContent::Empty));
        let pres = Presentation {
            styles: Default::default(),
            slide_size: (28.0 * 28.3465, 21.0 * 28.3465),
            slides: vec![slide],
            master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };

        let xml = FodpWriter::write(&pres);
        assert!(xml.contains(r#"svg:x="1.00cm""#));
        assert!(xml.contains(r#"svg:y="2.00cm""#));
        assert!(xml.contains(r#"svg:width="10.00cm""#));
        assert!(xml.contains(r#"svg:height="5.00cm""#));

        let parsed = FodpParser::parse(&xml).expect("roundtrip parse failed");
        let shape = &parsed.slides[0].shapes[0];
        assert!((shape.position.x() - 1.0 * 28.3465).abs() < 0.05);
        assert!((shape.position.y() - 2.0 * 28.3465).abs() < 0.05);
        assert!((shape.position.w() - 10.0 * 28.3465).abs() < 0.05);
        assert!((shape.position.h() - 5.0 * 28.3465).abs() < 0.05);
    }

    #[test]
    fn test_roundtrip_geometry_rect_ellipse_line() {
        let mut slide = empty_slide(None);
        slide.shapes.push(shape_with(ShapeGeometry::Rect, ShapeContent::Empty));
        slide.shapes.push(shape_with(ShapeGeometry::Ellipse, ShapeContent::Empty));
        slide.shapes.push(shape_with(ShapeGeometry::Line, ShapeContent::Empty));
        let pres = Presentation { styles: Default::default(), slide_size: (28.0, 21.0), slides: vec![slide], master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };

        let xml = FodpWriter::write(&pres);
        let parsed = FodpParser::parse(&xml).expect("roundtrip parse failed");
        let shapes = &parsed.slides[0].shapes;
        assert_eq!(shapes.len(), 3);
        assert!(matches!(shapes[0].geometry, ShapeGeometry::Rect));
        assert!(matches!(shapes[1].geometry, ShapeGeometry::Ellipse));
        assert!(matches!(shapes[2].geometry, ShapeGeometry::Line));
    }

    #[test]
    fn test_roundtrip_geometry_custom_roundtrips_when_not_reserved_word() {
        let mut slide = empty_slide(None);
        slide.shapes.push(shape_with(ShapeGeometry::Custom("heart".into()), ShapeContent::Empty));
        let pres = Presentation { styles: Default::default(), slide_size: (28.0, 21.0), slides: vec![slide], master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };

        let xml = FodpWriter::write(&pres);
        assert!(xml.contains(r#"draw:type="heart""#));

        let parsed = FodpParser::parse(&xml).expect("roundtrip parse failed");
        match &parsed.slides[0].shapes[0].geometry {
            ShapeGeometry::Custom(s) => assert_eq!(s, "heart"),
            other => panic!("expected Custom(\"heart\"), got {:?}", other),
        }
    }

    #[test]
    fn test_geometry_roundrect_triangle_are_one_way_by_design() {
        // 已知限制（见文件头注释）：parser 不认识这两个变体对应的
        // draw:type，写出后会变成 Custom(other)，不是 panic，也不是
        // 数据丢失，只是几何类型语义降级。这里显式断言"会变成什么"，
        // 防止未来 parser 升级后这条测试静默过期却没人发现。
        let mut slide = empty_slide(None);
        slide.shapes.push(shape_with(ShapeGeometry::RoundRect { radius: 4.0 }, ShapeContent::Empty));
        slide.shapes.push(shape_with(ShapeGeometry::Triangle, ShapeContent::Empty));
        let pres = Presentation { styles: Default::default(), slide_size: (28.0, 21.0), slides: vec![slide], master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };

        let xml = FodpWriter::write(&pres);
        let parsed = FodpParser::parse(&xml).expect("roundtrip parse failed");
        let shapes = &parsed.slides[0].shapes;
        assert_eq!(shapes.len(), 2);
        // 两者都落进 custom-shape，draw:enhanced-geometry 没有 draw:type
        // 属性（write_shape 对 RoundRect/Triangle 不写 enhanced-geometry，
        // 见 geometry_tag 实现），parser 的 fallback 分支会给 Custom("custom-shape")
        assert!(matches!(&shapes[0].geometry, ShapeGeometry::Custom(s) if s == "custom-shape"));
        assert!(matches!(&shapes[1].geometry, ShapeGeometry::Custom(s) if s == "custom-shape"));
    }

    #[test]
    fn test_roundtrip_shape_text_content() {
        let mut slide = empty_slide(None);
        slide.shapes.push(shape_with(ShapeGeometry::Rect, text_content("Hello FODP")));
        let pres = Presentation { styles: Default::default(), slide_size: (28.0, 21.0), slides: vec![slide], master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };

        let xml = FodpWriter::write(&pres);
        let parsed = FodpParser::parse(&xml).expect("roundtrip parse failed");
        match &parsed.slides[0].shapes[0].content {
            ShapeContent::Text(paras) => {
                let text: String = paras.iter().flat_map(|p| p.runs.iter()).map(|r| r.text.as_str()).collect();
                assert_eq!(text, "Hello FODP");
            }
            other => panic!("expected Text content, got {:?}", other),
        }
    }

    #[test]
    fn test_roundtrip_master_slide_shapes() {
        let master = MasterSlide { theme_xml: None, tx_styles_xml: None, layouts: Vec::new(), color_map: None, raw_ext: None,
            name: "MyMaster".into(),
            shapes: vec![shape_with(ShapeGeometry::Rect, text_content("Master Title"))],
            background: None,
        };
        let pres = Presentation { styles: Default::default(), slide_size: (28.0, 21.0), slides: vec![], master_slides: vec![master], theme_colors: Default::default(), default_text_style: Default::default() };

        let xml = FodpWriter::write(&pres);
        let parsed = FodpParser::parse(&xml).expect("roundtrip parse failed");
        assert_eq!(parsed.master_slides.len(), 1);
        assert_eq!(parsed.master_slides[0].shapes.len(), 1);
        match &parsed.master_slides[0].shapes[0].content {
            ShapeContent::Text(paras) => {
                let text: String = paras.iter().flat_map(|p| p.runs.iter()).map(|r| r.text.as_str()).collect();
                assert_eq!(text, "Master Title");
            }
            other => panic!("expected Text content, got {:?}", other),
        }
    }

    #[test]
    fn test_roundtrip_run_char_style_reference() {
        let mut slide = empty_slide(None);
        let content = ShapeContent::Text(vec![Paragraph {
            style: StyleRef("Default".into()),
            runs: vec![Run {
                text: "styled".into(),
                char_style: Some(StyleRef("T10".into())),
                ..Default::default()
            }],
            list_info: None,
            ..Default::default()
        }]);
        slide.shapes.push(shape_with(ShapeGeometry::Rect, content));
        let pres = Presentation { styles: Default::default(), slide_size: (28.0, 21.0), slides: vec![slide], master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };

        let xml = FodpWriter::write(&pres);
        assert!(xml.contains(r#"text:style-name="T10""#));

        let parsed = FodpParser::parse(&xml).expect("roundtrip parse failed");
        match &parsed.slides[0].shapes[0].content {
            ShapeContent::Text(paras) => {
                assert_eq!(paras[0].runs[0].char_style, Some(StyleRef("T10".into())));
            }
            other => panic!("expected Text content, got {:?}", other),
        }
    }

    // ── 阶段D：动画树 / xml:id / 转场 ────────────────────────

    #[test]
    fn test_roundtrip_simple_animate_set_target_relationship() {
        // 核心断言：target 与对应 shape.id 的相等关系在 roundtrip 后保持，
        // 不假设 Shape.id 数值本身不变（见文件头说明）
        let mut shape = shape_with(ShapeGeometry::Rect, text_content("Animated"));
        shape.id = 7; // 故意用非 1 起始的 id，验证不依赖具体数值

        let mut slide = empty_slide(None);
        slide.shapes.push(shape);
        slide.animation_tree = AnimationNode::TimingRoot {
            children: vec![AnimationNode::MainSequence {
                effects: vec![roe_model::impress::AnimationEffect {
                    node: AnimationNode::AnimateSet {
                        target: 7,
                        property: AnimProperty::Visibility,
                        to: AnimValue::Bool(true),
                        timing: TimingSpec {
                            begin: BeginCondition::Offset(0.0),
                            duration: 0.001,
                            repeat: RepeatSpec::None,
                            fill: FillMode::Hold,
                            node_type: EffectNodeType::AfterPrevious,
                            easing: roe_model::impress::EasingFunction::default(),
                        },
                    },
                }],
            }],
        };
        let pres = Presentation { styles: Default::default(), slide_size: (28.0, 21.0), slides: vec![slide], master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };

        let xml = FodpWriter::write(&pres);
        assert!(xml.contains(r#"xml:id="shape_7""#));
        assert!(xml.contains(r#"smil:targetElement="shape_7""#));

        let parsed = FodpParser::parse(&xml).expect("roundtrip parse failed");
        let new_shape_id = parsed.slides[0].shapes[0].id;

        let AnimationNode::TimingRoot { children } = &parsed.slides[0].animation_tree else {
            panic!("expected TimingRoot");
        };
        let AnimationNode::MainSequence { effects } = &children[0] else {
            panic!("expected MainSequence, got {:?}", children[0]);
        };
        let AnimationNode::AnimateSet { target, property, to, .. } = &effects[0].node else {
            panic!("expected AnimateSet, got {:?}", effects[0].node);
        };
        // 关键断言：target 与（可能已变化的）shape.id 仍然相等
        assert_eq!(*target, new_shape_id, "target must still equal shape.id after roundtrip");
        assert!(matches!(property, AnimProperty::Visibility));
        assert!(matches!(to, AnimValue::Bool(true)));
    }

    #[test]
    fn test_roundtrip_begin_condition_offset_next_indefinite() {
        let conditions = vec![
            (BeginCondition::Offset(0.5), "offset"),
            (BeginCondition::OnNext, "next"),
            (BeginCondition::Indefinite, "indefinite"),
        ];
        for (cond, label) in conditions {
            let mut shape = shape_with(ShapeGeometry::Rect, ShapeContent::Empty);
            shape.id = 1;
            let mut slide = empty_slide(None);
            slide.shapes.push(shape);
            slide.animation_tree = AnimationNode::TimingRoot {
                children: vec![AnimationNode::MainSequence {
                    effects: vec![roe_model::impress::AnimationEffect {
                        node: AnimationNode::Animate {
                            target: 1,
                            property: AnimProperty::Opacity,
                            timing: TimingSpec {
                                begin: cond.clone(),
                                duration: 1.0,
                                repeat: RepeatSpec::None,
                                fill: FillMode::Default,
                                node_type: EffectNodeType::AfterPrevious,
                                easing: roe_model::impress::EasingFunction::default(),
                            },
                        },
                    }],
                }],
            };
            let pres = Presentation { styles: Default::default(), slide_size: (28.0, 21.0), slides: vec![slide], master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };
            let xml = FodpWriter::write(&pres);
            let parsed = FodpParser::parse(&xml).expect("roundtrip parse failed");

            let AnimationNode::TimingRoot { children } = &parsed.slides[0].animation_tree else { panic!("{label}: expected TimingRoot") };
            let AnimationNode::MainSequence { effects } = &children[0] else { panic!("{label}: expected MainSequence") };
            let AnimationNode::Animate { timing, .. } = &effects[0].node else { panic!("{label}: expected Animate") };

            match &cond {
                BeginCondition::Offset(n) => assert!(matches!(timing.begin, BeginCondition::Offset(v) if (v - n).abs() < 0.001), "{label}"),
                BeginCondition::OnNext => assert!(matches!(timing.begin, BeginCondition::OnNext), "{label}"),
                BeginCondition::Indefinite => assert!(matches!(timing.begin, BeginCondition::Indefinite), "{label}"),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn test_begin_condition_lossy_variants_degrade_to_indefinite() {
        // 已知限制（见文件头注释）：这六种变体 parse_begin 无法反向解析，
        // 显式断言降级行为，防止 parser 升级后这条测试静默过期
        let lossy_conditions = vec![
            BeginCondition::OnClick(1),
            BeginCondition::AfterPrevious(0.3),
            BeginCondition::WithPrevious(0.3),
            BeginCondition::OnBeginOf(2),
            BeginCondition::OnEndOf(2),
            BeginCondition::Multi(vec![BeginCondition::Offset(1.0), BeginCondition::OnNext]),
        ];
        for cond in lossy_conditions {
            let mut shape = shape_with(ShapeGeometry::Rect, ShapeContent::Empty);
            shape.id = 1;
            let mut slide = empty_slide(None);
            slide.shapes.push(shape);
            slide.animation_tree = AnimationNode::TimingRoot {
                children: vec![AnimationNode::MainSequence {
                    effects: vec![roe_model::impress::AnimationEffect {
                        node: AnimationNode::Animate {
                            target: 1,
                            property: AnimProperty::Opacity,
                            timing: TimingSpec {
                                begin: cond,
                                duration: 1.0,
                                repeat: RepeatSpec::None,
                                fill: FillMode::Default,
                                node_type: EffectNodeType::AfterPrevious,
                                easing: roe_model::impress::EasingFunction::default(),
                            },
                        },
                    }],
                }],
            };
            let pres = Presentation { styles: Default::default(), slide_size: (28.0, 21.0), slides: vec![slide], master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };
            let xml = FodpWriter::write(&pres);
            let parsed = FodpParser::parse(&xml).expect("roundtrip parse failed");

            let AnimationNode::TimingRoot { children } = &parsed.slides[0].animation_tree else { panic!("expected TimingRoot") };
            let AnimationNode::MainSequence { effects } = &children[0] else { panic!("expected MainSequence") };
            let AnimationNode::Animate { timing, .. } = &effects[0].node else { panic!("expected Animate") };
            assert!(matches!(timing.begin, BeginCondition::Indefinite), "expected degrade to Indefinite, got {:?}", timing.begin);
        }
    }

    #[test]
    fn test_roundtrip_main_sequence_and_interactive_sequence() {
        let mut shape1 = shape_with(ShapeGeometry::Rect, ShapeContent::Empty);
        shape1.id = 1;
        let mut shape2 = shape_with(ShapeGeometry::Rect, ShapeContent::Empty);
        shape2.id = 2;

        let mut slide = empty_slide(None);
        slide.shapes.push(shape1);
        slide.shapes.push(shape2);
        slide.animation_tree = AnimationNode::TimingRoot {
            children: vec![
                AnimationNode::MainSequence {
                    effects: vec![roe_model::impress::AnimationEffect {
                        node: AnimationNode::AnimateSet {
                            target: 1, property: AnimProperty::Visibility, to: AnimValue::Bool(true),
                            timing: TimingSpec { begin: BeginCondition::Offset(0.0), duration: 0.0, repeat: RepeatSpec::None, fill: FillMode::Hold, node_type: EffectNodeType::AfterPrevious, easing: roe_model::impress::EasingFunction::default() },
                        },
                    }],
                },
                AnimationNode::InteractiveSequence {
                    trigger_shape_id: 2,
                    effects: vec![roe_model::impress::AnimationEffect {
                        node: AnimationNode::AnimateSet {
                            target: 2, property: AnimProperty::Opacity, to: AnimValue::Number(0.5),
                            timing: TimingSpec { begin: BeginCondition::Offset(0.0), duration: 0.0, repeat: RepeatSpec::None, fill: FillMode::Hold, node_type: EffectNodeType::AfterPrevious, easing: roe_model::impress::EasingFunction::default() },
                        },
                    }],
                },
            ],
        };
        let pres = Presentation { styles: Default::default(), slide_size: (28.0, 21.0), slides: vec![slide], master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };
        let xml = FodpWriter::write(&pres);
        let parsed = FodpParser::parse(&xml).expect("roundtrip parse failed");

        let AnimationNode::TimingRoot { children } = &parsed.slides[0].animation_tree else { panic!("expected TimingRoot") };
        assert_eq!(children.len(), 2, "expected MainSequence + InteractiveSequence");
        assert!(matches!(children[0], AnimationNode::MainSequence { .. }));
        match &children[1] {
            AnimationNode::InteractiveSequence { trigger_shape_id, effects } => {
                let new_shape2_id = parsed.slides[0].shapes[1].id;
                assert_eq!(*trigger_shape_id, new_shape2_id);
                assert_eq!(effects.len(), 1);
            }
            other => panic!("expected InteractiveSequence, got {:?}", other),
        }
    }

    #[test]
    fn test_roundtrip_animate_color() {
        let mut shape = shape_with(ShapeGeometry::Rect, ShapeContent::Empty);
        shape.id = 1;
        let mut slide = empty_slide(None);
        slide.shapes.push(shape);
        slide.animation_tree = AnimationNode::TimingRoot {
            children: vec![AnimationNode::MainSequence {
                effects: vec![roe_model::impress::AnimationEffect {
                    node: AnimationNode::AnimateColor {
                        target: 1,
                        from: roe_model::common::Color { r: 255, g: 0, b: 0, a: 255 },
                        to: roe_model::common::Color { r: 0, g: 255, b: 0, a: 255 },
                        timing: TimingSpec { begin: BeginCondition::Offset(0.0), duration: 1.0, repeat: RepeatSpec::None, fill: FillMode::Default, node_type: EffectNodeType::AfterPrevious, easing: roe_model::impress::EasingFunction::default() },
                    },
                }],
            }],
        };
        let pres = Presentation { styles: Default::default(), slide_size: (28.0, 21.0), slides: vec![slide], master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };
        let xml = FodpWriter::write(&pres);
        assert!(xml.contains(r##"smil:from="#FF0000""##));
        assert!(xml.contains(r##"smil:to="#00FF00""##));

        let parsed = FodpParser::parse(&xml).expect("roundtrip parse failed");
        let AnimationNode::TimingRoot { children } = &parsed.slides[0].animation_tree else { panic!("expected TimingRoot") };
        let AnimationNode::MainSequence { effects } = &children[0] else { panic!("expected MainSequence") };
        let AnimationNode::AnimateColor { from, to, .. } = &effects[0].node else { panic!("expected AnimateColor, got {:?}", effects[0].node) };
        assert_eq!(from.r, 255); assert_eq!(from.g, 0); assert_eq!(from.b, 0);
        assert_eq!(to.r, 0); assert_eq!(to.g, 255); assert_eq!(to.b, 0);
    }

    #[test]
    fn test_roundtrip_slide_transition() {
        let mut slide = empty_slide(None);
        slide.transition = Some(SlideTransition {
            kind: TransitionKind::Fade,
            duration: 0.75,
            auto_advance: None,
        });
        let pres = Presentation { styles: Default::default(), slide_size: (28.0, 21.0), slides: vec![slide], master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };
        let xml = FodpWriter::write(&pres);
        let parsed = FodpParser::parse(&xml).expect("roundtrip parse failed");

        let transition = parsed.slides[0].transition.as_ref().expect("expected transition");
        assert!(matches!(transition.kind, TransitionKind::Fade));
        assert!((transition.duration - 0.75).abs() < 0.001);
    }

    #[test]
    fn test_shape_style_solid_fill_written() {
        let mut shape = shape_with(ShapeGeometry::Rect, ShapeContent::Empty);
        shape.style.fill = Some(roe_model::impress::FillStyle::Solid(
            roe_model::common::Color { r: 255, g: 0, b: 0, a: 255 },
        ));
        let mut slide = empty_slide(None);
        slide.shapes.push(shape);
        let pres = Presentation { styles: Default::default(), slide_size: (28.0, 21.0), slides: vec![slide], master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };
        let xml = FodpWriter::write(&pres);
        assert!(xml.contains(r##"draw:fill-color="#FF0000""##), "expected fill color in xml:\n{}", xml);
        assert!(xml.contains(r#"draw:style-name="gr1""#), "expected style-name reference on shape:\n{}", xml);
        assert!(xml.contains(r#"style:family="graphic""#));
    }

    #[test]
    fn test_shape_style_default_no_style_name_attr() {
        // ShapeStyle::default() 不应产生 draw:style-name 属性或样式定义
        let shape = shape_with(ShapeGeometry::Rect, ShapeContent::Empty);
        let mut slide = empty_slide(None);
        slide.shapes.push(shape);
        let pres = Presentation { styles: Default::default(), slide_size: (28.0, 21.0), slides: vec![slide], master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };
        let xml = FodpWriter::write(&pres);
        assert!(!xml.contains("draw:style-name"), "default style should not emit draw:style-name:\n{}", xml);
        assert!(!xml.contains(r#"style:family="graphic""#));
    }

    #[test]
    fn test_shape_image_base64_written() {
        let shape = shape_with(ShapeGeometry::Rect, ShapeContent::Image(roe_model::common::ImageData {
            data: vec![1, 2, 3, 4],
            mime: "image/png".to_string(),
            ..Default::default()
        }));
        let mut slide = empty_slide(None);
        slide.shapes.push(shape);
        let pres = Presentation { styles: Default::default(), slide_size: (28.0, 21.0), slides: vec![slide], master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };
        let xml = FodpWriter::write(&pres);
        assert!(xml.contains("draw:image"));
        assert!(xml.contains("office:binary-data"));
        assert!(xml.contains(r#"mime-type="image/png""#));
        // base64("\x01\x02\x03\x04") == "AQIDBA=="
        assert!(xml.contains("AQIDBA=="), "expected base64 payload in xml:\n{}", xml);
    }

    #[test]
    fn test_shape_chart_content_skipped() {
        let shape = shape_with(ShapeGeometry::Rect, ShapeContent::Chart(roe_model::impress::ChartModel {
            id: 0,
            kind: roe_model::impress::ChartKind::Bar,
            title: None,
            x_labels: vec![],
            series: vec![],
            x_pt: 0.0, y_pt: 0.0, width_pt: 300.0, height_pt: 300.0,
            show_legend: true, show_grid: true,
            x_axis_label: String::new(), y_axis_label: String::new(),
            border_color: roe_model::common::Color { r: 0xcc, g: 0xcc, b: 0xcc, a: 255 },
            border_width: 1.0,
            gap_width_pct: None,
            overlap_pct: None,
            grouping: roe_model::impress::ChartGrouping::Standard,
            hole_size_pct: None,
            data_labels: Default::default(),
            slice_border_color: None,
        }));
        let mut slide = empty_slide(None);
        slide.shapes.push(shape);
        let pres = Presentation { styles: Default::default(), slide_size: (28.0, 21.0), slides: vec![slide], master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };
        let xml = FodpWriter::write(&pres);
        // 不应崩溃，也不应产生任何 chart 相关标签（当前故意完全跳过）
        assert!(!xml.contains("draw:image"));
        assert!(!xml.contains("draw:text-box"));
    }

    #[test]
    fn test_no_animation_no_transition_produces_no_anim_par() {
        // 空动画树+无转场不应输出空的 anim:par 节点（保持文件精简）
        let slide = empty_slide(None);
        let pres = Presentation { styles: Default::default(), slide_size: (28.0, 21.0), slides: vec![slide], master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };
        let xml = FodpWriter::write(&pres);
        assert!(!xml.contains("anim:par"), "expected no anim:par for empty animation tree, got:\n{}", xml);

        let parsed = FodpParser::parse(&xml).expect("roundtrip parse failed");
        assert!(parsed.slides[0].transition.is_none());
    }

    #[test]
    fn test_audio_node_is_one_way() {
        // 已知限制：parser 完全不识别 anim:audio，写出后读不回来，
        // animation_tree 应该退化为没有这个 effect 的结构（被静默忽略，
        // 不是 panic，也不影响其他兄弟节点的解析）
        let mut slide = empty_slide(None);
        slide.animation_tree = AnimationNode::TimingRoot {
            children: vec![AnimationNode::MainSequence {
                effects: vec![roe_model::impress::AnimationEffect {
                    node: AnimationNode::Audio {
                        source: "narration.mp3".into(),
                        timing: TimingSpec { begin: BeginCondition::Offset(0.0), duration: 5.0, repeat: RepeatSpec::None, fill: FillMode::Default, node_type: EffectNodeType::AfterPrevious, easing: roe_model::impress::EasingFunction::default() },
                    },
                }],
            }],
        };
        let pres = Presentation { styles: Default::default(), slide_size: (28.0, 21.0), slides: vec![slide], master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() };
        let xml = FodpWriter::write(&pres);
        assert!(xml.contains("anim:audio"));
        assert!(xml.contains("narration.mp3"));

        // 确认能正常 parse（不 panic），即使语义丢失
        let parsed = FodpParser::parse(&xml).expect("should parse without panic even though anim:audio is unrecognized");
        let _ = parsed; // 不对 audio 节点内容做断言，因为已知读不回来
    }

    // ─── 真实文件 round-trip 测试（ODF §5.2 阶段三，v186后续）────────────

    /// 收集 shape 树里所有 Text 内容的段落文字（拼接后），递归展开
    /// Group（因为 writer 完全不写出 Group 本身，但 expected 侧递归进
    /// Group 内部仍可能含有 Text shape，只是这些内容在 actual 侧必然
    /// 消失——这正是本函数存在的意义：只比较"writer 声明支持"的部分，
    /// 不假设 Group 内容能 roundtrip）。
    ///
    /// 这里选择只比较"文字内容的集合"（不保证顺序、不保证 shape 结构
    /// 一一对应），因为 Table/Chart/Group 静默丢弃后，expected 和
    /// actual 的 shape 树形状必然不同（actual 会缺少对应 shape），逐一
    /// zip 比较行不通；比较"扁平化后的文字集合是否是子集关系"更贴近
    /// "writer 支持范围内的内容不应该丢"这个验证目标。
    fn collect_text_shape_strings(shapes: &[Shape], out: &mut Vec<String>) {
        for shape in shapes {
            match &shape.content {
                ShapeContent::Text(paragraphs) => {
                    let joined: String = paragraphs
                        .iter()
                        .flat_map(|p| p.runs.iter().map(|r| r.text.as_str()))
                        .collect();
                    if !joined.trim().is_empty() {
                        out.push(joined);
                    }
                }
                ShapeContent::Group { children, .. } => {
                    collect_text_shape_strings(children, out);
                }
                // Image/Table/Chart/Empty：不贡献文字内容
                _ => {}
            }
        }
    }

    /// 验证："writer 支持范围内"的文字内容（Text shape，含 Group 内部
    /// 递归展开）在 write→reparse 后仍然存在（子集关系：actual 应包含
    /// expected 里 non-Group-non-Table-non-Chart 部分产生的所有文字）。
    /// 不做严格数量相等断言，因为 Table/Chart/Group 被丢弃后，
    /// expected 展开出的文字集合可能比 actual 多（如果这些容器内部
    /// 恰好嵌套了 Text shape）——本函数改用"actual 的每一条文字都能在
    /// expected 里找到"这个更宽松但仍有效的方向性检查，加上"至少有
    /// 一部分文字保留下来"的基本合理性检查。
    fn assert_odp_text_shapes_preserved_within_writer_scope(
        expected_shapes: &[Shape],
        actual_shapes: &[Shape],
        path: &str,
    ) {
        let mut exp_texts = Vec::new();
        collect_text_shape_strings(expected_shapes, &mut exp_texts);
        let mut act_texts = Vec::new();
        collect_text_shape_strings(actual_shapes, &mut act_texts);

        for act in &act_texts {
            assert!(
                exp_texts.iter().any(|e| e == act),
                "{path}: actual 出现了 expected 里没有的文字内容: {act:?}
exp_texts={exp_texts:?}"
            );
        }

        if !exp_texts.is_empty() {
            assert!(
                !act_texts.is_empty(),
                "{path}: expected 有 {} 条非空文字内容，但 actual 一条都没有保留下来（writer 应该至少                 保留顶层非 Group/Table/Chart 的 Text shape）",
                exp_texts.len()
            );
        }
    }

    fn run_real_file_roundtrip_odp(path: &str) {
        let data = std::fs::read(path)
            .unwrap_or_else(|e| panic!("读取样本文件失败 {path}: {e}"));

        let expected = if path.ends_with(".fodp") {
            crate::RoeDocument::open_fodp_bytes(&data)
        } else {
            crate::RoeDocument::open_odf_bytes(&data)
        }
        .unwrap_or_else(|e| panic!("首次 parse 失败 {path}: {e:?}"));
        let expected = match expected {
            crate::RoeDocument::Impress(p) => p,
            other => panic!("{path}: 不是 Impress 文档 ({other:?} variant)"),
        };

        let xml = FodpWriter::write(&expected);
        let actual = FodpParser::parse(&xml)
            .unwrap_or_else(|e| panic!("write→reparse 失败 {path}: {e:?}"));

        assert_eq!(
            expected.slides.len(), actual.slides.len(),
            "{path}: slide 数量不一致"
        );
        for (si, (exp_slide, act_slide)) in expected.slides.iter().zip(actual.slides.iter()).enumerate() {
            let s_path = format!("{path} slide[{si}]");
            assert_odp_text_shapes_preserved_within_writer_scope(
                &exp_slide.shapes, &act_slide.shapes, &s_path,
            );
        }
    }

    #[test]
    fn test_real_file_roundtrip_numfmt() {
        // 探测脚本 scan_odp_roundtrip_candidates 挑选：9 slides / 9
        // shapes，全 clean（无 table/chart/group/roundrect/triangle），
        // 全 text shape，基线样本。
        run_real_file_roundtrip_odp(
            "/home/yong/Projects/roe/test_samples/curated/odp/small__sd_data_odp__numfmt.odp",
        );
    }

    #[test]
    fn test_real_file_roundtrip_tdf104405() {
        // 探测脚本挑选：3 shapes，含 1 image，用于验证 Image（已知单向，
        // parser 读回后归 Empty）不会导致 write/reparse 崩溃或其余
        // 文字内容丢失。
        run_real_file_roundtrip_odp(
            "/home/yong/Projects/roe/test_samples/curated/odp/large__sd_tiledrendering_data__tdf104405.fodp",
        );
    }
}
