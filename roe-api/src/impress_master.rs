//! Impress 母版视图后端（v320h1）：把母版或版式合成一张临时页并渲染。
//! 只读：不修改文档。母版页 layout_idx 传 -1。

use std::ffi::CString;
use std::os::raw::c_char;
use std::slice;

use roe_impress::slide_renderer::SlideRenderer;
use roe_model::impress::{Presentation, Slide};
use roe_parser::RoeDocument;

use crate::error::set_last_error;
use crate::handle;

fn esc(s: &str) -> String {
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

/// 与 tile.rs 的 compute_render_dims 同一公式，保证尺寸一致。
fn dims(orig_w: f32, orig_h: f32, width: u32) -> (f32, u32, u32) {
    let scale = if orig_w > 0.0 { width as f32 / orig_w } else { 1.0 };
    (scale, (orig_w * scale) as u32, (orig_h * scale) as u32)
}

fn cstr(json: String, name: &str) -> *mut c_char {
    match CString::new(json) {
        Ok(cs) => cs.into_raw(),
        Err(e) => {
            set_last_error(format!("{name}: {e}"));
            std::ptr::null_mut()
        }
    }
}

fn list_json(pres: &Presentation) -> String {
    let mut out = String::from("[");
    for (mi, m) in pres.master_slides.iter().enumerate() {
        if mi > 0 { out.push(','); }
        out.push_str(&format!("{{\"master\":{},\"name\":\"{}\",\"layouts\":[", mi, esc(&m.name)));
        for (li, l) in m.layouts.iter().enumerate() {
            if li > 0 { out.push(','); }
            let used: Vec<String> = pres.slides.iter().enumerate()
                .filter(|(_, sl)| sl.master_idx.unwrap_or(0) == mi && sl.layout_idx == Some(li))
                .map(|(i, _)| (i + 1).to_string())
                .collect();
            out.push_str(&format!("{{\"index\":{},\"name\":\"{}\",\"used_by\":[{}]}}", li, esc(&l.name), used.join(",")));
        }
        out.push_str("]}");
    }
    out.push(']');
    out
}

/// 母版页：形状 = 母版全部形状（含占位符）。
/// 版式页：形状 = 版式全部形状；show_master_shapes 为真时垫母版装饰形状。
fn build_page(pres: &Presentation, master_idx: usize, layout_idx: i32) -> Result<Slide, String> {
    let m = pres
        .master_slides
        .get(master_idx)
        .ok_or_else(|| format!("master_idx={master_idx} out of range"))?;
    if layout_idx < 0 {
        return Ok(Slide {
            layout_idx: None,
            shapes: m.shapes.clone(),
            master_idx: None,
            animation_tree: Default::default(),
            transition: None,
            notes: None,
            background: m.background.clone(),
            layout_shapes: vec![],
        });
    }
    let l = m
        .layouts
        .get(layout_idx as usize)
        .ok_or_else(|| format!("layout_idx={layout_idx} out of range"))?;
    Ok(Slide {
        layout_idx: Some(layout_idx as usize),
        shapes: l.shapes.clone(),
        master_idx: if l.show_master_shapes { Some(master_idx) } else { None },
        animation_tree: Default::default(),
        transition: None,
        notes: None,
        background: l.background.clone().or_else(|| m.background.clone()),
        layout_shapes: vec![],
    })
}

fn render_into(pres: &Presentation, master_idx: usize, layout_idx: i32, width: u32, buf: &mut [u8]) -> Result<(), String> {
    let slide = build_page(pres, master_idx, layout_idx)?;
    let (sw, sh) = pres.slide_size;
    let (scale, _, _) = dims(sw, sh, width);
    let mut r = SlideRenderer::new(sw, sh);
    r.render_slide(&slide, &pres.styles, None, scale, buf, &pres.theme_colors, Some(&pres.master_slides))
        .map(|_| ())
}

/// 母版与版式两级列表（JSON）。失败返回 null，需 roe_free_string 释放。
#[no_mangle]
pub extern "C" fn roe_impress_master_list(handle: u64) -> *mut c_char {
    let r = handle::with_entry(handle, |entry| match &entry.doc {
        RoeDocument::Impress(p) => Ok(list_json(p)),
        _ => Err("not impress".to_string()),
    });
    match r {
        None => { set_last_error("roe_impress_master_list: invalid handle".to_string()); std::ptr::null_mut() }
        Some(Err(m)) => { set_last_error(format!("roe_impress_master_list: {m}")); std::ptr::null_mut() }
        Some(Ok(j)) => cstr(j, "roe_impress_master_list"),
    }
}

/// 出图尺寸（JSON {"width":W,"height":H}）。失败返回 null。
#[no_mangle]
pub extern "C" fn roe_impress_master_render_size(handle: u64, width: u32) -> *mut c_char {
    if width == 0 {
        set_last_error("roe_impress_master_render_size: width must be > 0".to_string());
        return std::ptr::null_mut();
    }
    let r = handle::with_entry(handle, |entry| match &entry.doc {
        RoeDocument::Impress(p) => {
            let (_, pw, ph) = dims(p.slide_size.0, p.slide_size.1, width);
            Ok(format!("{{\"width\":{},\"height\":{}}}", pw, ph))
        }
        _ => Err("not impress".to_string()),
    });
    match r {
        None => { set_last_error("roe_impress_master_render_size: invalid handle".to_string()); std::ptr::null_mut() }
        Some(Err(m)) => { set_last_error(format!("roe_impress_master_render_size: {m}")); std::ptr::null_mut() }
        Some(Ok(j)) => cstr(j, "roe_impress_master_render_size"),
    }
}

/// 渲染母版（layout_idx=-1）或版式到 RGBA 缓冲区。成功 0，失败 -1。
#[no_mangle]
pub extern "C" fn roe_impress_render_master(
    handle: u64,
    master_idx: u32,
    layout_idx: i32,
    width: u32,
    buf: *mut u8,
    buf_len: usize,
) -> i32 {
    if buf.is_null() {
        set_last_error("roe_impress_render_master: buf is null".to_string());
        return -1;
    }
    if width == 0 {
        set_last_error("roe_impress_render_master: width must be > 0".to_string());
        return -1;
    }
    // SAFETY: 调用方保证 buf 指向 buf_len 字节的可写内存。
    let buf_slice = unsafe { slice::from_raw_parts_mut(buf, buf_len) };
    let r = handle::with_entry(handle, |entry| match &entry.doc {
        RoeDocument::Impress(p) => render_into(p, master_idx as usize, layout_idx, width, buf_slice),
        _ => Err("not impress".to_string()),
    });
    match r {
        None => { set_last_error("roe_impress_render_master: invalid handle".to_string()); -1 }
        Some(Err(m)) => { set_last_error(format!("roe_impress_render_master: {m}")); -1 }
        Some(Ok(())) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::roe_new_document;

    fn slide_size(h: u64) -> (f32, f32) {
        handle::with_entry(h, |entry| match &entry.doc {
            RoeDocument::Impress(p) => p.slide_size,
            _ => panic!("not impress"),
        })
        .unwrap()
    }

    #[test]
    fn list_has_master_and_layouts() {
        let h = roe_new_document(3);
        let ptr = roe_impress_master_list(h);
        assert!(!ptr.is_null());
        let j = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        assert!(j.starts_with("[{\"master\":0"), "{j}");
        assert!(j.contains("Title Slide"), "{j}");
        assert!(j.contains("\"layouts\":["), "{j}");
        handle::remove(h);
    }

    #[test]
    fn render_master_and_layouts_ok_and_errors() {
        let h = roe_new_document(3);
        let (sw, sh) = slide_size(h);
        let (_, pw, ph) = dims(sw, sh, 400);
        let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];
        assert_eq!(roe_impress_render_master(h, 0, -1, 400, buf.as_mut_ptr(), buf.len()), 0);
        for li in 0..6 {
            assert_eq!(roe_impress_render_master(h, 0, li, 400, buf.as_mut_ptr(), buf.len()), 0, "layout {li}");
        }
        assert_eq!(roe_impress_render_master(h, 0, 99, 400, buf.as_mut_ptr(), buf.len()), -1);
        assert_eq!(roe_impress_render_master(h, 9, -1, 400, buf.as_mut_ptr(), buf.len()), -1);
        assert_eq!(roe_impress_render_master(h, 0, -1, 400, std::ptr::null_mut(), 0), -1);
        let ptr = roe_impress_master_render_size(h, 400);
        assert!(!ptr.is_null());
        let j = unsafe { CString::from_raw(ptr) }.into_string().unwrap();
        assert_eq!(j, format!("{{\"width\":{},\"height\":{}}}", pw, ph));
        handle::remove(h);
    }

    #[test]
    fn layout_background_is_drawn() {
        let h = roe_new_document(3);
        let _ = handle::with_entry(h, |entry| {
            if let RoeDocument::Impress(p) = &mut entry.doc {
                p.master_slides[0].layouts[1].background =
                    Some(roe_model::common::FillStyle::Solid(roe_model::common::Color { r: 220, g: 80, b: 80, a: 255 }));
            }
            Ok::<(), String>(())
        });
        let (sw, sh) = slide_size(h);
        let (_, pw, ph) = dims(sw, sh, 400);
        let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];
        assert_eq!(roe_impress_render_master(h, 0, 1, 400, buf.as_mut_ptr(), buf.len()), 0);
        let (x, y) = ((pw / 20) as usize, (ph / 2) as usize);
        let i = (y * pw as usize + x) * 4;
        assert_eq!(&buf[i..i + 3], &[220, 80, 80], "版式背景应被画出");
        handle::remove(h);
    }
}

