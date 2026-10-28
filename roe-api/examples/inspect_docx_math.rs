//! 诊断脚本：检查docx文档里的OMML数学公式解析结果（latex/尺寸/display_list）。
//! 跟inspect_formulas.rs（xlsx）/inspect_formula_any.rs（xlsx/ods/fods）
//! 是不同用途——那两个是电子表格公式，这个是数学公式(m:oMath)。
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = &args[1];
    let data = std::fs::read(path).expect("read failed");
    let doc = roe_parser::RoeDocument::open_docx_bytes(&data).expect("parse failed");
    let wdoc = match doc {
        roe_parser::RoeDocument::Writer(d) => d,
        _ => panic!("not a Writer document"),
    };

    let mut count = 0;
    for elem in &wdoc.body {
        if let roe_model::writer::BodyElement::Paragraph(p) = elem {
            for run in &p.runs {
                if let Some(f) = &run.formula {
                    count += 1;
                    println!("--- formula #{} ---", count);
                    println!("latex: {:?}", f.latex);
                    println!("width_pt={} height_pt={} depth_pt={}", f.width_pt, f.height_pt, f.depth_pt);
                    println!("display_list.width={} height={} depth={}",
                        f.display_list.width, f.display_list.height, f.display_list.depth);
                    println!("item count: {}", f.display_list.items.len());
                    for (i, item) in f.display_list.items.iter().enumerate() {
                        println!("  [{}] {:?}", i, item);
                    }
                }
            }
        }
    }
    println!("=== total formulas found: {} ===", count);
    let _ = Path::new(path);
}
