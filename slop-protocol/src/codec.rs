//! Binary codec: Encode/Decode traits and implementations for Request/Event.
//!
//! Wire format per message (framing handled by Connection):
//! `[tag: u8][field1][field2]...[fieldN]`
//!
//! All fields are little-endian. No padding, no alignment.

use crate::types::{Event, ProtocolError, Request};

/// Serialize into a byte buffer. Returns number of bytes written.
pub trait Encode {
    fn encode(&self, buf: &mut [u8]) -> Result<usize, ProtocolError>;
}

/// Deserialize from a byte buffer. Returns (value, bytes_consumed).
pub trait Decode: Sized {
    fn decode(buf: &[u8]) -> Result<(Self, usize), ProtocolError>;
}

// ── Primitive helpers ─────────────────────────────────────────────────────

fn put_u8(buf: &mut [u8], pos: usize, v: u8) -> Result<usize, ProtocolError> {
    if pos + 1 > buf.len() {
        return Err(ProtocolError::MessageTooLarge);
    }
    buf[pos] = v;
    Ok(pos + 1)
}

fn put_u16(buf: &mut [u8], pos: usize, v: u16) -> Result<usize, ProtocolError> {
    if pos + 2 > buf.len() {
        return Err(ProtocolError::MessageTooLarge);
    }
    buf[pos..pos + 2].copy_from_slice(&v.to_le_bytes());
    Ok(pos + 2)
}

fn put_u32(buf: &mut [u8], pos: usize, v: u32) -> Result<usize, ProtocolError> {
    if pos + 4 > buf.len() {
        return Err(ProtocolError::MessageTooLarge);
    }
    buf[pos..pos + 4].copy_from_slice(&v.to_le_bytes());
    Ok(pos + 4)
}

fn put_i32(buf: &mut [u8], pos: usize, v: i32) -> Result<usize, ProtocolError> {
    put_u32(buf, pos, v as u32)
}

fn put_bool(buf: &mut [u8], pos: usize, v: bool) -> Result<usize, ProtocolError> {
    put_u8(buf, pos, v as u8)
}

fn put_bytes(buf: &mut [u8], pos: usize, src: &[u8]) -> Result<usize, ProtocolError> {
    if pos + src.len() > buf.len() {
        return Err(ProtocolError::MessageTooLarge);
    }
    buf[pos..pos + src.len()].copy_from_slice(src);
    Ok(pos + src.len())
}

fn get_u8(buf: &[u8], pos: usize) -> Result<(u8, usize), ProtocolError> {
    if pos + 1 > buf.len() {
        return Err(ProtocolError::MalformedMessage);
    }
    Ok((buf[pos], pos + 1))
}

fn get_u16(buf: &[u8], pos: usize) -> Result<(u16, usize), ProtocolError> {
    if pos + 2 > buf.len() {
        return Err(ProtocolError::MalformedMessage);
    }
    let v = u16::from_le_bytes([buf[pos], buf[pos + 1]]);
    Ok((v, pos + 2))
}

fn get_u32(buf: &[u8], pos: usize) -> Result<(u32, usize), ProtocolError> {
    if pos + 4 > buf.len() {
        return Err(ProtocolError::MalformedMessage);
    }
    let v = u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]);
    Ok((v, pos + 4))
}

fn get_i32(buf: &[u8], pos: usize) -> Result<(i32, usize), ProtocolError> {
    let (v, p) = get_u32(buf, pos)?;
    Ok((v as i32, p))
}

fn get_bool(buf: &[u8], pos: usize) -> Result<(bool, usize), ProtocolError> {
    let (v, p) = get_u8(buf, pos)?;
    Ok((v != 0, p))
}

fn get_bytes<const N: usize>(buf: &[u8], pos: usize) -> Result<([u8; N], usize), ProtocolError> {
    if pos + N > buf.len() {
        return Err(ProtocolError::MalformedMessage);
    }
    let mut arr = [0u8; N];
    arr.copy_from_slice(&buf[pos..pos + N]);
    Ok((arr, pos + N))
}

// ── Request tag constants ─────────────────────────────────────────────────

