//! Impress 幻灯片级操作：新建 / 复制 / 删除（v220）。
//! 撤销由前台 begin_edit/commit_edit 事务负责，这里不处理。

use roe_model::impress::{Presentation, Shape, Slide};
use roe_parser::RoeDocument;

use crate::error::set_last_error;
use crate::handle;

/// 在 after 之后插入一张空白页，沿用参考页的母版、背景、版式装饰形状。返回新页下标。
fn insert_blank_after(pres: &mut Presentation, after: usize) -> usize {
    let (master_idx, background, layout_shapes, at) = if pres.slides.is_empty() {
        (None, None, Vec::new(), 0)
    } else {
        let r = after.min(pres.slides.len() - 1);
        let s = &pres.slides[r];
        (s.master_idx, s.background.clone(), s.layout_shapes.clone(), r + 1)
    };
    let slide = Slide { layout_idx: None,
        shapes: vec![],
        master_idx,
        animation_tree: Default::default(),
        transition: None,
        notes: None,
        background,
        layout_shapes,
    };
    pres.slides.insert(at, slide);
    at
}

fn insert_after(pres: &mut Presentation, after: usize) -> usize {
    let at = insert_blank_after(pres, after);
    // new slide follows the reference slide layout, when it has one
    let ref_layout = if pres.slides.len() > 1 {
        let r = if at > 0 { at - 1 } else { 0 };
        pres.slides.get(r).and_then(|s| s.layout_idx)
    } else {
        None
    };
    if let Some(li) = ref_layout {
        let _ = apply_layout(pres, at, li);
    }
    at
}

fn ph_key(s: &Shape) -> Option<(String, u32)> {
    s.placeholder.as_ref().map(|p| (p.ph_type.clone().unwrap_or_default(), p.index.unwrap_or(0)))
}

fn ph_family(ty: &str) -> &'static str {
    match ty {
        "ctrTitle" | "title" => "T",
        "subTitle" | "body" | "obj" | "" => "B",
        _ => "O",
    }
}

fn shape_text_empty(s: &Shape) -> bool {
    match &s.content {
        roe_model::impress::ShapeContent::Text(paras) => paras.iter().all(|p| crate::impress_text_edit::para_len(p) == 0),
        _ => false,
    }
}

