//! RecalcScheduler：脏单元格队列 → 拓扑排序 → 按序求值

use roe_model::calc::{CellValue, Workbook};
use crate::dag::{CellId, FormulaDAG};
use crate::interpreter::InterpError;

#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error("interp error: {0}")]
    Interp(#[from] InterpError),
}

pub struct RecalcScheduler;

impl RecalcScheduler {
    /// 重算所有 dirty 的公式单元格。
    /// 若存在循环引用：环上的单元格不参与拓扑求值，直接产出
    /// CellError::CircularRef 结果（v104 修复，此前 toposort 失败时
    /// 整批 dirty 单元格被静默跳过，环上单元格保持旧值且无任何报错）。
    pub fn recalc(
        dag: &mut FormulaDAG,
        workbook: &Workbook,
    ) -> Vec<(CellId, Result<CellValue, SchedulerError>)> {
        let mut results = Vec::new();

        let order = match dag.dirty_topo_order() {
            Ok(order) => order,
            Err(cyclic_ids) => {
                for cell_id in cyclic_ids {
                    let err_val = CellValue::Error(roe_model::calc::CellError::CircularRef);
                    dag.set_cached(cell_id, err_val.clone());
                    results.push((cell_id, Ok(err_val)));
                }
                return results;
            }
        };

        // v105 修复：eval 过程中动态建边可能在本轮中途才检测到环
        // （见 eval_with_dag 里 has_path 拦截逻辑），此时 has_path 拦截
        // 分支会用 mark_dirty 把环上两个 cell 都重新标脏——但它们可能
        // 不在本次固定的 order 列表里，或者其中一个已经在 order 更早
        // 的位置被处理过了。用 while + handled 集合支持"处理过程中
        // 出现的新 dirty 节点也纳入本轮"，直到没有新 dirty 节点为止。
        let mut queue: std::collections::VecDeque<CellId> = order.into_iter().collect();
        let mut handled: std::collections::HashSet<CellId> = std::collections::HashSet::new();
        // 性能修复:queued 替代 queue.contains(O(n));新建节点走日志增量入队,
        // 只有环拦截后 mark_dirty_excluding 才做一次全图重扫。
        let mut queued: std::collections::HashSet<CellId> = queue.iter().copied().collect();
        let mut need_full_rescan = false;
        // 进入循环前新建的节点都已 dirty 且在 order 里,丢弃旧日志
        let _ = dag.take_new_nodes();

        while let Some(cell_id) = queue.pop_front() {
            if handled.contains(&cell_id) {
                continue;
            }
            handled.insert(cell_id);

            let formula = get_formula(workbook, cell_id);
            let formula = match formula {
                Some(f) => f,
                None => {
                    // 非公式单元格：直接缓存原始值
                    let val = get_raw_value(workbook, cell_id)
                        .unwrap_or(CellValue::Empty);
                    dag.set_cached(cell_id, val);
                    continue;
                }
            };

            // EvalContext 的实现：查 DAG 缓存，fallback 查 workbook
            // 为了避免同时持有 &mut dag 和 &dag，把查询逻辑内联进闭包
            let result = eval_with_dag(&formula, cell_id, dag, workbook);

            match result {
                Ok(val) => {
                    dag.set_cached(cell_id, val.clone());
                    results.push((cell_id, Ok(val)));
                }
                Err(InterpError::CircularRef(cyclic_id)) => {
                    // is_running 触发的旧式单节点自环检测（DFS running flag），
                    // eval_with_dag 没有额外 set_cached，这里补上。
                    let err_val = CellValue::Error(roe_model::calc::CellError::CircularRef);
                    dag.set_cached(cyclic_id, err_val.clone());
                    handled.insert(cyclic_id);
                    results.push((cyclic_id, Ok(err_val)));
                }
                Err(InterpError::CircularRefPair(a, b)) => {
                    // v105 修复：has_path 拦截触发的跨批次环检测。eval_with_dag
                    // 已经把 a、b 两者都 set_cached 成 CircularRef，这里只需要：
                    // 1) 把两者锁定进 handled，阻止它们被当成普通脏节点重新 eval
                    //    （否则会重新走 clear_dependencies + has_path 判断，
                    //    此时边已清空、成环边从未真正建立，判断不出环，导致
                    //    刚设置的 CircularRef 被覆盖——v105 早期版本的 bug）。
                    // 2) 只标脏 a、b 的下游（明确排除 a、b 自身），让真正依赖
                    //    这两个错误单元格的其他 cell 重新入队计算。
                    let err_val = CellValue::Error(roe_model::calc::CellError::CircularRef);
                    handled.insert(a);
                    handled.insert(b);
                    results.push((a, Ok(err_val.clone())));
                    if b != a {
                        results.push((b, Ok(err_val)));
                    }
                    let exclude: std::collections::HashSet<CellId> =
                        [a, b].into_iter().collect();
                    dag.mark_dirty_excluding(a, &exclude);
                    dag.mark_dirty_excluding(b, &exclude);
                    need_full_rescan = true;
                }
                Err(e) => {
                    dag.set_cached(
                        cell_id,
                        CellValue::Error(roe_model::calc::CellError::Value),
                    );
                    results.push((cell_id, Err(SchedulerError::Interp(e))));
                }
            }

            // 本次处理是否让某些之前已经 handled 的下游节点被重新标脏了？
            // 把当前图里所有 dirty 且未 handled 的节点补进 queue 尾部。
            // （日常场景下这个扫描代价很小：只在真正发生环拦截时才会
            // 产生新的 dirty 节点，正常路径下这里几乎不做任何事。）
            let mut candidates = dag.take_new_nodes();
            if need_full_rescan {
                candidates.extend(dag.dirty_cell_ids());
                need_full_rescan = false;
            }
            for id in candidates {
                if dag.is_dirty(id) && !handled.contains(&id) && queued.insert(id) {
                    queue.push_back(id);
                }
            }
        }

        // v105 修复：同一个 cell 可能在 results 里出现多条记录——例如
        // A1/A2 互相依赖、此前从未建立过任何边时，在同一次 recalc 内部：
        // 处理到 A2 时图上还没有边，A2 正常求值一次（产出一条"中间态"
        // 记录，读到 A1 当时的旧值）；紧接着处理到 A1 时才因为 has_path
        // 检测到刚才建的边导致成环，判定 A1/A2 都是 CircularRef（又产出
        // 两条记录）。这是运行时增量建边的必然结果：环只有在第二个
        // 节点被处理时才真正闭合，第一个节点已经"抢跑"算出了一个值。
        // 按 CellId 去重、只保留最后一条，能保证最终结果始终是正确的
        // （CircularRef 覆盖掉抢跑产生的中间值），也让调用方不用关心
        // 这种时序细节。用 IndexMap 语义手写：遍历时用 HashMap 记录
        // 每个 CellId 最新的结果，同时保留首次出现的相对顺序。
        let mut order_seen: Vec<CellId> = Vec::new();
        let mut latest: std::collections::HashMap<CellId, Result<CellValue, SchedulerError>> =
            std::collections::HashMap::new();
        for (id, r) in results {
            if !latest.contains_key(&id) {
                order_seen.push(id);
            }
            latest.insert(id, r);
        }
        order_seen
            .into_iter()
            .map(|id| {
                let r = latest.remove(&id).expect("just inserted");
                (id, r)
            })
            .collect()
    }
}

