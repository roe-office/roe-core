//! roe_copy_calc_range / roe_paste_calc_range（及 Writer/Impress 对应函数）
//!
//! 进程级（thread_local）单槽位剪贴板，不依赖、不实现操作系统剪贴板
//! 历史（如 Win+V）——那是前端平台层的事。这里只管理"最后一次复制的
//! 一份结构化数据"，支持跨文档粘贴。三种内容互斥：一次只能持有
//! Writer/Calc/Impress 三者之一，复制新内容会覆盖旧内容。

use crate::error::{set_last_error, RoeErrorCode};
use crate::handle;
use roe_model::calc::{CellFormat, CellValue};
use roe_model::impress::Shape;
use roe_model::writer::BodyElement;
use roe_parser::RoeDocument;
use std::cell::RefCell;

/// 剪贴板内容。三种子系统的内容互斥。
pub enum ClipboardContent {
    Writer(Vec<BodyElement>),
    Calc {
        rows: u32,
        cols: u32,
        /// (相对行, 相对列, 值) —— 只存非空单元格，空单元格不记录，
        /// 粘贴时未被记录的位置保持目标处原值不变？不，保持语义一致：
        /// 粘贴会清空目标区域内未被覆盖的位置为 Empty，下面 paste 实现
        /// 会先把整个目标矩形清空再写入，行为与多数表格软件一致
        /// （粘贴区域内的旧内容会被覆盖，包括空白挤掉原值）。
        cells: Vec<(u32, u32, CellValue)>,
        /// (相对行, 相对列, 格式) —— 只存非默认格式的单元格。
        formats: Vec<(u32, u32, CellFormat)>,
    },
    Impress(Vec<Shape>),
}

thread_local! {
    static CLIPBOARD: RefCell<Option<ClipboardContent>> = const { RefCell::new(None) };
}

/// 查询剪贴板是否有内容（任意类型）。前端可用来决定"粘贴"按钮是否可点。
/// 返回 1 表示有内容，0 表示空。
#[no_mangle]
pub extern "C" fn roe_clipboard_has_content() -> i32 {
    CLIPBOARD.with(|c| if c.borrow().is_some() { 1 } else { 0 })
}

/// 清空剪贴板。
#[no_mangle]
pub extern "C" fn roe_clipboard_clear() {
    CLIPBOARD.with(|c| *c.borrow_mut() = None);
}

// ─── Writer ──────────────────────────────────────────────

/// 复制 body 中一段连续的 BodyElement（按下标闭区间 [start, end]）到剪贴板。
/// start<=end 由调用方保证，不满足时自动交换。
/// 成功返回 ROE_OK(0)；句柄无效或不是 Writer 文档返回 ROE_INVALID_HANDLE(-1)；
/// 下标越界返回 ROE_UNSUPPORTED(-5)。
#[no_mangle]
pub extern "C" fn roe_copy_writer_range(handle: u64, start: u32, end: u32) -> i32 {
    let (start, end) = (start.min(end), end.max(start));

    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Writer(doc) = &entry.doc else {
            return Err("not a Writer document");
        };
        if end as usize >= doc.body.len() {
            return Err("index out of range");
        }
        let fragment = doc.body[start as usize..=end as usize].to_vec();
        Ok(fragment)
    });

    match result {
        Some(Ok(fragment)) => {
            CLIPBOARD.with(|c| *c.borrow_mut() = Some(ClipboardContent::Writer(fragment)));
            RoeErrorCode::Ok as i32
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_copy_writer_range: {msg}"));
            RoeErrorCode::Unsupported as i32
        }
        None => {
            set_last_error(format!("roe_copy_writer_range: invalid handle {handle}"));
            RoeErrorCode::InvalidHandle as i32
        }
    }
}

