//! Impress 形状文字编辑（文本框）。
//!
//! 编辑逻辑是作用在 Vec<Paragraph> 上的纯函数（便于单测），FFI 层只负责
//! 按 (slide_idx, shape_id) 找到形状的 ShapeContent::Text 后调用它们。
//! 偏移语义与 Writer 一致：段落内所有 run 文本拼接后的字节偏移。
//! 撤销由前台 begin_edit/commit_edit 事务负责，这里不处理。

use std::ffi::CString;
use std::os::raw::c_char;

use roe_model::writer::{Paragraph, Run};
use roe_parser::RoeDocument;

use crate::error::set_last_error;
use crate::handle;

pub(crate) fn para_len(p: &Paragraph) -> usize {
    p.runs.iter().map(|r| r.text.len()).sum()
}

/// 校验 offset 落在字符边界上（跨 run 累加）。
fn check_boundary(p: &Paragraph, offset: usize) -> Result<(), String> {
    let total = para_len(p);
    if offset > total {
        return Err(format!("offset={offset} 超出段落长度 {total}"));
    }
    let mut consumed = 0usize;
    for run in &p.runs {
        let len = run.text.len();
        if offset <= consumed + len {
            let o = offset - consumed;
            if !run.text.is_char_boundary(o) {
                return Err(format!("offset={offset} 不在字符边界上"));
            }
            return Ok(());
        }
        consumed += len;
    }
    Ok(())
}

pub fn insert_text(
    paras: &mut Vec<Paragraph>,
    para: usize,
    offset: usize,
    text: &str,
) -> Result<(usize, usize), String> {
    let p = paras.get_mut(para).ok_or_else(|| format!("para={para} 越界"))?;
    check_boundary(p, offset)?;
    if p.runs.is_empty() {
        p.runs.push(Run { text: text.to_string(), ..Default::default() });
        return Ok((para, text.len()));
    }
    let mut consumed = 0usize;
    for run in p.runs.iter_mut() {
        let len = run.text.len();
        if offset <= consumed + len {
            let o = offset - consumed;
            run.text.insert_str(o, text);
            return Ok((para, offset + text.len()));
        }
        consumed += len;
    }
    Err("未能定位到 run".to_string())
}

fn merge_with_prev(paras: &mut Vec<Paragraph>, idx: usize) -> Result<(usize, usize), String> {
    if idx == 0 || idx >= paras.len() {
        return Err("无法合并".to_string());
    }
    let cur = paras.remove(idx);
    let prev = &mut paras[idx - 1];
    let off = para_len(prev);
    prev.runs.extend(cur.runs);
    Ok((idx - 1, off))
}

/// backward=true：Backspace；false：Delete。返回删除后的光标位置。
pub fn delete_char(
    paras: &mut Vec<Paragraph>,
    para: usize,
    offset: usize,
    backward: bool,
) -> Result<(usize, usize), String> {
    if para >= paras.len() {
        return Err(format!("para={para} 越界"));
    }
    check_boundary(&paras[para], offset)?;
    let total = para_len(&paras[para]);
    if backward && offset == 0 {
        let hang_off = match &paras[para].bullet {
            Some(b) if !matches!(b.kind, roe_model::impress::BulletKind::None) => Some((b.mar_l.unwrap_or(0.0), b.indent.map_or(0.0, |i| (-i).max(0.0)))),
            _ => None,
        };
        if let Some((m, hang)) = hang_off {
            let p = &mut paras[para];
            p.bullet_from_master = false;
            p.bullet = Some(roe_model::impress::BulletInfo { kind: roe_model::impress::BulletKind::None, mar_l: Some((m - hang).max(0.0)), indent: None, color: None });
            return Ok((para, 0));
        }
        if para == 0 {
            return Err("已在文字开头".to_string());
        }
        return merge_with_prev(paras, para);
    }
    if !backward && offset == total {
        if para + 1 >= paras.len() {
            return Err("已在文字末尾".to_string());
        }
        merge_with_prev(paras, para + 1)?;
        return Ok((para, offset));
    }
    let p = &mut paras[para];
    let mut consumed = 0usize;
    let mut new_off = offset;
    let mut done = false;
    for run in p.runs.iter_mut() {
        let len = run.text.len();
        if backward {
            if consumed < offset && offset <= consumed + len {
                let o = offset - consumed;
                let ch = run.text[..o].chars().next_back().ok_or("无字符可删")?;
                let cl = ch.len_utf8();
                run.text.drain((o - cl)..o);
                new_off = offset - cl;
                done = true;
                break;
            }
        } else if consumed <= offset && offset < consumed + len {
            let o = offset - consumed;
            let ch = run.text[o..].chars().next().ok_or("无字符可删")?;
            run.text.drain(o..(o + ch.len_utf8()));
            done = true;
            break;
        }
        consumed += len;
    }
    if !done {
        return Err("未能定位到要删除的字符".to_string());
    }
    p.runs.retain(|r| !r.text.is_empty());
    Ok((para, new_off))
}

/// 在光标处拆段落，返回新段落开头 (para+1, 0)。
pub fn split_paragraph(
    paras: &mut Vec<Paragraph>,
    para: usize,
    offset: usize,
) -> Result<(usize, usize), String> {
    let p = paras.get(para).ok_or_else(|| format!("para={para} 越界"))?;
    check_boundary(p, offset)?;
    let mut new_p = p.clone();
    new_p.runs.clear();

    let p = &mut paras[para];
    let old = std::mem::take(&mut p.runs);
    let mut tail: Vec<Run> = Vec::new();
    let mut consumed = 0usize;
    for run in old {
        let len = run.text.len();
        if consumed + len <= offset {
            p.runs.push(run);
        } else if consumed >= offset {
            tail.push(run);
        } else {
            let o = offset - consumed;
            let mut left = run.clone();
            left.text.truncate(o);
            let mut right = run;
            right.text = right.text[o..].to_string();
            p.runs.push(left);
            tail.push(right);
        }
        consumed += len;
    }
    new_p.runs = tail;
    paras.insert(para + 1, new_p);
    Ok((para + 1, 0))
}

// ───────────────────────── FFI ─────────────────────────

fn edit_paras<F>(handle: u64, slide_idx: u32, shape_id: u32, f: F) -> Result<(usize, usize), String>
where
    F: Fn(&mut Vec<Paragraph>) -> Result<(usize, usize), String>,
{
    let r = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("非 Impress 文档".to_string());
        };
        let slide = pres
            .slides
            .get_mut(slide_idx as usize)
            .ok_or_else(|| format!("slide_idx={slide_idx} 越界"))?;
        let shape = slide
            .shapes
            .iter_mut()
            .find(|s| s.id == shape_id)
            .ok_or_else(|| format!("shape_id={shape_id} 不存在"))?;
        match &mut shape.content {
            roe_model::impress::ShapeContent::Text(paras) => f(paras),
            _ => Err("该形状不是文字形状".to_string()),
        }
    });
    match r {
        None => Err("无效句柄".to_string()),
        Some(v) => v,
    }
}