/// Set the layout of a slide. Placeholders already on the slide are matched
/// to the layout (type+idx, then type, then family) and reset to the layout
/// geometry, text body and hint style, keeping typed text. Unmatched empty
/// placeholders are removed, unmatched ones with text are kept. Missing
/// layout placeholders are added. Cached decoration shapes and background
/// are rebuilt from the master and layout.
pub(crate) fn apply_layout(pres: &mut Presentation, slide_idx: usize, layout_idx: usize) -> Result<(), String> {
    if slide_idx >= pres.slides.len() {
        return Err(format!("slide_idx={slide_idx} out of range"));
    }
    let master_idx = pres.slides[slide_idx].master_idx.unwrap_or(0);
    let master = pres.master_slides.get(master_idx).ok_or_else(|| format!("master_idx={master_idx} missing"))?;
    let layout = master.layouts.get(layout_idx).ok_or_else(|| format!("layout_idx={layout_idx} missing"))?;
    let layout_ph: Vec<Shape> = layout.shapes.iter().filter(|x| x.placeholder.is_some()).cloned().collect();
    let layout_deco: Vec<Shape> = layout.shapes.iter().filter(|x| x.placeholder.is_none()).cloned().collect();
    let layout_ids: std::collections::HashSet<u32> = layout_deco.iter().map(|x| x.id).collect();
    let mut cache: Vec<Shape> = master.shapes.iter()
        .filter(|x| x.placeholder.is_none() && !layout_ids.contains(&x.id))
        .cloned()
        .collect();
    if layout.show_master_shapes == false {
        cache.clear();
    }
    cache.extend(layout_deco);
    let background = layout.background.clone().or_else(|| master.background.clone());

    let slide = &mut pres.slides[slide_idx];
    let mut used = vec![false; layout_ph.len()];
    let mut remove_ids: Vec<u32> = Vec::new();
    for sh in slide.shapes.iter_mut() {
        let (ty, ix) = match sh.placeholder.as_ref() {
            Some(info) => (info.ph_type.clone().unwrap_or_default(), info.index.unwrap_or(0)),
            None => continue,
        };
        let mut found = (0..layout_ph.len()).find(|&i| !used[i] && ph_key(&layout_ph[i]) == Some((ty.clone(), ix)));
        if found.is_none() {
            found = (0..layout_ph.len()).find(|&i| {
                !used[i] && layout_ph[i].placeholder.as_ref().and_then(|p| p.ph_type.clone()).unwrap_or_default() == ty
            });
        }
        if found.is_none() {
            found = (0..layout_ph.len()).find(|&i| {
                !used[i] && ph_family(&layout_ph[i].placeholder.as_ref().and_then(|p| p.ph_type.clone()).unwrap_or_default()) == ph_family(&ty)
            });
        }
        match found {
            Some(i) => {
                used[i] = true;
                let l = &layout_ph[i];
                sh.position = l.position.clone();
                if let roe_model::impress::ShapeContent::Text(_) = &sh.content {
                    sh.text_body = l.text_body.clone();
                }
                if let (Some(dst), Some(src)) = (sh.placeholder.as_mut(), l.placeholder.as_ref()) {
                    dst.ph_type = src.ph_type.clone();
                    dst.index = src.index;
                    dst.hint_style = src.hint_style.clone();
                }
            }
            None => {
                if shape_text_empty(sh) {
                    remove_ids.push(sh.id);
                }
            }
        }
    }
    slide.shapes.retain(|s| !remove_ids.contains(&s.id));
    let mut next_id = slide.shapes.iter().map(|x| x.id).max().unwrap_or(1) + 1;
    for (i, mut ph) in layout_ph.into_iter().enumerate() {
        if used[i] { continue; }
        ph.id = next_id;
        next_id += 1;
        slide.shapes.push(ph);
    }
    slide.layout_idx = Some(layout_idx);
    slide.layout_shapes = cache;
    slide.background = background;
    Ok(())
}

fn delete_at(pres: &mut Presentation, idx: usize) -> Result<i32, String> {
    if pres.slides.len() <= 1 {
        return Err("至少保留一张幻灯片".to_string());
    }
    if idx >= pres.slides.len() {
        return Err(format!("idx={idx} 越界"));
    }
    pres.slides.remove(idx);
    Ok(idx as i32)
}

fn duplicate_at(pres: &mut Presentation, idx: usize) -> Result<i32, String> {
    if idx >= pres.slides.len() {
        return Err(format!("idx={idx} 越界"));
    }
    let cloned = pres.slides[idx].clone();
    pres.slides.insert(idx + 1, cloned);
    Ok((idx + 1) as i32)
}

fn finish(name: &str, result: Option<Result<i32, String>>) -> i32 {
    match result {
        None => {
            set_last_error(format!("{name}: 无效句柄"));
            -1
        }
        Some(Err(msg)) => {
            set_last_error(format!("{name}: {msg}"));
            -1
        }
        Some(Ok(v)) => v,
    }
}

/// 在 after_idx 之后新建空白页。成功返回新页下标，失败 -1。
#[no_mangle]
pub extern "C" fn roe_impress_insert_slide(handle: u64, after_idx: u32) -> i32 {
    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("非 Impress 文档".to_string());
        };
        Ok(insert_after(pres, after_idx as usize) as i32)
    });
    finish("roe_impress_insert_slide", result)
}

fn json_escape(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push(' '),
            c => o.push(c),
        }
    }
    o
}

fn layouts_json(pres: &Presentation, slide_idx: usize) -> Result<String, String> {
    let slide = pres.slides.get(slide_idx).ok_or_else(|| format!("slide_idx={slide_idx} out of range"))?;
    let mi = slide.master_idx.unwrap_or(0);
    let mut out = String::from("[");
    if let Some(m) = pres.master_slides.get(mi) {
        for (i, l) in m.layouts.iter().enumerate() {
            if i > 0 { out.push(','); }
            let n_ph = l.shapes.iter().filter(|x| x.placeholder.is_some()).count();
            out.push_str(&format!(
                "{{\"index\":{},\"name\":\"{}\",\"kind\":\"{:?}\",\"placeholders\":{}}}",
                i, json_escape(&l.name), l.kind, n_ph
            ));
        }
    }
    out.push(']');
    Ok(out)
}

