use roe_parser::RoeDocument;
use std::path::Path;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "test_samples/pptx".to_string());
    let mut found = 0;

    for entry in std::fs::read_dir(&dir).expect("read_dir failed") {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("pptx") {
            continue;
        }
        let doc = match RoeDocument::open_pptx_file(path.to_str().unwrap()) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if let RoeDocument::Impress(pres) = doc {
            for (si, slide) in pres.slides.iter().enumerate() {
                for s in &slide.shapes {
                    if let roe_model::common::ShapeGeometry::PresetPath(cmds) = &s.geometry {
                        let arc_count = cmds.iter().filter(|c| matches!(c, roe_model::common::PathCommand::ArcTo { .. })).count();
                        if arc_count > 0 {
                            println!("{} slide{} shape_id={} arcs={}", path.file_name().unwrap().to_string_lossy(), si, s.id, arc_count);
                            found += 1;
                            if found >= 15 { return; }
                        }
                    }
                }
            }
        }
    }
    println!("total found: {}", found);
}
