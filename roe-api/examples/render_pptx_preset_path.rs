use roe_impress::slide_renderer::SlideRenderer;
use roe_parser::RoeDocument;

fn main() {
    let path = std::env::args().nth(1).expect("usage: render_pptx_preset_path <path.pptx>");
    let doc = RoeDocument::open_pptx_file(&path).expect("parse pptx failed");

    match doc {
        RoeDocument::Impress(pres) => {
            let (sw, sh) = pres.slide_size;
            println!("slide_size: {:.1}x{:.1} pt, {} slides", sw, sh, pres.slides.len());

            let scale = 2.0f32;
            let pw = (sw * scale) as u32;
            let ph = (sh * scale) as u32;

            let mut renderer = SlideRenderer::new(sw, sh);
            let mut any_preset = false;

            for (i, slide) in pres.slides.iter().enumerate() {
                let preset_shapes: Vec<_> = slide.shapes.iter()
                    .filter(|s| matches!(&s.geometry, roe_model::common::ShapeGeometry::PresetPath(_)))
                    .collect();

                if preset_shapes.is_empty() {
                    continue;
                }
                any_preset = true;

                println!("slide {}: {} shapes total, {} PresetPath shape(s)", i, slide.shapes.len(), preset_shapes.len());
                for s in &preset_shapes {
                    if let roe_model::common::ShapeGeometry::PresetPath(cmds) = &s.geometry {
                        let arc_count = cmds.iter().filter(|c| matches!(c, roe_model::common::PathCommand::ArcTo { .. })).count();
                        println!(
                            "  shape_id={} pos=({:.1},{:.1},{:.1},{:.1}) cmds={} arcs={}",
                            s.id, s.position.x(), s.position.y(), s.position.w(), s.position.h(),
                            cmds.len(), arc_count
                        );
                    }
                }

                let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];
                renderer.render_slide(
                    slide, &pres.styles, None, scale, &mut buf,
                    &pres.theme_colors, Some(&pres.master_slides)
                ).expect("render_slide failed");

                let pixmap = tiny_skia::Pixmap::from_vec(
                    buf, tiny_skia::IntSize::from_wh(pw, ph).unwrap()
                ).unwrap();
                let png = pixmap.encode_png().unwrap();

                let stem = std::path::Path::new(&path).file_stem().unwrap().to_string_lossy();
                let out = format!("/tmp/{}_slide{}.png", stem, i);
                std::fs::write(&out, &png).unwrap();
                println!("  → {} ({} bytes)", out, png.len());
            }

            if !any_preset {
                println!("(no slide with PresetPath shape found)");
            }
        }
        _ => println!("not Impress document"),
    }
}
