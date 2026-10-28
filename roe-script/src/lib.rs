//! roe-script：Rhai 绑定层
//! 操作目标：ROE 内存模型（不再操作 XML 字符串）
//!
//! 模块：
//!   calc_api   — Workbook 的 get/set cell/formula
//!   writer_api — WriterDocument 的 get/set paragraph
//!   impress_api— Presentation 的 get/set shape text
//!   media_context — 媒体资产读取能力的桥接 trait（供 impress_api
//!                   的 set_slide_background picture 分支使用，见
//!                   ROE_Agent_Workspace_设计文档_v1.md）
//!   index      — extractModelIndex → JSON 拓扑（隐私保护：只含结构）
//!   engine     — ScriptEngine 门面

pub mod calc_api;
pub mod calc_api_meta;
pub mod writer_api_meta;
pub mod impress_api_meta;
#[cfg(test)]
mod impress_meta_test;
#[cfg(test)]
mod writer_meta_test;
pub mod calc_df_meta;
mod calc_api_list;
mod calc_api_pivot;
mod api_notes;

pub mod calc_df_api;
pub mod writer_api;
pub mod impress_api;
pub mod media_context;
pub mod index;
pub mod engine;

pub use engine::ScriptEngine;

pub mod script_kind;
