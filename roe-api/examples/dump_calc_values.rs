//! 升级对比用:打开一个 xlsx,全量重算,按 (sheet,col,row) 排序输出所有非空格的值。
//! 用法: dump_calc_values <file>   输出到 stdout;耗时输出到 stderr。
use roe_model::calc::CellValue;

fn fmt(v: &CellValue) -> String {
    match v {
        CellValue::Empty => String::new(),
        CellValue::Number(n) => format!("N:{:.10e}", n),
        CellValue::Text(s) => format!("T:{:?}", s),
        CellValue::Bool(b) => format!("B:{}", b),
        CellValue::Error(e) => format!("E:{:?}", e),
        CellValue::Formula { source, cached } => format!("F:{:?} => {}", source, fmt(cached)),
        other => format!("O:{:?}", other),
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: dump_calc_values <file>");
    std::panic::set_hook(Box::new(|_| {}));
    let t = std::time::Instant::now();
    let r = std::panic::catch_unwind(|| {
        let data = std::fs::read(&path).map_err(|e| format!("READ_ERR {e}"))?;
        let mut doc = roe_parser::RoeDocument::open_xlsx_bytes(&data)
            .map_err(|e| format!("OPEN_ERR {e:?}"))?;
        let _engine = roe_api::document::init_calc_engine_for_doc(&mut doc);
        let wb = match doc {
            roe_parser::RoeDocument::Calc(wb) => wb,
            _ => return Err("NOT_CALC".to_string()),
        };
        let mut rows: Vec<(usize, u32, u32, String)> = Vec::new();
        for (si, sheet) in wb.sheets.iter().enumerate() {
            for (&col, column) in sheet.cells.cols.iter() {
                for block in &column.blocks {
                    for (off, v) in block.values.iter().enumerate() {
                        let s = fmt(v);
                        if !s.is_empty() {
                            rows.push((si, col, block.start_row + off as u32, s));
                        }
                    }
                }
            }
        }
        rows.sort();
        Ok(rows)
    });
    match r {
        Ok(Ok(rows)) => {
            for (si, col, row, s) in rows {
                println!("{}|{}|{}|{}", si, row, col, s);
            }
        }
        Ok(Err(e)) => println!("{}", e),
        Err(_) => println!("PANIC"),
    }
    eprintln!("{:?}", t.elapsed());
}
