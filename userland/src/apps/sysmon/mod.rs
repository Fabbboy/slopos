use std::format;
use std::string::String;

use slopos_abi::PAGE_SIZE;
use slopos_abi::draw::Color32;
use slopos_slibc::mem::malloc::heap_stats;

use crate::appkit::{
    Action, App, ButtonStyle, CrossAxisAlignment, EdgeInsets, Length, MessageId, Node,
    ScrollDirection, ScrollbarVisibility, SortIndicator, TableColumn, TableColumnWidth,
    TextAlignment,
};
use crate::syscall::process as sys_proc;

mod format;
mod state;

pub(crate) use format::*;
pub(crate) use state::{SortColumn, SysmonApp, Tab};

// Colors kept for format.rs (which imports them from super::).
pub(crate) const COLOR_DIM: Color32 = Color32::rgb(0x60, 0x68, 0x70);
pub(crate) const COLOR_STATE_RUN: Color32 = Color32::rgb(0x44, 0xCC, 0x44);
pub(crate) const COLOR_STATE_BLOCK: Color32 = Color32::rgb(0xCC, 0xAA, 0x44);
pub(crate) const COLOR_STATE_READY: Color32 = Color32::rgb(0xCC, 0xCC, 0xCC);

pub(crate) const MAX_CPUS: usize = 16;
pub(crate) const REFRESH_INTERVAL_MS: u64 = 1000;

const MSG_TAB: MessageId = MessageId::new(1);
const MSG_SORT: MessageId = MessageId::new(2);
const MSG_SELECT: MessageId = MessageId::new(3);
const MSG_KILL: MessageId = MessageId::new(4);
const MSG_CANCEL: MessageId = MessageId::new(5);
const MSG_HW_SCROLL: MessageId = MessageId::new(6);

#[derive(Clone, Debug)]
pub enum SysmonMsg {
    TabChanged(usize),
    SortColumn(usize),
    SelectRow(usize),
    Kill,
    Cancel,
    HwScroll(i32),
    Unknown(#[allow(dead_code)] MessageId),
}

impl From<MessageId> for SysmonMsg {
    fn from(m: MessageId) -> Self {
        match m.id {
            1 => SysmonMsg::TabChanged(m.payload as usize),
            2 => SysmonMsg::SortColumn(m.payload as usize),
            3 => SysmonMsg::SelectRow(m.payload as usize),
            4 => SysmonMsg::Kill,
            5 => SysmonMsg::Cancel,
            6 => SysmonMsg::HwScroll(m.payload as i32),
            _ => SysmonMsg::Unknown(m),
        }
    }
}

impl App for SysmonApp {
    type Message = SysmonMsg;

    fn view(&self) -> Node {
        Node::TabBar {
            tabs: vec![
                String::from("Overview"),
                String::from("Processes"),
                String::from("Hardware"),
            ],
            active: match self.active_tab {
                Tab::Overview => 0,
                Tab::Processes => 1,
                Tab::Hardware => 2,
            },
            on_change: MSG_TAB,
            content: vec![
                self.view_overview(),
                self.view_processes(),
                self.view_hardware(),
            ],
        }
    }

    fn update(&mut self, msg: SysmonMsg) -> Action {
        match msg {
            SysmonMsg::TabChanged(tab) => {
                self.active_tab = match tab {
                    0 => Tab::Overview,
                    1 => Tab::Processes,
                    _ => Tab::Hardware,
                };
                self.confirm_kill = None;
                Action::Rebuild
            }
            SysmonMsg::SortColumn(col_idx) => {
                let col = match col_idx {
                    0 => SortColumn::Pid,
                    1 => SortColumn::Name,
                    2 => SortColumn::State,
                    3 => SortColumn::CpuPct,
                    4 => SortColumn::Priority,
                    5 => SortColumn::Cpu,
                    _ => SortColumn::Runtime,
                };
                self.cycle_sort_for_column(col);
                Action::Rebuild
            }
            SysmonMsg::SelectRow(row) => {
                self.selected_row = row;
                Action::Rebuild
            }
            SysmonMsg::Kill => {
                if let Some(pid) = self.confirm_kill {
                    let _ = sys_proc::kill(pid, 9);
                    self.confirm_kill = None;
                    self.refresh_data();
                }
                Action::Rebuild
            }
            SysmonMsg::Cancel => {
                self.confirm_kill = None;
                Action::Rebuild
            }
            SysmonMsg::HwScroll(y) => {
                self.hardware_scroll_y = y;
                Action::None
            }
            SysmonMsg::Unknown(_) => Action::None,
        }
    }

