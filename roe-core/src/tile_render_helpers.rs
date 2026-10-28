//! Tile 渲染辅助函数：文本转换、颜色解析、条件格式匹配、图表渲染

use tiny_skia::{Paint, PathBuilder, Transform};
use crate::tile_geometry::{col_width_at, CalcRenderConfig};
use crate::formula_hook::{eval_as_bool, FormulaEvaluator};
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

/// 单元格文字样式(v208):在 bold 之外承载 italic / underline。
/// draw_cell_text 保持原签名并委托 draw_cell_text_styled,其余次要调用点零改动。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellTextStyle {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// Parley 排版的画刷类型：ROE 只需要纯色文字，用 RGBA8 表示。
/// Brush trait 要求 Clone+PartialEq+Default+Debug，派生即可满足。
#[derive(Clone, Copy, Debug, PartialEq, Default)]
struct ColorBrush {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

/// 把 skrifa 的矢量轮廓画到 tiny_skia PixmapMut 上的笔。
/// 照抄 Parley 官方 examples/tiny_skia_render/src/main.rs 里的 TinySkiaPen
/// 实现，仅将裁剪从"无裁剪"改为支持可选的 tiny_skia::Mask（ROE 单元格
/// 文字需要水平裁剪，官方 example 没有这个需求所以原版没有 mask 参数）。
struct TinySkiaPen<'a, 'b> {
    pixmap: &'b mut tiny_skia::PixmapMut<'a>,
    x: f32,
    y: f32,
    paint: tiny_skia::Paint<'static>,
    open_path: tiny_skia::PathBuilder,
    clip_mask: Option<tiny_skia::Mask>,
    // WROOT-XLSX-SHAPE-ROT-01: 浮动形状旋转（xdr:sp rot）支持。默认
    // identity，不影响原有单元格文字渲染调用点（draw_cell_text 等
    // 两处从未调用 set_transform，行为与改动前完全一致）。
    transform: tiny_skia::Transform,
}

impl<'a, 'b> TinySkiaPen<'a, 'b> {
    fn new(pixmap: &'b mut tiny_skia::PixmapMut<'a>) -> Self {
        TinySkiaPen {
            pixmap,
            x: 0.0,
            y: 0.0,
            paint: tiny_skia::Paint::default(),
            open_path: tiny_skia::PathBuilder::new(),
            clip_mask: None,
            transform: tiny_skia::Transform::identity(),
        }
    }

    /// WROOT-XLSX-SHAPE-ROT-01: 设置本次 fill_path/fill_rect 使用的
    /// 变换矩阵（用于浮动形状 rot 旋转文字）。不调用则保持 identity。
    fn set_transform(&mut self, transform: tiny_skia::Transform) {
        self.transform = transform;
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
            self.pixmap.fill_rect(
                rect, &self.paint, self.transform,
                self.clip_mask.as_ref(),
            );
        }
    }

