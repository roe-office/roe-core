//! DOCX-8-v2: 公式（OMML/LaTeX）绘制。
//!
//! 独立于tile.rs的新模块（而不是直接写进tile.rs），因为这是全新代码，
//! 从一开始就没有跟已有渲染逻辑纠缠在一起的必要——tile.rs只需要在两处
//! （underline/strikethrough判断之后附近）查到is_formula的run，拿到
//! Formula数据后调用本模块的draw_formula一行代码，具体绘制细节完全
//! 封装在这里。
//!
//! 坐标系约定（核实自RaTeX ratex-layout::to_display_list源码）：
//! DisplayList/DisplayItem的y坐标是"从公式框顶部往下量的正值"——
//! y=0是公式框顶部，y=height是基线，y=height+depth是公式框底部。
//! 这跟正文渲染的"顶部为0、向下为正"的坐标系是同一种约定，不需要做
//! y轴翻转。
//!
//! Formula.display_list存的坐标已经是pt单位（不是RaTeX原生的em），
//! 见roe-formula::latex_to_display_list的实现和注释。这里只需要再
//! 乘一次pt->像素的scale，不需要关心公式当初用的是几号字。
//!
//! Parley迁移（v40）：GlyphPath分支不再自建cosmic-text Buffer+手工ascent
//! 近似换算，而是直接复用tile.rs::render_glyph_run（正文渲染同一条路径：
//! Parley排版 -> skrifa矢量轮廓 -> tiny-skia路径填充）。单字符也走
//! ranged_builder构建一个Layout<RunBrush>，取其唯一的GlyphRun调用
//! render_glyph_run，origin_y直接传公式基线的像素位置（GlyphRun内部用
//! glyph_run.baseline()作为run_y偏移，不需要再手动估算ascent）。

use tiny_skia::{Paint, PathBuilder, PixmapMut, Rect, Stroke, Transform};
use ratex_types::{DisplayItem, PathCommand};
use parley::{PositionedLayoutItem, StyleProperty};
use crate::tile::{render_glyph_run, RunBrush};

/// 把ratex_types::Color（f32, 0.0~1.0）转成tiny_skia::Color（u8, 0~255）。
fn to_tiny_skia_color(c: ratex_types::Color) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(
        (c.r.clamp(0.0, 1.0) * 255.0) as u8,
        (c.g.clamp(0.0, 1.0) * 255.0) as u8,
        (c.b.clamp(0.0, 1.0) * 255.0) as u8,
        (c.a.clamp(0.0, 1.0) * 255.0) as u8,
    )
}

/// 把ratex_types::Color转成RunBrush用的[u8;4]数组。
fn to_brush_color(c: ratex_types::Color) -> [u8; 4] {
    [
        (c.r.clamp(0.0, 1.0) * 255.0) as u8,
        (c.g.clamp(0.0, 1.0) * 255.0) as u8,
        (c.b.clamp(0.0, 1.0) * 255.0) as u8,
        (c.a.clamp(0.0, 1.0) * 255.0) as u8,
    ]
}

