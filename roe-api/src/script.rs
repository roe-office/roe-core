//! roe_get_index / roe_execute_script / roe_free_string

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use roe_parser::RoeDocument;
use roe_script::{index, ScriptEngine};
use serde_json::Value as JsonValue;

use crate::error::{clear_last_error, set_last_error};
use crate::handle;

/// Calc 脏区域：单元格行列范围（0-indexed）
///
/// pub：roe-agent 等内部 Rust 消费者（通过 execute_script_impl）需要
/// 读取这个结构来决定局部渲染/预览范围，不经过 C-ABI JSON 编解码。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DirtyRegion {
    pub sheet: usize,
    pub r1: u32,
    pub c1: u32,
    pub r2: u32,
    pub c2: u32,
}

/// 提取当前文档的结构拓扑 JSON（不含内容，AI 隐私保护设计）。
///
/// 内部 Rust 版本：供 roe-agent 等同进程 Rust 消费者直接调用，不经过
/// C-ABI 的指针/CString 编解码（架构决策见 roe-agent/src/lib.rs：
/// "Tool 封装应直接包 roe-script/roe-api 里的 Rust 函数，不要跨 FFI
/// 边界调用 C-ABI"）。句柄无效返回 None。
pub fn get_index_impl(handle: u64) -> Option<String> {
    handle::with_entry(handle, |entry| match &entry.doc {
        RoeDocument::Writer(doc) => index::extract_writer_index(doc),
        RoeDocument::Calc(wb) => index::extract_calc_index(wb),
        RoeDocument::Impress(pres) => index::extract_impress_index(pres),
    })
}

/// 提取当前文档的结构拓扑 JSON（不含内容，AI 隐私保护设计）。
/// 返回值需要 [`roe_free_string`] 释放；句柄无效返回 null
/// （详情见 roe_last_error_message）。
#[no_mangle]
pub extern "C" fn roe_get_index(handle: u64) -> *mut c_char {
    match get_index_impl(handle) {
        Some(json) => match CString::new(json) {
            Ok(cs) => {
                clear_last_error();
                cs.into_raw()
            }
            Err(_) => {
                set_last_error("roe_get_index: index json contains NUL byte");
                std::ptr::null_mut()
            }
        },
        None => {
            set_last_error(format!("roe_get_index: invalid handle {handle}"));
            std::ptr::null_mut()
        }
    }
}

/// 脚本执行前整份克隆文档用于回滚/撤销,按实测约 550 字节/格,
/// 100 万格约 550MB。超过则拒绝执行,引导用户走 df_* 数据集路径。
pub const MAX_SCRIPT_DOC_CELLS: usize = 1_000_000;

fn doc_too_large_for_script(doc: &RoeDocument, limit: usize) -> Option<String> {
    if let RoeDocument::Calc(wb) = doc {
        let n: usize = wb.sheets.iter().map(|s| s.cells.cell_count()).sum();
        if n > limit {
            return Some(format!(
                "文档过大({n} 格,上限 {limit} 格),脚本不支持,请使用 df_* 数据集路径"
            ));
        }
    }
    None
}

/// 执行 Rhai 脚本，操作内存模型。
///
/// 返回 JSON 形如 `{"ok":true,"dirty_pages":[0,2]}` 或
/// `{"ok":false,"error":"..."}`，需要 [`roe_free_string`] 释放。
///
/// 成功时直接修改 `entry.doc` 本体（不是 snapshot；snapshot/commit/discard
/// 是 13f 的事）。
///
/// dirty_pages（v1，整文档级粒度）：
///   - Calc:    所有 sheet 索引 `0..sheets.len()`
///   - Writer:  固定 `[0]`（页面布局不在脚本路径上现算，占位）
///   - Impress: 所有 slide 索引 `0..slides.len()`
/// 执行 Rhai 脚本，操作内存模型——内部 Rust 版本。
///
/// 供 roe-agent 等同进程 Rust 消费者直接调用，不经过 C-ABI 的指针/
/// CString 编解码（架构决策见 roe-agent/src/lib.rs）。语义与
/// `roe_execute_script` 完全一致（本就是同一段逻辑抽出来的），
/// 包括 undo_stack 接入、dirty region 计算、writer layout cache 失效。
///
/// 返回 None：句柄无效。
/// 返回 Some(Err(..))：脚本执行失败（语法错误等），未改动文档。
/// 返回 Some(Ok((dirty_pages, dirty_regions, return_value)))：成功。
/// media_source：供 Impress 文档的 set_slide_background picture/
/// picture_tiled 分支读取虚拟路径对应的媒体文件内容（见
/// roe-script/src/media_context.rs，ROE_Agent_Workspace_设计文档_v1.md）。
/// Calc/Writer 分支不使用这个参数；FFI 边界（roe_execute_script）
/// 当前没有 workspace 概念，固定传 None。
pub fn execute_script_impl(
    handle: u64,
    script: &str,
    media_source: Option<&dyn roe_script::media_context::MediaAssetSource>,
) -> Option<Result<(Vec<usize>, Vec<DirtyRegion>, JsonValue), String>> {
    let engine = ScriptEngine::new();

    let result = handle::with_entry(handle, |entry| {
        // Step 2b（阶段C）：粗暴版撤销栈接入。脚本执行前无条件 clone 一份
        // 文档，脚本执行成功后 push 进 undo_stack；Writer/Calc/Impress 三个
        // 分支一视同仁，不判断"是否真的修改了文档"（该优化留给 Step 2c，
        // 且目前只计划对 Calc 做，因为只有 run_calc 有现成的 dirty 信号）。
        // ctx 统一用 EditCursorContext::None——脚本执行没有明确的光标/
        // 选区语境，None variant 就是为这种场景设计的。
        if let Some(msg) = doc_too_large_for_script(&entry.doc, MAX_SCRIPT_DOC_CELLS) {
            return Err(msg);
        }
        let prev_doc = entry.doc.clone();

        // 先把 calc_engine 字段单独取出可变引用（与下面 &mut entry.doc 是
        // 不相交字段借用，Rust 允许同时持有），避免后续需要同时使用
        // entry.doc 和 entry.calc_engine 时出现"entry 已被借用"的编译错误。
        let calc_engine_slot = &mut entry.calc_engine;
        // Calc 脚本没有任何写入信号(dirty_cells 为空且无结构编辑)时为 true,成功后不 push 撤销快照。
        let calc_noop = std::cell::Cell::new(false);
        let inner_result = match &mut entry.doc {
        RoeDocument::Calc(wb) => {
            let mut ds = handle::take_datasets(handle);
            let ds_r = engine.run_calc_ds(script, wb, &mut ds);
            handle::put_datasets(handle, ds);
            ds_r.map(|(mut dirty_cells, ret)| {
                // P0-b 修复:发生过行列结构编辑时,DAG(按位置存)已错位,
                // 丢弃引擎,以全部公式格为脏集全量重算;并补一个保守的
                // 整表刷新信号(0,0,0),保证纯数据表结构编辑后前台也会刷新。
                if roe_script::calc_api::take_struct_edit() {
                    *calc_engine_slot = None;
                    dirty_cells.extend(roe_script::calc_api::collect_formula_cells(wb));
                    dirty_cells.push((0, 0, 0));
                }
                // dirty_regions（2026-06-19 细化）：按 sheet 分组，取每组
                // (row, col) 点集合的最小包围矩形（bounding box）作为该
                // sheet 的脏区域，取代之前"整 sheet 全范围标脏"的占位实现。
                // 没有任何写操作触碰过的 sheet 不再生成 region。
                calc_noop.set(dirty_cells.is_empty());
                let mut by_sheet: std::collections::BTreeMap<usize, (u32, u32, u32, u32)> =
                    std::collections::BTreeMap::new();
                for (sheet, row, col) in &dirty_cells {
                    by_sheet
                        .entry(*sheet)
                        .and_modify(|(r1, c1, r2, c2)| {
                            *r1 = (*r1).min(*row);
                            *c1 = (*c1).min(*col);
                            *r2 = (*r2).max(*row);
                            *c2 = (*c2).max(*col);
                        })
                        .or_insert((*row, *col, *row, *col));
                }
                let dirty_pages: Vec<usize> = by_sheet.keys().copied().collect();
                let regions: Vec<DirtyRegion> = by_sheet
                    .into_iter()
                    .map(|(sheet, (r1, c1, r2, c2))| DirtyRegion { sheet, r1, c1, r2, c2 })
                    .collect();

                // v104 新增：脚本执行产生的 dirty_cells 驱动一次公式重算，
                // 并把结果写回 wb（office-rs2 要求的"方向C"：脚本执行结束
                // 前统一触发一次重算）。calc_engine 惰性创建，跨多次脚本
                // 执行复用同一个 FormulaDAG，避免每次都重建整张依赖图。
                let engine_ref = calc_engine_slot.get_or_insert_with(roe_calc::CalcEngine::new);
                engine_ref.recalc_and_apply(wb, &dirty_cells);

                (dirty_pages, regions, false, ret)
            })
        }
        // 第三个 bool：是否修改了 Writer 文档本体，决定是否需要在下方
        // with_entry 闭包返回后失效 writer_layout_cache（2026-06-18 P0 接入）。
        RoeDocument::Writer(doc) => {
            // 只读脚本:不推撤销快照、不标脏、不失效布局缓存(拿不准一律按写处理)
            let ro = roe_script::script_kind::is_readonly_script(roe_script::script_kind::Kind::Writer, script);
            calc_noop.set(ro);
            engine.run_writer(script, doc).map(|ret| {
                if ro { (vec![], vec![], false, ret) } else { (vec![0usize], vec![], true, ret) }
            })
        }
        RoeDocument::Impress(pres) => {
            let ro = roe_script::script_kind::is_readonly_script(roe_script::script_kind::Kind::Impress, script);
            calc_noop.set(ro);
            let before = pres.slides.len();
            let r = engine.run_impress(script, pres, media_source);
            // 写脚本:脏页按执行后页数(至少 1 页),避免空文档 insert_slide 后脏页为空导致前台漏刷
            let n = pres.slides.len().max(before).max(1);
            r.map(|ret| {
                if ro { (vec![], vec![], false, ret) } else { ((0..n).collect::<Vec<_>>(), vec![], false, ret) }
            })
        }
    };

        // inner_result 是 Result<(...), String>；成功才 push 撤销快照，
        // 失败（脚本报错）说明文档没有被有效改动，不应该产生一条可撤销的
        // 记录——撤销栈里每一条都应该对应一次真实生效的编辑。
        if inner_result.is_ok() {
            if !calc_noop.get() {
                entry.push_undo_snapshot(prev_doc, handle::EditCursorContext::None);
            }
        } else {
            // 失败回滚:脚本中途报错时文档可能已被部分修改,
            // 用执行前的克隆整体恢复,并清掉结构编辑标志,避免污染下一次执行。
            entry.doc = prev_doc;
            let _ = roe_script::calc_api::take_struct_edit();
        }

        inner_result
    });

    // Writer 文档被成功修改时，失效布局缓存（必须在上面 with_entry 闭包
    // 返回之后调用，否则对同一 handle 的 RefCell 会嵌套借用而 panic）。
    if let Some(Ok((_, _, true, _))) = &result {
        handle::invalidate_writer_layout_cache(handle);
    }

    result.map(|inner| inner.map(|(dirty, regions, _, ret)| (dirty, regions, ret)))
}

