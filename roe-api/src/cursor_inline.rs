//! roe_cursor_move_inline —— 单行内光标移动（路径1，见
//! roe_cursor_selection_final_v1.md 3.2）。
//!
//! 四个命令（CharLeft/CharRight/WordLeft/WordRight）均已实现，共用同一套
//! "从 RoeLocation 定位到 frame+行+行内偏移"逻辑（move_inline_in_entry），
//! 只在最后一步按 command 分派到 Parley Cursor 的对应方法
//! （next_visual/previous_visual/next_visual_word/previous_visual_word）。
//!
//! 定位逻辑（从 RoeLocation 找到具体 LineBox）复用
//! layout_query.rs::query_rect_in_entry 的思路（按链上各 frame 文字长度
//! 累加定位 frame + frame 内偏移，再在 frame 内按行长度累加定位到具体
//! 行 + 行内偏移），因为这条定位路径已经过现有 roe_query_layout_rect
//! 测试验证，直接复用比重新设计一遍风险更低。
//!
//! 与 char_offset_to_x/x_to_char_offset 用的 shape_line 不同，这里用
//! shape_line_full（line_shape.rs 新增，不查/不写 shape_cache），因为
//! 需要真正的 parley::Layout<RunBrush> 供 Cursor 方法调用（原因见
//! line_shape.rs::shape_line_full 顶部注释）。

use std::ffi::CString;
use std::os::raw::c_char;

use parley::Cursor as ParleyCursor;
use parley::Affinity;

use roe_core::arena::FrameId;
use roe_model::writer::BodyElement;
use roe_parser::RoeDocument;
use roe_writer::layout::resolve_para_style;
use roe_writer::line_shape::shape_line_full;

use crate::cursor::{CursorInlineCommand, RoeLocation};
use crate::error::set_last_error;
use crate::handle;

/// 单行内移动光标（字符级/词级，不跨行）。
///
/// 参数：
/// - handle：文档句柄
/// - container_type/container_id/local_para_index/char_offset：拆解后的
///   RoeLocation（见 cursor.rs 顶部 FFI 边界约定）
/// - command：CursorInlineCommand 的 u32 编码
///
/// 返回：JSON 字符串
/// `{"container_type":N,"container_id":N,"local_para_index":N,"char_offset":N}`
/// 即移动后的新 RoeLocation。若移动会越出当前行边界，返回值与输入相同
/// （Parley Cursor::next_visual/previous_visual 在边界处的原生语义就是
/// "原地不动"，不需要额外钳制逻辑）。跨行由前端检测"位置未变化"后自行
/// 处理（见 final 文档五.3）。
///
/// 失败返回 null，调用 roe_last_error_message 获取详情。
/// 调用方必须用 roe_free_string 释放返回值。
///
/// 本次（v94）仅支持 command=CharRight（1）与 container_type=BODY（0）。
/// 其余取值返回 null + last_error（不是最终形态，是分步实现的中间状态，
/// 见本文件顶部注释）。
#[no_mangle]
pub extern "C" fn roe_cursor_move_inline(
    handle: u64,
    container_type: u32,
    container_id: u32,
    local_para_index: u32,
    char_offset: u32,
    command: u32,
) -> *mut c_char {
    let loc = RoeLocation { container_type, container_id, local_para_index, char_offset };

    if !loc.is_body() {
        set_last_error("roe_cursor_move_inline: 本次仅支持 container_type=BODY（表格cell/文本框预留未实现）");
        return std::ptr::null_mut();
    }

    let Some(cmd) = CursorInlineCommand::from_u32(command) else {
        set_last_error(format!("roe_cursor_move_inline: 非法 command 值 {command}"));
        return std::ptr::null_mut();
    };

    if !handle::ensure_writer_layout(handle) {
        set_last_error("roe_cursor_move_inline: 无效句柄或非 Writer 文档");
        return std::ptr::null_mut();
    }

    let (para_index, char_offset) = loc.as_legacy_body();

    let result = handle::with_entry(handle, |entry| {
        move_inline_in_entry(entry, para_index, char_offset, cmd)
    });

    match result {
        None => {
            set_last_error("roe_cursor_move_inline: 无效句柄");
            std::ptr::null_mut()
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_cursor_move_inline: {msg}"));
            std::ptr::null_mut()
        }
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(e) => {
                set_last_error(format!("roe_cursor_move_inline: {e}"));
                std::ptr::null_mut()
            }
        },
    }
}

