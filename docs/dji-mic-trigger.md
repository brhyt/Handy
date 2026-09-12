# DJI Mic button → Handy dictation (macOS)

Handy can start and stop dictation from a **DJI wireless-mic receiver button** when the receiver is plugged into a Mac over USB.

This is not a generic “any hardware button” feature. It is scoped to the HID event macOS actually exposes for these receivers.

## Short answer

| Gesture                                                                                                                             | Works on a USB-connected Mac?                                                                                                  |
| ----------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| Receiver USB consumer-control button (reported as volume up/down)                                                                   | **Yes** — this is what Handy listens for                                                                                       |
| Receiver linking / connecting button, if that press is the volume HID event                                                         | **Yes**, same path                                                                                                             |
| Transmitter **Link** button as its own Mac HID device                                                                               | **No** — macOS does not present a dedicated TX Link key                                                                        |
| Transmitter Link button forwarded by the RX as the same volume HID (camera-shutter feature on some models, confirmed on Mic Mini 2) | **Maybe** — Handy will fire if that press appears as the RX volume event. This has **not** been hardware-verified on DJI Mic 2 |
| Bluetooth-only (no USB receiver)                                                                                                    | **No** — audio only, no button HID                                                                                             |

Do not assume the Mic 2 transmitter Link button works until you verify it with the steps below.

## Prerequisites

1. macOS (this path is not implemented on Windows or Linux).
2. Handy built from this fork (`com.pais.handy`).
3. DJI receiver connected with **USB-C to the Mac**. The transmitter can stay wirelessly paired to the receiver.
4. A transcription model already downloaded in Handy.
5. **Accessibility** permission for Handy (already required to paste text).
6. **Input Monitoring** for Handy if you want the DJI button **not** to change system volume.

   System Settings → Privacy & Security → Accessibility  
   System Settings → Privacy & Security → Input Monitoring

## Enable it

1. Open Handy → **General**.
2. Turn on **DJI Mic button**.
3. If Handy has not already chosen a microphone, it selects a connected input named like `Wireless Microphone RX` / `Wireless Mic Rx`.
4. Set **Shortcut Behavior** to **Toggle** or **Auto**. Push-to-talk is a poor fit for a click-style DJI button unless the press is held and HID reports a real hold.
5. Press the receiver button that macOS sees as volume (often the RX linking/connecting control). Handy should start listening; press again to stop and dictate.

The status line under the toggle updates after the first HID event: `Receiver button seen: …`.

## How it works

Handy already has three first-party triggers:

- Global keyboard shortcuts (Tauri or handy-keys), with Toggle / Hold / Auto
- CLI: `handy --toggle-transcription` via the single-instance plugin
- Unix signals: `SIGUSR2` (and `SIGUSR1` on macOS for post-process)

It does **not** bind media keys or arbitrary HID devices.

A USB-connected DJI receiver is an audio device plus a **consumer-control** HID interface (vendor `0x2CA3`). The button macOS can see is volume increment (`0x0C` / `0xE9`) or decrement (`0x0C` / `0xEA`), not a keyboard key. Known product IDs from third-party tools: `0x4008`, `0x4011` (“Wireless Mic Rx”). Unknown Mic 2 IDs still match on vendor + that consumer usage.

When **DJI Mic button** is on, Handy:

1. Opens an `IOHIDManager` matching DJI vendor `0x2CA3`.
2. Marks a short window when that device sends consumer volume.
3. If Input Monitoring allows a `CGEvent` tap, swallows the matching system media-key **only in that window** so the MacBook volume keys stay normal, then feeds Handy’s existing transcription coordinator (same Toggle / Hold / Auto modes).
4. If the tap cannot be created, Handy still toggles from the HID pulse. The DJI button may also change system volume until Input Monitoring is granted.

No `hidutil` mapping is installed, so quitting Handy restores the receiver’s normal volume-button behavior. Laptop volume keys are never remapped.

## What will not work

- **TX Link as a dedicated Mac button.** The transmitter linking control is a pairing / camera-shutter control on the TX. A Mac with the RX on USB does not get a separate “Link” HID usage for that key.
- **Bluetooth-only mode.** No receiver HID, so no button events.
- **Remapping someone else’s volume keys.** The tap only swallows a media-key that arrived within 350 ms of a DJI HID event.

## How to verify which button you have

With the receiver plugged in:

```bash
hidutil list | grep -i -A2 -B2 "Wireless Mic"
```

You want a device whose Vendor ID is `0x2ca3` (11427) and whose name looks like `Wireless Mic Rx` or `Wireless Microphone RX`. Product ID is often `0x4008` or `0x4011`; other IDs can still work.

Then:

1. Enable **DJI Mic button** in Handy.
2. Enable Handy’s debug logging (or watch the log directory from Settings).
3. Press the **receiver** button once. A log line `DJI mic trigger: HID vendor=0x2ca3 …` and the settings status `Receiver button seen` mean that button is the working one.
4. Press the **transmitter Link** button. If the same HID line appears, that TX press is forwarded and will start/stop Handy. If nothing is logged, that TX button cannot be observed on this Mac — use the receiver button.

Optional environment overrides (Handy process):

```bash
DJI_VENDOR_ID=0x2ca3 DJI_PRODUCT_ID=0x4011 /Applications/Handy.app/Contents/MacOS/Handy
```

`DJI_PRODUCT_ID` restricts matching to one product. Leave it unset unless you are debugging a specific receiver.

## Permissions

| Permission       | Why                                                                                                            |
| ---------------- | -------------------------------------------------------------------------------------------------------------- |
| Microphone       | Record audio for transcription (existing)                                                                      |
| Accessibility    | Paste the transcript (existing). Also required for the media-key tap.                                          |
| Input Monitoring | Read the system-defined media-key stream so Handy can swallow DJI volume without touching keyboard volume keys |

If Input Monitoring is denied, dictation can still start from the HID listener; the DJI button may change volume.

## Related third-party approaches

These were used as research only. This fork calls Handy’s coordinator directly instead of synthesizing `fn+F18` or remapping to Right Command.

- [drpedapati/handy-dji-mic-trigger](https://github.com/drpedapati/handy-dji-mic-trigger) — LaunchAgent + `IOHIDManager` + `CGEventTap` → `fn+F18`
- [hueyluox/dji-mic-command](https://github.com/hueyluox/dji-mic-command) — `hidutil` maps RX connecting key (volume HID, PID `0x4011`) to Right Command
- [caezium/dji-mic-wispr-flow](https://github.com/caezium/dji-mic-wispr-flow) — Karabiner maps Mic Mini 2 TX linking → RX USB volume HID to Wispr Flow
