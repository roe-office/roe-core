//! roe_hit_test —— 给定页面坐标 (page_idx, x, y)，反查命中的文档元素
//! （Writer 场景：段落索引 + 字符偏移）。
//!
//! 与 roe_query_layout_rect（layout_query.rs）是对称的反向操作：
//! - roe_query_layout_rect：(para_index, char_offset) -> (x, y, w, h)
//! - roe_hit_test：           (page_idx, x, y) -> (para_index, char_offset)
//!
//! x 坐标定位字符已升级为"方案B：重新 shape 拿真实 glyph x 偏移"
//! （roe_writer::line_shape::x_to_char_offset），与 layout_query.rs 对称
//! 复用同一套 shape 逻辑和样式解析（resolve_para_style）。
//!
//! 边界处理：点击位置落在内容区域之外（y 超出所有 frame 范围、x 超出
//! 行宽度）时钳制到最近的合法位置，不报错——这是鼠标点击场景下标准的
//! UX 预期（点在页面留白处也应该有一个合理的光标落点，不能直接报错）。
//!
//! ⚠ 仅支持 Writer 文档（para_index/char_offset 是 Writer 专属概念）。
//! Calc 文档的几何命中查询请用 roe_calc_hit_test（见 calc_geometry.rs），
//! 参数和返回结构均不同（sheet_idx/doc_x/doc_y → row/col），不是同一套接口。

use std::ffi::CString;
use std::os::raw::c_char;

use roe_model::writer::BodyElement;
use roe_parser::RoeDocument;
use roe_writer::frame::{FrameArena, LineBox, ParagraphFrame};
use roe_writer::layout::resolve_para_style;
use roe_writer::line_shape::x_to_char_offset;

use crate::error::set_last_error;
use crate::handle;

/// 给定全局坐标，反查命中的文档元素（段落索引 + 字符偏移）。
///
/// 参数：
/// - handle：文档句柄
/// - x, y：文档全局坐标系下的坐标（单位 pt，与 render_writer_viewport 坐标系一致）
///
/// 返回：JSON 字符串 `{"para_index":N,"char_offset":M}`。
/// 失败返回 null，调用 roe_last_error_message 获取详情。
/// 调用方必须用 roe_free_string 释放返回值。
///
/// 仅支持 Writer 文档；非 Writer 文档、无效句柄均返回 null + last_error，不会 panic。
/// 坐标落在内容区域之外会被钳制到最近合法位置，不算错误。
#[no_mangle]
pub extern "C" fn roe_hit_test(handle: u64, x: f64, y: f64) -> *mut c_char {
    let x = x as f32;
    let y = y as f32;

    if !handle::ensure_writer_layout(handle) {
        set_last_error("roe_hit_test: 无效句柄或非 Writer 文档");
        return std::ptr::null_mut();
    }

    let result = handle::with_entry(handle, |entry| hit_test_in_entry(entry, x, y));

    match result {
        None => {
            set_last_error("roe_hit_test: 无效句柄");
            std::ptr::null_mut()
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_hit_test: {msg}"));
            std::ptr::null_mut()
        }
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(e) => {
                set_last_error(format!("roe_hit_test: {e}"));
                std::ptr::null_mut()
            }
        },
    }
}