fn to_json_ptr(name: &str, res: Result<(usize, usize), String>) -> *mut c_char {
    match res {
        Ok((p, o)) => match CString::new(format!("{{\"para\":{},\"offset\":{}}}", p, o)) {
            Ok(cs) => cs.into_raw(),
            Err(e) => {
                set_last_error(format!("{name}: {e}"));
                std::ptr::null_mut()
            }
        },
        Err(msg) => {
            set_last_error(format!("{name}: {msg}"));
            std::ptr::null_mut()
        }
    }
}


/// Insert text; when the shape is a placeholder with no text yet, the first
/// run inherits the placeholder hint style (size, color, font, alignment).
fn insert_with_hint(handle: u64, slide_idx: u32, shape_id: u32, para: usize, offset: usize, text: &str) -> Result<(usize, usize), String> {
    let r = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("not impress".to_string());
        };
        let slide = pres.slides.get_mut(slide_idx as usize).ok_or_else(|| format!("slide_idx={slide_idx} out of range"))?;
        let shape = slide.shapes.iter_mut().find(|s| s.id == shape_id).ok_or_else(|| format!("shape_id={shape_id} not found"))?;
        let hint = shape.placeholder.as_ref().and_then(|p| p.hint_style.clone());
        match &mut shape.content {
            roe_model::impress::ShapeContent::Text(paras) => {
                let was_empty = paras.iter().all(|p| para_len(p) == 0);
                if paras.is_empty() { paras.push(Paragraph::default()); }
                let res = insert_text(paras, para, offset, text)?;
                if was_empty {
                    if let Some(h) = hint {
                        if let Some(p) = paras.get_mut(res.0) {
                            if p.alignment.is_none() { p.alignment = h.algn; }
                            if let Some(run) = p.runs.first_mut() {
                                if run.font_size.is_none() { run.font_size = h.font_size; }
                                if run.bold.is_none() { run.bold = h.bold; }
                                if run.italic.is_none() { run.italic = h.italic; }
                                if run.color.is_none() { run.color = h.color; }
                                if run.font_family.is_none() { run.font_family = h.font_family.clone(); }
                            }
                        }
                    }
                }
                Ok(res)
            }
            _ => Err("not a text shape".to_string()),
        }
    });
    match r {
        None => Err("invalid handle".to_string()),
        Some(v) => v,
    }
}

#[no_mangle]
pub extern "C" fn roe_impress_text_insert(
    handle: u64,
    slide_idx: u32,
    shape_id: u32,
    para: u32,
    offset: u32,
    text_utf8: *const u8,
    text_len: u32,
) -> *mut c_char {
    if text_utf8.is_null() || text_len == 0 {
        set_last_error("roe_impress_text_insert: 文本为空".to_string());
        return std::ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(text_utf8, text_len as usize) };
    let text = match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(e) => {
            set_last_error(format!("roe_impress_text_insert: 不是合法UTF-8: {e}"));
            return std::ptr::null_mut();
        }
    };
    let res = insert_with_hint(handle, slide_idx, shape_id, para as usize, offset as usize, &text);
    to_json_ptr("roe_impress_text_insert", res)
}

/// direction: 0=Backspace，1=Delete
#[no_mangle]
pub extern "C" fn roe_impress_text_delete(
    handle: u64,
    slide_idx: u32,
    shape_id: u32,
    para: u32,
    offset: u32,
    direction: u32,
) -> *mut c_char {
    let backward = direction == 0;
    let res = edit_paras(handle, slide_idx, shape_id, |ps| {
        delete_char(ps, para as usize, offset as usize, backward)
    });
    to_json_ptr("roe_impress_text_delete", res)
}

/// 形状文字是否为空：1=空，0=非空，-1=失败（形状不存在/非文字形状/句柄无效）。
#[no_mangle]
pub extern "C" fn roe_impress_text_is_empty(handle: u64, slide_idx: u32, shape_id: u32) -> i32 {
    let r = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &entry.doc else {
            return Err("非 Impress 文档".to_string());
        };
        let slide = pres
            .slides
            .get(slide_idx as usize)
            .ok_or_else(|| format!("slide_idx={slide_idx} 越界"))?;
        let shape = slide
            .shapes
            .iter()
            .find(|s| s.id == shape_id)
            .ok_or_else(|| format!("shape_id={shape_id} 不存在"))?;
        match &shape.content {
            roe_model::impress::ShapeContent::Text(paras) => {
                Ok(paras.iter().all(|p| para_len(p) == 0))
            }
            _ => Err("该形状不是文字形状".to_string()),
        }
    });
    match r {
        None => {
            set_last_error("roe_impress_text_is_empty: 无效句柄".to_string());
            -1
        }
        Some(Ok(true)) => 1,
        Some(Ok(false)) => 0,
        Some(Err(msg)) => {
            set_last_error(format!("roe_impress_text_is_empty: {msg}"));
            -1
        }
    }
}

/// 光标按字符移动：dir 0=左 1=右。段首左移到上段末尾，段尾右移到下段开头。
#[no_mangle]
pub extern "C" fn roe_impress_text_move(handle: u64, slide_idx: u32, shape_id: u32, para: u32, offset: u32, dir: u32) -> *mut c_char {
    let r = edit_paras(handle, slide_idx, shape_id, |paras| {
        let para = para as usize;
        let offset = offset as usize;
        let p = paras.get(para).ok_or_else(|| format!("para={para} 越界"))?;
        check_boundary(p, offset)?;
        let text: String = p.runs.iter().map(|r| r.text.as_str()).collect();
        if dir == 0 {
            if offset > 0 {
                let n = text[..offset].chars().next_back().map(|c| c.len_utf8()).unwrap_or(0);
                Ok((para, offset - n))
            } else if para > 0 {
                Ok((para - 1, para_len(&paras[para - 1])))
            } else {
                Ok((para, offset))
            }
        } else if offset < text.len() {
            let n = text[offset..].chars().next().map(|c| c.len_utf8()).unwrap_or(0);
            Ok((para, offset + n))
        } else if para + 1 < paras.len() {
            Ok((para + 1, 0))
        } else {
            Ok((para, offset))
        }
    });
    to_json_ptr("roe_impress_text_move", r)
}

#[no_mangle]
pub extern "C" fn roe_impress_text_split(
    handle: u64,
    slide_idx: u32,
    shape_id: u32,
    para: u32,
    offset: u32,
) -> *mut c_char {
    let res = edit_paras(handle, slide_idx, shape_id, |ps| {
        split_paragraph(ps, para as usize, offset as usize)
    });
    to_json_ptr("roe_impress_text_split", res)
}