/// 绘制一个公式的DisplayList到pixmap上。
///
/// 参数：
/// - `baseline_x_px`：公式左边界（DisplayList x=0对应位置）的像素x坐标，
///   调用方直接传这一行underline逻辑里已经算好的`lx0`（run起始x）即可。
/// - `baseline_y_px`：公式基线（DisplayList y=height对应位置）的像素y
///   坐标，调用方传`draw_y + glyph_run.baseline()`（跟underline用的是
///   同一个基准，不需要额外计算）。
/// - `formula_height_pt`：Formula.height_pt，用来把baseline_y_px换算
///   回DisplayList的y=0原点（公式框顶部）。
/// - `scale`：pt -> 像素缩放系数（DisplayList坐标已经是pt，只需这一次
///   缩放）。
///
/// 只处理Line/Rect/Path/GlyphPath四种指令。
pub(crate) fn draw_formula(
    pixmap: &mut PixmapMut,
    font_context: &mut parley::FontContext,
    layout_context: &mut parley::LayoutContext<RunBrush>,
    display_list: &ratex_types::DisplayList,
    baseline_x_px: f32,
    baseline_y_px: f32,
    formula_height_pt: f32,
    scale: f32,
) {
    // DisplayList的y=0（公式框顶部）在像素坐标系里的位置。
    let top_y_px = baseline_y_px - formula_height_pt * scale;

    for item in &display_list.items {
        match item {
            DisplayItem::Line { x, y, width, thickness, color, dashed: _ } => {
                // dashed（\hdashline虚线）本次暂不支持虚线样式，按实线画，
                // 不是v2文档验证顺序里的优先项，先保证线条位置/粗细正确。
                let x0 = baseline_x_px + (*x as f32) * scale;
                let x1 = x0 + (*width as f32) * scale;
                let y_px = top_y_px + (*y as f32) * scale;
                let thickness_px = (*thickness as f32) * scale;
                let c = to_tiny_skia_color(*color);
                draw_h_line(pixmap, x0, x1, y_px, thickness_px, c);
            }
            DisplayItem::Rect { x, y, width, height, color } => {
                let x0 = baseline_x_px + (*x as f32) * scale;
                let y0 = top_y_px + (*y as f32) * scale;
                let w = (*width as f32) * scale;
                let h = (*height as f32) * scale;
                if let Some(rect) = Rect::from_xywh(x0, y0, w, h) {
                    let mut paint = Paint::default();
                    paint.set_color(to_tiny_skia_color(*color));
                    paint.blend_mode = tiny_skia::BlendMode::SourceOver;
                    pixmap.fill_rect(rect, &paint, Transform::identity(), None);
                }
            }
            DisplayItem::Path { x, y, commands, fill, color } => {
                let origin_x = baseline_x_px + (*x as f32) * scale;
                let origin_y = top_y_px + (*y as f32) * scale;
                let mut pb = PathBuilder::new();
                for cmd in commands {
                    match cmd {
                        PathCommand::MoveTo { x, y } => {
                            pb.move_to(origin_x + (*x as f32) * scale, origin_y + (*y as f32) * scale);
                        }
                        PathCommand::LineTo { x, y } => {
                            pb.line_to(origin_x + (*x as f32) * scale, origin_y + (*y as f32) * scale);
                        }
                        PathCommand::CubicTo { x1, y1, x2, y2, x, y } => {
                            pb.cubic_to(
                                origin_x + (*x1 as f32) * scale, origin_y + (*y1 as f32) * scale,
                                origin_x + (*x2 as f32) * scale, origin_y + (*y2 as f32) * scale,
                                origin_x + (*x as f32) * scale,  origin_y + (*y as f32) * scale,
                            );
                        }
                        PathCommand::QuadTo { x1, y1, x, y } => {
                            pb.quad_to(
                                origin_x + (*x1 as f32) * scale, origin_y + (*y1 as f32) * scale,
                                origin_x + (*x as f32) * scale,  origin_y + (*y as f32) * scale,
                            );
                        }
                        PathCommand::Close => {
                            pb.close();
                        }
                    }
                }
                if let Some(path) = pb.finish() {
                    let mut paint = Paint::default();
                    paint.set_color(to_tiny_skia_color(*color));
                    paint.blend_mode = tiny_skia::BlendMode::SourceOver;
                    paint.anti_alias = true;
                    if *fill {
                        pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, Transform::identity(), None);
                    } else {
                        let mut stroke = Stroke::default();
                        stroke.width = 1.0; // RaTeX的Path指令目前未提供描边线宽字段，暂用1.0px兜底
                        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
                    }
                }
            }
            DisplayItem::GlyphPath { x, y, scale: glyph_scale, font, char_code, color } => {
                // Parley迁移（v40）：char_code是标准Unicode码点（核实自
                // ratex-layout/src/engine.rs的`let char_code = ch as u32`），
                // 直接转回字符，走跟正文完全相同的 Parley shape+skrifa画
                // 轮廓路径（render_glyph_run），不再自建cosmic-text Buffer。
                //
                // font字符串（如"Main-Regular"/"Size2-Regular"）与
                // FontRegistry::load_katex_fonts()注册进font_context.collection
                // 的19个KaTeX字体族名已核实完全一致（v39已用真实数据验证
                // register_fonts返回的族名与文件名推导字符串逐一对应），
                // 直接用parley::FontFamily::named(font)选中对应字体。
                let Some(ch) = char::from_u32(*char_code) else { continue };
                let mut buf = [0u8; 4];
                let s = ch.encode_utf8(&mut buf);

                // 已用真实样本核实（math-placeholders.docx的∑符号）：
                // GlyphPath.scale不是相对系数，而是直接就是"这个字形应画
                // 多大"的字号，单位pt，量级跟公式整体width_pt/height_pt
                // 一致。正确换算：直接把scale(pt)乘上pt->px的scale系数。
                let font_size_px = (*glyph_scale as f32) * scale;

                let brush = RunBrush { color: to_brush_color(*color), run_idx: None };
                let full_text: String = s.to_string();
                let mut builder = layout_context.ranged_builder(font_context, &full_text, 1.0, true);
                builder.push_default(StyleProperty::Brush(brush));
                builder.push_default(StyleProperty::FontSize(font_size_px));
                builder.push_default(StyleProperty::FontFamily(parley::FontFamily::named(font)));
                let mut layout: parley::Layout<RunBrush> = builder.build(&full_text);
                // 单字符、不换行：给一个足够宽的约束宽度即可，不依赖
                // 自动换行（跟原cosmic-text版本set_size给4倍宽度同理）。
                layout.break_all_lines(Some(font_size_px * 4.0));

                // 已用真实样本核实：GlyphPath.y的值精确等于display_list
                // 的height（本例y=15.015000..., height_pt=15.015053，
                // 浮点误差范围内相等），证实B0调研纪要"y=height是基线"
                // 的判断准确。draw_formula的baseline_y_px参数定义就是
                // "公式基线在像素坐标系里的位置"，所以GlyphPath自己的y值
                // 应该直接等价于baseline_y_px（不需要再额外做top_y_px换算），
                // x仍需要按公式框左边界(baseline_x_px)加上x*scale算左边界
                // 像素位置。
                //
                // render_glyph_run内部用glyph_run.baseline()作为run_y偏移
                // 叠加到origin_y上，所以这里直接把origin_y设为该字形的
                // 基线像素位置即可，不需要再手动估算/减去ascent——这是相
                // 比旧版cosmic-text手工换算（real_ascent_px等）更准确的
                // 做法，因为用的是跟正文渲染完全同一条Parley+skrifa路径，
                // 基线定义天然一致。
                let px = baseline_x_px + (*x as f32) * scale;
                let py_baseline = baseline_y_px - formula_height_pt * scale + (*y as f32) * scale;
                for line in layout.lines() {
                        let line: parley::Line<'_, crate::tile::RunBrush> = line;
                    for item in line.items() {
                        let PositionedLayoutItem::GlyphRun(glyph_run) = item else { continue };
                        let mut pen = crate::tile::TinySkiaPen::new(pixmap);
                        // [FIX] render_glyph_run 内部会对传入的 origin_y 再叠加一次
                        // glyph_run.baseline()（该函数原为正文整行渲染设计：正文调用时
                        // origin_y 传的是行顶部坐标，函数内部加 baseline() 换算成基线，
                        // 这在正文场景下是对的）。但这里 py_baseline 已经是我们按公式
                        // DisplayList 坐标手工算好的、该字符应处的最终绝对基线像素位置，
                        // 不应再被二次叠加，否则造成双重偏移——且偏移量因这个临时单字符
                        // layout 自己的 baseline() 值而异（取决于该字符字号），导致公式
                        // 内不同大小的字符（主字符/上下标）之间出现相对错位，而不是整体
                        // 统一偏移。这里预先减掉 glyph_run.baseline()，让内部再加回后
                        // 刚好等于 py_baseline，抵消掉这次意外的二次叠加。
                        let origin_y = py_baseline - glyph_run.baseline();
                        render_glyph_run(&glyph_run, &mut pen, px, origin_y);
                    }
                }
            }
        }
    }
}

