//! roe-api — C-ABI 层
//!
//! # 内存所有权铁律 (FFI Ownership Rules)
//!
//! - 字符串：所有 `*mut c_char` 返回值，调用方用完必须调用 [`roe_free_string`]
//!   释放（内部用 `CString::from_raw` 收尸，不能用 C/C++ 的 `free`/`delete`）。
//! - tile buffer：调用方预分配并持有内存，Rust 端只写入，不涉及释放。
//! - 句柄：通过 [`roe_open_document_from_bytes`] 获得，必须用
//!   [`roe_close_document`] 释放。`0` 表示无效/失败句柄，永不被分配。
//!
//! 详见 ROE_Progress_v8.md「roe-api（C-ABI 层）」章节。

mod clipboard;
mod writer_clipboard;
mod cursor;
mod cursor_inline;
mod selection_rects;
mod cursor_move_line;
mod cursor_para_info;
mod text_edit;
mod animation;
mod calc_geometry;
pub mod document;
pub mod test_support;
mod error;
mod formula_function_list;
mod events;
pub mod handle;
mod hit_test;
mod formula_edit;
mod layout_query;
mod table_cell_cursor;
mod table_cell_edit;
pub mod script;
pub mod shadow;
mod snapshot;
mod pivot;
mod chart;
mod chart_query;
mod impress_geometry;
pub use impress_geometry::{roe_impress_set_shape_style, roe_impress_set_slide_background, roe_impress_set_slide_background_image, roe_impress_set_text_style, roe_impress_set_text_style_range, roe_impress_text_style_query, roe_impress_set_para_style};
mod impress_text_edit;
mod impress_slide_ops;
mod impress_master;
pub use impress_master::{roe_impress_master_list, roe_impress_master_render_size, roe_impress_render_master, roe_impress_master_hit_test, roe_impress_master_query_shape_rect, roe_impress_master_set_shape_rect, roe_impress_master_shape_ids, roe_impress_master_set_background, roe_impress_master_set_background_image, roe_impress_master_insert_image, roe_impress_master_layout_usage, roe_impress_master_rename_layout, roe_impress_master_set_shape_rotation, roe_impress_master_insert_placeholder, roe_impress_master_layout_placeholders, roe_impress_master_delete_layout, roe_impress_master_duplicate_layout, roe_impress_master_delete_shape};
mod impress_chart_ops;
pub use impress_chart_ops::*;
pub use impress_slide_ops::{roe_impress_insert_slide, roe_impress_delete_slide, roe_impress_duplicate_slide, roe_impress_insert_slide_with_layout, roe_impress_set_slide_layout, roe_impress_list_layouts, roe_impress_slide_layout};
pub use impress_text_edit::{roe_impress_insert_textbox, roe_impress_text_insert, roe_impress_text_delete, roe_impress_text_split, roe_impress_text_caret, roe_impress_text_hit, roe_impress_text_is_empty, roe_impress_text_move, roe_impress_text_copy, roe_impress_text_paste, roe_impress_text_extent};
mod tile;
mod comments;
mod char_format;
pub mod sort_paragraphs;
pub mod page_setup;
pub mod header_footer;
mod find_replace;
mod auto_fit;
mod range_json;

