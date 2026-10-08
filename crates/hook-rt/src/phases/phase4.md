# Phase 4: Hardening (AC deny, fail-open, attach, x86)
Status: STUB — spec developed, code deferred

Design:
- AC deny: module snapshot (CreateToolhelp32Snapshot) — already in hook-host; extend to hook-rt (refuse install if AC modules present)
- Fail-open: if hook fails, resume game unhooked (hook-host already does this)
- Attach: explicit opt-in only (hook-host cmd_attach); bounded 10s wait
- x86: 32-bit injector (separate binary or WOW64 path); hook-rt must compile for x86
- Security: no stealth; informed consent; log loudly

Blocker: RTSS; x86 build needs separate toolchain check.
