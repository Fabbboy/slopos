use core::cmp::Ordering;
use core::option::Option::{self, None, Some};
use std::format;
use std::string::String;

use slopos_abi::DamageRect;
use slopos_abi::PAGE_SIZE;
use slopos_abi::draw::Color32;
use slopos_abi::task::MAX_TASKS;

use crate::appkit::{self, ControlFlow, Event, Window, WindowedApp};
use crate::gfx::draw_str_clipped;
use crate::gfx::font::{FONT_CHAR_HEIGHT, string_width};
use crate::gfx::{DrawBuffer, draw_rect, fill_rect, fill_rect_clipped};
use crate::syscall::{
    UserCpuInfo, UserNetInfo, UserPerCpuStats, UserSysInfo, UserTaskEntry, core as sys_core,
    net as sys_net, process as sys_proc,
};
use slopos_slibc::mem::malloc::heap_stats;

const COLOR_BG: Color32 = Color32::rgb(0x0A, 0x0E, 0x14);
const COLOR_TEXT: Color32 = Color32::rgb(0xD0, 0xD0, 0xD0);
const COLOR_DIM: Color32 = Color32::rgb(0x60, 0x68, 0x70);
const COLOR_BRIGHT: Color32 = Color32::rgb(0xFF, 0xFF, 0xFF);
const COLOR_TAB_ACTIVE: Color32 = Color32::rgb(0x30, 0x6C, 0xB0);
const COLOR_TAB_INACTIVE: Color32 = Color32::rgb(0x1A, 0x22, 0x2E);
const COLOR_TAB_BAR: Color32 = Color32::rgb(0x12, 0x18, 0x20);
const COLOR_HEADER: Color32 = Color32::rgb(0x18, 0x20, 0x2A);
const COLOR_ROW_EVEN: Color32 = Color32::rgb(0x0E, 0x14, 0x1C);
const COLOR_ROW_ODD: Color32 = Color32::rgb(0x12, 0x18, 0x22);
const COLOR_ROW_SELECTED: Color32 = Color32::rgb(0x1E, 0x3A, 0x5C);
const COLOR_BAR_LOW: Color32 = Color32::rgb(0x2E, 0xAA, 0x4E);
const COLOR_BAR_MED: Color32 = Color32::rgb(0xCC, 0xAA, 0x22);
const COLOR_BAR_HIGH: Color32 = Color32::rgb(0xCC, 0x33, 0x33);
const COLOR_BAR_BG: Color32 = Color32::rgb(0x1A, 0x20, 0x28);
const COLOR_SECTION: Color32 = Color32::rgb(0x50, 0x90, 0xD0);
const COLOR_STATE_RUN: Color32 = Color32::rgb(0x44, 0xCC, 0x44);
const COLOR_STATE_BLOCK: Color32 = Color32::rgb(0xCC, 0xAA, 0x44);
const COLOR_STATE_READY: Color32 = Color32::rgb(0xCC, 0xCC, 0xCC);
const COLOR_KILL_RED: Color32 = Color32::rgb(0xDD, 0x33, 0x33);

const MAX_CPUS: usize = 16;
const REFRESH_INTERVAL_MS: u64 = 1000;
const TAB_HEIGHT: i32 = 26;
const TAB_WIDTH: i32 = 120;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Overview,
    Processes,
    Hardware,
}

#[derive(Clone, Copy, PartialEq)]
enum SortColumn {
    Pid,
    Name,
    CpuPct,
    State,
    Runtime,
}

struct SysmonApp {
    active_tab: Tab,
    sys_info: UserSysInfo,
    cpu_info: UserCpuInfo,
    tasks: [UserTaskEntry; MAX_TASKS],
    task_count: usize,
    percpu: [UserPerCpuStats; MAX_CPUS],
    cpu_count: usize,
    net_info: UserNetInfo,
    prev_tasks: [UserTaskEntry; MAX_TASKS],
    prev_task_count: usize,
    prev_percpu: [UserPerCpuStats; MAX_CPUS],
    task_cpu_pct: [u32; MAX_TASKS],
    cpu_usage_pct: [u32; MAX_CPUS],
    selected_row: usize,
    scroll_offset: usize,
    sort_column: SortColumn,
    sort_ascending: bool,
    last_refresh_ms: u64,
    confirm_kill: Option<u32>,
    sorted_indices: [usize; MAX_TASKS],
}

