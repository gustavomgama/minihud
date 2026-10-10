# HOOKING — in-process present-hook capture backend

This is the **backend** capture/injection foundation for a future on-screen
display. The injected runtime **records frames only**: one `QueryPerformanceCounter`
(QPC) timestamp per present, plus swapchain/back-buffer metadata for a future
OSD. It renders **nothing** — no Direct2D/DirectWrite, no in-swapchain quad, no
overlay window, no minihud UI/config work. The host reads the shared ring and
prints numbers.

Distilled from the research project (`../research`):
`docs/11-rtss-deep-re.md` (injection pipeline, per-API detour set, the explicit
"do not build evasion" rule), `docs/07-verified-facts.md` §1/§1.1 (the VERIFIED
vtable indices), `docs/02-capture-methods.md` (per-API capture), and
`docs/03-anti-cheat-and-safety.md` (risk tiers). `poc/shm_transport.py` is the
seqlock latest-value design reused for the wire format.

## Workspace layout

| Crate | Role |
|---|---|
| `crates/hook-ipc` | Wire format: block layout, seqlock ring, named mapping, trailing-fps math. Unit-tested. |
| `crates/hook-rt` (cdylib) | The injected recorder: own IAT + COM vtable patching, detours for the hook set, `mh_install`/`mh_uninstall`. |
| `crates/hook-vk-layer` (cdylib) | The Vulkan **implicit layer**: wraps `vkQueuePresentKHR` for apps that resolve Vulkan only through `GetProcAddress`. Records into the same ring. |
| `crates/hook-test` | A tiny D3D11/D3D9/D3D12/Vulkan/OpenGL presenter that prints `pid=` and presents N frames. A test target, not an OSD. |
| `src/hook/mod.rs` (minihud) | Host injector + ring reader + `--capture-hook`/`--read-frames` CLI. |

## 1. Hook set

Vtable indices are copied from `research/docs/07-verified-facts.md`
(VERIFIED against the mingw-w64 headers). `crates/hook-rt/src/api.rs` is the
single source of truth; its tests pin every index.

| API | Entry point | Vtable index |
|---|---|---|
| DXGI | `IDXGISwapChain::Present` | **8** |
| DXGI | `IDXGISwapChain1::Present1` | **22** |
| DXGI | `IDXGISwapChain::SetFullscreenState` | 10 (derived: Present=8, GetBuffer=9, …) |
| DXGI | `IDXGISwapChain::ResizeBuffers` | 13 (derived: … GetDesc=12, ResizeBuffers=13) |
| DXGI | `IDXGIFactory::CreateSwapChain` | **10** |
| DXGI | `IDXGIFactory2::CreateSwapChainForHwnd` | **15** |
| DXGI | `IDXGIFactory2::CreateSwapChainForCoreWindow` | **16** |
| DXGI | `IDXGIFactory2::CreateSwapChainForComposition` | **24** |
| D3D9 | `IDirect3DDevice9::Present` | **17** |
| D3D9 | `IDirect3DDevice9::EndScene` | **42** |
| D3D9 | `IDirect3DDevice9Ex::PresentEx` | **121** |
| D3D9 | `IDirect3DDevice9Ex::ResetEx` | **132** |
| D3D9 | `IDirect3D9::CreateDevice` | **16** |
| D3D9 | `IDirect3D9Ex::CreateDeviceEx` | **20** |
| D3D12 | `ID3D12CommandQueue::ExecuteCommandLists` | **10** |