/// 新建文本框：无填充、无边框的 Rect，带一个空段落。
/// vertical=true 时 bodyPr vert=eaVert（竖排）。成功返回新形状 id，失败 -1。
#[no_mangle]
pub extern "C" fn roe_impress_insert_textbox(
    handle: u64,
    slide_idx: u32,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    vertical: bool,
) -> i32 {
    use roe_model::impress::{
        Shape, ShapeContent, ShapeGeometry, TextAnchor, TextAutofit, TextBodyProps, TextVert,
    };
    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("非 Impress 文档".to_string());
        };
        let slide = pres
            .slides
            .get_mut(slide_idx as usize)
            .ok_or_else(|| format!("slide_idx={slide_idx} 越界"))?;
        let next_id = slide.shapes.iter().map(|sh| sh.id).max().unwrap_or(0) + 1;
        slide.shapes.push(Shape {
            id: next_id,
            geometry: ShapeGeometry::Rect,
            position: roe_model::common::Transform::from_rect(x, y, w, h),
            style: roe_model::common::ShapeStyle { fill: Some(roe_model::common::FillStyle::None), stroke: None, ..Default::default() },
            content: ShapeContent::Text(vec![Paragraph::default()]),
            text_body: TextBodyProps {
                autofit: TextAutofit::NoAutofit,
                wrap: true,
                anchor: TextAnchor::Top,
                ins_l: None,
                ins_r: None,
                ins_t: None,
                ins_b: None,
                vert: if vertical { TextVert::EaVert } else { TextVert::Horizontal },
            },
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        });
        Ok(next_id)
    });
    match result {
        None => {
            set_last_error("roe_impress_insert_textbox: 无效句柄".to_string());
            -1
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_impress_insert_textbox: {msg}"));
            -1
        }
        Some(Ok(id)) => id as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Paragraph {
        let mut para = Paragraph::default();
        if !s.is_empty() {
            para.runs.push(Run { text: s.to_string(), ..Default::default() });
        }
        para
    }
    fn text(ps: &[Paragraph], i: usize) -> String {
        ps[i].runs.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn insert_into_empty_paragraph() {
        let mut ps = vec![p("")];
        assert_eq!(insert_text(&mut ps, 0, 0, "你好").unwrap(), (0, 6));
        assert_eq!(text(&ps, 0), "你好");
    }

    #[test]
    fn insert_in_middle_and_reject_bad_boundary() {
        let mut ps = vec![p("你好")];
        assert_eq!(insert_text(&mut ps, 0, 3, "a").unwrap(), (0, 4));
        assert_eq!(text(&ps, 0), "你a好");
        assert!(insert_text(&mut ps, 0, 1, "x").is_err());
        assert!(insert_text(&mut ps, 0, 99, "x").is_err());
    }

    #[test]
    fn backspace_and_delete_chars() {
        let mut ps = vec![p("你好吗")];
        assert_eq!(delete_char(&mut ps, 0, 9, true).unwrap(), (0, 6));
        assert_eq!(text(&ps, 0), "你好");
        assert_eq!(delete_char(&mut ps, 0, 0, false).unwrap(), (0, 0));
        assert_eq!(text(&ps, 0), "好");
    }

    #[test]
    fn split_then_merge_roundtrip() {
        let mut ps = vec![p("你好世界")];
        assert_eq!(split_paragraph(&mut ps, 0, 6).unwrap(), (1, 0));
        assert_eq!(ps.len(), 2);
        assert_eq!(text(&ps, 0), "你好");
        assert_eq!(text(&ps, 1), "世界");
        assert_eq!(delete_char(&mut ps, 1, 0, true).unwrap(), (0, 6));
        assert_eq!(ps.len(), 1);
        assert_eq!(text(&ps, 0), "你好世界");
    }

    #[test]
    fn delete_forward_at_end_merges_next() {
        let mut ps = vec![p("ab"), p("cd")];
        assert_eq!(delete_char(&mut ps, 0, 2, false).unwrap(), (0, 2));
        assert_eq!(text(&ps, 0), "abcd");
        assert!(delete_char(&mut ps, 0, 4, false).is_err());
        assert!(delete_char(&mut ps, 0, 0, true).is_err());
    }

    #[test]
    fn split_at_start_and_end_gives_empty_paragraph() {
        let mut ps = vec![p("ab")];
        split_paragraph(&mut ps, 0, 2).unwrap();
        assert_eq!(text(&ps, 1), "");
        split_paragraph(&mut ps, 0, 0).unwrap();
        assert_eq!(text(&ps, 0), "");
        assert_eq!(text(&ps, 1), "ab");
    }
}


// ───────── v216: 光标定位接口（slide 坐标 pt；已处理形状旋转/翻转） ─────────
fn local_to_slide(sh: &roe_model::impress::Shape, lx: f32, ly: f32) -> (f32, f32) {
    let (x, y, w, h) = (sh.position.x(), sh.position.y(), sh.position.w(), sh.position.h());
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let (mut px, mut py) = (x + lx, y + ly);
    if sh.position.flip_h.unwrap_or(false) { px = 2.0 * cx - px; }
    if sh.position.flip_v.unwrap_or(false) { py = 2.0 * cy - py; }
    let rad = sh.position.rotation.unwrap_or(0.0).to_radians();
    let (dx, dy) = (px - cx, py - cy);
    (cx + dx * rad.cos() - dy * rad.sin(), cy + dx * rad.sin() + dy * rad.cos())
}

fn slide_to_local(sh: &roe_model::impress::Shape, sx: f32, sy: f32) -> (f32, f32) {
    let (x, y, w, h) = (sh.position.x(), sh.position.y(), sh.position.w(), sh.position.h());
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let rad = -sh.position.rotation.unwrap_or(0.0).to_radians();
    let (dx, dy) = (sx - cx, sy - cy);
    let (mut px, mut py) = (cx + dx * rad.cos() - dy * rad.sin(), cy + dx * rad.sin() + dy * rad.cos());
    if sh.position.flip_h.unwrap_or(false) { px = 2.0 * cx - px; }
    if sh.position.flip_v.unwrap_or(false) { py = 2.0 * cy - py; }
    (px - x, py - y)
}

/// v219: 竖排框 -> 横排虚拟框（宽高对调，内边距映射与渲染器一致）。返回 (虚拟框, 是否顺时针90度)。
fn vert_virtual(sh: &roe_model::impress::Shape) -> Option<(roe_model::impress::Shape, bool)> {
    use roe_model::impress::TextVert;
    let tb = &sh.text_body;
    let (cw, (l, t, r, b)) = match tb.vert {
        TextVert::Horizontal => return None,
        TextVert::Vert => (true, (tb.ins_b, tb.ins_l, tb.ins_t, tb.ins_r)),
        TextVert::Vert270 => (false, (tb.ins_t, tb.ins_r, tb.ins_b, tb.ins_l)),
        TextVert::EaVert => (true, (tb.ins_t, tb.ins_r, tb.ins_b, tb.ins_l)),
    };
    let mut v = sh.clone();
    v.position = roe_model::common::Transform::from_rect(0.0, 0.0, sh.position.h(), sh.position.w());
    v.text_body.vert = TextVert::Horizontal;
    v.text_body.ins_l = l;
    v.text_body.ins_t = t;
    v.text_body.ins_r = r;
    v.text_body.ins_b = b;
    Some((v, cw))
}

/// 虚拟横排框内坐标 -> 原形状局部坐标（w,h 为原形状宽高）
fn virt_to_local(w: f32, h: f32, cw: bool, lx: f32, ly: f32) -> (f32, f32) {
    if cw { (w - ly, lx) } else { (ly, h - lx) }
}

/// 原形状局部坐标 -> 虚拟横排框内坐标
fn local_to_virt(w: f32, h: f32, cw: bool, px: f32, py: f32) -> (f32, f32) {
    if cw { (py, w - px) } else { (h - py, px) }
}

#[cfg(test)]
mod vert_map_tests {
    use super::*;
    #[test]
    fn vert_map_roundtrip() {
        for cw in [true, false] {
            let (px, py) = virt_to_local(100.0, 300.0, cw, 12.0, 34.0);
            let (lx, ly) = local_to_virt(100.0, 300.0, cw, px, py);
            assert!((lx - 12.0).abs() < 1e-4 && (ly - 34.0).abs() < 1e-4);
        }
        // 顺时针：虚拟左上角 -> 原框右上角
        assert_eq!(virt_to_local(100.0, 300.0, true, 0.0, 0.0), (100.0, 0.0));
    }
}

fn with_text_renderer<F>(handle: u64, slide_idx: u32, shape_id: u32, f: F) -> Result<String, String>
where
    F: FnOnce(&mut roe_impress::slide_renderer::SlideRenderer, &roe_model::impress::Shape, &roe_model::style::StyleRegistry) -> Result<String, String>,
{
    let r = handle::with_entry(handle, |entry| {
        let fc = entry.impress_font_registry.take().unwrap_or_else(parley::FontContext::new);
        let mut renderer = roe_impress::slide_renderer::SlideRenderer::new_with_fonts(1.0, 1.0, fc);
        let res = (|| {
            let RoeDocument::Impress(pres) = &entry.doc else { return Err("非 Impress 文档".to_string()); };
            let slide = pres.slides.get(slide_idx as usize).ok_or_else(|| format!("slide_idx={slide_idx} 越界"))?;
            let shape = slide.shapes.iter().find(|s| s.id == shape_id).ok_or_else(|| format!("shape_id={shape_id} 不存在"))?;
            f(&mut renderer, shape, &pres.styles)
        })();
        entry.impress_font_registry = Some(renderer.take_fonts());
        res
    });
    match r { None => Err("无效句柄".to_string()), Some(v) => v }
}

fn str_to_ptr(name: &str, res: Result<String, String>) -> *mut c_char {
    match res {
        Ok(s) => match CString::new(s) {
            Ok(cs) => cs.into_raw(),
            Err(e) => { set_last_error(format!("{name}: {e}")); std::ptr::null_mut() }
        },
        Err(m) => { set_last_error(format!("{name}: {m}")); std::ptr::null_mut() }
    }
}

/// v320o: 空占位符的光标定位用形状：把提示文字当成真实段落（与渲染层一致），
/// 这样光标会贴在提示文字的最左侧（含居中/居右/缩进/垂直锚点）。非空占位符返回 None。
fn v320o_hint_shape(sh: &roe_model::impress::Shape) -> Option<roe_model::impress::Shape> {
    let ph = sh.placeholder.as_ref()?;
    if !roe_impress::placeholder_hints::is_placeholder_empty(sh) { return None; }
    let text = roe_impress::placeholder_hints::hint_text_for_ph_type(ph.ph_type.as_deref());
    let hs = ph.hint_style.as_ref();
    let run = roe_model::writer::Run {
        text: text.to_string(),
        font_size: hs.and_then(|s| s.font_size),
        bold: hs.and_then(|s| s.bold),
        italic: hs.and_then(|s| s.italic),
        font_family: hs.and_then(|s| s.font_family.clone()),
        ..Default::default()
    };
    let para = roe_model::writer::Paragraph {
        runs: vec![run],
        alignment: hs.and_then(|s| s.algn),
        ..Default::default()
    };
    let mut c = sh.clone();
    c.content = roe_model::impress::ShapeContent::Text(vec![para]);
    if let Some(a) = hs.and_then(|s| s.anchor.clone()) { c.text_body.anchor = a; }
    Some(c)
}

/// 返回光标线段两端点（slide pt）：{"x1":..,"y1":..,"x2":..,"y2":..}。竖排暂不支持（返回 null）。
#[no_mangle]
pub extern "C" fn roe_impress_text_caret(handle: u64, slide_idx: u32, shape_id: u32, para: u32, offset: u32) -> *mut c_char {
    let res = with_text_renderer(handle, slide_idx, shape_id, |r, sh, styles| {
        let (w, h) = (sh.position.w(), sh.position.h());
        let virt = vert_virtual(sh);
        let hint_owned = if para == 0 && offset == 0 && virt.is_none() { v320o_hint_shape(sh) } else { None };
        let base: &roe_model::impress::Shape = hint_owned.as_ref().unwrap_or(sh);
        let target = virt.as_ref().map(|(v, _)| v).unwrap_or(base);
        let conv = |a: f32, b: f32| match &virt { Some((_, cw)) => virt_to_local(w, h, *cw, a, b), None => (a, b) };
        let (lx, ty, by) = r.text_caret_local(target, styles, para as usize, offset as usize)
            .ok_or("该形状不支持光标定位（竖排或非文字）")?;
        let (ax1, ay1) = conv(lx, ty);
        let (x1, y1) = local_to_slide(sh, ax1, ay1);
        let (ax2, ay2) = conv(lx, by);
        let (x2, y2) = local_to_slide(sh, ax2, ay2);
        Ok(format!("{{\"x1\":{x1},\"y1\":{y1},\"x2\":{x2},\"y2\":{y2}}}"))
    });
    str_to_ptr("roe_impress_text_caret", res)
}

/// 点击点（slide pt）-> {"para":N,"offset":N}
#[no_mangle]
pub extern "C" fn roe_impress_text_hit(handle: u64, slide_idx: u32, shape_id: u32, x_pt: f32, y_pt: f32) -> *mut c_char {
    let res = with_text_renderer(handle, slide_idx, shape_id, |r, sh, styles| {
        let (px, py) = slide_to_local(sh, x_pt, y_pt);
        let virt = vert_virtual(sh);
        let target = virt.as_ref().map(|(v, _)| v).unwrap_or(sh);
        let (lx, ly) = match &virt { Some((_, cw)) => local_to_virt(sh.position.w(), sh.position.h(), *cw, px, py), None => (px, py) };
        let (p, o) = r.text_hit_local(target, styles, lx, ly).ok_or("该形状不支持点击定位（竖排或非文字）")?;
        Ok(format!("{{\"para\":{p},\"offset\":{o}}}"))
    });
    str_to_ptr("roe_impress_text_hit", res)
}

#[cfg(test)]
mod caret_ffi_tests {
    use super::*;
    #[test]
    fn rotate_roundtrip() {
        let mut sh = roe_model::impress::Shape {
            id: 1,
            geometry: roe_model::impress::ShapeGeometry::Rect,
            position: roe_model::common::Transform::from_rect(10.0, 20.0, 200.0, 80.0),
            style: Default::default(),
            content: roe_model::impress::ShapeContent::Text(vec![]),
            text_body: Default::default(),
            placeholder: None, st_cxn: None, end_cxn: None,
        };
        sh.position.rotation = Some(33.0);
        sh.position.flip_h = Some(true);
        let (sx, sy) = local_to_slide(&sh, 40.0, 15.0);
        let (lx, ly) = slide_to_local(&sh, sx, sy);
        assert!((lx - 40.0).abs() < 1e-3 && (ly - 15.0).abs() < 1e-3, "{lx} {ly}");
    }
}

/// v318s：选区高亮矩形（slide pt）：[{"x":..,"y":..,"w":..,"h":..}, ...]。
/// 按字符边界取光标位置，同一行合并成一个矩形；长选区会抽样。竖排暂不支持。
#[no_mangle]
pub extern "C" fn roe_impress_text_sel_rects(
    handle: u64,
    slide_idx: u32,
    shape_id: u32,
    p1: u32,
    o1: u32,
    p2: u32,
    o2: u32,
) -> *mut c_char {
    let res = with_text_renderer(handle, slide_idx, shape_id, |r, sh, styles| {
        if vert_virtual(sh).is_some() {
            return Err("竖排暂不支持选区".to_string());
        }
        let paras = match &sh.content {
            roe_model::impress::ShapeContent::Text(ps) => ps,
            _ => return Err("该形状没有文字".to_string()),
        };
        let a = (p1 as usize, o1 as usize);
        let b = (p2 as usize, o2 as usize);
        let (s, e) = if a <= b { (a, b) } else { (b, a) };
        let mut out: Vec<String> = Vec::new();
        for pi in s.0..=e.0 {
            let Some(p) = paras.get(pi) else { break };
            let text: String = p.runs.iter().map(|r| r.text.as_str()).collect();
            let from = if pi == s.0 { s.1 } else { 0 };
            let to = if pi == e.0 { e.1.min(text.len()) } else { text.len() };
            if from > to { continue; }
            let mut offs = vec![from];
            for (i, _) in text.char_indices() {
                if i > from && i < to { offs.push(i); }
            }
            if to > from { offs.push(to); }
            let step = (offs.len() / 150).max(1);
            let last = offs.len().saturating_sub(1);
            let mut lines: Vec<(i64, f32, f32, f32, f32)> = Vec::new();
            for (idx, &o) in offs.iter().enumerate() {
                if idx % step != 0 && idx != last { continue; }
                if let Some((lx, ty, by)) = r.text_caret_local(sh, styles, pi, o) {
                    let key = (ty * 2.0).round() as i64;
                    match lines.iter_mut().find(|l| l.0 == key) {
                        Some(l) => { l.1 = l.1.min(lx); l.2 = l.2.max(lx); }
                        None => lines.push((key, lx, lx, ty, by)),
                    }
                }
            }
            for (_, minx, maxx, ty, by) in lines {
                if maxx - minx < 0.01 { continue; }
                let (ax, ay) = local_to_slide(sh, minx, ty);
                let (bx, by2) = local_to_slide(sh, maxx, by);
                out.push(format!(
                    "{{\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}",
                    ax.min(bx), ay.min(by2), (bx - ax).abs(), (by2 - ay).abs()
                ));
            }
        }
        Ok(format!("[{}]", out.join(",")))
    });
    str_to_ptr("roe_impress_text_sel_rects", res)
}


// ---- v319o: in-app text clipboard (formatted fragments) ----
thread_local! {
    static TEXT_CLIP: std::cell::RefCell<Vec<Paragraph>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Cut paragraphs [p1:o1, p2:o2] (p1:o1 <= p2:o2) into standalone fragments.
pub fn slice_paras(paras: &[Paragraph], p1: usize, o1: usize, p2: usize, o2: usize) -> Result<Vec<Paragraph>, String> {
    if p1 >= paras.len() || p2 >= paras.len() || (p1, o1) > (p2, o2) {
        return Err("bad range".to_string());
    }
    let mut out: Vec<Paragraph> = Vec::new();
    for i in p1..=p2 {
        let src = &paras[i];
        let from = if i == p1 { o1 } else { 0 };
        let to = if i == p2 { o2 } else { para_len(src) };
        let mut np = src.clone();
        np.runs.clear();
        let mut consumed = 0usize;
        for run in &src.runs {
            let len = run.text.len();
            let s = from.max(consumed);
            let e = to.min(consumed + len);
            if s < e {
                let mut r = run.clone();
                r.text = run.text[(s - consumed)..(e - consumed)].to_string();
                np.runs.push(r);
            }
            consumed += len;
        }
        out.push(np);
    }
    Ok(out)
}

/// Insert fragments at (para, offset); returns the caret after the pasted text.
pub fn paste_paras(paras: &mut Vec<Paragraph>, para: usize, offset: usize, frags: &[Paragraph]) -> Result<(usize, usize), String> {
    if frags.is_empty() { return Err("empty clipboard".to_string()); }
    if para >= paras.len() { return Err(format!("para={para} out of range")); }
    check_boundary(&paras[para], offset)?;
    if frags.len() == 1 {
        let add = para_len(&frags[0]);
        let (p, o) = split_paragraph(paras, para, offset)?;
        let _ = (p, o);
        let tail = paras.remove(para + 1);
        paras[para].runs.extend(frags[0].runs.clone());
        paras[para].runs.extend(tail.runs);
        return Ok((para, offset + add));
    }
    split_paragraph(paras, para, offset)?;
    let tail = paras.remove(para + 1);
    paras[para].runs.extend(frags[0].runs.clone());
    let last = frags.len() - 1;
    let mut ins: Vec<Paragraph> = Vec::new();
    for f in &frags[1..last] { ins.push(f.clone()); }
    let mut lastp = frags[last].clone();
    let caret = para_len(&lastp);
    lastp.runs.extend(tail.runs);
    ins.push(lastp);
    let n = ins.len();
    for (k, p) in ins.into_iter().enumerate() { paras.insert(para + 1 + k, p); }
    Ok((para + n, caret))
}

fn read_range_paras<R>(handle: u64, slide_idx: u32, shape_id: u32, f: impl Fn(&Vec<Paragraph>) -> Result<R, String>) -> Result<R, String> {
    let r = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &entry.doc else { return Err("not impress".to_string()); };
        let slide = pres.slides.get(slide_idx as usize).ok_or_else(|| "slide out of range".to_string())?;
        let shape = slide.shapes.iter().find(|s| s.id == shape_id).ok_or_else(|| "shape missing".to_string())?;
        match &shape.content {
            roe_model::impress::ShapeContent::Text(ps) => f(ps),
            _ => Err("not a text shape".to_string()),
        }
    });
    match r { None => Err("bad handle".to_string()), Some(v) => v }
}