// ===== v320i: 母版/版式画布上的命中、查询、移动缩放、背景、插入图片 =====
// layout_idx < 0 表示操作母版自身的形状/背景，否则操作该版式。

macro_rules! v320i_target_shapes {
    ($pres:expr, $mi:expr, $li:expr) => {{
        let m = $pres
            .master_slides
            .get_mut($mi as usize)
            .ok_or_else(|| format!("master_idx={} out of range", $mi))?;
        if $li < 0 {
            &mut m.shapes
        } else {
            let l = m
                .layouts
                .get_mut($li as usize)
                .ok_or_else(|| format!("layout_idx={} out of range", $li))?;
            &mut l.shapes
        }
    }};
}

fn v320i_run<R>(
    handle: u64,
    fn_name: &str,
    f: impl FnOnce(&mut Presentation) -> Result<R, String>,
) -> Option<R> {
    let r = handle::with_entry(handle, |entry| match &mut entry.doc {
        RoeDocument::Impress(p) => f(p),
        _ => Err("not impress".to_string()),
    });
    match r {
        None => { set_last_error(format!("{fn_name}: invalid handle")); None }
        Some(Err(m)) => { set_last_error(format!("{fn_name}: {m}")); None }
        Some(Ok(v)) => Some(v),
    }
}

/// 母版/版式内的命中测试，返回 {"shape_id":N} 或 {"shape_id":null}；失败返回 null。
#[no_mangle]
pub extern "C" fn roe_impress_master_hit_test(handle: u64, master_idx: u32, layout_idx: i32, x_pt: f32, y_pt: f32) -> *mut c_char {
    let j = v320i_run(handle, "roe_impress_master_hit_test", |p| {
        let shapes = v320i_target_shapes!(p, master_idx, layout_idx);
        let hit = shapes
            .iter()
            .rev()
            .find(|s| crate::impress_geometry::point_hits_shape(x_pt, y_pt, s))
            .map(|s| s.id);
        Ok(match hit {
            Some(id) => format!("{{\"shape_id\":{id}}}"),
            None => "{\"shape_id\":null}".to_string(),
        })
    });
    match j {
        Some(j) => cstr(j, "roe_impress_master_hit_test"),
        None => std::ptr::null_mut(),
    }
}

/// 查询母版/版式内形状矩形，字段与 roe_impress_query_shape_rect 相同。
#[no_mangle]
pub extern "C" fn roe_impress_master_query_shape_rect(handle: u64, master_idx: u32, layout_idx: i32, shape_id: u32) -> *mut c_char {
    let j = v320i_run(handle, "roe_impress_master_query_shape_rect", |p| {
        let shapes = v320i_target_shapes!(p, master_idx, layout_idx);
        let shape = shapes
            .iter()
            .find(|s| s.id == shape_id)
            .ok_or_else(|| format!("shape_id={shape_id} not found"))?;
        let t = &shape.position;
        let o = |v: Option<f32>| v.map(|x| x.to_string()).unwrap_or_else(|| "null".to_string());
        let ob = |v: Option<bool>| v.map(|x| x.to_string()).unwrap_or_else(|| "null".to_string());
        Ok(format!(
            "{{\"x_pt\":{:.2},\"y_pt\":{:.2},\"width_pt\":{:.2},\"height_pt\":{:.2},\"rotation\":{},\"flip_h\":{},\"flip_v\":{},\"shear_x\":{},\"shear_y\":{},\"is_line\":{}}}",
            t.x(), t.y(), t.w(), t.h(),
            o(t.rotation), ob(t.flip_h), ob(t.flip_v), o(t.shear_x), o(t.shear_y),
            crate::impress_geometry::shape_is_line(shape)
        ))
    });
    match j {
        Some(j) => cstr(j, "roe_impress_master_query_shape_rect"),
        None => std::ptr::null_mut(),
    }
}

