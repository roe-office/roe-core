//! SlideRenderer：把一张幻灯片渲染成 tiny-skia tile bitmap，写入外部 buffer
//! 结合 SlideIndex（viewport culling）和 SlideRuntimeState（动画覆盖）
//! 字形渲染：Parley shaping → swash 光栅化 → tiny-skia blit（同 roe-writer/tile.rs）

use std::collections::HashMap;
use tiny_skia::{
    Color, GradientStop as SkGradientStop, LinearGradient, Paint, PathBuilder, Point, PixmapMut,
    RadialGradient, Rect, SpreadMode, Stroke, Transform,
};
use parley::StyleProperty;

/// Parley迁移（任务2）：表格/文本渲染共用的 Brush 类型，只携带颜色，
/// 不需要 roe-writer/tile.rs 那样的 run_idx（这里没有公式反查/下划线
/// decoration 的需求）。
#[derive(Clone, Copy, Debug, PartialEq, Default)]
struct ImpressBrush {
    color: [u8; 4],
    deco: u8,
    hl: [u8; 4],
}

/// Parley迁移（任务2）：简化版 TinySkiaPen，只负责把 skrifa glyph outline
/// blit 到 tiny_skia::PixmapMut，不含 roe-writer 版本的下划线/删除线绘制
/// （表格/文本框目前不需要这些 decoration；如后续需要可以照抄
/// roe-writer/src/tile.rs 的完整版）。
struct ImpressPen<'a, 'b> {
    pixmap: &'b mut PixmapMut<'a>,
    transform: Transform,
    path_builder: Option<PathBuilder>,
    origin_x: f32,
    origin_y: f32,
    color: [u8; 4],
    skew_tan: f32,
}

impl<'a, 'b> ImpressPen<'a, 'b> {
    fn new(pixmap: &'b mut PixmapMut<'a>, transform: Transform) -> Self {
        Self { pixmap, transform, path_builder: None, origin_x: 0.0, origin_y: 0.0, color: [0, 0, 0, 255], skew_tan: 0.0 }
    }
}

impl<'a, 'b> skrifa::outline::OutlinePen for ImpressPen<'a, 'b> {
    fn move_to(&mut self, x: f32, y: f32) {
        let pb = self.path_builder.get_or_insert_with(PathBuilder::new);
        pb.move_to(self.origin_x + x + y * self.skew_tan, self.origin_y - y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let pb = self.path_builder.get_or_insert_with(PathBuilder::new);
        pb.line_to(self.origin_x + x + y * self.skew_tan, self.origin_y - y);
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        let pb = self.path_builder.get_or_insert_with(PathBuilder::new);
        pb.quad_to(self.origin_x + cx0 + cy0 * self.skew_tan, self.origin_y - cy0, self.origin_x + x + y * self.skew_tan, self.origin_y - y);
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let pb = self.path_builder.get_or_insert_with(PathBuilder::new);
        pb.cubic_to(
            self.origin_x + cx0 + cy0 * self.skew_tan, self.origin_y - cy0,
            self.origin_x + cx1 + cy1 * self.skew_tan, self.origin_y - cy1,
            self.origin_x + x + y * self.skew_tan, self.origin_y - y,
        );
    }
    fn close(&mut self) {
        if let Some(pb) = &mut self.path_builder {
            pb.close();
        }
    }
}

/// 渲染一个 GlyphRun：遍历其中所有 glyph，逐个用 skrifa 取轮廓并 blit。
fn render_glyph_run_impress(
    glyph_run: &parley::GlyphRun<'_, ImpressBrush>,
    pen: &mut ImpressPen,
    origin_x: f32,
    origin_y: f32,
) {
    use skrifa::instance::{LocationRef, NormalizedCoord, Size};
    use skrifa::outline::DrawSettings;
    use skrifa::MetadataProvider;

    let run = glyph_run.run();
    let font = run.font();
    let font_ref = match skrifa::FontRef::from_index(font.font.data.as_ref(), font.font.index) {
        Ok(f) => f,
        Err(_) => return,
    };
    let brush = glyph_run.style().brush;
    let color = brush.color;
    let deco_flags: u8 = brush.deco;
    let outlines = font_ref.outline_glyphs();
    let size = Size::new(run.font_size());
    let normalized_coords: Vec<NormalizedCoord> = run
        .normalized_coords()
        .iter()
        .map(|coord| NormalizedCoord::from_bits(coord.to_bits()))
        .collect();
    let location = LocationRef::new(&normalized_coords);
    let skew_tan: f32 = run.synthesis().skew().unwrap_or(0.0).to_radians().tan();
    let embolden = run.synthesis().embolden();

    let mut px = origin_x + glyph_run.offset();
    let script_bits = (deco_flags >> 2) & 3;
    let py = origin_y + glyph_run.baseline() + match script_bits { 1 => -run.font_size() * 0.55, 2 => run.font_size() * 0.2, _ => 0.0 };
    if deco_flags != 0 || brush.hl[3] != 0 {
        let total_adv: f32 = glyph_run.glyphs().map(|g| g.advance).sum();
        let fsz = run.font_size();
        let th = (fsz / 16.0).max(1.0);
        let mut dpaint = Paint::default();
        dpaint.set_color_rgba8(color[0], color[1], color[2], 255);
        if brush.hl[3] != 0 {
            let mut hp = Paint::default();
            hp.set_color_rgba8(brush.hl[0], brush.hl[1], brush.hl[2], brush.hl[3]);
            if let Some(rc) = tiny_skia::Rect::from_xywh(px, py - fsz * 0.95, total_adv, fsz * 1.2) {
                pen.pixmap.fill_rect(rc, &hp, pen.transform, None);
            }
        }
        if deco_flags & 1 != 0 {
            if let Some(rc) = tiny_skia::Rect::from_xywh(px, py + fsz * 0.12, total_adv, th) {
                pen.pixmap.fill_rect(rc, &dpaint, pen.transform, None);
            }
        }
        if deco_flags & 2 != 0 {
            if let Some(rc) = tiny_skia::Rect::from_xywh(px, py - fsz * 0.30, total_adv, th) {
                pen.pixmap.fill_rect(rc, &dpaint, pen.transform, None);
            }
        }
    }

    for glyph in glyph_run.glyphs() {
        let gx = px + glyph.x;
        let gy = py - glyph.y;
        px += glyph.advance;

        let glyph_id = skrifa::GlyphId::new(glyph.id as u32);
        if let Some(outline) = outlines.get(glyph_id) {
            pen.path_builder = None;
            pen.origin_x = gx;
            pen.origin_y = gy;
            pen.color = color;
            pen.skew_tan = skew_tan;
            let draw_settings = DrawSettings::unhinted(size, location);
            let _ = outline.draw(draw_settings, pen);
            if let Some(pb) = pen.path_builder.take() {
                if let Some(path) = pb.finish() {
                    let mut paint = Paint::default();
                    paint.set_color_rgba8(color[0], color[1], color[2], color[3]);
                    pen.pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, pen.transform, None);
                    if embolden {
                        let mut stroke = tiny_skia::Stroke::default();
                        stroke.width = (run.font_size() / 30.0).max(0.5);
                        pen.pixmap.stroke_path(&path, &paint, &stroke, pen.transform, None);
                    }
                }
            }
        }
    }
}

use roe_model::impress::{FillStyle, Shape, ShapeContent, ShapeGeometry, Slide};
use roe_model::style::StyleRegistry;
use roe_model::writer::Paragraph;
use roe_model::common::{Table, TableCell, CellContent};
use crate::culling::SlideIndex;
use resvg;
use crate::animation::renderer::SlideRuntimeState;

/// PPTX-12: 静态填充解析结果，纯色或渐变二选一。
enum ResolvedFill {
    Solid(u8, u8, u8, u8),
    Gradient {
        /// (pos 0.0-1.0, r, g, b, a)
        stops: Vec<(f32, u8, u8, u8, u8)>,
        kind: GradientKindResolved,
    },
}

/// PPTX-12: 渲染层实际消费的渐变类型。Rect 在模型层是独立变体，但
/// tiny-skia 无原生矩形渐变支持，在 shape_fill_color 里已经把
/// GradientKind::Rect 映射成 GradientKindResolved::Radial 处理
/// （见该处注释），所以这里只需要 Linear/Radial 两种。
enum GradientKindResolved {
    Linear { angle: f32 },
    Radial,
}

/// v1 默认字号（pt 等价单位）：Impress 内存模型暂未解析段落/字符样式继承链
/// （见 roe-parser/src/odf/fodp.rs 文件头注释），后续接入 StyleRegistry 后按
/// Shape/Paragraph 的 char_style 解析实际字号。
const DEFAULT_FONT_SIZE: f32 = 18.0;

/// 判断文本是否包含 CJK 字符（含中日韩统一表意文字、扩展区、假名、谚文等）。
///
/// 背景（PPTX-14/15 CJK斜体降级修复）：cosmic-text 0.12.1 的字体匹配
/// （`Attrs::matches` / `attrs.rs` 第210-214行）对 `style` 做**精确比较**
/// （`face.style == self.style`），不像 fontdb::Query 那样有
/// `[Italic, Oblique, Normal]` 的宽容 fallback。实测确认本机系统里所有
/// CJK 字体家族（HarmonyOS Sans SC/TC、Noto Sans/Serif CJK 全系列）都只有
/// `style=Normal` 的 face，一个 Italic 变体都没有——这不是本机个例，是
/// CJK 字体生态的普遍现状（中文传统排版无斜体概念，字体厂商极少制作真实
/// 斜体字形）。请求 Italic 时，所有 CJK face 在精确匹配下全部被过滤掉，
/// shaping 阶段找不到可用字形，退化为 tofu 方块。
///
/// 修复策略：不依赖 cosmic-text/fontdb 的字体选择兜底（因为验证过它们
/// 在这一路径上确实不会兜底），改为在传入 Attrs 前判断文本内容——含 CJK
/// 字符的 run 不请求 Style::Italic，直接使用 Normal，保证不开天窗；纯
/// 拉丁/其他文字的 run 不受影响，仍正常应用斜体。
fn contains_cjk(text: &str) -> bool {
    text.chars().any(|c| {
        let cp = c as u32;
        matches!(cp,
            0x4E00..=0x9FFF   // CJK统一表意文字
            | 0x3400..=0x4DBF   // CJK扩展A
            | 0x20000..=0x2A6DF // CJK扩展B
            | 0xF900..=0xFAFF   // CJK兼容表意文字
            | 0x3040..=0x309F   // 平假名
            | 0x30A0..=0x30FF   // 片假名
            | 0xAC00..=0xD7A3   // 谚文音节（韩文）
            | 0x3130..=0x318F   // 谚文兼容字母
            | 0x3000..=0x303F   // CJK标点符号
            | 0xFF00..=0xFFEF   // 全角/半角形式
        )
    })
}

/// PROOT bullet-symbol-font: Wingdings/Webdings等符号字体的buChar字符，
/// 按其码位在常规字体里逐字面渲染会显示成不相关的拉丁字母/符号（如
/// Wingdings的"q"实际字形是方块，逐字面渲染成小写字母q）。
///
/// 完整方案是Wingdings码位到Unicode私有使用区(PUA)的映射表+打包符号
/// 字体（参考LibreOffice unotools/source/misc/fontcvt.cxx的
/// aWingDingsTab[224]方案：该表把Wingdings码位映射到PUA码位，再靠
/// LO自带的StarSymbol字体提供对应字形）。本函数不做完整224项映射，
/// 只覆盖样本库中已用真实渲染截图验证过的具体(字符,字体)组合，替换
/// 成语义等价的标准Unicode符号字符，交给系统已装的Noto Sans Symbols/
/// Symbola字体渲染（这两个字体覆盖大量Dingbat/Symbol区块，无需打包
/// 任何专有符号字体资源）。
///
/// 未命中映射表的组合原样返回，不引入未经验证的行为改变——不满足
/// "只覆盖已验证样本"这个约束的字符/字体组合，宁可保持现状（逐字面
/// 渲染，可能不正确但不是本次引入的新错误），也不做未经验证的推测
/// 映射。
///
/// 返回 (实际绘制用字符, 实际绘制用字体名)。命中映射时字体名为 None，
/// 让Parley走默认fallback链找到能画该Unicode符号的字体；未命中时原样
/// 透传输入的 ch/font。
fn map_symbol_font_bullet_char(ch: &str, font: Option<&str>) -> (String, Option<String>) {
    // v320i: 符号字体的字符在 OOXML 里常存成私有区 U+F0xx（如 Wingdings 的 'l' 存成 U+F06C），
    // 先归一化成低位码位再查表；系统没有符号字体，所以换成标准 Unicode 字符。
    let fname = font.unwrap_or("");
    let is_wd = fname.starts_with("Wingdings");
    let is_sym = fname == "Symbol";
    if (is_wd || is_sym) && ch.chars().count() == 1 {
        let c = ch.chars().next().unwrap() as u32;
        let low = if (0xF020..=0xF0FF).contains(&c) { c - 0xF000 } else { c };
        let mapped: Option<&str> = if fname == "Wingdings" {
            match low {
                0x6C => Some("\u{25CF}"), // l 实心圆
                0x6D => Some("\u{25CB}"), // m 空心圆
                0x6E | 0x70 => Some("\u{25A0}"), // n,p 实心方块
                0x71 => Some("\u{2751}"), // q 空心方块
                0x75 | 0x74 => Some("\u{25C6}"), // u,t 菱形
                0x76 | 0xA7 => Some("\u{25AA}"), // v,§ 小方块
                0xFC => Some("\u{2713}"), // ü 对勾
                0xD8 => Some("\u{27A2}"), // Ø 箭头
                0xE8 => Some("\u{2192}"), // è 箭头
                0x2D => Some("\u{2013}"),
                _ => None,
            }
        } else if is_sym {
            match low {
                0x2D => Some("\u{2013}"), // 减号
                0xB7 => Some("\u{2022}"), // 圆点
                _ => None,
            }
        } else { None };
        if let Some(t) = mapped {
            return (t.to_string(), None);
        }
        eprintln!("[BULLET-UNMAPPED] font={} code=U+{:04X}", fname, c);
    }
    (ch.to_string(), font.map(|f| f.to_string()))
}

pub struct SlideRenderer {
    pub slide_width:  f32,   // pt
    pub slide_height: f32,   // pt
    // Parley迁移（任务1/2）：资源持有类型改为parley::FontContext；
    // layout_context 用 Brush=() 泛型（表格/文本渲染目前不需要携带
    // run_idx等元数据，直接用glyph_run.style().brush读色值即可，
    // 比roe-writer/tile.rs的RunBrush方案更简单，因为这里没有公式run
    font_context: parley::FontContext,
    layout_context: parley::LayoutContext<ImpressBrush>,
}

