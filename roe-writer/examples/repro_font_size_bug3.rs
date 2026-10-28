// 复现v3：验证"Brush(颜色)相同"是否是触发GlyphRun合并、进而导致
// font_size()错误统一的真正条件（v1/v2都用了相同颜色[0,0,0,255]，
// 从未变化过这个变量，是最大的漏测点）。
//
// 对照组A：两个run，颜色相同[0,0,0,255]，字号不同(12/48) —— 模拟真实tdf99602场景
// 对照组B：两个run，颜色不同（黑/红），字号不同(12/48) —— 变量隔离

use parley::{
    Alignment, AlignmentOptions, FontContext, FontFamily, GenericFamily, Layout,
    LayoutContext, LineHeight, PositionedLayoutItem, StyleProperty,
};

#[derive(Clone, Copy, Debug, PartialEq, Default)]
struct DummyBrush {
    color: [u8; 4],
}

fn run_test(label: &str, base_color: [u8; 4], subscript_color: [u8; 4]) {
    let text = "BaseSubscript";
    let base_range = 0..4usize;
    let subscript_range = 4..13usize;
    let font_name = "Liberation Sans";

    let mut font_cx = FontContext::new();
    let mut layout_cx = LayoutContext::new();

    let mut builder = layout_cx.ranged_builder(&mut font_cx, text, 1.0, true);
    builder.push_default(StyleProperty::Brush(DummyBrush { color: [0, 0, 0, 255] }));
    builder.push_default(GenericFamily::SystemUi);
    builder.push_default(LineHeight::FontSizeRelative(1.3));
    builder.push_default(StyleProperty::FontSize(12.0));

    // Base run
    builder.push(StyleProperty::FontFamily(FontFamily::Source(font_name.into())), base_range.clone());
    builder.push(StyleProperty::Brush(DummyBrush { color: base_color }), base_range.clone());
    builder.push(StyleProperty::FontSize(12.0), base_range.clone());

    // Subscript run
    builder.push(StyleProperty::FontFamily(FontFamily::Source(font_name.into())), subscript_range.clone());
    builder.push(StyleProperty::Brush(DummyBrush { color: subscript_color }), subscript_range.clone());
    builder.push(StyleProperty::FontSize(48.0), subscript_range.clone());

    let mut layout: Layout<DummyBrush> = builder.build(text);
    layout.break_all_lines(Some(2000.0));
    layout.align(Alignment::Start, AlignmentOptions::default());

    println!("=== {label} (base_color={base_color:?} subscript_color={subscript_color:?}) ===");
    let mut run_count = 0;
    for line in layout.lines() {
        for item in line.items() {
            if let PositionedLayoutItem::GlyphRun(glyph_run) = item {
                let run = glyph_run.run();
                let text_range = run.text_range();
                let font_size = run.font_size();
                println!("  GlyphRun #{run_count}: text_range={:?} font_size()={}", text_range, font_size);
                run_count += 1;
            }
        }
    }
    if run_count < 2 {
        println!("  ⚠️ 只有{run_count}个GlyphRun —— 发生了合并！");
    }
    println!();
}

fn main() {
    run_test("对照组A：颜色相同（模拟真实tdf99602）", [0, 0, 0, 255], [0, 0, 0, 255]);
    run_test("对照组B：颜色不同", [0, 0, 0, 255], [255, 0, 0, 255]);
}
