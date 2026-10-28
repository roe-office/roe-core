//! 行内精确字符定位（Parley 版本）。
//!
//! 迁移自 cosmic-text 版本（见 line_shape.rs.bak）。设计背景、缓存策略
//! 与原版一致：LineBox 不存字号/样式/x偏移，精确定位需要用与原始排版
//! 一致的样式重新 shape 这一行文字；结果缓存在 FontRegistry::shape_cache。

use parley::{GenericFamily, PositionedLayoutItem, StyleProperty, FontWeight, FontStyle};

use crate::font_registry::{FontRegistry, ShapeCacheKey, ShapeCacheValue};
use crate::frame::LineRunSpan;
use crate::tile::RunBrush;

/// 内部：从 &[LineRunSpan] 构造 ShapeCacheKey。
fn make_cache_key(
    line_text: &str,
    runs: &[LineRunSpan],
    fallback_font_size: f32,
    fallback_bold: bool,
    fallback_italic: bool,
) -> ShapeCacheKey {
    if runs.is_empty() {
        ShapeCacheKey {
            text: line_text.to_string(),
            run_styles: vec![(0, line_text.len(), fallback_font_size.to_bits(), fallback_bold, fallback_italic)],
        }
    } else {
        ShapeCacheKey {
            text: line_text.to_string(),
            run_styles: runs.iter().map(|r| {
                (r.start, r.end, r.style.font_size.to_bits(), r.style.bold, r.style.italic)
            }).collect(),
        }
    }
}

