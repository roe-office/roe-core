// 一次性调试脚本：打印 small__sw_dmapper_data__table-style-para-border.docx
// 里表格行的每个 cell 的 lines 数量/文字内容/border_left/border_right 字段，
// 定位"B1边框没画出来"的根因。
//
// 用法：
//   cp /tmp/roe_patches/debug_table_cells.rs roe-api/examples/debug_table_cells.rs
//   cargo run -p roe-api --example debug_table_cells -- \
//     test_samples/curated/docx/small__sw_dmapper_data__table-style-para-border.docx

use std::path::Path;
use roe_writer::layout::ParagraphLayoutEngine;
use roe_writer::FontRegistry;
use roe_parser::RoeDocument;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input_path = args.get(1).expect("usage: debug_table_cells <input.docx>");

    let data = std::fs::read(input_path).expect("read file");
    let doc = RoeDocument::open_docx_bytes(&data).expect("parse docx");
    let wdoc = match doc {
        RoeDocument::Writer(d) => d,
        _ => panic!("not a Writer document"),
    };

    let registry = FontRegistry::new_with_system_fonts();
    let mut engine = ParagraphLayoutEngine::new(registry);
    let result = engine.layout(&wdoc);
    let arena = &result.arena;

    for &pid in &arena.page_order {
        let page = &arena.pages[pid];
        for &fid in &page.frames {
            let frame = &arena.frames[fid];
            match &frame.content {
                roe_writer::frame::FrameContent::Paragraph(lines) => {
                    println!("[Paragraph frame] para_index={} lines={}", frame.para_index, lines.len());
                }
                roe_writer::frame::FrameContent::TableRow(cells) => {
                    println!("[TableRow frame] para_index={} y_on_page={:.2} height={:.2} num_cells={}",
                        frame.para_index, frame.y_on_page, frame.height(), cells.len());
                    for (ci, cell) in cells.iter().enumerate() {
                        println!(
                            "  cell[{}]: x_offset={:.2} cell_width={:.2} border_top={:?} border_bottom={:?} border_left={:?} border_right={:?} num_lines={}",
                            ci, cell.x_offset, cell.cell_width,
                            cell.border_top, cell.border_bottom,
                            cell.border_left, cell.border_right,
                            cell.lines.len(),
                        );
                        for (li, l) in cell.lines.iter().enumerate() {
                            println!(
                                "    line[{}]: text={:?} y_offset={:.2} line_height={:.2} x_offset={:.2} cell_width={:.2}",
                                li, l.text, l.y_offset, l.line_height, l.x_offset, l.cell_width,
                            );
                        }
                    }
                }
            }
        }
    }
}