/// Copy range into the in-app clipboard. Returns 0 ok, -1 fail.
#[no_mangle]
pub extern "C" fn roe_impress_text_copy(handle: u64, slide_idx: u32, shape_id: u32, p1: u32, o1: u32, p2: u32, o2: u32) -> i32 {
    let res = read_range_paras(handle, slide_idx, shape_id, |ps| slice_paras(ps, p1 as usize, o1 as usize, p2 as usize, o2 as usize));
    match res {
        Ok(frags) => { TEXT_CLIP.with(|c| *c.borrow_mut() = frags); 0 }
        Err(e) => { set_last_error(format!("roe_impress_text_copy: {e}")); -1 }
    }
}

#[no_mangle]
pub extern "C" fn roe_impress_text_paste(handle: u64, slide_idx: u32, shape_id: u32, para: u32, offset: u32) -> *mut c_char {
    let frags: Vec<Paragraph> = TEXT_CLIP.with(|c| c.borrow().clone());
    let res = edit_paras(handle, slide_idx, shape_id, |ps| paste_paras(ps, para as usize, offset as usize, &frags));
    to_json_ptr("roe_impress_text_paste", res)
}

/// End of text: {"para","offset"} of the last paragraph end.
#[no_mangle]
pub extern "C" fn roe_impress_text_extent(handle: u64, slide_idx: u32, shape_id: u32) -> *mut c_char {
    let res = read_range_paras(handle, slide_idx, shape_id, |ps| {
        if ps.is_empty() { return Err("empty".to_string()); }
        Ok((ps.len() - 1, para_len(&ps[ps.len() - 1])))
    });
    to_json_ptr("roe_impress_text_extent", res)
}

