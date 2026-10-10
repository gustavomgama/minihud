# minihud — project rules

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
