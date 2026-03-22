use core::option::Option::{None, Some};

use std::format;
use std::string::String;

use slopos_abi::DamageRect;

use crate::gfx::{DrawBuffer, draw_rect, fill_rect, fill_rect_clipped};
use crate::syscall::process as sys_proc;

use super::{
    COL_CPU_PCT_X, COL_CPU_X, COL_NAME_X, COL_PID_X, COL_PRI_X, COL_RUNTIME_X, COL_STATE_X,
    COLOR_BG, COLOR_BRIGHT, COLOR_DIM, COLOR_HEADER, COLOR_KILL_RED, COLOR_ROW_EVEN, COLOR_ROW_ODD,
    COLOR_ROW_SELECTED, COLOR_TAB_INACTIVE, ContextMenu, PROCESS_HEADER_H, PROCESS_HEADER_Y,
    PROCESS_ROW_H, PROCESS_ROWS_Y, PROCESS_STATUS_H, SysmonApp, Tab, format_pct, format_runtime,
    priority_label, task_name_string, task_state, truncate_name,
};

const CONTEXT_MENU_W: i32 = 140;
const CONTEXT_MENU_ITEM_H: i32 = 20;
const CONTEXT_MENU_H: i32 = CONTEXT_MENU_ITEM_H * 2 + 2;

const KILL_DIALOG_W: i32 = 300;
const KILL_DIALOG_H: i32 = 80;
const KILL_BUTTON_W: i32 = 80;
const KILL_BUTTON_H: i32 = 24;

#[derive(Clone, Copy)]
struct DialogLayout {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    kill_x: i32,
    cancel_x: i32,
    btn_y: i32,
}

impl SysmonApp {
    pub(crate) fn draw_processes(&mut self, fb: &mut DrawBuffer<'_>, width: i32, height: i32) {
        let max_rows = self.process_max_rows(height);
        self.ensure_process_scroll(max_rows);

        fill_rect(
            fb,
            0,
            PROCESS_HEADER_Y,
            width,
            PROCESS_HEADER_H + 2,
            COLOR_HEADER,
        );

        self.draw_header_cell(
            fb,
            COL_PID_X,
            PROCESS_HEADER_Y,
            "PID",
            super::SortColumn::Pid,
        );
        self.draw_header_cell(
            fb,
            COL_NAME_X,
            PROCESS_HEADER_Y,
            "Name",
            super::SortColumn::Name,
        );
        self.draw_header_cell(
            fb,
            COL_STATE_X,
            PROCESS_HEADER_Y,
            "State",
            super::SortColumn::State,
        );
        self.draw_header_cell(
            fb,
            COL_CPU_PCT_X,
            PROCESS_HEADER_Y,
            "CPU%",
            super::SortColumn::CpuPct,
        );
        self.draw_header_cell(
            fb,
            COL_PRI_X,
            PROCESS_HEADER_Y,
            "Pri",
            super::SortColumn::Priority,
        );
        self.draw_header_cell(
            fb,
            COL_CPU_X,
            PROCESS_HEADER_Y,
            "CPU",
            super::SortColumn::Cpu,
        );
        self.draw_header_cell(
            fb,
            COL_RUNTIME_X,
            PROCESS_HEADER_Y,
            "Runtime",
            super::SortColumn::Runtime,
        );

        let rows_h = (height - PROCESS_ROWS_Y - PROCESS_STATUS_H).max(0);
        let clip = DamageRect {
            x0: 0,
            y0: PROCESS_ROWS_Y,
            x1: width - 1,
            y1: PROCESS_ROWS_Y + rows_h - 1,
        };
        fill_rect(fb, 0, PROCESS_ROWS_Y, width, rows_h, COLOR_BG);

        for row in 0..max_rows {
            let task_row = self.scroll_offset + row;
            if task_row >= self.task_count {
                break;
            }
            let Some(idx) = self.sorted_task_index(task_row) else {
                continue;
            };
            let task = &self.tasks[idx];
            let y = PROCESS_ROWS_Y + (row as i32) * PROCESS_ROW_H;

            let row_bg = if task_row == self.selected_row {
                COLOR_ROW_SELECTED
            } else if row % 2 == 0 {
                COLOR_ROW_EVEN
            } else {
                COLOR_ROW_ODD
            };
            fill_rect_clipped(fb, 0, y, width, PROCESS_ROW_H, row_bg, &clip);

            let (state, state_color) = task_state(task.state);

            let pid = format!("{}", task.task_id);
            Self::text(fb, COL_PID_X, y, &pid, super::COLOR_TEXT, row_bg);
            let name = truncate_name(&task_name_string(task), 16);
            Self::text(fb, COL_NAME_X, y, &name, super::COLOR_TEXT, row_bg);
            Self::text(fb, COL_STATE_X, y, state, state_color, row_bg);
            let cpu_pct = format_pct(self.task_cpu_pct[idx]);
            Self::text(fb, COL_CPU_PCT_X, y, &cpu_pct, super::COLOR_TEXT, row_bg);
            Self::text(
                fb,
                COL_PRI_X,
                y,
                priority_label(task.priority),
                super::COLOR_TEXT,
                row_bg,
            );
            let last_cpu = format!("{}", task.last_cpu);
            Self::text(fb, COL_CPU_X, y, &last_cpu, super::COLOR_TEXT, row_bg);
            let runtime = format_runtime(task.total_runtime_us);
            Self::text(fb, COL_RUNTIME_X, y, &runtime, super::COLOR_TEXT, row_bg);
        }

        let status_y = height - PROCESS_STATUS_H;
        fill_rect(fb, 0, status_y, width, PROCESS_STATUS_H, COLOR_HEADER);
        Self::text(
            fb,
            6,
            status_y + 2,
            &format!(
                "{} tasks | Tab Switch | Up/Down Navigate | Right-click Menu",
                self.task_count
            ),
            COLOR_DIM,
            COLOR_HEADER,
        );

        if self.context_menu.is_some() {
            self.draw_context_menu(fb, width, height);
        }
        if let Some(pid) = self.confirm_kill {
            self.draw_kill_confirm_dialog(fb, width, height, pid);
        }
    }

