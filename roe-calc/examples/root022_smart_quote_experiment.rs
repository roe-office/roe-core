use formualizer_common::LiteralValue;
use formualizer_eval::engine::{Engine, EvalConfig};
use formualizer_eval::test_workbook::TestWorkbook;
use formualizer_parse::parser::parse;

/// 归一化策略A：智能双引号(“”)→ASCII双引号(")，智能单引号(‘’)保持不变
fn normalize_smart_double_quotes_only(formula: &str) -> String {
    formula.replace(['\u{201C}', '\u{201D}'], "\"")
}

/// 归一化策略B：智能双引号+智能单引号全部归一化为对应ASCII引号
fn normalize_all_smart_quotes(formula: &str) -> String {
    formula
        .replace(['\u{201C}', '\u{201D}'], "\"")
        .replace(['\u{2018}', '\u{2019}'], "'")
}

fn try_eval(label: &str, formula: &str) {
    let wb = TestWorkbook::new();
    let mut engine = Engine::new(wb, EvalConfig::default());
    // D/E 列留空（不设值），模拟原始样本里的空单元格引用
    match parse(formula) {
        Ok(ast) => {
            if let Err(e) = engine.set_cell_formula("Sheet1", 1, 1, ast) {
                println!("[{label}] formula={formula:?} => set_cell_formula ERR: {e:?}");
                return;
            }
            match engine.evaluate_all() {
                Ok(_) => {
                    let result = engine.get_cell_value("Sheet1", 1, 1);
                    println!("[{label}] formula={formula:?} => {result:?}");
                }
                Err(e) => println!("[{label}] formula={formula:?} => evaluate_all ERR: {e:?}"),
            }
        }
        Err(e) => println!("[{label}] formula={formula:?} => parse ERR: {e:?}"),
    }
}

fn main() {
    // 原始触发问题的四组公式（用 D2/E2 模拟 D1/E1，避免和 A1 单元格本身冲突，实际列号不重要）
    let originals = [
        "=\u{201C}",                                   // A1: “
        "=OR(D1=0,D1<>\u{201C}\u{201C})",              // B1: OR(D1=0,D1<>““)
        "=OR(E1=0,E1<>\u{201C})",                       // C1: OR(E1=0,E1<>“)
        "=\u{2018}",                                    // A3: ‘
        "=OR(D3=0,D3<>\u{2018}\u{2018})",               // B3: OR(D3=0,D3<>‘‘)
        "=OR(E3=0,E3<>\u{2018})",                        // C3: OR(E3=0,E3<>‘)
    ];

    println!("=== 原始（未归一化，预期复现bug）===");
    for f in &originals {
        try_eval("raw", f);
    }

    println!("\n=== 策略A：仅智能双引号归一化 ===");
    for f in &originals {
        let normalized = normalize_smart_double_quotes_only(f);
        try_eval("strategyA", &normalized);
    }

    println!("\n=== 策略B：智能双引号+单引号全部归一化 ===");
    for f in &originals {
        let normalized = normalize_all_smart_quotes(f);
        try_eval("strategyB", &normalized);
    }
}
