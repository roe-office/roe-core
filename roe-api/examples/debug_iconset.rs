// v130临时调试脚本：排查ROOT-024（IconSet完全未渲染）。
// 打印cond_formats列表，确认IconSet规则是否被正确解析（数据层），
// 以及对应单元格的实际数值（用于后续判断render_icon_set内部阈值计算是否正确）。
use roe_api::test_support::open_calc_for_test as open_calc;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: debug_iconset <file>");
    let wb = open_calc(path).expect("open failed");
    for sheet in &wb.sheets {
        println!("sheet: {}", sheet.name);
        println!("cond_formats count: {}", sheet.cond_formats.len());
        for (i, rule) in sheet.cond_formats.iter().enumerate() {
            println!("  [{}] range={:?}", i, rule.range);
            match &rule.kind {
                roe_model::calc::ConditionalFormatKind::IconSet { icon_style, thresholds, percent, reverse, icons_only, .. } => {
                    println!("      IconSet icon_style={:?} thresholds={:?} percent={} reverse={} icons_only={}",
                        icon_style, thresholds, percent, reverse, icons_only);
                }
                other => {
                    println!("      other kind: {:?}", other);
                }
            }
        }
        // 打印规则涉及范围内的实际单元格数值
        for rule in &sheet.cond_formats {
            if let roe_model::calc::ConditionalFormatKind::IconSet { .. } = &rule.kind {
                let (r1, c1) = rule.range.start;
                let (r2, c2) = rule.range.end;
                for row_idx in r1..=r2 {
                    for col_idx in c1..=c2 {
                        let val = sheet.cells.get(row_idx, col_idx);
                        println!("    cell ({},{}) = {:?}", row_idx, col_idx, val);
                    }
                }
            }
        }
    }
}
