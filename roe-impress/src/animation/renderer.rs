//! AnimationRenderer：把 AnimationFrame 应用到 Shape 列表
//! 每帧调用 apply_frames()，修改 Shape 的运行时状态

use roe_model::impress::{AnimProperty, AnimValue, Shape};
use roe_model::common::Color;
use super::runtime_node::AnimationFrame;

/// Shape 的运行时覆盖状态（不修改文档模型，只用于渲染）
#[derive(Debug, Clone, Default)]
pub struct ShapeRuntimeState {
    pub shape_id:   u32,
    pub opacity:    Option<f64>,        // 0.0 ~ 1.0
    pub offset_x:   Option<f64>,        // pt
    pub offset_y:   Option<f64>,        // pt
    pub scale_x:    Option<f64>,
    pub scale_y:    Option<f64>,
    pub rotation:   Option<f64>,        // 度
    pub fill_color: Option<Color>,
    pub visible:    Option<bool>,
}

impl ShapeRuntimeState {
    pub fn new(shape_id: u32) -> Self {
        Self { shape_id, ..Default::default() }
    }

    /// 应用一帧动画变化
    pub fn apply(&mut self, property: &AnimProperty, value: &AnimValue) {
        match (property, value) {
            (AnimProperty::Opacity, AnimValue::Number(v)) => {
                self.opacity = Some(v.clamp(0.0, 1.0));
            }
            (AnimProperty::PositionX, AnimValue::Number(v)) => {
                self.offset_x = Some(*v);
            }
            (AnimProperty::PositionY, AnimValue::Number(v)) => {
                self.offset_y = Some(*v);
            }
            (AnimProperty::ScaleX, AnimValue::Number(v)) => {
                self.scale_x = Some(*v);
            }
            (AnimProperty::ScaleY, AnimValue::Number(v)) => {
                self.scale_y = Some(*v);
            }
            (AnimProperty::Rotation, AnimValue::Number(v)) => {
                self.rotation = Some(*v);
            }
            (AnimProperty::FillColor, AnimValue::Color(c)) => {
                self.fill_color = Some(*c);
            }
            (AnimProperty::Visibility, AnimValue::Bool(b)) => {
                self.visible = Some(*b);
            }
            _ => {} // 其他组合暂忽略
        }
    }
}

/// 管理一张幻灯片上所有 Shape 的运行时状态
pub struct SlideRuntimeState {
    pub states: Vec<ShapeRuntimeState>,
}

impl SlideRuntimeState {
    /// 从 Shape 列表初始化（全部默认状态）
    pub fn from_shapes(shapes: &[Shape]) -> Self {
        Self {
            states: shapes.iter().map(|s| ShapeRuntimeState::new(s.id)).collect(),
        }
    }

    /// 应用一批 AnimationFrame
    pub fn apply_frames(&mut self, frames: &[AnimationFrame]) {
        for frame in frames {
            if let Some(state) = self.states.iter_mut()
                .find(|s| s.shape_id == frame.target_shape_id)
            {
                state.apply(&frame.property, &frame.value);
            }
        }
    }

    /// 获取指定 shape 的运行时状态
    pub fn get(&self, shape_id: u32) -> Option<&ShapeRuntimeState> {
        self.states.iter().find(|s| s.shape_id == shape_id)
    }

    /// 重置所有状态（切换幻灯片时调用）
    pub fn reset(&mut self) {
        for s in &mut self.states {
            *s = ShapeRuntimeState::new(s.shape_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::impress::AnimProperty;

    #[test]
    fn test_apply_opacity() {
        let mut state = ShapeRuntimeState::new(1);
        state.apply(&AnimProperty::Opacity, &AnimValue::Number(0.5));
        assert!((state.opacity.unwrap() - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_apply_color() {
        let mut state = ShapeRuntimeState::new(2);
        let color = Color { r: 255, g: 0, b: 128, a: 255 };
        state.apply(&AnimProperty::FillColor, &AnimValue::Color(color));
        assert_eq!(state.fill_color.unwrap().r, 255);
        assert_eq!(state.fill_color.unwrap().b, 128);
    }

    #[test]
    fn test_slide_runtime_apply_frames() {
        use roe_model::common::Transform;
        use roe_model::impress::{Shape, ShapeGeometry, ShapeStyle, ShapeContent};

        let shapes = vec![
            Shape {
                id: 1,
                geometry: ShapeGeometry::Rect,
                position: Transform::from_rect(0.0, 0.0, 100.0, 50.0),
                style: ShapeStyle::default(),
                content: ShapeContent::Empty,
                text_body: Default::default(),
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        },
            Shape {
                id: 2,
                geometry: ShapeGeometry::Ellipse,
                position: Transform::from_rect(200.0, 100.0, 80.0, 80.0),
                style: ShapeStyle::default(),
                content: ShapeContent::Empty,
                text_body: Default::default(),
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        },
        ];

        let mut runtime = SlideRuntimeState::from_shapes(&shapes);

        let frames = vec![
            AnimationFrame {
                target_shape_id: 1,
                property: AnimProperty::Opacity,
                value: AnimValue::Number(0.3),
            },
            AnimationFrame {
                target_shape_id: 2,
                property: AnimProperty::Visibility,
                value: AnimValue::Bool(false),
            },
        ];

        runtime.apply_frames(&frames);

        assert!((runtime.get(1).unwrap().opacity.unwrap() - 0.3).abs() < 0.001);
        assert_eq!(runtime.get(2).unwrap().visible, Some(false));
    }

    #[test]
    fn test_reset() {
        let mut state = SlideRuntimeState {
            states: vec![ShapeRuntimeState {
                shape_id: 1,
                opacity: Some(0.5),
                ..Default::default()
            }],
        };
        state.reset();
        assert!(state.states[0].opacity.is_none());
    }
}
