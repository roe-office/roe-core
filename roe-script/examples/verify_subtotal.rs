//! v227/v228 SUBTOTAL 坐标bug 端到端回归验证工具（保留为长期工具，非一次性脚本）。
//!
//! 背景：v227 会话里，SUBTOTAL改名后单元测试全绿，但真实多列xlsx文件依然
//! 算错（207 vs 期望105）。原因是当时所有相关单元测试碰巧只测过A列，
//! 掩盖了 SheetResolver::resolve_range_view 返回的 RangeView 坐标语义
//! 问题（详见 roe-calc/src/formula_engine.rs 中 RoeSubtotalFn::eval()
//! 上方注释）。这次教训是"单元测试绿≠真实场景对"，因此保留本工具作为
//! 用真实文件端到端验证的补充手段，不依赖构造出来的测试数据。
//!
//! 用法：
//!   mkdir -p /tmp/roe_test
//!   cp test_samples/xlsx/sc_data_xlsx__subtotal-above.xlsx /tmp/roe_test/
//!   cargo run -p roe-script --example verify_subtotal
//!
//! 期望输出（v228修复后）：B2=Number(105.0)，C2=Number(119.0)，
//! 与 WPS/OnlyOffice/Excel 标准行为、该xlsx文件原始缓存值一致。
//! 若未来改动了 SUBTOTAL/坐标解析相关代码，建议重跑本工具作为回归验证，
//! 不要只依赖单元测试通过就判断没问题。

// 临时验证脚本：读取真实xlsx文件，走真实导入+重算路径，打印B2/C2结果
use roe_calc::CalcEngine;
use roe_parser::ooxml::xlsx::XlsxParser;

fn print_cell(wb: &roe_model::calc::Workbook, r: u32, c: u32, label: &str, tag: &str) {
    let sheet = &wb.sheets[0];
    if let Some(col_data) = sheet.cells.cols.get(&c) {
        for block in &col_data.blocks {
            if r >= block.start_row && r < block.start_row + block.values.len() as u32 {
                let idx = (r - block.start_row) as usize;
                println!("{} ({}): {:?}", label, tag, block.values[idx]);
                return;
            }
        }
    }
    println!("{} ({}): <not found>", label, tag);
}

fn main() {
    let path = "/tmp/roe_test/sc_data_xlsx__subtotal-above.xlsx";
    let bytes = std::fs::read(path).expect("read file");
    let mut wb = XlsxParser::parse(&bytes).expect("parse xlsx");

    print_cell(&wb, 1, 1, "B2", "导入原始");
    print_cell(&wb, 1, 2, "C2", "导入原始");

    let mut calc_engine = CalcEngine::new();
    let all_dirty: Vec<(usize, u32, u32)> = vec![(0, 1, 1), (0, 1, 2)];
    calc_engine.recalc_and_apply(&mut wb, &all_dirty);

    print_cell(&wb, 1, 1, "B2", "重算后");
    print_cell(&wb, 1, 2, "C2", "重算后");
}