#[cfg(test)]
mod clip_tests_v319o {
    use super::*;
    fn p(s: &str) -> Paragraph { Paragraph { runs: vec![Run { text: s.to_string(), ..Default::default() }], ..Default::default() } }
    fn t(ps: &[Paragraph], i: usize) -> String { ps[i].runs.iter().map(|r| r.text.as_str()).collect() }
    #[test]
    fn slice_and_paste() {
        let ps = vec![p("hello"), p("world")];
        let frags = slice_paras(&ps, 0, 2, 1, 3).unwrap();
        assert_eq!(t(&frags, 0), "llo");
        assert_eq!(t(&frags, 1), "wor");
        let mut dst = vec![p("abXcd")];
        let caret = paste_paras(&mut dst, 0, 2, &frags).unwrap();
        assert_eq!(dst.len(), 2);
        assert_eq!(t(&dst, 0), "abllo");
        assert_eq!(t(&dst, 1), "worXcd");
        assert_eq!(caret, (1, 3));
        let mut one = vec![p("12")];
        let c = paste_paras(&mut one, 0, 1, &[p("zz")]).unwrap();
        assert_eq!(t(&one, 0), "1zz2");
        assert_eq!(c, (0, 3));
    }
}


// v319s: table cell hit / caret for Impress tables (cell = virtual text box).
/// Slide point -> {"row":N,"col":N}
#[no_mangle]
pub extern "C" fn roe_impress_cell_at(handle: u64, slide_idx: u32, shape_id: u32, x_pt: f32, y_pt: f32) -> *mut c_char {
    let res = with_text_renderer(handle, slide_idx, shape_id, |r, sh, _styles| {
        let (px, py) = slide_to_local(sh, x_pt, y_pt);
        let (row, col) = r.table_cell_at_local(sh, px, py).ok_or("point is not inside a table cell")?;
        Ok(format!("{{\"row\":{row},\"col\":{col}}}"))
    });
    str_to_ptr("roe_impress_cell_at", res)
}

