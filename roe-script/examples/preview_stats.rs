//! 统计工具11项输出效果预览（v163交接文档第七节任务队列第1项）。
//!
//! 目的：v163完成的9个统计函数（describe_stats/t_test/f_test/correl/
//! covariance/regression/moving_average/exponential_smoothing/
//! anova_single_factor/anova_two_factor）此前只验证了公式字符串生成
//! 正确，从未验证 formualizer-eval 实际求值这批公式的数值结果。本脚本
//! 通过真实的 ScriptEngine（生产入口）写入公式，再用 CalcEngine::
//! recalc_and_apply 触发真实求值，打印每个输出单元格的"公式 -> 求值
//! 结果"，重点核实：
//!   1. 是否有 #REF!/CircularRef/其他 Error 出现
//!   2. ANOVA双因素的双向交叉引用（Rows/Columns引用未写的Error行，
//!      Error/Total互相引用）在真实求值时是否被误判为循环依赖
//!   3. P-value是否落在[0,1]合理区间、F值/方差等是否为正常数值
//!
//! 用法（debug模式，不要用--release）：
//!   cargo run -p roe-script --example preview_stats                # 全部9个
//!   cargo run -p roe-script --example preview_stats -- anova_two_factor  # 只跑一个
//!
//! 可选函数名：describe_stats / t_test / f_test / correl / covariance /
//!            regression / moving_average / exponential_smoothing /
//!            anova_single_factor / anova_two_factor

use roe_calc::CalcEngine;
use roe_model::calc::{CellError, CellValue, Sheet, SparseGrid, Workbook};
use roe_script::engine::ScriptEngine;
use std::collections::HashMap;

fn empty_wb() -> Workbook {
    Workbook {
        sheets: vec![Sheet {
            col_style: vec![],
            tables: vec![],
            name: "Sheet1".into(),
            cells: SparseGrid::default(),
            col_widths: vec![],
            row_heights: vec![],
            frozen_rows: 0,
            frozen_cols: 0,
            date1904: false,
            zoom_scale: None,
            show_grid_lines: None,
            col_hidden: vec![],
            row_hidden: vec![],
            col_outline_level: vec![],
            row_outline_level: vec![],
            cell_formats: HashMap::new(),
            cond_formats: vec![],
            data_validations: vec![],
            auto_filter: None,
            merged_cells: vec![],
            pivot: None,
            default_col_width: 0.0,
            default_row_height: 0.0,
            floating_images: vec![],
            floating_shapes: vec![],
            next_object_id: 0,
            floating_charts: vec![],
            floating_stat_charts: vec![],
            sparkline_groups: vec![],
        }],
        named_ranges: HashMap::new(),
        named_formulas: HashMap::new(),
        styles: roe_model::style::StyleRegistry::default(),
        formula_dialect_source: Default::default(),
    }
}

/// 写一个单元格（数字），通过生产入口 run_calc 走 set_cell。
fn set_num(engine: &ScriptEngine, wb: &mut Workbook, sheet: usize, row: usize, col: usize, v: f64) {
    let script = format!("set_cell({}, {}, {}, \"{}\")", sheet, row, col, v);
    engine.run_calc(&script, wb).expect("set_cell 不应失败");
}

/// 跑一段生成统计公式的 Rhai 脚本（如 describe_stats(...)），返回它自己
/// 声称写了多少行（rows_written JSON），以及本次操作触碰到的 dirty
/// region（后面要喂给 recalc_and_apply）。
fn run_stat_script(engine: &ScriptEngine, wb: &mut Workbook, script: &str) -> Vec<(usize, u32, u32)> {
    let (dirty, ret_val) = engine.run_calc(script, wb).expect("统计脚本执行不应失败");
    println!("  脚本: {}", script);
    println!("  返回: {}", ret_val);
    dirty
}

