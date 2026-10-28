//! 页眉/页脚渲染。
//!
//! [WROOT-任务A/B] 原本这段逻辑写在 tile.rs 的 render_viewport 内部
//! （约140行），随着 tile.rs 逼近3000行不宜再扩大，本次抽成独立文件。
//! 同时借这次抽取，把任务A遗留的"已知限制"（v79文档2.4节：只读
//! doc.header.default，所有页统一显示同一份内容）补成任务B要做的事：
//! 按每页的 section 归属 + 奇偶页 + 首页，取正确的那一份 header/footer。
//!
//! 取值优先级抄自 LibreOffice Writer 的 SwPageDesc::GetRightFormat /
//! GetLeftFormat（sw/inc/pagedesc.hxx, sw/source/core/layout/pagechg.cxx）：
//! 每个 section 的页眉页脚实际有 default/even/first 三份，本节没设置的
//! 时候（Option::None）才 fallback 到 WriterDocument 顶层字段。docx 原生
//! 概念（w:headerReference type="default/even/first"）与 LOK 的
//! Master/Left/FirstMaster 直接对应，不需要额外转换。

use tiny_skia::PixmapMut;
use parley::{Alignment, AlignmentOptions, GenericFamily, LineHeight, PositionedLayoutItem, StyleProperty};

use crate::frame::FrameArena;
use crate::tile::{render_glyph_run, RunBrush, TinySkiaPen, WriterTileRenderer};
use roe_model::writer::{HeaderFooterSet, Paragraph, WriterDocument};

/// 把一组段落拼成纯文本，段落间用 \n 分隔（任务A原有实现，原样保留）。
///
/// WROOT-131：page_no 为当前页的页码（全局页序，从 1 起）。run.field 为 PageNumber
/// 的 run（解析层从 PAGE 复杂域缓存值识别出来），文字用页码代替，忽略缓存值 text。
/// 未做：节内重起编号（w:pgNumType w:start）、NUMPAGES 等其他域。
fn hf_text_from_paragraphs(paras: &[Paragraph], page_no: usize, total_pages: usize) -> String {
    paras.iter()
        .map(|p| p.runs.iter()
            .map(|r| {
                if r.field == Some(roe_model::writer::FieldKind::PageNumber) {
                    page_no.to_string()
                } else if r.field == Some(roe_model::writer::FieldKind::NumPages) {
                    // WROOT-132：NUMPAGES = 文档总页数（arena.page_order.len()）
                    total_pages.to_string()
                } else {
                    r.text.clone()
                }
            })
            .collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// WROOT-136：一段 run 在拼接后字符串里的字节区间与样式（页眉页脚逐 run 样式用）。
#[derive(Clone, Debug)]
struct HfRunSpan {
    start: usize,
    end: usize,
    font_size: Option<f32>,
    bold: bool,
    script: roe_model::writer::ScriptPosition,
}

/// 与 hf_text_from_paragraphs 拼出完全相同的字符串，同时返回每个 run 的字节区间。
/// 段落间的 "\n" 不属于任何 run。
fn hf_runs_from_paragraphs(paras: &[Paragraph], page_no: usize, total_pages: usize)
    -> (String, Vec<HfRunSpan>) {
    let mut text = String::new();
    let mut spans: Vec<HfRunSpan> = Vec::new();
    for (pi, p) in paras.iter().enumerate() {
        if pi > 0 { text.push('\n'); }
        for r in &p.runs {
            let piece = if r.field == Some(roe_model::writer::FieldKind::PageNumber) {
                page_no.to_string()
            } else if r.field == Some(roe_model::writer::FieldKind::NumPages) {
                total_pages.to_string()
            } else {
                r.text.clone()
            };
            let start = text.len();
            text.push_str(&piece);
            let end = text.len();
            if end > start {
                spans.push(HfRunSpan {
                    start, end,
                    font_size: r.font_size,
                    bold: r.bold == Some(true),
                    script: r.script.unwrap_or(roe_model::writer::ScriptPosition::Normal),
                });
            }
        }
    }
    (text, spans)
}

/// 取第一个"有实际内容"段落的 alignment + 第一个 run 的 font_family/font_size
/// 作为整段 header/footer 的统一样式（任务A原有实现，原样保留）。
fn hf_style_from_paragraphs(paras: &[Paragraph])
    -> (Option<roe_model::style::Alignment>, Option<String>, Option<f32>) {
    for p in paras {
        if let Some(first_run) = p.runs.first() {
            return (p.alignment, first_run.font_family.clone(), first_run.font_size);
        }
    }
    (None, None, None)
}

fn map_alignment(a: roe_model::style::Alignment) -> Alignment {
    match a {
        roe_model::style::Alignment::Left    => Alignment::Left,
        roe_model::style::Alignment::Center  => Alignment::Center,
        roe_model::style::Alignment::Right   => Alignment::Right,
        roe_model::style::Alignment::Justify => Alignment::Justify,
    }
}

/// [WROOT-任务B] 按 LOK GetRightFormat/GetLeftFormat 的优先级，从一个
/// HeaderFooterSet 里选出"这一页"该用哪一份段落列表。
///
/// 优先级（对应 LOK 的 UseOnPage 标志位语义，这里没有单独的 share 开关，
/// 直接用"该档位是否为 None"来表达"是否与 default 共享"，效果等价）：
/// - is_first 且 set.first 有值 → 用 first
/// - !is_first 且是偶数页 且 even_and_odd_headers 开启 且 set.even 有值 → 用 even
/// - 否则 → 用 default
///
/// 返回 None 表示这一档完全没有内容（例如 n750935 Section1 的 header 就是空）。
fn pick_paragraphs<'a>(
    set: &'a HeaderFooterSet,
    is_first: bool,
    is_even_page: bool,
    even_and_odd_headers: bool,
) -> Option<&'a [Paragraph]> {
    if is_first {
        if let Some(ref first) = set.first {
            return Some(first);
        }
        // 没有专门的 first，按 LOK IsFirstShared 语义 fallback 到 default。
    } else if is_even_page && even_and_odd_headers {
        if let Some(ref even) = set.even {
            return Some(even);
        }
        // 没有专门的 even，fallback 到 default。
    }
    set.default.as_deref()
}