/// 把剪贴板中的 Writer 内容插入到 body 的指定下标位置（插入，不覆盖；
/// index 等于 body.len() 表示追加到末尾）。
/// 成功返回 ROE_OK(0)；句柄无效或不是 Writer 文档返回 ROE_INVALID_HANDLE(-1)；
/// index 越界（> body.len()）返回 ROE_UNSUPPORTED(-5)；剪贴板为空或不是
/// Writer 内容返回 ROE_SNAPSHOT_ERROR(-8)。
#[no_mangle]
pub extern "C" fn roe_paste_writer_range(handle: u64, index: u32) -> i32 {
    let content = CLIPBOARD.with(|c| c.borrow().as_ref().and_then(|content| match content {
        ClipboardContent::Writer(elements) => Some(elements.clone()),
        _ => None,
    }));

    let Some(elements) = content else {
        set_last_error("roe_paste_writer_range: clipboard is empty or not Writer content".to_string());
        return RoeErrorCode::SnapshotError as i32;
    };

    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Writer(doc) = &mut entry.doc else {
            return Err("not a Writer document");
        };
        if index as usize > doc.body.len() {
            return Err("index out of range");
        }
        let insert_at = index as usize;
        for (offset, element) in elements.into_iter().enumerate() {
            doc.body.insert(insert_at + offset, element);
        }
        Ok(())
    });

    match result {
        Some(Ok(())) => {
            handle::invalidate_writer_layout_cache(handle);
            RoeErrorCode::Ok as i32
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_paste_writer_range: {msg}"));
            RoeErrorCode::Unsupported as i32
        }
        None => {
            set_last_error(format!("roe_paste_writer_range: invalid handle {handle}"));
            RoeErrorCode::InvalidHandle as i32
        }
    }
}

// ─── Calc ────────────────────────────────────────────────

/// 复制一块矩形单元格区域（含格式）到剪贴板。坐标为闭区间，
/// 即 [r1, r2] x [c1, c2]。r1<=r2 且 c1<=c2 由调用方保证，
/// 不满足时本函数会自动交换以保证矩形有效。
/// 操作针对 sheet_idx 指定的 sheet。roe-script 的 get_cell/set_cell/
/// set_formula 此前硬编码 sheets[0]，2026-06-21 已修复为接受 sheet
/// 参数，本函数与之保持一致，均支持任意 sheet_idx。
/// 成功返回 ROE_OK(0)；句柄无效或不是 Calc 文档返回 ROE_INVALID_HANDLE(-1)；
/// sheet_idx 越界返回 ROE_UNSUPPORTED(-5)。
#[no_mangle]
pub extern "C" fn roe_copy_calc_range(
    handle: u64,
    sheet_idx: u32,
    r1: u32,
    c1: u32,
    r2: u32,
    c2: u32,
) -> i32 {
    let (r1, r2) = (r1.min(r2), r1.max(r2));
    let (c1, c2) = (c1.min(c2), c1.max(c2));

    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Calc(wb) = &entry.doc else {
            return Err("not a Calc document");
        };
        let Some(sheet) = wb.sheets.get(sheet_idx as usize) else {
            return Err("sheet_idx out of range");
        };

        let mut cells = Vec::new();
        let mut formats = Vec::new();
        for row in r1..=r2 {
            for col in c1..=c2 {
                let value = sheet.cells.get(row, col);
                if !matches!(value, CellValue::Empty) {
                    cells.push((row - r1, col - c1, value));
                }
                if let Some(fmt) = sheet.cell_formats.get(&(row, col)) {
                    formats.push((row - r1, col - c1, fmt.clone()));
                }
            }
        }

        Ok(ClipboardContent::Calc {
            rows: r2 - r1 + 1,
            cols: c2 - c1 + 1,
            cells,
            formats,
        })
    });

    match result {
        Some(Ok(content)) => {
            CLIPBOARD.with(|c| *c.borrow_mut() = Some(content));
            RoeErrorCode::Ok as i32
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_copy_calc_range: {msg}"));
            RoeErrorCode::Unsupported as i32
        }
        None => {
            set_last_error(format!("roe_copy_calc_range: invalid handle {handle}"));
            RoeErrorCode::InvalidHandle as i32
        }
    }
}

