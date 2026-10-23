# ATK Lua API v1

`ghub-lua` provides a Lua 5.4 runtime with the selected G HUB-compatible calls and an ATK-specific namespace, `atk`. The namespace is installed in every script automatically; no callback registration is required to access it. Hardware operations are requests to the host `InputBackend`. A function being present does not imply hardware support: inspect `atk.capabilities` and handle backend errors.

```lua
atk.log.write("ATK Lua API %d", atk.api_version)
atk.log.write("keyboard=%s", tostring(atk.capabilities.keyboard))
```

## Capability flags

`atk.capabilities` is a table captured from the configured backend when the Lua state starts. `api_version` is `1`. The boolean flags are `keyboard`, `unicode_text`, `absolute_mouse`, `mouse_position`, `scroll`, and `keyboard_events`. They describe real backend support. Unsupported operations return a Lua error; they are not silently ignored. Keyboard event capture is opt-in even when `keyboard_events` is true.

## Events and shutdown

Register one event callback with `atk.on_event(function(event) ... end)`. The table contains `name`, optional `arg`, optional `family`, and monotonic `time_us` elapsed since the script worker started. Keyboard events also contain `key = { scan_code = ..., extended = ... }`. The host supplies event names and families. The app currently maps keyboard transitions to `KEY_PRESSED` / `KEY_RELEASED` with family `keyboard`, mouse buttons to the legacy mouse event names and family `mouse`, and vertical/horizontal wheel input to wheel events with family `wheel`; profile events are supplied by the app. G HUB's `OnEvent(event, arg, family)` remains available. When `atk.on_event` is registered, it takes priority and `OnEvent` is not invoked for that same event.

`atk.on_stop(function() ... end)` registers one no-argument callback. During orderly shutdown, the runtime delivers `PROFILE_DEACTIVATED` through the selected event handler, then invokes `atk.on_stop`, then releases any synthetic inputs still owned by the script. This also runs best-effort after a top-level or event callback runtime error while preserving that original script failure. Each shutdown path is guarded so the callbacks are attempted at most once.

Lua callbacks are interrupted cooperatively at the configured shutdown deadline. The standalone runtime cannot interrupt a non-yieldable C call or a blocked backend call. Its `stop()` can return `ShutdownTimeout`; cleanup occurs only if that worker later returns. In the process runtime, the child gets the smaller of `RuntimeConfig.shutdown_grace` and the parent's `stop_grace` minus a 200 ms broker-cleanup margin (defaults: 700 ms and 1 s). The parent force-terminates a child that has not exited by `stop_grace`, then releases inputs still recorded by its broker. A force-kill can interrupt a non-yieldable C call, so it cannot guarantee that either shutdown callback completed. `stop_timeout` bounds how long the public stop call waits for broker cleanup; it is separate from the child callback grace.

The legacy `EnablePrimaryMouseButtonEvents` gate remains available to G HUB `OnEvent` scripts. ATK event callbacks receive primary mouse-button events without requiring that legacy opt-in. Registering both event handlers does not dispatch one host event twice.

## Mouse

Mouse buttons use the G HUB range: `1` left, `2` middle, `3` right, `4` X1, `5` X2.

| Call | Behavior |
| --- | --- |
| `atk.mouse.press(button)` | Press a button and track script ownership for cleanup. |
| `atk.mouse.release(button)` | Release a button. Releases remain allowed during shutdown. |
| `atk.mouse.click(button, duration_ms?)` | Press, wait, then release. The default wait is 10 ms; the maximum is 60,000 ms. It errors if that button is already held by this script. |
| `atk.mouse.is_pressed(button)` | Query button state from the backend. The Windows backend reads global asynchronous state, which can include synthetic input. |
| `atk.mouse.move(x, y)` | Relative movement in pixels; each coordinate must be between -1,000,000 and 1,000,000. |
| `atk.mouse.move_to(x, y)` | Move to an absolute coordinate in the virtual desktop. The left/top edges are included; right/bottom edges are excluded. |
| `atk.mouse.position()` | Return `{ x = ..., y = ... }` from the backend. |
| `atk.mouse.scroll(vertical, horizontal?)` | Scroll in wheel units of 120 per notch. Horizontal defaults to zero; each value is limited to ±1,200,000. |

Absolute coordinates can be negative when a monitor is placed left of or above the primary display. `move_to` validates against the backend's virtual-desktop rectangle before requesting movement.

## Keyboard

Key arguments may be a supported name, a numeric Set-1 scan code, or a table `{ scan_code = n, extended = true_or_false }`. Numeric values are scan codes, not Windows virtual-key codes; scan codes must be in the range 1–255. E0 keys use `extended = true`; E1 sequences such as Pause/Break are not supported or converted into ordinary keys. The supported name subset is:

