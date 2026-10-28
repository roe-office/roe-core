use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use crate::common::Color;
use crate::writer::ScriptPosition;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StyleRegistry {
    pub paragraph_styles: HashMap<String, ParagraphStyle>,
    pub char_styles:      HashMap<String, CharStyle>,
    pub cell_styles:      HashMap<String, CellStyle>,
    /// text:list-style 定义，key 为 style:name（与 ListInfo.style_name 对应）
    #[serde(default)]
    pub list_styles:      HashMap<String, crate::writer::ListStyle>,
    #[serde(default)]
    pub doc_default_line_height: Option<f32>,
    #[serde(default)]
    pub doc_default_font_size: Option<f32>,
    #[serde(default)]
    pub doc_default_color: Option<crate::common::Color>,
    #[serde(default)]
    pub doc_default_space_before: Option<f32>,
    #[serde(default)]
    pub doc_default_space_after: Option<f32>,
    pub doc_default_indent_left: Option<f32>,
    pub doc_default_indent_first: Option<f32>,
    // WROOT-040-FONT：docDefaults(w:rPrDefault) 的兜底字体，参见
    // roe-parser/src/ooxml/docx.rs 对应字段注释。
    #[serde(default)]
    pub doc_default_font_family: Option<String>,
    #[serde(default)]
    pub doc_default_font_family_ascii: Option<String>,
}

