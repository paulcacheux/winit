use cocoa::{self, appkit, foundation};
use cocoa::appkit::{NSApplication, NSEvent, NSView, NSWindow};
use events::{self, ElementState, Event, MouseButton, TouchPhase, WindowEvent};
use super::window::Window;
use std;


pub struct EventsLoop {
    pub windows: std::sync::Mutex<Vec<std::sync::Arc<Window>>>,
    pub pending_events: std::sync::Mutex<std::collections::VecDeque<Event>>,
    modifiers: std::sync::Mutex<Modifiers>,
    interrupted: std::sync::atomic::AtomicBool,

    // The user event callback given via either of the `poll_events` or `run_forever` methods.
    //
    // We store the user's callback here so that it may be accessed by each of the window delegate
    // callbacks (e.g. resize, close, etc) for the duration of a call to either of the
    // `poll_events` or `run_forever` methods.
    //
    // This is *only* `Some` for the duration of a call to either of these methods and will be
    // `None` otherwise.
    pub user_callback: UserCallback,
}

struct Modifiers {
    shift_pressed: bool,
    ctrl_pressed: bool,
    win_pressed: bool,
    alt_pressed: bool,
}

// Wrapping the user callback in a type allows us to:
//
// - ensure the callback pointer is never accidentally cloned
// - ensure that only the `EventsLoop` can `store` and `drop` the callback pointer
// - `unsafe impl Send` and `Sync` so that `Send` and `Sync` can be implemented for `EventsLoop`.
pub struct UserCallback {
    mutex: std::sync::Mutex<Option<*mut FnMut(Event)>>,
}


unsafe impl Send for UserCallback {}
unsafe impl Sync for UserCallback {}

impl UserCallback {

    // Here we store user's `callback` behind the mutex so that they may be safely shared between
    // each of the window delegates.
    //
    // In order to make sure that the pointer is always valid, we must manually guarantee that it
    // is dropped before the callback itself is dropped. Thus, this should *only* be called at the
    // beginning of a call to `poll_events` and `run_forever`, both of which *must* drop the
    // callback at the end of their scope using `drop_callback`.
    fn store<F>(&self, callback: &mut F)
        where F: FnMut(Event)
    {
        let trait_object = callback as &mut FnMut(Event);
        let trait_object_ptr = trait_object as *const FnMut(Event) as *mut FnMut(Event);
        *self.mutex.lock().unwrap() = Some(trait_object_ptr);
    }

    // Emits the given event via the user-given callback.
    //
    // This is *only* called within the `poll_events` and `run_forever` methods so we know that it
    // is safe to `unwrap` the last callback without causing a panic as there must be at least one
    // callback stored.
    //
    // This is unsafe as it requires dereferencing the pointer to the user-given callback. We
    // guarantee this is safe by ensuring the `UserCallback` never lives longer than the user-given
    // callback.
    pub unsafe fn call_with_event(&self, event: Event) {
        let callback: *mut FnMut(Event) = self.mutex.lock().unwrap().take().unwrap();
        (*callback)(event);
        *self.mutex.lock().unwrap() = Some(callback);
    }

    // Used to drop the user callback pointer at the end of the `poll_events` and `run_forever`
    // methods. This is done to enforce our guarantee that the top callback will never live longer
    // than the call to either `poll_events` or `run_forever` to which it was given.
    fn drop(&self) {
        self.mutex.lock().unwrap().take();
    }

}


impl EventsLoop {

    pub fn new() -> Self {
        let modifiers = Modifiers {
            shift_pressed: false,
            ctrl_pressed: false,
            win_pressed: false,
            alt_pressed: false,
        };
        EventsLoop {
            windows: std::sync::Mutex::new(Vec::new()),
            pending_events: std::sync::Mutex::new(std::collections::VecDeque::new()),
            modifiers: std::sync::Mutex::new(modifiers),
            interrupted: std::sync::atomic::AtomicBool::new(false),
            user_callback: UserCallback { mutex: std::sync::Mutex::new(None) },
        }
    }