    pub(crate) fn handle_tab_click(&mut self, x: i32, y: i32) -> bool {
        if y >= super::TAB_HEIGHT {
            return false;
        }

        let tab_step = super::TAB_WIDTH + 2;
        if x < 4 {
            return false;
        }

        let idx = (x - 4) / tab_step;
        let tab_x = 4 + idx * tab_step;
        if idx < 0 || idx >= 3 || x >= tab_x + super::TAB_WIDTH {
            return false;
        }

        self.active_tab = match idx {
            0 => Tab::Overview,
            1 => Tab::Processes,
            _ => Tab::Hardware,
        };
        self.context_menu = None;
        self.confirm_kill = None;
        self.confirm_kill_hover = 0;
        true
    }

    pub(crate) fn handle_processes_left_click(&mut self, width: i32, height: i32, x: i32, y: i32) {
        if let Some(col) = self.process_sort_column_from_pointer(x, y) {
            self.cycle_sort_for_column(col);
            self.context_menu = None;
            return;
        }

        if let Some(row) = self.process_row_from_pointer(height, y) {
            self.selected_row = row;
            self.context_menu = None;
        } else if x >= 0 && x < width {
            self.context_menu = None;
        }
    }

    pub(crate) fn handle_processes_right_click(&mut self, width: i32, height: i32, x: i32, y: i32) {
        if let Some(row) = self.process_row_from_pointer(height, y)
            && let Some(task_idx) = self.sorted_task_index(row)
        {
            self.selected_row = row;
            let (mx, my) = clamp_menu_position(width, height, x, y);
            self.context_menu = Some(ContextMenu {
                task_id: self.tasks[task_idx].task_id,
                task_name: self.tasks[task_idx].name,
                x: mx,
                y: my,
            });
            return;
        }
        self.context_menu = None;
    }

    pub(crate) fn handle_context_menu_click(&mut self, x: i32, y: i32) -> bool {
        let Some(menu) = self.context_menu else {
            return false;
        };

        let item1_y = menu.y + 1;
        let in_x = x >= menu.x && x < menu.x + CONTEXT_MENU_W;
        let in_y = y >= menu.y && y < menu.y + CONTEXT_MENU_H;
        if in_x && in_y {
            if y >= item1_y && y < item1_y + CONTEXT_MENU_ITEM_H {
                self.confirm_kill = Some(menu.task_id);
                self.confirm_kill_hover = 0;
            }
            self.context_menu = None;
            return true;
        }

        self.context_menu = None;
        true
    }

