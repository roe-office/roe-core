// WROOT-040-HYPHEN-VERIFY：验证 hypher crate 能否正常引入、基本调用是否符合预期。
// 只做技术选型验证，不接入任何排版逻辑，验证完可以保留作为回归测试或后续删除。
#[test]
fn hypher_basic_call_works() {
    let syllables: Vec<&str> = hypher::hyphenate("different", hypher::Lang::English).collect();
    println!("different -> {:?}", syllables);
    let joined = syllables.join("-");
    println!("joined: {}", joined);
    assert!(joined.contains('-'), "应该至少有一个断点，实际结果: {}", joined);

    let syllables2: Vec<&str> = hypher::hyphenate("atmosphere", hypher::Lang::English).collect();
    let joined2 = syllables2.join("-");
    println!("atmosphere -> {}", joined2);
    assert!(joined2.contains('-'));
}
