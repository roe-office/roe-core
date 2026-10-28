//! FODP (Flat ODF Presentation) 解析器
//! 解析 FODP XML → roe_model::impress::Presentation
//!
//! 已知简化（v1，后续版本逐步补全）：
//! - ShapeStyle（填充/描边/阴影）暂不解析图形样式继承链，统一 ShapeStyle::default()
//! - draw:image 暂不解码 base64，归为 ShapeContent::Empty
//! - anim:transitionFilter 带 smil:targetElement 的形状级转场效果（如 dissolve）
//!   暂不映射（AnimationNode 尚无 TransitionFilter 变体，见架构文档 8.4，
//!   语义上同组的 anim:set 仍会设置 visibility，不会完全丢失信息）
//! - smil:begin="idN.begin" / "idN.end" 这种跨动画节点的事件引用，
//!   v1 映射为 BeginCondition::Indefinite（需要两遍扫描建立节点 id 索引，留待后续）

use std::collections::HashMap;


use crate::ParseError;
use roe_model::common::{Color, StyleRef, Transform};
use roe_model::style::{CharStyle, ParagraphStyle, StyleRegistry};
use roe_model::impress::{
    AnimProperty, AnimValue, AnimationEffect, AnimationNode, BeginCondition, EasingFunction, EffectNodeType,
    FillMode, MasterSlide, Presentation, RepeatSpec, Shape, ShapeContent, ShapeGeometry,
    ShapeStyle, Slide, SlideTransition, TimingSpec, TransformKind, TransitionKind,
};
use roe_model::writer::{ListInfo, Paragraph, Run};

pub struct FodpParser;

impl FodpParser {
    /// 解析 FODP XML 字符串，返回 Presentation
    pub fn parse(xml: &str) -> Result<Presentation, ParseError> {
        let root = parse_tree(xml)?;
        Ok(build_presentation(&root))
    }
}

// ════════════════════════════════════════════════════════════
// 通用 XML 树（一次性解析为树，后续按需遍历）
// ════════════════════════════════════════════════════════════

use super::xml_tree::{XNode, parse_tree};

// ════════════════════════════════════════════════════════════
// 顶层：Presentation 构建
// ════════════════════════════════════════════════════════════

fn build_presentation(root: &XNode) -> Presentation {
    let doc = root.first_child_tag("office:document").unwrap_or(root);

    let slide_size = find_slide_size(doc);

    // 解析图形样式表
    let style_map = parse_style_map(doc);
    // 解析文字样式表
    let styles = parse_text_style_map(doc);

    // 母版
    let mut master_slides = Vec::new();
    let mut master_name_to_idx: HashMap<String, usize> = HashMap::new();
    if let Some(master_styles) = doc.first_child_tag("office:master-styles") {
        for (i, mp) in master_styles.children_tag("style:master-page").into_iter().enumerate() {
            let name = mp.attr("style:name").unwrap_or("").to_string();
            let mut id_map = HashMap::new();
            let mut next_id = 1u32;
            let shapes = collect_shapes(mp, &mut id_map, &mut next_id, &style_map);
            master_name_to_idx.insert(name.clone(), i);
            master_slides.push(MasterSlide { theme_xml: None, tx_styles_xml: None, layouts: Vec::new(), color_map: None, raw_ext: None, name, shapes, background: None }); // PROOT-005: ODF侧本次不解析背景
        }
    }

    // 幻灯片
    let mut slides = Vec::new();
    if let Some(body) = doc.first_child_tag("office:body") {
        if let Some(presentation) = body.first_child_tag("office:presentation") {
            for page in presentation.children_tag("draw:page") {
                slides.push(build_slide(page, &master_name_to_idx, &style_map));
            }
        }
    }

    Presentation {
        slide_size,
        slides,
        master_slides,
        styles,
        theme_colors: Default::default(),
        // ODF(fodp) 侧没有 OOXML <p:defaultTextStyle> 对应概念，全 None 兜底。
        default_text_style: Default::default(),
    }
}

