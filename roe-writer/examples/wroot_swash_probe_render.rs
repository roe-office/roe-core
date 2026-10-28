// WROOT-EMOJI-COLOR 最小复现程序 v3
// 最终判定测试：用tile.rs实测抓到的真实glyph_id，直接复刻失败场景

use std::fs;
use swash::scale::{Render, ScaleContext, Source};
use swash::FontRef;

fn main() {
    println!("swash最小复现程序v3 - 用真实glyph_id直接判定");

    let mut scale_ctx = ScaleContext::new();

    let data = fs::read("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc")
        .expect("读取ttc文件失败");
    println!("文件字节数: {}", data.len());

    let font_ref = FontRef::from_index(&data, 2).expect("FontRef构造失败");
    println!("FontRef构造成功, offset={}, key={:?}", font_ref.offset, font_ref.key);

    let mut scaler = scale_ctx
        .builder(font_ref)
        .size(24.0)  // 复刻实际font_size=24
        .hint(true)
        .normalized_coords(std::iter::empty::<i16>())  // 复刻空coords
        .build();
    println!("Scaler构建完成 (size=24.0, 空coords, hint=true)");

    // 用tile.rs实测抓到的真实glyph_id列表逐个测试
    let real_glyph_ids: &[u16] = &[10186, 10265, 10287, 10402, 10849, 10899, 10909, 10921];
    println!("\n开始逐个测试真实glyph_id:");
    for &gid in real_glyph_ids {
        let result = Render::new(&[Source::Outline]).render(&mut scaler, gid);
        match &result {
            Some(image) => println!(
                "  glyph_id={} [OK] 渲染成功: content={:?}, size={}x{}",
                gid, image.content, image.placement.width, image.placement.height
            ),
            None => println!("  glyph_id={} [失败] 渲染返回None", gid),
        }
    }

    // 对照：同样的font_ref，用之前验证过能成功的'中'字glyph_id=9544再测一次
    println!("\n对照：用之前验证成功的'中'字glyph_id=9544:");
    let charmap = FontRef::from_index(&data, 2).unwrap().charmap();
    let zhong_gid = charmap.map('中');
    println!("  charmap查到的'中'glyph_id = {}", zhong_gid);
    let result = Render::new(&[Source::Outline]).render(&mut scaler, zhong_gid);
    match &result {
        Some(image) => println!(
            "  [OK] 渲染成功: content={:?}, size={}x{}",
            image.content, image.placement.width, image.placement.height
        ),
        None => println!("  [失败] 渲染返回None"),
    }

    println!("\n========== 判定结束 ==========");
    println!("如果真实glyph_id全部失败但'中'字(charmap查到的)成功 -> glyph_id本身有问题");
    println!("如果全部都成功 -> 说明用size=24+空coords也没问题，需要回头对比其他被忽略的细节");
}