    fn draw_glyph(
        &mut self,
        glyph: &skrifa::outline::OutlineGlyph<'_>,
        size: f32,
        normalized_coords: &[NormalizedCoord],
    ) {
        let location_ref = LocationRef::new(normalized_coords);
        let settings = DrawSettings::unhinted(Size::new(size), location_ref);
        if glyph.draw(settings, self).is_err() {
            self.open_path = tiny_skia::PathBuilder::new();
            return;
        }

        let builder = core::mem::replace(&mut self.open_path, tiny_skia::PathBuilder::new());
        if let Some(path) = builder.finish() {
            self.pixmap.fill_path(
                &path, &self.paint, tiny_skia::FillRule::Winding,
                self.transform, self.clip_mask.as_ref(),
            );
        }
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

/// 渲染一个 GlyphRun 到指定原点（origin_x/origin_y 为该行文字左上角的
/// 像素坐标），套用 Parley 官方 example 的 render_glyph_run 模式。
fn render_glyph_run(
    glyph_run: &GlyphRun<'_, [u8; 4]>,
    pen: &mut TinySkiaPen<'_, '_>,
    origin_x: f32,
    origin_y: f32,
    brush: [u8; 4],
) {
    let mut run_x = glyph_run.offset();
    let run_y = glyph_run.baseline();
    let run = glyph_run.run();
    let font = run.font();
    let font_size = run.font_size();

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

    for glyph in glyph_run.glyphs() {
        let glyph_x = origin_x + run_x + glyph.x;
        let glyph_y = origin_y + run_y + glyph.y;
        run_x += glyph.advance;

        let glyph_id = GlyphId::from(glyph.id);
        if let Some(glyph_outline) = outlines.get(glyph_id) {
            pen.set_origin(glyph_x, glyph_y);
            pen.set_color_rgba8(brush[0], brush[1], brush[2], brush[3]);
            pen.draw_glyph(&glyph_outline, font_size, &normalized_coords);
        }
    }
}

/// 判断 num_format 字符串是否为日期/时间格式
/// CellValue → 显示用文本（带可选数字格式）
/// ROOT-041: 新增date1904参数（workbook级别1904日期系统标记），透传给
/// numfmt::render_number用于日期类型格式的序列号换算。非日期格式(纯数值/
/// 文本)不受影响。调用方应传入所在sheet的date1904字段。
pub fn cell_value_to_text_fmt(val: &roe_model::calc::CellValue, fmt: Option<&str>, date1904: bool) -> String {
    use roe_model::calc::CellValue;
    match val {
        CellValue::Number(n) => {
            if let Some(f) = fmt {
                let parsed = crate::numfmt::parse_format(f);
                return crate::numfmt::render_number(&parsed, *n, date1904).text;
            }
            if n.fract() == 0.0 && n.abs() < 1e15 {
                format!("{}", *n as i64)
            } else {
                format!("{}", n)
            }
        }
        CellValue::Text(s) => {
            if let Some(f) = fmt {
                let parsed = crate::numfmt::parse_format(f);
                if parsed.sections.len() >= 4 {
                    return crate::numfmt::render_text(&parsed, s);
                }
            }
            cell_value_to_text(val)
        }
        // 公式格:cached 已重算时按数字格式渲染缓存值;仍为 Empty 时退化显示公式原文(v104)
        CellValue::Formula { cached, .. } => match cached.as_ref() {
            CellValue::Empty => cell_value_to_text(val),
            other => cell_value_to_text_fmt(other, fmt, date1904),
        },
        _ => cell_value_to_text(val),
    }
}

/// CellValue → 显示用文本
pub fn cell_value_to_text(val: &roe_model::calc::CellValue) -> String {
    use roe_model::calc::{CellValue, CellError};
    match val {
        CellValue::Empty       => String::new(),
        CellValue::Number(n)   => {
            // 整数不带小数点；小数最多保留2位，去掉尾零
            if n.fract() == 0.0 && n.abs() < 1e15 {
                format!("{}", *n as i64)
            } else {
                // round to 2 decimal places, strip trailing zeros
                let rounded = (n * 100.0).round() / 100.0;
                let s = format!("{:.2}", rounded);
                let s = s.trim_end_matches('0');
                let s = s.trim_end_matches('.');
                s.to_string()
            }
        }
        CellValue::Text(s)     => s.clone(),
        // ROOT-078: 显示用纯文本场景（复制/搜索/条件格式字符串比较等）
        // 不关心run级格式，拼接全部run文本即可，语义与Text分支一致。
        CellValue::RichText(runs) => runs.iter().map(|r| r.text.as_str()).collect(),
        CellValue::Formula { source, cached } => {
            // v104：cached 由 roe-calc recalc_and_apply 写回后就是真实计算
            // 结果；还没被重算过（cached 仍是 Empty，比如刚从文件解析出来、
            // 尚未触发过一次 roe_execute_script）时，退化显示公式原文，
            // 让用户至少能看到内容而不是空白。这里是 cell_value_to_text
            // （无 fmt 参数版本），递归调用自身即可，不涉及数字格式化。
            match cached.as_ref() {
                CellValue::Empty => source.clone(),
                other => cell_value_to_text(other),
            }
        }
        CellValue::Bool(b)     => if *b { "TRUE".to_string() } else { "FALSE".to_string() },
        CellValue::Error(e)    => match e {
            CellError::Div0         => "#DIV/0!".to_string(),
            CellError::Ref         => "#REF!".to_string(),
            CellError::Name        => "#NAME?".to_string(),
            CellError::Value       => "#VALUE!".to_string(),
            CellError::Na          => "#N/A".to_string(),
            CellError::Num         => "#NUM!".to_string(),
            CellError::Null        => "#NULL!".to_string(),
            CellError::CircularRef => "#CIRCULAR!".to_string(),
        },
    }
}

/// 计算 col_idx 这一格的可用渲染宽度（真实列宽版本，供 paint_tile 使用）。
/// 规则（贴近 Excel 行为）：从 col_idx 自身宽度开始，向右探测连续的空单元格
/// （同一行 row_idx 在该列没有值），每多一个空列就把它的宽度累加进可用宽度；
/// 遇到非空单元格、或到达 col_limit（tile/viewport 边界）则停止。
/// 注意：这里只看“同一行”是否为空——溢出判断必须按行，不能按整列是否全空。
pub fn compute_available_width(
    sheet: &roe_model::calc::Sheet,
    col_idx: u32,
    col_limit: u32,
    cfg: &CalcRenderConfig,
) -> f32 {
    // 由于本函数不知道具体 row_idx（调用方在内层循环已知），这里改为
    // 由调用方传入更精确的版本；保留这个函数签名是为了兼容旧调用点。
    // 实际精确判断见 compute_available_width_for_row。
    col_width_at(sheet, col_idx, cfg).min(
        (col_limit.saturating_sub(col_idx)) as f32 * cfg.default_col_width,
    )
}

/// 精确版：按行判断右侧单元格是否有内容，决定文字可溢出的宽度。
/// 右侧单元格有内容→截断到本列宽；右侧为空→累加宽度直到非空或越界。
pub fn compute_available_width_for_row(
    sheet: &roe_model::calc::Sheet,
    col_idx: u32,
    row_idx: u32,
    col_limit: u32,
    cfg: &CalcRenderConfig,
) -> f32 {
    let mut width = col_width_at(sheet, col_idx, cfg);
    let mut c = col_idx + 1;
    while c < col_limit {
        let has_content = sheet.cells.cols.get(&c).is_some_and(|sc| {
            sc.blocks.iter().any(|b| {
                let offset = row_idx.saturating_sub(b.start_row);
                if row_idx >= b.start_row && (offset as usize) < b.values.len() {
                    !matches!(b.values[offset as usize], roe_model::calc::CellValue::Empty)
                } else {
                    false
                }
            })
        });
        if has_content { break; }
        width += col_width_at(sheet, c, cfg);
        c += 1;
    }
    width
}

/// render_sheet_preview 专用：可用宽度探测（向右找空单元格）。
/// ROOT-003/005修复：此前函数名/参数名带"uniform"（等宽），起始宽度和
/// 每一步累加宽度都用调用方传入的固定 col_width（实际传的是
/// cfg.default_col_width，即sheet未设置col_widths时的兜底默认值），
/// 完全没有读取该sheet每列各自的customWidth（sheet.col_widths）。
/// 对于所有列都没有自定义宽度、真正等宽的sheet，这个近似恰好蒙对；
/// 但一旦sheet任何一列设置了customWidth（本函数触发场景的
/// pivottable_tabular_mode__sheet2_users.xlsx 五列全部有customWidth，
/// A列22.57字符≈123pt，远大于default_col_width的48pt），当前列自己
/// 的可用宽度就会被硬编为远小于实际列宽的默认值，导致文字被严重
/// 提前截断（不是相邻列有数据"该"截断的情况，是本该有约123pt可用
/// 宽度、却被当成48pt来算）。
/// 现改为：起始宽度用 col_idx 自己的真实列宽（col_width_at），向右
/// 探测时每一步累加的宽度也用该列各自的真实列宽，不再假设所有列
/// 等宽。col_width 参数保留只作为"该sheet未提供任何列宽数据"时
/// col_width_at 内部的最终兜底（cfg.default_col_width），语义不变。
pub fn compute_available_width_uniform(
    sheet: &roe_model::calc::Sheet,
    col_idx: u32,
    max_cols: u32,
    cfg: &CalcRenderConfig,
) -> f32 {
    let mut width = col_width_at(sheet, col_idx, cfg);
    let mut c = col_idx + 1;
    while c < max_cols {
        let occupied = sheet.cells.cols.get(&c).is_some_and(|sc| {
            sc.blocks.iter().any(|b| !b.values.is_empty())
        });
        // 简化版：只要该列存在任意数据就视为“非空”，不区分具体行。
        // render_sheet_preview 是整表静态预览场景，用这个近似足够；
        // 精确到行的判断在 paint_tile 路径的 compute_available_width_for_row 里。
        if occupied { break; }
        width += col_width_at(sheet, c, cfg);
        c += 1;
    }
    width
}

/// 单元格文字渲染（Parley 排版 + skrifa 矢量轮廓 + tiny_skia 路径填充）。
/// 单行、水平裁剪、垂直居中于 cell_h。参数语义与旧 cosmic-text 版本一致
/// （见 .bak_before_temp_freeze）：
/// - cell_x/cell_y/avail_w/cell_h：文档坐标系下的单元格区域
/// - tx_off_x/tx_off_y：视口滚动偏移
/// - scale：文档坐标 → 像素坐标缩放系数（用于坐标/内边距的换算，
///   ROOT-050修复后此值可能含PT_TO_PX，专供坐标定位使用）
/// - font_scale（ROOT-050后续修复新增）：字号缩放系数，只应包含
///   zoom缩放（如HiDPI的2.0），不应包含PT_TO_PX。这是因为Parley的
///   ranged_builder本身会把font_size_pt按这个系数缩放成最终像素字号，
///   font_size_pt(12.0)是设计字号常量，不是"文档坐标"，混入PT_TO_PX
///   会让字号额外放大约1.333倍。scale和font_scale刻意分离成两个参数，
///   避免坐标换算需求（ROOT-050 Bug A）和字号缩放需求被同一个数值
///   误绑定——这正是ROOT-050第一轮修复时踩过的坑：把scale统一改成
///   scale*PT_TO_PX后，坐标修好了，但字号被同步放大，在前台交互
///   渲染（render_viewport_into路径）中表现为文字明显偏大、溢出单元格。
pub fn draw_cell_text<'a, 'b>(
    font_context: &mut parley::FontContext,
    layout_context: &mut parley::LayoutContext,
    pixmap: &'b mut tiny_skia::PixmapMut<'a>,
    text: &str,
    cell_x: f32,
    cell_y: f32,
    avail_w: f32,
    cell_h: f32,
    tx_off_x: f32,
    tx_off_y: f32,
    ui_offset_x_px: f32,
    ui_offset_y_px: f32,
    scale: f32,
    font_scale: f32,
    text_color: [u8; 3],
    bold: bool,
    wrap_text: bool,
    font_size_pt: f32,
    font_family: &str,
    align_h: &str,
    align_v: &str,
    cell_own_width: f32,
) {
    draw_cell_text_styled(
        font_context, layout_context, pixmap,
        text, cell_x, cell_y, avail_w, cell_h,
        tx_off_x, tx_off_y, ui_offset_x_px, ui_offset_y_px,
        scale, font_scale, text_color,
        CellTextStyle { bold, ..Default::default() },
        wrap_text, font_size_pt, font_family, align_h, align_v, cell_own_width,
    );
}

/// draw_cell_text 的样式化版本:支持 bold / italic / underline,参数含义同 draw_cell_text。
pub fn draw_cell_text_styled<'a, 'b>(
    font_context: &mut parley::FontContext,
    layout_context: &mut parley::LayoutContext,
    pixmap: &'b mut tiny_skia::PixmapMut<'a>,
    text: &str,
    cell_x: f32,
    cell_y: f32,
    avail_w: f32,
    cell_h: f32,
    tx_off_x: f32,
    tx_off_y: f32,
    ui_offset_x_px: f32,
    ui_offset_y_px: f32,
    scale: f32,
    font_scale: f32,
    text_color: [u8; 3],
    style: CellTextStyle,
    wrap_text: bool,
    font_size_pt: f32,
    font_family: &str,
    align_h: &str,
    align_v: &str,
    cell_own_width: f32,
) {
    if text.is_empty() {
        return;
    }
    // ROOT-076修复：实测sharedStrings.xml里的换行符是\r\n（非纯\n），
    // 此前只处理\n导致\r残留、渲染异常（\r被字体当控制符处理，视觉上表现
    // 为文字重叠/截断）。这里统一先把\r\n和单独的\r都规范化成\n，再按
    // wrap_text决定是过滤还是保留交给Parley断行。
    let normalized_text;
    let text: &str = if text.contains('\r') {
        normalized_text = text.replace("\r\n", "\n").replace('\r', "\n");
        &normalized_text
    } else {
        text
    };
    // wrap_text=false时过滤掉\n，避免手动换行符在未开启自动换行的单元格里
    // 意外触发Parley断行（实测对照WPS：非wrapText单元格即使内容含\n，也是
    // 整体连续显示，换行符本身消失，不产生空格也不产生断行）。
    let owned_text;
    let text: &str = if !wrap_text && text.contains('\n') {
        owned_text = text.replace('\n', "");
        &owned_text
    } else {
        text
    };

    let pad = 2.0_f32 * scale;
    // font_size 用 font_scale（纯zoom，不含PT_TO_PX）而不是 scale（可能含
    // PT_TO_PX），避免字号被坐标换算系数误伤，见函数文档说明。
    // ranged_builder 的缩放参数同样改用 font_scale。
    let font_size = font_size_pt * font_scale;
    // ROOT-076修复：此前误用cell_h（单元格整体高度）作为单行行高下限，
    // 单行场景下cell_h恰好约等于一行高度看不出问题，但多行场景下cell_h
    // 往往是"容纳N行"而预设的整体高度（如本例ht=45是为了装3行），若每行
    // 都按cell_h=45pt计算，3行合计135pt远超cell_h本身，会被纵向裁剪掉
    // 后面的行。单行行高只应该由字号决定，与单元格整体高度无关；cell_h
    // 只用于下方的垂直居中和纵向裁剪边界计算。
    // ROOT-082系数统一修复（v141交接文档第二节"测绘撕裂"）：此前用经验值
    // font_size_pt*1.2，与Layout阶段measure_row_height_wrap/cheap的LOK公式
    // （estimate_line_height，1.18系数+STD_ROWHEIGHT_DIFF修正）不一致，
    // 导致Paint阶段实际排版高度超过Layout阶段分配的单元格高度，触发纵向
    // 裁剪吞字。改用同一个函数，确保Layout算的和Paint画的是同一套数字。
    let line_height_pt = crate::row_height_measure::estimate_line_height(font_size_pt);

    let draw_x = (cell_x - tx_off_x) * scale + pad + ui_offset_x_px;
    let clip_right = (cell_x - tx_off_x + avail_w) * scale - pad + ui_offset_x_px;
    let cell_top_y = (cell_y - tx_off_y) * scale + ui_offset_y_px;
    if clip_right <= draw_x {
        return;
    }

    let brush: [u8; 4] = [text_color[0], text_color[1], text_color[2], 255];
    let mut builder = layout_context.ranged_builder(font_context, text, font_scale.max(1.0) * crate::tile_geometry::PT_TO_PX, true);
    builder.push_default(parley::StyleProperty::Brush(brush));
    // ROOT-082字体统一修复：见row_height_measure.rs同名注释，Layout/Paint
    // 两阶段必须用同一字体才能保证测量结果与实际渲染一致。
    builder.push_default(parley::StyleProperty::FontFamily(parley::FontFamily::named(font_family)));
    builder.push_default(LineHeight::Absolute(line_height_pt));
    builder.push_default(StyleProperty::FontSize(font_size_pt));
    if style.bold {
        builder.push_default(StyleProperty::FontWeight(FontWeight::BOLD));
    }
    // 斜体/下划线(v208):斜体走字体变体(Carlito Italic 已安装);
    // 下划线 Parley 只携带装饰信息不绘制,需在下方画字形后手画横条。
    if style.italic {
        builder.push_default(StyleProperty::FontStyle(FontStyle::Italic));
    }
    if style.underline {
        builder.push_default(StyleProperty::Underline(true));
    }
    let mut layout: parley::Layout<[u8; 4]> = builder.build(text);
    // ROOT-076: wrapText单元格按可用宽度自动换行（传Some(width)启用），
    // 非wrapText单元格保持原行为——不限宽度，依赖调用方算好的avail_w做
    // 水平裁剪/溢出到右侧空单元格，不能一刀切对所有单元格都限宽，否则
    // 会把此前"文字溢出到相邻空单元格"的正常表现改成强制截断。
    // 无论哪种模式，文本内的\n都会触发Parley断出多行；此前bug是
    // render_glyph_run对每一行都传同一个固定draw_y，而实测证实
    // GlyphRun::baseline()内部就是line.metrics().baseline（vendor
    // parley源码line.rs:360，构造时直接拷贝），也就是"相对整个layout
    // 顶部的绝对基线坐标"——每行本来就自带正确的行间距，只需要外部
    // 传入"多行文本块的顶部起点"这一个统一origin_y，不能再叠加
    // line.metrics().baseline，否则会重复计算导致行位置整体下移。
    let resolved_align_h = crate::row_height_measure::resolve_horizontal_alignment(align_h);
    if wrap_text {
        // ROOT-076修复：wrap_width_px必须用font_scale（layout内部坐标系的
        // 缩放基准，见上方ranged_builder调用），不能用scale（含PT_TO_PX，
        // 与font_scale相差PT_TO_PX≈1.333倍）。此前误用scale导致换行阈值
        // 系统性偏大，实测tdf123026用例wrapText单元格完全不触发自动换行，
        // 追查到这里才发现两个缩放系数被在同一个宽度计算里混用。
        let wrap_width_px = (avail_w * crate::tile_geometry::PT_TO_PX * font_scale.max(1.0) - pad * 2.0).max(1.0);
        layout.break_all_lines(Some(wrap_width_px));
    } else if resolved_align_h != parley::Alignment::Left {
        // ROOT-082对齐bug修复（第二轮）：Parley文档明确说明——break_all_lines(None)时，
        // align()是相对"最长行的固有宽度"对齐，等价于该行自身宽度，对单行
        // 无换行文本而言"相对自身宽度居中"是no-op，这是center/right/distributed
        // 对齐完全不生效的根因。
        // 第一轮修复曾直接用avail_w作为align参照宽度，但avail_w在非merge非wrap
        // 场景下语义是"允许溢出的可用宽度"（可能横跨多个空列一路累加），拿它做
        // 居中参照会把文字推到远超单元格自身范围的位置（实测distributed对齐+
        // avail_w=2051px这类场景，文字视觉上像是"跑到后面几列"）。改用调用方
        // 传入的cell_own_width（单元格自身宽度，merge时是合并区域宽度），这才是
        // 对齐真正应该参照的宽度，与avail_w分开、不互相影响。
        // 代价与第一轮一致：center/right/distributed对齐的单元格，如果文字本身
        // 比cell_own_width更宽，会被Parley按这个宽度断行，不再溢出到相邻单元格
        // ——这符合Excel/LibreOffice真实行为，不是回归。
        let align_width_px = (cell_own_width * crate::tile_geometry::PT_TO_PX * font_scale.max(1.0) - pad * 2.0).max(1.0);
        layout.break_all_lines(Some(align_width_px));
    } else {
        layout.break_all_lines(None);
    }
    layout.align(resolved_align_h, AlignmentOptions::default());

    // 多行垂直居中：用所有行line_height之和算出整个文字块的总高度，
    // 再居中于cell_h。单行场景下total_text_height约等于原来的font_size
    // 居中效果（line_height_pt=max(cell_h, font_size*1.2)略有出入但视觉
    // 差异可忽略），多行场景下这是本次要修的核心差异点：此前的公式
    // 完全没有随行数变化，3行文字被当成1行居中，导致整体位置算错。
    let total_text_height: f32 = layout.lines().map(|l| l.metrics().line_height).sum();
    // ROOT-082垂直对齐接入：此前永远居中，见
    // row_height_measure::resolve_vertical_alignment注释。
    let cell_h_px = cell_h * scale;
    let draw_y_top = match crate::row_height_measure::resolve_vertical_alignment(align_v) {
        crate::row_height_measure::VerticalAlign::Top => cell_top_y,
        crate::row_height_measure::VerticalAlign::Bottom => cell_top_y + (cell_h_px - total_text_height).max(0.0),
        crate::row_height_measure::VerticalAlign::Middle => cell_top_y + (cell_h_px - total_text_height).max(0.0) / 2.0,
    };

    let pw = pixmap.width();
    let ph = pixmap.height();
    let mut mask = match tiny_skia::Mask::new(pw, ph) {
        Some(m) => m,
        None => return,
    };
    // ROOT-076: 纵向也裁到cell_h范围内（此前只做水平裁剪，纵向用整个画布
    // 高度0..ph，多行文字块一旦总高度超出cell_h就会画到下方相邻单元格的
    // 物理位置上——这是本次会话第一轮修复后新暴露的问题，通过真实渲染图
    // 对比发现）。裁剪矩形顶/底用cell_top_y/cell_top_y+cell_h*font_scale，
    // 与draw_y_top的居中计算共用同一套坐标系。
    let clip_top = cell_top_y.max(0.0);
    let clip_bottom = (cell_top_y + cell_h_px).min(ph as f32);
    let clip_rect = match tiny_skia::Rect::from_ltrb(
        draw_x.max(0.0), clip_top, clip_right.min(pw as f32), clip_bottom.max(clip_top),
    ) {
        Some(r) => r,
        None => return,
    };
    let mut clip_path_builder = tiny_skia::PathBuilder::new();
    clip_path_builder.push_rect(clip_rect);
    let clip_path = match clip_path_builder.finish() {
        Some(p) => p,
        None => return,
    };
    mask.fill_path(&clip_path, tiny_skia::FillRule::Winding, true, tiny_skia::Transform::identity());

    let mut pen = TinySkiaPen::new(pixmap);
    pen.clip_mask = Some(mask);

    for line in layout.lines() {
        for item in line.items() {
            if let PositionedLayoutItem::GlyphRun(glyph_run) = item {
                // origin_y传draw_y_top（多行文本块顶部起点，对所有行一样）；
                // render_glyph_run内部run_y=glyph_run.baseline()已经是
                // 该行相对layout顶部的绝对基线偏移，两者相加即可得到
                // 每行独立的正确纵向位置，不需要（也不能）额外叠加
                // line.metrics().baseline，见上方说明。
                render_glyph_run(&glyph_run, &mut pen, draw_x, draw_y_top, brush);
                // 下划线:与 draw_cell_rich_text 的删除线同一套画法(取该 run 的
                // 字体度量 offset/size,横条跨整个 glyph run),写法参照 roe-writer/tile.rs。
                let run_style = glyph_run.style();
                if let Some(decoration) = &run_style.underline {
                    let run_metrics = glyph_run.run().font_metrics();
                    let offset = decoration.offset.unwrap_or(run_metrics.underline_offset);
                    let size = decoration.size.unwrap_or(run_metrics.underline_size).max(1.0);
                    let uy = draw_y_top + glyph_run.baseline() - offset;
                    let lx0 = draw_x + glyph_run.offset();
                    let lx1 = lx0 + glyph_run.advance();
                    if lx1 > lx0 {
                        if let Some(rect) = tiny_skia::Rect::from_xywh(lx0, uy - size / 2.0, lx1 - lx0, size) {
                            let mut paint = Paint::default();
                            paint.set_color_rgba8(brush[0], brush[1], brush[2], brush[3]);
                            paint.blend_mode = tiny_skia::BlendMode::SourceOver;
                            pen.pixmap.fill_rect(rect, &paint, tiny_skia::Transform::identity(), pen.clip_mask.as_ref());
                        }
                    }
                }
            }
        }
    }
}

/// ROOT-078: cell rich text (multi-run mixed formatting) rendering.
/// Parallel to draw_cell_text, does not change its signature/behavior --
/// most cells (0/1 run) still go through draw_cell_text; only cells whose
/// sharedStrings parse produced CellValue::RichText (>=2 runs) call this.
/// Scope of this implementation: only strikethrough is supported (matches
/// the strike-through.xlsx test case). bold/italic/underline are left for
/// future real test cases.
pub fn draw_cell_rich_text<'a, 'b>(
    font_context: &mut parley::FontContext,
    layout_context: &mut parley::LayoutContext,
    pixmap: &'b mut tiny_skia::PixmapMut<'a>,
    runs: &[roe_model::calc::TextRun],
    cell_x: f32,
    cell_y: f32,
    avail_w: f32,
    cell_h: f32,
    tx_off_x: f32,
    tx_off_y: f32,
    ui_offset_x_px: f32,
    ui_offset_y_px: f32,
    scale: f32,
    font_scale: f32,
    text_color: [u8; 3],
    bold: bool,
    wrap_text: bool,
    font_size_pt: f32,
    font_family: &str,
    align_h: &str,
    align_v: &str,
    cell_own_width: f32,
) {
    let mut full_text = String::new();
    let mut run_ranges: Vec<(std::ops::Range<usize>, &roe_model::calc::TextRun)> = Vec::new();
    for run in runs {
        let start = full_text.len();
        full_text.push_str(&run.text);
        let end = full_text.len();
        run_ranges.push((start..end, run));
    }
    if full_text.is_empty() {
        return;
    }
    // Note: run_ranges byte offsets assume full_text is used as-is (no
    // CR/LF normalization applied here, unlike draw_cell_text). The
    // strike-through.xlsx test case text has no CR/LF, so this is fine
    // for now; a future rich-text + manual-newline combination would need
    // the offset remap done alongside any CR/LF filtering.

    let pad = 2.0_f32 * scale;
    // ROOT-082系数统一修复：与draw_cell_text(403行)同理，改用LOK公式，
    // 保持与Layout阶段measure_row_height_wrap一致。见该处注释。
    let line_height_pt = crate::row_height_measure::estimate_line_height(font_size_pt);

    let draw_x = (cell_x - tx_off_x) * scale + pad + ui_offset_x_px;
    let clip_right = (cell_x - tx_off_x + avail_w) * scale - pad + ui_offset_x_px;
    let cell_top_y = (cell_y - tx_off_y) * scale + ui_offset_y_px;
    if clip_right <= draw_x {
        return;
    }

    let brush: [u8; 4] = [text_color[0], text_color[1], text_color[2], 255];
    let mut builder = layout_context.ranged_builder(font_context, &full_text, font_scale.max(1.0) * crate::tile_geometry::PT_TO_PX, true);
    builder.push_default(parley::StyleProperty::Brush(brush));
    // ROOT-082字体统一修复：见row_height_measure.rs同名注释，Layout/Paint
    // 两阶段必须用同一字体才能保证测量结果与实际渲染一致。
    builder.push_default(parley::StyleProperty::FontFamily(parley::FontFamily::named(font_family)));
    builder.push_default(LineHeight::Absolute(line_height_pt));
    builder.push_default(StyleProperty::FontSize(font_size_pt));
    if bold {
        builder.push_default(StyleProperty::FontWeight(FontWeight::BOLD));
    }
    for (range, run) in &run_ranges {
        if run.strikethrough {
            builder.push(StyleProperty::Strikethrough(true), range.clone());
        }
    }
    let mut layout: parley::Layout<[u8; 4]> = builder.build(&full_text);
    let resolved_align_h = crate::row_height_measure::resolve_horizontal_alignment(align_h);
    if wrap_text {
        let wrap_width_px = (avail_w * crate::tile_geometry::PT_TO_PX * font_scale.max(1.0) - pad * 2.0).max(1.0);
        layout.break_all_lines(Some(wrap_width_px));
    } else if resolved_align_h != parley::Alignment::Left {
        // ROOT-082对齐bug修复（第二轮）：与draw_cell_text同理，见该处注释。
        // 用cell_own_width（单元格自身宽度）而非avail_w（含溢出的可用宽度）
        // 作为align参照，避免居中/分散对齐把文字推到远超单元格自身范围的位置。
        let align_width_px = (cell_own_width * crate::tile_geometry::PT_TO_PX * font_scale.max(1.0) - pad * 2.0).max(1.0);
        layout.break_all_lines(Some(align_width_px));
    } else {
        layout.break_all_lines(None);
    }
    layout.align(resolved_align_h, AlignmentOptions::default());

    let total_text_height: f32 = layout.lines().map(|l| l.metrics().line_height).sum();
    // ROOT-082垂直对齐接入：与draw_cell_text同理，见
    // row_height_measure::resolve_vertical_alignment注释。
    let cell_h_px = cell_h * scale;
    let draw_y_top = match crate::row_height_measure::resolve_vertical_alignment(align_v) {
        crate::row_height_measure::VerticalAlign::Top => cell_top_y,
        crate::row_height_measure::VerticalAlign::Bottom => cell_top_y + (cell_h_px - total_text_height).max(0.0),
        crate::row_height_measure::VerticalAlign::Middle => cell_top_y + (cell_h_px - total_text_height).max(0.0) / 2.0,
    };

    let pw = pixmap.width();
    let ph = pixmap.height();
    let mut mask = match tiny_skia::Mask::new(pw, ph) {
        Some(m) => m,
        None => return,
    };
    let clip_top = cell_top_y.max(0.0);
    let clip_bottom = (cell_top_y + cell_h_px).min(ph as f32);
    let clip_rect = match tiny_skia::Rect::from_ltrb(
        draw_x.max(0.0), clip_top, clip_right.min(pw as f32), clip_bottom.max(clip_top),
    ) {
        Some(r) => r,
        None => return,
    };
    let mut clip_path_builder = tiny_skia::PathBuilder::new();
    clip_path_builder.push_rect(clip_rect);
    let clip_path = match clip_path_builder.finish() {
        Some(p) => p,
        None => return,
    };
    mask.fill_path(&clip_path, tiny_skia::FillRule::Winding, true, tiny_skia::Transform::identity());

    let mut pen = TinySkiaPen::new(pixmap);
    pen.clip_mask = Some(mask);

    // ROOT-078: strikethrough is drawn manually by the renderer (Parley
    // only carries decoration info, it does not draw it -- following the
    // same pattern already proven in roe-writer::tile.rs). After drawing
    // each GlyphRun's glyphs, read glyph_run.style().strikethrough; if
    // present, use the run's own font_metrics() offset/size to compute
    // the line position and draw a horizontal bar spanning the run.
    for line in layout.lines() {
        for item in line.items() {
            if let PositionedLayoutItem::GlyphRun(glyph_run) = item {
                render_glyph_run(&glyph_run, &mut pen, draw_x, draw_y_top, brush);
                let style = glyph_run.style();
                if let Some(decoration) = &style.strikethrough {
                    let run_metrics = glyph_run.run().font_metrics();
                    let offset = decoration.offset.unwrap_or(run_metrics.strikethrough_offset);
                    let size = decoration.size.unwrap_or(run_metrics.strikethrough_size).max(1.0);
                    let sy = draw_y_top + glyph_run.baseline() - offset;
                    let lx0 = draw_x + glyph_run.offset();
                    let lx1 = lx0 + glyph_run.advance();
                    if lx1 > lx0 {
                        if let Some(rect) = tiny_skia::Rect::from_xywh(lx0, sy - size / 2.0, lx1 - lx0, size) {
                            let mut paint = Paint::default();
                            paint.set_color_rgba8(brush[0], brush[1], brush[2], brush[3]);
                            paint.blend_mode = tiny_skia::BlendMode::SourceOver;
                            pen.pixmap.fill_rect(rect, &paint, tiny_skia::Transform::identity(), pen.clip_mask.as_ref());
                        }
                    }
                }
            }
        }
    }
}

/// 在 pixmap 指定位置画一个右下角填充三角形（下拉/筛选指示符通用组件）。
/// rx/by：三角形右下角坐标（文档坐标系，tx 负责转换到 pixmap 局部坐标）。
/// size：三角形直角边长（像素）。color：RGBA8。
pub fn draw_indicator_triangle(
    pixmap: &mut tiny_skia::PixmapMut,
    tx: Transform,
    rx: f32,
    by: f32,
    size: f32,
    color: [u8; 4],
) {
    let mut paint = Paint::default();
    paint.set_color_rgba8(color[0], color[1], color[2], color[3]);
    paint.anti_alias = true;
    let mut pb = PathBuilder::new();
    pb.move_to(rx, by);
    pb.line_to(rx - size, by);
    pb.line_to(rx, by - size);
    pb.close();
    if let Some(path) = pb.finish() {
        pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, tx, None);
    }
}

/// 根据 CellValue 类型返回填充色（临时可视化）
/// Number=蓝色, Text=绿色, Formula=橙色, Bool=紫色, Error=红色
/// 解析 "#RRGGBB" 为 [r,g,b,a]，失败返回 None
pub fn parse_hex_color(s: &str) -> Option<[u8; 4]> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 { return None; }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some([r, g, b, 0xff])
}