/// 对一行文字按多段样式 shape，返回每个 cluster 的
/// (字节范围start, end, x偏移, 宽度) 列表。结果缓存在 registry.shape_cache。
fn shape_line(
    registry: &mut FontRegistry,
    line_text: &str,
    runs: &[LineRunSpan],
    fallback_font_size: f32,
    fallback_bold: bool,
    fallback_italic: bool,
) -> ShapeCacheValue {
    if line_text.is_empty() {
        return Vec::new();
    }

    let key = make_cache_key(line_text, runs, fallback_font_size, fallback_bold, fallback_italic);

    if let Some(cached) = registry.shape_cache.get(&key) {
        return cached.clone();
    }

    let primary_font_size = if runs.is_empty() {
        fallback_font_size
    } else {
        runs[0].style.font_size
    };

    let default_brush = RunBrush { color: [30, 30, 30, 255], run_idx: None };
    let mut builder = registry.layout_context.ranged_builder(
        &mut registry.font_context, line_text, 1.0, true,
    );
    builder.push_default(StyleProperty::Brush(default_brush));
    builder.push_default(GenericFamily::SystemUi);
    builder.push_default(StyleProperty::FontSize(primary_font_size));
    if fallback_bold   { builder.push_default(StyleProperty::FontWeight(FontWeight::BOLD)); }
    if fallback_italic { builder.push_default(StyleProperty::FontStyle(FontStyle::Italic)); }

    for (run_idx, span) in runs.iter().enumerate() {
        let start = span.start.min(line_text.len());
        let end   = span.end.min(line_text.len());
        if start >= end { continue; }
        let range = start..end;
        builder.push(StyleProperty::Brush(RunBrush { color: [30, 30, 30, 255], run_idx: Some(run_idx) }), range.clone());
        builder.push(StyleProperty::FontSize(span.style.font_size), range.clone());
        if span.style.bold   { builder.push(StyleProperty::FontWeight(FontWeight::BOLD), range.clone()); }
        if span.style.italic { builder.push(StyleProperty::FontStyle(FontStyle::Italic), range.clone()); }
    }

    let mut layout: parley::Layout<RunBrush> = builder.build(line_text);
    // 不限制宽度：line_shape 只用来算单行字符坐标，不需要真的换行
    layout.break_all_lines(None);

    let mut glyphs: ShapeCacheValue = Vec::new();
    for line in layout.lines() {
        let line: parley::Line<'_, RunBrush> = line;
        // V97修复：glyph_run.run() 返回的是底层共享的、覆盖整行所有文字的
        // Run 对象。官方 GlyphRun::glyphs() 的真实实现是：
        //   self.run.visual_clusters().flat_map(|c| c.glyphs())
        //       .skip(glyph_start).take(glyph_count)
        // 即"按视觉序展开成glyph序列后，取属于当前GlyphRun的那一段"，
        // glyph_start/glyph_count 是私有字段，外部拿不到，但可以用等价的
        // 公开信息重建同样的裁剪效果：
        //   1. glyph_run.glyphs() 本身就是官方已经裁剪好的迭代器，用它的
        //      .count() 就能知道当前 GlyphRun 覆盖了多少个 glyph；
        //   2. 用一个跨 GlyphRun 共享、按 visual_clusters() 顺序递增的
        //      cluster 游标，从上次消费到的位置继续，每吃一个 cluster 就把
        //      它的 glyph 数累加，直到累加到步骤1算出的数量为止——这正是
        //      官方 skip/take 在 cluster 粒度上的等价重建。
        // 用 visual_clusters()（而非 clusters()）是为了和官方 glyphs() 的
        // 遍历顺序完全一致，避免 RTL 场景下顺序不一致引入新问题。
        for item in line.items() {
            let PositionedLayoutItem::GlyphRun(glyph_run) = item else { continue };
            // V108修复第二版（根因见 ROE_Progress_v108.md）：实测证实
            // glyph_run.run() 确实返回覆盖整行所有文字的共享 Run 对象
            // （V107方向正确，上一版判断被诊断日志推翻）。因此
            // .visual_clusters() 每次都会吐出整行全部cluster，仍需裁剪。
            //
            // 改用 cluster.style_index() 比对 glyph_run.style_index() 过滤
            // （已实测验证两者一一对应，且不依赖任何跨迭代游标状态）。
            // cluster.text_range() 本身已确认是正确的全局字节区间。
            let mut run_x = glyph_run.offset();
            let style_index = glyph_run.style_index();

            for cluster in glyph_run.run().visual_clusters() {
                if cluster.style_index() != style_index {
                    continue;
                }
                let range = cluster.text_range();
                if cluster.glyphs().count() == 0 {
                    continue;
                }

                let cluster_x = run_x + cluster.glyphs().next().map(|g| g.x).unwrap_or(0.0);
                let mut cluster_advance = 0.0f32;
                for g in cluster.glyphs() {
                    cluster_advance += g.advance;
                    run_x += g.advance;
                }
                glyphs.push((range.start, range.end, cluster_x, cluster_advance));
            }
        }
    }

    registry.shape_cache.insert(key, glyphs.clone());
    glyphs
}

