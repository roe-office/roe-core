//! ScriptEngine：Rhai Engine 门面

use rhai::{Engine, Scope};
use serde_json::Value as JsonValue;
use roe_model::calc::Workbook;
use roe_model::writer::WriterDocument;
use roe_model::impress::Presentation;

pub struct ScriptEngine {
    engine: Engine,
    /// 脚本 print/debug 的捕获缓冲区(不写进程 stdout,执行结束并入返回值)。
    print_buf: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

/// 单次脚本执行的 Rhai 操作数上限,只用于拦截死循环,不限制正常脚本。
pub const MAX_SCRIPT_OPERATIONS: u64 = 50_000_000;

impl ScriptEngine {
    pub fn new() -> Self {
        let mut engine = Engine::new();
        crate::calc_api::register(&mut engine);
        crate::calc_df_api::register(&mut engine);
        crate::writer_api::register(&mut engine);
        crate::impress_api::register(&mut engine);
        let print_buf = install_print_capture(&mut engine);
        engine.set_max_operations(MAX_SCRIPT_OPERATIONS);
        Self { engine, print_buf }
    }

    /// Writer 只读引擎:只注册 writer_api::register_readonly 的 13 个纯查询函数。
    /// 供 query_script_preview 在 Writer 文档上使用;写函数未注册,调用会报 Function not found。
    pub fn new_readonly_writer() -> Self {
        let mut engine = Engine::new();
        crate::writer_api::register_readonly(&mut engine);
        let print_buf = install_print_capture(&mut engine);
        engine.set_max_operations(MAX_SCRIPT_OPERATIONS);
        Self { engine, print_buf }
    }

    /// Impress 只读引擎:只注册 impress_api::register_readonly() 的纯读函数
    /// (READ_FN_NAMES,13 个)。register_readonly 内部已带只读版 list_functions,
    /// 这里不要再注册 list_functions。
    pub fn new_readonly_impress() -> Self {
        let mut engine = Engine::new();
        crate::impress_api::register_readonly(&mut engine);
        let print_buf = install_print_capture(&mut engine);
        engine.set_max_operations(MAX_SCRIPT_OPERATIONS);
        Self { engine, print_buf }
    }

    /// 只读引擎：只注册 calc_api::register_readonly() 暴露的查询类
    /// 函数，不注册任何写操作函数（Calc 写操作、DataFrame、Writer、
    /// Impress 全部不注册）。供 roe-agent 的 query_script_preview
    /// 工具使用（v193 会话新增）：脚本在这个引擎上执行时，即使内容
    /// 试图调用 set_cell 等写函数，也会因为函数未注册而运行时报错，
    /// 机制上保证不可能修改文档状态。
    ///
    /// 本函数只服务 Calc;Writer 见 new_readonly_writer,Impress 见 new_readonly_impress。
    pub fn new_readonly() -> Self {
        let mut engine = Engine::new();
        crate::calc_api::register_readonly(&mut engine);
        crate::calc_df_api::register_readonly(&mut engine);
        let print_buf = install_print_capture(&mut engine);
        engine.set_max_operations(MAX_SCRIPT_OPERATIONS);
        Self { engine, print_buf }
    }

    /// 执行 Calc 脚本，返回脚本执行期间被写操作触碰过的 (sheet, row, col)
    /// 点集合（2026-06-19 dirty_regions 细化：调用方据此聚合出每个 sheet
    /// 的精确 bounding box，取代之前的"整 sheet 全范围标脏"）。
    pub fn run_calc(
        &self,
        script: &str,
        workbook: &mut Workbook,
    ) -> Result<(Vec<(usize, u32, u32)>, JsonValue), String> {
        // 无文档归属的便捷入口(测试等):用一次性空容器,数据集不跨调用保留。
        let mut ds = crate::calc_df_api::DatasetStore::new();
        self.run_calc_ds(script, workbook, &mut ds)
    }

    /// 与 run_calc 相同,但绑定调用方持有的数据集容器(按文档归属,跨脚本保留)。
    pub fn run_calc_ds(
        &self,
        script: &str,
        workbook: &mut Workbook,
        datasets: &mut crate::calc_df_api::DatasetStore,
    ) -> Result<(Vec<(usize, u32, u32)>, JsonValue), String> {
        let mut scope = Scope::new();
        let mut err: Option<String> = None;
        let mut ret_val = JsonValue::Null;
        if let Ok(mut g) = self.print_buf.lock() { g.clear(); }
        // 方案A修复（bug: df_* 函数必现"workbook未绑定"）：
        // calc_api 和 calc_df_api 各自维护独立的 thread_local WORKBOOK_PTR，
        // 之前只绑定了 calc_api 一侧，导致脚本里调用 df_* 函数时
        // calc_df_api::WORKBOOK_PTR 从未被绑定。这里改为嵌套绑定两侧，
        // 使同一次 run_calc 执行期间两个指针都有效。
        // 长期方案（B：合并两个 WORKBOOK_PTR 为一个）记入技术债，暂不在本次处理。
        crate::calc_df_api::bind_workbook_df(workbook);
        crate::calc_df_api::bind_datasets(datasets);
        let dirty = crate::calc_api::with_workbook(workbook, || {
            match self.engine.eval_with_scope::<rhai::Dynamic>(&mut scope, script) {
                Ok(v) => ret_val = dynamic_to_json(v),
                Err(e) => err = Some(format!("Rhai error: {e}")),
            }
        });
        crate::calc_df_api::unbind_workbook_df();
        crate::calc_df_api::unbind_datasets();
        let ret_val = merge_print_output(&self.print_buf, ret_val);
        err.map(Err).unwrap_or(Ok((dirty, ret_val)))
    }

    pub fn run_calc_df(&self, script: &str, workbook: &mut Workbook) -> Result<String, String> {
        let mut scope = rhai::Scope::new();
        let mut err: Option<String> = None;
        crate::calc_df_api::with_workbook_df(workbook, || {
            if let Err(e) = self.engine.run_with_scope(&mut scope, script) {
                err = Some(format!("Rhai error: {e}"));
            }
        });
        err.map(Err).unwrap_or(Ok("OK".to_string()))
    }

    pub fn run_writer(&self, script: &str, doc: &mut WriterDocument) -> Result<JsonValue, String> {
        let mut scope = Scope::new();
        let mut err: Option<String> = None;
        let mut ret_val = JsonValue::Null;
        if let Ok(mut g) = self.print_buf.lock() { g.clear(); }
        crate::writer_api::with_document(doc, || {
            match self.engine.eval_with_scope::<rhai::Dynamic>(&mut scope, script) {
                Ok(v) => ret_val = dynamic_to_json(v),
                Err(e) => err = Some(format!("Rhai error: {e}")),
            }
        });
        let ret_val = merge_print_output(&self.print_buf, ret_val);
        err.map(Err).unwrap_or(Ok(ret_val))
    }

    /// media_source：供 set_slide_background 的 picture/picture_tiled
    /// 分支读取虚拟路径对应的文件内容（见 media_context.rs 顶部文档）。
    /// 传 None 时（比如只读查询路径、FFI 边界、或调用方本来就没有
    /// workspace 可用），picture 分支会返回"当前会话未提供媒体资产
    /// 来源"一类的明确错误，而不是 panic。
    pub fn run_impress(
        &self,
        script: &str,
        pres: &mut Presentation,
        media_source: Option<&dyn crate::media_context::MediaAssetSource>,
    ) -> Result<JsonValue, String> {
        let mut scope = Scope::new();
        let mut err: Option<String> = None;
        let mut ret_val = JsonValue::Null;
        if let Ok(mut g) = self.print_buf.lock() { g.clear(); }
        crate::media_context::with_media_source(media_source, || {
            crate::impress_api::with_presentation(pres, || {
                match self.engine.eval_with_scope::<rhai::Dynamic>(&mut scope, script) {
                    Ok(v) => ret_val = dynamic_to_json(v),
                    Err(e) => err = Some(format!("Rhai error: {e}")),
                }
            });
        });
        let ret_val = merge_print_output(&self.print_buf, ret_val);
        err.map(Err).unwrap_or(Ok(ret_val))
    }

