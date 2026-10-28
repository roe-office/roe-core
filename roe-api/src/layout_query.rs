//! roe_query_layout_rect —— 给定文档元素定位（段落索引 + 段落内字符偏移），
//! 查询其在页面坐标系下的 bounding rect（page/slide 坐标系，单位 pt）。
//!
//! 用途：office-rs2 光标渲染 / 选区扩展场景的"反向查询"——前端需要知道
//! "当前光标应该画在屏幕的哪个像素位置"。
//!
//! 元素定位方式（B块设计决策，详见交接文档）：para_index（段落在
//! WriterDocument::body 中的索引）+ char_offset（该段落 runs 拼接后的
//! full_text 上的字节偏移，与 LineBox::text 的切片口径一致）。
//!
//! x 坐标计算：已升级为"方案B：重新 shape 拿真实 glyph x 偏移"
//! （roe_writer::line_shape::char_offset_to_x）。原"方案A：avg_char_width
//! 估算"已替换，不再使用。重新 shape 需要该行原始字号/加粗/斜体样式，
//! 通过 roe_writer::layout::resolve_para_style 从段落样式解析得到，
//! 与原始排版使用同一份样式解析逻辑，保证一致性。
//!
//! ⚠ 仅支持 Writer 文档（para_index/char_offset 是 Writer 专属概念）。
//! Calc 文档的布局矩形查询请用 roe_calc_query_layout_rect（见
//! calc_geometry.rs），参数和返回结构均不同（sheet_idx/row/col →
//! x/y/width/height），不是同一套接口。

use std::ffi::CString;
use std::os::raw::c_char;

use roe_core::arena::FrameId;
use roe_model::writer::BodyElement;
use roe_parser::RoeDocument;
use roe_writer::layout::resolve_para_style;
use roe_writer::line_shape::char_offset_to_x;

use crate::error::set_last_error;
use crate::handle;

/// 查询文档元素（段落索引 + 字符偏移）在页面坐标系下的 bounding rect。
///
/// 参数：
/// - handle：文档句柄
/// - para_index：段落在 WriterDocument::body 中的索引
/// - char_offset：段落内字符偏移（字节偏移，对应该段落所有 Run 拼接后的
///   full_text；与 LineBox::text 的切片口径一致）
///
/// 返回：JSON 字符串 `{"page_idx":N,"x":..,"y":..,"w":..,"h":..}`
/// （坐标单位 pt，page_idx 从 0 开始）。
/// 失败返回 null，调用 roe_last_error_message 获取详情。
/// 调用方必须用 roe_free_string 释放返回值。
///
/// 仅支持 Writer 文档；非 Writer 文档、无效句柄、para_index 越界、
/// char_offset 越界均返回 null + last_error，不会 panic。
#[no_mangle]
pub extern "C" fn roe_query_layout_rect(
    handle: u64,
    para_index: u64,
    char_offset: u64,
) -> *mut c_char {
    let para_index = para_index as usize;
    let char_offset = char_offset as usize;

    if !handle::ensure_writer_layout(handle) {
        set_last_error("roe_query_layout_rect: 无效句柄或非 Writer 文档");
        return std::ptr::null_mut();
    }

    let result = handle::with_entry(handle, |entry| {
        query_rect_in_entry(entry, para_index, char_offset)
    });

    match result {
        None => {
            set_last_error("roe_query_layout_rect: 无效句柄");
            std::ptr::null_mut()
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_query_layout_rect: {msg}"));
            std::ptr::null_mut()
        }
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(e) => {
                set_last_error(format!("roe_query_layout_rect: {e}"));
                std::ptr::null_mut()
            }
        },
    }
}

