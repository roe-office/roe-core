use std::ffi::{CStr, CString};

fn cstr_out(ptr: *mut std::os::raw::c_char) -> String {
    if ptr.is_null() {
        return "<null>".to_string();
    }
    let s = unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned();
    roe_api::roe_free_string(ptr);
    s
}

fn main() {
    let csv = "A,B\n1,2\n3,4\n";
    let handle = roe_api::roe_open_document_from_bytes(csv.as_ptr(), csv.len(), 10);
    println!("handle = {}", handle);
    if handle == 0 {
        let err = roe_api::roe_last_error_message();
        println!("open failed: {}", cstr_out(err));
        return;
    }

    let chart_json = r#"{"kind":"Bar","title":"Test Chart","x_labels":["A","B"],"series":[{"name":"S1","data":[10.0,20.0]}]}"#;
    let script = format!(r#"insert_chart(0, 2, 1, `{chart_json}`)"#);
    let script_c = CString::new(script).unwrap();
    let result_ptr = roe_api::roe_execute_script(handle, script_c.as_ptr());
    println!("insert_chart script result = {}", cstr_out(result_ptr));

    let chart_id: u64 = 1;

    let rect_ptr = roe_api::roe_get_chart_rect(handle, 0, chart_id);
    println!("get_chart_rect = {}", cstr_out(rect_ptr));

    let config_ptr = roe_api::roe_get_chart_config(handle, 0, chart_id);
    println!("get_chart_config = {}", cstr_out(config_ptr));

    let resize_ok = roe_api::roe_resize_chart(handle, 0, chart_id, 500.0, 400.0);
    println!("resize_chart = {}", resize_ok);

    let move_ok = roe_api::roe_move_chart(handle, 0, chart_id, 50.0, 60.0);
    println!("move_chart = {}", move_ok);

    let rect_ptr2 = roe_api::roe_get_chart_rect(handle, 0, chart_id);
    println!("get_chart_rect after resize/move = {}", cstr_out(rect_ptr2));

    let style_json = CString::new(r##"{"show_legend":false,"border_color":"#ff0000","border_width":2.5}"##).unwrap();
    let style_ok = roe_api::roe_set_chart_style(handle, 0, chart_id, style_json.as_ptr());
    println!("set_chart_style = {}", style_ok);

    let config_ptr2 = roe_api::roe_get_chart_config(handle, 0, chart_id);
    println!("get_chart_config after style = {}", cstr_out(config_ptr2));

    let style2 = CString::new(r##"{"grouping":"PercentStacked","show_val":true,"gap_width_pct":50,"overlap_pct":100,"hole_size_pct":60}"##).unwrap();
    println!("set_chart_style(新字段) = {}", roe_api::roe_set_chart_style(handle, 0, chart_id, style2.as_ptr()));
    println!("get_chart_config(新字段) = {}", cstr_out(roe_api::roe_get_chart_config(handle, 0, chart_id)));

    let style3 = CString::new(r##"{"grouping":"Standard","show_val":false,"gap_width_pct":null,"overlap_pct":null,"hole_size_pct":null}"##).unwrap();
    println!("set_chart_style(还原) = {}", roe_api::roe_set_chart_style(handle, 0, chart_id, style3.as_ptr()));
    println!("get_chart_config(还原) = {}", cstr_out(roe_api::roe_get_chart_config(handle, 0, chart_id)));

    let bad_rect = roe_api::roe_get_chart_rect(handle, 0, 9999);
    println!("get_chart_rect (bad id, should be null) = {}", cstr_out(bad_rect));

    let delete_ok = roe_api::roe_delete_chart(handle, 0, chart_id);
    println!("delete_chart = {}", delete_ok);

    let delete_again = roe_api::roe_delete_chart(handle, 0, chart_id);
    println!("delete_chart again (should be false) = {}", delete_again);
}
