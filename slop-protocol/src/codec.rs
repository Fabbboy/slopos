//! Binary codec: Encode/Decode traits and implementations for Request/Event.
//!
//! Wire format per message (framing handled by Connection):
//! `[tag: u8][field1][field2]...[fieldN]`
//!
//! All fields are little-endian. No padding, no alignment.
//! Strings: `[u8 len][utf8 bytes]` (max 128 bytes).
//! Clipboard: `[u16 len][bytes]` (max 4096 bytes).

use crate::types::{ClipboardData, Event, MAX_STRING_LEN, ProtocolError, Request};

/// Serialize into a byte buffer. Returns number of bytes written.
pub trait Encode {
    fn encode(&self, buf: &mut [u8]) -> Result<usize, ProtocolError>;
}

/// Deserialize from a byte buffer. Returns (value, bytes_consumed).
pub trait Decode: Sized {
    fn decode(buf: &[u8]) -> Result<(Self, usize), ProtocolError>;
}

// -- Primitive helpers ----------------------------------------------------

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

fn put_u64(buf: &mut [u8], pos: usize, v: u64) -> Result<usize, ProtocolError> {
    if pos + 8 > buf.len() {
        return Err(ProtocolError::MessageTooLarge);
    }
    buf[pos..pos + 8].copy_from_slice(&v.to_le_bytes());
    Ok(pos + 8)
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

fn get_u64(buf: &[u8], pos: usize) -> Result<(u64, usize), ProtocolError> {
    if pos + 8 > buf.len() {
        return Err(ProtocolError::MalformedMessage);
    }
    let v = u64::from_le_bytes([
        buf[pos],
        buf[pos + 1],
        buf[pos + 2],
        buf[pos + 3],
        buf[pos + 4],
        buf[pos + 5],
        buf[pos + 6],
        buf[pos + 7],
    ]);
    Ok((v, pos + 8))
}

fn get_i32(buf: &[u8], pos: usize) -> Result<(i32, usize), ProtocolError> {
    let (v, p) = get_u32(buf, pos)?;
    Ok((v as i32, p))
}

fn get_bool(buf: &[u8], pos: usize) -> Result<(bool, usize), ProtocolError> {
    let (v, p) = get_u8(buf, pos)?;
    Ok((v != 0, p))
}

// -- Request tag constants ------------------------------------------------

const REQ_HELLO: u8 = 0;
const REQ_CREATE_SURFACE: u8 = 1;
const REQ_SURFACE_ATTACH: u8 = 2;
const REQ_SURFACE_DAMAGE: u8 = 3;
const REQ_SURFACE_COMMIT: u8 = 4;
const REQ_SURFACE_FRAME: u8 = 5;
const REQ_SURFACE_DESTROY: u8 = 6;
const REQ_GET_TOPLEVEL: u8 = 7;
const REQ_TOPLEVEL_SET_TITLE: u8 = 8;
const REQ_TOPLEVEL_SET_APP_ID: u8 = 9;
const REQ_TOPLEVEL_DESTROY: u8 = 10;
const REQ_ACK_CONFIGURE: u8 = 11;
const REQ_SET_CURSOR_SHAPE: u8 = 12;
const REQ_CLIPBOARD_COPY: u8 = 13;
const REQ_CLIPBOARD_PASTE: u8 = 14;
const REQ_INTERACTIVE_MOVE: u8 = 15;
const REQ_INTERACTIVE_RESIZE: u8 = 16;

// -- Event tag constants --------------------------------------------------

const EVT_HELLO: u8 = 0;
const EVT_OBJECT_DESTROYED: u8 = 1;
const EVT_OUTPUT_INFO: u8 = 2;
const EVT_FRAME_DONE: u8 = 3;
const EVT_CONFIGURE: u8 = 4;
const EVT_CLOSE: u8 = 5;
const EVT_POINTER_ENTER: u8 = 6;
const EVT_POINTER_LEAVE: u8 = 7;
const EVT_POINTER_MOTION: u8 = 8;
const EVT_POINTER_BUTTON: u8 = 9;
const EVT_POINTER_AXIS: u8 = 10;
const EVT_KEYBOARD_ENTER: u8 = 11;
const EVT_KEYBOARD_LEAVE: u8 = 12;
const EVT_KEY: u8 = 13;
const EVT_MODIFIERS: u8 = 14;
const EVT_PASTE_RESULT: u8 = 15;
const EVT_ERROR: u8 = 16;

// -- Encode for Request ---------------------------------------------------

impl Encode for Request {
    fn encode(&self, buf: &mut [u8]) -> Result<usize, ProtocolError> {
        match self {
            Request::Hello { version } => {
                let p = put_u8(buf, 0, REQ_HELLO)?;
                put_u32(buf, p, *version)
            }
            Request::CreateSurface { new_id } => {
                let p = put_u8(buf, 0, REQ_CREATE_SURFACE)?;
                put_u32(buf, p, *new_id)
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
                put_u32(buf, p, *height)
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
                put_i32(buf, p, *h)
            }
            Request::SurfaceCommit { surface } => {
                let p = put_u8(buf, 0, REQ_SURFACE_COMMIT)?;
                put_u32(buf, p, *surface)
            }
            Request::SurfaceFrame { surface } => {
                let p = put_u8(buf, 0, REQ_SURFACE_FRAME)?;
                put_u32(buf, p, *surface)
            }
            Request::SurfaceDestroy { surface } => {
                let p = put_u8(buf, 0, REQ_SURFACE_DESTROY)?;
                put_u32(buf, p, *surface)
            }
            Request::GetToplevel { surface, new_id } => {
                let p = put_u8(buf, 0, REQ_GET_TOPLEVEL)?;
                let p = put_u32(buf, p, *surface)?;
                put_u32(buf, p, *new_id)
            }
            Request::ToplevelSetTitle {
                toplevel,
                title,
                len,
            } => {
                let actual = (*len as usize).min(MAX_STRING_LEN);
                let p = put_u8(buf, 0, REQ_TOPLEVEL_SET_TITLE)?;
                let p = put_u32(buf, p, *toplevel)?;
                let p = put_u8(buf, p, actual as u8)?;
                put_bytes(buf, p, &title[..actual])
            }
            Request::ToplevelSetAppId {
                toplevel,
                app_id,
                len,
            } => {
                let actual = (*len as usize).min(MAX_STRING_LEN);
                let p = put_u8(buf, 0, REQ_TOPLEVEL_SET_APP_ID)?;
                let p = put_u32(buf, p, *toplevel)?;
                let p = put_u8(buf, p, actual as u8)?;
                put_bytes(buf, p, &app_id[..actual])
            }
            Request::ToplevelDestroy { toplevel } => {
                let p = put_u8(buf, 0, REQ_TOPLEVEL_DESTROY)?;
                put_u32(buf, p, *toplevel)
            }
            Request::AckConfigure { serial } => {
                let p = put_u8(buf, 0, REQ_ACK_CONFIGURE)?;
                put_u32(buf, p, *serial)
            }
            Request::SetCursorShape { surface, shape } => {
                let p = put_u8(buf, 0, REQ_SET_CURSOR_SHAPE)?;
                let p = put_u32(buf, p, *surface)?;
                put_u8(buf, p, *shape)
            }
            Request::ClipboardCopy(cb) => {
                let actual = (cb.len as usize).min(cb.data.len());
                let p = put_u8(buf, 0, REQ_CLIPBOARD_COPY)?;
                let p = put_u16(buf, p, actual as u16)?;
                put_bytes(buf, p, &cb.data[..actual])
            }
            Request::ClipboardPaste => put_u8(buf, 0, REQ_CLIPBOARD_PASTE),
            Request::InteractiveMove { toplevel, serial } => {
                let p = put_u8(buf, 0, REQ_INTERACTIVE_MOVE)?;
                let p = put_u32(buf, p, *toplevel)?;
                put_u32(buf, p, *serial)
            }
            Request::InteractiveResize {
                toplevel,
                serial,
                edges,
            } => {
                let p = put_u8(buf, 0, REQ_INTERACTIVE_RESIZE)?;
                let p = put_u32(buf, p, *toplevel)?;
                let p = put_u32(buf, p, *serial)?;
                put_u32(buf, p, *edges)
            }
        }
    }
}

// -- Decode for Request ---------------------------------------------------

impl Decode for Request {
    fn decode(buf: &[u8]) -> Result<(Self, usize), ProtocolError> {
        let (tag, p) = get_u8(buf, 0)?;
        match tag {
            REQ_HELLO => {
                let (version, p) = get_u32(buf, p)?;
                Ok((Request::Hello { version }, p))
            }
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
                let (raw_len, p) = get_u8(buf, p)?;
                let len = (raw_len as usize).min(MAX_STRING_LEN);
                let mut title = [0u8; MAX_STRING_LEN];
                if p + len > buf.len() {
                    return Err(ProtocolError::MalformedMessage);
                }
                title[..len].copy_from_slice(&buf[p..p + len]);
                Ok((
                    Request::ToplevelSetTitle {
                        toplevel,
                        title,
                        len: len as u8,
                    },
                    p + len,
                ))
            }
            REQ_TOPLEVEL_SET_APP_ID => {
                let (toplevel, p) = get_u32(buf, p)?;
                let (raw_len, p) = get_u8(buf, p)?;
                let len = (raw_len as usize).min(MAX_STRING_LEN);
                let mut app_id = [0u8; MAX_STRING_LEN];
                if p + len > buf.len() {
                    return Err(ProtocolError::MalformedMessage);
                }
                app_id[..len].copy_from_slice(&buf[p..p + len]);
                Ok((
                    Request::ToplevelSetAppId {
                        toplevel,
                        app_id,
                        len: len as u8,
                    },
                    p + len,
                ))
            }
            REQ_TOPLEVEL_DESTROY => {
                let (toplevel, p) = get_u32(buf, p)?;
                Ok((Request::ToplevelDestroy { toplevel }, p))
            }
            REQ_ACK_CONFIGURE => {
                let (serial, p) = get_u32(buf, p)?;
                Ok((Request::AckConfigure { serial }, p))
            }
            REQ_SET_CURSOR_SHAPE => {
                let (surface, p) = get_u32(buf, p)?;
                let (shape, p) = get_u8(buf, p)?;
                Ok((Request::SetCursorShape { surface, shape }, p))
            }
            REQ_CLIPBOARD_COPY => {
                let (raw_len, p) = get_u16(buf, p)?;
                let len = (raw_len as usize).min(4096);
                let mut data = [0u8; 4096];
                if p + len > buf.len() {
                    return Err(ProtocolError::MalformedMessage);
                }
                data[..len].copy_from_slice(&buf[p..p + len]);
                Ok((
                    Request::ClipboardCopy(alloc::boxed::Box::new(ClipboardData {
                        data,
                        len: len as u16,
                    })),
                    p + len,
                ))
            }
            REQ_CLIPBOARD_PASTE => Ok((Request::ClipboardPaste, p)),
            REQ_INTERACTIVE_MOVE => {
                let (toplevel, p) = get_u32(buf, p)?;
                let (serial, p) = get_u32(buf, p)?;
                Ok((Request::InteractiveMove { toplevel, serial }, p))
            }
            REQ_INTERACTIVE_RESIZE => {
                let (toplevel, p) = get_u32(buf, p)?;
                let (serial, p) = get_u32(buf, p)?;
                let (edges, p) = get_u32(buf, p)?;
                Ok((
                    Request::InteractiveResize {
                        toplevel,
                        serial,
                        edges,
                    },
                    p,
                ))
            }
            _ => Err(ProtocolError::MalformedMessage),
        }
    }
}

