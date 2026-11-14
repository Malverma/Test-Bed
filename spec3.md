# Test Bed — Specification 3: PC tab over the internet

The PC tab works only on the local network today: PCs are found with mDNS,
reached at a LAN address, and the firewall opens UDP 47800 to the home
network only. This spec lets a paired PC be reached from another network,
built into Test Bed, using [iroh](https://github.com/n0-computer/iroh)
(QUIC with hole punching and a relay fallback). No Tailscale, no port
forwarding.

## 1. Goals and non-goals

Goals:

- **G1** A PC on another network can be paired and controlled from Test
  Bed's PC tab, as its own way of connecting with its own button and list
  (*Computers on other networks*), separate from the local-network one.
- **G2** Two PCs that have never been on the same network pair with an
  invite code sent by any means (chat, email).
- **G3** The LAN path, its speed, and compatibility with RemoteLinux stay
  exactly as they are.
- **G4** Nothing new is exposed: over the internet a host answers only PCs
  paired over the internet, and only while access from other networks is on.
- **G5** A Windows 11 build of Test Bed can be downloaded, and two PCs
  running Test Bed (Linux or Windows) connect either way (§9).

Non-goals: NAT traversal without any public server (impossible in general,
see §3).

## 2. Choice of approach

| Option | Verdict |
|---|---|
| Replace quinn with iroh everywhere | Breaks RemoteLinux compatibility and every existing pairing; iroh's endpoint id would replace the certificate fingerprint. Rejected for now. |
| Tunnel the existing quinn connection through iroh datagrams | QUIC needs 1200-byte packets; iroh's datagrams are smaller than that once wrapped, so the inner connection can't start. Rejected. |
| **Two transports behind one connection type** | quinn for the LAN as today, iroh for the internet. Session, pairing and streaming code run on either. **Chosen.** |

iroh 1.4 is built on `noq`, a fork of quinn with the same connection,
stream and datagram API, so the wrapper is thin.

## 3. What has to be public

Two PCs behind different routers can't find each other without a third
party. iroh uses two kinds:

- **Relays**: both PCs keep an outgoing connection to a relay. It passes
  hole-punching messages, then the PCs talk directly. When hole punching
  fails (both behind hard NAT), the video goes through the relay.
- **Address lookup**: finds a PC's current relay from its endpoint id.

Default: n0's public relays and DNS lookup (iroh's `N0` preset). They're
free, but rate-limited and run by someone else. Settings let you name your
own relay (`iroh-relay`, one small server, e.g. a VPS), and Test Bed also
remembers each paired PC's last relay so lookup is rarely needed. All
traffic is end-to-end encrypted; a relay sees only ciphertext.

## 4. Identity and trust

- Each PC gets two iroh secret keys, one for sharing and one for viewing
  (`~/.local/share/test-bed/remote/iroh-host.key`, `iroh-client.key`, 0600),
  like the LAN identity's host and client roles: both run at once, and each
  publishes where it can be reached. A key's public half, the **endpoint
  id**, names the PC on the internet.
- PCs paired over the internet live in their own files,
  `internet_hosts.toml` and `internet_clients.toml`, with the endpoint id
  where LAN entries have a certificate fingerprint. The LAN files and their
  meaning don't change; a PC can be in both lists.
- The LAN and internet paths never share trust: a LAN pairing doesn't let a
  PC in over the internet, or the other way round.

### 4.1 Pairing across networks

**Invite code** on the host: *Share this PC › Allow access from other
networks*, then *Show invite code*. The code holds the host's endpoint id,
its relay and a fresh 6-digit PIN: `tb1:<id>:<pin>[:<relay URL>]`. It works
for 10 minutes and 3 tries. The other PC pastes it under *Computers on other
networks › Add with invite code*, dials the host over iroh with ALPN
`remotelinux-pair/1`, and runs the same SPAKE2 PIN exchange as the LAN. The
pairing MACs bind both endpoint ids, which iroh's handshake has already
authenticated.

## 5. Connecting

*Connect* on a PC in *Computers on other networks* dials it over iroh by
endpoint id (with its last relay as a hint), ALPN `remotelinux/1`. The
session protocol is the LAN's. The host runs one iroh endpoint next to its
quinn endpoint, only while access from other networks is on, and lets a
session in only if the peer's endpoint id is in `internet_clients.toml`.

### 5.1 Tuning for the internet

- The LAN's fixed 16 MB congestion window would flood an internet path, so
  iroh connections use the default congestion controller.
- Defaults over the internet: 1080p max, 8 Mbit/s, adaptive bitrate on.
- Video datagrams are already split to `max_datagram_size`, which is
  smaller through a relay; nothing else changes.

### 5.2 Firewall

iroh binds a random UDP port and makes only outgoing connections (relay
over HTTPS, hole punching over UDP), which ufw's default policy allows.
No new inbound rule.

## 6. UI

