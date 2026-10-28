use ratex_parser::{parser::Parser, parse_node::ParseNode};

pub fn latex_to_mathml(latex: &str) -> Result<String, String> {
    let mut parser = Parser::new(latex);
    let nodes = parser.parse().map_err(|e| format!("{:?}", e))?;
    let mut body = String::new();
    for node in &nodes { serialize_node(node, &mut body); }
    Ok(format!(
        r#"<math:math xmlns:math="http://www.w3.org/1998/Math/MathML"><math:mrow>{}</math:mrow></math:math>"#,
        body
    ))
}

pub fn estimate_size_from_mathml(mathml: &str) -> (f64, f64) {
    estimate_node(mathml)
}

/// 用RaTeX真实排版引擎计算公式尺寸（DOCX-8方案，替代count_tag估算法）。
///
/// 输入：LaTeX字符串 + 该公式所在位置的字号（pt）。
/// 输出：(width_pt, height_pt, depth_pt)。
///
/// 单位换算说明：RaTeX/ratex-layout内部的DisplayList.width/height/depth
/// 是em相对值（不依赖具体字号，是"字号的倍数"）。em的定义就是"1em=当前字号"，
/// 这是排版学通用定义，不是RaTeX特有的常数，所以换算只需要
/// 乘以调用方传入的实际字号，不需要用MathConstants::pt_per_em（那是
/// RaTeX内部字体度量表的校准基准，不是输出换算系数）。
///
/// MathStyle固定用Text（行内公式的通常样式）。当OMML→LaTeX转换层
/// 完成后，如果能区分m:oMathPara（独立公式）与裸m:oMath（行内公式），
/// 可以在上层按需传入MathStyle::Display，本函数签名预留了这个可能性
/// 但当前先用固定值，因为调用方目前还不存在（OMML转换层是DOCX-8的
/// 下一步主体任务，尚未开始）。
///
/// 错误处理：lex/parse失败返回Err（不panic），供调用方决定是否降级到
/// estimate_size_from_mathml作为兜底。layout()和to_display_list()
/// 在RaTeX原版中未见panic!/unwrap的证据（本函数未做catch_unwind，
/// 如果后续真实文档测试中发现panic案例，需要补充）。
pub fn latex_layout_size(latex: &str, font_size_pt: f64) -> Result<(f64, f64, f64), String> {
    let nodes = ratex_parser::parse(latex).map_err(|e| format!("{:?}", e))?;
    let options = ratex_layout::LayoutOptions::default();
    let layout_box = ratex_layout::layout(&nodes, &options);
    let display_list = ratex_layout::to_display_list(&layout_box);

    let width_pt = display_list.width * font_size_pt;
    let height_pt = display_list.height * font_size_pt;
    let depth_pt = display_list.depth * font_size_pt;

    Ok((width_pt, height_pt, depth_pt))
}


/// DOCX-8-v2(方案乙): 计算公式的绘制指令列表（DisplayList），供tile.rs
/// 直接遍历绘制，不需要在渲染阶段重新parse LaTeX（避免render_viewport
/// 每次重绘都重复解析的性能债，见ROE_DOCX_Progress交接文档）。
///
/// 返回的DisplayList坐标单位已经是pt（不是RaTeX原生的em），跟
/// latex_layout_size返回的width_pt/height_pt/depth_pt保持同一套单位
/// 体系——调用方（docx.rs）本来就要同时调这两个函数存进同一个Formula
/// 结构体，单位不统一容易在tile.rs画图时被搞混，所以在这里就把em→pt
/// 的换算做完，tile.rs侧只需要再乘一次pt→像素的scale。
///
/// 错误处理：同latex_layout_size，parse失败返回Err，不panic。
pub fn latex_to_display_list(latex: &str, font_size_pt: f64) -> Result<ratex_types::DisplayList, String> {
    let nodes = ratex_parser::parse(latex).map_err(|e| format!("{:?}", e))?;
    let options = ratex_layout::LayoutOptions::default();
    let layout_box = ratex_layout::layout(&nodes, &options);
    let display_list = ratex_layout::to_display_list(&layout_box);

    Ok(scale_display_list(&display_list, font_size_pt))
}