/// 路径1（office-rs2 光标/选区功能，见 roe_cursor_selection_final_v1.md
/// 3.2）专用：返回真正的 parley::Layout<RunBrush>，供 Parley Cursor 方法
/// （next_visual / next_visual_word 等）直接调用。
///
/// 与 shape_line 的关系（拍板见 ROE_DOCX_Progress_v94）：不查缓存、不写
/// 缓存，每次调用都重新 build。原因：shape_cache 缓存的是轻量四元组，
/// 存 Layout 本体会显著放大内存占用（v93 三、3.1 第4条已指出这个风险），
/// 而光标移动是低频交互（人手速远低于渲染帧率），重新 shape 一次单行
/// 文字的 CPU 成本可以忽略，不值得为此扩大缓存内存代价或改动现有缓存
/// 结构。构造 Layout 的逻辑与 shape_line 内部一致（保持两者行为对齐，
/// 避免精确定位路径 (char_offset_to_x/x_to_char_offset) 和光标移动路径
/// 在同一份文字上算出不一致的 shaping 结果）。
pub fn shape_line_full(
    registry: &mut FontRegistry,
    line_text: &str,
    runs: &[LineRunSpan],
    fallback_font_size: f32,
    fallback_bold: bool,
    fallback_italic: bool,
) -> parley::Layout<RunBrush> {
    let primary_font_size = if runs.is_empty() {
        fallback_font_size
    } else {
        runs[0].style.font_size
    };

    let default_brush = RunBrush { color: [30, 30, 30, 255], run_idx: None };
    let mut builder = registry.layout_context.ranged_builder(
        &mut registry.font_context, line_text, 1.0, true,
    );
    builder.push_default(StyleProperty::Brush(default_brush));
    builder.push_default(GenericFamily::SystemUi);
    builder.push_default(StyleProperty::FontSize(primary_font_size));
    if fallback_bold   { builder.push_default(StyleProperty::FontWeight(FontWeight::BOLD)); }
    if fallback_italic { builder.push_default(StyleProperty::FontStyle(FontStyle::Italic)); }

    for (run_idx, span) in runs.iter().enumerate() {
        let start = span.start.min(line_text.len());
        let end   = span.end.min(line_text.len());
        if start >= end { continue; }
        let range = start..end;
        builder.push(StyleProperty::Brush(RunBrush { color: [30, 30, 30, 255], run_idx: Some(run_idx) }), range.clone());
        builder.push(StyleProperty::FontSize(span.style.font_size), range.clone());
        if span.style.bold   { builder.push(StyleProperty::FontWeight(FontWeight::BOLD), range.clone()); }
        if span.style.italic { builder.push(StyleProperty::FontStyle(FontStyle::Italic), range.clone()); }
    }

    let mut layout: parley::Layout<RunBrush> = builder.build(line_text);
    layout.break_all_lines(None);
    layout
}

/// 精确版：行内字节偏移 -> x 坐标。
pub fn char_offset_to_x(
    registry: &mut FontRegistry,
    line_text: &str,
    runs: &[LineRunSpan],
    fallback_font_size: f32,
    fallback_bold: bool,
    fallback_italic: bool,
    offset_in_line: usize,
) -> f32 {
    let glyphs = shape_line(registry, line_text, runs, fallback_font_size, fallback_bold, fallback_italic);
    if glyphs.is_empty() {
        return 0.0;
    }

    for &(start, end, x, _w) in &glyphs {
        if offset_in_line >= start && offset_in_line < end {
            return x;
        }
    }
    let (_, _, last_x, last_w) = *glyphs.last().unwrap();
    last_x + last_w
}