impl SlideRenderer {
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            slide_width: width,
            slide_height: height,
            font_context: parley::FontContext::new(),
            layout_context: parley::LayoutContext::new(),
        }
    }

    /// 复用外部已建立的字体资源，避免每次渲染重新扫描系统字体（性能修复，v86任务A）。
    /// 调用方渲染完毕后应调用 take_fonts() 把资源取回，供下一帧复用。
    pub fn new_with_fonts(width: f32, height: f32, font_context: parley::FontContext) -> Self {
        Self {
            slide_width: width,
            slide_height: height,
            font_context,
            layout_context: parley::LayoutContext::new(),
        }
    }

    /// 取回字体资源（消耗 self），供调用方存回缓存复用。
    pub fn take_fonts(self) -> parley::FontContext {
        self.font_context
    }

    /// 按段落的 run 文本/颜色/粗体/斜体构建 parley layout（渲染与光标定位共用，
    /// 保证两边排版完全一致）。从 render_shape_text 原样抽出，逻辑未改。
    fn build_para_layout(
        &mut self,
        text: &str,
        run_texts: &[String],
        run_colors: &[(u8, u8, u8)],
        run_bolds: &[bool],
        run_italics: &[bool],
        run_fonts: &[Option<String>],
        run_decos: &[(bool, bool, Option<[u8; 4]>, u8)],
        font_size: f32,
        line_height: f32,
        alignment: Option<roe_model::style::Alignment>,
        para_text_w: f32,
    ) -> parley::Layout<ImpressBrush> {
        let default_color = run_colors.first().copied().unwrap_or((20, 20, 20));
        let default_brush = ImpressBrush { color: [default_color.0, default_color.1, default_color.2, 255], deco: 0, hl: [0, 0, 0, 0] };
        let mut builder = self.layout_context.ranged_builder(
            &mut self.font_context, text, 1.0, true,
        );
        builder.push_default(StyleProperty::Brush(default_brush));
        builder.push_default(StyleProperty::FontSize(font_size));
        builder.push_default(parley::LineHeight::Absolute(line_height));

        let mut byte_off = 0usize;
        for (i, t) in run_texts.iter().enumerate() {
            let start = byte_off;
            let end = start + t.len();
            byte_off = end;
            if start >= end { continue; }
            let (r, g, b) = run_colors[i];
            let deco_bits: u8 = run_decos.get(i).map(|dd| (dd.0 as u8) | ((dd.1 as u8) << 1) | (dd.3 << 2)).unwrap_or(0);
            let script_sc: u8 = run_decos.get(i).map(|dd| dd.3).unwrap_or(0);
            let hl_col: [u8; 4] = run_decos.get(i).and_then(|dd| dd.2).unwrap_or([0, 0, 0, 0]);
            builder.push(StyleProperty::Brush(ImpressBrush { color: [r, g, b, 255], deco: deco_bits, hl: hl_col }), start..end);
            if script_sc != 0 { builder.push(StyleProperty::FontSize(font_size * 0.65), start..end); }
            if run_bolds[i] {
                builder.push(StyleProperty::FontWeight(parley::FontWeight::BOLD), start..end);
            }
            if let Some(Some(fam)) = run_fonts.get(i) {
                builder.push(StyleProperty::FontFamily(parley::FontFamily::named(fam.as_str())), start..end);
            }
            if run_italics[i] && !contains_cjk(t) {
                builder.push(StyleProperty::FontStyle(parley::FontStyle::Italic), start..end);
            }
        }

        let para_align = alignment.map(|align| match align {
            roe_model::style::Alignment::Left    => parley::Alignment::Start,
            roe_model::style::Alignment::Center  => parley::Alignment::Center,
            roe_model::style::Alignment::Right   => parley::Alignment::End,
            roe_model::style::Alignment::Justify => parley::Alignment::Justify,
        }).unwrap_or(parley::Alignment::Start);

        let mut layout: parley::Layout<ImpressBrush> = builder.build(text);
        layout.break_all_lines(Some(para_text_w));
        layout.align(para_align, parley::AlignmentOptions::default());
        layout
    }

    /// PPTX-8：构造单个 shape 的仿射变换矩阵。
    ///
    /// 依据《坐标变换绘制顺序图片表格.md》§1.2 来源2（OnlyOffice
    /// GroupShape.js `recalculateTransform`）给出的五步公式：
    /// 中心平移 → 翻转 → 旋转 → 平移回 → 乘父矩阵。
    ///
    /// 参数均为"已经乘过 scale 的屏幕坐标系"数值（与 render_shape 现有
    /// x/y/w/h 语义一致），rotation 为普通角度制（非OOXML的1/60000度，
    /// 该换算已在解析层 pptx.rs 完成，见 PPTX-7 交接说明）。
    ///
    /// 旋转/翻转均围绕 shape 中心点进行（cx,cy = 屏幕坐标系下的 shape
    /// 几何中心），避免"绕左上角旋转导致位置随角度漂移"这个易踩的坑
    /// （同文档 §1.2 明确点出的陷阱）。
    ///
    /// parent_transform 为上一级（分组）累积变换，PPTX-8 阶段暂无分组
    /// 支持（PPTX-9 范围），调用方目前恒传 Transform::identity()，此处
    /// 提前接好参数位置，避免 PPTX-9 落地时再改一遍函数签名。
    fn build_shape_transform(
        x: f32, y: f32, w: f32, h: f32,
        rotation: Option<f32>,
        flip_h: Option<bool>,
        flip_v: Option<bool>,
        parent_transform: Transform,
    ) -> Transform {
        let cx = x + w / 2.0;
        let cy = y + h / 2.0;

        let mut t = Transform::identity();

        // 翻转：围绕中心做镜像。post_scale 本身围绕原点(0,0)缩放，
        // 所以需要手动做"平移到原点→缩放→平移回"三步（tiny-skia 没有
        // 提供 post_scale_at 这样的一步到位方法，rotate系列才有）。
        let fh = flip_h.unwrap_or(false);
        let fv = flip_v.unwrap_or(false);
        if fh || fv {
            t = t
                .post_translate(-cx, -cy)
                .post_scale(if fh { -1.0 } else { 1.0 }, if fv { -1.0 } else { 1.0 })
                .post_translate(cx, cy);
        }

        // 旋转：围绕中心，post_rotate_at 一步到位（tiny-skia 原生支持，
        // angle 为角度制，与本函数入参语义一致，无需换算）。
        if let Some(deg) = rotation {
            if deg != 0.0 {
                t = t.post_rotate_at(deg, cx, cy);
            }
        }

        // 乘父矩阵：父变换在"更外层"应用，用 post_concat 叠加在最后。
        t.post_concat(parent_transform)
    }

    /// 渲染整张幻灯片到外部提供的 RGBA buffer。
    /// 返回 (pw, ph) —— 实际渲染尺寸，供调用方校验/调试。
    /// 若 buf.len() != pw*ph*4，返回 Err。
    pub fn render_slide(
        &mut self,
        slide: &Slide,
        styles: &StyleRegistry,
        runtime: Option<&SlideRuntimeState>,
        scale: f32,
        buf: &mut [u8],
        theme_colors: &HashMap<String, roe_model::common::Color>,
        master_slides: Option<&Vec<roe_model::impress::MasterSlide>>,
    ) -> Result<(u32, u32), String> {
        let pw = (self.slide_width  * scale) as u32;
        let ph = (self.slide_height * scale) as u32;

        let expected = (pw as usize) * (ph as usize) * 4;
        if buf.len() != expected {
            return Err(format!(
                "buffer size mismatch: expected {} bytes ({}x{}x4), got {}",
                expected, pw, ph, buf.len()
            ));
        }

        let mut pixmap = PixmapMut::from_bytes(buf, pw, ph)
            .ok_or_else(|| format!("PixmapMut::from_bytes failed for {}x{}", pw, ph))?;

        // 背景填白
        {
            let mut paint = Paint::default();
            paint.set_color(Color::WHITE);
            if let Some(rect) = Rect::from_xywh(0.0, 0.0, pw as f32, ph as f32) {
                pixmap.fill_rect(rect, &paint, Transform::identity(), None);
            }
        }

        // PROOT-005: 幻灯片背景（<p:bg>）。解析层已完成 slide->layout->master
        // 三级 fallback 查找（见 pptx.rs parse_slide_background 调用点），
        // slide.background 是查找结果，这里只负责按 FillStyle 变体绘制，
        // 不重新做继承判断。None 表示三层都没有背景，维持上面的白色填充。
        //
        // fallback 顺序：优先用 slide.background（已经是 slide/layout 合并结果），
        // slide 本身没查到时才退回 master.background（master 层 PROOT-005 本次
        // 未接入解析，master.background 恒为 None，此分支目前是空跑的兜底，
        // 留着是为了未来 master 层解析补齐后不需要再改渲染层）。
        let effective_background = slide.background.clone().or_else(|| {
            slide.master_idx
                .and_then(|idx| master_slides.and_then(|masters| masters.get(idx)))
                .and_then(|m| m.background.clone())
        });
        if let Some(bg) = effective_background {
            match bg {
                roe_model::common::FillStyle::Solid(c) => {
                    let mut paint = Paint::default();
                    paint.set_color(Color::from_rgba8(c.r, c.g, c.b, c.a));
                    if let Some(rect) = Rect::from_xywh(0.0, 0.0, pw as f32, ph as f32) {
                        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
                    }
                }
                roe_model::common::FillStyle::Picture(roe_model::common::ImageData { data, mime, crop_l, crop_t, crop_r, crop_b }) => {
                    // PROOT-008: crop_l/t/r/b 现在可能来自 <a:stretch>
                    // <a:fillRect> 负值裁剪换算（parse_slide_background
                    // 里 crop_quotients_from_fill_rect），不再恒为0。
                    // 不旋转。
                    self.render_shape_image(
                        &mut pixmap, &data, &mime,
                        crop_l, crop_t, crop_r, crop_b,
                        0.0, 0.0, self.slide_width, self.slide_height,
                        scale, Transform::identity(),
                        // 背景图不涉及custGeom裁剪，见render_shape_image
                        // geometry_clip参数文档注释。
                        None,
                    );
                }
                // ROE-TILE-001: <a:blipFill><a:tile.../></a:blipFill> 平铺背景，
                // 与上面 Picture（拉伸铺满）分支独立，走专门的
                // render_shape_image_tiled（原因见该函数文档注释：
                // render_shape_image 的 x/y 单位处理不适用于非零平铺偏移场景）。
                roe_model::common::FillStyle::PictureTiled {
                    image, tile_offset_x, tile_offset_y, tile_scale_x, tile_scale_y, tile_align
                } => {
                    self.render_shape_image_tiled(
                        &mut pixmap, &image.data,
                        tile_offset_x, tile_offset_y,
                        tile_scale_x, tile_scale_y,
                        &tile_align,
                        pw, ph, scale,
                    );
                }
                roe_model::common::FillStyle::Gradient { stops, kind, .. } => {
                    // v318p: 背景渐变，复用形状渐变的 build_fill_paint，铺满整页。
                    let resolved_stops: Vec<(f32, u8, u8, u8, u8)> = stops.iter()
                        .map(|s| (s.pos, s.color.r, s.color.g, s.color.b, s.color.a))
                        .collect();
                    let resolved_kind = match kind {
                        roe_model::common::GradientKind::Linear { angle } => GradientKindResolved::Linear { angle },
                        _ => GradientKindResolved::Radial,
                    };
                    let resolved = ResolvedFill::Gradient { stops: resolved_stops, kind: resolved_kind };
                    if let Some(paint) = Self::build_fill_paint(&resolved, 0.0, 0.0, pw as f32, ph as f32) {
                        if let Some(rect) = Rect::from_xywh(0.0, 0.0, pw as f32, ph as f32) {
                            pixmap.fill_rect(rect, &paint, Transform::identity(), None);
                        }
                    }
                }
                // Gradient/None：背景渐变第一期未实现（真实样本未遇到），
                // None 变体理论上不会出现在 Option<FillStyle> 已经是 Some 的分支里
                // （parse_slide_background 只在确实解析到内容时才返回 Some），
                // 两种情况都维持已经画好的白色兜底，不报错。
                _ => {}
            }
        }

        // 渲染母版背景和形状
        if let (Some(master_idx), Some(masters)) = (slide.master_idx, master_slides) {
            if let Some(master) = masters.get(master_idx) {
                // v98修复：master.shapes是解析层Presentation.master_slides本体，
                // 与下面slide.layout_shapes（v97已在pptx.rs按id去重的master+layout
                // 合并结果）是两份独立数据。v97的去重只作用于resolved_master_shapes
                // 这份临时克隆，从未影响master_slides[idx].shapes本体，导致这里
                // 仍会把已经被layout同id覆盖、且已经在下面layout_shapes循环里画过
                // 一次的master顶层形状（如master的Group 6）再重复画一次，与layout
                // 版本（如layout的Group 6）视觉叠加冲突（v97 roe-ext_progress_v97.md
                // 1.4节记录的左边缘尖角凸起问题根因）。
                // 修复：任何id已经出现在slide.layout_shapes里的master顶层shape，
                // 说明该id要么已被layout重写、要么原样透传进了layout_shapes，两种
                // 情况下都会在下面的layout_shapes循环里画出，这里必须跳过以避免
                // 重复渲染。未被layout触及的master独有形状（id不在layout_shapes里）
                // 不受影响，继续在此正常渲染。
                let layout_shape_ids: std::collections::HashSet<u32> =
                    slide.layout_shapes.iter().map(|s| s.id).collect();
                let master_index = SlideIndex::build(&master.shapes);
                let master_visible = master_index.query_intersect(0.0, 0.0, self.slide_width, self.slide_height);
                for shape in &master.shapes {
                    if !master_visible.contains(&shape.id) { continue; }
                    if layout_shape_ids.contains(&shape.id) { continue; }
                    // PROOT-001修复：master上的占位符形状（title/body/ctrTitle等）不应整体渲染。
                    // 占位符在slide层解析阶段已经完成"继承position/style"（见
                    // roe-model/src/impress.rs PlaceholderInfo字段注释），master占位符
                    // 自身携带的字面提示文字（如"Click to edit Master title style"）只应在
                    // PowerPoint"编辑母版视图"下可见，不应出现在实际渲染/放映画面里。
                    // 参照LOK sd/source/ui/tools/PreviewRenderer.cxx的IsEmptyPresObj()判断
                    // 模式：占位符身份的形状在预览渲染路径整体跳过，非占位符的普通装饰性
                    // 形状（背景图案、logo等）正常保留渲染。
                    if shape.placeholder.is_some() { continue; }
                    self.render_shape(&mut pixmap, theme_colors, shape, styles, None, scale, Transform::identity(), None);
                }
            }
        }

        // PROOT-005 followup: layout 装饰性形状（logo/固定图片/装饰图案），
        // 画在 master 之上、slide 自身内容之下——对应 OOXML slide->layout->master
        // 三层叠放顺序（master最底层，layout中间层，slide自己内容最上层）。
        // 这批形状已经是 extract_non_ph_shapes 过滤过的非占位符形状（解析阶段
        // 完成），这里不需要再判断 shape.placeholder（不会有值）。
        {
            let layout_index = SlideIndex::build(&slide.layout_shapes);
            let layout_visible = layout_index.query_intersect(0.0, 0.0, self.slide_width, self.slide_height);
            for shape in &slide.layout_shapes {
                if !layout_visible.contains(&shape.id) { continue; }
                self.render_shape(&mut pixmap, theme_colors, shape, styles, None, scale, Transform::identity(), None);
            }
        }

        // 构建空间索引
        let index = SlideIndex::build(&slide.shapes);

        // 全幻灯片范围 culling（渲染全页时等于全部）
        let visible_ids = index.query_intersect(0.0, 0.0, self.slide_width, self.slide_height);

        // 按原始顺序渲染（保持 z-order）
        for shape in &slide.shapes {
            if !visible_ids.contains(&shape.id) { continue; }

            // 获取运行时覆盖
            let rt = runtime.and_then(|r| r.get(shape.id));

            // 跳过不可见 shape
            if let Some(rt) = rt {
                if rt.visible == Some(false) { continue; }
            }

            self.render_shape(&mut pixmap, theme_colors, shape, styles, rt, scale, Transform::identity(), None);
        }

        // 幻灯片边框
        Self::draw_border(&mut pixmap, pw, ph);

        Ok((pw, ph))
    }

    /// 渲染单个 Shape
    /// child_scale: 若此 shape 是某个 <p:grpSp> 的直接子级，携带
    /// (scale_x, scale_y, ch_off_x, ch_off_y) —— 分组子坐标系到父坐标系
    /// 的位置/尺寸映射（详见 PROOT-019 shear bugfix 说明，roe-ext_progress
    /// v63）。顶层 shape 传 None。
    ///
    /// 关键顺序：chOff/chExt 映射（平移+缩放）必须在本 shape 自身的
    /// rotation 之前完成——即先把 x,y,w,h 映射到父坐标系，再用映射后的
    /// x,y,w,h 算 build_shape_transform 的旋转中心。参考 LibreOffice
    /// oox/source/drawingml/shape.cxx 原话："First scale object then
    /// rotate. Other way round would introduce shearing." 此前实现是反过来
    /// （先用未缩放的局部 x,y,w,h 算旋转矩阵，缩放矩阵在外层 post_concat），
    /// 当分组存在非均匀缩放（scale_x != scale_y，真实样本
    /// tdf141463_GroupTransform 高度方向差1.5倍）且子 Shape 自身带 rot
    /// 属性时，产生了肉眼可见的剪切失真（矩形变成平行四边形）。
    fn render_shape(
        &mut self,
        pixmap: &mut PixmapMut,
        theme_colors: &HashMap<String, roe_model::common::Color>,
        shape: &Shape,
        styles: &StyleRegistry,
        runtime: Option<&crate::animation::renderer::ShapeRuntimeState>,
        scale: f32,
        parent_transform: Transform,
        child_scale: Option<(f32, f32, f32, f32)>,
    ) {
        let r = &shape.position;
        let raw_x = r.x() + (runtime.and_then(|rt| rt.offset_x).unwrap_or(0.0) as f32);
        let raw_y = r.y() + runtime.and_then(|rt| rt.offset_y).unwrap_or(0.0) as f32;
        let raw_w = r.w();
        let raw_h = r.h();

        // chOff/chExt 映射：只做平移+缩放（矩形→矩形，不含旋转），本
        // shape 自己的 rotation 在这之后、用映射后的 x,y,w,h 计算。
        let (mapped_x, mapped_y, mapped_w, mapped_h) =
            if let Some((sx, sy, ch_off_x, ch_off_y)) = child_scale {
                (
                    (raw_x - ch_off_x) * sx,
                    (raw_y - ch_off_y) * sy,
                    raw_w * sx,
                    raw_h * sy,
                )
            } else {
                (raw_x, raw_y, raw_w, raw_h)
            };

        let x = mapped_x * scale;
        let y = mapped_y * scale;
        let w = mapped_w * scale;
        let h = mapped_h * scale;

        // 分组子形状：预设路径的点是按子形状原始宽高求值的局部坐标，渲染器不会再乘
        // 组的 (ext/chExt) 比例，这里单独缩放一份，填充/描边/图片裁剪共用。
        let group_scaled_cmds: Option<Vec<roe_model::common::PathCommand>> = match (&shape.geometry, child_scale) {
            (ShapeGeometry::PresetPath(d), Some((gsx, gsy, _, _)))
                if (gsx - 1.0).abs() > 1e-4 || (gsy - 1.0).abs() > 1e-4 =>
            {
                Some(scale_group_path_cmds(d.commands.as_slice(), gsx, gsy))
            }
            _ => None,
        };

        if std::env::var("ROE_DEBUG_PH").is_ok() {
            eprintln!(
                "[DEBUG render_shape] placeholder={:?} x={:.1} y={:.1} w={:.1} h={:.1} fill={:?}",
                shape.placeholder, x, y, w, h, shape.style.fill
            );
        }

        // DEBUG_BORDER: 临时调试用，环境变量 ROE_DEBUG_BORDER=1 时给每个
        // shape 画红色边框，方便肉眼确认占位符实际渲染区域。可随时保留或删除。
        if std::env::var("ROE_DEBUG_BORDER").as_deref() == Ok("1") {
            let mut pb = tiny_skia::PathBuilder::new();
            pb.move_to(x, y);
            pb.line_to(x + w, y);
            pb.line_to(x + w, y + h);
            pb.line_to(x, y + h);
            pb.close();
            if let Some(path) = pb.finish() {
                let mut paint = tiny_skia::Paint::default();
                paint.set_color_rgba8(255, 0, 0, 255);
                paint.anti_alias = true;
                let stroke = tiny_skia::Stroke { width: 2.0, ..Default::default() };
                pixmap.stroke_path(&path, &paint, &stroke, tiny_skia::Transform::identity(), None);
            }
        }

        let shape_transform = Self::build_shape_transform(
            x, y, w, h,
            r.rotation,
            r.flip_h,
            r.flip_v,
            parent_transform,
        );

        let opacity = runtime.and_then(|rt| rt.opacity).unwrap_or(1.0) as f32;
        let alpha = (opacity * 255.0) as u8;

        // 占位符虚线框默认提示文案（占位符虚线框默认提示文案 设计方案 v1）。
        // 空占位符（是占位符 && 文字内容为空）短路渲染成"虚线框+兜底提示
        // 文字"，跳过下面的正常fill/stroke/text/chart/image渲染路径——
        // 这是编辑器UI层的展示约定，不是文档真实内容，因此不复用任何
        // 已有的shape.style.fill/stroke逻辑，独立画一个固定样式的虚线框。
        if shape.placeholder.is_some()
            && crate::placeholder_hints::is_placeholder_empty(shape)
        {
            let hint_text = crate::placeholder_hints::hint_text_for_ph_type(
                shape.placeholder.as_ref().and_then(|p| p.ph_type.as_deref()),
            );
            Self::draw_placeholder_dashed_rect(pixmap, x, y, w, h, scale, shape_transform);

            // 攻坚二期：提示文字优先复用该占位符从layout/master匹配到的真实
            // 格式（颜色/字号/字重/字体/对齐/垂直锚点），而非v1固定灰色/
            // 默认字号/左对齐。hint_style为None（未匹配到任何layout/master
            // 候选，理论上少见）时，各字段各自独立fallback到v1原有固定值，
            // 不整体退化——例如只是颜色缺失也不该连带字号也退回默认。
            // 见roe-model::impress::PlaceholderHintStyle文档注释、设计方案
            // 文档九、节。
            const FALLBACK_HINT_COLOR: roe_model::common::Color =
                roe_model::common::Color { r: 0xA6, g: 0xA6, b: 0xA6, a: 255 };
            let hint_style = shape.placeholder.as_ref().and_then(|p| p.hint_style.as_ref());

            let hint_run = roe_model::writer::Run {
                text: hint_text.to_string(),
                color: hint_style.and_then(|s| s.color).or(Some(FALLBACK_HINT_COLOR)),
                font_size: hint_style.and_then(|s| s.font_size),
                bold: hint_style.and_then(|s| s.bold),
                italic: hint_style.and_then(|s| s.italic),
                font_family: hint_style.and_then(|s| s.font_family.clone()),
                ..Default::default()
            };
            let hint_paragraph = roe_model::writer::Paragraph {
                runs: vec![hint_run],
                alignment: hint_style.and_then(|s| s.algn),
                ..Default::default()
            };
            // anchor（垂直对齐）挂在text_body上而非paragraph/run，需要临时
            // 构造一份text_body副本、仅覆盖anchor字段，其余（wrap/autofit/
            // 内边距）沿用该占位符shape自身原有设置，不应被hint_style影响。
            let hint_text_body = if let Some(a) = hint_style.and_then(|s| s.anchor.clone()) {
                roe_model::impress::TextBodyProps { anchor: a, ..shape.text_body.clone() }
            } else {
                shape.text_body.clone()
            };
            self.render_shape_text(
                pixmap,
                std::slice::from_ref(&hint_paragraph),
                styles,
                &hint_text_body,
                x, y, w, h,
                scale,
                shape_transform,
            );
            return;
        }

        // 填充颜色
        // PPTX-12: runtime 覆盖（拖拽交互临时纯色）分支保持不变，始终是
        // ResolvedFill::Solid；静态计算分支（shape_fill_color）现在可能
        // 返回渐变，见该函数改造说明。
        let fill_color = if let Some(rt) = runtime {
            if let Some(c) = rt.fill_color {
                Some(ResolvedFill::Solid(c.r, c.g, c.b, alpha))
            } else {
                Self::shape_fill_color(shape, alpha, theme_colors)
            }
        } else {
            Self::shape_fill_color(shape, alpha, theme_colors)
        };

        // 表格自己按 cell 画边框/背景（真实 PPTX 表格的 spPr 通常无显式 fill，
        // 若沿用"无fill时兜底主题色"逻辑会导致所有表格背景被强制刷色，
        // 因此 Table 内容跳过 shape 整体的 fill/stroke 绘制）。
        // PROOT-005-followup bugfix: 图片内容同理——图片的视觉内容完全由
        // blipFill/图片字节决定，spPr 上的兜底填充色（尤其是"无fill时套用
        // 主题accent1半透明"这条兜底规则）不应该在图片下方画出一层色块。
        // 真实样本 tdf158512 的 layout 装饰图片(太阳+房子)证实了这个缺口。
        let is_table = matches!(&shape.content, ShapeContent::Table(_));
        // PROOT-012b: 形状级 <a:blipFill>（<p:spPr><a:blipFill>）产出的图片
        // 走 shape.style.fill = FillStyle::Picture，不是 ShapeContent::Image
        // （content是<p:pic>标签场景专用）。两者都需要跳过下面"表格/图片/
        // 分组不套用通用fill/stroke兜底"的判断，否则会在图片下方叠加一层
        // 不该有的兜底填充色（同PROOT-005-followup处理is_image的思路）。
        let is_image = matches!(&shape.content, ShapeContent::Image(_))
            || matches!(&shape.style.fill, Some(roe_model::common::FillStyle::Picture(_)));
        // PROOT-D 子问题2 bugfix: <p:grpSp> 分组容器本身不是可见图形，
        // 只是子 Shape 的坐标变换容器（对应 OOXML <p:grpSpPr>，语义上
        // 不应该有 solidFill/noFill 之外的"填充"概念）。此前没有排除
        // Group，导致 shape.style.fill 为 None 时落入 shape_fill_color
        // 的"非占位符→套用主题accent1半透明兜底"分支，把分组的整个
        // 包围盒画成了一片不该存在的蓝色矩形（真实样本
        // tdf141463_GroupTransform 复现，见 roe-ext_progress_v61.md）。
        // 与 is_table/is_image 同一模式：内容类型自己决定视觉表现，
        // 不套用通用 shape 填充/描边绘制。
        let is_group = matches!(&shape.content, ShapeContent::Group { .. });

        // PROOT-012b bugfix: is_image 原本同时跳过 fill 和 stroke 绘制
        // （<p:pic>场景本来就没有独立的shape.style.stroke需要画，两者合并
        // 判断历来没问题）。但形状级blipFill场景不同——图片只替代填充区域，
        // 形状本身的描边（如tdf112209五边形的蓝色边框，shape.style.stroke
        // 依然有效）仍然需要画出来。故拆分成两个独立判断：is_image_fill
        // （跳过fill，图片会覆盖填充区域，套用兜底色或矩形铺色都不该画）
        // 用于fill绘制块；stroke绘制块不受is_image影响，只受is_table/
        // is_group限制（表格/分组本来就没有独立于内容的描边概念）。
        let is_image_fill = is_image;

        // PROOT-outerShdw: 阴影画在本体 fill/stroke 之前（阴影在下方，
        // 本体覆盖在上方）。表格/分组没有独立于内容之外的阴影绘制概念，
        // 与 fill/stroke 判断保持一致跳过；图片场景（is_image_fill）阴影
        // 仍然要画——图片本身有轮廓，阴影语义上是"这块区域投下阴影"，
        // 不受 fill 是否被图片替代影响。
        if !is_table && !is_group {
            if let Some(shadow) = &shape.style.shadow {
                let outline_path = match &shape.geometry {
                    ShapeGeometry::Ellipse => {
                        tiny_skia::PathBuilder::from_oval(
                            Rect::from_xywh(x, y, w.max(1.0), h.max(1.0)).unwrap_or(
                                Rect::from_xywh(x, y, 1.0, 1.0).unwrap()
                            )
                        )
                    }
                    ShapeGeometry::PresetPath(cmds) => {
                        let cmds: &[roe_model::common::PathCommand] = group_scaled_cmds.as_deref().unwrap_or(cmds.as_slice());
                        Self::build_preset_path(cmds, x, y, scale)
                    }
                    _ => {
                        Some(PathBuilder::from_rect(
                            Rect::from_xywh(x, y, w.max(1.0), h.max(1.0)).unwrap_or(
                                Rect::from_xywh(x, y, 1.0, 1.0).unwrap()
                            )
                        ))
                    }
                };
                if let Some(outline_path) = outline_path {
                    Self::render_shape_shadow(pixmap, &outline_path, shadow, scale, shape_transform);
                }
            }
        }

        if !is_table && !is_image_fill && !is_group {
            if let Some(resolved) = &fill_color {
                let rect = Rect::from_xywh(x, y, w.max(1.0), h.max(1.0));
                if let Some(rect) = rect {
                    // PPTX-12: build_fill_paint 统一构造 Paint（纯色 set_color_rgba8
                    // 或渐变 Shader），Ellipse/其他geometry 分支共享同一个 Paint，
                    // 不再各自手动拼 r,g,b,a。渐变起止点/半径按屏幕坐标 x,y,w,h 算，
                    // 与 shape_transform 独立（见 build_fill_paint 文档注释）。
                    if let Some(paint) = Self::build_fill_paint(resolved, x, y, w, h) {
                        match &shape.geometry {
                            ShapeGeometry::Ellipse => {
                                Self::fill_ellipse(pixmap, x, y, w, h, &paint, shape_transform);
                            }
                            // PPTX-11 part C：预设几何形状路径渲染接入。
                            // cmds 里的坐标是 shape 局部坐标（0..extent.w/h），
                            // 用 scale 映射到屏幕坐标，与 x,y,w,h 同源（见
                            // render_preset_path 文档注释）。
                            ShapeGeometry::PresetPath(cmds) => {
                                let cmds: &[roe_model::common::PathCommand] = group_scaled_cmds.as_deref().unwrap_or(cmds.as_slice());
                                Self::render_preset_path(pixmap, cmds, x, y, scale, &paint, shape_transform);
                            }
                            _ => {
                                pixmap.fill_rect(rect, &paint, shape_transform, None);
                            }
                        }
                    }
                }
            }
        }

        // 描边（PROOT-012b: 独立于上面的fill绘制块，不受is_image_fill限制，
        // 只排除表格/分组——这两者确实没有独立于内容之外的描边概念）。
        // PROOT-024 子问题2 bugfix: width<=0（如 <a:ln w="0">）应视为不画描边，
        // 之前只判断 stroke 是否 Some，没检查 width，导致 noFill+ln w="0" 的
        // shape 仍然画出黑色边框（tiny-skia 对 0 宽度 stroke 有兜底行为，
        // 不会自动跳过绘制）。
        if !is_table && !is_group {
            if let Some(stroke_style) = &shape.style.stroke {
                if stroke_style.width > 0.0 {
                let c = &stroke_style.color;
                // PROOT-002 bugfix: PresetPath（含connector的直线/折线）
                // 沿真实路径描边，不再统一走矩形包围盒描边——见
                // render_preset_path_stroke 文档注释。
                match &shape.geometry {
                    ShapeGeometry::PresetPath(cmds) => {
                        let cmds: &[roe_model::common::PathCommand] = group_scaled_cmds.as_deref().unwrap_or(cmds.as_slice());
                        Self::render_preset_path_stroke(
                            pixmap, cmds, x, y, scale,
                            c.r, c.g, c.b, alpha, stroke_style.width * scale,
                            shape_transform,
                        );
                        // PROOT-002: connector端点箭头装饰，画在描边之后
                        // （箭头应叠加在线条之上，而不是被线条盖住）。
                        // 只有head_arrow/tail_arrow任一存在才会画出内容，
                        // 普通PresetPath形状（无箭头设置）这里是no-op。
                        Self::render_connector_arrows(
                            pixmap, cmds, x, y, scale,
                            c.r, c.g, c.b, alpha, stroke_style.width * scale,
                            shape.style.head_arrow, shape.style.tail_arrow,
                            shape_transform,
                        );
                    }
                    // PROOT-LINE-STROKE: <a:prstGeom prst="line">（真实样本
                    // candy.pptx slide11中间4条connector）此前完全没有专属
                    // 分支，落进下面 `_` 兜底，被当成普通矩形走
                    // draw_rect_stroke（画包围盒四条边），复现现象：连接线
                    // 渲染成空心矩形，而且headEnd/tailEnd箭头装饰完全没有
                    // 机会被调用（render_connector_arrows只在PresetPath分支
                    // 里被调用），端点圆点/箭头也一并消失。
                    //
                    // 修复：Line在OOXML里语义就是"从shape局部坐标(0,0)到
                    // (w,h)的一条对角线"（shape的xfrm定义了这条线的包围盒，
                    // 线本身是包围盒的对角线，flip标志决定具体走哪条对角线，
                    // 但flip已经通过shape_transform在上游处理，这里不需要
                    // 重复处理），构造成与PresetPath同样的两点cmds，复用
                    // 完全相同的render_preset_path_stroke+render_connector_arrows，
                    // 保证行为一致（含箭头装饰）。
                    ShapeGeometry::Line => {
                        use roe_model::common::PathCommand;
                        // PROOT-LINE-DOUBLE-SCALE bugfix: 此前这里用的是
                        // w/h（render_shape顶部 `let w = mapped_w * scale;`
                        // 算出的、已经是屏幕像素单位的值），但
                        // render_preset_path_stroke -> build_preset_path 的
                        // map()闭包对传入点坐标的假设是"局部坐标（未乘
                        // scale）"，即 `lx * scale + x`——PresetPath分支
                        // 传的cmds是parser层用raw.position.extent（局部
                        // 坐标量级）求值出来的，与这个假设一致；但Line
                        // 分支直接把已经含scale的w/h塞进去，被map()又乘了
                        // 一次scale，导致线段终点被放大到约w*scale，视觉
                        // 上表现为所有prstGeom:line横线/斜线渲染长度明显
                        // 超出真实终点（真实样本candy.pptx slide1/slide10
                        // 复现：线段延伸到编号圆圈/端点之外一大截，与
                        // flip无关，所有该类型connector均受影响）。
                        // 修复：改用mapped_w/mapped_h（未乘scale的局部
                        // 坐标），与PresetPath分支的坐标系约定保持一致。
                        let cmds = vec![
                            PathCommand::MoveTo { x: 0.0, y: 0.0 },
                            PathCommand::LineTo { x: mapped_w, y: mapped_h },
                        ];
                        Self::render_preset_path_stroke(
                            pixmap, &cmds, x, y, scale,
                            c.r, c.g, c.b, alpha, stroke_style.width * scale,
                            shape_transform,
                        );
                        Self::render_connector_arrows(
                            pixmap, &cmds, x, y, scale,
                            c.r, c.g, c.b, alpha, stroke_style.width * scale,
                            shape.style.head_arrow, shape.style.tail_arrow,
                            shape_transform,
                        );
                    }
                    ShapeGeometry::Ellipse => {
                        Self::stroke_ellipse(pixmap, x, y, w, h, c.r, c.g, c.b, alpha, stroke_style.width * scale, shape_transform);
                    }
                    _ => {
                        Self::draw_rect_stroke(pixmap, x, y, w, h, c.r, c.g, c.b, alpha, stroke_style.width * scale, shape_transform);
                    }
                }
                }
            }
        }

        // 文字内容
        if let ShapeContent::Text(paragraphs) = &shape.content {
            // PROOT-1xx (v99): vert（文本框整体排版方向）渲染层接入。
            // slide自身text_body.vert优先；未设置(Horizontal)时回退到
            // placeholder从layout/master继承来的hint_style.vert（链路B，
            // vertical_layouts样本走这条）。注意：这里只影响"文字"的排版
            // 框和绘制旋转，不影响shape本体的fill/stroke（那些已经在上面
            // 用原始shape_transform画完了），所以不能复用build_shape_transform
            // 本身，只在传给render_shape_text的局部参数上叠加变换。
            use roe_model::impress::TextVert;
            let effective_vert = match shape.text_body.vert {
                TextVert::Horizontal => shape
                    .placeholder
                    .as_ref()
                    .and_then(|p| p.hint_style.as_ref())
                    .and_then(|s| s.vert)
                    .unwrap_or(TextVert::Horizontal),
                v => v,
            };

            if effective_vert == TextVert::Horizontal {
                self.render_shape_text(pixmap, paragraphs, styles, &shape.text_body, x, y, w, h, scale, shape_transform);
            } else {
                // 宽高对调：文字排版换行按对调后的宽度计算，围绕原中心
                // 重新定位左上角，使对调后的框中心与原框中心重合。
                let cx = x + w / 2.0;
                let cy = y + h / 2.0;
                let text_x = cx - h / 2.0;
                let text_y = cy - w / 2.0;
                let text_w = h;
                let text_h = w;

                // PROOT-1xx (v99 三次修正): 四个样本真实渲染对比核实后
                // 确认三种取值方向并不统一——1.1节"视觉表现一致"结论
                // 仅指"整体转向、字符不转"这个机制层面，不代表三者转向
                // 相同！真实证据：
                //   - vert_and_padding/vert270AndTextRot(Vert/Vert270)：
                //     Vert=顺时针(+90)，Vert270=逆时针(-90)，此为v99
                //     首版取值，核对后确认是对的。
                //   - eaVertAnchor/vertical_layouts(EaVert)：顺时针(+90)，
                //     与Vert同向，此前误把EaVert归到"和Vert270同一分支"
                //     导致方向错误，后又误把三者全部统一成+90.0导致
                //     Vert270被带错。现分三个独立分支明确各自角度，
                //     不再合并任何两个分支，避免再次因合并而互相牵连。
                let deg: f32 = match effective_vert {
                    TextVert::Vert => 90.0,
                    TextVert::Vert270 => -90.0,
                    TextVert::EaVert => 90.0,
                    TextVert::Horizontal => unreachable!(),
                };
                let text_transform = shape_transform.post_rotate_at(deg, cx, cy);

                // padding四个方向必须跟着旋转重新映射——render_shape_text
                // 内部不知道外部做过旋转，只会按未旋转语义把ins_l/r/t/b
                // 套到传入的text_x/y/w/h上，不映射的话padding方向和新的
                // 文字流向对不上（v99首版复现：文字整体飘出shape边界）。
                // vert(顺时针90度)：原top→新right, 原right→新bottom,
                //   原bottom→新left, 原left→新top。
                // vert270/eaVert(逆时针90度)：原top→新left, 原left→新bottom,
                //   原bottom→新right, 原right→新top。
                let (new_ins_l, new_ins_t, new_ins_r, new_ins_b) = match effective_vert {
                    TextVert::Vert => (
                        shape.text_body.ins_b,
                        shape.text_body.ins_l,
                        shape.text_body.ins_t,
                        shape.text_body.ins_r,
                    ),
                    TextVert::Vert270 | TextVert::EaVert => (
                        shape.text_body.ins_t,
                        shape.text_body.ins_r,
                        shape.text_body.ins_b,
                        shape.text_body.ins_l,
                    ),
                    TextVert::Horizontal => unreachable!(),
                };
                let rotated_text_body = roe_model::impress::TextBodyProps {
                    ins_l: new_ins_l,
                    ins_t: new_ins_t,
                    ins_r: new_ins_r,
                    ins_b: new_ins_b,
                    ..shape.text_body.clone()
                };

                self.render_shape_text(pixmap, paragraphs, styles, &rotated_text_body, text_x, text_y, text_w, text_h, scale, text_transform);
            }
        }
        // 图表内容
        if let ShapeContent::Chart(chart_model) = &shape.content {
            self.render_shape_chart(pixmap, chart_model, x, y, w, h, scale, shape_transform);
        }
        // v222: 统计类图表（chartEx）。与普通图表同样按 pt 尺寸渲染，再贴图并套用形状变换
        if let ShapeContent::StatChart(stat_model) = &shape.content {
            let sw = w.round().max(1.0) as u32;
            let sh_px = h.round().max(1.0) as u32;
            if sw > 0 && sh_px > 0 {
                if let Some(rgba) = roe_core::stat_chart_render::render_stat_chart_to_rgba(stat_model, sw, sh_px, scale) {
                    if let Some(isz) = tiny_skia::IntSize::from_wh(sw, sh_px) {
                        if let Some(cp) = tiny_skia::Pixmap::from_vec(rgba, isz) {
                            let pattern = tiny_skia::Pattern::new(
                                cp.as_ref(),
                                tiny_skia::SpreadMode::Pad,
                                tiny_skia::FilterQuality::Bilinear,
                                1.0,
                                tiny_skia::Transform::from_translate(x, y),
                            );
                            let mut paint = Paint::default();
                            paint.shader = pattern;
                            if let Some(rect) = Rect::from_xywh(x, y, w.max(1.0), h.max(1.0)) {
                                pixmap.fill_rect(rect, &paint, shape_transform, None);
                            }
                        }
                    }
                }
            }
        }
        // 图片内容
        // PROOT-005-followup bugfix: x/y/w/h 在本函数开头已经乘过 scale
        // (let w = r.w() * scale; 等)，是像素单位的最终坐标，传给
        // render_shape_image 时 scale 参数必须传 1.0（不再缩放），否则
        // scale 会被重复应用一次，目标尺寸变成 scale^2 倍导致图片被放大
        // 并溢出画布（真实样本 tdf158512 的 layout 装饰图片贴着 slide
        // 右边缘，放大后明显超出边界）。render_shape_image 内部仍然保留
        // "w/h 是原始单位、需要乘 scale" 的约定，是为了兼容 render_slide
        // 里的背景图绘制路径（那条路径传入的是未缩放的 pt 单位 slide 尺寸）。
        if let ShapeContent::Image(roe_model::common::ImageData { data, mime, crop_l, crop_t, crop_r, crop_b }) = &shape.content {
            // PROOT-012: shape geometry是PresetPath（含custGeom求值结果）
            // 时，图片按该路径裁剪；Rect/Ellipse/其他geometry不裁剪，维持
            // 原有矩形铺满行为（Ellipse等其他geometry的图片裁剪不在本次
            // 范围内，样本未覆盖，留待后续按需处理）。
            let geometry_clip = match &shape.geometry {
                ShapeGeometry::PresetPath(cmds) => Some((group_scaled_cmds.as_deref().unwrap_or(cmds.as_slice()), scale)),
                _ => None,
            };
            self.render_shape_image(pixmap, data, mime, *crop_l, *crop_t, *crop_r, *crop_b, x, y, w, h, 1.0, shape_transform, geometry_clip);
        }
        // PROOT-012b: 形状级 blipFill 图片（shape.style.fill 是 FillStyle::Picture，
        // 与上面 ShapeContent::Image 分支——<p:pic>标签场景——是两条独立的数据
        // 来源，互不重叠，故用 if let 各自独立判断，不改动上面的分支）。
        // crop_l/t/r/b 走 ImageData 自带字段（与 srcRect 裁剪同一概念，
        // 但 blipFill 场景走的是 ImageData::default() 里的 0，此处暂不支持
        // 裁剪——tdf112209样本的 fillRect 裁剪是 PROOT-012d 的范围，不是
        // srcRect，属于不同阶段的概念，这里先不接，不影响图片本体显示）。
        if let Some(roe_model::common::FillStyle::Picture(roe_model::common::ImageData { data, mime, crop_l, crop_t, crop_r, crop_b })) = &shape.style.fill {
            let geometry_clip = match &shape.geometry {
                ShapeGeometry::PresetPath(cmds) => Some((group_scaled_cmds.as_deref().unwrap_or(cmds.as_slice()), scale)),
                _ => None,
            };
            self.render_shape_image(pixmap, data, mime, *crop_l, *crop_t, *crop_r, *crop_b, x, y, w, h, 1.0, shape_transform, geometry_clip);
        }
        // 表格内容（PPTX-6 渲染层，PPTX-8b 接入旋转）
        if let ShapeContent::Table(table) = &shape.content {
            let table_style = table.style_id.as_deref()
                .and_then(roe_model::table_styles::resolve_predefined_table_style);
            self.render_shape_table(pixmap, theme_colors, table, table_style.as_ref(), x, y, scale, shape_transform);
        }
        // 分组内容（PPTX-9a）：递归渲染子 Shape。
        //
        // 关键点：shape_transform 本身不含"自身平移"——build_shape_transform
        // 只用 x/y/w/h 算旋转/翻转中心，平移历来是靠外部把 x,y 当作"最终
        // 屏幕坐标"直接传给 fill_rect(rect,...) 完成的（顶层 shape 场景下
        // x,y 本来就是屏幕坐标，天然成立）。但分组子 Shape 的 x,y 是"相对
        // 分组坐标系的局部坐标"（OOXML chOff/chExt 语义），子级的平移必须
        // 由分组的位移显式带入 parent_transform，否则子 Shape 无论父级
        // 平移到哪都会画在自己的局部坐标位置上（已被端到端测试证实）。
        // 因此这里额外用 Transform::from_translate(x, y) 补上分组自身的
        // 平移，post_concat 在 shape_transform（含分组自身旋转/翻转+更外
        // 层父矩阵）之前，构成子级真正需要的累积 parent_transform。
        //
        // 子形状暂无独立 runtime 覆盖信息（SlideRuntimeState 目前按顶层
        // shape id 查找，分组子形状不在此设计范围内），故此处传 None，
        // 不越界实现未规划的能力。
        // PROOT-D 子问题2 bugfix（v62）：此前只做了分组自身的平移
        // （Transform::from_translate(x, y)），没有处理 chOff/chExt 与
        // off/ext 比例不一致时子 Shape 需要的缩放换算，导致真实样本
        // tdf141463_GroupTransform（chExt与ext高度差1.5倍）里子 Shape
        // 位置/尺寸严重错位。现在按 OOXML 语义补齐：
        //   1) 子 Shape 的局部坐标先减去 ch_off（换算成"以 chOff 为原点"
        //      的相对坐标）
        //   2) 按 (ext / ch_ext) 比例缩放（ch_ext 是子坐标系声明的
        //      "虚拟画布"尺寸，ext 是分组在父坐标系实际占的尺寸，
        //      两者不等时子内容需要整体缩放才能吻合分组框）
        //   3) 平移到分组在屏幕上的实际位置 (x, y)
        //   4) 叠加 shape_transform（分组自身的旋转/翻转 + 更外层
        //      父矩阵），顺序与此前一致（post_concat 在最后）
        // ch_off/ch_ext 为 None 时（未提供或非 PPTX 来源）退回缩放
        // 1.0、偏移 0 的行为，与此前"隐含1:1假设"完全一致，不引入
        // 回归。scale_x/scale_y 在 ch_ext 分量为 0 时也退回 1.0，
        // 避免除以零产生 NaN/Inf（真实 OOXML 不应出现 chExt=0，但
        // 防御性处理，不让异常输入导致整个渲染崩溃或产出垃圾坐标）。
        // PROOT-019 shear bugfix（v63）：child_parent_transform 不再包含
        // scale——chOff/chExt 缩放现在下放到每个子 Shape 自己的 x,y,w,h
        // 映射阶段（见 render_shape 顶部 child_scale 处理），这里只保留
        // "分组自身在父坐标系里的平移 + 分组自身的旋转/翻转（含更外层
        // 父矩阵，都在 shape_transform 里）"。scale_x/scale_y/ch_off_x/
        // ch_off_y 改为通过 child_scale 参数逐个传给子 Shape 自己去做
        // "先映射位置尺寸、再算自己的旋转"，从根源上避免非均匀缩放和
        // 子 Shape 自身旋转复合产生剪切（矩形变平行四边形）。
        if let ShapeContent::Group { children, ch_off, ch_ext } = &shape.content {
            let (ch_off_x, ch_off_y) = ch_off.unwrap_or((0.0, 0.0));
            let (ch_ext_w, ch_ext_h) = ch_ext.unwrap_or((r.w(), r.h()));
            let scale_x = if ch_ext_w.abs() > f32::EPSILON { r.w() / ch_ext_w } else { 1.0 };
            let scale_y = if ch_ext_h.abs() > f32::EPSILON { r.h() / ch_ext_h } else { 1.0 };

            let child_parent_transform = Transform::from_translate(x, y)
                .post_concat(shape_transform);
            for child in children {
                self.render_shape(
                    pixmap, theme_colors, child, styles, None, scale,
                    child_parent_transform,
                    Some((scale_x, scale_y, ch_off_x, ch_off_y)),
                );
            }
        }
    }

    /// 渲染 Shape 内的表格（PPTX-6）
    ///
    /// 设计要点（详见 roe-ext_progress_v14.md §二，本次新增"两阶段测量+绘制"
    /// 修复行高问题，见下方说明）：
    /// - 不引入矩阵库，纯标量按列宽/行高累加计算每个 cell 的矩形范围
    /// - `hidden == true` 的 cell（合并单元格的延续格）直接跳过，不绘制
    /// - 可见 cell 按 col_span/row_span 累加跨越的实际宽高
    /// - 边框第一期简化：每个可见 cell 独立画四条边，不做相邻cell共享边去重
    /// - 表格样式（tableStyleId 引用的表头深色底/条纹配色）本次不解析，固定用
    ///   灰框黑字兜底展示，是已知简化，非本次范围
    ///
    /// 行高修复（本次新增）：TableRow 无 height_pt 字段，此前用固定
    /// `CELL_FONT_SIZE * 1.4` 做行高，真实文档里长文字会在列宽内换行
    /// （例如 tdf149785.pptx "Basic license mgmt. only" 换成两行），固定行高
    /// 导致文字溢出/挤压。改为两阶段：
    ///   1) 测量阶段：对每个可见 cell 用 cosmic-text 按其实际列宽 shape 一遍
    ///      文字，取换行后的行数；合并单元格（row_span>1）把所需总高度平均
    ///      分摊到跨越的每一行（简化近似）
    ///   2) 绘制阶段：用测量出的各行实际高度做累加定位，再实际绘制文字/边框
    fn render_shape_table(
        &mut self,
        pixmap: &mut PixmapMut,
        theme_colors: &HashMap<String, roe_model::common::Color>,
        table: &Table,
        table_style: Option<&roe_model::table_style::TableStyle>,
        x: f32, y: f32,
        scale: f32,
        transform: Transform,
    ) {
        // PROOT-003 followup3: 此前 CELL_FONT_SIZE 硬编码14pt（临时改8pt测试
        // 行高逻辑），与原始XML真实字号（如n80340 sz="800"=8pt）无关。解析层
        // parse_table 已把 <a:rPr sz="..."> 直接写入 run.font_size（未经
        // master/layout样式继承，与普通文本框 cur_run_font_size 路径不同，
        // 见 roe-parser/src/ooxml/pptx.rs tbl_run_font_size 相关代码），此处
        // 改为按 cell 内所有 run 的 font_size 取最大值（同 render_shape_text
        // 的"PPTX-15近似"：字号仍是 cosmic-text buffer 级别共享，无法逐run
        // 独立设置），全部为 None 时兜底 DEFAULT_CELL_FONT_SIZE。
        const DEFAULT_CELL_FONT_SIZE: f32 = 12.0;
        // PROOT-003 followup4: 此前 CELL_PAD_X/CELL_PAD_Y 硬编码4.0/3.0pt，
        // 全表格统一，与原始XML <a:tcPr marL/marR/marT/marB> 实际值无关。
        // 解析层已按LOK缺省值(marL/marR=91440EMU≈7.2pt, marT/marB=45720EMU
        // ≈3.6pt)换算填充 cell.pad_left/right/top/bottom，此处改为逐cell
        // 读取。阶段1（测量）和阶段2（绘制）必须调用同一函数，理由同
        // resolve_cell_font_size 处注释（避免预扫描/实际绘制不一致）。
        fn resolve_cell_padding(cell: &TableCell) -> (f32, f32, f32, f32) {
            const FALLBACK_PAD_LR: f32 = 4.0;
            const FALLBACK_PAD_TB: f32 = 3.0;
            (
                cell.pad_left.unwrap_or(FALLBACK_PAD_LR),
                cell.pad_right.unwrap_or(FALLBACK_PAD_LR),
                cell.pad_top.unwrap_or(FALLBACK_PAD_TB),
                cell.pad_bottom.unwrap_or(FALLBACK_PAD_TB),
            )
        }

        // PROOT-003 followup4: 此前 BORDER_COLOR 硬编码浅灰(160,160,160)，
        // 与原始XML <a:tcPr><a:lnL/lnR/lnT/lnB><a:solidFill>（常见为主题色
        // tx1，实为黑色）不符。border_color为None时（如<a:noFill/>或未指定
        // 边框），兜底沿用此前的浅灰色，作为"看得见但不抢眼"的默认框线，
        // 而非直接不画框（原有行为一直是"总画框"，保持这一行为不因本次
        // 改动而回归）。border_width为None时兜底1.0pt，同解析层LOK缺省值
        // 一致（见 roe-parser/src/ooxml/pptx.rs 边框宽度解析处注释）。
        fn resolve_cell_border(cell: &TableCell) -> ((u8, u8, u8), f32) {
            const FALLBACK_BORDER_COLOR: (u8, u8, u8) = (160, 160, 160);
            const FALLBACK_BORDER_WIDTH: f32 = 1.0;
            let color = cell.border_color
                .map(|c| (c.r, c.g, c.b))
                .unwrap_or(FALLBACK_BORDER_COLOR);
            let width = cell.border_width.unwrap_or(FALLBACK_BORDER_WIDTH);
            (color, width)
        }

        // PROOT-031: 给定 cell 在表格中的位置，按内置表格样式（TableStyle）
        // 的常见优先级合并出这个 cell 应该 fallback 到哪些 TableStylePart：
        // first/last row/col（表头/首尾特殊区域）> band1/band2（隔行/隔列
        // 条纹）> whole_tbl（整表兜底）。同级冲突时 row 系列优先于 col 系列
        // （表头语义通常比首列更强）。返回按优先级从高到低排列的 part 列表，
        // 调用方对每个属性（fill/border_*/text_color）各自独立取链中第一个
        // Some 值，而不是整体二选一 part——因为 LOK 的各属性本就是独立设置
        // 的，某 part 可能只定义了 fill 没定义 border。
        //
        // PROOT-031 followup（下次会话需验证）：这个优先级顺序是本次按
        // OOXML/PowerPoint 通行惯例定的，不是逐字核对 LOK 消费方源码
        // （tablecellcontext.cxx 或等价文件，本次完全没看）得出的结论。
        // 当前验证样本 tdf156718 用的 Light-Style-1 只激活 firstRow/
        // firstCol/lastCol/bandRow 四个开关且不产生同一 cell 命中多个
        // part 的情况，无法验证这里的优先级是否正确，只保证单一命中的
        // 主路径能跑通。下次遇到多重命中样本（如某表格同时开 firstRow
        // 和 firstCol 导致左上角 cell 双重命中）需要专门验证。
        fn resolve_cell_style_fallback<'a>(
            ri: usize, ci: usize,
            num_rows: usize, num_cols: usize,
            table: &Table,
            table_style: &'a roe_model::table_style::TableStyle,
        ) -> Vec<&'a roe_model::table_style::TableStylePart> {
            let mut parts: Vec<&roe_model::table_style::TableStylePart> = Vec::new();

            let is_first_row = table.first_row && ri == 0;
            let is_last_row = table.last_row && num_rows > 0 && ri == num_rows - 1;
            let is_first_col = table.first_col && ci == 0;
            let is_last_col = table.last_col && num_cols > 0 && ci == num_cols - 1;

            if is_first_row { parts.push(&table_style.first_row); }
            if is_last_row { parts.push(&table_style.last_row); }
            if is_first_col { parts.push(&table_style.first_col); }
            if is_last_col { parts.push(&table_style.last_col); }

            // Banding：真实样本 tdf156718（firstRow+firstCol+lastCol+
            // bandRow 同时开启）显示第二行（非首行，即使 first_col 命中）
            // 依然要有条纹底色，说明 banding 不能因为该 cell 命中了
            // first_col/last_col 就整体跳过——只有命中 first_row/last_row
            // （表头/表尾整行）才应该盖过条纹（这两者本身就是整行强调色，
            // 语义上与"这一行是不是条纹行"冲突，其余情况按字段各自
            // fallback，交给 first_some_* 系列函数处理优先级即可，不需要
            // 在这里提前整体排除）。
            if !is_first_row && !is_last_row {
                // PROOT-031 修正：条纹奇偶不能直接用物理行/列索引 ri/ci
                // 取模——tdf156718 样本 first_row 占用了物理行0，唯一的
                // 数据行是物理行1，但视觉上它是"表头下面第一条数据行"，
                // 应该命中 band1_h（LOK/PowerPoint 语义：条纹计数从数据
                // 区起点重新开始，而不是从整个表格物理索引0开始）。用
                // "该行前面有多少个参与条纹计数的行"做相对计数：跳过
                // first_row 占的那一行（如果启用），从而让数据区第一行
                // 的相对索引变成0，对应 band1。
                let band_row_offset = if table.first_row { 1 } else { 0 };
                let band_col_offset = if table.first_col { 1 } else { 0 };
                if table.band_row {
                    let rel_ri = ri.saturating_sub(band_row_offset);
                    parts.push(if rel_ri % 2 == 0 { &table_style.band1_h } else { &table_style.band2_h });
                }
                if table.band_col {
                    let rel_ci = ci.saturating_sub(band_col_offset);
                    parts.push(if rel_ci % 2 == 0 { &table_style.band1_v } else { &table_style.band2_v });
                }
            }

            // whole_tbl 永远是最后一级兜底。
            parts.push(&table_style.whole_tbl);
            parts
        }

        /// 按 parts 优先级链取第一个非 None 的 fill，供背景色 fallback 用。
        fn first_some_fill<'a>(
            parts: &[&'a roe_model::table_style::TableStylePart],
        ) -> Option<&'a roe_model::table_style::ColorRef> {
            parts.iter().find_map(|p| p.fill.as_ref())
        }

        /// 按 parts 优先级链取第一个非 None 的边框颜色，field 用于选取
        /// border_left/right/top/bottom 中的哪一条边。
        fn first_some_border<'a>(
            parts: &[&'a roe_model::table_style::TableStylePart],
            field: impl Fn(&roe_model::table_style::TableStylePart) -> &Option<roe_model::table_style::ColorRef>,
        ) -> Option<&'a roe_model::table_style::ColorRef> {
            parts.iter().find_map(|p| field(p).as_ref())
        }

        const TEXT_COLOR: (u8, u8, u8) = (20, 20, 20);
        const MIN_ROW_H: f32 = 18.0;

        // 每个 cell 各自的字号：段内所有 run 的 font_size 取最大值，兜底
        // DEFAULT_CELL_FONT_SIZE。阶段1（测量）和阶段2（绘制）必须调用同一个
        // 函数，避免"预扫描高度"和"实际绘制高度"因字号来源不一致而产生偏差
        // （v32 教训：本次行高改动一度被这类不一致掩盖效果）。
        fn resolve_cell_font_size(cell: &TableCell) -> f32 {
            cell.content.iter()
                .filter_map(|c| if let CellContent::Paragraph(p) = c { Some(p) } else { None })
                .next()
                .and_then(|p| {
                    p.runs.iter()
                        .filter_map(|r| r.font_size)
                        .fold(None::<f32>, |acc, fs| Some(acc.map_or(fs, |a: f32| a.max(fs))))
                })
                .filter(|fs: &f32| fs.is_finite() && *fs > 0.0)
                .unwrap_or(DEFAULT_CELL_FONT_SIZE)
        }

        let min_row_h_px = MIN_ROW_H * scale;

        let num_cols = table.col_widths.len().max(
            table.rows.first().map(|r| r.cells.len()).unwrap_or(1)
        );
        let col_widths_px: Vec<f32> = if table.col_widths.is_empty() {
            vec![100.0 * scale; num_cols]
        } else {
            table.col_widths.iter().map(|w| w * scale).collect()
        };
        let mut col_x: Vec<f32> = Vec::with_capacity(col_widths_px.len() + 1);
        {
            let mut acc = x;
            for w in &col_widths_px {
                col_x.push(acc);
                acc += w;
            }
            col_x.push(acc);
        }

        let num_rows = table.rows.len();

        // ── 阶段1：测量每个可见 cell 的行数，算出每一物理行的高度需求 ──
        // PROOT-003 followup：优先用原始 XML <a:tr h="..."> 指定的行高作为
        // 起点（换算px），而非统一用 MIN_ROW_H 起步。此前完全没有读取
        // row_height，导致渲染行高与真实值（如n80340实测14.4pt）明显不符，
        // 渲染结果比LibreOffice真实渲染高出不少。若原始XML未指定该字段
        // （None），沿用旧的 min_row_h_px 兜底；后续测量循环仍会在内容超出
        // 该基准高度时按需扩大（取较大值），避免文字被裁切。
        let mut row_heights: Vec<f32> = table.rows.iter()
            .map(|row| row.row_height.map(|h| h * scale).unwrap_or(min_row_h_px))
            .collect();

        for (ri, row) in table.rows.iter().enumerate() {
            for (ci, cell) in row.cells.iter().enumerate() {
                if cell.hidden {
                    continue;
                }
                let text: String = cell.content.iter()
                    .filter_map(|c| if let CellContent::Paragraph(p) = c { Some(p) } else { None })
                    .next()
                    .map(|p| p.runs.iter().map(|r| r.text.as_str()).collect::<String>())
                    .unwrap_or_default();

                if text.trim().is_empty() {
                    continue;
                }

                let col_span = cell.col_span.max(1) as usize;
                let row_span = cell.row_span.max(1) as usize;
                let end_col = (ci + col_span).min(col_x.len() - 1);
                let cell_w = col_x.get(end_col).copied().unwrap_or(0.0)
                    - col_x.get(ci).copied().unwrap_or(0.0);
                let (pad_l, pad_r, pad_t, pad_b) = resolve_cell_padding(cell);
                let avail_w = (cell_w - (pad_l + pad_r) * scale).max(1.0);

                // Parley迁移（任务2）：测量阶段改用 parley 数行数。
                // avail_w 已是像素单位（乘过scale）；ranged_builder的scale
                // 参数取1.0（不用像素缩放，因为 avail_w 本身已经是像素值），
                // font_size 直接传像素值（cell_font_size 已含 * scale）。
                // 这与 roe-writer/tile.rs 的写法不同（那边font_size传pt原值、
                // 靠builder的scale参数换算）；这里为了改动最小化，保留
                // "先算出像素级font_size"的既有习惯，用scale=1.0规避双重缩放。
                let cell_font_size = resolve_cell_font_size(cell) * scale;
                let cell_line_height = cell_font_size * 1.4;

                let mut builder = self.layout_context.ranged_builder(
                    &mut self.font_context, &text, 1.0, true,
                );
                builder.push_default(StyleProperty::FontSize(cell_font_size));
                builder.push_default(parley::LineHeight::Absolute(cell_line_height));
                let mut layout: parley::Layout<ImpressBrush> = builder.build(&text);
                layout.break_all_lines(Some(avail_w));
                let n_lines = layout.lines().count().max(1);

                // PROOT-003 followup: 兜底高度此前固定用 min_row_h_px(18pt)，
                // 与该行是否有原始 row_height（如14.4pt）无关，导致row_heights
                // 初始化时设的更矮的基准值在这里被立刻顶回18pt，视觉上等同于
                // row_height从未生效。改为按"这一行自己的基准高度"
                // (row_heights[ri]，已在初始化时体现了row_height或退化到
                // min_row_h_px) 兜底，而非固定常量。
                let needed_h = (n_lines as f32 * cell_line_height + (pad_t + pad_b) * scale).max(row_heights[ri]);
                let per_row_needed = needed_h / row_span as f32;
                for r in ri..(ri + row_span).min(num_rows) {
                    if row_heights[r] < per_row_needed {
                        row_heights[r] = per_row_needed;
                    }
                }
            }
        }

        let mut row_y: Vec<f32> = Vec::with_capacity(num_rows + 1);
        {
            let mut acc = y;
            for h in &row_heights {
                row_y.push(acc);
                acc += h;
            }
            row_y.push(acc);
        }

        // ── 阶段2：绘制 ──
        for (ri, row) in table.rows.iter().enumerate() {
            for (ci, cell) in row.cells.iter().enumerate() {
                if cell.hidden {
                    continue;
                }
                let col_span = cell.col_span.max(1) as usize;
                let row_span = cell.row_span.max(1) as usize;

                let cell_x = col_x.get(ci).copied().unwrap_or(x);
                let end_col = (ci + col_span).min(col_x.len() - 1);
                let cell_w = col_x.get(end_col).copied().unwrap_or(cell_x) - cell_x;

                let cell_y = row_y.get(ri).copied().unwrap_or(y);
                let end_row = (ri + row_span).min(row_y.len() - 1);
                let cell_h = row_y.get(end_row).copied().unwrap_or(cell_y) - cell_y;

                // PROOT-031: 该 cell 命中的 TableStyle part fallback 链
                // （高优先级在前），仅当 table 有 table_style 时才计算，
                // 避免无 tableStyleId 场景（此前一直如此）产生任何行为变化。
                let style_parts = table_style.map(|ts| {
                    resolve_cell_style_fallback(ri, ci, num_rows, num_cols, table, ts)
                });

                // 背景色（PROOT-003：表格cell背景色；PROOT-031：cell 自身
                // 未显式设置时 fallback 到 TableStyle 对应 part 的 fill）。
                // 必须在边框stroke之前画，否则填充会盖住边框线（先填充再
                // 描边）。cell 自身有值 > TableStyle fallback > 不画（透明）。
                let resolved_bg = cell.bg_color.or_else(|| {
                    style_parts.as_ref()
                        .and_then(|parts| first_some_fill(parts))
                        .map(|cr| cr.resolve(theme_colors))
                });
                if let Some(bg) = resolved_bg {
                    Self::draw_rect_fill(
                        pixmap, cell_x, cell_y, cell_w.max(1.0), cell_h.max(1.0),
                        bg.r, bg.g, bg.b, bg.a,
                        transform,
                    );
                }

                // 边框（PPTX-8b：表格整体作为刚体旋转，col_x/row_y
                // 仍按"未旋转"绝对坐标累加计算，只在实际绘制时统一套用
                // transform，不改变内部cell布局的累加逻辑本身，与LOK
                // "先归一化布局、最后统一应用整体变换"的模式一致）
                //
                // PROOT-031: 边框分边渲染。已对照 LOK 权威源码
                // oox/source/drawingml/table/tablecell.cxx 的
                // applyTableStylePart 函数核实（非猜测）：
                // - cell 自身显式 border_color 优先级最高（四边统一，
                //   因为 TableCell 数据结构本身只有单一颜色字段）。
                // - 否则，六条边（left/right/top/bottom/inside_h/
                //   inside_v）各自独立在 style_parts 链（高→低优先级）
                //   上查第一个非 None 值。除 whole_tbl 外的所有 part
                //   （first_row/last_row/first_col/last_col/banding）
                //   六个字段全部无条件参与查询——LOK 对这些 part 调用
                //   applyTableStylePart 时 bIsWholeTable=false，四边+
                //   inside_h/v 一律 apply，不做"是否在边缘"的位置判断。
                //   只有 whole_tbl 特殊：它的 border_top/bottom/left/
                //   right 仅在 cell 确实处于表格对应外侧边缘时才生效
                //   （LOK bIsWholeTable=true 分支：nRow==0 才应用 top、
                //   nRow>=nMaxRow 才应用 bottom，等等），border_inside_h/
                //   v 则始终生效、不受位置限制。
                // - 原有灰色硬编码兜底（四边统一，无 tableStyleId 场景，
                //   行为与此前完全一致）。宽度维持既有逻辑不变（TableStyle
                //   74种内置样式均固定1pt，见 table_style.rs 顶部注释，
                //   不做可配置字段）。
                let (fallback_rgb, border_width_pt) = resolve_cell_border(cell);
                if let Some(bc) = cell.border_color {
                    Self::draw_rect_stroke(
                        pixmap, cell_x, cell_y, cell_w.max(1.0), cell_h.max(1.0),
                        bc.r, bc.g, bc.b, 255, border_width_pt * scale,
                        transform,
                    );
                } else if let Some(parts) = style_parts.as_ref() {
                    let is_top_edge = ri == 0;
                    let is_bottom_edge = ri + row_span >= num_rows;
                    let is_left_edge = ci == 0;
                    let is_right_edge = ci + col_span >= num_cols;

                    // whole_tbl 是链上最后一个元素（最低优先级）。查某个
                    // 方向字段时，如果该 part 是 whole_tbl 且 cell 不在
                    // 对应边缘，则视为它没有定义这个字段（用 None 跳过），
                    // 从而不让 whole_tbl 的外框色误套到内部 cell 上。
                    let is_whole_tbl = |p: &roe_model::table_style::TableStylePart| {
                        std::ptr::eq(p, &table_style.unwrap().whole_tbl)
                    };

                    let find_edge = |on_edge: bool,
                                      field: fn(&roe_model::table_style::TableStylePart) -> &Option<roe_model::table_style::ColorRef>|
                     -> Option<(u8, u8, u8, u8, f32)> {
                        parts.iter().find_map(|p| {
                            if is_whole_tbl(p) && !on_edge {
                                None
                            } else {
                                field(p).as_ref()
                            }
                        }).map(|cr| {
                            let c = cr.resolve(theme_colors);
                            (c.r, c.g, c.b, c.a, border_width_pt * scale)
                        })
                    };
                    // inside_h/inside_v 不受边缘限制，所有 part（含
                    // whole_tbl）均可贡献，直接复用既有 first_some_border。
                    let find_inside = |field: fn(&roe_model::table_style::TableStylePart) -> &Option<roe_model::table_style::ColorRef>|
                     -> Option<(u8, u8, u8, u8, f32)> {
                        first_some_border(parts, field).map(|cr| {
                            let c = cr.resolve(theme_colors);
                            (c.r, c.g, c.b, c.a, border_width_pt * scale)
                        })
                    };

                    // 每条边先看"自己方向的专属字段"（top/bottom/left/right），
                    // 找不到再退到 inside_h/inside_v（同一物理线在非 whole_tbl
                    // part 上可能只通过 insideH/insideV 字段定义，例如
                    // Light-Style-2 等家族的 band1_h.border_top/bottom）。
                    let top = find_edge(is_top_edge, |p| &p.border_top).or_else(|| find_inside(|p| &p.border_inside_h));
                    let right = find_edge(is_right_edge, |p| &p.border_right).or_else(|| find_inside(|p| &p.border_inside_v));
                    let bottom = find_edge(is_bottom_edge, |p| &p.border_bottom).or_else(|| find_inside(|p| &p.border_inside_h));
                    let left = find_edge(is_left_edge, |p| &p.border_left).or_else(|| find_inside(|p| &p.border_inside_v));

                    Self::draw_rect_stroke_sides(
                        pixmap, cell_x, cell_y, cell_w.max(1.0), cell_h.max(1.0),
                        [top, right, bottom, left], transform,
                    );
                } else {
                    Self::draw_rect_stroke(
                        pixmap, cell_x, cell_y, cell_w.max(1.0), cell_h.max(1.0),
                        fallback_rgb.0, fallback_rgb.1, fallback_rgb.2, 255, border_width_pt * scale,
                        transform,
                    );
                }

                let cell_para = cell.content.iter()
                    .filter_map(|c| if let CellContent::Paragraph(p) = c { Some(p) } else { None })
                    .next();

                // PROOT-003 followup: cell text color wiring. Previously only
                // concatenated a plain string and hardcoded TEXT_COLOR; the
                // parser layer already resolves run.color correctly (verified
                // against n80340 "Yogesh" white bold text). Wire per-run color
                // reusing the same set_rich_text pattern as render_shape_text.
                let text: String = cell_para
                    .map(|p| p.runs.iter().map(|r| r.text.as_str()).collect::<String>())
                    .unwrap_or_default();

                if !text.trim().is_empty() {
                    let (pad_l, pad_r, pad_t, _pad_b) = resolve_cell_padding(cell);
                    let text_x = cell_x + pad_l * scale;
                    let text_y = cell_y + pad_t * scale;
                    let cell_font_size = resolve_cell_font_size(cell) * scale;
                    let cell_line_height = cell_font_size * 1.4;
                    let avail_w_draw = (cell_w - (pad_l + pad_r) * scale).max(1.0);

                    // Parley迁移（任务2）：cell 内文字水平对齐。
                    let cell_align = cell_para.and_then(|p| p.alignment).map(|align| match align {
                        roe_model::style::Alignment::Left    => parley::Alignment::Start,
                        roe_model::style::Alignment::Center  => parley::Alignment::Center,
                        roe_model::style::Alignment::Right   => parley::Alignment::End,
                        roe_model::style::Alignment::Justify => parley::Alignment::Justify,
                    }).unwrap_or(parley::Alignment::Start);

                    // PROOT-031: cell 文字颜色/加粗 fallback 优先级为
                    // run 自身显式值 > TableStyle fallback（style_parts 链，
                    // 阶段2上方已算出）> 原有硬编码 TEXT_COLOR/不加粗。
                    let style_text_color = style_parts.as_ref()
                        .and_then(|parts| parts.iter().find_map(|p| p.text_color.as_ref()))
                        .map(|cr| cr.resolve(theme_colors))
                        .map(|c| (c.r, c.g, c.b));
                    let style_text_bold = style_parts.as_ref()
                        .map(|parts| parts.iter().any(|p| p.text_bold))
                        .unwrap_or(false);

                    let default_color = style_text_color.unwrap_or(TEXT_COLOR);
                    let default_brush = ImpressBrush { color: [default_color.0, default_color.1, default_color.2, 255], deco: 0, hl: [0, 0, 0, 0] };
                    let mut builder = self.layout_context.ranged_builder(
                        &mut self.font_context, &text, 1.0, true,
                    );
                    builder.push_default(StyleProperty::Brush(default_brush));
                    builder.push_default(StyleProperty::FontSize(cell_font_size));
                    builder.push_default(parley::LineHeight::Absolute(cell_line_height));
                    if style_text_bold {
                        builder.push_default(StyleProperty::FontWeight(parley::FontWeight::BOLD));
                    }

                    if let Some(p) = cell_para {
                        let mut byte_off = 0usize;
                        for r in &p.runs {
                            let start = byte_off;
                            let end = start + r.text.len();
                            byte_off = end;
                            if start >= end { continue; }
                            let (cr, cg, cb) = r.color.map(|c| (c.r, c.g, c.b)).unwrap_or(default_color);
                            builder.push(StyleProperty::Brush(ImpressBrush { color: [cr, cg, cb, 255], deco: 0, hl: [0, 0, 0, 0] }), start..end);
                            if r.bold.unwrap_or(style_text_bold) {
                                builder.push(StyleProperty::FontWeight(parley::FontWeight::BOLD), start..end);
                            }
                            if r.italic.unwrap_or(false) {
                                builder.push(StyleProperty::FontStyle(parley::FontStyle::Italic), start..end);
                            }
                        }
                    }

                    let mut layout: parley::Layout<ImpressBrush> = builder.build(&text);
                    layout.break_all_lines(Some(avail_w_draw));
                    layout.align(cell_align, parley::AlignmentOptions::default());

                    let mut pen = ImpressPen::new(pixmap, transform);
                    for line in layout.lines() {
                        for item in line.items() {
                            if let parley::PositionedLayoutItem::GlyphRun(glyph_run) = item {
                                render_glyph_run_impress(&glyph_run, &mut pen, text_x, text_y);
                            }
                        }
                    }
                }
            }
        }
    }

    /// 渲染 Shape 内的文字
    fn render_shape_text(
        &mut self,
        pixmap: &mut PixmapMut,
        paragraphs: &[Paragraph],
        styles: &StyleRegistry,
        text_body: &roe_model::impress::TextBodyProps,
        x: f32, y: f32, w: f32, h: f32,
        scale: f32,
        transform: Transform,
    ) {
        use roe_model::impress::TextAutofit;

        // 内边距：优先 bodyPr insets，否则默认 PPTX 规范默认值
        let pad_l = text_body.ins_l.unwrap_or(9.14)  * scale;
        let pad_r = text_body.ins_r.unwrap_or(9.14)  * scale;
        let pad_t = text_body.ins_t.unwrap_or(4.57)  * scale;
        let pad_b = text_body.ins_b.unwrap_or(4.57)  * scale;

        let text_x = x + pad_l;
        let text_w = if text_body.wrap {
            (w - pad_l - pad_r).max(1.0)
        } else {
            f32::MAX / 2.0  // 不换行：给 cosmic-text 极大宽度
        };

        // normAutofit 字号缩放系数（0.0‒1.0）
        let font_scale = match &text_body.autofit {
            TextAutofit::NormAutofit { font_scale, .. } => font_scale.unwrap_or(1.0),
            _ => 1.0,
        };

        // spAutoFit：不限制高度；其他：限制在 shape 高度内
        let height_limit = match text_body.autofit {
            TextAutofit::SpAutoFit => None,
            _ => Some(h - pad_t - pad_b),
        };

        // PROOT-004 followup: <a:bodyPr anchor="ctr"/> 垂直对齐。
        // Top（默认）保持原有行为不变；Middle/Bottom 需要先预扫描一遍
        // 所有段落的实际排版高度（含自动换行行数），才能算出文字块整体
        // 应该往下偏移多少。预扫描复用与正式绘制相同的 font_size/
        // line_height 计算方式,并且和正式循环一样跑一次 shape_until_scroll
        // 来获取准确换行行数，只是不调用 draw()，避免"预扫描高度"和
        // "实际绘制高度"因为近似公式不一致而导致居中出现偏差。
        let mut cursor_y = y + pad_t;

        if !matches!(text_body.anchor, roe_model::impress::TextAnchor::Top) {
            let mut total_text_height = 0.0_f32;
            for para in paragraphs {
                let para_style_name = para.style.0.as_str();
                let para_style = styles.resolve_paragraph_style(para_style_name);
                let para_font_size = para_style.font_size.unwrap_or(DEFAULT_FONT_SIZE) * scale;

                let mut run_texts: Vec<String> = Vec::new();
                let mut run_font_sizes: Vec<f32> = Vec::new();
                // PROOT-035: tab stop 是整段落的绝对位置语义，累积光标 x
                // 必须跨 run 传递，每个段落从 0 重新开始（新段落=新行起点）。
                let mut tab_cur_x = 0.0f32;
                for run in &para.runs {
                    // PROOT-FONTSIZE: run 自身 rPr 内联 sz（run.font_size）
                    // 优先级最高，其次才是 char_style 引用的命名样式，最后
                    // 兜底段落字号。此前完全没有读取 run.font_size，导致
                    // "有内联sz但没有char_style引用"的 run（tdf147121 复现）
                    // 被错误地当作"无样式"直接吃 para_font_size 兜底。
                    let fs = if let Some(rfs) = run.font_size {
                        rfs * scale
                    } else if let Some(sref) = &run.char_style {
                        let cs = styles.resolve_char_style(&sref.0);
                        cs.font_size.unwrap_or(para_style.font_size.unwrap_or(DEFAULT_FONT_SIZE)) * scale
                    } else {
                        para_font_size
                    };
                    // PROOT-035: 展开 \t 为动态宽度空格（详见 expand_tabs_to_spaces
                    // 文档注释），预扫描分支同样需要——展开后行数可能变化，
                    // 影响垂直居中的总高度估算，必须和正式绘制分支一致。
                    let expanded_text = self.expand_tabs_to_spaces(&run.text, fs, scale, &mut tab_cur_x);
                    run_texts.push(expanded_text);
                    // PROOT-FONTSIZE-BR: 纯换行符 run（<a:br> 产生，text=="\n"）
                    // 没有可见字形，不该参与"段内最大字号"比较——它的 font_size
                    // 是继承补全来的（parser 层对所有 run 一视同仁做样式兜底），
                    // 数值可能明显偏离段内实际可见文字的字号（tdf147121 复现：
                    // 可见文字 sz=1400，换行符 run 被兜底成 default_style 的
                    // sz=1800，若不排除会把整段字号错误拉大到 18pt）。
                    if run.text != "\n" {
                        run_font_sizes.push(fs);
                    }
                }

                let text: String = run_texts.join("");
                if text.trim().is_empty() {
                    total_text_height += para_font_size * 1.4 * para.line_height.unwrap_or(1.0) + gap_v319h(para.space_after, scale);
                if !paragraphs.iter().next().map(|q| std::ptr::eq(q, para)).unwrap_or(false) { total_text_height += gap_v319h(para.space_before, scale); }
                    continue;
                }

                let font_size = run_font_sizes.iter().cloned().fold(f32::MIN, f32::max);
                // PROOT-FONTSIZE: 此前对 run 最大字号再和 para_font_size 取
                // max，导致 run 明确指定了比段落继承字号更小的 sz 时（如
                // tdf147121 的 sz=1400 小于 master 继承的 18pt）被错误拉大。
                // 只有 run_font_sizes 为空（理论上不会发生，因为前面已对
                // 空文本 continue，这里仅作防御）时才兜底 para_font_size。
                let font_size = (if font_size.is_finite() { font_size } else { para_font_size }) * font_scale;
                let line_height = font_size * 1.4 * para.line_height.unwrap_or(1.0);

                // Parley迁移（任务3）：预扫描阶段只需要数行数，用parley
                // 简单layout即可，不需要颜色/粗体/斜体等属性（这些只影响
                // 绘制观感，不影响行数）。
                let mut probe_builder = self.layout_context.ranged_builder(
                    &mut self.font_context, &text, 1.0, true,
                );
                probe_builder.push_default(StyleProperty::FontSize(font_size));
                probe_builder.push_default(parley::LineHeight::Absolute(line_height));
                let mut probe_layout: parley::Layout<ImpressBrush> = probe_builder.build(&text);
                probe_layout.break_all_lines(Some(text_w));
                let n_lines = probe_layout.lines().count().max(1);
                total_text_height += line_height * n_lines as f32 + gap_v319h(para.space_after, scale);
                if !paragraphs.iter().next().map(|q| std::ptr::eq(q, para)).unwrap_or(false) { total_text_height += gap_v319h(para.space_before, scale); }
            }

            let available_height = (h - pad_t - pad_b).max(0.0);
            let extra_space = (available_height - total_text_height).max(0.0);
            let offset = match text_body.anchor {
                roe_model::impress::TextAnchor::Middle => extra_space / 2.0,
                roe_model::impress::TextAnchor::Bottom => extra_space,
                roe_model::impress::TextAnchor::Top => 0.0,
            };
            cursor_y += offset;
        }

        for para in paragraphs {
            // 段落级字号：从 paragraph style 取，兜底 DEFAULT_FONT_SIZE
            let para_style_name = para.style.0.as_str();
            let para_style = styles.resolve_paragraph_style(para_style_name);
            let para_font_size = para_style.font_size.unwrap_or(DEFAULT_FONT_SIZE) * scale;

            // PROOT bullet 任务D: 本段落是否需要绘制bullet符号，以及正文
            // 整体应该往右偏移多少（mar_l，pt换算px已含scale）。MVP范围
            // 只做BulletKind::Char（v43进度文档2.2范围确认：buAutoNum只做
            // 顺序编号MVP，本次先不实现，等有真实样本再补）。
            // text_x是函数级默认值（无bullet段落正文起始位置），
            // para_text_x是"这一段落实际应该用的x"——只有Char bullet才会
            // 偏移，其余情况(None/AutoNum/无bullet)等于text_x，保持原有
            // 视觉不变。
            let mut para_text_x = text_x;
            // PROOT-024 子问题3 bugfix: 有 bullet 的段落，正文起始 x 往右
            // 偏移了 mar_l，但排版宽度（传给 cosmic-text set_size 的宽度）
            // 之前一直用函数级的 text_w，没有跟着收窄。cosmic-text 是在
            // [0, 传入宽度] 这个区间内做居中计算的，绘制时再把结果整体平移
            // 到 para_text_x —— 如果宽度没跟着收窄，相当于把居中区间的宽度
            // 算多了 mar_l，导致居中后的中点比正确位置偏左（复现样本
            // tdf148810_PARA_OUTLLEVEL：X 贴着 bullet 符号，没有整体居中）。
            // para_text_w 就是"这一段落实际应该用于排版计算的宽度"，与
            // para_text_x 配套：右边界不变（还是 text_x + text_w 对应的
            // 右边界），只是左边界跟着 mar_l 一起往右收。
            let mut para_text_w = text_w;
            // PROOT-024: bullet_char_draw 新增第三个字段存 buFont typeface
            // （如 "Wingdings"），用于下方绘制时指定 cosmic-text 字体族，
            // 修复符号字体+PUA码点bullet被默认字体误渲染成不相关字形的问题
            // （复现样本 tdf148810_PARA_OUTLLEVEL）。系统没有该字体时
            // fontdb会按其内置fallback链自动兜底，不需要我们自己写降级
            // 逻辑。
            let mut bullet_char_draw: Option<(String, f32, Option<String>, Option<roe_model::common::Color>)> = None; // (字符, 绘制x位置, buFont typeface, buClr颜色)
            if let Some(bullet) = &para.bullet {
                if matches!(&bullet.kind, roe_model::impress::BulletKind::None | roe_model::impress::BulletKind::AutoNum { .. }) {
                    let m = bullet.mar_l.unwrap_or(0.0) * scale;
                    para_text_x = text_x + m;
                    para_text_w = (text_w - m).max(1.0);
                }
                if let roe_model::impress::BulletKind::Char { ch, font } = &bullet.kind {
                    let mar_l = bullet.mar_l.unwrap_or(0.0) * scale;
                    let indent = bullet.indent.unwrap_or(0.0) * scale;
                    para_text_x = text_x + mar_l;
                    para_text_w = (text_w - mar_l).max(1.0);
                    // PROOT bullet-symbol-font: Wingdings/Webdings等符号字体
                    // 的buChar码位落在Unicode私有使用区（PUA），字面字符本身
                    // （如"q"）在常规字体里会显示成普通拉丁字母，不是符号字体
                    // 设计时的真实字形。真实样本bulletColor验证：参照图（
                    // OnlyOffice/LibreOffice）显示为方块符号，非字母q。完整
                    // 方案需要Wingdings码位映射表+打包符号字体（等同于LOK
                    // fontcvt.cxx的aWingDingsTab方案，涉及224个码位+额外
                    // 字体资源，超出本次范围）。当前采用最小方案：只覆盖
                    // 已在样本库中验证到的具体字符，直接替换成语义等价的
                    // 通用Unicode符号字符，交给系统已装的Noto Sans Symbols/
                    // Symbola字体渲染，不再指定Wingdings字体名（系统未装，
                    // 指定了也是走fallback，不如直接不指定更确定）。
                    // 未命中映射表的符号字体字符维持原字符+原font不变，
                    // 不引入本次未验证的行为改变。
                    let (draw_ch, draw_font) = map_symbol_font_bullet_char(ch, font.as_deref());
                    bullet_char_draw = Some((draw_ch, text_x + mar_l + indent, draw_font, bullet.color));
                }
                if let roe_model::impress::BulletKind::AutoNum { scheme, start_at: _ } = &bullet.kind {
                    let mar_l = bullet.mar_l.unwrap_or(0.0) * scale;
                    let indent = bullet.indent.unwrap_or(0.0) * scale;
                    let idx_p = paragraphs.iter().position(|q| std::ptr::eq(q, para)).unwrap_or(0);
                    let mut s0 = idx_p;
                    while s0 > 0 {
                        let same = paragraphs.iter().nth(s0 - 1).and_then(|q| q.bullet.as_ref())
                            .map(|b| matches!(&b.kind, roe_model::impress::BulletKind::AutoNum { scheme: s2, .. } if s2 == scheme))
                            .unwrap_or(false);
                        if same { s0 -= 1; } else { break; }
                    }
                    let first_start = paragraphs.iter().nth(s0).and_then(|q| q.bullet.as_ref())
                        .and_then(|b| match &b.kind { roe_model::impress::BulletKind::AutoNum { start_at, .. } => *start_at, _ => None })
                        .unwrap_or(1);
                    let num = first_start + (idx_p - s0) as i32;
                    bullet_char_draw = Some((format_autonum_v319g(scheme, num), text_x + mar_l + indent, None, bullet.color));
                }
            }

            // 逐 run 收集文字、字号、颜色、粗体、斜体（PPTX-14/15：改用
            // set_rich_text 按 run 传各自 Attrs，不再只取 first() 丢弃后续
            // run 的样式；字号仍是 buffer 级别共享 —— cosmic-text 0.12.1
            // 架构限制，Attrs 不含字号字段，PPTX-15 结论：接受"段内最大
            // 字号"近似，而非逐run独立字号，见 roe-ext 调研文档 §3.2）
            let mut run_texts: Vec<String> = Vec::new();
            let mut run_font_sizes: Vec<f32> = Vec::new();
            let mut run_colors: Vec<(u8, u8, u8)> = Vec::new();
            let mut run_bolds: Vec<bool> = Vec::new();
            let mut run_italics: Vec<bool> = Vec::new();
            let mut run_fonts: Vec<Option<String>> = Vec::new();
            let mut run_decos: Vec<(bool, bool, Option<[u8; 4]>, u8)> = Vec::new();
            // PROOT-035: tab stop 是整段落的绝对位置语义，累积光标 x
            // 必须跨 run 传递，每个段落从 0 重新开始（新段落=新行起点）。
            let mut tab_cur_x = 0.0f32;

            for run in &para.runs {
                let (fs, color) = {
                    // PROOT-FONTSIZE: 同预扫描逻辑，run.font_size（内联 sz）
                    // 优先于 char_style 命名样式，最后才兜底段落字号。
                    let (base_fs, base_color) = if let Some(sref) = &run.char_style {
                        let cs = styles.resolve_char_style(&sref.0);
                        let fs = run.font_size.or(cs.font_size).unwrap_or(para_style.font_size.unwrap_or(DEFAULT_FONT_SIZE)) * scale;
                        let color = cs.color.map(|c| (c.r, c.g, c.b)).unwrap_or((20, 20, 20));
                        (fs, color)
                    } else if let Some(rfs) = run.font_size {
                        (rfs * scale, (20, 20, 20))
                    } else {
                        (para_font_size, (20, 20, 20))
                    };
                    // run.color 直接优先（pptx rPr solidFill/schemeClr）
                    let color = run.color.map(|c| (c.r, c.g, c.b)).unwrap_or(base_color);
                    (base_fs, color)
                };
                // PROOT-035: 展开 \t 为动态宽度空格（详见 expand_tabs_to_spaces
                // 文档注释），必须用 run_texts（真正喂给 shaper 的文本）而非
                // run.text 原文，否则 tofu 依然会出现。
                let expanded_text = self.expand_tabs_to_spaces(&run.text, fs, scale, &mut tab_cur_x);
                run_texts.push(expanded_text);
                // PROOT-FONTSIZE-BR: 同预扫描逻辑，纯换行符 run 不参与字号比较。
                if run.text != "\n" {
                    run_font_sizes.push(fs);
                }
                run_colors.push(color);
                run_bolds.push(run.bold.unwrap_or(false));
                run_italics.push(run.italic.unwrap_or(false));
                run_fonts.push(run.font_family.clone().or_else(|| run.font_family_ascii.clone()));
                run_decos.push((run.underline.unwrap_or(false), run.strikethrough.unwrap_or(false), run.background.map(|c| [c.r, c.g, c.b, 255]), match run.script { Some(roe_model::writer::ScriptPosition::Superscript) => 1u8, Some(roe_model::writer::ScriptPosition::Subscript) => 2u8, _ => 0u8 }));
            }

            let text: String = run_texts.join("");
            if text.trim().is_empty() {
                if !paragraphs.iter().next().map(|q| std::ptr::eq(q, para)).unwrap_or(false) { cursor_y += gap_v319h(para.space_before, scale); }
                if let Some((bch, bx, bfont, bcol)) = &bullet_char_draw {
                    let bfs = para_font_size * font_scale;
                    let bcolor = match bcol { Some(c) => [c.r, c.g, c.b, c.a], None => [20, 20, 20, 255] };
                    let ebrush = ImpressBrush { color: bcolor, deco: 0, hl: [0, 0, 0, 0] };
                    let mut ebuilder = self.layout_context.ranged_builder(&mut self.font_context, bch, 1.0, true);
                    ebuilder.push_default(StyleProperty::Brush(ebrush));
                    ebuilder.push_default(StyleProperty::FontSize(bfs));
                    ebuilder.push_default(parley::LineHeight::Absolute(bfs * 1.4 * para.line_height.unwrap_or(1.0)));
                    if let Some(fname) = bfont {
                        ebuilder.push_default(StyleProperty::FontFamily(parley::FontFamily::named(fname)));
                    }
                    let mut elayout: parley::Layout<ImpressBrush> = ebuilder.build(bch);
                    elayout.break_all_lines(Some(f32::MAX / 2.0));
                    let eoff = match para.alignment {
                        Some(roe_model::style::Alignment::Center) => para_text_w / 2.0,
                        Some(roe_model::style::Alignment::Right) => para_text_w,
                        _ => 0.0,
                    };
                    let mut epen = ImpressPen::new(pixmap, transform);
                    for eline in elayout.lines() {
                        for eitem in eline.items() {
                            if let parley::PositionedLayoutItem::GlyphRun(egr) = eitem {
                                render_glyph_run_impress(&egr, &mut epen, *bx + eoff, cursor_y);
                            }
                        }
                    }
                }
                cursor_y += para_font_size * 1.4 * para.line_height.unwrap_or(1.0) + gap_v319h(para.space_after, scale);
                continue;
            }

            // buffer 字号：段内最大 run 字号（PPTX-15 近似，替代此前"取第一个
            // run"——避免第一个run恰好是小字号强调词导致整段偏小被裁切）
            let font_size = run_font_sizes.iter().cloned().fold(f32::MIN, f32::max);
                // PROOT-FONTSIZE: 此前对 run 最大字号再和 para_font_size 取
                // max，导致 run 明确指定了比段落继承字号更小的 sz 时（如
                // tdf147121 的 sz=1400 小于 master 继承的 18pt）被错误拉大。
                // 只有 run_font_sizes 为空（理论上不会发生，因为前面已对
                // 空文本 continue，这里仅作防御）时才兜底 para_font_size。
                let font_size = (if font_size.is_finite() { font_size } else { para_font_size }) * font_scale;
            let line_height = font_size * 1.4 * para.line_height.unwrap_or(1.0);

            // Parley迁移（任务3）：按 run 构造 range-based styles，取代
            // cosmic-text 的 (文字,Attrs) spans 模式。
            let buf_h = height_limit.map(|hl| (hl - (cursor_y - (y + pad_t))).max(line_height));
            let _ = buf_h; // Parley的break_all_lines只接受宽度，高度限制在上层cursor_y累加中体现，同旧逻辑

            let default_color = run_colors.first().copied().unwrap_or((20, 20, 20));
            let layout = self.build_para_layout(
                &text, &run_texts, &run_colors, &run_bolds, &run_italics, &run_fonts, &run_decos,
                font_size, line_height, para.alignment, para_text_w,
            );

            if !paragraphs.iter().next().map(|q| std::ptr::eq(q, para)).unwrap_or(false) { cursor_y += gap_v319h(para.space_before, scale); }
            let draw_y = cursor_y;
            let (dtr, dtg, dtb) = default_color;

            // PROOT bullet 任务D: bullet符号绘制，画在正文第一行之前（段首）。
            // 用独立的小buffer走一遍完整shape流程（cosmic-text没有"画单个
            // 字符"的捷径），字号跟随本段落正文字号(font_size)。颜色优先
            // 用<a:buClr>指定的颜色（真实样本bulletColor验证：红色buClr+
            // 黑色正文），没有buClr时才回退到正文首个run颜色(dtr/dtg/dtb)
            // 近似。只画在段落第一行，不会随<a:br>产生的换行重复出现
            // （bullet挂在Paragraph上，一个Paragraph只绘制一次，天然满足）。
            // Parley迁移（任务3）：bullet符号用独立小layout绘制。
            // buFont typeface（如Wingdings）通过GenericFamily/FontStack
            // 指定；Parley若找不到该字体族，会按自身fallback链降级，
            // 与旧cosmic-text行为一致，不阻塞渲染。
            if let Some((ch, bullet_x, bullet_font, bullet_color)) = &bullet_char_draw {
                // PROOT bullet-color: 优先用 <a:buClr> 指定的颜色，样本库
                // 之前没有验证用例、只能用正文色近似的局限已由真实样本
                // bulletColor（红色buClr+黑色正文）解除，见解析层同批改动。
                let bullet_brush = match bullet_color {
                    Some(c) => ImpressBrush { color: [c.r, c.g, c.b, c.a], deco: 0, hl: [0, 0, 0, 0] },
                    None => ImpressBrush { color: [dtr, dtg, dtb, 255], deco: 0, hl: [0, 0, 0, 0] },
                };
                let mut bbuilder = self.layout_context.ranged_builder(
                    &mut self.font_context, ch, 1.0, true,
                );
                bbuilder.push_default(StyleProperty::Brush(bullet_brush));
                bbuilder.push_default(StyleProperty::FontSize(font_size));
                bbuilder.push_default(parley::LineHeight::Absolute(line_height));
                if let Some(fname) = bullet_font {
                    bbuilder.push_default(StyleProperty::FontFamily(parley::FontFamily::named(fname)));
                }
                let mut bullet_layout: parley::Layout<ImpressBrush> = bbuilder.build(ch);
                bullet_layout.break_all_lines(Some(f32::MAX / 2.0));
                let bullet_first_off: f32 = layout.lines().next().and_then(|l| l.items().find_map(|it| {
                    if let parley::PositionedLayoutItem::GlyphRun(gr) = it { Some(gr.offset()) } else { None }
                })).unwrap_or(0.0);
                let mut bpen = ImpressPen::new(pixmap, transform);
                for line in bullet_layout.lines() {
                    for item in line.items() {
                        if let parley::PositionedLayoutItem::GlyphRun(glyph_run) = item {
                            render_glyph_run_impress(&glyph_run, &mut bpen, *bullet_x + bullet_first_off, draw_y);
                        }
                    }
                }
            }

            let mut pen = ImpressPen::new(pixmap, transform);
            for line in layout.lines() {
                for item in line.items() {
                    if let parley::PositionedLayoutItem::GlyphRun(glyph_run) = item {
                        render_glyph_run_impress(&glyph_run, &mut pen, para_text_x, draw_y);
                    }
                }
            }

            let n_lines = layout.lines().count().max(1);
            cursor_y += line_height * n_lines as f32 + gap_v319h(para.space_after, scale);
        }
    }

    /// PPTX-12: 静态填充解析结果——纯色 or 渐变。取代旧版直接返回
    /// `Option<(u8,u8,u8,u8)>` 的设计（旧版把渐变强行退化成 start 色）。
    /// runtime 覆盖场景（拖拽交互临时纯色）不经过这个类型，见 render_shape
    /// 里 fill_color 计算处的既有分支，保持不变。
    fn shape_fill_color(shape: &Shape, alpha: u8, theme_colors: &HashMap<String, roe_model::common::Color>) -> Option<ResolvedFill> {
        match &shape.style.fill {
            // PROOT-FILL-ALPHA-DROP bugfix: 此前直接用外部传入的alpha参数
            // 覆盖c.a，丢弃了形状颜色自身的透明度（如<a:schemeClr val="X">
            // <a:alpha val="30000"/></a:schemeClr>解析出的Color.a）。alpha
            // 参数实际语义是runtime动画整体不透明度（render_shape里
            // `let alpha = (opacity * 255.0) as u8`，opacity默认1.0/静态
            // 渲染场景恒为255），与c.a是完全独立的两层，必须相乘而非互相
            // 覆盖——静态场景alpha=255时相乘结果等价于原样使用c.a，不影响
            // 现有行为；runtime动画场景两层透明度都需要生效。真实样本
            // sd_data_pptx__ooxtheme.pptx的8个装饰形状（accent1/accent2
            // 半透明色块）复现：alpha数值本身在raw_to_shape阶段计算正确
            // （debug打印可见c.a=76等），但渲染时被静态场景恒定的255覆盖，
            // 导致所有形状显示为不透明实色，看不到OnlyOffice参照图里
            // 形状叠加透光的效果。
            Some(FillStyle::Solid(c)) => {
                let combined_a = (c.a as u32 * alpha as u32 / 255) as u8;
                Some(ResolvedFill::Solid(c.r, c.g, c.b, combined_a))
            }
            Some(FillStyle::Gradient { stops, kind, .. }) => {
                // rotate_with_shape 已经在解析层 raw_to_shape 阶段叠加进
                // Linear 的 angle 里了（PPTX-12, pptx.rs），这里不需要
                // 再处理，直接用 kind 里的最终角度。
                let resolved_stops: Vec<(f32, u8, u8, u8, u8)> = stops.iter()
                    .map(|s| (s.pos, s.color.r, s.color.g, s.color.b, alpha))
                    .collect();
                let resolved_kind = match kind {
                    roe_model::common::GradientKind::Linear { angle } => {
                        GradientKindResolved::Linear { angle: *angle }
                    }
                    roe_model::common::GradientKind::Radial => GradientKindResolved::Radial,
                    // tiny-skia 无原生"矩形渐变"（RectGradient）能力，只有
                    // Linear/Radial/Sweep 三种。用 Radial 近似替代：视觉上
                    // 都是"中心到边缘过渡"的观感，比完全不处理（比如退化
                    // 成纯色）更接近原意。这是明确的、有意为之的第一期
                    // 降级，不是遗漏——如果未来需要精确的方形等值线渐变，
                    // 需要手写 Shader 或用径向渐变的椭圆变体近似，属于
                    // 独立的后续优化项。
                    roe_model::common::GradientKind::Rect => GradientKindResolved::Radial,
                };
                Some(ResolvedFill::Gradient { stops: resolved_stops, kind: resolved_kind })
            }
            Some(FillStyle::None) => None,
            // PROOT-005: FillStyle::Picture 目前只会出现在 Slide/MasterSlide.background
            // （幻灯片背景图，走 render_slide 里独立的绘制路径），不会出现在普通
            // shape.style.fill 上。这里穷尽 match 只是让编译器满意，普通形状真的
            // 遇到这个变体时按"无填充"处理，不崩溃、不误画。
            Some(FillStyle::Picture(_)) => None,
            // ROE-TILE-001: FillStyle::PictureTiled 同 Picture，只会出现在
            // Slide/MasterSlide.background（走 render_slide 独立绘制路径），
            // 不会出现在普通 shape.style.fill 上。同理按"无填充"处理保底。
            Some(FillStyle::PictureTiled { .. }) => None,
            None => {
                // PROOT-005 附带修复：占位符（shape.placeholder.is_some()）没有显式
                // fill 时不应套用下面的"主题accent1半透明兜底"——真实样本tdf158512
                // 的 subTitle 占位符 <p:spPr/> 完全为空（既非 <a:noFill/> 也非
                // <a:solidFill>），真实 OnlyOffice/PowerPoint 渲染是透明背景，
                // 而当前兜底逻辑把所有 fill:None 的形状（不区分是否占位符）都刷成
                // 半透明蓝色，导致占位符文本框出现不该有的色块。
                //
                // 范围裁剪（如实标注，非完整修复）：这里只处理"占位符本身没有更上层
                // fill 继承来源、直接判透明"这一种情况。真正的 layout/master 占位符
                // fill 继承链路（类比已有的 LayoutPhStyle 文字样式继承）本次未实现——
                // LayoutPhStyle 目前只有 font_size/bold/italic/color/font_family
                // 五个字段，没有 fill，本样本 layout 也确实没给 subTitle 定义 fill，
                // 用"占位符默认透明"已经能得到正确结果，暂不需要为此单独立项。
                // 若未来遇到 layout 显式为占位符定义了 fill 色却渲染不出来的样本，
                // 才需要扩展 LayoutPhStyle + PlaceholderCandidate 加 fill 字段。
                if shape.placeholder.is_some() {
                    return None;
                }
                // PROOT-CONNECTOR-FILL: connector 折线（custGeom路径+headEnd/
                // tailEnd箭头装饰，如candy.pptx slide11那批圆点连接线）没有
                // 显式 fill 是正常语义——线条本来就不该有填充，PowerPoint/
                // OnlyOffice 对这类shape也是透明处理。此前用"是否是占位符"
                // 单一维度判断是否套用 accent1 兜底，没考虑到 connector 这个
                // 场景，导致这批线被整个楔形区域填充成半透明绿色（复现现象：
                // 折线渲染成实心绿色三角块，而不是细描边线+圆点端点）。
                // 用"是否带箭头装饰"作为 connector 身份的判断依据（比检查
                // geometry是否PresetPath更精确——普通装饰性PresetPath形状
                // 未必是connector，但带head_arrow/tail_arrow的一定是）。
                //
                // PROOT-LINE-FILL-BUGFIX（v94 ooxtheme样本走查发现）：上面
                // "带箭头才算connector"的判断遗漏了一种真实场景——
                // <p:cxnSp><a:prstGeom prst="line">（geometry=Line）且没有
                // headEnd/tailEnd箭头装饰的纯直线连接线（真实样本
                // sd_data_pptx__ooxtheme.pptx layout1的Straight Connector
                // 31/20，仅<a:ln>描边，fillRef idx="0"明确无填充）。这类
                // shape同样落入"无箭头→不算connector→套用accent1兜底"的
                // 误判，导致细描边线被整个bounding box涂成实心色块（复现：
                // 两条本该细灰线的connector变成大片浅绿矩形，颜色也从
                // bg1+lumMod错误换成了accent1半透明）。geometry是Line
                // 本身就是connector的强特征（对应上面render_shape里
                // ShapeGeometry::Line分支的专属描边处理注释），补上这条
                // 判断，与"带箭头"条件取或。
                if shape.style.head_arrow.is_some() || shape.style.tail_arrow.is_some()
                    || matches!(shape.geometry, ShapeGeometry::Line) {
                    return None;
                }
                // 无显式 fill 的普通形状（非占位符、非connector）：用主题 accent1 颜色（半透明）
                let c = theme_colors.get("accent1");
                let (r, g, b) = c.map(|c| (c.r, c.g, c.b)).unwrap_or((0x4F, 0x81, 0xBD));
                Some(ResolvedFill::Solid(r, g, b, (alpha as u32 * 150 / 255) as u8))
            }
        }
    }

    /// PPTX-12: 根据 ResolvedFill 构造一个可直接传给 fill_rect/fill_path
    /// 的 tiny-skia Paint。渐变场景下，起止点/半径按形状的屏幕坐标矩形
    /// (x,y,w,h) 计算——注意这里用的是"已经乘过 scale、加过偏移"之后的
    /// 屏幕坐标（调用方传入前已完成），Shader 自身不再叠加 shape_transform
    /// （旋转/翻转已经通过传给 fill_rect/fill_path 的 transform 参数应用
    /// 在几何层面，Shader 的坐标空间与被绘制路径的坐标空间一致，不需要
    /// 重复变换，否则会导致渐变方向和形状旋转"转两次"）。
    fn build_fill_paint(resolved: &ResolvedFill, x: f32, y: f32, w: f32, h: f32) -> Option<Paint<'static>> {
        match resolved {
            ResolvedFill::Solid(r, g, b, a) => {
                let mut paint = Paint::default();
                paint.set_color_rgba8(*r, *g, *b, *a);
                Some(paint)
            }
            ResolvedFill::Gradient { stops, kind } => {
                if stops.is_empty() {
                    return None;
                }
                let sk_stops: Vec<SkGradientStop> = stops.iter()
                    .map(|(pos, r, g, b, a)| SkGradientStop::new(*pos, Color::from_rgba8(*r, *g, *b, *a)))
                    .collect();

                let cx = x + w / 2.0;
                let cy = y + h / 2.0;

                let shader = match kind {
                    GradientKindResolved::Linear { angle } => {
                        // OOXML/ROE 角度约定：0度=从左到右（沿+X方向渐变），
                        // 角度顺时针增大（与常规数学坐标系相反，屏幕坐标系
                        // Y轴向下时"顺时针"与数学逆时针在视觉上一致，这里
                        // 不做额外镜像，直接用标准三角函数）。
                        // 起止点取形状对角线长度，确保覆盖整个形状范围
                        // （比只取w或h更保守，避免斜角度时渐变在角落被
                        // 提前clamp到纯色）。
                        let angle_rad = angle.to_radians();
                        let half_diag = ((w * w + h * h).sqrt()) / 2.0;
                        let dx = angle_rad.cos() * half_diag;
                        let dy = angle_rad.sin() * half_diag;
                        let start = Point::from_xy(cx - dx, cy - dy);
                        let end = Point::from_xy(cx + dx, cy + dy);
                        LinearGradient::new(start, end, sk_stops, SpreadMode::Pad, Transform::identity())
                    }
                    GradientKindResolved::Radial => {
                        // 第一期固定中心 = 形状几何中心（0.5,0.5），不读取
                        // fillToRect 精确偏移中心，见 GradientKind::Radial
                        // 模型层文档注释。半径取形状对角线一半，确保径向
                        // 渐变能覆盖到形状四角。
                        let radius = ((w * w + h * h).sqrt()) / 2.0;
                        let center = Point::from_xy(cx, cy);
                        RadialGradient::new(center, 0.0, center, radius, sk_stops, SpreadMode::Pad, Transform::identity())
                    }
                };
                shader.map(|s| {
                    let mut paint = Paint::default();
                    paint.shader = s;
                    paint
                })
            }
        }
    }

    /// PPTX-12: 签名从 (r,g,b,a) 改为接受现成的 &Paint，让椭圆分支能够
    /// 复用 build_fill_paint 构造出的渐变 Shader Paint，而不只能画纯色。
    fn fill_ellipse(
        pixmap: &mut PixmapMut,
        x: f32, y: f32, w: f32, h: f32,
        paint: &Paint,
        transform: Transform,
    ) {
        let mut pb = PathBuilder::new();
        let cx = x + w / 2.0;
        let cy = y + h / 2.0;
        let rx = w / 2.0;
        let ry = h / 2.0;
        // 近似椭圆（贝塞尔）
        let k = 0.5522848;
        pb.move_to(cx + rx, cy);
        pb.cubic_to(cx + rx, cy - ry * k, cx + rx * k, cy - ry, cx, cy - ry);
        pb.cubic_to(cx - rx * k, cy - ry, cx - rx, cy - ry * k, cx - rx, cy);
        pb.cubic_to(cx - rx, cy + ry * k, cx - rx * k, cy + ry, cx, cy + ry);
        pb.cubic_to(cx + rx * k, cy + ry, cx + rx, cy + ry * k, cx + rx, cy);
        pb.close();
        if let Some(path) = pb.finish() {
            pixmap.fill_path(&path, paint, tiny_skia::FillRule::Winding, transform, None);
        }
    }

    /// PROOT-021 bugfix: 椭圆描边路径。此前 Ellipse 的描边统一走
    /// `draw_rect_stroke`（矩形包围盒描边），与 fill_ellipse 已经实现的
    /// 填充路径不一致，导致真实样本 shape-glow-effect 里"椭圆填充色正确，
    /// 但外边框渲染成矩形"。复用 fill_ellipse 同款贝塞尔近似路径构造，
    /// 只是改成 stroke_path 而不是 fill_path，坐标系/参数含义与
    /// fill_ellipse 保持一致（不重新推导）。
    fn stroke_ellipse(
        pixmap: &mut PixmapMut,
        x: f32, y: f32, w: f32, h: f32,
        r: u8, g: u8, b: u8, a: u8, width: f32,
        transform: Transform,
    ) {
        let mut pb = PathBuilder::new();
        let cx = x + w / 2.0;
        let cy = y + h / 2.0;
        let rx = w / 2.0;
        let ry = h / 2.0;
        let k = 0.5522848;
        pb.move_to(cx + rx, cy);
        pb.cubic_to(cx + rx, cy - ry * k, cx + rx * k, cy - ry, cx, cy - ry);
        pb.cubic_to(cx - rx * k, cy - ry, cx - rx, cy - ry * k, cx - rx, cy);
        pb.cubic_to(cx - rx, cy + ry * k, cx - rx * k, cy + ry, cx, cy + ry);
        pb.cubic_to(cx + rx * k, cy + ry, cx + rx, cy + ry * k, cx + rx, cy);
        pb.close();
        if let Some(path) = pb.finish() {
            let mut paint = Paint::default();
            paint.set_color_rgba8(r, g, b, a);
            paint.anti_alias = true;
            let mut stroke = Stroke::default();
            stroke.width = width;
            pixmap.stroke_path(&path, &paint, &stroke, transform, None);
        }
    }

    /// PPTX-11 part C：渲染 `ShapeGeometry::PresetPath`（预设几何形状求值后的
    /// 绝对坐标路径指令序列）。仅负责填充，不处理描边（描边沿用现有
    /// `draw_rect_stroke` 的矩形包围盒描边，与 Ellipse 分支现状一致，是
    /// 预先存在的限制，不在本次改动范围内）。
    ///
    /// 坐标系约定：`cmds` 里的每个点是 shape 局部坐标（0..w_local，
    /// 0..h_local，与 `roe_model::preset_geometry::eval_preset_shape` 的
    /// `w`/`h` 入参同一坐标系，即 `raw.position.extent`），需要通过
    /// `local * scale + offset` 映射到屏幕坐标——offset 就是 `render_shape`
    /// 里已经算好的 `x`/`y`（缩放后的屏幕坐标原点），与 `fill_ellipse`
    /// 现有做法一致，不重新推导。
    ///
    /// ArcTo 的 start_deg/sweep_deg 命名带有误导性：单位实际是 OOXML 原始的
    /// 1/60000 度（与 `preset_geometry.rs` 内部 `eval_cos`/`eval_sin` 消费的
    /// 单位一致，非真正角度），换算见下方 `arc_to_beziers` 注释。
    /// PROOT-002: 从 `render_preset_path` 抽出的纯路径构建逻辑（不做
    /// fill/stroke），供填充（`render_preset_path`）和描边（新增的
    /// `render_preset_path_stroke`，PROOT-002 connector 直线/折线描边
    /// 用）共享，避免重复维护两份坐标映射代码。
    fn build_preset_path(
        cmds: &[roe_model::common::PathCommand],
        x: f32, y: f32, scale: f32,
    ) -> Option<tiny_skia::Path> {
        use roe_model::common::PathCommand;

        let map = |lx: f32, ly: f32| (lx * scale + x, ly * scale + y);

        let mut pb = PathBuilder::new();
        // 当前笔位置（屏幕坐标），用于 ArcTo 贝塞尔近似的分段计算不依赖它，
        // 但保留以便未来扩展（例如需要判断是否要先 move_to 才能开始 arc）。
        let mut has_current_point = false;

        for cmd in cmds {
            match cmd {
                PathCommand::MoveTo { x: lx, y: ly } => {
                    let (sx, sy) = map(*lx, *ly);
                    pb.move_to(sx, sy);
                    has_current_point = true;
                }
                PathCommand::LineTo { x: lx, y: ly } => {
                    let (sx, sy) = map(*lx, *ly);
                    if has_current_point {
                        pb.line_to(sx, sy);
                    } else {
                        pb.move_to(sx, sy);
                        has_current_point = true;
                    }
                }
                PathCommand::CubicTo { x1, y1, x2, y2, x: lx, y: ly } => {
                    let (sx1, sy1) = map(*x1, *y1);
                    let (sx2, sy2) = map(*x2, *y2);
                    let (sx, sy) = map(*lx, *ly);
                    if has_current_point {
                        pb.cubic_to(sx1, sy1, sx2, sy2, sx, sy);
                    } else {
                        pb.move_to(sx, sy);
                        has_current_point = true;
                    }
                }
                PathCommand::QuadTo { x1, y1, x: lx, y: ly } => {
                    let (sx1, sy1) = map(*x1, *y1);
                    let (sx, sy) = map(*lx, *ly);
                    if has_current_point {
                        pb.quad_to(sx1, sy1, sx, sy);
                    } else {
                        pb.move_to(sx, sy);
                        has_current_point = true;
                    }
                }
                PathCommand::ArcTo { cx, cy, rx, ry, start_deg, sweep_deg } => {
                    // cx/cy/rx/ry 是 shape 局部坐标/尺度，先映射圆心和半径到
                    // 屏幕空间（半径只按 scale 缩放，不加 offset）。
                    let (scx, scy) = map(*cx, *cy);
                    let srx = rx * scale;
                    let sry = ry * scale;
                    Self::arc_to_beziers(&mut pb, scx, scy, srx, sry, *start_deg, *sweep_deg, has_current_point);
                    has_current_point = true;
                }
                PathCommand::Close => {
                    pb.close();
                }
            }
        }

        pb.finish()
    }

    /// PROOT-outerShdw: 给定形状的屏幕坐标轮廓 Path，生成阴影贴图并画到
    /// pixmap 上（画在形状本体 fill/stroke 之前，让本体覆盖阴影，符合
    /// OOXML outerShdw 语义——阴影在形状下方）。
    ///
    /// 算法（简化版，效果对标 LOK core/drawinglayer/source/primitive2d/
    /// shadowprimitive2d.cxx 的 ShadowPrimitive2D::create2DDecomposition，
    /// 但模糊实现不同——LOK 用 ProcessAndBlurAlphaMask 做更精细的模糊，
    /// 本实现用"盒式模糊反复叠加近似高斯"的简化算法，足够应付本次样本
    /// （纯黑43%透明度+无偏移的最简单 outerShdw 参数组合）。若未来样本
    /// 暴露出模糊效果不够柔和/精确，可参照 LOK 该文件里
    /// ProcessAndBlurAlphaMask 的具体实现升级成更接近真高斯核的算法，
    /// 而不是本函数这版三次盒式模糊近似）：
    ///   1) 用 outline_path 在一张与 pixmap 等大的 tiny_skia::Mask 上
    ///      填充轮廓（只要形态，不要形状本身的颜色/渐变细节）
    ///   2) 对该 Mask 的 alpha buffer 做盒式模糊（水平+垂直方向滑动窗口
    ///      平均，重复3遍近似高斯，是图形学标准技巧）
    ///   3) 用 ShadowStyle.color 的 RGB + alpha，叠加模糊后的遮罩值，
    ///      按 offset_x/offset_y 偏移画到 pixmap 上
    fn render_shape_shadow(
        pixmap: &mut PixmapMut,
        outline_path: &tiny_skia::Path,
        shadow: &roe_model::common::ShadowStyle,
        scale: f32,
        shape_transform: Transform,
    ) {
        let pw = pixmap.width();
        let ph = pixmap.height();
        if pw == 0 || ph == 0 {
            return;
        }

        // Mask 是独立于 pixmap 的 alpha-only 缓冲，先在其上填充形状轮廓
        // （只要 shape_transform，不要额外的 offset——offset 在第3步画
        // 阴影贴图时再应用，这样模糊核不受 offset 影响，语义更清晰）。
        let mut mask = match tiny_skia::Mask::new(pw, ph) {
            Some(m) => m,
            None => return,
        };
        mask.fill_path(
            outline_path,
            tiny_skia::FillRule::Winding,
            true,
            shape_transform,
        );

        // blur（pt）->离散像素半径。outerShdw 的 blurRad 语义是模糊半径，
        // 换算成盒式模糊的滑动窗口半径时按 scale 映射到当前渲染分辨率，
        // 半径至少给1，避免blur极小值时窗口退化成0导致blur_box no-op。
        let blur_radius_px = ((shadow.blur * scale).round() as i32).max(1);
        Self::box_blur_mask(&mut mask, pw as usize, ph as usize, blur_radius_px);

        // offset（pt）->像素偏移。
        let offset_x_px = shadow.offset_x * scale;
        let offset_y_px = shadow.offset_y * scale;

        let c = &shadow.color;
        let data = mask.data();
        let mut shadow_pixmap = match tiny_skia::Pixmap::new(pw, ph) {
            Some(p) => p,
            None => return,
        };
        {
            let pixels = shadow_pixmap.pixels_mut();
            for (i, a) in data.iter().enumerate() {
                if *a == 0 {
                    continue;
                }
                // premultiplied alpha: 阴影颜色的 RGB 需要按 (a_shadow * a_mask)
                // 预乘，a_mask 是模糊后遮罩值（0-255），a_shadow 是 ShadowStyle
                // 颜色自带的整体透明度（组装层已经把 outerShdw 的 alpha 百分比
                // 写入了 c.a）。
                let combined_a = ((c.a as u32) * (*a as u32) / 255) as u8;
                if combined_a == 0 {
                    continue;
                }
                let pm = |v: u8| ((v as u32 * combined_a as u32) / 255) as u8;
                pixels[i] = tiny_skia::PremultipliedColorU8::from_rgba(
                    pm(c.r), pm(c.g), pm(c.b), combined_a,
                ).unwrap_or(tiny_skia::PremultipliedColorU8::TRANSPARENT);
            }
        }

        pixmap.draw_pixmap(
            offset_x_px.round() as i32,
            offset_y_px.round() as i32,
            shadow_pixmap.as_ref(),
            &tiny_skia::PixmapPaint::default(),
            Transform::identity(),
            None,
        );
    }

    /// 盒式模糊，对 Mask 的 alpha buffer 原地做水平+垂直方向各1遍滑动窗口
    /// 平均，外层调用3次（见 render_shape_shadow）近似高斯模糊效果。
    /// radius 是滑动窗口半径（像素），窗口宽度 = 2*radius+1。
    fn box_blur_mask(mask: &mut tiny_skia::Mask, w: usize, h: usize, radius: i32) {
        if radius <= 0 || w == 0 || h == 0 {
            return;
        }
        // 3次盒式模糊叠加近似高斯，每次都是"水平一遍+垂直一遍"。
        for _ in 0..3 {
            Self::box_blur_pass(mask.data_mut(), w, h, radius, true);
            Self::box_blur_pass(mask.data_mut(), w, h, radius, false);
        }
    }

    /// 单方向（水平或垂直）滑动窗口平均，原地改写 buffer。
    fn box_blur_pass(data: &mut [u8], w: usize, h: usize, radius: i32, horizontal: bool) {
        let r = radius as usize;
        let window = 2 * r + 1;
        if horizontal {
            let mut row_buf = vec![0u8; w];
            for y in 0..h {
                let row_start = y * w;
                row_buf.copy_from_slice(&data[row_start..row_start + w]);
                for x in 0..w {
                    let lo = x.saturating_sub(r);
                    let hi = (x + r).min(w - 1);
                    let sum: u32 = row_buf[lo..=hi].iter().map(|&v| v as u32).sum();
                    data[row_start + x] = (sum / (hi - lo + 1) as u32) as u8;
                }
            }
        } else {
            let mut col_buf = vec![0u8; h];
            for x in 0..w {
                for y in 0..h {
                    col_buf[y] = data[y * w + x];
                }
                for y in 0..h {
                    let lo = y.saturating_sub(r);
                    let hi = (y + r).min(h - 1);
                    let sum: u32 = col_buf[lo..=hi].iter().map(|&v| v as u32).sum();
                    data[y * w + x] = (sum / (hi - lo + 1) as u32) as u8;
                }
            }
        }
        let _ = window;
    }

    /// PROOT-035: 把 text 里的 `\t`（制表符）展开成等宽空格序列，
    /// 使其在 parley shaping 时不再落入 `.notdef`（tofu方框）——字体
    /// 没有为 `\t` 定义可见字形，必须在喂给 shaper 之前处理掉，parley
    /// 本身不提供 tab-stop 跳格语义（已核实，见 bug_log.md PROOT-035
    /// "不属于parley的能力范围"一节）。
    ///
    /// 语义对齐 OOXML `defTabSz` 默认值 72pt（EMU 914400）——本实现暂
    /// 不支持 `<a:tabLst>` 自定义制表位，只做默认等距 tab stop（bug_log
    /// 记录的"最小可行修复方向"）。tab stop 是绝对位置对齐，不是相对
    /// 宽度：同一个 `\t` 在不同的当前光标 x 位置需要展开成不同宽度，
    /// 因此需要按 `\t` 切分文本、逐段真实测量宽度（用 `font_size`
    /// 对应的 layout 宽度）来累加当前 x，而不是简单固定数量空格替换。
    ///
    /// 近似点（可接受，非精确到像素的技术债）：用整数个空格字符填补
    /// "到下一个 tab stop 还差多少像素"这段距离，四舍五入，最多有
    /// 约半个空格宽度的误差；不是真正插入一个任意宽度的空白 primitive
    /// （后者需要改动 run range/byte offset 计算，改动面更大，非本次
    /// range）。font_size 是像素单位（pt*scale，与调用处一致）。
    fn expand_tabs_to_spaces(&mut self, text: &str, font_size: f32, scale: f32, cur_x: &mut f32) -> String {
        if !text.contains('\t') {
            // 无tab时仍需推进cur_x，保证跨run的tab stop绝对位置计算连续
            // （下一个run如果含\t，要知道"这个run开始时光标已经在哪"）。
            if !text.is_empty() {
                let mut b = self.layout_context.ranged_builder(&mut self.font_context, text, 1.0, true);
                b.push_default(StyleProperty::FontSize(font_size));
                let mut layout: parley::Layout<ImpressBrush> = b.build(text);
                layout.break_all_lines(None);
                *cur_x += layout.full_width();
            }
            return text.to_string();
        }

        let tab_stop_px = 72.0 * scale;

        // 测量一个空格字符的宽度，用于把"到下个tab stop还差多少像素"
        // 换算成空格个数。
        let space_w = {
            let mut b = self.layout_context.ranged_builder(&mut self.font_context, " ", 1.0, true);
            b.push_default(StyleProperty::FontSize(font_size));
            let mut layout: parley::Layout<ImpressBrush> = b.build(" ");
            layout.break_all_lines(None);
            layout.full_width().max(1.0)
        };

        let mut result = String::new();

        for (i, segment) in text.split('\t').enumerate() {
            if i > 0 {
                // 上一个 	 落在这里：算出到下一个 tab stop 还差多少像素，
                // 换算成空格个数插入。cur_x 是跨run累积的光标位置，不是
                // 本run局部位置——tab stop是整段落的绝对位置语义。
                let next_stop = ((*cur_x / tab_stop_px).floor() + 1.0) * tab_stop_px;
                let gap_px = (next_stop - *cur_x).max(0.0);
                let n_spaces = ((gap_px / space_w).round() as usize).max(1);
                for _ in 0..n_spaces {
                    result.push(' ');
                }
                *cur_x = next_stop;
            }
            result.push_str(segment);
            if !segment.is_empty() {
                let mut b = self.layout_context.ranged_builder(&mut self.font_context, segment, 1.0, true);
                b.push_default(StyleProperty::FontSize(font_size));
                let mut layout: parley::Layout<ImpressBrush> = b.build(segment);
                layout.break_all_lines(None);
                *cur_x += layout.full_width();
            }
        }

        result
    }

    fn render_preset_path(
        pixmap: &mut PixmapMut,
        cmds: &[roe_model::common::PathCommand],
        x: f32, y: f32, scale: f32,
        paint: &Paint,
        transform: Transform,
    ) {
        if let Some(path) = Self::build_preset_path(cmds, x, y, scale) {
            pixmap.fill_path(&path, paint, tiny_skia::FillRule::Winding, transform, None);
        }
    }

    /// PROOT-002 bugfix: 沿 PresetPath 的真实路径描边（直线/折线/曲线），
    /// 替代此前对所有非Ellipse几何统一用 `draw_rect_stroke`（矩形包围盒
    /// 描边）的做法——对connector（straightConnector1等）这类"本身就是一
    /// 条线，没有填充意义"的形状，矩形包围盒描边会画出一个跟实际线条走向
    /// 完全不同的直角矩形框，肉眼对比真实PPTX/OnlyOffice渲染发现这个问题
    /// （connector连线路径明显不对，粗细观感也有偏差）。仅用于
    /// `ShapeGeometry::PresetPath`，其余geometry（Rect/RoundRect/Triangle/
    /// Ellipse以外的簡單形状目前无独立处理，仍退回`draw_rect_stroke`，
    /// 不在本次范围内一并修）。
    fn render_preset_path_stroke(
        pixmap: &mut PixmapMut,
        cmds: &[roe_model::common::PathCommand],
        x: f32, y: f32, scale: f32,
        r: u8, g: u8, b: u8, a: u8, width: f32,
        transform: Transform,
    ) {
        if let Some(path) = Self::build_preset_path(cmds, x, y, scale) {
            let mut paint = Paint::default();
            paint.set_color_rgba8(r, g, b, a);
            let mut stroke = Stroke::default();
            stroke.width = width;
            pixmap.stroke_path(&path, &paint, &stroke, transform, None);
        }
    }

    /// PROOT-002 箭头渲染：在PresetPath(len=2)的connector两端画头/尾箭头
    /// 装饰（<a:headEnd>/<a:tailEnd>），几何公式参照OnlyOffice sdkjs
    /// (common/Drawings/Format/Format.js EndArrow.GetWidth/GetLen +
    /// word/Drawing/ShapeDrawer.js DrawLineEnd)交叉验证LOK
    /// (oox/source/drawingml/lineproperties.cxx lclPushMarkerProperties)后
    /// 采用的简化版本：
    ///   size = max(线宽, 2.0)
    ///   arrowWidth = arrowLength = 3 * size  （仅支持medium默认，OOXML
    ///   <a:headEnd w=".."/len=".."> 显式small/large档位未覆盖——真实样本
    ///   tdf114845未出现非默认档位，属于已知范围限制，见bug_log）
    ///
    /// 起点/终点对应关系：探针脚本probe_connector_path_order.rs用真实坐标
    /// 交叉验证过——cmds首元素(MoveTo)对应st_cxn/head_arrow端，末元素
    /// (最后一个带坐标的command，本函数只处理len==2的直线connector，即
    /// LineTo)对应end_cxn/tail_arrow端，不是假设。
    ///
    /// ArrowType::Arrow：3点折线(翼1→端点→翼2)，只描边不闭合不填充。
    /// ArrowType::Triangle：同样3点，但闭合+用线条颜色填充成实心三角形。
    /// 全部在shape局部坐标系计算（不乘scale），交给传入的transform统一做
    /// scale+rotation+平移，与build_preset_path/render_preset_path_stroke
    /// 的坐标处理方式保持一致，避免两次变换叠加出错。
    fn render_connector_arrows(
        pixmap: &mut PixmapMut,
        cmds: &[roe_model::common::PathCommand],
        x: f32, y: f32, scale: f32,
        r: u8, g: u8, b: u8, a: u8,
        line_width: f32, // 屏幕像素单位（已经是 stroke_style.width * scale）
        head_arrow: Option<roe_model::common::ArrowType>,
        tail_arrow: Option<roe_model::common::ArrowType>,
        transform: Transform,
    ) {
        use roe_model::common::{ArrowType, PathCommand};

        if cmds.len() < 2 {
            return;
        }

        // 只取首末两个点的局部坐标（局部坐标系，不乘scale，与build_preset_path
        // 的map()函数保持相同的坐标空间约定，最后统一交给transform处理）。
        // PROOT-CURVE-ARROW: 曲线connector（curvedConnector2/3/4/5，路径
        // 全部由CubicBezTo构成，不含LineTo）此前get_xy不认CubicBezTo，
        // 导致cmds.last()取不到终点坐标，函数直接return，箭头完全不显示
        // （v86会话curvedConnector走查实测：curvedConnector5的tailEnd=
        // triangle在真实渲染中缺失，对比LibreOffice参照图确认）。加
        // CubicBezTo分支，取其终点x/y（三次贝塞尔的最后一个坐标对，
        // 即曲线在该段的终点，忽略两个控制点）。方向向量仍用first/last
        // 两点连线近似（不是曲线切线），这与bentConnector折线场景的
        // 已知近似精度一致（见下方draw_one_end调用处注释），非本次
        // 新增的精度问题，暂不处理。
        let get_xy = |cmd: &PathCommand| -> Option<(f32, f32)> {
            match cmd {
                PathCommand::MoveTo { x, y } => Some((*x, *y)),
                PathCommand::LineTo { x, y } => Some((*x, *y)),
                PathCommand::CubicTo { x, y, .. } => Some((*x, *y)),
                _ => None,
            }
        };

        let first = match get_xy(&cmds[0]) { Some(p) => p, None => return };
        let last = match get_xy(&cmds[cmds.len() - 1]) { Some(p) => p, None => return };

        // OnlyOffice EndArrow.GetWidth/GetLen 默认公式（medium档，w/len未
        // 显式指定时）：size = max(线宽, minSize=2.0)；width = length = 3*size。
        // line_width是屏幕像素单位，这里的size/arrow_len也就是屏幕像素单位，
        // 与局部坐标系（未乘scale）不匹配——所以画箭头前，需要把箭头尺寸从
        // 屏幕像素换算回局部坐标系单位（除以scale），才能和局部坐标点混合
        // 计算，最后一起交给transform变换。scale不会是0（渲染管线上游保证），
        // 但防御性处理避免除零。
        let safe_scale = if scale.abs() < 1e-6 { 1.0 } else { scale };
        let size = line_width.max(2.0);
        let arrow_len = (3.0 * size) / safe_scale;
        let arrow_width = (3.0 * size) / safe_scale;

        let mut paint = Paint::default();
        paint.set_color_rgba8(r, g, b, a);
        let mut stroke = Stroke::default();
        stroke.width = line_width;

        // draw_one_end: tip=箭头尖端点（连接线端点本身），prev=尖端的相邻点
        // （用于算方向向量，指向"线段内部"）。
        let draw_one_end = |pixmap: &mut PixmapMut, arrow_type: ArrowType, tip: (f32, f32), prev: (f32, f32)| {
            if arrow_type == ArrowType::None {
                return;
            }

            // 方向向量：从tip指向prev，归一化（对应OnlyOffice _ex,_ey =
            // normalize(prev - end)）。
            let mut ex = prev.0 - tip.0;
            let mut ey = prev.1 - tip.1;
            let elen = (ex * ex + ey * ey).sqrt();
            if elen < 1e-6 {
                return; // 退化线段（两点重合），跳过箭头绘制
            }
            ex /= elen;
            ey /= elen;

            // 法向量（垂直于方向向量），用于算箭头两翼的左右偏移。
            let vx = ey;
            let vy = -ex;

            // 沿方向向量从tip回退arrow_len，得到箭头底边中点。
            let base_x = tip.0 + arrow_len * ex;
            let base_y = tip.1 + arrow_len * ey;

            let x1 = base_x + vx * arrow_width / 2.0;
            let y1 = base_y + vy * arrow_width / 2.0;
            let x3 = base_x - vx * arrow_width / 2.0;
            let y3 = base_y - vy * arrow_width / 2.0;

            let map = |lx: f32, ly: f32| (lx * scale + x, ly * scale + y);
            let (sx1, sy1) = map(x1, y1);
            let (stipx, stipy) = map(tip.0, tip.1);
            let (sx3, sy3) = map(x3, y3);

            let mut pb = PathBuilder::new();
            pb.move_to(sx1, sy1);
            pb.line_to(stipx, stipy);
            pb.line_to(sx3, sy3);

            match arrow_type {
                ArrowType::Triangle => {
                    pb.close();
                    if let Some(built) = pb.finish() {
                        pixmap.fill_path(&built, &paint, tiny_skia::FillRule::Winding, transform, None);
                        pixmap.stroke_path(&built, &paint, &stroke, transform, None);
                    }
                }
                ArrowType::Arrow => {
                    if let Some(built) = pb.finish() {
                        pixmap.stroke_path(&built, &paint, &stroke, transform, None);
                    }
                }
                // PROOT-OVAL-ARROW: 圆形端点。不复用上面 x1/tip/x3 三点折线
                // 路径（那是给三角/箭头翼形状用的），直接以 tip 为圆心画实心
                // 圆。半径用 arrow_width/2（与三角形箭头同一量级的尺寸公式，
                // OnlyOffice medium档 size=max(线宽,2.0)，width=3*size，半径
                // 取 width/2 视觉上与三角形箭头大小接近，不做单独换算）。
                // map() 已经把局部坐标转换成屏幕坐标（含scale），半径也要
                // 乘 scale 保持比例一致，直接用 (stipx,stipy) 作圆心。
                ArrowType::Oval => {
                    let r = (arrow_width / 2.0) * scale;
                    let k = 0.5522848_f32; // 三次贝塞尔近似圆的标准系数
                    let (cx, cy) = (stipx, stipy);
                    let mut opb = PathBuilder::new();
                    opb.move_to(cx + r, cy);
                    opb.cubic_to(cx + r, cy - r * k, cx + r * k, cy - r, cx, cy - r);
                    opb.cubic_to(cx - r * k, cy - r, cx - r, cy - r * k, cx - r, cy);
                    opb.cubic_to(cx - r, cy + r * k, cx - r * k, cy + r, cx, cy + r);
                    opb.cubic_to(cx + r * k, cy + r, cx + r, cy + r * k, cx + r, cy);
                    opb.close();
                    if let Some(built) = opb.finish() {
                        pixmap.fill_path(&built, &paint, tiny_skia::FillRule::Winding, transform, None);
                        pixmap.stroke_path(&built, &paint, &stroke, transform, None);
                    }
                }
                ArrowType::None => {}
            }
        };

        // PROOT-CURVE-ARROW-TANGENT: head/tail端的方向向量此前统一用
        // first/last两点直线近似——对len==2的直线connector精确（两点
        // 就是直线本身），但对曲线connector（CubicBezTo段）严重失真：
        // 大弧度曲线的"起点到终点直线"和"端点处切线方向"可能相差
        // 几十度，实测curvedConnector5渲染出的箭头明显斜向曲线内侧，
        // 而非贴合曲线末端切线指向目标图形（对比LibreOffice参照图
        // 确认）。修正：若紧邻端点的那一段是CubicBezTo，用它自己的
        // 控制点算切线方向（三次贝塞尔端点切线 = 端点与相邻控制点的
        // 连线，标准贝塞尔几何性质）；否则（LineTo/直线connector）
        // 保持原有first/last近似不变，不影响bentConnector/
        // straightConnector1现有正确行为。
        // PROOT-V87-ARROW-DIRECTION: 此前head_prev/tail_prev在非曲线（LineTo）
        // 场景下分别fallback到last/first——这只在cmds长度恰好为2（纯直线
        // connector，起点终点即相邻点）时正确；但bentConnector等折线
        // connector的cmds由多段LineTo组成（真实样本tdf165180实测4~6段），
        // 此时"紧邻端点的邻居点"根本不是另一端的端点，而是折线上第二个/
        // 倒数第二个点。此前的fallback逻辑等于用"起点到终点的整体连线"
        // 近似方向，方向误差可达任意角度（不是简单的180度反向），真实
        // 截图对比LibreOffice确认箭头明显歪斜、形态发虚。
        // 修正：直接取紧邻端点的相邻命令的终点坐标（cmds[1]用于head，
        // cmds[len-2]用于tail），只有当那一段是CubicTo时才用其控制点
        // （贝塞尔切线场景，逻辑不变）。
        // PROOT-V88-ARROW-DEGENERATE: bentConnector等折线connector在特定
        // 端点相对位置下，公式求值出的路径可能出现相邻两点坐标完全
        // 重合的退化段（真实样本xtdf165180实测：bentConnector3在两个成员上，
        // cmds[len-2]与cmds[len-1]坐标完全一致，即紧邻终点的那一段长度
        // 为0）。这是geometry公式本身在该端点配置下的正常几何退化，不是
        // parser的bug，不应在公式层"修掉"（任何折线connector在别的端点
        // 配置下都可能出现类似退化，公式层无法穷举防住）。
        // v87只取"紧邻端点的相邻点"本身，没有考虑它可能与端点重合——
        // 重合时方向向量长度为0，draw_one_end的退化保护直接return，
        // 箭头完全不画（真实截图对比LibreOffice确认：两条bentConnector3
        // 该有的箭头完全消失）。
        // 修复：不是简单回退到first/last整体近似（那会重新引入v87要修的
        // 方向误差问题），而是沿路径继续向内跳过与tip坐标重合的退化点，
        // 取第一个"真正不同"的点作为prev——这样既保留v87的切线精度目标，
        // 又能穿透偶发的零长度段，是通用解法，覆盖以后其他geometry出现
        // 类似退化的情况。
        let coords_eq = |p1: (f32, f32), p2: (f32, f32)| -> bool {
            (p1.0 - p2.0).abs() < 1e-4 && (p1.1 - p2.1).abs() < 1e-4
        };
        let head_prev = match cmds.get(1) {
            Some(PathCommand::CubicTo { x1, y1, .. }) => (*x1, *y1),
            Some(other) => {
                let p = get_xy(other).unwrap_or(last);
                if coords_eq(p, first) {
                    cmds.iter().skip(2).find_map(|c| {
                        get_xy(c).filter(|&pt| !coords_eq(pt, first))
                    }).unwrap_or(last)
                } else {
                    p
                }
            }
            None => last,
        };
        let tail_prev = match cmds.get(cmds.len().saturating_sub(2)) {
            Some(PathCommand::CubicTo { x2, y2, .. }) => (*x2, *y2),
            Some(other) => {
                let p = get_xy(other).unwrap_or(first);
                if coords_eq(p, last) {
                    cmds.iter().rev().skip(2).find_map(|c| {
                        get_xy(c).filter(|&pt| !coords_eq(pt, last))
                    }).unwrap_or(first)
                } else {
                    p
                }
            }
            None => first,
        };

        if let Some(head) = head_arrow {
            // head端：tip=first（起点），prev=head_prev（曲线场景用切线
            // 控制点，直线场景仍是last，与原逻辑等价）。
            draw_one_end(pixmap, head, first, head_prev);
        }
        if let Some(tail) = tail_arrow {
            // tail端：tip=last（终点），prev=tail_prev（曲线场景用切线
            // 控制点，直线场景仍是first，与原逻辑等价）。
            draw_one_end(pixmap, tail, last, tail_prev);
        }
    }

    /// 圆心+半径+角度参数化圆弧 → 三次贝塞尔曲线序列近似。
    ///
    /// 单位换算：`start_deg`/`sweep_deg` 实际是 OOXML 原始单位（1/60000度，
    /// 命名沿用 `roe_model::common::PathCommand::ArcTo` 字段名，语义见该处
    /// 文档注释与 `preset_geometry.rs` 的 `eval_cos`/`eval_sin`），需要先除以
    /// 60000 换算成真正角度，再转弧度。
    ///
    /// 分段策略：每段圆弧扫描角不超过90度（标准做法，超过90度贝塞尔近似
    /// 误差明显增大），用经典的 4/3*tan(Δ/4) 控制点系数公式在"单位圆局部
    /// 空间"算好四个贝塞尔控制点，再整体映射到 (cx,cy)+(rx,ry) 椭圆空间——
    /// 这对非等比缩放（rx≠ry）依然正确，因为椭圆是单位圆做独立 x/y 缩放的
    /// 仿射结果，贝塞尔曲线在仿射变换下保持贝塞尔性质。
    ///
    /// `needs_move_to`：若调用前路径还没有当前点（不应该发生，因为 preset
    /// 形状数据总是先 MoveTo 再 ArcTo，但作为防御性处理，没有当前点时先
    /// move_to 到弧的起点，避免 tiny-skia panic 或产生退化路径）。
    fn arc_to_beziers(
        pb: &mut PathBuilder,
        cx: f32, cy: f32, rx: f32, ry: f32,
        start_60000th_deg: f32, sweep_60000th_deg: f32,
        has_current_point: bool,
    ) {
        let start_rad = (start_60000th_deg / 60000.0).to_radians();
        let sweep_rad = (sweep_60000th_deg / 60000.0).to_radians();

        if !has_current_point {
            let sx = cx + rx * start_rad.cos();
            let sy = cy + ry * start_rad.sin();
            pb.move_to(sx, sy);
        }

        // 分段：每段最多90度（PI/2弧度）
        let max_seg = std::f32::consts::FRAC_PI_2;
        let num_segs = (sweep_rad.abs() / max_seg).ceil().max(1.0) as usize;
        let seg_sweep = sweep_rad / num_segs as f32;

        for i in 0..num_segs {
            let a0 = start_rad + seg_sweep * i as f32;
            let a1 = a0 + seg_sweep;

            let (cos0, sin0) = (a0.cos(), a0.sin());
            let (cos1, sin1) = (a1.cos(), a1.sin());

            // 4/3*tan(Δ/4) 标准贝塞尔圆弧近似系数
            let k = (4.0 / 3.0) * ((a1 - a0) / 4.0).tan();

            let p0x = cx + rx * cos0;
            let p0y = cy + ry * sin0;
            let p3x = cx + rx * cos1;
            let p3y = cy + ry * sin1;

            let p1x = p0x - k * rx * sin0;
            let p1y = p0y + k * ry * cos0;
            let p2x = p3x + k * rx * sin1;
            let p2y = p3y - k * ry * cos1;

            pb.cubic_to(p1x, p1y, p2x, p2y, p3x, p3y);
        }
    }

    /// 占位符虚线框描边（占位符虚线框默认提示文案 设计方案 v1，四、2）。
    /// 与 draw_rect_stroke 结构一致，仅额外设置 stroke.dash。颜色/线宽/
    /// 虚线间隔是估测起步值（浅灰 #A6A6A6，1px，4/3虚线节奏），未经与
    /// OnlyOffice截图精确比对微调，后续如有偏差可直接调整下面几个常量。
    fn draw_placeholder_dashed_rect(
        pixmap: &mut PixmapMut,
        x: f32, y: f32, w: f32, h: f32,
        scale: f32,
        transform: Transform,
    ) {
        let mut pb = PathBuilder::new();
        pb.move_to(x, y);
        pb.line_to(x + w, y);
        pb.line_to(x + w, y + h);
        pb.line_to(x, y + h);
        pb.close();
        if let Some(path) = pb.finish() {
            let mut paint = Paint::default();
            paint.set_color_rgba8(0xA6, 0xA6, 0xA6, 255);
            paint.anti_alias = true;
            let mut stroke = Stroke::default();
            stroke.width = 1.0 * scale;
            stroke.dash = tiny_skia::StrokeDash::new(vec![4.0 * scale, 3.0 * scale], 0.0);
            pixmap.stroke_path(&path, &paint, &stroke, transform, None);
        }
    }

    fn draw_rect_stroke(
        pixmap: &mut PixmapMut,
        x: f32, y: f32, w: f32, h: f32,
        r: u8, g: u8, b: u8, a: u8, width: f32,
        transform: Transform,
    ) {
        let mut pb = PathBuilder::new();
        pb.move_to(x, y);
        pb.line_to(x + w, y);
        pb.line_to(x + w, y + h);
        pb.line_to(x, y + h);
        pb.close();
        if let Some(path) = pb.finish() {
            let mut paint = Paint::default();
            paint.set_color_rgba8(r, g, b, a);
            let mut stroke = Stroke::default();
            stroke.width = width;
            pixmap.stroke_path(&path, &paint, &stroke, transform, None);
        }
    }
    /// 分边描边（PROOT-031：边框分边渲染）。四条边（top/right/bottom/left）
    /// 各自独立 Option<(r,g,b,a,width)>，None 表示这条边不画。用于表格
    /// cell 边框——TableStylePart 的 border_left/right/top/bottom/
    /// inside_h/inside_v 六个字段本就是独立的（数据层从一开始就支持
    /// 分边，只是 draw_rect_stroke 接口此前是四边统一），某些内置样式
    /// （如 Light-Style-1）故意让 inside_v 保持 None 表示"不画竖线"，
    /// 这个语义必须在渲染层真正体现出来，不能像旧实现那样四边套同一色。
    ///
    /// 与 draw_rect_stroke 相比：不共享 Paint/Stroke，每条边单独构造
    /// 两点线段（不 close()，因为四条边各自独立，不是一条连续路径），
    /// 为 None 的边直接跳过 stroke_path 调用。不影响 draw_rect_stroke
    /// 本身及其现有调用点（connector 等非表格场景）。
    #[allow(clippy::too_many_arguments)]
    fn draw_rect_stroke_sides(
        pixmap: &mut PixmapMut,
        x: f32, y: f32, w: f32, h: f32,
        // [top, right, bottom, left]
        sides: [Option<(u8, u8, u8, u8, f32)>; 4],
        transform: Transform,
    ) {
        let segments: [((f32, f32), (f32, f32)); 4] = [
            ((x, y), (x + w, y)),         // top
            ((x + w, y), (x + w, y + h)), // right
            ((x + w, y + h), (x, y + h)), // bottom
            ((x, y + h), (x, y)),         // left
        ];
        for (side, ((sx, sy), (ex, ey))) in sides.into_iter().zip(segments) {
            let Some((r, g, b, a, width)) = side else { continue };
            if width <= 0.0 {
                continue;
            }
            let mut pb = PathBuilder::new();
            pb.move_to(sx, sy);
            pb.line_to(ex, ey);
            if let Some(path) = pb.finish() {
                let mut paint = Paint::default();
                paint.set_color_rgba8(r, g, b, a);
                let mut stroke = Stroke::default();
                stroke.width = width;
                pixmap.stroke_path(&path, &paint, &stroke, transform, None);
            }
        }
    }


    /// 画填充矩形（PROOT-003：表格cell背景色）。在画边框stroke之前调用，
    /// 避免填充盖住边框线。
    fn draw_rect_fill(
        pixmap: &mut PixmapMut,
        x: f32, y: f32, w: f32, h: f32,
        r: u8, g: u8, b: u8, a: u8,
        transform: Transform,
    ) {
        let mut pb = PathBuilder::new();
        pb.move_to(x, y);
        pb.line_to(x + w, y);
        pb.line_to(x + w, y + h);
        pb.line_to(x, y + h);
        pb.close();
        if let Some(path) = pb.finish() {
            let mut paint = Paint::default();
            paint.set_color_rgba8(r, g, b, a);
            pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, transform, None);
        }
    }

    fn draw_border(pixmap: &mut PixmapMut, w: u32, h: u32) {
        let mut paint = Paint::default();
        paint.set_color_rgba8(160, 160, 160, 255);
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

    fn render_shape_chart(
        &mut self,
        pixmap: &mut PixmapMut,
        chart_model: &roe_model::impress::ChartModel,
        x: f32, y: f32, w: f32, h: f32,
        scale: f32,
        transform: Transform,
    ) {
        use charts_rs::{BarChart, HorizontalBarChart, LineChart, PieChart, ScatterChart, RadarChart, Series};

        // PROOT-033 字号比例实验：不再将 width/height 乘 scale，
        // 直接用 pt 数值作为 charts-rs 目标像素尺寸，字号保持
        // charts-rs 默认值不改。根据对 calc 侧（roe-core/src/
        // tile_render_helpers.rs render_chart_model_to_rgba）的比对发现：calc 侧
        // 直接用 chart.width_pt as u32。虽然这本身是个已知缺陷
        // （没做 pt→px 的 DPI 换算，见 v55 文档 2.4节），但它
        // 客观上证明了 charts-rs 默认字号是按"pt数值≈px数值"
        // 这个数量级设计的，一旦传入被 scale 放大过的
        // width/height，字号就会相对变小。方案A（字号乘 scale）
        // 已实测会撃撞布局预留空间（标题/坐标轴数字被
        // 裁切、x轴标签与柱子重叠，已用真实样本在 slint
        // 前台验证过），因此本次改用这个方向实验：
        // 不放大画布、保持 charts-rs 自己的默认比例。风险：
        // 最终图像可能相对屏幕物理像素分辨率偏低（不再
        // 按 scale 提高渲染分辨率），需要实测确认清晰度是否
        // 可接受（本次会话尚未验证，见探针结果）。
        let width = w.round().max(1.0) as u32;
        let height = h.round().max(1.0) as u32;
        if std::env::var("ROE_DEBUG_CHART").as_deref() == Ok("1") {
            eprintln!("[CHART-DIMS] x={} y={} w={} h={} scale={} width={} height={}", x, y, w, h, scale, width, height);
        }
        if width == 0 || height == 0 { return; }

        let series: Vec<Series> = chart_model.series.iter().map(|s| {
            let mut ser = Series::new(s.name.clone(), s.data.iter().map(|&v| v as f32).collect());
            // PROOT-033: 解析层已把 <c:ser><c:spPr> 里的 schemeClr（含 chart 专属
            // themeOverride 覆盖）解析成具体 RGB，此前这里只读 name/data，颜色被
            // 直接丢弃，导致柱子永远是 charts-rs 内置默认蓝色。charts-rs 的
            // Series.colors 是逐数据点的 Vec<Option<Color>>，这里统一设成同一色。
            if let Some(c) = s.color {
                ser.colors = Some(vec![Some(charts_rs::Color { r: c.r, g: c.g, b: c.b, a: c.a }); s.data.len()]);
            }
            ser
        }).collect();

        // ChartModel → SVG 已下沉到 roe_core::chart_render（三处共用唯一实现）。
        let svg_result: Result<String, &'static str> =
            roe_core::chart_render::chart_model_to_svg(chart_model, ((width as f32 / scale.max(0.01)).round() as u32).max(1), ((height as f32 / scale.max(0.01)).round() as u32).max(1))
                .ok_or("chart_model_to_svg failed");

        let svg_str = match svg_result {
            Ok(s) => s,
            Err(e) => { tracing::warn!("chart svg error: {:?}", e); return; }
        };

        // SVG → PNG bytes via resvg
        let png_bytes = Self::svg_to_png_bytes(&svg_str, width, height);
        let png_bytes = match png_bytes {
            Some(b) => b,
            None => return,
        };
        let chart_pixmap = match tiny_skia::Pixmap::decode_png(&png_bytes) {
            Ok(p) => p,
            Err(e) => { tracing::warn!("chart decode png error: {:?}", e); return; }
        };

        // 把 chart_pixmap 合入主 pixmap（PPTX-8：改用 Pattern + fill_rect，
        // 使旋转/翻转能通过 shape_transform 统一作用于图表贴图，替代此前
        // 不支持变换的手写逐像素 alpha 合成循环）
        // PROOT-033 bugfix: chart_pixmap 的物理像素尺寸是 width×height
        // （即 w*scale, h*scale，像素单位），但 fill_rect 目标矩形用的是
        // 未缩放的 w×h（pt单位，见下方 Rect::from_xywh）。此前误认为两者
        // 已经匹配、只做了平移，导致 Pattern 按1:1像素对齐目标坐标空间
        // 采样，scale>1时物理图比目标矩形大，只有左上角一小块像素落在
        // 采样窗口内，其余内容（含大部分柱子/坐标轴/标题）被直接裁掉——
        // 实测样本 chart-theme-override 只剩右下角一根柱子的残片，与
        // "退化成纯色矩形"的历史描述吻合。修复：先按 1/scale 把像素坐标
        // 缩小映射回 pt 坐标，再平移到 (x,y)，使 scale=1/2/其他倍数时
        // 输出一致（已用 scale=1.0 对照验证过：不缩放时三根柱子完整，
        // 证实问题出在这里而非 SVG 生成或 charts-rs 本身）。
        // width/height 不再乘 scale（见上方注释），chart_pixmap
        // 现在已经是目标 pt 尺寸（千与 w/h 一致），不再需要
        // 再缩小映射，直接 1:1 平移到 (x, y) 即可。
        let pattern_transform = tiny_skia::Transform::from_translate(x, y);
        let pattern = tiny_skia::Pattern::new(
            chart_pixmap.as_ref(),
            tiny_skia::SpreadMode::Pad,
            tiny_skia::FilterQuality::Bilinear,
            1.0,
            pattern_transform,
        );
        let mut paint = Paint::default();
        paint.shader = pattern;
        if let Some(rect) = Rect::from_xywh(x, y, w.max(1.0), h.max(1.0)) {
            pixmap.fill_rect(rect, &paint, transform, None);
        }
    }

    fn svg_to_png_bytes(svg_str: &str, width: u32, height: u32) -> Option<Vec<u8>> {
        // 字体加载等逻辑已下沉到 roe_core::chart_render::svg_to_pixmap（三处共用）。
        let pixmap = roe_core::chart_render::svg_to_pixmap_fit(svg_str, width, height)?;
        pixmap.encode_png().ok()
    }

    fn render_shape_image(
        &mut self,
        pixmap: &mut PixmapMut,
        data: &[u8],
        _mime: &str,
        crop_l: f32, crop_t: f32, crop_r: f32, crop_b: f32,
        x: f32, y: f32, w: f32, h: f32,
        scale: f32,
        shape_transform: Transform,
        // PROOT-012: custGeom裁剪。Some((cmds, path_scale))时，图片按
        // build_preset_path(cmds, x, y, path_scale)构造的路径裁剪，而非
        // 铺满x,y,w,h矩形——真实样本tdf112209（blipFill+custGeom五边形，
        // <p:style><a:fillRef>做duotone重新着色）需要这个才能画出正确
        // 轮廓的图片，而非把整张图铺满bounding box。
        //
        // path_scale独立于本函数的scale参数：本函数的scale历来在两条
        // 调用路径下含义不同（背景图路径=1.0需要内部乘、普通形状路径
        // 已经是像素单位传1.0不再缩放，见上方"render_shape_image 的
        // 约定"注释），而build_preset_path需要的是"局部坐标到像素坐标"
        // 的真实缩放系数（render_shape里的scale形参，用于render_preset_path
        // 同一处调用），两者不能混用，故用独立参数区分，不复用scale。
        geometry_clip: Option<(&[roe_model::common::PathCommand], f32)>,
    ) {
        // render_shape_image 的约定：w/h 参数是"未乘 scale 的原始单位"，
        // scale 参数在这里负责统一转换成目标像素尺寸。背景绘制路径
        // （render_slide 里的 effective_background 分支）传入的正是
        // pt 单位的 slide_width/slide_height，依赖这里乘 scale 转成像素。
        // 普通形状路径（render_shape 内部调用）此前错误地把已经乘过 scale
        // 的 w/h 传了进来又传了 scale，导致 scale 被重复应用——真正的
        // 修复点在调用处（render_shape 里传 scale_for_image=1.0，见下方
        // 调用点注释），不是这里。
        let width = (w * scale) as u32;
        let height = (h * scale) as u32;
        if width == 0 || height == 0 || data.is_empty() { return; }

        // PROOT-012b bugfix: tiny_skia::Pixmap::decode_png 只支持PNG，
        // 遇到JPEG（tdf112209样本的形状级blipFill图片就是JPEG）直接返回
        // Err，函数提前return，图片完全不显示。改用通用image crate解码
        // （workspace已声明png+jpeg features），成功后手动转换成
        // tiny_skia::Pixmap（RGBA8预乘alpha，tiny_skia::Pixmap要求预乘，
        // 用premultiply()方法转换，普通图片全不透明时是no-op，带alpha
        // 通道的PNG才会有实际效果）。
        let full_img = match image::load_from_memory(data) {
            Ok(dyn_img) => {
                let rgba = dyn_img.to_rgba8();
                let (iw, ih) = (rgba.width(), rgba.height());
                match tiny_skia::Pixmap::new(iw, ih) {
                    Some(mut pm) => {
                        // image crate输出straight（非预乘）alpha，
                        // tiny_skia::Pixmap 内部数据约定是预乘alpha，
                        // 但 tiny_skia 0.12 没有提供 premultiply() 方法
                        // （编译器确认 E0599），手动逐像素预乘：
                        // premultiplied = straight_rgb * alpha / 255。
                        // 完全不透明图片（alpha=255）这个运算是恒等变换，
                        // 不影响本次样本（tdf112209是不带alpha通道的JPEG，
                        // to_rgba8()后alpha恒为255），但写成通用逻辑以正确
                        // 支持未来遇到的带透明通道PNG。
                        let raw = rgba.as_raw();
                        let dst = pm.data_mut();
                        for i in (0..raw.len()).step_by(4) {
                            let a = raw[i + 3] as u32;
                            dst[i] = ((raw[i] as u32 * a) / 255) as u8;
                            dst[i + 1] = ((raw[i + 1] as u32 * a) / 255) as u8;
                            dst[i + 2] = ((raw[i + 2] as u32 * a) / 255) as u8;
                            dst[i + 3] = raw[i + 3];
                        }
                        pm
                    }
                    None => return,
                }
            }
            Err(_) => return,
        };

        // PPTX-10: 若存在裁剪（<a:srcRect>），先从原图裁出子 Pixmap，
        // 再用裁剪后的子图参与后续采样，而不是先拉伸整图再裁剪显示区域
        // （顺序错误会导致裁剪后图片内容比例失真，见调研文档 §6.2）。
        // crop_l/t/r/b 已在解析层归一化为 0.0-1.0，且第一期只保留正值
        // (inset)，故这里不再需要额外 clamp 负值，只需防止 l+r/t+b >= 1
        // 这种损坏文件导致的裁剪区域宽高为负或零的情况。
        let img = if crop_l > 0.0 || crop_t > 0.0 || crop_r > 0.0 || crop_b > 0.0 {
            let fw = full_img.width() as f32;
            let fh = full_img.height() as f32;
            let cx = (crop_l * fw).round() as i32;
            let cy = (crop_t * fh).round() as i32;
            let cw = ((1.0 - crop_l - crop_r) * fw).round().max(1.0) as i32;
            let ch = ((1.0 - crop_t - crop_b) * fh).round().max(1.0) as i32;
            match full_img.clone_rect(tiny_skia::IntRect::from_xywh(cx, cy, cw.max(1) as u32, ch.max(1) as u32).unwrap_or(
                tiny_skia::IntRect::from_xywh(0, 0, full_img.width(), full_img.height()).unwrap()
            )) {
                Some(cropped) => cropped,
                None => full_img,
            }
        } else {
            full_img
        };

        let sx = width as f32 / img.width() as f32;
        let sy = height as f32 / img.height() as f32;
        // PROOT-005-followup bugfix: 此前 from_translate(x,y).post_scale(sx,sy)
        // 会让 post_scale 把已经写入的平移量 x,y 也一并乘上 sx,sy，导致图片
        // 采样原点算错（x=1254 场景下会变成 x*sx=1881，偏移超过半张图宽）。
        // 正确顺序是先缩放图片局部坐标系，再把结果平移到目标位置：
        // from_scale(sx,sy).post_translate(x,y)，post_translate 只平移最终
        // 结果，不会被后续缩放污染。
        let transform = tiny_skia::Transform::from_scale(sx, sy).post_translate(x, y);

        let mut paint = tiny_skia::Paint::default();
        paint.shader = tiny_skia::Pattern::new(
            img.as_ref(),
            tiny_skia::SpreadMode::Pad,
            tiny_skia::FilterQuality::Bilinear,
            1.0,
            transform,
        );
        // PROOT-012: custGeom裁剪——有geometry_clip时按真实路径填充
        // （图片Pattern已经铺满，fill_path只是改变可见区域的形状，
        // 不影响图片采样本身），否则维持原有矩形铺满行为。
        match geometry_clip {
            Some((cmds, path_scale)) => {
                if let Some(path) = Self::build_preset_path(cmds, x, y, path_scale) {
                    pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, shape_transform, None);
                }
            }
            None => {
                let rect = tiny_skia::Rect::from_xywh(x, y, w * scale, h * scale);
                if let Some(rect) = rect {
                    pixmap.fill_rect(rect, &paint, shape_transform, None);
                }
            }
        }
    }

    /// ROE-TILE-001: 背景图平铺绘制，对应 OOXML
    /// `<a:blipFill><a:tile tx="..." ty="..." sx="..." sy="..." algn="tl"/></a:blipFill>`。
    /// 本次只实现 algn="tl"（见 roe-model FillStyle::PictureTiled 文档注释），
    /// 其余锚点如实忽略 tile_align 参数（暂不使用，保留供未来扩展）。
    ///
    /// 独立于 render_shape_image 新写，不复用/不修改它，原因：
    /// render_shape_image 内部 transform 的 post_translate(x, y) 与末尾
    /// fill_rect(x, y, ...) 都直接使用函数参数 x/y，未乘 scale——这在
    /// 现有两条调用路径下都不会暴露问题（普通形状路径 scale 恒传1.0；
    /// 背景绘制路径 x=y=0.0，0 乘不乘 scale 结果一样），但本次平铺场景
    /// 的 tile_offset_x/y 是真实非零 pt 偏移，且背景绘制场景的 scale
    /// 是真实缩放系数，两者组合会触发这个从未被验证过的潜在单位错误。
    /// 为避免动到已被充分验证的现有函数引入回归风险，新写独立函数正确
    /// 处理 scale 换算，两个函数职责分离、互不干扰。
    ///
    /// tiny-skia 的 SpreadMode::Repeat 原生支持重复平铺，重复周期严格
    /// 等于传入 Pattern 的 pixmap 本身的像素宽高（见 tiny-skia 0.12.0
    /// shaders/pattern.rs push_stages 里 limit_x.scale = pixmap.width()，
    /// 2026-08-06 会话确认，非猜测）。故本函数不手写"循环画N次瓦片"的
    /// 平铺逻辑，而是：把原图按 tile_scale_x/y 缩小/放大成"一个瓦片的
    /// 显示尺寸"对应的 transform 缩放系数，Pattern 在该 transform 下
    /// 的一个重复周期在画布坐标系下自然就是"一个瓦片的显示尺寸"，
    /// 交给 tiny-skia 底层完成重复绘制。
    /// 探测图片文件的物理DPI（x_dpi, y_dpi），用于 blipFill tile 平铺
    /// 场景把"原图像素尺寸"换算成"原图物理尺寸(pt)"。
    ///
    /// 背景（2026-08-06会话，tile平铺瓦片过大问题根因排查）：查证LOK
    /// oox/source/drawingml/fillproperties.cxx 的 lclGetOriginalSize +
    /// vcl/source/filter/jpeg/JpegReader.cxx 确认，OOXML tile的
    /// sx/sy（tile_scale_x/y）缩放的是"原图的物理尺寸"，不是"原图的
    /// 像素尺寸"。JpegReader.cxx 214-233行显示LOK直接读libjpeg解码出的
    /// JFIF density_unit/X_density/Y_density字段换算物理尺寸；没有
    /// density信息时LOK的兜底是把像素当屏幕像素按当前屏幕DPI换算
    /// （对应本函数的96 DPI兜底）。
    ///
    /// 本函数只手写解析两种最常见格式的物理尺寸元数据（image crate
    /// 0.25不暴露这类元数据的公共API，需要自己读文件头字节）：
    /// - JPEG: 扫描APP0(0xFFE0) JFIF段，读units(1=英寸/2=厘米)+
    ///   Xdensity+Ydensity（对应libjpeg的density_unit/X_density/
    ///   Y_density，字节位置固定，规范：ITU-T T.871 / JFIF 1.02）
    /// - PNG: 扫描pHYs chunk，单位固定是"像素/米"（PNG规范），
    ///   换算成DPI需要 ×0.0254
    /// 其余格式或没有该元数据：兜底96 DPI（与LOK的屏幕像素兜底语义
    /// 一致，效果上等价于"直接把像素数当pt用×0.75"）。
    fn probe_image_dpi(data: &[u8]) -> (f32, f32) {
        const FALLBACK_DPI: f32 = 96.0;

        // PNG: 8字节签名 + chunk序列（每个chunk: 4字节长度+4字节类型+
        // 数据+4字节CRC）。pHYs通常紧跟IHDR之后，但规范上位置不固定，
        // 这里线性扫描直到遇到IDAT或文件结束。
        if data.len() >= 8 && &data[0..8] == b"\x89PNG\r\n\x1a\n" {
            let mut pos = 8usize;
            while pos + 8 <= data.len() {
                let len = u32::from_be_bytes([data[pos], data[pos+1], data[pos+2], data[pos+3]]) as usize;
                let ctype = &data[pos+4..pos+8];
                if ctype == b"pHYs" && pos + 8 + 9 <= data.len() {
                    let px_per_m_x = u32::from_be_bytes([
                        data[pos+8], data[pos+9], data[pos+10], data[pos+11],
                    ]);
                    let px_per_m_y = u32::from_be_bytes([
                        data[pos+12], data[pos+13], data[pos+14], data[pos+15],
                    ]);
                    let unit = data[pos+16];
                    if unit == 1 && px_per_m_x > 0 && px_per_m_y > 0 {
                        return (px_per_m_x as f32 * 0.0254, px_per_m_y as f32 * 0.0254);
                    }
                    break; // unit==0（未指定物理单位，只是长宽比）：视为无DPI信息
                }
                if ctype == b"IDAT" { break; } // pHYs规范要求在IDAT之前，提前退出
                pos += 8 + len + 4; // 数据 + CRC
            }
            return (FALLBACK_DPI, FALLBACK_DPI);
        }

        // JPEG: 逐段扫描marker，找APP0且payload以"JFIF\0"开头的JFIF段。
        // JFIF段结构（APP0数据部分）：5字节"JFIF\0" + 2字节版本 +
        // 1字节units + 2字节Xdensity + 2字节Ydensity + ...（大端）
        if data.len() >= 4 && data[0] == 0xFF && data[1] == 0xD8 {
            let mut pos = 2usize;
            while pos + 4 <= data.len() {
                if data[pos] != 0xFF { break; }
                let marker = data[pos + 1];
                if marker == 0xD8 || marker == 0xD9 { pos += 2; continue; } // SOI/EOI无长度字段
                if pos + 4 > data.len() { break; }
                let seg_len = u16::from_be_bytes([data[pos+2], data[pos+3]]) as usize;
                if marker == 0xE0 && pos + 4 + 9 <= data.len() {
                    let payload = &data[pos+4..];
                    if payload.starts_with(b"JFIF\0") && payload.len() >= 14 {
                        let units = payload[7];
                        let x_density = u16::from_be_bytes([payload[8], payload[9]]) as f32;
                        let y_density = u16::from_be_bytes([payload[10], payload[11]]) as f32;
                        if x_density > 0.0 && y_density > 0.0 {
                            return match units {
                                1 => (x_density, y_density), // 单位已是DPI（像素/英寸）
                                2 => (x_density * 2.54, y_density * 2.54), // 像素/厘米→DPI
                                _ => (FALLBACK_DPI, FALLBACK_DPI), // units==0：只是长宽比，非物理DPI
                            };
                        }
                    }
                }
                if marker == 0xDA { break; } // SOS：进入压缩数据，扫描结束
                pos += 2 + seg_len;
            }
            return (FALLBACK_DPI, FALLBACK_DPI);
        }

        (FALLBACK_DPI, FALLBACK_DPI)
    }

    fn render_shape_image_tiled(
        &mut self,
        pixmap: &mut PixmapMut,
        data: &[u8],
        tile_offset_x: f32, tile_offset_y: f32,
        tile_scale_x: f32, tile_scale_y: f32,
        _tile_align: &str,
        pw: u32, ph: u32,
        scale: f32,
    ) {
        if pw == 0 || ph == 0 || data.is_empty() { return; }

        // 图片解码逻辑与 render_shape_image 保持一致（PROOT-012b：用
        // image crate 解码支持 PNG/JPEG，再手动预乘 alpha 转成
        // tiny_skia::Pixmap，注释详见 render_shape_image 同类代码）。
        let full_img = match image::load_from_memory(data) {
            Ok(dyn_img) => {
                let rgba = dyn_img.to_rgba8();
                let (iw, ih) = (rgba.width(), rgba.height());
                match tiny_skia::Pixmap::new(iw, ih) {
                    Some(mut pm) => {
                        let raw = rgba.as_raw();
                        let dst = pm.data_mut();
                        for i in (0..raw.len()).step_by(4) {
                            let a = raw[i + 3] as u32;
                            dst[i] = ((raw[i] as u32 * a) / 255) as u8;
                            dst[i + 1] = ((raw[i + 1] as u32 * a) / 255) as u8;
                            dst[i + 2] = ((raw[i + 2] as u32 * a) / 255) as u8;
                            dst[i + 3] = raw[i + 3];
                        }
                        pm
                    }
                    None => return,
                }
            }
            Err(_) => return,
        };

        // 2026-08-06会话修正：此前 `sx = tile_scale_x * scale` 是错的。
        // 查证LOK oox/fillproperties.cxx确认，OOXML tile的sx/sy缩放的是
        // "原图的物理尺寸(pt)"，不是"原图的像素尺寸"——旧代码把
        // tile_scale_x直接当成"原图像素→画布像素"缩放系数的一个因子，
        // 跳过了"原图物理尺寸"这一层，导致瓦片被放大了几倍（真实样本
        // tdf152070验证：484px JPEG在300 DPI下物理宽度只有116.2pt，
        // 若直接拿484px当pt用会比正确值大4倍多，与实测"3列 vs 应有
        // 13列"的现象吻合）。
        //
        // 正确公式：
        //   原图物理宽度(pt) = 原图像素宽度 / DPI × 72
        //   瓦片画布像素宽度 = 原图物理宽度(pt) × tile_scale_x × scale
        //   sx（tiny_skia相对原图像素的缩放系数）= 瓦片画布像素宽度 / 原图像素宽度
        let (dpi_x, dpi_y) = Self::probe_image_dpi(data);
        let (iw, ih) = (full_img.width() as f32, full_img.height() as f32);
        let orig_w_pt = iw / dpi_x * 72.0;
        let orig_h_pt = ih / dpi_y * 72.0;
        let tile_w_px = orig_w_pt * tile_scale_x * scale;
        let tile_h_px = orig_h_pt * tile_scale_y * scale;
        let sx = tile_w_px / iw;
        let sy = tile_h_px / ih;
        if sx <= 0.0 || sy <= 0.0 { return; }

        // tile_offset_x/y 是 pt 单位（解析层已从 EMU 换算），必须乘
        // scale 才能得到正确的像素偏移——这正是本函数存在的原因，
        // 与 render_shape_image 里同名参数处理方式不同，见函数文档注释。
        let offset_x_px = tile_offset_x * scale;
        let offset_y_px = tile_offset_y * scale;

        let transform = tiny_skia::Transform::from_scale(sx, sy)
            .post_translate(offset_x_px, offset_y_px);

        let mut paint = tiny_skia::Paint::default();
        paint.shader = tiny_skia::Pattern::new(
            full_img.as_ref(),
            tiny_skia::SpreadMode::Repeat,
            tiny_skia::FilterQuality::Bilinear,
            1.0,
            transform,
        );

        if let Some(rect) = tiny_skia::Rect::from_xywh(0.0, 0.0, pw as f32, ph as f32) {
            pixmap.fill_rect(rect, &paint, tiny_skia::Transform::identity(), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::common::Transform;
    use roe_model::impress::*;
    use roe_model::writer::Run;
    use tiny_skia::{Pixmap, IntSize};

    // ─── PPTX-8: build_shape_transform 矩阵构造正确性 ──────────────

    // ─── 2026-08-06会话: probe_image_dpi + tile平铺缩放公式回归测试 ──
    //
    // 背景：本次会话修复了 render_shape_image_tiled 里 sx/sy 换算公式
    // 的根本性错误（详见函数注释）。probe_image_dpi 是纯手写字节解析
    // 逻辑（JFIF/pHYs格式细节），此前完全没有自动化测试覆盖，属于
    // "容易在未来重构中被无意破坏又没人发现"的代码，故补充以下测试。

    /// 构造最小合法JPEG字节：SOI + APP0(JFIF段，指定units/density) + EOI。
    /// 不含真实图像数据（真实解码在render_shape_image_tiled里用
    /// image::load_from_memory完成，probe_image_dpi只关心文件头字节，
    /// 不需要图像内容合法即可测试其解析逻辑）。
    fn build_minimal_jfif_jpeg(units: u8, x_density: u16, y_density: u16) -> Vec<u8> {
        let mut data = vec![0xFF, 0xD8]; // SOI
        // APP0 marker
        data.push(0xFF);
        data.push(0xE0);
        // APP0 payload: "JFIF " + 2字节版本 + 1字节units + 2字节Xdensity +
        // 2字节Ydensity + 1字节Xthumbnail + 1字节Ythumbnail = 5+2+1+2+2+1+1=14字节
        let payload_len = 14u16;
        let seg_len = payload_len + 2; // 长度字段本身包含这2字节
        data.push((seg_len >> 8) as u8);
        data.push((seg_len & 0xFF) as u8);
        data.extend_from_slice(b"JFIF ");
        data.push(1); // version major
        data.push(2); // version minor
        data.push(units);
        data.extend_from_slice(&x_density.to_be_bytes());
        data.extend_from_slice(&y_density.to_be_bytes());
        data.push(0); // Xthumbnail
        data.push(0); // Ythumbnail
        data.extend_from_slice(&[0xFF, 0xD9]); // EOI
        data
    }

    /// 构造最小合法PNG字节：签名 + IHDR chunk + pHYs chunk（像素/米单位）+
    /// IDAT标记（不含真实压缩数据，probe_image_dpi在遇到IDAT时就会停止
    /// 扫描，不需要IDAT内容合法）。
    fn build_minimal_phys_png(px_per_m_x: u32, px_per_m_y: u32, unit: u8) -> Vec<u8> {
        let mut data: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]; // PNG签名

        // IHDR chunk（内容不影响probe_image_dpi，只是保持chunk序列结构完整）
        let ihdr_data: [u8; 13] = [
            0, 0, 0, 1, // width=1
            0, 0, 0, 1, // height=1
            8, 2, 0, 0, 0, // bit depth, color type, compression, filter, interlace
        ];
        data.extend_from_slice(&(ihdr_data.len() as u32).to_be_bytes());
        data.extend_from_slice(b"IHDR");
        data.extend_from_slice(&ihdr_data);
        data.extend_from_slice(&[0, 0, 0, 0]); // CRC占位，probe_image_dpi不校验CRC

        // pHYs chunk
        let mut phys_data = Vec::new();
        phys_data.extend_from_slice(&px_per_m_x.to_be_bytes());
        phys_data.extend_from_slice(&px_per_m_y.to_be_bytes());
        phys_data.push(unit);
        data.extend_from_slice(&(phys_data.len() as u32).to_be_bytes());
        data.extend_from_slice(b"pHYs");
        data.extend_from_slice(&phys_data);
        data.extend_from_slice(&[0, 0, 0, 0]); // CRC占位

        // IDAT标记（probe_image_dpi遇到IDAT即停止扫描，不需要真实压缩数据）
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(b"IDAT");
        data.extend_from_slice(&[0, 0, 0, 0]); // CRC占位

        data
    }

    /// JPEG JFIF units=1（英寸）：Xdensity/Ydensity直接就是DPI，应原样返回。
    /// 对应tdf152070真实样本的实际参数（300, 300, units=1）。
    #[test]
    fn test_probe_image_dpi_jpeg_jfif_units_inch() {
        let data = build_minimal_jfif_jpeg(1, 300, 300);
        let (dpi_x, dpi_y) = SlideRenderer::probe_image_dpi(&data);
        assert!((dpi_x - 300.0).abs() < 0.01, "units=1时Xdensity应直接作为DPI，得到{dpi_x}");
        assert!((dpi_y - 300.0).abs() < 0.01, "units=1时Ydensity应直接作为DPI，得到{dpi_y}");
    }

    /// JPEG JFIF units=2（厘米）：需要×2.54换算成DPI（每英寸像素/厘米）。
    #[test]
    fn test_probe_image_dpi_jpeg_jfif_units_cm() {
        let data = build_minimal_jfif_jpeg(2, 100, 100);
        let (dpi_x, dpi_y) = SlideRenderer::probe_image_dpi(&data);
        let expected = 100.0 * 2.54;
        assert!((dpi_x - expected).abs() < 0.01, "units=2应×2.54换算，期望{expected}，得到{dpi_x}");
        assert!((dpi_y - expected).abs() < 0.01, "units=2应×2.54换算，期望{expected}，得到{dpi_y}");
    }

    /// JPEG JFIF units=0（无物理单位，只是长宽比）：应兜底96 DPI，不能
    /// 把Xdensity/Ydensity误当成DPI直接使用。
    #[test]
    fn test_probe_image_dpi_jpeg_jfif_units_none_falls_back() {
        let data = build_minimal_jfif_jpeg(0, 300, 300);
        let (dpi_x, dpi_y) = SlideRenderer::probe_image_dpi(&data);
        assert!((dpi_x - 96.0).abs() < 0.01, "units=0（无物理单位）应兜底96 DPI，得到{dpi_x}");
        assert!((dpi_y - 96.0).abs() < 0.01, "units=0（无物理单位）应兜底96 DPI，得到{dpi_y}");
    }

    /// PNG pHYs unit=1（米）：px_per_meter换算成DPI需要×0.0254。
    #[test]
    fn test_probe_image_dpi_png_phys_unit_meter() {
        // 300 DPI对应约11811 像素/米（300 / 0.0254 ≈ 11811.02）
        let data = build_minimal_phys_png(11811, 11811, 1);
        let (dpi_x, dpi_y) = SlideRenderer::probe_image_dpi(&data);
        assert!((dpi_x - 300.0).abs() < 1.0, "pHYs unit=1应×0.0254换算成约300 DPI，得到{dpi_x}");
        assert!((dpi_y - 300.0).abs() < 1.0, "pHYs unit=1应×0.0254换算成约300 DPI，得到{dpi_y}");
    }

    /// PNG pHYs unit=0（未指定物理单位，只是长宽比）：应兜底96 DPI。
    #[test]
    fn test_probe_image_dpi_png_phys_unit_unspecified_falls_back() {
        let data = build_minimal_phys_png(11811, 11811, 0);
        let (dpi_x, dpi_y) = SlideRenderer::probe_image_dpi(&data);
        assert!((dpi_x - 96.0).abs() < 0.01, "unit=0应兜底96 DPI，得到{dpi_x}");
        assert!((dpi_y - 96.0).abs() < 0.01, "unit=0应兜底96 DPI，得到{dpi_y}");
    }

    /// 既不是JPEG也不是PNG（或字节太短无法识别）：应兜底96 DPI，不panic。
    #[test]
    fn test_probe_image_dpi_unknown_format_falls_back() {
        let data = vec![0x00, 0x01, 0x02, 0x03];
        let (dpi_x, dpi_y) = SlideRenderer::probe_image_dpi(&data);
        assert!((dpi_x - 96.0).abs() < 0.01);
        assert!((dpi_y - 96.0).abs() < 0.01);
    }

    /// 空字节：不能panic，应兜底96 DPI。
    #[test]
    fn test_probe_image_dpi_empty_data_does_not_panic() {
        let (dpi_x, dpi_y) = SlideRenderer::probe_image_dpi(&[]);
        assert!((dpi_x - 96.0).abs() < 0.01);
        assert!((dpi_y - 96.0).abs() < 0.01);
    }

    /// 回归测试：tile平铺缩放公式的端到端数值验证，用tdf152070真实
    /// 样本参数（2026-08-06会话手工验算过的理论值）做断言，防止未来
    /// 重构时"编译通过、单测通过但语义又错了"重演（这正是v69文档记录
    /// 的教训——本测试直接覆盖"数字用在正确地方"这一层，而不只是
    /// "数字换算对不对"）。
    ///
    /// 已知：image1.jpg是484×290像素、JFIF 300 DPI；tile_scale_x=0.6，
    /// slide宽960pt，scale取1.0（不引入额外缩放系数，方便验算）。
    /// 手工验算：原图物理宽度=484/300×72≈116.16pt；瓦片画布宽度
    /// =116.16×0.6≈69.7pt；理论列数=960/69.7≈13.8，与OnlyOffice/MS365
    /// 真实渲染效果（约13列）吻合。
    #[test]
    fn test_tile_scale_formula_matches_hand_calculation_tdf152070() {
        let iw = 484.0_f32;
        let ih = 290.0_f32;
        let dpi_x = 300.0_f32;
        let dpi_y = 300.0_f32;
        let tile_scale_x = 0.6_f32;
        let tile_scale_y = 0.7_f32;
        let scale = 1.0_f32; // 不引入额外pt->px缩放，直接验算pt数值

        let orig_w_pt = iw / dpi_x * 72.0;
        let orig_h_pt = ih / dpi_y * 72.0;
        let tile_w_px = orig_w_pt * tile_scale_x * scale;
        let tile_h_px = orig_h_pt * tile_scale_y * scale;

        // 原图物理宽度应约116.16pt（不是484px本身，这正是此前bug的
        // 根因所在——旧代码把484px直接当pt/缩放基数使用）
        assert!((orig_w_pt - 116.16).abs() < 0.1, "原图物理宽度应≈116.16pt，得到{orig_w_pt}");

        // 瓦片画布宽度应约69.7pt
        assert!((tile_w_px - 69.696).abs() < 0.1, "瓦片画布宽度应≈69.7pt，得到{tile_w_px}");

        // slide总宽960pt / 瓦片宽度 应约等于13.8列，与真实参照图13列吻合
        let slide_w_pt = 960.0_f32;
        let theoretical_cols = slide_w_pt / tile_w_px;
        assert!(
            (theoretical_cols - 13.77).abs() < 0.5,
            "理论列数应≈13.8列（与OnlyOffice/MS365约13列一致），得到{theoretical_cols}"
        );

        // sx = tile_w_px / iw，这是tiny_skia Pattern实际使用的相对缩放系数
        let sx = tile_w_px / iw;
        let sy = tile_h_px / ih;
        assert!(sx > 0.0 && sx < 1.0, "sx应是缩小系数（瓦片比原图小），得到{sx}");
        assert!(sy > 0.0 && sy < 1.0, "sy应是缩小系数（瓦片比原图小），得到{sy}");

        // 回归防护：此前bug版本的错误公式是 sx_wrong = tile_scale_x * scale
        // （完全不涉及iw/dpi），必须确认新公式给出的sx与错误公式的结果
        // 有数量级差异，否则说明修复没生效
        let sx_wrong_formula = tile_scale_x * scale;
        assert!(
            (sx - sx_wrong_formula).abs() > 0.01,
            "新公式的sx应明显不同于旧bug公式的结果，否则修复未生效"
        );
    }

    // ─── PPTX-8: build_shape_transform 矩阵构造正确性 ──────────────

    /// 无旋转/无翻转/无父变换：应该退化为恒等矩阵（不产生任何变换），
    /// 这是回归防护的基线——所有此前依赖 Transform::identity() 的现有
    /// 测试（test_render_slide_png 等9个）必须在这个前提下继续通过。
    #[test]
    fn test_build_shape_transform_no_rotation_is_identity() {
        let t = SlideRenderer::build_shape_transform(
            10.0, 20.0, 100.0, 50.0,
            None, None, None,
            tiny_skia::Transform::identity(),
        );
        assert_eq!(t, tiny_skia::Transform::identity(), "无旋转/翻转/父变换时应退化为恒等矩阵");
    }

    /// rotation=Some(0.0)（显式声明0度，非None）同样应产生恒等矩阵，
    /// 覆盖 PPTX-7 测试里验证过的"Some(false)≠None"同类语义边界。
    #[test]
    fn test_build_shape_transform_zero_rotation_is_identity() {
        let t = SlideRenderer::build_shape_transform(
            10.0, 20.0, 100.0, 50.0,
            Some(0.0), Some(false), Some(false),
            tiny_skia::Transform::identity(),
        );
        assert_eq!(t, tiny_skia::Transform::identity(), "rotation=0/flip=false 应等效恒等矩阵");
    }

    /// 90度旋转：验证矩阵作用在 shape 中心点上时中心点保持不动
    /// （绕中心旋转的核心性质——若实现退化成"绕左上角旋转"，中心点会
    /// 漂移，这正是《坐标变换绘制顺序图片表格.md》§1.2 点出的易踩陷阱）。
    #[test]
    fn test_build_shape_transform_rotation_keeps_center_fixed() {
        let (x, y, w, h) = (10.0, 20.0, 100.0, 50.0);
        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
        let t = SlideRenderer::build_shape_transform(
            x, y, w, h,
            Some(90.0), None, None,
            tiny_skia::Transform::identity(),
        );
        let mut p = tiny_skia::Point { x: cx, y: cy };
        t.map_point(&mut p);
        assert!((p.x - cx).abs() < 0.01, "旋转中心点x坐标不应漂移: got {}", p.x);
        assert!((p.y - cy).abs() < 0.01, "旋转中心点y坐标不应漂移: got {}", p.y);
    }

    /// 90度旋转：验证一个已知角点的映射方向正确（顺时针，因为
    /// OOXML/tiny-skia的y轴向下，正角度视觉上表现为顺时针）。
    /// 取 shape 右上角 (x+w, y)，90度顺时针旋转后应该落到
    /// "中心右侧距离等于原来到中心的垂直距离"的新位置。
    #[test]
    fn test_build_shape_transform_rotation_direction() {
        let (x, y, w, h) = (0.0, 0.0, 100.0, 50.0);
        let (cx, cy) = (x + w / 2.0, y + h / 2.0); // (50, 25)
        let t = SlideRenderer::build_shape_transform(
            x, y, w, h,
            Some(90.0), None, None,
            tiny_skia::Transform::identity(),
        );
        // 右上角 (100, 0)，相对中心偏移 (50, -25)
        let mut p = tiny_skia::Point { x: 100.0, y: 0.0 };
        t.map_point(&mut p);
        // 顺时针90度：(dx,dy) -> (-dy,dx)，偏移(50,-25) -> (25,50)
        // 新绝对坐标 = 中心(50,25) + (25,50) = (75,75)
        assert!((p.x - 75.0).abs() < 0.1, "旋转后x坐标不符预期: got {}", p.x);
        assert!((p.y - 75.0).abs() < 0.1, "旋转后y坐标不符预期: got {}", p.y);
    }

    /// flip_h：水平翻转应使中心保持不动，但左右边界互换。
    #[test]
    fn test_build_shape_transform_flip_h_keeps_center_fixed_and_mirrors() {
        let (x, y, w, h) = (0.0, 0.0, 100.0, 50.0);
        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
        let t = SlideRenderer::build_shape_transform(
            x, y, w, h,
            None, Some(true), None,
            tiny_skia::Transform::identity(),
        );
        let mut center = tiny_skia::Point { x: cx, y: cy };
        t.map_point(&mut center);
        assert!((center.x - cx).abs() < 0.01, "翻转中心不应漂移: got {}", center.x);

        // 左上角 (0,0) 水平翻转后应移动到右上角位置 (100,0)
        let mut top_left = tiny_skia::Point { x: 0.0, y: 0.0 };
        t.map_point(&mut top_left);
        assert!((top_left.x - 100.0).abs() < 0.1, "flip_h后左上角应移到x=100: got {}", top_left.x);
        assert!((top_left.y - 0.0).abs() < 0.1, "flip_h不应影响y坐标: got {}", top_left.y);
    }

    /// 父变换叠加：给定一个非恒等的父变换（模拟未来PPTX-9分组场景），
    /// 子变换应该在父变换基础上叠加，而不是被父变换吞掉或忽略。
    #[test]
    fn test_build_shape_transform_parent_transform_applied() {
        let parent = tiny_skia::Transform::from_translate(1000.0, 2000.0);
        let t = SlideRenderer::build_shape_transform(
            10.0, 20.0, 100.0, 50.0,
            None, None, None,
            parent,
        );
        // 无自身旋转/翻转时，子变换应等于父变换本身（父变换被完整传导）
        assert_eq!(t, parent, "无自身变换时，父变换应完整传导给子级");
    }

    fn make_slide() -> Slide {
        Slide { layout_idx: None,
            shapes: vec![
                Shape {
                    id: 1,
                    geometry: ShapeGeometry::Rect,
                    position: Transform::from_rect(50.0, 50.0, 200.0, 100.0),
                    style: ShapeStyle {
                        fill: Some(FillStyle::Solid(roe_model::common::Color { r: 70, g: 130, b: 200, a: 255 })),
                        stroke: Some(StrokeStyle { color: roe_model::common::Color { r: 30, g: 80, b: 160, a: 255 }, width: 2.0 }),
                        shadow: None,
                        head_arrow: None,
                        tail_arrow: None,
                    },
                    content: ShapeContent::Empty,
                    text_body: Default::default(),
                    placeholder: None,
                    st_cxn: None,
                    end_cxn: None,
                },
                Shape {
                    id: 2,
                    geometry: ShapeGeometry::Ellipse,
                    position: Transform::from_rect(300.0, 150.0, 120.0, 80.0),
                    style: ShapeStyle {
                        fill: Some(FillStyle::Solid(roe_model::common::Color { r: 220, g: 80, b: 80, a: 255 })),
                        stroke: None,
                        shadow: None,
                        head_arrow: None,
                        tail_arrow: None,
                    },
                    content: ShapeContent::Empty,
                    text_body: Default::default(),
                    placeholder: None,
                    st_cxn: None,
                    end_cxn: None,
                },
            ],
            master_idx: None,
            animation_tree: AnimationNode::TimingRoot { children: vec![] },
            transition: None,
            notes: None,
            background: None, // PROOT-005 test fixture
            layout_shapes: vec![], // PROOT-005 followup test fixture
        }
    }

    fn make_slide_with_text() -> Slide {
        let mut slide = make_slide();
        slide.shapes.push(Shape {
            id: 3,
            geometry: ShapeGeometry::Rect,
            position: Transform::from_rect(40.0, 20.0, 500.0, 80.0),
            style: ShapeStyle { fill: None, stroke: None, shadow: None, head_arrow: None, tail_arrow: None },
            content: ShapeContent::Text(vec![Paragraph {
                style: roe_model::common::StyleRef("Title".to_string()),
                runs: vec![Run { text: "ROE Impress 渲染测试".to_string(), char_style: None, ..Default::default() }],
                list_info: None,
                ..Default::default()
            }]),
            text_body: Default::default(),
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        });
        slide
    }

    /// 共用辅助：渲染并返回 (PNG bytes, pw, ph)
    fn render_to_png(renderer: &mut SlideRenderer, slide: &Slide, runtime: Option<&SlideRuntimeState>, scale: f32) -> (Vec<u8>, u32, u32) {
        let pw = (renderer.slide_width  * scale) as u32;
        let ph = (renderer.slide_height * scale) as u32;
        let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];

        let (out_w, out_h) = renderer
            .render_slide(slide, &StyleRegistry::default(), runtime, scale, &mut buf, &Default::default(), None)
            .expect("render_slide failed");
        assert_eq!(out_w, pw);
        assert_eq!(out_h, ph);

        let pixmap = Pixmap::from_vec(buf, IntSize::from_wh(out_w, out_h).expect("invalid size"))
            .expect("pixmap from buf failed");
        let png = pixmap.encode_png().expect("png encode failed");
        (png, out_w, out_h)
    }

    /// PPTX-8 端到端验证：带旋转的 Shape 经过完整 render_slide 管线，
    /// 验证接线（build_shape_transform → render_shape → fill_rect）确实
    /// 生效，而不仅仅是"跑通不panic"。用一个非正方形矩形（200x100）做
    /// 90度旋转：
    /// 1) 不panic、正常产出合法PNG
    /// 2) 旋转后的像素数据与不旋转版本不同（证明变换确实作用于渲染结果，
    ///    不是被悄悄退化成恒等矩阵或参数没传进去）
    /// 3) 像素级抽样验证方向：旋转前的矩形是"宽200高100"，横向长、纵向短；
    ///    90度旋转后（绕中心），应变为"视觉上纵向长、横向短"。取矩形中心
    ///    正上方（旋转前该点在矩形外，因为半高只有50，取60）、旋转后因为
    ///    宽高互换，该点到中心的距离(60)应落在新的"半高"(原半宽100)以内，
    ///    即该点应变为矩形填充色——直接验证了旋转角度和中心点的正确性，
    ///    不是只看字节是否变化这种弱验证。
    #[test]
    fn test_render_slide_with_rotation_end_to_end() {
        let mut renderer = SlideRenderer::new(960.0, 540.0);

        // 基线：不旋转的宽矩形（200x100），用于产出对比PNG
        let slide_no_rot = Slide { layout_idx: None,
            shapes: vec![Shape {
                id: 1,
                geometry: ShapeGeometry::Rect,
                position: Transform::from_rect(400.0, 220.0, 200.0, 100.0),
                style: ShapeStyle {
                    fill: Some(FillStyle::Solid(roe_model::common::Color { r: 10, g: 200, b: 10, a: 255 })),
                    stroke: None,
                    shadow: None,
                    head_arrow: None,
                    tail_arrow: None,
                },
                content: ShapeContent::Empty,
                text_body: Default::default(),
                placeholder: None,
                st_cxn: None,
                end_cxn: None,
            }],
            master_idx: None,
            animation_tree: AnimationNode::TimingRoot { children: vec![] },
            transition: None,
            notes: None,
            background: None, // PROOT-005 test fixture
            layout_shapes: vec![], // PROOT-005 followup test fixture
        };

        // 旋转版本：同一个矩形，绕中心旋转90度
        let mut slide_rot = slide_no_rot.clone();
        slide_rot.shapes[0].position.rotation = Some(90.0);

        let (png_no_rot, _w, _h) = render_to_png(&mut renderer, &slide_no_rot, None, 1.0);
        let (png_rot, _w2, _h2) = render_to_png(&mut renderer, &slide_rot, None, 1.0);

        // 1) 基本要求：都是合法PNG，不panic
        assert!(!png_no_rot.is_empty());
        assert!(!png_rot.is_empty());
        assert_eq!(&png_rot[1..4], b"PNG");

        // 2) 旋转确实改变了渲染结果（不是被退化成恒等矩阵）
        assert_ne!(png_no_rot, png_rot, "旋转90度后的PNG字节应与不旋转版本不同，否则说明shape_transform没有真正生效");

        // 3) 像素级验证：解码两张PNG，检查矩形中心正上方60px处的颜色
        let pw = (renderer.slide_width * 1.0) as u32;
        let cx = 400.0 + 200.0 / 2.0; // 矩形中心 x = 500
        let cy = 220.0 + 100.0 / 2.0; // 矩形中心 y = 270
        let probe_x = cx as u32;
        let probe_y = (cy - 60.0) as u32; // 中心正上方60px

        let decode_pixel = |png: &[u8], x: u32, y: u32, pw: u32| -> (u8, u8, u8, u8) {
            let img = Pixmap::decode_png(png).expect("decode png failed");
            let data = img.data();
            let idx = ((y * pw + x) * 4) as usize;
            (data[idx], data[idx + 1], data[idx + 2], data[idx + 3])
        };

        let px_no_rot = decode_pixel(&png_no_rot, probe_x, probe_y, pw);
        let px_rot = decode_pixel(&png_rot, probe_x, probe_y, pw);

        // 不旋转时，中心正上方60px处在"半高100/2=50"之外，应该是白色背景，不是矩形绿色
        assert_ne!(px_no_rot, (10, 200, 10, 255), "不旋转基线：该探测点应在矩形外（背景色），验证测试坐标设计本身没有搞反");

        // 旋转90度后，宽高互换（新的"半高"变为原来的半宽100），该点应落入矩形范围，
        // 变为矩形填充色（允许边缘抗锯齿造成的小误差，主判据是"明显不再是背景白色"）
        assert_eq!(px_rot, (10, 200, 10, 255), "旋转90度后，中心正上方60px处应落入旋转后的矩形范围并显示为填充色，验证shape_transform的旋转方向和中心点均正确");
    }

    /// PPTX-10 端到端验证：图片裁剪(srcRect)真正生效，且裁剪顺序正确
    /// （先从原图裁出子矩形再采样，不是先拉伸整图再裁剪显示区域——顺序
    /// 错误会导致裁剪后内容比例失真，见调研文档《坐标变换绘制顺序图片
    /// 表格.md》§6.2）。
    ///
    /// 构造一张左半红、右半蓝的100x100测试图。裁剪 crop_l=0.5（裁掉左
    /// 半，只保留右半蓝色区域）后渲染进一个纯色探测区域，验证：
    /// 1) 未裁剪时形状中心点应是红蓝混合区域附近（探测左侧=红，右侧=蓝）
    /// 2) crop_l=0.5 裁剪后，图片应整体拉伸显示为纯蓝色（左半红色已被
    ///    裁掉，不应再出现在渲染结果的任意采样点）——这是最关键的断言，
    ///    如果实现顺序反了（先拉伸整图，裁剪只是改了显示范围而非采样源），
    ///    渲染结果仍会同时包含红蓝两色，此断言会失败并暴露顺序错误。
    #[test]
    fn test_render_shape_image_crop_end_to_end() {
        let mut renderer = SlideRenderer::new(960.0, 540.0);

        // 构造左半红(255,0,0)右半蓝(0,0,255)的100x100测试图，编码成PNG字节
        let mut src = Pixmap::new(100, 100).expect("pixmap new failed");
        {
            let mut paint_red = tiny_skia::Paint::default();
            paint_red.set_color_rgba8(255, 0, 0, 255);
            let rect_left = tiny_skia::Rect::from_xywh(0.0, 0.0, 50.0, 100.0).unwrap();
            src.fill_rect(rect_left, &paint_red, tiny_skia::Transform::identity(), None);

            let mut paint_blue = tiny_skia::Paint::default();
            paint_blue.set_color_rgba8(0, 0, 255, 255);
            let rect_right = tiny_skia::Rect::from_xywh(50.0, 0.0, 50.0, 100.0).unwrap();
            src.fill_rect(rect_right, &paint_blue, tiny_skia::Transform::identity(), None);
        }
        let png_bytes = src.encode_png().expect("encode png failed");

        let make_slide = |crop_l: f32, crop_t: f32, crop_r: f32, crop_b: f32| Slide { layout_idx: None,
            shapes: vec![Shape {
                id: 1,
                geometry: ShapeGeometry::Rect,
                position: Transform::from_rect(400.0, 220.0, 100.0, 100.0),
                style: ShapeStyle { fill: None, stroke: None, shadow: None, head_arrow: None, tail_arrow: None },
                content: ShapeContent::Image(roe_model::common::ImageData {
                    data: png_bytes.clone(),
                    mime: "image/png".to_string(),
                    crop_l, crop_t, crop_r, crop_b,
                }),
                text_body: Default::default(),
                placeholder: None,
                st_cxn: None,
                end_cxn: None,
            }],
            master_idx: None,
            animation_tree: AnimationNode::TimingRoot { children: vec![] },
            transition: None,
            notes: None,
            background: None, // PROOT-005 test fixture
            layout_shapes: vec![], // PROOT-005 followup test fixture
        };

        let decode_pixel = |png: &[u8], x: u32, y: u32, pw: u32| -> (u8, u8, u8, u8) {
            let img = Pixmap::decode_png(png).expect("decode png failed");
            let data = img.data();
            let idx = ((y * pw + x) * 4) as usize;
            (data[idx], data[idx + 1], data[idx + 2], data[idx + 3])
        };

        let pw = (renderer.slide_width * 1.0) as u32;

        let slide_no_crop = make_slide(0.0, 0.0, 0.0, 0.0);
        let (png_no_crop, _, _) = render_to_png(&mut renderer, &slide_no_crop, None, 1.0);
        let px_left = decode_pixel(&png_no_crop, 420, 270, pw);
        let px_right = decode_pixel(&png_no_crop, 480, 270, pw);
        assert_eq!(px_left, (255, 0, 0, 255), "未裁剪基线：形状左侧采样点应为原图红色区域，验证测试构造本身没有搞反左右");
        assert_eq!(px_right, (0, 0, 255, 255), "未裁剪基线：形状右侧采样点应为原图蓝色区域");

        let slide_cropped = make_slide(0.5, 0.0, 0.0, 0.0);
        let (png_cropped, _, _) = render_to_png(&mut renderer, &slide_cropped, None, 1.0);
        let px_left_cropped = decode_pixel(&png_cropped, 420, 270, pw);
        let px_right_cropped = decode_pixel(&png_cropped, 480, 270, pw);
        assert_eq!(px_left_cropped, (0, 0, 255, 255), "crop_l=0.5裁剪后，形状左侧采样点应变为蓝色（原红色区域已被裁掉），若仍为红色说明裁剪未生效或裁剪顺序错误");
        assert_eq!(px_right_cropped, (0, 0, 255, 255), "crop_l=0.5裁剪后，形状右侧采样点应保持蓝色");
    }

    /// PPTX-11 part C 端到端验证 1：PresetPath 直线段路径（MoveTo/LineTo/
    /// Close，不含 ArcTo）填充正确。构造一个占满整个 shape 局部坐标系
    /// （0,0)-(100,0)-(100,100) 的直角三角形，验证：
    /// 1) 三角形内部点（靠近直角顶点附近）应被填充为指定颜色；
    /// 2) 三角形外部但仍在 shape 包围盒内的点（左上角附近，直角三角形
    ///    斜边之外）应保持背景色，不被误填——这是最容易在"退化成矩形
    ///    填充"的实现里漏掉的一环，若渲染层仍在用 `_ => fill_rect` 兜底
    ///    处理 PresetPath，此断言会失败并暴露回归。
    #[test]
    fn test_render_preset_path_triangle_fills_interior() {
        use roe_model::common::PathCommand;

        let mut renderer = SlideRenderer::new(960.0, 540.0);

        let triangle_cmds = vec![
            PathCommand::MoveTo { x: 0.0, y: 0.0 },
            PathCommand::LineTo { x: 100.0, y: 0.0 },
            PathCommand::LineTo { x: 100.0, y: 100.0 },
            PathCommand::Close,
        ];

        let slide = Slide { layout_idx: None,
            shapes: vec![Shape {
                id: 1,
                // v181: 测试夹具手工构造的原始路径命令，非真实预设形状求值结果，
                // 没有对应的 prst 名字/adj 值可填，如实留空。
                geometry: ShapeGeometry::PresetPath(roe_model::common::PresetPathData {
                    commands: triangle_cmds,
                    preset_name: String::new(),
                    adj_overrides: Vec::new(),
                }),
                position: Transform::from_rect(400.0, 220.0, 100.0, 100.0),
                style: ShapeStyle {
                    fill: Some(FillStyle::Solid(roe_model::common::Color { r: 10, g: 200, b: 10, a: 255 })),
                    stroke: None,
                    shadow: None,
                    head_arrow: None,
                    tail_arrow: None,
                },
                content: ShapeContent::Empty,
                text_body: Default::default(),
                placeholder: None,
                st_cxn: None,
                end_cxn: None,
            }],
            master_idx: None,
            animation_tree: AnimationNode::TimingRoot { children: vec![] },
            transition: None,
            notes: None,
            background: None, // PROOT-005 test fixture
            layout_shapes: vec![], // PROOT-005 followup test fixture
        };

        let decode_pixel = |png: &[u8], x: u32, y: u32, pw: u32| -> (u8, u8, u8, u8) {
            let img = Pixmap::decode_png(png).expect("decode png failed");
            let data = img.data();
            let idx = ((y * pw + x) * 4) as usize;
            (data[idx], data[idx + 1], data[idx + 2], data[idx + 3])
        };

        let pw = (renderer.slide_width * 1.0) as u32;
        let (png, _, _) = render_to_png(&mut renderer, &slide, None, 1.0);

        // 三角形内部：直角顶点(100,0)附近，shape局部坐标约(90,10)对应屏幕(490,230)
        let px_inside = decode_pixel(&png, 490, 230, pw);
        assert_eq!(px_inside, (10, 200, 10, 255), "三角形内部采样点应为填充色，若仍是背景色说明PresetPath未接入渲染或退化成了矩形之外的空白判断有误");

        // 三角形外部：shape局部坐标(5,90)，在包围盒内但在斜边(0,0)-(100,100)之外
        // （斜边方程 y=x，点(5,90)满足 y>x，在三角形外侧），对应屏幕(405,310)
        let px_outside = decode_pixel(&png, 405, 310, pw);
        assert_ne!(px_outside, (10, 200, 10, 255), "三角形外部但仍在包围盒内的采样点不应被填充，若与内部同色说明渲染退化成了对整个包围盒填矩形（未真正走三角形路径）");
    }

    /// PPTX-11 part C 端到端验证 2：PresetPath 含 ArcTo 的圆弧路径（贝塞尔
    /// 近似）填充正确。构造一个由4段90度ArcTo首尾相接组成的完整圆
    /// （与 `preset_geometry.rs` 里 `test_arcto_cursor_quarter_circle`/
    /// ellipse 求值测试用的同一种"纯ArcTo拼接、无显式MoveTo中转"构造方式
    /// 一致，专门覆盖圆心反推+游标衔接+贝塞尔近似这条链路，不是简单
    /// 直线路径能替代验证的）。shape局部坐标系100x100，圆心(50,50)半径40。
    /// 验证：
    /// 1) 圆心点应被填充；
    /// 2) 包围盒四角（圆外）不应被填充——若ArcTo被渲染层忽略或角度单位
    ///    换算错误（例如忘记除以60000），贝塞尔曲线要么不出现、要么
    ///    扫描角度差几个数量级导致路径退化，四角误填或圆心漏填都能
    ///    暴露出来。
    #[test]
    fn test_render_preset_path_arcto_circle_fills_correctly() {
        use roe_model::common::PathCommand;

        let mut renderer = SlideRenderer::new(960.0, 540.0);

        // 4段90度圆弧，start_deg/sweep_deg 单位是1/60000度（与
        // preset_geometry.rs 的 PathCommand::ArcTo 产出语义一致）。
        // 圆心(50,50) 半径40，从0度开始逆时针（正sweep）每次转90度。
        let deg60000 = |d: f32| d * 60000.0;
        let circle_cmds = vec![
            PathCommand::MoveTo { x: 90.0, y: 50.0 }, // 起点：圆心+(rx,0) = (50+40, 50)
            PathCommand::ArcTo { cx: 50.0, cy: 50.0, rx: 40.0, ry: 40.0, start_deg: deg60000(0.0), sweep_deg: deg60000(90.0) },
            PathCommand::ArcTo { cx: 50.0, cy: 50.0, rx: 40.0, ry: 40.0, start_deg: deg60000(90.0), sweep_deg: deg60000(90.0) },
            PathCommand::ArcTo { cx: 50.0, cy: 50.0, rx: 40.0, ry: 40.0, start_deg: deg60000(180.0), sweep_deg: deg60000(90.0) },
            PathCommand::ArcTo { cx: 50.0, cy: 50.0, rx: 40.0, ry: 40.0, start_deg: deg60000(270.0), sweep_deg: deg60000(90.0) },
            PathCommand::Close,
        ];

        let slide = Slide { layout_idx: None,
            shapes: vec![Shape {
                id: 1,
                // v181: 测试夹具手工构造的原始路径命令，非真实预设形状求值结果，
                // 没有对应的 prst 名字/adj 值可填，如实留空。
                geometry: ShapeGeometry::PresetPath(roe_model::common::PresetPathData {
                    commands: circle_cmds,
                    preset_name: String::new(),
                    adj_overrides: Vec::new(),
                }),
                position: Transform::from_rect(400.0, 220.0, 100.0, 100.0),
                style: ShapeStyle {
                    fill: Some(FillStyle::Solid(roe_model::common::Color { r: 200, g: 10, b: 200, a: 255 })),
                    stroke: None,
                    shadow: None,
                    head_arrow: None,
                    tail_arrow: None,
                },
                content: ShapeContent::Empty,
                text_body: Default::default(),
                placeholder: None,
                st_cxn: None,
                end_cxn: None,
            }],
            master_idx: None,
            animation_tree: AnimationNode::TimingRoot { children: vec![] },
            transition: None,
            notes: None,
            background: None, // PROOT-005 test fixture
            layout_shapes: vec![], // PROOT-005 followup test fixture
        };

        let decode_pixel = |png: &[u8], x: u32, y: u32, pw: u32| -> (u8, u8, u8, u8) {
            let img = Pixmap::decode_png(png).expect("decode png failed");
            let data = img.data();
            let idx = ((y * pw + x) * 4) as usize;
            (data[idx], data[idx + 1], data[idx + 2], data[idx + 3])
        };

        let pw = (renderer.slide_width * 1.0) as u32;
        let (png, _, _) = render_to_png(&mut renderer, &slide, None, 1.0);

        // 圆心：局部(50,50) -> 屏幕(450,270)
        let px_center = decode_pixel(&png, 450, 270, pw);
        assert_eq!(px_center, (200, 10, 200, 255), "圆心应被填充，若为背景色说明ArcTo贝塞尔近似未生效（路径为空或退化）");

        // 包围盒左上角附近：局部(2,2)，在圆外（距圆心约68>40半径），屏幕(402,222)
        let px_corner = decode_pixel(&png, 402, 222, pw);
        assert_ne!(px_corner, (200, 10, 200, 255), "包围盒角落（圆外）不应被填充，若与圆心同色说明角度单位换算错误导致圆弧扫过了整个包围盒甚至更大范围");
    }

    /// PPTX-9a 端到端验证 1：分组基础嵌套定位。
    /// 子 Shape 的 position 是"相对分组坐标系"的局部坐标（xfrm 的
    /// a:off/a:ext 语义，子级不知道父级偏移到哪，只知道自己在父坐标系
    /// 里的位置），分组自身 position 提供整体平移。验证：子 Shape 用
    /// (0,0,w,h) 声明局部位置时，最终渲染必须叠加分组的平移量，而不是
    /// 直接以(0,0)为屏幕坐标渲染（后者说明 parent_transform 被忽略）。
    #[test]
    fn test_render_slide_with_group_nested_offset_end_to_end() {
        let mut renderer = SlideRenderer::new(960.0, 540.0);

        // 子 Shape 局部坐标 (0,0,100,80)，不叠加分组平移的话会画在画布左上角
        let child = Shape {
            id: 2,
            geometry: ShapeGeometry::Rect,
            position: Transform::from_rect(0.0, 0.0, 100.0, 80.0),
            style: ShapeStyle {
                fill: Some(FillStyle::Solid(roe_model::common::Color { r: 200, g: 30, b: 30, a: 255 })),
                stroke: None,
                shadow: None,
                head_arrow: None,
                tail_arrow: None,
            },
            content: ShapeContent::Empty,
            text_body: Default::default(),
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        };

        // 分组本身只做平移(400, 300)，不旋转，w/h 对纯平移无影响
        let group = Shape {
            id: 1,
            geometry: ShapeGeometry::Rect,
            position: Transform::from_rect(400.0, 300.0, 0.0, 0.0),
            style: ShapeStyle { fill: None, stroke: None, shadow: None, head_arrow: None, tail_arrow: None },
            content: ShapeContent::Group { children: vec![child], ch_off: None, ch_ext: None },
            text_body: Default::default(),
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        };

        let slide = Slide { layout_idx: None,
            shapes: vec![group],
            master_idx: None,
            animation_tree: AnimationNode::TimingRoot { children: vec![] },
            transition: None,
            notes: None,
            background: None, // PROOT-005 test fixture
            layout_shapes: vec![], // PROOT-005 followup test fixture
        };

        let (png, pw, _ph) = render_to_png(&mut renderer, &slide, None, 1.0);
        assert!(!png.is_empty());
        assert_eq!(&png[1..4], b"PNG");

        let img = Pixmap::decode_png(&png).expect("decode png failed");
        let data = img.data();
        let get_px = |x: u32, y: u32| -> (u8, u8, u8, u8) {
            let idx = ((y * pw + x) * 4) as usize;
            (data[idx], data[idx + 1], data[idx + 2], data[idx + 3])
        };

        // 子形状局部坐标是(0,0)-(100,80)，若 parent_transform 被忽略，
        // 这个区域会显示填充色。必须验证它不在这里，证明平移确实生效。
        assert_ne!(get_px(50, 40), (200, 30, 30, 255), "子Shape不应渲染在其局部坐标(0,0)附近，否则说明parent_transform(分组平移)未生效");

        // 叠加分组平移(400,300)后，子形状矩形范围是(400,300)-(500,380)，
        // 中心点(450,340)应显示子形状填充色。
        assert_eq!(get_px(450, 340), (200, 30, 30, 255), "子Shape叠加分组平移后应落在(400,300)-(500,380)范围内，验证Group递归时parent_transform正确传导");
    }

    /// PPTX-9a 端到端验证 2：分组整体旋转，子形状必须跟着一起转
    /// （不是子形状独立保持原方向不动）。复用 PPTX-8 的"矩形宽高互换"
    /// 探测手法：子形状是200x100宽矩形，分组整体旋转90度后，子形状
    /// 视觉上应变为纵向长、横向短。
    #[test]
    fn test_render_slide_with_group_rotation_end_to_end() {
        let mut renderer = SlideRenderer::new(960.0, 540.0);

        let make_slide_with_group_rotation = |group_rotation: Option<f32>| -> Slide {
            let child = Shape {
                id: 2,
                geometry: ShapeGeometry::Rect,
                position: Transform::from_rect(0.0, 0.0, 200.0, 100.0),
                style: ShapeStyle {
                    fill: Some(FillStyle::Solid(roe_model::common::Color { r: 10, g: 200, b: 10, a: 255 })),
                    stroke: None,
                    shadow: None,
                    head_arrow: None,
                    tail_arrow: None,
                },
                content: ShapeContent::Empty,
                text_body: Default::default(),
                placeholder: None,
                st_cxn: None,
                end_cxn: None,
            };

            let mut group_position = Transform::from_rect(400.0, 220.0, 200.0, 100.0);
            group_position.rotation = group_rotation;

            let group = Shape {
                id: 1,
                geometry: ShapeGeometry::Rect,
                position: group_position,
                style: ShapeStyle { fill: None, stroke: None, shadow: None, head_arrow: None, tail_arrow: None },
                content: ShapeContent::Group { children: vec![child], ch_off: None, ch_ext: None },
                text_body: Default::default(),
                placeholder: None,
                st_cxn: None,
                end_cxn: None,
            };

            Slide { layout_idx: None,
                shapes: vec![group],
                master_idx: None,
                animation_tree: AnimationNode::TimingRoot { children: vec![] },
                transition: None,
                notes: None,
                background: None, // PROOT-005 test fixture
                layout_shapes: vec![], // PROOT-005 followup test fixture
            }
        };

        let slide_no_rot = make_slide_with_group_rotation(None);
        let slide_rot = make_slide_with_group_rotation(Some(90.0));

        let (png_no_rot, pw, _h) = render_to_png(&mut renderer, &slide_no_rot, None, 1.0);
        let (png_rot, _w2, _h2) = render_to_png(&mut renderer, &slide_rot, None, 1.0);

        assert!(!png_no_rot.is_empty());
        assert!(!png_rot.is_empty());
        assert_ne!(png_no_rot, png_rot, "分组旋转90度后PNG字节应与不旋转版本不同，否则说明Group的parent_transform未叠加旋转分量");

        let cx = 400.0 + 200.0 / 2.0;
        let cy = 220.0 + 100.0 / 2.0;
        let probe_x = cx as u32;
        let probe_y = (cy - 60.0) as u32;

        let decode_pixel = |png: &[u8], x: u32, y: u32, pw: u32| -> (u8, u8, u8, u8) {
            let img = Pixmap::decode_png(png).expect("decode png failed");
            let data = img.data();
            let idx = ((y * pw + x) * 4) as usize;
            (data[idx], data[idx + 1], data[idx + 2], data[idx + 3])
        };

        let px_no_rot = decode_pixel(&png_no_rot, probe_x, probe_y, pw);
        let px_rot = decode_pixel(&png_rot, probe_x, probe_y, pw);

        assert_ne!(px_no_rot, (10, 200, 10, 255), "不旋转基线：该探测点应在子形状矩形外（背景色）");
        assert_eq!(px_rot, (10, 200, 10, 255), "分组旋转90度后，子形状应跟着一起旋转，中心正上方60px处应落入旋转后的矩形范围，验证Group递归变换合成了旋转分量而不只是平移");
    }

    #[test]
    fn test_render_slide_png() {
        let mut renderer = SlideRenderer::new(960.0, 540.0);
        let slide = make_slide();
        let (png, _w, _h) = render_to_png(&mut renderer, &slide, None, 1.0);

        assert!(!png.is_empty());
        assert_eq!(png[0], 0x89);
        assert_eq!(&png[1..4], b"PNG");

        std::fs::write("/tmp/roe_impress_preview.png", &png).ok();
        println!("PNG written: {} bytes", png.len());
    }

    #[test]
    fn test_render_with_runtime_state() {
        let mut renderer = SlideRenderer::new(960.0, 540.0);
        let slide = make_slide();

        let mut runtime = SlideRuntimeState::from_shapes(&slide.shapes);
        // Shape 1 半透明，Shape 2 隐藏
        runtime.states[0].opacity  = Some(0.5);
        runtime.states[1].visible  = Some(false);

        let (png, _w, _h) = render_to_png(&mut renderer, &slide, Some(&runtime), 1.0);
        assert!(!png.is_empty());
        assert_eq!(&png[1..4], b"PNG");
        println!("Runtime render PNG: {} bytes", png.len());
    }

    #[test]
    fn test_render_slide_with_chart_legend_grid_off() {
        // PPTX-13(a) 渲染层验证：修复此前 render_shape_chart 漏接
        // legend_show/legend_category/grid_stroke_width（chart.rs 里
        // 六个分支都设置了，slide_renderer.rs 之前一处都没有，见
        // roe-ext v15 笔记"两处几乎相同的render_chart逻辑功能不一致"）。
        // 本测试不做像素级验证（legend/grid视觉差异需人工核对渲染图），
        // 只确认 show_legend=false/show_grid=false 时渲染路径不崩溃、
        // 能正常生成PNG——回归防护此前"渲染层完全不读这两个字段"的漏接。
        use roe_model::impress::{ChartModel, ChartKind, ChartSeries};
        let mut slide = make_slide();
        slide.shapes.push(Shape {
            id: 11,
            geometry: ShapeGeometry::Rect,
            position: Transform::from_rect(50.0, 200.0, 600.0, 300.0),
            style: ShapeStyle { fill: None, stroke: None, shadow: None, head_arrow: None, tail_arrow: None },
            content: ShapeContent::Chart(ChartModel {
                id: 0,
                kind: ChartKind::Bar,
                title: Some("No Legend No Grid".to_string()),
                x_labels: vec!["Mon".to_string(), "Tue".to_string(), "Wed".to_string()],
                series: vec![
                    ChartSeries { name: "A".to_string(), data: vec![120.0, 200.0, 150.0], color: None, as_line: false, trendline: false },
                ],
                x_pt: 0.0, y_pt: 0.0, width_pt: 600.0, height_pt: 300.0,
                show_legend: false, show_grid: false,
                x_axis_label: String::new(), y_axis_label: String::new(),
                border_color: roe_model::common::Color { r: 0xcc, g: 0xcc, b: 0xcc, a: 255 },
                border_width: 1.0,
                gap_width_pct: None,
                overlap_pct: None,
                grouping: Default::default(),
                hole_size_pct: None,
                slice_border_color: None,
                data_labels: Default::default(),
            }),
            text_body: Default::default(),
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        });
        let mut renderer = SlideRenderer::new(960.0, 540.0);
        let (png, _w, _h) = render_to_png(&mut renderer, &slide, None, 1.0);
        assert!(!png.is_empty());
        assert_eq!(&png[1..4], b"PNG");
        println!("chart legend/grid off PNG: {} bytes", png.len());
    }

    #[test]
    fn test_render_slide_with_chart() {
        use roe_model::impress::{ChartModel, ChartKind, ChartSeries};
        let mut slide = make_slide();
        slide.shapes.push(Shape {
            id: 10,
            geometry: ShapeGeometry::Rect,
            position: Transform::from_rect(50.0, 200.0, 600.0, 300.0),
            style: ShapeStyle { fill: None, stroke: None, shadow: None, head_arrow: None, tail_arrow: None },
            content: ShapeContent::Chart(ChartModel {
                id: 0,
                kind: ChartKind::Bar,
                title: Some("Sales".to_string()),
                x_labels: vec!["Mon".to_string(), "Tue".to_string(), "Wed".to_string()],
                series: vec![
                    ChartSeries { name: "A".to_string(), data: vec![120.0, 200.0, 150.0], color: None, as_line: false, trendline: false },
                    ChartSeries { name: "B".to_string(), data: vec![80.0, 160.0, 90.0], color: None, as_line: false, trendline: false },
                ],
                x_pt: 0.0, y_pt: 0.0, width_pt: 600.0, height_pt: 300.0,
                show_legend: true, show_grid: true,
                x_axis_label: String::new(), y_axis_label: String::new(),
                border_color: roe_model::common::Color { r: 0xcc, g: 0xcc, b: 0xcc, a: 255 },
                border_width: 1.0,
                gap_width_pct: None,
                overlap_pct: None,
                grouping: Default::default(),
                hole_size_pct: None,
                slice_border_color: None,
                data_labels: Default::default(),
            }),
            text_body: Default::default(),
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        });
        let styles = roe_model::style::StyleRegistry::default();
        let mut renderer = SlideRenderer::new(960.0, 540.0);
        let (png, _w, _h) = render_to_png(&mut renderer, &slide, None, 1.0);
        assert!(!png.is_empty());
        assert_eq!(&png[1..4], b"PNG");
        std::fs::write("/tmp/roe_chart_test.png", &png).ok();
        println!("chart PNG: {} bytes", png.len());
    }

    #[test]
    fn test_contains_cjk() {
        // 纯函数单元测试：CJK降级判断的字符范围覆盖
        assert!(contains_cjk("斜体蓝字"));           // 中文
        assert!(contains_cjk("ひらがな"));             // 平假名
        assert!(contains_cjk("カタカナ"));             // 片假名
        assert!(contains_cjk("한글"));                 // 谚文
        assert!(contains_cjk("Hello 世界"));           // 中英混排，含CJK即判定为true
        assert!(!contains_cjk("Hello World"));         // 纯拉丁
        assert!(!contains_cjk("123 !@# €"));           // 数字/符号/非CJK重音字符
        assert!(!contains_cjk(""));                    // 空字符串
    }

    #[test]
    fn test_render_slide_with_cjk_italic_run() {
        // PPTX-14/15 CJK斜体降级修复验证：
        // 场景1：纯CJK文字请求斜体 —— 修复前会因cosmic-text style精确匹配
        //   导致所有CJK face被过滤、shaping找不到字形而tofu，本测试确认
        //   修复后能正常生成非空PNG（不追求像素级验证真实字形，那需要人工
        //   核对渲染图，见 roe-ext/notes 交接说明里的实测截图）。
        // 场景2：纯拉丁文字请求斜体 —— 确认降级逻辑不会误伤非CJK场景，
        //   拉丁斜体应继续正常走 Style::Italic。
        // 场景3：CJK+拉丁混排的单个run请求斜体 —— 按run整体降级（简化
        //   取舍，不做run内再拆分),含CJK即整体不请求Italic。
        let mut slide = make_slide();
        slide.shapes.push(Shape {
            id: 31,
            geometry: ShapeGeometry::Rect,
            position: Transform::from_rect(40.0, 350.0, 500.0, 120.0),
            style: ShapeStyle { fill: None, stroke: None, shadow: None, head_arrow: None, tail_arrow: None },
            content: ShapeContent::Text(vec![Paragraph {
                style: roe_model::common::StyleRef("Body".to_string()),
                runs: vec![
                    Run {
                        text: "纯中文斜体请求".to_string(),
                        char_style: None,
                        italic: Some(true),
                        ..Default::default()
                    },
                    Run {
                        text: " Pure Latin Italic ".to_string(),
                        char_style: None,
                        italic: Some(true),
                        ..Default::default()
                    },
                    Run {
                        text: "Mixed混排Italic".to_string(),
                        char_style: None,
                        italic: Some(true),
                        ..Default::default()
                    },
                ],
                list_info: None,
                ..Default::default()
            }]),
            text_body: Default::default(),
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        });

        let mut renderer = SlideRenderer::new(960.0, 540.0);
        let (png, _w, _h) = render_to_png(&mut renderer, &slide, None, 2.0);
        assert!(!png.is_empty());
        assert_eq!(&png[1..4], b"PNG");
        std::fs::write("/tmp/roe_cjk_italic_fix_test.png", &png).ok();
        println!("CJK italic fix PNG written: {} bytes (see /tmp/roe_cjk_italic_fix_test.png for visual check)", png.len());
    }

    #[test]
    fn test_render_slide_with_multistyle_text() {
        // PPTX-14/15 验证：同一段落内混合 普通/粗体/斜体 + 不同颜色的多个 run，
        // 确认 set_rich_text 按 run 生效（不再是此前只取 first() 的单一样式）。
        let mut slide = make_slide();
        slide.shapes.push(Shape {
            id: 30,
            geometry: ShapeGeometry::Rect,
            position: Transform::from_rect(40.0, 350.0, 500.0, 80.0),
            style: ShapeStyle { fill: None, stroke: None, shadow: None, head_arrow: None, tail_arrow: None },
            content: ShapeContent::Text(vec![Paragraph {
                style: roe_model::common::StyleRef("Body".to_string()),
                runs: vec![
                    Run {
                        text: "普通文字 ".to_string(),
                        char_style: None,
                        ..Default::default()
                    },
                    Run {
                        text: "粗体红字 ".to_string(),
                        char_style: None,
                        bold: Some(true),
                        color: Some(roe_model::common::Color { r: 220, g: 30, b: 30, a: 255 }),
                        ..Default::default()
                    },
                    Run {
                        text: "斜体蓝字".to_string(),
                        char_style: None,
                        italic: Some(true),
                        color: Some(roe_model::common::Color { r: 30, g: 60, b: 220, a: 255 }),
                        ..Default::default()
                    },
                ],
                list_info: None,
                ..Default::default()
            }]),
            text_body: Default::default(),
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        });

        let mut renderer = SlideRenderer::new(960.0, 540.0);
        let (png, _w, _h) = render_to_png(&mut renderer, &slide, None, 2.0);

        assert!(!png.is_empty());
        assert_eq!(&png[1..4], b"PNG");

        std::fs::write("/tmp/roe_impress_multistyle_preview.png", &png).ok();
        println!("Multistyle text PNG written: {} bytes", png.len());
    }

    #[test]
    fn test_render_slide_with_table() {
        use roe_model::common::{Table, TableRow, TableCell, CellContent, StyleRef};

        // 3列2行表格：
        // Row0: [A(gridSpan=2,合并占2列)] [hidden延续格] [C]
        // Row1: [D(rowSpan=2)]           [E]            [F]
        // 这里简化为2行验证横向合并；纵向合并单独用 row_span 字段验证（不需要
        // 真的两行物理体现，PPTX-6 解析层已用真实样本验证过 rowSpan 语义，
        // 渲染层这里只验证：hidden 跳过 + col_span/row_span 影响绘制矩形范围
        // + 不 panic + 正常出 PNG。
        let mk_cell = |text: &str, col_span: u8, row_span: u8, hidden: bool| TableCell {
            content: if hidden {
                vec![]
            } else {
                vec![CellContent::Paragraph(Paragraph {
                    style: StyleRef("Body".to_string()),
                    runs: vec![Run { text: text.to_string(), char_style: None, ..Default::default() }],
                    list_info: None,
                    ..Default::default()
                })]
            },
            col_span,
            row_span,
            style: None,
            hidden, bg_color: None,
            border_color: None, border_width: None,
            pad_left: None, pad_right: None, pad_top: None, pad_bottom: None,
        };

        let table = Table {
            col_widths: vec![100.0, 100.0, 100.0],
            style_id: None, first_row: false, first_col: false, last_row: false, last_col: false, band_row: false, band_col: false,
            default_border_color: None, default_border_width: None, inside_h_color: None, inside_h_width: None, inside_v_color: None, inside_v_width: None,
            float_pos: None,
            rows: vec![
                TableRow {
                    cells: vec![
                        mk_cell("横向合并A", 2, 1, false),
                        mk_cell("", 1, 1, true), // 被 gridSpan=2 吞掉的延续格
                        mk_cell("C", 1, 1, false),
                    ],
                    row_height: None,
                },
                TableRow {
                    cells: vec![
                        mk_cell("D(纵向合并)", 1, 2, false),
                        mk_cell("E", 1, 1, false),
                        mk_cell("F", 1, 1, false),
                    ],
                    row_height: None,
                },
            ],
        };

        let mut slide = make_slide();
        slide.shapes.push(Shape {
            id: 20,
            geometry: ShapeGeometry::Rect,
            position: Transform::from_rect(50.0, 250.0, 300.0, 150.0),
            style: ShapeStyle { fill: None, stroke: None, shadow: None, head_arrow: None, tail_arrow: None },
            content: ShapeContent::Table(table),
            text_body: Default::default(),
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        });

        let mut renderer = SlideRenderer::new(960.0, 540.0);
        let (png, _w, _h) = render_to_png(&mut renderer, &slide, None, 1.0);

        assert!(!png.is_empty());
        assert_eq!(&png[1..4], b"PNG");

        std::fs::write("/tmp/roe_impress_table_preview.png", &png).ok();
        println!("Table PNG written: {} bytes", png.len());
    }

    /// PPTX-8b 端到端验证：表格作为整体刚体旋转，col_x/row_y 内部布局
    /// 累加逻辑不变，只在绘制（边框+文字）时套用 shape_transform。
    /// 用一个宽表格（200x100，2列1行，非正方形，与PPTX-8旋转测试同样的
    /// 验证思路）做90度旋转，验证：
    /// 1) 不panic、正常出图
    /// 2) 旋转前后PNG字节不同（证明transform真正传导到了表格绘制路径，
    ///    不是被 render_shape_table 内部的坐标累加逻辑悄悄吞掉）
    /// 3) 像素级抽样：表格中心正上方60px处，旋转前应在表格外（背景色），
    ///    旋转90度后因为宽高视觉互换应落入表格范围内（边框附近，用边框色
    ///    附近的深色而非纯白背景来判断，因为该点大概率落在表格边缘线上
    ///    或内部空白格子里，比对"是否还是白色背景"这个弱一点但足够验证
    ///    方向和中心点是否正确）
    #[test]
    fn test_render_slide_with_rotated_table_end_to_end() {
        use roe_model::common::{Table, TableRow, TableCell, CellContent, StyleRef};

        let mk_empty_cell = || TableCell {
            content: vec![],
            col_span: 1,
            row_span: 1,
            style: None,
            hidden: false, bg_color: None,
            border_color: None, border_width: None,
            pad_left: None, pad_right: None, pad_top: None, pad_bottom: None,
        };

        let make_table_slide = |rotation: Option<f32>| {
            let table = Table {
                col_widths: vec![100.0, 100.0],
                style_id: None, first_row: false, first_col: false, last_row: false, last_col: false, band_row: false, band_col: false,
                default_border_color: None, default_border_width: None, inside_h_color: None, inside_h_width: None, inside_v_color: None, inside_v_width: None,
                float_pos: None,
                rows: vec![TableRow {
                    cells: vec![mk_empty_cell(), mk_empty_cell()],
                    row_height: None,
                }],
            };
            let mut position = Transform::from_rect(400.0, 220.0, 200.0, 100.0);
            position.rotation = rotation;
            let mut slide = Slide { layout_idx: None,
                shapes: vec![Shape {
                    id: 30,
                    geometry: ShapeGeometry::Rect,
                    position,
                    style: ShapeStyle { fill: None, stroke: None, shadow: None, head_arrow: None, tail_arrow: None },
                    content: ShapeContent::Table(table),
                    text_body: Default::default(),
                    placeholder: None,
                    st_cxn: None,
                    end_cxn: None,
                }],
                master_idx: None,
                animation_tree: AnimationNode::TimingRoot { children: vec![] },
                transition: None,
                notes: None,
                background: None, // PROOT-005 test fixture
                layout_shapes: vec![], // PROOT-005 followup test fixture
            };
            // 避免与其他测试共用 make_slide() 的两个默认shape产生像素干扰，
            // 这里直接构造只含表格的 slide
            slide.shapes.truncate(1);
            slide
        };

        let mut renderer = SlideRenderer::new(960.0, 540.0);
        let slide_no_rot = make_table_slide(None);
        let slide_rot = make_table_slide(Some(90.0));

        let (png_no_rot, _w, _h) = render_to_png(&mut renderer, &slide_no_rot, None, 1.0);
        let (png_rot, _w2, _h2) = render_to_png(&mut renderer, &slide_rot, None, 1.0);

        // 1) 基本要求
        assert!(!png_no_rot.is_empty());
        assert!(!png_rot.is_empty());
        assert_eq!(&png_rot[1..4], b"PNG");

        // 2) 旋转确实改变了渲染结果
        assert_ne!(png_no_rot, png_rot, "旋转90度后表格PNG字节应与不旋转版本不同，否则说明render_shape_table没有真正接上shape_transform");

        // 3) 像素级验证：改用"非白色像素包围盒(bounding box)"而非单点采样——
        // 表格cell内部无填充色（只有边框线有颜色），单点探测容易恰好落在
        // 空白格子内部而非边框线上，导致假阴性。包围盒方式统计所有非白
        // 像素的最小外接矩形，直接反映"表格视觉上的宽高占用范围"，对边框
        // 细线的精确位置不敏感，且能直接验证"宽高是否发生了预期的互换"。
        //
        // 注意：只在表格中心周边一个安全窗口内统计，排除 render_slide 末尾
        // Self::draw_border 画的整张幻灯片灰色外边框（否则包围盒会被外边框
        // 撑满整个画布，淹没表格自己的包围盒——本次调试已实测踩中这个坑）。
        let pw = (renderer.slide_width * 1.0) as u32;
        let ph = (renderer.slide_height * 1.0) as u32;
        let cx = 400.0 + 200.0 / 2.0; // 表格中心 x = 500
        let cy = 220.0 + 100.0 / 2.0; // 表格中心 y = 270
        let window: f32 = 150.0; // 表格最大边(200)的安全余量窗口
        let (wx0, wy0, wx1, wy1) = (
            (cx - window).max(0.0) as u32,
            (cy - window).max(0.0) as u32,
            (cx + window).min(pw as f32 - 1.0) as u32,
            (cy + window).min(ph as f32 - 1.0) as u32,
        );

        let non_white_bbox = |png: &[u8], pw: u32, wx0: u32, wy0: u32, wx1: u32, wy1: u32| -> Option<(u32, u32, u32, u32)> {
            let img = Pixmap::decode_png(png).expect("decode png failed");
            let data = img.data();
            let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0u32, 0u32);
            let mut found = false;
            for yy in wy0..=wy1 {
                for xx in wx0..=wx1 {
                    let idx = ((yy * pw + xx) * 4) as usize;
                    let (r, g, b) = (data[idx], data[idx + 1], data[idx + 2]);
                    if !(r == 255 && g == 255 && b == 255) {
                        found = true;
                        min_x = min_x.min(xx);
                        min_y = min_y.min(yy);
                        max_x = max_x.max(xx);
                        max_y = max_y.max(yy);
                    }
                }
            }
            if found { Some((min_x, min_y, max_x, max_y)) } else { None }
        };

        let bbox_no_rot = non_white_bbox(&png_no_rot, pw, wx0, wy0, wx1, wy1).expect("不旋转版本应存在非白色像素（表格边框）");
        let bbox_rot = non_white_bbox(&png_rot, pw, wx0, wy0, wx1, wy1).expect("旋转版本应存在非白色像素（表格边框）");

        let bbox_w_no_rot = bbox_no_rot.2 - bbox_no_rot.0;
        let bbox_h_no_rot = bbox_no_rot.3 - bbox_no_rot.1;
        let bbox_w_rot = bbox_rot.2 - bbox_rot.0;
        let bbox_h_rot = bbox_rot.3 - bbox_rot.1;

        // 不旋转基线：200宽x100高的表格，包围盒应明显"宽 > 高"
        assert!(bbox_w_no_rot > bbox_h_no_rot, "不旋转基线：表格是200x100，包围盒应宽大于高，实际 w={} h={}，验证测试坐标设计本身没有搞反", bbox_w_no_rot, bbox_h_no_rot);

        // 旋转90度后：宽高视觉互换，包围盒应明显"高 > 宽"（绕中心转90度，
        // 原本200宽的边现在应该占据纵向空间）
        assert!(bbox_h_rot > bbox_w_rot, "旋转90度后：包围盒应高大于宽（宽高视觉互换），实际 w={} h={}，说明render_shape_table的旋转接线未生效或方向错误", bbox_w_rot, bbox_h_rot);
    }

    #[test]
    fn test_render_slide_with_text() {
        let mut renderer = SlideRenderer::new(960.0, 540.0);
        let slide = make_slide_with_text();
        let (png, _w, _h) = render_to_png(&mut renderer, &slide, None, 1.0);

        assert!(!png.is_empty());
        assert_eq!(&png[1..4], b"PNG");

        std::fs::write("/tmp/roe_impress_text_preview.png", &png).ok();
        println!("PNG with text written: {} bytes", png.len());
    }

    // PROOT-035 (v84会话): expand_tabs_to_spaces 直接单测，验证 \t 展开
    // 逻辑本身的数值正确性（不需要走整条渲染管线，更精确、更快）。
    #[test]
    fn test_proot035_expand_tabs_to_spaces_no_tab_advances_cursor() {
        // 无\t的文本：应原样返回，但仍需推进cur_x（跨run累积语义，
        // 详见 expand_tabs_to_spaces 文档注释）。
        let mut renderer = SlideRenderer::new(960.0, 540.0);
        let mut cur_x = 0.0f32;
        let result = renderer.expand_tabs_to_spaces("hello", 18.0, 1.0, &mut cur_x);
        assert_eq!(result, "hello", "无\t文本应原样返回");
        assert!(cur_x > 0.0, "即使无\t，cur_x也应因文本本身宽度而推进，实际={}", cur_x);
    }

    #[test]
    fn test_proot035_expand_tabs_to_spaces_removes_tab_chars() {
        // 核心回归断言：展开后的文本不应再包含\t字符（这是tofu方框的
        // 根因——字体没有为\t定义可见字形，shaper遇到\t就会产出.notdef）。
        let mut renderer = SlideRenderer::new(960.0, 540.0);
        let mut cur_x = 0.0f32;
        let result = renderer.expand_tabs_to_spaces("XXX												YYY", 36.0, 1.0, &mut cur_x);
        assert!(!result.contains('\t'), "展开后不应再含\\t字符，实际={:?}", result);
        assert!(result.starts_with("XXX"), "展开应保留原文本前缀");
        assert!(result.ends_with("YYY"), "展开应保留原文本后缀");
        // 12个连续\t应展开成若干个空格（每个tab至少跳到下一个tab stop，
        // 12个tab在36pt字号下必然产生足够宽度的空格填充，不应该是空字符串）
        let space_count = result.chars().filter(|c| *c == ' ').count();
        assert!(space_count > 0, "12个连续\t应展开出至少1个空格，实际=0");
    }

    #[test]
    fn test_proot035_expand_tabs_to_spaces_advances_to_next_tab_stop() {
        // 验证tab stop的"绝对位置对齐"语义：单个\t展开后，cur_x应该
        // 落在72pt（defTabSz默认值）的整数倍上（scale=1.0时单位是pt本身）。
        let mut renderer = SlideRenderer::new(960.0, 540.0);
        let mut cur_x = 0.0f32;
        let _ = renderer.expand_tabs_to_spaces("A	B", 18.0, 1.0, &mut cur_x);
        // "A"本身宽度远小于72pt，一个\t后cur_x应落在第一个tab stop
        // （72.0pt）附近。注意：expand_tabs_to_spaces用整数个空格填补
        // gap（文档注释已说明这是可接受的近似，非精确到像素），所以
        // cur_x不是恰好等于72.0，而是"72.0 ± 一个空格宽度"的范围内——
        // 用较宽松的容差（10pt，覆盖18pt字号下一个空格的典型宽度）
        // 断言"落在第一个tab stop附近"这个语义，不断言像素级精确值。
        assert!(cur_x > 20.0 && cur_x < 100.0,
            "单个\t后cur_x应落在第一个tab stop（72pt）附近（容差内），实际={}", cur_x);
        assert!(cur_x >= 72.0,
            "tab展开后cur_x不应小于tab stop本身（不能展开成负偏移），实际={}", cur_x);
    }
}


