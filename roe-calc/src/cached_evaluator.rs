//! ROOT-078 (eval_hook架构)：`roe-core::formula_hook::FormulaEvaluator`
//! trait 的具体实现，供 `roe-api` 组装后注入渲染管线。
//!
//! `roe-core` 只定义了"我需要一个能求值公式字符串的东西"（trait），
//! 不知道 `roe-calc` 的存在；这个文件是唯一知道两者都存在、把它们
//! 缝合起来的地方。这也是 `roe-calc/Cargo.toml` 里那条早期声明、
//! 长期零使用的 `roe-core` 依赖第一次被真正用上的地方（trait 定义
//! 在 roe-core，这里要 impl 它，必须依赖 roe-core 才能拿到 trait
//! 本身）。
//!
//! AST 解析缓存：`formualizer_parse::parser::ASTNode` 是可独立持有、
//! 不绑定 workbook 生命周期的纯语法树（parse 阶段不访问任何单元格
//! 数据），因此可以按"公式文本 -> AST"缓存，避免条件格式规则命中
//! 大量单元格时重复 parse 同一段公式文本。注意：缓存的必须是"改写
//! 前"的原始 AST——`eval_ast_with_resolver_and_cell` 内部会在每次
//! 调用时对 clone 出来的副本做结构化引用改写（依赖 current_cell，
//! 不同单元格改写结果不同），改写后的结果本身不缓存，正确性不会
//! 因为缓存而打折扣（见 formula_engine.rs 里 `eval_ast_with_resolver_and_cell`
//! 的文档注释）。

use std::cell::RefCell;
use std::collections::HashMap;

use formualizer_parse::parser::ASTNode;
use roe_core::formula_hook::FormulaEvaluator;
use roe_model::calc::{CellValue, Sheet, Workbook};

use crate::formula_engine::eval_ast_with_resolver_and_cell;
use crate::formula_engine::SheetResolver;

/// 带 AST 解析缓存的 `FormulaEvaluator` 实现。
///
/// 生命周期设计：`workbook: &'a Workbook` 借用整个渲染过程用到的
/// workbook；`ast_cache` 用 `RefCell` 做内部可变性（trait 方法签名
/// 是 `&self`，符合 Rust 惯例的"只读接口、内部状态自己管"）。
/// 一次渲染（`CalcTileRenderer::render_viewport_into` 单次调用）
/// 构造一个 `CachedEvaluator` 实例，渲染结束、实例析构，缓存自动
/// 释放——不需要额外的缓存失效/淘汰逻辑，因为它的生命周期本来就
/// 绑定到"这一次渲染任务"，不跨渲染任务持久化（workbook 内容如果
/// 在两次渲染之间发生变化，也不存在"缓存了旧AST但workbook已变"
/// 的数据不一致风险——AST 只跟公式文本本身有关，不跟 workbook 内容
/// 绑定，公式文本不变，AST 就不需要重新解析，这一点在两次渲染之间
/// 复用也是安全的，但当前先按"每次渲染新建一个实例"的简单模型来，
/// 避免过早引入跨渲染缓存复用的复杂度）。
pub struct CachedEvaluator<'a> {
    workbook: &'a Workbook,
    ast_cache: RefCell<HashMap<String, ASTNode>>,
}

impl<'a> CachedEvaluator<'a> {
    pub fn new(workbook: &'a Workbook) -> Self {
        Self {
            workbook,
            ast_cache: RefCell::new(HashMap::new()),
        }
    }

    /// 拿到（缓存命中）或解析（缓存未命中，解析后存入缓存）formula
    /// 对应的 AST。解析失败（语法错误的公式文本）返回 None，调用方
    /// 据此返回 `CellValue::Error`，不 panic。
    ///
    /// 缓存 key 用 formula 原始文本（未做 `=` 前缀归一化处理）——
    /// 归一化逻辑在 `formualizer_parse::parse_with_dialect` 调用前
    /// 完成，这里为了让缓存逻辑独立于 eval_ast_with_resolver_and_cell
    /// 的内部实现细节，直接在这里做一次归一化再 parse，两处
    /// 归一化逻辑（这里 + eval_formula_with_deps_with_resolver_and_cell
    /// 内部）目前重复，但各自只有一行，重复成本可接受，避免为了
    /// 去重而把 parse 逻辑拆得更零散。
    fn get_or_parse_ast(&self, formula: &str) -> Option<ASTNode> {
        if let Some(cached) = self.ast_cache.borrow().get(formula) {
            return Some(cached.clone());
        }
        let normalized = if formula.starts_with('=') {
            formula.to_string()
        } else {
            format!("={formula}")
        };
        // 条件格式场景（cfvo/expr）目前只处理 Excel 方言公式（xlsx 来源），
        // ODF 方言支持留给未来真的出现 ODF 条件格式公式场景时再补，
        // 不在本次范围内引入额外分支复杂度。
        let ast = formualizer_parse::parse_with_dialect(
            &normalized,
            formualizer_parse::FormulaDialect::Excel,
        ).ok()?;
        self.ast_cache.borrow_mut().insert(formula.to_string(), ast.clone());
        Some(ast)
    }
}

impl<'a> FormulaEvaluator for CachedEvaluator<'a> {
    fn evaluate(&self, formula: &str, workbook: &Workbook, sheet: &Sheet, cell_ctx: Option<(u32, u32)>, anchor_cell: Option<(u32, u32)>) -> CellValue {
        // 接口约定 workbook 由调用方传入（trait 签名要求），但本
        // struct 自身也持有一份 &'a Workbook——两者理应是同一个
        // workbook（roe-core 渲染循环里 self.workbook 和构造
        // CachedEvaluator 时传入的是同一个引用）。这里以 trait 方法
        // 参数传入的 workbook 为准做实际求值（更符合"调用方对上下文
        // 负责"的原则），self.workbook 只用于 AST 缓存生命周期管理，
        // 不用于求值本身。
        let ast = match self.get_or_parse_ast(formula) {
            Some(ast) => ast,
            None => return CellValue::Error(roe_model::calc::CellError::Name),
        };
        let resolver = SheetResolver::new(workbook, &sheet.name);
        let (value, _deps) = eval_ast_with_resolver_and_cell(resolver, &ast, cell_ctx, anchor_cell);
        value
    }
}