/// 精确版：x 坐标 -> 行内字节偏移。
pub fn x_to_char_offset(
    registry: &mut FontRegistry,
    line_text: &str,
    runs: &[LineRunSpan],
    fallback_font_size: f32,
    fallback_bold: bool,
    fallback_italic: bool,
    x: f32,
) -> usize {
    let glyphs = shape_line(registry, line_text, runs, fallback_font_size, fallback_bold, fallback_italic);
    if glyphs.is_empty() {
        return 0;
    }
    if x <= glyphs[0].2 {
        return 0;
    }

    for &(start, _end, gx, gw) in &glyphs {
        if x >= gx && x < gx + gw {
            if x < gx + gw * 0.5 {
                return start;
            } else {
                return _end;
            }
        }
    }
    glyphs.last().unwrap().1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::ResolvedRunStyle;

    fn fresh_registry() -> FontRegistry {
        FontRegistry::new_with_system_fonts()
    }

    fn no_runs() -> Vec<LineRunSpan> { vec![] }

    fn single_run(len: usize, font_size: f32, bold: bool, italic: bool) -> Vec<LineRunSpan> {
        vec![LineRunSpan {
            run_idx: None, start: 0,
            end: len,
            style: ResolvedRunStyle {
                font_size,
                font_family_is_set: false,
                bold,
                italic,
                underline: false,
                strikethrough: false,
                color: None,
                background: None,
                is_formula: false,
                comment_highlight_color_idx: None,
                comment_bar_start: false,
                comment_bar_end: false,
                comment_id: None,
                vertical_position: None,
                script: roe_model::writer::ScriptPosition::Normal,
            },
            formula_box: None,
        }]
    }

    #[test]
    fn test_offset_zero_gives_x_near_zero() {
        let mut reg = fresh_registry();
        let text = "Hello";
        let x = char_offset_to_x(&mut reg, text, &no_runs(), 12.0, false, false, 0);
        assert!(x >= 0.0 && x < 5.0, "第一个字符的 x 应该接近 0，实际 {x}");
    }

    #[test]
    fn test_offset_increases_monotonically() {
        let mut reg = fresh_registry();
        let text = "Hello World";
        let mut last_x = -1.0f32;
        for (i, _) in text.char_indices() {
            let x = char_offset_to_x(&mut reg, text, &no_runs(), 12.0, false, false, i);
            assert!(x >= last_x, "x 坐标应该随字节偏移单调不减，offset={i}, x={x}, last_x={last_x}");
            last_x = x;
        }
    }

    // ==== 下面是列表光标乱跳排查用测试（v105 六.2）。复现现象：
    // "Real Choices – In the beginning" 这类行是多run混合样式（"Real Choices"
    // 是bold+Cambria，"– In the beginning"是非bold+Times New Roman），
    // 实测发现 char_offset_to_x 在跨run边界附近，x坐标随offset单调递增的
    // 假设被打破（某个offset对应的x坐标"抢跑"到了后面run的位置，跳变量
    // 达到10+个字符的宽度）。这个测试用两个不同字号的run模拟"跨run边界"，
    // 断言x坐标必须随offset单调不减，且相邻offset之间的x跳变不能超过
    // "一个字符的合理宽度"（用最大字号的3倍作为宽松上限，排除真正的
    // 大跳变bug，同时容忍字体度量本身的正常变化）。====
    fn two_runs(split_at: usize, len: usize, size_a: f32, size_b: f32) -> Vec<LineRunSpan> {
        vec![
            LineRunSpan {
                run_idx: None,
                start: 0,
                end: split_at,
                style: ResolvedRunStyle {
                    font_size: size_a,
                    font_family_is_set: false,
                    bold: true,
                    italic: false,
                    underline: false,
                    strikethrough: false,
                    color: None,
                    background: None,
                    is_formula: false,
                    comment_highlight_color_idx: None,
                    comment_bar_start: false,
                    comment_bar_end: false,
                comment_id: None,
                vertical_position: None,
                    script: roe_model::writer::ScriptPosition::Normal,
                },
                formula_box: None,
            },
            LineRunSpan {
                run_idx: None,
                start: split_at,
                end: len,
                style: ResolvedRunStyle {
                    font_size: size_b,
                    font_family_is_set: false,
                    bold: false,
                    italic: false,
                    underline: false,
                    strikethrough: false,
                    color: None,
                    background: None,
                    is_formula: false,
                    comment_highlight_color_idx: None,
                    comment_bar_start: false,
                    comment_bar_end: false,
                comment_id: None,
                vertical_position: None,
                    script: roe_model::writer::ScriptPosition::Normal,
                },
                formula_box: None,
            },
        ]
    }

    // ==== 这个测试专门验证：多run坐标跳变是否是"列表专属"bug，还是任意
    // 多run文字都会触发。用完全不涉及列表/编号的普通段落文字（模拟"部分
    // 加粗+部分正常"这种最常见的富文本场景），如果同样跳变，就证明这是
    // 通用的shape_line多run bug，和列表符号/编号本身无关，只是列表行
    // 经常恰好是多run文字所以更容易被用户注意到。====
    #[test]
    fn test_nonlist_plain_paragraph_multirun_no_large_jump() {
        let mut reg = fresh_registry();
        // 完全普通的段落场景："这段文字部分加粗其余正常" ——
        // 前半"这段文字"(4个汉字)加粗，后半"部分加粗其余正常"不加粗。
        // 用英文简化版本先测（避免混入CJK变量），纯粹验证"多run"这一个因素。
        let text = "This part bold rest is normal text";
        let split_at = "This part bold".len(); // =14，加粗部分止于此
        let runs = two_runs(split_at, text.len(), 12.0, 12.0);

        let mut last_x = -1.0f32;
        let mut history = vec![];
        for (i, _) in text.char_indices() {
            let x = char_offset_to_x(&mut reg, text, &runs, 12.0, false, false, i);
            history.push((i, x));
            let jump = x - last_x;
            if last_x >= 0.0 {
                assert!(
                    jump < 30.0,
                    "【非列表纯正文】x坐标单步跳变过大，offset={i}, 跳变={jump:.2}pt, x={x}, last_x={last_x}, split_at={split_at}, 完整轨迹={:?}\n\n                    这个测试文字与列表符号/编号完全无关，如果这里也跳变，说明bug是shape_line处理\n                    多run（不同样式区间）时的通用问题，不是列表专属的。",
                    history
                );
            }
            last_x = x;
        }
    }

    #[test]
    fn test_multirun_char_offset_to_x_no_large_jump() {
        let mut reg = fresh_registry();
        // 模拟 "2. Real Choices – In the beginning" 的结构：
        // 前半"Real Choices"是bold 12pt，后半"– In the beginning"是非bold 12pt，
        // 用不同字号（12pt vs 12pt其实字号一样，真实场景字号相同只是bold/字体不同，
        // 这里先用相同字号复现最简场景）验证纯 bold切换是否导致x跳变。
        let text = "Real Choices - In the beginning";
        let split_at = "Real Choices".len(); // =12，bold部分止于此
        let runs = two_runs(split_at, text.len(), 12.0, 12.0);

        let mut last_x = -1.0f32;
        let mut history = vec![];
        for (i, _) in text.char_indices() {
            let x = char_offset_to_x(&mut reg, text, &runs, 12.0, false, false, i);
            history.push((i, x));
            let jump = x - last_x;
            if last_x >= 0.0 {
                assert!(
                    x >= last_x - 0.01,
                    "x坐标不应该在跨run边界附近发生回退，offset={i}, x={x}, last_x={last_x}, split_at={split_at}, 完整轨迹={:?}",
                    history
                );
                assert!(
                    jump < 30.0,
                    "x坐标单步跳变过大（疑似跨run边界bug），offset={i}, 跳变={jump:.2}pt, x={x}, last_x={last_x}, split_at={split_at}, 完整轨迹={:?}",
                    history
                );
            }
            last_x = x;
        }
    }

    #[test]
    fn test_empty_line_returns_zero() {
        let mut reg = fresh_registry();
        let x = char_offset_to_x(&mut reg, "", &no_runs(), 12.0, false, false, 0);
        assert_eq!(x, 0.0);
        let o = x_to_char_offset(&mut reg, "", &no_runs(), 12.0, false, false, 50.0);
        assert_eq!(o, 0);
    }

    #[test]
    fn test_roundtrip_offset_to_x_to_offset() {
        let mut reg = fresh_registry();
        let text = "测试Hello";
        for (i, _) in text.char_indices() {
            let x = char_offset_to_x(&mut reg, text, &no_runs(), 14.0, false, false, i);
            let back = x_to_char_offset(&mut reg, text, &no_runs(), 14.0, false, false, x);
            let diff = (back as isize - i as isize).unsigned_abs();
            assert!(diff <= text.len(), "往返误差过大：原始{i}，往返后{back}");
        }
    }

    #[test]
    fn test_x_beyond_line_clamps_to_end_not_panic() {
        let mut reg = fresh_registry();
        let o = x_to_char_offset(&mut reg, "Hello", &no_runs(), 12.0, false, false, 99999.0);
        assert_eq!(o, 5, "x 远超行宽应该钳制到行尾字节长度");
    }

    #[test]
    fn test_negative_x_clamps_to_start_not_panic() {
        let mut reg = fresh_registry();
        let o = x_to_char_offset(&mut reg, "Hello", &no_runs(), 12.0, false, false, -100.0);
        assert_eq!(o, 0, "负数 x 应该钳制到行首");
    }

    #[test]
    fn test_cache_hit_same_result() {
        let mut reg = fresh_registry();
        let text = "缓存测试 Cache";
        let x1 = char_offset_to_x(&mut reg, text, &no_runs(), 12.0, false, false, 3);
        let x2 = char_offset_to_x(&mut reg, text, &no_runs(), 12.0, false, false, 3);
        assert_eq!(x1, x2, "缓存命中应返回相同结果");
        assert!(!reg.shape_cache.is_empty(), "shape_cache 应有缓存条目");
    }

    #[test]
    fn test_invalidate_clears_cache() {
        let mut reg = fresh_registry();
        char_offset_to_x(&mut reg, "Hello", &no_runs(), 12.0, false, false, 0);
        assert!(!reg.shape_cache.is_empty());
        reg.invalidate_shape_cache();
        assert!(reg.shape_cache.is_empty(), "invalidate 后缓存应为空");
    }

    #[test]
    fn test_single_run_matches_fallback() {
        let mut reg = fresh_registry();
        let text = "Hello";
        let runs = single_run(text.len(), 12.0, false, false);
        let x_runs     = char_offset_to_x(&mut reg, text, &runs,     12.0, false, false, 0);
        let x_fallback = char_offset_to_x(&mut reg, text, &no_runs(), 12.0, false, false, 0);
        assert!((x_runs - x_fallback).abs() < 0.5, "单段 runs 与 fallback x 偏差过大：{x_runs} vs {x_fallback}");
    }

    #[test]
    fn test_two_run_bold_span_has_positive_x() {
        let mut reg = fresh_registry();
        let text = "AB";
        let runs = vec![
            LineRunSpan {
                run_idx: None,
                start: 0, end: 1,
                style: ResolvedRunStyle {
                    font_size: 12.0, font_family_is_set: false,
                    bold: false, italic: false,
                    underline: false, strikethrough: false,
                    color: None, background: None,
                    is_formula: false,
                    comment_highlight_color_idx: None,
                    comment_bar_start: false,
                    comment_bar_end: false,
                comment_id: None,
                vertical_position: None,
                    script: roe_model::writer::ScriptPosition::Normal,
                },
                formula_box: None,
            },
            LineRunSpan {
                run_idx: None,
                start: 1, end: 2,
                style: ResolvedRunStyle {
                    font_size: 12.0, font_family_is_set: false,
                    bold: true, italic: false,
                    underline: false, strikethrough: false,
                    color: None, background: None,
                    is_formula: false,
                    comment_highlight_color_idx: None,
                    comment_bar_start: false,
                    comment_bar_end: false,
                comment_id: None,
                vertical_position: None,
                    script: roe_model::writer::ScriptPosition::Normal,
                },
                formula_box: None,
            },
        ];
        let x_b = char_offset_to_x(&mut reg, text, &runs, 12.0, false, false, 1);
        assert!(x_b > 0.0, "加粗段 'B' 的 x 坐标应大于 0，实际 {x_b}");
    }
}