impl SysmonApp {
    fn new() -> Self {
        let mut app = Self {
            active_tab: Tab::Overview,
            sys_info: UserSysInfo::default(),
            cpu_info: UserCpuInfo::default(),
            tasks: [UserTaskEntry::default(); MAX_TASKS],
            task_count: 0,
            percpu: [UserPerCpuStats::default(); MAX_CPUS],
            cpu_count: 0,
            net_info: UserNetInfo::default(),
            prev_tasks: [UserTaskEntry::default(); MAX_TASKS],
            prev_task_count: 0,
            prev_percpu: [UserPerCpuStats::default(); MAX_CPUS],
            task_cpu_pct: [0; MAX_TASKS],
            cpu_usage_pct: [0; MAX_CPUS],
            selected_row: 0,
            scroll_offset: 0,
            sort_column: SortColumn::CpuPct,
            sort_ascending: false,
            last_refresh_ms: 0,
            confirm_kill: None,
            sorted_indices: [0; MAX_TASKS],
        };
        app.refresh_data();
        app
    }

    fn refresh_data(&mut self) {
        let now_ms = sys_core::get_time_ms();
        let elapsed_ms = if self.last_refresh_ms == 0 {
            REFRESH_INTERVAL_MS
        } else {
            now_ms.saturating_sub(self.last_refresh_ms).max(1)
        };
        self.last_refresh_ms = now_ms;

        let _ = sys_core::sys_info(&mut self.sys_info);

        let task_count = sys_core::process_list(&mut self.tasks);
        self.task_count = if task_count <= 0 {
            0
        } else {
            (task_count as usize).min(MAX_TASKS)
        };

        let cpu_count = sys_core::percpu_stats(&mut self.percpu);
        self.cpu_count = if cpu_count <= 0 {
            0
        } else {
            (cpu_count as usize).min(MAX_CPUS)
        };

        if self.cpu_info.cpu_count == 0 {
            let _ = sys_core::cpu_info(&mut self.cpu_info);
        }

        let _ = sys_net::net_info(&mut self.net_info);

        self.compute_cpu_usage();
        self.compute_task_cpu(elapsed_ms);

        self.prev_task_count = self.task_count;
        self.prev_tasks[..self.task_count].copy_from_slice(&self.tasks[..self.task_count]);

        self.prev_percpu[..self.cpu_count].copy_from_slice(&self.percpu[..self.cpu_count]);

        self.sort_tasks();

        if self.task_count == 0 {
            self.selected_row = 0;
            self.scroll_offset = 0;
            self.confirm_kill = None;
        } else if self.selected_row >= self.task_count {
            self.selected_row = self.task_count - 1;
        }
    }

    fn compute_cpu_usage(&mut self) {
        for i in 0..self.cpu_count {
            let cpu_id = self.percpu[i].cpu_id;
            let mut prev = None;
            for j in 0..self.cpu_count {
                if self.prev_percpu[j].cpu_id == cpu_id {
                    prev = Some(self.prev_percpu[j]);
                    break;
                }
            }

            let usage = if let Some(prev_cpu) = prev {
                let new_ticks = self.percpu[i].total_ticks;
                let old_ticks = prev_cpu.total_ticks;
                let new_idle = self.percpu[i].idle_ticks;
                let old_idle = prev_cpu.idle_ticks;

                let delta_ticks = new_ticks.saturating_sub(old_ticks);
                let delta_idle = new_idle.saturating_sub(old_idle);
                if delta_ticks == 0 {
                    0
                } else {
                    let active = delta_ticks.saturating_sub(delta_idle);
                    ((active.saturating_mul(100)) / delta_ticks).min(100) as u32
                }
            } else {
                0
            };

            self.cpu_usage_pct[i] = usage;
        }
    }