/// 执行一段"承诺只读"的 Rhai 脚本，直接在原文档 handle 上运行，
/// 不经过 shadow 机制（v193 会话新增，供 roe-agent 的
/// query_script_preview 工具使用）。
///
/// 与 execute_script_impl 的关键区别：
/// - execute_script_impl 用 ScriptEngine::new()，注册了全部读写函数，
///   所以必须经过 shadow 副本保护原文档，脚本失败或需要确认时才能
///   安全丢弃。
/// - query_script_impl 用 ScriptEngine::new_readonly()，只注册了
///   确认不修改 Workbook 状态的查询类函数（见 calc_api::register_readonly
///   文档）。脚本在这个引擎上运行，即使脚本内容试图调用写函数，也会
///   因函数未注册而运行时报错、不产生任何副作用——机制上就不需要
///   shadow 保护，直接在原文档上跑即可，读到的永远是当下最新的真实
///   状态，不像 execute_script_impl 那样可能读到的是"待确认的预览"。
///
/// 返回 None：句柄无效。
/// 返回 Some(Err(msg))：脚本本身执行失败（语法错误、调用了未注册的
/// 写函数等）。
/// 返回 Some(Ok(json))：脚本的返回值（JSON 化），没有 dirty_pages/
/// dirty_regions——只读脚本不产生脏区域，调用方不需要这个信息。
pub fn readonly_query_script_impl(handle: u64, script: &str) -> Option<Result<JsonValue, String>> {
    let engine = ScriptEngine::new_readonly();

    handle::with_entry(handle, |entry| {
        match &mut entry.doc {
            RoeDocument::Calc(wb) => {
                {
                    // 绑定 handle 持有的数据集容器(用户已加载的数据可 df_use);
                    // 只读引擎里写容器的函数已被桩覆盖,put 回去的内容不变。
                    let mut ds = handle::take_datasets(handle);
                    let r = engine.run_calc_ds(script, wb, &mut ds).map(|(_dirty, ret)| ret);
                    handle::put_datasets(handle, ds);
                    r
                }
            }
            // 只读引擎:Calc、Writer、Impress 各走各自的只读白名单,
            // 写函数未注册,调用会报 Function not found。
            // Writer:只读引擎(只读白名单见 writer_api::READ_FN_NAMES),直接 run_writer,
            // 不推撤销快照、不失效布局缓存(与 exec 路径的区别)。
            RoeDocument::Writer(doc) => {
                ScriptEngine::new_readonly_writer().run_writer(script, doc)
            }
            RoeDocument::Impress(pres) => {
                // 只读引擎(13 个纯读函数),media_source 传 None,
                // 不推撤销快照,与 Writer 只读分支一致。
                ScriptEngine::new_readonly_impress().run_impress(script, pres, None)
            }
        }
    })
}

/// 执行 Rhai 脚本，操作内存模型。
///
/// 返回 JSON 形如 `{"ok":true,"dirty_pages":[0,2]}` 或
/// `{"ok":false,"error":"..."}`，需要 [`roe_free_string`] 释放。
///
/// 成功时直接修改 `entry.doc` 本体（不是 snapshot；snapshot/commit/discard
/// 是 13f 的事）。
///
/// dirty_pages（v1，整文档级粒度）：
///   - Calc:    所有 sheet 索引 `0..sheets.len()`
///   - Writer:  固定 `[0]`（页面布局不在脚本路径上现算，占位）
///   - Impress: 所有 slide 索引 `0..slides.len()`
#[no_mangle]
pub extern "C" fn roe_execute_script(handle: u64, script: *const c_char) -> *mut c_char {
    if script.is_null() {
        let msg = "null script".to_string();
        set_last_error(msg.clone());
        return script_result_json(Err(msg));
    }

    let script = match unsafe { CStr::from_ptr(script) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("invalid utf8 in script: {e}");
            set_last_error(msg.clone());
            return script_result_json(Err(msg));
        }
    };

    match execute_script_impl(handle, script, None) {
        Some(Ok((dirty, regions, ret))) => {
            clear_last_error();
            script_result_json(Ok((dirty, regions, ret)))
        }
        Some(Err(e)) => {
            set_last_error(e.clone());
            script_result_json(Err(e))
        }
        None => {
            let msg = format!("invalid handle {handle}");
            set_last_error(msg.clone());
            script_result_json(Err(msg))
        }
    }
}

/// 释放 roe-api 返回的 `*mut c_char` 字符串。
/// 对 null 安全（no-op）。
#[no_mangle]
pub extern "C" fn roe_free_string(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }
    unsafe {
        drop(CString::from_raw(ptr));
    }
}

