//! roe_cursor_move_line —— 跨行移动光标（路径2，见
//! roe_cursor_selection_final_v1.md 3.3）。
//!
//! ideal_x 归属：前端持有并传入，roe 不存储（final 文档五.2，已拍板，
//! 不引入任何形式的会话状态）。本函数是纯函数：每次调用都独立完成
//! "定位当前行 -> 算目标行 y -> 命中目标行 -> 用 ideal_x 求行内偏移"，
//! 不依赖任何跨调用状态。
//!
//! 跨页处理：不需要特殊逻辑。frame.y_on_page 实际上是全局坐标（含
//! page_content_global_y 的跨页累加，字段名有历史遗留但语义是全局的，
//! 已核实 layout.rs 所有赋值点），locate_frame_by_y_global 天然能在
//! 跨页场景下命中正确的 frame，Up/Down 移动到相邻页第一行不需要额外
//! 换算。
//!
//! 目标行 y 的计算：取当前行的垂直中点，按 direction 整体偏移"当前行
//! line_height"的距离，用这个新 y 去命中目标行。用当前行高做步进（而
//! 不是固定像素）是为了在相邻行字号不同的情况下也能稳定跨到下一行，
//! 不会因为步进太小卡在同一行，也不会因步进太大跳过一行。

use std::ffi::CString;
use std::os::raw::c_char;

use roe_core::arena::FrameId;
use roe_model::writer::BodyElement;
use roe_parser::RoeDocument;
use roe_writer::layout::resolve_para_style;
use roe_writer::line_shape::{char_offset_to_x, x_to_char_offset};

use crate::cursor::RoeLocation;
use crate::error::set_last_error;
use crate::handle;
use crate::hit_test::{locate_frame_by_y_global, locate_line_by_y};

/// 跨行移动方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum CursorLineDirection {
    Up = 0,
    Down = 1,
}

impl CursorLineDirection {
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::Up),
            1 => Some(Self::Down),
            _ => None,
        }
    }
}

/// 跨行移动光标（含 ideal_x 处理）。
///
/// 参数：
/// - handle：文档句柄
/// - container_type/container_id/local_para_index/char_offset：当前
///   RoeLocation（拆解裸参数，语义同 cursor.rs FFI 约定）
/// - direction：CursorLineDirection 的 u32 编码
/// - ideal_x：前端持有并传入的期望水平位置（pt）。连续按上/下键时应
///   保持不变，由前端负责维护（roe 不存储，见本文件顶部注释）
///
/// 返回：JSON 字符串
/// `{"new_loc":{...RoeLocation...},"actual_x":F}`
/// actual_x 是本次实际落点的 x 坐标（pt），供前端绘制光标用，前端不应
/// 用它覆盖自己保存的 ideal_x（final 文档五.2 明确指出这是常见实现
/// 错误）。
///
/// 若已经在文档第一行按 Up、或最后一行按 Down，返回值与输入位置相同
/// （不移动，不报错——这是标准编辑器行为，光标停在原地）。
///
/// 失败返回 null，调用 roe_last_error_message 获取详情。
/// 调用方必须用 roe_free_string 释放返回值。
///
/// 本次仅支持 container_type=BODY；非 BODY 返回 null + last_error。
#[no_mangle]
pub extern "C" fn roe_cursor_move_line(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    char_offset: u32,
    direction: u32,
    ideal_x: f32,
) -> *mut c_char {
    let loc = RoeLocation { container_type, container_id, local_para_index, char_offset };

    if !loc.is_body() {
        set_last_error("roe_cursor_move_line: 本次仅支持 container_type=BODY（表格cell/文本框预留未实现）");
        return std::ptr::null_mut();
    }

    let Some(dir) = CursorLineDirection::from_u32(direction) else {
        set_last_error(format!("roe_cursor_move_line: 非法 direction 值 {direction}"));
        return std::ptr::null_mut();
    };

    if !handle::ensure_writer_layout(handle) {
        set_last_error("roe_cursor_move_line: 无效句柄或非 Writer 文档");
        return std::ptr::null_mut();
    }

    let (para_index, char_offset) = loc.as_legacy_body();

    let result = handle::with_entry(handle, |entry| {
        move_line_in_entry(entry, para_index, char_offset, dir, ideal_x)
    });

    match result {
        None => {
            set_last_error("roe_cursor_move_line: 无效句柄");
            std::ptr::null_mut()
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_cursor_move_line: {msg}"));
            std::ptr::null_mut()
        }
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(e) => {
                set_last_error(format!("roe_cursor_move_line: {e}"));
                std::ptr::null_mut()
            }
        },
    }
}

