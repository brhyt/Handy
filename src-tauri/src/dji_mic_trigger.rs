//! DJI wireless-mic button → Handy transcription (macOS).
//!
//! Handy already starts and stops recording from keyboard shortcuts, CLI flags
//! (`--toggle-transcription`), and Unix signals. It has no generic HID or media-
//! key binding. The DJI receiver, when plugged into a Mac over USB, is an audio
//! device plus a **consumer-control** HID interface: the physical button macOS
//! can see is reported as volume increment/decrement (usage page 0x0C, usages
//! 0xE9 / 0xEA), not as a dedicated keyboard key.
//!
//! On **DJI Mic 2**, a short press of the transmitter **Link** button is
//! forwarded by the USB receiver (`0x2CA3` / `0x4008`, "Wireless Microphone RX")
//! as that same consumer-volume HID. macOS turns it into an `NSSystemDefined`
//! Sound Up/Down event (this is why Link changes system volume when Handy is
//! off). The receiver is often an `AppleUserHIDEventService`, so `IOHIDManager`
//! input-value callbacks may never fire even though the device is listed by
//! `hidutil` — the CGEvent tap is then the reliable edge.
//!
//! Hold Link = pairing. TX Power = noise reduction. TX Rec hold = Bluetooth
//! mode. Those are on-device and are not this trigger.
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
/// `0x4008` is Mic 2 / handy-dji-mic-trigger; `0x4011` is dji-mic-command
/// ("Wireless Mic Rx").
pub const KNOWN_RECEIVER_PRODUCT_IDS: &[u32] = &[0x4008, 0x4011];

/// HID Consumer Control usage page.
pub const HID_CONSUMER_PAGE: u32 = 0x0C;
/// Device-level PrimaryUsage advertised by Mic 2 RX (`hidutil`).
pub const HID_CONSUMER_CONTROL_COLLECTION: u32 = 0x01;
/// Volume increment (consumer usage) — Mic 2 TX Link short-press.
pub const HID_VOLUME_INCREMENT: u32 = 0xE9;
/// Volume decrement (consumer usage).
pub const HID_VOLUME_DECREMENT: u32 = 0xEA;
pub const HID_MUTE: u32 = 0xE2;
pub const HID_PLAY_PAUSE: u32 = 0xCD;
pub const HID_SCAN_NEXT: u32 = 0xB5;
pub const HID_SCAN_PREV: u32 = 0xB6;
pub const HID_STOP: u32 = 0xB7;
pub const HID_EJECT: u32 = 0xB8;

/// How long after a DJI HID event a system media-key is treated as that button.
pub const DJI_MEDIA_WINDOW_MS: u64 = 350;

/// Delay before re-opening Wireless Microphone RX after a Link edge, so a USB
/// composite reset from HID handling can finish before capture starts.
pub const DJI_STREAM_RECOVER_MS: u64 = 280;

/// NSEvent subtype for aux/consumer control buttons (volume, play, etc.).
const AUX_CONTROL_SUBTYPE: i16 = 8;
const NX_KEYTYPE_SOUND_UP: i64 = 0;
const NX_KEYTYPE_SOUND_DOWN: i64 = 1;
const NX_KEYTYPE_MUTE: i64 = 7;
const NX_KEY_STATE_DOWN: i64 = 0x0A;
const NX_KEY_STATE_UP: i64 = 0x0B;
const CG_KEYBOARD_EVENT_KEYBOARD_TYPE: u32 = 10;

/// Process-wide listener flag so the settings command can report status
/// without holding the macOS run-loop thread.
static LISTENER_RUNNING: AtomicBool = AtomicBool::new(false);
static VOLUME_SWALLOW_ACTIVE: AtomicBool = AtomicBool::new(false);
static RECEIVER_PRESENT: AtomicBool = AtomicBool::new(false);
static BUTTON_SEEN: AtomicBool = AtomicBool::new(false);
static HID_VALUES_SEEN: AtomicBool = AtomicBool::new(false);
static LAST_DEVICE_NAME: Mutex<Option<String>> = Mutex::new(None);
static LAST_HID_USAGE: Mutex<Option<String>> = Mutex::new(None);

