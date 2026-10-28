//! FontRegistry：封装 cosmic-text FontSystem，支持桌面/WASM 两种加载方式

use std::collections::HashMap;
use cosmic_text::FontSystem;

/// 行高与字号的度量组合。原为 cosmic_text::Metrics 的本地替代——
/// 该类型在 cosmic-text 里本来就只有 font_size/line_height 两个字段，
/// Parley 迁移后 shaping 已不走 cosmic-text，改用本地定义避免残留依赖。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub font_size: f32,
    pub line_height: f32,
}

impl Metrics {
    pub const fn new(font_size: f32, line_height: f32) -> Self {
        Self { font_size, line_height }
    }
}

/// shape 缓存的 key：行文字 + 该行的 run 样式分段。
/// D2 改造：原先假设"一行只有一套字号/粗体/斜体"（单一 font_size_bits/
/// bold/italic 三个字段），D3/D4 要支持一行内多个 run 各自样式不同，
/// 故 key 改为 run_styles 列表：每项 (start, end, font_size_bits, bold, italic)，
/// start/end 是该 run 片段在 text 内的字节范围。
/// 长度为1、范围覆盖全行的 Vec，等价于原先"整行一套样式"的语义，
/// 旧调用方式（shape_line 仍接收单一套参数，D4 才改）通过这种方式构造
/// 兼容 key，不影响当前行为。
/// 注意：Color/background 等不影响 glyph 形状与位置的样式字段，不参与
/// 此 key（缓存命中与否只取决于会改变 shaping 结果的字段）。
#[derive(Hash, PartialEq, Eq, Clone)]
pub struct ShapeCacheKey {
    pub text: String,
    pub run_styles: Vec<(usize, usize, u32, bool, bool)>,
}

/// shape 缓存的 value：(start, end, x, w) glyph 列表
pub type ShapeCacheValue = Vec<(usize, usize, f32, f32)>;

/// shape 缓存的容量上限（条目数）。见 ROE_DOCX_Progress_v93.md
/// 三、3.1 第4条：因 shape_line 改造后 Layout 对象体积远大于原
/// (start,end,x,width) 元组，需要 LRU 淘汰机制防止无界增长。
/// 500 为工程估算值（单行文字 shape 结果通常几 KB 级别，500 条
/// 对应最坏情况几 MB，桌面应用可接受），非精确实测（实测受阻于
/// RunBrush/layout_context 为 pub(crate)，见 v93 三、3.1 第4条）。
pub const SHAPE_CACHE_CAPACITY: usize = 500;

/// 简易 LRU 缓存，专用于 shape_cache。手写而非引入 lru crate
/// （roe-writer 依赖里没有 lru，见 v93 三、3.2），API 对齐现有
/// HashMap 用法（get/insert/clear/is_empty），使 line_shape.rs
/// 调用方无需改动调用方式。
///
/// 实现方式：HashMap 存数据，VecDeque 记录访问顺序（队头最旧，
/// 队尾最新）。条目数上限 500，规模小，用 O(n) 线性 scan 更新
/// 顺序即可，不需要侵入式链表或 unsafe。
pub struct LruShapeCache {
    map: HashMap<ShapeCacheKey, ShapeCacheValue>,
    order: std::collections::VecDeque<ShapeCacheKey>,
    capacity: usize,
}

impl LruShapeCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: std::collections::VecDeque::new(),
            capacity,
        }
    }

    /// 命中时将该 key 移到队尾（标记为最近使用）。
    pub fn get(&mut self, key: &ShapeCacheKey) -> Option<&ShapeCacheValue> {
        if self.map.contains_key(key) {
            if let Some(pos) = self.order.iter().position(|k| k == key) {
                let k = self.order.remove(pos).unwrap();
                self.order.push_back(k);
            }
            self.map.get(key)
        } else {
            None
        }
    }

    /// 插入新条目；若已存在则更新值并刷新为最近使用；超出容量时
    /// 淘汰队头（最久未使用）条目。
    pub fn insert(&mut self, key: ShapeCacheKey, value: ShapeCacheValue) {
        if self.map.contains_key(&key) {
            if let Some(pos) = self.order.iter().position(|k| k == &key) {
                self.order.remove(pos);
            }
        } else if self.map.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, value);
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

pub struct FontRegistry {
    pub font_system: FontSystem,
    pub font_context: parley::FontContext,
    pub(crate) layout_context: parley::LayoutContext<crate::tile::RunBrush>,
    pub default_font_size: f32,
    pub default_line_height: f32,
    /// 行级 shape 结果缓存（key = 文字+样式，value = glyph 列表）。
    /// 拖拽/hit_test 场景下同一行会被反复 shape，缓存后降为 O(1)。
    /// 文档内容变更时（invalidate_writer_layout_cache）一并清空。
    pub shape_cache: LruShapeCache,
}

