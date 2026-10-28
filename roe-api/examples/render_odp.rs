use roe_impress::slide_renderer::SlideRenderer;

fn main() {
    let path = std::env::args().nth(1).unwrap_or("/home/yong/candy.odp".to_string());
    let data = std::fs::read(&path).expect("read file");
    let doc = roe_parser::RoeDocument::open_odf_bytes(&data).expect("parse");

    match doc {
        roe_parser::RoeDocument::Impress(pres) => {
            let (sw, sh) = pres.slide_size;
            println!("slide_size: {:.1}x{:.1} pt, {} slides", sw, sh, pres.slides.len());

            let scale = 1.0f32;
            let pw = (sw * scale) as u32;
            let ph = (sh * scale) as u32;
            let mut buf = vec![0u8; (pw * ph * 4) as usize];

            let mut renderer = SlideRenderer::new(sw, sh);
            for (i, slide) in pres.slides.iter().enumerate().take(3) {
                println!("slide {}: {} shapes", i, slide.shapes.len());
                renderer.render_slide(slide, &pres.styles, None, scale, &mut buf, &pres.theme_colors, Some(&pres.master_slides)).expect("render");

                let pixmap = tiny_skia::Pixmap::from_vec(
                    buf.clone(),
                    tiny_skia::IntSize::from_wh(pw, ph).unwrap(),
                ).unwrap();
                let png = pixmap.encode_png().unwrap();
                let out = format!("/tmp/candy_slide{}.png", i);
                std::fs::write(&out, &png).unwrap();
                println!("  → {} ({} bytes)", out, png.len());
            }
        }
        _ => println!("not Impress"),
    }
}
