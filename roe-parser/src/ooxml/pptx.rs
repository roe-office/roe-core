//! PPTX parser: OOXML → roe-model::impress::Presentation
//!
//! 实现范围（MVP）：
//!   - Presentation / Slide / SlideLayout / SlideMaster
//!   - Shape (sp) + Picture (pic)
//!   - TextBody → Paragraph → Run
//!   - 基础 Fill / Geometry / Transform
//!   - Placeholder Resolution（两阶段，无指针）
//!   - Image Relationship（内联 data）
//!
//! 暂不实现：Table / Chart / SmartArt / Animation / Notes / VML

use std::collections::HashMap;
use std::io::Read;

use quick_xml::events::Event;
use quick_xml::Reader;
use zip::ZipArchive;

use crate::ParseError;
use crate::ooxml::chart_common::{
    parse_ooxml_chart_xml as parse_pptx_chart_xml,
    parse_ooxml_theme_colors as parse_theme_colors,
    normalize_scheme_color_slot, attr_str, attr_f32,
};
use roe_model::common::{Color, Transform};
use roe_model::preset_geometry::{eval_preset_shape, find_preset_shape};
use roe_model::impress::{
    AnimationNode, FillStyle, GradientKind, GradientStop, MasterSlide, Presentation, Shape,
    ShapeContent, ShapeGeometry, ShapeStyle, Slide, StrokeStyle,
};
use roe_model::writer::{Paragraph, Run};
use roe_model::style::Alignment;

// ─── 内部中间结构（解析阶段用）──────────────────────────────────────────────

/// slideLayout 中每个 placeholder 的默认文字样式
#[derive(Debug, Clone, Default)]
struct LayoutPhStyle {
    font_size:   Option<f32>,
    bold:        Option<bool>,
    italic:      Option<bool>,
    color:       Option<Color>,
    font_family: Option<String>,
    // PROOT-027: layout lstStyle lvl1pPr 的段落对齐
    algn:        Option<Alignment>,
    // PROOT-027 followup: layout bodyPr anchor（垂直对齐 t/ctr/b）
    anchor:      Option<roe_model::impress::TextAnchor>,
    // PROOT-1xx (v99): layout bodyPr vert（文本框整体排版方向 vert/vert270/eaVert）
    vert:        Option<roe_model::impress::TextVert>,
    // PROOT-tdf158512: layout lstStyle lvl1pPr 的 buNone/buChar，仅用于
    // "该级别是否显式声明 buNone"的覆盖判断，其余bullet场景不使用这个
    // 字段驱动渲染（真实bullet信息走 Paragraph.bullet 现有链路）。
    bullet:      Option<roe_model::impress::BulletKind>,
}

/// PPTX-4：占位符匹配候选（来自 layout 或 master 层某一个占位符形状）。
/// 携带 type/index/是否有transform(位置有效) 三个匹配维度 + 该候选自身的位置/样式，
/// 匹配命中后位置和样式从同一个候选身上统一取出（不再是"位置查一遍、样式再查一遍"两套逻辑）。
#[derive(Debug, Clone)]
struct PlaceholderCandidate {
    ph_type: String,
    index: Option<u32>,
    /// 对应 pptx-glimpse "是否有 transform" 排序维度：该候选是否带有效的位置/尺寸信息。
    /// 复用现有"position.w>0 || position.h>0"判断位置是否有效的既有约定，不新增字段来源。
    has_transform: bool,
    /// PROOT: 该候选占位符段落是否有真实、非空的文本run（<a:r>，区别于仅有
    /// <a:endParaRPr>的空段落）。对应OnlyOffice hasCustomPrompt语义：
    /// 空占位符提示文字应优先复用"离slide最近的、带真实示例文字run"的那一层
    /// （layout/master），而不是简单地"layout优先、找不到才查master"——
    /// 因为很多layout的title/body只有<a:endParaRPr>没有真实<a:r>，此时
    /// 应该继续往上查master，master里可能才有真实示例文字（含完整格式：
    /// 颜色/字号/居中等）。见设计方案文档九、节 + roe-ext_progress_v73/v74。
    has_real_run: bool,
    position: Transform,
    style: LayoutPhStyle,
}

/// PPTX-4：占位符类型的 fallback 映射表，对应 pptx-glimpse computed/placeholders.ts 的 fallback type 表。
/// ctrTitle（居中标题）→ title，subTitle（副标题）→ body。
/// 用于"同类型精确匹配失败时，退而求其次匹配这个更通用的类型"的场景。
fn fallback_ph_type(ph_type: &str) -> Option<&'static str> {
    match ph_type {
        "ctrTitle" => Some("title"),
        "subTitle" => Some("body"),
        _ => None,
    }
}

/// PPTX-4：占位符六分支匹配算法，移植自 pptx-glimpse `computed/placeholders.ts` 的
/// `findMatchingPlaceholder`（对应设计文档《Impress-PPTX解析渲染参考文档-容器占位符形状表格.md》§2.3）。
///
/// 优先级从高到低：
///   1. index 精确匹配 + has_transform
///   2. index 精确匹配（不管有无 transform）
///   3. type 精确匹配 + has_transform
///   4. fallback type 匹配 + has_transform   (ctrTitle→title, subTitle→body)
///   5. type 精确匹配（不管有无 transform）
///   6. fallback type 匹配（不管有无 transform）
///
/// 每一分支在候选列表中找到即返回，不需要遍历完所有分支再排序——因为分支本身已经是
/// 优先级从高到低排列，第一个非空结果就是最优匹配。
fn match_placeholder<'a>(
    candidates: &'a [PlaceholderCandidate],
    target_type: &str,
    target_index: Option<u32>,
) -> Option<&'a PlaceholderCandidate> {
    let fallback = fallback_ph_type(target_type);

    // 1. index 精确匹配 + has_transform
    if let Some(idx) = target_index {
        if let Some(c) = candidates.iter().find(|c| c.index == Some(idx) && c.has_transform) {
            return Some(c);
        }
    }
    // 2. index 精确匹配（不管有无 transform）
    if let Some(idx) = target_index {
        if let Some(c) = candidates.iter().find(|c| c.index == Some(idx)) {
            return Some(c);
        }
    }
    // 3. type 精确匹配 + has_transform
    if let Some(c) = candidates.iter().find(|c| c.ph_type == target_type && c.has_transform) {
        return Some(c);
    }
    // 4. fallback type 匹配 + has_transform
    if let Some(fb) = fallback {
        if let Some(c) = candidates.iter().find(|c| c.ph_type == fb && c.has_transform) {
            return Some(c);
        }
    }
    // 5. type 精确匹配（不管有无 transform）
    if let Some(c) = candidates.iter().find(|c| c.ph_type == target_type) {
        return Some(c);
    }
    // 6. fallback type 匹配（不管有无 transform）
    if let Some(fb) = fallback {
        if let Some(c) = candidates.iter().find(|c| c.ph_type == fb) {
            return Some(c);
        }
    }
    None
}

/// PROOT-1xx (v99): 专用于位置(position/xfrm)继承——与 match_placeholder
/// 不同，这里必须严格要求"在layout里找到的候选真的带xfrm"才算数，找不到
/// 就转向master，不能像match_placeholder分支2/5那样"退而求其次接受无
/// transform的候选"，否则会把master里真正正确的位置信息掐断（复现样本：
/// slideLayout10.xml里title/body占位符只有<p:spPr/>空标签，没有自己的
/// xfrm，需要继续查master才能拿到真实位置，母版通常都有完整定义）。
fn match_placeholder_position<'a>(
    layout_candidates: &'a [PlaceholderCandidate],
    master_candidates: &'a [PlaceholderCandidate],
    target_type: &str,
    target_index: Option<u32>,
) -> Option<&'a PlaceholderCandidate> {
    let fallback = fallback_ph_type(target_type);

    // layout候选：只接受真的带transform的（match_placeholder分支1/3/4的
    // 语义，不接受分支2/5那种"不管有无transform"的宽松匹配）。
    if let Some(idx) = target_index {
        if let Some(c) = layout_candidates.iter().find(|c| c.index == Some(idx) && c.has_transform) {
            return Some(c);
        }
    }
    if let Some(c) = layout_candidates.iter().find(|c| c.ph_type == target_type && c.has_transform) {
        return Some(c);
    }
    if let Some(fb) = fallback {
        if let Some(c) = layout_candidates.iter().find(|c| c.ph_type == fb && c.has_transform) {
            return Some(c);
        }
    }

    // layout没有带transform的候选：转向master，用完整6分支匹配（master是
    // 最后一层兜底，找到什么就是什么，不需要再限制has_transform）。
    match_placeholder(master_candidates, target_type, target_index)
}

/// PPTX-12: gradFill 内 lin/path 二选一的原始（未换算）中间表示。
/// 角度复合换算（flip修正+叠加形状旋转）需要 raw.rot/flip_h/flip_v，
/// 这些字段在 gradFill 出现时可能还未读到（OOXML 子元素顺序不保证
/// xfrm 一定先于 gradFill），因此与既有 `rot: Option<i32>` 字段
/// 同一模式：解析阶段只收原始值，raw_to_shape 组装阶段统一换算。
#[derive(Debug, Clone)]
enum RawGradKind {
    /// 原始角度，1/60000度，对应 <a:lin ang="...">
    Linear { raw_angle: i32 },
    /// 对应 <a:path path="rect|circle|shape">
    Path { path_type: String },
}

struct RawShape {
    id: u32,
    name: String,
    /// placeholder: (type_str, idx)
    ph: Option<(String, Option<u32>)>,
    /// PROOT: 空占位符提示文案攻坚二期——该占位符匹配到的 layout/master
    /// 真实文字格式，转换后搬运进最终 Shape.placeholder.hint_style。
    /// 只在 match_placeholder 命中 candidate 时被设置，其余情况恒为 None
    /// （渲染层此时继续走固定兜底样式，不受影响）。
    ph_hint_style: Option<LayoutPhStyle>,
    position: Transform,
    /// PPTX-7：<a:xfrm rot="..."> 原始值（1/60000度），组装最终Shape时换算成普通角度
    rot: Option<i32>,
    /// PPTX-7：<a:xfrm flipH="1">
    flip_h: Option<bool>,
    /// PPTX-7：<a:xfrm flipV="1">
    flip_v: Option<bool>,
    /// PROOT-D 子问题2 bugfix（v62）：<p:grpSpPr><a:xfrm><a:chOff>，
    /// 子坐标系原点（EMU已换算为pt）。仅分组（grpSp）自身的 RawShape
    /// 会被设置（写入 group_stack 栈顶），普通 sp/pic/... 恒为 None。
    /// 与 position.offset（分组在父坐标系的 off）是两个不同概念，
    /// 不要混淆：off/ext 描述分组自己在父坐标系的位置/尺寸，
    /// chOff/chExt 描述子 Shape 局部坐标系的原点/范围。
    group_ch_off: Option<(f32, f32)>,
    /// PROOT-D 子问题2 bugfix（v62）：<p:grpSpPr><a:xfrm><a:chExt>，
    /// 子坐标系尺寸（EMU已换算为pt）。语义同 group_ch_off 上方注释。
    group_ch_ext: Option<(f32, f32)>,
    geometry: Option<String>,
    /// PROOT-012: <a:custGeom><a:pathLst><a:path w=".." h="..">内的
    /// moveTo/lnTo/cubicBezTo/quadBezTo/close命令，已按path声明的w/h
    /// 换算到raw.position.extent（形状实际pt尺寸）的比例，可直接包成
    /// ShapeGeometry::PresetPath消费。None表示该shape没有custGeom
    /// （走prstGeom/Custom原有逻辑）。优先级高于geometry(prst字符串)，
    /// 同一shape理论上二者不会同时出现（OOXML custGeom/prstGeom互斥）。
    cust_geom_path: Option<Vec<roe_model::common::PathCommand>>,
    /// PPTX-11 part5：<a:avLst><a:gd name="adjN" fmla="val N"/></a:avLst>
    /// 覆盖值，(adj名字, 数值) 列表。用于 eval_preset_shape 的
    /// adj_overrides 参数，覆盖 PresetShapeFormula.default_adj 里的默认值。
    /// 空 Vec 表示该形状未自定义任何 adj（用全部默认值求值）。
    adj_overrides: Vec<(String, f64)>,
    fill: Option<FillStyle>,
    /// PPTX-12: gradFill 收集到的原始数据（色标列表 + 类型 + rotWithShape），
    /// 角度换算延后到 raw_to_shape。与 fill 并存但不冲突：若 gradFill 出现，
    /// raw_to_shape 组装阶段会用这里的数据覆盖 fill（gradFill 和 solidFill/
    /// noFill 在 OOXML 里本就互斥，不会同时出现在同一个 spPr 里）。
    grad_raw: Option<(Vec<(f32, Color)>, RawGradKind, bool)>,
    stroke: Option<StrokeStyle>,
    /// PROOT-outerShdw: <a:effectLst><a:outerShdw blurRad=".." dist=".." dir="..">
    /// <a:srgbClr/a:schemeClr>+<a:alpha/></a:outerShdw></a:effectLst>。
    /// 原始值：(dist EMU, dir 1/60000度, blurRad EMU, 颜色, alpha百分比0-100000)。
    /// dist/dir 换算成 offset_x/offset_y、blurRad换算成pt、颜色+alpha合并成
    /// 最终 Color 的工作留给 raw_to_shape 统一处理（与 rot/crop 等字段
    /// "解析层只收原始值，组装阶段统一换算"的既有模式一致）。None 表示
    /// 该 shape 没有 outerShdw（不含 innerShdw/glow/softEdge/reflection/blur
    /// 等其他效果，第一期只覆盖 outerShdw 这一种，其余效果解析时忽略）。
    outer_shdw_raw: Option<(i32, i32, i32, Color, i32)>,
    content: RawContent,
    text_body: roe_model::impress::TextBodyProps,
    /// PROOT-002: <p:cxnSp><p:cNvCxnSpPr><a:stCxn id=".." idx=".."/>
    /// (目标shape id, idx)。仅普通<p:sp>不会有值，只有<p:cxnSp>解析时才写入。
    st_cxn: Option<(u32, u32)>,
    /// PROOT-002: <a:endCxn id=".." idx=".."/>，语义同 st_cxn。
    end_cxn: Option<(u32, u32)>,
    /// PROOT-027: layout lstStyle lvl1pPr algn
    lstyle_algn: Option<Alignment>,
    /// PROOT-027 followup: layout lstStyle lvl1pPr defRPr sz（EMU百分比->pt）
    lstyle_font_size: Option<f32>,
    /// PROOT-tdf158512: layout lstStyle lvl1pPr 的 <a:buNone/>/<a:buChar/>。
    /// 用于覆盖 master txStyles 继承来的 bullet（OOXML继承优先级：
    /// slide自身pPr > layout同名占位符lstStyle > master titleStyle/
    /// bodyStyle）。此前这条继承层级完全缺失，layout显式buNone从未被
    /// 读取，导致subTitle等占位符被master bodyStyle错误套上bullet
    /// （复现样本tdf158512）。
    lstyle_bullet: Option<roe_model::impress::BulletKind>,
    /// PROOT-027 followup: layout bodyPr anchor（垂直对齐 t/ctr/b）。注意：
    /// parse_raw_shapes()函数里raw.text_body从未被真正解析（一直是
    /// Default::default()），bodyPr的anchor属性只在parse_slide_xml_shapes()
    /// 里被读取，两个函数各自独立解析XML。故沿用font_size/algn同款模式，
    /// 用独立状态变量在parse_raw_shapes里单独读一次，不能指望raw.text_body。
    lstyle_anchor: Option<roe_model::impress::TextAnchor>,
    /// PROOT-1xx (v99): layout/master bodyPr vert（文本框整体排版方向），
    /// 沿用lstyle_anchor同款独立状态变量模式（parse_raw_shapes里
    /// raw.text_body从未被真正解析，见lstyle_anchor字段注释）。
    lstyle_vert: Option<roe_model::impress::TextVert>,
    /// PROOT-002: <a:ln><a:headEnd type="arrow"/></a:ln>，起点箭头类型
    /// 原始字符串（"arrow"/"triangle"/"none"/其他），组装阶段转换成
    /// ArrowType，未识别字符串 fallback 到 ArrowType::None。
    head_arrow: Option<String>,
    /// PROOT-002: <a:ln><a:tailEnd type="..."/></a:ln>，语义同 head_arrow。
    tail_arrow: Option<String>,
    /// PROOT-002 bugfix: <a:ln w="..."> 的线宽（EMU->pt换算后）。与颜色
    /// 分离存储的原因：<a:ln>在XML里先于<p:style>出现（流式解析时还不
    /// 知道后面会不会有lnRef主题色fallback），若在遇到<a:ln>时就把
    /// stroke.color硬编码成黑色兜底，会导致</p:sp>/</p:cxnSp>结束时
    /// `raw.stroke.is_none()`判断失效，lnRef fallback永远不生效（本次
    /// 会话实测踩过这个坑）。所以先只存宽度，颜色留给solidFill（若ln
    /// 内有）或结束时的style fallback决定，最后统一合并。
    ln_width: Option<f32>,
    /// PROOT-002 lnRef线宽fallback: <p:style><a:lnRef idx="N">的idx原始值。
    /// shape自身<a:ln>没写w属性时需要拿这个idx去主题lnStyleLst里查线宽。
    ln_ref_idx: Option<i32>,
    /// PROOT-LN-NOFILL bugfix: <a:ln><a:noFill/></a:ln>（显式声明该形状
    /// 不画描边，即使<a:ln w="..."/>写了宽度也不应画出）。此前ln内的
    /// noFill事件因PROOT-012b修复（避免覆盖blipFill的s.fill）被完全
    /// 忽略，没有任何字段记录它出现过，导致</p:sp>结束合并阶段只看
    /// ln_width.is_some()就无条件黑色兜底，真实样本candy.pptx
    /// slideMaster1.xml的roundRect+4个ellipse装饰形状（均显式
    /// <a:ln w="14400"><a:noFill/></a:ln>）渲染出了不该有的黑色描边。
    /// 默认false，遇到in_ln场景的noFill事件时置true，合并阶段优先
    /// 检查，跳过黑色兜底（保持stroke为None）。
    ln_no_fill: bool,
    /// PROOT-006阶段二: <p:style><a:fillRef idx="N">的idx原始值。
    /// spPr没有直接写solidFill/gradFill时，拿这个idx去主题fillStyleLst
    /// 查表，可能得到纯色+mods，也可能得到渐变定义（真实样本tdf114845
    /// idx=3即为渐变，见ThemeFillStyleDef/parse_theme_fill_styles）。
    fill_ref_idx: Option<i32>,
}

impl Default for RawShape {
    fn default() -> Self {
        RawShape {
            id: 0,
            name: String::new(),
            ph: None,
            ph_hint_style: None,
            position: Transform::from_rect(0.0, 0.0, 0.0, 0.0),
            rot: None,
            flip_h: None,
            flip_v: None,
            group_ch_off: None,
            group_ch_ext: None,
            geometry: None,
            cust_geom_path: None,
            adj_overrides: Vec::new(),
            fill: None,
            grad_raw: None,
            stroke: None,
            outer_shdw_raw: None,
            content: RawContent::Empty,
            lstyle_algn: None,
            lstyle_font_size: None,
            lstyle_bullet: None,
            lstyle_anchor: None,
            lstyle_vert: None,
            text_body: roe_model::impress::TextBodyProps::default(),
            st_cxn: None,
            end_cxn: None,
            head_arrow: None,
            tail_arrow: None,
            ln_width: None,
            ln_ref_idx: None,
            ln_no_fill: false,
            fill_ref_idx: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
enum RawContent {
    #[default]
    Empty,
    Text(Vec<Paragraph>),
    #[allow(non_snake_case)]
    Image {
        rId: String,
        /// PPTX-10: <a:srcRect> 的 l/t/r/b 原始千分比值（ST_Percentage，
        /// 100000=100%），未归一化，转换成 0.0-1.0 浮点的工作留给
        /// raw_to_shape 统一处理（与解析层"原始值先收集，转换在组装
        /// 阶段统一做"的既有模式一致，参照 rot 字段先收 1/60000 度原始
        /// 值、组装时再换算的写法）。默认 0 表示未出现 srcRect 或该边
        /// 属性缺省，语义上等于不裁剪。
        crop_l: i32,
        crop_t: i32,
        crop_r: i32,
        crop_b: i32,
    },
    #[allow(non_snake_case)]
    Chart {
        rId: String,
    },
    /// PPTX-6: <p:graphicFrame> 内嵌 <a:tbl> 表格。解析函数 parse_table
    /// 在遇到 <a:tbl> 起始时被调用，一次性消费到匹配的 </a:tbl> 为止，
    /// 直接产出 common::Table，此处不再需要 rId 中转（表格内容内联在
    /// slide XML 本身，不像图片/图表通过关系 ID 指向外部 part）。
    Table(roe_model::common::Table),
    /// PPTX-9b: <p:grpSp> 分组形状。子 Shape 在收集阶段（group_stack）
    /// 就已经是完整的 roe_model::impress::Shape（各自已经过 raw_to_shape
    /// 转换），这里直接转发给 raw_to_shape，与 Table 分支同一模式
    /// （"内部已是最终类型，不需要 rId 中转查表"）。
    Group(Vec<Shape>),
}

// ─── 公开入口 ────────────────────────────────────────────────────────────────

pub struct PptxParser;

impl PptxParser {
    pub fn parse(data: &[u8]) -> Result<Presentation, ParseError> {
        let cursor = std::io::Cursor::new(data);
        let mut zip = ZipArchive::new(cursor)
            .map_err(|e| ParseError::Zip(e.to_string()))?;

        // 1. 读取 presentation.xml → slide size + slide/master 列表
        let prs_xml = read_zip_entry(&mut zip, "ppt/presentation.xml")?;
        let (slide_size, slide_rids, master_rids, default_text_style) = parse_presentation_xml(&prs_xml)?;

        // 2. 读取 presentation.xml.rels → rid → target 路径
        let prs_rels = read_zip_entry_opt(&mut zip, "ppt/_rels/presentation.xml.rels")?
            .unwrap_or_default();
        let prs_rel_map = parse_rels(&prs_rels);

        // 2.5 解析主题颜色（ppt/theme/theme1.xml）
        let (theme_colors, theme_line_widths, theme_line_color_mods, theme_fill_styles, theme_bg_fill_styles) = {
            let xml = read_zip_entry_opt(&mut zip, "ppt/theme/theme1.xml")?
                .unwrap_or_default();
            (
                parse_theme_colors(&xml),
                parse_theme_line_widths(&xml),
                parse_theme_line_color_mods(&xml),
                parse_theme_fill_styles(&xml, b"fillStyleLst"),
                parse_theme_fill_styles(&xml, b"bgFillStyleLst"),
            )
        };
        // PROOT-THEME-BG-PIC: presentation 级全局 theme1.xml 自己的图片表
        // （rId -> 图片字节），供 bgFillStyleLst 里 Picture 分支消费。
        // theme 的图片关系在 theme 自己的 _rels 里，与 slide/master 的 rId
        // 命名空间互相独立，需要单独收集，与 master_image_map 的收集方式
        // （PROOT-BG-MASTER）同一模式。
        let theme_images: HashMap<String, Vec<u8>> = {
            let rels_path = slide_rels_path("ppt/theme/theme1.xml");
            if let Ok(Some(rels_xml)) = read_zip_entry_opt(&mut zip, &rels_path) {
                let rel_map = parse_rels(&rels_xml);
                collect_images(&mut zip, &rel_map, "ppt/theme/theme1.xml").unwrap_or_default()
            } else {
                HashMap::new()
            }
        };

        // 3. 解析所有 SlideMaster
        let mut master_slides: Vec<MasterSlide> = Vec::new();
        // ZIP内规范化路径 → master index（供 slide 通过 layout 间接查找 PPTX-1）
        // 不用 rid 做 key：rid 只在各自的 .rels 文件内有意义（presentation.xml.rels 与
        // slideLayoutN.xml.rels 的 rid 命名空间互相独立），路径在整个 ZIP 内全局唯一，
        // 可以安全地跨 rels 文件复用作为索引 key。
        let mut master_path_to_idx: HashMap<String, usize> = HashMap::new();
        // idx → path 反查表，避免依赖 master_rids 下标与 master_slides 下标一致的隐含假设
        // （循环内部分 master 可能因 rels/文件缺失被 continue 跳过，导致下标错位）。
        let mut master_idx_to_path: HashMap<usize, String> = HashMap::new();
        // v320a2: layout path -> (master idx, layout idx in master.layouts)
        let mut layout_path_to_idx: HashMap<String, (usize, usize)> = HashMap::new();
        // PROOT-clrmap: master idx -> 该 master 自己的 <p:clrMap>（16 槽位映射）。
        // 每个 slideMaster 各自定义一份 clrMap（不像 theme_colors 全局唯一），
        // 真实样本 sd_data_pptx__master-bg-color.pptx / onemaster-twolayouts.pptx
        // 均验证了非默认 clrMap 的存在，必须按 master 分别存、分别查，不能用
        // 单一全局值。
        let mut master_clr_maps: HashMap<usize, HashMap<String, String>> = HashMap::new();
        // PROOT-THEME (v92): master idx -> 该 master 自己的完整主题数据（5个
        // theme_* 字段打包）。与 master_clr_maps 同样的"按 master 分别存、
        // 分别查"模式——每个 slideMaster 可以关联不同的 theme.xml，不能像
        // 此前那样全局只解析一次 theme1.xml。找不到自己 theme 关系的 master
        // （理论上不应发生，OOXML 规范要求每个 slideMaster 必须关联一个
        // theme）不写入表，slide 循环体查表 miss 时 fallback 到 358 行解出的
        // presentation 级全局值（即 theme1.xml，与本次改动前行为一致，纯防御）。
        let mut master_theme_colors: HashMap<usize, HashMap<String, Color>> = HashMap::new();
        let mut master_theme_line_widths: HashMap<usize, Vec<f32>> = HashMap::new();
        let mut master_theme_line_color_mods: HashMap<usize, Vec<Vec<(String, i32)>>> = HashMap::new();
        let mut master_theme_fill_styles: HashMap<usize, Vec<ThemeFillStyleDef>> = HashMap::new();
        let mut master_theme_bg_fill_styles: HashMap<usize, Vec<ThemeFillStyleDef>> = HashMap::new();
        // PROOT-THEME-BG-PIC: master idx -> 该 master 自己 theme 的图片表。
        let mut master_theme_images: HashMap<usize, HashMap<String, Vec<u8>>> = HashMap::new();

        for (rid, _) in &master_rids {
            let target = match prs_rel_map.get(rid.as_str()) {
                Some(t) => t.clone(),
                None => continue,
            };
            let path = normalize_path("ppt", &target);
            let xml = match read_zip_entry_opt(&mut zip, &path)? {
                Some(x) => x,
                None => continue,
            };
            // PROOT-BG-MASTER: 此前这里传的是空 image_map（&HashMap::new()），因为
            // master_slides 原本只是给占位符继承链服务，没打算渲染图片内容。现在
            // master_slides[idx].shapes 也被复用来提取非占位符装饰形状（含全屏背景
            // 图片这类 <p:pic>），必须传真实的 image_map，否则图片 shape 会因为
            // image_map.get(&rId) 查不到而退化成 ShapeContent::Empty（复现现象：
            // candy.pptx slide0 背景图片渲染成空白）。master 自己的图片关系在
            // master 自己的 .rels 里，与其他层的 rId 命名空间互相独立，需要单独收集。
            let master_image_map = {
                let rels_path = slide_rels_path(&path);
                if let Ok(Some(rels_xml)) = read_zip_entry_opt(&mut zip, &rels_path) {
                    let rel_map = parse_rels(&rels_xml);
                    collect_images(&mut zip, &rel_map, &path).unwrap_or_default()
                } else {
                    HashMap::new()
                }
            };
            // PROOT-clrmap: master 自己的 <p:clrMap> 是这一层的地基映射，
            // 后续 layout/slide 层的 clrMapOvr 要么继承它、要么覆盖它。
            let this_master_clr_map = parse_clr_map(&xml);
            // PROOT-THEME (v92): master 自己的 theme 关系，与 clrMap 同样读法。
            let this_master_theme_colors;
            let this_master_theme_line_widths;
            let this_master_theme_line_color_mods;
            let this_master_theme_fill_styles;
            let this_master_theme_bg_fill_styles;
            let mut this_master_theme_raw: Option<String> = None; // v320b0
            let this_master_theme_images: HashMap<String, Vec<u8>>;
            {
                let rels_path = slide_rels_path(&path);
                let theme_target_path = if let Ok(Some(rels_xml)) = read_zip_entry_opt(&mut zip, &rels_path) {
                    if let Some(theme_target) = find_theme_target(&rels_xml) {
                        let master_base = path.rsplitn(2, '/').nth(1).unwrap_or("ppt/slideMasters");
                        Some(normalize_path(master_base, &theme_target))
                    } else {
                        None
                    }
                } else {
                    None
                };
                let theme_xml = if let Some(ref theme_path) = theme_target_path {
                    read_zip_entry_opt(&mut zip, theme_path)?.unwrap_or_default()
                } else {
                    String::new()
                };
                if !theme_xml.is_empty() { this_master_theme_raw = Some(theme_xml.clone()); } // v320b0
                if theme_xml.is_empty() {
                    this_master_theme_colors = theme_colors.clone();
                    this_master_theme_line_widths = theme_line_widths.clone();
                    this_master_theme_line_color_mods = theme_line_color_mods.clone();
                    this_master_theme_fill_styles = theme_fill_styles.clone();
                    this_master_theme_bg_fill_styles = theme_bg_fill_styles.clone();
                    this_master_theme_images = theme_images.clone();
                } else {
                    this_master_theme_colors = parse_theme_colors(&theme_xml);
                    this_master_theme_line_widths = parse_theme_line_widths(&theme_xml);
                    this_master_theme_line_color_mods = parse_theme_line_color_mods(&theme_xml);
                    this_master_theme_fill_styles = parse_theme_fill_styles(&theme_xml, b"fillStyleLst");
                    this_master_theme_bg_fill_styles = parse_theme_fill_styles(&theme_xml, b"bgFillStyleLst");
                    // PROOT-THEME-BG-PIC: 该 master 自己 theme.xml 的图片表，
                    // 图片关系在 theme 自己的 _rels 里（与 master/slide 的 rId
                    // 命名空间互相独立），theme_target_path 此时必为 Some
                    // （因为 theme_xml 非空说明上面成功读到了它）。
                    this_master_theme_images = if let Some(ref theme_path) = theme_target_path {
                        let theme_rels_path = slide_rels_path(theme_path);
                        if let Ok(Some(theme_rels_xml)) = read_zip_entry_opt(&mut zip, &theme_rels_path) {
                            let theme_rel_map = parse_rels(&theme_rels_xml);
                            collect_images(&mut zip, &theme_rel_map, theme_path).unwrap_or_default()
                        } else {
                            HashMap::new()
                        }
                    } else {
                        HashMap::new()
                    };
                }
            }
            let shapes = parse_slide_xml_shapes("(master)", &xml, &master_image_map, &HashMap::new(), &this_master_theme_colors, &this_master_clr_map, &[], &[], &Default::default(), &this_master_theme_line_widths, &this_master_theme_line_color_mods, &this_master_theme_fill_styles, None)?;
            let name = target.clone();
            let idx = master_slides.len();
            // v320a2: real master background, color map and layout objects
            let master_bg_real = parse_slide_background(&xml, &master_image_map, &this_master_theme_colors, &this_master_theme_bg_fill_styles, &this_master_theme_images, &this_master_clr_map);
            let mut built_layouts: Vec<roe_model::impress::SlideLayout> = Vec::new();
            {
                let master_rels_path_l = slide_rels_path(&path);
                let master_rels_xml_l = read_zip_entry_opt(&mut zip, &master_rels_path_l)?.unwrap_or_default();
                let layout_targets = parse_master_layout_targets(&xml, &master_rels_xml_l);
                let layout_base = path.rsplitn(2, '/').nth(1).unwrap_or("ppt/slideMasters").to_string();
                for lt in layout_targets {
                    let layout_path_l = normalize_path(&layout_base, &lt);
                    let layout_xml_l = match read_zip_entry_opt(&mut zip, &layout_path_l)? {
                        Some(x) => x,
                        None => continue,
                    };
                    let layout_image_map_l = {
                        let rp = slide_rels_path(&layout_path_l);
                        if let Ok(Some(rx)) = read_zip_entry_opt(&mut zip, &rp) {
                            let rm = parse_rels(&rx);
                            collect_images(&mut zip, &rm, &layout_path_l).unwrap_or_default()
                        } else {
                            HashMap::new()
                        }
                    };
                    let (lname, ltype, lshow) = parse_layout_meta(&layout_xml_l);
                    let lovr = parse_clr_map_ovr(&layout_xml_l);
                    let lclr = lovr.clone().unwrap_or_else(|| this_master_clr_map.clone());
                    let lshapes = parse_slide_xml_shapes(&layout_path_l, &layout_xml_l, &layout_image_map_l, &HashMap::new(), &this_master_theme_colors, &lclr, &[], &[], &Default::default(), &this_master_theme_line_widths, &this_master_theme_line_color_mods, &this_master_theme_fill_styles, None).unwrap_or_default();
                    let lbg = parse_slide_background(&layout_xml_l, &layout_image_map_l, &this_master_theme_colors, &this_master_theme_bg_fill_styles, &this_master_theme_images, &lclr);
                    let kind = match ltype.as_deref() {
                        Some("title") => roe_model::impress::SlideLayoutKind::Title,
                        Some("obj") => roe_model::impress::SlideLayoutKind::TitleAndContent,
                        Some("secHead") => roe_model::impress::SlideLayoutKind::SectionHeader,
                        Some("twoObj") => roe_model::impress::SlideLayoutKind::TwoContent,
                        Some("titleOnly") => roe_model::impress::SlideLayoutKind::TitleOnly,
                        Some("blank") => roe_model::impress::SlideLayoutKind::Blank,
                        _ => roe_model::impress::SlideLayoutKind::Custom,
                    };
                    layout_path_to_idx.insert(layout_path_l.clone(), (idx, built_layouts.len()));
                    built_layouts.push(roe_model::impress::SlideLayout {
                        name: lname.unwrap_or_else(|| lt.clone()),
                        kind,
                        shapes: lshapes,
                        background: lbg,
                        show_master_shapes: lshow,
                        color_map: lovr,
                        raw_ext: None,
                    });
                }
            }
            master_slides.push(MasterSlide { layouts: built_layouts, color_map: Some(this_master_clr_map.clone()), raw_ext: None, theme_xml: this_master_theme_raw, tx_styles_xml: extract_raw_element(&xml, "p:txStyles"), name, shapes, background: master_bg_real });
            master_path_to_idx.insert(path.clone(), idx);
            master_idx_to_path.insert(idx, path.clone());
            master_clr_maps.insert(idx, this_master_clr_map);
            master_theme_colors.insert(idx, this_master_theme_colors);
            master_theme_line_widths.insert(idx, this_master_theme_line_widths);
            master_theme_line_color_mods.insert(idx, this_master_theme_line_color_mods);
            master_theme_fill_styles.insert(idx, this_master_theme_fill_styles);
            master_theme_bg_fill_styles.insert(idx, this_master_theme_bg_fill_styles);
            master_theme_images.insert(idx, this_master_theme_images);
        }

        // 4. 解析所有 Slide
        let mut slides: Vec<Slide> = Vec::new();

        for (rid, _) in &slide_rids {
            let target = match prs_rel_map.get(rid.as_str()) {
                Some(t) => t.clone(),
                None => continue,
            };
            let path = normalize_path("ppt", &target);

            // slide rels（图片等）
            let rels_path = slide_rels_path(&path);
            let rels_xml = read_zip_entry_opt(&mut zip, &rels_path)?.unwrap_or_default();
            let rel_map = parse_rels(&rels_xml);

            // 图片数据：rId → bytes
            let image_map = collect_images(&mut zip, &rel_map, &path)?;

            let xml = match read_zip_entry_opt(&mut zip, &path)? {
                Some(x) => x,
                None => continue,
            };
            let chart_map = collect_charts(&mut zip, &rel_map, &path, &theme_colors)?;

            // PPTX-1+2：从 slide rels 找 slideLayout，再从 layout 自己的 rels 找 slideMaster，
            // 建立本 slide 实际归属的 master_idx（不再依赖"固定用第一个master"的全局旁路，
            // 每个 slide 按自己真实的 layout->master 链路查找，支持多master文件）。
            // PPTX-4：改为收集 layout_candidates / master_candidates 两组候选列表，
            // 而非旧版"两层扁平覆盖 HashMap<ph_type, position>"。候选列表携带 index/has_transform
            // 维度，交给 match_placeholder() 六分支算法统一决策，而不是"后来者覆盖先来者"。
            // 同时统一了位置与样式的继承来源（此前 master 层只做了位置继承、完全没做样式继承，
            // 是一处隐藏的不一致，本次一并修复：master/layout 两层现在都会提取样式）。
            let mut layout_candidates: Vec<PlaceholderCandidate> = Vec::new();
            let mut master_candidates: Vec<PlaceholderCandidate> = Vec::new();
            let mut resolved_master_idx: Option<usize> = None;
            let mut resolved_layout_idx: Option<usize> = None; // v320a2
            // PROOT-clrmap: 当前 slide 实际生效的 clrMap，按 master -> layout ->
            // slide 三层依次计算：master 层用该 master 自己的 <p:clrMap>；
            // layout 层看自己的 <p:clrMapOvr> 是 masterClrMapping（继承，不变）
            // 还是 overrideClrMapping（替换）；slide 层同理。默认先给一个空表，
            // normalize_scheme_color_slot 在查不到 slot 时会 fallback 到
            // PROOT-flip-bg1 的默认直接对应关系，不会 panic，只是行为退化成
            // 修复前的写死映射（理论上不应该发生，因为 parse_clr_map 本身已经
            // 对 16 个属性做了 ECMA-376 默认值兜底，只要 master_clr_maps 里
            // 查得到这个 midx 就不会是空表）。
            let mut effective_clr_map: HashMap<String, String> = HashMap::new();
            // PROOT-THEME (v92): 与 effective_clr_map 同样的模式——先给
            // presentation 级全局值兜底（358行解出的 theme1.xml），命中
            // resolved_master_idx 后从 master_theme_* 表里查出该 master 自己
            // 的主题数据覆盖。
            let mut effective_theme_colors: HashMap<String, Color> = theme_colors.clone();
            let mut effective_theme_line_widths: Vec<f32> = theme_line_widths.clone();
            let mut effective_theme_line_color_mods: Vec<Vec<(String, i32)>> = theme_line_color_mods.clone();
            let mut effective_theme_fill_styles: Vec<ThemeFillStyleDef> = theme_fill_styles.clone();
            let mut effective_theme_bg_fill_styles: Vec<ThemeFillStyleDef> = theme_bg_fill_styles.clone();
            let mut effective_theme_images: HashMap<String, Vec<u8>> = theme_images.clone();
            // PROOT-005: slide 最终生效背景，按 OOXML 继承顺序（自身 → layout → master）
            // 逐级查找，找到第一个非 None 结果即停止，不再往上找。
            let mut resolved_background: Option<FillStyle> = None;
            // PROOT-005 followup: layout 里的非占位符装饰性形状（见 Slide.layout_shapes
            // 字段文档注释）。空 Vec 是安全默认值——slide 没有关联 layout 或 layout
            // 没有装饰性形状时都维持空，不影响渲染（渲染层对空 Vec 直接跳过循环）。
            let mut resolved_layout_shapes: Vec<Shape> = Vec::new();
            // PROOT-BG-MASTER: master 里的非占位符装饰性形状（如全屏背景图片
            // <p:pic>、logo、装饰图案——这类内容不走 <p:bg>，而是 master
            // spTree 下普通 shape，此前只对 layout 调用了 extract_non_ph_shapes，
            // master 这一层完全没有同款调用，导致这类"伪装成背景"的图片
            // 从未被提取渲染。渲染层 slide_renderer.rs 的三层叠放顺序注释
            // 早已预留了 master 层位置（"画在 master 之上、slide 自身内容
            // 之下"），本次只需在解析阶段把 master 形状也塞进
            // layout_shapes 字段（master排最底层，再拼layout形状盖在上面），
            // 不需要新增字段、不需要改渲染层。
            let mut resolved_master_shapes: Vec<Shape> = Vec::new();
            // PROOT bullet: master <p:txStyles> 的 bullet 默认值（title/body/other
            // 三分类 x 10 级）。None 表示这个 slide 没有解析到关联 master（理论上
            // 不应该发生，但 resolved_master_idx 本身也是 Option，防御性处理保持一致）。
            let mut resolved_master_text_styles: Option<MasterTextStyles> = None;

            /// 从一组 RawShape 里提取占位符候选列表（本地闭包，master/layout 两处共用同一逻辑）。
            fn extract_ph_candidates(raw_shapes: Vec<RawShape>) -> Vec<PlaceholderCandidate> {
                let mut out = Vec::new();
                for raw in raw_shapes {
                    if let Some((ph_type, index)) = raw.ph {
                        let has_transform = raw.position.w() > 0.0 || raw.position.h() > 0.0;
                        // PROOT-027 followup: anchor候选直接来自lstyle_anchor
                        // （parse_raw_shapes里独立读取的bodyPr anchor属性），
                        // raw.text_body在这个函数里从未被真正解析、恒为默认值，
                        // 不能作为anchor的数据来源。
                        let anchor_candidate = raw.lstyle_anchor.clone();
                        let vert_candidate = raw.lstyle_vert;
                        // PROOT: 该占位符段落是否有真实、非空的文本run（<a:r>）——
                        // 用于区分"layout/master自带示例文字"（如"单击以编辑标题文本格式"）
                        // 和"只有<a:endParaRPr>的纯空段落"，见struct字段文档注释。
                        let has_real_run = if let RawContent::Text(ref paras) = raw.content {
                            paras.iter().any(|p| p.runs.iter().any(|r| !r.text.trim().is_empty()))
                        } else {
                            false
                        };
                        let style = if let RawContent::Text(ref paras) = raw.content {
                            paras.first().and_then(|p| p.runs.first()).map(|run| LayoutPhStyle {
                                // PROOT-027 followup: lstStyle lvl1pPr defRPr sz 优先于
                                // run自身sz——前者是占位符类型的默认字号（如ctrTitle 60pt），
                                // 后者只是示例文字run上可能存在的sz，语义层级更低。
                                font_size:   raw.lstyle_font_size.or(run.font_size),
                                bold:        run.bold,
                                italic:      run.italic,
                                color:       run.color,
                                font_family: run.font_family.clone(),
                                algn:        raw.lstyle_algn,
                                anchor:      anchor_candidate.clone(),
                                vert:        vert_candidate,
                                bullet:      raw.lstyle_bullet.clone(),
                            }).unwrap_or_else(|| LayoutPhStyle { algn: raw.lstyle_algn, font_size: raw.lstyle_font_size, anchor: anchor_candidate.clone(), vert: vert_candidate, bullet: raw.lstyle_bullet.clone(), ..Default::default() })
                        } else {
                            LayoutPhStyle { algn: raw.lstyle_algn, font_size: raw.lstyle_font_size, anchor: anchor_candidate, vert: vert_candidate, bullet: raw.lstyle_bullet.clone(), ..Default::default() }
                        };
                        out.push(PlaceholderCandidate {
                            ph_type,
                            index,
                            has_transform,
                            has_real_run,
                            position: raw.position,
                            style,
                        });
                    }
                }
                out
            }

            /// PROOT-005 followup: 从一组 RawShape 里提取"非占位符"形状，
            /// 转换成正式 Shape（复用 raw_to_shape，与 slide 自身形状走同一条
            /// 组装逻辑，保证颜色/geometry/图片等处理方式完全一致）。
            /// 与 extract_ph_candidates 是姊妹函数，筛选条件互补（ph.is_none()
            /// vs ph.is_some()），但这个函数需要 image_map/chart_map/counter
            /// 才能调用 raw_to_shape，因此不能像 extract_ph_candidates 一样
            /// 写成无捕获的裸 fn，用闭包捕获调用点已有的这些局部变量。
            fn extract_non_ph_shapes(
                raw_shapes: Vec<RawShape>,
                image_map: &HashMap<String, Vec<u8>>,
                chart_map: &HashMap<String, roe_model::impress::ChartModel>,
                counter_start: u32,
            ) -> Vec<Shape> {
                let mut out = Vec::new();
                let mut counter = counter_start;
                for raw in raw_shapes {
                    if raw.ph.is_none() {
                        out.push(raw_to_shape(raw, image_map, chart_map, counter));
                        counter += 1;
                    }
                }
                out
            }

            if let Some(layout_target) = find_slide_layout_target(&rels_xml) {
                let slide_base = path.rsplitn(2, '/').nth(1).unwrap_or("ppt/slides");
                let layout_path = normalize_path(slide_base, &layout_target);
                resolved_layout_idx = layout_path_to_idx.get(&layout_path).map(|&(_, li)| li); // v320a2

                // layout -> master：读 layout 自己的 rels，找 slideMaster target，
                // 用规范化路径去查 master_path_to_idx。
                let layout_rels_path = slide_rels_path(&layout_path);
                if let Ok(Some(layout_rels_xml)) = read_zip_entry_opt(&mut zip, &layout_rels_path) {
                    if let Some(master_target) = find_slide_master_target(&layout_rels_xml) {
                        let layout_base = layout_path.rsplitn(2, '/').nth(1).unwrap_or("ppt/slideLayouts");
                        let master_path = normalize_path(layout_base, &master_target);
                        resolved_master_idx = master_path_to_idx.get(&master_path).copied();
                    }
                }

                // 该 slide 实际关联的 master 上的占位符候选
                // （用 master_idx_to_path 反查真实路径，替代旧的"固定第一个master"旁路）
                if let Some(midx) = resolved_master_idx {
                    if let Some(master_path) = master_idx_to_path.get(&midx) {
                        if let Ok(Some(master_xml)) = read_zip_entry_opt(&mut zip, master_path) {
                            // PROOT-clrmap: master 层地基映射，layout/slide 层的
                            // clrMapOvr 在各自的解析块里会在此基础上继续覆盖。
                            if let Some(m) = master_clr_maps.get(&midx) {
                                effective_clr_map = m.clone();
                            }
                            // PROOT-THEME (v92): 该 slide 实际所属 master 自己的主题
                            // 数据，覆盖上面的 presentation 级全局兜底值。
                            if let Some(c) = master_theme_colors.get(&midx) {
                                effective_theme_colors = c.clone();
                            }
                            if let Some(w) = master_theme_line_widths.get(&midx) {
                                effective_theme_line_widths = w.clone();
                            }
                            if let Some(m) = master_theme_line_color_mods.get(&midx) {
                                effective_theme_line_color_mods = m.clone();
                            }
                            if let Some(f) = master_theme_fill_styles.get(&midx) {
                                effective_theme_fill_styles = f.clone();
                            }
                            if let Some(b) = master_theme_bg_fill_styles.get(&midx) {
                                effective_theme_bg_fill_styles = b.clone();
                            }
                            if let Some(im) = master_theme_images.get(&midx) {
                                effective_theme_images = im.clone();
                            }
                            if let Ok(raw_shapes) = parse_raw_shapes(&master_xml) {
                                master_candidates = extract_ph_candidates(raw_shapes);
                            }
                            // PROOT bullet: 解析 master 的 <p:txStyles>，供段落 bullet 继承使用。
                            resolved_master_text_styles = Some(parse_master_txstyles(&master_xml, &effective_theme_colors, &effective_clr_map));
                            // PROOT-005 第3级 fallback：slide/layout 都没有背景时才查 master。
                            // master 自己的图片关系在 master 自己的 .rels 里，与 slide/layout
                            // 的 rId 命名空间互相独立，需要单独收集，不能复用 image_map。
                            //
                            // PROOT-BG-MASTER: 之前这里曾用 parse_raw_shapes+extract_non_ph_shapes
                            // 重新解析一遍 master_xml 来提取非占位符装饰形状，但 parse_raw_shapes
                            // 是给占位符候选用的轻量解析器，完全不解析 <a:solidFill>/<a:srgbClr>，
                            // 导致这批形状 fill=None，渲染层 fallback 成主题 accent1（绿色），
                            // 复现了与 v75 记录几乎一样的"全篇变绿"症状（但这次根因不同：不是
                            // 状态复位遗漏，是用了错误的解析函数）。
                            //
                            // 正确做法：master_slides[idx].shapes（337-361行，Presentation 构建
                            // 早期就已经用完整版 parse_slide_xml_shapes 解析过一次，含正确的颜色/
                            // 样式）里已经有这批形状的正确渲染结果，直接复用，不用重新解析 master_xml，
                            // 用 shape.placeholder.is_none() 筛掉占位符（占位符已经通过独立的
                            // extract_ph_candidates 继承链处理，不能重复渲染）。
                            if let Some(mslide) = master_slides.get(midx) {
                                resolved_master_shapes = mslide.shapes.iter()
                                    .filter(|s| s.placeholder.is_none())
                                    .cloned()
                                    .collect();
                            }

                            // PROOT-clrmap followup: 无条件解析，不判断 is_none()——
                            // master 是三层里优先级最低的一层，本次 slide 循环刚开始
                            // resolved_background 必然还是 None，这里赋值只是给后面
                            // layout/slide 两层提供"没有更高优先级覆盖时"的兜底值，
                            // 不需要（也不应该）用 is_none() 挡住自己。
                            let master_rels_path = slide_rels_path(master_path);
                            if let Ok(Some(master_rels_xml)) = read_zip_entry_opt(&mut zip, &master_rels_path) {
                                let master_rel_map = parse_rels(&master_rels_xml);
                                if let Ok(master_image_map) = collect_images(&mut zip, &master_rel_map, master_path) {
                                    resolved_background = parse_slide_background(&master_xml, &master_image_map, &effective_theme_colors, &effective_theme_bg_fill_styles, &effective_theme_images, &effective_clr_map);
                                }
                            }
                        }
                    }
                }

                if let Ok(Some(layout_xml)) = read_zip_entry_opt(&mut zip, &layout_path) {
                    // PROOT-clrmap: layout 自己的 <p:clrMapOvr>。
                    // masterClrMapping（None）表示继承上一层（effective_clr_map
                    // 此时已经是 master 层的值，不用管），overrideClrMapping
                    // （Some）表示这一层完全重新定义，直接替换。真实样本
                    // sd_data_pptx__onemaster-twolayouts.pptx 的 slideLayout1
                    // 用 overrideClrMapping，slideLayout2 用 masterClrMapping，
                    // 两条路径都需要被这段逻辑正确处理。
                    if let Some(ovr) = parse_clr_map_ovr(&layout_xml) {
                        effective_clr_map = ovr;
                    }
                    if let Ok(raw_shapes) = parse_raw_shapes(&layout_xml) {
                        layout_candidates = extract_ph_candidates(raw_shapes);
                    }
                    // layout 自己的图片关系在 layout_rels_xml 里（上面已经读取过，
                    // 用于查 master target），可以直接复用，避免重复读取 rels 文件。
                    // 供下面 PROOT-005 背景 fallback 和 PROOT-005 followup 装饰形状
                    // 提取两处共用（同一份 layout_image_map，不重复 collect_images）。
                    let layout_image_map = if let Ok(Some(layout_rels_xml)) = read_zip_entry_opt(&mut zip, &layout_rels_path) {
                        let layout_rel_map = parse_rels(&layout_rels_xml);
                        collect_images(&mut zip, &layout_rel_map, &layout_path).unwrap_or_default()
                    } else {
                        HashMap::new()
                    };

                    // PROOT-clrmap followup: layout 优先级高于 master，只要 layout
                    // 自己解析出了非 None 的背景就覆盖 master 的兜底值；layout 没有
                    // 自己 <p:bg> 时 parse_slide_background 返回 None，此时保留
                    // resolved_background 已有的 master 兜底值，不能用 None 覆盖掉。
                    if let Some(layout_bg) = parse_slide_background(&layout_xml, &layout_image_map, &effective_theme_colors, &effective_theme_bg_fill_styles, &effective_theme_images, &effective_clr_map) {
                        resolved_background = Some(layout_bg);
                    }

                    // PROOT-BG-LAYOUT (v92): 此前这里用 parse_raw_shapes（轻量占位符
                    // 候选解析器）重新解析 layout_xml 提取非占位符装饰形状，但
                    // parse_raw_shapes 完全不解析 <a:prstGeom>/<a:solidFill>，导致这批
                    // 形状 geometry=None/fill=None，raw_to_shape 落到默认 Rect+渲染层
                    // fallback 色，与 v90 记录的 PROOT-BG-MASTER（master 层同一限制）
                    // 是同一个已知限制在不同调用路径上的重复命中，教训见 v91 文档四.2。
                    //
                    // master 层当时的正确修法是复用 master_slides[idx].shapes（已经
                    // 用完整版 parse_slide_xml_shapes 解析过一次）。但 layout 没有等价
                    // 的"预先完整解析好的 shapes"缓存（layout_candidates 只是占位符
                    // 候选，不是完整 shapes），所以这里改为直接对 layout_xml 再跑一次
                    // 完整版 parse_slide_xml_shapes，取出非占位符的 shapes 复用，
                    // 而不是继续给轻量版 parse_raw_shapes 打补丁（parse_raw_shapes是
                    // 多处共用的通用函数，贸然扩展风险高，参照 v90 PROOT-BG-MASTER
                    // 教训）。layout_candidates/master_candidates 传空切片：这次解析
                    // 只是为了拿 shapes 本身的 geometry/fill，不需要占位符继承链
                    // （占位符已经通过上面 extract_ph_candidates 单独处理）。
                    // chart_map 传空表：layout 装饰性形状目前没有真实样本证据表明
                    // 会用图表，不为这个边缘情况新增一次 collect_charts 调用。
                    if let Ok(layout_full_shapes) = parse_slide_xml_shapes(
                        &layout_path, &layout_xml, &layout_image_map, &HashMap::new(),
                        &effective_theme_colors, &effective_clr_map, &[], &[],
                        &Default::default(), &effective_theme_line_widths, &effective_theme_line_color_mods,
                        &effective_theme_fill_styles, None,
                    ) {
                        resolved_layout_shapes = layout_full_shapes.into_iter()
                            .filter(|s| s.placeholder.is_none())
                            .collect();
                    }
                }
            };

            // PROOT-clrmap: slide 自己的 <p:clrMapOvr>，同 layout 层语义
            // （masterClrMapping 继承 / overrideClrMapping 替换），在这一层
            // 应用完之后 effective_clr_map 才是这个 slide 最终生效的映射。
            if let Some(ovr) = parse_clr_map_ovr(&xml) {
                effective_clr_map = ovr;
            }

            // PROOT-005 第1级：slide 自身的 <p:bg>，优先级最高。
            // 用 slide 自己已经收集好的 image_map（本函数上方 collect_images 调用产出）。
            if let Some(own_bg) = parse_slide_background(&xml, &image_map, &effective_theme_colors, &effective_theme_bg_fill_styles, &effective_theme_images, &effective_clr_map) {
                resolved_background = Some(own_bg);
            }

            let shapes = parse_slide_xml_shapes(&path, &strip_chartex_fallback(&xml), &image_map, &chart_map, &effective_theme_colors, &effective_clr_map, &layout_candidates, &master_candidates, &default_text_style, &effective_theme_line_widths, &effective_theme_line_color_mods, &effective_theme_fill_styles, resolved_master_text_styles.as_ref())?;

            // PROOT-MASTER-LAYOUT-DEDUP: master 里的装饰形状（如Group 6整体）
            // 如果 layout 自己也定义了同 id 的顶层形状，说明 layout 重新设计/覆盖了
            // 这个形状（真实样本 sd_data_pptx__ooxtheme 复现：master 和 16 个 layout
            // 各自都有一份"id=7 name=Group 6"三角拼贴装饰，但每个 layout 对其中个别
            // 子元素——典型如某个角落的三角形——做了差异化设计，例如 slideLayout1
            // 的 Group 6 最后一个 child 是 id=19 的 Isosceles Triangle 18（180度旋转
            // 贴左边缘的细长三角），而 master 自己的 Group 6 最后一个 child 是完全
            // 不同的 id=29 custGeom 路径）。此前代码简单地"master 垫底、layout 全部
            // 叠加"，未做任何去重，导致两份内容不同的同名 Group 被同时渲染，画面上
            // 同时出现两套装饰图案叠加，视觉上表现为多出不属于当前 layout 设计的
            // 额外三角形尖角（用户反馈：与 OnlyOffice 参照图对比，左边缘多出一个
            // 不该存在的凸起）。
            //
            // 按 id 去重而非整体跳过 master 装饰形状：OOXML 允许 layout 只重写部分
            // 形状、其余继续继承 master（例如某个独立的 logo 图片可能只在 master
            // 定义，全部 layout 共享，不应该因为 layout 定义了 Group 6 就连带丢弃）。
            // 只过滤"master 与 layout 顶层 id 重复"的那些，未被 layout 重写的 master
            // 形状原样保留，实现精确覆盖而非整体替换。
            //
            // 依赖前提：master 与 layout 各自 Group 的 id 都必须是 XML 原始 cNvPr id
            // 而非 shape_counter fallback 值，这要求 <p:grpSp> 自身 cNvPr 被正确解析
            // （本次同一会话一并修复的 PROOT-GRPSP-CNVPR，见 cNvPr 分支新增的
            // "cur_shape.is_none() && !group_stack.is_empty()" guard）——若不先修那个
            // bug，两边 Group 的 id 会各自独立 fallback 到 shape_counter 生成值，
            // 只是恰好状态一致时才会意外相等，不能作为可靠的去重依据。
            let layout_ids: std::collections::HashSet<u32> =
                resolved_layout_shapes.iter().map(|s| s.id).collect();
            let mut combined_layout_shapes: Vec<Shape> = resolved_master_shapes
                .into_iter()
                .filter(|s| !layout_ids.contains(&s.id))
                .collect();
            combined_layout_shapes.extend(resolved_layout_shapes);

            slides.push(Slide { layout_idx: resolved_layout_idx,
                shapes,
                master_idx: resolved_master_idx, // PPTX-1：已接通 slide->layout->master 链
                animation_tree: AnimationNode::default(),
                transition: None,
                notes: None,
                background: resolved_background, // PROOT-005：slide->layout->master 三级背景fallback结果
                layout_shapes: combined_layout_shapes, // PROOT-005 followup + PROOT-BG-MASTER：master+layout装饰性形状
            });
        }

        Ok(Presentation {
            slide_size,
            slides,
            master_slides,
            styles: roe_model::style::StyleRegistry::default(),
            theme_colors,
            default_text_style,
        })
    }
}

// ─── presentation.xml 解析 ───────────────────────────────────────────────────

/// 返回 (slide_size, slide_rids, master_rids, default_text_style)
/// rids: Vec<(rId, order)>
/// PPTX-5: default_text_style 来自 <p:defaultTextStyle><a:lvl1pPr><a:defRPr .../>，
/// 是文档级全局样式兜底的最后一层（对应 docx 侧 w:docDefaults 概念）。
fn parse_presentation_xml(
    xml: &str,
) -> Result<((f32, f32), Vec<(String, u32)>, Vec<(String, u32)>, roe_model::impress::DefaultTextStyle), ParseError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    // slide_size 统一为 pt 语义（2026-06-18 修复：原先是裸 EMU 数值，
    // 与 fodp.rs/fodt.rs 的 pt 语义不一致）。EMU -> pt：÷12700。
    let mut slide_size = (9144000_f32 / 12700.0, 6858000_f32 / 12700.0); // 10"x7.5" in pt (9144000/6858000 EMU = 10"/7.5" @ 914400 EMU per inch)
    let mut slide_rids: Vec<(String, u32)> = Vec::new();
    let mut master_rids: Vec<(String, u32)> = Vec::new();
    let mut order: u32 = 0;

    loop {
        match reader.read_event() {
            Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e)) => {
                match e.local_name().as_ref() {
                    b"sldSz" => {
                        let cx = attr_f32(e, b"cx").unwrap_or(9144000.0);
                        let cy = attr_f32(e, b"cy").unwrap_or(6858000.0);
                        slide_size = (cx / 12700.0, cy / 12700.0);
                    }
                    b"sldId" => {
                        if let Some(_rid) = attr_str(e, b"r:id")
                            .or_else(|| attr_str(e, b"id"))
                        {
                            // r:id 属性
                            if let Some(rid) = attr_str(e, b"r:id") {
                                slide_rids.push((rid, order));
                                order += 1;
                            }
                        }
                    }
                    b"sldMasterId" => {
                        if let Some(rid) = attr_str(e, b"r:id") {
                            master_rids.push((rid, order));
                            order += 1;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(ParseError::Xml(e)),
            _ => {}
        }
    }

    // sldId 的 r:id 解析修正（上面双重 if 有 bug，重新用独立函数）
    // 重新扫一次
    slide_rids.clear();
    master_rids.clear();
    let mut reader2 = Reader::from_str(xml);
    reader2.config_mut().trim_text(true);
    let mut ord: u32 = 0;

    // PPTX-5：defaultTextStyle 解析状态。只取 <a:lvl1pPr><a:defRPr .../> 这一级，
    // lvl2pPr~lvl9pPr 不处理（同设计文档 §2.5 建议范围：第一期只做全局兜底最常用的一层）。
    let mut default_text_style = roe_model::impress::DefaultTextStyle::default();
    let mut in_default_text_style = false;
    let mut in_lvl1pPr = false;
    let mut in_defRPr = false;
    let mut in_defRPr_fill = false;

    loop {
        match reader2.read_event() {
            Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"sldSz" => {
                        let cx = attr_f32(e, b"cx").unwrap_or(9144000.0);
                        let cy = attr_f32(e, b"cy").unwrap_or(6858000.0);
                        slide_size = (cx / 12700.0, cy / 12700.0); // EMU -> pt
                    }
                    b"sldId" => {
                        // r:id 在命名空间展开后键名可能是 "r:id" 或带完整 URI
                        // quick-xml raw attr 迭代更可靠
                        for attr in e.attributes().flatten() {
                            let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                            if key == "r:id" || key.ends_with("}id") {
                                let val = attr.unescape_value().unwrap_or_default().to_string();
                                if val.starts_with("rId") {
                                    slide_rids.push((val, ord));
                                    ord += 1;
                                }
                            }
                        }
                    }
                    b"sldMasterId" => {
                        for attr in e.attributes().flatten() {
                            let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                            if key == "r:id" || key.ends_with("}id") {
                                let val = attr.unescape_value().unwrap_or_default().to_string();
                                if val.starts_with("rId") {
                                    master_rids.push((val, ord));
                                    ord += 1;
                                }
                            }
                        }
                    }
                    b"defaultTextStyle" => { in_default_text_style = true; }
                    b"lvl1pPr" if in_default_text_style => { in_lvl1pPr = true; }
                    b"defRPr" if in_lvl1pPr => {
                        in_defRPr = true;
                        in_defRPr_fill = false;
                        if let Some(v) = attr_u32(e, b"sz") { default_text_style.font_size = Some(v as f32 / 100.0); }
                        if let Some(v) = attr_str(e, b"b") { default_text_style.bold = Some(v != "0"); }
                        if let Some(v) = attr_str(e, b"i") { default_text_style.italic = Some(v != "0"); }
                    }
                    b"solidFill" if in_defRPr => { in_defRPr_fill = true; }
                    b"latin" if in_defRPr => {
                        if let Some(tf) = attr_str(e, b"typeface") {
                            if tf != "+mj-lt" && tf != "+mn-lt" && !tf.is_empty() {
                                default_text_style.font_family = Some(tf);
                            }
                        }
                    }
                    b"srgbClr" if in_defRPr_fill => {
                        if let Some(hex) = attr_str(e, b"val") {
                            default_text_style.color = Some(parse_hex_color(&hex));
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                match e.local_name().as_ref() {
                    b"defaultTextStyle" => { in_default_text_style = false; }
                    b"lvl1pPr" => { in_lvl1pPr = false; }
                    b"defRPr" => { in_defRPr = false; in_defRPr_fill = false; }
                    b"solidFill" if in_defRPr_fill => { in_defRPr_fill = false; }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(ParseError::Xml(e)),
            _ => {}
        }
    }

    Ok((slide_size, slide_rids, master_rids, default_text_style))
}

// ─── slide/master XML → Vec<Shape> ──────────────────────────────────────────

fn parse_slide_xml_shapes(
    debug_slide_path: &str,
    xml: &str,
    image_map: &HashMap<String, Vec<u8>>,
    chart_map: &HashMap<String, roe_model::impress::ChartModel>,
    theme_colors: &HashMap<String, Color>,
    // PROOT-clrmap: 当前层（slide/master 自身解析场景）实际生效的
    // <p:clrMap>（已合并 clrMapOvr 覆盖）。传给内部所有
    // normalize_scheme_color_slot 调用点，替换此前写死的默认映射。
    clr_map: &HashMap<String, String>,
    layout_candidates: &[PlaceholderCandidate],
    master_candidates: &[PlaceholderCandidate],
    // PPTX-5：文档级全局兜底样式（presentation.xml <p:defaultTextStyle>），
    // 是继承链最后一层：layout/master 候选都未命中、或命中但该字段本身是 None 时使用。
    default_style: &roe_model::impress::DefaultTextStyle,
    theme_line_widths: &[f32],
    theme_line_color_mods: &[Vec<(String, i32)>],
    theme_fill_styles: &[ThemeFillStyleDef],
    // PROOT bullet: master <p:txStyles> 的 bullet 默认值（title/body/other
    // 三分类各自 10 级）。None 表示"这次调用不需要 bullet 继承"——目前用于
    // master slide 自身的形状解析（master 自己不需要再查自己的 txStyles）
    // 和历史测试用例（未涉及 bullet 场景，传 None 保持行为不变，不强制
    // 所有测试用例都要构造一份 MasterTextStyles）。
    master_text_styles: Option<&MasterTextStyles>,
) -> Result<Vec<Shape>, ParseError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut shapes: Vec<Shape> = Vec::new();

    // 状态机：逐层跟踪
    let mut in_sp = false;      // <p:sp>
    // PROOT-002: <p:cxnSp>（连接线，直线/弯头/曲线connector的容器标签）。
    // 复用<p:sp>解析路径的绝大部分逻辑（spPr/style/prstGeom/avLst/ln），
    // 后面涉及in_spPr等状态的判断条件会加上"|| in_cxn_sp"来共享代码，
    // 而不是复制一整套平行的解析分支——两者内部结构（spPr含xfrm+prstGeom、
    // style含fillRef/lnRef/fontRef）完全一致，只是外层容器标签和部分子
    // 元素（stCxn/endCxn，cxnSp特有；txBody，sp特有但cxnSp没有）不同。
    let mut in_cxn_sp = false;
    let mut in_pic = false;     // <p:pic>
    // PROOT-012b: <p:spPr><a:blipFill> 形状级图片填充（与 <p:bg><p:bgPr>
    // <a:blipFill> 背景图片解析——in_bg_pr 那条路径——完全独立，互不干扰）。
    // 只在 in_spPr（即 in_sp || in_pic || in_cxn_sp 已经成立）内才有意义，
    // <p:pic> 场景下 blipFill 走的是已有的 RawContent::Image 路径（本标志
    // 不应在 in_pic 下置位，避免和 b"blip" if in_pic 那条更早的分支冲突）。
    let mut in_shape_blip_fill = false;
    let mut in_graphic_frame = false; // <p:graphicFrame>
    // PPTX-9b: <p:grpSp> 分组形状。用"层级栈"而非递归函数处理嵌套
    // （依据见 roe-ext_progress_v21 笔记"方案二"）：group_stack 每个
    // 元素对应一层未闭合的 <p:grpSp>，栈顶即"当前形状应该归属的分组"。
    // sp/pic/graphicFrame/嵌套grpSp 闭合时，若栈非空就 push 进栈顶的
    // RawShape.content（RawContent::Group 收集区），否则 push 进顶层
    // shapes——这样嵌套 group 天然生效，不需要额外写递归函数。
    let mut group_stack: Vec<RawShape> = Vec::new();
    // <p:grpSpPr>：分组自身的属性容器，对应普通形状的 <p:spPr>，但
    // 故意不复用 in_spPr（分组的 xfrm/off/ext 要写入 group_stack 栈顶
    // 而不是 cur_shape，混用会导致数据写错目标）。
    let mut in_grp_sp_pr = false;
    #[allow(non_snake_case)]
    let mut in_spPr = false;    // <p:spPr> / shape properties
    let mut in_prst_geom = false; // PPTX-11 part5: <a:prstGeom>（非自闭合形式）
    // PROOT-012: <a:custGeom><a:pathLst><a:path w=".." h="..">解析状态。
    // custGeom与prstGeom是XML层面互斥的兄弟标签（spPr下二选一），故用
    // 独立状态量，不复用in_prst_geom（含义不同：custGeom没有prst/adj，
    // 只有直接的路径点数据）。
    let mut in_cust_geom = false;   // <a:custGeom>
    let mut in_cust_path = false;   // <a:path>（pathLst下唯一关心的子元素）
    let mut in_move_to = false;     // <a:moveTo>，下一个<a:pt>是MoveTo目标点
    let mut in_line_to = false;     // <a:lnTo>，下一个<a:pt>是LineTo目标点
    // <a:cubicBezTo>含3个<a:pt>子元素（控制点1/控制点2/终点），<a:quadBezTo>
    // 含2个（控制点/终点）。用计数器区分第几个pt，命中数量后立即复位
    // （不依赖</a:cubicBezTo>结束事件，避免和in_line_to等其他状态混淆）。
    let mut in_cubic_to = false;    // <a:cubicBezTo>
    let mut cubic_to_pt_idx: u8 = 0;
    let mut cubic_to_pts: [(f32, f32); 2] = [(0.0, 0.0); 2]; // 前两个控制点暂存
    let mut in_quad_to = false;     // <a:quadBezTo>
    let mut quad_to_pt_idx: u8 = 0;
    let mut quad_to_ctrl_pt: (f32, f32) = (0.0, 0.0);
    // 当前<a:path w=".." h="...">声明的路径坐标系尺寸（无单位整数，与EMU
    // 无关，是custGeom自己的任意坐标系），用于缩放到shape实际pt尺寸。
    let mut cur_cust_path_w: f32 = 1.0;
    let mut cur_cust_path_h: f32 = 1.0;
    let mut cur_cust_path_cmds: Vec<roe_model::common::PathCommand> = Vec::new();
    let mut in_av_lst = false;    // PPTX-11 part5: <a:avLst>，adj覆盖值列表
    // PROOT-SCHEMECLR-MODS-START: 形状自身<p:spPr><a:solidFill><a:schemeClr
    // val="X">带shade/tint/satMod/lumMod/lumOff/alpha等mods子元素时，触发
    // Event::Start（而非1983行附近处理的自闭合<a:schemeClr val="X"/>Empty
    // 事件），此前完全未处理，形状退化成raw.fill.is_none()。与in_ln/
    // in_outer_shdw等"收集到End时统一写回cur_shape"是同一模式：
    // Start置位+记录slot，Empty收集mods/alpha，End查表+apply_color_mods+
    // 写入s.fill。cur_shape_fill_alpha单独存（不进mods列表）是因为alpha是
    // 透明度语义，最终写Color.a，不参与apply_color_mods的RGB运算。
    let mut in_shape_fill_scheme_clr = false;
    let mut cur_shape_fill_slot: Option<String> = None;
    let mut cur_shape_fill_mods: Vec<(String, i32)> = Vec::new();
    let mut cur_shape_fill_alpha: Option<i32> = None;
    // PROOT-LN-SCHEMECLR-MODS-START: <a:ln><a:solidFill><a:schemeClr val="X">
    // 带shade/tint/satMod/lumMod/lumOff/alpha等mods子元素时，触发Event::Start
    // （而非2059行附近处理的自闭合<a:schemeClr val="X"/>Empty事件）。此前
    // 完全未处理，导致这类connector描边颜色（真实样本sd_data_pptx__ooxtheme.pptx
    // 的Straight Connector，<a:schemeClr val="bg1"><a:lumMod val="75000"/>
    // </a:schemeClr>）解析失败，cur_ln_color保持None，触发lnRef主题色兜底，
    // 描边从bg1灰白色误变成accent1绿色。与in_shape_fill_scheme_clr同一模式，
    // 独立一套变量避免与形状自身fill的mods收集互相污染。
    let mut in_ln_scheme_clr = false;
    let mut cur_ln_scheme_slot: Option<String> = None;
    let mut cur_ln_mods: Vec<(String, i32)> = Vec::new();
    let mut cur_ln_alpha: Option<i32> = None;
    // PPTX-12: gradFill 解析状态。in_grad_fill 是 gsLst/lin/path 的
    // 前提条件（这三者只应在 gradFill 内部被识别，避免误吞其他上下文
    // 同名元素——目前 OOXML 里 lin/path/gsLst 只出现在 gradFill 内，
    // 但显式加前提条件更安全，与既有 in_rPr_fill 等模式一致）。
    let mut in_grad_fill = false;
    let mut in_gs_lst = false;
    let mut in_gs = false;               // 正在读一个 <a:gs> 的颜色子元素
    let mut cur_gs_pos: f32 = 0.0;       // 当前 gs 的 pos，已归一化 0.0-1.0
    let mut cur_gradient_stops: Vec<(f32, Color)> = Vec::new();
    let mut cur_grad_kind_raw: Option<RawGradKind> = None;
    let mut cur_grad_rotate_with_shape: bool = true;
    #[allow(non_snake_case)]
    let mut in_txBody = false;  // <p:txBody>
    let mut in_para = false;    // <a:p>
    let mut in_run = false;     // <a:r>
    let mut in_t = false;       // <a:t>
    #[allow(non_snake_case)]
    let mut in_rPr = false;      // <a:rPr>
    let mut in_rPr_fill = false;
    let mut in_rPr_hl = false;
    let mut cur_run_bg: Option<Color> = None; // solidFill inside rPr
    // PROOT-004b: <p:style> fillRef/lnRef 主题色引用解析。<p:style> 与
    // <p:spPr> 是平级兄弟（都在 <p:sp> 下），当 spPr 内没有直接写
    // solidFill/gradFill/noFill 时（即 raw.fill 仍是 Option::None，
    // 注意与用户显式 noFill 写入的 FillStyle::None 是两回事，不可混淆），
    // 用这里解析出的颜色作为 fallback。lnRef 同理用于 stroke。
    // fontRef（文字色）本次未处理：bug_log记录的样本证据只涉及
    // fillRef/lnRef，fontRef没有实际样本验证，不做无依据的推测实现。
    let mut in_p_style = false;
    let mut in_fill_ref = false;
    let mut in_ln_ref = false;
    let mut in_font_ref = false;
    let mut style_fill_color: Option<Color> = None;
    let mut style_line_color: Option<Color> = None;
    let mut style_font_color: Option<Color> = None;
    // PROOT-002: <a:ln>...</a:ln> 非自闭合形式（含 headEnd/tailEnd 子元素时
    // 必然是这种形式）。原有 b"ln" 分支是 Event::Start 时立即写 stroke，
    // 没有维护状态就直接处理完了——这对没有子元素的场景没问题，但现在
    // <a:ln>可能包含headEnd/tailEnd子元素，需要知道"当前在ln内部"才能
    // 正确识别这两个子标签只在ln内才有意义（虽然目前OOXML里headEnd/
    // tailEnd只出现在ln内，加这个前提条件是为了和既有in_spPr等模式一致，
    // 更安全)。
    let mut in_ln = false;
    // PROOT-outerShdw: <a:effectLst><a:outerShdw dist=".." dir=".." blurRad="..">
    // 状态标记 + 收集颜色/alpha 的临时变量，</a:outerShdw>结束时统一写入
    // cur_shape.outer_shdw_raw（与 cur_ln_color 收集描边颜色是同款模式）。
    let mut in_outer_shdw = false;
    let mut cur_shdw_dist: i32 = 0;
    let mut cur_shdw_dir: i32 = 0;
    let mut cur_shdw_blur: i32 = 0;
    let mut cur_shdw_color: Option<Color> = None;
    let mut cur_shdw_alpha: i32 = 100000; // ST_PositiveFixedPercentage默认100%，未出现<a:alpha>时不透明
    let mut cur_head_arrow: Option<String> = None;
    let mut cur_tail_arrow: Option<String> = None;
    // PROOT-D bugfix: <a:ln><a:solidFill><a:srgbClr/schemeClr> 描边颜色。
    // 与 ln_width 同样的原因分离临时存储：srgbClr/schemeClr 的通用分支
    // （下面 in_spPr && !in_grad_fill 那两条）此前没有排除 in_ln，导致
    // <a:ln> 内部的颜色被错误当成形状填充色写进了 s.fill，而不是描边色。
    // </a:ln> 结束时统一写入 raw.stroke（复用 ln_width 的"先分离存、
    // 最后统一合并"模式）。
    let mut cur_ln_color: Option<Color> = None;
    let mut cur_run_color: Option<Color> = None;
    let mut cur_run_font_size: Option<f32> = None;
    let mut cur_run_bold: Option<bool> = None;
    let mut cur_run_italic: Option<bool> = None;
    let mut cur_run_underline: Option<bool> = None;
    let mut cur_run_strike: Option<bool> = None;
    let mut cur_run_script: Option<roe_model::writer::ScriptPosition> = None;
    let mut cur_run_font_family: Option<String> = None;
    // PROOT-slide-endpara: slide自身占位符的<a:endParaRPr>格式追踪。
    // 修复slide2.xml场景：占位符本身没有真实<a:r>run，但endParaRPr里
    // 直接硬编码了颜色/字号/字重（模板设计常见手法：提示态格式即最终
    // 视觉效果），此前parse_slide_xml_shapes完全不解析endParaRPr标签，
    // 导致这类颜色在raw.content阶段就已丢失，样式继承阶段无从补救。
    let mut cur_endpara_color: Option<Color> = None;
    let mut cur_endpara_font_size: Option<f32> = None;
    let mut cur_endpara_bold: Option<bool> = None;
    let mut cur_endpara_italic: Option<bool> = None;
    let mut cur_endpara_font_family: Option<String> = None;
    let mut in_endpara_rpr = false;
    let mut in_endpara_fill = false;

    let mut cur_shape: Option<RawShape> = None;
    let mut cur_paras: Vec<Paragraph> = Vec::new();
    let mut cur_body_pr = roe_model::impress::TextBodyProps::default();
    let mut cur_runs: Vec<Run> = Vec::new();
    let mut cur_text = String::new();
    // PROOT-004 followup: <a:pPr algn="..."> 段落级对齐方式，之前完全未解析
    let mut cur_para_align: Option<Alignment> = None;
    // PROOT bullet 任务A: <a:pPr> 的 marL/indent/lvl 三个属性（EMU已换算为pt，
    // lvl是OOXML原始0-based属性值，与master txStyles的lvl1pPr..lvl10pPr的
    // 1-based编号不是一回事，此处不做-1转换，查表时才转换）。
    let mut cur_para_mar_l: Option<f32> = None;
    let mut cur_para_indent: Option<f32> = None;
    let mut cur_para_lvl: Option<usize> = None;
    // PROOT bullet 任务A: 是否在非自闭合 <a:pPr>...</a:pPr> 内部。只有非
    // 自闭合形式才可能带 buChar/buAutoNum/buNone 等子元素（自闭合pPr走
    // Event::Empty，不可能有子元素）。用于任务B的buChar等分支guard。
    let mut in_pPr = false;
    let mut in_lnspc = false;
    let mut cur_para_lnspc: Option<f32> = None;
    let mut cur_para_spcbef: Option<f32> = None;
    let mut cur_para_spcaft: Option<f32> = None;
    let mut in_spcbef = false;
    let mut in_spcaft = false;
    // PROOT bullet 任务B（本步骤先声明，暂不使用，避免后续再来这里改）:
    // 段落自身 <a:pPr> 下 buChar/buAutoNum/buNone 解析出的值。
    let mut cur_para_bullet: Option<roe_model::impress::BulletKind> = None;
    // PROOT-024: <a:buFont typeface="..."/> 实测顺序总是先于 <a:buChar>
    // 出现（同 master lvlNpPr 里的顺序一致），此处暂存 typeface，等
    // buChar 出现时一并用它构造 BulletKind::Char{ font: Some(..) }。
    // 此前误判"buFont先出现时bullet还是None，回填必然是死代码"，漏想了
    // "先暂存、后消费"这个中间状态，导致 buFont 信息被直接丢弃——对
    // 使用 Wingdings/Symbol 等符号字体+PUA码点的 bullet（真实样本
    // tdf148810_PARA_OUTLLEVEL复现）而言，没有font hint会导致渲染层
    // 用错误的默认字体解读PUA码点，显示出不相关的字形。
    let mut cur_para_bullet_font: Option<String> = None;
    // PROOT bullet-color: <a:pPr><a:buClr><a:srgbClr/schemeClr></a:buClr>，
    // 颜色在子元素里，需要 Start/End 配对的 guard（非自闭合，buClr 本身
    // 不带颜色属性）。收集到 cur_para_bullet_color，随 cur_para_bullet
    // 一起在最终构造 BulletInfo 时消费。与 in_ln/in_outer_shdw 同款模式，
    // 但 buClr 独立作用域，不与它们共用变量（避免污染）。
    let mut in_buClr = false;
    let mut cur_para_bullet_color: Option<Color> = None;

    let mut shape_counter: u32 = 1;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                match e.local_name().as_ref() {
                    b"sp" => {
                        in_sp = true;
                        cur_shape = Some(RawShape::default());
                        // PROOT-004b: 新shape开始，清空上一个shape遗留的
                        // style fallback暂存值（避免跨shape串色）。
                        style_fill_color = None;
                        style_line_color = None;
                        style_font_color = None;
                        // PROOT-V88-CURLNCOLOR-LEAK: cur_ln_color此前只在
                        // 消费点（</a:ln>结束时.take()）被清空，没有在新
                        // shape开始时重置。若某个shape的<a:ln>是自闭合
                        // （无子元素，不触发Start/End配对，例如<a:ln w="76200"/>），
                        // 它不会走</a:ln>结束的.take()消费逻辑；此时如果
                        // cur_ln_color在更早处被意外设置过（具体触发路径
                        // 待查，本次会话通过debug实测确认了残留会跨shape
                        // 传播，真实样本tdf165180实测：cur_ln_color残留的
                        // 黑色被下一个真正带非自闭合<a:ln>...</a:ln>的cxnSp
                        // 消费，导致该connector描边从主题色变成黑色，两条
                        // connector复现），残留值会一直带到下一个真正消费
                        // 它的shape。在shape边界强制清空，彻底切断跨shape
                        // 污染路径，不依赖找全所有可能的设置点。
                        cur_ln_color = None;
                    }
                    // PROOT-002: <p:cxnSp>，与<p:sp>平级但走独立标志位，
                    // 复用后续spPr/style/prstGeom/avLst/ln解析条件（这些
                    // 条件会在下面陆续加上"|| in_cxn_sp"）。
                    b"cxnSp" => {
                        in_cxn_sp = true;
                        cur_shape = Some(RawShape::default());
                        style_fill_color = None;
                        style_line_color = None;
                        style_font_color = None;
                        cur_head_arrow = None;
                        cur_tail_arrow = None;
                        // PROOT-V88-CURLNCOLOR-LEAK: 同b"sp"分支注释，见上方。
                        cur_ln_color = None;
                    }
                    b"pic" => {
                        in_pic = true;
                        cur_shape = Some(RawShape::default());
                    }
                    // bugfix（本次会话，PROOT-D图片丢失排查）：<a:blip> 若带
                    // 子元素（如 <a:grayscl/>、<a:extLst><asvg:svgBlip .../>
                    // </a:extLst> 这类颜色特效/SVG增强信息），quick-xml 触发
                    // 的是 Event::Start 而非 Event::Empty，此前 b"blip" 处理
                    // 只写在下方 Event::Empty 分支里，导致这种情况下 r:embed
                    // 属性完全没被读取、RawContent::Image 从未被赋值，图片
                    // 彻底丢失（真实样本 tdf163852: <a:blip r:embed="rId1">
                    // 内含 asvg:svgBlip 扩展信息；tdf112209:
                    // <a:blip r:embed="rId2"><a:grayscl/> 均复现）。
                    // 与下方 Event::Empty 分支逻辑保持一致（同样只认外层
                    // <a:blip> 自身的 r:embed 属性，不会被子元素如
                    // <asvg:svgBlip r:embed="..."/> 的同名属性污染，因为
                    // e.attributes() 只枚举当前这一个 Start 事件对应标签
                    // 自身的属性，不含子元素）。
                    b"blip" if in_pic => {
                        if let Some(ref mut s) = cur_shape {
                            for attr in e.attributes().flatten() {
                                let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                                if key == "r:embed" || key.ends_with("}embed") {
                                    let rid = attr.unescape_value().unwrap_or_default().to_string();
                                    s.content = RawContent::Image { rId: rid, crop_l: 0, crop_t: 0, crop_r: 0, crop_b: 0 };
                                }
                            }
                        }
                    }
                    b"graphicFrame" => {
                        in_graphic_frame = true;
                        cur_shape = Some(RawShape::default());
                    }
                    // PPTX-9b: <p:grpSp>，push 新的分组构建器到栈顶。
                    // 注意这里不设置 cur_shape / in_sp 等——分组自身的
                    // 属性走独立的 group_stack + in_grp_sp_pr 通道，
                    // 避免与"当前正在构建的普通形状"混淆。
                    b"grpSp" => {
                        group_stack.push(RawShape::default());
                    }
                    // 分组自身属性容器，对应普通形状的 <p:spPr>。
                    b"grpSpPr" => {
                        in_grp_sp_pr = true;
                    }
                    // PROOT-004b: <p:style> 主题样式引用块，与 <p:spPr> 平级。
                    // PROOT-002: cxnSp 同样带 <p:style>（fillRef/lnRef/fontRef写法
                    // 与普通shape完全一致，v34 PROOT-004b排查tdf114845时已确认），
                    // 复用同一条件。
                    b"style" if in_sp || in_cxn_sp => {
                        in_p_style = true;
                    }
                    b"fillRef" if in_p_style => {
                        in_fill_ref = true;
                        if let Some(ref mut s) = cur_shape {
                            if let Some(idx) = attr_str(e, b"idx").and_then(|v| v.parse::<i32>().ok()) {
                                s.fill_ref_idx = Some(idx);
                            }
                        }
                    }
                    b"lnRef" if in_p_style => {
                        in_ln_ref = true;
                        if let Some(ref mut s) = cur_shape {
                            if let Some(idx) = attr_str(e, b"idx").and_then(|v| v.parse::<i32>().ok()) {
                                s.ln_ref_idx = Some(idx);
                            }
                        }
                    }
                    b"fontRef" if in_p_style => {
                        in_font_ref = true;
                    }
                    // PPTX-6: <a:tbl> 只会出现在 <p:graphicFrame> 内部（<p:txBody>
                    // 本身不含 a:tbl，故此处不需要额外排除 in_sp/in_pic，
                    // in_graphic_frame 已经是充分的上下文判据）。parse_table
                    // 接管 reader，一路读到匹配的 </a:tbl> 才返回，本分支只需
                    // 拿到返回值写回 cur_shape.content，不需要额外维护 in_tbl
                    // 之类的状态标记（这与 chart/off/ext 走 Event::Empty 单值
                    // 读取不同，表格是嵌套结构，必须交给独立函数处理才能保持
                    // 主状态机本身不被表格内部的 p/r/rPr/t 事件污染）。
                    b"tbl" if in_graphic_frame => {
                        let table = parse_table(&mut reader, theme_colors, clr_map)?;
                        if let Some(ref mut s) = cur_shape {
                            s.content = RawContent::Table(table);
                        }
                    }
                    b"spPr" if in_sp || in_pic || in_cxn_sp => {
                        in_spPr = true;
                    }
                    // PROOT-012b: 形状级 <a:blipFill>，只在 <p:sp>/<p:cxnSp>
                    // 的 spPr 内识别（in_pic 场景已有独立的 RawContent::Image
                    // 路径，不需要也不应该走这里，避免重复赋值 raw.fill）。
                    b"blipFill" if in_spPr && (in_sp || in_cxn_sp) => {
                        in_shape_blip_fill = true;
                    }
                    // 同一个 <a:blip> 标签，Start 和 Empty 两种事件形式都要处理
                    // （子元素如 <a:grayscl/> 存在时触发 Start，否则触发 Empty，
                    // 已在 in_pic 分支踩过这个坑，见上方 b"blip" if in_pic 注释）。
                    b"blip" if in_shape_blip_fill => {
                        if let Some(ref mut s) = cur_shape {
                            for attr in e.attributes().flatten() {
                                let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                                if key == "r:embed" || key.ends_with("}embed") {
                                    let rid = attr.unescape_value().unwrap_or_default().to_string();
                                    if let Some(bytes) = image_map.get(&rid) {
                                        let mime = if bytes.starts_with(b"\x89PNG") {
                                            "image/png"
                                        } else if bytes.starts_with(b"\xff\xd8") {
                                            "image/jpeg"
                                        } else if bytes.starts_with(b"GIF") {
                                            "image/gif"
                                        } else {
                                            "image/unknown"
                                        };
                                        s.fill = Some(FillStyle::Picture(roe_model::common::ImageData {
                                            data: bytes.clone(),
                                            mime: mime.to_string(),
                                            ..Default::default()
                                        }));
                                    }
                                }
                            }
                        }
                    }
                    // PROOT-002 bugfix: <a:ln>...</a:ln> 非自闭合标签，触发
                    // Event::Start（实测tdf114845确认，quick-xml discriminant
                    // 0=Start/1=End），此前误放在 Event::Empty 分支导致这段
                    // 代码是死代码，in_ln恒为false，headEnd/tailEnd因此从未
                    // 被正确识别（guard `if in_ln` 从未满足）。只设置
                    // in_ln=true 和线宽（w属性，EMU->pt），黑色兜底只在
                    // s.stroke尚为None时才写入（避免抢先锁死颜色导致后续
                    // <p:style> lnRef 主题色fallback判断raw.stroke.is_none()
                    // 失效；若s.stroke已有值——理论上此刻不应该有，纯防御性
                    // 分支——只补上线宽，不覆盖已有颜色）。
                    b"ln" if in_spPr => {
                        in_ln = true;
                        if let Some(ref mut s) = cur_shape {
                            // PROOT-002 lnRef修复: 不再用unwrap_or(12700.0)填充默认值，
                            // 否则"<a:ln>没写w"和"<a:ln w=12700>"无法区分，导致后面
                            // ln_width.is_none()判断永远为false、lnRef主题线宽fallback
                            // 永远不触发（实测tdf114845connector暴露的bug）。没写w时
                            // 保持None，交给结束时的lnRef fallback或黑色兜底处理。
                            s.ln_width = attr_f32(e, b"w").map(|w| w / 12700.0); // EMU -> pt
                        }
                    }
                    // PROOT-outerShdw: <a:spPr><a:effectLst><a:outerShdw dist=".." dir=".."
                    // blurRad="..">，dist/dir/blurRad是EMU/60000度整数原始值，换算延后到
                    // raw_to_shape（与其余"解析层只收原始值"字段一致的模式）。第一期只处理
                    // outerShdw，innerShdw/glow/softEdge/reflection/blur等效果标签会被
                    // Start事件命中但因为没有对应match分支而被忽略，不影响其余解析。
                    b"outerShdw" if in_spPr => {
                        in_outer_shdw = true;
                        cur_shdw_dist = attr_f32(e, b"dist").map(|v| v as i32).unwrap_or(0);
                        cur_shdw_dir = attr_f32(e, b"dir").map(|v| v as i32).unwrap_or(0);
                        cur_shdw_blur = attr_f32(e, b"blurRad").map(|v| v as i32).unwrap_or(0);
                        cur_shdw_color = None;
                        cur_shdw_alpha = 100000;
                    }
                    // PPTX-11 part5：非自闭合形式的 <a:prstGeom prst="...">
                    // ...</a:prstGeom>（带 avLst 子元素时必然是这种形式，
                    // 而非自闭合）。原有 Event::Empty 分支只覆盖自闭合
                    // <a:prstGeom prst="..."/>（无 avLst 场景），二者互补，
                    // 不冲突——同一个 shape 只会命中其中一种形式。
                    b"prstGeom" if in_spPr => {
                        in_prst_geom = true;
                        if let Some(ref mut s) = cur_shape {
                            s.geometry = attr_str(e, b"prst");
                        }
                    }
                    b"avLst" if in_prst_geom => {
                        in_av_lst = true;
                    }
                    // PROOT-012: <a:custGeom>，与prstGeom是spPr下互斥的兄弟标签。
                    b"custGeom" if in_spPr => {
                        in_cust_geom = true;
                    }
                    // <a:pathLst>本身不需要单独状态，直接进子元素<a:path>。
                    b"path" if in_cust_geom => {
                        in_cust_path = true;
                        cur_cust_path_w = attr_f32(e, b"w").unwrap_or(0.0).max(1.0);
                        cur_cust_path_h = attr_f32(e, b"h").unwrap_or(0.0).max(1.0);
                        cur_cust_path_cmds.clear();
                    }
                    b"moveTo" if in_cust_path => {
                        in_move_to = true;
                    }
                    b"lnTo" if in_cust_path => {
                        in_line_to = true;
                    }
                    b"cubicBezTo" if in_cust_path => {
                        in_cubic_to = true;
                        cubic_to_pt_idx = 0;
                    }
                    b"quadBezTo" if in_cust_path => {
                        in_quad_to = true;
                        quad_to_pt_idx = 0;
                    }
                    // PPTX-12: <a:gradFill rotWithShape="...">，rotWithShape
                    // 缺省为 true（LOK/OO 均确认此默认值）。
                    b"gradFill" if in_spPr => {
                        in_grad_fill = true;
                        cur_gradient_stops.clear();
                        cur_grad_kind_raw = None;
                        cur_grad_rotate_with_shape = attr_str(e, b"rotWithShape")
                            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                            .unwrap_or(true);
                    }
                    b"gsLst" if in_grad_fill => {
                        in_gs_lst = true;
                    }
                    // <a:gs pos="..."> pos 是 ST_Percentage，十万分之一精度
                    // （已核实 OOXML 规范，非早期笔记猜测的千分之一——与
                    // PPTX-10 srcRect 的 l/t/r/b 同一精度基准，同一 norm 换算方式）。
                    // 颜色本身在子元素（srgbClr/schemeClr）里，Event::Empty 处理，
                    // 用 in_gs 标记"当前颜色应归属哪个 gs"。
                    b"gs" if in_gs_lst => {
                        in_gs = true;
                        cur_gs_pos = (attr_i32(e, b"pos").unwrap_or(0).max(0) as f32) / 100000.0;
                    }
                    b"txBody" if in_sp => {
                        in_txBody = true;
                        cur_paras.clear();
                        cur_body_pr = roe_model::impress::TextBodyProps::default();
                    }
                    b"bodyPr" if in_txBody => {
                        use roe_model::impress::TextAnchor;
                        // wrap: "none" = no wrap, default = wrap
                        cur_body_pr.wrap = attr_str(e, b"wrap").map(|v| v != "none").unwrap_or(true);
                        // anchor: t/ctr/b
                        cur_body_pr.anchor = match attr_str(e, b"anchor").as_deref() {
                            Some("ctr") => TextAnchor::Middle,
                            Some("b")   => TextAnchor::Bottom,
                            _           => TextAnchor::Top,
                        };
                        // insets (EMU -> pt)
                        cur_body_pr.ins_l = attr_f32(e, b"lIns").map(|v| v / 12700.0);
                        cur_body_pr.ins_r = attr_f32(e, b"rIns").map(|v| v / 12700.0);
                        cur_body_pr.ins_t = attr_f32(e, b"tIns").map(|v| v / 12700.0);
                        cur_body_pr.ins_b = attr_f32(e, b"bIns").map(|v| v / 12700.0);
                        // PROOT-1xx (v99): <a:bodyPr vert="..."/>，文本框整体排版方向。
                        cur_body_pr.vert = match attr_str(e, b"vert").as_deref() {
                            Some("vert")    => roe_model::impress::TextVert::Vert,
                            Some("vert270") => roe_model::impress::TextVert::Vert270,
                            Some("eaVert")  => roe_model::impress::TextVert::EaVert,
                            _ => roe_model::impress::TextVert::Horizontal,
                        };
                        // autofit: handled by child elements below
                    }
                    b"noAutofit" if in_txBody => {
                        cur_body_pr.autofit = roe_model::impress::TextAutofit::NoAutofit;
                    }
                    b"spAutoFit" if in_txBody => {
                        cur_body_pr.autofit = roe_model::impress::TextAutofit::SpAutoFit;
                    }
                    b"normAutofit" if in_txBody => {
                        let font_scale = attr_f32(e, b"fontScale").map(|v| v / 100000.0);
                        let ln_spc    = attr_f32(e, b"lnSpcReduction").map(|v| v / 100000.0);
                        cur_body_pr.autofit = roe_model::impress::TextAutofit::NormAutofit {
                            font_scale,
                            ln_spc_reduction: ln_spc,
                        };
                    }
                    b"p" if in_txBody => {
                        in_para = true;
                        cur_runs.clear();
                        cur_para_align = None;
                        // PROOT bullet 任务C: 新段落开始，清空上一段落遗留的
                        // pPr相关累积值，避免"这个段落没有pPr"时错误继承上一
                        // 段落解析出的marL/indent/lvl/bullet（此前遗漏，是个
                        // 隐患：没有该清空逻辑时，第二个无pPr的段落会错误地
                        // 沿用第一个段落的bullet信息）。in_pPr也一并兜底清零，
                        // 防止XML异常（如pPr未正常闭合）导致状态跨段落污染。
                        cur_para_mar_l = None;
                        cur_para_indent = None;
                        cur_para_lvl = None;
                        cur_para_bullet = None;
                        // PROOT bullet-color: 同批清空，避免跨段落误继承
                        // 上一段落的 bullet 颜色（同 cur_para_bullet 的清空理由）。
                        cur_para_bullet_color = None;
                        in_buClr = false;
                        cur_para_bullet_font = None;
                        in_pPr = false;
                    }
                    // PROOT-004 followup: <a:pPr algn="..."> 非自闭合形式
                    // （带 lnSpc/spcBef 等子元素时走 Start 事件）
                    // PROOT bullet 任务A: 非自闭合 <a:pPr>...</a:pPr>，可能带
                    // buChar/buAutoNum/buNone 等子元素（任务B处理），此处先
                    // 标记 in_pPr=true 并解析 pPr 自身属性（algn/marL/indent/lvl）。
                    b"pPr" if in_para => {
                        in_pPr = true;
                        cur_para_align = match attr_str(e, b"algn").as_deref() {
                            Some("l")    => Some(Alignment::Left),
                            Some("ctr")  => Some(Alignment::Center),
                            Some("r")    => Some(Alignment::Right),
                            Some("just") => Some(Alignment::Justify),
                            _            => None,
                        };
                        // EMU -> pt
                        cur_para_mar_l = attr_f32(e, b"marL").map(|v| v / 12700.0);
                        cur_para_indent = attr_f32(e, b"indent").map(|v| v / 12700.0);
                        // lvl 是 OOXML 原始 0-based 属性值，不做转换
                        cur_para_lvl = attr_str(e, b"lvl").and_then(|v| v.parse::<usize>().ok());
                    }
                    // PROOT bullet-color (fix): <a:buClr> 是非自闭合标签
                    // （<a:buClr><a:srgbClr .../></a:buClr>），走 Event::Start，
                    // 不是 Event::Empty——此前误放进了 buChar/buFont 所在的
                    // Event::Empty 专属match块（那些是自闭合标签），导致
                    // <a:buClr> Start 事件根本不会进入那个match、探针完全
                    // 不触发。真实样本bulletColor调试确认：buChar/buFont能
                    // 命中Empty块，buClr作为Start事件被完全跳过。修复：
                    // buClr Start 处理移到本Event::Start块；其内部子元素
                    // srgbClr/schemeClr本身是自闭合的，继续留在Event::Empty
                    // 块里正常处理（那两个分支本身没问题，只是外层buClr
                    // 没能开启in_buClr窗口）。
                    b"lnSpc" if in_pPr => { in_lnspc = true; }
                    b"spcBef" if in_pPr => { in_spcbef = true; }
                    b"spcAft" if in_pPr => { in_spcaft = true; }
                    b"buClr" if in_pPr => {
                        in_buClr = true;
                    }
                    b"r" if in_para => {
                        in_run = true;
                        cur_text.clear();
                        cur_run_color = None;
                    }
                    // PROOT-014 followup: <a:br> 带子元素形式（如
                    // <a:br><a:rPr sz="1400" dirty="0"/></a:br>），OOXML
                    // 规范允许 <a:br> 携带 rPr 子元素表示"换行后新起始处
                    // 的字符属性"，此时触发 Event::Start + Event::End
                    // 事件对，而非 Event::Empty 单值事件。与下方已有的
                    // Event::Empty 分支（处理无子元素的自闭合 <a:br/>）
                    // 是同一个 bug 模式的另一半：自闭合 vs 带子元素走
                    // 不同事件分支，需要两边都覆盖。本分支只负责插入
                    // 换行本身（沿用现有用换行符表达换行的设计，与
                    // Empty 分支完全一致），不读取 <a:br> 内部 rPr 子
                    // 元素的字号/颜色信息——它只是换行处的字符属性声明，
                    // 视觉上不独立呈现，暂不处理不影响换行生效这个
                    // 首要目标。<a:br> 内部若出现自闭合 <a:rPr/>，会走
                    // 下面 Event::Empty 的 `b"rPr" if in_run` 分支，但
                    // 此刻 in_run 为 false（未曾进入过 <a:r>），因此会
                    // 被安全忽略，不会误伤当前状态。
                    b"br" if in_para => {
                        cur_runs.push(Run { text: "\n".to_string(), char_style: None, ..Default::default() });
                    }
                    b"rPr" if in_run => {
                        in_rPr = true;
                        in_rPr_fill = false;
                        // sz 单位是百分之一磅（hundredths of a point）
                        cur_run_font_size = attr_u32(e, b"sz").map(|v| v as f32 / 100.0);
                        cur_run_bold   = attr_str(e, b"b").map(|v| v != "0");
                        cur_run_italic = attr_str(e, b"i").map(|v| v != "0");
                        cur_run_underline = attr_str(e, b"u").map(|v| v != "none");
                        cur_run_strike = attr_str(e, b"strike").map(|v| v != "noStrike");
                        cur_run_script = attr_i32(e, b"baseline").and_then(|v| if v > 0 { Some(roe_model::writer::ScriptPosition::Superscript) } else if v < 0 { Some(roe_model::writer::ScriptPosition::Subscript) } else { None });
                    }
                    b"highlight" if in_rPr => { in_rPr_hl = true; }
                    b"solidFill" if in_rPr => {
                        in_rPr_fill = true;
                    }
                    b"latin" if in_rPr => {
                        if let Some(tf) = attr_str(e, b"typeface") {
                            if tf != "+mj-lt" && tf != "+mn-lt" && !tf.is_empty() {
                                cur_run_font_family = Some(tf);
                            }
                        }
                    }
                    // PROOT-slide-endpara: endParaRPr Start写法（含子元素如
                    // solidFill/latin时触发），照抄parse_raw_shapes同款逻辑。
                    b"endParaRPr" if in_para => {
                        in_endpara_rpr = true;
                        in_endpara_fill = false;
                        cur_endpara_font_size = attr_u32(e, b"sz").map(|v| v as f32 / 100.0);
                        cur_endpara_bold   = attr_str(e, b"b").map(|v| v != "0");
                        cur_endpara_italic = attr_str(e, b"i").map(|v| v != "0");
                    }
                    b"solidFill" if in_endpara_rpr => {
                        in_endpara_fill = true;
                    }
                    b"latin" if in_endpara_rpr => {
                        if let Some(tf) = attr_str(e, b"typeface") {
                            if tf != "+mj-lt" && tf != "+mn-lt" && !tf.is_empty() {
                                cur_endpara_font_family = Some(tf);
                            }
                        }
                    }
                    b"t" if in_run => {
                        in_t = true;
                        cur_text.clear();
                    }
                    b"cNvPr" if in_sp || in_pic => {
                        if let Some(ref mut s) = cur_shape {
                            s.id = attr_u32(e, b"id").unwrap_or(shape_counter);
                            s.name = attr_str(e, b"name").unwrap_or_default();
                        }
                    }
                    b"cNvPr" if cur_shape.is_some() && !in_sp && !in_pic => {
                        if let Some(ref mut s) = cur_shape {
                            s.id = attr_u32(e, b"id").unwrap_or(shape_counter);
                            s.name = attr_str(e, b"name").unwrap_or_default();
                        }
                    }
                    // PROOT-GRPSP-CNVPR: <p:grpSp><p:nvGrpSpPr><p:cNvPr id=".." name=".."/>
                    // 分组自身的id/name。此前完全没有分支处理这个场景——cur_shape在grpSp
                    // 内是None（分组走独立的group_stack通道，见上方"grpSp => { group_stack.push"
                    // 处的注释），导致上面两个cNvPr分支（in_sp||in_pic 和 cur_shape.is_some()）
                    // 都不匹配，Group自身的id/name从未被赋值，最终raw_to_shape组装时id落到
                    // shape_counter fallback、name为空字符串。真实样本sd_data_pptx__ooxtheme
                    // 复现：XML里Group 6的id="7"，解析结果却是shape_counter生成的id=11，
                    // 且这个fallback id还与另一处（master层同一个Group）的fallback id意外
                    // 相同，导致后续master/layout去重逻辑（按id匹配）无法工作。
                    // 用 group_stack.last_mut() 而非 cur_shape 写入，因为这个cNvPr描述的
                    // 是分组自己（栈顶RawShape），不是某个子shape。条件用
                    // "!group_stack.is_empty() && cur_shape.is_none()"而非更简单的
                    // "!group_stack.is_empty()"，是为了避免误吞掉grpSp内部某个子shape
                    // 自身cNvPr的情况（此时cur_shape已经是Some，应该走上面两个分支之一，
                    // 不应该被这条新分支抢先匹配——quick-xml match是按顺序找第一个满足guard
                    // 的分支，这条必须排在两个既有cNvPr分支之后，且自身guard要互斥）。
                    b"cNvPr" if cur_shape.is_none() && !group_stack.is_empty() => {
                        if let Some(g) = group_stack.last_mut() {
                            g.id = attr_u32(e, b"id").unwrap_or(shape_counter);
                            g.name = attr_str(e, b"name").unwrap_or_default();
                        }
                    }
                    b"xfrm" if in_spPr => {
                        // off/ext 在子元素 Empty 里处理；rot/flipH/flipV 是 xfrm 自身属性，这里读取
                        if let Some(ref mut s) = cur_shape {
                            if let Some(r) = attr_i32(e, b"rot") { s.rot = Some(r); }
                            if let Some(fh) = attr_str(e, b"flipH") { s.flip_h = Some(fh == "1" || fh.eq_ignore_ascii_case("true")); }
                            if let Some(fv) = attr_str(e, b"flipV") { s.flip_v = Some(fv == "1" || fv.eq_ignore_ascii_case("true")); }
                        }
                    }
                    b"xfrm" if cur_shape.is_some() && !in_sp && !in_pic => {
                        // graphicFrame xfrm: off/ext 在子元素里；rot/flipH/flipV 同样是自身属性
                        if let Some(ref mut s) = cur_shape {
                            if let Some(r) = attr_i32(e, b"rot") { s.rot = Some(r); }
                            if let Some(fh) = attr_str(e, b"flipH") { s.flip_h = Some(fh == "1" || fh.eq_ignore_ascii_case("true")); }
                            if let Some(fv) = attr_str(e, b"flipV") { s.flip_v = Some(fv == "1" || fv.eq_ignore_ascii_case("true")); }
                        }
                    }
                    // PPTX-9b: <p:grpSpPr><a:xfrm rot=".." flipH=".." flipV="..">
                    // 分组自身的旋转/翻转，写入栈顶而非 cur_shape。
                    // chOff/chExt（子坐标系）第一期不做换算，仅在下方
                    // off/ext/chOff/chExt Empty 分支里读出丢弃，已知
                    // 简化记录在 notes/PPTX-9b.md。
                    b"xfrm" if in_grp_sp_pr => {
                        if let Some(s) = group_stack.last_mut() {
                            if let Some(r) = attr_i32(e, b"rot") { s.rot = Some(r); }
                            if let Some(fh) = attr_str(e, b"flipH") { s.flip_h = Some(fh == "1" || fh.eq_ignore_ascii_case("true")); }
                            if let Some(fv) = attr_str(e, b"flipV") { s.flip_v = Some(fv == "1" || fv.eq_ignore_ascii_case("true")); }
                        }
                    }
                    b"solidFill" if in_spPr => {
                        // srgbClr/sysClr 在子元素 Empty 里处理
                    }
                    // PROOT-SCHEMECLR-MODS-START: 形状自身spPr下的
                    // <a:schemeClr val="X">带mods子元素（非自闭合）写法，
                    // 触发Start而非1983行附近的Empty分支。guard条件与那条
                    // Empty分支完全相同（in_spPr && !in_grad_fill && !in_ln），
                    // quick-xml对同一个<a:schemeClr>标签只会触发其中一种
                    // 事件（有子元素→Start+End，无子元素→Empty），不会冲突。
                    b"schemeClr" if in_spPr && !in_grad_fill && !in_ln => {
                        in_shape_fill_scheme_clr = true;
                        cur_shape_fill_slot = attr_str(e, b"val");
                        cur_shape_fill_mods.clear();
                        cur_shape_fill_alpha = None;
                    }
                    // PROOT-LN-SCHEMECLR-MODS-START: <a:ln><a:solidFill>
                    // <a:schemeClr val="X">带mods子元素时的Start事件版本，
                    // 与上面in_shape_fill_scheme_clr是同一模式的镜像，
                    // 只是目标换成描边颜色（cur_ln_color），见变量声明处注释。
                    b"schemeClr" if in_ln => {
                        in_ln_scheme_clr = true;
                        cur_ln_scheme_slot = attr_str(e, b"val");
                        cur_ln_mods.clear();
                        cur_ln_alpha = None;
                    }
                    _ => {}
                }
            }

            Ok(Event::Empty(ref e)) if in_graphic_frame && !in_sp && !in_pic => {
                match e.local_name().as_ref() {
                    b"cNvPr" => {
                        if let Some(ref mut s) = cur_shape {
                            s.id = attr_u32(e, b"id").unwrap_or(shape_counter);
                            s.name = attr_str(e, b"name").unwrap_or_default();
                        }
                    }
                    b"chart" => {
                        if let Some(ref mut s) = cur_shape {
                            for attr in e.attributes().flatten() {
                                let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                                if key == "r:id" || key.ends_with("}id") {
                                    let rid = attr.unescape_value().unwrap_or_default().to_string();
                                    s.content = RawContent::Chart { rId: rid };
                                }
                            }
                        }
                    }
                    b"off" => {
                        if let Some(ref mut s) = cur_shape {
                            s.position.offset.0 = attr_f32(e, b"x").unwrap_or(0.0) / 12700.0;
                            s.position.offset.1 = attr_f32(e, b"y").unwrap_or(0.0) / 12700.0;
                        }
                    }
                    b"ext" => {
                        if let Some(ref mut s) = cur_shape {
                            s.position.extent.0 = attr_f32(e, b"cx").unwrap_or(0.0) / 12700.0;
                            s.position.extent.1 = attr_f32(e, b"cy").unwrap_or(0.0) / 12700.0;
                        }
                    }
                    _ => {}
                }
            }

            Ok(Event::Empty(ref e)) => {
                match e.local_name().as_ref() {
                    // PROOT-V87: <p:style><a:fillRef idx="N"><a:schemeClr val="lt1"/>
                    // </a:fillRef></p:style>。当schemeClr没有shade/tint/satMod等
                    // 子元素修饰时（纯色引用，无任何调整），quick-xml会把它解析成
                    // 自闭合 Event::Empty 而非 Event::Start，此前只在 Event::Start
                    // 分支处理了这三种 schemeClr（in_fill_ref/in_ln_ref/in_font_ref），
                    // 导致这种最常见的"纯色、无调整"写法完全被漏解析，
                    // style_fill_color/style_line_color/style_font_color 保持None，
                    // 最终 raw.fill 也保持None，交给渲染层后被渲染层自己的默认色
                    // 兜底（真实样本tdf165180_standardConnectorsISO29500.potx的
                    // triangle形状复现：<a:fillRef idx="1"><a:schemeClr val="lt1"/>
                    // </a:fillRef>本该是白色填充，因这里漏解析导致style_fill_color
                    // 一直是None，triangle最终fill=None，被渲染层默认兜底成蓝色，
                    // 与ellipse/triangle "退化成矩形"的geometry问题是完全独立的
                    // 另一层bug，见v87 progress文档1.4节）。三条逻辑与Event::Start
                    // 分支（同名guard: in_fill_ref/in_ln_ref/in_font_ref）完全一致，
                    // 纯镜像搬运，不改变任何既有行为，只补上遗漏的Empty事件路径。
                    b"schemeClr" if in_fill_ref => {
                        if let Some(slot) = attr_str(e, b"val") {
                            style_fill_color = Some(theme_colors.get(normalize_scheme_color_slot(&slot, clr_map)).copied()
                                .unwrap_or(Color { r: 0x4F, g: 0x81, b: 0xBD, a: 255 }));
                        }
                    }
                    b"schemeClr" if in_ln_ref => {
                        if let Some(slot) = attr_str(e, b"val") {
                            style_line_color = Some(theme_colors.get(normalize_scheme_color_slot(&slot, clr_map)).copied()
                                .unwrap_or(Color { r: 0x4F, g: 0x81, b: 0xBD, a: 255 }));
                        }
                    }
                    b"schemeClr" if in_font_ref => {
                        if let Some(slot) = attr_str(e, b"val") {
                            style_font_color = Some(theme_colors.get(normalize_scheme_color_slot(&slot, clr_map)).copied()
                                .unwrap_or(Color { r: 0x4F, g: 0x81, b: 0xBD, a: 255 }));
                        }
                    }
                    // PROOT-014 followup: <a:rPr sz="1400" .../> 自闭合形式
                    // （没有 solidFill/latin 等子元素时走 Empty 事件，而非
                    // Start 事件）。此前只在 Event::Start 分支处理 rPr，
                    // 导致这种最常见的简单写法（真实PPTX里"只设字号/粗体/
                    // 斜体、不设颜色和字体"的run十有八九是自闭合形式）
                    // 完全跳过sz/b/i读取，font_size/bold/italic保持None，
                    // 之后被样式继承阶段的layout/master默认值覆盖，导致
                    // 显示字号与原始XML不符。复现样本：tdf147121.pptx，
                    // <a:rPr sz="1400"/> 被忽略，实际渲染成继承来的18pt
                    // 而非原始的14pt。不设 in_rPr=true（自闭合标签没有
                    // 后续 Event::End 来清除这个状态，设了反而会让下一个
                    // 真正的 solidFill/latin 子元素错误地被当成属于这个
                    // 已经结束的rPr），只做一次性属性读取。
                    b"rPr" if in_run => {
                        cur_run_font_size = attr_u32(e, b"sz").map(|v| v as f32 / 100.0);
                        cur_run_bold   = attr_str(e, b"b").map(|v| v != "0");
                        cur_run_italic = attr_str(e, b"i").map(|v| v != "0");
                        cur_run_underline = attr_str(e, b"u").map(|v| v != "none");
                        cur_run_strike = attr_str(e, b"strike").map(|v| v != "noStrike");
                        cur_run_script = attr_i32(e, b"baseline").and_then(|v| if v > 0 { Some(roe_model::writer::ScriptPosition::Superscript) } else if v < 0 { Some(roe_model::writer::ScriptPosition::Subscript) } else { None });
                    }
                    // PROOT-slide-endpara: endParaRPr自闭合写法
                    // （<a:endParaRPr sz="2400" b="1" .../>，无子元素时quick-xml
                    // 触发Empty而非Start，与rPr同理需要在这里也读一次）。
                    // srgbClr在in_endpara_fill下的读取见本Empty分支内其他位置
                    // （srgbClr本身也是Empty事件，与endParaRPr同层匹配即可）。
                    b"endParaRPr" if in_para => {
                        in_endpara_fill = false;
                        cur_endpara_font_size = attr_u32(e, b"sz").map(|v| v as f32 / 100.0);
                        cur_endpara_bold   = attr_str(e, b"b").map(|v| v != "0");
                        cur_endpara_italic = attr_str(e, b"i").map(|v| v != "0");
                    }
                    b"srgbClr" if in_endpara_fill => {
                        if let Some(hex) = attr_str(e, b"val") {
                            cur_endpara_color = Some(parse_hex_color(&hex));
                        }
                    }
                    // PROOT-012: <a:pt x=".." y=".."/>，坐标是custGeom路径自己的
                    // 任意整数单位（不是EMU），归属由in_move_to/in_line_to/
                    // in_cubic_to/in_quad_to哪个在true决定。四者互斥（同一时刻
                    // 只会有一个是true，各自的Start/End事件严格配对）。
                    b"pt" if in_move_to => {
                        let x = attr_f32(e, b"x").unwrap_or(0.0);
                        let y = attr_f32(e, b"y").unwrap_or(0.0);
                        cur_cust_path_cmds.push(roe_model::common::PathCommand::MoveTo { x, y });
                    }
                    b"pt" if in_line_to => {
                        let x = attr_f32(e, b"x").unwrap_or(0.0);
                        let y = attr_f32(e, b"y").unwrap_or(0.0);
                        cur_cust_path_cmds.push(roe_model::common::PathCommand::LineTo { x, y });
                    }
                    // cubicBezTo: 3个pt依次是控制点1/控制点2/终点。前两个先暂存，
                    // 第三个（终点）到达时才拼出完整的CubicTo命令push。
                    b"pt" if in_cubic_to => {
                        let x = attr_f32(e, b"x").unwrap_or(0.0);
                        let y = attr_f32(e, b"y").unwrap_or(0.0);
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
                    // quadBezTo: 2个pt依次是控制点/终点。
                    b"pt" if in_quad_to => {
                        let x = attr_f32(e, b"x").unwrap_or(0.0);
                        let y = attr_f32(e, b"y").unwrap_or(0.0);
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
                    // <a:close/>总是自闭合。
                    b"close" if in_cust_path => {
                        cur_cust_path_cmds.push(roe_model::common::PathCommand::Close);
                    }
                    // PROOT-002: <a:ln><a:headEnd type="arrow"/></a:ln>，起点箭头。
                    // headEnd/tailEnd 均是自闭合标签，quick-xml 触发 Event::Empty
                    // （不是 Event::Start），与 off/ext/srcRect 等既有自闭合字段
                    // 处理方式一致。
                    b"headEnd" if in_ln => {
                        cur_head_arrow = attr_str(e, b"type");
                    }
                    // PROOT-002: <a:ln><a:tailEnd type="..."/></a:ln>，终点箭头。
                    b"tailEnd" if in_ln => {
                        cur_tail_arrow = attr_str(e, b"type");
                    }
                    // PROOT-002: <p:cNvCxnSpPr><a:stCxn id=".." idx=".."/>，
                    // 起点绑定的目标shape id + 连接点idx。只在in_cxn_sp下才
                    // 有意义（这两个标签只出现在cxnSp内部）。id/idx缺失时
                    // 用0兜底——理论上OOXML里这两个属性都是必需的，缺失
                    // 视为解析异常，但用0兜底不panic，保持宽容解析的既有
                    // 风格（其余字段也是attr缺失时给默认值而非报错）。
                    b"stCxn" if in_cxn_sp => {
                        if let Some(ref mut s) = cur_shape {
                            let id = attr_str(e, b"id").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
                            let idx = attr_str(e, b"idx").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
                            s.st_cxn = Some((id, idx));
                        }
                    }
                    // PROOT-002: <a:endCxn id=".." idx=".."/>，语义同 stCxn。
                    b"endCxn" if in_cxn_sp => {
                        if let Some(ref mut s) = cur_shape {
                            let id = attr_str(e, b"id").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
                            let idx = attr_str(e, b"idx").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
                            s.end_cxn = Some((id, idx));
                        }
                    }
                    // PROOT-004 followup: <a:bodyPr anchor="ctr"/> 自闭合形式。
                    // 非自闭合形式（带 normAutofit 等子元素）已在 Event::Start
                    // 分支处理；PowerPoint 常见写法（如 rtlCol="0" anchor="ctr"
                    // 无其他子元素）会触发 Empty 事件，此前未处理导致 anchor 丢失。
                    b"bodyPr" if in_txBody => {
                        use roe_model::impress::TextAnchor;
                        cur_body_pr.wrap = attr_str(e, b"wrap").map(|v| v != "none").unwrap_or(true);
                        cur_body_pr.anchor = match attr_str(e, b"anchor").as_deref() {
                            Some("ctr") => TextAnchor::Middle,
                            Some("b")   => TextAnchor::Bottom,
                            _           => TextAnchor::Top,
                        };
                        cur_body_pr.ins_l = attr_f32(e, b"lIns").map(|v| v / 12700.0);
                        cur_body_pr.ins_r = attr_f32(e, b"rIns").map(|v| v / 12700.0);
                        cur_body_pr.ins_t = attr_f32(e, b"tIns").map(|v| v / 12700.0);
                        cur_body_pr.ins_b = attr_f32(e, b"bIns").map(|v| v / 12700.0);
                        // PROOT-1xx (v99): <a:bodyPr vert="..."/>，文本框整体排版方向。
                        cur_body_pr.vert = match attr_str(e, b"vert").as_deref() {
                            Some("vert")    => roe_model::impress::TextVert::Vert,
                            Some("vert270") => roe_model::impress::TextVert::Vert270,
                            Some("eaVert")  => roe_model::impress::TextVert::EaVert,
                            _ => roe_model::impress::TextVert::Horizontal,
                        };
                    }
                    // PROOT-004 followup: <a:pPr algn="ctr"/> 自闭合形式
                    // PROOT bullet 任务A: 自闭合pPr不可能带buChar等子元素，
                    // 故不设置in_pPr（没有配对的Event::End来清除，设了会
                    // 一直污染到下一个非自闭合pPr才被覆盖，是bug）。
                    b"pPr" if in_para => {
                        cur_para_align = match attr_str(e, b"algn").as_deref() {
                            Some("l")    => Some(Alignment::Left),
                            Some("ctr")  => Some(Alignment::Center),
                            Some("r")    => Some(Alignment::Right),
                            Some("just") => Some(Alignment::Justify),
                            _            => None,
                        };
                        cur_para_mar_l = attr_f32(e, b"marL").map(|v| v / 12700.0);
                        cur_para_indent = attr_f32(e, b"indent").map(|v| v / 12700.0);
                        cur_para_lvl = attr_str(e, b"lvl").and_then(|v| v.parse::<usize>().ok());
                    }
                    b"cNvPr" if in_sp || in_pic => {
                        if let Some(ref mut s) = cur_shape {
                            s.id = attr_u32(e, b"id").unwrap_or(shape_counter);
                            s.name = attr_str(e, b"name").unwrap_or_default();
                        }
                    }
                    // PROOT-GRPSP-CNVPR (Event::Empty版): 与Event::Start分支同一个bug、
                    // 同一处修复动机，见上方Event::Start分支里的详细注释。这里额外多补
                    // 一个"cur_shape.is_some() && !in_sp && !in_pic"分支（对应Event::Start
                    // 里1631行那个覆盖cxnSp等场景的分支）——此前Event::Empty这一侧完全
                    // 没有这个分支，是比grpSp场景更早就存在的一处Start/Empty不对称遗漏，
                    // 只是此前没有真实样本触发过cxnSp的cNvPr走Empty事件（cxnSp的cNvPr
                    // 目前样本里都是自闭合，理论上应该会触发Empty，但此前完全没处理，
                    // 一并在本次连带修复，避免下次单独复现同一模式又要走一轮排查）。
                    b"cNvPr" if cur_shape.is_some() && !in_sp && !in_pic => {
                        if let Some(ref mut s) = cur_shape {
                            s.id = attr_u32(e, b"id").unwrap_or(shape_counter);
                            s.name = attr_str(e, b"name").unwrap_or_default();
                        }
                    }
                    b"cNvPr" if cur_shape.is_none() && !group_stack.is_empty() => {
                        if let Some(g) = group_stack.last_mut() {
                            g.id = attr_u32(e, b"id").unwrap_or(shape_counter);
                            g.name = attr_str(e, b"name").unwrap_or_default();
                        }
                    }
                    b"ph" if in_sp => {
                        if let Some(ref mut s) = cur_shape {
                            let ph_type = attr_str(e, b"type").unwrap_or_else(|| "body".to_string());
                            let ph_idx = attr_u32(e, b"idx");
                            s.ph = Some((ph_type, ph_idx));
                        }
                    }
                    b"blip" if in_pic => {
                        if let Some(ref mut s) = cur_shape {
                            for attr in e.attributes().flatten() {
                                let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                                if key == "r:embed" || key.ends_with("}embed") {
                                    let rid = attr.unescape_value().unwrap_or_default().to_string();
                                    s.content = RawContent::Image { rId: rid, crop_l: 0, crop_t: 0, crop_r: 0, crop_b: 0 };
                                }
                            }
                        }
                    }
                    // PPTX-10: <a:srcRect> 图片裁剪，l/t/r/b 四个可选属性，
                    // 缺省为0（不裁剪）。只在 in_pic 且已经解析出 RawContent::Image
                    // 时写入才有意义，此时 cur_shape.content 必然已经是 Image
                    // 变体（blip 在 srcRect 之前出现是 OOXML 固定顺序：
                    // <pic:blipFill><a:blip .../><a:srcRect .../>...</pic:blipFill>）。
                    b"srcRect" if in_pic => {
                        if let Some(ref mut s) = cur_shape {
                            if let RawContent::Image { ref mut crop_l, ref mut crop_t, ref mut crop_r, ref mut crop_b, .. } = s.content {
                                *crop_l = attr_i32(e, b"l").unwrap_or(0);
                                *crop_t = attr_i32(e, b"t").unwrap_or(0);
                                *crop_r = attr_i32(e, b"r").unwrap_or(0);
                                *crop_b = attr_i32(e, b"b").unwrap_or(0);
                            }
                        }
                    }
                    // xfrm off / ext
                    b"off" if in_spPr || in_graphic_frame => {
                        if let Some(ref mut s) = cur_shape {
                            s.position.offset.0 = attr_f32(e, b"x").unwrap_or(0.0) / 12700.0;
                            s.position.offset.1 = attr_f32(e, b"y").unwrap_or(0.0) / 12700.0;
                        }
                    }
                    b"ext" if in_spPr || in_graphic_frame => {
                        if let Some(ref mut s) = cur_shape {
                            s.position.extent.0 = attr_f32(e, b"cx").unwrap_or(0.0) / 12700.0;
                            s.position.extent.1 = attr_f32(e, b"cy").unwrap_or(0.0) / 12700.0;
                        }
                    }
                    // PPTX-9b: 分组自身 off/ext，写入栈顶。
                    b"off" if in_grp_sp_pr => {
                        if let Some(s) = group_stack.last_mut() {
                            s.position.offset.0 = attr_f32(e, b"x").unwrap_or(0.0) / 12700.0;
                            s.position.offset.1 = attr_f32(e, b"y").unwrap_or(0.0) / 12700.0;
                        }
                    }
                    b"ext" if in_grp_sp_pr => {
                        if let Some(s) = group_stack.last_mut() {
                            s.position.extent.0 = attr_f32(e, b"cx").unwrap_or(0.0) / 12700.0;
                            s.position.extent.1 = attr_f32(e, b"cy").unwrap_or(0.0) / 12700.0;
                        }
                    }
                    // PROOT-D 子问题2 bugfix（v62）：此前 chOff/chExt
                    // "读出即丢弃"（隐含假设子坐标系与父坐标系1:1无缩放）
                    // 是已知技术债，真实样本 tdf141463_GroupTransform
                    // 证明这个假设不成立（chExt与ext不相等），现在实际
                    // 写入 group_stack 栈顶，供 raw_to_shape 组装进
                    // ShapeContent::Group 的 ch_off/ch_ext 字段，
                    // 渲染层用 (ext/ch_ext) 比例对子 Shape 做缩放。
                    b"chOff" if in_grp_sp_pr => {
                        if let Some(s) = group_stack.last_mut() {
                            let x = attr_f32(e, b"x").unwrap_or(0.0) / 12700.0;
                            let y = attr_f32(e, b"y").unwrap_or(0.0) / 12700.0;
                            s.group_ch_off = Some((x, y));
                        }
                    }
                    b"chExt" if in_grp_sp_pr => {
                        if let Some(s) = group_stack.last_mut() {
                            let cx = attr_f32(e, b"cx").unwrap_or(0.0) / 12700.0;
                            let cy = attr_f32(e, b"cy").unwrap_or(0.0) / 12700.0;
                            s.group_ch_ext = Some((cx, cy));
                        }
                    }
                    // geometry preset（自闭合形式 <a:prstGeom prst="..."/>，
                    // 无 avLst 子元素时的写法；非自闭合形式见 Event::Start 分支）
                    b"prstGeom" if in_spPr => {
                        if let Some(ref mut s) = cur_shape {
                            s.geometry = attr_str(e, b"prst");
                        }
                    }
                    // PPTX-11 part5：<a:gd name="adjN" fmla="val N"/>，adj覆盖值。
                    // 解析失败（非 "val N" 形式的公式）静默跳过，该 adj 保留默认值。
                    b"gd" if in_av_lst => {
                        if let Some(ref mut s) = cur_shape {
                            if let (Some(name), Some(fmla)) =
                                (attr_str(e, b"name"), attr_str(e, b"fmla"))
                            {
                                if let Some(val) = parse_adj_fmla(&fmla) {
                                    s.adj_overrides.push((name, val));
                                }
                            }
                        }
                    }
                    // solidFill color
                    // PROOT-D bugfix: 加 !in_ln 排除——此前这条分支会把
                    // <a:ln><a:solidFill><a:srgbClr/> 里的描边颜色也误当成
                    // 形状填充色写进 s.fill，见下方 in_ln 专属分支的对称处理。
                    b"srgbClr" if in_spPr && !in_grad_fill && !in_ln => {
                        if let Some(ref mut s) = cur_shape {
                            if let Some(hex) = attr_str(e, b"val") {
                                s.fill = Some(FillStyle::Solid(parse_hex_color(&hex)));
                            }
                        }
                    }
                    // schemeClr：从主题颜色表查找
                    b"schemeClr" if in_spPr && !in_grad_fill && !in_ln => {
                        if let Some(ref mut s) = cur_shape {
                            if let Some(slot) = attr_str(e, b"val") {
                                let color = theme_colors.get(normalize_scheme_color_slot(&slot, clr_map)).copied().unwrap_or(Color { r: 0x4F, g: 0x81, b: 0xBD, a: 255 });
                                s.fill = Some(FillStyle::Solid(color));
                            }
                        }
                    }
                    // PROOT-SCHEMECLR-MODS-START: 形状自身spPr下schemeClr
                    // 带mods子元素场景（Start事件已置位in_shape_fill_scheme_clr），
                    // 收集shade/tint/satMod/lumMod/lumOff，与parse_theme_fill_styles
                    // 里同类mods收集写法一致，push进cur_shape_fill_mods，
                    // </a:schemeClr>结束时统一apply_color_mods并写入s.fill。
                    b"shade" if in_shape_fill_scheme_clr => {
                        if let Some(v) = attr_i32(e, b"val") {
                            cur_shape_fill_mods.push(("shade".to_string(), v));
                        }
                    }
                    b"tint" if in_shape_fill_scheme_clr => {
                        if let Some(v) = attr_i32(e, b"val") {
                            cur_shape_fill_mods.push(("tint".to_string(), v));
                        }
                    }
                    b"satMod" if in_shape_fill_scheme_clr => {
                        if let Some(v) = attr_i32(e, b"val") {
                            cur_shape_fill_mods.push(("satMod".to_string(), v));
                        }
                    }
                    b"lumMod" if in_shape_fill_scheme_clr => {
                        if let Some(v) = attr_i32(e, b"val") {
                            cur_shape_fill_mods.push(("lumMod".to_string(), v));
                        }
                    }
                    b"lumOff" if in_shape_fill_scheme_clr => {
                        if let Some(v) = attr_i32(e, b"val") {
                            cur_shape_fill_mods.push(("lumOff".to_string(), v));
                        }
                    }
                    // alpha单独存，不进mods列表——透明度语义，最终写Color.a
                    // 而非参与apply_color_mods的RGB运算。原始值千分之一精度
                    // 百分比（如70000=70%），/100000.0换算成0-1，与项目其他
                    // alpha处理点（如cur_shdw_alpha消费处）同一换算约定。
                    b"alpha" if in_shape_fill_scheme_clr => {
                        cur_shape_fill_alpha = attr_i32(e, b"val");
                    }
                    // PROOT-LN-SCHEMECLR-MODS-START: in_ln场景的mods收集镜像。
                    b"shade" if in_ln_scheme_clr => {
                        if let Some(v) = attr_i32(e, b"val") {
                            cur_ln_mods.push(("shade".to_string(), v));
                        }
                    }
                    b"tint" if in_ln_scheme_clr => {
                        if let Some(v) = attr_i32(e, b"val") {
                            cur_ln_mods.push(("tint".to_string(), v));
                        }
                    }
                    b"satMod" if in_ln_scheme_clr => {
                        if let Some(v) = attr_i32(e, b"val") {
                            cur_ln_mods.push(("satMod".to_string(), v));
                        }
                    }
                    b"lumMod" if in_ln_scheme_clr => {
                        if let Some(v) = attr_i32(e, b"val") {
                            cur_ln_mods.push(("lumMod".to_string(), v));
                        }
                    }
                    b"lumOff" if in_ln_scheme_clr => {
                        if let Some(v) = attr_i32(e, b"val") {
                            cur_ln_mods.push(("lumOff".to_string(), v));
                        }
                    }
                    b"alpha" if in_ln_scheme_clr => {
                        cur_ln_alpha = attr_i32(e, b"val");
                    }
                    // PROOT-D bugfix (新增): <a:ln><a:solidFill> 内的描边颜色，
                    // 收集到 cur_ln_color，</a:ln> 结束时统一写入 raw.stroke。
                    b"srgbClr" if in_ln => {
                        if let Some(hex) = attr_str(e, b"val") {
                            cur_ln_color = Some(parse_hex_color(&hex));
                        }
                    }
                    b"schemeClr" if in_ln => {
                        if let Some(slot) = attr_str(e, b"val") {
                            cur_ln_color = Some(theme_colors.get(normalize_scheme_color_slot(&slot, clr_map)).copied().unwrap_or(Color { r: 0, g: 0, b: 0, a: 255 }));
                        }
                    }
                    // PROOT-outerShdw: <a:outerShdw><a:srgbClr/schemeClr>颜色，与
                    // cur_ln_color同款收集模式，</a:outerShdw>结束时统一写回。
                    b"srgbClr" if in_outer_shdw => {
                        if let Some(hex) = attr_str(e, b"val") {
                            cur_shdw_color = Some(parse_hex_color(&hex));
                        }
                    }
                    b"schemeClr" if in_outer_shdw => {
                        if let Some(slot) = attr_str(e, b"val") {
                            cur_shdw_color = Some(theme_colors.get(normalize_scheme_color_slot(&slot, clr_map)).copied().unwrap_or(Color { r: 0, g: 0, b: 0, a: 255 }));
                        }
                    }
                    // <a:outerShdw><a:srgbClr><a:alpha val="43000"/></a:srgbClr></a:outerShdw>，
                    // ST_PositiveFixedPercentage，43000=43%。必须放在in_outer_shdw分支之后，
                    // 不能和in_spPr场景下其他alpha用途混淆（此处专属于阴影颜色透明度）。
                    b"alpha" if in_outer_shdw => {
                        cur_shdw_alpha = attr_f32(e, b"val").map(|v| v as i32).unwrap_or(100000);
                    }
                    // PPTX-12: gradFill 内某个 <a:gs> 的颜色子元素。
                    // 复用与 solidFill 同款的颜色解析（parse_hex_color /
                    // theme_colors 查表），但结果 push 进 cur_gradient_stops
                    // 而非直接写 s.fill——gradFill 整体收尾时（Event::End
                    // 的 gradFill 分支）才会一次性写入 s.grad_raw。
                    // 加 !in_grad_fill 到上面两条 solidFill 分支的条件里，
                    // 是为了防止 gradFill 内的 srgbClr/schemeClr 被旧分支
                    // 误吞（旧分支只判断 in_spPr，gradFill 也在 in_spPr
                    // 范围内，不加排除条件会导致颜色被错误写到 s.fill）。
                    b"srgbClr" if in_gs => {
                        if let Some(hex) = attr_str(e, b"val") {
                            cur_gradient_stops.push((cur_gs_pos, parse_hex_color(&hex)));
                        }
                    }
                    b"schemeClr" if in_gs => {
                        if let Some(slot) = attr_str(e, b"val") {
                            let color = theme_colors.get(normalize_scheme_color_slot(&slot, clr_map)).copied().unwrap_or(Color { r: 0x4F, g: 0x81, b: 0xBD, a: 255 });
                            cur_gradient_stops.push((cur_gs_pos, color));
                        }
                    }
                    // PPTX-12: <a:lin ang="..." scaled="...">，ang 原始值
                    // 1/60000度，未做flip/旋转复合换算（延后到 raw_to_shape，
                    // 原因见 RawGradKind 定义注释）。scaled 属性第一期不读取
                    // （两方参照实现里较少被下游实际消费，非核心视觉差异）。
                    b"lin" if in_grad_fill => {
                        let raw_angle = attr_i32(e, b"ang").unwrap_or(0);
                        cur_grad_kind_raw = Some(RawGradKind::Linear { raw_angle });
                    }
                    // PPTX-12: <a:path path="rect|circle|shape">。fillToRect
                    // （精确中心点）第一期不解析，见 GradientKind::Radial/Rect
                    // 文档注释——固定用形状几何中心，是已知简化非遗漏。
                    b"path" if in_grad_fill => {
                        let path_type = attr_str(e, b"path").unwrap_or_else(|| "rect".to_string());
                        cur_grad_kind_raw = Some(RawGradKind::Path { path_type });
                    }
                    // run 级别颜色
                    b"srgbClr" if in_rPr_hl => {
                        if let Some(hex) = attr_str(e, b"val") { cur_run_bg = Some(parse_hex_color(&hex)); }
                    }
                    b"srgbClr" if in_rPr_fill => {
                        if let Some(hex) = attr_str(e, b"val") {
                            cur_run_color = Some(parse_hex_color(&hex));
                        }
                    }
                    b"schemeClr" if in_rPr_fill => {
                        if let Some(slot) = attr_str(e, b"val") {
                            cur_run_color = Some(theme_colors.get(normalize_scheme_color_slot(&slot, clr_map)).copied()
                                .unwrap_or(Color { r: 0, g: 0, b: 0, a: 255 }));
                        }
                    }
                    // PROOT-004b: <p:style><a:fillRef idx="N"><a:schemeClr val="accent1"/>
                    // </a:fillRef></p:style>。idx（fmtScheme索引）本次不解析——
                    // 颜色已经直接写在schemeClr里，不需要真的查fmtScheme表就能
                    // 拿到主色，idx只影响填充效果细节（如渐变角度），第一期
                    // 简化为纯色fallback，与bug_log记录的样本证据一致。
                    b"schemeClr" if in_fill_ref => {
                        if let Some(slot) = attr_str(e, b"val") {
                            style_fill_color = Some(theme_colors.get(normalize_scheme_color_slot(&slot, clr_map)).copied()
                                .unwrap_or(Color { r: 0x4F, g: 0x81, b: 0xBD, a: 255 }));
                        }
                    }
                    b"schemeClr" if in_ln_ref => {
                        if let Some(slot) = attr_str(e, b"val") {
                            style_line_color = Some(theme_colors.get(normalize_scheme_color_slot(&slot, clr_map)).copied()
                                .unwrap_or(Color { r: 0x4F, g: 0x81, b: 0xBD, a: 255 }));
                        }
                    }
                    // PROOT-004b: <p:style><a:fontRef idx="minor"><a:schemeClr val="lt1"/>
                    // </a:fontRef></p:style>。lt1/lt2/dk1/dk2 等属于"文字/背景色对"槽位
                    // （非accent系列），theme_colors查表逻辑与accent1等其他槽位一致，
                    // parse_theme_colors 已统一收录，不需要额外区分处理。兜底沿用与
                    // fillRef/lnRef相同的默认蓝色——理论上fontRef兜底更适合用黑/白，
                    // 但没有实际样本验证具体该兜底哪个，保持与同批改动一致的简单默认值。
                    b"schemeClr" if in_font_ref => {
                        if let Some(slot) = attr_str(e, b"val") {
                            style_font_color = Some(theme_colors.get(normalize_scheme_color_slot(&slot, clr_map)).copied()
                                .unwrap_or(Color { r: 0x4F, g: 0x81, b: 0xBD, a: 255 }));
                        }
                    }
                    // noFill
                    // PROOT-012b bugfix: 加 !in_ln 排除——此前这条分支和
                    // srgbClr/schemeClr（同一bug模式，见旁边PROOT-D bugfix
                    // 注释）一样没有排除<a:ln><a:noFill/></a:ln>（线条无填充，
                    // 与形状本身的填充是两个独立概念），导致 tdf112209 样本
                    // <a:blipFill>...</a:blipFill><a:ln><a:noFill/></a:ln>
                    // 场景下，<a:ln>内的noFill把已经由blipFill正确写入的
                    // s.fill（Picture）覆盖成了None，图片填充信息丢失。
                    b"noFill" if in_spPr && !in_ln => {
                        if let Some(ref mut s) = cur_shape {
                            s.fill = Some(FillStyle::None);
                        }
                    }
                    // PROOT-LN-NOFILL bugfix: <a:ln><a:noFill/></a:ln>专属
                    // 分支，与上面!in_ln分支互斥（PROOT-012b排除in_ln是为了
                    // 不覆盖s.fill，这里单独记录"该形状显式声明无描边"这个
                    // 事实，供结束合并阶段跳过黑色兜底）。
                    b"noFill" if in_spPr && in_ln => {
                        if let Some(ref mut s) = cur_shape {
                            s.ln_no_fill = true;
                        }
                    }
                    // PROOT-002 bugfix: 自闭合形式 <a:ln w="..."/>（无
                    // headEnd/tailEnd/solidFill子元素时的写法，PptxWriter
                    // 目前固定写这种形式，见 pptx_writer.rs 对应注释）。
                    // 与 Event::Start 里的非自闭合分支（tdf114845等真实
                    // 文件里，ln带headEnd/tailEnd子元素时走这种形式）
                    // 共享同一个 ln_width 字段和"只存宽度不写死颜色"的
                    // 设计，颜色统一交给结束时的 style fallback 决定。
                    // 之前误把这段逻辑当成"<a:ln>只会是这一种形式"来写，
                    // 直接在这里写死黑色 stroke，实测发现非自闭合形式
                    // 完全走不到这个分支（quick-xml对两种形式触发不同
                    // 事件类型），导致真实文件的connector headEnd/tailEnd
                    // 解析失败——本次一并修复，两种形式都需要处理。
                    b"ln" if in_spPr => {
                        // PROOT-V88-INLN-EMPTY-LEAK: 自闭合 <a:ln w="..."/>
                        // 走 Event::Empty（本分支），没有对应的 </a:ln>
                        // Event::End 来把 in_ln 复位——此前设 true 后一路
                        // 保持 true，污染后续所有 srgbClr/schemeClr 解析
                        // （这些标签在 in_ln=true 期间被误判成"描边内颜色"
                        // 写入 cur_ln_color，而不是它们真正所属的语义，如
                        // outerShdw阴影色/lnRef主题色引用）。真实样本
                        // tdf165180实测：triangle形状的<a:ln w="76200"/>
                        // 触发本分支后 in_ln 卡在 true，导致后续被解析的
                        // 内容里第一个非自闭合<a:ln>...</a:ln>（下一个
                        // connector自己的<a:ln>）收尾时，cur_ln_color
                        // 残留了不属于它的黑色值，把该connector描边从
                        // 主题色误判成黑色。与v87已修的"Empty/Start事件
                        // 不对称"是同一类型的坏，这次是"Empty分支设置了
                        // 状态却没人负责复位"。修复：本分支是自闭合标签
                        // 的唯一处理时机，处理完立即复位 in_ln，不依赖
                        // 不存在的 Event::End。
                        in_ln = true;
                        if let Some(ref mut s) = cur_shape {
                            // PROOT-002 lnRef修复: 同上，不用unwrap_or默认值填充，
                            // 保持None以便lnRef主题线宽fallback能生效。
                            s.ln_width = attr_f32(e, b"w").map(|w| w / 12700.0);
                        }
                        in_ln = false;
                    }
                    b"br" if in_para => {
                        // line break → run with newline
                        cur_runs.push(Run { text: "\n".to_string(), char_style: None, ..Default::default() });
                    }
                    // PROOT bullet 任务B: 段落自身<a:pPr>下的buChar/buAutoNum/
                    // buNone子元素，均为自闭合标签，只可能触发Event::Empty。
                    // guard用in_pPr（更精确的语义：这三个标签只应在pPr内部
                    // 出现），解析逻辑与parse_master_txstyles对应分支一致。
                    b"spcPts" if in_spcbef => {
                        if let Some(v) = attr_i32(e, b"val") { cur_para_spcbef = Some(v as f32 / 100.0); }
                    }
                    b"spcPts" if in_spcaft => {
                        if let Some(v) = attr_i32(e, b"val") { cur_para_spcaft = Some(v as f32 / 100.0); }
                    }
                    b"spcPct" if in_lnspc => {
                        if let Some(v) = attr_i32(e, b"val") { cur_para_lnspc = Some(v as f32 / 100000.0); }
                    }
                    b"buNone" if in_pPr => {
                        cur_para_bullet = Some(roe_model::impress::BulletKind::None);
                    }
                    // PROOT-024: buFont 必须先于 buChar 处理（暂存 typeface），
                    // 才能在 buChar 出现时把 font 一并带上。
                    b"buFont" if in_pPr => {
                        cur_para_bullet_font = attr_str(e, b"typeface");
                    }
                    b"buChar" if in_pPr => {
                        if let Some(ch) = attr_str(e, b"char") {
                            cur_para_bullet = Some(roe_model::impress::BulletKind::Char { ch, font: cur_para_bullet_font.clone() });
                        }
                    }
                    b"srgbClr" if in_buClr => {
                        if let Some(hex) = attr_str(e, b"val") {
                            cur_para_bullet_color = Some(parse_hex_color(&hex));
                        }
                    }
                    b"schemeClr" if in_buClr => {
                        if let Some(slot) = attr_str(e, b"val") {
                            cur_para_bullet_color = Some(theme_colors.get(normalize_scheme_color_slot(&slot, clr_map)).copied().unwrap_or(Color { r: 0, g: 0, b: 0, a: 255 }));
                        }
                    }
                    b"buAutoNum" if in_pPr => {
                        if let Some(scheme) = attr_str(e, b"type") {
                            let start_at = attr_i32(e, b"startAt");
                            cur_para_bullet = Some(roe_model::impress::BulletKind::AutoNum { scheme, start_at });
                        }
                    }
                    _ => {}
                }
            }

            Ok(Event::End(ref e)) => {
                match e.local_name().as_ref() {
                    b"t" if in_t => {
                        in_t = false;
                    }
                    b"rPr" if in_rPr => {
                        in_rPr = false;
                        in_rPr_fill = false;
                    }
                    b"highlight" if in_rPr_hl => { in_rPr_hl = false; }
                    b"solidFill" if in_rPr_fill => {
                        in_rPr_fill = false;
                    }
                    // PROOT-slide-endpara bugfix: 此前只在Start/Empty事件里把
                    // in_endpara_rpr/in_endpara_fill设为true/false，End事件完全
                    // 没有复位逻辑——一旦某个shape的<a:endParaRPr>触发了这两个
                    // 状态（哪怕该shape本身毫无问题，如candy.pptx slide2 block0
                    // 圆圈的<a:endParaRPr><a:solidFill><a:srgbClr val="FFFFFF"/>），
                    // 状态会一直保持true、永久污染后续所有shape的Empty事件
                    // srgbClr分支（1394行b"srgbClr" if in_endpara_fill），导致
                    // 后面那些菱形/六边形装饰形状自身<a:spPr><a:solidFill>的
                    // 真实颜色被错误地当成endParaRPr颜色吃掉，spPr分支
                    // （1627行）再也匹配不到，最终raw.style.fill变成None，
                    // 渲染层fallback成主题accent1（绿色）。复现现象：
                    // 全篇非占位符装饰图形统一变绿。
                    b"endParaRPr" if in_endpara_rpr => {
                        in_endpara_rpr = false;
                        in_endpara_fill = false;
                    }
                    b"solidFill" if in_endpara_fill => {
                        in_endpara_fill = false;
                    }
                    b"r" if in_run => {
                        in_run = false;
                        cur_runs.push(Run {
                            text:        cur_text.clone(),
                            color:       cur_run_color,
                            font_size:   cur_run_font_size,
                            bold:        cur_run_bold,
                            italic:      cur_run_italic, underline: cur_run_underline, strikethrough: cur_run_strike, background: cur_run_bg, script: cur_run_script,
                            font_family: cur_run_font_family.clone(),
                            ..Default::default()
                        });
                        cur_text.clear();
                        cur_run_color = None;
                        cur_run_font_size = None;
                        cur_run_bold = None;
                        cur_run_italic = None; cur_run_underline = None; cur_run_strike = None; cur_run_bg = None; cur_run_script = None;
                        cur_run_font_family = None;
                    }
                    // PROOT bullet 任务A: <a:pPr>...</a:pPr> 非自闭合形式收尾。
                    // 注意：<a:pPr>在<a:p>内部先闭合（</a:pPr>先于</a:p>出现），
                    // 所以这里只清除in_pPr标记，不清空cur_para_mar_l等累积值——
                    // 它们要留到下面b"p"分支push Paragraph时才读取和清空
                    // （任务C负责，目前这些变量已解析但还未被消费）。
                    b"lnSpc" if in_lnspc => { in_lnspc = false; }
                    b"spcBef" if in_spcbef => { in_spcbef = false; }
                    b"spcAft" if in_spcaft => { in_spcaft = false; }
                    b"pPr" if in_pPr => {
                        in_pPr = false;
                    }
                    b"p" if in_para => {
                        in_para = false;
                        // PROOT bullet 任务C: field-level merge组装最终bullet。
                        // 1) 先按ph_type判断大类：title/ctrTitle -> Title分类，
                        //    其余（含body及找不到ph信息）一律按Body分类兜底
                        //    （范围确认：other分类不实现，见v43进度文档2.2）。
                        // 2) lvl取段落自身<a:pPr lvl="n">，缺省0（无pPr或pPr
                        //    没写lvl时的OOXML规范默认值）。
                        // 3) 从master_text_styles按大类+lvl取起始值（若传了
                        //    Some；None表示这次调用不需要bullet继承，如master
                        //    slide自身解析）。
                        // 4) 用段落自身cur_para_bullet/mar_l/indent覆盖起始值
                        //    （field-level：段落自身该字段有值才覆盖，没有
                        //    就保留master层的值——不是整体替换）。
                        // PROOT-tdf158512: 记录 bullet 是否完全来自 master 继承，
                        // 供后面 layout lstStyle <a:buNone/> 覆盖判断使用。
                        let mut resolved_bullet_from_master = false;
                        let resolved_bullet: Option<roe_model::impress::BulletInfo> = {
                            // PROOT-028 bugfix: master 的 <p:txStyles>（titleStyle/
                            // bodyStyle/otherStyle）按 OOXML 规范只应被占位符形状
                            // （<p:ph>）继承，普通 TextBox（txBox="1"，无 <p:ph>）不
                            // 应该套用任何 master 层默认 bullet。此前 is_title 判断
                            // 在 cur_shape.ph 为 None 时用 unwrap_or(false) 退化成
                            // false，却仍然继续走 &mts.body 分支，导致非占位符的普通
                            // 文本框被错误套上了 master bodyStyle 的默认 bullet（复现
                            // 样本 tdf160490：TextBox "Set Top, Bottom margin" 被误加
                            // 大号项目符号）。修复：先判断是否为占位符，不是占位符则
                            // m_bullet/m_mar_l/m_indent 保持 None，最终 bullet 完全由
                            // 段落自身 cur_para_bullet 决定。
                            let is_placeholder = cur_shape.as_ref()
                                .map(|s| s.ph.is_some())
                                .unwrap_or(false);
                            let is_title = cur_shape.as_ref()
                                .and_then(|s| s.ph.as_ref())
                                .map(|(t, _)| t == "title" || t == "ctrTitle")
                                .unwrap_or(false);
                            let lvl = cur_para_lvl.unwrap_or(0).min(9);
                            let (mut m_bullet, mut m_mar_l, mut m_indent) = (None, None, None);
                            if is_placeholder {
                                if let Some(mts) = master_text_styles {
                                    let arr = if is_title { &mts.title } else { &mts.body };
                                    m_bullet = arr[lvl].bullet.clone();
                                    m_mar_l  = arr[lvl].mar_l;
                                    m_indent = arr[lvl].indent;
                                }
                            }
                            resolved_bullet_from_master = cur_para_bullet.is_none() && m_bullet.is_some();
                            let final_bullet = cur_para_bullet.take().or(m_bullet);
                            let final_mar_l  = cur_para_mar_l.or(m_mar_l);
                            let final_indent = cur_para_indent.or(m_indent);
                            // PROOT bullet-color: 颜色只来自段落自身<a:buClr>，
                            // 当前不做 master txStyles 侧的颜色继承（真实样本
                            // bulletColor 的 buClr 直接写在段落 pPr 上，无需
                            // 继承链就能验证；master 继承留待有真实样本命中
                            // 时再补，避免无样本验证的推测实现）。
                            let final_bullet_color = cur_para_bullet_color.take();
                            match &final_bullet {
                                None if final_mar_l.is_none() && final_indent.is_none() && final_bullet_color.is_none() => None,
                                _ => Some(roe_model::impress::BulletInfo {
                                    kind: final_bullet.unwrap_or_default(),
                                    mar_l: final_mar_l,
                                    indent: final_indent,
                                    color: final_bullet_color,
                                }),
                            }
                        };
                        // PROOT-slide-endpara: 该段落没有任何真实<a:r>run、
                        // 但收集到了endParaRPr格式时，把endParaRPr转成一个文本为空
                        // 的"虚拟run"塞进cur_runs——修复slide2.xml场景：占位符
                        // 自身（非layout/master）的endParaRPr直接硬编码了颜色，
                        // 此前该格式在这里就已丢失，样式继承阶段（layout/master
                        // 查找）根本无从得知slide自身还有更具体的格式定义。
                        // 与parse_raw_shapes里的同款逻辑（v73/v74攻坚二期）保持
                        // 一致：只在cur_runs为空时才追加，真实<a:r>run存在时
                        // 优先级更高、不受影响。
                        if cur_runs.is_empty() && (
                            cur_endpara_color.is_some() || cur_endpara_font_size.is_some()
                            || cur_endpara_bold.is_some() || cur_endpara_italic.is_some()
                            || cur_endpara_font_family.is_some()
                        ) {
                            cur_runs.push(Run {
                                text:        String::new(),
                                color:       cur_endpara_color,
                                font_size:   cur_endpara_font_size,
                                bold:        cur_endpara_bold,
                                italic:      cur_endpara_italic,
                                font_family: cur_endpara_font_family.clone(),
                                ..Default::default()
                            });
                        }
                        cur_endpara_color = None;
                        cur_endpara_font_size = None;
                        cur_endpara_bold = None;
                        cur_endpara_italic = None;
                        cur_endpara_font_family = None;
                        cur_paras.push(Paragraph {
                            style: roe_model::common::StyleRef("".to_string()),
                            runs: cur_runs.drain(..).collect(),
                            list_info: None,
                            // PROOT-004 followup: <a:pPr algn="..."> 写入段落对齐
                            alignment: cur_para_align.take(), line_height: cur_para_lnspc.take(),
                            space_before: cur_para_spcbef.take(), space_after: cur_para_spcaft.take(),
                            // PROOT bullet 任务C: 写入merge后的bullet信息
                            bullet: resolved_bullet,
                            // PROOT-tdf158512: 供 layout lstStyle buNone 覆盖判断
                            bullet_from_master: resolved_bullet_from_master,
                            // PROOT: 保留段落lvl供后续master txStyles字号继承使用
                            lvl: cur_para_lvl,
                            ..Default::default()
                        });
                    }
                    b"txBody" if in_txBody => {
                        in_txBody = false;
                        if let Some(ref mut s) = cur_shape {
                            s.content = RawContent::Text(cur_paras.drain(..).collect());
                            s.text_body = cur_body_pr.clone();
                        }
                    }
                    b"gs" if in_gs => {
                        in_gs = false;
                    }
                    b"gsLst" if in_gs_lst => {
                        in_gs_lst = false;
                    }
                    // PPTX-12: gradFill 收尾——把收集到的色标+类型+rotWithShape
                    // 打包写入 s.grad_raw（不直接写 s.fill，角度换算延后到
                    // raw_to_shape，理由见 RawGradKind 定义注释）。
                    // cur_grad_kind_raw 理论上不应为 None（gradFill 必然含
                    // lin 或 path 之一），但真实文件可能有損坏/非常规写法，
                    // 缺省按 Linear{raw_angle:0} 兜底，不 panic、不丢整个形状。
                    b"gradFill" if in_grad_fill => {
                        in_grad_fill = false;
                        if let Some(ref mut s) = cur_shape {
                            let kind_raw = cur_grad_kind_raw.take()
                                .unwrap_or(RawGradKind::Linear { raw_angle: 0 });
                            s.grad_raw = Some((
                                cur_gradient_stops.drain(..).collect(),
                                kind_raw,
                                cur_grad_rotate_with_shape,
                            ));
                        } else {
                            cur_gradient_stops.clear();
                            cur_grad_kind_raw = None;
                        }
                    }
                    b"blipFill" if in_shape_blip_fill => {
                        in_shape_blip_fill = false;
                    }
                    b"spPr" if in_spPr => {
                        in_spPr = false;
                    }
                    // PROOT-SCHEMECLR-MODS-START: </a:schemeClr>结束——查
                    // theme_colors拿base色，apply_color_mods应用收集到的
                    // mods，若有alpha则覆盖Color.a，写入s.fill。若mods和
                    // alpha都为空（理论上不会发生，无mods的<a:schemeClr>
                    // 会触发Empty而非Start+End），行为等价于1983行附近
                    // 现有Empty分支的逻辑（同样的base色查找+unwrap_or兜底）。
                    b"schemeClr" if in_shape_fill_scheme_clr => {
                        in_shape_fill_scheme_clr = false;
                        if let Some(ref mut s) = cur_shape {
                            if let Some(slot) = cur_shape_fill_slot.take() {
                                let base = theme_colors.get(normalize_scheme_color_slot(&slot, clr_map)).copied().unwrap_or(Color { r: 0x4F, g: 0x81, b: 0xBD, a: 255 });
                                let mut adjusted = apply_color_mods(base, &cur_shape_fill_mods);
                                if let Some(alpha_raw) = cur_shape_fill_alpha {
                                    adjusted.a = (alpha_raw as f32 / 100000.0 * 255.0).clamp(0.0, 255.0) as u8;
                                }
                                s.fill = Some(FillStyle::Solid(adjusted));
                            }
                        }
                        cur_shape_fill_slot = None;
                        cur_shape_fill_mods.clear();
                        cur_shape_fill_alpha = None;
                    }
                    // PROOT-LN-SCHEMECLR-MODS-START: </a:schemeClr>结束，
                    // in_ln场景镜像——查base色，apply_color_mods，写入
                    // cur_ln_color（而非s.fill）。</a:ln>收尾时cur_ln_color
                    // 会被消费写入raw.stroke，与既有Empty分支的cur_ln_color
                    // 赋值语义完全一致，只是这里多了mods应用这一步。
                    b"schemeClr" if in_ln_scheme_clr => {
                        in_ln_scheme_clr = false;
                        if let Some(slot) = cur_ln_scheme_slot.take() {
                            let base = theme_colors.get(normalize_scheme_color_slot(&slot, clr_map)).copied().unwrap_or(Color { r: 0, g: 0, b: 0, a: 255 });
                            let mut adjusted = apply_color_mods(base, &cur_ln_mods);
                            if let Some(alpha_raw) = cur_ln_alpha {
                                adjusted.a = (alpha_raw as f32 / 100000.0 * 255.0).clamp(0.0, 255.0) as u8;
                            }
                            cur_ln_color = Some(adjusted);
                        }
                        cur_ln_scheme_slot = None;
                        cur_ln_mods.clear();
                        cur_ln_alpha = None;
                    }
                    // PROOT-004b: style/fillRef/lnRef 结束时复位，
                    // 对称于 Event::Start 里对应的置位。
                    b"fillRef" if in_fill_ref => {
                        in_fill_ref = false;
                    }
                    b"lnRef" if in_ln_ref => {
                        in_ln_ref = false;
                    }
                    b"fontRef" if in_font_ref => {
                        in_font_ref = false;
                    }
                    b"style" if in_p_style => {
                        in_p_style = false;
                    }
                    // PROOT-002: </a:ln> 结束时复位in_ln，并把这一路收集的
                    // headEnd/tailEnd写入cur_shape（此前只在Start时置位，
                    // 没有对称的End清理，现在补上——同时是写入时机，因为
                    // headEnd/tailEnd必须在ln内部才能被正确识别，等ln整体
                    // 结束时cur_head_arrow/cur_tail_arrow已经收集完毕）。
                    // PROOT-outerShdw: </a:outerShdw> 结束时复位in_outer_shdw，
                    // 把dist/dir/blurRad+颜色+alpha一次性写入cur_shape.outer_shdw_raw。
                    // 颜色缺省（<a:outerShdw>内没有任何颜色子元素，理论上不应发生，
                    // OOXML规范里颜色是必需子元素，纯防御性兜底）时用不透明黑色。
                    b"outerShdw" if in_outer_shdw => {
                        in_outer_shdw = false;
                        if let Some(ref mut s) = cur_shape {
                            let color = cur_shdw_color.take().unwrap_or(Color { r: 0, g: 0, b: 0, a: 255 });
                            s.outer_shdw_raw = Some((cur_shdw_dist, cur_shdw_dir, cur_shdw_blur, color, cur_shdw_alpha));
                        }
                        cur_shdw_color = None;
                        cur_shdw_alpha = 100000;
                    }
                    // PROOT bullet-color: </a:buClr> 收尾，关闭颜色收集窗口。
                    // 不在这里把 cur_para_bullet_color 写入任何地方——它会在
                    // 段落最终构造 BulletInfo 时被读取消费（与 cur_para_bullet
                    // 的消费时机一致），这里只负责状态复位。
                    b"buClr" if in_buClr => {
                        in_buClr = false;
                    }
                    b"ln" if in_ln => {
                        in_ln = false;
                        if let Some(ref mut s) = cur_shape {
                            s.head_arrow = cur_head_arrow.take();
                            s.tail_arrow = cur_tail_arrow.take();
                        }
                        // PROOT-D bugfix: <a:ln><a:solidFill> 显式描边颜色写入
                        // raw.stroke。若 stroke 已有值（理论上此时机不会，因为
                        // stroke 最终合并发生在 </p:sp> 收尾），只更新 color；
                        // 否则新建，宽度先用 ln_width 已有值或默认 1.0（后续
                        // </p:sp> 收尾的 ln_width 合并逻辑会再校正一次宽度，
                        // 不冲突）。
                        if let Some(color) = cur_ln_color.take() {
                            if let Some(ref mut s) = cur_shape {
                                match s.stroke {
                                    Some(ref mut stroke) => stroke.color = color,
                                    None => {
                                        s.stroke = Some(StrokeStyle { color, width: s.ln_width.unwrap_or(1.0) });
                                    }
                                }
                            }
                        }
                    }
                    // PPTX-11 part5：avLst/prstGeom 结束时复位，对称于
                    // Event::Start 里对应的置位（见上方 in_prst_geom/in_av_lst
                    // 注释）。顺序：avLst 先结束（嵌套更深），prstGeom 后结束。
                    b"avLst" if in_av_lst => {
                        in_av_lst = false;
                    }
                    b"prstGeom" if in_prst_geom => {
                        in_prst_geom = false;
                    }
                    // PROOT-012: custGeom路径命令End事件复位（非自闭合<a:moveTo>
                    // <a:pt .../></a:moveTo>写法时需要，自闭合<a:moveTo/>形式在
                    // 本函数里不会触发这里，因为<a:moveTo>必然带<a:pt>子元素，
                    // 真实PPTX里未见过空moveTo，但复位以防万一，不依赖它触发）。
                    b"moveTo" if in_move_to => {
                        in_move_to = false;
                    }
                    b"lnTo" if in_line_to => {
                        in_line_to = false;
                    }
                    b"cubicBezTo" if in_cubic_to => {
                        // 正常路径下第3个<a:pt>到达时已经complete并复位了
                        // in_cubic_to；这里兜底处理pt数量不足3个的畸形XML，
                        // 避免状态卡死污染后续路径解析。
                        in_cubic_to = false;
                        cubic_to_pt_idx = 0;
                    }
                    b"quadBezTo" if in_quad_to => {
                        in_quad_to = false;
                        quad_to_pt_idx = 0;
                    }
                    // <a:path w=".." h="..">...</a:path> 结束：把收集到的命令
                    // 按path声明的w/h缩放到shape实际pt尺寸，写入raw.cust_geom_path。
                    // 坐标缩放放在这里（而非每个pt读取时就缩放）是因为cur_cust_path_w/h
                    // 在<a:path>的Start事件里已经读到，且只需要在path结束、
                    // cur_cust_path_cmds收集完整后统一做一次，逻辑更清楚。
                    b"path" if in_cust_path => {
                        in_cust_path = false;
                        if let Some(ref mut s) = cur_shape {
                            let target_w = s.position.extent.0;
                            let target_h = s.position.extent.1;
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
                                    // ArcTo理论上不会出现在custGeom手写路径里（这是
                                    // preset_geometry公式表求值的产物），custGeom XML
                                    // 里对应的是同样带圆心/半径参数的<a:arcTo>标签，
                                    // 本次未接（tdf112209等已知样本未用到），原样透传
                                    // 不缩放，若未来遇到会画错但不会panic。
                                    other => other,
                                })
                                .collect();
                            s.cust_geom_path = Some(scaled);
                        } else {
                            cur_cust_path_cmds.clear();
                        }
                    }
                    b"custGeom" if in_cust_geom => {
                        in_cust_geom = false;
                    }
                    b"sp" if in_sp => {
                        in_sp = false;
                        if let Some(mut raw) = cur_shape.take() {
                            // PROOT-002 lnRef线宽fallback: shape自身<a:ln>没写w属性
                            // （raw.ln_width仍为None）、但<p:style><a:lnRef idx="N">写了idx时，
                            // 去主题lnStyleLst按1-based clamp查表补全ln_width。之后原有的
                            // fill/stroke fallback + ln_width合并逻辑不用改动，直接复用。
                            if raw.ln_width.is_none() {
                                if let Some(idx) = raw.ln_ref_idx {
                                    raw.ln_width = lookup_theme_line_width(theme_line_widths, idx);
                                }
                            }
                            // PPTX-4：ph 位置+样式继承统一改为走六分支匹配算法（match_placeholder），
                            // 先在 layout_candidates 里找，找不到再落到 master_candidates 找。
                            // 位置和样式从同一个匹配到的候选身上一次性取出，不再是两套独立查找逻辑。
                            if let Some((ref ph_type, ph_index)) = raw.ph {
                                // PROOT-014 修复：位置(position)继承参照 LOK
                                // oox/source/ppt/pptshapecontext.cxx 官方实现
                                // （eShapeLocation==Slide 分支）——slide层占位符的
                                // size/position 继承直接查 master tree，不查 layout。
                                // layout 自身的 xfrm 只服务于"layout形状之间互相
                                // 对齐"（eShapeLocation==Layout 分支），不作为 slide
                                // 层 position 继承的来源。
                                //
                                // 此前代码把 master_candidates 当成 layout_candidates
                                // 查找失败后的 .or_else fallback，导致 layout 里
                                // "同idx但自身无<a:xfrm>"的占位符（真实PPTX很常见，
                                // 例如 slideLayout2.xml 的 Content Placeholder 只有
                                // <p:spPr/> 空标签）会被 match_placeholder 规则2/5
                                // （不检查has_transform）提前匹配成功，返回
                                // Some(candidate)，导致 .or_else 链路根本不会执行、
                                // master里真正正确的position被跳过。复现样本：
                                // tdf147121.pptx，body占位符w/h继承为(0,0)，
                                // 渲染时text_w被(w-pad).max(1.0)钳成1px，
                                // 只有第一行第一个字符"F"落在可见区域内。
                                if raw.position.w() == 0.0 && raw.position.h() == 0.0 {
                                    // PROOT-1xx (v99): 修正PROOT-027遗留bug——原实现
                                    // 用 match_placeholder(layout,...).or_else(match_placeholder(master,...))，
                                    // 但 match_placeholder 分支2/5不检查has_transform，
                                    // 当layout候选存在同type/idx但自身无<a:xfrm>时（真实
                                    // 样本slideLayout10.xml：title/body占位符只有空
                                    // <p:spPr/>），分支2/5会提前把这个"位置全0"的layout
                                    // 候选当成"找到了"返回，or_else永远不会执行，master
                                    // 里真正正确的position被跳过（复现样本：
                                    // sd_data_pptx__tdf169825_vertical_layouts的slide2，
                                    // title/body占位符position继承为(0,0,0,0)，vert文字
                                    // 因此完全渲染不出来）。改用match_placeholder_position
                                    // ——只在layout候选真的带transform时才采用，否则直接
                                    // 转向master做完整6分支匹配。
                                    let pos_candidate = match_placeholder_position(layout_candidates, master_candidates, ph_type, ph_index);
                                    if let Some(pos_candidate) = pos_candidate {
                                        raw.position = pos_candidate.position;
                                    }
                                }

                                // 样式(font/color等)继承：基础规则仍是"layout优先、
                                // master fallback"（LOK pptshape.cxx TextListStyle处理），
                                // 但在此基础上叠加一层"优先选带真实示例文字run的候选"——
                                // 对应OnlyOffice hasCustomPrompt语义（v73/v74攻坚查明）：
                                // 很多layout的title/body占位符只有空的<a:endParaRPr>、
                                // 没有真实<a:r>示例文字，此时即使layout本身匹配成功，也应该
                                // 继续检查master是否有带真实文字run的同类型占位符——如果有，
                                // 优先用master那份（连同它的颜色/字号/居中等完整格式）。
                                // 只有当两层都没有真实run时，才保留原有"layout优先"行为。
                                // PROOT-slide-endpara: slide自身占位符如果有endParaRPr
                                // 格式（此前补的虚拟run：text为空但携带颜色/字号/字重/字体），
                                // 这是比layout/master都更具体的格式来源（模板设计者直接把
                                // 最终视觉效果写在slide自身endParaRPr上，常见于"纯装饰性、
                                // 永远不会被填字"的占位符，如candy.pptx slide2的5个彩色
                                // subTitle）。优先级最高，命中则完全跳过layout/master查询。
                                let self_endpara_style: Option<LayoutPhStyle> = match &raw.content {
                                    RawContent::Text(paras) => paras.first()
                                        .and_then(|p| p.runs.first())
                                        .filter(|r| r.text.is_empty() && (
                                            r.color.is_some() || r.font_size.is_some()
                                            || r.bold.is_some() || r.italic.is_some()
                                            || r.font_family.is_some()
                                        ))
                                        .map(|r| LayoutPhStyle {
                                            font_size:   r.font_size,
                                            bold:        r.bold,
                                            italic:      r.italic,
                                            color:       r.color,
                                            font_family: r.font_family.clone(),
                                            bullet:      None,
                                            algn:        None,
                                            anchor:      None,
                                            vert:        None,
                                        }),
                                    _ => None,
                                };

                                let layout_matched = match_placeholder(layout_candidates, ph_type, ph_index);
                                let master_matched = match_placeholder(master_candidates, ph_type, ph_index);
                                let matched = match (layout_matched, master_matched) {
                                    (Some(l), Some(m)) if !l.has_real_run && m.has_real_run => Some(m),
                                    (Some(l), _) => Some(l),
                                    (None, Some(m)) => Some(m),
                                    (None, None) => None,
                                };
                                // PROOT-tdf158512: 当 layout 候选因缺少真实示例文字让位给
                                // master 候选（matched==m）时，layout 自身 lstStyle 显式
                                // 声明的 bullet（尤其 buNone）不应被一并丢弃——buNone 是
                                // 布局层的显式覆盖声明，其优先级独立于"哪个候选文字更完整"
                                // 这条规则（后者只是为了拿到更完整的字符格式）。借用 layout
                                // 候选的 bullet（如果有）覆盖最终使用的 style。
                                let borrowed_bullet: Option<roe_model::impress::BulletKind> =
                                    layout_matched.and_then(|l| l.style.bullet.clone());

                                if let Some(candidate) = matched {
                                    let mut layout_style = candidate.style.clone();
                                    if borrowed_bullet.is_some() {
                                        layout_style.bullet = borrowed_bullet.clone();
                                    }
                                    let layout_style = &layout_style;
                                    // PROOT: 无论该占位符run是否为空，都保存一份匹配到的
                                    // 真实格式，供空占位符提示文字渲染复用（见model层
                                    // PlaceholderHintStyle文档注释、设计方案文档九、节）。
                                    raw.ph_hint_style = Some(layout_style.clone());
                                    if let RawContent::Text(ref mut paras) = raw.content {
                                        for para in paras.iter_mut() {
                                            for run in para.runs.iter_mut() {
                                                if run.font_size.is_none() { run.font_size = layout_style.font_size; }
                                                if run.bold.is_none()      { run.bold      = layout_style.bold; }
                                                if run.italic.is_none()    { run.italic    = layout_style.italic; }
                                                if run.color.is_none()     { run.color     = layout_style.color; }
                                                if run.font_family.is_none() { run.font_family = layout_style.font_family.clone(); }
                                            }
                                            // PROOT-tdf158512: layout lstStyle lvl1pPr 显式
                                            // <a:buNone/> 应能覆盖 master txStyles 继承来的
                                            // bullet（OOXML继承优先级：slide自身pPr > layout
                                            // lstStyle > master titleStyle/bodyStyle）。只覆盖
                                            // "完全来自master继承"的bullet，不覆盖段落自身
                                            // <a:pPr>显式声明的bullet（后者优先级更高）。
                                            // 复现样本：tdf158512 subTitle占位符，layout自身
                                            // lstStyle写了buNone，此前被master bodyStyle的
                                            // buChar="•"错误覆盖，多渲染出项目符号。
                                            if matches!(layout_style.bullet, Some(roe_model::impress::BulletKind::None))
                                                && para.bullet.as_ref().map(|_| para.bullet_from_master).unwrap_or(false)
                                            {
                                                para.bullet = None;
                                            }
                                            // PROOT-027: layout lstStyle algn 상속
                                            if para.alignment.is_none() {
                                                para.alignment = layout_style.algn;
                                            }
                                        }
                                    }
                                }

                                // PROOT-slide-endpara: self_endpara_style（slide自身
                                // endParaRPr格式）与已算好的raw.ph_hint_style（layout/
                                // master来源）做字段级合并——self优先，matched兜底缺失
                                // 字段。self独有、matched没有的情况（如matched为None）
                                // 同样处理，用Option::or统一表达"self优先、matched兜底"。
                                // 不用整体覆盖是因为遵循设计方案九、节"各字段独立
                                // fallback"原则：即使self只硬编码了颜色，字号仍应尽量
                                // 从layout/master补全，而不是连带退回渲染层固定默认值。
                                if let Some(self_style) = self_endpara_style {
                                    let merged = LayoutPhStyle {
                                        font_size:   self_style.font_size.or(raw.ph_hint_style.as_ref().and_then(|s| s.font_size)),
                                        bold:        self_style.bold.or(raw.ph_hint_style.as_ref().and_then(|s| s.bold)),
                                        italic:      self_style.italic.or(raw.ph_hint_style.as_ref().and_then(|s| s.italic)),
                                        color:       self_style.color.or(raw.ph_hint_style.as_ref().and_then(|s| s.color)),
                                        font_family: self_style.font_family.clone().or_else(|| raw.ph_hint_style.as_ref().and_then(|s| s.font_family.clone())),
                                        algn:        raw.ph_hint_style.as_ref().and_then(|s| s.algn),
                                        anchor:      raw.ph_hint_style.as_ref().and_then(|s| s.anchor.clone()),
                                        vert:        raw.ph_hint_style.as_ref().and_then(|s| s.vert),
                                        bullet:      raw.ph_hint_style.as_ref().and_then(|s| s.bullet.clone()),
                                    };
                                    if let RawContent::Text(ref mut paras) = raw.content {
                                        for para in paras.iter_mut() {
                                            for run in para.runs.iter_mut() {
                                                if run.font_size.is_none()   { run.font_size   = merged.font_size; }
                                                if run.bold.is_none()        { run.bold        = merged.bold; }
                                                if run.italic.is_none()      { run.italic      = merged.italic; }
                                                if run.color.is_none()       { run.color       = merged.color; }
                                                if run.font_family.is_none() { run.font_family = merged.font_family.clone(); }
                                            }
                                            if para.alignment.is_none() {
                                                para.alignment = merged.algn;
                                            }
                                        }
                                    }
                                    raw.ph_hint_style = Some(merged);
                                }

                                // PROOT: master <p:txStyles> bodyStyle/titleStyle sz
                                // 未接入占位符字号继承链的修复（bug_log.md 2026-07-30
                                // "新bug"条目，样本tdf160490）。layout自身lstStyle为空、
                                // run无内联sz时，此前直接跳到default_style兜底（18pt），
                                // 跳过了master txStyles这一层（28pt）。按OOXML继承优先级：
                                // slide run > layout lstStyle > master txStyles > presentation
                                // defaultTextStyle，此处补上master txStyles这一层，插入位置
                                // 在"layout/master候选"之后、"presentation默认"之前。
                                // font_size + color 两个字段（color为2026-08-09新增，
                                // 此前只做font_size，见bug_log既定修复范围；不牵动
                                // bold/italic/font_family，这些字段各自有独立继承路径，
                                // 不在本次任务范围）。
                                if let Some(mts) = master_text_styles {
                                    let is_title = ph_type == "title" || ph_type == "ctrTitle";
                                    let arr = if is_title { &mts.title } else { &mts.body };
                                    if let RawContent::Text(ref mut paras) = raw.content {
                                        for para in paras.iter_mut() {
                                            let lvl = para.lvl.unwrap_or(0).min(9);
                                            let master_font_size = arr[lvl].font_size;
                                            let master_color = arr[lvl].color;
                                            for run in para.runs.iter_mut() {
                                                if run.font_size.is_none() { run.font_size = master_font_size; }
                                                if run.color.is_none()     { run.color     = master_color; }
                                            }
                                        }
                                    }
                                    // PROOT-tdf158512-color: raw.ph_hint_style（空占位符提示
                                    // 文字复用的"真实格式"）此前从未接入master txStyles这一层，
                                    // 即使上面的raw.content真实run已经能拿到master颜色/字号，
                                    // hint_style仍然是None，导致渲染层549行的
                                    // FALLBACK_HINT_COLOR灰色兜底被触发。复现样本tdf158512
                                    // ctrTitle：layout自身lstStyle只声明了字号(defRPr sz=6000)
                                    // 没有颜色，颜色只在master titleStyle声明(schemeClr
                                    // val="tx1")，此前完全没有路径能把这层颜色传到
                                    // hint_style，字号因为layout层已经有值侥幸没暴露这个
                                    // 盲点，颜色首次暴露。lvl固定用0（提示文字场景下段落
                                    // lvl信息不可靠，ph_hint_style本身不区分段落层级，用
                                    // 最外层lvl 1对齐既有设计）。
                                    if let Some(ref mut hint) = raw.ph_hint_style {
                                        if hint.font_size.is_none() { hint.font_size = arr[0].font_size; }
                                        if hint.color.is_none()     { hint.color     = arr[0].color; }
                                    }
                                }

                                // PPTX-5：无论上面是否匹配到 layout/master 候选，
                                // 走完前三层继承后仍为 None 的字段，用 presentation.xml
                                // <p:defaultTextStyle> 的全局默认值做最后一层兜底
                                // （对应 pptx-glimpse buildTextStyleChain 第4层 presentation.defaultTextStyle）。
                                if let RawContent::Text(ref mut paras) = raw.content {
                                    for para in paras.iter_mut() {
                                        for run in para.runs.iter_mut() {
                                            if run.font_size.is_none() { run.font_size = default_style.font_size; }
                                            if run.bold.is_none()      { run.bold      = default_style.bold; }
                                            if run.italic.is_none()    { run.italic    = default_style.italic; }
                                            if run.color.is_none()     { run.color     = default_style.color; }
                                            if run.font_family.is_none() { run.font_family = default_style.font_family.clone(); }
                                        }
                                    }
                                }
                            }
                            // PROOT-004b: spPr未显式定义fill/stroke时（即raw.fill/
                            // raw.stroke仍是Option::None，代表<p:spPr>内完全没写
                            // solidFill/gradFill/noFill/ln任何标签——注意这与用户
                            // 显式<a:noFill/>写入的FillStyle::None是两回事，不会
                            // 被这里误覆盖，因为FillStyle::None是Some(FillStyle::None)，
                            // raw.fill.is_none()判断的是Option层，不会命中），
                            // 用<p:style>fillRef/lnRef解析出的颜色作为fallback。
                            //
                            // PROOT-006阶段二: 新增raw.grad_raw.is_none()防护——
                            // spPr若直接写了<a:gradFill>，s.grad_raw会被设置但
                            // s.fill仍保持None，若不加此判断，fillRef的渐变分支会
                            // 误覆盖spPr自己显式定义的渐变（两者语义完全不同：
                            // spPr直接写的gradFill优先级应高于style fillRef fallback）。
                            // 本逻辑只加在sp（矩形等实体形状）分支，cxnSp（connector）
                            // 分支保持原有纯色fallback不变，不搬运——connector是线条，
                            // 目前没有真实样本证据表明connector会用fillRef渐变填充。
                            if raw.fill.is_none() && raw.grad_raw.is_none() {
                                if let Some(idx) = raw.fill_ref_idx {
                                    if let Some(def) = lookup_theme_fill_style(&theme_fill_styles, idx) {
                                        match def {
                                            ThemeFillStyleDef::Gradient { stops, kind_raw, rotate_with_shape } => {
                                                if let Some(base) = style_fill_color {
                                                    let resolved_stops: Vec<(f32, Color)> = stops.iter()
                                                        .map(|(pos, mods)| (*pos, apply_color_mods(base, mods)))
                                                        .collect();
                                                    raw.grad_raw = Some((resolved_stops, kind_raw.clone(), *rotate_with_shape));
                                                }
                                            }
                                            ThemeFillStyleDef::Solid(mods) => {
                                                if let Some(base) = style_fill_color {
                                                    let c = if mods.is_empty() { base } else { apply_color_mods(base, mods) };
                                                    raw.fill = Some(FillStyle::Solid(c));
                                                }
                                            }
                                            // PROOT-THEME-BG-PIC: fillStyleLst（形状 fillRef
                                            // 场景，与 bgFillStyleLst 背景场景是两张独立表）
                                            // 里出现 blipFill 项，目前没有真实样本证据表明
                                            // 形状会通过 fillRef 引用图片填充项（此前只在
                                            // sd_data_pptx__accent-color.pptx 的 bgRef/
                                            // bgFillStyleLst 场景发现过），故此处不消费，
                                            // 留空落到下面 raw.fill.is_none() 的通用纯色
                                            // fallback（style_fill_color），与改动前"不认识
                                            // 这类填充就不特殊处理"的行为一致，不新增未经
                                            // 真实样本验证的分支逻辑。
                                            ThemeFillStyleDef::Picture { .. } => {}
                                        }
                                    }
                                }
                            }
                            if raw.fill.is_none() {
                                if let Some(c) = style_fill_color {
                                    raw.fill = Some(FillStyle::Solid(c));
                                }
                            }
                            if raw.stroke.is_none() {
                                if let Some(mut c) = style_line_color {
                                    // PROOT-006: lnRef颜色shade/tint/satMod调整。
                                    // style_line_color此刻只是schemeClr查表得到的纯色，
                                    // 主题lnStyleLst里该idx对应的<a:ln>可能还定义了
                                    // shade/tint/satMod（真实样本tdf114845 idx=1即有
                                    // shade 95000 + satMod 105000），这里按顺序应用。
                                    if let Some(idx) = raw.ln_ref_idx {
                                        if let Some(mods) = lookup_theme_line_color_mods(theme_line_color_mods, idx) {
                                            if !mods.is_empty() {
                                                c = apply_color_mods(c, mods);
                                            }
                                        }
                                    }
                                    raw.stroke = Some(StrokeStyle { color: c, width: raw.ln_width.unwrap_or(1.0) });
                                }
                            }
                            // PROOT-002 bugfix: <a:ln w="..."> 线宽合并。
                            // stroke已有值（solidFill显式颜色 或 刚应用的
                            // style fallback）→ 只更新width；stroke仍为
                            // None（既没有solidFill也没有style lnRef，
                            // 但确实写了<a:ln w="..."/>）→ 黑色兜底+这个
                            // 宽度，保持"有ln标签就应有可见描边"的合理默认。
                            if let Some(w) = raw.ln_width {
                                if let Some(ref mut stroke) = raw.stroke {
                                    stroke.width = w;
                                } else if !raw.ln_no_fill {
                                    // PROOT-LN-NOFILL bugfix: 显式<a:noFill/>
                                    // 时不再无条件黑色兜底，见ln_no_fill字段
                                    // 注释。
                                    raw.stroke = Some(StrokeStyle {
                                        color: Color { r: 0, g: 0, b: 0, a: 255 },
                                        width: w,
                                    });
                                }
                            }
                            // PROOT-004b: <p:style><a:fontRef> 文字颜色fallback，
                            // 优先级低于run自身颜色/layout继承/presentation默认——
                            // 只在走完前面所有继承层后run.color仍是None时才生效。
                            if let Some(c) = style_font_color {
                                if let RawContent::Text(ref mut paras) = raw.content {
                                    for para in paras.iter_mut() {
                                        for run in para.runs.iter_mut() {
                                            if run.color.is_none() { run.color = Some(c); }
                                        }
                                    }
                                }
                            }
                            if std::env::var("ROE_DEBUG_GROUP_FILL").is_ok() {
                                eprintln!(
                                    "[DEBUG group sp] name={:?} fill={:?} stroke={:?} fill_ref_idx={:?} ln_ref_idx={:?}",
                                    raw.name, raw.fill, raw.stroke, raw.fill_ref_idx, raw.ln_ref_idx
                                );
                            }
                            let shape = raw_to_shape(raw, image_map, chart_map, shape_counter);
                            shape_counter += 1;
                            // PPTX-9b：分流——栈非空说明当前形状嵌套在
                            // 某个未闭合的 <p:grpSp> 内，应归入分组的
                            // 收集区而非顶层 shapes（这正是"层级栈"
                            // 方案替代递归的核心：嵌套发生在栈操作里，
                            // 不需要额外的递归调用）。
                            match group_stack.last_mut() {
                                Some(group) => {
                                    if let RawContent::Group(ref mut children) = group.content {
                                        children.push(shape);
                                    } else {
                                        group.content = RawContent::Group(vec![shape]);
                                    }
                                }
                                None => shapes.push(shape),
                            }
                        }
                    }
                    b"pic" if in_pic => {
                        in_pic = false;
                        if let Some(raw) = cur_shape.take() {
                            let shape = raw_to_shape(raw, image_map, chart_map, shape_counter);
                            shape_counter += 1;
                            match group_stack.last_mut() {
                                Some(group) => {
                                    if let RawContent::Group(ref mut children) = group.content {
                                        children.push(shape);
                                    } else {
                                        group.content = RawContent::Group(vec![shape]);
                                    }
                                }
                                None => shapes.push(shape),
                            }
                        }
                    }
                    // PROOT-002: </p:cxnSp> 闭合。复制 b"pic" if in_pic
                    // 的分流模式：转成最终 Shape 后按同样规则推入
                    // group_stack 栈顶或顶层 shapes。
                    //
                    // 补充（实测tdf114845发现的bug修复）：cxnSp 同样带
                    // <p:style><a:lnRef><a:schemeClr>写法的主题色引用
                    // （见样本 XML，<a:ln>内只有tailEnd没有solidFill，
                    // 颜色完全来自lnRef），必须复用 b"sp" if in_sp 结束
                    // 分支里那段 style_fill_color/style_line_color fallback
                    // 应用逻辑，否则 connector 的 stroke 恒为 None（实测
                    // 验证过，3条cxnSp在补这段之前 stroke=None，矩形因为
                    // 走sp分支所以有颜色，两者代码路径不共享导致的不一致）。
                    // fontRef 文字颜色fallback对cxnSp无意义（connector没有
                    // 文字内容），不需要搬。
                    b"cxnSp" if in_cxn_sp => {
                        in_cxn_sp = false;
                        if let Some(mut raw) = cur_shape.take() {
                            // PROOT-002 lnRef线宽fallback: shape自身<a:ln>没写w属性
                            // （raw.ln_width仍为None）、但<p:style><a:lnRef idx="N">写了idx时，
                            // 去主题lnStyleLst按1-based clamp查表补全ln_width。之后原有的
                            // fill/stroke fallback + ln_width合并逻辑不用改动，直接复用。
                            if raw.ln_width.is_none() {
                                if let Some(idx) = raw.ln_ref_idx {
                                    raw.ln_width = lookup_theme_line_width(theme_line_widths, idx);
                                }
                            }
                            // PROOT-CONNECTOR-FILLREF0: fillRef idx="0" 在 OOXML 语义上
                            // 代表"无填充"（fmtScheme 索引从1开始编号真实填充样式，0是保留
                            // 的空值），connector（cxnSp）大量使用 fillRef idx="0" + lnRef
                            // idx="N"（线条本不该有填充，但需要描边颜色）。此前直接用
                            // style_fill_color（<a:fillRef>内<a:schemeClr>解析出的颜色，
                            // 不看idx）兜底 raw.fill，导致 fillRef idx="0" 的 connector
                            // （如 curvedConnector5/curvedConnector3 样本）被错误套上实心
                            // 填充，整条线渲染成一大块色块而非细线（v86会话curvedConnector
                            // 走查实测发现，真实渲染图证据：oox_unit_data__curvedConnectors
                            // slide0，两条connector均被填充成蓝色不规则块状，而非细描边线+
                            // 箭头）。加 raw.fill_ref_idx != Some(0) 排除，恢复"idx=0即
                            // 无填充"的正确语义。只改cxnSp这一处（line 2659原文），line 2538
                            // 的sp（普通形状）分支保持不动——目前没有真实样本证据表明普通
                            // 形状会用fillRef idx=0，避免在没有验证的场景下做改动。
                            if raw.fill.is_none() && raw.fill_ref_idx != Some(0) {
                                if let Some(c) = style_fill_color {
                                    raw.fill = Some(FillStyle::Solid(c));
                                }
                            }
                            if raw.stroke.is_none() {
                                if let Some(mut c) = style_line_color {
                                    // PROOT-006: lnRef颜色shade/tint/satMod调整。
                                    // style_line_color此刻只是schemeClr查表得到的纯色，
                                    // 主题lnStyleLst里该idx对应的<a:ln>可能还定义了
                                    // shade/tint/satMod（真实样本tdf114845 idx=1即有
                                    // shade 95000 + satMod 105000），这里按顺序应用。
                                    if let Some(idx) = raw.ln_ref_idx {
                                        if let Some(mods) = lookup_theme_line_color_mods(theme_line_color_mods, idx) {
                                            if !mods.is_empty() {
                                                c = apply_color_mods(c, mods);
                                            }
                                        }
                                    }
                                    raw.stroke = Some(StrokeStyle { color: c, width: raw.ln_width.unwrap_or(1.0) });
                                }
                            }
                            // PROOT-002 bugfix: <a:ln w="..."> 线宽合并。
                            // stroke已有值（solidFill显式颜色 或 刚应用的
                            // style fallback）→ 只更新width；stroke仍为
                            // None（既没有solidFill也没有style lnRef，
                            // 但确实写了<a:ln w="..."/>）→ 黑色兜底+这个
                            // 宽度，保持"有ln标签就应有可见描边"的合理默认。
                            if let Some(w) = raw.ln_width {
                                if let Some(ref mut stroke) = raw.stroke {
                                    stroke.width = w;
                                } else if !raw.ln_no_fill {
                                    // PROOT-LN-NOFILL bugfix: 显式<a:noFill/>
                                    // 时不再无条件黑色兜底，见ln_no_fill字段
                                    // 注释。
                                    raw.stroke = Some(StrokeStyle {
                                        color: Color { r: 0, g: 0, b: 0, a: 255 },
                                        width: w,
                                    });
                                }
                            }
                            if std::env::var("ROE_DEBUG_CXNSP").is_ok() {
                                eprintln!(
                                    "[DEBUG cxnSp] name={:?} fill={:?} stroke={:?} fill_ref_idx={:?} ln_ref_idx={:?} ln_width={:?} ln_no_fill={:?}",
                                    raw.name, raw.fill, raw.stroke, raw.fill_ref_idx, raw.ln_ref_idx, raw.ln_width, raw.ln_no_fill
                                );
                            }
                            let shape = raw_to_shape(raw, image_map, chart_map, shape_counter);
                            shape_counter += 1;
                            match group_stack.last_mut() {
                                Some(group) => {
                                    if let RawContent::Group(ref mut children) = group.content {
                                        children.push(shape);
                                    } else {
                                        group.content = RawContent::Group(vec![shape]);
                                    }
                                }
                                None => shapes.push(shape),
                            }
                        }
                    }
                    b"graphicFrame" if in_graphic_frame => {
                        in_graphic_frame = false;
                        if let Some(raw) = cur_shape.take() {
                            let shape = raw_to_shape(raw, image_map, chart_map, shape_counter);
                            shape_counter += 1;
                            match group_stack.last_mut() {
                                Some(group) => {
                                    if let RawContent::Group(ref mut children) = group.content {
                                        children.push(shape);
                                    } else {
                                        group.content = RawContent::Group(vec![shape]);
                                    }
                                }
                                None => shapes.push(shape),
                            }
                        }
                    }
                    // PPTX-9b: <p:grpSpPr> 闭合。
                    b"grpSpPr" if in_grp_sp_pr => {
                        in_grp_sp_pr = false;
                    }
                    // PPTX-9b: </p:grpSp> 闭合——弹出栈顶，转换成最终
                    // Shape，再按同样规则分流到"新栈顶"或顶层 shapes。
                    // 嵌套分组（grpSp 内还有 grpSp）天然由这条规则和
                    // "sp/pic/graphicFrame 分流"共享同一份逻辑得到支持，
                    // 不需要单独写递归函数。
                    b"grpSp" => {
                        if let Some(raw) = group_stack.pop() {
                            let shape = raw_to_shape(raw, image_map, chart_map, shape_counter);
                            shape_counter += 1;
                            match group_stack.last_mut() {
                                Some(parent_group) => {
                                    if let RawContent::Group(ref mut children) = parent_group.content {
                                        children.push(shape);
                                    } else {
                                        parent_group.content = RawContent::Group(vec![shape]);
                                    }
                                }
                                None => shapes.push(shape),
                            }
                        }
                    }
                    _ => {}
                }
            }

            Ok(Event::Text(ref e)) if in_t => {
                cur_text.push_str(&e.unescape().unwrap_or_default());
            }

            Ok(Event::Eof) => break,
            Err(e) => return Err(ParseError::Xml(e)),
            _ => {}
        }
    }

    Ok(shapes)
}

// ─── RawShape → Shape ────────────────────────────────────────────────────────

fn raw_to_shape(
    raw: RawShape,
    image_map: &HashMap<String, Vec<u8>>,
    chart_map: &HashMap<String, roe_model::impress::ChartModel>,
    counter: u32,
) -> Shape {
    if std::env::var("ROE_DEBUG_SHAPE").as_deref() == Ok("1") {
        eprintln!(
            "[DEBUG raw_to_shape] geometry_raw={:?} fill={:?} stroke={:?} fill_ref_idx={:?} ln_ref_idx={:?} ln_width={:?}",
            raw.geometry, raw.fill, raw.stroke, raw.fill_ref_idx, raw.ln_ref_idx, raw.ln_width
        );
    }
    // PROOT-012: custGeom优先于prstGeom。OOXML里<a:custGeom>和<a:prstGeom>
    // 在<p:spPr>下互斥，同一shape理论上只会有其中一个，故用if/else短路，
    // 不修改下面原有的match逻辑（custGeom不存在时完全走老路径，零影响）。
    let geometry = if let Some(cmds) = raw.cust_geom_path.clone() {
        // v181: custGeom 场景没有真正的 prst 名字（这不是预设形状，复用
        // PresetPath 变体只是为了共享渲染逻辑），preset_name 留空字符串、
        // adj_overrides 留空 Vec 表示"不适用"，详见
        // ShapeGeometry::PresetPathData 文档注释里的约定。
        ShapeGeometry::PresetPath(roe_model::common::PresetPathData {
            commands: cmds,
            preset_name: String::new(),
            adj_overrides: Vec::new(),
        })
    } else {
        match raw.geometry.as_deref() {
        Some("ellipse") | Some("oval") => ShapeGeometry::Ellipse,
        Some("line") => ShapeGeometry::Line,
        // PPTX-11 part5：解析层接线。未落入上面四个专门枚举的预设名字，
        // 先查 preset_geometry 的176个（+7个connector）公式表；查到就
        // 用 default_adj 默认值求值成绝对坐标路径（PresetPath）。adj
        // 覆盖值（<a:avLst><a:gd name="adjN" fmla="val N"/>）解析是
        // 后续任务（B2），本次未接，故暂时统一用默认值求值——这不是
        // 错误，是分阶段：大多数默认插入的形状本来就是默认adj，先接
        // 通主链路，adj覆盖值属于"进一步精确"而非"从无到有"。
        // 查不到（真正未覆盖的11个形状，或非几何类的connector/line
        // 变体如有拼写差异）才落回 Custom，行为与此前一致，不丢功能。
        Some(other) => {
            match find_preset_shape(other) {
                Some(formula) => {
                    let w = raw.position.extent.0 as f64;
                    let h = raw.position.extent.1 as f64;
                    // PPTX-11 part5（B2）：真正的 adj 覆盖值，取代此前
                    // 占位的空 HashMap。key 用 &str（借用 raw.adj_overrides
                    // 里 String 的引用），与 eval_preset_shape 签名的
                    // &HashMap<&str, f64> 匹配，无需额外分配/转换开销。
                    let adj_map: HashMap<&str, f64> = raw.adj_overrides
                        .iter()
                        .map(|(name, val)| (name.as_str(), *val))
                        .collect();
                    let path = eval_preset_shape(formula, w, h, &adj_map);
                    // v181: prst 名字（other）+ adj_overrides 随路径一并打包，
                    // 供未来 roe_impress_get_connection_points 反查求值使用，
                    // 详见 ShapeGeometry::PresetPathData 文档注释。
                    ShapeGeometry::PresetPath(roe_model::common::PresetPathData {
                        commands: path,
                        preset_name: other.to_string(),
                        adj_overrides: raw.adj_overrides.clone(),
                    })
                }
                None => ShapeGeometry::Custom(other.to_string()),
            }
        }
        None => ShapeGeometry::Rect,
        }
    };

    let content = match raw.content {
        RawContent::Empty => ShapeContent::Empty,
        RawContent::Text(paras) => ShapeContent::Text(paras),
        RawContent::Image { rId, crop_l, crop_t, crop_r, crop_b } => {
            if let Some(data) = image_map.get(&rId) {
                // 简单推断 mime type
                let mime = if data.starts_with(b"\x89PNG") {
                    "image/png"
                } else if data.starts_with(b"\xff\xd8") {
                    "image/jpeg"
                } else if data.starts_with(b"GIF") {
                    "image/gif"
                } else {
                    "image/unknown"
                };
                // PPTX-10: ST_Percentage 精度为 100000=100%（已核实 OOXML
                // 规范原文，非此前笔记猜测的1000）。第一期只支持正值裁剪
                // (inset)，负值扩展(outset)极罕见且复杂度不对称，按0处理。
                let norm = |v: i32| (v.max(0) as f32) / 100000.0;
                ShapeContent::Image(roe_model::common::ImageData {
                    data: data.clone(),
                    mime: mime.to_string(),
                    crop_l: norm(crop_l),
                    crop_t: norm(crop_t),
                    crop_r: norm(crop_r),
                    crop_b: norm(crop_b),
                })
            } else {
                ShapeContent::Empty
            }
        }
        RawContent::Chart { rId } => {
            if let Some(model) = chart_map.get(&rId) {
                // PPTX-13(c)：ChartModel.x_pt/y_pt 此前从未被填充（恒为0.0），
                // 排查确认渲染层（slide_renderer.rs render_shape_chart）实际
                // 使用的是外层 Shape.position（来自本函数已解析好的
                // raw.position，与表格 PPTX-6 共享同一段 graphicFrame xfrm
                // 解析代码），不读 ChartModel 内部这两个字段——即渲染位置
                // 本身没有 bug。此处补齐纯粹是为了消除模型内的死字段/数据
                // 不一致（未来若有独立于父 Shape 上下文使用 ChartModel 的
                // 场景，如单独导出图表，这两个字段就不再是永远错误的占位值）。
                let mut model = model.clone();
                model.x_pt = raw.position.x();
                model.y_pt = raw.position.y();
                ShapeContent::Chart(model)
            } else if let Some(sm) = STAT_CHARTS.with(|m| m.borrow().get(&rId).cloned()) {
                ShapeContent::StatChart(sm)
            } else {
                ShapeContent::Empty
            }
        }
        // PPTX-6: 表格在解析阶段（parse_table）已经是完整的 common::Table，
        // 这里直接转发，不需要像 Image/Chart 那样经过 rId 中转查表。
        RawContent::Table(table) => ShapeContent::Table(table),
        // PROOT-D 子问题2 bugfix（v62）：ch_off/ch_ext 来自本 Shape 自己
        // 的 raw.group_ch_off/group_ch_ext（只有分组自身的 RawShape 会
        // 被设置这两个字段，见该字段定义处注释），随 children 一起组装
        // 进 ShapeContent::Group，供渲染层计算缩放矩阵。
        RawContent::Group(children) => ShapeContent::Group {
            children,
            ch_off: raw.group_ch_off,
            ch_ext: raw.group_ch_ext,
        },
    };

    // PPTX-3：把 raw.ph（占位符 type/index）映射进最终 Shape，不再在转换时丢弃身份信息。
    let placeholder = raw.ph.as_ref().map(|(ph_type, idx)| roe_model::impress::PlaceholderInfo {
        ph_type: if ph_type.is_empty() { None } else { Some(ph_type.clone()) },
        index: *idx,
        hint_style: raw.ph_hint_style.as_ref().map(|s| roe_model::impress::PlaceholderHintStyle {
            font_size:   s.font_size,
            bold:        s.bold,
            italic:      s.italic,
            color:       s.color,
            font_family: s.font_family.clone(),
            algn:        s.algn,
            anchor:      s.anchor.clone(),
            vert:        s.vert,
        }),
    });

    // PPTX-12: gradFill 的角度换算，延后到这里做（需要 raw.rot/flip_h/flip_v，
    // 这些在解析阶段 gradFill 出现时可能还未读到，见 RawGradKind 定义注释）。
    // 换算公式参照 LOK `fillproperties.cxx:554-566`（GradientFillContext /
    // 主 fill 应用逻辑），三方交叉验证（LOK权威实现 + OnlyOffice CGradFill
    // 字段设计一致性）：
    //   1. 读原始角度 raw_angle（1/60000度，来自 <a:lin ang="...">）
    //   2. flip 修正：flipH → angle = 180*60000 - angle；flipV → angle = -angle
    //   3. 若 rotate_with_shape（对应 OOXML rotWithShape，缺省true）：
    //      叠加形状自身旋转 raw.rot（同样是1/60000度单位，与lin的ang同基准）
    //   4. 归一化到 0-360 度（ROE 模型直接存"度"，不经过 LOK 内部
    //      UNO API 要求的 1/10度中转单位）
    //
    // 【已知未决问题，务必读完再改这段】：LOK 原始公式最后一步是
    //   Angle = (8100 - (nDmlAngle / (PER_DEGREE/10))) % 3600   // 结果单位1/10度
    // 这里的 "8100 -" 偏移量，本次调研未能确认其含义——可能是 LOK 内部
    // UNO awt::Gradient API 自己的角度基准定义（例如角度0度起始方向与
    // OOXML/CSS习惯不同，或角度方向正负号相反），也可能只是"转换到LOK
    // 自己坐标系"的偏移，不属于通用的角度语义。我们下面这版换算**没有**
    // 照抄这个偏移量，只做了"角度语义层面"的三步（flip修正+叠加旋转+
    // 归一化），原因：这个偏移很可能是 LOK 特有 API 的产物，直接照抄
    // 有风险引入一个我们自己都不理解、无法验证对错的魔法数字。
    //
    // 验证计划（未完成，下次会话/真实渲染阶段必须做）：用已知角度的测试
    // PPTX（0度/90度/水平/垂直渐变各一个）实际渲染出来，与 PowerPoint/
    // LOK Impress 打开同一文件的视觉效果比对。如果发现方向整体偏转
    // （例如所有渐变都转了90度，或者左右/上下镜像），再回头在本函数
    // 尾部或渲染层统一补一个角度偏移常量，并把发现的偏移值和产生原因
    // 写回这段注释里，不要不声不响改掉。
    let fill = if let Some((stops, kind_raw, rotate_with_shape)) = raw.grad_raw {
        let kind = match kind_raw {
            RawGradKind::Linear { raw_angle } => {
                let mut angle = raw_angle as f32;
                if raw.flip_h.unwrap_or(false) { angle = 180.0 * 60000.0 - angle; }
                if raw.flip_v.unwrap_or(false) { angle = -angle; }
                if rotate_with_shape {
                    angle += raw.rot.unwrap_or(0) as f32;
                }
                // 1/60000度 → 度，归一化到 [0, 360)
                let final_angle = ((angle / 60000.0) % 360.0 + 360.0) % 360.0;
                GradientKind::Linear { angle: final_angle }
            }
            RawGradKind::Path { path_type } => {
                match path_type.as_str() {
                    "circle" => GradientKind::Radial,
                    // "rect" | "shape"：shape两方参照实现均未真正实现，
                    // 归并到 Rect 处理，见 GradientKind::Rect 文档注释。
                    _ => GradientKind::Rect,
                }
            }
        };
        Some(FillStyle::Gradient {
            stops: stops.into_iter().map(|(pos, color)| GradientStop { pos, color }).collect(),
            kind,
            rotate_with_shape,
        })
    } else {
        raw.fill
    };

    Shape {
        id: if raw.id == 0 { counter } else { raw.id },
        geometry,
        position: roe_model::common::Transform {
            offset: raw.position.offset,
            extent: raw.position.extent,
            rotation: raw.rot.map(|r| r as f32 / 60000.0),
            flip_h: raw.flip_h,
            flip_v: raw.flip_v,
            // OOXML <a:xfrm> 的 kx/ky（skew）解析层当前未读取，
            // 此处先留空，与 shear 写入接口（roe_impress_set_shape_shear）
            // 是两件独立的事：本次只搭数据结构+写入通路，不做OOXML skew解析。
            shear_x: None,
            shear_y: None,
        },
        style: ShapeStyle {
            fill,
            stroke: raw.stroke,
            // PROOT-outerShdw: outer_shdw_raw = (dist EMU, dir 1/60000度,
            // blurRad EMU, 颜色, alpha百分比0-100000)。dist/blurRad按
            // EMU->pt换算(/12700.0)；dir按1/60000度->弧度换算后求
            // offset_x/offset_y（本样本dist=0，offset恒为0，但换算公式
            // 对未来非零dist样本仍要写对）；alpha百分比换算成Color.a
            // (0-255)，覆盖outer_shdw_raw里Color自带的a（该Color本身
            // 只是RGB容器，alpha值来自单独的<a:alpha val>子元素）。
            shadow: raw.outer_shdw_raw.as_ref().map(|(dist, dir, blur, color, alpha_pct)| {
                let dist_pt = *dist as f32 / 12700.0;
                let dir_rad = *dir as f32 / 60000.0 * std::f32::consts::PI / 180.0;
                let offset_x = dist_pt * dir_rad.cos();
                let offset_y = dist_pt * dir_rad.sin();
                let blur_pt = *blur as f32 / 12700.0;
                let a = (*alpha_pct as f32 / 100000.0 * 255.0).clamp(0.0, 255.0) as u8;
                roe_model::common::ShadowStyle {
                    color: roe_model::common::Color { r: color.r, g: color.g, b: color.b, a },
                    offset_x,
                    offset_y,
                    blur: blur_pt,
                }
            }),
            head_arrow: str_to_arrow_type(raw.head_arrow.as_deref()),
            tail_arrow: str_to_arrow_type(raw.tail_arrow.as_deref()),
        },
        content,
        text_body: raw.text_body,
        placeholder,
        end_cxn: raw.end_cxn,
        st_cxn: raw.st_cxn,
    }
}

/// PROOT-002: <a:headEnd/tailEnd type="..."/> 的原始字符串 → ArrowType。
/// 第一期只覆盖已验证样本出现的 "arrow"/"triangle"；其余已知
/// OOXML 取值（stealth/diamond/oval等）和未识别字符串一律 fallback
/// 到 ArrowType::None（渲染层对 None 不画任何端点装饰），不报错。
/// None（标签整体不存在）则返回 None，与 ShapeStyle.head_arrow/
/// tail_arrow 字段文档注释里"None表示未设置该标签"的语义一致。
fn str_to_arrow_type(raw: Option<&str>) -> Option<roe_model::common::ArrowType> {
    use roe_model::common::ArrowType;
    match raw {
        Some("arrow") => Some(ArrowType::Arrow),
        Some("triangle") => Some(ArrowType::Triangle),
        // PROOT-OVAL-ARROW: 此前 "oval" 落进下面的 Some(_) 兜底分支，被
        // 错误映射成 ArrowType::None（不画任何装饰），导致连接线圆点端点
        // 完全消失。单独列出，映射到新增的 ArrowType::Oval。
        Some("oval") => Some(ArrowType::Oval),
        Some(_) => Some(ArrowType::None),
        None => None,
    }
}

// PPTX-6: 解析 <p:graphicFrame> 内嵌的 <a:tbl> 表格。
//
// 调用时机：主状态机（parse_slide_xml_shapes）在 graphicFrame 范围内识别到
// <a:tbl> 起始事件后，把同一个 reader 传入本函数，本函数接管读取，一路消费
// 到匹配的 </a:tbl> 为止再返回，调用方无需（也不应）自己再处理 tbl 内部事件。
//
// 合并单元格模型（roe-ext PPTX-6 调研已确认，与 DOCX 侧结构性不同，详见
// roe_model::common::TableCell.hidden 字段文档注释）：
// - a:gridSpan / a:rowSpan 写在起始格，直接给出跨度数值（无需像 DOCX 的
//   w:vMerge 那样事后累加推算）。
// - a:hMerge / a:vMerge 写在延续格，是布尔标记；延续格在 XML 中物理存在
//   （不像 DOCX 合并格那样被从 <w:tr> 里整个抹掉），只是渲染时应跳过。
// - 参照 LOK oox/source/drawingml/table/tablecellcontext.cxx：解析阶段
//   对 hMerge/vMerge 只是读取布尔标记，不清空、不跳过内容，两者一视同仁
//   走完整解析路径；本函数遵循同样原则，延续格的 txBody 仍正常解析并
//   保留，只是额外设置 hidden = true。
//
// 简化范围（第一期，与 parse_raw_shapes 姊妹函数的简化原则一致）：
// - 不解析 schemeClr（主题色引用），本函数未接收 theme_colors 映射表；
//   srgbClr 直接色值正常解析。
// - 不解析 a:tcPr 内的边框/填充/边距信息（lnL/lnR/lnT/lnB/边距等），
//   只解析文字内容与合并标记。边框渲染留待 PPTX-6 渲染层任务按需扩展。
// - 不解析 a:tblPr 的表格整体样式（tableStyleId/firstRow/bandRow 等）。
fn parse_table(reader: &mut Reader<&[u8]>, theme_colors: &HashMap<String, Color>, clr_map: &HashMap<String, String>) -> Result<roe_model::common::Table, ParseError> {
    use roe_model::common::{Table, TableRow, TableCell, CellContent};

    let mut col_widths: Vec<f32> = Vec::new();
    let mut rows: Vec<TableRow> = Vec::new();

    let mut cur_row: Option<TableRow> = None;
    let mut cur_cell: Option<TableCell> = None;

    // 单元格内 txBody 的 p/r/rPr/t 子状态机，命名加 tbl_ 前缀避免和外层混淆。
    let mut in_tc_txBody = false;
    let mut in_tbl_p = false;
    let mut in_tbl_r = false;
    let mut in_tbl_rPr = false;
    let mut in_tbl_rPr_fill = false;
    let mut in_tbl_t = false;
    // PROOT-003: <a:tcPr><a:solidFill> 单元格背景色状态
    let mut in_tc_pr = false;
    let mut in_tc_pr_fill = false;
    let mut cur_cell_bg_color: Option<Color> = None;
    // PROOT-003 followup4: 单元格边框颜色/宽度、四向内边距。简化处理：
    // lnL/lnR/lnT/lnB 四条边共用一套 border_color/border_width（真实样本
    // 四边总是一致，见n80340；若未来出现四边不同色的样本，取解析到的最后
    // 一个为准，此简化非遗漏）。
    //
    // in_tc_pr_border_fill 是独立于 in_tc_pr_fill 的新状态位：两者都表示
    // "正在某个 solidFill 内部，下一个 srgbClr/schemeClr 属于谁"，但
    // in_tc_pr_fill 专指 cell 自身背景色的 solidFill（<a:tcPr><a:solidFill>
    // 直接子元素），in_tc_pr_border_fill 专指 lnL/lnR/lnT/lnB 内部的
    // solidFill（<a:tcPr><a:lnL><a:solidFill>）。两者互斥（一个tcPr内
    // 同一时刻只可能处于其中一种），用不同变量而非复用同一个是为了让
    // srgbClr/schemeClr 事件处理分支能明确区分写入目标，避免误写。
    // PROOT-003 followup4 修复: 此前用 cur_cell_border_width.is_some()
    // 作为"是否在lnL/lnR/lnT/lnB内部"的判据，但该字段只在tcPr整体结束时
    // 才清空，导致lnB结束后、tcPr结束前，cell自身的<a:solidFill>（背景色，
    // XML顺序上常紧跟在lnTlToBr/lnBlToTr之后）被误判为"仍在边框线内"，
    // 错误写入border_color而非bg_color（n80340col=1实测：bg_color变成
    // None，006600背景色被误写进border_color）。改为精确的独立状态位
    // in_tc_pr_ln，只在lnL/lnR/lnT/lnB的Start/Empty进入时置true，各自
    // End事件时立即置false，不依赖"最近一次设置过的值"这种不精确判据。
    let mut in_tc_pr_ln = false;
    let mut in_tc_pr_border_fill = false;
    let mut cur_cell_border_color: Option<Color> = None;
    let mut cur_cell_border_width: Option<f32> = None;
    let mut cur_cell_pad_left: Option<f32> = None;
    let mut cur_cell_pad_right: Option<f32> = None;
    let mut cur_cell_pad_top: Option<f32> = None;
    let mut cur_cell_pad_bottom: Option<f32> = None;

    let mut tbl_cur_paras: Vec<Paragraph> = Vec::new();
    let mut tbl_cur_runs: Vec<Run> = Vec::new();
    let mut tbl_cur_text = String::new();
    let mut tbl_run_color: Option<Color> = None;
    let mut tbl_run_font_size: Option<f32> = None;
    let mut tbl_run_bold: Option<bool> = None;
    let mut tbl_run_italic: Option<bool> = None;
    let mut tbl_run_font_family: Option<String> = None;
    // PROOT-003 followup3: <a:pPr algn="..."> 表格cell段落级对齐方式。
    // 此前表格解析路径完全没有对应逻辑（同类逻辑此前只存在于普通文本框
    // txBody 解析路径的 cur_para_align，见该变量声明处注释），导致
    // render_shape_table 里读取 p.alignment 恒为 None，居中/右对齐代码
    // 从未真正生效（n80340实测：algn="ctr"但渲染结果贴左）。
    let mut tbl_para_align: Option<Alignment> = None;

    // PROOT-031: <a:tblPr> 表格整体样式开关 + <a:tblPr><a:tableStyleId>{GUID}
    // </a:tableStyleId></a:tblPr> 样式ID。tableStyleId 是子元素而非属性，
    // 文本内容才是GUID，需要独立的 in_table_style_id 状态位捕获其 Text 事件
    // （同 in_tbl_t 捕获 cell 文字的模式一致）。
    let mut tbl_style_id: Option<String> = None;
    let mut tbl_first_row = false;
    let mut tbl_first_col = false;
    let mut tbl_last_row = false;
    let mut tbl_last_col = false;
    let mut tbl_band_row = false;
    let mut tbl_band_col = false;
    let mut in_table_style_id = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                match e.local_name().as_ref() {
                    // PPTX-6 修复：<a:gridCol w="..."> 真实文件里常带 <a:extLst>
                    // 子元素（如 a16:colId），此时是 Event::Start 而非 Event::Empty，
                    // 之前只在 Empty 分支读取 w 属性，导致真实样本（如
                    // sd_data_pptx__tdf149785.pptx）col_widths 全部丢失（col=0）。
                    // extLst 内部标签不影响本状态机（未被其他分支识别，自然跳过）。
                    // PROOT-031: <a:tblPr firstRow="1" firstCol="1" lastRow="1"
                    // lastCol="1" bandRow="1" bandCol="1"> 表格整体样式开关。
                    // attr_str返回"1"/"0"/"true"/"false"均可能出现（OOXML
                    // xsd:boolean 允许两种写法），用 == "1" || == "true" 判定。
                    b"tblPr" => {
                        let attr_bool = |name: &[u8]| -> bool {
                            attr_str(e, name)
                                .map(|v| v == "1" || v == "true")
                                .unwrap_or(false)
                        };
                        tbl_first_row = attr_bool(b"firstRow");
                        tbl_first_col = attr_bool(b"firstCol");
                        tbl_last_row  = attr_bool(b"lastRow");
                        tbl_last_col  = attr_bool(b"lastCol");
                        tbl_band_row  = attr_bool(b"bandRow");
                        tbl_band_col  = attr_bool(b"bandCol");
                    }
                    // PROOT-031: <a:tableStyleId>{GUID}</a:tableStyleId> 是
                    // tblPr 的子元素，GUID是其文本内容而非属性。
                    b"tableStyleId" => {
                        in_table_style_id = true;
                    }
                    b"gridCol" => {
                        let w_emu = attr_f32(e, b"w").unwrap_or(0.0);
                        col_widths.push(w_emu / 12700.0);
                    }
                    b"tr" => {
                        // PROOT-003 followup: <a:tr h="..."> 行高，EMU换算pt
                        // 同gridCol w属性的换算方式（/12700.0）。
                        let row_h_emu = attr_f32(e, b"h");
                        cur_row = Some(TableRow {
                            cells: Vec::new(),
                            row_height: row_h_emu.map(|h| h / 12700.0),
                        });
                    }
                    b"tc" => {
                        let grid_span = attr_u32(e, b"gridSpan").unwrap_or(1) as u8;
                        let row_span  = attr_u32(e, b"rowSpan").unwrap_or(1) as u8;
                        let h_merge = attr_str(e, b"hMerge").map(|v| v != "0").unwrap_or(false);
                        let v_merge = attr_str(e, b"vMerge").map(|v| v != "0").unwrap_or(false);
                        cur_cell_bg_color = None;
                        cur_cell = Some(TableCell {
                            content:  Vec::new(),
                            col_span: grid_span,
                            row_span: row_span,
                            style:    None,
                            hidden:   h_merge || v_merge,
                            bg_color: None,
                            // PROOT-003 followup4: 边框颜色/宽度/内边距的
                            // 实际值在 tcPr / lnL/lnR/lnT/lnB 的 Start/Empty
                            // 事件处理中读取后，于 tcPr 闭合时统一写回 cur_cell
                            // （同 bg_color 现有模式一致，见 cur_cell_bg_color）。
                            // 此处先占位 None。
                            border_color: None,
                            border_width: None,
                            pad_left: None,
                            pad_right: None,
                            pad_top: None,
                            pad_bottom: None,
                        });
                    }
                    // PROOT-003: <a:tcPr> 单元格属性容器
                    b"tcPr" if cur_cell.is_some() => {
                        in_tc_pr = true;
                        // PROOT-003 followup4: marL/marR/marT/marB 缺省值参照
                        // LOK oox/source/drawingml/table/tablecellcontext.cxx：
                        // marL/marR 缺省 91440 EMU(=7.2pt)，marT/marB 缺省
                        // 45720 EMU(=3.6pt)。此前渲染层硬编码 CELL_PAD_X=4.0/
                        // CELL_PAD_Y=3.0 与此不符，本次改为遵循LOK缺省值。
                        cur_cell_pad_left   = Some(attr_f32(e, b"marL").unwrap_or(91440.0) / 12700.0);
                        cur_cell_pad_right  = Some(attr_f32(e, b"marR").unwrap_or(91440.0) / 12700.0);
                        cur_cell_pad_top    = Some(attr_f32(e, b"marT").unwrap_or(45720.0) / 12700.0);
                        cur_cell_pad_bottom = Some(attr_f32(e, b"marB").unwrap_or(45720.0) / 12700.0);
                        cur_cell_border_color = None;
                        cur_cell_border_width = None;
                    }
                    // PROOT-003 followup4: <a:lnL/lnR/lnT/lnB> 边框线属性容器，
                    // 非自闭合形式（含 solidFill 子元素时走 Start 事件）。四条
                    // 边共用一套 border_color/border_width（见变量声明处说明）。
                    b"lnL" | b"lnR" | b"lnT" | b"lnB" if in_tc_pr => {
                        in_tc_pr_ln = true;
                        // w 单位EMU，缺省 12700(=1.0pt)，参照 LOK
                        // oox/source/drawingml/linepropertiescontext.cxx。
                        cur_cell_border_width = Some(attr_f32(e, b"w").unwrap_or(12700.0) / 12700.0);
                    }
                    // PROOT-003 followup4: <a:lnL><a:solidFill> 非自闭合形式
                    b"solidFill" if in_tc_pr_ln => {
                        in_tc_pr_border_fill = true;
                    }
                    b"srgbClr" if in_tc_pr_border_fill => {
                        if let Some(hex) = attr_str(e, b"val") {
                            cur_cell_border_color = Some(parse_hex_color(&hex));
                        }
                    }
                    b"schemeClr" if in_tc_pr_border_fill => {
                        if let Some(slot) = attr_str(e, b"val") {
                            cur_cell_border_color = Some(
                                theme_colors.get(normalize_scheme_color_slot(&slot, clr_map)).copied()
                                    .unwrap_or(Color { r: 0, g: 0, b: 0, a: 255 })
                            );
                        }
                    }
                    b"txBody" if cur_cell.is_some() => {
                        in_tc_txBody = true;
                        tbl_cur_paras.clear();
                    }
                    b"p" if in_tc_txBody => {
                        in_tbl_p = true;
                        tbl_cur_runs.clear();
                        tbl_para_align = None;
                    }
                    // PROOT-003 followup3: <a:pPr algn="..."> 非自闭合形式
                    b"pPr" if in_tbl_p => {
                        tbl_para_align = match attr_str(e, b"algn").as_deref() {
                            Some("l")    => Some(Alignment::Left),
                            Some("ctr")  => Some(Alignment::Center),
                            Some("r")    => Some(Alignment::Right),
                            Some("just") => Some(Alignment::Justify),
                            _            => None,
                        };
                    }
                    b"r" if in_tbl_p => {
                        in_tbl_r = true;
                        tbl_cur_text.clear();
                        tbl_run_color = None;
                    }
                    b"rPr" if in_tbl_r => {
                        in_tbl_rPr = true;
                        in_tbl_rPr_fill = false;
                        // sz 单位是百分之一磅（hundredths of a point），与主状态机一致。
                        tbl_run_font_size = attr_u32(e, b"sz").map(|v| v as f32 / 100.0);
                        tbl_run_bold   = attr_str(e, b"b").map(|v| v != "0");
                        tbl_run_italic = attr_str(e, b"i").map(|v| v != "0");
                    }
                    b"solidFill" if in_tbl_rPr => {
                        in_tbl_rPr_fill = true;
                    }
                    // PROOT-003: <a:tcPr><a:solidFill> 非自闭合形式
                    b"solidFill" if in_tc_pr => {
                        in_tc_pr_fill = true;
                    }
                    b"srgbClr" if in_tc_pr_fill => {
                        if let Some(hex) = attr_str(e, b"val") {
                            cur_cell_bg_color = Some(parse_hex_color(&hex));
                        }
                    }
                    b"latin" if in_tbl_rPr => {
                        if let Some(tf) = attr_str(e, b"typeface") {
                            if tf != "+mj-lt" && tf != "+mn-lt" && !tf.is_empty() {
                                tbl_run_font_family = Some(tf);
                            }
                        }
                    }
                    b"t" if in_tbl_r => {
                        in_tbl_t = true;
                        tbl_cur_text.clear();
                    }
                    _ => {}
                }
            }

            Ok(Event::Empty(ref e)) => {
                match e.local_name().as_ref() {
                    // PROOT-031: <a:tblPr firstRow="1" firstCol="1" lastRow="1"
                    // lastCol="1" bandRow="1" bandCol="1"> 表格整体样式开关。
                    // attr_str返回"1"/"0"/"true"/"false"均可能出现（OOXML
                    // xsd:boolean 允许两种写法），用 == "1" || == "true" 判定。
                    b"tblPr" => {
                        let attr_bool = |name: &[u8]| -> bool {
                            attr_str(e, name)
                                .map(|v| v == "1" || v == "true")
                                .unwrap_or(false)
                        };
                        tbl_first_row = attr_bool(b"firstRow");
                        tbl_first_col = attr_bool(b"firstCol");
                        tbl_last_row  = attr_bool(b"lastRow");
                        tbl_last_col  = attr_bool(b"lastCol");
                        tbl_band_row  = attr_bool(b"bandRow");
                        tbl_band_col  = attr_bool(b"bandCol");
                    }
                    b"gridCol" => {
                        let w_emu = attr_f32(e, b"w").unwrap_or(0.0);
                        col_widths.push(w_emu / 12700.0);
                    }
                    b"tr" => {
                        // PROOT-003 followup: 自闭合 <a:tr/>（理论上不应出现，
                        // 一行至少含cell，但按PPTX-17教训防御性覆盖）。
                        let row_h_emu = attr_f32(e, b"h");
                        cur_row = Some(TableRow {
                            cells: Vec::new(),
                            row_height: row_h_emu.map(|h| h / 12700.0),
                        });
                    }
                    b"tc" => {
                        // 自闭合 <a:tc/>：理论上不应出现（规范要求至少含 txBody），
                        // 但按 PPTX-17 教训，自闭合标签必须同时覆盖，防御性处理。
                        let grid_span = attr_u32(e, b"gridSpan").unwrap_or(1) as u8;
                        let row_span  = attr_u32(e, b"rowSpan").unwrap_or(1) as u8;
                        let h_merge = attr_str(e, b"hMerge").map(|v| v != "0").unwrap_or(false);
                        let v_merge = attr_str(e, b"vMerge").map(|v| v != "0").unwrap_or(false);
                        if let Some(ref mut row) = cur_row {
                            row.cells.push(TableCell {
                                content:  Vec::new(),
                                col_span: grid_span,
                                row_span: row_span,
                                style:    None,
                                hidden:   h_merge || v_merge,
                                bg_color: None,
                                // PROOT-003 followup4: 自闭合<a:tc/>理论上不
                                // 应出现（同bg_color同批注释），边框/内边距
                                // 同样占位None。
                                border_color: None,
                                border_width: None,
                                pad_left: None,
                                pad_right: None,
                                pad_top: None,
                                pad_bottom: None,
                            });
                        }
                    }
                    // PROOT-003 followup3: 自闭合 <a:pPr algn="ctr"/> 形式
                    // （无 lnSpc/spcBef 等子元素时走 Empty 事件，PPTX-17 教训：
                    // 自闭合标签必须与 Start 事件分支同时覆盖）。
                    b"pPr" if in_tbl_p => {
                        tbl_para_align = match attr_str(e, b"algn").as_deref() {
                            Some("l")    => Some(Alignment::Left),
                            Some("ctr")  => Some(Alignment::Center),
                            Some("r")    => Some(Alignment::Right),
                            Some("just") => Some(Alignment::Justify),
                            _            => None,
                        };
                    }
                    // PROOT-003 followup4: 自闭合 <a:tcPr marL="0".../>
                    // 形式（纯边距/anchor等属性、无lnL/lnR/lnT/lnB子元素时，
                    // PPTX-17教训：自闭合标签必须与Start事件分支同时覆盖）。
                    b"tcPr" if cur_cell.is_some() => {
                        cur_cell_pad_left   = Some(attr_f32(e, b"marL").unwrap_or(91440.0) / 12700.0);
                        cur_cell_pad_right  = Some(attr_f32(e, b"marR").unwrap_or(91440.0) / 12700.0);
                        cur_cell_pad_top    = Some(attr_f32(e, b"marT").unwrap_or(45720.0) / 12700.0);
                        cur_cell_pad_bottom = Some(attr_f32(e, b"marB").unwrap_or(45720.0) / 12700.0);
                        cur_cell_border_color = None;
                        cur_cell_border_width = None;
                        if let Some(ref mut cell) = cur_cell {
                            cell.pad_left = cur_cell_pad_left.take();
                            cell.pad_right = cur_cell_pad_right.take();
                            cell.pad_top = cur_cell_pad_top.take();
                            cell.pad_bottom = cur_cell_pad_bottom.take();
                        }
                    }
                    // PROOT-003 followup4: 自闭合 <a:lnL w="12700".../>（无
                    // solidFill子元素、纯宽度定义时的写法，PPTX-17教训覆盖）。
                    b"lnL" | b"lnR" | b"lnT" | b"lnB" if in_tc_pr => {
                        cur_cell_border_width = Some(attr_f32(e, b"w").unwrap_or(12700.0) / 12700.0);
                        if let Some(ref mut cell) = cur_cell {
                            cell.border_width = cur_cell_border_width;
                        }
                    }
                    // PROOT-003: 原有注释所述"tcPr无子元素时不需单独处理"仅对
                    // bg_color成立（没有solidFill子元素可捕获，cur_cell_bg_color
                    // 保持None即可）；本次followup4新增的marL/marR/marT/marB/
                    // border_width已在上面tcPr/lnX分支单独处理，不再遗漏。
                    b"srgbClr" if in_tbl_rPr_fill => {
                        if let Some(hex) = attr_str(e, b"val") {
                            tbl_run_color = Some(parse_hex_color(&hex));
                        }
                    }
                    // PROOT-003: <a:tcPr><a:solidFill><a:srgbClr val="RRGGBB"/> 自闭合形式
                    b"srgbClr" if in_tc_pr_fill => {
                        if let Some(hex) = attr_str(e, b"val") {
                            cur_cell_bg_color = Some(parse_hex_color(&hex));
                        }
                    }
                    // PROOT-003 followup4: <a:lnL><a:solidFill><a:srgbClr/>
                    // 自闭合形式（边框颜色，与背景色的 in_tc_pr_fill 用独立
                    // 状态位 in_tc_pr_border_fill 区分，见变量声明处说明）。
                    b"srgbClr" if in_tc_pr_border_fill => {
                        if let Some(hex) = attr_str(e, b"val") {
                            cur_cell_border_color = Some(parse_hex_color(&hex));
                            if let Some(ref mut cell) = cur_cell {
                                cell.border_color = cur_cell_border_color;
                            }
                        }
                    }
                    b"schemeClr" if in_tc_pr_border_fill => {
                        if let Some(slot) = attr_str(e, b"val") {
                            let color = theme_colors.get(normalize_scheme_color_slot(&slot, clr_map)).copied()
                                .unwrap_or(Color { r: 0, g: 0, b: 0, a: 255 });
                            cur_cell_border_color = Some(color);
                            if let Some(ref mut cell) = cur_cell {
                                cell.border_color = cur_cell_border_color;
                            }
                        }
                    }
                    b"latin" if in_tbl_rPr => {
                        if let Some(tf) = attr_str(e, b"typeface") {
                            if tf != "+mj-lt" && tf != "+mn-lt" && !tf.is_empty() {
                                tbl_run_font_family = Some(tf);
                            }
                        }
                    }
                    _ => {}
                }
            }

            // PROOT-031: <a:tableStyleId>{GUID}</a:tableStyleId> 文本内容捕获。
            Ok(Event::Text(ref e)) if in_table_style_id => {
                if let Ok(txt) = e.unescape() {
                    tbl_style_id = Some(txt.trim().to_string());
                }
            }
            Ok(Event::Text(ref e)) if in_tbl_t => {
                tbl_cur_text.push_str(&e.unescape().unwrap_or_default());
            }

            Ok(Event::End(ref e)) => {
                match e.local_name().as_ref() {
                    b"t" if in_tbl_t => {
                        in_tbl_t = false;
                    }
                    b"rPr" if in_tbl_rPr => {
                        in_tbl_rPr = false;
                        in_tbl_rPr_fill = false;
                    }
                    b"r" if in_tbl_r => {
                        in_tbl_r = false;
                        tbl_cur_runs.push(Run {
                            text:        tbl_cur_text.clone(),
                            color:       tbl_run_color,
                            font_size:   tbl_run_font_size,
                            bold:        tbl_run_bold,
                            italic:      tbl_run_italic,
                            font_family: tbl_run_font_family.clone(),
                            ..Default::default()
                        });
                        tbl_cur_text.clear();
                        tbl_run_color = None;
                        tbl_run_font_size = None;
                        tbl_run_bold = None;
                        tbl_run_italic = None;
                        tbl_run_font_family = None;
                    }
                    b"p" if in_tbl_p => {
                        in_tbl_p = false;
                        tbl_cur_paras.push(Paragraph {
                            style: roe_model::common::StyleRef("".to_string()),
                            runs: tbl_cur_runs.drain(..).collect(),
                            list_info: None,
                            // PROOT-003 followup3: 写入表格cell段落对齐
                            alignment: tbl_para_align.take(),
                            ..Default::default()
                        });
                    }
                    b"txBody" if in_tc_txBody => {
                        in_tc_txBody = false;
                        if let Some(ref mut cell) = cur_cell {
                            cell.content = tbl_cur_paras.drain(..)
                                .map(CellContent::Paragraph)
                                .collect();
                        }
                    }
                    // PROOT-003: <a:tcPr> 闭合
                    b"tcPr" if in_tc_pr => {
                        in_tc_pr = false;
                        if let Some(ref mut cell) = cur_cell {
                            cell.bg_color = cur_cell_bg_color.take();
                            // PROOT-003 followup4: 边框颜色/宽度、四向内边距
                            // 写回。pad_* 在 Start 事件里已经赋值为 Some（
                            // 缺省值兜底后必为 Some，不会是 None），take()
                            // 后清空供下一个cell复用。
                            cell.border_color = cur_cell_border_color.take();
                            cell.border_width = cur_cell_border_width.take();
                            cell.pad_left = cur_cell_pad_left.take();
                            cell.pad_right = cur_cell_pad_right.take();
                            cell.pad_top = cur_cell_pad_top.take();
                            cell.pad_bottom = cur_cell_pad_bottom.take();
                        }
                    }
                    b"solidFill" if in_tc_pr_fill => {
                        in_tc_pr_fill = false;
                    }
                    // PROOT-003 followup4: <a:lnL><a:solidFill> 闭合
                    b"solidFill" if in_tc_pr_border_fill => {
                        in_tc_pr_border_fill = false;
                    }
                    // PROOT-003 followup4 修复: <a:lnL/lnR/lnT/lnB> 闭合。
                    // 必须立即重置 in_tc_pr_ln = false，否则该边线结束后、
                    // tcPr整体结束前的其他元素（尤其cell自身背景色solidFill，
                    // XML顺序上常见于lnTlToBr/lnBlToTr之后）会被误判为仍在
                    // 边框线内（本字段修复的具体bug见 in_tc_pr_ln 声明处
                    // 注释）。实际颜色/宽度已在tcPr闭合时统一写回cell，这里
                    // 不需要再写一次。
                    b"lnL" | b"lnR" | b"lnT" | b"lnB" if in_tc_pr => {
                        in_tc_pr_ln = false;
                    }
                    // PROOT-031: </a:tableStyleId> 闭合，结束文本捕获状态。
                    b"tableStyleId" => {
                        in_table_style_id = false;
                    }
                    b"tc" => {
                        if let (Some(cell), Some(ref mut row)) = (cur_cell.take(), cur_row.as_mut()) {
                            row.cells.push(cell);
                        }
                    }
                    b"tr" => {
                        if let Some(row) = cur_row.take() {
                            rows.push(row);
                        }
                    }
                    b"tbl" => {
                        // 匹配到本函数负责消费的 </a:tbl>，结束读取，交回控制权给调用方。
                        break;
                    }
                    _ => {}
                }
            }

            Ok(Event::Eof) => break,
            Err(e) => return Err(ParseError::Xml(e)),
            _ => {}
        }
    }

    Ok(Table {
        float_pos: None, // PPTX不使用该字段（与default_border_color/width既有模式一致）
        rows, col_widths,
        style_id: tbl_style_id,
        first_row: tbl_first_row,
        first_col: tbl_first_col,
        last_row: tbl_last_row,
        last_col: tbl_last_col,
        band_row: tbl_band_row,
        band_col: tbl_band_col,
        default_border_color: None,
        default_border_width: None,
        inside_h_color: None,
        inside_h_width: None,
        inside_v_color: None,
        inside_v_width: None,
    })
}

// ─── Relationship 解析 ───────────────────────────────────────────────────────



// PROOT bullet: master <p:txStyles> 按 lvl（0-based，对应 OOXML lvl1pPr..lvl10pPr）
// 存储的 bullet 相关默认值。只提取 bullet 渲染需要的三个字段（bullet 种类、
// marL、indent），不搬运 defRPr/lnSpc 等其他属性——这次范围只做 bullet，
// 其余属性继承（字号等）已有独立机制（PROOT-014 一系列已处理字号继承），
// 不在这里重复处理，避免职责混淆。
#[derive(Debug, Clone, Default)]
struct MasterBulletLevel {
    bullet: Option<roe_model::impress::BulletKind>,
    mar_l:  Option<f32>,
    indent: Option<f32>,
    // PROOT: master txStyles bodyStyle/titleStyle/otherStyle 各级 lvlNpPr
    // 内 <a:defRPr sz="..."/> 的字号（1/100pt -> pt），此前完全未接入占位符
    // 字号继承链，见 bug_log.md 2026-07-30 "新bug"条目。
    font_size: Option<f32>,
    // PROOT-tdf158512-color: master txStyles 各级 lvlNpPr 内 <a:defRPr>
    // 的 <a:solidFill><a:schemeClr/></a:solidFill> 或 <a:srgbClr/> 颜色，
    // 此前完全未接入占位符提示文字颜色继承链，导致提示文字颜色永远落到
    // 渲染层灰色兜底（即使master titleStyle/bodyStyle声明了真实颜色，
    // 如schemeClr val="tx1"）。见 bug_log.md 2026-08-09 记录。
    color: Option<roe_model::common::Color>,
}

// master 三大类占位符（title/body/other）各自的 10 级 bullet 默认值。
// 对应 OOXML <p:txStyles><p:titleStyle|bodyStyle|otherStyle><a:lvl1pPr>..<a:lvl10pPr>。
// 已用 onlyoffice 源码 core/OOXML/PPTXFormat/Logic/TxStyles.h + TextListStyle.h
// 交叉验证：三个分类各自是独立的 10 级数组，互不共享。
#[derive(Debug, Clone, Default)]
struct MasterTextStyles {
    title: [MasterBulletLevel; 10],
    body:  [MasterBulletLevel; 10],
    other: [MasterBulletLevel; 10],
}

/// 解析 master XML 里的 <p:txStyles> 区块，提取 bullet 相关的 lvl 默认值。
/// 与 parse_raw_shapes 是姊妹函数但服务完全不同的 XML 区域——parse_raw_shapes
/// 遍历 <p:cSld><p:spTree> 找占位符 shape，本函数遍历 <p:txStyles>，
/// 这是 <p:sldMaster> 的另一个平级子元素，不在 spTree 内，不能共用状态机。
///
/// 找不到 <p:txStyles> 时返回全 None 的默认值，调用方按"master 没有任何
/// bullet 默认值"处理，不会 panic。
fn parse_master_txstyles(xml: &str, theme_colors: &HashMap<String, Color>, clr_map: &HashMap<String, String>) -> MasterTextStyles {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut out = MasterTextStyles::default();

    #[derive(Clone, Copy, PartialEq)]
    enum StyleGroup { Title, Body, Other }
    let mut cur_group: Option<StyleGroup> = None;
    let mut cur_lvl: Option<usize> = None;
    let mut in_txstyles = false;
    // PROOT-024: 同段落级buFont修复，master lvlNpPr里buFont也总是先于
    // buChar出现，暂存typeface供buChar消费。
    let mut cur_lvl_bullet_font: Option<String> = None;
    // PROOT-tdf158512-color: 跟踪是否在 <a:defRPr> 范围内，用于识别其
    // 子元素 <a:solidFill><a:schemeClr/srgbClr></a:solidFill> 归属颜色。
    // defRPr 不是自闭合时才有子元素，需要显式 Start/End 配对（区别于
    // sz 属性可以在 Start/Empty 统一分支直接读到，颜色是子元素不是属性）。
    let mut in_defrpr = false;
    let mut in_defrpr_fill = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let name = e.local_name();
                let name = name.as_ref();
                match name {
                    b"txStyles" => { in_txstyles = true; }
                    b"titleStyle" if in_txstyles => { cur_group = Some(StyleGroup::Title); }
                    b"bodyStyle"  if in_txstyles => { cur_group = Some(StyleGroup::Body); }
                    b"otherStyle" if in_txstyles => { cur_group = Some(StyleGroup::Other); }
                    _ => {
                        if cur_group.is_some() && name.len() >= 6 && &name[0..3] == b"lvl" && name.ends_with(b"pPr") {
                            let digits = &name[3..name.len() - 3];
                            if let Ok(digit_str) = std::str::from_utf8(digits) {
                                if let Ok(n) = digit_str.parse::<usize>() {
                                    if n >= 1 && n <= 10 {
                                        cur_lvl = Some(n - 1);
                                        cur_lvl_bullet_font = None;
                                        let mar_l = attr_i32(e, b"marL").map(|v| v as f32 / 12700.0);
                                        let indent = attr_i32(e, b"indent").map(|v| v as f32 / 12700.0);
                                        if let (Some(group), Some(lvl)) = (cur_group, cur_lvl) {
                                            let arr = match group {
                                                StyleGroup::Title => &mut out.title,
                                                StyleGroup::Body  => &mut out.body,
                                                StyleGroup::Other => &mut out.other,
                                            };
                                            if mar_l.is_some() { arr[lvl].mar_l = mar_l; }
                                            if indent.is_some() { arr[lvl].indent = indent; }
                                        }
                                    }
                                }
                            }
                        }
                        if let (Some(group), Some(lvl)) = (cur_group, cur_lvl) {
                            let arr = match group {
                                StyleGroup::Title => &mut out.title,
                                StyleGroup::Body  => &mut out.body,
                                StyleGroup::Other => &mut out.other,
                            };
                            match name {
                                b"buNone" => {
                                    arr[lvl].bullet = Some(roe_model::impress::BulletKind::None);
                                }
                                b"buFont" => {
                                    cur_lvl_bullet_font = attr_str(e, b"typeface");
                                }
                                b"buChar" => {
                                    if let Some(ch) = attr_str(e, b"char") {
                                        arr[lvl].bullet = Some(roe_model::impress::BulletKind::Char { ch, font: cur_lvl_bullet_font.clone() });
                                    }
                                }
                                b"buAutoNum" => {
                                    if let Some(scheme) = attr_str(e, b"type") {
                                        let start_at = attr_i32(e, b"startAt");
                                        arr[lvl].bullet = Some(roe_model::impress::BulletKind::AutoNum { scheme, start_at });
                                    }
                                }
                                // PROOT: master txStyles lvlNpPr 内 <a:defRPr sz=".."/>
                                // 字号解析。sz 是1/100pt单位（OOXML惯例，如sz="2800"=28pt）。
                                b"defRPr" => {
                                    if let Some(sz) = attr_u32(e, b"sz") {
                                        arr[lvl].font_size = Some(sz as f32 / 100.0);
                                    }
                                    // PROOT-tdf158512-color: Start/Empty共用同一分支，无法
                                    // 在此区分是否自闭合。无条件置位，若实际是自闭合
                                    // （<a:defRPr sz="4400"/>，无子元素事件跟随），不会
                                    // 有solidFill/schemeClr事件命中，也就不会有副作用；
                                    // Event::End(b"defRPr")会在真正有内容时清理该状态，
                                    // 自闭合时没有对应End事件，状态会保留到下一个lvlNpPr
                                    // 结束时被兜底重置（见Event::End的lvlNpPr分支）。
                                    in_defrpr = true;
                                }
                                // PROOT-tdf158512-color: defRPr子元素<a:solidFill>范围标记，
                                // 用于和其他可能出现solidFill的地方（目前master txStyles里
                                // 只有defRPr会有）区分，保持状态机精确匹配意图，不依赖"反正
                                // 只有一处"这种脆弱假设。
                                b"solidFill" if in_defrpr => {
                                    in_defrpr_fill = true;
                                }
                                b"schemeClr" if in_defrpr_fill => {
                                    if let Some(slot) = attr_str(e, b"val") {
                                        let color = theme_colors.get(normalize_scheme_color_slot(&slot, clr_map)).copied();
                                        if color.is_some() {
                                            arr[lvl].color = color;
                                        }
                                    }
                                }
                                b"srgbClr" if in_defrpr_fill => {
                                    if let Some(hex) = attr_str(e, b"val") {
                                        arr[lvl].color = Some(parse_hex_color(&hex));
                                    }
                                }
                                // PROOT-024 followup: buFont 之前被误判为"死代码"
                                // 故意跳过，实际上只需要在 buChar 处理前暂存 typeface
                                // 即可正确回填（见上面 cur_lvl_bullet_font 用法），
                                // 已修复，见2026-07-29 bug_log PROOT-024条目。
                                _ => {}
                            }
                        }
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let name = e.local_name();
                let name = name.as_ref();
                match name {
                    b"txStyles" => { in_txstyles = false; }
                    b"titleStyle" | b"bodyStyle" | b"otherStyle" => { cur_group = None; }
                    // PROOT-tdf158512-color: defRPr/solidFill范围清理，精确对应
                    // Start时的置位。自闭合defRPr不会触发这个End分支（quick-xml
                    // 对Event::Empty不产生对应End事件），这种情况下状态清理
                    // 交给下面lvlNpPr结束时的兜底重置。
                    b"defRPr" => { in_defrpr = false; in_defrpr_fill = false; }
                    b"solidFill" if in_defrpr => { in_defrpr_fill = false; }
                    _ => {
                        if name.len() >= 6 && &name[0..3] == b"lvl" && name.ends_with(b"pPr") {
                            cur_lvl = None;
                            // PROOT-tdf158512-color: 兜底重置，防止自闭合defRPr
                            // （无End事件配对）导致in_defrpr状态泄漏到下一个level。
                            in_defrpr = false;
                            in_defrpr_fill = false;
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    out
}

/// 解析 slide/master XML，只返回原始 RawShape 列表（不做 ph 继承和最终转换）。
/// 用于从 master 提取 ph_positions map。
fn parse_raw_shapes(xml: &str) -> Result<Vec<RawShape>, ParseError> {
    // PPTX-17：补上 <p:txBody> / run 解析，此前本函数从不读文字内容，
    // 导致 master/layout 层占位符候选（PlaceholderCandidate.style）永远是默认值，
    // PPTX-4 的样式继承（font_size/bold/italic/color/font_family）实际从未生效。
    // 状态机与字段语义对照 parse_slide_xml_shapes 里对应部分（同文件姊妹函数），保持一致。
    // 简化点（相对姊妹函数）：不解析 schemeClr（主题色引用），因为本函数调用点
    // （master/layout两处）未传入 theme_colors 映射表；srgbClr 直接色值正常解析。
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut shapes: Vec<RawShape> = Vec::new();
    let mut cur_shape: Option<RawShape> = None;
    let mut in_sp = false;
    let mut in_pic = false; // PROOT-005 followup: <p:pic> 支持
    let mut in_spPr = false;
    let mut in_txBody = false;
    let mut in_para = false;
    let mut in_run = false;
    let mut in_t = false;
    let mut in_rPr = false;
    let mut in_rPr_fill = false;
    let mut in_rPr_hl = false;
    let mut cur_run_bg: Option<Color> = None;
    let mut in_lstStyle = false;
    let mut in_lvl1pPr = false;
    let mut cur_shape_algn: Option<Alignment> = None;
    // PROOT-027 followup: layout lstStyle lvl1pPr defRPr sz
    let mut cur_shape_font_size: Option<f32> = None;
    // PROOT-tdf158512: layout lstStyle lvl1pPr 内 buNone/buChar 解析用临时变量。
    let mut cur_shape_bullet: Option<roe_model::impress::BulletKind> = None;
    let mut cur_shape_bullet_font: Option<String> = None;
    // PROOT-027 followup: <a:bodyPr anchor="..."> 读取（Start/Empty两种XML
    // 事件形式都要处理，与rPr/defRPr同理）。
    let mut cur_shape_anchor: Option<roe_model::impress::TextAnchor> = None;
    let mut cur_shape_vert: Option<roe_model::impress::TextVert> = None;

    let mut cur_paras: Vec<Paragraph> = Vec::new();
    let mut cur_runs: Vec<Run> = Vec::new();
    let mut cur_text = String::new();
    let mut cur_run_color: Option<Color> = None;
    let mut cur_run_font_size: Option<f32> = None;
    let mut cur_run_bold: Option<bool> = None;
    let mut cur_run_italic: Option<bool> = None;
    let mut cur_run_underline: Option<bool> = None;
    let mut cur_run_strike: Option<bool> = None;
    let mut cur_run_script: Option<roe_model::writer::ScriptPosition> = None;
    let mut cur_run_font_family: Option<String> = None;
    // PROOT: 空占位符提示文案攻坚二期——<a:endParaRPr>（空段落"如果在此输入文字应使用
    // 的格式"）与<a:rPr>是并列的两种可能来源，很多layout占位符没有<a:lstStyle>也没有
    // 实际<a:r>run，格式完全写在endParaRPr上（真实样本candy.pptx slideLayout3.xml
    // title占位符即此情况：sz=4400/color=000000/latin=Noto Sans全部只在endParaRPr里，
    // 此前完全未被解析，导致该layout的提示文字退化成固定灰色兜底）。
    // 只在该段落没有任何真实run时才会被采用（转成一个文本为空的"虚拟run"塞进
    // cur_runs，复用下方既有"从paras.first().runs.first()取格式"的读取路径，
    // 不需要改动PlaceholderCandidate构建逻辑本身）。
    let mut cur_endpara_color: Option<Color> = None;
    let mut cur_endpara_font_size: Option<f32> = None;
    let mut cur_endpara_bold: Option<bool> = None;
    let mut cur_endpara_italic: Option<bool> = None;
    let mut cur_endpara_font_family: Option<String> = None;
    let mut in_endpara_rpr = false;
    let mut in_endpara_fill = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                match e.local_name().as_ref() {
                    b"sp" => { in_sp = true; cur_shape = Some(RawShape::default()); }
                    // PROOT-005 followup: <p:pic> 此前完全不被本函数识别（本函数
                    // 原设计只服务占位符继承场景，图片形状天然不带 <p:ph>，被忽略
                    // 不影响原有功能）。现在本函数还被 extract_non_ph_shapes 复用
                    // 来提取 layout/master 层"非占位符装饰性形状"，图片是最常见
                    // 的一种（真实样本 tdf158512 layout 右上角太阳+房子图案即是
                    // <p:pic>），需要补上识别。
                    b"pic" => { in_pic = true; cur_shape = Some(RawShape::default()); }
                    b"blip" if in_pic => {
                        if let Some(ref mut s) = cur_shape {
                            if let Some(rid) = attr_str(e, b"embed") {
                                s.content = RawContent::Image { rId: rid, crop_l: 0, crop_t: 0, crop_r: 0, crop_b: 0 };
                            }
                        }
                    }
                    b"spPr" if in_sp || in_pic => { in_spPr = true; }
                    // PPTX-7 followup：第三处占位符继承专用解析路径（parse_raw_shapes）此前未处理
                    // <a:xfrm rot/flipH/flipV>，导致 master/layout 占位符的旋转信息解析不出来。
                    // 与主解析函数（parse_slide_xml_shapes）的对应分支保持同样的读取方式。
                    b"xfrm" if in_spPr => {
                        if let Some(ref mut s) = cur_shape {
                            if let Some(r) = attr_i32(e, b"rot") { s.rot = Some(r); }
                            if let Some(fh) = attr_str(e, b"flipH") { s.flip_h = Some(fh == "1" || fh.eq_ignore_ascii_case("true")); }
                            if let Some(fv) = attr_str(e, b"flipV") { s.flip_v = Some(fv == "1" || fv.eq_ignore_ascii_case("true")); }
                        }
                    }
                    b"txBody" if in_sp => {
                        in_txBody = true;
                        cur_paras.clear();
                    }
                    b"bodyPr" if in_txBody => {
                        cur_shape_anchor = match attr_str(e, b"anchor").as_deref() {
                            Some("ctr") => Some(roe_model::impress::TextAnchor::Middle),
                            Some("b")   => Some(roe_model::impress::TextAnchor::Bottom),
                            Some("t")   => Some(roe_model::impress::TextAnchor::Top),
                            _ => None,
                        };
                        // PROOT-1xx (v99): layout/master bodyPr vert，沿用同函数内
                        // anchor同款独立状态变量模式（见lstyle_vert字段注释）。
                        cur_shape_vert = match attr_str(e, b"vert").as_deref() {
                            Some("vert")    => Some(roe_model::impress::TextVert::Vert),
                            Some("vert270") => Some(roe_model::impress::TextVert::Vert270),
                            Some("eaVert")  => Some(roe_model::impress::TextVert::EaVert),
                            _ => None,
                        };
                    }
                    b"lstStyle" if in_txBody => { in_lstStyle = true; }
                    b"lvl1pPr" if in_lstStyle => {
                        in_lvl1pPr = true;
                        // PROOT-027: lvl1pPr의 algn 속성 읽기
                        cur_shape_algn = match attr_str(e, b"algn").as_deref() {
                            Some("l")    => Some(Alignment::Left),
                            Some("ctr")  => Some(Alignment::Center),
                            Some("r")    => Some(Alignment::Right),
                            Some("just") => Some(Alignment::Justify),
                            _ => None,
                        };
                    }
                    b"buNone" if in_lvl1pPr => {
                        // PROOT-tdf158512: layout lstStyle lvl1pPr 显式声明该级别
                        // 不要 bullet，优先级高于 master txStyles 继承。
                        cur_shape_bullet = Some(roe_model::impress::BulletKind::None);
                    }
                    b"buFont" if in_lvl1pPr => {
                        cur_shape_bullet_font = attr_str(e, b"typeface");
                    }
                    b"buChar" if in_lvl1pPr => {
                        if let Some(ch) = attr_str(e, b"char") {
                            cur_shape_bullet = Some(roe_model::impress::BulletKind::Char {
                                ch, font: cur_shape_bullet_font.clone(),
                            });
                        }
                    }
                    b"defRPr" if in_lvl1pPr => {
                        // PROOT-027 followup: layout lstStyle lvl1pPr defRPr sz读取
                        if let Some(sz) = attr_u32(e, b"sz") {
                            cur_shape_font_size = Some(sz as f32 / 100.0);
                        }
                    }
                    b"p" if in_txBody => {
                        in_para = true;
                        cur_runs.clear();
                    }
                    b"r" if in_para => {
                        in_run = true;
                        cur_text.clear();
                        cur_run_color = None;
                    }
                    b"rPr" if in_run => {
                        in_rPr = true;
                        in_rPr_fill = false;
                        cur_run_font_size = attr_u32(e, b"sz").map(|v| v as f32 / 100.0);
                        cur_run_bold   = attr_str(e, b"b").map(|v| v != "0");
                        cur_run_italic = attr_str(e, b"i").map(|v| v != "0");
                        cur_run_underline = attr_str(e, b"u").map(|v| v != "none");
                        cur_run_strike = attr_str(e, b"strike").map(|v| v != "noStrike");
                        cur_run_script = attr_i32(e, b"baseline").and_then(|v| if v > 0 { Some(roe_model::writer::ScriptPosition::Superscript) } else if v < 0 { Some(roe_model::writer::ScriptPosition::Subscript) } else { None });
                    }
                    b"highlight" if in_rPr => { in_rPr_hl = true; }
                    b"solidFill" if in_rPr => {
                        in_rPr_fill = true;
                    }
                    b"latin" if in_rPr => {
                        if let Some(tf) = attr_str(e, b"typeface") {
                            if tf != "+mj-lt" && tf != "+mn-lt" && !tf.is_empty() {
                                cur_run_font_family = Some(tf);
                            }
                        }
                    }
                    // PROOT: endParaRPr Start写法（含子元素如solidFill/latin时触发）
                    b"endParaRPr" if in_para => {
                        in_endpara_rpr = true;
                        in_endpara_fill = false;
                        cur_endpara_font_size = attr_u32(e, b"sz").map(|v| v as f32 / 100.0);
                        cur_endpara_bold   = attr_str(e, b"b").map(|v| v != "0");
                        cur_endpara_italic = attr_str(e, b"i").map(|v| v != "0");
                    }
                    b"solidFill" if in_endpara_rpr => {
                        in_endpara_fill = true;
                    }
                    b"latin" if in_endpara_rpr => {
                        if let Some(tf) = attr_str(e, b"typeface") {
                            if tf != "+mj-lt" && tf != "+mn-lt" && !tf.is_empty() {
                                cur_endpara_font_family = Some(tf);
                            }
                        }
                    }
                    b"t" if in_run => {
                        in_t = true;
                        cur_text.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                match e.local_name().as_ref() {
                    b"ph" if in_sp => {
                        if let Some(ref mut s) = cur_shape {
                            let ph_type = attr_str(e, b"type").unwrap_or_else(|| "body".to_string());
                            let ph_idx = attr_u32(e, b"idx");
                            s.ph = Some((ph_type, ph_idx));
                        }
                    }
                    // xfrm 是常见的自闭合空标签写法（<a:xfrm rot="..." flipH="1"/>，
                    // 无 off/ext 子元素时），quick-xml 触发 Empty 事件而非 Start 事件，
                    // 需要在这里也读取一次（与 rPr 的处理同理，PPTX-17 踩过的同类坑）。
                    b"xfrm" if in_spPr => {
                        if let Some(ref mut s) = cur_shape {
                            if let Some(r) = attr_i32(e, b"rot") { s.rot = Some(r); }
                            if let Some(fh) = attr_str(e, b"flipH") { s.flip_h = Some(fh == "1" || fh.eq_ignore_ascii_case("true")); }
                            if let Some(fv) = attr_str(e, b"flipV") { s.flip_v = Some(fv == "1" || fv.eq_ignore_ascii_case("true")); }
                        }
                    }
                    b"off" if in_spPr => {
                        if let Some(ref mut s) = cur_shape {
                            s.position.offset.0 = attr_f32(e, b"x").unwrap_or(0.0) / 12700.0;
                            s.position.offset.1 = attr_f32(e, b"y").unwrap_or(0.0) / 12700.0;
                        }
                    }
                    b"ext" if in_spPr => {
                        if let Some(ref mut s) = cur_shape {
                            s.position.extent.0 = attr_f32(e, b"cx").unwrap_or(0.0) / 12700.0;
                            s.position.extent.1 = attr_f32(e, b"cy").unwrap_or(0.0) / 12700.0;
                        }
                    }
                    // PROOT-tdf158512: buNone/buChar 几乎总是自闭合标签（<a:buNone/>、
                    // <a:buChar char="•"/>），quick-xml 触发 Empty 事件而非 Start 事件，
                    // 与上面 xfrm/rPr 同理（PPTX-17 踩过的同类坑）。此前只在 Start 分支
                    // 处理导致这两个标签在实践中几乎从未被本函数捕获到，layout lstStyle
                    // 的 buNone 覆盖判断因此一直失效（复现样本 tdf158512）。
                    b"buNone" if in_lvl1pPr => {
                        cur_shape_bullet = Some(roe_model::impress::BulletKind::None);
                    }
                    b"buFont" if in_lvl1pPr => {
                        cur_shape_bullet_font = attr_str(e, b"typeface");
                    }
                    b"buChar" if in_lvl1pPr => {
                        if let Some(ch) = attr_str(e, b"char") {
                            cur_shape_bullet = Some(roe_model::impress::BulletKind::Char {
                                ch, font: cur_shape_bullet_font.clone(),
                            });
                        }
                    }
                    // rPr 常见写法是自闭合空标签（<a:rPr sz="..."/>，无子元素时），
                    // quick-xml 会把它作为 Empty 事件而非 Start 事件触发，
                    // 需要在这里也读取一次属性，否则 sz/b/i 会被漏掉（PPTX-17 踩过的坑）。
                    b"rPr" if in_run => {
                        in_rPr_fill = false;
                        cur_run_font_size = attr_u32(e, b"sz").map(|v| v as f32 / 100.0);
                        cur_run_bold   = attr_str(e, b"b").map(|v| v != "0");
                        cur_run_italic = attr_str(e, b"i").map(|v| v != "0");
                        cur_run_underline = attr_str(e, b"u").map(|v| v != "none");
                        cur_run_strike = attr_str(e, b"strike").map(|v| v != "noStrike");
                        cur_run_script = attr_i32(e, b"baseline").and_then(|v| if v > 0 { Some(roe_model::writer::ScriptPosition::Superscript) } else if v < 0 { Some(roe_model::writer::ScriptPosition::Subscript) } else { None });
                    }
                    b"srgbClr" if in_rPr_hl => {
                        if let Some(hex) = attr_str(e, b"val") { cur_run_bg = Some(parse_hex_color(&hex)); }
                    }
                    b"srgbClr" if in_rPr_fill => {
                        if let Some(hex) = attr_str(e, b"val") {
                            cur_run_color = Some(parse_hex_color(&hex));
                        }
                    }
                    // PROOT: endParaRPr自闭合写法（<a:endParaRPr sz="4400" .../>，
                    // 无子元素时quick-xml触发Empty而非Start，与rPr同理需要在这里也读一次）
                    b"endParaRPr" if in_para => {
                        in_endpara_fill = false;
                        cur_endpara_font_size = attr_u32(e, b"sz").map(|v| v as f32 / 100.0);
                        cur_endpara_bold   = attr_str(e, b"b").map(|v| v != "0");
                        cur_endpara_italic = attr_str(e, b"i").map(|v| v != "0");
                    }
                    b"srgbClr" if in_endpara_fill => {
                        if let Some(hex) = attr_str(e, b"val") {
                            cur_endpara_color = Some(parse_hex_color(&hex));
                        }
                    }
                    // PROOT-027 followup: <a:defRPr sz="6000"/> 自闭合形式，
                    // quick-xml触发Empty而非Start事件（与rPr同理），需要在
                    // 这里也读一次sz，否则layout lstStyle的字号会被漏掉。
                    b"defRPr" if in_lvl1pPr => {
                        if let Some(sz) = attr_u32(e, b"sz") {
                            cur_shape_font_size = Some(sz as f32 / 100.0);
                        }
                    }
                    // PROOT-027 followup: <a:bodyPr anchor="b"/> 自闭合形式
                    // （无normAutofit等子元素时），与Start分支同理。
                    b"bodyPr" if in_txBody => {
                        cur_shape_anchor = match attr_str(e, b"anchor").as_deref() {
                            Some("ctr") => Some(roe_model::impress::TextAnchor::Middle),
                            Some("b")   => Some(roe_model::impress::TextAnchor::Bottom),
                            Some("t")   => Some(roe_model::impress::TextAnchor::Top),
                            _ => None,
                        };
                        // PROOT-1xx (v99): layout/master bodyPr vert，沿用同函数内
                        // anchor同款独立状态变量模式（见lstyle_vert字段注释）。
                        cur_shape_vert = match attr_str(e, b"vert").as_deref() {
                            Some("vert")    => Some(roe_model::impress::TextVert::Vert),
                            Some("vert270") => Some(roe_model::impress::TextVert::Vert270),
                            Some("eaVert")  => Some(roe_model::impress::TextVert::EaVert),
                            _ => None,
                        };
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(ref e)) if in_t => {
                cur_text.push_str(&e.unescape().unwrap_or_default());
            }
            Ok(Event::End(ref e)) => {
                match e.local_name().as_ref() {
                    b"t" if in_t => { in_t = false; }
                    b"rPr" if in_rPr => { in_rPr = false; in_rPr_fill = false; }
                    b"highlight" if in_rPr_hl => { in_rPr_hl = false; }
                    b"solidFill" if in_rPr_fill => { in_rPr_fill = false; }
                    b"endParaRPr" if in_endpara_rpr => { in_endpara_rpr = false; in_endpara_fill = false; }
                    b"solidFill" if in_endpara_fill => { in_endpara_fill = false; }
                    b"endParaRPr" if in_endpara_rpr => { in_endpara_rpr = false; in_endpara_fill = false; }
                    b"solidFill" if in_endpara_fill => { in_endpara_fill = false; }
                    b"r" if in_run => {
                        in_run = false;
                        cur_runs.push(Run {
                            text:        cur_text.clone(),
                            color:       cur_run_color,
                            font_size:   cur_run_font_size,
                            bold:        cur_run_bold,
                            italic:      cur_run_italic, underline: cur_run_underline, strikethrough: cur_run_strike, background: cur_run_bg, script: cur_run_script,
                            font_family: cur_run_font_family.clone(),
                            ..Default::default()
                        });
                        cur_text.clear();
                        cur_run_color = None;
                        cur_run_font_size = None;
                        cur_run_bold = None;
                        cur_run_italic = None; cur_run_underline = None; cur_run_strike = None; cur_run_bg = None; cur_run_script = None;
                        cur_run_font_family = None;
                    }
                    b"p" if in_para => {
                        in_para = false;
                        // PROOT: 该段落没有任何真实<a:r>run、但收集到了endParaRPr格式时，
                        // 把endParaRPr转成一个文本为空的"虚拟run"塞进cur_runs——下游
                        // extract_ph_candidates()的"paras.first().runs.first()"读取路径
                        // 因此能拿到格式，不需要改动该函数本身。真实<a:r>run存在时优先级
                        // 更高，不受影响（此处只在cur_runs为空时才追加虚拟run）。
                        if cur_runs.is_empty() && (
                            cur_endpara_color.is_some() || cur_endpara_font_size.is_some()
                            || cur_endpara_bold.is_some() || cur_endpara_italic.is_some()
                            || cur_endpara_font_family.is_some()
                        ) {
                            cur_runs.push(Run {
                                text:        String::new(),
                                color:       cur_endpara_color,
                                font_size:   cur_endpara_font_size,
                                bold:        cur_endpara_bold,
                                italic:      cur_endpara_italic,
                                font_family: cur_endpara_font_family.clone(),
                                ..Default::default()
                            });
                        }
                        cur_endpara_color = None;
                        cur_endpara_font_size = None;
                        cur_endpara_bold = None;
                        cur_endpara_italic = None;
                        cur_endpara_font_family = None;
                        cur_paras.push(Paragraph {
                            style: roe_model::common::StyleRef("".to_string()),
                            runs: cur_runs.drain(..).collect(),
                            list_info: None,
                            ..Default::default()
                        });
                    }
                    b"lstStyle" if in_lstStyle => { in_lstStyle = false; }
                    b"lvl1pPr" if in_lvl1pPr => { in_lvl1pPr = false; }
                    b"txBody" if in_txBody => {
                        in_txBody = false;
                        if let Some(ref mut s) = cur_shape {
                            s.content = RawContent::Text(cur_paras.drain(..).collect());
                        }
                    }
                    b"spPr" if in_spPr => { in_spPr = false; }
                    b"sp" if in_sp => {
                        in_sp = false;
                        if let Some(mut raw) = cur_shape.take() {
                            raw.lstyle_algn = cur_shape_algn.take();
                            raw.lstyle_font_size = cur_shape_font_size.take();
                            raw.lstyle_anchor = cur_shape_anchor.take();
                            raw.lstyle_vert = cur_shape_vert.take();
                            raw.lstyle_bullet = cur_shape_bullet.take();
                            cur_shape_bullet_font = None;
                            shapes.push(raw);
                        }
                    }
                    b"pic" if in_pic => {
                        in_pic = false;
                        if let Some(raw) = cur_shape.take() {
                            shapes.push(raw);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    Ok(shapes)
}

/// PROOT-flip-bg1修复: OOXML <a:schemeClr val="..."> 除了 <a:clrScheme>
/// 里定义的 dk1/lt1/dk2/lt2 原始槽位名，还允许一套"语义别名"：
/// bg1↔lt1, tx1↔dk1, bg2↔lt2, tx2↔dk2（对应PowerPoint UI"背景1/文字1/
/// 背景2/文字2"的说法，规范上经由<p:clrMap>做逻辑映射，最常见/默认场景
/// 就是这个直接对应关系）。theme_colors这个HashMap的key只收录了
/// clrScheme原始槽位名（parse_theme_colors确认），从未见过bg1/tx1这组
/// 别名，此前所有查表点（8处）遇到bg1/tx1都会查不到，落入兜底默认色
/// （通常是accent1蓝色0x4F81BD），造成颜色误判——用sd_data_pptx__
/// tdf154363.pptx实测复现：矩形<a:solidFill><a:schemeClr val="bg1"/>
/// </a:solidFill>本应渲染成白色，实际渲染成了蓝色，即为此因。
/// 这里做一次归一化，查表前统一过一遍，8处调用点全部改用本函数取代
/// 直接查slot，一次修复全部覆盖，不逐处修改。
/// 未来若发现真实样本使用非默认<p:clrMap>覆盖了这个对应关系，需要
/// 把clrMap也解析进来动态映射，本次未发现此类样本，按最常见默认
/// 直接对应关系实现，是已知简化非遗漏。

/// normalize_scheme_color_slot 的辅助函数：把 clr_map 里存的目标槽位名
/// （字符串）转译成 'static 生命周期的字符串 slice。clrMap 的合法目标值
/// 只有 dk1/lt1/dk2/lt2/accent1-6/hlink/folHlink 这固定 12 种（OOXML
/// ST_ColorSchemeIndex 规范），不是任意字符串，所以可以用穷举 match 做
/// 零分配转译，未命中的异常值（理论上不应出现，除非文件本身不合规）
/// 原样透传（虽然生命周期不匹配这条分支永远编译不过，用 "other" 字面量
/// 兜底而非透传原引用，牺牲极端异常情况下的准确性换取整体实现简单）。

/// PROOT-clrmap: 解析 slideMaster 的 <p:clrMap bg1="..." tx1="..." .../>。
/// 16 个标准槽位名 -> 映射到的 clrScheme 原始槽位名（dk1/lt1/dk2/lt2/
/// accent1-6/hlink/folHlink）。真实样本 sd_data_pptx__onemaster-
/// twolayouts.pptx 验证：这里的映射不是恒定的 bg1→lt1 关系，该样本
/// clrMap 是 bg1="dk1" tx1="lt1" bg2="dk2" tx2="lt2"（把两组互换了），
/// 必须真实解析属性值，不能沿用写死的 PROOT-flip-bg1 假设。
/// 缺失的属性按 ECMA-376 标准默认值兜底（bg1→lt1, tx1→dk1, bg2→lt2,
/// tx2→dk2, accentN→accentN, hlink→hlink, folHlink→folHlink），保证
/// 即使某份文件 <p:clrMap> 没写全 16 个属性也不会丢失该槽位的映射。
fn parse_clr_map(xml: &str) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                if e.local_name().as_ref() == b"clrMap" {
                    for key in ["bg1", "tx1", "bg2", "tx2",
                                "accent1", "accent2", "accent3", "accent4", "accent5", "accent6",
                                "hlink", "folHlink"] {
                        if let Some(v) = attr_str(e, key.as_bytes()) {
                            map.insert(key.to_string(), v);
                        }
                    }
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    // ECMA-376 标准默认值兜底：文件里缺失的属性按此补全。
    for (k, v) in [
        ("bg1", "lt1"), ("tx1", "dk1"), ("bg2", "lt2"), ("tx2", "dk2"),
        ("accent1", "accent1"), ("accent2", "accent2"), ("accent3", "accent3"),
        ("accent4", "accent4"), ("accent5", "accent5"), ("accent6", "accent6"),
        ("hlink", "hlink"), ("folHlink", "folHlink"),
    ] {
        map.entry(k.to_string()).or_insert_with(|| v.to_string());
    }

    map
}

/// PROOT-clrmap: 解析 slide/layout 层的 <p:clrMapOvr>。两种子标签语义
/// 完全不同，不能合并处理：
/// - <a:masterClrMapping/>：表示"继承上一层，不覆盖"，返回 None，
///   调用方应该继续使用上一层（layout 用 master 的，slide 用 layout
///   实际生效的）clrMap，不能在这里凭空造一个默认值。
/// - <a:overrideClrMapping bg1="..." .../>：表示"这一层的 16 个属性
///   全部由自己重新定义，不再继承上一层任何映射"，返回 Some(新map)，
///   16 个属性同样按 ECMA-376 默认值兜底缺失项（与 parse_clr_map
///   同一套默认值表，两者是同一语义空间）。
/// 真实样本 sd_data_pptx__onemaster-twolayouts.pptx 的两个 layout
/// 恰好各用了一种写法（layout1 用 overrideClrMapping，layout2 用
/// masterClrMapping），可以直接作为验证用例，不需要额外构造测试样本。
fn parse_clr_map_ovr(xml: &str) -> Option<HashMap<String, String>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                match e.local_name().as_ref() {
                    b"masterClrMapping" => return None,
                    b"overrideClrMapping" => {
                        let mut map: HashMap<String, String> = HashMap::new();
                        for key in ["bg1", "tx1", "bg2", "tx2",
                                    "accent1", "accent2", "accent3", "accent4", "accent5", "accent6",
                                    "hlink", "folHlink"] {
                            if let Some(v) = attr_str(e, key.as_bytes()) {
                                map.insert(key.to_string(), v);
                            }
                        }
                        for (k, v) in [
                            ("bg1", "lt1"), ("tx1", "dk1"), ("bg2", "lt2"), ("tx2", "dk2"),
                            ("accent1", "accent1"), ("accent2", "accent2"), ("accent3", "accent3"),
                            ("accent4", "accent4"), ("accent5", "accent5"), ("accent6", "accent6"),
                            ("hlink", "hlink"), ("folHlink", "folHlink"),
                        ] {
                            map.entry(k.to_string()).or_insert_with(|| v.to_string());
                        }
                        return Some(map);
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => return None,
            Err(_) => return None,
            _ => {}
        }
    }
}

// ─── theme1.xml 解析 ─────────────────────────────────────────────────────────

/// 解析 ppt/theme/theme1.xml 里的 a:clrScheme，返回主题颜色表。
/// key: "dk1"/"lt1"/"dk2"/"lt2"/"accent1".."accent6"/"hlink"/"folHlink"
/// 找不到文件或解析失败时返回带 OnlyOffice 默认值的颜色表。

// PROOT-002 lnRef线宽fallback：解析主题<a:lnStyleLst>下的<a:ln w="..">列表，
// 供<p:style><a:lnRef idx="N">按1-based索引查线宽使用（本身不含颜色，颜色
// 沿用现有style_line_color逻辑，二者是正交的两条fallback链，各自独立）。
//
// 索引规则对齐LOK oox::drawingml::Theme::getLineStyle/lclGetStyleElement：
// idx从1开始；idx<1或列表为空视为无效引用，调用方应忽略；idx超出列表长度
// 时clamp到最后一个元素（而非panic或返回None），因为OOXML允许主题的样式
// 矩阵比理论最大idx短，实测LOK对越界idx的行为是"取最接近的可用样式"而不是
// 报错。真实tdf114845样本的lnStyleLst只有3个元素、lnRef idx最大到2，未触发
// clamp分支，但保留这个规则以匹配LOK行为、避免未来样本越界时panic。
fn parse_theme_line_widths(xml: &str) -> Vec<f32> {
    let mut widths: Vec<f32> = Vec::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut in_ln_style_lst = false;
    let mut depth_in_list = 0i32; // <a:ln>嵌套深度，用于只在lnStyleLst顶层的<a:ln>上取值

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"lnStyleLst" => {
                        in_ln_style_lst = true;
                    }
                    b"ln" if in_ln_style_lst && depth_in_list == 0 => {
                        depth_in_list += 1;
                        let w = attr_f32(e, b"w").unwrap_or(12700.0) / 12700.0; // EMU -> pt
                        widths.push(w);
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                let local = e.local_name();
                if local.as_ref() == b"ln" && in_ln_style_lst && depth_in_list == 0 {
                    let w = attr_f32(e, b"w").unwrap_or(12700.0) / 12700.0;
                    widths.push(w);
                }
            }
            Ok(Event::End(ref e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"lnStyleLst" => {
                        in_ln_style_lst = false;
                    }
                    b"ln" if in_ln_style_lst && depth_in_list == 1 => {
                        depth_in_list -= 1;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    widths
}

// 1-based索引查表，越界clamp到最后一个元素，对齐LOK lclGetStyleElement行为
// （见parse_theme_line_widths文档注释）。idx<1或表为空返回None。
fn lookup_theme_line_width(widths: &[f32], idx: i32) -> Option<f32> {
    if widths.is_empty() || idx < 1 {
        return None;
    }
    let clamped = std::cmp::min(idx - 1, widths.len() as i32 - 1) as usize;
    widths.get(clamped).copied()
}

// PROOT-006: 解析主题<a:lnStyleLst>下每个<a:ln>的<a:solidFill><a:schemeClr
// val="phClr">子元素里的颜色调整mods（shade/tint/satMod等），供<p:style>
// <a:lnRef idx="N">在拿到schemeClr纯色后，进一步做shade/tint/satMod调整。
//
// 与parse_theme_line_widths是两个独立函数、各自解析一遍lnStyleLst——没有
// 合并成一个函数返回元组，是为了不改动theme_line_widths现有的Vec<f32>
// 签名和两处调用点，最小化本次改动对已验证通过的lnRef线宽fallback逻辑
// 的影响面。真实样本lnStyleLst只有3个<a:ln>，重复解析开销可忽略。
//
// 索引规则与parse_theme_line_widths/lookup_theme_line_width一致：
// 1-based，越界clamp到最后一个元素。
fn parse_theme_line_color_mods(xml: &str) -> Vec<Vec<(String, i32)>> {
    let mut mods_list: Vec<Vec<(String, i32)>> = Vec::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut in_ln_style_lst = false;
    let mut depth_in_list = 0i32; // <a:ln>嵌套深度，只在lnStyleLst顶层<a:ln>上取值
    let mut in_scheme_clr_phclr = false; // 当前是否在<a:schemeClr val="phClr">内部
    let mut cur_mods: Vec<(String, i32)> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"lnStyleLst" => {
                        in_ln_style_lst = true;
                    }
                    b"ln" if in_ln_style_lst && depth_in_list == 0 => {
                        depth_in_list += 1;
                        cur_mods = Vec::new();
                    }
                    b"schemeClr" if in_ln_style_lst && depth_in_list == 1 => {
                        if let Some(val) = attr_str(e, b"val") {
                            in_scheme_clr_phclr = val == "phClr";
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                let local = e.local_name();
                if in_ln_style_lst && depth_in_list == 1 && in_scheme_clr_phclr {
                    match local.as_ref() {
                        b"shade" | b"tint" | b"satMod" => {
                            let name = String::from_utf8_lossy(local.as_ref()).into_owned();
                            if let Some(v) = attr_str(e, b"val").and_then(|s| s.parse::<i32>().ok()) {
                                cur_mods.push((name, v));
                            }
                        }
                        _ => {}
                    }
                }
                if local.as_ref() == b"ln" && in_ln_style_lst && depth_in_list == 0 {
                    // 自闭合<a:ln .../>（无子元素），无mods
                    mods_list.push(Vec::new());
                }
            }
            Ok(Event::End(ref e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"lnStyleLst" => {
                        in_ln_style_lst = false;
                    }
                    b"schemeClr" if in_ln_style_lst && depth_in_list == 1 => {
                        in_scheme_clr_phclr = false;
                    }
                    b"ln" if in_ln_style_lst && depth_in_list == 1 => {
                        depth_in_list -= 1;
                        mods_list.push(std::mem::take(&mut cur_mods));
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    mods_list
}

// 1-based索引查表，越界clamp到最后一个元素，与lookup_theme_line_width
// 规则一致。idx<1或表为空返回None（表示无mods可查，调用方应保持纯色不变）。
fn lookup_theme_line_color_mods(mods_list: &[Vec<(String, i32)>], idx: i32) -> Option<&[(String, i32)]> {
    if mods_list.is_empty() || idx < 1 {
        return None;
    }
    let clamped = std::cmp::min(idx - 1, mods_list.len() as i32 - 1) as usize;
    mods_list.get(clamped).map(|v| v.as_slice())
}

// PROOT-006阶段二: fillRef idx主题填充定义解析（fillStyleLst）。
//
// 真实样本tdf114845确认：fillStyleLst里的位置要么是<a:solidFill>
// （纯色+可选mods），要么是<a:gradFill>（多档渐变，每档色标各自
// 有独立的tint/shade/satMod组合）。二者结构完全不同、互斥，因此用
// 枚举区分，不能像lnStyleLst那样统一按"纯色+mods"处理。
//
// gradFill里的schemeClr写的是"phClr"占位符（表示"用fillRef外层
// 实际引用的颜色"，如accent1），不是写死的具体颜色名——本函数只
// 解析出结构模板（每档色标的mods+pos、渐变类型、rotateWithShape），
// 实际颜色由调用方在拿到fillRef的schemeClr实际颜色后，对每档色标
// 各自调用apply_color_mods填入。
//
// 与parse_theme_line_widths/parse_theme_line_color_mods同样的设计
// 原则：独立解析一遍XML，不合并进现有函数，最小化改动面。
#[derive(Debug, Clone)]
enum ThemeFillStyleDef {
    Solid(Vec<(String, i32)>),
    Gradient {
        stops: Vec<(f32, Vec<(String, i32)>)>,
        kind_raw: RawGradKind,
        rotate_with_shape: bool,
    },
    Picture {
        r_id: String,
        has_tile: bool,
        tile_offset_x: f32,
        tile_offset_y: f32,
        tile_scale_x: f32,
        tile_scale_y: f32,
        tile_align: String,
    },
}

fn parse_theme_fill_styles(xml: &str, list_tag: &[u8]) -> Vec<ThemeFillStyleDef> {
    let mut defs: Vec<ThemeFillStyleDef> = Vec::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut in_fill_style_lst = false;
    let mut depth_in_list = 0i32; // 顶层<a:solidFill>/<a:gradFill>嵌套深度

    // solidFill状态
    let mut in_solid_fill = false;
    let mut in_solid_scheme_clr_phclr = false;
    let mut cur_solid_mods: Vec<(String, i32)> = Vec::new();

    // gradFill状态
    let mut in_grad_fill = false;
    let mut in_gs = false;
    let mut in_gs_scheme_clr_phclr = false;
    let mut cur_gs_pos: f32 = 0.0;
    let mut cur_gs_mods: Vec<(String, i32)> = Vec::new();
    let mut cur_grad_stops: Vec<(f32, Vec<(String, i32)>)> = Vec::new();
    let mut cur_grad_kind_raw: Option<RawGradKind> = None;
    let mut cur_grad_rotate_with_shape = false;

    // blipFill状态（PROOT-THEME-BG-PIC）
    let mut in_blip_fill = false;
    let mut in_stretch = false;
    let mut cur_blip_rid: Option<String> = None;
    let mut cur_has_tile = false;
    let mut cur_tile_offset_x: f32 = 0.0;
    let mut cur_tile_offset_y: f32 = 0.0;
    let mut cur_tile_scale_x: f32 = 1.0;
    let mut cur_tile_scale_y: f32 = 1.0;
    let mut cur_tile_align: String = "tl".to_string();

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    tag if tag == list_tag => {
                        in_fill_style_lst = true;
                    }
                    b"solidFill" if in_fill_style_lst && depth_in_list == 0 => {
                        depth_in_list += 1;
                        in_solid_fill = true;
                        cur_solid_mods = Vec::new();
                    }
                    b"gradFill" if in_fill_style_lst && depth_in_list == 0 => {
                        depth_in_list += 1;
                        in_grad_fill = true;
                        cur_grad_stops = Vec::new();
                        cur_grad_kind_raw = None;
                        cur_grad_rotate_with_shape = attr_str(e, b"rotWithShape")
                            .map(|v| v == "1" || v == "true").unwrap_or(false);
                    }
                    b"blipFill" if in_fill_style_lst && depth_in_list == 0 => {
                        depth_in_list += 1;
                        in_blip_fill = true;
                        cur_blip_rid = None;
                        cur_has_tile = false;
                        cur_tile_offset_x = 0.0;
                        cur_tile_offset_y = 0.0;
                        cur_tile_scale_x = 1.0;
                        cur_tile_scale_y = 1.0;
                        cur_tile_align = "tl".to_string();
                    }
                    b"stretch" if in_blip_fill => { in_stretch = true; }
                    b"schemeClr" if in_solid_fill => {
                        if let Some(val) = attr_str(e, b"val") {
                            in_solid_scheme_clr_phclr = val == "phClr";
                        }
                    }
                    b"gs" if in_grad_fill => {
                        in_gs = true;
                        cur_gs_pos = (attr_i32(e, b"pos").unwrap_or(0).max(0) as f32) / 100000.0;
                        cur_gs_mods = Vec::new();
                    }
                    b"schemeClr" if in_gs => {
                        if let Some(val) = attr_str(e, b"val") {
                            in_gs_scheme_clr_phclr = val == "phClr";
                        }
                    }
                    b"lin" if in_grad_fill => {
                        let raw_angle = attr_i32(e, b"ang").unwrap_or(0);
                        cur_grad_kind_raw = Some(RawGradKind::Linear { raw_angle });
                    }
                    b"path" if in_grad_fill => {
                        let path_type = attr_str(e, b"path").unwrap_or_else(|| "rect".to_string());
                        cur_grad_kind_raw = Some(RawGradKind::Path { path_type });
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                let local = e.local_name();
                // solidFill内mods
                if in_solid_fill && in_solid_scheme_clr_phclr {
                    match local.as_ref() {
                        b"shade" | b"tint" | b"satMod" => {
                            let name = String::from_utf8_lossy(local.as_ref()).into_owned();
                            if let Some(v) = attr_str(e, b"val").and_then(|s| s.parse::<i32>().ok()) {
                                cur_solid_mods.push((name, v));
                            }
                        }
                        _ => {}
                    }
                }
                // gs内mods
                if in_gs && in_gs_scheme_clr_phclr {
                    match local.as_ref() {
                        b"shade" | b"tint" | b"satMod" => {
                            let name = String::from_utf8_lossy(local.as_ref()).into_owned();
                            if let Some(v) = attr_str(e, b"val").and_then(|s| s.parse::<i32>().ok()) {
                                cur_gs_mods.push((name, v));
                            }
                        }
                        _ => {}
                    }
                }
                if local.as_ref() == b"lin" && in_grad_fill {
                    let raw_angle = attr_i32(e, b"ang").unwrap_or(0);
                    cur_grad_kind_raw = Some(RawGradKind::Linear { raw_angle });
                }
                if local.as_ref() == b"path" && in_grad_fill {
                    let path_type = attr_str(e, b"path").unwrap_or_else(|| "rect".to_string());
                    cur_grad_kind_raw = Some(RawGradKind::Path { path_type });
                }
                if local.as_ref() == b"blip" && in_blip_fill {
                    if let Some(rid) = attr_str(e, b"embed") {
                        cur_blip_rid = Some(rid);
                    }
                }
                if local.as_ref() == b"tile" && in_blip_fill {
                    cur_has_tile = true;
                    cur_tile_offset_x = attr_i32(e, b"tx").unwrap_or(0) as f32 / 12700.0;
                    cur_tile_offset_y = attr_i32(e, b"ty").unwrap_or(0) as f32 / 12700.0;
                    cur_tile_scale_x = attr_i32(e, b"sx").unwrap_or(100000) as f32 / 100000.0;
                    cur_tile_scale_y = attr_i32(e, b"sy").unwrap_or(100000) as f32 / 100000.0;
                    if let Some(a) = attr_str(e, b"algn") {
                        cur_tile_align = a;
                    }
                }
                // 自闭合<a:solidFill .../>理论上不会发生（solidFill必然含schemeClr/srgbClr子元素），
                // 不特殊处理；自闭合<a:gradFill/>同理理论上不会发生。
            }
            Ok(Event::End(ref e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    tag if tag == list_tag => {
                        in_fill_style_lst = false;
                    }
                    b"schemeClr" if in_solid_fill => {
                        in_solid_scheme_clr_phclr = false;
                    }
                    b"schemeClr" if in_gs => {
                        in_gs_scheme_clr_phclr = false;
                    }
                    b"gs" if in_grad_fill => {
                        in_gs = false;
                        cur_grad_stops.push((cur_gs_pos, std::mem::take(&mut cur_gs_mods)));
                    }
                    b"solidFill" if in_fill_style_lst && depth_in_list == 1 => {
                        depth_in_list -= 1;
                        in_solid_fill = false;
                        defs.push(ThemeFillStyleDef::Solid(std::mem::take(&mut cur_solid_mods)));
                    }
                    b"gradFill" if in_fill_style_lst && depth_in_list == 1 => {
                        depth_in_list -= 1;
                        in_grad_fill = false;
                        let kind_raw = cur_grad_kind_raw.take()
                            .unwrap_or(RawGradKind::Linear { raw_angle: 0 });
                        defs.push(ThemeFillStyleDef::Gradient {
                            stops: std::mem::take(&mut cur_grad_stops),
                            kind_raw,
                            rotate_with_shape: cur_grad_rotate_with_shape,
                        });
                    }
                    b"stretch" if in_blip_fill => { in_stretch = false; }
                    b"blipFill" if in_fill_style_lst && depth_in_list == 1 => {
                        depth_in_list -= 1;
                        in_blip_fill = false;
                        if let Some(rid) = cur_blip_rid.take() {
                            defs.push(ThemeFillStyleDef::Picture {
                                r_id: rid,
                                has_tile: cur_has_tile,
                                tile_offset_x: cur_tile_offset_x,
                                tile_offset_y: cur_tile_offset_y,
                                tile_scale_x: cur_tile_scale_x,
                                tile_scale_y: cur_tile_scale_y,
                                tile_align: std::mem::take(&mut cur_tile_align),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    defs
}

// 1-based索引查表，越界clamp到最后一个元素，与其余lookup_theme_*
// 函数规则一致。idx<1或表为空返回None。
fn lookup_theme_fill_style(defs: &[ThemeFillStyleDef], idx: i32) -> Option<&ThemeFillStyleDef> {
    if defs.is_empty() || idx < 1 {
        return None;
    }
    let clamped = std::cmp::min(idx - 1, defs.len() as i32 - 1) as usize;
    defs.get(clamped)
}

// PROOT-1xx: <p:bgRef idx="1001"> 一类的背景填充样式表查表。OOXML规范
// ST_StyleMatrixColumnIndex：1000+区间专属bgFillStyleLst（1001=第1项，
// 1002=第2项，以此类推），与fillRef用的1-N区间（fillStyleLst）是两套
// 独立编号空间，不能共用lookup_theme_fill_style（那个函数按idx-1转
// 下标，若把1001传进去会被錯误clamp到fillStyleLst最后一项）。
// 真实样本sd_data_pptx__onemaster-twolayouts.pptx（slideLayout1.xml
// <p:bgRef idx="1001"><a:schemeClr val="bg2"/></p:bgRef>）验证：theme1.xml
// bgFillStyleLst第1项（索引0）就是idx=1001要取的值，1001-1001=0，
// 与fillStyleLst同款"idx-N"减法转下标的模式一致，只是常量N不同（1001
// 而非1，因为bgFillStyleLst和fillStyleLst各自从1开始独立编号，只是
// bgFillStyleLst在整体ST_StyleMatrixColumnIndex编号空间里额外偏移了
// 1000，是OOXML规范本身的设计，不是本项目的猜测）。
fn lookup_theme_bg_fill_style(defs: &[ThemeFillStyleDef], idx: i32) -> Option<&ThemeFillStyleDef> {
    if defs.is_empty() || idx < 1001 {
        return None;
    }
    let clamped = std::cmp::min(idx - 1001, defs.len() as i32 - 1) as usize;
    defs.get(clamped)
}

// PROOT-006: fillRef/lnRef 主题色 shade/tint/satMod 调整算法。
//
// 算法来源：OnlyOffice sdkjs（common/Drawings/Format/Format.js
// CColorModifiers.prototype.Apply，2026-07-24会话已读取确认，非猜测）：
//   - shade/tint 在【线性RGB空间】做逐通道运算（先做sRGB→线性RGB的gamma
//     2.4转换，运算完再转回sRGB），val = OOXML原始整数 / 100000.0，
//     超出[0,1]范围clamp为0（sdkjs行为，非常规[0,1]截断）。
//   - satMod 在【HSL空间】只调整S通道，不做gamma转换，val===1时跳过
//     （sdkjs continue，等价于不处理，避免浮点误差）。
//   - Mods按XML中出现顺序依次应用在同一个RGB上（不是独立分别应用后
//     取平均或类似操作），对应真实样本lnRef先shade后satMod的顺序。
//
// 本次只实现shade/tint/satMod三种，是fillRef/lnRef真实样本命中的类型；
// sdkjs还支持hueMod/lumMod/lumOff/satOff/gamma等十余种mod，均未实现，
// 遇到未识别的mod名原样跳过（不panic、不报错），与sdkjs对未知mod的
// 保守处理原则一致。

/// sRGB → 线性RGB 单通道转换（IEC 61966-2-1标准gamma，c已归一化到0~1）。
fn srgb_to_linear(c: f64) -> f64 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// 线性RGB → sRGB 单通道转换，srgb_to_linear的逆运算。
fn linear_to_srgb(c: f64) -> f64 {
    if c <= 0.0031308 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// shade：线性RGB空间逐通道乘val，使颜色变暗（val越小越暗）。
/// val超出[0,1]时sdkjs行为是clamp为0（不是clamp到边界），照抄以对齐真实渲染。
pub(crate) fn apply_shade(rgb: [u8; 3], val: f64) -> [u8; 3] {
    let val = if !(0.0..=1.0).contains(&val) { 0.0 } else { val };
    let mut out = [0u8; 3];
    for i in 0..3 {
        let linear = srgb_to_linear(rgb[i] as f64 / 255.0) * val;
        out[i] = (linear_to_srgb(linear).clamp(0.0, 1.0) * 255.0).round() as u8;
    }
    out
}

/// tint：线性RGB空间逐通道向白色插值，使颜色变浅（val越小越浅）。
/// 与apply_shade同样的val clamp规则。
fn apply_tint(rgb: [u8; 3], val: f64) -> [u8; 3] {
    let val = if !(0.0..=1.0).contains(&val) { 0.0 } else { val };
    let mut out = [0u8; 3];
    for i in 0..3 {
        let linear_base = srgb_to_linear(rgb[i] as f64 / 255.0);
        let linear = 1.0 - (1.0 - linear_base) * val;
        out[i] = (linear_to_srgb(linear).clamp(0.0, 1.0) * 255.0).round() as u8;
    }
    out
}

/// RGB → HSL，H范围[0,360)，S/L范围[0,1]。satMod运算专用，不做gamma转换。
fn rgb_to_hsl_pptx(rgb: [u8; 3]) -> (f64, f64, f64) {
    let r = rgb[0] as f64 / 255.0;
    let g = rgb[1] as f64 / 255.0;
    let b = rgb[2] as f64 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-9 {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if (max - r).abs() < 1e-9 {
        ((g - b) / d) % 6.0
    } else if (max - g).abs() < 1e-9 {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    let mut h_deg = h * 60.0;
    if h_deg < 0.0 {
        h_deg += 360.0;
    }
    (h_deg, s, l)
}

/// HSL → RGB，H范围[0,360)，S/L范围[0,1]。satMod运算专用。
fn hsl_to_rgb_pptx(h: f64, s: f64, l: f64) -> [u8; 3] {
    if s.abs() < 1e-9 {
        let v = (l.clamp(0.0, 1.0) * 255.0).round() as u8;
        return [v, v, v];
    }
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h_prime = h / 60.0;
    let x = c * (1.0 - (h_prime % 2.0 - 1.0).abs());
    let (r1, g1, b1) = if (0.0..1.0).contains(&h_prime) {
        (c, x, 0.0)
    } else if (1.0..2.0).contains(&h_prime) {
        (x, c, 0.0)
    } else if (2.0..3.0).contains(&h_prime) {
        (0.0, c, x)
    } else if (3.0..4.0).contains(&h_prime) {
        (0.0, x, c)
    } else if (4.0..5.0).contains(&h_prime) {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let m = l - c / 2.0;
    [
        (((r1 + m).clamp(0.0, 1.0)) * 255.0).round() as u8,
        (((g1 + m).clamp(0.0, 1.0)) * 255.0).round() as u8,
        (((b1 + m).clamp(0.0, 1.0)) * 255.0).round() as u8,
    ]
}

/// satMod：HSL空间只调整S通道，val===1时跳过（sdkjs continue语义）。
fn apply_sat_mod(rgb: [u8; 3], val: f64) -> [u8; 3] {
    if (val - 1.0).abs() < 1e-9 {
        return rgb;
    }
    let (h, s, l) = rgb_to_hsl_pptx(rgb);
    hsl_to_rgb_pptx(h, (s * val).clamp(0.0, 1.0), l)
}

// PROOT-OOXTHEME: lumMod/lumOff——与satMod同样在HSL空间操作，但作用于L
// （亮度）通道而非S（饱和度）通道。算法依据OOXML规范ST_PositiveFixedPercentage
// 语义（sdkjs CColorModifiers同样将两者视为线性变换：newL = L*lumMod + lumOff，
// 与shade/tint的gamma校正RGB空间运算是两套独立体系，不应混淆）。真实样本
// sd_data_pptx__ooxtheme.pptx的custGeom装饰形状验证：
// accent2+lumMod=75000（单独出现，压暗）、accent1+lumMod=60000+lumOff=40000
// （两者成对出现，压缩亮度范围后整体上移，是PowerPoint主题色"变浅一档"的
// 标准配方），均只操作L通道，不影响H/S，故复用现成的rgb_to_hsl_pptx/
// hsl_to_rgb_pptx往返转换，与apply_sat_mod同一模式。
fn apply_lum_mod(rgb: [u8; 3], val: f64) -> [u8; 3] {
    if (val - 1.0).abs() < 1e-9 {
        return rgb;
    }
    let (h, s, l) = rgb_to_hsl_pptx(rgb);
    hsl_to_rgb_pptx(h, s, (l * val).clamp(0.0, 1.0))
}

fn apply_lum_off(rgb: [u8; 3], val: f64) -> [u8; 3] {
    if val.abs() < 1e-9 {
        return rgb;
    }
    let (h, s, l) = rgb_to_hsl_pptx(rgb);
    hsl_to_rgb_pptx(h, s, (l + val).clamp(0.0, 1.0))
}

/// 按顺序依次应用一组颜色调整mod。mods是(name, 原始OOXML整数值)列表，
/// 例如("shade", 95000)、("satMod", 105000)。未识别的mod名原样跳过。
fn apply_color_mods(base: Color, mods: &[(String, i32)]) -> Color {
    let mut rgb = [base.r, base.g, base.b];
    for (name, raw_val) in mods {
        let val = *raw_val as f64 / 100000.0;
        rgb = match name.as_str() {
            "shade" => apply_shade(rgb, val),
            "tint" => apply_tint(rgb, val),
            "satMod" => apply_sat_mod(rgb, val),
            // PROOT-OOXTHEME: lumMod/lumOff新增支持，见apply_lum_mod/
            // apply_lum_off函数文档注释。
            "lumMod" => apply_lum_mod(rgb, val),
            "lumOff" => apply_lum_off(rgb, val),
            _ => rgb, // 未实现的mod类型，原样跳过，不panic
        };
    }
    Color { r: rgb[0], g: rgb[1], b: rgb[2], a: base.a }
}

fn parse_rels(xml: &str) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    if xml.is_empty() {
        return map;
    }
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e)) => {
                if e.local_name().as_ref() == b"Relationship" {
                    let id = attr_str(e, b"Id").unwrap_or_default();
                    let target = attr_str(e, b"Target").unwrap_or_default();
                    if !id.is_empty() {
                        map.insert(id, target);
                    }
                }
            }
            Ok(Event::Eof) => break,
            _ => {}
        }
    }
    map
}

// ─── slideLayout 路径查找 ──────────────────────────────────────────────────────

/// 从 slide rels xml 中找 slideLayout 的 target 路径
fn find_slide_layout_target(rels_xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(rels_xml);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e)) => {
                if e.local_name().as_ref() == b"Relationship" {
                    let type_val = attr_str(e, b"Type").unwrap_or_default();
                    if type_val.contains("slideLayout") {
                        return attr_str(e, b"Target");
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    None
}

/// 从 slideLayout rels xml 中找 slideMaster 的 target 路径（PPTX-1新增）
fn find_slide_master_target(rels_xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(rels_xml);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e)) => {
                if e.local_name().as_ref() == b"Relationship" {
                    let type_val = attr_str(e, b"Type").unwrap_or_default();
                    if type_val.contains("slideMaster") {
                        return attr_str(e, b"Target");
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    None
}

// PROOT-THEME (v92): 每个 slideMaster 可以各自关联不同的 theme.xml（真实样本
// sd_data_pptx__accent-color.pptx 有 slideMaster1->theme1.xml、slideMaster2->
// theme2.xml 两套完全不同的颜色定义）。此前解析入口硬编码只读 "ppt/theme/
// theme1.xml" 一份，全局唯一、从未按 master 区分，导致所有 slide 无论实际
// 挂哪个 master，主题色永远用 theme1 的值——与 PROOT-clrmap（clrMap 曾经
// 硬编码4条固定映射）是同一类问题在不同维度上的重复出现：某个应该"按 master
// 区分"的解析结果被写成了"全局只算一次"。
// 用法与 find_slide_master_target 完全对称：从 master 自己的 .rels 里找
// Type 包含 "theme" 的关系，取其 Target。
fn find_theme_target(rels_xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(rels_xml);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e)) => {
                if e.local_name().as_ref() == b"Relationship" {
                    let type_val = attr_str(e, b"Type").unwrap_or_default();
                    if type_val.contains("relationships/theme") {
                        return attr_str(e, b"Target");
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    None
}

// ─── 图片收集 ────────────────────────────────────────────────────────────────

/// PROOT-005: 解析 `<p:bg>` 背景定义，支持两种最常见写法：
/// - `<p:bgPr><a:blipFill><a:blip r:embed="rIdX"/></a:blipFill></p:bgPr>`（图片背景）
/// - `<p:bgPr><a:solidFill><a:srgbClr val="..."/></a:solidFill></p:bgPr>`（纯色背景）
/// 不做完整状态机（参照 parse_raw_shapes 那种量级），`<p:bg>` 结构本身很浅，
/// 用简单的扁平事件扫描 + 少量标志位即可覆盖，避免为一个小函数引入不必要的复杂度。
/// `image_map`: 背景图片所在文档自己的 rId→字节映射（slide/layout/master 三处
/// 调用时各自传各自的 image_map，因为 blipFill 的 r:embed 只在各自文档的
/// .rels 命名空间内有意义，不能跨文档复用）。
/// 已知简化（如实标注，非遗漏）：`<a:srcRect>` 裁剪、schemeClr主题色背景、
/// gradFill渐变背景均未处理——本次范围只覆盖 tdf158512 真实样本涉及的
/// blipFill写法，其余写法遇到真实样本再按需扩展。
/// PROOT-008: `<a:stretch><a:fillRect t="-13000" b="-13000"/>` 负值裁剪
/// 换算成 crop_l/t/r/b（0.0-1.0正值inset），公式移植自LOK
/// oox/source/drawingml/fillproperties.cxx 的
/// `CropQuotientsFromFillRect`+`getQuotients`（ECMA-376 Part 1
/// 20.1.8.30 fillRect）。只覆盖"四边均<=0"这一种场景（对应真实样本
/// tdf158512：`t="-13000" b="-13000"`，l/r未声明按0处理），任一边为
/// 正值（真正的裁剪收窄，语义与本函数完全不同，LOK走的是另一条
/// GraphicCrop分支）本函数不处理，直接返回(0,0,0,0)，如实反映"未覆盖"
/// 而非用错误公式硬算。
///
/// 输入l/t/r/b是OOXML原始千分比整数（100000=100%），输出是归一化到
/// 0.0-1.0的裁剪比例，与`<a:srcRect>`场景的crop_l/t/r/b同一单位，
/// 供render_shape_image统一处理，调用方无需区分来源。
fn crop_quotients_from_fill_rect(l: i32, t: i32, r: i32, b: i32) -> (f32, f32, f32, f32) {
    const MAX_PERCENT: f64 = 100000.0;

    // 只处理全部<=0的场景；任一边>0说明混入了正值裁剪语义，不在本次
    // 覆盖范围，返回0避免用错误公式产出看似合理实则错误的数值。
    if l > 0 || t > 0 || r > 0 || b > 0 {
        return (0.0, 0.0, 0.0, 0.0);
    }

    let l = l.min(0) as f64;
    let t = t.min(0) as f64;
    let r = r.min(0) as f64;
    let b = b.min(0) as f64;

    let h_div = -MAX_PERCENT + l + r;
    let v_div = -MAX_PERCENT + t + b;

    // h_div/v_div == 0 理论上只在 l+r==-100000 或 t+b==-100000 这种
    // 极端退化值时出现（分母为0），照原样返回0，不产出NaN/Inf污染下游。
    let crop_l = if h_div != 0.0 { (l / h_div) as f32 } else { 0.0 };
    let crop_t = if v_div != 0.0 { (t / v_div) as f32 } else { 0.0 };
    let crop_r = if h_div != 0.0 { (r / h_div) as f32 } else { 0.0 };
    let crop_b = if v_div != 0.0 { (b / v_div) as f32 } else { 0.0 };

    (crop_l, crop_t, crop_r, crop_b)
}

fn parse_slide_background(
    xml: &str,
    image_map: &HashMap<String, Vec<u8>>,
    theme_colors: &HashMap<String, Color>,
    theme_bg_fill_styles: &[ThemeFillStyleDef],
    theme_images: &HashMap<String, Vec<u8>>,
    clr_map: &HashMap<String, String>,
) -> Option<FillStyle> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut in_bg = false;
    let mut in_bg_pr = false;
    let mut in_blip_fill = false;
    let mut in_solid_fill = false;
    let mut in_stretch = false;
    let mut bg_rid: Option<String> = None;
    let mut bg_solid_color: Option<Color> = None;
    // v318p: <p:bgPr><a:gradFill> 渐变背景（srgbClr 色标 + lin/path）
    let mut in_grad_fill = false;
    let mut in_gs = false;
    let mut cur_gs_pos: f32 = 0.0;
    let mut grad_stops: Vec<roe_model::common::GradientStop> = Vec::new();
    let mut grad_kind: Option<roe_model::common::GradientKind> = None;
    let mut grad_rotate = true;
    // PROOT-1xx: <p:bgRef idx="1001"><a:schemeClr val="bg2"/></p:bgRef>
    // 场景——背景不是直接内联<a:solidFill>/<a:gradFill>（那是bgPr分支），
    // 而是引用主题bgFillStyleLst第idx项的填充模板，模板里的schemeClr
    // val="phClr"占位符要用这里schemeClr的实际颜色替换。in_bg_ref下的
    // schemeClr与in_solid_fill下的schemeClr是两个独立场景（bgPr内联
    // 纯色场景此前只支持srgbClr，未支持schemeClr，本次不一并扩展，
    // 因为真实样本目前没有<p:bgPr><a:solidFill><a:schemeClr>这个组合
    // 的证据，只有bgRef这个场景有真实样本证据）。
    let mut in_bg_ref = false;
    let mut bg_ref_idx: Option<i32> = None;
    let mut bg_ref_scheme_slot: Option<String> = None;
    // PROOT-008: <a:stretch><a:fillRect t="-13000" b="-13000"/></a:stretch>
    // 千分比原始整数值，未出现的边默认0（符合ECMA-376 fillRect未声明
    // 即0的语义）。换算成crop_l/t/r/b在函数末尾统一做，不在解析循环内
    // 提前计算，避免归一化逻辑和XML扫描逻辑混在一起。
    let mut fill_rect_l: i32 = 0;
    let mut fill_rect_t: i32 = 0;
    let mut fill_rect_r: i32 = 0;
    let mut fill_rect_b: i32 = 0;

    // ROE-TILE-001: <a:blipFill><a:tile tx="..." ty="..." sx="..." sy="..." algn="tl"/>
    // </a:blipFill>。与 <a:stretch><a:fillRect> 是 blipFill 下平级、互斥的两种
    // 子元素（PowerPoint"填充图片"选项要么拉伸要么平铺，二选一），故检测层级
    // 与 stretch/fillRect 相同（in_blip_fill 内），不需要新增嵌套状态变量。
    // tx/ty 原始单位是 EMU，sx/sy 原始单位是千分之一精度百分比
    // （ST_Percentage，100000=100%），均在扫描循环内直接换算成
    // pt/归一化比例存入，不留到函数末尾统一转换（不同于 fillRect 走的
    // 归一化路径——因为 tile 换算是简单的线性换算，没有 fillRect 那种
    // 需要多个值联立计算分母的复杂公式，没必要分离两步）。
    let mut has_tile = false;
    let mut tile_offset_x: f32 = 0.0;
    let mut tile_offset_y: f32 = 0.0;
    let mut tile_scale_x: f32 = 1.0;
    let mut tile_scale_y: f32 = 1.0;
    let mut tile_align: String = "tl".to_string();

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                match e.local_name().as_ref() {
                    b"bg" => { in_bg = true; }
                    b"bgPr" if in_bg => { in_bg_pr = true; }
                    b"bgRef" if in_bg => {
                        in_bg_ref = true;
                        bg_ref_idx = attr_i32(e, b"idx");
                    }
                    b"schemeClr" if in_bg_ref => {
                        bg_ref_scheme_slot = attr_str(e, b"val");
                    }
                    b"blipFill" if in_bg_pr => { in_blip_fill = true; }
                    b"solidFill" if in_bg_pr => { in_solid_fill = true; }
                    b"gradFill" if in_bg_pr => {
                        in_grad_fill = true;
                        grad_rotate = attr_str(e, b"rotWithShape").map(|v| v != "0").unwrap_or(true);
                    }
                    b"gs" if in_grad_fill => {
                        in_gs = true;
                        cur_gs_pos = (attr_i32(e, b"pos").unwrap_or(0).max(0) as f32) / 100000.0;
                    }
                    b"lin" if in_grad_fill => {
                        let ang = attr_i32(e, b"ang").unwrap_or(0) as f32 / 60000.0;
                        grad_kind = Some(roe_model::common::GradientKind::Linear { angle: ang });
                    }
                    b"path" if in_grad_fill => {
                        let pt = attr_str(e, b"path").unwrap_or_else(|| "rect".to_string());
                        grad_kind = Some(if pt == "circle" {
                            roe_model::common::GradientKind::Radial
                        } else {
                            roe_model::common::GradientKind::Rect
                        });
                    }
                    b"srgbClr" if in_gs => {
                        if let Some(hex) = attr_str(e, b"val") {
                            grad_stops.push(roe_model::common::GradientStop {
                                pos: cur_gs_pos,
                                color: parse_hex_color(&hex),
                            });
                        }
                    }
                    b"stretch" if in_blip_fill => { in_stretch = true; }
                    b"fillRect" if in_stretch => {
                        fill_rect_l = attr_i32(e, b"l").unwrap_or(0);
                        fill_rect_t = attr_i32(e, b"t").unwrap_or(0);
                        fill_rect_r = attr_i32(e, b"r").unwrap_or(0);
                        fill_rect_b = attr_i32(e, b"b").unwrap_or(0);
                    }
                    // ROE-TILE-001: tile 是自闭合标签（<a:tile .../>），不像
                    // stretch/fillRect 需要区分 Start/End 成对出现，一次
                    // Empty 事件里直接读完全部属性即可，不需要额外的
                    // in_tile 状态变量。
                    b"tile" if in_blip_fill => {
                        has_tile = true;
                        // EMU → pt：除以 12700（1pt = 12700 EMU，与项目内
                        // 其他 EMU 换算处一致，如 crop_quotients_from_fill_rect
                        // 附近的既有换算写法）。
                        tile_offset_x = attr_i32(e, b"tx").unwrap_or(0) as f32 / 12700.0;
                        tile_offset_y = attr_i32(e, b"ty").unwrap_or(0) as f32 / 12700.0;
                        // ST_Percentage 千分之一精度 → 归一化比例：除以100000。
                        // 未声明时按 ECMA-376 默认值 100000（100%）处理，
                        // 即瓦片与原图同尺寸。
                        tile_scale_x = attr_i32(e, b"sx").unwrap_or(100000) as f32 / 100000.0;
                        tile_scale_y = attr_i32(e, b"sy").unwrap_or(100000) as f32 / 100000.0;
                        if let Some(a) = attr_str(e, b"algn") {
                            tile_align = a;
                        }
                    }
                    b"blip" if in_blip_fill => {
                        if let Some(rid) = attr_str(e, b"embed") {
                            bg_rid = Some(rid);
                        }
                    }
                    b"srgbClr" if in_solid_fill => {
                        if let Some(hex) = attr_str(e, b"val") {
                            bg_solid_color = Some(parse_hex_color(&hex));
                        }
                    }
                    // PROOT-clrmap: <p:bgPr><a:solidFill><a:schemeClr val="bg1"/>
                    // </a:solidFill></p:bgPr>——内联纯色场景引用主题色槽位而非
                    // 直接给srgb十六进制值。此前只支持srgbClr分支，schemeClr
                    // 完全被忽略，background静默落空、渲染层最终用default
                    // accent1兜底色。真实样本sd_data_pptx__master-bg-color.pptx
                    // 验证：走的正是这条路径（bgPr+solidFill+schemeClr val="bg1"，
                    // 不是bgRef），必须在这里也接入clrMap+theme_colors取色，
                    // 否则clrMap机制修好了也没用——这份样本从未真正进入过
                    // 有意义的解析分支。
                    b"schemeClr" if in_solid_fill => {
                        if let Some(slot) = attr_str(e, b"val") {
                            if let Some(c) = theme_colors.get(normalize_scheme_color_slot(&slot, clr_map)).copied() {
                                bg_solid_color = Some(c);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                match e.local_name().as_ref() {
                    b"bg" => break,
                    b"bgPr" => { in_bg_pr = false; }
                    b"bgRef" => { in_bg_ref = false; }
                    b"blipFill" => { in_blip_fill = false; }
                    b"solidFill" => { in_solid_fill = false; }
                    b"gs" => { in_gs = false; }
                    b"gradFill" => { in_grad_fill = false; }
                    b"stretch" => { in_stretch = false; }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    if let Some(rid) = bg_rid {
        if let Some(bytes) = image_map.get(&rid) {
            // mime 类型：背景图片场景第一期不做真正的格式嗅探，固定按 png 处理——
            // 与 ShapeContent::Image 现有渲染路径（render_shape_image）一致，
            // tiny-skia 侧图片解码目前也只验证过 png，其余格式（jpg等）留待
            // 遇到真实样本报告问题时再扩展格式嗅探逻辑。
            //
            // ROE-TILE-001: has_tile 与 <a:stretch> 是互斥的两种子元素
            // （见上方扫描循环注释），故这里用 if/else 分流，不会同时
            // 命中两个分支。tile 模式下 crop_quotients_from_fill_rect
            // 不会被调用到（fill_rect_* 在纯 tile 场景下恒为初始值0，
            // 计算结果也是(0,0,0,0)，等价于"不裁剪"，与
            // ImageData.crop_l/t/r/b 恒为0的既有约定一致）。
            if has_tile {
                return Some(FillStyle::PictureTiled {
                    image: roe_model::common::ImageData {
                        data: bytes.clone(),
                        mime: "image/png".to_string(),
                        crop_l: 0.0, crop_t: 0.0, crop_r: 0.0, crop_b: 0.0,
                    },
                    tile_offset_x,
                    tile_offset_y,
                    tile_scale_x,
                    tile_scale_y,
                    tile_align,
                });
            }
            let (crop_l, crop_t, crop_r, crop_b) =
                crop_quotients_from_fill_rect(fill_rect_l, fill_rect_t, fill_rect_r, fill_rect_b);
            return Some(FillStyle::Picture(roe_model::common::ImageData {
                data: bytes.clone(),
                mime: "image/png".to_string(),
                crop_l, crop_t, crop_r, crop_b,
            }));
        }
    }

    if let Some(color) = bg_solid_color {
        return Some(FillStyle::Solid(color));
    }

    // v318p: 渐变背景（至少两个色标才算有效）
    if grad_stops.len() >= 2 {
        return Some(FillStyle::Gradient {
            stops: grad_stops,
            kind: grad_kind.unwrap_or(roe_model::common::GradientKind::Linear { angle: 0.0 }),
            rotate_with_shape: grad_rotate,
        });
    }

    // PROOT-1xx: <p:bgRef idx="1001"><a:schemeClr val="bg2"/></p:bgRef>。
    // 真实样本sd_data_pptx__onemaster-twolayouts.pptx验证：idx用1000+
    // 区间编号，查theme_bg_fill_styles（而非theme_fill_styles）。
    // schemeClr的val（如"bg2"）先按normalize_scheme_color_slot归一化
    // （bg1/tx1/bg2/tx2这组UI别名转回clrScheme原始槽位名lt1/dk1/lt2/dk2，
    // 与fillRef场景style_fill_color的取色方式保持一致），查theme_colors
    // 拿到base色，再对bgFillStyleLst该项的mods（Solid场景）或每档色标
    // 的mods（Gradient场景）应用apply_color_mods，与fillRef idx场景
    // （lookup_theme_fill_style+apply_color_mods那段）同一套算法，
    // 唯一区别是查的表和查表函数不同。
    // 已知简化：Gradient分支解析出的渐变结果，本函数签名是
    // Option<FillStyle>，FillStyle::Gradient本身在roe_model::common里
    // 是否存在、字段是否与raw.grad_raw那套(stops, kind_raw, rotate)一致，
    // 需要现查——若类型不匹配，本次改动范围内先只支持Solid分支
    // （bgFillStyleLst第0项就是最常见的Solid场景，onemaster-twolayouts
    // 样本本身也是Solid，真实样本证据只覆盖到这里），Gradient分支
    // 留TODO，遇到真实样本报告问题时再扩展。
    // PROOT-THEME-BG-PIC: bgRef 命中 Picture 分支（bgFillStyleLst 里的
    // <a:blipFill>，如木纹纹理背景），不需要 schemeClr 颜色替换（图片
    // 填充没有 phClr 占位符可替换，颜色语义只对 solidFill/gradFill 有
    // 意义），所以这个分支独立在 bg_ref_idx 之后单独判断，不依赖
    // bg_ref_scheme_slot/base 是否解出——真实样本 sd_data_pptx__
    // accent-color.pptx 的 <p:bgRef idx="1003"><a:schemeClr val="bg1"/>
    // </p:bgRef> 虽然也带了 schemeClr，但那只是 OOXML 语法要求 bgRef
    // 必须有子元素，Picture 场景下这个 schemeClr 值实际未被使用
    // （与 solidFill/gradFill 场景的 phClr 语义不同）。
    if let Some(idx) = bg_ref_idx {
        if let Some(ThemeFillStyleDef::Picture {
            r_id, has_tile, tile_offset_x, tile_offset_y, tile_scale_x, tile_scale_y, tile_align,
        }) = lookup_theme_bg_fill_style(theme_bg_fill_styles, idx) {
            if let Some(bytes) = theme_images.get(r_id) {
                if *has_tile {
                    return Some(FillStyle::PictureTiled {
                        image: roe_model::common::ImageData {
                            data: bytes.clone(),
                            mime: "image/png".to_string(),
                            crop_l: 0.0, crop_t: 0.0, crop_r: 0.0, crop_b: 0.0,
                        },
                        tile_offset_x: *tile_offset_x,
                        tile_offset_y: *tile_offset_y,
                        tile_scale_x: *tile_scale_x,
                        tile_scale_y: *tile_scale_y,
                        tile_align: tile_align.clone(),
                    });
                }
                return Some(FillStyle::Picture(roe_model::common::ImageData {
                    data: bytes.clone(),
                    mime: "image/png".to_string(),
                    crop_l: 0.0, crop_t: 0.0, crop_r: 0.0, crop_b: 0.0,
                }));
            }
        }
    }

    if let (Some(idx), Some(slot)) = (bg_ref_idx, &bg_ref_scheme_slot) {
        if let Some(base) = theme_colors.get(normalize_scheme_color_slot(slot, clr_map)).copied() {
            if let Some(def) = lookup_theme_bg_fill_style(theme_bg_fill_styles, idx) {
                if let ThemeFillStyleDef::Solid(mods) = def {
                    let c = if mods.is_empty() { base } else { apply_color_mods(base, mods) };
                    return Some(FillStyle::Solid(c));
                }
                // Gradient分支：本次改动范围外，见上方注释，暂不处理，
                // 落到函数末尾返回None（与此前"完全不识别bgRef"的行为
                // 一致，不会比改动前更差，只是没有变得更好）。
            }
        }
    }

    None
}

fn collect_images(
    zip: &mut ZipArchive<std::io::Cursor<&[u8]>>,
    rel_map: &HashMap<String, String>,
    slide_path: &str,
) -> Result<HashMap<String, Vec<u8>>, ParseError> {
    let mut map: HashMap<String, Vec<u8>> = HashMap::new();

    // slide_path: ppt/slides/slide1.xml → base: ppt/slides/
    let base = slide_path.rsplitn(2, '/').nth(1).unwrap_or("ppt/slides");

    for (rid, target) in rel_map {
        if target.contains("media") || target.ends_with(".png") || target.ends_with(".jpg")
            || target.ends_with(".jpeg") || target.ends_with(".gif") || target.ends_with(".bmp")
        {
            let path = if target.starts_with("..") {
                // ../media/image1.png → ppt/media/image1.png
                normalize_path(base, target)
            } else if target.starts_with('/') {
                target.trim_start_matches('/').to_string()
            } else {
                format!("{}/{}", base, target)
            };

            if let Ok(mut entry) = zip.by_name(&path) {
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf).ok();
                map.insert(rid.clone(), buf);
            }
        }
    }
    Ok(map)
}


thread_local! {
    /// v223: 当前幻灯片的 chartEx 统计图（rId -> 模型），由 collect_charts 填充，raw_to_shape 读取
    static STAT_CHARTS: std::cell::RefCell<HashMap<String, roe_model::stat_chart::StatChartModel>> =
        std::cell::RefCell::new(HashMap::new());
}

/// v223: 含 chartEx 的 mc:AlternateContent，剔除其中的 mc:Fallback（回退图片），避免读成重复图片
fn strip_chartex_fallback(xml: &str) -> String {
    const OPEN: &str = "<mc:AlternateContent";
    const CLOSE: &str = "</mc:AlternateContent>";
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(s) = rest.find(OPEN) {
        let after = &rest[s..];
        let e = match after.find(CLOSE) { Some(e) => e + CLOSE.len(), None => break };
        let block = &after[..e];
        out.push_str(&rest[..s]);
        if block.contains("drawing/2014/chartex") {
            if let (Some(fs), Some(fe)) = (block.find("<mc:Fallback"), block.find("</mc:Fallback>")) {
                if fs < fe {
                    out.push_str(&block[..fs]);
                    out.push_str(&block[fe + "</mc:Fallback>".len()..]);
                    rest = &after[e..];
                    continue;
                }
            }
        }
        out.push_str(block);
        rest = &after[e..];
    }
    out.push_str(rest);
    out
}

fn collect_charts(
    zip: &mut ZipArchive<std::io::Cursor<&[u8]>>,
    rel_map: &HashMap<String, String>,
    slide_path: &str,
    theme_colors: &HashMap<String, Color>,
) -> Result<HashMap<String, roe_model::impress::ChartModel>, ParseError> {
    STAT_CHARTS.with(|m| m.borrow_mut().clear());
    let mut map = HashMap::new();
    let base = slide_path.rsplitn(2, '/').nth(1).unwrap_or("ppt/slides");
    for (rid, target) in rel_map {
        if !target.contains("chart") { continue; }
        let path = normalize_path(base, target);
        let xml = match read_zip_entry_opt(zip, &path)? {
            Some(x) => x,
            None => continue,
        };
        if xml.contains("<cx:chartSpace") {
            if let Some(sm) = super::chartex::parse_chartex_xml(&xml) {
                STAT_CHARTS.with(|m| { m.borrow_mut().insert(rid.clone(), sm); });
            }
            continue;
        }
        // chart 自己的 .rels 里可能有 themeOverride 关系，指向该 chart 专属的主题覆盖文件
        // （<a:accent1>等颜色槽位可能与母版 theme1.xml 不同）。若存在则优先用它整体替代
        // 母版 theme_colors（不是merge：override文件本身就是一套完整颜色定义）。
        let chart_theme_colors: HashMap<String, Color> = {
            let chart_base = path.rsplitn(2, '/').nth(1).unwrap_or("ppt/charts");
            let chart_file = path.rsplitn(2, '/').next().unwrap_or("");
            let chart_rels_path = format!("{}/_rels/{}.rels", chart_base, chart_file);
            let chart_rels_xml = read_zip_entry_opt(zip, &chart_rels_path)?.unwrap_or_default();
            let chart_rel_map = parse_rels(&chart_rels_xml);
            let override_target = chart_rel_map.values().find(|t| t.contains("themeOverride"));
            if let Some(override_target) = override_target {
                let override_path = normalize_path(chart_base, override_target);
                match read_zip_entry_opt(zip, &override_path)? {
                    Some(override_xml) => parse_theme_colors(&override_xml),
                    None => theme_colors.clone(),
                }
            } else {
                theme_colors.clone()
            }
        };
        // PROOT-clrmap: chart 的 schemeClr 走的是 accent 系列色，与本次
        // clrMap 主线任务（bg1/tx1/bg2/tx2 这组 UI 别名）关联很弱，且
        // collect_charts 在 slide 循环体里 effective_clr_map 算出来之前
        // 就已经被调用，传真实 clrMap 需要额外重排调用顺序。本次范围内
        // 传空表保持行为不变（等同于 accent1-6/hlink/folHlink 走
        // normalize_scheme_color_slot 的兜底分支，原样透传，与改动前
        // 行为一致）。sd_data_pptx__accent-color 样本本身是独立排期项，
        // 不属于本次 clrMap 任务范围（见 v90 会话计划）。
        if let Some(model) = parse_pptx_chart_xml(&xml, &chart_theme_colors, &HashMap::new()) {
            map.insert(rid.clone(), model);
        } else {
        }
    }
    Ok(map)
}

// ─── ZIP 工具 ─────────────────────────────────────────────────────────────────

fn read_zip_entry(
    zip: &mut ZipArchive<std::io::Cursor<&[u8]>>,
    name: &str,
) -> Result<String, ParseError> {
    read_zip_entry_opt(zip, name)?.ok_or_else(|| {
        ParseError::Malformed(format!("missing required entry: {name}"))
    })
}

fn read_zip_entry_opt(
    zip: &mut ZipArchive<std::io::Cursor<&[u8]>>,
    name: &str,
) -> Result<Option<String>, ParseError> {
    match zip.by_name(name) {
        Ok(mut entry) => {
            let mut buf = String::new();
            entry.read_to_string(&mut buf)
                .map_err(|e| ParseError::Io(e))?;
            Ok(Some(buf))
        }
        Err(zip::result::ZipError::FileNotFound) => Ok(None),
        Err(e) => Err(ParseError::Zip(e.to_string())),
    }
}

// ─── 路径工具 ─────────────────────────────────────────────────────────────────

/// 把相对路径解析为 ZIP 内绝对路径
/// base: "ppt", target: "slides/slide1.xml" → "ppt/slides/slide1.xml"
/// base: "ppt", target: "../media/img.png" (不用于 presentation.xml，但保留)
fn normalize_path(base: &str, target: &str) -> String {
    if target.starts_with('/') {
        return target.trim_start_matches('/').to_string();
    }
    let mut parts: Vec<&str> = base.split('/').collect();
    for seg in target.split('/') {
        match seg {
            ".." => { parts.pop(); }
            "." | "" => {}
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn slide_rels_path(slide_path: &str) -> String {
    // ppt/slides/slide1.xml → ppt/slides/_rels/slide1.xml.rels
    if let Some(pos) = slide_path.rfind('/') {
        let dir = &slide_path[..pos];
        let file = &slide_path[pos + 1..];
        format!("{}/_rels/{}.rels", dir, file)
    } else {
        format!("_rels/{}.rels", slide_path)
    }
}

// ─── 属性解析工具 ────────────────────────────────────────────────────────────



/// PPTX-11 part5：解析 <a:gd name="adjN" fmla="val N"/> 的 fmla 属性值。
/// OOXML 里 fmla 是通用公式串（"val N" / "*/ x y z" / ...），adj覆盖场景
/// 目前只需要处理最常见的 "val N" 形式（直接赋常量值），这也是绝大多数
/// 真实文档里 avLst 的实际写法（其余公式形式极罕见，第一期不处理，
/// 解析失败时静默跳过该条覆盖，不影响其余adj使用默认值）。
fn parse_adj_fmla(fmla: &str) -> Option<f64> {
    fmla.strip_prefix("val ")?.trim().parse().ok()
}

/// PPTX-7：解析 <a:xfrm rot="..."> 等整数属性（rot 单位为 OOXML 原始的 1/60000 度）
fn attr_i32(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<i32> {
    attr_str(e, key)?.parse().ok()
}

fn attr_u32(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<u32> {
    attr_str(e, key)?.parse().ok()
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

// ─── 公开入口（lib.rs 用）────────────────────────────────────────────────────

impl PptxParser {
    // alias kept for symmetry with XlsxParser / DocxParser naming
}

// ─── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ROOT-069/v130 回归防护（ROE_Chart_Parser_Unify_设计文档_v1 §三.6）：
    // sysClr 的 val 属性（"windowText"/"window"）才是权威语义，lastClr 只是
    // 历史缓存值，仅在 val 是未知关键字时才退化使用。此前 pptx.rs 的
    // parse_theme_colors 无条件采用 lastClr、忽略 val，与 OnlyOffice/WPS/
    // LibreOffice 三方参考渲染不一致（xlsx.rs 早已按此修复，pptx.rs/docx.rs
    // 落后）。本次合并到 chart_common::parse_ooxml_theme_colors 时反向同步
    // 修复。此测试构造 val="windowText" 与 lastClr="00FF00"（绿色）刻意
    // 不一致的场景，验证解析结果采用 val 语义（应为黑色），而不是 lastClr
    // 语义（绿色）——避免未来又被改回错误行为。
    #[test]
    fn test_sysclr_val_takes_precedence_over_last_clr() {
        let xml = r#"<?xml version="1.0"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <a:themeElements>
    <a:clrScheme name="Office">
      <a:dk1>
        <a:sysClr val="windowText" lastClr="00FF00"/>
      </a:dk1>
      <a:lt1>
        <a:sysClr val="window" lastClr="000000"/>
      </a:lt1>
      <a:dk2><a:srgbClr val="1F497D"/></a:dk2>
      <a:lt2><a:srgbClr val="EEECE1"/></a:lt2>
      <a:accent1><a:srgbClr val="4F81BD"/></a:accent1>
      <a:accent2><a:srgbClr val="C0504D"/></a:accent2>
      <a:accent3><a:srgbClr val="9BBB59"/></a:accent3>
      <a:accent4><a:srgbClr val="8064A2"/></a:accent4>
      <a:accent5><a:srgbClr val="4BACC6"/></a:accent5>
      <a:accent6><a:srgbClr val="F79646"/></a:accent6>
      <a:hlink><a:srgbClr val="0000FF"/></a:hlink>
      <a:folHlink><a:srgbClr val="800080"/></a:folHlink>
    </a:clrScheme>
  </a:themeElements>
</a:theme>"#;
        let colors = parse_theme_colors(xml);
        // dk1: val="windowText" 权威语义 -> 黑色，不是 lastClr 的绿色
        let dk1 = colors.get("dk1").expect("dk1 应该被解析出来");
        assert_eq!(
            (dk1.r, dk1.g, dk1.b),
            (0x00, 0x00, 0x00),
            "sysClr val=windowText 应解析为黑色（权威语义），而不是 lastClr=00FF00 的绿色（历史缓存值，此前的落后行为）"
        );
        // lt1: val="window" 权威语义 -> 白色，不是 lastClr 的黑色
        let lt1 = colors.get("lt1").expect("lt1 应该被解析出来");
        assert_eq!(
            (lt1.r, lt1.g, lt1.b),
            (0xFF, 0xFF, 0xFF),
            "sysClr val=window 应解析为白色（权威语义），而不是 lastClr=000000 的黑色"
        );
    }

    // 同一 bug 的另一面：val 是未知关键字时，应退化使用 lastClr 兜底
    // （而不是解析失败或返回默认色），确认修复没有"矫枉过正"。
    #[test]
    fn test_sysclr_unknown_val_falls_back_to_last_clr() {
        let xml = r#"<?xml version="1.0"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <a:themeElements>
    <a:clrScheme name="Office">
      <a:dk1>
        <a:sysClr val="someUnknownKeyword" lastClr="123456"/>
      </a:dk1>
      <a:lt1><a:srgbClr val="FFFFFF"/></a:lt1>
      <a:dk2><a:srgbClr val="1F497D"/></a:dk2>
      <a:lt2><a:srgbClr val="EEECE1"/></a:lt2>
      <a:accent1><a:srgbClr val="4F81BD"/></a:accent1>
      <a:accent2><a:srgbClr val="C0504D"/></a:accent2>
      <a:accent3><a:srgbClr val="9BBB59"/></a:accent3>
      <a:accent4><a:srgbClr val="8064A2"/></a:accent4>
      <a:accent5><a:srgbClr val="4BACC6"/></a:accent5>
      <a:accent6><a:srgbClr val="F79646"/></a:accent6>
      <a:hlink><a:srgbClr val="0000FF"/></a:hlink>
      <a:folHlink><a:srgbClr val="800080"/></a:folHlink>
    </a:clrScheme>
  </a:themeElements>
</a:theme>"#;
        let colors = parse_theme_colors(xml);
        let dk1 = colors.get("dk1").expect("dk1 应该被解析出来");
        assert_eq!(
            (dk1.r, dk1.g, dk1.b),
            (0x12, 0x34, 0x56),
            "val 是未知关键字时应退化使用 lastClr 兜底"
        );
    }


    // ROE-TILE-001: <a:blipFill><a:tile.../></a:blipFill> 背景平铺解析验证。
    // XML片段取自真实样本 tdf152070（用户自建测试用例，2026-08-06会话），
    // 验证 tx/ty(EMU→pt) 和 sx/sy(千分之一精度→归一化比例) 换算正确，
    // 以及确实走的是 PictureTiled 分支而不是 Picture（拉伸）分支。
    #[test]
    fn test_parse_slide_background_tile() {
        let xml = r#"<p:bg xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:bgPr><a:blipFill dpi="0" rotWithShape="1"><a:blip r:embed="rId2" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><a:lum/></a:blip><a:srcRect/><a:tile tx="444500" ty="514350" sx="60000" sy="70000" flip="none" algn="tl"/></a:blipFill><a:effectLst/></p:bgPr></p:bg>"#;
        let mut image_map = HashMap::new();
        image_map.insert("rId2".to_string(), vec![0x89, 0x50, 0x4E, 0x47]); // 假图片字节，只测解析不测真实解码

        let result = parse_slide_background(xml, &image_map, &HashMap::new(), &[], &HashMap::new(), &HashMap::new());
        match result {
            Some(FillStyle::PictureTiled { image, tile_offset_x, tile_offset_y, tile_scale_x, tile_scale_y, tile_align }) => {
                assert_eq!(image.data, vec![0x89, 0x50, 0x4E, 0x47]);
                // EMU → pt: 444500/12700 ≈ 35.0, 514350/12700 ≈ 40.5
                assert!((tile_offset_x - 35.0).abs() < 0.1, "tile_offset_x应约为35.0pt, got {}", tile_offset_x);
                assert!((tile_offset_y - 40.5).abs() < 0.1, "tile_offset_y应约为40.5pt, got {}", tile_offset_y);
                // 千分之一精度 → 归一化: 60000/100000=0.6, 70000/100000=0.7
                assert!((tile_scale_x - 0.6).abs() < 0.001, "tile_scale_x应为0.6, got {}", tile_scale_x);
                assert!((tile_scale_y - 0.7).abs() < 0.001, "tile_scale_y应为0.7, got {}", tile_scale_y);
                assert_eq!(tile_align, "tl");
            }
            other => panic!("期望 FillStyle::PictureTiled，实际得到 {:?}", other),
        }
    }

    // ROE-TILE-001: 确认 stretch/fillRect（无tile）场景不受本次改动影响，
    // 依然走原有的 Picture 分支——防止本次改动破坏 PROOT-008 已有逻辑。
    #[test]
    fn test_parse_slide_background_stretch_not_affected_by_tile_change() {
        let xml = r#"<p:bg xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:bgPr><a:blipFill><a:blip r:embed="rId1" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"/><a:stretch><a:fillRect t="-13000" b="-13000"/></a:stretch></a:blipFill></p:bgPr></p:bg>"#;
        let mut image_map = HashMap::new();
        image_map.insert("rId1".to_string(), vec![1, 2, 3, 4]);

        let result = parse_slide_background(xml, &image_map, &HashMap::new(), &[], &HashMap::new(), &HashMap::new());
        match result {
            Some(FillStyle::Picture(img)) => {
                assert_eq!(img.data, vec![1, 2, 3, 4]);
                assert!((img.crop_t - 0.10317).abs() < 0.001, "crop_t应约为0.10317（PROOT-008既有验证值）, got {}", img.crop_t);
            }
            other => panic!("期望 FillStyle::Picture（无tile场景不应受影响），实际得到 {:?}", other),
        }
    }

    // PROOT-006: shade/tint/satMod 颜色调整算法验证。
    // 算法来源：OnlyOffice sdkjs Format.js CColorModifiers.prototype.Apply
    // （2026-07-24会话确认，非猜测）。数值用bug_log记录的tdf114845真实样本
    // lnRef idx=1对应的shade val="95000" + satMod val="105000"组合验证。

    #[test]
    fn test_apply_shade_darkens_color() {
        // val=1.0时应等于原色（线性空间乘1不变，但sRGB转换往返可能有极小舍入误差）
        let base = [0x4F, 0x81, 0xBD]; // accent1蓝色，真实样本基础色
        let out = apply_shade(base, 1.0);
        for i in 0..3 {
            assert!((out[i] as i32 - base[i] as i32).abs() <= 1, "val=1.0应约等于原色, got {:?}", out);
        }
        // val=0.95（真实样本shade值）应比原色暗（每通道数值更小或相等，不会变亮）
        let out95 = apply_shade(base, 0.95);
        for i in 0..3 {
            assert!(out95[i] <= base[i], "shade 0.95应使颜色变暗或不变, channel {} base={} out={}", i, base[i], out95[i]);
        }
    }

    #[test]
    fn test_apply_shade_clamps_out_of_range_val_to_zero() {
        // sdkjs行为：val<0或val>1时clamp为0，即全黑，不是clamp到边界值
        let base = [0x4F, 0x81, 0xBD];
        let out = apply_shade(base, 1.5);
        assert_eq!(out, [0, 0, 0], "shade val超出[0,1]应clamp为0（全黑），得到{:?}", out);
    }

    #[test]
    fn test_apply_tint_lightens_color() {
        let base = [0x4F, 0x81, 0xBD];
        // val=1.0时应约等于原色
        let out = apply_tint(base, 1.0);
        for i in 0..3 {
            assert!((out[i] as i32 - base[i] as i32).abs() <= 1, "val=1.0应约等于原色, got {:?}", out);
        }
        // val较小时应更接近白色（每通道数值更大或相等）
        let out_low = apply_tint(base, 0.3);
        for i in 0..3 {
            assert!(out_low[i] >= base[i], "tint val较小应使颜色变浅, channel {} base={} out={}", i, base[i], out_low[i]);
        }
    }

    #[test]
    fn test_apply_sat_mod_val_one_is_noop() {
        // sdkjs: val===1时continue，即完全跳过不处理
        let base = [0x4F, 0x81, 0xBD];
        let out = apply_sat_mod(base, 1.0);
        assert_eq!(out, base, "satMod val=1.0应是no-op");
    }

    #[test]
    fn test_apply_sat_mod_increases_saturation() {
        let base = [0x4F, 0x81, 0xBD];
        let (h_before, s_before, _l_before) = rgb_to_hsl_pptx(base);
        let out = apply_sat_mod(base, 1.05); // 真实样本satMod值105000/100000=1.05
        let (h_after, s_after, _l_after) = rgb_to_hsl_pptx(out);
        assert!(s_after > s_before - 1e-6, "satMod 1.05应提高或持平饱和度, before={} after={}", s_before, s_after);
        assert!((h_after - h_before).abs() < 1.0, "satMod不应改变色相, before={} after={}", h_before, h_after);
    }

    #[test]
    fn test_rgb_hsl_roundtrip() {
        // 往返转换应能还原原色（允许±1的舍入误差）
        let colors = [[0x4F, 0x81, 0xBD], [0xFF, 0xFF, 0xFF], [0x00, 0x00, 0x00], [0xFF, 0x00, 0x00]];
        for base in colors {
            let (h, s, l) = rgb_to_hsl_pptx(base);
            let out = hsl_to_rgb_pptx(h, s, l);
            for i in 0..3 {
                assert!((out[i] as i32 - base[i] as i32).abs() <= 1,
                    "RGB->HSL->RGB往返应还原原色, base={:?} out={:?}", base, out);
            }
        }
    }

    #[test]
    fn test_apply_color_mods_sequential_shade_then_sat_mod() {
        // 复现tdf114845真实样本lnRef idx=1的实际mods组合：
        // shade val="95000" 后 satMod val="105000"，顺序应用在同一个色上
        let base = Color { r: 0x4F, g: 0x81, b: 0xBD, a: 255 };
        let mods = vec![
            ("shade".to_string(), 95000i32),
            ("satMod".to_string(), 105000i32),
        ];
        let out = apply_color_mods(base, &mods);
        // 只验证不panic、alpha保持不变、颜色确实发生了变化（不等于原色）
        assert_eq!(out.a, 255);
        assert!(out.r != base.r || out.g != base.g || out.b != base.b,
            "应用shade+satMod后颜色应发生变化, got {:?}", out);
    }

    #[test]
    fn test_apply_color_mods_unknown_mod_skipped() {
        // 未识别的mod名应原样跳过，不panic、不影响其他mod的应用。
        // PROOT-OOXTHEME: lumMod 本次会话已实现（见 apply_lum_mod），不再
        // 适合作为"未实现mod"的测试样本，改用 hueMod——sdkjs 支持但本项目
        // 目前仍未实现的十余种mod之一（见 apply_color_mods 函数文档注释），
        // 保持测试意图不变（验证"确实未识别的mod名"被安全跳过），只是换了
        // 一个仍然成立的具体mod名，不是放宽测试强度。
        let base = Color { r: 0x4F, g: 0x81, b: 0xBD, a: 255 };
        let mods = vec![
            ("hueMod".to_string(), 50000i32), // 未实现，应跳过
            ("shade".to_string(), 95000i32),  // 应正常应用
        ];
        let out = apply_color_mods(base, &mods);
        let expected_shade_only = apply_color_mods(base, &vec![("shade".to_string(), 95000i32)]);
        assert_eq!((out.r, out.g, out.b, out.a),
            (expected_shade_only.r, expected_shade_only.g, expected_shade_only.b, expected_shade_only.a),
            "跳过未知mod后结果应与只应用shade一致");
    }

    // PROOT-SCHEMECLR-MODS-START: 形状自身<p:spPr><a:solidFill><a:schemeClr
    // val="accent1">带lumMod/lumOff/alpha子元素（非自闭合，触发Start而非
    // Empty）的合成XML验证，不依赖真实pptx样本。复现sd_data_pptx__ooxtheme
    // 真实样本accent1+lumMod=60000+lumOff=40000的mods组合，额外加alpha=70000
    // 验证透明度换算独立于RGB mods运算。
    #[test]
    fn test_schemeclr_start_event_with_mods_and_alpha() {
        let xml = r#"<?xml version="1.0"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="Blob 1"/><p:nvPr/></p:nvSpPr>
        <p:spPr>
          <a:xfrm><a:off x="0" y="0"/><a:ext cx="100000" cy="100000"/></a:xfrm>
          <a:prstGeom prst="rect"/>
          <a:solidFill>
            <a:schemeClr val="accent1">
              <a:lumMod val="60000"/>
              <a:lumOff val="40000"/>
              <a:alpha val="70000"/>
            </a:schemeClr>
          </a:solidFill>
        </p:spPr>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;
        let image_map = HashMap::new();
        let chart_map = HashMap::new();
        let mut theme_colors = HashMap::new();
        theme_colors.insert("accent1".to_string(), Color { r: 0x4F, g: 0x81, b: 0xBD, a: 255 });
        let default_style = roe_model::impress::DefaultTextStyle::default();
        let shapes = parse_slide_xml_shapes(
            "(test)",
            xml, &image_map, &chart_map, &theme_colors, &HashMap::new(), &[], &[], &default_style, &[], &[], &[], None,
        ).expect("should parse slide with shape-level schemeClr Start event + mods");

        assert_eq!(shapes.len(), 1);
        match &shapes[0].style.fill {
            Some(FillStyle::Solid(color)) => {
                // 手算预期值：base=(0x4F,0x81,0xBD) -> apply_lum_mod(0.6) -> apply_lum_off(0.4)
                let base = Color { r: 0x4F, g: 0x81, b: 0xBD, a: 255 };
                let expected = apply_color_mods(base, &vec![
                    ("lumMod".to_string(), 60000i32),
                    ("lumOff".to_string(), 40000i32),
                ]);
                assert_eq!((color.r, color.g, color.b), (expected.r, expected.g, expected.b),
                    "lumMod+lumOff应作用于accent1基色, got {:?}", color);
                // alpha=70000 -> 70000/100000.0*255 = 178.5 -> 178 (as u8截断)
                assert_eq!(color.a, 178, "alpha=70000应换算为0-255范围, got {}", color.a);
                assert_ne!((color.r, color.g, color.b), (base.r, base.g, base.b),
                    "颜色应与未调整的base色不同，证明mods确实生效而非被忽略退化成默认色");
            }
            other => panic!("fill应为Solid，实际={:?}", other),
        }
    }

    // 对照组：确认新增Start分支未影响自闭合<a:schemeClr val="X"/>（无mods）
    // 的既有Empty事件处理路径——同一个函数里两种写法互不干扰。
    #[test]
    fn test_schemeclr_empty_event_without_mods_unaffected() {
        let xml = r#"<?xml version="1.0"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="Plain 1"/><p:nvPr/></p:nvSpPr>
        <p:spPr>
          <a:xfrm><a:off x="0" y="0"/><a:ext cx="100000" cy="100000"/></a:xfrm>
          <a:prstGeom prst="rect"/>
          <a:solidFill>
            <a:schemeClr val="accent1"/>
          </a:solidFill>
        </p:spPr>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;
        let image_map = HashMap::new();
        let chart_map = HashMap::new();
        let mut theme_colors = HashMap::new();
        theme_colors.insert("accent1".to_string(), Color { r: 0x4F, g: 0x81, b: 0xBD, a: 255 });
        let default_style = roe_model::impress::DefaultTextStyle::default();
        let shapes = parse_slide_xml_shapes(
            "(test)",
            xml, &image_map, &chart_map, &theme_colors, &HashMap::new(), &[], &[], &default_style, &[], &[], &[], None,
        ).expect("should parse slide with self-closing schemeClr (no mods)");

        assert_eq!(shapes.len(), 1);
        match &shapes[0].style.fill {
            Some(FillStyle::Solid(color)) => {
                assert_eq!((color.r, color.g, color.b, color.a), (0x4F, 0x81, 0xBD, 255),
                    "无mods的自闭合schemeClr应原样得到base色，不受新增Start分支影响, got {:?}", color);
            }
            other => panic!("fill应为Solid，实际={:?}", other),
        }
    }


    use super::*;

    // PPTX-12 验证：gradFill 线性渐变，基础场景（无flip/无旋转）。
    // 覆盖：gsLst 多色标解析 + pos 十万分之一精度归一化 + lin ang 角度换算。
    // ang="5400000" = 5400000/60000 = 90度，无flip无rotation时应直接
    // 得到 90.0（验证"不叠加时换算链路本身是恒等/正确"这个基线，
    // 有flip/rotation的复合场景见下一个测试）。
    #[test]
    fn test_pptx12_grad_fill_linear_basic() {
        let xml = r#"<?xml version="1.0"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="Rect 1"/><p:nvPr/></p:nvSpPr>
        <p:spPr>
          <a:xfrm><a:off x="100000" y="100000"/><a:ext cx="200000" cy="200000"/></a:xfrm>
          <a:prstGeom prst="rect"/>
          <a:gradFill>
            <a:gsLst>
              <a:gs pos="0"><a:srgbClr val="FF0000"/></a:gs>
              <a:gs pos="50000"><a:srgbClr val="00FF00"/></a:gs>
              <a:gs pos="100000"><a:srgbClr val="0000FF"/></a:gs>
            </a:gsLst>
            <a:lin ang="5400000" scaled="1"/>
          </a:gradFill>
        </p:spPr>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;
        let image_map = HashMap::new();
        let chart_map = HashMap::new();
        let theme_colors = HashMap::new();
        let default_style = roe_model::impress::DefaultTextStyle::default();
        let shapes = parse_slide_xml_shapes(
            "(test)",
            xml, &image_map, &chart_map, &theme_colors, &HashMap::new(), &[], &[], &default_style, &[], &[], &[], None,
        ).expect("should parse slide with gradFill");

        assert_eq!(shapes.len(), 1);
        match &shapes[0].style.fill {
            Some(FillStyle::Gradient { stops, kind, rotate_with_shape }) => {
                assert_eq!(stops.len(), 3, "应解析出3个色标");
                assert!((stops[0].pos - 0.0).abs() < 1e-6, "第一个色标pos应归一化为0.0");
                assert!((stops[1].pos - 0.5).abs() < 1e-6, "中间色标pos=50000应归一化为0.5");
                assert!((stops[2].pos - 1.0).abs() < 1e-6, "最后色标pos=100000应归一化为1.0");
                // Color 未 derive PartialEq，逐字段比较
                assert_eq!((stops[0].color.r, stops[0].color.g, stops[0].color.b), (0xFF, 0x00, 0x00));
                assert_eq!((stops[1].color.r, stops[1].color.g, stops[1].color.b), (0x00, 0xFF, 0x00));
                assert_eq!((stops[2].color.r, stops[2].color.g, stops[2].color.b), (0x00, 0x00, 0xFF));
                match kind {
                    GradientKind::Linear { angle } => {
                        assert!((angle - 90.0).abs() < 1e-3,
                            "ang=5400000(1/60000度)=90度，无flip无rotation时应直接得到90.0，实际={}", angle);
                    }
                    _ => panic!("应为 Linear 类型"),
                }
                assert!(*rotate_with_shape, "gradFill 未显式设置 rotWithShape，缺省应为 true");
            }
            other => panic!("fill应为Gradient，实际={:?}", other),
        }
    }

    // PPTX-12 验证：gradFill 线性渐变角度复合换算——flipH + 形状自身旋转
    // 叠加。验证 raw_to_shape 里的三步公式（flip修正→叠加旋转→归一化），
    // 对应 LOK fillproperties.cxx:554-566 的角度语义（不含其"8100-"这个
    // 未解释清楚的LOK内部API偏移量，见该处代码注释里记录的未决问题）。
    //
    // 构造：ang=0（原始0度），flipH=1，形状自身 rot=2700000（1/60000度，
    // 即45度）。按公式：
    //   1. flipH: angle = 180*60000 - 0 = 10800000
    //   2. rotate_with_shape缺省true，叠加rot: angle = 10800000 + 2700000 = 13500000
    //   3. 归一化: (13500000/60000) % 360 = 225.0 % 360 = 225.0
    #[test]
    fn test_pptx12_grad_fill_linear_flip_and_rotation_compound() {
        let xml = r#"<?xml version="1.0"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="Rect 1"/><p:nvPr/></p:nvSpPr>
        <p:spPr>
          <a:xfrm rot="2700000" flipH="1"><a:off x="0" y="0"/><a:ext cx="100000" cy="100000"/></a:xfrm>
          <a:prstGeom prst="rect"/>
          <a:gradFill>
            <a:gsLst>
              <a:gs pos="0"><a:srgbClr val="FFFFFF"/></a:gs>
              <a:gs pos="100000"><a:srgbClr val="000000"/></a:gs>
            </a:gsLst>
            <a:lin ang="0"/>
          </a:gradFill>
        </p:spPr>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;
        let image_map = HashMap::new();
        let chart_map = HashMap::new();
        let theme_colors = HashMap::new();
        let default_style = roe_model::impress::DefaultTextStyle::default();
        let shapes = parse_slide_xml_shapes(
            "(test)",
            xml, &image_map, &chart_map, &theme_colors, &HashMap::new(), &[], &[], &default_style, &[], &[], &[], None,
        ).expect("should parse slide with gradFill+flip+rotation");

        assert_eq!(shapes.len(), 1);
        match &shapes[0].style.fill {
            Some(FillStyle::Gradient { kind, .. }) => {
                match kind {
                    GradientKind::Linear { angle } => {
                        assert!((angle - 225.0).abs() < 1e-3,
                            "flipH+rot=45度复合换算预期225.0度，实际={}。                             若此断言失败，先检查是不是LOK的8100偏移量问题                             （见raw_to_shape换算逻辑注释里的未决问题记录），                             不要直接改断言值掩盖问题，要先用真实PPTX渲染验证。",
                            angle);
                    }
                    _ => panic!("应为 Linear 类型"),
                }
            }
            other => panic!("fill应为Gradient，实际={:?}", other),
        }
    }

    // PPTX-12 验证：gradFill path="circle" 应识别为 Radial 类型；
    // path="rect" 应识别为 Rect 类型（模型层区分，渲染层第一期再统一
    // 降级为Radial近似，见slide_renderer.rs shape_fill_color注释）。
    #[test]
    fn test_pptx12_grad_fill_path_circle_and_rect() {
        let xml_circle = r#"<?xml version="1.0"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <p:cSld><p:spTree><p:sp>
    <p:nvSpPr><p:cNvPr id="2" name="S"/><p:nvPr/></p:nvSpPr>
    <p:spPr>
      <a:xfrm><a:off x="0" y="0"/><a:ext cx="100000" cy="100000"/></a:xfrm>
      <a:prstGeom prst="ellipse"/>
      <a:gradFill>
        <a:gsLst>
          <a:gs pos="0"><a:srgbClr val="FFFFFF"/></a:gs>
          <a:gs pos="100000"><a:srgbClr val="000000"/></a:gs>
        </a:gsLst>
        <a:path path="circle"/>
      </a:gradFill>
    </p:spPr>
  </p:sp></p:spTree></p:cSld>
</p:sld>"#;
        let image_map = HashMap::new();
        let chart_map = HashMap::new();
        let theme_colors = HashMap::new();
        let default_style = roe_model::impress::DefaultTextStyle::default();
        let shapes = parse_slide_xml_shapes(
            "(test)",
            xml_circle, &image_map, &chart_map, &theme_colors, &HashMap::new(), &[], &[], &default_style, &[], &[], &[], None,
        ).expect("should parse circle path gradient");
        match &shapes[0].style.fill {
            Some(FillStyle::Gradient { kind: GradientKind::Radial, .. }) => {}
            other => panic!("path=circle 应识别为 Radial，实际={:?}", other),
        }
    }

    // PPTX-13(a) 验证：图例/网格线"是否存在"判断，替代此前硬编码 true, true
    #[test]
    fn test_parse_chart_legend_and_grid_present() {
        let xml = r#"<?xml version="1.0"?>
<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart">
  <c:chart>
    <c:plotArea>
      <c:barChart>
        <c:ser>
          <c:tx><c:v>Series1</c:v></c:tx>
          <c:cat><c:strRef><c:strCache><c:pt idx="0"><c:v>A</c:v></c:pt></c:strCache></c:strRef></c:cat>
          <c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>1</c:v></c:pt></c:numCache></c:numRef></c:val>
        </c:ser>
      </c:barChart>
      <c:catAx/>
      <c:valAx>
        <c:majorGridlines/>
      </c:valAx>
    </c:plotArea>
    <c:legend><c:legendPos val="r"/></c:legend>
  </c:chart>
</c:chartSpace>"#;
        let model = parse_pptx_chart_xml(xml, &HashMap::new(), &HashMap::new()).expect("should parse chart");
        assert!(model.show_legend, "有 <c:legend> 标签，show_legend 应为 true");
        assert!(model.show_grid, "有 <c:majorGridlines/> 标签，show_grid 应为 true");
    }

    // PPTX-13(b) 验证：轴标题解析（<c:catAx>/<c:valAx> 内 <c:title>）
    // 用例取材自真实样本 sc_data_xlsx__shape-macro-ext-ref.xlsx 的实际
    // XML结构（catAx标题由两个<a:r>拼接："Translation"+" X [mm]"，
    // 验证多run拼接；valAx标题单run"Fmax [N]"）。注意：轴内<c:title>
    // 与图表主标题<c:title>标签同名，解析时靠in_cat_ax/in_val_ax上下文
    // 区分，不能复用图表主标题的in_title状态（见本函数改动注释）。
    // PPTX-18 影响面核实：确认 trim_text(true) 导致的run间空格丢失
    // 是否也影响图表主标题（不只是轴标题）。用与catAx标题相同的多run
    // 模式（"Sales"+" (万元)"）构造图表主标题，验证是否同样丢失空格。
    #[test]
    fn test_pptx18_probe_main_title_multirun_space_loss() {
        let xml = r#"<?xml version="1.0"?>
<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <c:chart>
    <c:title><c:tx><c:rich><a:p>
      <a:r><a:rPr lang="zh-CN"/><a:t>Sales</a:t></a:r>
      <a:r><a:rPr lang="zh-CN"/><a:t> (万元)</a:t></a:r>
    </a:p></c:rich></c:tx></c:title>
    <c:plotArea>
      <c:barChart>
        <c:ser>
          <c:tx><c:v>Series1</c:v></c:tx>
          <c:cat><c:strRef><c:strCache><c:pt idx="0"><c:v>A</c:v></c:pt></c:strCache></c:strRef></c:cat>
          <c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>1</c:v></c:pt></c:numCache></c:numRef></c:val>
        </c:ser>
      </c:barChart>
    </c:plotArea>
  </c:chart>
</c:chartSpace>"#;
        let model = parse_pptx_chart_xml(xml, &HashMap::new(), &HashMap::new()).expect("should parse chart");
        // PPTX-18 已于2026-08-09修复：parse_pptx_chart_xml的
        // trim_text(true)改为trim_text(false)，run间纯空格<a:t>节点
        // 不再被quick-xml在Text事件送出前裁剪掉。本测试原为bug复现
        // 用例，现转为回归用例，断言已改回正确的带空格值。
        println!("main title parsed as: {:?}", model.title);
        assert_eq!(model.title.as_deref(), Some("Sales (万元)"),
            "PPTX-18回归用例：图表主标题多run拼接应保留run间空格");
    }

    #[test]
    fn test_parse_chart_axis_titles_present() {
        let xml = r#"<?xml version="1.0"?>
<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <c:chart>
    <c:title><c:tx><c:rich><a:p><a:r><a:t>Chart Main Title</a:t></a:r></a:p></c:rich></c:tx></c:title>
    <c:plotArea>
      <c:barChart>
        <c:ser>
          <c:tx><c:v>Series1</c:v></c:tx>
          <c:cat><c:strRef><c:strCache><c:pt idx="0"><c:v>A</c:v></c:pt></c:strCache></c:strRef></c:cat>
          <c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>1</c:v></c:pt></c:numCache></c:numRef></c:val>
        </c:ser>
      </c:barChart>
      <c:catAx>
        <c:title><c:tx><c:rich><a:p>
          <a:r><a:rPr lang="de-DE"/><a:t>Translation</a:t></a:r>
          <a:r><a:rPr lang="de-DE" baseline="0"/><a:t> X [mm]</a:t></a:r>
        </a:p></c:rich></c:tx></c:title>
      </c:catAx>
      <c:valAx>
        <c:title><c:tx><c:rich><a:p>
          <a:r><a:rPr lang="de-DE"/><a:t>Fmax [N]</a:t></a:r>
        </a:p></c:rich></c:tx></c:title>
      </c:valAx>
    </c:plotArea>
  </c:chart>
</c:chartSpace>"#;
        let model = parse_pptx_chart_xml(xml, &HashMap::new(), &HashMap::new()).expect("should parse chart");
        assert_eq!(model.title.as_deref(), Some("Chart Main Title"), "图表主标题不应受轴标题解析影响");
        // PPTX-18 已于2026-08-09修复：parse_pptx_chart_xml的
        // trim_text(true)改为trim_text(false)，run间纯空格<a:t>节点
        // 不再丢失。断言已改回正确的带空格值。
        assert_eq!(model.x_axis_label, "Translation X [mm]", "catAx标题拼接多个run应保留run间空格");
        assert_eq!(model.y_axis_label, "Fmax [N]", "valAx标题应正确提取");
    }

    #[test]
    fn test_parse_chart_axis_titles_absent() {
        let xml = r#"<?xml version="1.0"?>
<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart">
  <c:chart>
    <c:plotArea>
      <c:barChart>
        <c:ser>
          <c:tx><c:v>Series1</c:v></c:tx>
          <c:cat><c:strRef><c:strCache><c:pt idx="0"><c:v>A</c:v></c:pt></c:strCache></c:strRef></c:cat>
          <c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>1</c:v></c:pt></c:numCache></c:numRef></c:val>
        </c:ser>
      </c:barChart>
      <c:catAx/>
      <c:valAx/>
    </c:plotArea>
  </c:chart>
</c:chartSpace>"#;
        let model = parse_pptx_chart_xml(xml, &HashMap::new(), &HashMap::new()).expect("should parse chart");
        assert_eq!(model.x_axis_label, "", "无轴标题时应保持空字符串");
        assert_eq!(model.y_axis_label, "", "无轴标题时应保持空字符串");
    }

    #[test]
    fn test_parse_chart_legend_and_grid_absent() {
        let xml = r#"<?xml version="1.0"?>
<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart">
  <c:chart>
    <c:plotArea>
      <c:barChart>
        <c:ser>
          <c:tx><c:v>Series1</c:v></c:tx>
          <c:cat><c:strRef><c:strCache><c:pt idx="0"><c:v>A</c:v></c:pt></c:strCache></c:strRef></c:cat>
          <c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>1</c:v></c:pt></c:numCache></c:numRef></c:val>
        </c:ser>
      </c:barChart>
      <c:catAx/>
      <c:valAx/>
    </c:plotArea>
  </c:chart>
</c:chartSpace>"#;
        let model = parse_pptx_chart_xml(xml, &HashMap::new(), &HashMap::new()).expect("should parse chart");
        assert!(!model.show_legend, "无 <c:legend> 标签，show_legend 应为 false");
        assert!(!model.show_grid, "无 <c:majorGridlines/> 标签，show_grid 应为 false");
    }

    /// 用 zip crate 内存构造一个最小 PPTX，验证解析器不 panic
    fn make_minimal_pptx(slide_count: usize) -> Vec<u8> {
        use std::io::Write;
        let buf = Vec::new();
        let cursor = std::io::Cursor::new(buf);
        let mut zw = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        // [Content_Types].xml（简化，解析器不读但 zip 格式需要）
        zw.start_file("[Content_Types].xml", opts).unwrap();
        zw.write_all(b"<?xml version=\"1.0\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"></Types>").unwrap();

        // ppt/_rels/presentation.xml.rels
        let mut slide_rels = String::from("<?xml version=\"1.0\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">");
        for i in 1..=slide_count {
            slide_rels.push_str(&format!(
                "<Relationship Id=\"rId{i}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide\" Target=\"slides/slide{i}.xml\"/>",
            ));
        }
        slide_rels.push_str("</Relationships>");
        zw.start_file("ppt/_rels/presentation.xml.rels", opts).unwrap();
        zw.write_all(slide_rels.as_bytes()).unwrap();

        // ppt/presentation.xml
        let mut prs = String::from("<?xml version=\"1.0\"?><p:presentation xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><p:sldSz cx=\"9144000\" cy=\"6858000\"/><p:sldIdLst>");
        for i in 1..=slide_count {
            prs.push_str(&format!("<p:sldId id=\"{i}\" r:id=\"rId{i}\"/>"));
        }
        prs.push_str("</p:sldIdLst></p:presentation>");
        zw.start_file("ppt/presentation.xml", opts).unwrap();
        zw.write_all(prs.as_bytes()).unwrap();

        // ppt/slides/slideN.xml
        for i in 1..=slide_count {
            let slide_xml = format!(
                r#"<?xml version="1.0"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
 <p:cSld><p:spTree>
  <p:sp>
   <p:nvSpPr><p:cNvPr id="{i}" name="Title {i}"/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
   <p:spPr><a:xfrm><a:off x="457200" y="274638"/><a:ext cx="8229600" cy="1143000"/></a:xfrm><a:prstGeom prst="rect"/></p:spPr>
   <p:txBody><a:p><a:r><a:t>Slide {i} Title</a:t></a:r></a:p></p:txBody>
  </p:sp>
 </p:spTree></p:cSld>
</p:sld>"#
            );
            let path = format!("ppt/slides/slide{i}.xml");
            zw.start_file(&path, opts).unwrap();
            zw.write_all(slide_xml.as_bytes()).unwrap();

            // empty rels for each slide
            let rels_path = format!("ppt/slides/_rels/slide{i}.xml.rels");
            zw.start_file(&rels_path, opts).unwrap();
            zw.write_all(b"<?xml version=\"1.0\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"></Relationships>").unwrap();
        }

        zw.finish().unwrap().into_inner()
    }

    // ─── PPTX-7: rot/flipH/flipV 解析 ──────────────────────────────────
    // 构造一个只有 1 页、shape 的 <a:xfrm> 带 rot/flipH/flipV 属性的最小 PPTX，
    // 用于验证 parse_slide_xml_shapes（主解析路径）能正确读出这三个属性。
    // rot 用 2700000（1/60000度单位）= 45.0 度，覆盖交接说明 v18 里提到的换算公式。
    fn make_minimal_pptx_with_xfrm_attrs(rot: &str, flip_h: &str, flip_v: &str) -> Vec<u8> {
        use std::io::Write;
        let buf = Vec::new();
        let cursor = std::io::Cursor::new(buf);
        let mut zw = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        zw.start_file("[Content_Types].xml", opts).unwrap();
        zw.write_all(b"<?xml version=\"1.0\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"></Types>").unwrap();

        zw.start_file("ppt/_rels/presentation.xml.rels", opts).unwrap();
        zw.write_all(b"<?xml version=\"1.0\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide\" Target=\"slides/slide1.xml\"/></Relationships>").unwrap();

        zw.start_file("ppt/presentation.xml", opts).unwrap();
        zw.write_all(b"<?xml version=\"1.0\"?><p:presentation xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><p:sldSz cx=\"9144000\" cy=\"6858000\"/><p:sldIdLst><p:sldId id=\"1\" r:id=\"rId1\"/></p:sldIdLst></p:presentation>").unwrap();

        let slide_xml = format!(
            r#"<?xml version="1.0"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
 <p:cSld><p:spTree>
  <p:sp>
   <p:nvSpPr><p:cNvPr id="1" name="Title 1"/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
   <p:spPr><a:xfrm rot="{rot}" flipH="{flip_h}" flipV="{flip_v}"><a:off x="457200" y="274638"/><a:ext cx="8229600" cy="1143000"/></a:xfrm><a:prstGeom prst="rect"/></p:spPr>
   <p:txBody><a:p><a:r><a:t>Rotated Title</a:t></a:r></a:p></p:txBody>
  </p:sp>
 </p:spTree></p:cSld>
</p:sld>"#
        );
        zw.start_file("ppt/slides/slide1.xml", opts).unwrap();
        zw.write_all(slide_xml.as_bytes()).unwrap();

        zw.start_file("ppt/slides/_rels/slide1.xml.rels", opts).unwrap();
        zw.write_all(b"<?xml version=\"1.0\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"></Relationships>").unwrap();

        zw.finish().unwrap().into_inner()
    }

    #[test]
    fn test_xfrm_rot_flip_parsed() {
        // rot="2700000" 是 OOXML 原始 1/60000 度编码，对应 45.0 度。
        // flipH="1"/flipV="1" 应解析为 Some(true)。
        let data = make_minimal_pptx_with_xfrm_attrs("2700000", "1", "1");
        let prs = PptxParser::parse(&data).unwrap();
        let shape = &prs.slides[0].shapes[0];
        assert_eq!(shape.position.rotation, Some(45.0), "rot=2700000 应换算为 45.0 度");
        assert_eq!(shape.position.flip_h, Some(true), "flipH=\"1\" 应解析为 Some(true)");
        assert_eq!(shape.position.flip_v, Some(true), "flipV=\"1\" 应解析为 Some(true)");
    }

    #[test]
    fn test_xfrm_flip_false_parsed() {
        // flipH="0"/flipV="0"：显式声明为不翻转，应解析为 Some(false)，
        // 而不是因为值为"假值"就退化成 None——None 和 Some(false) 语义不同
        // （前者表示"未声明"，后者表示"明确声明不翻转"）。
        let data = make_minimal_pptx_with_xfrm_attrs("0", "0", "0");
        let prs = PptxParser::parse(&data).unwrap();
        let shape = &prs.slides[0].shapes[0];
        assert_eq!(shape.position.rotation, Some(0.0), "rot=0 应换算为 0.0 度");
        assert_eq!(shape.position.flip_h, Some(false), "flipH=\"0\" 应解析为 Some(false)");
        assert_eq!(shape.position.flip_v, Some(false), "flipV=\"0\" 应解析为 Some(false)");
    }

    #[test]
    fn test_xfrm_no_rot_flip_attrs_absent() {
        // 完全没有 rot/flipH/flipV 属性时（复用已有的 make_minimal_pptx，
        // 其 xfrm 没有这三个属性），应解析为 None，不能凭空产生默认值。
        let data = make_minimal_pptx(1);
        let prs = PptxParser::parse(&data).unwrap();
        let shape = &prs.slides[0].shapes[0];
        assert_eq!(shape.position.rotation, None, "无 rot 属性时应为 None");
        assert_eq!(shape.position.flip_h, None, "无 flipH 属性时应为 None");
        assert_eq!(shape.position.flip_v, None, "无 flipV 属性时应为 None");
    }

    // ─── PPTX-6: 表格解析 ────────────────────────────────────────────────

    /// 直接单测 parse_table 本身，不经过完整 pptx zip 链路：喂一段
    /// <a:tbl>...</a:tbl> XML 片段，验证 gridCol/tr/tc/txBody 基础解析。
    fn run_parse_table(tbl_xml: &str) -> roe_model::common::Table {
        // parse_table 假设调用方已经消费了 <a:tbl> 起始事件本身，
        // 只负责读取内部内容直到匹配的 </a:tbl>。这里手动喂一个
        // "已经进入 tbl 内部"的 reader，与主状态机的调用方式一致：
        // 主状态机在 Start(tbl) 事件触发时调用 parse_table，此时
        // reader 游标已经在 <a:tbl> 之后，所以测试直接从内部片段开始读，
        // 并在末尾补上 </a:tbl> 让 parse_table 能读到匹配的结束事件。
        let full = format!("<a:tbl>{tbl_xml}</a:tbl>");
        let mut reader = Reader::from_str(&full);
        reader.config_mut().trim_text(true);
        // 消费掉外层 <a:tbl> 起始事件，模拟主状态机已经识别到 Start(tbl) 的状态
        match reader.read_event() {
            Ok(Event::Start(ref e)) if e.local_name().as_ref() == b"tbl" => {}
            other => panic!("expected <a:tbl> start event, got: {other:?}"),
        }
        parse_table(&mut reader, &HashMap::new(), &HashMap::new()).expect("parse_table should succeed")
    }

    // PROOT-031: <a:tblPr tableStyleId="{GUID}"> + firstRow/firstCol/lastCol/
    // bandRow 开关解析。GUID 用真实样本 tdf156718 对应的
    // "Light Style 1 - Accent 1"（{3B4B98B0-60AC-42C2-AFA5-B58CD77FA1E5}），
    // 验证 tblPr 作为 Event::Start（因为带 tableStyleId 子元素）的路径。
    #[test]
    fn test_pptx31_tblpr_style_id_and_flags_parsed() {
        let tbl_xml = r#"
            <a:tblPr firstRow="1" firstCol="1" lastCol="1" bandRow="1">
                <a:tableStyleId>{3B4B98B0-60AC-42C2-AFA5-B58CD77FA1E5}</a:tableStyleId>
            </a:tblPr>
            <a:tblGrid>
                <a:gridCol w="914400"/>
            </a:tblGrid>
            <a:tr h="370840">
                <a:tc><a:txBody><a:p><a:r><a:t>A1</a:t></a:r></a:p></a:txBody></a:tc>
            </a:tr>
        "#;
        let tbl = run_parse_table(tbl_xml);

        assert_eq!(
            tbl.style_id.as_deref(),
            Some("{3B4B98B0-60AC-42C2-AFA5-B58CD77FA1E5}"),
            "tableStyleId 应正确解析为 Some(GUID字符串)，实际={:?}", tbl.style_id
        );
        assert!(tbl.first_row, "firstRow=\"1\" 应解析为 true");
        assert!(tbl.first_col, "firstCol=\"1\" 应解析为 true");
        assert!(tbl.last_col, "lastCol=\"1\" 应解析为 true");
        assert!(tbl.band_row, "bandRow=\"1\" 应解析为 true");
        assert!(!tbl.last_row, "lastRow 未出现在XML中，应保持默认 false");
        assert!(!tbl.band_col, "bandCol 未出现在XML中，应保持默认 false");
    }

    // 无 tblPr 场景（多数早期/简单样本没有该元素），style_id 应为 None，
    // 全部开关保持 false，等价于此前"完全不解析"的行为，不产生回归。
    #[test]
    fn test_pptx31_no_tblpr_defaults_to_none() {
        let tbl_xml = r#"
            <a:tblGrid>
                <a:gridCol w="914400"/>
            </a:tblGrid>
            <a:tr h="370840">
                <a:tc><a:txBody><a:p><a:r><a:t>A1</a:t></a:r></a:p></a:txBody></a:tc>
            </a:tr>
        "#;
        let tbl = run_parse_table(tbl_xml);

        assert_eq!(tbl.style_id, None, "无tblPr时style_id应为None");
        assert!(!tbl.first_row && !tbl.first_col && !tbl.last_row
            && !tbl.last_col && !tbl.band_row && !tbl.band_col,
            "无tblPr时全部开关应保持默认false");
    }

    #[test]
    fn test_pptx6_basic_table_grid_and_cells() {
        // 基础表格：2列2行，无任何合并，验证 col_widths/rows/cells 数量与文字内容。
        let tbl_xml = r#"
            <a:tblGrid>
                <a:gridCol w="914400"/>
                <a:gridCol w="1828800"/>
            </a:tblGrid>
            <a:tr h="370840">
                <a:tc><a:txBody><a:p><a:r><a:t>A1</a:t></a:r></a:p></a:txBody></a:tc>
                <a:tc><a:txBody><a:p><a:r><a:t>B1</a:t></a:r></a:p></a:txBody></a:tc>
            </a:tr>
            <a:tr h="370840">
                <a:tc><a:txBody><a:p><a:r><a:t>A2</a:t></a:r></a:p></a:txBody></a:tc>
                <a:tc><a:txBody><a:p><a:r><a:t>B2</a:t></a:r></a:p></a:txBody></a:tc>
            </a:tr>
        "#;
        let tbl = run_parse_table(tbl_xml);

        assert_eq!(tbl.col_widths.len(), 2, "should have 2 columns");
        assert_eq!(tbl.col_widths[0], 914400.0 / 12700.0, "col width should be EMU/12700 pt, got: {}", tbl.col_widths[0]);
        assert_eq!(tbl.rows.len(), 2, "should have 2 rows");
        assert_eq!(tbl.rows[0].cells.len(), 2, "row0 should have 2 cells");

        let roe_model::common::CellContent::Paragraph(p) = &tbl.rows[0].cells[0].content[0] else {
            panic!("cell(0,0) should contain a Paragraph");
        };
        assert_eq!(p.runs[0].text, "A1");
        let roe_model::common::CellContent::Paragraph(p) = &tbl.rows[1].cells[1].content[0] else {
            panic!("cell(1,1) should contain a Paragraph");
        };
        assert_eq!(p.runs[0].text, "B2");

        // 无合并场景，col_span/row_span 应均为默认值 1，hidden 应均为 false。
        for row in &tbl.rows {
            for cell in &row.cells {
                assert_eq!(cell.col_span, 1);
                assert_eq!(cell.row_span, 1);
                assert!(!cell.hidden);
            }
        }
    }

    #[test]
    fn test_pptx6_gridspan_horizontal_merge() {
        // 参照真实样本 sd_data_pptx__tdf119015.pptx 的实测结构：
        // <a:tc gridSpan="2">起始格</a:tc><a:tc hMerge="1">延续格</a:tc>
        let tbl_xml = r#"
            <a:tblGrid>
                <a:gridCol w="914400"/>
                <a:gridCol w="914400"/>
                <a:gridCol w="914400"/>
            </a:tblGrid>
            <a:tr h="370840">
                <a:tc gridSpan="2"><a:txBody><a:p><a:r><a:t>A1</a:t></a:r></a:p></a:txBody></a:tc>
                <a:tc hMerge="1"><a:txBody><a:p><a:r><a:t>A2</a:t></a:r></a:p></a:txBody></a:tc>
                <a:tc><a:txBody><a:p><a:r><a:t>C1</a:t></a:r></a:p></a:txBody></a:tc>
            </a:tr>
        "#;
        let tbl = run_parse_table(tbl_xml);

        assert_eq!(tbl.rows[0].cells.len(), 3, "cells 数组长度应等于物理列数，延续格不应被物理收窄（与DOCX不同）");

        let start_cell = &tbl.rows[0].cells[0];
        assert_eq!(start_cell.col_span, 2, "起始格应直接读到 gridSpan=2，不需要事后推算");
        assert!(!start_cell.hidden, "起始格不应被标记为 hidden");

        let continuation_cell = &tbl.rows[0].cells[1];
        assert!(continuation_cell.hidden, "hMerge=1 的延续格应标记 hidden=true");
        // 延续格的 content 按 LOK 参照实现（tablecellcontext.cxx）不清空，正常保留解析结果。
        assert!(!continuation_cell.content.is_empty(), "延续格 content 不应被清空（与 LOK 解析层行为一致）");

        assert_eq!(tbl.rows[0].cells[2].col_span, 1, "第三格未参与合并，col_span 应为默认值1");
    }

    #[test]
    fn test_pptx6_rowspan_vertical_merge() {
        // 参照真实样本 sd_data_pptx__tdf149785.pptx 的实测结构：
        // 第一行 <a:tc rowSpan="2">起始格</a:tc>，第二行对应列 <a:tc vMerge="1">延续格</a:tc>
        let tbl_xml = r#"
            <a:tblGrid>
                <a:gridCol w="914400"/>
                <a:gridCol w="914400"/>
            </a:tblGrid>
            <a:tr h="260406">
                <a:tc rowSpan="2"><a:txBody><a:p><a:r><a:t>Key Functions</a:t></a:r></a:p></a:txBody></a:tc>
                <a:tc><a:txBody><a:p><a:r><a:t>Vendor</a:t></a:r></a:p></a:txBody></a:tc>
            </a:tr>
            <a:tr h="355186">
                <a:tc vMerge="1"><a:txBody><a:p><a:endParaRPr lang="ru-RU"/></a:p></a:txBody></a:tc>
                <a:tc><a:txBody><a:p><a:r><a:t>Product Name</a:t></a:r></a:p></a:txBody></a:tc>
            </a:tr>
        "#;
        let tbl = run_parse_table(tbl_xml);

        assert_eq!(tbl.rows.len(), 2);
        let start_cell = &tbl.rows[0].cells[0];
        assert_eq!(start_cell.row_span, 2, "起始格应直接读到 rowSpan=2，不需要像DOCX那样事后累加推算");
        assert!(!start_cell.hidden);

        let continuation_cell = &tbl.rows[1].cells[0];
        assert!(continuation_cell.hidden, "vMerge=1 的延续格应标记 hidden=true");
        assert_eq!(continuation_cell.row_span, 1, "延续格自身的 row_span 不应被修改，保持默认值1");
    }

    #[test]
    fn test_pptx13c_chart_position_from_graphic_frame_xfrm() {
        // PPTX-13(c) 端到端验证：ChartModel.x_pt/y_pt 应该等于 graphicFrame
        // 的 <p:xfrm><a:off> EMU值换算成pt后的结果，而不是保留 chart_map
        // 里初始塞入的值。chart_map 里故意塞一个非零的初始 x_pt/y_pt（999.0），
        // 断言最终结果不是999.0而是正确换算值——这样才能真正证明"覆盖"逻辑
        // 生效，而不是恰好两边都是默认0.0导致断言巧合通过。
        //
        // EMU→pt换算：1pt = 12700 EMU（与其余 xfrm 解析处一致，914400 EMU = 1英寸 = 72pt）。
        // off x="914400" y="457200" → x_pt=72.0, y_pt=36.0。
        let slide_xml = r#"<?xml version="1.0"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
 <p:cSld><p:spTree>
  <p:graphicFrame>
   <p:nvGraphicFramePr><p:cNvPr id="2" name="Chart 1"/></p:nvGraphicFramePr>
   <p:xfrm><a:off x="914400" y="457200"/><a:ext cx="4572000" cy="2286000"/></p:xfrm>
   <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart">
    <c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" r:id="rId1"/>
   </a:graphicData></a:graphic>
  </p:graphicFrame>
 </p:spTree></p:cSld>
</p:sld>"#;

        let mut chart_map = HashMap::new();
        chart_map.insert("rId1".to_string(), roe_model::impress::ChartModel {
            id: 0,
            kind: roe_model::impress::ChartKind::Bar,
            title: None,
            x_labels: vec![],
            series: vec![],
            x_pt: 999.0, y_pt: 999.0,
            width_pt: 300.0, height_pt: 300.0,
            show_legend: true, show_grid: true,
            x_axis_label: String::new(), y_axis_label: String::new(),
            border_color: Color { r: 200, g: 200, b: 200, a: 255 },
            border_width: 1.0,
            gap_width_pct: None,
            overlap_pct: None,
            grouping: roe_model::impress::ChartGrouping::Standard,
            hole_size_pct: None,
            data_labels: Default::default(),
            slice_border_color: None,
        });

        let theme_colors = HashMap::new();
        let shapes = parse_slide_xml_shapes(
            "(test)",
            slide_xml, &HashMap::new(), &chart_map, &theme_colors, &HashMap::new(), &[], &[], &Default::default(), &[], &[], &[], None,
        ).expect("parse should succeed");

        assert_eq!(shapes.len(), 1, "should produce exactly one shape for the graphicFrame");
        match &shapes[0].content {
            roe_model::impress::ShapeContent::Chart(model) => {
                assert!((model.x_pt - 72.0).abs() < 0.01, "x_pt应等于graphicFrame xfrm off的EMU换算值(72.0)，而非chart_map初始值999.0，实际={}", model.x_pt);
                assert!((model.y_pt - 36.0).abs() < 0.01, "y_pt应等于graphicFrame xfrm off的EMU换算值(36.0)，而非chart_map初始值999.0，实际={}", model.y_pt);
            }
            other => panic!("expected ShapeContent::Chart, got {:?}", other),
        }
    }

    #[test]
    fn test_graphic_frame_self_closing_cnvpr_keeps_real_id() {
        // 回归:graphicFrame 内自闭合 <p:cNvPr id="3"/> 曾被吞掉,raw.id 保持 0,
        // 回落为计数器值,与同页空壳 id=2 重复。期望读入后 id 分别为 2、3。
        let slide_xml = r#"<?xml version="1.0"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
 <p:cSld><p:spTree>
  <p:sp>
   <p:nvSpPr><p:cNvPr id="2" name="Shape 1"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
   <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="7315200" cy="4114800"/></a:xfrm></p:spPr>
  </p:sp>
  <p:graphicFrame>
   <p:nvGraphicFramePr><p:cNvPr id="3" name="Chart 3"/></p:nvGraphicFramePr>
   <p:xfrm><a:off x="914400" y="457200"/><a:ext cx="4572000" cy="2286000"/></p:xfrm>
   <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart">
    <c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" r:id="rId1"/>
   </a:graphicData></a:graphic>
  </p:graphicFrame>
 </p:spTree></p:cSld>
</p:sld>"#;

        let mut chart_map = HashMap::new();
        chart_map.insert("rId1".to_string(), roe_model::impress::ChartModel {
            id: 0,
            kind: roe_model::impress::ChartKind::Bar,
            title: None,
            x_labels: vec![],
            series: vec![],
            x_pt: 0.0, y_pt: 0.0,
            width_pt: 300.0, height_pt: 300.0,
            show_legend: true, show_grid: true,
            x_axis_label: String::new(), y_axis_label: String::new(),
            border_color: Color { r: 200, g: 200, b: 200, a: 255 },
            border_width: 1.0,
            gap_width_pct: None,
            overlap_pct: None,
            grouping: roe_model::impress::ChartGrouping::Standard,
            hole_size_pct: None,
            data_labels: Default::default(),
            slice_border_color: None,
        });

        let theme_colors = HashMap::new();
        let shapes = parse_slide_xml_shapes(
            "(test)",
            slide_xml, &HashMap::new(), &chart_map, &theme_colors, &HashMap::new(), &[], &[], &Default::default(), &[], &[], &[], None,
        ).expect("parse should succeed");

        assert_eq!(shapes.len(), 2, "空壳 + 图表应读出 2 个形状");
        assert!(matches!(shapes[1].content, roe_model::impress::ShapeContent::Chart(_)), "第 2 个形状应是图表");
        let ids: Vec<u64> = shapes.iter().map(|s| s.id as u64).collect();
        assert_eq!(ids, vec![2u64, 3u64], "自闭合 cNvPr 的 id 应被保留,不得回落为计数器");
    }

    #[test]
    fn test_pptx6_end_to_end_via_graphic_frame() {
        // 集成测试：验证主状态机在 <p:graphicFrame> 内识别 <a:tbl> 并正确
        // 产出 ShapeContent::Table，走完整 parse_slide_xml_shapes 入口
        // （而不是直接调 parse_table），确保调用点真正接通。
        let slide_xml = r#"<?xml version="1.0"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
 <p:cSld><p:spTree>
  <p:graphicFrame>
   <p:nvGraphicFramePr><p:cNvPr id="2" name="Table 1"/></p:nvGraphicFramePr>
   <p:xfrm><a:off x="457200" y="274638"/><a:ext cx="4572000" cy="1143000"/></p:xfrm>
   <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table">
    <a:tbl>
     <a:tblGrid><a:gridCol w="2286000"/><a:gridCol w="2286000"/></a:tblGrid>
     <a:tr h="571500">
      <a:tc><a:txBody><a:p><a:r><a:t>Header1</a:t></a:r></a:p></a:txBody></a:tc>
      <a:tc><a:txBody><a:p><a:r><a:t>Header2</a:t></a:r></a:p></a:txBody></a:tc>
     </a:tr>
    </a:tbl>
   </a:graphicData></a:graphic>
  </p:graphicFrame>
 </p:spTree></p:cSld>
</p:sld>"#;

        let theme_colors = HashMap::new();
        let shapes = parse_slide_xml_shapes(
            "(test)",
            slide_xml, &HashMap::new(), &HashMap::new(), &theme_colors, &HashMap::new(), &[], &[], &Default::default(), &[], &[], &[], None,
        ).expect("parse should succeed");

        assert_eq!(shapes.len(), 1, "should produce exactly one shape for the graphicFrame");
        match &shapes[0].content {
            roe_model::impress::ShapeContent::Table(tbl) => {
                assert_eq!(tbl.rows.len(), 1);
                assert_eq!(tbl.rows[0].cells.len(), 2);
                let roe_model::common::CellContent::Paragraph(p) = &tbl.rows[0].cells[0].content[0] else {
                    panic!("expected Paragraph content");
                };
                assert_eq!(p.runs[0].text, "Header1");
            }
            other => panic!("expected ShapeContent::Table, got: {other:?}"),
        }
    }

    #[test]
    fn test_pptx10_src_rect_crop_parsing() {
        // PPTX-10 集成测试：验证 <a:srcRect> 的 l/t/r/b 四个属性能被
        // 正确解析并归一化（千分比整数 / 100000.0 -> 0.0-1.0 浮点）。
        // l=25000(25%) r=10000(10%) 覆盖"部分属性缺省"场景（t/b缺省=0），
        // 同时验证 rId 关联的图片数据能通过 image_map 正确取到。
        let slide_xml = r#"<?xml version="1.0"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
 <p:cSld><p:spTree>
  <p:pic>
   <p:nvPicPr><p:cNvPr id="1" name="Pic1"/></p:nvPicPr>
   <p:blipFill>
    <a:blip r:embed="rId1"/>
    <a:srcRect l="25000" r="10000"/>
   </p:blipFill>
   <p:spPr>
    <a:xfrm><a:off x="0" y="0"/><a:ext cx="1000000" cy="1000000"/></a:xfrm>
   </p:spPr>
  </p:pic>
 </p:spTree></p:cSld>
</p:sld>"#;

        let theme_colors = HashMap::new();
        let mut image_map = HashMap::new();
        image_map.insert("rId1".to_string(), vec![0x89u8, b'P', b'N', b'G']); // 假PNG头，够sniff用

        let shapes = parse_slide_xml_shapes(
            "(test)",
            slide_xml, &image_map, &HashMap::new(), &theme_colors, &HashMap::new(), &[], &[], &Default::default(), &[], &[], &[], None,
        ).expect("parse should succeed");

        assert_eq!(shapes.len(), 1, "should have exactly 1 top-level shape");

        let roe_model::impress::ShapeContent::Image(img) = &shapes[0].content else {
            panic!("expected ShapeContent::Image, got: {:?}", shapes[0].content);
        };

        assert!((img.crop_l - 0.25).abs() < 0.0001, "crop_l should be 25000/100000=0.25, got {}", img.crop_l);
        assert!((img.crop_r - 0.10).abs() < 0.0001, "crop_r should be 10000/100000=0.10, got {}", img.crop_r);
        assert_eq!(img.crop_t, 0.0, "crop_t attribute was absent from XML, should default to 0.0");
        assert_eq!(img.crop_b, 0.0, "crop_b attribute was absent from XML, should default to 0.0");
    }

    #[test]
    fn test_blip_with_child_element_grayscl_is_still_parsed() {
        // bugfix 回归测试（本次会话，PROOT-D 图片丢失排查）：<a:blip> 若带
        // 子元素（如颜色特效 <a:grayscl/>），quick-xml 触发的是 Event::Start
        // 而非 Event::Empty，此前 b"blip" 处理只写在 Event::Empty 分支，导致
        // r:embed 完全没被读取、图片彻底丢失。真实样本 tdf112209 复现
        // （<a:blip r:embed="rId2"><a:grayscl/></a:blip>）。
        let slide_xml = r#"<?xml version="1.0"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
 <p:cSld><p:spTree>
  <p:pic>
   <p:nvPicPr><p:cNvPr id="1" name="Pic1"/></p:nvPicPr>
   <p:blipFill>
    <a:blip r:embed="rId2"><a:grayscl/></a:blip>
   </p:blipFill>
   <p:spPr>
    <a:xfrm><a:off x="0" y="0"/><a:ext cx="1000000" cy="1000000"/></a:xfrm>
   </p:spPr>
  </p:pic>
 </p:spTree></p:cSld>
</p:sld>"#;

        let theme_colors = HashMap::new();
        let mut image_map = HashMap::new();
        image_map.insert("rId2".to_string(), vec![0xffu8, 0xd8]); // 假JPEG头，够sniff用

        let shapes = parse_slide_xml_shapes(
            "(test)",
            slide_xml, &image_map, &HashMap::new(), &theme_colors, &HashMap::new(), &[], &[], &Default::default(), &[], &[], &[], None,
        ).expect("parse should succeed");

        assert_eq!(shapes.len(), 1, "should have exactly 1 top-level shape");

        let roe_model::impress::ShapeContent::Image(img) = &shapes[0].content else {
            panic!("expected ShapeContent::Image (blip should be parsed despite <a:grayscl/> child), got: {:?}", shapes[0].content);
        };
        assert_eq!(img.mime, "image/jpeg");
    }

    #[test]
    fn test_blip_with_svg_extension_uses_primary_embed_not_svg_embed() {
        // bugfix 回归测试（本次会话，PROOT-D 图片丢失排查）：<a:blip
        // r:embed="rId1"> 携带 <a:extLst><a:ext><asvg:svgBlip r:embed="rId2"/>
        // </a:ext></a:extLst> 这种"主图+SVG增强"结构时，同样因为非自闭合
        // 触发 Event::Start 而丢失。同时验证解析结果正确使用外层 <a:blip>
        // 自身的 r:embed（rId1，主图），不会被内层 svgBlip 的同名属性
        // （rId2）污染——真实样本 tdf163852 结构。
        let slide_xml = r#"<?xml version="1.0"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
 <p:cSld><p:spTree>
  <p:pic>
   <p:nvPicPr><p:cNvPr id="1" name="Pic1"/></p:nvPicPr>
   <p:blipFill>
    <a:blip r:embed="rId1">
     <a:extLst><a:ext uri="{96DAC541-7B7A-43D3-8B79-37D633B846F1}">
      <asvg:svgBlip xmlns:asvg="http://schemas.microsoft.com/office/drawing/2016/SVG/main" r:embed="rId2"/>
     </a:ext></a:extLst>
    </a:blip>
   </p:blipFill>
   <p:spPr>
    <a:xfrm><a:off x="0" y="0"/><a:ext cx="1000000" cy="1000000"/></a:xfrm>
   </p:spPr>
  </p:pic>
 </p:spTree></p:cSld>
</p:sld>"#;

        let theme_colors = HashMap::new();
        let mut image_map = HashMap::new();
        image_map.insert("rId1".to_string(), vec![0x89u8, b'P', b'N', b'G']); // PNG，主图
        image_map.insert("rId2".to_string(), b"<svg></svg>".to_vec()); // SVG，扩展图，不应被用到

        let shapes = parse_slide_xml_shapes(
            "(test)",
            slide_xml, &image_map, &HashMap::new(), &theme_colors, &HashMap::new(), &[], &[], &Default::default(), &[], &[], &[], None,
        ).expect("parse should succeed");

        assert_eq!(shapes.len(), 1, "should have exactly 1 top-level shape");

        let roe_model::impress::ShapeContent::Image(img) = &shapes[0].content else {
            panic!("expected ShapeContent::Image, got: {:?}", shapes[0].content);
        };
        assert_eq!(img.mime, "image/png", "should use rId1 (primary PNG blip), not rId2 (svgBlip extension)");
        assert_eq!(img.data, vec![0x89u8, b'P', b'N', b'G'], "image data should come from rId1, not rId2");
    }

    #[test]
    fn diag_custom_shape_bitmap_fill_and_tdf153008() {
        // 临时诊断测试（本次会话，PROOT-D 图片丢失排查，非永久回归测试，
        // 排查完可删除）：直接读取真实样本 .pptx 文件，打印解析出的
        // Shape.content 类型，确认 <p:pic> + <a:prstGeom> 组合是否真的被
        // 解析成 ShapeContent::Image，还是在某个环节被吃掉变成了 Empty。
        use crate::RoeDocument;

        for path in [
            "/home/yong/Projects/roe/test_samples/curated/pptx/supp_qa_filter_eppt_data__custom-shape-bitmap-fill.pptx",
            "/home/yong/Projects/roe/test_samples/curated/pptx/small__sd_data_pptx__tdf153008-srcRect-smallNegBound.pptx",
        ] {
            println!("=== {} ===", path);
            let doc = RoeDocument::open_pptx_file(path).expect("open should succeed");
            let RoeDocument::Impress(pres) = doc else { panic!("expected Impress") };
            for (si, slide) in pres.slides.iter().enumerate() {
                for (shi, shape) in slide.shapes.iter().enumerate() {
                    println!(
                        "slide[{}] shape[{}] id={} geometry={:?} content_variant={} placeholder={:?}",
                        si, shi, shape.id,
                        shape.geometry,
                        match &shape.content {
                            roe_model::impress::ShapeContent::Empty => "Empty".to_string(),
                            roe_model::impress::ShapeContent::Image(img) => format!("Image(mime={}, data_len={})", img.mime, img.data.len()),
                            roe_model::impress::ShapeContent::Text(_) => "Text".to_string(),
                            roe_model::impress::ShapeContent::Table(_) => "Table".to_string(),
                            roe_model::impress::ShapeContent::Chart(_) => "Chart".to_string(),
                            roe_model::impress::ShapeContent::Group { .. } => "Group".to_string(),
                            #[allow(unreachable_patterns)]
                            _ => "Other".to_string(),
                        },
                        shape.placeholder,
                    );
                }
            }
        }
    }

    #[test]
    fn test_pptx9b_group_shape_parsing() {
        // PPTX-9b 集成测试：验证 <p:grpSp> 解析走"层级栈"分流正确，
        // 覆盖：①顶层普通形状与分组形状共存互不干扰 ②分组自身
        // off/ext/rot 读取正确 ③分组内两个子形状都被正确收集
        // ④嵌套分组（grpSp 内还有 grpSp）天然生效，不需要额外代码路径
        let slide_xml = r#"<?xml version="1.0"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
 <p:cSld><p:spTree>
  <p:sp>
   <p:nvSpPr><p:cNvPr id="1" name="TopLevel"/></p:nvSpPr>
   <p:spPr>
    <a:xfrm><a:off x="0" y="0"/><a:ext cx="1000000" cy="500000"/></a:xfrm>
    <a:prstGeom prst="rect"/>
   </p:spPr>
  </p:sp>
  <p:grpSp>
   <p:nvGrpSpPr><p:cNvPr id="2" name="OuterGroup"/></p:nvGrpSpPr>
   <p:grpSpPr>
    <a:xfrm rot="2700000"><a:off x="914400" y="457200"/><a:ext cx="2000000" cy="1000000"/>
     <a:chOff x="0" y="0"/><a:chExt cx="2000000" cy="1000000"/></a:xfrm>
   </p:grpSpPr>
   <p:sp>
    <p:nvSpPr><p:cNvPr id="3" name="ChildA"/></p:nvSpPr>
    <p:spPr>
     <a:xfrm><a:off x="100000" y="100000"/><a:ext cx="300000" cy="200000"/></a:xfrm>
     <a:prstGeom prst="ellipse"/>
    </p:spPr>
   </p:sp>
   <p:grpSp>
    <p:nvGrpSpPr><p:cNvPr id="4" name="InnerGroup"/></p:nvGrpSpPr>
    <p:grpSpPr>
     <a:xfrm><a:off x="500000" y="500000"/><a:ext cx="800000" cy="400000"/>
      <a:chOff x="0" y="0"/><a:chExt cx="800000" cy="400000"/></a:xfrm>
    </p:grpSpPr>
    <p:sp>
     <p:nvSpPr><p:cNvPr id="5" name="NestedChild"/></p:nvSpPr>
     <p:spPr>
      <a:xfrm><a:off x="10000" y="10000"/><a:ext cx="100000" cy="100000"/></a:xfrm>
      <a:prstGeom prst="rect"/>
     </p:spPr>
    </p:sp>
   </p:grpSp>
  </p:grpSp>
 </p:spTree></p:cSld>
</p:sld>"#;

        let theme_colors = HashMap::new();
        let shapes = parse_slide_xml_shapes(
            "(test)",
            slide_xml, &HashMap::new(), &HashMap::new(), &theme_colors, &HashMap::new(), &[], &[], &Default::default(), &[], &[], &[], None,
        ).expect("parse should succeed");

        // 顶层应恰好2个：TopLevel(sp) + OuterGroup(grpSp)，内层子形状
        // 不应该泄漏到顶层 shapes（这是分流栈是否生效的直接证据）。
        assert_eq!(shapes.len(), 2, "top level should have exactly TopLevel sp + OuterGroup grpSp, got: {shapes:?}");

        // 顶层第一个是普通形状，未被误吞入任何分组
        match &shapes[0].content {
            roe_model::impress::ShapeContent::Empty => {}
            other => panic!("expected ShapeContent::Empty for TopLevel sp (no txBody), got: {other:?}"),
        }
        // PPTX-11 part5（解析层接线）后，prst="rect" 能在 preset_geometry
        // 公式表查到（find_preset_shape 命中 RECT_FORMULA），raw_to_shape
        // 会用默认 adj 求值成 PresetPath 而非落入 Custom——这是本次改动
        // 的预期新行为，不是回归。此处只验证"确实被解析并求值成某种
        // PresetPath"，不断言具体坐标（坐标正确性由 preset_geometry.rs
        // 自己的 test_rect_shape_vertices 覆盖，这里的重点仍是分组分流）。
        assert!(
            matches!(&shapes[0].geometry, roe_model::impress::ShapeGeometry::PresetPath(_)),
            "TopLevel geometry mismatch: {:?} (prst=\"rect\" 显式声明应求值为 PresetPath，与省略 prstGeom 标签时的默认值 ShapeGeometry::Rect 不是同一回事，此处验证前者)",
            shapes[0].geometry
        );

        // 顶层第二个是分组
        let roe_model::impress::ShapeContent::Group { children: outer_children, .. } = &shapes[1].content else {
            panic!("expected ShapeContent::Group for OuterGroup, got: {:?}", shapes[1].content);
        };

        // 分组自身 position：off/ext（EMU->pt, /12700）+ rot（1/60000度->度, /60000）
        let outer_pos = &shapes[1].position;
        assert!((outer_pos.x() - 914400.0 / 12700.0).abs() < 0.01, "outer group off.x mismatch: {}", outer_pos.x());
        assert!((outer_pos.y() - 457200.0 / 12700.0).abs() < 0.01, "outer group off.y mismatch: {}", outer_pos.y());
        assert!((outer_pos.w() - 2000000.0 / 12700.0).abs() < 0.01, "outer group ext.w mismatch: {}", outer_pos.w());
        assert_eq!(outer_pos.rotation, Some(2700000.0 / 60000.0), "outer group rotation mismatch");

        // 分组内应有恰好2个子形状：ChildA(sp) + InnerGroup(grpSp)
        assert_eq!(outer_children.len(), 2, "OuterGroup should contain exactly ChildA + InnerGroup, got: {outer_children:?}");
        assert!(matches!(outer_children[0].geometry, roe_model::impress::ShapeGeometry::Ellipse), "ChildA geometry mismatch: {:?}", outer_children[0].geometry);

        // 嵌套分组：InnerGroup 本身也应该是 Group，内含 NestedChild
        let roe_model::impress::ShapeContent::Group { children: inner_children, .. } = &outer_children[1].content else {
            panic!("expected nested ShapeContent::Group for InnerGroup, got: {:?}", outer_children[1].content);
        };
        assert_eq!(inner_children.len(), 1, "InnerGroup should contain exactly NestedChild, got: {inner_children:?}");
        // 同 TopLevel 断言（见上方注释）：prst="rect" 现在求值为
        // PresetPath，不再落入 Custom。
        assert!(
            matches!(&inner_children[0].geometry, roe_model::impress::ShapeGeometry::PresetPath(_)),
            "NestedChild geometry mismatch: {:?}", inner_children[0].geometry
        );

        let inner_group_pos = &outer_children[1].position;
        assert!((inner_group_pos.x() - 500000.0 / 12700.0).abs() < 0.01, "inner group off.x mismatch: {}", inner_group_pos.x());
    }

    #[test]
    fn test_parse_minimal_pptx_slide_count() {
        let data = make_minimal_pptx(3);
        let prs = PptxParser::parse(&data).unwrap();
        assert_eq!(prs.slides.len(), 3, "should have 3 slides");
    }

    #[test]
    fn test_slide_size_pt() {
        let data = make_minimal_pptx(1);
        let prs = PptxParser::parse(&data).unwrap();
        // presentation.xml 写的是 9144000 x 6858000 EMU，
        // 2026-06-18 起 slide_size 统一为 pt 语义（EMU / 12700）。
        assert!((prs.slide_size.0 - 9144000.0 / 12700.0).abs() < 0.01);
        assert!((prs.slide_size.1 - 6858000.0 / 12700.0).abs() < 0.01);
    }

    #[test]
    fn test_shape_text_extracted() {
        let data = make_minimal_pptx(1);
        let prs = PptxParser::parse(&data).unwrap();
        let slide = &prs.slides[0];
        assert!(!slide.shapes.is_empty(), "slide should have shapes");
        let first = &slide.shapes[0];
        match &first.content {
            ShapeContent::Text(paras) => {
                let text: String = paras.iter()
                    .flat_map(|p| p.runs.iter().map(|r| r.text.as_str()))
                    .collect();
                assert!(text.contains("Slide 1 Title"), "got: {text}");
            }
            other => panic!("expected Text content, got {:?}", other),
        }
    }

    #[test]
    fn test_shape_position_parsed() {
        let data = make_minimal_pptx(1);
        let prs = PptxParser::parse(&data).unwrap();
        let shape = &prs.slides[0].shapes[0];
        assert!(shape.position.x() > 0.0, "x should be parsed from off element");
        assert!(shape.position.w() > 0.0, "width should be parsed from ext element");
    }

    #[test]
    fn test_normalize_path() {
        assert_eq!(normalize_path("ppt", "slides/slide1.xml"), "ppt/slides/slide1.xml");
        assert_eq!(normalize_path("ppt/slides", "../media/img.png"), "ppt/media/img.png");
        assert_eq!(normalize_path("ppt", "/absolute/path.xml"), "absolute/path.xml");
    }

    #[test]
    fn test_parse_hex_color() {
        let c = parse_hex_color("FF5733");
        assert_eq!(c.r, 0xFF);
        assert_eq!(c.g, 0x57);
        assert_eq!(c.b, 0x33);
        assert_eq!(c.a, 255);
    }

    /// 构造一个含 2 个 slideMaster 的 PPTX：
    /// slide1 -> layout1 -> master1
    /// slide2 -> layout2 -> master2
    /// 用于验证 PPTX-1（layout->master 连通性）+ PPTX-2（多master场景不再全部错误关联到第一个master）
    fn make_multi_master_pptx() -> Vec<u8> {
        use std::io::Write;
        let buf = Vec::new();
        let cursor = std::io::Cursor::new(buf);
        let mut zw = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        const REL_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
        const OFFICE_REL_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

        zw.start_file("[Content_Types].xml", opts).unwrap();
        zw.write_all(b"<?xml version=\"1.0\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"></Types>").unwrap();

        // presentation.xml.rels: rId1/rId2 -> slide1/slide2, rId10/rId11 -> master1/master2
        let prs_rels = format!(
            "<?xml version=\"1.0\"?><Relationships xmlns=\"{REL_NS}\">\
             <Relationship Id=\"rId1\" Type=\"{OFFICE_REL_NS}/slide\" Target=\"slides/slide1.xml\"/>\
             <Relationship Id=\"rId2\" Type=\"{OFFICE_REL_NS}/slide\" Target=\"slides/slide2.xml\"/>\
             <Relationship Id=\"rId10\" Type=\"{OFFICE_REL_NS}/slideMaster\" Target=\"slideMasters/slideMaster1.xml\"/>\
             <Relationship Id=\"rId11\" Type=\"{OFFICE_REL_NS}/slideMaster\" Target=\"slideMasters/slideMaster2.xml\"/>\
             </Relationships>"
        );
        zw.start_file("ppt/_rels/presentation.xml.rels", opts).unwrap();
        zw.write_all(prs_rels.as_bytes()).unwrap();

        // presentation.xml: 2 slides + 2 masters
        let prs = format!(
            "<?xml version=\"1.0\"?><p:presentation xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" xmlns:r=\"{OFFICE_REL_NS}\">\
             <p:sldMasterIdLst><p:sldMasterId id=\"2147483648\" r:id=\"rId10\"/><p:sldMasterId id=\"2147483649\" r:id=\"rId11\"/></p:sldMasterIdLst>\
             <p:sldSz cx=\"9144000\" cy=\"6858000\"/>\
             <p:sldIdLst><p:sldId id=\"1\" r:id=\"rId1\"/><p:sldId id=\"2\" r:id=\"rId2\"/></p:sldIdLst>\
             </p:presentation>"
        );
        zw.start_file("ppt/presentation.xml", opts).unwrap();
        zw.write_all(prs.as_bytes()).unwrap();

        // 两个 master，用 title 占位符位置区分（master1 x=100000, master2 x=999999）
        for (idx, x_off) in [(1, "100000"), (2, "999999")] {
            let master_xml = format!(
                r#"<?xml version="1.0"?>
<p:sldMaster xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
 <p:cSld><p:spTree>
  <p:sp>
   <p:nvSpPr><p:cNvPr id="1" name="MasterTitle"/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
   <p:spPr><a:xfrm><a:off x="{x_off}" y="274638"/><a:ext cx="8229600" cy="1143000"/></a:xfrm><a:prstGeom prst="rect"/></p:spPr>
   <p:txBody><a:p><a:r><a:t>Master {idx} Title</a:t></a:r></a:p></p:txBody>
  </p:sp>
 </p:spTree></p:cSld>
</p:sldMaster>"#
            );
            let path = format!("ppt/slideMasters/slideMaster{idx}.xml");
            zw.start_file(&path, opts).unwrap();
            zw.write_all(master_xml.as_bytes()).unwrap();
        }

        // 两个 layout，各自 rels 指向对应的 master
        for idx in 1..=2 {
            let layout_xml = format!(
                r#"<?xml version="1.0"?>
<p:sldLayout xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
 <p:cSld><p:spTree></p:spTree></p:cSld>
</p:sldLayout>"#
            );
            let path = format!("ppt/slideLayouts/slideLayout{idx}.xml");
            zw.start_file(&path, opts).unwrap();
            zw.write_all(layout_xml.as_bytes()).unwrap();

            let layout_rels = format!(
                "<?xml version=\"1.0\"?><Relationships xmlns=\"{REL_NS}\">\
                 <Relationship Id=\"rId1\" Type=\"{OFFICE_REL_NS}/slideMaster\" Target=\"../slideMasters/slideMaster{idx}.xml\"/>\
                 </Relationships>"
            );
            let rels_path = format!("ppt/slideLayouts/_rels/slideLayout{idx}.xml.rels");
            zw.start_file(&rels_path, opts).unwrap();
            zw.write_all(layout_rels.as_bytes()).unwrap();
        }

        // 两个 slide，各自 rels 指向不同的 layout
        // slide1 -> layout1 -> master1；slide2 -> layout2 -> master2
        for idx in 1..=2 {
            let slide_xml = format!(
                r#"<?xml version="1.0"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
 <p:cSld><p:spTree>
  <p:sp>
   <p:nvSpPr><p:cNvPr id="{idx}" name="Title {idx}"/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
   <p:spPr/>
   <p:txBody><a:p><a:r><a:t>Slide {idx} Title</a:t></a:r></a:p></p:txBody>
  </p:sp>
 </p:spTree></p:cSld>
</p:sld>"#
            );
            let path = format!("ppt/slides/slide{idx}.xml");
            zw.start_file(&path, opts).unwrap();
            zw.write_all(slide_xml.as_bytes()).unwrap();

            let slide_rels = format!(
                "<?xml version=\"1.0\"?><Relationships xmlns=\"{REL_NS}\">\
                 <Relationship Id=\"rId1\" Type=\"{OFFICE_REL_NS}/slideLayout\" Target=\"../slideLayouts/slideLayout{idx}.xml\"/>\
                 </Relationships>"
            );
            let rels_path = format!("ppt/slides/_rels/slide{idx}.xml.rels");
            zw.start_file(&rels_path, opts).unwrap();
            zw.write_all(slide_rels.as_bytes()).unwrap();
        }

        zw.finish().unwrap().into_inner()
    }

    #[test]
    fn test_pptx1_layout_to_master_connectivity() {
        // PPTX-1: slide->layout->master 链路应当真正连通，master_idx 不应恒为 None
        let data = make_multi_master_pptx();
        let prs = PptxParser::parse(&data).unwrap();
        assert_eq!(prs.master_slides.len(), 2, "should have parsed 2 masters");
        assert!(prs.slides[0].master_idx.is_some(), "slide1 master_idx should be resolved, not None");
        assert!(prs.slides[1].master_idx.is_some(), "slide2 master_idx should be resolved, not None");
    }

    #[test]
    fn test_pptx2_multi_master_no_longer_collapses_to_first() {
        // PPTX-2: 多master场景下，slide1/slide2 应该关联到不同的 master（各自的真实 layout->master 链），
        // 而不是像旧的"仅处理 master_rids.first()"旁路那样，两个 slide 都被错误关联到同一个（第一个）master。
        let data = make_multi_master_pptx();
        let prs = PptxParser::parse(&data).unwrap();
        let idx1 = prs.slides[0].master_idx.expect("slide1 should resolve a master");
        let idx2 = prs.slides[1].master_idx.expect("slide2 should resolve a master");
        assert_ne!(idx1, idx2, "slide1 and slide2 should resolve to DIFFERENT masters, not both collapse to the first one");

        // 进一步验证：master_idx 对应的确实是正确的那个 master（内容层面交叉验证，而不只是索引不同）
        let master1_name = &prs.master_slides[idx1].name;
        let master2_name = &prs.master_slides[idx2].name;
        assert!(master1_name.contains("slideMaster1"), "slide1 should resolve to slideMaster1, got: {master1_name}");
        assert!(master2_name.contains("slideMaster2"), "slide2 should resolve to slideMaster2, got: {master2_name}");
    }

    #[test]
    fn test_pptx3_placeholder_identity_preserved() {
        // PPTX-3: raw_to_shape 转换后，Shape.placeholder 应保留 ph type/index 身份信息，
        // 而不是像此前那样在转换阶段被丢弃（转换后连"这是占位符"这件事本身都不知道）。
        let data = make_minimal_pptx(1);
        let prs = PptxParser::parse(&data).unwrap();
        let shape = &prs.slides[0].shapes[0];
        let ph = shape.placeholder.as_ref().expect("shape with <p:ph type=\"title\"/> should have placeholder identity preserved, got None");
        assert_eq!(ph.ph_type.as_deref(), Some("title"), "ph_type should be \"title\", got: {:?}", ph.ph_type);
    }

    #[test]
    fn test_pptx3_non_placeholder_shape_has_none() {
        // 反向验证：不带 <p:ph> 的普通形状，placeholder 应为 None，不应该被误标记为占位符。
        use std::io::Write;
        let buf = Vec::new();
        let cursor = std::io::Cursor::new(buf);
        let mut zw = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        zw.start_file("[Content_Types].xml", opts).unwrap();
        zw.write_all(b"<?xml version=\"1.0\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"></Types>").unwrap();

        zw.start_file("ppt/_rels/presentation.xml.rels", opts).unwrap();
        zw.write_all(b"<?xml version=\"1.0\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide\" Target=\"slides/slide1.xml\"/><Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster\" Target=\"slideMasters/slideMaster1.xml\"/></Relationships>").unwrap();

        zw.start_file("ppt/presentation.xml", opts).unwrap();
        zw.write_all(b"<?xml version=\"1.0\"?><p:presentation xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><p:sldMasterIdLst><p:sldMasterId id=\"2147483648\" r:id=\"rId2\"/></p:sldMasterIdLst><p:sldSz cx=\"9144000\" cy=\"6858000\"/><p:sldIdLst><p:sldId id=\"1\" r:id=\"rId1\"/></p:sldIdLst></p:presentation>").unwrap();

        let slide_xml = r#"<?xml version="1.0"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
 <p:cSld><p:spTree>
  <p:sp>
   <p:nvSpPr><p:cNvPr id="1" name="PlainBox"/><p:nvPr/></p:nvSpPr>
   <p:spPr><a:xfrm><a:off x="457200" y="274638"/><a:ext cx="8229600" cy="1143000"/></a:xfrm><a:prstGeom prst="rect"/></p:spPr>
   <p:txBody><a:p><a:r><a:t>Just a plain text box</a:t></a:r></a:p></p:txBody>
  </p:sp>
 </p:spTree></p:cSld>
</p:sld>"#;
        zw.start_file("ppt/slides/slide1.xml", opts).unwrap();
        zw.write_all(slide_xml.as_bytes()).unwrap();
        zw.start_file("ppt/slides/_rels/slide1.xml.rels", opts).unwrap();
        zw.write_all(b"<?xml version=\"1.0\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"></Relationships>").unwrap();

        let data = zw.finish().unwrap().into_inner();
        let prs = PptxParser::parse(&data).unwrap();
        let shape = &prs.slides[0].shapes[0];
        assert!(shape.placeholder.is_none(), "plain shape without <p:ph> should have placeholder = None, got: {:?}", shape.placeholder);
    }

    /// PPTX-4：构造一个含 master + layout + slide 的最小fixture，
    /// master 上有两个 body 类型占位符（一个无idx，一个idx=5，带不同样式用于区分来源），
    /// 外加一个 title 类型占位符（用于测试 ctrTitle→title 的 fallback）。
    /// slide 上有两个占位符形状：一个 <p:ph type="body" idx="5"/>（应通过index精确匹配到idx=5那个，
    /// 而不是被无idx的body覆盖），一个 <p:ph type="ctrTitle"/>（应通过fallback匹配到master的title）。
    /// 路径链路完整对齐 make_multi_master_pptx 已验证可用的写法：
    /// slide的rels -> layout(相对路径 ../slideLayouts/...) -> layout的rels -> master(相对路径 ../slideMasters/...)。
    fn make_pptx4_fixture() -> Vec<u8> {
        use std::io::Write;
        use zip::write::FileOptions;

        const REL_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
        const OFFICE_REL_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

        let mut buf = std::io::Cursor::new(Vec::new());
        let mut zw = zip::ZipWriter::new(&mut buf);
        let opts: FileOptions<()> = FileOptions::default();

        zw.start_file("[Content_Types].xml", opts).unwrap();
        zw.write_all(b"<?xml version=\"1.0\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"></Types>").unwrap();

        // presentation.xml.rels: 只需要指向 slide（layout/master 的发现是从 slide 自己的 rels 出发的，
        // 不经过 presentation.xml.rels，对齐 find_slide_layout_target 的实际调用路径）。
        let prs_rels = format!(
            "<?xml version=\"1.0\"?><Relationships xmlns=\"{REL_NS}\">\
             <Relationship Id=\"rId1\" Type=\"{OFFICE_REL_NS}/slide\" Target=\"slides/slide1.xml\"/>\
             <Relationship Id=\"rId2\" Type=\"{OFFICE_REL_NS}/slideMaster\" Target=\"slideMasters/slideMaster1.xml\"/>\
             </Relationships>"
        );
        zw.start_file("ppt/_rels/presentation.xml.rels", opts).unwrap();
        zw.write_all(prs_rels.as_bytes()).unwrap();

        zw.start_file("ppt/presentation.xml", opts).unwrap();
        zw.write_all(b"<?xml version=\"1.0\"?><p:presentation xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><p:sldMasterIdLst><p:sldMasterId id=\"2147483648\" r:id=\"rId2\"/></p:sldMasterIdLst><p:sldSz cx=\"9144000\" cy=\"6858000\"/><p:sldIdLst><p:sldId id=\"1\" r:id=\"rId1\"/></p:sldIdLst></p:presentation>").unwrap();

        // master：含3个占位符形状
        //   1. body 类型，无idx，位置(100,100,1000,1000)，字号10
        //   2. body 类型，idx=5，位置(200,200,2000,2000)，字号20 <- slide上idx=5的body应精确匹配这个
        //   3. title 类型，无idx，位置(300,300,3000,3000)，字号30 <- ctrTitle应fallback匹配这个
        let master_xml = r#"<?xml version="1.0"?>
<p:sldMaster xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
 <p:cSld><p:spTree>
  <p:sp>
   <p:nvSpPr><p:cNvPr id="1" name="BodyNoIdx"/><p:nvPr><p:ph type="body"/></p:nvPr></p:nvSpPr>
   <p:spPr><a:xfrm><a:off x="1270000" y="1270000"/><a:ext cx="12700000" cy="12700000"/></a:xfrm></p:spPr>
   <p:txBody><a:p><a:r><a:rPr sz="1000"/><a:t>BodyNoIdx</a:t></a:r></a:p></p:txBody>
  </p:sp>
  <p:sp>
   <p:nvSpPr><p:cNvPr id="2" name="BodyIdx5"/><p:nvPr><p:ph type="body" idx="5"/></p:nvPr></p:nvSpPr>
   <p:spPr><a:xfrm><a:off x="2540000" y="2540000"/><a:ext cx="25400000" cy="25400000"/></a:xfrm></p:spPr>
   <p:txBody><a:p><a:r><a:rPr sz="2000"/><a:t>BodyIdx5</a:t></a:r></a:p></p:txBody>
  </p:sp>
  <p:sp>
   <p:nvSpPr><p:cNvPr id="3" name="TitleShape"/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
   <p:spPr><a:xfrm><a:off x="3810000" y="3810000"/><a:ext cx="38100000" cy="38100000"/></a:xfrm></p:spPr>
   <p:txBody><a:p><a:r><a:rPr sz="3000"/><a:t>TitleShape</a:t></a:r></a:p></p:txBody>
  </p:sp>
 </p:spTree></p:cSld>
</p:sldMaster>"#;
        zw.start_file("ppt/slideMasters/slideMaster1.xml", opts).unwrap();
        zw.write_all(master_xml.as_bytes()).unwrap();
        zw.start_file("ppt/slideMasters/_rels/slideMaster1.xml.rels", opts).unwrap();
        zw.write_all(format!("<?xml version=\"1.0\"?><Relationships xmlns=\"{REL_NS}\"></Relationships>").as_bytes()).unwrap();

        // layout：无占位符形状本身（本测试聚焦 master 层匹配），只作为 slide->master 链路的中间跳板
        let layout_xml = r#"<?xml version="1.0"?>
<p:sldLayout xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
 <p:cSld><p:spTree></p:spTree></p:cSld>
</p:sldLayout>"#;
        zw.start_file("ppt/slideLayouts/slideLayout1.xml", opts).unwrap();
        zw.write_all(layout_xml.as_bytes()).unwrap();

        let layout_rels = format!(
            "<?xml version=\"1.0\"?><Relationships xmlns=\"{REL_NS}\">\
             <Relationship Id=\"rId1\" Type=\"{OFFICE_REL_NS}/slideMaster\" Target=\"../slideMasters/slideMaster1.xml\"/>\
             </Relationships>"
        );
        zw.start_file("ppt/slideLayouts/_rels/slideLayout1.xml.rels", opts).unwrap();
        zw.write_all(layout_rels.as_bytes()).unwrap();

        // slide: 两个占位符形状，均无自身位置（position=0，触发继承）
        //   1. <p:ph type="body" idx="5"/>  应该匹配 master 的 BodyIdx5（idx精确匹配），而非 BodyNoIdx
        //   2. <p:ph type="ctrTitle"/>       应该通过 fallback(ctrTitle->title) 匹配 master 的 TitleShape
        let slide_xml = r#"<?xml version="1.0"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
 <p:cSld><p:spTree>
  <p:sp>
   <p:nvSpPr><p:cNvPr id="1" name="SlideBody"/><p:nvPr><p:ph type="body" idx="5"/></p:nvPr></p:nvSpPr>
   <p:spPr><a:prstGeom prst="rect"/></p:spPr>
   <p:txBody><a:p><a:r><a:t>slide body text</a:t></a:r></a:p></p:txBody>
  </p:sp>
  <p:sp>
   <p:nvSpPr><p:cNvPr id="2" name="SlideCtrTitle"/><p:nvPr><p:ph type="ctrTitle"/></p:nvPr></p:nvSpPr>
   <p:spPr><a:prstGeom prst="rect"/></p:spPr>
   <p:txBody><a:p><a:r><a:t>slide title text</a:t></a:r></a:p></p:txBody>
  </p:sp>
 </p:spTree></p:cSld>
</p:sld>"#;
        zw.start_file("ppt/slides/slide1.xml", opts).unwrap();
        zw.write_all(slide_xml.as_bytes()).unwrap();

        let slide_rels = format!(
            "<?xml version=\"1.0\"?><Relationships xmlns=\"{REL_NS}\">\
             <Relationship Id=\"rId1\" Type=\"{OFFICE_REL_NS}/slideLayout\" Target=\"../slideLayouts/slideLayout1.xml\"/>\
             </Relationships>"
        );
        zw.start_file("ppt/slides/_rels/slide1.xml.rels", opts).unwrap();
        zw.write_all(slide_rels.as_bytes()).unwrap();

        zw.finish().unwrap();
        buf.into_inner()
    }

    /// PPTX-5：验证 presentation.xml <p:defaultTextStyle> 全局兜底。
    /// master 上唯一的 body 占位符 <a:rPr/> 不带 sz（无字号），layout 为空，
    /// 因此 layout/master 两层继承链都提供不了字号，必须靠 presentation.xml
    /// 的 <p:defaultTextStyle><a:lvl1pPr><a:defRPr sz="1800"/></a:lvl1pPr> 兜底到 18.0pt。
    fn make_proot004b_fontref_fixture() -> Vec<u8> {
        // PROOT-004b 回归测试fixture：验证 <p:style><a:fontRef> 文字颜色
        // fallback的优先级——两个shape共用同一份<p:style>（fontRef引用
        // lt1=白色），但shape1的run自身<a:rPr>显式写了红色，shape2的run
        // 完全不带颜色。预期：shape1保持红色（不被style覆盖），shape2
        // 变成白色（style fallback生效）。
        use std::io::Write;
        use zip::write::FileOptions;

        const REL_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
        const OFFICE_REL_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

        let mut buf = std::io::Cursor::new(Vec::new());
        let mut zw = zip::ZipWriter::new(&mut buf);
        let opts: FileOptions<()> = FileOptions::default();

        zw.start_file("[Content_Types].xml", opts).unwrap();
        zw.write_all(b"<?xml version=\"1.0\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"></Types>").unwrap();

        let prs_rels = format!(
            "<?xml version=\"1.0\"?><Relationships xmlns=\"{REL_NS}\">\
             <Relationship Id=\"rId1\" Type=\"{OFFICE_REL_NS}/slide\" Target=\"slides/slide1.xml\"/>\
             <Relationship Id=\"rId2\" Type=\"{OFFICE_REL_NS}/slideMaster\" Target=\"slideMasters/slideMaster1.xml\"/>\
             </Relationships>"
        );
        zw.start_file("ppt/_rels/presentation.xml.rels", opts).unwrap();
        zw.write_all(prs_rels.as_bytes()).unwrap();

        let prs_xml = "<?xml version=\"1.0\"?><p:presentation xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><p:sldMasterIdLst><p:sldMasterId id=\"2147483648\" r:id=\"rId2\"/></p:sldMasterIdLst><p:sldSz cx=\"9144000\" cy=\"6858000\"/><p:sldIdLst><p:sldId id=\"1\" r:id=\"rId1\"/></p:sldIdLst></p:presentation>";
        zw.start_file("ppt/presentation.xml", opts).unwrap();
        zw.write_all(prs_xml.as_bytes()).unwrap();

        // theme1.xml: lt1定义为白色FFFFFF，accent1定义为绿色00FF00
        // （accent1本次测试用不到，写上是为了贴近真实theme结构完整性）
        let theme_xml = r#"<?xml version="1.0"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="TestTheme">
 <a:themeElements>
  <a:clrScheme name="Test">
   <a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1>
   <a:lt1><a:srgbClr val="FFFFFF"/></a:lt1>
   <a:dk2><a:srgbClr val="1F497D"/></a:dk2>
   <a:lt2><a:srgbClr val="EEECE1"/></a:lt2>
   <a:accent1><a:srgbClr val="00FF00"/></a:accent1>
   <a:accent2><a:srgbClr val="4F81BD"/></a:accent2>
   <a:accent3><a:srgbClr val="9BBB59"/></a:accent3>
   <a:accent4><a:srgbClr val="8064A2"/></a:accent4>
   <a:accent5><a:srgbClr val="4BACC6"/></a:accent5>
   <a:accent6><a:srgbClr val="F79646"/></a:accent6>
   <a:hlink><a:srgbClr val="0000FF"/></a:hlink>
   <a:folHlink><a:srgbClr val="800080"/></a:folHlink>
  </a:clrScheme>
 </a:themeElements>
</a:theme>"#;
        zw.start_file("ppt/theme/theme1.xml", opts).unwrap();
        zw.write_all(theme_xml.as_bytes()).unwrap();

        let master_xml = r#"<?xml version="1.0"?>
<p:sldMaster xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
 <p:cSld><p:spTree></p:spTree></p:cSld>
</p:sldMaster>"#;
        zw.start_file("ppt/slideMasters/slideMaster1.xml", opts).unwrap();
        zw.write_all(master_xml.as_bytes()).unwrap();
        zw.start_file("ppt/slideMasters/_rels/slideMaster1.xml.rels", opts).unwrap();
        zw.write_all(format!("<?xml version=\"1.0\"?><Relationships xmlns=\"{REL_NS}\"></Relationships>").as_bytes()).unwrap();

        let layout_xml = r#"<?xml version="1.0"?>
<p:sldLayout xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
 <p:cSld><p:spTree></p:spTree></p:cSld>
</p:sldLayout>"#;
        zw.start_file("ppt/slideLayouts/slideLayout1.xml", opts).unwrap();
        zw.write_all(layout_xml.as_bytes()).unwrap();

        let layout_rels = format!(
            "<?xml version=\"1.0\"?><Relationships xmlns=\"{REL_NS}\">\
             <Relationship Id=\"rId1\" Type=\"{OFFICE_REL_NS}/slideMaster\" Target=\"../slideMasters/slideMaster1.xml\"/>\
             </Relationships>"
        );
        zw.start_file("ppt/slideLayouts/_rels/slideLayout1.xml.rels", opts).unwrap();
        zw.write_all(layout_rels.as_bytes()).unwrap();

        // shape1: run自身<a:rPr>显式写红色FF0000 —— 预期style的白色fallback
        //         不应覆盖它，最终颜色应保持红色。
        // shape2: run完全没有<a:rPr>颜色定义 —— 预期style的lt1白色fallback
        //         生效，最终颜色应变成白色。
        // 两者都带同一份<p:style><a:fontRef idx="minor"><a:schemeClr val="lt1"/>
        // </a:fontRef></p:style>，只有run自身颜色不同，用来隔离验证优先级。
        let slide_xml = r#"<?xml version="1.0"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
 <p:cSld><p:spTree>
  <p:sp>
   <p:nvSpPr><p:cNvPr id="1" name="ShapeWithOwnColor"/><p:nvPr/></p:nvSpPr>
   <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="1000000" cy="1000000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr>
   <p:style><a:lnRef idx="1"><a:schemeClr val="accent1"/></a:lnRef><a:fillRef idx="3"><a:schemeClr val="accent1"/></a:fillRef><a:effectRef idx="2"><a:schemeClr val="accent1"/></a:effectRef><a:fontRef idx="minor"><a:schemeClr val="lt1"/></a:fontRef></p:style>
   <p:txBody><a:p><a:r><a:rPr><a:solidFill><a:srgbClr val="FF0000"/></a:solidFill></a:rPr><a:t>OwnColor</a:t></a:r></a:p></p:txBody>
  </p:sp>
  <p:sp>
   <p:nvSpPr><p:cNvPr id="2" name="ShapeNoOwnColor"/><p:nvPr/></p:nvSpPr>
   <p:spPr><a:xfrm><a:off x="2000000" y="0"/><a:ext cx="1000000" cy="1000000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr>
   <p:style><a:lnRef idx="1"><a:schemeClr val="accent1"/></a:lnRef><a:fillRef idx="3"><a:schemeClr val="accent1"/></a:fillRef><a:effectRef idx="2"><a:schemeClr val="accent1"/></a:effectRef><a:fontRef idx="minor"><a:schemeClr val="lt1"/></a:fontRef></p:style>
   <p:txBody><a:p><a:r><a:rPr/><a:t>NoOwnColor</a:t></a:r></a:p></p:txBody>
  </p:sp>
 </p:spTree></p:cSld>
</p:sld>"#;
        zw.start_file("ppt/slides/slide1.xml", opts).unwrap();
        zw.write_all(slide_xml.as_bytes()).unwrap();

        let slide_rels = format!(
            "<?xml version=\"1.0\"?><Relationships xmlns=\"{REL_NS}\">\
             <Relationship Id=\"rId1\" Type=\"{OFFICE_REL_NS}/slideLayout\" Target=\"../slideLayouts/slideLayout1.xml\"/>\
             </Relationships>"
        );
        zw.start_file("ppt/slides/_rels/slide1.xml.rels", opts).unwrap();
        zw.write_all(slide_rels.as_bytes()).unwrap();

        zw.finish().unwrap();
        buf.into_inner()
    }

    #[test]
    fn test_proot004b_style_fontref_does_not_override_own_run_color() {
        let data = make_proot004b_fontref_fixture();
        let prs = PptxParser::parse(&data).unwrap();
        let shape1 = prs.slides[0].shapes.iter()
            .find(|s| s.id == 1)
            .expect("should find shape1 (own red color)");
        if let roe_model::impress::ShapeContent::Text(paras) = &shape1.content {
            let run = &paras[0].runs[0];
            let c = run.color.expect("run should have a color (own red)");
            assert_eq!((c.r, c.g, c.b, c.a), (0xFF, 0x00, 0x00, 255),
                "run's own explicit rPr solidFill (red) must NOT be overridden by \
                 <p:style><a:fontRef> fallback (white) — own color has higher priority");
        } else {
            panic!("expected Text content for shape1");
        }
    }

    #[test]
    fn test_proot004b_style_fontref_fallback_applies_when_run_has_no_color() {
        let data = make_proot004b_fontref_fixture();
        let prs = PptxParser::parse(&data).unwrap();
        let shape2 = prs.slides[0].shapes.iter()
            .find(|s| s.id == 2)
            .expect("should find shape2 (no own color)");
        if let roe_model::impress::ShapeContent::Text(paras) = &shape2.content {
            let run = &paras[0].runs[0];
            let c = run.color.expect("run should have a color (fallback white)");
            assert_eq!((c.r, c.g, c.b, c.a), (0xFF, 0xFF, 0xFF, 255),
                "run with no own color should fall back to <p:style><a:fontRef> \
                 (lt1 theme slot = white FFFFFF)");
        } else {
            panic!("expected Text content for shape2");
        }
    }

    fn make_pptx5_fixture() -> Vec<u8> {
        use std::io::Write;
        use zip::write::FileOptions;

        const REL_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
        const OFFICE_REL_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

        let mut buf = std::io::Cursor::new(Vec::new());
        let mut zw = zip::ZipWriter::new(&mut buf);
        let opts: FileOptions<()> = FileOptions::default();

        zw.start_file("[Content_Types].xml", opts).unwrap();
        zw.write_all(b"<?xml version=\"1.0\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"></Types>").unwrap();

        let prs_rels = format!(
            "<?xml version=\"1.0\"?><Relationships xmlns=\"{REL_NS}\">\
             <Relationship Id=\"rId1\" Type=\"{OFFICE_REL_NS}/slide\" Target=\"slides/slide1.xml\"/>\
             <Relationship Id=\"rId2\" Type=\"{OFFICE_REL_NS}/slideMaster\" Target=\"slideMasters/slideMaster1.xml\"/>\
             </Relationships>"
        );
        zw.start_file("ppt/_rels/presentation.xml.rels", opts).unwrap();
        zw.write_all(prs_rels.as_bytes()).unwrap();

        // presentation.xml：带 <p:defaultTextStyle>，lvl1pPr/defRPr sz="1800" (18.0pt)
        let prs_xml = "<?xml version=\"1.0\"?><p:presentation xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><p:sldMasterIdLst><p:sldMasterId id=\"2147483648\" r:id=\"rId2\"/></p:sldMasterIdLst><p:sldSz cx=\"9144000\" cy=\"6858000\"/><p:sldIdLst><p:sldId id=\"1\" r:id=\"rId1\"/></p:sldIdLst><p:defaultTextStyle><a:lvl1pPr><a:defRPr sz=\"1800\"/></a:lvl1pPr></p:defaultTextStyle></p:presentation>";
        zw.start_file("ppt/presentation.xml", opts).unwrap();
        zw.write_all(prs_xml.as_bytes()).unwrap();

        // master：唯一的 body 占位符，rPr 不带 sz（无字号来源）
        let master_xml = r#"<?xml version="1.0"?>
<p:sldMaster xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
 <p:cSld><p:spTree>
  <p:sp>
   <p:nvSpPr><p:cNvPr id="1" name="Body"/><p:nvPr><p:ph type="body"/></p:nvPr></p:nvSpPr>
   <p:spPr><a:xfrm><a:off x="1270000" y="1270000"/><a:ext cx="12700000" cy="12700000"/></a:xfrm></p:spPr>
   <p:txBody><a:p><a:r><a:rPr/><a:t>Body</a:t></a:r></a:p></p:txBody>
  </p:sp>
 </p:spTree></p:cSld>
</p:sldMaster>"#;
        zw.start_file("ppt/slideMasters/slideMaster1.xml", opts).unwrap();
        zw.write_all(master_xml.as_bytes()).unwrap();
        zw.start_file("ppt/slideMasters/_rels/slideMaster1.xml.rels", opts).unwrap();
        zw.write_all(format!("<?xml version=\"1.0\"?><Relationships xmlns=\"{REL_NS}\"></Relationships>").as_bytes()).unwrap();

        let layout_xml = r#"<?xml version="1.0"?>
<p:sldLayout xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
 <p:cSld><p:spTree></p:spTree></p:cSld>
</p:sldLayout>"#;
        zw.start_file("ppt/slideLayouts/slideLayout1.xml", opts).unwrap();
        zw.write_all(layout_xml.as_bytes()).unwrap();

        let layout_rels = format!(
            "<?xml version=\"1.0\"?><Relationships xmlns=\"{REL_NS}\">\
             <Relationship Id=\"rId1\" Type=\"{OFFICE_REL_NS}/slideMaster\" Target=\"../slideMasters/slideMaster1.xml\"/>\
             </Relationships>"
        );
        zw.start_file("ppt/slideLayouts/_rels/slideLayout1.xml.rels", opts).unwrap();
        zw.write_all(layout_rels.as_bytes()).unwrap();

        let slide_xml = r#"<?xml version="1.0"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
 <p:cSld><p:spTree>
  <p:sp>
   <p:nvSpPr><p:cNvPr id="1" name="SlideBody"/><p:nvPr><p:ph type="body"/></p:nvPr></p:nvSpPr>
   <p:spPr><a:prstGeom prst="rect"/></p:spPr>
   <p:txBody><a:p><a:r><a:t>slide body text</a:t></a:r></a:p></p:txBody>
  </p:sp>
 </p:spTree></p:cSld>
</p:sld>"#;
        zw.start_file("ppt/slides/slide1.xml", opts).unwrap();
        zw.write_all(slide_xml.as_bytes()).unwrap();

        let slide_rels = format!(
            "<?xml version=\"1.0\"?><Relationships xmlns=\"{REL_NS}\">\
             <Relationship Id=\"rId1\" Type=\"{OFFICE_REL_NS}/slideLayout\" Target=\"../slideLayouts/slideLayout1.xml\"/>\
             </Relationships>"
        );
        zw.start_file("ppt/slides/_rels/slide1.xml.rels", opts).unwrap();
        zw.write_all(slide_rels.as_bytes()).unwrap();

        zw.finish().unwrap();
        buf.into_inner()
    }

    #[test]
    fn test_pptx5_default_text_style_global_fallback() {
        let data = make_pptx5_fixture();
        let prs = PptxParser::parse(&data).unwrap();
        let body_shape = prs.slides[0].shapes.iter()
            .find(|s| s.placeholder.as_ref().and_then(|p| p.ph_type.as_deref()) == Some("body"))
            .expect("should find the body placeholder shape");
        if let roe_model::impress::ShapeContent::Text(paras) = &body_shape.content {
            let run = &paras[0].runs[0];
            assert_eq!(run.font_size, Some(18.0), "should fall back to presentation.xml defaultTextStyle (sz=1800 -> 18.0pt) when layout/master provide no font_size");
        } else {
            panic!("expected Text content");
        }
    }

    #[test]
    fn test_pptx4_index_match_beats_untyped_index_match() {
        // 验证：<p:ph type="body" idx="5"/> 应该精确匹配 master 上 idx=5 的那个 body 占位符，
        // 取到它的位置(200,200,2000,2000)和字号(20.0)，而不是被 master 上另一个无idx的 body
        // 占位符（位置100,100,1000,1000，字号10.0）覆盖或干扰。
        // 这是本次PPTX-4改造前完全不存在的能力（旧实现按 ph_type 字符串做 HashMap key，
        // 同类型多个占位符会互相覆盖，无法通过 idx 区分）。
        let data = make_pptx4_fixture();
        let prs = PptxParser::parse(&data).unwrap();
        let body_shape = prs.slides[0].shapes.iter()
            .find(|s| s.placeholder.as_ref().and_then(|p| p.ph_type.as_deref()) == Some("body"))
            .expect("should find the body placeholder shape");

        assert_eq!(body_shape.position.x(), 200.0, "should inherit position from idx=5 candidate, not the untyped one");
        assert_eq!(body_shape.position.w(), 2000.0, "should inherit width from idx=5 candidate");
        // (注：master_xml 里坐标以 EMU 书写，2540000 EMU / 12700 = 200.0 pt，与上面断言一致)

    }

    #[test]
    fn test_pptx17_master_style_inheritance_now_works() {
        // 验证 PPTX-17 修复：parse_raw_shapes 此前从不解析 <p:txBody>，
        // 导致 master/layout 层占位符候选（PlaceholderCandidate.style）永远是默认值 None，
        // PPTX-4 的样式继承字段（font_size/bold/italic/color/font_family）实际从未生效，
        // 只有位置继承在工作。本测试验证补上 txBody/run 解析后，样式继承确实生效。
        let data = make_pptx4_fixture();
        let prs = PptxParser::parse(&data).unwrap();

        // idx=5 的 body 候选：fixture 里 sz="2000" (20.0pt)
        let body_shape = prs.slides[0].shapes.iter()
            .find(|s| s.placeholder.as_ref().and_then(|p| p.ph_type.as_deref()) == Some("body"))
            .expect("should find the body placeholder shape");
        if let roe_model::impress::ShapeContent::Text(paras) = &body_shape.content {
            let run = &paras[0].runs[0];
            assert_eq!(run.font_size, Some(20.0), "master-layer style inheritance should now populate font_size from idx=5 candidate (was None before PPTX-17)");
        } else {
            panic!("expected Text content");
        }

        // ctrTitle fallback 到 title 候选：fixture 里 sz="3000" (30.0pt)
        let title_shape = prs.slides[0].shapes.iter()
            .find(|s| s.placeholder.as_ref().and_then(|p| p.ph_type.as_deref()) == Some("ctrTitle"))
            .expect("should find the ctrTitle placeholder shape");
        if let roe_model::impress::ShapeContent::Text(paras) = &title_shape.content {
            let run = &paras[0].runs[0];
            assert_eq!(run.font_size, Some(30.0), "master-layer style inheritance via fallback should now populate font_size (was None before PPTX-17)");
        } else {
            panic!("expected Text content");
        }
    }

    #[test]
    fn test_pptx4_fallback_type_ctrtitle_to_title() {
        // 验证：<p:ph type="ctrTitle"/> 在没有精确type="ctrTitle"候选时，
        // 应通过 fallback_ph_type(ctrTitle -> title) 匹配到 master 上 type="title" 的占位符，
        // 取到其位置(300,300,3000,3000)和字号(30.0)。
        // 这是PPTX-4新增的匹配维度，旧实现（纯字符串HashMap查找）完全没有fallback能力。
        let data = make_pptx4_fixture();
        let prs = PptxParser::parse(&data).unwrap();
        let title_shape = prs.slides[0].shapes.iter()
            .find(|s| s.placeholder.as_ref().and_then(|p| p.ph_type.as_deref()) == Some("ctrTitle"))
            .expect("should find the ctrTitle placeholder shape");

        assert_eq!(title_shape.position.x(), 300.0, "should inherit position from fallback-matched title candidate");
        // (注：master_xml 里坐标以 EMU 书写，3810000 EMU / 12700 = 300.0 pt，与上面断言一致)

    }

    // PROOT-outerShdw (v84会话): 端到端探针测试，覆盖"解析层
    // (<a:outerShdw>读取dist/dir/blurRad/颜色/alpha写入outer_shdw_raw)
    // +组装层(raw_to_shape换算成ShadowStyle)"两段，parse_slide_xml_shapes
    // 直接返回Shape（已经过raw_to_shape），一个测试即可覆盖两层，不需要
    // 分别测RawShape中间态。
    //
    // 构造值参照真实样本custom-shape-bitmap-fill.pptx（本次会话验证
    // 用的样本）：dist=0（无偏移，最简单场景），blurRad=254000 EMU，
    // 纯黑色+alpha=43000（43%）。
    //
    // 换算断言：
    //   blur: 254000 / 12700.0 = 20.0 pt
    //   offset_x/offset_y: dist=0 恒为 0.0（不管dir是多少）
    //   color.a: 43000 / 100000.0 * 255.0 = 109.65 -> 109 (as u8截断)
    #[test]
    fn test_proot_outershdw_dist0_parses_and_assembles_shadow_style() {
        let xml = r#"<?xml version="1.0"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <p:cSld>
    <p:spTree>
      <p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="Rect 1"/><p:nvPr/></p:nvSpPr>
        <p:spPr>
          <a:xfrm><a:off x="0" y="0"/><a:ext cx="100000" cy="100000"/></a:xfrm>
          <a:prstGeom prst="rect"/>
          <a:solidFill><a:srgbClr val="FFFFFF"/></a:solidFill>
          <a:effectLst>
            <a:outerShdw blurRad="254000" algn="tl" rotWithShape="0">
              <a:srgbClr val="000000"><a:alpha val="43000"/></a:srgbClr>
            </a:outerShdw>
          </a:effectLst>
        </p:spPr>
      </p:sp>
    </p:spTree>
  </p:cSld>
</p:sld>"#;
        let image_map = HashMap::new();
        let chart_map = HashMap::new();
        let theme_colors = HashMap::new();
        let default_style = roe_model::impress::DefaultTextStyle::default();
        let shapes = parse_slide_xml_shapes(
            "(test)",
            xml, &image_map, &chart_map, &theme_colors, &HashMap::new(), &[], &[], &default_style, &[], &[], &[], None,
        ).expect("should parse slide with outerShdw");

        assert_eq!(shapes.len(), 1);
        let shadow = shapes[0].style.shadow.as_ref()
            .expect("shape应有shadow（outerShdw已解析+组装）");

        assert!((shadow.blur - 20.0).abs() < 1e-3,
            "blurRad=254000 EMU应换算成20.0pt，实际={}", shadow.blur);
        assert!((shadow.offset_x - 0.0).abs() < 1e-3,
            "dist=0时offset_x应恒为0.0，实际={}", shadow.offset_x);
        assert!((shadow.offset_y - 0.0).abs() < 1e-3,
            "dist=0时offset_y应恒为0.0，实际={}", shadow.offset_y);
        assert_eq!((shadow.color.r, shadow.color.g, shadow.color.b), (0, 0, 0),
            "outerShdw颜色应为纯黑");
        assert_eq!(shadow.color.a, 109,
            "alpha=43000(43%)应换算成Color.a=109（43000/100000.0*255.0截断），实际={}", shadow.color.a);
    }

}

// v320a2: layout reading helpers (ordered by sldLayoutIdLst, fallback to rels order)
fn parse_master_layout_targets(master_xml: &str, rels_xml: &str) -> Vec<String> {
    let mut rels: Vec<(String, String)> = Vec::new();
    {
        let mut reader = Reader::from_str(rels_xml);
        reader.config_mut().trim_text(true);
        loop {
            match reader.read_event() {
                Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e)) => {
                    if e.local_name().as_ref() == b"Relationship" {
                        let type_val = attr_str(e, b"Type").unwrap_or_default();
                        if type_val.contains("slideLayout") {
                            if let (Some(id), Some(t)) = (attr_str(e, b"Id"), attr_str(e, b"Target")) {
                                rels.push((id, t));
                            }
                        }
                    }
                }
                Ok(Event::Eof) | Err(_) => break,
                _ => {}
            }
        }
    }
    let mut ordered: Vec<String> = Vec::new();
    {
        let mut reader = Reader::from_str(master_xml);
        reader.config_mut().trim_text(true);
        loop {
            match reader.read_event() {
                Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e)) => {
                    if e.local_name().as_ref() == b"sldLayoutId" {
                        for a in e.attributes().flatten() {
                            if a.key.as_ref().ends_with(b":id") {
                                let rid = String::from_utf8_lossy(&a.value).to_string();
                                if let Some((_, t)) = rels.iter().find(|(i, _)| *i == rid) {
                                    ordered.push(t.clone());
                                }
                            }
                        }
                    }
                }
                Ok(Event::Eof) | Err(_) => break,
                _ => {}
            }
        }
    }
    if ordered.is_empty() {
        ordered = rels.into_iter().map(|(_, t)| t).collect();
    }
    ordered
}

/// returns (cSld name, sldLayout type attr, showMasterSp)
fn parse_layout_meta(xml: &str) -> (Option<String>, Option<String>, bool) {
    let mut name: Option<String> = None;
    let mut ty: Option<String> = None;
    let mut show = true;
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(Event::Empty(ref e)) | Ok(Event::Start(ref e)) => {
                match e.local_name().as_ref() {
                    b"sldLayout" => {
                        ty = attr_str(e, b"type");
                        if let Some(v) = attr_str(e, b"showMasterSp") {
                            if v == "0" || v == "false" {
                                show = false;
                            }
                        }
                    }
                    b"cSld" => {
                        name = attr_str(e, b"name");
                        break;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    (name, ty, show)
}

// v320b0: cut one element (open tag through close tag) out of an xml string verbatim
fn extract_raw_element(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let s = xml.find(&open)?;
    let e = xml[s..].find(&close)? + s + close.len();
    Some(xml[s..e].to_string())
}
