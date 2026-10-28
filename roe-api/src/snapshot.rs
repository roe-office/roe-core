//! roe_begin_edit / roe_commit_edit / roe_discard_edit
//!
//! 对应 office-rs 的 activateShadow / commitShadow / discardShadow。
//! v1：snapshot = entry.doc 的整体 Clone（RoeDocument 及其三个内存模型
//! 均已 derive Clone）。Clone 性能暂不预判，先跑通；如果实测大文档
//! clone 太慢，再考虑 COW / im / rpds（接口形态不变）。

use std::ffi::CString;
use std::os::raw::c_char;

use crate::error::{set_last_error, RoeErrorCode};
use crate::handle;

/// 整份文档被换回旧版本后,把被换下来的文档里各 sheet 的 next_object_id 合并回来(取 max),
/// 保证撤销/重做/丢弃编辑不会让浮动对象 id 被复用。
fn merge_object_counters(replaced: &roe_parser::RoeDocument, current: &mut roe_parser::RoeDocument) {
    if let (roe_parser::RoeDocument::Calc(a), roe_parser::RoeDocument::Calc(b)) = (replaced, current) {
        roe_model::object_edit::merge_next_object_ids(&a.sheets, &mut b.sheets);
    }
}

/// 克隆当前文档作为快照，进入编辑模式。
/// 成功返回 ROE_OK(0)；句柄无效返回 ROE_INVALID_HANDLE(-1)。
#[no_mangle]
pub extern "C" fn roe_begin_edit(handle: u64) -> i32 {
    match handle::with_entry(handle, |entry| {
        entry.snapshot = Some(entry.doc.clone());
    }) {
        Some(()) => RoeErrorCode::Ok as i32,
        None => {
            set_last_error(format!("roe_begin_edit: invalid handle {handle}"));
            RoeErrorCode::InvalidHandle as i32
        }
    }
}

/// 提交编辑：丢弃快照，保留当前模型。
/// 成功返回 ROE_OK(0)；句柄无效返回 ROE_INVALID_HANDLE(-1)。
///
/// 若提交的是 Writer 文档，提交后失效 writer_layout_cache（2026-06-18
/// P0 接入）：编辑期间 entry.doc 可能已被脚本等途径修改过内容，提交后
/// 旧布局缓存不再可信，否则后续 hit_test / query_layout_rect 会基于
/// 过时布局返回错误结果。
#[no_mangle]
pub extern "C" fn roe_commit_edit(handle: u64) -> i32 {
    let result = handle::with_entry(handle, |entry| {
        // 把编辑前的快照存入 undo_stack（而不是直接丢弃），这样
        // roe_commit_edit 在原有"结束编辑、保留改动"的语义之上，
        // 顺带建立了撤销历史。没有 begin_edit 过的提交（snapshot 为
        // None）没有"编辑前"状态可存，直接跳过，不算错误。
        if let Some(prev) = entry.snapshot.take() {
            // v115 undo/redo 光标定位方案：roe_commit_edit 是
            // begin_edit/commit_edit 手写事务对（死代码路径，text_edit.rs
            // 各 xxx_in_entry 函数不走这条路，自己手写等价逻辑），这里
            // 没有"编辑前光标位置"这个概念可用（调用方可能是 Calc 改单元
            // 格之类，不是文本编辑），占位成 (0, 0)，不代表真实光标位置。
            // 阶段C v170：roe_commit_edit 是通用的手写事务对入口
            // （Writer 走 text_edit.rs 各 xxx_in_entry 自己的等价逻辑，
            // 不走这条路），这里没有"编辑前光标位置"这个概念可用
            // （调用方可能是 Calc 改单元格、Impress 改形状之类），
            // 用 None 表示"无明确焦点"，替代之前 (0,0) 的数字占位——
            // (0,0) 曾被误读为"第0段第0个字符"，None 语义更明确。
            let ctx = handle::EditCursorContext::None;
            // 阶段C v170 重构：push + 超限丢弃 + 清空redo_stack 四步
            // 收进 DocumentEntry::push_undo_snapshot，见 handle.rs。
            entry.push_undo_snapshot(prev, ctx);
        }
        matches!(entry.doc, roe_parser::RoeDocument::Writer(_))
    });

    match result {
        Some(is_writer) => {
            // 必须在上面 with_entry 闭包返回之后调用，否则对同一 handle
            // 的 RefCell 会嵌套借用而 panic。
            if is_writer {
                handle::invalidate_writer_layout_cache(handle);
            }
            RoeErrorCode::Ok as i32
        }
        None => {
            set_last_error(format!("roe_commit_edit: invalid handle {handle}"));
            RoeErrorCode::InvalidHandle as i32
        }
    }
}

