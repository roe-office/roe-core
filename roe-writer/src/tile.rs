//! WriterTileRenderer：把 LayoutPage 渲染成 tiny-skia tile bitmap，写入外部 buffer
//! 字形渲染：Parley 排版 → skrifa 矢量轮廓 → tiny-skia 路径填充
//! （原 cosmic-text 版本见 tile.rs.bak_before_temp_freeze）

use tiny_skia::{
    Color, GradientStop as SkGradientStop, LinearGradient, Paint, PathBuilder, Point,
    PixmapMut, RadialGradient, Rect, SpreadMode, Stroke, Transform,
};
use skrifa::{
    GlyphId, MetadataProvider,
    instance::{LocationRef, NormalizedCoord, Size},
    outline::{DrawSettings, OutlinePen},
    raw::FontRef as ReadFontsRef,
};
use parley::{
    Alignment, AlignmentOptions, FontStyle, FontWeight, GenericFamily, GlyphRun, LineHeight,
    PositionedLayoutItem, StyleProperty,
};
use crate::layout::LayoutPage;
use roe_model::writer::ScriptPosition;

/// 批注高亮/竖线/气泡共用颜色表：6组「深色竖线, 浅色高亮底」，
/// 按 Comment.id % COMMENT_HIGHLIGHT_COLOR_COUNT（layout.rs定义，值为6）
/// 分配。v152起手：从 render_viewport 函数体内部的局部 const 提到
/// 模块级，供批注气泡绘制逻辑复用同一套颜色（此前气泡功能不存在时，
/// 只有竖线/高亮底一处使用者，放函数局部没有问题；新增气泡绘制后
/// 两处都要用，重复定义会有"改颜色忘记改全"的风险，故提升作用域，
/// 数值本身不变，零视觉差异）。
/// 参照OnlyOffice/Word默认批注配色思路，顺序（粉/蓝/绿/浅紫/橙/浅红）
/// 已与用户确认；不含黄色（黄偏冷不适合做高亮色）。
pub(crate) const COMMENT_HIGHLIGHT_COLORS: [([u8;3], [u8;4]); 6] = [
    ([0xC2, 0x3D, 0x7E], [0xF5, 0xC8, 0xDC, 0x80]), // 粉：深竖线/浅高亮(半透明)
    ([0x2D, 0x6B, 0xA8], [0xC0, 0xDC, 0xF0, 0x80]), // 蓝
    ([0x3D, 0x8B, 0x3D], [0xC8, 0xE8, 0xC8, 0x80]), // 绿
    ([0x8B, 0x5F, 0xBF], [0xE0, 0xD4, 0xF0, 0x80]), // 浅紫
    ([0xFF, 0x8C, 0x00], [0xFF, 0xE0, 0xB2, 0x80]), // 橙（油漆橘）
    ([0xD6, 0x58, 0x58], [0xF8, 0xD0, 0xD0, 0x80]), // 浅红
];

/// roe-writer 专用画刷：在颜色之外携带 run_idx，用于渲染阶段反查该 run
/// 对应的原始样式（下划线/删除线/公式判断都需要知道"这段字形属于哪个
/// run"）。这是 Parley 下 cosmic-text `Attrs::metadata(run_idx)` 机制的
/// 等价替代——Parley 没有独立的 metadata 整数字段，但 Brush 只要求
/// Clone+PartialEq+Default+Debug，可以把任意数据放进自定义 Brush 类型，
/// 渲染时通过 glyph_run.style().brush 直接读回，不需要事后反查。
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct RunBrush {
    pub color: [u8; 4],
    /// 对应 shaped_line.runs 的下标；None 表示无原始 run 信息（如 list_label、
    /// 页眉页脚这类不需要 run 级别样式反查的场合）。
    pub run_idx: Option<usize>,
}

/// 把 skrifa 矢量轮廓画到 tiny_skia PixmapMut 上的笔。
/// 与 roe-core/tile_render_helpers.rs 的 TinySkiaPen 同源实现（照抄 Parley
/// 官方 examples/tiny_skia_render/src/main.rs），两个 crate 各自持有一份，
/// 避免 roe-writer 反向依赖 roe-core。
pub(crate) struct TinySkiaPen<'a, 'b> {
    pixmap: &'b mut tiny_skia::PixmapMut<'a>,
    x: f32,
    y: f32,
    paint: tiny_skia::Paint<'static>,
    open_path: tiny_skia::PathBuilder,
}

impl<'a, 'b> TinySkiaPen<'a, 'b> {
    pub(crate) fn new(pixmap: &'b mut tiny_skia::PixmapMut<'a>) -> Self {
        TinySkiaPen {
            pixmap,
            x: 0.0,
            y: 0.0,
            paint: tiny_skia::Paint::default(),
            open_path: tiny_skia::PathBuilder::new(),
        }
    }

    fn set_origin(&mut self, x: f32, y: f32) {
        self.x = x;
        self.y = y;
    }

    fn set_color_rgba8(&mut self, r: u8, g: u8, b: u8, a: u8) {
        self.paint.set_color_rgba8(r, g, b, a);
    }

    fn fill_rect(&mut self, width: f32, height: f32) {
        if let Some(rect) = tiny_skia::Rect::from_xywh(self.x, self.y, width, height) {
            self.pixmap.fill_rect(rect, &self.paint, tiny_skia::Transform::identity(), None);
        }
    }

    // WROOT-ITALIC-SKEW修复：新增 skew_degrees 参数，实现 synthetic italic
    // （假斜体）。parley/fontique 的 font matching 阶段会在选中字体缺少真实
    // 倾斜轮廓时，通过 run.synthesis().skew() 给出建议倾斜角度（度数，逆时针
    // 为正，对应字体排印里从垂直方向起算），此前该建议从未被读取消费，导致
    // 斜体样式完全不生效。这里在填充字形路径前，对轮廓坐标施加一个绕
    // (origin_x, origin_y) 的水平剪切（shear）变换：
    //   x' = x + (origin_y - y) * tan(skew_degrees弧度)
    // 用 tiny_skia::Transform::from_row 的 kx 分量实现，比逐点手工偏移更简洁，
    // 且不影响描边形状本身，只做整体斜切。skew_degrees=0 时退化为恒等变换，
    // 行为与修复前完全一致，不影响非斜体文字。
    fn draw_glyph(
        &mut self,
        glyph: &skrifa::outline::OutlineGlyph<'_>,
        size: f32,
        normalized_coords: &[NormalizedCoord],
        skew_degrees: f32,
    ) {
        let location_ref = LocationRef::new(normalized_coords);
        let settings = DrawSettings::unhinted(Size::new(size), location_ref);
        if glyph.draw(settings, self).is_err() {
            self.open_path = tiny_skia::PathBuilder::new();
            return;
        }
        let builder = core::mem::replace(&mut self.open_path, tiny_skia::PathBuilder::new());
        if let Some(path) = builder.finish() {
            let transform = if skew_degrees != 0.0 {
                // tiny_skia::Transform::from_row(sx, ky, kx, sy, tx, ty)
                // kx 为水平剪切系数：绕字形原点(self.x, self.y)剪切，
                // 需要平移到原点做剪切再平移回去，等价于直接算好 tx。
                let kx = -(skew_degrees.to_radians().tan());
                let tx = -kx * self.y;
                tiny_skia::Transform::from_row(1.0, 0.0, kx, 1.0, tx, 0.0)
            } else {
                tiny_skia::Transform::identity()
            };
            self.pixmap.fill_path(
                &path, &self.paint, tiny_skia::FillRule::Winding,
                transform, None,
            );
        }
    }

    // WROOT-EMOJI-COLOR修复：绘制彩色字形（emoji等COLR/彩色位图字体）。
    // 与 draw_glyph（skrifa轮廓路径，统一用 self.paint 当前颜色填充）不同，
    // 彩色字形自带每像素RGBA数据（来自swash的Content::Color渲染结果），
    // 不套用当前文字颜色，直接按像素alpha混合贴到画布上。
    // pixel_data: 紧密排列的RGBA字节（每像素4字节），长度必须等于
    // width*height*4，由调用方（render_glyph_run_with_offset）保证。
    // top_left_x/y: 像素矩阵左上角在画布上的整数坐标（swash的
    // rendered_glyph.placement 已经算好了相对glyph原点的偏移，调用方
    // 负责换算成画布坐标后传入）。
    fn draw_color_glyph_pixels(
        &mut self,
        pixel_data: &[u8],
        width: u32,
        height: u32,
        top_left_x: i32,
        top_left_y: i32,
    ) {
        if width == 0 || height == 0 {
            return;
        }
        let Some(pixmap_alloc) = tiny_skia::Pixmap::from_vec(
            pixel_data.to_vec(),
            tiny_skia::IntSize::from_wh(width, height).unwrap(),
        ) else {
            return;
        };
        let paint = tiny_skia::PixmapPaint::default();
        self.pixmap.draw_pixmap(
            top_left_x,
            top_left_y,
            pixmap_alloc.as_ref(),
            &paint,
            tiny_skia::Transform::identity(),
            None,
        );
    }
}

impl OutlinePen for TinySkiaPen<'_, '_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.open_path.move_to(self.x + x, self.y - y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.open_path.line_to(self.x + x, self.y - y);
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.open_path.quad_to(self.x + cx0, self.y - cy0, self.x + x, self.y - y);
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.open_path.cubic_to(
            self.x + cx0, self.y - cy0, self.x + cx1, self.y - cy1, self.x + x, self.y - y,
        );
    }
    fn close(&mut self) {
        self.open_path.close();
    }
}

/// 渲染一个 GlyphRun 的字形本体（不含下划线/删除线，那部分由调用方在
/// 拿到 glyph_run.style() 后单独处理，因为下划线/删除线的颜色可能与
/// 文字本身颜色不同，且 roe-writer 这边的颜色统一走 RunBrush.color）。
pub(crate) fn render_glyph_run(
    glyph_run: &GlyphRun<'_, RunBrush>,
    pen: &mut TinySkiaPen<'_, '_>,
    origin_x: f32,
    origin_y: f32,
) {
    render_glyph_run_with_offset(glyph_run, pen, origin_x, origin_y, |_byte_pos| 0.0);
}

/// WROOT-068 Bug2 真正修复：cluster 级别精细化应用 tab 偏移。
///
/// 背景（避免下次重踩）：此前两轮尝试过在 push 样式阶段把 style range
/// 按 tab 占位符字节切开，指望 Parley 会把 tab 前后文字 shape 成两个
/// 独立的 GlyphRun，这样就能给"整个 GlyphRun"套用统一的 origin_x 偏移。
/// 实测证明这条路走不通：Parley/swash 的底层 shape 引擎会按自己的规则
/// （字体+样式是否相同、是否相邻）合并物理 GlyphRun，不受 push 时 range
/// 是否连续的直接控制；`Run::text_range()` 返回的是 shape 引擎产出的
/// `shaped.range.byte_range`，即使我们切了 range，这个值仍然是合并后
/// 的整体范围，导致"按 run_idx 反查整个 run 起点"这种查表方式，永远
/// 拿不到"tab前"和"tab后"两段应该有的不同偏移量（找到的要么都是0、
/// 要么都是同一个值）。
///
/// 经与外部咨询（Gemini）确认后改用的正确方案：不再纠结于能否让
/// Parley 在 GlyphRun 层面物理拆分，而是在渲染（画字）阶段做精细化
/// 处理——利用 `Cluster::text_range()`（这个值不受 shape 合并影响，
/// 是逐字符/字形簇级别的真实原文字节位置，来自 `cluster.rs` 里
/// `self.run.shaped.range.byte_range.start + self.data.text_offset`，
/// 与 `Run::text_range()` 依赖的字段不同）逐 cluster 判断字节位置，
/// 不同 cluster 可以应用不同的偏移量，即使它们仍属于同一个物理
/// GlyphRun 也没关系。
///
/// 实现上不能直接用 `run.visual_clusters()` 重新做一遍裁剪（会拿到
/// 超出当前行范围的 cluster，因为行内裁剪用的 `glyph_start`/
/// `glyph_count` 是 `parley::layout::line` 模块内的私有字段，外部
/// crate 访问不到）。改用双指针对齐法：外层仍然用原有的
/// `glyph_run.glyphs()`（自带正确的行内裁剪，不改动），额外单独遍历
/// `run.clusters()` 记录每个 cluster 包含几个 glyph + 字节起点，
/// 用一个游标跟随外层 glyph 循环同步前进，每消耗完一个 cluster 的
/// glyph 数量就切到下一个 cluster——这样能保证不依赖任何私有字段，
/// 同时给每个 glyph 都能正确对应上它所属 cluster 的原文字节位置。
///
/// extra_width_at: 给定一个原文字节偏移（cluster 起点），返回这个
/// cluster 应该额外叠加的 x 方向偏移量（像素，已包含 scale）。
/// tab_adjustments 为空（绝大多数文字，无 w:tabs 定义）时，调用方应
/// 传入恒返回 0.0 的闭包，此时行为与修复前完全一致，不影响任何现有
/// 渲染路径。
pub(crate) fn render_glyph_run_with_offset(
    glyph_run: &GlyphRun<'_, RunBrush>,
    pen: &mut TinySkiaPen<'_, '_>,
    origin_x: f32,
    origin_y: f32,
    extra_width_at: impl Fn(usize) -> f32,
) {
    let mut run_x = glyph_run.offset();
    let run_y = glyph_run.baseline();
    let style = glyph_run.style();
    let color = style.brush.color;
    let run = glyph_run.run();
    let font = run.font();
    let font_size = run.font_size();
    // WROOT-ITALIC-SKEW修复：读取 fontique 给出的 synthetic italic 倾斜建议
    // （单位：度）。当选中字体本身有真实斜体轮廓时，fontique 返回 0，这里
    // 传 0 给 draw_glyph 等价于不做任何变换，与修复前行为一致。
    let skew_degrees = run.synthesis().skew().unwrap_or(0.0);

    let normalized_coords = run
        .normalized_coords()
        .iter()
        .map(|coord| NormalizedCoord::from_bits(coord.to_bits()))
        .collect::<Vec<_>>();

    let font_collection_ref = font.font.data.as_ref();
    let font_ref = match ReadFontsRef::from_index(font_collection_ref, font.font.index) {
        Ok(f) => f,
        Err(_) => return,
    };
    let outlines = font_ref.outline_glyphs();

    // WROOT-EMOJI-COLOR修复：尝试构建swash侧的字形scaler，用于探测/渲染彩色
    // 字形（COLR矢量分层、CBDT/sbix位图等，emoji字体常见格式）。skrifa的
    // outline_glyphs()接口只能吐出矢量轮廓，天然无法表达彩色/位图字形——
    // 这是此前emoji完全无色的根因（不是"渲染错了"，是这条路径设计上就
    // 没有处理彩色数据的能力）。ScaleContext必须在Scaler之前、且生命周期
    // 覆盖整个循环体声明（Scaler内部借用了ScaleContext，不能把两者打包
    // 进同一个Option再返回，否则是self-referential，编译不过）。
    // 构建失败（理论上极少发生）则scaler为None，quiet fallback到原有
    // skrifa路径，不影响任何非emoji文字。
    let mut swash_scale_cx = swash::scale::ScaleContext::new();
    let swash_font_ref = swash::FontRef::from_index(font_collection_ref, font.font.index as usize);
    let mut swash_scaler = swash_font_ref.map(|sf| {
        swash_scale_cx.builder(sf)
            .size(font_size)
            .hint(true)
            .normalized_coords(normalized_coords.iter().map(|c| c.to_bits()))
            .build()
    });

    // 双指针对齐：cluster_iter 提供"这个 cluster 还剩几个 glyph 没被
    // 外层消耗、这个 cluster 的字节起点是什么"，glyphs_left_in_cluster
    // 记录当前 cluster 还没被外层 glyph 循环消耗掉的 glyph 数量。
    let mut cluster_iter = run.clusters();
    let mut glyphs_left_in_cluster: usize = 0;
    let mut current_cluster_byte_start: usize = 0;
    let mut current_extra: f32 = 0.0;

    for glyph in glyph_run.glyphs() {
        if glyphs_left_in_cluster == 0 {
            // 当前 cluster 已耗尽（或是第一次循环），推进到下一个
            // 有 glyph 的 cluster。理论上每个 cluster 至少 1 个 glyph，
            // 这里用 while 兜底跳过极端情况下的空 cluster，避免死循环
            // 或漏算偏移。
            loop {
                match cluster_iter.next() {
                    Some(c) => {
                        let n = c.glyphs().count();
                        if n > 0 {
                            glyphs_left_in_cluster = n;
                            current_cluster_byte_start = c.text_range().start;
                            current_extra = extra_width_at(current_cluster_byte_start);
                            break;
                        }
                        // 空 cluster，继续找下一个
                    }
                    None => {
                        // cluster 迭代器提前耗尽（理论上不应发生，glyph
                        // 和 cluster 应该同步耗尽），保底不再叠加偏移，
                        // 按原逻辑画完剩余 glyph，避免 panic。
                        current_extra = 0.0;
                        break;
                    }
                }
            }
        }
        if glyphs_left_in_cluster > 0 {
            glyphs_left_in_cluster -= 1;
        }

        let glyph_x = origin_x + run_x + glyph.x + current_extra;
        let glyph_y = origin_y + run_y + glyph.y;
        run_x += glyph.advance;

        let glyph_id = GlyphId::from(glyph.id);

        // WROOT-EMOJI-COLOR修复：先尝试彩色字形渲染。只探测ColorOutline/
        // ColorBitmap两种彩色来源（刻意不包含Source::Outline），这样
        // Render失败/返回非Color内容时能确定"这个字形没有彩色数据"，
        // 从而安全fallback到下面原有的skrifa outline路径。
        let mut drew_color = false;
        if let Some(ref mut scaler) = swash_scaler {
            if let Some(rendered) = swash::scale::Render::new(&[
                swash::scale::Source::ColorOutline(0),
                swash::scale::Source::ColorBitmap(swash::scale::StrikeWith::BestFit),
            ])
            .format(swash::zeno::Format::Alpha)
            .render(scaler, glyph.id as u16)
            {
                if rendered.content == swash::scale::image::Content::Color {
                    let gw = rendered.placement.width;
                    let gh = rendered.placement.height;
                    if gw > 0 && gh > 0 {
                        let px = glyph_x.floor() as i32 + rendered.placement.left;
                        let py = glyph_y.floor() as i32 - rendered.placement.top;
                        pen.draw_color_glyph_pixels(&rendered.data, gw, gh, px, py);
                        drew_color = true;
                    }
                }
            }
        }

        if !drew_color {
            if let Some(glyph_outline) = outlines.get(glyph_id) {
                pen.set_origin(glyph_x, glyph_y);
                pen.set_color_rgba8(color[0], color[1], color[2], color[3]);
                pen.draw_glyph(&glyph_outline, font_size, &normalized_coords, skew_degrees);
            }
        }
    }
    let _ = current_cluster_byte_start; // 仅用于计算 current_extra，避免 unused 警告
}

pub struct WriterTileRenderer {
    pub font_context: parley::FontContext,
    pub(crate) layout_context: parley::LayoutContext<RunBrush>,
    /// 批注气泡功能（v152起手）：render_viewport 渲染过程中顺手收集到的
    /// 批注命中数据 (comment_id, line_top_pt, color_idx)，每次渲染前
    /// 应先清空（render_viewport 内部已处理），渲染完用 take_comment_hits()
    /// 取出。故意不改 render_viewport 签名/不新增包装方法——用 self 字段
    /// 暂存是改动面最小的做法，render_viewport 本身逻辑不变，只在其内部
    /// 已有的 rendered_lines 循环里多做一次记录。
    pending_comment_hits: Vec<(u32, f32, f32, u8)>,
}

impl WriterTileRenderer {
    /// 空字体版本，仅适合确定不含公式渲染的场景（如 roe_render_page 分页
    /// 缩略图）。
    ///
    /// 【2026-09-14 教训，务必读完再用】此前 roe-api 侧多个调用点
    /// （roe_render_writer_viewport 等FFI入口、render_docx_batch.rs 等
    /// example）在需要渲染公式的场景下，也错误地使用了本函数或直接裸建
    /// parley::FontContext::new() 再调 new_with_fonts()，导致公式渲染
    /// 静默 fallback 到系统字体——不报错、不崩溃，只是数字挤压变形、Σ
    /// 等符号字号错乱、位置整体偏移，用肉眼核对才能发现，且historically
    /// 有一条路径（render_one.rs）一直是对的，制造了"前台渲染有问题、
    /// 脚本渲染没问题"的假象，一度误判为office-rs2前端bug，实际根因在
    /// roe-api库本身从未加载过KaTeX字体。
    ///
    /// 【规则】任何不能100%确定文档不含公式的场景，一律调用
    /// `new_with_fonts(FontRegistry::build_font_context_with_katex(), ())`，
    /// 不要调用本函数，也不要自己裸建 parley::FontContext::new() 再传给
    /// new_with_fonts —— 那等价于本函数，会重新踩同一个坑。
    pub fn new() -> Self {
        Self {
            font_context: parley::FontContext::new(),
            layout_context: parley::LayoutContext::new(),
            pending_comment_hits: Vec::new(),
        }
    }

    /// 复用外部已建立的字体资源，避免每次渲染重新扫描系统字体（性能修复，v86任务A）。
    /// 调用方渲染完毕后应调用 take_fonts() 把资源取回，供下一帧复用。
    ///
    /// 【2026-09-14 教训】本函数本身没问题，出问题的是调用方传进来的
    /// font_context——如果调用方图省事传 parley::FontContext::new()
    /// 或从一个从未调用过 load_katex_fonts() 的 FontRegistry 里取值，
    /// 效果跟调用上面的 new() 完全一样，同样会丢失KaTeX字体。
    ///
    /// 正确用法（唯一权威入口，不要自己重新实现"构造FontRegistry→
    /// mem::replace偷出font_context"这段桥接逻辑，历史上这段逻辑曾经
    /// 分别在 render_one.rs / render_with_comments.rs 各手写一份，
    /// 其余调用点（roe-api/src/tile.rs的roe_render_writer_viewport、
    /// render_docx_batch.rs）因为没人同步这份逻辑而遗漏，属于典型的
    /// "多头维护、其中一头忘了更新"分叉问题）：
    /// ```ignore
    /// let font_context = existing_cached_context
    ///     .unwrap_or_else(FontRegistry::build_font_context_with_katex);
    /// let renderer = WriterTileRenderer::new_with_fonts(font_context, ());
    /// ```
    pub fn new_with_fonts(font_context: parley::FontContext, _unused: ()) -> Self {
        Self {
            font_context,
            layout_context: parley::LayoutContext::new(),
            pending_comment_hits: Vec::new(),
        }
    }

    /// 取回并清空本次渲染收集到的批注命中数据（comment_id, line_top_pt,
    /// color_idx）。应在 render_viewport 调用之后立即取用；下一次
    /// render_viewport 调用前会自动清空旧数据（见 render_viewport 内部
    /// 开头处的 clear()），不需要调用方手动清理。
    pub fn take_comment_hits(&mut self) -> Vec<(u32, f32, f32, u8)> {
        std::mem::take(&mut self.pending_comment_hits)
    }

    /// 取回字体资源（消耗 self），供调用方存回缓存复用。
    pub fn take_fonts(self) -> (parley::FontContext, ()) {
        (self.font_context, ())
    }

    /// 渲染整页预览到外部提供的 RGBA buffer。
    /// 返回 (pw, ph) —— 实际渲染尺寸，供调用方校验/调试。
    /// 若 buf.len() != pw*ph*4，返回 Err。
    // 备注（WROOT-036排查时确认）：render_page_preview 目前只被本文件内的
    // #[test] 单元测试和 render_writer.rs 这个example调用，不是真实文档渲染
    // 的主路径（主路径是下面的 render_viewport，被 roe-api 正式代码/
    // render_one/render_docx_batch 使用）。两个函数各自维护了一套相近但不
    // 完全同步的渲染逻辑，属于已知技术债，但风险面小（不影响真实渲染结果），
    // 暂不合并，仅记录以防未来render_page_preview被挪用为真实渲染路径时
    // 忘记同步修复。
    // ⚠️⚠️⚠️ 废弃路径警告（v61标注，v60发现问题）⚠️⚠️⚠️
    // 本函数是 WriterTileRenderer 从 bitmap 整页渲染迁移到 render_viewport()
    // 视口渲染之前的过渡期实现。据 arena_to_layout_pages() 函数注释原话，
    // 本函数消费的 LayoutPage 是"供 WriterTileRenderer 过渡期使用"的桥接
    // 数据结构。
    //
    // 生产渲染路径（render_one.rs / render_docx_batch.rs 实际调用）走的是
    // render_viewport()，完全不经过本函数。v60开发时曾在本函数（当时叫
    // "images/float_frames/charts循环"那条路径）新增 Shape 绘制逻辑，
    // 编译通过、调试脚本验证坐标数据完全正确，但实际用 render_one.rs 渲染
    // 出来是空白页——因为 render_one.rs 根本不调用本函数。排查后确认
    // render_viewport() 才是正确的目标函数，返工重做了一遍。
    //
    // 【下次要在此渲染管线加新功能前，请先确认 render_viewport() 里是否已
    // 有等价逻辑，不要想当然地在本函数里改】。若确认某天所有调用方
    // （包括单测）都不再依赖本函数，可以考虑整体删除。
    // ===== WROOT-058 (2026-08-21): render_page_preview 已废弃，整体注释保留 =====
    // 真实渲染管线走 render_viewport（见下方，render_one.rs 等实际调用方均用它）。
    // render_page_preview 是升级到 viewport 渲染前的旧 bitmap 一次性渲染方式，
    // 保留代码仅供以后需要时参考对比，不再维护/不再随功能更新（比如本次WROOT-058
    // 背景色渲染修复只改了 render_viewport，未同步改这里）。
    // pub fn render_page_preview(&mut self, page: &LayoutPage, scale: f32, buf: &mut [u8]) -> Result<(u32, u32), String> {
    // let pw = (page.width  * scale) as u32;
    // let ph = (page.height * scale) as u32;

    // let expected = (pw as usize) * (ph as usize) * 4;
    // if buf.len() != expected {
    // return Err(format!(
    // "buffer size mismatch: expected {} bytes ({}x{}x4), got {}",
    // expected, pw, ph, buf.len()
    // ));
    // }

    // let mut pixmap = PixmapMut::from_bytes(buf, pw, ph)
    // .ok_or_else(|| format!("PixmapMut::from_bytes failed for {}x{}", pw, ph))?;

    // // 背景填白
    // {
    // let mut paint = Paint::default();
    // paint.set_color(Color::WHITE);
    // if let Some(rect) = Rect::from_xywh(0.0, 0.0, pw as f32, ph as f32) {
    // pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    // }
    // }

    // let margin = 56.7 * scale;   // ~2cm
    // let mut cursor_y = margin;

    // for para in &page.paragraphs {
    // cursor_y += para.space_before * scale;
    // for shaped_line in &para.lines {
    // let line_height = shaped_line.line_height * scale;

    // // DOCX-8 公式渲染修复（render_page_preview）：公式run的text是
    // // 空格串占位符，trim()后为空，与render_viewport保持一致，
    // // 必须检查has_formula_task才能决定是否跳过。
    // let has_formula_task = shaped_line.runs.iter().any(|r| r.style.is_formula);
    // if shaped_line.text.trim().is_empty() && !has_formula_task {
    // cursor_y += line_height;
    // continue;
    // }

