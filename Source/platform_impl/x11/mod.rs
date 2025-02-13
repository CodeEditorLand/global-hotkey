// Copyright 2022-2022 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{collections::BTreeMap, ffi::c_ulong, ptr};

use crossbeam_channel::{Receiver, Sender, unbounded};
use keyboard_types::{Code, Modifiers};
use x11_dl::{
	keysym,
	xlib::{self, _XDisplay, Xlib},
};

use crate::{GlobalHotKeyEvent, hotkey::HotKey};

enum ThreadMessage {
	RegisterHotKey(HotKey, Sender<crate::Result<()>>),
	RegisterHotKeys(Vec<HotKey>, Sender<crate::Result<()>>),
	UnRegisterHotKey(HotKey, Sender<crate::Result<()>>),
	UnRegisterHotKeys(Vec<HotKey>, Sender<crate::Result<()>>),
	DropThread,
}

pub struct GlobalHotKeyManager {
	thread_tx:Sender<ThreadMessage>,
}

impl GlobalHotKeyManager {
	pub fn new() -> crate::Result<Self> {
		let (thread_tx, thread_rx) = unbounded();

		std::thread::spawn(|| events_processor(thread_rx));

		Ok(Self { thread_tx })
	}

	pub fn register(&self, hotkey:HotKey) -> crate::Result<()> {
		let (tx, rx) = crossbeam_channel::bounded(1);

		let _ = self.thread_tx.send(ThreadMessage::RegisterHotKey(hotkey, tx));

		if let Ok(result) = rx.recv() {
			result?;
		}

		Ok(())
	}

	pub fn unregister(&self, hotkey:HotKey) -> crate::Result<()> {
		let (tx, rx) = crossbeam_channel::bounded(1);

		let _ = self.thread_tx.send(ThreadMessage::UnRegisterHotKey(hotkey, tx));

		if let Ok(result) = rx.recv() {
			result?;
		}

		Ok(())
	}

	pub fn register_all(&self, hotkeys:&[HotKey]) -> crate::Result<()> {
		let (tx, rx) = crossbeam_channel::bounded(1);

		let _ = self.thread_tx.send(ThreadMessage::RegisterHotKeys(hotkeys.to_vec(), tx));

		if let Ok(result) = rx.recv() {
			result?;
		}

		Ok(())
	}

	pub fn unregister_all(&self, hotkeys:&[HotKey]) -> crate::Result<()> {
		let (tx, rx) = crossbeam_channel::bounded(1);

		let _ = self.thread_tx.send(ThreadMessage::UnRegisterHotKeys(hotkeys.to_vec(), tx));

		if let Ok(result) = rx.recv() {
			result?;
		}

		Ok(())
	}
}

impl Drop for GlobalHotKeyManager {
	fn drop(&mut self) { let _ = self.thread_tx.send(ThreadMessage::DropThread); }
}

// XGrabKey works only with the exact state (modifiers)
// and since X11 considers NumLock, ScrollLock and CapsLock a modifier when it
// is ON, we also need to register our shortcut combined with these extra
// modifiers as well
const IGNORED_MODS:[u32; 4] = [
	0,              // modifier only
	xlib::Mod2Mask, // NumLock
	xlib::LockMask, // CapsLock
	xlib::Mod2Mask | xlib::LockMask,
];