/// office:automatic-styles 里的文字样式（family=text/presentation）
/// → StyleRegistry（char_styles + paragraph_styles）
fn parse_text_style_map(doc: &XNode) -> StyleRegistry {
    let mut registry = StyleRegistry::default();
    let auto = match doc.first_child_tag("office:automatic-styles") {
        Some(a) => a,
        None => return registry,
    };
    for style_node in auto.children_tag("style:style") {
        let family = style_node.attr("style:family").unwrap_or("");
        let name = match style_node.attr("style:name") {
            Some(n) => n.to_string(),
            None => continue,
        };
        let based_on = style_node.attr("style:parent-style-name").map(|s| s.to_string());

        match family {
            "text" => {
                if let Some(tp) = style_node.first_child_tag("style:text-properties") {
                    let font_size = tp.attr("fo:font-size").map(parse_pt);
                    let font_family = tp.attr("style:font-name").map(|s| s.to_string())
                        .or_else(|| tp.attr("fo:font-family").map(|s| s.to_string()));
                    let bold = tp.attr("fo:font-weight") == Some("bold");
                    let italic = tp.attr("fo:font-style") == Some("italic");
                    let underline = tp.attr("style:text-underline-style")
                        .map(|s| s != "none").unwrap_or(false);
                    let color = tp.attr("fo:color").and_then(parse_color);
                    registry.char_styles.insert(name.clone(), CharStyle {
                        name, based_on, font_size, font_family,
                        bold, italic, underline, color,
                        ..Default::default()
                    });
                }
            }
            "paragraph" | "presentation" => {
                let font_size = style_node
                    .first_child_tag("style:text-properties")
                    .and_then(|tp| tp.attr("fo:font-size"))
                    .map(parse_pt);
                let font_family = style_node
                    .first_child_tag("style:text-properties")
                    .and_then(|tp| tp.attr("style:font-name").or_else(|| tp.attr("fo:font-family")))
                    .map(|s| s.to_string());
                let bold = style_node
                    .first_child_tag("style:text-properties")
                    .and_then(|tp| tp.attr("fo:font-weight"))
                    .map(|s| s == "bold")
                    .unwrap_or(false);
                let italic = style_node
                    .first_child_tag("style:text-properties")
                    .and_then(|tp| tp.attr("fo:font-style"))
                    .map(|s| s == "italic")
                    .unwrap_or(false);
                registry.paragraph_styles.insert(name.clone(), ParagraphStyle {
                    name, based_on, font_size, font_family, bold, italic,
                    ..Default::default()
                });
            }
            _ => {}
        }
    }
    registry
}

/// "24pt" / "24" → f32（pt）
fn parse_pt(s: &str) -> f32 {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("pt") {
        n.trim().parse::<f32>().unwrap_or(18.0)
    } else {
        s.parse::<f32>().unwrap_or(18.0)
    }
}

/// office:automatic-styles 里的 style:style（family=graphic/drawing-page）
/// → HashMap<style_name, ShapeStyle>
fn parse_style_map(doc: &XNode) -> HashMap<String, ShapeStyle> {
    let mut map = HashMap::new();
    let auto = match doc.first_child_tag("office:automatic-styles") {
        Some(a) => a,
        None => return map,
    };
    for style_node in auto.children_tag("style:style") {
        let family = style_node.attr("style:family").unwrap_or("");
        if family != "graphic" && family != "drawing-page" && family != "presentation" {
            continue;
        }
        let name = match style_node.attr("style:name") {
            Some(n) => n.to_string(),
            None => continue,
        };
        if let Some(gp) = style_node.first_child_tag("style:graphic-properties") {
            map.insert(name, parse_graphic_properties(gp));
        }
    }
    map
}

/// style:graphic-properties → ShapeStyle
fn parse_graphic_properties(gp: &XNode) -> ShapeStyle {
    use roe_model::impress::{FillStyle, StrokeStyle, ShadowStyle};
    use roe_model::common::Color;

    // 填充
    let fill = match gp.attr("draw:fill") {
        Some("solid") => {
            let color = gp.attr("draw:fill-color")
                .and_then(parse_color)
                .unwrap_or(Color { r: 200, g: 220, b: 245, a: 255 });
            Some(FillStyle::Solid(color))
        }
        Some("none") => Some(FillStyle::None),
        Some("gradient") => {
            // PPTX-12 模型改造后的适配：ODF 侧渐变解析本次未纳入完整
            // 调研范围（ODF 的 draw:gradient 结构与 OOXML gradFill 不同，
            // 是独立的解析工作）。此处只做最小改动让代码继续编译通过，
            // 用 start/end 两色映射成等价的二色线性渐变（pos 0.0/1.0），
            // 语义与改造前完全一致，不丢失也不新增信息。角度固定读
            // draw:angle（ODF 单位是普通"度"，不需要额外换算），比改造前
            // 硬编码 0.0 更准确一点，顺手修了。
            let start = gp.attr("draw:start-color")
                .and_then(parse_color)
                .unwrap_or(Color { r: 200, g: 220, b: 245, a: 255 });
            let end = gp.attr("draw:end-color")
                .and_then(parse_color)
                .unwrap_or(Color { r: 100, g: 140, b: 200, a: 255 });
            let angle = gp.attr("draw:angle")
                .and_then(|s| s.trim_end_matches("deg").parse::<f32>().ok())
                .unwrap_or(0.0);
            Some(FillStyle::Gradient {
                stops: vec![
                    roe_model::common::GradientStop { pos: 0.0, color: start },
                    roe_model::common::GradientStop { pos: 1.0, color: end },
                ],
                kind: roe_model::common::GradientKind::Linear { angle },
                rotate_with_shape: true,
            })
        }
        _ => None,
    };

    // 描边
    let stroke = match gp.attr("draw:stroke") {
        Some("none") => None,
        _ => {
            // solid 或未指定，尝试读颜色和宽度
            let color = gp.attr("svg:stroke-color")
                .and_then(parse_color);
            let width = gp.attr("svg:stroke-width")
                .map(parse_measure)
                .unwrap_or(0.0);
            if let Some(color) = color {
                if width > 0.0 {
                    Some(StrokeStyle { color, width })
                } else {
                    None
                }
            } else {
                None
            }
        }
    };

    // 阴影
    let shadow = if gp.attr("draw:shadow") == Some("visible") {
        let color = gp.attr("draw:shadow-color")
            .and_then(parse_color)
            .unwrap_or(Color { r: 128, g: 128, b: 128, a: 180 });
        let offset_x = gp.attr("draw:shadow-offset-x").map(parse_measure).unwrap_or(3.0);
        let offset_y = gp.attr("draw:shadow-offset-y").map(parse_measure).unwrap_or(3.0);
        Some(ShadowStyle { color, offset_x, offset_y, blur: 0.0 })
    } else {
        None
    };

    ShapeStyle { fill, stroke, shadow, head_arrow: None, tail_arrow: None }
}



