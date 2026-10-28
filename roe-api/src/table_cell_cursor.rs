//! v314：表格单元格内的命中测试与光标矩形（只读，不做编辑）。
//!
//! 位置编码：ct=0 为正文（para=body 下标，off=字节偏移）；
//! ct=1 为表格单元格，cid 高 16 位 = 表格在 body 中的序号，
//! 低 16 位 = 行*64+列，para = 单元格内容下标（cell.content 的下标），
//! off = 该段落内字节偏移。
//! 设计借鉴 WordCraft（Apache-2.0）"每行带路径"的思路，用本项目类型重写。

use std::ffi::CString;
use std::os::raw::c_char;

use roe_writer::frame::{FrameContent, LineBox};
use roe_writer::line_shape::{char_offset_to_x, x_to_char_offset};

use crate::error::set_last_error;
use crate::handle;

fn to_c(result: Option<Result<String, String>>, name: &str) -> *mut c_char {
    match result {
        None => {
            set_last_error(format!("{name}: 无效句柄"));
            std::ptr::null_mut()
        }
        Some(Err(msg)) => {
            set_last_error(format!("{name}: {msg}"));
            std::ptr::null_mut()
        }
        Some(Ok(json)) => match CString::new(json) {
            Ok(cs) => cs.into_raw(),
            Err(e) => {
                set_last_error(format!("{name}: {e}"));
                std::ptr::null_mut()
            }
        },
    }
}

/// 命中测试（扩展版）。返回 {"ct":0|1,"cid":N,"para":N,"off":N}。
/// 点在表格行上返回 ct=1；其他情况退回旧的 roe_hit_test 逻辑（ct=0）。
#[no_mangle]
pub extern "C" fn roe_hit_test_ex(handle: u64, x: f64, y: f64) -> *mut c_char {
    let x = x as f32;
    let y = y as f32;
    if !handle::ensure_writer_layout(handle) {
        set_last_error("roe_hit_test_ex: 无效句柄或非 Writer 文档");
        return std::ptr::null_mut();
    }
    let result = handle::with_entry(handle, |entry| hit_ex_in_entry(entry, x, y));
    to_c(result, "roe_hit_test_ex")
}

fn hit_ex_in_entry(entry: &mut handle::DocumentEntry, x: f32, y: f32) -> Result<String, String> {
    // 第一阶段：只读 arena，找出命中的单元格行，拷贝需要的数据。
    let cell_hit = 'blk: {
        let Some(layout) = entry.writer_layout_cache.as_ref() else {
            break 'blk None;
        };
        let arena = &layout.arena;

        let mut target = None;
        'outer: for &pid in arena.page_order.iter() {
            for &fid in arena.pages[pid].frames.iter() {
                let f = &arena.frames[fid];
                if matches!(f.content, FrameContent::TableRow(_)) {
                    let top = f.y_on_page;
                    if y >= top && y < top + f.height() {
                        target = Some(fid);
                        break 'outer;
                    }
                }
            }
        }
        let Some(fid) = target else {
            break 'blk None;
        };
        let f = &arena.frames[fid];
        let FrameContent::TableRow(cells) = &f.content else {
            break 'blk None;
        };
        let margin_left = arena.pages[f.page].margin_left;

        // 按 x 选列：第一个右边界大于 x 的 cell，没有就取最后一个。
        let mut chosen = None;
        for cell in cells.iter() {
            chosen = Some(cell);
            if x < margin_left + cell.x_offset + cell.cell_width {
                break;
            }
        }
        let Some(cell) = chosen else {
            break 'blk None;
        };

        let tagged: Vec<&LineBox> = cell.lines.iter().filter(|l| l.cell_ref.is_some()).collect();
        if tagged.is_empty() {
            break 'blk None;
        }
        // 按 y 选行：第一个底边大于 y 的行，没有就取最后一行。
        let mut idx = tagged.len() - 1;
        for (i, l) in tagged.iter().enumerate() {
            if y < f.y_on_page + l.y_offset + l.line_height {
                idx = i;
                break;
            }
        }
        let line = tagged[idx];
        let cr = line.cell_ref.unwrap();
        let mut before = 0usize;
        for l in tagged[..idx].iter() {
            if l.cell_ref == Some(cr) {
                before += l.text.len();
            }
        }
        let abs_x = margin_left + line.text_x_offset;
        Some((cr, line.text.clone(), line.runs.clone(), abs_x, before))
    };

    match cell_hit {
        None => {
            // 不在表格上：沿用旧逻辑，换一下 JSON 键名。
            let s = crate::hit_test::hit_test_in_entry(entry, x, y)?;
            Ok(s.replace("{\"para_index\":", "{\"ct\":0,\"cid\":0,\"para\":")
                .replace(",\"char_offset\":", ",\"off\":"))
        }
        Some((cr, text, runs, abs_x, before)) => {
            let registry = entry
                .writer_font_registry
                .as_mut()
                .ok_or_else(|| "writer_font_registry 未建立（内部错误）".to_string())?;
            let fs = registry.default_font_size;
            let off_in_line = if text.is_empty() {
                0
            } else {
                x_to_char_offset(registry, &text, &runs, fs, false, false, (x - abs_x).max(0.0))
            };
            let cid: u32 = (cr.0.min(0xFFFF) << 16) | (cr.1 * 64 + cr.2).min(0xFFFF);
            Ok(format!(
                "{{\"ct\":1,\"cid\":{},\"para\":{},\"off\":{}}}",
                cid,
                cr.3,
                before + off_in_line
            ))
        }
    }
}

