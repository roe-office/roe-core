use roe_parser::ooxml::pptx::PptxParser;
use roe_parser::ooxml::pptx_writer::PptxWriter;

fn nonph(v: &[roe_model::writer::Shape]) -> usize { v.len() }

fn main() {
    let dir = "test_samples/pptx";
    let mut files: Vec<_> = std::fs::read_dir(dir).expect("read dir").flatten()
        .map(|e| e.path()).filter(|p| p.extension().map(|x| x == "pptx").unwrap_or(false)).collect();
    files.sort();
    let (mut ok, mut skip, mut wfail, mut rfail) = (0, 0, 0, 0);
    let mut diffs: Vec<String> = Vec::new();
    let _ = nonph;
    for p in &files {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let data = std::fs::read(p).unwrap();
        let a = match PptxParser::parse(&data) { Ok(x) => x, Err(_) => { skip += 1; continue; } };
        let bytes = match PptxWriter::write(&a) { Ok(x) => x, Err(_) => { wfail += 1; diffs.push(format!("{}: WRITE FAIL", name)); continue; } };
        let b = match PptxParser::parse(&bytes) { Ok(x) => x, Err(_) => { rfail += 1; diffs.push(format!("{}: REREAD FAIL", name)); continue; } };
        let mut d: Vec<String> = Vec::new();
        if a.slides.len() != b.slides.len() { d.push(format!("slides {} vs {}", a.slides.len(), b.slides.len())); }
        if a.master_slides.len() != b.master_slides.len() { d.push(format!("masters {} vs {}", a.master_slides.len(), b.master_slides.len())); }
        for (i, (ma, mb)) in a.master_slides.iter().zip(b.master_slides.iter()).enumerate() {
            if ma.layouts.len() != mb.layouts.len() { d.push(format!("m{} layouts {} vs {}", i, ma.layouts.len(), mb.layouts.len())); }
            if ma.background.is_some() != mb.background.is_some() { d.push(format!("m{} bg {} vs {}", i, ma.background.is_some(), mb.background.is_some())); }
            let na = ma.shapes.iter().filter(|s| s.placeholder.is_none()).count();
            let nb = mb.shapes.iter().filter(|s| s.placeholder.is_none()).count();
            if na != nb { d.push(format!("m{} nonph shapes {} vs {}", i, na, nb)); }
            let pa = ma.shapes.iter().filter(|s| s.placeholder.is_some()).count();
            let pb = mb.shapes.iter().filter(|s| s.placeholder.is_some()).count();
            if pa != pb { d.push(format!("m{} ph shapes {} vs {}", i, pa, pb)); }
            for (j, (la, lb)) in ma.layouts.iter().zip(mb.layouts.iter()).enumerate() {
                let xa = la.shapes.iter().filter(|s| s.placeholder.is_none()).count();
                let xb = lb.shapes.iter().filter(|s| s.placeholder.is_none()).count();
                if xa != xb { d.push(format!("m{}l{} nonph shapes {} vs {}", i, j, xa, xb)); }
                let qa = la.shapes.iter().filter(|s| s.placeholder.is_some()).count();
                let qb = lb.shapes.iter().filter(|s| s.placeholder.is_some()).count();
                if qa != qb { d.push(format!("m{}l{} ph shapes {} vs {}", i, j, qa, qb)); }
                if la.show_master_shapes != lb.show_master_shapes { d.push(format!("m{}l{} showMaster", i, j)); }
                if la.kind != lb.kind { d.push(format!("m{}l{} kind", i, j)); }
            }
        }
        for (i, (sa, sb)) in a.slides.iter().zip(b.slides.iter()).enumerate() {
            let la = sa.layout_idx.unwrap_or(0);
            let lb = sb.layout_idx.unwrap_or(0);
            if sa.master_idx.unwrap_or(0) != sb.master_idx.unwrap_or(0) || la != lb { d.push(format!("slide{} master/layout {:?}/{:?} vs {:?}/{:?}", i, sa.master_idx, sa.layout_idx, sb.master_idx, sb.layout_idx)); }
            if sa.background.is_some() != sb.background.is_some() { d.push(format!("slide{} bg {} vs {}", i, sa.background.is_some(), sb.background.is_some())); }
        }
        if d.is_empty() { ok += 1; } else { diffs.push(format!("{}: {}", name, d.iter().take(4).cloned().collect::<Vec<_>>().join("; "))); }
    }
    for x in diffs.iter().take(60) { println!("{}", x); }
    println!("==== roundtrip same={} with_diff={} skipped_parse={} write_fail={} reread_fail={}", ok, diffs.len(), skip, wfail, rfail);
}