/// 核心查询逻辑，从 DocumentEntry 中已建立的 writer_layout_cache 取数据。
/// 调用前必须先确保 ensure_writer_layout 已成功（writer_layout_cache 和
/// writer_font_registry 都是 Some）。需要 &mut：重新 shape 一行文字要对
/// FontRegistry::font_system 做可变借用。
pub(crate) fn query_rect_in_entry(
    entry: &mut handle::DocumentEntry,
    para_index: usize,
    char_offset: usize,
) -> Result<String, String> {
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };

    let para = match doc.body.get(para_index) {
        Some(BodyElement::Paragraph(p)) => p,
        _ => return Err(format!("para_index={para_index} 越界或不存在")),
    };
    let default_font_size = entry
        .writer_font_registry
        .as_ref()
        .map(|r| r.default_font_size)
        .ok_or_else(|| "writer_font_registry 未建立（内部错误）".to_string())?;
    let resolved = resolve_para_style(doc, para, default_font_size);

    let layout = entry
        .writer_layout_cache
        .as_ref()
        .ok_or_else(|| "writer_layout_cache 未建立（内部错误）".to_string())?;
    let arena = &layout.arena;

    let chain = arena.find_chain_by_para_index(para_index);
    if chain.is_empty() {
        return Err(format!("para_index={para_index} 越界或不存在"));
    }

    // 按链上各 frame 的文字总长度累加，定位 char_offset 落在哪个 frame、
    // 哪一行、行内偏移多少。
    let mut consumed = 0usize; // 已经跨过的字节数（跨 frame 累加）
    let mut target: Option<(FrameId, usize)> = None;
    for &fid in &chain {
        let frame = &arena.frames[fid];
        let frame_len: usize = frame.text_lines().iter().map(|l| l.text.len()).sum();

        if char_offset < consumed + frame_len || is_last_frame(&chain, fid) {
            target = Some((fid, char_offset.saturating_sub(consumed)));
            break;
        }
        consumed += frame_len;
    }

    let (fid, offset_in_frame) = target
        .ok_or_else(|| format!("char_offset={char_offset} 超出段落范围"))?;

    // arena 的借用到这里结束（target 计算完后不再需要 layout/arena 的引用），
    // 接下来需要 &mut entry.writer_font_registry，必须先让上面的不可变借用
    // 释放——通过重新从 entry 取引用的方式重新借用，避免同时存在冲突借用。
    let arena = &entry.writer_layout_cache.as_ref().unwrap().arena;
    let registry = entry
        .writer_font_registry
        .as_mut()
        .ok_or_else(|| "writer_font_registry 未建立（内部错误）".to_string())?;

    locate_in_frame(doc, arena, fid, offset_in_frame, registry, &resolved)
}

fn is_last_frame(chain: &[FrameId], fid: FrameId) -> bool {
    chain.last() == Some(&fid)
}

/// 在单个 frame 内，根据 frame 内字节偏移定位到具体行，并计算 bounding rect。
/// registry / resolved：方案B 重新 shape 所需的 FontRegistry 和该段落的
/// 已解析样式（字号/加粗/斜体），由调用方（query_rect_in_entry）提供。
fn locate_in_frame(
    doc: &roe_model::writer::WriterDocument,
    arena: &roe_writer::frame::FrameArena,
    fid: FrameId,
    offset_in_frame: usize,
    registry: &mut roe_writer::FontRegistry,
    resolved: &roe_writer::layout::ResolvedParaStyle,
) -> Result<String, String> {
    let frame = &arena.frames[fid];

    // WROOT-YOFFSET-01修复：此前这里完全没有考虑 space_before（段前距），
    // 而 tile.rs 的真实渲染路径（约793行 render_writer_viewport 内）
    // 是 cursor_y_pt = frame.y_on_page + space_before 起算每一行的屏幕y，
    // 光标/选区查询这条路径此前只用了 frame.y_on_page（漏了 space_before），
    // 导致任何 space_before>0 的段落（典型如标题样式默认段前距较大），
    // 从这个frame开始，之后同frame内所有行查询出的y坐标相比真实渲染
    // 位置系统性偏小（偏上）恰好 space_before 那么多pt——与tile.rs
    // 保持完全一致的计算方式，此处补上同样的 space_before 查找逻辑。
    let space_before = doc.body.get(frame.para_index).and_then(|e| {
        if let roe_model::writer::BodyElement::Paragraph(p) = e {
            p.space_before.or(
                doc.styles.resolve_paragraph_style(p.style.0.as_str()).space_before
            )
        } else { None }
    }).unwrap_or(0.0);

    let mut consumed = 0usize;
    for line in frame.text_lines() {
        let line_len = line.text.len();
        // 钳制：如果是本 frame 最后一行，即使 offset 超出也归到这一行末尾
        let is_last_line = consumed + line_len >= offset_in_frame
            || line_byte_is_last(frame.text_lines(), line);

        if offset_in_frame < consumed + line_len || is_last_line {
            let offset_in_line = offset_in_frame.saturating_sub(consumed).min(line_len);

            // 方案B：重新 shape 这一行文字，拿真实 glyph x 偏移。
            // char_offset_to_x 返回的是"行内相对坐标"（以本行文字第一个字符为
            // 原点），与 hit_test.rs::x_to_char_offset 用的是同一套坐标系（同一份
            // shape_line 产出的 glyphs）。调用方（前端 writer_query_cursor_rect）
            // 期望拿到的是"文档全局坐标"（与 roe_hit_test 输入、render_writer_viewport
            // 坐标系一致），所以这里要加上这一行在页面上的绝对起始x
            // （page.margin_left + line.text_x_offset）才能返回正确的全局坐标。
            // v95修复：此前直接返回行内相对x，导致前端拿到的光标x坐标系统性缺少
            // margin_left（约几十pt），与hit_test.rs的输入侧坐标系不匹配bug同源。
            let x_in_line = char_offset_to_x(
                registry,
                &line.text,
                &line.runs,
                resolved.font_size,
                resolved.bold,
                resolved.italic,
                offset_in_line,
            );
            let line_abs_x_start = arena.pages[frame.page].margin_left + line.text_x_offset;
            let x = x_in_line + line_abs_x_start;
            eprintln!("[V96-ROE-DEBUG] locate_in_frame: offset_in_frame={} consumed={} line_len={} offset_in_line={} line.text={:?} x_in_line={:.2} margin_left={:.2} line.text_x_offset={:.2} line_abs_x_start={:.2} final_x={:.2}",
                offset_in_frame, consumed, line_len, offset_in_line, line.text, x_in_line,
                arena.pages[frame.page].margin_left, line.text_x_offset, line_abs_x_start, x);

            let page_idx = arena.page_order.iter().position(|&pid| pid == frame.page).unwrap_or(0);
            let y = frame.y_on_page + space_before + consumed_line_height(frame.text_lines(), line);
            // 光标宽度：用字号的一个粗略占位（不是真实字符宽度，光标本身
            // 通常画成固定细线，这里给个跟字号相关的占位值即可）。
            let w = (resolved.font_size * 0.08).max(1.0);
            let h = line.line_height;

            return Ok(format!(
                "{{\"page_idx\":{},\"x\":{:.2},\"y\":{:.2},\"w\":{:.2},\"h\":{:.2}}}",
                page_idx, x, y, w, h
            ));
        }
        consumed += line_len;
    }

    Err("frame 内无行数据（内部错误）".to_string())
}