/// 核心逻辑：定位到 para_index/char_offset 所在的行，在行内按 command
/// 用对应的 Parley Cursor 方法移动一步，返回新的 (para_index,
/// char_offset) 对应的 RoeLocation JSON。
///
/// 定位 frame+行内偏移的逻辑与 layout_query.rs::query_rect_in_entry 的
/// 前半段（127-148行）结构一致，故意保持同构，方便对照排查。
fn move_inline_in_entry(
    entry: &mut handle::DocumentEntry,
    para_index: usize,
    char_offset: usize,
    command: CursorInlineCommand,
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

    let mut consumed = 0usize;
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

    let arena = &entry.writer_layout_cache.as_ref().unwrap().arena;
    let frame = &arena.frames[fid];

    // 在 frame 内定位到具体行 + 行内偏移 + 该行在段落 full_text 中的起点
    // （用于把"行内新偏移"换算回"段落级 char_offset"）。
    let mut consumed_in_frame = 0usize;
    let mut found_line: Option<(&roe_writer::frame::LineBox, usize, usize)> = None; // (line, offset_in_line, line_start_in_para)
    // line_start_in_para = 该行在段落 full_text 中的起点 = 之前所有 frame 消耗的字节 + 本 frame 内之前所有行消耗的字节
    let para_prefix = char_offset - offset_in_frame.min(char_offset).max(0); // 占位，下面重新算，见下方注释
    let _ = para_prefix; // 避免未使用警告；真实计算见下方 line_start_base

    // consumed（上面循环结束时的值）就是"本 frame 之前所有 frame 消耗的字节数"，
    // 与 offset_in_frame 相加即为该 frame 起点在段落 full_text 中的字节偏移。
    let frame_start_in_para = char_offset - offset_in_frame;

    for line in frame.text_lines() {
        let line_len = line.text.len();
        let is_last_line = consumed_in_frame + line_len >= offset_in_frame
            || line_byte_is_last(frame.text_lines(), line);

        if offset_in_frame < consumed_in_frame + line_len || is_last_line {
            let offset_in_line = offset_in_frame.saturating_sub(consumed_in_frame).min(line_len);
            found_line = Some((line, offset_in_line, frame_start_in_para + consumed_in_frame));
            break;
        }
        consumed_in_frame += line_len;
    }

    let (line, offset_in_line, line_start_in_para) = found_line
        .ok_or_else(|| "frame 内无行数据（内部错误）".to_string())?;

    let registry = entry
        .writer_font_registry
        .as_mut()
        .ok_or_else(|| "writer_font_registry 未建立（内部错误）".to_string())?;

    let parley_layout = shape_line_full(
        registry,
        &line.text,
        &line.runs,
        resolved.font_size,
        resolved.bold,
        resolved.italic,
    );

    eprintln!("[LIST-CURSOR-DEBUG] ==== move_inline_in_entry 入参/定位结果 ====");
    eprintln!("[LIST-CURSOR-DEBUG] para_index={} char_offset(入参,段落级)={}", para_index, char_offset);
    eprintln!("[LIST-CURSOR-DEBUG] fid={:?} offset_in_frame={}", fid, offset_in_frame);
    eprintln!("[LIST-CURSOR-DEBUG] line.text={:?} line.text.len()={}", line.text, line.text.len());
    eprintln!("[LIST-CURSOR-DEBUG] line.list_label={:?} line.list_label_x={:.2}", line.list_label, line.list_label_x);
    eprintln!("[LIST-CURSOR-DEBUG] line.x_offset={:.2}", line.x_offset);
    eprintln!("[LIST-CURSOR-DEBUG] offset_in_line(定位到的行内偏移)={}", offset_in_line);
    eprintln!("[LIST-CURSOR-DEBUG] line_start_in_para(该行在段落full_text中的起点)={}", line_start_in_para);

    let cursor = ParleyCursor::from_byte_index(&parley_layout, offset_in_line, Affinity::Downstream);
    let moved = match command {
        CursorInlineCommand::CharRight => cursor.next_visual(&parley_layout),
        CursorInlineCommand::CharLeft => cursor.previous_visual(&parley_layout),
        CursorInlineCommand::WordRight => cursor.next_visual_word(&parley_layout),
        CursorInlineCommand::WordLeft => cursor.previous_visual_word(&parley_layout),
    };
    let new_offset_in_line = moved.index();

    eprintln!("[LIST-CURSOR-DEBUG] command={:?} new_offset_in_line(移动后,行内)={}", command, new_offset_in_line);

    let new_para_char_offset = line_start_in_para + new_offset_in_line;

    eprintln!("[LIST-CURSOR-DEBUG] new_para_char_offset(移动后,段落级)={}", new_para_char_offset);
    eprintln!("[LIST-CURSOR-DEBUG] ==== 本次调用结束 ====");

    Ok(format!(
        "{{\"container_type\":0,\"container_id\":0,\"local_para_index\":{},\"char_offset\":{}}}",
        para_index, new_para_char_offset
    ))
}

