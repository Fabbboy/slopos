use core::ffi::c_void;

use slopos_abi::arch::x86_64::paging::PAGE_SIZE_4KB;

use crate::gfx::{self, DrawBuffer, PixelFormat};
use crate::syscall::{
    sys_fb_info, sys_get_cpu_count, sys_get_current_cpu, sys_surface_commit, sys_surface_set_title,
    sys_sys_info, sys_write, sys_yield, DisplayInfo, ShmBuffer, UserSysInfo,
};
use crate::theme::{COLOR_BACKGROUND, COLOR_TEXT};

const SYSINFO_WIDTH: i32 = 360;
const SYSINFO_HEIGHT: i32 = 220;
const SYSINFO_MARGIN_X: i32 = 12;
const SYSINFO_MARGIN_Y: i32 = 12;
const SYSINFO_LINE_HEIGHT: i32 = 18;

pub struct SysinfoApp {
    shm_buffer: Option<ShmBuffer>,
    width: i32,
    height: i32,
    pitch: usize,
    bytes_pp: u8,
    pixel_format: PixelFormat,
}

impl SysinfoApp {
    pub fn new() -> Self {
        Self {
            shm_buffer: None,
            width: SYSINFO_WIDTH,
            height: SYSINFO_HEIGHT,
            pitch: 0,
            bytes_pp: 4,
            pixel_format: PixelFormat::Bgra,
        }
    }

    fn init_surface(&mut self) -> bool {
        let mut fb_info = DisplayInfo::default();
        if sys_fb_info(&mut fb_info) != 0 {
            return false;
        }

        self.bytes_pp = fb_info.bytes_per_pixel();
        self.pitch = (self.width as usize) * (self.bytes_pp as usize);
        self.pixel_format = if fb_info.format.is_bgr_order() {
            PixelFormat::Bgra
        } else {
            PixelFormat::Rgba
        };

        let buffer_size = self.pitch * (self.height as usize);
        let shm = match ShmBuffer::create(buffer_size) {
            Ok(buf) => buf,
            Err(_) => return false,
        };

        if shm
            .attach_surface(self.width as u32, self.height as u32)
            .is_err()
        {
            return false;
        }

        sys_surface_set_title("Sysinfo");
        self.shm_buffer = Some(shm);
        true
    }