/// 判断单元格值是否满足条件格式条件字符串
/// 格式：
///   "gt:N"             大于 N
///   "lt:N"             小于 N
///   "gte:N"            大于等于 N
///   "lte:N"            小于等于 N
///   "eq:N"             等于 N（数字或字符串）
///   "neq:N"            不等于 N
///   "between:A:B"      在 A 和 B 之间（含）
///   "text_contains:X"  文本包含 X
///   "not_empty"        非空
///   "is_empty"         为空
/// 递归取 CellValue 的数值表示：Number 直接返回；Formula 取其 cached 结果
/// 递归下探（cached 正常不会再是 Formula，但递归写法对未来更稳妥）。
pub fn cell_value_as_f64_local(val: &roe_model::calc::CellValue) -> Option<f64> {
    use roe_model::calc::CellValue;
    match val {
        CellValue::Number(n) => Some(*n),
        CellValue::Formula { cached, .. } => cell_value_as_f64_local(cached),
        _ => None,
    }
}

/// 解析 "$B$3" / "B3" 形式的单元格引用 → (row_0based, col_0based)。
/// 与 roe-parser::ooxml::xlsx::parse_cell_ref 逻辑等价，但先剥离 "$" 符号
/// （原 parser 版本没有过滤 "$"，若 "$" 出现在列字母前会导致解析失败）。
/// 独立实现是为了避免 roe-core 反向依赖 roe-parser。
fn parse_cell_ref_local(addr: &str) -> Option<(u32, u32)> {
    let cleaned: String = addr.chars().filter(|c| *c != '$').collect();
    let mut col: u32 = 0;
    let mut row_str = "";
    let mut found_digit = false;
    for (i, ch) in cleaned.char_indices() {
        if ch.is_ascii_alphabetic() && !found_digit {
            col = col.saturating_mul(26).saturating_add(ch.to_ascii_uppercase() as u32 - b'A' as u32 + 1);
        } else if ch.is_ascii_digit() {
            found_digit = true;
            row_str = &cleaned[i..];
            break;
        } else {
            return None;
        }
    }
    if col == 0 || row_str.is_empty() { return None; }
    let row: u32 = row_str.parse().ok()?;
    if row == 0 { return None; }
    Some((row - 1, col - 1))
}

