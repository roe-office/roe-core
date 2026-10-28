//! roe_render_sheet_viewport / roe_render_page / roe_get_render_page_size
//!
//! （2026-08-22 v92会话注：本文件顶部注释与下方部分历史注释中的
//! "roe_render_sheet_tile" 已更新为 "roe_render_sheet_viewport"——项目
//! 已从 Calc tile 瓦片方案切换到虚拟视口方案，旧函数名已不存在。）
//!
//! buf 由前端预分配（RGBA8），Rust 用
//! tiny_skia::PixmapMut::from_bytes(buf, width, height) 直接绘制，零拷贝。
//!
//! v1 简化：
//! - Writer/Impress 每次调用都重新创建 FontSystem/SwashCache 并重新 layout，
//!   不做渲染器/布局结果缓存（DocumentEntry 暂不加缓存字段）。后续如有性能
//!   问题，再回头给 DocumentEntry 加 layout cache。
//!
//! ⚠ 2026-06-18 修复（office-rs2 反馈 ±1px buffer mismatch）：
//!   roe_render_page 的 scale 由 `width as f32 / 原始宽度` 反推，pw/ph 由
//!   `(原始尺寸 * scale) as u32` 截断得到。调用方独立计算期望尺寸的方式
//!   与这套"反推再截断"逻辑是两条独立浮点路径，可能产生 ±1px 偏差。
//!   修复方向A：新增 roe_get_render_page_size 查询函数，与 roe_render_page
//!   共用 compute_render_dims 计算逻辑，调用方应先查询再分配 buffer，
//!   不应再自行猜测/计算期望尺寸。

use std::ffi::CString;
use std::os::raw::c_char;
use std::slice;

use roe_core::tile::CalcTileRenderer;
use roe_core::tile_geometry::CalcRenderConfig;
use roe_core::row_height_measure::layout_measure_viewport;
use roe_parser::RoeDocument;
use roe_writer::layout::arena_to_layout_pages;
use roe_writer::layout::ParagraphLayoutEngine;
use roe_writer::FontRegistry;
use roe_writer::WriterTileRenderer;
use roe_impress::slide_renderer::SlideRenderer;

use crate::error::{set_last_error, RoeErrorCode};
use crate::handle;

/// 根据原始内容尺寸（pt）和调用方期望的 width，反推 scale 并计算精确的
/// (pw, ph) 像素尺寸。`roe_render_page` 与 `roe_get_render_page_size`
/// 共用此函数，保证两者计算结果一致，不会出现"查询说A、渲染却用B"的偏差。
fn compute_render_dims(orig_w: f32, orig_h: f32, width: u32) -> (f32, u32, u32) {
    let scale = if orig_w > 0.0 { width as f32 / orig_w } else { 1.0 };
    let pw = (orig_w * scale) as u32;
    let ph = (orig_h * scale) as u32;
    (scale, pw, ph)
}

/// 查询 roe_render_page 在给定 width 下精确期望的 (width, height) 像素尺寸，
/// 返回 JSON 字符串 `{"width":W,"height":H}`。调用方应先调用此函数算出
/// 精确尺寸再分配 buffer，避免因浮点反推/截断导致的 buffer size mismatch
/// （历史问题：office-rs2 曾遇到 ±1px 偏差，2026-06-18 修复方向A）。
/// 失败返回 null，错误信息见 roe_last_error_message。
/// 返回值需要 roe_free_string 释放。
#[no_mangle]
pub extern "C" fn roe_get_render_page_size(handle: u64, page_idx: u32, width: u32) -> *mut c_char {
    if width == 0 {
        set_last_error("roe_get_render_page_size: width must be > 0");
        return std::ptr::null_mut();
    }

    // 先建缓存，避免在 with_entry 内部再次借用
    if matches!(self::handle::with_entry(handle, |e| matches!(e.doc, RoeDocument::Writer(_))), Some(true))
        && !self::handle::ensure_writer_layout(handle) {
            set_last_error("roe_get_render_page_size: layout cache build failed");
            return std::ptr::null_mut();
        }
    let result = self::handle::with_entry(handle, |entry| {
        match &mut entry.doc {
            RoeDocument::Writer(_doc) => {
                let pages = match entry.writer_layout_cache.as_ref() {
                    Some(cache) => {
                        let doc = match &entry.doc { RoeDocument::Writer(d) => d, _ => unreachable!() };
                        arena_to_layout_pages(&cache.arena, doc)
                    }
                    None => vec![],
                };
                let page = match pages.get(page_idx as usize) {
                    Some(p) => p,
                    None => return Err(format!(
                        "page_idx {} out of range (pages={})", page_idx, pages.len()
                    )),
                };
                let (_, pw, ph) = compute_render_dims(page.width, page.height, width);
                Ok((pw, ph))
            }
            RoeDocument::Impress(presentation) => {
                let (slide_w, slide_h) = presentation.slide_size;
                if page_idx as usize >= presentation.slides.len() {
                    return Err(format!(
                        "page_idx {} out of range (slides={})", page_idx, presentation.slides.len()
                    ));
                }
                let (_, pw, ph) = compute_render_dims(slide_w, slide_h, width);
                Ok((pw, ph))
            }
            RoeDocument::Calc(_) => Err("document is a Calc workbook, use roe_render_sheet_viewport".to_string()),
        }
    });

    match result {
        Some(Ok((pw, ph))) => {
            let json = format!("{{\"width\":{},\"height\":{}}}", pw, ph);
            match CString::new(json) {
                Ok(cs) => cs.into_raw(),
                Err(e) => {
                    set_last_error(format!("roe_get_render_page_size: {e}"));
                    std::ptr::null_mut()
                }
            }
        }
        Some(Err(e)) => {
            set_last_error(format!("roe_get_render_page_size: {e}"));
            std::ptr::null_mut()
        }
        None => {
            set_last_error("roe_get_render_page_size: invalid handle");
            std::ptr::null_mut()
        }
    }
}

