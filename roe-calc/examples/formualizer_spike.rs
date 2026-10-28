use formualizer_common::LiteralValue;
use formualizer_eval::engine::{Engine, EvalConfig};
use formualizer_eval::test_workbook::TestWorkbook;
use formualizer_parse::parser::parse;

fn main() {
    let wb = TestWorkbook::new();
    let mut engine = Engine::new(wb, EvalConfig::default());

    engine.set_cell_value("Sheet1", 1, 1, LiteralValue::Int(1)).unwrap();
    engine.set_cell_value("Sheet1", 2, 1, LiteralValue::Int(2)).unwrap();
    engine.set_cell_value("Sheet1", 3, 1, LiteralValue::Int(3)).unwrap();
    engine.set_cell_value("Sheet1", 4, 1, LiteralValue::Int(4)).unwrap();
    engine.set_cell_value("Sheet1", 5, 1, LiteralValue::Int(5)).unwrap();

    let ast = parse("=SUM(A1:A5)").unwrap();
    engine.set_cell_formula("Sheet1", 6, 1, ast).unwrap();

    engine.evaluate_all().unwrap();

    let result = engine.get_cell_value("Sheet1", 6, 1);
    println!("A6 (SUM(A1:A5)) = {:?}", result);
}