/// Frontend / command snapshot of the DJI trigger.
#[derive(Debug, Clone, Serialize, Type)]
pub struct DjiMicTriggerStatus {
    /// Always `true` on macOS; `false` elsewhere (UI should hide the toggle).
    pub supported: bool,
    pub enabled: bool,
    pub listener_running: bool,
    /// `true` when the CGEvent tap is installed and can suppress DJI volume.
    pub volume_swallow_active: bool,
    /// USB receiver is enumerated (HID and/or CoreAudio), even if no button fired.
    pub receiver_present: bool,
    /// A HID value, input report, or attributed Link/volume media-key was seen.
    pub button_seen: bool,
    pub last_device_name: Option<String>,
    /// Last observed usage, e.g. `0x000c/0x00e9` or `media:sound-up`.
    pub last_hid_usage: Option<String>,
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
        RECEIVER_PRESENT.store(false, Ordering::SeqCst);
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
        stop_warm_stream_if_on_demand(app);
        return;
    }

    if guard.is_some() {
        return;
    }

    maybe_select_dji_microphone(app);
    if let Some(name) = find_dji_input_device_name() {
        remember_device_present(&name);
    }

    // Open Wireless Microphone RX *before* HID/tap so a later Link press does
    // not coincide with the first CoreAudio open (that pair can drop TX audio
    // on the USB composite device).
    warm_dji_receiver_stream(app);

    #[cfg(target_os = "macos")]
    {
        *guard = Some(macos::start_listener(app.clone()));
    }
    #[cfg(not(target_os = "macos"))]
    {
        warn!("DJI mic trigger is only implemented on macOS");
    }

    recover_dji_receiver_stream(app.clone());
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
    if enabled {
        if let Some(name) = find_dji_input_device_name() {
            remember_device_present(&name);
        }
    }
    let last_device_name = LAST_DEVICE_NAME.lock().ok().and_then(|g| g.clone());
    let last_hid_usage = LAST_HID_USAGE.lock().ok().and_then(|g| g.clone());
    DjiMicTriggerStatus {
        supported: cfg!(target_os = "macos"),
        enabled,
        listener_running: LISTENER_RUNNING.load(Ordering::SeqCst),
        volume_swallow_active: VOLUME_SWALLOW_ACTIVE.load(Ordering::SeqCst),
        receiver_present: RECEIVER_PRESENT.load(Ordering::SeqCst)
            || last_device_name
                .as_deref()
                .is_some_and(looks_like_dji_mic_name),
        button_seen: BUTTON_SEEN.load(Ordering::SeqCst),
        last_device_name,
        last_hid_usage,
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
    is_dji_receiver_hid(vendor_id, product_id, product_name)
        && is_consumer_volume_usage(usage_page, usage)
}