/// Writer / Impress 整页渲染：page_idx 对应页码/幻灯片序号。
#[no_mangle]
pub extern "C" fn roe_render_page(
    handle: u64,
    page_idx: u32,
    width: u32,
    height: u32,
    buf: *mut u8,
    buf_len: usize,
) -> i32 {
    if buf.is_null() {
        set_last_error("roe_render_page: buf is null");
        return RoeErrorCode::NullPointer as i32;
    }
    if width == 0 || height == 0 {
        set_last_error("roe_render_page: width/height must be > 0");
        return RoeErrorCode::RenderError as i32;
    }

    // SAFETY: 同 roe_render_sheet_viewport。
    let buf_slice = unsafe { slice::from_raw_parts_mut(buf, buf_len) };

    // 先在外层 with_entry 之前建立缓存，避免 double borrow
    if matches!(self::handle::with_entry(handle, |e| matches!(e.doc, RoeDocument::Writer(_))), Some(true))
        && !self::handle::ensure_writer_layout(handle) {
            set_last_error("roe_render_page: layout cache build failed");
            return RoeErrorCode::RenderError as i32;
        }
    let result = self::handle::with_entry(handle, |entry| {
        match &mut entry.doc {
            RoeDocument::Writer(_) => {
                let doc = match &entry.doc { RoeDocument::Writer(d) => d, _ => unreachable!() };
                let arena = match entry.writer_layout_cache.as_ref() {
                    Some(cache) => &cache.arena,
                    None => {
                        set_last_error("roe_render_page: writer_layout_cache 未建立（内部错误）".to_string());
                        return RoeErrorCode::RenderError as i32;
                    }
                };

                let pid = match arena.page_order.get(page_idx as usize) {
                    Some(&pid) => pid,
                    None => {
                        set_last_error(format!(
                            "roe_render_page: page_idx {} out of range (pages={})",
                            page_idx,
                            arena.page_order.len()
                        ));
                        return RoeErrorCode::RenderError as i32;
                    }
                };
                let page = &arena.pages[pid];

                let (scale, pw, ph) = compute_render_dims(page.width, page.height, width);
                // 注：scale 仅用于 compute_render_dims 内部推导 pw/ph，
                // render_viewport 自己会用 pixel_w/vp_w 重新算一次等价的 scale，
                // 这里提前消费掉避免 unused warning，同时不影响下面 match
                // 作为该分支的尾表达式（尾表达式后面不能再跟语句，否则
                // 整个分支类型会从 i32 变成 ()）。
                let _ = scale;
                let vp_y = arena.page_global_y(pid);

                let mut renderer = WriterTileRenderer::new();
                match renderer.render_viewport(arena, doc, 0.0, vp_y, page.width, page.height, pw, ph, buf_slice) {
                    Ok(_) => RoeErrorCode::Ok as i32,
                    Err(e) => {
                        set_last_error(format!("roe_render_page: {e}"));
                        RoeErrorCode::RenderError as i32
                    }
                }
            }

            RoeDocument::Impress(presentation) => {
                let (slide_w, slide_h) = presentation.slide_size;

                let slide = match presentation.slides.get(page_idx as usize) {
                    Some(s) => s,
                    None => {
                        set_last_error(format!(
                            "roe_render_page: page_idx {} out of range (slides={})",
                            page_idx,
                            presentation.slides.len()
                        ));
                        return RoeErrorCode::RenderError as i32;
                    }
                };

                let (scale, _, _) = compute_render_dims(slide_w, slide_h, width);

                let mut renderer = SlideRenderer::new(slide_w, slide_h);
                let runtime_ref = entry.animation.get(&(page_idx as usize)).map(|s| &s.runtime);
                match renderer.render_slide(slide, &presentation.styles, runtime_ref, scale, buf_slice, &presentation.theme_colors, Some(&presentation.master_slides)) {
                    Ok(_) => RoeErrorCode::Ok as i32,
                    Err(e) => {
                        set_last_error(format!("roe_render_page: {e}"));
                        RoeErrorCode::RenderError as i32
                    }
                }
            }

            RoeDocument::Calc(_) => {
                set_last_error("roe_render_page: document is a Calc workbook, use roe_render_sheet_viewport");
                RoeErrorCode::Unsupported as i32
            }
        }
    });

    match result {
        Some(code) => code,
        None => {
            set_last_error("roe_render_page: invalid handle");
            RoeErrorCode::InvalidHandle as i32
        }
    }
}


