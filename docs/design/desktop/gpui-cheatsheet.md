# GPUI workbench conventions

Use `gpui_platform::application()` with the pinned platform backends. The existing workbench is the default. Pass `--desk-next` to exercise the new store and native views.

Use `kit::button` for a labeled action. It delegates pointer and keyboard activation, focus traversal and disabled state to gpui-component. Use the shared theme's primary and secondary colors. Fields use `InputState` for single lines and `TextareaState` for the multiline composer.

Render constant-height thread rows with `uniform_list` and `cx.processor`. Its range contains only rows needed for the current viewport and overscan. For an entity's background subscription, own its task in that entity. Drain the inbox on the GPUI executor and apply updates outside the queue lock.

Run `ox-desktop --desk-next --desk-smoke /path/to/project` on a display to render and exit. `--desk-stats` outputs GPUI draw and dirty-to-present measurements together with inbox and view counters for the new workbench.

Use the shared list row for a session title, selection, billing route and status. Its windowless text snapshots exercise the same labels used by the GPUI control. The shared sheet provides scrollable request details. Native dropdown menus handle project, route and permission choices. Use the shared field constructor with masking enabled for credentials and secret questions.

The transcript uses a variable-height GPUI list, bottom follow mode and targeted row remeasurement. Scrolling up exposes Jump to latest. Opening an earlier history page prepends rows and preserves the viewport. Project and route switches keep the entire workbench entity, including editor selection and list position.

On Linux the palette watcher listens to the parent directories of both generations of the Omarchy theme path and their resolved targets. Replacing a symlink reinstalls the watches. The system appearance callback supplies the light or dark palette when no Omarchy file exists.