pub fn is_dji_receiver_hid(vendor_id: u32, product_id: u32, product_name: &str) -> bool {
    if !is_dji_vendor(vendor_id) {
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
    // Unknown DJI product that still speaks consumer control — accept it so a
    // Mic 2 receiver with an unpublished PID is not silently ignored.
    product_name.trim().is_empty() || !looks_like_unrelated_dji_product(product_name)
}

pub fn is_dji_vendor(vendor_id: u32) -> bool {
    vendor_id == env_vendor_id()
}

pub fn is_consumer_volume_usage(usage_page: u32, usage: u32) -> bool {
    usage_page == HID_CONSUMER_PAGE
        && (usage == HID_VOLUME_INCREMENT || usage == HID_VOLUME_DECREMENT)
}

/// Usages that should start/stop Handy. Volume increment (Link) is preferred.
pub fn is_dji_trigger_usage(usage_page: u32, usage: u32) -> bool {
    usage_page == HID_CONSUMER_PAGE
        && matches!(
            usage,
            HID_VOLUME_INCREMENT
                | HID_VOLUME_DECREMENT
                | HID_MUTE
                | HID_PLAY_PAUSE
                | HID_SCAN_NEXT
                | HID_SCAN_PREV
                | HID_STOP
                | HID_EJECT
        )
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

fn volume_fallback_enabled() -> bool {
    match std::env::var("DJI_VOLUME_FALLBACK") {
        Ok(v) => {
            let v = v.trim();
            v != "0" && !v.eq_ignore_ascii_case("false")
        }
        Err(_) => true,
    }
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
        NX_KEYTYPE_MUTE => AuxMediaKey::Mute,
        _ => return None,
    };
    Some((key, pressed))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuxMediaKey {
    SoundUp,
    SoundDown,
    Mute,
}

/// Decide whether a system volume/mute media-key is the DJI Link path.
///
/// Mic 2 RX is often `AppleUserHIDEventService`: Link changes system volume
/// (CGEvent) but `IOHIDManager` value callbacks never run. In that case we
/// still treat a sourceless volume key as Link while the USB receiver is
/// present, so laptop keys with a real keyboard type stay untouched.
pub fn should_treat_as_dji_media_event(
    receiver_present: bool,
    hid_recent: bool,
    event_vendor_id: u32,
    event_product_id: u32,
    keyboard_type: i64,
    hid_values_seen: bool,
    volume_fallback: bool,
) -> bool {
    if hid_recent {
        return true;
    }
    if is_dji_vendor(event_vendor_id) {
        return true;
    }
    if event_product_id != 0 && KNOWN_RECEIVER_PRODUCT_IDS.contains(&event_product_id) {
        return true;
    }
    volume_fallback
        && receiver_present
        && !hid_values_seen
        && event_vendor_id == 0
        && keyboard_type == 0
}

fn format_hid_usage(usage_page: u32, usage: u32) -> String {
    format!("0x{usage_page:04x}/0x{usage:04x}")
}

fn remember_device_name(name: &str) {
    if name.trim().is_empty() {
        return;
    }
    if let Ok(mut slot) = LAST_DEVICE_NAME.lock() {
        if slot.as_deref() != Some(name) {
            info!("DJI mic trigger: receiver HID '{name}'");
            *slot = Some(name.to_string());
        }
    }
}

fn remember_device_present(name: &str) {
    remember_device_name(name);
    RECEIVER_PRESENT.store(true, Ordering::SeqCst);
}

fn remember_button(usage: &str) {
    BUTTON_SEEN.store(true, Ordering::SeqCst);
    RECEIVER_PRESENT.store(true, Ordering::SeqCst);
    if let Ok(mut slot) = LAST_HID_USAGE.lock() {
        if slot.as_deref() != Some(usage) {
            *slot = Some(usage.to_string());
        }
    }
}

fn receiver_is_present() -> bool {
    RECEIVER_PRESENT.load(Ordering::SeqCst)
        || LAST_DEVICE_NAME
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .is_some_and(|n| looks_like_dji_mic_name(&n))
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

fn warm_dji_receiver_stream(app: &AppHandle) {
    let Some(rm) = app.try_state::<std::sync::Arc<crate::managers::audio::AudioRecordingManager>>()
    else {
        return;
    };
    match rm.start_microphone_stream() {
        Ok(()) => info!(
            "DJI mic trigger: warmed Wireless Microphone RX stream so Link \
             does not re-init USB audio on the first press"
        ),
        Err(e) => warn!("DJI mic trigger: could not warm microphone stream: {e}"),
    }
}

fn recover_dji_receiver_stream(app: AppHandle) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(DJI_STREAM_RECOVER_MS));
        let Some(rm) =
            app.try_state::<std::sync::Arc<crate::managers::audio::AudioRecordingManager>>()
        else {
            return;
        };
        if let Err(e) = rm.start_microphone_stream() {
            warn!("DJI mic trigger: microphone stream recover failed: {e}");
        } else {
            debug!("DJI mic trigger: microphone stream recover/keep-alive ok");
        }
    });
}