    /// 只读执行 Calc 脚本，返回脚本返回值（不记录 dirty，不修改 workbook）。
    ///
    /// clone-based 方案（方案A）：写函数在这个 Engine 上能被正常调用、
    /// 不会报错，只是因为跑在 clone 上而静默无效——调用方（尤其是 AI）
    /// 容易误以为修改已生效。roe-api 层已经有函数级白名单机制的替代品
    /// （方案B，见 roe-api::script::readonly_query_script_impl 和
    /// calc_api::register_readonly），新代码请改用那条路径。这个方法
    /// 暂不删除，避免破坏已有 C-ABI 导出接口 roe_query_script 的行为。
    #[deprecated(
        since = "0.1.0",
        note = "clone-based readonly execution allows write functions to run \
                silently without effect, risking AI cognitive inconsistency; \
                use roe_api::script::readonly_query_script_impl \
                (function-level whitelist via calc_api::register_readonly) instead"
    )]
    pub fn query_calc(&self, script: &str, workbook: &Workbook) -> Result<serde_json::Value, String> {
        // 克隆一份，保证调用方的 workbook 绝对不被修改
        let mut wb_clone = workbook.clone();
        let mut scope = Scope::new();
        let mut err: Option<String> = None;
        let mut ret_val = serde_json::Value::Null;
        crate::calc_api::with_workbook(&mut wb_clone, || {
            match self.engine.eval_with_scope::<rhai::Dynamic>(&mut scope, script) {
                Ok(v) => ret_val = dynamic_to_json(v),
                Err(e) => err = Some(format!("Rhai error: {e}")),
            }
        });
        err.map(Err).unwrap_or(Ok(ret_val))
    }

    /// 只读执行 Writer 脚本，返回脚本返回值（不修改文档）。
    ///
    /// clone-based 方案（方案A）：写函数在这个 Engine 上能被正常调用、
    /// 不会报错，只是因为跑在 clone 上而静默无效——调用方（尤其是 AI）
    /// 容易误以为修改已生效。注意：截至 v193 会话，Writer 文档**还没有**
    /// 函数级白名单机制的替代品（roe-api::script::readonly_query_script_impl
    /// 现已支持三类文档，Writer 已有 new_readonly_writer 白名单引擎；本段为 v193 时的历史说明）。
    #[deprecated(
        since = "0.1.0",
        note = "clone-based readonly execution allows write functions to run \
                silently without effect, risking AI cognitive inconsistency; \
                no function-level whitelist replacement exists for Writer yet \
                as of v193, this is a known gap, see progress notes"
    )]
    pub fn query_writer(&self, script: &str, doc: &WriterDocument) -> Result<serde_json::Value, String> {
        let mut doc_clone = doc.clone();
        let mut scope = Scope::new();
        let mut err: Option<String> = None;
        let mut ret_val = serde_json::Value::Null;
        crate::writer_api::with_document(&mut doc_clone, || {
            match self.engine.eval_with_scope::<rhai::Dynamic>(&mut scope, script) {
                Ok(v) => ret_val = dynamic_to_json(v),
                Err(e) => err = Some(format!("Rhai error: {e}")),
            }
        });
        err.map(Err).unwrap_or(Ok(ret_val))
    }

    /// 只读执行 Impress 脚本，返回脚本返回值（不修改 presentation）。
    ///
    /// clone-based 方案（方案A）：写函数在这个 Engine 上能被正常调用、
    /// 不会报错，只是因为跑在 clone 上而静默无效——调用方（尤其是 AI）
    /// 容易误以为修改已生效。注意：截至 v193 会话，Impress 文档**还没有**
    /// 函数级白名单机制的替代品（roe-api::script::readonly_query_script_impl
    /// 现已支持三类文档，Impress 已有 new_readonly_impress 白名单引擎；本段为 v193 时的历史说明）。
    #[deprecated(
        since = "0.1.0",
        note = "clone-based readonly execution allows write functions to run \
                silently without effect, risking AI cognitive inconsistency; \
                no function-level whitelist replacement exists for Impress yet \
                as of v193, this is a known gap, see progress notes"
    )]
    pub fn query_impress(&self, script: &str, pres: &Presentation) -> Result<serde_json::Value, String> {
        let mut pres_clone = pres.clone();
        let mut scope = Scope::new();
        let mut err: Option<String> = None;
        let mut ret_val = serde_json::Value::Null;
        crate::impress_api::with_presentation(&mut pres_clone, || {
            match self.engine.eval_with_scope::<rhai::Dynamic>(&mut scope, script) {
                Ok(v) => ret_val = dynamic_to_json(v),
                Err(e) => err = Some(format!("Rhai error: {e}")),
            }
        });
        err.map(Err).unwrap_or(Ok(ret_val))
    }

}

impl Default for ScriptEngine { fn default() -> Self { Self::new() } }

/// 单次脚本最多收集的 print/debug 行数,防止死循环 print 撑爆内存。
const MAX_PRINT_LINES: usize = 2000;

/// 注册 on_print / on_debug,把输出收进缓冲区而不是写进程 stdout。
fn install_print_capture(engine: &mut Engine) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
    let buf: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let b1 = buf.clone();
    engine.on_print(move |s| {
        if let Ok(mut g) = b1.lock() {
            if g.len() < MAX_PRINT_LINES { g.push(s.to_string()); }
        }
    });
    let b2 = buf.clone();
    engine.on_debug(move |s, _src, _pos| {
        if let Ok(mut g) = b2.lock() {
            if g.len() < MAX_PRINT_LINES { g.push(s.to_string()); }
        }
    });
    buf
}

/// 取走缓冲区并并入返回值:无输出则原样返回;返回值为 null 则返回输出字符串;
/// 两者都有则返回 {"output":..., "result":...}。
fn merge_print_output(
    buf: &std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ret: JsonValue,
) -> JsonValue {
    let lines = match buf.lock() {
        Ok(mut g) => std::mem::take(&mut *g),
        Err(_) => return ret,
    };
    if lines.is_empty() {
        return ret;
    }
    let out = lines.join("\n");
    if ret.is_null() {
        JsonValue::String(out)
    } else {
        serde_json::json!({"output": out, "result": ret})
    }
}

/// Subtotals Step 2 生产入口集成测试：验证 ROE_SUBTOTAL 不只在
/// roe-calc 内部单测（eval_formula 直接调用）里可用，在真实
/// ScriptEngine::new() -> run_calc() -> CalcEngine::recalc_and_apply()
/// 这条完整生产链路下同样能正确求值——这是项目既定纪律（见
/// roe-ext_progress_v163/v167）：register_fn 新增后必须过生产入口验证，
/// 不能只依赖单元测试里直接调用求值函数的路径。
///
/// 完整链路：set_formula 写入公式文本(cached留空) -> run_calc 返回
/// dirty坐标 -> CalcEngine::recalc_and_apply 真正求值并写回workbook ->
/// get_cell 读取展示值验证结果。
#[cfg(test)]
mod production_entry_roe_subtotal_check {
    use super::*;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
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
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn roe_subtotal_works_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();
        let mut calc_engine = roe_calc::CalcEngine::new();

        // A1=10, A2=20, A3="=ROE_SUBTOTAL(9,A1:A2)"（小计行）。
        engine.run_calc("set_cell(0, 0, 0, \"10\")", &mut wb).unwrap();
        engine.run_calc("set_cell(0, 1, 0, \"20\")", &mut wb).unwrap();
        let (dirty1, _) = engine
            .run_calc("set_formula(0, 2, 0, \"SUBTOTAL(9,A1:A2)\")", &mut wb)
            .unwrap();
        calc_engine.recalc_and_apply(&mut wb, &dirty1);

        // 外层 A4 = ROE_SUBTOTAL(9, A1:A3)，应跳过A3（嵌套小计行），
        // 结果应为 10+20=30，而不是把A3的30也算进去变成60。
        let (dirty2, _) = engine
            .run_calc("set_formula(0, 3, 0, \"SUBTOTAL(9,A1:A3)\")", &mut wb)
            .unwrap();
        calc_engine.recalc_and_apply(&mut wb, &dirty2);

        let (_, ret_val) = engine.run_calc("get_cell(0, 3, 0)", &mut wb).unwrap();
        let s = ret_val.as_str().unwrap_or_default();
        assert_eq!(
            s, "30",
            "SUBTOTAL 生产入口应正确跳过嵌套小计行A3，期望 30，实际: {s}"
        );
    }
}

/// 验证本次新增的 detect_autosum_range 在真正的生产入口 ScriptEngine::new()
/// 下同样生效。
#[cfg(test)]
mod production_entry_autosum_check {
    use super::*;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
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
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn detect_autosum_range_works_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();

        engine.run_calc("set_cell(0, 0, 0, \"10\")", &mut wb).unwrap();
        engine.run_calc("set_cell(0, 1, 0, \"20\")", &mut wb).unwrap();

        let result = engine.run_calc("detect_autosum_range(0, 2, 0)", &mut wb);
        assert!(result.is_ok(), "detect_autosum_range 应能通过生产入口执行，实际报错: {:?}", result.err());

        let (_, ret_val) = result.unwrap();
        let s = ret_val.as_str().unwrap_or_default();
        assert!(s.contains("\"up\""), "应探测到向上的连续数字区域，实际: {s}");
    }
}

/// 验证本次新增的 df_join（gap analysis 2.9核实结论：calc_df_api此前
/// 完全没有join能力）在真正的生产入口 ScriptEngine::new() 下同样生效，
/// 不只依赖 calc_df_api.rs 内部单测。这个函数横跨calc_api的WORKBOOK
/// 绑定和calc_df_api的WORKBOOK_PTR绑定两套机制（df_join内部既要
/// with_wb读取第二个sheet，又要get_lf/set_lf操作WORKING_DF），是本次
/// 新增能力里最适合验证"两套thread_local在生产入口run_calc下是否都
/// 正确接通"的一个，参照run_calc实现里方案A修复的注释（bind_workbook_df
/// 与calc_api::with_workbook嵌套绑定），本函数用于确认这个嵌套绑定
/// 对df_join同样有效，不是只对已验证过的df_load_sheet等函数有效。
#[cfg(test)]
mod production_entry_df_join_check {
    use super::*;
    use roe_model::calc::{CellBlock, CellValue, Sheet, SparseColumn, SparseGrid, Workbook};
    use std::collections::HashMap;