- Letters `a`–`z`, digits `0`–`9`, and `f1`–`f12`.
- `esc` / `escape`, `backspace`, `tab`, `enter` / `return`, `space`.
- `capslock`, `numlock`, `scrolllock` and underscore variants.
- `minus`, `equal` / `equals`, `left_bracket`, `right_bracket`, `semicolon`, `apostrophe`, `grave` / `backtick`, `backslash`, `comma`, `period`, `slash`.
- Modifiers: `ctrl` / `control`, `shift`, `alt`, `win` / `meta`, plus left/right names such as `leftctrl`, `rightctrl`, `lshift`, and `ralt`.
- Navigation: `insert`, `home`, `pageup` / `pgup`, `delete` / `del`, `end`, `pagedown` / `pgdn`, `up`, `down`, `left`, `right` (and the corresponding `*arrow` names).
- Numpad: `numpad0`–`numpad9`, `numpad_enter`, `numpad_slash`, `numpad_multiply`, `numpad_minus`, `numpad_plus`, `numpad_decimal`.

| Call | Behavior |
| --- | --- |
| `atk.keyboard.press(key)` / `release(key)` | Request a key transition. Successfully pressed keys are tracked and released on cleanup. |
| `atk.keyboard.tap(key, duration_ms?)` | Press, wait, then release. The default is 10 ms and the maximum is 60,000 ms. It errors if that key is already held by this script. |
| `atk.keyboard.is_pressed(key)` | Query key state. The Windows backend reads global asynchronous state, which can include synthetic input. |
| `atk.keyboard.is_modifier_pressed(name)` | Query modifier state. Generic `ctrl`, `shift`, `alt`, `win`, and `meta` names check both left and right keys; a left/right name checks only that key. |
| `atk.keyboard.is_lock_on(name)` | Query `capslock`, `numlock`, or `scrolllock` state. Names are case-insensitive and may use underscores. |
| `atk.keyboard.type_text(text)` | Type Unicode text through the backend; the limit is 16,384 UTF-8 bytes. |
| `atk.keyboard.capture_events(enabled)` | Enable or disable physical keyboard event capture. Capture starts disabled. Enabling it requires the `keyboard_events` capability. |
| `atk.keyboard.chord(keys, callback)` | Press a dense list of up to 16 distinct keys, call the function, then release only keys newly pressed by the chord, in reverse order. Keys already held by this script stay held. |

```lua
atk.keyboard.chord({"ctrl", "a"}, function()
    atk.log.write("Ctrl+A requested")
end)

atk.keyboard.capture_events(true) -- only if atk.capabilities.keyboard_events is true
```

## Time and logging

| Call | Behavior |
| --- | --- |
| `atk.time.sleep(milliseconds)` | Cancellable sleep, maximum 3,600,000 ms. |
| `atk.time.sleep_us(microseconds)` | Cancellable sleep, maximum 3,600,000,000 µs. |
| `atk.time.now_us()` | Elapsed monotonic microseconds since this worker started. |
| `atk.log.write(message_or_format, ...args)` | With one argument, send that UTF-8 message unchanged. With additional arguments, format using Lua `string.format`. The final message is limited to 8,192 bytes. No newline is appended automatically. |
| `atk.log.clear()` | Clear the host script log. |

Logs and releases remain available to shutdown callbacks until the runtime's deadline. New input presses and movement are rejected after shutdown begins. Microsecond arguments and timestamps do not guarantee microsecond scheduling accuracy; Windows scheduling and the active backend determine the actual delay.

## Finite sequences

`atk.sequence(actions)` accepts a dense Lua array with 1–256 action tables. It validates the full array, every action and every bound before performing the first side effect. It stops on the first backend error or cancellation. Supported entries:

```lua
atk.sequence({
    { kind = "key", action = "press", key = "ctrl" },
    { kind = "key", action = "tap", key = "a", duration_ms = 10 },
    { kind = "mouse", action = "click", button = 1, duration_ms = 15 },
    { kind = "move", x = 8, y = -2 },
    { kind = "scroll", vertical = 120, horizontal = 0 },
    { kind = "key", action = "release", key = "ctrl" },
    { kind = "sleep", ms = 20 },
})
```

`mouse` actions support `press`, `release`, and `click`; `key` actions support `press`, `release`, and `tap`. `duration_ms` is optional for click/tap and defaults to 10. `move` uses the relative-movement bound, `scroll` uses wheel units, and `sleep` uses the millisecond sleep bound. Absolute movement, text entry, callbacks, and nested sequences are not sequence actions.

## Compatibility boundary

The G HUB global API remains a selected compatibility subset. This ATK namespace is a separate versioned API and is not a claim of complete Logitech G HUB parity. Profiles, device selection, firmware commands and physical event sources are controlled by the application and backend. The API can report or request only capabilities implemented by the active backend; mock-backend tests do not establish hardware behavior for every mouse or firmware.
