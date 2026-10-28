//! RoeErrorCode + thread_local last-error 存储

use std::cell::RefCell;
use std::ffi::CString;
use std::os::raw::c_char;

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoeErrorCode {
    Ok = 0,
    InvalidHandle = -1,
    ParseError = -2,
    ScriptError = -3,
    RenderError = -4,
    Unsupported = -5,
    NullPointer = -6,
    Utf8Error = -7,
    SnapshotError = -8,
    NoUndoHistory = -9,
    NoRedoHistory = -10,
}

thread_local! {
    static LAST_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// 内部辅助：设置 thread_local 错误信息，供其他模块调用。
pub(crate) fn set_last_error(msg: impl Into<String>) {
    LAST_ERROR.with(|e| *e.borrow_mut() = Some(msg.into()));
}

/// 内部辅助：清空错误信息（一般在每次 FFI 调用开始时调用）。
#[allow(dead_code)]
pub(crate) fn clear_last_error() {
    LAST_ERROR.with(|e| *e.borrow_mut() = None);
}

/// 获取最近一次错误信息。无错误时返回 null。
/// 返回值需要 [`crate::roe_free_string`] 释放。
#[no_mangle]
pub extern "C" fn roe_last_error_message() -> *mut c_char {
    LAST_ERROR.with(|e| match &*e.borrow() {
        Some(msg) => match CString::new(msg.as_str()) {
            Ok(cs) => cs.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        None => std::ptr::null_mut(),
    })
}