/// 把剪贴板中的 Calc 内容粘贴到指定位置（左上角坐标 dest_row, dest_col）。
/// 粘贴会覆盖目标矩形区域内的现有内容（含清空未被剪贴板数据覆盖的位置，
/// 与主流表格软件行为一致）。
/// 成功返回 ROE_OK(0)；句柄无效或不是 Calc 文档返回 ROE_INVALID_HANDLE(-1)；
/// sheet_idx 越界返回 ROE_UNSUPPORTED(-5)；剪贴板为空或不是 Calc 内容
/// 返回 ROE_SNAPSHOT_ERROR(-8)（复用既有"状态不对"错误码，避免新增
/// 过多相近语义的错误码）。
#[no_mangle]
pub extern "C" fn roe_paste_calc_range(
    handle: u64,
    sheet_idx: u32,
    dest_row: u32,
    dest_col: u32,
) -> i32 {
    let content = CLIPBOARD.with(|c| c.borrow().as_ref().and_then(|content| match content {
        ClipboardContent::Calc { rows, cols, cells, formats } => {
            Some((*rows, *cols, cells.clone(), formats.clone()))
        }
        _ => None,
    }));

    let Some((rows, cols, cells, formats)) = content else {
        set_last_error("roe_paste_calc_range: clipboard is empty or not Calc content".to_string());
        return RoeErrorCode::SnapshotError as i32;
    };

    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Calc(wb) = &mut entry.doc else {
            return Err("not a Calc document");
        };
        let Some(sheet) = wb.sheets.get_mut(sheet_idx as usize) else {
            return Err("sheet_idx out of range");
        };

        // 先清空目标矩形区域，再写入剪贴板数据（覆盖式粘贴）。
        for row in dest_row..dest_row + rows {
            for col in dest_col..dest_col + cols {
                sheet.cells.set(row, col, CellValue::Empty);
                sheet.cell_formats.remove(&(row, col));
            }
        }
        for (rel_row, rel_col, value) in cells {
            sheet.cells.set(dest_row + rel_row, dest_col + rel_col, value);
        }
        for (rel_row, rel_col, fmt) in formats {
            sheet.cell_formats.insert((dest_row + rel_row, dest_col + rel_col), fmt);
        }

        Ok(())
    });

    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(msg)) => {
            set_last_error(format!("roe_paste_calc_range: {msg}"));
            RoeErrorCode::Unsupported as i32
        }
        None => {
            set_last_error(format!("roe_paste_calc_range: invalid handle {handle}"));
            RoeErrorCode::InvalidHandle as i32
        }
    }
}

// ─── Impress ─────────────────────────────────────────────

/// 粘贴时形状自动偏移的距离（pt），避免和原图完全重叠看不清，
/// 是大多数演示软件的标准行为。
const PASTE_OFFSET: f32 = 12.0;

