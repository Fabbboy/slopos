//! Protocol bridge: server-side compositor protocol over AF_UNIX sockets.
//!
//! Listens on `/run/compositor`, accepts client connections, and translates
//! typed protocol requests into local surface state.

use core::num::NonZeroU32;

use slopos_abi::damage::DamageRect;
use slopos_abi::window::{AppId, MAX_WINDOW_DAMAGE_REGIONS, WindowInfo};
use slopos_protocol::server::{MAX_CLIENTS, Server};
use slopos_protocol::types::{
    Event, MAX_STRING_LEN, OwnedFd, PROTOCOL_VERSION, ProtocolError, Request, SurfaceId,
    ToplevelId, caps,
};

use crate::syscall::CachedShmMapping;
use crate::syscall::tty;

/// Hard ceiling on a single clipboard payload (16 MiB). Caps a hostile or
/// runaway selection size so a copy cannot exhaust memory.
const MAX_CLIPBOARD_BYTES: u32 = 16 * 1024 * 1024;

const MAX_SURFACES: usize = 32;
const MAX_PENDING_DAMAGE: usize = 8;
const MAX_CHILDREN: usize = 8;

/// Surface role assigned via get_toplevel.
#[derive(Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
#[allow(dead_code)]
enum SurfaceRole {
    None = 0,
    Toplevel = 1,
    Popup = 2,
    Subsurface = 3,
}

/// Per-surface state managed by the protocol bridge.
#[allow(dead_code)]
struct ProtocolSurface {
    active: bool,
    client_idx: usize,
    surface_id: SurfaceId,
    toplevel_id: ToplevelId,
    shm_token: u32,
    width: u32,
    height: u32,
    frame_width: u32,
    frame_height: u32,
    // Damage tracking
    pending_damage: [DamageRect; MAX_PENDING_DAMAGE],
    pending_damage_count: u8,
    committed_damage: [DamageRect; MAX_PENDING_DAMAGE],
    committed_damage_count: u8,
    dirty: bool,
    // Window state
    window_x: i32,
    window_y: i32,
    z_order: u32,
    visible: bool,
    window_state: u8,
    // Frame callback
    frame_callback_pending: bool,
    last_present_time_ms: u64,
    // Role & hierarchy
    role: SurfaceRole,
    parent_surface_idx: Option<usize>,
    children: [Option<usize>; MAX_CHILDREN],
    child_count: u8,
    relative_x: i32,
    relative_y: i32,
    // Configure ack tracking
    acked_serial: u32,
    // Metadata
    title: [u8; MAX_STRING_LEN],
    app_id: [u8; MAX_STRING_LEN],
    cursor_shape: u8,
}

impl ProtocolSurface {
    const fn empty() -> Self {
        Self {
            active: false,
            client_idx: 0,
            surface_id: SurfaceId::NONE,
            toplevel_id: ToplevelId::NONE,
            shm_token: 0,
            width: 0,
            height: 0,
            frame_width: 0,
            frame_height: 0,
            pending_damage: [DamageRect::invalid(); MAX_PENDING_DAMAGE],
            pending_damage_count: 0,
            committed_damage: [DamageRect::invalid(); MAX_PENDING_DAMAGE],
            committed_damage_count: 0,
            dirty: false,
            window_x: 100,
            window_y: 100,
            z_order: 0,
            visible: false,
            window_state: 0,
            frame_callback_pending: false,
            last_present_time_ms: 0,
            role: SurfaceRole::None,
            parent_surface_idx: None,
            children: [None; MAX_CHILDREN],
            child_count: 0,
            relative_x: 0,
            relative_y: 0,
            acked_serial: 0,
            title: [0u8; MAX_STRING_LEN],
            app_id: [0u8; MAX_STRING_LEN],
            cursor_shape: 0,
        }
    }
}

/// Clipboard state shared across all clients: a read-only mapping of the
/// source memfd received on copy (owns the fd; dropped when replaced) plus the
/// valid byte count. `None` source means the clipboard is empty.
struct Clipboard {
    source: Option<CachedShmMapping>,
    len: u32,
}

/// Protocol bridge: translates wire protocol messages into local surface state.
pub struct ProtocolBridge {
    server: Server,
    surfaces: [ProtocolSurface; MAX_SURFACES],
    next_z_order: u32,
    clipboard: Clipboard,
    /// Display dimensions passed to new clients on accept.
    display_width: u32,
    display_height: u32,
    display_format: u32,
    display_pitch: u32,
    /// Monotonic serial counter for configure events.
    configure_serial: u32,
    /// Per-slot connection generation. Bumped each time a slot is (re)used by
    /// `accept_and_collect`. Disambiguates a slot+fd that the kernel/Server
    /// recycle for a successor client from the original owner, so a stale
    /// per-client task cannot drive or tear down the wrong connection.
    client_gen: [u64; MAX_CLIENTS],
    /// Monotonic source for `client_gen` values (never reused).
    next_gen: u64,
}

