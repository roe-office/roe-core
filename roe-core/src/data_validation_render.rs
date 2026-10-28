//! 数据验证（Data Validation）可视化渲染：
//!   任意 kind，当前值违反规则且 show_error=true：右上角画红色错误小三角
//!   （list 类型的下拉入口由前端按钮提供，此处不再画常驻绿三角）
//!
//! 校验逻辑（"值是否合法"）参考 univer `base-data-validator.ts` 骨架抄写：
//!   - 空值：直接看 allow_blank
//!   - 非空：按 kind 转换出"参与比较的数字"，再按 operator 做 8 种比较之一
//! kind 覆盖范围: whole / decimal / textLength。
//! list/date/time/custom/none：不做红色校验（list 无需校验，
//! date/time 需要日期序列号支持——ROE 目前没有这个能力，custom/none 语义模糊）。
//!
//! 提示气泡内容/交互完全交给前端：本文件只画静态像素标记，
//! 不产生任何弹窗行为（遵循 ROE 激进无头原则）。

use tiny_skia::{Paint, PathBuilder, PixmapMut, Transform};

use crate::tile::{col_width_at_px, col_x_px, row_height_at_px, row_y_px, CalcRenderConfig};
use roe_model::calc::Sheet;

/// 错误标记颜色（红色）
const ERROR_MARK_COLOR: [u8; 3] = [0xd3, 0x2f, 0x2f];