/// Slide point inside a cell -> {"para":0,"offset":N}
#[no_mangle]
pub extern "C" fn roe_impress_cell_hit(handle: u64, slide_idx: u32, shape_id: u32, row: u32, col: u32, x_pt: f32, y_pt: f32) -> *mut c_char {
    let res = with_text_renderer(handle, slide_idx, shape_id, |r, sh, styles| {
        let (px, py) = slide_to_local(sh, x_pt, y_pt);
        let off = r.table_cell_hit_local(sh, styles, row as usize, col as usize, px, py).ok_or("cell hit failed")?;
        Ok(format!("{{\"para\":0,\"offset\":{off}}}"))
    });
    str_to_ptr("roe_impress_cell_hit", res)
}

/// Caret segment in slide pt: {"x1":..,"y1":..,"x2":..,"y2":..}
#[no_mangle]
pub extern "C" fn roe_impress_cell_caret(handle: u64, slide_idx: u32, shape_id: u32, row: u32, col: u32, offset: u32) -> *mut c_char {
    let res = with_text_renderer(handle, slide_idx, shape_id, |r, sh, styles| {
        let (lx, ty, by) = r.table_cell_caret_local(sh, styles, row as usize, col as usize, offset as usize).ok_or("cell caret failed")?;
        let (x1, y1) = local_to_slide(sh, lx, ty);
        let (x2, y2) = local_to_slide(sh, lx, by);
        Ok(format!("{{\"x1\":{x1},\"y1\":{y1},\"x2\":{x2},\"y2\":{y2}}}"))
    });
    str_to_ptr("roe_impress_cell_caret", res)
}

// v319u: is the shape a table. 1=table, 0=other shape, -1=failure.
#[no_mangle]
pub extern "C" fn roe_impress_is_table(handle: u64, slide_idx: u32, shape_id: u32) -> i32 {
    let r = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &entry.doc else {
            return Err("not an Impress document".to_string());
        };
        let slide = pres
            .slides
            .get(slide_idx as usize)
            .ok_or_else(|| format!("slide_idx={slide_idx} out of range"))?;
        let shape = slide
            .shapes
            .iter()
            .find(|s| s.id == shape_id)
            .ok_or_else(|| format!("shape_id={shape_id} not found"))?;
        Ok(matches!(shape.content, roe_model::impress::ShapeContent::Table(_)))
    });
    match r {
        None => {
            set_last_error("roe_impress_is_table: invalid handle".to_string());
            -1
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_impress_is_table: {msg}"));
            -1
        }
        Some(Ok(b)) => if b { 1 } else { 0 },
    }
}

