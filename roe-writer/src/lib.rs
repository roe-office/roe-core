//! roe-writer：流式文本布局 + 分页引擎
//! cosmic-text 负责 shaping/BiDi，自己只做分页决策

pub mod font_registry;
pub mod hyphenation;
pub mod layout;
pub mod frame;
pub mod line_shape;
pub mod page_fit;
pub mod widow_orphan;
pub mod split;
pub mod tile;
pub mod tile_formula;
pub mod char_format;
pub mod find_replace;
pub(crate) mod headerfooter;

pub use font_registry::FontRegistry;
pub use layout::ParagraphLayoutEngine;
pub use frame::Margins;
pub use line_shape::{char_offset_to_x, x_to_char_offset};
pub use tile::WriterTileRenderer;
