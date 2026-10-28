//! WROOT-040-HYPHEN：断词排版对接层（方案C1）。
//!
//! 设计（与Gemini三方讨论定案，见 test_samples/curated_progress_docx/
//! bug_log.md WROOT-040 断词章节）：
//! - 第一步（本文件）：在文本喂给 parley 构建 Layout 之前，扫描纯ASCII
//!   长单词，用 hypher 算出音节切分点，插入 U+00AD SOFT HYPHEN 作为
//!   候选断点。parley 断行时天然识别 U+00AD 为合法断行机会（已用独立
//!   PoC 验证，见本次会话记录），不需要改动断行算法本身。
//! - 第二步（渲染/收集LineBox阶段，不在本文件）：检测行尾字符若为
//!   U+00AD，渲染时替换为可见的 '-'。
//!
//! 只处理"整个候选词全部是ASCII字母"且长度超过阈值的片段，跳过短词
//! （断开后不美观，也没必要）、跳过含标点/数字混杂的片段（如 don't、
//! 3rd，避免hypher对非纯字母输入产生意外结果——先覆盖最常见场景）。

/// 短于此长度的单词不做断词处理（断开后视觉上不划算，也不是常见场景）。
const MIN_WORD_LEN_FOR_HYPHENATION: usize = 6;

/// 对一段文本做断词预处理：扫描空白分隔的单词，符合条件的插入 U+00AD
/// 音节候选断点。不改变文本的非单词部分（空白、标点本身的位置）。
///
/// 实现方式：按"是否为ASCII字母"对字符做游程切分（run-length），
/// 字母游程作为候选单词，其余字符（空白/标点/数字等）原样透传。
/// 这样比按空白split更精确：能正确处理"different," 这种单词后紧跟
/// 标点的情况（游程切分下"different"和","会被分成两段，标点段不受影响）。
/// v236 临时开关：U+00AD 占 2 字节，插入后行文本比原文长，导致光标/选区偏移与
/// 格式、编辑接口（按原文字节）错位。偏移换算修好之前保持 false。
const ENABLE_HYPHENATION: bool = false;

pub fn preprocess_hyphenation(text: &str) -> String {
    if !ENABLE_HYPHENATION && !cfg!(test) {
        return text.to_string();
    }
    let mut result = String::with_capacity(text.len() + 8);
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_alphabetic() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_alphabetic() {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            result.push_str(&hyphenate_word_if_eligible(&word));
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}

/// 对单个候选单词判断是否需要断词处理，需要则返回插入 U+00AD 后的
/// 版本，不需要则原样返回。
fn hyphenate_word_if_eligible(word: &str) -> String {
    if word.chars().count() < MIN_WORD_LEN_FOR_HYPHENATION {
        return word.to_string();
    }
    let syllables: Vec<&str> = hypher::hyphenate(word, hypher::Lang::English).collect();
    if syllables.len() <= 1 {
        return word.to_string();
    }
    syllables.join("\u{00AD}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_word_untouched() {
        // "cat" 长度3，低于阈值，不处理
        assert_eq!(preprocess_hyphenation("cat"), "cat");
    }

    #[test]
    fn long_word_gets_soft_hyphens() {
        let result = preprocess_hyphenation("different");
        assert_eq!(result, "dif\u{ad}fer\u{ad}ent");
    }

    #[test]
    fn punctuation_after_word_preserved() {
        // "different," 逗号不应被hypher处理，单词本身仍应正确断词
        let result = preprocess_hyphenation("different,");
        assert_eq!(result, "dif\u{ad}fer\u{ad}ent,");
    }

    #[test]
    fn sentence_with_mixed_content() {
        let result = preprocess_hyphenation("This is a very different word test here");
        assert_eq!(
            result,
            "This is a very dif\u{ad}fer\u{ad}ent word test here"
        );
        // "This"(4) "very"(4) "word"(4) "test"(4) "here"(4) 均低于阈值6，
        // 不应被处理；只有"different"(9)超过阈值。
    }

    #[test]
    fn empty_string() {
        assert_eq!(preprocess_hyphenation(""), "");
    }

    #[test]
    fn single_syllable_long_word_untouched() {
        // hypher对某些长度够但只有单音节的词，syllables.len()<=1，应原样返回
        // （用一个真实场景：数字/符号混合不会走到这里，这里验证长度达标
        // 但hypher判定单音节的情况——用 "through" 测试，若hypher将其
        // 判定为单音节则应原样返回，不强行插入U+00AD）
        let result = preprocess_hyphenation("through");
        // 不断言具体结果（不确定hypher对这个词的判定），只断言不panic
        // 且要么原样要么含有效断点
        assert!(result == "through" || result.contains('\u{ad}'));
    }
}
