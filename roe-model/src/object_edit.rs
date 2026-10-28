//! 浮动对象(图表/统计图/图片/图形)的写操作公共件,供 C 导出与 Rhai 共用。

use crate::calc::Sheet;

/// 按 id 设置对象的位置与尺寸(pt,原点为数据区左上角)。
/// 查找顺序:图表 -> 统计图 -> 图片 -> 图形(共用 id 空间)。旋转角不变。
pub fn set_object_rect(sheet: &mut Sheet, id: u64, x: f32, y: f32, w: f32, h: f32) -> Result<(), String> {
    if id == 0 {
        return Err("id 必须是正整数".to_string());
    }
    if ![x, y, w, h].iter().all(|v| v.is_finite()) {
        return Err("位置和尺寸必须是有限数字".to_string());
    }
    if x < 0.0 || y < 0.0 {
        return Err(format!("x_pt/y_pt 不能为负(收到 {x}, {y})"));
    }
    if w <= 0.0 || h <= 0.0 {
        return Err(format!("w_pt/h_pt 必须大于 0(收到 {w}, {h})"));
    }
    if let Some(c) = sheet.floating_charts.iter_mut().find(|c| c.id == id) {
        c.x_pt = x; c.y_pt = y; c.width_pt = w; c.height_pt = h;
        return Ok(());
    }
    if let Some(c) = sheet.floating_stat_charts.iter_mut().find(|c| c.id == id) {
        c.x_pt = x; c.y_pt = y; c.width_pt = w; c.height_pt = h;
        return Ok(());
    }
    if let Some(i) = sheet.floating_images.iter_mut().find(|i| i.id == id) {
        i.x_pt = x; i.y_pt = y; i.width_pt = w; i.height_pt = h;
        return Ok(());
    }
    if let Some(s) = sheet.floating_shapes.iter_mut().find(|s| s.id == id) {
        rescale_shape_path(s, w, h);
        s.x_pt = x; s.y_pt = y; s.width_pt = w; s.height_pt = h;
        return Ok(());
    }
    Err(format!("对象 id={id} 不存在"))
}

/// 按 id 删除浮动对象(图表/统计图/图片/图形)。删除了返回 true,id 不存在返回 false。
/// 只删除,不碰 next_object_id,所以 id 不会被复用。
pub fn delete_object(sheet: &mut Sheet, id: u64) -> bool {
    if id == 0 { return false; }
    let total = |s: &Sheet| s.floating_charts.len() + s.floating_stat_charts.len()
        + s.floating_images.len() + s.floating_shapes.len();
    let before = total(sheet);
    sheet.floating_charts.retain(|c| c.id != id);
    sheet.floating_stat_charts.retain(|c| c.id != id);
    sheet.floating_images.retain(|i| i.id != id);
    sheet.floating_shapes.retain(|s| s.id != id);
    total(sheet) != before
}

/// 图片来源解码后的字节数上限。
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
/// 宽高都传 0 时采用的默认宽度(pt)。
pub const DEFAULT_IMAGE_WIDTH_PT: f32 = 200.0;

/// 分配新的浮动对象 id:取 max(计数器, 所有现有对象 id) + 1,并写回计数器。
/// 图表/统计图/图片/图形共用 id 空间;计数器只增不减,删除后 id 不复用。
pub fn alloc_object_id(sheet: &mut Sheet) -> u64 {
    let mut m = sheet.next_object_id;
    for c in &sheet.floating_charts { m = m.max(c.id); }
    for c in &sheet.floating_stat_charts { m = m.max(c.id); }
    for i in &sheet.floating_images { m = m.max(i.id); }
    for s in &sheet.floating_shapes { m = m.max(s.id); }
    let id = m + 1;
    sheet.next_object_id = id;
    id
}

/// 按文件头识别 PNG/JPEG,并读出像素宽高。返回 (mime, 宽px, 高px)。
pub fn sniff_image(b: &[u8]) -> Option<(&'static str, u32, u32)> {
    const PNG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if b.len() >= 24 && b[..8] == PNG {
        let w = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);
        let h = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
        return Some(("image/png", w, h));
    }
    if b.len() >= 4 && b[0] == 0xFF && b[1] == 0xD8 {
        let mut i = 2usize;
        while i + 4 <= b.len() {
            if b[i] != 0xFF { return None; }
            let m = b[i + 1];
            if m == 0xFF { i += 1; continue; }
            if m == 0xD8 || m == 0x01 || (0xD0..=0xD7).contains(&m) { i += 2; continue; }
            if m == 0xD9 || m == 0xDA { return None; }
            let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            let is_sof = (0xC0..=0xCF).contains(&m) && m != 0xC4 && m != 0xC8 && m != 0xCC;
            if is_sof {
                if i + 9 > b.len() { return None; }
                let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
                let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
                return Some(("image/jpeg", w, h));
            }
            if len < 2 { return None; }
            i += 2 + len;
        }
    }
    None
}

