use slopos_abi::PAGE_SIZE;

use crate::appkit::{self, Window, WindowedApp};
use crate::gfx::{self, DrawBuffer};
use crate::syscall::{UserSysInfo, core as sys_core};
use crate::theme::{COLOR_BACKGROUND, COLOR_TEXT};

const SYSINFO_WIDTH: u32 = 360;
const SYSINFO_HEIGHT: u32 = 258;
const MARGIN_X: i32 = 12;
const MARGIN_Y: i32 = 12;
const LINE_HEIGHT: i32 = 18;

pub struct SysinfoApp;

impl WindowedApp for SysinfoApp {
    fn init(&mut self, win: &mut Window) {
        win.set_title("Sysinfo");
        win.request_redraw();
    }

    fn draw(&mut self, fb: &mut DrawBuffer<'_>) {
        let width = fb.width() as i32;
        let height = fb.height() as i32;
        gfx::fill_rect(fb, 0, 0, width, height, COLOR_BACKGROUND);

        let cpu_count = sys_core::get_cpu_count() as u64;
        let current_cpu = sys_core::get_current_cpu() as u64;
        let mut info = UserSysInfo::default();
        let sys_rc = sys_core::sys_info(&mut info);

        let mut y = MARGIN_Y;

        draw_text(fb, MARGIN_X, y, "SLOPOS SYSINFO");
        y += LINE_HEIGHT;

        draw_text(fb, MARGIN_X, y, &format!("CPUs available: {}", cpu_count));
        y += LINE_HEIGHT;

        draw_text(fb, MARGIN_X, y, &format!("Current CPU: {}", current_cpu));
        y += LINE_HEIGHT;

        if sys_rc == 0 {
            let total_mib = (info.total_pages as u64).saturating_mul(PAGE_SIZE) / (1024 * 1024);
            let free_mib = (info.free_pages as u64).saturating_mul(PAGE_SIZE) / (1024 * 1024);
            let alloc_mib = (info.allocated_pages as u64).saturating_mul(PAGE_SIZE) / (1024 * 1024);

            draw_text(fb, MARGIN_X, y, &format!("Memory total: {} MiB", total_mib));
            y += LINE_HEIGHT;
            draw_text(fb, MARGIN_X, y, &format!("Memory free: {} MiB", free_mib));
            y += LINE_HEIGHT;
            draw_text(fb, MARGIN_X, y, &format!("Memory alloc: {} MiB", alloc_mib));
            y += LINE_HEIGHT;
            draw_text(
                fb,
                MARGIN_X,
                y,
                &format!("Tasks total: {}", info.total_tasks as u64),
            );
            y += LINE_HEIGHT;
            draw_text(
                fb,
                MARGIN_X,
                y,
                &format!("Tasks active: {}", info.active_tasks as u64),
            );
            y += LINE_HEIGHT;
            draw_text(
                fb,
                MARGIN_X,
                y,
                &format!("Tasks ready: {}", info.ready_tasks as u64),
            );
            y += LINE_HEIGHT;
            draw_text(
                fb,
                MARGIN_X,
                y,
                &format!("Task ctx switches: {}", info.task_context_switches),
            );
            y += LINE_HEIGHT;
            draw_text(
                fb,
                MARGIN_X,
                y,
                &format!("Scheduler switches: {}", info.scheduler_context_switches),
            );
            y += LINE_HEIGHT;
            draw_text(
                fb,
                MARGIN_X,
                y,
                &format!("Scheduler yields: {}", info.scheduler_yields),
            );
            y += LINE_HEIGHT;
        } else {
            draw_text(fb, MARGIN_X, y, "System info: unavailable");
            y += LINE_HEIGHT;
        }

        draw_text(fb, MARGIN_X, y, "Drivers: kernel-managed");
    }
}

pub fn sysinfo_main() -> ! {
    appkit::run(SysinfoApp, SYSINFO_WIDTH, SYSINFO_HEIGHT)
}

fn draw_text(fb: &mut DrawBuffer<'_>, x: i32, y: i32, text: &str) {
    gfx::font::draw_string(fb, x, y, text, COLOR_TEXT, COLOR_BACKGROUND);
}
