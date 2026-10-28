#!/usr/bin/env python3
"""
把 probe_cxn.py 的探测结果（/tmp/cxn_probe_result.json）转成 Rust CxnDef
数组字面量，逐个替换进 preset_geometry.rs 里对应常量当前的占位
`cxn_lst: &[],`。

匹配依据：常量的 name 字段字符串 == 探测结果的 prst key（已交叉验证
174/174 全部精确匹配，2026-09）。

ang/x/y 三个 GuideRef 字段的判断规则（已用真实数据核实，见本次会话记录）：
- 纯数字字符串（如 "0"、"1800000"）-> GuideRef::lit(数值 as f64)
- 其他（具名角度常量如 "_3cd4"/"cd4"，或形状私有 guide 名如 "hc"/"il"/"x1"）
  -> GuideRef::named("原字符串")
这与 Rust resolve() 对 Named 查表失败 fallback 到 0.0（不panic）的行为
配套：若本脚本误判某 token 类型，不会编译报错也不会运行时panic，只会
产生错误坐标——因此翻译完成后必须做抽样人工核对，不能只满足于编译通过。
"""
import json
import re
import sys

PROBE_RESULT = "/tmp/cxn_probe_result.json"
RS_PATH = "/home/yong/Projects/roe/roe-model/src/preset_geometry.rs"

NUMERIC_RE = re.compile(r"^-?\d+$")

def guide_ref(token: str) -> str:
    if NUMERIC_RE.match(token):
        return f"GuideRef::lit({float(token)})"
    else:
        return f'GuideRef::named("{token}")'

def format_cxn_lst(cnxs) -> str:
    if not cnxs:
        return "&[]"
    lines = ["&["]
    for ang, x, y in cnxs:
        lines.append(
            f"        CxnDef {{ ang: {guide_ref(ang)}, x: {guide_ref(x)}, y: {guide_ref(y)} }},"
        )
    lines.append("    ]")
    return "\n".join(lines)

def main():
    with open(PROBE_RESULT, encoding="utf-8") as f:
        probe = json.load(f)

    with open(RS_PATH, encoding="utf-8") as f:
        content = f.read()

    # 逐个常量定位：找 `name: "xxx",` ... 直到该常量块内的 `cxn_lst: &[],`（占位）
    # 用非贪婪匹配，块与块之间以下一个 `pub static` 或文件结尾为界，避免跨常量误匹配
    replaced = 0
    not_found = []
    for prst_name, cnxs in probe.items():
        # 定位这个 name 所在的常量声明起点
        name_marker = f'name: "{prst_name}",'
        idx = content.find(name_marker)
        if idx == -1:
            not_found.append(prst_name)
            continue

        # 从 idx 开始，找下一个 "cxn_lst: &[]," 占位（该常量块内的字段，
        # 且必须早于下一个 "pub static" 出现，否则说明占位缺失/越界，报错而非静默替换）
        next_static_idx = content.find("\npub static ", idx + 1)
        if next_static_idx == -1:
            next_static_idx = len(content)

        placeholder = "    cxn_lst: &[],"
        placeholder_idx = content.find(placeholder, idx)
        if placeholder_idx == -1 or placeholder_idx > next_static_idx:
            print(f"[error] {prst_name}: 在其常量块范围内找不到占位 cxn_lst，跳过", file=sys.stderr)
            not_found.append(prst_name)
            continue

        new_field = f"    cxn_lst: {format_cxn_lst(cnxs)},"
        content = (
            content[:placeholder_idx]
            + new_field
            + content[placeholder_idx + len(placeholder):]
        )
        replaced += 1

    with open(RS_PATH, "w", encoding="utf-8") as f:
        f.write(content)

    print(f"[info] 成功替换 {replaced} / {len(probe)} 个形状的 cxn_lst", file=sys.stderr)
    if not_found:
        print(f"[error] 未能替换的形状: {not_found}", file=sys.stderr)

if __name__ == "__main__":
    main()