/// 虚拟视口渲染：渲染整个可视区域（含行号/列字母标题栏）到 buf。
/// scroll_x_pt / scroll_y_pt：逻辑滚动位置（pt，100% zoom）。
/// vw / vh：viewport 像素宽高。
/// zoom_pct：缩放百分比（100=1x，200=2x）。
/// header_col_w_pt：左侧行号标题栏宽度（pt），传 0 表示不画。
/// header_row_h_pt：顶部列字母标题栏高度（pt），传 0 表示不画。
/// buf 由调用方预分配，大小必须等于 vw * vh * 4。
/// 成功返回 0，失败返回负数错误码。
#[no_mangle]
pub extern "C" fn roe_render_sheet_viewport(
    handle: u64,
    sheet_idx: u32,
    scroll_x_pt: f32,
    scroll_y_pt: f32,
    vw: u32,
    vh: u32,
    zoom_pct: u32,
    header_col_w_pt: f32,
    header_row_h_pt: f32,
    buf: *mut u8,
    buf_len: usize,
) -> i32 {
    if buf.is_null() {
        set_last_error("roe_render_sheet_viewport: buf is null");
        return RoeErrorCode::NullPointer as i32;
    }
    if vw == 0 || vh == 0 {
        set_last_error("roe_render_sheet_viewport: vw/vh must be > 0");
        return RoeErrorCode::RenderError as i32;
    }
    let buf_slice = unsafe { slice::from_raw_parts_mut(buf, buf_len) };

    // ROOT-082 (optimalRowHeight) 阶段一：Layout。
    // 独占可变借用 &mut Workbook，对视口范围内 Auto{is_dirty:true} 的行
    // 触发"贪吃蛇式动态推进 + Overscan"测量，写回 RowHeight::Auto.current。
    // 借用极短暂：只做测量写回，不做任何绘制。
    //
    // 时序说明（见 roe-ext_progress_v140.md 3.1节）：这里独立算一遍
    // data_logical_h，与阶段二 render_viewport_into 内部的算法完全一致
    // （scale/header 换算逻辑相同），是为了在不改变 render_viewport_into
    // 签名和职责的前提下，让 Layout 阶段能独立获得同样的视口底部坐标。
    // 两处算法必须保持一致，若未来任一处的换算逻辑变更，需同步修改。
    self::handle::with_entry(handle, |entry| {
        if let RoeDocument::Calc(workbook) = &mut entry.doc {
            if let Some(sheet) = workbook.sheets.get_mut(sheet_idx as usize) {
                let cfg = CalcRenderConfig::default();
                let scale = (zoom_pct.max(100) as f32) / 100.0;
                let data_vh = vh as i32 - (header_row_h_pt * scale) as i32;
                if data_vh > 0 {
                    let data_logical_h = data_vh as f32 / scale;
                    let px2pt = 72.0f32 / 96.0f32;
                    layout_measure_viewport(sheet, &cfg, scroll_y_pt * px2pt, data_logical_h * px2pt);
                }
            }
        }
    });

    // ROOT-082 阶段二：Paint。纯只读借用，原有逻辑完全不变——此时
    // row_height_at 读到的都是阶段一已经"铺平"写回的真实测量值
    // （视口内），未覆盖到的行仍是估算值（符合预期，见
    // tile_geometry.rs::row_height_at 文档注释）。
    let result = self::handle::with_entry(handle, |entry| {
        match &entry.doc {
            RoeDocument::Calc(workbook) => {
                // ROOT-078 (eval_hook)：注入公式求值能力，条件格式的
                // expr 类型规则 / cfvo formula-num 阈值渲染时需要用到
                // （见 roe-core::formula_hook 模块文档）。CachedEvaluator
                // 生命周期绑定这一次渲染调用，内部 AST 解析缓存在
                // renderer 析构时一并释放。
                let evaluator = Box::new(roe_calc::CachedEvaluator::new(workbook));
                let mut renderer = CalcTileRenderer::new(workbook)
                    .with_formula_evaluator(evaluator);
                renderer.sheet_idx = sheet_idx as usize;
                if renderer.render_viewport_into(
                    scroll_x_pt, scroll_y_pt, vw, vh, zoom_pct,
                    header_col_w_pt, header_row_h_pt, buf_slice,
                ) {
                    RoeErrorCode::Ok as i32
                } else {
                    set_last_error(format!(
                        "roe_render_sheet_viewport: render failed (buf_len={}, expected {})",
                        buf_slice.len(), vw as usize * vh as usize * 4
                    ));
                    RoeErrorCode::RenderError as i32
                }
            }
            _ => {
                set_last_error("roe_render_sheet_viewport: document is not a Calc workbook");
                RoeErrorCode::Unsupported as i32
            }
        }
    });
    match result {
        Some(code) => code,
        None => {
            set_last_error("roe_render_sheet_viewport: invalid handle");
            RoeErrorCode::InvalidHandle as i32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::WriterOptions;
    use std::collections::{HashMap, VecDeque};

    use roe_core::tile::CalcRenderConfig;
    use roe_model::calc::{SparseGrid, Sheet, Workbook};
    use roe_model::common::{Color as ModelColor, Transform as ModelTransform};
    use roe_model::impress::*;
    use roe_parser::odf::fodt::FodtParser;

    use crate::handle::DocumentEntry;

    // ---------- 构造测试文档 ----------

    fn make_calc_doc() -> RoeDocument {
        RoeDocument::Calc(Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
                col_widths: vec![],
                row_heights: vec![],
                frozen_rows: 0,
                frozen_cols: 0,
                cell_formats:     std::collections::HashMap::new(),
                cond_formats:     vec![],
                data_validations: vec![],
                auto_filter:      None,
                merged_cells:     vec![],
                pivot:            None,
                ..Default::default()
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        })
    }

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
  </office:styles>
  <office:body>
    <office:text>
      <text:p text:style-name="Default">roe-api render_page 测试段落。</text:p>
    </office:text>
  </office:body>
</office:document>"#;

    fn make_writer_doc() -> RoeDocument {
        let doc = FodtParser::parse(SAMPLE_FODT).expect("fodt parse failed");
        RoeDocument::Writer(doc)
    }

    fn make_impress_doc() -> RoeDocument {
        let slide = Slide { layout_idx: None,
            shapes: vec![Shape {
                id: 1,
                geometry: ShapeGeometry::Rect,
                position: ModelTransform::from_rect(50.0, 50.0, 200.0, 100.0),
                style: ShapeStyle {
                    fill: Some(FillStyle::Solid(ModelColor { r: 70, g: 130, b: 200, a: 255 })),
                    stroke: None,
                    shadow: None,
                    head_arrow: None,
                    tail_arrow: None,
                },
                content: ShapeContent::Empty,
                text_body: Default::default(),
            placeholder: None,
            end_cxn: None,
            st_cxn: None,
        }],
            master_idx: None,
            animation_tree: AnimationNode::TimingRoot { children: vec![] },
            transition: None,
            notes: None,
            background: None,
            layout_shapes: vec![],
        };
        RoeDocument::Impress(Presentation {
            styles: roe_model::style::StyleRegistry::default(),
            slide_size: (960.0, 540.0),
            slides: vec![slide],
            master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() })
    }

    fn open(doc: RoeDocument) -> u64 {
        handle::insert(DocumentEntry {
            doc,
            snapshot: None,
            is_shadow: false,
            events: VecDeque::new(),
            animation: HashMap::new(),
            writer_layout_cache: None,
            writer_font_registry: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            writer_options: WriterOptions::default(),
            impress_font_registry: None,
            writer_parley_font_context: None,
            calc_engine: None,
            pending_comment_hits: Vec::new(),
        })
    }

    // roe_render_sheet_tile 测试块（原 tile 瓦片方案时代遗留）已于 v92
    // 会话删除：该函数本体已不存在（项目已从 Calc tile 瓦片方案切换到
    // roe_render_sheet_viewport 虚拟视口方案），这4个测试引用的是一个
    // 早已不存在的函数名，属于纯死代码，导致 cargo test -p roe-api 无法
    // 编译。viewport 方案的测试见下方 test_render_sheet_viewport_basic。

    // ---------- roe_render_page ----------

    #[test]
    fn test_render_page_writer_ok() {
        let h = open(make_writer_doc());

        // 单独跑一遍 layout 拿到 page.width/height，按 roe_render_page 内部相同
        // 公式推导 pw/ph，避免浮点截断导致 buf 尺寸不一致。
        let (pw, ph) = handle::with_entry(h, |entry| match &entry.doc {
            RoeDocument::Writer(doc) => {
                let registry = FontRegistry::new_with_system_fonts();
                let mut engine = ParagraphLayoutEngine::new(registry);
                let result = engine.layout(doc);
                let pages = arena_to_layout_pages(&result.arena, doc);
                let page = &pages[0];
                let width = page.width as u32;
                let scale = width as f32 / page.width;
                let pw = (page.width * scale) as u32;
                let ph = (page.height * scale) as u32;
                (pw, ph)
            }
            _ => unreachable!(),
        }).expect("handle missing");

        let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];
        let code = roe_render_page(h, 0, pw, ph, buf.as_mut_ptr(), buf.len());
        assert_eq!(code, RoeErrorCode::Ok as i32);

        let all_white = buf.chunks(4).all(|p| p[0] == 0xff && p[1] == 0xff && p[2] == 0xff);
        assert!(!all_white, "rendered page should have non-white pixels (border + text)");

        handle::remove(h);
    }

    #[test]
    fn test_render_page_impress_ok() {
        let h = open(make_impress_doc());

        let (slide_w, slide_h) = (960.0_f32, 540.0_f32);
        let width = slide_w as u32;
        let scale = width as f32 / slide_w;
        let pw = (slide_w * scale) as u32;
        let ph = (slide_h * scale) as u32;

        let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];
        let code = roe_render_page(h, 0, pw, ph, buf.as_mut_ptr(), buf.len());
        assert_eq!(code, RoeErrorCode::Ok as i32);

        let all_white = buf.chunks(4).all(|p| p[0] == 0xff && p[1] == 0xff && p[2] == 0xff);
        assert!(!all_white, "rendered slide should have non-white pixels (shape + border)");

        handle::remove(h);
    }

    #[test]
    fn test_render_page_calc_unsupported() {
        let h = open(make_calc_doc());
        let mut buf = vec![0u8; 4 * 4 * 4];
        let code = roe_render_page(h, 0, 4, 4, buf.as_mut_ptr(), buf.len());
        assert_eq!(code, RoeErrorCode::Unsupported as i32);
        handle::remove(h);
    }

    #[test]
    fn test_render_page_invalid_handle() {
        let mut buf = vec![0u8; 4];
        let code = roe_render_page(999_999, 0, 1, 1, buf.as_mut_ptr(), buf.len());
        assert_eq!(code, RoeErrorCode::InvalidHandle as i32);
    }

    #[test]
    fn test_render_page_zero_dims() {
        let h = open(make_writer_doc());
        let mut buf = vec![0u8; 4];
        let code = roe_render_page(h, 0, 0, 0, buf.as_mut_ptr(), buf.len());
        assert_eq!(code, RoeErrorCode::RenderError as i32);
        handle::remove(h);
    }

    // ---------- roe_get_render_page_size ----------

    /// 从 roe_get_render_page_size 返回的 JSON `{"width":W,"height":H}`
    /// 中解析出 (W, H)。测试专用的简单字符串解析，不引入额外依赖。
    fn parse_size_json(ptr: *mut c_char) -> (u32, u32) {
        assert!(!ptr.is_null(), "roe_get_render_page_size returned null");
        let s = unsafe { std::ffi::CStr::from_ptr(ptr) }.to_str().unwrap().to_string();
        crate::roe_free_string(ptr);
        let w = s.split("\"width\":").nth(1).unwrap()
            .split(',').next().unwrap()
            .trim().parse::<u32>().unwrap();
        let h = s.split("\"height\":").nth(1).unwrap()
            .split('}').next().unwrap()
            .trim().parse::<u32>().unwrap();
        (w, h)
    }

    #[test]
    fn test_get_render_page_size_writer_matches_render() {
        let h = open(make_writer_doc());

        // 用一个"任意挑选"的 width（不是 page.width 本身的整数值），
        // 验证查询出来的 (pw, ph) 一定能让 roe_render_page 渲染成功，
        // 不需要调用方自己猜测/反推/重试。
        let width = 850u32;
        let size_ptr = roe_get_render_page_size(h, 0, width);
        let (pw, ph) = parse_size_json(size_ptr);
        assert!(pw > 0 && ph > 0);

        let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];
        let code = roe_render_page(h, 0, pw, ph, buf.as_mut_ptr(), buf.len());
        assert_eq!(code, RoeErrorCode::Ok as i32, "查询出的尺寸应该让渲染一次成功");

        handle::remove(h);
    }

    #[test]
    fn test_get_render_page_size_impress_matches_render() {
        let h = open(make_impress_doc());

        let width = 640u32;
        let size_ptr = roe_get_render_page_size(h, 0, width);
        let (pw, ph) = parse_size_json(size_ptr);
        assert!(pw > 0 && ph > 0);

        let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];
        let code = roe_render_page(h, 0, pw, ph, buf.as_mut_ptr(), buf.len());
        assert_eq!(code, RoeErrorCode::Ok as i32, "查询出的尺寸应该让渲染一次成功");

        handle::remove(h);
    }

    #[test]
    fn test_get_render_page_size_invalid_handle() {
        let ptr = roe_get_render_page_size(999_999, 0, 800);
        assert!(ptr.is_null());
    }

    #[test]
    fn test_get_render_page_size_calc_unsupported() {
        let h = open(make_calc_doc());
        let ptr = roe_get_render_page_size(h, 0, 800);
        assert!(ptr.is_null(), "Calc 文档应返回 null（应使用 roe_render_sheet_viewport）");
        handle::remove(h);
    }

    #[test]
    fn test_get_render_page_size_zero_width() {
        let h = open(make_writer_doc());
        let ptr = roe_get_render_page_size(h, 0, 0);
        assert!(ptr.is_null());
        handle::remove(h);
    }
}