    fn compute_task_cpu(&mut self, elapsed_ms: u64) {
        self.task_cpu_pct.fill(0);

        let cpu_div = self.cpu_count.max(1) as u64;
        let denom = elapsed_ms.saturating_mul(1000).saturating_mul(cpu_div);
        if denom == 0 {
            return;
        }

        for i in 0..self.task_count {
            let tid = self.tasks[i].task_id;
            let mut prev_runtime = None;

            for j in 0..self.prev_task_count {
                if self.prev_tasks[j].task_id == tid {
                    prev_runtime = Some(self.prev_tasks[j].total_runtime_us);
                    break;
                }
            }

            if let Some(old_runtime) = prev_runtime {
                let delta_us = self.tasks[i].total_runtime_us.saturating_sub(old_runtime);
                let pct_x10 = (delta_us.saturating_mul(1000) / denom).min(1000) as u32;
                self.task_cpu_pct[i] = pct_x10;
            }
        }
    }

    fn sort_tasks(&mut self) {
        for i in 0..self.task_count {
            self.sorted_indices[i] = i;
        }

        for i in 1..self.task_count {
            let key = self.sorted_indices[i];
            let mut j = i;
            while j > 0 {
                let prev = self.sorted_indices[j - 1];
                let ord = self.compare_task_indices(key, prev);
                let should_shift = if self.sort_ascending {
                    ord == Ordering::Less
                } else {
                    ord == Ordering::Greater
                };
                if !should_shift {
                    break;
                }
                self.sorted_indices[j] = self.sorted_indices[j - 1];
                j -= 1;
            }
            self.sorted_indices[j] = key;
        }
    }

    fn compare_task_indices(&self, a_idx: usize, b_idx: usize) -> Ordering {
        let a = &self.tasks[a_idx];
        let b = &self.tasks[b_idx];
        match self.sort_column {
            SortColumn::Pid => a.task_id.cmp(&b.task_id),
            SortColumn::Name => task_name_bytes(a).cmp(task_name_bytes(b)),
            SortColumn::CpuPct => self.task_cpu_pct[a_idx].cmp(&self.task_cpu_pct[b_idx]),
            SortColumn::State => a.state.cmp(&b.state),
            SortColumn::Runtime => a.total_runtime_us.cmp(&b.total_runtime_us),
        }
    }

    fn cycle_tab(&mut self) {
        self.active_tab = match self.active_tab {
            Tab::Overview => Tab::Processes,
            Tab::Processes => Tab::Hardware,
            Tab::Hardware => Tab::Overview,
        };
        self.confirm_kill = None;
    }

    fn cycle_sort_column(&mut self) {
        self.sort_column = match self.sort_column {
            SortColumn::Pid => SortColumn::Name,
            SortColumn::Name => SortColumn::CpuPct,
            SortColumn::CpuPct => SortColumn::State,
            SortColumn::State => SortColumn::Runtime,
            SortColumn::Runtime => SortColumn::Pid,
        };
        self.sort_ascending = match self.sort_column {
            SortColumn::Name | SortColumn::Pid => true,
            _ => false,
        };
        self.sort_tasks();
    }

    fn move_selection_up(&mut self) {
        if self.selected_row > 0 {
            self.selected_row -= 1;
        }
    }

    fn move_selection_down(&mut self) {
        if self.task_count == 0 {
            self.selected_row = 0;
        } else if self.selected_row + 1 < self.task_count {
            self.selected_row += 1;
        }
    }

    fn selected_task_pid(&self) -> Option<u32> {
        if self.task_count == 0 || self.selected_row >= self.task_count {
            return None;
        }
        let idx = self.sorted_indices[self.selected_row];
        Some(self.tasks[idx].task_id)
    }

    fn text(fb: &mut DrawBuffer<'_>, x: i32, y: i32, s: &str, fg: Color32, bg: Color32) {
        let clip = DamageRect {
            x0: 0,
            y0: 0,
            x1: fb.width() as i32 - 1,
            y1: fb.height() as i32 - 1,
        };
        draw_str_clipped(fb, x, y, s, fg, bg, &clip);
    }

    fn draw_tab_bar(&self, fb: &mut DrawBuffer<'_>, width: i32) {
        fill_rect(fb, 0, 0, width, TAB_HEIGHT, COLOR_TAB_BAR);

        self.draw_tab(fb, 4, "Overview", self.active_tab == Tab::Overview);
        self.draw_tab(
            fb,
            4 + TAB_WIDTH + 2,
            "Processes",
            self.active_tab == Tab::Processes,
        );
        self.draw_tab(
            fb,
            4 + (TAB_WIDTH + 2) * 2,
            "Hardware",
            self.active_tab == Tab::Hardware,
        );

        draw_rect(fb, 0, TAB_HEIGHT - 1, width, 1, COLOR_DIM);
    }

