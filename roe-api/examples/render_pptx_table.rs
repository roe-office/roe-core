use roe_impress::slide_renderer::SlideRenderer;
use roe_parser::RoeDocument;

fn main() {
    let path = std::env::args().nth(1).expect("usage: render_pptx_table <path.pptx>");
    let doc = RoeDocument::open_pptx_file(&path).expect("parse pptx failed");

    match doc {
        RoeDocument::Impress(pres) => {
            let (sw, sh) = pres.slide_size;
            println!("slide_size: {:.1}x{:.1} pt, {} slides", sw, sh, pres.slides.len());

            let scale = 2.0f32; // 放大一点方便肉眼看清表格边框/文字
            let pw = (sw * scale) as u32;
            let ph = (sh * scale) as u32;

            let mut renderer = SlideRenderer::new(sw, sh);
            let mut any_table = false;

            for (i, slide) in pres.slides.iter().enumerate() {
                let table_shapes: Vec<_> = slide.shapes.iter()
                    .filter(|s| matches!(&s.content, roe_model::impress::ShapeContent::Table(_)))
                    .collect();

                if table_shapes.is_empty() {
                    continue;
                }
                any_table = true;

                println!("slide {}: {} shapes total, {} table shape(s)", i, slide.shapes.len(), table_shapes.len());
                for (ti, s) in table_shapes.iter().enumerate() {
                    if let roe_model::impress::ShapeContent::Table(t) = &s.content {
                        println!(
                            "  table[{}] shape_id={} pos=({:.1},{:.1},{:.1},{:.1}) rows={} cols={}",
                            ti, s.id, s.position.x(), s.position.y(), s.position.w(), s.position.h(),
                            t.rows.len(), t.col_widths.len()
                        );
                        for (ri, row) in t.rows.iter().enumerate() {
                            for (ci, cell) in row.cells.iter().enumerate() {
                                let text: String = cell.content.iter()
                                    .filter_map(|c| if let roe_model::common::CellContent::Paragraph(p) = c { Some(p) } else { None })
                                    .flat_map(|p| p.runs.iter().map(|r| r.text.as_str()))
                                    .collect();
                                println!(
                                    "    r{} c{}: col_span={} row_span={} hidden={} text={:?}",
                                    ri, ci, cell.col_span, cell.row_span, cell.hidden, text
                                );
                            }
                        }
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

            if !any_table {
                println!("(no slide with Table content found)");
            }
        }
        _ => println!("not Impress document"),
    }
}