- *Share this PC* gains *Allow access from other networks* (off by default;
  restarts sharing) and *Invite a computer on another network › Show invite
  code*, which shows the code with a Copy button and closes when the other
  PC pairs.
- A new group, *Computers on other networks*, lists PCs paired over the
  internet with Online/Offline (checked every 20 s through the relay),
  Connect and Forget, and ends with *Add with invite code*.
- *Computers that can control this PC* marks internet pairings *Other
  network*.
- Switch sides stays LAN-only for now; its button is hidden for internet
  connections.
- Later: show *direct* or *through a relay* and the round trip in the
  viewer; a relay field in Preferences (the `relay` key in `host.toml` and
  `client.toml` works now).

## 7. Steps

| Step | Work | Status |
|---|---|---|
| **I1** | `ac_remote::net::Conn`: one connection type over quinn or iroh. | Done |
| **I2** | iroh keys, internet trust files, invite codes, pairing and sessions over iroh; host endpoint; PC tab controls and list. | Done; loopback tests and a live pairing through n0's relay pass |
| **I3** | Field test: laptop on a phone hotspot pairs with and controls this PC. | Next |
| **I4** | Path and round trip in the viewer; relay in Preferences; lower default bitrate over the internet. | |
| **W1–W5** | Windows 11 build (§9). | |
| **F1** | Sending files: **Send file** in the viewer bar, or drop files on the remote screen. One stream per file inside the session (`FileOffer` → `Ready` → bytes → `Saved`), so it works on the LAN and across the internet. Saved to Downloads under a free name; a drop on a Windows Explorer window or the desktop saves there. An older host never answers and the send fails after 10 s. | Done; loopback tested, not yet tried between two PCs. |
| **F2** | The person at the shared PC comes first: any use of its own keyboard, mouse or touchpad drops the remote's input until 1.5 s after they stop; held keys are released and the viewer is told both ways. Windows: low-level hooks, skipping injected input. Linux: reads the physical evdev devices (not our virtual ones); Finish setup adds a udev `uaccess` rule for keyboards, mice, touchpads and touchscreens. | Built; not yet tried on real PCs. |
| **A1** | The phone app (renamed Test Bed, from Wireless Debugging Keeper; APK `test-bed.apk`) controls PCs: **Remote PCs** pairs by PIN on the LAN (mDNS) or by invite code over the internet, shows who is online, and views a PC with `ac-remote`'s own connection (`crates/ac-remote-android`, JNI). Video via MediaCodec, sound via Android's Opus decoder; touch is the mouse (tap, hold, drag, two-finger scroll); a swipe up from the bottom edge opens the keyboard, with a row of PC keys. | Built; LAN pairing, online check, video and the keyboard swipe tried in the emulator against this PC; internet and typing not yet tried. |

## 8. Risks

- **n0's public relays** can rate-limit or change; the relay setting is the
  way out.
- **Both PCs behind carrier-grade or symmetric NAT** means relay-only
  video: it works, but with more delay and lower bitrate.
- **Build size and time**: iroh adds many crates. It's acceptable for the
  share service and the app; the phone crates don't depend on it.

## 9. Windows 11

Goal: a Test Bed download for Windows 11; two PCs running Test Bed, Linux or
Windows, pair and connect in either direction, on a LAN or across networks.

What's already portable: the protocol, pairing, trust, both transports
(quinn, iroh), Opus, and the viewer's networking (`ac-remote`), apart from
small Unix-only helpers (file modes, the control socket).

| Step | Work |
|---|---|
| **W1** | Share service on Windows: screen capture with DXGI Desktop Duplication, encoding with ffmpeg (NVENC, Intel Quick Sync, Media Foundation, else x264 capped at 720p 30 fps to keep the processor cool; AMF later), input with `SendInput` (protocol key codes are Linux evdev: translated to scancodes), audio with WASAPI loopback (cpal), monitor list. Runs in the signed-in session, started at login. UAC prompts and the lock screen stay out of reach (needs a SYSTEM service; later). **Done; type-checks, not yet run on Windows.** |
| **W2** | `ac-remote` on Windows: control socket over a named pipe, file permissions, paths under `%APPDATA%`, starting the share service at login (an HKCU `Run` entry) instead of systemd. **Done; type-checks.** |
| **W3** | Test Bed on Windows: the GTK4/libadwaita app built with MSYS2, the PC tab only (`pc_window.rs`); software video decoding (D3D11VA later); audio playback through WASAPI (cpal); GTK virtual-key codes translated to evdev (`ac_input::pc_keys`). **Done; type-checks.** |
| **W4** | Packaging: `.github/workflows/windows.yml` (MSYS2 CLANG64) builds a zip (later an installer) with the GTK runtime, ffmpeg and the share service, attached to each `v*` release and kept as a run artifact. **Written; not run yet.** |
| **W5** | Test on a real Windows 11 PC: LAN and internet, both directions. |

