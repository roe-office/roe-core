use roe_parser::ooxml::docx::DocxParser;
use roe_parser::ooxml::docx_writer::DocxWriter;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).expect("read input");
    let doc = DocxParser::parse(&data).expect("parse");
    let out = DocxWriter::write(&doc).expect("write");
    std::fs::write(&args[2], out).expect("write output");
    println!("写入 {}", args[2]);
}