/// 把DisplayList从em单位（RaTeX原生输出）线性缩放成pt单位。
/// 所有DisplayItem的坐标/尺寸字段（含PathCommand内部坐标）统一乘以
/// font_size_pt——这是em的排版学定义本身（1em = 当前字号），不是
/// RaTeX特有换算，跟latex_layout_size里的换算逻辑同源。
fn scale_display_list(dl: &ratex_types::DisplayList, font_size_pt: f64) -> ratex_types::DisplayList {
    use ratex_types::{DisplayItem, PathCommand};
    let s = font_size_pt;

    let scale_path_command = |cmd: &PathCommand| -> PathCommand {
        match cmd {
            PathCommand::MoveTo { x, y } => PathCommand::MoveTo { x: x * s, y: y * s },
            PathCommand::LineTo { x, y } => PathCommand::LineTo { x: x * s, y: y * s },
            PathCommand::CubicTo { x1, y1, x2, y2, x, y } => PathCommand::CubicTo {
                x1: x1 * s, y1: y1 * s, x2: x2 * s, y2: y2 * s, x: x * s, y: y * s,
            },
            PathCommand::QuadTo { x1, y1, x, y } => PathCommand::QuadTo {
                x1: x1 * s, y1: y1 * s, x: x * s, y: y * s,
            },
            PathCommand::Close => PathCommand::Close,
        }
    };

    let items = dl.items.iter().map(|item| match item {
        DisplayItem::GlyphPath { x, y, scale, font, char_code, color } => DisplayItem::GlyphPath {
            x: x * s, y: y * s, scale: scale * s, font: font.clone(), char_code: *char_code, color: *color,
        },
        DisplayItem::Line { x, y, width, thickness, color, dashed } => DisplayItem::Line {
            x: x * s, y: y * s, width: width * s, thickness: thickness * s, color: *color, dashed: *dashed,
        },
        DisplayItem::Rect { x, y, width, height, color } => DisplayItem::Rect {
            x: x * s, y: y * s, width: width * s, height: height * s, color: *color,
        },
        DisplayItem::Path { x, y, commands, fill, color } => DisplayItem::Path {
            x: x * s, y: y * s,
            commands: commands.iter().map(&scale_path_command).collect(),
            fill: *fill, color: *color,
        },
    }).collect();

    ratex_types::DisplayList {
        items,
        width: dl.width * s,
        height: dl.height * s,
        depth: dl.depth * s,
    }
}


fn serialize_nodes(nodes: &[ParseNode], out: &mut String) {
    for node in nodes { serialize_node(node, out); }
}