/// 原子设置母版/版式内形状矩形（位置+尺寸，不改旋转/翻转）。
#[no_mangle]
pub extern "C" fn roe_impress_master_set_shape_rect(
    handle: u64, master_idx: u32, layout_idx: i32, shape_id: u32,
    x_pt: f32, y_pt: f32, width_pt: f32, height_pt: f32,
) -> bool {
    v320i_run(handle, "roe_impress_master_set_shape_rect", |p| {
        let shapes = v320i_target_shapes!(p, master_idx, layout_idx);
        let shape = shapes
            .iter_mut()
            .find(|s| s.id == shape_id)
            .ok_or_else(|| format!("shape_id={shape_id} not found"))?;
        let old_off = shape.position.offset;
        let old_ext = shape.position.extent;
        let key = shape
            .placeholder
            .as_ref()
            .map(|ph| (ph.ph_type.clone().unwrap_or_default(), ph.index.unwrap_or(0)));
        let sid = shape.id;
        shape.position.offset = (x_pt, y_pt);
        let (ow, oh) = shape.position.extent;
        shape.position.extent = (width_pt, height_pt);
        crate::impress_geometry::reflow_geometry(shape, ow, oh);
        if layout_idx >= 0 {
            v320j_propagate(p, master_idx as usize, layout_idx as usize, sid, key, old_off, old_ext, (x_pt, y_pt), (width_pt, height_pt));
        } else {
            v320j_propagate_master(p, master_idx as usize, sid, key, old_off, old_ext, (x_pt, y_pt), (width_pt, height_pt));
        }
        Ok(())
    })
    .is_some()
}

/// 设置母版/版式内形状的绝对旋转角度（度）。只改这一份，不向幻灯片传播。
#[no_mangle]
pub extern "C" fn roe_impress_master_set_shape_rotation(handle: u64, master_idx: u32, layout_idx: i32, shape_id: u32, degrees: f32) -> bool {
    v320i_run(handle, "roe_impress_master_set_shape_rotation", |p| {
        let shapes = v320i_target_shapes!(p, master_idx, layout_idx);
        let shape = shapes
            .iter_mut()
            .find(|s| s.id == shape_id)
            .ok_or_else(|| format!("shape_id={shape_id} not found"))?;
        shape.position.rotation = Some(degrees);
        Ok(())
    })
    .is_some()
}

fn v320j_norm_ph(t: &str) -> &'static str {
    match t {
        "ctrTitle" | "title" => "title",
        "subTitle" | "body" | "obj" | "" => "body",
        "dt" => "dt",
        "ftr" => "ftr",
        "sldNum" => "sldNum",
        _ => "other",
    }
}

fn v320j_sync_one(sh: &mut roe_model::impress::Shape, old_off: (f32, f32), old_ext: (f32, f32), new_off: (f32, f32), new_ext: (f32, f32)) {
    if v320j_same(sh.position.offset, old_off) && v320j_same(sh.position.extent, old_ext) {
        let (ow, oh) = sh.position.extent;
        sh.position.offset = new_off;
        sh.position.extent = new_ext;
        crate::impress_geometry::reflow_geometry(sh, ow, oh);
    }
}

/// 母版自身的形状改了位置后：同步位置仍等于旧位置的版式占位符、幻灯片占位符，
/// 以及幻灯片缓存的母版装饰形状。占位符按归一化类型配对（忽略 index）。
fn v320j_propagate_master(
    p: &mut Presentation, master_idx: usize, shape_id: u32,
    key: Option<(String, u32)>, old_off: (f32, f32), old_ext: (f32, f32),
    new_off: (f32, f32), new_ext: (f32, f32),
) {
    let ph_class = |sh: &roe_model::impress::Shape| -> Option<&'static str> {
        sh.placeholder
            .as_ref()
            .map(|ph| v320j_norm_ph(&ph.ph_type.clone().unwrap_or_default()))
    };
    let want = key.as_ref().map(|k| v320j_norm_ph(&k.0));
    if let Some(w) = want {
        if let Some(m) = p.master_slides.get_mut(master_idx) {
            for l in m.layouts.iter_mut() {
                for sh in l.shapes.iter_mut() {
                    if ph_class(sh) == Some(w) {
                        v320j_sync_one(sh, old_off, old_ext, new_off, new_ext);
                    }
                }
            }
        }
    }
    for slide in p.slides.iter_mut() {
        if slide.master_idx.unwrap_or(0) != master_idx {
            continue;
        }
        match want {
            Some(w) => {
                for sh in slide.shapes.iter_mut() {
                    if ph_class(sh) == Some(w) {
                        v320j_sync_one(sh, old_off, old_ext, new_off, new_ext);
                    }
                }
            }
            None => {
                for sh in slide.layout_shapes.iter_mut() {
                    if sh.id == shape_id {
                        v320j_sync_one(sh, old_off, old_ext, new_off, new_ext);
                    }
                }
            }
        }
    }
}

fn v320j_same(a: (f32, f32), b: (f32, f32)) -> bool {
    (a.0 - b.0).abs() < 0.05 && (a.1 - b.1).abs() < 0.05
}

/// 版式内形状改了位置后，同步到使用该版式、且位置仍等于旧位置的幻灯片。
fn v320j_propagate(
    p: &mut Presentation, master_idx: usize, layout_idx: usize, shape_id: u32,
    key: Option<(String, u32)>, old_off: (f32, f32), old_ext: (f32, f32),
    new_off: (f32, f32), new_ext: (f32, f32),
) {
    for slide in p.slides.iter_mut() {
        if slide.master_idx.unwrap_or(0) != master_idx || slide.layout_idx != Some(layout_idx) {
            continue;
        }
        if let Some(k) = &key {
            for sh in slide.shapes.iter_mut() {
                let sk = sh
                    .placeholder
                    .as_ref()
                    .map(|ph| (ph.ph_type.clone().unwrap_or_default(), ph.index.unwrap_or(0)));
                if sk.as_ref() == Some(k)
                    && v320j_same(sh.position.offset, old_off)
                    && v320j_same(sh.position.extent, old_ext)
                {
                    let (ow, oh) = sh.position.extent;
                    sh.position.offset = new_off;
                    sh.position.extent = new_ext;
                    crate::impress_geometry::reflow_geometry(sh, ow, oh);
                }
            }
        } else {
            for sh in slide.layout_shapes.iter_mut() {
                if sh.id == shape_id
                    && v320j_same(sh.position.offset, old_off)
                    && v320j_same(sh.position.extent, old_ext)
                {
                    let (ow, oh) = sh.position.extent;
                    sh.position.offset = new_off;
                    sh.position.extent = new_ext;
                    crate::impress_geometry::reflow_geometry(sh, ow, oh);
                }
            }
        }
    }
}

