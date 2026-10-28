// 复现v2：在v1基础上，精确模拟layout.rs真实的push顺序——
// 发现真实代码对同一个range，FontFamily被push了两次（不同写法），
// 且第二次FontFamily push排在FontSize push之后，怀疑这种"交错push"
// 顺序触发了RangedBuilder内部区间合并的边界条件bug。
//
// 真实push顺序（照抄layout.rs 1970-2024行）：
//   for each run:
//     1. push(FontFamily::Source(name), range)      <- 第一次FontFamily
//     2. push(Brush(color), range)
//     3. push(FontSize(size), range)
//     4. push(FontFamily::named(name), range)        <- 第二次FontFamily（重复！）
//     5. push(FontWeight/FontStyle if needed, range)

use parley::{
    Alignment, AlignmentOptions, FontContext, FontFamily, GenericFamily, Layout,
    LayoutContext, LineHeight, PositionedLayoutItem, StyleProperty,
};

#[derive(Clone, Copy, Debug, PartialEq, Default)]
struct DummyBrush {
    color: [u8; 4],
}

fn push_run_styles_like_real_code(
    builder: &mut parley::RangedBuilder<'_, DummyBrush>,
    range: std::ops::Range<usize>,
    font_size: f32,
    family_name: &str,
    color: [u8; 4],
) {
    // 第一次 FontFamily push（WROOT-FONTFAMILY修复，用Source写法）
    builder.push(
        StyleProperty::FontFamily(FontFamily::Source(family_name.into())),
        range.clone(),
    );
    // Brush
    builder.push(StyleProperty::Brush(DummyBrush { color }), range.clone());
    // FontSize
    builder.push(StyleProperty::FontSize(font_size), range.clone());
    // 第二次 FontFamily push（旧代码残留，named写法，重复push同一range！）
    builder.push(
        StyleProperty::FontFamily(FontFamily::named(family_name)),
        range.clone(),
    );
}

fn main() {
    let text = "BaseSubscript";
    let base_range = 0..4usize;
    let subscript_range = 4..13usize;
    let font_name = "Times New Roman"; // 真实docx docDefaults指定的字体

    let mut font_cx = FontContext::new();
    let mut layout_cx = LayoutContext::new();

    let mut builder = layout_cx.ranged_builder(&mut font_cx, text, 1.0, true);
    builder.push_default(StyleProperty::Brush(DummyBrush { color: [0, 0, 0, 255] }));
    builder.push_default(GenericFamily::SystemUi);
    builder.push_default(LineHeight::FontSizeRelative(1.3));
    builder.push_default(StyleProperty::FontSize(12.0));

    // "Base" run：12pt
    push_run_styles_like_real_code(&mut builder, base_range.clone(), 12.0, font_name, [0, 0, 0, 255]);
    // "Subscript" run：48pt（引用字符样式CharSize48）
    push_run_styles_like_real_code(&mut builder, subscript_range.clone(), 48.0, font_name, [0, 0, 0, 255]);

    let mut layout: Layout<DummyBrush> = builder.build(text);
    layout.break_all_lines(Some(2000.0));
    layout.align(Alignment::Start, AlignmentOptions::default());

    println!("=== 复现测试v2：模拟真实的重复/交错FontFamily push ===");
    println!("期望 Base=12.0, Subscript=48.0");
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
    if run_count >= 2 {
        println!("=== 若两个font_size()相同，说明'重复+交错push FontFamily'就是触发条件 ===");
    } else {
        println!("⚠️ 只检测到{run_count}个GlyphRun，本身可能就是线索（两个run被合并了）");
    }
}
