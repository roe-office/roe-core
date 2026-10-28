//! ParagraphLayoutEngine：把 WriterDocument 段落喂给 cosmic-text，
//! 做分页决策（PageFitChecker → WidowOrphanResolver → SplitEngine），
//! 产出 FrameArena（多页 Follow Chain）

use base64::Engine;
// cosmic-text 已在本文件完全退役（layout.rs 主循环+cell路径均已迁移到 Parley）
use parley::{
    Alignment, AlignmentOptions, FontFamily, FontStyle, FontWeight, GenericFamily, LineHeight,
    PositionedLayoutItem, StyleProperty, YieldData,
};
// WROOT-093 图文混排逐行环绕：taffy::FloatContext 管理每页浮动图片占用区域，
// 详见 ROE_DOCX_Progress_v44.md 第四节方案。仅支持贴左/贴右图片（FloatDirection
// 语义），居中/任意x坐标图片暂不缩窄（维持兜底不环绕行为），已与用户确认此范围。
use taffy::{Clear, FloatContext, FloatDirection};

/// WROOT-093-C：区间安全宽度查询。
///
/// taffy::FloatContext 只提供单点查询 find_content_slot(min_y, ...)，
/// 不提供区间查询。本函数通过反复调用 find_content_slot 并传入上一次
/// 返回的 segment_id（作为 `after` 参数）向下走，把 [y_start, y_end]
/// 区间内途经的所有 segment 依次枚举出来，取其中最窄的可用宽度/对应
/// 的 x 偏移，作为这个y区间内"安全"的断行宽度（保证区间内任意一点都
/// 不会比这个更窄，从而不会跟浮动对象重叠）。
///
/// 返回 (safe_width, safe_x)：
/// - safe_width：区间内最窄的可用宽度（如果区间内没有任何浮动对象
///   影响，返回值等于 effective_w，即完全不收窄）
/// - safe_x：safe_width 对应的那个 segment 的起始x（用于 set_line_x）
///
/// 注：immediately clamp 到 effective_w，避免个别 segment 的
/// width 字段是 f32::INFINITY（无浮动影响时的返回值，见 taffy
/// find_content_slot 源码第486/509/517行）导致比较结果失真。
fn find_safe_content_slots(
    fctx: &FloatContext,
    y_start: f32,
    y_end: f32,
    effective_w: f32,
) -> (f32, f32) {
    // y_end 理论上应该 >= y_start；若因浮点误差极小反转，钳制为一个点查询，
    // 退化行为等价于旧的单点查询，不会比原逻辑更差。
    let y_end = y_end.max(y_start);

    let mut safe_width = effective_w;
    let mut safe_x = 0.0f32;
    let mut narrowest_found = f32::INFINITY;

    let mut after: Option<usize> = None;
    let mut probe_y = y_start;
    // 防御性上限：正常文档一个行高区间不应跨越几十个 segment，
    // 上限只是防止 segment 划分异常（比如浮点误差导致 y 不推进）
    // 时死循环，不代表预期触发次数。
    const MAX_SEGMENTS_PROBED: u32 = 64;
    let mut probed = 0u32;

    loop {
        if probe_y > y_end || probed >= MAX_SEGMENTS_PROBED {
            break;
        }
        let slot = fctx.find_content_slot(probe_y, [0.0, 0.0], Clear::None, after);
        let this_width = slot.width.min(effective_w);
        if this_width < narrowest_found {
            narrowest_found = this_width;
            safe_width = this_width;
            safe_x = slot.x;
        }

        match slot.segment_id {
            None => {
                // 没有更多浮动对象影响，之后的y都是满宽，无需继续探测。
                break;
            }
            Some(seg_id) => {
                // 用当前 segment 的高度信息继续向下走：
                // find_content_slot 返回的 slot.y 是该 segment 的起点，
                // slot.height 通常是 f32::INFINITY（见 taffy 源码），
                // 无法直接从 slot 本身得知这个 segment 在哪里结束。
                // 因此改用 after=Some(seg_id) 让 find_content_slot
                // 内部的 hwm 逻辑跳到下一个 segment，用它返回的
                // slot.y（下一个 segment 的起点）判断是否已经越过 y_end。
                let next = fctx.find_content_slot(probe_y, [0.0, 0.0], Clear::None, Some(seg_id));
                if next.segment_id.is_none() {
                    // 下一个 segment 不存在，说明当前 segment 已经是最后一个
                    // 有浮动影响的区间，之后满宽，无需继续。
                    break;
                }
                if next.y <= probe_y {
                    // 防御：理论上 next.y 应该严格大于 probe_y（向下推进），
                    // 若未推进（异常情况），主动退出避免死循环。
                    break;
                }
                probe_y = next.y;
                after = Some(seg_id);
            }
        }
        probed += 1;
    }

    (safe_width, safe_x)
}

use roe_model::writer::{BodyElement, Run, WriterDocument, ScriptPosition};
use roe_model::common::CellContent;
use roe_model::common::Color;

use crate::font_registry::FontRegistry;
use crate::tile::RunBrush;
use crate::frame::{FrameArena, LineBox, FrameContent, Margins, TableCellFrame};
use crate::page_fit::PageFitChecker;
use crate::split::{move_frame_to_new_page, split_frame};
use crate::widow_orphan::{ResolveResult, WidowOrphanResolver};

/// 表格样式移植 Layer 1步骤B（渲染部分）：单元格边框颜色/宽度渲染用最终值
/// 合并逻辑，从 layout_impl 内联逻辑抽出为独立可测试的小函数。cell 自身的
/// border_color/width（docx.rs w:tcBorders 解析写入，None 表示原始XML该cell
/// 没有自己的 w:tcBorders）优先，没有时 fallback 到表格级默认边框（docx.rs
/// w:tblBorders 解析写入的 Table.default_border_color/width）。两者都是
/// None 时，函数返回 (None, None)——调用方（tile.rs 渲染循环）自行兜底黑色/
/// 1.0px，不是本函数职责。不回写 model 层 TableCell，只用于计算渲染专用的
/// 中间值（TableCellFrame.border_color/width），保持 model 层原始解析语义
/// 不被覆盖（关系到未来若做脚本读取-编辑-回写场景的保真度，见
/// ROE_DOCX_Progress_v64.md 二节"核心原则"）。
fn resolve_cell_border(
    cell: &roe_model::common::TableCell,
    table: &roe_model::common::Table,
) -> (Option<Color>, Option<f32>) {
    (
        cell.border_color.or(table.default_border_color),
        cell.border_width.or(table.default_border_width),
    )
}

/// WROOT-100（表格边框BorderStyle重构）：判断表格某条cell边是"外框"还是
/// "内部线"，并据此算出这条边的完整 BorderStyle。
///
/// 外框（is_edge=true）：颜色/宽度用 resolve_cell_border（cell自身优先，
/// fallback到table.default_border_color/width），与Layer 1-B已验证行为
/// 完全一致，dashed恒为false（外框我们从未见过要虚线的场景）。
///
/// 内部线（is_edge=false）：颜色/宽度优先用 inside_color/inside_width
/// 参数（调用方传入table.inside_h_color/width或inside_v_color/width，
/// 横线/竖线各自独立）；若这两者都是None（原始docx未指定insideH/insideV
/// 具体颜色宽度，不区分"未写"还是"w:val=none"，两种情况在docx.rs解析层
/// 就已经统一变成None，语义上等价），画灰色虚线兜底（0xA6A6A6，1.0pt，
/// dashed=true），这是本次会话用户明确拍板的产品设计决策（不追求和
/// MS365/LibreOffice"insideH=none就完全不画线"的行为一致，我们希望内部
/// 网格线视觉上始终可见、给用户清晰的单元格边界引导，参照roe-impress
/// draw_placeholder_dashed_rect占位符虚线框的视觉语言）。
///
/// 返回 None 的唯一情况：is_edge=false 且外部调用方决定这条线不该画
/// （目前没有这种调用场景，预留language，本函数自身总是返回Some）。
fn resolve_table_border_side(
    is_edge: bool,
    cell: &roe_model::common::TableCell,
    table: &roe_model::common::Table,
    inside_color: Option<Color>,
    inside_width: Option<f32>,
) -> Option<crate::frame::BorderStyle> {
    const DASH_FALLBACK_GRAY: Color = Color { r: 0xA6, g: 0xA6, b: 0xA6, a: 255 };
    const DASH_FALLBACK_WIDTH: f32 = 1.0;

    if is_edge {
        let (color, width) = resolve_cell_border(cell, table);
        Some(crate::frame::BorderStyle {
            color: color.unwrap_or(Color { r: 0, g: 0, b: 0, a: 255 }),
            width: width.unwrap_or(1.0),
            dashed: false,
        })
    } else {
        match (inside_color, inside_width) {
            (Some(c), w) => Some(crate::frame::BorderStyle {
                color: c,
                width: w.unwrap_or(1.0),
                dashed: false,
            }),
            (None, _) => Some(crate::frame::BorderStyle {
                color: DASH_FALLBACK_GRAY,
                width: DASH_FALLBACK_WIDTH,
                dashed: true,
            }),
        }
    }
}

/// 把一组 Run（同一样式来源，如一个表格 cell 的内容）shape 成真正换行的
/// Vec<LineBox>。从段落主路径的 D3 shaping 逻辑抽取而来的简化版本：
/// 表格 cell 场景不需要 run 级别的 metadata 切分追踪（cell 目前只取纯文本
/// 拼接，尚不支持 cell 内多样式渲染，与重构前行为一致，记入已知限制），
/// 只需要"给定宽度，跑出多行文字"这个核心能力。
///
/// width: 换行宽度（pt）
/// font_size: 字号（pt）
/// line_height: 行距相对字号的倍数（债务A修复：调用方应传入该cell段落
/// 自身的 line_height，取值来源与正文段落一致——docx.rs 解析的
/// w:line/240.0。cell 路径目前只读段落自身值，暂不接样式表继承链
/// （与本函数文档注释中"cell场景简化"的既有设计保持一致，记入已知
/// 限制，后续如需支持再扩展）。
/// WROOT-101（表格浮动绕排 第一步）：预测表格总高度（pt），供前置扫描阶段
/// 把表格注册进 taffy::FloatContext 用（注册浮动区域必须先知道height，但
/// 真实排版要等表格实际排完才知道多少行——这是"先有鸡还是先有蛋"问题，此
/// 函数就是解决它的"先测量一遍"方案）。
///
/// **重要：这是方案A（简化版独立测量函数），不是从主循环表格分支重构提取
/// 出来的公共函数**——本次会话（v67）与用户明确讨论过方案A vs 方案B：
/// - 方案A（本函数采用）：复制主循环表格分支的核心测量逻辑（列宽分配+
///   逐cell shape_text_to_lines+按行取最大高度累加），但只保留累加高度
///   这一条主线，不产出TableCellFrame、不处理缩进/悬挂缩进/段落间距等
///   边界情况的精细对齐、不插入arena。风险低，但与主循环表格分支
///   （下方约450-620行附近，`if let BodyElement::Table(tbl) = elem`分支）
///   存在逻辑重复，两处需要分别维护。
/// - 方案B（更彻底）：重构主循环表格分支，拆出一个"只算高度"和"产出
///   TableCellFrame"共用的核心计算函数，只有一份逻辑。未采用的原因：
///   主循环表格分支是历史上改过至少5轮的高风险代码（注释里的"BugB
///   修复/续修/三修/四修/五修"，专门处理缩进、悬挂缩进、x_offset与
///   text_x_offset分离等边界情况），这次任务的主线是"表格绕排"而非
///   "重构测量逻辑"，不希望在同一个改动里引入历史高风险代码的重构
///   风险，导致绕排新bug和重构回归难以互相区分排查。
///
/// **后续维护提醒**：如果发现本函数算出的高度与主循环真实排版结果经常
/// 对不上（比如cell内有复杂缩进/多段落场景导致测量值偏差较大，进而使
/// FloatContext里注册的浮动区域偏小/偏大，绕排效果不准），说明简化测量
/// 的精度已经不够用，届时应该考虑重构为方案B，而不是继续在本函数里
/// "打补丁式"地把主循环那五轮修复的边界情况逐条搬过来抄一遍——那样做
/// 只会让两份逻辑更加重复膨胀、更难同步维护，本函数的"简化"价值也会
/// 随之丧失。
///
/// 已知简化（相对主循环表格分支，故意不做的部分）：
/// - 不处理段落缩进（indent_left/indent_first/悬挂缩进）对换行宽度的
///   影响，测量时统一用cell完整可写宽度（effective_cw），可能导致有
///   深层级缩进列表的cell测量高度偏低（真实排版换行更多、更高）。
/// - 不处理列宽的gridSpan/合并单元格特殊情况，与主循环当前实现保持
///   一致的简化程度（WROOT-099已知限制，非本函数引入的新简化）。
fn measure_table_height(
    registry: &mut FontRegistry,
    tbl: &roe_model::common::Table,
    content_w: f32,
) -> f32 {
    let font_size = registry.default_font_size;
    let cell_pad = 4.0_f32;

    let col_widths: Vec<f32> = if tbl.col_widths.is_empty() {
        let n = tbl.rows.first().map(|r| r.cells.len()).unwrap_or(1).max(1);
        vec![content_w / n as f32; n]
    } else {
        let total: f32 = tbl.col_widths.iter().sum();
        if total > 0.0 {
            tbl.col_widths.iter().map(|w| w / total * content_w).collect()
        } else {
            let n = tbl.col_widths.len().max(1);
            vec![content_w / n as f32; n]
        }
    };
    let num_cols = col_widths.len();

    const CELL_PARAGRAPH_SPACING: f32 = 2.0;
    let mut total_height = 0.0_f32;

    for row in tbl.rows.iter() {
        let mut row_height = 0.0_f32;
        for (ci, cell) in row.cells.iter().enumerate() {
            let cw = col_widths.get(ci).copied().unwrap_or(content_w / num_cols as f32);
            let effective_cw = (cw - cell_pad).max(cw * 0.3);

            let mut cell_height = 0.0_f32;
            for ce in cell.content.iter() {
                let roe_model::common::CellContent::Paragraph(p) = ce else { continue };
                if p.runs.is_empty() {
                    cell_height += font_size * 1.3;
                    continue;
                }
                let cell_line_height = p.line_abs_pt.filter(|_| p.line_exact.is_some()).map(|a| a / font_size.max(1.0)).or(p.line_height).unwrap_or(1.3);
                let para_lines = shape_text_to_lines(
                    registry, &p.runs, effective_cw, font_size, cell_line_height,
                );
                if para_lines.is_empty() {
                    cell_height += font_size * 1.3;
                    continue;
                }
                // WROOT-101修复：此前这里把 CELL_PARAGRAPH_SPACING 累加进
                // cell_height，但主循环真实排版时 place_table_row 计算的
                // row_height（决定这一行实际占用/渲染高度的唯一权威值）
                // 只对 lines 的 line_height 求和，不含这个间距——两处口径
                // 不一致，导致每有一个段落就多预估 CELL_PARAGRAPH_SPACING，
                // 8行×每格2段落的样本累积偏差达32pt，taffy据此注册的浮动
                // 避让区域比表格真实渲染范围明显偏高，视觉上表现为表格
                // 下方多出一截文字本该环绕却空出的留白。改为与 row_height
                // 同口径：只累加 line_height，不再重复加间距。
                let para_height: f32 = para_lines.iter().map(|l| l.line_height).sum();
                cell_height += para_height;
            }
            // WROOT-104：与place_table_row/TableCellFrame构造处（约1475行）
            // 同款兜底，口径必须一致——该函数是简化预估路径，不走
            // TableCellFrame，需要在这里单独补一次，否则空单元格表格
            // 在此函数里预估出的total_height仍是0，即使真实排版路径
            // 已经不再塌缩，两处口径分裂会重新引入WROOT-101类偏差。
            if cell_height <= 0.0 {
                cell_height = font_size * 1.3;
            }
            row_height = row_height.max(cell_height);
        }
        total_height += row_height;
    }

    total_height
}

fn shape_text_to_lines(
    registry: &mut FontRegistry,
    runs: &[Run],
    width: f32,
    font_size: f32,
    line_height: f32,
) -> Vec<LineBox> {
    shape_text_to_lines_impl(registry, runs, width, font_size, line_height, None, None, Alignment::Start)
}

/// v317c：styles 为 Some 时逐 run 应用样式并给每行挂 LineRunSpan（单元格段落用）
fn shape_text_to_lines_impl(
    registry: &mut FontRegistry,
    runs: &[Run],
    width: f32,
    font_size: f32,
    line_height: f32,
    styles: Option<&[ResolvedRunStyle]>,
    families: Option<&[Option<String>]>,
    v317h_align: Alignment,
) -> Vec<LineBox> {
    // WROOT-025 第三场景修复（表格cell内tab）：与 layout_impl 主循环
    // （990行附近）同样的 tofu 方框问题，同样的 expand_tabs_to_spaces
    // 方案。这里所有 run 共用同一个 font_size 参数（不同于主循环需要
    // resolve_run_style 逐 run 查字号），所以不需要按 run 重新解析字号，
    // 直接用传入的 font_size 即可。tab_cur_x 是本函数（即本次
    // shape_text_to_lines 调用，通常对应一个 cell 内一个段落）级别的
    // 累积作用域，跨 run 累积。
    let mut tab_cur_x = 0.0f32;
    let expanded_run_texts: Vec<String> = runs.iter().map(|r| {
        if !r.text.contains('\t') {
            if !r.text.is_empty() {
                tab_cur_x += registry.measure_text_width(&r.text, font_size);
            }
            return r.text.clone();
        }
        let tab_stop_pt = 72.0f32; // OOXML defTabSz 默认值
        let space_w = registry.measure_text_width(" ", font_size).max(0.01);
        let tab_count = r.text.matches('\t').count();
        let mut out = String::with_capacity(r.text.len());
        for (i, segment) in r.text.split('\t').enumerate() {
            out.push_str(segment);
            if !segment.is_empty() {
                tab_cur_x += registry.measure_text_width(segment, font_size);
            }
            if i < tab_count {
                let next_stop = ((tab_cur_x / tab_stop_pt).floor() + 1.0) * tab_stop_pt;
                let gap = (next_stop - tab_cur_x).max(0.0);
                let n_spaces = (gap / space_w).round().max(1.0) as usize;
                for _ in 0..n_spaces {
                    out.push(' ');
                }
                tab_cur_x = next_stop;
            }
        }
        out
    }).collect();
    let text: String = expanded_run_texts.join("");
    if text.trim().is_empty() {
        return Vec::new();
    }

    // Parley 迁移：line_height 参数原本是"相对字号的倍数"（cosmic-text
    // Metrics::relative 语义），Parley 的 LineHeight::Absolute 要求传入
    // 绝对 pt 值，这里手动换算一次，其余逻辑不变。
    let v317_eff_fs = match styles {
        Some(sts) => {
            let m = sts.iter().zip(runs.iter()).filter(|(_, r)| !r.text.is_empty()).map(|(s, _)| s.font_size).fold(0.0f32, f32::max);
            if m > 0.0 { m } else { font_size }
        }
        None => font_size,
    };
    let abs_line_height = v317_eff_fs * line_height;

    let default_brush = RunBrush { color: [0, 0, 0, 255], run_idx: None };
    let mut builder = registry.layout_context.ranged_builder(
        &mut registry.font_context, &text, 1.0, true,
    );
    builder.push_default(StyleProperty::Brush(default_brush));
    builder.push_default(GenericFamily::SystemUi);
    builder.push_default(LineHeight::Absolute(abs_line_height));
    builder.push_default(StyleProperty::FontSize(font_size));

    let mut v317_ranges: Vec<(usize, usize)> = Vec::new();
    if let Some(sts) = styles {
        let mut off = 0usize;
        for (i, rt) in expanded_run_texts.iter().enumerate() {
            let (s0, e0) = (off, off + rt.len());
            off = e0;
            v317_ranges.push((s0, e0));
            if s0 >= e0 { continue; }
            let Some(rs) = sts.get(i) else { continue };
            let col = rs.color.map(|c| [c.r, c.g, c.b, c.a]).unwrap_or([0, 0, 0, 255]);
            builder.push(StyleProperty::Brush(RunBrush { color: col, run_idx: Some(i) }), s0..e0);
            builder.push(StyleProperty::FontSize(rs.font_size), s0..e0);
            if let Some(Some(f)) = families.and_then(|v| v.get(i)) {
                builder.push(StyleProperty::FontFamily(parley::FontFamily::named(f.as_str())), s0..e0);
            }
            if rs.bold { builder.push(StyleProperty::FontWeight(FontWeight::BOLD), s0..e0); }
            if rs.italic { builder.push(StyleProperty::FontStyle(FontStyle::Italic), s0..e0); }
        }
    }

    let mut layout: parley::Layout<RunBrush> = builder.build(&text);
    layout.break_all_lines(Some(width.max(1.0)));
    layout.align(v317h_align, AlignmentOptions::default());

    let mut lines = Vec::new();
    let mut y = 0.0f32;
    for line in layout.lines() {
        let line: parley::Line<'_, RunBrush> = line;
        let lm = line.metrics();
        let line_text_range = line.text_range();
        let line_text = text.get(line_text_range.clone()).unwrap_or("").to_string();
        let mut v317_spans: Vec<crate::frame::LineRunSpan> = Vec::new();
        if let Some(sts) = styles {
            for (i, &(rs0, re0)) in v317_ranges.iter().enumerate() {
                let a = rs0.max(line_text_range.start);
                let b = re0.min(line_text_range.end);
                if a >= b { continue; }
                let Some(st) = sts.get(i) else { continue };
                v317_spans.push(crate::frame::LineRunSpan {
                    start: a - line_text_range.start,
                    end: b - line_text_range.start,
                    style: *st,
                    run_idx: Some(i),
                    formula_box: None,
                });
            }
        }

        lines.push(LineBox {
            y_offset:    y,
            ascent:      lm.baseline - lm.content_block_min_coord,
            descent:     lm.content_block_max_coord - lm.baseline,
            line_height: lm.line_height,
            text:        line_text,
            runs:        v317_spans,
            text_x_offset: lm.offset,
            ..Default::default()
        });
        y += lm.line_height;
    }

    if lines.is_empty() {
        lines.push(LineBox {
            y_offset: 0.0,
            ascent: font_size,
            descent: font_size * 0.2,
            line_height: font_size * 1.3,
            text,
            runs: Vec::new(),
            ..Default::default()
        });
    }

    lines
}

/// 布局引擎输出：整个文档的 Frame Arena
pub struct LayoutResult {
    pub arena: FrameArena,
}

pub struct ParagraphLayoutEngine {
    pub registry:    FontRegistry,
    pub page_width:  f32,
    pub page_height: f32,
    pub margins:     Margins,
    pub fit_checker: PageFitChecker,
    pub widow_resolver: WidowOrphanResolver,
}

impl ParagraphLayoutEngine {
    pub fn new(registry: FontRegistry) -> Self {
        Self {
            registry,
            page_width:  595.0,
            page_height: 842.0,
            margins:     Margins::default(),
            fit_checker:     PageFitChecker::new(),
            widow_resolver:  WidowOrphanResolver::new(),
        }
    }

    pub fn layout(&mut self, doc: &WriterDocument) -> LayoutResult {
        // WROOT-ANCHOR-01：图片 relativeFrom="paragraph"/"character" 时，
        // y_pt 是相对锚点段落的偏移，需要先跑一遍 dry run 拿到每个段落
        // 排版后的真实 y 坐标，才能正确换算。没有此类图片的文档零额外
        // 开销，走原有单遍路径。
        // WROOT-ANCHOR-01: 判断"y坐标是否相对锚点段落"要看V方向
        // (anchor_type_v)，不是H方向——H/V语义独立，只有V决定y坐标基准。
        let has_paragraph_relative_frame = doc.body.iter().any(|e| {
            matches!(e, BodyElement::Frame(ff) if matches!(
                ff.anchor_type_v.as_deref(),
                Some("paragraph") | Some("character")
            ))
        });

        let anchor_y_map: Option<std::collections::HashMap<usize, f32>> = if has_paragraph_relative_frame {
            // WROOT-ANCHOR-03 修复：anchor_y_map 必须记录段落的"起始"y坐标
            // (para_y_start，段落排版前、文字第一行的顶部)，不能用
            // arena.frames 里的 f.y_on_page —— 后者是该段落排完所有行之后
            // 的位置（约等于段落底部/下一元素起点），两者相差整段文字的
            // 高度。之前误用 f.y_on_page 导致 relativeFrom="paragraph" 的
            // posOffset 从"段落底部"而非"段落顶部"起算，图片被系统性地
            // 多推下去一整段高度，压住了后面1-2个段落而非贴合自己锚点段落。
            let mut para_y_start_map = std::collections::HashMap::new();
            let _dry_result = self.layout_impl(doc, true, None, Some(&mut para_y_start_map));
            Some(para_y_start_map)
        } else {
            None
        };

        self.layout_impl(doc, false, anchor_y_map.as_ref(), None)
    }

    /// WROOT-068 渲染算法本体：LOK PostFormat 两阶段算法的第二遍。
    /// 对断行完成后的某一行文字，结合本行内的 tab 占位标记（tab_markers_in_line，
    /// 已转换为行内相对字节偏移，按顺序排列）与段落的 tab_stops 定义，
    /// 计算每个 tab 应撑大的宽度（累积右移量），驱动 tile.rs 画字时的 x 偏移，
    /// 并生成引导线绘制信息。返回 (tab_adjustments, tab_leaders)，均已按
    /// LineBox/ShapedLine 要求的格式就绪，调用方直接塞进 LineBox 构造即可。
    ///
    /// 参数：
    /// - line_text: 本行文字（与 LineBox.text 完全一致的切片内容）
    /// - tab_markers_in_line: 落在本行的 tab 标记列表，(rel_offset, run_idx, tab_seq)，
    ///   按 rel_offset 升序（调用方保证，通常天然有序无需再排）
    /// - tab_stops: para.tab_stops，按 pos 升序
    /// - font_size_for_run: 闭包，给定 run_idx 返回该 run 的字号（pt），
    ///   用于测量宽度；找不到时调用方应兜底传段落默认字号
    /// - registry: 字体测宽器（&mut，因为 measure_text_width 需要可变借用）
    fn compute_tab_adjustments(
        line_text: &str,
        tab_markers_in_line: &[(usize, usize, usize)], // (rel_offset, run_idx, tab_seq)
        tab_stops: &[roe_model::writer::TabStop],
        font_size_for_run: impl Fn(usize) -> f32,
        registry: &mut crate::font_registry::FontRegistry,
    ) -> (Vec<crate::frame::TabAdjustment>, Vec<crate::frame::TabLeaderSegment>) {
        let mut adjustments: Vec<crate::frame::TabAdjustment> = Vec::new();
        let mut leaders: Vec<crate::frame::TabLeaderSegment> = Vec::new();
        if tab_markers_in_line.is_empty() || tab_stops.is_empty() {
            return (adjustments, leaders);
        }

        let mut cumulative_extra = 0.0f32;

        for (idx, &(rel_offset, run_idx, tab_seq)) in tab_markers_in_line.iter().enumerate() {
            let fs = font_size_for_run(run_idx);
            // WROOT-068: 占位符本身是1个半角空格，其宽度需要单独测量（不同
            // 字号/字体下不是恒定值），用于算 x_start（撑大前的tab终点）。
            let placeholder_w = registry.measure_text_width(" ", fs).max(0.01);

            // fix_x：这个tab之前所有文字的累积宽度（含之前tab造成的累积右移）。
            // rel_offset 是占位空格本身的字节位置，[0, rel_offset) 是tab之前的
            // 文字内容，测这段宽度即可，不含占位空格自身。
            let text_before = line_text.get(0..rel_offset).unwrap_or("");
            let text_before_w = if text_before.is_empty() { 0.0 } else {
                registry.measure_text_width(text_before, fs)
            };
            let fix_x = text_before_w + cumulative_extra;

            // tab后内容范围：占位空格之后（rel_offset + 1字节）到下一个tab
            // 标记（如果有）或行尾。
            let content_start = rel_offset + 1; // ' ' 是1字节
            let content_end = tab_markers_in_line.get(idx + 1)
                .map(|&(next_rel, _, _)| next_rel)
                .unwrap_or(line_text.len());
            let content_end = content_end.max(content_start).min(line_text.len());
            let tab_content = line_text.get(content_start..content_end).unwrap_or("");
            let n_por_width = if tab_content.is_empty() { 0.0 } else {
                registry.measure_text_width(tab_content, fs)
            };

            let tab_stop = &tab_stops[tab_seq.min(tab_stops.len() - 1)];
            let n_right = tab_stop.pos;
            let n_diff_width = n_right - fix_x;
            eprintln!("[WROOT068-DEBUG2] rel_offset={} text_before={:?} text_before_w={:.3} fix_x={:.3} placeholder_w={:.3} content_start={} content_end={} tab_content={:?} n_por_width={:.3} n_right={:.3} n_diff_width={:.3}",
                rel_offset, text_before, text_before_w, fix_x, placeholder_w, content_start, content_end, tab_content, n_por_width, n_right, n_diff_width);

            // LOK txttab.cxx PostFormat 核心公式（三种对齐 + 降级保护）。
            // 修正记录：x_end（撑大后tab的终点，即文字实际起笔位置）必须满足
            // x_end + n_por_width == n_right（文字右边缘精确落在目标位置），
            // 而 x_end = fix_x + placeholder_w + extra_width，所以
            // extra_width = n_right - n_por_width - fix_x - placeholder_w
            // ——比"教科书式"LOK公式（n_diff_width - n_por_width）多减一个
            // placeholder_w，因为我们的tab起点坐标(fix_x)是"文字前"位置，
            // 中间还隔着一个已经占了placeholder_w宽度的占位空格，这个宽度
            // 也要从"应该撑大的量"里刨除，否则会多算一次，导致x_end比
            // n_right多缩进placeholder_w（本次实测就是这个bug：页码没贴
            // 到右边距，差值正好约等于一个空格宽）。
            // 目标空间不够时不撑大，避免产生负宽度/文字反而更靠左的荒谬结果，
            // 文字紧跟在tab最小宽度占位符后面（extra_width=0）。
            let extra_width = match tab_stop.alignment {
                roe_model::writer::TabAlignment::Right => {
                    let want = n_diff_width - n_por_width - placeholder_w;
                    if want > 0.0 { want } else { 0.0 }
                }
                roe_model::writer::TabAlignment::Center => {
                    let half = n_por_width / 2.0;
                    let want = n_diff_width - half - placeholder_w;
                    if want > 0.0 { want } else { 0.0 }
                }
                roe_model::writer::TabAlignment::Left => {
                    if n_diff_width > 0.0 { n_diff_width } else { 0.0 }
                }
            };

            cumulative_extra += extra_width;

            adjustments.push(crate::frame::TabAdjustment {
                byte_offset: rel_offset,
                cumulative_extra,
            });

            if tab_stop.leader != roe_model::writer::TabLeader::None {
                let x_start = fix_x + placeholder_w;
                let x_end = x_start + extra_width;
                if x_end > x_start {
                    leaders.push(crate::frame::TabLeaderSegment {
                        x_start,
                        x_end,
                        leader: tab_stop.leader,
                    });
                }
            }
        }

        (adjustments, leaders)
    }