impl ProtocolBridge {
    /// Create a new bridge.
    ///
    /// The compositor creates its OWN listen socket via `Server::bind()`.
    /// After binding, it signals readiness to init by writing to fd 3
    /// (the readiness pipe inherited from init, xinit/weston/s6 pattern).
    /// Init blocks on the read end until this signal arrives, then spawns
    /// client apps whose connect() succeeds immediately via kernel backlog.
    pub fn new() -> Option<Self> {
        let server = Server::bind(b"/run/compositor").ok()?;
        tty::write(b"COMPOSITOR: protocol bridge listening on /run/compositor\n");

        Some(Self {
            server,
            surfaces: [const { ProtocolSurface::empty() }; MAX_SURFACES],
            next_z_order: 1,
            clipboard: Clipboard {
                source: None,
                len: 0,
            },
            display_width: 0,
            display_height: 0,
            display_format: 0,
            display_pitch: 0,
            configure_serial: 0,
            client_gen: [0u64; MAX_CLIENTS],
            next_gen: 1,
        })
    }

    /// Set display dimensions (called by compositor after framebuffer init).
    pub fn set_display_info(&mut self, width: u32, height: u32, format: u32, pitch: u32) {
        self.display_width = width;
        self.display_height = height;
        self.display_format = format;
        self.display_pitch = pitch;
    }

    /// Queue the per-accept greeting (Hello + OutputInfo) to a freshly
    /// accepted client. Shared by the synchronous and async accept paths.
    fn greet_client(&mut self, idx: usize) {
        let _ = self.server.queue_event(
            idx,
            &Event::Hello {
                version: PROTOCOL_VERSION,
                capabilities: caps::TOPLEVEL | caps::CLIPBOARD | caps::INTERACTIVE_MOVE_RESIZE,
            },
        );
        let _ = self.server.queue_event(
            idx,
            &Event::OutputInfo {
                width: self.display_width,
                height: self.display_height,
                format: self.display_format,
                pitch: self.display_pitch,
                scale: 1,
            },
        );
    }

    /// Non-blocking accept loop for new clients.
    pub fn accept_clients(&mut self) {
        // Accept up to 4 connections per frame to avoid stalling the loop
        for _ in 0..4 {
            match self.server.accept() {
                Ok(Some(idx)) => self.greet_client(idx),
                Ok(None) => break,
                Err(_) => break,
            }
        }
    }

    /// The listening socket's FD — used to arm an async accept-readiness
    /// stream (`poll_add_multishot(listen_fd, POLLIN)`).
    pub fn listen_fd(&self) -> i32 {
        self.server.listen_fd()
    }

    /// A connected client's socket FD, or `None` if the slot is empty —
    /// used to arm a per-client readiness stream
    /// (`poll_add_multishot(client_fd, POLLIN)`).
    pub fn client_fd(&self, idx: usize) -> Option<i32> {
        self.server
            .clients
            .get(idx)
            .and_then(|slot| slot.as_ref().filter(|c| c.active).map(|c| c.conn.fd()))
    }

    /// A connected client slot's current generation, or `None` if the slot is
    /// empty. Paired with [`client_fd`] it uniquely identifies one connection:
    /// the kernel/Server recycle slot indices *and* fd numbers across
    /// disconnect→reconnect, but `client_gen` is monotonic and never reused, so
    /// a successor occupying the same slot+fd always carries a different value.
    pub fn client_gen(&self, idx: usize) -> Option<u64> {
        if self.server.is_connected(idx) {
            self.client_gen.get(idx).copied()
        } else {
            None
        }
    }

