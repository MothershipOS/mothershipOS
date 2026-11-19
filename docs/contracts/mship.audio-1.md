# mship.audio 1.x

The phone's audio devices as the guest's own sound devices, one per phone device, so the guest's audio stack routes
between them natively, as on the phone (Android's audio policy: earpiece for calls, the wired headset while it is
plugged in, the speaker for media; a Linux desktop: its own output/input selection). The host keeps the amplifiers,
the codec, the drivers, the speaker protection and the mixing with the host's own sounds.

Four handles, each exclusive (one VM holds it at a time) and lent separately:

| Handle | Group | The phone's device |
|---|---|---|
| `audio:speaker` | `speaker` | the loudspeaker |
| `audio:earpiece` | `earpiece` | the earpiece |
| `audio:headset` | `headset` | the 3.5 mm jack: headphones, headset mic and the jack's plug state |
| `audio:mic` | `microphone` | the built-in microphones (main and back) |

## Guest view
- **Device:** one virtio-snd device (the platform says how; `cuttlefish-arm64`: a vhost-user sound device) with six
  PCM streams, each its own PCM device (the stream's `hda_fn_nid` = its index), in this fixed order:

  | PCM | Direction | Device |
  |---|---|---|
  | 0 | playback | speaker |
  | 1 | playback | earpiece |
  | 2 | playback | wired headset / headphones |
  | 3 | capture | built-in microphone (main, bottom) |
  | 4 | capture | back microphone |
  | 5 | capture | wired headset microphone |

  A phone without one of these devices still has the stream (it stays empty).
- **Jacks:** the headset jack, twice: as two virtio-snd jacks (headphone on PCM 2, microphone on PCM 5; HDA pin
  configurations: headphone out / mic in, 1/8" jack, presence detect; VIRTIO_SND_EVT_JACK_CONNECTED / DISCONNECTED),
  which Linux's virtio-snd driver makes phantom jacks (ALSA controls only); and as input switches
  (SW_HEADPHONE_INSERT, SW_MICROPHONE_INSERT) of a virtio-input switches device, which Android's wired accessory
  handling and desktop sessions use. Both follow the phone's jack while the VM holds `audio:headset`.
- **Formats:** what virtio-snd negotiates; the host converts. The host mixes at 48 kHz.
- **Timing:** a stream's buffers complete as the device plays / records them (real time), whether or not the handle
  is held.
- **No negotiation message:** the device only; the manifest slot pins the version.

## Caps
`devices`: the handles the phone has (nio: speaker, earpiece, headset, mic).

## Attached / empty
- **Empty (handle not held):** its streams exist and run in real time; playback is not heard, capture delivers
  silence; for `audio:headset` the jacks read unplugged.
- **Attach:** the guest's streams reach the device at once (a stream already playing included); `audio:headset`
  reports the jack's current state (an event if plugged in).
- **Release (any reason):** that device goes silent for the guest at once (capture: silence); for `audio:headset` the
  jacks report unplugged. Streams stay open and keep running in real time.
- **Host default afterwards:** the device serves the host's own sounds.

**History:** 1.0 (2026.1) initial. (An earlier, unreleased draft of 1.0 had one generic output and input, routed by the
host.)

**Implementation:** the launcher (`audio_slot()` in `crates/mship-launch/src/main.rs`) starts per VM one PipeWire node
per stream, `mship-vm-<vm>.<speaker|earpiece|headset|mic|backmic|headsetmic>` (pw-loopbacks: sinks whose output
`.out`, sources whose input `-in`, mship-audio links; the phone side is passive, so a device only runs while the guest
uses it), and `vhost-device-sound --backend alsa` with `MSHIP_SOUND_STREAMS=out,out,out,in,in,in` and
`MSHIP_SOUND_JACKS=2:hp,5:mic` (our patches, `host/vm-sound/patches`: 0013 streams and jacks; 0011 completion when
played; 0012 device open only while in use; 0006–0010 PipeWire fixes and tracing, `MSHIP_SOUND_TRACE=1`). The
backend's PCM `mship_pcm<n>` is the PipeWire plugin with that stream's node (`<vm dir>/audio/.asoundrc`); the jack
state comes from `<vm dir>/audio/jacks` (`hp=0|1 mic=0|1`), which the launcher also serves as the switches device
(crosvm `--input=switches`, `<vm dir>/jack.sock`). `mship-audio` (`crates/mship-devices/src/bin/audio.rs`,
`mship-audio.service`) links the nodes to the phone's devices the VM holds, else to the null sink `mship-silence` (its
monitor for inputs); writes the jack state for the headset holder; and makes the host's own routing follow the jack
(card profile: headphones/earpiece, headset mic/built-in mics). Devices: `/etc/mothership/audio.json` (`speaker`,
`earpiece`, `headphones`, `mic` (stereo: main, back), `headset_mic`). Control socket `/run/mothership/audio.sock`
(`attach <vm> <handle>`, `detach <vm> [reason] <handle>`, `status`), mothershipd items `speaker`, `earpiece`,
`headset`, `microphone` (lent while the VM runs). Guest side (PixelOS): the stock AIDL audio HAL with its device list
extended to these devices and PCMs, and Android's wired-accessory handling on input switches
(`pixelOS_16_VM/docs/android-vm.md`). The nio's devices: `motorola_nio/docs/audio.md`.

**Gaps (fix before 2026.1):**
- Earpiece and headphones share one codec path on the nio (as stock): the host plays whichever the jack selects; the
  guest's other one of the two is not heard meanwhile. Same for the main and the headset mic (one ADC).
- No call audio (voice runs in the modem's DSP on the nio); no Bluetooth or USB audio for VMs.
- The caps aren't published (no broker `list-handles` yet).
