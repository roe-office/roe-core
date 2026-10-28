//! roe-model：统一内存模型（纯数据结构，无业务逻辑）
//! 所有 ODF/OOXML 标签语义映射到此层
//! Rhai API、Layout、Formula、Animation 都从此层派生

pub mod writer;
pub mod calc;
pub mod object_ids;
pub mod object_edit;
pub mod object_list;
pub mod impress;
pub mod stats;
pub mod stat_chart;
pub mod style;
pub mod common;
pub mod pivot_layout;
/// PPTX-11: 预设几何形状 fmla 公式求值系统。见模块内文档注释。
pub mod preset_geometry;
pub mod table_style;
pub mod table_styles;
pub mod excel_table_styles;
pub mod excel_table_styles_data;