/// 解析图片来源并取出字节:"base64:..." / "data:image/xxx;base64,..." 按 base64,否则当本地路径。
fn load_image_source(source: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let s = source.trim();
    if s.is_empty() {
        return Err("source 为空".to_string());
    }
    let b64: Option<&str> = if let Some(r) = s.strip_prefix("base64:") {
        Some(r)
    } else if s.starts_with("data:image/") {
        let comma = s.find(',').ok_or_else(|| "data:image 格式不对:缺少逗号".to_string())?;
        if !s[..comma].contains(";base64") {
            return Err("data:image 必须是 ;base64 编码".to_string());
        }
        Some(&s[comma + 1..])
    } else {
        None
    };
    if let Some(data) = b64 {
        let cleaned: String = data.chars().filter(|c| !c.is_whitespace()).collect();
        if cleaned.len() / 4 * 3 > MAX_IMAGE_BYTES {
            return Err(format!("图片过大(上限 {} MB)", MAX_IMAGE_BYTES / 1024 / 1024));
        }
        base64::engine::general_purpose::STANDARD
            .decode(cleaned.as_bytes())
            .map_err(|e| format!("base64 解码失败: {e}"))
    } else {
        let meta = std::fs::metadata(s).map_err(|e| format!("无法读取文件 {s}: {e}"))?;
        if meta.len() as usize > MAX_IMAGE_BYTES {
            return Err(format!("图片过大(上限 {} MB)", MAX_IMAGE_BYTES / 1024 / 1024));
        }
        std::fs::read(s).map_err(|e| format!("无法读取文件 {s}: {e}"))
    }
}

/// 插入浮动图片,返回新对象 id。x/y 为 pt(原点为数据区左上角)。
/// w/h 都为 0:宽取 200pt、高按原比例;只有一边为 0:另一边按原比例算;都大于 0:照用。
pub fn insert_image(sheet: &mut Sheet, source: &str, x: f32, y: f32, w: f32, h: f32) -> Result<u64, String> {
    use base64::Engine as _;
    if ![x, y, w, h].iter().all(|v| v.is_finite()) {
        return Err("位置和尺寸必须是有限数字".to_string());
    }
    if x < 0.0 || y < 0.0 {
        return Err(format!("x_pt/y_pt 不能为负(收到 {x}, {y})"));
    }
    if w < 0.0 || h < 0.0 {
        return Err(format!("w_pt/h_pt 不能为负(收到 {w}, {h});传 0.0 表示按原比例"));
    }
    let bytes = load_image_source(source)?;
    if bytes.is_empty() {
        return Err("图片数据为空".to_string());
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!("图片过大(上限 {} MB)", MAX_IMAGE_BYTES / 1024 / 1024));
    }
    let (mime, pw, ph) = sniff_image(&bytes)
        .ok_or_else(|| "不是 PNG/JPEG 图片(按文件头识别)".to_string())?;
    if pw == 0 || ph == 0 {
        return Err("图片像素尺寸为 0".to_string());
    }
    let ratio = ph as f32 / pw as f32;
    let (fw, fh) = if w > 0.0 && h > 0.0 {
        (w, h)
    } else if w > 0.0 {
        (w, w * ratio)
    } else if h > 0.0 {
        (h / ratio, h)
    } else {
        (DEFAULT_IMAGE_WIDTH_PT, DEFAULT_IMAGE_WIDTH_PT * ratio)
    };
    if !(fw.is_finite() && fh.is_finite() && fw > 0.0 && fh > 0.0) {
        return Err("按原比例算出的尺寸无效,请显式给出宽和高".to_string());
    }
    let id = alloc_object_id(sheet);
    sheet.floating_images.push(crate::calc::FloatingImage {
        id,
        data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        mime: mime.to_string(),
        width_pt: fw,
        height_pt: fh,
        x_pt: x,
        y_pt: y,
        rot: 0.0,
        hidden: false,
    });
    Ok(id)
}

fn parse_fill_hex(s: &str) -> Result<Option<crate::common::FillStyle>, String> {
    let t = s.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    let h = t.trim_start_matches('#');
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("fill 必须是 #rrggbb 或空串(收到 {s})"));
    }
    let p = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0);
    Ok(Some(crate::common::FillStyle::Solid(crate::common::Color { r: p(0), g: p(2), b: p(4), a: 255 })))
}

