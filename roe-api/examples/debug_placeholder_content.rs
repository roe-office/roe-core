use roe_parser::RoeDocument;
use roe_model::impress::ShapeContent;

fn main() {
    let path = std::env::args().nth(1).expect("usage: debug_placeholder_content <path.pptx>");
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
                    let text_desc = match &s.content {
                        ShapeContent::Text(paragraphs) => {
                            let combined: String = paragraphs.iter()
                                .flat_map(|p| p.runs.iter().map(|r| r.text.as_str()))
                                .collect::<Vec<_>>()
                                .join("");
                            format!("Text(len={}, content={:?})", combined.len(), combined.chars().take(50).collect::<String>())
                        }
                        ShapeContent::Empty => "Empty".to_string(),
                        ShapeContent::Table(_) => "Table".to_string(),
                        ShapeContent::Image(_) => "Image".to_string(),
                        ShapeContent::Chart(_) => "Chart".to_string(),
                        ShapeContent::Group(children) => format!("Group({} children)", children.len()),
                    };
                    println!("  shape_id={} {} content={}", s.id, ph_desc, text_desc);
                }
            }
        }
        _ => println!("not Impress document"),
    }
}
