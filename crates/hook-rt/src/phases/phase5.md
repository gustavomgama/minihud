# Phase 5: Additive HUD integration
Status: STUB — spec developed, code deferred

Design:
- HUD reads hook-ipc ring (same SPSC ring, version 2)
- Per-app FPS/frametime/graph rows added to existing LHM overlay
- Config merge: minihud.sample.toml defines per-process settings
- Drag + persist: existing overlay features; hook data feeds into them
- No ETW/appstats: hook is the only per-app source (as per user requirement)

Blocker: RTSS; needs Phase 0/1 validation first.