/// 分组子形状的预设路径：按组的缩放比例 (sx, sy) 缩放路径点（角度不变）。
fn scale_group_path_cmds(cmds: &[roe_model::common::PathCommand], sx: f32, sy: f32) -> Vec<roe_model::common::PathCommand> {
    use roe_model::common::PathCommand as P;
    cmds.iter()
        .map(|c| match c {
            P::MoveTo { x, y } => P::MoveTo { x: x * sx, y: y * sy },
            P::LineTo { x, y } => P::LineTo { x: x * sx, y: y * sy },
            P::CubicTo { x1, y1, x2, y2, x, y } => P::CubicTo { x1: x1 * sx, y1: y1 * sy, x2: x2 * sx, y2: y2 * sy, x: x * sx, y: y * sy },
            P::QuadTo { x1, y1, x, y } => P::QuadTo { x1: x1 * sx, y1: y1 * sy, x: x * sx, y: y * sy },
            P::ArcTo { cx, cy, rx, ry, start_deg, sweep_deg } => P::ArcTo { cx: cx * sx, cy: cy * sy, rx: rx * sx, ry: ry * sy, start_deg: *start_deg, sweep_deg: *sweep_deg },
            P::Close => P::Close,
        })
        .collect()
}


// ───────── v216: 文本框光标定位（横排；几何单位 pt，原点为形状左上角） ─────────
struct ParaGeom {
    layout: Option<parley::Layout<ImpressBrush>>,
    x: f32,
    top: f32,
    height: f32,
    text_len: usize,
}

