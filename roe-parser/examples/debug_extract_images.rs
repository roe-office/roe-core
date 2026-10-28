fn main() {
    let xml = std::fs::read_to_string("/tmp/hidden_shapes_inspect/extracted/xl/drawings/drawing1.xml").unwrap();
    // extract_image_rids_from_drawing 是 private fn，用 include! 不方便，
    // 这里换个思路：直接调用完整 open_xlsx_bytes 走一遍，看 sheet.floating_images。
    println!("xml len: {}", xml.len());
    println!("contains twoCellAnchor: {}", xml.contains("twoCellAnchor"));
    println!("contains <xdr:pic>: {}", xml.contains("<xdr:pic>"));
    println!("contains a:blip with extLst (non-self-closing): {}", xml.contains("<a:blip") && xml.contains("</a:blip>"));
}