const REQ_CREATE_SURFACE: u8 = 0;
const REQ_SURFACE_ATTACH: u8 = 1;
const REQ_SURFACE_DAMAGE: u8 = 2;
const REQ_SURFACE_COMMIT: u8 = 3;
const REQ_SURFACE_FRAME: u8 = 4;
const REQ_SURFACE_DESTROY: u8 = 5;
const REQ_GET_TOPLEVEL: u8 = 6;
const REQ_TOPLEVEL_SET_TITLE: u8 = 7;
const REQ_TOPLEVEL_SET_APP_ID: u8 = 8;
const REQ_TOPLEVEL_DESTROY: u8 = 9;
const REQ_SET_CURSOR_SHAPE: u8 = 10;
const REQ_GET_SUBSURFACE: u8 = 11;
const REQ_SUBSURFACE_SET_POSITION: u8 = 12;
const REQ_SUBSURFACE_DESTROY: u8 = 13;
const REQ_GET_POPUP: u8 = 14;
const REQ_POPUP_DESTROY: u8 = 15;
const REQ_GET_POINTER: u8 = 16;
const REQ_GET_KEYBOARD: u8 = 17;
const REQ_CLIPBOARD_COPY: u8 = 18;
const REQ_CLIPBOARD_PASTE: u8 = 19;

// ── Event tag constants ───────────────────────────────────────────────────
// Tags 0-5 were removed (client-assigned IDs). Remaining tags keep stable values.

const EVT_FRAME_DONE: u8 = 6;
const EVT_CONFIGURE: u8 = 7;
const EVT_CLOSE: u8 = 8;
const EVT_POINTER_ENTER: u8 = 9;
const EVT_POINTER_LEAVE: u8 = 10;
const EVT_POINTER_MOTION: u8 = 11;
const EVT_POINTER_BUTTON: u8 = 12;
const EVT_POINTER_AXIS: u8 = 13;
const EVT_KEYBOARD_ENTER: u8 = 14;
const EVT_KEYBOARD_LEAVE: u8 = 15;
const EVT_KEY: u8 = 16;
const EVT_MODIFIERS: u8 = 17;
const EVT_OUTPUT_INFO: u8 = 18;
const EVT_PASTE_RESULT: u8 = 19;
const EVT_ERROR: u8 = 20;

// ── Encode for Request ────────────────────────────────────────────────────

