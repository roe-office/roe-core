//! 诊断脚本：打印公式所在行/段落的排版几何信息（y_on_page, line的y_offset,
//! x_offset等），用于跟GlyphPath的绝对坐标核对，校准tile_formula.rs里的
//! 坐标换算。
use roe_writer::layout::ParagraphLayoutEngine;
use roe_writer::FontRegistry;
use roe_writer::frame::FrameContent;

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let data = std::fs::read(&path).unwrap();
    let doc = roe_parser::RoeDocument::open_docx_bytes(&data).unwrap();
    let wdoc = match doc { roe_parser::RoeDocument::Writer(d) => d, _ => panic!("not writer doc") };

    let registry = FontRegistry::new_with_system_fonts();
    let mut engine = ParagraphLayoutEngine::new(registry);
    let result = engine.layout(&wdoc);
    let arena = &result.arena;

    println!("total pages: {}", arena.page_order.len());
    for &pid in &arena.page_order {
        let page = &arena.pages[pid];
        println!("page width={} height={} margin_top={} margin_left={}",
            page.width, page.height, page.margin_top, page.margin_left);
        for &fid in &page.frames {
            let frame = &arena.frames[fid];
            println!("  frame: y_on_page={}", frame.y_on_page);
            if let FrameContent::Paragraph(lines) = &frame.content {
                for (li, line) in lines.iter().enumerate() {
                    println!("    line[{}]: y_offset={} x_offset={} line_height={} ascent={} descent={} text={:?}",
                        li, line.y_offset, line.x_offset, line.line_height, line.ascent, line.descent, line.text);
                    for (ri, run) in line.runs.iter().enumerate() {
                        println!("      run[{}]: start={} end={} is_formula={} font_size={}",
                            ri, run.start, run.end, run.style.is_formula, run.style.font_size);
                    }
                }
            } else {
                println!("    (non-paragraph frame content)");
            }
        }
    }
}
