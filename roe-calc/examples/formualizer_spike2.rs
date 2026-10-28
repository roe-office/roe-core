// Spike B：验证不经过 Engine，直接用 Interpreter::evaluate_ast 对自己的数据求值
use formualizer_common::{LiteralValue, ExcelError};
use formualizer_eval::test_workbook::TestWorkbook;
use formualizer_eval::interpreter::Interpreter;
use formualizer_parse::parser::parse;
use formualizer_eval::builtins::load_builtins;

fn main() {
    load_builtins();

    // 先用 TestWorkbook 当 resolver（验证链路本身），
    // 后续再评估换成 roe_model::calc::Sheet 的适配层成本
    let wb = TestWorkbook::new()
        .with_cell("Sheet1", 1, 1, LiteralValue::Number(1.0))
        .with_cell("Sheet1", 2, 1, LiteralValue::Number(2.0))
        .with_cell("Sheet1", 3, 1, LiteralValue::Number(3.0))
        .with_cell("Sheet1", 4, 1, LiteralValue::Number(4.0))
        .with_cell("Sheet1", 5, 1, LiteralValue::Number(5.0));

    let ast = parse("=SUM(A1:A5)").unwrap();

    let interp = Interpreter::new(&wb, "Sheet1");
    let result: Result<_, ExcelError> = interp.evaluate_ast(&ast).map(|v| v.into_literal());

    println!("SUM(A1:A5) via Interpreter (no Engine) = {:?}", result);
}