fn find_slide_size(doc: &XNode) -> (f32, f32) {
    let mut layouts: Vec<&XNode> = Vec::new();
    if let Some(auto) = doc.first_child_tag("office:automatic-styles") {
        auto.descendants_tag("style:page-layout", &mut layouts);
    }
    for layout in &layouts {
        if let Some(props) = layout.first_child_tag("style:page-layout-properties") {
            let w = props.attr("fo:page-width").map(parse_measure);
            let h = props.attr("fo:page-height").map(parse_measure);
            if let (Some(w), Some(h)) = (w, h) {
                return (w, h);
            }
        }
    }
    // 默认 4:3 兜底（pt，对应 28cm x 21cm）
    (793.70, 595.28)
}

// ════════════════════════════════════════════════════════════
// Slide
// ════════════════════════════════════════════════════════════

fn build_slide(page: &XNode, master_name_to_idx: &HashMap<String, usize>, style_map: &HashMap<String, ShapeStyle>) -> Slide {
    let master_idx = page
        .attr("draw:master-page-name")
        .and_then(|name| master_name_to_idx.get(name).copied());

    let mut id_map: HashMap<String, u32> = HashMap::new();
    let mut next_id = 1u32;
    let shapes = collect_shapes(page, &mut id_map, &mut next_id, style_map);

    let notes = page.first_child_tag("presentation:notes").and_then(|n| {
        let mut out = Vec::new();
        collect_notes_text(n, &mut out);
        let joined = out.join("\n");
        if joined.trim().is_empty() {
            None
        } else {
            Some(joined)
        }
    });

    let timing_root = page
        .children_tag("anim:par")
        .into_iter()
        .find(|n| n.attr("presentation:node-type") == Some("timing-root"));

    let (animation_tree, transition) = match timing_root {
        Some(root_node) => {
            let transition = find_slide_transition(root_node);
            let tree = parse_animation_node(root_node, &id_map).unwrap_or_default();
            (tree, transition)
        }
        None => (AnimationNode::default(), None),
    };

    Slide { layout_idx: None,
        shapes,
        master_idx,
        animation_tree,
        transition,
        notes,
        // PROOT-005: ODF/fodp 解析器本次不做背景解析（范围仅覆盖 OOXML/pptx），
        // None 如实表示"未解析"，非"确认无背景"。
        background: None,
        // PROOT-005 followup: 同上，ODF侧本次不提取layout装饰性形状。
        layout_shapes: vec![],
    }
}

fn collect_notes_text(node: &XNode, out: &mut Vec<String>) {
    for child in &node.children {
        if child.tag == "draw:frame" {
            if let Some(tb) = child.first_child_tag("draw:text-box") {
                for p in extract_paragraphs(tb, 0, "") {
                    let text: String = p.runs.iter().map(|r| r.text.as_str()).collect();
                    if !text.trim().is_empty() {
                        out.push(text);
                    }
                }
            }
        }
    }
}

// ════════════════════════════════════════════════════════════
// 形状（Shape）
// ════════════════════════════════════════════════════════════

const SHAPE_TAGS: &[&str] = &[
    "draw:frame",
    "draw:custom-shape",
    "draw:rect",
    "draw:ellipse",
    "draw:line",
    "draw:connector",
    "draw:polygon",
    "draw:polyline",
];

fn collect_shapes(parent: &XNode, id_map: &mut HashMap<String, u32>, next_id: &mut u32, style_map: &HashMap<String, ShapeStyle>) -> Vec<Shape> {
    let mut shapes = Vec::new();
    for child in &parent.children {
        match child.tag.as_str() {
            "presentation:notes" | "draw:page-thumbnail" | "office:forms" | "officeooo:annotation" => {
                continue;
            }
            "draw:g" => {
                // 编组：展开为扁平形状列表
                shapes.extend(collect_shapes(child, id_map, next_id, style_map));
            }
            tag if SHAPE_TAGS.contains(&tag) => {
                shapes.push(build_shape(child, id_map, next_id, style_map));
            }
            _ => {}
        }
    }
    shapes
}

