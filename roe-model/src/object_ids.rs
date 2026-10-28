//! 浮动对象(图表/统计图/图片/图形)的统一 id 分配。
//!
//! 规则:同一 sheet 内共用 id 空间;已有非零 id 保留(图表优先);
//! 其余按 图片 -> 图形 的文档顺序依次分配;计数器 Sheet.next_object_id
//! 只增不减,因此删除对象后 id 不会被复用。

use std::collections::HashSet;

use crate::calc::Sheet;

/// 给 sheet 里 id 为 0 或重复的图片、图形分配新 id。可重复调用(幂等)。
/// 图表/统计图的 id 不改动(为 0 的图表保持原样,由图表自己的路径处理)。
pub fn ensure_object_ids(sheet: &mut Sheet) {
    let mut used: HashSet<u64> = HashSet::new();
    let mut max_id = sheet.next_object_id;
    for c in &sheet.floating_charts {
        if c.id != 0 { used.insert(c.id); max_id = max_id.max(c.id); }
    }
    for c in &sheet.floating_stat_charts {
        if c.id != 0 { used.insert(c.id); max_id = max_id.max(c.id); }
    }
    for i in &sheet.floating_images { max_id = max_id.max(i.id); }
    for s in &sheet.floating_shapes { max_id = max_id.max(s.id); }

    let mut next = max_id;
    for i in sheet.floating_images.iter_mut() {
        if i.id == 0 || used.contains(&i.id) {
            next += 1;
            i.id = next;
        }
        used.insert(i.id);
    }
    for s in sheet.floating_shapes.iter_mut() {
        if s.id == 0 || used.contains(&s.id) {
            next += 1;
            s.id = next;
        }
        used.insert(s.id);
    }
    sheet.next_object_id = next;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calc::{FloatingImage, FloatingShape};

    fn img() -> FloatingImage {
        FloatingImage {
            id: 0,
            data_base64: String::new(),
            mime: "image/png".to_string(),
            width_pt: 10.0, height_pt: 10.0, x_pt: 0.0, y_pt: 0.0,
            rot: 0.0, hidden: false,
        }
    }

    fn shape() -> FloatingShape {
        FloatingShape { stroke: None,
            id: 0,
            x_pt: 0.0, y_pt: 0.0, width_pt: 10.0, height_pt: 10.0,
            rot: 0.0, flip_v: false,
            geometry: crate::common::ShapeGeometry::Rect,
            fill: None,
            text_content: vec![],
        }
    }

    #[test]
    fn assigns_images_then_shapes_in_order() {
        let mut sh = Sheet::default();
        sh.floating_images = vec![img(), img()];
        sh.floating_shapes = vec![shape()];
        ensure_object_ids(&mut sh);
        assert_eq!(sh.floating_images[0].id, 1);
        assert_eq!(sh.floating_images[1].id, 2);
        assert_eq!(sh.floating_shapes[0].id, 3);
        assert_eq!(sh.next_object_id, 3);
    }

    #[test]
    fn idempotent_and_never_reuses_after_delete() {
        let mut sh = Sheet::default();
        sh.floating_images = vec![img(), img()];
        ensure_object_ids(&mut sh);
        ensure_object_ids(&mut sh);
        assert_eq!(sh.floating_images[1].id, 2);
        sh.floating_images.pop(); // 删掉最大 id
        sh.floating_images.push(img());
        ensure_object_ids(&mut sh);
        assert_eq!(sh.floating_images[1].id, 3, "删除后 id 不应复用");
    }

    #[test]
    fn duplicate_ids_are_reassigned() {
        let mut sh = Sheet::default();
        let mut a = img(); a.id = 5;
        let mut b = img(); b.id = 5;
        sh.floating_images = vec![a, b];
        ensure_object_ids(&mut sh);
        assert_eq!(sh.floating_images[0].id, 5);
        assert_eq!(sh.floating_images[1].id, 6);
    }
}
