//! roe_query_selection_rects —— 选区几何查询（见
//! roe_cursor_selection_final_v1.md 3.4）。
//!
//! 设计要点（final 文档 二、"选区几何同理"，已拍板，不用 Parley
//! Selection::geometry，因为它假设单个 Layout 内部，不知道 roe 的多行
//! LineBox / 跨 FrameArena 段落链结构）：
//! - 按行盒拼装：遍历选区覆盖的每一行 LineBox，每行产出一个矩形。
//! - 首尾行按实际选中字节范围裁剪 x 范围（重新 shape 精确算 x）；
//!   中间完整行用整行宽度，不重新 shape（性能更好，视觉上下边缘也更整齐，
//!   Gemini 提出已采纳）。
//! - 整行宽度：本次只支持 BODY（普通段落）容器，用
//!   PageFrame::content_width()（页面内容区宽度）。LineBox.x_offset/
//!   cell_width 是"表格 cell 用"字段，普通段落场景下恒为 0.0，不能当
//!   "整行宽度"用（本次不支持表格，容器类型已在入口拦截）。
//! - 支持跨段落选区：从 start_loc 到 end_loc 之间的每个 para_index 各自
//!   find_chain_by_para_index，按文档顺序（para_index 从小到大）遍历。
//!   若调用方传入 start_loc 在 end_loc 之后（用户从后往前拖拽选择），
//!   入口处按 para_index 做交换，保证内部处理顺序恒定。

use std::ffi::CString;
use std::os::raw::c_char;

use roe_core::arena::FrameId;
use roe_model::writer::BodyElement;
use roe_parser::RoeDocument;
use roe_writer::frame::LineBox;
use roe_writer::layout::resolve_para_style;
use roe_writer::line_shape::char_offset_to_x;

use crate::cursor::RoeLocation;
use crate::error::set_last_error;
use crate::handle;

/// 查询选区覆盖的矩形列表（一行一个矩形，按行盒拼装）。
///
/// 参数：拆解后的两组 RoeLocation 裸字段（start_/end_ 前缀），语义同
/// cursor.rs 的 FFI 边界约定。
///
/// 返回：JSON 字符串
/// `[{"page_idx":N,"x":..,"y":..,"w":..,"h":..}, ...]`
/// 坐标单位 pt，与 roe_query_layout_rect 相同坐标系（x 是"文档全局坐标"，
/// 已包含 margin_left + text_x_offset，与 layout_query.rs::locate_in_frame
/// 的 final_x 语义完全一致；v103修复，此前这里错误地返回不含margin的
/// 行内相对坐标，导致选区矩形与光标系统性错位，同源于v95已修过的bug）。
/// 选区为空（start==end）时返回空数组 `[]`，不是
/// null（空数组不是错误，是合法结果）。
///
/// 失败返回 null，调用 roe_last_error_message 获取详情。
/// 调用方必须用 roe_free_string 释放返回值。
///
/// 本次（v94）仅支持 container_type=BODY；非 BODY 返回 null + last_error。
#[no_mangle]
pub extern "C" fn roe_query_selection_rects(
    handle: u64,
    start_container_type: u32,
    start_container_id: u32,
    start_local_para_index: u32,
    start_char_offset: u32,
    end_container_type: u32,
    end_container_id: u32,
    end_local_para_index: u32,
    end_char_offset: u32,
) -> *mut c_char {
    let start_loc = RoeLocation {
        container_type: start_container_type,
        container_id: start_container_id,
        local_para_index: start_local_para_index,
        char_offset: start_char_offset,
    };
    let end_loc = RoeLocation {
        container_type: end_container_type,
        container_id: end_container_id,
        local_para_index: end_local_para_index,
        char_offset: end_char_offset,
    };

    if !start_loc.is_body() || !end_loc.is_body() {
        set_last_error("roe_query_selection_rects: 本次仅支持 container_type=BODY（表格cell/文本框预留未实现）");
        return std::ptr::null_mut();
    }

    if !handle::ensure_writer_layout(handle) {
        set_last_error("roe_query_selection_rects: 无效句柄或非 Writer 文档");
        return std::ptr::null_mut();
    }

    let (start_para, start_offset) = start_loc.as_legacy_body();
    let (end_para, end_offset) = end_loc.as_legacy_body();

    // 按文档顺序（para_index 从小到大；同一段落内 char_offset 从小到大）
    // 归一化，不假设调用方一定按顺序传 start/end（用户可能从后往前拖拽）。
    let ((from_para, from_offset), (to_para, to_offset)) =
        if (start_para, start_offset) <= (end_para, end_offset) {
            ((start_para, start_offset), (end_para, end_offset))
        } else {
            ((end_para, end_offset), (start_para, start_offset))
        };

    let result = handle::with_entry(handle, |entry| {
        query_selection_rects_in_entry(entry, from_para, from_offset, to_para, to_offset)
    });

    match result {
        None => {
            set_last_error("roe_query_selection_rects: 无效句柄");
            std::ptr::null_mut()
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_query_selection_rects: {msg}"));
            std::ptr::null_mut()
        }
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(e) => {
                set_last_error(format!("roe_query_selection_rects: {e}"));
                std::ptr::null_mut()
            }
        },
    }
}