/// 图形改尺寸时让预设路径跟着变:有预设名按新宽高重新求值(保留 adj 覆盖值),
/// 没有预设名的自定义路径按新旧宽高比例缩放坐标。必须在写入新宽高之前调用。
fn rescale_shape_path(s: &mut crate::calc::FloatingShape, w: f32, h: f32) {
    use crate::common::{PathCommand, ShapeGeometry};
    let (ow, oh) = (s.width_pt, s.height_pt);
    if let ShapeGeometry::PresetPath(d) = &mut s.geometry {
        if !d.preset_name.is_empty() {
            if let Some(formula) = crate::preset_geometry::find_preset_shape(&d.preset_name) {
                let adj: std::collections::HashMap<&str, f64> =
                    d.adj_overrides.iter().map(|(k, v)| (k.as_str(), *v)).collect();
                d.commands = crate::preset_geometry::eval_preset_shape(formula, w as f64, h as f64, &adj);
                return;
            }
        }
        if ow > 0.0 && oh > 0.0 {
            let (kx, ky) = (w / ow, h / oh);
            for c in d.commands.iter_mut() {
                match c {
                    PathCommand::MoveTo { x, y } | PathCommand::LineTo { x, y } => { *x *= kx; *y *= ky; }
                    PathCommand::CubicTo { x1, y1, x2, y2, x, y } => {
                        *x1 *= kx; *y1 *= ky; *x2 *= kx; *y2 *= ky; *x *= kx; *y *= ky;
                    }
                    PathCommand::QuadTo { x1, y1, x, y } => { *x1 *= kx; *y1 *= ky; *x *= kx; *y *= ky; }
                    PathCommand::ArcTo { cx, cy, rx, ry, .. } => { *cx *= kx; *cy *= ky; *rx *= kx; *ry *= ky; }
                    PathCommand::Close => {}
                }
            }
        }
    }
}

fn parse_shape_geometry(name: &str, w: f32, h: f32) -> Result<crate::common::ShapeGeometry, String> {
    use crate::common::{PresetPathData, ShapeGeometry};
    match name.trim() {
        "rect" => Ok(ShapeGeometry::Rect),
        "ellipse" => Ok(ShapeGeometry::Ellipse),
        "line" => Ok(ShapeGeometry::Line),
        other => match crate::preset_geometry::find_preset_shape(other) {
            Some(formula) => {
                let empty: std::collections::HashMap<&str, f64> = std::collections::HashMap::new();
                let path = crate::preset_geometry::eval_preset_shape(formula, w as f64, h as f64, &empty);
                Ok(ShapeGeometry::PresetPath(PresetPathData {
                    commands: path,
                    preset_name: other.to_string(),
                    adj_overrides: Vec::new(),
                }))
            }
            None => Err(format!("未知图形预设名 {other}(可用 rect/ellipse/line 或 OOXML 预设名,如 heart)")),
        },
    }
}

fn check_rect(x: f32, y: f32, w: f32, h: f32) -> Result<(), String> {
    if ![x, y, w, h].iter().all(|v| v.is_finite()) {
        return Err("位置和尺寸必须是有限数字".to_string());
    }
    if x < 0.0 || y < 0.0 {
        return Err(format!("x_pt/y_pt 不能为负(收到 {x}, {y})"));
    }
    if w <= 0.0 || h <= 0.0 {
        return Err(format!("w_pt/h_pt 必须大于 0(收到 {w}, {h})"));
    }
    Ok(())
}

/// 文字上限(字符数)。
pub const MAX_OBJECT_TEXT_CHARS: usize = 100_000;

/// 把纯文本按 \n 分段;保留模板段落/首个 run 的格式。空文本得到空 Vec。
fn build_paragraphs(text: &str, tpl: Option<&crate::writer::Paragraph>) -> Vec<crate::writer::Paragraph> {
    let norm = text.replace("\r\n", "\n");
    if norm.is_empty() {
        return Vec::new();
    }
    let tpl_p = tpl.cloned().unwrap_or_default();
    let run_tpl = tpl_p.runs.first().cloned().unwrap_or_default();
    norm.split('\n').map(|line| {
        let mut p = tpl_p.clone();
        let mut r = run_tpl.clone();
        r.text = line.to_string();
        p.runs = vec![r];
        p
    }).collect()
}

fn check_text(text: &str) -> Result<(), String> {
    if text.chars().count() > MAX_OBJECT_TEXT_CHARS {
        return Err(format!("文字过长(上限 {MAX_OBJECT_TEXT_CHARS} 字符)"));
    }
    Ok(())
}