fn move_line_in_entry(
    entry: &mut handle::DocumentEntry,
    para_index: usize,
    char_offset: usize,
    direction: CursorLineDirection,
    ideal_x: f32,
) -> Result<String, String> {
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };
    if doc.body.get(para_index).is_none() {
        return Err(format!("para_index={para_index} 越界或不存在"));
    }

    // ===== 阶段一：定位当前行，算出当前行的全局 y 范围 + 目标 y =====
    let (target_y, at_document_edge_fallback) = {
        let layout = entry
            .writer_layout_cache
            .as_ref()
            .ok_or_else(|| "writer_layout_cache 未建立（内部错误）".to_string())?;
        let arena = &layout.arena;

        let chain = arena.find_chain_by_para_index(para_index);
        if chain.is_empty() {
            return Err(format!("para_index={para_index} 越界或不存在"));
        }

        let mut consumed = 0usize;
        let mut target: Option<(FrameId, usize)> = None;
        for &fid in &chain {
            let frame = &arena.frames[fid];
            let frame_len: usize = frame.text_lines().iter().map(|l| l.text.len()).sum();
            if char_offset < consumed + frame_len || chain.last() == Some(&fid) {
                target = Some((fid, char_offset.saturating_sub(consumed)));
                break;
            }
            consumed += frame_len;
        }
        let (fid, offset_in_frame) = target
            .ok_or_else(|| format!("char_offset={char_offset} 超出段落范围"))?;

        let frame = &arena.frames[fid];
        let mut consumed_in_frame = 0usize;
        let mut current_line_top: Option<f32> = None;
        let mut current_line_height: Option<f32> = None;
        for line in frame.text_lines() {
            let line_len = line.text.len();
            let is_last_line = consumed_in_frame + line_len >= offset_in_frame
                || std::ptr::eq(frame.text_lines().last().unwrap(), line);
            if offset_in_frame < consumed_in_frame + line_len || is_last_line {
                current_line_top = Some(frame.y_on_page + line.y_offset);
                current_line_height = Some(line.line_height);
                break;
            }
            consumed_in_frame += line_len;
        }
        let current_line_top = current_line_top
            .ok_or_else(|| "frame 内无行数据（内部错误）".to_string())?;
        let current_line_height = current_line_height.unwrap();

        let current_line_mid = current_line_top + current_line_height * 0.5;
        let step = current_line_height;
        let ty = match direction {
            CursorLineDirection::Up => current_line_mid - step,
            CursorLineDirection::Down => current_line_mid + step,
        };
        // 目标 y 小于0：说明已经在文档最顶行按 Up，钳制到文档顶部（第一行）。
        // 这里不特殊判断"最后一行按 Down"，交给 locate_frame_by_y_global
        // 自身的钳制语义（y 超出范围时返回最后一个 frame），行为一致。
        (ty.max(0.0), ty < 0.0)
    };

    // ===== 阶段二：用 target_y 命中目标行 =====
    let (new_para_index, new_char_offset_before_line, line_text, line_runs, target_font_size, target_bold, target_italic, line_abs_x_start) = {
        let layout = entry
            .writer_layout_cache
            .as_ref()
            .ok_or_else(|| "writer_layout_cache 未建立（内部错误）".to_string())?;
        let arena = &layout.arena;

        let all_frame_ids: Vec<_> = arena.page_order.iter()
            .flat_map(|&pid| arena.pages[pid].frames.iter().copied())
            .filter(|&fid| !arena.frames[fid].text_lines().is_empty())
            .collect();
        if all_frame_ids.is_empty() {
            return Err("文档没有任何段落".to_string());
        }

        let target_fid = locate_frame_by_y_global(arena, &all_frame_ids, target_y);
        let frame = &arena.frames[target_fid];
        let y_in_frame = (target_y - frame.y_on_page).max(0.0);
        let (line, offset_before_line) = locate_line_by_y(frame, y_in_frame);

        let chain = arena.find_chain_by_para_index(frame.para_index);
        let mut bytes_before = 0usize;
        for &fid in &chain {
            if fid == target_fid { break; }
            let f = &arena.frames[fid];
            bytes_before += f.text_lines().iter().map(|l| l.text.len()).sum::<usize>();
        }

        // V98修复：与 hit_test.rs 的 v95 修复同理——x_to_char_offset/
        // char_offset_to_x 用的是"行内相对坐标"（以本行文字第一个字符为
        // 原点），但 ideal_x 是前端传入的"文档全局坐标"（含 page.margin_left
        // 及段落缩进/列表缩进等）。此前这里直接把 ideal_x 传给
        // x_to_char_offset，数值域不匹配（全局坐标通常有几十pt的
        // margin_left起步，远超行内相对坐标范围），短行场景下会冲出行尾，
        // 系统性地把落点钳到行尾字符——这正是 v97 会话中发现的"上下键后
        // 落到字符上/行尾/污染后续左右键"现象的根因。这里补上换算，与
        // hit_test.rs 完全对称。
        let line_abs_x_start = arena.pages[frame.page].margin_left + line.text_x_offset;

        (
            frame.para_index,
            bytes_before + offset_before_line,
            line.text.clone(),
            line.runs.clone(),
            0.0f32, // 占位，样式在下面单独解析（需要 doc 引用，这里先不重复借用）
            false,
            false,
            line_abs_x_start,
        )
    };
    let _ = (target_font_size, target_bold, target_italic); // 上面占位值不使用，样式重新解析

    // ===== 阶段三：解析目标段落样式，在目标行内用 ideal_x 求行内偏移 =====
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };
    let target_para = match doc.body.get(new_para_index) {
        Some(BodyElement::Paragraph(p)) => p,
        _ => return Err(format!("目标 para_index={new_para_index} 对应的段落不存在（内部错误）")),
    };
    let default_font_size = entry
        .writer_font_registry
        .as_ref()
        .map(|r| r.default_font_size)
        .ok_or_else(|| "writer_font_registry 未建立（内部错误）".to_string())?;
    let resolved = resolve_para_style(doc, target_para, default_font_size);

    let registry = entry
        .writer_font_registry
        .as_mut()
        .ok_or_else(|| "writer_font_registry 未建立（内部错误）".to_string())?;

    // V98修复：ideal_x（全局坐标）-> 行内相对坐标，再查字符位置。
    let ideal_x_in_line = (ideal_x - line_abs_x_start).max(0.0);
    let offset_in_line = x_to_char_offset(
        registry, &line_text, &line_runs,
        resolved.font_size, resolved.bold, resolved.italic,
        ideal_x_in_line,
    );
    let actual_x_in_line = char_offset_to_x(
        registry, &line_text, &line_runs,
        resolved.font_size, resolved.bold, resolved.italic,
        offset_in_line,
    );
    // actual_x 返回给前端用于绘制光标/下次可能被存为 ideal_x，前端预期的是
    // 全局坐标（与 writer_query_cursor_rect/点击流程里 cx 的坐标系一致），
    // 换回全局坐标再返回。
    let actual_x = actual_x_in_line + line_abs_x_start;

    let new_char_offset = new_char_offset_before_line + offset_in_line;
    let _ = at_document_edge_fallback; // 钳制由 locate_frame_by_y_global 内部语义保证，此变量仅供未来调试参考

    Ok(format!(
        "{{\"new_loc\":{{\"container_type\":0,\"container_id\":0,\"local_para_index\":{},\"char_offset\":{}}},\"actual_x\":{:.2}}}",
        new_para_index, new_char_offset, actual_x
    ))
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::WriterOptions;
    use roe_parser::odf::fodt::FodtParser;
    use std::collections::{HashMap, VecDeque};

    // 两个独立短段落（不依赖自动换行是否发生，用"跨段落"验证 Down/Up
    // 能正确从一个段落移动到相邻段落——这是路径2最容易出错的场景，
    // 比单段落内部多行换行更能暴露 chain/para_index 换算的 bug）。
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
      <text:p text:style-name="Default">First line here</text:p>
      <text:p text:style-name="Default">Second line here</text:p>
      <text:p text:style-name="Default">Third line here</text:p>
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

    fn parse_field(json: &str, field: &str) -> String {
        let key = format!("\"{field}\":");
        let start = json.find(&key).unwrap_or_else(|| panic!("json 应含字段 {field}，实际 {json}")) + key.len();
        let rest = &json[start..];
        let end = rest.find(|c: char| c == ',' || c == '}').unwrap_or(rest.len());
        rest[..end].trim().to_string()
    }

    #[test]
    fn test_move_down_crosses_to_next_paragraph() {
        let h = open_writer();
        // 段落0 "First line here" offset=0，向下移动应该落到段落1。
        let ptr = roe_cursor_move_line(h, 0, 0, 0, 0, CursorLineDirection::Down as u32, 0.0);
        assert!(!ptr.is_null(), "Down 移动应该成功");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let new_para: usize = parse_field(&s, "local_para_index").parse().unwrap();
        assert_eq!(new_para, 1, "从段落0向下移动应该落到段落1，实际 json={s}");
        handle::remove(h);
    }

    #[test]
    fn test_move_up_crosses_to_previous_paragraph() {
        let h = open_writer();
        // 段落1 "Second line here" offset=0，向上移动应该落到段落0。
        let ptr = roe_cursor_move_line(h, 0, 0, 1, 0, CursorLineDirection::Up as u32, 0.0);
        assert!(!ptr.is_null(), "Up 移动应该成功");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let new_para: usize = parse_field(&s, "local_para_index").parse().unwrap();
        assert_eq!(new_para, 0, "从段落1向上移动应该落到段落0，实际 json={s}");
        handle::remove(h);
    }

    #[test]
    fn test_move_up_at_first_paragraph_clamps() {
        let h = open_writer();
        // 已在第一段，再向上移动应该钳制（仍停留在段落0，不报错、不越界到负数）。
        let ptr = roe_cursor_move_line(h, 0, 0, 0, 0, CursorLineDirection::Up as u32, 0.0);
        assert!(!ptr.is_null(), "文档顶部 Up 不应该失败");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let new_para: usize = parse_field(&s, "local_para_index").parse().unwrap();
        assert_eq!(new_para, 0, "文档顶部再向上移动应该钳制在段落0，实际 json={s}");
        handle::remove(h);
    }

    #[test]
    fn test_move_down_at_last_paragraph_clamps() {
        let h = open_writer();
        // 已在最后一段（段落2），再向下移动应该钳制在段落2。
        let ptr = roe_cursor_move_line(h, 0, 0, 2, 0, CursorLineDirection::Down as u32, 0.0);
        assert!(!ptr.is_null(), "文档底部 Down 不应该失败");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let new_para: usize = parse_field(&s, "local_para_index").parse().unwrap();
        assert_eq!(new_para, 2, "文档底部再向下移动应该钳制在段落2，实际 json={s}");
        handle::remove(h);
    }

    #[test]
    fn test_actual_x_present_and_non_negative() {
        let h = open_writer();
        let ptr = roe_cursor_move_line(h, 0, 0, 0, 0, CursorLineDirection::Down as u32, 20.0);
        assert!(!ptr.is_null());
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let actual_x: f32 = parse_field(&s, "actual_x").parse().unwrap();
        assert!(actual_x >= 0.0, "actual_x 应该是非负数，实际 {actual_x}");
        handle::remove(h);
    }

    #[test]
    fn test_ideal_x_zero_lands_near_line_start() {
        let h = open_writer();
        // ideal_x=0.0 向下移动到段落1，应该落在该行接近开头的位置
        // （char_offset 应该很小，不是行尾）。
        let ptr = roe_cursor_move_line(h, 0, 0, 0, 0, CursorLineDirection::Down as u32, 0.0);
        assert!(!ptr.is_null());
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset: usize = parse_field(&s, "char_offset").parse().unwrap();
        assert!(offset <= 2, "ideal_x=0 应该落在行首附近，实际 offset={offset}, json={s}");
        handle::remove(h);
    }

    #[test]
    fn test_non_body_container_type_rejected() {
        let h = open_writer();
        let ptr = roe_cursor_move_line(h, 1, 0, 0, 0, CursorLineDirection::Down as u32, 0.0);
        assert!(ptr.is_null(), "非 BODY container_type 应该返回 null");
        handle::remove(h);
    }

    #[test]
    fn test_invalid_direction_rejected() {
        let h = open_writer();
        let ptr = roe_cursor_move_line(h, 0, 0, 0, 0, 999, 0.0);
        assert!(ptr.is_null(), "非法 direction 值应该返回 null");
        handle::remove(h);
    }

    #[test]
    fn test_invalid_handle_returns_null() {
        let ptr = roe_cursor_move_line(999_999, 0, 0, 0, 0, CursorLineDirection::Down as u32, 0.0);
        assert!(ptr.is_null(), "无效句柄应该返回 null");
    }
}
