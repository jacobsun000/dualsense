# Agent Notes

- Linux Rust project for DualSense input, semantic event decoding, and keyboard/mouse output via `uinput`.
- Keep direct mode and TUI mode on the same shared input/mapping code.
- TUI is the default Cargo feature; run it with `cargo run -- --tui`.
- Preserve Colemak compensation when changing logical keyboard mappings.
- Validate changes with `cargo fmt`, `cargo check`, `cargo check --features tui`, and `cargo test --features tui`.
- Use Conventional Commit messages.