fn serialize_node(node: &ParseNode, out: &mut String) {
    match node {
        ParseNode::Atom { text, family, .. } => {
            use ratex_parser::parse_node::AtomFamily;
            let tag = match family {
                AtomFamily::Bin | AtomFamily::Rel => "math:mo",
                AtomFamily::Open | AtomFamily::Close => "math:mo",
                _ => "math:mi",
            };
            out.push_str(&format!("<{}>{}</{}>", tag, escape_xml(text), tag));
        }
        ParseNode::MathOrd { text, .. } => {
            let resolved = resolve_symbol(text);
            out.push_str(&format!("<math:mi>{}</math:mi>", escape_xml(resolved)));
        }
        ParseNode::TextOrd { text, .. } => {
            let resolved = resolve_symbol(text);
            if is_operator(resolved) {
                out.push_str(&format!("<math:mo>{}</math:mo>", escape_xml(resolved)));
            } else {
                out.push_str(&format!("<math:mi>{}</math:mi>", escape_xml(resolved)));
            }
        }
        ParseNode::OpToken { text, .. } => {
            out.push_str(&format!("<math:mo>{}</math:mo>", escape_xml(text)));
        }
        ParseNode::SpacingNode { .. } => {
            out.push_str("<math:mspace width=\"0.2em\"/>");
        }
        ParseNode::OrdGroup { body, .. } => {
            out.push_str("<math:mrow>");
            serialize_nodes(body, out);
            out.push_str("</math:mrow>");
        }
        ParseNode::GenFrac { numer, denom, has_bar_line, left_delim, right_delim, .. } => {
            let mut num = String::new();
            let mut den = String::new();
            serialize_node(numer, &mut num);
            serialize_node(denom, &mut den);
            if left_delim.is_some() || right_delim.is_some() {
                let l = left_delim.as_deref().unwrap_or("(");
                let r = right_delim.as_deref().unwrap_or(")");
                out.push_str(&format!(
                    "<math:mo>{}</math:mo><math:mfrac linethickness=\"0\"><math:mrow>{}</math:mrow><math:mrow>{}</math:mrow></math:mfrac><math:mo>{}</math:mo>",
                    l, num, den, r
                ));
            } else if *has_bar_line {
                out.push_str(&format!(
                    "<math:mfrac><math:mrow>{}</math:mrow><math:mrow>{}</math:mrow></math:mfrac>",
                    num, den
                ));
            } else {
                out.push_str(&format!(
                    "<math:mfrac linethickness=\"0\"><math:mrow>{}</math:mrow><math:mrow>{}</math:mrow></math:mfrac>",
                    num, den
                ));
            }
        }
        ParseNode::Sqrt { body, index, .. } => {
            let mut body_str = String::new();
            serialize_node(body, &mut body_str);
            if let Some(idx) = index {
                let mut idx_str = String::new();
                serialize_node(idx, &mut idx_str);
                out.push_str(&format!(
                    "<math:mroot><math:mrow>{}</math:mrow><math:mrow>{}</math:mrow></math:mroot>",
                    body_str, idx_str
                ));
            } else {
                out.push_str(&format!(
                    "<math:msqrt><math:mrow>{}</math:mrow></math:msqrt>",
                    body_str
                ));
            }
        }
        ParseNode::SupSub { base, sup, sub, .. } => {
            if let Some(b) = base {
                if let ParseNode::Op { name, limits, .. } = b.as_ref() {
                    let op_str = op_to_str(name.as_deref().unwrap_or(""));
                    let mut sub_str = String::new();
                    let mut sup_str = String::new();
                    if let Some(s) = sub { serialize_node(s, &mut sub_str); }
                    if let Some(s) = sup { serialize_node(s, &mut sup_str); }
                    if *limits {
                        match (sub, sup) {
                            (Some(_), Some(_)) => out.push_str(&format!(
                                "<math:munderover><math:mo>{}</math:mo><math:mrow>{}</math:mrow><math:mrow>{}</math:mrow></math:munderover>",
                                op_str, sub_str, sup_str)),
                            (Some(_), None) => out.push_str(&format!(
                                "<math:munder><math:mo>{}</math:mo><math:mrow>{}</math:mrow></math:munder>",
                                op_str, sub_str)),
                            (None, Some(_)) => out.push_str(&format!(
                                "<math:mover><math:mo>{}</math:mo><math:mrow>{}</math:mrow></math:mover>",
                                op_str, sup_str)),
                            (None, None) => out.push_str(&format!("<math:mo>{}</math:mo>", op_str)),
                        }
                    } else {
                        match (sub, sup) {
                            (Some(_), Some(_)) => out.push_str(&format!(
                                "<math:msubsup><math:mo>{}</math:mo><math:mrow>{}</math:mrow><math:mrow>{}</math:mrow></math:msubsup>",
                                op_str, sub_str, sup_str)),
                            (Some(_), None) => out.push_str(&format!(
                                "<math:msub><math:mo>{}</math:mo><math:mrow>{}</math:mrow></math:msub>",
                                op_str, sub_str)),
                            (None, Some(_)) => out.push_str(&format!(
                                "<math:msup><math:mo>{}</math:mo><math:mrow>{}</math:mrow></math:msup>",
                                op_str, sup_str)),
                            (None, None) => out.push_str(&format!("<math:mo>{}</math:mo>", op_str)),
                        }
                    }
                    return;
                }
            }
            let mut base_str = String::new();
            if let Some(b) = base { serialize_node(b, &mut base_str); }
            match (sup, sub) {
                (Some(sp), None) => {
                    let mut sup_str = String::new();
                    serialize_node(sp, &mut sup_str);
                    out.push_str(&format!(
                        "<math:msup><math:mrow>{}</math:mrow><math:mrow>{}</math:mrow></math:msup>",
                        base_str, sup_str));
                }
                (None, Some(sb)) => {
                    let mut sub_str = String::new();
                    serialize_node(sb, &mut sub_str);
                    out.push_str(&format!(
                        "<math:msub><math:mrow>{}</math:mrow><math:mrow>{}</math:mrow></math:msub>",
                        base_str, sub_str));
                }
                (Some(sp), Some(sb)) => {
                    let mut sup_str = String::new();
                    let mut sub_str = String::new();
                    serialize_node(sp, &mut sup_str);
                    serialize_node(sb, &mut sub_str);
                    out.push_str(&format!(
                        "<math:msubsup><math:mrow>{}</math:mrow><math:mrow>{}</math:mrow><math:mrow>{}</math:mrow></math:msubsup>",
                        base_str, sub_str, sup_str));
                }
                (None, None) => out.push_str(&base_str),
            }
        }
        ParseNode::Op { name, .. } => {
            let op_str = op_to_str(name.as_deref().unwrap_or(""));
            out.push_str(&format!("<math:mo>{}</math:mo>", op_str));
        }
        ParseNode::LeftRight { body, left, right, .. } => {
            let mut body_str = String::new();
            serialize_nodes(body, &mut body_str);
            let l = delim_char(left);
            let r = delim_char(right);
            if l.is_empty() && r.is_empty() {
                out.push_str(&format!("<math:mrow>{}</math:mrow>", body_str));
            } else {
                out.push_str(&format!(
                    "<math:mrow><math:mo>{}</math:mo>{}<math:mo>{}</math:mo></math:mrow>",
                    l, body_str, r));
            }
        }
        ParseNode::Accent { label, base, .. } => {
            let chr = accent_to_char(label);
            let mut base_str = String::new();
            serialize_node(base, &mut base_str);
            out.push_str(&format!(
                "<math:mover accent=\"true\"><math:mrow>{}</math:mrow><math:mo>{}</math:mo></math:mover>",
                base_str, chr));
        }
        ParseNode::Overline { body, .. } => {
            let mut body_str = String::new();
            serialize_node(body, &mut body_str);
            out.push_str(&format!(
                "<math:mover accent=\"false\"><math:mrow>{}</math:mrow><math:mo>&#x00AF;</math:mo></math:mover>",
                body_str));
        }
        ParseNode::Underline { body, .. } => {
            let mut body_str = String::new();
            serialize_node(body, &mut body_str);
            out.push_str(&format!(
                "<math:munder accent=\"false\"><math:mrow>{}</math:mrow><math:mo>_</math:mo></math:munder>",
                body_str));
        }
        ParseNode::Array { body, .. } => {
            let mut rows = String::new();
            for row in body {
                let mut cells = String::new();
                for cell in row {
                    let mut cell_str = String::new();
                    serialize_node(cell, &mut cell_str);
                    cells.push_str(&format!("<math:mtd><math:mrow>{}</math:mrow></math:mtd>", cell_str));
                }
                rows.push_str(&format!("<math:mtr>{}</math:mtr>", cells));
            }
            out.push_str(&format!("<math:mtable>{}</math:mtable>", rows));
        }
        ParseNode::Styling { body, .. } | ParseNode::Phantom { body, .. } => {
            serialize_nodes(body, out);
        }
        ParseNode::Font { body, .. } => { serialize_node(body, out); }
        ParseNode::Text { body, .. } => {
            out.push_str("<math:mtext>");
            for n in body {
                if let ParseNode::TextOrd { text, .. } | ParseNode::MathOrd { text, .. } = n {
                    out.push_str(&escape_xml(text));
                }
            }
            out.push_str("</math:mtext>");
        }
        _ => {}
    }
}

