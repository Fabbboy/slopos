use slopos_abi::draw::Color32;

use crate::appkit::{self, ControlFlow, Event, Window, WindowedApp};
use crate::gfx::DrawBuffer;
use crate::gfx::fill_rect;
use crate::gfx::font;

mod format;
mod hardware;
mod overview;
mod processes;
mod state;
mod widgets;

pub(crate) use format::*;
pub(crate) use state::{ContextMenu, SortColumn, SysmonApp, Tab};

pub(crate) const COLOR_BG: Color32 = Color32::rgb(0x0A, 0x0E, 0x14);
pub(crate) const COLOR_TEXT: Color32 = Color32::rgb(0xD0, 0xD0, 0xD0);
pub(crate) const COLOR_DIM: Color32 = Color32::rgb(0x60, 0x68, 0x70);
pub(crate) const COLOR_BRIGHT: Color32 = Color32::rgb(0xFF, 0xFF, 0xFF);
pub(crate) const COLOR_TAB_ACTIVE: Color32 = Color32::rgb(0x30, 0x6C, 0xB0);
pub(crate) const COLOR_TAB_INACTIVE: Color32 = Color32::rgb(0x1A, 0x22, 0x2E);
pub(crate) const COLOR_TAB_BAR: Color32 = Color32::rgb(0x12, 0x18, 0x20);
pub(crate) const COLOR_HEADER: Color32 = Color32::rgb(0x18, 0x20, 0x2A);
pub(crate) const COLOR_ROW_EVEN: Color32 = Color32::rgb(0x0E, 0x14, 0x1C);
pub(crate) const COLOR_ROW_ODD: Color32 = Color32::rgb(0x12, 0x18, 0x22);
pub(crate) const COLOR_ROW_SELECTED: Color32 = Color32::rgb(0x1E, 0x3A, 0x5C);
pub(crate) const COLOR_BAR_LOW: Color32 = Color32::rgb(0x2E, 0xAA, 0x4E);
pub(crate) const COLOR_BAR_MED: Color32 = Color32::rgb(0xCC, 0xAA, 0x22);
pub(crate) const COLOR_BAR_HIGH: Color32 = Color32::rgb(0xCC, 0x33, 0x33);
pub(crate) const COLOR_BAR_BG: Color32 = Color32::rgb(0x1A, 0x20, 0x28);
pub(crate) const COLOR_SECTION: Color32 = Color32::rgb(0x50, 0x90, 0xD0);
pub(crate) const COLOR_STATE_RUN: Color32 = Color32::rgb(0x44, 0xCC, 0x44);
pub(crate) const COLOR_STATE_BLOCK: Color32 = Color32::rgb(0xCC, 0xAA, 0x44);
pub(crate) const COLOR_STATE_READY: Color32 = Color32::rgb(0xCC, 0xCC, 0xCC);
pub(crate) const COLOR_KILL_RED: Color32 = Color32::rgb(0xDD, 0x33, 0x33);

pub(crate) const MAX_CPUS: usize = 16;
pub(crate) const REFRESH_INTERVAL_MS: u64 = 1000;
pub(crate) const TAB_HEIGHT: i32 = 26;
pub(crate) const TAB_WIDTH: i32 = 120;

pub(crate) const PROCESS_HEADER_Y: i32 = TAB_HEIGHT + 2;
pub(crate) fn process_header_h() -> i32 { font::cell_height() }
pub(crate) fn process_rows_y() -> i32 { PROCESS_HEADER_Y + process_header_h() }
pub(crate) fn process_row_h() -> i32 { font::cell_height() }
pub(crate) const PROCESS_STATUS_H: i32 = 20;

pub(crate) const COL_PID_X: i32 = 8;
pub(crate) const COL_NAME_X: i32 = 58;
pub(crate) const COL_STATE_X: i32 = 188;
pub(crate) const COL_CPU_PCT_X: i32 = 248;
pub(crate) const COL_PRI_X: i32 = 313;
pub(crate) const COL_CPU_X: i32 = 363;
pub(crate) const COL_RUNTIME_X: i32 = 403;

impl WindowedApp for SysmonApp {
    fn init(&mut self, win: &mut Window) {
        win.set_title("System Monitor");
        win.request_redraw();
    }

    fn on_event(&mut self, win: &mut Window, event: Event) -> ControlFlow {
        match event {
            Event::CloseRequest => return ControlFlow::Exit,
            Event::PointerMotion { x, y } => {
                self.pointer_x = x;
                self.pointer_y = y;
                if self.confirm_kill.is_some() {
                    self.update_confirm_kill_hover(win.width() as i32, win.height() as i32);
                }
            }
            Event::KeyPress { scancode, ascii } => {
                if ascii == b'\t' {
                    self.cycle_tab();
                } else if self.active_tab == Tab::Processes {
                    match scancode {
                        0x48 => self.move_selection_up(),
                        0x50 => self.move_selection_down(),
                        _ => {}
                    }
                }
            }
            Event::PointerPress { button } => {
                let width = win.width() as i32;
                let height = win.height() as i32;
                let (x, y) = (self.pointer_x, self.pointer_y);
                if button == 1 {
                    if self.handle_confirm_kill_click(width, height, x, y) {
                        win.request_redraw();
                        return ControlFlow::Continue;
                    }
                    if self.handle_context_menu_click(x, y) {
                        win.request_redraw();
                        return ControlFlow::Continue;
                    }
                    if self.handle_tab_click(x, y) {
                        win.request_redraw();
                        return ControlFlow::Continue;
                    }
                    if self.active_tab == Tab::Processes {
                        self.handle_processes_left_click(width, height, x, y);
                    }
                } else if button == 2 {
                    if self.active_tab == Tab::Processes && self.confirm_kill.is_none() {
                        self.handle_processes_right_click(width, height, x, y);
                    } else {
                        self.context_menu = None;
                    }
                }
            }
            _ => {}
        }

        win.request_redraw();
        ControlFlow::Continue
    }

    fn draw(&mut self, fb: &mut DrawBuffer<'_>) {
        let now = crate::syscall::core::get_time_ms();
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

pub fn sysmon_main() -> ! {
    appkit::run(SysmonApp::new(), 640, 480)
}
