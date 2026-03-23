use core::cmp::Ordering;
use core::option::Option::{self, None, Some};

use slopos_abi::task::MAX_TASKS;

use crate::syscall::{
    UserCpuInfo, UserNetInfo, UserPerCpuStats, UserSysInfo, UserTaskEntry, core as sys_core,
    net as sys_net,
};

use super::{
    COL_CPU_PCT_X, COL_CPU_X, COL_NAME_X, COL_PID_X, COL_PRI_X, COL_RUNTIME_X, COL_STATE_X,
    MAX_CPUS, PROCESS_HEADER_Y, PROCESS_STATUS_H,
    process_header_h, process_row_h, process_rows_y,
    REFRESH_INTERVAL_MS, task_name_bytes,
};

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Tab {
    Overview,
    Processes,
    Hardware,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum SortColumn {
    Pid,
    Name,
    State,
    CpuPct,
    Priority,
    Cpu,
    Runtime,
}

#[derive(Clone, Copy)]
pub(crate) struct ContextMenu {
    pub(crate) task_id: u32,
    pub(crate) task_name: [u8; 32],
    pub(crate) x: i32,
    pub(crate) y: i32,
}

pub(crate) struct SysmonApp {
    pub(crate) active_tab: Tab,
    pub(crate) sys_info: UserSysInfo,
    pub(crate) cpu_info: UserCpuInfo,
    pub(crate) tasks: [UserTaskEntry; MAX_TASKS],
    pub(crate) task_count: usize,
    pub(crate) percpu: [UserPerCpuStats; MAX_CPUS],
    pub(crate) cpu_count: usize,
    pub(crate) net_info: UserNetInfo,
    pub(crate) prev_tasks: [UserTaskEntry; MAX_TASKS],
    pub(crate) prev_task_count: usize,
    pub(crate) prev_percpu: [UserPerCpuStats; MAX_CPUS],
    pub(crate) task_cpu_pct: [u32; MAX_TASKS],
    pub(crate) cpu_usage_pct: [u32; MAX_CPUS],
    pub(crate) selected_row: usize,
    pub(crate) scroll_offset: usize,
    pub(crate) sort_column: SortColumn,
    pub(crate) sort_ascending: bool,
    pub(crate) last_refresh_ms: u64,
    pub(crate) confirm_kill: Option<u32>,
    pub(crate) confirm_kill_hover: u8,
    pub(crate) context_menu: Option<ContextMenu>,
    pub(crate) sorted_indices: [usize; MAX_TASKS],
    pub(crate) pointer_x: i32,
    pub(crate) pointer_y: i32,
}

impl SysmonApp {
    pub(crate) fn new() -> Self {
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
            confirm_kill_hover: 0,
            context_menu: None,
            sorted_indices: [0; MAX_TASKS],
            pointer_x: 0,
            pointer_y: 0,
        };
        app.refresh_data();
        app
    }

    pub(crate) fn refresh_data(&mut self) {
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
            self.context_menu = None;
            self.confirm_kill_hover = 0;
        } else if self.selected_row >= self.task_count {
            self.selected_row = self.task_count - 1;
        }

        if let Some(pid) = self.confirm_kill {
            if self.find_task_index_by_pid(pid).is_none() {
                self.confirm_kill = None;
                self.confirm_kill_hover = 0;
            }
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
            SortColumn::State => a.state.cmp(&b.state),
            SortColumn::CpuPct => self.task_cpu_pct[a_idx].cmp(&self.task_cpu_pct[b_idx]),
            SortColumn::Priority => a.priority.cmp(&b.priority),
            SortColumn::Cpu => a.last_cpu.cmp(&b.last_cpu),
            SortColumn::Runtime => a.total_runtime_us.cmp(&b.total_runtime_us),
        }
    }

    pub(crate) fn cycle_tab(&mut self) {
        self.active_tab = match self.active_tab {
            Tab::Overview => Tab::Processes,
            Tab::Processes => Tab::Hardware,
            Tab::Hardware => Tab::Overview,
        };
        self.confirm_kill = None;
        self.confirm_kill_hover = 0;
        self.context_menu = None;
    }

    pub(crate) fn cycle_sort_for_column(&mut self, col: SortColumn) {
        if self.sort_column == col {
            self.sort_ascending = !self.sort_ascending;
        } else {
            self.sort_column = col;
            self.sort_ascending = match col {
                SortColumn::CpuPct | SortColumn::Runtime => false,
                SortColumn::Pid
                | SortColumn::Name
                | SortColumn::State
                | SortColumn::Priority
                | SortColumn::Cpu => true,
            };
        }
        self.sort_tasks();
    }

    pub(crate) fn move_selection_up(&mut self) {
        if self.selected_row > 0 {
            self.selected_row -= 1;
        }
    }

    pub(crate) fn move_selection_down(&mut self) {
        if self.task_count == 0 {
            self.selected_row = 0;
        } else if self.selected_row + 1 < self.task_count {
            self.selected_row += 1;
        }
    }

    pub(crate) fn process_max_rows(&self, height: i32) -> usize {
        let rows_h = (height - process_rows_y() - PROCESS_STATUS_H).max(0);
        (rows_h / process_row_h()).max(0) as usize
    }

    pub(crate) fn ensure_process_scroll(&mut self, max_rows: usize) {
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
    }

    pub(crate) fn process_row_from_pointer(&self, height: i32, pointer_y: i32) -> Option<usize> {
        if pointer_y < process_rows_y() {
            return None;
        }
        let max_rows = self.process_max_rows(height);
        if max_rows == 0 {
            return None;
        }
        let row = ((pointer_y - process_rows_y()) / process_row_h()) as usize;
        if row >= max_rows {
            return None;
        }
        let task_row = self.scroll_offset + row;
        if task_row >= self.task_count {
            return None;
        }
        Some(task_row)
    }

    pub(crate) fn process_sort_column_from_pointer(
        &self,
        pointer_x: i32,
        pointer_y: i32,
    ) -> Option<SortColumn> {
        if pointer_y < PROCESS_HEADER_Y || pointer_y >= PROCESS_HEADER_Y + process_header_h() {
            return None;
        }

        if pointer_x >= COL_PID_X && pointer_x < COL_NAME_X {
            Some(SortColumn::Pid)
        } else if pointer_x >= COL_NAME_X && pointer_x < COL_STATE_X {
            Some(SortColumn::Name)
        } else if pointer_x >= COL_STATE_X && pointer_x < COL_CPU_PCT_X {
            Some(SortColumn::State)
        } else if pointer_x >= COL_CPU_PCT_X && pointer_x < COL_PRI_X {
            Some(SortColumn::CpuPct)
        } else if pointer_x >= COL_PRI_X && pointer_x < COL_CPU_X {
            Some(SortColumn::Priority)
        } else if pointer_x >= COL_CPU_X && pointer_x < COL_RUNTIME_X {
            Some(SortColumn::Cpu)
        } else if pointer_x >= COL_RUNTIME_X {
            Some(SortColumn::Runtime)
        } else {
            None
        }
    }

    pub(crate) fn find_task_index_by_pid(&self, pid: u32) -> Option<usize> {
        for i in 0..self.task_count {
            if self.tasks[i].task_id == pid {
                return Some(i);
            }
        }
        None
    }

    pub(crate) fn sorted_task_index(&self, row: usize) -> Option<usize> {
        if row >= self.task_count {
            return None;
        }
        Some(self.sorted_indices[row])
    }
}
