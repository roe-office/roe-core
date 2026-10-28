// 独立 example，不改动任何现有文件。
// 用途：生成一个真实 /tmp/styles_test.docx 文件，供人工在
// Word / WPS / LibreOffice 里打开，验证：
// 1. "样式"面板里能看到自定义样式 "MyHeading" / "MyBody" / "MyIndented"
// 2. 应用了 "MyHeading" 样式的段落，粗体+居中+字号16pt+深红色是否生效
// 3. 应用了 "MyBody" 样式的段落（based_on MyHeading），斜体是否
//    在继承 MyHeading 格式的基础上叠加生效（用来验证 basedOn 继承
//    链在真实渲染器里是否被正确处理，不只是我们自己 reparse 测试
//    里的字符串层面正确）
// 4. 应用了 "MyIndented" 样式的段落，悬挂缩进（indent_first负值）/
//    左缩进/1.5倍行距是否生效——这一批是本次新补齐的字段
//
// 运行方式：
//   cd ~/Projects/roe
//   cargo run -p roe-parser --example gen_styles_test

use roe_model::common::{Color, StyleRef};
use roe_model::style::{Alignment, CharStyle, ParagraphStyle, StyleRegistry};
use roe_model::writer::{BodyElement, Paragraph, Run, WriterDocument};
use roe_parser::ooxml::docx_writer::DocxWriter;

