// WROOT-EMOJI-COLOR 最小复现程序
// 目的：完全绕开 FontData/parley 包装，只用 swash 0.2.10 + 裸字体文件路径，
// 验证 swash 的 FontRef 构造 + Render.render() 本身能否在这台机器上正常工作。
// 用法（在 ~/Projects/roe 下）：
//   cp /tmp/wroot_swash_probe_render.rs roe-writer/examples/wroot_swash_probe_render.rs
//   cargo run --example wroot_swash_probe_render -p roe-writer

use std::fs;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::{CacheKey, FontRef};

struct TestFont {
    label: &'static str,
    path: &'static str,
    // 用于查 glyph id 的测试字符
    probe_char: char,
}

fn probe(scale_ctx: &mut ScaleContext, tf: &TestFont) {
    println!("\n========== 测试字体: {} ({}) ==========", tf.label, tf.path);

    let data = match fs::read(tf.path) {
        Ok(d) => d,
        Err(e) => {
            println!("  [失败] 读取文件失败: {e}");
            return;
        }
    };
    println!("  文件读取成功, 字节数 = {}", data.len());

    // 步骤1: 构造 FontRef
    let font_ref = match FontRef::from_index(&data, 0) {
        Some(f) => {
            println!("  [OK] FontRef::from_index 返回 Some");
            f
        }
        None => {
            println!("  [失败] FontRef::from_index 返回 None，构造阶段就失败了");
            return;
        }
    };

    // 打印 FontRef 内部关键信息，验证它不是"表面Some、内部空"
    println!(
        "  FontRef信息: offset={}, key={:?}",
        font_ref.offset,
        font_ref.key
    );
    let charmap = font_ref.charmap();
    let glyph_id = charmap.map(tf.probe_char);
    println!(
        "  探测字符 '{}' (U+{:04X}) -> glyph_id = {}",
        tf.probe_char, tf.probe_char as u32, glyph_id
    );
    if glyph_id == 0 {
        println!("  [警告] glyph_id=0 说明这个字体的charmap里没有映射到这个字符，探测字符选得不对，结果不可信");
        return;
    }

    // 步骤2: 用 ScaleContext 建立 Scaler
    let mut scaler = scale_ctx
        .builder(font_ref)
        .size(64.0)
        .hint(true)
        .build();
    println!("  [OK] Scaler 构建完成 (size=64.0)");

    // 步骤3a: 只尝试黑白轮廓 (Outline)，作为对照组
    let outline_result = Render::new(&[Source::Outline])
        .render(&mut scaler, glyph_id);
    match &outline_result {
        Some(image) => println!(
            "  [OK] Outline渲染成功: content={:?}, size={}x{}",
            image.content, image.placement.width, image.placement.height
        ),
        None => println!("  [失败] Outline渲染返回None（连黑白轮廓都渲染不出来，说明问题在更底层）"),
    }

    // 步骤3b: 尝试彩色位图 (ColorBitmap) —— NotoColorEmoji走这条路径
    let color_result = Render::new(&[
        Source::ColorOutline(0),
        Source::ColorBitmap(StrikeWith::BestFit),
    ])
    .render(&mut scaler, glyph_id);
    match &color_result {
        Some(image) => println!(
            "  [OK] 彩色渲染成功: content={:?}, size={}x{}, data字节数={}",
            image.content,
            image.placement.width,
            image.placement.height,
            image.data.len()
        ),
        None => println!("  [失败] 彩色渲染返回None"),
    }
}

fn main() {
    println!("swash最小复现程序 - WROOT-EMOJI-COLOR排查");
    println!("swash版本应为0.2.10（见Cargo.lock）");

    let mut scale_ctx = ScaleContext::new();

    // 对照组1: 普通黑白字体，用一个基本拉丁字母测试
    probe(
        &mut scale_ctx,
        &TestFont {
            label: "DejaVu Sans (黑白对照)",
            path: "/usr/share/fonts/TTF/DejaVuSans.ttf",
            probe_char: 'A',
        },
    );

    // 测试组: NotoColorEmoji，CBDT/CBLC格式
    // U+1F310 是文档里提到的🌐 (globe with meridians)
    probe(
        &mut scale_ctx,
        &TestFont {
            label: "Noto Color Emoji (CBDT/CBLC彩色)",
            path: "/usr/share/fonts/noto/NotoColorEmoji.ttf",
            probe_char: '\u{1F310}',
        },
    );

    println!("\n========== 复现程序结束 ==========");
    println!("解读提示：");
    println!("- 如果 DejaVu 的 Outline 渲染也返回 None -> swash本身/这个版本在这台机器上有基础问题，跟FontData无关");
    println!("- 如果 DejaVu 的 Outline 渲染是 Some，但 NotoColorEmoji 的彩色渲染是 None -> 需要看具体是ColorBitmap这个Source本身不工作，还是这个字体的strike数据有问题");
    println!("- 如果两者都成功 -> 说明swash本身完全没问题，问题100%在roe-writer里FontData->FontRef的转换那一步，下次直接去查font.font.data.as_ref()");
}