    /// Async accept path: drain every pending connection (greeting each like
    /// [`accept_clients`]) and record the new `(client_idx, client_fd,
    /// client_gen)` triples into `out`, returning the count. The caller spawns
    /// a per-client task per returned triple. Unlike [`accept_clients`] this
    /// drains the full backlog (it is driven by listen-socket readiness, not a
    /// per-frame budget) so a burst of connects is serviced in one wake.
    ///
    /// Each new client is stamped with a fresh generation here, atomically with
    /// the accept (no `await` between), so the triple the caller captures is
    /// the identity of *this* connection for the lifetime of its task.
    pub fn accept_and_collect(&mut self, out: &mut [(usize, i32, u64)]) -> usize {
        let mut n = 0;
        while n < out.len() {
            match self.server.accept() {
                Ok(Some(idx)) => {
                    self.greet_client(idx);
                    let generation = self.next_gen;
                    self.next_gen = self.next_gen.wrapping_add(1);
                    if idx < MAX_CLIENTS {
                        self.client_gen[idx] = generation;
                    }
                    let fd = self.client_fd(idx).unwrap_or(-1);
                    out[n] = (idx, fd, generation);
                    n += 1;
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
        n
    }

    /// Process all pending requests from all clients.
    pub fn process_requests(&mut self) {
        for client_idx in 0..32 {
            self.process_client(client_idx);
        }
    }

    /// Drain one client's pending requests (the per-client async path calls
    /// this on each readiness yield). Returns `true` while the client stays
    /// connected; `false` once it disconnects — at which point the client is
    /// already torn down via [`cleanup_client`], so the caller's per-client
    /// task should exit (dropping its readiness stream → `OP_CANCEL`).
    pub fn process_client(&mut self, client_idx: usize) -> bool {
        if !self.server.is_connected(client_idx) {
            return false;
        }
        loop {
            match self.server.recv_request(client_idx) {
                Ok(Some(req)) => self.handle_request(client_idx, req),
                Ok(None) => break,
                Err(ProtocolError::Disconnected) => {
                    self.cleanup_client(client_idx);
                    return false;
                }
                Err(_) => break,
            }
        }
        true
    }

    fn handle_request(&mut self, client_idx: usize, req: Request) {
        match req {
            Request::Hello { .. } => {
                // Client echoing our Hello -- already accepted, ignore.
            }
            Request::CreateSurface { new_id } => {
                self.handle_create_surface(client_idx, new_id);
            }
            Request::SurfaceAttach {
                surface,
                shm_token: _,
                width,
                height,
                buffer_fd,
            } => {
                // The memfd fd is now decoded inline as part of the Request.
                if let Some(fd) = buffer_fd {
                    self.handle_surface_attach(
                        client_idx,
                        surface,
                        fd.into_raw() as u32,
                        width,
                        height,
                    );
                }
            }
            Request::SurfaceDamage {
                surface,
                x,
                y,
                w,
                h,
            } => {
                self.handle_surface_damage(client_idx, surface, x, y, w, h);
            }
            Request::SurfaceCommit { surface } => {
                self.handle_surface_commit(client_idx, surface);
            }
            Request::SurfaceFrame { surface } => {
                self.handle_surface_frame(client_idx, surface);
            }
            Request::SurfaceDestroy { surface } => {
                self.handle_surface_destroy(client_idx, surface);
            }
            Request::GetToplevel { surface, new_id } => {
                self.handle_get_toplevel(client_idx, surface, new_id);
            }
            Request::ToplevelSetTitle {
                toplevel,
                title,
                len,
            } => {
                self.handle_toplevel_set_title(client_idx, toplevel, &title, len as usize);
            }
            Request::ToplevelSetAppId {
                toplevel,
                app_id,
                len,
            } => {
                self.handle_toplevel_set_app_id(client_idx, toplevel, &app_id, len as usize);
            }
            Request::ToplevelDestroy { toplevel } => {
                self.handle_toplevel_destroy(client_idx, toplevel);
            }
            Request::AckConfigure { serial } => {
                self.handle_ack_configure(client_idx, serial);
            }
            Request::SetCursorShape { surface, shape } => {
                self.handle_set_cursor_shape(client_idx, surface, shape as u32);
            }
            Request::InteractiveMove { toplevel, serial } => {
                self.handle_interactive_move(client_idx, toplevel, serial);
            }
            Request::InteractiveResize {
                toplevel,
                serial,
                edges,
            } => {
                self.handle_interactive_resize(client_idx, toplevel, serial, edges);
            }
            Request::ClipboardCopy { len, buffer_fd } => {
                self.handle_clipboard_copy(buffer_fd, len);
            }
            Request::ClipboardPaste => {
                self.handle_clipboard_paste(client_idx);
            }
            Request::ClipboardRead { len, buffer_fd } => {
                self.handle_clipboard_read(client_idx, buffer_fd, len);
            }
        }
    }

    // ── Surface lifecycle ──────────────────────────────────────────────────

    fn handle_create_surface(&mut self, client_idx: usize, new_id: SurfaceId) {
        // Reject zero IDs (used as sentinel) and duplicates within this client.
        if new_id == SurfaceId::NONE || self.find_surface(client_idx, new_id).is_some() {
            let _ = self.server.queue_event(
                client_idx,
                &Event::Error {
                    object_id: 0,
                    code: 2,
                },
            );
            return;
        }

        let slot = match self.surfaces.iter().position(|s| !s.active) {
            Some(idx) => idx,
            None => {
                let _ = self.server.queue_event(
                    client_idx,
                    &Event::Error {
                        object_id: 0,
                        code: 1,
                    },
                );
                return;
            }
        };

        self.surfaces[slot] = ProtocolSurface::empty();
        self.surfaces[slot].active = true;
        self.surfaces[slot].client_idx = client_idx;
        self.surfaces[slot].surface_id = new_id;
        self.surfaces[slot].z_order = self.next_z_order;
        self.next_z_order = self.next_z_order.wrapping_add(1).max(1);
    }

    fn handle_surface_attach(
        &mut self,
        client_idx: usize,
        surface_id: SurfaceId,
        shm_token: u32,
        width: u32,
        height: u32,
    ) {
        if let Some(idx) = self.find_surface(client_idx, surface_id) {
            self.surfaces[idx].shm_token = shm_token;
            self.surfaces[idx].width = width;
            self.surfaces[idx].height = height;
        }
    }

    fn handle_surface_damage(
        &mut self,
        client_idx: usize,
        surface_id: SurfaceId,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) {
        if let Some(idx) = self.find_surface(client_idx, surface_id) {
            let surface = &mut self.surfaces[idx];
            let di = surface.pending_damage_count as usize;
            if di < MAX_PENDING_DAMAGE {
                surface.pending_damage[di] = DamageRect {
                    x0: x,
                    y0: y,
                    x1: x + w - 1,
                    y1: y + h - 1,
                };
                surface.pending_damage_count += 1;
            } else {
                // Too many damage rects — collapse to full-surface damage.
                // This matches the Wayland compositor pattern: when precise
                // tracking is exhausted, fall back to repainting everything.
                surface.pending_damage[0] = DamageRect {
                    x0: 0,
                    y0: 0,
                    x1: surface.width.saturating_sub(1) as i32,
                    y1: surface.height.saturating_sub(1) as i32,
                };
                surface.pending_damage_count = 1;
            }
        }
    }

    fn handle_surface_commit(&mut self, client_idx: usize, surface_id: SurfaceId) {
        if let Some(idx) = self.find_surface(client_idx, surface_id) {
            let surface = &mut self.surfaces[idx];
            surface.committed_damage = surface.pending_damage;
            surface.committed_damage_count = surface.pending_damage_count;
            surface.pending_damage = [DamageRect::invalid(); MAX_PENDING_DAMAGE];
            surface.pending_damage_count = 0;
            surface.dirty = true;
            if surface.shm_token != 0 && surface.width > 0 && surface.height > 0 {
                surface.visible = true;
            }
        }
    }

    fn handle_surface_frame(&mut self, client_idx: usize, surface_id: SurfaceId) {
        if let Some(idx) = self.find_surface(client_idx, surface_id) {
            self.surfaces[idx].frame_callback_pending = true;
        }
    }

    fn handle_surface_destroy(&mut self, client_idx: usize, surface_id: SurfaceId) {
        if let Some(idx) = self.find_surface(client_idx, surface_id) {
            self.destroy_surface(idx);
        }
    }

    fn handle_set_cursor_shape(&mut self, client_idx: usize, surface_id: SurfaceId, shape: u32) {
        if let Some(idx) = self.find_surface(client_idx, surface_id) {
            self.surfaces[idx].cursor_shape = shape as u8;
        }
    }

    // ── Toplevel ───────────────────────────────────────────────────────────

    fn handle_get_toplevel(
        &mut self,
        client_idx: usize,
        surface_id: SurfaceId,
        new_id: ToplevelId,
    ) {
        let idx = match self.find_surface(client_idx, surface_id) {
            Some(i) => i,
            None => return,
        };

        // A surface can only have one role (Wayland protocol requirement).
        if self.surfaces[idx].role != SurfaceRole::None {
            return;
        }

        self.surfaces[idx].role = SurfaceRole::Toplevel;
        self.surfaces[idx].toplevel_id = new_id;
    }

    fn handle_toplevel_set_title(
        &mut self,
        client_idx: usize,
        toplevel_id: ToplevelId,
        title: &[u8; MAX_STRING_LEN],
        title_len: usize,
    ) {
        if let Some(idx) = self.find_surface_by_toplevel(client_idx, toplevel_id) {
            let copy_len = title_len.min(MAX_STRING_LEN);
            self.surfaces[idx].title[..copy_len].copy_from_slice(&title[..copy_len]);
            if copy_len < MAX_STRING_LEN {
                self.surfaces[idx].title[copy_len..].fill(0);
            }
        }
    }

    fn handle_toplevel_set_app_id(
        &mut self,
        client_idx: usize,
        toplevel_id: ToplevelId,
        app_id: &[u8; MAX_STRING_LEN],
        app_id_len: usize,
    ) {
        if let Some(idx) = self.find_surface_by_toplevel(client_idx, toplevel_id) {
            let copy_len = app_id_len.min(MAX_STRING_LEN);
            self.surfaces[idx].app_id[..copy_len].copy_from_slice(&app_id[..copy_len]);
            if copy_len < MAX_STRING_LEN {
                self.surfaces[idx].app_id[copy_len..].fill(0);
            }
        }
    }

    fn handle_toplevel_destroy(&mut self, client_idx: usize, toplevel_id: ToplevelId) {
        if let Some(idx) = self.find_surface_by_toplevel(client_idx, toplevel_id) {
            self.surfaces[idx].role = SurfaceRole::None;
            self.surfaces[idx].toplevel_id = ToplevelId::NONE;
        }
    }

    // ── Configure ack ──────────────────────────────────────────────────────

    fn handle_ack_configure(&mut self, client_idx: usize, serial: u32) {
        // Record the acked serial on any surface belonging to this client.
        for s in &mut self.surfaces {
            if s.active && s.client_idx == client_idx && s.role == SurfaceRole::Toplevel {
                s.acked_serial = serial;
            }
        }
    }

    // ── Interactive move/resize ───────────────────────────────────────────

    fn handle_interactive_move(
        &mut self,
        client_idx: usize,
        toplevel_id: ToplevelId,
        _serial: u32,
    ) {
        // Client-initiated interactive move -- currently a no-op.
        // The compositor drives moves via title-bar drag; log for future use.
        let _ = (client_idx, toplevel_id);
    }

    fn handle_interactive_resize(
        &mut self,
        client_idx: usize,
        toplevel_id: ToplevelId,
        _serial: u32,
        _edges: u32,
    ) {
        // Client-initiated interactive resize -- currently a no-op.
        let _ = (client_idx, toplevel_id);
    }

    // ── Clipboard ──────────────────────────────────────────────────────────

    /// Publish a new clipboard: map the received source memfd read-only and
    /// keep it (replacing — and so closing — any previous source). The mapping
    /// keeps the memfd backing alive after the client closes its own copy.
    fn handle_clipboard_copy(&mut self, buffer_fd: Option<OwnedFd>, len: u32) {
        let Some(fd) = buffer_fd else { return };
        let len = len.min(MAX_CLIPBOARD_BYTES);
        if len == 0 {
            // An empty copy clears the clipboard.
            self.clipboard.source = None;
            self.clipboard.len = 0;
            return;
        }
        // `into_raw` releases the OwnedFd's close-on-drop; on a successful map
        // the mapping owns the fd, but on failure we must close it ourselves.
        let raw = fd.into_raw();
        match CachedShmMapping::map_readonly_fd(raw, len as usize) {
            Some(mapping) => {
                self.clipboard.source = Some(mapping);
                self.clipboard.len = len;
            }
            None => {
                crate::syscall::memory::close(raw);
                self.clipboard.source = None;
                self.clipboard.len = 0;
            }
        }
    }

    /// Announce the current clipboard size; the client follows up with a
    /// `ClipboardRead` carrying a destination memfd (the server event path
    /// cannot itself carry an fd).
    fn handle_clipboard_paste(&mut self, client_idx: usize) {
        let _ = self.server.queue_event(
            client_idx,
            &Event::PasteReady {
                len: self.clipboard.len,
            },
        );
    }

    /// Copy the clipboard into the client-provided destination memfd, then tell
    /// the client how many bytes are valid. The source mapping is retained so
    /// the clipboard survives repeated pastes.
    fn handle_clipboard_read(
        &mut self,
        client_idx: usize,
        buffer_fd: Option<OwnedFd>,
        dst_len: u32,
    ) {
        let Some(fd) = buffer_fd else { return };
        let dst_len = dst_len.min(MAX_CLIPBOARD_BYTES) as usize;
        let raw = fd.into_raw();
        let mut copied = 0u32;
        match CachedShmMapping::map_writable_fd(raw, dst_len) {
            Some(mut dst) => {
                if let Some(src) = self.clipboard.source.as_ref() {
                    let n = (self.clipboard.len as usize).min(dst_len);
                    dst.as_mut_slice()[..n].copy_from_slice(&src.as_slice()[..n]);
                    copied = n as u32;
                }
                // `dst` (and its fd) is released here.
            }
            None => {
                crate::syscall::memory::close(raw);
            }
        }
        let _ = self
            .server
            .queue_event(client_idx, &Event::PasteResult { len: copied });
    }

    // ── Frame callbacks ────────────────────────────────────────────────────

    /// Send frame_done events to all surfaces with pending frame callbacks.
    pub fn mark_frames_done(&mut self, timestamp_ms: u64) {
        for i in 0..MAX_SURFACES {
            let surface = &mut self.surfaces[i];
            if surface.active && surface.frame_callback_pending && surface.visible {
                surface.frame_callback_pending = false;
                surface.last_present_time_ms = timestamp_ms;

                let _ = self.server.queue_event(
                    surface.client_idx,
                    &Event::FrameDone {
                        surface: surface.surface_id,
                        timestamp_ms: timestamp_ms as u32,
                    },
                );
            }
        }
    }

    // ── Outgoing events (called by compositor input/WM code) ───────────────

    /// Send toplevel configure event to a protocol surface.
    pub fn send_configure(&mut self, surface_idx: usize, width: u32, height: u32, states: u32) {
        let surface = match self.surfaces.get(surface_idx) {
            Some(s) if s.active && s.toplevel_id != ToplevelId::NONE => s,
            _ => return,
        };

        self.configure_serial = self.configure_serial.wrapping_add(1);
        let serial = self.configure_serial;
        let client_idx = surface.client_idx;
        let toplevel = surface.toplevel_id;
        let _ = self.server.queue_event(
            client_idx,
            &Event::Configure {
                toplevel,
                serial,
                width,
                height,
                states,
            },
        );
    }

    /// Send toplevel close event to a protocol surface.
    pub fn send_close(&mut self, surface_idx: usize) {
        let surface = match self.surfaces.get(surface_idx) {
            Some(s) if s.active && s.toplevel_id != ToplevelId::NONE => s,
            _ => return,
        };

        let client_idx = surface.client_idx;
        let toplevel = surface.toplevel_id;
        let _ = self
            .server
            .queue_event(client_idx, &Event::Close { toplevel });
    }

    /// Send pointer enter event.
    pub fn send_pointer_enter(&mut self, surface_idx: usize, _serial: u32, x: i32, y: i32) {
        let surface = match self.surfaces.get(surface_idx) {
            Some(s) if s.active => s,
            _ => return,
        };

        let client_idx = surface.client_idx;
        let surface_id = surface.surface_id;
        let _ = self.server.queue_event(
            client_idx,
            &Event::PointerEnter {
                surface: surface_id,
                x,
                y,
            },
        );
    }

    /// Send pointer leave event.
    pub fn send_pointer_leave(&mut self, surface_idx: usize, _serial: u32) {
        let surface = match self.surfaces.get(surface_idx) {
            Some(s) if s.active => s,
            _ => return,
        };

        let client_idx = surface.client_idx;
        let surface_id = surface.surface_id;
        let _ = self.server.queue_event(
            client_idx,
            &Event::PointerLeave {
                surface: surface_id,
            },
        );
    }

    /// Send pointer motion event.
    pub fn send_pointer_motion(&mut self, surface_idx: usize, time: u32, x: i32, y: i32) {
        let surface = match self.surfaces.get(surface_idx) {
            Some(s) if s.active => s,
            _ => return,
        };

        let client_idx = surface.client_idx;
        let _ = self
            .server
            .queue_event(client_idx, &Event::PointerMotion { time, x, y });
    }

    /// Send pointer button event.
    pub fn send_pointer_button(
        &mut self,
        surface_idx: usize,
        serial: u32,
        time: u32,
        button: u32,
        state: u32,
    ) {
        let surface = match self.surfaces.get(surface_idx) {
            Some(s) if s.active => s,
            _ => return,
        };

        let client_idx = surface.client_idx;
        let _ = self.server.queue_event(
            client_idx,
            &Event::PointerButton {
                serial,
                time,
                button,
                pressed: state != 0,
            },
        );
    }

    /// Send pointer axis (scroll) event.
    pub fn send_pointer_axis(&mut self, surface_idx: usize, time: u32, axis: u32, value: i32) {
        let surface = match self.surfaces.get(surface_idx) {
            Some(s) if s.active => s,
            _ => return,
        };

        let client_idx = surface.client_idx;
        let _ = self
            .server
            .queue_event(client_idx, &Event::PointerAxis { time, axis, value });
    }

    /// Send keyboard key event.
    pub fn send_key(
        &mut self,
        surface_idx: usize,
        serial: u32,
        time: u32,
        scancode: u32,
        ascii: u32,
        state: u32,
    ) {
        let surface = match self.surfaces.get(surface_idx) {
            Some(s) if s.active => s,
            _ => return,
        };

        let client_idx = surface.client_idx;
        let _ = self.server.queue_event(
            client_idx,
            &Event::Key {
                serial,
                time,
                scancode,
                ascii,
                pressed: state != 0,
            },
        );
    }

    /// Send keyboard modifiers event.
    pub fn send_modifiers(&mut self, surface_idx: usize, mods: u32) {
        let surface = match self.surfaces.get(surface_idx) {
            Some(s) if s.active => s,
            _ => return,
        };

        let client_idx = surface.client_idx;
        let _ = self
            .server
            .queue_event(client_idx, &Event::Modifiers { mods });
    }

    // ── Input forwarding ───────────────────────────────────────────────────

    /// Forward raw input events to the appropriate protocol surface.
    pub fn forward_input_events(
        &mut self,
        events: &[slopos_abi::InputEvent],
        pointer_focus_task: u32,
        keyboard_focus_task: u32,
        mouse_x: i32,
        mouse_y: i32,
        modifier_state: u8,
        serial: &mut u32,
    ) {
        use slopos_abi::InputEventType;

        let ptr_idx = self.task_id_to_surface_idx(pointer_focus_task);
        let kbd_idx = self.task_id_to_surface_idx(keyboard_focus_task);

        for event in events {
            let time = event.timestamp_ms as u32;
            match event.event_type {
                InputEventType::PointerMotion => {
                    if let Some(idx) = ptr_idx {
                        let local_x = mouse_x - self.surfaces[idx].window_x;
                        let local_y = mouse_y - self.surfaces[idx].window_y;
                        self.send_pointer_motion(idx, time, local_x, local_y);
                    }
                }
                InputEventType::PointerButtonPress => {
                    if let Some(idx) = ptr_idx {
                        *serial = serial.wrapping_add(1);
                        let button = event.data.data0;
                        self.send_pointer_button(idx, *serial, time, button, 1);
                    }
                }
                InputEventType::PointerButtonRelease => {
                    if let Some(idx) = ptr_idx {
                        *serial = serial.wrapping_add(1);
                        let button = event.data.data0;
                        self.send_pointer_button(idx, *serial, time, button, 0);
                    }
                }
                InputEventType::PointerAxis => {
                    if let Some(idx) = ptr_idx {
                        self.send_pointer_axis(idx, time, event.axis_id(), event.axis_value_v120());
                    }
                }
                InputEventType::KeyPress => {
                    if let Some(idx) = kbd_idx {
                        *serial = serial.wrapping_add(1);
                        // Modifiers first (the wl_keyboard rule): the client
                        // must judge the key against current modifier state,
                        // never the previous event's.
                        self.send_modifiers(idx, modifier_state as u32);
                        self.send_key(
                            idx,
                            *serial,
                            time,
                            event.key_scancode() as u32,
                            event.key_ascii() as u32,
                            1,
                        );
                    }
                }
                InputEventType::KeyRelease => {
                    if let Some(idx) = kbd_idx {
                        *serial = serial.wrapping_add(1);
                        self.send_modifiers(idx, modifier_state as u32);
                        self.send_key(
                            idx,
                            *serial,
                            time,
                            event.key_scancode() as u32,
                            event.key_ascii() as u32,
                            0,
                        );
                    }
                }
                _ => {}
            }
        }
    }

    /// Send pointer enter event to a protocol surface (by pseudo task_id).
    pub fn send_pointer_enter_for_task(
        &mut self,
        task_id: u32,
        serial: u32,
        x: i32,
        y: i32,
    ) -> bool {
        if let Some(idx) = self.task_id_to_surface_idx(task_id) {
            let local_x = x - self.surfaces[idx].window_x;
            let local_y = y - self.surfaces[idx].window_y;
            self.send_pointer_enter(idx, serial, local_x, local_y);
            true
        } else {
            false
        }
    }

    /// Send pointer leave event to a protocol surface (by pseudo task_id).
    pub fn send_pointer_leave_for_task(&mut self, task_id: u32, serial: u32) {
        if let Some(idx) = self.task_id_to_surface_idx(task_id) {
            self.send_pointer_leave(idx, serial);
        }
    }

    // ── Window enumeration ─────────────────────────────────────────────────

    /// Fill a WindowInfo array from local protocol surfaces, sorted by z_order.
    pub fn get_windows(&self, out: &mut [WindowInfo]) -> u32 {
        let mut indices = [0usize; MAX_SURFACES];
        let mut count = 0usize;

        for i in 0..MAX_SURFACES {
            let s = &self.surfaces[i];
            if s.active && s.visible && s.role == SurfaceRole::Toplevel {
                indices[count] = i;
                count += 1;
            }
        }

        // Insertion sort by z_order
        for i in 1..count {
            let key = indices[i];
            let key_z = self.surfaces[key].z_order;
            let mut j = i;
            while j > 0 && self.surfaces[indices[j - 1]].z_order > key_z {
                indices[j] = indices[j - 1];
                j -= 1;
            }
            indices[j] = key;
        }

        let write_count = count.min(out.len());
        for i in 0..write_count {
            let s = &self.surfaces[indices[i]];
            let mut info = WindowInfo::default();
            // SAFETY: surface index + 1 is always > 0 (MAX_SURFACES << u32::MAX).
            info.task_id = NonZeroU32::new(indices[i] as u32 + 1).unwrap().get();
            info.x = s.window_x;
            info.y = s.window_y;
            info.width = s.width;
            info.height = s.height;
            info.state = s.window_state;
            info.shm_token = s.shm_token;
            info.cursor_shape = s.cursor_shape;
            info.frame_width = s.frame_width;
            info.frame_height = s.frame_height;
            info.title.copy_from_slice(&s.title[..32]);
            let mut aid = [0u8; 32];
            aid.copy_from_slice(&s.app_id[..32]);
            info.app_id = AppId(aid);

            if s.dirty {
                if s.committed_damage_count == 0 {
                    info.damage_count = u8::MAX;
                } else {
                    let dc = (s.committed_damage_count as usize).min(MAX_WINDOW_DAMAGE_REGIONS);
                    info.damage_count = dc as u8;
                    for j in 0..dc {
                        info.damage_regions[j] = s.committed_damage[j];
                    }
                }
            }

            out[i] = info;
        }

        write_count as u32
    }

    /// Clear dirty flags on all surfaces after rendering.
    pub fn clear_dirty(&mut self) {
        for s in &mut self.surfaces {
            if s.active {
                s.dirty = false;
                s.committed_damage_count = 0;
            }
        }
    }

    // ── Window management ──────────────────────────────────────────────────

    pub fn set_window_position(&mut self, task_id: u32, x: i32, y: i32) {
        if let Some(idx) = self.task_id_to_surface_idx(task_id) {
            self.surfaces[idx].window_x = x;
            self.surfaces[idx].window_y = y;
        }
    }

    pub fn set_window_size(&mut self, task_id: u32, w: u32, h: u32) {
        if let Some(idx) = self.task_id_to_surface_idx(task_id) {
            self.surfaces[idx].frame_width = w;
            self.surfaces[idx].frame_height = h;
        }
    }

    pub fn set_window_state(&mut self, task_id: u32, state: u8) {
        if let Some(idx) = self.task_id_to_surface_idx(task_id) {
            self.surfaces[idx].window_state = state;
        }
    }

    pub fn raise_window(&mut self, task_id: u32) {
        if let Some(idx) = self.task_id_to_surface_idx(task_id) {
            self.surfaces[idx].z_order = self.next_z_order;
            self.next_z_order = self.next_z_order.wrapping_add(1).max(1);
        }
    }

    pub fn send_configure_for_task(&mut self, task_id: u32, width: u32, height: u32, states: u32) {
        if let Some(idx) = self.task_id_to_surface_idx(task_id) {
            self.send_configure(idx, width, height, states);
        }
    }

    pub fn send_close_for_task(&mut self, task_id: u32) -> bool {
        if let Some(idx) = self.task_id_to_surface_idx(task_id) {
            self.send_close(idx);
            true
        } else {
            false
        }
    }

    // ── Client cleanup ─────────────────────────────────────────────────────

    /// Detect and reap client disconnections.
    ///
    /// Two detection sources converge here, both routed through the single
    /// teardown funnel ([`cleanup_client`]):
    ///
    /// 1. `probe_disconnected()` does a non-blocking recv into the read
    ///    buffer to catch a client that closed without sending data (no
    ///    framed messages are consumed; any bytes are preserved for the
    ///    next `process_requests`).  On EOF it flags the client dead.
    /// 2. The Server independently flags clients dead when a flush hits a
    ///    broken pipe or a write buffer overflows — the usual signal that a
    ///    GUI client was *killed*, since the compositor continuously sends
    ///    it input and frame events.
    ///
    /// `take_disconnected()` returns every flagged-but-unreaped client from
    /// both sources; we destroy each one's surfaces and free its slot.
    /// Because a connection slot is freed *only* on this path, an active
    /// surface always implies a live owning connection — a killed client can
    /// never leave a ghost window behind.
    pub fn cleanup_disconnected(&mut self) {
        for idx in 0..MAX_CLIENTS {
            self.server.probe_disconnected(idx);
        }
        self.reap_disconnected_clients();
    }

    /// Build poll FDs for the listen socket + all connected clients.
    pub fn server_poll_fds(&self, out: &mut [slopos_abi::syscall::types::UserPollFd]) -> usize {
        self.server.build_poll_fds(out)
    }

    /// Flush all per-client write buffers to their sockets (non-blocking).
    ///
    /// Call once per frame.  EAGAIN is absorbed — data stays buffered for
    /// the next frame.  A hard error flags the client dead inside the Server;
    /// we immediately drain those through the teardown funnel so a killed
    /// client's window is gone within the same frame.
    pub fn flush_all(&mut self) {
        self.server.flush_clients();
        self.reap_disconnected_clients();
    }

    /// Tear down a client by index through the single teardown funnel.
    ///
    /// The per-client async task calls this when its readiness stream
    /// terminates (`POLLHUP`/`POLLERR`) without a prior `recv_request`
    /// disconnect — e.g. a client that closes without ever sending. A no-op
    /// if the slot is already free, so it is safe to call unconditionally on
    /// task exit.
    pub fn disconnect_client(&mut self, client_idx: usize) {
        if self.server.is_connected(client_idx) {
            self.cleanup_client(client_idx);
        }
    }

    /// The single client-teardown funnel.
    ///
    /// Destroys every surface owned by `client_idx`, then frees the Server
    /// connection slot.  This is the only place a connection slot is freed,
    /// mirroring libwayland-server's `wl_client_destroy`: detection is split
    /// across many call sites, but teardown — surfaces first, then the
    /// connection — happens in exactly one place.
    fn cleanup_client(&mut self, client_idx: usize) {
        for i in 0..MAX_SURFACES {
            if self.surfaces[i].active && self.surfaces[i].client_idx == client_idx {
                self.destroy_surface(i);
            }
        }
        self.server.disconnect(client_idx);
    }

    /// Drain every client the Server has flagged disconnected and run each
    /// through the teardown funnel.  Called after both detection passes
    /// (`probe_disconnected` in `cleanup_disconnected`, flush errors in
    /// `flush_all`).
    fn reap_disconnected_clients(&mut self) {
        let mut dead = [0usize; MAX_CLIENTS];
        let n = self.server.take_disconnected(&mut dead);
        for &idx in &dead[..n] {
            self.cleanup_client(idx);
        }
    }

    // ── Internal helpers ───────────────────────────────────────────────────

    fn find_surface(&self, client_idx: usize, surface_id: SurfaceId) -> Option<usize> {
        self.surfaces
            .iter()
            .position(|s| s.active && s.client_idx == client_idx && s.surface_id == surface_id)
    }

    fn find_surface_by_toplevel(
        &self,
        client_idx: usize,
        toplevel_id: ToplevelId,
    ) -> Option<usize> {
        self.surfaces
            .iter()
            .position(|s| s.active && s.client_idx == client_idx && s.toplevel_id == toplevel_id)
    }

    fn task_id_to_surface_idx(&self, task_id: u32) -> Option<usize> {
        let idx = (NonZeroU32::new(task_id)?.get() - 1) as usize;
        if idx < MAX_SURFACES && self.surfaces[idx].active {
            Some(idx)
        } else {
            None
        }
    }

    fn destroy_surface(&mut self, idx: usize) {
        if idx >= MAX_SURFACES || !self.surfaces[idx].active {
            return;
        }

        if let Some(parent_idx) = self.surfaces[idx].parent_surface_idx {
            if parent_idx < MAX_SURFACES && self.surfaces[parent_idx].active {
                let parent = &mut self.surfaces[parent_idx];
                for j in 0..parent.child_count as usize {
                    if parent.children[j] == Some(idx) {
                        for k in j..parent.child_count as usize - 1 {
                            parent.children[k] = parent.children[k + 1];
                        }
                        parent.children[parent.child_count as usize - 1] = None;
                        parent.child_count -= 1;
                        break;
                    }
                }
            }
        }

        // Snapshot the children array before recursing, since recursive
        // destroy_surface calls can modify parent children lists.
        let children_snapshot = self.surfaces[idx].children;
        let child_count = self.surfaces[idx].child_count as usize;
        // Clear children from this surface BEFORE recursing to prevent
        // the recursive parent-removal logic from modifying us mid-iteration.
        self.surfaces[idx].children = [None; MAX_CHILDREN];
        self.surfaces[idx].child_count = 0;
        for j in 0..child_count {
            if let Some(child_idx) = children_snapshot[j] {
                self.destroy_surface(child_idx);
            }
        }

        self.surfaces[idx] = ProtocolSurface::empty();
    }
}