/// Layout index of slide_idx. Returns -1 on failure, -2 if the slide has no layout.
#[no_mangle]
pub extern "C" fn roe_impress_slide_layout(handle: u64, slide_idx: u32) -> i32 {
    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &entry.doc else {
            return Err("not impress".to_string());
        };
        let slide = pres
            .slides
            .get(slide_idx as usize)
            .ok_or_else(|| format!("slide_idx={slide_idx} out of range"))?;
        Ok(match slide.layout_idx {
            Some(i) => i as i32,
            None => -2,
        })
    });
    finish("roe_impress_slide_layout", result)
}

/// List the layouts of the master used by slide_idx, as a JSON array. Null on failure.
#[no_mangle]
pub extern "C" fn roe_impress_list_layouts(handle: u64, slide_idx: u32) -> *mut std::os::raw::c_char {
    let r = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &entry.doc else {
            return Err("not impress".to_string());
        };
        layouts_json(pres, slide_idx as usize)
    });
    let json = match r {
        None => { set_last_error("roe_impress_list_layouts: invalid handle".to_string()); return std::ptr::null_mut(); }
        Some(Err(m)) => { set_last_error(format!("roe_impress_list_layouts: {m}")); return std::ptr::null_mut(); }
        Some(Ok(j)) => j,
    };
    match std::ffi::CString::new(json) {
        Ok(cs) => cs.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Insert a new slide after after_idx using the given layout. Returns new index or -1.
#[no_mangle]
pub extern "C" fn roe_impress_insert_slide_with_layout(handle: u64, after_idx: u32, layout_idx: u32) -> i32 {
    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("not impress".to_string());
        };
        let at = insert_blank_after(pres, after_idx as usize);
        apply_layout(pres, at, layout_idx as usize)?;
        Ok(at as i32)
    });
    finish("roe_impress_insert_slide_with_layout", result)
}

/// Change the layout of an existing slide (also usable as reset: adds missing placeholders). Returns slide_idx or -1.
#[no_mangle]
pub extern "C" fn roe_impress_set_slide_layout(handle: u64, slide_idx: u32, layout_idx: u32) -> i32 {
    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("not impress".to_string());
        };
        apply_layout(pres, slide_idx as usize, layout_idx as usize)?;
        Ok(slide_idx as i32)
    });
    finish("roe_impress_set_slide_layout", result)
}

/// 删除第 idx 页。只剩一页时拒绝。成功返回被删下标，失败 -1。
#[no_mangle]
pub extern "C" fn roe_impress_delete_slide(handle: u64, idx: u32) -> i32 {
    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("非 Impress 文档".to_string());
        };
        delete_at(pres, idx as usize)
    });
    finish("roe_impress_delete_slide", result)
}