/// 列出母版/版式内所有形状 id（JSON 数组），测试和前台调试用。
#[no_mangle]
pub extern "C" fn roe_impress_master_shape_ids(handle: u64, master_idx: u32, layout_idx: i32) -> *mut c_char {
    let j = v320i_run(handle, "roe_impress_master_shape_ids", |p| {
        let shapes = v320i_target_shapes!(p, master_idx, layout_idx);
        let ids: Vec<String> = shapes.iter().map(|s| s.id.to_string()).collect();
        Ok(format!("[{}]", ids.join(",")))
    });
    match j {
        Some(j) => cstr(j, "roe_impress_master_shape_ids"),
        None => std::ptr::null_mut(),
    }
}

/// 设置母版/版式背景。color: "#rrggbb" 纯色；"none" 清除（版式清除后回退到母版背景）。
#[no_mangle]
pub extern "C" fn roe_impress_master_set_background(handle: u64, master_idx: u32, layout_idx: i32, color: *const c_char) -> bool {
    let color = if color.is_null() {
        String::new()
    } else {
        unsafe { std::ffi::CStr::from_ptr(color) }.to_string_lossy().to_string()
    };
    v320i_run(handle, "roe_impress_master_set_background", |p| {
        let fill = if color == "none" {
            None
        } else {
            let c = crate::impress_geometry::parse_hex_rgb_v318o(&color)
                .ok_or_else(|| format!("bad color {color}"))?;
            Some(roe_model::common::FillStyle::Solid(c))
        };
        let m = p
            .master_slides
            .get_mut(master_idx as usize)
            .ok_or_else(|| format!("master_idx={master_idx} out of range"))?;
        if layout_idx < 0 {
            m.background = fill;
        } else {
            let l = m
                .layouts
                .get_mut(layout_idx as usize)
                .ok_or_else(|| format!("layout_idx={layout_idx} out of range"))?;
            l.background = fill;
        }
        Ok(())
    })
    .is_some()
}

fn v320i_detect_mime(data: &[u8], mime: *const c_char) -> Result<String, String> {
    if !mime.is_null() {
        let s = unsafe { std::ffi::CStr::from_ptr(mime) }.to_string_lossy().to_string();
        if !s.is_empty() {
            return Ok(s);
        }
    }
    if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        Ok("image/png".to_string())
    } else if data.starts_with(&[0xFF, 0xD8]) {
        Ok("image/jpeg".to_string())
    } else {
        Err("unknown image format".to_string())
    }
}

/// 设置母版/版式背景为图片（拉伸铺满）。
#[no_mangle]
pub extern "C" fn roe_impress_master_set_background_image(
    handle: u64, master_idx: u32, layout_idx: i32,
    bytes: *const u8, len: usize, mime: *const c_char,
) -> bool {
    if bytes.is_null() || len == 0 {
        set_last_error("roe_impress_master_set_background_image: empty data".to_string());
        return false;
    }
    let data: Vec<u8> = unsafe { slice::from_raw_parts(bytes, len) }.to_vec();
    let mime_s = match v320i_detect_mime(&data, mime) {
        Ok(s) => s,
        Err(e) => { set_last_error(format!("roe_impress_master_set_background_image: {e}")); return false; }
    };
    v320i_run(handle, "roe_impress_master_set_background_image", |p| {
        let fill = Some(roe_model::common::FillStyle::Picture(roe_model::common::ImageData {
            data,
            mime: mime_s,
            crop_l: 0.0, crop_t: 0.0, crop_r: 0.0, crop_b: 0.0,
        }));
        let m = p
            .master_slides
            .get_mut(master_idx as usize)
            .ok_or_else(|| format!("master_idx={master_idx} out of range"))?;
        if layout_idx < 0 {
            m.background = fill;
        } else {
            let l = m
                .layouts
                .get_mut(layout_idx as usize)
                .ok_or_else(|| format!("layout_idx={layout_idx} out of range"))?;
            l.background = fill;
        }
        Ok(())
    })
    .is_some()
}

/// 在母版/版式里插入图片形状，返回新形状 id，失败 -1。
#[no_mangle]
pub extern "C" fn roe_impress_master_insert_image(
    handle: u64, master_idx: u32, layout_idx: i32,
    bytes: *const u8, len: usize, mime: *const c_char,
    x: f32, y: f32, w: f32, h: f32,
) -> i32 {
    if bytes.is_null() || len == 0 {
        set_last_error("roe_impress_master_insert_image: empty data".to_string());
        return -1;
    }
    let data: Vec<u8> = unsafe { slice::from_raw_parts(bytes, len) }.to_vec();
    let mime_s = match v320i_detect_mime(&data, mime) {
        Ok(s) => s,
        Err(e) => { set_last_error(format!("roe_impress_master_insert_image: {e}")); return -1; }
    };
    let r = v320i_run(handle, "roe_impress_master_insert_image", |p| {
        let shapes = v320i_target_shapes!(p, master_idx, layout_idx);
        let next_id = shapes.iter().map(|s| s.id).max().unwrap_or(0) + 1;
        shapes.push(roe_model::impress::Shape {
            id: next_id,
            geometry: roe_model::common::ShapeGeometry::Rect,
            position: roe_model::common::Transform::from_rect(x, y, w, h),
            style: roe_model::common::ShapeStyle { fill: None, stroke: None, ..Default::default() },
            content: roe_model::impress::ShapeContent::Image(roe_model::common::ImageData {
                data,
                mime: mime_s,
                crop_l: 0.0, crop_t: 0.0, crop_r: 0.0, crop_b: 0.0,
            }),
            text_body: Default::default(),
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        });
        Ok(next_id)
    });
    match r {
        Some(id) => id as i32,
        None => -1,
    }
}

#[cfg(test)]
mod tests_v320i {
    use super::*;
    use crate::document::roe_new_document;

    fn take(ptr: *mut c_char) -> String {
        assert!(!ptr.is_null());
        unsafe { CString::from_raw(ptr) }.into_string().unwrap()
    }

    fn first_layout_with_shape(h: u64) -> (u32, i32, u32) {
        let j = take(roe_impress_master_list(h));
        let _ = j;
        for li in 0..6i32 {
            let ids = take(roe_impress_master_shape_ids(h, 0, li));
            if ids != "[]" {
                let id: u32 = ids.trim_matches(|c| c == '[' || c == ']').split(',').next().unwrap().parse().unwrap();
                return (0, li, id);
            }
        }
        panic!("no layout has shapes");
    }