impl Encode for Request {
    fn encode(&self, buf: &mut [u8]) -> Result<usize, ProtocolError> {
        match self {
            Request::CreateSurface { new_id } => {
                let p = put_u8(buf, 0, REQ_CREATE_SURFACE)?;
                let p = put_u32(buf, p, *new_id)?;
                Ok(p)
            }
            Request::SurfaceAttach {
                surface,
                shm_token,
                width,
                height,
            } => {
                let p = put_u8(buf, 0, REQ_SURFACE_ATTACH)?;
                let p = put_u32(buf, p, *surface)?;
                let p = put_u32(buf, p, *shm_token)?;
                let p = put_u32(buf, p, *width)?;
                let p = put_u32(buf, p, *height)?;
                Ok(p)
            }
            Request::SurfaceDamage {
                surface,
                x,
                y,
                w,
                h,
            } => {
                let p = put_u8(buf, 0, REQ_SURFACE_DAMAGE)?;
                let p = put_u32(buf, p, *surface)?;
                let p = put_i32(buf, p, *x)?;
                let p = put_i32(buf, p, *y)?;
                let p = put_i32(buf, p, *w)?;
                let p = put_i32(buf, p, *h)?;
                Ok(p)
            }
            Request::SurfaceCommit { surface } => {
                let p = put_u8(buf, 0, REQ_SURFACE_COMMIT)?;
                let p = put_u32(buf, p, *surface)?;
                Ok(p)
            }
            Request::SurfaceFrame { surface } => {
                let p = put_u8(buf, 0, REQ_SURFACE_FRAME)?;
                let p = put_u32(buf, p, *surface)?;
                Ok(p)
            }
            Request::SurfaceDestroy { surface } => {
                let p = put_u8(buf, 0, REQ_SURFACE_DESTROY)?;
                let p = put_u32(buf, p, *surface)?;
                Ok(p)
            }
            Request::GetToplevel { surface, new_id } => {
                let p = put_u8(buf, 0, REQ_GET_TOPLEVEL)?;
                let p = put_u32(buf, p, *surface)?;
                let p = put_u32(buf, p, *new_id)?;
                Ok(p)
            }
            Request::ToplevelSetTitle {
                toplevel,
                title,
                len,
            } => {
                let p = put_u8(buf, 0, REQ_TOPLEVEL_SET_TITLE)?;
                let p = put_u32(buf, p, *toplevel)?;
                let p = put_bytes(buf, p, title)?;
                let p = put_u8(buf, p, *len)?;
                Ok(p)
            }
            Request::ToplevelSetAppId {
                toplevel,
                app_id,
                len,
            } => {
                let p = put_u8(buf, 0, REQ_TOPLEVEL_SET_APP_ID)?;
                let p = put_u32(buf, p, *toplevel)?;
                let p = put_bytes(buf, p, app_id)?;
                let p = put_u8(buf, p, *len)?;
                Ok(p)
            }
            Request::ToplevelDestroy { toplevel } => {
                let p = put_u8(buf, 0, REQ_TOPLEVEL_DESTROY)?;
                let p = put_u32(buf, p, *toplevel)?;
                Ok(p)
            }
            Request::SetCursorShape { surface, shape } => {
                let p = put_u8(buf, 0, REQ_SET_CURSOR_SHAPE)?;
                let p = put_u32(buf, p, *surface)?;
                let p = put_u8(buf, p, *shape)?;
                Ok(p)
            }
            Request::GetSubsurface {
                surface,
                parent,
                new_id,
            } => {
                let p = put_u8(buf, 0, REQ_GET_SUBSURFACE)?;
                let p = put_u32(buf, p, *surface)?;
                let p = put_u32(buf, p, *parent)?;
                let p = put_u32(buf, p, *new_id)?;
                Ok(p)
            }
            Request::SubsurfaceSetPosition { subsurface, x, y } => {
                let p = put_u8(buf, 0, REQ_SUBSURFACE_SET_POSITION)?;
                let p = put_u32(buf, p, *subsurface)?;
                let p = put_i32(buf, p, *x)?;
                let p = put_i32(buf, p, *y)?;
                Ok(p)
            }
            Request::SubsurfaceDestroy { subsurface } => {
                let p = put_u8(buf, 0, REQ_SUBSURFACE_DESTROY)?;
                let p = put_u32(buf, p, *subsurface)?;
                Ok(p)
            }
            Request::GetPopup {
                surface,
                parent,
                new_id,
            } => {
                let p = put_u8(buf, 0, REQ_GET_POPUP)?;
                let p = put_u32(buf, p, *surface)?;
                let p = put_u32(buf, p, *parent)?;
                let p = put_u32(buf, p, *new_id)?;
                Ok(p)
            }
            Request::PopupDestroy { popup } => {
                let p = put_u8(buf, 0, REQ_POPUP_DESTROY)?;
                let p = put_u32(buf, p, *popup)?;
                Ok(p)
            }
            Request::GetPointer { new_id } => {
                let p = put_u8(buf, 0, REQ_GET_POINTER)?;
                let p = put_u32(buf, p, *new_id)?;
                Ok(p)
            }
            Request::GetKeyboard { new_id } => {
                let p = put_u8(buf, 0, REQ_GET_KEYBOARD)?;
                let p = put_u32(buf, p, *new_id)?;
                Ok(p)
            }
            Request::ClipboardCopy { data, len } => {
                let p = put_u8(buf, 0, REQ_CLIPBOARD_COPY)?;
                let p = put_bytes(buf, p, data)?;
                let p = put_u16(buf, p, *len)?;
                Ok(p)
            }
            Request::ClipboardPaste => {
                let p = put_u8(buf, 0, REQ_CLIPBOARD_PASTE)?;
                Ok(p)
            }
        }
    }
}

// ── Decode for Request ────────────────────────────────────────────────────