/// 触发真实重算，并打印输出区域内每个格子的"公式 -> 求值结果"。
/// out_sheet/out_row_start/rows/cols 描述扫描范围（不依赖dirty region的
/// 精确性，只要扫描范围覆盖输出区即可，避免受dirty聚合逻辑影响预览的
/// 完整性）。
fn recalc_and_print(
    wb: &mut Workbook,
    dirty: Vec<(usize, u32, u32)>,
    out_sheet: usize,
    out_row_start: u32,
    rows: u32,
    out_col_start: u32,
    cols: u32,
) {
    let mut calc_engine = CalcEngine::new();
    let results = calc_engine.recalc_and_apply(wb, &dirty);

    let mut circular_count = 0;
    let mut error_count = 0;

    println!("  -- 求值结果（sheet={}, 扫描 row {}..{}, col {}..{}）--",
        out_sheet, out_row_start, out_row_start + rows, out_col_start, out_col_start + cols);

    for r in out_row_start..(out_row_start + rows) {
        for c in out_col_start..(out_col_start + cols) {
            let sheet = &wb.sheets[out_sheet];
            let col_data = match sheet.cells.cols.get(&c) {
                Some(cd) => cd,
                None => continue,
            };
            for block in &col_data.blocks {
                if r >= block.start_row && r < block.start_row + block.values.len() as u32 {
                    let idx = (r - block.start_row) as usize;
                    let cell = &block.values[idx];
                    match cell {
                        CellValue::Formula { source, cached } => {
                            let display = describe_cellvalue(cached);
                            if matches!(**cached, CellValue::Error(CellError::CircularRef)) {
                                circular_count += 1;
                                println!("    [!! CIRCULAR REF !!] R{}C{}: {} => {}", r, c, source, display);
                            } else if matches!(**cached, CellValue::Error(_)) {
                                error_count += 1;
                                println!("    [!! ERROR !!] R{}C{}: {} => {}", r, c, source, display);
                            } else {
                                println!("    R{}C{}: {} => {}", r, c, source, display);
                            }
                        }
                        CellValue::Number(n) => {
                            println!("    R{}C{}: (常量数字) {}", r, c, n);
                        }
                        CellValue::Text(t) => {
                            println!("    R{}C{}: (文本) {}", r, c, t);
                        }
                        CellValue::Empty => {}
                        other => {
                            println!("    R{}C{}: (其他) {:?}", r, c, other);
                        }
                    }
                }
            }
        }
    }

    // recalc_and_apply 返回值里也可能有本次实际重算到、但不在我们扫描
    // 范围内的格子报错（理论上不应该，但双重保险打印一下汇总）。
    let total_results = results.len();
    println!("  -- 本次重算共处理 {} 个公式单元格，扫描区域内发现 CIRCULAR_REF={} 个, 其他ERROR={} 个 --",
        total_results, circular_count, error_count);
    if circular_count > 0 {
        println!("  !!!!! 警告：出现循环依赖，v163文档担忧的问题已复现，需要排查 !!!!!");
    }
}

fn describe_cellvalue(v: &CellValue) -> String {
    match v {
        CellValue::Number(n) => format!("{}", n),
        CellValue::Text(t) => format!("\"{}\"", t),
        CellValue::Empty => "(empty)".to_string(),
        CellValue::Error(e) => format!("#ERROR({:?})", e),
        other => format!("{:?}", other),
    }
}

// ============================================================
// 各函数的预览用例（数据直接取自 calc_api.rs 现有单元测试，保证语义
// 已被验证过一次，本脚本只关心"求值"这一层，不重新设计数据集）
// ============================================================

fn preview_describe_stats() {
    println!("\n===== describe_stats =====");
    let engine = ScriptEngine::new();
    let mut wb = empty_wb();
    for (i, v) in [1.0, 2.0, 3.0, 4.0, 5.0].iter().enumerate() {
        set_num(&engine, &mut wb, 0, i, 0, *v);
    }
    let dirty = run_stat_script(&engine, &mut wb, "describe_stats(0, 0, 4, 0, 0, 0, 3)");
    recalc_and_print(&mut wb, dirty, 0, 0, 15, 3, 2);
}