    pub(crate) fn handle_confirm_kill_click(
        &mut self,
        width: i32,
        height: i32,
        x: i32,
        y: i32,
    ) -> bool {
        let Some(pid) = self.confirm_kill else {
            return false;
        };

        let layout = confirm_dialog_layout(width, height);
        let inside_dialog = in_rect(x, y, layout.x, layout.y, layout.w, layout.h);
        if !inside_dialog {
            self.confirm_kill = None;
            self.confirm_kill_hover = 0;
            return true;
        }

        if in_rect(
            x,
            y,
            layout.kill_x,
            layout.btn_y,
            KILL_BUTTON_W,
            KILL_BUTTON_H,
        ) {
            let _ = sys_proc::kill(pid, 9);
            self.confirm_kill = None;
            self.confirm_kill_hover = 0;
            self.context_menu = None;
            self.refresh_data();
            return true;
        }

        if in_rect(
            x,
            y,
            layout.cancel_x,
            layout.btn_y,
            KILL_BUTTON_W,
            KILL_BUTTON_H,
        ) {
            self.confirm_kill = None;
            self.confirm_kill_hover = 0;
            self.context_menu = None;
            return true;
        }

        true
    }

    pub(crate) fn update_confirm_kill_hover(&mut self, width: i32, height: i32) {
        if self.confirm_kill.is_none() {
            self.confirm_kill_hover = 0;
            return;
        }
        let layout = confirm_dialog_layout(width, height);
        if in_rect(
            self.pointer_x,
            self.pointer_y,
            layout.kill_x,
            layout.btn_y,
            KILL_BUTTON_W,
            KILL_BUTTON_H,
        ) {
            self.confirm_kill_hover = 1;
        } else if in_rect(
            self.pointer_x,
            self.pointer_y,
            layout.cancel_x,
            layout.btn_y,
            KILL_BUTTON_W,
            KILL_BUTTON_H,
        ) {
            self.confirm_kill_hover = 2;
        } else {
            self.confirm_kill_hover = 0;
        }
    }

    fn draw_context_menu(&self, fb: &mut DrawBuffer<'_>, width: i32, height: i32) {
        let Some(menu) = self.context_menu else {
            return;
        };
        let (x, y) = clamp_menu_position(width, height, menu.x, menu.y);

        fill_rect(fb, x, y, CONTEXT_MENU_W, CONTEXT_MENU_H, COLOR_HEADER);
        draw_rect(fb, x, y, CONTEXT_MENU_W, CONTEXT_MENU_H, COLOR_DIM);

        let item1_y = y + 1;
        let item2_y = item1_y + CONTEXT_MENU_ITEM_H;
        let hover_in_menu = self.pointer_x >= x && self.pointer_x < x + CONTEXT_MENU_W;
        let hover_item1 = hover_in_menu
            && self.pointer_y >= item1_y
            && self.pointer_y < item1_y + CONTEXT_MENU_ITEM_H;
        let hover_item2 = hover_in_menu
            && self.pointer_y >= item2_y
            && self.pointer_y < item2_y + CONTEXT_MENU_ITEM_H;

        if hover_item1 {
            fill_rect(
                fb,
                x + 1,
                item1_y,
                CONTEXT_MENU_W - 2,
                CONTEXT_MENU_ITEM_H,
                COLOR_ROW_SELECTED,
            );
        }
        if hover_item2 {
            fill_rect(
                fb,
                x + 1,
                item2_y,
                CONTEXT_MENU_W - 2,
                CONTEXT_MENU_ITEM_H,
                COLOR_ROW_SELECTED,
            );
        }

        draw_rect(fb, x + 1, item2_y - 1, CONTEXT_MENU_W - 2, 1, COLOR_DIM);

        let title = if menu.task_name[0] == 0 {
            "Kill Process"
        } else {
            "Kill Process"
        };
        Self::text(fb, x + 8, item1_y + 3, title, COLOR_KILL_RED, COLOR_HEADER);
        Self::text(
            fb,
            x + 8,
            item2_y + 3,
            "Cancel",
            super::COLOR_TEXT,
            COLOR_HEADER,
        );
    }

