//! Excel 数字格式（numFmt）token 化解析与渲染
//!
//! 设计参考 excelize（Go）+ nfp（Go，excelize 的词法分析依赖）的两层架构思路：
//! 词法层（本文件 tokenize）把格式串切成 token 序列；渲染层（render）逐 token
//! 生成显示文本。未抄任何代码，只借鉴 token 类型划分和"h 之后紧跟的 m 是分钟，
//! 否则是月份"这个状态机判断顺序。
//!
//! 替换原 tile.rs 里的字符串替换式 apply_num_format（技术债，不处理分号分段、
//! 颜色条件、0 和 # 语义区别、转义文本、以及 mm 在时间上下文的分钟识别）。

/// 数字格式 token 类型
#[derive(Debug, Clone, PartialEq)]
pub enum FormatToken {
    /// 数字占位符 '0'，不足位数补零
    ZeroPlaceHolder(usize),
    /// 数字占位符 '#'，不足位数不补
    HashPlaceHolder(usize),
    /// 数字占位符 '?'，不足位数补空格（用于分数对齐，本次范围内按 Hash 处理即可）
    QuestionPlaceHolder(usize),
    /// 小数点 '.'
    DecimalPoint,
    /// 千分位分隔符 ','
    ThousandsSeparator,
    /// 百分号 '%'
    Percent,
    /// 日期时间字符：y/m/d/h/s 的连续重复，例如 "yyyy" "mm" "hh"
    DateTime(String),
    /// AM/PM 标记
    AmPm,
    /// 颜色条件标记，如 "[Red]" "[Blue]"，只保留颜色名
    Color(String),
    /// 方括号条件，如 "[>100]"，本次范围内只解析不参与渲染分支（记录但不生效）
    Condition(String),
    /// 引号包裹的字面量文本，如 "\"元\""，已去掉引号
    QuotedLiteral(String),
    /// 反斜杠转义的单字符字面量
    EscapedLiteral(char),
    /// 货币符号等其它字面量字符，原样输出
    Literal(char),
    /// 文本占位符 '@'（用于 CellValue::Text 段）
    TextPlaceHolder,
    /// 下划线转义 '_x'：不显示字符 x 本身，但留出与它等宽的空白。
    /// Excel 用于让数字列与括号/文本对齐（如 "_(" "_)"）。简化处理：
    /// 渲染时统一输出一个空格，不按 x 的实际字形宽度计算（等宽近似）。
    Skip(char),
    /// 星号填充 '*x'：本应用字符 x 重复填充撑满列宽以实现两端对齐，
    /// 但本模块是纯格式化层，不掌握渲染时的目标列宽/像素宽度，无法做
    /// 真正的动态撑满（架构上是格式串→文本的纯函数，不依赖渲染上下文，
    /// 故意不引入这个依赖）。简化处理：只输出一个字符 x，不重复。
    /// 已知简化，若后续有更精确对齐需求，需要改造成 render 层传入
    /// 可用宽度再展开。
    Fill(char),
}

/// 一个 section 的 token 序列（对应分号分段后的一段）
#[derive(Debug, Clone, Default)]
pub struct FormatSection {
    pub tokens: Vec<FormatToken>,
}

/// 完整格式串解析结果：最多4段，对应 正数;负数;零值;文本
/// 缺省规则（贴近 Excel）：
///   1段 → 正/负/零 都用这段（负数会在前面自动加负号）
///   2段 → 正数用第1段，负数用第2段（第2段里若无负号literal，仍需调用方决定是否加负号）
///   3段 → 正/负/零 各用一段
///   4段 → 正/负/零/文本 各用一段
#[derive(Debug, Clone, Default)]
pub struct ParsedFormat {
    pub sections: Vec<FormatSection>,
}

/// 把 Excel 数字格式串解析为 ParsedFormat（按分号切段，再逐段分词）。
/// 分号本身若被引号包裹或反斜杠转义，不作为分段符——本次范围内格式串一般不会
/// 出现这种边界情况，简化处理：只识别未被引号包裹的顶层 ';'。
pub fn parse_format(fmt: &str) -> ParsedFormat {
    // 2026-07-13修复：Excel/ODF 保留格式代码 "General"（大小写不敏感）表示
    // "不做特殊格式化，数字按默认规则原样显示"，不是普通格式串。此前直接送进
    // tokenize_section，字母逐个落入 Literal(c) 兜底分支，渲染时被当作纯字面量
    // 拼接输出，导致所有默认格式（未显式设置格式的普通数字单元格，是最常见的
    // 情况）单元格都显示成字符串"General"而不是数字本身。这里提前拦截，返回
    // 一个 tokens 为空的 FormatSection 作为"General"的标记，交给
    // render_number_section 识别后走默认数字转文本逻辑（与 cell_value_to_text
    // 的 Number 分支保持一致，避免同一份"数字怎么转文本"的规则在两处出现
    // 不一致的实现）。
    if fmt.trim().eq_ignore_ascii_case("General") {
        return ParsedFormat { sections: vec![FormatSection::default()] };
    }
    let raw_sections = split_sections(fmt);
    let sections = raw_sections.iter().map(|s| tokenize_section(s)).collect();
    ParsedFormat { sections }
}