    fn draw_buffer(&mut self) -> Option<DrawBuffer<'_>> {
        let shm = self.shm_buffer.as_mut()?;
        let mut buf = DrawBuffer::new(
            shm.as_mut_slice(),
            self.width as u32,
            self.height as u32,
            self.pitch,
            self.bytes_pp,
        )?;
        buf.set_pixel_format(self.pixel_format);
        Some(buf)
    }

    fn draw_text_line(buf: &mut DrawBuffer<'_>, x: i32, y: i32, text: &str) {
        gfx::font::draw_string(buf, x, y, text, COLOR_TEXT, COLOR_BACKGROUND);
    }

    fn format_line<'a>(buf: &'a mut [u8; 96], label: &str, value: u64, suffix: &str) -> &'a str {
        let mut idx = 0usize;
        idx = copy_str(buf, idx, label);
        idx = write_u64(buf, idx, value);
        idx = copy_str(buf, idx, suffix);
        unsafe { core::str::from_utf8_unchecked(&buf[..idx]) }
    }

    fn draw_info(&mut self) {
        let width = self.width;
        let height = self.height;
        let mut buf = match self.draw_buffer() {
            Some(buf) => buf,
            None => return,
        };

        gfx::fill_rect(&mut buf, 0, 0, width, height, COLOR_BACKGROUND);

        let cpu_count = sys_get_cpu_count() as u64;
        let current_cpu = sys_get_current_cpu() as u64;
        let mut info = UserSysInfo::default();
        let sys_rc = sys_sys_info(&mut info);

        let mut line = [0u8; 96];
        let mut y = SYSINFO_MARGIN_Y;

        Self::draw_text_line(&mut buf, SYSINFO_MARGIN_X, y, "SLOPOS SYSINFO");
        y += SYSINFO_LINE_HEIGHT;

        let line_str = Self::format_line(&mut line, "CPUs available: ", cpu_count, "");
        Self::draw_text_line(&mut buf, SYSINFO_MARGIN_X, y, line_str);
        y += SYSINFO_LINE_HEIGHT;

        let line_str = Self::format_line(&mut line, "Current CPU: ", current_cpu, "");
        Self::draw_text_line(&mut buf, SYSINFO_MARGIN_X, y, line_str);
        y += SYSINFO_LINE_HEIGHT;

        if sys_rc == 0 {
            let total_mib = pages_to_mib(info.total_pages as u64);
            let free_mib = pages_to_mib(info.free_pages as u64);
            let alloc_mib = pages_to_mib(info.allocated_pages as u64);

            let line_str = Self::format_line(&mut line, "Memory total: ", total_mib, " MiB");
            Self::draw_text_line(&mut buf, SYSINFO_MARGIN_X, y, line_str);
            y += SYSINFO_LINE_HEIGHT;

            let line_str = Self::format_line(&mut line, "Memory free: ", free_mib, " MiB");
            Self::draw_text_line(&mut buf, SYSINFO_MARGIN_X, y, line_str);
            y += SYSINFO_LINE_HEIGHT;

            let line_str = Self::format_line(&mut line, "Memory alloc: ", alloc_mib, " MiB");
            Self::draw_text_line(&mut buf, SYSINFO_MARGIN_X, y, line_str);
            y += SYSINFO_LINE_HEIGHT;

            let line_str =
                Self::format_line(&mut line, "Tasks total: ", info.total_tasks as u64, "");
            Self::draw_text_line(&mut buf, SYSINFO_MARGIN_X, y, line_str);
            y += SYSINFO_LINE_HEIGHT;

            let line_str =
                Self::format_line(&mut line, "Tasks active: ", info.active_tasks as u64, "");
            Self::draw_text_line(&mut buf, SYSINFO_MARGIN_X, y, line_str);
            y += SYSINFO_LINE_HEIGHT;

            let line_str =
                Self::format_line(&mut line, "Tasks ready: ", info.ready_tasks as u64, "");
            Self::draw_text_line(&mut buf, SYSINFO_MARGIN_X, y, line_str);
            y += SYSINFO_LINE_HEIGHT;

            let line_str = Self::format_line(
                &mut line,
                "Task ctx switches: ",
                info.task_context_switches,
                "",
            );
            Self::draw_text_line(&mut buf, SYSINFO_MARGIN_X, y, line_str);
            y += SYSINFO_LINE_HEIGHT;

            let line_str = Self::format_line(
                &mut line,
                "Scheduler switches: ",
                info.scheduler_context_switches,
                "",
            );
            Self::draw_text_line(&mut buf, SYSINFO_MARGIN_X, y, line_str);
            y += SYSINFO_LINE_HEIGHT;

            let line_str =
                Self::format_line(&mut line, "Scheduler yields: ", info.scheduler_yields, "");
            Self::draw_text_line(&mut buf, SYSINFO_MARGIN_X, y, line_str);
            y += SYSINFO_LINE_HEIGHT;
        } else {
            Self::draw_text_line(&mut buf, SYSINFO_MARGIN_X, y, "System info: unavailable");
            y += SYSINFO_LINE_HEIGHT;
        }

        Self::draw_text_line(&mut buf, SYSINFO_MARGIN_X, y, "Drivers: kernel-managed");
    }
}

#[unsafe(link_section = ".user_text")]
pub fn sysinfo_main(_arg: *mut c_void) {
    let mut app = SysinfoApp::new();
    if !app.init_surface() {
        let _ = sys_write(b"sysinfo: framebuffer unavailable\n");
        return;
    }

    app.draw_info();
    let _ = sys_surface_commit();

    loop {
        sys_yield();
    }
}

fn pages_to_mib(pages: u64) -> u64 {
    pages.saturating_mul(PAGE_SIZE_4KB) / (1024 * 1024)
}

fn copy_str(buf: &mut [u8; 96], mut idx: usize, value: &str) -> usize {
    for &b in value.as_bytes() {
        if idx >= buf.len() {
            break;
        }
        buf[idx] = b;
        idx += 1;
    }
    idx
}

fn write_u64(buf: &mut [u8; 96], mut idx: usize, mut value: u64) -> usize {
    let mut tmp = [0u8; 32];
    let mut len = 0usize;

    if value == 0 {
        tmp[0] = b'0';
        len = 1;
    } else {
        while value != 0 && len < tmp.len() {
            tmp[len] = b'0' + (value % 10) as u8;
            value /= 10;
            len += 1;
        }
        tmp[..len].reverse();
    }

    for &b in &tmp[..len] {
        if idx >= buf.len() {
            break;
        }
        buf[idx] = b;
        idx += 1;
    }
    idx
}
