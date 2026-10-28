//! roe-calc：网格 + 公式 DAG + 重算调度
//!
//! 架构（六层）：
//!   Storage     → roe-model::calc::SparseGrid
//!   Dependency  → dag::FormulaDAG (petgraph)
//!   Dirty       → dag::FormulaDAG::mark_dirty
//!   Scheduling  → scheduler::RecalcScheduler
//!   Execution   → interpreter::FormulaInterpreter
//!   Parallel    → (TODO: rayon，当前单线程)

pub mod dag;
pub mod group;
pub mod interpreter;
pub mod scheduler;
pub mod formula_engine;
pub mod cached_evaluator;
pub mod ref_shift;
pub use cached_evaluator::CachedEvaluator;

pub use dag::{CellId, FormulaDAG};
pub use scheduler::RecalcScheduler;

/// CalcEngine：把六层组合在一起的门面
pub struct CalcEngine {
    pub dag: FormulaDAG,
}

impl CalcEngine {
    pub fn new() -> Self {
        Self { dag: FormulaDAG::new() }
    }

    /// 标记某单元格已修改，需要重算
    pub fn notify_changed(&mut self, id: CellId) {
        self.dag.mark_dirty(id);
    }

    /// 触发全量重算，返回每个公式单元格的求值结果。
    /// 只算不写回——调用方若想让求值结果反映到 workbook.cells，
    /// 需要自己遍历返回值写回，或者直接用 [`Self::recalc_and_apply`]。
    pub fn recalc(
        &mut self,
        workbook: &roe_model::calc::Workbook,
    ) -> Vec<(CellId, Result<roe_model::calc::CellValue, scheduler::SchedulerError>)> {
        RecalcScheduler::recalc(&mut self.dag, workbook)
    }

    /// v104 新增：标记 dirty_cells 为脏 → 重算 → 把每个公式单元格的求值
    /// 结果写回 workbook.sheets[].cells 对应坐标。
    ///
    /// 这是 roe-api 应该调用的入口：roe_execute_script 跑完脚本后，只
    /// 拿到了"用户直接写了哪些单元格"（dirty_cells），公式本身求值、
    /// 结果落回内存模型（供 tile 渲染读取）都由这个函数负责。
    ///
    /// 返回值：本次重算实际产生结果的 (CellId, 求值结果) 列表，调用方
    /// 可以用它来判断有没有单元格出现 CircularRef 等错误。
    /// 注意：这里只处理 Formula 类型的单元格结果写回；非 Formula 单元格
    /// （用户直接写入的普通值）本身已经在 workbook 里了，不需要额外写回，
    /// scheduler 内部只是把它们的值原样缓存进 DAG 供依赖方读取。
    pub fn recalc_and_apply(
        &mut self,
        workbook: &mut roe_model::calc::Workbook,
        dirty_cells: &[(usize, u32, u32)],
    ) -> Vec<(CellId, roe_model::calc::CellValue)> {
        for &(sheet, row, col) in dirty_cells {
            let id = CellId::new(sheet as u32, row, col);
            self.dag.get_or_insert(id);
            self.notify_changed(id);
        }

        // 栈溢出止血:eval_with_dag 的递归求值钩子深度等于依赖链长度,
        // 默认栈下 debug 约 300 层、release 阈值未测,溢出是 abort 无法捕获。
        // 在大栈线程里跑重算(虚拟内存按需提交,平时不占物理内存)。
        let results = {
            let dag = &mut self.dag;
            let wb: &roe_model::calc::Workbook = &*workbook;
            std::thread::scope(|sc| {
                let h = std::thread::Builder::new()
                    .name("roe-recalc".into())
                    .stack_size(256 * 1024 * 1024)
                    .spawn_scoped(sc, move || RecalcScheduler::recalc(dag, wb))
                    .expect("spawn recalc thread");
                match h.join() {
                    Ok(r) => r,
                    Err(e) => std::panic::resume_unwind(e),
                }
            })
        };

        let mut applied = Vec::with_capacity(results.len());
        for (id, result) in results {
            let value = match result {
                Ok(v) => v,
                Err(_) => {
                    // SchedulerError（非循环引用的求值错误，比如 formualizer
                    // 内部报错）：写入通用 Value 错误，不 panic，不中断其余
                    // 单元格的写回。
                    roe_model::calc::CellValue::Error(roe_model::calc::CellError::Value)
                }
            };

            // 只对"原本就是 Formula"的单元格写回求值结果；跳过非公式单元格，
            // 避免把普通数值/文本单元格意外覆盖成缓存值（scheduler 内部对
            // 非公式单元格也会走 set_cached，但那只是 DAG 内部缓存，不代表
            // 应该覆盖 workbook 里的原始值）。
            // v104 修复：CellValue::Formula 已拆分为 { source, cached }
            // 两个字段（见 roe-model::calc::CellValue 定义），写回重算结果
            // 时必须保留原有 source（公式原文），只替换 cached（缓存的计算
            // 结果），不能像之前那样直接用裸值整体覆盖掉这个格子——那样会
            // 让公式字符串彻底丢失，编辑器再也看不到公式，导出文件里 <f>
            // 标签也会消失。只对"原本就是 Formula"的单元格执行这个操作；
            // 非公式单元格（用户直接写入的普通值）不需要写回。
            if let Some(sheet_ref) = workbook.sheets.get(id.sheet as usize) {
                let existing_source = match sheet_ref.cells.get(id.row, id.col) {
                    roe_model::calc::CellValue::Formula { source, .. } => Some(source),
                    _ => None,
                };
                if let Some(source) = existing_source {
                    if let Some(sheet_mut) = workbook.sheets.get_mut(id.sheet as usize) {
                        sheet_mut.cells.set(
                            id.row,
                            id.col,
                            roe_model::calc::CellValue::Formula {
                                source,
                                cached: Box::new(value.clone()),
                            },
                        );
                    }
                }
            }

            applied.push((id, value));
        }

        applied
    }
}