pub(crate) fn hit_test_in_entry(
    entry: &mut handle::DocumentEntry,
    x: f32,
    y: f32,
) -> Result<String, String> {
    // 第一阶段：只借 arena（不可变），算出 target_fid / line 的内容拷贝 /
    // para_index / chain 之前各 frame 的字节长度。这些都不依赖 FontRegistry，
    // 算完后这一阶段的借用就结束，不会跟下面 &mut registry 冲突。
    let (line_text, line_runs, line_ascent, offset_in_frame_before_line, para_index, bytes_before_target_frame, line_abs_x_start) = {
        let RoeDocument::Writer(_doc) = &entry.doc else {
            return Err("非 Writer 文档".to_string());
        };

        let layout = entry
            .writer_layout_cache
            .as_ref()
            .ok_or_else(|| "writer_layout_cache 未建立（内部错误）".to_string())?;
        let arena = &layout.arena;

        // 用全局 y 坐标找到所有 frame 中最近的
        let all_frame_ids: Vec<_> = arena.page_order.iter()
            .flat_map(|&pid| arena.pages[pid].frames.iter().copied())
            .filter(|&fid| !arena.frames[fid].text_lines().is_empty())
            .collect();
        if all_frame_ids.is_empty() {
            return Err("文档没有任何段落".to_string());
        }

        // 用全局 y 坐标定位落在哪个 frame
        let target_fid = locate_frame_by_y_global(arena, &all_frame_ids, y);
        let frame = &arena.frames[target_fid];

        // 在 frame 内用相对 y 坐标定位具体行（y_on_page 现在是全局坐标）。
        let y_in_frame = (y - frame.y_on_page).max(0.0);
        let (line, offset_before_line) = locate_line_by_y(frame, y_in_frame);

        // 把"该 frame 之前（同一 para_index 链上）所有 frame 的字节长度"
        // 累加好，留给第二阶段直接用。
        let chain = arena.find_chain_by_para_index(frame.para_index);
        let mut bytes_before = 0usize;
        for &fid in &chain {
            if fid == target_fid {
                break;
            }
            let f = &arena.frames[fid];
            bytes_before += f.text_lines().iter().map(|l| l.text.len()).sum::<usize>();
        }

        // v95修复：额外算出这一行文字在页面上的绝对起始x坐标（page.margin_left +
        // line.text_x_offset），随其他字段一起带出闭包，供第二阶段做坐标系换算用。
        let line_abs_x_start = arena.pages[frame.page].margin_left + line.text_x_offset;

        (line.text.clone(), line.runs.clone(), line.ascent, offset_before_line, frame.para_index, bytes_before, line_abs_x_start)
    };

    // 第二阶段：解析样式（需要读 entry.doc，不冲突），再借 &mut registry
    // 重新 shape 这一行拿精确字符偏移。
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };
    let para = match doc.body.get(para_index) {
        Some(BodyElement::Paragraph(p)) => p,
        _ => return Err(format!("para_index={para_index} 对应的段落不存在（内部错误）")),
    };
    let default_font_size = entry
        .writer_font_registry
        .as_ref()
        .map(|r| r.default_font_size)
        .ok_or_else(|| "writer_font_registry 未建立（内部错误）".to_string())?;
    let resolved = resolve_para_style(doc, para, default_font_size);
    let _ = line_ascent; // 方案B 不再需要 ascent 近似值，保留变量名便于回溯排查

    let registry = entry
        .writer_font_registry
        .as_mut()
        .ok_or_else(|| "writer_font_registry 未建立（内部错误）".to_string())?;

    // v95修复：shape_line/x_to_char_offset 用的是"行内相对坐标"（以本行文字
    // 第一个字符为原点），但本函数收到的 x 是"文档全局坐标"（含 page.margin_left
    // 及段落缩进/列表缩进等）。此前直接把全局x传给x_to_char_offset，数值域完全
    // 不匹配（全局x通常有几十pt的margin_left起步，远超行内相对坐标范围），导致
    // 命中结果与点击位置无关，只会钳制到行首或行尾附近。这里换算成行内相对坐标。
    let x_in_line = (x - line_abs_x_start).max(0.0);
    let offset_in_line = x_to_char_offset(
        registry,
        &line_text,
        &line_runs,
        resolved.font_size,
        resolved.bold,
        resolved.italic,
        x_in_line,
    );

    let char_offset = bytes_before_target_frame + offset_in_frame_before_line + offset_in_line;

    Ok(format!(
        "{{\"para_index\":{},\"char_offset\":{}}}",
        para_index, char_offset
    ))
}

/// 在所有 frame 中，按全局 y 坐标定位命中的 frame。
/// y 小于第一个 frame 顶部 -> 钳制到第一个；y 超出最后一个 frame 底部 -> 钳制到最后一个。
pub(crate) fn locate_frame_by_y_global(
    arena: &FrameArena,
    frame_ids: &[roe_core::arena::FrameId],
    y: f32,
) -> roe_core::arena::FrameId {
    debug_assert!(!frame_ids.is_empty());

    for &fid in frame_ids {
        let f = &arena.frames[fid];
        let top = f.y_on_page;       // 全局坐标
        let bottom = top + f.height();
        if y < bottom {
            return fid;
        }
        let _ = top;
    }
    *frame_ids.last().unwrap()
}