    pub fn poll_events<F>(&self, mut callback: F)
        where F: FnMut(Event),
    {
        unsafe {
            if !msg_send![cocoa::base::class("NSThread"), isMainThread] {
                panic!("Events can only be polled from the main thread on macOS");
            }
        }

        self.user_callback.store(&mut callback);

        // Loop as long as we have pending events to return.
        loop {
            unsafe {
                // First, yield all pending events.
                while let Some(event) = self.pending_events.lock().unwrap().pop_front() {
                    self.user_callback.call_with_event(event);
                }

                let pool = foundation::NSAutoreleasePool::new(cocoa::base::nil);

                // Poll for the next event, returning `nil` if there are none.
                let ns_event = appkit::NSApp().nextEventMatchingMask_untilDate_inMode_dequeue_(
                    appkit::NSAnyEventMask.bits() | appkit::NSEventMaskPressure.bits(),
                    foundation::NSDate::distantPast(cocoa::base::nil),
                    foundation::NSDefaultRunLoopMode,
                    cocoa::base::YES);

                let event = self.ns_event_to_event(ns_event);

                let _: () = msg_send![pool, release];

                match event {
                    // Call the user's callback.
                    Some(event) => self.user_callback.call_with_event(event),
                    None => break,
                }
            }
        }

        self.user_callback.drop();
    }

    pub fn run_forever<F>(&self, mut callback: F)
        where F: FnMut(Event)
    {
        self.interrupted.store(false, std::sync::atomic::Ordering::Relaxed);

        unsafe {
            if !msg_send![cocoa::base::class("NSThread"), isMainThread] {
                panic!("Events can only be polled from the main thread on macOS");
            }
        }

        self.user_callback.store(&mut callback);

        loop {
            unsafe {
                // First, yield all pending events.
                while let Some(event) = self.pending_events.lock().unwrap().pop_front() {
                    self.user_callback.call_with_event(event);
                }

                let pool = foundation::NSAutoreleasePool::new(cocoa::base::nil);

                // Wait for the next event. Note that this function blocks during resize.
                let ns_event = appkit::NSApp().nextEventMatchingMask_untilDate_inMode_dequeue_(
                    appkit::NSAnyEventMask.bits() | appkit::NSEventMaskPressure.bits(),
                    foundation::NSDate::distantFuture(cocoa::base::nil),
                    foundation::NSDefaultRunLoopMode,
                    cocoa::base::YES);

                let maybe_event = self.ns_event_to_event(ns_event);

                // Release the pool before calling the top callback in case the user calls either
                // `run_forever` or `poll_events` within the callback.
                let _: () = msg_send![pool, release];

                if let Some(event) = maybe_event {
                    self.user_callback.call_with_event(event);
                }
            }

            if self.interrupted.load(std::sync::atomic::Ordering::Relaxed) {
                self.interrupted.store(false, std::sync::atomic::Ordering::Relaxed);
                break;
            }
        }

        self.user_callback.drop();
    }

    pub fn interrupt(&self) {
        self.interrupted.store(true, std::sync::atomic::Ordering::Relaxed);

        // Awaken the event loop by triggering `NSApplicationActivatedEventType`.
        unsafe {
            let pool = foundation::NSAutoreleasePool::new(cocoa::base::nil);
            let event =
                NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2_(
                    cocoa::base::nil,
                    appkit::NSApplicationDefined,
                    foundation::NSPoint::new(0.0, 0.0),
                    appkit::NSEventModifierFlags::empty(),
                    0.0,
                    0,
                    cocoa::base::nil,
                    appkit::NSEventSubtype::NSApplicationActivatedEventType,
                    0,
                    0);
            appkit::NSApp().postEvent_atStart_(event, cocoa::base::NO);
            foundation::NSAutoreleasePool::drain(pool);
        }
    }