impl FontRegistry {
    /// 桌面端：加载系统字体
    pub fn new_with_system_fonts() -> Self {
        let mut registry = Self {
            font_system: FontSystem::new(),
            font_context: parley::FontContext::new(),
            layout_context: parley::LayoutContext::new(),
            default_font_size: 12.0,
            default_line_height: 16.0,
            shape_cache: LruShapeCache::new(SHAPE_CACHE_CAPACITY),
        };
        // DOCX-8公式渲染: 自动加载19个KaTeX字体，所有调用方（render_one/
        // render_docx_batch等）自动获得公式渲染能力，不需要每处手动调用。
        registry.load_katex_fonts();
        registry
    }

    /// WASM 端：空 FontSystem，之后通过 register_font_blob 注入
    pub fn new_empty() -> Self {
        Self {
            font_system: FontSystem::new(),
            font_context: parley::FontContext::new(),
            layout_context: parley::LayoutContext::new(),
            default_font_size: 12.0,
            default_line_height: 16.0,
            shape_cache: LruShapeCache::new(SHAPE_CACHE_CAPACITY),
        }
    }

    /// WASM 字体注入接口
    pub fn register_font_blob(&mut self, data: Vec<u8>) {
        self.font_system.db_mut().load_font_data(data);
    }

    /// DOCX-8公式渲染: 加载19个KaTeX字体（vendor自RaTeX/fonts，OFL-1.1协议，
    /// 见roe-writer/assets/katex_fonts/目录）。用include_bytes!编译期内嵌，
    /// 不依赖运行时文件路径（桌面/WASM场景都能用，跟register_font_blob走
    /// 同一条底层API：font_system.db_mut().load_font_data）。
    ///
    /// 字体族名约定：文件名去掉"KaTeX_"前缀、去掉".ttf"后缀，如
    /// "KaTeX_Main-Regular.ttf" -> family name "Main-Regular"，与
    /// ratex_font::FontId::as_str()产出的字符串（同时也是
    /// DisplayItem::GlyphPath.font字段的值）完全一致，不需要额外映射表
    /// （已核实：RaTeX/crates/ratex-font/src/font_id.rs的as_str()实现，
    /// 字符串跟ttf文件名前缀部分逐一对应）。cosmic-text后续通过
    /// Family::Name("Main-Regular")即可选中对应字体。
    pub fn load_katex_fonts(&mut self) {
        const KATEX_FONTS: &[&[u8]] = &[
            include_bytes!("../assets/katex_fonts/KaTeX_AMS-Regular.ttf"),
            include_bytes!("../assets/katex_fonts/KaTeX_Caligraphic-Bold.ttf"),
            include_bytes!("../assets/katex_fonts/KaTeX_Caligraphic-Regular.ttf"),
            include_bytes!("../assets/katex_fonts/KaTeX_Fraktur-Bold.ttf"),
            include_bytes!("../assets/katex_fonts/KaTeX_Fraktur-Regular.ttf"),
            include_bytes!("../assets/katex_fonts/KaTeX_Main-Bold.ttf"),
            include_bytes!("../assets/katex_fonts/KaTeX_Main-BoldItalic.ttf"),
            include_bytes!("../assets/katex_fonts/KaTeX_Main-Italic.ttf"),
            include_bytes!("../assets/katex_fonts/KaTeX_Main-Regular.ttf"),
            include_bytes!("../assets/katex_fonts/KaTeX_Math-BoldItalic.ttf"),
            include_bytes!("../assets/katex_fonts/KaTeX_Math-Italic.ttf"),
            include_bytes!("../assets/katex_fonts/KaTeX_SansSerif-Bold.ttf"),
            include_bytes!("../assets/katex_fonts/KaTeX_SansSerif-Italic.ttf"),
            include_bytes!("../assets/katex_fonts/KaTeX_SansSerif-Regular.ttf"),
            include_bytes!("../assets/katex_fonts/KaTeX_Script-Regular.ttf"),
            include_bytes!("../assets/katex_fonts/KaTeX_Size1-Regular.ttf"),
            include_bytes!("../assets/katex_fonts/KaTeX_Size2-Regular.ttf"),
            include_bytes!("../assets/katex_fonts/KaTeX_Size3-Regular.ttf"),
            include_bytes!("../assets/katex_fonts/KaTeX_Size4-Regular.ttf"),
            include_bytes!("../assets/katex_fonts/KaTeX_Typewriter-Regular.ttf"),
        ];
        for font_bytes in KATEX_FONTS {
            self.font_system.db_mut().load_font_data(font_bytes.to_vec());
            // Parley 迁移（tile_formula.rs 前置调研）：cosmic-text 的 db_mut()
            // 和 Parley 的 font_context.collection 是两套独立字体注册表，
            // 必须分别加载同一份字体数据。register_fonts 从字体文件内部
            // name table 解析出族名（不是从文件名推导），返回值只在这里
            // 用于调试确认注册结果，实际引用字体走 Family::named(字符串)。
            let blob = parley::fontique::Blob::new(std::sync::Arc::new(font_bytes.to_vec()));
            self.font_context.collection.register_fonts(blob, None);
        }
    }