    fn draw_kill_confirm_dialog(&self, fb: &mut DrawBuffer<'_>, width: i32, height: i32, pid: u32) {
        let layout = confirm_dialog_layout(width, height);
        fill_rect(fb, layout.x, layout.y, layout.w, layout.h, COLOR_HEADER);
        draw_rect(fb, layout.x, layout.y, layout.w, layout.h, COLOR_DIM);
        draw_rect(
            fb,
            layout.x + 1,
            layout.y + 1,
            layout.w - 2,
            layout.h - 2,
            COLOR_DIM,
        );

        let task_name = if let Some(idx) = self.find_task_index_by_pid(pid) {
            task_name_string(&self.tasks[idx])
        } else {
            String::from("unknown")
        };
        let title = format!("Kill task '{}' (PID {})?", task_name, pid);
        let text_x = layout.x + ((layout.w - (title.len() as i32 * 8)) / 2).max(8);
        Self::text(
            fb,
            text_x,
            layout.y + 12,
            &title,
            COLOR_BRIGHT,
            COLOR_HEADER,
        );

        let kill_bg = if self.confirm_kill_hover == 1 {
            super::COLOR_BAR_HIGH
        } else {
            COLOR_KILL_RED
        };
        let cancel_bg = if self.confirm_kill_hover == 2 {
            super::COLOR_TAB_ACTIVE
        } else {
            COLOR_TAB_INACTIVE
        };

        fill_rect(
            fb,
            layout.kill_x,
            layout.btn_y,
            KILL_BUTTON_W,
            KILL_BUTTON_H,
            kill_bg,
        );
        draw_rect(
            fb,
            layout.kill_x,
            layout.btn_y,
            KILL_BUTTON_W,
            KILL_BUTTON_H,
            COLOR_DIM,
        );
        fill_rect(
            fb,
            layout.cancel_x,
            layout.btn_y,
            KILL_BUTTON_W,
            KILL_BUTTON_H,
            cancel_bg,
        );
        draw_rect(
            fb,
            layout.cancel_x,
            layout.btn_y,
            KILL_BUTTON_W,
            KILL_BUTTON_H,
            COLOR_DIM,
        );

        let kill_text_x = layout.kill_x + (KILL_BUTTON_W - (4 * 8)) / 2;
        let cancel_text_x = layout.cancel_x + (KILL_BUTTON_W - (6 * 8)) / 2;
        Self::text(
            fb,
            kill_text_x,
            layout.btn_y + 4,
            "Kill",
            COLOR_BRIGHT,
            kill_bg,
        );
        Self::text(
            fb,
            cancel_text_x,
            layout.btn_y + 4,
            "Cancel",
            super::COLOR_TEXT,
            cancel_bg,
        );
    }
}

fn clamp_menu_position(width: i32, height: i32, x: i32, y: i32) -> (i32, i32) {
    let max_x = (width - CONTEXT_MENU_W).max(0);
    let max_y = (height - CONTEXT_MENU_H).max(0);
    (x.clamp(0, max_x), y.clamp(0, max_y))
}

fn confirm_dialog_layout(width: i32, height: i32) -> DialogLayout {
    let x = ((width - KILL_DIALOG_W) / 2).max(0);
    let y = ((height - KILL_DIALOG_H) / 2).max(0);
    let total_buttons_w = (KILL_BUTTON_W * 2) + 40;
    let buttons_x = x + ((KILL_DIALOG_W - total_buttons_w) / 2);
    DialogLayout {
        x,
        y,
        w: KILL_DIALOG_W,
        h: KILL_DIALOG_H,
        kill_x: buttons_x,
        cancel_x: buttons_x + KILL_BUTTON_W + 40,
        btn_y: y + KILL_DIALOG_H - KILL_BUTTON_H - 10,
    }
}

fn in_rect(px: i32, py: i32, x: i32, y: i32, w: i32, h: i32) -> bool {
    px >= x && px < x + w && py >= y && py < y + h
}
