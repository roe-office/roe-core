//! Writer Rhai 扩展(P0): inspect_document / get_table_cell / set_table_cell。
//! 作为 writer_api 的子模块加载(#[path]),可直接使用父模块私有的 with_doc / para_text。
//! 失败语义: 返回以 "err:" 开头的字符串;set_table_cell 成功返回 "ok"。
//! 下标约定: body_idx 即 doc.body 下标(与 paragraph_count/get_paragraph/insert_table/table_* 同一空间)。

use rhai::Engine;
use roe_model::common::{CellContent, StyleRef, TableCell};
use roe_model::writer::{BodyElement, Paragraph, WriterDocument};

use super::{para_text, with_doc};

const TEXT_PREVIEW_CHARS: usize = 40;
const NO_DOC: &str = "err: 没有打开的文档";

fn r2(v: f32) -> f64 {
    ((v as f64) * 100.0).round() / 100.0
}

fn to_idx(v: i64) -> Option<usize> {
    if v < 0 { None } else { Some(v as usize) }
}

fn cell_text(cell: &TableCell) -> String {
    cell.content
        .iter()
        .filter_map(|c| match c {
            CellContent::Paragraph(p) => Some(para_text(p)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn find_cell<'a>(
    doc: &'a mut WriterDocument,
    body_idx: i64,
    row: i64,
    col: i64,
) -> Result<&'a mut TableCell, String> {
    let len = doc.body.len();
    let b = to_idx(body_idx)
        .filter(|b| *b < len)
        .ok_or_else(|| format!("err: body_idx {} 越界(body 共 {} 个元素)", body_idx, len))?;
    let table = match &mut doc.body[b] {
        BodyElement::Table(t) => t,
        _ => return Err(format!("err: body_idx {} 不是表格(用 inspect_document() 查看各元素类型)", body_idx)),
    };
    let rows = table.rows.len();
    let r = to_idx(row)
        .filter(|r| *r < rows)
        .ok_or_else(|| format!("err: row {} 越界(表格共 {} 行)", row, rows))?;
    let cols = table.rows[r].cells.len();
    let c = to_idx(col)
        .filter(|c| *c < cols)
        .ok_or_else(|| format!("err: col {} 越界(第 {} 行共 {} 列)", col, r, cols))?;
    Ok(&mut table.rows[r].cells[c])
}

// ───────── P1 查找替换:find_text / replace_text_ex ─────────
// find_build_pattern 复制自 roe-api/find_replace.rs(改一处请同步另一处)。
const FIND_MAX_MATCHES: usize = 200;
const FIND_TEXT_CHARS: usize = 80;

fn find_build_pattern(p: &str, whole_word: bool, use_regex: bool) -> String {
    let mut s = if use_regex {
        p.to_string()
    } else {
        let mut o = String::new();
        for ch in p.chars() {
            if "\\.+*?()|[]{}^$#&-~".contains(ch) { o.push('\\'); }
            o.push(ch);
        }
        o
    };
    if whole_word { s = format!(r"\b(?:{s})\b"); }
    s
}

// ───────── P1 结构类:insert_page_break / insert_column_break / change_case / delete_range ─────────
// 逻辑复制自 roe-api(writer_clipboard.rs 的 split_runs/convert_case/delete_in,comments.rs 的 insert_column_break_core),
// 改一处请同步另一处。偏移 = 段内全部 run 文本拼接后的 UTF-8 字节偏移(一个汉字 3 字节)。
// 与内核不同:偏移越界/落在字符中间、空选区一律报错;出错时不改文档。

fn st_para(doc: &WriterDocument, i: i64) -> Result<&Paragraph, String> {
    if i < 0 {
        return Err(format!("para {} 越界", i));
    }
    match doc.body.get(i as usize) {
        Some(BodyElement::Paragraph(p)) => Ok(p),
        Some(_) => Err(format!("body_idx {} 不是普通段落(用 inspect_document() 查看各元素类型)", i)),
        None => Err(format!("para {} 越界(body 共 {} 个元素)", i, doc.body.len())),
    }
}

fn st_text_len(runs: &[roe_model::writer::Run]) -> usize {
    runs.iter().map(|r| r.text.len()).sum()
}

fn st_offset(p: &Paragraph, offset: i64) -> Result<usize, String> {
    let total = st_text_len(&p.runs);
    if offset < 0 || offset as usize > total {
        return Err(format!("offset {} 超出段落长度 {}(UTF-8 字节,一个汉字 3 字节)", offset, total));
    }
    Ok(offset as usize)
}

fn st_split(runs: &[roe_model::writer::Run], offset: usize) -> Result<(Vec<roe_model::writer::Run>, Vec<roe_model::writer::Run>), String> {
    let mut left = Vec::new();
    let mut right = Vec::new();
    let mut acc = 0usize;
    for r in runs {
        let len = r.text.len();
        if acc + len <= offset {
            left.push(r.clone());
        } else if acc >= offset {
            right.push(r.clone());
        } else {
            let rel = offset - acc;
            if !r.text.is_char_boundary(rel) {
                return Err(format!("偏移 {} 不在字符边界上(中文一个字 3 字节)", offset));
            }
            let mut l = r.clone();
            let mut rr = r.clone();
            l.text = r.text[..rel].to_string();
            rr.text = r.text[rel..].to_string();
            l.comment_range_end.clear();
            rr.comment_range_start.clear();
            rr.footnote_ref = None;
            rr.endnote_ref = None;
            rr.comment_ref = None;
            left.push(l);
            right.push(rr);
        }
        acc += len;
    }
    Ok((left, right))
}

fn st_convert_case(text: &str, mode: i64) -> String {
    match mode {
        1 => text.to_lowercase(),
        2 => text.to_uppercase(),
        3 => {
            let mut out = String::with_capacity(text.len());
            let mut start = true;
            for ch in text.chars() {
                if ch.is_alphanumeric() || ch == '\u{27}' {
                    if start { out.extend(ch.to_uppercase()); } else { out.extend(ch.to_lowercase()); }
                    start = false;
                } else {
                    out.push(ch);
                    start = true;
                }
            }
            out
        }
        4 => text.chars().map(|c| {
            if c.is_lowercase() { c.to_uppercase().collect::<String>() }
            else if c.is_uppercase() { c.to_lowercase().collect::<String>() }
            else { c.to_string() }
        }).collect(),
        _ => {
            let mut out = String::with_capacity(text.len());
            let mut cap = true;
            for ch in text.chars() {
                if ch.is_alphabetic() {
                    if cap { out.extend(ch.to_uppercase()); } else { out.extend(ch.to_lowercase()); }
                    cap = false;
                } else {
                    out.push(ch);
                    if matches!(ch, '.' | '!' | '?' | '。' | '！' | '？') { cap = true; }
                }
            }
            out
        }
    }
}

fn st_page_break(doc: &mut WriterDocument, para: i64, offset: i64) -> Result<(), String> {
    let target = st_para(doc, para)?.clone();
    let o = st_offset(&target, offset)?;
    let p = para as usize;
    if o == 0 {
        let mut np = target;
        np.page_break_before = true;
        doc.body[p] = BodyElement::Paragraph(np);
    } else {
        let (left, right) = st_split(&target.runs, o)?;
        let mut first = target.clone();
        first.runs = left;
        let mut second = target;
        second.runs = right;
        second.bookmarks.clear();
        second.page_break_before = true;
        doc.body[p] = BodyElement::Paragraph(first);
        doc.body.insert(p + 1, BodyElement::Paragraph(second));
    }
    Ok(())
}

fn st_column_break(doc: &mut WriterDocument, para: i64, offset: i64) -> Result<(), String> {
    let multi = doc.columns.num_cols > 1
        || doc.sections.iter().any(|s| s.columns.as_ref().map(|c| c.num_cols > 1).unwrap_or(false));
    if !multi {
        return Err("当前文档为单栏,分栏符无意义(先设置两栏或三栏)".to_string());
    }
    let target = st_para(doc, para)?.clone();
    let o = st_offset(&target, offset)?;
    let (left, right) = st_split(&target.runs, o)?;
    let p = para as usize;
    let mut first = target.clone();
    first.runs = left;
    first.column_break_after = true;
    let mut second = target;
    second.runs = right;
    second.bookmarks.clear();
    second.page_break_before = false;
    doc.body[p] = BodyElement::Paragraph(first);
    doc.body.insert(p + 1, BodyElement::Paragraph(second));
    Ok(())
}

fn st_change_case(doc: &mut WriterDocument, para: i64, start: i64, end: i64, mode: i64) -> Result<(), String> {
    if !(0..=4).contains(&mode) {
        return Err(format!("mode {} 非法(0句首 1小写 2大写 3每词首字母 4切换)", mode));
    }
    if start < 0 || end <= start {
        return Err(format!("区间 [{}, {}) 无效:需要 0 <= start < end", start, end));
    }
    let target = st_para(doc, para)?.clone();
    let total = st_text_len(&target.runs);
    if end as usize > total {
        return Err(format!("end {} 超出段落长度 {}(UTF-8 字节,一个汉字 3 字节)", end, total));
    }
    let (s, e) = (start as usize, end as usize);
    let (left, right) = st_split(&target.runs, s)?;
    let (mut mid, tail) = st_split(&right, e - s)?;
    for r in mid.iter_mut() {
        if r.formula.is_some() || r.field.is_some() { continue; }
        r.text = st_convert_case(&r.text, mode);
    }
    let mut runs = left;
    runs.extend(mid);
    runs.extend(tail);
    let mut np = target;
    np.runs = runs;
    doc.body[para as usize] = BodyElement::Paragraph(np);
    Ok(())
}

fn st_delete_range(doc: &mut WriterDocument, pa: i64, oa: i64, pb: i64, ob: i64) -> Result<(), String> {
    let (pa, oa, pb, ob) = if (pa, oa) <= (pb, ob) { (pa, oa, pb, ob) } else { (pb, ob, pa, oa) };
    if pa == pb && oa == ob {
        return Err("区间为空(起止位置相同)".to_string());
    }
    if pa < 0 || oa < 0 || ob < 0 {
        return Err("段落号和偏移不能为负".to_string());
    }
    if pb as usize >= doc.body.len() {
        return Err(format!("para {} 越界(body 共 {} 个元素)", pb, doc.body.len()));
    }
    for i in pa..=pb {
        st_para(doc, i).map_err(|_| "选区包含表格/图片等非普通段落,暂不支持(用 inspect_document() 查看)".to_string())?;
    }
    let first = st_para(doc, pa)?;
    let last = st_para(doc, pb)?;
    let oa = st_offset(first, oa)?;
    let ob = st_offset(last, ob as i64)?;
    let mut left = st_split(&first.runs, oa)?.0;
    let right = st_split(&last.runs, ob)?.1;
    left.extend(right);
    let mut np = first.clone();
    np.runs = left;
    let (pa, pb) = (pa as usize, pb as usize);
    doc.body[pa] = BodyElement::Paragraph(np);
    if pb > pa {
        doc.body.drain((pa + 1)..=pb);
    }
    Ok(())
}

fn st_reply(r: Result<(), String>) -> String {
    match r {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("err: {}", e),
    }
}

/// P1 结构类写函数(只读引擎不注册)。
pub fn register_structure(engine: &mut Engine) {
    // insert_page_break(para, offset): offset=0 只给本段置"段前分页";offset>0 在该处拆段,后半段(para+1)置段前分页。
    // 拆段后其后的 body 下标加 1。偏移为 UTF-8 字节。成功 ok,失败 err:。
    engine.register_fn("insert_page_break", |para: i64, offset: i64| -> String {
        with_doc(|doc| st_reply(st_page_break(doc, para, offset))).unwrap_or_else(|| NO_DOC.to_string())
    });
    // insert_column_break(para, offset): 在该处拆段(后半段为 para+1),前半段末尾换栏。仅多栏文档可用。
    engine.register_fn("insert_column_break", |para: i64, offset: i64| -> String {
        with_doc(|doc| st_reply(st_column_break(doc, para, offset))).unwrap_or_else(|| NO_DOC.to_string())
    });
    // change_case(para, start, end, mode): 段内 [start,end) 改大小写,偏移为 UTF-8 字节。
    // mode: 0句首 1小写 2大写 3每词首字母 4切换。公式/域不改。
    engine.register_fn("change_case", |para: i64, start: i64, end: i64, mode: i64| -> String {
        with_doc(|doc| st_reply(st_change_case(doc, para, start, end, mode))).unwrap_or_else(|| NO_DOC.to_string())
    });
    // delete_range(para_a, off_a, para_b, off_b): 删除 [(a),(b)) 区间,可跨段(起止顺序不限),
    // 区间内须全是普通段落。删除后 para_a 与 para_b 合并,其后下标前移。偏移为 UTF-8 字节。
    engine.register_fn("delete_range", |pa: i64, oa: i64, pb: i64, ob: i64| -> String {
        with_doc(|doc| st_reply(st_delete_range(doc, pa, oa, pb, ob))).unwrap_or_else(|| NO_DOC.to_string())
    });
}

// ───────── P1 格式类:set_paragraph_shading / set_paragraph_list ─────────
// set_paragraph_list 逻辑复制自 roe-api/char_format.rs(roe_list_style / roe_set_paragraph_list),改一处请同步另一处。
// 与结构类一致:先校验再修改,出错不改文档;成功 ok,失败 err:。

fn fm_para_mut(doc: &mut WriterDocument, i: i64) -> Result<&mut Paragraph, String> {
    if i < 0 {
        return Err(format!("para {} 越界", i));
    }
    let len = doc.body.len();
    match doc.body.get_mut(i as usize) {
        Some(BodyElement::Paragraph(p)) => Ok(p),
        Some(_) => Err(format!("body_idx {} 不是普通段落(用 inspect_document() 查看各元素类型)", i)),
        None => Err(format!("para {} 越界(body 共 {} 个元素)", i, len)),
    }
}

fn fm_parse_hex(s: &str) -> Option<[u8; 4]> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some([((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8, 255])
}

fn fm_shading(doc: &mut WriterDocument, para: i64, color: &str) -> Result<(), String> {
    let v = if color.eq_ignore_ascii_case("none") {
        None
    } else {
        match fm_parse_hex(color.trim()) {
            Some(c) => Some(c),
            None => return Err(format!("颜色 {:?} 无效,应为 \"#rrggbb\" 或 \"none\"", color)),
        }
    };
    let p = fm_para_mut(doc, para)?;
    p.shading = v;
    Ok(())
}

fn fm_list_style(kind: i64, variant: i64) -> (String, roe_model::writer::ListStyle) {
    use roe_model::writer::{BulletListLevel, ListIndent, ListLevelStyle, ListStyle, NumFormat, NumberListLevel};
    let name = if kind == 1 { format!("ROE_Bullet_{variant}") } else { format!("ROE_Number_{variant}") };
    let mut levels = Vec::new();
    for lv in 0..9u32 {
        let indent = ListIndent { margin_left: 36.0 * (lv as f32 + 1.0), text_indent: -18.0, min_label_width: 18.0 };
        if kind == 1 {
            let ch = match variant { 1 => "●", 2 => "○", 3 => "■", 4 => "–", _ => "✓" };
            levels.push(ListLevelStyle::Bullet(BulletListLevel {
                bullet_char: ch.to_string(), prefix: String::new(), suffix: String::new(), indent,
            }));
        } else {
            let (format, suffix) = match variant {
                1 => (NumFormat::Arabic, "."),
                2 => (NumFormat::Arabic, ")"),
                3 => (NumFormat::LowerAlpha, "."),
                4 => (NumFormat::UpperAlpha, "."),
                5 => (NumFormat::LowerRoman, "."),
                _ => (NumFormat::UpperRoman, "."),
            };
            levels.push(ListLevelStyle::Number(NumberListLevel {
                format, prefix: String::new(), suffix: suffix.to_string(),
                start_value: 1, display_levels: 1, indent,
            }));
        }
    }
    (name.clone(), ListStyle { name, levels })
}

fn fm_list(doc: &mut WriterDocument, para: i64, kind: i64, variant: i64) -> Result<(), String> {
    if !(0..=2).contains(&kind) {
        return Err(format!("kind {} 非法(0取消 1项目符号 2编号)", kind));
    }
    if kind > 0 && !(1..=6).contains(&variant) {
        return Err(format!("variant {} 非法(1..=6)", variant));
    }
    let (existing, prev_info) = {
        let existing = fm_para_mut(doc, para)?.list_info.clone();
        let pi = para as usize;
        let prev_info = if pi > 0 {
            match doc.body.get(pi - 1) {
                Some(BodyElement::Paragraph(p)) => p.list_info.clone(),
                _ => None,
            }
        } else { None };
        (existing, prev_info)
    };
    let strip_imported = existing.as_ref().map_or(false, |li| !li.style_name.starts_with("ROE_"));
    if kind == 0 && existing.is_none() {
        return Ok(());
    }
    let new_info = if kind == 0 {
        None
    } else {
        let (name, ls) = fm_list_style(kind, variant);
        doc.styles.list_styles.entry(name.clone()).or_insert(ls);
        let list_id = match &prev_info {
            Some(pi) if pi.style_name == name => pi.list_id.clone(),
            _ => {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
                format!("ROE_L{nanos}")
            }
        };
        Some(roe_model::writer::ListInfo { list_id, level: 0, style_name: name })
    };
    let p = fm_para_mut(doc, para)?;
    p.list_info = new_info;
    if strip_imported {
        let drop_first = p.runs.first().map_or(false, |r| {
            r.text.ends_with(' ') && !r.text.trim_end().contains(' ') && !r.text.trim().is_empty()
        });
        if drop_first { p.runs.remove(0); }
        p.indent_left = None;
        p.indent_first = None;
    }
    Ok(())
}

/// P1 格式类写函数(只读引擎不注册)。
pub fn register_format(engine: &mut Engine) {
    // set_paragraph_shading(para, color): 段落底纹。color 为 "#rrggbb" 或 "none"(清除)。
    engine.register_fn("set_paragraph_shading", |para: i64, color: String| -> String {
        with_doc(|doc| st_reply(fm_shading(doc, para, &color))).unwrap_or_else(|| NO_DOC.to_string())
    });
    // set_paragraph_list(para, kind, variant): kind 0取消 1项目符号 2编号;variant 1..=6(kind=0 时忽略)。
    // 项目符号 1●2○3■4–5✓;编号 1"1." 2"1)" 3"a." 4"A." 5"i." 6"I."。上一段是同一样式时接续同一个列表。
    engine.register_fn("set_paragraph_list", |para: i64, kind: i64, variant: i64| -> String {
        with_doc(|doc| st_reply(fm_list(doc, para, kind, variant))).unwrap_or_else(|| NO_DOC.to_string())
    });
}

// ───────── P1 批注:insert_comment / reply_comment / get_comment_threads ─────────
// cmt_now_iso 复制自 roe-api/comments.rs 的 cm_now_iso;cmt_insert 复制自 insert_comment_core;
// cmt_reply 复制自 reply_comment_core;cmt_format_date 复制自 format_comment_date。改一处请同步另一处。
// 与内核不同:先校验全部偏移再一次性写回(内核先改首段再处理尾段,尾段非法时首段已被改);空内容报错;出错不改文档。
// 偏移为段内 UTF-8 字节(一个汉字 3 字节)。

fn cmt_now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 { y += 1; }
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, sod / 3600, (sod % 3600) / 60, sod % 60)
}

fn cmt_format_date(date: &str) -> String {
    let ok = date.len() == 20 && date.ends_with('Z') && date.as_bytes().get(10) == Some(&b'T');
    if ok { format!("{} {}", &date[0..10], &date[11..16]) } else { date.to_string() }
}

fn cmt_content(text: &str) -> Vec<BodyElement> {
    text.split('\n')
        .map(|seg| BodyElement::Paragraph(Paragraph {
            runs: vec![roe_model::writer::Run { text: seg.to_string(), ..Default::default() }],
            ..Default::default()
        }))
        .collect()
}

fn cmt_next_id(doc: &WriterDocument) -> u32 {
    doc.comments.iter().map(|c| c.id + 1).max().unwrap_or(0)
}

fn cmt_insert(doc: &mut WriterDocument, pa: i64, oa: i64, pb: i64, ob: i64, author: &str, text: &str) -> Result<u32, String> {
    use roe_model::writer::{Comment, Run};
    if text.is_empty() {
        return Err("批注内容为空".to_string());
    }
    let ((pa, oa), (pb, ob)) = if (pa, oa) <= (pb, ob) { ((pa, oa), (pb, ob)) } else { ((pb, ob), (pa, oa)) };
    if pa == pb && oa == ob {
        return Err("区间为空(起止位置相同),请指定要批注的文字范围".to_string());
    }
    let first = st_para(doc, pa)?.clone();
    let last = st_para(doc, pb)?.clone();
    let oa_u = st_offset(&first, oa)?;
    let ob_u = st_offset(&last, ob)?;
    let id = cmt_next_id(doc);
    let ref_run = || Run { comment_ref: Some(id), ..Default::default() };
    if pa == pb {
        let (left, right) = st_split(&first.runs, ob_u)?;
        let (a, mut mid) = st_split(&left, oa_u)?;
        if mid.is_empty() {
            return Err("选区内没有内容".to_string());
        }
        mid[0].comment_range_start.push(id);
        let l = mid.len() - 1;
        mid[l].comment_range_end.push(id);
        mid.push(ref_run());
        let mut runs = a;
        runs.extend(mid);
        runs.extend(right);
        fm_para_mut(doc, pa)?.runs = runs;
    } else {
        let (a, mut r) = st_split(&first.runs, oa_u)?;
        if r.is_empty() {
            r.push(Run::default());
        }
        r[0].comment_range_start.push(id);
        let mut runs_a = a;
        runs_a.extend(r);
        let (mut l, r2) = st_split(&last.runs, ob_u)?;
        if l.is_empty() {
            l.push(Run::default());
        }
        let li = l.len() - 1;
        l[li].comment_range_end.push(id);
        l.push(ref_run());
        l.extend(r2);
        fm_para_mut(doc, pa)?.runs = runs_a;
        fm_para_mut(doc, pb)?.runs = l;
    }
    doc.comments.push(Comment {
        id,
        author: author.to_string(),
        date: Some(cmt_now_iso()),
        initials: author.chars().next().map(|ch| ch.to_string()),
        content: cmt_content(text),
        done: false,
        parent_id: None,
    });
    Ok(id)
}

fn cmt_reply(doc: &mut WriterDocument, parent_id: i64, author: &str, text: &str) -> Result<u32, String> {
    use roe_model::writer::{Comment, Run};
    if text.is_empty() {
        return Err("回复内容为空".to_string());
    }
    let pid = u32::try_from(parent_id).map_err(|_| format!("批注 {} 不存在(用 get_comment_threads() 查看)", parent_id))?;
    let parent_done = {
        let Some(parent) = doc.comments.iter().find(|c| c.id == pid) else {
            return Err(format!("批注 {} 不存在(用 get_comment_threads() 查看)", parent_id));
        };
        if parent.parent_id.is_some() {
            return Err("不能回复一条回复(回复只允许一层,请回复根批注)".to_string());
        }
        parent.done
    };
    let has_anchor = doc.body.iter().any(|el| matches!(el, BodyElement::Paragraph(p)
        if p.runs.iter().any(|r| r.comment_ref == Some(pid)
            || r.comment_range_start.contains(&pid)
            || r.comment_range_end.contains(&pid))));
    if !has_anchor {
        return Err("未在正文中找到父批注的锚点".to_string());
    }
    let id = cmt_next_id(doc);
    for el in doc.body.iter_mut() {
        if let BodyElement::Paragraph(p) = el {
            let mut out: Vec<Run> = Vec::with_capacity(p.runs.len() + 1);
            for mut r in std::mem::take(&mut p.runs) {
                if r.comment_range_start.contains(&pid) {
                    r.comment_range_start.push(id);
                }
                if r.comment_range_end.contains(&pid) {
                    r.comment_range_end.push(id);
                }
                let is_ref = r.comment_ref == Some(pid);
                out.push(r);
                if is_ref {
                    out.push(Run { comment_ref: Some(id), ..Default::default() });
                }
            }
            p.runs = out;
        }
    }
    doc.comments.push(Comment {
        id,
        author: author.to_string(),
        date: Some(cmt_now_iso()),
        initials: author.chars().next().map(|ch| ch.to_string()),
        content: cmt_content(text),
        done: parent_done,
        parent_id: Some(pid),
    });
    Ok(id)
}

fn cmt_reply_json(r: Result<u32, String>) -> String {
    match r {
        Ok(id) => format!("{{\"id\":{}}}", id),
        Err(e) => format!("err: {}", e),
    }
}

/// 批注只读函数(get_comment_threads),由 register_read 调用,只读引擎也可用。
pub fn register_comment_read(engine: &mut Engine) {
    wfml_register_read(engine);
    // get_writer_chart_json(index): 读 body[index] 内联图表的 ChartModel JSON(可整体改后喂回 set_writer_chart_json)。统计图和非图表返回 err:。
    engine.register_fn("get_writer_chart_json", |index: i64| -> String {
        with_doc(|doc| wobj_chart_get(doc, index)).unwrap_or_else(|| NO_DOC.to_string())
    });
    // get_object_rotation(index): 读 body[index] 上图片或形状的旋转角度(度,无旋转返回 0)。
    // 下标越界、不是图片或形状返回 err:(内核对无效对象静默返回 0)。逻辑对应 roe_writer_object_rotation。
    engine.register_fn("get_object_rotation", |index: i64| -> String {
        with_doc(|doc| {
            if index < 0 {
                return "err: 下标不能为负".to_string();
            }
            let r = match doc.body.get(index as usize) {
                Some(BodyElement::Shape(sh)) => sh.rotation,
                Some(BodyElement::Image(img)) => img.rotation,
                _ => return format!("err: body[{index}] 不是图片或形状"),
            };
            format!("{}", r.unwrap_or(0.0))
        })
        .unwrap_or_else(|| NO_DOC.to_string())
    });
    // get_comment_threads(): 全部批注讨论串,JSON:{"threads":[{"root":{id,author,date,done,text},"replies":[{id,author,date,text}]}]}。
    // date 为 "YYYY-MM-DD HH:MM"(原始串不是标准 ISO 时原样返回);没有批注返回 {"threads":[]}。
    engine.register_fn("get_comment_threads", || -> String {
        with_doc(|doc| {
            let threads: Vec<serde_json::Value> = doc.comment_threads().iter().map(|t| {
                let replies: Vec<serde_json::Value> = t.replies.iter().map(|r| serde_json::json!({
                    "id": r.id,
                    "author": r.author,
                    "date": cmt_format_date(r.date.as_deref().unwrap_or("")),
                    "text": roe_model::writer::comment_plain_text(r),
                })).collect();
                serde_json::json!({
                    "root": {
                        "id": t.root.id,
                        "author": t.root.author,
                        "date": cmt_format_date(t.root.date.as_deref().unwrap_or("")),
                        "done": t.root.done,
                        "text": roe_model::writer::comment_plain_text(t.root),
                    },
                    "replies": replies,
                })
            }).collect();
            serde_json::json!({ "threads": threads }).to_string()
        })
        .unwrap_or_else(|| NO_DOC.to_string())
    });
}

// ───────── P1 脚注/尾注:insert_footnote ─────────
// note_split 复制自 roe-api/comments.rs 的 cm_split;note_insert 复制自 insert_note_core。改一处请同步另一处。
// 与内核不同:先校验 para/kind/text/偏移(含超过段落总长)再修改,出错不改文档(内核先改尾注编号格式再校验偏移);空内容报错。
// 与内核一致:尾注首次插入且编号格式为阿拉伯数字时,改为小写罗马。偏移为段内 UTF-8 字节。

fn note_split(runs: &[roe_model::writer::Run], offset: usize) -> Result<(Vec<roe_model::writer::Run>, Vec<roe_model::writer::Run>), String> {
    let mut left = Vec::new();
    let mut right = Vec::new();
    let mut acc = 0usize;
    for r in runs {
        let len = r.text.len();
        if acc + len <= offset {
            left.push(r.clone());
        } else if acc >= offset {
            right.push(r.clone());
        } else {
            let rel = offset - acc;
            if !r.text.is_char_boundary(rel) {
                return Err("err: 偏移不在字符边界上".to_string());
            }
            let mut l = r.clone();
            let mut rr = r.clone();
            l.text = r.text[..rel].to_string();
            rr.text = r.text[rel..].to_string();
            l.comment_range_end.clear();
            rr.comment_range_start.clear();
            rr.footnote_ref = None;
            rr.endnote_ref = None;
            rr.comment_ref = None;
            left.push(l);
            right.push(rr);
        }
        acc += len;
    }
    Ok((left, right))
}

fn note_insert(doc: &mut WriterDocument, para: i64, offset: i64, kind: i64, text: &str) -> Result<u32, String> {
    use roe_model::writer::{FieldKind, Note, NoteType, NumFormat, Run, ScriptPosition};
    if kind != 0 && kind != 1 {
        return Err(format!("err: kind 只能是 0(脚注)或 1(尾注),收到 {kind}"));
    }
    if text.trim().is_empty() {
        return Err("err: 脚注内容为空".to_string());
    }
    if para < 0 || offset < 0 {
        return Err("err: 段落号和偏移不能为负".to_string());
    }
    let (para, offset) = (para as usize, offset as usize);
    // ---- 先校验(只读) ----
    let (left0, right0) = match doc.body.get(para) {
        Some(BodyElement::Paragraph(p)) => {
            let total: usize = p.runs.iter().map(|r| r.text.len()).sum();
            if offset > total {
                return Err(format!("err: 偏移 {offset} 超出段落长度 {total}"));
            }
            note_split(&p.runs, offset)?
        }
        _ => return Err(format!("err: 段落 {para} 不存在或不是普通段落")),
    };
    // ---- 校验通过,开始修改 ----
    let is_end = kind == 1;
    if is_end
        && matches!(doc.endnote_num_fmt, NumFormat::Arabic)
        && !doc.endnotes.iter().any(|n| matches!(n.note_type, NoteType::Normal))
    {
        doc.endnote_num_fmt = NumFormat::LowerRoman;
    }
    let id = {
        let notes = if is_end { &doc.endnotes } else { &doc.footnotes };
        notes.iter().map(|n| n.id + 1).max().unwrap_or(1).max(1)
    };
    let field = if is_end { FieldKind::EndnoteRef } else { FieldKind::FootnoteRef };
    let ref_run = Run {
        footnote_ref: if is_end { None } else { Some(id) },
        endnote_ref: if is_end { Some(id) } else { None },
        field: Some(field),
        script: Some(ScriptPosition::Superscript),
        ..Default::default()
    };
    if let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(para) {
        let mut left = left0;
        left.push(ref_run);
        left.extend(right0);
        p.runs = left;
    }
    let mut content: Vec<BodyElement> = Vec::new();
    for (i, seg) in text.split('\n').enumerate() {
        let mut runs: Vec<Run> = Vec::new();
        if i == 0 {
            runs.push(Run { field: Some(field), script: Some(ScriptPosition::Superscript), ..Default::default() });
            runs.push(Run { text: format!(" {seg}"), ..Default::default() });
        } else {
            runs.push(Run { text: seg.to_string(), ..Default::default() });
        }
        content.push(BodyElement::Paragraph(Paragraph { runs, ..Default::default() }));
    }
    let note = Note { id, content, note_type: NoteType::Normal };
    if is_end { doc.endnotes.push(note); } else { doc.footnotes.push(note); }
    Ok(id as u32)
}

// ───────── P1 段落排序:sort_paragraphs ─────────
// sort_text_key / sort_first_number 复制自 roe-api/src/sort_paragraphs.rs;sort_paras 复制自 roe_sort_paragraphs 的排序部分。改一处请同步另一处。
// 与内核不同:负数/越界参数报错;无撤销快照(脚本函数不带撤销);先校验再修改,出错不改文档。
// 区间 start..=end 为 body 下标,须全是普通段落且不含脚注/尾注引用。

fn sort_text_key(s: &str) -> String {
    use pinyin::ToPinyin;
    let mut out = String::new();
    for c in s.trim().chars() {
        if c.is_ascii() {
            out.push(c.to_ascii_lowercase());
        } else if let Some(p) = c.to_pinyin() {
            out.push_str(p.plain());
            out.push(c);
        } else {
            out.push(c);
        }
    }
    out
}

fn sort_first_number(s: &str) -> Option<f64> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let neg = i > 0 && chars[i - 1] == '-';
            let mut t = String::new();
            let mut dot = false;
            while i < chars.len() && (chars[i].is_ascii_digit() || (chars[i] == '.' && !dot && i + 1 < chars.len() && chars[i + 1].is_ascii_digit())) {
                if chars[i] == '.' { dot = true; }
                t.push(chars[i]);
                i += 1;
            }
            return t.parse::<f64>().ok().map(|v| if neg { -v } else { v });
        }
        i += 1;
    }
    None
}