/// 核心逻辑：遍历 from_para..=to_para 之间所有段落，对每个段落找到其
/// frame 链，链内每一行判断是否与选区相交，相交则产出一个矩形。
fn query_selection_rects_in_entry(
    entry: &mut handle::DocumentEntry,
    from_para: usize,
    from_offset: usize,
    to_para: usize,
    to_offset: usize,
) -> Result<String, String> {
    let RoeDocument::Writer(doc) = &entry.doc else {
        return Err("非 Writer 文档".to_string());
    };

    if doc.body.get(from_para).is_none() || doc.body.get(to_para).is_none() {
        return Err(format!("para_index 越界：from={from_para} to={to_para}"));
    }

    let default_font_size = entry
        .writer_font_registry
        .as_ref()
        .map(|r| r.default_font_size)
        .ok_or_else(|| "writer_font_registry 未建立（内部错误）".to_string())?;

    let mut rects: Vec<String> = Vec::new();

    for para_index in from_para..=to_para {
        let para = match doc.body.get(para_index) {
            Some(BodyElement::Paragraph(p)) => p,
            // 非段落元素（比如表格）本次不支持选区几何，跳过而不是报错
            // ——选区可能横跨"段落+表格+段落"，表格部分本次没有行数据
            // 可产出矩形，静默跳过好过整体失败。
            _ => continue,
        };
        let resolved = resolve_para_style(doc, para, default_font_size);

        // 本段落内选区裁剪范围：中间段落（既不是 from_para 也不是
        // to_para）整段全选；起始段落从 from_offset 到段落末尾；
        // 结束段落从段落开头到 to_offset；若 from_para==to_para，则
        // 从 from_offset 到 to_offset。
        let para_start_offset = if para_index == from_para { Some(from_offset) } else { None };
        let para_end_offset = if para_index == to_para { Some(to_offset) } else { None };

        let layout = entry
            .writer_layout_cache
            .as_ref()
            .ok_or_else(|| "writer_layout_cache 未建立（内部错误）".to_string())?;
        let arena = &layout.arena;

        let chain = arena.find_chain_by_para_index(para_index);
        if chain.is_empty() {
            continue;
        }

        let mut consumed_before_frame = 0usize; // 本段落内，已跨过的字节数（跨 frame 累加）
        let last_fid_in_chain = *chain.last().expect("chain已判空，此处必有元素");
        for &fid in &chain {
            let frame = &arena.frames[fid];
            let page = &arena.pages[frame.page];
            let content_width = page.content_width();
            let page_idx = arena.page_order.iter().position(|&pid| pid == frame.page).unwrap_or(0);
            let is_last_frame_in_chain = fid == last_fid_in_chain;

            // WROOT-YOFFSET-01修复（同源于 layout_query.rs::locate_in_frame
            // v96/本次修复）：这里的 y 计算此前完全没有考虑 space_before
            // （段前距），与 tile.rs 真实渲染路径（cursor_y_pt = frame.y_on_page
            // + space_before 起算）不一致，导致任何 space_before>0 的段落
            // （典型如标题样式），从这个frame开始选区矩形y坐标相比真实渲染
            // 位置系统性偏小（偏上）恰好 space_before 那么多pt。
            let space_before = doc.body.get(frame.para_index).and_then(|e| {
                if let roe_model::writer::BodyElement::Paragraph(p) = e {
                    p.space_before.or(
                        doc.styles.resolve_paragraph_style(p.style.0.as_str()).space_before
                    )
                } else { None }
            }).unwrap_or(0.0);

            let mut consumed_in_frame = 0usize;
            let lines_in_frame = frame.text_lines();
            let last_line_index = lines_in_frame.len().checked_sub(1);
            for (line_index, line) in lines_in_frame.iter().enumerate() {
                let line_len = line.text.len();
                // 本行是否是"本段落的物理最后一行"（跨frame链条的最后一个
                // frame、且是该frame内最后一行）。与"选区在本段落是否到此
                // 为止（end_clip_at_line_tail）"是两个不同维度的概念，不能
                // 混用——即使选区边界判定为"不裁剪"（比如中间段落全选），
                // 只要这行是段落物理最后一行，就必须裁剪到文字实际宽度，
                // 不能整行铺满到content_width（v103二次修复：此前把"选区
                // 是否到行尾"和"是否为段落最后一行"当成同一件事，导致段落
                // 最后一行在跨段落选区里被错误铺满到页面右边界）。
                let is_para_last_line = is_last_frame_in_chain
                    && last_line_index == Some(line_index);
                let line_start_in_para = consumed_before_frame + consumed_in_frame;
                let line_end_in_para = line_start_in_para + line_len;

                let line_selected = line_intersects_selection(
                    line_start_in_para, line_end_in_para,
                    para_start_offset, para_end_offset,
                );

                if line_selected {
                    let y = frame.y_on_page + space_before + line.y_offset;
                    let h = line.line_height;

                    // v103修复：与 roe_query_layout_rect（layout_query.rs 的
                    // locate_in_frame，v95已修过同源bug）保持相同坐标系——
                    // x 必须是"文档全局坐标"（含 margin_left + text_x_offset），
                    // 不能是行内相对坐标。此前这里只返回 x_in_line，导致选区
                    // 矩形系统性偏左约 margin_left 的量，与光标坐标系不一致。
                    let line_abs_x_start = page.margin_left + line.text_x_offset;

                    // 本行是否需要裁剪起点/终点（首尾行精确到字符，
                    // 中间行用整行宽度，不重新 shape）。
                    let needs_start_clip = para_start_offset
                        .map(|o| o > line_start_in_para && o < line_end_in_para)
                        .unwrap_or(false);
                    let needs_end_clip = para_end_offset
                        .map(|o| o > line_start_in_para && o < line_end_in_para)
                        .unwrap_or(false);
                    // 起点落在本行内部（选区从这一行中间开始，而不是行首）
                    let start_clip_at_line_head = para_start_offset
                        .map(|o| o <= line_start_in_para)
                        .unwrap_or(true);
                    let end_clip_at_line_tail = para_end_offset
                        .map(|o| o >= line_end_in_para)
                        .unwrap_or(true);

                    let (x0, x1) = if needs_start_clip || needs_end_clip
                        || !start_clip_at_line_head || !end_clip_at_line_tail
                        || is_para_last_line
                    {
                        let registry = entry
                            .writer_font_registry
                            .as_mut()
                            .ok_or_else(|| "writer_font_registry 未建立（内部错误）".to_string())?;

                        let start_x = if start_clip_at_line_head {
                            line_abs_x_start
                        } else {
                            let offset_in_line = para_start_offset
                                .unwrap()
                                .saturating_sub(line_start_in_para)
                                .min(line_len);
                            line_abs_x_start + char_offset_to_x(
                                registry, &line.text, &line.runs,
                                resolved.font_size, resolved.bold, resolved.italic,
                                offset_in_line,
                            )
                        };
                        // 段落最后一行：即使选区判定"到行尾"，也不能铺满到
                        // content_width，要么裁剪到选区真实终点（如果选区
                        // 终点就落在这一行内），要么裁剪到本行文字的实际
                        // 结束宽度（选区终点在本行之后，即本段落被后续更
                        // 靠后的段落选区覆盖，此时应显示这一行文字全部被
                        // 选中，但只到文字末尾、不含行尾空白）。
                        let end_x = if needs_end_clip {
                            let offset_in_line = para_end_offset
                                .unwrap()
                                .saturating_sub(line_start_in_para)
                                .min(line_len);
                            line_abs_x_start + char_offset_to_x(
                                registry, &line.text, &line.runs,
                                resolved.font_size, resolved.bold, resolved.italic,
                                offset_in_line,
                            )
                        } else if is_para_last_line {
                            line_abs_x_start + char_offset_to_x(
                                registry, &line.text, &line.runs,
                                resolved.font_size, resolved.bold, resolved.italic,
                                line_len,
                            )
                        } else {
                            line_abs_x_start + (content_width - line.text_x_offset).max(0.0)
                        };
                        (start_x, end_x)
                    } else {
                        (line_abs_x_start, line_abs_x_start + (content_width - line.text_x_offset).max(0.0))
                    };

                    let w = (x1 - x0).max(0.0);
                    rects.push(format!(
                        "{{\"page_idx\":{},\"x\":{:.2},\"y\":{:.2},\"w\":{:.2},\"h\":{:.2}}}",
                        page_idx, x0, y, w, h
                    ));
                }

                consumed_in_frame += line_len;
            }

            // 重新借用 arena（entry.writer_font_registry 的可变借用结束后）
            let _ = arena; // 仅为通过借用检查的可读性标记，见下方注释
            consumed_before_frame += frame_text_len(&entry.writer_layout_cache.as_ref().unwrap().arena, fid);
        }
    }

    Ok(format!("[{}]", rects.join(",")))
}