fn stop_warm_stream_if_on_demand(app: &AppHandle) {
    if get_settings(app).always_on_microphone {
        return;
    }
    let Some(rm) = app.try_state::<std::sync::Arc<crate::managers::audio::AudioRecordingManager>>()
    else {
        return;
    };
    if rm.is_recording() {
        return;
    }
    rm.stop_microphone_stream();
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
        report_bufs: Mutex<Vec<Box<[u8]>>>,
    }

    pub fn stop_runloop() {
        let ptr = RUNLOOP.load(Ordering::SeqCst);
        if !ptr.is_null() {
            let rl = unsafe { CFRunLoop::wrap_under_get_rule(ptr as _) };
            rl.stop();
        }
    }

    pub fn start_listener(app: AppHandle) -> ListenerGuard {
        BUTTON_SEEN.store(false, Ordering::SeqCst);
        HID_VALUES_SEEN.store(false, Ordering::SeqCst);
        if let Ok(mut slot) = LAST_HID_USAGE.lock() {
            *slot = None;
        }
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
            report_bufs: Mutex::new(Vec::new()),
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
                "DJI mic trigger: listening for vendor 0x{:04x} (SInt32 match, \
                 event tap on, Link short-press = volume HID)",
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

    fn hid_s32(value: u32) -> CFNumber {
        // IOKit VendorID/ProductID matching expects kCFNumberSInt32Type.
        // CFNumber::from(i64) is SInt64 and often matches *nothing*.
        CFNumber::from(value as i32)
    }

    fn matching_dict(pairs: &[(&'static str, u32)]) -> CFDictionary {
        // core-foundation 0.10 defaults CFDictionary to<*const c_void,*const c_void>;
        // build typed pairs then convert to untyped for IOKit.
        let owned: Vec<(CFString, CFNumber)> = pairs
            .iter()
            .map(|(key, value)| (CFString::from_static_string(key), hid_s32(*value)))
            .collect();
        CFDictionary::from_CFType_pairs(&owned).to_untyped()
    }

    fn open_hid_manager(shared: &Shared) -> Option<HidManager> {
        unsafe {
            let manager = IOHIDManagerCreate(std::ptr::null(), K_IO_HID_OPTIONS_NONE);
            if manager.is_null() {
                return None;
            }

            // Vendor-only, SInt32. Product-only matching is too easy to miss a
            // firmware PID; 0x4008 still matches because it is vendor 0x2CA3.
            let vendor = matching_dict(&[("VendorID", env_vendor_id())]);
            IOHIDManagerSetDeviceMatching(manager, vendor.as_concrete_TypeRef() as *const c_void);

            // Receive every element, not just volume increment.
            IOHIDManagerSetInputValueMatching(manager, std::ptr::null());

            IOHIDManagerRegisterDeviceMatchingCallback(
                manager,
                hid_device_matching_callback,
                shared as *const Shared as *mut c_void,
            );
            IOHIDManagerRegisterDeviceRemovalCallback(
                manager,
                hid_device_removal_callback,
                shared as *const Shared as *mut c_void,
            );
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

            if !log_matched_devices(manager) {
                info!(
                    "DJI mic trigger: no vendor match after SInt32 open — \
                     broadening IOHIDManager to all HID devices"
                );
                IOHIDManagerSetDeviceMatching(manager, std::ptr::null());
                IOHIDManagerSetInputValueMatching(manager, std::ptr::null());
                let _ = log_matched_devices(manager);
            }

            Some(HidManager { manager })
        }
    }

    fn log_matched_devices(manager: *mut c_void) -> bool {
        unsafe {
            let raw = IOHIDManagerCopyDevices(manager);
            if raw.is_null() {
                info!("DJI mic trigger: IOHIDManager CopyDevices is empty");
                return false;
            }
            let count = CFSetGetCount(raw);
            info!("DJI mic trigger: IOHIDManager matched {count} HID device(s)");
            if count <= 0 {
                CFRelease(raw);
                return false;
            }
            let mut values = vec![std::ptr::null(); count as usize];
            CFSetGetValues(raw, values.as_mut_ptr());
            let mut saw_dji = false;
            for device in values {
                if device.is_null() {
                    continue;
                }
                let device = device as *mut c_void;
                let vendor_id = hid_number_prop(device, "VendorID").unwrap_or(0);
                let product_id = hid_number_prop(device, "ProductID").unwrap_or(0);
                let usage_page = hid_number_prop(device, "PrimaryUsagePage").unwrap_or(0);
                let usage = hid_number_prop(device, "PrimaryUsage").unwrap_or(0);
                let name = hid_string_prop(device, "Product").unwrap_or_default();
                info!(
                    "DJI mic trigger: matched HID vendor=0x{vendor_id:04x} \
                     product=0x{product_id:04x} primary={usage_page:#06x}/{usage:#06x} \
                     name='{name}'"
                );
                if is_dji_vendor(vendor_id) {
                    saw_dji = true;
                    remember_device_present(&name);
                }
            }
            CFRelease(raw);
            saw_dji
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

    fn describe_device(device: *mut c_void) -> (u32, u32, String, u32, u32) {
        let vendor_id = hid_number_prop(device, "VendorID").unwrap_or(0);
        let product_id = hid_number_prop(device, "ProductID").unwrap_or(0);
        let usage_page = hid_number_prop(device, "PrimaryUsagePage").unwrap_or(0);
        let usage = hid_number_prop(device, "PrimaryUsage").unwrap_or(0);
        let name = hid_string_prop(device, "Product").unwrap_or_default();
        (vendor_id, product_id, name, usage_page, usage)
    }

    fn register_input_report(shared: &Shared, device: *mut c_void) {
        let max_len = hid_number_prop(device, "MaxInputReportSize")
            .unwrap_or(64)
            .clamp(8, 256) as usize;
        let mut buf = vec![0u8; max_len].into_boxed_slice();
        unsafe {
            IOHIDDeviceRegisterInputReportCallback(
                device,
                buf.as_mut_ptr(),
                buf.len() as isize,
                hid_report_callback,
                shared as *const Shared as *mut c_void,
            );
        }
        if let Ok(mut slots) = shared.report_bufs.lock() {
            slots.push(buf);
        }
    }

    extern "C" fn hid_device_matching_callback(
        context: *mut c_void,
        _result: i32,
        _sender: *mut c_void,
        device: *mut c_void,
    ) {
        if device.is_null() {
            return;
        }
        let (vendor_id, product_id, name, usage_page, usage) = describe_device(device);
        if !is_dji_vendor(vendor_id) {
            return;
        }
        remember_device_present(&name);
        info!(
            "DJI mic trigger: device arrived vendor=0x{vendor_id:04x} \
             product=0x{product_id:04x} primary={usage_page:#06x}/{usage:#06x} \
             name='{name}'"
        );
        let shared = unsafe { &*(context as *const Shared) };
        register_input_report(shared, device);
    }

    extern "C" fn hid_device_removal_callback(
        _context: *mut c_void,
        _result: i32,
        _sender: *mut c_void,
        device: *mut c_void,
    ) {
        if device.is_null() {
            return;
        }
        let (vendor_id, product_id, name, _, _) = describe_device(device);
        if is_dji_vendor(vendor_id) {
            info!(
                "DJI mic trigger: device removed vendor=0x{vendor_id:04x} \
                 product=0x{product_id:04x} name='{name}'"
            );
        }
    }

    extern "C" fn hid_value_callback(
        context: *mut c_void,
        _result: i32,
        _sender: *mut c_void,
        value: *mut c_void,
    ) {
        let shared = unsafe { &*(context as *const Shared) };
        let Some(event) = read_hid_event(value) else {
            return;
        };
        if !is_dji_vendor(event.vendor_id) {
            return;
        }

        remember_device_present(&event.product_name);
        let usage = format_hid_usage(event.usage_page, event.usage);
        // Log every DJI usage — previous builds only logged 0xE9/0xEA, so a
        // different Link usage looked like "callback never fired".
        info!(
            "DJI mic trigger: HID vendor=0x{:04x} product=0x{:04x} \
             usage={usage} value={} name='{}'",
            event.vendor_id, event.product_id, event.int_value, event.product_name
        );

        if !is_dji_trigger_usage(event.usage_page, event.usage) {
            return;
        }

        HID_VALUES_SEEN.store(true, Ordering::SeqCst);
        shared.last_dji_hid_ms.store(now_ms(), Ordering::SeqCst);
        if event.int_value != 0 {
            remember_button(&usage);
            if !shared.tap_active.load(Ordering::SeqCst) {
                fire_hid_only_toggle(&shared.app);
                recover_dji_receiver_stream(shared.app.clone());
            }
        }
    }

    extern "C" fn hid_report_callback(
        _context: *mut c_void,
        _result: i32,
        sender: *mut c_void,
        _type: u32,
        report_id: u32,
        report: *mut u8,
        report_length: isize,
    ) {
        if sender.is_null() || report.is_null() || report_length <= 0 {
            return;
        }
        let vendor_id = hid_number_prop(sender, "VendorID").unwrap_or(0);
        if !is_dji_vendor(vendor_id) {
            return;
        }
        let product_id = hid_number_prop(sender, "ProductID").unwrap_or(0);
        let name = hid_string_prop(sender, "Product").unwrap_or_default();
        remember_device_present(&name);
        let len = report_length as usize;
        let bytes = unsafe { std::slice::from_raw_parts(report, len) };
        let hex: String = bytes
            .iter()
            .take(16)
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        info!(
            "DJI mic trigger: HID report vendor=0x{vendor_id:04x} \
             product=0x{product_id:04x} id={report_id} len={len} bytes=[{hex}] \
             name='{name}'"
        );
    }

    extern "C" fn event_tap_callback(
        _proxy: *mut c_void,
        event_type: u32,
        event: *mut c_void,
        user_info: *mut c_void,
    ) -> *mut c_void {
        let shared = unsafe { &*(user_info as *const Shared) };

        if event_type == K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT
            || event_type == K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT
        {
            let port = shared.tap_port.load(Ordering::SeqCst);
            if !port.is_null() {
                unsafe { CGEventTapEnable(port, true) };
            }
            return event;
        }

        if event_type != NS_EVENT_TYPE_SYSTEM_DEFINED {
            return event;
        }

        let Some(aux) = ns_event_aux_fields(event) else {
            return event;
        };
        let Some((key, pressed)) = decode_aux_control(aux.subtype, aux.data1) else {
            return event;
        };

        let keyboard_type =
            unsafe { CGEventGetIntegerValueField(event, CG_KEYBOARD_EVENT_KEYBOARD_TYPE) };
        let age = now_ms().saturating_sub(shared.last_dji_hid_ms.load(Ordering::SeqCst));
        let hid_recent =
            shared.last_dji_hid_ms.load(Ordering::SeqCst) != 0 && age <= DJI_MEDIA_WINDOW_MS;
        let attributed = should_treat_as_dji_media_event(
            receiver_is_present(),
            hid_recent,
            aux.vendor_id,
            aux.product_id,
            keyboard_type,
            HID_VALUES_SEEN.load(Ordering::SeqCst),
            volume_fallback_enabled(),
        );

        info!(
            "DJI mic trigger: media {:?} pressed={pressed} vendor=0x{:04x} \
             product=0x{:04x} kbdType={keyboard_type} data1={} age_ms={age} \
             hid_recent={hid_recent} attributed={attributed}",
            key, aux.vendor_id, aux.product_id, aux.data1
        );

        if !attributed {
            return event;
        }

        let usage = match key {
            AuxMediaKey::SoundUp => "media:sound-up",
            AuxMediaKey::SoundDown => "media:sound-down",
            AuxMediaKey::Mute => "media:mute",
        };
        remember_button(usage);
        fire_media_edge(&shared.app, pressed);
        if pressed {
            recover_dji_receiver_stream(shared.app.clone());
        }
        debug!("DJI mic trigger: swallowed media key pressed={pressed} age_ms={age}");
        // Swallow so Link does not change system volume. This does not undo
        // on-device TX mute/NR — those are firmware, not a CGEvent.
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

    struct AuxEventFields {
        subtype: i16,
        data1: i64,
        vendor_id: u32,
        product_id: u32,
    }

    fn ns_event_aux_fields(event: *mut c_void) -> Option<AuxEventFields> {
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
            let vendor_id: isize = msg_send![ns_event, vendorID];
            let product_id: isize = msg_send![ns_event, productID];
            Some(AuxEventFields {
                subtype: subtype as i16,
                data1: data1 as i64,
                vendor_id: vendor_id as u32,
                product_id: product_id as u32,
            })
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
    type IOHIDDeviceCallback = extern "C" fn(*mut c_void, i32, *mut c_void, *mut c_void);
    type IOHIDReportCallback =
        extern "C" fn(*mut c_void, i32, *mut c_void, u32, u32, *mut u8, isize);
    type CGEventTapCallBack =
        extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> *mut c_void;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOHIDManagerCreate(allocator: *const c_void, options: u32) -> *mut c_void;
        fn IOHIDManagerSetDeviceMatching(manager: *mut c_void, matching: *const c_void);
        fn IOHIDManagerSetInputValueMatching(manager: *mut c_void, matching: *const c_void);
        fn IOHIDManagerRegisterInputValueCallback(
            manager: *mut c_void,
            callback: IOHIDValueCallback,
            context: *mut c_void,
        );
        fn IOHIDManagerRegisterDeviceMatchingCallback(
            manager: *mut c_void,
            callback: IOHIDDeviceCallback,
            context: *mut c_void,
        );
        fn IOHIDManagerRegisterDeviceRemovalCallback(
            manager: *mut c_void,
            callback: IOHIDDeviceCallback,
            context: *mut c_void,
        );
        fn IOHIDManagerCopyDevices(manager: *mut c_void) -> *mut c_void;
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
        fn IOHIDDeviceRegisterInputReportCallback(
            device: *mut c_void,
            report: *mut u8,
            report_length: isize,
            callback: IOHIDReportCallback,
            context: *mut c_void,
        );
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
        fn CGEventGetIntegerValueField(event: *mut c_void, field: u32) -> i64;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: *const c_void);
        fn CFSetGetCount(set: *mut c_void) -> isize;
        fn CFSetGetValues(set: *mut c_void, values: *mut *const c_void);
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
    fn trigger_usages_include_volume_and_common_media() {
        assert!(is_dji_trigger_usage(
            HID_CONSUMER_PAGE,
            HID_VOLUME_INCREMENT
        ));
        assert!(is_dji_trigger_usage(HID_CONSUMER_PAGE, HID_MUTE));
        assert!(is_dji_trigger_usage(HID_CONSUMER_PAGE, HID_PLAY_PAUSE));
        assert!(!is_dji_trigger_usage(
            HID_CONSUMER_PAGE,
            HID_CONSUMER_CONTROL_COLLECTION
        ));
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
        assert_eq!(
            decode_aux_control(8, 0x00070A00),
            Some((AuxMediaKey::Mute, true))
        );
        assert_eq!(decode_aux_control(0, 2560), None);
        assert_eq!(decode_aux_control(8, 0), None);
    }

    #[test]
    fn event_tap_uses_hid_window_even_without_vendor_on_nsevent() {
        assert!(should_treat_as_dji_media_event(
            true, true, 0, 0, 58, true, true
        ));
    }

    #[test]
    fn nsevent_vendor_is_enough_without_hid_values() {
        assert!(should_treat_as_dji_media_event(
            true,
            false,
            DJI_VENDOR_ID,
            0x4008,
            0,
            false,
            true
        ));
    }

    #[test]
    fn event_service_fallback_only_when_receiver_present_and_no_hid_values() {
        assert!(should_treat_as_dji_media_event(
            true, false, 0, 0, 0, false, true
        ));
        assert!(!should_treat_as_dji_media_event(
            false, false, 0, 0, 0, false, true
        ));
        assert!(!should_treat_as_dji_media_event(
            true, false, 0, 0, 58, false, true
        ));
        assert!(!should_treat_as_dji_media_event(
            true, false, 0, 0, 0, true, true
        ));
        assert!(!should_treat_as_dji_media_event(
            true, false, 0, 0, 0, false, false
        ));
    }

    #[test]
    fn laptop_volume_with_keyboard_type_is_not_dji_without_hid() {
        assert!(!should_treat_as_dji_media_event(
            true, false, 0x05ac, 0x0267, 58, false, true
        ));
    }
}