    /// [WROOT-footnote-budget Patch2] 给定一个 note（footnote/endnote 内容），
    /// 断行生成完整的 Vec<LineBox>（note 内所有段落的行拼接在一起，
    /// 段落序号信息不保留——本函数只用于"预估高度"和"实际渲染"两处
    /// 共用的纯计算逻辑，不产生任何 arena 副作用，不调用 place_paragraph）。
    ///
    /// 与原 layout_impl 内联版本（Patch 3 遗留代码）行为完全对齐：
    /// - 第一个段落的第一行会带上编号标签（number_label）作为独立的
    ///   带样式 LineRunSpan（run_idx: None，因为它是渲染器合成的文字，
    ///   不对应 para.runs 里的任何一个 run）。
    /// - 每个段落独立按 hard_segments（\n 切分）+ Parley 断行处理。
    /// - 每个段落如果断行结果为空（空段落），补一个空行占位。
    ///
    /// 返回：Vec<(段落自身产生的 lines, 该段落是否需要在 place_paragraph
    /// 时使用 space_after=4.0)> —— 实际上所有段落的 space_after 目前都
    /// 固定是 4.0，这里仍然按"每段落一组"返回而不是展平成一个大 Vec，
    /// 是因为调用方（无论是预估高度还是真实渲染）都需要按段落分别调用
    /// place_paragraph（现状如此，未改变这个调用粒度）。
    fn build_footnote_note_lines(
        &mut self,
        note: &roe_model::writer::Note,
        number_label: &str,
        footnote_font_size: f32,
        footnote_line_height: f32,
        content_w: f32,
        doc: &roe_model::writer::WriterDocument,
    ) -> Vec<Vec<LineBox>> {
        use roe_model::writer::BodyElement;
        use roe_model::writer::ScriptPosition;
        use crate::layout::ResolvedRunStyle;
        use parley::{StyleProperty, GenericFamily, LineHeight, Alignment, AlignmentOptions, YieldData};
        use crate::tile::RunBrush;

        let newline_char = char::from_u32(10).unwrap();
        let tab_char = char::from_u32(9).unwrap();

        let mut result: Vec<Vec<LineBox>> = Vec::new();
        let mut is_first_para = true;

        for body_elem in &note.content {
            let BodyElement::Paragraph(p) = body_elem else { continue };

            let para_margin_left = p.indent_left.unwrap_or(0.0).max(0.0);
            let para_indent_first = p.indent_first.unwrap_or(0.0);
            let hanging_indent_pt = para_margin_left;
            let footnote_effective_w = (content_w - hanging_indent_pt).max(content_w * 0.3);

            let mut raw_text = String::new();
            let mut note_label_byte_len: usize = 0;
            if is_first_para {
                raw_text.push_str(number_label);
                note_label_byte_len = number_label.len();
            }
            for r in &p.runs {
                raw_text.push_str(&r.text);
            }
            let raw_text = raw_text.replace(tab_char, " ");
            let hard_segments: Vec<&str> = raw_text.split(newline_char).collect();
            let hard_segments: Vec<&str> = if hard_segments.is_empty() {
                vec![""]
            } else {
                hard_segments
            };

            let mut lines: Vec<LineBox> = Vec::new();
            let mut running_line_idx: usize = 0;
            for segment in hard_segments.iter() {
                let seg_text = (*segment).to_string();
                let default_brush = RunBrush { color: [0, 0, 0, 255], run_idx: None };
                let mut seg_builder = self.registry.layout_context.ranged_builder(
                    &mut self.registry.font_context, &seg_text, 1.0, true,
                );
                seg_builder.push_default(StyleProperty::Brush(default_brush));
                seg_builder.push_default(GenericFamily::SystemUi);
                seg_builder.push_default(LineHeight::Absolute(footnote_line_height));
                seg_builder.push_default(StyleProperty::FontSize(footnote_font_size));
                let mut seg_layout: parley::Layout<RunBrush> = seg_builder.build(&seg_text);
                {
                    let mut seg_breaker = seg_layout.break_lines();
                    let seg_state = seg_breaker.state_mut();
                    seg_state.set_layout_max_advance(footnote_effective_w);
                    seg_state.set_line_max_advance(footnote_effective_w);
                    loop {
                        let Some(yield_data) = seg_breaker.break_next() else { break; };
                        if let YieldData::LineBreak(_) = yield_data {
                            let seg_state = seg_breaker.state_mut();
                            seg_state.set_line_max_advance(footnote_effective_w);
                        }
                    }
                    seg_breaker.finish();
                }
                seg_layout.align(Alignment::Start, AlignmentOptions::default());
                for seg_line in seg_layout.lines() {
                    let seg_line: parley::Line<'_, RunBrush> = seg_line;
                    let range = seg_line.text_range();
                    let text = seg_text.get(range).unwrap_or("").to_string();
                    let is_note_head = is_first_para && running_line_idx == 0;
                    let this_line_x_offset = if running_line_idx == 0 {
                        (para_margin_left + para_indent_first).max(0.0)
                    } else {
                        para_margin_left
                    };
                    let mut line = LineBox {
                        y_offset: running_line_idx as f32 * footnote_line_height,
                        ascent: footnote_font_size,
                        descent: footnote_font_size * 0.2,
                        line_height: footnote_line_height,
                        text,
                        runs: Vec::new(),
                        x_offset: this_line_x_offset,
                        text_x_offset: this_line_x_offset,
                        ..Default::default()
                    };
                    if is_note_head && note_label_byte_len > 0 {
                        let mut label_color: Option<roe_model::common::Color> = None;
                        let mut label_font_size = footnote_font_size;
                        let mut label_script = ScriptPosition::Normal;
                        if let Some(BodyElement::Paragraph(fp0)) = note.content.first() {
                            if let Some(fr0) = fp0.runs.first() {
                                if matches!(fr0.script, Some(ScriptPosition::Superscript)) { label_script = ScriptPosition::Superscript; }
                            }
                        }
                        if let Some(first_para_elem) = note.content.first() {
                            if let BodyElement::Paragraph(first_para) = first_para_elem {
                                if let Some(first_run) = first_para.runs.first() {
                                    if let Some(style_ref) = &first_run.char_style {
                                        let cs = doc.styles.resolve_char_style(style_ref.0.as_str());
                                        label_color = cs.color;
                                        if let Some(sz) = cs.font_size {
                                            label_font_size = sz;
                                        }
                                        if matches!(cs.script, Some(ScriptPosition::Superscript)) {
                                            label_script = ScriptPosition::Superscript;
                                        } else if matches!(cs.script, Some(ScriptPosition::Subscript)) {
                                            label_script = ScriptPosition::Subscript;
                                        }
                                    }
                                }
                            }
                        }
                        line.runs.push(crate::frame::LineRunSpan {
                            start: 0,
                            end: note_label_byte_len,
                            style: ResolvedRunStyle {
                                font_size: label_font_size,
                                font_family_is_set: false,
                                bold: false,
                                italic: false,
                                underline: false,
                                strikethrough: false,
                                color: label_color,
                                background: None,
                                script: label_script,
                                is_formula: false,
                                comment_highlight_color_idx: None,
                                comment_bar_start: false,
                                comment_bar_end: false,
                                comment_id: None,
                                vertical_position: None,
                            },
                            run_idx: None,
                            formula_box: None,
                        });
                    }
                    lines.push(line);
                    running_line_idx += 1;
                }
            }
            if lines.is_empty() {
                lines.push(LineBox {
                    y_offset: 0.0,
                    ascent: footnote_font_size,
                    descent: footnote_font_size * 0.2,
                    line_height: footnote_line_height,
                    text: String::new(),
                    runs: Vec::new(),
                    x_offset: para_margin_left,
                    text_x_offset: para_margin_left,
                    ..Default::default()
                });
            }
            is_first_para = false;
            result.push(lines);
        }

        result
    }

    /// [WROOT-footnote-budget Patch2] 预估一个 note 在页面上完整渲染后
    /// 占用的高度（不含分隔线，分隔线是页级别只插一次，由调用方另行
    /// 加总）。复用 build_footnote_note_lines 保证与实际渲染同一套断行
    /// 逻辑，避免"预估"和"实际"用两份不同代码导致数值对不上。
    ///
    /// 公式来源：used_height_in_column 对每个链头+链尾 frame 的贡献是
    /// space_before + height() + space_after；footnote 每个段落调用
    /// place_paragraph 时固定传 space_before=0.0, space_after=4.0
    /// （见 layout_impl 内 self.place_paragraph 调用处），height() 对
    /// Paragraph variant 是 sum(line_height)。所以：
    /// note总高度 = sum(每段落: sum(该段落各行line_height) + 4.0)
    fn estimate_footnote_note_height(
        &mut self,
        note: &roe_model::writer::Note,
        number_label: &str,
        footnote_font_size: f32,
        footnote_line_height: f32,
        content_w: f32,
        doc: &roe_model::writer::WriterDocument,
    ) -> f32 {
        const FOOTNOTE_PARA_SPACE_AFTER: f32 = 4.0;
        let paragraphs = self.build_footnote_note_lines(
            note, number_label, footnote_font_size, footnote_line_height, content_w, doc,
        );
        paragraphs.iter()
            .map(|lines| {
                let sum_line_height: f32 = lines.iter().map(|l| l.line_height).sum();
                sum_line_height + FOOTNOTE_PARA_SPACE_AFTER
            })
            .sum()
    }

