use roe_parser::ooxml::pptx::PptxParser;
use roe_parser::ooxml::pptx_writer::PptxWriter;
use roe_parser::RoeDocument;

fn main() {
    let doc = RoeDocument::new_impress();
    let a = match doc { RoeDocument::Impress(p) => p, _ => panic!("not impress") };
    println!("new: slides={} masters={} layouts={} slide0 layout_idx={:?} master_idx={:?} slide0 shapes={}",
        a.slides.len(), a.master_slides.len(),
        a.master_slides.first().map(|m| m.layouts.len()).unwrap_or(0),
        a.slides[0].layout_idx, a.slides[0].master_idx, a.slides[0].shapes.len());
    let bytes = PptxWriter::write(&a).expect("write");
    println!("written bytes={}", bytes.len());
    let b = PptxParser::parse(&bytes).expect("reparse");
    println!("reread: slides={} masters={} layouts={} slide0 layout_idx={:?} master_idx={:?} slide0 shapes={}",
        b.slides.len(), b.master_slides.len(),
        b.master_slides.first().map(|m| m.layouts.len()).unwrap_or(0),
        b.slides[0].layout_idx, b.slides[0].master_idx, b.slides[0].shapes.len());
    for (i, l) in b.master_slides[0].layouts.iter().enumerate() {
        let n_ph = l.shapes.iter().filter(|s| s.placeholder.is_some()).count();
        println!("layout {} name={:?} kind={:?} ph={}", i, l.name, l.kind, n_ph);
    }
    let m_ph = b.master_slides[0].shapes.iter().filter(|s| s.placeholder.is_some()).count();
    println!("master ph={}", m_ph);
    for s in &b.slides[0].shapes {
        println!("slide0 shape id={} ph={:?}", s.id, s.placeholder.as_ref().map(|p| (p.ph_type.clone(), p.index)));
    }
    std::fs::write("/tmp/default_tpl.pptx", &bytes).unwrap();
    println!("saved /tmp/default_tpl.pptx");
}