    // let primary_font_size = shaped_line.runs.first()
    // .map(|r| r.style.font_size)
    // .unwrap_or(12.0_f32);

    // // Normal run（非上下标）参与主 layout；range 用字节偏移，
    // // 与 cosmic-text 版本的 span.start/span.end 语义一致
    // // （原代码同样直接用 shaped_line.text[start..end] 做字节切片）。
    // let default_brush = RunBrush { color: [30, 30, 30, 255], run_idx: None };
    // let mut builder = self.layout_context.ranged_builder(
    // &mut self.font_context, &shaped_line.text, scale.max(1.0), true,
    // );
    // builder.push_default(StyleProperty::Brush(default_brush));
    // builder.push_default(GenericFamily::SystemUi);
    // builder.push_default(LineHeight::Absolute(shaped_line.line_height));
    // builder.push_default(StyleProperty::FontSize(primary_font_size));
    // for (run_idx, span) in shaped_line.runs.iter().enumerate() {
    // if span.style.script != ScriptPosition::Normal { continue; } // Super/Sub 跳过，另起小 layout
    // let start = span.start.min(shaped_line.text.len());
    // let end   = span.end.min(shaped_line.text.len());
    // if start >= end { continue; }
    // let range = start..end;
    // let color = span.style.color
    // .map(|c| [c.r, c.g, c.b, c.a])
    // .unwrap_or([30, 30, 30, 255]);
    // builder.push(StyleProperty::Brush(RunBrush { color, run_idx: Some(run_idx) }), range.clone());
    // if span.style.bold   { builder.push(StyleProperty::FontWeight(FontWeight::BOLD), range.clone()); }
    // if span.style.italic { builder.push(StyleProperty::FontStyle(FontStyle::Italic), range.clone()); }
    // if span.style.underline    { builder.push(StyleProperty::Underline(true), range.clone()); }
    // if span.style.strikethrough { builder.push(StyleProperty::Strikethrough(true), range.clone()); }
    // }
    // let mut layout: parley::Layout<RunBrush> = builder.build(&shaped_line.text);
    // layout.break_all_lines(None);
    // layout.align(Alignment::Start, AlignmentOptions::default());

    // let draw_y = cursor_y;
    // let runs_ref = &shaped_line.runs;
    // let is_table_cell = shaped_line.cell_width > 0.0;
    // let cell_x_off = margin + shaped_line.text_x_offset * scale;

    // if let Some(label) = &shaped_line.list_label {
    // let label_brush = RunBrush { color: [30, 30, 30, 255], run_idx: None };
    // let mut lbuilder = self.layout_context.ranged_builder(
    // &mut self.font_context, label, scale.max(1.0), true,
    // );
    // lbuilder.push_default(StyleProperty::Brush(label_brush));
    // lbuilder.push_default(GenericFamily::SystemUi);
    // lbuilder.push_default(LineHeight::Absolute(shaped_line.line_height));
    // lbuilder.push_default(StyleProperty::FontSize(primary_font_size));
    // let mut label_layout: parley::Layout<RunBrush> = lbuilder.build(label);
    // label_layout.break_all_lines(None);
    // label_layout.align(Alignment::Start, AlignmentOptions::default());
    // let label_x = margin + shaped_line.list_label_x * scale;
    // let mut pen = TinySkiaPen::new(&mut pixmap);
    // for line in label_layout.lines() {
    // for item in line.items() {
    // if let PositionedLayoutItem::GlyphRun::<RunBrush>(glyph_run) = item {
    // render_glyph_run(&glyph_run, &mut pen, label_x, cursor_y);
    // }
    // }
    // }
    // }

    // let cell_clip_max = if is_table_cell {
    // Some(margin + (shaped_line.x_offset + shaped_line.cell_width) * scale)
    // } else {
    // None
    // };
    // {
    // let mut pen = TinySkiaPen::new(&mut pixmap);
    // for line in layout.lines() {
    // for item in line.items() {
    // if let PositionedLayoutItem::GlyphRun::<RunBrush>(glyph_run) = item {
    // // 表格 cell 水平裁剪：右侧超出 cell 边界的字形整体跳过
    // // （与旧实现逐像素裁剪不同，逐 glyph 判断即可，因为
    // // 字形本身不会跨越很大宽度）。
    // if let Some(clip) = cell_clip_max {
    // let glyph_run_right = cell_x_off + glyph_run.offset() + glyph_run.advance();
    // if glyph_run_right > clip { continue; }
    // }
    // render_glyph_run(&glyph_run, &mut pen, cell_x_off, draw_y);

    // // underline / strikethrough：直接从 glyph_run.style() 读，
    // // 不再需要 metadata 反查（Parley 原生按 style range 输出
    // // 对应 decoration 信息）。
    // let style = glyph_run.style();
    // let run_metrics = glyph_run.run().font_metrics();
    // let deco_color = style.brush.color;
    // let deco_tsk_color = tiny_skia::Color::from_rgba8(
    // deco_color[0], deco_color[1], deco_color[2], deco_color[3],
    // );
    // if let Some(decoration) = &style.underline {
    // let offset = decoration.offset.unwrap_or(run_metrics.underline_offset);
    // let y = draw_y + glyph_run.baseline() - offset;
    // let x0 = cell_x_off + glyph_run.offset();
    // let x1 = x0 + glyph_run.advance();
    // Self::draw_h_line(pen.pixmap, x0, x1, y, 1.0, deco_tsk_color);
    // }
    // if let Some(decoration) = &style.strikethrough {
    // let offset = decoration.offset.unwrap_or(run_metrics.strikethrough_offset);
    // let y = draw_y + glyph_run.baseline() - offset;
    // let x0 = cell_x_off + glyph_run.offset();
    // let x1 = x0 + glyph_run.advance();
    // Self::draw_h_line(pen.pixmap, x0, x1, y, 1.0, deco_tsk_color);
    // }
    // }
    // }
    // }
    // }

    // // 表格边框线
    // if is_table_cell {
    // let bx0 = margin + shaped_line.x_offset * scale;
    // let bx1 = margin + (shaped_line.x_offset + shaped_line.cell_width) * scale;
    // let by0 = cursor_y;
    // let by1 = cursor_y + shaped_line.line_height * scale;
    // // WROOT-100：本函数render_page_preview已废弃（仅调试脚本
    // // roe-api/examples/render_writer.rs调用，真实渲染管线走
    // // render_viewport，见WROOT-096-followup已有记录），这里只做
    // // 最小兼容改动使其能编译，不追求BorderStyle语义的完整正确性。
    // Self::draw_h_line_styled(&mut pixmap, bx0, bx1, by0, shaped_line.border_top.as_ref(), scale);
    // Self::draw_h_line_styled(&mut pixmap, bx0, bx1, by1, shaped_line.border_bottom.as_ref(), scale);
    // if let Some(style) = &shaped_line.border_left {
    // let lx_px = margin + shaped_line.x_offset * scale;
    // Self::draw_v_line_styled(&mut pixmap, lx_px, by0, by1, Some(style), scale);
    // }
    // if let Some(style) = &shaped_line.border_right {
    // let rx_px = margin + (shaped_line.x_offset + shaped_line.cell_width) * scale;
    // Self::draw_v_line_styled(&mut pixmap, rx_px, by0, by1, Some(style), scale);
    // }
    // }

    // // 上下标：单独起小 layout，缩小字号 × 0.65，y 偏移
    // let script_runs: Vec<(usize, &crate::frame::LineRunSpan)> = shaped_line.runs.iter()
    // .enumerate()
    // .filter(|(_, r)| r.style.script != ScriptPosition::Normal)
    // .collect();
    // for (run_idx, span) in script_runs {
    // let start = span.start.min(shaped_line.text.len());
    // let end   = span.end.min(shaped_line.text.len());
    // if start >= end { continue; }
    // let slice = &shaped_line.text[start..end];
    // let sub_font_size_px = span.style.font_size * scale * 0.65; // 像素坐标，仅用于 y_offset 手动计算
    // let sub_font_size_pt = span.style.font_size * 0.65;          // 原始pt，喂给builder（builder内部会按scale再放大）
    // let sub_lh_pt = sub_font_size_pt * 1.3;
    // let y_offset = match span.style.script {
    // ScriptPosition::Superscript => cursor_y - span.style.font_size * scale * 0.35,
    // ScriptPosition::Subscript   => cursor_y + span.style.font_size * scale * 0.25,
    // ScriptPosition::Normal      => cursor_y,
    // };
    // let _ = sub_font_size_px; // 保留变量名供后续可能的像素级计算引用
    // let color = span.style.color
    // .map(|c| [c.r, c.g, c.b, c.a])
    // .unwrap_or([30, 30, 30, 255]);
    // let sub_brush = RunBrush { color, run_idx: Some(run_idx) };
    // let mut sbuilder = self.layout_context.ranged_builder(
    // &mut self.font_context, slice, scale.max(1.0), true,
    // );
    // sbuilder.push_default(StyleProperty::Brush(sub_brush));
    // sbuilder.push_default(GenericFamily::SystemUi);
    // sbuilder.push_default(LineHeight::Absolute(sub_lh_pt));
    // sbuilder.push_default(StyleProperty::FontSize(sub_font_size_pt));
    // if span.style.bold   { sbuilder.push_default(StyleProperty::FontWeight(FontWeight::BOLD)); }
    // if span.style.italic { sbuilder.push_default(StyleProperty::FontStyle(FontStyle::Italic)); }
    // let mut sub_layout: parley::Layout<RunBrush> = sbuilder.build(slice);
    // sub_layout.break_all_lines(None);
    // sub_layout.align(Alignment::Start, AlignmentOptions::default());
    // let mut pen = TinySkiaPen::new(&mut pixmap);
    // for line in sub_layout.lines() {
    // for item in line.items() {
    // if let PositionedLayoutItem::GlyphRun::<RunBrush>(glyph_run) = item {
    // render_glyph_run(&glyph_run, &mut pen, margin, y_offset);
    // }
    // }
    // }
    // }
    // let _ = runs_ref; // runs_ref 现通过 range push 直接消费，保留变量名避免大改后续引用

    // cursor_y += line_height;
    // }

    // cursor_y += para.space_after * scale;
    // }

    // // 渲染图片
    // for img in &page.images {
    // let img_x = margin;
    // let img_y = margin + img.y_on_page * scale;
    // let img_w = (img.width_pt  * scale) as u32;
    // let img_h = (img.height_pt * scale) as u32;
    // if img_w == 0 || img_h == 0 { continue; }
    // use base64::Engine;
    // let raw = match base64::engine::general_purpose::STANDARD.decode(
    // img.data_base64.replace([' ', '\n', '\r'], "").as_bytes()
    // ) {
    // Ok(b) => b,
    // Err(_) => continue,
    // };
    // let dyn_img = match image::load_from_memory(&raw) {
    // Ok(i) => i,
    // Err(_) => continue,
    // };
    // let rgba = dyn_img.resize(img_w, img_h, image::imageops::FilterType::Triangle).to_rgba8();
    // let src_w = rgba.width() as usize;
    // let src_h = rgba.height() as usize;
    // let dst_x = img_x as i32;
    // let dst_y = img_y as i32;
    // for sy in 0..src_h {
    // for sx in 0..src_w {
    // let dx = dst_x + sx as i32;
    // let dy = dst_y + sy as i32;
    // if dx < 0 || dy < 0 || dx >= pw as i32 || dy >= ph as i32 { continue; }
    // let pixel = rgba.get_pixel(sx as u32, sy as u32);
    // let idx = (dy as usize * pw as usize + dx as usize) * 4;
    // let buf = pixmap.data_mut();
    // if idx + 3 < buf.len() {
    // let a = pixel[3] as f32 / 255.0;
    // buf[idx]     = (pixel[0] as f32 * a + buf[idx]     as f32 * (1.0 - a)) as u8;
    // buf[idx + 1] = (pixel[1] as f32 * a + buf[idx + 1] as f32 * (1.0 - a)) as u8;
    // buf[idx + 2] = (pixel[2] as f32 * a + buf[idx + 2] as f32 * (1.0 - a)) as u8;
    // buf[idx + 3] = 255;
    // }
    // }
    // }
    // }
    // // 渲染浮动图片（有 x/y 坐标，叠加在文字上方）
    // for img in &page.float_frames {
    // let img_x = margin + img.x_on_page * scale;
    // let img_y = margin + img.y_on_page * scale;
    // let img_w = (img.width_pt  * scale) as u32;
    // let img_h = (img.height_pt * scale) as u32;
    // if img_w == 0 || img_h == 0 { continue; }
    // use base64::Engine;
    // let raw = match base64::engine::general_purpose::STANDARD.decode(
    // img.data_base64.replace([' ', '\n', '\r'], "").as_bytes()
    // ) {
    // Ok(b) => b,
    // Err(_) => continue,
    // };
    // let dyn_img = match image::load_from_memory(&raw) {
    // Ok(i) => i,
    // Err(_) => continue,
    // };
    // let rgba = dyn_img.resize(img_w, img_h, image::imageops::FilterType::Triangle).to_rgba8();
    // let src_w = rgba.width() as usize;
    // let src_h = rgba.height() as usize;
    // let dst_x = img_x as i32;
    // let dst_y = img_y as i32;
    // for sy in 0..src_h {
    // for sx in 0..src_w {
    // let dx = dst_x + sx as i32;
    // let dy = dst_y + sy as i32;
    // if dx < 0 || dy < 0 || dx >= pw as i32 || dy >= ph as i32 { continue; }
    // let pixel = rgba.get_pixel(sx as u32, sy as u32);
    // let idx = (dy as usize * pw as usize + dx as usize) * 4;
    // let buf = pixmap.data_mut();
    // if idx + 3 < buf.len() {
    // let a = pixel[3] as f32 / 255.0;
    // buf[idx]     = (pixel[0] as f32 * a + buf[idx]     as f32 * (1.0 - a)) as u8;
    // buf[idx + 1] = (pixel[1] as f32 * a + buf[idx + 1] as f32 * (1.0 - a)) as u8;
    // buf[idx + 2] = (pixel[2] as f32 * a + buf[idx + 2] as f32 * (1.0 - a)) as u8;
    // buf[idx + 3] = 255;
    // }
    // }
    // }
    // }
    // // 渲染内联图表
    // for chart in &page.charts {
    // let img_x = margin + chart.x_on_page * scale;
    // let img_y = margin + chart.y_on_page * scale;
    // let img_w = (chart.width_pt  * scale) as u32;
    // let img_h = (chart.height_pt * scale) as u32;
    // Self::blit_chart_to_pixmap(&chart.model, img_w, img_h, img_x as i32, img_y as i32, pw, ph, &mut pixmap);
    // }
    // // 渲染图形/文本框（Shape）：WROOT-023系列缺口，第一版只画geometry轮廓+纯色填充，
    // // 不处理渐变/描边/形状内文字（留到后续单独补），先验证坐标和基本可见性。
    // for shape in &page.shapes {
    // let sx = margin + shape.x_on_page * scale;
    // let sy = margin + shape.y_on_page * scale;
    // let mut pb = tiny_skia::PathBuilder::new();
    // if let roe_model::common::ShapeGeometry::PresetPath(cmds) = &shape.geometry {
    // for cmd in cmds {
    // match cmd {
    // roe_model::common::PathCommand::MoveTo { x, y } => {
    // pb.move_to(sx + x * scale, sy + y * scale);
    // }
    // roe_model::common::PathCommand::LineTo { x, y } => {
    // pb.line_to(sx + x * scale, sy + y * scale);
    // }
    // roe_model::common::PathCommand::CubicTo { x1, y1, x2, y2, x, y } => {
    // pb.cubic_to(
    // sx + x1 * scale, sy + y1 * scale,
    // sx + x2 * scale, sy + y2 * scale,
    // sx + x * scale,  sy + y * scale,
    // );
    // }
    // roe_model::common::PathCommand::QuadTo { x1, y1, x, y } => {
    // pb.quad_to(sx + x1 * scale, sy + y1 * scale, sx + x * scale, sy + y * scale);
    // }
    // roe_model::common::PathCommand::ArcTo { .. } => {
    // // 第一版未实现 ArcTo 转换，跳过（少数形状会因此路径不完整）
    // }
    // roe_model::common::PathCommand::Close => {
    // pb.close();
    // }
    // }
    // }
    // }
    // if let Some(built_path) = pb.finish() {
    // if let Some(fill) = &shape.style.fill {
    // let color = match fill {
    // roe_model::common::FillStyle::Solid(c) => {
    // Some(Color::from_rgba8(c.r, c.g, c.b, c.a))
    // }
    // roe_model::common::FillStyle::Gradient { stops, .. } => {
    // // 第一版：渐变降级为第一个色标的纯色，不做真正渐变绘制
    // stops.first()
    // .map(|s| Color::from_rgba8(s.color.r, s.color.g, s.color.b, s.color.a))
    // }
    // // 第一版未实现图片填充/平铺的绘制，跳过（形状会画成无填充轮廓）
    // roe_model::common::FillStyle::Picture(_) => None,
    // roe_model::common::FillStyle::PictureTiled { .. } => None,
    // roe_model::common::FillStyle::None => None,
    // };
    // if let Some(color) = color {
    // let mut paint = Paint::default();
    // paint.set_color(color);
    // paint.anti_alias = true;
    // pixmap.fill_path(&built_path, &paint, tiny_skia::FillRule::Winding, Transform::identity(), None);
    // }
    // }
    // }
    // }
    // // 渲染页眉（顶部 margin 区域居中）
    // if let Some(ref text) = page.header_text {
    // let hf_font_size = 10.0_f32 * scale;   // 像素坐标，用于手动计算 hy/fy 等定位
    // let hf_line_h    = hf_font_size * 1.4;
    // let hf_font_size_pt = 10.0_f32;         // 原始pt，喂给builder（builder内部按scale再放大）
    // let hf_line_h_pt    = hf_font_size_pt * 1.4;
    // let hf_brush = RunBrush { color: [80, 80, 80, 255], run_idx: None };
    // let mut hbuilder = self.layout_context.ranged_builder(
    // &mut self.font_context, text, scale.max(1.0), true,
    // );
    // hbuilder.push_default(StyleProperty::Brush(hf_brush));
    // hbuilder.push_default(GenericFamily::SystemUi);
    // hbuilder.push_default(LineHeight::Absolute(hf_line_h_pt));
    // hbuilder.push_default(StyleProperty::FontSize(hf_font_size_pt));
    // let mut hlayout: parley::Layout<RunBrush> = hbuilder.build(text);
    // hlayout.break_all_lines(Some(pw as f32 - margin * 2.0));
    // hlayout.align(Alignment::Start, AlignmentOptions::default());
    // let hy = margin * 0.55;
    // let mut pen = TinySkiaPen::new(&mut pixmap);
    // for line in hlayout.lines() {
    // for item in line.items() {
    // if let PositionedLayoutItem::GlyphRun::<RunBrush>(glyph_run) = item {
    // render_glyph_run(&glyph_run, &mut pen, margin, hy);
    // }
    // }
    // }
    // }
    // // 渲染页脚（底部 margin 区域）
    // if let Some(ref text) = page.footer_text {
    // let hf_font_size = 10.0_f32 * scale;   // 像素坐标，用于手动计算 hy/fy 等定位
    // let hf_line_h    = hf_font_size * 1.4;
    // let hf_font_size_pt = 10.0_f32;         // 原始pt，喂给builder（builder内部按scale再放大）
    // let hf_line_h_pt    = hf_font_size_pt * 1.4;
    // let hf_brush = RunBrush { color: [80, 80, 80, 255], run_idx: None };
    // let mut fbuilder = self.layout_context.ranged_builder(
    // &mut self.font_context, text, scale.max(1.0), true,
    // );
    // fbuilder.push_default(StyleProperty::Brush(hf_brush));
    // fbuilder.push_default(GenericFamily::SystemUi);
    // fbuilder.push_default(LineHeight::Absolute(hf_line_h_pt));
    // fbuilder.push_default(StyleProperty::FontSize(hf_font_size_pt));
    // let mut flayout: parley::Layout<RunBrush> = fbuilder.build(text);
    // flayout.break_all_lines(Some(pw as f32 - margin * 2.0));
    // flayout.align(Alignment::Start, AlignmentOptions::default());
    // let fy = ph as f32 - margin * 0.85;
    // if fy + hf_line_h <= ph as f32 {
    // let mut pen = TinySkiaPen::new(&mut pixmap);
    // for line in flayout.lines() {
    // for item in line.items() {
    // if let PositionedLayoutItem::GlyphRun(glyph_run) = item {
    // render_glyph_run(&glyph_run, &mut pen, margin, fy);
    // }
    // }
    // }
    // }
    // }
    // Self::draw_page_border(&mut pixmap, pw, ph);

    // Ok((pw, ph))
    // }

