use roe_parser::RoeDocument;

fn main() {
    let path = std::env::args().nth(1).expect("usage: debug_shape_style <path.pptx>");
    let doc = RoeDocument::open_pptx_file(&path).expect("parse pptx failed");

    match doc {
        RoeDocument::Impress(pres) => {
            for (i, slide) in pres.slides.iter().enumerate() {
                println!("=== slide {} ({} shapes) ===", i, slide.shapes.len());
                for s in &slide.shapes {
                    println!("shape_id={}", s.id);
                    println!("  style.fill = {:?}", s.style.fill);
                    println!("  style.stroke = {:?}", s.style.stroke);
                    println!("  text_body = {:?}", s.text_body);
                    if let roe_model::impress::ShapeContent::Text(paragraphs) = &s.content {
                        for (pi, p) in paragraphs.iter().enumerate() {
                            println!("  paragraph[{}] align={:?}", pi, p.alignment);
                            for (ri, r) in p.runs.iter().enumerate() {
                                println!("    run[{}] text={:?} color={:?} font_size={:?}", ri, r.text, r.color, r.font_size);
                            }
                        }
                    }
                    if let roe_model::impress::ShapeContent::Table(table) = &s.content {
                        println!("  table: {} rows, col_widths={:?}", table.rows.len(), table.col_widths);
                        for (ri, row) in table.rows.iter().enumerate() {
                            println!("    row[{}] row_height={:?}", ri, row.row_height);
                        }
                        for (ri, row) in table.rows.iter().enumerate() {
                            for (ci, cell) in row.cells.iter().enumerate() {
                                println!("    cell[row={}, col={}] bg_color={:?} hidden={} col_span={} row_span={}",
                                    ri, ci, cell.bg_color, cell.hidden, cell.col_span, cell.row_span);
                                println!("      border_color={:?} border_width={:?} pad(L/R/T/B)=({:?},{:?},{:?},{:?})",
                                    cell.border_color, cell.border_width,
                                    cell.pad_left, cell.pad_right, cell.pad_top, cell.pad_bottom);
                                for content in &cell.content {
                                    if let roe_model::common::CellContent::Paragraph(p) = content {
                                        for r in &p.runs {
                                            println!("      run text={:?} color={:?} font_size={:?}", r.text, r.color, r.font_size);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        _ => println!("not Impress document"),
    }
}