/// 若 raw 是形如 "$B$3" 的单元格引用且 sheet 已提供，返回该单元格的数值
/// 表示（用于 gt/lt/eq 等数值比较）；否则回退为把 raw 自身当字面量数字解析。
fn resolve_num_operand(raw: &str, sheet: Option<&roe_model::calc::Sheet>) -> Option<f64> {
    if let Some(sheet) = sheet {
        if let Some((row, col)) = parse_cell_ref_local(raw) {
            let cell = sheet.cells.get(row, col);
            if let Some(n) = cell_value_as_f64_local(&cell) {
                return Some(n);
            }
        }
    }
    raw.parse::<f64>().ok()
}

/// 若 raw 是形如 "$B$3" 的单元格引用且 sheet 已提供，返回该单元格的文本
/// 表示（用于 begins_with/ends_with/text_contains 等文本比较）；否则回退
/// 为把 raw 自身当字面量文本。
fn resolve_str_operand(raw: &str, sheet: Option<&roe_model::calc::Sheet>) -> String {
    use roe_model::calc::CellValue;
    if let Some(sheet) = sheet {
        if let Some((row, col)) = parse_cell_ref_local(raw) {
            let cell = sheet.cells.get(row, col);
            let resolved = match &cell {
                CellValue::Text(s) => Some(s.clone()),
                CellValue::Number(n) => Some(n.to_string()),
                CellValue::Formula { cached, .. } => match cached.as_ref() {
                    CellValue::Text(s) => Some(s.clone()),
                    CellValue::Number(n) => Some(n.to_string()),
                    _ => None,
                },
                _ => None,
            };
            if let Some(r) = resolved { return r; }
        }
    }
    raw.to_string()
}

