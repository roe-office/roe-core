//! Agent 运行时的"当前活动文档"上下文——极薄的快照 Provider。
//!
//! ============================================================
//! 归属说明（本次会话从 office-rs2/agent-bridge 挪过来）
//! ============================================================
//! 最初这个类型定义在 office-rs2 侧的 agent-bridge crate 里
//! （因为最早只是给 GUI 用）。但 ExecuteScriptTool/QueryScriptTool
//! 需要在 Tool::call() 内部主动查询"当前活动文档是哪个 handle"，
//! 而 roe-agent 不能反过来依赖 agent-bridge（agent-bridge 依赖
//! slint，会让纯后端仓库 roe-agent 间接沾上 GUI 框架依赖，违反
//! 既定仓库边界原则）。
//!
//! 语义上，"Agent 工具调用时该操作哪个文档"本来就是 Agent 运行时
//! 契约的一部分，不是 GUI 专属概念，因此挪到这里定义更贴切。
//! office-rs2/agent-bridge 侧改为直接使用 roe_agent::context 导出
//! 的这几个类型，不再自己定义。
//!
//! 对应设计方案"五、Handle 获取机制"：不做复杂的多Tab感知框架，
//! 只用 Arc<RwLock<Option<...>>> 承载"当前唯一活动文档"的瞬间快照。
//!
//! - UI 线程：文档被替换时调用 set_active_doc 同步更新。
//! - Agent 后台线程：Tool 执行前调用 get_active_doc() 取瞬间快照，
//!   若为 None，应向 LLM 报告"当前未打开任何文档"，不得操作空 handle。
//!
//! 不使用 channel 查询式跨线程通信（会多一次异步往返）；
//! Arc<RwLock<...>> 是读多写少场景下更合适的选择。

use std::sync::{Arc, RwLock};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveDocumentSnapshot {
    /// 底层 ROE 原始句柄（doc-core 的 TileCacheRust::debug_handle()
    /// 返回值）。0 表示未打开文档（与 doc-core 内部惯例一致）。
    pub raw_handle: u64,
    /// 预留字段：当前单文档场景下取值方式待定，暂不参与任何业务
    /// 逻辑判断。留空字符串占位。
    pub doc_id: String,
}

/// 极薄的 Handle 观察者：后台 Agent task 只持有此 Provider 的 Clone。
#[derive(Clone, Default)]
pub struct AgentContextProvider {
    inner: Arc<RwLock<Option<ActiveDocumentSnapshot>>>,
}

impl AgentContextProvider {
    /// UI 线程调用：在替换文档句柄的同一时刻同步调用。
    pub fn set_active_doc(&self, snapshot: Option<ActiveDocumentSnapshot>) {
        if let Ok(mut guard) = self.inner.write() {
            *guard = snapshot;
        }
    }

    /// 后台 Agent Tool 调用前获取瞬间快照。
    pub fn get_active_doc(&self) -> Option<ActiveDocumentSnapshot> {
        self.inner.read().ok()?.clone()
    }
}