/// 内联的 eval：把 EvalContext 的查询逻辑直接写在这里，
/// 绕开同时 &mut dag + &dag 的借用冲突
fn eval_with_dag(
    formula: &str,
    self_id: CellId,
    dag: &mut FormulaDAG,
    workbook: &Workbook,
) -> Result<CellValue, InterpError> {

    // 循环引用检测
    if dag.is_running(self_id) {
        return Err(InterpError::CircularRef(self_id));
    }
    dag.set_running(self_id, true);

    let sheet_name = match workbook.sheets.get(self_id.sheet as usize) {
        Some(s) => s.name.clone(),
        None => {
            dag.set_running(self_id, false);
            return Err(InterpError::Parse(format!(
                "invalid sheet index: {}",
                self_id.sheet
            )));
        }
    };

    // ROOT-012 修复：递归惰性求值钩子（对齐 LOK ScFormulaCell::MaybeInterpret 设计）。
    // 用 RefCell 包一层 &mut FormulaDAG，让下面的闭包能在 &self 方法调用链
    // （SheetResolver::resolve_range_view 只有 &self）中，通过内部可变性
    // 拿到 &mut FormulaDAG 递归调用 eval_with_dag 自身。
    //
    // 闭包职责：当公式引擎发现一个依赖坐标(dep_sheet, dep_row0, dep_col0)本身
    // 也是 CellValue::Formula 时，不再直接返回该单元格的静态旧 cached 值——
    // 而是在这里"就地先算出它的真实结果"，再把结果喂回去。这样无论
    // dirty_topo_order 把谁排在前面，被引用的公式格总是会在被读取的那一刻
    // 先被算好，彻底消除"读到尚未求值的旧/空缓存"的问题（ROOT-012）。
    //
    // 环检测：若递归途中又访问回 self_id 或任何一个当前正在 running 的节点，
    // is_running 检测会命中，这里返回 None，让调用方按 literal_for_cell 的
    // 兜底逻辑退回读取该单元格的静态 cached（通常是 CircularRef 或旧值），
    // 不在 hook 内部产生新的错误分支，避免打乱现有 has_path/CircularRefPair
    // 环检测机制（那套机制仍然按原样工作，作为这层递归求值兜底之后的
    // 最终防线）。
    let dag_cell = std::sync::RwLock::new(dag);
    let hook = |dep_sheet_name: &str, dep_row0: u32, dep_col0: u32| -> Option<CellValue> {
        let dep_sheet_idx = workbook
            .sheets
            .iter()
            .position(|s| s.name == dep_sheet_name)?;
        let dep_id = CellId::new(dep_sheet_idx as u32, dep_row0, dep_col0);

        // 自己引用自己，或递归回到调用链上正在 running 的节点：是环，
        // 交还给外层原有的环检测机制处理，这里不介入。
        if dag_cell.read().expect("dag lock poisoned").is_running(dep_id) {
            return None;
        }

        // 已经算过且不是 dirty：直接用缓存，不用重新求值。
        {
            let dag_ref = dag_cell.read().expect("dag lock poisoned");
            if !dag_ref.is_dirty(dep_id) {
                if let Some(v) = dag_ref.get_cached(dep_id) {
                    return Some(v.clone());
                }
            }
        }

        let dep_formula = get_formula(workbook, dep_id)?;
        let mut dag_mut = dag_cell.write().expect("dag lock poisoned");
        match eval_with_dag(&dep_formula, dep_id, &mut dag_mut, workbook) {
            Ok(v) => {
                dag_mut.set_cached(dep_id, v.clone());
                Some(v)
            }
            Err(_) => None, // 递归求值出错：兜底交还给静态 cached 分支
        }
    };

    let (result, deps) = {
        let resolver = crate::formula_engine::SheetResolver::new_with_recalc_hook(
            workbook,
            &sheet_name,
            &hook,
        );
        // ROOT-016 修复：传入 self_id 的 (row, col)，使 ROW()/COLUMN() 无参调用
        // 能通过 EvaluationContext::current_cell() 拿到当前单元格坐标。
        crate::formula_engine::eval_formula_with_deps_with_resolver_and_cell(
            resolver,
            formula,
            Some((self_id.row, self_id.col)),
        )
    };

    let dag = dag_cell.into_inner().expect("dag lock poisoned");

    // 第 4 步：把本次求值访问过的单元格注册为 self_id 的依赖。
    // 先清空 self_id 现有的入边（公式可能已经改变，旧依赖不再有效），
    // 再按本次实际访问到的单元格重新建边。
    dag.clear_dependencies(self_id);
    for (dep_sheet_name, dep_row, dep_col) in deps {
        if let Some(dep_sheet_idx) = workbook
            .sheets
            .iter()
            .position(|s| s.name == dep_sheet_name)
        {
            let dep_id = CellId::new(dep_sheet_idx as u32, dep_row, dep_col);

            // v105 修复：跨批次动态建边导致的环漏检。
            // 场景：批次1 A1="=A2" 建边 A2→A1（A2 当时无公式，图不成环）。
            // 批次2 A2="=A1"，本轮 dirty_topo_order 在 eval 之前跑，此时
            // 图上只有 A2→A1 一条边，toposort 不会失败；只有等到这里、
            // eval A2 的过程中才会尝试建 A1→A2 这条边，图才真正成环——但
            // 这已经晚于本轮"要不要重算"的判断了，若不在这里拦截，环会
            // 被漏检（A2 会基于 A1 的旧值算出一个"正常"结果）。
            //
            // 拦截方式：加这条 dep_id→self_id 边之前，先看 self_id 是否
            // 已经能沿现有边走到 dep_id——如果能，说明加上这条边会成环。
            // 不真的加这条边（保留图结构，方便以后用户改公式解环），
            // 直接把 self_id 和 dep_id 都标为 CircularRef 并返回。
            if dag.has_path(self_id, dep_id) {
                dag.set_running(self_id, false);
                dag.set_cached(self_id, CellValue::Error(roe_model::calc::CellError::CircularRef));
                dag.set_cached(dep_id, CellValue::Error(roe_model::calc::CellError::CircularRef));
                // v105 修复：不在这里 mark_dirty(self_id)/mark_dirty(dep_id)
                // ——那样会导致这两个刚判定为环的节点自己也被标脏，进而被
                // scheduler 主循环当成普通脏节点重新 eval，重新走一遍
                // clear_dependencies + has_path 判断（此时边已被清空、且
                // 从未真正建立过那条导致成环的边），把刚设置好的
                // CircularRef 覆盖成"看似正常"的值。
                // 是否要标脏下游、以及具体标脏谁（排除这两者自身），
                // 交给 scheduler 主循环用 CircularRefPair 里带的两个
                // CellId 去做（见 recalc 里对应的 match 分支）。
                return Err(InterpError::CircularRefPair(self_id, dep_id));
            }

            dag.add_dependency(dep_id, self_id);
        }
        // 找不到 sheet 理论上不应该发生（SheetResolver 自己也要 find_sheet 成功
        // 才会记录访问），静默跳过而非 panic，避免因为极端情况中断整个重算。
    }

    dag.set_running(self_id, false);
    Ok(result)
}

