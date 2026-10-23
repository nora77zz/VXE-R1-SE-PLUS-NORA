# G HUB Lua compatibility

This crate implements an initial, host-side subset of the Logitech G-series Lua API for the ATK HUB reconstruction. The implementation is in `crates/ghub-lua` and uses the Lua 5.4 runtime supplied by `mlua 0.12.2`. It is a compatibility layer for selected script calls; it does not establish complete G HUB compatibility.

## Reference

The API contract used here is the *Logitech G-Series Lua API*, version 2023.5, 45 pages (SHA-256 `0e9f5532708c8e6d811eededa2dbba458d9f891bf3b5f503346dd93079052f52`). The reference PDF is a local validation input and is not included in this repository.

Relevant printed pages are: 5–6 for `OnEvent` and event arguments; 9–13 for `Sleep`, logging, and elapsed time; 18–21 for mouse buttons and button numbers; 24 for relative movement; 38 for primary-button event opt-in; and 42 for the documented Lua 5.4 math, string, and table libraries. Pages 36–37 explicitly say that G HUB itself does not implement its DPI-table functions.

## Implemented calls

| G HUB API | Behavior in this crate |
| --- | --- |
| `OnEvent(event, arg, family)` | A global Lua function receives events queued by the host. `arg` and `family` are `nil` when absent. The runtime recognizes the documented profile and mouse event names when the host sends them; it does not create those host events itself. |
| `EnablePrimaryMouseButtonEvents(enable)` | Enables or disables button 1 press/release events. Button 1 events are ignored by default, matching the manual. Both boolean values and numeric `1`/`0` are accepted. |
| `OutputLogMessage(...)` | Uses Lua's `string.format` rules and sends the resulting text to the host-provided log backend. A line ending is not added automatically. |
| `ClearLog()` | Clears the host-provided script log. |
| `GetRunningTime()` | Returns elapsed whole milliseconds since this script worker started. |
| `Sleep(milliseconds)` | Pauses the worker and wakes promptly when the host cancels it. |
| `IsMouseButtonPressed(button)` | Queries the host backend. |
| `PressMouseButton(button)` / `ReleaseMouseButton(button)` | Requests a mouse-button transition through the host backend. Successfully pressed buttons are tracked for cleanup. |
| `PressAndReleaseMouseButton(button)` | Requests press followed by release through the backend. |
| `MoveMouseRelative(x, y)` | Requests a relative movement through the backend. Positive and negative axis meanings follow the manual. |

Mouse button numbers follow the manual: 1 left, 2 middle, 3 right, 4 X1, and 5 X2. Calls that accept button numbers reject values outside 1–5.

The worker runs only after the host explicitly calls `GHubRuntime::start`. Events are delivered serially on that worker. Its event queue is bounded (128 entries by default); a full queue rejects a new event. A configured Lua memory limit defaults to 16 MiB. Cancellation is checked periodically by a Lua count hook, which yields the host-driven script thread when Lua permits yielding. `Sleep` is also woken when the host requests cancellation. On normal stop, script error, or unwind, the worker attempts to release every synthetic button it knows was pressed; any backend release errors appear in `WorkerReport.cleanup_errors`.

`stop()` waits for the worker to finish and attempt cleanup. Lua hooks can yield only from yieldable Lua contexts. If a script loops forever inside a non-yieldable C-library callback (for example, a function replacement passed to `string.gsub`) or inside a long-running C-library operation, the count hook cannot force that call to return, and standalone `stop()` may wait indefinitely. A backend callback that blocks also cannot be interrupted. The application should run this standalone runtime in a killable child process if it needs a hard timeout; the app-level process supervisor and its cleanup broker are separate from this crate. Dropping a handle requests cancellation but detaches the worker instead of blocking the caller; cleanup happens if and when that worker exits. The host must treat cleanup as an attempt, especially if a backend reports release failures.

## Not implemented or not established

- There is no Windows input backend in this crate. A future app layer must supply `InputBackend`, event capture, and any actual input delivery. The crate tests use only a mock backend.
- Keyboard calls, Logitech G/M-key events, macros, wheel movement, absolute cursor movement, mouse-position queries, LCD/backlight, and device-specific APIs are not implemented.
- DPI-table calls are not offered. The cited G HUB manual itself marks `SetMouseDPITable` and `SetMouseDPITableIndex` as unimplemented in G HUB, so this crate does not imply support for them on the VXE mouse.
- Profile activation and hardware events must be sent by the application through `dispatch_event`; this crate does not read profiles or monitor a physical device.
- The exact G HUB behavior for errors, timing, simultaneous profiles, multiple mice, and all documented standard-library edge cases has not been validated against Logitech hardware or software. This is a selected subset, not a drop-in replacement guarantee.

The state retains Lua 5.4's `pcall`, `xpcall`, coroutine, UTF-8, math, string, and table libraries. `coroutine.create` and `coroutine.wrap` are host wrappers that create mlua-managed threads so those coroutines inherit the cancellation hook; `coroutine.resume` and `coroutine.yield` remain the Lua standard-library functions. The protected-call functions remain available because cancellation is signaled by yielding the host-driven thread, rather than by a Lua error that a protected call could swallow. File, operating-system, package-loading, and debug access are removed. The host supplies only the input and logging operations listed above.

## Validation

The crate's six focused mock-backend tests cover cancellation inside `pcall`, cancellation through `coroutine.create` and `coroutine.wrap`, cancellation during `Sleep` with pressed-button cleanup, the default primary-button gate and its opt-in, formatting/log clearing plus mouse calls, and cleanup after a script error. They do not send input to a real device.
