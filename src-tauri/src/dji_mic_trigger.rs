//! DJI wireless-mic button → Handy transcription (macOS).
//!
//! Handy already starts and stops recording from keyboard shortcuts, CLI flags
//! (`--toggle-transcription`), and Unix signals. It has no generic HID or media-
//! key binding. The DJI receiver, when plugged into a Mac over USB, is an audio
//! device plus a **consumer-control** HID interface: the physical button macOS
//! can see is reported as volume increment/decrement (usage page 0x0C, usages
//! 0xE9 / 0xEA), not as a dedicated keyboard key.
//!
//! This module watches that scoped HID stream (DJI vendor `0x2CA3`) and feeds
//! the existing [`TranscriptionCoordinator`]. A CGEvent tap swallows the
//! matching system media-key only when a DJI HID event was just seen, so the
//! laptop's own volume keys keep working.
//!
//! ## What this can and cannot observe
//!
//! - **Works:** the USB receiver's consumer volume event. Third-party tools
//!   have seen this from product IDs `0x4008` and `0x4011` ("Wireless Mic Rx").
//!   On some DJI models the receiver's own linking/connecting button is that
//!   event. On Mic Mini 2, DJI's camera-shutter feature also forwards the
//!   *transmitter* linking button over the wireless link as the same RX USB
//!   volume HID. This crate implements that HID path; if a Mic 2 TX linking
//!   press is forwarded the same way, it will fire automatically.
//! - **Not claimed:** a Mic 2 transmitter linking button as its own Mac HID
//!   device. When the TX is Bluetooth-linked to a phone the linking button can
//!   act as a shutter; that path is not what a USB-connected Mac sees.
//! - **Does not work:** Bluetooth-only / no USB receiver. macOS then sees an
//!   audio device without button HID.
//!
//! Keyboard volume keys are never remapped. No leftover `hidutil` mapping is
//! applied, so quitting Handy restores normal DJI volume-button behavior.

use log::{debug, info, warn};
use serde::Serialize;
use specta::Type;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager};

use crate::settings::{get_settings, write_settings};

/// USB vendor ID for DJI Technology Co., Ltd.
pub const DJI_VENDOR_ID: u32 = 0x2CA3;

/// Product IDs reported by existing DJI wireless-mic receivers on macOS.
/// `0x4008` is used by handy-dji-mic-trigger; `0x4011` by dji-mic-command /
/// dji-mic-wispr-flow ("Wireless Mic Rx"). Unknown Mic 2 IDs still match on
/// vendor + consumer volume (see [`is_dji_consumer_volume_event`]).
pub const KNOWN_RECEIVER_PRODUCT_IDS: &[u32] = &[0x4008, 0x4011];

/// HID Consumer Control usage page.
pub const HID_CONSUMER_PAGE: u32 = 0x0C;
/// Volume increment (consumer usage).
pub const HID_VOLUME_INCREMENT: u32 = 0xE9;
/// Volume decrement (consumer usage).
pub const HID_VOLUME_DECREMENT: u32 = 0xEA;

/// How long after a DJI HID event a system media-key is treated as that button.
pub const DJI_MEDIA_WINDOW_MS: u64 = 350;

/// NSEvent subtype for aux/consumer control buttons (volume, play, etc.).
const AUX_CONTROL_SUBTYPE: i16 = 8;
const NX_KEYTYPE_SOUND_UP: i64 = 0;
const NX_KEYTYPE_SOUND_DOWN: i64 = 1;
const NX_KEY_STATE_DOWN: i64 = 0x0A;
const NX_KEY_STATE_UP: i64 = 0x0B;

/// Process-wide listener flag so the settings command can report status
/// without holding the macOS run-loop thread.
static LISTENER_RUNNING: AtomicBool = AtomicBool::new(false);
static VOLUME_SWALLOW_ACTIVE: AtomicBool = AtomicBool::new(false);
static RECEIVER_SEEN: AtomicBool = AtomicBool::new(false);
static LAST_DEVICE_NAME: Mutex<Option<String>> = Mutex::new(None);