// -- Encode for Event -----------------------------------------------------

impl Encode for Event {
    fn encode(&self, buf: &mut [u8]) -> Result<usize, ProtocolError> {
        match self {
            Event::Hello {
                version,
                capabilities,
            } => {
                let p = put_u8(buf, 0, EVT_HELLO)?;
                let p = put_u32(buf, p, *version)?;
                put_u64(buf, p, *capabilities)
            }
            Event::ObjectDestroyed { id } => {
                let p = put_u8(buf, 0, EVT_OBJECT_DESTROYED)?;
                put_u32(buf, p, *id)
            }
            Event::OutputInfo {
                width,
                height,
                format,
                pitch,
                scale,
            } => {
                let p = put_u8(buf, 0, EVT_OUTPUT_INFO)?;
                let p = put_u32(buf, p, *width)?;
                let p = put_u32(buf, p, *height)?;
                let p = put_u32(buf, p, *format)?;
                let p = put_u32(buf, p, *pitch)?;
                put_u32(buf, p, *scale)
            }
            Event::FrameDone {
                surface,
                timestamp_ms,
            } => {
                let p = put_u8(buf, 0, EVT_FRAME_DONE)?;
                let p = put_u32(buf, p, *surface)?;
                put_u32(buf, p, *timestamp_ms)
            }
            Event::Configure {
                toplevel,
                serial,
                width,
                height,
                states,
            } => {
                let p = put_u8(buf, 0, EVT_CONFIGURE)?;
                let p = put_u32(buf, p, *toplevel)?;
                let p = put_u32(buf, p, *serial)?;
                let p = put_u32(buf, p, *width)?;
                let p = put_u32(buf, p, *height)?;
                put_u32(buf, p, *states)
            }
            Event::Close { toplevel } => {
                let p = put_u8(buf, 0, EVT_CLOSE)?;
                put_u32(buf, p, *toplevel)
            }
            Event::PointerEnter { surface, x, y } => {
                let p = put_u8(buf, 0, EVT_POINTER_ENTER)?;
                let p = put_u32(buf, p, *surface)?;
                let p = put_i32(buf, p, *x)?;
                put_i32(buf, p, *y)
            }
            Event::PointerLeave { surface } => {
                let p = put_u8(buf, 0, EVT_POINTER_LEAVE)?;
                put_u32(buf, p, *surface)
            }
            Event::PointerMotion { time, x, y } => {
                let p = put_u8(buf, 0, EVT_POINTER_MOTION)?;
                let p = put_u32(buf, p, *time)?;
                let p = put_i32(buf, p, *x)?;
                put_i32(buf, p, *y)
            }
            Event::PointerButton {
                serial,
                time,
                button,
                pressed,
            } => {
                let p = put_u8(buf, 0, EVT_POINTER_BUTTON)?;
                let p = put_u32(buf, p, *serial)?;
                let p = put_u32(buf, p, *time)?;
                let p = put_u32(buf, p, *button)?;
                put_bool(buf, p, *pressed)
            }
            Event::PointerAxis { time, axis, value } => {
                let p = put_u8(buf, 0, EVT_POINTER_AXIS)?;
                let p = put_u32(buf, p, *time)?;
                let p = put_u32(buf, p, *axis)?;
                put_i32(buf, p, *value)
            }
            Event::KeyboardEnter { surface } => {
                let p = put_u8(buf, 0, EVT_KEYBOARD_ENTER)?;
                put_u32(buf, p, *surface)
            }
            Event::KeyboardLeave { surface } => {
                let p = put_u8(buf, 0, EVT_KEYBOARD_LEAVE)?;
                put_u32(buf, p, *surface)
            }
            Event::Key {
                serial,
                time,
                scancode,
                ascii,
                pressed,
            } => {
                let p = put_u8(buf, 0, EVT_KEY)?;
                let p = put_u32(buf, p, *serial)?;
                let p = put_u32(buf, p, *time)?;
                let p = put_u32(buf, p, *scancode)?;
                let p = put_u32(buf, p, *ascii)?;
                put_bool(buf, p, *pressed)
            }
            Event::Modifiers { mods } => {
                let p = put_u8(buf, 0, EVT_MODIFIERS)?;
                put_u32(buf, p, *mods)
            }
            Event::PasteResult(cb) => {
                let actual = (cb.len as usize).min(cb.data.len());
                let p = put_u8(buf, 0, EVT_PASTE_RESULT)?;
                let p = put_u16(buf, p, actual as u16)?;
                put_bytes(buf, p, &cb.data[..actual])
            }
            Event::Error { object_id, code } => {
                let p = put_u8(buf, 0, EVT_ERROR)?;
                let p = put_u32(buf, p, *object_id)?;
                put_u32(buf, p, *code)
            }
        }
    }
}

