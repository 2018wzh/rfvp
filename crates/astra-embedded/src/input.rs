use astra_emu_abi::*;
use rfvp::app::embedded::session::Input;
use winit::{dpi::PhysicalPosition, event::{DeviceId, ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent},
    keyboard::{Key, KeyCode, NamedKey, NativeKeyCode, PhysicalKey}};

fn physical(name: &str) -> PhysicalKey {
    let key = match name {
        "Escape" => KeyCode::Escape, "Enter" => KeyCode::Enter, "NumpadEnter" => KeyCode::NumpadEnter,
        "Space" => KeyCode::Space, "Tab" => KeyCode::Tab,
        "F11" => KeyCode::F11,
        "ArrowLeft" => KeyCode::ArrowLeft, "ArrowRight" => KeyCode::ArrowRight,
        "ArrowUp" => KeyCode::ArrowUp, "ArrowDown" => KeyCode::ArrowDown,
        "Home" => KeyCode::Home, "End" => KeyCode::End, "PageUp" => KeyCode::PageUp, "PageDown" => KeyCode::PageDown,
        _ => return PhysicalKey::Unidentified(NativeKeyCode::Unidentified),
    };
    PhysicalKey::Code(key)
}
fn logical(name: &str) -> Key {
    let named = match name {
        "Escape" => NamedKey::Escape, "Enter" => NamedKey::Enter, "Space" | " " => NamedKey::Space,
        "Shift" => NamedKey::Shift, "Control" => NamedKey::Control, "Alt" => NamedKey::Alt,
        "Tab" => NamedKey::Tab, "ArrowLeft" => NamedKey::ArrowLeft, "ArrowRight" => NamedKey::ArrowRight,
        "ArrowUp" => NamedKey::ArrowUp, "ArrowDown" => NamedKey::ArrowDown,
        "Home" => NamedKey::Home, "End" => NamedKey::End, "PageUp" => NamedKey::PageUp, "PageDown" => NamedKey::PageDown,
        "F1" => NamedKey::F1, "F2" => NamedKey::F2, "F3" => NamedKey::F3, "F4" => NamedKey::F4,
        "F5" => NamedKey::F5, "F6" => NamedKey::F6, "F7" => NamedKey::F7, "F8" => NamedKey::F8,
        "F9" => NamedKey::F9, "F10" => NamedKey::F10, "F11" => NamedKey::F11, "F12" => NamedKey::F12,
        _ => return Key::Character(name.into()),
    };
    Key::Named(named)
}

pub unsafe fn translate(events: &[InputEvent], epoch: u64, newest: u64) -> Result<(Vec<Input>, Option<bool>), Status> {
    let mut native = Vec::with_capacity(events.len());
    let mut suspended = None;
    for event in events {
        if event.pressed > 1 || event.repeat > 1 || !event.x.is_finite() || !event.y.is_finite() {
            return Err(INVALID_ARGUMENT);
        }
        if matches!(event.kind, INPUT_POINTER_MOVE | INPUT_POINTER_BUTTON | INPUT_WHEEL)
            && (event.displayed.session_epoch != epoch || event.displayed.output_epoch != 1
                || event.displayed.generation == 0 || event.displayed.generation > newest) {
            return Err(STALE_CONTEXT);
        }
        // These events carry no OS device ownership; only the native input mapper consumes them.
        let device_id = DeviceId::dummy();
        match event.kind {
            INPUT_KEY => native.push(Input::Key {
                physical: physical(unsafe { event.physical_key.text(128)? }),
                logical: logical(unsafe { event.logical_key.text(128)? }),
                pressed: event.pressed != 0, repeat: event.repeat != 0,
            }),
            INPUT_POINTER_MOVE => native.push(Input::Window(WindowEvent::CursorMoved {
                device_id, position: PhysicalPosition::new(event.x, event.y),
            })),
            INPUT_POINTER_BUTTON => {
                let button = match event.button {
                    BUTTON_PRIMARY => MouseButton::Left,
                    BUTTON_SECONDARY => MouseButton::Right,
                    BUTTON_MIDDLE => MouseButton::Middle,
                    _ => return Err(INVALID_ARGUMENT),
                };
                native.push(Input::Window(WindowEvent::CursorMoved {
                    device_id, position: PhysicalPosition::new(event.x, event.y),
                }));
                native.push(Input::Window(WindowEvent::MouseInput { device_id, button,
                    state: if event.pressed != 0 { ElementState::Pressed } else { ElementState::Released } }));
            }
            INPUT_WHEEL => native.push(Input::Window(WindowEvent::MouseWheel {
                device_id, delta: MouseScrollDelta::LineDelta(event.x as f32, event.y as f32), phase: TouchPhase::Moved,
            })),
            INPUT_WINDOW => {
                if event.window.focused > 1 || event.window.suspended > 1
                    || !event.window.scale_factor.is_finite() || event.window.scale_factor <= 0.0 {
                    return Err(INVALID_ARGUMENT);
                }
                native.push(Input::Window(WindowEvent::Focused(event.window.focused != 0)));
                suspended = Some(event.window.suspended != 0);
                // O/scale factor never mutate the configured R or script-visible L.
            }
            _ => return Err(INVALID_ARGUMENT),
        }
    }
    Ok((native, suspended))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn button(button: u32) -> InputEvent {
        InputEvent {
            kind: INPUT_POINTER_BUTTON, pressed: 1, repeat: 0, button,
            physical_key: Bytes::EMPTY, logical_key: Bytes::EMPTY, x: 150.0, y: 75.0,
            displayed: FrameIdentity { session_epoch: 7, output_epoch: 1, generation: 2 },
            window: WindowState { drawable_width: 1920, drawable_height: 1080, scale_factor: 2.0, focused: 1, suspended: 0 },
        }
    }

    #[test]
    fn frame_pixels_are_not_scaled_again_at_abi_boundary() {
        let (events, _) = unsafe { translate(&[button(BUTTON_SECONDARY)], 7, 3) }.unwrap();
        assert!(matches!(&events[0], Input::Window(WindowEvent::CursorMoved { position, .. })
            if position.x == 150.0 && position.y == 75.0));
        assert!(matches!(&events[1], Input::Window(WindowEvent::MouseInput { button: MouseButton::Right, .. })));
    }

    #[test]
    fn malformed_buttons_and_old_session_inputs_are_rejected() {
        assert!(matches!(unsafe { translate(&[button(0)], 7, 3) }, Err(INVALID_ARGUMENT)));
        assert!(matches!(unsafe { translate(&[button(BUTTON_PRIMARY)], 8, 3) }, Err(STALE_CONTEXT)));
        let mut event = button(BUTTON_PRIMARY);
        event.pressed = 2;
        assert!(matches!(unsafe { translate(&[event], 7, 3) }, Err(INVALID_ARGUMENT)));
    }
}