    // Convert some given `NSEvent` into a winit `Event`.
    unsafe fn ns_event_to_event(&self, ns_event: cocoa::base::id) -> Option<Event> {
        if ns_event == cocoa::base::nil {
            return None;
        }

        // FIXME: Despite not being documented anywhere, an `NSEvent` is produced when a user opens
        // Spotlight while the NSApplication is in focus. This `NSEvent` produces a `NSEventType`
        // with value `21`. This causes a SEGFAULT as soon as we try to match on the `NSEventType`
        // enum as there is no variant associated with the value. Thus, we return early if this
        // sneaky event occurs. If someone does find some documentation on this, please fix this by
        // adding an appropriate variant to the `NSEventType` enum in the cocoa-rs crate.
        if ns_event.eventType() as u64 == 21 {
            return None;
        }

        let event_type = ns_event.eventType();
        let ns_window = ns_event.window();
        let window_id = super::window::get_window_id(ns_window);
        let windows = self.windows.lock().unwrap();
        let maybe_window = windows.iter().find(|window| window_id == window.id());

        // FIXME: Document this. Why do we do this? Seems like it passes on events to window/app.
        // If we don't do this, window does not become main for some reason.
        match event_type {
            appkit::NSKeyDown => (),
            _ => appkit::NSApp().sendEvent_(ns_event),
        }

        let into_event = |window_event| Event::WindowEvent {
            window_id: ::WindowId(window_id),
            event: window_event,
        };

        // Returns `Some` window if one of our windows is the key window.
        let maybe_key_window = || windows.iter().find(|window| {
            let is_key_window: cocoa::base::BOOL = msg_send![*window.window, isKeyWindow];
            is_key_window == cocoa::base::YES
        });

        match event_type {

            appkit::NSKeyDown => {
                let mut events = std::collections::VecDeque::new();
                let received_c_str = foundation::NSString::UTF8String(ns_event.characters());
                let received_str = std::ffi::CStr::from_ptr(received_c_str);
                for received_char in std::str::from_utf8(received_str.to_bytes()).unwrap().chars() {
                    let window_event = WindowEvent::ReceivedCharacter(received_char);
                    events.push_back(into_event(window_event));
                }

                let vkey =  to_virtual_key_code(NSEvent::keyCode(ns_event));
                let state = ElementState::Pressed;
                let code = NSEvent::keyCode(ns_event) as u8;
                let window_event = WindowEvent::KeyboardInput(state, code, vkey);
                events.push_back(into_event(window_event));
                let event = events.pop_front();
                self.pending_events.lock().unwrap().extend(events.into_iter());
                event
            },

            appkit::NSKeyUp => {
                let vkey =  to_virtual_key_code(NSEvent::keyCode(ns_event));

                let state = ElementState::Released;
                let code = NSEvent::keyCode(ns_event) as u8;
                let window_event = WindowEvent::KeyboardInput(state, code, vkey);
                Some(into_event(window_event))
            },

            appkit::NSFlagsChanged => {
                let mut modifiers = self.modifiers.lock().unwrap();

                unsafe fn modifier_event(event: cocoa::base::id,
                                         keymask: appkit::NSEventModifierFlags,
                                         key: events::VirtualKeyCode,
                                         key_pressed: bool) -> Option<WindowEvent>
                {
                    if !key_pressed && NSEvent::modifierFlags(event).contains(keymask) {
                        let state = ElementState::Pressed;
                        let code = NSEvent::keyCode(event) as u8;
                        let window_event = WindowEvent::KeyboardInput(state, code, Some(key));
                        Some(window_event)

                    } else if key_pressed && !NSEvent::modifierFlags(event).contains(keymask) {
                        let state = ElementState::Released;
                        let code = NSEvent::keyCode(event) as u8;
                        let window_event = WindowEvent::KeyboardInput(state, code, Some(key));
                        Some(window_event)

                    } else {
                        None
                    }
                }

                let mut events = std::collections::VecDeque::new();
                if let Some(window_event) = modifier_event(ns_event,
                                                           appkit::NSShiftKeyMask,
                                                           events::VirtualKeyCode::LShift,
                                                           modifiers.shift_pressed)
                {
                    modifiers.shift_pressed = !modifiers.shift_pressed;
                    events.push_back(into_event(window_event));
                }

                if let Some(window_event) = modifier_event(ns_event,
                                                           appkit::NSControlKeyMask,
                                                           events::VirtualKeyCode::LControl,
                                                           modifiers.ctrl_pressed)
                {
                    modifiers.ctrl_pressed = !modifiers.ctrl_pressed;
                    events.push_back(into_event(window_event));
                }

                if let Some(window_event) = modifier_event(ns_event,
                                                           appkit::NSCommandKeyMask,
                                                           events::VirtualKeyCode::LWin,
                                                           modifiers.win_pressed)
                {
                    modifiers.win_pressed = !modifiers.win_pressed;
                    events.push_back(into_event(window_event));
                }

                if let Some(window_event) = modifier_event(ns_event,
                                                           appkit::NSAlternateKeyMask,
                                                           events::VirtualKeyCode::LAlt,
                                                           modifiers.alt_pressed)
                {
                    modifiers.alt_pressed = !modifiers.alt_pressed;
                    events.push_back(into_event(window_event));
                }

                let event = events.pop_front();
                self.pending_events.lock().unwrap().extend(events.into_iter());
                event
            },

            appkit::NSLeftMouseDown => { Some(into_event(WindowEvent::MouseInput(ElementState::Pressed, MouseButton::Left))) },
            appkit::NSLeftMouseUp => { Some(into_event(WindowEvent::MouseInput(ElementState::Released, MouseButton::Left))) },
            appkit::NSRightMouseDown => { Some(into_event(WindowEvent::MouseInput(ElementState::Pressed, MouseButton::Right))) },
            appkit::NSRightMouseUp => { Some(into_event(WindowEvent::MouseInput(ElementState::Released, MouseButton::Right))) },
            appkit::NSOtherMouseDown => { Some(into_event(WindowEvent::MouseInput(ElementState::Pressed, MouseButton::Middle))) },
            appkit::NSOtherMouseUp => { Some(into_event(WindowEvent::MouseInput(ElementState::Released, MouseButton::Middle))) },

            appkit::NSMouseEntered => { Some(into_event(WindowEvent::MouseEntered)) },
            appkit::NSMouseExited => { Some(into_event(WindowEvent::MouseLeft)) },

            appkit::NSMouseMoved |
            appkit::NSLeftMouseDragged |
            appkit::NSOtherMouseDragged |
            appkit::NSRightMouseDragged => {
                // If the mouse movement was on one of our windows, use it.
                // Otherwise, if one of our windows is the key window (receiving input), use it.
                // Otherwise, return `None`.
                let window = match maybe_window.or_else(maybe_key_window) {
                    Some(window) => window,
                    None => return None,
                };

                let window_point = ns_event.locationInWindow();
                let view_point = if ns_window == cocoa::base::nil {
                    let ns_size = foundation::NSSize::new(0.0, 0.0);
                    let ns_rect = foundation::NSRect::new(window_point, ns_size);
                    let window_rect = window.window.convertRectFromScreen_(ns_rect);
                    window.view.convertPoint_fromView_(window_rect.origin, cocoa::base::nil)
                } else {
                    window.view.convertPoint_fromView_(window_point, cocoa::base::nil)
                };
                let view_rect = NSView::frame(*window.view);
                let scale_factor = window.hidpi_factor();

                let x = (scale_factor * view_point.x as f32) as i32;
                let y = (scale_factor * (view_rect.size.height - view_point.y) as f32) as i32;
                let window_event = WindowEvent::MouseMoved(x, y);
                let event = Event::WindowEvent { window_id: ::WindowId(window.id()), event: window_event };
                Some(event)
            },

            appkit::NSScrollWheel => {
                // If none of the windows received the scroll, return `None`.
                let window = match maybe_window {
                    Some(window) => window,
                    None => return None,
                };

                use events::MouseScrollDelta::{LineDelta, PixelDelta};
                let scale_factor = window.hidpi_factor();
                let delta = if ns_event.hasPreciseScrollingDeltas() == cocoa::base::YES {
                    PixelDelta(scale_factor * ns_event.scrollingDeltaX() as f32,
                               scale_factor * ns_event.scrollingDeltaY() as f32)
                } else {
                    LineDelta(scale_factor * ns_event.scrollingDeltaX() as f32,
                              scale_factor * ns_event.scrollingDeltaY() as f32)
                };
                let phase = match ns_event.phase() {
                    appkit::NSEventPhaseMayBegin | appkit::NSEventPhaseBegan => TouchPhase::Started,
                    appkit::NSEventPhaseEnded => TouchPhase::Ended,
                    _ => TouchPhase::Moved,
                };
                let window_event = WindowEvent::MouseWheel(delta, phase);
                Some(into_event(window_event))
            },

            appkit::NSEventTypePressure => {
                let pressure = ns_event.pressure();
                let stage = ns_event.stage();
                let window_event = WindowEvent::TouchpadPressure(pressure, stage);
                Some(into_event(window_event))
            },

            appkit::NSApplicationDefined => match ns_event.subtype() {
                appkit::NSEventSubtype::NSApplicationActivatedEventType => {
                    Some(into_event(WindowEvent::Awakened))
                },
                _ => None,
            },

            _  => None,
        }
    }

}