/// 插入浮动图形,返回新对象 id。fill 为 "#rrggbb" 或空串(无填充)。文字用 set_object_text 另设。
pub fn insert_shape(sheet: &mut Sheet, geometry: &str, x: f32, y: f32, w: f32, h: f32, fill: &str) -> Result<u64, String> {
    check_rect(x, y, w, h)?;
    let geo = parse_shape_geometry(geometry, w, h)?;
    let fill = parse_fill_hex(fill)?;
    let id = alloc_object_id(sheet);
    sheet.floating_shapes.push(crate::calc::FloatingShape { stroke: None,
        id, x_pt: x, y_pt: y, width_pt: w, height_pt: h, rot: 0.0, flip_v: false,
        geometry: geo, fill, text_content: Vec::new(),
    });
    Ok(id)
}

/// 插入文本框(无填充的矩形图形,带文字),返回新对象 id。
pub fn insert_textbox(sheet: &mut Sheet, x: f32, y: f32, w: f32, h: f32, text: &str) -> Result<u64, String> {
    check_rect(x, y, w, h)?;
    check_text(text)?;
    let id = alloc_object_id(sheet);
    sheet.floating_shapes.push(crate::calc::FloatingShape { stroke: None,
        id, x_pt: x, y_pt: y, width_pt: w, height_pt: h, rot: 0.0, flip_v: false,
        geometry: crate::common::ShapeGeometry::Rect,
        fill: Some(crate::common::FillStyle::Solid(crate::common::Color { r: 255, g: 255, b: 255, a: 255 })),
        text_content: build_paragraphs(text, None),
    });
    Ok(id)
}

/// 整体替换图形/文本框的文字(\n 分段,保留首段与首个 run 的格式)。只对图形有效。
pub fn set_object_text(sheet: &mut Sheet, id: u64, text: &str) -> Result<(), String> {
    if id == 0 {
        return Err("id 必须是正整数".to_string());
    }
    check_text(text)?;
    if let Some(s) = sheet.floating_shapes.iter_mut().find(|s| s.id == id) {
        let paras = build_paragraphs(text, s.text_content.first());
        s.text_content = paras;
        return Ok(());
    }
    let other = sheet.floating_charts.iter().any(|c| c.id == id)
        || sheet.floating_stat_charts.iter().any(|c| c.id == id)
        || sheet.floating_images.iter().any(|i| i.id == id);
    if other {
        Err(format!("对象 id={id} 是图表或图片,没有文字可设置(只有图形/文本框可以)"))
    } else {
        Err(format!("对象 id={id} 不存在"))
    }
}

/// 按 id 取对象矩形 (x, y, w, h),单位 pt。图表/统计图/图片/图形都支持;不存在返回 None。
pub fn get_object_rect(sheet: &Sheet, id: u64) -> Option<(f32, f32, f32, f32)> {
    if id == 0 { return None; }
    if let Some(c) = sheet.floating_charts.iter().find(|c| c.id == id) {
        return Some((c.x_pt, c.y_pt, c.width_pt, c.height_pt));
    }
    if let Some(c) = sheet.floating_stat_charts.iter().find(|c| c.id == id) {
        return Some((c.x_pt, c.y_pt, c.width_pt, c.height_pt));
    }
    if let Some(i) = sheet.floating_images.iter().find(|i| i.id == id) {
        return Some((i.x_pt, i.y_pt, i.width_pt, i.height_pt));
    }
    sheet.floating_shapes.iter().find(|s| s.id == id)
        .map(|s| (s.x_pt, s.y_pt, s.width_pt, s.height_pt))
}

