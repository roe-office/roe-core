use roe_parser::ooxml::xlsx::XlsxParser;
use roe_parser::ooxml::xlsx_writer::XlsxWriter;

fn counts(wb: &roe_model::calc::Workbook) -> Vec<(usize, usize)> {
    wb.sheets.iter().map(|s| (s.floating_images.len(), s.floating_shapes.len())).collect()
}

#[test]
fn real_files_read_write_read() {
    let names = [
        "sc_data_xlsx__image_hyperlink.xlsx",
        "sc_data_xlsx__hiddenShape.xlsx",
        "sc_data_xlsx__groupShape.xlsx",
        "sc_data_xlsx__tdf135828_Shape_Rect.xlsx",
    ];
    let mut img_total = 0usize;
    for n in names {
        let path = format!("{}/../test_samples/xlsx/{}", env!("CARGO_MANIFEST_DIR"), n);
        let data = match std::fs::read(&path) { Ok(d) => d, Err(_) => { eprintln!("跳过(找不到) {n}"); continue; } };
        let a = XlsxParser::parse(&data).expect("首次读取失败");
        let bytes = XlsxWriter::write(&a).expect("写出失败");
        let b = XlsxParser::parse(&bytes).expect("回读失败");
        let (ca, cb) = (counts(&a), counts(&b));
        eprintln!("{n}\n  原始(图片,图形)={:?}\n  回读(图片,图形)={:?}", ca, cb);
        for (sa, sb) in a.sheets.iter().zip(b.sheets.iter()) {
            for (ia, ib) in sa.floating_images.iter().zip(sb.floating_images.iter()) {
                eprintln!("  图片位置 原({:.1},{:.1},{:.1},{:.1}) 回({:.1},{:.1},{:.1},{:.1})",
                    ia.x_pt, ia.y_pt, ia.width_pt, ia.height_pt, ib.x_pt, ib.y_pt, ib.width_pt, ib.height_pt);
            }
        }
        let ia: usize = ca.iter().map(|c| c.0).sum();
        let ib: usize = cb.iter().map(|c| c.0).sum();
        img_total += ia;
        assert_eq!(ia, ib, "{n} 图片数量回读不一致");
    }
    assert!(img_total > 0, "没有任何样本含图片，测试无效");
}