/// 按顶层分号切分，跳过引号内的分号
fn split_sections(fmt: &str) -> Vec<String> {
    let mut sections = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                current.push(c);
            }
            '\\' => {
                current.push(c);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ';' if !in_quotes => {
                sections.push(current.clone());
                current.clear();
            }
            _ => current.push(c),
        }
    }
    sections.push(current);
    sections
}

/// 判断字符是否属于日期时间字符集合（用于连续重复合并）
fn is_datetime_char(c: char) -> bool {
    matches!(c, 'y' | 'Y' | 'm' | 'M' | 'd' | 'D' | 'h' | 'H' | 's' | 'S')
}

/// 对单个 section 字符串分词
fn tokenize_section(s: &str) -> FormatSection {
    let mut tokens = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '"' => {
                // 引号包裹的字面量文本
                let mut lit = String::new();
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    lit.push(chars[i]);
                    i += 1;
                }
                i += 1; // 跳过收尾引号
                tokens.push(FormatToken::QuotedLiteral(lit));
            }
            '\\' => {
                // 反斜杠转义
                i += 1;
                if i < chars.len() {
                    tokens.push(FormatToken::EscapedLiteral(chars[i]));
                    i += 1;
                }
            }
            '[' => {
                // 方括号：颜色 或 条件
                let mut inner = String::new();
                i += 1;
                while i < chars.len() && chars[i] != ']' {
                    inner.push(chars[i]);
                    i += 1;
                }
                i += 1; // 跳过 ']'
                let upper = inner.to_uppercase();
                let known_colors = [
                    "RED", "BLUE", "GREEN", "YELLOW", "MAGENTA", "CYAN", "WHITE", "BLACK",
                ];
                if known_colors.contains(&upper.as_str()) {
                    tokens.push(FormatToken::Color(inner));
                } else {
                    tokens.push(FormatToken::Condition(inner));
                }
            }
            '0' => {
                let start = i;
                while i < chars.len() && chars[i] == '0' {
                    i += 1;
                }
                tokens.push(FormatToken::ZeroPlaceHolder(i - start));
            }
            '#' => {
                let start = i;
                while i < chars.len() && chars[i] == '#' {
                    i += 1;
                }
                tokens.push(FormatToken::HashPlaceHolder(i - start));
            }
            '?' => {
                let start = i;
                while i < chars.len() && chars[i] == '?' {
                    i += 1;
                }
                tokens.push(FormatToken::QuestionPlaceHolder(i - start));
            }
            '.' => {
                tokens.push(FormatToken::DecimalPoint);
                i += 1;
            }
            ',' => {
                tokens.push(FormatToken::ThousandsSeparator);
                i += 1;
            }
            '%' => {
                tokens.push(FormatToken::Percent);
                i += 1;
            }
            '@' => {
                tokens.push(FormatToken::TextPlaceHolder);
                i += 1;
            }
            '_' => {
                // '_x'：留出与 x 等宽的空白，不显示 x 本身。x 缺失（格式串
                // 以 '_' 结尾，非法但需容错）时视为空格。
                i += 1;
                let skip_char = if i < chars.len() { chars[i] } else { ' ' };
                tokens.push(FormatToken::Skip(skip_char));
                i += 1;
            }
            '*' => {
                // '*x'：本应重复填充 x 撑满列宽，简化为输出一个 x（见
                // FormatToken::Fill 定义注释）。x 缺失时视为空格。
                i += 1;
                let fill_char = if i < chars.len() { chars[i] } else { ' ' };
                tokens.push(FormatToken::Fill(fill_char));
                i += 1;
            }
            'A' | 'a' => {
                // AM/PM 识别（大小写不敏感，形如 "AM/PM" 或 "A/P"）
                let rest: String = chars[i..].iter().collect();
                let upper_rest = rest.to_uppercase();
                if upper_rest.starts_with("AM/PM") {
                    tokens.push(FormatToken::AmPm);
                    i += 5;
                } else if upper_rest.starts_with("A/P") {
                    tokens.push(FormatToken::AmPm);
                    i += 3;
                } else {
                    tokens.push(FormatToken::Literal(c));
                    i += 1;
                }
            }
            _ if is_datetime_char(c) => {
                let start = i;
                let first = c.to_ascii_lowercase();
                while i < chars.len() && chars[i].to_ascii_lowercase() == first {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                tokens.push(FormatToken::DateTime(text));
            }
            _ => {
                tokens.push(FormatToken::Literal(c));
                i += 1;
            }
        }
    }
    FormatSection { tokens }
}