fn is_operator(s: &str) -> bool {
    matches!(s, "+" | "-" | "×" | "÷" | "·" | "±" | "∓" | "=" | "<" | ">" |
               "≤" | "≥" | "≠" | "≈" | "≡" | "∼" | "∈" | "∉" | "⊂" | "⊃" |
               "∪" | "∩" | "→" | "←" | "⇒" | "⇐" | "↔" | "∀" | "∃")
}

fn resolve_symbol(text: &str) -> &str {
    match text {
        "\\infty" => "∞",
        "\\alpha" => "α", "\\beta" => "β", "\\gamma" => "γ",
        "\\delta" => "δ", "\\epsilon" | "\\varepsilon" => "ε",
        "\\zeta" => "ζ", "\\eta" => "η", "\\theta" => "θ",
        "\\iota" => "ι", "\\kappa" => "κ", "\\lambda" => "λ",
        "\\mu" => "μ", "\\nu" => "ν", "\\xi" => "ξ",
        "\\pi" => "π", "\\rho" => "ρ", "\\sigma" => "σ",
        "\\tau" => "τ", "\\upsilon" => "υ", "\\phi" | "\\varphi" => "φ",
        "\\chi" => "χ", "\\psi" => "ψ", "\\omega" => "ω",
        "\\Gamma" => "Γ", "\\Delta" => "Δ", "\\Theta" => "Θ",
        "\\Lambda" => "Λ", "\\Xi" => "Ξ", "\\Pi" => "Π",
        "\\Sigma" => "Σ", "\\Phi" => "Φ", "\\Psi" => "Ψ", "\\Omega" => "Ω",
        "\\pm" => "±", "\\mp" => "∓", "\\times" => "×", "\\div" => "÷",
        "\\cdot" => "·", "\\leq" => "≤", "\\geq" => "≥", "\\neq" => "≠",
        "\\approx" => "≈", "\\equiv" => "≡", "\\sim" => "∼",
        "\\in" => "∈", "\\notin" => "∉", "\\subset" => "⊂", "\\supset" => "⊃",
        "\\cup" => "∪", "\\cap" => "∩",
        "\\forall" => "∀", "\\exists" => "∃",
        "\\nabla" => "∇", "\\partial" => "∂", "\\emptyset" => "∅",
        "\\rightarrow" | "\\to" => "→", "\\leftarrow" => "←",
        "\\Rightarrow" => "⇒", "\\Leftarrow" => "⇐", "\\leftrightarrow" => "↔",
        "\\ldots" => "…", "\\cdots" => "⋯", "\\vdots" => "⋮", "\\ddots" => "⋱",
        other => other,
    }
}