    fn rect_json(h: u64, mi: u32, li: i32, id: u32) -> String {
        take(roe_impress_master_query_shape_rect(h, mi, li, id))
    }

    #[test]
    fn set_rect_moves_layout_shape_and_hit_test_follows() {
        let h = roe_new_document(3);
        let (mi, li, id) = first_layout_with_shape(h);
        assert!(roe_impress_master_set_shape_rect(h, mi, li, id, 100.0, 120.0, 200.0, 80.0));
        let j = rect_json(h, mi, li, id);
        assert!(j.contains("\"x_pt\":100.00"), "{j}");
        assert!(j.contains("\"y_pt\":120.00"), "{j}");
        assert!(j.contains("\"width_pt\":200.00"), "{j}");
        assert!(j.contains("\"height_pt\":80.00"), "{j}");
        let hit = take(roe_impress_master_hit_test(h, mi, li, 150.0, 150.0));
        assert!(hit.contains(&format!("\"shape_id\":{id}")), "{hit}");
        handle::remove(h);
    }

    #[test]
    fn set_rect_on_master_itself() {
        let h = roe_new_document(3);
        let ids = take(roe_impress_master_shape_ids(h, 0, -1));
        if ids == "[]" { handle::remove(h); return; }
        let id: u32 = ids.trim_matches(|c| c == '[' || c == ']').split(',').next().unwrap().parse().unwrap();
        assert!(roe_impress_master_set_shape_rect(h, 0, -1, id, 11.0, 22.0, 33.0, 44.0));
        let j = rect_json(h, 0, -1, id);
        assert!(j.contains("\"x_pt\":11.00") && j.contains("\"height_pt\":44.00"), "{j}");
        handle::remove(h);
    }

    #[test]
    fn background_set_and_clear() {
        let h = roe_new_document(3);
        let red = CString::new("#dc5050").unwrap();
        assert!(roe_impress_master_set_background(h, 0, 1, red.as_ptr()));
        assert!(roe_impress_master_set_background(h, 0, -1, red.as_ptr()));
        let ok = handle::with_entry(h, |e| match &e.doc {
            RoeDocument::Impress(p) => Ok(p.master_slides[0].layouts[1].background.is_some() && p.master_slides[0].background.is_some()),
            _ => Err("x".to_string()),
        }).unwrap().unwrap();
        assert!(ok);
        let none = CString::new("none").unwrap();
        assert!(roe_impress_master_set_background(h, 0, 1, none.as_ptr()));
        let cleared = handle::with_entry(h, |e| match &e.doc {
            RoeDocument::Impress(p) => Ok(p.master_slides[0].layouts[1].background.is_none()),
            _ => Err("x".to_string()),
        }).unwrap().unwrap();
        assert!(cleared);
        let bad = CString::new("zzz").unwrap();
        assert!(!roe_impress_master_set_background(h, 0, 1, bad.as_ptr()));
        handle::remove(h);
    }

    fn png_1x1() -> Vec<u8> {
        vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
    }

    #[test]
    fn insert_image_into_layout_adds_shape() {
        let h = roe_new_document(3);
        let before = take(roe_impress_master_shape_ids(h, 0, 1));
        let n0 = if before == "[]" { 0 } else { before.split(',').count() };
        let data = png_1x1();
        let id = roe_impress_master_insert_image(h, 0, 1, data.as_ptr(), data.len(), std::ptr::null(), 10.0, 10.0, 50.0, 50.0);
        assert!(id > 0, "id={id}");
        let after = take(roe_impress_master_shape_ids(h, 0, 1));
        assert_eq!(after.split(',').count(), n0 + 1, "{after}");
        let junk = [1u8, 2, 3];
        assert_eq!(roe_impress_master_insert_image(h, 0, 1, junk.as_ptr(), junk.len(), std::ptr::null(), 0.0, 0.0, 1.0, 1.0), -1);
        handle::remove(h);
    }

    #[test]
    fn out_of_range_fails() {
        let h = roe_new_document(3);
        assert!(!roe_impress_master_set_shape_rect(h, 9, -1, 1, 0.0, 0.0, 1.0, 1.0));
        assert!(!roe_impress_master_set_shape_rect(h, 0, 99, 1, 0.0, 0.0, 1.0, 1.0));
        assert!(!roe_impress_master_set_shape_rect(h, 0, -1, 99999, 0.0, 0.0, 1.0, 1.0));
        assert!(roe_impress_master_hit_test(h, 0, 99, 0.0, 0.0).is_null());
        assert!(roe_impress_master_query_shape_rect(h, 0, -1, 99999).is_null());
        handle::remove(h);
    }
}

#[cfg(test)]
mod tests_v320j {
    use super::*;

    #[test]
    fn layout_move_propagates_only_to_untouched_slides() {
        let h = crate::document::roe_new_document(3);
        assert_eq!(crate::roe_impress_set_slide_layout(h, 0, 1), 0);
        assert_eq!(crate::roe_impress_insert_slide(h, 0), 1);
        assert_eq!(crate::roe_impress_set_slide_layout(h, 1, 1), 1);
        let mut sid = 0u32;
        let _ = handle::with_entry(h, |e| {
            if let RoeDocument::Impress(p) = &mut e.doc {
                p.slides[1].shapes[0].position = roe_model::common::Transform::from_rect(1.0, 2.0, 3.0, 4.0);
                sid = p.master_slides[0].layouts[1].shapes[0].id;
            }
            Ok::<(), String>(())
        });
        assert!(roe_impress_master_set_shape_rect(h, 0, 1, sid, 10.0, 20.0, 300.0, 40.0));
        let mut r0 = (0.0, 0.0, 0.0, 0.0);
        let mut r1 = (0.0, 0.0, 0.0, 0.0);
        let _ = handle::with_entry(h, |e| {
            if let RoeDocument::Impress(p) = &mut e.doc {
                let a = &p.slides[0].shapes[0].position;
                r0 = (a.x(), a.y(), a.w(), a.h());
                let b = &p.slides[1].shapes[0].position;
                r1 = (b.x(), b.y(), b.w(), b.h());
            }
            Ok::<(), String>(())
        });
        assert_eq!(r0, (10.0, 20.0, 300.0, 40.0));
        assert_eq!(r1, (1.0, 2.0, 3.0, 4.0));
        handle::remove(h);
    }
}

#[cfg(test)]
mod tests_v320j_master {
    use super::*;