/// 查询 Writer 文档全局总高度（pt），供前台设置滚动区域高度。
/// 返回 JSON `{"width":W,"height":H}`（pt），失败返回 null。
#[no_mangle]
pub extern "C" fn roe_get_writer_document_size(handle: u64) -> *mut c_char {
    use std::ffi::CString;
    if !self::handle::ensure_writer_layout(handle) {
        set_last_error("roe_get_writer_document_size: 无效句柄或非 Writer 文档");
        return std::ptr::null_mut();
    }
    // 逻辑画布常量
    const CANVAS_TOP_PT: f32 = 24.0;
    const CANVAS_BOTTOM_PT: f32 = 48.0;

    let result = self::handle::with_entry(handle, |entry| {
        let canvas_ruler_pt = if entry.writer_options.ruler_enabled {
            entry.writer_options.ruler_height_pt
        } else { 0.0 };
        let cache = entry.writer_layout_cache.as_ref()?;
        let arena = &cache.arena;
        let width = arena.page_order.first()
            .map(|&pid| arena.pages[pid].width)
            .unwrap_or(595.0);
        let doc_height = arena.total_height();
        let canvas_height = canvas_ruler_pt + CANVAS_TOP_PT + doc_height + CANVAS_BOTTOM_PT;
        let page_count = arena.page_order.len();

        // 构建 pages 数组：每页在逻辑画布中的 y 坐标
        let mut pages_json = String::new();
        for (i, &pid) in arena.page_order.iter().enumerate() {
            let page = &arena.pages[pid];
            let doc_y = arena.page_global_y(pid);
            let canvas_y = canvas_ruler_pt + CANVAS_TOP_PT + doc_y;
            if i > 0 { pages_json.push(','); }
            pages_json.push_str(&format!(
                r#"{{"y":{y:.2},"width":{w:.2},"height":{h:.2}}}"#,
                y = canvas_y, w = page.width, h = page.height
            ));
        }

        Some(format!(
            r#"{{"width":{w:.2},"canvas_height":{ch:.2},"page_count":{pc},"pages":[{pages}]}}"#,
            w = width, ch = canvas_height, pc = page_count, pages = pages_json
        ))
    });
    match result.flatten() {
        Some(json) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        None => {
            set_last_error("roe_get_writer_document_size: 失败");
            std::ptr::null_mut()
        }
    }
}

