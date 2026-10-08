# Phase 2: Legacy D3D9 + OpenGL hooks
Status: STUB — spec developed, code deferred (RTSS blocker on Phase 0/1)

Design (from §1-15 spec):
- D3D9: hook IDirect3DDevice9::Present via vtable wrap (same pattern as DXGI)
- OpenGL: hook wglSwapBuffers / glXSwapBuffers via module export scan
- Per-swapchain tracking: reuse ipc::Slot format (QPC + swapchain ptr + flags)
- Filter: TEST filter applies (only count when process matches --process)
- AC stance: default-deny; informed consent required

Blocker: RTSSHooks64.dll rewrites executable bytes continuously; vtable-wrap avoids this but factory hook needs clean env to validate.