/// 丢弃编辑：用快照覆盖当前模型（回滚）。
/// 成功返回 ROE_OK(0)；句柄无效返回 ROE_INVALID_HANDLE(-1)；
/// 无快照（未 begin_edit）返回 ROE_SNAPSHOT_ERROR(-8)。
///
/// 若回滚的是 Writer 文档，回滚后失效 writer_layout_cache（2026-06-18
/// P0 补全）：用快照覆盖 entry.doc 后，当前缓存对应的是回滚前的内容，
/// 已不可信，否则后续 hit_test / query_layout_rect 会基于过时布局返回
/// 错误结果。
#[no_mangle]
pub extern "C" fn roe_discard_edit(handle: u64) -> i32 {
    let result = handle::with_entry(handle, |entry| match entry.snapshot.take() {
        Some(snap) => {
            let is_writer = matches!(snap, roe_parser::RoeDocument::Writer(_));
            let replaced = std::mem::replace(&mut entry.doc, snap);
            merge_object_counters(&replaced, &mut entry.doc);
            (RoeErrorCode::Ok as i32, is_writer)
        }
        None => (RoeErrorCode::SnapshotError as i32, false),
    });

    match result {
        Some((code, is_writer)) => {
            if code == RoeErrorCode::SnapshotError as i32 {
                set_last_error(format!(
                    "roe_discard_edit: no snapshot (handle {handle} never called roe_begin_edit)"
                ));
            } else if is_writer {
                // 必须在 with_entry 闭包返回之后调用，避免 RefCell 嵌套借用 panic。
                handle::invalidate_writer_layout_cache(handle);
            }
            code
        }
        None => {
            set_last_error(format!("roe_discard_edit: invalid handle {handle}"));
            RoeErrorCode::InvalidHandle as i32
        }
    }
}

/// 撤销上一步提交的编辑：把 undo_stack 栈顶弹出覆盖当前文档，
/// 同时把当前文档（撤销前的状态）推入 redo_stack，供 roe_redo 找回。
/// 成功返回 ROE_OK(0)；句柄无效返回 ROE_INVALID_HANDLE(-1)；
/// undo_stack 为空（没有可撤销的历史）返回 ROE_NO_UNDO_HISTORY(-9)。
///
/// 注意：调用前如果存在未提交的编辑（entry.snapshot 是 Some，即
/// begin_edit 之后还没 commit/discard），本函数不处理这种情况——
/// 调用方应先 commit 或 discard 当前编辑，再调用 undo。这与大多数
/// 编辑器的行为一致（撤销作用于已提交的历史，不影响进行中的编辑）。
/// v115 方案：返回值从 i32 状态码改为 JSON 字符串（对齐 text_edit.rs
/// 系列函数的返回风格），携带 undo 后光标应恢复到的位置（编辑发生前的
/// 锚点/焦点，见 handle::EditCursorContext）。失败返回 null，调用方用
/// roe_last_error_message 取详情，与项目里其它函数的既有约定一致。
/// 调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_undo(handle: u64) -> *mut c_char {
    let result = handle::with_entry(handle, |entry| match entry.undo_stack.pop() {
        Some((prev, ctx)) => {
            let current = std::mem::replace(&mut entry.doc, prev);
            merge_object_counters(&current, &mut entry.doc);
            // P0-c/P0-d 修复:doc 被整体替换后,旧引擎的 DAG 与缓存已失效,
            // 丢弃并全量重算(复用打开文档时的同一入口)。
            let fresh = crate::document::init_calc_engine_for_doc(&mut entry.doc);
            entry.calc_engine = fresh;
            // redo_stack 这一侧目前没有更好的 context 来源：redo 时要
            // 恢复的是"撤销前"的文档状态，但撤销前的光标信息已经不在
            // 我们手上（那是下一次操作要处理的东西，不是这次 undo 产生
            // 的）。用同一个 ctx 占位，保持类型一致；真正显示给用户的
            // 是 undo 弹栈时的 ctx（下面返回的那个），不是这里存的。
            entry.redo_stack.push((current, ctx));
            let is_writer = matches!(entry.doc, roe_parser::RoeDocument::Writer(_));
            (Some(ctx), is_writer)
        }
        None => (None, false),
    });

    match result {
        Some((Some(ctx), is_writer)) => {
            if is_writer {
                // 必须在 with_entry 闭包返回之后调用，避免 RefCell 嵌套借用 panic。
                handle::invalidate_writer_layout_cache(handle);
            }
            match CString::new(ctx.to_json()) {
                Ok(cs) => cs.into_raw(),
                Err(e) => {
                    set_last_error(format!("roe_undo: {e}"));
                    std::ptr::null_mut()
                }
            }
        }
        Some((None, _)) => {
            set_last_error(format!("roe_undo: no undo history (handle {handle})"));
            std::ptr::null_mut()
        }
        None => {
            set_last_error(format!("roe_undo: invalid handle {handle}"));
            std::ptr::null_mut()
        }
    }
}