fn to_virtual_key_code(code: u16) -> Option<events::VirtualKeyCode> {
    Some(match code {
        0x00 => events::VirtualKeyCode::A,
        0x01 => events::VirtualKeyCode::S,
        0x02 => events::VirtualKeyCode::D,
        0x03 => events::VirtualKeyCode::F,
        0x04 => events::VirtualKeyCode::H,
        0x05 => events::VirtualKeyCode::G,
        0x06 => events::VirtualKeyCode::Z,
        0x07 => events::VirtualKeyCode::X,
        0x08 => events::VirtualKeyCode::C,
        0x09 => events::VirtualKeyCode::V,
        //0x0a => World 1,
        0x0b => events::VirtualKeyCode::B,
        0x0c => events::VirtualKeyCode::Q,
        0x0d => events::VirtualKeyCode::W,
        0x0e => events::VirtualKeyCode::E,
        0x0f => events::VirtualKeyCode::R,
        0x10 => events::VirtualKeyCode::Y,
        0x11 => events::VirtualKeyCode::T,
        0x12 => events::VirtualKeyCode::Key1,
        0x13 => events::VirtualKeyCode::Key2,
        0x14 => events::VirtualKeyCode::Key3,
        0x15 => events::VirtualKeyCode::Key4,
        0x16 => events::VirtualKeyCode::Key6,
        0x17 => events::VirtualKeyCode::Key5,
        0x18 => events::VirtualKeyCode::Equals,
        0x19 => events::VirtualKeyCode::Key9,
        0x1a => events::VirtualKeyCode::Key7,
        0x1b => events::VirtualKeyCode::Minus,
        0x1c => events::VirtualKeyCode::Key8,
        0x1d => events::VirtualKeyCode::Key0,
        0x1e => events::VirtualKeyCode::RBracket,
        0x1f => events::VirtualKeyCode::O,
        0x20 => events::VirtualKeyCode::U,
        0x21 => events::VirtualKeyCode::LBracket,
        0x22 => events::VirtualKeyCode::I,
        0x23 => events::VirtualKeyCode::P,
        0x24 => events::VirtualKeyCode::Return,
        0x25 => events::VirtualKeyCode::L,
        0x26 => events::VirtualKeyCode::J,
        0x27 => events::VirtualKeyCode::Apostrophe,
        0x28 => events::VirtualKeyCode::K,
        0x29 => events::VirtualKeyCode::Semicolon,
        0x2a => events::VirtualKeyCode::Backslash,
        0x2b => events::VirtualKeyCode::Comma,
        0x2c => events::VirtualKeyCode::Slash,
        0x2d => events::VirtualKeyCode::N,
        0x2e => events::VirtualKeyCode::M,
        0x2f => events::VirtualKeyCode::Period,
        0x30 => events::VirtualKeyCode::Tab,
        0x31 => events::VirtualKeyCode::Space,
        0x32 => events::VirtualKeyCode::Grave,
        0x33 => events::VirtualKeyCode::Back,
        //0x34 => unkown,
        0x35 => events::VirtualKeyCode::Escape,
        0x36 => events::VirtualKeyCode::RWin,
        0x37 => events::VirtualKeyCode::LWin,
        0x38 => events::VirtualKeyCode::LShift,
        //0x39 => Caps lock,
        //0x3a => Left alt,
        0x3b => events::VirtualKeyCode::LControl,
        0x3c => events::VirtualKeyCode::RShift,
        //0x3d => Right alt,
        0x3e => events::VirtualKeyCode::RControl,
        //0x3f => Fn key,
        //0x40 => F17 Key,
        0x41 => events::VirtualKeyCode::Decimal,
        //0x42 -> unkown,
        0x43 => events::VirtualKeyCode::Multiply,
        //0x44 => unkown,
        0x45 => events::VirtualKeyCode::Add,
        //0x46 => unkown,
        0x47 => events::VirtualKeyCode::Numlock,
        //0x48 => KeypadClear,
        0x49 => events::VirtualKeyCode::VolumeUp,
        0x4a => events::VirtualKeyCode::VolumeDown,
        0x4b => events::VirtualKeyCode::Divide,
        0x4c => events::VirtualKeyCode::NumpadEnter,
        //0x4d => unkown,
        0x4e => events::VirtualKeyCode::Subtract,
        //0x4f => F18 key,
        //0x50 => F19 Key,
        0x51 => events::VirtualKeyCode::NumpadEquals,
        0x52 => events::VirtualKeyCode::Numpad0,
        0x53 => events::VirtualKeyCode::Numpad1,
        0x54 => events::VirtualKeyCode::Numpad2,
        0x55 => events::VirtualKeyCode::Numpad3,
        0x56 => events::VirtualKeyCode::Numpad4,
        0x57 => events::VirtualKeyCode::Numpad5,
        0x58 => events::VirtualKeyCode::Numpad6,
        0x59 => events::VirtualKeyCode::Numpad7,
        //0x5a => F20 Key,
        0x5b => events::VirtualKeyCode::Numpad8,
        0x5c => events::VirtualKeyCode::Numpad9,
        //0x5d => unkown,
        //0x5e => unkown,
        //0x5f => unkown,
        0x60 => events::VirtualKeyCode::F5,
        0x61 => events::VirtualKeyCode::F6,
        0x62 => events::VirtualKeyCode::F7,
        0x63 => events::VirtualKeyCode::F3,
        0x64 => events::VirtualKeyCode::F8,
        0x65 => events::VirtualKeyCode::F9,
        //0x66 => unkown,
        0x67 => events::VirtualKeyCode::F11,
        //0x68 => unkown,
        0x69 => events::VirtualKeyCode::F13,
        //0x6a => F16 Key,
        0x6b => events::VirtualKeyCode::F14,
        //0x6c => unkown,
        0x6d => events::VirtualKeyCode::F10,
        //0x6e => unkown,
        0x6f => events::VirtualKeyCode::F12,
        //0x70 => unkown,
        0x71 => events::VirtualKeyCode::F15,
        0x72 => events::VirtualKeyCode::Insert,
        0x73 => events::VirtualKeyCode::Home,
        0x74 => events::VirtualKeyCode::PageUp,
        0x75 => events::VirtualKeyCode::Delete,
        0x76 => events::VirtualKeyCode::F4,
        0x77 => events::VirtualKeyCode::End,
        0x78 => events::VirtualKeyCode::F2,
        0x79 => events::VirtualKeyCode::PageDown,
        0x7a => events::VirtualKeyCode::F1,
        0x7b => events::VirtualKeyCode::Left,
        0x7c => events::VirtualKeyCode::Right,
        0x7d => events::VirtualKeyCode::Down,
        0x7e => events::VirtualKeyCode::Up,
        //0x7f =>  unkown,

        _ => return None,
    })
}
