//! 脚本只读判定:供 execute_script_impl 决定是否推撤销快照、是否标脏。
//! 黑名单思路:全量引擎注册名 - 只读白名单 = 写函数名;脚本里以标识符出现任一写函数名,
//! 或出现动态调用(call/Fn/eval/curry),就按"可能写"处理。误判方向永远是"判成写"。
use std::collections::HashSet;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Writer,
    Impress,
}

fn write_names(k: Kind) -> &'static HashSet<String> {
    static W: OnceLock<HashSet<String>> = OnceLock::new();
    static I: OnceLock<HashSet<String>> = OnceLock::new();
    match k {
        Kind::Writer => W.get_or_init(|| {
            let mut n: HashSet<String> = crate::writer_api_meta::WRITER_API_META
                .iter().map(|m| m.0.to_string()).collect();
            for r in crate::writer_api::READ_FN_NAMES.iter() {
                n.remove(*r);
            }
            n.remove("list_functions");
            n
        }),
        Kind::Impress => I.get_or_init(|| {
            let mut n: HashSet<String> = crate::impress_api_meta::IMPRESS_API_META
                .iter().map(|m| m.0.to_string()).collect();
            for r in crate::impress_api::READ_FN_NAMES.iter() {
                n.remove(*r);
            }
            n.remove("list_functions");
            n
        }),
    }
}

/// 脚本是否可确定为只读(true=只读;拿不准一律 false)。
pub fn is_readonly_script(k: Kind, script: &str) -> bool {
    let w = write_names(k);
    let mut tok = String::new();
    for ch in script.chars().chain(std::iter::once(' ')) {
        if ch.is_alphanumeric() || ch == '_' {
            tok.push(ch);
        } else if !tok.is_empty() {
            if w.contains(&tok) || matches!(tok.as_str(), "call" | "Fn" | "eval" | "curry") {
                return false;
            }
            tok.clear();
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_read_and_write() {
        assert!(is_readonly_script(Kind::Writer, "paragraph_count() + 1"));
        assert!(is_readonly_script(Kind::Writer, "let n = paragraph_count(); get_paragraph(n - 1)"));
        assert!(!is_readonly_script(Kind::Writer, r#"set_paragraph(0, "x")"#));
        assert!(!is_readonly_script(Kind::Writer, r#"let f = Fn("set_paragraph"); call(f, 0, "x")"#));
    }

    #[test]
    fn impress_read_and_write() {
        assert!(is_readonly_script(Kind::Impress, "slide_count() + shape_count(0)"));
        assert!(!is_readonly_script(Kind::Impress, r#"set_shape_text(0, 0, "x")"#));
        assert!(!is_readonly_script(Kind::Impress, "insert_slide(0, -1)"));
        assert!(!is_readonly_script(Kind::Impress, "eval(\"x\")"));
    }

    #[test]
    fn meta_counts_match_inventory() {
        // meta 漏登记写函数会被误判成只读(不安全方向);新增函数后须同步 meta 并更新这里的数量
        assert_eq!(crate::writer_api_meta::WRITER_API_META.len(), 113, "Writer meta 条数应为 113");
        assert_eq!(crate::impress_api_meta::IMPRESS_API_META.len(), 54, "Impress meta 条数应为 54");
        assert!(write_names(Kind::Writer).len() >= 113 - 22 - 1);
        assert!(write_names(Kind::Impress).len() >= 54 - 18);
    }

    #[test]
    fn write_sets_exclude_read_names() {
        let wi = write_names(Kind::Impress);
        assert!(wi.contains("insert_slide") && !wi.contains("slide_count"));
        let ww = write_names(Kind::Writer);
        assert!(ww.contains("set_paragraph") && !ww.contains("paragraph_count"));
    }
}