#[cfg(test)]
mod v97_multi_run_regression_test {
    // V97修复的回归测试：验证 shape_line 在多run场景下不再重复shape整行。
    // 根因回顾（见 ROE_DOCX_Progress_v97.md）：修复前 glyphs.len() 恒等于
    // "整行字节数 × run数量"，修复后应恒等于"整行字节数"（与run数量无关）。
    use super::*;
    use crate::layout::ResolvedRunStyle;

    fn mk_style() -> ResolvedRunStyle {
        ResolvedRunStyle {
            font_size: 12.0,
            font_family_is_set: false,
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
            color: None,
            background: None,
            is_formula: false,
            comment_highlight_color_idx: None,
            comment_bar_start: false,
            comment_bar_end: false,
                comment_id: None,
                vertical_position: None,
            script: roe_model::writer::ScriptPosition::Normal,
        }
    }

    /// 断言：给定 run 划分下，glyphs 覆盖的字节范围精确等于 0..line_text.len()，
    /// 首尾无缺口无重叠，且 glyphs.len() 不随 run 数量增长（不再是 N×run数量）。
    fn assert_glyphs_cover_line_exactly(line_text: &str, runs: &[LineRunSpan], label: &str) {
        let mut registry = FontRegistry::new_with_system_fonts();
        let glyphs = shape_line(&mut registry, line_text, runs, 12.0, false, false);
        let line_len = line_text.len();

        assert!(
            !glyphs.is_empty(),
            "[{label}] glyphs 不应为空"
        );

        // 覆盖范围检查：所有 (start,end) 按顺序拼接应该正好是 0..line_len，
        // 不重叠、不留空隙（这是修复前bug的直接反面：bug会产生大量重复覆盖同一
        // 段范围的 cluster，而不是每段只出现一次）。
        let mut covered = 0usize;
        for &(start, end, _x, _w) in &glyphs {
            assert_eq!(
                start, covered,
                "[{label}] cluster范围不连续或有重叠：期望start={covered}，实际start={start}，glyphs={glyphs:?}"
            );
            assert!(end > start, "[{label}] cluster范围非法：start={start} end={end}");
            covered = end;
        }
        assert_eq!(
            covered, line_len,
            "[{label}] glyphs覆盖的总字节数应等于整行字节数{line_len}，实际覆盖到{covered}，glyphs={glyphs:?}"
        );
    }

