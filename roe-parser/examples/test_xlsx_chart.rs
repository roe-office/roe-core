use roe_parser::ooxml::xlsx::XlsxParser;

fn main() {
    let data = std::fs::read("/tmp/test_chart.xlsx").expect("读取 xlsx 失败");
    let wb = XlsxParser::parse(&data).expect("解析失败");
    println!("sheet 数: {}", wb.sheets.len());
    for (i, sheet) in wb.sheets.iter().enumerate() {
        println!("Sheet {}: {} 个浮动图表", i, sheet.floating_charts.len());
        for (j, chart) in sheet.floating_charts.iter().enumerate() {
            println!("  Chart {}: {:?}", j, chart);
        }
    }
}
