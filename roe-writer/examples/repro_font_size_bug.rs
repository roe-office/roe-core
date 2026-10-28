// 最小复现：验证"同一个 ranged_builder 内 push 多个不同 FontSize range"
// 是否会导致 shaping 之后 GlyphRun.run().font_size() 被错误统一。
// 完全绕开 docx 解析/roe-parser/roe-model/FontRegistry，只用 Parley
// 公开 API，模拟 layout.rs 里 tdf99602 场景的真实调用序列：
//   push_default(FontSize=12) -> push(FontSize=12, range0) -> push(FontSize=48, range1)
//   -> build() -> 遍历 PositionedLayoutItem -> 打印每个 GlyphRun 的 font_size()
//
// 参照 float_wrap_demo.rs 的初始化套路（不用 FontRegistry，直接裸建
// FontContext/LayoutContext）。

use parley::{
    Alignment, AlignmentOptions, FontContext, GenericFamily, Layout,
    LayoutContext, LineHeight, PositionedLayoutItem, StyleProperty,
};

#[derive(Clone, Copy, Debug, PartialEq, Default)]
struct DummyBrush;

fn main() {
    // 模拟 tdf99602：一行内 "Base" 是默认12pt，"Subscript" 是字符样式
    // 引用的48pt，两个run前后相邻，属于同一段落同一行同一次build()。
    let text = "BaseSubscript";
    let base_range = 0..4;       // "Base"
    let subscript_range = 4..13; // "Subscript"

    let mut font_cx = FontContext::new();
    let mut layout_cx = LayoutContext::new();

    let mut builder = layout_cx.ranged_builder(&mut font_cx, text, 1.0, true);
    builder.push_default(StyleProperty::Brush(DummyBrush));
    builder.push_default(GenericFamily::SystemUi);
    builder.push_default(LineHeight::FontSizeRelative(1.3));
    builder.push_default(StyleProperty::FontSize(12.0));

    // 关键：显式给两个不同字节range push不同FontSize，模拟layout.rs
    // 2019行 `builder.push(StyleProperty::FontSize(rs.font_size), range.clone())`
    builder.push(StyleProperty::FontSize(12.0), base_range.clone());
    builder.push(StyleProperty::FontSize(48.0), subscript_range.clone());

    let mut layout: Layout<DummyBrush> = builder.build(text);
    layout.break_all_lines(Some(2000.0)); // 给足够宽度，保证不会意外换行
    layout.align(Alignment::Start, AlignmentOptions::default());

    println!("=== 复现测试：期望 Base=12.0, Subscript=48.0 ===");
    println!("文本: {:?}", text);
    println!("base_range={:?} subscript_range={:?}", base_range, subscript_range);
    println!();

    let mut run_count = 0;
    for line in layout.lines() {
        for item in line.items() {
            if let PositionedLayoutItem::GlyphRun(glyph_run) = item {
                let run = glyph_run.run();
                let text_range = run.text_range();
                let font_size = run.font_size();
                let slice = &text[text_range.clone()];
                println!(
                    "GlyphRun #{run_count}: text_range={:?} text={:?} font_size()={}",
                    text_range, slice, font_size
                );
                run_count += 1;
            }
        }
    }

    println!();
    println!("=== 结论 ===");
    if run_count < 2 {
        println!("⚠️ 只检测到 {run_count} 个 GlyphRun，期望至少2个（Base/Subscript各一个）。");
        println!("   可能两个run被shaping合并成了一个GlyphRun，这本身可能就是线索。");
    } else {
        println!("检测到 {run_count} 个 GlyphRun，请对照上面打印的 font_size() 逐一核对：");
        println!("如果所有GlyphRun的font_size()都相同（不管是12.0/48.0/还是别的值），");
        println!("说明复现成功——bug确实源于'同一builder内多个不同FontSize range'这个模式。");
        println!("如果每个GlyphRun的font_size()与其对应range本该有的字号一致，");
        println!("说明这个最小场景没有复现，问题可能与docx解析链路上的其他因素有关");
        println!("（比如字符样式的Brush/FontFamily push顺序、或者rs.color的push time等）。");
    }
}
