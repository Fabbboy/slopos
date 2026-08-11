use core::cmp::Ordering;
use core::option::Option::{self, None, Some};
use std::string::String;

/// Sysmon's display-side cap on visible tasks. Independent of the
/// kernel's `MAX_TASKS`: the TUI can only show a bounded set on screen
/// regardless of how many tasks the kernel actually supports. The
/// `process_list` syscall truncates to this many entries.
pub(crate) const MAX_TASKS: usize = 256;

use crate::syscall::{UserCpuInfo, UserPerCpuStats, UserSysInfo, UserTaskEntry, core as sys_core};

use super::{MAX_CPUS, REFRESH_INTERVAL_MS, is_idle_task, task_name_bytes, task_name_string};

/// The network facts the overview shows, read from `net_query` so sysmon and
/// `ip` cannot disagree about what "online" means.
#[derive(Clone, Copy)]
pub(crate) struct NetSummary {
    pub(crate) oper_state: u8,
    pub(crate) mac: [u8; 6],
    pub(crate) addr: [u8; 4],
    pub(crate) prefix_len: u8,
}

impl NetSummary {
    /// The first non-loopback interface, and the first global address on it.
    ///
    /// `None` means the query itself failed or there is no such interface,
    /// which the panel renders as "unavailable" rather than as "offline".
    fn fetch() -> Option<Self> {
        use slopos_abi::net::{
            NET_ADDR_SCOPE_GLOBAL, NET_IFINDEX_NONE, NET_IFKIND_LOOPBACK, NET_Q_ADDRS,
            NET_Q_IFACES, UserAddr, UserIface,
        };
        let ifaces = crate::net_query::fetch::<UserIface>(NET_Q_IFACES, NET_IFINDEX_NONE).ok()?;
        let iface = ifaces
            .records
            .iter()
            .find(|i| i.kind != NET_IFKIND_LOOPBACK)?;

        let mut out = Self {
            oper_state: iface.oper_state,
            mac: iface.mac,
            addr: [0; 4],
            prefix_len: 0,
        };
        if let Ok(addrs) = crate::net_query::fetch::<UserAddr>(NET_Q_ADDRS, iface.ifindex)
            && let Some(addr) = addrs
                .records
                .iter()
                .find(|a| a.scope == NET_ADDR_SCOPE_GLOBAL as u8)
        {
            out.addr = addr.addr;
            out.prefix_len = addr.prefix_len;
        }
        Some(out)
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Tab {
    Overview,
    Processes,
    Hardware,
}

/// A task named by the row the user acted on.
///
/// The name is captured with the pid rather than re-read at confirm time: task
/// ids recycle, so a refresh between opening the dialog and pressing Kill can
/// put a different task on that number. Holding both lets
/// [`SysmonApp::pending_kill_target`] notice the swap and refuse, instead of
/// killing a stranger the user never saw.
#[derive(Clone, PartialEq)]
pub(crate) struct KillTarget {
    pub(crate) pid: u32,
    pub(crate) name: String,
}

/// The row-level context menu, anchored where it was raised.
pub(crate) struct ContextMenu {
    pub(crate) target: KillTarget,
    pub(crate) x: i32,
    pub(crate) y: i32,
}

/// Outcome of a kill attempt, surfaced in the process panel's status line.
pub(crate) enum KillOutcome {
    Sent { name: String },
    Failed { name: String, errno: i32 },
    Vanished { name: String },
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

pub(crate) struct SysmonApp {
    pub(crate) active_tab: Tab,
    pub(crate) sys_info: UserSysInfo,
    pub(crate) cpu_info: UserCpuInfo,
    pub(crate) tasks: [UserTaskEntry; MAX_TASKS],
    pub(crate) task_count: usize,
    pub(crate) percpu: [UserPerCpuStats; MAX_CPUS],
    pub(crate) cpu_count: usize,
    pub(crate) net: Option<NetSummary>,
    pub(crate) prev_tasks: [UserTaskEntry; MAX_TASKS],
    pub(crate) prev_task_count: usize,
    pub(crate) prev_percpu: [UserPerCpuStats; MAX_CPUS],
    pub(crate) task_cpu_pct: [u32; MAX_TASKS],
    pub(crate) cpu_usage_pct: [u32; MAX_CPUS],
    pub(crate) selected_row: usize,
    pub(crate) sort_column: SortColumn,
    pub(crate) sort_ascending: bool,
    pub(crate) last_refresh_ms: u64,
    pub(crate) confirm_kill: Option<KillTarget>,
    pub(crate) context_menu: Option<ContextMenu>,
    pub(crate) last_kill: Option<KillOutcome>,
    pub(crate) sorted_indices: [usize; MAX_TASKS],
    pub(crate) hardware_scroll_y: i32,
    /// This process's own id, so sysmon cannot be asked to kill itself.
    pub(crate) self_pid: u32,
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
            net: None,
            prev_tasks: [UserTaskEntry::default(); MAX_TASKS],
            prev_task_count: 0,
            prev_percpu: [UserPerCpuStats::default(); MAX_CPUS],
            task_cpu_pct: [0; MAX_TASKS],
            cpu_usage_pct: [0; MAX_CPUS],
            selected_row: 0,
            sort_column: SortColumn::CpuPct,
            sort_ascending: false,
            last_refresh_ms: 0,
            confirm_kill: None,
            context_menu: None,
            last_kill: None,
            sorted_indices: [0; MAX_TASKS],
            hardware_scroll_y: 0,
            self_pid: crate::syscall::process::getpid(),
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

        let raw_count = sys_core::process_list(&mut self.tasks);
        let raw_count = if raw_count <= 0 {
            0
        } else {
            (raw_count as usize).min(MAX_TASKS)
        };

        // Hide kernel idle tasks from the process list. The per-CPU
        // usage bar already surfaces system idleness; rows like
        // `idle/0`, `idle/1`, ... would just dominate the table.
        let mut kept = 0;
        for i in 0..raw_count {
            if !is_idle_task(&self.tasks[i]) {
                if kept != i {
                    self.tasks[kept] = self.tasks[i];
                }
                kept += 1;
            }
        }
        self.task_count = kept;

        let cpu_count = sys_core::percpu_stats(&mut self.percpu);
        self.cpu_count = if cpu_count <= 0 {
            0
        } else {
            (cpu_count as usize).min(MAX_CPUS)
        };

        if self.cpu_info.cpu_count == 0 {
            let _ = sys_core::cpu_info(&mut self.cpu_info);
        }

        self.net = NetSummary::fetch();

        self.compute_cpu_usage();
        self.compute_task_cpu(elapsed_ms);

        self.prev_task_count = self.task_count;
        self.prev_tasks[..self.task_count].copy_from_slice(&self.tasks[..self.task_count]);
        self.prev_percpu[..self.cpu_count].copy_from_slice(&self.percpu[..self.cpu_count]);

        self.sort_tasks();

        if self.task_count == 0 {
            self.selected_row = 0;
        } else if self.selected_row >= self.task_count {
            self.selected_row = self.task_count - 1;
        }

        // A target that exited while its menu or dialog was open no longer
        // designates anything; drop the affordance rather than let it act on
        // whatever inherits the number.
        if self
            .confirm_kill
            .as_ref()
            .is_some_and(|t| self.live_target(t).is_none())
        {
            self.confirm_kill = None;
        }
        if self
            .context_menu
            .as_ref()
            .is_some_and(|m| self.live_target(&m.target).is_none())
        {
            self.context_menu = None;
        }
    }

    /// The row for `target`, but only while the pid still carries the name it
    /// was captured with. `None` once the id has been recycled.
    fn live_target(&self, target: &KillTarget) -> Option<usize> {
        let idx = self.find_task_index_by_pid(target.pid)?;
        (task_name_string(&self.tasks[idx]) == target.name).then_some(idx)
    }

    /// Whether `pid` may be offered a Kill action.
    ///
    /// Refusing our own pid keeps the window from tearing itself down mid-frame;
    /// the kernel enforces the real privilege rules and answers EPERM.
    pub(crate) fn is_killable(&self, pid: u32) -> bool {
        pid != self.self_pid
    }

    /// Build a kill target from a display row, if the row names a live task.
    pub(crate) fn target_for_row(&self, row: usize) -> Option<KillTarget> {
        let idx = self.sorted_task_index(row)?;
        let task = self.tasks.get(idx)?;
        Some(KillTarget {
            pid: task.task_id,
            name: task_name_string(task),
        })
    }

    /// Re-validate the pending target at the moment Kill is pressed.
    pub(crate) fn pending_kill_target(&self) -> Option<&KillTarget> {
        let target = self.confirm_kill.as_ref()?;
        self.live_target(target).map(|_| target)
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
