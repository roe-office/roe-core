use roe_parser::ooxml::xlsx::XlsxParser;
use roe_parser::ooxml::xlsx_writer::XlsxWriter;

fn tables(wb: &roe_model::calc::Workbook) -> Vec<&roe_model::calc::Table> {
    wb.sheets.iter().flat_map(|s| s.tables.iter()).collect()
}

#[test]
fn table_roundtrip_keeps_identity_and_style() {
    for f in ["TableStyleTest", "tdf131424", "tablerefsnamed", "TableEmptyHeaders"] {
        let path = format!("{}/../test_samples/xlsx/sc_data_xlsx__{}.xlsx", env!("CARGO_MANIFEST_DIR"), f);
        let wb1 = XlsxParser::parse(&std::fs::read(&path).unwrap()).expect("parse1");
        let bytes = XlsxWriter::write(&wb1).expect("write");
        let wb2 = XlsxParser::parse(&bytes).expect("parse2");
        let (a, b) = (tables(&wb1), tables(&wb2));
        assert!(!a.is_empty(), "{f}: 样本应有表格");
        assert_eq!(a.len(), b.len(), "{f}: 表格数量");
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.name, y.name, "{f} name");
            assert_eq!(x.range.start, y.range.start, "{f} range.start");
            assert_eq!(x.range.end, y.range.end, "{f} range.end");
            assert_eq!(x.has_header, y.has_header, "{f} has_header");
            assert_eq!(x.has_totals, y.has_totals, "{f} has_totals");
            assert_eq!(x.style_name, y.style_name, "{f} style_name");
            assert_eq!(x.show_first_column, y.show_first_column, "{f}");
            assert_eq!(x.show_last_column, y.show_last_column, "{f}");
            assert_eq!(x.show_row_stripes, y.show_row_stripes, "{f}");
            assert_eq!(x.show_column_stripes, y.show_column_stripes, "{f}");
            assert_eq!(x.columns, y.columns, "{f} columns");
            assert_eq!(x.auto_filter.is_some(), y.auto_filter.is_some(), "{f} auto_filter");
        }
    }
}

#[test]
fn theme_roundtrip() {
    let path = format!("{}/../test_samples/xlsx/sc_data_xlsx__TableStyleTest.xlsx", env!("CARGO_MANIFEST_DIR"));
    let wb1 = XlsxParser::parse(&std::fs::read(&path).unwrap()).expect("parse1");
    let bytes = XlsxWriter::write(&wb1).expect("write");
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).expect("zip");
    assert!(z.by_name("xl/theme/theme1.xml").is_ok(), "应写出主题");
    let wb2 = XlsxParser::parse(&bytes).expect("parse2");
    let t = wb2.sheets.iter().flat_map(|s| s.tables.iter()).next().expect("table");
    assert_eq!(t.theme_palette.get(4), Some(&[0x15, 0x60, 0x82]));
}