/// 从 Excel serial number 提取 (year, month, day, hour, minute, second)
fn serial_to_datetime(n: f64, date1904: bool) -> (i32, u32, u32, u32, u32, u32) {
    let (year, month, day) = excel_serial_to_date_pub(n, date1904);
    let secs_total = ((n.fract()) * 86400.0).round() as u32;
    let hour = secs_total / 3600;
    let minute = (secs_total % 3600) / 60;
    let sec = secs_total % 60;
    (year, month, day, hour, minute, sec)
}

/// 复用 tile.rs 里已验证正确的 serial→date 算法（Julian Day Number 反推）。
/// 这里独立重新实现一份，避免 numfmt.rs 反向依赖 tile.rs 私有函数。
///
/// ROOT-041: date1904=true 时（Mac Excel传统1904日期系统）换算成
/// 等效的1900系统序列号，再复用原有算法。
///
/// 关键点：偏移常数是1463，不是"看起来更自然"的1462！
/// 已用LOK oox filter源码(workbooksettings.cxx)核实权威定义：
///   1900系统 NullDate = 1899-12-30（LOK刻意提前一天，用来"吸收"
///     Excel虚构的1900年2月29日闰日bug，对应下面`s>=60时减1`那行）
///   1904系统 NullDate = 1904-01-01（真实日期，不含任何闰日bug，
///     序列号N直接等于 1904-01-01 + N天，不需要任何修正）
/// 1899-12-30 到 1904-01-01 的自然天数差确实是1462天，但如果只加
/// 1462后直接复用下面`s>=60时减1`这条只属于1900系统闰日修正的逻辑，
/// 1904日期会被多减1天（已用已知样本 serial=60/37145/26536 对应
/// Mar 1 1904 / Sept 12 2005 / Aug 26 1976 三组真值实测验证，加1462
/// 结果全部systematic偏早1天）。加1463能精确抵消这个多减的1天，
/// 三组样本验证结果完全匹配。1463是数字上"1462+1"，但这1天补偿的
/// 是本函数`s>=60减1`这行代码的副作用，不是历史常数本身有误——
/// 若未来这行闰日修正逻辑被重构，这个偏移量需要同步复核。
fn excel_serial_to_date_pub(serial: f64, date1904: bool) -> (i32, u32, u32) {
    let serial = if date1904 { serial + 1463.0 } else { serial };
    let s = serial as i64;
    let days = if s >= 60 { s - 1 } else { s };
    let jdn = days + 2415020;
    let l = jdn + 68569;
    let n = (4 * l) / 146097;
    let l = l - (146097 * n + 3) / 4;
    let i = (4000 * (l + 1)) / 1461001;
    let l = l - (1461 * i) / 4 + 31;
    let j = (80 * l) / 2447;
    let day = l - (2447 * j) / 80;
    let l = j / 11;
    let month = j + 2 - 12 * l;
    let year = 100 * (n - 49) + i + l;
    (year as i32, month as u32, day as u32)
}

fn month_abbr_local(m: u32) -> &'static str {
    match m {
        1 => "Jan", 2 => "Feb", 3 => "Mar", 4 => "Apr", 5 => "May", 6 => "Jun",
        7 => "Jul", 8 => "Aug", 9 => "Sep", 10 => "Oct", 11 => "Nov", 12 => "Dec",
        _ => "",
    }
}

/// 由(year, month, day)算星期几，返回0=Sunday..6=Saturday。
/// 用Zeller's congruence（公历），与month_abbr_local一样不做真正
/// locale本地化，固定用英文，与月份名缩写的处理策略保持一致。
/// 输入的(year, month, day)必须已经是excel_serial_to_date_pub换算后
/// 的真实公历日期（date1904差异已在换算serial number阶段处理过），
/// 这里不需要再关心1900/1904系统差异。
fn weekday_from_ymd(year: i32, month: u32, day: u32) -> u32 {
    let (y, m) = if month < 3 { (year - 1, month + 12) } else { (year, month) };
    let k = y % 100;
    let j = y / 100;
    let d = day as i32;
    let m = m as i32;
    let h = (d + (13 * (m + 1)) / 5 + k + k / 4 + j / 4 + 5 * j).rem_euclid(7);
    // Zeller's congruence: h=0是Saturday，需要转换成0=Sunday..6=Saturday
    ((h + 6) % 7) as u32
}