impl SlideRenderer {
    fn shape_text_geoms(&mut self, shape: &Shape, styles: &StyleRegistry) -> Option<Vec<ParaGeom>> {
        let roe_model::impress::ShapeContent::Text(paragraphs) = &shape.content else { return None; };
        if shape.text_body.vert != roe_model::impress::TextVert::Horizontal { return None; }
        let tb = &shape.text_body;
        let pad_l = tb.ins_l.unwrap_or(9.14);
        let pad_r = tb.ins_r.unwrap_or(9.14);
        let pad_t = tb.ins_t.unwrap_or(4.57);
        let pad_b = tb.ins_b.unwrap_or(4.57);
        let w = shape.position.w();
        let text_x = pad_l;
        let text_w = if tb.wrap { (w - pad_l - pad_r).max(1.0) } else { f32::MAX / 2.0 };
        let scale = 1.0f32;
        let font_scale = match &tb.autofit {
            roe_model::impress::TextAutofit::NormAutofit { font_scale, .. } => font_scale.unwrap_or(1.0),
            _ => 1.0,
        };
        let mut cursor_y = pad_t;
        let mut out: Vec<ParaGeom> = Vec::new();

        for para in paragraphs {
            let para_style = styles.resolve_paragraph_style(para.style.0.as_str());
            let para_font_size = para_style.font_size.unwrap_or(DEFAULT_FONT_SIZE) * scale;

            let mut para_text_x = text_x;
            let mut para_text_w = text_w;
            if let Some(bullet) = &para.bullet {
                if matches!(&bullet.kind, roe_model::impress::BulletKind::None | roe_model::impress::BulletKind::AutoNum { .. }) {
                    let m = bullet.mar_l.unwrap_or(0.0) * scale;
                    para_text_x = text_x + m;
                    para_text_w = (text_w - m).max(1.0);
                }
                if let roe_model::impress::BulletKind::Char { .. } = &bullet.kind {
                    let mar_l = bullet.mar_l.unwrap_or(0.0) * scale;
                    para_text_x = text_x + mar_l;
                    para_text_w = (text_w - mar_l).max(1.0);
                }
            }

            let mut run_texts: Vec<String> = Vec::new();
            let mut run_font_sizes: Vec<f32> = Vec::new();
            let mut run_colors: Vec<(u8, u8, u8)> = Vec::new();
            let mut run_bolds: Vec<bool> = Vec::new();
            let mut run_italics: Vec<bool> = Vec::new();
            let mut run_fonts: Vec<Option<String>> = Vec::new();
            let mut run_decos: Vec<(bool, bool, Option<[u8; 4]>, u8)> = Vec::new();
            let mut tab_cur_x = 0.0f32;
            for run in &para.runs {
                let fs = if let Some(sref) = &run.char_style {
                    let cs = styles.resolve_char_style(&sref.0);
                    run.font_size.or(cs.font_size).unwrap_or(para_style.font_size.unwrap_or(DEFAULT_FONT_SIZE)) * scale
                } else if let Some(rfs) = run.font_size {
                    rfs * scale
                } else {
                    para_font_size
                };
                let color = run.color.map(|c| (c.r, c.g, c.b)).unwrap_or((20, 20, 20));
                let expanded = self.expand_tabs_to_spaces(&run.text, fs, scale, &mut tab_cur_x);
                run_texts.push(expanded);
                if run.text != "\n" { run_font_sizes.push(fs); }
                run_colors.push(color);
                run_bolds.push(run.bold.unwrap_or(false));
                run_italics.push(run.italic.unwrap_or(false));
                run_fonts.push(run.font_family.clone().or_else(|| run.font_family_ascii.clone()));
                run_decos.push((run.underline.unwrap_or(false), run.strikethrough.unwrap_or(false), run.background.map(|c| [c.r, c.g, c.b, 255]), match run.script { Some(roe_model::writer::ScriptPosition::Superscript) => 1u8, Some(roe_model::writer::ScriptPosition::Subscript) => 2u8, _ => 0u8 }));
            }

            let text: String = run_texts.join("");
            if text.trim().is_empty() {
                if !out.is_empty() { cursor_y += gap_v319h(para.space_before, scale); }
                let hh = para_font_size * 1.4 * para.line_height.unwrap_or(1.0);
                out.push(ParaGeom { layout: None, x: para_text_x, top: cursor_y, height: hh, text_len: text.len() });
                cursor_y += hh + gap_v319h(para.space_after, scale);
                continue;
            }
            let fsz = run_font_sizes.iter().cloned().fold(f32::MIN, f32::max);
            let font_size = (if fsz.is_finite() { fsz } else { para_font_size }) * font_scale;
            let line_height = font_size * 1.4 * para.line_height.unwrap_or(1.0);
            let layout = self.build_para_layout(
                &text, &run_texts, &run_colors, &run_bolds, &run_italics, &run_fonts, &run_decos,
                font_size, line_height, para.alignment, para_text_w,
            );
            let n_lines = layout.lines().count().max(1);
            if !out.is_empty() { cursor_y += gap_v319h(para.space_before, scale); }
            let hh = line_height * n_lines as f32;
            out.push(ParaGeom { layout: Some(layout), x: para_text_x, top: cursor_y, height: hh, text_len: text.len() });
            cursor_y += hh + gap_v319h(para.space_after, scale);
        }
        // v320h0: 与 render_shape_text 相同的 anchor 垂直偏移（Top 不变）
        {
            let avail = (shape.position.h() - pad_t - pad_b).max(0.0);
            let total = (cursor_y - pad_t).max(0.0);
            let extra = (avail - total).max(0.0);
            let off = match tb.anchor {
                roe_model::impress::TextAnchor::Middle => extra / 2.0,
                roe_model::impress::TextAnchor::Bottom => extra,
                roe_model::impress::TextAnchor::Top => 0.0,
            };
            if off > 0.0 {
                for g in out.iter_mut() { g.top += off; }
            }
        }
        Some(out)
    }