/// 渲染 sheet.data_validations 的可视化标记（在指定 tile 可见范围内）。
/// 在网格线之上、文字之下调用（与 cond_format_render 同一时机）。
#[allow(clippy::too_many_arguments)]
pub fn render_data_validation_marks(
    pixmap: &mut PixmapMut,
    sheet: &Sheet,
    cfg: &CalcRenderConfig,
    tx: Transform,
    row_start: u32,
    row_end: u32,
    col_start: u32,
    col_end: u32,
) {
    let mut paint = Paint::default();
    paint.anti_alias = true;

    for rule in &sheet.data_validations {
        let r1 = rule.range.start.0;
        let c1 = rule.range.start.1;
        let r2 = rule.range.end.0;
        let c2 = rule.range.end.1;
        if r2 < row_start || r1 >= row_end { continue; }
        if c2 < col_start || c1 >= col_end { continue; }

        for row_idx in r1.max(row_start)..=r2.min(row_end.saturating_sub(1)) {
            for col_idx in c1.max(col_start)..=c2.min(col_end.saturating_sub(1)) {
                // 必须用 _px 版本:tx 只有平移没有缩放(见 tile.rs),
                // 与 cond_format_render.rs 保持同一套像素坐标。
                let cell_x = col_x_px(sheet, col_idx, cfg);
                let cell_y = row_y_px(sheet, row_idx, cfg);
                let cell_w = col_width_at_px(sheet, col_idx, cfg);
                let cell_h = row_height_at_px(sheet, row_idx, cfg);

                // show_error 且当前值违反规则：右上角红色小三角
                if rule.show_error {
                    let val = sheet.cells.get(row_idx, col_idx);
                    // list：候选来自字面量或当前 sheet 的范围引用；跨 sheet 引用在渲染层
                    // 取不到候选(只拿得到当前 sheet)，此时不标红(宁可漏标不误标)
                    let invalid = if rule.kind == "list" {
                        match rule.list_candidates(&[], sheet) {
                            Some(c) => !rule.list_accepts(&val, &c),
                            None => false,
                        }
                    } else {
                        !rule.is_valid(&val)
                    };
                    if invalid {
                        let tri_size = 5.0f32.min(cell_h * 0.5).min(cell_w * 0.5);
                        let mut pb = PathBuilder::new();
                        // 内缩 1px:右/上边界落在网格线上会被吃掉一半
                        let right = cell_x + cell_w - 1.0;
                        let top = cell_y + 1.0;
                        pb.move_to(right - tri_size, top);
                        pb.line_to(right, top);
                        pb.line_to(right, top + tri_size);
                        pb.close();
                        if let Some(path) = pb.finish() {
                            paint.set_color_rgba8(ERROR_MARK_COLOR[0], ERROR_MARK_COLOR[1], ERROR_MARK_COLOR[2], 0xff);
                            pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, tx, None);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::calc::{CellRange, CellValue, DataValidationRule};

    fn rule(kind: &str, operator: &str, formula1: &str, formula2: Option<&str>, allow_blank: bool) -> DataValidationRule {
        DataValidationRule {
            range: CellRange { sheet: String::new(), start: (0, 0), end: (0, 0) },
            kind: kind.to_string(),
            operator: operator.to_string(),
            formula1: formula1.to_string(),
            formula2: formula2.map(|s| s.to_string()),
            allow_blank,
            show_error: true,
            error_msg: None,
        }
    }

    #[test]
    fn test_validate_whole_between() {
        let r = rule("whole", "between", "1", Some("100"), false);
        assert!(r.is_valid(&CellValue::Number(50.0)));
        assert!(r.is_valid(&CellValue::Number(1.0)));
        assert!(r.is_valid(&CellValue::Number(100.0)));
        assert!(!r.is_valid(&CellValue::Number(101.0)));
        assert!(!r.is_valid(&CellValue::Number(0.0)));
        // 非整数：whole 类型直接判违规
        assert!(!r.is_valid(&CellValue::Number(50.5)));
    }

    #[test]
    fn test_validate_decimal_greater_than() {
        let r = rule("decimal", "greaterThan", "3.14", None, false);
        assert!(r.is_valid(&CellValue::Number(4.0)));
        assert!(!r.is_valid(&CellValue::Number(3.0)));
        // decimal 允许小数
        assert!(r.is_valid(&CellValue::Number(3.15)));
    }

    #[test]
    fn test_validate_text_length_between() {
        let r = rule("textLength", "between", "2", Some("5"), false);
        assert!(r.is_valid(&CellValue::Text("abc".into())));   // len 3, ok
        assert!(!r.is_valid(&CellValue::Text("a".into())));    // len 1, too short
        assert!(!r.is_valid(&CellValue::Text("abcdef".into()))); // len 6, too long
    }

    #[test]
    fn test_validate_allow_blank() {
        let r_allow = rule("whole", "greaterThan", "0", None, true);
        assert!(r_allow.is_valid(&CellValue::Empty));
        let r_disallow = rule("whole", "greaterThan", "0", None, false);
        assert!(!r_disallow.is_valid(&CellValue::Empty));
    }

    #[test]
    fn test_validate_list_kind_not_checked() {
        // list 类型不做红色校验，任何值都视为合法
        let r = rule("list", "", "\"a,b,c\"", None, false);
        assert!(r.is_valid(&CellValue::Text("not_in_list".into())));
    }

    #[test]
    fn test_parse_a1_range_odf_style() {
        use roe_model::calc::parse_a1_range;
        // 以下字符串均取自 LibreOffice 官方测试样本(of:cell-content-is-in-list 内层)
        assert_eq!(
            parse_a1_range("[$Feuille1.$E$6:.$E$11]"),
            Some((Some("Feuille1".to_string()), (5, 4), (10, 4)))
        );
        assert_eq!(
            parse_a1_range("[$Sheet2.$A$1:.$A$3]"),
            Some((Some("Sheet2".to_string()), (0, 0), (2, 0)))
        );
        assert_eq!(
            parse_a1_range("[$Const.$A$2:$Const.$A$6]"),
            Some((Some("Const".to_string()), (1, 0), (5, 0)))
        );
        // 整行到 ZZ 列
        assert_eq!(
            parse_a1_range("[$Classes.$C$1:$Classes.$ZZ$1]"),
            Some((Some("Classes".to_string()), (0, 2), (0, 701)))
        );
        // 省略方括号 / 省略 $ / 单格
        assert_eq!(parse_a1_range("$Tabelle1.$A$2:.$A$13"), Some((Some("Tabelle1".to_string()), (1, 0), (12, 0))));
        assert_eq!(parse_a1_range("[Sheet1.A1]"), Some((Some("Sheet1".to_string()), (0, 0), (0, 0))));
        // sheet 名带引号
        assert_eq!(
            parse_a1_range("['my sheet'.A1:.A3]"),
            Some((Some("my sheet".to_string()), (0, 0), (2, 0)))
        );
        // Excel 写法里 sheet 名含点，仍按 Excel 解析
        assert_eq!(
            parse_a1_range("Sheet.1!A1:A2"),
            Some((Some("Sheet.1".to_string()), (0, 0), (1, 0)))
        );
        // 三维引用不支持 / 非引用 / 错误引用
        for bad in ["[$A.$A$1:$B.$B$5]", "#REF!", "[.]", "[$S.]", "OFFSET([.$A$1];1;0;COUNTA([.$A$2:.$A$100]))"] {
            assert_eq!(parse_a1_range(bad), None, "{:?} 应解析失败", bad);
        }
    }

    #[test]
    fn test_list_literal_items_real_world_samples() {
        // 真实样本里的 formula1：引用/表达式/命名区域一律不当字面量(None)，字面量照常
        for f in [
            "[$Feuille1.$E$6:.$E$11]",
            "[$Const.$A$2:$Const.$A$6]",
            "Arkusz2!$A$2:$A$21",
            "$K$1:$K$4",
            "nydim",
            "DRUH_VLASTNICTVI",
            "List",
            "#REF!",
            "OFFSET([.$A$1];1;0;COUNTA([.$A$2:.$A$100]))",
            "ROW(INDIRECT(\"1:10\"))",
        ] {
            let r = rule("list", "", f, None, false);
            assert_eq!(r.list_literal_items(), None, "formula1={} 不应当字面量", f);
        }
        // LibreOffice 的 "=C3:C5"（带引号的字面量字符串，含义就是这串文本）
        let r = rule("list", "", "\"=C3:C5\"", None, false);
        assert_eq!(r.list_literal_items(), Some(vec!["=C3:C5".to_string()]));
        // ODF 字面量含空项("")：空项被过滤
        let r = rule("list", "", "\"70\";\"71\";\"\"", None, false);
        assert_eq!(r.list_literal_items(), Some(vec!["70".to_string(), "71".to_string()]));
        // 前端 CJK 单项仍是字面量；已知代价：纯英文单项按命名引用处理
        let r = rule("list", "", "苹果", None, false);
        assert_eq!(r.list_literal_items(), Some(vec!["苹果".to_string()]));
        let r = rule("list", "", "apple", None, false);
        assert_eq!(r.list_literal_items(), None, "已知取舍:纯英文单项视为命名引用");
        // 多项英文字面量不受影响
        let r = rule("list", "", "apple,banana", None, false);
        assert_eq!(r.list_literal_items(), Some(vec!["apple".to_string(), "banana".to_string()]));
    }

    #[test]
    fn test_parse_a1_range() {
        use roe_model::calc::parse_a1_range;
        assert_eq!(parse_a1_range("A1"), Some((None, (0, 0), (0, 0))));
        assert_eq!(parse_a1_range("D1:D3"), Some((None, (0, 3), (2, 3))));
        assert_eq!(parse_a1_range("=$A$1:$A$5"), Some((None, (0, 0), (4, 0))));
        assert_eq!(parse_a1_range("B5:A1"), Some((None, (0, 0), (4, 1))), "起止应规范化");
        assert_eq!(parse_a1_range("AA10"), Some((None, (9, 26), (9, 26))));
        assert_eq!(parse_a1_range("XFD1"), Some((None, (0, 16383), (0, 16383))));
        assert_eq!(
            parse_a1_range("Sheet2!A1:A5"),
            Some((Some("Sheet2".to_string()), (0, 0), (4, 0)))
        );
        assert_eq!(
            parse_a1_range("='my sheet'!$B$2:$B$3"),
            Some((Some("my sheet".to_string()), (1, 1), (2, 1)))
        );
        assert_eq!(
            parse_a1_range("'it''s'!A1"),
            Some((Some("it's".to_string()), (0, 0), (0, 0)))
        );
        // 非法
        for bad in ["", "=", "A", "1", "A0", "A1:", ":A1", "A1:B2:C3", "AAAA1", "a,b", "苹果", "Sheet!"] {
            assert_eq!(parse_a1_range(bad), None, "{:?} 应解析失败", bad);
        }
    }

    #[test]
    fn test_list_literal_items_formats() {
        // 前端弹窗：无引号逗号分隔
        let r = rule("list", "", "苹果,香蕉, 橙子", None, false);
        assert_eq!(r.list_literal_items(), Some(vec!["苹果".to_string(), "香蕉".to_string(), "橙子".to_string()]));
        // xlsx：一对外层引号
        let r = rule("list", "", "\"苹果,香蕉,橙子\"", None, false);
        assert_eq!(r.list_literal_items(), Some(vec!["苹果".to_string(), "香蕉".to_string(), "橙子".to_string()]));
        // ODF：每项一对引号，分号分隔
        let r = rule("list", "", "\"苹果\";\"香蕉\";\"橙子\"", None, false);
        assert_eq!(r.list_literal_items(), Some(vec!["苹果".to_string(), "香蕉".to_string(), "橙子".to_string()]));
        // 引号转义 ""
        let r = rule("list", "", "\"a\"\"b\";\"c\"", None, false);
        assert_eq!(r.list_literal_items(), Some(vec!["a\"b".to_string(), "c".to_string()]));
        // 空项被过滤
        let r = rule("list", "", "a,,b,", None, false);
        assert_eq!(r.list_literal_items(), Some(vec!["a".to_string(), "b".to_string()]));
    }

    #[test]
    fn test_list_literal_items_range_ref_and_edge() {
        // 范围引用 → None（交给调用方读单元格）
        for f in ["D1:D3", "=$A$1:$A$5", "$A$1:$A$5", "Sheet2!A1:A5", "'my sheet'!A1:A5", "A1"] {
            let r = rule("list", "", f, None, false);
            assert_eq!(r.list_literal_items(), None, "formula1={} 应视为范围引用", f);
        }
        // 空串 / 全空项 → None
        assert_eq!(rule("list", "", "", None, false).list_literal_items(), None);
        assert_eq!(rule("list", "", " , , ", None, false).list_literal_items(), None);
        // 非 list → None
        assert_eq!(rule("whole", "between", "1", None, false).list_literal_items(), None);
        // 形似单元格的纯文本候选（带逗号）不误判为范围：含逗号的 "A1,B2" 不是范围
        let r = rule("list", "", "A1,B2", None, false);
        assert_eq!(r.list_literal_items(), Some(vec!["A1".to_string(), "B2".to_string()]));
    }

    #[test]
    fn test_validate_not_equal_and_not_between() {
        let r_ne = rule("whole", "notEqual", "5", None, false);
        assert!(r_ne.is_valid(&CellValue::Number(6.0)));
        assert!(!r_ne.is_valid(&CellValue::Number(5.0)));

        let r_nb = rule("decimal", "notBetween", "1", Some("10"), false);
        assert!(r_nb.is_valid(&CellValue::Number(20.0)));
        assert!(!r_nb.is_valid(&CellValue::Number(5.0)));
    }
}