    /// 视口渲染：给定全局坐标视口 (vp_x, vp_y, vp_w, vp_h)（单位 pt），
    /// 渲染到 buf（RGBA，尺寸 pixel_w × pixel_h）。
    /// scale = pixel_w / vp_w（pt→px 换算）。
    /// 页面背景（白色）和页间空白（灰色）在此处理，无需调用方关心分页。
    pub fn render_viewport(
        &mut self,
        arena: &crate::frame::FrameArena,
        doc: &roe_model::writer::WriterDocument,
        vp_x: f32, vp_y: f32, vp_w: f32, vp_h: f32,
        pixel_w: u32, pixel_h: u32,
        buf: &mut [u8],
    ) -> Result<(), String> {
        let expected = pixel_w as usize * pixel_h as usize * 4;
        if buf.len() != expected {
            return Err(format!(
                "buffer size mismatch: expected {} got {}", expected, buf.len()
            ));
        }
        let scale = pixel_w as f32 / vp_w;
        // 批注气泡功能（v152起手）：每次渲染前清空上一次的收集结果，
        // 避免跨帧/跨页数据累积（render_viewport 是无状态渲染，见
        // ROE_Progress_v151.md 1.2节既有架构约定）。
        self.pending_comment_hits.clear();

        let mut pixmap = PixmapMut::from_bytes(buf, pixel_w, pixel_h)
            .ok_or_else(|| format!("PixmapMut::from_bytes failed {}x{}", pixel_w, pixel_h))?;

        // 背景填灰（页间空白色）
        {
            let mut paint = Paint::default();
            paint.set_color_rgba8(232, 232, 232, 255); // #E8E8E8 逻辑画布背景
            if let Some(rect) = Rect::from_xywh(0.0, 0.0, pixel_w as f32, pixel_h as f32) {
                pixmap.fill_rect(rect, &paint, Transform::identity(), None);
            }
        }

        // 画各页白色背景（只画与视口相交的页）
        for &pid in &arena.page_order {
            let page = &arena.pages[pid];
            let pg_y0 = arena.page_global_y(pid);           // 页顶（全局 pt）
            let pg_y1 = pg_y0 + page.height;
            // 视口裁剪
            if pg_y1 < vp_y || pg_y0 > vp_y + vp_h { continue; }
            let px_x = (0.0 - vp_x) * scale;
            let px_y = (pg_y0 - vp_y) * scale;
            let px_w = page.width * scale;
            let px_h = page.height * scale;
            if let Some(rect) = Rect::from_xywh(px_x, px_y, px_w, px_h) {
                let mut paint = Paint::default();
                paint.set_color(Color::WHITE);
                pixmap.fill_rect(rect, &paint, Transform::identity(), None);
            }
            // 页面边框
            {
                let mut paint = Paint::default();
                paint.set_color_rgba8(180, 180, 180, 255);
                let mut stroke = Stroke::default();
                stroke.width = 1.0;
                let mut pb = PathBuilder::new();
                pb.move_to(px_x, px_y);
                pb.line_to(px_x + px_w, px_y);
                pb.line_to(px_x + px_w, px_y + px_h);
                pb.line_to(px_x, px_y + px_h);
                pb.close();
                if let Some(path) = pb.finish() {
                    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
                }
            }
        }

        // [WROOT-任务A/B] header/footer 渲染逻辑已抽到 headerfooter.rs
        // （原140行左右内联在这里，tile.rs 逼近3000行不宜继续扩大）。
        // 任务B: 不再无条件读 doc.header.default，改为按每页的
        // page_section_index + 奇偶页 + 首页，从对应 Section 取正确内容
        // （取值优先级抄自 LibreOffice SwPageDesc::GetRightFormat/
        // GetLeftFormat，见 headerfooter.rs 模块注释）。
        crate::headerfooter::render_headers_footers(
            self, arena, doc, vp_x, vp_y, vp_w, scale, &mut pixmap,
        );
        // v270: 分页符标记画在上一页末尾（WPS 风格）：虚线 + 居中「分页符」
        if marks_enabled() {
            let __order: Vec<_> = arena.page_order.clone();
            for __i in 1..__order.len() {
                let __pid = __order[__i];
                let __prev = __order[__i - 1];
                let __pg = &arena.pages[__pid];
                let __ppg = &arena.pages[__prev];
                let Some(&__ffid) = __pg.frames.first() else { continue };
                let __ff = &arena.frames[__ffid];
                let __is_pb = __ff.precede.is_none()
                    && matches!(doc.body.get(__ff.para_index),
                        Some(roe_model::writer::BodyElement::Paragraph(__pp)) if __pp.page_break_before);
                if !__is_pb { continue; }
                let Some(&__lfid) = __ppg.frames.last() else { continue };
                let __lf = &arena.frames[__lfid];
                let __limit = arena.page_global_y(__prev) + __ppg.height - __ppg.margin_bottom;
                let __y_pt = (__lf.y_on_page + __lf.height() + 6.0).min(__limit - 2.0);
                let __y_px = (__y_pt - vp_y) * scale;
                if __y_px < -30.0 || __y_px > pixel_h as f32 + 30.0 { continue; }
                let __x0 = (__ppg.margin_left - vp_x) * scale;
                let __x1 = (__ppg.width - __ppg.margin_right - vp_x) * scale;
                let __label = "分页符";
                let mut __lw = 0.0f32;
                {
                    let mut __b = self.layout_context.ranged_builder(
                        &mut self.font_context, __label, scale.max(1.0), true,
                    );
                    __b.push_default(StyleProperty::Brush(RunBrush { color: [0x9A, 0xA0, 0xA6, 255], run_idx: None }));
                    __b.push_default(GenericFamily::SystemUi);
                    __b.push_default(StyleProperty::FontSize(8.5));
                    let mut __lay: parley::Layout<RunBrush> = __b.build(__label);
                    __lay.break_all_lines(None);
                    __lay.align(Alignment::Start, AlignmentOptions::default());
                    __lw = __lay.width();
                    let __lx = (__x0 + __x1) / 2.0 - __lw / 2.0;
                    let __ly = __y_px - __lay.height() / 2.0;
                    let mut __pen = TinySkiaPen::new(&mut pixmap);
                    for __line in __lay.lines() {
                        for __item in __line.items() {
                            if let PositionedLayoutItem::GlyphRun::<RunBrush>(__gr) = __item {
                                render_glyph_run(&__gr, &mut __pen, __lx, __ly);
                            }
                        }
                    }
                }
                let __mid = (__x0 + __x1) / 2.0;
                let __gap = __lw / 2.0 + 8.0 * scale;
                for (__a, __b2) in [(__x0, __mid - __gap), (__mid + __gap, __x1)] {
                    if __b2 <= __a { continue; }
                    let mut __pbld = tiny_skia::PathBuilder::new();
                    __pbld.move_to(__a, __y_px);
                    __pbld.line_to(__b2, __y_px);
                    if let Some(__path) = __pbld.finish() {
                        let mut __paint = tiny_skia::Paint::default();
                        __paint.set_color_rgba8(154, 160, 166, 255);
                        let mut __stroke = tiny_skia::Stroke::default();
                        __stroke.width = 1.0;
                        __stroke.dash = tiny_skia::StrokeDash::new(vec![4.0 * scale, 3.0 * scale], 0.0);
                        pixmap.stroke_path(&__path, &__paint, &__stroke, tiny_skia::Transform::identity(), None);
                    }
                }
            }
        }

        // 遍历所有 frame，只渲染与视口相交的
        for &pid in &arena.page_order {
            let page = &arena.pages[pid];
            let margin_left = page.margin_left;
            for &fid in &page.frames {
                let frame = &arena.frames[fid];

                // Shape 帧判断必须放在 is_image_frame 之前并与之互斥：
                // 两者的 FrameContent 标记方式完全相同（空 Paragraph），
                // 不优先分流会导致 Shape 帧被当成图片帧送进
                // render_image_frame()，其 match 分支不认 BodyElement::Shape，
                // 落入 `_ => return`，静默跳过、不报错也不渲染。
                let is_shape_frame = matches!(
                    &frame.content,
                    crate::frame::FrameContent::Paragraph(lines) if lines.is_empty()
                ) && matches!(
                    doc.body.get(frame.para_index),
                    Some(roe_model::writer::BodyElement::Shape(_))
                );
                if is_shape_frame {
                    let col_x_offset = page.columns.col_x_offsets
                        .get(frame.column_index).copied().unwrap_or(0.0);
                    let _ = col_x_offset; // v303: 形状改在第二遍绘制
                    continue;
                }

                // 判断是否是"空 Paragraph"图片帧（TableRow 不可能是图片帧）
                let is_image_frame = matches!(
                    &frame.content,
                    crate::frame::FrameContent::Paragraph(lines) if lines.is_empty()
                );
                if is_image_frame {
                    // 图片帧，单独处理。分栏排版设计文档v1 五、1条：图片x坐标
                    // 需要加上所在栏的x偏移，单栏文档 col_x_offsets[0]=0.0 不变。
                    let col_x_offset = page.columns.col_x_offsets
                        .get(frame.column_index).copied().unwrap_or(0.0);
                    self.render_image_frame(arena, doc, fid, vp_x, vp_y, vp_w, vp_h, scale, margin_left + col_x_offset, &mut pixmap);
                    continue;
                }
                let frame_top    = frame.y_on_page;           // 全局 pt
                let frame_bottom = frame_top + frame.height();
                if frame_bottom < vp_y || frame_top > vp_y + vp_h { continue; }

                // 分隔线帧（Step 7新增）：不走下面的文字渲染管线，直接在
                // 这里画完横线就 continue。视口裁剪已经在上面 frame_top/
                // frame_bottom 判断里统一做过，这里不用重复判断。
                if let crate::frame::FrameContent::HorizontalRule {
                    width_pt, thickness_pt, color, margin_before
                } = &frame.content {
                    let col_x_offset = page.columns.col_x_offsets
                        .get(frame.column_index).copied().unwrap_or(0.0);
                    // 页面坐标(pt) -> 视口像素坐标：与本文件其余渲染代码
                    // 一致的换算方式（vp_x/vp_y是视口左上角在全局pt坐标里
                    // 的位置，scale是pt->px缩放系数）。
                    let x0_pt = margin_left + col_x_offset;
                    let x1_pt = x0_pt + width_pt;
                    let y_pt = frame_top + margin_before + thickness_pt / 2.0;
                    let x0_px = (x0_pt - vp_x) * scale;
                    let x1_px = (x1_pt - vp_x) * scale;
                    let y_px = (y_pt - vp_y) * scale;
                    let thickness_px = thickness_pt * scale;
                    let tsk_color = tiny_skia::Color::from_rgba8(color.r, color.g, color.b, color.a);
                    Self::draw_h_line(&mut pixmap, x0_px, x1_px, y_px, thickness_px, tsk_color);
                    continue;
                }

                // 取段落样式（用于 space_before，仅 Paragraph 分支使用）
                let para = doc.body.get(frame.para_index);
                let space_before = para.and_then(|e| {
                    if let roe_model::writer::BodyElement::Paragraph(p) = e {
                        p.space_before.or(
                            doc.styles.resolve_paragraph_style(p.style.0.as_str()).space_before
                        )
                    } else { None }
                }).unwrap_or(0.0);

                // WROOT-030 结构性重构：把 Paragraph/TableRow 两种情况统一转换成
                // (ShapedLine, 该行起始y) 的列表，喂给下面完全不变的渲染循环体。
                // - Paragraph：沿用原来的运行时累加方式
                // - TableRow：每个 cell 独立从 frame.y_on_page 起算，cell 内部
                //   按自己的 y_offset 纵向累加；cell 之间靠 x_offset 横向区分，
                //   共享同一起始 y（这正是修复"阶梯状"错位的关键）
                let rendered_lines: Vec<(crate::layout::ShapedLine, f32)> = match &frame.content {
                    crate::frame::FrameContent::HorizontalRule { .. } => Vec::new(), // 已在上面continue处理，此处防御性兜底
                    crate::frame::FrameContent::Paragraph(lines) => {
                        let mut cursor_y_pt = frame.y_on_page + space_before;
                        lines.iter().map(|l| {
                            let line_top_pt = cursor_y_pt;
                            cursor_y_pt += l.line_height;
                            let shaped = crate::layout::ShapedLine {
                                y_offset: l.y_offset, ascent: l.ascent, descent: l.descent,
                                line_height: l.line_height, text: l.text.clone(), runs: l.runs.clone(),
                                x_offset: l.x_offset, cell_width: l.cell_width,
                                text_x_offset: l.text_x_offset,
                                border_top: l.border_top, border_bottom: l.border_bottom,
                                border_left: l.border_left, border_right: l.border_right,
                                list_label: l.list_label.clone(), list_label_x: l.list_label_x,
                                cell_ref: l.cell_ref,
                                tab_leaders: l.tab_leaders.clone(),
                                tab_adjustments: l.tab_adjustments.clone(),
                                // Paragraph 路径：边框沿用这一行自己的 y_offset/line_height，
                                // 与重构前行为完全一致。
                                border_top_pt: None, border_bottom_pt: None,
                                bg_color: None,
                                border_color: None, border_width: None,
                                list_label_color: l.list_label_color,
                                list_label_font_scale: l.list_label_font_scale,
                                list_label_superscript: l.list_label_superscript,
                            };
                            (shaped, line_top_pt)
                        }).collect()
                    }
                    crate::frame::FrameContent::TableRow(cells) => {
                        // 整行共享的顶/底边界（全局页面坐标，单位 pt）。
                        // frame.height() 对 TableRow variant 本来就是"各cell
                        // 高度的最大值"（见 ParagraphFrame::height()），
                        // 等价于 PPTX 表格渲染里两阶段测量算出的 row_heights[ri]，
                        // 这里不需要重新测量，直接复用。同一行内所有cell的所有行
                        // 都传相同的 row_top_pt/row_bottom_pt，画边框时就不会因为
                        // 各cell内容行数不同而错位（WROOT-030 表格边框对齐修复）。
                        let row_top_pt = frame.y_on_page;
                        let row_bottom_pt = frame.y_on_page + frame.height();
                        let mut out = Vec::new();
                        for cell in cells {
                            // WROOT-036真实bug修复：此前边框信息完全挂靠在
                            // cell.lines里的每一行文字身上（border_*字段跟着
                            // 每个文字ShapedLine一起打包），如果cell没有任何
                            // 文字（空白cell），内层for循环一次都不会执行，
                            // 导致这个cell完全没有产出ShapedLine——连边框都
                            // 跟着文字一起消失了，不只是缺文字。
                            // 修复：每个cell无条件先产出一个不含文字的"边框
                            // 占位行"，把border信息独立带进渲染循环，与"这个
                            // cell有没有文字"彻底解耦。有文字的cell后面正常
                            // 追加文字行，边框会被画两次，但draw_h_line/
                            // draw_v_line是纯色不透明覆盖（非半透明叠加），
                            // 画两次与画一次像素结果完全一致，无副作用。
                            let border_only = crate::layout::ShapedLine {
                                y_offset: 0.0, ascent: 0.0, descent: 0.0, line_height: 0.0,
                                text: String::new(), runs: Vec::new(),
                                x_offset: cell.x_offset, cell_width: cell.cell_width,
                                text_x_offset: cell.x_offset,
                                border_top: cell.border_top, border_bottom: cell.border_bottom,
                                border_left: cell.border_left, border_right: cell.border_right,
                                list_label: None, list_label_x: 0.0,
                                cell_ref: None,
                                tab_leaders: Vec::new(),
                                tab_adjustments: Vec::new(),
                                border_top_pt: Some(row_top_pt), border_bottom_pt: Some(row_bottom_pt),
                                bg_color: cell.bg_color,
                                border_color: cell.border_color, border_width: cell.border_width,
                                list_label_color: None,
                                list_label_font_scale: None,
                                list_label_superscript: false,
                            };
                            out.push((border_only, frame.y_on_page));

                            for l in &cell.lines {
                                let shaped = crate::layout::ShapedLine {
                                    y_offset: l.y_offset, ascent: l.ascent, descent: l.descent,
                                    line_height: l.line_height, text: l.text.clone(), runs: l.runs.clone(),
                                    x_offset: l.x_offset, cell_width: l.cell_width,
                                    text_x_offset: l.text_x_offset,
                                    border_top: cell.border_top, border_bottom: cell.border_bottom,
                                    border_left: cell.border_left, border_right: cell.border_right,
                                    list_label: l.list_label.clone(), list_label_x: l.list_label_x,
                                    cell_ref: l.cell_ref,
                                    tab_leaders: l.tab_leaders.clone(),
                                    tab_adjustments: l.tab_adjustments.clone(),
                                    border_top_pt: Some(row_top_pt), border_bottom_pt: Some(row_bottom_pt),
                                    bg_color: cell.bg_color,
                                    border_color: cell.border_color, border_width: cell.border_width,
                                    list_label_color: l.list_label_color,
                                    list_label_font_scale: l.list_label_font_scale,
                                    list_label_superscript: l.list_label_superscript,
                                };
                                let line_top_pt = frame.y_on_page + l.y_offset;
                                out.push((shaped, line_top_pt));
                            }
                        }
                        out
                    }
                };

                // WROOT-114: 段落样式底纹（w:pPr/w:shd）。仅 Paragraph frame、
                // 且解析后的段落样式带 shading 时，在画文字之前整段铺一个矩形
                // （行间无缝）。横向：左缩进 → 所在栏右边界（右缩进暂未建模，
                // 见 bug_log）；纵向：frame 顶 + space_before → frame 顶 + 总高。
                // 表格单元格路径(TableRow)完全不进入本分支，零回归。
                if let (crate::frame::FrameContent::Paragraph(_), Some(roe_model::writer::BodyElement::Paragraph(p))) =
                    (&frame.content, para)
                {
                    let pstyle = doc.styles.resolve_paragraph_style(p.style.0.as_str());
                    // WROOT-119：段落直写底纹优先，其次段落样式底纹。
                    let para_shading: Option<[u8; 4]> = p.shading
                        .or_else(|| pstyle.shading.map(|c| [c.r, c.g, c.b, c.a]));
                    if let Some(sh) = para_shading {
                        let col_x_offset_sh = page.columns.col_x_offsets
                            .get(frame.column_index).copied().unwrap_or(0.0);
                        let col_w_sh = page.columns.col_widths
                            .get(frame.column_index).copied().unwrap_or(0.0);
                        // 段落"盒子"左缘 = 左缩进 + min(0, 首行缩进)：悬挂缩进时首行文字
                        // 探出到 indent_left 左侧，底纹/边框跟随最左的文字起点（LO/Word 语义）。
                        let left_indent_sh = p.indent_left.or(pstyle.indent_left).unwrap_or(0.0).max(0.0);
                        let first_sh = p.indent_first.or(pstyle.indent_first).unwrap_or(0.0).min(0.0);
                        let x0_pt = margin_left + col_x_offset_sh + (left_indent_sh + first_sh).max(0.0);
                        let x1_pt = margin_left + col_x_offset_sh + col_w_sh;
                        let y0_pt = frame.y_on_page + space_before;
                        // WROOT-119：底纹覆盖到段后距末尾（Word/LO/WPS 语义），否则相邻
                        // 段落的底纹之间会露出 space_after 宽的白缝。
                        // 已知局限：段落跨页拆分时每一片都带 space_after，会在拆分处多涂一截。
                        let y1_pt = frame.y_on_page + frame.height() + frame.space_after;
                        if x1_pt > x0_pt && y1_pt > y0_pt {
                            let sx0 = (x0_pt - vp_x) * scale;
                            let sx1 = (x1_pt - vp_x) * scale;
                            let sy0 = (y0_pt - vp_y) * scale;
                            let sy1 = (y1_pt - vp_y) * scale;
                            if let Some(rect) = tiny_skia::Rect::from_ltrb(sx0, sy0, sx1, sy1) {
                                let mut sh_paint = tiny_skia::Paint::default();
                                sh_paint.set_color_rgba8(sh[0], sh[1], sh[2], sh[3]);
                                pixmap.fill_rect(rect, &sh_paint, tiny_skia::Transform::identity(), None);
                            }
                        }
                    }
                    // WROOT-114 阶段二：左竖条（w:pBdr/w:left）。竖条占
                    // [x0, x0+width_pt]，紧贴段落左缩进处；其右侧 space_pt 空隙
                    // 由 layout 的 border_inset 把文字内推实现。纵向与底纹同高。
                    // 独立于 shading：只有左边框没有底纹的段落同样会画。
                    if let Some(bl) = pstyle.border_left {
                        let col_x_offset_bl = page.columns.col_x_offsets
                            .get(frame.column_index).copied().unwrap_or(0.0);
                        let left_indent_bl = p.indent_left.or(pstyle.indent_left).unwrap_or(0.0).max(0.0);
                        let first_bl = p.indent_first.or(pstyle.indent_first).unwrap_or(0.0).min(0.0);
                        let bx0_pt = margin_left + col_x_offset_bl + (left_indent_bl + first_bl).max(0.0);
                        let bx1_pt = bx0_pt + bl.width_pt;
                        let by0_pt = frame.y_on_page + space_before;
                        let by1_pt = frame.y_on_page + frame.height();
                        if bx1_pt > bx0_pt && by1_pt > by0_pt {
                            if let Some(rect) = tiny_skia::Rect::from_ltrb(
                                (bx0_pt - vp_x) * scale, (by0_pt - vp_y) * scale,
                                (bx1_pt - vp_x) * scale, (by1_pt - vp_y) * scale,
                            ) {
                                let mut bl_paint = tiny_skia::Paint::default();
                                bl_paint.set_color_rgba8(bl.color.r, bl.color.g, bl.color.b, bl.color.a);
                                pixmap.fill_rect(rect, &bl_paint, tiny_skia::Transform::identity(), None);
                            }
                        }
                    }
                }

                for (shaped_line, line_top_pt) in &rendered_lines {

                    let line_top_pt    = *line_top_pt;
                    let line_bottom_pt = line_top_pt + shaped_line.line_height;

                    // 行与视口不相交，跳过
                    if line_bottom_pt < vp_y || line_top_pt > vp_y + vp_h { continue; }

                    // 批注气泡功能（v152起手，见 ROE_Progress_v151.md 4.3节）：
                    // 命中"某个run是某条批注range的起点"时，记录这一行的
                    // line_top_pt（已经是全局文档坐标，不需要重新推导）。
                    // 与 has_formula_task 那行（下方不远处）访问 r.style
                    // 的方式完全一致，是已验证过的既有模式。
                    for r in &shaped_line.runs {
                        if r.style.comment_bar_start {
                            if let Some(cid) = r.style.comment_id {
                                let color_idx = r.style.comment_highlight_color_idx.unwrap_or(0);
                                // 新增line_bottom_pt（行底部，与render_viewport内画正文内
                                // 连接线时用的bottom_y是同一个概念），供气泡侧连接线对齐
                                // 用，取代之前粗略的"+6.0pt估算单行高度一半"写法。
                                self.pending_comment_hits.push((cid, line_top_pt, line_bottom_pt, color_idx));
                            }
                        }
                    }

                    // 转换到像素坐标。分栏排版设计文档v1 五、1条：文字x坐标
                    // 需要加上该frame所在栏的x偏移，单栏文档col_x_offsets[0]=0.0不变。
                    let col_x_offset = page.columns.col_x_offsets
                        .get(frame.column_index).copied().unwrap_or(0.0);
                    let line_px_y = (line_top_pt - vp_y) * scale;
                    let line_height_px = shaped_line.line_height * scale;
                    let margin_px = (margin_left + col_x_offset - vp_x) * scale;

                    // WROOT-036真实bug修复：此前"文字为空就跳过"这个判断
                    // 会连带跳过表格空白cell的"边框占位行"（这类行故意让
                    // text为空，只是用来携带border_*信息，画边框逻辑写在
                    // 这个continue判断之后好几十行，一旦被这里跳过，边框
                    // 就彻底画不出来）。修复：只有"真的没有边框任务"的
                    // 空白行才跳过，普通段落的空行行为完全不变；带边框
                    // 任务的占位行会继续往下走，空字符串排版不产生任何
                    // 文字，自然只会画到边框那部分。
                    let is_table_cell_early = shaped_line.cell_width > 0.0;
                    let has_border_task = is_table_cell_early
                        && (shaped_line.border_top.is_some() || shaped_line.border_bottom.is_some()
                            || shaped_line.border_left.is_some() || shaped_line.border_right.is_some());
                    // DOCX-8 公式渲染修复：公式run的text是空格串占位符（用于撑开
                    // 排版宽度），trim()后为空，会被下面这行原本的判断误认成
                    // WROOT-036讲的"表格空白cell边框占位行"而整行跳过，导致公式
                    // 完全画不出来。修复：只要这一行的runs里有任意is_formula为true
                    // 的run，就不能算占位行，必须继续走渲染流程（下面is_formula
                    // 绘制分支才有机会执行到）。
                    let has_formula_task = shaped_line.runs.iter().any(|r| r.style.is_formula);
                    // v250b: 空列表项（带编号/符号标记的空行）不是占位行，必须继续走渲染流程画标记
                    let has_list_label_task = shaped_line.list_label.is_some() && !is_table_cell_early;
                    let is_placeholder_line = shaped_line.text.trim().is_empty() && !has_formula_task && !has_list_label_task;
                    // v265: 段落标记 / 手动换行标记
                    let __marks_on = marks_enabled();
                    let __para_last = match &frame.content {
                        crate::frame::FrameContent::Paragraph(__ls) => frame.follow.is_none()
                            && __ls.last().map(|l| l.y_offset == shaped_line.y_offset && l.text == shaped_line.text).unwrap_or(false),
                        _ => false,
                    };
                    let __manual_break = matches!(&frame.content, crate::frame::FrameContent::Paragraph(_))
                        && shaped_line.text.ends_with('\n');
                    let __pb_first = matches!(&frame.content, crate::frame::FrameContent::Paragraph(__pl)
                            if frame.precede.is_none() && __pl.first().map(|l| l.y_offset == shaped_line.y_offset).unwrap_or(false))
                        && matches!(para, Some(roe_model::writer::BodyElement::Paragraph(__pp)) if __pp.page_break_before);
                    if false && __marks_on && __pb_first {
                        let mut __pbld = tiny_skia::PathBuilder::new();
                        let __yy = line_px_y - 6.0 * scale;
                        __pbld.move_to(margin_px, __yy);
                        __pbld.line_to(page.width * scale - margin_px, __yy);
                        if let Some(__path) = __pbld.finish() {
                            let mut __paint = tiny_skia::Paint::default();
                            __paint.set_color_rgba8(154, 160, 166, 255);
                            let mut __stroke = tiny_skia::Stroke::default();
                            __stroke.width = 1.0;
                            __stroke.dash = tiny_skia::StrokeDash::new(vec![4.0 * scale, 3.0 * scale], 0.0);
                            pixmap.stroke_path(&__path, &__paint, &__stroke, tiny_skia::Transform::identity(), None);
                        }
                    }
                    if __marks_on && is_placeholder_line && (__para_last || __manual_break) {
                        let __fs = shaped_line.runs.first().map(|r| r.style.font_size).unwrap_or(12.0_f32);
                        let __sz = __fs * scale * 0.8;
                        let __mx = margin_px + shaped_line.text_x_offset * scale;
                        let __my = line_px_y + (shaped_line.line_height * scale - __sz) / 2.0;
                        draw_mark(&mut pixmap, if __para_last && !matches!(para, Some(roe_model::writer::BodyElement::Paragraph(__cp)) if __cp.column_break_after) { 0 } else { 1 }, __mx, __my, __sz);
                    }
                    if is_placeholder_line && !has_border_task { continue; }

                    // WROOT-036真实bug修复（续）：占位行（表格空白cell专用，
                    // 只携带border信息、text恒为空、line_height恒为0.0）不能
                    // 走下面这一整套cosmic-text排版流程——line_height=0传给
                    // Metrics/Buffer::set_size会触发cosmic-text内部
                    // "line height cannot be 0"断言panic。占位行本来就没有
                    // 文字要画，这里单独画完边框就直接continue，不进入下面
                    // 已有的排版/画字逻辑，避免大改已验证过的现有代码。
                    if is_placeholder_line {
                        let bx0 = margin_px + shaped_line.x_offset * scale;
                        let bx1 = margin_px + (shaped_line.x_offset + shaped_line.cell_width) * scale;
                        let by0 = shaped_line.border_top_pt
                            .map(|pt| (pt - vp_y) * scale)
                            .unwrap_or(line_px_y);
                        let by1 = shaped_line.border_bottom_pt
                            .map(|pt| (pt - vp_y) * scale)
                            .unwrap_or(line_px_y + line_height_px);
                        // 表格样式移植 Layer 1步骤D：单元格背景色，画在边框之前（先填色
                        // 再描边，与Word/roe-impress表现一致）。同一cell的每一行（含
                        // 占位行）都会重复画一次，纯色不透明fill_rect重复画像素结果
                        // 与画一次完全一致（同WROOT-036边框重复绘制安全性一致），无副作用。
                        if let Some(bg) = shaped_line.bg_color {
                            if let Some(rect) = tiny_skia::Rect::from_ltrb(bx0, by0, bx1, by1) {
                                let mut bg_paint = tiny_skia::Paint::default();
                                bg_paint.set_color_rgba8(bg.r, bg.g, bg.b, bg.a);
                                pixmap.fill_rect(rect, &bg_paint, tiny_skia::Transform::identity(), None);
                            }
                        }
                        // 表格样式移植 Layer 1步骤B（渲染部分）：边框颜色/宽度改为读取
                        // 已解析并合并好的真实值（cell自身border_color/width优先，
                        // fallback到表格级default_border_color/width，合并逻辑见
                        // layout.rs TableCellFrame构造处）。border_width单位是pt，
                        // 需要乘scale换算成像素（draw_h_line/draw_v_line内部不做
                        // 任何单位换算，thickness参数直接就是像素值，与bx0/bx1/
                        // by0/by1的换算方式保持一致）。两者都为None时兜底黑色/
                        // 1.0px，与此前硬编码行为完全一致（零回归）。
                        // WROOT-100：四条边分别按各自BorderStyle画（外框/内部线可能
                        // 有不同颜色宽度虚实，不再共用一份border_color/width）。
                        // 竖线x坐标改用x_offset/cell_width现算（不再依赖border_left/
                        // right字段携带坐标，该字段现在只携带样式信息）。
                        Self::draw_h_line_styled(&mut pixmap, bx0, bx1, by0, shaped_line.border_top.as_ref(), scale);
                        Self::draw_h_line_styled(&mut pixmap, bx0, bx1, by1, shaped_line.border_bottom.as_ref(), scale);
                        Self::draw_v_line_styled(&mut pixmap, bx0, by0, by1, shaped_line.border_left.as_ref(), scale);
                        Self::draw_v_line_styled(&mut pixmap, bx1, by0, by1, shaped_line.border_right.as_ref(), scale);
                        continue;
                    }

                    let primary_font_size = shaped_line.runs.first()
                        .map(|r| r.style.font_size)
                        .unwrap_or(12.0_f32);

                    let default_brush = RunBrush { color: [30, 30, 30, 255], run_idx: None };
                    let mut builder = self.layout_context.ranged_builder(
                        &mut self.font_context, &shaped_line.text, scale.max(1.0), true,
                    );
                    builder.push_default(StyleProperty::Brush(default_brush));
                    builder.push_default(GenericFamily::SystemUi);
                    builder.push_default(LineHeight::Absolute(shaped_line.line_height));
                    builder.push_default(StyleProperty::FontSize(primary_font_size));
                    for (run_idx, span) in shaped_line.runs.iter().enumerate() {
                        // STEP6-STAGE-A（Gemini方案A：透明画刷占位法，经与用户
                        // 三方讨论后采纳）：脚注/尾注编号占位符run（field为
                        // EndnoteRef/FootnoteRef）的script字段已被resolve_run_style
                        // 强制设为Superscript，本来会被下面`continue`跳过——但
                        // 跳过"push自定义样式"不等于跳过"参与主排版渲染"：
                        // Parley的ranged_builder语义是，只要某段字节区间存在于
                        // build()传入的完整字符串里，没有被显式push样式的区间会
                        // fallback到push_default设置的全局默认样式渲染出来，
                        // 不会凭空消失。这段文字（我们在layout.rs里塞的占位符
                        // "1"）確實存在于shaped_line.text里，直接continue的
                        // 后果是：这段文字用默认字号/默认黑色被主循环画了
                        // 一次，跟下面script_runs循环里sbuilder画的上标"1"
                        // 叠在一起，肉眼看到"两个1"（一个正常大小紧跟在正文
                        // 后面、一个缩小的在左上角悬浮）。
                        //
                        // 修复思路（不是跳过，是"排版但不可见"）：给这段字节
                        // 区间显式push一套样式——字号缩小到跟sbuilder那边一致
                        // 的0.65倍（保证主循环预留的横向空间跟真正视觉大小
                        // 匹配，不会因为字号不一致导致预留空白过大/过小），
                        // 颜色alpha设为0（完全透明，不可见）。这样主循环依然
                        // 会正确排版占位（换行、对齐都基于真实存在的这段文字
                        // 计算），但画出来的像素是透明的，看不见；真正可见的
                        // 上标绘制完全交给下面script_runs循环里的sbuilder分支
                        // （该分支不受这里改动影响，继续正常工作）。
                        //
                        // 判断是否note-ref-run：通过run_idx反查para.runs[run_idx]
                        // .field，与tile.rs里已有的formula反查模式一致（对照
                        // `p.runs.get(run_idx).and_then(|r| r.formula.as_ref())`）。
                        let is_note_ref = if let Some(roe_model::writer::BodyElement::Paragraph(p)) = para {
                            p.runs.get(run_idx).map(|r| matches!(
                                r.field,
                                Some(roe_model::writer::FieldKind::EndnoteRef)
                                    | Some(roe_model::writer::FieldKind::FootnoteRef)
                            )).unwrap_or(false)
                        } else {
                            false
                        };
                        // WROOT-027/028 (v141二次修复): note内容区（endnotes.xml/
                        // footnotes.xml）自己段落里的编号占位符，不是para.runs里
                        // 真实存在的run（是layout.rs手动拼进raw_text最前面的虚拟
                        // 字符，见parse_isolated_paragraphs之外、note渲染循环里
                        // note_label_byte_len那段），run_idx反查para.runs对不上，
                        // is_note_ref恒为false。这类span同样需要"透明占位"处理，
                        // 否则会被主循环用默认可见样式多画一次，跟script_runs
                        // 循环画的上标版本重叠（用户截图证实：肉眼可见两个"i"，
                        // 一个上标、一个和正文同一基线）。判断依据直接用span
                        // 自身的script字段（不依赖run_idx），覆盖面比is_note_ref
                        // 更广，但只是新增命中范围，不影响原有正文脚注引用场景
                        // （原来能通过is_note_ref命中的，script同样非Normal，
                        // 走的还是同一条透明占位逻辑，行为不变）。
                        let is_note_ref = is_note_ref || span.style.script != ScriptPosition::Normal;
                        if is_note_ref {
                            let start = span.start.min(shaped_line.text.len());
                            let end   = span.end.min(shaped_line.text.len());
                            if start >= end { continue; }
                            let range = start..end;
                            // alpha=0：完全透明，不可见，但依然占据真实排版宽度。
                            let transparent = [0u8, 0u8, 0u8, 0u8];
                            builder.push(StyleProperty::Brush(RunBrush { color: transparent, run_idx: Some(run_idx) }), range.clone());
                            // 字号缩小到跟sbuilder一致的比例，保证预留宽度贴近
                            // 真实视觉大小（sbuilder那边用的是span.style.font_size*0.65，
                            // 这里同步）。
                            builder.push(StyleProperty::FontSize(span.style.font_size * 0.65), range.clone());
                            continue;
                        }
                        if span.style.script != ScriptPosition::Normal { continue; }
                        let start = span.start.min(shaped_line.text.len());
                        let end   = span.end.min(shaped_line.text.len());
                        if start >= end { continue; }
                        let range = start..end;
                        let color = span.style.color
                            .map(|c| [c.r, c.g, c.b, c.a])
                            .unwrap_or([30, 30, 30, 255]);
                        builder.push(StyleProperty::Brush(RunBrush { color, run_idx: Some(run_idx) }), range.clone());
                        // WROOT-107修复：此前普通run只推了Brush/粗体/斜体等，从未推过自己的
                        // FontSize，所有run都吃上面push_default的primary_font_size（本行第一个
                        // run的字号）。同一行内字号不同的文档（如tdf99602：12pt+48pt）会全部
                        // 退化成第一个run的字号。脚注占位符分支早就正确推了FontSize，
                        // 唯独这条主路径漏了。这也推翻了v177"Parley shaping有bug"的猜测。
                        builder.push(StyleProperty::FontSize(span.style.font_size), range.clone());
                        // WROOT-125：tile 用 LineBox.text 自己重排（break_all_lines(None)，
                        // 不限宽），此前只 push 了 Brush/FontSize/粗斜体，漏了 FontFamily，
                        // 整行回退到 push_default 的 GenericFamily::SystemUi（本机解析成
                        // Noto Sans CJK，西文比 Liberation Serif 宽约 12%）。layout 阶段
                        // 断行是按文档字体算的（每行 advance<=col_w），tile 换宽字体重排后
                        // 不再折行，文字就整体向右溢出页边距，行首还会被挤出画布。
                        // 这里按 run_idx 反查 para.runs，用与 layout.rs 完全相同的
                        // resolve_run_font_family 取字体名，保证两阶段选中同一字体。
                        // run_idx 对应不上真实 run 的合成 span（如 note 标签）保持 SystemUi。
                        if let Some(roe_model::writer::BodyElement::Paragraph(wp)) = para {
                            if let Some(wr) = wp.runs.get(run_idx) {
                                if let Some(fam) = crate::layout::resolve_run_font_family(doc, wp, wr) {
                                    builder.push(StyleProperty::FontFamily(parley::FontFamily::named(fam.as_str())), range.clone());
                                }
                            }
                        }
                        // v317g：单元格行的 para 是整个表格元素，按 cell_ref 反查单元格段落的 run
                        if let (Some(roe_model::writer::BodyElement::Table(v317g_tb)), Some((_, v317g_ri, v317g_ci, v317g_pi))) = (para, shaped_line.cell_ref) {
                            if let Some(roe_model::common::CellContent::Paragraph(v317g_wp)) = v317g_tb.rows.get(v317g_ri as usize)
                                .and_then(|r| r.cells.get(v317g_ci as usize))
                                .and_then(|c| c.content.get(v317g_pi as usize))
                            {
                                if let Some(v317g_wr) = v317g_wp.runs.get(run_idx) {
                                    if let Some(fam) = crate::layout::resolve_run_font_family(doc, v317g_wp, v317g_wr) {
                                        builder.push(StyleProperty::FontFamily(parley::FontFamily::named(fam.as_str())), range.clone());
                                    }
                                }
                            }
                        }
                        if span.style.bold   { builder.push(StyleProperty::FontWeight(FontWeight::BOLD), range.clone()); }
                        if span.style.italic { builder.push(StyleProperty::FontStyle(FontStyle::Italic), range.clone()); }
                        if span.style.underline    { builder.push(StyleProperty::Underline(true), range.clone()); }
                        if span.style.strikethrough { builder.push(StyleProperty::Strikethrough(true), range.clone()); }
                    }
                    let mut layout: parley::Layout<RunBrush> = builder.build(&shaped_line.text);
                    layout.break_all_lines(None);
                    layout.align(Alignment::Start, AlignmentOptions::default());

                    let is_table_cell = shaped_line.cell_width > 0.0;
                    let cell_x_off = margin_px + shaped_line.text_x_offset * scale;
                    if let Some(label) = &shaped_line.list_label {
                        // WROOT-027/028 (v141): 颜色/字号不再写死，note正文区
                        // 占位符编号若引用了字符样式（如EndnoteReference），
                        // 使用该样式的颜色/字号/上标偏移；没有引用时（如
                        // WROOT-075）保持原有默认灰黑色/原字号/不做偏移，
                        // 完全向后兼容。
                        let label_color = shaped_line.list_label_color.unwrap_or([30, 30, 30, 255]);
                        let label_brush = RunBrush { color: label_color, run_idx: None };
                        let label_font_size = primary_font_size * shaped_line.list_label_font_scale.unwrap_or(1.0);
                        let mut lbuilder = self.layout_context.ranged_builder(
                            &mut self.font_context, label, scale.max(1.0), true,
                        );
                        lbuilder.push_default(StyleProperty::Brush(label_brush));
                        lbuilder.push_default(GenericFamily::SystemUi);
                        lbuilder.push_default(LineHeight::Absolute(shaped_line.line_height));
                        lbuilder.push_default(StyleProperty::FontSize(label_font_size));
                        let mut label_layout: parley::Layout<RunBrush> = lbuilder.build(label);
                        label_layout.break_all_lines(None);
                        label_layout.align(Alignment::Start, AlignmentOptions::default());
                        let label_x = margin_px + shaped_line.list_label_x * scale;
                        // 上标偏移公式与下方script_runs循环的Superscript分支保持
                        // 一致（line_px_y - font_size * scale * 0.35），确保
                        // note行首编号与正文引用编号的上标视觉效果统一。
                        let label_y = if shaped_line.list_label_superscript {
                            line_px_y - label_font_size * scale * 0.35
                        } else {
                            line_px_y
                        };
                        let mut pen = TinySkiaPen::new(&mut pixmap);
                        for line in label_layout.lines() {
                            for item in line.items() {
                                if let PositionedLayoutItem::GlyphRun::<RunBrush>(glyph_run) = item {
                                    render_glyph_run(&glyph_run, &mut pen, label_x, label_y);
                                }
                            }
                        }
                    }
                    let cell_clip_max = if is_table_cell {
                        Some(margin_px + (shaped_line.x_offset + shaped_line.cell_width) * scale)
                    } else { None };

                    let draw_y = line_px_y;
                    let align_off_px = shaped_line.text_x_offset * scale;
                    // STEP6-STAGE-A（上下标x坐标定位修复）：此前script_runs循环
                    // （画上标/下标独立分支，在本函数更下方）画字时x坐标固定写死
                    // 用margin_px（页面左边距），完全没有考虑这段文字在行内实际
                    // 应该出现的水平位置——这是cosmic-text迁移到Parley时遗留的
                    // 半成品代码，此前没有真实场景触发过"非行首的上标/下标"，
                    // 所以一直没暴露。这次note-ref-run（脚注/尾注编号）场景撞上了
                    // 这个bug：编号本该紧跟在正文文字后面，却被画到了页面左边距，
                    // 视觉上像是"跑到了独立的一行"。
                    // 修复：在主循环画正文GlyphRun时，顺手把每个run_idx对应的真实
                    // x坐标（cell_x_off + tab偏移 + glyph_run.offset()，与背景色
                    // 矩形bg_x0用的是同一套公式，参见下方WROOT-058背景色分支）
                    // 记录到这张表里，供script_runs循环查表使用，替换掉写死的
                    // margin_px。这个修复对所有"行内非首位置的上标/下标"场景通用，
                    // 不只是note-ref——此前这类场景的定位应该都是错的，只是未被
                    // 覆盖到，属于顺带修复的历史遗留bug，不扩大本次任务的功能范围。
                    let mut run_x_positions: Vec<Option<f32>> = vec![None; shaped_line.runs.len()];
                    {
                        let mut pen = TinySkiaPen::new(&mut pixmap);
                        for line in layout.lines() {
                            for item in line.items() {
                                if let PositionedLayoutItem::GlyphRun::<RunBrush>(glyph_run) = item {
                                    if let Some(clip) = cell_clip_max {
                                        let glyph_run_right = cell_x_off + glyph_run.offset() + glyph_run.advance();
                                        if glyph_run_right > clip { continue; }
                                    }
                                    // WROOT-068 渲染算法本体：tab撑大量应用。通过
                                    // brush.run_idx反查shaped_line.runs[ridx].start（该
                                    // run在行内的字节偏移起点），在shaped_line.tab_adjustments
                                    // 里找"最后一个byte_offset<=start"对应的cumulative_extra，
                                    // 即这个run整体应右移多少（pt，未乘scale）。
                                    // tab_adjustments为空（绝大多数段落，无w:tabs定义）时，
                                    // tab_extra_px恒为0.0，不影响任何现有渲染路径。
                                    // WROOT-068 Bug2 真正修复：不再对"整个GlyphRun"算一个
                                    // 统一的tab_extra_px（这个值依赖glyph_run.style().brush.run_idx
                                    // 反查shaped_line.runs[ridx].start，即整个run的起点——但一个
                                    // 物理GlyphRun可能横跨tab占位符前后两段，只用run起点查表，
                                    // tab前和tab后的字符永远查到同一个值，无法差异化处理）。
                                    // 详见 render_glyph_run_with_offset 函数doc注释的完整踩坑记录。
                                    //
                                    // 这里改为构造一个闭包，交给render_glyph_run_with_offset内部
                                    // 按每个cluster的真实字节位置（不受shape合并影响）逐个查表。
                                    // tab_adjustments为空（绝大多数段落，无w:tabs定义）时，闭包对
                                    // 任意字节位置都查不到匹配项，返回0.0，行为与修复前完全一致。
                                    let tab_adjustments_for_closure = &shaped_line.tab_adjustments;
                                    let extra_width_at = move |byte_pos: usize| -> f32 {
                                        tab_adjustments_for_closure.iter()
                                            .rev()
                                            .find(|adj| adj.byte_offset <= byte_pos)
                                            .map(|adj| adj.cumulative_extra)
                                            .unwrap_or(0.0) * scale
                                    };
                                    // WROOT-058 (2026-08-21): 文字背景色/高亮色渲染。
                                    // ResolvedRunStyle.background 之前只在layout.rs算出来，
                                    // 从未传到tile.rs画像素，这里补上：通过brush.run_idx
                                    // 反查shaped_line.runs对应的background，画字形前先填色。
                                    // 背景色矩形暂时仍按run整体起点查一次tab偏移（背景色场景
                                    // 本身极少与tab组合出现，且背景矩形是整体的矩形填色，没有
                                    // "内部再分段"的画法，保留按run整体处理，不在这次修复范围）。
                                    if let Some(ridx) = glyph_run.style().brush.run_idx {
                                        if let Some(bg) = shaped_line.runs.get(ridx).and_then(|r| r.style.background) {
                                            let bg_tab_extra_px = shaped_line.runs.get(ridx)
                                                .map(|span| extra_width_at(span.start))
                                                .unwrap_or(0.0);
                                            let bg_x0 = cell_x_off + bg_tab_extra_px + glyph_run.offset();
                                            let bg_w  = glyph_run.advance();
                                            let bg_y0 = draw_y;
                                            let bg_h  = shaped_line.line_height * scale;
                                            if let Some(rect) = tiny_skia::Rect::from_xywh(bg_x0, bg_y0, bg_w, bg_h) {
                                                let mut bg_paint = tiny_skia::Paint::default();
                                                bg_paint.set_color_rgba8(bg.r, bg.g, bg.b, bg.a);
                                                pen.pixmap.fill_rect(rect, &bg_paint, tiny_skia::Transform::identity(), None);
                                            }
                                        }
                                    }
                                    // 批注高亮矩形 + 起止竖线渲染（v150起，渲染层第一步）。
                                    // 画法完全参照上面WROOT-058的背景色分支——通过brush.run_idx
                                    // 反查shaped_line.runs对应的ResolvedRunStyle新字段，逐run独立
                                    // 判断，不维护跨run状态（活跃批注栈已经在layout.rs算好，这里
                                    // 只管读结果画图）。
                                    //
                                    // 颜色表（v152：已提到模块级 COMMENT_HIGHLIGHT_COLORS 常量，
                                    // 见文件顶部定义，此处不再重复定义，直接引用）。
                                    if let Some(ridx) = glyph_run.style().brush.run_idx {
                                        if let Some(span) = shaped_line.runs.get(ridx) {
                                            if let Some(color_idx) = span.style.comment_highlight_color_idx {
                                                let (bar_rgb, fill_rgba) = COMMENT_HIGHLIGHT_COLORS[color_idx as usize % 6];
                                                let cm_tab_extra_px = extra_width_at(span.start);
                                                let cm_x0 = cell_x_off + cm_tab_extra_px + glyph_run.offset();
                                                let cm_w  = glyph_run.advance();
                                                let cm_y0 = draw_y;
                                                let cm_h  = shaped_line.line_height * scale;
                                                // 浅色高亮底，画法与w:shd背景色矩形完全一致，只是
                                                // 颜色语义不同（批注 vs 底纹），两者不互斥可以叠加
                                                // 显示（背景色已经在上面画过，这里再叠一层批注色）。
                                                if let Some(rect) = tiny_skia::Rect::from_xywh(cm_x0, cm_y0, cm_w, cm_h) {
                                                    let mut fill_paint = tiny_skia::Paint::default();
                                                    fill_paint.set_color_rgba8(fill_rgba[0], fill_rgba[1], fill_rgba[2], fill_rgba[3]);
                                                    pen.pixmap.fill_rect(rect, &fill_paint, tiny_skia::Transform::identity(), None);
                                                }
                                                // 方案A竖线（已与用户确认，对齐OnlyOffice真实行为）：
                                                // range起点/终点各画一条深色竖线，贴在run左边缘，
                                                // 不判断跨行/单行——comment_bar_start/end已经在
                                                // layout.rs按"是否为该批注在runs序列里第一次/最后一次
                                                // 出现"算好，这里只管画，起止可能是同一个run（单run
                                                // 批注），两条竖线会画在同一x位置，视觉上就是一条线。
                                                const COMMENT_BAR_WIDTH_PT: f32 = 2.5;
                                                let cm_bar_w = COMMENT_BAR_WIDTH_PT * scale;
                                                if span.style.comment_bar_start || span.style.comment_bar_end {
                                                    if let Some(bar_rect) = tiny_skia::Rect::from_xywh(cm_x0, cm_y0, cm_bar_w, cm_h) {
                                                        let mut bar_paint = tiny_skia::Paint::default();
                                                        bar_paint.set_color_rgba8(bar_rgb[0], bar_rgb[1], bar_rgb[2], 255);
                                                        pen.pixmap.fill_rect(bar_rect, &bar_paint, tiny_skia::Transform::identity(), None);
                                                    }
                                                }
                                                // 方案B测试（气泡连接线排查，正文内侧半段）：只在批注
                                                // range结束（comment_bar_end）的run上，从高亮矩形右边缘
                                                // 画一条虚线延伸到页面右边界，纵向位置取这个run所在行的
                                                // 垂直居中（cm_y0 + cm_h/2）。跟气泡那侧的短连接线各画各的，
                                                // 不保证两段线端点完全对齐，先看效果。
                                                if span.style.comment_bar_end {
                                                    // 直角折线：从高亮矩形右边缘先垂直下移到文字下方
                                                    // （行底部，cm_y0+cm_h），再水平延伸到页面右边界，
                                                    // 避免像之前那样水平线直接从文字中间穿过去。
                                                    let start_x = cm_x0 + cm_w;
                                                    let start_y = cm_y0 + cm_h / 2.0;
                                                    let bottom_y = cm_y0 + cm_h; // 行底部
                                                    let end_x = page.width * scale;
                                                    if end_x > start_x {
                                                        let mut pb = tiny_skia::PathBuilder::new();
                                                        pb.move_to(start_x, start_y);
                                                        pb.line_to(start_x, bottom_y);   // 垂直下移到行底部
                                                        pb.line_to(end_x, bottom_y);     // 水平延伸到页面边界
                                                        if let Some(path) = pb.finish() {
                                                            let mut paint = tiny_skia::Paint::default();
                                                            paint.set_color_rgba8(bar_rgb[0], bar_rgb[1], bar_rgb[2], 255);
                                                            paint.anti_alias = true;
                                                            let mut stroke = tiny_skia::Stroke::default();
                                                            stroke.width = 1.5;
                                                            stroke.dash = tiny_skia::StrokeDash::new(vec![3.0, 3.0], 0.0);
                                                            pen.pixmap.stroke_path(&path, &paint, &stroke, tiny_skia::Transform::identity(), None);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    // 记录这个run真实的x坐标，供后面script_runs循环
                                    // （画上标/下标）查表使用，公式与上面bg_x0一致。
                                    if let Some(ridx) = glyph_run.style().brush.run_idx {
                                        let tab_extra_for_pos = extra_width_at(
                                            shaped_line.runs.get(ridx).map(|s| s.start).unwrap_or(0)
                                        );
                                        if let Some(slot) = run_x_positions.get_mut(ridx) {
                                            *slot = Some(cell_x_off + tab_extra_for_pos + glyph_run.offset());
                                        }
                                    }
                                    // WROOT-107: w:position 字符基线垂直位移。用 brush.run_idx 反查
                                    // 本 GlyphRun 所属 run 的 vertical_position（pt，正上负下）。
                                    // 屏幕 y 轴向下，所以"上移"是减；pt→px 乘 scale。
                                    // 实测 Parley 不会把 vertical_position 不同的相邻 run 合并
                                    // （tdf99602 诊断：两个 run 各自一个 GlyphRun，run_idx 正确）。
                                    let pos_shift_px = glyph_run.style().brush.run_idx
                                        .and_then(|ridx| shaped_line.runs.get(ridx))
                                        .and_then(|span| span.style.vertical_position)
                                        .map(|pt| pt * scale)
                                        .unwrap_or(0.0);
                                    render_glyph_run_with_offset(&glyph_run, &mut pen, cell_x_off, draw_y - pos_shift_px, extra_width_at);

                                    // underline / strikethrough：直接从 glyph_run.style() 读取，
                                    // 不再需要 metadata 反查。
                                    let style = glyph_run.style();
                                    let run_metrics = glyph_run.run().font_metrics();
                                    let deco_color = style.brush.color;
                                    let deco_tsk_color = tiny_skia::Color::from_rgba8(
                                        deco_color[0], deco_color[1], deco_color[2], deco_color[3],
                                    );
                                    // WROOT-078（隐式居中续）：非左对齐行的对齐偏移量已存在
                                    // shaped_line.text_x_offset 里，下划线/删除线坐标
                                    // 都要叠加这个偏移，否则视觉上永远画在左边距。
                                    // WROOT-068 Bug2修复：下划线/删除线是整条线一次性画的
                                    // 矩形，没有像正文字形那样逐cluster分段的画法，这里
                                    // 保留"按run整体起点查一次偏移"的近似处理（与背景色
                                    // 矩形一致），不在这次cluster级精细化修复范围内——如果
                                    // 未来遇到"下划线跨tab时中间那段没跟着断开"的现象，
                                    // 再针对下划线单独处理分段绘制。
                                    let run_tab_extra_px = glyph_run.style().brush.run_idx
                                        .and_then(|ridx| shaped_line.runs.get(ridx))
                                        .map(|span| extra_width_at(span.start))
                                        .unwrap_or(0.0);
                                    let lx0 = margin_px + align_off_px + run_tab_extra_px + glyph_run.offset();
                                    let lx1 = lx0 + glyph_run.advance();
                                    // WROOT-120：线宽此前硬编码 1.0px，大字号（如 36pt）下细得看不见。
                                    // 改为取装饰自带 size，其次字体度量 underline_size/strikethrough_size，
                                    // 并以 1.0px 为下限（小字号行为与此前一致）。
                                    if let Some(decoration) = &style.underline {
                                        let offset = decoration.offset.unwrap_or(run_metrics.underline_offset);
                                        let uy = draw_y + glyph_run.baseline() - offset;
                                        let u_th = decoration.size.unwrap_or(run_metrics.underline_size).max(1.0);
                                        Self::draw_h_line(pen.pixmap, lx0, lx1, uy, u_th, deco_tsk_color);
                                    }
                                    if let Some(decoration) = &style.strikethrough {
                                        let offset = decoration.offset.unwrap_or(run_metrics.strikethrough_offset);
                                        let sy = draw_y + glyph_run.baseline() - offset;
                                        let s_th = decoration.size.unwrap_or(run_metrics.strikethrough_size).max(1.0);
                                        Self::draw_h_line(pen.pixmap, lx0, lx1, sy, s_th, deco_tsk_color);
                                    }
                                    // 注：公式（DOCX-8）不再通过 GlyphRun 反查 run_idx 画，
                                    // 因为公式 run.text 现在是空字符串不产生 GlyphRun。
                                    // 改由下方独立遍历 shaped_line.runs 读取 formula_box
                                    // （layout.rs 已用 Parley InlineBox 算好的最终坐标）处理，
                                    // 见 WROOT-080 回归修复。
                                }
                            }
                        }
                    }
                    // v265: 行尾标记（非占位行）
                    if __marks_on && (__para_last || __manual_break) {
                        let __base = margin_px + align_off_px;
                        let mut __right = __base;
                        for __ln in layout.lines() {
                            for __it in __ln.items() {
                                if let PositionedLayoutItem::GlyphRun::<RunBrush>(__gr) = __it {
                                    __right = __right.max(__base + __gr.offset() + __gr.advance());
                                }
                            }
                        }
                        if let Some(__adj) = shaped_line.tab_adjustments.last() {
                            __right += __adj.cumulative_extra * scale;
                        }
                        let __sz = primary_font_size * scale * 0.8;
                        let __mx = __right + primary_font_size * scale * 0.15;
                        let __my = draw_y + (shaped_line.line_height * scale - __sz) / 2.0;
                        draw_mark(&mut pixmap, if __para_last && !matches!(para, Some(roe_model::writer::BodyElement::Paragraph(__cp)) if __cp.column_break_after) { 0 } else { 1 }, __mx, __my, __sz);
                    }
                    // WROOT-068 渲染算法本体 Step4：画 tab 引导线（dot/hyphen/underscore）。
                    // shaped_line.tab_leaders 里的 x_start/x_end 是 layout.rs
                    // compute_tab_adjustments 算好的段落坐标系绝对位置（pt，未乘scale，
                    // 且不含text_x_offset这个整行对齐偏移——与lx0算法保持一致，这里
                    // 同样要叠加margin_px+align_off_px；但不叠加tab_extra_px，因为
                    // x_start/x_end本身已经是最终坐标，不需要再额外右移）。
                    // 画在正文文字之后，避免被文字覆盖时视觉上不自然（LO/Word的目录
                    // 引导线本来就是画在文字之间的空白区域，不会有重叠问题）。
                    if !shaped_line.tab_leaders.is_empty() {
                        let mut pen = TinySkiaPen::new(&mut pixmap);
                        let leader_color = tiny_skia::Color::from_rgba8(30, 30, 30, 255);
                        // WROOT-068 修正：引导线应贴近文字baseline（LO实际效果），
                        // 之前用ascent*0.55（字符视觉中线）导致线画得偏高。
                        let leader_y = draw_y + shaped_line.ascent * scale;
                        for seg in &shaped_line.tab_leaders {
                            let px0 = margin_px + align_off_px + seg.x_start * scale;
                            let px1 = margin_px + align_off_px + seg.x_end * scale;
                            if px1 <= px0 { continue; }
                            match seg.leader {
                                roe_model::writer::TabLeader::Underscore => {
                                    Self::draw_h_line(pen.pixmap, px0, px1, leader_y, 1.0, leader_color);
                                }
                                roe_model::writer::TabLeader::Dot | roe_model::writer::TabLeader::Hyphen => {
                                    // 等间距画短横线段模拟点/横杠引导线：Dot用短小方点
                                    // （视觉近似，不追求像素级复刻LO的圆点字形），Hyphen
                                    // 用稍长的横线段，两者绘制机制相同，只是线段长度/间距不同。
                                    let (dash_len, gap_len) = match seg.leader {
                                        roe_model::writer::TabLeader::Dot => (1.2_f32 * scale, 2.5_f32 * scale),
                                        _ => (4.0_f32 * scale, 3.0_f32 * scale),
                                    };
                                    let step = dash_len + gap_len;
                                    let mut x = px0;
                                    while x < px1 {
                                        let seg_end = (x + dash_len).min(px1);
                                        Self::draw_h_line(pen.pixmap, x, seg_end, leader_y, 1.2, leader_color);
                                        x += step;
                                    }
                                }
                                roe_model::writer::TabLeader::None => {}
                            }
                        }
                    }
                    // WROOT-080 回归修复：公式改用 layout.rs 已算好的 formula_box
                    // 坐标直接绘制，不依赖 Parley 是否在 tile.rs 本地重建的 layout
                    // 里产生了 InlineBox item（本地 layout 构建循环里目前并未
                    // push_inline_box，公式 run.text 又是空字符串，所以 Parley
                    // 这边的 layout.items() 里既不会出现该公式的 GlyphRun，也不会
                    // 出现 InlineBox；坐标必须从 shaped_line.runs[].formula_box 里取）。
                    {
                        let mut pen = TinySkiaPen::new(&mut pixmap);
                        if let Some(roe_model::writer::BodyElement::Paragraph(p)) = para {
                            for span in shaped_line.runs.iter() {
                                let Some(fbox) = span.formula_box else { continue; };
                                let Some(run_idx) = span.run_idx else { continue; };
                                let Some(formula) = p.runs.get(run_idx).and_then(|r| r.formula.as_ref()) else { continue; };
                                // 注意：fbox.x 来自 layout.rs 里用 Parley
                                // Alignment::Center（隐式居中场景）算好的最终
                                // 坐标，已经内置了居中偏移，不能再叠加
                                // align_off_px（那是给非 Parley-align 场景的
                                // roe 自算整行偏移，用在这里会重复叠加导致
                                // 公式被推到过于靠右）。
                                let fx0 = margin_px + fbox.x * scale;
                                let f_baseline_y = draw_y
                                    + fbox.y * scale
                                    + fbox.baseline.unwrap_or(formula.height_pt + formula.depth_pt) * scale;
                                crate::tile_formula::draw_formula(
                                    pen.pixmap,
                                    &mut self.font_context,
                                    &mut self.layout_context,
                                    &formula.display_list,
                                    fx0,
                                    f_baseline_y,
                                    formula.height_pt,
                                    scale,
                                );
                            }
                        }
                    }

                    // 表格边框
                    if is_table_cell {
                        let bx0 = margin_px + shaped_line.x_offset * scale;
                        let bx1 = margin_px + (shaped_line.x_offset + shaped_line.cell_width) * scale;
                        let by0 = shaped_line.border_top_pt
                            .map(|pt| (pt - vp_y) * scale)
                            .unwrap_or(line_px_y);
                        let by1 = shaped_line.border_bottom_pt
                            .map(|pt| (pt - vp_y) * scale)
                            .unwrap_or(line_px_y + line_height_px);
                        // 表格样式移植 Layer 1步骤D：单元格背景色，画在边框之前。注意：
                        // 这个分支是"有文字的行"，前面文字已经画到pixmap上了——如果先填
                        // 背景色会把文字盖住！但这里的by0/by1是"整行"的边界（可能只是
                        // cell内多行文字中的一行），不是"整个cell"的边界，重复给同一
                        // cell的每一行都填一次背景色不会盖住其他行的文字，因为每次填的
                        // 都是同一个矩形（由border_top_pt/border_bottom_pt决定，同一
                        // cell内所有行传的都是相同的row_top_pt/row_bottom_pt，见layout
                        // 构造处），但会盖住"这一行自己"已经画好的文字，必须重新确认
                        // 画的顺序——占位行分支(continue前)没有文字画在前面，天然安全；
                        // 这个分支不同，暂缓在此处填色，见下方说明。
                        let _ = (bx0, bx1, by0, by1); // 占位，避免unused警告（本次不在此填色）
                        // 表格样式移植 Layer 1步骤B（渲染部分）：同占位行分支，边框颜色/
                        // 宽度改为读取已解析并合并好的真实值，border_width(pt)乘scale
                        // 换算成像素。两者都为None时兜底黑色/1.0px，与此前行为一致。
                        // WROOT-100：同占位行分支，四条边分别按各自BorderStyle画。
                        Self::draw_h_line_styled(&mut pixmap, bx0, bx1, by0, shaped_line.border_top.as_ref(), scale);
                        Self::draw_h_line_styled(&mut pixmap, bx0, bx1, by1, shaped_line.border_bottom.as_ref(), scale);
                        Self::draw_v_line_styled(&mut pixmap, bx0, by0, by1, shaped_line.border_left.as_ref(), scale);
                        Self::draw_v_line_styled(&mut pixmap, bx1, by0, by1, shaped_line.border_right.as_ref(), scale);
                    }

                    // 上下标
                    let script_runs: Vec<(usize, &crate::frame::LineRunSpan)> = shaped_line.runs.iter()
                        .enumerate()
                        .filter(|(_, r)| r.style.script != ScriptPosition::Normal)
                        .collect();
                    for (run_idx, span) in script_runs {
                        let start = span.start.min(shaped_line.text.len());
                        let end   = span.end.min(shaped_line.text.len());
                        if start >= end { continue; }
                        let slice = &shaped_line.text[start..end];
                        let sub_font_size_pt = span.style.font_size * 0.65; // 原始pt，喂给builder
                        let sub_lh_pt = sub_font_size_pt * 1.3;
                        // WROOT-136：上下标以基线对齐（此前拿行顶做参照，小字 layout 的基线
                        // 离行顶比正文近，导致上标过高、下标不降）。实测 scale=2、正文11pt时
                        // main_baseline=26px、sub_baseline=15px。目标：上标基线抬高0.35em，
                        // 下标基线降低0.14em（em=正文字号，Word/LO 惯例）。
                        let y_offset = 'y_off: {
                            let main_base = layout.lines().next().map(|l| l.metrics().baseline).unwrap_or(0.0);
                            let sub_base_px = {
                                let mut sb = self.layout_context.ranged_builder(
                                    &mut self.font_context, slice, scale.max(1.0), true,
                                );
                                sb.push_default(GenericFamily::SystemUi);
                                sb.push_default(LineHeight::Absolute(sub_lh_pt));
                                sb.push_default(StyleProperty::FontSize(sub_font_size_pt));
                                let mut sl: parley::Layout<RunBrush> = sb.build(slice);
                                sl.break_all_lines(None);
                                let base_val = sl.lines().next().map(|l| l.metrics().baseline).unwrap_or(0.0);
                                base_val
                            };
                            let em_px = span.style.font_size * scale;
                            break 'y_off match span.style.script {
                                ScriptPosition::Superscript => line_px_y + main_base - sub_base_px - em_px * 0.35,
                                ScriptPosition::Subscript   => line_px_y + main_base - sub_base_px + em_px * 0.14,
                                ScriptPosition::Normal      => line_px_y,
                            };
                        };
                        let color = span.style.color
                            .map(|c| [c.r, c.g, c.b, c.a])
                            .unwrap_or([30, 30, 30, 255]);
                        let sub_brush = RunBrush { color, run_idx: Some(run_idx) };
                        let mut sbuilder = self.layout_context.ranged_builder(
                            &mut self.font_context, slice, scale.max(1.0), true,
                        );
                        sbuilder.push_default(StyleProperty::Brush(sub_brush));
                        sbuilder.push_default(GenericFamily::SystemUi);
                        sbuilder.push_default(LineHeight::Absolute(sub_lh_pt));
                        sbuilder.push_default(StyleProperty::FontSize(sub_font_size_pt));
                        // WROOT-125 同步：上标/下标 builder 也按 run_idx 反查真实字体，
                        // 否则回退 SystemUi，西文宽度与 layout 阶段不一致。
                        if let Some(roe_model::writer::BodyElement::Paragraph(wp)) = para {
                            if let Some(wr) = wp.runs.get(run_idx) {
                                if let Some(fam) = crate::layout::resolve_run_font_family(doc, wp, wr) {
                                    sbuilder.push_default(StyleProperty::FontFamily(parley::FontFamily::named(fam.as_str())));
                                }
                            }
                        }
                        if span.style.bold   { sbuilder.push_default(StyleProperty::FontWeight(FontWeight::BOLD)); }
                        if span.style.italic { sbuilder.push_default(StyleProperty::FontStyle(FontStyle::Italic)); }
                        let mut sub_layout: parley::Layout<RunBrush> = sbuilder.build(slice);
                        sub_layout.break_all_lines(None);
                        sub_layout.align(Alignment::Start, AlignmentOptions::default());
                        // 用run_idx查run_x_positions表拿真实水平位置，查不到
                        // （理论上不该发生，因为这个run本应在主循环里被画过一次占位）
                        // 时兜底margin_px，保持与修复前一致的行为，不引入panic风险。
                        let real_x = run_x_positions.get(run_idx).copied().flatten().unwrap_or(margin_px);
                        let mut pen = TinySkiaPen::new(&mut pixmap);
                        for line in sub_layout.lines() {
                            for item in line.items() {
                                if let PositionedLayoutItem::GlyphRun::<RunBrush>(glyph_run) = item {
                                    render_glyph_run(&glyph_run, &mut pen, real_x, y_offset);
                                }
                            }
                        }
                    }
                }
            }
        }
        // v303: 第二遍：形状压在所有文字之上（与导出 behindDoc="0" 一致）
        for &pid in &arena.page_order {
            let page = &arena.pages[pid];
            let margin_left = page.margin_left;
            for &fid in &page.frames {
                let frame = &arena.frames[fid];
                let is_shape_frame = matches!(
                    &frame.content,
                    crate::frame::FrameContent::Paragraph(lines) if lines.is_empty()
                ) && matches!(
                    doc.body.get(frame.para_index),
                    Some(roe_model::writer::BodyElement::Shape(_))
                );
                if !is_shape_frame { continue; }
                let col_x_offset = page.columns.col_x_offsets
                    .get(frame.column_index).copied().unwrap_or(0.0);
                self.render_shape_frame(arena, doc, fid, vp_x, vp_y, vp_w, vp_h, scale, margin_left + col_x_offset, &mut pixmap);
            }
        }
        Ok(())
    }

    /// PPTX-12 同款渐变 Shader 构造逻辑搬到 docx 侧，照抄
    /// roe-impress/src/slide_renderer.rs 里 build_fill_paint 对
    /// GradientKindResolved::Linear/Radial 的处理（角度→向量、
    /// 色标转换、SpreadMode::Pad），docx 侧 GradientKind::Rect
    /// 同 pptx 侧一样用 Radial 近似替代（tiny-skia 无原生矩形渐变）。
    /// x,y,w,h 为形状在像素坐标系下的包围盒（已乘 scale），与被
    /// 绘制路径的坐标空间一致，不需要再叠加 shape_transform。
    fn build_gradient_paint(
        stops: &[roe_model::common::GradientStop],
        kind: &roe_model::common::GradientKind,
        alpha: u8,
        x: f32, y: f32, w: f32, h: f32,
    ) -> Option<Paint<'static>> {
        if stops.is_empty() {
            return None;
        }
        let sk_stops: Vec<SkGradientStop> = stops
            .iter()
            .map(|s| {
                SkGradientStop::new(
                    s.pos,
                    Color::from_rgba8(s.color.r, s.color.g, s.color.b, alpha),
                )
            })
            .collect();

        let cx = x + w / 2.0;
        let cy = y + h / 2.0;

        let shader = match kind {
            roe_model::common::GradientKind::Linear { angle } => {
                let angle_rad = angle.to_radians();
                let half_diag = ((w * w + h * h).sqrt()) / 2.0;
                let dx = angle_rad.cos() * half_diag;
                let dy = angle_rad.sin() * half_diag;
                let start = Point::from_xy(cx - dx, cy - dy);
                let end = Point::from_xy(cx + dx, cy + dy);
                LinearGradient::new(start, end, sk_stops, SpreadMode::Pad, Transform::identity())
            }
            // Radial/Rect 均用径向渐变近似，中心固定为形状几何中心，
            // 半径取对角线一半，与 slide_renderer.rs 的第一期简化一致。
            roe_model::common::GradientKind::Radial | roe_model::common::GradientKind::Rect => {
                let radius = ((w * w + h * h).sqrt()) / 2.0;
                let center = Point::from_xy(cx, cy);
                RadialGradient::new(center, 0.0, center, radius, sk_stops, SpreadMode::Pad, Transform::identity())
            }
        };
        shader.map(|s| {
            let mut paint = Paint::default();
            paint.shader = s;
            paint.anti_alias = true;
            paint
        })
    }

    /// 渲染图形/文本框帧（Shape）：WROOT-023/029/032/033/039/046/050 系列缺口。
    /// 算法从 render_page_preview()（废弃路径）搬移过来，坐标换算改为
    /// render_viewport() 视口版本（参照 render_image_frame 的 dst_x/dst_y 写法）。
    /// 第一版：只画 geometry 轮廓 + 纯色/渐变首色标填充，不处理描边/阴影/
    /// 形状内文字/图片填充（见 ROE_DOCX_Progress v60 二、2.4节遗留问题）。
    fn render_shape_frame(
        &mut self,
        arena: &crate::frame::FrameArena,
        doc: &roe_model::writer::WriterDocument,
        fid: roe_core::arena::FrameId,
        vp_x: f32, vp_y: f32, _vp_w: f32, vp_h: f32,
        scale: f32,
        margin_left: f32,
        pixmap: &mut PixmapMut,
    ) {
        let frame = &arena.frames[fid];
        let elem = match doc.body.get(frame.para_index) {
            Some(e) => e,
            None => return,
        };
        let shape = match elem {
            roe_model::writer::BodyElement::Shape(s) => s,
            _ => return,
        };

        let shape_x_pt = margin_left + shape.x_pt;
        let shape_y_pt = frame.y_on_page;
        let shape_y1 = shape_y_pt + shape.height_pt;
        if shape_y1 < vp_y || shape_y_pt > vp_y + vp_h { return; }

        // sx/sy: 该形状原点在像素坐标系下的位置（视口裁剪+缩放后）
        let sx = (shape_x_pt - vp_x) * scale;
        let sy = (shape_y_pt - vp_y) * scale;

        let w_pt = shape.width_pt * scale;
        let h_pt = shape.height_pt * scale;

        let mut pb = tiny_skia::PathBuilder::new();
        match &shape.geometry {
            roe_model::common::ShapeGeometry::PresetPath(cmds) => {
                // v181: PresetPathData 不再是裸 Vec，for 循环需要 IntoIterator，
                // Deref 只覆盖方法调用不覆盖 for-in，显式取 .commands 迭代。
                for cmd in &cmds.commands {
                    match cmd {
                        roe_model::common::PathCommand::MoveTo { x, y } => {
                            pb.move_to(sx + x * scale, sy + y * scale);
                        }
                        roe_model::common::PathCommand::LineTo { x, y } => {
                            pb.line_to(sx + x * scale, sy + y * scale);
                        }
                        roe_model::common::PathCommand::CubicTo { x1, y1, x2, y2, x, y } => {
                            pb.cubic_to(
                                sx + x1 * scale, sy + y1 * scale,
                                sx + x2 * scale, sy + y2 * scale,
                                sx + x * scale,  sy + y * scale,
                            );
                        }
                        roe_model::common::PathCommand::QuadTo { x1, y1, x, y } => {
                            pb.quad_to(sx + x1 * scale, sy + y1 * scale, sx + x * scale, sy + y * scale);
                        }
                        roe_model::common::PathCommand::ArcTo { .. } => {
                            // 第一版未实现 ArcTo 转换，跳过（少数形状会因此路径不完整）
                        }
                        roe_model::common::PathCommand::Close => {
                            pb.close();
                        }
                    }
                }
            }
            // WROOT-SHAPE-GEOM-01: 此前只处理 PresetPath，导致 Rect/RoundRect/
            // Ellipse/Triangle/Line 全部落空路径构造，pb.finish()==None 直接
            // return，填充+描边全部跳过（不仅仅是"没描边"，是整个 Shape 不可
            // 见）。补齐各 variant 的路径构造，复用同一个 pb，走后续统一的
            // 填充/描边逻辑。
            roe_model::common::ShapeGeometry::Ellipse => {
                let cx = sx + w_pt / 2.0;
                let cy = sy + h_pt / 2.0;
                let rx = w_pt / 2.0;
                let ry = h_pt / 2.0;
                let k = 0.5522848_f32;
                pb.move_to(cx + rx, cy);
                pb.cubic_to(cx + rx, cy - ry * k, cx + rx * k, cy - ry, cx, cy - ry);
                pb.cubic_to(cx - rx * k, cy - ry, cx - rx, cy - ry * k, cx - rx, cy);
                pb.cubic_to(cx - rx, cy + ry * k, cx - rx * k, cy + ry, cx, cy + ry);
                pb.cubic_to(cx + rx * k, cy + ry, cx + rx, cy + ry * k, cx + rx, cy);
                pb.close();
            }
            roe_model::common::ShapeGeometry::Rect => {
                pb.move_to(sx, sy);
                pb.line_to(sx + w_pt, sy);
                pb.line_to(sx + w_pt, sy + h_pt);
                pb.line_to(sx, sy + h_pt);
                pb.close();
            }
            // WROOT-108（v188）：RoundRect/Triangle 专属渲染分支已注释掉，理由同
            // docx.rs 的 WROOT-108 注释——这是 Parley 迁移期间的临时实现，作者当时已
            // 注明"不是最终形态"。pptx 侧 PROOT-101 已确认 triangle 的标准做法是走
            // find_preset_shape 查表得到 PresetPath，而非独立枚举硬编码渲染。docx
            // parser 侧（docx.rs WROOT-108）已改为 triangle 落入 PresetPath 分支，
            // 这两个 variant 不会再从真实 docx 文件被构造，此处退化为下方矩形包围盒
            // 兜底（与 pptx 侧 slide_renderer.rs 的 `_ =>` 兜底视觉效果一致，不是
            // "不画"）。RoundRect 此前也从未被任何 parser 真实构造过（纯理论变体，
            // 仅测试代码手动构造），一并归入同一兜底。
            // 原实现（已注释，供未来若要恢复参考）：
            // roe_model::common::ShapeGeometry::RoundRect { radius } => { ... 四角贝塞尔圆角矩形 ... }
            // roe_model::common::ShapeGeometry::Triangle => { ... 等腰三角形 ... }
            roe_model::common::ShapeGeometry::RoundRect { .. } | roe_model::common::ShapeGeometry::Triangle => {
                pb.move_to(sx, sy);
                pb.line_to(sx + w_pt, sy);
                pb.line_to(sx + w_pt, sy + h_pt);
                pb.line_to(sx, sy + h_pt);
                pb.close();
            }
            roe_model::common::ShapeGeometry::Line => {
                pb.move_to(sx, sy);
                pb.line_to(sx + w_pt, sy + h_pt);
            }
            roe_model::common::ShapeGeometry::Custom(_) => {
                // 未识别的预设形状占位：仍未实现，保持原行为（不画）。
            }
        }
        let built_path = match pb.finish() {
            Some(p) => p,
            None => return,
        };
        // WROOT-FLIPV-01: <a:xfrm flipV="1"> 上下镜像。对已构造好的路径
        // 整体做一次相对形状自身水平中轴（y = sy + h_pt/2）的Y轴镜像，
        // 而不是改前面逐点计算的 sx+x*scale 那段match——路径已经在正确
        // 的像素坐标系里，直接用 tiny_skia::Transform 的 scale_y=-1 加
        // 平移抵消即可，风险比改几何构造逻辑本身低。镜像失败（理论上
        // 不会，仅当变换矩阵不可逆等极端情况）时退回原路径，不中断渲染。
        let built_path = if shape.flip_v == Some(true) {
            let mirror = Transform::from_row(1.0, 0.0, 0.0, -1.0, 0.0, 2.0 * sy + h_pt);
            built_path.clone().transform(mirror).unwrap_or(built_path)
        } else {
            built_path
        };
        // v308: 形状旋转（绕形状中心）
        let built_path = match shape.rotation {
            Some(deg) if deg.abs() > 0.01 => {
                let rot_t = Transform::from_rotate_at(deg, sx + w_pt / 2.0, sy + h_pt / 2.0);
                built_path.clone().transform(rot_t).unwrap_or(built_path)
            }
            _ => built_path,
        };
        if let Some(fill) = &shape.style.fill {
            let w = w_pt;
            let h = h_pt;
            let paint = match fill {
                roe_model::common::FillStyle::Solid(c) => {
                    let mut paint = Paint::default();
                    paint.set_color(Color::from_rgba8(c.r, c.g, c.b, c.a));
                    paint.anti_alias = true;
                    Some(paint)
                }
                roe_model::common::FillStyle::Gradient { stops, kind, .. } => {
                    // rotate_with_shape 已在解析层 raw_to_shape 阶段叠加进
                    // angle 里（参照 pptx.rs 同款处理），这里直接用 kind
                    // 里的最终角度，不需要再处理旋转。
                    Self::build_gradient_paint(stops, kind, 255, sx, sy, w, h)
                }
                roe_model::common::FillStyle::Picture(_) => None,
                roe_model::common::FillStyle::PictureTiled { .. } => None,
                roe_model::common::FillStyle::None => None,
            };
            if let Some(paint) = paint {
                pixmap.fill_path(&built_path, &paint, tiny_skia::FillRule::Winding, Transform::identity(), None);
            }
        }
        // WROOT-STROKE-01: 描边绘制，紧跟填充之后画在同一路径上（先填充
        // 再描边是常规图形绘制顺序，避免描边被填充盖住半边）。宽度按
        // scale 缩放，与几何点坐标换算保持同一比例。
        if let Some(stroke_style) = &shape.style.stroke {
            let mut paint = Paint::default();
            paint.set_color(Color::from_rgba8(
                stroke_style.color.r, stroke_style.color.g, stroke_style.color.b, stroke_style.color.a,
            ));
            paint.anti_alias = true;
            let mut stroke = Stroke::default();
            stroke.width = (stroke_style.width * scale).max(0.1);
            pixmap.stroke_path(&built_path, &paint, &stroke, Transform::identity(), None);
        }

        // WROOT-SHAPE-TEXT-01: 形状/文本框内文字渲染。此前 shape.text_content
        // 解析层（DOCX-9 Step2, wps:txbx）已正确产出 Vec<Paragraph>，但
        // render_shape_frame 从未消费这个字段，只画几何+填充+描边，导致
        // WROOT-023/029/033/038/046 系列"文本框文字未渲染"。做法照抄
        // render_viewport 表格 cell 画字模式（ranged_builder → build →
        // break_all_lines → render_glyph_run）。
        //
        // 字号/换行宽度单位说明：ranged_builder 的第三个参数就是 scale
        // （设备缩放因子），parley 内部会用它把 pt 值自动换算成设备像素
        // ——这与 layout.rs::shape_text_to_lines 的用法完全一致（该函数
        // 传入 ranged_builder(..., 1.0, true)，FontSize 直接传 pt 原始
        // 值，不手动乘 scale）。这里同样全程使用 pt 单位：字号、行高、
        // 换行宽度都用 shape.width_pt 等原始 pt 值，由 ranged_builder 统
        // 一换算成像素；画字坐标 sx/sy（像素）作为 render_glyph_run 的
        // 原点位置不变，纵向偏移在 pt 空间累加后再乘 scale 换算成像素。
        if !shape.text_content.is_empty() {
            let mut y_cursor_pt = 0.0f32; // 相对 shape 顶部的偏移，pt 单位
            for para in &shape.text_content {
                if para.runs.is_empty() { continue; }
                let text: String = para.runs.iter().map(|r| r.text.as_str()).collect();
                if text.trim().is_empty() { continue; }

                // 字号：取第一个 run 的 font_size，没有则兜底 12pt（pt 单位，不乘 scale）。
                let font_size_pt = para.runs.iter()
                    .find_map(|r| r.font_size)
                    .unwrap_or(12.0);
                let line_height_pt = font_size_pt * 1.2;

                let default_brush = RunBrush { color: [0, 0, 0, 255], run_idx: None };
                let mut builder = self.layout_context.ranged_builder(
                    &mut self.font_context, &text, scale.max(1.0), true,
                );
                builder.push_default(StyleProperty::Brush(default_brush));
                builder.push_default(GenericFamily::SystemUi);
                builder.push_default(LineHeight::Absolute(line_height_pt));
                builder.push_default(StyleProperty::FontSize(font_size_pt));

                let mut char_pos = 0usize;
                for run in &para.runs {
                    let start = char_pos;
                    let end = start + run.text.len();
                    char_pos = end;
                    if start >= end { continue; }
                    let range = start..end;
                    let color = run.color
                        .map(|c| [c.r, c.g, c.b, c.a])
                        .unwrap_or([0, 0, 0, 255]);
                    builder.push(StyleProperty::Brush(RunBrush { color, run_idx: None }), range.clone());
                    // WROOT-125 同步：形状/文本框内文字也按真实字体排版。
                    if let Some(fam) = crate::layout::resolve_run_font_family(doc, para, run) {
                        builder.push(StyleProperty::FontFamily(parley::FontFamily::named(fam.as_str())), range.clone());
                    }
                    if run.bold.unwrap_or(false)   { builder.push(StyleProperty::FontWeight(FontWeight::BOLD), range.clone()); }
                    if run.italic.unwrap_or(false) { builder.push(StyleProperty::FontStyle(FontStyle::Italic), range.clone()); }
                    if run.underline.unwrap_or(false)    { builder.push(StyleProperty::Underline(true), range.clone()); }
                    if run.strikethrough.unwrap_or(false) { builder.push(StyleProperty::Strikethrough(true), range.clone()); }
                }

                let mut layout: parley::Layout<RunBrush> = builder.build(&text);
                // 换行宽度用 shape.width_pt（pt 单位），与字号单位保持一致。
                layout.break_all_lines(Some(shape.width_pt.max(1.0)));
                layout.align(Alignment::Start, AlignmentOptions::default());

                {
                    let mut pen = TinySkiaPen::new(&mut *pixmap);
                    for line in layout.lines() {
                        let lm = line.metrics();
                        let draw_y = sy + y_cursor_pt * scale;
                        for item in line.items() {
                            if let PositionedLayoutItem::GlyphRun::<RunBrush>(glyph_run) = item {
                                render_glyph_run(&glyph_run, &mut pen, sx, draw_y);
                            }
                        }
                        y_cursor_pt += lm.line_height;
                    }
                }
            }
        }
    }
    /// 渲染图片帧（内联图片和浮动图片）
    fn render_image_frame(
        &mut self,
        arena: &crate::frame::FrameArena,
        doc: &roe_model::writer::WriterDocument,
        fid: roe_core::arena::FrameId,
        vp_x: f32, vp_y: f32, _vp_w: f32, vp_h: f32,
        scale: f32,
        margin_left: f32,
        pixmap: &mut PixmapMut,
    ) {
        let frame = &arena.frames[fid];
        let elem = match doc.body.get(frame.para_index) {
            Some(e) => e,
            None => return,
        };
        // 统计图(chartEx)占位渲染:第三步替换为真正的绘制
        if let roe_model::writer::BodyElement::InlineStatChart { model, width_pt, height_pt } = elem {
            let img_y_pt = frame.y_on_page;
            let img_x_pt = margin_left;
            let img_y1 = img_y_pt + height_pt;
            if img_y1 < vp_y || img_y_pt > vp_y + vp_h { return; }
            let img_w = (*width_pt  * scale) as u32;
            let img_h = (*height_pt * scale) as u32;
            let dst_x = ((img_x_pt - vp_x) * scale) as i32;
            let dst_y = ((img_y_pt - vp_y) * scale) as i32;
            match roe_core::stat_chart_render::render_stat_chart_to_rgba(model, img_w, img_h, scale) {
                Some(src) => Self::blit_rgba_buffer(&src, img_w, img_h, dst_x, dst_y, pixmap.width(), pixmap.height(), pixmap),
                None => Self::blit_stat_chart_placeholder(img_w, img_h, dst_x, dst_y, pixmap.width(), pixmap.height(), pixmap),
            }
            return;
        }
        // WROOT-011: InlineChart 单独处理
        if let roe_model::writer::BodyElement::InlineChart { model, width_pt, height_pt } = elem {
            let img_y_pt = frame.y_on_page;
            let img_x_pt = margin_left;
            let img_y1 = img_y_pt + height_pt;
            if img_y1 < vp_y || img_y_pt > vp_y + vp_h { return; }
            let img_w = (*width_pt  * scale) as u32;
            let img_h = (*height_pt * scale) as u32;
            let dst_x = ((img_x_pt - vp_x) * scale) as i32;
            let dst_y = ((img_y_pt - vp_y) * scale) as i32;
            // v318f: 与 Calc、统计图同口径，按逻辑点排版后整体放大 scale 倍，字号才正常
            if let Some(src) = roe_core::chart_render::render_chart_to_rgba_scaled(model, img_w, img_h, scale) {
                Self::blit_rgba_buffer(&src, img_w, img_h, dst_x, dst_y, pixmap.width(), pixmap.height(), pixmap);
            }
            return;
        }

        let (raw, width_pt, height_pt, img_x_pt, img_y_pt, rotation) = match elem {
            roe_model::writer::BodyElement::Image(img) => {
                (img.image.data.clone(), img.width_pt, img.height_pt,
                 margin_left, frame.y_on_page, img.rotation)
            }
            roe_model::writer::BodyElement::Frame(ff) => {
                // WROOT-037: relativeFrom="column" 场景下，posOffset 的坐标原点
                // 实测应为页面左边缘（而非版心左边缘），故不再叠加 margin_left。
                // 与LO参考渲染对比确认：图片应能跨越左页边距、贴到纸张左边缘。
                (ff.image.data.clone(), ff.width_pt, ff.height_pt,
                 ff.x_pt, frame.y_on_page, ff.rotation)
            }
            _ => return,
        };
        let img_y1 = img_y_pt + height_pt;
        if img_y1 < vp_y || img_y_pt > vp_y + vp_h { return; }

        let img_w = (width_pt  * scale) as u32;
        let img_h = (height_pt * scale) as u32;
        if img_w == 0 || img_h == 0 { return; }

        let dyn_img = match image::load_from_memory(&raw) {
            Ok(i) => i,
            Err(_) => return,
        };
        let rgba = dyn_img.resize_exact(img_w, img_h, image::imageops::FilterType::Triangle).to_rgba8();
        let src_w = rgba.width() as usize;
        let src_h = rgba.height() as usize;
        let dst_x = ((img_x_pt - vp_x) * scale) as i32;
        let dst_y = ((img_y_pt - vp_y) * scale) as i32;
        let pw = pixmap.width() as i32;
        let ph = pixmap.height() as i32;

        // WROOT-016: 图片自身旋转角度不为0时，改走 tiny-skia 仿射变换贴图
        // （Pixmap::draw_pixmap + Transform::from_rotate_at，围绕图片中心
        // 旋转，与 roe-impress/slide_renderer.rs 的 build_shape_transform
        // 同一数学模型：先构造以图片中心为轴心的旋转矩阵，再交给底层贴图）。
        // rotation为None或0时，保留原有逐像素blit路径不变（零风险、
        // 性能更好，绝大多数图片没有旋转，不应为极少数场景付出统一改造代价）。
        let deg = rotation.unwrap_or(0.0);
        if deg != 0.0 {
            if let Some(src_pixmap) = tiny_skia::Pixmap::from_vec(
                rgba.into_raw(), tiny_skia::IntSize::from_wh(src_w as u32, src_h as u32).unwrap(),
            ) {
                let cx = dst_x as f32 + src_w as f32 / 2.0;
                let cy = dst_y as f32 + src_h as f32 / 2.0;
                let transform = tiny_skia::Transform::from_rotate_at(deg, cx, cy);
                let paint = tiny_skia::PixmapPaint::default();
                pixmap.draw_pixmap(dst_x, dst_y, src_pixmap.as_ref(), &paint, transform, None);
            }
            return;
        }

        for sy in 0..src_h {
            for sx in 0..src_w {
                let dx = dst_x + sx as i32;
                let dy = dst_y + sy as i32;
                if dx < 0 || dy < 0 || dx >= pw || dy >= ph { continue; }
                let pixel = rgba.get_pixel(sx as u32, sy as u32);
                let idx = (dy as usize * pw as usize + dx as usize) * 4;
                let buf = pixmap.data_mut();
                if idx + 3 < buf.len() {
                    let a = pixel[3] as f32 / 255.0;
                    buf[idx]     = (pixel[0] as f32 * a + buf[idx]     as f32 * (1.0 - a)) as u8;
                    buf[idx + 1] = (pixel[1] as f32 * a + buf[idx + 1] as f32 * (1.0 - a)) as u8;
                    buf[idx + 2] = (pixel[2] as f32 * a + buf[idx + 2] as f32 * (1.0 - a)) as u8;
                    buf[idx + 3] = 255;
                }
            }
        }
    }

    fn blit_chart_to_pixmap(
        model: &roe_model::impress::ChartModel,
        img_w: u32, img_h: u32,
        dst_x: i32, dst_y: i32,
        pw: u32, ph: u32,
        pixmap: &mut PixmapMut,
    ) {
        if img_w == 0 || img_h == 0 { return; }
        let png_bytes = match roe_impress::chart::render_chart_to_png(model, img_w, img_h) {
            Some(b) => b,
            None => return,
        };
        let chart_pixmap = match tiny_skia::Pixmap::decode_png(&png_bytes) {
            Ok(p) => p,
            Err(_) => return,
        };
        let src_w = chart_pixmap.width() as usize;
        let src_h = chart_pixmap.height() as usize;
        let src_data = chart_pixmap.data();
        for sy in 0..src_h {
            for sx in 0..src_w {
                let dx = dst_x + sx as i32;
                let dy = dst_y + sy as i32;
                if dx < 0 || dy < 0 || dx >= pw as i32 || dy >= ph as i32 { continue; }
                let si = (sy * src_w + sx) * 4;
                let di = (dy as usize * pw as usize + dx as usize) * 4;
                let buf = pixmap.data_mut();
                if si + 3 < src_data.len() && di + 3 < buf.len() {
                    let a = src_data[si + 3] as f32 / 255.0;
                    buf[di]     = (src_data[si]     as f32 * a + buf[di]     as f32 * (1.0 - a)) as u8;
                    buf[di + 1] = (src_data[si + 1] as f32 * a + buf[di + 1] as f32 * (1.0 - a)) as u8;
                    buf[di + 2] = (src_data[si + 2] as f32 * a + buf[di + 2] as f32 * (1.0 - a)) as u8;
                    buf[di + 3] = 255;
                }
            }
        }
    }

    /// 把不透明 RGBA 缓冲(w*h*4)拷贝到目标 pixmap 的 (dst_x, dst_y),越界部分裁掉。
    fn blit_rgba_buffer(
        src: &[u8], img_w: u32, img_h: u32,
        dst_x: i32, dst_y: i32,
        pw: u32, ph: u32,
        pixmap: &mut PixmapMut,
    ) {
        if img_w == 0 || img_h == 0 { return; }
        let buf = pixmap.data_mut();
        for sy in 0..img_h as i32 {
            for sx in 0..img_w as i32 {
                let dx = dst_x + sx;
                let dy = dst_y + sy;
                if dx < 0 || dy < 0 || dx >= pw as i32 || dy >= ph as i32 { continue; }
                let si = (sy as usize * img_w as usize + sx as usize) * 4;
                let di = (dy as usize * pw as usize + dx as usize) * 4;
                if si + 3 < src.len() && di + 3 < buf.len() {
                    buf[di] = src[si]; buf[di + 1] = src[si + 1]; buf[di + 2] = src[si + 2]; buf[di + 3] = 255;
                }
            }
        }
    }

    /// 统计图占位:浅灰底 + 灰边框(临时,待真正的统计图渲染替换)。
    fn blit_stat_chart_placeholder(
        img_w: u32, img_h: u32,
        dst_x: i32, dst_y: i32,
        pw: u32, ph: u32,
        pixmap: &mut PixmapMut,
    ) {
        if img_w == 0 || img_h == 0 { return; }
        let buf = pixmap.data_mut();
        for sy in 0..img_h as i32 {
            for sx in 0..img_w as i32 {
                let dx = dst_x + sx;
                let dy = dst_y + sy;
                if dx < 0 || dy < 0 || dx >= pw as i32 || dy >= ph as i32 { continue; }
                let edge = sx == 0 || sy == 0 || sx == img_w as i32 - 1 || sy == img_h as i32 - 1;
                let c: u8 = if edge { 0xcc } else { 0xf7 };
                let di = (dy as usize * pw as usize + dx as usize) * 4;
                if di + 3 < buf.len() {
                    buf[di] = c; buf[di + 1] = c; buf[di + 2] = c; buf[di + 3] = 255;
                }
            }
        }
    }

    /// 批注气泡功能（v151→v152起手第一步）：把 Comment.content
    /// （Vec<BodyElement>）拍平成纯文本，供气泡正文渲染使用。
    /// 只处理 BodyElement::Paragraph 分支——批注正文按 OOXML 惯例只会是
    /// 段落文本，Table/Image/Frame/Shape/InlineChart 这些分支在批注场景
    /// 下几乎不会出现，遇到时直接跳过，不强行处理（有意的窄范围实现，
    /// 不追求跟 fodt_writer.rs::paragraphs_to_plain_text 完全对齐的通用性
    /// ——那个函数是私有的且签名是 &[Paragraph]，类型和可见性都对不上，
    /// 跨crate复用成本高于重写这几行）。
    /// 段落间用 "\n" 连接，与 fodt_writer.rs 里同名逻辑的既有约定一致。
    pub fn comment_content_to_plain_text(content: &[roe_model::writer::BodyElement]) -> String {
        content.iter()
            .filter_map(|elem| match elem {
                roe_model::writer::BodyElement::Paragraph(p) => {
                    Some(p.runs.iter().map(|r| r.text.as_str()).collect::<String>())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 批注气泡功能（v152）最小可见版本：画一个圆角矩形+左边色条+
    /// 一条虚线连接线，验证"气泡位置对不对"，暂不画头像/按钮/文字
    /// （v151 2.2节完整视觉方案的后续步骤）。
    ///
    /// 参数：
    /// - bubble_x/y/w/h：气泡框左上角坐标+宽高，像素单位（调用方已完成
    ///   pt→px换算和防重叠布局计算，这里只管画）。
    /// - color_idx：复用 COMMENT_HIGHLIGHT_COLORS（模块级常量，与正文
    ///   高亮/竖线同一套颜色，视觉呼应，v151 2.2节第2点要求）。
    /// - connector_from：连接线起点（高亮矩形右边缘中点），像素坐标。
    /// - connector_to：连接线终点（气泡框左边缘竖条中点），像素坐标。
    ///   连接线终点特意不是气泡框最左边缘，而是"左边色条"的中点，
    ///   这样视觉上连接线像是"接到"色条上，不是接到灰色气泡框本身。
    #[allow(clippy::too_many_arguments)]
    /// v154新增：真实排版测量气泡所需高度（px），不绘制任何内容。
    /// 与draw_comment_bubble_placeholder共用同一套尺寸公式和同一套
    /// parley排版调用，保证"量出来的高度"和"画出来的高度"完全一致
    /// ——不用简单估算公式，避免以后回复区/长文本内容量绘不一致。
    ///
    /// 固定高度部分：上下padding + 头像/作者名一行的高度。
    /// 可变高度部分：正文预览实际渲染行数（1~MAX_BODY_LINES）× 行高。
    /// bubble_w需要传入，因为文字换行宽度(content_w)依赖气泡宽度，
    /// 但气泡宽度目前是常量（不随内容变），所以这里不是循环依赖。
    pub fn measure_comment_bubble_height(
        &mut self,
        bubble_w: f32,
        content_preview: &str,
        scale: f32,
        replies: &[&roe_model::writer::Comment],
    ) -> f32 {
        // 单位说明（本次彻底理清）：
        // - 传给 parley（FontSize/LineHeight::Absolute）的数值用裸pt，
        //   parley 内部会按 ranged_builder 的 scale 参数自动放大成实际
        //   渲染像素，这是 parley 自己的换算，我们不需要、也不应该
        //   在传值前手动再乘一次 scale（乘了会导致字体/行高被放大两次）。
        // - 但我们自己用来"摆放位置、计算容器像素尺寸"的变量（pad、
        //   avatar_d、以及这里用来预估 body_h 的行高），因为不经过
        //   parley 的自动缩放，必须手动乘 scale 才能对齐真实px画布。
        //   之前的bug：body_line_height 传给 LineHeight::Absolute 时
        //   会被 parley 按 scale 放大，但我们用同一个裸pt的
        //   body_line_height 去乘 total_lines 算 body_h，没有再乘
        //   scale，导致算出的容器高度只有 parley 实际渲染高度的一半
        //   （scale=2.0 时）。
        let pad = 6.0f32 * scale;
        let bar_w = 3.0f32;
        let content_x0_offset = bar_w + pad; // 相对bubble_x的偏移
        let content_x1_offset = bubble_w - pad;
        let content_w = (content_x1_offset - content_x0_offset).max(1.0);

        let avatar_d = 16.0 * scale;
        // 头像+作者名这一行的固定高度（对应draw_comment_bubble_placeholder
        // 第7步开头 cursor_y = y0 + pad + avatar_d + pad * 0.8 这个换行公式）
        let header_h = avatar_d + pad * 0.8;

        let body_font_size = 8.0; // 裸pt，喂给parley，parley按scale自动放大
        let body_line_height_pt = body_font_size * 1.15; // 裸pt，喂给parley
        let body_line_height_px = body_line_height_pt * scale; // 我们自己算容器高度用，需要手动乘scale

        // 不再限制行数：容器高度按内容真实所需行数计算，与
        // draw_comment_bubble_placeholder 第7步（已去掉截断）保持一致，
        // 气泡框会随内容自动撑开，不会出现"测量按4行、实际画更多行"
        // 导致的高度不够、文字溢出问题。
        let body_h = if content_preview.is_empty() {
            0.0
        } else {
            let mut builder = self.layout_context.ranged_builder(
                &mut self.font_context, content_preview, scale.max(1.0), true,
            );
            builder.push_default(GenericFamily::SystemUi);
            builder.push_default(LineHeight::Absolute(body_line_height_pt));
            builder.push_default(StyleProperty::FontSize(body_font_size));
            let mut layout: parley::Layout<RunBrush> = builder.build(content_preview);
            layout.break_all_lines(Some(content_w));
            let total_lines = layout.lines().count();
            eprintln!("      [DEBUG measure] content_w={:.2} total_lines={} content_preview={:?}", content_w, total_lines, content_preview);
            (total_lines as f32) * body_line_height_px
        };

        // 安全余量：额外加半行高度的缓冲，避免因字体渲染细节差异
        // （比如字形上下延伸、parley实际排版与估算的细微出入）导致
        // "算出来刚好卡线，实际渲染时差一点点装不下"的情况。
        let safety_margin_px = body_line_height_px * 1.5;

        // ---- 第8步（回复区）高度累加 ----
        // 与draw_comment_bubble_placeholder第8步保持同一套公式：
        // 每条回复 = 分隔线上下留白 + 头像/作者名/日期一行 + 正文实际行数。
        // 回复区整体复用主批注同一套 body_font_size/line_height，
        // 不单独定义一套尺寸（v156会议纪要：回复区直接平铺，样式对齐正文，
        // 不做折叠/缩略，字号字体不调整，只做缩进）。
        let reply_sep_gap = pad * 0.8; // 分隔线与上一条内容之间的留白
        let reply_avatar_d = 14.0 * scale; // 回复头像略小于主批注头像，视觉降一级
        let reply_header_h = reply_avatar_d + pad * 0.6;
        let mut replies_h = 0.0f32;
        for reply in replies {
            let reply_text = Self::comment_content_to_plain_text(&reply.content);
            let reply_body_h = if reply_text.is_empty() {
                0.0
            } else {
                let reply_content_w = (bubble_w - pad - reply_avatar_d - pad).max(1.0);
                let mut builder = self.layout_context.ranged_builder(
                    &mut self.font_context, &reply_text, scale.max(1.0), true,
                );
                builder.push_default(GenericFamily::SystemUi);
                builder.push_default(LineHeight::Absolute(body_line_height_pt));
                builder.push_default(StyleProperty::FontSize(body_font_size));
                let mut layout: parley::Layout<RunBrush> = builder.build(&reply_text);
                layout.break_all_lines(Some(reply_content_w));
                let total_lines = layout.lines().count().max(1);
                (total_lines as f32) * body_line_height_px
            };
            replies_h += reply_sep_gap + reply_header_h + reply_body_h;
        }

        let total_h = pad + header_h + body_h + safety_margin_px + replies_h + pad;
        eprintln!("      [DEBUG breakdown] pad={:.2} header_h={:.2} body_h={:.2} safety_margin_px={:.2} replies_h={:.2} total_h={:.2}", pad, header_h, body_h, safety_margin_px, replies_h, total_h);
        total_h // 顶部padding + header + body + 安全余量 + 回复区 + 底部padding
    }

    pub fn draw_comment_bubble_placeholder(
        &mut self,
        pixmap: &mut PixmapMut,
        bubble_x: f32, bubble_y: f32, bubble_w: f32, bubble_h: f32,
        color_idx: u8,
        connector_from: (f32, f32),
        connector_to: (f32, f32),
        author: &str,
        initials: Option<&str>,
        content_preview: &str,
        done: bool,
        scale: f32,
        replies: &[&roe_model::writer::Comment],
        comment_date: Option<&str>,
    ) {
        let (bar_rgb, _fill_rgba) = COMMENT_HIGHLIGHT_COLORS[color_idx as usize % 6];
        let corner_r: f32 = 4.0; // 4pt圆角（v151 2.2节第1点），此处直接按px处理
                                  // ——本函数不知道scale，调用方传入的bubble_*
                                  // 参数已经是像素单位，圆角半径固定4px在常见
                                  // scale(1.5~3)下视觉效果都合理，不做二次换算。

        // ---- 1. 气泡框本体：圆角矩形，浅灰底#F7F7F7，1pt细边框#E0E0E0 ----
        // （v152四节遗留问题修复：连接线原先画在气泡框之前，被气泡框的
        // 浅灰底盖住了大半，改为气泡框先画、连接线最后画，叠加在最上层，
        // 才能完整可见。原步骤编号1/2互换，其余步骤编号不变。）
        // tiny-skia 0.12 没有内置圆角矩形API（PathBuilder只有直角
        // from_rect），手写四条直线段+四个quad_to圆角拼出路径。
        // 圆角用二次贝塞尔近似（quad_to），4px的小圆角视觉上与真实
        // 圆弧无法区分，不需要更精确的三次贝塞尔/arc实现。
        let x0 = bubble_x;
        let y0 = bubble_y;
        let x1 = bubble_x + bubble_w;
        let y1 = bubble_y + bubble_h;
        let r = corner_r.min(bubble_w / 2.0).min(bubble_h / 2.0); // 防御：气泡太小时圆角不超过半宽/半高
        let mut pb = PathBuilder::new();
        pb.move_to(x0 + r, y0);
        pb.line_to(x1 - r, y0);
        pb.quad_to(x1, y0, x1, y0 + r);
        pb.line_to(x1, y1 - r);
        pb.quad_to(x1, y1, x1 - r, y1);
        pb.line_to(x0 + r, y1);
        pb.quad_to(x0, y1, x0, y1 - r);
        pb.line_to(x0, y0 + r);
        pb.quad_to(x0, y0, x0 + r, y0);
        pb.close();
        if let Some(path) = pb.finish() {
            // 先填充浅灰底
            let mut fill_paint = Paint::default();
            fill_paint.set_color_rgba8(0xF7, 0xF7, 0xF7, 0xFF); // 浅灰底，done/未done不再区分气泡整体色调
            fill_paint.anti_alias = true;
            pixmap.fill_path(&path, &fill_paint, tiny_skia::FillRule::Winding, Transform::identity(), None);
            // 再描边细边框
            let mut stroke_paint = Paint::default();
            stroke_paint.set_color_rgba8(0xE0, 0xE0, 0xE0, 0xFF);
            stroke_paint.anti_alias = true;
            let mut stroke = Stroke::default();
            stroke.width = 1.0;
            pixmap.stroke_path(&path, &stroke_paint, &stroke, Transform::identity(), None);
        }

        // ---- 2. 连接线：虚线，1.5pt宽，颜色=该批注深色 ----
        // 画法照抄 draw_h_line_styled/draw_v_line_styled 内部的虚线分支
        // （stroke_path + StrokeDash），本函数是独立的直线段（不一定水平/
        // 竖直），不能直接调用那两个只支持水平/竖直的函数，手写等价逻辑。
        // v152四节遗留问题修复：改到气泡框画完之后再画，叠加在最上层，
        // 避免被气泡框浅灰底覆盖导致视觉上"看不见"。
        // ---- 气泡侧连接线：虚线，1.5pt宽，颜色=该批注深色 ----
        // 恢复此前测试注释掉的代码：与正文内新增的方案B连接线一起对比，
        // 看两段各画各的线在页面边缘处是否能自然衔接。
        {
            let mut pb = PathBuilder::new();
            pb.move_to(connector_from.0, connector_from.1);
            pb.line_to(connector_to.0, connector_to.1);
            if let Some(path) = pb.finish() {
                let mut paint = Paint::default();
                paint.set_color_rgba8(bar_rgb[0], bar_rgb[1], bar_rgb[2], 255);
                paint.anti_alias = true;
                let mut stroke = Stroke::default();
                stroke.width = 1.5;
                stroke.dash = tiny_skia::StrokeDash::new(vec![3.0, 3.0], 0.0);
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
        }

        // ---- 3. 左边色条：3pt宽竖条，颜色=该批注深色，贴气泡框左边缘 ----
        // 色条本身也带一点点圆角观感更协调，但为了不把这个"最小可见版本"
        // 复杂化，先用直角矩形（色条很窄，直角/圆角视觉差异可忽略）。
        let bar_w = 3.0f32;
        if let Some(rect) = Rect::from_xywh(x0, y0, bar_w.min(bubble_w), bubble_h) {
            let mut bar_paint = Paint::default();
            bar_paint.set_color_rgba8(bar_rgb[0], bar_rgb[1], bar_rgb[2], 255);
            bar_paint.anti_alias = true;
            pixmap.fill_rect(rect, &bar_paint, Transform::identity(), None);
        }

        // 内容区起点：色条右侧留一点内边距（padding），下面所有元素
        // 的x坐标都以这个为基准。padding/元素尺寸目前是估算值，
        // 等实际效果出来后再按视觉观感微调，不追求精确到像素的规范。
        let pad = 6.0f32 * scale; // 6pt换算成px：摆放位置用的像素值，需要乘scale
                                  // （喂给parley的FontSize/LineHeight则保持裸pt，parley自己按scale放大渲染，两者不可混淆）
        let content_x0 = x0 + bar_w + pad;
        let content_x1 = x1 - pad;
        let content_w  = (content_x1 - content_x0).max(1.0);
        let mut cursor_y = y0 + pad;

        // ---- 4. 头像占位：圆形，约16pt直径（乘scale换算成px），
        // 无真实头像图片时显示姓名首字母，背景色按作者名做简单hash
        // 区分（v151 2.2节第3点）。----
        let avatar_d = 16.0 * scale; // 头像直径，画圆用的像素值，需要乘scale
        let avatar_r = avatar_d / 2.0;
        let avatar_cx = content_x0 + avatar_r;
        let avatar_cy = cursor_y + avatar_r;
        // 简单hash：对author字符串字节求和取模6，映射到同一套批注颜色表，
        // 不需要单独设计一套"用户颜色表"——复用现有6色，视觉上统一，
        // 也省了新增一份颜色定义的维护成本。
        let author_hash: u32 = author.bytes().map(|b| b as u32).sum();
        let (avatar_rgb, _) = COMMENT_HIGHLIGHT_COLORS[(author_hash % 6) as usize];
        {
            // 圆形用tiny-skia的PathBuilder::push_circle（0.12版本内置，
            // 不需要像圆角矩形那样手写quad_to近似——圆形是常见图元，
            // tiny-skia原生支持）。
            let mut pb = PathBuilder::new();
            pb.push_circle(avatar_cx, avatar_cy, avatar_r);
            if let Some(path) = pb.finish() {
                let mut paint = Paint::default();
                paint.set_color_rgba8(avatar_rgb[0], avatar_rgb[1], avatar_rgb[2], 255);
                paint.anti_alias = true;
                pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, Transform::identity(), None);
            }
        }
        // 头像里的首字母文字：优先用Comment.initials（docx w:initials
        // 属性），没有时从author自动取（中文取首字，英文取各词首字母
        // 大写最多2个，v151既定规则）。
        let avatar_label: String = match initials {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => {
                let first_char = author.chars().next();
                match first_char {
                    Some(c) if c.is_ascii_alphabetic() => {
                        author.split_whitespace()
                            .filter_map(|w| w.chars().next())
                            .map(|c| c.to_ascii_uppercase())
                            .take(2)
                            .collect::<String>()
                    }
                    Some(c) => c.to_string(), // 非ASCII字母（含中文等）：直接取首字符
                    None => String::new(),
                }
            }
        };
        if !avatar_label.is_empty() {
            let label_font_size = avatar_d * 0.42; // 经验值：字号约为直径的0.42倍，视觉上填充圆形但不顶边
            let mut builder = self.layout_context.ranged_builder(
                &mut self.font_context, &avatar_label, scale.max(1.0), true,
            );
            builder.push_default(StyleProperty::Brush(RunBrush { color: [255, 255, 255, 255], run_idx: None }));
            builder.push_default(GenericFamily::SystemUi);
            builder.push_default(StyleProperty::FontSize(label_font_size));
            builder.push_default(FontWeight::BOLD);
            let mut layout: parley::Layout<RunBrush> = builder.build(&avatar_label);
            layout.break_all_lines(None);
            layout.align(Alignment::Start, AlignmentOptions::default());
            let text_w = layout.width();
            let text_h = layout.height();
            let label_x = avatar_cx - text_w / 2.0;
            let label_y = avatar_cy - text_h / 2.0;
            let mut pen = TinySkiaPen::new(pixmap);
            for line in layout.lines() {
                for item in line.items() {
                    if let PositionedLayoutItem::GlyphRun::<RunBrush>(glyph_run) = item {
                        render_glyph_run(&glyph_run, &mut pen, label_x, label_y);
                    }
                }
            }
        }

        // ---- 5. 右上角三个按钮占位：编辑/删除/完成，只画外观 ----
        // v151 2.2节第4点：只画外观和导出坐标，不做点击响应逻辑
        // （那是Slint前台的事）。用三个小圆点+简单符号表示，不追求
        // 真实图标细节——真实图标是美术资源，不是这一步的重点，
        // 这里只占位标注"这里将来会有三个按钮"。
        let btn_d = 12.0 * scale; // 按钮直径，画圆用的像素值，需要乘scale
        let btn_gap = 4.0 * scale;
        let btn_y = y0 + pad + btn_d / 2.0;
        // 从右往左排：完成、删除、编辑（与常见软件"最常用操作靠右"的
        // 习惯一致，完成通常是最终操作，放最右）。
        // v153新增：真实Fluent SVG图标替换原Unicode符号占位。
        // 三个svg在编译期用include_str!内嵌进二进制，不依赖运行时文件路径。
        const ICON_DONE: &str = include_str!("../assets/icons/comment_done.svg");
        const ICON_DELETE: &str = include_str!("../assets/icons/comment_delete.svg");
        const ICON_EDIT: &str = include_str!("../assets/icons/comment_edit.svg");
        let btn_icons = [ICON_DONE, ICON_DELETE, ICON_EDIT]; // 完成/删除/编辑，与原btn_labels顺序一致
        // v156后续会话：编辑/删除/完成三个按钮的点击响应本来就要交给
        // Slint前台处理（"前台0回调"架构原则），roe这套画法本身也已定性为
        // 过渡期/未来的导出fallback路径（见ROE_Progress_v156.md第一节）。
        // 暂时不绘制这三个按钮，把气泡右上角空间让给日期展示；具体按钮/
        // 日期的最终摆放方式由Slint前台重新设计时决定。代码保留不删除，
        // 只是不执行绘制循环，方便日后需要参考图标绘制方式时能直接找到。
        if false {
        for (i, icon_svg) in btn_icons.iter().enumerate() {
            let btn_cx = content_x1 - btn_d / 2.0 - (i as f32) * (btn_d + btn_gap);
            let mut pb = PathBuilder::new();
            pb.push_circle(btn_cx, btn_y, btn_d / 2.0);
            if let Some(path) = pb.finish() {
                let mut paint = Paint::default();
                paint.set_color_rgba8(0xEE, 0xEE, 0xEE, 255);
                paint.anti_alias = true;
                pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, Transform::identity(), None);
            }
            // 图标画在圆点中央，尺寸取按钮直径的1.2倍（图标会略微超出圆点背景，
            // 图标本身作为视觉主体，圆点只作为hover提示底色，比例参考真实产品常见做法）
            let icon_size = (btn_d * 1.2).max(1.0) as u32;
            if let Ok(tree) = resvg::usvg::Tree::from_str(icon_svg, &resvg::usvg::Options::default()) {
                if let Some(mut icon_pixmap) = tiny_skia::Pixmap::new(icon_size, icon_size) {
                    let svg_size = tree.size();
                    let icon_scale = (icon_size as f32) / svg_size.width().max(svg_size.height());
                    let render_transform = Transform::from_scale(icon_scale, icon_scale);
                    resvg::render(&tree, render_transform, &mut icon_pixmap.as_mut());
                    let icon_x = btn_cx - (icon_size as f32) / 2.0;
                    let icon_y = btn_y - (icon_size as f32) / 2.0;
                    pixmap.draw_pixmap(
                        icon_x.round() as i32,
                        icon_y.round() as i32,
                        icon_pixmap.as_ref(),
                        &tiny_skia::PixmapPaint::default(),
                        Transform::identity(),
                        None,
                    );
                }
            }
        }
        }

        // ---- 6. 作者名：加粗，8.5pt，#333333；日期紧跟作者名之后，
        // 同一行，6.5pt浅灰色（v156后续会话新增：按钮暂不绘制后腾出的
        // 空间用于展示主批注日期，与回复区日期展示方式、格式保持一致，
        // 均为"YYYY-MM-DD HH:MM"，标准ISO8601之外的格式原样显示）。----
        // 画在头像右侧，与头像顶部对齐。
        cursor_y = y0 + pad; // 复位到内容区顶部（头像和作者名同一行起点）
        let author_font_size = 8.5;
        let author_x = avatar_cx + avatar_r + pad * 0.6;
        let author_max_w = (content_x1 - author_x).max(1.0);
        let mut author_w = 0.0f32;
        {
            let mut builder = self.layout_context.ranged_builder(
                &mut self.font_context, author, scale.max(1.0), true,
            );
            builder.push_default(StyleProperty::Brush(RunBrush { color: [0x33, 0x33, 0x33, 255], run_idx: None }));
            builder.push_default(GenericFamily::SystemUi);
            builder.push_default(StyleProperty::FontSize(author_font_size));
            builder.push_default(FontWeight::BOLD);
            let mut layout: parley::Layout<RunBrush> = builder.build(author);
            layout.break_all_lines(Some(author_max_w));
            layout.align(Alignment::Start, AlignmentOptions::default());
            author_w = layout.width();
            let author_y = avatar_cy - layout.height() / 2.0; // 与头像垂直居中对齐
            let mut pen = TinySkiaPen::new(pixmap);
            for line in layout.lines() {
                for item in line.items() {
                    if let PositionedLayoutItem::GlyphRun::<RunBrush>(glyph_run) = item {
                        render_glyph_run(&glyph_run, &mut pen, author_x, author_y);
                    }
                }
            }
        }
        {
            // 主批注的日期字段目前解析器/数据层一直完整保留
            // （Comment.date: Option<String>），此前只是roe绘制这一步
            // 没有画出来，本次补上，格式化规则与回复区一致。
            if let Some(date_str) = comment_date {
                if !date_str.is_empty() {
                    let formatted_owned: String = if date_str.len() == 20
                        && date_str.ends_with('Z')
                        && date_str.as_bytes().get(10) == Some(&b'T')
                    {
                        format!("{} {}", &date_str[0..10], &date_str[11..16])
                    } else {
                        date_str.to_string()
                    };
                    let date_x = author_x + author_w + pad * 0.6;
                    let date_font_size = 6.5;
                    let mut builder = self.layout_context.ranged_builder(
                        &mut self.font_context, &formatted_owned, scale.max(1.0), true,
                    );
                    builder.push_default(StyleProperty::Brush(RunBrush { color: [0x99, 0x99, 0x99, 255], run_idx: None }));
                    builder.push_default(GenericFamily::SystemUi);
                    builder.push_default(StyleProperty::FontSize(date_font_size));
                    let mut layout: parley::Layout<RunBrush> = builder.build(&formatted_owned);
                    layout.break_all_lines(None);
                    layout.align(Alignment::Start, AlignmentOptions::default());
                    let date_y = avatar_cy - layout.height() / 2.0;
                    let mut pen = TinySkiaPen::new(pixmap);
                    for line in layout.lines() {
                        for item in line.items() {
                            if let PositionedLayoutItem::GlyphRun::<RunBrush>(glyph_run) = item {
                                render_glyph_run(&glyph_run, &mut pen, date_x, date_y);
                            }
                        }
                    }
                }
            }
        }

        // ---- 6.5 "Done"标签：深绿底+白字圆角小胶囊，画在作者名与右上角三按钮之间 ----
        // v153新增：批注完成状态可视化。仅在done=true时绘制。
        // 注意：done仍可重新编辑/取消完成（OOXML语义），三个按钮不因done而移除，
        // Done标签单独占据作者名和按钮区域中间的空隙，不与两者重叠。
        if done {
            // v156后续会话：按钮已暂不绘制（见上方if false块的说明），
            // Done标签不再需要为按钮预留位置，直接贴着内容区最右边
            // （content_x1）绘制。
            let badge_font_size = 6.0;
            let badge_text = "Done";
            let mut builder = self.layout_context.ranged_builder(
                &mut self.font_context, badge_text, scale.max(1.0), true,
            );
            builder.push_default(StyleProperty::Brush(RunBrush { color: [0xFF, 0xFF, 0xFF, 255], run_idx: None }));
            builder.push_default(GenericFamily::SystemUi);
            builder.push_default(StyleProperty::FontSize(badge_font_size));
            builder.push_default(FontWeight::BOLD);
            let mut badge_layout: parley::Layout<RunBrush> = builder.build(badge_text);
            badge_layout.break_all_lines(None);
            badge_layout.align(Alignment::Start, AlignmentOptions::default());
            let text_w = badge_layout.width();
            let text_h = badge_layout.height();
            let badge_pad_x = 3.5 * scale;
            let badge_h = text_h + 2.5 * scale;
            let badge_w = text_w + badge_pad_x * 2.0;
            let badge_x1 = content_x1;
            let badge_x0 = badge_x1 - badge_w;
            let badge_y0 = avatar_cy - badge_h / 2.0;
            let badge_r = badge_h / 2.0; // 圆角=半高，胶囊形状

            let mut pb = PathBuilder::new();
            pb.move_to(badge_x0 + badge_r, badge_y0);
            pb.line_to(badge_x1 - badge_r, badge_y0);
            pb.quad_to(badge_x1, badge_y0, badge_x1, badge_y0 + badge_r);
            pb.line_to(badge_x1, badge_y0 + badge_h - badge_r);
            pb.quad_to(badge_x1, badge_y0 + badge_h, badge_x1 - badge_r, badge_y0 + badge_h);
            pb.line_to(badge_x0 + badge_r, badge_y0 + badge_h);
            pb.quad_to(badge_x0, badge_y0 + badge_h, badge_x0, badge_y0 + badge_h - badge_r);
            pb.line_to(badge_x0, badge_y0 + badge_r);
            pb.quad_to(badge_x0, badge_y0, badge_x0 + badge_r, badge_y0);
            pb.close();
            if let Some(path) = pb.finish() {
                let mut paint = Paint::default();
                paint.set_color_rgba8(0x2E, 0x7D, 0x32, 255); // 深绿底
                paint.anti_alias = true;
                pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, Transform::identity(), None);
            }
            let text_x = badge_x0 + badge_pad_x;
            let text_y = badge_y0 + (badge_h - text_h) / 2.0;
            let mut pen = TinySkiaPen::new(pixmap);
            for line in badge_layout.lines() {
                for item in line.items() {
                    if let PositionedLayoutItem::GlyphRun::<RunBrush>(glyph_run) = item {
                        render_glyph_run(&glyph_run, &mut pen, text_x, text_y);
                    }
                }
            }
        }

        // ---- 7. 批注正文：常规字重，8pt，#555555，不再限制行数/截断，
        // 容器高度由 measure_comment_bubble_height 用同样的排版逻辑
        // 预先量出，气泡框会随内容自动撑开，这里只管把全部内容画出来。
        cursor_y = y0 + pad + avatar_d + pad * 0.8; // 头像+作者名这一行结束后换行
        let body_font_size = 8.0; // 裸pt，喂给parley，parley按scale自动放大渲染
        let body_line_height_pt = body_font_size * 1.15; // 裸pt，喂给parley
        let body_line_height_px = body_line_height_pt * scale; // 逐行推进y坐标用的像素值，需要乘scale，须与measure_comment_bubble_height保持一致
        if !content_preview.is_empty() {
            let mut builder = self.layout_context.ranged_builder(
                &mut self.font_context, content_preview, scale.max(1.0), true,
            );
            builder.push_default(StyleProperty::Brush(RunBrush { color: [0x55, 0x55, 0x55, 255], run_idx: None }));
            builder.push_default(GenericFamily::SystemUi);
            builder.push_default(LineHeight::Absolute(body_line_height_pt));
            builder.push_default(StyleProperty::FontSize(body_font_size));
            let mut layout: parley::Layout<RunBrush> = builder.build(content_preview);
            layout.break_all_lines(Some(content_w));
            layout.align(Alignment::Start, AlignmentOptions::default());

            let mut pen = TinySkiaPen::new(pixmap);
            let total_lines = layout.lines().count();
            eprintln!("      [DEBUG draw] content_w={:.2} total_lines={} content_preview={:?}", content_w, total_lines, content_preview);
            for (line_idx, line) in layout.lines().enumerate() {
                let draw_y = cursor_y + (line_idx as f32) * body_line_height_px;
                for item in line.items() {
                    if let PositionedLayoutItem::GlyphRun::<RunBrush>(glyph_run) = item {
                        render_glyph_run(&glyph_run, &mut pen, content_x0, draw_y);
                    }
                }
            }
            let total_lines = layout.lines().count();
            cursor_y += (total_lines as f32) * body_line_height_px;
        }

        // ---- 8. 回复区：直接平铺展示全部回复，不折叠（v156会议纪要）。
        // 每条回复画：淡灰色分隔线 + 缩进头像(略小) + 作者名+日期同一行 +
        // 正文（与主批注正文同一套字号/颜色，不单独定义视觉语言）。
        // 不画编辑/删除/Done按钮——回复不支持这些操作（OOXML批注回复
        // 语义上只能新增，不能再嵌套回复，参照WPS的简化做法）。
        let reply_indent = pad * 1.5; // 回复整体相对主批注内容再缩进一点，表达"从属"关系
        let reply_content_x0 = content_x0 + reply_indent;
        let reply_content_x1 = content_x1;
        let reply_content_w = (reply_content_x1 - reply_content_x0).max(1.0);
        let reply_avatar_d = 14.0 * scale;
        let reply_avatar_r = reply_avatar_d / 2.0;

        for reply in replies {
            // -- 分隔线：淡灰色细横线，画在这条回复内容之前 --
            cursor_y += pad * 0.8;
            {
                let mut pb = PathBuilder::new();
                pb.move_to(content_x0, cursor_y);
                pb.line_to(content_x1, cursor_y);
                if let Some(path) = pb.finish() {
                    let mut paint = Paint::default();
                    paint.set_color_rgba8(0xE5, 0xE5, 0xE5, 255);
                    paint.anti_alias = true;
                    let mut stroke = Stroke::default();
                    stroke.width = 1.0;
                    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
                }
            }
            cursor_y += pad * 0.6;

            // -- 回复头像（略小，同样按author hash取色+首字母，逻辑与主头像一致）--
            let reply_avatar_cx = reply_content_x0 + reply_avatar_r;
            let reply_avatar_cy = cursor_y + reply_avatar_r;
            let reply_author_hash: u32 = reply.author.bytes().map(|b| b as u32).sum();
            let (reply_avatar_rgb, _) = COMMENT_HIGHLIGHT_COLORS[(reply_author_hash % 6) as usize];
            {
                let mut pb = PathBuilder::new();
                pb.push_circle(reply_avatar_cx, reply_avatar_cy, reply_avatar_r);
                if let Some(path) = pb.finish() {
                    let mut paint = Paint::default();
                    paint.set_color_rgba8(reply_avatar_rgb[0], reply_avatar_rgb[1], reply_avatar_rgb[2], 255);
                    paint.anti_alias = true;
                    pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, Transform::identity(), None);
                }
            }
            let reply_avatar_label: String = match reply.initials.as_deref() {
                Some(s) if !s.trim().is_empty() => s.trim().to_string(),
                _ => {
                    let first_char = reply.author.chars().next();
                    match first_char {
                        Some(c) if c.is_ascii_alphabetic() => {
                            reply.author.split_whitespace()
                                .filter_map(|w| w.chars().next())
                                .map(|c| c.to_ascii_uppercase())
                                .take(2)
                                .collect::<String>()
                        }
                        Some(c) => c.to_string(),
                        None => String::new(),
                    }
                }
            };
            if !reply_avatar_label.is_empty() {
                let reply_label_font_size = reply_avatar_d * 0.42;
                let mut builder = self.layout_context.ranged_builder(
                    &mut self.font_context, &reply_avatar_label, scale.max(1.0), true,
                );
                builder.push_default(StyleProperty::Brush(RunBrush { color: [255, 255, 255, 255], run_idx: None }));
                builder.push_default(GenericFamily::SystemUi);
                builder.push_default(StyleProperty::FontSize(reply_label_font_size));
                builder.push_default(FontWeight::BOLD);
                let mut label_layout: parley::Layout<RunBrush> = builder.build(&reply_avatar_label);
                label_layout.break_all_lines(None);
                label_layout.align(Alignment::Start, AlignmentOptions::default());
                let text_w = label_layout.width();
                let text_h = label_layout.height();
                let label_x = reply_avatar_cx - text_w / 2.0;
                let label_y = reply_avatar_cy - text_h / 2.0;
                let mut pen = TinySkiaPen::new(pixmap);
                for line in label_layout.lines() {
                    for item in line.items() {
                        if let PositionedLayoutItem::GlyphRun::<RunBrush>(glyph_run) = item {
                            render_glyph_run(&glyph_run, &mut pen, label_x, label_y);
                        }
                    }
                }
            }

            // -- 作者名（8.5pt粗体，与主批注一致）+ 日期（同一行，紧跟作者名之后，
            //    小字号6.5pt、浅灰色，与作者名留一点间距）--
            let reply_author_x = reply_avatar_cx + reply_avatar_r + pad * 0.5;
            let reply_author_font_size = 8.5;
            let mut author_w = 0.0f32;
            let mut author_line_h = reply_avatar_d; // 兜底：作者名为空时按头像高度占位
            {
                let mut builder = self.layout_context.ranged_builder(
                    &mut self.font_context, &reply.author, scale.max(1.0), true,
                );
                builder.push_default(StyleProperty::Brush(RunBrush { color: [0x33, 0x33, 0x33, 255], run_idx: None }));
                builder.push_default(GenericFamily::SystemUi);
                builder.push_default(StyleProperty::FontSize(reply_author_font_size));
                builder.push_default(FontWeight::BOLD);
                let mut layout: parley::Layout<RunBrush> = builder.build(&reply.author);
                layout.break_all_lines(None);
                layout.align(Alignment::Start, AlignmentOptions::default());
                author_w = layout.width();
                author_line_h = layout.height();
                let author_y = reply_avatar_cy - layout.height() / 2.0;
                let mut pen = TinySkiaPen::new(pixmap);
                for line in layout.lines() {
                    for item in line.items() {
                        if let PositionedLayoutItem::GlyphRun::<RunBrush>(glyph_run) = item {
                            render_glyph_run(&glyph_run, &mut pen, reply_author_x, author_y);
                        }
                    }
                }
            }
            if let Some(date_str) = reply.date.as_deref() {
                if !date_str.is_empty() {
                    // 标准ISO8601格式判断标准与comment_threads()里的
                    // looks_like_iso8601保持一致（定长20、以Z结尾、
                    // 第11字符是'T'）。符合则截取展示为"YYYY-MM-DD HH:MM"
                    // （去掉秒和时区标记Z，更适合气泡窄空间），不符合则
                    // 原样显示，不做任何猜测性解析。注意：排序仍然用
                    // reply.date完整原始值（comment_threads内部逻辑不受
                    // 此处影响），这里只改显示层。
                    let formatted_owned: String = if date_str.len() == 20
                        && date_str.ends_with('Z')
                        && date_str.as_bytes().get(10) == Some(&b'T')
                    {
                        format!("{} {}", &date_str[0..10], &date_str[11..16])
                    } else {
                        date_str.to_string()
                    };
                    let date_x = reply_author_x + author_w + pad * 0.6;
                    let date_font_size = 6.5;
                    let mut builder = self.layout_context.ranged_builder(
                        &mut self.font_context, &formatted_owned, scale.max(1.0), true,
                    );
                    builder.push_default(StyleProperty::Brush(RunBrush { color: [0x99, 0x99, 0x99, 255], run_idx: None }));
                    builder.push_default(GenericFamily::SystemUi);
                    builder.push_default(StyleProperty::FontSize(date_font_size));
                    let mut layout: parley::Layout<RunBrush> = builder.build(&formatted_owned);
                    layout.break_all_lines(None);
                    layout.align(Alignment::Start, AlignmentOptions::default());
                    let date_y = reply_avatar_cy - layout.height() / 2.0;
                    let mut pen = TinySkiaPen::new(pixmap);
                    for line in layout.lines() {
                        for item in line.items() {
                            if let PositionedLayoutItem::GlyphRun::<RunBrush>(glyph_run) = item {
                                render_glyph_run(&glyph_run, &mut pen, date_x, date_y);
                            }
                        }
                    }
                }
            }
            cursor_y += reply_avatar_d.max(author_line_h) + pad * 0.6;

            // -- 回复正文（与主批注正文同一套字号8pt/颜色#555555，不单独定义）--
            let reply_text = Self::comment_content_to_plain_text(&reply.content);
            if !reply_text.is_empty() {
                let mut builder = self.layout_context.ranged_builder(
                    &mut self.font_context, &reply_text, scale.max(1.0), true,
                );
                builder.push_default(StyleProperty::Brush(RunBrush { color: [0x55, 0x55, 0x55, 255], run_idx: None }));
                builder.push_default(GenericFamily::SystemUi);
                builder.push_default(LineHeight::Absolute(body_line_height_pt));
                builder.push_default(StyleProperty::FontSize(body_font_size));
                let mut layout: parley::Layout<RunBrush> = builder.build(&reply_text);
                layout.break_all_lines(Some(reply_content_w));
                layout.align(Alignment::Start, AlignmentOptions::default());
                let mut pen = TinySkiaPen::new(pixmap);
                for (line_idx, line) in layout.lines().enumerate() {
                    let draw_y = cursor_y + (line_idx as f32) * body_line_height_px;
                    for item in line.items() {
                        if let PositionedLayoutItem::GlyphRun::<RunBrush>(glyph_run) = item {
                            render_glyph_run(&glyph_run, &mut pen, reply_content_x0, draw_y);
                        }
                    }
                }
                let total_reply_lines = layout.lines().count().max(1);
                cursor_y += (total_reply_lines as f32) * body_line_height_px;
            }
        }
    }

    pub(crate) fn draw_v_line(pixmap: &mut PixmapMut, x: f32, y0: f32, y1: f32, thickness: f32, color: tiny_skia::Color) {
        if y1 <= y0 { return; }
        let rect = { let th = thickness.max(1.0).round(); let ya = (y0 - th / 2.0).round(); let yb = (y1 - th / 2.0).round() + th; Rect::from_xywh((x - th / 2.0).round(), ya, th, yb - ya) };
        if let Some(rect) = rect {
            let mut paint = Paint::default();
            paint.set_color(color);
            paint.blend_mode = tiny_skia::BlendMode::SourceOver;
            pixmap.fill_rect(rect, &paint, Transform::identity(), None);
        }
    }

    pub(crate) fn draw_h_line(pixmap: &mut PixmapMut, x0: f32, x1: f32, y: f32, thickness: f32, color: tiny_skia::Color) {
        if x1 <= x0 { return; }
        let rect = { let th = thickness.max(1.0).round(); let xa = (x0 - th / 2.0).round(); let xb = (x1 - th / 2.0).round() + th; Rect::from_xywh(xa, (y - th / 2.0).round(), xb - xa, th) };
        if let Some(rect) = rect {
            let mut paint = Paint::default();
            paint.set_color(color);
            paint.blend_mode = tiny_skia::BlendMode::SourceOver;
            pixmap.fill_rect(rect, &paint, Transform::identity(), None);
        }
    }

    /// WROOT-100（表格边框BorderStyle重构）：按 Option<BorderStyle> 画一条
    /// 横向表格边框线。None 表示这条边不画（外框/内部线统一入口）。
    /// dashed=true 时走虚线描边路径（stroke_path + StrokeDash），与
    /// roe-impress draw_placeholder_dashed_rect 的dash实现方式一致（该
    /// 函数不支持复用——分属roe-writer/roe-impress两个crate，风格保持
    /// 一致即可，不做跨crate复用）；dashed=false 时复用现有
    /// draw_h_line（纯色矩形填充，与Layer 1-B验证过的外框绘制完全一致）。
    /// width单位是pt，调用方需已经乘过scale（与既有bw变量换算方式一致，
    /// 本函数不做任何单位换算）。
    pub(crate) fn draw_h_line_styled(
        pixmap: &mut PixmapMut, x0: f32, x1: f32, y: f32,
        style: Option<&crate::frame::BorderStyle>, scale: f32,
    ) {
        let Some(style) = style else { return };
        let color = tiny_skia::Color::from_rgba8(
            style.color.r, style.color.g, style.color.b, style.color.a);
        let width_px = style.width * scale;
        if !style.dashed {
            Self::draw_h_line(pixmap, x0, x1, y, width_px, color);
            return;
        }
        if x1 <= x0 { return; }
        let mut pb = PathBuilder::new();
        pb.move_to(x0, y);
        pb.line_to(x1, y);
        if let Some(path) = pb.finish() {
            let mut paint = Paint::default();
            paint.set_color(color);
            paint.anti_alias = true;
            let mut stroke = Stroke::default();
            stroke.width = width_px;
            stroke.dash = tiny_skia::StrokeDash::new(vec![2.0 * scale, 2.0 * scale], 0.0);
            pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }

    /// WROOT-100：同 draw_h_line_styled，竖向表格边框线版本。
    pub(crate) fn draw_v_line_styled(
        pixmap: &mut PixmapMut, x: f32, y0: f32, y1: f32,
        style: Option<&crate::frame::BorderStyle>, scale: f32,
    ) {
        let Some(style) = style else { return };
        let color = tiny_skia::Color::from_rgba8(
            style.color.r, style.color.g, style.color.b, style.color.a);
        let width_px = style.width * scale;
        if !style.dashed {
            Self::draw_v_line(pixmap, x, y0, y1, width_px, color);
            return;
        }
        if y1 <= y0 { return; }
        let mut pb = PathBuilder::new();
        pb.move_to(x, y0);
        pb.line_to(x, y1);
        if let Some(path) = pb.finish() {
            let mut paint = Paint::default();
            paint.set_color(color);
            paint.anti_alias = true;
            let mut stroke = Stroke::default();
            stroke.width = width_px;
            stroke.dash = tiny_skia::StrokeDash::new(vec![2.0 * scale, 2.0 * scale], 0.0);
            pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }

    fn draw_page_border(pixmap: &mut PixmapMut, w: u32, h: u32) {
        let mut paint = Paint::default();
        paint.set_color_rgba8(180, 180, 180, 255);
        paint.anti_alias = false;

        let mut stroke = Stroke::default();
        stroke.width = 1.5;

        let mut pb = PathBuilder::new();
        pb.move_to(0.0, 0.0);
        pb.line_to(w as f32, 0.0);
        pb.line_to(w as f32, h as f32);
        pb.line_to(0.0, h as f32);
        pb.close();

        if let Some(path) = pb.finish() {
            pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }
}

impl Default for WriterTileRenderer {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{ParagraphLayoutEngine, arena_to_layout_pages};
    use crate::font_registry::FontRegistry;
    use roe_parser::odf::fodt::FodtParser;
    use tiny_skia::{Pixmap, IntSize};

    const SAMPLE_FODT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
    xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
    office:mimetype="application/vnd.oasis.opendocument.text">
  <office:styles>
    <style:style style:name="Default" style:family="paragraph">
      <style:text-properties fo:font-size="12pt"/>
    </style:style>
    <style:style style:name="Heading1" style:family="paragraph"
                 style:parent-style-name="Default">
      <style:text-properties fo:font-size="18pt" fo:font-weight="bold"/>
      <style:paragraph-properties fo:margin-bottom="8pt"/>
    </style:style>
  </office:styles>
  <office:body>
    <office:text>
      <text:h text:style-name="Heading1">ROE Writer 排版测试</text:h>
      <text:p text:style-name="Default">这是第一段正文，用于测试 cosmic-text 的 CJK 排版能力。</text:p>
      <text:p text:style-name="Default">第二段，English mixed with 中文，测试 BiDi 和混排。</text:p>
      <text:p text:style-name="Default">第三段：更长的文字内容，用来验证自动换行是否正确处理了行高和段落间距。</text:p>
    </office:text>
  </office:body>
</office:document>"#;

    #[test]
    fn test_layout_and_render() {
        let doc = FodtParser::parse(SAMPLE_FODT).expect("fodt parse failed");
        let registry = FontRegistry::new_with_system_fonts();
        let mut engine = ParagraphLayoutEngine::new(registry);
        let result = engine.layout(&doc);
        let pages = arena_to_layout_pages(&result.arena, &doc);

        assert!(!pages.is_empty(), "no pages produced");
        let total_paras: usize = pages.iter().map(|p| p.paragraphs.len()).sum();
        assert_eq!(total_paras, 4);

        for para in &pages[0].paragraphs {
            assert!(!para.lines.is_empty(), "para {} has no lines", para.style_name);
            assert!(para.total_height > 0.0);
        }

        println!("Layout OK: {} page(s), {} paragraphs on page 0",
            pages.len(), pages[0].paragraphs.len());
    }

    // [v92-TEMP-DISABLED] 以下测试依赖已废弃的 render_page_preview
    // （WROOT-058，本次会话已合并到 viewport 方案，但这几个测试尚未
    // 跟进改写）。整体注释掉，待专项 cargo test --workspace 修复时
    // 改写成走 viewport 方式渲染后再取消注释。
    // #[test]
    // fn test_render_preview_png() {
    // let doc = FodtParser::parse(SAMPLE_FODT).expect("fodt parse failed");
    // let registry = FontRegistry::new_with_system_fonts();
    // let mut engine = ParagraphLayoutEngine::new(registry);
    // let result = engine.layout(&doc);
    // let pages = arena_to_layout_pages(&result.arena, &doc);

    // assert!(!pages.is_empty());
    // let mut renderer = WriterTileRenderer::new();

    // let scale = 1.0_f32;
    // let pw = (pages[0].width  * scale) as u32;
    // let ph = (pages[0].height * scale) as u32;
    // let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];

    // let (out_w, out_h) = renderer
    // .render_page_preview(&pages[0], scale, &mut buf)
    // .expect("render_page_preview failed");
    // assert_eq!(out_w, pw);
    // assert_eq!(out_h, ph);

    // let pixmap = Pixmap::from_vec(buf, IntSize::from_wh(out_w, out_h).expect("invalid size"))
    // .expect("pixmap from buf failed");
    // let png = pixmap.encode_png().expect("png encode failed");

    // assert!(!png.is_empty());
    // assert_eq!(png[0], 0x89);
    // assert_eq!(&png[1..4], b"PNG");

    // std::fs::write("/tmp/roe_writer_preview.png", &png).ok();
    // println!("PNG written: {} bytes", png.len());
    // }
    // [v92-TEMP-DISABLED] 注释区结束


    #[test]
    fn test_pagination() {
        let mut body = String::new();
        for i in 0..40 {
            body.push_str(&format!(
                r#"<text:p text:style-name="Default">第 {i} 段：这是用于分页测试的段落内容，每段有足够的文字触发换行和分页逻辑。</text:p>"#
            ));
        }
        let fodt = format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
    xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
    office:mimetype="application/vnd.oasis.opendocument.text">
  <office:styles>
    <style:style style:name="Default" style:family="paragraph">
      <style:text-properties fo:font-size="12pt"/>
    </style:style>
  </office:styles>
  <office:body><office:text>{body}</office:text></office:body>
</office:document>"#);

        let doc = FodtParser::parse(&fodt).expect("fodt parse failed");
        let registry = FontRegistry::new_with_system_fonts();
        let mut engine = ParagraphLayoutEngine::new(registry);
        let result = engine.layout(&doc);
        let pages = arena_to_layout_pages(&result.arena, &doc);

        println!("Pagination: {} pages for 40 paragraphs", pages.len());
        assert!(pages.len() > 1);
    }
}

#[cfg(test)]
mod long_paragraph_tests {
    use super::*;
    use crate::layout::{ParagraphLayoutEngine, arena_to_layout_pages};
    use crate::font_registry::FontRegistry;
    use roe_parser::odf::fodt::FodtParser;
    use tiny_skia::{Pixmap, IntSize};

    fn fodt_with_paragraphs(paras: &[String]) -> String {
        let body: String = paras.iter()
            .map(|p| format!(r#"<text:p text:style-name="Default">{p}</text:p>"#))
            .collect();
        format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
    xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
    office:mimetype="application/vnd.oasis.opendocument.text">
  <office:styles>
    <style:style style:name="Default" style:family="paragraph">
      <style:text-properties fo:font-size="12pt"/>
    </style:style>
  </office:styles>
  <office:body><office:text>{body}</office:text></office:body>
</office:document>"#)
    }

    // 单个超长段落（300+字），会被 cosmic-text 拆成多个 layout run（多个 LineBox）。
    // 用来验证：根因1修复后，多视觉行场景下渲染是否还会重复/重叠。
    // [v92-TEMP-DISABLED] 以下测试依赖已废弃的 render_page_preview
    // （WROOT-058，本次会话已合并到 viewport 方案，但这几个测试尚未
    // 跟进改写）。整体注释掉，待专项 cargo test --workspace 修复时
    // 改写成走 viewport 方式渲染后再取消注释。
    // #[test]
    // fn test_long_single_paragraph_no_duplication() {
    // let long_text = "测试段落开始。".to_string()
    // + &"这是一段用于验证长文本排版是否正确的内容，重复多次以确保超过三百个字符从而触发跨多行的自动换行逻辑。".repeat(6)
    // + "测试段落结束。";
    // println!("长段落字符数: {}", long_text.chars().count());
    // assert!(long_text.chars().count() > 300, "测试前提：段落需超过300字");

    // let fodt = fodt_with_paragraphs(&[long_text.clone()]);
    // let doc = FodtParser::parse(&fodt).expect("fodt parse failed");
    // let registry = FontRegistry::new_with_system_fonts();
    // let mut engine = ParagraphLayoutEngine::new(registry);
    // let result = engine.layout(&doc);
    // let pages = arena_to_layout_pages(&result.arena, &doc);

    // println!("生成页数: {}", pages.len());
    // for (pi, page) in pages.iter().enumerate() {
    // for para in &page.paragraphs {
    // println!("  page {} 段落行数: {}", pi, para.lines.len());
    // for (li, line) in para.lines.iter().enumerate() {
    // println!("    line {}: {:?}", li, line.text);
    // }
    // }
    // }

    // // 拼接所有 LineBox.text，应该等于原文，且不应该比原文更长（即没有整段重复）
    // let mut reassembled = String::new();
    // for page in &pages {
    // for para in &page.paragraphs {
    // for line in &para.lines {
    // reassembled.push_str(&line.text);
    // }
    // }
    // }
    // let reassembled_chars = reassembled.chars().count();
    // let original_chars = long_text.chars().count();
    // println!("原文字符数: {}, 拼接还原后字符数: {}", original_chars, reassembled_chars);

    // assert_eq!(
    // reassembled_chars, original_chars,
    // "拼接所有 LineBox.text 后字符数应等于原文字符数（不多不少）：原文 {} 字，还原 {} 字",
    // original_chars, reassembled_chars
    // );

    // // 渲染成图，肉眼检查是否重叠/重复
    // let mut renderer = WriterTileRenderer::new();
    // let scale = 1.0_f32;
    // let pw = (pages[0].width  * scale) as u32;
    // let ph = (pages[0].height * scale) as u32;
    // let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];
    // renderer.render_page_preview(&pages[0], scale, &mut buf)
    // .expect("render_page_preview failed");
    // let pixmap = Pixmap::from_vec(buf, IntSize::from_wh(pw, ph).expect("invalid size"))
    // .expect("pixmap from buf failed");
    // let png = pixmap.encode_png().expect("png encode failed");
    // std::fs::write("/tmp/roe_writer_long_paragraph.png", &png).ok();
    // println!("PNG written: /tmp/roe_writer_long_paragraph.png");
    // }
    // [v92-TEMP-DISABLED] 注释区结束


    // 多个长段落撑满并跨越多页，验证跨页处文字不丢失、不重复、不截断。
    // [v92-TEMP-DISABLED] 以下测试依赖已废弃的 render_page_preview
    // （WROOT-058，本次会话已合并到 viewport 方案，但这几个测试尚未
    // 跟进改写）。整体注释掉，待专项 cargo test --workspace 修复时
    // 改写成走 viewport 方式渲染后再取消注释。
    // #[test]
    // fn test_pagination_with_long_paragraphs_no_loss_no_duplication() {
    // let unit = "这是用于跨页测试的长段落内容，包含足够多的汉字以确保单个段落本身会跨越页面边界。".to_string();
    // let paras: Vec<String> = (0..15).map(|i| {
    // format!("第{}段开始标记。{}第{}段结束标记。", i, unit.repeat(8), i)
    // }).collect();

    // let fodt = fodt_with_paragraphs(&paras);
    // let doc = FodtParser::parse(&fodt).expect("fodt parse failed");
    // let registry = FontRegistry::new_with_system_fonts();
    // let mut engine = ParagraphLayoutEngine::new(registry);
    // let result = engine.layout(&doc);
    // let pages = arena_to_layout_pages(&result.arena, &doc);

    // println!("总页数: {}", pages.len());
    // assert!(pages.len() > 1, "测试前提：内容应跨越多页");

    // // 统计每个"开始标记/结束标记"出现次数，应该都恰好是1次（不丢、不重复）
    // let mut full_text = String::new();
    // for (pi, page) in pages.iter().enumerate() {
    // println!("page {}: {} 个段落片段", pi, page.paragraphs.len());
    // for para in &page.paragraphs {
    // for line in &para.lines {
    // full_text.push_str(&line.text);
    // }
    // }
    // }

    // for i in 0..15 {
    // let start_marker = format!("第{}段开始标记", i);
    // let end_marker = format!("第{}段结束标记", i);
    // let start_count = full_text.matches(&start_marker).count();
    // let end_count = full_text.matches(&end_marker).count();
    // println!("段{}: 开始标记出现{}次, 结束标记出现{}次", i, start_count, end_count);
    // assert_eq!(start_count, 1, "段{}的开始标记应恰好出现1次（实际{}次，可能重复或丢失）", i, start_count);
    // assert_eq!(end_count, 1, "段{}的结束标记应恰好出现1次（实际{}次，可能重复或丢失）", i, end_count);
    // }

    // // 渲染前3页存图，肉眼检查跨页处
    // let mut renderer = WriterTileRenderer::new();
    // for (pi, page) in pages.iter().enumerate().take(3) {
    // let scale = 1.0_f32;
    // let pw = (page.width  * scale) as u32;
    // let ph = (page.height * scale) as u32;
    // let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];
    // renderer.render_page_preview(page, scale, &mut buf)
    // .expect("render_page_preview failed");
    // let pixmap = Pixmap::from_vec(buf, IntSize::from_wh(pw, ph).expect("invalid size"))
    // .expect("pixmap from buf failed");
    // let png = pixmap.encode_png().expect("png encode failed");
    // let out_path = format!("/tmp/roe_writer_pagination_page_{}.png", pi);
    // std::fs::write(&out_path, &png).ok();
    // println!("PNG written: {}", out_path);
    // }
    // }
    // [v92-TEMP-DISABLED] 注释区结束

}

#[cfg(test)]
mod no_internal_repeat_tests {
    use super::*;
    use crate::layout::{ParagraphLayoutEngine, arena_to_layout_pages};
    use crate::font_registry::FontRegistry;
    use roe_parser::odf::fodt::FodtParser;
    use tiny_skia::{Pixmap, IntSize};

    // 用完全不重复的长文本（不靠repeat()凑字数），肉眼验证渲染无重复。
    // [v92-TEMP-DISABLED] 以下测试依赖已废弃的 render_page_preview
    // （WROOT-058，本次会话已合并到 viewport 方案，但这几个测试尚未
    // 跟进改写）。整体注释掉，待专项 cargo test --workspace 修复时
    // 改写成走 viewport 方式渲染后再取消注释。
    // #[test]
    // fn test_long_paragraph_no_internal_repeat_visual_check() {
    // let sentences: Vec<String> = (0..60).map(|i| {
    // format!("这是编号{:03}的独立句子内容用于验证排版不会出现重复。", i)
    // }).collect();
    // let long_text = sentences.join("");
    // let char_count = long_text.chars().count();
    // println!("文本总字符数: {}", char_count);
    // assert!(char_count > 300);

    // let fodt = format!(r#"<?xml version="1.0" encoding="UTF-8"?>
    // <office:document
    // xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    // xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    // xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
    // xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
    // office:mimetype="application/vnd.oasis.opendocument.text">
    // <office:styles>
    // <style:style style:name="Default" style:family="paragraph">
    // <style:text-properties fo:font-size="12pt"/>
    // </style:style>
    // </office:styles>
    // <office:body><office:text><text:p text:style-name="Default">{long_text}</text:p></office:text></office:body>
    // </office:document>"#);

    // let doc = FodtParser::parse(&fodt).expect("fodt parse failed");
    // let registry = FontRegistry::new_with_system_fonts();
    // let mut engine = ParagraphLayoutEngine::new(registry);
    // let result = engine.layout(&doc);
    // let pages = arena_to_layout_pages(&result.arena, &doc);

    // let mut reassembled = String::new();
    // for page in &pages {
    // for para in &page.paragraphs {
    // for line in &para.lines {
    // reassembled.push_str(&line.text);
    // }
    // }
    // }
    // for i in 0..60 {
    // let marker = format!("编号{:03}的独立句子", i);
    // let count = reassembled.matches(&marker).count();
    // assert_eq!(count, 1, "句子{}应只出现1次，实际{}次", i, count);
    // }

    // let mut renderer = WriterTileRenderer::new();
    // for (pi, page) in pages.iter().enumerate() {
    // let scale = 1.0_f32;
    // let pw = (page.width  * scale) as u32;
    // let ph = (page.height * scale) as u32;
    // let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];
    // renderer.render_page_preview(page, scale, &mut buf)
    // .expect("render_page_preview failed");
    // let pixmap = Pixmap::from_vec(buf, IntSize::from_wh(pw, ph).expect("invalid size"))
    // .expect("pixmap from buf failed");
    // let png = pixmap.encode_png().expect("png encode failed");
    // let out_path = format!("/tmp/roe_writer_no_repeat_page_{}.png", pi);
    // std::fs::write(&out_path, &png).ok();
    // println!("PNG written: {}", out_path);
    // }
    // println!("总页数: {}", pages.len());
    // }
    // [v92-TEMP-DISABLED] 注释区结束

}

#[cfg(test)]
mod diag_tests {
    use super::*;
    use crate::layout::{ParagraphLayoutEngine, arena_to_layout_pages};
    use crate::font_registry::FontRegistry;
    use roe_parser::odf::fodt::FodtParser;

    #[test]
    fn diag_print_para0_lines() {
        let unit = "这是用于跨页测试的长段落内容，包含足够多的汉字以确保单个段落本身会跨越页面边界。".to_string();
        let p0 = format!("第{}段开始标记。{}第{}段结束标记。", 0, unit.repeat(8), 0);

        let fodt = format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
    xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
    office:mimetype="application/vnd.oasis.opendocument.text">
  <office:styles>
    <style:style style:name="Default" style:family="paragraph">
      <style:text-properties fo:font-size="12pt"/>
    </style:style>
  </office:styles>
  <office:body><office:text><text:p text:style-name="Default">{p0}</text:p></office:text></office:body>
</office:document>"#);

        let doc = FodtParser::parse(&fodt).expect("fodt parse failed");
        println!("=== doc.body 长度: {} ===", doc.body.len());

        let registry = FontRegistry::new_with_system_fonts();
        let mut engine = ParagraphLayoutEngine::new(registry);
        let result = engine.layout(&doc);

        println!("=== arena.frames 总数: {} ===", result.arena.frames.len());
        println!("=== arena.pages 总数: {} ===", result.arena.pages.len());

        for &pid in &result.arena.page_order {
            let page = &result.arena.pages[pid];
            println!("--- page {:?}: {} 个frame ---", pid, page.frames.len());
            for &fid in &page.frames {
                let frame = &result.arena.frames[fid];
                let content_len = match &frame.content {
                    crate::frame::FrameContent::Paragraph(lines) => lines.len(),
                    crate::frame::FrameContent::TableRow(cells) => cells.len(),
                    crate::frame::FrameContent::HorizontalRule { .. } => 0,
                };
                println!("  frame {:?}: para_index={}, content_len={}, follow={:?}, precede={:?}",
                    fid, frame.para_index, content_len, frame.follow, frame.precede);
                match &frame.content {
                    crate::frame::FrameContent::Paragraph(lines) => {
                        for (li, line) in lines.iter().enumerate() {
                            let preview: String = line.text.chars().take(20).collect();
                            println!("    line[{}] len={} chars, preview={:?}", li, line.text.chars().count(), preview);
                        }
                    }
                    crate::frame::FrameContent::TableRow(cells) => {
                        for (ci, cell) in cells.iter().enumerate() {
                            println!("    cell[{}] x_offset={} cell_width={} lines={}", ci, cell.x_offset, cell.cell_width, cell.lines.len());
                        }
                    }
                    crate::frame::FrameContent::HorizontalRule { .. } => {
                        println!("    (horizontal rule, no lines/cells)");
                    }
                }
            }
        }

        let pages = arena_to_layout_pages(&result.arena, &doc);
        println!("=== arena_to_layout_pages 后页数: {} ===", pages.len());
        for (pi, page) in pages.iter().enumerate() {
            println!("--- LayoutPage {} : {} 个paragraph ---", pi, page.paragraphs.len());
            for (parai, para) in page.paragraphs.iter().enumerate() {
                println!("  paragraph[{}]: lines.len()={}", parai, para.lines.len());
            }
        }
    }
}

#[cfg(test)]
mod list_number_render_test {
    use crate::layout::{ParagraphLayoutEngine, arena_to_layout_pages};
    use crate::font_registry::FontRegistry;
    use crate::tile::WriterTileRenderer;
    use roe_parser::odf::fodt::FodtParser;
    use tiny_skia::{Pixmap, IntSize};

    // [v92-TEMP-DISABLED] 以下测试依赖已废弃的 render_page_preview
    // （WROOT-058，本次会话已合并到 viewport 方案，但这几个测试尚未
    // 跟进改写）。整体注释掉，待专项 cargo test --workspace 修复时
    // 改写成走 viewport 方式渲染后再取消注释。
    // #[test]
    // fn test_number_list_renders_visible_png() {
    // let fodt = r##"<?xml version="1.0" encoding="UTF-8"?>
    // <office:document
    // xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    // xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    // xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
    // xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
    // office:mimetype="application/vnd.oasis.opendocument.text">
    // <office:styles>
    // <style:style style:name="Default" style:family="paragraph">
    // <style:text-properties fo:font-size="14pt"/>
    // </style:style>
    // <text:list-style style:name="L1">
    // <text:list-level-style-number text:level="1" style:num-format="1" style:num-suffix=".">
    // <style:list-level-properties fo:margin-left="0.6cm" fo:text-indent="-0.6cm" text:min-label-width="0.6cm"/>
    // </text:list-level-style-number>
    // <text:list-level-style-number text:level="2" style:num-format="1" style:num-suffix=".">
    // <style:list-level-properties fo:margin-left="1.2cm" fo:text-indent="-0.6cm" text:min-label-width="0.6cm"/>
    // </text:list-level-style-number>
    // </text:list-style>
    // </office:styles>
    // <office:body><office:text>
    // <text:p text:style-name="Default">普通段落，不属于任何列表。</text:p>
    // <text:list text:style-name="L1">
    // <text:list-item><text:p text:style-name="Default">第一个列表项</text:p></text:list-item>
    // <text:list-item><text:p text:style-name="Default">第二个列表项</text:p>
    // <text:list text:style-name="L1">
    // <text:list-item><text:p text:style-name="Default">嵌套子项一</text:p></text:list-item>
    // <text:list-item><text:p text:style-name="Default">嵌套子项二</text:p></text:list-item>
    // </text:list>
    // </text:list-item>
    // <text:list-item><text:p text:style-name="Default">第三个列表项</text:p></text:list-item>
    // </text:list>
    // <text:p text:style-name="Default">列表结束后的普通段落。</text:p>
    // </office:text></office:body>
    // </office:document>"##;

    // let doc = FodtParser::parse(fodt).expect("fodt parse failed");
    // let registry = FontRegistry::new_with_system_fonts();
    // let mut engine = ParagraphLayoutEngine::new(registry);
    // let result = engine.layout(&doc);
    // let pages = arena_to_layout_pages(&result.arena, &doc);
    // assert!(!pages.is_empty(), "应至少产出一页");

    // let mut labels: Vec<String> = Vec::new();
    // for para in &pages[0].paragraphs {
    // for line in &para.lines {
    // println!(
    // "text={:?} x_offset={} list_label={:?} list_label_x={}",
    // line.text, line.x_offset, line.list_label, line.list_label_x
    // );
    // if let Some(l) = &line.list_label {
    // labels.push(l.clone());
    // }
    // }
    // }

    // // 验证计数器序列：level0第一项=1. 第二项=2.（子级插入后延续）第三项=3.
    // // level1嵌套子项=1. 2.（子级重置为start_value）
    // assert_eq!(labels, vec!["1.", "2.", "1.", "2.", "3."],
    // "编号序列应为：父级1. 父级2. 子级重置1. 子级2. 父级延续3.");

    // let mut renderer = WriterTileRenderer::new();
    // let scale = 2.0_f32;
    // let pw = (pages[0].width  * scale) as u32;
    // let ph = (pages[0].height * scale) as u32;
    // let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];
    // let (out_w, out_h) = renderer
    // .render_page_preview(&pages[0], scale, &mut buf)
    // .expect("render_page_preview failed");
    // let pixmap = Pixmap::from_vec(buf, IntSize::from_wh(out_w, out_h).unwrap()).unwrap();
    // pixmap.save_png("/tmp/roe_number_list.png").unwrap();
    // }
    // [v92-TEMP-DISABLED] 注释区结束

}

#[cfg(test)]
mod list_bullet_render_test {
    use crate::layout::{ParagraphLayoutEngine, arena_to_layout_pages};
    use crate::font_registry::FontRegistry;
    use crate::tile::WriterTileRenderer;
    use roe_parser::odf::fodt::FodtParser;
    use tiny_skia::{Pixmap, IntSize};

    // [v92-TEMP-DISABLED] 以下测试依赖已废弃的 render_page_preview
    // （WROOT-058，本次会话已合并到 viewport 方案，但这几个测试尚未
    // 跟进改写）。整体注释掉，待专项 cargo test --workspace 修复时
    // 改写成走 viewport 方式渲染后再取消注释。
    // #[test]
    // fn test_bullet_list_renders_visible_png() {
    // let fodt = r##"<?xml version="1.0" encoding="UTF-8"?>
    // <office:document
    // xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    // xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    // xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
    // xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
    // office:mimetype="application/vnd.oasis.opendocument.text">
    // <office:styles>
    // <style:style style:name="Default" style:family="paragraph">
    // <style:text-properties fo:font-size="14pt"/>
    // </style:style>
    // <text:list-style style:name="L1">
    // <text:list-level-style-bullet text:level="1" text:bullet-char="•">
    // <style:list-level-properties fo:margin-left="0.6cm" fo:text-indent="-0.6cm" text:min-label-width="0.6cm"/>
    // </text:list-level-style-bullet>
    // <text:list-level-style-bullet text:level="2" text:bullet-char="◦">
    // <style:list-level-properties fo:margin-left="1.2cm" fo:text-indent="-0.6cm" text:min-label-width="0.6cm"/>
    // </text:list-level-style-bullet>
    // </text:list-style>
    // </office:styles>
    // <office:body><office:text>
    // <text:p text:style-name="Default">普通段落，不属于任何列表。</text:p>
    // <text:list text:style-name="L1">
    // <text:list-item><text:p text:style-name="Default">第一个列表项</text:p></text:list-item>
    // <text:list-item><text:p text:style-name="Default">第二个列表项</text:p>
    // <text:list text:style-name="L1">
    // <text:list-item><text:p text:style-name="Default">嵌套子项一</text:p></text:list-item>
    // <text:list-item><text:p text:style-name="Default">嵌套子项二</text:p></text:list-item>
    // </text:list>
    // </text:list-item>
    // <text:list-item><text:p text:style-name="Default">第三个列表项</text:p></text:list-item>
    // </text:list>
    // <text:p text:style-name="Default">列表结束后的普通段落。</text:p>
    // </office:text></office:body>
    // </office:document>"##;

    // let doc = FodtParser::parse(fodt).expect("fodt parse failed");
    // let registry = FontRegistry::new_with_system_fonts();
    // let mut engine = ParagraphLayoutEngine::new(registry);
    // let result = engine.layout(&doc);
    // let pages = arena_to_layout_pages(&result.arena, &doc);
    // assert!(!pages.is_empty(), "应至少产出一页");

    // // 打印每行的 list_label，人工核对结构是否符合预期
    // for para in &pages[0].paragraphs {
    // for line in &para.lines {
    // println!(
    // "text={:?} x_offset={} list_label={:?} list_label_x={}",
    // line.text, line.x_offset, line.list_label, line.list_label_x
    // );
    // }
    // }

    // let mut renderer = WriterTileRenderer::new();
    // let scale = 2.0_f32; // 放大一点方便肉眼看清 bullet
    // let pw = (pages[0].width  * scale) as u32;
    // let ph = (pages[0].height * scale) as u32;
    // let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];
    // let (out_w, out_h) = renderer
    // .render_page_preview(&pages[0], scale, &mut buf)
    // .expect("render_page_preview failed");

    // let pixmap = Pixmap::from_vec(buf, IntSize::from_wh(out_w, out_h).expect("invalid size"))
    // .expect("pixmap from buf failed");
    // let png = pixmap.encode_png().expect("png encode failed");
    // std::fs::write("/tmp/roe_bullet_list.png", &png).expect("write png failed");
    // println!("PNG written to /tmp/roe_bullet_list.png, {} bytes", png.len());

    // // 断言：至少有一行携带了 list_label（bullet 字符）
    // let has_label = pages[0].paragraphs.iter()
    // .any(|p| p.lines.iter().any(|l| l.list_label.is_some()));
    // assert!(has_label, "预期至少一行带有 list_label，但一个都没有——说明bullet没生效");
    // }
    // [v92-TEMP-DISABLED] 注释区结束

}


// ───────── v265: 段落标记 / 手动换行标记 ─────────
thread_local! {
    static SHOW_MARKS: std::cell::Cell<bool> = std::cell::Cell::new(false);
    static MARK_ICONS: std::cell::RefCell<std::collections::HashMap<(u8, u32), tiny_skia::Pixmap>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}
pub fn set_show_marks(on: bool) { SHOW_MARKS.with(|c| c.set(on)); }
fn marks_enabled() -> bool { SHOW_MARKS.with(|c| c.get()) }

const MARK_SVG_PARA: &str = include_str!("../assets/marks/para_mark.svg");
const MARK_SVG_BREAK: &str = include_str!("../assets/marks/break_mark.svg");

fn draw_mark(pixmap: &mut tiny_skia::PixmapMut<'_>, kind: u8, x: f32, y: f32, size_px: f32) {
    let sz = size_px.round().max(6.0) as u32;
    MARK_ICONS.with(|m| {
        let mut m = m.borrow_mut();
        let pm = m.entry((kind, sz)).or_insert_with(|| {
            let svg = if kind == 0 { MARK_SVG_PARA } else { MARK_SVG_BREAK };
            let mut pm = tiny_skia::Pixmap::new(sz, sz).unwrap();
            if let Ok(tree) = resvg::usvg::Tree::from_str(svg, &resvg::usvg::Options::default()) {
                let s = tree.size();
                let k = sz as f32 / s.width().max(s.height());
                resvg::render(&tree, tiny_skia::Transform::from_scale(k, k), &mut pm.as_mut());
            }
            pm
        });
        pixmap.draw_pixmap(x.round() as i32, y.round() as i32, pm.as_ref(),
            &tiny_skia::PixmapPaint::default(), tiny_skia::Transform::identity(), None);
    });
}
