# GPUI migration

The isolated desktop workspace uses Longbridge's published Zed GPUI snapshot, `gpui-pre` and `gpui-pre-platform` 0.3.12, together with `gpui-component` 0.10.0. The component crate requires this exact GPUI version. The platform crate supplies the Linux Wayland and X11 backends. The GUI starts via `gpui_platform::application()`.

The multiline composers use `TextareaState` and `Textarea`. Keep the single-line inputs for fields. The platform submit shortcut is `cmd-enter` on macOS and `ctrl-enter` on Linux. GPUI parses bindings at runtime, so the packaged app must launch in CI. TextView markdown now takes an element ID and a source string. The maximum scroll offset now has point coordinates.

The original Linux xattr workaround has gone. The desktop still uses libc for process-group shutdown. Its lockfile now resolves libc 0.2.191. The dependency contract checks the standalone CLI, desktop core and probe for GPUI dependencies.
