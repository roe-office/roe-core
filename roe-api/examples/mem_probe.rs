//! 内存探针: 打开文件 -> 数格数 -> 克隆一份文档,打印每格字节数与克隆增量。
//! 用法: cargo run --release -p roe-api --example mem_probe -- <xlsx|csv 路径>

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

fn main() {
    // cell_count 自检
    {
        use roe_model::calc::{CellValue, SparseGrid};
        let mut g = SparseGrid::default();
        assert_eq!(g.cell_count(), 0);
        g.set(0, 0, CellValue::Number(1.0));
        g.set(5, 0, CellValue::Number(2.0));
        g.set(0, 3, CellValue::Number(3.0));
        assert_eq!(g.cell_count(), 3);
        println!("cell_count 自检通过");
    }

    let path = std::env::args().nth(1).expect("usage: mem_probe <path>");
    let ext = std::path::Path::new(&path)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let r0 = rss_kb();
    let data = std::fs::read(&path).expect("read");
    println!("文件 {} MB, 起始 RSS {} MB", data.len() / 1_000_000, r0 / 1024);
    let r1 = rss_kb();

    let doc = match ext.as_str() {
        "xlsx" => roe_parser::RoeDocument::open_xlsx_bytes(&data),
        "csv" => roe_parser::RoeDocument::open_csv_bytes(&data, None),
        o => panic!("unsupported ext {o}"),
    }
    .expect("open");
    drop(data);
    let r2 = rss_kb();

    let cells: usize = match &doc {
        roe_parser::RoeDocument::Calc(wb) => wb.sheets.iter().map(|s| s.cells.cell_count()).sum(),
        _ => panic!("not a calc doc"),
    };
    let opened_mb = (r2.saturating_sub(r1)) / 1024;
    println!("格数 {cells}, 打开后 RSS 增量约 {opened_mb} MB");
    if cells > 0 {
        println!("每格约 {} 字节(含读文件残留,仅供参考)", (r2.saturating_sub(r1)) * 1024 / cells as u64);
    }

    let r3 = rss_kb();
    let cloned = doc.clone();
    let r4 = rss_kb();
    println!("克隆一份 RSS 增量约 {} MB", (r4.saturating_sub(r3)) / 1024);
    drop(cloned);
    drop(doc);
}