    #[test]
    fn master_move_propagates_to_layout_and_untouched_slides() {
        let h = crate::document::roe_new_document(3);
        assert_eq!(crate::roe_impress_set_slide_layout(h, 0, 1), 0);
        assert_eq!(crate::roe_impress_insert_slide(h, 0), 1);
        assert_eq!(crate::roe_impress_set_slide_layout(h, 1, 1), 1);
        let mut mid = 0u32;
        let _ = handle::with_entry(h, |e| {
            if let RoeDocument::Impress(p) = &mut e.doc {
                p.slides[1].shapes[0].position = roe_model::common::Transform::from_rect(1.0, 2.0, 3.0, 4.0);
                let mut ms = p.master_slides[0].layouts[1].shapes[0].clone();
                ms.id = 9999;
                mid = ms.id;
                p.master_slides[0].shapes.push(ms);
            }
            Ok::<(), String>(())
        });
        assert!(roe_impress_master_set_shape_rect(h, 0, -1, mid, 10.0, 20.0, 300.0, 40.0));
        let mut lay = (0.0, 0.0, 0.0, 0.0);
        let mut r0 = (0.0, 0.0, 0.0, 0.0);
        let mut r1 = (0.0, 0.0, 0.0, 0.0);
        let _ = handle::with_entry(h, |e| {
            if let RoeDocument::Impress(p) = &mut e.doc {
                let a = &p.master_slides[0].layouts[1].shapes[0].position;
                lay = (a.x(), a.y(), a.w(), a.h());
                let b = &p.slides[0].shapes[0].position;
                r0 = (b.x(), b.y(), b.w(), b.h());
                let c = &p.slides[1].shapes[0].position;
                r1 = (c.x(), c.y(), c.w(), c.h());
            }
            Ok::<(), String>(())
        });
        assert_eq!(lay, (10.0, 20.0, 300.0, 40.0));
        assert_eq!(r0, (10.0, 20.0, 300.0, 40.0));
        assert_eq!(r1, (1.0, 2.0, 3.0, 4.0));
        handle::remove(h);
    }
}

/// 返回指定母版下每个版式被幻灯片使用的次数（JSON 数组，下标即 layout_idx）。
#[no_mangle]
pub extern "C" fn roe_impress_master_layout_usage(handle: u64, master_idx: u32) -> *mut c_char {
    let j = v320i_run(handle, "roe_impress_master_layout_usage", |p| {
        let m = p
            .master_slides
            .get(master_idx as usize)
            .ok_or_else(|| format!("master_idx={master_idx} out of range"))?;
        let counts: Vec<String> = (0..m.layouts.len())
            .map(|li| {
                p.slides
                    .iter()
                    .filter(|s| s.master_idx.unwrap_or(0) == master_idx as usize && s.layout_idx == Some(li))
                    .count()
                    .to_string()
            })
            .collect();
        Ok(format!("[{}]", counts.join(",")))
    });
    match j {
        Some(j) => cstr(j, "roe_impress_master_layout_usage"),
        None => std::ptr::null_mut(),
    }
}

/// 删除版式。有幻灯片在用则拒绝（返回 false）；只剩一个版式也拒绝。
/// 删除后同母版下 layout_idx 更大的幻灯片下标减 1。
#[no_mangle]
pub extern "C" fn roe_impress_master_delete_layout(handle: u64, master_idx: u32, layout_idx: u32) -> bool {
    v320i_run(handle, "roe_impress_master_delete_layout", |p| {
        let mi = master_idx as usize;
        let li = layout_idx as usize;
        let n = p
            .master_slides
            .get(mi)
            .ok_or_else(|| format!("master_idx={master_idx} out of range"))?
            .layouts
            .len();
        if li >= n {
            return Err(format!("layout_idx={layout_idx} out of range"));
        }
        if n <= 1 {
            return Err("cannot delete the last layout".to_string());
        }
        if p.slides.iter().any(|s| s.master_idx.unwrap_or(0) == mi && s.layout_idx == Some(li)) {
            return Err("layout is in use".to_string());
        }
        p.master_slides[mi].layouts.remove(li);
        for s in p.slides.iter_mut() {
            if s.master_idx.unwrap_or(0) == mi {
                if let Some(x) = s.layout_idx {
                    if x > li {
                        s.layout_idx = Some(x - 1);
                    }
                }
            }
        }
        Ok(())
    })
    .is_some()
}

/// 复制版式并插到原版式之后，名称加"副本"。成功返回新版式下标，失败 -1。
#[no_mangle]
pub extern "C" fn roe_impress_master_duplicate_layout(handle: u64, master_idx: u32, layout_idx: u32) -> i32 {
    v320i_run(handle, "roe_impress_master_duplicate_layout", |p| {
        let mi = master_idx as usize;
        let li = layout_idx as usize;
        let m = p
            .master_slides
            .get_mut(mi)
            .ok_or_else(|| format!("master_idx={master_idx} out of range"))?;
        let mut copy = m
            .layouts
            .get(li)
            .ok_or_else(|| format!("layout_idx={layout_idx} out of range"))?
            .clone();
        copy.name = format!("{} 副本", copy.name);
        m.layouts.insert(li + 1, copy);
        for s in p.slides.iter_mut() {
            if s.master_idx.unwrap_or(0) == mi {
                if let Some(x) = s.layout_idx {
                    if x > li {
                        s.layout_idx = Some(x + 1);
                    }
                }
            }
        }
        Ok((li + 1) as i32)
    })
    .unwrap_or(-1)
}

/// 重命名版式（名称首尾空白会去掉；空名拒绝）。成功返回 true。
#[no_mangle]
pub extern "C" fn roe_impress_master_rename_layout(handle: u64, master_idx: u32, layout_idx: u32, name: *const c_char) -> bool {
    if name.is_null() {
        set_last_error("roe_impress_master_rename_layout: null name".to_string());
        return false;
    }
    let s = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy().trim().to_string();
    if s.is_empty() {
        set_last_error("roe_impress_master_rename_layout: empty name".to_string());
        return false;
    }
    v320i_run(handle, "roe_impress_master_rename_layout", |p| {
        let m = p
            .master_slides
            .get_mut(master_idx as usize)
            .ok_or_else(|| format!("master_idx={master_idx} out of range"))?;
        let lay = m
            .layouts
            .get_mut(layout_idx as usize)
            .ok_or_else(|| format!("layout_idx={layout_idx} out of range"))?;
        lay.name = s;
        Ok(())
    })
    .is_some()
}

#[cfg(test)]
mod tests_v320k {
    use super::*;