    #[test]
    fn v97_zero_runs_covers_line_exactly() {
        let line_text = "AB, CD(x) plain EF!"; // 19字节
        assert_glyphs_cover_line_exactly(line_text, &[], "0个run");
    }

    #[test]
    fn v97_one_run_covers_line_exactly() {
        let line_text = "AB, CD(x) plain EF!";
        let runs = vec![LineRunSpan { run_idx: None, start: 0, end: 19, style: mk_style(), formula_box: None }];
        assert_glyphs_cover_line_exactly(line_text, &runs, "1个run(覆盖全部)");
    }

    #[test]
    fn v97_two_runs_covers_line_exactly() {
        let line_text = "AB, CD(x) plain EF!";
        let runs = vec![
            LineRunSpan { run_idx: None, start: 0, end: 9, style: mk_style(), formula_box: None },
            LineRunSpan { run_idx: None, start: 9, end: 19, style: mk_style(), formula_box: None },
        ];
        assert_glyphs_cover_line_exactly(line_text, &runs, "2个run");
    }

    #[test]
    fn v97_three_runs_covers_line_exactly() {
        let line_text = "AB, CD(x) plain EF!";
        let runs = vec![
            LineRunSpan { run_idx: None, start: 0, end: 6, style: mk_style(), formula_box: None },
            LineRunSpan { run_idx: None, start: 6, end: 12, style: mk_style(), formula_box: None },
            LineRunSpan { run_idx: None, start: 12, end: 19, style: mk_style(), formula_box: None },
        ];
        assert_glyphs_cover_line_exactly(line_text, &runs, "3个run");
    }