pub fn cond_matches(val: &roe_model::calc::CellValue, condition: &str) -> bool {
    cond_matches_ctx(val, condition, None, None, None, None, None)
}

/// cond_matches 的上下文感知版本：condition 里的比较操作数（如
/// "begins_with:$B$3" 中的 "$B$3"）若形如单元格引用且提供了 sheet，
/// 会先查出该单元格实际值再参与比较，而不是把引用文本本身当字面量。
/// A3(`$B$3`)动态单元格引用限制的修复：给 cond_matches 增加 sheet 上下文，
/// 与 ROOT-011（结构化表格引用）性质类似但实现路径不同（此处只是局部
/// 函数签名扩展，不涉及 Table 数据结构）。
pub fn cond_matches_ctx(
    val: &roe_model::calc::CellValue,
    condition: &str,
    sheet: Option<&roe_model::calc::Sheet>,
    // ROOT-078 (eval_hook)：expr 类型条件求值需要 workbook（供
    // eval_formula 内部跨表/命名区域引用）+ evaluator（实际求值实现，
    // 由 roe-api 组装注入，见 formula_hook.rs 模块文档）+ 当前单元格
    // 坐标（expression 规则的公式语义是对"当前被判断单元格"求布尔值，
    // 相对引用需要知道具体是哪个单元格）。三者任一为 None 时，"expr:"
    // 分支退化为不命中（false），不 panic——覆盖了 cond_matches() 这个
    // 无上下文旧调用路径继续可用的情况。
    workbook: Option<&roe_model::calc::Workbook>,
    evaluator: Option<&dyn FormulaEvaluator>,
    cell_ctx: Option<(u32, u32)>,
    // ROOT-078附带发现：expr条件格式规则的公式是相对于sqref范围锚点
    // （范围左上角）写的，求值其余单元格时需要这个坐标才能把公式里
    // 的相对引用平移到"当前单元格"的语义。cond_matches()无上下文旧
    // 调用路径继续传None，等价于不做平移（与偏移前行为一致）。
    anchor_cell: Option<(u32, u32)>,
) -> bool {
    use roe_model::calc::CellValue;
    let parts: Vec<&str> = condition.splitn(3, ':').collect();
    let op = parts[0];
    let num_val = match val {
        CellValue::Number(n) => Some(*n),
        CellValue::Formula { cached, .. } => cell_value_as_f64_local(cached),
        _ => None,
    };
    let str_val = match val {
        CellValue::Text(s) => Some(s.as_str()),
        CellValue::Formula { cached, .. } => match cached.as_ref() {
            CellValue::Text(s) => Some(s.as_str()),
            _ => None,
        },
        _ => None,
    };
    match op {
        // v127: A3(`$B$3`)动态单元格引用修复——比较操作数统一经过
        // resolve_num_operand/resolve_str_operand，若形如单元格引用
        // 且提供了 sheet，会先查出该单元格实际值再参与比较。
        "gt"  => { let n = parts.get(1).and_then(|s| resolve_num_operand(s, sheet)); num_val.zip(n).map(|(v,n)| v > n).unwrap_or(false) }
        "lt"  => { let n = parts.get(1).and_then(|s| resolve_num_operand(s, sheet)); num_val.zip(n).map(|(v,n)| v < n).unwrap_or(false) }
        "gte" => { let n = parts.get(1).and_then(|s| resolve_num_operand(s, sheet)); num_val.zip(n).map(|(v,n)| v >= n).unwrap_or(false) }
        "lte" => { let n = parts.get(1).and_then(|s| resolve_num_operand(s, sheet)); num_val.zip(n).map(|(v,n)| v <= n).unwrap_or(false) }
        "eq"  => {
            if let (Some(v), Some(n)) = (num_val, parts.get(1).and_then(|s| resolve_num_operand(s, sheet))) {
                (v - n).abs() < 1e-10
            } else {
                match parts.get(1) {
                    Some(raw) => { let n = resolve_str_operand(raw, sheet); str_val.map(|v| v == n).unwrap_or(false) }
                    None => false,
                }
            }
        }
        // ROOT-009部分修复(v123)：原来这里写的是"neq"，但解析层
        // build_cf_condition()（xlsx.rs）对notEqual生成的是"ne:{}"，
        // 与xlsx_styles_writer.rs::condition_to_cf_type()的反向映射
        // （"ne" -> notEqual）保持一致——"ne"才是项目里的规范拼写，
        // 之前"neq"是本函数单方面的笔误，导致"不等于"规则永远不命中。
        "ne" => {
            if let (Some(v), Some(n)) = (num_val, parts.get(1).and_then(|s| resolve_num_operand(s, sheet))) {
                (v - n).abs() >= 1e-10
            } else {
                match parts.get(1) {
                    Some(raw) => { let n = resolve_str_operand(raw, sheet); str_val.map(|v| v != n).unwrap_or(false) }
                    None => false,
                }
            }
        }
        "between" => {
            let a = parts.get(1).and_then(|s| resolve_num_operand(s, sheet));
            let b = parts.get(2).and_then(|s| resolve_num_operand(s, sheet));
            if let (Some(v), Some(a), Some(b)) = (num_val, a, b) { v >= a && v <= b } else { false }
        }
        // ROOT-009部分修复(v123)：新增，与build_cf_condition() notBetween
        // 生成的"not_between:{}:{}"对应，原来完全没有这个分支，规则永远不命中。
        "not_between" => {
            let a = parts.get(1).and_then(|s| resolve_num_operand(s, sheet));
            let b = parts.get(2).and_then(|s| resolve_num_operand(s, sheet));
            if let (Some(v), Some(a), Some(b)) = (num_val, a, b) { !(v >= a && v <= b) } else { false }
        }
        "text_contains" => {
            let needle = parts.get(1).map(|s| resolve_str_operand(s, sheet)).unwrap_or_default();
            str_val.map(|v| v.contains(&needle)).unwrap_or(false)
        }
        // ROOT-009部分修复(v123)：新增，对应build_cf_condition()的
        // notContainsText/beginsWith/endsWith，原来完全没有实现。
        "text_not_contains" => {
            let needle = parts.get(1).map(|s| resolve_str_operand(s, sheet)).unwrap_or_default();
            str_val.map(|v| !v.contains(&needle)).unwrap_or(false)
        }
        "begins_with" => {
            let prefix = parts.get(1).map(|s| resolve_str_operand(s, sheet)).unwrap_or_default();
            str_val.map(|v| v.starts_with(&prefix)).unwrap_or(false)
        }
        "ends_with" => {
            let suffix = parts.get(1).map(|s| resolve_str_operand(s, sheet)).unwrap_or_default();
            str_val.map(|v| v.ends_with(&suffix)).unwrap_or(false)
        }
        "not_empty" => !matches!(val, CellValue::Empty),
        "is_empty"  =>  matches!(val, CellValue::Empty),
        // ROOT-078 (eval_hook) 已实现：expression 条件格式规则求值。
        // condition 字符串形如 "expr:A1<>1"（见 xlsx.rs build_cf_condition
        // "expression" 分支），parts[1] 就是完整公式原文（不含前导 '='）。
        // 需要 workbook + evaluator 都提供才能求值；任一缺失（如
        // cond_matches() 无上下文旧调用路径）退化为不命中，不 panic。
        "expr" => {
            match (workbook, evaluator, parts.get(1), sheet) {
                (Some(wb), Some(ev), Some(formula), Some(sh)) => {
                    eval_as_bool(ev, formula, wb, sh, cell_ctx, anchor_cell)
                }
                _ => false,
            }
        }
        // ROOT-009已知限制（本次不实现，需要整列/整区域上下文，
        // 不是单cell判断，超出cond_matches当前签名能力范围）：
        // "duplicate"/"unique"（需要对比同范围内其他单元格值）、
        // "top10"（需要区域内排序）、"above_average"（需要区域内均值）、
        // "time_period"（需要日期解析+时间段语义，当前CellValue无原生
        // 日期类型，date相关值目前是Text，需另行设计）。
        // 这几种解析层已能生成对应condition字符串，但渲染层cond_matches
        // 落到下面_ => false分支，等同于"规则从不命中"（不报错，只是
        // 视觉上不生效），不是panic，属于已知功能缺口而非本次范围内bug。
        _ => false,
    }
}

