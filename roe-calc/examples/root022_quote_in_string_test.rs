use formualizer_eval::engine::{Engine, EvalConfig};
use formualizer_eval::test_workbook::TestWorkbook;
use formualizer_parse::parser::parse;

fn main() {
    // 智能引号作为字符串内容的一部分（外层是合法ASCII双引号定界符）
    let formula = "=\"她说：\u{201C}你好\u{201D}\"";
    let wb = TestWorkbook::new();
    let mut engine = Engine::new(wb, EvalConfig::default());
    match parse(formula) {
        Ok(ast) => {
            engine.set_cell_formula("Sheet1", 1, 1, ast).unwrap();
            engine.evaluate_all().unwrap();
            let result = engine.get_cell_value("Sheet1", 1, 1);
            println!("formula={formula:?} => {result:?}");
        }
        Err(e) => println!("formula={formula:?} => parse ERR: {e:?}"),
    }
}