    fn draw_tab(&self, fb: &mut DrawBuffer<'_>, x: i32, label: &str, active: bool) {
        let bg = if active {
            COLOR_TAB_ACTIVE
        } else {
            COLOR_TAB_INACTIVE
        };
        let fg = if active { COLOR_BRIGHT } else { COLOR_DIM };
        fill_rect(fb, x, 2, TAB_WIDTH, TAB_HEIGHT - 3, bg);
        draw_rect(fb, x, 2, TAB_WIDTH, TAB_HEIGHT - 3, COLOR_DIM);
        Self::text(
            fb,
            x + ((TAB_WIDTH - string_width(label)) / 2).max(4),
            6,
            label,
            fg,
            bg,
        );
    }

    fn draw_section_title(&self, fb: &mut DrawBuffer<'_>, x: i32, y: i32, title: &str) {
        Self::text(fb, x, y, title, COLOR_SECTION, COLOR_BG);
    }

    fn draw_usage_bar(
        &self,
        fb: &mut DrawBuffer<'_>,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        pct: u32,
        label: &str,
    ) {
        fill_rect(fb, x, y, w, h, COLOR_BAR_BG);
        let fill_w = ((w as u32).saturating_mul(pct.min(100)) / 100) as i32;
        let color = if pct < 50 {
            COLOR_BAR_LOW
        } else if pct < 80 {
            COLOR_BAR_MED
        } else {
            COLOR_BAR_HIGH
        };
        fill_rect(fb, x, y, fill_w, h, color);
        draw_rect(fb, x, y, w, h, COLOR_DIM);
        Self::text(fb, x + 4, y + 1, label, COLOR_BRIGHT, COLOR_BAR_BG);
    }

    fn draw_overview(&self, fb: &mut DrawBuffer<'_>, width: i32, _height: i32) {
        let mut y = TAB_HEIGHT + 8;

        self.draw_section_title(fb, 10, y, "SYSTEM");
        y += FONT_CHAR_HEIGHT;
        Self::text(
            fb,
            10,
            y,
            &format!("Uptime: {}", format_uptime(self.last_refresh_ms)),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += FONT_CHAR_HEIGHT + 4;

        self.draw_section_title(fb, 10, y, "CPU");
        y += FONT_CHAR_HEIGHT;
        let bar_w = (width - 170).max(80);
        for i in 0..self.cpu_count {
            let usage = self.cpu_usage_pct[i];
            let label = format!("CPU{} {:>3}%", self.percpu[i].cpu_id, usage);
            self.draw_usage_bar(fb, 10, y, bar_w, 14, usage, &label);
            y += 16;
        }
        y += 4;

        self.draw_section_title(fb, 10, y, "MEMORY");
        y += FONT_CHAR_HEIGHT;
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
        self.draw_usage_bar(fb, 10, y, bar_w, 14, mem_pct, &mem_label);
        y += 18;

        self.draw_section_title(fb, 10, y, "TASKS");
        y += FONT_CHAR_HEIGHT;
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
        y += FONT_CHAR_HEIGHT + 4;

        self.draw_section_title(fb, 10, y, "NETWORK");
        y += FONT_CHAR_HEIGHT;
        Self::text(
            fb,
            10,
            y,
            "RX: 0.0 MiB (0 pkts)  TX: 0.0 MiB (0 pkts)",
            COLOR_TEXT,
            COLOR_BG,
        );
        y += FONT_CHAR_HEIGHT + 4;

        self.draw_section_title(fb, 10, y, "TOP PROCESSES");
        y += FONT_CHAR_HEIGHT;
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
            y += FONT_CHAR_HEIGHT;
        }
    }

