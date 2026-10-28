//! 临时诊断：直接从 wdoc.body 里找出所有携带 formula 的 Run，
//! 打印 latex / width_pt / height_pt / depth_pt 原始值（RaTeX 排版结果，
//! 未经 layout.rs 二次处理），用于核对 overbrace/underbrace 间距异常问题。
use roe_model::writer::BodyElement;

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let data = std::fs::read(&path).unwrap();
    let doc = roe_parser::RoeDocument::open_docx_bytes(&data).unwrap();
    let wdoc = match doc { roe_parser::RoeDocument::Writer(d) => d, _ => panic!("not writer doc") };

    for (bi, elem) in wdoc.body.iter().enumerate() {
        if let BodyElement::Paragraph(p) = elem {
            for (ri, run) in p.runs.iter().enumerate() {
                if let Some(f) = &run.formula {
                    println!(
                        "body[{}] run[{}]: latex={:?} width_pt={} height_pt={} depth_pt={}",
                        bi, ri, f.latex, f.width_pt, f.height_pt, f.depth_pt
                    );
                }
            }
        }
    }
}