fn frame_text_len(arena: &roe_writer::frame::FrameArena, fid: FrameId) -> usize {
    arena.frames[fid].text_lines().iter().map(|l: &LineBox| l.text.len()).sum()
}

/// 判断 [line_start, line_end) 是否与选区范围相交。
/// para_start/para_end 为 None 表示该段落这一侧没有裁剪（中间段落全选，
/// 对应"该侧不设边界"）。
fn line_intersects_selection(
    line_start: usize,
    line_end: usize,
    para_start_offset: Option<usize>,
    para_end_offset: Option<usize>,
) -> bool {
    let sel_start = para_start_offset.unwrap_or(0);
    let sel_end = para_end_offset.unwrap_or(usize::MAX);

    if sel_start == sel_end {
        // 空选区（光标位置，无选中范围）：不产出矩形。
        return false;
    }

    // 区间相交判断：[line_start, line_end) 与 [sel_start, sel_end) 相交。
    // 空行（line_start == line_end）特殊处理：只要选区跨过这个点就算相交
    // （空行本身也应该有一个矩形，比如空段落被整段选中时）。
    if line_start == line_end {
        return sel_start <= line_start && line_start < sel_end;
    }
    line_start < sel_end && sel_start < line_end
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
      <text:p text:style-name="Default">Second paragraph text</text:p>
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

    /// 极简 JSON 数组元素计数：数组顶层 `{...}` 的个数（不做真正的 JSON
    /// 解析，与本项目其余测试一致的手写风格）。
    fn count_rects(json: &str) -> usize {
        json.matches("\"page_idx\"").count()
    }

    #[test]
    fn test_selection_within_single_line() {
        let h = open_writer();
        // 段落0 "Hello World"，选中 offset 0..5 ("Hello")，落在同一行，
        // 应该只产出1个矩形。
        let ptr = roe_query_selection_rects(h, 0, 0, 0, 0, 0, 0, 0, 5);
        assert!(!ptr.is_null(), "单行内选区查询应该成功");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        assert_eq!(count_rects(&s), 1, "单行选区应产出1个矩形，实际 json={s}");
        assert!(s.contains("\"page_idx\":0"));
        handle::remove(h);
    }

    #[test]
    fn test_empty_selection_returns_empty_array() {
        let h = open_writer();
        // start == end：空选区（纯光标，无选中范围），应返回空数组。
        let ptr = roe_query_selection_rects(h, 0, 0, 0, 3, 0, 0, 0, 3);
        assert!(!ptr.is_null(), "空选区查询本身不应失败");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        assert_eq!(s, "[]", "空选区应返回空数组，实际 {s}");
        handle::remove(h);
    }

    #[test]
    fn test_cross_paragraph_selection_produces_multiple_rects() {
        let h = open_writer();
        // 从段落0 offset=6 ("World" 起点) 选到段落1 offset=6 ("paragraph" 附近)，
        // 跨越两个段落，应该至少产出2个矩形（每段至少1行）。
        let ptr = roe_query_selection_rects(h, 0, 0, 0, 6, 0, 0, 1, 6);
        assert!(!ptr.is_null(), "跨段落选区查询应该成功");
        let s = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        let n = count_rects(&s);
        assert!(n >= 2, "跨段落选区应至少产出2个矩形，实际 {n}个，json={s}");
        handle::remove(h);
    }

    #[test]
    fn test_reversed_start_end_normalized() {
        let h = open_writer();
        // start 在 end 之后（用户从后往前拖拽），应该被内部归一化，
        // 结果应与正序传参一致（都应产出1个矩形，覆盖同一段文字）。
        let ptr_forward = roe_query_selection_rects(h, 0, 0, 0, 0, 0, 0, 0, 5);
        let s_forward = unsafe { CString::from_raw(ptr_forward) }.into_string().unwrap();

        let ptr_reversed = roe_query_selection_rects(h, 0, 0, 0, 5, 0, 0, 0, 0);
        assert!(!ptr_reversed.is_null(), "反向传参不应失败");
        let s_reversed = unsafe { CString::from_raw(ptr_reversed) }.into_string().unwrap();

        assert_eq!(s_forward, s_reversed, "正序/反序传参应归一化为相同结果");
        handle::remove(h);
    }

    #[test]
    fn test_non_body_container_type_rejected() {
        let h = open_writer();
        let ptr = roe_query_selection_rects(h, 1, 0, 0, 0, 0, 0, 0, 5);
        assert!(ptr.is_null(), "非 BODY container_type 应该返回 null");
        handle::remove(h);
    }

    #[test]
    fn test_invalid_para_index_returns_error() {
        let h = open_writer();
        let ptr = roe_query_selection_rects(h, 0, 0, 999, 0, 0, 0, 999, 5);
        assert!(ptr.is_null(), "越界 para_index 应该返回 null");
        handle::remove(h);
    }

    #[test]
    fn test_offset_beyond_line_end_clamped_not_panic() {
        let h = open_writer();
        // "Hello World" 长度11，选到超出范围的 offset=99999，应该钳制而非 panic。
        let ptr = roe_query_selection_rects(h, 0, 0, 0, 0, 0, 0, 0, 99999);
        assert!(!ptr.is_null(), "越界 offset 应该被钳制返回结果，而不是失败");
        handle::remove(h);
    }

    #[test]
    fn test_invalid_handle_returns_null() {
        let ptr = roe_query_selection_rects(999_999, 0, 0, 0, 0, 0, 0, 0, 5);
        assert!(ptr.is_null(), "无效句柄应该返回 null");
    }
}