fn op_to_str(name: &str) -> &str {
    match name {
        "\\int" => "∫", "\\iint" => "∬", "\\iiint" => "∭", "\\oint" => "∮",
        "\\sum" => "∑", "\\prod" => "∏", "\\coprod" => "∐",
        "\\bigcup" => "⋃", "\\bigcap" => "⋂",
        "\\lim" => "lim", "\\sup" => "sup", "\\inf" => "inf",
        "\\max" => "max", "\\min" => "min",
        _ => "∫",
    }
}

fn accent_to_char(label: &str) -> &str {
    match label {
        "\\hat" | "\\widehat" => "^",
        "\\tilde" | "\\widetilde" => "~",
        "\\bar" => "¯", "\\vec" => "→",
        "\\dot" => "˙", "\\ddot" => "¨",
        _ => "^",
    }
}

fn delim_char(delim: &str) -> &str {
    match delim {
        "(" => "(", ")" => ")",
        "[" => "[", "]" => "]",
        "\\{" | "\\lbrace" => "{", "\\}" | "\\rbrace" => "}",
        "|" => "|", "\\|" | "\\Vert" => "‖",
        "\\lfloor" => "⌊", "\\rfloor" => "⌋",
        "\\lceil" => "⌈", "\\rceil" => "⌉",
        "." => "",
        _ => delim,
    }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn count_tag(xml: &str, tag: &str) -> usize {
    xml.matches(&format!("<{}", tag)).count()
}

fn estimate_node(xml: &str) -> (f64, f64) {
    let base_char_w = 0.35_f64;
    let base_h = 0.75_f64;
    let n_frac   = count_tag(xml, "math:mfrac");
    let n_sqrt   = count_tag(xml, "math:msqrt") + count_tag(xml, "math:mroot");
    let n_uover  = count_tag(xml, "math:munderover") + count_tag(xml, "math:munder") + count_tag(xml, "math:mover");
    let n_subsup = count_tag(xml, "math:msubsup");
    let n_sup    = count_tag(xml, "math:msup");
    let n_sub    = count_tag(xml, "math:msub");
    let text_only: String = xml.chars()
        .scan(false, |in_tag, c| {
            if c == '<' { *in_tag = true; Some(None) }
            else if c == '>' { *in_tag = false; Some(None) }
            else if *in_tag { Some(None) }
            else { Some(Some(c)) }
        })
        .flatten()
        .filter(|c| !c.is_whitespace())
        .collect();
    let leaf_chars = text_only.chars().count().max(1);
    let w_raw = (leaf_chars as f64) * base_char_w;
    let w_frac_discount = (n_frac as f64) * base_char_w * 1.2;
    let w_script_discount = ((n_uover + n_subsup) as f64) * base_char_w * 0.4;
    let w = (w_raw - w_frac_discount - w_script_discount).max(leaf_chars as f64 * 0.22).max(0.35);
    let h = base_h
        + (n_frac as f64) * base_h * 0.85
        + (n_sqrt as f64) * base_h * 0.20
        + (n_uover as f64) * base_h * 0.65
        + (n_subsup as f64) * base_h * 0.45
        + (n_sup as f64) * base_h * 0.22
        + (n_sub as f64) * base_h * 0.12;
    (w.min(12.0), h.min(6.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mathml_frac() {
        let r = latex_to_mathml(r"\frac{1}{2}").unwrap();
        assert!(r.contains("math:mfrac"));
    }

    #[test]
    fn mathml_integral() {
        let r = latex_to_mathml(r"\int_0^\infty e^{-x} dx").unwrap();
        assert!(r.contains("math:munderover") || r.contains("math:msubsup") || r.contains("math:msub"));
    }

    #[test]
    fn mathml_sqrt() {
        let r = latex_to_mathml(r"\sqrt{\frac{a^2+b^2}{c}}").unwrap();
        assert!(r.contains("math:msqrt"));
    }

    #[test]
    fn mathml_simple_var() {
        let r = latex_to_mathml(r"E = mc^2").unwrap();
        assert!(r.contains("math:math"));
    }

    #[test]
    fn estimate_frac_taller() {
        let mathml = latex_to_mathml(r"\frac{1}{2}").unwrap();
        let (_, h) = estimate_size_from_mathml(&mathml);
        assert!(h > 0.75, "frac should be taller than base height");
    }

    #[test]
    fn estimate_simple_positive_size() {
        let mathml = latex_to_mathml(r"x").unwrap();
        let (w, h) = estimate_size_from_mathml(&mathml);
        assert!(w > 0.0 && h > 0.0);
    }

    // ---- latex_layout_size (DOCX-8 真实排版尺寸方案) ----

    #[test]
    fn layout_size_simple_var_positive() {
        let (w, h, d) = latex_layout_size("x", 12.0).unwrap();
        assert!(w > 0.0, "width应为正数, 实际: {}", w);
        assert!(h > 0.0, "height应为正数, 实际: {}", h);
        assert!(d >= 0.0, "depth应非负, 实际: {}", d);
    }

    #[test]
    fn layout_size_scales_with_font_size() {
        // 同一个公式，字号翻倍，尺寸应该也翻倍（因为是em相对值 * font_size_pt的线性关系）
        let (w1, h1, _) = latex_layout_size(r"\frac{1}{2}", 12.0).unwrap();
        let (w2, h2, _) = latex_layout_size(r"\frac{1}{2}", 24.0).unwrap();
        assert!((w2 - 2.0 * w1).abs() < 1e-6, "宽度应精确翻倍: w1={}, w2={}", w1, w2);
        assert!((h2 - 2.0 * h1).abs() < 1e-6, "高度应精确翻倍: h1={}, h2={}", h1, h2);
    }

    #[test]
    fn layout_size_frac_taller_than_single_char() {
        // 分数的高度(height+depth)应该明显大于单个字符，因为要容纳分子分母
        let (_, h_frac, d_frac) = latex_layout_size(r"\frac{1}{2}", 12.0).unwrap();
        let (_, h_char, d_char) = latex_layout_size("x", 12.0).unwrap();
        assert!(
            h_frac + d_frac > h_char + d_char,
            "分数总高度应大于单字符: frac={}, char={}",
            h_frac + d_frac, h_char + d_char
        );
    }

    #[test]
    fn layout_size_nested_frac_taller_than_flat() {
        // 嵌套分数 vs 并列分数：嵌套应该更高（印证调研文档指出的count_tag老方法的失真点：
        // 并列3个mfrac与嵌套3层mfrac，count_tag计数相同但真实高度应完全不同）
        let nested = r"\frac{1}{\frac{1}{\frac{1}{2}}}";
        let flat = r"\frac{1}{2} + \frac{1}{2} + \frac{1}{2}";
        let (_, h_nested, d_nested) = latex_layout_size(nested, 12.0).unwrap();
        let (_, h_flat, d_flat) = latex_layout_size(flat, 12.0).unwrap();
        assert!(
            h_nested + d_nested > h_flat + d_flat,
            "嵌套分数应比并列分数更高: nested={}, flat={}",
            h_nested + d_nested, h_flat + d_flat
        );
    }

    #[test]
    fn layout_size_invalid_latex_returns_err() {
        // 格式错误的LaTeX应该返回Err而不是panic
        let result = latex_layout_size(r"\frac{1}", 12.0);
        assert!(result.is_err(), "不完整的\\frac应该返回Err");
    }

    #[test]
    fn layout_size_sqrt_taller_than_flat_char() {
        let (_, h_sqrt, d_sqrt) = latex_layout_size(r"\sqrt{x}", 12.0).unwrap();
        let (_, h_char, d_char) = latex_layout_size("x", 12.0).unwrap();
        assert!(
            h_sqrt + d_sqrt >= h_char + d_char,
            "根号应不小于裸字符高度: sqrt={}, char={}",
            h_sqrt + d_sqrt, h_char + d_char
        );
    }
}