#[inline]
fn register_hotkey(
	xlib:&Xlib,
	display:*mut _XDisplay,
	root:c_ulong,
	hotkeys:&mut BTreeMap<u32, Vec<(u32, u32, bool)>>,
	hotkey:HotKey,
) -> crate::Result<()> {
	let (modifiers, key) =
		(modifiers_to_x11_mods(hotkey.mods), keycode_to_x11_scancode(hotkey.key));

	if let Some(key) = key {
		let keycode = unsafe { (xlib.XKeysymToKeycode)(display, key as _) };

		for m in IGNORED_MODS {
			let result = unsafe {
				(xlib.XGrabKey)(
					display,
					keycode as _,
					modifiers | m,
					root,
					0,
					xlib::GrabModeAsync,
					xlib::GrabModeAsync,
				)
			};

			if result == xlib::BadAccess as _ {
				for m in IGNORED_MODS {
					unsafe { (xlib.XUngrabKey)(display, keycode as _, modifiers | m, root) };
				}

				return Err(crate::Error::AlreadyRegistered(hotkey));
			}
		}

		let entry = hotkeys.entry(keycode as _).or_default();

		match entry.iter().find(|e| e.1 == modifiers) {
			None => {
				entry.push((hotkey.id(), modifiers, false));

				Ok(())
			},

			Some(_) => Err(crate::Error::AlreadyRegistered(hotkey)),
		}
	} else {
		Err(crate::Error::FailedToRegister(format!(
			"Unable to register accelerator (unknown scancode for this key: {}).",
			hotkey.key
		)))
	}
}

#[inline]
fn unregister_hotkey(
	xlib:&Xlib,
	display:*mut _XDisplay,
	root:c_ulong,
	hotkeys:&mut BTreeMap<u32, Vec<(u32, u32, bool)>>,
	hotkey:HotKey,
) -> crate::Result<()> {
	let (modifiers, key) =
		(modifiers_to_x11_mods(hotkey.mods), keycode_to_x11_scancode(hotkey.key));

	if let Some(key) = key {
		let keycode = unsafe { (xlib.XKeysymToKeycode)(display, key as _) };

		for m in IGNORED_MODS {
			unsafe { (xlib.XUngrabKey)(display, keycode as _, modifiers | m, root) };
		}

		let entry = hotkeys.entry(keycode as _).or_default();

		entry.retain(|k| k.1 != modifiers);

		Ok(())
	} else {
		Err(crate::Error::FailedToUnRegister(hotkey))
	}
}

fn events_processor(thread_rx:Receiver<ThreadMessage>) {
	//                           key    id,  mods, pressed
	let mut hotkeys = BTreeMap::<u32, Vec<(u32, u32, bool)>>::new();

	if let Ok(xlib) = xlib::Xlib::open() {
		unsafe {
			let display = (xlib.XOpenDisplay)(ptr::null());

			let root:c_ulong = (xlib.XDefaultRootWindow)(display);

			// Only trigger key release at end of repeated keys
			let mut supported_rtrn:i32 = 0;
			(xlib.XkbSetDetectableAutoRepeat)(display, 1, &mut supported_rtrn);

			(xlib.XSelectInput)(display, root, xlib::KeyPressMask);

			let mut event:xlib::XEvent = std::mem::zeroed();

			loop {
				// Always service all pending events to avoid a queue of events from building
				// up.
				while (xlib.XPending)(display) > 0 {
					(xlib.XNextEvent)(display, &mut event);

					match event.get_type() {
						e @ xlib::KeyPress | e @ xlib::KeyRelease => {
							let keycode = event.key.keycode;
							// X11 sends masks for Lock keys also and we only care about the 4 below
							let event_mods = event.key.state
								& (xlib::ControlMask
									| xlib::ShiftMask | xlib::Mod4Mask
									| xlib::Mod1Mask);

							if let Some(entry) = hotkeys.get_mut(&keycode) {
								match e {
									xlib::KeyPress => {
										for (id, mods, pressed) in entry {
											if event_mods == *mods && !*pressed {
												GlobalHotKeyEvent::send(GlobalHotKeyEvent {
													id:*id,
													state:crate::HotKeyState::Pressed,
												});
												*pressed = true;
											}
										}
									},

									xlib::KeyRelease => {
										for (id, _, pressed) in entry {
											if *pressed {
												GlobalHotKeyEvent::send(GlobalHotKeyEvent {
													id:*id,
													state:crate::HotKeyState::Released,
												});
												*pressed = false;
											}
										}
									},

									_ => {},
								}
							}
						},

						_ => {},
					}
				}

				if let Ok(msg) = thread_rx.try_recv() {
					match msg {
						ThreadMessage::RegisterHotKey(hotkey, tx) => {
							let _ = tx.send(register_hotkey(
								&xlib,
								display,
								root,
								&mut hotkeys,
								hotkey,
							));
						},

						ThreadMessage::RegisterHotKeys(keys, tx) => {
							for hotkey in keys {
								if let Err(e) =
									register_hotkey(&xlib, display, root, &mut hotkeys, hotkey)
								{
									let _ = tx.send(Err(e));
								}
							}

							let _ = tx.send(Ok(()));
						},

						ThreadMessage::UnRegisterHotKey(hotkey, tx) => {
							let _ = tx.send(unregister_hotkey(
								&xlib,
								display,
								root,
								&mut hotkeys,
								hotkey,
							));
						},

						ThreadMessage::UnRegisterHotKeys(keys, tx) => {
							for hotkey in keys {
								if let Err(e) =
									unregister_hotkey(&xlib, display, root, &mut hotkeys, hotkey)
								{
									let _ = tx.send(Err(e));
								}
							}

							let _ = tx.send(Ok(()));
						},

						ThreadMessage::DropThread => {
							(xlib.XCloseDisplay)(display);

							return;
						},
					}
				}

				std::thread::sleep(std::time::Duration::from_millis(50));
			}
		};
	} else {
		#[cfg(debug_assertions)]
		eprintln!(
			"Failed to open Xlib, maybe you are not running under X11? Other window systems on \
			 Linux are not supported by `global-hotkey` crate."
		);
	}
}