fn sort_paras(doc: &mut WriterDocument, start: i64, end: i64, kind: i64, descending: i64) -> Result<(), String> {
    if kind != 0 && kind != 1 {
        return Err(format!("err: kind 只能是 0(文字)或 1(数字),收到 {kind}"));
    }
    if start < 0 || end < 0 {
        return Err("err: 段落号不能为负".to_string());
    }
    if end <= start {
        return Err("err: 请选择多个段落(end 必须大于 start)".to_string());
    }
    let (s, e) = (start as usize, end as usize);
    if e >= doc.body.len() {
        return Err(format!("err: 范围越界:end={e},文档共 {} 个 body 元素,end 最大为 {}(start/end 都是 body 下标,end 包含)", doc.body.len(), doc.body.len().saturating_sub(1)));
    }
    for i in s..=e {
        match &doc.body[i] {
            BodyElement::Paragraph(p) => {
                if p.runs.iter().any(|r| r.footnote_ref.is_some() || r.endnote_ref.is_some()) {
                    return Err(format!("err: 第 {i} 段含脚注/尾注引用,暂不支持排序"));
                }
            }
            _ => return Err(format!("err: 第 {i} 个元素不是普通段落(表格/图片/分节符等),暂不支持排序")),
        }
    }
    let items: Vec<BodyElement> = doc.body.drain(s..=e).collect();
    let mut keyed: Vec<(String, Option<f64>, BodyElement)> = items
        .into_iter()
        .map(|el| {
            let text = match &el {
                BodyElement::Paragraph(p) => p.runs.iter().map(|r| r.text.as_str()).collect::<String>(),
                _ => String::new(),
            };
            (sort_text_key(&text), sort_first_number(&text), el)
        })
        .collect();
    let desc = descending != 0;
    keyed.sort_by(|a, b| {
        use std::cmp::Ordering;
        let o = if kind == 1 {
            match (a.1, b.1) {
                (None, None) => a.0.cmp(&b.0),
                (None, Some(_)) => Ordering::Less,
                (Some(_), None) => Ordering::Greater,
                (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal).then_with(|| a.0.cmp(&b.0)),
            }
        } else {
            a.0.cmp(&b.0)
        };
        if desc { o.reverse() } else { o }
    });
    for (i, (_, _, el)) in keyed.into_iter().enumerate() {
        doc.body.insert(s + i, el);
    }
    Ok(())
}

// ───────── P1 形状/旋转:insert_writer_shape / set_object_rotation ─────────
// wshape_build 复制自 roe-api/comments.rs 的 ws_build_shape(含 ws_preset);旋转规则复制自 roe_writer_set_object_rotation。改一处请同步另一处。
// 与内核不同:位置由调用方给出(at=插入的 body 下标,x/y=相对正文区的 pt),不依赖布局缓存;未知 geometry 报错(内核静默退成矩形);
// 宽高须为有限正数(内核静默抬到最小值);先校验再修改,无撤销快照。

