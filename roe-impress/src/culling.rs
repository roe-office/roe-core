//! Viewport Culling：用 rstar R-tree 加速 tile 渲染时的对象裁剪
//! 替代线性遍历，只处理与 tile rect 相交的 Shape

use rstar::{RTree, RTreeObject, AABB};
use roe_model::impress::Shape;

/// R-tree 中存储的 Shape 包围盒条目
#[derive(Debug, Clone)]
pub struct ShapeEntry {
    pub shape_id: u32,
    pub envelope: AABB<[f32; 2]>,
}

impl RTreeObject for ShapeEntry {
    type Envelope = AABB<[f32; 2]>;
    fn envelope(&self) -> Self::Envelope {
        self.envelope
    }
}

/// 幻灯片的空间索引
pub struct SlideIndex {
    tree: RTree<ShapeEntry>,
}

impl SlideIndex {
    /// 从 Shape 列表构建 R-tree
    pub fn build(shapes: &[Shape]) -> Self {
        let entries: Vec<ShapeEntry> = shapes.iter().map(|s| {
            let r = &s.position;
            ShapeEntry {
                shape_id: s.id,
                envelope: AABB::from_corners(
                    [r.x(), r.y()],
                    [r.x() + r.w(), r.y() + r.h()],
                ),
            }
        }).collect();

        Self { tree: RTree::bulk_load(entries) }
    }

    /// 查询与给定矩形相交的所有 Shape ID
    pub fn query_intersect(
        &self,
        x: f32, y: f32, w: f32, h: f32,
    ) -> Vec<u32> {
        let envelope = AABB::from_corners([x, y], [x + w, y + h]);
        self.tree
            .locate_in_envelope_intersecting(&envelope)
            .map(|e| e.shape_id)
            .collect()
    }

    pub fn shape_count(&self) -> usize {
        self.tree.size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::common::Transform;
    use roe_model::impress::{Shape, ShapeGeometry, ShapeStyle, ShapeContent};

    fn make_shape(id: u32, x: f32, y: f32, w: f32, h: f32) -> Shape {
        Shape {
            id,
            geometry: ShapeGeometry::Rect,
            position: Transform::from_rect(x, y, w, h),
            style: ShapeStyle::default(),
            content: ShapeContent::Empty,
            text_body: Default::default(),
            st_cxn: None,
            end_cxn: None,
            placeholder: None,
        }
    }

    #[test]
    fn test_build_and_query() {
        let shapes = vec![
            make_shape(1,   0.0,   0.0, 100.0, 100.0),
            make_shape(2, 200.0,   0.0, 100.0, 100.0),
            make_shape(3,   0.0, 200.0, 100.0, 100.0),
            make_shape(4, 500.0, 500.0,  50.0,  50.0),
        ];

        let index = SlideIndex::build(&shapes);
        assert_eq!(index.shape_count(), 4);

        // tile [0,0,150,150]：应命中 shape 1 和 2（2 的 x=200 未相交）
        let mut hits = index.query_intersect(0.0, 0.0, 150.0, 150.0);
        hits.sort();
        assert_eq!(hits, vec![1]);

        // tile [150,0,200,150]：应命中 shape 2
        let mut hits2 = index.query_intersect(150.0, 0.0, 200.0, 150.0);
        hits2.sort();
        assert_eq!(hits2, vec![2]);

        // tile [0,0,600,600]：命中全部
        let mut hits3 = index.query_intersect(0.0, 0.0, 600.0, 600.0);
        hits3.sort();
        assert_eq!(hits3, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_no_hit() {
        let shapes = vec![make_shape(1, 0.0, 0.0, 100.0, 100.0)];
        let index = SlideIndex::build(&shapes);
        let hits = index.query_intersect(200.0, 200.0, 50.0, 50.0);
        assert!(hits.is_empty());
    }
}
