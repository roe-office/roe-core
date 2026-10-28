#!/usr/bin/env python3
"""从 writer_api.rs 的 register_fn 条目生成 roe-script/src/writer_api_meta.rs。接口变更后重跑。"""
import re, json, pathlib
ROOT = pathlib.Path(__file__).resolve().parent.parent
def _prod(n):
    s = (ROOT / n).read_text(encoding="utf-8")
    return s[:s.index("\nmod tests")]
src = _prod("roe-script/src/writer_api.rs") + "\n" + _prod("roe-script/src/writer_api_ext.rs") + "\nmod tests\n"
cut = src.index("\nmod tests")  # 只扫生产代码,不扫测试模块
body = src[:cut]
sq = lambda s: re.sub(r"\s+", " ", s).strip()
rows = []
for m in re.finditer(r'register_fn\(\s*"([^"]+)"\s*,\s*(?:move\s*)?(\|[^|]*\|)\s*(?:->\s*([^{]+?))?\s*\{', body):
    params = sq(m.group(2).strip("|")).rstrip(",").strip()
    ret = sq(m.group(3) or "()")
    rows.append((m.group(1), params, ret))
names = [r[0] for r in rows]
assert rows, "没解析到任何函数"
assert len(names) == len(set(names)), "重名: %s" % sorted({n for n in names if names.count(n) > 1})
out = ["// 自动生成,勿手改。脚本: scripts/gen_writer_api_meta.py(接口变更后重跑)",
       "// (名称, 参数, 返回)。Writer 没有只读/可写标记:writer_api 目前不区分只读引擎。",
       "pub static WRITER_API_META: &[(&str, &str, &str)] = &["]
for n, p, r in rows:
    out.append("    (%s, %s, %s)," % (json.dumps(n), json.dumps(p, ensure_ascii=False), json.dumps(r, ensure_ascii=False)))
out.append("];")
(ROOT / "roe-script/src/writer_api_meta.rs").write_text("\n".join(out) + "\n", encoding="utf-8")
print("条目:", len(rows))