/// 在给定 frame 内，按相对 frame 顶部的 y 坐标定位行。
/// 返回 (命中的 LineBox 引用, 该行之前所有行的字节长度之和)。
pub(crate) fn locate_line_by_y(frame: &ParagraphFrame, y_in_frame: f32) -> (&LineBox, usize) {
    let mut consumed_height = 0.0f32;
    let mut consumed_bytes = 0usize;

    for line in frame.text_lines() {
        let bottom = consumed_height + line.line_height;
        if y_in_frame < bottom {
            return (line, consumed_bytes);
        }
        consumed_height = bottom;
        consumed_bytes += line.text.len();
    }
    // y 超出所有行，钳制到最后一行
    let last = frame.text_lines().last().expect("frame.text_lines() 不应为空");
    let bytes_before_last: usize = frame.text_lines()[..frame.text_lines().len() - 1]
        .iter()
        .map(|l| l.text.len())
        .sum();
    (last, bytes_before_last)
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
    fn test_hit_test_top_left_hits_first_paragraph() {
        let h = open_writer();
        let ptr = roe_hit_test(h, 0.0, 0.0);
        assert!(!ptr.is_null(), "点击页面左上角应该命中第一段");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        assert!(s.contains("\"para_index\":0"));
        handle::remove(h);
    }

    #[test]
    fn test_hit_test_valid_coords_hits_something() {
        let h = open_writer();
        let ptr = roe_hit_test(h, 0.0, 0.0);
        assert!(!ptr.is_null(), "坐标 (0,0) 应该命中内容");
        unsafe { CString::from_raw(ptr) };
        handle::remove(h);
    }

    #[test]
    fn test_hit_test_invalid_handle_returns_null() {
        let ptr = roe_hit_test(999_999, 0.0, 0.0);
        assert!(ptr.is_null(), "无效句柄应该返回 null");
    }

    #[test]
    fn test_hit_test_y_beyond_page_clamps_to_last_frame_not_panic() {
        let h = open_writer();
        // y 给一个极大值，超出所有内容，应该钳制而不是 panic
        let ptr = roe_hit_test(h, 0.0, 99999.0);
        assert!(!ptr.is_null(), "y 越界应该钳制返回结果，而不是失败");
        handle::remove(h);
    }

    #[test]
    fn test_hit_test_x_beyond_line_clamps_not_panic() {
        let h = open_writer();
        // x 给一个极大值，超出整行宽度，应该钳制而不是 panic
        let ptr = roe_hit_test(h, 99999.0, 0.0);
        assert!(!ptr.is_null(), "x 越界应该钳制返回结果，而不是失败");
        handle::remove(h);
    }

    #[test]
    fn test_hit_test_roundtrip_with_query_layout_rect() {
        // 往返一致性：query_layout_rect(para=0, offset=0) 给出 (x,y)，
        // hit_test 在该 (x,y) 附近应该重新命中 para_index=0。
        use crate::layout_query::roe_query_layout_rect;
        let h = open_writer();

        let rect_ptr = roe_query_layout_rect(h, 0, 0);
        assert!(!rect_ptr.is_null());
        let rect_json = unsafe { CString::from_raw(rect_ptr) }.into_string().unwrap();
        // 粗略解析 y 字段（不引入 json 库依赖，简单字符串定位即可，测试场景够用）
        let y_val: f32 = rect_json
            .split("\"y\":")
            .nth(1)
            .and_then(|s| s.split(',').next())
            .and_then(|s| s.trim_end_matches('}').parse().ok())
            .expect("解析 y 失败");

        let hit_ptr = roe_hit_test(h, 0.0, y_val as f64);
        assert!(!hit_ptr.is_null());
        let hit_json = unsafe { CString::from_raw(hit_ptr) }.into_string().unwrap();
        assert!(hit_json.contains("\"para_index\":0"), "往返测试应命中段落0，实际：{hit_json}");

        handle::remove(h);
    }
}