    fn layout_impl(
        &mut self,
        doc: &WriterDocument,
        dry_run: bool,
        anchor_y_map: Option<&std::collections::HashMap<usize, f32>>,
        mut para_y_start_out: Option<&mut std::collections::HashMap<usize, f32>>,
    ) -> LayoutResult {
        let mut arena = FrameArena::new();
        arena.doc_columns = doc.columns.clone();

        // STEP6-STAGE-B：脚注/尾注编号的真实动态计数。按正文里
        // Run.endnote_ref/footnote_ref 首次出现的先后顺序编号（从1开始），
        // key 是 Note.id（对应 WriterDocument.endnotes/footnotes 里的
        // 那个 id，不是"第几次引用"）。同一个 note 若被正文引用多次，
        // 沿用相同的编号——当前样本集没有这种边缘情况，这是按直觉选的
        // 语义，遇到反例样本再回头调整（v135交接文档已记录这个决策）。
        // 两套计数完全独立（endnote 和 footnote 各自从1开始），不互相
        // 干扰，与 Word/OOXML 规范里两者是独立编号序列的语义一致。
        // 扫描顺序：按 doc.body 的段落顺序、段落内 run 的顺序线性扫描，
        // 这与 Parley/tile.rs 实际渲染文字的顺序一致，保证"编号顺序"
        // 与"视觉阅读顺序"相符（不依赖 Note.id 本身的大小或列表顺序）。
        let mut endnote_numbers: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
        let mut footnote_numbers: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
        {
            let mut next_endnote_num: u32 = 1;
            let mut next_footnote_num: u32 = 1;
            fn scan_body_elements(
                elements: &[BodyElement],
                endnote_numbers: &mut std::collections::HashMap<u32, u32>,
                footnote_numbers: &mut std::collections::HashMap<u32, u32>,
                next_endnote_num: &mut u32,
                next_footnote_num: &mut u32,
            ) {
                for el in elements {
                    if let BodyElement::Paragraph(p) = el {
                        for r in &p.runs {
                            if let Some(id) = r.endnote_ref {
                                endnote_numbers.entry(id).or_insert_with(|| {
                                    let n = *next_endnote_num;
                                    *next_endnote_num += 1;
                                    n
                                });
                            }
                            if let Some(id) = r.footnote_ref {
                                footnote_numbers.entry(id).or_insert_with(|| {
                                    let n = *next_footnote_num;
                                    *next_footnote_num += 1;
                                    n
                                });
                            }
                        }
                    }
                    // WROOT任务B: BodyElement::Section 等其它分支目前不含
                    // 段落文字，note-ref 只可能出现在 Paragraph 分支里，
                    // 与 expanded_run_texts 现有的处理范围保持一致。
                }
            }
            scan_body_elements(
                &doc.body,
                &mut endnote_numbers,
                &mut footnote_numbers,
                &mut next_endnote_num,
                &mut next_footnote_num,
            );
        }

        // [WROOT-047] 必须在 content_w/content_h 计算之前完成尺寸初始化——
        // 这两个派生变量是"可用排版高度/宽度"的唯一来源，后续所有换页/
        // 换栏的"放不放得下"判断全部基于它们，一旦算完就不会重新计算。
        // 此前 self.page_width/height/margins 从 new() 默认值（A4）开始，
        // 从未在正式排版前用文档第一个 section 的实际尺寸初始化过——多节
        // 文档第0节尺寸与A4不同时（本次样本是明信片尺寸），第一页会用
        // 错误（过大的A4）尺寸算出 content_h，导致本该放得下的内容被误判
        // 换页。BodyElement::Section 分支只处理"后续"的尺寸切换（第0节
        // 结束、第1节开始那一刻），完全没有覆盖"文档开头、第0节自己"这个
        // 时间点——补在这里，与 BodyElement::Section 分支是同一套 fallback
        // 逻辑（Some(section)取节自身值，None fallback到doc顶层默认值）。
        //
        // 踩坑记录：第一版补丁把这段初始化插在了 content_w/content_h 计算
        // 之后（"建第一页"之前），self.page_width/height 确实被更新了，
        // 但 content_h 早已用旧值算完，后续判断全程用的还是过时的可用
        // 高度，问题未被修复。教训：任何"更新 self 上尺寸相关字段"的
        // 补丁，必须确认所有从这些字段派生出的局部变量都在更新之后重新
        // 计算，不能只看字段本身有没有被赋新值。
        if let Some(section) = doc.sections.first() {
            self.page_width  = section.page_size.map(|s| s.0).unwrap_or(doc.page_size.0);
            self.page_height = section.page_size.map(|s| s.1).unwrap_or(doc.page_size.1);
            self.margins = Margins {
                top:    section.margin_top.unwrap_or(doc.margin_top),
                bottom: section.margin_bottom.unwrap_or(doc.margin_bottom),
                left:   section.margin_left.unwrap_or(doc.margin_left),
                right:  section.margin_right.unwrap_or(doc.margin_right),
            };
        } else {
            // v276: 无分节文档（如新建空白文档）直接用顶层页面设置
            self.page_width  = doc.page_size.0;
            self.page_height = doc.page_size.1;
            self.margins = Margins {
                top:    doc.margin_top,
                bottom: doc.margin_bottom,
                left:   doc.margin_left,
                right:  doc.margin_right,
            };
        }

        let content_w = self.page_width - self.margins.left - self.margins.right;
        // WROOT-134：改为 mut。节切换时（BodyElement::Section 分支）尺寸/边距会变，
        // content_h 必须同步重算，否则后续节的"放不放得下"判断仍用旧节的高度
        // （tdf166205：节1 top=15026 正文区仅 48pt，节2 空段被误判放不下，多出一页）。
        let mut content_h = self.page_height - self.margins.top - self.margins.bottom;

        // 建第一页
        let mut cur_page = arena.new_page(self.page_width, self.page_height, self.margins);
        // [WROOT-footnote-p1] 记录第一页的 PageId，用于后续判断某个
        // footnote_ref 是否'出现在第一页'。本次范围收窄为仅渲染第一页的
        // footnote 内容（见交接文档 v143 第三节），不追求真实的按页分布。
        // [WROOT-footnote-allpages] 每一页各自出现过哪些footnote_ref，
        // key是该段落place_paragraph调用后最终落地的PageId（可能与段落
        // 开始构造时的cur_page不同，因为place_paragraph内部可能翻页）。
        // 取代之前"只记第一页"的first_page_footnote_ids，本次扩展到
        // 所有页，用HashMap按页分组。
        let mut page_footnote_ids: std::collections::HashMap<roe_core::arena::PageId, std::collections::HashSet<u32>> = std::collections::HashMap::new();
        // [WROOT-任务B] 当前排版扫描到第几个 section（0-based）。body 里每
        // 遇到一个 BodyElement::Section 边界就 +1；每次建页（初建/跨页/
        // SHIFT_UP）都把当前值写进 arena.page_section_index，供渲染层查询
        // 这一页该用第几个 section 的 header/footer。
        let mut cur_section_index: usize = 0;
        // [WROOT-hf-parity] 与 cur_section_index 同步维护："这一页在当前
        // section 内排第几"（1-based）。每次真正跨 section 边界（下面
        // `cur_section_index += 1` 那一刻）重置为1；同一 section 内每次
        // 建新页则 +1。用一个小 helper（见下方 mark_new_page_section 闭包）
        // 统一维护，避免8处 insert 各自手写容易漏改。
        let mut cur_page_in_section: usize = 1;
        let mut section_just_changed = false;
        arena.page_section_index.insert(cur_page, cur_section_index);
        arena.page_index_in_section.insert(cur_page, cur_page_in_section);
        let mut cur_column: usize = 0;
        let mut list_counters: std::collections::HashMap<String, Vec<u32>> = std::collections::HashMap::new();
        // WROOT-093：每页一个 FloatContext，图片贴左/贴右时注册进对应页的
        // Context，段落断行时按当前 cur_page 查询该行可用宽度/起始x。
        // 用 PageId 做 key 是因为 cur_page 在跨页时会被 place_paragraph 等
        // 辅助函数通过 &mut 回写改变，同一页可能被多次访问，需要能查回同一个
        // Context 而不是重建。
        // WROOT-037-B：taffy::FloatContext::find_content_slot 的 ContentSlot
        // 只回答"给定的一个y点，可用宽度是多少"，无法查询"当前所在的这个
        // segment 在y方向上到哪结束"（ContentSlot.height 字段恒为
        // f32::INFINITY，taffy 源码 compute/float.rs 第505/517行硬编码）。
        // 当图片宽度超过版心、导致某个y区间可用宽度探测为负数（完全占满）
        // 且这已经是唯一/最后一个 segment 时，find_safe_content_slots 内部
        // 的探测循环无法从 taffy 反查出该往下跳到哪个y重试，只能自己在
        // 注册浮动框时把y范围额外记一份，供断行阶段在遇到负宽度时兜底查询。
        // key=页面，value=该页所有已注册浮动框的 (y_start, y_end) 区间列表，
        // 均已模拟 taffy place_floated_box_inner 内部对负 min_y 的钳制
        // （segments为空时 `start_y = last_y_end.max(start_y)`，即负数min_y
        // 会被钳成0.0，见 WROOT-037-B 实测记录），不能直接用未钳制的原始
        // real_float_y。
        // WROOT-037-GAP修正：float_y_ranges 记录的是"钳制后"taffy真实占用
        // 区间，专供断行阶段负宽度探测跳转使用（这部分必须用钳制值，因为
        // taffy内部真的是这么占用的）。但"1"视觉上该往下移多少来贴合图片
        // 真实底边，应该用图片真实的 real_float_y（未钳制，可能为负），
        // 否则"1"会被多推一个"|real_float_y|"的距离，图片和"1"之间凭空
        // 多出一截空白（本次会话现象）。故新增 visual_float_y_ranges，
        // 单独记录基于 real_float_y 算出的真实视觉底边，与 float_y_ranges
        // 结构一致，但数值不经过 clamp。
        let mut visual_float_y_ranges: std::collections::HashMap<roe_core::arena::PageId, Vec<(f32, f32)>> =
            std::collections::HashMap::new();
        let mut float_y_ranges: std::collections::HashMap<roe_core::arena::PageId, Vec<(f32, f32)>> =
            std::collections::HashMap::new();
        let mut float_contexts: std::collections::HashMap<roe_core::arena::PageId, FloatContext> =
            std::collections::HashMap::new();
        let mut last_font_size: f32 = self.registry.default_font_size; // 跟踪上一个非空段落的字号，供空段落用

        // WROOT-093 方案B（已与用户确认，见交接文档）：全量前置扫描注册，
        // 取代"处理到图片元素时才注册"。原方案的问题是图片在 doc.body 里
        // 的顺序若晚于它实际影响的段落（例如图片定义在标题之后但y坐标却
        // 与标题重叠——docx里完全合法，浮动图片的文档顺序位置与视觉y坐标
        // 没有强制对应关系），排在图片前面的段落断行时查询 float_contexts
        // 必然是 None（图片还没被注册），导致该收窄的没收窄。
        // 简化实现：把所有 wrap_type=square/tight/through 的图片全部注册
        // 进首页（cur_page，此刻必然是第一页）的 FloatContext，不做跨页
        // 预判——多数docx场景图片和它影响的段落本就同页，此简化足够用。
        // 已知限制（记入交接文档）：若图片实际排版结果落在非首页，但与
        // 首页某处y坐标"数值上"重叠，会误触发首页对应位置文字的避让。
        // 根治需要先完整跑一遍排版预演（dry run）才能确定图片的真实页码，
        // 超出本次范围，留给"多页完整上下文渲染"专项处理。
        // WROOT-ANCHOR-01：dry run 阶段完全跳过 FloatContext 建立——dry run
        // 只用来测量"无环绕情况下"每个段落的真实 y 坐标，不能掺入环绕逻辑，
        // 否则两遍结果不可比（这不是性能优化，是正确性要求）。
        if !dry_run {
        for (prescan_index, e) in doc.body.iter().enumerate() {
            let ff = match e {
                BodyElement::Frame(ff) => ff,
                _ => continue,
            };
            let wraps_square = matches!(
                ff.wrap_type.as_deref(),
                Some("square") | Some("tight") | Some("through")
            );
            if !wraps_square {
                continue;
            }
            // WROOT-ANCHOR-02: 前置扫描注册进 FloatContext 的 y 坐标必须
            // 跟"图片Frame插入时的真实y坐标"（下面主循环里算的 anchor_base_y
            // + ff.y_pt）保持一致，否则 FloatContext 里登记的收窄区间和图片
            // 实际视觉位置对不上，会导致"收窄生效了，但生效在错误的段落上"
            // ——本次bug就是这里当初漏改，只在图片Frame插入分支加了换算，
            // 这里前置扫描分支还在用裸的 ff.y_pt，两处必须用同一套算法。
            let prescan_anchor_base_y = match ff.anchor_type_v.as_deref() {
                Some("paragraph") | Some("character") => {
                    // WROOT-ANCHOR-04: 优先用解析阶段精确记录的原属段落
                    // index，只有该值缺失（理论上罕见）才回退旧的"下一个
                    // 段落"猜测——旧猜测在"图片是某段落内第二个run"这种
                    // 常见结构下方向猜反，导致锚点错位到文档流里更靠后
                    // 的段落。
                    let target_idx = ff.origin_para_body_index.or_else(|| {
                        doc.body.iter()
                            .enumerate()
                            .skip(prescan_index + 1)
                            .find(|(_, e2)| matches!(e2, BodyElement::Paragraph(_)))
                            .map(|(i, _)| i)
                    });
                    target_idx
                        .and_then(|idx| anchor_y_map.and_then(|m| m.get(&idx)))
                        .copied()
                        .unwrap_or(0.0)
                }
                _ => 0.0,
            };
            // distT/distB/distL/distR：OOXML wp:anchor 的环绕间距属性，
            // taffy::FloatContext 原生不支持"图文间距"概念，故手动叠加：
            // - distT 加到 min_y 上，让图片整体往下让出上间距；
            // - distB 加到 floated_box 的 height 上，让 FloatContext 认为
            //   图片更"高"，从而下方文字的避让区间随之下移，变相留出下间距；
            // - distL/distR 通过 containing_block_insets 传入，taffy 内部
            //   按 direction 对应的 slot 各自取用，两个值一起传即可。
            let real_float_y = prescan_anchor_base_y + ff.y_pt;
            let left_occupied = ff.x_pt + ff.width_pt;
            let direction = if left_occupied <= content_w * 0.5 {
                FloatDirection::Left
            } else {
                FloatDirection::Right
            };
            let fctx = float_contexts.entry(cur_page).or_insert_with(|| {
                let mut c = FloatContext::new();
                c.set_width(content_w);
                c
            });
            fctx.place_floated_box(
                taffy::Size { width: ff.width_pt, height: ff.height_pt + ff.dist_b_pt },
                real_float_y,
                [ff.dist_l_pt, ff.dist_r_pt],
                direction,
                Clear::None,
            );
            // WROOT-037-B修正：taffy::FloatContext::place_floated_box_inner
            // (compute/float.rs "Handle case where floated box is placed after
            // all existing segments"分支) 内部用
            // `let start_y = last_y_end.max(start_y);` 对传入的 min_y 做了
            // 钳制——segments为空时 last_y_end 恒为0.0，任何负数 min_y 都会
            // 被钳成0.0，taffy实际生效的浮动区间起点并不是我们传入的
            // real_float_y 原始值。float_y_ranges 必须同步模拟这个钳制，
            // 否则算出的 float_y_end 比taffy真实生效的范围更小。
            // 注：此处只精确模拟"segments为空（页面第一个浮动框）"这一种
            // 场景的钳制逻辑，与本样本吻合；若后续同页有多个浮动框导致走
            // 其他分支，钳制细节可能不同，届时需要重新核对taffy源码。
            let clamped_start_y = real_float_y.max(0.0);
            let float_y_end = clamped_start_y + ff.height_pt + ff.dist_b_pt;
            float_y_ranges.entry(cur_page).or_default()
                .push((clamped_start_y, float_y_end));
            // WROOT-037-GAP修正：真实视觉底边，不经过clamp，用于后续"1"
            // 视觉偏移量计算，避免与图片真实渲染位置（第877行附近，同样
            // 用未钳制的 real_float_y/ff.y_pt）脱节。
            let visual_float_y_end = real_float_y + ff.height_pt + ff.dist_b_pt;
            visual_float_y_ranges.entry(cur_page).or_default()
                .push((real_float_y, visual_float_y_end));
        }
        } // 闭合 if !dry_run (WROOT-ANCHOR-01)

        for (para_index, elem) in doc.body.iter().enumerate() {
            // [WROOT-任务B] BodyElement::Section 是纯粹的节边界标记（parser
            // 在段落内嵌 w:sectPr 时插入，见 roe-parser docx.rs），不携带任何
            // 需要参与排版的可见内容，只需要递增"当前第几节"计数器并跳过。
            // 必须放在这个 for 循环的最前面（在 inline_size / Frame / Shape /
            // Table 等分支之前），因为它不属于任何一种可渲染内容，不能落入
            // 后面任何一个分支。
            if matches!(elem, BodyElement::Section(_)) {
                cur_section_index += 1;
                // [WROOT-hf-parity] 标记"下一次建页时，section 内页码要
                // 从1重新开始"，而不是延续上一个 section 的计数。
                section_just_changed = true;

                // [WROOT-047] 此前这个分支只递增计数器，完全没有读取新
                // section 自己的页面尺寸/margin——doc.sections[cur_section_index]
                // 才是真正携带 page_size 的完整 Section 数据（BodyElement::
                // Section 本身只是 body 流里的边界占位符，不含这些属性）。
                // 若新 section 的 page_size/margin 与当前 self.page_width/
                // height/margins 不同，说明这是一次真正的"页面规格切换"，
                // 必须：①更新 self.page_width/height/margins（后续 new_page()
                // 调用都读这几个字段，不需要逐个改调用点）②强制换页，且
                // 换页要在更新尺寸之后，让新页立即用新尺寸建（不能沿用旧
                // 尺寸页面再补内容）。
                //
                // 换页判断复用 page_break_before 已验证的"当前页非空才换页"
                // 逻辑，避免文档开头/连续两个空section时产生多余空白页。
                //
                // margin fallback：Section 各字段是 Option，None 表示"沿用
                // WriterDocument 顶层默认值"，与 title_page/header/footer
                // 已经采用的 fallback 约定一致。
                if let Some(section) = doc.sections.get(cur_section_index) {
                    let new_page_size = section.page_size.unwrap_or(doc.page_size);
                    let new_margins = Margins {
                        top:    section.margin_top.unwrap_or(doc.margin_top),
                        bottom: section.margin_bottom.unwrap_or(doc.margin_bottom),
                        left:   section.margin_left.unwrap_or(doc.margin_left),
                        right:  section.margin_right.unwrap_or(doc.margin_right),
                    };
                    // WROOT-136x(分栏修复)：仿照 page_size/margin 的 fallback 模式读取
                    // section.columns。None 表示"沿用 WriterDocument 顶层默认值"，与
                    // 其它 per-section 字段的约定一致。
                    let new_columns = section.columns.clone().unwrap_or_else(|| doc.columns.clone());
                    let size_changed = new_page_size != (self.page_width, self.page_height);
                    let margins_changed = new_margins != self.margins;
                    // WROOT-136x(分栏修复)：分栏本身的变化此前完全没有触发换页判断
                    // （原条件只看 size_changed/margins_changed），导致即使下面把
                    // arena.doc_columns 更新了，因为不满足换页条件、当前页不会重建，
                    // 新的分栏配置永远用不上（new_page() 内部才会读 arena.doc_columns
                    // 生成实际的栏位布局，已建好的页不会回头重新分栏）。
                    // WROOT-136x(分栏修复)：DocumentColumns 未实现 PartialEq（定义在
                    // roe-model crate，本次改动范围收窄为只碰 roe-parser/roe-writer，不
                    // 跨 crate 加 derive），手动比较会影响实际栏位布局的四个字段。
                    let columns_changed = new_columns.num_cols != arena.doc_columns.num_cols
                        || new_columns.uniform_spacing != arena.doc_columns.uniform_spacing
                        || new_columns.col_widths != arena.doc_columns.col_widths
                        || new_columns.col_spacing != arena.doc_columns.col_spacing;
                    // WROOT-138(止血)：columns_changed 不再参与"是否换页"判断，只用来
                    // 决定是否需要更新 arena.doc_columns。分栏变化本身不应该强制换页——
                    // 这与 page_break_before 字段（parser层已根据 section_type 算好是否
                    // 需要换页）的语义冲突，continuous 节应该换栏不换页；size/margins
                    // 变化仍然需要强制换页，这条语义不变（见 ROE_Progress_v187 1.4/1.5节）。
                    if columns_changed {
                        arena.doc_columns = new_columns.clone();
                    }
                    if size_changed || margins_changed {
                        self.page_width  = new_page_size.0;
                        self.page_height = new_page_size.1;
                        self.margins     = new_margins;
                        // WROOT-134：同步刷新由 self.* 派生的 content_h（WROOT-047 的遗漏另一半）。
                        content_h = self.page_height - self.margins.top - self.margins.bottom;
                        // WROOT-136x(分栏修复)：必须在 new_page() 调用之前更新，
                        // new_page() 内部用 self.doc_columns 计算这一页的实际栏位布局。
                        arena.doc_columns = new_columns;
                        if !arena.pages[cur_page].frames.is_empty() {
                            cur_page = arena.new_page(self.page_width, self.page_height, self.margins);
                            cur_page_in_section = if section_just_changed { section_just_changed = false; 1 } else { cur_page_in_section + 1 };
                            arena.page_section_index.insert(cur_page, cur_section_index);
                            arena.page_index_in_section.insert(cur_page, cur_page_in_section);
                            cur_column = 0;
                        }
                    }
                }
                continue;
            }
            // 处理内联图片和内联图表
            // WROOT-097: 元组新增 dist_t_pt/dist_b_pt 两项。InlineImage 现在携带
            // wp:inline 自身的 distT/distB（此前完全未解析、渲染时图片贴着/压着
            // 相邻文字），InlineChart 没有这个概念，两项固定填0，保持原行为不变。
            let inline_size: Option<(f32, f32, f32, f32)> = match elem {
                BodyElement::Image(img) => Some((img.width_pt, img.height_pt, img.dist_t_pt, img.dist_b_pt)),
                BodyElement::InlineChart { width_pt, height_pt, .. } => Some((*width_pt, *height_pt, 0.0, 0.0)),
                BodyElement::InlineStatChart { width_pt, height_pt, .. } => Some((*width_pt, *height_pt, 0.0, 0.0)),
                _ => None,
            };
            if let Some((_img_w, img_h, dist_t_pt, dist_b_pt)) = inline_size {
                let img_h = img_h;
                // v318f: 本页放不下就换页（页内已有内容时才换，避免超高图片无限换页）
                if arena.used_height(cur_page) > 0.0
                    && arena.used_height(cur_page) + dist_t_pt + img_h + dist_b_pt > content_h
                {
                    cur_page = arena.new_page(self.page_width, self.page_height, self.margins);
                    cur_page_in_section = cur_page_in_section + 1;
                    arena.page_section_index.insert(cur_page, cur_section_index);
                    arena.page_index_in_section.insert(cur_page, cur_page_in_section);
                    cur_column = 0;
                }
                // WROOT-097 fix: 此前这里自己手写累加"所有frame的height+space_after"，
                // 漏加了每个frame自己的space_before，导致算出的y_on_page系统性偏小
                // （偏上），图片会往上顶、压住/叠在前一段文字里（比distT/distB本身
                // 有没有生效更严重的bug，此前误判为"distT/distB看不出效果"）。
                // 改为复用 arena.used_height()——与Keep/ShiftUp分支完全同源的高度
                // 累加公式，保证图片的y坐标和文字段落用的是同一套基准。
                let y_on_page = arena.page_content_global_y(cur_page)
                    + arena.used_height(cur_page)
                    + dist_t_pt;
                let fid = arena.frames.insert(crate::frame::ParagraphFrame {
                    id: slotmap::KeyData::from_ffi(0).into(),
                    page: cur_page,
                    follow: None,
                    precede: None,
                    content: FrameContent::Paragraph(vec![]),
                    y_on_page,
                    column_index: 0,
                    para_index,
                    space_before: dist_t_pt,
                    space_after: img_h + dist_b_pt,
                    is_floating: false,
                });
                arena.frames[fid].id = fid;
                arena.pages[cur_page].frames.push(fid);
                continue;
            }
            // 处理浮动图片（有 x/y 坐标，插入空 frame 供 arena_to_layout_pages 收集）
            if let BodyElement::Frame(ff) = elem {
                // WROOT-ANCHOR-01：relativeFrom="paragraph"/"character" 时，
                // ff.y_pt 是相对"锚点段落"的偏移，不是相对页面内容区顶部。
                // 锚点段落 = body 里紧邻这个 Frame 之后的第一个 Paragraph。
                // WROOT-ANCHOR-01: y坐标基准由V方向(anchor_type_v)决定，
                // 不是H方向(anchor_type_h)——之前的合并字段优先取H导致这里
                // 一直判断错误(真实文档常见H="column" V="paragraph"的组合)。
                let anchor_base_y = match ff.anchor_type_v.as_deref() {
                    Some("paragraph") | Some("character") => {
                        // WROOT-ANCHOR-04: 同 prescan 分支，优先用精确记录
                        // 的原属段落 index，回退才用旧的"下一个段落"猜测。
                        let target_idx = ff.origin_para_body_index.or_else(|| {
                            doc.body.iter()
                                .enumerate()
                                .skip(para_index + 1)
                                .find(|(_, e)| matches!(e, BodyElement::Paragraph(_)))
                                .map(|(i, _)| i)
                        });
                        target_idx
                            .and_then(|idx| anchor_y_map.and_then(|m| m.get(&idx)))
                            .copied()
                            .unwrap_or_else(|| arena.used_height(cur_page))
                    }
                    _ => 0.0,
                };
                let fid = arena.frames.insert(crate::frame::ParagraphFrame {
                    id: slotmap::KeyData::from_ffi(0).into(),
                    page: cur_page,
                    follow: None,
                    precede: None,
                    content: FrameContent::Paragraph(vec![]),
                    y_on_page: arena.page_content_global_y(cur_page) + anchor_base_y + ff.y_pt,
                    column_index: 0,
                    para_index,
                    space_before: 0.0,
                    space_after: 0.0,  // 浮动图片不占文档流高度
                    is_floating: false,
                });
                arena.frames[fid].id = fid;
                arena.pages[cur_page].frames.push(fid);
                // WROOT-093：图片的 FloatContext 注册已挪到主循环最前面的
                // 全量前置扫描（见 layout() 开头 for e in doc.body.iter()
                // 那一段），这里不再重复注册——重复调用 place_floated_box
                // 会导致同一张图片被摆放两次，互相顶开彼此位置，是真实bug
                // 不是无害冗余，必须删除。
                continue;
            }

            // ── 图形/文本框（Shape）：WROOT-023/029/032/033/039/046/050 系列缺口 ──
            // Step2 第一版：简化实现，先只按绝对坐标（x_pt/y_pt 相对页面内容区）
            // 占坑，不处理 Frame 分支那套 anchor_type_v 段落锚定逻辑（Shape 虽然
            // 有同名字段，但复杂锚定场景留到坐标验证通过后再补，先把链路打通）。
            if let BodyElement::Shape(shape) = elem {
                let fid = arena.frames.insert(crate::frame::ParagraphFrame {
                    id: slotmap::KeyData::from_ffi(0).into(),
                    page: cur_page,
                    follow: None,
                    precede: None,
                    content: FrameContent::Paragraph(vec![]),
                    y_on_page: arena.page_content_global_y(cur_page) + shape.y_pt,
                    column_index: 0,
                    para_index,
                    space_before: 0.0,
                    space_after: 0.0,  // 图形不占文档流高度（与浮动图片同语义）
                    is_floating: false,
                });
                arena.frames[fid].id = fid;
                arena.pages[cur_page].frames.push(fid);
                continue;
            }

            // ── 表格：按列宽布局，每个 cell 产出独立几何+真正多行 shaping ──────
            // WROOT-030 结构性重构：不再把整行 cell 拍平成 Vec<LineBox> 走
            // place_paragraph；改为每个 cell 用 shape_text_to_lines 产出真正
            // 换行的多行文字，包装成 TableCellFrame，整行走 place_table_row。
            if let BodyElement::Table(tbl) = elem {
                let font_size = self.registry.default_font_size;
                let cell_pad = 4.0_f32; // 左内边距

                // 计算列宽（按比例分配内容区宽度）
                let content_w = self.page_width - self.margins.left - self.margins.right;

                // WROOT-101：表格浮动绕排（w:tblpPr）。与图片浮动（WROOT-093）
                // 共用同一个 float_contexts（HashMap<PageId, FloatContext>），
                // 但注册时机和方向判断规则不同，不能照搬图片分支的逻辑：
                //
                // 1. 时机：图片需要 dry run + anchor_y_map 预知"未来"的锚点段落
                //    y坐标（relativeFrom="paragraph"时锚点段落可能晚于图片出现）。
                //    表格 vertAnchor="text" 只贴着"当下"位置，主循环走到这个
                //    BodyElement::Table分支时，前面所有内容已经排完，
                //    arena.used_height(cur_page) 就是表格的真实起始y，不需要
                //    预扫描/dry run（详见交接文档v67三、三节，参考LibreOffice
                //    sw/source/core/objectpositioning/一致：LOK对内容流锚定对象
                //    走的是排版驱动循环里的当下定位，不是预扫描）。
                //
                // 2. 方向：图片分支用 x_pt与content_w中点比较来判断左右浮动，
                //    这是图片有显式x_pt时的启发式。表格这次遇到的实际情况是
                //    w:tblpX/w:horzAnchor均缺失——查证LibreOffice
                //    writerfilter/dmapper/TablePositionHandler.hxx/.cxx
                //    （m_aHorzAnchor默认初始化为"text"，m_nX默认0，
                //    nHoriOrient缺省为HoriOrientation::NONE，不做任何猜测），
                //    确认docx真实语义是"缺省时偏移量按0处理，贴靠锚点起始边，
                //    不做启发式方向推断"。锚点起始边即内容区左边界，故
                //    x_pt缺省时直接给FloatDirection::Left，不做中点比较——
                //    这不是猜测，是把"偏移0+锚定文字流起始位置"唯一确定地
                //    翻译成taffy要求的Left/Right二选一。若后续遇到显式给出
                //    x_pt的表格样本，才需要引入类似图片分支的中点判断。
                // WROOT-101修正：表格浮动宽度此前误用w:tblW声明的百分比
                // （content_w*0.2）写死，与本渲染管线其余表格逻辑一贯采用的
                // "w:tblGrid列宽总和"来源不一致（本项目从未解析过w:tblW，
                // 现有非浮动表格col_widths分配也是完全基于tbl.col_widths，
                // 即tblGrid的gridCol总和；实测验证：本用例w:tblW声明40%，
                // tblGrid列宽总和155.3pt/content_w 391.2pt≈39.7%，两者数值
                // 接近但来源不同，选用与既有代码路径一致的tblGrid总和，
                // 不引入未解析字段导致两条真相来源打架）。
                let table_float_w = if tbl.col_widths.is_empty() {
                    content_w * 0.2 // 无col_widths时的保守兜底，未在真实样本验证
                } else {
                    tbl.col_widths.iter().sum::<f32>().min(content_w)
                };
                // WROOT-101修复：float_y 提升到外层作用域，供下面表格内部
                // 行循环的 float_row_y_cursor 累加器复用——place_table_row 给
                // 浮动表格行分配 y 坐标时不能依赖 arena.used_height()（该函数
                // 对 is_floating=true 的 frame 直接跳过不计入累加，这是为了让
                // 浮动表格"不占用纵向流空间、不挤开后续段落"，但连带导致同一
                // 张浮动表格内部的多行也全部被跳过累加，算出的 y 坐标彼此
                // 几乎相同，8行叠在同一处只露出最后画的一行）。用独立于
                // used_height 的累加器，从 float_y 起点开始按行高度递增。
                let mut float_row_y_cursor: Option<f32> = None;
                if let Some(fp) = &tbl.float_pos {
                    let tbl_height = measure_table_height(&mut self.registry, tbl, table_float_w);
                    let float_y = arena.used_height(cur_page) + fp.y_pt.unwrap_or(0.0);
                    float_row_y_cursor = Some(float_y);
                    let direction = match fp.x_pt {
                        Some(x) if x + table_float_w <= content_w * 0.5 => FloatDirection::Left,
                        Some(_) => FloatDirection::Right,
                        // x_pt缺省：按LOK语义贴锚点起始边（内容区左边界），
                        // 不做中点猜测。见上方注释。
                        None => FloatDirection::Left,
                    };
                    let fctx = float_contexts.entry(cur_page).or_insert_with(|| {
                        let mut c = FloatContext::new();
                        c.set_width(content_w);
                        c
                    });
                    fctx.place_floated_box(
                        taffy::Size { width: table_float_w, height: tbl_height },
                        float_y,
                        [fp.left_from_text_pt, fp.right_from_text_pt],
                        direction,
                        Clear::None,
                    );
                }

                // WROOT-101：浮动表格用自身收窄后的宽度分配列宽，而不是整页
                // content_w——否则表格会铺满整行，绕排毫无意义（20%宽度的窄表格
                // 若还是按满宽分配列宽，视觉上就是铺满，文字无处环绕）。
                // 非浮动表格（tbl.float_pos为None）行为完全不变，仍用content_w。
                let table_layout_w = if tbl.float_pos.is_some() {
                    table_float_w // 与上方place_floated_box注册的宽度保持同一来源
                } else {
                    content_w
                };
                let col_widths: Vec<f32> = if tbl.col_widths.is_empty() {
                    let n = tbl.rows.first().map(|r| r.cells.len()).unwrap_or(1).max(1);
                    vec![table_layout_w / n as f32; n]
                } else {
                    let total: f32 = tbl.col_widths.iter().sum();
                    if total > 0.0 {
                        tbl.col_widths.iter().map(|w| w / total * table_layout_w).collect()
                    } else {
                        let n = tbl.col_widths.len().max(1);
                        vec![table_layout_w / n as f32; n]
                    }
                };

                let num_cols = col_widths.len();
                for (ri, row) in tbl.rows.iter().enumerate() {
                    let mut cells: Vec<TableCellFrame> = Vec::new();
                    // WROOT-101：浮动方向为Left时表格贴内容区左边界，x起点仍是0，
                    // 不需要额外偏移——col_widths已经用收窄后的table_layout_w算过，
                    // 表格自然只占左侧一小段，右侧空间靠find_content_slot让给文字。
                    // 若后续遇到Right方向的浮动表格样本，这里需要改成
                    // content_w - table_layout_w 作为起点，当前用例未覆盖，先不加
                    // 死代码，留给下次遇到真实Right样本时再补。
                    let mut x = 0.0_f32;
                    for (ci, cell) in row.cells.iter().enumerate() {
                        let cw = col_widths.get(ci).copied().unwrap_or(content_w / num_cols as f32);
                        // cell 内可用宽度需减去左内边距，避免文字贴边
                        let effective_cw = (cw - cell_pad).max(cw * 0.3);
                        // BugB 修复：此前用 flat_map 把 cell 内所有段落的所有 run
                        // 直接拍平合并成一个 Vec<Run> 再整体 shape，段落边界信息
                        // 完全丢失，导致多段落被当成一整段连续文字排版（丢失换行）。
                        // 改为逐段落独立 shape，再按纵向累计高度依次堆叠拼接。
                        let mut cell_lines: Vec<LineBox> = Vec::new();
                        let mut cell_y_cursor = 0.0_f32;
                        let mut cell_paragraph_count: usize = 0;
                        let mut v317_extra: f32 = 0.0;
                        // 段落间垂直间距（pt）。当前用保守小值，未读取段落自身
                        // space_after/space_before 属性，cell 内段落间距的精细
                        // 样式支持记入已知限制，后续可按需扩展。
                        const CELL_PARAGRAPH_SPACING: f32 = 2.0;
                        for (cell_pi, ce) in cell.content.iter().enumerate() {
                            let CellContent::Paragraph(p) = ce else { continue };
                            if p.runs.is_empty() {
{
    let bl_h = font_size * 1.3;
    cell_lines.push(LineBox { y_offset: cell_y_cursor, ascent: font_size, descent: font_size * 0.2, line_height: bl_h, text: String::new(), runs: Vec::new(), x_offset: x, cell_width: cw, text_x_offset: x + cell_pad, cell_ref: Some((para_index as u32, ri as u32, ci as u32, cell_pi as u32)), ..Default::default() });
    cell_paragraph_count += 1;
    cell_y_cursor += bl_h + CELL_PARAGRAPH_SPACING;
}
                                continue;
                            }
                            // BugB 续修：读取该段落自身的左缩进（docx.rs 解析层已从
                            // numbering level 定义写入 p.indent_left，见 docx.rs
                            // 第2181-2185行），从可用宽度里扣除，并在产出的每一行
                            // x 坐标上叠加，使不同层级的编号（1./a./i.等）呈现正确
                            // 的阶梯缩进，而不是全部贴着 cell 左边界。
                            // BugB 五修：para_margin_left 此前只读 indent_left，
                            // 忽略了 w:hanging（存为 indent_first，通常是负值）。Word
                            // 的悬挂缩进语义是：indent_left 是正文换行后行的起点，
                            // 而"label + 第一行文字"的真实起点是 indent_left +
                            // indent_first（更靠左）。此前用纯 indent_left 当作这一
                            // 行的起点去算换行宽度，等于把 hanging 那部分本该让给
                            // label 的空间也算成了"已占用"，导致可写宽度比 Word 实际
                            // 给的更窄，深层级 label（如"i. Sublevel2.1"）因此被迫
                            // 换行。改为叠加 indent_first 后，该行真实起点更靠左，
                            // 可写宽度随之正确变宽。
                            let para_margin_left = (p.indent_left.unwrap_or(0.0)
                                + p.indent_first.unwrap_or(0.0)).max(0.0);
                            // BugB 三修：indent 不应该缩小排版换行用的可用宽度。此前
                            // 这里用 (effective_cw - para_margin_left) 传给
                            // shape_text_to_lines，导致缩进越深、可写宽度越窄，深层级
                            // 的"label+短文字"（如"i. Sublevel2.1"）在窄列表格cell里
                            // 被挤到不足以放下一行，被迫强制换行成两行——这是换行
                            // 修复那次patch本身引入的次生bug（用diff对比拍平合并前的
                            // 版本确认：旧版本换行判断用的是完整effective_cw，从未扣过
                            // 缩进）。缩进的语义应该只是"这一行的起点往右挪"，不应该
                            // 连带"这一行的终点也跟着往左挪、缩小整个可写区间"——用
                            // 完整 effective_cw 做换行判断，缩进只在下面的
                            // x_offset/text_x_offset 位置叠加时体现（该部分逻辑已经
                            // 是对的，不用改）。
                            // BugB 四修：换行宽度应扣减本段落缩进，但下限保护要用
                            // 固定绝对值（够放下几个字符），不能用 effective_cw 的百分比
                            // ——百分比下限跟缩进深度无关，宽度越大下限跟着越大，无法
                            // 反映"缩进越深、可写区间理应越窄"这个事实，这正是导致
                            // 深层级 label（如"i. Sublevel2.1"）在窄列里溢出穿透右
                            // 边框的根因（三修时误将"不能用错误的百分比下限扣缩进"
                            // 理解成了"完全不能扣缩进"，属于修复过头）。
                            const MIN_WRAP_WIDTH: f32 = 20.0;
                            let para_wrap_w = (effective_cw - para_margin_left).max(MIN_WRAP_WIDTH);
                            // 债务A修复：cell内段落自身的line_height，
                            // 未设置时用1.3兜底（原写死值，零回归）
                            let cell_line_height = p.line_abs_pt.filter(|_| p.line_exact.is_some()).map(|a| a / font_size.max(1.0)).or(p.line_height).unwrap_or(1.3);
                            let v317_styles: Vec<ResolvedRunStyle> = p.runs.iter()
                                .map(|r| resolve_run_style(doc, p, r, font_size))
                                .collect();
                            let v317_fams: Vec<Option<String>> = p.runs.iter()
                                .map(|r| resolve_run_font_family(doc, p, r).map(|f| f.to_string()))
                                .collect();
                            let mut para_lines = shape_text_to_lines_impl(
                                &mut self.registry, &p.runs, para_wrap_w, font_size, cell_line_height, Some(&v317_styles), Some(&v317_fams),
                                match p.alignment.or(doc.styles.resolve_paragraph_style(p.style.0.as_str()).alignment) {
                                    Some(roe_model::style::Alignment::Center) => Alignment::Center,
                                    Some(roe_model::style::Alignment::Right) => Alignment::End,
                                    _ => Alignment::Start,
                                },
                            );
                            if para_lines.is_empty() {
{
    let bl_h = font_size * 1.3;
    cell_lines.push(LineBox { y_offset: cell_y_cursor, ascent: font_size, descent: font_size * 0.2, line_height: bl_h, text: String::new(), runs: Vec::new(), x_offset: x, cell_width: cw, text_x_offset: x + cell_pad, cell_ref: Some((para_index as u32, ri as u32, ci as u32, cell_pi as u32)), ..Default::default() });
    cell_paragraph_count += 1;
    cell_y_cursor += bl_h + CELL_PARAGRAPH_SPACING;
}
                                continue;
                            }
                            cell_paragraph_count += 1;
                            // BugB indent修复v2：x_offset 语义是"该行相对cell左边界
                            // 的几何偏移，用于画边框"，不应该被段落缩进影响——边框
                            // 永远沿cell整体宽度画。缩进只应该体现在 text_x_offset
                            // 上。上一版patch把 para_margin_left 同时加到两个字段，
                            // 导致两者永远相等，下面收尾循环里 indent = text_x_offset
                            // - x_offset 恒为0，缩进从这里就已经"合并"消失了，跟收尾
                            // 循环那次覆盖无关。这次只加到 text_x_offset。
                            let v317_sb = p.space_before.unwrap_or(0.0).max(0.0);
                            let v317_sa = p.space_after.unwrap_or(0.0).max(0.0);
                            for l in para_lines.iter_mut() {
                                l.y_offset += cell_y_cursor + v317_sb;
                                l.text_x_offset += para_margin_left;
                                l.cell_ref = Some((para_index as u32, ri as u32, ci as u32, cell_pi as u32));
                            }
                            let para_height: f32 = para_lines.iter().map(|l| l.line_height).sum();
                            cell_y_cursor += v317_sb + para_height + CELL_PARAGRAPH_SPACING + v317_sa;
                            v317_extra += v317_sb + v317_sa;
                            cell_lines.extend(para_lines);
                        }
                        // WROOT-030 followup修复：x_offset/cell_width 恢复为cell
                        // 原始几何边界（不含内边距），供tile.rs画边框时使用；
                        // 文字实际绘制起点单独存入text_x_offset，避免和边框
                        // 坐标共用同一字段导致边框被内边距污染（见bug_log.md
                        // WROOT-030 followup条目：左边框与顶部横线交界处出现
                        // 视觉断口，根因即此处此前把text_x_offset的值直接
                        // 覆写进了x_offset）。
                        // BugB 再修：上面这个循环此前会无条件把每一行的
                        // text_x_offset 覆盖成统一的 x + cell_pad，导致此前在
                        // 段落级别算好的缩进（para_margin_left，见上面
                        // l.x_offset += para_margin_left / l.text_x_offset +=
                        // para_margin_left 那两行）被原地吞掉——覆盖前后 x_offset
                        // 是"段落内相对cell左边界的缩进量"，覆盖后要保留这个相对
                        // 缩进量，只是把"基准点"从0改成cell的真实几何坐标x。
                        // 做法：先记录每行覆盖前的 text_x_offset 相对 x_offset
                        // 的偏移量（这个偏移量此前只可能是 para_margin_left 或
                        // 0.0，跟cell_pad无关，因为cell_pad是这里才第一次加上），
                        // 再用新的 x/cw 基准重建两个字段，缩进量保留、内边距叠加。
                        for l in cell_lines.iter_mut() {
                            let indent = l.text_x_offset - l.x_offset;
                            l.x_offset = x;
                            l.cell_width = cw;
                            l.text_x_offset = x + cell_pad + indent.max(0.0);
                        }
                        // WROOT-104：空单元格行高兜底。此前空段落（cell_lines为空，
                        // 例如单元格只含无runs的空w:p，golden用例
                        // table-style-border-export/table-style-cell-back-color/
                        // tdf124399_SingleCellTableBorders三例均属此类，靠tblStyle
                        // 底纹/边框呈现效果、无文字内容）导致该行row_height算作0，
                        // 整行/整表在place_table_row/measure_table_height两处
                        // 都塌缩为0高度，视觉上整张表格被压缩成一条线。这里补一个
                        // 与正文空段落兜底同款的空LineBox（见约1781行"哑元空段落"
                        // 写法），保证空单元格也至少占用一行默认行高，两处调用方
                        // （measure_table_height同步改，见该函数内对应位置）无需
                        // 各自识别"空cell"这一特殊情况，从源头统一。
                        if cell_lines.is_empty() {
                            let fallback_font_size = self.registry.default_font_size;
                            cell_lines.push(LineBox {
                                y_offset:      0.0,
                                ascent:        fallback_font_size,
                                descent:       fallback_font_size * 0.2,
                                line_height:   fallback_font_size * 1.3,
                                text:          String::new(),
                                runs:          Vec::new(),
                                x_offset:      x,
                                cell_width:    cw,
                                text_x_offset: x + cell_pad,
                                cell_ref: Some((para_index as u32, ri as u32, ci as u32, cell.content.iter().position(|ce| matches!(ce, CellContent::Paragraph(_))).unwrap_or(0) as u32)),
                                ..Default::default()
                            });
                        }
                        // WROOT-100：四条边分别判断是否为外框（第一行/最后一行/
                        // 第一列/最后一列），外框用cell/table默认边框色，内部线用
                        // table.inside_h/v_color/width，两者都无则虚线兜底。
                        // num_rows现算（tbl.rows.len()，避免额外传参改动更多调用点）。
                        let num_rows = tbl.rows.len();
                        let is_top_edge    = ri == 0;
                        let is_bottom_edge = ri + 1 == num_rows;
                        let is_left_edge   = ci == 0;
                        let is_right_edge  = ci + 1 == num_cols;
                        cells.push(TableCellFrame {
                            x_offset:      x,
                            cell_width:    cw,
                            lines:         cell_lines,
                            border_top:    resolve_table_border_side(
                                is_top_edge, cell, tbl, tbl.inside_h_color, tbl.inside_h_width),
                            border_bottom: resolve_table_border_side(
                                is_bottom_edge, cell, tbl, tbl.inside_h_color, tbl.inside_h_width),
                            border_left:   resolve_table_border_side(
                                is_left_edge, cell, tbl, tbl.inside_v_color, tbl.inside_v_width),
                            border_right:  resolve_table_border_side(
                                is_right_edge, cell, tbl, tbl.inside_v_color, tbl.inside_v_width),
                            // BugB续修：cell_paragraph_count 统计的是上面循环里
                            // 真正 extend 进 cell_lines 的段落数（即跳过了空
                            // runs/空产出的段落后剩下的），与 cell_y_cursor 的
                            // 累积次数完全对齐，供 TableCellFrame::height() 用。
                            paragraph_count: cell_paragraph_count,
                            extra_spacing: v317_extra,
                            // 表格样式移植 Layer 1步骤D：原样透传 model 层已解析好的
                            // 单元格背景色（docx.rs w:tcPr/w:shd 解析写入），此处 cell
                            // 就是 roe_model::common::TableCell，字段名相同直接复制。
                            bg_color: cell.bg_color,
                            // 表格样式移植 Layer 1步骤B（渲染部分）：cell 自身边框颜色/
                            // 宽度优先，没有时 fallback 到表格级默认边框（w:tblBorders
                            // 解析结果，tbl.default_border_color/width）。只在这里做
                            // 一次性合并，不回写 model 层 TableCell（见 frame.rs 该字段
                            // 注释、ROE_DOCX_Progress_v64.md 二节"核心原则"）。
                            border_color: resolve_cell_border(cell, tbl).0,
                            border_width: resolve_cell_border(cell, tbl).1,
                        });
                        x += cw;
                    }
                    let row_h = self.place_table_row(
                        &mut arena, &mut cur_page, &mut cur_column,
                        para_index, cells, content_h, tbl.float_pos.is_some(),
                        float_row_y_cursor,
                    );
                    // WROOT-101修复：浮动表格行按自身累加器递增，不依赖
                    // used_height（跳过浮动frame，见上方注释）。非浮动表格
                    // float_row_y_cursor 恒为 None，这个分支不生效，行为不变。
                    if let Some(cursor) = float_row_y_cursor.as_mut() {
                        *cursor += row_h;
                    }
                }
                if let Some(final_cursor) = float_row_y_cursor {
                }
                continue;
            }
            let BodyElement::Paragraph(para) = elem else { continue };

            // WROOT-001: 段落标记了强制分页（来自docx pageBreakBefore或
            // 段内嵌sectPr分节符），且当前页已有内容时，新开一页再排此段。
            // 若当前页还是空页（刚new_page还没放任何frame），则不需要再开一页，
            // 避免文档开头就带page_break_before时产生多余空白页。
            if para.page_break_before && !arena.pages[cur_page].frames.is_empty() {
                cur_page = arena.new_page(self.page_width, self.page_height, self.margins);
                cur_page_in_section = if section_just_changed { section_just_changed = false; 1 } else { cur_page_in_section + 1 };
                arena.page_section_index.insert(cur_page, cur_section_index);
                arena.page_index_in_section.insert(cur_page, cur_page_in_section);
                cur_column = 0;
            }

            // 修正记录（第二次实测修正）：column_break_after 标记在分栏符所在的
            // 空段落自己身上，但真实渲染对比（OnlyOffice/LibreOffice参考图）
            // 证实：这一行空白应出现在新栏（右栏）顶部，而不是旧栏（左栏）
            // 末尾——即触发时机必须是"排这个空段落之前先换栏"，而不是"排完
            // 它之后再换栏"（第一次修正曾尝试"段后"，实测空白行错误地留在
            // 了左栏，未生效在右栏，已改回本方案）。字段名虽然叫
            // column_break_after，语义是"该段落结束时会跨栏"，但由于该段落
            // 自身就是这次跨栏的诱因、且它必须成为新栏的第一行，实际触发点
            // 放在排它之前。与page_break_before是独立触发条件，逻辑对称：
            // 不管当前栏是否排满都强制换栏；已是最后一栏则退化为换页，与
            // place_paragraph内部"内容溢出触发换栏"分支（约1098-1114行）
            // 共用同一条"是否还有下一栏"判断规则。若当前栏/页还是空的
            // （刚翻过来没放任何内容），不需要再换一次，避免产生空白栏。
            if para.column_break_after && !arena.pages[cur_page].frames.is_empty() {
                let num_cols = arena.pages[cur_page].columns.num_cols;
                if cur_column + 1 < num_cols {
                    cur_column += 1;
                } else {
                    cur_page = arena.new_page(self.page_width, self.page_height, self.margins);
                    cur_page_in_section = if section_just_changed { section_just_changed = false; 1 } else { cur_page_in_section + 1 };
                    arena.page_section_index.insert(cur_page, cur_section_index);
                    arena.page_index_in_section.insert(cur_page, cur_page_in_section);
                    cur_column = 0;
                }
            }

            // WROOT-026：该段落自身承载 <w:br w:type="page"/>（page_break_after）。
            // 真实渲染（LibreOffice截图确认，样本first-header-footer）证实这个
            // 段落完全不产生可见行——与column_break_after（占一行再换栏）和
            // 空段落占位分支（也会占一行）都不同。必须在resolved/font_size等
            // 计算之前、空段落占位分支之前就拦截并整段跳过，不能让它落入
            // 下面的"空段落：单行占位"逻辑，否则会在换页前的旧页多产生一行
            // 空白，与参考渲染（干净分页，无空行）不符。
            if para.page_break_after {
                if !arena.pages[cur_page].frames.is_empty() {
                    cur_page = arena.new_page(self.page_width, self.page_height, self.margins);
                    cur_page_in_section = if section_just_changed { section_just_changed = false; 1 } else { cur_page_in_section + 1 };
                    arena.page_section_index.insert(cur_page, cur_section_index);
                    arena.page_index_in_section.insert(cur_page, cur_page_in_section);
                    cur_column = 0;
                }
                continue;
            }

            let resolved    = resolve_para_style(doc, para, self.registry.default_font_size);
            let font_size   = resolved.font_size;
            let space_after = resolved.space_after;
            // 债务A修复：不再写死1.3倍，改用该段落实际解析出的行距倍数
            let metrics     = self.registry.metrics(Some(font_size), resolved.line_height);

            // WROOT-025（移植自PPTX侧 PROOT-035 expand_tabs_to_spaces 方案，
            // 做法与 roe-impress/src/slide_renderer.rs 一致）：\t 此前被
            // 原样塞进 full_text，字体没有 \t 的可见字形，Parley shaping
            // 时落入 .notdef 替代字形（tofu方框）。这里在 full_text 拼接
            // 之前对每个 run 的文本单独展开 tab：按 \t 切分，逐段测量已
            // 排版宽度累加到 tab_cur_x，每遇到一个 \t 就算到下一个 tab
            // stop（OOXML defTabSz 默认72pt）还差多少像素，换算成空格数
            // 插入（四舍五入取整，是本实现唯一的近似点）。
            // v85 PROOT-035 踩坑记录：必须逐 run 分别展开后再拼接，不能对
            // 拼接后的整体 full_text 做替换——后面 run_byte_offset 是按
            // 每个 run（展开后）的文本长度累加的，若只在整体上替换，run
            // 的字节 range 会和 full_text 长度对不上，样式会错位。
            // tab_cur_x 段落级作用域，跨 run 累积，每个新段落重新从0开始。
            let mut tab_cur_x = 0.0f32;
            // WROOT-068渲染算法本体：para.tab_stops非空时（真实w:tabs定义存在），
            // 不再用72pt栅格公式展开tab，改为每个tab用1个空格最小宽度占位，
            // 同时记录TabMarker（tab在full_text里的绝对字节偏移+所属run_idx+
            // 段落内第几个tab），供断行完成后的LOK两阶段算法第二遍使用。
            // tab_stops为空的段落（绝大多数普通文档）完全走原72pt栅格逻辑，
            // 一行代码不改，是本次改动最重要的防回归要求。
            let has_tab_stops = !para.tab_stops.is_empty();
            let mut tab_markers: Vec<crate::frame::TabMarker> = Vec::new();
            let mut byte_offset_acc: usize = 0usize;
            let mut tab_seq: usize = 0usize;
            let mut expanded_run_texts: Vec<String> = Vec::with_capacity(para.runs.len());
            for (run_idx, r) in para.runs.iter().enumerate() {
                if !r.text.contains('\t') {
                    // STEP6-STAGE-A（阶段A最小验证）：脚注/尾注编号占位符run
                    // （w:endnoteRef/w:footnoteRef）的run.text恒为空字符串——
                    // docx原文里这类标签就没有w:t子元素。如果原样push空字符串
                    // 进expanded_run_texts，这个run在full_text里占0字节，后续
                    // Parley排版时不会为它产生任何GlyphRun/InlineBox（Parley
                    // 不会给零长度片段生成layout item），run_spans.push那一步
                    // 永远轮不到它，编号连"存在感"都没有——这是继parser层
                    // "空run被静默丢弃"之后，在渲染层再次撞上的同一类问题，
                    // 只是这次的丢失点在Parley排版这一步，不是push判断条件。
                    //
                    // 修复：给这类run塞一个占位文字而非空字符串，让它被Parley
                    // 当成普通文字处理，天然产生GlyphRun走完整条正常渲染路径，
                    // 不需要像公式(WROOT-080)那样额外造InlineBox分支。
                    // 阶段A写死占位符"1"（不分footnote/endnote、不做真实计数），
                    // 只验证这条链路本身走不walk通；阶段B会替换成真实计数结果。
                    // resolve_run_style()已把这类run的script设为Superscript，
                    // 配合这里给的占位文字，会被tile.rs已有的上标渲染逻辑
                    // （script_runs循环）自动接住，不需要在tile.rs里额外改动。
                    let is_note_ref_placeholder = r.text.is_empty() && matches!(
                        r.field,
                        Some(roe_model::writer::FieldKind::EndnoteRef)
                            | Some(roe_model::writer::FieldKind::FootnoteRef)
                    );
                    // STEP6-STAGE-B：阶段A写死的占位符"1"，改为查上面新建
                    // 的 endnote_numbers/footnote_numbers 表拿真实序号，再用
                    // roe-parser 已提升为 pub 的 format_num 按文档的
                    // endnote_num_fmt/footnote_num_fmt（如 lowerRoman）格式化。
                    // 查不到序号时（理论上不应发生——扫描表和这里遍历的是
                    // 同一份 doc.body，除非数据不一致）兜底显示"?"而非
                    // panic，保持渲染管线不中断的既有原则。
                    let effective_text: String = if is_note_ref_placeholder {
                        match r.field {
                            Some(roe_model::writer::FieldKind::EndnoteRef) => {
                                let num = r.endnote_ref
                                    .and_then(|id| endnote_numbers.get(&id).copied())
                                    .unwrap_or(0);
                                if num == 0 {
                                    "?".to_string()
                                } else {
                                    let fmt_str = match doc.endnote_num_fmt {
                                        roe_model::writer::NumFormat::LowerRoman => "lowerRoman",
                                        roe_model::writer::NumFormat::UpperRoman => "upperRoman",
                                        roe_model::writer::NumFormat::LowerAlpha => "lowerLetter",
                                        roe_model::writer::NumFormat::UpperAlpha => "upperLetter",
                                        roe_model::writer::NumFormat::None => "none",
                                        roe_model::writer::NumFormat::Arabic => "decimal",
                                    };
                                    roe_parser::ooxml::docx::format_num(num, fmt_str)
                                }
                            }
                            Some(roe_model::writer::FieldKind::FootnoteRef) => {
                                let num = r.footnote_ref
                                    .and_then(|id| footnote_numbers.get(&id).copied())
                                    .unwrap_or(0);
                                if num == 0 {
                                    "?".to_string()
                                } else {
                                    let fmt_str = match doc.footnote_num_fmt {
                                        roe_model::writer::NumFormat::LowerRoman => "lowerRoman",
                                        roe_model::writer::NumFormat::UpperRoman => "upperRoman",
                                        roe_model::writer::NumFormat::LowerAlpha => "lowerLetter",
                                        roe_model::writer::NumFormat::UpperAlpha => "upperLetter",
                                        roe_model::writer::NumFormat::None => "none",
                                        roe_model::writer::NumFormat::Arabic => "decimal",
                                    };
                                    roe_parser::ooxml::docx::format_num(num, fmt_str)
                                }
                            }
                            _ => "1".to_string(), // 不应到达，兜底保留阶段A行为
                        }
                    } else {
                        r.text.clone()
                    };
                    if !effective_text.is_empty() {
                        let fs = resolve_run_style(doc, para, r, self.registry.default_font_size).font_size;
                        tab_cur_x += self.registry.measure_text_width(&effective_text, fs);
                    }
                    byte_offset_acc += effective_text.len();
                    expanded_run_texts.push(effective_text);
                    continue;
                }
                let fs = resolve_run_style(doc, para, r, self.registry.default_font_size).font_size;
                let tab_stop_pt = 72.0f32; // OOXML defTabSz 默认值，has_tab_stops=false 时使用
                let space_w = self.registry.measure_text_width(" ", fs).max(0.01);
                let tab_count = r.text.matches('\t').count();
                let mut out = String::with_capacity(r.text.len());
                for (i, segment) in r.text.split('\t').enumerate() {
                    out.push_str(segment);
                    byte_offset_acc += segment.len();
                    if !segment.is_empty() {
                        tab_cur_x += self.registry.measure_text_width(segment, fs);
                    }
                    if i < tab_count {
                        if has_tab_stops {
                            // WROOT-068：真实tab_stops存在，不用栅格公式算宽度，
                            // 固定塞1个空格最小宽度占位（LOK PreFormat阶段做法），
                            // 真实宽度留到断行后的PostFormat阶段按LOK公式反推。
                            out.push(' ');
                            tab_markers.push(crate::frame::TabMarker {
                                byte_offset: byte_offset_acc,
                                run_idx,
                                tab_seq,
                            });
                            byte_offset_acc += 1; // ' ' 是1字节
                            tab_cur_x += space_w;
                            tab_seq += 1;
                        } else {
                            let next_stop = ((tab_cur_x / tab_stop_pt).floor() + 1.0) * tab_stop_pt;
                            let gap = (next_stop - tab_cur_x).max(0.0);
                            let n_spaces = (gap / space_w).round().max(1.0) as usize;
                            for _ in 0..n_spaces {
                                out.push(' ');
                            }
                            byte_offset_acc += n_spaces; // 空格全是1字节ASCII
                            tab_cur_x = next_stop;
                        }
                    }
                }
                expanded_run_texts.push(out);
            }
            // WROOT-040-HYPHEN：断词预处理（方案C1第一步）。必须在这里、
            // full_text拼接之前、run_byte_ranges计算之前插入——tab展开
            // （上面byte_offset_acc/tab_markers那段）已经完成，不受影响；
            // 后面run_byte_ranges是用expanded_run_texts[run_idx].len()
            // 重新算的（见下方注释"必须用展开后的长度累加字节偏移"），
            // 原地替换这里的元素后，样式range会自动用上插入软连字符后的
            // 正确长度，不需要改动run_byte_ranges的计算逻辑本身。
            for t in expanded_run_texts.iter_mut() {
                *t = crate::hyphenation::preprocess_hyphenation(t);
            }
            let full_text: String = expanded_run_texts.join("");

            // DOCX-8 公式渲染修复（真正根因）：公式run的text是空格占位符
            // （Step2占位符改造后的产物），trim()后为空。如果一个段落全部
            // 由公式run组成（常见于"独立成段的公式"，比如m:oMathPara场景），
            // full_text.trim().is_empty()会误判成空段落，导致整段被替换成
            // 完全空的LineBox（text/runs都清空），后面per-run的resolve_run_style
            // 循环、is_formula标记、metadata(run_idx)全部被跳过——公式因此
            // 从layout阶段起就没有进入渲染管线，是WROOT-059/060/061空白问题
            // 的真正根源（比tile.rs里is_placeholder_line那处更早生效）。
            // 修复：只要段落里有任意一个run.formula是Some，就不能算空段落。
            let has_formula_run = para.runs.iter().any(|r| r.formula.is_some());

            // ── 空段落：单行占位 ──────────────────────────────────────
            if full_text.trim().is_empty() && !has_formula_run {
                // 修正记录（第三次修正，感谢用户实测截图确认）：真实Word/
                // OnlyOffice/LibreOffice行为显示，带column_break_after的
                // 哑元空段落，其可见行高取决于*下一段*（通常是紧跟的标题，
                // 如"材料"Heading1 16pt）的字号，而不是*上一段*的字号。
                // 用户在OnlyOffice里选中该空段落验证：选区高度等于下一段
                // 标题的字高，不是自己Normal 11pt的字高。这与我们原先
                // last_font_size.max(font_size)（看上一段）的逻辑方向相反。
                // 只对带column_break_after的空段落生效这条"看下一段"规则，
                // 避免影响其他不涉及分栏符的普通空段落（那些维持原逻辑，
                // 未经真实样本验证，不贸然扩大改动范围）。
                let effective_font_size = if para.column_break_after {
                    let next_font = doc.body.get(para_index + 1).and_then(|next_elem| {
                        if let BodyElement::Paragraph(next_para) = next_elem {
                            Some(resolve_para_style(doc, next_para, self.registry.default_font_size).font_size)
                        } else {
                            None
                        }
                    });
                    next_font.unwrap_or(font_size).max(font_size)
                } else {
                    last_font_size.max(font_size)
                };
                let line_h = effective_font_size * 1.3;
                let mut lines  = vec![LineBox {
                    y_offset:    0.0,
                    ascent:      effective_font_size,
                    descent:     effective_font_size * 0.2,
                    line_height: line_h,
                    text:        String::new(),
                    runs:        Vec::new(),
                    ..Default::default()
                }];
                // v250: 空列表项也要显示标记并缩进（编号计数器照常累加）
                if let Some(li) = para.list_info.as_ref() {
                    let eb: Option<(String, f32, f32)> = doc.styles.list_styles.get(&li.style_name)
                        .and_then(|ls| ls.level(li.level))
                        .map(|level_style| match level_style {
                            roe_model::writer::ListLevelStyle::Bullet(b) => {
                                (format!("{}{}{}", b.prefix, b.bullet_char, b.suffix),
                                 b.indent.margin_left, b.indent.text_indent)
                            }
                            roe_model::writer::ListLevelStyle::Number(n) => {
                                let counters = list_counters.entry(li.list_id.clone()).or_default();
                                let level = li.level as usize;
                                while counters.len() <= level { counters.push(n.start_value); }
                                counters.truncate(level + 1);
                                let count = counters[level];
                                counters[level] += 1;
                                let num_str = match n.format {
                                    roe_model::writer::NumFormat::Arabic => count.to_string(),
                                    roe_model::writer::NumFormat::LowerAlpha => roe_parser::ooxml::docx::format_num(count, "lowerLetter"),
                                    roe_model::writer::NumFormat::UpperAlpha => roe_parser::ooxml::docx::format_num(count, "upperLetter"),
                                    roe_model::writer::NumFormat::LowerRoman => roe_parser::ooxml::docx::format_num(count, "lowerRoman"),
                                    roe_model::writer::NumFormat::UpperRoman => roe_parser::ooxml::docx::format_num(count, "upperRoman"),
                                    roe_model::writer::NumFormat::None => String::new(),
                                };
                                (format!("{}{}{}", n.prefix, num_str, n.suffix),
                                 n.indent.margin_left, n.indent.text_indent)
                            }
                        });
                    if let Some((label, ml, ti)) = eb {
                        if let Some(line) = lines.first_mut() {
                            line.x_offset = ml;
                            line.text_x_offset = ml + ti;
                            line.list_label = Some(label);
                            line.list_label_x = ml + ti
                                - if li.style_name.starts_with("ROE_") { 18.0 } else { 0.0 };
                        }
                    }
                }
                self.place_paragraph(
                    &mut arena, &mut cur_page, &mut cur_column,
                    para_index, lines, resolved.space_before, space_after, 0.0,
                    content_h, cur_section_index,
                    &mut cur_page_in_section, &mut section_just_changed,
                    None,
                    Some((doc, &page_footnote_ids, &para.runs)),
                );
                for r in &para.runs {
                    if let Some(id) = r.footnote_ref {
                        page_footnote_ids.entry(cur_page).or_default().insert(id);
                    }
                }
                continue;
            }

            // ── cosmic-text shaping（D3：per-run 多样式，set_rich_text）──
            // D3 设计：不再用整段统一 Attrs 的 set_text，改为对 para.runs 逐个
            // 调 resolve_run_style/resolve_run_font_family，构造各自的 Attrs，
            // 通过 metadata(run_idx) 标记每个 span 来源，shape 完后从
            // LayoutGlyph.metadata 反查 glyph 属于哪个原始 run，从而把每个
            // LineBox 覆盖到的 run 边界切分进 LineBox.runs。
            // 已知技术债（记录于交接文档，留待后续）：
            //   - 不处理 Paragraph.alignment/indent_left/indent_first
            //   - color/background/underline/strikethrough/script 只解析流转，
            //     不在此处渲染（渲染是 E 块 tile.rs 的职责）
            // ── WROOT-093：逐行环绕（取代原 AABB 整段估算方案）────────────
            // 原方案用"段落起始y + 估4行高度"粗估段落范围去判断是否与图片
            // y 范围重叠，一旦段落实际超过4行（手册.docx的常见情况），估算
            // 范围覆盖不到真实渲染位置，导致误判。改造后不再估算：段落在
            // 页面上的当前y坐标（used_height）在断行过程中随每一行的产出
            // 实时推进，每断一行前就查询当前 cur_page 对应的 FloatContext，
            // 取得该y坐标下真正可用的宽度（max_advance）和起始x偏移
            // （line_x）——参照 vendor/parley/parley_tests/tests/floats.rs
            // 的 layout_floats() 模式。有无图片注册进当前页的 FloatContext
            // 已在 BodyElement::Frame 分支处理（仅贴左/贴右的 square/tight/
            // through 图片会被注册，见该分支注释）。
            // 段落起始y（相对页面内容区顶部，与 FloatContext 用的坐标系一致，
            // 供后面断行循环换算每一行的 min_y）。
            let para_y_start = arena.used_height(cur_page);
            // WROOT-037-B：本段因避让浮动框（图片横向溢出版心导致某y区间
            // 完全占满）而需要额外增加的顶部间距，在下面断行阶段（首行分支）
            // 探测到负宽度、发生跳转时被赋值；默认0.0，不影响无浮动框场景。
            let mut para_extra_offset_y: f32 = 0.0;
            if let Some(ref mut m) = para_y_start_out {
                m.entry(para_index).or_insert(para_y_start);
            }
            // WROOT-036 Bug A：元组新增第4个字段 self_labeled。
            // - false（ODF路径，走 list_styles 查找）：label 需要下面单独画（原有行为不变）。
            // - true （DOCX兜底路径）：label 在 docx.rs 解析阶段已经被硬编码拼进了
            //   run 文字（见 docx.rs 里 `p.runs.insert(0, Run { text: format!("{label} ")..})`），
            //   会被 cosmic-text 当成正文的一部分正常 shape 出来。这种情况下绝对不能再走
            //   list_label 单独绘制，否则 label 会被画两次（run文字里一次、tile.rs画label
            //   逻辑里再一次）。
            let list_bullet: Option<(String, f32, f32, bool)> = para.list_info.as_ref().and_then(|li| {
                doc.styles.list_styles.get(&li.style_name)
                    .and_then(|ls| ls.level(li.level))
                    .map(|level_style| match level_style {
                        roe_model::writer::ListLevelStyle::Bullet(b) => {
                            let label = format!("{}{}{}", b.prefix, b.bullet_char, b.suffix);
                            (label, b.indent.margin_left, b.indent.text_indent, false)
                        }
                        roe_model::writer::ListLevelStyle::Number(n) => {
                            // 计数器状态机最简路径：只支持 Arabic，无 continue-numbering，
                            // 无 display_levels。子级（level 更深）重置为 start_value，
                            // 回到父级/同级延续原计数。
                            let counters = list_counters.entry(li.list_id.clone())
                                .or_default();
                            let level = li.level as usize;
                            // 补齐到当前 level 长度，新出现的更深 level 用 start_value 初始化
                            while counters.len() <= level {
                                counters.push(n.start_value);
                            }
                            // 截断比当前 level 更深的旧计数（回到浅层后，深层计数失效）
                            counters.truncate(level + 1);
                            let count = counters[level];
                            counters[level] += 1;
                            let num_str = match n.format {
                                roe_model::writer::NumFormat::Arabic => count.to_string(),
                                roe_model::writer::NumFormat::LowerAlpha => roe_parser::ooxml::docx::format_num(count, "lowerLetter"),
                                roe_model::writer::NumFormat::UpperAlpha => roe_parser::ooxml::docx::format_num(count, "upperLetter"),
                                roe_model::writer::NumFormat::LowerRoman => roe_parser::ooxml::docx::format_num(count, "lowerRoman"),
                                roe_model::writer::NumFormat::UpperRoman => roe_parser::ooxml::docx::format_num(count, "upperRoman"),
                                roe_model::writer::NumFormat::None => String::new(),
                            };
                            let label = format!("{}{}{}", n.prefix, num_str, n.suffix);
                            (label, n.indent.margin_left, n.indent.text_indent, false)
                        }
                    })
                    // WROOT-036 Bug A 兜底：DOCX 段落的 list_info.style_name 存的是
                    // 段落样式名（cur_style_id），不是编号样式名，doc.styles.list_styles
                    // 对 DOCX 文档查不到对应条目（ListLevelStyle::Number/Bullet 全项目
                    // 只有 ODF 解析器 fodt.rs 会构造），上面的查找链必然返回 None。
                    // 但 docx.rs 已经把 numbering.xml 里 w:ind 解析出的缩进值写进了
                    // para.indent_left / para.indent_first（段落通用字段，两种格式共用），
                    // 这里直接读它们作为缩进来源。label 留空字符串——self_labeled=true
                    // 意味着下面不会用到这个 label 字段。
                    .or_else(|| {
                        if para.list_info.is_some() {
                            let margin_left = para.indent_left.unwrap_or(0.0);
                            let text_indent = para.indent_first.unwrap_or(0.0);
                            Some((String::new(), margin_left, text_indent, true))
                        } else {
                            None
                        }
                    })
            });
            // WROOT-126：普通段落（无 list_info）的左缩进/首行缩进/悬挂。
            // 此前只有列表分支消费 indent_left/indent_first，普通段落 effective_w 不减缩进、
            // final_x_offset 不加缩进，导致 w:ind 对普通段落完全无效。
            // 复用列表 self_labeled 路径（label 为空、不设 list_label，首行 margin_left+text_indent，
            // 续行 margin_left），断行宽度经下面 list_margin_left 自动缩窄。
            // 取值：段落直写优先，其次样式（basedOn 继承已由 resolve_paragraph_style 处理）。
            // indent_first 约定：正=首行缩进，负=悬挂（解析层已取负）。
            // 仅处理左缩进/首行，右缩进未建模（WROOT-127 另做）。
            let list_bullet: Option<(String, f32, f32, bool)> = if list_bullet.is_some() || para.list_info.is_some() {
                list_bullet
            } else {
                let pstyle_ind = doc.styles.resolve_paragraph_style(para.style.0.as_str());
                let ind_left  = para.indent_left.or(pstyle_ind.indent_left).unwrap_or(0.0);
                let ind_first = para.indent_first.or(pstyle_ind.indent_first).unwrap_or(0.0);
                if ind_left != 0.0 || ind_first != 0.0 {
                    Some((String::new(), ind_left, ind_first, true))
                } else {
                    None
                }
            };
            let list_margin_left = list_bullet.as_ref().map(|(_, m, _, _)| *m).unwrap_or(0.0);
            // WROOT-114 阶段二：段落样式带左边框（w:pBdr/w:left）时，文字需整体
            // 内推 边框宽度 + w:space 的距离（本用例 6pt+6pt=12pt）。仅样式带
            // border_left 的段落非 0，其余恒为 0（零回归）。本轮只在 list 路径
            // 的 text_x_offset 消费；无编号的普通段落路径待下一轮单独验证。
            let border_inset: f32 = doc.styles
                .resolve_paragraph_style(para.style.0.as_str())
                .border_left
                .map(|b| b.width_pt + b.space_pt)
                .unwrap_or(0.0);
            // 分栏排版设计文档v1 四、4.3节：段落换行宽度应改为所在栏的宽度，
            // 而不是整页 content_w。单栏文档 col_widths[0] == content_w，
            // 对现有行为无影响；分栏文档时用当前栏的真实宽度断行。
            let col_w = arena.pages[cur_page].columns.col_widths
                .get(cur_column).copied().unwrap_or(content_w);
            // WROOT-093：effective_w 不再减 float_indent（图片造成的收窄已
            // 下放到逐行断行循环里按 FloatContext 实时查询），这里只保留
            // list_margin_left（列表缩进，与图片环绕无关的独立需求）。
            // layout_max_advance 用作断行的"硬上限"——即使某行因为不在
            // 图片y范围内可以用满宽，也不能超过减去列表缩进后的宽度。
            // WROOT-127：右缩进。para.indent_right 来自正文段落直写 w:ind w:right/w:end
            // （样式级右缩进尚未建模）。右缩进使每一行可用宽度整体缩窄，与图文环绕的
            // 逐行收窄取 min，故只需在这一处上限里减去。
            let indent_right_pt = para.indent_right.unwrap_or(0.0).max(0.0);
            let effective_w = (col_w - list_margin_left - border_inset - indent_right_pt).max(col_w * 0.3);

            let run_families: Vec<Option<String>> = para.runs.iter()
                .map(|r| resolve_run_font_family(doc, para, r))
                .collect();
            let mut run_styles: Vec<crate::layout::ResolvedRunStyle> = para.runs.iter()
                .map(|r| resolve_run_style(doc, para, r, self.registry.default_font_size))
                .collect();

            // 批注高亮/竖线状态叠加（v150起，渲染层第一步）：resolve_run_style
            // 是纯函数（逐run独立调用，见函数doc注释），无法感知"前面哪些
            // 批注range还没结束"这种跨run上下文，这里用一个独立的第二遍循环
            // 顺序遍历para.runs，维护一个"当前活跃批注栈"来补算。
            // 不改resolve_run_style签名本身——它在1650/1657行等处仍被直接
            // 调用取font_size，保持纯函数语义不受影响，改动面更小。
            //
            // 方案A竖线规则（已与用户确认，对齐OnlyOffice真实行为）：不管
            // 单行还是跨行，一个批注range的第一个run画起始竖线，最后一个
            // run画终止竖线，中间run不画——不需要判断"当前是第几行/共几
            // 行"，只需要知道"是不是这个批注在runs序列里第一次/最后一次
            // 出现"，这两个条件在下面的栈操作里天然可判断，无需额外状态。
            //
            // 颜色分配：Comment.id % COMMENT_HIGHLIGHT_COLORS.len()（颜色
            // 数组本身定义在tile.rs，这里只存索引，保持ResolvedRunStyle
            // 的Copy约束——见结构体定义处注释）。
            //
            // 重叠批注取色规则（已与用户确认）：多个批注同时活跃时，取
            // 活跃栈栈顶（最内层/最晚打开的那个）作为高亮颜色，与
            // comment_ref图标通常指向"最贴近这段文字"的那条批注保持视觉
            // 一致；这是真实文档里的边缘情况（正常批注互不重叠），不需要
            // 过度设计，取栈顶足够。
            {
                // (comment_id, color_idx)，栈顶=最内层/最晚打开的批注。
                let mut active_comments: Vec<(u32, u8)> = Vec::new();
                const COMMENT_HIGHLIGHT_COLOR_COUNT: u32 = 6; // 与tile.rs颜色表长度保持一致（v150续修：粉/蓝/绿/浅紫/橙/浅红6色，橙替换原黄色——黄偏冷不适合做高亮，已与用户确认）
                for (run_idx, run) in para.runs.iter().enumerate() {
                    // 1. 先处理本run携带的批注range起点：压栈，且标记本run为
                    //    该批注的起始竖线位置。
                    for &cid in &run.comment_range_start {
                        let color_idx = (cid % COMMENT_HIGHLIGHT_COLOR_COUNT) as u8;
                        active_comments.push((cid, color_idx));
                        run_styles[run_idx].comment_bar_start = true;
                    }
                    // 2. 若此刻处于任意批注range内，用栈顶批注的颜色给本run
                    //    染色（栈顶=最内层）。start/end都在本run时，这里会
                    //    在start已压栈后立即生效，同一run正确获得高亮。
                    if let Some(&(top_comment_id, top_color_idx)) = active_comments.last() {
                        run_styles[run_idx].comment_highlight_color_idx = Some(top_color_idx);
                        run_styles[run_idx].comment_id = Some(top_comment_id);
                    }
                    // 3. 再处理本run携带的批注range终点：标记终止竖线，并
                    //    出栈。真实文档start/end不会跨run.push被截断（同一
                    //    个w:r内部或紧邻裸标签之间的关系），但用retain防御性
                    //    处理"end先于对应start入栈之外的顺序"这类异常情况，
                    //    不会panic。
                    for &cid in &run.comment_range_end {
                        run_styles[run_idx].comment_bar_end = true;
                        active_comments.retain(|&(id, _)| id != cid);
                    }
                }
            }

            // Parley 迁移（D3 主循环）：cosmic-text 的 set_rich_text(spans, ...)
            // 靠 Vec<(&str, Attrs)> 顺序隐式表达每个 run 的文字片段，Parley 的
            // ranged_builder 需要先给完整 full_text，再用显式字节 range 指定
            // 每段样式作用区间——这里手动按 run.text.len() 累计每个 run 的
            // 字节偏移，取代原来"每个 span 天然是独立 &str 切片"的隐式写法。
            let default_brush = RunBrush { color: [0, 0, 0, 255], run_idx: None };
            let mut builder = self.registry.layout_context.ranged_builder(
                &mut self.registry.font_context, &full_text, 1.0, true,
            );
            builder.push_default(StyleProperty::Brush(default_brush));
            builder.push_default(GenericFamily::SystemUi);
            builder.push_default(LineHeight::Absolute(metrics.line_height));
            builder.push_default(StyleProperty::FontSize(metrics.font_size));

            // WROOT-055-RANGE: Parley 在同字体同字号的相邻 run 之间会把
            // 多个不同颜色的 style range 合并成同一个物理 GlyphRun（一个
            // GlyphRun 不再保证只对应一个原始 run），导致后面收集 LineRunSpan
            // 时如果用 glyph_run.run().text_range() 反查字节范围，拿到的是
            // 合并后的整体范围而非单个 run 的范围，多个不同颜色的 run 会
            // 互相覆盖成最后一个 run 的颜色（bug 现象：一整行文字全部变成
            // 最后一个 run 的颜色）。这里改为自己维护 run_idx -> 字节范围
            // 的映射，后面统一用这个表查，不再信任 Parley 返回的 text_range。
            let mut run_byte_ranges: Vec<(usize, usize)> = Vec::with_capacity(para.runs.len());
            let mut run_byte_offset = 0usize;
            for (run_idx, run) in para.runs.iter().enumerate() {
                let rs = &run_styles[run_idx];
                let start = run_byte_offset;
                // WROOT-025: full_text 现在是 expanded_run_texts 拼接而成
                // （tab 已展开成空格），必须用展开后的长度累加字节偏移，
                // 不能再用原始 run.text.len()（含未展开的\t字节数），否则
                // 这里算出的 range 会和 full_text 里真实的字节位置对不上，
                // 导致颜色/粗体/斜体等样式错位到别的文字上（v85 PROOT-035
                // 踩坑记录的同一类问题）。
                let end = start + expanded_run_texts[run_idx].len();
                run_byte_offset = end;
                run_byte_ranges.push((start, end));
                let range = start..end;

                // WROOT-FONTFAMILY修复: run_families（第646-648行已算出）
                // 此前从未真正传给 Parley builder，全部run统一走
                // GenericFamily::SystemUi 默认字体，导致断行测量用的字宽
                // 跟文档实际指定字体（如SimSun→fallback到Noto Sans CJK SC）
                // 不一致，产生和OnlyOffice/LibreOffice不同的换行结果。
                // 此处按run补上真正的字体族，与颜色/字号等字段的per-run
                // push模式保持一致；没有解析出字体名的run维持push_default
                // 的GenericFamily::SystemUi兜底不变。
                if let Some(family_name) = &run_families[run_idx] {
                    builder.push(
                        StyleProperty::FontFamily(FontFamily::Source(family_name.as_str().into())),
                        range.clone(),
                    );
                }

                // WROOT-080修复：公式run不再靠空格文字撑宽度（会被Parley
                // align()当成trailing_whitespace从居中计算里排除，详见
                // docx.rs公式run构造处注释），改用Parley原生InlineBox
                // 机制——直接把width_pt/height_pt+depth_pt作为盒子尺寸
                // 交给Parley，居中/对齐会把这个盒子当成真正的行内容参与
                // free_space计算，不会被当空白忽略。run.text此时是空
                // 字符串（start==end），不走下面的文字样式range分支。
                if let Some(formula) = &run.formula {
                    builder.push_inline_box(parley::InlineBox {
                        id: run_idx as u64,
                        kind: parley::InlineBoxKind::InFlow,
                        index: start,
                        width: formula.width_pt,
                        height: formula.height_pt + formula.depth_pt,
                        baseline: Some(formula.height_pt),
                    });
                    continue;
                }

                if range.start >= range.end { continue; } // 空 run 不 push 样式，避免 Parley 对空 range 报错

                // WROOT-055 续修：此前这里硬编码 [0,0,0,255]（纯黑），
                // rs.color（resolve_run_style 已经按 段落样式->字符样式->
                // Run内联 优先级正确算出）从未被真正用来设置画笔颜色，
                // 导致任何非默认黑色文字（包括这次超链接的藏青色、以及
                // WROOT-RUNCOLOR/WROOT-DOCDEFAULT 两次此前的解析层修复）
                // 全部在这一步被悄悄改回黑色，是比 WROOT-055 本身更底层
                // 的独立缺陷，这次顺带一并修正。
                let run_color_rgba = rs.color
                    .map(|c| [c.r, c.g, c.b, c.a])
                    .unwrap_or([0, 0, 0, 255]);
                builder.push(StyleProperty::Brush(RunBrush { color: run_color_rgba, run_idx: Some(run_idx) }), range.clone());
                builder.push(StyleProperty::FontSize(rs.font_size), range.clone());
                if let Some(fam) = &run_families[run_idx] {
                    builder.push(StyleProperty::FontFamily(parley::FontFamily::named(fam.as_str())), range.clone());
                }
                if rs.bold   { builder.push(StyleProperty::FontWeight(FontWeight::BOLD), range.clone()); }
                if rs.italic { builder.push(StyleProperty::FontStyle(FontStyle::Italic), range.clone()); }
            }

            // WROOT-078：对齐不再是 shape 调用的参数，而是 build 完之后单独
            // 调用 layout.align(...)（Parley 没有"构建时传对齐"的写法）。
            let parley_align = match resolved.alignment {
                Some(roe_model::style::Alignment::Left)    => Alignment::Start,
                Some(roe_model::style::Alignment::Center)  => Alignment::Center,
                Some(roe_model::style::Alignment::Right)   => Alignment::End,
                Some(roe_model::style::Alignment::Justify) => Alignment::Justify,
                None => Alignment::Start,
            };
            let mut layout: parley::Layout<RunBrush> = builder.build(&full_text);

            // WROOT-093：逐行断行，取代 break_all_lines 一次性整段断行。
            // 参照 vendor/parley/parley_tests/tests/floats.rs 的
            // layout_floats() 模式：每断一行前，用当前已提交的 y 坐标
            // （段落起始y + 本段内已产出的行高累计，即 breaker.state().line_y()）
            // 查询该页 FloatContext，取得这一行的 max_advance / line_x。
            // per_line_x 按行顺序记录，供下面收集 LineBox 的循环按行索引
            // 取用（该循环遍历 layout.lines()，顺序与这里产出顺序一致）。
            let mut per_line_x: Vec<f32> = Vec::new();
            {
                let mut breaker = layout.break_lines();
                let state = breaker.state_mut();
                state.set_layout_max_advance(effective_w);
                // 段落第一行的 min_y 用 para_y_start（段落在页面上的起始y，
                // 断行过程中 line_y 会随每行推进，无需再手动累加）。
                if let Some(fctx) = float_contexts.get(&cur_page) {
                    // WROOT-101修复：find_content_slot 对 min_y 的匹配依赖
                    // taffy 内部 segment 划分。当浮动框的起始 y（如 tblpY 声明的
                    // 极小偏移，实测样本中为 1 twip=0.05pt）与 para_y_start 之间
                    // 存在亚像素级差异时，taffy 会在 [0, float_y) 生成一个未收窄
                    // 的占位 segment，导致恰好等于该边界之前的查询点被误判为
                    // "浮动框尚未开始"，从而拿到满宽结果而不是收窄结果。
                    // 用 1pt 容差（远大于典型的亚像素误差、但远小于任何可感知的
                    // 视觉排版单位）保证首行查询能落入浮动框已生效的 segment。
                    const FLOAT_QUERY_EPSILON: f32 = 1.0;
                    // WROOT-093-C：单点查询升级为区间查询。用本段的绝对行高
                    // metrics.line_height（第1113行 self.registry.metrics(...)
                    // 算出，本段内已知的精确值，非估算）作为候选行的高度区间，
                    // 取区间内最窄的安全宽度断第一行，取代原来只查
                    // para_y_start 一个点、可能在行内跨界的做法。
                    let mut probe_min_y = para_y_start + FLOAT_QUERY_EPSILON;
                    let (mut safe_w, mut safe_x) = find_safe_content_slots(
                        fctx,
                        probe_min_y,
                        probe_min_y + metrics.font_size * 2.0,
                        effective_w,
                    );
                    // WROOT-037-B：safe_w<=0 说明这一整个y区间被浮动框完全占满
                    // （常见于图片宽度超过版心，横向溢出两侧），此时不能把非法
                    // 宽度传给断行器（会导致这一行排版失败/内容消失，即WROOT-037
                    // "1"文字消失的根因）。用前置扫描记录的 float_y_ranges 查出
                    // 占满当前探测点的浮动框的 y_end，跳到该 y_end 重新探测，
                    // 循环直到拿到正数宽度或达到重试上限（兜底放弃收窄，用满宽，
                    // 保证不丢内容——即使视觉上会与图片重叠，也好过完全消失）。
                    //
                    // 重要：这里只是为了拿到"这一行的可用宽度/x偏移"，不代表
                    // 这一行最终就画在 probe_min_y 这个绝对y坐标——最终的段落
                    // 起始y是由 place_paragraph 的 space_before 参数决定的
                    // （下面 para_extra_offset_y 记录了因避让产生的额外顶部
                    // 间距，累加进 space_before 一并传给 place_paragraph）。
                    // 之前一版错误地直接改了 state.set_line_y(probe_min_y)，
                    // 但"收集LineBox"阶段的相对y坐标是独立从0开始累加的
                    // （见该阶段 `let mut y = 0.0f32;`），跟断行阶段的
                    // set_line_y 互不相通，导致那次改动只让断行本身成功、
                    // 但内容仍被摆在页面顶部与图片重叠、视觉上"消失"。
                    const MAX_NEGW_RETRY: u32 = 8;
                    let mut negw_retry = 0u32;
                    while safe_w <= 0.0 && negw_retry < MAX_NEGW_RETRY {
                        let jump_y = float_y_ranges.get(&cur_page)
                            .and_then(|ranges| {
                                ranges.iter()
                                    .filter(|(y0, y1)| *y0 <= probe_min_y && probe_min_y < *y1)
                                    .map(|(_, y1)| *y1)
                                    .fold(None, |acc: Option<f32>, y1| {
                                        Some(acc.map_or(y1, |a| a.max(y1)))
                                    })
                            });
                        let Some(jump_y) = jump_y else { break; };
                        if jump_y <= probe_min_y { break; }
                        probe_min_y = jump_y + FLOAT_QUERY_EPSILON;
                        let (w2, x2) = find_safe_content_slots(
                            fctx,
                            probe_min_y,
                            probe_min_y + metrics.font_size * 2.0,
                            effective_w,
                        );
                        safe_w = w2;
                        safe_x = x2;
                        negw_retry += 1;
                    }
                    // 因避让浮动框而需要给整个段落额外增加的顶部间距，供本
                    // 段末尾调用 place_paragraph 时累加进 space_before。
                    // negw_retry==0（没触发跳转）时保持0.0，不影响原有行为。
                    // WROOT-037-GAP修正：跳转本身（probe_min_y）用的是钳制
                    // 后taffy真实占用区间，这部分不能变——但"1"视觉上该往下
                    // 移多少，应该贴合图片真实底边（visual_float_y_ranges，
                    // 未钳制），而不是直接照抄 probe_min_y（那样等于贴合了
                    // taffy钳制区间的底边，比图片真实底边多出|real_float_y|
                    // 那么多空白，就是本次会话要修的现象）。
                    // 若 negw_retry==0（没触发跳转），说明这一行本来就没被
                    // 浮动框挡住，此时不需要额外视觉偏移，保持0.0。
                    let visual_offset = if negw_retry > 0 {
                        visual_float_y_ranges.get(&cur_page)
                            .and_then(|ranges| {
                                ranges.iter()
                                    .map(|(_, y1)| *y1)
                                    .fold(None, |acc: Option<f32>, y1| {
                                        Some(acc.map_or(y1, |a| a.max(y1)))
                                    })
                            })
                            .map(|y1| (y1 + FLOAT_QUERY_EPSILON - para_y_start).max(0.0))
                            .unwrap_or(0.0)
                    } else {
                        0.0
                    };
                    para_extra_offset_y = visual_offset;
                    eprintln!("[DEBUG WROOT037-GAP] para_y_start={:.2} probe_min_y(钳制版,仅供跳转)={:.2} para_extra_offset_y(视觉版,已改用真实底边)={:.2}", para_y_start, probe_min_y, para_extra_offset_y);
                    state.set_line_max_advance(safe_w.max(1.0).min(effective_w));
                    state.set_line_x(safe_x);
                    state.set_line_y(para_y_start as f64);
                } else {
                    state.set_line_max_advance(effective_w);
                    state.set_line_y(para_y_start as f64);
                }

                // WROOT-093-B → WROOT-093-C 后降级为兜底分支：
                // 自 find_safe_content_slots 引入后，断行前已经用区间查询
                // （[y, y+line_height]）取过安全宽度，正常情况下这一行不应
                // 再跨界。本段校验保留作为防御——万一实际行高超出预估的
                // metrics.line_height（例如软换行/特殊场景导致行高与预期
                // 不符），仍能兜底重试，只是预期触发频率从"常态"降为"罕见"。
                const MAX_CROSS_BOUNDARY_RETRY: u32 = 3;

                loop {
                    let Some(mut yield_data) = breaker.break_next() else { break };

                    if let YieldData::LineBreak(lb_data) = &yield_data {
                        let mut lb_data = lb_data.clone();
                        let mut retry = 0u32;
                        loop {
                            let start_slot = float_contexts.get(&cur_page).map(|fctx| {
                                fctx.find_content_slot(lb_data.line_y_start as f32, [0.0, 0.0], Clear::None, None)
                            });
                            let end_slot = float_contexts.get(&cur_page).map(|fctx| {
                                fctx.find_content_slot(lb_data.line_y_end as f32, [0.0, 0.0], Clear::None, None)
                            });

                            let crosses_boundary = match (&start_slot, &end_slot) {
                                (Some(s), Some(e)) => s.segment_id != e.segment_id,
                                _ => false,
                            };

                            if !crosses_boundary {
                                break;
                            }
                            if retry >= MAX_CROSS_BOUNDARY_RETRY {
                                eprintln!(
                                    "[WARN WROOT-093-B] 跨图文边界断行重试达到上限({}次)，接受当前行结果，line_y_start={:.1} line_y_end={:.1}",
                                    MAX_CROSS_BOUNDARY_RETRY, lb_data.line_y_start, lb_data.line_y_end
                                );
                                break;
                            }

                            if !breaker.revert() {
                                // 理论上不该发生：刚断完一行必然有 prev_state 可 revert。
                                // 防御性退出，接受当前（未收窄）结果，避免panic。
                                break;
                            }
                            if let Some(end_slot) = &end_slot {
                                let state = breaker.state_mut();
                                state.set_line_max_advance(end_slot.width.min(effective_w));
                                state.set_line_x(end_slot.x);
                            }
                            let Some(new_yield) = breaker.break_next() else { break };
                            if let YieldData::LineBreak(new_lb) = &new_yield {
                                lb_data = new_lb.clone();
                            }
                            yield_data = new_yield;
                            retry += 1;
                        }
                    }

                    // 确认这一行不再需要 revert 之后，才记录 per_line_x，
                    // 避免被 revert 撤销的行污染行号顺序。
                    let state = breaker.state_mut();
                    per_line_x.push(state.line_x());

                    if let YieldData::LineBreak(_) = yield_data {
                        let y = state.line_y();
                        if let Some(fctx) = float_contexts.get(&cur_page) {
                            // WROOT-093-C：同上，用 metrics.line_height 圈出候选
                            // 行高区间，取区间内最窄安全宽度，取代原来只查
                            // 行起点y一个点、可能在行内跨界的做法。这是本次架构
                            // 修正的核心改动点——"先框定候选行范围，再查询"，
                            // 而不是"先断出一行，再检查是否跨界"。
                            let (safe_w, safe_x) = find_safe_content_slots(
                                fctx,
                                y as f32,
                                y as f32
                                // WROOT-093-D：同上，改用更保守的 font_size*2.0 预估，
                                // 不信任 metrics.line_height 这个纯数学近似值。
                                + metrics.font_size * 2.0,
                                effective_w,
                            );
                            state.set_line_max_advance(safe_w);
                            state.set_line_x(safe_x);
                        } else {
                            state.set_line_max_advance(effective_w);
                            state.set_line_x(0.0);
                        }
                    }
                }
                breaker.finish();
            }

            layout.align(parley_align, AlignmentOptions::default());

            // 收集 LineBox。D3 原逻辑靠逐 glyph 扫描 metadata 手动合并连续同
            // run_idx 的片段；Parley 已经按 style range 把一行切成了若干个
            // GlyphRun（一个 GlyphRun 天然对应一段连续同 run_idx 区间），
            // 不需要再手动判断"same_run && 连续"才合并，直接一对一转换即可。
            let mut lines = Vec::new();
            let mut y = 0.0f32;
            for (line_idx, line) in layout.lines().enumerate() {
                let line: parley::Line<'_, RunBrush> = line;
                let lm = line.metrics();
                let line_text_range = line.text_range();
                let mut line_text = full_text.get(line_text_range.clone()).unwrap_or("").to_string();
                // WROOT-040-HYPHEN-RENDER: 断词第二步。预处理阶段插入的 U+00AD
                // SOFT HYPHEN 只是给 parley 提供断行候选点，本身不可见；行尾如果
                // 真的在此处断开，需要在这里替换成可见的连字符 '-' 才能让用户看到
                // 断词效果。U+00AD 和 '-' 在 UTF-8 下都是 1 字节，原地替换不改变
                // 字节长度，因此不影响 hit_test / cursor_inline / selection_rects /
                // cursor_move_line / layout_query 等下游依赖 line.text.len() 做字节
                // 偏移计算的逻辑（已排查确认，这些逻辑只依赖长度不依赖具体字符）。
                if line_text.ends_with('­') {
                    line_text.pop();
                    line_text.push('-');
                }

                let mut run_spans: Vec<crate::frame::LineRunSpan> = Vec::new();
                // DOCX-8-v2(Step4/5): 本行若含公式run，取公式height_pt/depth_pt
                // 最大值，供后面跟常规文字的ascent/descent取max。
                let mut line_formula_ascent: f32 = 0.0;
                let mut line_formula_descent: f32 = 0.0;

                for item in line.items() {
                    // WROOT-080修复：公式run现在走InlineBox分支（不再产生
                    // GlyphRun，run.text已是空字符串），这里新增分支处理，
                    // 从Parley对齐后的最终盒子坐标(positioned_box.x/y)构造
                    // LineRunSpan，line_formula_ascent/descent的计算也从
                    // 原GlyphRun分支挪到这里（公式尺寸信息来源不变，仍是
                    // para.runs[run_idx].formula.height_pt/depth_pt，只是
                    // 触发时机从"遇到公式的GlyphRun"改成"遇到公式的InlineBox"）。
                    if let PositionedLayoutItem::InlineBox(positioned_box) = item {
                        let run_idx = positioned_box.id as usize;
                        if let Some(r) = para.runs.get(run_idx) {
                            if let Some(f) = &r.formula {
                                if f.height_pt > line_formula_ascent {
                                    line_formula_ascent = f.height_pt;
                                }
                                if f.depth_pt > line_formula_descent {
                                    line_formula_descent = f.depth_pt;
                                }
                            }
                        }
                        let rstyle = run_styles.get(run_idx).copied()
                            .unwrap_or(run_styles.first().copied().unwrap_or(
                                crate::layout::ResolvedRunStyle {
                                    font_size: metrics.font_size,
                                    font_family_is_set: false,
                                    bold: resolved.bold,
                                    italic: resolved.italic,
                                    underline: false,
                                    strikethrough: false,
                                    color: None,
                                    background: None,
                                    script: roe_model::writer::ScriptPosition::Normal,
                                    is_formula: true,
                                    comment_highlight_color_idx: None,
                                    comment_bar_start: false,
                                    comment_bar_end: false,
                                    comment_id: None,
                                    vertical_position: None,
                                }
                            ));
                        // index是构建阶段传入的full_text字节偏移（run的起点），
                        // 公式run.text为空字符串，start==end，LineRunSpan的
                        // start/end都用这个偏移即可（下游按is_formula+formula_box
                        // 判断，不依赖end-start的长度）。
                        let byte_pos = positioned_box.x as usize; // 占位，不用于渲染
                        let _ = byte_pos;
                        run_spans.push(crate::frame::LineRunSpan {
                            start: 0,
                            end:   0,
                            style: rstyle,
                            run_idx: Some(run_idx),
                            formula_box: Some(crate::frame::FormulaBoxPosition {
                                x: positioned_box.x,
                                y: positioned_box.y,
                                baseline: positioned_box.baseline,
                            }),
                        });
                        continue;
                    }
                    let PositionedLayoutItem::GlyphRun(glyph_run) = item else { continue };
                    let style = glyph_run.style();
                    let Some(run_idx) = style.brush.run_idx else { continue };

                    // WROOT-055-RANGE: 不再用 glyph_run.run().text_range()
                    // （见上方 run_byte_ranges 声明处注释，该值在多 run 合并
                    // 场景下不可靠），改用构建阶段记录的、按 run_idx 索引的
                    // 精确字节范围，再减去这一行在 full_text 里的起始偏移
                    // 换算成行内相对偏移。
                    let (run_start_abs, run_end_abs) = run_byte_ranges.get(run_idx).copied().unwrap_or((0, 0));
                    // 裁剪到当前行范围内：一个 run 跨行时，每一行只应该看到
                    // 自己那一段，不能用 run 的完整范围（否则长 run 换行场景
                    // 会算出超出本行 line_text 长度的越界字节偏移）。
                    let clamped_start_abs: usize = run_start_abs.max(line_text_range.start).min(line_text_range.end);
                    let clamped_end_abs: usize   = run_end_abs.max(line_text_range.start).min(line_text_range.end);
                    let rel_start = clamped_start_abs.saturating_sub(line_text_range.start);
                    let rel_end   = clamped_end_abs.saturating_sub(line_text_range.start);

                    let rstyle = run_styles.get(run_idx).copied()
                        .unwrap_or(run_styles.first().copied().unwrap_or(
                            crate::layout::ResolvedRunStyle {
                                font_size: metrics.font_size,
                                font_family_is_set: false,
                                bold: resolved.bold,
                                italic: resolved.italic,
                                underline: false,
                                strikethrough: false,
                                color: None,
                                background: None,
                                script: roe_model::writer::ScriptPosition::Normal,
                                is_formula: false,
                                comment_highlight_color_idx: None,
                                comment_bar_start: false,
                                comment_bar_end: false,
                                comment_id: None,
                                vertical_position: None,
                            }
                        ));
                    run_spans.push(crate::frame::LineRunSpan {
                        start: rel_start,
                        end:   rel_end,
                        style: rstyle,
                        run_idx: Some(run_idx),
                        formula_box: None,
                    });
                }

                // DOCX-8-v2(Step5): 含公式的行，ascent/descent要跟公式尺寸取max，
                // line_height相应重算（否则公式可能比行框高，画出来会跟上一行/
                // 下一行重叠）。不含公式的行行为完全不变，max(x, 0.0)恒等于x。
                let base_ascent  = lm.baseline - lm.content_block_min_coord;
                let base_descent = lm.content_block_max_coord - lm.baseline;
                let line_ascent  = base_ascent.max(line_formula_ascent);
                let line_descent = base_descent.max(line_formula_descent);
                // 公式盒子 y 换算成"相对本行顶部"：Parley 给的是排版内绝对坐标，
                // 盒子顶到文字基线 = y - lm.baseline，文字基线到行顶 = line_ascent。
                for sp in run_spans.iter_mut() {
                    if let Some(fb) = sp.formula_box.as_mut() {
                        fb.y = fb.y - lm.baseline + line_ascent;
                    }
                }
                let line_height  = if matches!((para.line_abs_pt, para.line_exact), (Some(_), Some(true)))
                    && line_formula_ascent <= 0.0 && line_formula_descent <= 0.0
                { lm.line_height } else { lm.line_height.max(line_ascent + line_descent) };
                // WROOT-078：LineMetrics.offset 就是 Parley 官方文档标注的
                // "Offset for alignment"，对齐产生的位移已经算好，直接读取，
                // 不需要像 cosmic-text 版本那样手动摸第一个 glyph 的 x 坐标。
                let align_x_offset = lm.offset;
                // WROOT-093：float_start_x 不再是段落级固定值，改为按
                // line_idx 从 per_line_x 查表取——每行的起点右移量由该行
                // 实际所在y坐标查 FloatContext 决定（逐行环绕的核心改动）。
                // get(line_idx)兜底0.0：理论上per_line_x长度应与
                // layout.lines()行数一致（同一次断行过程产出），兜底只是
                // 防御性写法，避免边界情况panic。
                let float_start_x = per_line_x.get(line_idx).copied().unwrap_or(0.0);
                // WROOT-072：环绕图片造成的起点右移（float_start_x）与对齐造成的
                // 位移（align_x_offset）是两个独立的偏移来源，须相加而非互相替代
                // —— 一行文字可以同时"因图片让出左侧空间"又"在剩余空间内居中"。
                let final_x_offset = float_start_x + align_x_offset;

                // WROOT-068 渲染算法本体：过滤出落在本行的 tab_markers（按
                // 绝对字节偏移落在 line_text_range 内），转换成行内相对偏移，
                // 调用 LOK PostFormat 两阶段算法第二遍，算出本行的 tab
                // 撑大量表 + 引导线绘制信息。tab_markers 为空（绝大多数
                // 段落，因为只有 has_tab_stops 分支才会产生）或本行没有
                // 落入任何 marker 时，两个 Vec 均为空，不产生任何行为变化。
                let markers_in_line: Vec<(usize, usize, usize)> = tab_markers.iter()
                    .filter(|m| m.byte_offset >= line_text_range.start && m.byte_offset < line_text_range.end)
                    .map(|m| (m.byte_offset - line_text_range.start, m.run_idx, m.tab_seq))
                    .collect();
                let (tab_adjustments, tab_leaders) = if markers_in_line.is_empty() {
                    (Vec::new(), Vec::new())
                } else {
                    eprintln!("[WROOT068-DEBUG] effective_w={:.2} line_text={:?} markers={:?}", effective_w, line_text, markers_in_line);
                    for ts in &para.tab_stops {
                        eprintln!("[WROOT068-DEBUG] tab_stop pos={:.2} alignment={:?} leader={:?}", ts.pos, ts.alignment, ts.leader);
                    }
                    let result = Self::compute_tab_adjustments(
                        &line_text,
                        &markers_in_line,
                        &para.tab_stops,
                        |run_idx| run_styles.get(run_idx).map(|s| s.font_size).unwrap_or(metrics.font_size),
                        &mut self.registry,
                    );
                    eprintln!("[WROOT068-DEBUG] result adjustments={:?} leaders={:?}", result.0, result.1);
                    result
                };

                lines.push(LineBox {
                    y_offset:    y,
                    ascent:      line_ascent,
                    descent:     line_descent,
                    line_height,
                    text:        line_text,
                    runs:        run_spans,
                    x_offset:      final_x_offset,
                    text_x_offset: final_x_offset,
                    tab_adjustments,
                    tab_leaders,
                    ..Default::default()
                });
                y += line_height;
            }
            if lines.is_empty() {
                lines.push(LineBox {
                    y_offset: 0.0, ascent: font_size,
                    descent: font_size * 0.2,
                    line_height: font_size * 1.3,
                    text: full_text.clone(),
                    runs: Vec::new(),
                    ..Default::default()
                });
            }

            if let Some((label, margin_left, text_indent, self_labeled)) = &list_bullet {
                // WROOT-036 Bug A修复：此前只写了x_offset（表格边框专用几何字段，
                // 对普通段落渲染无实际作用），从未写text_x_offset（真正被tile.rs
                // 读取作为文字绘制起点的字段），导致编号缩进的label画对了位置，
                // 但label后面的正文文字仍然固定画在x=0（页边距处），看起来完全
                // 没有缩进。所有行（不只首行）都要缩进到text_indent之后的位置，
                // 保证换行续行也对齐到文字起始列，而不是编号列。
                //
                // self_labeled（DOCX兜底路径）分支：label 文字已经在 run 里，
                // 和正文是同一个连续文本流，无法把 label 单独摘出来定位，只能
                // 用标准 hanging indent 做法（等同 CSS text-indent 语义）：
                // 首行整体左移 text_indent（通常为负值，即 w:hanging），让
                // label 天然探出到更左边；续行固定对齐到 margin_left。绝不能
                // 再设置 list_label / list_label_x，否则 label 会被 tile.rs
                // 额外画一次，导致重复。
                for (i, line) in lines.iter_mut().enumerate() {
                    // WROOT-109：覆盖前，line.text_x_offset 此刻等于
                    // final_x_offset = float_start_x + align_x_offset（见上方
                    // LineBox 收集处）。此前下面直接用 margin_left 整个覆盖，
                    // Parley 已算好的居中/右对齐位移 align_x_offset 被丢弃，
                    // 带编号的居中标题（如 tdf135164_cancelledNumbering 的
                    // Heading1，样式级 w:jc=center）因此靠左。这里把对齐位移
                    // 单独取出并叠加回去。对齐位移 = text_x_offset - 该行
                    // float_start_x；左对齐（Start）时 lm.offset=0，差值为 0，
                    // 行为与修复前完全一致。effective_w 已扣除 margin_left，
                    // 所以 Parley 的对齐位移是相对缩进后可用宽度的，直接叠加
                    // 到 margin_left 上正确。
                    let float_x = per_line_x.get(i).copied().unwrap_or(0.0);
                    let align_off = (line.text_x_offset - float_x).max(0.0);
                    if *self_labeled {
                        line.x_offset = *margin_left;
                        line.text_x_offset = if i == 0 {
                            *margin_left + *text_indent + align_off + border_inset
                        } else {
                            *margin_left + align_off + border_inset
                        };
                        // 不设置 list_label / list_label_x：label 已随正文一起画出。
                    } else {
                        line.x_offset = *margin_left;
                        line.text_x_offset = *margin_left + *text_indent + align_off + border_inset;
                        if i == 0 {
                            line.list_label = Some(label.clone());
                            line.list_label_x = *margin_left + *text_indent
                                - if para.list_info.as_ref().map_or(false, |li| li.style_name.starts_with("ROE_")) { 18.0 } else { 0.0 };
                        }
                    }
                }
            }

            // 更新 last_font_size 为本段所有 run 里最大的字号（run级w:sz优先于段落样式）
            let max_run_font = para.runs.iter()
                .filter_map(|r| r.font_size)
                .fold(font_size, f32::max);
            last_font_size = max_run_font;
            self.place_paragraph(
                &mut arena, &mut cur_page, &mut cur_column,
                para_index, lines, resolved.space_before, space_after, para_extra_offset_y,
                content_h, cur_section_index,
                &mut cur_page_in_section, &mut section_just_changed,
                None,
                Some((doc, &page_footnote_ids, &para.runs)),
            );
            for r in &para.runs {
                if let Some(id) = r.footnote_ref {
                    page_footnote_ids.entry(cur_page).or_default().insert(id);
                }
            }
        }

        // Step7 Stage3: endnote body rendering, second pass.
        // Fixes applied over the first pass (see progress doc for the
        // full history): numbering now uses the same format_num helper
        // and NumFormat that Step6 already uses for the superscript
        // reference marks, so lowerRoman etc renders as real roman
        // numerals instead of raw arabic digits. A hanging indent is
        // applied so the number sits flush left and all paragraph text
        // (first line and any continuation paragraphs/soft line breaks)
        // aligns to the same left edge, matching the OnlyOffice
        // reference rendering. Rule width/thickness reduced from the
        // first-pass conservative full-width/3pt guess down to a
        // shorter, thinner line closer to the OnlyOffice reference;
        // still a placeholder pending an exact spec-driven value.
        if !dry_run {
            let real_endnotes: Vec<&roe_model::writer::Note> = doc.endnotes.iter()
                .filter(|n| n.note_type == roe_model::writer::NoteType::Normal)
                .collect();
            if !real_endnotes.is_empty() {
                // WROOT-027 (v141三次修复): v141续那版"分隔线应跟正文段落
                // 等宽(content_w)"的判断依据不足——不同Office实现对这条线的
                // 处理本身就不统一（用户核实：WPS对separator/continuationSeparator
                // 一律渲染成固定短线，不区分类型、不搞页面全宽），先不追求
                // 区分separator/continuationSeparator两种类型分别给不同宽度
                // （v141文档3.3节提出的方案，需要额外传"这是不是第一次出现"
                // 的上下文，改动量较大，作为独立技术债留待以后需要时再做），
                // 统一固定为144pt短线，是v140之前长期使用、没有出过问题的
                // 已知稳定值。
                let rule_width_pt = 144.0_f32;
                let rule_thickness_pt = 1.0_f32;
                let rule_color = roe_model::common::Color { r: 0, g: 0, b: 0, a: 255 };
                let rule_margin_before = 12.0_f32;

                let rule_y = arena.page_content_global_y(cur_page)
                    + arena.used_height(cur_page)
                    + rule_margin_before;
                let rule_fid = arena.insert_horizontal_rule_frame(
                    cur_page,
                    doc.body.len(),
                    rule_width_pt,
                    rule_thickness_pt,
                    rule_color,
                    rule_margin_before,
                    rule_y,
                    rule_margin_before,
                    6.0,
                );
                let _ = rule_fid;

                let endnote_font_size = self.registry.default_font_size;
                let endnote_line_height = endnote_font_size * 1.3;
                let newline_char = char::from_u32(10).unwrap();
                let tab_char = char::from_u32(9).unwrap();
                let hanging_indent_pt = 20.0_f32;
                for note in real_endnotes {
                    let number = endnote_numbers.get(&note.id).copied().unwrap_or(0);
                    let number_label = if number == 0 {
                        "?".to_string()
                    } else {
                        let fmt_str = match doc.endnote_num_fmt {
                            roe_model::writer::NumFormat::LowerRoman => "lowerRoman",
                            roe_model::writer::NumFormat::UpperRoman => "upperRoman",
                            roe_model::writer::NumFormat::LowerAlpha => "lowerLetter",
                            roe_model::writer::NumFormat::UpperAlpha => "upperLetter",
                            roe_model::writer::NumFormat::None => "none",
                            roe_model::writer::NumFormat::Arabic => "decimal",
                        };
                        roe_parser::ooxml::docx::format_num(number, fmt_str)
                    };
                    let mut is_first_para = true;
                    for body_elem in &note.content {
                        if let BodyElement::Paragraph(p) = body_elem {
                            // WROOT-027/028 (v141续): 不再对所有note段落一律套用
                            // 固定20pt悬挂缩进——改为读这个段落自己的indent_left/
                            // indent_first（v140会话确认的结论：该不该缩进、缩进
                            // 多少，完全由文档自身的w:ind决定，不存在"note类型
                            // 该怎样"的通用规则）。算法与正文段落渲染（约1139/
                            // 1624行margin_left/text_indent那套）保持一致：
                            // indent_first为负值即hanging，此处沿用原有语义，
                            // 直接把"这段左边界该往右缩进多少"取为indent_left，
                            // 悬挂缩进导致的"首行比后续行更靠左"通过line_indent
                            // （仅第一行减去|indent_first|）实现。
                            let para_margin_left = p.indent_left.unwrap_or(0.0).max(0.0);
                            let para_indent_first = p.indent_first.unwrap_or(0.0);
                            let hanging_indent_pt = para_margin_left;
                            let endnote_effective_w = (content_w - hanging_indent_pt).max(content_w * 0.3);
                            // WROOT-027/028 (v141二次修复): 彻底废弃"编号走list_label
                            // 悬挂缩进机制"的方案——经用户用OnlyOffice非打印标记截图
                            // 核实，note编号（如"i"）本质上就是段落正文的第一个字符，
                            // 只是继承了EndnoteReference等字符样式的上标(superscript)
                            // 效果；编号后面若有间距，是文档自己的<w:tab/>字符造成的
                            // 制表符跳转效果，不是什么"编号列对齐"的排版语义。075当初
                            // "需要悬挂缩进"是对这个tab效果的误读，list_label机制是
                            // 因此被带偏发明出来的、完全不必要的特例，予以废弃（相关
                            // 代码原样注释保留在下方is_note_head分支里，不删除，
                            // 便于回退对照）。
                            // 于是这里把编号字符串直接拼进raw_text最前面，让它跟其余
                            // 正文一样走同一套Parley排版流程；上标视觉效果改由下面
                            // is_note_head分支往line.runs塞一个LineRunSpan实现，复用
                            // tile.rs里script_runs循环这套已验证可用的通用上标渲染
                            // 机制（本来就是给正文里的脚注引用编号设计的，note内容区
                            // 的编号用同一套完全合适）。
                            let mut raw_text = String::new();
                            let mut note_label_byte_len: usize = 0;
                            if is_first_para {
                                raw_text.push_str(&number_label);
                                note_label_byte_len = number_label.len();
                            }
                            for r in &p.runs {
                                raw_text.push_str(&r.text);
                            }
                            let raw_text = raw_text.replace(tab_char, " ");
                            // WROOT-027/028 fix: previously each segment split by an
                            // explicit newline_char (from <w:br/>) was pushed whole
                            // into a single LineBox with no width-based wrapping at
                            // all, so any segment longer than one physical line ran
                            // off the page edge instead of soft-wrapping. Each
                            // newline-delimited segment is now run through Parley's
                            // own line breaker (the same engine the main body loop
                            // uses) to produce properly wrapped LineBoxes, while
                            // still respecting the explicit hard breaks first.
                            let hard_segments: Vec<&str> = raw_text.split(newline_char).collect();
                            let hard_segments: Vec<&str> = if hard_segments.is_empty() {
                                vec![""]
                            } else {
                                hard_segments
                            };
                            let mut lines: Vec<LineBox> = Vec::new();
                            let mut running_line_idx: usize = 0;
                            // WROOT-113：行累计高度。此前 y_offset = idx * endnote_line_height
                            // 假设所有行等高；编号被字符样式放大（如 36pt）撑高首行后，
                            // 后续行必须基于累计高度定位，否则与首行重叠。行高一致时
                            // 累加结果与 idx * endnote_line_height 数值相同，零回归。
                            let mut running_y: f32 = 0.0;
                            for segment in hard_segments.iter() {
                                let seg_text = (*segment).to_string();
                                let default_brush = RunBrush { color: [0, 0, 0, 255], run_idx: None };
                                let mut seg_builder = self.registry.layout_context.ranged_builder(
                                    &mut self.registry.font_context, &seg_text, 1.0, true,
                                );
                                seg_builder.push_default(StyleProperty::Brush(default_brush));
                                seg_builder.push_default(GenericFamily::SystemUi);
                                seg_builder.push_default(LineHeight::Absolute(endnote_line_height));
                                seg_builder.push_default(StyleProperty::FontSize(endnote_font_size));
                                let mut seg_layout: parley::Layout<RunBrush> = seg_builder.build(&seg_text);
                                {
                                    let mut seg_breaker = seg_layout.break_lines();
                                    let seg_state = seg_breaker.state_mut();
                                    // WROOT-027 fix: set_layout_max_advance alone does
                                    // NOT constrain line width. Parley's BreakerState
                                    // has a separate line_max_advance field (defaults
                                    // to 0.0) that must be set explicitly for every
                                    // line, or break_next() treats each line as having
                                    // essentially zero available width and breaks after
                                    // almost every word. The main body loop (see the
                                    // non-float `else` branch above) always pairs
                                    // set_layout_max_advance with set_line_max_advance;
                                    // this endnote path previously only called the
                                    // former, which was the root cause of the
                                    // one-word-per-line bug.
                                    seg_state.set_layout_max_advance(endnote_effective_w);
                                    seg_state.set_line_max_advance(endnote_effective_w);
                                    loop {
                                        let Some(yield_data) = seg_breaker.break_next() else { break; };
                                        if let YieldData::LineBreak(_) = yield_data {
                                            let seg_state = seg_breaker.state_mut();
                                            seg_state.set_line_max_advance(endnote_effective_w);
                                        }
                                    }
                                    seg_breaker.finish();
                                }
                                seg_layout.align(Alignment::Start, AlignmentOptions::default());
                                for seg_line in seg_layout.lines() {
                                    let seg_line: parley::Line<'_, RunBrush> = seg_line;
                                    let range = seg_line.text_range();
                                    let text = seg_text.get(range).unwrap_or("").to_string();
                                    let is_note_head = is_first_para && running_line_idx == 0;
                                    // 悬挂缩进语义：首行文字起点 = margin_left + indent_first
                                    // （indent_first为负时首行更靠左，即"悬挂"），非首行固定
                                    // 用margin_left。indent_first为None（如027/028大多数
                                    // 段落）时para_indent_first=0.0，首行和非首行相同，
                                    // 退化为普通左对齐，无悬挂效果——符合027/028的预期。
                                    let this_line_x_offset = if running_line_idx == 0 {
                                        (para_margin_left + para_indent_first).max(0.0)
                                    } else {
                                        para_margin_left
                                    };
                                    let mut line = LineBox {
                                        y_offset: running_y,
                                        ascent: endnote_font_size,
                                        descent: endnote_font_size * 0.2,
                                        line_height: endnote_line_height,
                                        text,
                                        runs: Vec::new(),
                                        x_offset: this_line_x_offset,
                                        text_x_offset: this_line_x_offset,
                                        ..Default::default()
                                    };
                                    if is_note_head {
                                        // ---- 以下为v141旧方案(list_label+悬挂缩进)，
                                        // 已废弃，原样注释保留供回退对照，不删除 ----
                                        // line.list_label = Some(format!("{}", number_label));
                                        // line.list_label_x = para_margin_left;
                                        // ---- 旧方案注释结束 ----
                                        //
                                        // WROOT-027/028 (v141二次修复): 编号字符（已在
                                        // raw_text拼接阶段塞进这一行文字最前面，字节区间
                                        // 0..note_label_byte_len）该渲染成什么样式，完全
                                        // 由占位符run自身通过w:rStyle引用的字符样式决定
                                        // （v140会话确认的结论）。故意不调用
                                        // resolve_run_style：该函数末尾STEP6-STAGE-A有
                                        // 一条"run.field为EndnoteRef/FootnoteRef就无条件
                                        // superscript"的历史规则，是专为"正文段落里的
                                        // 引用编号"场景设计的，对note正文区占位符run同样
                                        // 生效会导致"该不该上标"被错误地从"看rStyle"篡改
                                        // 成"无条件是"（v140文档3.3节已验证并回滚过的坑）。
                                        // 这里改为只手动解析char_style本身，不触碰该函数。
                                        //
                                        // 渲染机制：不再用专属的list_label分支画这几个
                                        // 字符，改为构造一个LineRunSpan塞进line.runs，
                                        // 复用tile.rs里script_runs循环（本来就是给正文中
                                        // 脚注引用编号的上标效果设计的通用机制，见
                                        // roe-writer/src/tile.rs 约1558行），该循环会自动
                                        // 挑出script!=Normal的run单独用上标y偏移绘制。
                                        if note_label_byte_len > 0 {
                                            let mut label_color: Option<roe_model::common::Color> = None;
                                            let mut label_font_size = endnote_font_size;
                                            let mut label_script = ScriptPosition::Normal;
                                            if let Some(BodyElement::Paragraph(fp0)) = note.content.first() {
                                                if let Some(fr0) = fp0.runs.first() {
                                                    if matches!(fr0.script, Some(ScriptPosition::Superscript)) { label_script = ScriptPosition::Superscript; }
                                                }
                                            }
                                            if let Some(first_para_elem) = note.content.first() {
                                                if let BodyElement::Paragraph(first_para) = first_para_elem {
                                                    if let Some(first_run) = first_para.runs.first() {
                                                        if let Some(style_ref) = &first_run.char_style {
                                                            let cs = doc.styles.resolve_char_style(style_ref.0.as_str());
                                                            label_color = cs.color;
                                                            if let Some(sz) = cs.font_size {
                                                                label_font_size = sz;
                                                            }
                                                            if matches!(cs.script, Some(ScriptPosition::Superscript)) {
                                                                label_script = ScriptPosition::Superscript;
                                                            } else if matches!(cs.script, Some(ScriptPosition::Subscript)) {
                                                                label_script = ScriptPosition::Subscript;
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            line.runs.push(crate::frame::LineRunSpan {
                                                start: 0,
                                                end: note_label_byte_len,
                                                style: ResolvedRunStyle {
                                                    font_size: label_font_size,
                                                    font_family_is_set: false,
                                                    bold: false,
                                                    italic: false,
                                                    underline: false,
                                                    strikethrough: false,
                                                    color: label_color,
                                                    background: None,
                                                    script: label_script,
                                                    is_formula: false,
                                                    comment_highlight_color_idx: None,
                                                    comment_bar_start: false,
                                                    comment_bar_end: false,
                                                    comment_id: None,
                                                    vertical_position: None,
                                                },
                                                run_idx: None,
                                                formula_box: None,
                                            });
                                            // WROOT-113：编号字号显著大于正文（>1.5倍，如
                                            // EndnoteCharacters sz=72）时撑高首行，避免大号编号
                                            // 与上下条尾注重叠。正常编号（上标/同字号）不触发。
                                            if label_font_size > endnote_font_size * 1.5 {
                                                line.ascent = label_font_size;
                                                line.descent = label_font_size * 0.2;
                                                line.line_height = label_font_size * 1.3;
                                            }
                                        }
                                    }
                                    // WROOT-112：note 正文（编号之后的文字）此前在 line.runs 里没有
                                    // 任何 span，只有编号占位符一个 span。tile.rs 取 runs.first() 的
                                    // 字号作为整行 push_default 字号，编号 span 若被字符样式
                                    // （如 EndnoteCharacters sz=72）放大到 36pt，没有自己 span 的
                                    // 正文就整体吃到 36pt（tdf109310_endnoteStyleForMSO）。
                                    // 这里给正文补一个 Normal span（endnote_font_size），使其走
                                    // tile.rs 普通分支 push(FontSize)。首行正文从编号之后开始，
                                    // 非首行从 0 开始。
                                    {
                                        let body_start = if is_note_head { note_label_byte_len.min(line.text.len()) } else { 0 };
                                        let body_end = line.text.len();
                                        if body_start < body_end {
                                            line.runs.push(crate::frame::LineRunSpan {
                                                start: body_start,
                                                end: body_end,
                                                style: ResolvedRunStyle {
                                                    font_size: endnote_font_size,
                                                    font_family_is_set: false,
                                                    bold: false,
                                                    italic: false,
                                                    underline: false,
                                                    strikethrough: false,
                                                    color: None,
                                                    background: None,
                                                    script: ScriptPosition::Normal,
                                                    is_formula: false,
                                                    comment_highlight_color_idx: None,
                                                    comment_bar_start: false,
                                                    comment_bar_end: false,
                                                    comment_id: None,
                                                    vertical_position: None,
                                                },
                                                run_idx: None,
                                                formula_box: None,
                                            });
                                        }
                                    }
                                    running_y += line.line_height;
                                    lines.push(line);
                                    running_line_idx += 1;
                                }
                            }
                            if lines.is_empty() {
                                lines.push(LineBox {
                                    y_offset: 0.0,
                                    ascent: endnote_font_size,
                                    descent: endnote_font_size * 0.2,
                                    line_height: endnote_line_height,
                                    text: String::new(),
                                    runs: Vec::new(),
                                    x_offset: para_margin_left,
                                    text_x_offset: para_margin_left,
                                    ..Default::default()
                                });
                            }
                            is_first_para = false;
                            // WROOT-027 (v141四次修复): note内容跨页时，续页
                            // 开头也应该有一条分隔线（用户核实：不需要区分
                            // separator/continuationSeparator两种类型分别给
                            // 不同处理，跟本页开头那条一样统一用固定短线即可，
                            // 简化了v141文档3.3节原计划的双规格方案）。
                            // 实现方式：place_paragraph内部会在放不下时通过
                            // *cur_page = new_page 修改这个可变引用（已确认，
                            // 见2577/2624/2663/2691/2745行），调用前后比较
                            // cur_page是否变化即可判断"刚发生了分页"。PageId
                            // 由slotmap::new_key_type!宏生成，天然实现Copy+
                            // PartialEq，可以直接复制保存、直接==比较。
                            // [WROOT-027-CONT] 修复：不再靠"调用前后比较cur_page"
                            // 事后判断（此前实测027场景下第2页完全没插上线、
                            // 第3页线跑到内容下方，见交接文档v142第四节根因分析——
                            // 一次place_paragraph调用内部可能连续翻好几页，外部
                            // 只看首尾两个时间点会漏掉中间页；且事后再算y坐标时
                            // used_height已经把这段文字自己的高度算进去了，
                            // 位置自然不对）。现在改为把线规格直接传给
                            // place_paragraph，让它在内部每一个真正翻页的时刻
                            // （4处*cur_page=new_page赋值点）各自立即插一条，
                            // 新页este刚创建、used_height必然是0，不存在这个问题。
                            self.place_paragraph(
                                &mut arena, &mut cur_page, &mut cur_column,
                                doc.body.len(), lines, 0.0, 4.0, 0.0,
                                content_h, cur_section_index,
                                &mut cur_page_in_section, &mut section_just_changed,
                                Some((rule_width_pt, rule_thickness_pt, rule_color, rule_margin_before)),
                                None,
                            );
                        }
                    }
                }
            }
        }

        // [WROOT-footnote-allpages] footnote body rendering, extended to all pages.
        // 在v143交接文档"仅第一页"的基础上扩展：外层遍历 arena.page_order
        // 里的每一页，对每一页各自过滤出属于它的 footnote（page_footnote_ids
        // 按页分组的HashMap，在body主循环里、每个段落 place_paragraph 调用
        // 之后记录），分别画分隔线+内容。内层逻辑与之前完全一致（几乎照抄
        // endnote渲染块），只是最外层多了一层按页循环，且过滤条件从
        // "id属于first_page_footnote_ids"换成"id属于page_footnote_ids[page_id]"。
        // 注意：arena.page_order 在这个循环执行期间会被 place_paragraph
        // 内部可能触发的新增页操作追加新条目（若某页footnote内容本身放不下
        // 溢出到临时页），故这里先clone一份固定的页列表再遍历，避免遍历时
        // 修改集合导致的动态行为（不追求处理这类footnote内容跨页的边缘情况，
        // 但至少不能让遍历本身在运行时行为不可预测）。
        // 分隔线规格与endnote一致，统一用WPS风格固定短线（144pt/1pt/黑色/
        // 12pt margin_before/6pt space_after），不做长短或页面占比计算。
        // 已知限制（本次不做，见与用户讨论记录）：footnote内容目前接在该页
        // 正文流的used_height之后，不是贴在页面底部（Word/LOK标准做法是
        // footnote区域固定贴底、正文可用高度动态让出空间，LOK对应实现是
        // SwFootnoteBossFrame::m_nMaxFootnoteHeight一套动态回退机制，需要
        // 重新设计content_h的计算时机，属于架构级改动，留作独立技术债）。
        if !dry_run {
            let footnote_font_size = self.registry.default_font_size;
            let footnote_line_height = footnote_font_size * 1.3;
            let newline_char = char::from_u32(10).unwrap();
            let tab_char = char::from_u32(9).unwrap();

            let all_page_ids: Vec<roe_core::arena::PageId> = arena.page_order.clone();
            for page_id in all_page_ids {
                let empty_set: std::collections::HashSet<u32> = std::collections::HashSet::new();
                let ids_for_this_page = page_footnote_ids.get(&page_id).unwrap_or(&empty_set);
                let real_footnotes: Vec<&roe_model::writer::Note> = doc.footnotes.iter()
                    .filter(|n| n.note_type == roe_model::writer::NoteType::Normal)
                    .filter(|n| ids_for_this_page.contains(&n.id))
                    .collect();
                if real_footnotes.is_empty() {
                    continue;
                }

                let rule_width_pt = 144.0_f32;
                let rule_thickness_pt = 1.0_f32;
                let rule_color = roe_model::common::Color { r: 0, g: 0, b: 0, a: 255 };
                let rule_margin_before = 12.0_f32;

                // [WROOT-footnote-pindown] 方案D收尾：footnote区域贴底定位。
                // 先一次性算出这一页footnote区域的总高度（分隔线固定开销
                // 31.0pt = 2*margin_before(12) + thickness(1) + space_after(6)，
                // 与Patch 3c算预算时用的固定值来源一致，见交接文档2.2节第6点），
                // 再算出这个区域应该贴着页面底部时、分隔线顶部应该落在的
                // 全局y坐标。footnote_cursor_y在下面note循环里逐条累加下推，
                // 每条note用当前used_height对应的"自然位置"与
                // footnote_cursor_y的差值作为float_offset_y传给
                // place_paragraph，让其脱离正文堆叠流、直接定位到贴底位置
                // （复用WROOT-037-B已有的float_offset_y+is_floating机制，
                // 见该函数参数注释）。
                let rule_thickness_and_margins = 2.0 * rule_margin_before + rule_thickness_pt + 6.0;
                let mut footnote_block_total_height = rule_thickness_and_margins;
                for note in &real_footnotes {
                    let number = footnote_numbers.get(&note.id).copied().unwrap_or(0);
                    let number_label = if number == 0 {
                        "?".to_string()
                    } else {
                        let fmt_str = match doc.footnote_num_fmt {
                            roe_model::writer::NumFormat::LowerRoman => "lowerRoman",
                            roe_model::writer::NumFormat::UpperRoman => "upperRoman",
                            roe_model::writer::NumFormat::LowerAlpha => "lowerLetter",
                            roe_model::writer::NumFormat::UpperAlpha => "upperLetter",
                            roe_model::writer::NumFormat::None => "none",
                            roe_model::writer::NumFormat::Arabic => "decimal",
                        };
                        roe_parser::ooxml::docx::format_num(number, fmt_str)
                    };
                    footnote_block_total_height += self.estimate_footnote_note_height(
                        note, &number_label, footnote_font_size, footnote_line_height, content_w, doc,
                    );
                }
                let target_block_top_y = arena.page_content_global_y(page_id) + content_h
                    - footnote_block_total_height;
                let rule_y = target_block_top_y + rule_margin_before;
                let rule_fid = arena.insert_horizontal_rule_frame(
                    page_id,
                    doc.body.len() + 1,
                    rule_width_pt,
                    rule_thickness_pt,
                    rule_color,
                    rule_margin_before,
                    rule_y,
                    rule_margin_before,
                    6.0,
                );
                let _ = rule_fid;

                let mut footnote_page = page_id;
                let mut footnote_column = 0usize;
                let mut footnote_page_in_section = *arena.page_index_in_section.get(&page_id).unwrap_or(&1);
                let mut footnote_section_just_changed = false;
                // 贴底游标：从分隔线块结束的位置开始，逐条note往下推。
                let mut footnote_cursor_y = target_block_top_y + rule_thickness_and_margins;

                for note in real_footnotes {
                    let number = footnote_numbers.get(&note.id).copied().unwrap_or(0);
                    let number_label = if number == 0 {
                        "?".to_string()
                    } else {
                        let fmt_str = match doc.footnote_num_fmt {
                            roe_model::writer::NumFormat::LowerRoman => "lowerRoman",
                            roe_model::writer::NumFormat::UpperRoman => "upperRoman",
                            roe_model::writer::NumFormat::LowerAlpha => "lowerLetter",
                            roe_model::writer::NumFormat::UpperAlpha => "upperLetter",
                            roe_model::writer::NumFormat::None => "none",
                            roe_model::writer::NumFormat::Arabic => "decimal",
                        };
                        roe_parser::ooxml::docx::format_num(number, fmt_str)
                    };
                    let note_natural_y_before = arena.page_content_global_y(footnote_page)
                        + arena.used_height(footnote_page);
                    let note_estimated_height = self.estimate_footnote_note_height(
                        note, &number_label, footnote_font_size, footnote_line_height, content_w, doc,
                    );
                    let mut is_first_para = true;
                    for body_elem in &note.content {
                        if let BodyElement::Paragraph(p) = body_elem {
                            let para_margin_left = p.indent_left.unwrap_or(0.0).max(0.0);
                            let para_indent_first = p.indent_first.unwrap_or(0.0);
                            let hanging_indent_pt = para_margin_left;
                            let footnote_effective_w = (content_w - hanging_indent_pt).max(content_w * 0.3);
                            let mut raw_text = String::new();
                            let mut note_label_byte_len: usize = 0;
                            if is_first_para {
                                raw_text.push_str(&number_label);
                                note_label_byte_len = number_label.len();
                            }
                            for r in &p.runs {
                                raw_text.push_str(&r.text);
                            }
                            let raw_text = raw_text.replace(tab_char, " ");
                            let hard_segments: Vec<&str> = raw_text.split(newline_char).collect();
                            let hard_segments: Vec<&str> = if hard_segments.is_empty() {
                                vec![""]
                            } else {
                                hard_segments
                            };
                            let mut lines: Vec<LineBox> = Vec::new();
                            let mut running_line_idx: usize = 0;
                            for segment in hard_segments.iter() {
                                let seg_text = (*segment).to_string();
                                let default_brush = RunBrush { color: [0, 0, 0, 255], run_idx: None };
                                let mut seg_builder = self.registry.layout_context.ranged_builder(
                                    &mut self.registry.font_context, &seg_text, 1.0, true,
                                );
                                seg_builder.push_default(StyleProperty::Brush(default_brush));
                                seg_builder.push_default(GenericFamily::SystemUi);
                                seg_builder.push_default(LineHeight::Absolute(footnote_line_height));
                                seg_builder.push_default(StyleProperty::FontSize(footnote_font_size));
                                let mut seg_layout: parley::Layout<RunBrush> = seg_builder.build(&seg_text);
                                {
                                    let mut seg_breaker = seg_layout.break_lines();
                                    let seg_state = seg_breaker.state_mut();
                                    seg_state.set_layout_max_advance(footnote_effective_w);
                                    seg_state.set_line_max_advance(footnote_effective_w);
                                    loop {
                                        let Some(yield_data) = seg_breaker.break_next() else { break; };
                                        if let YieldData::LineBreak(_) = yield_data {
                                            let seg_state = seg_breaker.state_mut();
                                            seg_state.set_line_max_advance(footnote_effective_w);
                                        }
                                    }
                                    seg_breaker.finish();
                                }
                                seg_layout.align(Alignment::Start, AlignmentOptions::default());
                                for seg_line in seg_layout.lines() {
                                    let seg_line: parley::Line<'_, RunBrush> = seg_line;
                                    let range = seg_line.text_range();
                                    let text = seg_text.get(range).unwrap_or("").to_string();
                                    let is_note_head = is_first_para && running_line_idx == 0;
                                    let this_line_x_offset = if running_line_idx == 0 {
                                        (para_margin_left + para_indent_first).max(0.0)
                                    } else {
                                        para_margin_left
                                    };
                                    let mut line = LineBox {
                                        y_offset: running_line_idx as f32 * footnote_line_height,
                                        ascent: footnote_font_size,
                                        descent: footnote_font_size * 0.2,
                                        line_height: footnote_line_height,
                                        text,
                                        runs: Vec::new(),
                                        x_offset: this_line_x_offset,
                                        text_x_offset: this_line_x_offset,
                                        ..Default::default()
                                    };
                                    if is_note_head {
                                        if note_label_byte_len > 0 {
                                            let mut label_color: Option<roe_model::common::Color> = None;
                                            let mut label_font_size = footnote_font_size;
                                            let mut label_script = ScriptPosition::Normal;
                                            if let Some(BodyElement::Paragraph(fp0)) = note.content.first() {
                                                if let Some(fr0) = fp0.runs.first() {
                                                    if matches!(fr0.script, Some(ScriptPosition::Superscript)) { label_script = ScriptPosition::Superscript; }
                                                }
                                            }
                                            if let Some(first_para_elem) = note.content.first() {
                                                if let BodyElement::Paragraph(first_para) = first_para_elem {
                                                    if let Some(first_run) = first_para.runs.first() {
                                                        if let Some(style_ref) = &first_run.char_style {
                                                            let cs = doc.styles.resolve_char_style(style_ref.0.as_str());
                                                            label_color = cs.color;
                                                            if let Some(sz) = cs.font_size {
                                                                label_font_size = sz;
                                                            }
                                                            if matches!(cs.script, Some(ScriptPosition::Superscript)) {
                                                                label_script = ScriptPosition::Superscript;
                                                            } else if matches!(cs.script, Some(ScriptPosition::Subscript)) {
                                                                label_script = ScriptPosition::Subscript;
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            line.runs.push(crate::frame::LineRunSpan {
                                                start: 0,
                                                end: note_label_byte_len,
                                                style: ResolvedRunStyle {
                                                    font_size: label_font_size,
                                                    font_family_is_set: false,
                                                    bold: false,
                                                    italic: false,
                                                    underline: false,
                                                    strikethrough: false,
                                                    color: label_color,
                                                    background: None,
                                                    script: label_script,
                                                    is_formula: false,
                                                    comment_highlight_color_idx: None,
                                                    comment_bar_start: false,
                                                    comment_bar_end: false,
                                                    comment_id: None,
                                                    vertical_position: None,
                                                },
                                                run_idx: None,
                                                formula_box: None,
                                            });
                                        }
                                    }
                                    lines.push(line);
                                    running_line_idx += 1;
                                }
                            }
                            if lines.is_empty() {
                                lines.push(LineBox {
                                    y_offset: 0.0,
                                    ascent: footnote_font_size,
                                    descent: footnote_font_size * 0.2,
                                    line_height: footnote_line_height,
                                    text: String::new(),
                                    runs: Vec::new(),
                                    x_offset: para_margin_left,
                                    text_x_offset: para_margin_left,
                                    ..Default::default()
                                });
                            }
                            // [WROOT-footnote-pindown] 这条note的第一个段落
                            // 需要真正的贴底偏移量；同一条note内后续段落沿用
                            // 自然堆叠（相对第一段的位置本来就是对的，不需要
                            // 重复加偏移，否则会被错误地二次下移）。
                            let this_para_float_offset_y = if is_first_para {
                                (footnote_cursor_y - note_natural_y_before).max(0.0)
                            } else {
                                0.0
                            };
                            is_first_para = false;
                            self.place_paragraph(
                                &mut arena, &mut footnote_page, &mut footnote_column,
                                doc.body.len() + 1, lines, 0.0, 4.0, this_para_float_offset_y,
                                content_h, cur_section_index,
                                &mut footnote_page_in_section, &mut footnote_section_just_changed,
                                Some((rule_width_pt, rule_thickness_pt, rule_color, rule_margin_before)),
                                None,
                            );
                        }
                    }
                    // 这条note处理完毕，游标下推它的估算高度，供下一条note使用。
                    footnote_cursor_y += note_estimated_height;
                }
            }
        }
        LayoutResult { arena }
    }

    /// 把一段 lines 放到当前页，必要时分页（递归处理溢出）
    fn place_paragraph(
        &mut self,
        arena:       &mut FrameArena,
        cur_page:    &mut roe_core::arena::PageId,
        cur_column:  &mut usize,
        para_index:  usize,
        lines:       Vec<LineBox>,
        space_before: f32,
        space_after: f32,
        // WROOT-037-B：因避让浮动框（图片横向溢出版心导致某y区间完全
        // 占满）而需要让本段"视觉上"额外下移的量。不同于 space_before
        // （会计入 used_height、顺延推挤后续文档流元素），这个偏移只
        // 影响本段 frame 最终的 y_on_page 绘制位置，且伴随
        // is_floating=true 一起生效，让本段脱离纵向堆叠流统计——图片
        // 本身是"浮动"定位，不占用文档流高度，因避让图片而位移的文字
        // 段落同理也不应该把这段位移"焊死"进文档流高度里，否则会把
        // 排在该段落之后的其他内容一并顶下去（本次会话已实测复现，
        // 用 space_before 方案会导致图片被"1"倒挤到下方、中间空一大截，
        // 见交接文档记录）。绝大多数段落没有此问题，传0.0，行为不变。
        float_offset_y: f32,
        content_h:   f32,
        // [WROOT-任务B] 调用这次 place_paragraph 时排版扫描到的 section 序号
        // （0-based）。一次 place_paragraph 调用不会跨越 Section 边界（边界
        // 在 layout_impl 主循环里单独处理，不会把 BodyElement::Section 传进
        // 这里），所以这里是个不变的值，只用于函数内部换页时补写
        // arena.page_section_index。
        cur_section_index: usize,
        // [WROOT-hf-parity] 同一批状态里唯二会在函数内部"新建页"的地方
        // （MoveToNextPage / ShiftUp 两个分支），必须能同步更新调用方的
        // cur_page_in_section / section_just_changed，否则这两个分支建的
        // 页会漏记节内页码。用 &mut 传引用，函数退出后调用方状态保持一致，
        // 语义上和 cur_page/cur_column 用 &mut 传递是同一个模式。
        cur_page_in_section: &mut usize,
        section_just_changed: &mut bool,
        // [WROOT-027-CONT] 若 Some，说明这次 place_paragraph 调用属于
        // note(endnote/footnote) 内容渲染，一旦函数内部真的发生翻页
        // （4个 *cur_page = new_page 赋值点之一），要在新页顶部插入
        // 一条与首页同规格的续接分隔线。参数是
        // (width_pt, thickness_pt, color, margin_before)。传 None
        // 表示普通正文段落调用，不需要这个行为，行为与之前完全一致。
        continuation_rule: Option<(f32, f32, roe_model::common::Color, f32)>,
        // [WROOT-footnote-budget Patch3] 方案D：footnote贴底预算扣减。
        // Some((doc, page_footnote_ids, para_runs)) 表示这次调用是"可能
        // 引入新footnote引用的正文段落"，需要在 check() 判断能否放下
        // 之前，先为这批 lines 里新出现的 footnote_ref 预留渲染高度。
        // None 表示不需要这个逻辑（endnote续接分隔线调用、footnote内容
        // 自身的渲染调用都传 None——它们不会引用别的footnote，是被预算
        // 保护的对象，不是产生预算需求的源头，见交接文档设计讨论）。
        //
        // 试算隔离（Gemini建议1）：这里只读 page_footnote_ids 判断"这个
        // footnote是不是这页已经算过"，绝不在本函数内部写入它——写入
        // 仍然由调用方在 place_paragraph 返回之后另行处理（现状不变）。
        footnote_budget_ctx: Option<(
            &roe_model::writer::WriterDocument,
            &std::collections::HashMap<roe_core::arena::PageId, std::collections::HashSet<u32>>,
            &[roe_model::writer::Run],
        )>,
    ) {
        let used_height_before = arena.used_height_in_column(*cur_page, *cur_column);
        let remaining_raw = content_h - used_height_before;

        // [WROOT-footnote-budget Patch3c] 方案D核心：在check()判断能否
        // 放下之前，先为这批lines里"新出现的"footnote引用预留渲染高度。
        //
        // 范围收窄（本次会话设计讨论明确）：只处理这里（首次check()调用），
        // 不处理函数下方循环内的二次check()调用（loop_decision那处）——
        // 那是"已经split到新页、还要不要继续切"的边缘路径，新页
        // page_footnote_ids必然为空，场景复杂度更高，本次不纳入范围，
        // 维持"不预留"的原有行为。
        let footnote_budget: f32 = if let Some((doc, page_footnote_ids, para_runs)) = footnote_budget_ctx {
            // 反查这批lines里出现的所有footnote_ref（去重）：用LineRunSpan.run_idx
            // 反查para_runs[idx].footnote_ref。run_idx为None的span（如纯样式
            // 覆盖、公式等）直接跳过，不是错误。
            let mut new_footnote_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
            for line in &lines {
                for span in &line.runs {
                    if let Some(idx) = span.run_idx {
                        if let Some(run) = para_runs.get(idx) {
                            if let Some(fid) = run.footnote_ref {
                                new_footnote_ids.insert(fid);
                            }
                        } else {
                        }
                    }
                }
            }

            // 试算隔离（Gemini建议1）：这里只读，不写page_footnote_ids。
            // 过滤掉这一页已经记录过的footnote id，避免重复扣预算
            // （理论上不该出现同一footnote被多次引用触发多次扣减，
            // 这里是防御性去重）。
            let already_on_page = page_footnote_ids.get(cur_page);
            let truly_new_ids: Vec<u32> = new_footnote_ids.into_iter()
                .filter(|fid| already_on_page.map_or(true, |set| !set.contains(fid)))
                .collect();

            if truly_new_ids.is_empty() {
                0.0
            } else {
                let footnote_font_size = self.registry.default_font_size;
                let footnote_line_height = footnote_font_size * 1.3;
                let content_w = self.page_width - self.margins.left - self.margins.right;

                // 首个脚注开销（Gemini建议2）：这一页目前一条footnote都
                // 还没有时，还要为分隔线预留固定高度。分隔线规格与
                // layout_impl其余两处硬编码值保持一致（144pt/1pt/12pt），
                // 真实高度公式取自split.rs同款计算：
                // 2*margin_before + thickness_pt + space_after(6.0)
                // = 2*12.0 + 1.0 + 6.0 = 31.0
                let page_has_no_footnote_yet = already_on_page.map_or(true, |set| set.is_empty());
                let rule_extra_h: f32 = if page_has_no_footnote_yet { 31.0 } else { 0.0 };

                let mut total_h = rule_extra_h;
                for fid in &truly_new_ids {
                    if let Some(note) = doc.footnotes.iter().find(|n| n.id == *fid) {
                        // 编号标签文字：预算阶段不需要精确的格式化数字
                        // （lowerRoman/decimal等），只需要长度大致相近即可
                        // 影响断行结果——直接用"?"占位，与layout_impl里
                        // number==0时的兜底显示一致，误差可忽略。
                        total_h += self.estimate_footnote_note_height(
                            note, "?", footnote_font_size, footnote_line_height, content_w, doc,
                        );
                    }
                }
                total_h
            }
        } else {
            0.0
        };

        if footnote_budget_ctx.is_some() {
        }
        let remaining = (remaining_raw - footnote_budget).max(0.0);

        // 先在当前栏插入 frame（lines 可能随后被 split）
        let fid = arena.insert_frame(*cur_page, para_index, lines.clone(), 0.0, space_before, space_after, *cur_column);

        // [WROOT-047] is_empty_container：该页/栏在插入这段内容之前是否为空。
        // 见 page_fit.rs::check() 函数级注释——容器为空时换页无法腾出更多
        // 空间，一行都放不下也应就地放置、允许视觉溢出，而不是无谓换页。
        let is_empty_container = used_height_before <= 0.5; // 0.5pt 浮点容差，与check()内部一致
        let decision = self.fit_checker.check(&lines, remaining, space_after, is_empty_container);
        let total    = lines.len();
        let resolve  = self.widow_resolver.resolve(decision, total);
        match resolve {
            ResolveResult::Keep(break_at) if break_at < total => {
                // WROOT-040-PAGEFIT-KEEP修复（本次会话核心发现）：Keep(usize)
                // 携带的数字此前被两处消费者（这里和split.rs::split_frame）
                // 一律用 `_` 忽略，误当成"全部放得下"处理。但
                // WidowOrphanResolver::resolve 在孤寡行检测都不触发时，是把
                // SplitDecision::BreakBefore{break_at} 原样转成 Keep(break_at)
                // 返回的（widow_orphan.rs 第59行，注释"不需要调整，使用原
                // break_at"——意思是不需要再ShiftUp调整断点位置，不是"不用
                // 分页"）。break_at < total 说明这就是这种情况：仍然需要在
                // break_at 处真正分页，只是不需要孤寡行调整。此前完全没有
                // 调用 split_frame，导致该分页的段落整段硬塞在当前页，
                // 造成视觉溢出（WROOT-040样本25行正文只有前7行放得下，
                // decision算出BreakBefore{7}，却因为这个bug从未真正分页）。
                let result = split_frame(
                    arena, fid,
                    ResolveResult::ShiftUp(break_at),
                    self.page_width, self.page_height, self.margins,
                    continuation_rule,
                );
                let used_before_this_frame = arena.used_height_in_column(*cur_page, *cur_column)
                    - arena.frames[fid].space_before
                    - arena.frames[fid].height();
                arena.frames[fid].y_on_page =
                    arena.page_content_global_y(*cur_page) + used_before_this_frame;
                if let Some((new_page, new_fid)) = result {
                    *cur_page = new_page;
                    *cur_page_in_section = if *section_just_changed { *section_just_changed = false; 1 } else { *cur_page_in_section + 1 };
                    arena.page_section_index.insert(new_page, cur_section_index);
                    arena.page_index_in_section.insert(new_page, *cur_page_in_section);
                    // WROOT-040-MULTIPAGE修复（本次会话）：此前 split_frame
                    // 只做"一次性切割"，把溢出部分整体扔进新页，从不检查
                    // 新页是否也装不下——旧假设是"一段最多跨2页"，但
                    // WROOT-040样本25行正文切一刀后剩18行，18行本身仍然
                    // 超出一页可用高度，需要继续分页。这里改造成循环：
                    // 每次切出新的 follow frame 后，重新对它跑一遍
                    // check→resolve，如果还放不下就继续 split_frame，
                    // 直到某一轮判定"全部放得下"为止。content_h 复用
                    // 同一个值是安全的——一次 place_paragraph 调用不会
                    // 跨 section 边界（函数头注释已注明），新页与当前页
                    // 属于同一 section，页面尺寸不变。
                    let mut loop_fid = new_fid;
                    loop {
                        let loop_lines: Vec<LineBox> = match &arena.frames[loop_fid].content {
                            FrameContent::Paragraph(ls) => ls.clone(),
                            FrameContent::TableRow(_) => break,
                            FrameContent::HorizontalRule { .. } => break,
                        };
                        let loop_total = loop_lines.len();
                        let loop_used_before = arena.used_height_in_column(*cur_page, *cur_column)
                            - arena.frames[loop_fid].height();
                        let loop_remaining = content_h - loop_used_before;
                        let loop_is_empty = loop_used_before <= 0.5;
                        let loop_decision = self.fit_checker.check(&loop_lines, loop_remaining, space_after, loop_is_empty);
                        let loop_resolve = self.widow_resolver.resolve(loop_decision, loop_total);
                        let loop_break_at = match loop_resolve {
                            ResolveResult::Keep(b) if b < loop_total => b,
                            ResolveResult::ShiftUp(b) => b,
                            ResolveResult::MoveToNextPage => 0,
                            _ => break, // 全部放得下，循环结束
                        };
                        let loop_result = split_frame(
                            arena, loop_fid,
                            ResolveResult::ShiftUp(loop_break_at),
                            self.page_width, self.page_height, self.margins,
                            continuation_rule,
                        );
                        let loop_used_before_this = arena.used_height_in_column(*cur_page, *cur_column)
                            - arena.frames[loop_fid].space_before
                            - arena.frames[loop_fid].height();
                        arena.frames[loop_fid].y_on_page =
                            arena.page_content_global_y(*cur_page) + loop_used_before_this;
                        match loop_result {
                            Some((loop_new_page, loop_new_fid)) => {
                                *cur_page = loop_new_page;
                                *cur_page_in_section = if *section_just_changed { *section_just_changed = false; 1 } else { *cur_page_in_section + 1 };
                                arena.page_section_index.insert(loop_new_page, cur_section_index);
                                arena.page_index_in_section.insert(loop_new_page, *cur_page_in_section);
                                loop_fid = loop_new_fid;
                            }
                            None => break, // split_frame 返回 None（没有需要移的行），结束
                        }
                    }
                }
            }
            ResolveResult::Keep(_) => {
                // 真正全部放得下（来自 SplitDecision::Fits），更新 y_on_page（全局坐标）
                let used = arena.used_height_in_column(*cur_page, *cur_column) - arena.frames[fid].space_before - arena.frames[fid].height() - space_after;
                arena.frames[fid].y_on_page = arena.page_content_global_y(*cur_page) + used;
                // WROOT-037-B：本段因避让浮动框而需要额外视觉下移。
                // 必须放在上面 y_on_page 正常赋值之后，用"额外加offset"
                // 的方式覆盖，而不是从一开始就把offset混进used_height的
                // 计算链路——is_floating=true 之后，本段不再计入
                // used_height_in_column的统计（frame.rs used_height_in_column
                // 对is_floating的frame直接跳过累加，WROOT-101同款机制），
                // 从而不会顶下排在它之后的图片/其他段落。
                if float_offset_y > 0.0 {
                    arena.frames[fid].y_on_page += float_offset_y;
                    arena.frames[fid].is_floating = true;
                }
            }
            ResolveResult::MoveToNextPage => {
                let num_cols = arena.pages[*cur_page].columns.num_cols;
                if *cur_column + 1 < num_cols {
                    // 还有下一栏可换：不新建页，只把这个 frame 挪到下一栏顶部
                    // （分栏排版设计文档 v1 四、4.1节 MoveToNextColumn 分支）。
                    *cur_column += 1;
                    arena.frames[fid].column_index = *cur_column;
                    arena.frames[fid].y_on_page = arena.page_content_global_y(*cur_page);
                } else {
                    // 已是最后一栏（或单栏文档 num_cols=1）：走原有换页逻辑，栏序号重置为0
                    let (new_page, _new_fid) =
                        move_frame_to_new_page(arena, fid, self.page_width, self.page_height, self.margins, continuation_rule);
                    *cur_page = new_page;
                    *cur_page_in_section = if *section_just_changed { *section_just_changed = false; 1 } else { *cur_page_in_section + 1 };
                    arena.page_section_index.insert(new_page, cur_section_index);
                    arena.page_index_in_section.insert(new_page, *cur_page_in_section);
                    *cur_column = 0;
                }
            }
            ResolveResult::ShiftUp(break_at) => {
                let result = split_frame(
                    arena, fid,
                    ResolveResult::ShiftUp(break_at),
                    self.page_width, self.page_height, self.margins,
                    continuation_rule,
                );
                // BUG修复（本次会话核心发现）：split_frame 只把"尾部行"移到新页
                // 并为新 frame 正确赋值 y_on_page，却完全没有更新"留在本页"的
                // 原 fid 的 y_on_page —— 该字段自 insert_frame 起一直是硬编码的
                // 占位值 0.0（只有 Keep 分支才会更新它），导致 ShiftUp 场景下
                // 留在本页的头部若干行始终以 y_on_page=0.0 参与渲染，被画到
                // 页面最顶部（在 margin/抬头信息之前）。必须放在 split_frame
                // 调用之后：此时 arena.frames[fid] 的 lines 已被截断为头部
                // 剩余行，height() 才反映截断后的真实高度，用与 Keep 分支
                // 完全相同的公式补算。
                let used_before_this_frame = arena.used_height_in_column(*cur_page, *cur_column)
                    - arena.frames[fid].space_before
                    - arena.frames[fid].height();
                arena.frames[fid].y_on_page =
                    arena.page_content_global_y(*cur_page) + used_before_this_frame;
                if let Some((new_page, new_fid)) = result {
                    *cur_page = new_page;
                    *cur_page_in_section = if *section_just_changed { *section_just_changed = false; 1 } else { *cur_page_in_section + 1 };
                    arena.page_section_index.insert(new_page, cur_section_index);
                    arena.page_index_in_section.insert(new_page, *cur_page_in_section);
                } else {
                }
            }
        }
    }

    /// 把一行表格 cell 放到当前页，必要时整行移到下一页。
    /// WROOT-030 结构性重构新增：与 place_paragraph 平行，但分页判断
    /// 简化为 Keep / MoveToNextPage 两态——不做行内断裂（某些 cell 在这页
    /// 写完、某些续到下一页），这是刻意简化的初版范围，记入已知限制。
    fn place_table_row(
        &mut self,
        arena:       &mut FrameArena,
        cur_page:    &mut roe_core::arena::PageId,
        cur_column:  &mut usize,
        para_index:  usize,
        cells:       Vec<TableCellFrame>,
        content_h:   f32,
        is_floating: bool,
        row_y_override: Option<f32>,
    ) -> f32 {
        // 表格行不参与分栏（已知限制，见设计文档4.3节），插入时固定 column_index=0，
        // 但换页时仍需把外层 cur_column 同步重置，避免表格后续段落带着旧栏号却已在新页。
        let remaining = content_h - arena.used_height(*cur_page);
        let row_height = cells.iter()
            .map(|c| c.lines.iter().map(|l| l.y_offset + l.line_height).fold(0.0f32, f32::max))
            .fold(0.0, f32::max);

        if row_height <= remaining + 0.5 || arena.pages[*cur_page].frames.is_empty() {
            // 放得下；或者当前页还是空页（避免死循环式地不断新开空页）
            let fid = arena.insert_table_row_frame(*cur_page, para_index, cells, 0.0, 0.0, 0.0, is_floating);
            // WROOT-101修复：浮动表格行（row_y_override为Some）用调用方传入的
            // 累加器直接定位，不走 used_height()——is_floating=true 的 frame
            // 被 used_height() 跳过不计入累加，若仍用 used_height() 推算，
            // 同一张表格内连续多行会算出几乎相同的 y（重叠）。非浮动表格
            // （row_y_override为None）行为完全不变，仍走原 used_height() 路径。
            let y = match row_y_override {
                Some(cursor) => arena.page_content_global_y(*cur_page) + cursor,
                None => {
                    let used = arena.used_height(*cur_page) - arena.frames[fid].height();
                    arena.page_content_global_y(*cur_page) + used
                }
            };
            arena.frames[fid].y_on_page = y;
        } else {
            // 整行放不下，先在当前页插入（与 place_paragraph 的写法一致：
            // 先插入再挪走），然后整体搬到下一页
            let fid = arena.insert_table_row_frame(*cur_page, para_index, cells, 0.0, 0.0, 0.0, is_floating);
            let (new_page, _new_fid) =
                move_frame_to_new_page(arena, fid, self.page_width, self.page_height, self.margins, None);
            *cur_page = new_page;
            *cur_column = 0;
        }
        row_height
    }
}

// ── 向后兼容：保留 LayoutPage / ShapedLine / LayoutParagraph 供 tile.rs 使用 ──

#[derive(Debug, Clone)]
pub struct ShapedLine {
    pub y_offset: f32,
    pub ascent:   f32,
    pub descent:  f32,
    pub line_height: f32,
    pub text:     String,
    pub runs:     Vec<crate::frame::LineRunSpan>,
    pub x_offset:      f32,
    pub cell_width:    f32,
    /// 文字实际绘制起点 x 坐标（pt），= x_offset + cell内边距。
    /// 与 x_offset 分开存放，避免边框绘制坐标被内边距污染
    /// （WROOT-030 followup）。Paragraph 路径下等于 x_offset。
    pub text_x_offset: f32,
    // WROOT-100重构：四字段类型从bool/Option<f32>改为
    // Option<crate::frame::BorderStyle>，语义同frame.rs LineBox/
    // TableCellFrame同名字段（None=不画，Some=完整颜色/宽度/虚实）。
    pub border_top:    Option<crate::frame::BorderStyle>,
    pub border_bottom: Option<crate::frame::BorderStyle>,
    pub border_left:   Option<crate::frame::BorderStyle>,
    pub border_right:  Option<crate::frame::BorderStyle>,
    pub list_label:    Option<String>,
    pub list_label_x:  f32,
    /// WROOT-068: 本行的 tab 引导线绘制信息，从 LineBox.tab_leaders 原样搬运
    pub tab_leaders:   Vec<crate::frame::TabLeaderSegment>,
    /// WROOT-068 渲染算法本体：本行的 tab 累积右移量表，从 LineBox.tab_adjustments 原样搬运
    pub tab_adjustments: Vec<crate::frame::TabAdjustment>,
    /// 边框专用的顶部/底部 y 坐标覆盖（全局页面坐标，单位 pt）。
    /// None：边框沿用这一行自己的 y_offset/line_height（Paragraph 路径，
    /// 行为与重构前完全一致）。
    /// Some(pt)：整行共享的顶/底边界（TableRow 路径，同一行所有cell
    /// 传相同值，用 frame.y_on_page / frame.y_on_page + frame.height()，
    /// 使同一行内边框对齐，不因各cell内容行数不同而错位）。
    pub border_top_pt:    Option<f32>,
    pub border_bottom_pt: Option<f32>,
    /// 表格样式移植 Layer 1步骤D：单元格背景色，从 TableCellFrame.bg_color
    /// 透传而来。Paragraph 路径（非表格）恒为 None。
    pub bg_color: Option<roe_model::common::Color>,
    /// 表格样式移植 Layer 1步骤B（渲染部分）：单元格边框颜色/宽度渲染用
    /// 最终值，从 TableCellFrame.border_color/border_width 透传而来。
    /// Paragraph 路径（非表格）恒为 None。
    pub border_color: Option<roe_model::common::Color>,
    pub border_width: Option<f32>,
    /// WROOT-027/028 (v141): 从 LineBox 同名字段原样搬运，见 frame.rs
    /// LineBox 定义处注释。
    pub list_label_color: Option<[u8;4]>,
    pub list_label_font_scale: Option<f32>,
    pub list_label_superscript: bool,
    /// v317g：单元格行来源，从 LineBox.cell_ref 搬运
    pub cell_ref: Option<(u32, u32, u32, u32)>,
}

#[derive(Debug, Clone)]
pub struct LayoutParagraph {
    pub style_name:   String,
    pub lines:        Vec<ShapedLine>,
    pub total_height: f32,
    pub space_before: f32,
    pub space_after:  f32,
}

#[derive(Debug, Clone)]
pub struct LayoutShape {
    pub geometry:     roe_model::common::ShapeGeometry,
    pub style:        roe_model::common::ShapeStyle,
    pub text_content: Vec<crate::layout::LayoutParagraph>,
    pub width_pt:     f32,
    pub height_pt:    f32,
    pub x_on_page:    f32,
    pub y_on_page:    f32,
}

#[derive(Debug, Clone)]
pub struct LayoutImage {
    pub data_base64: String,
    pub width_pt:    f32,
    pub height_pt:   f32,
    pub y_on_page:   f32,  // 在页面内容区的 y 偏移（pt）
    pub x_on_page:   f32,  // 在页面内容区的 x 偏移（pt），内联图片为 0
}

#[derive(Debug, Clone)]
pub struct LayoutChart {
    pub model:     roe_model::impress::ChartModel,
    pub width_pt:  f32,
    pub height_pt: f32,
    pub y_on_page: f32,
    pub x_on_page: f32,
}

#[derive(Debug, Clone)]
pub struct LayoutPage {
    pub width:      f32,
    pub height:     f32,
    pub paragraphs: Vec<LayoutParagraph>,
    pub images:     Vec<LayoutImage>,
    pub shapes:     Vec<LayoutShape>,  // 图形/文本框（PPTX-Shape对应物，WROOT-023等系列缺口）
    pub float_frames: Vec<LayoutImage>,  // 浮动图片（有 x/y 坐标）
    pub charts:     Vec<LayoutChart>,    // 内联图表
    pub header_text: Option<String>,     // 页眉纯文本（渲染层直接画）
    pub footer_text: Option<String>,     // 页脚纯文本（渲染层直接画）
}

/// 把 FrameArena 转换为旧版 LayoutPage 列表（供 WriterTileRenderer 过渡期使用）
// ⚠️⚠️⚠️ 废弃路径警告（v61标注，v60发现问题）⚠️⚠️⚠️
// 本函数产出的 LayoutPage 只被同样已废弃的 tile.rs::render_page_preview()
// 消费。生产渲染路径（render_one.rs / render_docx_batch.rs）走的是
// render_viewport()，直接消费 FrameArena + WriterDocument，完全不经过
// 本函数这层转换。
//
// v60开发时曾误以为这条路径是"生产主线"，在此新增了 Shape 收集逻辑
// （shapes 字段），编译通过、坐标验证正确，但因为本函数本身就是废弃路径，
// 收集到的数据没有任何渲染代码会读取。详见 tile.rs::render_page_preview()
// 函数头注释。
//
// 【下次要在此加新功能前，请先确认 render_viewport() 是否已有等价逻辑】。
pub fn arena_to_layout_pages(arena: &FrameArena, doc: &WriterDocument) -> Vec<LayoutPage> {

    arena.page_order.iter().map(|&pid| {
        let page = &arena.pages[pid];
        let paragraphs = page.frames.iter().map(|&fid| {
            let frame = &arena.frames[fid];
            let para  = doc.body.get(frame.para_index);
            let style_name = para.and_then(|e| {
                if let BodyElement::Paragraph(p) = e { Some(p.style.0.clone()) } else { None }
            }).unwrap_or_default();
            // WROOT-030 结构性重构：Paragraph 直接映射；TableRow 展开所有
            // cell 的 lines，保留各自的 x_offset/cell_width/border 信息，
            // 与重构前"整行拍平"的桥接语义等价（仅供 tile.rs 内非主渲染
            // 路径的调试/统计代码使用，真正渲染走 tile.rs 的 rendered_lines）。
            let lines: Vec<ShapedLine> = match &frame.content {
                FrameContent::Paragraph(ls) => ls.iter().map(|l| ShapedLine {
                    y_offset: l.y_offset,
                    ascent:   l.ascent,
                    descent:  l.descent,
                    line_height: l.line_height,
                    text:     l.text.clone(),
                    runs:     l.runs.clone(),
                    x_offset:      l.x_offset,
                    cell_width:    l.cell_width,
                    text_x_offset: l.text_x_offset,
                    border_top:    l.border_top,
                    border_bottom: l.border_bottom,
                    border_left:   l.border_left,
                    border_right:  l.border_right,
                    list_label:    l.list_label.clone(),
                    list_label_x:  l.list_label_x,
                    cell_ref: l.cell_ref,
                    tab_leaders:   Vec::new(),
                    tab_adjustments: Vec::new(),
                    // 这条桥接路径不参与真正渲染（真正渲染走 render_viewport），
                    // 边框y坐标覆盖字段填 None 即可。
                    border_top_pt:    None,
                    border_bottom_pt: None,
                    bg_color: None,
                    border_color: None,
                    border_width: None,
                    list_label_color: l.list_label_color,
                    list_label_font_scale: l.list_label_font_scale,
                    list_label_superscript: l.list_label_superscript,
                }).collect(),
                FrameContent::HorizontalRule { .. } => Vec::new(),
                FrameContent::TableRow(cells) => cells.iter().flat_map(|c| {
                    c.lines.iter().map(move |l| ShapedLine {
                        y_offset: l.y_offset,
                        ascent:   l.ascent,
                        descent:  l.descent,
                        line_height: l.line_height,
                        text:     l.text.clone(),
                        runs:     l.runs.clone(),
                        x_offset:      l.x_offset,
                        cell_width:    l.cell_width,
                        text_x_offset: l.text_x_offset,
                        border_top:    c.border_top,
                        border_bottom: c.border_bottom,
                        border_left:   c.border_left,
                        border_right:  c.border_right,
                        list_label:    l.list_label.clone(),
                        list_label_x:  l.list_label_x,
                        cell_ref: l.cell_ref,
                        tab_leaders:   Vec::new(),
                        tab_adjustments: Vec::new(),
                        // 这条桥接路径不参与真正渲染（真正渲染走 render_viewport），
                        // 边框y坐标覆盖字段填 None 即可。
                        border_top_pt:    None,
                        border_bottom_pt: None,
                        bg_color: c.bg_color,
                        border_color: c.border_color,
                        border_width: c.border_width,
                        list_label_color: l.list_label_color,
                        list_label_font_scale: l.list_label_font_scale,
                        list_label_superscript: l.list_label_superscript,
                    })
                }).collect(),
            };
            let total_height = frame.height();
            let space_before = para.and_then(|e| {
                if let BodyElement::Paragraph(p) = e {
                    p.space_before.or(
                        doc.styles.resolve_paragraph_style(p.style.0.as_str()).space_before
                    )
                } else { None }
            }).unwrap_or(0.0);
            LayoutParagraph { style_name, lines, total_height, space_before, space_after: frame.space_after }
        }).collect();
        // 收集本页图片：找出 para_index 指向 Image 元素的空帧
        let images = page.frames.iter().filter_map(|&fid| {
            let frame = &arena.frames[fid];
            let is_image_frame = matches!(
                &frame.content,
                FrameContent::Paragraph(ls) if ls.is_empty()
            );
            if !is_image_frame { return None; }
            let elem = doc.body.get(frame.para_index)?;
            if let BodyElement::Image(img) = elem {
                Some(LayoutImage {
                    data_base64: base64::engine::general_purpose::STANDARD.encode(&img.image.data),
                    width_pt:    img.width_pt,
                    height_pt:   img.height_pt,
                    y_on_page:   frame.y_on_page,
                    x_on_page:   0.0,
                })
            } else { None }
        }).collect::<Vec<_>>();
        // 收集图形/文本框：Shape 变体，有明确 x/y 坐标（WROOT-023系列缺口）
        let shapes = page.frames.iter().filter_map(|&fid| {
            let frame = &arena.frames[fid];
            let is_shape_frame = matches!(
                &frame.content,
                FrameContent::Paragraph(ls) if ls.is_empty()
            );
            if !is_shape_frame { return None; }
            let elem = doc.body.get(frame.para_index)?;
            if let BodyElement::Shape(shape) = elem {
                Some(LayoutShape {
                    geometry:     shape.geometry.clone(),
                    style:        shape.style.clone(),
                    text_content: vec![],  // Step3 第一版：形状内文字渲染留到后续单独处理
                    width_pt:     shape.width_pt,
                    height_pt:    shape.height_pt,
                    x_on_page:    shape.x_pt,
                    y_on_page:    frame.y_on_page,
                })
            } else { None }
        }).collect::<Vec<_>>();
        // 收集浮动图片：Frame 变体，有明确 x/y 坐标
        let float_frames = page.frames.iter().filter_map(|&fid| {
            let frame = &arena.frames[fid];
            let is_image_frame = matches!(
                &frame.content,
                FrameContent::Paragraph(ls) if ls.is_empty()
            );
            if !is_image_frame { return None; }
            let elem = doc.body.get(frame.para_index)?;
            if let BodyElement::Frame(ff) = elem {
                Some(LayoutImage {
                    data_base64: base64::engine::general_purpose::STANDARD.encode(&ff.image.data),
                    width_pt:    ff.width_pt,
                    height_pt:   ff.height_pt,
                    y_on_page:   frame.y_on_page,  // WROOT-ANCHOR-01：改用已换算好的绝对坐标
                    x_on_page:   ff.x_pt,
                })
            } else { None }
        }).collect::<Vec<_>>();
        // 收集内联图表
        let charts = page.frames.iter().filter_map(|&fid| {
            let frame = &arena.frames[fid];
            let is_image_frame = matches!(
                &frame.content,
                FrameContent::Paragraph(ls) if ls.is_empty()
            );
            if !is_image_frame { return None; }
            let elem = doc.body.get(frame.para_index)?;
            if let BodyElement::InlineChart { model, width_pt, height_pt } = elem {
                Some(LayoutChart {
                    model:     model.clone(),
                    width_pt:  *width_pt,
                    height_pt: *height_pt,
                    y_on_page: frame.y_on_page,
                    x_on_page: 0.0,
                })
            } else { None }
        }).collect::<Vec<_>>();
        // 从 doc 提取页眉/页脚纯文本（所有段落所有 run 拼接）
        let header_text = doc.header.default.as_ref().map(|paras| {
            paras.iter().flat_map(|p| p.runs.iter().map(|r| r.text.as_str())).collect::<String>()
        }).filter(|s| !s.is_empty());
        let footer_text = doc.footer.default.as_ref().map(|paras| {
            paras.iter().flat_map(|p| p.runs.iter().map(|r| r.text.as_str())).collect::<String>()
        }).filter(|s| !s.is_empty());
        LayoutPage { width: page.width, height: page.height, paragraphs, images, float_frames, charts, shapes, header_text, footer_text }
    }).collect()
}

// 让 FrameArena 在此模块可见（pub re-export）
// FrameArena re-exported via crate::frame


/// 从段落解析出排版需要的关键样式参数（字号、加粗、斜体、段后间距）。
/// 抽出此函数的目的：roe-api 的 hit_test / layout_query 在升级到"方案B：
/// 重新 shape 拿精确 glyph x 坐标"时，需要用与原始排版完全一致的样式
/// 重新跑 cosmic-text shaping，必须复用同一份解析逻辑，不能各写一份、
/// 让两边样式解析逐渐漂移不一致。
pub struct ResolvedParaStyle {
    pub font_size:   f32,
    pub bold:        bool,
    pub italic:       bool,
    pub space_before: f32,
    pub space_after:  f32,
    /// WROOT-078: 段落对齐。取值优先级：段落自身 w:jc（Paragraph.alignment）
    /// > 样式表继承链（ParagraphStyle.alignment，含 basedOn 逐层覆盖）
    /// > None（None 时 layout.rs 侧按 cosmic-text 默认 Align::Left 处理，
    /// 不显式调用 set_align，保持与改动前行为一致）。
    pub alignment: Option<roe_model::style::Alignment>,
    /// 债务A修复：行距相对字号的倍数（来自 w:line/w:lineRule=auto 语义，
    /// docx.rs 已算好 val/240.0 存入 Paragraph.line_height，此前该字段
    /// 从未被 roe-writer 读取，metrics()一直写死1.3倍）。取值优先级同
    /// space_before/space_after：段落自身 > 样式表 > 1.3兜底（原写死值，
    /// 保证未设置w:line的文档零回归）。
    pub line_height: f32,
}

/// 复用 ParagraphLayoutEngine::layout() 内部解析段落样式的逻辑。
/// default_font_size 来自 FontRegistry，调用方传入即可，不需要持有
/// 整个 ParagraphLayoutEngine。
pub fn resolve_para_style(
    doc: &WriterDocument,
    para: &roe_model::writer::Paragraph,
    default_font_size: f32,
) -> ResolvedParaStyle {
    let style = doc.styles.resolve_paragraph_style(para.style.0.as_str());
    ResolvedParaStyle {
        font_size:   style.font_size.unwrap_or(default_font_size),
        bold:        style.bold,
        italic:      style.italic,
        // 优先用段落自身的 space_after（来自 w:spacing w:after），
        // 其次用样式表的值，最后 fallback 4pt
        space_before: para.space_before
            .or(style.space_before)
            .unwrap_or(0.0),
        space_after: para.space_after
            .or(style.space_after)
            .unwrap_or(4.0),
        // 债务A修复：行距倍数，段落自身优先于样式表，1.3是修复前的写死默认值
        // （保持未设置w:line的文档零回归）。
        line_height: match (para.line_abs_pt, para.line_exact) {
            (Some(a), Some(_)) => a / style.font_size.unwrap_or(default_font_size).max(1.0),
            _ => para.line_height.or(style.line_height).unwrap_or(1.3),
        },
        // WROOT-078: 段落自身声明优先于样式表继承（标准OOXML/CSS级联语义，
        // 与 ParagraphStyle::merge 里 basedOn 继承链"更具体覆盖更通用"的
        // 逻辑一致）。第三级兜底见下方 implicit_formula_center：段落自身和
        // 样式表都没有显式对齐时，才轮到"公式独立成段"的隐式居中规则。
        alignment: para.alignment.or(style.alignment).or_else(|| {
            implicit_formula_center_alignment(para)
        }),
    }
}

/// m:oMath隐式居中（DOCX_LATEX公式渲染调研纪要_v2 + ROE_DOCX_Progress_v27
/// 遗留的原始目标）。规则：段落没有任何显式对齐来源（自身w:jc和样式表继承
/// 都是None，调用方已经用.or_else确保了这一点）、且段落内容实质上只是
/// "一个公式、其余全是空白"时，视为公式独立成段，隐式按Center处理——这是
/// Word/LibreOffice等主流实现对独立公式段落的通行展示习惯，不是OOXML规范
/// 强制要求，纯粹是"没有更具体信息时的更好默认值"。
///
/// 判断标准（已与用户拍板，v2调研纪要"开放问题"的最终结论）：
/// 1. runs中恰好一个run的formula.is_some()（不多不少，多个公式或零个公式
///    都不算"独立成段"，不触发）。
/// 2. 其余run的text.trim()必须为空（允许空格/纯空白run，不允许任何非空白
///    正文文字与公式同段）。
/// 3. Paragraph.bookmarks 是段落级独立字段、不在runs里，天然不影响这个
///    判断，不需要专门排除。
/// 4. m:oMathPara场景：docx.rs当前只抠取m:oMathPara内部<m:oMath>子串转
///    LaTeX，m:oMathParaPr携带的对齐元信息目前完全未解析（只在注释提及，
///    代码未读取），所以oMathPara包裹的公式和裸m:oMath在对齐层面待遇
///    一致，此规则不需要区分两者。未来若要支持m:oMathParaPr的显式对齐，
///    应插在 para.alignment.or(style.alignment) 与本函数之间的优先级
///    链条上，不需要改动本函数。
fn implicit_formula_center_alignment(
    para: &roe_model::writer::Paragraph,
) -> Option<roe_model::style::Alignment> {
    let mut formula_count = 0usize;
    for run in &para.runs {
        if run.formula.is_some() {
            formula_count += 1;
        } else if !run.text.trim().is_empty() {
            // 存在非空白正文文字，不是"纯公式段落"，不触发隐式居中
            return None;
        }
    }
    if formula_count == 1 {
        Some(roe_model::style::Alignment::Center)
    } else {
        None
    }
}

/// 单个 Run 最终解析出的完整样式（逐层 merge 后的结果）：
/// 默认值 -> 段落样式表(ParagraphStyle) -> 字符样式表(CharStyle, 通过 span 引用)
/// -> Run 内联字段（优先级最高）。
///
/// 块D设计：之前 resolve_para_style 只返回"整段统一一套样式"，无法表达
/// 一段话内部分加粗这种 Office 真实场景。resolve_run_style 是 per-run
/// 精度的替代品，layout.rs 主循环（D3）会对每个 Run 调用一次。
#[derive(Debug, Clone, Copy)]
pub struct ResolvedRunStyle {
    pub font_size:     f32,
    pub font_family_is_set: bool, // 占位：字体族字符串另存，避免这里强行 Copy String
    pub bold:          bool,
    pub italic:        bool,
    pub underline:     bool,
    pub strikethrough: bool,
    pub color:         Option<Color>,
    pub background:    Option<Color>,
    pub script:        ScriptPosition,
    /// 公式渲染（WROOT-059/060/061）：标记这个span是否是公式run。
    /// 为true时，tile.rs画字阶段应跳过常规文字绘制，转而用run_idx
    /// （来自layout_run.glyphs[...].metadata）回查para.runs[run_idx].formula
    /// 拿完整Formula数据（latex + width_pt/height_pt/depth_pt），走RaTeX
    /// DisplayList绘制路径。不直接把Formula塞进本结构体，是为了保留
    /// #[derive(Copy)]（Formula.latex是String，不能Copy）。
    /// 详见 DOCX_LATEX公式渲染调研纪要_v2.md「已拍板（方案B）」一节。
    pub is_formula:    bool,
    /// 批注功能第三步（渲染层，v150起）：本run处于某个批注range高亮内时，
    /// 为Some(颜色索引)，索引对应tile.rs侧维护的一组预设批注颜色（按
    /// Comment.id % 颜色数量 轮询分配，见resolve_run_comment_state）。
    /// None表示不在任何批注range内。之所以不直接存Color/id（Vec<u32>等
    /// 非Copy类型），是为了保留ResolvedRunStyle的#[derive(Copy)]约束
    /// （与is_formula字段旁注释里"新增Option<String>会破坏Copy、已实测
    /// 踩坑回退过"是同一条硬约束）。
    pub comment_highlight_color_idx: Option<u8>,
    /// 本run是否是它所属批注range（comment_highlight_color_idx对应的
    /// 那个批注）的第一个run——tile.rs据此在这个run左侧画批注起始竖线。
    /// 与comment_bar_end相互独立判断（方案A，OnlyOffice同款：不管单行
    /// 多行，range起止各画一条竖线，不需要跨行状态判断是第几行/共几行）。
    pub comment_bar_start: bool,
    /// 本run是否是它所属批注range的最后一个run，tile.rs据此画批注终止
    /// 竖线。单run的批注（起点终点是同一个run）应同时为true。
    pub comment_bar_end: bool,
    /// 本run所属批注的真实id（Comment.id），用于气泡渲染阶段反查
    /// doc.comments 里的作者/内容/done状态。与 comment_highlight_color_idx
    /// 分开存放的原因：颜色索引是 id % COMMENT_HIGHLIGHT_COLOR_COUNT 取模
    /// 得到的，不同id可能撞到同一个颜色索引，不能从颜色反推真实id，
    /// 必须单独存一份原始id。Option<u32> 本身是 Copy 类型，不破坏
    /// ResolvedRunStyle 的 #[derive(Copy)] 约束（教训见本结构体其他
    /// 字段注释：Option<String> 曾破坏Copy约束，已回退）。
    pub comment_id: Option<u32>,
    /// WROOT-107: 字符基线垂直位移（w:position，pt，正上负下）。
    /// Option<f32> 是 Copy，不破坏本结构体的 #[derive(Copy)]。
    pub vertical_position: Option<f32>,
}

/// 解析单个 Run 的最终样式。font_family 因为是 String，不放进
/// ResolvedRunStyle（保持 Copy），单独通过 resolve_run_font_family 取。
pub fn resolve_run_style(
    doc: &WriterDocument,
    para: &roe_model::writer::Paragraph,
    run: &Run,
    default_font_size: f32,
) -> ResolvedRunStyle {
    // 第一层：段落样式表（含继承链）
    let para_style = doc.styles.resolve_paragraph_style(para.style.0.as_str());

    // 第二层：字符样式表（run.char_style 引用的 span 样式，含继承链）
    let char_style = run.char_style.as_ref()
        .map(|s| doc.styles.resolve_char_style(s.0.as_str()));

    // font_size：段落样式 -> 字符样式 -> Run 内联 -> 默认值
    let mut font_size = para_style.font_size.unwrap_or(default_font_size);
    if let Some(cs) = &char_style {
        if let Some(v) = cs.font_size { font_size = v; }
    }
    if let Some(v) = run.font_size { font_size = v; }

    // bold / italic：bool 字段无法区分"未设置"，采用"任一层为 true 即 true"
    // 的语义（与现有 ParagraphStyle::merge / CharStyle::merge 一致）；
    // Run 内联用 Option<bool>，Some(false) 视为显式覆盖为不加粗/不斜体。
    let mut bold = para_style.bold;
    if let Some(cs) = &char_style { if cs.bold { bold = true; } }
    if let Some(v) = run.bold { bold = v; }

    let mut italic = para_style.italic;
    if let Some(cs) = &char_style { if cs.italic { italic = true; } }
    if let Some(v) = run.italic { italic = v; }

    let mut underline = false;
    if let Some(cs) = &char_style { if cs.underline { underline = true; } }
    if let Some(v) = run.underline { underline = v; }

    let mut strikethrough = false;
    if let Some(cs) = &char_style {
        if let Some(v) = cs.strikethrough { strikethrough = v; }
    }
    if let Some(v) = run.strikethrough { strikethrough = v; }

    // WROOT-DOCDEFAULT 续修（v49今日会话发现，比原修复更深一层）：
    // 段落样式（w:type="paragraph"）自带的 w:rPr/w:color（例如 Title/
    // Subtitle 等标题样式常见写法）此前完全没有进入这条兜底链——
    // ParagraphStyle 结构体原来没有 color 字段，导致真实文档里"颜色
    // 定义在段落样式、run 不引用独立字符样式"这一常见场景（标题、
    // 副标题等）全部被短路成 doc_default_color 灰色，即使原来的
    // "字符样式 -> run"两层链路本身没问题。完整链路现在是：
    // 文档级默认 -> 段落样式 -> 字符样式 -> Run 内联，与 font_size
    // 等字段的优先级顺序保持一致。
    let mut color = doc.styles.doc_default_color;
    if para_style.color.is_some() { color = para_style.color; }
    if let Some(cs) = &char_style { if cs.color.is_some() { color = cs.color; } }
    if run.color.is_some() { color = run.color; }

    let mut background = None;
    if let Some(cs) = &char_style { if cs.background.is_some() { background = cs.background; } }
    if run.background.is_some() { background = run.background; }

    let mut script = ScriptPosition::Normal;
    if let Some(cs) = &char_style { if let Some(v) = cs.script { script = v; } }
    if let Some(v) = run.script { script = v; }

    // WROOT-107: 字符基线垂直位移（w:position）。与 script 是两套独立机制：
    // 段落样式无此属性，只有 字符样式 -> Run 内联 两层，后者覆盖前者。
    let mut vertical_position: Option<f32> = None;
    if let Some(cs) = &char_style { if cs.vertical_position.is_some() { vertical_position = cs.vertical_position; } }
    if run.vertical_position.is_some() { vertical_position = run.vertical_position; }

    // STEP6-STAGE-A（阶段A最小验证）：run.field 为 EndnoteRef/FootnoteRef
    // 时，这个 run 是脚注/尾注的编号占位符（docx 原文 <w:endnoteRef/>，
    // run.text 恒为空字符串），Office 里这类编号的标准显示方式就是上标，
    // 这里复用已有的 script 字段触发已有的上标渲染管线（tile.rs 的
    // script_runs 循环），不新增任何字段——ResolvedRunStyle 有
    // #[derive(Copy)]，新增 Option<String> 字段会破坏这个约束（已实测
    // 踩过一次，回退过），所以编号文字本身不经这个结构体传递，改为
    // tile.rs 渲染时通过 run_idx 反查 para.runs[run_idx].field 现场判断
    // （与 tile.rs 已有的 formula 反查模式一致，见 tile.rs 里
    // `p.runs.get(run_idx).and_then(|r| r.formula.as_ref())` 那行）。
    // 阶段A本分支只负责"让这个run被上标渲染管线选中"，具体显示什么编号
    // 文字由 tile.rs 侧决定（阶段A写死"1"，阶段B改真实计数）。
    if matches!(run.field, Some(roe_model::writer::FieldKind::EndnoteRef)
        | Some(roe_model::writer::FieldKind::FootnoteRef))
    {
        script = ScriptPosition::Superscript;
    }

    ResolvedRunStyle {
        font_size,
        font_family_is_set: false,
        bold,
        italic,
        underline,
        strikethrough,
        color,
        background,
        script,
        is_formula: run.formula.is_some(),
        // 批注高亮/竖线状态：resolve_run_style本身是纯函数（逐run独立
        // 调用，见函数doc注释），没有"前面哪些批注range还没结束"这种
        // 跨run上下文，无法在这里算出真实值。真正的值由调用方
        // （1873行附近run_styles构建循环）在拿到这个默认值之后，
        // 用一个维护"当前活跃批注栈"的独立循环二次叠加写入——这里
        // 只负责给出安全默认值（不在任何批注内），保证没有走到那条
        // 叠加逻辑的调用点（比如测试里直接调用resolve_run_style）
        // 不会因为编译器要求穷尽字段而写错值。
        comment_highlight_color_idx: None,
        comment_bar_start: false,
        comment_bar_end: false,
        comment_id: None,
        vertical_position,
    }
}

/// font_family 单独解析（String，不放进 ResolvedRunStyle 以保持 Copy）。
/// 优先级同 resolve_run_style：段落样式 -> 字符样式 -> Run 内联。
pub fn resolve_run_font_family(
    doc: &WriterDocument,
    para: &roe_model::writer::Paragraph,
    run: &Run,
) -> Option<String> {
    let para_style = doc.styles.resolve_paragraph_style(para.style.0.as_str());
    let mut family = para_style.font_family;
    let mut family_ascii = para_style.font_family_ascii;

    if let Some(cs_ref) = &run.char_style {
        let cs = doc.styles.resolve_char_style(cs_ref.0.as_str());
        if cs.font_family.is_some() { family = cs.font_family; }
        if cs.font_family_ascii.is_some() { family_ascii = cs.font_family_ascii; }
    }
    if run.font_family.is_some() { family = run.font_family.clone(); }
    if run.font_family_ascii.is_some() { family_ascii = run.font_family_ascii.clone(); }

    // [WROOT-093小尾巴2] w:rFonts 的 ascii/eastAsia 本应按文字的 Unicode
    // 范围逐段选择（拉丁字符用 ascii，CJK/西里尔等非基本拉丁字符用
    // eastAsia），但此前解析层用的是"整体二选一，eastAsia 优先"这个粗糙
    // 策略，导致纯拉丁文字的 run 被错误套用东亚字体（字重/字宽/衬线风格
    // 与文档实际设计不符，例如把 Liberation Serif 误判成 DejaVu Sans）。
    // 这里做一个折中的过渡启发式（未做到逐字符切分那么精确，但能覆盖
    // "整个 run 都是基本拉丁字符"这个最常见场景）：若 run 的文字全部落在
    // 基本拉丁字符范围（ASCII），且存在 ascii 候选字体，则优先使用 ascii
    // 候选，忽略 eastAsia 选出的结果。
    let is_all_ascii = run.text.chars().all(|c| c.is_ascii());
    if family_ascii.is_some() && is_all_ascii {
        family = family_ascii.clone();
    }

    family
}

#[cfg(test)]
mod d3_tests {
    use super::*;
    use roe_parser::odf::fodt::FodtParser;

    const RUN_STYLE_FODT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
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
    <style:style style:name="Bold" style:family="text">
      <style:text-properties fo:font-weight="bold"/>
    </style:style>
  </office:styles>
  <office:body>
    <office:text>
      <text:p text:style-name="Default">普通文字<text:span text:style-name="Bold">加粗文字</text:span></text:p>
    </office:text>
  </office:body>
</office:document>"#;

    /// D3 验证：一个段落内两个不同样式的 run（普通 + 加粗），跑完
    /// layout() 后，产出的 LineBox.runs 应该能区分出两段不同 bold 值的
    /// LineRunSpan，而不是整行只有一套统一样式（D3 之前的旧行为）。
    #[test]
    fn test_per_run_style_produces_distinct_spans() {
        let doc = FodtParser::parse(RUN_STYLE_FODT).expect("parse failed");
        let registry = FontRegistry::new_with_system_fonts();
        let mut engine = ParagraphLayoutEngine::new(registry);
        let result = engine.layout(&doc);

        let arena = &result.arena;
        let page = &arena.pages[arena.page_order[0]];
        assert!(!page.frames.is_empty(), "应至少有一个 frame");

        let frame = &arena.frames[page.frames[0]];
        assert!(!frame.text_lines().is_empty(), "应至少有一行");

        let line = &frame.text_lines()[0];
        // 核心断言：runs 应该有至少2段（普通 run + 加粗 run），
        // 不是 D3 之前那种空 Vec 或单一覆盖全行的旧行为。
        assert!(
            line.runs.len() >= 2,
            "一行内应区分出至少2个不同样式的 run 片段，实际 runs.len()={}, runs={:?}",
            line.runs.len(), line.runs
        );

        // 至少有一段是 bold=true，至少有一段是 bold=false（验证样式确实
        // 区分开了，不是两段都用了同一套样式）。
        let has_bold   = line.runs.iter().any(|r| r.style.bold);
        let has_normal = line.runs.iter().any(|r| !r.style.bold);
        assert!(has_bold,   "应存在 bold=true 的 run 片段，实际 runs={:?}", line.runs);
        assert!(has_normal, "应存在 bold=false 的 run 片段，实际 runs={:?}", line.runs);
    }

    /// WROOT-096-followup（Layer 1-B渲染层）：resolve_cell_border 合并逻辑
    /// 单测。覆盖4种组合：cell自己有值优先、cell没有时fallback到表格默认值、
    /// 两者都没有时返回None（渲染层兜底不是本函数职责）、颜色与宽度各自独立
    /// 合并（不应互相影响，例如cell自己设了颜色但没设宽度时，宽度仍应
    /// fallback到表格默认值，不能因为颜色命中了cell自身就连宽度也一起
    /// 锁定成cell自身的None）。
    #[test]
    fn test_resolve_cell_border_fallback() {
        use roe_model::common::{Table, TableCell, TableRow, Color};

        let red = Color { r: 255, g: 0, b: 0, a: 255 };
        let green = Color { r: 0, g: 176, b: 80, a: 255 };

        fn make_table(default_color: Option<Color>, default_width: Option<f32>) -> Table {
            Table {
                float_pos: None,
                rows: Vec::<TableRow>::new(),
                col_widths: Vec::new(),
                style_id: None,
                first_row: false,
                first_col: false,
                last_row: false,
                last_col: false,
                band_row: false,
                band_col: false,
                default_border_color: default_color,
                default_border_width: default_width,
                inside_h_color: None,
                inside_h_width: None,
                inside_v_color: None,
                inside_v_width: None,
            }
        }

        fn make_cell(border_color: Option<Color>, border_width: Option<f32>) -> TableCell {
            TableCell {
                content: Vec::new(),
                col_span: 1,
                row_span: 1,
                style: None,
                hidden: false,
                bg_color: None,
                border_color,
                border_width,
                pad_left: None,
                pad_right: None,
                pad_top: None,
                pad_bottom: None,
            }
        }

        // Color 未实现 PartialEq（跨 crate 定义，不受本文件控制），
        // 用小helper逐字段比较，避免直接 assert_eq! 整个struct/tuple。
        fn color_eq(a: Option<Color>, b: Option<Color>) -> bool {
            match (a, b) {
                (None, None) => true,
                (Some(x), Some(y)) => x.r == y.r && x.g == y.g && x.b == y.b && x.a == y.a,
                _ => false,
            }
        }

        // 情况1：cell自己有颜色和宽度，表格也有默认值 —— 应优先用cell自己的
        let table1 = make_table(Some(green), Some(3.0));
        let cell1 = make_cell(Some(red), Some(1.5));
        let (c1, w1) = resolve_cell_border(&cell1, &table1);
        assert!(color_eq(c1, Some(red)), "cell自身有值时应优先使用cell自身的border_color，不应被表格默认值覆盖，实际={:?}", c1);
        assert_eq!(w1, Some(1.5), "cell自身有值时应优先使用cell自身的border_width，不应被表格默认值覆盖");

        // 情况2：cell自己没有，表格有默认值 —— 应fallback到表格默认值
        let table2 = make_table(Some(green), Some(3.0));
        let cell2 = make_cell(None, None);
        let (c2, w2) = resolve_cell_border(&cell2, &table2);
        assert!(color_eq(c2, Some(green)), "cell自身无值时应fallback到表格级default_border_color，实际={:?}", c2);
        assert_eq!(w2, Some(3.0), "cell自身无值时应fallback到表格级default_border_width");

        // 情况3：两者都没有 —— 应返回 (None, None)，兜底交给渲染层tile.rs处理
        let table3 = make_table(None, None);
        let cell3 = make_cell(None, None);
        let (c3, w3) = resolve_cell_border(&cell3, &table3);
        assert!(color_eq(c3, None), "cell和表格都没有边框定义时应返回None，不应在此函数内部提前兜底黑色，实际={:?}", c3);
        assert_eq!(w3, None, "cell和表格都没有边框定义时应返回None，不应在此函数内部提前兜底1.0px");

        // 情况4：颜色与宽度独立合并 —— cell只设了颜色没设宽度，宽度应独立
        // fallback到表格默认值，不受颜色命中cell自身这件事影响
        let table4 = make_table(Some(green), Some(3.0));
        let cell4 = make_cell(Some(red), None);
        let (c4, w4) = resolve_cell_border(&cell4, &table4);
        assert!(color_eq(c4, Some(red)), "颜色应独立合并：应使用cell自身的red，实际={:?}", c4);
        assert_eq!(w4, Some(3.0), "宽度应独立合并：cell未设置宽度时应fallback到表格的3.0，不应受颜色命中cell自身这件事影响");
    }
    /// WROOT-080 排查：实测 Parley 下空格字符的真实宽度，
    /// 对比 docx.rs 里公式占位符沿用的 cosmic-text 系数
    /// `space_width_pt = font_size_pt * 0.26`（见 v18 交接文档），
    /// 判断迁移到 Parley 后这个系数是否还准确。
    #[test]
    fn measure_parley_space_width() {
        use parley::{Alignment, AlignmentOptions, GenericFamily, LineHeight, StyleProperty};

        let mut registry = FontRegistry::new_with_system_fonts();

        fn measure(registry: &mut FontRegistry, font_size_pt: f32, space_count: usize) -> f32 {
            let text = " ".repeat(space_count);
            // WROOT-FONTPIPE v85诊断C：必须放在 ranged_builder() 借走
        // &mut font_context 之前，否则触发E0499。打印"Liberation Serif"
        // family_by_name查到的FamilyId，跟vendor插桩打印的FamilyId(115)对比。
        if let Some(fam) = registry.font_context.collection.family_by_name("Liberation Serif") {
            eprintln!("[DIAG] family_by_name(\"Liberation Serif\") 返回的 FamilyId = {:?}", fam.id());
        }

        let default_brush = crate::tile::RunBrush { color: [0, 0, 0, 255], run_idx: None };
            let mut builder = registry.layout_context.ranged_builder(
                &mut registry.font_context, &text, 1.0, true,
            );
            builder.push_default(StyleProperty::Brush(default_brush));
            builder.push_default(GenericFamily::SystemUi);
            builder.push_default(LineHeight::Absolute(font_size_pt));
            builder.push_default(StyleProperty::FontSize(font_size_pt));

            let mut layout: parley::Layout<crate::tile::RunBrush> = builder.build(&text);
            layout.break_all_lines(Some(10000.0));
            layout.align(Alignment::Start, AlignmentOptions::default());

            let mut total_advance = 0.0f32;
            for line in layout.lines() {
                let lm = line.metrics();
                total_advance = lm.advance;
            }
            total_advance
        }

        eprintln!("{:<10} {:<12} {:<20} {:<20} {:<10}",
            "font_pt", "space_n", "parley_total_w", "cosmic_assumed_w", "ratio");

        for font_size_pt in [9.0f32, 10.5, 12.0, 14.0, 18.0] {
            for space_count in [1usize, 5, 10, 20] {
                let parley_w = measure(&mut registry, font_size_pt, space_count);
                let cosmic_assumed_single = font_size_pt * 0.26;
                let cosmic_assumed_total = cosmic_assumed_single * space_count as f32;
                let ratio = if cosmic_assumed_total > 0.0 { parley_w / cosmic_assumed_total } else { 0.0 };
                eprintln!("{:<10} {:<12} {:<20.3} {:<20.3} {:<10.3}",
                    font_size_pt, space_count, parley_w, cosmic_assumed_total, ratio);
            }
        }

        eprintln!("\n单个空格的 Parley 实际宽度（用于反推新系数）：");
        for font_size_pt in [9.0f32, 10.5, 12.0, 14.0, 18.0] {
            let w20 = measure(&mut registry, font_size_pt, 20);
            let per_space = w20 / 20.0;
            let implied_coefficient = per_space / font_size_pt;
            eprintln!("font_size_pt={:<6} per_space_width_pt={:<10.4} implied_coefficient={:.4} (cosmic用的是0.26)",
                font_size_pt, per_space, implied_coefficient);
        }
    }

    #[test]
    fn font_pipeline_push_actually_selects_liberation_serif() {
        // WROOT-FONTPIPE 回归测试（v85定案）：验证 ranged_builder.push()
        // 对具名字体的请求，最终会被 Parley 正确选中，而不是被
        // push_default() 的通用族默认值覆盖。
        //
        // 用真实字节内容的哈希比较字体是否一致，不用 fontique 内部的
        // Blob自增id——id只在同一次连续调用序列内有意义，插入额外的
        // 字体load操作会让后续所有id整体偏移，导致比较失真。
        //
        // 找"预期字体"时必须同时限定 weight==NORMAL 且 style==Normal。
        // 只按weight筛选是不够的：Liberation Serif的Italic和Regular
        // 变体weight都是400.0，仅按weight筛会先命中Italic返回，
        // 这正是本次排查里push()被误判"没生效"的真正原因。
        use parley::{Alignment, AlignmentOptions, GenericFamily, LineHeight, StyleProperty, FontFamily};
        use core::hash::{Hash, Hasher};

        fn blob_hash(blob: &parley::fontique::Blob<u8>) -> u64 {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            let bytes: &[u8] = blob.as_ref();
            bytes.hash(&mut hasher);
            hasher.finish()
        }

        let mut registry = FontRegistry::new_with_system_fonts();

        let expected_family = registry
            .font_context
            .collection
            .family_by_name("Liberation Serif")
            .expect("系统字体库里查不到Liberation Serif，测试环境异常（需确认已装该字体）");
        let expected_font_info = expected_family
            .fonts()
            .iter()
            .find(|info| {
                info.weight() == parley::fontique::FontWeight::NORMAL
                    && info.style() == parley::fontique::FontStyle::Normal
            })
            .expect("Liberation Serif family下找不到Regular(weight=400, style=Normal)变体，测试环境异常");
        let expected_blob = expected_font_info
            .load(None)
            .expect("FontInfo::load(None)未能加载出字体数据，测试环境异常");
        let expected_hash = blob_hash(&expected_blob);

        let text = "Hello World";
        let font_size_pt = 12.0f32;

        let default_brush = crate::tile::RunBrush { color: [0, 0, 0, 255], run_idx: None };
        let mut builder = registry.layout_context.ranged_builder(
            &mut registry.font_context, text, 1.0, true,
        );
        builder.push_default(StyleProperty::Brush(default_brush));
        builder.push_default(GenericFamily::SystemUi);
        builder.push_default(LineHeight::Absolute(font_size_pt));
        builder.push_default(StyleProperty::FontSize(font_size_pt));

        let range = 0..text.len();
        builder.push(
            StyleProperty::FontFamily(FontFamily::named("Liberation Serif")),
            range,
        );

        let mut layout: parley::Layout<crate::tile::RunBrush> = builder.build(text);
        layout.break_all_lines(Some(10000.0));
        layout.align(Alignment::Start, AlignmentOptions::default());

        let mut checked_any = false;
        for line in layout.lines() {
            for run in line.runs() {
                checked_any = true;
                let actual_blob = run.font().font.data.clone();
                let actual_hash = blob_hash(&actual_blob);
                assert_eq!(
                    actual_hash, expected_hash,
                    "push(FontFamily::named(\"Liberation Serif\")) 之后，Parley实际选中的字体                      与系统fontique按名字查到的Liberation Serif Regular不一致（按内容哈希比较）。"
                );
            }
        }
        assert!(checked_any, "layout.lines()为空，没有run可供检查，text或push调用可能有问题");
    }
}



#[cfg(test)]
mod stepb_note_numbering_tests {
    //! STEP6-STAGE-B 单元测试：验证脚注/尾注真实动态计数 + numFmt 格式化。
    //! 参照 v135 交接文档 2.2 节要求的覆盖点：
    //! ①同文档多个引用递增计数 ②endnote/footnote独立互不干扰
    //! ③无endnotes/footnotes时的回归保护 ④numFmt缺省时正确兜底Arabic
    //!
    //! 这些测试断言的是"layout不panic、能正常产出排版结果"这条底线
    //! （回归保护），不是逐字断言渲染出的具体文字内容——那部分依赖真实
    //! 样本肉眼核对（WROOT-075），因为从 FrameArena/ParagraphFrame 里
    //! 反查某个run最终渲染成了什么文字字符串，需要深入tile.rs内部结构，
    //! 超出了这批测试的目的（防止阶段B改动导致排版整体崩溃/panic）。
    use super::*;
    use roe_model::writer::{
        WriterDocument, BodyElement, Paragraph, Run, FieldKind, NumFormat,
    };
    use roe_model::common::StyleRef;

    fn make_note_ref_run(field: FieldKind, endnote_ref: Option<u32>, footnote_ref: Option<u32>) -> Run {
        Run {
            text: String::new(),
            field: Some(field),
            endnote_ref,
            footnote_ref,
            ..Default::default()
        }
    }

    fn make_text_run(text: &str) -> Run {
        Run { text: text.to_string(), ..Default::default() }
    }

    fn make_paragraph(runs: Vec<Run>) -> BodyElement {
        BodyElement::Paragraph(Paragraph {
            style: StyleRef("Normal".to_string()),
            runs,
            ..Default::default()
        })
    }

    fn make_doc(body: Vec<BodyElement>, endnote_num_fmt: NumFormat, footnote_num_fmt: NumFormat) -> WriterDocument {
        WriterDocument {
            body,
            endnote_num_fmt,
            footnote_num_fmt,
            ..Default::default()
        }
    }

    fn new_engine() -> ParagraphLayoutEngine {
        let registry = crate::font_registry::FontRegistry::new_with_system_fonts();
        ParagraphLayoutEngine::new(registry)
    }

    /// ①同文档内多个 endnote 引用应能正常排版（递增编号逻辑不panic）。
    #[test]
    fn endnote_numbering_increments_across_multiple_refs() {
        let doc = make_doc(
            vec![
                make_paragraph(vec![
                    make_text_run("first "),
                    make_note_ref_run(FieldKind::EndnoteRef, Some(2), None),
                ]),
                make_paragraph(vec![
                    make_text_run("second "),
                    make_note_ref_run(FieldKind::EndnoteRef, Some(3), None),
                ]),
            ],
            NumFormat::Arabic,
            NumFormat::Arabic,
        );
        let mut engine = new_engine();
        let result = engine.layout(&doc);
        assert!(!result.arena.frames.is_empty(), "layout should produce at least one frame");
    }

    /// ②endnote 和 footnote 各自独立计数序列，混合出现时不应互相干扰
    /// （不共享同一个全局计数器）。
    #[test]
    fn endnote_and_footnote_numbering_are_independent_sequences() {
        let doc = make_doc(
            vec![
                make_paragraph(vec![
                    make_text_run("text "),
                    make_note_ref_run(FieldKind::FootnoteRef, None, Some(1)),
                    make_text_run(" more "),
                    make_note_ref_run(FieldKind::EndnoteRef, Some(1), None),
                ]),
            ],
            NumFormat::Arabic,
            NumFormat::Arabic,
        );
        let mut engine = new_engine();
        let result = engine.layout(&doc);
        assert!(!result.arena.frames.is_empty(), "layout should produce at least one frame");
    }

    /// ③没有任何 note 引用的普通文档，layout 不应受本次改动影响。
    #[test]
    fn documents_without_any_note_ref_are_unaffected() {
        let doc = make_doc(
            vec![make_paragraph(vec![make_text_run("just plain text, no notes")])],
            NumFormat::Arabic,
            NumFormat::Arabic,
        );
        let mut engine = new_engine();
        let result = engine.layout(&doc);
        assert!(!result.arena.frames.is_empty(), "plain document should still lay out fine");
    }

    /// ④numFmt 缺省（WriterDocument::default()）时应兜底为 Arabic。
    #[test]
    fn missing_num_fmt_defaults_to_arabic() {
        let doc = WriterDocument {
            body: vec![make_paragraph(vec![
                make_text_run("text "),
                make_note_ref_run(FieldKind::EndnoteRef, Some(5), None),
            ])],
            ..Default::default()
        };
        assert_eq!(doc.endnote_num_fmt, NumFormat::Arabic, "NumFormat default should be Arabic");
        let mut engine = new_engine();
        let result = engine.layout(&doc);
        assert!(!result.arena.frames.is_empty(), "layout with default num_fmt should still succeed");
    }

    /// ⑤lowerRoman 格式下，两个不同 endnote 引用应各自映射到不同的
    /// 罗马数字编号（间接验证：至少不应panic，真实文字内容核对见
    /// WROOT-075 真实样本渲染）。
    #[test]
    fn lower_roman_num_fmt_does_not_panic() {
        let doc = make_doc(
            vec![make_paragraph(vec![
                make_text_run("some text"),
                make_note_ref_run(FieldKind::EndnoteRef, Some(2), None),
                make_text_run("some other text"),
                make_note_ref_run(FieldKind::EndnoteRef, Some(3), None),
            ])],
            NumFormat::LowerRoman,
            NumFormat::Arabic,
        );
        let mut engine = new_engine();
        let result = engine.layout(&doc);
        assert!(!result.arena.frames.is_empty(), "lowerRoman formatting should not break layout");
    }
}