fn wshape_build(name: &str, x_pt: f32, y_pt: f32, w: f32, h: f32) -> Result<roe_model::writer::Shape, String> {
    use roe_model::common::{ArrowType, Color, FillStyle, PresetPathData, ShapeGeometry, ShapeStyle, StrokeStyle};
    use std::collections::HashMap;
    let (base, want_head, want_tail) = match name {
        "lineWithArrow" => ("line", false, true),
        "lineWithTwoArrows" => ("line", true, true),
        "bentConnector5WithArrow" => ("bentConnector5", false, true),
        "bentConnector5WithTwoArrows" => ("bentConnector5", true, true),
        "curvedConnector3WithArrow" => ("curvedConnector3", false, true),
        "curvedConnector3WithTwoArrows" => ("curvedConnector3", true, true),
        other => (other, false, false),
    };
    let is_line = base == "line" || base.starts_with("straightConnector")
        || base.starts_with("bentConnector") || base.starts_with("curvedConnector");
    let geometry = match base {
        "rect" => ShapeGeometry::Rect,
        "ellipse" => ShapeGeometry::Ellipse,
        "line" => ShapeGeometry::Line,
        other => {
            let formula = roe_model::preset_geometry::find_preset_shape(other)
                .ok_or_else(|| format!("err: 未知形状类型 {name}"))?;
            let empty: HashMap<&str, f64> = HashMap::new();
            let path = roe_model::preset_geometry::eval_preset_shape(formula, w as f64, h as f64, &empty);
            ShapeGeometry::PresetPath(PresetPathData { commands: path, preset_name: other.to_string(), adj_overrides: Vec::new() })
        }
    };
    let stroke_color = Color { r: 0x38, g: 0x5D, b: 0x8A, a: 255 };
    let style = if is_line {
        ShapeStyle {
            fill: Some(FillStyle::None),
            stroke: Some(StrokeStyle { color: stroke_color, width: 1.5 }),
            head_arrow: if want_head { Some(ArrowType::Triangle) } else { None },
            tail_arrow: if want_tail { Some(ArrowType::Triangle) } else { None },
            ..Default::default()
        }
    } else {
        ShapeStyle {
            fill: Some(FillStyle::Solid(Color { r: 0x4F, g: 0x81, b: 0xBD, a: 255 })),
            stroke: Some(StrokeStyle { color: stroke_color, width: 1.0 }),
            ..Default::default()
        }
    };
    Ok(roe_model::writer::Shape {
        geometry,
        style,
        text_content: Vec::new(),
        width_pt: w,
        height_pt: h,
        x_pt,
        y_pt,
        anchor_type_h: Some("column".to_string()),
        anchor_type_v: Some("margin".to_string()),
        align_h: None,
        align_v: None,
        wrap_type: Some("none".to_string()),
        dist_t_pt: 0.0,
        dist_b_pt: 0.0,
        dist_l_pt: 0.0,
        dist_r_pt: 0.0,
        flip_v: None,
        rotation: None,
    })
}

fn wshape_insert(doc: &mut WriterDocument, at: i64, geometry: &str, x: f64, y: f64, w: f64, h: f64) -> Result<usize, String> {
    if !(x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite()) {
        return Err("err: 位置和大小必须是有限数字".to_string());
    }
    if w <= 0.0 || h <= 0.0 {
        return Err("err: 宽高必须大于 0".to_string());
    }
    if at < 0 || at as usize > doc.body.len() {
        return Err(format!("err: 插入位置 {at} 越界(0..={})", doc.body.len()));
    }
    let shape = wshape_build(geometry, x as f32, y as f32, w as f32, h as f32)?;
    let at = at as usize;
    doc.body.insert(at, BodyElement::Shape(shape));
    Ok(at)
}

fn wshape_rotate(doc: &mut WriterDocument, index: i64, degrees: f64) -> Result<(), String> {
    if !degrees.is_finite() {
        return Err("err: 角度必须是有限数字".to_string());
    }
    if index < 0 {
        return Err("err: 下标不能为负".to_string());
    }
    let d = (degrees as f32).rem_euclid(360.0);
    let v = if d < 0.01 || d > 359.99 { None } else { Some(d) };
    match doc.body.get_mut(index as usize) {
        Some(BodyElement::Shape(sh)) => { sh.rotation = v; Ok(()) }
        Some(BodyElement::Image(img)) => { img.rotation = v; Ok(()) }
        _ => Err(format!("err: body[{index}] 不是图片或形状")),
    }
}

// ───────── P1 图表:insert_writer_chart ─────────
// wchart_insert 复制自 roe-api/comments.rs 的 insert_chart_core + insert_image_core 的定位/拆段逻辑(省掉占位图片,直接插图表元素);
// 切 run 复用本文件的 note_split(复制自 cm_split)。改一处请同步另一处。
// 与内核不同:先校验 JSON/宽高/para/偏移再修改(偏移超过段落总长、落在字符中间报错,内核对超长偏移静默放行);
// 返回图表自身的 body 下标(内核返回 para);无撤销快照。

fn wchart_insert(doc: &mut WriterDocument, para: i64, offset: i64, json: &str, w: f64, h: f64, is_stat: bool) -> Result<usize, String> {
    if json.trim().is_empty() {
        return Err("err: 图表 JSON 为空".to_string());
    }
    if !(w.is_finite() && h.is_finite()) || w <= 0.0 || h <= 0.0 {
        return Err("err: 宽高必须是大于 0 的有限数字".to_string());
    }
    if para < 0 || offset < 0 {
        return Err("err: 段落号和偏移不能为负".to_string());
    }
    let (w, h) = (w as f32, h as f32);
    // ---- 先校验(只读) ----
    let elem = if is_stat {
        let model = serde_json::from_str::<roe_model::stat_chart::StatChartModel>(json)
            .map_err(|e| format!("err: 统计图 JSON 解析失败: {e}"))?;
        BodyElement::InlineStatChart { model, width_pt: w, height_pt: h }
    } else {
        let mut model = serde_json::from_str::<roe_model::impress::ChartModel>(json)
            .map_err(|e| format!("err: 图表 JSON 解析失败: {e}"))?;
        model.width_pt = w;
        model.height_pt = h;
        BodyElement::InlineChart { model, width_pt: w, height_pt: h }
    };
    let (para, offset) = (para as usize, offset as usize);
    let split = match doc.body.get(para) {
        Some(BodyElement::Paragraph(p)) => {
            let total: usize = p.runs.iter().map(|r| r.text.len()).sum();
            if offset > total {
                return Err(format!("err: 偏移 {offset} 超出段落长度 {total}"));
            }
            if offset == 0 { None } else { Some(note_split(&p.runs, offset)?) }
        }
        _ => return Err(format!("err: 段落 {para} 不存在或不是普通段落")),
    };
    // ---- 校验通过,开始修改 ----
    let Some((left, right)) = split else {
        doc.body.insert(para, elem);
        return Ok(para);
    };
    let right_empty = right.iter().all(|r| {
        r.text.is_empty() && r.footnote_ref.is_none() && r.endnote_ref.is_none()
            && r.comment_ref.is_none() && r.formula.is_none() && r.comment_range_end.is_empty()
    });
    if right_empty {
        doc.body.insert(para + 1, elem);
        return Ok(para + 1);
    }
    let second = {
        let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(para) else { unreachable!() };
        let mut sp = p.clone();
        sp.runs = right;
        sp.bookmarks.clear();
        sp.page_break_before = false;
        p.runs = left;
        p.column_break_after = false;
        sp
    };
    doc.body.insert(para + 1, elem);
    doc.body.insert(para + 2, BodyElement::Paragraph(second));
    Ok(para + 1)
}

// ───────── P2-A Writer 对象闭环:delete_writer_object / set_writer_shape_style / get|set_writer_chart_json ─────────
// wobj_delete 复制自 roe-api/comments.rs 的 delete_object_core;wobj_style 复制自 set_shape_style_core(含 ws_parse_hex);
// chart json 读写复制自 roe_writer_get_chart_json / roe_writer_set_chart_json。改一处请同步另一处。
// 与内核不同:样式的 fill/stroke 全部解析成功后才修改(内核先改 fill 再解析 stroke);下标为负报错;无撤销快照。

fn wobj_color(s: &str) -> Option<roe_model::common::Color> {
    let t = s.trim().trim_start_matches('#');
    if t.len() != 6 { return None; }
    let v = u32::from_str_radix(t, 16).ok()?;
    Some(roe_model::common::Color { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8, a: 255 })
}

fn wobj_delete(doc: &mut WriterDocument, index: i64) -> Result<(), String> {
    if index < 0 {
        return Err("err: 下标不能为负".to_string());
    }
    let i = index as usize;
    if !matches!(
        doc.body.get(i),
        Some(BodyElement::Image(_)) | Some(BodyElement::Shape(_)) | Some(BodyElement::InlineChart { .. }) | Some(BodyElement::InlineStatChart { .. })
    ) {
        return Err(format!("err: body[{index}] 不是图片、形状或图表"));
    }
    doc.body.remove(i);
    Ok(())
}

fn wobj_style(doc: &mut WriterDocument, index: i64, fill: &str, stroke: &str, width: f64) -> Result<(), String> {
    use roe_model::common::{Color, FillStyle, GradientKind, GradientStop, StrokeStyle};
    if index < 0 {
        return Err("err: 下标不能为负".to_string());
    }
    if !width.is_finite() {
        return Err("err: 边框粗细必须是有限数字".to_string());
    }
    let i = index as usize;
    if !matches!(doc.body.get(i), Some(BodyElement::Shape(_))) {
        return Err(format!("err: body[{index}] 不是形状"));
    }
    let (fill, stroke) = (fill.trim(), stroke.trim());
    // ---- 先全部解析(只读) ----
    let new_fill: Option<FillStyle> = if fill.is_empty() {
        None
    } else if let Some(g) = fill.strip_prefix("grad:") {
        let parts: Vec<&str> = g.split(',').collect();
        if parts.len() < 3 {
            return Err(format!("err: 渐变参数无效: {g}"));
        }
        let c1 = wobj_color(parts[0]).ok_or_else(|| format!("err: 颜色无效: {}", parts[0]))?;
        let c2 = wobj_color(parts[1]).ok_or_else(|| format!("err: 颜色无效: {}", parts[1]))?;
        let ang: f32 = parts[2].trim().parse().unwrap_or(0.0);
        Some(FillStyle::Gradient {
            stops: vec![GradientStop { pos: 0.0, color: c1 }, GradientStop { pos: 1.0, color: c2 }],
            kind: GradientKind::Linear { angle: ang },
            rotate_with_shape: true,
        })
    } else if fill == "none" {
        Some(FillStyle::None)
    } else {
        Some(FillStyle::Solid(wobj_color(fill).ok_or_else(|| format!("err: 颜色无效: {fill}"))?))
    };
    let new_stroke: Option<Option<Color>> = if stroke.is_empty() {
        None
    } else if stroke == "none" {
        Some(None)
    } else {
        Some(Some(wobj_color(stroke).ok_or_else(|| format!("err: 颜色无效: {stroke}"))?))
    };
    // ---- 校验通过,开始修改 ----
    let Some(BodyElement::Shape(sh)) = doc.body.get_mut(i) else { unreachable!() };
    if let Some(f) = new_fill {
        sh.style.fill = Some(f);
    }
    match new_stroke {
        Some(None) => { sh.style.stroke = None; }
        Some(Some(c)) => match &mut sh.style.stroke {
            Some(s) => { s.color = c; }
            None => { sh.style.stroke = Some(StrokeStyle { color: c, width: 1.0 }); }
        },
        None => {}
    }
    if width >= 0.0 {
        let w = width as f32;
        match &mut sh.style.stroke {
            Some(s) => { s.width = w; }
            None => { sh.style.stroke = Some(StrokeStyle { color: Color { r: 0x38, g: 0x5D, b: 0x8A, a: 255 }, width: w }); }
        }
    }
    Ok(())
}

fn wobj_chart_get(doc: &WriterDocument, index: i64) -> String {
    if index < 0 {
        return "err: 下标不能为负".to_string();
    }
    match doc.body.get(index as usize) {
        Some(BodyElement::InlineChart { model, .. }) => {
            serde_json::to_string(model).unwrap_or_else(|e| format!("err: 序列化失败: {e}"))
        }
        Some(BodyElement::InlineStatChart { .. }) => "err: 统计图暂不支持读取数据".to_string(),
        _ => format!("err: body[{index}] 不是图表"),
    }
}

fn wobj_chart_set(doc: &mut WriterDocument, index: i64, json: &str) -> Result<(), String> {
    if index < 0 {
        return Err("err: 下标不能为负".to_string());
    }
    if json.trim().is_empty() {
        return Err("err: 图表 JSON 为空".to_string());
    }
    let mut nm = serde_json::from_str::<roe_model::impress::ChartModel>(json)
        .map_err(|e| format!("err: 图表 JSON 解析失败: {e}"))?;
    match doc.body.get_mut(index as usize) {
        Some(BodyElement::InlineChart { model, width_pt, height_pt }) => {
            nm.id = model.id;
            nm.x_pt = model.x_pt;
            nm.y_pt = model.y_pt;
            nm.width_pt = *width_pt;
            nm.height_pt = *height_pt;
            *model = nm;
            Ok(())
        }
        _ => Err(format!("err: body[{index}] 不是可编辑的图表")),
    }
}

// ───────── P2-B 批注/脚注管理:set_comment_done / edit_comment / delete_comment / delete_note ─────────
// cmt_clean_runs / cmt_clean_table 复制自 roe-api/comments.rs 的 cm_clean_runs / cm_clean_table;cmgr_delete_comment 复制自 delete_comment_core;
// cmgr_delete_note 复制自 delete_note_core;set_comment_done / edit_comment 复制自 roe_set_comment_done / roe_edit_comment。改一处请同步另一处。
// 与内核不同:delete_note 的偏移按段内 UTF-8 字节(内核按字符数累计),且必须恰好落在引用 run 的起点,找不到报错(内核选最近的一个);
// 无撤销快照;出错不改文档。

fn cmt_clean_runs(runs: &mut Vec<roe_model::writer::Run>, ids: &[u32]) -> bool {
    let mut changed = false;
    let mut drop_flags: Vec<bool> = Vec::with_capacity(runs.len());
    for r in runs.iter_mut() {
        let a = r.comment_range_start.len();
        let b = r.comment_range_end.len();
        r.comment_range_start.retain(|x| !ids.contains(x));
        r.comment_range_end.retain(|x| !ids.contains(x));
        if r.comment_range_start.len() != a || r.comment_range_end.len() != b {
            changed = true;
        }
        let mut was_ref = false;
        if let Some(c) = r.comment_ref {
            if ids.contains(&c) {
                r.comment_ref = None;
                was_ref = true;
                changed = true;
            }
        }
        drop_flags.push(
            was_ref
                && r.text.is_empty()
                && r.comment_range_start.is_empty()
                && r.comment_range_end.is_empty()
                && r.footnote_ref.is_none()
                && r.endnote_ref.is_none()
                && r.field.is_none(),
        );
    }
    let mut it = drop_flags.into_iter();
    runs.retain(|_| !it.next().unwrap_or(false));
    changed
}

fn cmt_clean_table(t: &mut roe_model::common::Table, ids: &[u32]) -> bool {
    let mut ch = false;
    for row in t.rows.iter_mut() {
        for cell in row.cells.iter_mut() {
            for c in cell.content.iter_mut() {
                match c {
                    roe_model::common::CellContent::Paragraph(p) => { ch |= cmt_clean_runs(&mut p.runs, ids); }
                    roe_model::common::CellContent::Table(inner) => { ch |= cmt_clean_table(inner, ids); }
                    _ => {}
                }
            }
        }
    }
    ch
}

fn cmgr_id(id: i64) -> Result<u32, String> {
    u32::try_from(id).map_err(|_| format!("err: 批注 {id} 不存在(用 get_comment_threads() 查看)"))
}

fn cmgr_done(doc: &mut WriterDocument, id: i64, done: bool) -> Result<(), String> {
    let id = cmgr_id(id)?;
    if !doc.comments.iter().any(|c| c.id == id) {
        return Err(format!("err: 批注 {id} 不存在(用 get_comment_threads() 查看)"));
    }
    for c in doc.comments.iter_mut() {
        if c.id == id || c.parent_id == Some(id) {
            c.done = done;
        }
    }
    Ok(())
}

fn cmgr_edit(doc: &mut WriterDocument, id: i64, text: &str) -> Result<(), String> {
    let id = cmgr_id(id)?;
    if text.trim().is_empty() {
        return Err("err: 批注内容不能为空".to_string());
    }
    let Some(c) = doc.comments.iter_mut().find(|c| c.id == id) else {
        return Err(format!("err: 批注 {id} 不存在(用 get_comment_threads() 查看)"));
    };
    c.content = cmt_content(text);
    Ok(())
}

fn cmgr_delete_comment(doc: &mut WriterDocument, id: i64) -> Result<(), String> {
    let id = cmgr_id(id)?;
    if !doc.comments.iter().any(|c| c.id == id) {
        return Err(format!("err: 批注 {id} 不存在(用 get_comment_threads() 查看)"));
    }
    let mut ids = vec![id];
    for c in doc.comments.iter() {
        if c.parent_id == Some(id) { ids.push(c.id); }
    }
    for el in doc.body.iter_mut() {
        match el {
            BodyElement::Paragraph(p) => { cmt_clean_runs(&mut p.runs, &ids); }
            BodyElement::Table(t) => { cmt_clean_table(t, &ids); }
            _ => {}
        }
    }
    doc.comments.retain(|c| !ids.contains(&c.id));
    Ok(())
}