fn main() {
    let mut registry = StyleRegistry::default();

    registry.paragraph_styles.insert(
        "MyHeading".to_string(),
        ParagraphStyle {
            name: "MyHeading".to_string(),
            bold: true,
            alignment: Some(Alignment::Center),
            font_size: Some(16.0),
            color: Some(Color { r: 0xC0, g: 0x00, b: 0x00, a: 255 }), // 深红色
            ..Default::default()
        },
    );

    registry.paragraph_styles.insert(
        "MyBody".to_string(),
        ParagraphStyle {
            name: "MyBody".to_string(),
            based_on: Some("MyHeading".to_string()),
            italic: true,
            font_size: Some(11.0),
            ..Default::default()
        },
    );

    // 新增：验证本批次补齐的 indent_left/indent_first/space_before/
    // space_after/line_height 字段。indent_first 用负值测试 w:hanging
    // 悬挂缩进分支（首行不缩进，后续行整体右移，常见于编号列表/参考
    // 文献场景，肉眼比"首行缩进"更容易一眼分辨对不对）。
    //
    // 注意：space_before/space_after/line_height 这三个字段虽然写出
    // 正确，但 styles.xml 的 ParagraphStyle 解析路径目前不支持读回
    // （docx.rs 里 in_ppr 分支缺少 b"w:spacing"，是本次摸底新发现的
    // parser 缺口，见交接文档记录），所以这里生成的真实文件里，如果
    // 后续再拿去反向 roe 自己解析读回，这三个值会读不到——但这不影响
    // Word/WPS/LibreOffice 三方渲染器本身是否正确显示，因为它们是按
    // OOXML 标准直接读 XML，不经过 roe 自己的 read_styles 这条路径。
    registry.paragraph_styles.insert(
        "MyIndented".to_string(),
        ParagraphStyle {
            name: "MyIndented".to_string(),
            indent_left: Some(36.0),
            indent_first: Some(-18.0), // 悬挂缩进
            space_before: Some(12.0),
            space_after: Some(12.0),
            line_height: Some(1.5),
            ..Default::default()
        },
    );

    // 新增：CharStyle 写出层验证。Emphasis2 用来测试 Run.char_style ->
    // w:rStyle 引用链路（本次会话新发现并修复的写出层缺口——此前只有
    // 解析层支持读 w:rStyle，写出层从未实现，定义了字符样式也无法在
    // 文档里实际引用），以及 CharStyle 本身八个基础字段（不含
    // background，那是独立的下一小步）是否在真实渲染器里正确显示：
    // 加粗+下划线+特定字体+深蓝色。
    registry.char_styles.insert(
        "Emphasis2".to_string(),
        CharStyle {
            name: "Emphasis2".to_string(),
            bold: true,
            underline: true,
            font_family_ascii: Some("Courier New".to_string()),
            color: Some(Color { r: 0x00, g: 0x00, b: 0x8B, a: 255 }), // 深蓝色
            // background：本次会话新打通的字段，走 w:shd（任意RGB），不是
            // w:highlight（16种预设色名）。故意选一个不在16种预设色里的
            // 紫色，如果渲染器显示出的背景色恰好是这个紫色（而不是被
            // 悄悄换成某个相近的预设色），说明w:shd路径真的生效了。
            background: Some(Color { r: 0xD8, g: 0xBF, b: 0xF0, a: 255 }), // 淡紫色
            ..Default::default()
        },
    );

    let mut heading_para = Paragraph::default();
    heading_para.style = StyleRef("MyHeading".to_string());
    heading_para.runs.push(Run {
        text: "这是应用了 MyHeading 样式的标题（应该是粗体+居中+16pt）".to_string(),
        ..Default::default()
    });

    let mut body_para = Paragraph::default();
    body_para.style = StyleRef("MyBody".to_string());
    body_para.runs.push(Run {
        text: "这是应用了 MyBody 样式的正文（继承自 MyHeading，应该是斜体，字号被覆盖为11pt）".to_string(),
        ..Default::default()
    });

    let mut plain_para = Paragraph::default();
    plain_para.runs.push(Run {
        text: "这是没有应用任何自定义样式的普通段落，作为对照组。".to_string(),
        ..Default::default()
    });

    // 新增：一个段落里混合"应用了 Emphasis2 字符样式的run"和"普通run"，
    // 用来验证字符样式只影响被引用的那一小段文字，不会像段落样式一样
    // 波及整段。
    let mut char_style_para = Paragraph::default();
    char_style_para.runs.push(Run {
        text: "这段文字前半部分是普通文字，后半部分——".to_string(),
        ..Default::default()
    });
    char_style_para.runs.push(Run {
        text: "这几个字应用了Emphasis2字符样式（应该是加粗+下划线+Courier New字体+深蓝色）".to_string(),
        char_style: Some(StyleRef("Emphasis2".to_string())),
        ..Default::default()
    });
    char_style_para.runs.push(Run {
        text: "——这里恢复普通文字。".to_string(),
        ..Default::default()
    });

    let mut indented_para = Paragraph::default();
    indented_para.style = StyleRef("MyIndented".to_string());
    indented_para.runs.push(Run {
        text: "这是应用了 MyIndented 样式的段落，用来验证悬挂缩进（首行不缩进、后续行整体右移18pt）、左缩进36pt、以及1.5倍行距。这段文字刻意写得比较长，为的是让它自动换行到第二行，这样才能肉眼观察到悬挂缩进的效果——如果第二行相对第一行整体向右移动了一截，说明缩进生效了；如果行与行之间的间距明显比其他段落更宽松，说明1.5倍行距也生效了。"
            .to_string(),
        ..Default::default()
    });

    let doc = WriterDocument {
        body: vec![
            BodyElement::Paragraph(heading_para),
            BodyElement::Paragraph(body_para),
            BodyElement::Paragraph(plain_para),
            BodyElement::Paragraph(char_style_para),
            BodyElement::Paragraph(indented_para),
        ],
        styles: registry,
        ..Default::default()
    };

    let bytes = DocxWriter::write(&doc).expect("write failed");
    std::fs::write("/tmp/styles_test.docx", &bytes).expect("write file failed");

    println!("已生成 /tmp/styles_test.docx，共 {} 字节", bytes.len());
    println!("请在 Word / WPS / LibreOffice 中打开，检查：");
    println!("1. 样式面板（Word: 开始->样式；WPS/LO 类似）里能否看到 MyHeading / MyBody / MyIndented 三个自定义样式");
    println!("2. 第一段是否显示为：粗体、居中、字号明显偏大（16pt）、深红色");
    println!("3. 第二段是否显示为：斜体、字号偏小（11pt）——重点看 based_on 继承是否生效");
    println!("4. 第三段（对照组）是否保持普通默认格式，不受前后样式影响");
    println!("5. 第四段中间那几个字是否只有中间部分显示为：加粗+下划线+Courier New字体+深蓝色，前后两截保持普通格式（验证字符样式只影响引用它的run，不影响整段）");
    println!("6. 第五段是否显示为：悬挂缩进（第二行起整体右移）、行距明显更宽松（1.5倍）");
}