/// 撤销/重做/丢弃编辑换回旧文档后调用:每个 sheet 的 next_object_id 取
/// max(被换下来的文档, 当前文档),保证计数器不回滚、id 不被复用。
/// 按 sheet 下标一一对应;下标超出的部分忽略。
pub fn merge_next_object_ids(replaced: &[Sheet], current: &mut [Sheet]) {
    for (old, cur) in replaced.iter().zip(current.iter_mut()) {
        if old.next_object_id > cur.next_object_id {
            cur.next_object_id = old.next_object_id;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calc::{FloatingImage, FloatingShape};
    use crate::common::ShapeGeometry;
    use crate::object_ids::ensure_object_ids;

    fn sheet_with_two() -> Sheet {
        let mut sh = Sheet::default();
        sh.floating_images.push(FloatingImage {
            id: 0, data_base64: String::new(), mime: "image/png".into(),
            width_pt: 100.0, height_pt: 50.0, x_pt: 10.0, y_pt: 20.0, rot: 0.0, hidden: false,
        });
        sh.floating_shapes.push(FloatingShape { stroke: None,
            id: 0, x_pt: 1.0, y_pt: 2.0, width_pt: 30.0, height_pt: 40.0, rot: 90.0, flip_v: false,
            geometry: ShapeGeometry::Ellipse, fill: None, text_content: vec![],
        });
        ensure_object_ids(&mut sh);
        sh
    }

    #[test]
    fn set_fill_and_clear_on_shape() {
        let mut sh = sheet_with_two();
        set_object_fill(&mut sh, 2, "#ff8000").unwrap();
        match &sh.floating_shapes[0].fill {
            Some(crate::common::FillStyle::Solid(c)) => assert_eq!((c.r, c.g, c.b), (255, 128, 0)),
            other => panic!("填充不对: {other:?}"),
        }
        set_object_fill(&mut sh, 2, "none").unwrap();
        assert!(sh.floating_shapes[0].fill.is_none());
    }

    #[test]
    fn set_stroke_and_clear_on_shape() {
        let mut sh = sheet_with_two();
        set_object_stroke(&mut sh, 2, "#333333", 1.5).unwrap();
        let st = sh.floating_shapes[0].stroke.as_ref().unwrap();
        assert_eq!((st.color.r, st.width), (0x33, 1.5));
        set_object_stroke(&mut sh, 2, "", 1.0).unwrap();
        assert!(sh.floating_shapes[0].stroke.is_none());
    }

    #[test]
    fn fill_stroke_reject_image_missing_and_bad_args() {
        let mut sh = sheet_with_two();
        assert!(set_object_fill(&mut sh, 1, "#ff0000").unwrap_err().contains("图片"));
        assert!(set_object_stroke(&mut sh, 1, "#ff0000", 1.0).unwrap_err().contains("图片"));
        assert!(set_object_fill(&mut sh, 99, "#ff0000").unwrap_err().contains("不存在"));
        assert!(set_object_fill(&mut sh, 0, "#ff0000").is_err());
        assert!(set_object_fill(&mut sh, 2, "red").is_err());
        assert!(set_object_stroke(&mut sh, 2, "#000000", 0.0).is_err());
        assert!(set_object_stroke(&mut sh, 2, "#000000", 101.0).is_err());
        assert!(set_object_stroke(&mut sh, 2, "#000000", f32::NAN).is_err());
    }

    #[test]
    fn set_rect_on_image_and_shape() {
        let mut sh = sheet_with_two();
        set_object_rect(&mut sh, 1, 5.0, 6.0, 70.0, 80.0).unwrap();
        set_object_rect(&mut sh, 2, 7.0, 8.0, 90.0, 60.0).unwrap();
        let i = &sh.floating_images[0];
        assert_eq!((i.x_pt, i.y_pt, i.width_pt, i.height_pt), (5.0, 6.0, 70.0, 80.0));
        let s = &sh.floating_shapes[0];
        assert_eq!((s.x_pt, s.y_pt, s.width_pt, s.height_pt), (7.0, 8.0, 90.0, 60.0));
        assert_eq!(s.rot, 90.0);
    }

    #[test]
    fn set_rect_rejects_bad_input() {
        let mut sh = sheet_with_two();
        assert!(set_object_rect(&mut sh, 99, 1.0, 1.0, 10.0, 10.0).unwrap_err().contains("不存在"));
        assert!(set_object_rect(&mut sh, 0, 1.0, 1.0, 10.0, 10.0).is_err());
        assert!(set_object_rect(&mut sh, 1, -1.0, 1.0, 10.0, 10.0).is_err());
        assert!(set_object_rect(&mut sh, 1, 1.0, 1.0, 0.0, 10.0).is_err());
        assert!(set_object_rect(&mut sh, 1, f32::NAN, 1.0, 10.0, 10.0).is_err());
        let i = &sh.floating_images[0];
        assert_eq!((i.x_pt, i.width_pt), (10.0, 100.0));
    }

    #[test]
    fn merge_takes_max_per_sheet() {
        let mut old = vec![Sheet::default(), Sheet::default()];
        old[0].next_object_id = 9;
        old[1].next_object_id = 2;
        let mut cur = vec![Sheet::default(), Sheet::default()];
        cur[0].next_object_id = 4;
        cur[1].next_object_id = 5;
        merge_next_object_ids(&old, &mut cur);
        assert_eq!(cur[0].next_object_id, 9);
        assert_eq!(cur[1].next_object_id, 5);
    }
}

#[cfg(test)]
mod delete_tests {
    use super::*;
    use crate::calc::{FloatingImage, FloatingShape};
    use crate::common::ShapeGeometry;
    use crate::object_ids::ensure_object_ids;

    #[test]
    fn delete_removes_and_never_reuses_id() {
        let mut sh = Sheet::default();
        sh.floating_images.push(FloatingImage {
            id: 0, data_base64: String::new(), mime: "image/png".into(),
            width_pt: 10.0, height_pt: 10.0, x_pt: 0.0, y_pt: 0.0, rot: 0.0, hidden: false,
        });
        sh.floating_shapes.push(FloatingShape { stroke: None,
            id: 0, x_pt: 1.0, y_pt: 2.0, width_pt: 30.0, height_pt: 40.0, rot: 0.0, flip_v: false,
            geometry: ShapeGeometry::Ellipse, fill: None, text_content: vec![],
        });
        ensure_object_ids(&mut sh);
        assert_eq!(sh.next_object_id, 2);
        assert!(delete_object(&mut sh, 1));
        assert!(sh.floating_images.is_empty());
        assert_eq!(sh.floating_shapes.len(), 1);
        assert!(!delete_object(&mut sh, 1));
        assert!(!delete_object(&mut sh, 0));
        assert!(delete_object(&mut sh, 2));
        assert_eq!(sh.next_object_id, 2, "删除不得动计数器");
    }
}

#[cfg(test)]
mod insert_tests {
    use super::*;
    use base64::Engine as _;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0]);
        v
    }

    fn jpeg(w: u16, h: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 0, 0, 0xFF, 0xC0, 0, 0x11, 8];
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&[3, 0, 0, 0]);
        v
    }

    fn b64(v: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(v)
    }

    #[test]
    fn sniff_png_jpeg_and_garbage() {
        assert_eq!(sniff_image(&png(400, 200)), Some(("image/png", 400, 200)));
        assert_eq!(sniff_image(&jpeg(300, 150)), Some(("image/jpeg", 300, 150)));
        assert_eq!(sniff_image(b"hello world, not an image at all"), None);
        assert_eq!(sniff_image(&[]), None);
        assert_eq!(sniff_image(&[0xFF, 0xD8, 0xFF]), None);
    }

    #[test]
    fn default_size_keeps_ratio() {
        let mut sh = Sheet::default();
        let id = insert_image(&mut sh, &format!("base64:{}", b64(&png(400, 200))), 10.0, 20.0, 0.0, 0.0).unwrap();
        assert_eq!(id, 1);
        let i = &sh.floating_images[0];
        assert_eq!((i.width_pt, i.height_pt, i.x_pt, i.y_pt), (200.0, 100.0, 10.0, 20.0));
        assert_eq!(i.mime, "image/png");
    }

    #[test]
    fn one_side_or_both_given() {
        let mut sh = Sheet::default();
        let s = format!("base64:{}", b64(&jpeg(300, 150)));
        insert_image(&mut sh, &s, 0.0, 0.0, 60.0, 0.0).unwrap();
        insert_image(&mut sh, &s, 0.0, 0.0, 0.0, 30.0).unwrap();
        insert_image(&mut sh, &s, 0.0, 0.0, 80.0, 80.0).unwrap();
        let v = &sh.floating_images;
        assert_eq!((v[0].width_pt, v[0].height_pt), (60.0, 30.0));
        assert_eq!((v[1].width_pt, v[1].height_pt), (60.0, 30.0));
        assert_eq!((v[2].width_pt, v[2].height_pt), (80.0, 80.0));
        assert_eq!(v[0].mime, "image/jpeg");
    }

    #[test]
    fn data_uri_and_path_sources() {
        let mut sh = Sheet::default();
        let uri = format!("data:image/png;base64,{}", b64(&png(100, 100)));
        insert_image(&mut sh, &uri, 0.0, 0.0, 0.0, 0.0).unwrap();
        let p = std::env::temp_dir().join("roe_object_edit_insert_test.png");
        std::fs::write(&p, png(50, 100)).unwrap();
        insert_image(&mut sh, p.to_str().unwrap(), 0.0, 0.0, 0.0, 0.0).unwrap();
        let _ = std::fs::remove_file(&p);
        assert_eq!((sh.floating_images[1].width_pt, sh.floating_images[1].height_pt), (200.0, 400.0));
    }

    #[test]
    fn rejects_bad_input_and_changes_nothing() {
        let mut sh = Sheet::default();
        let e = insert_image(&mut sh, &format!("base64:{}", b64(b"hello world, not an image at all")), 0.0, 0.0, 0.0, 0.0).unwrap_err();
        assert!(e.contains("PNG/JPEG"), "{e}");
        assert!(insert_image(&mut sh, "base64:!!!不是base64", 0.0, 0.0, 0.0, 0.0).unwrap_err().contains("解码"));
        let big = format!("base64:{}", "A".repeat(15_000_000));
        assert!(insert_image(&mut sh, &big, 0.0, 0.0, 0.0, 0.0).unwrap_err().contains("过大"));
        let ok = format!("base64:{}", b64(&png(10, 10)));
        assert!(insert_image(&mut sh, &ok, -1.0, 0.0, 0.0, 0.0).unwrap_err().contains("不能为负"));
        assert!(insert_image(&mut sh, &ok, 0.0, 0.0, -5.0, 0.0).unwrap_err().contains("不能为负"));
        assert!(insert_image(&mut sh, "/不存在/的/文件.png", 0.0, 0.0, 0.0, 0.0).is_err());
        assert!(insert_image(&mut sh, "", 0.0, 0.0, 0.0, 0.0).is_err());
        assert!(sh.floating_images.is_empty());
        assert_eq!(sh.next_object_id, 0, "失败不得消耗 id");
    }

    #[test]
    fn ids_are_monotonic_after_delete() {
        let mut sh = Sheet::default();
        let s = format!("base64:{}", b64(&png(10, 10)));
        assert_eq!(insert_image(&mut sh, &s, 0.0, 0.0, 0.0, 0.0).unwrap(), 1);
        assert_eq!(insert_image(&mut sh, &s, 0.0, 0.0, 0.0, 0.0).unwrap(), 2);
        assert!(delete_object(&mut sh, 2));
        assert_eq!(insert_image(&mut sh, &s, 0.0, 0.0, 0.0, 0.0).unwrap(), 3);
    }
}