/// 判断给定的 line 是否是 lines 中最后一行（按地址比较，lines 内不会有
/// 完全重复的 LineBox 指针场景，用 std::ptr::eq 避免依赖 PartialEq）。
fn line_byte_is_last(lines: &[roe_writer::frame::LineBox], target: &roe_writer::frame::LineBox) -> bool {
    match lines.last() {
        Some(last) => std::ptr::eq(last, target),
        None => false,
    }
}

/// 累加 target 之前所有行的 line_height（用于算 y 坐标）。
fn consumed_line_height(lines: &[roe_writer::frame::LineBox], target: &roe_writer::frame::LineBox) -> f32 {
    let mut acc = 0.0f32;
    for line in lines {
        if std::ptr::eq(line, target) {
            break;
        }
        acc += line.line_height;
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::WriterOptions;
    use roe_parser::odf::fodt::FodtParser;
    use std::collections::{HashMap, VecDeque};

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
      <text:p text:style-name="Default">第一段测试文字。</text:p>
      <text:p text:style-name="Default">第二段。</text:p>
    </office:text>
  </office:body>
</office:document>"#;

    fn open_writer() -> u64 {
        let doc = FodtParser::parse(SAMPLE_FODT).expect("parse failed");
        handle::insert(handle::DocumentEntry {
            doc: RoeDocument::Writer(doc),
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
    fn test_query_start_of_paragraph() {
        let h = open_writer();
        let ptr = roe_query_layout_rect(h, 0, 0);
        assert!(!ptr.is_null(), "查询段落0开头应该成功");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        assert!(s.contains("\"page_idx\":0"));
        handle::remove(h);
    }

    #[test]
    fn test_query_second_paragraph() {
        let h = open_writer();
        let ptr = roe_query_layout_rect(h, 1, 0);
        assert!(!ptr.is_null(), "查询段落1开头应该成功");
        handle::remove(h);
    }

    #[test]
    fn test_invalid_para_index_returns_null() {
        let h = open_writer();
        let ptr = roe_query_layout_rect(h, 999, 0);
        assert!(ptr.is_null(), "越界段落索引应该返回 null");
        handle::remove(h);
    }

    #[test]
    fn test_invalid_handle_returns_null() {
        let ptr = roe_query_layout_rect(999_999, 0, 0);
        assert!(ptr.is_null(), "无效句柄应该返回 null");
    }

    #[test]
    fn test_char_offset_beyond_end_clamped_not_panic() {
        let h = open_writer();
        // 段落"第一段测试文字。"远小于 99999 字节，offset 越界应该被钳制到行尾，不能 panic
        let ptr = roe_query_layout_rect(h, 0, 99999);
        assert!(!ptr.is_null(), "越界 offset 应该被钳制返回结果，而不是失败");
        handle::remove(h);
    }
}


