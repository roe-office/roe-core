// ROE-TILE-001 临时验证脚本：渲染单个pptx第一张slide输出PNG，用于人工核对
// blipFill tile 平铺背景渲染效果。验证完成后可删除，不是正式功能代码。
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input = args.get(1).expect("usage: verify_tile <pptx_path> <output_png>");
    let output = args.get(2).expect("usage: verify_tile <pptx_path> <output_png>");

    let bytes = std::fs::read(input).expect("read pptx");
    let presentation = roe_parser::ooxml::pptx::PptxParser::parse(&bytes).expect("parse pptx");

    let (slide_w, slide_h) = presentation.slide_size;
    let scale = 2.0f32;
    let pw = (slide_w * scale) as u32;
    let ph = (slide_h * scale) as u32;

    let font_context = parley::FontContext::new();
    let mut renderer = roe_impress::slide_renderer::SlideRenderer::new_with_fonts(slide_w, slide_h, font_context);

    let mut buf = vec![0u8; (pw * ph * 4) as usize];
    let slide = &presentation.slides[0];
    renderer.render_slide(
        slide,
        &presentation.styles,
        None,
        scale,
        &mut buf,
        &presentation.theme_colors,
        Some(&presentation.master_slides),
    ).expect("render_slide");

    let mut img = image::RgbaImage::new(pw, ph);
    for (i, px) in img.pixels_mut().enumerate() {
        let off = i * 4;
        let a = buf[off+3] as u32;
        let unmul = |c: u8| -> u8 {
            if a == 0 { 0 } else { ((c as u32 * 255) / a).min(255) as u8 }
        };
        *px = image::Rgba([unmul(buf[off]), unmul(buf[off+1]), unmul(buf[off+2]), buf[off+3]]);
    }
    img.save(Path::new(output)).expect("save png");
    println!("saved to {}", output);
}
