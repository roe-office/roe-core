use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = &args[1];
    let data = std::fs::read(path).expect("read failed");
    let doc = roe_parser::RoeDocument::open_xlsx_bytes(&data).expect("parse failed");
    let mut wb = match doc {
        roe_parser::RoeDocument::Calc(wb) => wb,
        _ => panic!("not calc"),
    };

    println!("=== BEFORE recalc (ALL cells, not just Formula) ===");
    for (si, sheet) in wb.sheets.iter().enumerate() {
        for (&col, column) in sheet.cells.cols.iter() {
            for block in &column.blocks {
                for (offset, value) in block.values.iter().enumerate() {
                    let row = block.start_row + offset as u32;
                    println!("sheet{} r{} c{}: {:?}", si, row, col, value);
                }
            }
        }
    }

    let _engine = roe_api::document::init_calc_engine_for_doc(&mut roe_parser::RoeDocument::Calc(wb.clone()));

    println!("=== calling init_calc_engine_for_doc via reopen ===");
    let data2 = std::fs::read(path).expect("read failed");
    let doc2 = roe_parser::RoeDocument::open_xlsx_bytes(&data2).expect("parse failed");
    let mut doc2 = doc2;
    let _engine2 = roe_api::document::init_calc_engine_for_doc(&mut doc2);
    if let roe_parser::RoeDocument::Calc(wb2) = doc2 {
        println!("=== AFTER recalc ===");
        for (si, sheet) in wb2.sheets.iter().enumerate() {
            for (&col, column) in sheet.cells.cols.iter() {
                for block in &column.blocks {
                    for (offset, value) in block.values.iter().enumerate() {
                        if let roe_model::calc::CellValue::Formula { source, cached } = value {
                            let row = block.start_row + offset as u32;
                            println!("sheet{} r{} c{}: formula={:?} cached={:?}", si, row, col, source, cached);
                        }
                    }
                }
            }
        }
    }
}
