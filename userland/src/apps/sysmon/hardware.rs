use std::format;

use slopos_abi::PAGE_SIZE;
use slopos_slibc::mem::malloc::heap_stats;

use crate::gfx::DrawBuffer;
use crate::gfx::draw_rect;
use crate::gfx::font;

use super::{
    COLOR_BG, COLOR_BRIGHT, COLOR_DIM, COLOR_STATE_BLOCK, COLOR_STATE_RUN, COLOR_TEXT, SysmonApp,
    format_bytes_mib, format_cpu_features, format_number, format_uptime, trim_ascii,
};

impl SysmonApp {
    pub(crate) fn draw_hardware(&self, fb: &mut DrawBuffer<'_>, width: i32, _height: i32) {
        let mut y = super::TAB_HEIGHT + 8;
        let lx = 10;
        let vx = 130;
        let bar_w = (width - 170).max(80);

        self.draw_section_title(fb, lx, y, "PROCESSOR");
        y += font::cell_height() + 2;
        Self::text(fb, lx + 4, y, "Model", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &trim_ascii(&self.cpu_info.brand_string),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height();
        Self::text(fb, lx + 4, y, "Vendor", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &trim_ascii(&self.cpu_info.vendor),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height();
        Self::text(fb, lx + 4, y, "Cores", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format!("{}", self.cpu_info.cpu_count),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height();
        Self::text(fb, lx + 4, y, "Family/Model", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format!(
                "{} / {} / step {}",
                self.cpu_info.family, self.cpu_info.model, self.cpu_info.stepping
            ),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height();
        Self::text(fb, lx + 4, y, "Features", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_cpu_features(self.cpu_info.features),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height() + 6;

        draw_rect(fb, lx, y, width - 20, 1, COLOR_DIM);
        y += 6;

        self.draw_section_title(fb, lx, y, "MEMORY");
        y += font::cell_height() + 2;
        let total_bytes = (self.sys_info.total_pages as u64).saturating_mul(PAGE_SIZE);
        let used_bytes = (self.sys_info.allocated_pages.min(self.sys_info.total_pages) as u64)
            .saturating_mul(PAGE_SIZE);
        let free_bytes = (self.sys_info.free_pages as u64).saturating_mul(PAGE_SIZE);
        let mem_pct = if total_bytes == 0 {
            0
        } else {
            ((used_bytes.saturating_mul(100) / total_bytes).min(100)) as u32
        };
        let mem_bar = format!(
            "{} / {} ({}%)",
            format_bytes_mib(used_bytes),
            format_bytes_mib(total_bytes),
            mem_pct
        );
        self.draw_usage_bar(fb, lx, y, bar_w, 14, mem_pct, &mem_bar);
        y += 18;
        Self::text(fb, lx + 4, y, "Total", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format!(
                "{} ({} pages)",
                format_bytes_mib(total_bytes),
                self.sys_info.total_pages
            ),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height();
        Self::text(fb, lx + 4, y, "Free", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_bytes_mib(free_bytes),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height();
        Self::text(fb, lx + 4, y, "Allocated", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_bytes_mib(used_bytes),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height() + 6;

        draw_rect(fb, lx, y, width - 20, 1, COLOR_DIM);
        y += 6;

        self.draw_section_title(fb, lx, y, "SCHEDULER");
        y += font::cell_height() + 2;
        Self::text(fb, lx + 4, y, "Ctx switches", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_number(self.sys_info.task_context_switches),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height();
        Self::text(fb, lx + 4, y, "Sched switches", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_number(self.sys_info.scheduler_context_switches),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height();
        Self::text(fb, lx + 4, y, "Yields", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_number(self.sys_info.scheduler_yields),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height();
        Self::text(fb, lx + 4, y, "Schedule calls", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_number(self.sys_info.schedule_calls as u64),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height() + 6;

        draw_rect(fb, lx, y, width - 20, 1, COLOR_DIM);
        y += 6;

        self.draw_section_title(fb, lx, y, "HEAP");
        y += font::cell_height() + 2;
        let stats = heap_stats();
        Self::text(fb, lx + 4, y, "Heap size", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_bytes_mib(stats.heap_size as u64),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height();
        Self::text(fb, lx + 4, y, "Wilderness", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_bytes_mib(stats.wilderness as u64),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height();
        Self::text(fb, lx + 4, y, "Mmap allocs", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_number(stats.mmap_count as u64),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height() + 6;

        draw_rect(fb, lx, y, width - 20, 1, COLOR_DIM);
        y += 6;

        self.draw_section_title(fb, lx, y, "NETWORK");
        y += font::cell_height() + 2;
        Self::text(fb, lx + 4, y, "Status", COLOR_DIM, COLOR_BG);
        let net_status = if self.net_info.nic_ready != 0 {
            if self.net_info.link_up != 0 {
                "Online"
            } else {
                "No link"
            }
        } else {
            "Offline"
        };
        let status_color = if self.net_info.link_up != 0 {
            COLOR_STATE_RUN
        } else {
            COLOR_STATE_BLOCK
        };
        Self::text(fb, vx, y, net_status, status_color, COLOR_BG);
        y += font::cell_height();
        Self::text(fb, lx + 4, y, "IP", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format!(
                "{}.{}.{}.{}",
                self.net_info.ipv4[0],
                self.net_info.ipv4[1],
                self.net_info.ipv4[2],
                self.net_info.ipv4[3]
            ),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height();
        Self::text(fb, lx + 4, y, "MAC", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format!(
                "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                self.net_info.mac[0],
                self.net_info.mac[1],
                self.net_info.mac[2],
                self.net_info.mac[3],
                self.net_info.mac[4],
                self.net_info.mac[5]
            ),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height() + 6;

        draw_rect(fb, lx, y, width - 20, 1, COLOR_DIM);
        y += 6;

        self.draw_section_title(fb, lx, y, "BOOT");
        y += font::cell_height() + 2;
        Self::text(fb, lx + 4, y, "Uptime", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_uptime(self.last_refresh_ms),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height();
        Self::text(fb, lx + 4, y, "Boot flags", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format!("0x{:08x}", self.sys_info.boot_flags),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height();
        Self::text(fb, lx + 4, y, "W/L Balance", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_number(self.sys_info.wl_balance as u64),
            COLOR_BRIGHT,
            COLOR_BG,
        );
    }
}