pub fn cell_fill_color(val: &roe_model::calc::CellValue) -> [u8; 4] {
    use roe_model::calc::CellValue;
    match val {
        CellValue::Number(_)  => [0xbb, 0xd6, 0xfb, 0xcc],
        CellValue::Text(_)    => [0xb7, 0xeb, 0xc8, 0xcc],
        // ROOT-078: 富文本按"文字类"同色处理，这只是调试可视化配色，
        // 不影响正式渲染。
        CellValue::RichText(_) => [0xb7, 0xeb, 0xc8, 0xcc],
        CellValue::Formula { .. } => [0xff, 0xd6, 0x99, 0xcc],
        CellValue::Bool(_)    => [0xd9, 0xb3, 0xff, 0xcc],
        CellValue::Error(_)   => [0xff, 0xb3, 0xb3, 0xff],
        CellValue::Empty      => [0; 4], // 透明，跳过
    }
}

#[cfg(test)]
mod test_b2a {
    use super::*;

    // test_apply_num_format_* 系列已随 apply_num_format 一起移除：
    // 该函数被 numfmt.rs 的 token 化解析+渲染取代（见 numfmt.rs 内部测试，
    // 覆盖 decimal/percent/comma/date 等同等场景，且额外修复了
    // hh:mm:ss 分钟被误判为月份、负号重复等 bug）。

    #[test]
    fn test_cell_value_to_text_fmt_formula_cached() {
        use roe_model::calc::CellValue;
        let f = |c: CellValue| CellValue::Formula { source: "=X()".into(), cached: Box::new(c) };
        assert_eq!(cell_value_to_text_fmt(&f(CellValue::Number(0.000812)), Some("0.0000"), false), "0.0008");
        assert_eq!(cell_value_to_text_fmt(&f(CellValue::Number(8.0)), Some("0.0000"), false), "8.0000");
        assert_eq!(cell_value_to_text_fmt(&f(CellValue::Number(2.5)), None, false), "2.5");
        assert_eq!(cell_value_to_text_fmt(&f(CellValue::Empty), Some("0.0000"), false), "=X()");
    }
    #[test]
    fn test_cell_value_to_text_fmt_no_format() {
        use roe_model::calc::CellValue;
        assert_eq!(cell_value_to_text_fmt(&CellValue::Number(42.0), None, false), "42");
        assert_eq!(cell_value_to_text_fmt(&CellValue::Text("hi".into()), Some("0.00"), false), "hi");
    }

    // ROOT-009部分修复(v123)：验证cond_matches新增/修正的op分支。
    // "ne"/"not_between"字符串与xlsx.rs::build_cf_condition()生成的
    // condition字符串保持一致（此前"neq"拼写错误、"not_between"完全缺失，
    // 导致这两种条件格式规则永远不命中）。
    #[test]
    fn test_cond_matches_ne() {
        use roe_model::calc::CellValue;
        assert!(cond_matches(&CellValue::Number(5.0), "ne:0"));
        assert!(!cond_matches(&CellValue::Number(0.0), "ne:0"));
        assert!(cond_matches(&CellValue::Text("foo".into()), "ne:bar"));
        assert!(!cond_matches(&CellValue::Text("bar".into()), "ne:bar"));
    }