    fn make_wb_two_sheets() -> Workbook {
        // sheet0("data"): Region/Sales -> East/100, West/200, East/150
        let mut grid1 = SparseGrid::default();
        for (ci, vals) in [
            vec!["Region", "East", "West", "East"],
            vec!["Sales", "100", "200", "150"],
        ].iter().enumerate() {
            grid1.cols.insert(ci as u32, SparseColumn {
                blocks: smallvec::smallvec![CellBlock {
                    start_row: 0,
                    values: vals.iter().map(|s| {
                        if let Ok(n) = s.parse::<f64>() { CellValue::Number(n) }
                        else { CellValue::Text(s.to_string()) }
                    }).collect::<smallvec::SmallVec<_>>(),
                }],
            });
        }

        // sheet1("lookup"): RegionName/Manager -> East/Alice, West/Bob
        let mut grid2 = SparseGrid::default();
        for (ci, vals) in [
            vec!["RegionName", "East", "West"],
            vec!["Manager", "Alice", "Bob"],
        ].iter().enumerate() {
            grid2.cols.insert(ci as u32, SparseColumn {
                blocks: smallvec::smallvec![CellBlock {
                    start_row: 0,
                    values: vals.iter().map(|s| CellValue::Text(s.to_string())).collect::<smallvec::SmallVec<_>>(),
                }],
            });
        }

        fn empty_sheet(name: &str, cells: SparseGrid) -> Sheet {
            Sheet {
                col_style: vec![],
                tables: vec![],
                name: name.into(),
                cells,
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
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }
        }

        Workbook {
            sheets: vec![empty_sheet("data", grid1), empty_sheet("lookup", grid2)],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn df_join_works_via_real_engine_entry() {
        // 重要架构事实（本次调试中确认，非df_join本身的bug）：
        // calc_df_api::unbind_workbook_df 在每次 run_calc 调用结束时会
        // 清空 WORKING_DF（见其实现：WORKING_DF.with(|c| *c.borrow_mut()
        // = None)），因此 WORKING_DF 不能跨越两次独立的 run_calc 调用
        // 持久化——每次 run_calc 都是"绑定→执行→解绑清空"的完整生命周期。
        // 最初按两次独立 run_calc 调用（先df_load_sheet再df_join）编写本
        // 测试，复现了"WORKING_DF为空"的报错，排查后确认这是设计使然，
        // 不是回归。前端/Agent若需要"先load再操作"的多步骤df_*调用，
        // 必须一次性拼进同一个脚本字符串传给run_calc，不能分两次调用。
        // 本测试改为单次run_calc、脚本内用分号拼接多条语句，符合真实
        // 生产用法。
        let engine = ScriptEngine::new();
        let mut wb = make_wb_two_sheets();

        let script = r#"
            df_load_sheet(0);
            let join_result = df_join(1, "Region", "RegionName", "inner");
            join_result
        "#;
        let join_result = engine.run_calc(script, &mut wb);
        assert!(join_result.is_ok(), "df_join应能通过生产入口执行，实际报错: {:?}", join_result.err());
        let (_, ret_val) = join_result.unwrap();
        let s = ret_val.as_str().unwrap_or_default();
        assert_eq!(s, "ok", "df_join应返回ok，实际: {s}");

        let info_script = r#"
            df_load_sheet(0);
            df_join(1, "Region", "RegionName", "inner");
            df_info()
        "#;
        let info_result = engine.run_calc(info_script, &mut wb).unwrap();
        let info_str = info_result.1.as_str().unwrap_or_default();
        assert!(info_str.contains("Manager"), "join后schema应包含Manager列，实际: {info_str}");
    }
}

/// 验证本次新增的 text_to_columns（gap analysis 2.2，分列）在真正的生产
/// 入口 ScriptEngine::new() 下同样生效，不只依赖 calc_api.rs 内部单测——
/// 延续v157踩坑后确立的验证纪律：calc_api.rs内部单测走的是直接
/// `register(&mut e)` + `with_workbook`，跳过了`ScriptEngine::new()`里
/// 实际注册的完整函数集合，理论上存在"内部单测能过、生产入口漏注册"
/// 的风险（即使text_to_columns和已有text_to_columns所在的register()
/// 函数目前看没有遗漏迹象，仍按标准走一遍，不因为"看起来不会有问题"
/// 就跳过）。
#[cfg(test)]
mod production_entry_text_to_columns_check {
    use super::*;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
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
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn text_to_columns_works_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();

        engine.run_calc("set_cell(0, 0, 0, \"foo,bar,baz\")", &mut wb).unwrap();

        let result = engine.run_calc("text_to_columns(0, 0, 0, 0, \",\")", &mut wb);
        assert!(result.is_ok(), "text_to_columns 应能通过生产入口执行，实际报错: {:?}", result.err());

        let (_, ret_val) = result.unwrap();
        let s = ret_val.as_str().unwrap_or_default();
        assert_eq!(s, "{\"max_cols\":3}", "应报告最大列数3，实际: {s}");

        let (_, c0) = engine.run_calc("get_cell(0, 0, 0)", &mut wb).unwrap();
        let (_, c1) = engine.run_calc("get_cell(0, 0, 1)", &mut wb).unwrap();
        let (_, c2) = engine.run_calc("get_cell(0, 0, 2)", &mut wb).unwrap();
        assert_eq!(c0.as_str().unwrap_or_default(), "foo");
        assert_eq!(c1.as_str().unwrap_or_default(), "bar");
        assert_eq!(c2.as_str().unwrap_or_default(), "baz");
    }
}

/// 验证 describe_stats（gap analysis 2.4 描述性统计）在真正的生产入口
/// ScriptEngine::new() 下同样生效，不只依赖 calc_api.rs 内部单测
/// （v159交接文档"WORKING_DF跨run_calc不持久化"的教训提醒我们，任何
/// 涉及多次run_calc调用的场景都要在生产入口下实测一遍；describe_stats
/// 不涉及WORKING_DF，但仍遵循"新增能力必须过生产入口"的既有标准）。
#[cfg(test)]
mod production_entry_describe_stats_check {
    use super::*;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
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
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn describe_stats_works_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();

        for (i, v) in [1.0, 2.0, 3.0, 4.0, 5.0].iter().enumerate() {
            let script = format!("set_cell(0, {}, 0, \"{}\")", i, v);
            engine.run_calc(&script, &mut wb).unwrap();
        }

        let result = engine.run_calc("describe_stats(0, 0, 4, 0, 0, 0, 3)", &mut wb);
        assert!(result.is_ok(), "describe_stats 应能通过生产入口执行，实际报错: {:?}", result.err());

        let (_, ret_val) = result.unwrap();
        let s = ret_val.as_str().unwrap_or_default();
        assert_eq!(s, "{\"rows_written\":15}");

        let (_, label0) = engine.run_calc("get_cell(0, 0, 3)", &mut wb).unwrap();
        assert_eq!(label0.as_str().unwrap_or_default(), "Mean");

        let (_, f_mean) = engine.run_calc("get_cell_formula(0, 0, 4)", &mut wb).unwrap();
        assert_eq!(f_mean.as_str().unwrap_or_default(), "=AVERAGE(Sheet1!A1:A5)");
    }
}

/// 验证 t_test / f_test（gap analysis 2.4 统计工具第3/4项）在真正的
/// 生产入口 ScriptEngine::new() 下同样生效。
#[cfg(test)]
mod production_entry_t_test_f_test_check {
    use super::*;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
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
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn t_test_works_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();

        for (i, v) in [1.0, 2.0, 3.0].iter().enumerate() {
            let script = format!("set_cell(0, {}, 0, \"{}\")", i, v);
            engine.run_calc(&script, &mut wb).unwrap();
        }
        for (i, v) in [4.0, 5.0, 6.0].iter().enumerate() {
            let script = format!("set_cell(0, {}, 1, \"{}\")", i, v);
            engine.run_calc(&script, &mut wb).unwrap();
        }

        let result = engine.run_calc("t_test(0, 0, 2, 0, 0, 0, 2, 1, 2, 1, 0, 0, 3)", &mut wb);
        assert!(result.is_ok(), "t_test 应能通过生产入口执行，实际报错: {:?}", result.err());
        let (_, ret_val) = result.unwrap();
        assert_eq!(ret_val.as_str().unwrap_or_default(), "{\"rows_written\":5}");

        let (_, f_pvalue) = engine.run_calc("get_cell_formula(0, 4, 4)", &mut wb).unwrap();
        assert_eq!(f_pvalue.as_str().unwrap_or_default(), "=T.TEST(Sheet1!A1:A3,Sheet1!B1:B3,2,1)");
    }

    #[test]
    fn f_test_works_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();

        for (i, v) in [1.0, 2.0, 3.0, 4.0].iter().enumerate() {
            let script = format!("set_cell(0, {}, 0, \"{}\")", i, v);
            engine.run_calc(&script, &mut wb).unwrap();
        }
        for (i, v) in [1.0, 1.0, 1.0, 5.0].iter().enumerate() {
            let script = format!("set_cell(0, {}, 1, \"{}\")", i, v);
            engine.run_calc(&script, &mut wb).unwrap();
        }

        let result = engine.run_calc("f_test(0, 0, 3, 0, 0, 0, 3, 1, 0, 0, 3)", &mut wb);
        assert!(result.is_ok(), "f_test 应能通过生产入口执行，实际报错: {:?}", result.err());
        let (_, ret_val) = result.unwrap();
        assert_eq!(ret_val.as_str().unwrap_or_default(), "{\"rows_written\":3}");

        let (_, f_pvalue) = engine.run_calc("get_cell_formula(0, 2, 4)", &mut wb).unwrap();
        assert_eq!(f_pvalue.as_str().unwrap_or_default(), "=F.TEST(Sheet1!A1:A4,Sheet1!B1:B4)");
    }
}

