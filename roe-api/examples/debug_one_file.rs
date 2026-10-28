//! 调试单个文件的panic，不catch_unwind，直接暴露完整backtrace。
//! 用法: RUST_BACKTRACE=full cargo run -p roe-api --example debug_one_file -- <文件路径>

fn main() {
    let path = std::env::args().nth(1).expect("usage: debug_one_file <path>");
    let data = std::fs::read(&path).expect("read file");
    let ext = std::path::Path::new(&path)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    println!("opening {} (ext={})", path, ext);

    let result = match ext.as_str() {
        "docx" | "docm" | "dotx" | "dotm" => roe_parser::RoeDocument::open_docx_bytes(&data),
        "xlsx" | "xlsm" | "xlsb" => roe_parser::RoeDocument::open_xlsx_bytes(&data),
        "pptx" | "pptm" | "ppsx" | "potx" => roe_parser::RoeDocument::open_pptx_bytes(&data),
        "odt" | "ott" | "sxw" | "odm" => roe_parser::RoeDocument::open_odf_bytes(&data),
        "fodt" => roe_parser::RoeDocument::open_fodt_bytes(&data),
        "ods" => roe_parser::RoeDocument::open_odf_bytes(&data),
        "fods" => roe_parser::RoeDocument::open_fods_bytes(&data),
        "odp" => roe_parser::RoeDocument::open_odf_bytes(&data),
        "fodp" => roe_parser::RoeDocument::open_fodp_bytes(&data),
        "csv" => roe_parser::RoeDocument::open_csv_bytes(&data, None),
        other => panic!("unknown ext: {other}"),
    };

    match result {
        Ok(_) => println!("OK: parsed successfully"),
        Err(e) => println!("PARSE ERROR: {e}"),
    }
}
