//! 验证 line_shape 在连续调用场景下的性能（模拟拖拽光标）。
//! 运行：cargo bench -p roe-writer

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use roe_writer::font_registry::FontRegistry;
use roe_writer::line_shape::{char_offset_to_x, x_to_char_offset};

fn bench_char_offset_to_x(c: &mut Criterion) {
    let mut reg = FontRegistry::new_with_system_fonts();
    let text = "Hello World，这是一段中英混排的测试文字。";
    let font_size = 12.0f32;

    c.bench_function("char_offset_to_x_per_char", |b| {
        b.iter(|| {
            for (i, _) in text.char_indices() {
                black_box(char_offset_to_x(
                    &mut reg,
                    black_box(text),
                    font_size,
                    false,
                    false,
                    i,
                ));
            }
        })
    });
}

fn bench_x_to_char_offset(c: &mut Criterion) {
    let mut reg = FontRegistry::new_with_system_fonts();
    let text = "Hello World，这是一段中英混排的测试文字。";
    let font_size = 12.0f32;

    // 模拟拖拽：在行宽范围内均匀采样 50 个 x 坐标
    c.bench_function("x_to_char_offset_drag_50pts", |b| {
        b.iter(|| {
            for i in 0..50 {
                let x = i as f32 * 4.0; // 0, 4, 8 ... 196 pt
                black_box(x_to_char_offset(
                    &mut reg,
                    black_box(text),
                    font_size,
                    false,
                    false,
                    x,
                ));
            }
        })
    });
}

criterion_group!(benches, bench_char_offset_to_x, bench_x_to_char_offset);
criterion_main!(benches);