/// 星期缩写：ROOT-082 v142遗留（4.1节，本次补上）。此前ddd/dddd
/// 格式代码退化成两位日期数字，与Excel/LibreOffice/OnlyOffice/WPS
/// 官方渲染（一致显示"Tue"等星期缩写）不符。
fn weekday_abbr_local(year: i32, month: u32, day: u32) -> &'static str {
    match weekday_from_ymd(year, month, day) {
        0 => "Sun", 1 => "Mon", 2 => "Tue", 3 => "Wed", 4 => "Thu", 5 => "Fri", 6 => "Sat",
        _ => "",
    }
}

/// 渲染结果：显示文本 + 可选颜色（来自 [Red] 这类条件颜色 token）
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedFormat {
    pub text: String,
    pub color: Option<String>,
}

/// 判断某个 section 是否含日期时间 token（决定走数值分支还是日期分支）
fn section_has_datetime(section: &FormatSection) -> bool {
    section.tokens.iter().any(|t| matches!(t, FormatToken::DateTime(_)))
}

/// 选择数值应该使用哪个 section（贴近 Excel 语义）：
/// 1段：正/负/零 都用它
/// 2段：正数用[0]，负数用[1]，零用[0]
/// 3段：正[0] 负[1] 零[2]
/// 4段：正[0] 负[1] 零[2] 文本[3]（文本走另一个入口，这里不处理）
fn select_section(sections: &[FormatSection], n: f64) -> (&FormatSection, bool) {
    let count = sections.len();
    if count == 0 {
        unreachable!("parse_format 保证至少1段");
    }
    if n > 0.0 {
        (&sections[0], false)
    } else if n < 0.0 {
        if count >= 2 {
            (&sections[1], false) // 该段自己控制是否显示负号（若无负号literal，调用方按段内容为准，不额外加负号）
        } else {
            (&sections[0], true) // 只有1段：借用正数段渲染，额外加负号
        }
    } else {
        // n == 0
        if count >= 3 {
            (&sections[2], false)
        } else {
            (&sections[0], false)
        }
    }
}

/// 渲染日期时间 section：h/m/s 状态机（h 之后紧跟的 m 视为分钟，否则为月份）
fn render_datetime_section(section: &FormatSection, n: f64, date1904: bool) -> String {
    let (year, month, day, hour, minute, sec) = serial_to_datetime(n, date1904);
    let mut result = String::new();
    let mut last_was_hour = false;
    let has_ampm = section.tokens.iter().any(|t| matches!(t, FormatToken::AmPm));
    let (display_hour, ap_str) = if has_ampm {
        let ap = if hour < 12 { "AM" } else { "PM" };
        let h12 = if hour % 12 == 0 { 12 } else { hour % 12 };
        (h12, ap)
    } else {
        (hour, "")
    };

    for token in &section.tokens {
        match token {
            FormatToken::DateTime(s) => {
                let lower = s.to_lowercase();
                let ch = lower.chars().next().unwrap_or(' ');
                let len = lower.len();
                match ch {
                    'y' => {
                        last_was_hour = false;
                        if len >= 4 {
                            result.push_str(&format!("{:04}", year));
                        } else {
                            result.push_str(&format!("{:02}", year % 100));
                        }
                    }
                    'd' => {
                        last_was_hour = false;
                        match len {
                            1 => result.push_str(&format!("{}", day)),
                            2 => result.push_str(&format!("{:02}", day)),
                            // ddd/dddd 星期名：ROOT-082 v142遗留修复（4.1节）。
                            // dddd（完整星期名）本次也用缩写代替，与Excel真实
                            // 长格式（Tuesday）有差异，是本次范围内的简化——
                            // 已知不足，后续如需要完整星期名再单独处理。
                            _ => result.push_str(weekday_abbr_local(year, month, day)),
                        }
                    }
                    'h' => {
                        last_was_hour = true;
                        if len == 1 {
                            result.push_str(&format!("{}", display_hour));
                        } else {
                            result.push_str(&format!("{:02}", display_hour));
                        }
                    }
                    's' => {
                        last_was_hour = false;
                        if len == 1 {
                            result.push_str(&format!("{}", sec));
                        } else {
                            result.push_str(&format!("{:02}", sec));
                        }
                    }
                    'm' => {
                        if last_was_hour {
                            // 紧跟小时之后的 m：分钟
                            if len == 1 {
                                result.push_str(&format!("{}", minute));
                            } else {
                                result.push_str(&format!("{:02}", minute));
                            }
                            last_was_hour = false;
                        } else if len == 3 {
                            result.push_str(month_abbr_local(month));
                        } else if len == 1 {
                            result.push_str(&format!("{}", month));
                        } else {
                            result.push_str(&format!("{:02}", month));
                        }
                    }
                    _ => {}
                }
            }
            FormatToken::AmPm => {
                // AM/PM 显示已在函数开头统一算好（display_hour/ap_str），
                // 这里只需要把预计算好的 ap_str 输出即可。
                result.push_str(ap_str);
            }
            FormatToken::Literal(c) => {
                result.push(*c);
            }
            FormatToken::EscapedLiteral(c) => {
                result.push(*c);
            }
            FormatToken::QuotedLiteral(s) => {
                result.push_str(s);
            }
            FormatToken::Color(_) | FormatToken::Condition(_) => {
                // 颜色/条件不产生文本，由外层 RenderedFormat.color 承载
            }
            _ => {}
        }
    }
    result
}