/// [WROOT-任务B] 给定页在 arena.page_order 里的下标（0-based），解析出这一页
/// 实际该用的 header/footer 段落列表 + section 的 title_page/even_and_odd
/// 设置。封装成一个函数，因为 header 和 footer 两处调用逻辑完全对称。
struct ResolvedHf<'a> {
    header_paras: Option<&'a [Paragraph]>,
    footer_paras: Option<&'a [Paragraph]>,
}

fn resolve_hf_for_page<'a>(
    doc: &'a WriterDocument,
    arena: &FrameArena,
    pid: roe_core::arena::PageId,
    page_order_index: usize,
) -> ResolvedHf<'a> {
    let section_idx = arena.page_section_index(pid);
    // sections 为空（单节文档/parser未产出分节数据）时，直接用顶层字段，
    // 语义与任务A的行为完全一致（fallback，不是新增分支）。
    let section = doc.sections.get(section_idx);

    // [WROOT-hf-parity] 修复：section.title_page 只表达"本节是否启用独立
    // 首页格式"这个节级别开关，不等于"这一页就是首页"——必须同时满足
    // "这一页确实是本节遇到的第一页"。同理，奇偶判断此前用的是
    // page_order_index（文档全局物理页码），应改用"节内第几页"，
    // 否则一个 section 从文档第3页开始时，节内第1页会被全局页码误判成
    // 偶数页。page_index_in_section 是1-based，"节内第1页"即取值为1。
    let page_in_section = arena.page_index_in_section(pid);
    // WROOT-137：continuous 节不开启新的"首页"。只有文档第1页（page_order_index==0）
    // 才可能因 continuous 节的 titlePg 使用 first 档；其余页（如 continuous 节因边距变化
    // 被强制换出的新页，LO 断言 cont-sect-break-header-footer 第3页取 default 档）不算首页。
    let is_continuous_section = section.and_then(|s| s.section_type.as_deref()) == Some("continuous");
    let is_first = page_in_section == 1
        && section.and_then(|s| s.title_page).unwrap_or(doc.title_page)
        && !(is_continuous_section && page_order_index > 0);
    // [WROOT-093] sectPr 的 w:type（evenPage/oddPage）是节级别的显式强制
    // 声明，语义上应优先于"节内页码自然奇偶"——当一个 section 恰好只有1页
    // 时，page_in_section 恒为1（奇数），若该 section 的 type 显式声明为
    // evenPage，靠 page_in_section 推算会得出错误结果。只有当本节没有
    // 显式声明 type（None，即默认 nextPage 语义或 continuous）时，才退回
    // 用 page_in_section 奇偶判断。
    let is_even_page = match section.and_then(|s| s.section_type.as_deref()) {
        Some("evenPage") => true,
        Some("oddPage") => false,
        _ => page_in_section % 2 == 0,
    };
    let even_and_odd = section
        .and_then(|s| s.even_and_odd_headers)
        .unwrap_or(doc.even_and_odd_headers);

    let header_set = section.and_then(|s| s.header.as_ref()).unwrap_or(&doc.header);
    let footer_set = section.and_then(|s| s.footer.as_ref()).unwrap_or(&doc.footer);

    ResolvedHf {
        header_paras: pick_paragraphs(header_set, is_first, is_even_page, even_and_odd),
        footer_paras: pick_paragraphs(footer_set, is_first, is_even_page, even_and_odd),
    }
}