impl Decode for Request {
    fn decode(buf: &[u8]) -> Result<(Self, usize), ProtocolError> {
        let (tag, p) = get_u8(buf, 0)?;
        match tag {
            REQ_CREATE_SURFACE => {
                let (new_id, p) = get_u32(buf, p)?;
                Ok((Request::CreateSurface { new_id }, p))
            }
            REQ_SURFACE_ATTACH => {
                let (surface, p) = get_u32(buf, p)?;
                let (shm_token, p) = get_u32(buf, p)?;
                let (width, p) = get_u32(buf, p)?;
                let (height, p) = get_u32(buf, p)?;
                Ok((
                    Request::SurfaceAttach {
                        surface,
                        shm_token,
                        width,
                        height,
                    },
                    p,
                ))
            }
            REQ_SURFACE_DAMAGE => {
                let (surface, p) = get_u32(buf, p)?;
                let (x, p) = get_i32(buf, p)?;
                let (y, p) = get_i32(buf, p)?;
                let (w, p) = get_i32(buf, p)?;
                let (h, p) = get_i32(buf, p)?;
                Ok((
                    Request::SurfaceDamage {
                        surface,
                        x,
                        y,
                        w,
                        h,
                    },
                    p,
                ))
            }
            REQ_SURFACE_COMMIT => {
                let (surface, p) = get_u32(buf, p)?;
                Ok((Request::SurfaceCommit { surface }, p))
            }
            REQ_SURFACE_FRAME => {
                let (surface, p) = get_u32(buf, p)?;
                Ok((Request::SurfaceFrame { surface }, p))
            }
            REQ_SURFACE_DESTROY => {
                let (surface, p) = get_u32(buf, p)?;
                Ok((Request::SurfaceDestroy { surface }, p))
            }
            REQ_GET_TOPLEVEL => {
                let (surface, p) = get_u32(buf, p)?;
                let (new_id, p) = get_u32(buf, p)?;
                Ok((Request::GetToplevel { surface, new_id }, p))
            }
            REQ_TOPLEVEL_SET_TITLE => {
                let (toplevel, p) = get_u32(buf, p)?;
                let (title, p) = get_bytes::<32>(buf, p)?;
                let (len, p) = get_u8(buf, p)?;
                Ok((
                    Request::ToplevelSetTitle {
                        toplevel,
                        title,
                        len,
                    },
                    p,
                ))
            }
            REQ_TOPLEVEL_SET_APP_ID => {
                let (toplevel, p) = get_u32(buf, p)?;
                let (app_id, p) = get_bytes::<32>(buf, p)?;
                let (len, p) = get_u8(buf, p)?;
                Ok((
                    Request::ToplevelSetAppId {
                        toplevel,
                        app_id,
                        len,
                    },
                    p,
                ))
            }
            REQ_TOPLEVEL_DESTROY => {
                let (toplevel, p) = get_u32(buf, p)?;
                Ok((Request::ToplevelDestroy { toplevel }, p))
            }
            REQ_SET_CURSOR_SHAPE => {
                let (surface, p) = get_u32(buf, p)?;
                let (shape, p) = get_u8(buf, p)?;
                Ok((Request::SetCursorShape { surface, shape }, p))
            }
            REQ_GET_SUBSURFACE => {
                let (surface, p) = get_u32(buf, p)?;
                let (parent, p) = get_u32(buf, p)?;
                let (new_id, p) = get_u32(buf, p)?;
                Ok((
                    Request::GetSubsurface {
                        surface,
                        parent,
                        new_id,
                    },
                    p,
                ))
            }
            REQ_SUBSURFACE_SET_POSITION => {
                let (subsurface, p) = get_u32(buf, p)?;
                let (x, p) = get_i32(buf, p)?;
                let (y, p) = get_i32(buf, p)?;
                Ok((Request::SubsurfaceSetPosition { subsurface, x, y }, p))
            }
            REQ_SUBSURFACE_DESTROY => {
                let (subsurface, p) = get_u32(buf, p)?;
                Ok((Request::SubsurfaceDestroy { subsurface }, p))
            }
            REQ_GET_POPUP => {
                let (surface, p) = get_u32(buf, p)?;
                let (parent, p) = get_u32(buf, p)?;
                let (new_id, p) = get_u32(buf, p)?;
                Ok((
                    Request::GetPopup {
                        surface,
                        parent,
                        new_id,
                    },
                    p,
                ))
            }
            REQ_POPUP_DESTROY => {
                let (popup, p) = get_u32(buf, p)?;
                Ok((Request::PopupDestroy { popup }, p))
            }
            REQ_GET_POINTER => {
                let (new_id, p) = get_u32(buf, p)?;
                Ok((Request::GetPointer { new_id }, p))
            }
            REQ_GET_KEYBOARD => {
                let (new_id, p) = get_u32(buf, p)?;
                Ok((Request::GetKeyboard { new_id }, p))
            }
            REQ_CLIPBOARD_COPY => {
                let (data, p) = get_bytes::<4096>(buf, p)?;
                let (len, p) = get_u16(buf, p)?;
                Ok((Request::ClipboardCopy { data, len }, p))
            }
            REQ_CLIPBOARD_PASTE => Ok((Request::ClipboardPaste, p)),
            _ => Err(ProtocolError::MalformedMessage),
        }
    }
}