fn keycode_to_x11_scancode(key:Code) -> Option<u32> {
	Some(match key {
		Code::KeyA => keysym::XK_A,
		Code::KeyB => keysym::XK_B,
		Code::KeyC => keysym::XK_C,
		Code::KeyD => keysym::XK_D,
		Code::KeyE => keysym::XK_E,
		Code::KeyF => keysym::XK_F,
		Code::KeyG => keysym::XK_G,
		Code::KeyH => keysym::XK_H,
		Code::KeyI => keysym::XK_I,
		Code::KeyJ => keysym::XK_J,
		Code::KeyK => keysym::XK_K,
		Code::KeyL => keysym::XK_L,
		Code::KeyM => keysym::XK_M,
		Code::KeyN => keysym::XK_N,
		Code::KeyO => keysym::XK_O,
		Code::KeyP => keysym::XK_P,
		Code::KeyQ => keysym::XK_Q,
		Code::KeyR => keysym::XK_R,
		Code::KeyS => keysym::XK_S,
		Code::KeyT => keysym::XK_T,
		Code::KeyU => keysym::XK_U,
		Code::KeyV => keysym::XK_V,
		Code::KeyW => keysym::XK_W,
		Code::KeyX => keysym::XK_X,
		Code::KeyY => keysym::XK_Y,
		Code::KeyZ => keysym::XK_Z,
		Code::Backslash => keysym::XK_backslash,
		Code::BracketLeft => keysym::XK_bracketleft,
		Code::BracketRight => keysym::XK_bracketright,
		Code::Backquote => keysym::XK_quoteleft,
		Code::Comma => keysym::XK_comma,
		Code::Digit0 => keysym::XK_0,
		Code::Digit1 => keysym::XK_1,
		Code::Digit2 => keysym::XK_2,
		Code::Digit3 => keysym::XK_3,
		Code::Digit4 => keysym::XK_4,
		Code::Digit5 => keysym::XK_5,
		Code::Digit6 => keysym::XK_6,
		Code::Digit7 => keysym::XK_7,
		Code::Digit8 => keysym::XK_8,
		Code::Digit9 => keysym::XK_9,
		Code::Equal => keysym::XK_equal,
		Code::Minus => keysym::XK_minus,
		Code::Period => keysym::XK_period,
		Code::Quote => keysym::XK_leftsinglequotemark,
		Code::Semicolon => keysym::XK_semicolon,
		Code::Slash => keysym::XK_slash,
		Code::Backspace => keysym::XK_BackSpace,
		Code::CapsLock => keysym::XK_Caps_Lock,
		Code::Enter => keysym::XK_Return,
		Code::Space => keysym::XK_space,
		Code::Tab => keysym::XK_Tab,
		Code::Delete => keysym::XK_Delete,
		Code::End => keysym::XK_End,
		Code::Home => keysym::XK_Home,
		Code::Insert => keysym::XK_Insert,
		Code::PageDown => keysym::XK_Page_Down,
		Code::PageUp => keysym::XK_Page_Up,
		Code::ArrowDown => keysym::XK_Down,
		Code::ArrowLeft => keysym::XK_Left,
		Code::ArrowRight => keysym::XK_Right,
		Code::ArrowUp => keysym::XK_Up,
		Code::Numpad0 => keysym::XK_KP_0,
		Code::Numpad1 => keysym::XK_KP_1,
		Code::Numpad2 => keysym::XK_KP_2,
		Code::Numpad3 => keysym::XK_KP_3,
		Code::Numpad4 => keysym::XK_KP_4,
		Code::Numpad5 => keysym::XK_KP_5,
		Code::Numpad6 => keysym::XK_KP_6,
		Code::Numpad7 => keysym::XK_KP_7,
		Code::Numpad8 => keysym::XK_KP_8,
		Code::Numpad9 => keysym::XK_KP_9,
		Code::NumpadAdd => keysym::XK_KP_Add,
		Code::NumpadDecimal => keysym::XK_KP_Decimal,
		Code::NumpadDivide => keysym::XK_KP_Divide,
		Code::NumpadMultiply => keysym::XK_KP_Multiply,
		Code::NumpadSubtract => keysym::XK_KP_Subtract,
		Code::Escape => keysym::XK_Escape,
		Code::PrintScreen => keysym::XK_Print,
		Code::ScrollLock => keysym::XK_Scroll_Lock,
		Code::NumLock => keysym::XK_F1,
		Code::F1 => keysym::XK_F1,
		Code::F2 => keysym::XK_F2,
		Code::F3 => keysym::XK_F3,
		Code::F4 => keysym::XK_F4,
		Code::F5 => keysym::XK_F5,
		Code::F6 => keysym::XK_F6,
		Code::F7 => keysym::XK_F7,
		Code::F8 => keysym::XK_F8,
		Code::F9 => keysym::XK_F9,
		Code::F10 => keysym::XK_F10,
		Code::F11 => keysym::XK_F11,
		Code::F12 => keysym::XK_F12,
		Code::AudioVolumeDown => keysym::XF86XK_AudioLowerVolume,
		Code::AudioVolumeMute => keysym::XF86XK_AudioMute,
		Code::AudioVolumeUp => keysym::XF86XK_AudioRaiseVolume,
		Code::MediaPlay => keysym::XF86XK_AudioPlay,
		Code::MediaPause => keysym::XF86XK_AudioPause,
		Code::MediaStop => keysym::XF86XK_AudioStop,
		Code::MediaTrackNext => keysym::XF86XK_AudioNext,
		Code::MediaTrackPrevious => keysym::XF86XK_AudioPrev,
		Code::Pause => keysym::XK_Pause,
		_ => return None,
	})
}

fn modifiers_to_x11_mods(modifiers:Modifiers) -> u32 {
	let mut x11mods = 0;

	if modifiers.contains(Modifiers::SHIFT) {
		x11mods |= xlib::ShiftMask;
	}

	if modifiers.intersects(Modifiers::SUPER | Modifiers::META) {
		x11mods |= xlib::Mod4Mask;
	}

	if modifiers.contains(Modifiers::ALT) {
		x11mods |= xlib::Mod1Mask;
	}

	if modifiers.contains(Modifiers::CONTROL) {
		x11mods |= xlib::ControlMask;
	}

	x11mods
}
