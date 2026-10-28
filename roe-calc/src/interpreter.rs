//! InterpError：公式求值过程中的错误类型
//! 求值本身已迁移到 formula_engine（formualizer 适配层，见 scheduler.rs 调用）；
//! 本文件只保留 scheduler.rs 仍在使用的 InterpError 类型。
//! 旧的字符串扫描实现（FormulaInterpreter/EvalContext/DagContext）已删除，
//! 备份见 interpreter.rs.bak_cleanup。

use crate::dag::CellId;
use roe_model::calc::CellError;

#[derive(Debug, thiserror::Error)]
pub enum InterpError {
    #[error("circular reference at {0:?}")]
    CircularRef(CellId),
    /// v105 新增：eval 过程中动态建边检测到的跨批次循环引用，携带
    /// 环上两个节点的 CellId（self_id, dep_id），供 scheduler 主循环
    /// 精确锁定这两者、只级联标脏它们的下游（不含自身）。
    #[error("circular reference detected between {0:?} and {1:?}")]
    CircularRefPair(CellId, CellId),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("cell error: {0:?}")]
    CellErr(CellError),
}
