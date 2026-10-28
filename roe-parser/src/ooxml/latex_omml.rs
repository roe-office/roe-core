//! LaTeX -> OMML（m:oMath）转换器，docx 写出专用。
//!
//! 是 docx.rs 中 parse_omml_to_latex 的逆运算，覆盖常用结构：
//! 分数/二项式、上下标、根号、求和积分(nary)、定界符(\left\right\middle)、
//! 函数名(\sin \lim ...)、重音、上下划线、上下括号、矩阵与对齐环境、
//! 希腊字母与常用符号、\text。
//! 无法识别的命令原样作为文本 run 输出（\name），保证不丢内容。
//! 已知局限：nary 与函数名的被作用对象只取其后的一个原子（含其上下标）。

#[derive(Clone)]
enum Item {
    Text(String),
    Xml(String),
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn esc_attr(s: &str) -> String {
    esc(s).replace('"', "&quot;")
}

const MATH_FONT: &str =
    r#"<w:rPr><w:rFonts w:ascii="Cambria Math" w:hAnsi="Cambria Math"/></w:rPr>"#;

fn seg_run(s: &str, italic: bool) -> String {
    let rpr = if italic { "" } else { r#"<m:rPr><m:sty m:val="p"/></m:rPr>"# };
    format!(
        r#"<m:r>{rpr}{MATH_FONT}<m:t xml:space="preserve">{}</m:t></m:r>"#,
        esc(s)
    )
}

/// 字母保持斜体（变量），数字/运算符/空格直立，避免 WPS 把数字也斜体化。
fn run(s: &str) -> String {
    let mut out = String::new();
    let mut seg = String::new();
    let mut seg_italic = false;
    for c in s.chars() {
        let it = c.is_alphabetic();
        if !seg.is_empty() && it != seg_italic {
            out.push_str(&seg_run(&seg, seg_italic));
            seg.clear();
        }
        seg_italic = it;
        seg.push(c);
    }
    if !seg.is_empty() {
        out.push_str(&seg_run(&seg, seg_italic));
    }
    out
}

fn run_plain(s: &str) -> String {
    format!(
        r#"<m:r><m:rPr><m:sty m:val="p"/></m:rPr><w:rPr><w:rFonts w:ascii="Cambria Math" w:hAnsi="Cambria Math"/></w:rPr><m:t xml:space="preserve">{}</m:t></m:r>"#,
        esc(s)
    )
}

fn run_nor(s: &str) -> String {
    format!(
        r#"<m:r><m:rPr><m:nor/></m:rPr><w:rPr><w:rFonts w:ascii="Cambria Math" w:hAnsi="Cambria Math"/></w:rPr><m:t xml:space="preserve">{}</m:t></m:r>"#,
        esc(s)
    )
}

fn item_xml(it: &Item) -> String {
    match it {
        Item::Text(s) => run(s),
        Item::Xml(x) => x.clone(),
    }
}

fn push_merge(items: &mut Vec<Item>, it: Item) {
    if let Item::Text(t) = &it {
        if let Some(Item::Text(prev)) = items.last_mut() {
            prev.push_str(t);
            return;
        }
    }
    items.push(it);
}

fn wrap_scripts(e: &str, sub: Option<String>, sup: Option<String>) -> String {
    match (sub, sup) {
        (Some(b), Some(p)) => format!(
            "<m:sSubSup><m:e>{e}</m:e><m:sub>{b}</m:sub><m:sup>{p}</m:sup></m:sSubSup>"
        ),
        (Some(b), None) => format!("<m:sSub><m:e>{e}</m:e><m:sub>{b}</m:sub></m:sSub>"),
        (None, Some(p)) => format!("<m:sSup><m:e>{e}</m:e><m:sup>{p}</m:sup></m:sSup>"),
        (None, None) => e.to_string(),
    }
}

fn wrap_d(beg: &str, end: &str, inner: &str) -> String {
    format!(
        r#"<m:d><m:dPr><m:begChr m:val="{}"/><m:endChr m:val="{}"/></m:dPr><m:e>{}</m:e></m:d>"#,
        esc_attr(beg),
        esc_attr(end),
        inner
    )
}

fn accent_chr(n: &str) -> Option<&'static str> {
    Some(match n {
        "hat" => "\u{0302}",
        "tilde" => "\u{0303}",
        "bar" => "\u{0304}",
        "vec" => "\u{20d7}",
        "dot" => "\u{0307}",
        "ddot" => "\u{0308}",
        "acute" => "\u{0301}",
        "grave" => "\u{0300}",
        "breve" => "\u{0306}",
        "check" => "\u{030c}",
        _ => return None,
    })
}

fn symbol(n: &str) -> Option<&'static str> {
    Some(match n {
        "alpha" => "α", "beta" => "β", "gamma" => "γ", "delta" => "δ",
        "epsilon" => "ϵ", "varepsilon" => "ε", "zeta" => "ζ", "eta" => "η",
        "theta" => "θ", "vartheta" => "ϑ", "iota" => "ι", "kappa" => "κ",
        "lambda" => "λ", "mu" => "μ", "nu" => "ν", "xi" => "ξ",
        "pi" => "π", "varpi" => "ϖ", "rho" => "ρ", "varrho" => "ϱ",
        "sigma" => "σ", "varsigma" => "ς", "tau" => "τ", "upsilon" => "υ",
        "phi" => "ϕ", "varphi" => "φ", "chi" => "χ", "psi" => "ψ", "omega" => "ω",
        "Gamma" => "Γ", "Delta" => "Δ", "Theta" => "Θ", "Lambda" => "Λ",
        "Xi" => "Ξ", "Pi" => "Π", "Sigma" => "Σ", "Upsilon" => "Υ",
        "Phi" => "Φ", "Psi" => "Ψ", "Omega" => "Ω",
        "cdot" => "·", "times" => "×", "div" => "÷", "pm" => "±", "mp" => "∓",
        "leq" | "le" => "≤", "geq" | "ge" => "≥", "neq" | "ne" => "≠",
        "approx" => "≈", "equiv" => "≡", "sim" => "∼", "propto" => "∝",
        "infty" => "∞", "partial" => "∂", "nabla" => "∇",
        "in" => "∈", "notin" => "∉", "subset" => "⊂", "supset" => "⊃",
        "subseteq" => "⊆", "supseteq" => "⊇", "cup" => "∪", "cap" => "∩",
        "forall" => "∀", "exists" => "∃", "emptyset" => "∅",
        "to" | "rightarrow" => "→", "leftarrow" => "←", "leftrightarrow" => "↔",
        "Rightarrow" => "⇒", "Leftarrow" => "⇐", "Leftrightarrow" => "⇔",
        "mapsto" => "↦", "ldots" | "dots" => "…", "cdots" => "⋯",
        "vdots" => "⋮", "ddots" => "⋱", "mid" => "∣",
        "langle" => "⟨", "rangle" => "⟩", "circ" => "∘", "bullet" => "∙",
        "star" => "⋆", "ll" => "≪", "gg" => "≫", "angle" => "∠",
        "perp" => "⊥", "parallel" => "∥", "neg" => "¬",
        "land" | "wedge" => "∧", "lor" | "vee" => "∨",
        "oplus" => "⊕", "otimes" => "⊗", "hbar" => "ℏ", "ell" => "ℓ",
        "Re" => "ℜ", "Im" => "ℑ", "aleph" => "ℵ", "prime" => "′",
        _ => return None,
    })
}

