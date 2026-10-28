use roe_parser::ooxml::pptx::PptxParser;

fn main() {
    let dir = "test_samples/pptx";
    let mut files: Vec<_> = std::fs::read_dir(dir).expect("read dir").flatten()
        .map(|e| e.path()).filter(|p| p.extension().map(|x| x == "pptx").unwrap_or(false)).collect();
    files.sort();
    let (mut ok, mut fail, mut bad) = (0, 0, 0);
    let (mut no_theme, mut no_tx) = (0usize, 0usize);
    let verbose = std::env::args().nth(1).is_some();
    for p in &files {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let data = std::fs::read(p).unwrap();
        let pres = match PptxParser::parse(&data) { Ok(x) => x, Err(_) => { fail += 1; continue; } };
        ok += 1;
        for m in &pres.master_slides { if m.theme_xml.is_none() { no_theme += 1; } if m.tx_styles_xml.is_none() { no_tx += 1; } }
        let nl: Vec<usize> = pres.master_slides.iter().map(|m| m.layouts.len()).collect();
        let mut issues: Vec<String> = Vec::new();
        for (i, s) in pres.slides.iter().enumerate() {
            match (s.master_idx, s.layout_idx) {
                (Some(m), Some(l)) => {
                    let n = pres.master_slides.get(m).map(|x| x.layouts.len()).unwrap_or(0);
                    if l >= n { issues.push(format!("slide{} layout_idx {} out of range {}", i, l, n)); }
                }
                (Some(_), None) => issues.push(format!("slide{} has master but no layout_idx", i)),
                _ => {}
            }
        }
        if !issues.is_empty() { bad += 1; }
        if verbose || !issues.is_empty() {
            println!("{} | slides={} masters={} layouts={:?}", name, pres.slides.len(), pres.master_slides.len(), nl);
            for (mi, m) in pres.master_slides.iter().enumerate() {
                println!("   master{} bg={} clrmap={} layouts:", mi, m.background.is_some(), m.color_map.is_some());
                for l in &m.layouts {
                    println!("      {:?} {:?} shapes={} bg={} showMaster={} ovr={}", l.name, l.kind, l.shapes.len(), l.background.is_some(), l.show_master_shapes, l.color_map.is_some());
                }
            }
            let idxs: Vec<String> = pres.slides.iter().map(|s| format!("{:?}", s.layout_idx)).collect();
            println!("   slide layout_idx: {}", idxs.join(","));
            for x in &issues { println!("   !! {}", x); }
        }
    }
    println!("==== parsed ok={} parse_fail={} files_with_issues={} masters_without_theme={} masters_without_txstyles={}", ok, fail, bad, no_theme, no_tx);
}
