fn main() {
    let path = std::env::args().nth(1).unwrap_or("/home/yong/candy.odp".to_string());
    let data = std::fs::read(&path).expect("read file");
    match roe_parser::RoeDocument::open_odf_bytes(&data) {
        Ok(doc) => println!("OK, type={}", match doc {
            roe_parser::RoeDocument::Impress(p) => format!("Impress, {} slides", p.slides.len()),
            roe_parser::RoeDocument::Calc(w) => format!("Calc, {} sheets", w.sheets.len()),
            roe_parser::RoeDocument::Writer(d) => format!("Writer, {} paras", d.body.len()),
        }),
        Err(e) => println!("ERR: {:?}", e),
    }
}
