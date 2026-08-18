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

/// Hard ceiling on a single clipboard payload: a hostile or runaway selection
/// cannot exhaust memory.
const MAX_CLIPBOARD_BYTES: u32 = 16 * 1024 * 1024;

const MAX_SURFACES: usize = 32;
const MAX_PENDING_DAMAGE: usize = 8;
const MAX_CHILDREN: usize = 8;

const MAX_SURFACE_BUFFERS: usize = 2;
/// `current_buffer` sentinel: no buffer has been committed yet.
const NO_BUFFER: u8 = u8::MAX;

/// One client-registered buffer slot. The bridge owns `fd` for the slot's
/// lifetime and closes it on teardown and re-registration; the renderer's
/// surface cache only borrows a read-only mapping.
#[derive(Copy, Clone)]
struct SurfaceBuffer {
    fd: i32,
    width: u32,
    height: u32,
    registered: bool,
}

impl SurfaceBuffer {
    const EMPTY: Self = Self {
        fd: -1,
        width: 0,
        height: 0,
        registered: false,
    };
}

#[derive(Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
#[allow(dead_code)]
enum SurfaceRole {
    None = 0,
    Toplevel = 1,
    Popup = 2,
    Subsurface = 3,
}

#[allow(dead_code)]
struct ProtocolSurface {
    active: bool,
    client_idx: usize,
    surface_id: SurfaceId,
    toplevel_id: ToplevelId,
    /// Current buffer's fd (= `buffers[current_buffer].fd`), threaded to the
    /// renderer via `WindowInfo.shm_token`; 0 when nothing is committed.
    shm_token: u32,
    width: u32,
    height: u32,
    frame_width: u32,
    frame_height: u32,
    buffers: [SurfaceBuffer; MAX_SURFACE_BUFFERS],
    pending_buffer: u8,
    current_buffer: u8,
    pending_damage: [DamageRect; MAX_PENDING_DAMAGE],
    pending_damage_count: u8,
    committed_damage: [DamageRect; MAX_PENDING_DAMAGE],
    committed_damage_count: u8,
    dirty: bool,
    window_x: i32,
    window_y: i32,
    z_order: u32,
    visible: bool,
    window_state: u8,
    /// Monotonic incarnation id: distinguishes a recycled surface slot (same
    /// `task_id`) from an earlier surface in it, so the renderer's buffer cache
    /// cannot alias a stale mapping.
    generation: u32,
    frame_callback_pending: bool,
    last_present_time_ms: u64,
    role: SurfaceRole,
    parent_surface_idx: Option<usize>,
    children: [Option<usize>; MAX_CHILDREN],
    child_count: u8,
    relative_x: i32,
    relative_y: i32,
    acked_serial: u32,
    title: [u8; MAX_STRING_LEN],
    app_id: [u8; MAX_STRING_LEN],
    cursor_shape: u8,
    last_enter_serial: u32,
    has_pointer: bool,
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
            buffers: [SurfaceBuffer::EMPTY; MAX_SURFACE_BUFFERS],
            pending_buffer: 0,
            current_buffer: NO_BUFFER,
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
            generation: 0,
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
            last_enter_serial: 0,
            has_pointer: false,
        }
    }
}

/// Clipboard shared across all clients: a read-only mapping of the source memfd
/// (owns the fd; dropped when replaced) and its valid byte count. `None` source
/// means the clipboard is empty.
struct Clipboard {
    source: Option<CachedShmMapping>,
    len: u32,
}

pub struct ProtocolBridge {
    server: Server,
    surfaces: [ProtocolSurface; MAX_SURFACES],
    next_z_order: u32,
    /// Monotonic source for `ProtocolSurface.generation` (never reused).
    next_surface_gen: u32,
    clipboard: Clipboard,
    display_width: u32,
    display_height: u32,
    display_format: u32,
    display_pitch: u32,
    configure_serial: u32,
    /// Per-slot connection generation. Slot indices and fd numbers are recycled
    /// across disconnect→reconnect, so without this a stale per-client task
    /// could drive or tear down a successor's connection.
    client_gen: [u64; MAX_CLIENTS],
    /// Monotonic source for `client_gen` values (never reused).
    next_gen: u64,
}