impl Default for CalcEngine {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::calc::{CellBlock, CellValue, Sheet, SparseColumn, SparseGrid, Workbook};
    use smallvec::smallvec;
    use std::collections::HashMap;

    fn make_workbook(cells: Vec<(u32, u32, CellValue)>) -> Workbook {
        let mut grid = SparseGrid::default();
        // 按列分组
        let mut by_col: std::collections::HashMap<u32, Vec<(u32, CellValue)>> = HashMap::new();
        for (row, col, val) in cells {
            by_col.entry(col).or_default().push((row, val));
        }
        for (col, mut rows) in by_col {
            rows.sort_by_key(|(r, _)| *r);
            let mut blocks: smallvec::SmallVec<[CellBlock; 4]> = smallvec::SmallVec::new();
            for (row, val) in rows {
                if let Some(last) = blocks.last_mut() {
                    if row == last.start_row + last.values.len() as u32 {
                        last.values.push(val);
                        continue;
                    }
                }
                blocks.push(CellBlock { start_row: row, values: smallvec![val] });
            }
            grid.cols.insert(col, SparseColumn { blocks });
        }
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: grid,
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
                cell_formats:     std::collections::HashMap::new(),
                cond_formats:     vec![],
                data_validations: vec![],
                auto_filter:      None,
                merged_cells:     vec![],
                pivot:            None,
                floating_images:  vec![],
                floating_charts:  vec![],
                floating_stat_charts: vec![],
                floating_shapes:  vec![],
                next_object_id: 0,
                default_col_width: 0.0,
                default_row_height: 0.0,
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn test_simple_formula() {
        // A1=10, A2=20, B1==A1+A2 → 30
        let wb = make_workbook(vec![
            (0, 0, CellValue::Number(10.0)),
            (1, 0, CellValue::Number(20.0)),
            (0, 1, CellValue::Formula { source: "=A1+A2".into(), cached: Box::new(CellValue::Empty) }),
        ]);
        let mut engine = CalcEngine::new();
        let id_b1 = CellId::new(0, 0, 1);
        engine.dag.get_or_insert(id_b1);
        engine.notify_changed(id_b1);
        let results = engine.recalc(&wb);
        assert_eq!(results.len(), 1);
        let (_, ref val) = results[0];
        assert!(matches!(val, Ok(CellValue::Number(n)) if (*n - 30.0).abs() < 1e-9),
            "expected 30, got {:?}", val);
    }

    #[test]
    fn test_sum_function() {
        // A1=1, A2=2, A3=3, B1==SUM(A1,A2,A3) → 6
        let wb = make_workbook(vec![
            (0, 0, CellValue::Number(1.0)),
            (1, 0, CellValue::Number(2.0)),
            (2, 0, CellValue::Number(3.0)),
            (0, 1, CellValue::Formula { source: "=SUM(A1,A2,A3)".into(), cached: Box::new(CellValue::Empty) }),
        ]);
        let mut engine = CalcEngine::new();
        let id = CellId::new(0, 0, 1);
        engine.dag.get_or_insert(id);
        engine.notify_changed(id);
        let results = engine.recalc(&wb);
        let (_, ref val) = results[0];
        assert!(matches!(val, Ok(CellValue::Number(n)) if (*n - 6.0).abs() < 1e-9),
            "expected 6, got {:?}", val);
    }

    #[test]
    fn test_if_function() {
        // B1==IF(TRUE,42,0) → 42
        let wb = make_workbook(vec![
            (0, 1, CellValue::Formula { source: "=IF(TRUE,42,0)".into(), cached: Box::new(CellValue::Empty) }),
        ]);
        let mut engine = CalcEngine::new();
        let id = CellId::new(0, 0, 1);
        engine.dag.get_or_insert(id);
        engine.notify_changed(id);
        let results = engine.recalc(&wb);
        let (_, ref val) = results[0];
        assert!(matches!(val, Ok(CellValue::Number(n)) if (*n - 42.0).abs() < 1e-9),
            "expected 42, got {:?}", val);
    }

    #[test]
    fn test_div_zero() {
        // B1==1/0 → Error(Div0)
        let wb = make_workbook(vec![
            (0, 1, CellValue::Formula { source: "=1/0".into(), cached: Box::new(CellValue::Empty) }),
        ]);
        let mut engine = CalcEngine::new();
        let id = CellId::new(0, 0, 1);
        engine.dag.get_or_insert(id);
        engine.notify_changed(id);
        let results = engine.recalc(&wb);
        let (_, ref val) = results[0];
        assert!(matches!(val, Ok(CellValue::Error(roe_model::calc::CellError::Div0))),
            "expected Div0, got {:?}", val);
    }
}
