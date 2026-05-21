//! Global hotkeys via Carbon's `RegisterEventHotKey`.
//!
//! Why Carbon, in 2026? `RegisterEventHotKey` is the only macOS
//! API that delivers a global hotkey *without* requiring
//! Accessibility / Input Monitoring permission. The alternatives
//! (`CGEventTap`, `NSEvent::addGlobalMonitor`) all need the user
//! to flip a toggle in System Settings, which is hostile for a
//! first-run experience. Carbon hotkeys are quietly still
//! supported on Sequoia and beyond — Apple has never deprecated
//! them, despite the broader Carbon framework being dead.

use std::os::raw::{c_int, c_void};

use crate::{Accelerator, HotkeyId, PlatformError, PlatformEvent, Result};

use super::keymap::accelerator_to_carbon;

#[repr(C)]
#[derive(Clone, Copy)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

const FOUR_CC_VRNR: u32 = u32::from_be_bytes(*b"VRNR");

#[repr(C)]
#[derive(Clone, Copy)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

// kEventClassKeyboard / kEventHotKeyPressed
const K_EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
const K_EVENT_HOT_KEY_PRESSED: u32 = 5;

// EventParamName/Type magic strings.
const K_EVENT_PARAM_DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");
const TYPE_EVENT_HOT_KEY_ID: u32 = u32::from_be_bytes(*b"hkid");

type EventRef = *mut c_void;
type EventHandlerRef = *mut c_void;
type EventTargetRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;

type EventHandlerUPP = unsafe extern "C" fn(
    next_handler: EventHandlerCallRef,
    event: EventRef,
    user_data: *mut c_void,
) -> c_int;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> EventTargetRef;

    fn RegisterEventHotKey(
        in_hot_key_code: u32,
        in_hot_key_modifiers: u32,
        in_hot_key_id: EventHotKeyID,
        in_target: EventTargetRef,
        in_options: u32,
        out_ref: *mut *mut c_void,
    ) -> c_int;

    fn UnregisterEventHotKey(in_hot_key_ref: *mut c_void) -> c_int;

    fn InstallEventHandler(
        in_target: EventTargetRef,
        in_handler: EventHandlerUPP,
        in_num_types: u32,
        in_list: *const EventTypeSpec,
        in_user_data: *mut c_void,
        out_handler: *mut EventHandlerRef,
    ) -> c_int;

    fn GetEventParameter(
        in_event: EventRef,
        in_name: u32,
        in_desired_type: u32,
        out_actual_type: *mut u32,
        in_buffer_size: u32,
        out_actual_size: *mut u32,
        out_data: *mut c_void,
    ) -> c_int;
}

pub(crate) struct HotkeyResources {
    pub carbon_ref: *mut c_void,
}

// Carbon handles are main-thread-only by convention. We never
// touch them off-main.
unsafe impl Send for HotkeyResources {}

pub(crate) fn register(accelerator: Accelerator, label: &str) -> Result<HotkeyId> {
    let _ = label; // unused — Carbon doesn't surface labels.
    let (vkey, carbon_mods) = accelerator_to_carbon(accelerator.modifiers, accelerator.key)
        .ok_or_else(|| {
            PlatformError::Other(anyhow::anyhow!(
                "macOS: cannot map accelerator {:?} to a Carbon vkey/modifier pair",
                accelerator
            ))
        })?;
    let new_id = HotkeyId(super::next_id());

    super::app::run_on_main_sync(move || -> Result<HotkeyId> {
        ensure_handler_installed()?;
        let mut carbon_ref: *mut c_void = std::ptr::null_mut();
        let status = unsafe {
            RegisterEventHotKey(
                vkey,
                carbon_mods,
                EventHotKeyID {
                    signature: FOUR_CC_VRNR,
                    id: new_id.0 as u32,
                },
                GetApplicationEventTarget(),
                0,
                &mut carbon_ref,
            )
        };
        if status != 0 || carbon_ref.is_null() {
            return Err(PlatformError::Other(anyhow::anyhow!(
                "RegisterEventHotKey returned OSStatus {status}"
            )));
        }
        super::with_main_state(|s| {
            s.hotkeys.insert(new_id, HotkeyResources { carbon_ref });
        });
        Ok(new_id)
    })
}

pub(crate) fn unregister(id: HotkeyId) -> Result<()> {
    super::app::run_on_main_sync(move || -> Result<()> {
        let resources = super::with_main_state(|s| s.hotkeys.remove(&id));
        let Some(res) = resources else {
            return Ok(());
        };
        let status = unsafe { UnregisterEventHotKey(res.carbon_ref) };
        if status != 0 {
            return Err(PlatformError::Other(anyhow::anyhow!(
                "UnregisterEventHotKey returned OSStatus {status}"
            )));
        }
        Ok(())
    })
}

fn ensure_handler_installed() -> Result<()> {
    super::with_main_state(|state| {
        if state.carbon_handler_installed {
            return Ok(());
        }
        let spec = EventTypeSpec {
            event_class: K_EVENT_CLASS_KEYBOARD,
            event_kind: K_EVENT_HOT_KEY_PRESSED,
        };
        let mut handler_ref: EventHandlerRef = std::ptr::null_mut();
        let status = unsafe {
            InstallEventHandler(
                GetApplicationEventTarget(),
                hotkey_handler,
                1,
                &spec,
                std::ptr::null_mut(),
                &mut handler_ref,
            )
        };
        if status != 0 {
            return Err(PlatformError::Other(anyhow::anyhow!(
                "InstallEventHandler returned OSStatus {status}"
            )));
        }
        state.carbon_handler_installed = true;
        Ok(())
    })
}

unsafe extern "C" fn hotkey_handler(
    _next: EventHandlerCallRef,
    event: EventRef,
    _user_data: *mut c_void,
) -> c_int {
    let mut hk_id = EventHotKeyID {
        signature: 0,
        id: 0,
    };
    let mut actual_size: u32 = 0;
    let status = unsafe {
        GetEventParameter(
            event,
            K_EVENT_PARAM_DIRECT_OBJECT,
            TYPE_EVENT_HOT_KEY_ID,
            std::ptr::null_mut(),
            std::mem::size_of::<EventHotKeyID>() as u32,
            &mut actual_size,
            (&mut hk_id) as *mut _ as *mut c_void,
        )
    };
    if status != 0 || hk_id.signature != FOUR_CC_VRNR {
        return 0; // noErr — let the event continue.
    }
    if let Some(tx) = super::event_tx() {
        let _ = tx.send(PlatformEvent::HotkeyPressed(HotkeyId(hk_id.id as u64)));
    }
    0
}