/// 重做：撤销 roe_undo 的效果。把 redo_stack 栈顶弹出覆盖当前文档，
/// 同时把当前文档推回 undo_stack。
/// 成功返回 ROE_OK(0)；句柄无效返回 ROE_INVALID_HANDLE(-1)；
/// redo_stack 为空（没有可重做的历史，或上一步操作不是 undo）返回
/// ROE_NO_REDO_HISTORY(-10)。
/// v115 方案：返回值从 i32 状态码改为 JSON 字符串，语义同 roe_undo
/// （见该函数文档注释）。携带 redo 后光标应恢复到的位置。失败返回
/// null。调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_redo(handle: u64) -> *mut c_char {
    let result = handle::with_entry(handle, |entry| match entry.redo_stack.pop() {
        Some((next, ctx)) => {
            let current = std::mem::replace(&mut entry.doc, next);
            merge_object_counters(&current, &mut entry.doc);
            // P0-c/P0-d 修复:doc 被整体替换后,旧引擎的 DAG 与缓存已失效,
            // 丢弃并全量重算(复用打开文档时的同一入口)。
            let fresh = crate::document::init_calc_engine_for_doc(&mut entry.doc);
            entry.calc_engine = fresh;
            entry.undo_stack.push((current, ctx));
            let is_writer = matches!(entry.doc, roe_parser::RoeDocument::Writer(_));
            (Some(ctx), is_writer)
        }
        None => (None, false),
    });

    match result {
        Some((Some(ctx), is_writer)) => {
            if is_writer {
                handle::invalidate_writer_layout_cache(handle);
            }
            match CString::new(ctx.to_json()) {
                Ok(cs) => cs.into_raw(),
                Err(e) => {
                    set_last_error(format!("roe_redo: {e}"));
                    std::ptr::null_mut()
                }
            }
        }
        Some((None, _)) => {
            set_last_error(format!("roe_redo: no redo history (handle {handle})"));
            std::ptr::null_mut()
        }
        None => {
            set_last_error(format!("roe_redo: invalid handle {handle}"));
            std::ptr::null_mut()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::WriterOptions;
    use crate::handle::DocumentEntry;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use roe_model::style::StyleRegistry;
    use roe_parser::RoeDocument;
    use std::collections::{HashMap, VecDeque};

    fn make_calc_handle(sheet_name: &str) -> u64 {
        let wb = Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                name: sheet_name.into(),
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

    fn sheet0_name(h: u64) -> String {
        handle::with_entry(h, |entry| match &entry.doc {
            RoeDocument::Calc(wb) => wb.sheets[0].name.clone(),
            _ => unreachable!(),
        })
        .unwrap()
    }

    fn has_snapshot(h: u64) -> bool {
        handle::with_entry(h, |entry| entry.snapshot.is_some()).unwrap()
    }

    /// v115 测试 helper：roe_undo/roe_redo 现在返回 *mut c_char（JSON 或
    /// null），测试只关心"成功与否"，用这个 helper 转成 Option<String>
    /// 并负责释放指针，避免每个测试用例里重复写 unsafe 转换代码。
    fn undo_redo_result_to_option(ptr: *mut c_char) -> Option<String> {
        if ptr.is_null() {
            None
        } else {
            unsafe {
                let cs = CString::from_raw(ptr);
                Some(cs.to_string_lossy().into_owned())
            }
        }
    }

    #[test]
    fn test_begin_commit_invalid_handle() {
        assert_eq!(roe_begin_edit(999_999), RoeErrorCode::InvalidHandle as i32);
        assert_eq!(roe_commit_edit(999_999), RoeErrorCode::InvalidHandle as i32);
        assert_eq!(roe_discard_edit(999_999), RoeErrorCode::InvalidHandle as i32);
    }

    #[test]
    fn test_discard_without_begin() {
        let h = make_calc_handle("Data");
        assert_eq!(roe_discard_edit(h), RoeErrorCode::SnapshotError as i32);
    }

    #[test]
    fn test_begin_commit_keeps_changes() {
        let h = make_calc_handle("Data");

        assert_eq!(roe_begin_edit(h), RoeErrorCode::Ok as i32);
        assert!(has_snapshot(h));

        // 编辑：改 sheet 名
        handle::with_entry(h, |entry| match &mut entry.doc {
            RoeDocument::Calc(wb) => wb.sheets[0].name = "Renamed".into(),
            _ => unreachable!(),
        });

        assert_eq!(roe_commit_edit(h), RoeErrorCode::Ok as i32);
        assert!(!has_snapshot(h));
        assert_eq!(sheet0_name(h), "Renamed");

        // commit 后再 discard 应报 SnapshotError（快照已被丢弃）
        assert_eq!(roe_discard_edit(h), RoeErrorCode::SnapshotError as i32);
        assert_eq!(sheet0_name(h), "Renamed");
    }

    #[test]
    fn test_undo_redo_roundtrip() {
        let h = make_calc_handle("Data");

        // 第一次编辑并提交：Data -> Renamed1
        assert_eq!(roe_begin_edit(h), RoeErrorCode::Ok as i32);
        handle::with_entry(h, |entry| match &mut entry.doc {
            RoeDocument::Calc(wb) => wb.sheets[0].name = "Renamed1".into(),
            _ => unreachable!(),
        });
        assert_eq!(roe_commit_edit(h), RoeErrorCode::Ok as i32);
        assert_eq!(sheet0_name(h), "Renamed1");

        // 第二次编辑并提交：Renamed1 -> Renamed2
        assert_eq!(roe_begin_edit(h), RoeErrorCode::Ok as i32);
        handle::with_entry(h, |entry| match &mut entry.doc {
            RoeDocument::Calc(wb) => wb.sheets[0].name = "Renamed2".into(),
            _ => unreachable!(),
        });
        assert_eq!(roe_commit_edit(h), RoeErrorCode::Ok as i32);
        assert_eq!(sheet0_name(h), "Renamed2");

        // undo 一次：回到 Renamed1
        assert!(undo_redo_result_to_option(roe_undo(h)).is_some());
        assert_eq!(sheet0_name(h), "Renamed1");

        // 再 undo 一次：回到最初的 Data
        assert!(undo_redo_result_to_option(roe_undo(h)).is_some());
        assert_eq!(sheet0_name(h), "Data");

        // 没有更多历史了，再 undo 应报错，文档不变
        assert!(undo_redo_result_to_option(roe_undo(h)).is_none());
        assert_eq!(sheet0_name(h), "Data");

        // redo 一次：回到 Renamed1
        assert!(undo_redo_result_to_option(roe_redo(h)).is_some());
        assert_eq!(sheet0_name(h), "Renamed1");

        // redo 一次：回到 Renamed2
        assert!(undo_redo_result_to_option(roe_redo(h)).is_some());
        assert_eq!(sheet0_name(h), "Renamed2");

        // redo 栈也耗尽了，应报错
        assert!(undo_redo_result_to_option(roe_redo(h)).is_none());
        assert_eq!(sheet0_name(h), "Renamed2");
    }

    #[test]
    fn test_new_commit_clears_redo_stack() {
        let h = make_calc_handle("Data");

        assert_eq!(roe_begin_edit(h), RoeErrorCode::Ok as i32);
        handle::with_entry(h, |entry| match &mut entry.doc {
            RoeDocument::Calc(wb) => wb.sheets[0].name = "Renamed1".into(),
            _ => unreachable!(),
        });
        assert_eq!(roe_commit_edit(h), RoeErrorCode::Ok as i32);

        // undo 回到 Data，此时 redo_stack 里有 Renamed1
        assert!(undo_redo_result_to_option(roe_undo(h)).is_some());
        assert_eq!(sheet0_name(h), "Data");

        // 不 redo，而是直接做一次新编辑并提交：Data -> Branched
        assert_eq!(roe_begin_edit(h), RoeErrorCode::Ok as i32);
        handle::with_entry(h, |entry| match &mut entry.doc {
            RoeDocument::Calc(wb) => wb.sheets[0].name = "Branched".into(),
            _ => unreachable!(),
        });
        assert_eq!(roe_commit_edit(h), RoeErrorCode::Ok as i32);
        assert_eq!(sheet0_name(h), "Branched");

        // 旧的 "Renamed1" 分支应该已经被清空，redo 应报错（标准线性历史，不留分支）
        assert!(undo_redo_result_to_option(roe_redo(h)).is_none());
        assert_eq!(sheet0_name(h), "Branched");
    }

    #[test]
    fn test_undo_redo_invalid_handle() {
        assert!(undo_redo_result_to_option(roe_undo(999_999)).is_none());
        assert!(undo_redo_result_to_option(roe_redo(999_999)).is_none());
    }

    #[test]
    fn test_undo_empty_stack_without_any_edit() {
        let h = make_calc_handle("Data");
        assert!(undo_redo_result_to_option(roe_undo(h)).is_none());
        assert!(undo_redo_result_to_option(roe_redo(h)).is_none());
    }

    #[test]
    fn test_begin_discard_rolls_back() {
        let h = make_calc_handle("Data");

        assert_eq!(roe_begin_edit(h), RoeErrorCode::Ok as i32);

        handle::with_entry(h, |entry| match &mut entry.doc {
            RoeDocument::Calc(wb) => wb.sheets[0].name = "Temp".into(),
            _ => unreachable!(),
        });
        assert_eq!(sheet0_name(h), "Temp");

        assert_eq!(roe_discard_edit(h), RoeErrorCode::Ok as i32);
        assert_eq!(sheet0_name(h), "Data");
        assert!(!has_snapshot(h));
    }
}

#[cfg(test)]
mod object_counter_tests {
    use super::merge_object_counters;
    use roe_model::calc::{Sheet, Workbook};
    use roe_model::style::StyleRegistry;
    use roe_parser::RoeDocument;
    use std::collections::HashMap;

    fn doc(counter: u64) -> RoeDocument {
        let mut sh = Sheet::default();
        sh.next_object_id = counter;
        RoeDocument::Calc(Workbook {
            sheets: vec![sh],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        })
    }

    fn counter(d: &RoeDocument) -> u64 {
        match d { RoeDocument::Calc(w) => w.sheets[0].next_object_id, _ => 0 }
    }

    #[test]
    fn undo_does_not_roll_back_counter() {
        let mut restored = doc(3);
        let replaced = doc(7);
        merge_object_counters(&replaced, &mut restored);
        assert_eq!(counter(&restored), 7);
        let mut restored2 = doc(9);
        merge_object_counters(&doc(7), &mut restored2);
        assert_eq!(counter(&restored2), 9);
    }
}