Import-address-table (IAT) hooks, applied to the **application's own import
table** (the app's imports of these exports): `dxgi.dll!CreateDXGIFactory`/`1`/`2`,
`d3d11.dll!D3D11CreateDeviceAndSwapChain`, `d3d9.dll!Direct3DCreate9`/`Ex`,
`opengl32.dll!wglSwapBuffers`/`wglSwapLayerBuffers`, `gdi32.dll!SwapBuffers`,
`libEGL.dll!eglSwapBuffers`, `vulkan-1.dll!vkQueuePresentKHR`/`vkCreateSwapchainKHR`/
`vkGetSwapchainImagesKHR`/`vkCreateDevice`/`vkDestroyDevice`, and the Vulkan
loader entry points `vulkan-1.dll!vkGetInstanceProcAddr`/`vkGetDeviceProcAddr`.

**Vulkan loader proc-addr chain.** Vulkan entry points are almost never imported
directly; an app imports `vkGetInstanceProcAddr` and resolves every other
function by name through it (device-level functions through
`vkGetDeviceProcAddr`). The recorder therefore also IAT-hooks those two exports:
its detour calls the real proc-addr, and when the requested name is one of our
targets (`vkQueuePresentKHR`, `vkCreateSwapchainKHR`, `vkGetSwapchainImagesKHR`,
`vkCreateDevice`, `vkDestroyDevice`, or `vkGetDeviceProcAddr` for the chain)
it records the **real** pointer and returns **our** detour for it, forwarding
the real pointer for every other name. So `vkQueuePresentKHR`, however the app
obtained it, lands on the recorder. The name → target table is the pure
`crates/hook-rt/src/vk.rs::vk_target` (unit-tested); the detours live in
`crates/hook-rt/src/detour.rs`. The static `vulkan-1.dll!vkQueuePresentKHR` IAT
hook is kept as well, for apps that do import it directly.

**Vulkan implicit layer (fully dynamic apps).** An app that never imports the
loader's proc-addr export — it resolves Vulkan with `GetProcAddress` alone
(`ash::Entry::load()` / libloading) — is invisible to the IAT hook. That case is
covered by `crates/hook-vk-layer`, a Khronos **implicit layer**
(`VK_LAYER_MINIHUD_capture`, `type: GLOBAL`) the loader loads before the app
resolves any entry point. The layer:

- negotiates the loader/layer interface version 2
  (`vkNegotiateLoaderLayerInterfaceVersion`), capturing the next chain's
  `vkGetInstanceProcAddr`/`vkGetDeviceProcAddr` from the
  `VkLayerInstanceCreateInfo`/`VkLayerDeviceCreateInfo` link info during
  `vkCreateInstance`/`vkCreateDevice`;
- substitutes its own `vkQueuePresentKHR` for that name in
  `vkGetInstanceProcAddr`/`vkGetDeviceProcAddr`, records one
  `QueryPerformanceCounter` timestamp per present into `minihud-frames-<pid>`
  — creating the ring if absent, exactly like the recorder's standalone path —
  then forwards to the next entity's real present;
- forwards every other entry point untouched.

It is dependency-light: the small Vulkan loader subset is hand-declared, and it
reuses `hook-ipc` for the ring. It is fail-open and panic-free across the FFI
boundary (a panic is caught and the real present forwarded). The pure name
dispatch is `crates/hook-vk-layer/src/dispatch.rs::proc_kind` (unit-tested); the
ring write path is `crates/hook-vk-layer/src/ring.rs` (unit-tested).

**Implicit-layer registration.** The loader discovers implicit layers either
from the registry, or — for non-elevated testing only — from the
`VK_ADD_IMPLICIT_LAYER_PATH` environment variable (a directory containing the
manifest; see §7). Production registration is documented in §4.1 and is **not**
performed by this repo.

**Mechanism.** Own IAT patching (walk the PE import directory, swap the IAT
slot) plus COM vtable-slot patching (swap the slot in the shared class vtable).
No third-party hook library. Vtable patches on extended interfaces
(`IDXGISwapChain1`, `IDXGIFactory2`, `IDirect3DDevice9Ex`, `IDirect3D9Ex`,
`ID3D12CommandQueue`) are gated by a real `QueryInterface`, so a base-interface
vtable is never written past its end.

**D3D12** has no device `Present`: the command queue a swapchain presents from
is the `pDevice` argument of the `IDXGIFactory*CreateSwapChain*` detours; it is
recognised by `QueryInterface` for `ID3D12CommandQueue` and its
`ExecuteCommandLists` slot is patched.

## 2. Injection pipeline

Host side (`src/hook/mod.rs`), mirroring `docs/11` §1.1:

1. `OpenProcess(PROCESS_ALL_ACCESS, pid)`.
2. Refuse WOW64 targets (`IsWow64Process`) — the recorder is 64-bit only.
3. Refuse targets with a known anti-cheat module loaded (§4).
4. `VirtualAllocEx` + `WriteProcessMemory` the recorder DLL path (UTF-16).
5. `CreateRemoteThread(kernel32!LoadLibraryW, path)`.
6. Find the recorder's remote base by module name (Toolhelp) and compute the
   `mh_install` export RVA locally (`LoadLibraryW` + `GetProcAddress`).
7. `CreateRemoteThread(remote_base + rva)` → remote `mh_install`; the thread
   exit code is the installed-API bitmask.

`mh_install`/`mh_uninstall` match `LPTHREAD_START_ROUTINE` and are wrapped in
`catch_unwind` (panic-free across the FFI boundary).

## 3. Wire format (`crates/hook-ipc`)

One named mapping, `minihud-frames-<pid>`:

```text
offset 0     : Header  (48 bytes: magic "MHFR", version, qpc_freq, next_seq,
                          errors, calls, capacity, slot_size, pid)
offset 48    : Status  (32 bytes: installed mask, errors, last_error,
                          rescan gen/count, install attempts)
offset 80    : Ring    (4096 × 40-byte slots)
```

A 40-byte slot is `seq: u64, qpc_start: u64, qpc_stop: u64, swapchain: u64,
flags: u32, api: u16, pad: u16`.

**Seqlock publication.** The writer sets a slot's `seq` odd, writes the payload,
then sets `seq` even; `header.next_seq` is published last. A reader accepts a
slot only when `seq` is even and unchanged across its copy; a torn head falls
back to the previous stable slot (bounded scan), never returning a partial
record. This is `poc/shm_transport.py`'s seqlock applied to a ring.

The host reads the ring and computes trailing fps/frametime from the QPC
timestamps (`hook_ipc::trailing`).

## 4. Safety: tier, consent, exclusions

An in-process present hook is **tier 3** ("inline hooks on the graphics present
path", `docs/03` §2) — detectable by inline checks, module enumeration and
memory scans. Therefore:

- **Opt-in only.** Nothing injects unless `--capture-hook <pid>` is passed.
  The CLI is the explicit consent step; elevation is requested separately.
- **Deny-list.** The injector refuses a target with a known anti-cheat module
  loaded (`EasyAntiCheat`, `BattlEye`/`BEDaisy`/`BEService`, `vanguard`/`vgtray`/
  `vgk`, `Denuvo`/`Irdeto`, `FACEIT`, `ESEA`, `EQU8`, `XIGNCODE`, `nProtect`,
  `GameGuard`).
- **Never a protected title.** If a target is protected, do not inject it.
- **No Stealth/evasion.** `docs/11` §4 excludes relocated/"floating" hook sites
  and any anti-detection technique. This backend deliberately implements **none**:
  hooks are installed at the ordinary vtable/IAT sites.

### 4.1 Production registration of the Vulkan implicit layer

The layer is active only when the loader discovers its manifest. **This repo
does not write the registry.** Production registration (an installer's job) is a
machine-wide value under the Khronos implicit-layer key:

- Key (64-bit apps): `HKLM\SOFTWARE\Khronos\Vulkan\ImplicitLayers`
- Key (32-bit apps): `HKLM\SOFTWARE\WOW6432Node\Khronos\Vulkan\ImplicitLayers`
- Value name: the **absolute** path to `hook_vk_layer.json`
- Value data: `DWORD` `0` (0 = enabled; the value's absence disables the layer)

The manifest's `library_path` (`.\hook_vk_layer.dll`) resolves relative to the
manifest, so the DLL must sit next to the JSON. `cargo xtask dist` stages both
together. Uninstall removes the value and the files.

For **non-elevated testing** (used by the acceptance run), point the loader at
the manifest directory instead — no registry write:

```sh
VK_ADD_IMPLICIT_LAYER_PATH=<dir with hook_vk_layer.json + .dll>  hook-test.exe --api vulkan-dynamic
```

The loader ignores `VK_ADD_IMPLICIT_LAYER_PATH` when the process is elevated, so
test unelevated. `DISABLE_MINIHUD_VK_LAYER=1` disables the layer at runtime
(its manifest `disable_environment`).

## 5. Milestones

- **M1 — capture hooks (this work).** Injection, the hook set, the seqlock ring,
  the host reader, and `--capture-hook`. The recorder draws nothing.
- **M2 — in-swapchain OSD (NOT built).** Rendering the HUD into the game's
  backbuffer during the hooked present, and the OSD text handoff. Explicitly out
  of scope here; this document is the source of truth for that work.

## 6. Known limitations (honest scope)

- **DXGI is the verified path.** End-to-end injection + vtable hooking +
  ring + reader is exercised against `hook-test` (D3D11) and captures frames at
  the presenter's cadence with zero errors.
- **D3D9 already-running devices.** The DXGI class vtable is shared, so patching
  a bootstrap (dummy) swapchain reaches the app's existing swapchain. D3D9 device
  vtables are not reliably shared, so a device created *before* injection may not
  be reached; a device created *after* injection (via the hooked `Direct3DCreate9`
  /`CreateDevice`) is patched.
- **OpenGL delay-load.** `SwapBuffers` is delay-loaded in the test binary (it is
  not in the regular import directory), so a pure IAT hook cannot intercept it
  before first call. GL capture therefore needs the app to import the swap
  function normally, or a future wrapper/`wglSwapBuffers` hook.
- **Only the main module's IAT is patched.** Patching a *graphics DLL's own*
  import table was observed to destabilise the target and is deliberately
  excluded; the spec's "IAT hooks on `dxgi.dll` CreateDXGIFactory" means hooking
  the app's imports *of* those exports.
- **Vulkan.** Natively-linked apps are covered: `hook-test` statically imports
  `vulkan-1.dll!vkGetInstanceProcAddr` (`#[link(name = "vulkan-1", kind =
  "raw-dylib")]`) and builds `ash::Entry::from_static_fn`, so the recorder's IAT
  hook on the loader's proc-addr export intercepts every subsequent lookup and
  the present records as `vk.queuepresent`. A Vulkan present can also produce a
  nested `dxgi.present1` record when the ICD presents the surface through DXGI;
  the host prints the per-API breakdown so both are visible.
- **Vulkan fully-dynamic apps — implicit layer (built).** An app that resolves
  Vulkan **only** via `GetProcAddress` (never statically importing the loader's
  proc-addr) is captured by the `crates/hook-vk-layer` implicit layer, not by
  IAT patching. Verified live: `hook-test --api vulkan-dynamic` (a
  `ash::Entry::load()` presenter) with no injection, observed read-only via
  `minihud --read-frames <pid>` as `api vk.queuepresent` at the presenter's
  cadence, 0 errors. Registration is documented in §4.1.
- **RTSS conflict (environmental).** RivaTuner Statistics Server
  (`RTSSHooks64.dll`) installs a present hook that faults (`movaps`, access
  violation) whenever a *custom present-wrapping Vulkan layer* sits above it —
  its hook is not robust to an extra layer. This is RTSS's bug, not the
  layer's: the same target captured fine with the layer's present
  interception disabled. The `hook-test` **validation target** opts out of RTSS
  via RTSS's own sanctioned mechanism (it exports `RTSSHooksCompatibility`, see
  `crates/hook-test/build.rs`), which isolates the test from the overlay. A real
  target that must coexist with RTSS needs its own RTSS exclusion
  (`[Hooking] EnableHooking=0` profile, or the same export). The production
  layer does nothing about this; document it for end users.
- **One writer per ring (guarded).** The layer and the injected recorder both
  target `minihud-frames-<pid>`; two writers on one ring **double-count** every
  present (each present is published once by each writer). The layer **defers**:
  on each present it reads the recorder's installed-API mask from the shared
  status block (`RingWriter::installed`) and publishes nothing while that mask is
  non-zero, so an injected recorder stays the single writer. A layer-only process
  (fully-dynamic Vulkan, no injection) still sees mask `0` and records normally.
  Prefer the layer for fully-dynamic apps and injection for the rest; if the
  recorder installs *after* the layer has already published, a few early presents
  can still be counted twice before the layer defers.

## 7. Build & run

```sh
cargo build -p hook-rt -p hook-test -p minihud
# start the presenter, note its pid
target/debug/hook-test.exe --api d3d11 --frames 2000
# in another shell (elevated for cross-process access), capture for 5s
target/debug/minihud.exe --capture-hook <pid> 5

# Vulkan implicit layer (fully-dynamic app), no injection:
VK_ADD_IMPLICIT_LAYER_PATH=target/debug target/debug/hook-test.exe --api vulkan-dynamic --frames 2000
# observe the ring the layer created, read-only, no elevation:
MINIHUD_NO_ELEVATE=1 target/debug/minihud.exe --read-frames <pid> 4
```

The recorder DLL must sit next to `minihud.exe` (`hook_rt.dll`, the cargo
artifact name; a hyphenated spelling is also accepted). The layer's DLL and
manifest must sit together in the `VK_ADD_IMPLICIT_LAYER_PATH` directory
(`hook_vk_layer.dll` + `hook_vk_layer.json`); `cargo xtask build` stages both
into `target/<profile>/` and `cargo xtask dist` into `dist/`.

### 7.1 Live-injection coverage (on demand)

The deterministic gate (`tools/audit.ps1`) runs unit tests only, so the host
functions that need a real injected target (`inject`, `unhook`,
`launch_and_inject`, `launch_capture`, `capture_hook`, `read_frames`) stay at
0% and are hidden by the CRAP gate's `--allow` list. To prove they execute and
measure their real hit counts on a machine with a GPU:

```powershell
powershell -ExecutionPolicy Bypass -File tools/coverage-live.ps1
```

It builds `hook-test` + `hook_rt`, builds the host instrumented with
`cargo llvm-cov run --no-report` (accumulating `.profraw` across runs), copies
`hook_rt.dll` next to the instrumented exe, runs `--launch` (d3d11 + d3d9),
`--capture-hook`, `--read-frames`, and a brief `--follow`, then prints the
`src/hook` coverage summary and a per-function hit table. Not part of CI (no
GPU there); see `audit.md` "Live coverage" for the recorded numbers.