#[cfg(test)]
mod shape_tests {
    use super::*;

    fn text_of(sh: &Sheet, idx: usize) -> String {
        sh.floating_shapes[idx].text_content.iter()
            .map(|p| p.runs.iter().map(|r| r.text.as_str()).collect::<String>())
            .collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn insert_shape_geometry_and_fill() {
        let mut sh = Sheet::default();
        let a = insert_shape(&mut sh, "ellipse", 1.0, 2.0, 30.0, 40.0, "#ff0010").unwrap();
        let b = insert_shape(&mut sh, "heart", 0.0, 0.0, 50.0, 50.0, "").unwrap();
        assert_eq!((a, b), (1, 2));
        assert!(matches!(sh.floating_shapes[0].geometry, crate::common::ShapeGeometry::Ellipse));
        match &sh.floating_shapes[0].fill {
            Some(crate::common::FillStyle::Solid(c)) => assert_eq!((c.r, c.g, c.b), (255, 0, 16)),
            _ => panic!("应为纯色填充"),
        }
        assert!(sh.floating_shapes[1].fill.is_none());
        match &sh.floating_shapes[1].geometry {
            crate::common::ShapeGeometry::PresetPath(d) => assert_eq!(d.preset_name, "heart"),
            _ => panic!("heart 应为 PresetPath"),
        }
    }

    #[test]
    fn insert_shape_rejects_bad_input_and_changes_nothing() {
        let mut sh = Sheet::default();
        assert!(insert_shape(&mut sh, "notashape", 0.0, 0.0, 10.0, 10.0, "").unwrap_err().contains("未知图形预设名"));
        assert!(insert_shape(&mut sh, "rect", 0.0, 0.0, 10.0, 10.0, "red").unwrap_err().contains("fill"));
        assert!(insert_shape(&mut sh, "rect", -1.0, 0.0, 10.0, 10.0, "").is_err());
        assert!(insert_shape(&mut sh, "rect", 0.0, 0.0, 0.0, 10.0, "").is_err());
        assert!(sh.floating_shapes.is_empty());
        assert_eq!(sh.next_object_id, 0, "失败不得消耗 id");
    }

    #[test]
    fn textbox_and_set_text_keep_format() {
        let mut sh = Sheet::default();
        let id = insert_textbox(&mut sh, 5.0, 6.0, 100.0, 40.0, "甲\n乙").unwrap();
        assert_eq!(text_of(&sh, 0), "甲\n乙");
        assert_eq!(sh.floating_shapes[0].text_content.len(), 2);
        sh.floating_shapes[0].text_content[0].runs[0].bold = Some(true);
        set_object_text(&mut sh, id, "丙\r\n丁\n戊").unwrap();
        assert_eq!(text_of(&sh, 0), "丙\n丁\n戊");
        assert_eq!(sh.floating_shapes[0].text_content[2].runs[0].bold, Some(true));
        set_object_text(&mut sh, id, "").unwrap();
        assert!(sh.floating_shapes[0].text_content.is_empty());
    }

    #[test]
    fn set_text_errors() {
        let mut sh = Sheet::default();
        sh.floating_images.push(crate::calc::FloatingImage {
            id: 0, data_base64: String::new(), mime: "image/png".into(),
            width_pt: 10.0, height_pt: 10.0, x_pt: 0.0, y_pt: 0.0, rot: 0.0, hidden: false,
        });
        crate::object_ids::ensure_object_ids(&mut sh);
        assert!(set_object_text(&mut sh, 1, "x").unwrap_err().contains("没有文字"));
        assert!(set_object_text(&mut sh, 99, "x").unwrap_err().contains("不存在"));
        assert!(set_object_text(&mut sh, 0, "x").is_err());
    }
}

#[cfg(test)]
mod rect_get_tests {
    use super::*;