// ── Encode for Event ──────────────────────────────────────────────────────

impl Encode for Event {
    fn encode(&self, buf: &mut [u8]) -> Result<usize, ProtocolError> {
        match self {
            Event::FrameDone {
                surface,
                timestamp_ms,
            } => {
                let p = put_u8(buf, 0, EVT_FRAME_DONE)?;
                let p = put_u32(buf, p, *surface)?;
                let p = put_u32(buf, p, *timestamp_ms)?;
                Ok(p)
            }
            Event::Configure {
                toplevel,
                width,
                height,
            } => {
                let p = put_u8(buf, 0, EVT_CONFIGURE)?;
                let p = put_u32(buf, p, *toplevel)?;
                let p = put_u32(buf, p, *width)?;
                let p = put_u32(buf, p, *height)?;
                Ok(p)
            }
            Event::Close { toplevel } => {
                let p = put_u8(buf, 0, EVT_CLOSE)?;
                let p = put_u32(buf, p, *toplevel)?;
                Ok(p)
            }
            Event::PointerEnter { surface, x, y } => {
                let p = put_u8(buf, 0, EVT_POINTER_ENTER)?;
                let p = put_u32(buf, p, *surface)?;
                let p = put_i32(buf, p, *x)?;
                let p = put_i32(buf, p, *y)?;
                Ok(p)
            }
            Event::PointerLeave { surface } => {
                let p = put_u8(buf, 0, EVT_POINTER_LEAVE)?;
                let p = put_u32(buf, p, *surface)?;
                Ok(p)
            }
            Event::PointerMotion { time, x, y } => {
                let p = put_u8(buf, 0, EVT_POINTER_MOTION)?;
                let p = put_u32(buf, p, *time)?;
                let p = put_i32(buf, p, *x)?;
                let p = put_i32(buf, p, *y)?;
                Ok(p)
            }
            Event::PointerButton {
                time,
                button,
                pressed,
            } => {
                let p = put_u8(buf, 0, EVT_POINTER_BUTTON)?;
                let p = put_u32(buf, p, *time)?;
                let p = put_u32(buf, p, *button)?;
                let p = put_bool(buf, p, *pressed)?;
                Ok(p)
            }
            Event::PointerAxis { time, axis, value } => {
                let p = put_u8(buf, 0, EVT_POINTER_AXIS)?;
                let p = put_u32(buf, p, *time)?;
                let p = put_u32(buf, p, *axis)?;
                let p = put_i32(buf, p, *value)?;
                Ok(p)
            }
            Event::KeyboardEnter { surface } => {
                let p = put_u8(buf, 0, EVT_KEYBOARD_ENTER)?;
                let p = put_u32(buf, p, *surface)?;
                Ok(p)
            }
            Event::KeyboardLeave { surface } => {
                let p = put_u8(buf, 0, EVT_KEYBOARD_LEAVE)?;
                let p = put_u32(buf, p, *surface)?;
                Ok(p)
            }
            Event::Key {
                time,
                scancode,
                ascii,
                pressed,
            } => {
                let p = put_u8(buf, 0, EVT_KEY)?;
                let p = put_u32(buf, p, *time)?;
                let p = put_u32(buf, p, *scancode)?;
                let p = put_u32(buf, p, *ascii)?;
                let p = put_bool(buf, p, *pressed)?;
                Ok(p)
            }
            Event::Modifiers { mods } => {
                let p = put_u8(buf, 0, EVT_MODIFIERS)?;
                let p = put_u32(buf, p, *mods)?;
                Ok(p)
            }
            Event::OutputInfo {
                width,
                height,
                format,
                pitch,
            } => {
                let p = put_u8(buf, 0, EVT_OUTPUT_INFO)?;
                let p = put_u32(buf, p, *width)?;
                let p = put_u32(buf, p, *height)?;
                let p = put_u32(buf, p, *format)?;
                let p = put_u32(buf, p, *pitch)?;
                Ok(p)
            }
            Event::PasteResult { data, len } => {
                let p = put_u8(buf, 0, EVT_PASTE_RESULT)?;
                let p = put_bytes(buf, p, data)?;
                let p = put_u16(buf, p, *len)?;
                Ok(p)
            }
            Event::Error { code } => {
                let p = put_u8(buf, 0, EVT_ERROR)?;
                let p = put_u32(buf, p, *code)?;
                Ok(p)
            }
        }
    }
}