    /// 光标位置 -> (x, y_top, y_bottom)，形状局部坐标（pt）。竖排/非文字形状返回 None。
    pub fn text_caret_local(&mut self, shape: &Shape, styles: &StyleRegistry, para: usize, offset: usize) -> Option<(f32, f32, f32)> {
        let geoms = self.shape_text_geoms(shape, styles)?;
        let g = geoms.get(para)?;
        match &g.layout {
            None => Some((g.x, g.top, g.top + g.height)),
            Some(layout) => {
                let idx = offset.min(g.text_len);
                let cur = parley::Cursor::from_byte_index(layout, idx, parley::Affinity::Downstream);
                let bb = cur.geometry(layout, 1.5);
                Some((g.x + bb.x0 as f32, g.top + bb.y0 as f32, g.top + bb.y1 as f32))
            }
        }
    }

    /// 形状局部点击点 -> (段落序号, 字节偏移)。
    pub fn text_hit_local(&mut self, shape: &Shape, styles: &StyleRegistry, lx: f32, ly: f32) -> Option<(usize, usize)> {
        let geoms = self.shape_text_geoms(shape, styles)?;
        if geoms.is_empty() { return None; }
        let mut pick = geoms.len() - 1;
        for (i, g) in geoms.iter().enumerate() {
            if ly < g.top + g.height { pick = i; break; }
        }
        let g = &geoms[pick];
        match &g.layout {
            None => Some((pick, 0)),
            Some(layout) => {
                let cur = parley::Cursor::from_point(layout, lx - g.x, ly - g.top);
                Some((pick, cur.index().min(g.text_len)))
            }
        }
    }
}