fn is_last_frame(chain: &[FrameId], fid: FrameId) -> bool {
    chain.last() == Some(&fid)
}

fn line_byte_is_last(lines: &[roe_writer::frame::LineBox], target: &roe_writer::frame::LineBox) -> bool {
    match lines.last() {
        Some(last) => std::ptr::eq(last, target),
        None => false,
    }
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
      <text:p text:style-name="Default">Hello World</text:p>
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

    fn parse_char_offset(json: &str) -> usize {
        // 简单手写解析，避免为测试引入 serde_json 依赖：从
        // "char_offset":N 提取 N（与本文件其余部分风格一致，
        // hit_test.rs/layout_query.rs 测试里也是用 contains 断言，
        // 这里需要取值所以多写一点解析）。
        let key = "\"char_offset\":";
        let start = json.find(key).expect("json 应含 char_offset 字段") + key.len();
        let rest = &json[start..];
        let end = rest.find('}').unwrap_or(rest.len());
        rest[..end].trim().parse().expect("char_offset 应为数字")
    }

    #[test]
    fn test_char_right_moves_forward_one_char() {
        let h = open_writer();
        // "Hello World" 第0段，从 offset=0 (H) 向右移动一次应该到 offset=1 (e)
        let ptr = roe_cursor_move_inline(h, 0, 0, 0, 0, CursorInlineCommand::CharRight as u32);
        assert!(!ptr.is_null(), "CharRight 应该成功");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        assert!(s.contains("\"local_para_index\":0"));
        let offset = parse_char_offset(&s);
        assert_eq!(offset, 1, "从 offset=0 向右移动一次应该到 offset=1，实际 {offset}");
        handle::remove(h);
    }

    #[test]
    fn test_char_right_at_line_end_stays_or_clamps() {
        let h = open_writer();
        // "Hello World" 长度为11，offset=11 已经是行尾，再向右移动
        // Parley Cursor::next_visual 在无下一个 cluster 时返回原地不动，
        // 所以应该仍然是 11（不 panic、不越界到下一段）。
        let ptr = roe_cursor_move_inline(h, 0, 0, 0, 11, CursorInlineCommand::CharRight as u32);
        assert!(!ptr.is_null(), "行尾 CharRight 不应该失败");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset = parse_char_offset(&s);
        assert_eq!(offset, 11, "行尾再向右移动应该原地不动，实际 {offset}");
        handle::remove(h);
    }

    #[test]
    fn test_non_body_container_type_rejected() {
        let h = open_writer();
        let ptr = roe_cursor_move_inline(h, 1, 0, 0, 0, CursorInlineCommand::CharRight as u32);
        assert!(ptr.is_null(), "非 BODY container_type 应该返回 null");
        handle::remove(h);
    }

    #[test]
    fn test_char_left_moves_backward_one_char() {
        let h = open_writer();
        // "Hello World" 第0段，从 offset=1 (e) 向左移动一次应该回到 offset=0 (H)
        let ptr = roe_cursor_move_inline(h, 0, 0, 0, 1, CursorInlineCommand::CharLeft as u32);
        assert!(!ptr.is_null(), "CharLeft 应该成功");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset = parse_char_offset(&s);
        assert_eq!(offset, 0, "从 offset=1 向左移动一次应该到 offset=0，实际 {offset}");
        handle::remove(h);
    }

    #[test]
    fn test_char_left_at_line_start_stays_or_clamps() {
        let h = open_writer();
        let ptr = roe_cursor_move_inline(h, 0, 0, 0, 0, CursorInlineCommand::CharLeft as u32);
        assert!(!ptr.is_null(), "行首 CharLeft 不应该失败");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset = parse_char_offset(&s);
        assert_eq!(offset, 0, "行首再向左移动应该原地不动，实际 {offset}");
        handle::remove(h);
    }

    #[test]
    fn test_word_right_moves_to_next_word_boundary() {
        let h = open_writer();
        // "Hello World"：从 offset=0 (H) 向右按词移动，应该越过 "Hello" 落在
        // 空格后 "World" 开头附近（offset=6），具体落点由 Parley 词边界算法
        // 决定，这里只断言"确实前进了，且没有冲过头到整行结束"。
        let ptr = roe_cursor_move_inline(h, 0, 0, 0, 0, CursorInlineCommand::WordRight as u32);
        assert!(!ptr.is_null(), "WordRight 应该成功");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset = parse_char_offset(&s);
        assert!(offset > 0 && offset <= 11, "WordRight 应该前进到某个词边界，实际 {offset}");
        handle::remove(h);
    }

    #[test]
    fn test_word_left_moves_to_previous_word_boundary() {
        let h = open_writer();
        // 从行尾 (offset=11) 向左按词移动，应该退回到某个更靠前的词边界。
        let ptr = roe_cursor_move_inline(h, 0, 0, 0, 11, CursorInlineCommand::WordLeft as u32);
        assert!(!ptr.is_null(), "WordLeft 应该成功");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let offset = parse_char_offset(&s);
        assert!(offset < 11, "WordLeft 应该后退到更靠前的词边界，实际 {offset}");
        handle::remove(h);
    }

    #[test]
    fn test_invalid_command_value_rejected() {
        let h = open_writer();
        let ptr = roe_cursor_move_inline(h, 0, 0, 0, 0, 999);
        assert!(ptr.is_null(), "非法 command 数值应该返回 null，而不是 panic");
        handle::remove(h);
    }

    #[test]
    fn test_invalid_handle_returns_null() {
        let ptr = roe_cursor_move_inline(999_999, 0, 0, 0, 0, CursorInlineCommand::CharRight as u32);
        assert!(ptr.is_null(), "无效句柄应该返回 null");
    }

    // ==== 下面是列表光标乱跳排查用测试（v105 六.2），参照 fodt.rs 现成的
    // <text:list>/<text:list-item> XML 结构（fodt.rs 约809-826行），构造一个
    // 编号列表，两个 list-item，对第一项逐字符 CharRight，断言每步只推进
    // "一个字符的字节数"，不应该跳过一大截（复现用户反馈的"类似v97跳过约10
    // 个字符"现象）。====

    const LIST_FODT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
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
    <text:list-style style:name="L1">
      <text:list-level-style-number text:level="1" style:num-format="1" style:num-suffix=".">
      </text:list-level-style-number>
    </text:list-style>
  </office:styles>
  <office:body>
    <office:text>
      <text:list text:style-name="L1">
        <text:list-item><text:p text:style-name="Default">Apple</text:p></text:list-item>
        <text:list-item><text:p text:style-name="Default">Grape</text:p></text:list-item>
      </text:list>
    </office:text>
  </office:body>
</office:document>"#;

    fn open_writer_list() -> u64 {
        let doc = FodtParser::parse(LIST_FODT).expect("parse failed");
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
    fn test_list_item_char_right_no_skip() {
        let h = open_writer_list();
        // para_index=0 是列表第一项 "Apple"（5字节，全ASCII，逐字符=逐字节）。
        // 从 offset=0 开始，连续 CharRight 5 次，期望依次是 1,2,3,4,5，
        // 任何一步如果跳过不止1，就是本次要抓的"乱跳"现象。
        let mut offset = 0u32;
        let mut history = vec![offset];
        for _ in 0..5 {
            let ptr = roe_cursor_move_inline(h, 0, 0, 0, offset, CursorInlineCommand::CharRight as u32);
            assert!(!ptr.is_null(), "列表项内 CharRight 不应该失败，当前offset={offset}");
            let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
            let new_offset = parse_char_offset(&s) as u32;
            history.push(new_offset);
            assert!(
                new_offset == offset + 1,
                "列表项内逐字符右移应该每步+1，实际从offset={offset}跳到{new_offset}（跳跃量={}），完整轨迹={:?}",
                new_offset as i64 - offset as i64, history
            );
            offset = new_offset;
        }
        assert_eq!(offset, 5, "5次CharRight后应该到达'Apple'末尾offset=5，实际{offset}");
        handle::remove(h);
    }

    #[test]
    fn test_list_second_item_char_right_no_skip() {
        let h = open_writer_list();
        // para_index=1 是列表第二项 "Grape"（5字节），同样逐字符验证。
        // 第二项更容易暴露"label累加算重复/算漏"这类问题，因为它前面已经
        // 有第一项的label参与过排版。
        let mut offset = 0u32;
        let mut history = vec![offset];
        for _ in 0..5 {
            let ptr = roe_cursor_move_inline(h, 0, 0, 1, offset, CursorInlineCommand::CharRight as u32);
            assert!(!ptr.is_null(), "第二个列表项内 CharRight 不应该失败，当前offset={offset}");
            let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
            let new_offset = parse_char_offset(&s) as u32;
            history.push(new_offset);
            assert!(
                new_offset == offset + 1,
                "第二个列表项内逐字符右移应该每步+1，实际从offset={offset}跳到{new_offset}（跳跃量={}），完整轨迹={:?}",
                new_offset as i64 - offset as i64, history
            );
            offset = new_offset;
        }
        assert_eq!(offset, 5, "5次CharRight后应该到达'Grape'末尾offset=5，实际{offset}");
        handle::remove(h);
    }
}