// v319u: caret move inside a table cell. dir 0=left 1=right. Returns new byte offset, -1 on failure.
#[no_mangle]
pub extern "C" fn roe_impress_cell_move(handle: u64, slide_idx: u32, shape_id: u32, row: u32, col: u32, offset: u32, dir: u32) -> i32 {
    let r = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &entry.doc else {
            return Err("not an Impress document".to_string());
        };
        let shape = pres
            .slides
            .get(slide_idx as usize)
            .and_then(|sl| sl.shapes.iter().find(|s| s.id == shape_id))
            .ok_or_else(|| "shape not found".to_string())?;
        let t = match &shape.content {
            roe_model::impress::ShapeContent::Table(t) => t,
            _ => return Err("shape is not a table".to_string()),
        };
        let cell = t
            .rows
            .get(row as usize)
            .and_then(|rw| rw.cells.get(col as usize))
            .ok_or_else(|| "cell not found".to_string())?;
        let text: String = cell
            .content
            .iter()
            .find_map(|c| match c {
                roe_model::common::CellContent::Paragraph(p) => {
                    Some(p.runs.iter().map(|r| r.text.as_str()).collect::<String>())
                }
                _ => None,
            })
            .unwrap_or_default();
        let off = (offset as usize).min(text.len());
        if !text.is_char_boundary(off) {
            return Err("offset is not on a char boundary".to_string());
        }
        let new = if dir == 0 {
            text[..off].char_indices().next_back().map(|(i, _)| i).unwrap_or(0)
        } else {
            text[off..].chars().next().map(|c| off + c.len_utf8()).unwrap_or(off)
        };
        Ok(new as i32)
    });
    match r {
        Some(Ok(v)) => v,
        _ => -1,
    }
}

// v319u3: grow/shrink the table shape height to the measured height. Returns height in pt, -1.0 on failure.
#[no_mangle]
pub extern "C" fn roe_impress_table_fit(handle: u64, slide_idx: u32, shape_id: u32) -> f32 {
    let measured = with_text_renderer(handle, slide_idx, shape_id, |r, sh, _styles| {
        let t = match &sh.content {
            roe_model::impress::ShapeContent::Table(t) => t,
            _ => return Err("shape is not a table".to_string()),
        };
        let rects = r.table_cell_rects_local(t);
        let mut bottom = 0.0f32;
        for row in &rects {
            for c in row {
                if c.2 > 0.0 || c.3 > 0.0 {
                    bottom = bottom.max(c.1 + c.3);
                }
            }
        }
        if bottom <= 0.0 { return Err("empty table".to_string()); }
        Ok(format!("{bottom}"))
    });
    let bottom: f32 = match measured.ok().and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => return -1.0,
    };
    let r = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else { return Err("not an Impress document".to_string()); };
        let slide = pres.slides.get_mut(slide_idx as usize).ok_or("slide out of range")?;
        let shape = slide.shapes.iter_mut().find(|s| s.id == shape_id).ok_or("shape not found")?;
        let (w, h) = shape.position.extent;
        if (h - bottom).abs() > 0.5 {
            shape.position.extent = (w, bottom);
        }
        Ok(shape.position.extent.1)
    });
    match r {
        Some(Ok(h)) => h,
        _ => -1.0,
    }
}

// v319u4: after a resize, settle table row heights. Target = shape height.
// target >= measured min height: extra height is spread over all rows.
// target <  measured min height: shape height snaps back to the measured height.
// Returns final shape height in pt, -1.0 on failure.
#[no_mangle]
pub extern "C" fn roe_impress_table_settle(handle: u64, slide_idx: u32, shape_id: u32) -> f32 {
    let measured = with_text_renderer(handle, slide_idx, shape_id, |r, sh, _styles| {
        let t = match &sh.content {
            roe_model::impress::ShapeContent::Table(t) => t,
            _ => return Err("shape is not a table".to_string()),
        };
        let rects = r.table_cell_rects_local(t);
        let n = rects.len();
        if n == 0 { return Err("empty table".to_string()); }
        let mut tops: Vec<f32> = Vec::with_capacity(n);
        let mut bottom = 0.0f32;
        for (ri, row) in rects.iter().enumerate() {
            let mut top = if ri > 0 { tops[ri - 1] } else { 0.0 };
            for c in row {
                if c.2 > 0.0 || c.3 > 0.0 {
                    top = c.1;
                    break;
                }
            }
            tops.push(top);
            for c in row {
                if c.2 > 0.0 || c.3 > 0.0 {
                    bottom = bottom.max(c.1 + c.3);
                }
            }
        }
        let mut hs: Vec<String> = Vec::with_capacity(n);
        for ri in 0..n {
            let end = if ri + 1 < n { tops[ri + 1] } else { bottom };
            hs.push(format!("{}", (end - tops[ri]).max(18.0)));
        }
        Ok(hs.join(","))
    });
    let hs: Vec<f32> = match measured {
        Ok(s) => s.split(',').filter_map(|v| v.parse().ok()).collect(),
        Err(_) => return -1.0,
    };
    if hs.is_empty() { return -1.0; }
    let r = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else { return Err("not an Impress document".to_string()); };
        let slide = pres.slides.get_mut(slide_idx as usize).ok_or("slide out of range")?;
        let shape = slide.shapes.iter_mut().find(|s| s.id == shape_id).ok_or("shape not found")?;
        let (w, target) = shape.position.extent;
        let total: f32 = hs.iter().sum();
        if let roe_model::impress::ShapeContent::Table(t) = &mut shape.content {
            if t.rows.len() == hs.len() { for (i, row) in t.rows.iter_mut().enumerate() { row.row_height = Some(hs[i]); } }
            if target > total + 0.5 && t.rows.len() == hs.len() {
                let extra = (target - total) / hs.len() as f32;
                for (i, row) in t.rows.iter_mut().enumerate() {
                    row.row_height = Some(hs[i] + extra);
                }
            } else if (target - total).abs() > 0.5 {
                shape.position.extent = (w, total);
            }
        }
        Ok(shape.position.extent.1)
    });
    match r {
        Some(Ok(h)) => h,
        _ => -1.0,
    }
}

#[cfg(test)]
mod cell_geom_tests_v319s {
    use roe_model::common::CellContent;
    use roe_model::writer::Run;