    #[test]
    fn get_rect_for_image_and_shape() {
        let mut sh = Sheet::default();
        let a = insert_shape(&mut sh, "rect", 1.0, 2.0, 30.0, 40.0, "").unwrap();
        assert_eq!(get_object_rect(&sh, a), Some((1.0, 2.0, 30.0, 40.0)));
        assert_eq!(get_object_rect(&sh, 99), None);
        assert_eq!(get_object_rect(&sh, 0), None);
    }
}

#[cfg(test)]
mod rescale_tests {
    use super::*;
    use crate::calc::Sheet;
    use crate::common::{FillStyle, PathCommand, ShapeGeometry};

    fn max_x(sh: &Sheet, id: u64) -> f32 {
        let s = sh.floating_shapes.iter().find(|s| s.id == id).unwrap();
        match &s.geometry {
            ShapeGeometry::PresetPath(d) => d.commands.iter().map(|c| match c {
                PathCommand::MoveTo { x, .. } | PathCommand::LineTo { x, .. } => *x,
                PathCommand::CubicTo { x, .. } | PathCommand::QuadTo { x, .. } => *x,
                _ => 0.0,
            }).fold(0.0, f32::max),
            _ => 0.0,
        }
    }

    #[test]
    fn preset_path_follows_resize() {
        let mut sh = Sheet::default();
        let id = insert_shape(&mut sh, "heart", 10.0, 10.0, 100.0, 100.0, "").unwrap();
        let before = max_x(&sh, id);
        set_object_rect(&mut sh, id, 10.0, 10.0, 200.0, 50.0).unwrap();
        let after = max_x(&sh, id);
        assert!(after > before * 1.5, "路径应随宽度变大: {before} -> {after}");
    }