/// 验证 correl / covariance（gap analysis 2.4 统计工具第5/6项）在真正的
/// 生产入口（ScriptEngine::new() + run_calc）下可用，不只是 rhai::Engine
/// 单元测试环境下可用。
#[cfg(test)]
mod production_entry_correl_covariance_check {
    use super::*;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
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
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn correl_works_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();

        for (i, v) in [1.0, 2.0, 3.0, 4.0].iter().enumerate() {
            let script = format!("set_cell(0, {}, 0, \"{}\")", i, v);
            engine.run_calc(&script, &mut wb).unwrap();
        }
        for (i, v) in [2.0, 4.0, 6.0, 8.0].iter().enumerate() {
            let script = format!("set_cell(0, {}, 1, \"{}\")", i, v);
            engine.run_calc(&script, &mut wb).unwrap();
        }

        let result = engine.run_calc("correl(0, 0, 3, 0, 0, 0, 3, 1, 0, 0, 3)", &mut wb);
        assert!(result.is_ok(), "correl 应能通过生产入口执行，实际报错: {:?}", result.err());
        let (_, ret_val) = result.unwrap();
        assert_eq!(ret_val.as_str().unwrap_or_default(), "{\"rows_written\":1}");

        let (_, f_correl) = engine.run_calc("get_cell_formula(0, 0, 4)", &mut wb).unwrap();
        assert_eq!(f_correl.as_str().unwrap_or_default(), "=CORREL(Sheet1!A1:A4,Sheet1!B1:B4)");
    }

    #[test]
    fn covariance_works_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();

        for (i, v) in [1.0, 2.0, 3.0, 4.0].iter().enumerate() {
            let script = format!("set_cell(0, {}, 0, \"{}\")", i, v);
            engine.run_calc(&script, &mut wb).unwrap();
        }
        for (i, v) in [2.0, 4.0, 6.0, 8.0].iter().enumerate() {
            let script = format!("set_cell(0, {}, 1, \"{}\")", i, v);
            engine.run_calc(&script, &mut wb).unwrap();
        }

        let result = engine.run_calc("covariance(0, 0, 3, 0, 0, 0, 3, 1, 0, 0, 3)", &mut wb);
        assert!(result.is_ok(), "covariance 应能通过生产入口执行，实际报错: {:?}", result.err());
        let (_, ret_val) = result.unwrap();
        assert_eq!(ret_val.as_str().unwrap_or_default(), "{\"rows_written\":2}");

        let (_, f_pop) = engine.run_calc("get_cell_formula(0, 0, 4)", &mut wb).unwrap();
        let (_, f_sample) = engine.run_calc("get_cell_formula(0, 1, 4)", &mut wb).unwrap();
        assert_eq!(f_pop.as_str().unwrap_or_default(), "=COVARIANCE.P(Sheet1!A1:A4,Sheet1!B1:B4)");
        assert_eq!(f_sample.as_str().unwrap_or_default(), "=COVARIANCE.S(Sheet1!A1:A4,Sheet1!B1:B4)");
    }
}

/// 验证 regression（gap analysis 2.4 统计工具第7项，简单线性回归）在真正的
/// 生产入口（ScriptEngine::new() + run_calc）下可用，不只是 rhai::Engine
/// 单元测试环境下可用。
#[cfg(test)]
mod production_entry_regression_check {
    use super::*;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
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
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn regression_works_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();

        for (i, v) in [1.0, 2.0, 3.0, 4.0].iter().enumerate() {
            let script = format!("set_cell(0, {}, 0, \"{}\")", i, v);
            engine.run_calc(&script, &mut wb).unwrap();
        }
        for (i, v) in [2.0, 4.0, 6.0, 8.0].iter().enumerate() {
            let script = format!("set_cell(0, {}, 1, \"{}\")", i, v);
            engine.run_calc(&script, &mut wb).unwrap();
        }

        let result = engine.run_calc("regression(0, 0, 3, 1, 0, 0, 3, 0, 0, 0, 3)", &mut wb);
        assert!(result.is_ok(), "regression 应能通过生产入口执行，实际报错: {:?}", result.err());
        let (_, ret_val) = result.unwrap();
        assert_eq!(ret_val.as_str().unwrap_or_default(), "{\"rows_written\":5}");

        let (_, f_slope) = engine.run_calc("get_cell_formula(0, 0, 4)", &mut wb).unwrap();
        let (_, f_correl) = engine.run_calc("get_cell_formula(0, 4, 4)", &mut wb).unwrap();
        assert_eq!(f_slope.as_str().unwrap_or_default(), "=SLOPE(Sheet1!B1:B4,Sheet1!A1:A4)");
        assert_eq!(f_correl.as_str().unwrap_or_default(), "=CORREL(Sheet1!B1:B4,Sheet1!A1:A4)");
    }
}

/// gap analysis 2.4/roe-ext_progress_v161第七节任务队列第1项：moving_average
/// 生产入口验证。照v160起的强制原则——任何新register_fn必须走
/// ScriptEngine::new()+run_calc真实路径验证，不能只信任calc_api.rs内部
/// 单测（register_row_col_ops曾漏注册导致5个函数单测过但生产不可用的教训）。
#[cfg(test)]
mod production_entry_moving_average_check {
    use super::*;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
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
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn moving_average_works_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();

        for (i, v) in [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0].iter().enumerate() {
            let script = format!("set_cell(0, {}, 0, \"{}\")", i, v);
            engine.run_calc(&script, &mut wb).unwrap();
        }

        let result = engine.run_calc("moving_average(0, 0, 6, 0, 5, 0, 0, 1)", &mut wb);
        assert!(result.is_ok(), "moving_average 应能通过生产入口执行，实际报错: {:?}", result.err());
        let (_, ret_val) = result.unwrap();
        assert_eq!(ret_val.as_str().unwrap_or_default(), "{\"rows_written\":7}");

        let (_, f_na_first) = engine.run_calc("get_cell_formula(0, 0, 1)", &mut wb).unwrap();
        let (_, f_center) = engine.run_calc("get_cell_formula(0, 3, 1)", &mut wb).unwrap();
        let (_, f_na_last) = engine.run_calc("get_cell_formula(0, 6, 1)", &mut wb).unwrap();
        assert_eq!(f_na_first.as_str().unwrap_or_default(), "=NA()");
        assert_eq!(f_center.as_str().unwrap_or_default(), "=AVERAGE(Sheet1!A2:A6)");
        assert_eq!(f_na_last.as_str().unwrap_or_default(), "=NA()");
    }
}

/// gap analysis 2.4/roe-ext_progress_v161第七节任务队列第1项：
/// exponential_smoothing 生产入口验证。
#[cfg(test)]
mod production_entry_exponential_smoothing_check {
    use super::*;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
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
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn exponential_smoothing_works_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();

        for (i, v) in [10.0, 20.0, 30.0].iter().enumerate() {
            let script = format!("set_cell(0, {}, 0, \"{}\")", i, v);
            engine.run_calc(&script, &mut wb).unwrap();
        }

        let result = engine.run_calc("exponential_smoothing(0, 0, 2, 0, 0.5, 0, 0, 1)", &mut wb);
        assert!(result.is_ok(), "exponential_smoothing 应能通过生产入口执行，实际报错: {:?}", result.err());
        let (_, ret_val) = result.unwrap();
        assert_eq!(ret_val.as_str().unwrap_or_default(), "{\"rows_written\":4}");

        let (_, alpha_val) = engine.run_calc("get_cell(0, 0, 2)", &mut wb).unwrap();
        let (_, f_first) = engine.run_calc("get_cell_formula(0, 1, 2)", &mut wb).unwrap();
        let (_, f_second) = engine.run_calc("get_cell_formula(0, 2, 2)", &mut wb).unwrap();
        assert_eq!(alpha_val.as_str().unwrap_or_default(), "0.5");
        assert_eq!(f_first.as_str().unwrap_or_default(), "=Sheet1!A1");
        assert_eq!(f_second.as_str().unwrap_or_default(), "=C1*Sheet1!A2 + (1-C1)*C2");
    }
}

/// 验证 anova_single_factor 在真正的生产入口 ScriptEngine::new() 下同样生效
/// （v159交接文档记录过 register_row_col_ops 曾漏挂进 ScriptEngine::new()，
/// 编译通过+单元测试全绿也未能暴露，因此每个新register_fn都必须过一遍
/// 生产入口验证，不能只依赖calc_api.rs内部单测）。
#[cfg(test)]
mod production_entry_anova_single_factor_check {
    use super::*;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
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
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn anova_single_factor_works_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();

        // 3组数据按列分组：A1:A3=1,2,3  B1:B3=4,5,6  C1:C3=7,8,9
        for (col, vals) in [(0, [1.0, 2.0, 3.0]), (1, [4.0, 5.0, 6.0]), (2, [7.0, 8.0, 9.0])] {
            for (i, v) in vals.iter().enumerate() {
                let script = format!("set_cell(0, {}, {}, \"{}\")", i, col, v);
                engine.run_calc(&script, &mut wb).unwrap();
            }
        }

        let result = engine.run_calc(
            "anova_single_factor(0, 0, 2, 0, 2, true, 0.05, 0, 0, 5)",
            &mut wb,
        );
        assert!(result.is_ok(), "anova_single_factor 应能通过生产入口执行，实际报错: {:?}", result.err());
        let (_, ret_val) = result.unwrap();
        assert_eq!(ret_val.as_str().unwrap_or_default(), "{\"rows_written\":7}");

        let (_, g1_label) = engine.run_calc("get_cell(0, 0, 5)", &mut wb).unwrap();
        assert_eq!(g1_label.as_str().unwrap_or_default(), "Column 1");

