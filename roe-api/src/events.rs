//! roe_poll_event
//!
//! 对应 KernelEvent::IntentSuspended（FontMissing / PasswordRequired /
//! FormatConflict）。范围明确小于 LOK 的实时回调，不含光标/选区。
//!
//! v1 这个队列基本是空的（roe-script 还不产出 IntentSuspended），
//! 但接口先就位：handle::with_entry 取 entry.events.pop_front()，
//! 序列化为 JSON 后返回。

use std::ffi::CString;
use std::os::raw::c_char;

use crate::error::{clear_last_error, set_last_error};
use crate::handle;

/// 弹出一个挂起事件，序列化为 JSON。
/// 队列为空或句柄无效时返回 null（句柄无效会写 last_error；
/// 队列为空不算错误，不写 last_error）。
/// 返回值需要 [`roe_free_string`] 释放。
#[no_mangle]
pub extern "C" fn roe_poll_event(handle: u64) -> *mut c_char {
    let popped = handle::with_entry(handle, |entry| entry.events.pop_front());

    match popped {
        Some(Some(event)) => match serde_json::to_string(&event) {
            Ok(json) => match CString::new(json) {
                Ok(cs) => {
                    clear_last_error();
                    cs.into_raw()
                }
                Err(_) => {
                    set_last_error("roe_poll_event: event json contains NUL byte");
                    std::ptr::null_mut()
                }
            },
            Err(e) => {
                set_last_error(format!("roe_poll_event: serialize error: {e}"));
                std::ptr::null_mut()
            }
        },
        Some(None) => {
            clear_last_error();
            std::ptr::null_mut()
        }
        None => {
            set_last_error(format!("roe_poll_event: invalid handle {handle}"));
            std::ptr::null_mut()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::WriterOptions;
    use crate::handle::DocumentEntry;
    use roe_core::suspension::{KernelEvent, ResumeToken, SuspendReason};
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use roe_model::style::StyleRegistry;
    use roe_parser::RoeDocument;
    use std::collections::{HashMap, VecDeque};
    use std::ffi::CStr;

    fn make_handle() -> u64 {
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

    #[test]
    fn test_poll_event_empty() {
        let h = make_handle();
        let ptr = roe_poll_event(h);
        assert!(ptr.is_null());
    }

    #[test]
    fn test_poll_event_invalid_handle() {
        let ptr = roe_poll_event(999_999);
        assert!(ptr.is_null());
    }

    #[test]
    fn test_poll_event_intent_suspended() {
        let h = make_handle();
        handle::with_entry(h, |entry| {
            entry.events.push_back(KernelEvent::IntentSuspended {
                reason: SuspendReason::FontMissing {
                    font_name: "NotoSansCJK".into(),
                    fallbacks: vec!["DejaVu Sans".into()],
                },
                context: serde_json::json!({"paragraph": 3}),
                resume_token: ResumeToken(42),
            });
        });

        let ptr = roe_poll_event(h);
        assert!(!ptr.is_null());
        let s = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_string();
        crate::roe_free_string(ptr);

        assert!(s.contains("IntentSuspended"));
        assert!(s.contains("FontMissing"));
        assert!(s.contains("NotoSansCJK"));
        assert!(s.contains("42"));

        // 队列已消费，再 poll 应为空
        let ptr2 = roe_poll_event(h);
        assert!(ptr2.is_null());
    }
}