    fn draw_processes(&mut self, fb: &mut DrawBuffer<'_>, width: i32, height: i32) {
        const PID_W: i32 = 50;
        const NAME_W: i32 = 130;
        const STATE_W: i32 = 60;
        const CPU_W: i32 = 65;
        const PRI_W: i32 = 50;
        const CORE_W: i32 = 40;
        const RT_W: i32 = 100;
        const ROW_H: i32 = 16;

        let header_y = TAB_HEIGHT + 2;
        let rows_y = header_y + FONT_CHAR_HEIGHT + 4;
        let status_h = 20;
        let rows_h = (height - rows_y - status_h).max(0);
        let max_rows = (rows_h / ROW_H).max(0) as usize;

        if self.task_count > 0 {
            if self.selected_row < self.scroll_offset {
                self.scroll_offset = self.selected_row;
            }
            if max_rows > 0 && self.selected_row >= self.scroll_offset + max_rows {
                self.scroll_offset = self.selected_row + 1 - max_rows;
            }
        } else {
            self.scroll_offset = 0;
        }

        fill_rect(fb, 0, header_y, width, FONT_CHAR_HEIGHT + 2, COLOR_HEADER);

        let mut x = 6;
        self.draw_header_cell(fb, x, header_y, "PID", SortColumn::Pid);
        x += PID_W;
        self.draw_header_cell(fb, x, header_y, "Name", SortColumn::Name);
        x += NAME_W;
        self.draw_header_cell(fb, x, header_y, "State", SortColumn::State);
        x += STATE_W;
        self.draw_header_cell(fb, x, header_y, "CPU%", SortColumn::CpuPct);
        x += CPU_W;
        Self::text(fb, x, header_y, "Pri", COLOR_BRIGHT, COLOR_HEADER);
        x += PRI_W;
        Self::text(fb, x, header_y, "CPU", COLOR_BRIGHT, COLOR_HEADER);
        x += CORE_W;
        Self::text(fb, x, header_y, "Runtime", COLOR_BRIGHT, COLOR_HEADER);

        let clip = DamageRect {
            x0: 0,
            y0: rows_y,
            x1: width - 1,
            y1: rows_y + rows_h - 1,
        };
        fill_rect(fb, 0, rows_y, width, rows_h, COLOR_BG);

        for row in 0..max_rows {
            let task_row = self.scroll_offset + row;
            if task_row >= self.task_count {
                break;
            }
            let idx = self.sorted_indices[task_row];
            let task = &self.tasks[idx];
            let y = rows_y + (row as i32) * ROW_H;

            let row_bg = if task_row == self.selected_row {
                COLOR_ROW_SELECTED
            } else if row % 2 == 0 {
                COLOR_ROW_EVEN
            } else {
                COLOR_ROW_ODD
            };
            fill_rect_clipped(fb, 0, y, width, ROW_H, row_bg, &clip);

            let (state, state_color) = task_state(task.state);

            let mut col_x = 6;
            let pid = format!("{}", task.task_id);
            Self::text(fb, col_x, y, &pid, COLOR_TEXT, row_bg);
            col_x += PID_W;
            let name = truncate_name(&task_name_string(task), 16);
            Self::text(fb, col_x, y, &name, COLOR_TEXT, row_bg);
            col_x += NAME_W;
            Self::text(fb, col_x, y, state, state_color, row_bg);
            col_x += STATE_W;
            let cpu_pct = format_pct(self.task_cpu_pct[idx]);
            Self::text(fb, col_x, y, &cpu_pct, COLOR_TEXT, row_bg);
            col_x += CPU_W;
            Self::text(
                fb,
                col_x,
                y,
                priority_label(task.priority),
                COLOR_TEXT,
                row_bg,
            );
            col_x += PRI_W;
            let last_cpu = format!("{}", task.last_cpu);
            Self::text(fb, col_x, y, &last_cpu, COLOR_TEXT, row_bg);
            col_x += CORE_W;
            let runtime = format_runtime(task.total_runtime_us);
            Self::text(fb, col_x, y, &runtime, COLOR_TEXT, row_bg);
        }

        let status_y = height - status_h;
        fill_rect(fb, 0, status_y, width, status_h, COLOR_HEADER);
        Self::text(
            fb,
            6,
            status_y + 2,
            &format!(
                "{} tasks | Up/Down Nav | K Kill | S Sort | Tab Switch",
                self.task_count
            ),
            COLOR_DIM,
            COLOR_HEADER,
        );

        if let Some(pid) = self.confirm_kill {
            self.draw_kill_confirm(fb, width, height, pid);
        }

        let _ = RT_W;
    }