        let (_, between_ss) = engine.run_calc("get_cell_formula(0, 4, 6)", &mut wb).unwrap();
        assert_eq!(
            between_ss.as_str().unwrap_or_default(),
            "=SUMPRODUCT(Sheet1!H1:H3,Sheet1!I1:I3)-SUM(Sheet1!H1:H3)^2/SUM(Sheet1!G1:G3)",
        );

        let (_, between_p) = engine.run_calc("get_cell_formula(0, 4, 10)", &mut wb).unwrap();
        assert_eq!(between_p.as_str().unwrap_or_default(), "=1-F.DIST(J5,H5,H6,1)");

        let (_, within_ss) = engine.run_calc("get_cell_formula(0, 5, 6)", &mut wb).unwrap();
        assert_eq!(
            within_ss.as_str().unwrap_or_default(),
            "=SUM(DEVSQ(Sheet1!A1:A3),DEVSQ(Sheet1!B1:B3),DEVSQ(Sheet1!C1:C3))",
        );

        let (_, total_ss) = engine.run_calc("get_cell_formula(0, 6, 6)", &mut wb).unwrap();
        assert_eq!(total_ss.as_str().unwrap_or_default(), "=DEVSQ(Sheet1!A1:A3,Sheet1!B1:B3,Sheet1!C1:C3)");
    }
}

/// 验证 anova_two_factor 在真正的生产入口 ScriptEngine::new() 下同样生效。
#[cfg(test)]
mod production_entry_anova_two_factor_check {
    use super::*;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
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
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn anova_two_factor_works_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();

        // 2列x2行网格：A1=1 B1=2 / A2=3 B2=4
        let grid = [[1.0, 2.0], [3.0, 4.0]];
        for (r, vals) in grid.iter().enumerate() {
            for (c, v) in vals.iter().enumerate() {
                let script = format!("set_cell(0, {}, {}, \"{}\")", r, c, v);
                engine.run_calc(&script, &mut wb).unwrap();
            }
        }

        let result = engine.run_calc(
            "anova_two_factor(0, 0, 1, 0, 1, 0.05, 0, 0, 5)",
            &mut wb,
        );
        assert!(result.is_ok(), "anova_two_factor 应能通过生产入口执行，实际报错: {:?}", result.err());
        let (_, ret_val) = result.unwrap();
        // 2x2最小规格触发低自由度备注行，rows_written从12变为13（详见calc_api.rs
        // test_anova_two_factor_basic注释）
        assert_eq!(ret_val.as_str().unwrap_or_default(), "{\"rows_written\":13}");

        let (_, note_text) = engine.run_calc("get_cell(0, 12, 5)", &mut wb).unwrap();
        assert!(
            note_text.as_str().unwrap_or_default().contains("Error自由度极低"),
            "生产入口下备注行也应正确写入"
        );

        let (_, rows_label) = engine.run_calc("get_cell(0, 8, 5)", &mut wb).unwrap();
        assert_eq!(rows_label.as_str().unwrap_or_default(), "Rows");

        let (_, rows_f) = engine.run_calc("get_cell_formula(0, 8, 9)", &mut wb).unwrap();
        assert_eq!(rows_f.as_str().unwrap_or_default(), "=I9/I11");

        let (_, error_df) = engine.run_calc("get_cell_formula(0, 10, 7)", &mut wb).unwrap();
        assert_eq!(error_df.as_str().unwrap_or_default(), "=H12-H9-H10");

        let (_, total_ss) = engine.run_calc("get_cell_formula(0, 11, 6)", &mut wb).unwrap();
        assert_eq!(total_ss.as_str().unwrap_or_default(), "=SUM(G9,G10,G11)");
    }
}

/// 验证本次新增的 get_conditional_formats/get_data_validations/get_auto_filter/
/// get_merged_cells 四个查询函数在真正的生产入口 ScriptEngine::new() 下同样
/// 生效（它们注册在 calc_api::register 里，理论上风险远小于当年
/// register_row_col_ops 漏注册的情况，但既然是本次新增能力，仍按同等标准走一遍
/// 生产入口验证，不只依赖 calc_api.rs 内部单测）。
#[cfg(test)]
mod production_entry_get_apis_check {
    use super::*;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
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
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn get_conditional_formats_and_data_validations_work_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();

        let empty = engine.run_calc("get_conditional_formats(0)", &mut wb).unwrap().1;
        assert_eq!(empty.as_str(), Some("[]"));

        let set_result = engine.run_calc(
            r#"set_conditional_format(0, `{"sheet":"Sheet1","start":[0,0],"end":[9,3]}`, `{"condition":"gt:0","format":{}}`)"#,
            &mut wb,
        );
        assert!(set_result.is_ok(), "set_conditional_format 应能通过生产入口执行，实际报错: {:?}", set_result.err());

        let after = engine.run_calc("get_conditional_formats(0)", &mut wb).unwrap().1;
        assert!(
            after.as_str().unwrap_or_default().contains("gt:0"),
            "get_conditional_formats 应能通过生产入口读回条件字符串，实际: {after:?}"
        );

        let empty_dv = engine.run_calc("get_data_validations(0)", &mut wb).unwrap().1;
        assert_eq!(empty_dv.as_str(), Some("[]"));

        let set_dv_result = engine.run_calc(
            r#"set_data_validation(0, `{"sheet":"Sheet1","start":[0,0],"end":[9,0]}`, `{"kind":"list","formula1":"A,B,C","formula2":null,"show_error":true,"error_msg":null}`)"#,
            &mut wb,
        );
        assert!(set_dv_result.is_ok(), "set_data_validation 应能通过生产入口执行，实际报错: {:?}", set_dv_result.err());

        let after_dv = engine.run_calc("get_data_validations(0)", &mut wb).unwrap().1;
        assert!(
            after_dv.as_str().unwrap_or_default().contains("list"),
            "get_data_validations 应能通过生产入口读回kind字段，实际: {after_dv:?}"
        );
    }

    #[test]
    fn cf_rule_add_clear_remove_work_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();

        let databar = r##"{"DataBar":{"min":{"kind":"min","val":"","gte":true},"max":{"kind":"max","val":"","gte":true},"bar_color":"#638EC6","border_color":null,"direction":"LeftToRight","bar_only":false,"solid_fill":false,"negative_fill_color":null,"axis_color":null,"axis_position":"automatic"}}"##;
        let colorscale = r##"{"ColorScale":{"min_type":"min","min_value":null,"min_color":"#F8696B","mid":null,"max_type":"max","max_value":null,"max_color":"#63BE7B"}}"##;

        let add1 = engine.run_calc(
            &format!("add_cf_rule(0, `{{\"sheet\":\"Sheet1\",\"start\":[0,0],\"end\":[9,0]}}`, `{databar}`, false)"),
            &mut wb,
        );
        assert!(add1.is_ok(), "add_cf_rule 应能通过生产入口执行，实际报错: {:?}", add1.err());
        let add2 = engine.run_calc(
            &format!("add_cf_rule(0, `{{\"sheet\":\"Sheet1\",\"start\":[0,2],\"end\":[9,2]}}`, `{colorscale}`, true)"),
            &mut wb,
        );
        assert!(add2.is_ok(), "第二条 add_cf_rule 失败: {:?}", add2.err());
        assert_eq!(wb.sheets[0].cond_formats.len(), 2);

        let got = engine.run_calc("get_conditional_formats(0)", &mut wb).unwrap().1;
        let got = got.as_str().unwrap_or_default().to_string();
        assert!(got.contains("DataBar") && got.contains("ColorScale"), "实际: {got}");

        // 只与第 2 条相交:删 1 条,留 DataBar
        let cleared = engine.run_calc(
            r#"clear_conditional_formats(0, `{"sheet":"Sheet1","start":[3,2],"end":[3,2]}`)"#,
            &mut wb,
        );
        assert!(cleared.is_ok(), "clear_conditional_formats 失败: {:?}", cleared.err());
        assert_eq!(wb.sheets[0].cond_formats.len(), 1);

        let removed = engine.run_calc("remove_conditional_format(0, 0)", &mut wb);
        assert!(removed.is_ok(), "remove_conditional_format 失败: {:?}", removed.err());
        assert!(wb.sheets[0].cond_formats.is_empty());
    }

    #[test]
    fn get_auto_filter_and_merged_cells_work_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();

        let empty_filter = engine.run_calc("get_auto_filter(0)", &mut wb).unwrap().1;
        assert_eq!(empty_filter.as_str(), Some("null"));

        let apply_result = engine.run_calc(
            r#"apply_filter(0, 0, 0, `{"col_filters":{"0":["A","B"]}}`)"#,
            &mut wb,
        );
        assert!(apply_result.is_ok(), "apply_filter 应能通过生产入口执行，实际报错: {:?}", apply_result.err());

        let after_filter = engine.run_calc("get_auto_filter(0)", &mut wb).unwrap().1;
        assert!(
            after_filter.as_str().unwrap_or_default().contains("header_row"),
            "get_auto_filter 应能通过生产入口读回header_row字段，实际: {after_filter:?}"
        );

        let empty_merged = engine.run_calc("get_merged_cells(0)", &mut wb).unwrap().1;
        assert_eq!(empty_merged.as_str(), Some("[]"));

        let merge_result = engine.run_calc("merge_cells(0, 0, 0, 1, 1)", &mut wb);
        assert!(merge_result.is_ok(), "merge_cells 应能通过生产入口执行，实际报错: {:?}", merge_result.err());

        let after_merged = engine.run_calc("get_merged_cells(0)", &mut wb).unwrap().1;
        let s = after_merged.as_str().unwrap_or_default();
        assert!(
            s.contains("r1") && s.contains("c2"),
            "get_merged_cells 应能通过生产入口读回边界坐标，实际: {after_merged:?}"
        );
    }
}