pub use clipboard::{roe_clipboard_has_content, roe_clipboard_clear, roe_copy_calc_range, roe_paste_calc_range, roe_copy_writer_range, roe_paste_writer_range, roe_copy_impress_shapes, roe_paste_impress_shapes};
pub use animation::{roe_on_click, roe_tick};
pub use document::{roe_close_document, roe_get_document_size, roe_get_page_count, roe_open_document_from_bytes, roe_save_document_to_bytes, RoeFormatHint};
pub use error::{roe_last_error_message, RoeErrorCode};
pub use events::roe_poll_event;
pub use hit_test::roe_hit_test;
pub use table_cell_cursor::{roe_hit_test_ex, roe_query_layout_rect_ex, roe_query_cell_selection_rects, roe_query_cell_range_rects, roe_cell_nav_line};
pub use table_cell_edit::{roe_cell_replace_range, roe_cell_delete_char, roe_cell_split_paragraph, roe_cell_tab, roe_cell_merge_paragraph, roe_cell_nav, roe_cell_fmt_char, roe_cell_fmt_para, roe_cell_fmt_get, roe_cell_fmt_font};
pub use formula_edit::{roe_formula_get, roe_formula_set, roe_formula_insert, roe_formula_hit_test, roe_formula_preview_rgba};
pub use layout_query::roe_query_layout_rect;
pub use calc_geometry::{roe_calc_hit_test, roe_calc_query_layout_rect, roe_get_column_width, roe_get_row_height, roe_set_column_width, roe_set_row_height, roe_get_frozen_panes, roe_get_visible_range, roe_get_row_offset, roe_get_col_offset, roe_get_row_at_offset, roe_get_col_at_offset, roe_get_used_range, roe_get_sheet_total_height, roe_get_sheet_total_width, roe_get_sheet_count, roe_get_sheet_name, roe_get_row_count, roe_get_col_count, roe_add_sheet, roe_remove_sheet, roe_rename_sheet};
pub use script::{roe_execute_script, roe_free_string, roe_get_formula_function_list, roe_get_index, roe_query_script};
pub use chart::{roe_render_chart_to_png, roe_render_stat_chart_to_rgba, roe_free_bytes};
pub use chart_query::{roe_get_chart_rect, roe_get_chart_config, roe_resize_chart, roe_move_chart, roe_set_chart_rect, roe_set_chart_style, roe_delete_chart, roe_list_charts, roe_calc_insert_stat_chart, roe_calc_list_objects, roe_calc_get_object, roe_calc_set_object_rect, roe_calc_delete_object, roe_calc_insert_image, roe_calc_insert_shape, roe_calc_insert_textbox, roe_calc_set_object_text, roe_calc_set_object_fill, roe_calc_set_object_stroke};
pub use snapshot::{roe_begin_edit, roe_commit_edit, roe_discard_edit, roe_undo, roe_redo};
pub use pivot::{roe_pivot_sheet, roe_get_pivot_unique_values, roe_sheet_preview, roe_sheet_is_pivot, roe_get_pivot_buttons, roe_pivot_set_filter, roe_pivot_set_sort, roe_get_pivot_config, roe_pivot_set_config};
pub use tile::{roe_get_render_page_size, roe_render_page, roe_render_sheet_viewport, roe_render_writer_viewport, roe_get_writer_document_size, roe_set_writer_options, roe_get_impress_document_size, roe_render_impress_viewport};
pub use cursor_inline::roe_cursor_move_inline;
pub use selection_rects::roe_query_selection_rects;
pub use cursor_move_line::roe_cursor_move_line;
pub use cursor::{RoeLocation, CursorInlineCommand};
pub use cursor_move_line::CursorLineDirection;
pub use cursor_para_info::roe_get_paragraph_info;
pub use text_edit::{roe_insert_char, roe_delete_char, roe_insert_text, roe_split_paragraph, roe_merge_paragraph, roe_replace_range};
pub use comments::{roe_get_comment_threads, roe_take_comment_hits, roe_insert_comment, insert_comment_core, roe_delete_comment, delete_comment_core, roe_set_comment_done, roe_edit_comment, roe_reply_comment, reply_comment_core, roe_insert_footnote, insert_note_core};
pub use char_format::{
    roe_set_range_char_format_bold, roe_get_range_char_format_bold,
    roe_set_range_char_format_italic, roe_get_range_char_format_italic,
    roe_set_range_char_format_underline, roe_get_range_char_format_underline,
    roe_set_range_char_format_font_size, roe_get_range_char_format_font_size,
    roe_set_range_char_format_font_family, roe_get_range_char_format_font_family,
    roe_set_range_char_format_font_family_ascii, roe_get_range_char_format_font_family_ascii,
    roe_set_range_char_format_underline_style, roe_get_range_char_format_underline_style,
    roe_set_range_char_format_strikethrough, roe_get_range_char_format_strikethrough,
    roe_set_range_char_format_script, roe_get_range_char_format_script,
    roe_set_paragraph_alignment,
    roe_set_paragraph_shading,
    roe_set_paragraph_line_rule,
    roe_set_range_char_format_color, roe_set_range_char_format_background,
    roe_clear_range_char_format_all,
    roe_undo_group_begin, roe_undo_group_end,
    roe_set_paragraph_list, roe_set_paragraph_style, roe_change_list_level, roe_set_paragraph_spacing,
};
pub use find_replace::{roe_replace_text, roe_replace_text_ex, roe_find_text};
pub use writer_clipboard::{roe_writer_copy, roe_writer_delete_range, roe_writer_paste, roe_writer_clip_has, roe_writer_painter_pick, roe_writer_painter_apply, roe_writer_change_case, roe_writer_insert_page_break};
pub use impress_geometry::{roe_impress_hit_test, roe_impress_query_shape_rect, roe_impress_set_shape_rect, roe_impress_set_shape_rotation, roe_impress_move_shape, roe_impress_resize_shape, roe_impress_shape_move_forward, roe_impress_shape_move_backward, roe_impress_shape_bring_to_front, roe_impress_shape_send_to_back, roe_impress_set_line_endpoints, roe_impress_insert_shape, roe_impress_delete_shape, roe_impress_shape_preview_path, roe_impress_insert_image, roe_image_dimensions, roe_impress_insert_table};
pub use auto_fit::roe_auto_fit_columns;
pub use range_json::{roe_get_range_json, roe_set_range_json};

pub use sort_paragraphs::roe_sort_paragraphs;
pub use page_setup::{roe_set_page_setup, roe_get_page_setup};
pub use header_footer::{roe_set_header_footer, roe_get_header_footer};

pub use comments::{roe_delete_note, roe_writer_insert_column_break, roe_writer_set_columns};
pub use comments::roe_writer_insert_image;
pub use comments::roe_writer_insert_chart;
pub use comments::{roe_writer_get_chart_json, roe_writer_set_chart_json};
pub use comments::{roe_writer_hit_object, roe_writer_resize_object, roe_writer_delete_object};
pub use comments::roe_writer_insert_shape;

pub use comments::{roe_writer_set_shape_style, roe_writer_object_kind};

pub use comments::{roe_writer_set_object_rotation, roe_writer_object_rotation};

pub use comments::roe_writer_insert_table;
pub use impress_text_edit::roe_impress_text_sel_rects;
pub use impress_text_edit::{roe_impress_cell_at, roe_impress_cell_hit, roe_impress_cell_caret, roe_impress_cell_insert, roe_impress_cell_delete, roe_impress_is_table, roe_impress_cell_move, roe_impress_table_fit, roe_impress_table_settle};