/// Writer 视口渲染。
/// vp_x/vp_y/vp_w/vp_h：视口在文档全局坐标系中的位置和尺寸（pt）。
/// pixel_w/pixel_h：输出 bitmap 尺寸（px）。
/// buf/buf_len：外部提供的 RGBA buffer。
/// 返回 0 成功，非 0 失败（调用 roe_last_error_message 获取详情）。
#[no_mangle]
pub extern "C" fn roe_render_writer_viewport(
    handle: u64,
    vp_x: f32, vp_y: f32, vp_w: f32, vp_h: f32,
    pixel_w: u32, pixel_h: u32,
    buf: *mut u8, buf_len: usize,
) -> i32 {
    if buf.is_null() {
        set_last_error("roe_render_writer_viewport: buf is null");
        return RoeErrorCode::NullPointer as i32;
    }
    if pixel_w == 0 || pixel_h == 0 || vp_w <= 0.0 || vp_h <= 0.0 {
        set_last_error("roe_render_writer_viewport: invalid dimensions");
        return RoeErrorCode::RenderError as i32;
    }
    if !self::handle::ensure_writer_layout(handle) {
        set_last_error("roe_render_writer_viewport: layout cache build failed");
        return RoeErrorCode::RenderError as i32;
    }
    let buf_slice = unsafe { slice::from_raw_parts_mut(buf, buf_len) };
    let result = self::handle::with_entry(handle, |entry| {
        let RoeDocument::Writer(doc) = &entry.doc else {
            set_last_error("roe_render_writer_viewport: 非 Writer 文档");
            return RoeErrorCode::Unsupported as i32;
        };
        let cache = match entry.writer_layout_cache.as_ref() {
            Some(c) => c,
            None => {
                set_last_error("roe_render_writer_viewport: layout cache missing");
                return RoeErrorCode::RenderError as i32;
            }
        };
        // 逻辑画布常量（与 roe_get_writer_document_size 保持一致）
        const CANVAS_TOP_PT: f32 = 24.0;
        let canvas_ruler_pt = if entry.writer_options.ruler_enabled {
            entry.writer_options.ruler_height_pt
        } else { 0.0 };
        let canvas_offset = canvas_ruler_pt + CANVAS_TOP_PT;

        // 将逻辑画布坐标转换为文档内部坐标
        // vp_y=0 对应画布顶部（灰色留白上边缘），vp_y=canvas_offset 对应第一页顶部
        let doc_vp_y = vp_y - canvas_offset;

        // Parley 迁移（v1方案类型A）：WriterTileRenderer 渲染路径已改用
        // parley::FontContext，取代原 cosmic_text::SwashCache 的角色。
        // writer_font_registry（FontRegistry.font_system）仍是 cosmic-text，
        // 服务于排版测量（ParagraphLayoutEngine），与渲染路径的字体资源
        // 类型不同，本轮迁移不涉及，继续按原方式复用/写回。
        // [FIX 2026-09-14] 此前 fallback 到 parley::FontContext::new()（完全空
        // 白、未加载KaTeX字体），导致首帧（及任何 writer_parley_font_context
        // 尚未建立时）渲染的公式静默 fallback 到系统字体，出现挤压变形、数字
        // 错位、Σ字号过小等回归现象，且不会报错，长期未被发现。
        // 现在统一走 FontRegistry::build_font_context_with_katex()，与
        // render_one.rs等已验证正确的调用路径保持一致，不再各写各的。
        let font_context = entry.writer_parley_font_context.take()
            .unwrap_or_else(FontRegistry::build_font_context_with_katex);
        roe_writer::tile::set_show_marks(entry.writer_options.show_marks);
        let mut renderer = WriterTileRenderer::new_with_fonts(font_context, ());
        let code = match renderer.render_viewport(
            &cache.arena, doc,
            vp_x, doc_vp_y, vp_w, vp_h,
            pixel_w, pixel_h, buf_slice,
        ) {
            Ok(_) => RoeErrorCode::Ok as i32,
            Err(e) => {
                set_last_error(format!("roe_render_writer_viewport: {e}"));
                RoeErrorCode::RenderError as i32
            }
        };
        // 批注气泡功能第一阶段：把本次渲染收集到的批注命中数据存入
        // entry，供 roe_take_comment_hits 导出函数读取。语义见
        // DocumentEntry.pending_comment_hits 字段注释：只覆盖本次
        // viewport 可见范围，不是全文档批注坐标。
        entry.pending_comment_hits = renderer.take_comment_hits();
        let (font_context, _) = renderer.take_fonts();
        entry.writer_parley_font_context = Some(font_context);
        code
    });
    result.unwrap_or_else(|| {
        set_last_error("roe_render_writer_viewport: 无效句柄");
        RoeErrorCode::InvalidHandle as i32
    })
}