// -- Decode for Event -----------------------------------------------------

impl Decode for Event {
    fn decode(buf: &[u8]) -> Result<(Self, usize), ProtocolError> {
        let (tag, p) = get_u8(buf, 0)?;
        match tag {
            EVT_HELLO => {
                let (version, p) = get_u32(buf, p)?;
                let (capabilities, p) = get_u64(buf, p)?;
                Ok((
                    Event::Hello {
                        version,
                        capabilities,
                    },
                    p,
                ))
            }
            EVT_OBJECT_DESTROYED => {
                let (id, p) = get_u32(buf, p)?;
                Ok((Event::ObjectDestroyed { id }, p))
            }
            EVT_OUTPUT_INFO => {
                let (width, p) = get_u32(buf, p)?;
                let (height, p) = get_u32(buf, p)?;
                let (format, p) = get_u32(buf, p)?;
                let (pitch, p) = get_u32(buf, p)?;
                let (scale, p) = get_u32(buf, p)?;
                Ok((
                    Event::OutputInfo {
                        width,
                        height,
                        format,
                        pitch,
                        scale,
                    },
                    p,
                ))
            }
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
                let (serial, p) = get_u32(buf, p)?;
                let (width, p) = get_u32(buf, p)?;
                let (height, p) = get_u32(buf, p)?;
                let (states, p) = get_u32(buf, p)?;
                Ok((
                    Event::Configure {
                        toplevel,
                        serial,
                        width,
                        height,
                        states,
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
                let (serial, p) = get_u32(buf, p)?;
                let (time, p) = get_u32(buf, p)?;
                let (button, p) = get_u32(buf, p)?;
                let (pressed, p) = get_bool(buf, p)?;
                Ok((
                    Event::PointerButton {
                        serial,
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
                let (serial, p) = get_u32(buf, p)?;
                let (time, p) = get_u32(buf, p)?;
                let (scancode, p) = get_u32(buf, p)?;
                let (ascii, p) = get_u32(buf, p)?;
                let (pressed, p) = get_bool(buf, p)?;
                Ok((
                    Event::Key {
                        serial,
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
            EVT_PASTE_RESULT => {
                let (raw_len, p) = get_u16(buf, p)?;
                let len = (raw_len as usize).min(4096);
                let mut data = [0u8; 4096];
                if p + len > buf.len() {
                    return Err(ProtocolError::MalformedMessage);
                }
                data[..len].copy_from_slice(&buf[p..p + len]);
                Ok((
                    Event::PasteResult(alloc::boxed::Box::new(ClipboardData {
                        data,
                        len: len as u16,
                    })),
                    p + len,
                ))
            }
            EVT_ERROR => {
                let (object_id, p) = get_u32(buf, p)?;
                let (code, p) = get_u32(buf, p)?;
                Ok((Event::Error { object_id, code }, p))
            }
            _ => Err(ProtocolError::MalformedMessage),
        }
    }
}
