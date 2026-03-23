use std::format;

use slopos_abi::PAGE_SIZE;

use crate::gfx::DrawBuffer;
use crate::gfx::font;

use super::{
    COLOR_BG, COLOR_TEXT, SysmonApp, format_bytes_mib, format_pct, format_uptime, task_name_string,
};

impl SysmonApp {
    pub(crate) fn draw_overview(&self, fb: &mut DrawBuffer<'_>, width: i32, _height: i32) {
        let mut y = super::tab_height() + 8;

        self.draw_section_title(fb, 10, y, "SYSTEM");
        y += font::cell_height();
        Self::text(
            fb,
            10,
            y,
            &format!("Uptime: {}", format_uptime(self.last_refresh_ms)),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height() + 4;

        self.draw_section_title(fb, 10, y, "CPU");
        y += font::cell_height();
        let bar_w = (width - 170).max(80);
        for i in 0..self.cpu_count {
            let usage = self.cpu_usage_pct[i];
            let label = format!("CPU{} {:>3}%", self.percpu[i].cpu_id, usage);
            let bar_h = font::cell_height();
            self.draw_usage_bar(fb, 10, y, bar_w, bar_h, usage, &label);
            y += bar_h + 2;
        }
        y += 4;

        self.draw_section_title(fb, 10, y, "MEMORY");
        y += font::cell_height();
        let total_bytes = (self.sys_info.total_pages as u64).saturating_mul(PAGE_SIZE);
        let used_bytes = (self.sys_info.allocated_pages.min(self.sys_info.total_pages) as u64)
            .saturating_mul(PAGE_SIZE);
        let mem_pct = if total_bytes == 0 {
            0
        } else {
            ((used_bytes.saturating_mul(100) / total_bytes).min(100)) as u32
        };
        let mem_label = format!(
            "Used: {} / {} ({}%)",
            format_bytes_mib(used_bytes),
            format_bytes_mib(total_bytes),
            mem_pct
        );
        let bar_h = font::cell_height();
        self.draw_usage_bar(fb, 10, y, bar_w, bar_h, mem_pct, &mem_label);
        y += bar_h + 4;

        self.draw_section_title(fb, 10, y, "TASKS");
        y += font::cell_height();
        let mut blocked = 0usize;
        for i in 0..self.task_count {
            if self.tasks[i].state == 3 {
                blocked += 1;
            }
        }
        let ready = self.sys_info.ready_tasks as usize;
        let active = self.sys_info.active_tasks as usize;
        Self::text(
            fb,
            10,
            y,
            &format!(
                "{} total  {} active  {} ready  {} blocked",
                self.task_count, active, ready, blocked
            ),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height() + 4;

        self.draw_section_title(fb, 10, y, "NETWORK");
        y += font::cell_height();
        Self::text(
            fb,
            10,
            y,
            "RX: 0.0 MiB (0 pkts)  TX: 0.0 MiB (0 pkts)",
            COLOR_TEXT,
            COLOR_BG,
        );
        y += font::cell_height() + 4;

        self.draw_section_title(fb, 10, y, "TOP PROCESSES");
        y += font::cell_height();
        let mut top = [usize::MAX; 3];
        for i in 0..self.task_count {
            for slot in 0..3 {
                if top[slot] == usize::MAX || self.task_cpu_pct[i] > self.task_cpu_pct[top[slot]] {
                    let mut k = 2;
                    while k > slot {
                        top[k] = top[k - 1];
                        k -= 1;
                    }
                    top[slot] = i;
                    break;
                }
            }
        }

        for (rank, idx) in top.iter().enumerate() {
            if *idx == usize::MAX {
                break;
            }
            let task = &self.tasks[*idx];
            let name = task_name_string(task);
            let name_padded = if name.chars().count() < 16 {
                format!("{:<16} ", name)
            } else {
                format!("{} ", name)
            };
            let pct = self.task_cpu_pct[*idx];
            let line = format!(
                "{}. {}{:>6}  pid {}",
                rank + 1,
                name_padded,
                format_pct(pct),
                task.task_id
            );
            Self::text(fb, 10, y, &line, COLOR_TEXT, COLOR_BG);
            y += font::cell_height();
        }
    }
}