#[cfg(test)]
mod viewport_tests {
    use super::*;
    use crate::handle::WriterOptions;
    use std::collections::{HashMap, VecDeque};
    use roe_model::calc::{SparseGrid, Sheet, Workbook, CellValue};
    use crate::handle::DocumentEntry;

    fn make_calc_doc_with_data() -> RoeDocument {
        let mut grid = SparseGrid::default();
        // 填一些测试数据
        for r in 0..10u32 {
            for c in 0..5u32 {
                grid.set(r, c, CellValue::Text(format!("R{}C{}", r+1, c+1)));
            }
        }
        RoeDocument::Calc(Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                name: "Sheet1".into(),
                cells: grid,
                col_widths: vec![],
                row_heights: vec![],
                frozen_rows: 0,
                frozen_cols: 0,
                cell_formats: HashMap::new(),
                cond_formats: vec![],
                data_validations: vec![],
                auto_filter: None,
                merged_cells: vec![],
                pivot: None,
                ..Default::default()
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        })
    }

    fn open(doc: RoeDocument) -> u64 {
        crate::handle::insert(DocumentEntry {
            doc,
            snapshot: None,
            is_shadow: false,
            events: VecDeque::new(),
            animation: HashMap::new(),
            writer_layout_cache: None,
            writer_font_registry: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            writer_options: WriterOptions::default(),
            impress_font_registry: None,
            writer_parley_font_context: None,
            calc_engine: None,
            pending_comment_hits: Vec::new(),
        })
    }

    #[test]
    fn test_render_sheet_viewport_basic() {
        let h = open(make_calc_doc_with_data());
        let vw = 800u32;
        let vh = 600u32;
        let mut buf = vec![0u8; (vw * vh * 4) as usize];

        let code = roe_render_sheet_viewport(
            h, 0,
            0.0, 0.0,   // scroll
            vw, vh,
            100,        // zoom 100%
            30.0, 20.0, // 行号栏宽30pt，列字母栏高20pt
            buf.as_mut_ptr(), buf.len(),
        );
        assert_eq!(code, 0, "viewport render should succeed");

        let all_white = buf.chunks(4).all(|p| p[0]==0xff && p[1]==0xff && p[2]==0xff);
        assert!(!all_white, "viewport should have non-white pixels");

        // 输出 PNG 供肉眼检查
        if let Some(pixmap) = tiny_skia::PixmapMut::from_bytes(&mut buf, vw, vh) {
            // 重新分配 owned pixmap
        }
        let pixmap = tiny_skia::Pixmap::from_vec(
            buf,
            tiny_skia::IntSize::from_wh(vw, vh).unwrap(),
        ).unwrap();
        let png = pixmap.encode_png().unwrap();
        std::fs::write("/tmp/roe_viewport_test.png", &png).ok();
        println!("PNG written: /tmp/roe_viewport_test.png ({} bytes)", png.len());

        crate::handle::remove(h);
    }
}
/// 设置 Writer 文档显示选项。
/// json 格式：`{"ruler":true,"ruler_height_pt":20.0}`
/// 所有字段均为可选，未提供的字段保持原值。
/// 返回 0 成功，非 0 失败。
#[no_mangle]
pub extern "C" fn roe_set_writer_options(handle: u64, json: *const c_char) -> i32 {
    use std::ffi::CStr;
    let json_str = if json.is_null() {
        set_last_error("roe_set_writer_options: json is null");
        return RoeErrorCode::NullPointer as i32;
    } else {
        match unsafe { CStr::from_ptr(json) }.to_str() {
            Ok(s) => s.to_owned(),
            Err(_) => {
                set_last_error("roe_set_writer_options: invalid utf8");
                return RoeErrorCode::ParseError as i32;
            }
        }
    };

    let result = self::handle::with_entry(handle, |entry| {
        // 解析 JSON，逐字段更新（未提供的字段保持原值）
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&json_str) {
            if let Some(b) = val.get("ruler").and_then(|v| v.as_bool()) {
                entry.writer_options.ruler_enabled = b;
            }
            if let Some(h) = val.get("ruler_height_pt").and_then(|v| v.as_f64()) {
                entry.writer_options.ruler_height_pt = h as f32;
            }
            if let Some(b) = val.get("show_marks").and_then(|v| v.as_bool()) {
                entry.writer_options.show_marks = b;
            }
        } else {
            set_last_error("roe_set_writer_options: json parse error");
            return RoeErrorCode::ParseError as i32;
        }
        RoeErrorCode::Ok as i32
    });

    result.unwrap_or_else(|| {
        set_last_error("roe_set_writer_options: 无效句柄");
        RoeErrorCode::InvalidHandle as i32
    })
}