    fn usage(h: u64) -> String {
        let ptr = roe_impress_master_layout_usage(h, 0);
        assert!(!ptr.is_null());
        let s = unsafe { std::ffi::CStr::from_ptr(ptr) }.to_string_lossy().to_string();
        unsafe { drop(CString::from_raw(ptr)) };
        s
    }

    #[test]
    fn delete_layout_rules_and_reindex() {
        let h = crate::document::roe_new_document(3);
        assert_eq!(crate::roe_impress_set_slide_layout(h, 0, 3), 0);
        assert_eq!(usage(h), "[0,0,0,1,0,0]");
        assert!(!roe_impress_master_delete_layout(h, 0, 3));
        assert!(roe_impress_master_delete_layout(h, 0, 1));
        assert_eq!(usage(h), "[0,0,1,0,0]");
        let mut idx = None;
        let _ = handle::with_entry(h, |e| {
            if let RoeDocument::Impress(p) = &e.doc {
                idx = p.slides[0].layout_idx;
            }
            Ok::<(), String>(())
        });
        assert_eq!(idx, Some(2));
        handle::remove(h);
    }

    #[test]
    fn duplicate_layout_reindexes() {
        let h = crate::document::roe_new_document(3);
        assert_eq!(crate::roe_impress_set_slide_layout(h, 0, 3), 0);
        assert_eq!(roe_impress_master_duplicate_layout(h, 0, 1), 2);
        assert_eq!(usage(h), "[0,0,0,0,1,0,0]");
        assert_eq!(roe_impress_master_duplicate_layout(h, 0, 99), -1);
        handle::remove(h);
    }
}

/// 删除母版/版式内的一个形状。不动幻灯片上已有的占位符；
/// 装饰形状（非占位符）同时从使用该母版/版式的幻灯片缓存里去掉。
#[no_mangle]
pub extern "C" fn roe_impress_master_delete_shape(handle: u64, master_idx: u32, layout_idx: i32, shape_id: u32) -> bool {
    v320i_run(handle, "roe_impress_master_delete_shape", |p| {
        let is_deco;
        {
            let shapes = v320i_target_shapes!(p, master_idx, layout_idx);
            let pos = shapes
                .iter()
                .position(|s| s.id == shape_id)
                .ok_or_else(|| format!("shape_id={shape_id} not found"))?;
            is_deco = shapes[pos].placeholder.is_none();
            shapes.remove(pos);
        }
        if is_deco {
            for slide in p.slides.iter_mut() {
                if slide.master_idx.unwrap_or(0) != master_idx as usize {
                    continue;
                }
                if layout_idx >= 0 && slide.layout_idx != Some(layout_idx as usize) {
                    continue;
                }
                slide.layout_shapes.retain(|x| x.id != shape_id);
            }
        }
        Ok(())
    })
    .is_some()
}

#[cfg(test)]
mod tests_v320l {
    use super::*;

    #[test]
    fn delete_layout_shape_keeps_slide_placeholders() {
        let h = crate::document::roe_new_document(3);
        assert_eq!(crate::roe_impress_set_slide_layout(h, 0, 1), 0);
        let mut sid = 0u32;
        let _ = handle::with_entry(h, |e| {
            if let RoeDocument::Impress(p) = &e.doc {
                sid = p.master_slides[0].layouts[1].shapes[0].id;
            }
            Ok::<(), String>(())
        });
        let mut before = 0usize;
        let _ = handle::with_entry(h, |e| {
            if let RoeDocument::Impress(p) = &e.doc {
                before = p.master_slides[0].layouts[1].shapes.len();
            }
            Ok::<(), String>(())
        });
        assert!(roe_impress_master_delete_shape(h, 0, 1, sid));
        assert!(!roe_impress_master_delete_shape(h, 0, 1, sid));
        let mut after = 0usize;
        let mut slide_shapes = 0usize;
        let _ = handle::with_entry(h, |e| {
            if let RoeDocument::Impress(p) = &e.doc {
                after = p.master_slides[0].layouts[1].shapes.len();
                slide_shapes = p.slides[0].shapes.len();
            }
            Ok::<(), String>(())
        });
        assert_eq!(after, before - 1);
        assert_eq!(slide_shapes, 2);
        handle::remove(h);
    }
}


fn v320n_ph_kind(k: &str) -> Option<(&'static str, Option<u32>)> {
    match k {
        "title" => Some(("title", None)),
        "body" => Some(("body", Some(1))),
        "date" => Some(("dt", Some(10))),
        "footer" => Some(("ftr", Some(11))),
        "slide_number" => Some(("sldNum", Some(12))),
        _ => None,
    }
}

fn v320n_is_ph(s: &roe_model::impress::Shape, norm: &str) -> bool {
    match s.placeholder.as_ref() {
        Some(ph) => v320j_norm_ph(ph.ph_type.as_deref().unwrap_or("")) == norm,
        None => false,
    }
}

/// 给版式加一个占位符。kind: title/body/date/footer/slide_number。
/// 版式已有同类型占位符时拒绝。成功返回新形状 id，失败 -1。
#[no_mangle]
pub extern "C" fn roe_impress_master_insert_placeholder(handle: u64, master_idx: u32, layout_idx: u32, kind: *const c_char) -> i32 {
    if kind.is_null() {
        set_last_error("roe_impress_master_insert_placeholder: null kind".to_string());
        return -1;
    }
    let k = unsafe { std::ffi::CStr::from_ptr(kind) }.to_string_lossy().to_string();
    let (ph_type, ph_index) = match v320n_ph_kind(&k) {
        Some(x) => x,
        None => { set_last_error(format!("roe_impress_master_insert_placeholder: bad kind {k}")); return -1; }
    };
    let r = v320i_run(handle, "roe_impress_master_insert_placeholder", |p| {
        let mi = master_idx as usize;
        let li = layout_idx as usize;
        let (reference, next_id) = {
            let m = p.master_slides.get(mi).ok_or_else(|| format!("master_idx={master_idx} out of range"))?;
            let l = m.layouts.get(li).ok_or_else(|| format!("layout_idx={layout_idx} out of range"))?;
            if l.shapes.iter().any(|s| v320n_is_ph(s, ph_type)) {
                return Err("layout already has this placeholder type".to_string());
            }
            let mut reference = m.shapes.iter().find(|s| v320n_is_ph(s, ph_type)).cloned();
            if reference.is_none() {
                for (i, other) in m.layouts.iter().enumerate() {
                    if i == li { continue; }
                    if let Some(s) = other.shapes.iter().find(|s| v320n_is_ph(s, ph_type)) {
                        reference = Some(s.clone());
                        break;
                    }
                }
            }
            let next_id = l.shapes.iter().map(|s| s.id).max().unwrap_or(0) + 1;
            (reference, next_id)
        };
        let mut sh = match reference {
            Some(r) => r,
            None => {
                let (x, y, w, h) = match ph_type {
                    "title" => (59.5f32, 18.0f32, 674.8f32, 74.5f32),
                    "body" => (59.5, 104.5, 674.8, 294.9),
                    "dt" => (59.5, 413.0, 185.0, 24.0),
                    "ftr" => (270.0, 413.0, 254.0, 24.0),
                    _ => (574.0, 413.0, 160.0, 24.0),
                };
                roe_model::impress::Shape {
                    id: 0,
                    geometry: roe_model::common::ShapeGeometry::Rect,
                    position: roe_model::common::Transform::from_rect(x, y, w, h),
                    style: roe_model::common::ShapeStyle { fill: None, stroke: None, ..Default::default() },
                    content: roe_model::impress::ShapeContent::Text(Vec::new()),
                    text_body: Default::default(),
                    placeholder: Some(roe_model::impress::PlaceholderInfo {
                        ph_type: Some(ph_type.to_string()),
                        index: ph_index,
                        hint_style: None,
                    }),
                    st_cxn: None,
                    end_cxn: None,
                }
            }
        };
        sh.id = next_id;
        let lay = p.master_slides[mi].layouts.get_mut(li).ok_or_else(|| "layout vanished".to_string())?;
        lay.shapes.push(sh);
        Ok(next_id)
    });
    match r {
        Some(id) => id as i32,
        None => -1,
    }
}

