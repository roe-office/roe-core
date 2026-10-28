//! v275: 页面设置（纸张大小、方向、页边距）

use crate::error::{set_last_error, RoeErrorCode};
use crate::handle;
use roe_parser::RoeDocument;
use std::ffi::CString;
use std::os::raw::c_char;

/// v275: 设置页面。width/height/四边距单位 pt。对顶层及所有已有分节统一设置。带一次撤销快照。
#[no_mangle]
pub extern "C" fn roe_set_page_setup(handle: u64, width: f32, height: f32, top: f32, bottom: f32, left: f32, right: f32) -> i32 {
    for v in [width, height, top, bottom, left, right] {
        if !v.is_finite() || v < 0.0 || v > 5000.0 {
            set_last_error("roe_set_page_setup: 数值非法");
            return RoeErrorCode::ScriptError as i32;
        }
    }
    if width < 72.0 || height < 72.0 {
        set_last_error("roe_set_page_setup: 纸张尺寸过小");
        return RoeErrorCode::ScriptError as i32;
    }
    if left + right >= width - 36.0 || top + bottom >= height - 36.0 {
        set_last_error("roe_set_page_setup: 页边距过大，正文区域不足");
        return RoeErrorCode::ScriptError as i32;
    }
    let result = handle::with_entry(handle, |entry| -> Result<(), String> {
        if !matches!(&entry.doc, RoeDocument::Writer(_)) { return Err("非 Writer 文档".to_string()); }
        let prev = entry.doc.clone();
        let RoeDocument::Writer(doc) = &mut entry.doc else { unreachable!() };
        doc.page_size = (width, height);
        doc.margin_top = top;
        doc.margin_bottom = bottom;
        doc.margin_left = left;
        doc.margin_right = right;
        for s in doc.sections.iter_mut() {
            s.page_size = Some((width, height));
            s.margin_top = Some(top);
            s.margin_bottom = Some(bottom);
            s.margin_left = Some(left);
            s.margin_right = Some(right);
        }
        entry.push_undo_snapshot(prev, handle::EditCursorContext::at(0, 0));
        Ok(())
    });
    let ok = matches!(&result, Some(Ok(())));
    if ok { handle::invalidate_writer_layout_cache(handle); }
    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(m)) => { set_last_error(format!("roe_set_page_setup: {m}")); RoeErrorCode::ScriptError as i32 }
        None => { set_last_error("roe_set_page_setup: 无效句柄"); RoeErrorCode::InvalidHandle as i32 }
    }
}

/// v275: 读取页面设置，返回 JSON {"width","height","top","bottom","left","right"}（pt）。
/// 返回的字符串由调用方按本库既有约定释放。
#[no_mangle]
pub extern "C" fn roe_get_page_setup(handle: u64) -> *mut c_char {
    let result = handle::with_entry(handle, |entry| -> Result<String, String> {
        let RoeDocument::Writer(doc) = &entry.doc else { return Err("非 Writer 文档".to_string()); };
        Ok(format!(
            "{{\"width\":{},\"height\":{},\"top\":{},\"bottom\":{},\"left\":{},\"right\":{}}}",
            doc.page_size.0, doc.page_size.1, doc.margin_top, doc.margin_bottom, doc.margin_left, doc.margin_right
        ))
    });
    match result {
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(e) => { set_last_error(format!("roe_get_page_setup: {e}")); std::ptr::null_mut() }
        },
        Some(Err(m)) => { set_last_error(format!("roe_get_page_setup: {m}")); std::ptr::null_mut() }
        None => { set_last_error("roe_get_page_setup: 无效句柄"); std::ptr::null_mut() }
    }
}