fn build_shape(node: &XNode, id_map: &mut HashMap<String, u32>, next_id: &mut u32, style_map: &HashMap<String, ShapeStyle>) -> Shape {
    let id = *next_id;
    *next_id += 1;
    if let Some(xml_id) = node.attr("xml:id") {
        id_map.insert(xml_id.to_string(), id);
    }

    let position = Transform::from_rect(
        node.attr("svg:x").map(parse_measure).unwrap_or(0.0),
        node.attr("svg:y").map(parse_measure).unwrap_or(0.0),
        node.attr("svg:width").map(parse_measure).unwrap_or(0.0),
        node.attr("svg:height").map(parse_measure).unwrap_or(0.0),
    );

    let geometry = build_geometry(node);
    let content = build_content(node);

    Shape {
        id,
        geometry,
        position,
        style: node.attr("draw:style-name")
            .and_then(|n| style_map.get(n).cloned())
            .unwrap_or_default(),
        content,
        text_body: roe_model::impress::TextBodyProps::default(),
        end_cxn: None,
        st_cxn: None,
        // ODP(ODF Impress) 侧本次未涉及占位符身份解析（PPTX-3 只覆盖 PPTX/OOXML 侧），
        // 按明确约定不做无根据的假设，先填 None，ODF 占位符对照留待专项调研（roadmap 已列为下一轮方向）。
        placeholder: None,
    }
}

fn build_geometry(node: &XNode) -> ShapeGeometry {
    match node.tag.as_str() {
        "draw:ellipse" => ShapeGeometry::Ellipse,
        "draw:line" | "draw:connector" => ShapeGeometry::Line,
        "draw:rect" => ShapeGeometry::Rect,
        "draw:custom-shape" => {
            if let Some(eg) = node.first_child_tag("draw:enhanced-geometry") {
                match eg.attr("draw:type") {
                    Some("rectangle") => ShapeGeometry::Rect,
                    Some("ellipse") => ShapeGeometry::Ellipse,
                    Some(other) => ShapeGeometry::Custom(other.to_string()),
                    None => ShapeGeometry::Custom("custom-shape".to_string()),
                }
            } else {
                ShapeGeometry::Custom("custom-shape".to_string())
            }
        }
        "draw:frame" => ShapeGeometry::Rect,
        other => ShapeGeometry::Custom(other.to_string()),
    }
}

fn build_content(node: &XNode) -> ShapeContent {
    // draw:frame 的文本内容在 draw:text-box 里
    if let Some(text_box) = node.first_child_tag("draw:text-box") {
        let paras = extract_paragraphs(text_box, 0, "");
        return if paras.is_empty() {
            ShapeContent::Empty
        } else {
            ShapeContent::Text(paras)
        };
    }

    // draw:object：嵌入图表
    if let Some(obj_node) = node.first_child_tag("draw:object") {
        if let Some(chart) = parse_chart_from_xml(obj_node) {
            return ShapeContent::Chart(chart);
        }
    }

    // draw:image：解码 base64 内联图片数据
    if let Some(img_node) = node.first_child_tag("draw:image") {
        // office:binary-data 是 base64 编码的图片数据
        if let Some(b64_node) = img_node.first_child_tag("office:binary-data") {
            let b64 = b64_node.all_text();
            let b64_clean: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
            if !b64_clean.is_empty() {
                use base64::Engine as _;
                if let Ok(data) = base64::engine::general_purpose::STANDARD.decode(&b64_clean) {
                    // 根据文件头推断 mime type
                    let mime = if data.starts_with(b"\x89PNG") {
                        "image/png"
                    } else if data.starts_with(b"\xff\xd8") {
                        "image/jpeg"
                    } else if data.starts_with(b"GIF") {
                        "image/gif"
                    } else {
                        "image/png" // 兜底
                    }.to_string();
                    return ShapeContent::Image(roe_model::common::ImageData { data, mime, ..Default::default() });
                }
            }
        }
        // xlink:href 外部路径引用（不内联）：暂归 Empty
        return ShapeContent::Empty;
    }

    // draw:custom-shape 等形状可能直接含 text:p（不经 draw:text-box）
    let paras = extract_paragraphs(node, 0, "");
    if paras.is_empty() {
        ShapeContent::Empty
    } else {
        ShapeContent::Text(paras)
    }
}

// ════════════════════════════════════════════════════════════
// 文本内容（Paragraph / Run / List）
// ════════════════════════════════════════════════════════════

fn extract_paragraphs(node: &XNode, level: u8, list_id: &str) -> Vec<Paragraph> {
    let mut out = Vec::new();
    for child in &node.children {
        match child.tag.as_str() {
            "text:p" => {
                let style = StyleRef(child.attr("text:style-name").unwrap_or("").to_string());
                let runs = extract_runs(child);
                let list_info = if level > 0 {
                    Some(ListInfo {
                        list_id: list_id.to_string(),
                        level,
                        style_name: list_id.to_string(),
                    })
                } else {
                    None
                };
                out.push(Paragraph { style, runs, list_info, ..Default::default() });
            }
            "text:list" => {
                let new_list_id = child.attr("text:style-name").unwrap_or(list_id).to_string();
                for item in child.children_tag("text:list-item") {
                    out.extend(extract_paragraphs(item, level + 1, &new_list_id));
                }
            }
            _ => {}
        }
    }
    out
}