// ─────────────────────────────────────────────────────────────────────────────
// Impress 虚拟视口
// ─────────────────────────────────────────────────────────────────────────────

/// 查询 Impress 文档全局画布尺寸与各幻灯片位置。
/// 返回 JSON 字符串（需调用 roe_free_string 释放）：
/// {"width":..., "canvas_height":..., "slide_count":...,
///  "slides":[{"y":...,"width":...,"height":...}, ...]}
/// 失败返回 null，调用 roe_last_error_message 获取详情。
#[no_mangle]
pub extern "C" fn roe_get_impress_document_size(handle: u64) -> *mut c_char {
    use std::ffi::CString;
    // 幻灯片间距（pt）
    const SLIDE_GAP_PT: f32 = 24.0;
    const CANVAS_PAD_PT: f32 = 24.0;

    let result = self::handle::with_entry(handle, |entry| {
        let presentation = match &entry.doc {
            RoeDocument::Impress(p) => p,
            _ => {
                set_last_error("roe_get_impress_document_size: 非 Impress 文档");
                return None;
            }
        };
        let (slide_w, slide_h) = presentation.slide_size;
        let slide_count = presentation.slides.len();

        let canvas_width = slide_w + CANVAS_PAD_PT * 2.0;
        let mut slides_json = String::new();
        let mut y = CANVAS_PAD_PT;
        for i in 0..slide_count {
            if i > 0 { slides_json.push(','); }
            slides_json.push_str(&format!(
                r#"{{"y":{y:.2},"width":{w:.2},"height":{h:.2}}}"#,
                y = y, w = slide_w, h = slide_h
            ));
            y += slide_h + SLIDE_GAP_PT;
        }
        // 最后一张之后不加间距，只加底部留白
        let canvas_height = if slide_count > 0 {
            y - SLIDE_GAP_PT + CANVAS_PAD_PT
        } else {
            CANVAS_PAD_PT * 2.0
        };

        Some(format!(
            r#"{{"width":{w:.2},"canvas_height":{ch:.2},"slide_count":{sc},"slides":[{slides}]}}"#,
            w = canvas_width, ch = canvas_height, sc = slide_count, slides = slides_json
        ))
    });

    match result.flatten() {
        Some(json) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        None => std::ptr::null_mut(),
    }
}