/// WROOT-114 阶段二：段落单边边框（对应 w:pPr/w:pBdr 下的 w:left 等）。
/// 单位均为 pt（w:sz 是 1/8 磅，解析时已换算；w:space 本身就是磅）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ParaBorder {
    pub color:    crate::common::Color,
    pub width_pt: f32,
    pub space_pt: f32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParagraphStyle {
    pub name:         String,
    pub based_on:     Option<String>,
    pub font_size:    Option<f32>,
    pub font_family:  Option<String>,
    /// [WROOT-093小尾巴2] w:rFonts 的 w:ascii 候选，与 font_family（此前存
    /// east_asia 优先选出的结果）并存。用于纯 ASCII 文字场景下覆盖
    /// font_family——east_asia 字体（如中文字体）在纯拉丁文字上通常
    /// 视觉效果不对（字重/字宽/衬线风格与文档实际设计不符）。
    #[serde(default)]
    pub font_family_ascii: Option<String>,
    pub bold:         bool,
    pub italic:       bool,
    pub alignment:    Option<Alignment>,
    pub line_height:  Option<f32>,
    pub space_before: Option<f32>,
    pub space_after:  Option<f32>,
    pub indent_left:  Option<f32>,
    pub indent_first: Option<f32>,
    /// WROOT-079/WROOT-095 关联发现：段落样式（w:type="paragraph"）自带的
    /// w:rPr/w:color 此前完全没有字段承载，导致段落样式定义的颜色（常见于
    /// 标题/副标题等未显式引用具名字符样式的场景）在端到端渲染路径上
    /// 被静默丢弃，退化成 doc_default_color。
    pub color: Option<crate::common::Color>,
    /// style:list-style-name —— 段落样式自带的列表样式引用（不依赖
    /// <text:list> 标签包裹，常见于 outline/heading 自动编号）。
    #[serde(default)]
    pub list_style_name: Option<String>,
    /// style:list-level —— 配合 list_style_name 指定默认 level（1-based，
    /// 解析时转换为 0-based 存入 ListInfo）。
    #[serde(default)]
    pub list_level: Option<u8>,
    /// WROOT-114: 段落样式自带的段落底纹（w:pPr/w:shd 的 w:fill）。
    /// 铺满整个段落区域（含所有行），与 run 级 w:shd 是两套机制。
    #[serde(default)]
    pub shading: Option<crate::common::Color>,
    /// WROOT-114 阶段二：段落样式自带的左边框（w:pBdr/w:left）。
    #[serde(default)]
    pub border_left: Option<ParaBorder>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CharStyle {
    pub name:        String,
    pub based_on:    Option<String>,
    pub font_size:   Option<f32>,
    pub font_family: Option<String>,
    /// 语义同 ParagraphStyle.font_family_ascii，见该处注释。
    #[serde(default)]
    pub font_family_ascii: Option<String>,
    pub bold:        bool,
    pub italic:      bool,
    pub underline:   bool,
    pub color:       Option<Color>,
    pub strikethrough: Option<bool>,
    pub background:    Option<Color>,
    pub script:        Option<ScriptPosition>,
    /// WROOT-107: 字符基线垂直位移（w:position，pt，正上负下），见 Run.vertical_position。
    #[serde(default)]
    pub vertical_position: Option<f32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CellStyle {
    pub name:          String,
    pub font_size:     Option<f32>,
    pub bold:          bool,
    pub bg_color:      Option<Color>,
    pub text_color:    Option<Color>,
    pub number_format: Option<String>,
    pub alignment:     Option<Alignment>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Alignment {
    Left, Center, Right, Justify,
}

impl StyleRegistry {
    pub fn resolve_paragraph_style(&self, name: &str) -> ParagraphStyle {
        let mut visited = std::collections::HashSet::new();
        self.resolve_paragraph_style_inner(name, &mut visited)
    }

    /// DOCX-2：basedOn 循环引用防护。三方生产实现（docxjs/OnlyOffice/LOK）
    /// 均无真正防护（仅 LOK 有一处 debug-only 断言），此处补一个 visited-set
    /// 兜底：命中重复 style name 时判定为格式错误，直接返回默认样式，不做
    /// 精细错误定位（行业公认可以放低优先级的 edge case）。
    fn resolve_paragraph_style_inner(
        &self,
        name: &str,
        visited: &mut std::collections::HashSet<String>,
    ) -> ParagraphStyle {
        if !visited.insert(name.to_string()) {
            return ParagraphStyle::default();
        }
        // WROOT-040-FONT续修：抽出隐式基类构造逻辑为闭包，供"具名style查
        // 不到"和"具名style存在但无based_on"两种情况共用。此前只有后者会
        // 走 docDefaults 兜底，前者直接 return ParagraphStyle::default()，
        // 导致本地化文档（如 styleId 被本地化成非"Normal"等常见名字，实际
        // 解析器硬编码用"Normal"去查）在段落无pStyle时，doc_default_font_
        // family 等 docDefaults 兜底字段完全不生效。语义上"查无此 style"
        // 和"style存在但无based_on"都应视为到达继承链顶端，统一走同一套
        // 隐式基类起点，而不是两套不同的默认值来源。
        let implicit_base = || {
            let mut base = ParagraphStyle::default();
            base.line_height = self.doc_default_line_height;
            base.font_size = self.doc_default_font_size;
            base.space_before = self.doc_default_space_before;
            base.space_after = self.doc_default_space_after;
            base.color = self.doc_default_color;
            base.font_family = self.doc_default_font_family.clone();
            base.font_family_ascii = self.doc_default_font_family_ascii.clone();
            base
        };
        let Some(style) = self.paragraph_styles.get(name) else {
            return implicit_base();
        };
        let mut result = if let Some(parent) = &style.based_on {
            self.resolve_paragraph_style_inner(parent, visited)
        } else {
            implicit_base()
        };
        result.merge(style);
        result
    }

    pub fn resolve_char_style(&self, name: &str) -> CharStyle {
        let mut visited = std::collections::HashSet::new();
        self.resolve_char_style_inner(name, &mut visited)
    }

    /// DOCX-2：同上，char style 侧的循环引用防护。
    fn resolve_char_style_inner(
        &self,
        name: &str,
        visited: &mut std::collections::HashSet<String>,
    ) -> CharStyle {
        if !visited.insert(name.to_string()) {
            return CharStyle::default();
        }
        let Some(style) = self.char_styles.get(name) else {
            return CharStyle::default();
        };
        let mut result = if let Some(parent) = &style.based_on {
            self.resolve_char_style_inner(parent, visited)
        } else {
            // 无 based_on 时，以 docDefaults 的字号/颜色作为隐式基类起点，
            // 与 resolve_paragraph_style_inner 的处理方式保持一致。
            let mut base = CharStyle::default();
            base.font_size = self.doc_default_font_size;
            base.color = self.doc_default_color;
            base
        };
        result.merge(style);
        result
    }
}

impl ParagraphStyle {
    pub fn merge(&mut self, other: &ParagraphStyle) {
        if other.font_size.is_some()    { self.font_size    = other.font_size; }
        if other.font_family.is_some()  { self.font_family  = other.font_family.clone(); }
        if other.font_family_ascii.is_some() { self.font_family_ascii = other.font_family_ascii.clone(); }
        if other.bold                   { self.bold         = true; }
        if other.italic                 { self.italic       = true; }
        if other.alignment.is_some()    { self.alignment    = other.alignment; }
        if other.line_height.is_some()  { self.line_height  = other.line_height; }
        if other.space_before.is_some() { self.space_before = other.space_before; }
        if other.space_after.is_some()  { self.space_after  = other.space_after; }
        if other.indent_left.is_some()  { self.indent_left  = other.indent_left; }
        if other.indent_first.is_some() { self.indent_first = other.indent_first; }
        if other.color.is_some() { self.color = other.color; }
        if other.shading.is_some() { self.shading = other.shading; }
        if other.border_left.is_some() { self.border_left = other.border_left; }
    }
}

impl CharStyle {
    pub fn merge(&mut self, other: &CharStyle) {
        if other.font_size.is_some()   { self.font_size   = other.font_size; }
        if other.font_family.is_some() { self.font_family = other.font_family.clone(); }
        if other.font_family_ascii.is_some() { self.font_family_ascii = other.font_family_ascii.clone(); }
        if other.bold      { self.bold      = true; }
        if other.italic    { self.italic    = true; }
        if other.underline { self.underline = true; }
        if other.color.is_some() { self.color = other.color; }
        if other.strikethrough.is_some() { self.strikethrough = other.strikethrough; }
        if other.background.is_some()   { self.background   = other.background; }
        if other.script.is_some()       { self.script       = other.script; }
        if other.vertical_position.is_some() { self.vertical_position = other.vertical_position; }
    }
}
