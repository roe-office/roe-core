//! SplitEngine：根据 ResolveResult 创建 Follow Frame，更新 FrameArena

use roe_core::arena::{FrameId, PageId};
use crate::frame::{FrameArena, FrameContent, LineBox, Margins, TableCellFrame};
use crate::widow_orphan::ResolveResult;

/// 分割一个已在 arena 中的 frame
/// 把 lines[break_at..] 移到新页上，建立 follow 链接
/// 返回：新页的 PageId 和新 frame 的 FrameId
///
/// WROOT-030 结构性重构：TableRow variant 不支持行内断裂（某些 cell 在这页
/// 写完、某些续到下一页），直接返回 None。这是刻意简化的初版范围，
/// 已记入 WROOT-030 设计文档的"已知限制"，后续如有实际样本需要再单独处理。
pub fn split_frame(
    arena: &mut FrameArena,
    frame_id: FrameId,
    resolve: ResolveResult,
    page_width: f32,
    page_height: f32,
    margins: Margins,
    // [WROOT-027-CONT] 若 Some，说明分割出的新页属于note内容渲染场景，
    // 需要在新页顶部插入一条续接分隔线。参数：(width_pt, thickness_pt,
    // color, margin_before)。必须在 arena.new_page 之后、把内容 frame
    // 塞进新页之前调用——这样内容 frame 的 y_on_page 才能正确地把这条
    // 线的高度算进去，而不是像此前的失败尝试那样在事后（内容已经就位）
    // 才补插，导致线和内容重叠（v142/v143交接文档记录的完整根因链）。
    continuation_rule: Option<(f32, f32, roe_model::common::Color, f32)>,
) -> Option<(PageId, FrameId)> {
    let break_at = match resolve {
        ResolveResult::Keep(_) => return None,   // 不需要分割
        ResolveResult::MoveToNextPage => 0,       // 整段移到下一页
        ResolveResult::ShiftUp(b) => b,
    };

    // 从当前 frame 取出需要移走的行（仅 Paragraph 支持；TableRow 直接放弃）
    let tail_lines: Vec<LineBox> = {
        let frame = &mut arena.frames[frame_id];
        match &mut frame.content {
            FrameContent::Paragraph(lines) => {
                if break_at >= lines.len() {
                    return None; // 没有需要移的行
                }
                lines.split_off(break_at)
            }
            FrameContent::TableRow(_) => {
                // 表格行不支持行内断裂，参见函数文档
                return None;
            }
            FrameContent::HorizontalRule { .. } => {
                // 分隔线不支持行内断裂：要么整条放在当前页，要么（由
                // 下面 split_frame_move_all 这类整体搬迁逻辑）整条移到
                // 下一页，没有"切一半"的概念。
                return None;
            }
        }
    };

    // 取出原 frame 的元数据
    let (para_index, space_after) = {
        let frame = &arena.frames[frame_id];
        (frame.para_index, frame.space_after)
    };

    // 原 frame 不再是链尾，space_after 归零（只有链尾才有）
    arena.frames[frame_id].space_after = 0.0;

    // 新建下一页
    let new_page = arena.new_page(page_width, page_height, margins);

    // [WROOT-027-CONT] 若需要续接分隔线，先插在新页顶部，再计算内容
    // frame 该让出多少高度。rule_extra_h 是线连同上下 margin 的总占用
    // 高度（thickness + space_before(=margin_before) + space_after(固定6.0，
    // 与首页那条线的规格保持一致，见 layout.rs 首次插线调用）。
    let mut rule_extra_h = 0.0_f32;
    if let Some((width_pt, thickness_pt, color, margin_before)) = continuation_rule {
        let rule_y = arena.page_content_global_y(new_page) + margin_before;
        let _ = arena.insert_horizontal_rule_frame(
            new_page, para_index, width_pt, thickness_pt, color,
            margin_before, rule_y, margin_before, 6.0,
        );
        // [WROOT-027-CONT修复] 此前这里写的 margin_before+thickness_pt+6.0
        // 少算了一个margin_before——insert_horizontal_rule_frame的
        // space_before参数也传了margin_before，而used_height_in_column
        // 对链头帧会把space_before单独计入一次，再加height()内部
        // 自己的margin_before，等于margin_before被算了两次。实测
        // debug打印验证：used_height_in_column对这条线的真实统计是
        // margin_before(space_before) + (thickness_pt+margin_before)(height())
        // + 6.0(space_after) = 2*margin_before + thickness_pt + 6.0。
        // 此前的公式少了一个margin_before(12pt)，多级分页嵌套时
        // （note内容连续翻两次页）这个偏差被放大，导致027第4页内容
        // 和分隔线视觉重叠。
        rule_extra_h = 2.0 * margin_before + thickness_pt + 6.0;
    }

    // 在新页插入 follow frame
    let follow_id = arena.insert_frame(
        new_page,
        para_index,
        tail_lines,
        arena.page_content_global_y(new_page) + rule_extra_h, // 让出续接分隔线的高度
        0.0, // follow frame 无 space_before
        space_after,
        0,  // 换页后重置为第0栏
    );

    // 建立双向链接
    arena.link_follow(frame_id, follow_id);

    Some((new_page, follow_id))
}