    #[test]
    fn test_cond_matches_not_between() {
        use roe_model::calc::CellValue;
        assert!(cond_matches(&CellValue::Number(15.0), "not_between:1:10"));
        assert!(!cond_matches(&CellValue::Number(5.0), "not_between:1:10"));
    }

    #[test]
    fn test_cond_matches_text_variants() {
        use roe_model::calc::CellValue;
        assert!(cond_matches(&CellValue::Text("hello world".into()), "text_not_contains:xyz"));
        assert!(!cond_matches(&CellValue::Text("hello world".into()), "text_not_contains:world"));
        assert!(cond_matches(&CellValue::Text("hello".into()), "begins_with:he"));
        assert!(!cond_matches(&CellValue::Text("hello".into()), "begins_with:lo"));
        assert!(cond_matches(&CellValue::Text("hello".into()), "ends_with:lo"));
        assert!(!cond_matches(&CellValue::Text("hello".into()), "ends_with:he"));
    }
}

/// 把 ChartModel 渲染为 RGBA8 像素缓冲（含外边框）。
/// 实际实现已下沉到 `crate::chart_render`（Writer/Impress/Calc 共用的唯一实现）。
pub fn render_chart_model_to_rgba(model: &roe_model::impress::ChartModel, width: u32, height: u32) -> Option<Vec<u8>> {
    crate::chart_render::render_chart_to_rgba(model, width, height)
}
/// 列号转字母标题（0→A, 1→B, 25→Z, 26→AA ...）
pub fn col_label(mut col: u32) -> String {
    let mut label = String::new();
    loop {
        label.insert(0, (b'A' + (col % 26) as u8) as char);
        if col < 26 { break; }
        col = col / 26 - 1;
    }
    label
}

/// 在标题栏区域居中绘制文字（行号/列字母）。
/// 参数语义与旧 cosmic-text 版本一致（见 .bak_before_temp_freeze）：
/// px_x/px_y/px_w/px_h 均为像素坐标系（调用方已完成 doc→px 转换）。
pub fn draw_header_text<'a, 'b>(
    font_context: &mut parley::FontContext,
    layout_context: &mut parley::LayoutContext,
    pixmap: &'b mut tiny_skia::PixmapMut<'a>,
    text: &str,
    px_x: f32,
    px_y: f32,
    px_w: f32,
    px_h: f32,
    scale: f32,
) {
    if text.is_empty() {
        return;
    }

    let pad_x = 4.0_f32 * scale;
    let pad_y = 2.0_f32 * scale;
    let font_size = 11.0_f32 * scale;
    let inner_h = (px_h - pad_y * 2.0).max(font_size * 1.2);
    let inner_w = (px_w - pad_x * 2.0).max(1.0);
    // font_size_pt/line_height_pt：喂给 ranged_builder 的必须是未乘 scale
    // 的原始值，scale 参数本身已经是 Parley 内置像素缩放系数，重复相乘
    // 会导致字号按 scale² 放大（同 WROOT v38 roe-writer/tile.rs 双重
    // 缩放 bug 同根因）。inner_h/inner_w/font_size（乘过scale的）仍
    // 保留给下面的像素布局计算（text_w/offset_x 等）使用，不受影响。
    let font_size_pt = 11.0_f32;
    let line_height_pt = (inner_h / scale.max(1.0)).max(font_size_pt * 1.2);

    let brush: [u8; 4] = [80, 80, 80, 255];
    let mut builder = layout_context.ranged_builder(font_context, text, scale.max(1.0), true);
    builder.push_default(parley::StyleProperty::Brush(brush));
    // ROOT-082字体统一修复：见row_height_measure.rs同名注释，Layout/Paint
    // 两阶段必须用同一字体才能保证测量结果与实际渲染一致。
    builder.push_default(parley::StyleProperty::FontFamily(parley::FontFamily::named("Carlito")));
    builder.push_default(LineHeight::Absolute(line_height_pt));
    builder.push_default(StyleProperty::FontSize(font_size_pt));
    let mut layout: parley::Layout<[u8; 4]> = builder.build(text);
    layout.break_all_lines(None);
    layout.align(Alignment::Start, AlignmentOptions::default());

    let text_w = layout.width();
    let offset_x = ((px_w - text_w) * 0.5).max(0.0);
    let draw_x = px_x + offset_x;
    let draw_y = px_y + pad_y;

    let _ = inner_w;

    let mut pen = TinySkiaPen::new(pixmap);
    for line in layout.lines() {
        for item in line.items() {
            if let PositionedLayoutItem::GlyphRun(glyph_run) = item {
                render_glyph_run(&glyph_run, &mut pen, draw_x, draw_y, brush);
            }
        }
    }
}