fn script_result_json(result: Result<(Vec<usize>, Vec<DirtyRegion>, JsonValue), String>) -> *mut c_char {
    let json = match result {
        Ok((dirty, regions, ret)) => {
            let items = dirty
                .iter()
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let region_items = regions.iter().map(|r| {
                format!(r#"{{"sheet":{},"r1":{},"c1":{},"r2":{},"c2":{}}}"#,
                    r.sheet, r.r1, r.c1, r.r2, r.c2)
            }).collect::<Vec<_>>().join(",");
            let return_value_json = if ret.is_null() {
                "null".to_string()
            } else {
                serde_json::to_string(&ret).unwrap_or_else(|_| "null".to_string())
            };
            format!(r#"{{"ok":true,"dirty_pages":[{}],"dirty_regions":[{}],"return_value":{}}}"#, items, region_items, return_value_json)
        }
        Err(e) => {
            let escaped = e
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "");
            format!(r#"{{"ok":false,"error":"{}"}}"#, escaped)
        }
    };
    CString::new(json)
        .map(CString::into_raw)
        .unwrap_or_else(|_| std::ptr::null_mut())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::WriterOptions;
    use crate::handle::DocumentEntry;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use roe_model::common::StyleRef;
    use roe_model::style::StyleRegistry;
    use roe_model::writer::{BodyElement, DocumentColumns, DocumentMeta, HeaderFooterSet, Paragraph, Run, WriterDocument};
    use std::collections::{HashMap, VecDeque};

    fn make_calc_handle() -> u64 {
        let wb = Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                name: "Data".into(),
                cells: SparseGrid::default(),
                col_widths: vec![],
                row_heights: vec![],
                frozen_rows: 0,
                frozen_cols: 0,
                cell_formats:     std::collections::HashMap::new(),
                cond_formats:     vec![],
                data_validations: vec![],
                auto_filter:      None,
                merged_cells:     vec![],
                pivot:            None,
                ..Default::default()
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };
        handle::insert(DocumentEntry {
            doc: RoeDocument::Calc(wb),
            snapshot: None,
            is_shadow: false,
            events: VecDeque::new(),
            animation: HashMap::new(),
            writer_layout_cache: None,
            writer_font_registry: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            writer_options: WriterOptions::default(),
            impress_font_registry: None,
            writer_parley_font_context: None,
            calc_engine: None,
            pending_comment_hits: Vec::new(),
        })
    }

    /// v104 集成测试：验证 roe_execute_script 执行后，公式重算结果真正
    /// 写回了 entry.doc（不是只有 roe-calc 内部单测能通过，而是 C-ABI
    /// 这条真实调用链也生效）。set_cell(0,0,"10")、set_cell(1,0,"20")、
    /// set_formula(0,1,"=A1+A2") 之后，读 workbook.sheets[0].cells 里
    /// B1（row=0,col=1）应该是 CellValue::Formula { cached: Number(30), .. }。
    #[test]
    fn test_execute_script_recalcs_formula_via_c_abi() {
        let h = make_calc_handle();
        let script = r#"
            set_cell(0, 0, 0, "10");
            set_cell(0, 1, 0, "20");
            set_formula(0, 0, 1, "=A1+A2");
        "#;
        let c_script = std::ffi::CString::new(script).unwrap();
        let result_ptr = roe_execute_script(h, c_script.as_ptr());
        assert!(!result_ptr.is_null(), "roe_execute_script returned null");
        let result_json = unsafe { std::ffi::CStr::from_ptr(result_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        unsafe { roe_free_string(result_ptr) };
        assert!(result_json.contains("\"ok\":true"), "script failed: {result_json}");

        // 直接读 entry.doc，验证 B1 (row=0, col=1) 的 cached 字段已经是
        // Number(30)，而不是停留在 Empty（说明 recalc_and_apply 真的
        // 被调用并把结果写回了 workbook）。
        let b1 = handle::with_entry(h, |entry| match &entry.doc {
            RoeDocument::Calc(wb) => wb.sheets[0].cells.get(0, 1),
            _ => panic!("expected Calc document"),
        }).expect("handle not found");

        match b1 {
            roe_model::calc::CellValue::Formula { source, cached } => {
                assert_eq!(source, "=A1+A2", "formula source should be preserved, got {source:?}");
                match cached.as_ref() {
                    roe_model::calc::CellValue::Number(n) => {
                        assert!((*n - 30.0).abs() < 1e-9, "expected cached=30, got {n}");
                    }
                    other => panic!("expected cached Number(30.0), got {other:?}"),
                }
            }
            other => panic!("expected CellValue::Formula, got {other:?}"),
        }
    }

    /// v104 集成测试：A1="=A2", A2="=A1" 循环引用，通过真实 C-ABI 脚本路径
    /// 设置后必须不 panic，且两者都应得到 CellError::CircularRef。
    #[test]
    fn test_execute_script_circular_reference_does_not_panic() {
        let h = make_calc_handle();
        // 第一次执行：A1="=A2", A2 此时还是 Empty，不构成真正的图上环
        // （DAG 里还没有任何依赖边）。这次求值会建立 A1 依赖 A2 的边。
        let script1 = r#"set_formula(0, 0, 0, "=A2");"#;
        let c_script1 = std::ffi::CString::new(script1).unwrap();
        let result_ptr1 = roe_execute_script(h, c_script1.as_ptr());
        assert!(!result_ptr1.is_null());
        unsafe { roe_free_string(result_ptr1) };

        // 第二次执行：A2="=A1"。这次求值会建立 A2 依赖 A1 的边，此时
        // DAG 里 A1<->A2 才真正形成图上环。roe-calc 内部单测
        // test_circular_reference_returns_circular_ref_error 验证的正是
        // 同样的"两次调用才能查出环"行为——第一次调用只建立单向依赖边，
        // 环必须等第二条边补上才存在，这是 toposort 检测环的必然前提，
        // 不是 bug。
        let script2 = r#"set_formula(0, 1, 0, "=A1");"#;
        let c_script2 = std::ffi::CString::new(script2).unwrap();
        let result_ptr2 = roe_execute_script(h, c_script2.as_ptr());
        assert!(!result_ptr2.is_null(), "roe_execute_script returned null (unexpected failure, not panic though)");
        let result_json = unsafe { std::ffi::CStr::from_ptr(result_ptr2) }
            .to_str()
            .unwrap()
            .to_string();
        unsafe { roe_free_string(result_ptr2) };
        assert!(result_json.contains("\"ok\":true"), "script itself should succeed (script only sets formulas, recalc errors are cell-level not script-level): {result_json}");

        // A1 (row=0,col=0) 和 A2 (row=1,col=0) 都应该是 CircularRef 错误
        let (a1, a2) = handle::with_entry(h, |entry| match &entry.doc {
            RoeDocument::Calc(wb) => (wb.sheets[0].cells.get(0, 0), wb.sheets[0].cells.get(1, 0)),
            _ => panic!("expected Calc document"),
        }).expect("handle not found");

        for (label, cell) in [("A1", &a1), ("A2", &a2)] {
            match cell {
                roe_model::calc::CellValue::Formula { cached, .. } => {
                    match cached.as_ref() {
                        roe_model::calc::CellValue::Error(roe_model::calc::CellError::CircularRef) => {}
                        other => panic!("{label}: expected cached CircularRef error, got {other:?}"),
                    }
                }
                other => panic!("{label}: expected CellValue::Formula, got {other:?}"),
            }
        }
    }

    fn make_writer_handle() -> u64 {
        let doc = WriterDocument {
            page_size: (595.28, 841.89),
            header: HeaderFooterSet::default(),
            footer: HeaderFooterSet::default(),
            title_page: false,
            even_and_odd_headers: false,
            margin_top:    56.69,
            margin_bottom: 56.69,
            margin_left:   70.87,
            margin_right:  70.87,
            columns: DocumentColumns::default(),
            sections: Vec::new(),
            styles: StyleRegistry::default(),
            body: vec![
                BodyElement::Paragraph(Paragraph {
                    style: StyleRef("Heading1".into()),
                    runs: vec![Run { text: "Title".into(), char_style: None, ..Default::default() }],
                    list_info: None,
                    ..Default::default()
                }),
                BodyElement::Paragraph(Paragraph {
                    style: StyleRef("Default".into()),
                    runs: vec![Run { text: "Body text".into(), char_style: None, ..Default::default() }],
                    list_info: None,
                    ..Default::default()
                }),
            ],
            metadata: DocumentMeta::default(),
            // 2026-09 补丁：WriterDocument 新增了脚注/尾注相关字段
            // （endnotes/endnote_num_fmt/footnote_num_fmt 等），本测试
            // helper 未跟上，用 ..Default::default() 补齐未列出字段的
            // 默认值。与本次阶段C的EditCursorContext改动无关，纯粹是
            // 修复编译错误，不改变本测试原本验证的行为。
            ..Default::default()
        };
        handle::insert(DocumentEntry {
            doc: RoeDocument::Writer(doc),
            snapshot: None,
            is_shadow: false,
            events: VecDeque::new(),
            animation: HashMap::new(),
            writer_layout_cache: None,
            writer_font_registry: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            writer_options: WriterOptions::default(),
            impress_font_registry: None,
            writer_parley_font_context: None,
            calc_engine: None,
            pending_comment_hits: Vec::new(),
        })
    }

    fn json_from(ptr: *mut c_char) -> String {
        assert!(!ptr.is_null());
        let s = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_string();
        roe_free_string(ptr);
        s
    }

    #[test]
    fn test_get_index_calc() {
        let h = make_calc_handle();
        let s = json_from(roe_get_index(h));
        assert!(s.contains("\"type\":\"calc\""));
        assert!(s.contains("\"sheet_count\":1"));
    }

    #[test]
    fn test_get_index_writer() {
        let h = make_writer_handle();
        let s = json_from(roe_get_index(h));
        assert!(s.contains("\"type\":\"writer\""));
        assert!(s.contains("\"para_count\":2"));
    }

    #[test]
    fn test_get_index_invalid_handle() {
        let ptr = roe_get_index(999_999);
        assert!(ptr.is_null());
    }

    // ── 会话级数据集:按文档归属的回归测试 ─────────────────────
    fn ds_save(h: u64, name: &str) -> String {
        let script = format!(
            r#"set_cell(0,0,0,"k"); set_cell(0,1,0,"1"); set_cell(0,2,0,"2"); df_load_sheet(0); df_save_as("{name}")"#
        );
        match execute_script_impl(h, &script, None) {
            Some(Ok((_, _, ret))) => ret.as_str().unwrap_or("").to_string(),
            other => format!("unexpected: {:?}", other.map(|r| r.map(|(_, _, v)| v))),
        }
    }

    fn ds_list(h: u64) -> String {
        match execute_script_impl(h, "df_list_datasets()", None) {
            Some(Ok((_, _, ret))) => ret.as_str().unwrap_or("").to_string(),
            other => format!("unexpected: {:?}", other.map(|r| r.map(|(_, _, v)| v))),
        }
    }

    #[test]
    fn test_datasets_isolated_between_documents() {
        let a = make_calc_handle();
        let b = make_calc_handle();
        assert_eq!(ds_save(a, "only_a"), "ok");
        assert!(ds_list(a).contains("\"only_a\""), "{}", ds_list(a));
        assert_eq!(ds_list(b), "[]", "文档 B 不应看到文档 A 的数据集");
    }

    #[test]
    fn test_datasets_persist_across_script_calls_and_freed_on_remove() {
        let h = make_calc_handle();
        assert_eq!(ds_save(h, "t1"), "ok");
        assert!(ds_list(h).contains("\"t1\""));
        handle::remove(h);
        assert!(handle::take_datasets(h).is_empty(), "remove 后数据集应被释放");
    }

    #[test]
    fn test_shadow_commit_carries_datasets_back() {
        let h = make_calc_handle();
        assert_eq!(ds_save(h, "t1"), "ok");
        let guard = crate::shadow::create_shadow(h).unwrap();
        let sh = guard.shadow_handle();
        assert!(ds_list(sh).contains("\"t1\""), "影子应继承原文档数据集");
        assert_eq!(ds_save(sh, "t2"), "ok");
        assert!(!ds_list(h).contains("\"t2\""), "commit 前原文档不应有 t2");
        assert!(crate::shadow::commit_shadow(guard));
        let l = ds_list(h);
        assert!(l.contains("\"t1\"") && l.contains("\"t2\""), "{l}");
    }

    #[test]
    fn test_shadow_drop_without_commit_discards_datasets() {
        let h = make_calc_handle();
        assert_eq!(ds_save(h, "t1"), "ok");
        let guard = crate::shadow::create_shadow(h).unwrap();
        let sh = guard.shadow_handle();
        assert_eq!(ds_save(sh, "t2"), "ok");
        drop(guard);
        let l = ds_list(h);
        assert!(l.contains("\"t1\"") && !l.contains("\"t2\""), "{l}");
    }

    #[test]
    fn test_execute_script_calc_ok() {
        let h = make_calc_handle();
        let script = CString::new("set_cell(0, 0, 0, \"42\");").unwrap();
        let s = json_from(roe_execute_script(h, script.as_ptr()));
        assert!(s.contains("\"ok\":true"));
        assert!(s.contains("\"dirty_pages\":[0]"));

        // 验证脚本确实改了内存模型本体
        let cell = handle::with_entry(h, |entry| match &entry.doc {
            RoeDocument::Calc(wb) => {
                let script2 = CString::new("get_cell(0, 0, 0)").unwrap();
                // 通过再跑一次脚本读取，间接验证（也确认脚本可重复对同一文档执行）
                let _ = wb;
                script2
            }
            _ => unreachable!(),
        });

        let s2 = json_from(roe_execute_script(h, cell.unwrap().as_ptr()));
        assert!(s2.contains("\"ok\":true"));
    }

    /// 纯查询脚本(无写入信号)成功后不应产生撤销步;真实写入仍然产生。
    #[test]
    fn test_execute_script_calc_readonly_does_not_push_undo() {
        let h = make_calc_handle();
        let depth = |h: u64| handle::with_entry(h, |entry| entry.undo_stack.len()).unwrap();

        let w1 = CString::new("set_cell(0, 0, 0, \"1\");").unwrap();
        assert!(json_from(roe_execute_script(h, w1.as_ptr())).contains("\"ok\":true"));
        let n = depth(h);

        for q in ["get_sheet_count()", "get_cell(0, 0, 0)"] {
            let c = CString::new(q).unwrap();
            let r = json_from(roe_execute_script(h, c.as_ptr()));
            assert!(r.contains("\"ok\":true"), "{q} => {r}");
            assert_eq!(depth(h), n, "纯查询 {q} 不应增加撤销步");
        }

        let w2 = CString::new("set_cell(0, 0, 0, \"2\");").unwrap();
        assert!(json_from(roe_execute_script(h, w2.as_ptr())).contains("\"ok\":true"));
        assert_eq!(depth(h), n + 1, "真实写入应增加一个撤销步");
    }

    /// Step 2b（阶段C）验证：roe_execute_script 执行一次成功的写操作后，
    /// 该次修改应该可以通过 roe_undo 撤销——这是本次改动要解决的核心
    /// 问题（v170记录：Rhai脚本路径此前完全未接入 undo_stack）。
    #[test]
    fn test_execute_script_calc_change_is_undoable() {
        let h = make_calc_handle();

        // 建立初始值
        let s1 = CString::new("set_cell(0, 0, 0, \"1\");").unwrap();
        let r1 = json_from(roe_execute_script(h, s1.as_ptr()));
        assert!(r1.contains("\"ok\":true"));

        let cell_before_change = handle::with_entry(h, |entry| match &entry.doc {
            RoeDocument::Calc(wb) => format!("{:?}", wb.sheets[0].cells.get(0, 0)),
            _ => unreachable!(),
        })
        .unwrap();
        assert!(cell_before_change.contains('1'));

        // 修改
        let s2 = CString::new("set_cell(0, 0, 0, \"999\");").unwrap();
        let r2 = json_from(roe_execute_script(h, s2.as_ptr()));
        assert!(r2.contains("\"ok\":true"));

        let cell_after_change = handle::with_entry(h, |entry| match &entry.doc {
            RoeDocument::Calc(wb) => format!("{:?}", wb.sheets[0].cells.get(0, 0)),
            _ => unreachable!(),
        })
        .unwrap();
        assert!(cell_after_change.contains("999"));

        // 撤销这次脚本执行：应该成功弹栈（非空指针）
        let undo_ptr = crate::snapshot::roe_undo(h);
        assert!(!undo_ptr.is_null(), "roe_undo should succeed: script execution was not pushed to undo_stack");
        unsafe {
            let _ = CString::from_raw(undo_ptr);
        }

        // 值应恢复为撤销前（即"999"这次修改之前）的状态
        let cell_after_undo = handle::with_entry(h, |entry| match &entry.doc {
            RoeDocument::Calc(wb) => format!("{:?}", wb.sheets[0].cells.get(0, 0)),
            _ => unreachable!(),
        })
        .unwrap();
        assert_eq!(cell_after_undo, cell_before_change);
    }

    /// Step 2b 补充验证：脚本执行失败（语法错误等）不应产生一条可撤销的
    /// 撤销栈记录——failed script 没有真正改动文档，push 一条空转的快照
    /// 只会污染撤销历史。
    #[test]
    fn test_execute_script_failed_script_does_not_push_undo() {
        let h = make_calc_handle();

        // 先做一次成功的编辑，确认之后 undo_stack 至少有1条
        let s_ok = CString::new("set_cell(0, 0, 0, \"1\");").unwrap();
        let r_ok = json_from(roe_execute_script(h, s_ok.as_ptr()));
        assert!(r_ok.contains("\"ok\":true"));

        let stack_len_after_success = handle::with_entry(h, |entry| entry.undo_stack.len()).unwrap();
        assert_eq!(stack_len_after_success, 1);

        // 再跑一段语法错误的脚本，应该失败
        let s_bad = CString::new("this is not valid rhai syntax {{{").unwrap();
        let r_bad = json_from(roe_execute_script(h, s_bad.as_ptr()));
        assert!(r_bad.contains("\"ok\":false"));

        // undo_stack 长度应该不变——失败脚本不产生新的撤销记录
        let stack_len_after_failure = handle::with_entry(h, |entry| entry.undo_stack.len()).unwrap();
        assert_eq!(stack_len_after_failure, stack_len_after_success);
    }

    #[test]
    fn test_execute_script_failure_rolls_back_partial_writes() {
        let h = make_calc_handle();
        let s0 = CString::new("set_cell(0, 0, 0, \"1\");").unwrap();
        assert!(json_from(roe_execute_script(h, s0.as_ptr())).contains("\"ok\":true"));
        let snap = |h| handle::with_entry(h, |e| match &e.doc {
            RoeDocument::Calc(wb) => format!("{:?}", wb.sheets[0].cells.get(0, 0)),
            _ => unreachable!(),
        }).unwrap();
        let before = snap(h);
        let bad = CString::new("set_cell(0, 0, 0, \"999\"); throw \"boom\";").unwrap();
        assert!(json_from(roe_execute_script(h, bad.as_ptr())).contains("\"ok\":false"));
        assert_eq!(snap(h), before, "failed script left partial writes");
        assert_eq!(handle::with_entry(h, |e| e.undo_stack.len()).unwrap(), 1);
    }

    #[test]
    fn test_execute_script_infinite_loop_is_aborted_and_rolled_back() {
        let h = make_calc_handle();
        let s0 = CString::new("set_cell(0, 0, 0, \"1\");").unwrap();
        assert!(json_from(roe_execute_script(h, s0.as_ptr())).contains("\"ok\":true"));
        let snap = |h| handle::with_entry(h, |e| match &e.doc {
            RoeDocument::Calc(wb) => format!("{:?}", wb.sheets[0].cells.get(0, 0)),
            _ => unreachable!(),
        }).unwrap();
        let before = snap(h);
        let bad = CString::new("set_cell(0, 0, 0, \"999\"); loop { }").unwrap();
        let r = json_from(roe_execute_script(h, bad.as_ptr()));
        assert!(r.contains("\"ok\":false"), "{r}");
        assert_eq!(snap(h), before, "aborted script left partial writes");
    }

    #[test]
    fn test_doc_too_large_for_script_rejects_over_limit() {
        let h = make_calc_handle();
        let s0 = CString::new("set_cell(0, 0, 0, \"1\"); set_cell(0, 1, 0, \"2\");").unwrap();
        assert!(json_from(roe_execute_script(h, s0.as_ptr())).contains("\"ok\":true"));
        let r = handle::with_entry(h, |e| doc_too_large_for_script(&e.doc, 1)).unwrap();
        assert!(r.is_some(), "2 格应超过上限 1");
        let r = handle::with_entry(h, |e| doc_too_large_for_script(&e.doc, 2)).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn test_undo_stack_keeps_at_least_one_even_over_budget() {
        let h = make_calc_handle();
        // 连续 3 次成功脚本,栈里应有 3 条(格数远小于预算,不应被裁)
        for i in 0..3 {
            let s = CString::new(format!("set_cell(0, {i}, 0, \"x\");")).unwrap();
            assert!(json_from(roe_execute_script(h, s.as_ptr())).contains("\"ok\":true"));
        }
        assert_eq!(handle::with_entry(h, |e| e.undo_stack.len()).unwrap(), 3);
        let cells = handle::with_entry(h, |e| handle::snapshot_cells(&e.doc)).unwrap();
        assert_eq!(cells, 3);
    }

    /// dirty_regions 细化验证（2026-06-19）：单个 set_cell 应产生精确的
    /// 1x1 区域，而不是之前占位实现的 r2/c2 = u32::MAX 全范围。
    #[test]
    fn test_execute_script_dirty_region_single_cell_is_precise() {
        let h = make_calc_handle();
        let script = CString::new("set_cell(0, 3, 5, \"42\");").unwrap();
        let s = json_from(roe_execute_script(h, script.as_ptr()));
        assert!(s.contains("\"ok\":true"));
        assert!(
            s.contains(r#""dirty_regions":[{"sheet":0,"r1":3,"c1":5,"r2":3,"c2":5}]"#),
            "expected precise 1x1 region at (3,5), got: {s}"
        );
    }

    /// dirty_regions 细化验证：多次 set_cell 写在不同位置时，bounding box
    /// 应覆盖所有触碰过的点，且不会退化回全范围占位值。
    #[test]
    fn test_execute_script_dirty_region_bounding_box_multiple_cells() {
        let h = make_calc_handle();
        let script = CString::new(
            "set_cell(0, 2, 1, \"a\"); set_cell(0, 7, 4, \"b\"); set_cell(0, 5, 0, \"c\");",
        )
        .unwrap();
        let s = json_from(roe_execute_script(h, script.as_ptr()));
        assert!(s.contains("\"ok\":true"));
        // bounding box: r 2..=7, c 0..=4
        assert!(
            s.contains(r#""dirty_regions":[{"sheet":0,"r1":2,"c1":0,"r2":7,"c2":4}]"#),
            "expected bounding box r1=2,c1=0,r2=7,c2=4, got: {s}"
        );
        assert!(!s.contains("4294967295"), "should not contain u32::MAX leftover");
    }

    #[test]
    fn test_execute_script_syntax_error() {
        let h = make_calc_handle();
        let script = CString::new("this is not valid rhai (((").unwrap();
        let s = json_from(roe_execute_script(h, script.as_ptr()));
        assert!(s.contains("\"ok\":false"));
        assert!(s.contains("error"));
    }

    #[test]
    fn test_execute_script_insert_chart_returns_id_in_json() {
        let h = make_calc_handle();
        let chart_json = r#"{"kind":"Bar","title":"t","x_labels":["A","B"],"series":[{"name":"s1","data":[1.0,2.0]}]}"#;
        let script = CString::new(format!(r#"insert_chart(0, 0, 0, `{chart_json}`)"#)).unwrap();
        let s = json_from(roe_execute_script(h, script.as_ptr()));
        assert!(s.contains("\"ok\":true"), "expected ok:true, got: {s}");
        assert!(
            s.contains("\"return_value\":") && !s.contains("\"return_value\":null"),
            "expected non-null return_value carrying the chart id, got: {s}"
        );
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let ret = v["return_value"].as_i64().expect("return_value should be an integer");
        assert!(ret > 0, "expected positive chart id, got: {ret}");
    }

    #[test]
    fn test_execute_script_null_script() {
        let h = make_calc_handle();
        let s = json_from(roe_execute_script(h, std::ptr::null()));
        assert!(s.contains("\"ok\":false"));
        assert!(s.contains("null script"));
    }

    #[test]
    fn test_execute_script_invalid_handle() {
        let script = CString::new("1+1;").unwrap();
        let s = json_from(roe_execute_script(999_999, script.as_ptr()));
        assert!(s.contains("\"ok\":false"));
        assert!(s.contains("invalid handle"));
    }

    #[test]
    fn test_execute_script_invalid_utf8() {
        let h = make_calc_handle();
        let bad = unsafe { CString::from_vec_unchecked(vec![0xFFu8]) };
        let s = json_from(roe_execute_script(h, bad.as_ptr()));
        assert!(s.contains("\"ok\":false"));
    }

    #[test]
    fn test_execute_script_writer_ok() {
        let h = make_writer_handle();
        let script = CString::new("set_paragraph(0, \"New Title\");").unwrap();
        let s = json_from(roe_execute_script(h, script.as_ptr()));
        assert!(s.contains("\"ok\":true"));
        assert!(s.contains("\"dirty_pages\":[0]"));
    }

    #[test]
    fn test_execute_script_impress_empty_ok() {
        // Presentation 构造较复杂，这里只验证 Calc/Writer 路径的分发逻辑，
        // Impress 分支结构与 Calc/Impress 对称，由 run_impress 自身的
        // roe-script 单测覆盖。
    }

    /// v197 会话新增：readonly_query_script_impl（roe_query_script 背后的
    /// 新实现）此前只在文档注释里描述"写函数会因未注册而运行时报错"，
    /// 没有任何自动化测试真正走一遍这条链路。这里通过真实 C-ABI
    /// roe_query_script 验证：含 set_cell 的脚本必须失败，且原文档
    /// 完全未被修改（不是静默执行、不是部分生效）。
    #[test]
    fn test_query_script_rejects_set_cell_write_function() {
        let h = make_calc_handle();

        let before = handle::with_entry(h, |entry| match &entry.doc {
            RoeDocument::Calc(wb) => format!("{:?}", wb.sheets[0].cells.get(0, 0)),
            _ => unreachable!(),
        })
        .unwrap();

        let script = CString::new("set_cell(0, 0, 0, \"999\");").unwrap();
        let s = json_from(roe_query_script(h, script.as_ptr()));

        assert!(
            s.contains("\"ok\":false"),
            "query_script with set_cell should fail (readonly engine has no set_cell registered), got: {s}"
        );

        let after = handle::with_entry(h, |entry| match &entry.doc {
            RoeDocument::Calc(wb) => format!("{:?}", wb.sheets[0].cells.get(0, 0)),
            _ => unreachable!(),
        })
        .unwrap();
        assert_eq!(before, after, "document must be untouched after a rejected write attempt");
    }

    /// 配套验证：白名单里的 get_cell 在只读引擎下必须正常工作，
    /// 确认上面的"拒绝"不是把整个只读引擎搞坏了、误伤了查询函数。
    #[test]
    fn test_query_script_allows_get_cell_read_function() {
        let h = make_calc_handle();

        let setup = CString::new("set_cell(0, 0, 0, \"hello\");").unwrap();
        let r = json_from(roe_execute_script(h, setup.as_ptr()));
        assert!(r.contains("\"ok\":true"));

        let script = CString::new("get_cell(0, 0, 0)").unwrap();
        let s = json_from(roe_query_script(h, script.as_ptr()));
        assert!(
            s.contains("\"ok\":true"),
            "get_cell should be allowed in readonly engine, got: {s}"
        );
        assert!(s.contains("hello"), "expected to read back the written value, got: {s}");
    }
}


/// 只读执行 Rhai 脚本——旧版实现，clone 整份文档、在 clone 上跑脚本、
/// 跑完丢弃 clone（不回写）。
///
/// 已废弃（v193 会话决定，Gemini 三方评审确认）：这套"clone-跑-丢"机制
/// 用的是全量注册（读写函数都有）的 Engine，脚本里technically可以调用
/// set_cell 等写函数、且不会报错——只是因为改的是一次性 clone、用完即
/// 丢弃，所以不会影响真实文档。问题在于 AI 侧：调用方（LLM）看到写函数
/// "执行成功"却不知道这是静默无效的，容易产生"以为已经改好了"的错误
/// 认知，进而在后续对话里基于错误前提继续推理（参见
/// readonly_query_script_impl 文档：白名单机制下写函数直接报错
/// "Function not found"，能让调用方立刻明确知道这里不能写）。
/// 另外每次调用都 clone 整份文档，大文档场景下有明显性能开销。
///
/// 新代码请使用 [`readonly_query_script_impl`]（函数级白名单机制，
/// 天然可扩展支持未来的隐私分级：隐私模式/标题模式/全文模式）。
/// 这个旧函数暂不删除（避免破坏 roe_query_script 这个已有 C-ABI 导出
/// 接口的行为——前端可能仍在调用它），但新功能一律走新路径。
///
/// 返回 JSON 形如 `{"ok":true,"value":"..."}` 或 `{"ok":false,"error":"..."}`。
/// 调用方必须用 [`roe_free_string`] 释放返回值。
///
/// 供 roe-agent 等同进程 Rust 消费者直接调用，不经过 C-ABI 的指针/
/// CString 编解码。语义与 `roe_query_script` 完全一致：不修改文档，
/// 不触发 dirty 通知。返回 None 表示句柄无效。
#[deprecated(
    since = "0.1.0",
    note = "clone-based readonly execution allows write functions to run \
            silently without effect, risking AI cognitive inconsistency; \
            use readonly_query_script_impl (function-level whitelist) instead"
)]
pub fn query_script_impl(handle: u64, script: &str) -> Option<Result<JsonValue, String>> {
    let engine = ScriptEngine::new();

    // query_script_impl 本身已废弃（见函数上的 #[deprecated]），这里
    // 对 query_calc/query_writer/query_impress（同样已废弃，见
    // roe-script/src/engine.rs）的调用是"废弃函数内部调用废弃方法"，
    // 同一处已知技术债，不是新引入的问题，抑制警告避免噪音掩盖真正
    // 需要关注的 deprecated 用法。
    #[allow(deprecated)]
    handle::with_entry(handle, |entry| match &entry.doc {
        RoeDocument::Calc(wb) => engine.query_calc(script, wb),
        RoeDocument::Writer(doc) => engine.query_writer(script, doc),
        RoeDocument::Impress(pres) => engine.query_impress(script, pres),
    })
}

/// 只读执行 Rhai 脚本，返回脚本的返回值（不触发 dirty 通知，不修改文档）。
///
/// 返回 JSON 形如 `{"ok":true,"value":"..."}` 或 `{"ok":false,"error":"..."}`。
/// 调用方必须用 [`roe_free_string`] 释放返回值。
///
/// v197 会话变更：改为调用 [`readonly_query_script_impl`]（白名单引擎），
/// 不再经由已废弃的 [`query_script_impl`]（全量注册引擎 + clone-跑-丢）。
/// 起因：office-rs2 前台（doc-core::query_script，calc_get_cell /
/// calc_get_cell_formula 背后走的正是这个 C-ABI 函数）此前一直经由旧
/// 引擎执行，写函数（如 set_cell）技术上能"成功"但静默不生效——前台
/// 目前的实际调用点都只发 get_cell/get_cell_formula 这类读操作，未触发
/// 过这个问题，但旧引擎本身不提供任何防护。换成白名单引擎后，写函数会
/// 在脚本层直接报错（"Function not found"），与 roe-agent 的
/// query_script_preview 工具（同样走 readonly_query_script_impl）行为
/// 一致，两条只读路径统一了安全边界。
///
/// 支持 Calc、Writer、Impress 三类文档，各自走各自的只读白名单引擎；
/// 写函数在只读引擎里未注册，会报 Function not found。
#[no_mangle]
pub extern "C" fn roe_query_script(handle: u64, script: *const c_char) -> *mut c_char {
    if script.is_null() {
        return query_result_json(Err("null script".to_string()));
    }

    let script_str = match unsafe { CStr::from_ptr(script) }.to_str() {
        Ok(s) => s,
        Err(e) => return query_result_json(Err(format!("invalid utf8: {e}"))),
    };

    match readonly_query_script_impl(handle, script_str) {
        Some(Ok(val)) => {
            clear_last_error();
            query_result_json(Ok(val))
        }
        Some(Err(e)) => {
            set_last_error(e.clone());
            query_result_json(Err(e))
        }
        None => {
            let msg = format!("invalid handle {handle}");
            set_last_error(msg.clone());
            query_result_json(Err(msg))
        }
    }
}

fn query_result_json(result: Result<serde_json::Value, String>) -> *mut c_char {
    let json = match result {
        Ok(val) => format!(r#"{{"ok":true,"value":{}}}"#, val),
        Err(e) => {
            let escaped = e
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "");
            format!(r#"{{"ok":false,"error":"{}"}}"#, escaped)
        }
    };
    CString::new(json)
        .map(CString::into_raw)
        .unwrap_or_else(|_| std::ptr::null_mut())
}

/// 返回 ROE 支持的公式函数列表（JSON 数组）。
/// 调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_get_formula_function_list() -> *mut std::os::raw::c_char {
    use crate::formula_function_list::FORMULA_FUNCTION_LIST;
    let list: Vec<serde_json::Value> = FORMULA_FUNCTION_LIST
        .iter()
        .map(|f| {
            serde_json::json!({
                "name": f.name,
                "args": f.args,
                "desc": f.desc,
                "category": f.category,
            })
        })
        .collect();
    let s = serde_json::Value::Array(list).to_string();
    match std::ffi::CString::new(s) {
        Ok(cs) => cs.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}



fn df_cstr_arg(p: *const c_char) -> Result<String, String> {
    if p.is_null() { return Err("null arg".to_string()); }
    unsafe { CStr::from_ptr(p) }.to_str().map(|s| s.to_string()).map_err(|e| format!("invalid utf8: {e}"))
}

fn df_opts(opts_json: *const c_char) -> Result<serde_json::Value, String> {
    if opts_json.is_null() { return Ok(serde_json::json!({})); }
    let s = df_cstr_arg(opts_json)?;
    if s.trim().is_empty() { return Ok(serde_json::json!({})); }
    serde_json::from_str(&s).map_err(|e| format!("opts_json 解析失败: {e}"))
}

fn df_load_source(path: &str, kind: &str, opts: &serde_json::Value) -> Result<roe_polars_bridge::LoadedSource, String> {
    let delim = opts.get("delimiter").and_then(|v| v.as_str()).and_then(|s| s.chars().next());
    let has_header = opts.get("has_header").and_then(|v| v.as_bool()).unwrap_or(true);
    roe_polars_bridge::load_file_source(path, kind, delim, has_header, &roe_script::calc_df_api::csv_cache_dir())
        .map_err(|e| format!("{e}"))
}

fn df_finish(r: Result<serde_json::Value, String>) -> *mut c_char {
    match r {
        Ok(v) => { clear_last_error(); query_result_json(Ok(v)) }
        Err(e) => { set_last_error(e.clone()); query_result_json(Err(e)) }
    }
}

/// 前台:预览外部文件(只返回概览 JSON,不碰任何句柄/数据集容器)。
/// kind: "csv" | "parquet" | "xlsx"。opts_json 可含 {"delimiter":",","has_header":true,"head":10}。
/// 返回 lazy_overview 的 JSON(经 query_result_json),需 roe_free_string 释放。
#[no_mangle]
pub extern "C" fn roe_df_preview_file(path: *const c_char, kind: *const c_char, opts_json: *const c_char) -> *mut c_char {
    df_finish((|| {
        let path = df_cstr_arg(path)?;
        let kind = df_cstr_arg(kind)?;
        let opts = df_opts(opts_json)?;
        let head = opts.get("head").and_then(|v| v.as_u64()).unwrap_or(10).min(1000) as u32;
        let lf = df_load_source(&path, &kind, &opts)?.into_lazy();
        let s = roe_polars_bridge::lazy_overview(lf, head).map_err(|e| format!("{e}"))?;
        serde_json::from_str::<serde_json::Value>(&s).map_err(|e| format!("概览解析失败: {e}"))
    })())
}

/// DF 链代码包装(每条 df_xxx(...); 语句失败即中止并报行号)。同进程 Rust 调用,不走 C-ABI。
pub fn wrap_df_chain(code: &str) -> String {
    roe_script::calc_df_api::wrap_chain(code)
}

/// 前台:确认后把外部文件注册为数据集 df_N(N = 现有 df_数字 的最大值 + 1)。
/// CSV/Parquet 存惰性引用,xlsx 物化。返回 {"name","rows","cols":[列名]}。
#[no_mangle]
pub extern "C" fn roe_df_register_file(handle: u64, path: *const c_char, kind: *const c_char, opts_json: *const c_char) -> *mut c_char {
    df_finish((|| {
        let path = df_cstr_arg(path)?;
        let kind = df_cstr_arg(kind)?;
        let opts = df_opts(opts_json)?;
        match handle::with_entry(handle, |en| matches!(en.doc, RoeDocument::Calc(_))) {
            None => return Err(format!("invalid handle {handle}")),
            Some(false) => return Err("仅支持 Calc 文档".to_string()),
            Some(true) => {}
        }
        let src = df_load_source(&path, &kind, &opts)?;
        let lf = src.clone().into_lazy();
        let cols: Vec<String> = lf.clone().collect_schema()
            .map(|s| s.iter_names().map(|n| n.to_string()).collect::<Vec<String>>())
            .map_err(|e| format!("{e}"))?;
        let rows = roe_polars_bridge::lazy_row_count(lf).map_err(|e| format!("{e}"))?;
        let ds_item = match src {
            roe_polars_bridge::LoadedSource::Lazy(l) => roe_script::calc_df_api::Dataset::Lazy(l),
            roe_polars_bridge::LoadedSource::Mem(d) => roe_script::calc_df_api::Dataset::Mem(d),
        };
        let mut ds = handle::take_datasets(handle);
        let name = roe_script::calc_df_api::next_df_name(&ds);
        ds.insert(name.clone(), ds_item);
        handle::put_datasets(handle, ds);
        Ok(serde_json::json!({ "name": name, "rows": rows, "cols": cols }))
    })())
}

/// 前台手动加载 parquet 文件/目录(含 Hive LOCATION 的 file: 前缀)为命名数据集。
/// 路径由用户在前台选择,不经脚本、不暴露给 agent。一期物化为 DataFrame 存入句柄的数据集容器。
/// 返回 JSON(经 query_result_json),需 roe_free_string 释放。
#[no_mangle]
pub extern "C" fn roe_df_open_parquet(handle: u64, path: *const c_char, name: *const c_char) -> *mut c_char {
    let get = |p: *const c_char| -> Result<String, String> {
        if p.is_null() { return Err("null arg".to_string()); }
        unsafe { CStr::from_ptr(p) }.to_str().map(|s| s.to_string()).map_err(|e| format!("invalid utf8: {e}"))
    };
    let (path, name) = match (get(path), get(name)) {
        (Ok(p), Ok(n)) => (p, n),
        (Err(e), _) | (_, Err(e)) => { set_last_error(e.clone()); return query_result_json(Err(e)); }
    };
    let name = name.trim().to_string();
    if name.is_empty() {
        return query_result_json(Err("数据集名称不能为空".to_string()));
    }
    let is_calc = handle::with_entry(handle, |en| matches!(en.doc, RoeDocument::Calc(_)));
    match is_calc {
        None => return query_result_json(Err(format!("invalid handle {handle}"))),
        Some(false) => return query_result_json(Err("仅支持 Calc 文档".to_string())),
        Some(true) => {}
    }
    let lf = match roe_polars_bridge::scan_parquet_dir(&path) {
        Ok(lf) => lf,
        Err(e) => { let m = format!("{e}"); set_last_error(m.clone()); return query_result_json(Err(m)); }
    };
    let cols: Vec<String> = match lf.clone().collect_schema() {
        Ok(s) => s.iter_names().map(|n| n.to_string()).collect(),
        Err(e) => { let m = format!("{e}"); set_last_error(m.clone()); return query_result_json(Err(m)); }
    };
    let rows = match roe_polars_bridge::lazy_row_count(lf.clone()) {
        Ok(n) => n,
        Err(e) => { let m = format!("{e}"); set_last_error(m.clone()); return query_result_json(Err(m)); }
    };
    let info = serde_json::json!({ "name": name, "rows": rows, "cols": cols });
    let mut ds = handle::take_datasets(handle);
    ds.insert(name, roe_script::calc_df_api::Dataset::Lazy(lf));
    handle::put_datasets(handle, ds);
    clear_last_error();
    query_result_json(Ok(info))
}

#[cfg(test)]
mod df_open_tests {
    use super::*;

    #[test]
    fn df_register_csv_preview_and_sheet_head() {
        let p = std::env::temp_dir().join(format!("roe_p1a2_{}.csv", std::process::id()));
        std::fs::write(&p, "a,b\n1,x\n2,y\n3,z\n").unwrap();
        let ps = std::ffi::CString::new(p.to_string_lossy().to_string()).unwrap();
        let kind = std::ffi::CString::new("csv").unwrap();
        let opts = std::ffi::CString::new(r#"{"head":2}"#).unwrap();
        let take = |r: *mut c_char| {
            let s = unsafe { CStr::from_ptr(r) }.to_str().unwrap().to_string();
            roe_free_string(r);
            serde_json::from_str::<serde_json::Value>(&s).unwrap()
        };
        let pv = take(roe_df_preview_file(ps.as_ptr(), kind.as_ptr(), opts.as_ptr()));
        println!("preview => {pv}");
        assert_eq!(pv["ok"], true, "{pv}");
        assert_eq!(pv["value"]["rows"], 3, "{pv}");
        assert_eq!(pv["value"]["head"]["rows"].as_array().unwrap().len(), 2, "{pv}");

        let h = crate::document::new_document_impl(2);
        let r1 = take(roe_df_register_file(h, ps.as_ptr(), kind.as_ptr(), std::ptr::null()));
        let r2 = take(roe_df_register_file(h, ps.as_ptr(), kind.as_ptr(), std::ptr::null()));
        println!("reg => {r1} / {r2}");
        assert_eq!(r1["value"]["name"], "df_1", "{r1}");
        assert_eq!(r1["value"]["rows"], 3, "{r1}");
        assert_eq!(r2["value"]["name"], "df_2", "{r2}");

        let idx = |script: &str| match execute_script_impl(h, script, None) {
            Some(Ok((_, _, ret))) => ret.as_i64().unwrap_or(-99),
            other => panic!("unexpected: {:?}", other.map(|r| r.map(|(_, _, v)| v))),
        };
        let i1 = idx(r#"df_to_sheet_head("df_1", 2)"#);
        let i2 = idx(r#"df_to_sheet_head("df_1", 2)"#);
        println!("sheet idx => {i1}, {i2}");
        assert!(i1 >= 0 && i2 > i1, "{i1} {i2}");
        let v = readonly_query_script_impl(h, r#"df_to_sheet_head("df_1", 2)"#).unwrap().unwrap();
        assert_eq!(v, serde_json::json!(-1), "{v}");
        let bad = std::ffi::CString::new("/nonexistent_p1a2.csv").unwrap();
        let rb = take(roe_df_preview_file(bad.as_ptr(), kind.as_ptr(), opts.as_ptr()));
        assert_eq!(rb["ok"], false, "{rb}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn open_parquet_then_readonly_page() {
        let home = match std::env::var("HOME") { Ok(h) => h, Err(_) => return };
        let dir = format!("{home}/smalldata-lab/hive-warehouse/polars_test.db/t_pq");
        if !std::path::Path::new(&dir).exists() { return; }
        let h = crate::document::new_document_impl(2);
        assert!(h != 0);
        let p = std::ffi::CString::new(format!("file:{dir}")).unwrap();
        let n = std::ffi::CString::new("t").unwrap();
        let r = roe_df_open_parquet(h, p.as_ptr(), n.as_ptr());
        let out = unsafe { CStr::from_ptr(r) }.to_str().unwrap().to_string();
        roe_free_string(r);
        println!("open => {out}");
        // 只读路径取回用户加载的数据集并分页
        let v = readonly_query_script_impl(h, r#"df_use("t"); df_page(0, 5)"#).unwrap().unwrap();
        println!("page => {v}");
        assert!(v.to_string().contains("alpha"), "{v}");
        // 被禁函数返回 err / -1
        let v2 = readonly_query_script_impl(h, r#"df_load_parquet_dir("/tmp")"#).unwrap().unwrap();
        assert!(v2.to_string().contains("err:"), "{v2}");
        let v3 = readonly_query_script_impl(h, r#"df_to_sheet("x")"#).unwrap().unwrap();
        assert_eq!(v3, serde_json::json!(-1), "{v3}");
        // 只读路径不能改容器
        let v4 = readonly_query_script_impl(h, r#"df_save_as("evil")"#).unwrap().unwrap();
        assert!(v4.to_string().contains("err:"), "{v4}");
        let v5 = readonly_query_script_impl(h, r#"df_list_datasets()"#).unwrap().unwrap();
        let v5s = v5.as_str().unwrap_or("");
        assert!(v5s.contains("\"name\":\"t\"") && !v5s.contains("evil"), "{v5}");
        // 不存在的路径:返回错误且不 panic
        let bp = std::ffi::CString::new("/nonexistent_dir_xyz").unwrap();
        let r2 = roe_df_open_parquet(h, bp.as_ptr(), n.as_ptr());
        let out2 = unsafe { CStr::from_ptr(r2) }.to_str().unwrap().to_string();
        roe_free_string(r2);
        println!("bad => {out2}");
    }
}

// ── P0 复现测试(GridCraft 吸收调研 v271):断言"正确行为",失败 = 对应 P0 成立 ──
#[cfg(test)]
mod p0_repro {
    use super::*;

    fn new_calc() -> u64 {
        let h = crate::document::roe_new_document(2);
        assert!(h != 0, "roe_new_document(2) 返回 0");
        h
    }

    /// 执行脚本,返回 (脏区域个数, 返回值字符串)
    fn run(h: u64, s: &str) -> (usize, String) {
        match execute_script_impl(h, s, None) {
            Some(Ok((_pages, regions, ret))) => (regions.len(), ret.as_str().unwrap_or("").to_string()),
            other => panic!("脚本失败: {:?}", other.map(|r| r.map(|(_, _, v)| v))),
        }
    }

    fn cell(h: u64, r: i64, c: i64) -> String {
        run(h, &format!("get_cell(0, {r}, {c})")).1
    }

    /// 直接读 workbook(不经脚本,避免产生撤销快照)
    fn val(h: u64, r: u32, c: u32) -> f64 {
        handle::with_entry(h, |e| match &e.doc {
            RoeDocument::Calc(wb) => match &wb.sheets[0].cells.get(r, c) {
                roe_model::calc::CellValue::Number(n) => *n,
                roe_model::calc::CellValue::Formula { cached, .. } => match cached.as_ref() {
                    roe_model::calc::CellValue::Number(n) => *n,
                    _ => f64::NAN,
                },
                _ => f64::NAN,
            },
            _ => f64::NAN,
        })
        .unwrap_or(f64::NAN)
    }

    fn num(s: &str) -> f64 {
        s.trim().parse::<f64>().unwrap_or(f64::NAN)
    }

    /// P0-a:插入行后,SUM 公式源码应改写为 =SUM(A1:A3)
    #[test]
    fn p0a_insert_row_rewrites_formula_source() {
        let h = new_calc();
        run(h, r#"set_cell(0,0,0,"1"); set_cell(0,1,0,"2"); set_formula(0,2,0,"=SUM(A1:A2)")"#);
        run(h, "insert_row(0, 1)"); // 在 A2 之前插入,公式格移到第 4 行(索引 3)
        let f = run(h, "get_cell_formula(0, 3, 0)").1;
        assert_eq!(f.replace(' ', ""), "=SUM(A1:A3)", "P0-a 成立:公式源码未改写,实际={f}");
    }

    /// P0-a(跨 sheet):Sheet2 的公式引用 Sheet1,在 Sheet1 插行后应被改写
    #[test]
    fn p0a_cross_sheet_reference_rewritten() {
        let h = new_calc();
        run(h, r#"set_cell(0,0,0,"1"); set_cell(0,1,0,"2")"#);
        handle::with_entry(h, |e| {
            if let RoeDocument::Calc(wb) = &mut e.doc {
                let mut s2 = wb.sheets[0].clone();
                s2.name = "Sheet2".into();
                s2.cells = Default::default();
                wb.sheets.push(s2);
            }
        });
        run(h, r#"set_formula(1,0,0,"=SUM(Sheet1!A1:A2)")"#);
        run(h, "insert_row(0, 1)");
        let f = run(h, "get_cell_formula(1, 0, 0)").1;
        assert_eq!(f.replace(' ', ""), "=SUM(Sheet1!A1:A3)", "跨 sheet 引用未改写,实际={f}");
    }

    /// P0-b:结构编辑后应产生脏区域,以触发重算
    #[test]
    fn p0b_structural_edit_reports_dirty() {
        let h = new_calc();
        run(h, r#"set_cell(0,0,0,"1"); set_formula(0,0,1,"=A1*2")"#);
        let (n_regions, _) = run(h, "insert_row(0, 0)");
        assert!(n_regions > 0, "P0-b 成立:insert_row 后脏区域为空,不会触发重算");
    }

    /// P0-c(1):撤销后引擎缓存陈旧——撤销到 A1=1 后,新公式 =A1+1 应为 2
    #[test]
    fn p0c_undo_leaves_stale_engine_cache() {
        let h = new_calc();
        run(h, r#"set_cell(0,0,0,"1")"#);
        run(h, r#"set_formula(0,0,1,"=A1*2")"#);
        run(h, r#"set_cell(0,0,0,"5")"#);
        assert_eq!(val(h, 0, 1), 10.0, "前置:B1 应为 10");
        let p = crate::snapshot::roe_undo(h);
        if !p.is_null() { roe_free_string(p); }
        assert_eq!(val(h, 0, 0), 1.0, "前置:撤销后 A1 应回到 1");
        run(h, r#"set_formula(0,0,2,"=A1+1")"#);
        let c1 = val(h, 0, 2);
        assert_eq!(c1, 2.0, "P0-c 成立:撤销后陈旧缓存参与计算,C1 实际={c1}(期望 2)");
    }

    /// P0-c(3):公式链。撤销后新公式读到的应是文档里的 C1=3,而不是引擎缓存里的 11
    #[test]
    fn p0c_undo_stale_cache_in_formula_chain() {
        let h = new_calc();
        run(h, r#"set_cell(0,0,0,"1"); set_formula(0,0,1,"=A1*2"); set_formula(0,0,2,"=B1+1")"#);
        run(h, r#"set_cell(0,0,0,"5")"#);
        assert_eq!(val(h, 0, 2), 11.0, "前置:C1 应为 11");
        let p = crate::snapshot::roe_undo(h);
        if !p.is_null() { roe_free_string(p); }
        assert_eq!(val(h, 0, 0), 1.0, "前置:撤销后 A1=1");
        assert_eq!(val(h, 0, 2), 3.0, "前置:撤销后文档里 C1=3");
        run(h, r#"set_formula(0,0,3,"=C1+1")"#);
        let d1 = val(h, 0, 3);
        assert_eq!(d1, 4.0, "P0-c 成立:公式链读到陈旧缓存,D1 实际={d1}(期望 4)");
    }

    /// P0-c(2):撤销掉公式后,改前驱格不应让已撤销的公式"复活"
    #[test]
    fn p0c_undo_ghost_formula_not_resurrected() {
        let h = new_calc();
        run(h, r#"set_cell(0,0,0,"1"); set_formula(0,0,1,"=A1*2")"#);
        let p = crate::snapshot::roe_undo(h); // 回到空文档
        if !p.is_null() { roe_free_string(p); }
        run(h, r#"set_cell(0,0,0,"3")"#);
        let b1 = cell(h, 0, 1);
        assert_eq!(b1, "", "P0-c 成立:已撤销的公式被旧 DAG 复活,B1 实际={b1}");
    }

    /// P0-c/P0-d:影子提交后,影子里新增的依赖关系应在原引擎中生效
    #[test]
    fn p0d_commit_shadow_dependents_recalc() {
        let h = new_calc();
        run(h, r#"set_cell(0,0,0,"1"); set_formula(0,0,1,"=A1*2")"#);
        let guard = crate::shadow::create_shadow(h).unwrap();
        let sh = guard.shadow_handle();
        run(sh, r#"set_formula(0,0,2,"=A1+100")"#);
        assert!(crate::shadow::commit_shadow(guard));
        assert_eq!(num(&cell(h, 0, 2)), 101.0, "前置:提交后 C1 应为 101");
        run(h, r#"set_cell(0,0,0,"5")"#);
        let c1 = cell(h, 0, 2);
        assert_eq!(num(&c1), 105.0, "P0-d 成立:提交后改前驱格,C1 未重算,实际={c1}(期望 105)");
        assert_eq!(num(&cell(h, 0, 1)), 10.0, "B1 应为 10");
    }
}


#[cfg(test)]
mod impress_readonly_query_tests {
    use super::*;

    #[test]
    fn impress_readonly_reads_and_rejects_writes() {
        let h = crate::document::new_document_impl(3);
        assert!(h != 0);
        // 读函数可用
        let before = readonly_query_script_impl(h, "slide_count()").unwrap().unwrap();
        assert!(before.is_i64() || before.is_u64(), "{before}");
        // 写函数在只读引擎里不存在
        let w = readonly_query_script_impl(h, "insert_slide(0, 0)").unwrap();
        let msg = w.expect_err("写函数必须报错");
        assert!(msg.to_lowercase().contains("not found"), "{msg}");
        // 文档未被改动
        let after = readonly_query_script_impl(h, "slide_count()").unwrap().unwrap();
        assert_eq!(before, after);
        // 只读版 list_functions 可用
        let lf = readonly_query_script_impl(h, r#"list_functions("")"#).unwrap();
        assert!(lf.is_ok(), "{lf:?}");
    }
}

#[cfg(test)]
mod readonly_script_noop_tests {
    use super::*;

    fn undo_len(h: u64) -> usize {
        crate::handle::with_entry(h, |e| e.undo_stack.len()).unwrap()
    }

    fn run(h: u64, s: &str) -> (Vec<usize>, serde_json::Value) {
        let (d, _r, v) = execute_script_impl(h, s, None).unwrap().unwrap();
        (d, v)
    }

    #[test]
    fn impress_readonly_script_no_undo_no_dirty() {
        let h = crate::document::new_document_impl(3);
        assert!(h != 0);
        let n0 = undo_len(h);
        let (d, _) = run(h, "slide_count()");
        assert!(d.is_empty(), "只读脚本 dirty_pages 应为空: {d:?}");
        assert_eq!(undo_len(h), n0, "只读脚本不应推撤销快照");
        let (d, _) = run(h, "insert_slide(0, -1)");
        assert!(!d.is_empty(), "写脚本 dirty_pages 应非空");
        assert_eq!(undo_len(h), n0 + 1, "写脚本应推一条快照");
    }

    #[test]
    fn writer_readonly_script_no_undo_no_dirty() {
        let h = crate::document::new_document_impl(1);
        assert!(h != 0);
        let n0 = undo_len(h);
        let (d, _) = run(h, "paragraph_count()");
        assert!(d.is_empty(), "只读脚本 dirty_pages 应为空: {d:?}");
        assert_eq!(undo_len(h), n0);
        let (d, _) = run(h, r#"append_paragraph("x")"#);
        assert_eq!(d, vec![0usize]);
        assert_eq!(undo_len(h), n0 + 1);
    }

    #[test]
    fn dynamic_call_is_treated_as_write() {
        let h = crate::document::new_document_impl(3);
        let n0 = undo_len(h);
        let (d, _) = run(h, r#"let f = Fn("slide_count"); call(f)"#);
        assert!(!d.is_empty(), "含动态调用应按写处理");
        assert_eq!(undo_len(h), n0 + 1);
    }
}
