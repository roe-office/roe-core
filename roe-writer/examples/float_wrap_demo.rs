// 最小demo：验证 taffy::FloatContext + parley::BreakLines 能否实现
// "图片浮动在左侧，文字逐行收窄环绕"的效果。
// 参照 vendor/parley/parley_tests/tests/floats.rs 的 layout_floats() 模式，
// 去掉了InlineBox/CustomOutOfFlow机制，直接用固定的图片y范围模拟（简化版，
// 不需要图片本身作为inline box参与断行，因为ROE里图片是Frame，位置已知，
// 不需要taffy帮忙"排"图片位置，只需要taffy帮忙算"这一行该多宽"）。

use parley::{
    Alignment, AlignmentOptions, FontContext, GenericFamily, Layout,
    LayoutContext, LineHeight, StyleProperty, YieldData,
};
use taffy::{Clear, FloatContext, FloatDirection};

#[derive(Clone, Copy, Debug, PartialEq, Default)]
struct DummyBrush;

fn main() {
    let text = "在此处插入文本".repeat(30); // 制造一段足够长、会跨越图片高度的文字
    let full_width = 400.0f32;

    // 模拟一张图片：左侧浮动，宽192.7pt，高219.4pt（取自debug_manual_wrap.rs
    // 之前打印的手册.docx真实Frame尺寸，w_pt=192.7 h_pt=219.4）
    let img_width = 192.7f32;
    let img_height = 219.4f32;

    let mut font_cx = FontContext::new();
    let mut layout_cx = LayoutContext::new();

    let mut builder = layout_cx.ranged_builder(&mut font_cx, &text, 1.0, true);
    builder.push_default(StyleProperty::Brush(DummyBrush));
    builder.push_default(GenericFamily::SystemUi);
    builder.push_default(LineHeight::FontSizeRelative(1.3));
    builder.push_default(StyleProperty::FontSize(16.0));

    let mut layout: Layout<DummyBrush> = builder.build(&text);

    // === 核心：用taffy::FloatContext管理"图片占用的浮动区域"，
    // 每次断下一行前查询当前y坐标对应的可用宽度/起始x坐标 ===
    let mut float_context = FloatContext::new();
    float_context.set_width(full_width);

    // 提前把图片"放"进浮动上下文（不同于floats.rs用InlineBox在断行过程中
    // 触发放置，这里图片位置已知，直接place，模拟"排版开始前就已知道图片在哪"）
    let placed = float_context.place_floated_box(
        taffy::Size { width: img_width, height: img_height },
        0.0, // min_y：图片从段落顶部y=0开始占用
        [0.0, 0.0],
        FloatDirection::Left,
        Clear::None,
    );
    println!(
        "图片放置结果: x={:.1} y={:.1}",
        placed.x, placed.y
    );

    let initial_slot = float_context.find_content_slot(0.0, [0.0, 0.0], Clear::None, None);

    let mut breaker = layout.break_lines();
    {
        let state = breaker.state_mut();
        state.set_layout_max_advance(full_width);
        state.set_line_max_advance(initial_slot.width);
        state.set_line_x(initial_slot.x);
        state.set_line_y(initial_slot.y as f64);
    }

    let mut line_no = 0usize;
    while let Some(yield_data) = breaker.break_next() {
        let state = breaker.state_mut();
        let y = state.line_y();
        println!(
            "line#{:02} y={:6.1} max_advance={:6.1} line_x={:5.1}",
            line_no, y, state.line_max_advance(), state.line_x()
        );
        line_no += 1;

        if let YieldData::LineBreak(_) = yield_data {
            let next_slot = float_context.find_content_slot(y as f32, [0.0, 0.0], Clear::None, None);
            state.set_line_max_advance(next_slot.width);
            state.set_line_x(next_slot.x);
            state.set_line_y(next_slot.y as f64);
        }
    }
    breaker.finish();

    layout.align(Alignment::Start, AlignmentOptions::default());
    println!("总行数: {}", line_no);
    println!("验证完成：图片高度范围内的行 max_advance 应该 < {}", full_width);
}