/// Frontend / command snapshot of the DJI trigger.
#[derive(Debug, Clone, Serialize, Type)]
pub struct DjiMicTriggerStatus {
    /// Always `true` on macOS; `false` elsewhere (UI should hide the toggle).
    pub supported: bool,
    pub enabled: bool,
    pub listener_running: bool,
    /// `true` when the CGEvent tap is installed and can suppress DJI volume.
    pub volume_swallow_active: bool,
    pub receiver_seen: bool,
    pub last_device_name: Option<String>,
}

/// Owned handle so the listener can be started and stopped with the setting.
#[derive(Default)]
pub struct DjiMicTriggerState {
    inner: Mutex<Option<ListenerGuard>>,
}

/// RAII stop token. Dropping it asks the macOS thread to exit.
#[allow(dead_code)]
struct ListenerGuard {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ListenerGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        #[cfg(target_os = "macos")]
        macos::stop_runloop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        LISTENER_RUNNING.store(false, Ordering::SeqCst);
        VOLUME_SWALLOW_ACTIVE.store(false, Ordering::SeqCst);
    }
}

/// Start or stop the listener so it matches the persisted setting.
pub fn sync_enabled(app: &AppHandle) {
    let enabled = get_settings(app).dji_mic_trigger_enabled;
    apply_enabled(app, enabled);
}

fn apply_enabled(app: &AppHandle, enabled: bool) {
    let Some(state) = app.try_state::<DjiMicTriggerState>() else {
        warn!("DjiMicTriggerState is not managed");
        return;
    };
    let mut guard = match state.inner.lock() {
        Ok(g) => g,
        Err(e) => {
            warn!("DJI mic trigger state lock poisoned: {e}");
            return;
        }
    };

    if !enabled {
        *guard = None;
        return;
    }

    if guard.is_some() {
        return;
    }

    maybe_select_dji_microphone(app);

    #[cfg(target_os = "macos")]
    {
        *guard = Some(macos::start_listener(app.clone()));
    }
    #[cfg(not(target_os = "macos"))]
    {
        warn!("DJI mic trigger is only implemented on macOS");
    }
}

#[tauri::command]
#[specta::specta]
pub fn change_dji_mic_trigger_enabled_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = get_settings(&app);
    settings.dji_mic_trigger_enabled = enabled;
    write_settings(&app, settings);

    apply_enabled(&app, enabled);

    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "dji_mic_trigger_enabled",
            "value": enabled
        }),
    );

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn get_dji_mic_trigger_status(app: AppHandle) -> DjiMicTriggerStatus {
    let enabled = get_settings(&app).dji_mic_trigger_enabled;
    let last_device_name = LAST_DEVICE_NAME.lock().ok().and_then(|g| g.clone());
    DjiMicTriggerStatus {
        supported: cfg!(target_os = "macos"),
        enabled,
        listener_running: LISTENER_RUNNING.load(Ordering::SeqCst),
        volume_swallow_active: VOLUME_SWALLOW_ACTIVE.load(Ordering::SeqCst),
        receiver_seen: RECEIVER_SEEN.load(Ordering::SeqCst),
        last_device_name,
    }
}

/// When the user turns the trigger on and has not already picked a microphone,
/// prefer a connected DJI wireless receiver input.
fn maybe_select_dji_microphone(app: &AppHandle) {
    let Some(name) = find_dji_input_device_name() else {
        debug!("DJI mic trigger: no matching audio input to select");
        return;
    };

    let mut settings = get_settings(app);
    if let Some(ref current) = settings.selected_microphone {
        if looks_like_dji_mic_name(current) {
            return;
        }
        debug!(
            "DJI mic trigger: leaving user-selected microphone '{}'",
            current
        );
        return;
    }

    info!("DJI mic trigger: selecting audio input '{}'", name);
    settings.selected_microphone = Some(name.clone());
    write_settings(app, settings);

    if let Some(rm) =
        app.try_state::<std::sync::Arc<crate::managers::audio::AudioRecordingManager>>()
    {
        if let Err(e) = rm.update_selected_device() {
            warn!("DJI mic trigger: failed to switch audio input: {e}");
        }
    }

    let _ = app.emit(
        "settings-changed",
        serde_json::json!({
            "setting": "selected_microphone",
            "value": name
        }),
    );
}

fn find_dji_input_device_name() -> Option<String> {
    crate::audio_toolkit::audio::list_input_devices()
        .ok()?
        .into_iter()
        .map(|d| d.name)
        .find(|name| looks_like_dji_mic_name(name))
}