// ── Decode for Event ──────────────────────────────────────────────────────

impl Decode for Event {
    fn decode(buf: &[u8]) -> Result<(Self, usize), ProtocolError> {
        let (tag, p) = get_u8(buf, 0)?;
        match tag {
            EVT_FRAME_DONE => {
                let (surface, p) = get_u32(buf, p)?;
                let (timestamp_ms, p) = get_u32(buf, p)?;
                Ok((
                    Event::FrameDone {
                        surface,
                        timestamp_ms,
                    },
                    p,
                ))
            }
            EVT_CONFIGURE => {
                let (toplevel, p) = get_u32(buf, p)?;
                let (width, p) = get_u32(buf, p)?;
                let (height, p) = get_u32(buf, p)?;
                Ok((
                    Event::Configure {
                        toplevel,
                        width,
                        height,
                    },
                    p,
                ))
            }
            EVT_CLOSE => {
                let (toplevel, p) = get_u32(buf, p)?;
                Ok((Event::Close { toplevel }, p))
            }
            EVT_POINTER_ENTER => {
                let (surface, p) = get_u32(buf, p)?;
                let (x, p) = get_i32(buf, p)?;
                let (y, p) = get_i32(buf, p)?;
                Ok((Event::PointerEnter { surface, x, y }, p))
            }
            EVT_POINTER_LEAVE => {
                let (surface, p) = get_u32(buf, p)?;
                Ok((Event::PointerLeave { surface }, p))
            }
            EVT_POINTER_MOTION => {
                let (time, p) = get_u32(buf, p)?;
                let (x, p) = get_i32(buf, p)?;
                let (y, p) = get_i32(buf, p)?;
                Ok((Event::PointerMotion { time, x, y }, p))
            }
            EVT_POINTER_BUTTON => {
                let (time, p) = get_u32(buf, p)?;
                let (button, p) = get_u32(buf, p)?;
                let (pressed, p) = get_bool(buf, p)?;
                Ok((
                    Event::PointerButton {
                        time,
                        button,
                        pressed,
                    },
                    p,
                ))
            }
            EVT_POINTER_AXIS => {
                let (time, p) = get_u32(buf, p)?;
                let (axis, p) = get_u32(buf, p)?;
                let (value, p) = get_i32(buf, p)?;
                Ok((Event::PointerAxis { time, axis, value }, p))
            }
            EVT_KEYBOARD_ENTER => {
                let (surface, p) = get_u32(buf, p)?;
                Ok((Event::KeyboardEnter { surface }, p))
            }
            EVT_KEYBOARD_LEAVE => {
                let (surface, p) = get_u32(buf, p)?;
                Ok((Event::KeyboardLeave { surface }, p))
            }
            EVT_KEY => {
                let (time, p) = get_u32(buf, p)?;
                let (scancode, p) = get_u32(buf, p)?;
                let (ascii, p) = get_u32(buf, p)?;
                let (pressed, p) = get_bool(buf, p)?;
                Ok((
                    Event::Key {
                        time,
                        scancode,
                        ascii,
                        pressed,
                    },
                    p,
                ))
            }
            EVT_MODIFIERS => {
                let (mods, p) = get_u32(buf, p)?;
                Ok((Event::Modifiers { mods }, p))
            }
            EVT_OUTPUT_INFO => {
                let (width, p) = get_u32(buf, p)?;
                let (height, p) = get_u32(buf, p)?;
                let (format, p) = get_u32(buf, p)?;
                let (pitch, p) = get_u32(buf, p)?;
                Ok((
                    Event::OutputInfo {
                        width,
                        height,
                        format,
                        pitch,
                    },
                    p,
                ))
            }
            EVT_PASTE_RESULT => {
                let (data, p) = get_bytes::<4096>(buf, p)?;
                let (len, p) = get_u16(buf, p)?;
                Ok((Event::PasteResult { data, len }, p))
            }
            EVT_ERROR => {
                let (code, p) = get_u32(buf, p)?;
                Ok((Event::Error { code }, p))
            }
            _ => Err(ProtocolError::MalformedMessage),
        }
    }
}