    fn tick_interval_ms(&self) -> Option<u64> {
        Some(REFRESH_INTERVAL_MS)
    }

    fn tick(&mut self) -> Action {
        self.refresh_data();
        Action::Rebuild
    }

    fn title(&self) -> &str {
        "System Monitor"
    }

    fn app_id(&self) -> &str {
        "org.slopos.sysmon"
    }
}

impl SysmonApp {
    fn view_overview(&self) -> Node {
        let mut children: Vec<Node> = Vec::new();

        // SYSTEM
        children.push(label("SYSTEM"));
        children.push(label(&format!(
            "Uptime: {}",
            format_uptime(self.last_refresh_ms)
        )));
        children.push(Node::Spacer {
            size: Length::Px(8),
        });

        // CPU
        children.push(label("CPU"));
        for i in 0..self.cpu_count {
            let pct = self.cpu_usage_pct[i];
            children.push(Node::ProgressBar {
                value: pct,
                label: format!("CPU{} {:>3}%", self.percpu[i].cpu_id, pct),
                color: None,
            });
        }
        children.push(Node::Spacer {
            size: Length::Px(8),
        });

        // MEMORY
        children.push(label("MEMORY"));
        let total_bytes = (self.sys_info.total_pages as u64).saturating_mul(PAGE_SIZE);
        let used_bytes = (self.sys_info.allocated_pages.min(self.sys_info.total_pages) as u64)
            .saturating_mul(PAGE_SIZE);
        let mem_pct = if total_bytes == 0 {
            0
        } else {
            ((used_bytes.saturating_mul(100) / total_bytes).min(100)) as u32
        };
        children.push(Node::ProgressBar {
            value: mem_pct,
            label: format!(
                "Used: {} / {} ({}%)",
                format_bytes_mib(used_bytes),
                format_bytes_mib(total_bytes),
                mem_pct
            ),
            color: None,
        });
        children.push(Node::Spacer {
            size: Length::Px(8),
        });

        // TASKS
        children.push(label("TASKS"));
        let mut blocked = 0usize;
        for i in 0..self.task_count {
            if self.tasks[i].state == 3 {
                blocked += 1;
            }
        }
        let ready = self.sys_info.ready_tasks as usize;
        let active = self.sys_info.active_tasks as usize;
        children.push(label(&format!(
            "{} total  {} active  {} ready  {} blocked",
            self.task_count, active, ready, blocked
        )));
        children.push(Node::Spacer {
            size: Length::Px(8),
        });

        // NETWORK
        children.push(label("NETWORK"));
        children.push(label("RX: 0.0 MiB (0 pkts)  TX: 0.0 MiB (0 pkts)"));
        Node::Padding {
            padding: EdgeInsets::all(10),
            child: Box::new(Node::VStack {
                spacing: 2,
                align: CrossAxisAlignment::Stretch,
                children,
            }),
        }
    }