struct P {
    c: Vec<char>,
    i: usize,
}

fn inner(s: &str) -> String {
    let mut p = P { c: s.chars().collect(), i: 0 };
    p.parse_expr()
}

impl P {
    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(ch) if ch.is_whitespace()) {
            self.i += 1;
        }
    }

    fn peek_cmd(&self) -> Option<String> {
        if self.peek() != Some('\\') {
            return None;
        }
        let mut j = self.i + 1;
        match self.c.get(j) {
            None => Some(String::new()),
            Some(ch) if ch.is_ascii_alphabetic() => {
                let mut s = String::new();
                while let Some(ch) = self.c.get(j) {
                    if ch.is_ascii_alphabetic() {
                        s.push(*ch);
                        j += 1;
                    } else {
                        break;
                    }
                }
                Some(s)
            }
            Some(ch) => Some(ch.to_string()),
        }
    }

    fn take_cmd(&mut self) -> Option<String> {
        let n = self.peek_cmd()?;
        self.i += 1 + n.chars().count();
        Some(n)
    }

    fn at_stop(&mut self) -> bool {
        self.skip_ws();
        match self.peek() {
            None => true,
            Some('}') | Some('&') => true,
            Some('\\') => matches!(
                self.peek_cmd().as_deref(),
                Some("\\") | Some("right") | Some("middle") | Some("end")
            ),
            _ => false,
        }
    }

    fn parse_expr(&mut self) -> String {
        let mut items: Vec<Item> = Vec::new();
        while !self.at_stop() {
            match self.parse_scripted() {
                Some(it) => push_merge(&mut items, it),
                None => break,
            }
        }
        items.iter().map(item_xml).collect::<String>()
    }

    fn parse_scripts(&mut self) -> (Option<String>, Option<String>) {
        let mut sub: Option<String> = None;
        let mut sup: Option<String> = None;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('^') if sup.is_none() => {
                    self.i += 1;
                    sup = Some(self.parse_arg());
                }
                Some('_') if sub.is_none() => {
                    self.i += 1;
                    sub = Some(self.parse_arg());
                }
                _ => break,
            }
        }
        (sub, sup)
    }

    fn parse_scripted(&mut self) -> Option<Item> {
        if self.at_stop() {
            return None;
        }
        let base = self.parse_atom()?;
        let (sub, sup) = self.parse_scripts();
        if sub.is_none() && sup.is_none() {
            return Some(base);
        }
        let e = item_xml(&base);
        Some(Item::Xml(wrap_scripts(&e, sub, sup)))
    }

    fn parse_operand(&mut self) -> String {
        if self.at_stop() {
            return String::new();
        }
        self.parse_scripted().map(|i| item_xml(&i)).unwrap_or_default()
    }

    fn parse_arg(&mut self) -> String {
        self.skip_ws();
        if self.peek() == Some('{') {
            self.i += 1;
            let x = self.parse_expr();
            self.skip_ws();
            if self.peek() == Some('}') {
                self.i += 1;
            }
            x
        } else {
            match self.parse_atom() {
                Some(it) => item_xml(&it),
                None => String::new(),
            }
        }
    }

    fn parse_raw_group(&mut self) -> String {
        self.skip_ws();
        let mut s = String::new();
        if self.peek() == Some('{') {
            self.i += 1;
            let mut depth = 1;
            while let Some(c) = self.peek() {
                self.i += 1;
                if c == '{' {
                    depth += 1;
                } else if c == '}' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                s.push(c);
            }
        } else if let Some(c) = self.peek() {
            self.i += 1;
            s.push(c);
        }
        s
    }

    fn parse_delim_char(&mut self) -> String {
        self.skip_ws();
        match self.peek() {
            None => String::new(),
            Some('\\') => {
                let n = self.take_cmd().unwrap_or_default();
                match n.as_str() {
                    "{" | "lbrace" => "{".to_string(),
                    "}" | "rbrace" => "}".to_string(),
                    "langle" => "⟨".to_string(),
                    "rangle" => "⟩".to_string(),
                    "|" | "Vert" => "‖".to_string(),
                    "vert" | "lvert" | "rvert" => "|".to_string(),
                    "lfloor" => "⌊".to_string(),
                    "rfloor" => "⌋".to_string(),
                    "lceil" => "⌈".to_string(),
                    "rceil" => "⌉".to_string(),
                    _ => String::new(),
                }
            }
            Some('.') => {
                self.i += 1;
                String::new()
            }
            Some(c) => {
                self.i += 1;
                c.to_string()
            }
        }
    }

    fn parse_atom(&mut self) -> Option<Item> {
        self.skip_ws();
        let ch = self.peek()?;
        match ch {
            '{' => {
                self.i += 1;
                let x = self.parse_expr();
                self.skip_ws();
                if self.peek() == Some('}') {
                    self.i += 1;
                }
                Some(Item::Xml(x))
            }
            '}' | '&' => None,
            '\\' => self.parse_command(),
            '~' => {
                self.i += 1;
                Some(Item::Text(" ".to_string()))
            }
            // 无底数的 ^ / _：用空底数，由 parse_scripted 消费脚本
            '^' | '_' => Some(Item::Text(String::new())),
            c => {
                self.i += 1;
                Some(Item::Text(c.to_string()))
            }
        }
    }

    fn parse_command(&mut self) -> Option<Item> {
        let name = self.peek_cmd()?;
        if matches!(name.as_str(), "\\" | "right" | "middle" | "end") {
            return None;
        }
        self.take_cmd();
        let n = name.as_str();

        if let Some(c) = accent_chr(n) {
            let e = self.parse_arg();
            return Some(Item::Xml(format!(
                r#"<m:acc><m:accPr><m:chr m:val="{c}"/></m:accPr><m:e>{e}</m:e></m:acc>"#
            )));
        }

        match n {
            "frac" | "dfrac" | "tfrac" => {
                let a = self.parse_arg();
                let b = self.parse_arg();
                Some(Item::Xml(format!(
                    "<m:f><m:num>{a}</m:num><m:den>{b}</m:den></m:f>"
                )))
            }
            "binom" => {
                let a = self.parse_arg();
                let b = self.parse_arg();
                Some(Item::Xml(format!(
                    r#"<m:f><m:fPr><m:type m:val="noBar"/></m:fPr><m:num>{a}</m:num><m:den>{b}</m:den></m:f>"#
                )))
            }
            "sqrt" => {
                self.skip_ws();
                let deg = if self.peek() == Some('[') {
                    self.i += 1;
                    let mut s = String::new();
                    while let Some(c) = self.peek() {
                        self.i += 1;
                        if c == ']' {
                            break;
                        }
                        s.push(c);
                    }
                    Some(inner(&s))
                } else {
                    None
                };
                let e = self.parse_arg();
                Some(Item::Xml(match deg {
                    Some(d) => format!(
                        r#"<m:rad><m:radPr><m:degHide m:val="0"/></m:radPr><m:deg>{d}</m:deg><m:e>{e}</m:e></m:rad>"#
                    ),
                    None => format!(
                        r#"<m:rad><m:radPr><m:degHide m:val="1"/></m:radPr><m:deg/><m:e>{e}</m:e></m:rad>"#
                    ),
                }))
            }
            "overline" | "underline" => {
                let pos = if n == "overline" { "top" } else { "bot" };
                let e = self.parse_arg();
                Some(Item::Xml(format!(
                    r#"<m:bar><m:barPr><m:pos m:val="{pos}"/></m:barPr><m:e>{e}</m:e></m:bar>"#
                )))
            }
            "overbrace" | "underbrace" => {
                let (chr, pos, vj) = if n == "overbrace" {
                    ("⏞", "top", "bot")
                } else {
                    ("⏟", "bot", "top")
                };
                let e = self.parse_arg();
                Some(Item::Xml(format!(
                    r#"<m:groupChr><m:groupChrPr><m:chr m:val="{chr}"/><m:pos m:val="{pos}"/><m:vertJc m:val="{vj}"/></m:groupChrPr><m:e>{e}</m:e></m:groupChr>"#
                )))
            }
            "sum" | "prod" | "int" | "bigcup" | "bigcap" => {
                let (chr, loc) = match n {
                    "sum" => ("∑", "undOvr"),
                    "prod" => ("∏", "undOvr"),
                    "int" => ("∫", "subSup"),
                    "bigcup" => ("⋃", "undOvr"),
                    _ => ("⋂", "undOvr"),
                };
                let (sub, sup) = self.parse_scripts();
                let e = self.parse_operand();
                let mut pr = format!(r#"<m:chr m:val="{chr}"/><m:limLoc m:val="{loc}"/>"#);
                if sub.is_none() {
                    pr.push_str(r#"<m:subHide m:val="1"/>"#);
                }
                if sup.is_none() {
                    pr.push_str(r#"<m:supHide m:val="1"/>"#);
                }
                Some(Item::Xml(format!(
                    "<m:nary><m:naryPr>{pr}</m:naryPr><m:sub>{}</m:sub><m:sup>{}</m:sup><m:e>{e}</m:e></m:nary>",
                    sub.unwrap_or_default(),
                    sup.unwrap_or_default()
                )))
            }
            "sin" | "cos" | "tan" | "cot" | "sec" | "csc" | "arcsin" | "arccos"
            | "arctan" | "sinh" | "cosh" | "tanh" | "coth" | "log" | "ln" | "lg"
            | "exp" | "lim" | "limsup" | "liminf" | "max" | "min" | "sup" | "inf"
            | "det" | "gcd" | "deg" | "dim" | "ker" | "arg" | "Pr" | "hom" => {
                let mut fname = run_plain(n);
                self.skip_ws();
                if self.peek() == Some('_') {
                    self.i += 1;
                    let low = self.parse_arg();
                    let under = matches!(
                        n,
                        "lim" | "limsup" | "liminf" | "max" | "min" | "sup" | "inf" | "det" | "gcd" | "Pr"
                    );
                    fname = if under {
                        format!("<m:limLow><m:e>{fname}</m:e><m:lim>{low}</m:lim></m:limLow>")
                    } else {
                        format!("<m:sSub><m:e>{fname}</m:e><m:sub>{low}</m:sub></m:sSub>")
                    };
                }
                let e = self.parse_operand();
                Some(Item::Xml(format!(
                    "<m:func><m:fName>{fname}</m:fName><m:e>{e}</m:e></m:func>"
                )))
            }
            "left" => {
                let beg = self.parse_delim_char();
                let mut segs: Vec<String> = Vec::new();
                let mut sep = String::new();
                let mut end = String::new();
                loop {
                    segs.push(self.parse_expr());
                    match self.peek_cmd().as_deref() {
                        Some("middle") => {
                            self.take_cmd();
                            let d = self.parse_delim_char();
                            if sep.is_empty() {
                                sep = d;
                            }
                        }
                        Some("right") => {
                            self.take_cmd();
                            end = self.parse_delim_char();
                            break;
                        }
                        _ => break,
                    }
                }
                let sep_xml = if !sep.is_empty() && segs.len() > 1 {
                    format!(r#"<m:sepChr m:val="{}"/>"#, esc_attr(&sep))
                } else {
                    String::new()
                };
                let es: String = segs.iter().map(|s| format!("<m:e>{s}</m:e>")).collect();
                Some(Item::Xml(format!(
                    r#"<m:d><m:dPr><m:begChr m:val="{}"/><m:endChr m:val="{}"/>{sep_xml}</m:dPr>{es}</m:d>"#,
                    esc_attr(&beg),
                    esc_attr(&end)
                )))
            }
            "begin" => {
                let env = self.parse_raw_group();
                if env == "array" {
                    let _ = self.parse_raw_group();
                }
                let mut rows: Vec<Vec<String>> = Vec::new();
                let mut row: Vec<String> = Vec::new();
                loop {
                    row.push(self.parse_expr());
                    match self.peek_cmd().as_deref() {
                        Some("\\") => {
                            self.take_cmd();
                            rows.push(std::mem::take(&mut row));
                        }
                        Some("end") => {
                            self.take_cmd();
                            let _ = self.parse_raw_group();
                            break;
                        }
                        _ => {
                            if self.peek() == Some('&') {
                                self.i += 1;
                            } else {
                                break;
                            }
                        }
                    }
                }
                if !(row.len() == 1 && row[0].is_empty()) {
                    rows.push(row);
                }
                let is_matrix = matches!(
                    env.as_str(),
                    "matrix" | "pmatrix" | "bmatrix" | "Bmatrix" | "vmatrix" | "Vmatrix"
                        | "smallmatrix" | "array"
                );
                if is_matrix {
                    let mut m = String::from("<m:m>");
                    for r in &rows {
                        m.push_str("<m:mr>");
                        for cell in r {
                            m.push_str(&format!("<m:e>{cell}</m:e>"));
                        }
                        m.push_str("</m:mr>");
                    }
                    m.push_str("</m:m>");
                    let (b, e) = match env.as_str() {
                        "pmatrix" => ("(", ")"),
                        "bmatrix" => ("[", "]"),
                        "Bmatrix" => ("{", "}"),
                        "vmatrix" => ("|", "|"),
                        "Vmatrix" => ("‖", "‖"),
                        _ => ("", ""),
                    };
                    if b.is_empty() && e.is_empty() {
                        Some(Item::Xml(m))
                    } else {
                        Some(Item::Xml(wrap_d(b, e, &m)))
                    }
                } else {
                    let mut eq = String::from("<m:eqArr>");
                    for r in &rows {
                        eq.push_str(&format!("<m:e>{}</m:e>", r.concat()));
                    }
                    eq.push_str("</m:eqArr>");
                    if env == "cases" {
                        Some(Item::Xml(wrap_d("{", "", &eq)))
                    } else {
                        Some(Item::Xml(eq))
                    }
                }
            }
            "text" | "mbox" => {
                let s = self.parse_raw_group();
                Some(Item::Xml(run_nor(&s)))
            }
            "mathrm" | "operatorname" => {
                let s = self.parse_raw_group();
                Some(Item::Xml(run_plain(&s)))
            }
            "mathbf" | "mathit" | "mathbb" | "mathcal" | "mathsf" | "mathtt"
            | "boldsymbol" | "bm" => {
                let a = self.parse_arg();
                Some(Item::Xml(a))
            }
            "displaystyle" | "textstyle" | "!" => Some(Item::Text(String::new())),
            "," | ";" | ":" | " " | "quad" | "qquad" => Some(Item::Text(" ".to_string())),
            "{" | "}" | "%" | "$" | "#" | "_" | "&" => Some(Item::Text(n.to_string())),
            "|" => Some(Item::Text("‖".to_string())),
            _ => {
                if let Some(s) = symbol(n) {
                    Some(Item::Text(s.to_string()))
                } else {
                    Some(Item::Text(format!("\\{n}")))
                }
            }
        }
    }
}

/// 把 LaTeX 转成完整的 `<m:oMath>...</m:oMath>`（需要外层声明 xmlns:m）。
pub fn latex_to_omml(latex: &str) -> String {
    let mut p = P { c: latex.chars().collect(), i: 0 };
    let mut body = String::new();
    loop {
        body.push_str(&p.parse_expr());
        let ch = match p.peek() {
            None => break,
            Some(c) => c,
        };
        // 游离的结束符（多余的 } & \\ \right 等）：原样输出，保证前进
        if ch == '\\' {
            let n = p.take_cmd().unwrap_or_default();
            body.push_str(&run(&format!("\\{n}")));
        } else {
            p.i += 1;
            body.push_str(&run(&ch.to_string()));
        }
    }
    format!("<m:oMath>{body}</m:oMath>")
}

#[cfg(test)]
mod tests {
    use super::latex_to_omml;

    #[test]
    fn frac_and_text() {
        let s = latex_to_omml(r"x=\frac{a+b}{c+d}");
        assert!(s.starts_with("<m:oMath>") && s.ends_with("</m:oMath>"));
        assert!(s.contains("<m:f><m:num>"));
        assert!(s.contains(">x<"));
        assert!(s.contains(">a<"));
    }

    #[test]
    fn superscript_and_subscript() {
        assert!(latex_to_omml("x^2").contains("<m:sSup>"));
        assert!(latex_to_omml("x_i").contains("<m:sSub>"));
        assert!(latex_to_omml("x_i^2").contains("<m:sSubSup>"));
    }

    #[test]
    fn sqrt_with_and_without_degree() {
        assert!(latex_to_omml(r"\sqrt{x}").contains(r#"<m:degHide m:val="1"/>"#));
        assert!(latex_to_omml(r"\sqrt[3]{x}").contains(r#"<m:degHide m:val="0"/>"#));
    }

    #[test]
    fn nary_sum() {
        let s = latex_to_omml(r"\sum_{i=1}^{n} a_i");
        assert!(s.contains("<m:nary>") && s.contains("∑"));
        assert!(s.contains(">i<") && s.contains(">n<"));
    }

    #[test]
    fn left_right_delimiters() {
        let s = latex_to_omml(r"\left( a+b \right)");
        assert!(s.contains(r#"<m:begChr m:val="("/>"#));
        assert!(s.contains(r#"<m:endChr m:val=")"/>"#));
    }

    #[test]
    fn pmatrix_two_rows() {
        let s = latex_to_omml(r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}");
        assert_eq!(s.matches("<m:mr>").count(), 2);
        assert!(s.contains("<m:d>"));
    }

    #[test]
    fn unknown_command_is_kept_as_text() {
        // a42b2ff 之后字母与非字母拆进不同的 m:r，所以按"去掉标签后的纯文本"判断
        let xml = latex_to_omml(r"\foo x");
        let mut text = String::new();
        let mut in_tag = false;
        for ch in xml.chars() {
            match ch {
                '<' => in_tag = true,
                '>' => in_tag = false,
                c if !in_tag => text.push(c),
                _ => {}
            }
        }
        assert!(text.contains("\\foo"), "text={text}");
    }

    #[test]
    fn xml_is_escaped() {
        assert!(latex_to_omml("a<b").contains("&lt;"));
    }

    #[test]
    fn stray_closing_brace_does_not_hang() {
        let s = latex_to_omml("a}b");
        assert!(s.contains("}"));
    }
}