/// Audio device names used by DJI wireless receivers on macOS.
pub fn looks_like_dji_mic_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("wireless mic") || n.contains("wireless microphone") || n.contains("dji mic")
}

/// True when this HID value should be treated as the DJI receiver button.
pub fn is_dji_consumer_volume_event(
    vendor_id: u32,
    product_id: u32,
    product_name: &str,
    usage_page: u32,
    usage: u32,
) -> bool {
    if !is_dji_vendor(vendor_id) {
        return false;
    }
    if !is_consumer_volume_usage(usage_page, usage) {
        return false;
    }
    if let Some(required) = env_product_filter() {
        return product_id == required;
    }
    if KNOWN_RECEIVER_PRODUCT_IDS.contains(&product_id) {
        return true;
    }
    if looks_like_dji_mic_name(product_name) {
        return true;
    }
    // Unknown DJI product that still speaks consumer volume — accept it so a
    // Mic 2 receiver with an unpublished PID is not silently ignored. Other
    // DJI gadgets almost never expose this HID usage.
    product_name.trim().is_empty() || !looks_like_unrelated_dji_product(product_name)
}

pub fn is_dji_vendor(vendor_id: u32) -> bool {
    vendor_id == env_vendor_id()
}

pub fn is_consumer_volume_usage(usage_page: u32, usage: u32) -> bool {
    usage_page == HID_CONSUMER_PAGE
        && (usage == HID_VOLUME_INCREMENT || usage == HID_VOLUME_DECREMENT)
}

fn looks_like_unrelated_dji_product(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("mavic")
        || n.contains("goggles")
        || n.contains("osmo")
        || n.contains("ronin")
        || n.contains("controller")
        || n.contains("drone")
}

fn env_vendor_id() -> u32 {
    parse_hex_env("DJI_VENDOR_ID").unwrap_or(DJI_VENDOR_ID)
}

fn env_product_filter() -> Option<u32> {
    parse_hex_env("DJI_PRODUCT_ID")
}

fn parse_hex_env(name: &str) -> Option<u32> {
    let raw = std::env::var(name).ok()?;
    let trimmed = raw.trim();
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16).ok()
    } else {
        trimmed
            .parse()
            .ok()
            .or_else(|| u32::from_str_radix(trimmed, 16).ok())
    }
}

