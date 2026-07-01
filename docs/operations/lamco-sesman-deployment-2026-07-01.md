# Lamco sesman deployment notes — 2026-07-01

## Summary

This change set moves the live `rui` RDP desktop from an ad-hoc shell-managed process tree to a Lamco-native session manager and records the runtime fixes needed for the nested Weston + niri + Lamco stack.

The deployed runtime shape is now:

```text
niri-rui.service
  └─ dbus-run-session
      └─ lamco-sesman run --restart
          ├─ weston --backend=headless --socket=wayland-weston --width=1920 --height=1080
          ├─ /usr/local/bin/niri
          └─ /usr/bin/lamco-rdp-server -c /home/rui/.config/lamco-rdp-server/config.toml
```

## Files and runtime paths

- Session manager binary: `/usr/bin/lamco-sesman`
- Session manager config: `/home/rui/.config/lamco-rdp-server/sesman.toml`
- Thin systemd wrapper: `/usr/local/bin/lamco-rdp-session-inner`
- Systemd unit: `niri-rui.service`
- Session registry: `/run/user/1000/lamco-sesman/rui.state.json`
- Logs:
  - `/tmp/weston-headless.log`
  - `/tmp/niri-nested.log`
  - `/tmp/lamco-rdp-server.log`

## Session manager behavior

`lamco-sesman` provides the xrdp-sesman-like pieces we need without reusing xrdp internals:

- component lifecycle for Weston, niri, and Lamco RDP;
- readiness checks for Wayland sockets and Lamco process liveness;
- stale socket/log cleanup, including transient `niri.wayland-1.*.sock` paths;
- stale lock detection;
- JSON status for automation;
- foreground `run` mode suitable for systemd;
- reusable session state and reconnect metadata.

## Runtime fixes included

### Direct-capture geometry and input mapping

Portal-generic/direct capture can now accept the client RDP desktop size even when the capture stream remains at the compositor output size. The display handler tracks RDP desktop size separately from capture stream size and updates input mapping as:

```text
RDP client coordinate space -> Wayland capture stream coordinate space
```

This is required for clients that request desktop sizes such as `1512x949` while the nested compositor is still capturing `1920x1080`.

### niri output resize command

A mobile client reported protocol error `0x200d` after Lamco attempted a temporary niri custom output mode with a mode string like:

```text
1512x949
```

Current niri requires an explicit refresh rate for custom modes. The helper now sends:

```text
1512x949@60.000
```

and logs stderr if niri rejects the mode. This removed the observed `refresh rate is required for custom modes` failure in the Lamco log.

### Pointer/cursor handling

Normal clients receive `DefaultPointer` instead of the Android-specific vertically flipped RGBA pointer. Android-specific pointer updates remain gated by EGFX/client negotiation. Portal-generic capture also requests hidden compositor cursor mode to avoid double pointers.

### EGFX negotiation model

EGFX capability handling now stores a single explicit negotiated mode:

- `Avc420`
- `Avc444`
- `Planar`

The display pipeline consumes this explicit mode instead of re-deriving behavior from scattered booleans. This makes Android/AVC-disabled clients and H.264 clients easier to reason about.

### VA-API H.264 low-power entrypoint

VA-API H.264 probing now accepts `VAEntrypointEncSliceLP` when `VAEntrypointEncSlice` is absent. This matches common Intel iHD/container environments where only the low-power encode entrypoint is advertised.

## Verification performed

On `root@192.168.1.67:/root/lamco-src-work`:

```text
cargo +stable fmt --check
cargo +stable check --bin lamco-rdp-server
cargo +stable build --release --bin lamco-rdp-server
```

The rebuilt server was installed to `/usr/bin/lamco-rdp-server` and `niri-rui.service` was restarted.

Post-restart checks:

- `niri-rui.service` active;
- `lamco-sesman --json status` reported `Healthy`;
- TCP `0.0.0.0:3389` was owned by the sesman-managed `lamco-rdp-server` process;
- stale pre-sesman/orphan monitor processes were removed;
- recent Lamco logs no longer showed the niri custom-mode refresh-rate error.

## Operational notes

- The current Lamco host uses Rust through rustup, not `/usr/bin/rustc`:

```bash
export PATH=/root/.cargo/bin:$PATH
export CARGO_HOME=/root/.cargo
export RUSTUP_HOME=/home/agent/.rustup
cargo +stable check --bin lamco-rdp-server
```

- For live diagnostics:

```bash
sudo -u rui XDG_RUNTIME_DIR=/run/user/1000 \
  /usr/bin/lamco-sesman \
  --config /home/rui/.config/lamco-rdp-server/sesman.toml \
  --json status

ss -ltnp | awk '/:3389/{print}'
tail -n 200 /tmp/lamco-rdp-server.log
```