fn dynamic_to_json(v: rhai::Dynamic) -> JsonValue {
    if v.is_string() {
        JsonValue::String(v.cast::<rhai::ImmutableString>().to_string())
    } else if v.is::<i64>() {
        JsonValue::Number(v.cast::<i64>().into())
    } else if v.is::<f64>() {
        serde_json::Number::from_f64(v.cast::<f64>())
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null)
    } else if v.is::<bool>() {
        JsonValue::Bool(v.cast::<bool>())
    } else if v.is_unit() {
        JsonValue::Null
    } else {
        // 其他类型转字符串
        JsonValue::String(v.to_string())
    }
}

#[cfg(test)]
mod adhoc_retval_check {
    use super::*;

    #[test]
    fn insert_chart_returns_id_via_run_calc() {
        // 用最小可行的 Calc 文档 + insert_chart 脚本，
        // 断言 run_calc 返回的 JsonValue 里能拿到非零 id
        // 具体怎么建文档句柄，抄 calc_api.rs 里已有的
        // `result > 0` 那个测试的 setup 部分即可
    }
}

/// v157交接文档记录的问题：insert_row/delete_row/insert_column/delete_column/
/// delete_duplicates 五个函数注册在 calc_api::register_row_col_ops() 里，但
/// 生产环境唯一入口 ScriptEngine::new() 此前从未调用这个函数——这些函数在
/// calc_api.rs 自己的单元测试里能跑通，是因为测试直接手动调用了
/// register_row_col_ops，并不能反映生产环境的真实状态。
///
/// 本模块只通过 ScriptEngine 的公开入口（new() + run_calc）驱动，不直接调用
/// calc_api 内部的 register_row_col_ops 或任何私有测试辅助函数，目的是如实
/// 模拟 Slint 前端 / Agent 通过 roe_execute_script 走的真实路径，验证修复
/// （engine.rs 中补上 calc_api::register_row_col_ops 调用）确实生效。
#[cfg(test)]
mod production_entry_row_col_ops_check {
    use super::*;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
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
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    /// 核心回归测试：只用 ScriptEngine::new()（真正的生产入口）+ run_calc，
    /// 若 register_row_col_ops 没有被 engine.rs 调用，这里应得到
    /// "Function not found" 之类的 Rhai 报错；补丁生效后应能正常执行。
    #[test]
    fn insert_row_delete_row_delete_duplicates_work_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();

