//! ROOT-078：`eval_hook` 依赖注入架构。
//!
//! `roe-core`（渲染层）需要在条件格式场景下求值 Excel 公式片段
//! （cfvo 的 formula/num 类型阈值、条件格式规则的 expression 类型），
//! 但求值能力在 `roe-calc`。`roe-calc` 的 Cargo.toml 声明了对
//! `roe-core` 的依赖（早期搭架子遗留，实际代码零引用），若 `roe-core`
//! 反过来直接依赖 `roe-calc` 在当前时点技术上可行（没有真实的循环），
//! 但会让两者的依赖方向长期纠缠：任何一次 `roe-calc` 未来真的用上
//! 那条声明的 `roe-core` 依赖，就会立刻编译失败且难以拆解。
//!
//! 因此采用依赖注入：`roe-core` 只定义"我需要什么能力"的 trait，
//! 不知道 `roe-calc` 的存在；具体实现（`CachedEvaluator`，内含公式
//! AST 解析缓存）放在 `roe-calc` 侧，由更上层（`roe-api`，本来就同时
//! 依赖两者）在构造渲染器时注入进来。
//!
//! 用 trait 而不是裸 `dyn Fn` 闭包：闭包无法优雅地携带内部状态
//! （AST 缓存需要 `RefCell<HashMap<..>>`），trait 对象可以。

use roe_model::calc::{CellValue, Sheet, Workbook};

/// 渲染层公式求值能力的依赖注入接口。
///
/// 实现方（`roe-calc::CachedEvaluator`）内部持有对 `Workbook` 的借用
/// 及公式 AST 解析缓存；调用方（`roe-core` 内部的条件格式渲染代码）
/// 只管传公式字符串，不关心求值细节或缓存策略。
///
/// 返回值统一用 `CellValue`（`roe_model` 里已有的通用值类型），调用方
/// 按场景需要的具体类型做模式匹配：
/// - cfvo 数值阈值场景：期望 `CellValue::Number`，匹配失败按现有的
///   data_min/data_max 字面量兜底处理（不 panic，退化为已知行为）。
/// - expression 条件格式场景：期望 `CellValue::Bool`；Excel 的
///   expression 规则语义上是"true则命中"，若求值结果是数字，非零也
///   应视为 true（沿用 Excel 一般"数字转布尔"的惯例）；求值出错
///   （`CellValue::Error`）按不命中处理。
pub trait FormulaEvaluator {
    /// 对 `formula`（不含前导 `=`，与项目内 `CellValue::Formula::source`
    /// 的约定一致）在给定 sheet 上下文中求值。
    ///
    /// `cell_ctx`：触发这次求值的单元格坐标（0-based row, col），用于
    /// 公式里可能出现的、依赖"当前单元格位置"的相对引用/结构化引用
    /// 语义。cfvo 阈值这类场景通常没有明确的"当前单元格"，传 `None`。
    ///
    /// `anchor_cell`：ROOT-078附带发现 —— 条件格式 expression 规则的
    /// 公式文本是相对于 sqref 范围锚点单元格（范围左上角）写的，对范围
    /// 内其他单元格求值时，需要知道锚点坐标才能把公式里的相对引用
    /// （非 `$` 绝对引用）按 (cell_ctx - anchor_cell) 的行列差整体平移。
    /// 与 `cell_ctx` 是两个独立概念：`cell_ctx` 回答"公式在哪个单元格
    /// 被求值"，`anchor_cell` 回答"公式文本本来是写给哪个单元格的"。
    /// 非条件格式场景（如cfvo阈值、普通公式求值）没有锚点概念，传
    /// `None`，此时不做任何相对引用偏移，与偏移前行为完全一致。
    fn evaluate(&self, formula: &str, workbook: &Workbook, sheet: &Sheet, cell_ctx: Option<(u32, u32)>, anchor_cell: Option<(u32, u32)>) -> CellValue;
}

/// 便捷方法：`evaluate` 结果直接取数值。cfvo 阈值场景使用。
pub fn eval_as_f64(
    evaluator: &dyn FormulaEvaluator,
    formula: &str,
    workbook: &Workbook,
    sheet: &Sheet,
) -> Option<f64> {
    match evaluator.evaluate(formula, workbook, sheet, None, None) {
        CellValue::Number(n) => Some(n),
        _ => None,
    }
}

/// 便捷方法：`evaluate` 结果直接取布尔。expression 条件格式场景使用。
/// 数字非零视为 true（沿用 Excel"数字转布尔"惯例）；求值出错/其余
/// 类型一律按不命中（false）处理，不 panic。
pub fn eval_as_bool(
    evaluator: &dyn FormulaEvaluator,
    formula: &str,
    workbook: &Workbook,
    sheet: &Sheet,
    cell_ctx: Option<(u32, u32)>,
    anchor_cell: Option<(u32, u32)>,
) -> bool {
    match evaluator.evaluate(formula, workbook, sheet, cell_ctx, anchor_cell) {
        CellValue::Bool(b) => b,
        CellValue::Number(n) => n != 0.0,
        _ => false,
    }
}
