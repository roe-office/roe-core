// 方案A PoC：验证 parley 在 U+00AD 处是否断行、断行后是否补画连字符
// 跑法：cargo run --example test_soft_hyphen -p roe-writer

use parley::{Alignment, AlignmentOptions, FontContext, LayoutContext, StyleProperty, GenericFamily, LineHeight};

fn main() {
    let syllables: Vec<&str> = hypher::hyphenate("different", hypher::Lang::English).collect();
    let word = syllables.join("\u{00AD}");
    // 前后加点其他文字，模拟真实场景：一个长单词卡在行尾
    let text = format!("This is a very {} word test here", word);
    println!("测试文本: {:?}", text);

    let mut font_cx = FontContext::new();
    let mut layout_cx: LayoutContext<[u8; 4]> = LayoutContext::new();

    let mut builder = layout_cx.ranged_builder(&mut font_cx, &text, 1.0, true);
    builder.push_default(StyleProperty::Brush([0, 0, 0, 255]));
    builder.push_default(GenericFamily::SystemUi);
    builder.push_default(LineHeight::Absolute(20.0));
    builder.push_default(StyleProperty::FontSize(16.0));

    let mut layout: parley::Layout<[u8; 4]> = builder.build(&text);

    // 故意设一个很窄的宽度，逼迫它必须在长单词内部断行
    // 先用一个明显足够宽的宽度看正常效果，再逐步收窄
    for width in [500.0f32, 150.0, 100.0, 80.0, 60.0] {
        let mut layout: parley::Layout<[u8; 4]> = builder_clone(&mut layout_cx, &mut font_cx, &text);
        layout.break_all_lines(Some(width));
        layout.align(Alignment::Start, AlignmentOptions::default());

        println!("\n--- width={} ---", width);
        for line in layout.lines() {
            let range = line.text_range();
            let line_text = &text[range.clone()];
            println!("  行内容(字节范围{:?}): {:?}", range, line_text);
        }
    }
}

fn builder_clone<'a>(
    layout_cx: &'a mut LayoutContext<[u8; 4]>,
    font_cx: &'a mut FontContext,
    text: &'a str,
) -> parley::Layout<[u8; 4]> {
    let mut builder = layout_cx.ranged_builder(font_cx, text, 1.0, true);
    builder.push_default(StyleProperty::Brush([0, 0, 0, 255]));
    builder.push_default(GenericFamily::SystemUi);
    builder.push_default(LineHeight::Absolute(20.0));
    builder.push_default(StyleProperty::FontSize(16.0));
    builder.build(text)
}