fn cmgr_delete_note(doc: &mut WriterDocument, para: i64, offset: i64, kind: i64) -> Result<(), String> {
    if kind != 0 && kind != 1 {
        return Err(format!("err: kind 只能是 0(脚注)或 1(尾注),收到 {kind}"));
    }
    if para < 0 || offset < 0 {
        return Err("err: 段落号和偏移不能为负".to_string());
    }
    let (para, offset) = (para as usize, offset as usize);
    let is_end = kind == 1;
    let id = {
        let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(para) else {
            return Err(format!("err: 段落 {para} 不存在或不是普通段落"));
        };
        // 先找到目标引用 run(只读)
        let mut pos = 0usize;
        let mut hit: Option<(usize, u32)> = None;
        let mut refs_at: Vec<usize> = Vec::new();
        for (i, r) in p.runs.iter().enumerate() {
            let rid = if is_end { r.endnote_ref } else { r.footnote_ref };
            if let Some(rid) = rid {
                refs_at.push(pos);
                if pos == offset && hit.is_none() { hit = Some((i, rid)); }
            }
            pos += r.text.len();
        }
        let Some((ri, rid)) = hit else {
            let what = if is_end { "尾注" } else { "脚注" };
            return Err(if refs_at.is_empty() {
                format!("err: 第 {para} 段没有{what}引用")
            } else {
                format!("err: 偏移 {offset} 处没有{what}引用(本段{what}引用位于偏移 {:?})", refs_at)
            });
        };
        // 校验通过,开始修改
        let pure = {
            let r = &p.runs[ri];
            r.text.is_empty() && r.comment_range_start.is_empty() && r.comment_range_end.is_empty() && r.comment_ref.is_none()
        };
        if pure {
            p.runs.remove(ri);
        } else {
            let r = &mut p.runs[ri];
            r.footnote_ref = None;
            r.endnote_ref = None;
            r.field = None;
        }
        rid
    };
    if is_end { doc.endnotes.retain(|n| n.id != id); } else { doc.footnotes.retain(|n| n.id != id); }
    Ok(())
}

/// P1 批注写函数(只读引擎不注册)。
pub fn register_comments(engine: &mut Engine) {
    wfml_register_write(engine);
    // insert_comment(pa, oa, pb, ob, author, text): 在 [(pa,oa),(pb,ob)) 上插入批注(起止顺序不限,可跨段),
    // 偏移为段内 UTF-8 字节。text 里的 \n 分段。成功返回 {"id":N},失败 err:。日期取当前 UTC。
    engine.register_fn("insert_comment", |pa: i64, oa: i64, pb: i64, ob: i64, author: String, text: String| -> String {
        with_doc(|doc| cmt_reply_json(cmt_insert(doc, pa, oa, pb, ob, &author, &text))).unwrap_or_else(|| NO_DOC.to_string())
    });
    // reply_comment(parent_id, author, text): 回复根批注(只允许一层)。parent_id 用 get_comment_threads() 查。成功返回 {"id":N}。
    engine.register_fn("reply_comment", |parent_id: i64, author: String, text: String| -> String {
        with_doc(|doc| cmt_reply_json(cmt_reply(doc, parent_id, &author, &text))).unwrap_or_else(|| NO_DOC.to_string())
    });
    // insert_footnote(para, offset, kind, text): 在 para 段的 offset(段内 UTF-8 字节)处插入脚注(kind=0)或尾注(kind=1)。
    // text 里的 \n 分段。成功返回 {"id":N},失败 err:。出错不改文档。尾注首次插入时编号格式由阿拉伯数字改为小写罗马。
    engine.register_fn("insert_footnote", |para: i64, offset: i64, kind: i64, text: String| -> String {
        with_doc(|doc| cmt_reply_json(note_insert(doc, para, offset, kind, &text))).unwrap_or_else(|| NO_DOC.to_string())
    });
    // insert_writer_shape(at, geometry, x, y, width, height): 在 body 下标 at(0..=长度)处插入形状,x/y 为相对正文区的 pt,
    // geometry 如 rect/ellipse/line/star5/lineWithArrow 等预设名。成功返回 {"index":N},失败 err:。出错不改文档。
    engine.register_fn("insert_writer_shape", |at: i64, geometry: String, x: f64, y: f64, width: f64, height: f64| -> String {
        with_doc(|doc| match wshape_insert(doc, at, &geometry, x, y, width, height) {
            Ok(i) => format!("{{\"index\":{i}}}"),
            Err(e) => e,
        })
        .unwrap_or_else(|| NO_DOC.to_string())
    });
    // set_object_rotation(index, degrees): 设置 body[index] 上图片或形状的旋转角度(度,取模 360,0 即不旋转)。成功 ok,失败 err:。
    engine.register_fn("set_object_rotation", |index: i64, degrees: f64| -> String {
        with_doc(|doc| match wshape_rotate(doc, index, degrees) {
            Ok(()) => "ok".to_string(),
            Err(e) => e,
        })
        .unwrap_or_else(|| NO_DOC.to_string())
    });
    // insert_writer_chart(para, offset, json, width, height, is_stat): 在 para 段的 offset(段内 UTF-8 字节)处插入内联图表。
    // offset=0 插在该段之前;offset>0 时在该处拆段,图表夹在两半之间(后半段为空则不新增空段)。
    // json 为 ChartModel(is_stat=false)或 StatChartModel(is_stat=true)的 JSON;宽高单位 pt。成功返回 {"index":N}(图表的 body 下标),失败 err:。出错不改文档。
    engine.register_fn("insert_writer_chart", |para: i64, offset: i64, json: String, width: f64, height: f64, is_stat: bool| -> String {
        with_doc(|doc| match wchart_insert(doc, para, offset, &json, width, height, is_stat) {
            Ok(i) => format!("{{\"index\":{i}}}"),
            Err(e) => e,
        })
        .unwrap_or_else(|| NO_DOC.to_string())
    });
    // delete_writer_object(index): 删除 body[index] 上的图片、形状或图表。成功 ok,失败 err:。其后下标前移 1。
    engine.register_fn("delete_writer_object", |index: i64| -> String {
        with_doc(|doc| match wobj_delete(doc, index) { Ok(()) => "ok".to_string(), Err(e) => e })
            .unwrap_or_else(|| NO_DOC.to_string())
    });
    // set_writer_shape_style(index, fill, stroke, width): 改形状样式。fill: ""=不改 "none"=无填充 "#rrggbb"=纯色 "grad:#rrggbb,#rrggbb,角度"=线性渐变;
    // stroke: ""=不改 "none"=无边框 "#rrggbb"=边框色;width<0 不改,否则边框粗细 pt。成功 ok,失败 err:(出错不改文档)。
    engine.register_fn("set_writer_shape_style", |index: i64, fill: String, stroke: String, width: f64| -> String {
        with_doc(|doc| match wobj_style(doc, index, &fill, &stroke, width) { Ok(()) => "ok".to_string(), Err(e) => e })
            .unwrap_or_else(|| NO_DOC.to_string())
    });
    // set_writer_chart_json(index, json): 用 ChartModel JSON 替换内联图表内容,保留原 id、位置和尺寸。统计图不支持。成功 ok,失败 err:。
    engine.register_fn("set_writer_chart_json", |index: i64, json: String| -> String {
        with_doc(|doc| match wobj_chart_set(doc, index, &json) { Ok(()) => "ok".to_string(), Err(e) => e })
            .unwrap_or_else(|| NO_DOC.to_string())
    });
    // set_comment_done(id, done): 设置批注完成状态,根批注连带其回复一起设置。id 用 get_comment_threads() 查。成功 ok,失败 err:。
    engine.register_fn("set_comment_done", |id: i64, done: bool| -> String {
        with_doc(|doc| match cmgr_done(doc, id, done) { Ok(()) => "ok".to_string(), Err(e) => e })
            .unwrap_or_else(|| NO_DOC.to_string())
    });
    // edit_comment(id, text): 替换批注正文(\n 分段),空内容报错。成功 ok,失败 err:。
    engine.register_fn("edit_comment", |id: i64, text: String| -> String {
        with_doc(|doc| match cmgr_edit(doc, id, &text) { Ok(()) => "ok".to_string(), Err(e) => e })
            .unwrap_or_else(|| NO_DOC.to_string())
    });
    // delete_comment(id): 删除批注及其全部回复,并清掉正文(含表格)里的起止标记与引用。成功 ok,失败 err:。
    engine.register_fn("delete_comment", |id: i64| -> String {
        with_doc(|doc| match cmgr_delete_comment(doc, id) { Ok(()) => "ok".to_string(), Err(e) => e })
            .unwrap_or_else(|| NO_DOC.to_string())
    });
    // delete_note(para, offset, kind): 删除 para 段中位于 offset(段内 UTF-8 字节)的脚注(kind=0)或尾注(kind=1)引用及其内容。
    // offset 必须正好是引用的位置(即 insert_footnote 当时用的偏移)。成功 ok,失败 err:。
    engine.register_fn("delete_note", |para: i64, offset: i64, kind: i64| -> String {
        with_doc(|doc| match cmgr_delete_note(doc, para, offset, kind) { Ok(()) => "ok".to_string(), Err(e) => e })
            .unwrap_or_else(|| NO_DOC.to_string())
    });
    // sort_paragraphs(start, end, kind, descending): 对 body 下标 start..=end 的段落排序(须全是普通段落,不含脚注/尾注引用)。
    // kind 0=文字(拼音/字母,忽略大小写) 1=数字(取段内第一个数字,无数字的排前面);descending 非 0 为降序。成功 ok,失败 err:。出错不改文档。
    engine.register_fn("sort_paragraphs", |start: i64, end: i64, kind: i64, descending: i64| -> String {
        with_doc(|doc| match sort_paras(doc, start, end, kind, descending) {
            Ok(()) => "ok".to_string(),
            Err(e) => e,
        })
        .unwrap_or_else(|| NO_DOC.to_string())
    });
}

/// 只读函数(inspect_document / get_table_cell),同时供只读引擎使用。
pub fn register_read(engine: &mut Engine) {
    register_comment_read(engine);
    // find_text: 查找全部匹配,返回 {"total","truncated","matches":[{"para","start","end","text"}]}。
    // para 是 body 下标(与 get_paragraph 同一空间),start/end 是段内 UTF-8 字节偏移(一个汉字 3 字节)。
    // 只搜 body 直属段落,不搜表格/文本框,不跨段匹配。最多返回 200 条,超出 truncated=true(total 仍是总数)。
    // use_regex=false 时 pattern 按字面量;whole_word=true 加整词边界。空 pattern、非法正则返回 err:。
    engine.register_fn("find_text", |pattern: String, case_sensitive: bool, whole_word: bool, use_regex: bool| -> String {
        if pattern.is_empty() {
            return "err: 查找内容为空".to_string();
        }
        let re = match roe_writer::find_replace::compile_pattern(&find_build_pattern(&pattern, whole_word, use_regex), case_sensitive) {
            Ok(r) => r,
            Err(e) => return format!("err: {}", e),
        };
        with_doc(|doc| {
            let mut total = 0usize;
            let mut matches: Vec<serde_json::Value> = Vec::new();
            for (i, el) in doc.body.iter().enumerate() {
                if let BodyElement::Paragraph(p) = el {
                    let full = para_text(p);
                    for m in re.find_iter(&full) {
                        if m.end() <= m.start() { continue; }
                        total += 1;
                        if matches.len() < FIND_MAX_MATCHES {
                            matches.push(serde_json::json!({
                                "para": i, "start": m.start(), "end": m.end(),
                                "text": full[m.start()..m.end()].chars().take(FIND_TEXT_CHARS).collect::<String>(),
                            }));
                        }
                    }
                }
            }
            serde_json::json!({ "total": total, "truncated": total > matches.len(), "matches": matches }).to_string()
        })
        .unwrap_or_else(|| NO_DOC.to_string())
    });

    // inspect_document: 一次返回 body 全部元素的结构 JSON。
    engine.register_fn("inspect_document", || -> String {
        with_doc(|doc| {
            let elements: Vec<serde_json::Value> = doc.body.iter().enumerate().map(|(i, el)| {
                match el {
                    BodyElement::Paragraph(p) => {
                        let text = para_text(p);
                        let total = text.chars().count();
                        serde_json::json!({
                            "index": i,
                            "type": "paragraph",
                            "style": p.style.0,
                            "text": text.chars().take(TEXT_PREVIEW_CHARS).collect::<String>(),
                            "text_len": total,
                            "text_truncated": total > TEXT_PREVIEW_CHARS,
                        })
                    }
                    BodyElement::Table(t) => serde_json::json!({
                        "index": i,
                        "type": "table",
                        "rows": t.rows.len(),
                        "cols": t.rows.first().map(|r| r.cells.len()).unwrap_or(0),
                    }),
                    BodyElement::Section(s) => serde_json::json!({
                        "index": i, "type": "section", "name": s.name,
                    }),
                    BodyElement::Image(img) => serde_json::json!({
                        "index": i,
                        "type": "image",
                        "mime": img.image.mime,
                        "width_pt": r2(img.width_pt),
                        "height_pt": r2(img.height_pt),
                    }),
                    BodyElement::Frame(_) => serde_json::json!({ "index": i, "type": "frame" }),
                    BodyElement::Shape(_) => serde_json::json!({ "index": i, "type": "shape" }),
                    BodyElement::InlineChart { model, width_pt, height_pt } => serde_json::json!({
                        "index": i,
                        "type": "chart",
                        "kind": format!("{:?}", model.kind),
                        "title": model.title,
                        "series_count": model.series.len(),
                        "width_pt": r2(*width_pt),
                        "height_pt": r2(*height_pt),
                    }),
                    BodyElement::InlineStatChart { model, width_pt, height_pt } => serde_json::json!({
                        "index": i,
                        "type": "stat_chart",
                        "kind": format!("{:?}", model.kind),
                        "title": model.title,
                        "series_count": model.series.len(),
                        "width_pt": r2(*width_pt),
                        "height_pt": r2(*height_pt),
                    }),
                }
            }).collect();
            serde_json::json!({
                "body_count": doc.body.len(),
                "section_count": doc.sections.len(),
                "elements": elements,
            }).to_string()
        })
        .unwrap_or_else(|| NO_DOC.to_string())
    });

    // get_table_cell: 读单元格文字(多段用 \n 连接)。出错返回 "err:..."。
    engine.register_fn("get_table_cell", |body_idx: i64, row: i64, col: i64| -> String {
        with_doc(|doc| match find_cell(doc, body_idx, row, col) {
            Ok(cell) => {
                if cell.hidden {
                    "err: 该单元格是合并单元格的延续格,请读合并区域的起始格".to_string()
                } else {
                    cell_text(cell)
                }
            }
            Err(e) => e,
        })
        .unwrap_or_else(|| NO_DOC.to_string())
    });

    // get_page_setup: 读页面设置(pt)。section_idx<0 取文档顶层;>=0 取该节生效值(节自己的值,没有则回退顶层)。
    // 返回 {"width","height","top","bottom","left","right","num_cols"};节下标越界返回 err:。
    engine.register_fn("get_page_setup", |section_idx: i64| -> String {
        with_doc(|doc| {
            if section_idx < 0 {
                return serde_json::json!({
                    "width": r2(doc.page_size.0), "height": r2(doc.page_size.1),
                    "top": r2(doc.margin_top), "bottom": r2(doc.margin_bottom),
                    "left": r2(doc.margin_left), "right": r2(doc.margin_right),
                    "num_cols": doc.columns.num_cols,
                }).to_string();
            }
            match doc.sections.get(section_idx as usize) {
                Some(s) => {
                    let ps = s.page_size.unwrap_or(doc.page_size);
                    serde_json::json!({
                        "width": r2(ps.0), "height": r2(ps.1),
                        "top": r2(s.margin_top.unwrap_or(doc.margin_top)),
                        "bottom": r2(s.margin_bottom.unwrap_or(doc.margin_bottom)),
                        "left": r2(s.margin_left.unwrap_or(doc.margin_left)),
                        "right": r2(s.margin_right.unwrap_or(doc.margin_right)),
                        "num_cols": s.columns.as_ref().map(|c| c.num_cols).unwrap_or(doc.columns.num_cols),
                    }).to_string()
                }
                None => format!("err: section_idx {} 越界(共 {} 节,传 -1 取文档顶层)", section_idx, doc.sections.len()),
            }
        })
        .unwrap_or_else(|| NO_DOC.to_string())
    });

    // get_header_footer: 读页眉页脚纯文本(段落用 \n 连接,没有则 null)。section_idx<0 取顶层;>=0 取该节(没有则回退顶层)。
    // 返回 {"header":{"default","even","first"},"footer":{...},"title_page","even_and_odd"}。不解析页码域。
    engine.register_fn("get_header_footer", |section_idx: i64| -> String {
        fn texts(hf: &roe_model::writer::HeaderFooterSet) -> serde_json::Value {
            let j = |o: &Option<Vec<Paragraph>>| o.as_ref().map(|ps| ps.iter().map(para_text).collect::<Vec<_>>().join("\n"));
            serde_json::json!({ "default": j(&hf.default), "even": j(&hf.even), "first": j(&hf.first) })
        }
        with_doc(|doc| {
            if section_idx < 0 {
                return serde_json::json!({
                    "header": texts(&doc.header), "footer": texts(&doc.footer),
                    "title_page": doc.title_page, "even_and_odd": doc.even_and_odd_headers,
                }).to_string();
            }
            match doc.sections.get(section_idx as usize) {
                Some(s) => serde_json::json!({
                    "header": texts(s.header.as_ref().unwrap_or(&doc.header)),
                    "footer": texts(s.footer.as_ref().unwrap_or(&doc.footer)),
                    "title_page": s.title_page.unwrap_or(doc.title_page),
                    "even_and_odd": s.even_and_odd_headers.unwrap_or(doc.even_and_odd_headers),
                }).to_string(),
                None => format!("err: section_idx {} 越界(共 {} 节,传 -1 取文档顶层)", section_idx, doc.sections.len()),
            }
        })
        .unwrap_or_else(|| NO_DOC.to_string())
    });
}