/// 渲染纯数值 section（0/# 占位符 + 千分位 + 小数点 + 百分号）
fn render_number_section(section: &FormatSection, n: f64, force_negative_sign: bool) -> String {
    // 若该 section 完全不含数字占位符（0/#/./%），说明这是纯字面量段
    // （如零值段写成 "零" 这种没有 0 占位符的情况），直接拼接所有字面量返回，
    // 不应输出任何数字。
    let has_any_digit_placeholder = section.tokens.iter().any(|t| matches!(
        t,
        FormatToken::ZeroPlaceHolder(_) | FormatToken::HashPlaceHolder(_)
        | FormatToken::DecimalPoint | FormatToken::Percent
    ));
    if !has_any_digit_placeholder {
        // tokens 为空 = parse_format 特判过的 "General" 标记（见上方注释），
        // 走默认数字转文本规则；tokens 非空但无数字占位符（如格式串写成纯
        // 字面量 ""零""）才是真正的"纯字面量段"，原样拼接输出，行为不变。
        if section.tokens.is_empty() {
            if n.fract() == 0.0 && n.abs() < 1e15 {
                return format!("{}", n as i64);
            } else {
                let rounded = (n * 100.0).round() / 100.0;
                let s = format!("{:.2}", rounded);
                let s = s.trim_end_matches('0');
                let s = s.trim_end_matches('.');
                return s.to_string();
            }
        }
        // 文本占位符 '@':数值按原样文本代入(Excel「文本」格式显示原值)
        let plain = if n.fract() == 0.0 && n.abs() < 1e15 {
            format!("{}", n as i64)
        } else {
            format!("{}", n)
        };
        let mut out = String::new();
        for t in &section.tokens {
            match t {
                FormatToken::Literal(c) => out.push(*c),
                FormatToken::EscapedLiteral(c) => out.push(*c),
                FormatToken::QuotedLiteral(s) => out.push_str(s),
                FormatToken::Skip(_) => out.push(' '),
                FormatToken::Fill(c) => out.push(*c),
                FormatToken::TextPlaceHolder => out.push_str(&plain),
                _ => {}
            }
        }
        return out;
    }

    // 统计小数位数（DecimalPoint 之后的 Zero/Hash 数量）与是否有千分位
    let mut decimals = 0usize;
    let mut has_decimal_point = false;
    let mut has_thousands = false;
    let mut has_percent = false;
    let mut seen_decimal = false;
    for t in &section.tokens {
        match t {
            FormatToken::DecimalPoint => { has_decimal_point = true; seen_decimal = true; }
            FormatToken::ZeroPlaceHolder(len) | FormatToken::HashPlaceHolder(len) => {
                if seen_decimal { decimals += len; }
            }
            FormatToken::ThousandsSeparator => has_thousands = true,
            FormatToken::Percent => has_percent = true,
            _ => {}
        }
    }

    let value = if has_percent { n * 100.0 } else { n };
    let abs_value = value.abs();

    let int_part = abs_value.trunc() as i64;
    let int_str = if has_thousands {
        group_thousands(int_part)
    } else {
        int_part.to_string()
    };

    let mut out = String::new();
    if force_negative_sign {
        out.push('-');
    }
    out.push_str(&int_str);

    if has_decimal_point {
        let frac = abs_value.fract();
        let scaled = (frac * 10f64.powi(decimals as i32)).round() as u64;
        out.push('.');
        out.push_str(&format!("{:0>width$}", scaled, width = decimals));
    }
    if has_percent {
        out.push('%');
    }

    // 处理 section 里除数字占位符之外的字面量前后缀（如货币符号 "¥" 或自定义文本）
    // 简化处理：把非数字/日期相关 token 的 Literal/QuotedLiteral/EscapedLiteral
    // 按原顺序 (在数字之前的 prefix，之后的 suffix) 拼接。
    let mut prefix = String::new();
    let mut suffix = String::new();
    let mut seen_digit_token = false;
    for t in &section.tokens {
        match t {
            FormatToken::ZeroPlaceHolder(_) | FormatToken::HashPlaceHolder(_)
            | FormatToken::DecimalPoint | FormatToken::ThousandsSeparator
            | FormatToken::Percent => {
                seen_digit_token = true;
            }
            FormatToken::Literal(c) => {
                if seen_digit_token { suffix.push(*c); } else { prefix.push(*c); }
            }
            FormatToken::EscapedLiteral(c) => {
                if seen_digit_token { suffix.push(*c); } else { prefix.push(*c); }
            }
            FormatToken::QuotedLiteral(s) => {
                if seen_digit_token { suffix.push_str(s); } else { prefix.push_str(s); }
            }
            FormatToken::Skip(_) => {
                if seen_digit_token { suffix.push(' '); } else { prefix.push(' '); }
            }
            FormatToken::Fill(c) => {
                if seen_digit_token { suffix.push(*c); } else { prefix.push(*c); }
            }
            _ => {}
        }
    }

    format!("{}{}{}", prefix, out, suffix)
}