/// 光标矩形（扩展版）。ct=0 走旧逻辑；ct=1 查表格单元格内的行。
/// 返回 {"page_idx":N,"x":..,"y":..,"w":..,"h":..}。
#[no_mangle]
pub extern "C" fn roe_query_layout_rect_ex(
    handle: u64,
    ct: u32,
    cid: u32,
    para: u32,
    off: u32,
) -> *mut c_char {
    if !handle::ensure_writer_layout(handle) {
        set_last_error("roe_query_layout_rect_ex: 无效句柄或非 Writer 文档");
        return std::ptr::null_mut();
    }
    let result = handle::with_entry(handle, |entry| {
        if ct == 0 {
            crate::layout_query::query_rect_in_entry(entry, para as usize, off as usize)
        } else if ct == 1 {
            rect_cell_in_entry(entry, cid, para, off as usize)
        } else {
            Err(format!("不支持的 container_type={ct}"))
        }
    });
    to_c(result, "roe_query_layout_rect_ex")
}

fn rect_cell_in_entry(
    entry: &mut handle::DocumentEntry,
    cid: u32,
    para: u32,
    off: usize,
) -> Result<String, String> {
    let t = cid >> 16;
    let rc = cid & 0xFFFF;
    let (r, c) = (rc / 64, rc % 64);
    let want = Some((t, r, c, para));

    // 第一阶段：收集该单元格段落的所有行（按页面顺序），拷贝数据。
    let mut cand = Vec::new();
    {
        let layout = entry
            .writer_layout_cache
            .as_ref()
            .ok_or_else(|| "writer_layout_cache 未建立（内部错误）".to_string())?;
        let arena = &layout.arena;
        for (page_idx, &pid) in arena.page_order.iter().enumerate() {
            for &fid in arena.pages[pid].frames.iter() {
                let f = &arena.frames[fid];
                if f.para_index != t as usize {
                    continue;
                }
                let FrameContent::TableRow(cells) = &f.content else {
                    continue;
                };
                let margin_left = arena.pages[f.page].margin_left;
                for cell in cells.iter() {
                    for l in cell.lines.iter() {
                        if l.cell_ref == want {
                            cand.push((
                                l.text.clone(),
                                l.runs.clone(),
                                margin_left + l.text_x_offset,
                                f.y_on_page + l.y_offset,
                                l.line_height,
                                page_idx,
                            ));
                        }
                    }
                }
            }
        }
    }
    if cand.is_empty() {
        return Err(format!("找不到单元格段落 t={t} r={r} c={c} p={para}"));
    }

    // 定位 off 落在哪一行。
    let last = cand.len() - 1;
    let mut consumed = 0usize;
    let mut pick = last;
    let mut off_in_line = 0usize;
    for (i, item) in cand.iter().enumerate() {
        let len = item.0.len();
        if off < consumed + len || i == last {
            pick = i;
            off_in_line = off.saturating_sub(consumed).min(len);
            break;
        }
        consumed += len;
    }
    let (text, runs, abs_x, y, h, page_idx) = &cand[pick];

    let registry = entry
        .writer_font_registry
        .as_mut()
        .ok_or_else(|| "writer_font_registry 未建立（内部错误）".to_string())?;
    let fs = registry.default_font_size;
    let x_in_line = if text.is_empty() {
        0.0
    } else {
        char_offset_to_x(registry, text, runs, fs, false, false, off_in_line)
    };
    let w = (fs * 0.08).max(1.0);
    Ok(format!(
        "{{\"page_idx\":{},\"x\":{:.2},\"y\":{:.2},\"w\":{:.2},\"h\":{:.2}}}",
        page_idx,
        x_in_line + abs_x,
        y,
        w,
        h
    ))
}