    fn view_processes(&self) -> Node {
        let sort_ind = |col: SortColumn| -> Option<SortIndicator> {
            if self.sort_column == col {
                Some(if self.sort_ascending {
                    SortIndicator::Ascending
                } else {
                    SortIndicator::Descending
                })
            } else {
                None
            }
        };

        let columns = vec![
            TableColumn {
                label: String::from("PID"),
                width: TableColumnWidth::Fixed(60),
                sort_indicator: sort_ind(SortColumn::Pid),
            },
            TableColumn {
                label: String::from("Name"),
                width: TableColumnWidth::Flex(2),
                sort_indicator: sort_ind(SortColumn::Name),
            },
            TableColumn {
                label: String::from("State"),
                width: TableColumnWidth::Fixed(70),
                sort_indicator: sort_ind(SortColumn::State),
            },
            TableColumn {
                label: String::from("CPU%"),
                width: TableColumnWidth::Fixed(70),
                sort_indicator: sort_ind(SortColumn::CpuPct),
            },
            TableColumn {
                label: String::from("Pri"),
                width: TableColumnWidth::Fixed(60),
                sort_indicator: sort_ind(SortColumn::Priority),
            },
            TableColumn {
                label: String::from("CPU"),
                width: TableColumnWidth::Fixed(50),
                sort_indicator: sort_ind(SortColumn::Cpu),
            },
            TableColumn {
                label: String::from("Runtime"),
                width: TableColumnWidth::Flex(1),
                sort_indicator: sort_ind(SortColumn::Runtime),
            },
        ];

        let mut rows: Vec<Vec<Node>> = Vec::new();
        for row_idx in 0..self.task_count {
            let Some(idx) = self.sorted_task_index(row_idx) else {
                continue;
            };
            let task = &self.tasks[idx];
            let (state_str, _) = task_state(task.state);
            rows.push(vec![
                cell(&format!("{}", task.task_id)),
                cell(&truncate_name(&task_name_string(task), 16)),
                cell(state_str),
                cell(&format_pct(self.task_cpu_pct[idx])),
                cell(priority_label(task.priority)),
                cell(&format!("{}", task.last_cpu)),
                cell(&format_runtime(task.total_runtime_us)),
            ]);
        }

        let selected = if self.task_count > 0 {
            Some(self.selected_row)
        } else {
            None
        };

        let table = Node::Table {
            columns,
            rows,
            row_height: 20,
            selected,
            on_select: MSG_SELECT,
            on_header_click: Some(MSG_SORT),
        };

        let mut children: Vec<Node> = vec![Node::Expand {
            weight: 1,
            child: Box::new(table),
        }];

        // Kill confirmation dialog as overlay.
        if let Some(pid) = self.confirm_kill {
            let task_name = if let Some(idx) = self.find_task_index_by_pid(pid) {
                task_name_string(&self.tasks[idx])
            } else {
                String::from("unknown")
            };
            children.push(Node::Dialog {
                title: format!("Kill task '{}' (PID {})?", task_name, pid),
                content: Box::new(Node::Label {
                    text: String::from("This action cannot be undone."),
                    alignment: TextAlignment::Center,
                    wrap: true,
                    max_lines: None,
                }),
                actions: vec![
                    Node::Button {
                        label: String::from("Kill"),
                        on_press: Some(MSG_KILL),
                        style: ButtonStyle::Destructive,
                        enabled: true,
                    },
                    Node::Button {
                        label: String::from("Cancel"),
                        on_press: Some(MSG_CANCEL),
                        style: ButtonStyle::Secondary,
                        enabled: true,
                    },
                ],
                on_dismiss: MSG_CANCEL,
            });
        }

        Node::VStack {
            spacing: 0,
            align: CrossAxisAlignment::Stretch,
            children,
        }
    }

