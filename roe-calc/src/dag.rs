//! FormulaDAG：用 petgraph 维护显式依赖图
//! 边 A→B 表示 B 依赖 A（A 变化时需要重算 B）

use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use roe_model::calc::CellValue;
use std::collections::HashMap;

/// 单元格唯一标识：(sheet_index, row, col)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellId {
    pub sheet: u32,
    pub row:   u32,
    pub col:   u32,
}

impl CellId {
    pub fn new(sheet: u32, row: u32, col: u32) -> Self {
        Self { sheet, row, col }
    }
}

/// DAG 节点数据
#[derive(Debug, Clone)]
pub struct CellNode {
    pub id:      CellId,
    pub running: bool,       // 循环引用检测用
    pub dirty:   bool,
    pub cached:  Option<CellValue>,  // 上次求值结果缓存
}

pub struct FormulaDAG {
    /// 有向图：节点是 CellNode，边无权重
    pub graph: DiGraph<CellNode, ()>,
    /// CellId → NodeIndex 映射
    cell_to_node: HashMap<CellId, NodeIndex>,
    /// 自上次 take_new_nodes 以来新建的节点(创建时 dirty=true),供 scheduler 增量入队
    new_nodes: Vec<CellId>,
}

impl FormulaDAG {
    pub fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            cell_to_node: HashMap::new(),
            new_nodes: Vec::new(),
        }
    }

    /// 获取或创建节点
    pub fn get_or_insert(&mut self, id: CellId) -> NodeIndex {
        if let Some(&idx) = self.cell_to_node.get(&id) {
            return idx;
        }
        let node = CellNode { id, running: false, dirty: true, cached: None };
        let idx = self.graph.add_node(node);
        self.cell_to_node.insert(id, idx);
        self.new_nodes.push(id);
        idx
    }

    /// 取走并清空"新建节点"日志(按创建顺序)。
    pub fn take_new_nodes(&mut self) -> Vec<CellId> {
        std::mem::take(&mut self.new_nodes)
    }

    /// 注册依赖关系：target 依赖 dep（dep 变化 → target 需要重算）
    pub fn add_dependency(&mut self, dep: CellId, target: CellId) {
        let dep_idx    = self.get_or_insert(dep);
        let target_idx = self.get_or_insert(target);
        // 避免重复边
        if !self.graph.contains_edge(dep_idx, target_idx) {
            self.graph.add_edge(dep_idx, target_idx, ());
        }
    }

    /// 移除某个 cell 的所有依赖关系（cell 公式改变时调用）
    pub fn clear_dependencies(&mut self, target: CellId) {
        if let Some(&idx) = self.cell_to_node.get(&target) {
            // 移除所有指向 target 的入边（target 不再依赖任何人）
            let incoming: Vec<_> = self.graph
                .edges_directed(idx, petgraph::Direction::Incoming)
                .map(|e| e.id())
                .collect();
            for eid in incoming {
                self.graph.remove_edge(eid);
            }
        }
    }

    /// 局部可达性检测：判断从 from 出发，沿依赖边（dep→target 方向）
    /// 能否走到 to。用于建立新依赖边 dep→target 之前判断"这条边会不会
    /// 成环"——若 target 已经能走到 dep（即 has_path(target, dep)），
    /// 说明加上 dep→target 后会形成环，此时不应该真的加这条边。
    ///
    /// v105 新增：修复跨批次动态建边导致的环漏检问题。用局部 DFS 而非
    /// 全图 kosaraju_scc，日常编辑场景下开销可忽略（图稀疏、路径浅）。
    pub fn has_path(&self, from: CellId, to: CellId) -> bool {
        use petgraph::visit::Dfs;
        let from_idx = match self.cell_to_node.get(&from) {
            Some(&idx) => idx,
            None => return false,
        };
        let to_idx = match self.cell_to_node.get(&to) {
            Some(&idx) => idx,
            None => return false,
        };
        let mut dfs = Dfs::new(&self.graph, from_idx);
        while let Some(nx) = dfs.next(&self.graph) {
            if nx == to_idx {
                return true;
            }
        }
        false
    }

    /// 返回当前图中所有 dirty 节点的 CellId（不保证顺序）。
    /// v105 新增：配合 scheduler 主循环支持"处理过程中动态出现的新
    /// dirty 节点也纳入本轮重算"（见 has_path 环拦截后 mark_dirty
    /// 产生的新脏节点）。
    pub fn dirty_cell_ids(&self) -> Vec<CellId> {
        self.graph
            .node_indices()
            .filter(|&nx| self.graph[nx].dirty)
            .map(|nx| self.graph[nx].id)
            .collect()
    }

    /// 标脏下游，但跳过 exclude 集合中的节点（不标记它们，但仍会
    /// 继续沿它们向更下游扩散——排除集合只影响"是否标记"，不影响
    /// BFS 的可达性遍历本身）。
    ///
    /// v105 新增：配合环检测拦截使用。环上两个节点（self_id/dep_id）
    /// 被判定为 CircularRef 后不应该再被 mark_dirty 波及自身（否则
    /// 会被 scheduler 主循环当成普通脏节点重新 eval，重新走一遍
    /// clear_dependencies + 建边判断，把刚设置好的 CircularRef 结果
    /// 覆盖掉——这是 v105 早期修复版本踩到的坑）。只有它们的下游
    /// （即真正依赖这两个错误节点的其他 cell）才需要被标脏重算。
    pub fn mark_dirty_excluding(&mut self, start: CellId, exclude: &std::collections::HashSet<CellId>) {
        use petgraph::visit::Bfs;
        let start_idx = match self.cell_to_node.get(&start) {
            Some(&idx) => idx,
            None => return,
        };
        let mut bfs = Bfs::new(&self.graph, start_idx);
        while let Some(nx) = bfs.next(&self.graph) {
            let id = self.graph[nx].id;
            if exclude.contains(&id) {
                continue;
            }
            self.graph[nx].dirty = true;
        }
    }

    /// 标脏：把 cell 及其所有后继（传递依赖）都标为 dirty
    pub fn mark_dirty(&mut self, id: CellId) {
        use petgraph::visit::Bfs;
        let start = match self.cell_to_node.get(&id) {
            Some(&idx) => idx,
            None => return,
        };
        let mut bfs = Bfs::new(&self.graph, start);
        while let Some(nx) = bfs.next(&self.graph) {
            self.graph[nx].dirty = true;
        }
    }

    /// 收集所有 dirty 的 cell，按拓扑序返回。
    /// 若图中存在环：返回 Err，携带该环上所有 dirty 节点的 CellId
    /// （由调用方 scheduler 将这些节点标记为 CellError::CircularRef，
    /// 而不是静默跳过整个重算——v104 修复）。
    pub fn dirty_topo_order(&self) -> Result<Vec<CellId>, Vec<CellId>> {
        use petgraph::algo::{kosaraju_scc, toposort};
        match toposort(&self.graph, None) {
            Ok(order) => Ok(order
                .into_iter()
                .filter(|&nx| self.graph[nx].dirty)
                .map(|nx| self.graph[nx].id)
                .collect()),
            Err(_cycle) => {
                // toposort 只告诉我们"有环"，不直接给环的完整节点集合。
                // 用 kosaraju_scc 找强连通分量，size > 1 的分量就是环
                // （size == 1 且无自环的分量不是环）。
                let sccs = kosaraju_scc(&self.graph);
                let mut cyclic_ids = Vec::new();
                for scc in sccs {
                    let is_cycle = scc.len() > 1
                        || (scc.len() == 1 && self.graph.contains_edge(scc[0], scc[0]));
                    if is_cycle {
                        // v105 修复：环上所有节点都必须报告为 CircularRef，
                        // 不能再按 dirty 过滤——dirty 是"是否需要重算"的
                        // 性能标记，跨调用建环时，环上先前已求值过的节点
                        // dirty 早被 set_cached 清成 false，若仍按 dirty
                        // 过滤会导致这些节点被漏报（cached 停留在成环前的
                        // 旧值），见 roe-api test_execute_script_circular_
                        // reference_does_not_panic 的失败案例。
                        for nx in scc {
                            cyclic_ids.push(self.graph[nx].id);
                        }
                    }
                }
                Err(cyclic_ids)
            }
        }
    }

    /// 循环引用检测：DFS running flag
    pub fn set_running(&mut self, id: CellId, running: bool) {
        if let Some(&idx) = self.cell_to_node.get(&id) {
            self.graph[idx].running = running;
        }
    }

    pub fn is_running(&self, id: CellId) -> bool {
        self.cell_to_node
            .get(&id)
            .map(|&idx| self.graph[idx].running)
            .unwrap_or(false)
    }

    /// 写入缓存结果，清除 dirty
    pub fn set_cached(&mut self, id: CellId, value: CellValue) {
        if let Some(&idx) = self.cell_to_node.get(&id) {
            self.graph[idx].cached = Some(value);
            self.graph[idx].dirty = false;
        }
    }

    pub fn get_cached(&self, id: CellId) -> Option<&CellValue> {
        self.cell_to_node
            .get(&id)
            .and_then(|&idx| self.graph[idx].cached.as_ref())
    }

    /// ROOT-012 修复配套：查询某 cell 当前是否 dirty。供 scheduler.rs 的
    /// 递归求值 hook 判断"这个依赖是否已经有可以直接复用的新鲜缓存"，
    /// 避免同一个依赖在一次 recalc 内被重复求值多次。
    /// 节点不存在（从未 get_or_insert 过）视为不 dirty——没有这个节点，
    /// 也就没有"需要被算"这回事，交给调用方的 get_cached 走 None 分支。
    pub fn is_dirty(&self, id: CellId) -> bool {
        self.cell_to_node
            .get(&id)
            .map(|&idx| self.graph[idx].dirty)
            .unwrap_or(false)
    }

    pub fn node_index(&self, id: CellId) -> Option<NodeIndex> {
        self.cell_to_node.get(&id).copied()
    }
}

impl Default for FormulaDAG {
    fn default() -> Self { Self::new() }
}