/// 复制指定 slide 中若干个 Shape（按 id 列表）到剪贴板。
/// 找不到的 id 静默跳过，不算错误（调用方传入的 id 可能是前端选区
/// 快照，文档可能在选区之后已发生变化）。
/// 成功返回 ROE_OK(0)；句柄无效或不是 Impress 文档返回
/// ROE_INVALID_HANDLE(-1)；slide_idx 越界返回 ROE_UNSUPPORTED(-5)；
/// id 列表为空或一个都没找到返回 ROE_UNSUPPORTED(-5)。
#[no_mangle]
pub extern "C" fn roe_copy_impress_shapes(
    handle: u64,
    slide_idx: u32,
    shape_ids: *const u32,
    count: usize,
) -> i32 {
    if shape_ids.is_null() || count == 0 {
        set_last_error("roe_copy_impress_shapes: empty shape_ids".to_string());
        return RoeErrorCode::Unsupported as i32;
    }
    // Safety: 调用方保证 shape_ids 指向至少 count 个有效 u32（FFI 调用惯例，
    // 与本文件其余 *const 参数处理方式一致）。
    let ids: Vec<u32> = unsafe { std::slice::from_raw_parts(shape_ids, count) }.to_vec();

    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &entry.doc else {
            return Err("not an Impress document");
        };
        let Some(slide) = pres.slides.get(slide_idx as usize) else {
            return Err("slide_idx out of range");
        };
        let shapes: Vec<Shape> = slide
            .shapes
            .iter()
            .filter(|s| ids.contains(&s.id))
            .cloned()
            .collect();
        if shapes.is_empty() {
            return Err("none of the given shape_ids found on this slide");
        }
        Ok(shapes)
    });

    match result {
        Some(Ok(shapes)) => {
            CLIPBOARD.with(|c| *c.borrow_mut() = Some(ClipboardContent::Impress(shapes)));
            RoeErrorCode::Ok as i32
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_copy_impress_shapes: {msg}"));
            RoeErrorCode::Unsupported as i32
        }
        None => {
            set_last_error(format!("roe_copy_impress_shapes: invalid handle {handle}"));
            RoeErrorCode::InvalidHandle as i32
        }
    }
}

/// 把剪贴板中的 Impress 形状粘贴到指定 slide。每个形状会被分配一个
/// 新 id（沿用该 slide 当前最大 id + 1 递增，避免和已有形状或彼此 id
/// 冲突），并且位置整体偏移 PASTE_OFFSET（pt），避免和原图完全重叠。
/// 成功返回 ROE_OK(0)；句柄无效或不是 Impress 文档返回
/// ROE_INVALID_HANDLE(-1)；slide_idx 越界返回 ROE_UNSUPPORTED(-5)；
/// 剪贴板为空或不是 Impress 内容返回 ROE_SNAPSHOT_ERROR(-8)。
#[no_mangle]
pub extern "C" fn roe_paste_impress_shapes(handle: u64, slide_idx: u32) -> i32 {
    let content = CLIPBOARD.with(|c| c.borrow().as_ref().and_then(|content| match content {
        ClipboardContent::Impress(shapes) => Some(shapes.clone()),
        _ => None,
    }));

    let Some(shapes) = content else {
        set_last_error("roe_paste_impress_shapes: clipboard is empty or not Impress content".to_string());
        return RoeErrorCode::SnapshotError as i32;
    };

    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("not an Impress document");
        };
        let Some(slide) = pres.slides.get_mut(slide_idx as usize) else {
            return Err("slide_idx out of range");
        };

        let mut next_id = slide.shapes.iter().map(|s| s.id).max().unwrap_or(0) + 1;
        for mut shape in shapes {
            shape.id = next_id;
            next_id += 1;
            shape.position.offset.0 += PASTE_OFFSET;
            shape.position.offset.1 += PASTE_OFFSET;
            slide.shapes.push(shape);
        }
        Ok(())
    });

    match result {
        Some(Ok(())) => RoeErrorCode::Ok as i32,
        Some(Err(msg)) => {
            set_last_error(format!("roe_paste_impress_shapes: {msg}"));
            RoeErrorCode::Unsupported as i32
        }
        None => {
            set_last_error(format!("roe_paste_impress_shapes: invalid handle {handle}"));
            RoeErrorCode::InvalidHandle as i32
        }
    }
}

#[cfg(test)]
mod tests_calc {
    use super::*;
    use crate::handle::WriterOptions;
    use crate::handle::DocumentEntry;
    use roe_model::style::StyleRegistry;
    use roe_model::calc::{Sheet, SparseGrid, Workbook};
    use std::collections::{HashMap, VecDeque};