fn extract_runs(p: &XNode) -> Vec<Run> {
    let mut runs = Vec::new();
    if !p.text.is_empty() {
        runs.push(Run { text: p.text.clone(), char_style: None, ..Default::default() });
    }
    for child in &p.children {
        match child.tag.as_str() {
            "text:span" => {
                let text = child.all_text();
                if !text.is_empty() {
                    let style = child.attr("text:style-name").map(|s| StyleRef(s.to_string()));
                    runs.push(Run { text, char_style: style, ..Default::default() });
                }
            }
            "text:page-number" => runs.push(Run { text: "{page-number}".to_string(), char_style: None, ..Default::default() }),
            "presentation:date-time" => runs.push(Run { text: "{date-time}".to_string(), char_style: None, ..Default::default() }),
            "presentation:footer" => runs.push(Run { text: "{footer}".to_string(), char_style: None, ..Default::default() }),
            "presentation:header" => runs.push(Run { text: "{header}".to_string(), char_style: None, ..Default::default() }),
            "text:line-break" | "text:s" => runs.push(Run { text: " ".to_string(), char_style: None, ..Default::default() }),
            _ => {
                let text = child.all_text();
                if !text.is_empty() {
                    runs.push(Run { text, char_style: None, ..Default::default() });
                }
            }
        }
    }
    runs
}

// ════════════════════════════════════════════════════════════
// 动画树（AnimationNode）
// ════════════════════════════════════════════════════════════

fn parse_animation_node(node: &XNode, id_map: &HashMap<String, u32>) -> Option<AnimationNode> {
    match node.tag.as_str() {
        "anim:par" => {
            let node_type = node.attr("presentation:node-type");
            match node_type {
                Some("timing-root") => {
                    let children = node
                        .children
                        .iter()
                        .filter(|c| !is_slide_transition_wrapper(c))
                        .filter_map(|c| parse_animation_node(c, id_map))
                        .collect();
                    Some(AnimationNode::TimingRoot { children })
                }
                Some("interactive-sequence") => Some(build_interactive_sequence(node, id_map)),
                _ => {
                    let children = node
                        .children
                        .iter()
                        .filter_map(|c| parse_animation_node(c, id_map))
                        .collect();
                    Some(AnimationNode::Parallel {
                        children,
                        timing: build_timing(node),
                    })
                }
            }
        }
        "anim:seq" => {
            let node_type = node.attr("presentation:node-type");
            match node_type {
                Some("main-sequence") => {
                    let effects = node
                        .children
                        .iter()
                        .filter_map(|c| parse_animation_node(c, id_map))
                        .map(|n| AnimationEffect { node: n })
                        .collect();
                    Some(AnimationNode::MainSequence { effects })
                }
                Some("interactive-sequence") => Some(build_interactive_sequence(node, id_map)),
                _ => {
                    let children = node
                        .children
                        .iter()
                        .filter_map(|c| parse_animation_node(c, id_map))
                        .collect();
                    Some(AnimationNode::Sequence {
                        children,
                        timing: build_timing(node),
                    })
                }
            }
        }
        "anim:set" => Some(AnimationNode::AnimateSet {
            target: resolve_target(node, id_map),
            property: parse_property(node.attr("smil:attributeName")),
            to: parse_anim_value(node.attr("smil:to")),
            timing: build_timing(node),
        }),
        "anim:animate" => Some(AnimationNode::Animate {
            target: resolve_target(node, id_map),
            property: parse_property(node.attr("smil:attributeName")),
            timing: build_timing(node),
        }),
        "anim:animateTransform" => Some(AnimationNode::AnimateTransform {
            target: resolve_target(node, id_map),
            kind: parse_transform_kind(node),
            timing: build_timing(node),
        }),
        "anim:animateColor" => Some(AnimationNode::AnimateColor {
            target: resolve_target(node, id_map),
            from: node.attr("smil:from").and_then(parse_color).unwrap_or(black()),
            to: node.attr("smil:to").and_then(parse_color).unwrap_or(black()),
            timing: build_timing(node),
        }),
        "anim:animateMotion" => Some(AnimationNode::AnimateMotion {
            target: resolve_target(node, id_map),
            path: node.attr("svg:path").unwrap_or("").to_string(),
            timing: build_timing(node),
        }),
        // anim:transitionFilter：v1 不映射形状级转场效果（见文件头注释）
        // 幻灯片级转场由 find_slide_transition 单独处理
        "anim:transitionFilter" => None,
        _ => None,
    }
}

fn build_interactive_sequence(node: &XNode, id_map: &HashMap<String, u32>) -> AnimationNode {
    let trigger_shape_id = node
        .attr("presentation:group-id")
        .or_else(|| node.attr("draw:id"))
        .and_then(|s| id_map.get(s))
        .copied()
        .unwrap_or(0);
    let effects = node
        .children
        .iter()
        .filter_map(|c| parse_animation_node(c, id_map))
        .map(|n| AnimationEffect { node: n })
        .collect();
    AnimationNode::InteractiveSequence {
        trigger_shape_id,
        effects,
    }
}

fn resolve_target(node: &XNode, id_map: &HashMap<String, u32>) -> u32 {
    node.attr("smil:targetElement")
        .and_then(|id| id_map.get(id))
        .copied()
        .unwrap_or(0)
}

fn build_timing(node: &XNode) -> TimingSpec {
    TimingSpec {
        begin: parse_begin(node.attr("smil:begin")),
        duration: parse_duration(node.attr("smil:dur")),
        repeat: parse_repeat(node.attr("smil:repeatCount")),
        fill: parse_fill(node.attr("smil:fill")),
        node_type: parse_node_type(node.attr("presentation:node-type")),
        easing: parse_easing(node.attr("smil:calcMode")),
    }
}

