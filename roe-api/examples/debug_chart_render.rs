// PROOT-033 探针：单独渲染chart-theme-override样本，眼见为实确认
// "纯色矩形"现象。
use roe_impress::slide_renderer::SlideRenderer;
use roe_parser::RoeDocument;

fn main() {
    let path = "/home/yong/Projects/roe/test_samples/curated/pptx/small__oox_unit_data__chart-theme-override.pptx";
    let doc = RoeDocument::open_pptx_file(path).expect("parse pptx");
    let pres = match doc {
        RoeDocument::Impress(p) => p,
        _ => panic!("not an Impress document"),
    };

    let (sw, sh) = pres.slide_size;
    println!("slide_size: {:.1}x{:.1} pt, {} slides", sw, sh, pres.slides.len());

    let scale = 2.0f32;
    let pw = (sw * scale) as u32;
    let ph = (sh * scale) as u32;

    let mut renderer = SlideRenderer::new(sw, sh);

    for (i, slide) in pres.slides.iter().enumerate() {
        let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];
        match renderer.render_slide(
            slide, &pres.styles, None, scale, &mut buf,
            &pres.theme_colors, Some(&pres.master_slides)
        ) {
            Ok((_actual_w, _actual_h)) => {
                let pixmap = tiny_skia::Pixmap::from_vec(
                    buf, tiny_skia::IntSize::from_wh(pw, ph).unwrap()
                ).expect("pixmap construction failed");
                let png = pixmap.encode_png().unwrap();
                let out_path = format!("/tmp/roe_chart_debug_slide{}.png", i);
                std::fs::write(&out_path, &png).unwrap();
                println!("slide {} -> {} ({}x{})", i, out_path, pw, ph);
            }
            Err(e) => {
                println!("slide {}: render_slide failed: {:?}", i, e);
            }
        }
    }
}
