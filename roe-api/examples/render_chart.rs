use std::ffi::CString;

fn main() {
    let json = r#"{
        "kind": "Bar",
        "title": "Monthly Sales",
        "width": 600,
        "height": 400,
        "x_labels": ["Jan","Feb","Mar","Apr"],
        "series": [
            {"name":"Product A","data":[120.0,200.0,150.0,180.0]},
            {"name":"Product B","data":[80.0,160.0,90.0,140.0]}
        ]
    }"#;

    let c_json = CString::new(json).unwrap();
    let mut out_len: u32 = 0;

    let ptr = unsafe {
        roe_api::roe_render_chart_to_png(c_json.as_ptr(), &mut out_len)
    };

    if ptr.is_null() {
        eprintln!("render failed");
        return;
    }

    let bytes = unsafe { std::slice::from_raw_parts(ptr, out_len as usize) };
    std::fs::write("/tmp/roe_chart_api.png", bytes).unwrap();
    println!("PNG written: {} bytes", out_len);

    unsafe { roe_api::roe_free_bytes(ptr, out_len); }
}