fn preview_t_test() {
    println!("\n===== t_test (配对样本, test_type=1) =====");
    let engine = ScriptEngine::new();
    let mut wb = empty_wb();
    // 配对样本要求逐点差值有方差，不能用完全线性相关的数据（会导致Div0）
    for (i, v) in [1.0, 5.0, 2.0, 8.0].iter().enumerate() {
        set_num(&engine, &mut wb, 0, i, 0, *v);
    }
    for (i, v) in [2.0, 4.0, 5.0, 6.0].iter().enumerate() {
        set_num(&engine, &mut wb, 0, i, 1, *v);
    }
    let dirty = run_stat_script(&engine, &mut wb, "t_test(0, 0, 3, 0, 0, 0, 3, 1, 2, 1, 0, 0, 3)");
    recalc_and_print(&mut wb, dirty, 0, 0, 5, 3, 2);

    println!("\n===== t_test (等方差双样本, test_type=2) =====");
    let engine2 = ScriptEngine::new();
    let mut wb2 = empty_wb();
    for (i, v) in [1.0, 5.0, 2.0, 8.0].iter().enumerate() {
        set_num(&engine2, &mut wb2, 0, i, 0, *v);
    }
    for (i, v) in [2.0, 4.0, 5.0, 6.0].iter().enumerate() {
        set_num(&engine2, &mut wb2, 0, i, 1, *v);
    }
    let dirty2 = run_stat_script(&engine2, &mut wb2, "t_test(0, 0, 3, 0, 0, 0, 3, 1, 2, 2, 0, 0, 3)");
    recalc_and_print(&mut wb2, dirty2, 0, 0, 5, 3, 2);
}

fn preview_f_test() {
    println!("\n===== f_test =====");
    let engine = ScriptEngine::new();
    let mut wb = empty_wb();
    for (i, v) in [1.0, 2.0, 3.0, 4.0].iter().enumerate() {
        set_num(&engine, &mut wb, 0, i, 0, *v);
    }
    for (i, v) in [2.0, 4.0, 6.0, 8.0].iter().enumerate() {
        set_num(&engine, &mut wb, 0, i, 1, *v);
    }
    // f_test(sheet1, r1s, r1e, c1, sheet2, r2s, r2e, c2, out_sheet, out_row, out_col)
    let dirty = run_stat_script(&engine, &mut wb, "f_test(0, 0, 3, 0, 0, 0, 3, 1, 0, 0, 3)");
    recalc_and_print(&mut wb, dirty, 0, 0, 5, 3, 2);
}

fn preview_correl() {
    println!("\n===== correl =====");
    let engine = ScriptEngine::new();
    let mut wb = empty_wb();
    for (i, v) in [1.0, 2.0, 3.0, 4.0].iter().enumerate() {
        set_num(&engine, &mut wb, 0, i, 0, *v);
    }
    for (i, v) in [2.0, 4.0, 6.0, 8.0].iter().enumerate() {
        set_num(&engine, &mut wb, 0, i, 1, *v);
    }
    let dirty = run_stat_script(&engine, &mut wb, "correl(0, 0, 3, 0, 0, 0, 3, 1, 0, 0, 3)");
    recalc_and_print(&mut wb, dirty, 0, 0, 3, 3, 2);
}

fn preview_covariance() {
    println!("\n===== covariance =====");
    let engine = ScriptEngine::new();
    let mut wb = empty_wb();
    for (i, v) in [1.0, 2.0, 3.0, 4.0].iter().enumerate() {
        set_num(&engine, &mut wb, 0, i, 0, *v);
    }
    for (i, v) in [2.0, 4.0, 6.0, 8.0].iter().enumerate() {
        set_num(&engine, &mut wb, 0, i, 1, *v);
    }
    let dirty = run_stat_script(&engine, &mut wb, "covariance(0, 0, 3, 0, 0, 0, 3, 1, 0, 0, 3)");
    recalc_and_print(&mut wb, dirty, 0, 0, 3, 3, 2);
}

fn preview_regression() {
    println!("\n===== regression =====");
    let engine = ScriptEngine::new();
    let mut wb = empty_wb();
    // 参数形态抄自 test: regression(0, 0, 3, 1, 0, 0, 3, 0, 0, 0, 3)
    // 依 doc 注释推断: regression(sheet_y, ry_s, ry_e, cy, sheet_x, rx_s(?), ... )
    // 保守起见，Y列(col=1)与X列(col=0)各填4个数，形成明确线性关系 y=2x
    for (i, v) in [1.0, 2.0, 3.0, 4.0].iter().enumerate() {
        set_num(&engine, &mut wb, 0, i, 0, *v); // X in col 0
    }
    for (i, v) in [2.0, 4.0, 6.0, 8.0].iter().enumerate() {
        set_num(&engine, &mut wb, 0, i, 1, *v); // Y in col 1
    }
    let dirty = run_stat_script(&engine, &mut wb, "regression(0, 0, 3, 1, 0, 0, 3, 0, 0, 0, 3)");
    recalc_and_print(&mut wb, dirty, 0, 0, 5, 3, 2);
}