fn parse_easing(s: Option<&str>) -> EasingFunction {
    match s {
        Some("linear")    => EasingFunction::Linear,
        Some("discrete")  => EasingFunction::Linear, // discrete 无插值，近似用 linear
        Some("paced")     => EasingFunction::Linear,
        // ODF 用 smil:calcMode，具体 easing 曲线在 anim:transitionFilter 属性里
        // 未来可扩展；当前默认 EaseInOut（PPT 最常见的动画曲线）
        _ => EasingFunction::EaseInOut,
    }
}

fn parse_node_type(s: Option<&str>) -> EffectNodeType {
    match s {
        Some("timing-root") => EffectNodeType::TimingRoot,
        Some("main-sequence") => EffectNodeType::MainSequence,
        Some("interactive-sequence") => EffectNodeType::InteractiveSequence,
        Some("on-click") => EffectNodeType::OnClick,
        Some("with-previous") => EffectNodeType::WithPrevious,
        Some("after-previous") => EffectNodeType::AfterPrevious,
        _ => EffectNodeType::AfterPrevious,
    }
}

fn parse_begin(s: Option<&str>) -> BeginCondition {
    match s {
        None => BeginCondition::Offset(0.0),
        Some("next") => BeginCondition::OnNext,
        Some("indefinite") => BeginCondition::Indefinite,
        Some(v) => {
            if let Some(num) = v.strip_suffix('s') {
                if let Ok(n) = num.parse::<f64>() {
                    return BeginCondition::Offset(n);
                }
            }
            // idN.begin / idN.end：跨动画节点引用，v1 未解析（见文件头注释）
            BeginCondition::Indefinite
        }
    }
}

fn parse_duration(s: Option<&str>) -> f64 {
    match s {
        None => 0.0,
        Some("indefinite") => -1.0,
        Some(v) => v
            .strip_suffix('s')
            .and_then(|n| n.parse::<f64>().ok())
            .unwrap_or(0.0),
    }
}

fn parse_repeat(s: Option<&str>) -> RepeatSpec {
    match s {
        None => RepeatSpec::None,
        Some("indefinite") => RepeatSpec::Indefinite,
        Some(v) => v.parse::<f64>().map(RepeatSpec::Count).unwrap_or(RepeatSpec::None),
    }
}

fn parse_fill(s: Option<&str>) -> FillMode {
    match s {
        Some("remove") => FillMode::Remove,
        Some("freeze") => FillMode::Freeze,
        Some("hold") => FillMode::Hold,
        Some("transition") => FillMode::Transition,
        Some("auto") => FillMode::Auto,
        _ => FillMode::Default,
    }
}

fn parse_property(s: Option<&str>) -> AnimProperty {
    match s {
        Some("visibility") => AnimProperty::Visibility,
        Some("opacity") => AnimProperty::Opacity,
        Some("x") | Some("svg:x") => AnimProperty::PositionX,
        Some("y") | Some("svg:y") => AnimProperty::PositionY,
        Some("fill-color") => AnimProperty::FillColor,
        Some("stroke-color") => AnimProperty::StrokeColor,
        Some("rotation") | Some("r") => AnimProperty::Rotation,
        Some(other) => AnimProperty::Custom(other.to_string()),
        None => AnimProperty::Custom(String::new()),
    }
}

fn parse_anim_value(s: Option<&str>) -> AnimValue {
    match s {
        Some("visible") => AnimValue::Bool(true),
        Some("hidden") => AnimValue::Bool(false),
        Some(v) if v.starts_with('#') => AnimValue::Color(parse_color(v).unwrap_or(black())),
        Some(v) => {
            if let Ok(n) = v.parse::<f64>() {
                AnimValue::Number(n)
            } else {
                AnimValue::Text(v.to_string())
            }
        }
        None => AnimValue::Text(String::new()),
    }
}

fn parse_transform_kind(node: &XNode) -> TransformKind {
    match node.attr("svg:type") {
        Some("rotate") => TransformKind::Rotate,
        Some("translate") => TransformKind::Translate,
        Some("skewX") => TransformKind::SkewX,
        Some("skewY") => TransformKind::SkewY,
        _ => TransformKind::Scale,
    }
}

/// 判断一个 anim:par 是否是"幻灯片转场包装节点"：
/// 唯一子节点是没有 smil:targetElement 的 anim:transitionFilter
fn is_slide_transition_wrapper(node: &XNode) -> bool {
    node.tag == "anim:par"
        && node.children.len() == 1
        && node.children[0].tag == "anim:transitionFilter"
        && !node.children[0].attrs.contains_key("smil:targetElement")
}

fn find_slide_transition(timing_root: &XNode) -> Option<SlideTransition> {
    for child in &timing_root.children {
        if is_slide_transition_wrapper(child) {
            let tf = &child.children[0];
            let kind = parse_transition_kind(tf.attr("smil:type"), tf.attr("smil:subtype"));
            let duration = parse_duration(tf.attr("smil:dur")).max(0.0);
            return Some(SlideTransition {
                kind,
                duration,
                auto_advance: None,
            });
        }
    }
    None
}