// ───────── v316c：单元格内上下键 ─────────

fn nav_line_in_entry(
    entry: &mut handle::DocumentEntry,
    cid: u32,
    para: u32,
    off: usize,
    dir: u32,
    ideal_x: f32,
) -> Result<String, String> {
    let t = cid >> 16;
    let rc = cid & 0xFFFF;
    let (r, c) = (rc / 64, rc % 64);

    // 收集某个单元格的全部行（按页面顺序）：(段落号, 文本, runs, 绝对 x)
    let collect = |e: &handle::DocumentEntry, cr: u32, cc: u32| {
        let mut v = Vec::new();
        if let Some(layout) = e.writer_layout_cache.as_ref() {
            let arena = &layout.arena;
            for &pid in arena.page_order.iter() {
                for &fid in arena.pages[pid].frames.iter() {
                    let f = &arena.frames[fid];
                    if f.para_index != t as usize {
                        continue;
                    }
                    let FrameContent::TableRow(cells) = &f.content else {
                        continue;
                    };
                    let ml = arena.pages[f.page].margin_left;
                    for cell in cells.iter() {
                        for l in cell.lines.iter() {
                            if let Some((a, b, c2, p)) = l.cell_ref {
                                if a == t && b == cr && c2 == cc {
                                    v.push((p, l.text.clone(), l.runs.clone(), ml + l.text_x_offset));
                                }
                            }
                        }
                    }
                }
            }
        }
        v
    };

    let lines = collect(&*entry, r, c);
    if lines.is_empty() {
        return Err(format!("找不到单元格 t={t} r={r} c={c} 的行"));
    }

    // 当前行：第一个满足 off 落在其中的行；否则取该段落最后一行。
    let mut cur: Option<usize> = None;
    let mut last_of_para: Option<usize> = None;
    let mut consumed = 0usize;
    let mut prev_p = u32::MAX;
    for (i, l) in lines.iter().enumerate() {
        if l.0 != prev_p {
            consumed = 0;
            prev_p = l.0;
        }
        if l.0 == para {
            last_of_para = Some(i);
            if off < consumed + l.1.len() {
                cur = Some(i);
                break;
            }
        }
        consumed += l.1.len();
    }
    let ci = cur.or(last_of_para).ok_or_else(|| "找不到当前行".to_string())?;

    // 目标行：本单元格内上/下一行，否则相邻行同列单元格的末行/首行。
    let stay = format!("{{\"ct\":1,\"cid\":{},\"para\":{},\"off\":{}}}", cid, para, off);
    let (tr, tc, tlines, ti) = if dir == 0 {
        if ci > 0 {
            (r, c, lines, ci - 1)
        } else if r > 0 {
            let tl = collect(&*entry, r - 1, c);
            if tl.is_empty() {
                return Ok(stay);
            }
            let n = tl.len();
            (r - 1, c, tl, n - 1)
        } else {
            return Ok(stay);
        }
    } else if ci + 1 < lines.len() {
        (r, c, lines, ci + 1)
    } else {
        let tl = collect(&*entry, r + 1, c);
        if tl.is_empty() {
            return Ok(stay);
        }
        (r + 1, c, tl, 0)
    };

    let (tp, text, runs, abs_x) = &tlines[ti];
    let before: usize = tlines[..ti].iter().filter(|l| l.0 == *tp).map(|l| l.1.len()).sum();
    let registry = entry
        .writer_font_registry
        .as_mut()
        .ok_or_else(|| "writer_font_registry 未建立（内部错误）".to_string())?;
    let fs = registry.default_font_size;
    let off_in_line = if text.is_empty() {
        0
    } else {
        x_to_char_offset(registry, text, runs, fs, false, false, (ideal_x - abs_x).max(0.0))
    };
    let ncid: u32 = (t.min(0xFFFF) << 16) | (tr * 64 + tc).min(0xFFFF);
    Ok(format!(
        "{{\"ct\":1,\"cid\":{},\"para\":{},\"off\":{}}}",
        ncid,
        tp,
        before + off_in_line
    ))
}

