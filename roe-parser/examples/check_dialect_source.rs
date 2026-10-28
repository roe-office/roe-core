fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = &args[1];
    let data = std::fs::read(path).expect("read failed");
    let doc = roe_parser::RoeDocument::open_fods_bytes(&data).expect("parse failed");
    if let roe_parser::RoeDocument::Calc(wb) = doc {
        println!("formula_dialect_source: {:?}", wb.formula_dialect_source);
    }
}