    #[test]
    fn cell_hit_and_caret_roundtrip() {
        let mut t = crate::impress_geometry::build_empty_table_v318r(3, 4, 400.0, 90.0);
        if let Some(CellContent::Paragraph(p)) = t.rows[0].cells[0].content.get_mut(0) {
            p.runs.clear();
            p.runs.push(Run { text: "Hello".to_string(), ..Default::default() });
        }
        let sh = roe_model::impress::Shape {
            id: 1,
            geometry: roe_model::impress::ShapeGeometry::Rect,
            position: roe_model::common::Transform::from_rect(50.0, 40.0, 400.0, 90.0),
            style: Default::default(),
            content: roe_model::impress::ShapeContent::Table(t),
            text_body: Default::default(),
            placeholder: None, st_cxn: None, end_cxn: None,
        };
        let styles = roe_model::style::StyleRegistry::default();
        let mut r = roe_impress::slide_renderer::SlideRenderer::new(960.0, 540.0);
        let table = match &sh.content { roe_model::impress::ShapeContent::Table(t) => t.clone(), _ => unreachable!() };
        let rects = r.table_cell_rects_local(&table);
        assert_eq!(rects.len(), 3);
        assert_eq!(rects[0].len(), 4);
        assert!(rects[1][0].1 > rects[0][0].1, "row 1 must be below row 0");
        assert!(rects[0][1].0 > rects[0][0].0, "col 1 must be right of col 0");
        let (x0, _, _) = r.table_cell_caret_local(&sh, &styles, 0, 0, 0).unwrap();
        let (x5, y5t, y5b) = r.table_cell_caret_local(&sh, &styles, 0, 0, 5).unwrap();
        assert!(x5 > x0, "caret moves right: {x0} {x5}");
        assert!(y5b > y5t);
        assert!(x0 >= rects[0][0].0);
        let off = r.table_cell_hit_local(&sh, &styles, 0, 0, x5 + 0.3, rects[0][0].1 + 5.0);
        assert_eq!(off, Some(5));
        let at = r.table_cell_at_local(&sh, rects[1][2].0 + 2.0, rects[1][2].1 + 2.0);
        assert_eq!(at, Some((1, 2)));
    }
}


// v319t: table cell text editing (first paragraph of the cell only).
fn edit_cell_in_table<F>(table: &mut roe_model::common::Table, row: usize, col: usize, f: F) -> Result<(usize, usize), String>
where
    F: FnOnce(&mut Vec<Paragraph>) -> Result<(usize, usize), String>,
{
    use roe_model::common::CellContent;
    let cell = table.rows.get_mut(row).and_then(|r| r.cells.get_mut(col))
        .ok_or_else(|| format!("cell r={row} c={col} not found"))?;
    if cell.hidden {
        return Err("cell is merged away".to_string());
    }
    let idx = cell.content.iter().position(|c| matches!(c, CellContent::Paragraph(_)));
    let mut paras: Vec<Paragraph> = match idx {
        Some(i) => match &cell.content[i] {
            CellContent::Paragraph(p) => vec![p.clone()],
            _ => vec![Paragraph::default()],
        },
        None => vec![Paragraph::default()],
    };
    let r = f(&mut paras)?;
    let newp = paras.into_iter().next().ok_or("cell paragraph lost")?;
    match idx {
        Some(i) => cell.content[i] = CellContent::Paragraph(newp),
        None => cell.content.insert(0, CellContent::Paragraph(newp)),
    }
    Ok(r)
}

fn edit_cell_paras<F>(handle: u64, slide_idx: u32, shape_id: u32, row: u32, col: u32, f: F) -> Result<(usize, usize), String>
where
    F: FnOnce(&mut Vec<Paragraph>) -> Result<(usize, usize), String>,
{
    let r = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("not an Impress document".to_string());
        };
        let slide = pres.slides.get_mut(slide_idx as usize)
            .ok_or_else(|| format!("slide_idx={slide_idx} out of range"))?;
        let shape = slide.shapes.iter_mut().find(|s| s.id == shape_id)
            .ok_or_else(|| format!("shape_id={shape_id} not found"))?;
        match &mut shape.content {
            roe_model::impress::ShapeContent::Table(t) => edit_cell_in_table(t, row as usize, col as usize, f),
            _ => Err("shape is not a table".to_string()),
        }
    });
    match r {
        None => Err("invalid handle".to_string()),
        Some(v) => v,
    }
}

#[no_mangle]
pub extern "C" fn roe_impress_cell_insert(
    handle: u64, slide_idx: u32, shape_id: u32, row: u32, col: u32, offset: u32,
    text_utf8: *const u8, text_len: u32,
) -> *mut c_char {
    if text_utf8.is_null() || text_len == 0 {
        set_last_error("roe_impress_cell_insert: empty text".to_string());
        return std::ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(text_utf8, text_len as usize) };
    let text = match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(e) => {
            set_last_error(format!("roe_impress_cell_insert: invalid UTF-8: {e}"));
            return std::ptr::null_mut();
        }
    };
    let res = edit_cell_paras(handle, slide_idx, shape_id, row, col, |ps| {
        insert_text(ps, 0, offset as usize, &text)
    });
    to_json_ptr("roe_impress_cell_insert", res)
}

/// direction: 0=Backspace, 1=Delete
#[no_mangle]
pub extern "C" fn roe_impress_cell_delete(
    handle: u64, slide_idx: u32, shape_id: u32, row: u32, col: u32, offset: u32, direction: u32,
) -> *mut c_char {
    let backward = direction == 0;
    let res = edit_cell_paras(handle, slide_idx, shape_id, row, col, |ps| {
        delete_char(ps, 0, offset as usize, backward)
    });
    to_json_ptr("roe_impress_cell_delete", res)
}

#[cfg(test)]
mod cell_edit_tests_v319t {
    use super::*;
    use roe_model::common::CellContent;

    #[test]
    fn cell_insert_and_delete() {
        let mut t = crate::impress_geometry::build_empty_table_v318r(2, 2, 200.0, 60.0);
        let r = edit_cell_in_table(&mut t, 1, 1, |ps| insert_text(ps, 0, 0, "Hello")).unwrap();
        assert_eq!(r, (0, 5));
        let r = edit_cell_in_table(&mut t, 1, 1, |ps| insert_text(ps, 0, 5, " X")).unwrap();
        assert_eq!(r, (0, 7));
        let r = edit_cell_in_table(&mut t, 1, 1, |ps| delete_char(ps, 0, 7, true)).unwrap();
        assert_eq!(r, (0, 6));
        let text: String = match &t.rows[1].cells[1].content[0] {
            CellContent::Paragraph(p) => p.runs.iter().map(|r| r.text.as_str()).collect(),
            _ => String::new(),
        };
        assert_eq!(text, "Hello ");
        // other cells untouched
        let other: String = match &t.rows[0].cells[0].content[0] {
            CellContent::Paragraph(p) => p.runs.iter().map(|r| r.text.as_str()).collect(),
            _ => String::new(),
        };
        assert_eq!(other, "");
        // failed edit leaves cell unchanged
        assert!(edit_cell_in_table(&mut t, 0, 0, |ps| delete_char(ps, 0, 0, true)).is_err());
        assert!(edit_cell_in_table(&mut t, 9, 9, |ps| insert_text(ps, 0, 0, "x")).is_err());
    }
}
