use roe_parser::RoeDocument;
use roe_model::writer::BodyElement;
use roe_writer::layout::{ParagraphLayoutEngine, arena_to_layout_pages};
use roe_writer::FontRegistry;

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let data = std::fs::read(&path).unwrap();
    let doc = RoeDocument::open_docx_bytes(&data).unwrap();
    let wdoc = match doc { RoeDocument::Writer(d) => d, _ => panic!("not a writer doc") };

    println!("=== body 元素扫描 ===");
    for (i, elem) in wdoc.body.iter().enumerate() {
        if let BodyElement::Shape(s) = elem {
            println!("[{}] Shape: x_pt={} y_pt={} width_pt={} height_pt={} anchor_h={:?} anchor_v={:?} wrap={:?}",
                i, s.x_pt, s.y_pt, s.width_pt, s.height_pt, s.anchor_type_h, s.anchor_type_v, s.wrap_type);
            match &s.geometry {
                roe_model::common::ShapeGeometry::PresetPath(cmds) => {
                    println!("    geometry=PresetPath, {} 个PathCommand，前5个: {:?}",
                        cmds.len(), &cmds[..cmds.len().min(5)]);
                }
                other => println!("    geometry={:?}", other),
            }
        }
    }

    println!("\n=== 排版结果 ===");
    let registry = FontRegistry::new_with_system_fonts();
    let mut engine = ParagraphLayoutEngine::new(registry);
    let result = engine.layout(&wdoc);
    let pages = arena_to_layout_pages(&result.arena, &wdoc);

    for (pi, page) in pages.iter().enumerate() {
        println!("--- page {} : {} 个shape ---", pi, page.shapes.len());
        for (si, shape) in page.shapes.iter().enumerate() {
            println!("  [{}] x_on_page={:.2} y_on_page={:.2} width_pt={:.2} height_pt={:.2}",
                si, shape.x_on_page, shape.y_on_page, shape.width_pt, shape.height_pt);
        }
    }
}
