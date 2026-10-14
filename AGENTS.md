# minihud — project rules

## MCPs, skills and plugins

**Pre-flight gate — before any action, with no exceptions.** On every request —
a task, a question, research, debugging, even a one-line answer — and before
any tool call: scan your available MCPs, skills, and plugins, then load or
consult every one whose domain matches the request or its likely next steps.
"Nothing matched" is a claim that requires having checked, not a guess. Never
answer from memory when a matching source could verify it.

### MCPs

| MCP | Use it for | When it is required |
|---|---|---|
| `context7` | Library and API docs | Any external-library or version-sensitive API question (pinned stack in the section below) |
| `rust-analyzer` | Real diagnostics, hover, go-to-definition, references in this workspace | Before using any Rust or `windows`-crate API — never invent one from memory |
| `gitmcp` | Upstream docs for `gustavomgama/minihud` | When the answer should come from minihud's own docs — check before a web search |
| `github` | Issues, PRs, releases, code search | Any work that touches GitHub |
| `rea` | Reverse-engineering native and managed binaries | PresentMon, Windows API internals, or other local artifacts |
| `egui-mcp` | Driving a live egui UI: observe → act → verify | Any UI change — verify the rendered result before claiming it works |

Configured MCPs not listed here (Postman, Neon, Render, Rails) are unrelated to
this project — leave them alone.

### Skills

| Skill | Load it when |
|---|---|
| `test-driven-development` | Before any production code — mandatory, see the TDD section below |
| `rust` | Any Rust task; runs the fmt/clippy/nextest gate |
| `jev` | Before writing or revising Jev questions — never invent a rubric from scratch |

Load the skill with the `skill` tool before writing code in its domain, not
after you are stuck.

### Plugins

Plugins load automatically from the opencode config — no action needed. When a
plugin's domain matches the task, prefer it over hand-rolling the equivalent.

## Context7 (library docs)

Always use Context7 (`resolve-library-id` + `query-docs`) for any library/API
question — never answer from memory. This repo's pinned stack, with known
Context7 IDs where they exist:

| Dependency | Version | Context7 ID |
|---|---|---|
| `windows` | 0.61 | resolve `windows-rs` (microsoft/windows-rs) |
| `eframe` / `egui` | 0.36.2 | `/emilk/egui` |
| `serde` / `serde_json` | 1 | `/serde-rs/serde` |
| `toml` | 1.1.8 | resolve `toml-rs` |
| `tracing` | 0.1 | `/tokio-rs/tracing` |
| PresentMon | 2.6.0 (local fork) | `/GameTechDev/PresentMon` |

Pass `language: "Rust"` on every `query-docs` call, and mention the exact
version from `Cargo.toml` in the query so version-specific docs come back.

## TDD is mandatory (all agents, every session)

This project is **test-driven, without exception**. Before writing ANY production
code — a new feature, a bug fix, a refactor, or a behavior change:

1. **Load the `test-driven-development` skill** (via the `skill` tool) and follow
   it exactly. Do this before touching implementation code.
2. **Write ONE failing test first.** Run it and **watch it fail for the right
   reason** (feature missing — not a typo/compile error).
3. **Write the minimal code** to make it pass. Run the full suite.
4. **Refactor only while green.**

**The Iron Law: NO PRODUCTION CODE WITHOUT A FAILING TEST FIRST.**

If code was written before its test, **delete it and start over**. Do not keep it
as "reference", do not "adapt" it, do not look at it. Delete means delete.

Exceptions require explicit human permission: throwaway prototypes, generated
code, and configuration files.

When writing or changing tests, also read the skill's `writing-good-tests.md`
companion (name the break the test catches; exercise the real code, not mocks).

## Definition of done (Rust)

Run these in order; all must pass before declaring work complete:

1. `cargo fmt --all`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo nextest run --workspace` — fall back to `cargo test --workspace` if
   nextest is missing. On a workspace with zero tests nextest exits 4; use
   `cargo nextest run --workspace --no-tests=pass` to treat an empty suite as a
   pass.

Never mark work done with a red test or a clippy warning. Report failures with
`file:line` and the smallest fix.

For the full audit (the same checks CI runs), run
`powershell -ExecutionPolicy Bypass -File tools/audit.ps1`.