/// 版式里已有的占位符类型（JSON 字符串数组，取值 title/body/dt/ftr/sldNum）；失败返回 null。
#[no_mangle]
pub extern "C" fn roe_impress_master_layout_placeholders(handle: u64, master_idx: u32, layout_idx: u32) -> *mut c_char {
    let j = v320i_run(handle, "roe_impress_master_layout_placeholders", |p| {
        let m = p.master_slides.get(master_idx as usize).ok_or_else(|| format!("master_idx={master_idx} out of range"))?;
        let l = m.layouts.get(layout_idx as usize).ok_or_else(|| format!("layout_idx={layout_idx} out of range"))?;
        let mut out: Vec<String> = Vec::new();
        for n in ["title", "body", "dt", "ftr", "sldNum"] {
            if l.shapes.iter().any(|s| v320n_is_ph(s, n)) {
                out.push(format!("\"{}\"", n));
            }
        }
        Ok(format!("[{}]", out.join(",")))
    });
    match j {
        Some(j) => cstr(j, "roe_impress_master_layout_placeholders"),
        None => std::ptr::null_mut(),
    }
}

#[cfg(test)]
mod tests_v320n {
    use super::*;

    fn layout_name(h: u64, li: usize) -> String {
        let mut nm = String::new();
        let _ = handle::with_entry(h, |e| {
            if let RoeDocument::Impress(p) = &e.doc {
                nm = p.master_slides[0].layouts[li].name.clone();
            }
            Ok::<(), String>(())
        });
        nm
    }

    #[test]
    fn rename_layout_basic() {
        let h = crate::document::roe_new_document(3);
        let old = layout_name(h, 1);
        let c = std::ffi::CString::new("  我的版式  ").unwrap();
        assert!(roe_impress_master_rename_layout(h, 0, 1, c.as_ptr()));
        assert_eq!(layout_name(h, 1), "我的版式");
        let e = std::ffi::CString::new("   ").unwrap();
        assert!(!roe_impress_master_rename_layout(h, 0, 1, e.as_ptr()));
        assert_eq!(layout_name(h, 1), "我的版式");
        assert!(!roe_impress_master_rename_layout(h, 0, 999, c.as_ptr()));
        assert!(!roe_impress_master_rename_layout(h, 5, 1, c.as_ptr()));
        assert_ne!(old, "我的版式");
    }
}


#[cfg(test)]
mod tests_v320n2 {
    use super::*;

    fn ph_list(h: u64, li: u32) -> String {
        let ptr = roe_impress_master_layout_placeholders(h, 0, li);
        assert!(!ptr.is_null());
        let s = unsafe { std::ffi::CStr::from_ptr(ptr) }.to_string_lossy().to_string();
        unsafe { drop(CString::from_raw(ptr)) };
        s
    }

    #[test]
    fn insert_placeholder_rules() {
        let h = crate::document::roe_new_document(3);
        for k in ["title", "body", "date", "footer", "slide_number"] {
            let c = std::ffi::CString::new(k).unwrap();
            let r = roe_impress_master_insert_placeholder(h, 0, 1, c.as_ptr());
            if r >= 0 {
                assert_eq!(roe_impress_master_insert_placeholder(h, 0, 1, c.as_ptr()), -1, "重复插入 {k} 应被拒绝");
            }
        }
        let list = ph_list(h, 1);
        for n in ["title", "body", "dt", "ftr", "sldNum"] {
            assert!(list.contains(&format!("\"{}\"", n)), "缺少 {n}: {list}");
        }
        let bad = std::ffi::CString::new("nope").unwrap();
        assert_eq!(roe_impress_master_insert_placeholder(h, 0, 1, bad.as_ptr()), -1);
        let t = std::ffi::CString::new("title").unwrap();
        assert_eq!(roe_impress_master_insert_placeholder(h, 0, 999, t.as_ptr()), -1);
    }
}


#[cfg(test)]
mod tests_v320o {
    use super::*;

    #[test]
    fn master_rotation_set_and_query() {
        let h = crate::document::roe_new_document(3);
        let ids = unsafe {
            let p = roe_impress_master_shape_ids(h, 0, -1);
            assert!(!p.is_null());
            let s = std::ffi::CStr::from_ptr(p).to_string_lossy().to_string();
            drop(CString::from_raw(p));
            s
        };
        let first: Option<u32> = ids.trim_matches(|c| c == '[' || c == ']').split(',').next().and_then(|x| x.trim().parse().ok());
        if let Some(id) = first {
            assert!(roe_impress_master_set_shape_rotation(h, 0, -1, id, 30.0));
            let mut rot = None;
            let _ = handle::with_entry(h, |e| {
                if let RoeDocument::Impress(p) = &e.doc {
                    rot = p.master_slides[0].shapes.iter().find(|s| s.id == id).and_then(|s| s.position.rotation);
                }
                Ok::<(), String>(())
            });
            assert_eq!(rot, Some(30.0));
        }
        assert!(!roe_impress_master_set_shape_rotation(h, 0, -1, 99999, 10.0));
        assert!(!roe_impress_master_set_shape_rotation(h, 9, -1, 1, 10.0));
    }
}
