use roe_parser::RoeDocument;
use roe_model::impress::ShapeContent;

fn main() {
    let path = std::env::args().nth(1).expect("usage: debug_bullet_info <path.pptx>");
    let doc = RoeDocument::open_pptx_file(&path).expect("parse pptx failed");

    match doc {
        RoeDocument::Impress(pres) => {
            for (i, slide) in pres.slides.iter().enumerate() {
                println!("=== slide {} ({} shapes) ===", i, slide.shapes.len());
                for s in &slide.shapes {
                    let ph_desc = match &s.placeholder {
                        Some(p) => format!("placeholder(type={:?}, idx={:?})", p.ph_type, p.index),
                        None => "not-placeholder".to_string(),
                    };
                    if let ShapeContent::Text(paragraphs) = &s.content {
                        println!("  shape_id={} {}", s.id, ph_desc);
                        for (pi, p) in paragraphs.iter().enumerate() {
                            let text: String = p.runs.iter().map(|r| r.text.as_str()).collect();
                            println!(
                                "    para[{}] text={:?} bullet={:?}",
                                pi,
                                text.chars().take(30).collect::<String>(),
                                p.bullet
                            );
                        }
                    }
                }
            }
        }
        _ => println!("not Impress document"),
    }
}