fn get_formula(wb: &Workbook, id: CellId) -> Option<String> {
    let sheet = wb.sheets.get(id.sheet as usize)?;
    get_raw_value_from_sheet(sheet, id.row, id.col).and_then(|v| {
        if let CellValue::Formula { source, .. } = v { Some(source) } else { None }
    })
}

fn get_raw_value(wb: &Workbook, id: CellId) -> Option<CellValue> {
    wb.sheets.get(id.sheet as usize)
        .and_then(|s| get_raw_value_from_sheet(s, id.row, id.col))
}

fn get_raw_value_from_sheet(
    sheet: &roe_model::calc::Sheet,
    row: u32,
    col: u32,
) -> Option<CellValue> {
    let sc = sheet.cells.cols.get(&col)?;
    for block in &sc.blocks {
        if row >= block.start_row {
            let idx = (row - block.start_row) as usize;
            if idx < block.values.len() {
                return Some(block.values[idx].clone());
            }
        }
    }
    None
}


#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::calc::{CellBlock, CellValue, Sheet, SparseColumn, SparseGrid, Workbook};
    use smallvec::smallvec;
    use std::collections::HashMap;

    /// 单列单单元格的简易 workbook 构造器，够用即可（不追求覆盖 make_workbook 全部字段）。
    fn make_workbook(cells: Vec<(u32, u32, CellValue)>) -> Workbook {
        let mut grid = SparseGrid::default();
        let mut by_col: HashMap<u32, Vec<(u32, CellValue)>> = HashMap::new();
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
                cell_formats: HashMap::new(),
                cond_formats: vec![],
                data_validations: vec![],
                auto_filter: None,
                merged_cells: vec![],
                pivot: None,
                floating_images: vec![],
                floating_charts: vec![],
                floating_stat_charts: vec![],
                floating_shapes: vec![],
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

    /// v104 新增：A1 依赖 A2，A2 依赖 A1（互相循环），验证两者都得到
    /// CellError::CircularRef，而不是 panic，也不是静默保持旧值不报错。
    #[test]
    fn test_circular_reference_returns_circular_ref_error() {
        // A1 = "=A2", A2 = "=A1"
        let wb = make_workbook(vec![
            (0, 0, CellValue::Formula { source: "=A2".into(), cached: Box::new(CellValue::Empty) }),
            (1, 0, CellValue::Formula { source: "=A1".into(), cached: Box::new(CellValue::Empty) }),
        ]);
        let mut dag = FormulaDAG::new();
        let a1 = CellId::new(0, 0, 0);
        let a2 = CellId::new(0, 1, 0);
        dag.get_or_insert(a1);
        dag.get_or_insert(a2);
        dag.mark_dirty(a1);
        dag.mark_dirty(a2);

        // 第一次 recalc：建立依赖边（此时图里还没有边，toposort 不会报错，
        // 但求值 A1 时会去访问 A2，A2 又是 Formula("=A1")，会产生嵌套求值
        // ——具体走哪条路径取决于 eval_with_dag 实现，这里只断言最终两个
        // 单元格都不 panic、且结果要么是数值要么是 CircularRef 错误。
        let results = RecalcScheduler::recalc(&mut dag, &wb);
        assert_eq!(results.len(), 2, "expected both A1 and A2 in first recalc, got {:?}", results);

        // 依赖边建立后，再次标脏触发第二次 recalc：此时 DAG 里 A1<->A2
        // 已经是真正的图上环，toposort 必然失败，dirty_topo_order 走
        // Err(cyclic_ids) 分支，两者都应该是 CircularRef。
        dag.mark_dirty(a1);
        dag.mark_dirty(a2);
        let results2 = RecalcScheduler::recalc(&mut dag, &wb);
        assert_eq!(results2.len(), 2, "expected both cells reported on cyclic recalc, got {:?}", results2);
        for (id, r) in &results2 {
            match r {
                Ok(CellValue::Error(roe_model::calc::CellError::CircularRef)) => {}
                other => panic!("cell {:?}: expected CircularRef error, got {:?}", id, other),
            }
        }
    }

        /// 第 4 步核心验证：改 A1 的值后，只 notify_changed(A1)，不手动 notify B1，
    /// B1（=A1 的依赖方）也应该被自动标脏并在下次 recalc 中拿到新值。
    /// 如果依赖边没有被正确建立，这个测试会失败（B1 停留在旧值，因为
    /// dirty_topo_order 根本不会把从未被标脏的 B1 纳入本次重算）。
    #[test]
    fn test_dependency_propagation_marks_dependent_dirty() {
        // A1 = 5, B1 = "=A1"
        let mut wb = make_workbook(vec![
            (0, 0, CellValue::Number(5.0)),
            (0, 1, CellValue::Formula { source: "=A1".into(), cached: Box::new(CellValue::Empty) }),
        ]);
        let mut dag = FormulaDAG::new();
        let a1 = CellId::new(0, 0, 0);
        let b1 = CellId::new(0, 0, 1);
        dag.get_or_insert(a1);
        dag.get_or_insert(b1);

        // 第一次 recalc：建立 B1 -> 依赖 A1 的边
        dag.mark_dirty(b1);
        let results = RecalcScheduler::recalc(&mut dag, &wb);
        let b1_val = results.iter().find(|(id, _)| *id == b1).map(|(_, r)| r);
        assert!(
            matches!(b1_val, Some(Ok(CellValue::Number(n))) if (*n - 5.0).abs() < 1e-9),
            "expected B1=5 on first recalc, got {:?}",
            b1_val
        );

        // 改 A1 的值为 99，只 notify_changed(A1)，不手动碰 B1
        wb.sheets[0]
            .cells
            .cols
            .get_mut(&0)
            .unwrap()
            .blocks[0]
            .values[0] = CellValue::Number(99.0);
        dag.mark_dirty(a1);

        // 第二次 recalc：验证 B1 因为依赖边存在而被自动标脏、拿到新值
        let results2 = RecalcScheduler::recalc(&mut dag, &wb);
        let b1_val2 = results2.iter().find(|(id, _)| *id == b1).map(|(_, r)| r);
        assert!(
            matches!(b1_val2, Some(Ok(CellValue::Number(n))) if (*n - 99.0).abs() < 1e-9),
            "expected B1=99 after A1 changed (dependency edge should propagate dirty), got {:?}",
            b1_val2
        );
    }

    /// ROOT-012 最小复现：B1 = "=A1"，A1 = "=UNDEFINED_NAME"（一个不存在的
    /// 命名区域，公式引擎应返回 Name 类错误；这里为了不依赖 formula_engine
    /// 具体报什么错，只关心 B1 是否正确跟随 A1 的最终结果，而不是读到 A1
    /// 的初始占位值 Empty）。
    ///
    /// 复现关键：B1 的 CellId (row=0) 在 A1 (row=1) 之前被 get_or_insert，
    /// 两者之间在 recalc 开始时都还没有边（首次打开场景），
    /// dirty_topo_order 对无边图的 toposort 退化为按节点插入顺序，
    /// B1 排在 A1 之前被求值，读到 A1 尚未算出的旧 cached。
    #[test]
    fn test_root012_dependent_reads_stale_value_when_evaluated_before_dependency() {
        let wb = make_workbook(vec![
            (0, 0, CellValue::Formula { source: "=A2".into(), cached: Box::new(CellValue::Empty) }),
            (1, 0, CellValue::Formula { source: "=NOSUCHNAME".into(), cached: Box::new(CellValue::Empty) }),
        ]);
        let mut dag = FormulaDAG::new();
        let b1 = CellId::new(0, 0, 0); // "=A2" -- 依赖 a1(row=1)
        let a1 = CellId::new(0, 1, 0); // "=NOSUCHNAME"

        // 故意先插入 b1 再插入 a1，模拟"扫描顺序恰好让依赖方先被建节点"的情况
        dag.get_or_insert(b1);
        dag.get_or_insert(a1);
        dag.mark_dirty(b1);
        dag.mark_dirty(a1);

        let results = RecalcScheduler::recalc(&mut dag, &wb);

        let a1_val = results.iter().find(|(id, _)| *id == a1).map(|(_, r)| r);
        let b1_val = results.iter().find(|(id, _)| *id == b1).map(|(_, r)| r);

        println!("ROOT-012 repro: a1={:?} b1={:?}", a1_val, b1_val);

        // 断言 A1 本身应该是错误（NOSUCHNAME 未定义）
        let a1_is_error = matches!(a1_val, Some(Ok(CellValue::Error(_))));
        assert!(a1_is_error, "expected A1 (=NOSUCHNAME) to be an error, got {:?}", a1_val);

        // 核心断言：B1 (=A2, 即依赖 A1) 求值时应该拿到 A1 的最终错误值，
        // 而不是 Empty。若此断言失败，说明 ROOT-012 复现成功。
        let b1_is_stale_empty = matches!(b1_val, Some(Ok(CellValue::Empty)));
        assert!(
            !b1_is_stale_empty,
            "ROOT-012 REPRODUCED: B1 read A1's stale initial cached value (Empty) \
             instead of A1's final computed error. b1_val={:?}",
            b1_val
        );
    }
}