impl ProtocolBridge {
    pub fn new() -> Option<Self> {
        let server = Server::bind(b"/run/compositor").ok()?;
        tty::write(b"COMPOSITOR: protocol bridge listening on /run/compositor\n");

        Some(Self {
            server,
            surfaces: [const { ProtocolSurface::empty() }; MAX_SURFACES],
            next_z_order: 1,
            next_surface_gen: 1,
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

    pub fn set_display_info(&mut self, width: u32, height: u32, format: u32, pitch: u32) {
        self.display_width = width;
        self.display_height = height;
        self.display_format = format;
        self.display_pitch = pitch;
    }

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

    pub fn accept_clients(&mut self) {
        // Bounded per frame so a connect burst cannot stall the compositor loop.
        for _ in 0..4 {
            match self.server.accept() {
                Ok(Some(idx)) => self.greet_client(idx),
                Ok(None) => break,
                Err(_) => break,
            }
        }
    }

    pub fn listen_fd(&self) -> i32 {
        self.server.listen_fd()
    }

    pub fn client_fd(&self, idx: usize) -> Option<i32> {
        self.server
            .clients
            .get(idx)
            .and_then(|slot| slot.as_ref().filter(|c| c.active).map(|c| c.conn.fd()))
    }

    /// Paired with [`client_fd`] this uniquely identifies one connection: slot
    /// indices and fd numbers are recycled, `client_gen` values never are.
    pub fn client_gen(&self, idx: usize) -> Option<u64> {
        if self.server.is_connected(idx) {
            self.client_gen.get(idx).copied()
        } else {
            None
        }
    }

    /// Async accept path: greet and record every pending connection as a
    /// `(client_idx, client_fd, client_gen)` triple in `out`, returning the
    /// count. Unlike [`accept_clients`] it drains the full backlog, being driven
    /// by listen-socket readiness rather than a per-frame budget. The generation
    /// is stamped atomically with the accept, so a triple names one connection
    /// for the lifetime of the caller's per-client task.
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

    pub fn process_requests(&mut self) {
        for client_idx in 0..32 {
            self.process_client(client_idx);
        }
    }

    /// Drain one client's pending requests. Returns `false` once the client
    /// disconnects — teardown has already run via [`cleanup_client`], so the
    /// caller's per-client task should exit.
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
                // Client echoing our Hello; the connection is already accepted.
            }
            Request::CreateSurface { new_id } => {
                self.handle_create_surface(client_idx, new_id);
            }
            Request::SurfaceAttach {
                surface,
                buffer_id,
                width,
                height,
                has_fd: _,
                buffer_fd,
            } => {
                let fd = buffer_fd.map(|f| f.into_raw());
                self.handle_surface_attach(client_idx, surface, buffer_id, fd, width, height);
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
            Request::SetCursorShape {
                surface,
                serial,
                shape,
            } => {
                self.handle_set_cursor_shape(client_idx, surface, serial, shape);
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

    fn handle_create_surface(&mut self, client_idx: usize, new_id: SurfaceId) {
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
        self.surfaces[slot].generation = self.next_surface_gen;
        self.next_surface_gen = self.next_surface_gen.wrapping_add(1).max(1);
    }

    /// Register or re-select a double-buffer slot for `surface`'s next commit.
    ///
    /// `fd` is `Some` only the first time a slot is used (the memfd received via
    /// SCM_RIGHTS); thereafter the client re-selects the slot by id and the
    /// stored fd is reused. The bridge owns every stored fd; one that cannot be
    /// parked (no such surface / bad slot) is closed here rather than leaked.
    fn handle_surface_attach(
        &mut self,
        client_idx: usize,
        surface_id: SurfaceId,
        buffer_id: u32,
        fd: Option<i32>,
        width: u32,
        height: u32,
    ) {
        let bid = buffer_id as usize;
        let idx = match self.find_surface(client_idx, surface_id) {
            Some(i) if bid < MAX_SURFACE_BUFFERS => i,
            _ => {
                if let Some(fd) = fd {
                    crate::syscall::memory::close(fd);
                }
                return;
            }
        };

        let surface = &mut self.surfaces[idx];
        surface.pending_buffer = buffer_id as u8;
        let slot = &mut surface.buffers[bid];
        if let Some(new_fd) = fd {
            if slot.registered && slot.fd >= 0 && slot.fd != new_fd {
                crate::syscall::memory::close(slot.fd);
            }
            slot.fd = new_fd;
            slot.registered = true;
        }
        slot.width = width;
        slot.height = height;
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
                // Precise tracking exhausted: collapse to full-surface damage.
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
        let Some(idx) = self.find_surface(client_idx, surface_id) else {
            return;
        };

        let release_prev = {
            let surface = &mut self.surfaces[idx];
            let prev = surface.current_buffer;
            let pend = surface.pending_buffer;
            surface.current_buffer = pend;

            let slot = surface.buffers[pend as usize];
            if slot.registered {
                surface.shm_token = slot.fd as u32;
                surface.width = slot.width;
                surface.height = slot.height;
                if surface.width > 0 && surface.height > 0 {
                    surface.visible = true;
                }
            }

            surface.committed_damage = surface.pending_damage;
            surface.committed_damage_count = surface.pending_damage_count;
            surface.pending_damage = [DamageRect::invalid(); MAX_PENDING_DAMAGE];
            surface.pending_damage_count = 0;
            surface.dirty = true;

            if prev != NO_BUFFER && prev != pend {
                Some(prev)
            } else {
                None
            }
        };

        if let Some(prev) = release_prev {
            let _ = self.server.queue_event(
                client_idx,
                &Event::BufferRelease {
                    surface: surface_id,
                    buffer_id: prev as u32,
                },
            );
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

    /// Honored only when the request carries the surface's most recent enter
    /// serial *and* the pointer is still inside it: no surface can influence the
    /// cursor unless the pointer is over it.
    fn handle_set_cursor_shape(
        &mut self,
        client_idx: usize,
        surface_id: SurfaceId,
        serial: u32,
        shape: u8,
    ) {
        if let Some(idx) = self.find_surface(client_idx, surface_id) {
            let s = &mut self.surfaces[idx];
            if s.has_pointer && serial != 0 && serial == s.last_enter_serial {
                s.cursor_shape = shape;
            }
        }
    }

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

    fn handle_ack_configure(&mut self, client_idx: usize, serial: u32) {
        for s in &mut self.surfaces {
            if s.active && s.client_idx == client_idx && s.role == SurfaceRole::Toplevel {
                s.acked_serial = serial;
            }
        }
    }

    fn handle_interactive_move(
        &mut self,
        client_idx: usize,
        toplevel_id: ToplevelId,
        _serial: u32,
    ) {
        // No-op: the compositor drives moves itself, via title-bar drag.
        let _ = (client_idx, toplevel_id);
    }

    fn handle_interactive_resize(
        &mut self,
        client_idx: usize,
        toplevel_id: ToplevelId,
        _serial: u32,
        _edges: u32,
    ) {
        // No-op: client-initiated resize is not honored.
        let _ = (client_idx, toplevel_id);
    }

    /// Publish a new clipboard: map the received source memfd read-only,
    /// replacing (and so closing) any previous source. The mapping keeps the
    /// memfd backing alive after the client closes its own copy.
    fn handle_clipboard_copy(&mut self, buffer_fd: Option<OwnedFd>, len: u32) {
        let Some(fd) = buffer_fd else { return };
        let len = len.min(MAX_CLIPBOARD_BYTES);
        if len == 0 {
            self.clipboard.source = None;
            self.clipboard.len = 0;
            return;
        }
        // `into_raw` drops close-on-drop: the mapping owns the fd on success, so
        // the failure path has to close it.
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
    /// `ClipboardRead` carrying a destination memfd, since the server event path
    /// cannot itself carry an fd.
    fn handle_clipboard_paste(&mut self, client_idx: usize) {
        let _ = self.server.queue_event(
            client_idx,
            &Event::PasteReady {
                len: self.clipboard.len,
            },
        );
    }

    /// Copy the clipboard into the client-provided destination memfd and report
    /// the valid byte count. The source mapping is retained so the clipboard
    /// survives repeated pastes.
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

    /// Records `serial` as the surface's enter serial and marks it as holding
    /// the pointer, which is what a later `SetCursorShape` is gated on.
    pub fn send_pointer_enter(&mut self, surface_idx: usize, serial: u32, x: i32, y: i32) {
        let surface = match self.surfaces.get_mut(surface_idx) {
            Some(s) if s.active => s,
            _ => return,
        };

        surface.last_enter_serial = serial;
        surface.has_pointer = true;
        let client_idx = surface.client_idx;
        let surface_id = surface.surface_id;
        let _ = self.server.queue_event(
            client_idx,
            &Event::PointerEnter {
                surface: surface_id,
                serial,
                x,
                y,
            },
        );
    }

    /// Clears the pointer-hold flag, so `SetCursorShape` from this surface is
    /// rejected until it is re-entered.
    pub fn send_pointer_leave(&mut self, surface_idx: usize, _serial: u32) {
        let surface = match self.surfaces.get_mut(surface_idx) {
            Some(s) if s.active => s,
            _ => return,
        };

        surface.has_pointer = false;
        let client_idx = surface.client_idx;
        let surface_id = surface.surface_id;
        let _ = self.server.queue_event(
            client_idx,
            &Event::PointerLeave {
                surface: surface_id,
            },
        );
    }

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

    #[allow(clippy::too_many_arguments)]
    pub fn send_key(
        &mut self,
        surface_idx: usize,
        serial: u32,
        time: u32,
        scancode: u32,
        ascii: u32,
        keycode: u32,
        codepoint: u32,
        modifiers: u32,
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
                keycode,
                codepoint,
                modifiers,
                pressed: state != 0,
            },
        );
    }

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

    /// Forward one key event to the keyboard-focus surface. Modifiers are sent
    /// first, per the wl_keyboard rule that a client judges a key against
    /// current modifier state, never the previous event's.
    pub fn forward_key_event(
        &mut self,
        keyboard_focus_task: u32,
        event: &slopos_abi::InputEvent,
        pressed: bool,
        modifier_state: u8,
        serial: &mut u32,
    ) {
        let time = event.timestamp_ms as u32;
        let Some(idx) = self.task_id_to_surface_idx(keyboard_focus_task) else {
            if pressed {
                // The keystroke is lost here; mirror it so input black holes
                // stay visible on the serial log.
                let msg = std::format!(
                    "COMP: key 0x{:02x} dropped (no surface for focus task {})\n",
                    event.key_ascii(),
                    keyboard_focus_task
                );
                let _ = tty::write(msg.as_bytes());
            }
            return;
        };
        *serial = serial.wrapping_add(1);
        self.send_modifiers(idx, modifier_state as u32);
        self.send_key(
            idx,
            *serial,
            time,
            event.key_scancode() as u32,
            event.key_ascii() as u32,
            event.key_keycode() as u32,
            event.key_codepoint(),
            modifier_state as u32,
            pressed as u32,
        );
    }

    /// `x`/`y` are global; the surface is sent surface-local coordinates.
    pub fn send_pointer_motion_for_task(&mut self, task_id: u32, time: u32, x: i32, y: i32) {
        if let Some(idx) = self.task_id_to_surface_idx(task_id) {
            let local_x = x - self.surfaces[idx].window_x;
            let local_y = y - self.surfaces[idx].window_y;
            self.send_pointer_motion(idx, time, local_x, local_y);
        }
    }

    pub fn send_pointer_button_for_task(
        &mut self,
        task_id: u32,
        serial: u32,
        time: u32,
        button: u32,
        state: u32,
    ) {
        if let Some(idx) = self.task_id_to_surface_idx(task_id) {
            self.send_pointer_button(idx, serial, time, button, state);
        }
    }

    pub fn send_pointer_axis_for_task(&mut self, task_id: u32, time: u32, axis: u32, value: i32) {
        if let Some(idx) = self.task_id_to_surface_idx(task_id) {
            self.send_pointer_axis(idx, time, axis, value);
        }
    }

    /// `x`/`y` are global; the surface is sent surface-local coordinates.
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

    pub fn send_pointer_leave_for_task(&mut self, task_id: u32, serial: u32) {
        if let Some(idx) = self.task_id_to_surface_idx(task_id) {
            self.send_pointer_leave(idx, serial);
        }
    }

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
            info.buffer_id = if s.current_buffer == NO_BUFFER {
                0
            } else {
                s.current_buffer
            };
            info.buffer_generation = s.generation;
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

    /// Clear dirty + committed damage only for surfaces whose content reached
    /// the screen this frame, identified by `task_id` (= surface slot index + 1,
    /// as minted by [`get_windows`]). A `SurfaceCommit` that landed after this
    /// frame's snapshot keeps its dirty flag and is exported on the next frame
    /// rather than cleared before it is shown.
    pub fn clear_presented(&mut self, presented_task_ids: &[u32]) {
        for &task_id in presented_task_ids {
            if let Some(idx) = self.task_id_to_surface_idx(task_id) {
                self.surfaces[idx].dirty = false;
                self.surfaces[idx].committed_damage_count = 0;
            }
        }
    }

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

    /// Detect and reap client disconnections. The probe is a non-blocking recv
    /// that catches a client which closed without sending; it consumes no framed
    /// message, and any bytes read stay for the next `process_requests`. Clients
    /// the Server flagged dead on a failed flush are reaped in the same pass.
    pub fn cleanup_disconnected(&mut self) {
        for idx in 0..MAX_CLIENTS {
            self.server.probe_disconnected(idx);
        }
        self.reap_disconnected_clients();
    }

    /// Poll FDs for the listen socket plus every connected client.
    pub fn server_poll_fds(&self, out: &mut [slopos_abi::syscall::types::UserPollFd]) -> usize {
        self.server.build_poll_fds(out)
    }

    /// Flush all per-client write buffers (non-blocking, once per frame).
    /// EAGAIN is absorbed and the data stays buffered; a hard error flags the
    /// client dead, and those are reaped here so a killed client's window is
    /// gone within the same frame.
    pub fn flush_all(&mut self) {
        self.server.flush_clients();
        self.reap_disconnected_clients();
    }

    /// Tear down a client whose readiness stream terminated
    /// (`POLLHUP`/`POLLERR`) with no prior `recv_request` disconnect. A no-op if
    /// the slot is already free, so it is safe on any task exit.
    pub fn disconnect_client(&mut self, client_idx: usize) {
        if self.server.is_connected(client_idx) {
            self.cleanup_client(client_idx);
        }
    }

    /// The single client-teardown funnel: surfaces first, then the connection
    /// slot. Detection is spread across call sites, but this is the only place a
    /// slot is freed, so an active surface always implies a live connection.
    fn cleanup_client(&mut self, client_idx: usize) {
        for i in 0..MAX_SURFACES {
            if self.surfaces[i].active && self.surfaces[i].client_idx == client_idx {
                self.destroy_surface(i);
            }
        }
        self.server.disconnect(client_idx);
    }

    fn reap_disconnected_clients(&mut self) {
        let mut dead = [0usize; MAX_CLIENTS];
        let n = self.server.take_disconnected(&mut dead);
        for &idx in &dead[..n] {
            self.cleanup_client(idx);
        }
    }

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

        // Snapshot and clear before recursing: a recursive destroy removes the
        // child from this list, which would otherwise mutate it mid-iteration.
        let children_snapshot = self.surfaces[idx].children;
        let child_count = self.surfaces[idx].child_count as usize;
        self.surfaces[idx].children = [None; MAX_CHILDREN];
        self.surfaces[idx].child_count = 0;
        for j in 0..child_count {
            if let Some(child_idx) = children_snapshot[j] {
                self.destroy_surface(child_idx);
            }
        }

        // A MAP_SHARED mapping the cache may still hold stays valid after the fd
        // is closed.
        for slot in &mut self.surfaces[idx].buffers {
            if slot.registered && slot.fd >= 0 {
                crate::syscall::memory::close(slot.fd);
            }
            *slot = SurfaceBuffer::EMPTY;
        }

        self.surfaces[idx] = ProtocolSurface::empty();
    }
}
