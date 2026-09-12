# DJI Mic button → Handy dictation (macOS)

Handy can start and stop dictation from **DJI Mic 2 transmitter Link** when the USB receiver is plugged into a Mac.

This is not a generic “any hardware button” feature. It is scoped to the HID / system-volume event macOS actually exposes for these receivers.

## Short answer (Mic 2)

| Gesture                          | What happens on a USB-connected Mac                                                                                         |
| -------------------------------- | --------------------------------------------------------------------------------------------------------------------------- |
| **TX Link short press**          | Receiver forwards **consumer volume HID** (usually Sound Up). Handy starts/stops dictation. **This is the control to use.** |
| **TX Link hold (~2s)**           | **Pairing / search.** Do not use this for Handy.                                                                            |
| **TX Power short press**         | **Noise reduction** on the transmitter. Can make dictation sound muted or dead. Not a Mac HID trigger.                      |
| **TX Rec hold (~3s)**            | Switch RX vs Bluetooth mode. Not a Mac HID trigger.                                                                         |
| RX touchscreen / RX power        | Device UI / lock. Not claimed as a Handy button.                                                                            |
| Bluetooth-only (no USB receiver) | Audio only, no button HID.                                                                                                  |

Do not hold Link. Do not use Power or Rec to trigger Handy.

## Prerequisites

1. macOS (this path is not implemented on Windows or Linux).
2. Handy from this checkout. Upstream bundle id is `com.pais.handy`. A local Brhyt rebrand (`com.brhyt.handy` under `/Users/gi1bertorey/Sites/brhyt-Handy`) uses the same code path.
3. DJI **receiver** connected with **USB-C to the Mac**. The transmitter stays wirelessly paired to the receiver.
4. A transcription model already downloaded in Handy.
5. **Accessibility** permission for Handy (already required to paste text).
6. **Input Monitoring** for Handy if you want Link **not** to change system volume.

   System Settings → Privacy & Security → Accessibility  
   System Settings → Privacy & Security → Input Monitoring

## Enable it

1. Open Handy → **General**.
2. Turn on **DJI Mic button**.
3. Confirm the status line becomes **USB receiver present** (not “Waiting for the USB receiver”). If it stays on waiting, the RX is not visible to CoreAudio yet — unplug/replug USB.
4. Set microphone to **Wireless Microphone RX**, channel as needed, **Shortcut Behavior** to **Toggle** or **Auto**.
5. **Short-press TX Link** once. Status should become **Link/volume HID seen**. Press again to stop and dictate.

Handy keeps the RX capture stream warm while this setting is on so Link does not coincide with the first CoreAudio open (that pair can drop TX audio on the USB composite device).

## How it works

A USB-connected DJI receiver is an audio device plus a **consumer-control** HID interface (vendor `0x2CA3`). Known product IDs: `0x4008` (Mic 2 “Wireless Microphone RX”), `0x4011`.

On Mic 2, **TX Link short-press is forwarded over the wireless link and emitted by the RX as volume increment** (`0x0C` / `0xE9`). macOS turns that into an `NSSystemDefined` Sound Up/Down event — the same event that bumps system volume when Handy is off.

The RX often appears in `hidutil` as `AppleUserHIDEventService` with device-level usage page 12 / usage 1 (Consumer Control **collection**, not volume). In that mode `IOHIDManager` **input-value callbacks may never fire**, even though the device is listed. Handy therefore:

1. Matches HID with **SInt32** vendor dictionaries (SInt64 matching often matches nothing), logs every DJI usage/report, and enumerates `CopyDevices`.
2. Treats the **CGEvent tap** as the reliable Link edge: Sound Up/Down attributed to DJI (NSEvent vendor/product, or a sourceless volume key while the USB receiver is present and no HID values have arrived).
3. Swallows that media-key so Link does not change system volume. Laptop volume keys with a real keyboard type are left alone.
4. Feeds Handy’s existing transcription coordinator (Toggle / Hold / Auto).
5. Warms and keeps Wireless Microphone RX open, and re-checks the stream after each Link edge, so opening capture is less likely to reset the USB audio path.

Swallowing a CGEvent does **not** undo on-device TX noise reduction. If audio dies after a **Power** press, turn NR off on the TX. If audio dies after **Link** while Handy is recording, Handy re-opens the RX stream; also try **Always-On Microphone**.

No `hidutil` mapping is installed.

## Settings status

| Status                                     | Meaning                                                |
| ------------------------------------------ | ------------------------------------------------------ |
| Waiting for the USB receiver               | CoreAudio does not see Wireless Microphone RX          |
| USB receiver present … short-press TX Link | RX is plugged in; Handy has not seen Link/volume yet   |
| Link/volume HID seen                       | A press was attributed (HID usage or `media:sound-up`) |

## How to rebuild and test (Brhyt Handy)

From `/Users/gi1bertorey/Sites/brhyt-Handy` on the same branch (`cursor/dji-mic-2-trigger-6c01`):

```bash
git pull
bun install
# optional VAD model, once:
mkdir -p src-tauri/resources/models
curl -o src-tauri/resources/models/silero_vad_v4.onnx https://blob.handy.computer/silero_vad_v4.onnx

bun run tauri dev
# or a local .app:
bun run tauri build
```

Then:

1. Plug in the USB receiver. Confirm `hidutil list` still shows Vendor `0x2ca3` Product `0x4008` “Wireless Microphone RX”.
2. Enable **DJI Mic button**. Status must switch to **USB receiver present** without pressing anything.
3. With Handy **off** (control): short-press Link — system volume should still bump. That proves the HID→media-key path.
4. With Handy **on**: short-press Link. Expect a log line `DJI mic trigger: media SoundUp … attributed=true` and/or `DJI mic trigger: HID …`. Status → **Link/volume HID seen**. Dictation should start. TX audio should keep reaching the Mac.
5. Confirm MacBook volume keys still change volume and do not toggle Handy.
6. Hold Link only if you intend to pair — that is not the Handy gesture.

Optional environment overrides (Handy process):

```bash
DJI_VENDOR_ID=0x2ca3 DJI_PRODUCT_ID=0x4008 /Applications/Brhyt\ Handy.app/Contents/MacOS/Handy
# Disable the sourceless-volume fallback if laptop volume keys are stolen:
DJI_VOLUME_FALLBACK=0
```

## Permissions

| Permission       | Why                                                                                                            |
| ---------------- | -------------------------------------------------------------------------------------------------------------- |
| Microphone       | Record audio for transcription (existing)                                                                      |
| Accessibility    | Paste the transcript (existing). Also required for the media-key tap.                                          |
| Input Monitoring | Read the system-defined media-key stream so Handy can swallow DJI volume without touching keyboard volume keys |

If Input Monitoring is denied, dictation can still start from a HID pulse; Link may change volume.

## Related third-party approaches

Research only. This fork calls Handy’s coordinator directly instead of synthesizing `fn+F18` or remapping to Right Command.

- [drpedapati/handy-dji-mic-trigger](https://github.com/drpedapati/handy-dji-mic-trigger) — LaunchAgent + `IOHIDManager` + `CGEventTap` → `fn+F18` (also requires a HID value unless `REQUIRE_DJI_HID_EVENT=0`)
- [hueyluox/dji-mic-command](https://github.com/hueyluox/dji-mic-command) — `hidutil` maps RX connecting key (volume HID, PID `0x4011`) to Right Command
- [caezium/dji-mic-wispr-flow](https://github.com/caezium/dji-mic-wispr-flow) — Karabiner maps Mic Mini 2 TX linking → RX USB volume HID to Wispr Flow