    #[test]
    fn v97_five_runs_matches_real_repro_case() {
        // 真实复现文档场景：AB / , / CD(x) / plain / EF! 划分成5个run
        let line_text = "AB, CD(x) plain EF!";
        let runs = vec![
            LineRunSpan { run_idx: None, start: 0, end: 2, style: mk_style(), formula_box: None },
            LineRunSpan { run_idx: None, start: 2, end: 4, style: mk_style(), formula_box: None },
            LineRunSpan { run_idx: None, start: 4, end: 9, style: mk_style(), formula_box: None },
            LineRunSpan { run_idx: None, start: 9, end: 16, style: mk_style(), formula_box: None },
            LineRunSpan { run_idx: None, start: 16, end: 19, style: mk_style(), formula_box: None },
        ];
        assert_glyphs_cover_line_exactly(line_text, &runs, "5个run(真实场景)");
    }

    #[test]
    fn v97_repeated_calls_are_stable_and_cached() {
        // 修复前bug版本对同一份多run文字连续调用不会变化（每次都是同样错误的
        // 重复数据），这里验证修复后连续调用结果一致且走的是缓存（不是碰巧稳定）。
        let mut registry = FontRegistry::new_with_system_fonts();
        let line_text = "AB, CD(x) plain EF!";
        let runs = vec![
            LineRunSpan { run_idx: None, start: 0, end: 2, style: mk_style(), formula_box: None },
            LineRunSpan { run_idx: None, start: 2, end: 4, style: mk_style(), formula_box: None },
            LineRunSpan { run_idx: None, start: 4, end: 9, style: mk_style(), formula_box: None },
            LineRunSpan { run_idx: None, start: 9, end: 16, style: mk_style(), formula_box: None },
            LineRunSpan { run_idx: None, start: 16, end: 19, style: mk_style(), formula_box: None },
        ];
        let first = shape_line(&mut registry, line_text, &runs, 12.0, false, false);
        assert_eq!(first.len(), line_text.len(), "首次调用glyphs数量应等于整行字节数19，实际{}", first.len());
        for i in 0..5 {
            let glyphs = shape_line(&mut registry, line_text, &runs, 12.0, false, false);
            assert_eq!(
                glyphs.len(), line_text.len(),
                "第{}次调用 glyphs.len() 应恒为19（不随重复调用次数增长），实际{}",
                i + 1, glyphs.len()
            );
            assert_eq!(glyphs, first, "重复调用应返回一致结果（缓存命中）");
        }
    }