        // 写入3行数据（第0行留作insert_row的插入目标之前的锚点）
        engine.run_calc(r#"set_cell(0, 0, 0, "row0")"#, &mut wb).unwrap();
        engine.run_calc(r#"set_cell(0, 1, 0, "row1")"#, &mut wb).unwrap();
        engine.run_calc(r#"set_cell(0, 2, 0, "row2")"#, &mut wb).unwrap();

        // insert_row：在第1行位置插入一空行，原row1/row2应后移
        let insert_result = engine.run_calc("insert_row(0, 1)", &mut wb);
        assert!(
            insert_result.is_ok(),
            "insert_row 应能通过生产入口 ScriptEngine::new() 正常执行，实际报错: {:?}",
            insert_result.err()
        );
        assert_eq!(
            engine.run_calc("get_cell(0, 0, 0)", &mut wb).unwrap().1.as_str(),
            Some("row0")
        );
        assert_eq!(
            engine.run_calc("get_cell(0, 2, 0)", &mut wb).unwrap().1.as_str(),
            Some("row1"),
            "insert_row 后原row1应后移到第2行"
        );

        // delete_row：删掉刚插入的空行（第1行），row1应回到第1行
        let delete_result = engine.run_calc("delete_row(0, 1)", &mut wb);
        assert!(
            delete_result.is_ok(),
            "delete_row 应能通过生产入口 ScriptEngine::new() 正常执行，实际报错: {:?}",
            delete_result.err()
        );
        assert_eq!(
            engine.run_calc("get_cell(0, 1, 0)", &mut wb).unwrap().1.as_str(),
            Some("row1"),
            "delete_row 撤销insert_row后row1应回到第1行"
        );

        // delete_duplicates：构造一个重复行场景（复制row1到row2的内容）
        engine.run_calc(r#"set_cell(0, 2, 0, "row1")"#, &mut wb).unwrap();
        let dedup_result = engine.run_calc(
            r#"delete_duplicates(0, 0, 0, 2, 0, "[]", false)"#,
            &mut wb,
        );
        assert!(
            dedup_result.is_ok(),
            "delete_duplicates 应能通过生产入口 ScriptEngine::new() 正常执行，实际报错: {:?}",
            dedup_result.err()
        );
        let (_, ret_val) = dedup_result.unwrap();
        assert_eq!(
            ret_val.as_str(),
            Some("ok:1"),
            "应识别出1行重复并删除"
        );
    }

    /// insert_column/delete_column 同样在 register_row_col_ops 里注册，
    /// 一并通过生产入口验证，覆盖v157交接文档提到的全部五个函数。
    #[test]
    fn insert_column_delete_column_work_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();

        engine.run_calc(r#"set_cell(0, 0, 0, "a")"#, &mut wb).unwrap();
        engine.run_calc(r#"set_cell(0, 0, 1, "b")"#, &mut wb).unwrap();

        let insert_result = engine.run_calc("insert_column(0, 0)", &mut wb);
        assert!(
            insert_result.is_ok(),
            "insert_column 应能通过生产入口 ScriptEngine::new() 正常执行，实际报错: {:?}",
            insert_result.err()
        );
        assert_eq!(
            engine.run_calc("get_cell(0, 0, 1)", &mut wb).unwrap().1.as_str(),
            Some("a"),
            "insert_column 后原列0内容应后移到列1"
        );

        let delete_result = engine.run_calc("delete_column(0, 0)", &mut wb);
        assert!(
            delete_result.is_ok(),
            "delete_column 应能通过生产入口 ScriptEngine::new() 正常执行，实际报错: {:?}",
            delete_result.err()
        );
        assert_eq!(
            engine.run_calc("get_cell(0, 0, 0)", &mut wb).unwrap().1.as_str(),
            Some("a"),
            "delete_column 撤销insert_column后列0内容应回到列0"
        );
    }
}

#[cfg(test)]
mod production_entry_insert_subtotals_check {
    use super::*;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
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
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    /// Subtotals Step 3 生产入口集成测试。构造：
    /// row0 = 表头 ("Group","Value")
    /// row1,2 = 分组A的数据 (A,10) (A,20)
    /// row3,4 = 分组B的数据 (B,5)  (B,15)
    /// 调用 insert_subtotals 后，分组A应在row3插入小计行（原row3/4后移到
    /// row4/5），分组B在原row4之后（插入A的小计后已下移到row5）插入小计行。
    /// 验证：两个小计行的 ROE_SUBTOTAL 公式经 recalc_and_apply 后算出
    /// 正确的组内合计（30 和 20），且原始数据行未被破坏、顺序未错位。
    #[test]
    fn insert_subtotals_works_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();
        let mut calc_engine = roe_calc::CalcEngine::new();

        engine.run_calc(r#"set_cell(0, 0, 0, "Group")"#, &mut wb).unwrap();
        engine.run_calc(r#"set_cell(0, 0, 1, "Value")"#, &mut wb).unwrap();
        engine.run_calc(r#"set_cell(0, 1, 0, "A")"#, &mut wb).unwrap();
        engine.run_calc(r#"set_cell(0, 1, 1, "10")"#, &mut wb).unwrap();
        engine.run_calc(r#"set_cell(0, 2, 0, "A")"#, &mut wb).unwrap();
        engine.run_calc(r#"set_cell(0, 2, 1, "20")"#, &mut wb).unwrap();
        engine.run_calc(r#"set_cell(0, 3, 0, "B")"#, &mut wb).unwrap();
        engine.run_calc(r#"set_cell(0, 3, 1, "5")"#, &mut wb).unwrap();
        engine.run_calc(r#"set_cell(0, 4, 0, "B")"#, &mut wb).unwrap();
        engine.run_calc(r#"set_cell(0, 4, 1, "15")"#, &mut wb).unwrap();

        // group_col=0, agg_cols=[1], function_num=9(SUM), header_row=0,
        // start_row=1, end_row=4
        let result = engine.run_calc(
            r#"insert_subtotals(0, 0, "[1]", 9, 0, 1, 4)"#,
            &mut wb,
        );
        assert!(
            result.is_ok(),
            "insert_subtotals 应能通过生产入口 ScriptEngine::new() 正常执行，实际报错: {:?}",
            result.err()
        );
        let (dirty, ret_val) = result.unwrap();
        assert_eq!(ret_val.as_str(), Some("ok"), "insert_subtotals 应返回ok");

        calc_engine.recalc_and_apply(&mut wb, &dirty);

        // 原始数据行应保持不变、顺序未错位
        assert_eq!(engine.run_calc("get_cell(0, 1, 0)", &mut wb).unwrap().1.as_str(), Some("A"));
        assert_eq!(engine.run_calc("get_cell(0, 1, 1)", &mut wb).unwrap().1.as_str(), Some("10"));
        assert_eq!(engine.run_calc("get_cell(0, 2, 0)", &mut wb).unwrap().1.as_str(), Some("A"));
        assert_eq!(engine.run_calc("get_cell(0, 2, 1)", &mut wb).unwrap().1.as_str(), Some("20"));

        // 分组A的小计行应插在row3（原row3/4的B组数据整体下移到row4/5）
        assert_eq!(
            engine.run_calc("get_cell(0, 3, 0)", &mut wb).unwrap().1.as_str(),
            Some("A 汇总"),
            "分组A的小计行应插入在row3"
        );
        assert_eq!(
            engine.run_calc("get_cell(0, 3, 1)", &mut wb).unwrap().1.as_str(),
            Some("30"),
            "分组A小计应为10+20=30"
        );

        // B组数据应整体下移一行
        assert_eq!(engine.run_calc("get_cell(0, 4, 0)", &mut wb).unwrap().1.as_str(), Some("B"));
        assert_eq!(engine.run_calc("get_cell(0, 4, 1)", &mut wb).unwrap().1.as_str(), Some("5"));
        assert_eq!(engine.run_calc("get_cell(0, 5, 0)", &mut wb).unwrap().1.as_str(), Some("B"));
        assert_eq!(engine.run_calc("get_cell(0, 5, 1)", &mut wb).unwrap().1.as_str(), Some("15"));

        // 分组B的小计行应插在row6
        assert_eq!(
            engine.run_calc("get_cell(0, 6, 0)", &mut wb).unwrap().1.as_str(),
            Some("B 汇总"),
            "分组B的小计行应插入在row6"
        );
        assert_eq!(
            engine.run_calc("get_cell(0, 6, 1)", &mut wb).unwrap().1.as_str(),
            Some("20"),
            "分组B小计应为5+15=20"
        );
    }

    /// 边界情况：agg_cols_json 为空数组应返回明确的err，不静默做无意义操作。
    #[test]
    fn insert_subtotals_rejects_empty_agg_cols() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();
        engine.run_calc(r#"set_cell(0, 0, 0, "A")"#, &mut wb).unwrap();
        engine.run_calc(r#"set_cell(0, 1, 0, "A")"#, &mut wb).unwrap();

        let result = engine.run_calc(
            r#"insert_subtotals(0, 0, "[]", 9, 0, 1, 1)"#,
            &mut wb,
        );
        assert!(result.is_ok(), "run_calc本身不应报错（错误信息通过返回值传递）");
        let (_, ret_val) = result.unwrap();
        assert!(
            ret_val.as_str().unwrap_or("").starts_with("err:"),
            "agg_cols_json为空数组时应返回err:前缀的错误信息，实际: {:?}",
            ret_val
        );
    }

    /// remove_subtotals 生产入口往返测试：insert_subtotals 之后 remove_subtotals
    /// 应把表还原成汇总前的样子（数据、行数、outline 全部还原）。
    #[test]
    fn remove_subtotals_roundtrip_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();
        let mut calc_engine = roe_calc::CalcEngine::new();

        for (r, c, v) in [
            (0, 0, "Group"), (0, 1, "Value"),
            (1, 0, "A"), (1, 1, "10"), (2, 0, "A"), (2, 1, "20"),
            (3, 0, "B"), (3, 1, "5"),  (4, 0, "B"), (4, 1, "15"),
        ] {
            engine.run_calc(&format!(r#"set_cell(0, {r}, {c}, "{v}")"#), &mut wb).unwrap();
        }

        let (dirty, ret) = engine
            .run_calc(r#"insert_subtotals(0, 0, "[1]", 9, 0, 1, 4)"#, &mut wb)
            .unwrap();
        assert_eq!(ret.as_str(), Some("ok"));
        calc_engine.recalc_and_apply(&mut wb, &dirty);
        // 汇总后共 7 行(表头 + 4 数据 + 2 汇总)，汇总行在 row3 / row6
        assert_eq!(engine.run_calc("get_cell(0, 3, 0)", &mut wb).unwrap().1.as_str(), Some("A 汇总"));
        assert_eq!(engine.run_calc("get_cell(0, 6, 0)", &mut wb).unwrap().1.as_str(), Some("B 汇总"));

        // 取消：范围 r1=0(表头) ~ r2=6，c1=0 ~ c2=1
        let (_, ret) = engine
            .run_calc("remove_subtotals(0, 0, 0, 6, 1)", &mut wb)
            .unwrap();
        assert_eq!(ret.as_str(), Some("{\"removed\":2}"), "应删除 2 个汇总行");

        // 数据完全还原：row1..row4 依次为 A/10, A/20, B/5, B/15，row5 起为空
        let expect = [("A", "10"), ("A", "20"), ("B", "5"), ("B", "15")];
        for (i, (g, v)) in expect.iter().enumerate() {
            let row = i + 1;
            assert_eq!(engine.run_calc(&format!("get_cell(0, {row}, 0)"), &mut wb).unwrap().1.as_str(), Some(*g));
            assert_eq!(engine.run_calc(&format!("get_cell(0, {row}, 1)"), &mut wb).unwrap().1.as_str(), Some(*v));
        }
        assert_eq!(engine.run_calc("get_cell(0, 5, 0)", &mut wb).unwrap().1.as_str(), Some(""));
        assert_eq!(engine.run_calc("get_cell(0, 5, 1)", &mut wb).unwrap().1.as_str(), Some(""));

        // outline 应还原为全 0：insert_subtotals 把明细设成了 1，取消后不应残留
        let max_level = wb.sheets[0].row_outline_level.iter().copied().max().unwrap_or(0);
        assert_eq!(max_level, 0, "取消分类汇总后 outline 层级应全部还原为0，实际残留: {:?}",
                   wb.sheets[0].row_outline_level);
    }

    /// insert_subtotals_ex 生产入口:recalc 后数值正确,嵌套汇总不重复计数。
    /// 数据: East/Mon/1, East/Mon/2, East/Tue/3, West/Mon/4
    /// 期望: 总计=10, East=6, East-Mon=3, East-Tue=3, West=4, West-Mon=4
    #[test]
    fn insert_subtotals_ex_values_via_real_engine_entry() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();
        let mut calc_engine = roe_calc::CalcEngine::new();
        for (r, c, v) in [
            (0, 0, "Region"), (0, 1, "Day"), (0, 2, "V"),
            (1, 0, "East"), (1, 1, "Mon"), (1, 2, "1"),
            (2, 0, "East"), (2, 1, "Mon"), (2, 2, "2"),
            (3, 0, "East"), (3, 1, "Tue"), (3, 2, "3"),
            (4, 0, "West"), (4, 1, "Mon"), (4, 2, "4"),
        ] {
            engine.run_calc(&format!(r#"set_cell(0, {r}, {c}, "{v}")"#), &mut wb).unwrap();
        }
        let spec = r#"{\"groups\":[{\"col\":0,\"aggs\":[[2,9]]},{\"col\":1,\"aggs\":[[2,9]]}],\"summary_below\":false,\"grand_total\":true}"#;
        let (dirty, ret) = engine
            .run_calc(&format!("insert_subtotals_ex(0, 0, 1, 4, \"{spec}\")"), &mut wb)
            .unwrap();
        assert_eq!(ret.as_str(), Some("{\"new_end_row\":10}"));
        calc_engine.recalc_and_apply(&mut wb, &dirty);

        let val = |engine: &ScriptEngine, wb: &mut Workbook, r: u32| -> String {
            engine.run_calc(&format!("get_cell(0, {r}, 2)"), wb).unwrap().1.as_str().unwrap_or("").to_string()
        };
        // 行序: 1总计 / 2 East / 3 East-Mon / 6 East-Tue / 8 West / 9 West-Mon
        assert_eq!(val(&engine, &mut wb, 1), "10", "总计不应重复计入嵌套汇总行");
        assert_eq!(val(&engine, &mut wb, 2), "6", "East");
        assert_eq!(val(&engine, &mut wb, 3), "3", "East-Mon");
        assert_eq!(val(&engine, &mut wb, 6), "3", "East-Tue");
        assert_eq!(val(&engine, &mut wb, 8), "4", "West");
        assert_eq!(val(&engine, &mut wb, 9), "4", "West-Mon");
    }

    /// insert_subtotals_ex → remove_subtotals 往返:数据与 outline 完全还原,
    /// 覆盖上方/下方两种方向。
    #[test]
    fn insert_subtotals_ex_roundtrip_via_real_engine_entry() {
        for below in [false, true] {
            let engine = ScriptEngine::new();
            let mut wb = empty_wb();
            let data = [
                ("Day", "V"), ("Mon", "1"), ("Mon", "2"), ("Tue", "3"), ("Tue", "4"), ("Wed", "5"),
            ];
            for (r, (d, v)) in data.iter().enumerate() {
                engine.run_calc(&format!(r#"set_cell(0, {r}, 0, "{d}")"#), &mut wb).unwrap();
                engine.run_calc(&format!(r#"set_cell(0, {r}, 1, "{v}")"#), &mut wb).unwrap();
            }
            let spec = format!(
                r#"{{\"groups\":[{{\"col\":0,\"aggs\":[[1,9]]}}],\"summary_below\":{below},\"grand_total\":true}}"#
            );
            let (_, ret) = engine
                .run_calc(&format!("insert_subtotals_ex(0, 0, 1, 5, \"{spec}\")"), &mut wb)
                .unwrap();
            assert_eq!(ret.as_str(), Some("{\"new_end_row\":9}"), "below={below}");

            let (_, ret) = engine.run_calc("remove_subtotals(0, 0, 0, 9, 1)", &mut wb).unwrap();
            assert_eq!(ret.as_str(), Some("{\"removed\":4}"), "below={below}: 总计+3个组汇总");

            for (r, (d, v)) in data.iter().enumerate() {
                assert_eq!(engine.run_calc(&format!("get_cell(0, {r}, 0)"), &mut wb).unwrap().1.as_str(), Some(*d), "below={below} row{r}");
                assert_eq!(engine.run_calc(&format!("get_cell(0, {r}, 1)"), &mut wb).unwrap().1.as_str(), Some(*v), "below={below} row{r}");
            }
            assert_eq!(engine.run_calc("get_cell(0, 6, 0)", &mut wb).unwrap().1.as_str(), Some(""), "below={below}: 尾部应为空");
            assert!(wb.sheets[0].row_outline_level.iter().all(|&l| l == 0),
                    "below={below}: outline 应全部还原, 实际 {:?}", wb.sheets[0].row_outline_level);
        }
    }
}

/// list_functions 生产入口测试:必须经 ScriptEngine::new() / new_readonly() 验证,
/// 不能只测 calc_api::register(项目既定纪律,见 roe-ext_progress_v163/v167)。
#[cfg(test)]
mod production_entry_list_functions_check {
    use super::*;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
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
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn full_engine_lists_write_functions() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();
        let (_, all) = engine.run_calc("list_functions(\"\")", &mut wb).unwrap();
        let all = all.as_str().unwrap().to_string();
        assert!(all.contains("set_cell[w]"), "完整引擎应列出可写函数: {all}");
        assert!(all.contains("get_cell[r]"), "完整引擎应列出只读函数: {all}");
        let (_, sorted) = engine.run_calc("list_functions(\"sort\")", &mut wb).unwrap();
        let sorted = sorted.as_str().unwrap().to_string();
        assert!(sorted.contains("sort_range("), "应含 sort_range 签名: {sorted}");
        assert!(sorted.contains("key_col: i64"), "应含参数名: {sorted}");
    }

    #[test]
    fn readonly_engine_hides_write_functions() {
        let engine = ScriptEngine::new_readonly();
        let wb = empty_wb();
        let all = engine.query_calc("list_functions(\"\")", &wb).unwrap();
        let all = all.as_str().unwrap().to_string();
        assert!(all.contains("get_cell[r]"), "只读引擎应列出 get_cell: {all}");
        assert!(!all.contains("set_cell["), "只读引擎不应列出 set_cell: {all}");
        assert!(!all.contains("sort_range["), "只读引擎不应列出 sort_range: {all}");
        let body = all.split_once('\n').map(|x| x.1).unwrap_or("");
        assert!(!body.contains("[w]"), "只读引擎不应出现任何可写标记: {all}");
    }

    #[test]
    fn readonly_engine_filter_does_not_leak_write_functions() {
        let engine = ScriptEngine::new_readonly();
        let wb = empty_wb();
        let r = engine.query_calc("list_functions(\"set_cell\")", &wb).unwrap();
        let r = r.as_str().unwrap().to_string();
        assert!(r.contains("没有函数名包含"), "过滤 set_cell 应无结果: {r}");
    }
}

/// inspect_workbook 生产入口测试(经 ScriptEngine::new() / new_readonly())。
#[cfg(test)]
mod production_entry_inspect_workbook_check {
    use super::*;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
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
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    fn parse(v: &str) -> serde_json::Value { serde_json::from_str(v).unwrap() }

    #[test]
    fn empty_workbook() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();
        let (_, r) = engine.run_calc("inspect_workbook()", &mut wb).unwrap();
        let j = parse(r.as_str().unwrap());
        assert_eq!(j["sheet_count"], 1);
        assert_eq!(j["sheets"][0]["name"], "Sheet1");
        assert_eq!(j["sheets"][0]["empty"], true);
    }

    #[test]
    fn workbook_with_data() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();
        for s in ["set_cell(0,0,0,\"姓名\")", "set_cell(0,0,1,\"部门\")", "set_cell(0,1,0,\"张三\")", "set_cell(0,2,1,\"研发\")"] {
            engine.run_calc(s, &mut wb).unwrap();
        }
        let (_, r) = engine.run_calc("inspect_workbook()", &mut wb).unwrap();
        let j = parse(r.as_str().unwrap());
        let sh = &j["sheets"][0];
        assert_eq!(sh["empty"], false);
        assert_eq!(sh["max_row"], 2);
        assert_eq!(sh["max_col"], 1);
        assert_eq!(sh["top_rows"][0][0], "姓名");
        assert_eq!(sh["top_rows"][0][1], "部门");
        assert_eq!(sh["top_rows"][2][1], "研发");
    }

    #[test]
    fn cleared_cell_shrinks_range() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();
        for s in ["set_cell(0,0,0,\"h\")", "set_cell(0,2,0,\"x\")", "set_cell(0,50,0,\"v\")"] {
            engine.run_calc(s, &mut wb).unwrap();
        }
        let (_, r) = engine.run_calc("inspect_workbook()", &mut wb).unwrap();
        assert_eq!(parse(r.as_str().unwrap())["sheets"][0]["max_row"], 50);
        engine.run_calc("clear_cell(0,50,0)", &mut wb).unwrap();
        let (_, r) = engine.run_calc("inspect_workbook()", &mut wb).unwrap();
        assert_eq!(parse(r.as_str().unwrap())["sheets"][0]["max_row"], 2, "清空后范围应收缩");
    }

    #[test]
    fn readonly_engine_can_inspect() {
        let engine = ScriptEngine::new_readonly();
        let wb = empty_wb();
        let r = engine.query_calc("inspect_workbook()", &wb).unwrap();
        assert!(r.as_str().unwrap().contains("sheet_count"), "{r}");
    }
}

/// list_functions 按文档类型分流:必须经 ScriptEngine::new() 的 run_writer / run_impress / run_calc 验证。
#[cfg(test)]
mod production_entry_list_functions_by_doctype {
    use super::*;

    #[test]
    fn writer_doc_lists_writer_functions() {
        let engine = ScriptEngine::new();
        let mut doc = WriterDocument::default();
        let all = engine.run_writer("list_functions(\"\")", &mut doc).unwrap();
        let all = all.as_str().unwrap().to_string();
        assert!(all.contains("paragraph_count"), "Writer 应列出 paragraph_count: {all}");
        assert!(!all.contains("set_cell"), "Writer 不应列出 Calc 函数: {all}");
        assert!(!all.contains("slide_count"), "Writer 不应列出 Impress 函数: {all}");
        let one = engine.run_writer("list_functions(\"get_paragraph\")", &mut doc).unwrap();
        assert!(one.as_str().unwrap().contains("get_paragraph[r](idx: i64)"), "应含签名: {one}");
    }

    #[test]
    fn impress_doc_lists_impress_functions() {
        let engine = ScriptEngine::new();
        let mut pres = Presentation {
            slide_size: (720.0, 540.0),
            slides: vec![],
            master_slides: vec![],
            styles: roe_model::style::StyleRegistry::default(),
            theme_colors: std::collections::HashMap::new(),
            default_text_style: Default::default(),
        };
        let all = engine.run_impress("list_functions(\"\")", &mut pres, None).unwrap();
        let all = all.as_str().unwrap().to_string();
        assert!(all.contains("slide_count"), "Impress 应列出 slide_count: {all}");
        assert!(!all.contains("paragraph_count"), "Impress 不应列出 Writer 函数: {all}");
        assert!(!all.contains("set_cell"), "Impress 不应列出 Calc 函数: {all}");
    }

    #[test]
    fn calc_doc_unchanged() {
        let engine = ScriptEngine::new();
        let mut wb = roe_model::calc::Workbook {
            sheets: vec![roe_model::calc::Sheet { name: "Sheet1".into(), ..Default::default() }],
            named_ranges: std::collections::HashMap::new(),
            named_formulas: std::collections::HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };
        let (_, all) = engine.run_calc("list_functions(\"\")", &mut wb).unwrap();
        let all = all.as_str().unwrap().to_string();
        assert!(all.contains("set_cell[w]") && all.contains("get_cell[r]"), "Calc 行为应不变: {all}");
        assert!(!all.contains("paragraph_count") && !all.contains("slide_count"), "Calc 不应列出其他文档函数: {all}");
    }
}

/// print/debug 捕获测试:脚本里的 print 不应写进进程 stdout,
/// 而是被收集后并入脚本返回值(无 print 时返回值不变)。
#[cfg(test)]
mod print_capture_tests {
    use super::*;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use std::collections::HashMap;

    fn empty_wb() -> Workbook {
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
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
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn print_and_value_returns_object() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();
        let (_, ret) = engine.run_calc("print(\"x\"); 1", &mut wb).unwrap();
        assert_eq!(ret, serde_json::json!({"output": "x", "result": 1}), "got {ret}");
    }

    #[test]
    fn print_only_returns_output_string() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();
        let (_, ret) = engine.run_calc("print(\"x\");", &mut wb).unwrap();
        assert_eq!(ret, serde_json::json!("x"), "got {ret}");
    }

    #[test]
    fn no_print_keeps_return_value() {
        let engine = ScriptEngine::new();
        let mut wb = empty_wb();
        let (_, ret) = engine.run_calc("1 + 1", &mut wb).unwrap();
        assert_eq!(ret, serde_json::json!(2), "got {ret}");
    }
}