    fn view_hardware(&self) -> Node {
        let mut children: Vec<Node> = Vec::new();

        // PROCESSOR
        children.push(label("PROCESSOR"));
        children.push(kv_row("Model", &trim_ascii(&self.cpu_info.brand_string)));
        children.push(kv_row("Vendor", &trim_ascii(&self.cpu_info.vendor)));
        children.push(kv_row("Cores", &format!("{}", self.cpu_info.cpu_count)));
        children.push(kv_row(
            "Family/Model",
            &format!(
                "{} / {} / step {}",
                self.cpu_info.family, self.cpu_info.model, self.cpu_info.stepping
            ),
        ));
        children.push(kv_row(
            "Features",
            &format_cpu_features(self.cpu_info.features),
        ));
        children.push(Node::Divider);

        // MEMORY
        children.push(label("MEMORY"));
        let total_bytes = (self.sys_info.total_pages as u64).saturating_mul(PAGE_SIZE);
        let used_bytes = (self.sys_info.allocated_pages.min(self.sys_info.total_pages) as u64)
            .saturating_mul(PAGE_SIZE);
        let free_bytes = (self.sys_info.free_pages as u64).saturating_mul(PAGE_SIZE);
        let mem_pct = if total_bytes == 0 {
            0
        } else {
            ((used_bytes.saturating_mul(100) / total_bytes).min(100)) as u32
        };
        children.push(Node::ProgressBar {
            value: mem_pct,
            label: format!(
                "{} / {} ({}%)",
                format_bytes_mib(used_bytes),
                format_bytes_mib(total_bytes),
                mem_pct
            ),
            color: None,
        });
        children.push(kv_row(
            "Total",
            &format!(
                "{} ({} pages)",
                format_bytes_mib(total_bytes),
                self.sys_info.total_pages
            ),
        ));
        children.push(kv_row("Free", &format_bytes_mib(free_bytes)));
        children.push(kv_row("Allocated", &format_bytes_mib(used_bytes)));
        children.push(Node::Divider);

        // SCHEDULER
        children.push(label("SCHEDULER"));
        children.push(kv_row(
            "Ctx switches",
            &format_number(self.sys_info.task_context_switches),
        ));
        children.push(kv_row(
            "Sched switches",
            &format_number(self.sys_info.scheduler_context_switches),
        ));
        children.push(kv_row(
            "Yields",
            &format_number(self.sys_info.scheduler_yields),
        ));
        children.push(kv_row(
            "Schedule calls",
            &format_number(self.sys_info.schedule_calls as u64),
        ));
        children.push(Node::Divider);

        // HEAP
        children.push(label("HEAP"));
        let stats = heap_stats();
        children.push(kv_row(
            "Heap size",
            &format_bytes_mib(stats.heap_size as u64),
        ));
        children.push(kv_row(
            "Wilderness",
            &format_bytes_mib(stats.wilderness as u64),
        ));
        children.push(kv_row(
            "Mmap allocs",
            &format_number(stats.mmap_count as u64),
        ));
        children.push(Node::Divider);

        // NETWORK
        children.push(label("NETWORK"));
        let net_status = if self.net_info.nic_ready != 0 {
            if self.net_info.link_up != 0 {
                "Online"
            } else {
                "No link"
            }
        } else {
            "Offline"
        };
        children.push(kv_row("Status", net_status));
        children.push(kv_row(
            "IP",
            &format!(
                "{}.{}.{}.{}",
                self.net_info.ipv4[0],
                self.net_info.ipv4[1],
                self.net_info.ipv4[2],
                self.net_info.ipv4[3]
            ),
        ));
        children.push(kv_row(
            "MAC",
            &format!(
                "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                self.net_info.mac[0],
                self.net_info.mac[1],
                self.net_info.mac[2],
                self.net_info.mac[3],
                self.net_info.mac[4],
                self.net_info.mac[5]
            ),
        ));
        children.push(Node::Divider);

        // BOOT
        children.push(label("BOOT"));
        children.push(kv_row("Uptime", &format_uptime(self.last_refresh_ms)));
        children.push(kv_row(
            "Boot flags",
            &format!("0x{:08x}", self.sys_info.boot_flags),
        ));
        children.push(kv_row(
            "W/L Balance",
            &format_number(self.sys_info.wl_balance as u64),
        ));

        Node::ScrollView {
            direction: ScrollDirection::Vertical,
            show_scrollbar: ScrollbarVisibility::WhenNeeded,
            scroll_y: self.hardware_scroll_y,
            on_scroll: Some(MSG_HW_SCROLL),
            child: Box::new(Node::Padding {
                padding: EdgeInsets::all(10),
                child: Box::new(Node::VStack {
                    spacing: 2,
                    align: CrossAxisAlignment::Stretch,
                    children,
                }),
            }),
        }
    }
}

fn label(text: &str) -> Node {
    Node::Label {
        text: String::from(text),
        alignment: TextAlignment::Start,
        wrap: false,
        max_lines: None,
    }
}

fn cell(text: &str) -> Node {
    Node::Label {
        text: String::from(text),
        alignment: TextAlignment::Start,
        wrap: false,
        max_lines: None,
    }
}

fn kv_row(key: &str, value: &str) -> Node {
    Node::HStack {
        spacing: 0,
        align: CrossAxisAlignment::Center,
        children: vec![
            Node::Label {
                text: String::from(key),
                alignment: TextAlignment::Start,
                wrap: false,
                max_lines: None,
            },
            Node::Spacer {
                size: Length::Px(8),
            },
            Node::Label {
                text: String::from(value),
                alignment: TextAlignment::Start,
                wrap: false,
                max_lines: None,
            },
        ],
    }
}

pub fn sysmon_main() -> ! {
    let app = SysmonApp::new();
    crate::appkit::run_app(app, 640, 480)
}