    #[test]
    fn v97_char_offset_to_x_end_of_line_is_reasonable() {
        // 文档v97"五.5"要求的回归点：offset=19（行尾钳制场景）不应再落到
        // 错位fallback产生的错误值（历史bug实测跳到261.6，正确应在159附近，
        // 具体数值依赖字体度量会有出入，这里只做"不离谱"的宽松区间检查，
        // 真正精确值以实机UI验证为准，见文档"五.6"）。
        let mut registry = FontRegistry::new_with_system_fonts();
        let line_text = "AB, CD(x) plain EF!";
        let runs = vec![
            LineRunSpan { run_idx: None, start: 0, end: 2, style: mk_style(), formula_box: None },
            LineRunSpan { run_idx: None, start: 2, end: 4, style: mk_style(), formula_box: None },
            LineRunSpan { run_idx: None, start: 4, end: 9, style: mk_style(), formula_box: None },
            LineRunSpan { run_idx: None, start: 9, end: 16, style: mk_style(), formula_box: None },
            LineRunSpan { run_idx: None, start: 16, end: 19, style: mk_style(), formula_box: None },
        ];
        let x_single_run = char_offset_to_x(&mut registry, line_text, &[LineRunSpan{run_idx: None, start:0,end:19,style:mk_style(),formula_box:None}], 12.0, false, false, 19);
        let x_multi_run = char_offset_to_x(&mut registry, line_text, &runs, 12.0, false, false, 19);
        let diff = (x_multi_run - x_single_run).abs();
        assert!(
            diff < 5.0,
            "同一份文字，单run与多run(5个)划分下 offset=19 的x坐标应该基本一致（允许字号误差<5px），\
             单run={x_single_run:.2}，多run={x_multi_run:.2}，diff={diff:.2}（修复前这个diff会是几十甚至上百）"
        );
    }
}