/// 把整段内容（所有 lines 或所有 cells）作为新 frame 放到新页
/// 用于 MoveToNextPage 场景（原 frame 的内容已清空，需要重建）
///
/// WROOT-030 结构性重构：Paragraph 和 TableRow 都支持整体搬移，
/// 只是 TableRow 搬的是 cells 而不是 lines。
pub fn move_frame_to_new_page(
    arena: &mut FrameArena,
    frame_id: FrameId,
    page_width: f32,
    page_height: f32,
    margins: Margins,
    // [WROOT-027-CONT] 语义与 split_frame 同名参数完全一致，见其注释。
    continuation_rule: Option<(f32, f32, roe_model::common::Color, f32)>,
) -> (PageId, FrameId) {
    // 取出原 frame 的全部内容（按 variant 分支处理）+ 元数据
    let (para_index, space_after) = {
        let f = &arena.frames[frame_id];
        (f.para_index, f.space_after)
    };
    arena.frames[frame_id].space_after = 0.0;

    let new_page = arena.new_page(page_width, page_height, margins);

    // [WROOT-027-CONT] 同 split_frame：先插续接线，再算内容该让出的高度。
    let mut rule_extra_h = 0.0_f32;
    if let Some((width_pt, thickness_pt, color, margin_before)) = continuation_rule {
        let rule_y = arena.page_content_global_y(new_page) + margin_before;
        let _ = arena.insert_horizontal_rule_frame(
            new_page, para_index, width_pt, thickness_pt, color,
            margin_before, rule_y, margin_before, 6.0,
        );
        // [WROOT-027-CONT修复] 此前这里写的 margin_before+thickness_pt+6.0
        // 少算了一个margin_before——insert_horizontal_rule_frame的
        // space_before参数也传了margin_before，而used_height_in_column
        // 对链头帧会把space_before单独计入一次，再加height()内部
        // 自己的margin_before，等于margin_before被算了两次。实测
        // debug打印验证：used_height_in_column对这条线的真实统计是
        // margin_before(space_before) + (thickness_pt+margin_before)(height())
        // + 6.0(space_after) = 2*margin_before + thickness_pt + 6.0。
        // 此前的公式少了一个margin_before(12pt)，多级分页嵌套时
        // （note内容连续翻两次页）这个偏差被放大，导致027第4页内容
        // 和分隔线视觉重叠。
        rule_extra_h = 2.0 * margin_before + thickness_pt + 6.0;
    }

    let new_fid = {
        let frame = &mut arena.frames[frame_id];
        match &mut frame.content {
            FrameContent::Paragraph(lines) => {
                let all_lines: Vec<LineBox> = std::mem::take(lines);
                arena.insert_frame(
                    new_page, para_index, all_lines,
                    arena.page_content_global_y(new_page) + rule_extra_h, 0.0, space_after,
                    0,  // 换页后重置为第0栏（跨页时不保留原栏序号，从新页第0栏开始）
                )
            }
            FrameContent::TableRow(cells) => {
                let all_cells: Vec<TableCellFrame> = std::mem::take(cells);
                // WROOT-101：跨页续行时延续原frame的is_floating状态，避免
                // 硬编码true/false破坏非浮动表格既有跨页行为（is_floating
                // 在这里读取是因为下面insert_table_row_frame调用会消耗
                // frame的可变借用，需要在此之前先读出这个Copy类型字段）。
                let is_floating = frame.is_floating;
                arena.insert_table_row_frame(
                    new_page, para_index, all_cells,
                    arena.page_content_global_y(new_page) + rule_extra_h, 0.0, space_after,
                    is_floating,
                )
            }
            FrameContent::HorizontalRule { width_pt, thickness_pt, color, margin_before } => {
                // 分隔线整条搬到新页开头：复用 FrameArena::insert_horizontal_rule_frame
                // （与 Paragraph/TableRow 分支共享同一套 SlotMap 插入模式）。
                let (w, t, c, m) = (*width_pt, *thickness_pt, *color, *margin_before);
                arena.insert_horizontal_rule_frame(
                    new_page, para_index, w, t, c, m,
                    arena.page_content_global_y(new_page) + rule_extra_h, 0.0, space_after,
                )
            }
        }
    };

    arena.link_follow(frame_id, new_fid);

    (new_page, new_fid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::LineBox;

    fn make_lines(count: usize) -> Vec<LineBox> {
        (0..count).map(|i| LineBox {
            y_offset: i as f32 * 20.0,
            ascent: 16.0,
            descent: 4.0,
            line_height: 20.0,
            text: format!("line {i}"),
            runs: Vec::new(),
            ..Default::default()
        }).collect()
    }

    /// 从 frame.content 里取出 Paragraph 的 lines 长度（测试专用小助手，
    /// 因为 content 字段现在是枚举，不能再直接 .lines.len()）
    fn paragraph_lines_len(arena: &FrameArena, fid: FrameId) -> usize {
        match &arena.frames[fid].content {
            FrameContent::Paragraph(lines) => lines.len(),
            FrameContent::TableRow(_) => panic!("expected Paragraph in this test"),
            FrameContent::HorizontalRule { .. } => panic!("expected Paragraph in this test"),
        }
    }

    #[test]
    fn test_split_frame_break_at_2() {
        let mut arena = FrameArena::new();
        let page0 = arena.new_page(595.0, 842.0, Margins::uniform(56.7));
        let fid = arena.insert_frame(page0, 0, make_lines(5), 0.0, 0.0, 4.0, 0);

        let result = split_frame(
            &mut arena,
            fid,
            ResolveResult::ShiftUp(2),
            595.0, 842.0, Margins::uniform(56.7),
            None,
        );

        assert!(result.is_some());
        let (_new_page, follow_id) = result.unwrap();

        // 原 frame 保留 2 行
        assert_eq!(paragraph_lines_len(&arena, fid), 2);
        // follow frame 有 3 行
        assert_eq!(paragraph_lines_len(&arena, follow_id), 3);
        // 链接正确
        assert_eq!(arena.frames[fid].follow, Some(follow_id));
        assert_eq!(arena.frames[follow_id].precede, Some(fid));
        // space_after 移到链尾
        assert_eq!(arena.frames[fid].space_after, 0.0);
        assert_eq!(arena.frames[follow_id].space_after, 4.0);
    }

    #[test]
    fn test_move_to_new_page() {
        let mut arena = FrameArena::new();
        let page0 = arena.new_page(595.0, 842.0, Margins::uniform(56.7));
        let fid = arena.insert_frame(page0, 0, make_lines(3), 0.0, 0.0, 4.0, 0);

        let (new_page, new_fid) = move_frame_to_new_page(&mut arena, fid, 595.0, 842.0, Margins::uniform(56.7), None);

        // 原 frame 行数清空
        assert_eq!(paragraph_lines_len(&arena, fid), 0);
        // 新 frame 有 3 行
        assert_eq!(paragraph_lines_len(&arena, new_fid), 3);
        // 新页存在
        assert!(arena.pages.contains_key(new_page));
        // follow 链接
        assert_eq!(arena.frames[fid].follow, Some(new_fid));
    }

    #[test]
    fn test_follow_chain() {
        let mut arena = FrameArena::new();
        let page0 = arena.new_page(595.0, 842.0, Margins::uniform(56.7));
        let fid0 = arena.insert_frame(page0, 0, make_lines(2), 0.0, 0.0, 4.0, 0);

        // 手动建两级 follow
        let (_, fid1) = move_frame_to_new_page(&mut arena, fid0, 595.0, 842.0, Margins::uniform(56.7), None);
        let (_, fid2) = move_frame_to_new_page(&mut arena, fid1, 595.0, 842.0, Margins::uniform(56.7), None);

        let chain = arena.follow_chain(fid0);
        assert_eq!(chain, vec![fid0, fid1, fid2]);
    }

    #[test]
    fn test_split_frame_table_row_returns_none() {
        // WROOT-030 简化范围验证：TableRow 不支持行内断裂
        let mut arena = FrameArena::new();
        let page0 = arena.new_page(595.0, 842.0, Margins::uniform(56.7));
        let cells = vec![TableCellFrame {
            extra_spacing: 0.0,
            x_offset: 0.0,
            cell_width: 100.0,
            lines: make_lines(3),
            border_top: Some(crate::frame::BorderStyle {
                color: roe_model::common::Color { r: 0, g: 0, b: 0, a: 255 },
                width: 1.0, dashed: false,
            }),
            border_bottom: Some(crate::frame::BorderStyle {
                color: roe_model::common::Color { r: 0, g: 0, b: 0, a: 255 },
                width: 1.0, dashed: false,
            }),
            border_left: Some(crate::frame::BorderStyle {
                color: roe_model::common::Color { r: 0, g: 0, b: 0, a: 255 },
                width: 1.0, dashed: false,
            }),
            border_right: Some(crate::frame::BorderStyle {
                color: roe_model::common::Color { r: 0, g: 0, b: 0, a: 255 },
                width: 1.0, dashed: false,
            }),
            paragraph_count: 1,
            bg_color: None,
            border_color: None,
            border_width: None,
        }];
        let fid = arena.insert_table_row_frame(page0, 0, cells, 0.0, 0.0, 0.0, false);

        let result = split_frame(&mut arena, fid, ResolveResult::ShiftUp(1), 595.0, 842.0, Margins::uniform(56.7), None);
        assert!(result.is_none());
    }

    #[test]
    fn test_move_table_row_to_new_page() {
        let mut arena = FrameArena::new();
        let page0 = arena.new_page(595.0, 842.0, Margins::uniform(56.7));
        let cells = vec![TableCellFrame {
            extra_spacing: 0.0,
            x_offset: 0.0,
            cell_width: 100.0,
            lines: make_lines(2),
            border_top: Some(crate::frame::BorderStyle {
                color: roe_model::common::Color { r: 0, g: 0, b: 0, a: 255 },
                width: 1.0, dashed: false,
            }),
            border_bottom: Some(crate::frame::BorderStyle {
                color: roe_model::common::Color { r: 0, g: 0, b: 0, a: 255 },
                width: 1.0, dashed: false,
            }),
            border_left: Some(crate::frame::BorderStyle {
                color: roe_model::common::Color { r: 0, g: 0, b: 0, a: 255 },
                width: 1.0, dashed: false,
            }),
            border_right: Some(crate::frame::BorderStyle {
                color: roe_model::common::Color { r: 0, g: 0, b: 0, a: 255 },
                width: 1.0, dashed: false,
            }),
            paragraph_count: 1,
            bg_color: None,
            border_color: None,
            border_width: None,
        }];
        let fid = arena.insert_table_row_frame(page0, 0, cells, 0.0, 0.0, 4.0, false);

        let (new_page, new_fid) = move_frame_to_new_page(&mut arena, fid, 595.0, 842.0, Margins::uniform(56.7), None);

        match &arena.frames[new_fid].content {
            FrameContent::TableRow(cells) => assert_eq!(cells.len(), 1),
            FrameContent::Paragraph(_) => panic!("expected TableRow"),
            FrameContent::HorizontalRule { .. } => panic!("expected TableRow"),
        }
        assert!(arena.pages.contains_key(new_page));
        assert_eq!(arena.frames[fid].follow, Some(new_fid));
    }
}