    fn make_calc_handle() -> u64 {
        let wb = Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
                col_widths: vec![],
                row_heights: vec![],
                frozen_rows: 0,
                frozen_cols: 0,
                cell_formats:     HashMap::new(),
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

    fn set_cell(h: u64, row: u32, col: u32, value: CellValue) {
        handle::with_entry(h, |entry| match &mut entry.doc {
            RoeDocument::Calc(wb) => wb.sheets[0].cells.set(row, col, value),
            _ => unreachable!(),
        });
    }

    fn get_cell(h: u64, row: u32, col: u32) -> CellValue {
        handle::with_entry(h, |entry| match &entry.doc {
            RoeDocument::Calc(wb) => wb.sheets[0].cells.get(row, col),
            _ => unreachable!(),
        }).unwrap()
    }

    #[test]
    fn test_copy_paste_basic_roundtrip() {
        roe_clipboard_clear();
        let h = make_calc_handle();
        set_cell(h, 0, 0, CellValue::Number(1.0));
        set_cell(h, 0, 1, CellValue::Text("hi".into()));
        set_cell(h, 1, 0, CellValue::Number(2.0));

        // 复制 (0,0)-(1,1) 这块 2x2 区域
        assert_eq!(roe_copy_calc_range(h, 0, 0, 0, 1, 1), RoeErrorCode::Ok as i32);
        assert_eq!(roe_clipboard_has_content(), 1);

        // 粘贴到 (10, 10)
        assert_eq!(roe_paste_calc_range(h, 0, 10, 10), RoeErrorCode::Ok as i32);

        assert!(matches!(get_cell(h, 10, 10), CellValue::Number(n) if n == 1.0));
        assert!(matches!(get_cell(h, 10, 11), CellValue::Text(s) if s == "hi"));
        assert!(matches!(get_cell(h, 11, 10), CellValue::Number(n) if n == 2.0));
        assert!(matches!(get_cell(h, 11, 11), CellValue::Empty));
    }

    #[test]
    fn test_paste_overwrites_destination() {
        roe_clipboard_clear();
        let h = make_calc_handle();
        set_cell(h, 0, 0, CellValue::Number(99.0));
        // 复制一个空单元格 (5,5)
        assert_eq!(roe_copy_calc_range(h, 0, 5, 5, 5, 5), RoeErrorCode::Ok as i32);
        // 粘贴到 (0,0)：目标原本是 99.0，粘贴空值后应变回 Empty
        assert_eq!(roe_paste_calc_range(h, 0, 0, 0), RoeErrorCode::Ok as i32);
        assert!(matches!(get_cell(h, 0, 0), CellValue::Empty));
    }

    #[test]
    fn test_copy_with_format() {
        roe_clipboard_clear();
        let h = make_calc_handle();
        set_cell(h, 0, 0, CellValue::Number(1.0));
        handle::with_entry(h, |entry| match &mut entry.doc {
            RoeDocument::Calc(wb) => {
                wb.sheets[0].cell_formats.insert((0, 0), CellFormat {
                    bold: Some(true),
                    ..Default::default()
                });
            }
            _ => unreachable!(),
        });

        assert_eq!(roe_copy_calc_range(h, 0, 0, 0, 0, 0), RoeErrorCode::Ok as i32);
        assert_eq!(roe_paste_calc_range(h, 0, 3, 3), RoeErrorCode::Ok as i32);

        let bold = handle::with_entry(h, |entry| match &entry.doc {
            RoeDocument::Calc(wb) => wb.sheets[0].cell_formats.get(&(3, 3)).and_then(|f| f.bold),
            _ => unreachable!(),
        }).unwrap();
        assert_eq!(bold, Some(true));
    }

    #[test]
    fn test_paste_empty_clipboard_fails() {
        roe_clipboard_clear();
        let h = make_calc_handle();
        assert_eq!(roe_paste_calc_range(h, 0, 0, 0), RoeErrorCode::SnapshotError as i32);
    }

    #[test]
    fn test_copy_invalid_handle() {
        assert_eq!(roe_copy_calc_range(999_999, 0, 0, 0, 0, 0), RoeErrorCode::InvalidHandle as i32);
    }

    #[test]
    fn test_paste_across_handles() {
        roe_clipboard_clear();
        let h1 = make_calc_handle();
        let h2 = make_calc_handle();
        set_cell(h1, 0, 0, CellValue::Text("from h1".into()));

        assert_eq!(roe_copy_calc_range(h1, 0, 0, 0, 0, 0), RoeErrorCode::Ok as i32);
        assert_eq!(roe_paste_calc_range(h2, 0, 2, 2), RoeErrorCode::Ok as i32);

        assert!(matches!(get_cell(h2, 2, 2), CellValue::Text(s) if s == "from h1"));
    }
}

#[cfg(test)]
mod tests_writer {
    use super::*;
    use crate::handle::WriterOptions;
    use crate::handle::DocumentEntry;
    use roe_model::writer::{Paragraph, Run, WriterDocument};
    use std::collections::{HashMap, VecDeque};