/// 复制第 idx 页到其后。成功返回新页下标，失败 -1。
#[no_mangle]
pub extern "C" fn roe_impress_duplicate_slide(handle: u64, idx: u32) -> i32 {
    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("非 Impress 文档".to_string());
        };
        duplicate_at(pres, idx as usize)
    });
    finish("roe_impress_duplicate_slide", result)
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use crate::document::roe_new_document;

    fn read<R>(h: u64, f: impl FnOnce(&Presentation) -> R) -> R {
        handle::with_entry(h, |entry| match &entry.doc {
            RoeDocument::Impress(p) => f(p),
            _ => panic!("not impress"),
        })
        .unwrap()
    }

    fn ph_types(p: &Presentation, i: usize) -> Vec<String> {
        p.slides[i].shapes.iter().filter_map(|s| s.placeholder.as_ref().and_then(|x| x.ph_type.clone())).collect()
    }

    #[test]
    fn insert_with_layout_adds_placeholders() {
        let h = roe_new_document(3);
        let at = roe_impress_insert_slide_with_layout(h, 0, 0);
        assert_eq!(at, 1);
        read(h, |p| {
            assert_eq!(p.slides[1].layout_idx, Some(0));
            assert_eq!(ph_types(p, 1), vec!["ctrTitle".to_string(), "subTitle".to_string()]);
            let ids: std::collections::HashSet<u32> = p.slides[1].shapes.iter().map(|s| s.id).collect();
            assert_eq!(ids.len(), p.slides[1].shapes.len());
        });
        handle::remove(h);
    }

    #[test]
    fn list_layouts_returns_six_default_layouts() {
        let h = roe_new_document(3);
        let j = read(h, |p| layouts_json(p, 0).unwrap());
        assert!(j.starts_with("[{\"index\":0,\"name\":\"Title Slide\",\"kind\":\"Title\",\"placeholders\":2}"), "{j}");
        assert_eq!(j.matches("\"index\"").count(), 6, "{j}");
        assert!(read(h, |p| layouts_json(p, 9)).is_err());
        let ptr = roe_impress_list_layouts(h, 0);
        assert!(!ptr.is_null());
        unsafe { drop(std::ffi::CString::from_raw(ptr)); }
        assert!(roe_impress_list_layouts(h, 9).is_null());
        handle::remove(h);
    }

    #[test]
    fn plain_insert_follows_reference_layout() {
        let h = roe_new_document(3);
        let at = roe_impress_insert_slide(h, 0);
        assert_eq!(at, 1);
        read(h, |p| {
            assert_eq!(p.slides[1].layout_idx, Some(1));
            assert_eq!(ph_types(p, 1), vec!["title".to_string(), "body".to_string()]);
        });
        handle::remove(h);
    }

    #[test]
    fn set_layout_resets_moved_placeholder() {
        let h = roe_new_document(3);
        let want = read(h, |p| format!("{:?}", p.master_slides[0].layouts[1].shapes[0].position));
        let _ = handle::with_entry(h, |entry| {
            if let RoeDocument::Impress(p) = &mut entry.doc {
                p.slides[0].shapes[0].position = roe_model::common::Transform::from_rect(1.0, 2.0, 3.0, 4.0);
            }
            Ok::<(), String>(())
        });
        assert_eq!(roe_impress_set_slide_layout(h, 0, 1), 0);
        read(h, |p| {
            assert_eq!(p.slides[0].shapes.len(), 2);
            assert_eq!(format!("{:?}", p.slides[0].shapes[0].position), want);
        });
        handle::remove(h);
    }

    #[test]
    fn set_layout_does_not_stack_placeholders() {
        let h = roe_new_document(3);
        assert_eq!(roe_impress_set_slide_layout(h, 0, 0), 0);
        read(h, |p| {
            assert_eq!(ph_types(p, 0), vec!["ctrTitle".to_string(), "subTitle".to_string()]);
        });
        assert_eq!(roe_impress_set_slide_layout(h, 0, 3), 0);
        read(h, |p| assert_eq!(p.slides[0].shapes.len(), 3));
        handle::remove(h);
    }

    #[test]
    fn slide_layout_query() {
        let h = roe_new_document(3);
        assert_eq!(roe_impress_slide_layout(h, 0), 1);
        assert_eq!(roe_impress_set_slide_layout(h, 0, 5), 0);
        assert_eq!(roe_impress_slide_layout(h, 0), 5);
        assert_eq!(roe_impress_slide_layout(h, 9), -1);
        handle::remove(h);
    }

    #[test]
    fn set_layout_keeps_existing_and_bad_index_fails() {
        let h = roe_new_document(3);
        assert_eq!(roe_impress_set_slide_layout(h, 0, 5), 0);
        read(h, |p| {
            assert_eq!(p.slides[0].layout_idx, Some(5));
            assert_eq!(p.slides[0].shapes.len(), 0);
        });
        assert_eq!(roe_impress_set_slide_layout(h, 0, 99), -1);
        assert_eq!(roe_impress_set_slide_layout(h, 9, 0), -1);
        handle::remove(h);
    }
}