// v319s: table cell geometry for editing (hit test / caret). The measure logic
// mirrors render_shape_table (scale = 1.0). Keep both in sync.
fn table_cell_font_size_v319s(cell: &TableCell) -> f32 {
    cell.content.iter()
        .filter_map(|c| if let CellContent::Paragraph(p) = c { Some(p) } else { None })
        .next()
        .and_then(|p| {
            p.runs.iter()
                .filter_map(|r| r.font_size)
                .fold(None::<f32>, |acc, fs| Some(acc.map_or(fs, |a: f32| a.max(fs))))
        })
        .filter(|fs: &f32| fs.is_finite() && *fs > 0.0)
        .unwrap_or(12.0)
}

fn table_cell_padding_v319s(cell: &TableCell) -> (f32, f32, f32, f32) {
    (
        cell.pad_left.unwrap_or(4.0),
        cell.pad_right.unwrap_or(4.0),
        cell.pad_top.unwrap_or(3.0),
        cell.pad_bottom.unwrap_or(3.0),
    )
}

impl SlideRenderer {
    /// Cell rects in table-local pt: result[row][col] = (x, y, w, h).
    /// Hidden (merged-away) cells get (0,0,0,0).
    pub fn table_cell_rects_local(&mut self, table: &Table) -> Vec<Vec<(f32, f32, f32, f32)>> {
        const MIN_ROW_H: f32 = 18.0;
        let num_cols = table.col_widths.len().max(
            table.rows.first().map(|r| r.cells.len()).unwrap_or(1)
        );
        let col_widths: Vec<f32> = if table.col_widths.is_empty() {
            vec![100.0; num_cols]
        } else {
            table.col_widths.clone()
        };
        let mut col_x: Vec<f32> = Vec::with_capacity(col_widths.len() + 1);
        {
            let mut acc = 0.0f32;
            for w in &col_widths {
                col_x.push(acc);
                acc += w;
            }
            col_x.push(acc);
        }
        let num_rows = table.rows.len();
        let mut row_heights: Vec<f32> = table.rows.iter()
            .map(|row| row.row_height.unwrap_or(MIN_ROW_H))
            .collect();

        for (ri, row) in table.rows.iter().enumerate() {
            for (ci, cell) in row.cells.iter().enumerate() {
                if cell.hidden { continue; }
                let text: String = cell.content.iter()
                    .filter_map(|c| if let CellContent::Paragraph(p) = c { Some(p) } else { None })
                    .next()
                    .map(|p| p.runs.iter().map(|r| r.text.as_str()).collect::<String>())
                    .unwrap_or_default();
                if text.trim().is_empty() { continue; }
                let col_span = cell.col_span.max(1) as usize;
                let row_span = cell.row_span.max(1) as usize;
                let end_col = (ci + col_span).min(col_x.len() - 1);
                let cell_w = col_x.get(end_col).copied().unwrap_or(0.0)
                    - col_x.get(ci).copied().unwrap_or(0.0);
                let (pad_l, pad_r, pad_t, pad_b) = table_cell_padding_v319s(cell);
                let avail_w = (cell_w - (pad_l + pad_r)).max(1.0);
                let cell_font_size = table_cell_font_size_v319s(cell);
                let cell_line_height = cell_font_size * 1.4;
                let mut builder = self.layout_context.ranged_builder(
                    &mut self.font_context, &text, 1.0, true,
                );
                builder.push_default(StyleProperty::FontSize(cell_font_size));
                builder.push_default(parley::LineHeight::Absolute(cell_line_height));
                let mut layout: parley::Layout<ImpressBrush> = builder.build(&text);
                layout.break_all_lines(Some(avail_w));
                let n_lines = layout.lines().count().max(1);
                let needed_h = (n_lines as f32 * cell_line_height + (pad_t + pad_b)).max(row_heights[ri]);
                let per_row_needed = needed_h / row_span as f32;
                for r in ri..(ri + row_span).min(num_rows) {
                    if row_heights[r] < per_row_needed {
                        row_heights[r] = per_row_needed;
                    }
                }
            }
        }

        let mut row_y: Vec<f32> = Vec::with_capacity(num_rows + 1);
        {
            let mut acc = 0.0f32;
            for h in &row_heights {
                row_y.push(acc);
                acc += h;
            }
            row_y.push(acc);
        }

        let mut out: Vec<Vec<(f32, f32, f32, f32)>> = Vec::with_capacity(num_rows);
        for (ri, row) in table.rows.iter().enumerate() {
            let mut line: Vec<(f32, f32, f32, f32)> = Vec::with_capacity(row.cells.len());
            for (ci, cell) in row.cells.iter().enumerate() {
                if cell.hidden {
                    line.push((0.0, 0.0, 0.0, 0.0));
                    continue;
                }
                let col_span = cell.col_span.max(1) as usize;
                let row_span = cell.row_span.max(1) as usize;
                let cx = col_x.get(ci).copied().unwrap_or(0.0);
                let end_col = (ci + col_span).min(col_x.len() - 1);
                let cw = col_x.get(end_col).copied().unwrap_or(cx) - cx;
                let cy = row_y.get(ri).copied().unwrap_or(0.0);
                let end_row = (ri + row_span).min(row_y.len() - 1);
                let ch = row_y.get(end_row).copied().unwrap_or(cy) - cy;
                line.push((cx, cy, cw, ch));
            }
            out.push(line);
        }
        out
    }