/// 全量注册:只读函数 + 写函数 set_table_cell。
pub fn register(engine: &mut Engine) {
    register_read(engine);
    register_structure(engine);
    register_format(engine);
    register_comments(engine);

    // replace_text_ex: 全文替换,支持整词与字面量。use_regex=false 时 pattern 按字面量。
    // 只处理 body 直属段落(不含表格/文本框)。返回 {"total_count":N,"affected_para_indices":[...]},失败 err:。
    // 现有 replace_text 保持不变(纯正则)。
    engine.register_fn("replace_text_ex", |pattern: String, replacement: String, case_sensitive: bool, whole_word: bool, use_regex: bool| -> String {
        if pattern.is_empty() {
            return "err: 查找内容为空".to_string();
        }
        let re = match roe_writer::find_replace::compile_pattern(&find_build_pattern(&pattern, whole_word, use_regex), case_sensitive) {
            Ok(r) => r,
            Err(e) => return format!("err: {}", e),
        };
        with_doc(|doc| {
            let r = roe_writer::find_replace::replace_text_in_document(doc, &re, &replacement, None);
            serde_json::json!({ "total_count": r.total_count, "affected_para_indices": r.affected_para_indices }).to_string()
        })
        .unwrap_or_else(|| NO_DOC.to_string())
    });

    // set_table_cell: 替换单元格文字,保留首段样式与首个 run 的格式;\n 拆成多段。
    // 单元格里的图片/嵌套表格保留(排在文字之后)。成功返回 "ok",失败返回 "err:..."。
    engine.register_fn("set_table_cell", |body_idx: i64, row: i64, col: i64, text: String| -> String {
        with_doc(|doc| match find_cell(doc, body_idx, row, col) {
            Ok(cell) => {
                if cell.hidden {
                    return "err: 该单元格是合并单元格的延续格,请写合并区域的起始格".to_string();
                }
                let template = cell.content.iter().find_map(|c| match c {
                    CellContent::Paragraph(p) => Some(p.clone()),
                    _ => None,
                }).unwrap_or_else(|| Paragraph {
                    style: StyleRef("Default".into()),
                    runs: vec![],
                    ..Default::default()
                });
                let run_tpl = template.runs.first().cloned().unwrap_or_default();
                let mut new_content: Vec<CellContent> = text.split('\n').map(|line| {
                    let mut p = template.clone();
                    let mut r = run_tpl.clone();
                    r.text = line.to_string();
                    p.runs = if line.is_empty() { vec![] } else { vec![r] };
                    CellContent::Paragraph(p)
                }).collect();
                new_content.extend(
                    cell.content.drain(..).filter(|c| !matches!(c, CellContent::Paragraph(_))),
                );
                cell.content = new_content;
                "ok".to_string()
            }
            Err(e) => e,
        })
        .unwrap_or_else(|| NO_DOC.to_string())
    });
}

// ───────── P2-D 公式:insert_writer_formula / get_writer_formula / set_writer_formula ─────────
// 逻辑对应 roe-api/src/formula_edit.rs(build_formula、insert_formula_run、roe_formula_get/set/insert),
// roe-script 不能依赖 roe-api,改一处请同步另一处。默认字号兜底 12.0(内核取字体注册表,前台实际也是 12.0)。

const WFML_DEFAULT_FONT_PT: f32 = 12.0;

fn wfml_build(latex: &str, font_pt: f32) -> Result<roe_model::writer::Formula, String> {
    if latex.trim().is_empty() {
        return Err("err: 公式为空".to_string());
    }
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<roe_model::writer::Formula, String> {
        let (w, h, d) = roe_formula::latex_layout_size(latex, font_pt as f64)?;
        let dl = roe_formula::latex_to_display_list(latex, font_pt as f64)?;
        Ok(roe_model::writer::Formula {
            latex: latex.to_string(),
            width_pt: w as f32,
            height_pt: h as f32,
            depth_pt: d as f32,
            display_list: dl,
            needs_vertical_strike: false,
        })
    }));
    match r {
        Ok(Ok(f)) => Ok(f),
        Ok(Err(e)) => Err(format!("err: 公式解析或排版失败: {e}")),
        Err(_) => Err("err: 公式排版内部错误".to_string()),
    }
}

fn wfml_para_font(doc: &WriterDocument, para: i64) -> Result<f32, String> {
    if para < 0 {
        return Err(format!("err: para {} 越界", para));
    }
    match doc.body.get(para as usize) {
        Some(BodyElement::Paragraph(p)) => Ok(roe_writer::layout::resolve_para_style(doc, p, WFML_DEFAULT_FONT_PT).font_size),
        _ => Err(format!("err: body[{}] 越界或不是段落", para)),
    }
}

fn wfml_insert_run(p: &mut Paragraph, offset: usize, formula: roe_model::writer::Formula) -> Result<usize, String> {
    let new_run = roe_model::writer::Run { formula: Some(formula), ..Default::default() };
    if p.runs.is_empty() {
        if offset != 0 {
            return Err(format!("err: offset {} 超出段落范围(空段落仅支持 0)", offset));
        }
        p.runs.push(new_run);
        return Ok(0);
    }
    let mut consumed = 0usize;
    for i in 0..p.runs.len() {
        let len = p.runs[i].text.len();
        if offset <= consumed + len {
            let off = offset - consumed;
            if off == 0 {
                p.runs.insert(i, new_run);
                return Ok(i);
            }
            if off == len {
                p.runs.insert(i + 1, new_run);
                return Ok(i + 1);
            }
            if !p.runs[i].text.is_char_boundary(off) {
                return Err(format!("err: offset {} 不在字符边界上(一个汉字 3 字节)", offset));
            }
            let mut right = p.runs[i].clone();
            right.text = p.runs[i].text[off..].to_string();
            right.comment_range_start.clear();
            p.runs[i].text.truncate(off);
            p.runs[i].comment_range_end.clear();
            p.runs.insert(i + 1, new_run);
            p.runs.insert(i + 2, right);
            return Ok(i + 1);
        }
        consumed += len;
    }
    Err(format!("err: offset {} 超出段落范围(总字节长度 {})", offset, consumed))
}

fn wfml_register_read(engine: &mut Engine) {
    // get_writer_formula(para, run_idx): 读公式 run 的 LaTeX 与尺寸,返回 JSON;失败 err:。
    engine.register_fn("get_writer_formula", |para: i64, run_idx: i64| -> String {
        with_doc(|doc| {
            if para < 0 || run_idx < 0 {
                return "err: para/run_idx 不能为负".to_string();
            }
            let Some(BodyElement::Paragraph(p)) = doc.body.get(para as usize) else {
                return format!("err: body[{}] 越界或不是段落", para);
            };
            let Some(run) = p.runs.get(run_idx as usize) else {
                return format!("err: run_idx {} 越界(该段共 {} 个 run)", run_idx, p.runs.len());
            };
            match &run.formula {
                Some(f) => serde_json::json!({
                    "latex": f.latex,
                    "width_pt": r2(f.width_pt),
                    "height_pt": r2(f.height_pt),
                    "depth_pt": r2(f.depth_pt),
                }).to_string(),
                None => format!("err: run[{}] 不是公式", run_idx),
            }
        })
        .unwrap_or_else(|| NO_DOC.to_string())
    });
}

fn wfml_register_write(engine: &mut Engine) {
    // insert_writer_formula(para, offset, latex): 在段内 UTF-8 字节偏移处插入公式,成功 {"para_index":N,"run_idx":M}。
    engine.register_fn("insert_writer_formula", |para: i64, offset: i64, latex: String| -> String {
        with_doc(|doc| {
            if offset < 0 {
                return "err: offset 不能为负".to_string();
            }
            let font = match wfml_para_font(doc, para) {
                Ok(f) => f,
                Err(e) => return e,
            };
            let formula = match wfml_build(&latex, font) {
                Ok(f) => f,
                Err(e) => return e,
            };
            let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(para as usize) else {
                return format!("err: body[{}] 越界或不是段落", para);
            };
            match wfml_insert_run(p, offset as usize, formula) {
                Ok(ri) => serde_json::json!({ "para_index": para, "run_idx": ri }).to_string(),
                Err(e) => e,
            }
        })
        .unwrap_or_else(|| NO_DOC.to_string())
    });

    // set_writer_formula(para, run_idx, latex): 改已有公式的 LaTeX 并重新排版,成功 ok。
    engine.register_fn("set_writer_formula", |para: i64, run_idx: i64, latex: String| -> String {
        with_doc(|doc| {
            if run_idx < 0 {
                return "err: run_idx 不能为负".to_string();
            }
            let para_font = match wfml_para_font(doc, para) {
                Ok(f) => f,
                Err(e) => return e,
            };
            let font = {
                let Some(BodyElement::Paragraph(p)) = doc.body.get(para as usize) else {
                    return format!("err: body[{}] 越界或不是段落", para);
                };
                let Some(run) = p.runs.get(run_idx as usize) else {
                    return format!("err: run_idx {} 越界(该段共 {} 个 run)", run_idx, p.runs.len());
                };
                if run.formula.is_none() {
                    return format!("err: run[{}] 不是公式", run_idx);
                }
                run.font_size.unwrap_or(para_font)
            };
            let formula = match wfml_build(&latex, font) {
                Ok(f) => f,
                Err(e) => return e,
            };
            if let Some(BodyElement::Paragraph(p)) = doc.body.get_mut(para as usize) {
                p.runs[run_idx as usize].formula = Some(formula);
            }
            "ok".to_string()
        })
        .unwrap_or_else(|| NO_DOC.to_string())
    });
}

#[cfg(test)]
mod tests {
    use crate::writer_api::tests::empty_doc;
    use crate::writer_api::{register, with_document};

    fn run(script: &str) -> String {
        let mut doc = empty_doc();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_document(&mut doc, || {
            out = e.eval::<String>(script).unwrap();
        });
        out
    }

    #[test]
    fn inspect_document_lists_paragraph_and_table() {
        let out = run("insert_table(1, 2, 3); inspect_document()");
        let v: serde_json::Value = serde_json::from_str(&out).expect("应是合法 JSON");
        assert_eq!(v["body_count"], 2);
        assert_eq!(v["elements"][0]["type"], "paragraph");
        assert_eq!(v["elements"][0]["text"], "Hello World");
        assert_eq!(v["elements"][0]["text_len"], 11);
        assert_eq!(v["elements"][0]["style"], "Default");
        assert_eq!(v["elements"][1]["type"], "table");
        assert_eq!(v["elements"][1]["index"], 1);
        assert_eq!(v["elements"][1]["rows"], 2);
        assert_eq!(v["elements"][1]["cols"], 3);
    }

