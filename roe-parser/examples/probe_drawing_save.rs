use roe_parser::ooxml::xlsx::XlsxParser;
use roe_parser::ooxml::xlsx_writer::XlsxWriter;
fn main() {
    let src = format!("{}/../test_samples/xlsx/sc_data_xlsx__image_hyperlink.xlsx", env!("CARGO_MANIFEST_DIR"));
    let wb = XlsxParser::parse(&std::fs::read(src).unwrap()).unwrap();
    std::fs::write("/tmp/drawing_probe.xlsx", XlsxWriter::write(&wb).unwrap()).unwrap();
    println!("已写出 /tmp/drawing_probe.xlsx");
}