/// Decode an NSSystemDefined aux-control `data1` payload.
///
/// `2560` (`0x00000A00`) is Sound Up key-down; `2816` (`0x00000B00`) is key-up.
/// Volume Down uses key code 1 in the high word.
pub fn decode_aux_control(subtype: i16, data1: i64) -> Option<(AuxMediaKey, bool)> {
    if subtype != AUX_CONTROL_SUBTYPE {
        return None;
    }
    let key_code = (data1 >> 16) & 0xFFFF;
    let state = (data1 >> 8) & 0xFF;
    let pressed = match state {
        NX_KEY_STATE_DOWN => true,
        NX_KEY_STATE_UP => false,
        _ => return None,
    };
    let key = match key_code {
        NX_KEYTYPE_SOUND_UP => AuxMediaKey::SoundUp,
        NX_KEYTYPE_SOUND_DOWN => AuxMediaKey::SoundDown,
        _ => return None,
    };
    Some((key, pressed))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuxMediaKey {
    SoundUp,
    SoundDown,
}

fn remember_device_name(name: &str) {
    RECEIVER_SEEN.store(true, Ordering::SeqCst);
    if let Ok(mut slot) = LAST_DEVICE_NAME.lock() {
        if slot.as_deref() != Some(name) {
            info!("DJI mic trigger: receiver HID '{}'", name);
            *slot = Some(name.to_string());
        }
    }
}

fn fire_media_edge(app: &AppHandle, is_pressed: bool) {
    let settings = get_settings(app);
    if let Some(coordinator) = app.try_state::<crate::TranscriptionCoordinator>() {
        coordinator.send_input(
            "transcribe",
            "dji-mic",
            is_pressed,
            settings.shortcut_activation,
            std::time::Duration::from_millis(settings.hold_threshold_ms),
        );
    } else {
        warn!("DJI mic trigger: TranscriptionCoordinator is not initialized");
    }
}

fn fire_hid_only_toggle(app: &AppHandle) {
    crate::signal_handle::send_transcription_input(app, "transcribe", "dji-mic");
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::runloop::{
        kCFRunLoopCommonModes, kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopSource,
    };
    use core_foundation::string::CFString;
    use std::ffi::c_void;
    use std::sync::atomic::AtomicPtr;
    use std::time::{SystemTime, UNIX_EPOCH};

    const K_IO_HID_OPTIONS_NONE: u32 = 0;
    const K_CG_HID_EVENT_TAP: u32 = 0;
    const K_CG_HEAD_INSERT_EVENT_TAP: u32 = 0;
    const K_CG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;
    const K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
    const K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;
    const NS_EVENT_TYPE_SYSTEM_DEFINED: u32 = 14;

    static RUNLOOP: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

    struct Shared {
        app: AppHandle,
        last_dji_hid_ms: std::sync::atomic::AtomicU64,
        tap_active: AtomicBool,
        tap_port: std::sync::atomic::AtomicPtr<c_void>,
    }

    pub fn stop_runloop() {
        let ptr = RUNLOOP.load(Ordering::SeqCst);
        if !ptr.is_null() {
            let rl = unsafe { CFRunLoop::wrap_under_get_rule(ptr as _) };
            rl.stop();
        }
    }

    pub fn start_listener(app: AppHandle) -> ListenerGuard {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let join = std::thread::Builder::new()
            .name("dji-mic-trigger".into())
            .spawn(move || run_loop(app, stop_for_thread))
            .expect("failed to spawn DJI mic trigger thread");
        ListenerGuard {
            stop,
            join: Some(join),
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn run_loop(app: AppHandle, stop: Arc<AtomicBool>) {
        let shared_ptr = Box::into_raw(Box::new(Shared {
            app,
            last_dji_hid_ms: std::sync::atomic::AtomicU64::new(0),
            tap_active: AtomicBool::new(false),
            tap_port: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
        }));
        // SAFETY: this thread owns `shared_ptr` until CFRunLoopRun returns.
        let shared = unsafe { &*shared_ptr };

        let hid = match open_hid_manager(shared) {
            Some(manager) => manager,
            None => {
                warn!("DJI mic trigger: IOHIDManager failed to open");
                return;
            }
        };

        let tap = install_event_tap(shared);
        if let Some(ref installed) = tap {
            shared.tap_port.store(installed.port, Ordering::SeqCst);
            shared.tap_active.store(true, Ordering::SeqCst);
            VOLUME_SWALLOW_ACTIVE.store(true, Ordering::SeqCst);
            info!(
                "DJI mic trigger: listening for vendor 0x{:04x} consumer volume (event tap on)",
                env_vendor_id()
            );
        } else {
            warn!(
                "DJI mic trigger: CGEvent tap unavailable — grant Input Monitoring \
                 to Handy if you want the DJI button not to change system volume. \
                 HID-only toggle is still active."
            );
        }

        LISTENER_RUNNING.store(true, Ordering::SeqCst);

        let current = CFRunLoop::get_current();
        RUNLOOP.store(
            current.as_concrete_TypeRef() as *mut c_void,
            Ordering::SeqCst,
        );

        // CFRunLoopRun until stop_runloop(). The stop flag is a backup; the
        // Drop impl always calls CFRunLoopStop.
        if !stop.load(Ordering::SeqCst) {
            unsafe {
                core_foundation::runloop::CFRunLoopRun();
            }
        }

        drop(hid);
        drop(tap);
        // SAFETY: no more HID/tap callbacks; this thread created the box.
        let _ = unsafe { Box::from_raw(shared_ptr) };
        LISTENER_RUNNING.store(false, Ordering::SeqCst);
        VOLUME_SWALLOW_ACTIVE.store(false, Ordering::SeqCst);
        RUNLOOP.store(std::ptr::null_mut(), Ordering::SeqCst);
        info!("DJI mic trigger: listener stopped");
    }

    fn open_hid_manager(shared: &Shared) -> Option<HidManager> {
        unsafe {
            let manager = IOHIDManagerCreate(std::ptr::null(), K_IO_HID_OPTIONS_NONE);
            if manager.is_null() {
                return None;
            }

            let vendor = CFNumber::from(env_vendor_id() as i64);
            let vendor_key = CFString::from_static_string("VendorID");
            let matching =
                CFDictionary::from_CFType_pairs(&[(vendor_key.as_CFType(), vendor.as_CFType())]);
            IOHIDManagerSetDeviceMatching(manager, matching.as_concrete_TypeRef() as *const c_void);

            IOHIDManagerRegisterInputValueCallback(
                manager,
                hid_value_callback,
                shared as *const Shared as *mut c_void,
            );

            let rl = CFRunLoop::get_current();
            IOHIDManagerScheduleWithRunLoop(
                manager,
                rl.as_concrete_TypeRef(),
                kCFRunLoopDefaultMode,
            );

            let result = IOHIDManagerOpen(manager, K_IO_HID_OPTIONS_NONE);
            if result != 0 {
                warn!("DJI mic trigger: IOHIDManagerOpen returned {result}");
                IOHIDManagerClose(manager, K_IO_HID_OPTIONS_NONE);
                CFRelease(manager as *const c_void);
                return None;
            }

            Some(HidManager { manager })
        }
    }

    fn install_event_tap(shared: &Shared) -> Option<EventTap> {
        unsafe {
            let mask: u64 = 1 << NS_EVENT_TYPE_SYSTEM_DEFINED;
            let port = CGEventTapCreate(
                K_CG_HID_EVENT_TAP,
                K_CG_HEAD_INSERT_EVENT_TAP,
                K_CG_EVENT_TAP_OPTION_DEFAULT,
                mask,
                event_tap_callback,
                shared as *const Shared as *mut c_void,
            );
            if port.is_null() {
                return None;
            }

            let source = CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0);
            if source.is_null() {
                CFRelease(port as *const c_void);
                return None;
            }

            let rl = CFRunLoop::get_current();
            let source_ref = CFRunLoopSource::wrap_under_create_rule(
                source as core_foundation::runloop::CFRunLoopSourceRef,
            );
            rl.add_source(&source_ref, kCFRunLoopCommonModes);
            CGEventTapEnable(port, true);

            Some(EventTap {
                port,
                _source: source_ref,
            })
        }
    }

    extern "C" fn hid_value_callback(
        _context: *mut c_void,
        _result: i32,
        _sender: *mut c_void,
        value: *mut c_void,
    ) {
        let context = unsafe { &*(_context as *const Shared) };
        let Some(event) = read_hid_event(value) else {
            return;
        };
        if !is_dji_consumer_volume_event(
            event.vendor_id,
            event.product_id,
            &event.product_name,
            event.usage_page,
            event.usage,
        ) {
            return;
        }

        remember_device_name(&event.product_name);
        context.last_dji_hid_ms.store(now_ms(), Ordering::SeqCst);
        debug!(
            "DJI mic trigger: HID vendor=0x{:04x} product=0x{:04x} \
             usage=0x{:02x} value={} name='{}'",
            event.vendor_id, event.product_id, event.usage, event.int_value, event.product_name
        );

        // Event tap owns press/release when it is running so we can match
        // Handy’s shortcut activation mode. Without a tap, each non-zero HID
        // pulse is a toggle — DJI buttons are click-style and often omit a
        // clean usage=0 release.
        if !context.tap_active.load(Ordering::SeqCst) && event.int_value != 0 {
            fire_hid_only_toggle(&context.app);
        }
    }

    extern "C" fn event_tap_callback(
        _proxy: *mut c_void,
        event_type: u32,
        event: *mut c_void,
        user_info: *mut c_void,
    ) -> *mut c_void {
        let context = unsafe { &*(user_info as *const Shared) };

        if event_type == K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT
            || event_type == K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT
        {
            let port = context.tap_port.load(Ordering::SeqCst);
            if !port.is_null() {
                unsafe { CGEventTapEnable(port, true) };
            }
            return event;
        }

        if event_type != NS_EVENT_TYPE_SYSTEM_DEFINED {
            return event;
        }

        let Some((subtype, data1)) = ns_event_aux_fields(event) else {
            return event;
        };
        let Some((_key, pressed)) = decode_aux_control(subtype, data1) else {
            return event;
        };

        let age = now_ms().saturating_sub(context.last_dji_hid_ms.load(Ordering::SeqCst));
        if age > DJI_MEDIA_WINDOW_MS {
            return event;
        }

        fire_media_edge(&context.app, pressed);
        debug!("DJI mic trigger: swallowed media key pressed={pressed} age_ms={age}");
        // Swallow so the DJI button does not change system volume.
        std::ptr::null_mut()
    }

    struct HidEvent {
        vendor_id: u32,
        product_id: u32,
        product_name: String,
        usage_page: u32,
        usage: u32,
        int_value: i64,
    }

    fn read_hid_event(value: *mut c_void) -> Option<HidEvent> {
        unsafe {
            if value.is_null() {
                return None;
            }
            let element = IOHIDValueGetElement(value);
            if element.is_null() {
                return None;
            }
            let usage_page = IOHIDElementGetUsagePage(element);
            let usage = IOHIDElementGetUsage(element);
            let int_value = IOHIDValueGetIntegerValue(value);
            let device = IOHIDElementGetDevice(element);
            if device.is_null() {
                return None;
            }

            let vendor_id = hid_number_prop(device, "VendorID").unwrap_or(0);
            let product_id = hid_number_prop(device, "ProductID").unwrap_or(0);
            let product_name = hid_string_prop(device, "Product").unwrap_or_default();

            Some(HidEvent {
                vendor_id,
                product_id,
                product_name,
                usage_page,
                usage,
                int_value,
            })
        }
    }

    fn hid_number_prop(device: *mut c_void, key: &'static str) -> Option<u32> {
        unsafe {
            let cf_key = CFString::from_static_string(key);
            let raw = IOHIDDeviceGetProperty(device, cf_key.as_concrete_TypeRef() as *const c_void);
            if raw.is_null() {
                return None;
            }
            let cf = CFType::wrap_under_get_rule(raw as _);
            let number = cf.downcast::<CFNumber>()?;
            number.to_i64().map(|n| n as u32)
        }
    }

    fn hid_string_prop(device: *mut c_void, key: &'static str) -> Option<String> {
        unsafe {
            let cf_key = CFString::from_static_string(key);
            let raw = IOHIDDeviceGetProperty(device, cf_key.as_concrete_TypeRef() as *const c_void);
            if raw.is_null() {
                return None;
            }
            let cf = CFType::wrap_under_get_rule(raw as _);
            let string = cf.downcast::<CFString>()?;
            Some(string.to_string())
        }
    }

    fn ns_event_aux_fields(event: *mut c_void) -> Option<(i16, i64)> {
        use objc2::runtime::AnyObject;
        use objc2::{msg_send, ClassType};

        objc2::rc::autoreleasepool(|_| unsafe {
            let ns_event: *mut AnyObject =
                msg_send![objc2_app_kit::NSEvent::class(), eventWithCGEvent: event];
            if ns_event.is_null() {
                return None;
            }
            let subtype: isize = msg_send![ns_event, subtype];
            let data1: isize = msg_send![ns_event, data1];
            Some((subtype as i16, data1 as i64))
        })
    }

    struct HidManager {
        manager: *mut c_void,
    }

    unsafe impl Send for HidManager {}

    impl Drop for HidManager {
        fn drop(&mut self) {
            unsafe {
                if !self.manager.is_null() {
                    IOHIDManagerUnscheduleFromRunLoop(
                        self.manager,
                        CFRunLoop::get_current().as_concrete_TypeRef(),
                        kCFRunLoopDefaultMode,
                    );
                    IOHIDManagerClose(self.manager, K_IO_HID_OPTIONS_NONE);
                    CFRelease(self.manager as *const c_void);
                }
            }
        }
    }

    struct EventTap {
        port: *mut c_void,
        _source: CFRunLoopSource,
    }

    unsafe impl Send for EventTap {}

    impl Drop for EventTap {
        fn drop(&mut self) {
            unsafe {
                if !self.port.is_null() {
                    CGEventTapEnable(self.port, false);
                    CFRelease(self.port as *const c_void);
                }
            }
        }
    }

    type IOHIDValueCallback = extern "C" fn(*mut c_void, i32, *mut c_void, *mut c_void);
    type CGEventTapCallBack =
        extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> *mut c_void;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOHIDManagerCreate(allocator: *const c_void, options: u32) -> *mut c_void;
        fn IOHIDManagerSetDeviceMatching(manager: *mut c_void, matching: *const c_void);
        fn IOHIDManagerRegisterInputValueCallback(
            manager: *mut c_void,
            callback: IOHIDValueCallback,
            context: *mut c_void,
        );
        fn IOHIDManagerScheduleWithRunLoop(
            manager: *mut c_void,
            run_loop: core_foundation::runloop::CFRunLoopRef,
            mode: core_foundation::runloop::CFRunLoopMode,
        );
        fn IOHIDManagerUnscheduleFromRunLoop(
            manager: *mut c_void,
            run_loop: core_foundation::runloop::CFRunLoopRef,
            mode: core_foundation::runloop::CFRunLoopMode,
        );
        fn IOHIDManagerOpen(manager: *mut c_void, options: u32) -> i32;
        fn IOHIDManagerClose(manager: *mut c_void, options: u32) -> i32;
        fn IOHIDValueGetElement(value: *mut c_void) -> *mut c_void;
        fn IOHIDValueGetIntegerValue(value: *mut c_void) -> i64;
        fn IOHIDElementGetUsagePage(element: *mut c_void) -> u32;
        fn IOHIDElementGetUsage(element: *mut c_void) -> u32;
        fn IOHIDElementGetDevice(element: *mut c_void) -> *mut c_void;
        fn IOHIDDeviceGetProperty(device: *mut c_void, key: *const c_void) -> *const c_void;
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventTapCreate(
            tap: u32,
            place: u32,
            options: u32,
            events_of_interest: u64,
            callback: CGEventTapCallBack,
            user_info: *mut c_void,
        ) -> *mut c_void;
        fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: *const c_void);
        fn CFMachPortCreateRunLoopSource(
            allocator: *const c_void,
            port: *mut c_void,
            order: isize,
        ) -> *mut c_void;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consumer_volume_usages_are_recognized() {
        assert!(is_consumer_volume_usage(
            HID_CONSUMER_PAGE,
            HID_VOLUME_INCREMENT
        ));
        assert!(is_consumer_volume_usage(
            HID_CONSUMER_PAGE,
            HID_VOLUME_DECREMENT
        ));
        assert!(!is_consumer_volume_usage(HID_CONSUMER_PAGE, 0xCD));
        assert!(!is_consumer_volume_usage(0x01, HID_VOLUME_INCREMENT));
    }

    #[test]
    fn keyboard_volume_is_not_dji() {
        assert!(!is_dji_consumer_volume_event(
            0x05ac,
            0x0267,
            "Apple Internal Keyboard",
            HID_CONSUMER_PAGE,
            HID_VOLUME_INCREMENT,
        ));
    }

    #[test]
    fn known_receiver_pids_match() {
        for pid in KNOWN_RECEIVER_PRODUCT_IDS {
            assert!(is_dji_consumer_volume_event(
                DJI_VENDOR_ID,
                *pid,
                "",
                HID_CONSUMER_PAGE,
                HID_VOLUME_INCREMENT,
            ));
        }
    }

    #[test]
    fn wireless_mic_name_matches_unknown_pid() {
        assert!(is_dji_consumer_volume_event(
            DJI_VENDOR_ID,
            0x4fff,
            "Wireless Microphone RX",
            HID_CONSUMER_PAGE,
            HID_VOLUME_DECREMENT,
        ));
    }

    #[test]
    fn unrelated_dji_gadget_is_ignored() {
        assert!(!is_dji_consumer_volume_event(
            DJI_VENDOR_ID,
            0x0008,
            "Mavic Mini Remote",
            HID_CONSUMER_PAGE,
            HID_VOLUME_INCREMENT,
        ));
    }

    #[test]
    fn audio_device_names() {
        assert!(looks_like_dji_mic_name("Wireless Microphone RX"));
        assert!(looks_like_dji_mic_name("Wireless Mic Rx"));
        assert!(!looks_like_dji_mic_name("MacBook Pro Microphone"));
    }

    #[test]
    fn aux_media_payloads_from_third_party_helper() {
        assert_eq!(
            decode_aux_control(8, 2560),
            Some((AuxMediaKey::SoundUp, true))
        );
        assert_eq!(
            decode_aux_control(8, 2816),
            Some((AuxMediaKey::SoundUp, false))
        );
        assert_eq!(
            decode_aux_control(8, 0x00010A00),
            Some((AuxMediaKey::SoundDown, true))
        );
        assert_eq!(decode_aux_control(0, 2560), None);
        assert_eq!(decode_aux_control(8, 0), None);
    }
}