    fn make_writer_handle(paragraph_texts: &[&str]) -> u64 {
        let body = paragraph_texts
            .iter()
            .map(|t| {
                BodyElement::Paragraph(Paragraph {
                    runs: vec![Run { text: t.to_string(), ..Default::default() }],
                    ..Default::default()
                })
            })
            .collect();
        let doc = WriterDocument { body, ..Default::default() };
        handle::insert(DocumentEntry {
            doc: RoeDocument::Writer(doc),
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

    fn body_texts(h: u64) -> Vec<String> {
        handle::with_entry(h, |entry| match &entry.doc {
            RoeDocument::Writer(doc) => doc.body.iter().map(|el| match el {
                BodyElement::Paragraph(p) => p.runs.first().map(|r| r.text.clone()).unwrap_or_default(),
                _ => "?".to_string(),
            }).collect(),
            _ => unreachable!(),
        }).unwrap()
    }

    #[test]
    fn test_copy_paste_insert_at_index() {
        roe_clipboard_clear();
        let h = make_writer_handle(&["A", "B", "C"]);

        // 复制段落 B（下标1）
        assert_eq!(roe_copy_writer_range(h, 1, 1), RoeErrorCode::Ok as i32);
        // 插入到下标0之前
        assert_eq!(roe_paste_writer_range(h, 0), RoeErrorCode::Ok as i32);

        assert_eq!(body_texts(h), vec!["B", "A", "B", "C"]);
    }

    #[test]
    fn test_copy_paste_append_at_end() {
        roe_clipboard_clear();
        let h = make_writer_handle(&["A", "B"]);
        assert_eq!(roe_copy_writer_range(h, 0, 1), RoeErrorCode::Ok as i32);
        // index == body.len()，追加到末尾
        assert_eq!(roe_paste_writer_range(h, 2), RoeErrorCode::Ok as i32);
        assert_eq!(body_texts(h), vec!["A", "B", "A", "B"]);
    }

    #[test]
    fn test_paste_index_out_of_range() {
        roe_clipboard_clear();
        let h = make_writer_handle(&["A"]);
        assert_eq!(roe_copy_writer_range(h, 0, 0), RoeErrorCode::Ok as i32);
        assert_eq!(roe_paste_writer_range(h, 99), RoeErrorCode::Unsupported as i32);
    }

    #[test]
    fn test_paste_empty_clipboard_fails() {
        roe_clipboard_clear();
        let h = make_writer_handle(&["A"]);
        assert_eq!(roe_paste_writer_range(h, 0), RoeErrorCode::SnapshotError as i32);
    }
}

#[cfg(test)]
mod tests_impress {
    use super::*;
    use crate::handle::WriterOptions;
    use crate::handle::DocumentEntry;
    use roe_model::common::Transform;
    use roe_model::impress::{Presentation, Slide, ShapeGeometry, ShapeStyle, ShapeContent};
    use std::collections::{HashMap, VecDeque};

    fn make_shape(id: u32, x: f32, y: f32) -> Shape {
        Shape {
            id,
            geometry: ShapeGeometry::Rect,
            position: Transform::from_rect(x, y, 10.0, 10.0),
            style: ShapeStyle::default(),
            content: ShapeContent::Empty,
            text_body: Default::default(),
            placeholder: None,
            end_cxn: None,
            st_cxn: None,
        }
    }

    fn make_impress_handle(shapes: Vec<Shape>) -> u64 {
        let pres = Presentation {
            styles: roe_model::style::StyleRegistry::default(),
            slide_size: (960.0, 540.0),
            slides: vec![Slide { layout_idx: None,
                shapes,
                master_idx: None,
                animation_tree: Default::default(),
                transition: None,
                notes: None,
                background: None,
                layout_shapes: vec![],
            }],
            master_slides: vec![],
            theme_colors: Default::default(),
            default_text_style: Default::default(),
        };
        handle::insert(DocumentEntry {
            doc: RoeDocument::Impress(pres),
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

    fn slide0_shapes(h: u64) -> Vec<Shape> {
        handle::with_entry(h, |entry| match &entry.doc {
            RoeDocument::Impress(pres) => pres.slides[0].shapes.clone(),
            _ => unreachable!(),
        }).unwrap()
    }

    #[test]
    fn test_copy_paste_offsets_position_and_assigns_new_id() {
        roe_clipboard_clear();
        let h = make_impress_handle(vec![make_shape(1, 100.0, 100.0)]);

        let ids = [1u32];
        assert_eq!(roe_copy_impress_shapes(h, 0, ids.as_ptr(), 1), RoeErrorCode::Ok as i32);
        assert_eq!(roe_paste_impress_shapes(h, 0), RoeErrorCode::Ok as i32);

        let shapes = slide0_shapes(h);
        assert_eq!(shapes.len(), 2);
        // 新形状 id 不应与原有冲突
        assert_ne!(shapes[1].id, shapes[0].id);
        assert_eq!(shapes[1].id, 2);
        // 位置应偏移
        assert_eq!(shapes[1].position.x(), 100.0 + PASTE_OFFSET);
        assert_eq!(shapes[1].position.y(), 100.0 + PASTE_OFFSET);
        // 原形状不受影响
        assert_eq!(shapes[0].position.x(), 100.0);
    }

    #[test]
    fn test_copy_missing_ids_returns_unsupported() {
        roe_clipboard_clear();
        let h = make_impress_handle(vec![make_shape(1, 0.0, 0.0)]);
        let ids = [999u32];
        assert_eq!(roe_copy_impress_shapes(h, 0, ids.as_ptr(), 1), RoeErrorCode::Unsupported as i32);
    }

    #[test]
    fn test_paste_empty_clipboard_fails() {
        roe_clipboard_clear();
        let h = make_impress_handle(vec![]);
        assert_eq!(roe_paste_impress_shapes(h, 0), RoeErrorCode::SnapshotError as i32);
    }

    #[test]
    fn test_paste_multiple_shapes_get_sequential_ids() {
        roe_clipboard_clear();
        let h = make_impress_handle(vec![make_shape(5, 0.0, 0.0), make_shape(7, 0.0, 0.0)]);
        let ids = [5u32, 7u32];
        assert_eq!(roe_copy_impress_shapes(h, 0, ids.as_ptr(), 2), RoeErrorCode::Ok as i32);
        assert_eq!(roe_paste_impress_shapes(h, 0), RoeErrorCode::Ok as i32);

        let shapes = slide0_shapes(h);
        assert_eq!(shapes.len(), 4);
        // 新分配的两个 id 应该是 8, 9（max(5,7)+1 起递增），互不相同
        let new_ids: Vec<u32> = shapes[2..].iter().map(|s| s.id).collect();
        assert_eq!(new_ids, vec![8, 9]);
    }
}