    /// 统一入口：构造一个已加载19个KaTeX字体的 parley::FontContext，
    /// 供任何需要正确渲染公式的调用方使用。
    ///
    /// 背景（2026-09-14 LaTeX公式渲染回归排查）：此前 roe-api 侧多个
    /// 调用点（roe_render_writer_viewport 等FFI入口、render_docx_batch.rs、
    /// render_pptx.rs）各自裸建 parley::FontContext::new()，从未加载过
    /// KaTeX 字体，导致公式渲染 fallback 到系统字体，出现挤压变形、
    /// 数字错位、Σ字号过小等现象。只有 render_one.rs / render_with_
    /// comments.rs 两个 example 手写了等价的桥接逻辑（构造
    /// FontRegistry::new_with_system_fonts() 再 mem::replace 偷出
    /// font_context），但这段逻辑从未下沉进库本身，属于"多头维护、
    /// 其中一头忘了更新"的分叉隐患。此函数是唯一权威实现，任何需要
    /// 给 WriterTileRenderer::new_with_fonts() 传参的地方，都应该调
    /// 用这个函数，不要再手写 FontRegistry::new_with_system_fonts()
    /// + mem::replace 这段桥接代码，也不要直接用 parley::FontContext::new()。
    pub fn build_font_context_with_katex() -> parley::FontContext {
        let mut registry = Self::new_with_system_fonts();
        std::mem::replace(&mut registry.font_context, parley::FontContext::new())
    }

    /// line_height_multiplier: 行高相对字号的倍数（来自docx.rs解析的
    /// w:line/240.0，例如w:line="276"对应1.15）。调用方没有明确倍数时
    /// （例如尚未接入该概念的旧调用点）应传1.3保持原有行为不变
    /// （债务A修复前的写死默认值，作为兜底而非硬编码，零回归）。
    pub fn metrics(&self, font_size: Option<f32>, line_height_multiplier: f32) -> Metrics {
        let fs = font_size.unwrap_or(self.default_font_size);
        Metrics::new(fs, fs * line_height_multiplier)
    }

    /// WROOT-025（移植自PPTX侧 PROOT-035 的 expand_tabs_to_spaces 方案）：
    /// 用 Parley 对一小段纯文本做一次轻量 shape，返回其整体前进宽度
    /// （pt）。用于制表符展开时"到下个tab stop还差多少像素"的动态
    /// 测量，取代不随字号/字体变化的固定空格数近似方案。测量逻辑
    /// 与本文件下方单元测试 measure_parley_space_width 完全一致
    /// （builder→build→break_all_lines→align→line.metrics().advance），
    /// 这里是把当时验证过的临时测试代码固化成正式可调用方法。
    pub fn measure_text_width(&mut self, text: &str, font_size_pt: f32) -> f32 {
        use parley::{Alignment, AlignmentOptions, GenericFamily, LineHeight, StyleProperty};

        if text.is_empty() {
            return 0.0;
        }

        let default_brush = crate::tile::RunBrush { color: [0, 0, 0, 255], run_idx: None };
        let mut builder = self.layout_context.ranged_builder(
            &mut self.font_context, text, 1.0, true,
        );
        builder.push_default(StyleProperty::Brush(default_brush));
        builder.push_default(GenericFamily::SystemUi);
        builder.push_default(LineHeight::Absolute(font_size_pt));
        builder.push_default(StyleProperty::FontSize(font_size_pt));

        let mut layout: parley::Layout<crate::tile::RunBrush> = builder.build(text);
        layout.break_all_lines(Some(10000.0));
        layout.align(Alignment::Start, AlignmentOptions::default());

        let mut total_advance = 0.0f32;
        for line in layout.lines() {
            let line: parley::Line<'_, crate::tile::RunBrush> = line;
            total_advance = line.metrics().advance;
        }
        total_advance
    }

    /// 清空 shape 缓存（文档内容变更后调用）
    pub fn invalidate_shape_cache(&mut self) {
        self.shape_cache.clear();
    }
}
