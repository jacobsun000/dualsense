# Partial Transcript Text Loss Through Wayland

## Summary

Characters are missing when transcript partials are sent through one persistent `WrtypeClient`. The issue reproduces in a single thread and remains present even with a one-second delay between partials.

## Minimal Reproducer

Create one `WrtypeClient`, wait two seconds to focus the target application, and invoke `type_text()` once for each partial-like chunk:

```rust
let mut client = wrtype::WrtypeClient::new()?;
std::thread::sleep(std::time::Duration::from_secs(2));

for _ in 0..100 {
    for partial in ["Hello", ", ", "World", "; "] {
        client.type_text(partial)?;
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
```

This does not involve the controller, microphone, OpenAI, threads, or a transcript queue.

## Observed Result

The target application may receive output such as:

```text
HelloolHellool...
```

Characters remain missing despite the one-second delay.

## Expected Result

The target application should receive:

```text
Hello, World; Hello, World; ...
```

with no missing characters.

## Findings

The behavior points to repeated virtual-keyboard keymap replacement rather than queue loss or concurrent `WrtypeClient` access.

In `./references/wrtype`:

- `src/lib.rs` routes every `WrtypeClient::type_text()` call through `execute_commands()`.
- `src/executor.rs` uploads the current keymap at the start of every `execute_commands()` call.
- The text handler uploads the generated keymap again before sending key events.
- Each key press and release performs another Wayland roundtrip.
- Each command ends by resetting modifiers and performing another roundtrip.

Consequently, splitting one phrase into four partial calls repeatedly replaces the virtual keyboard keymap. A delay changes event timing but does not remove the keymap replacement. The target application or compositor may not handle these repeated keymap changes consistently.

The local wrtype documentation also identifies keymap batching, event batching, caching, and event coalescing as optimization opportunities:

```text
./references/wrtype/docs/src/architecture.md
./references/wrtype/docs/src/wayland-protocol.md
```

## Likely Cause

`WrtypeClient::type_text()` is a complete text-command API, but the transcription flow uses it as a streaming partial-text API. Repeated keymap uploads and synchronization boundaries can cause the compositor or target application to lose or misinterpret key events.

## Possible Fixes

- Coalesce several partials before calling `type_text()`.
- Keep one stable keymap and stream key events without re-uploading it.
- Modify `./references/wrtype` so keymaps are uploaded only when they actually change.
- Add a dedicated streaming API that keeps keymap and Wayland state alive across partials.
- Use clipboard/paste as a fallback for completed transcripts.