/// 单元格内上(dir=0)/下(dir=1)移动，按 ideal_x 找目标行。只读，不入撤销栈。
/// 返回 {"ct":1,"cid":..,"para":..,"off":..}；到表格首行/末行时返回原位置。
#[no_mangle]
pub extern "C" fn roe_cell_nav_line(
    handle_val: u64,
    cid: u32,
    para: u32,
    off: u32,
    dir: u32,
    ideal_x: f64,
) -> *mut c_char {
    if !handle::ensure_writer_layout(handle_val) {
        set_last_error("roe_cell_nav_line: 无效句柄或非 Writer 文档");
        return std::ptr::null_mut();
    }
    let r = handle::with_entry(handle_val, |entry| {
        nav_line_in_entry(entry, cid, para, off as usize, dir, ideal_x as f32)
    });
    to_c(r, "roe_cell_nav_line")
}

// ───────── v318a：单元格内选区矩形 ─────────

#[no_mangle]
pub extern "C" fn roe_query_cell_selection_rects(
    handle: u64,
    cid: u32,
    start_para: u32,
    start_off: u32,
    end_para: u32,
    end_off: u32,
) -> *mut c_char {
    if !handle::ensure_writer_layout(handle) {
        set_last_error("roe_query_cell_selection_rects: 无效句柄或非 Writer 文档");
        return std::ptr::null_mut();
    }
    let result = handle::with_entry(handle, |entry| {
        cell_sel_rects_in_entry(entry, cid, (start_para, start_off as usize), (end_para, end_off as usize))
    });
    to_c(result, "roe_query_cell_selection_rects")
}