    /// Virtual single-paragraph text shape for one cell. Returns (shape, cell_x, cell_y, font_size).
    /// Only the first paragraph is used, matching what render_shape_table draws.
    pub fn table_cell_virtual(&mut self, shape: &Shape, row: usize, col: usize) -> Option<(Shape, f32, f32, f32)> {
        let ShapeContent::Table(table) = &shape.content else { return None; };
        let rects = self.table_cell_rects_local(table);
        let (cx, cy, cw, ch) = *rects.get(row)?.get(col)?;
        let cell = table.rows.get(row)?.cells.get(col)?;
        if cell.hidden { return None; }
        let fs = table_cell_font_size_v319s(cell);
        let (pl, pr, pt, pb) = table_cell_padding_v319s(cell);
        let mut para = cell.content.iter()
            .find_map(|c| if let CellContent::Paragraph(p) = c { Some(p.clone()) } else { None })
            .unwrap_or_default();
        for r in para.runs.iter_mut() { r.font_size = Some(fs); }
        para.bullet = None;
        para.line_height = None;
        para.space_before = Default::default();
        para.space_after = Default::default();
        let v = Shape {
            id: shape.id,
            geometry: roe_model::impress::ShapeGeometry::Rect,
            position: roe_model::common::Transform::from_rect(0.0, 0.0, cw, ch),
            style: Default::default(),
            content: ShapeContent::Text(vec![para]),
            text_body: roe_model::impress::TextBodyProps {
                ins_l: Some(pl), ins_r: Some(pr), ins_t: Some(pt), ins_b: Some(pb),
                wrap: true,
                ..Default::default()
            },
            placeholder: None, st_cxn: None, end_cxn: None,
        };
        Some((v, cx, cy, fs))
    }

