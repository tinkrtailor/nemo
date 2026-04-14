# Test: Hello World Function

## Goal

Add a `hello_world()` function to the codebase that returns the string `"hello, world"`.

Place the function in a new file `hello_world.rs` (or equivalent for the project's primary language) at a sensible location in the repo. If the project is a Rust workspace, add it as a public function in a suitable existing crate or a new module.

## Acceptance Criteria

- A function named `hello_world` exists and returns the string `"hello, world"` (exact value, lowercase, with comma and space).
- At least one test calls `hello_world()` and asserts the return value equals `"hello, world"`.
- All existing tests continue to pass.
- No existing files are removed or broken.

## Notes

This spec is intentionally trivial. Its purpose is to exercise the full nautiloop pipeline (implement, review, test stages) end-to-end without making meaningful changes to the codebase.