/// Impress 视口渲染。
/// vp_x/vp_y/vp_w/vp_h：视口在全局画布坐标系中的位置和尺寸（pt）。
/// pixel_w/pixel_h：输出 bitmap 尺寸（px）。
/// buf/buf_len：外部提供的 RGBA buffer（pixel_w * pixel_h * 4 字节）。
/// 返回 0 成功，非 0 失败。
#[no_mangle]
pub extern "C" fn roe_render_impress_viewport(
    handle: u64,
    vp_x: f32, vp_y: f32, vp_w: f32, vp_h: f32,
    pixel_w: u32, pixel_h: u32,
    buf: *mut u8, buf_len: usize,
) -> i32 {
    if buf.is_null() {
        set_last_error("roe_render_impress_viewport: buf is null");
        return RoeErrorCode::NullPointer as i32;
    }
    if pixel_w == 0 || pixel_h == 0 || vp_w <= 0.0 || vp_h <= 0.0 {
        set_last_error("roe_render_impress_viewport: invalid dimensions");
        return RoeErrorCode::RenderError as i32;
    }

    const SLIDE_GAP_PT: f32 = 24.0;
    const CANVAS_PAD_PT: f32 = 24.0;

    let buf_slice = unsafe { std::slice::from_raw_parts_mut(buf, buf_len) };

    // 先把 buf 清为深灰背景色（画布背景）
    for chunk in buf_slice.chunks_mut(4) {
        chunk[0] = 0x40; chunk[1] = 0x40; chunk[2] = 0x40; chunk[3] = 0xff;
    }

    let result = self::handle::with_entry(handle, |entry| {
        let presentation = match &entry.doc {
            RoeDocument::Impress(p) => p,
            _ => {
                set_last_error("roe_render_impress_viewport: 非 Impress 文档");
                return RoeErrorCode::Unsupported as i32;
            }
        };

        let (slide_w, slide_h) = presentation.slide_size;
        let scale = pixel_w as f32 / vp_w;  // pt → px 缩放比

        // 幻灯片在画布中的 x 偏移。
        // canvas_width 与 roe_get_impress_document_size 保持一致的权威定义
        // （完整画布 = slide 本身 + 左右各 CANVAS_PAD_PT 留白）。
        //
        // 调用方目前有两种视口语义，靠 vp_w 与 slide_w/canvas_width 的相对关系区分：
        // 1) 「单 slide 直出」模式（当前 slint-shell 翻页查看用的就是这种，
        //    见 office-rs2 crates/slint-shell/src/main.rs
        //    on_impress_render_viewport：vp_w 直接传 slide_w 本身，vp_x=0，
        //    调用方期望视口内 [0, slide_w] 恰好就是完整 slide，不需要任何
        //    画布留白）：此时 slide 不该被推给 CANVAS_PAD_PT 偏移，否则会
        //    超出调用方给定的视口右边界，导致 slide 内容被裁、左侧露出
        //    画布底色形成灰边（本次修复的问题）。
        // 2) 「完整画布」模式（vp_w 明显大于 slide_w，说明调用方传入的是
        //    canvas 坐标系下的视口，可能覆盖多张 slide 或 slide+留白）：
        //    保留原有 padding/居中语义，向后兼容。
        let canvas_width = slide_w + CANVAS_PAD_PT * 2.0;
        let slide_canvas_x = if vp_w > canvas_width {
            // 视口比完整画布还宽：真正居中
            (vp_w - slide_w) / 2.0
        } else if vp_w <= slide_w + 0.5 {
            // 单 slide 直出模式：视口宽度约等于（或小于）slide 本身宽度，
            // 说明调用方没有把画布留白算进视口，slide 应贴视口左边界，
            // 不叠加 CANVAS_PAD_PT，否则会把 slide 推出视口范围。
            vp_x
        } else {
            // 视口宽度介于 slide_w 和 canvas_width 之间：保留原有左侧
            // padding 语义，向后兼容。
            CANVAS_PAD_PT
        };

        let slide_count = presentation.slides.len();
        for i in 0..slide_count {
            // 当前幻灯片在画布中的 y 坐标
            let slide_y = CANVAS_PAD_PT + i as f32 * (slide_h + SLIDE_GAP_PT);
            let slide_x = slide_canvas_x;

            // 与视口不相交则跳过
            if slide_x + slide_w <= vp_x { continue; }
            if slide_x >= vp_x + vp_w    { continue; }
            if slide_y + slide_h <= vp_y { continue; }
            if slide_y >= vp_y + vp_h    { continue; }

            // 幻灯片在 pixel buffer 中的起点（px）
            let dst_px_x = ((slide_x - vp_x) * scale) as i32;
            let dst_px_y = ((slide_y - vp_y) * scale) as i32;

            let pw = (slide_w * scale) as u32;
            let ph = (slide_h * scale) as u32;
            if pw == 0 || ph == 0 { continue; }

            // 分配临时 slide buffer
            let mut slide_buf = vec![0u8; (pw * ph * 4) as usize];

            let slide = &presentation.slides[i];
            let runtime_ref = entry.animation.get(&i).map(|s| &s.runtime);
            let font_context = entry.impress_font_registry.take()
                .unwrap_or_else(parley::FontContext::new);
            let mut renderer = SlideRenderer::new_with_fonts(slide_w, slide_h, font_context);
            let slide_result = renderer.render_slide(
                slide,
                &presentation.styles,
                runtime_ref,
                scale,
                &mut slide_buf,
                &presentation.theme_colors,
                Some(&presentation.master_slides),
            );
            let font_context = renderer.take_fonts();
            entry.impress_font_registry = Some(font_context);
            match slide_result {
                Ok(_) => {}
                Err(e) => {
                    set_last_error(format!("roe_render_impress_viewport: slide {i}: {e}"));
                    return RoeErrorCode::RenderError as i32;
                }
            }

            // 将 slide_buf blit 到 buf_slice，处理边界裁剪
            let dst_stride = pixel_w as i32;
            let src_stride = pw as i32;
            for row in 0..ph as i32 {
                let dst_row = dst_px_y + row;
                if dst_row < 0 || dst_row >= pixel_h as i32 { continue; }
                for col in 0..pw as i32 {
                    let dst_col = dst_px_x + col;
                    if dst_col < 0 || dst_col >= pixel_w as i32 { continue; }
                    let src_off = ((row * src_stride + col) * 4) as usize;
                    let dst_off = ((dst_row * dst_stride + dst_col) * 4) as usize;
                    if src_off + 3 < slide_buf.len() && dst_off + 3 < buf_slice.len() {
                        buf_slice[dst_off..dst_off+4].copy_from_slice(&slide_buf[src_off..src_off+4]);
                    }
                }
            }
        }

        RoeErrorCode::Ok as i32
    });

    result.unwrap_or_else(|| {
        set_last_error("roe_render_impress_viewport: 无效句柄");
        RoeErrorCode::InvalidHandle as i32
    })
}