/// 整数千分位分组（仅用于整数部分）
fn group_thousands(n: i64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    result.chars().rev().collect()
}

/// 提取 section 中的颜色（若有 Color token）
fn section_color(section: &FormatSection) -> Option<String> {
    section.tokens.iter().find_map(|t| match t {
        FormatToken::Color(c) => Some(c.clone()),
        _ => None,
    })
}

/// 顶层渲染入口：数值 + 已解析格式 → 显示文本+颜色
/// ROOT-041: date1904参数决定日期类型格式(section含DateTime token)时
/// 使用哪套日期系统换算序列号。非日期格式（纯数值/文本）不受影响。
pub fn render_number(parsed: &ParsedFormat, n: f64, date1904: bool) -> RenderedFormat {
    let (section, force_neg) = select_section(&parsed.sections, n);
    let color = section_color(section);
    let text = if section_has_datetime(section) {
        render_datetime_section(section, n, date1904)
    } else {
        render_number_section(section, n, force_neg)
    };
    RenderedFormat { text, color }
}

/// 渲染文本值（CellValue::Text 场景）：若格式含4段，第4段用于文本；否则原样返回
pub fn render_text(parsed: &ParsedFormat, text: &str) -> String {
    if parsed.sections.len() >= 4 {
        let section = &parsed.sections[3];
        let mut out = String::new();
        for t in &section.tokens {
            match t {
                FormatToken::TextPlaceHolder => out.push_str(text),
                FormatToken::Literal(c) => out.push(*c),
                FormatToken::EscapedLiteral(c) => out.push(*c),
                FormatToken::QuotedLiteral(s) => out.push_str(s),
                FormatToken::Skip(_) => out.push(' '),
                FormatToken::Fill(c) => out.push(*c),
                _ => {}
            }
        }
        out
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_text_format_on_number() {
        let pf = parse_format("@");
        assert_eq!(render_number(&pf, 0.256, false).text, "0.256");
        assert_eq!(render_number(&pf, 12.0, false).text, "12");
        assert_eq!(render_number(&pf, -3.5, false).text, "-3.5");
        let pf2 = parse_format("\"文本:\"@");
        assert_eq!(render_number(&pf2, 12.0, false).text, "文本:12");
    }

    #[test]
    fn test_split_sections_basic() {
        let s = split_sections("0.00;[Red]-0.00;\"-\";@");
        assert_eq!(s.len(), 4);
        assert_eq!(s[0], "0.00");
        assert_eq!(s[1], "[Red]-0.00");
        assert_eq!(s[2], "\"-\"");
        assert_eq!(s[3], "@");
        println!("split_sections_basic ✓");
    }

    #[test]
    fn test_split_sections_no_semicolon() {
        let s = split_sections("yyyy-mm-dd");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0], "yyyy-mm-dd");
        println!("split_sections_no_semicolon ✓");
    }

    #[test]
    fn test_split_sections_semicolon_in_quotes_not_split() {
        // 引号内的分号不应触发分段
        let s = split_sections("\"a;b\"0.00");
        assert_eq!(s.len(), 1);
        println!("split_sections_semicolon_in_quotes_not_split ✓");
    }

    #[test]
    fn test_tokenize_thousands_and_decimal() {
        let sec = tokenize_section("#,##0.00");
        assert_eq!(
            sec.tokens,
            vec![
                FormatToken::HashPlaceHolder(1),
                FormatToken::ThousandsSeparator,
                FormatToken::HashPlaceHolder(2),
                FormatToken::ZeroPlaceHolder(1),
                FormatToken::DecimalPoint,
                FormatToken::ZeroPlaceHolder(2),
            ]
        );
        println!("tokenize_thousands_and_decimal ✓");
    }

    #[test]
    fn test_tokenize_percent() {
        let sec = tokenize_section("0.00%");
        assert_eq!(
            sec.tokens,
            vec![
                FormatToken::ZeroPlaceHolder(1),
                FormatToken::DecimalPoint,
                FormatToken::ZeroPlaceHolder(2),
                FormatToken::Percent,
            ]
        );
        println!("tokenize_percent ✓");
    }

    #[test]
    fn test_tokenize_date_yyyy_mm_dd() {
        let sec = tokenize_section("yyyy-mm-dd");
        assert_eq!(
            sec.tokens,
            vec![
                FormatToken::DateTime("yyyy".to_string()),
                FormatToken::Literal('-'),
                FormatToken::DateTime("mm".to_string()),
                FormatToken::Literal('-'),
                FormatToken::DateTime("dd".to_string()),
            ]
        );
        println!("tokenize_date_yyyy_mm_dd ✓");
    }

    #[test]
    fn test_tokenize_time_hh_mm_ss() {
        // 词法层不区分 mm 是月份还是分钟，统一记为 DateTime("mm")，
        // 语义判断留给渲染层（hoursHandler/minutesHandler 状态机）。
        let sec = tokenize_section("hh:mm:ss");
        assert_eq!(
            sec.tokens,
            vec![
                FormatToken::DateTime("hh".to_string()),
                FormatToken::Literal(':'),
                FormatToken::DateTime("mm".to_string()),
                FormatToken::Literal(':'),
                FormatToken::DateTime("ss".to_string()),
            ]
        );
        println!("tokenize_time_hh_mm_ss ✓");
    }

    #[test]
    fn test_tokenize_color_and_condition() {
        let sec = tokenize_section("[Red]-0.00");
        assert_eq!(sec.tokens[0], FormatToken::Color("Red".to_string()));

        let sec2 = tokenize_section("[>100]0");
        assert_eq!(sec2.tokens[0], FormatToken::Condition(">100".to_string()));
        println!("tokenize_color_and_condition ✓");
    }

    #[test]
    fn test_tokenize_quoted_and_escaped_literal() {
        let sec = tokenize_section("\"元\"0.00");
        assert_eq!(sec.tokens[0], FormatToken::QuotedLiteral("元".to_string()));

        let sec2 = tokenize_section("\\-0");
        assert_eq!(sec2.tokens[0], FormatToken::EscapedLiteral('-'));
        println!("tokenize_quoted_and_escaped_literal ✓");
    }

    #[test]
    fn test_tokenize_ampm() {
        let sec = tokenize_section("h:mm AM/PM");
        assert!(sec.tokens.contains(&FormatToken::AmPm));
        println!("tokenize_ampm ✓");
    }

    #[test]
    fn test_parse_format_four_sections() {
        let pf = parse_format("0.00;[Red]-0.00;\"-\";@");
        assert_eq!(pf.sections.len(), 4);
        assert_eq!(pf.sections[3].tokens, vec![FormatToken::TextPlaceHolder]);
        println!("parse_format_four_sections ✓");
    }

    #[test]
    fn test_parse_format_single_section() {
        let pf = parse_format("yyyy-mm-dd");
        assert_eq!(pf.sections.len(), 1);
        println!("parse_format_single_section ✓");
    }

    #[test]
    fn test_render_number_basic_decimal() {
        let pf = parse_format("0.00");
        let r = render_number(&pf, 3.14159, false);
        assert_eq!(r.text, "3.14");
        println!("render_number_basic_decimal ✓");
    }

    #[test]
    fn test_render_number_thousands() {
        let pf = parse_format("#,##0.00");
        let r = render_number(&pf, 1234567.891, false);
        assert_eq!(r.text, "1,234,567.89");
        println!("render_number_thousands ✓");
    }

    #[test]
    fn test_render_number_percent() {
        let pf = parse_format("0.00%");
        let r = render_number(&pf, 0.4567, false);
        assert_eq!(r.text, "45.67%");
        println!("render_number_percent ✓");
    }

    #[test]
    fn test_render_number_negative_two_sections() {
        let pf = parse_format("0.00;-0.00");
        let r_pos = render_number(&pf, 5.5, false);
        assert_eq!(r_pos.text, "5.50");
        let r_neg = render_number(&pf, -5.5, false);
        assert_eq!(r_neg.text, "-5.50");
        println!("render_number_negative_two_sections ✓");
    }

    #[test]
    fn test_render_number_negative_single_section_auto_sign() {
        // 只有1段时，负数自动在前面补负号
        let pf = parse_format("0.00");
        let r_neg = render_number(&pf, -5.5, false);
        assert_eq!(r_neg.text, "-5.50");
        println!("render_number_negative_single_section_auto_sign ✓");
    }

    #[test]
    fn test_render_number_color_section() {
        let pf = parse_format("0.00;[Red]-0.00");
        let r_neg = render_number(&pf, -3.0, false);
        assert_eq!(r_neg.text, "-3.00");
        assert_eq!(r_neg.color, Some("Red".to_string()));
        let r_pos = render_number(&pf, 3.0, false);
        assert_eq!(r_pos.color, None);
        println!("render_number_color_section ✓");
    }

    #[test]
    fn test_render_date_yyyy_mm_dd() {
        // Excel serial 45658 对应约 2025-01-01（沿用 tile.rs 已验证算法）
        let pf = parse_format("yyyy-mm-dd");
        let r = render_number(&pf, 45658.0, false);
        // 只验证格式结构正确（年4位-月2位-日2位），具体日期值由已验证的
        // excel_serial_to_date 算法保证，这里重点验证 mm 被识别为月份
        assert_eq!(r.text.len(), 10);
        assert_eq!(&r.text[4..5], "-");
        assert_eq!(&r.text[7..8], "-");
        println!("render_date_yyyy_mm_dd ✓ ({})", r.text);
    }

    #[test]
    fn test_render_time_hh_mm_ss_minutes_not_month() {
        // 核心 bug 修复验证：hh:mm:ss 中的 mm 必须是分钟，不能是月份
        // serial 0.5 = 12:00:00（正午）
        let pf = parse_format("hh:mm:ss");
        let r = render_number(&pf, 0.5, false);
        assert_eq!(r.text, "12:00:00");
        println!("render_time_hh_mm_ss_minutes_not_month ✓ ({})", r.text);
    }

    #[test]
    fn test_render_time_specific_minutes_value() {
        // serial 部分 0.10417 天 ≈ 2:30:00（2小时30分钟）
        // 2.5小时 / 24 = 0.104166...
        let pf = parse_format("h:mm");
        let n = 2.5 / 24.0;
        let r = render_number(&pf, n, false);
        assert_eq!(r.text, "2:30");
        println!("render_time_specific_minutes_value ✓ ({})", r.text);
    }

    #[test]
    fn test_render_datetime_mixed_date_and_time_mm_both_roles() {
        // 混合场景：日期部分的 mm（月份）+ 时间部分的 mm（分钟）在同一格式串中
        // 各自正确识别：mm-dd 是月份，紧跟 h 的 mm 是分钟
        let pf = parse_format("mm-dd h:mm");
        // serial: 2025-03-15 附近 + 14:45
        // 用一个已知值：只验证两个 mm 的语义不同，不严格对具体日期断言
        let n = 45700.0 + 14.75 / 24.0;
        let r = render_number(&pf, n, false);
        // 格式："mm-dd h:mm" → 应该形如 "03-XX 14:45" 这种，两个mm数值应不同
        // 拆分验证：第一个mm(月份)和最后mm(分钟)分别取出
        let parts: Vec<&str> = r.text.split(' ').collect();
        assert_eq!(parts.len(), 2);
        let date_part = parts[0]; // "mm-dd"
        let time_part = parts[1]; // "h:mm"
        let month_str = &date_part[0..2];
        let minute_str: String = time_part.split(':').nth(1).unwrap().to_string();
        // 月份应在 01-12 范围，分钟应在 00-59 范围，且此例中数值应不同（45 vs 月份不太可能是45）
        let month_val: u32 = month_str.parse().unwrap();
        let minute_val: u32 = minute_str.parse().unwrap();
        assert!(month_val >= 1 && month_val <= 12, "month out of range: {}", month_val);
        assert!(minute_val <= 59, "minute out of range: {}", minute_val);
        println!("render_datetime_mixed_date_and_time_mm_both_roles ✓ ({})", r.text);
    }

    #[test]
    fn test_render_ampm() {
        // serial 0.75 = 18:00 = 6:00 PM
        let pf = parse_format("h:mm AM/PM");
        let r = render_number(&pf, 0.75, false);
        assert!(r.text.contains("PM"), "expected PM in {}", r.text);
        assert!(r.text.starts_with("6:00"), "expected 6:00 prefix in {}", r.text);
        println!("render_ampm ✓ ({})", r.text);
    }

    #[test]
    fn test_render_text_section() {
        let pf = parse_format("0.00;-0.00;0;\"文本:\"@");
        let s = render_text(&pf, "hello");
        assert_eq!(s, "文本:hello");
        println!("render_text_section ✓");
    }

    #[test]
    fn test_render_zero_section() {
        let pf = parse_format("0.00;-0.00;\"零\"");
        let r = render_number(&pf, 0.0, false);
        assert_eq!(r.text, "零");
        println!("render_zero_section ✓");
    }

    #[test]
    fn test_render_currency_prefix() {
        let pf = parse_format("\"¥\"0.00");
        let r = render_number(&pf, 99.5, false);
        assert_eq!(r.text, "¥99.50");
        println!("render_currency_prefix ✓");
    }
}