/// v280: 把一组页眉/页脚段落逐段排版（每段各自对齐、各自字体），返回 (layout, 高度pt)
fn build_hf_layouts(
    renderer: &mut WriterTileRenderer,
    paras: &[Paragraph],
    page_no: usize,
    total_pages: usize,
    scale: f32,
    text_width_px: f32,
) -> Vec<(parley::Layout<RunBrush>, f32)> {
    let mut out: Vec<(parley::Layout<RunBrush>, f32)> = Vec::new();
    let brush = RunBrush { color: [80, 80, 80, 255], run_idx: None };
    for p in paras {
        let one = std::slice::from_ref(p);
        let (text, spans) = hf_runs_from_paragraphs(one, page_no, total_pages);
        if text.trim().is_empty() { continue; }
        let (align, family, size) = hf_style_from_paragraphs(one);
        let fs = size.unwrap_or(10.0);
        let max_run_pt = spans.iter()
            .filter(|sp| sp.script == roe_model::writer::ScriptPosition::Normal)
            .map(|sp| sp.font_size.unwrap_or(fs))
            .fold(fs, f32::max);
        let line_h = max_run_pt * 1.4;
        let mut b = renderer.layout_context.ranged_builder(
            &mut renderer.font_context, &text, scale.max(1.0), true,
        );
        b.push_default(StyleProperty::Brush(brush));
        if let Some(ref fam) = family {
            b.push_default(StyleProperty::FontFamily(parley::FontFamily::named(fam.as_str())));
        } else {
            b.push_default(GenericFamily::SystemUi);
        }
        b.push_default(LineHeight::Absolute(line_h));
        b.push_default(StyleProperty::FontSize(fs));
        for sp in &spans {
            let base = sp.font_size.unwrap_or(fs);
            let sz = if sp.script == roe_model::writer::ScriptPosition::Normal { base } else { base * 0.65 };
            b.push(StyleProperty::FontSize(sz), sp.start..sp.end);
            if sp.bold {
                b.push(StyleProperty::FontWeight(parley::FontWeight::BOLD), sp.start..sp.end);
            }
        }
        let mut layout: parley::Layout<RunBrush> = b.build(&text);
        layout.break_all_lines(Some(text_width_px));
        layout.align(
            align.map(map_alignment).unwrap_or(Alignment::Start),
            AlignmentOptions::default(),
        );
        let h_pt = layout.height() / scale.max(1.0);
        out.push((layout, h_pt));
    }
    out
}

/// v280: 从 y_start_pt（全局 pt）起自上而下依次画各段
fn draw_hf_layouts(
    pixmap: &mut PixmapMut,
    layouts: &[(parley::Layout<RunBrush>, f32)],
    origin_x_px: f32,
    y_start_pt: f32,
    vp_y: f32,
    scale: f32,
) {
    let mut y = y_start_pt;
    for (layout, h) in layouts {
        let origin_y_px = (y - vp_y) * scale;
        let mut pen = TinySkiaPen::new(pixmap);
        for line in layout.lines() {
            let line: parley::Line<'_, RunBrush> = line;
            for item in line.items() {
                if let PositionedLayoutItem::GlyphRun::<RunBrush>(glyph_run) = item {
                    render_glyph_run(&glyph_run, &mut pen, origin_x_px, origin_y_px);
                }
            }
        }
        y += *h;
    }
}