/// WROOT-XLSX-SHAPE-01: 渲染 xlsx 浮动形状（xdr:sp，含文本框）。
/// 几何构造照抄 roe-writer::tile.rs::render_shape_frame 的 match 分支
/// （PresetPath/Rect/RoundRect/Ellipse/Triangle/Line 六种，Custom 不画），
/// 坐标系换算改用 xlsx viewport 风格（scroll_x_pt/scroll_y_pt + 视口裁剪），
/// 与 draw_cell_text/浮动图片渲染保持同一套参数命名。
///
/// sx/sy：该形状左上角在像素坐标系下的位置（视口裁剪+缩放+data区域偏移后）。
/// 本轮（任务A）只处理 fill（Solid），stroke 和 Gradient/Picture 填充暂不
/// 处理（样本里较少见，且 FloatingShape 目前只有 fill: Option<FillStyle>
/// 一个字段，没有单独的 stroke 字段）。
#[allow(clippy::too_many_arguments)]
pub fn draw_floating_shape(
    font_context: &mut parley::FontContext,
    layout_context: &mut parley::LayoutContext,
    pixmap: &mut tiny_skia::PixmapMut,
    shape: &roe_model::calc::FloatingShape,
    scroll_x_pt: f32,
    scroll_y_pt: f32,
    data_px_x: i32,
    data_px_y: i32,
    vw: u32,
    vh: u32,
    scale: f32,
) {
    // 形状几何单位是 pt,而 scale 只是位图倍数;与浮动图片一致,换成 pt→像素系数(含 96/72)。
    // 本函数仅 tile.rs 浮动形状渲染一处调用。
    // scroll_x_pt/scroll_y_pt 实际是逻辑像素(与浮动图片一致),位置先把 pt 换成像素再减。
    let pt2px = crate::tile_geometry::PT_TO_PX;
    let bitmap_scale = scale;
    let scale = scale * pt2px;
    // 视口裁剪：形状完全在可视区域外则跳过(全部在逻辑像素下比较)
    let x0_px = shape.x_pt * pt2px;
    let y0_px = shape.y_pt * pt2px;
    let x1_px = x0_px + shape.width_pt * pt2px;
    let y1_px = y0_px + shape.height_pt * pt2px;
    if y1_px < scroll_y_pt || y0_px > scroll_y_pt + (vh as f32 / bitmap_scale) {
        return;
    }
    if x1_px < scroll_x_pt || x0_px > scroll_x_pt + (vw as f32 / bitmap_scale) {
        return;
    }

    let sx = (x0_px - scroll_x_pt) * bitmap_scale + data_px_x as f32;
    let sy = (y0_px - scroll_y_pt) * bitmap_scale + data_px_y as f32;
    let w_px = shape.width_pt * scale;
    let h_px = shape.height_pt * scale;

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
                            sx + x * scale, sy + y * scale,
                        );
                    }
                    roe_model::common::PathCommand::QuadTo { x1, y1, x, y } => {
                        pb.quad_to(sx + x1 * scale, sy + y1 * scale, sx + x * scale, sy + y * scale);
                    }
                    roe_model::common::PathCommand::ArcTo { .. } => {
                        // 照抄 roe-writer：第一版未实现 ArcTo 转换，跳过。
                    }
                    roe_model::common::PathCommand::Close => {
                        pb.close();
                    }
                }
            }
        }
        roe_model::common::ShapeGeometry::Ellipse => {
            let cx = sx + w_px / 2.0;
            let cy = sy + h_px / 2.0;
            let rx = w_px / 2.0;
            let ry = h_px / 2.0;
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
            pb.line_to(sx + w_px, sy);
            pb.line_to(sx + w_px, sy + h_px);
            pb.line_to(sx, sy + h_px);
            pb.close();
        }
        // WROOT-108（v188）：RoundRect/Triangle 专属渲染分支已注释掉，理由同
        // roe-writer/src/tile.rs 与 roe-parser/src/ooxml/docx.rs 的 WROOT-108 注释
        // ——Parley 迁移期间的临时实现，非最终形态；pptx 侧 PROOT-101 已确认 triangle
        // 标准做法是走 PresetPath；docx parser 侧已对齐，这两个 variant 不会再从真实
        // 文件被构造，此处退化为矩形包围盒兜底（与 pptx 侧 `_ =>` 兜底视觉一致）。
        // 原实现（已注释，供未来若要恢复参考）：
        // roe_model::common::ShapeGeometry::RoundRect { radius } => { ... 四角贝塞尔圆角矩形 ... }
        // roe_model::common::ShapeGeometry::Triangle => { ... 等腰三角形 ... }
        roe_model::common::ShapeGeometry::RoundRect { .. } | roe_model::common::ShapeGeometry::Triangle => {
            pb.move_to(sx, sy);
            pb.line_to(sx + w_px, sy);
            pb.line_to(sx + w_px, sy + h_px);
            pb.line_to(sx, sy + h_px);
            pb.close();
        }
        roe_model::common::ShapeGeometry::Line => {
            pb.move_to(sx, sy);
            pb.line_to(sx + w_px, sy + h_px);
        }
        roe_model::common::ShapeGeometry::Custom(_) => {
            // 未识别的预设形状占位：本轮不画，与 roe-writer 侧行为一致。
        }
    }

    let built_path = match pb.finish() {
        Some(p) => p,
        None => return,
    };

    // <a:xfrm flipV="1"> 上下镜像：相对形状自身水平中轴做 Y 轴镜像。
    let built_path = if shape.flip_v {
        let mirror = tiny_skia::Transform::from_row(1.0, 0.0, 0.0, -1.0, 0.0, 2.0 * sy + h_px);
        built_path.clone().transform(mirror).unwrap_or(built_path)
    } else {
        built_path
    };

    // WROOT-XLSX-SHAPE-ROT-01: <a:xfrm rot="..."> 旋转。OOXML 语义：
    // xfrm off/ext 定义的是旋转前的 bounding box，绕该 box 中心点
    // 顺时针旋转 rot 度（普通角度制，shape.rot 已在解析层完成
    // 1/60000度→普通角度换算）。tiny_skia 屏幕坐标系 Y 轴向下，
    // from_rotate_at 的角度方向与 OOXML 顺时针语义一致，无需取负。
    // 旋转是整体变换，放在 flip_v 之后应用（绕 flip 之后的中心点转）。
    let rotate_transform = if shape.rot != 0.0 {
        let cx = sx + w_px / 2.0;
        let cy = sy + h_px / 2.0;
        Some(tiny_skia::Transform::from_rotate_at(shape.rot, cx, cy))
    } else {
        None
    };
    let built_path = if let Some(rt) = rotate_transform {
        built_path.clone().transform(rt).unwrap_or(built_path)
    } else {
        built_path
    };

    if let Some(roe_model::common::FillStyle::Solid(c)) = &shape.fill {
        let mut paint = tiny_skia::Paint::default();
        paint.set_color(tiny_skia::Color::from_rgba8(c.r, c.g, c.b, c.a));
        paint.anti_alias = true;
        pixmap.fill_path(&built_path, &paint, tiny_skia::FillRule::Winding, tiny_skia::Transform::identity(), None);
    }
    // 边框:按 shape.stroke 描边,宽度 pt 乘 pt→像素系数(scale 此时已含该系数),至少 1 像素。
    if let Some(st) = &shape.stroke {
        let mut paint = tiny_skia::Paint::default();
        paint.set_color_rgba8(st.color.r, st.color.g, st.color.b, st.color.a);
        paint.anti_alias = true;
        let mut sk = tiny_skia::Stroke::default();
        sk.width = (st.width * scale).max(1.0);
        pixmap.stroke_path(&built_path, &paint, &sk, tiny_skia::Transform::identity(), None);
    }
    // 空文本框占位:无填充、无文字的矩形画浅灰虚线框(与 Impress 占位符虚线一致:#A6A6A6、1px、4/3 节奏)。
    // 注意 scale 此时已是 pt→像素系数,1px 线宽按位图倍数取 scale / PT_TO_PX。
    let is_empty_text = shape.text_content.iter().all(|p| p.runs.iter().all(|r| r.text.trim().is_empty()));
    let fill_blank = match &shape.fill {
        None => true,
        Some(roe_model::common::FillStyle::Solid(c)) => c.r == 255 && c.g == 255 && c.b == 255,
        _ => false,
    };
    if fill_blank
        && shape.stroke.is_none()
        && is_empty_text
        && matches!(shape.geometry, roe_model::common::ShapeGeometry::Rect)
    {
        let bm = scale / crate::tile_geometry::PT_TO_PX;
        let mut paint = tiny_skia::Paint::default();
        paint.set_color_rgba8(0xA6, 0xA6, 0xA6, 255);
        paint.anti_alias = true;
        let mut stroke = tiny_skia::Stroke::default();
        stroke.width = 1.0 * bm;
        stroke.dash = tiny_skia::StrokeDash::new(vec![4.0 * bm, 3.0 * bm], 0.0);
        pixmap.stroke_path(&built_path, &paint, &stroke, tiny_skia::Transform::identity(), None);
    }
    // 其余 FillStyle（Gradient/Picture/PictureTiled/None）本轮不处理，
    // 样本集里 xlsx 侧 sp 目前只出现 srgbClr/schemeClr（组装成 Solid），
    // 见 xlsx.rs::extract_shapes_from_drawing 的已知限制。

    // 文字渲染：shape.text_content 是 Vec<Paragraph>（每个 text_run 独立
    // 成一个 Paragraph，见 collect_xlsx_shapes 的组装逻辑）。逐段落画，
    // 段落顶部起算的纵向偏移在 pt 空间累加后再乘 scale 换算成像素，
    // 与 roe-writer::render_shape_frame 的 WROOT-SHAPE-TEXT-01 做法一致。
    if !shape.text_content.is_empty() {
        let mut y_cursor_pt = 0.0f32;
        for para in &shape.text_content {
            if para.runs.is_empty() { continue; }
            let text: String = para.runs.iter().map(|r| r.text.as_str()).collect();
            if text.trim().is_empty() { continue; }

            let font_size_pt = para.runs.iter().find_map(|r| r.font_size).unwrap_or(12.0);
            // ROOT-082字体统一修复：与font_size_pt对称，Run.font_family本已存在
            // （roe-model/src/writer.rs），此前只读了font_size却漏读font_family，
            // 是与font_size同源但独立的债，字体族部分本次一并补上。
            let font_family_raw = para.runs.iter().find_map(|r| r.font_family.clone()).unwrap_or_else(|| "Calibri".to_string());
            // 字体名归一化：见row_height_measure::resolve_font_family_for_parley注释，
            // "Calibri"字符串直接传给Parley会丢字形，必须先转换。
            let font_family = crate::row_height_measure::resolve_font_family_for_parley(&font_family_raw).to_string();
            let line_height_pt = font_size_pt * 1.2;
            let bold = para.runs.iter().any(|r| r.bold.unwrap_or(false));

            // 文字颜色：取第一个 run 的 color（xlsx.rs::collect_xlsx_shapes
            // 已经按 run自身solidFill > style fontRef兜底 > 黑色 的优先级
            // 求好写入了 Run.color，这里直接读取，不再重复兜底判断）。
            let text_color = para.runs.iter().find_map(|r| r.color).unwrap_or(
                roe_model::common::Color { r: 0, g: 0, b: 0, a: 255 }
            );
            let brush: [u8; 4] = [text_color.r, text_color.g, text_color.b, text_color.a];
            let mut builder = layout_context.ranged_builder(font_context, &text, scale.max(1.0), true);
            builder.push_default(StyleProperty::Brush(brush));
            // ROOT-082字体统一修复：改读per-run font_family（fallback Calibri），
            // 不再硬编码Carlito，与上面font_size_pt的读取方式保持对称。
            builder.push_default(parley::StyleProperty::FontFamily(parley::FontFamily::named(font_family.as_str())));
            builder.push_default(LineHeight::Absolute(line_height_pt));
            builder.push_default(StyleProperty::FontSize(font_size_pt));
            if bold {
                builder.push_default(StyleProperty::FontWeight(FontWeight::BOLD));
            }

            let mut layout: parley::Layout<[u8; 4]> = builder.build(&text);
            layout.break_all_lines(Some(shape.width_pt.max(1.0)));
            layout.align(Alignment::Start, AlignmentOptions::default());

            let mut pen = TinySkiaPen::new(pixmap);
            if let Some(rt) = rotate_transform {
                pen.set_transform(rt);
            }
            for line in layout.lines() {
                let lm = line.metrics();
                let draw_y = sy + y_cursor_pt * scale;
                for item in line.items() {
                    if let PositionedLayoutItem::GlyphRun(glyph_run) = item {
                        render_glyph_run(&glyph_run, &mut pen, sx, draw_y, brush);
                    }
                }
                y_cursor_pt += lm.line_height;
            }
        }
    }
}