fn parse_transition_kind(t: Option<&str>, sub: Option<&str>) -> TransitionKind {
    match t {
        Some("fade") => TransitionKind::Fade,
        Some("wipe") | Some("barWipe") | Some("slideWipe") => TransitionKind::Wipe,
        Some("push") | Some("pushWipe") => TransitionKind::Push,
        Some("zoom") => TransitionKind::Zoom,
        Some("none") | None => TransitionKind::None,
        Some(other) => {
            let label = match sub {
                Some(s) => format!("{}-{}", other, s),
                None => other.to_string(),
            };
            TransitionKind::Custom(label)
        }
    }
}

// ════════════════════════════════════════════════════════════
// 通用辅助函数
// ════════════════════════════════════════════════════════════

fn black() -> Color {
    Color { r: 0, g: 0, b: 0, a: 255 }
}

fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim().strip_prefix('#')?;
    if s.len() == 6 {
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        return Some(Color { r, g, b, a: 255 });
    }
    None
}

/// 解析 ODF 度量值，统一转为 pt（f32）
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
        s.parse().unwrap_or(0.0)
    }
}

// ════════════════════════════════════════════════════════════
// 测试
// ════════════════════════════════════════════════════════════


// ════════════════════════════════════════════════════════════
// 图表解析
// ════════════════════════════════════════════════════════════