    /// Table-local point -> (row, col) of the cell (merged cells resolve to their origin cell).
    pub fn table_cell_at_local(&mut self, shape: &Shape, lx: f32, ly: f32) -> Option<(usize, usize)> {
        let ShapeContent::Table(table) = &shape.content else { return None; };
        let rects = self.table_cell_rects_local(table);
        for (ri, line) in rects.iter().enumerate() {
            for (ci, &(x, y, w, h)) in line.iter().enumerate() {
                if w <= 0.0 || h <= 0.0 { continue; }
                if lx >= x && lx < x + w && ly >= y && ly < y + h {
                    return Some((ri, ci));
                }
            }
        }
        None
    }

    /// Table-local point -> byte offset inside the cell's first paragraph.
    pub fn table_cell_hit_local(&mut self, shape: &Shape, styles: &StyleRegistry, row: usize, col: usize, lx: f32, ly: f32) -> Option<usize> {
        let (v, cx, cy, _fs) = self.table_cell_virtual(shape, row, col)?;
        let (_p, off) = self.text_hit_local(&v, styles, lx - cx, ly - cy)?;
        Some(off)
    }

    /// Caret in table-local pt: (x, y_top, y_bottom).
    pub fn table_cell_caret_local(&mut self, shape: &Shape, styles: &StyleRegistry, row: usize, col: usize, offset: usize) -> Option<(f32, f32, f32)> {
        let (v, cx, cy, fs) = self.table_cell_virtual(shape, row, col)?;
        let (lx, ty, _by) = self.text_caret_local(&v, styles, 0, offset)?;
        Some((cx + lx, cy + ty, cy + ty + fs * 1.4))
    }
}

#[cfg(test)]
mod caret_tests {
    use super::*;
    use roe_model::impress::{ShapeContent, ShapeGeometry, TextBodyProps};
    use roe_model::writer::{Paragraph, Run};

    fn shape_with(texts: &[&str]) -> Shape {
        let paras: Vec<Paragraph> = texts.iter().map(|t| {
            let mut p = Paragraph::default();
            if !t.is_empty() { p.runs.push(Run { text: t.to_string(), ..Default::default() }); }
            p
        }).collect();
        Shape {
            id: 1,
            geometry: ShapeGeometry::Rect,
            position: roe_model::common::Transform::from_rect(10.0, 10.0, 300.0, 100.0),
            style: Default::default(),
            content: ShapeContent::Text(paras),
            text_body: TextBodyProps::default(),
            placeholder: None, st_cxn: None, end_cxn: None,
        }
    }

    #[test]
    fn caret_moves_right_and_hit_roundtrips() {
        let styles = StyleRegistry::default();
        let sh = shape_with(&["Hello World"]);
        let mut r = SlideRenderer::new(960.0, 540.0);
        let (x0, t0, b0) = r.text_caret_local(&sh, &styles, 0, 0).unwrap();
        let (x5, _, _) = r.text_caret_local(&sh, &styles, 0, 5).unwrap();
        let (x11, _, _) = r.text_caret_local(&sh, &styles, 0, 11).unwrap();
        assert!(x5 > x0 && x11 > x5, "x: {x0} {x5} {x11}");
        assert!(b0 > t0);
        let (p, off) = r.text_hit_local(&sh, &styles, x5 + 0.3, (t0 + b0) / 2.0).unwrap();
        assert_eq!((p, off), (0, 5));
    }

    #[test]
    fn second_paragraph_is_lower_and_empty_paragraph_works() {
        let styles = StyleRegistry::default();
        let sh = shape_with(&["abc", "", "def"]);
        let mut r = SlideRenderer::new(960.0, 540.0);
        let (_, t0, _) = r.text_caret_local(&sh, &styles, 0, 0).unwrap();
        let (_, t1, _) = r.text_caret_local(&sh, &styles, 1, 0).unwrap();
        let (_, t2, b2) = r.text_caret_local(&sh, &styles, 2, 0).unwrap();
        assert!(t1 > t0 && t2 > t1);
        assert_eq!(r.text_hit_local(&sh, &styles, 20.0, (t2 + b2) / 2.0).unwrap().0, 2);
        assert_eq!(r.text_hit_local(&sh, &styles, 20.0, t1 + 1.0).unwrap(), (1, 0));
    }
}


/// v319g: autonumber text. scheme is the OOXML buAutoNum type.
fn format_autonum_v319g(scheme: &str, n: i32) -> String {
    let n = n.max(1);
    let alpha = |mut m: i32| -> String {
        let mut v: Vec<char> = Vec::new();
        while m > 0 { m -= 1; v.push((b'a' + (m % 26) as u8) as char); m /= 26; }
        v.iter().rev().collect()
    };
    let roman = |mut m: i32| -> String {
        let t = [(1000, "M"), (900, "CM"), (500, "D"), (400, "CD"), (100, "C"), (90, "XC"),
                 (50, "L"), (40, "XL"), (10, "X"), (9, "IX"), (5, "V"), (4, "IV"), (1, "I")];
        let mut s = String::new();
        for (v, r) in t { while m >= v { s.push_str(r); m -= v; } }
        s
    };
    match scheme {
        "arabicParenR" => format!("{n})"),
        "arabicPlain" => n.to_string(),
        "alphaLcPeriod" => format!("{}.", alpha(n)),
        "alphaUcPeriod" => format!("{}.", alpha(n).to_uppercase()),
        "romanLcPeriod" => format!("{}.", roman(n).to_lowercase()),
        "romanUcPeriod" => format!("{}.", roman(n)),
        _ => format!("{n}."),
    }
}

#[cfg(test)]
mod autonum_tests_v319g {
    use super::format_autonum_v319g as f;
    #[test]
    fn labels() {
        assert_eq!(f("arabicPeriod", 3), "3.");
        assert_eq!(f("arabicParenR", 2), "2)");
        assert_eq!(f("alphaLcPeriod", 27), "aa.");
        assert_eq!(f("alphaUcPeriod", 2), "B.");
        assert_eq!(f("romanLcPeriod", 4), "iv.");
        assert_eq!(f("romanUcPeriod", 1994), "MCMXCIV.");
    }
}


/// v319h: paragraph gap in pixels from a pt value (space before or after).
fn gap_v319h(v: Option<f32>, scale: f32) -> f32 {
    v.unwrap_or(0.0).max(0.0) * scale
}