/// 复用tile.rs里WriterTileRenderer::draw_h_line的等价实现。
/// 不直接调用WriterTileRenderer::draw_h_line是因为本函数是自由函数
/// （不持有&mut self），而draw_h_line/draw_v_line虽然已经改成
/// pub(crate)，但仍是WriterTileRenderer的关联函数，调用需要写
/// `crate::tile::WriterTileRenderer::draw_h_line(...)`这种长路径；
/// 这里的实现直接复制一份（两个函数体完全一致，可核对tile.rs同名
/// 函数确认未漂移），避免每次调用都写长路径，可读性更好。
fn draw_h_line(pixmap: &mut PixmapMut, x0: f32, x1: f32, y: f32, thickness: f32, color: tiny_skia::Color) {
    if x1 <= x0 { return; }
    let rect = Rect::from_xywh(x0, y - thickness / 2.0, x1 - x0, thickness);
    if let Some(rect) = rect {
        let mut paint = Paint::default();
        paint.set_color(color);
        paint.blend_mode = tiny_skia::BlendMode::SourceOver;
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    }
}


/// v225：公式预览渲染（侧边栏用）。把 Formula 画到一块新建的 RGBA 画布上，
/// 返回 (宽px, 高px, 像素)。font_context 必须是加载了 KaTeX 字体的上下文
/// （FontRegistry::build_font_context_with_katex），否则公式会静默变形。
/// bg 为背景色 RGBA；传不透明色时像素即最终颜色（tiny-skia 内部是预乘 alpha，
/// 透明背景下半透明像素是预乘值）。尺寸为 0 或超过 4096 返回 None。
pub fn render_formula_preview_rgba(
    font_context: &mut parley::FontContext,
    formula: &roe_model::writer::Formula,
    scale: f32,
    padding_pt: f32,
    bg: [u8; 4],
) -> Option<(u32, u32, Vec<u8>)> {
    let w_f = (formula.width_pt + 2.0 * padding_pt) * scale;
    let h_f = (formula.height_pt + formula.depth_pt + 2.0 * padding_pt) * scale;
    if !(w_f.is_finite() && h_f.is_finite()) {
        return None;
    }
    let w = w_f.ceil() as u32;
    let h = h_f.ceil() as u32;
    if w == 0 || h == 0 || w > 4096 || h > 4096 {
        return None;
    }
    let mut pixmap = tiny_skia::Pixmap::new(w, h)?;
    pixmap.fill(tiny_skia::Color::from_rgba8(bg[0], bg[1], bg[2], bg[3]));
    let mut layout_context: parley::LayoutContext<RunBrush> = parley::LayoutContext::new();
    {
        let mut pm = pixmap.as_mut();
        draw_formula(
            &mut pm,
            font_context,
            &mut layout_context,
            &formula.display_list,
            padding_pt * scale,
            (padding_pt + formula.height_pt) * scale,
            formula.height_pt,
            scale,
        );
    }
    Some((w, h, pixmap.data().to_vec()))
}