    #[test]
    fn inspect_document_truncates_long_text() {
        let out = run(r#"let s = ""; for i in 0..100 { s += "a"; } set_paragraph(0, s); inspect_document()"#);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let el = &v["elements"][0];
        assert_eq!(el["text"].as_str().unwrap().chars().count(), 40);
        assert_eq!(el["text_len"], 100);
        assert_eq!(el["text_truncated"], true);
    }

    #[test]
    fn table_cell_set_then_get() {
        let out = run(r#"insert_table(1, 2, 3); let r = set_table_cell(1, 0, 1, "A"); r + "|" + get_table_cell(1, 0, 1) + "|" + get_table_cell(1, 1, 2)"#);
        assert_eq!(out, "ok|A|");
    }

    #[test]
    fn table_cell_multiline_roundtrip() {
        let out = run(r#"insert_table(1, 1, 1); set_table_cell(1, 0, 0, "a\nb"); get_table_cell(1, 0, 0)"#);
        assert_eq!(out, "a\nb");
    }

    #[test]
    fn table_cell_bad_input_reports_err() {
        // body[0] 是段落,不是表格
        let out = run(r#"insert_table(1, 2, 3); get_table_cell(0, 0, 0)"#);
        assert!(out.starts_with("err:"), "{out}");
        for script in [
            r#"insert_table(1, 2, 3); get_table_cell(9, 0, 0)"#,
            r#"insert_table(1, 2, 3); get_table_cell(1, 5, 0)"#,
            r#"insert_table(1, 2, 3); get_table_cell(1, 0, 9)"#,
            r#"insert_table(1, 2, 3); get_table_cell(-1, 0, 0)"#,
            r#"insert_table(1, 2, 3); set_table_cell(1, 0, 9, "x")"#,
            r#"set_table_cell(0, 0, 0, "x")"#,
        ] {
            let out = run(script);
            assert!(out.starts_with("err:"), "脚本 {script} 应返回 err:,实际 {out}");
        }
    }

    #[test]
    fn bad_set_does_not_modify_document() {
        let out = run(r#"insert_table(1, 1, 1); set_table_cell(1, 0, 5, "x"); get_table_cell(1, 0, 0)"#);
        assert_eq!(out, "");
    }

    #[test]
    fn without_document_returns_err() {
        let mut e = rhai::Engine::new();
        register(&mut e);
        for script in ["inspect_document()", "get_table_cell(0,0,0)", r#"set_table_cell(0,0,0,"x")"#] {
            let out = e.eval::<String>(script).unwrap();
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
    }
}

#[cfg(test)]
mod find_tests {
    use crate::writer_api::tests::empty_doc;
    use crate::writer_api::{register, with_document};
    use roe_model::common::StyleRef;
    use roe_model::writer::{BodyElement, Paragraph, Run, WriterDocument};

    fn para(text: &str) -> BodyElement {
        BodyElement::Paragraph(Paragraph {
            style: StyleRef("Default".into()),
            runs: vec![Run { text: text.to_string(), ..Default::default() }],
            ..Default::default()
        })
    }

    // body: 0 "Hello World"  1 "cat concatenate Cat"  2 "汉字cat"
    fn doc3() -> WriterDocument {
        let mut doc = empty_doc();
        doc.body.push(para("cat concatenate Cat"));
        doc.body.push(para("汉字cat"));
        doc
    }

    fn run(doc: &mut WriterDocument, script: &str) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_document(doc, || { out = e.eval::<String>(script).unwrap(); });
        out
    }

    fn json(s: &str) -> serde_json::Value { serde_json::from_str(s).unwrap() }

    fn para_texts(doc: &WriterDocument) -> Vec<String> {
        doc.body.iter().filter_map(|el| match el {
            BodyElement::Paragraph(p) => Some(p.runs.iter().map(|r| r.text.as_str()).collect::<String>()),
            _ => None,
        }).collect()
    }

    #[test]
    fn literal_case_and_whole_word() {
        let mut doc = doc3();
        // 大小写不敏感:cat / concatenate 里的 cat / Cat / 汉字cat 共 4 处
        let v = json(&run(&mut doc, r#"find_text("cat", false, false, false)"#));
        assert_eq!(v["total"], 4);
        assert_eq!(v["truncated"], false);
        // 区分大小写:少一个 Cat
        let v = json(&run(&mut doc, r#"find_text("cat", true, false, false)"#));
        assert_eq!(v["total"], 3);
        // 整词(不区分大小写):只有段 1 开头的 cat 和末尾的 Cat
        let v = json(&run(&mut doc, r#"find_text("cat", false, true, false)"#));
        assert_eq!(v["total"], 2);
        assert_eq!(v["matches"][0]["para"], 1);
        assert_eq!(v["matches"][0]["start"], 0);
        assert_eq!(v["matches"][0]["end"], 3);
        assert_eq!(v["matches"][0]["text"], "cat");
    }

    #[test]
    fn chinese_byte_offsets() {
        let mut doc = doc3();
        let v = json(&run(&mut doc, r#"find_text("汉字cat", true, false, false)"#));
        assert_eq!(v["total"], 1);
        assert_eq!(v["matches"][0]["para"], 2);
        assert_eq!(v["matches"][0]["start"], 0);
        assert_eq!(v["matches"][0]["end"], 9);
        let v = json(&run(&mut doc, r#"find_text("cat", true, false, false)"#));
        let last = v["matches"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last["para"], 2);
        assert_eq!(last["start"], 6);
    }

    #[test]
    fn literal_vs_regex() {
        let mut doc = doc3();
        // 字面量:点号不是通配
        assert_eq!(json(&run(&mut doc, r#"find_text("c.t", true, false, false)"#))["total"], 0);
        // 正则:点号匹配任意字符
        assert_eq!(json(&run(&mut doc, r#"find_text("c.t", true, false, true)"#))["total"], 3);
        // 字面量里的特殊字符不会报错
        assert_eq!(json(&run(&mut doc, r#"find_text("(", true, false, false)"#))["total"], 0);
    }

    #[test]
    fn truncates_over_200() {
        let mut doc = empty_doc();
        doc.body.push(para(&"a".repeat(250)));
        let v = json(&run(&mut doc, r#"find_text("a", true, false, false)"#));
        assert_eq!(v["total"], 250);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["matches"].as_array().unwrap().len(), 200);
    }

    #[test]
    fn find_bad_input_errs() {
        let mut doc = doc3();
        for script in [
            r#"find_text("", true, false, false)"#,
            r#"find_text("(", true, false, true)"#,
        ] {
            let out = run(&mut doc, script);
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
    }

    #[test]
    fn replace_ex_whole_word_and_literal() {
        let mut doc = doc3();
        let before = para_texts(&doc);
        // 整词替换:concatenate 里的 cat 不动
        let v = json(&run(&mut doc, r#"replace_text_ex("cat", "dog", false, true, false)"#));
        assert_eq!(v["total_count"], 2);
        assert_eq!(para_texts(&doc)[1], "dog concatenate dog");
        assert_eq!(para_texts(&doc)[0], before[0]);
        // 字面量:点号不是通配,不替换任何内容
        let v = json(&run(&mut doc, r#"replace_text_ex("c.t", "X", true, false, false)"#));
        assert_eq!(v["total_count"], 0);
        // 汉字段落替换
        let v = json(&run(&mut doc, r#"replace_text_ex("汉字", "中文", true, false, false)"#));
        assert_eq!(v["total_count"], 1);
        assert_eq!(para_texts(&doc)[2], "中文cat");
    }

    #[test]
    fn replace_ex_bad_input_errs_and_changes_nothing() {
        let mut doc = doc3();
        let before = para_texts(&doc);
        for script in [
            r#"replace_text_ex("", "x", true, false, false)"#,
            r#"replace_text_ex("(", "x", true, false, true)"#,
        ] {
            let out = run(&mut doc, script);
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
        assert_eq!(para_texts(&doc), before);
    }

    #[test]
    fn without_document_returns_err() {
        let mut e = rhai::Engine::new();
        register(&mut e);
        for script in [
            r#"find_text("a", true, false, false)"#,
            r#"replace_text_ex("a", "b", true, false, false)"#,
        ] {
            let out = e.eval::<String>(script).unwrap();
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
    }
}

#[cfg(test)]
mod page_tests {
    use crate::writer_api::tests::empty_doc;
    use crate::writer_api::{register, with_document};
    use roe_model::common::StyleRef;
    use roe_model::writer::{DocumentColumns, HeaderFooterSet, Paragraph, Run, Section, WriterDocument};

    fn paras(t: &str) -> Vec<Paragraph> {
        vec![Paragraph {
            style: StyleRef("Default".into()),
            runs: vec![Run { text: t.to_string(), ..Default::default() }],
            ..Default::default()
        }]
    }

    fn run(doc: &mut WriterDocument, script: &str) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_document(doc, || { out = e.eval::<String>(script).unwrap(); });
        out
    }

    fn json(s: &str) -> serde_json::Value { serde_json::from_str(s).unwrap() }

    fn doc_with_sections() -> WriterDocument {
        let mut doc = empty_doc();
        doc.margin_top = 72.0; doc.margin_bottom = 70.0; doc.margin_left = 60.0; doc.margin_right = 50.0;
        doc.header = HeaderFooterSet { default: Some(paras("顶层页眉")), even: None, first: Some(paras("首页页眉")) };
        doc.footer = HeaderFooterSet { default: Some(paras("顶层页脚")), even: None, first: None };
        doc.title_page = true;
        // 节 0:全部沿用顶层
        doc.sections.push(Section { name: "s0".into(), ..Default::default() });
        // 节 1:自己的页面、边距、分栏、页眉
        doc.sections.push(Section {
            name: "s1".into(),
            page_size: Some((300.0, 400.0)),
            margin_top: Some(10.0),
            columns: Some(DocumentColumns { num_cols: 2, uniform_spacing: 0.0, col_widths: vec![], col_spacing: vec![] }),
            header: Some(HeaderFooterSet { default: Some(paras("节1页眉")), even: Some(paras("偶数页")), first: None }),
            title_page: Some(false),
            ..Default::default()
        });
        doc
    }

    #[test]
    fn page_setup_top_and_sections() {
        let mut doc = doc_with_sections();
        let v = json(&run(&mut doc, "get_page_setup(-1)"));
        assert_eq!((v["width"].as_f64().unwrap(), v["top"].as_f64().unwrap(), v["num_cols"].as_i64().unwrap()), (595.28, 72.0, 1));
        // 节 0:全部回退顶层
        let v = json(&run(&mut doc, "get_page_setup(0)"));
        assert_eq!((v["width"].as_f64().unwrap(), v["left"].as_f64().unwrap()), (595.28, 60.0));
        // 节 1:自己的值覆盖,没写的边距回退顶层
        let v = json(&run(&mut doc, "get_page_setup(1)"));
        assert_eq!((v["width"].as_f64().unwrap(), v["height"].as_f64().unwrap(), v["top"].as_f64().unwrap()), (300.0, 400.0, 10.0));
        assert_eq!((v["bottom"].as_f64().unwrap(), v["num_cols"].as_i64().unwrap()), (70.0, 2));
    }

    #[test]
    fn header_footer_top_and_sections() {
        let mut doc = doc_with_sections();
        let v = json(&run(&mut doc, "get_header_footer(-1)"));
        assert_eq!(v["header"]["default"], "顶层页眉");
        assert_eq!(v["header"]["first"], "首页页眉");
        assert!(v["header"]["even"].is_null());
        assert_eq!(v["footer"]["default"], "顶层页脚");
        assert_eq!(v["title_page"], true);
        // 节 0:回退顶层
        let v = json(&run(&mut doc, "get_header_footer(0)"));
        assert_eq!(v["header"]["default"], "顶层页眉");
        // 节 1:页眉用自己的,页脚回退顶层,title_page 用自己的
        let v = json(&run(&mut doc, "get_header_footer(1)"));
        assert_eq!(v["header"]["default"], "节1页眉");
        assert_eq!(v["header"]["even"], "偶数页");
        assert_eq!(v["footer"]["default"], "顶层页脚");
        assert_eq!(v["title_page"], false);
    }

    #[test]
    fn out_of_range_errs() {
        let mut doc = doc_with_sections();
        for script in ["get_page_setup(2)", "get_page_setup(99)", "get_header_footer(2)"] {
            let out = run(&mut doc, script);
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
    }

    #[test]
    fn without_document_returns_err() {
        let mut e = rhai::Engine::new();
        register(&mut e);
        for script in ["get_page_setup(-1)", "get_header_footer(-1)"] {
            let out = e.eval::<String>(script).unwrap();
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
    }
}


#[cfg(test)]
mod structure_tests {
    use crate::engine::ScriptEngine;
    use roe_model::writer::{BodyElement, WriterDocument};

    fn run(doc: &mut WriterDocument, script: &str) -> String {
        let eng = ScriptEngine::new();
        match eng.run_writer(script, doc) {
            Ok(v) => v.as_str().map(|x| x.to_string()).unwrap_or_else(|| v.to_string()),
            Err(e) => format!("RUNERR:{e}"),
        }
    }

    fn texts(doc: &WriterDocument) -> Vec<String> {
        doc.body.iter().map(|b| match b {
            BodyElement::Paragraph(p) => p.runs.iter().map(|r| r.text.as_str()).collect::<String>(),
            _ => "<non-para>".to_string(),
        }).collect()
    }

    fn doc_with(lines: &[&str]) -> WriterDocument {
        let mut d = WriterDocument::default();
        for l in lines {
            let out = run(&mut d, &format!("append_paragraph(\"{}\")", l));
            assert!(!out.starts_with("RUNERR"), "造文档失败: {out}");
        }
        d.body.retain(|b| matches!(b, BodyElement::Paragraph(p) if !p.runs.is_empty()));
        d
    }

    fn para(d: &WriterDocument, i: usize) -> &roe_model::writer::Paragraph {
        match &d.body[i] { BodyElement::Paragraph(p) => p, _ => panic!("非段落") }
    }

    #[test]
    fn page_break_offset_zero_sets_flag_only() {
        let mut d = doc_with(&["abc"]);
        let n = d.body.len();
        assert_eq!(run(&mut d, "insert_page_break(0, 0)"), "ok");
        assert_eq!(d.body.len(), n);
        assert!(para(&d, 0).page_break_before);
    }

    #[test]
    fn page_break_splits_chinese_at_byte_offset() {
        let mut d = doc_with(&["你好世界"]);
        let n = d.body.len();
        assert_eq!(run(&mut d, "insert_page_break(0, 3)"), "ok");
        assert_eq!(d.body.len(), n + 1);
        let t = texts(&d);
        assert_eq!(t[0], "你");
        assert_eq!(t[1], "好世界");
        assert!(para(&d, 1).page_break_before);
        assert!(!para(&d, 0).page_break_before);
    }

    #[test]
    fn page_break_bad_offset_leaves_doc_unchanged() {
        let mut d = doc_with(&["你好世界"]);
        let before = texts(&d);
        for sc in ["insert_page_break(0, 1)", "insert_page_break(0, 99)", "insert_page_break(0, -1)", "insert_page_break(9, 0)", "insert_page_break(-1, 0)"] {
            let out = run(&mut d, sc);
            assert!(out.starts_with("err:"), "{sc} => {out}");
        }
        assert_eq!(texts(&d), before);
        assert!(!para(&d, 0).page_break_before);
    }

    #[test]
    fn column_break_rejected_on_single_column() {
        let mut d = doc_with(&["abcdef"]);
        let before = texts(&d);
        let out = run(&mut d, "insert_column_break(0, 3)");
        assert!(out.starts_with("err:"), "{out}");
        assert_eq!(texts(&d), before);
    }

    #[test]
    fn column_break_splits_on_multi_column() {
        let mut d = doc_with(&["abcdef"]);
        d.columns.num_cols = 2;
        let n = d.body.len();
        assert_eq!(run(&mut d, "insert_column_break(0, 3)"), "ok");
        assert_eq!(d.body.len(), n + 1);
        let t = texts(&d);
        assert_eq!((t[0].as_str(), t[1].as_str()), ("abc", "def"));
        assert!(para(&d, 0).column_break_after);
        assert!(!para(&d, 1).column_break_after);
        let out = run(&mut d, "insert_column_break(0, 99)");
        assert!(out.starts_with("err:"), "{out}");
    }

    #[test]
    fn change_case_basic_modes() {
        let mut d = doc_with(&["hello world"]);
        assert_eq!(run(&mut d, "change_case(0, 0, 5, 2)"), "ok");
        assert_eq!(texts(&d)[0], "HELLO world");
        assert_eq!(run(&mut d, "change_case(0, 0, 11, 3)"), "ok");
        assert_eq!(texts(&d)[0], "Hello World");
        assert_eq!(run(&mut d, "change_case(0, 0, 11, 1)"), "ok");
        assert_eq!(texts(&d)[0], "hello world");
        assert_eq!(run(&mut d, "change_case(0, 0, 11, 4)"), "ok");
        assert_eq!(texts(&d)[0], "HELLO WORLD");
    }

    #[test]
    fn change_case_errors_leave_doc_unchanged() {
        let mut d = doc_with(&["abc你好"]);
        let before = texts(&d);
        for sc in [
            "change_case(0, 0, 3, 9)",
            "change_case(0, 2, 2, 2)",
            "change_case(0, 3, 1, 2)",
            "change_case(0, -1, 2, 2)",
            "change_case(0, 0, 99, 2)",
            "change_case(0, 0, 4, 2)",
            "change_case(7, 0, 1, 2)",
        ] {
            let out = run(&mut d, sc);
            assert!(out.starts_with("err:"), "{sc} => {out}");
        }
        assert_eq!(texts(&d), before);
    }

    #[test]
    fn delete_range_within_and_across_paragraphs() {
        let mut d = doc_with(&["abcdef"]);
        assert_eq!(run(&mut d, "delete_range(0, 1, 0, 4)"), "ok");
        assert_eq!(texts(&d)[0], "aef");

        let mut d = doc_with(&["abcdef", "ghijkl", "mnopqr"]);
        let n = d.body.len();
        assert_eq!(run(&mut d, "delete_range(0, 3, 2, 3)"), "ok");
        assert_eq!(d.body.len(), n - 2);
        assert_eq!(texts(&d)[0], "abcpqr");

        let mut d = doc_with(&["abcdef", "ghijkl"]);
        assert_eq!(run(&mut d, "delete_range(1, 2, 0, 4)"), "ok");
        assert_eq!(texts(&d)[0], "abcdijkl");
    }

    #[test]
    fn delete_range_errors_leave_doc_unchanged() {
        let mut d = doc_with(&["你好世界", "second"]);
        let before = texts(&d);
        for sc in [
            "delete_range(0, 3, 0, 3)",
            "delete_range(0, 1, 0, 6)",
            "delete_range(0, 0, 0, 99)",
            "delete_range(0, 0, 9, 0)",
            "delete_range(-1, 0, 0, 3)",
            "delete_range(0, -1, 0, 3)",
        ] {
            let out = run(&mut d, sc);
            assert!(out.starts_with("err:"), "{sc} => {out}");
        }
        assert_eq!(texts(&d), before);
    }
}


#[cfg(test)]
mod format_tests {
    use crate::engine::ScriptEngine;
    use roe_model::writer::{BodyElement, Paragraph, WriterDocument};

    fn run(doc: &mut WriterDocument, script: &str) -> String {
        let eng = ScriptEngine::new();
        match eng.run_writer(script, doc) {
            Ok(v) => v.as_str().map(|x| x.to_string()).unwrap_or_else(|| v.to_string()),
            Err(e) => format!("RUNERR:{e}"),
        }
    }

    fn doc_with(lines: &[&str]) -> WriterDocument {
        let mut d = WriterDocument::default();
        for l in lines {
            let out = run(&mut d, &format!("append_paragraph(\"{}\")", l));
            assert!(!out.starts_with("RUNERR"), "造文档失败: {out}");
        }
        d.body.retain(|b| matches!(b, BodyElement::Paragraph(p) if !p.runs.is_empty()));
        d
    }

    fn para(d: &WriterDocument, i: usize) -> &Paragraph {
        match &d.body[i] { BodyElement::Paragraph(p) => p, _ => panic!("非段落") }
    }

    #[test]
    fn shading_set_and_clear() {
        let mut d = doc_with(&["abc"]);
        assert_eq!(run(&mut d, r##"set_paragraph_shading(0, "#ff8000")"##), "ok");
        assert_eq!(para(&d, 0).shading, Some([255, 128, 0, 255]));
        assert_eq!(run(&mut d, r#"set_paragraph_shading(0, "none")"#), "ok");
        assert_eq!(para(&d, 0).shading, None);
    }

    #[test]
    fn shading_errors_leave_doc_unchanged() {
        let mut d = doc_with(&["abc"]);
        for sc in [
            r#"set_paragraph_shading(0, "red")"#,
            r##"set_paragraph_shading(0, "#12345")"##,
            r##"set_paragraph_shading(0, "#gggggg")"##,
            r##"set_paragraph_shading(9, "#ffffff")"##,
            r##"set_paragraph_shading(-1, "#ffffff")"##,
        ] {
            let out = run(&mut d, sc);
            assert!(out.starts_with("err:"), "{sc} => {out}");
        }
        assert_eq!(para(&d, 0).shading, None);
    }

    #[test]
    fn list_set_continue_and_cancel() {
        let mut d = doc_with(&["a", "b"]);
        assert_eq!(run(&mut d, "set_paragraph_list(0, 2, 1)"), "ok");
        let li0 = para(&d, 0).list_info.clone().expect("应有 list_info");
        assert_eq!(li0.style_name, "ROE_Number_1");
        assert_eq!(li0.level, 0);
        assert!(d.styles.list_styles.contains_key("ROE_Number_1"));
        assert_eq!(run(&mut d, "set_paragraph_list(1, 2, 1)"), "ok");
        assert_eq!(para(&d, 1).list_info.as_ref().unwrap().list_id, li0.list_id, "同样式应接续同一列表");
        assert_eq!(run(&mut d, "set_paragraph_list(1, 1, 2)"), "ok");
        assert_eq!(para(&d, 1).list_info.as_ref().unwrap().style_name, "ROE_Bullet_2");
        assert_eq!(run(&mut d, "set_paragraph_list(0, 0, 0)"), "ok");
        assert!(para(&d, 0).list_info.is_none());
        assert_eq!(run(&mut d, "set_paragraph_list(0, 0, 0)"), "ok");
    }

    #[test]
    fn list_errors_leave_doc_unchanged() {
        let mut d = doc_with(&["a"]);
        for sc in [
            "set_paragraph_list(0, 3, 1)",
            "set_paragraph_list(0, -1, 1)",
            "set_paragraph_list(0, 2, 0)",
            "set_paragraph_list(0, 2, 7)",
            "set_paragraph_list(9, 2, 1)",
            "set_paragraph_list(-1, 2, 1)",
        ] {
            let out = run(&mut d, sc);
            assert!(out.starts_with("err:"), "{sc} => {out}");
        }
        assert!(para(&d, 0).list_info.is_none());
        assert!(d.styles.list_styles.is_empty(), "出错不应登记列表样式");
    }
}


#[cfg(test)]
mod comment_tests {
    use crate::engine::ScriptEngine;
    use roe_model::writer::{BodyElement, Paragraph, WriterDocument};

    fn run(doc: &mut WriterDocument, script: &str) -> String {
        let eng = ScriptEngine::new();
        match eng.run_writer(script, doc) {
            Ok(v) => v.as_str().map(|x| x.to_string()).unwrap_or_else(|| v.to_string()),
            Err(e) => format!("RUNERR:{e}"),
        }
    }

    fn doc_with(lines: &[&str]) -> WriterDocument {
        let mut d = WriterDocument::default();
        for l in lines {
            let out = run(&mut d, &format!("append_paragraph(\"{}\")", l));
            assert!(!out.starts_with("RUNERR"), "造文档失败: {out}");
        }
        d.body.retain(|b| matches!(b, BodyElement::Paragraph(p) if !p.runs.is_empty()));
        d
    }

    fn para(d: &WriterDocument, i: usize) -> &Paragraph {
        match &d.body[i] { BodyElement::Paragraph(p) => p, _ => panic!("非段落") }
    }

    fn texts(d: &WriterDocument) -> Vec<String> {
        d.body.iter().map(|b| match b {
            BodyElement::Paragraph(p) => p.runs.iter().map(|r| r.text.as_str()).collect::<String>(),
            _ => "<non-para>".to_string(),
        }).collect()
    }

    fn run_counts(d: &WriterDocument) -> Vec<usize> {
        d.body.iter().map(|b| match b { BodyElement::Paragraph(p) => p.runs.len(), _ => 0 }).collect()
    }

    fn threads(d: &mut WriterDocument) -> serde_json::Value {
        let s = run(d, "get_comment_threads()");
        serde_json::from_str(&s).unwrap_or_else(|e| panic!("非 JSON: {s} ({e})"))
    }

    #[test]
    fn threads_empty_doc() {
        let mut d = doc_with(&["abc"]);
        assert_eq!(run(&mut d, "get_comment_threads()"), r#"{"threads":[]}"#);
    }

    #[test]
    fn insert_comment_same_paragraph_and_reversed() {
        let mut d = doc_with(&["hello world"]);
        assert_eq!(run(&mut d, r#"insert_comment(0, 0, 0, 5, "Tom", "check")"#), r#"{"id":0}"#);
        assert_eq!(d.comments.len(), 1);
        assert_eq!(texts(&d)[0], "hello world");
        let p = para(&d, 0);
        assert!(p.runs.iter().any(|r| r.comment_range_start.contains(&0)));
        assert!(p.runs.iter().any(|r| r.comment_range_end.contains(&0)));
        assert!(p.runs.iter().any(|r| r.comment_ref == Some(0)));
        let t = threads(&mut d);
        assert_eq!(t["threads"][0]["root"]["author"], "Tom");
        assert_eq!(t["threads"][0]["root"]["text"], "check");
        assert_eq!(run(&mut d, r#"insert_comment(0, 11, 0, 6, "Amy", "rev")"#), r#"{"id":1}"#);
        assert_eq!(d.comments.len(), 2);
    }

    #[test]
    fn insert_comment_cross_paragraph_chinese() {
        let mut d = doc_with(&["abc", "你好世界"]);
        assert_eq!(run(&mut d, r#"insert_comment(0, 1, 1, 6, "T", "跨段")"#), r#"{"id":0}"#);
        assert_eq!(texts(&d), vec!["abc".to_string(), "你好世界".to_string()]);
        assert!(para(&d, 0).runs.iter().any(|r| r.comment_range_start.contains(&0)));
        assert!(para(&d, 1).runs.iter().any(|r| r.comment_range_end.contains(&0)));
        assert!(para(&d, 1).runs.iter().any(|r| r.comment_ref == Some(0)));
    }

    #[test]
    fn insert_comment_errors_leave_doc_unchanged() {
        let mut d = doc_with(&["abc", "你好"]);
        let before = run_counts(&d);
        for sc in [
            r#"insert_comment(0, 2, 0, 2, "T", "x")"#,
            r#"insert_comment(0, 0, 0, 2, "T", "")"#,
            r#"insert_comment(9, 0, 9, 1, "T", "x")"#,
            r#"insert_comment(0, -1, 0, 2, "T", "x")"#,
            r#"insert_comment(0, 0, 0, 99, "T", "x")"#,
            r#"insert_comment(0, 1, 1, 1, "T", "x")"#,
            r#"insert_comment(0, 1, 1, 99, "T", "x")"#,
        ] {
            let out = run(&mut d, sc);
            assert!(out.starts_with("err:"), "{sc} => {out}");
        }
        assert_eq!(run_counts(&d), before, "出错不应改段落");
        assert!(d.comments.is_empty());
        assert!(para(&d, 0).runs.iter().all(|r| r.comment_range_start.is_empty()));
    }

    #[test]
    fn reply_comment_flow_and_errors() {
        let mut d = doc_with(&["hello world"]);
        assert_eq!(run(&mut d, r#"insert_comment(0, 0, 0, 5, "Tom", "root")"#), r#"{"id":0}"#);
        assert_eq!(run(&mut d, r#"reply_comment(0, "Amy", "re")"#), r#"{"id":1}"#);
        let t = threads(&mut d);
        assert_eq!(t["threads"].as_array().unwrap().len(), 1);
        assert_eq!(t["threads"][0]["replies"][0]["author"], "Amy");
        assert_eq!(t["threads"][0]["replies"][0]["text"], "re");
        let n = d.comments.len();
        let counts = run_counts(&d);
        for sc in [
            r#"reply_comment(1, "X", "no")"#,
            r#"reply_comment(99, "X", "no")"#,
            r#"reply_comment(-1, "X", "no")"#,
            r#"reply_comment(0, "X", "")"#,
        ] {
            let out = run(&mut d, sc);
            assert!(out.starts_with("err:"), "{sc} => {out}");
        }
        assert_eq!(d.comments.len(), n);
        assert_eq!(run_counts(&d), counts);
    }
}


#[cfg(test)]
mod note_tests {
    use crate::engine::ScriptEngine;
    use roe_model::writer::{BodyElement, NumFormat, Paragraph, WriterDocument};

    fn run(doc: &mut WriterDocument, script: &str) -> String {
        let eng = ScriptEngine::new();
        match eng.run_writer(script, doc) {
            Ok(v) => v.as_str().map(|x| x.to_string()).unwrap_or_else(|| v.to_string()),
            Err(e) => format!("RUNERR:{e}"),
        }
    }

    fn doc_with(lines: &[&str]) -> WriterDocument {
        let mut d = WriterDocument::default();
        for l in lines {
            let out = run(&mut d, &format!("append_paragraph(\"{}\")", l));
            assert!(!out.starts_with("RUNERR"), "造文档失败: {out}");
        }
        d.body.retain(|b| matches!(b, BodyElement::Paragraph(p) if !p.runs.is_empty()));
        d
    }

    fn para(d: &WriterDocument, i: usize) -> &Paragraph {
        match &d.body[i] {
            BodyElement::Paragraph(p) => p,
            _ => panic!("不是段落"),
        }
    }

    #[test]
    fn footnote_basic_split_runs() {
        let mut d = doc_with(&["hello world"]);
        let out = run(&mut d, r#"insert_footnote(0, 5, 0, "note")"#);
        assert!(out.starts_with(r#"{"id":"#), "{out}");
        assert_eq!(d.footnotes.len(), 1);
        assert_eq!(d.endnotes.len(), 0);
        let p = para(&d, 0);
        assert_eq!(p.runs.len(), 3);
        assert_eq!(p.runs[0].text, "hello");
        assert!(p.runs[1].footnote_ref.is_some());
        assert_eq!(p.runs[2].text, " world");
        // 第二个脚注 id 递增
        let out2 = run(&mut d, r#"insert_footnote(0, 0, 0, "second")"#);
        assert_ne!(out, out2);
        assert_eq!(d.footnotes.len(), 2);
    }

    #[test]
    fn endnote_switches_to_lower_roman_once() {
        let mut d = doc_with(&["hello world"]);
        assert!(matches!(d.endnote_num_fmt, NumFormat::Arabic));
        let out = run(&mut d, r#"insert_footnote(0, 0, 1, "e1")"#);
        assert!(out.starts_with(r#"{"id":"#), "{out}");
        assert_eq!(d.endnotes.len(), 1);
        assert!(matches!(d.endnote_num_fmt, NumFormat::LowerRoman));
        assert!(para(&d, 0).runs.iter().any(|r| r.endnote_ref.is_some()));
    }

    #[test]
    fn footnote_chinese_offsets_and_multiline() {
        let mut d = doc_with(&["你好世界"]);
        let out = run(&mut d, r#"insert_footnote(0, 3, 0, "第一行\n第二行")"#);
        assert!(out.starts_with(r#"{"id":"#), "{out}");
        assert_eq!(d.footnotes[0].content.len(), 2);
        let p = para(&d, 0);
        assert_eq!(p.runs[0].text, "你");
        assert_eq!(p.runs[2].text, "好世界");
        // 偏移 1 落在汉字中间
        let before = d.footnotes.len();
        let bad = run(&mut d, r#"insert_footnote(0, 1, 0, "x")"#);
        assert!(bad.starts_with("err:"), "{bad}");
        assert_eq!(d.footnotes.len(), before);
    }

    #[test]
    fn footnote_errors_leave_doc_unchanged() {
        let mut d = doc_with(&["hello"]);
        let runs_before = para(&d, 0).runs.len();
        for sc in [
            r#"insert_footnote(9, 0, 0, "x")"#,
            r#"insert_footnote(-1, 0, 0, "x")"#,
            r#"insert_footnote(0, -1, 0, "x")"#,
            r#"insert_footnote(0, 99, 0, "x")"#,
            r#"insert_footnote(0, 99, 1, "x")"#,
            r#"insert_footnote(0, 0, 2, "x")"#,
            r#"insert_footnote(0, 0, 0, "")"#,
            r#"insert_footnote(0, 0, 1, "  ")"#,
        ] {
            let out = run(&mut d, sc);
            assert!(out.starts_with("err:"), "{sc} => {out}");
        }
        assert_eq!(d.footnotes.len(), 0);
        assert_eq!(d.endnotes.len(), 0);
        assert_eq!(para(&d, 0).runs.len(), runs_before);
        assert!(matches!(d.endnote_num_fmt, NumFormat::Arabic), "尾注编号格式被误改");
    }
}


#[cfg(test)]
mod sort_tests {
    use crate::engine::ScriptEngine;
    use roe_model::writer::{BodyElement, WriterDocument};

    fn run(doc: &mut WriterDocument, script: &str) -> String {
        let eng = ScriptEngine::new();
        match eng.run_writer(script, doc) {
            Ok(v) => v.as_str().map(|x| x.to_string()).unwrap_or_else(|| v.to_string()),
            Err(e) => format!("RUNERR:{e}"),
        }
    }

    fn doc_with(lines: &[&str]) -> WriterDocument {
        let mut d = WriterDocument::default();
        for l in lines {
            let out = run(&mut d, &format!("append_paragraph(\"{}\")", l));
            assert!(!out.starts_with("RUNERR"), "造文档失败: {out}");
        }
        d.body.retain(|b| matches!(b, BodyElement::Paragraph(p) if !p.runs.is_empty()));
        d
    }

    fn texts(d: &WriterDocument) -> Vec<String> {
        d.body
            .iter()
            .map(|b| match b {
                BodyElement::Paragraph(p) => p.runs.iter().map(|r| r.text.as_str()).collect::<String>(),
                _ => "<非段落>".to_string(),
            })
            .collect()
    }

    #[test]
    fn sort_text_pinyin_and_ascii() {
        let mut d = doc_with(&["张三", "Bob", "李四", "alice", "王五"]);
        assert_eq!(run(&mut d, "sort_paragraphs(0, 4, 0, 0)"), "ok");
        // 字母(忽略大小写)在前,汉字按拼音:li < wang < zhang
        assert_eq!(texts(&d), vec!["alice", "Bob", "李四", "王五", "张三"]);
        assert_eq!(run(&mut d, "sort_paragraphs(0, 4, 0, 1)"), "ok");
        assert_eq!(texts(&d), vec!["张三", "王五", "李四", "Bob", "alice"]);
    }

    #[test]
    fn sort_number_first_number_and_none_first() {
        let mut d = doc_with(&["item 10", "no number", "item 2", "item 33"]);
        assert_eq!(run(&mut d, "sort_paragraphs(0, 3, 1, 0)"), "ok");
        assert_eq!(texts(&d), vec!["no number", "item 2", "item 10", "item 33"]);
    }

    #[test]
    fn sort_sub_range_only() {
        let mut d = doc_with(&["z", "c", "b", "a", "y"]);
        assert_eq!(run(&mut d, "sort_paragraphs(1, 3, 0, 0)"), "ok");
        assert_eq!(texts(&d), vec!["z", "a", "b", "c", "y"]);
    }

    #[test]
    fn sort_out_of_range_error_tells_max_end() {
        let mut d = doc_with(&["b", "a", "c"]);
        let before = texts(&d);
        let out = run(&mut d, "sort_paragraphs(0, 3, 0, 0)");
        assert!(out.starts_with("err:") && out.contains("end 最大为 2") && out.contains("end 包含"), "{out}");
        assert_eq!(texts(&d), before);
    }

    #[test]
    fn sort_errors_leave_doc_unchanged() {
        let mut d = doc_with(&["b", "a", "c"]);
        let before = texts(&d);
        for sc in [
            "sort_paragraphs(0, 2, 2, 0)",
            "sort_paragraphs(-1, 2, 0, 0)",
            "sort_paragraphs(0, -1, 0, 0)",
            "sort_paragraphs(1, 1, 0, 0)",
            "sort_paragraphs(2, 0, 0, 0)",
            "sort_paragraphs(0, 9, 0, 0)",
        ] {
            let out = run(&mut d, sc);
            assert!(out.starts_with("err:"), "{sc} => {out}");
        }
        assert_eq!(texts(&d), before);
        // 含脚注引用的段落不能排
        assert!(run(&mut d, r#"insert_footnote(1, 0, 0, "n")"#).starts_with(r#"{"id":"#));
        let before2 = texts(&d);
        let out = run(&mut d, "sort_paragraphs(0, 2, 0, 0)");
        assert!(out.starts_with("err:"), "{out}");
        assert_eq!(texts(&d), before2);
    }
}


#[cfg(test)]
mod shape_tests {
    use crate::engine::ScriptEngine;
    use roe_model::writer::{BodyElement, WriterDocument};

    fn run(doc: &mut WriterDocument, script: &str) -> String {
        let eng = ScriptEngine::new();
        match eng.run_writer(script, doc) {
            Ok(v) => v.as_str().map(|x| x.to_string()).unwrap_or_else(|| v.to_string()),
            Err(e) => format!("RUNERR:{e}"),
        }
    }

    fn doc_with(lines: &[&str]) -> WriterDocument {
        let mut d = WriterDocument::default();
        for l in lines {
            let out = run(&mut d, &format!("append_paragraph(\"{}\")", l));
            assert!(!out.starts_with("RUNERR"), "造文档失败: {out}");
        }
        d.body.retain(|b| matches!(b, BodyElement::Paragraph(p) if !p.runs.is_empty()));
        d
    }

    #[test]
    fn insert_shape_positions_and_kinds() {
        let mut d = doc_with(&["a", "b", "c"]);
        assert_eq!(run(&mut d, r#"insert_writer_shape(1, "rect", 10.0, 20.0, 100.0, 50.0)"#), r#"{"index":1}"#);
        assert_eq!(d.body.len(), 4);
        match &d.body[1] {
            BodyElement::Shape(sh) => {
                assert_eq!(sh.width_pt, 100.0);
                assert_eq!(sh.height_pt, 50.0);
                assert_eq!(sh.x_pt, 10.0);
                assert_eq!(sh.y_pt, 20.0);
            }
            _ => panic!("不是形状"),
        }
        // 末尾插入、预设名、带箭头的线
        assert_eq!(run(&mut d, r#"insert_writer_shape(4, "star5", 0.0, 0.0, 80.0, 80.0)"#), r#"{"index":4}"#);
        assert_eq!(run(&mut d, r#"insert_writer_shape(0, "lineWithArrow", 0.0, 0.0, 60.0, 10.0)"#), r#"{"index":0}"#);
        assert!(matches!(d.body[0], BodyElement::Shape(_)));
        assert_eq!(d.body.len(), 6);
    }

    #[test]
    fn insert_shape_errors_leave_doc_unchanged() {
        let mut d = doc_with(&["a", "b"]);
        let n = d.body.len();
        for sc in [
            r#"insert_writer_shape(-1, "rect", 0.0, 0.0, 10.0, 10.0)"#,
            r#"insert_writer_shape(99, "rect", 0.0, 0.0, 10.0, 10.0)"#,
            r#"insert_writer_shape(0, "noSuchShape", 0.0, 0.0, 10.0, 10.0)"#,
            r#"insert_writer_shape(0, "rect", 0.0, 0.0, 0.0, 10.0)"#,
            r#"insert_writer_shape(0, "rect", 0.0, 0.0, 10.0, -5.0)"#,
        ] {
            let out = run(&mut d, sc);
            assert!(out.starts_with("err:"), "{sc} => {out}");
        }
        assert_eq!(d.body.len(), n);
    }

    #[test]
    fn rotation_set_normalize_and_errors() {
        let mut d = doc_with(&["a"]);
        assert_eq!(run(&mut d, r#"insert_writer_shape(1, "rect", 0.0, 0.0, 10.0, 10.0)"#), r#"{"index":1}"#);
        let rot = |d: &WriterDocument| match &d.body[1] {
            BodyElement::Shape(sh) => sh.rotation,
            _ => panic!("不是形状"),
        };
        assert_eq!(run(&mut d, "set_object_rotation(1, 90.0)"), "ok");
        assert_eq!(rot(&d), Some(90.0));
        assert_eq!(run(&mut d, "set_object_rotation(1, -90.0)"), "ok");
        assert_eq!(rot(&d), Some(270.0));
        assert_eq!(run(&mut d, "set_object_rotation(1, 360.0)"), "ok");
        assert_eq!(rot(&d), None);
        for sc in ["set_object_rotation(0, 30.0)", "set_object_rotation(9, 30.0)", "set_object_rotation(-1, 30.0)"] {
            let out = run(&mut d, sc);
            assert!(out.starts_with("err:"), "{sc} => {out}");
        }
    }

    #[test]
    fn get_rotation_reads_back_and_errors() {
        let mut d = doc_with(&["a"]);
        assert_eq!(run(&mut d, r#"insert_writer_shape(1, "rect", 0.0, 0.0, 10.0, 10.0)"#), r#"{"index":1}"#);
        assert_eq!(run(&mut d, "get_object_rotation(1)"), "0");
        assert_eq!(run(&mut d, "set_object_rotation(1, 90.0)"), "ok");
        assert_eq!(run(&mut d, "get_object_rotation(1)"), "90");
        assert_eq!(run(&mut d, "set_object_rotation(1, -90.0)"), "ok");
        assert_eq!(run(&mut d, "get_object_rotation(1)"), "270");
        for sc in ["get_object_rotation(0)", "get_object_rotation(9)", "get_object_rotation(-1)"] {
            let out = run(&mut d, sc);
            assert!(out.starts_with("err:"), "{sc} => {out}");
        }
    }
}


#[cfg(test)]
mod chart_tests {
    use crate::engine::ScriptEngine;
    use roe_model::writer::{BodyElement, WriterDocument};

    fn run(doc: &mut WriterDocument, script: &str) -> String {
        let eng = ScriptEngine::new();
        match eng.run_writer(script, doc) {
            Ok(v) => v.as_str().map(|x| x.to_string()).unwrap_or_else(|| v.to_string()),
            Err(e) => format!("RUNERR:{e}"),
        }
    }

    fn doc_with(lines: &[&str]) -> WriterDocument {
        let mut d = WriterDocument::default();
        for l in lines {
            let out = run(&mut d, &format!("append_paragraph(\"{}\")", l));
            assert!(!out.starts_with("RUNERR"), "造文档失败: {out}");
        }
        d.body.retain(|b| matches!(b, BodyElement::Paragraph(p) if !p.runs.is_empty()));
        d
    }

    fn ptext(d: &WriterDocument, i: usize) -> String {
        match &d.body[i] {
            BodyElement::Paragraph(p) => p.runs.iter().map(|r| r.text.as_str()).collect(),
            _ => "<非段落>".to_string(),
        }
    }

    // 把 JSON 转成 Rhai 普通字符串字面量
    fn lit(json: &str) -> String {
        format!("\"{}\"", json.replace('\\', "\\\\").replace('"', "\\\""))
    }

    fn chart_json() -> String {
        r#"{"kind":"Bar","title":null,"x_labels":["A","B"],"series":[{"name":"S1","data":[1.0,2.0]}]}"#.to_string()
    }

    #[test]
    fn insert_chart_positions() {
        let j = lit(&chart_json());
        // offset=0:插在段前
        let mut d = doc_with(&["hello world"]);
        assert_eq!(run(&mut d, &format!("insert_writer_chart(0, 0, {j}, 200.0, 100.0, false)")), r#"{"index":0}"#);
        assert_eq!(d.body.len(), 2);
        assert!(matches!(d.body[0], BodyElement::InlineChart { .. }));
        // 中间拆段
        let mut d = doc_with(&["hello world"]);
        assert_eq!(run(&mut d, &format!("insert_writer_chart(0, 5, {j}, 200.0, 100.0, false)")), r#"{"index":1}"#);
        assert_eq!(d.body.len(), 3);
        assert_eq!(ptext(&d, 0), "hello");
        match &d.body[1] {
            BodyElement::InlineChart { width_pt, height_pt, .. } => {
                assert_eq!(*width_pt, 200.0);
                assert_eq!(*height_pt, 100.0);
            }
            _ => panic!("不是图表"),
        }
        assert_eq!(ptext(&d, 2), " world");
        // 段尾:不新增空段
        let mut d = doc_with(&["hello"]);
        assert_eq!(run(&mut d, &format!("insert_writer_chart(0, 5, {j}, 50.0, 50.0, false)")), r#"{"index":1}"#);
        assert_eq!(d.body.len(), 2);
    }

    #[test]
    fn insert_chart_errors_leave_doc_unchanged() {
        let j = lit(&chart_json());
        let mut d = doc_with(&["你好"]);
        let n = d.body.len();
        let cases = vec![
            format!("insert_writer_chart(0, 0, {j}, 0.0, 100.0, false)"),
            format!("insert_writer_chart(0, 0, {j}, 100.0, -1.0, false)"),
            format!("insert_writer_chart(9, 0, {j}, 100.0, 100.0, false)"),
            format!("insert_writer_chart(-1, 0, {j}, 100.0, 100.0, false)"),
            format!("insert_writer_chart(0, -1, {j}, 100.0, 100.0, false)"),
            format!("insert_writer_chart(0, 99, {j}, 100.0, 100.0, false)"),
            format!("insert_writer_chart(0, 1, {j}, 100.0, 100.0, false)"),
            r#"insert_writer_chart(0, 0, "", 100.0, 100.0, false)"#.to_string(),
            r#"insert_writer_chart(0, 0, "not json", 100.0, 100.0, false)"#.to_string(),
            r#"insert_writer_chart(0, 0, "{}x", 100.0, 100.0, true)"#.to_string(),
        ];
        for sc in cases {
            let out = run(&mut d, &sc);
            assert!(out.starts_with("err:"), "{sc} => {out}");
        }
        assert_eq!(d.body.len(), n);
        assert_eq!(ptext(&d, 0), "你好");
    }
}


#[cfg(test)]
mod object_tests {
    use crate::engine::ScriptEngine;
    use roe_model::common::FillStyle;
    use roe_model::writer::{BodyElement, WriterDocument};

    fn run(doc: &mut WriterDocument, script: &str) -> String {
        let eng = ScriptEngine::new();
        match eng.run_writer(script, doc) {
            Ok(v) => v.as_str().map(|x| x.to_string()).unwrap_or_else(|| v.to_string()),
            Err(e) => format!("RUNERR:{e}"),
        }
    }

    fn doc_with(lines: &[&str]) -> WriterDocument {
        let mut d = WriterDocument::default();
        for l in lines {
            let out = run(&mut d, &format!("append_paragraph(\"{}\")", l));
            assert!(!out.starts_with("RUNERR"), "造文档失败: {out}");
        }
        d.body.retain(|b| matches!(b, BodyElement::Paragraph(p) if !p.runs.is_empty()));
        d
    }

    const CJ: &str = r#"{"kind":"Bar","title":"T1","x_labels":["A","B"],"series":[{"name":"S1","data":[1.0,2.0]}]}"#;

    #[test]
    fn delete_object_flow_and_errors() {
        let mut d = doc_with(&["a"]);
        assert_eq!(run(&mut d, r#"insert_writer_shape(1, "rect", 0.0, 0.0, 10.0, 10.0)"#), r#"{"index":1}"#);
        assert_eq!(run(&mut d, &format!("insert_writer_chart(0, 0, `{CJ}`, 100.0, 50.0, false)")), r#"{"index":0}"#);
        assert_eq!(d.body.len(), 3);
        assert_eq!(run(&mut d, "delete_writer_object(0)"), "ok");
        assert_eq!(run(&mut d, "delete_writer_object(1)"), "ok");
        assert_eq!(d.body.len(), 1);
        for sc in ["delete_writer_object(0)", "delete_writer_object(9)", "delete_writer_object(-1)"] {
            let out = run(&mut d, sc);
            assert!(out.starts_with("err:"), "{sc} => {out}");
        }
        assert_eq!(d.body.len(), 1);
    }

    #[test]
    fn shape_style_set_and_atomic_errors() {
        let mut d = doc_with(&["a"]);
        assert_eq!(run(&mut d, r#"insert_writer_shape(1, "rect", 0.0, 0.0, 10.0, 10.0)"#), r#"{"index":1}"#);
        assert_eq!(run(&mut d, r##"set_writer_shape_style(1, "#ff0000", "none", 2.0)"##), "ok");
        match &d.body[1] {
            BodyElement::Shape(sh) => {
                assert!(matches!(&sh.style.fill, Some(FillStyle::Solid(c)) if c.r == 255 && c.g == 0 && c.b == 0));
                assert_eq!(sh.style.stroke.as_ref().map(|s| s.width), Some(2.0)); // none 后 width>=0 重建边框,同内核
            }
            _ => panic!("不是形状"),
        }
        // fill 合法 + stroke 非法:整体不生效
        let out = run(&mut d, r##"set_writer_shape_style(1, "#00ff00", "bad", -1.0)"##);
        assert!(out.starts_with("err:"), "{out}");
        match &d.body[1] {
            BodyElement::Shape(sh) => assert!(matches!(&sh.style.fill, Some(FillStyle::Solid(c)) if c.r == 255)),
            _ => panic!(),
        }
        assert_eq!(run(&mut d, r##"set_writer_shape_style(1, "grad:#ff0000,#0000ff,90", "", -1.0)"##), "ok");
        match &d.body[1] {
            BodyElement::Shape(sh) => assert!(matches!(&sh.style.fill, Some(FillStyle::Gradient { .. }))),
            _ => panic!(),
        }
        for sc in [
            r#"set_writer_shape_style(0, "none", "", -1.0)"#,
            r#"set_writer_shape_style(9, "none", "", -1.0)"#,
            r#"set_writer_shape_style(-1, "none", "", -1.0)"#,
            r#"set_writer_shape_style(1, "grad:#ff0000", "", -1.0)"#,
        ] {
            let out = run(&mut d, sc);
            assert!(out.starts_with("err:"), "{sc} => {out}");
        }
    }

    #[test]
    fn chart_json_roundtrip_keeps_size() {
        let mut d = doc_with(&["a"]);
        assert_eq!(run(&mut d, &format!("insert_writer_chart(0, 0, `{CJ}`, 200.0, 100.0, false)")), r#"{"index":0}"#);
        let got = run(&mut d, "get_writer_chart_json(0)");
        let v: serde_json::Value = serde_json::from_str(&got).expect(&got);
        assert_eq!(v["title"], "T1");
        let new_json = CJ.replace("T1", "T2");
        assert_eq!(run(&mut d, &format!("set_writer_chart_json(0, `{new_json}`)")), "ok");
        let v2: serde_json::Value = serde_json::from_str(&run(&mut d, "get_writer_chart_json(0)")).unwrap();
        assert_eq!(v2["title"], "T2");
        assert_eq!(v2["width_pt"].as_f64(), Some(200.0));
        assert_eq!(v2["height_pt"].as_f64(), Some(100.0));
        for sc in [
            "get_writer_chart_json(1)".to_string(),
            "get_writer_chart_json(-1)".to_string(),
            "set_writer_chart_json(1, `{}`)".to_string(),
            "set_writer_chart_json(0, `not json`)".to_string(),
            "set_writer_chart_json(0, ``)".to_string(),
        ] {
            let out = run(&mut d, &sc);
            assert!(out.starts_with("err:"), "{sc} => {out}");
        }
        let v3: serde_json::Value = serde_json::from_str(&run(&mut d, "get_writer_chart_json(0)")).unwrap();
        assert_eq!(v3["title"], "T2");
    }
}


#[cfg(test)]
mod cmgr_tests {
    use crate::engine::ScriptEngine;
    use roe_model::writer::{BodyElement, WriterDocument};

    fn run(doc: &mut WriterDocument, script: &str) -> String {
        let eng = ScriptEngine::new();
        match eng.run_writer(script, doc) {
            Ok(v) => v.as_str().map(|x| x.to_string()).unwrap_or_else(|| v.to_string()),
            Err(e) => format!("RUNERR:{e}"),
        }
    }

    fn doc_with(lines: &[&str]) -> WriterDocument {
        let mut d = WriterDocument::default();
        for l in lines {
            let out = run(&mut d, &format!("append_paragraph(\"{}\")", l));
            assert!(!out.starts_with("RUNERR"), "造文档失败: {out}");
        }
        d.body.retain(|b| matches!(b, BodyElement::Paragraph(p) if !p.runs.is_empty()));
        d
    }

    fn threads(d: &mut WriterDocument) -> serde_json::Value {
        serde_json::from_str(&run(d, "get_comment_threads()")).unwrap()
    }

    #[test]
    fn comment_done_edit_delete() {
        let mut d = doc_with(&["hello world"]);
        assert_eq!(run(&mut d, r#"insert_comment(0, 0, 0, 5, "Tom", "root")"#), r#"{"id":0}"#);
        assert_eq!(run(&mut d, r#"reply_comment(0, "Amy", "re")"#), r#"{"id":1}"#);
        assert_eq!(run(&mut d, "set_comment_done(0, true)"), "ok");
        assert!(d.comments.iter().all(|c| c.done));
        assert_eq!(run(&mut d, "set_comment_done(0, false)"), "ok");
        assert!(d.comments.iter().all(|c| !c.done));
        assert_eq!(run(&mut d, r#"edit_comment(0, "new text")"#), "ok");
        assert_eq!(threads(&mut d)["threads"][0]["root"]["text"], "new text");
        assert_eq!(run(&mut d, "delete_comment(0)"), "ok");
        assert!(d.comments.is_empty());
        assert_eq!(threads(&mut d)["threads"].as_array().unwrap().len(), 0);
        if let BodyElement::Paragraph(p) = &d.body[0] {
            assert!(p.runs.iter().all(|r| r.comment_range_start.is_empty() && r.comment_range_end.is_empty() && r.comment_ref.is_none()));
            let t: String = p.runs.iter().map(|r| r.text.as_str()).collect();
            assert_eq!(t, "hello world");
        }
    }

    #[test]
    fn comment_mgr_errors() {
        let mut d = doc_with(&["hello"]);
        assert_eq!(run(&mut d, r#"insert_comment(0, 0, 0, 2, "T", "x")"#), r#"{"id":0}"#);
        let n = d.comments.len();
        for sc in [
            "set_comment_done(9, true)",
            "set_comment_done(-1, true)",
            r#"edit_comment(9, "x")"#,
            r#"edit_comment(0, "  ")"#,
            "delete_comment(9)",
            "delete_comment(-1)",
        ] {
            let out = run(&mut d, sc);
            assert!(out.starts_with("err:"), "{sc} => {out}");
        }
        assert_eq!(d.comments.len(), n);
    }

    #[test]
    fn delete_note_chinese_offsets_and_errors() {
        let mut d = doc_with(&["你好世界"]);
        assert!(run(&mut d, r#"insert_footnote(0, 3, 0, "n1")"#).starts_with(r#"{"id":"#));
        assert!(run(&mut d, r#"insert_footnote(0, 0, 1, "e1")"#).starts_with(r#"{"id":"#));
        // 偏移不在引用位置:报错且不改
        for sc in [
            r#"delete_note(0, 1, 0)"#,
            r#"delete_note(0, 0, 0)"#,
            r#"delete_note(9, 0, 0)"#,
            r#"delete_note(0, -1, 0)"#,
            r#"delete_note(0, 3, 2)"#,
        ] {
            let out = run(&mut d, sc);
            assert!(out.starts_with("err:"), "{sc} => {out}");
        }
        assert_eq!(d.footnotes.len(), 1);
        assert_eq!(d.endnotes.len(), 1);
        assert_eq!(run(&mut d, "delete_note(0, 3, 0)"), "ok");
        assert_eq!(d.footnotes.len(), 0);
        assert_eq!(d.endnotes.len(), 1);
        assert_eq!(run(&mut d, "delete_note(0, 0, 1)"), "ok");
        assert_eq!(d.endnotes.len(), 0);
        if let BodyElement::Paragraph(p) = &d.body[0] {
            let t: String = p.runs.iter().map(|r| r.text.as_str()).collect();
            assert_eq!(t, "你好世界");
            assert!(p.runs.iter().all(|r| r.footnote_ref.is_none() && r.endnote_ref.is_none()));
        }
    }
}

#[cfg(test)]
mod wfml_tests {
    use crate::engine::ScriptEngine;
    use roe_model::writer::{BodyElement, WriterDocument};

    fn run(doc: &mut WriterDocument, script: &str) -> String {
        let eng = ScriptEngine::new();
        match eng.run_writer(script, doc) {
            Ok(v) => v.as_str().map(|x| x.to_string()).unwrap_or_else(|| v.to_string()),
            Err(e) => format!("RUNERR:{e}"),
        }
    }

    fn doc_with(lines: &[&str]) -> WriterDocument {
        let mut d = WriterDocument::default();
        for l in lines {
            let out = run(&mut d, &format!("append_paragraph(\"{}\")", l));
            assert!(!out.starts_with("RUNERR"), "造文档失败: {out}");
        }
        d.body.retain(|b| matches!(b, BodyElement::Paragraph(p) if !p.runs.is_empty()));
        d
    }

    fn jv(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap_or_else(|_| panic!("不是 JSON: {s}"))
    }

    fn runs_len(d: &WriterDocument) -> usize {
        match &d.body[0] {
            BodyElement::Paragraph(p) => p.runs.len(),
            _ => 0,
        }
    }

    #[test]
    fn formula_insert_get_set() {
        let mut d = doc_with(&["hello"]);
        let r = jv(&run(&mut d, r#"insert_writer_formula(0, 5, "x^2")"#));
        assert_eq!(r["para_index"].as_i64().unwrap(), 0);
        let ri = r["run_idx"].as_i64().unwrap();
        assert_eq!(ri, 1);
        let g = jv(&run(&mut d, &format!("get_writer_formula(0, {ri})")));
        assert_eq!(g["latex"], "x^2");
        assert!(g["width_pt"].as_f64().unwrap() > 0.0);
        assert_eq!(run(&mut d, &format!(r#"set_writer_formula(0, {ri}, "\\frac{{a}}{{b}}")"#)), "ok");
        let g = jv(&run(&mut d, &format!("get_writer_formula(0, {ri})")));
        assert_eq!(g["latex"], "\\frac{a}{b}");
    }

    #[test]
    fn formula_insert_splits_run() {
        let mut d = doc_with(&["hello"]);
        let r = jv(&run(&mut d, r#"insert_writer_formula(0, 2, "a+b")"#));
        assert_eq!(r["run_idx"].as_i64().unwrap(), 1);
        assert_eq!(runs_len(&d), 3);
        assert_eq!(run(&mut d, "get_paragraph(0)"), "hello");
    }

    #[test]
    fn formula_errors_change_nothing() {
        let mut d = doc_with(&["hello"]);
        let before = runs_len(&d);
        for script in [
            r#"insert_writer_formula(0, 5, "")"#,
            r#"insert_writer_formula(0, 5, "   ")"#,
            r#"insert_writer_formula(0, 99, "x")"#,
            r#"insert_writer_formula(0, -1, "x")"#,
            r#"insert_writer_formula(99, 0, "x")"#,
            r#"insert_writer_formula(-1, 0, "x")"#,
            "get_writer_formula(0, 0)",
            "get_writer_formula(0, 99)",
            "get_writer_formula(99, 0)",
            "get_writer_formula(0, -1)",
            r#"set_writer_formula(0, 0, "x")"#,
            r#"set_writer_formula(0, 99, "x")"#,
            r#"set_writer_formula(99, 0, "x")"#,
        ] {
            let out = run(&mut d, script);
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
        assert_eq!(runs_len(&d), before, "失败的调用不应改动文档");
    }

    #[test]
    fn formula_set_empty_keeps_old() {
        let mut d = doc_with(&["hello"]);
        let r = jv(&run(&mut d, r#"insert_writer_formula(0, 5, "x^2")"#));
        let ri = r["run_idx"].as_i64().unwrap();
        let out = run(&mut d, &format!(r#"set_writer_formula(0, {ri}, "")"#));
        assert!(out.starts_with("err:"), "{out}");
        let g = jv(&run(&mut d, &format!("get_writer_formula(0, {ri})")));
        assert_eq!(g["latex"], "x^2");
    }
}