fn preview_moving_average() {
    println!("\n===== moving_average =====");
    let engine = ScriptEngine::new();
    let mut wb = empty_wb();
    for (i, v) in [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0].iter().enumerate() {
        set_num(&engine, &mut wb, 0, i, 0, *v);
    }
    // moving_average(sheet, r_start, r_end, c, interval, out_sheet, out_row, out_col)
    let dirty = run_stat_script(&engine, &mut wb, "moving_average(0, 0, 6, 0, 5, 0, 0, 1)");
    recalc_and_print(&mut wb, dirty, 0, 0, 7, 1, 1);
}

fn preview_exponential_smoothing() {
    println!("\n===== exponential_smoothing =====");
    let engine = ScriptEngine::new();
    let mut wb = empty_wb();
    for (i, v) in [10.0, 12.0, 11.0].iter().enumerate() {
        set_num(&engine, &mut wb, 0, i, 0, *v);
    }
    // exponential_smoothing(sheet, r_start, r_end, c, alpha, out_sheet, out_row, out_col)
    // 布局：Alpha标签在(row0,col1)/Alpha值在(row0,col2)；平滑结果值在col2，
    // row1..row3。扫描范围要覆盖col1..col3才能看全。
    let dirty = run_stat_script(&engine, &mut wb, "exponential_smoothing(0, 0, 2, 0, 0.5, 0, 0, 1)");
    recalc_and_print(&mut wb, dirty, 0, 0, 4, 1, 2);
}

fn preview_anova_single_factor() {
    println!("\n===== anova_single_factor =====");
    let engine = ScriptEngine::new();
    let mut wb = empty_wb();
    // 3组按列分组，每组3个数（抄自测试范围 0..2 行 x 0..2 列）
    let grid = [[1.0, 5.0, 9.0], [2.0, 6.0, 10.0], [3.0, 7.0, 11.0]];
    for (r, vals) in grid.iter().enumerate() {
        for (c, v) in vals.iter().enumerate() {
            set_num(&engine, &mut wb, 0, r, c, *v);
        }
    }
    // anova_single_factor(sheet, r_start, r_end, c_start, c_end, grouped_by_column, alpha, out_sheet, out_row, out_col)
    let dirty = run_stat_script(&engine, &mut wb, "anova_single_factor(0, 0, 2, 0, 2, true, 0.05, 0, 0, 5)");
    recalc_and_print(&mut wb, dirty, 0, 0, 10, 5, 7);
}

fn preview_anova_two_factor_minimal_2x2() {
    println!("\n===== anova_two_factor 2x2最小规格（抄自单元测试，预期SS_Error=0导致Div0，属退化情形非bug）=====");
    let engine = ScriptEngine::new();
    let mut wb = empty_wb();
    let grid = [[1.0, 2.0], [3.0, 4.0]];
    for (r, vals) in grid.iter().enumerate() {
        for (c, v) in vals.iter().enumerate() {
            set_num(&engine, &mut wb, 0, r, c, *v);
        }
    }
    let dirty = run_stat_script(&engine, &mut wb, "anova_two_factor(0, 0, 1, 0, 1, 0.05, 0, 0, 5)");
    recalc_and_print(&mut wb, dirty, 0, 0, 12, 5, 7);
}