fn cell_sel_rects_in_entry(
    entry: &mut handle::DocumentEntry,
    cid: u32,
    a: (u32, usize),
    b: (u32, usize),
) -> Result<String, String> {
    let (from, to) = if a <= b { (a, b) } else { (b, a) };
    if from == to {
        return Ok("[]".to_string());
    }
    let t = cid >> 16;
    let rc = cid & 0xFFFF;
    let (r, c) = (rc / 64, rc % 64);

    let mut cand = Vec::new();
    {
        let layout = entry
            .writer_layout_cache
            .as_ref()
            .ok_or_else(|| "writer_layout_cache 未建立（内部错误）".to_string())?;
        let arena = &layout.arena;
        for (page_idx, &pid) in arena.page_order.iter().enumerate() {
            for &fid in arena.pages[pid].frames.iter() {
                let f = &arena.frames[fid];
                if f.para_index != t as usize {
                    continue;
                }
                let FrameContent::TableRow(cells) = &f.content else {
                    continue;
                };
                let margin_left = arena.pages[f.page].margin_left;
                for cell in cells.iter() {
                    for l in cell.lines.iter() {
                        if let Some((lt, lr, lc, lp)) = l.cell_ref {
                            if lt == t && lr == r && lc == c && lp >= from.0 && lp <= to.0 {
                                cand.push((
                                    lp,
                                    l.text.clone(),
                                    l.runs.clone(),
                                    margin_left + l.text_x_offset,
                                    f.y_on_page + l.y_offset,
                                    l.line_height,
                                    page_idx,
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    if cand.is_empty() {
        return Err(format!("找不到单元格 t={t} r={r} c={c} 的选区行"));
    }

    let registry = entry
        .writer_font_registry
        .as_mut()
        .ok_or_else(|| "writer_font_registry 未建立（内部错误）".to_string())?;
    let fs = registry.default_font_size;

    let mut out: Vec<String> = Vec::new();
    let mut cur_para: Option<u32> = None;
    let mut consumed = 0usize;
    for (p, text, runs, abs_x, y, h, page_idx) in cand.iter() {
        if cur_para != Some(*p) {
            cur_para = Some(*p);
            consumed = 0;
        }
        let len = text.len();
        let lo = if *p == from.0 { from.1 } else { 0 };
        let hi = if *p == to.0 { to.1 } else { usize::MAX };
        if len == 0 {
            // 空行：只有选区越过该段末尾时才画一个小矩形
            if *p < to.0 {
                out.push(format!(
                    "{{\"page_idx\":{},\"x\":{:.2},\"y\":{:.2},\"w\":{:.2},\"h\":{:.2}}}",
                    page_idx, abs_x, y, fs * 0.5, h
                ));
            }
            continue;
        }
        let a0 = lo.max(consumed);
        let b0 = hi.min(consumed + len);
        if a0 < b0 {
            let x0 = if a0 == consumed { 0.0 } else { char_offset_to_x(registry, text, runs, fs, false, false, a0 - consumed) };
            let x1 = char_offset_to_x(registry, text, runs, fs, false, false, b0 - consumed);
            let w = (x1 - x0).max(1.0);
            out.push(format!(
                "{{\"page_idx\":{},\"x\":{:.2},\"y\":{:.2},\"w\":{:.2},\"h\":{:.2}}}",
                page_idx, x0 + abs_x, y, w, h
            ));
        }
        consumed += len;
    }
    Ok(format!("[{}]", out.join(",")))
}

// ───────── v318b：跨单元格选区（整格矩形） ─────────

#[no_mangle]
pub extern "C" fn roe_query_cell_range_rects(handle: u64, cid_a: u32, cid_b: u32) -> *mut c_char {
    if !handle::ensure_writer_layout(handle) {
        set_last_error("roe_query_cell_range_rects: 无效句柄或非 Writer 文档");
        return std::ptr::null_mut();
    }
    let result = handle::with_entry(handle, |entry| cell_range_rects_in_entry(entry, cid_a, cid_b));
    to_c(result, "roe_query_cell_range_rects")
}

fn cell_range_rects_in_entry(
    entry: &mut handle::DocumentEntry,
    a: u32,
    b: u32,
) -> Result<String, String> {
    let (ta, ra, ca) = (a >> 16, (a & 0xFFFF) / 64, (a & 0xFFFF) % 64);
    let (tb, rb, cb) = (b >> 16, (b & 0xFFFF) / 64, (b & 0xFFFF) % 64);
    if ta != tb {
        return Err("两个单元格不在同一张表".to_string());
    }
    let (r0, r1) = (ra.min(rb), ra.max(rb));
    let (c0, c1) = (ca.min(cb), ca.max(cb));
    let layout = entry
        .writer_layout_cache
        .as_ref()
        .ok_or_else(|| "writer_layout_cache 未建立（内部错误）".to_string())?;
    let arena = &layout.arena;
    let mut out: Vec<String> = Vec::new();
    for (page_idx, &pid) in arena.page_order.iter().enumerate() {
        for &fid in arena.pages[pid].frames.iter() {
            let f = &arena.frames[fid];
            if f.para_index != ta as usize {
                continue;
            }
            let FrameContent::TableRow(cells) = &f.content else {
                continue;
            };
            let margin_left = arena.pages[f.page].margin_left;
            let h = f.height();
            for cell in cells.iter() {
                let Some((_, r, c, _)) = cell.lines.iter().find_map(|l| l.cell_ref) else {
                    continue;
                };
                if r >= r0 && r <= r1 && c >= c0 && c <= c1 {
                    out.push(format!(
                        "{{\"page_idx\":{},\"x\":{:.2},\"y\":{:.2},\"w\":{:.2},\"h\":{:.2}}}",
                        page_idx,
                        margin_left + cell.x_offset,
                        f.y_on_page,
                        cell.cell_width,
                        h
                    ));
                }
            }
        }
    }
    Ok(format!("[{}]", out.join(",")))
}
