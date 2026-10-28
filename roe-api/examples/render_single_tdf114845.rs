// PROOT-002 验证：渲染 tdf114845_rotateShape.pptx 单个文件，输出PNG，
// 用于肉眼确认connector（直线，不含箭头）能否被渲染层正确画出。
// 用法：cargo run -p roe-api --example render_single_tdf114845 -- <output.png>

use roe_impress::slide_renderer::SlideRenderer;
use roe_parser::RoeDocument;

fn main() {
    let output_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/tdf114845_render.png".to_string());

    let input =
        "test_samples/curated/pptx/large__sd_data_pptx__tdf114845_rotateShape.pptx";

    let doc = RoeDocument::open_pptx_file(input).expect("failed to parse pptx");

    let pres = match doc {
        RoeDocument::Impress(p) => p,
        _ => panic!("expected Impress document"),
    };

    let slide = &pres.slides[0];
    let scale = 1.0_f32;
    let pw = (pres.slide_size.0 * scale) as u32;
    let ph = (pres.slide_size.1 * scale) as u32;

    let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];

    let mut renderer = SlideRenderer::new(pres.slide_size.0, pres.slide_size.1);

    renderer
        .render_slide(
            slide,
            &pres.styles,
            None,
            scale,
            &mut buf,
            &pres.theme_colors,
            Some(&pres.master_slides),
        )
        .expect("render_slide failed");

    let pixmap = tiny_skia::Pixmap::from_vec(
        buf,
        tiny_skia::IntSize::from_wh(pw, ph).expect("invalid image size"),
    )
    .expect("pixmap construction failed");

    let png = pixmap.encode_png().expect("failed to encode png");
    std::fs::write(&output_path, &png).expect("failed to save png");

    println!("rendered to {}", output_path);
}