pub fn parse_chart_from_xml(obj_node: &XNode) -> Option<roe_model::impress::ChartModel> {
    use roe_model::impress::{ChartModel, ChartKind, ChartSeries};

    let chart_node = obj_node.first_child_tag("chart:chart")?;

    // chart:class → ChartKind
    let kind = match chart_node.attr("chart:class").unwrap_or("chart:bar") {
        "chart:bar"            => ChartKind::Bar,
        "chart:horizontal-bar" => ChartKind::HorizontalBar,
        "chart:line"           => ChartKind::Line,
        "chart:pie"            => ChartKind::Pie,
        "chart:scatter"        => ChartKind::Scatter,
        "chart:radar"          => ChartKind::Radar,
        _                      => ChartKind::Bar,
    };

    // chart:title
    let title = chart_node
        .first_child_tag("chart:title")
        .map(|t| t.all_text().trim().to_string())
        .filter(|s| !s.is_empty());

    // chart:local-table → x_labels 和 series data
    let mut x_labels: Vec<String> = Vec::new();
    let mut series_data: Vec<Vec<f64>> = Vec::new();
    let mut series_names: Vec<String> = Vec::new();

    if let Some(local_table) = chart_node.first_child_tag("chart:local-table") {
        if let Some(header_rows) = local_table.first_child_tag("table:table-header-rows") {
            if let Some(header_row) = header_rows.first_child_tag("table:table-row") {
                let cells: Vec<_> = header_row.children_tag("table:table-cell");
                for cell in cells.iter().skip(1) {
                    series_names.push(cell.all_text().trim().to_string());
                    series_data.push(Vec::new());
                }
            }
        }
        for row in local_table.children_tag("table:table-row") {
            let cells: Vec<_> = row.children_tag("table:table-cell");
            if cells.is_empty() { continue; }
            x_labels.push(cells[0].all_text().trim().to_string());
            for (i, cell) in cells.iter().skip(1).enumerate() {
                let val = cell.attr("office:value")
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.0);
                if i < series_data.len() {
                    series_data[i].push(val);
                }
            }
        }
    }

    if series_names.is_empty() {
        if let Some(plot_area) = chart_node.first_child_tag("chart:plot-area") {
            for s in plot_area.children_tag("chart:series") {
                let name = s.attr("chart:label-cell-address")
                    .unwrap_or("系列")
                    .to_string();
                series_names.push(name);
                series_data.push(Vec::new());
            }
        }
    }

    let series: Vec<ChartSeries> = series_names.into_iter().zip(series_data.into_iter())
        .map(|(name, data)| ChartSeries { name, data, color: None, as_line: false, trendline: false })
        .collect();

    Some(ChartModel {
        id: 0,
        kind, title, x_labels, series, x_pt: 0.0, y_pt: 0.0, width_pt: 300.0, height_pt: 300.0,
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
  xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
  xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"
  xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
  xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0"
  xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
  xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0"
  xmlns:anim="urn:oasis:names:tc:opendocument:xmlns:animation:1.0"
  xmlns:smil="urn:oasis:names:tc:opendocument:xmlns:smil-compatible:1.0"
  office:mimetype="application/vnd.oasis.opendocument.presentation">
 <office:automatic-styles>
  <style:page-layout style:name="PM1">
   <style:page-layout-properties fo:page-width="28cm" fo:page-height="15.75cm" style:print-orientation="landscape"/>
  </style:page-layout>
 </office:automatic-styles>
 <office:master-styles>
  <style:master-page style:name="Midnightblue" style:page-layout-name="PM1">
   <draw:frame draw:layer="backgroundobjects" svg:width="2cm" svg:height="1cm" svg:x="0cm" svg:y="0cm">
    <draw:text-box><text:p>Master Title</text:p></draw:text-box>
   </draw:frame>
  </style:master-page>
 </office:master-styles>
 <office:body>
  <office:presentation>
   <draw:page draw:name="page1" draw:master-page-name="Midnightblue">
    <draw:frame draw:layer="layout" svg:width="10cm" svg:height="2cm" svg:x="1cm" svg:y="1cm" presentation:class="title">
     <draw:text-box>
      <text:p><text:span text:style-name="T10">First Slide</text:span></text:p>
     </draw:text-box>
    </draw:frame>
   </draw:page>
   <draw:page draw:name="page2" draw:master-page-name="Midnightblue">
    <draw:frame draw:layer="layout" svg:width="10cm" svg:height="2cm" svg:x="1cm" svg:y="1cm" presentation:class="title" xml:id="id_title">
     <draw:text-box><text:p>Animated Title</text:p></draw:text-box>
    </draw:frame>
    <anim:par presentation:node-type="timing-root">
     <anim:par smil:begin="id1.begin">
      <anim:transitionFilter smil:dur="1s" smil:type="fade"/>
     </anim:par>
     <anim:seq presentation:node-type="main-sequence">
      <anim:par smil:begin="next">
       <anim:par smil:begin="0s" smil:fill="hold" presentation:node-type="on-click">
        <anim:set smil:begin="0s" smil:dur="0.001s" smil:fill="hold" smil:targetElement="id_title" smil:attributeName="visibility" smil:to="visible"/>
        <anim:transitionFilter smil:dur="0.5s" smil:targetElement="id_title" smil:type="dissolve"/>
       </anim:par>
      </anim:par>
     </anim:seq>
    </anim:par>
   </draw:page>
  </office:presentation>
 </office:body>
</office:document>"#;

    #[test]
    fn test_parse_basic_structure() {
        let pres = FodpParser::parse(SAMPLE).unwrap();

        assert_eq!(pres.slides.len(), 2);
        // 28.0cm / 15.75cm 换算为 pt（x28.3465）
        assert!((pres.slide_size.0 - 793.70).abs() < 0.05, "width={}", pres.slide_size.0);
        assert!((pres.slide_size.1 - 446.46).abs() < 0.05, "height={}", pres.slide_size.1);

        assert_eq!(pres.master_slides.len(), 1);
        assert_eq!(pres.master_slides[0].name, "Midnightblue");
        assert_eq!(pres.master_slides[0].shapes.len(), 1);

        let p1 = &pres.slides[0];
        assert_eq!(p1.master_idx, Some(0));
        assert_eq!(p1.shapes.len(), 1);

        let shape = &p1.shapes[0];
        // 1.0cm/1.0cm/10.0cm/2.0cm 换算为 pt（x28.3465）
        assert!((shape.position.x() - 28.35).abs() < 0.05);
        assert!((shape.position.y() - 28.35).abs() < 0.05);
        assert!((shape.position.w() - 283.47).abs() < 0.05);
        assert!((shape.position.h() - 56.69).abs() < 0.05);
        assert!(matches!(shape.geometry, ShapeGeometry::Rect));

        match &shape.content {
            ShapeContent::Text(paras) => {
                assert_eq!(paras.len(), 1);
                assert_eq!(paras[0].runs.len(), 1);
                assert_eq!(paras[0].runs[0].text, "First Slide");
                assert_eq!(paras[0].runs[0].char_style, Some(StyleRef("T10".to_string())));
            }
            other => panic!("unexpected content: {:?}", other),
        }
    }

    #[test]
    fn test_parse_animation_tree_and_transition() {
        let pres = FodpParser::parse(SAMPLE).unwrap();
        let p2 = &pres.slides[1];

        // 幻灯片转场（来自 timing-root 下的转场包装节点）
        let transition = p2.transition.as_ref().expect("expected slide transition");
        assert!(matches!(transition.kind, TransitionKind::Fade), "{:?}", transition.kind);
        assert!((transition.duration - 1.0).abs() < 0.001);

        // 动画树：TimingRoot -> [MainSequence]（转场包装节点被排除）
        let AnimationNode::TimingRoot { children } = &p2.animation_tree else {
            panic!("expected TimingRoot, got {:?}", p2.animation_tree);
        };
        assert_eq!(children.len(), 1, "transition wrapper should be excluded");

        let AnimationNode::MainSequence { effects } = &children[0] else {
            panic!("expected MainSequence, got {:?}", children[0]);
        };
        assert_eq!(effects.len(), 1);

        // effect -> Parallel(begin=next) -> Parallel(on-click) -> [AnimateSet]
        // (transitionFilter 带 targetElement 被跳过)
        let AnimationNode::Parallel { children: outer, .. } = &effects[0].node else {
            panic!("expected Parallel, got {:?}", effects[0].node);
        };
        assert_eq!(outer.len(), 1);

        let AnimationNode::Parallel { children: inner, timing } = &outer[0] else {
            panic!("expected Parallel, got {:?}", outer[0]);
        };
        assert!(matches!(timing.node_type, EffectNodeType::OnClick));
        assert_eq!(inner.len(), 1, "transitionFilter-with-target should be skipped");

        let AnimationNode::AnimateSet { target, property, to, .. } = &inner[0] else {
            panic!("expected AnimateSet, got {:?}", inner[0]);
        };
        assert_eq!(*target, 1, "should resolve xml:id=id_title -> shape id 1");
        assert!(matches!(property, AnimProperty::Visibility));
        assert!(matches!(to, AnimValue::Bool(true)));
    }
}