    fn draw_header_cell(
        &self,
        fb: &mut DrawBuffer<'_>,
        x: i32,
        y: i32,
        label: &str,
        col: SortColumn,
    ) {
        if self.sort_column == col {
            let indicator = if self.sort_ascending { "^" } else { "v" };
            let header = format!("{} {}", label, indicator);
            Self::text(fb, x, y, &header, COLOR_BRIGHT, COLOR_HEADER);
        } else {
            Self::text(fb, x, y, label, COLOR_BRIGHT, COLOR_HEADER);
        }
    }

    fn draw_kill_confirm(&self, fb: &mut DrawBuffer<'_>, width: i32, height: i32, pid: u32) {
        let box_w = 220;
        let box_h = 64;
        let x = (width - box_w) / 2;
        let y = (height - box_h) / 2;
        fill_rect(fb, x, y, box_w, box_h, COLOR_HEADER);
        draw_rect(fb, x, y, box_w, box_h, COLOR_KILL_RED);
        Self::text(
            fb,
            x + 10,
            y + 18,
            &format!("Kill task {}? Y/N", pid),
            COLOR_BRIGHT,
            COLOR_HEADER,
        );
    }

    fn draw_hardware(&self, fb: &mut DrawBuffer<'_>, width: i32, _height: i32) {
        let mut y = TAB_HEIGHT + 8;
        let lx = 10;
        let vx = 130;
        let bar_w = (width - 170).max(80);

        self.draw_section_title(fb, lx, y, "PROCESSOR");
        y += FONT_CHAR_HEIGHT + 2;
        Self::text(fb, lx + 4, y, "Model", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &trim_ascii(&self.cpu_info.brand_string),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += FONT_CHAR_HEIGHT;
        Self::text(fb, lx + 4, y, "Vendor", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &trim_ascii(&self.cpu_info.vendor),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += FONT_CHAR_HEIGHT;
        Self::text(fb, lx + 4, y, "Cores", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format!("{}", self.cpu_info.cpu_count),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += FONT_CHAR_HEIGHT;
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
        y += FONT_CHAR_HEIGHT;
        Self::text(fb, lx + 4, y, "Features", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_cpu_features(self.cpu_info.features),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += FONT_CHAR_HEIGHT + 6;

        draw_rect(fb, lx, y, width - 20, 1, COLOR_DIM);
        y += 6;

        self.draw_section_title(fb, lx, y, "MEMORY");
        y += FONT_CHAR_HEIGHT + 2;
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
        y += FONT_CHAR_HEIGHT;
        Self::text(fb, lx + 4, y, "Free", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_bytes_mib(free_bytes),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += FONT_CHAR_HEIGHT;
        Self::text(fb, lx + 4, y, "Allocated", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_bytes_mib(used_bytes),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += FONT_CHAR_HEIGHT + 6;

        draw_rect(fb, lx, y, width - 20, 1, COLOR_DIM);
        y += 6;

        self.draw_section_title(fb, lx, y, "SCHEDULER");
        y += FONT_CHAR_HEIGHT + 2;
        Self::text(fb, lx + 4, y, "Ctx switches", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_number(self.sys_info.task_context_switches),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += FONT_CHAR_HEIGHT;
        Self::text(fb, lx + 4, y, "Sched switches", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_number(self.sys_info.scheduler_context_switches),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += FONT_CHAR_HEIGHT;
        Self::text(fb, lx + 4, y, "Yields", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_number(self.sys_info.scheduler_yields),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += FONT_CHAR_HEIGHT;
        Self::text(fb, lx + 4, y, "Schedule calls", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_number(self.sys_info.schedule_calls as u64),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += FONT_CHAR_HEIGHT + 6;

        draw_rect(fb, lx, y, width - 20, 1, COLOR_DIM);
        y += 6;

        self.draw_section_title(fb, lx, y, "HEAP");
        y += FONT_CHAR_HEIGHT + 2;
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
        y += FONT_CHAR_HEIGHT;
        Self::text(fb, lx + 4, y, "Wilderness", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_bytes_mib(stats.wilderness as u64),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += FONT_CHAR_HEIGHT;
        Self::text(fb, lx + 4, y, "Mmap allocs", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_number(stats.mmap_count as u64),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += FONT_CHAR_HEIGHT + 6;

        draw_rect(fb, lx, y, width - 20, 1, COLOR_DIM);
        y += 6;

        self.draw_section_title(fb, lx, y, "NETWORK");
        y += FONT_CHAR_HEIGHT + 2;
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
        y += FONT_CHAR_HEIGHT;
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
        y += FONT_CHAR_HEIGHT;
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
        y += FONT_CHAR_HEIGHT + 6;

        draw_rect(fb, lx, y, width - 20, 1, COLOR_DIM);
        y += 6;

        self.draw_section_title(fb, lx, y, "BOOT");
        y += FONT_CHAR_HEIGHT + 2;
        Self::text(fb, lx + 4, y, "Uptime", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format_uptime(self.last_refresh_ms),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += FONT_CHAR_HEIGHT;
        Self::text(fb, lx + 4, y, "Boot flags", COLOR_DIM, COLOR_BG);
        Self::text(
            fb,
            vx,
            y,
            &format!("0x{:08x}", self.sys_info.boot_flags),
            COLOR_TEXT,
            COLOR_BG,
        );
        y += FONT_CHAR_HEIGHT;
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

impl WindowedApp for SysmonApp {
    fn init(&mut self, win: &mut Window) {
        win.set_title("System Monitor");
        win.request_redraw();
    }

    fn on_event(&mut self, win: &mut Window, event: Event) -> ControlFlow {
        match event {
            Event::CloseRequest => return ControlFlow::Exit,
            Event::KeyPress { scancode, ascii } => {
                match ascii {
                    b'\t' => self.cycle_tab(),
                    b'1' => {
                        self.active_tab = Tab::Overview;
                        self.confirm_kill = None;
                    }
                    b'2' => {
                        self.active_tab = Tab::Processes;
                        self.confirm_kill = None;
                    }
                    b'3' => {
                        self.active_tab = Tab::Hardware;
                        self.confirm_kill = None;
                    }
                    b'q' | 27 => return ControlFlow::Exit,
                    b'r' => self.refresh_data(),
                    b'y' => {
                        if let Some(pid) = self.confirm_kill {
                            let _ = sys_proc::kill(pid, 9);
                            self.confirm_kill = None;
                            self.refresh_data();
                        }
                    }
                    b'n' => {
                        self.confirm_kill = None;
                    }
                    _ => {
                        if self.active_tab == Tab::Processes {
                            match ascii {
                                b's' => self.cycle_sort_column(),
                                b'k' | 127 => {
                                    self.confirm_kill = self.selected_task_pid();
                                }
                                _ => {}
                            }
                        }
                    }
                }

                if self.active_tab == Tab::Processes {
                    match scancode {
                        0x48 => self.move_selection_up(),
                        0x50 => self.move_selection_down(),
                        _ => {}
                    }
                }
            }
            Event::PointerPress { button } => {
                if button == 1 {
                    let (x, y) = win.pointer();
                    if y < TAB_HEIGHT {
                        let tab_step = TAB_WIDTH + 2;
                        if x >= 4 {
                            let idx = (x - 4) / tab_step;
                            let tab_x = 4 + idx * tab_step;
                            if idx >= 0 && idx < 3 && x < tab_x + TAB_WIDTH {
                                self.active_tab = match idx {
                                    0 => Tab::Overview,
                                    1 => Tab::Processes,
                                    _ => Tab::Hardware,
                                };
                                self.confirm_kill = None;
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        win.request_redraw();
        ControlFlow::Continue
    }

    fn draw(&mut self, fb: &mut DrawBuffer<'_>) {
        let now = sys_core::get_time_ms();
        if now.saturating_sub(self.last_refresh_ms) >= REFRESH_INTERVAL_MS {
            self.refresh_data();
        }

        let width = fb.width() as i32;
        let height = fb.height() as i32;

        fill_rect(fb, 0, 0, width, height, COLOR_BG);
        self.draw_tab_bar(fb, width);

        match self.active_tab {
            Tab::Overview => self.draw_overview(fb, width, height),
            Tab::Processes => self.draw_processes(fb, width, height),
            Tab::Hardware => self.draw_hardware(fb, width, height),
        }
    }
}

fn task_name_bytes(task: &UserTaskEntry) -> &[u8] {
    let end = task
        .name
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(task.name.len());
    &task.name[..end]
}

fn task_name_string(task: &UserTaskEntry) -> String {
    let bytes = task_name_bytes(task);
    if let core::result::Result::Ok(s) = core::str::from_utf8(bytes) {
        return s.to_string();
    }

    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_graphic() || b == b' ' {
            out.push(b as char);
        } else {
            out.push('?');
        }
    }
    out
}

fn task_state(state: u8) -> (&'static str, Color32) {
    match state {
        2 => ("Run", COLOR_STATE_RUN),
        3 => ("Block", COLOR_STATE_BLOCK),
        1 => ("Ready", COLOR_STATE_READY),
        5 => ("WillBlk", COLOR_STATE_BLOCK),
        4 => ("Dead", COLOR_DIM),
        _ => ("--", COLOR_DIM),
    }
}

fn priority_label(priority: u8) -> &'static str {
    match priority {
        0 => "Hi",
        1 => "Norm",
        2 => "Low",
        3 => "Idle",
        _ => "?",
    }
}

fn trim_ascii(bytes: &[u8]) -> String {
    let mut end = bytes.len();
    while end > 0 && (bytes[end - 1] == 0 || bytes[end - 1] == b' ') {
        end -= 1;
    }
    let slice = &bytes[..end];
    if let core::result::Result::Ok(s) = core::str::from_utf8(slice) {
        return s.to_string();
    }

    let mut out = String::with_capacity(slice.len());
    for &b in slice {
        if b.is_ascii_graphic() || b == b' ' {
            out.push(b as char);
        } else {
            out.push('?');
        }
    }
    out
}

fn truncate_name(name: &str, max_chars: usize) -> String {
    let total = name.chars().count();
    if total <= max_chars {
        return name.to_string();
    }

    let mut out = String::new();
    for ch in name.chars().take(max_chars.saturating_sub(1)) {
        out.push(ch);
    }
    out.push('~');
    out
}

fn format_bytes_mib(bytes: u64) -> String {
    let whole = bytes / (1024 * 1024);
    let frac = ((bytes % (1024 * 1024)).saturating_mul(10)) / (1024 * 1024);
    if frac == 0 {
        format!("{} MiB", whole)
    } else {
        format!("{}.{} MiB", whole, frac)
    }
}

fn format_uptime(ms: u64) -> String {
    let total_sec = ms / 1000;
    let days = total_sec / 86_400;
    let rem = total_sec % 86_400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    format!("{}d {:02}:{:02}:{:02}", days, h, m, s)
}

fn format_runtime(us: u64) -> String {
    let total_sec = us / 1_000_000;
    let h = total_sec / 3600;
    let m = (total_sec % 3600) / 60;
    let s = total_sec % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

fn format_pct(pct_x10: u32) -> String {
    format!("{}.{}%", pct_x10 / 10, pct_x10 % 10)
}

fn format_number(n: u64) -> String {
    let digits = n.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + ((len.saturating_sub(1)) / 3));
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn format_cpu_features(features: u64) -> String {
    let ecx = features as u32;
    let edx = (features >> 32) as u32;
    let mut parts: String = String::new();
    let mut push_part = |part: &str| {
        if !parts.is_empty() {
            parts.push(' ');
        }
        parts.push_str(part);
    };
    if edx & (1 << 25) != 0 {
        push_part("SSE");
    }
    if edx & (1 << 26) != 0 {
        push_part("SSE2");
    }
    if ecx & (1 << 0) != 0 {
        push_part("SSE3");
    }
    if ecx & (1 << 9) != 0 {
        push_part("SSSE3");
    }
    if ecx & (1 << 19) != 0 {
        push_part("SSE4.1");
    }
    if ecx & (1 << 20) != 0 {
        push_part("SSE4.2");
    }
    if ecx & (1 << 28) != 0 {
        push_part("AVX");
    }
    if ecx & (1 << 26) != 0 {
        push_part("XSAVE");
    }
    if ecx & (1 << 21) != 0 {
        push_part("x2APIC");
    }
    if edx & (1 << 9) != 0 {
        push_part("APIC");
    }
    if edx & (1 << 4) != 0 {
        push_part("TSC");
    }
    if parts.is_empty() {
        return format!("0x{:016x}", features);
    }
    parts
}

pub fn sysmon_main() -> ! {
    appkit::run(SysmonApp::new(), 640, 480)
}
