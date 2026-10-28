<div align="center">

# roe-core

**The Headless Core Engine of ROE Office (Rust Office Engine)**  
*Pure Rust • Memory-Safe • Headless Document Kernel • Modular Crates*

[![License: AGPL v3](https://img.shields.io/badge/License-AGPLv3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Built with Rust](https://img.shields.io/badge/Rust-1.80%2B-orange.svg?logo=rust)](https://www.rust-lang.org/)
[![Architecture: Headless](https://img.shields.io/badge/Architecture-Headless%20First-green.svg)](#architecture)
[![UI Consumer](https://img.shields.io/badge/UI%20Frontend-roe--desktop-purple.svg)](https://github.com/roe-office/roe-desktop)

</div>

---

## 📖 Overview

`roe-core` is the foundational headless computational engine of the ROE Office suite. It handles document parsing, memory data models, formula evaluation DAGs, typography layout, 2D vector rendering pipelines, and AI agent execution—**completely decoupled from any graphical user interface (GUI)**.

It is primarily consumed by [`roe-desktop`](https://github.com/roe-office/roe-desktop) (the Slint-based desktop frontend) and can be leveraged independently for server-side document automation, batch rendering, or programmatic AI toolchains via C-ABI and MCP.

---

## 🏗️ Workspace Architecture

The workspace is organized into modular, fine-grained Rust crates:

| Crate | Responsibility | Key Dependencies |
| :--- | :--- | :--- |
| **`roe-model`** | Core document object models & state trees | `serde`, `serde_json` |
| **`roe-core`** | Central orchestration, layout abstractions & 2D rendering pipeline | `tiny-skia`, `resvg` |
| **`roe-parser`** | High-speed OOXML parsing (`.docx`, `.xlsx`, `.pptx`) | `quick-xml` |
| **`roe-writer`** | Advanced text layout, line-breaking & paragraph flow | `parley`, `cosmic-text`, `taffy` |
| **`roe-calc`** | Spreadsheet formula evaluation engine & recalculation graph | `formualizer`, `petgraph` (FormulaDAG) |
| **`roe-impress`** | Presentation slide scene-graph & spatial spatial index | `rstar`, `charts-rs` |
| **`roe-formula`** | LaTeX math formula parsing and KaTeX typography | `RaTeX` (in-tree) |
| **`roe-script`** | Sandboxed, auditable scripting engine & read-only previews | `rhai` |
| **`roe-polars-bridge`** | High-performance big data analytics & instant pivot tables | `polars` |
| **`roe-api`** | Unified C-ABI facade (270+ functions) for multi-language consumers | C-ABI |
| **`roe-agent`** | AI tool abstraction & native Agent integration | `rig-core`, `rig-agent` |
| **`roe-export`** | Headless document export (PDF, HTML, etc.) | — |
| **`roe-bridge-lok`** / **`roe-uno-compat`** | Compatibility layer inspired by LibreOfficeKit / UNO idioms | — |

---

## 🛠️ Technology Stack

- **Calculation & Graph**:
  - `formualizer`: Fast Excel / OpenFormula parser and evaluator.
  - `petgraph`: Directed Acyclic Graph (DAG) for reactive formula dependency management.
  - `rstar`: R*-tree spatial indexing for interactive presentation objects and hit-testing.
- **Typography & Vector Graphics**:
  - `parley` / `taffy` / `skrifa` / `swash` / `cosmic-text`: State-of-the-art modular font handling and layout.
  - `RaTeX`: LaTeX mathematical formula typesetting rendered with KaTeX font metrics.
  - `charts-rs`: Lightweight declarative charting and data visualization.
  - `tiny-skia` / `resvg`: Pure software 2D rasterization without heavy GPU dependencies.
- **Data & Automation**:
  - `polars`: Columnar DataFrame computation engine for millions of rows.
  - `quick-xml`: Zero-allocation streaming XML parsing for OOXML structures.
  - `rhai`: Lightweight, sandboxed scripting language replacing legacy VBA.
  - `rig-core`: Rust-native AI agent tool orchestrator.

---

## 🚀 Building & Testing

Ensure you have a recent stable Rust toolchain installed (Rust 1.80+ recommended):

```bash
# Check workspace compilation without generating binaries
cargo check --workspace

# Run lib unit tests across all workspace crates
cargo test --workspace --lib
```

##  📌 Development Notes

 Vendored Patches: vendor/parley and vendor/formualizer-parse are in-tree patched versions maintained for layout fixes and upstream compatibility. They are integrated via workspace exclude and [patch.crates-io] rules. Refer to code comments inside respective crates for details.

 ## 📜 Licensing

roe-core is licensed under the GNU Affero General Public License v3.0 (AGPLv3).
For special entity commercial exemptions (Anthropic, Alphabet/Google, DeepSeek, and Public Good NGOs), please consult the organization-level Licensing Declaration.