fn preview_anova_two_factor_3x3_with_noise() {
    println!("\n===== anova_two_factor 3x3带真实残差（验证分母非零时数值是否合理）=====");
    let engine = ScriptEngine::new();
    let mut wb = empty_wb();
    // 3行(Row1-3) x 3列(Col1-3)，行/列均有主效应，且非完美可加(留有残差)
    // 教科书式构造：baseline + row_effect + col_effect + 扰动
    let grid = [
        [12.0, 18.0, 25.0],
        [15.0, 20.0, 30.0],
        [10.0, 16.0, 21.0],
    ];
    for (r, vals) in grid.iter().enumerate() {
        for (c, v) in vals.iter().enumerate() {
            set_num(&engine, &mut wb, 0, r, c, *v);
        }
    }
    // anova_two_factor(sheet, r_start, r_end, c_start, c_end, alpha, out_sheet, out_row, out_col)
    let dirty = run_stat_script(&engine, &mut wb, "anova_two_factor(0, 0, 2, 0, 2, 0.05, 0, 0, 5)");
    // 3x3: rows_written = 1(alpha)+1(空)+3(ColumnX)+1(空)+3(RowX)+1(空)+4(ANOVA) = 14
    recalc_and_print(&mut wb, dirty, 0, 0, 14, 5, 7);
}

fn preview_z_test() {
    println!("\n===== z_test (已知方差双样本) =====");
    println!("预期: Mean 4/4.25, z≈-0.1768, 单尾P≈0.4298, 单尾临界≈1.6449, 双尾P≈0.8597, 双尾临界≈1.9600");
    let engine = ScriptEngine::new();
    let mut wb = empty_wb();
    for (i, v) in [1.0, 5.0, 2.0, 8.0].iter().enumerate() {
        set_num(&engine, &mut wb, 0, i, 0, *v);
    }
    for (i, v) in [2.0, 4.0, 5.0, 6.0].iter().enumerate() {
        set_num(&engine, &mut wb, 0, i, 1, *v);
    }
    // z_test(s1, r1s, r1e, c1, s2, r2s, r2e, c2, var1, var2, hyp_diff, alpha, out_sheet, out_row, out_col)
    let dirty = run_stat_script(&engine, &mut wb, "z_test(0, 0, 3, 0, 0, 0, 3, 1, 4.0, 4.0, 0.0, 0.05, 0, 0, 3)");
    recalc_and_print(&mut wb, dirty, 0, 0, 12, 3, 2);
}

fn preview_chi_square_test() {
    println!("\n===== chi_square_test =====");
    println!("预期: E=12,18/28,42  贡献=0.3333,0.2222/0.1429,0.0952  Chi-square≈0.7937, DF=1, P≈0.373");
    let engine = ScriptEngine::new();
    let mut wb = empty_wb();
    let vals = [[10.0, 20.0], [30.0, 40.0]];
    for r in 0..2usize {
        for c in 0..2usize {
            set_num(&engine, &mut wb, 0, r, c, vals[r][c]);
        }
    }
    // chi_square_test(sheet, r_start, r_end, c_start, c_end, out_sheet, out_row, out_col): 共11行,输出在D列起
    let dirty = run_stat_script(&engine, &mut wb, "chi_square_test(0, 0, 1, 0, 1, 0, 0, 3)");
    recalc_and_print(&mut wb, dirty, 0, 0, 11, 3, 2);
}

fn preview_anova_two_factor() {
    preview_anova_two_factor_minimal_2x2();
    preview_anova_two_factor_3x3_with_noise();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let target = args.get(1).map(|s| s.as_str());

    let all: Vec<(&str, fn())> = vec![
        ("describe_stats", preview_describe_stats),
        ("t_test", preview_t_test),
        ("f_test", preview_f_test),
        ("correl", preview_correl),
        ("covariance", preview_covariance),
        ("regression", preview_regression),
        ("moving_average", preview_moving_average),
        ("exponential_smoothing", preview_exponential_smoothing),
        ("anova_single_factor", preview_anova_single_factor),
        ("anova_two_factor", preview_anova_two_factor),
        ("z_test", preview_z_test),
        ("chi_square_test", preview_chi_square_test),
    ];

    match target {
        Some(name) => {
            match all.iter().find(|(n, _)| *n == name) {
                Some((_, f)) => f(),
                None => {
                    eprintln!("未知函数名: {}。可选: {:?}", name, all.iter().map(|(n, _)| *n).collect::<Vec<_>>());
                    std::process::exit(1);
                }
            }
        }
        None => {
            for (_, f) in &all {
                f();
            }
        }
    }

    println!("\n===== 预览结束 =====");
}