    #[test]
    fn textbox_has_white_fill() {
        let mut sh = Sheet::default();
        let id = insert_textbox(&mut sh, 0.0, 0.0, 100.0, 40.0, "").unwrap();
        let s = sh.floating_shapes.iter().find(|s| s.id == id).unwrap();
        assert!(matches!(s.fill, Some(FillStyle::Solid(_))));
    }
}

fn shape_only_err(sheet: &Sheet, id: u64, what: &str) -> Result<(), String> {
    let other = sheet.floating_charts.iter().any(|c| c.id == id)
        || sheet.floating_stat_charts.iter().any(|c| c.id == id)
        || sheet.floating_images.iter().any(|i| i.id == id);
    if other {
        Err(format!("对象 id={id} 是图表或图片,不能设置{what}(只有图形/文本框可以)"))
    } else {
        Err(format!("对象 id={id} 不存在"))
    }
}

/// 设置图形/文本框的填充色。fill 为 #rrggbb;空串或 none 表示无填充。只对图形有效。
pub fn set_object_fill(sheet: &mut Sheet, id: u64, fill: &str) -> Result<(), String> {
    if id == 0 {
        return Err("id 必须是正整数".to_string());
    }
    let f = parse_fill_hex(fill)?;
    if let Some(s) = sheet.floating_shapes.iter_mut().find(|s| s.id == id) {
        s.fill = f;
        return Ok(());
    }
    shape_only_err(sheet, id, "填充")
}

/// 设置图形/文本框的边框。color 为 #rrggbb;空串或 none 表示无边框。width_pt 范围 (0, 100]。只对图形有效。
pub fn set_object_stroke(sheet: &mut Sheet, id: u64, color: &str, width_pt: f32) -> Result<(), String> {
    if id == 0 {
        return Err("id 必须是正整数".to_string());
    }
    let st = match parse_fill_hex(color)? {
        Some(crate::common::FillStyle::Solid(c)) => {
            if !width_pt.is_finite() || width_pt <= 0.0 || width_pt > 100.0 {
                return Err(format!("边框宽度必须在 (0, 100] pt 之间(收到 {width_pt})"));
            }
            Some(crate::common::StrokeStyle { color: c, width: width_pt })
        }
        _ => None,
    };
    if let Some(s) = sheet.floating_shapes.iter_mut().find(|s| s.id == id) {
        s.stroke = st;
        return Ok(());
    }
    shape_only_err(sheet, id, "边框")
}