/// 渲染所有与视口相交的页的页眉/页脚。
/// 从 tile.rs::render_viewport 搬出，签名保持能拿到的东西不变
/// （arena/doc/vp_*/scale/pixmap 全部沿用原来的局部变量）。
pub(crate) fn render_headers_footers(
    renderer: &mut WriterTileRenderer,
    arena: &FrameArena,
    doc: &WriterDocument,
    vp_x: f32, vp_y: f32, vp_w: f32,
    scale: f32,
    pixmap: &mut PixmapMut,
) {
    let _ = vp_w; // 保留参数位，与 render_viewport 调用处签名对齐；当前实现未直接用到。

    for (page_order_index, &pid) in arena.page_order.iter().enumerate() {
        let page = &arena.pages[pid];
        let pg_y0 = arena.page_global_y(pid);
        let pg_y1 = pg_y0 + page.height;
        let vp_h_check_top = vp_y;
        // 视口裁剪：与原逻辑一致，vp_h 通过外部循环前已确定的范围传入。
        // 这里用 pg_y1 < vp_y 的判断沿用原实现，vp_h 上界判断交给调用方
        // （render_viewport 已经在外层用同样的条件筛过一次 page_order，
        // 这里保留判断是为了这个函数将来能独立被调用/测试）。
        if pg_y1 < vp_h_check_top { continue; }

        let resolved = resolve_hf_for_page(doc, arena, pid, page_order_index);

        let page_no = page_order_index + 1;
        let total_pages = arena.page_order.len();
        // WROOT-136：改用 hf_runs_from_paragraphs，同时拿到逐 run 区间样式。
        let header_pair: Option<(String, Vec<HfRunSpan>)> = resolved.header_paras
            .map(|paras| hf_runs_from_paragraphs(paras, page_no, total_pages))
            .filter(|(t, _)| !t.trim().is_empty());
        let footer_pair: Option<(String, Vec<HfRunSpan>)> = resolved.footer_paras
            .map(|paras| hf_runs_from_paragraphs(paras, page_no, total_pages))
            .filter(|(t, _)| !t.trim().is_empty());
        let header_text: Option<String> = header_pair.as_ref().map(|(t, _)| t.clone());
        let footer_text: Option<String> = footer_pair.as_ref().map(|(t, _)| t.clone());
        let header_spans: Vec<HfRunSpan> = header_pair.map(|(_, sp)| sp).unwrap_or_default();
        let footer_spans: Vec<HfRunSpan> = footer_pair.map(|(_, sp)| sp).unwrap_or_default();
        if header_text.is_none() && footer_text.is_none() {
            continue;
        }
        let (header_align, header_font_family, header_font_size) = resolved.header_paras
            .map(hf_style_from_paragraphs)
            .unwrap_or((None, None, None));
        let (footer_align, footer_font_family, footer_font_size) = resolved.footer_paras
            .map(hf_style_from_paragraphs)
            .unwrap_or((None, None, None));

        let hf_font_size_pt = 10.0_f32;
        let hf_brush = RunBrush { color: [80, 80, 80, 255], run_idx: None };
        let text_width_pt = page.width - page.margin_left - page.margin_right;
        // pt/px 单位换算：见任务A交接记录，builder 用 scale.max(1.0) 构建
        // （像素坐标系排版），break_all_lines 的宽度必须同单位。
        let text_width_px = text_width_pt * scale;

        if let Some(paras) = resolved.header_paras {
            let ls = build_hf_layouts(renderer, paras, page_no, total_pages, scale, text_width_px);
            let y0 = pg_y0 + page.margin_top * 0.55;
            draw_hf_layouts(pixmap, &ls, (page.margin_left - vp_x) * scale, y0, vp_y, scale);
        }

        if let Some(paras) = resolved.footer_paras {
            let ls = build_hf_layouts(renderer, paras, page_no, total_pages, scale, text_width_px);
            let total: f32 = ls.iter().map(|l| l.1).sum();
            let mut y0 = pg_y1 - page.margin_bottom * 0.85;
            if y0 + total > pg_y1 - 6.0 { y0 = (pg_y1 - 6.0 - total).max(pg_y0); }
            draw_hf_layouts(pixmap, &ls, (page.margin_left - vp_x) * scale, y0, vp_y, scale);
        }
    }
}


#[cfg(test)]
mod tests_wroot131_page_no {
    use super::*;
    use roe_model::writer::{FieldKind, Run};

    #[test]
    fn page_number_run_uses_page_no_not_cached_text() {
        let para = Paragraph {
            runs: vec![
                Run { text: "Page ".to_string(), ..Default::default() },
                Run { text: "2".to_string(), field: Some(FieldKind::PageNumber), ..Default::default() },
            ],
            ..Default::default()
        };
        assert_eq!(hf_text_from_paragraphs(std::slice::from_ref(&para), 1, 9), "Page 1");
        assert_eq!(hf_text_from_paragraphs(std::slice::from_ref(&para), 7, 9), "Page 7");
    }

    /// WROOT-132：NumPages run 用总页数代替缓存文字；PageNumber 与 NumPages 并存互不串。
    #[test]
    fn numpages_run_uses_total_pages_not_cached_text() {
        let para = Paragraph {
            runs: vec![
                Run { text: "Page ".to_string(), ..Default::default() },
                Run { text: "1".to_string(), field: Some(FieldKind::PageNumber), ..Default::default() },
                Run { text: " of ".to_string(), ..Default::default() },
                Run { text: "9".to_string(), field: Some(FieldKind::NumPages), ..Default::default() },
            ],
            ..Default::default()
        };
        assert_eq!(hf_text_from_paragraphs(std::slice::from_ref(&para), 2, 3), "Page 2 of 3");
        assert_eq!(hf_text_from_paragraphs(std::slice::from_ref(&para), 5, 5), "Page 5 of 5");
    }
}
