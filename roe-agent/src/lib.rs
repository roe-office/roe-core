//! roe-agent —— ROE 的 AI Agent 层（设计阶段占位，未开始实现）
//!
//! ═══════════════════════════════════════════════════════════════
//! 背景（2026-07-14 讨论，对应交接文档 v116）
//! ═══════════════════════════════════════════════════════════════
//!
//! ROE_ARCH_v4.md 第9章原设计的"AI + Rhai 操作层"是：
//!   AI 生成 Rhai 脚本 → executeShadowScript/executeModelScript 执行
//!   → 内存模型变更 → tile 渲染 → 用户确认
//! 这套设计的前提是"人在回路里"：AI 生成一步，人看一眼，再继续。
//!
//! 本次讨论的新方向：ROE 本身是"激进无头"内核，天然适合被 agent
//! 当工具调用，不需要人在中间点确认。目标场景例如"分析这批数据
//! 给我看结论"、"帮我做十份 PPT"这类需要多步骤规划 + 自主执行 +
//! 自我纠错的复合任务，而不只是"生成一段脚本"。
//!
//! ═══════════════════════════════════════════════════════════════
//! 技术选型：rig（https://github.com/0xplaygrounds/rig）
//! ═══════════════════════════════════════════════════════════════
//!
//! 已浅克隆到 ~/openlib/rig/ 供设计阶段参考代码，尚未作为依赖引入
//! 本项目。Rust 生态目前可用的 agent 框架只有两个，rig 是其中之一。
//! 选型立场：走 ArchLinux 路线——不求十年不动，跟随生态最新演进，
//! 若未来 rig 停滞或有更优选择，接受迁移成本，直接跟着升级或更换。
//!
//! rig 是多 crate workspace（~36M，含大量向量数据库/RAG 基础设施），
//! 本项目只需要 rig-core 一个 crate，引入方式（未来落地时）：
//!   rig-core = { git = "https://github.com/0xplaygrounds/rig", rev = "..." }
//! Cargo 只会编译 rig-core 声明的依赖树，rig-qdrant/rig-milvus 等
//! 向量数据库 crate 是 workspace 兄弟 crate、不是 rig-core 的依赖，
//! 不会被传递引入，无需担心体积膨胀。
//!
//! rig-core 关键模块与用途对照（已读源码确认，未深入测试）：
//!   - tool/     Tool trait：const NAME + Args/Output/Error 关联类型
//!               + description() + parameters() (JSON Schema)
//!               + async fn call(&self, args) -> Result<Output, Error>
//!               —— 结构上与 ROE C-ABI 函数（输入明确、输出明确）
//!               对应关系自然，可以直接包装。
//!   - agent/    Agent 主循环：多轮工具调用调度、何时停止
//!   - providers/  各家 LLM API 协议适配（Anthropic/OpenAI/Gemini等）
//!   - extractor.rs  结构化输出提取（让模型稳定产出 JSON）
//!   - streaming.rs  流式响应协议
//!   - memory.rs     多轮对话上下文管理
//!   - tool/result.rs  ToolOutcome/ToolFailure：工具调用结果的
//!               结构化错误分类（成功/失败/跳过/被拒绝）
//!
//! ═══════════════════════════════════════════════════════════════
//! 关键架构决策（已达成共识）
//! ═══════════════════════════════════════════════════════════════
//!
//! 1. Tool 封装应直接包 roe-script / roe-api 里的 Rust 函数，
//!    不要跨 FFI 边界调用 C-ABI。C-ABI 是给非 Rust 语言消费者
//!    （如前端绑定）准备的边界；roe-agent 是纯 Rust 组件，
//!    直接调用 Rust 内部函数即可，避免多余的序列化/FFI 开销。
//!
//! 2. roe-agent 作为独立 crate（而非塞进 roe-script），因为职责不同：
//!    roe-script = Rhai 绑定内存模型 API（既有职责，不变）
//!    roe-agent  = agent 循环 + tool 封装 + 与 rig 打交道（新职责）
//!    roe-agent 可能依赖 roe-api 和/或 roe-script，具体依赖关系
//!    留待详细设计时确定。
//!
//! ═══════════════════════════════════════════════════════════════
//! 尚未讨论 / 待后续会话展开
//! ═══════════════════════════════════════════════════════════════
//!
//! - Agent 循环具体怎么跟 office-rs2 前端拼接（谁发起、结果怎么
//!   流回前端、要不要暴露新的 C-ABI 给前端展示 agent 执行过程）
//! - 需要暴露哪些 ROE 能力作为 Tool（初步候选：roe_execute_script、
//!   roe_pivot_sheet、文档打开/保存、渲染预览等，需逐一梳理）
//! - rig-core 实际引入的版本/rev 锁定策略
//! - 是否需要 RAG 能力（若未来需要"语义搜索历史文档"，
//!   再评估 rig-fastembed / 向量数据库 crate 的引入）
//! - 与现有影子副本机制（activateShadow/commitShadow/discardShadow）
//!   的关系：agent 自主执行时，撤销/确认机制如何设计
//!
//! 本文件为设计阶段占位，无实际实现代码。


pub mod workspace;
pub mod context;
pub mod executor;
pub mod worker;
pub mod tool_doc;
pub mod tool;
pub mod prompt;
pub mod log_hook;
pub mod saved_path;
