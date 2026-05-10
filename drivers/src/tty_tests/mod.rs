#![allow(dead_code, unused_imports)]

//! Regression tests for the TTY subsystem.
//!
//! Tests the `LineDisc`, `TtyDriverKind`, `TtyIndex`, TTY table, and
//! the per-TTY public API (compositor focus, foreground pgrp, active TTY).
//!
//! Coverage includes input flag processing, output processing, signal
//! generation, flow control, VLNEXT, VWERASE, ECHOCTL, compositor focus /
//! fg_pgrp split, check_read() as sole read gate, TtyIndex type safety,
//! and signal constant verification.

use slopos_abi::signal::{SIGCONT, SIGHUP, SIGINT, SIGQUIT, SIGTSTP, SIGTTIN, SIGTTOU, SIGWINCH};
use slopos_abi::syscall::{
    CcIndex, ControlFlags, InputFlags, LocalFlags, OutputFlags, POSIX_VDISABLE,
};
use slopos_testing::TestResult;
use slopos_utils::klog_info;
use slopos_utils::ring_buffer::RingBuffer;

use crate::tty;
use crate::tty::TtyError;
use crate::tty::TtyIndex;
use crate::tty::driver::{DriverId, TtyDriverKind, VConsoleDriver};
use crate::tty::ldisc::{InputAction, LdiscKind, LineDisc, OutputAction, RawDisc};
use crate::tty::session::TtySession;
use crate::tty::session::{
    ForegroundCheck, NO_FOREGROUND_PGRP, NO_SESSION, ProcessGroupId, SessionId,
};
use crate::tty::table::{TTY_GENERATIONS, TTY_OUTPUT_INFLIGHT, TTY_SLOTS};
use crate::tty::vconsole::{
    Cell, CellAttributes, CellGrid, CursorAttributes, VCONSOLE_MAX_COLS, VCONSOLE_MAX_ROWS,
    VConsoleState,
};
use crate::tty::vtparser::{Direction, EraseMode, SgrAttr, VtAction, VtParser};

use crate::tty::pty::PtyPeerHandle;

pub(crate) fn boxed_vconsole_state() -> slopos_ostd::KBox<VConsoleState> {
    let mut state = slopos_ostd::KBox::try_init(VConsoleState::init_default()).expect("test alloc");
    state.rows = 25;
    state.cols = 80;
    state.cursor_attrs = CursorAttributes {
        fg: 0x00AAAAAA,
        bg: 0x00000000,
        bold: false,
        underline: false,
        inverse: false,
    };
    state.saved_cursor_attrs = state.cursor_attrs;
    state.cells.allocate(VCONSOLE_MAX_ROWS, VCONSOLE_MAX_COLS);
    state
        .alt_cells
        .allocate(VCONSOLE_MAX_ROWS, VCONSOLE_MAX_COLS);
    state
}

pub(crate) fn drain_tty_nonblock(idx: TtyIndex) {
    let mut scratch = [0u8; 64];
    loop {
        match tty::read(idx, &mut scratch, true) {
            Ok(0) | Err(_) => break,
            Ok(_) => continue,
        }
    }
}

pub mod fixtures;
pub mod test_driver;
pub mod test_integration;
pub mod test_ioctls;
pub mod test_ioctls_ext;
pub mod test_ldisc_core;
pub mod test_ldisc_flags;
pub mod test_ldisc_flow;
pub mod test_ldisc_noncanon;
pub mod test_ldisc_regression;
pub mod test_ldisc_signals;
pub mod test_ldisc_utf8;
pub mod test_poll;
pub mod test_pty;
pub mod test_pty_core;
pub mod test_pty_packet;
pub mod test_rawdisc;
pub mod test_ringbuf;
pub mod test_session;
pub mod test_session_fg;
pub mod test_table;
pub mod test_vconsole;
pub mod test_vtparser;

pub use test_driver::*;
pub use test_integration::*;
pub use test_ioctls::*;
pub use test_ioctls_ext::*;
pub use test_ldisc_core::*;
pub use test_ldisc_flags::*;
pub use test_ldisc_flow::*;
pub use test_ldisc_noncanon::*;
pub use test_ldisc_regression::*;
pub use test_ldisc_signals::*;
pub use test_ldisc_utf8::*;
pub use test_poll::*;
pub use test_pty::*;
pub use test_pty_core::*;
pub use test_pty_packet::*;
pub use test_rawdisc::*;
pub use test_ringbuf::*;
pub use test_session::*;
pub use test_session_fg::*;
pub use test_table::*;
pub use test_vconsole::*;
pub use test_vtparser::*;

slopos_testing::stest!(name = test_ringbuf_new_is_empty, suite = tty);
slopos_testing::stest!(name = test_ringbuf_push_pop, suite = tty);
slopos_testing::stest!(name = test_ringbuf_full_returns_false, suite = tty);
slopos_testing::stest!(name = test_ringbuf_peek_does_not_consume, suite = tty);
slopos_testing::stest!(name = test_ringbuf_peek_at_offset, suite = tty);
slopos_testing::stest!(name = test_ringbuf_read_bulk, suite = tty);
slopos_testing::stest!(name = test_ringbuf_read_partial, suite = tty);
slopos_testing::stest!(name = test_ringbuf_flush_resets, suite = tty);
slopos_testing::stest!(name = test_ringbuf_wraparound, suite = tty);
slopos_testing::stest!(name = test_ringbuf_capacity_and_free, suite = tty);
slopos_testing::stest!(name = test_pty_data_roundtrip, suite = tty);
slopos_testing::stest!(name = test_pty_hangup_propagation, suite = tty);
slopos_testing::stest!(name = test_errno_background_maps_to_eio, suite = tty);
slopos_testing::stest!(name = test_ldisc_ringbuf_integration, suite = tty);
slopos_testing::stest!(name = test_echo_batching_correctness, suite = tty);
slopos_testing::stest!(name = test_ldisc_new_has_no_data, suite = tty);
slopos_testing::stest!(name = test_ldisc_read_empty, suite = tty);
slopos_testing::stest!(name = test_ldisc_canonical_newline, suite = tty);
slopos_testing::stest!(name = test_ldisc_canonical_backspace, suite = tty);
slopos_testing::stest!(name = test_ldisc_canonical_kill, suite = tty);
slopos_testing::stest!(name = test_ldisc_canonical_eof, suite = tty);
slopos_testing::stest!(name = test_ldisc_signal_ctrl_c, suite = tty);
slopos_testing::stest!(name = test_ldisc_raw_mode, suite = tty);
slopos_testing::stest!(name = test_ldisc_set_termios_flush, suite = tty);
slopos_testing::stest!(name = test_ldisc_flush_all, suite = tty);
slopos_testing::stest!(name = test_ldisc_echo_printable, suite = tty);
slopos_testing::stest!(name = test_ldisc_echo_newline, suite = tty);
slopos_testing::stest!(name = test_ldisc_multiple_reads, suite = tty);
slopos_testing::stest!(name = test_ldisc_backspace_empty, suite = tty);
slopos_testing::stest!(name = test_session_new_empty, suite = tty);
slopos_testing::stest!(name = test_session_attach, suite = tty);
slopos_testing::stest!(name = test_session_detach, suite = tty);
slopos_testing::stest!(name = test_session_check_read_foreground, suite = tty);
slopos_testing::stest!(name = test_session_check_read_background, suite = tty);
slopos_testing::stest!(name = test_session_check_read_no_session, suite = tty);
slopos_testing::stest!(name = test_session_check_read_kernel_task, suite = tty);
slopos_testing::stest!(name = test_session_check_write_no_tostop, suite = tty);
slopos_testing::stest!(
    name = test_session_check_write_tostop_background,
    suite = tty
);
// check_read replaces task_has_access
slopos_testing::stest!(
    name = test_session_check_read_replaces_task_has_access_foreground,
    suite = tty
);
slopos_testing::stest!(
    name = test_session_check_read_replaces_task_has_access_background,
    suite = tty
);
slopos_testing::stest!(
    name = test_session_check_read_replaces_task_has_access_permissive,
    suite = tty
);
slopos_testing::stest!(name = test_session_set_fg_pgrp_checked_allowed, suite = tty);
slopos_testing::stest!(name = test_session_set_fg_pgrp_checked_denied, suite = tty);
slopos_testing::stest!(
    name = test_session_set_fg_pgrp_checked_no_session,
    suite = tty
);
slopos_testing::stest!(name = test_tty_get_session_id_default, suite = tty);
slopos_testing::stest!(name = test_tty_attach_session, suite = tty);
slopos_testing::stest!(name = test_tty_detach_session, suite = tty);
slopos_testing::stest!(name = test_tty_detach_session_by_id, suite = tty);
slopos_testing::stest!(name = test_tty_set_fg_pgrp_checked, suite = tty);
slopos_testing::stest!(name = test_tty_index_eq, suite = tty);
slopos_testing::stest!(name = test_driver_none_no_panic, suite = tty);
slopos_testing::stest!(name = test_vconsole_drain_returns_zero, suite = tty);
slopos_testing::stest!(name = test_table_init_allocates_tty0_and_tty1, suite = tty);
slopos_testing::stest!(name = test_table_tty0_has_index_zero, suite = tty);
slopos_testing::stest!(name = test_table_tty0_active, suite = tty);
slopos_testing::stest!(name = test_table_with_tty_exists, suite = tty);
slopos_testing::stest!(name = test_table_with_tty_empty, suite = tty);
slopos_testing::stest!(name = test_active_tty_default, suite = tty);
slopos_testing::stest!(name = test_set_active_tty, suite = tty);
slopos_testing::stest!(name = test_foreground_pgrp, suite = tty);
slopos_testing::stest!(name = test_compositor_focus, suite = tty);
slopos_testing::stest!(
    name = test_keyboard_enter_scancode_reaches_active_tty,
    suite = tty
);
slopos_testing::stest!(
    name = test_keyboard_scancode_routes_to_active_tty_index,
    suite = tty
);
slopos_testing::stest!(
    name = test_keyboard_extended_up_arrow_reaches_tty,
    suite = tty
);
// Input flag processing
slopos_testing::stest!(name = test_ldisc_icrnl, suite = tty);
slopos_testing::stest!(name = test_ldisc_igncr, suite = tty);
slopos_testing::stest!(name = test_ldisc_inlcr, suite = tty);
slopos_testing::stest!(name = test_ldisc_istrip, suite = tty);
// Output processing
slopos_testing::stest!(name = test_ldisc_opost_onlcr, suite = tty);
slopos_testing::stest!(name = test_ldisc_opost_ocrnl, suite = tty);
slopos_testing::stest!(name = test_ldisc_output_raw, suite = tty);
// Signal generation
slopos_testing::stest!(name = test_ldisc_signal_ctrl_backslash, suite = tty);
slopos_testing::stest!(name = test_ldisc_signal_ctrl_z, suite = tty);
// Flow control
slopos_testing::stest!(name = test_ldisc_flow_control_ixon, suite = tty);
// ECHOCTL
slopos_testing::stest!(name = test_ldisc_echoctl, suite = tty);
// VLNEXT
slopos_testing::stest!(name = test_ldisc_vlnext, suite = tty);
// VWERASE
slopos_testing::stest!(name = test_ldisc_vwerase, suite = tty);
// edit_content / reprint
slopos_testing::stest!(name = test_ldisc_edit_content, suite = tty);
// Output processing via TTY write
slopos_testing::stest!(name = test_tty_write_returns_input_len, suite = tty);
// Input pipeline cleanup
slopos_testing::stest!(name = test_keyboard_input_event_delivery, suite = tty);
slopos_testing::stest!(name = test_keyboard_break_code_no_input, suite = tty);
slopos_testing::stest!(name = test_keyboard_modifier_no_input, suite = tty);
slopos_testing::stest!(name = test_keyboard_press_release_single_char, suite = tty);
slopos_testing::stest!(name = test_vconsole_drain_via_drain_hw_input, suite = tty);
slopos_testing::stest!(name = test_keyboard_multi_key_sequence, suite = tty);
// FD integration
slopos_testing::stest!(name = test_tty_write_output_processing, suite = tty);
slopos_testing::stest!(name = test_tty_write_raw_passthrough, suite = tty);
slopos_testing::stest!(name = test_tty_write_invalid_index, suite = tty);
slopos_testing::stest!(name = test_tty_per_tty_termios_isolation, suite = tty);
slopos_testing::stest!(name = test_tty_per_tty_winsize_isolation, suite = tty);
slopos_testing::stest!(name = test_tty_per_tty_fg_pgrp_isolation, suite = tty);
slopos_testing::stest!(name = test_tty_per_tty_has_data_isolation, suite = tty);
slopos_testing::stest!(name = test_tty_per_tty_session_isolation, suite = tty);
slopos_testing::stest!(name = test_tty_read_invalid_tty_returns_error, suite = tty);
// Control-Plane Correctness
slopos_testing::stest!(name = test_tty_index_abi_type, suite = tty);
slopos_testing::stest!(name = test_signal_constants, suite = tty);
slopos_testing::stest!(
    name = test_set_compositor_focus_does_not_set_fg_pgrp,
    suite = tty
);
slopos_testing::stest!(name = test_check_read_sole_gate_background, suite = tty);
slopos_testing::stest!(name = test_tty_open_count_lifecycle, suite = tty);
slopos_testing::stest!(
    name = test_tty_hangup_sets_flag_and_detaches_session,
    suite = tty
);
slopos_testing::stest!(name = test_tty_hangup_nonblock_read_eio, suite = tty);
slopos_testing::stest!(name = test_tty_hangup_blocking_read_eof, suite = tty);
slopos_testing::stest!(name = test_tty_error_variants, suite = tty);
slopos_testing::stest!(name = test_read_returns_result, suite = tty);
slopos_testing::stest!(name = test_read_invalid_index_error, suite = tty);
slopos_testing::stest!(name = test_read_not_allocated_error, suite = tty);
slopos_testing::stest!(name = test_write_returns_result, suite = tty);
slopos_testing::stest!(name = test_get_termios_returns_result, suite = tty);
slopos_testing::stest!(name = test_vmin0_vtime0_immediate_return, suite = tty);
slopos_testing::stest!(
    name = test_vmin0_vtime0_with_data_immediate_return,
    suite = tty
);
slopos_testing::stest!(name = test_vmin_enforcement, suite = tty);
slopos_testing::stest!(name = test_vmin_limited_by_buffer_size, suite = tty);
slopos_testing::stest!(
    name = test_canonical_to_noncanonical_preserves_buffered_data,
    suite = tty
);
slopos_testing::stest!(
    name = test_set_fg_pgrp_checked_permission_denied,
    suite = tty
);
slopos_testing::stest!(name = test_hangup_read_returns_hung_up, suite = tty);
// Per-TTY Locking & Performance
slopos_testing::stest!(name = test_per_tty_lock_independence, suite = tty);
slopos_testing::stest!(name = test_driver_id_round_trip, suite = tty);
slopos_testing::stest!(name = test_split_write_returns_input_len, suite = tty);
slopos_testing::stest!(name = test_idle_cb_iterates_all_ttys, suite = tty);
slopos_testing::stest!(name = test_merged_drain_read, suite = tty);
slopos_testing::stest!(name = test_with_tty_per_slot, suite = tty);
slopos_testing::stest!(name = test_driver_id_traits, suite = tty);
// Job Control Correctness
slopos_testing::stest!(name = test_sigttou_constant, suite = tty);
slopos_testing::stest!(
    name = test_check_write_tostop_blocks_background,
    suite = tty
);
slopos_testing::stest!(
    name = test_check_write_no_tostop_allows_background,
    suite = tty
);
slopos_testing::stest!(
    name = test_check_write_tostop_allows_foreground,
    suite = tty
);
slopos_testing::stest!(name = test_check_read_cross_session_rejected, suite = tty);
slopos_testing::stest!(name = test_check_read_same_session_foreground, suite = tty);
slopos_testing::stest!(name = test_check_read_kernel_task_allowed, suite = tty);
slopos_testing::stest!(name = test_tty_write_foreground_with_tostop, suite = tty);
// Non-Canonical Timing Fix
slopos_testing::stest!(
    name = test_vmin_vtime_enough_data_returns_immediately,
    suite = tty
);
slopos_testing::stest!(name = test_vmin_vtime_partial_nonblock, suite = tty);
slopos_testing::stest!(name = test_vmin_vtime_no_data_nonblock, suite = tty);
slopos_testing::stest!(
    name = test_vmin_vtime_interbyte_timeout_returns_partial,
    suite = tty
);
slopos_testing::stest!(name = test_ldisc_vmin_vtime_helper, suite = tty);
// Sane Defaults & Output Column Tracking
slopos_testing::stest!(name = test_default_termios_has_icrnl, suite = tty);
slopos_testing::stest!(name = test_default_termios_has_opost_onlcr, suite = tty);
slopos_testing::stest!(name = test_default_termios_has_full_lflag, suite = tty);
slopos_testing::stest!(name = test_output_column_tracking_printable, suite = tty);
slopos_testing::stest!(name = test_output_column_tracking_newline, suite = tty);
slopos_testing::stest!(name = test_output_column_tracking_cr, suite = tty);
slopos_testing::stest!(name = test_output_column_tracking_tab, suite = tty);
slopos_testing::stest!(name = test_output_column_tracking_backspace, suite = tty);
slopos_testing::stest!(name = test_onocr_at_column_zero, suite = tty);
slopos_testing::stest!(name = test_default_onlcr_newline_expands, suite = tty);
// ABI Signal Constant Unification
slopos_testing::stest!(name = test_signal_values_from_signal_module, suite = tty);
slopos_testing::stest!(name = test_ldisc_signal_uses_signal_module, suite = tty);
slopos_testing::stest!(name = test_hangup_signals_from_signal_module, suite = tty);
slopos_testing::stest!(
    name = test_job_control_signals_from_signal_module,
    suite = tty
);
// Responsibility Split — PTY Foundation
slopos_testing::stest!(name = test_session_id_zero_is_none, suite = tty);
slopos_testing::stest!(name = test_session_id_round_trip, suite = tty);
slopos_testing::stest!(name = test_pgrp_id_zero_is_none, suite = tty);
slopos_testing::stest!(name = test_pgrp_id_round_trip, suite = tty);
slopos_testing::stest!(name = test_session_option_fields, suite = tty);
slopos_testing::stest!(name = test_session_option_attach_detach, suite = tty);
slopos_testing::stest!(name = test_raw_disc_new_empty, suite = tty);
slopos_testing::stest!(name = test_raw_disc_input_read, suite = tty);
slopos_testing::stest!(name = test_raw_disc_output_passthrough, suite = tty);
slopos_testing::stest!(name = test_raw_disc_flush, suite = tty);
slopos_testing::stest!(name = test_ldisc_kind_ntty_delegation, suite = tty);
slopos_testing::stest!(name = test_ldisc_kind_raw_delegation, suite = tty);
slopos_testing::stest!(name = test_pty_driver_id_variants, suite = tty);
slopos_testing::stest!(name = test_pty_master_driver_kind, suite = tty);
slopos_testing::stest!(name = test_pty_slave_driver_kind, suite = tty);
// POSIX Quick Wins
slopos_testing::stest!(name = test_canonical_one_line_per_read, suite = tty);
slopos_testing::stest!(name = test_canonical_has_data_line_count, suite = tty);
slopos_testing::stest!(name = test_canonical_eof_line_boundary, suite = tty);
slopos_testing::stest!(name = test_sigwinch_constant, suite = tty);
slopos_testing::stest!(name = test_word_erase_path_boundary, suite = tty);
slopos_testing::stest!(name = test_word_erase_mixed_boundary, suite = tty);
slopos_testing::stest!(name = test_word_erase_trailing_spaces, suite = tty);
slopos_testing::stest!(name = test_canonical_small_buffer_read, suite = tty);
slopos_testing::stest!(name = test_tcsetsw_preserves_pending_input, suite = tty);
slopos_testing::stest!(name = test_tcsetsf_flushes_pending_input, suite = tty);
slopos_testing::stest!(
    name = test_read_with_attach_false_skips_auto_attach,
    suite = tty
);
slopos_testing::stest!(
    name = test_read_with_attach_true_skips_durable_attach,
    suite = tty
);
slopos_testing::stest!(
    name = test_acquire_and_release_controlling_terminal,
    suite = tty
);
slopos_testing::stest!(name = test_release_wrong_session_is_noop, suite = tty);
slopos_testing::stest!(name = test_get_ldisc_default_is_ntty, suite = tty);
slopos_testing::stest!(
    name = test_set_ldisc_round_trip_preserves_termios,
    suite = tty
);
slopos_testing::stest!(name = test_set_ldisc_invalid_id_rejected, suite = tty);
slopos_testing::stest!(name = test_pty_alloc_returns_master_and_slave, suite = tty);
slopos_testing::stest!(name = test_pty_master_to_slave_flow, suite = tty);
slopos_testing::stest!(name = test_pty_slave_to_master_flow, suite = tty);
slopos_testing::stest!(name = test_master_close_hangs_up_slave, suite = tty);
slopos_testing::stest!(name = test_slave_close_returns_master_eof, suite = tty);
slopos_testing::stest!(name = test_pty_canonical_editing_on_slave, suite = tty);
// Strict Session Gates & Foreground Outcomes
slopos_testing::stest!(name = test_bootstrap_allowed_no_session_read, suite = tty);
slopos_testing::stest!(name = test_bootstrap_allowed_no_fg_pgrp, suite = tty);
slopos_testing::stest!(name = test_denied_cross_session_read, suite = tty);
slopos_testing::stest!(name = test_denied_cross_session_write_tostop, suite = tty);
slopos_testing::stest!(
    name = test_cross_session_write_no_tostop_still_denied,
    suite = tty
);
slopos_testing::stest!(
    name = test_kernel_task_exempted_cross_session_read,
    suite = tty
);
slopos_testing::stest!(
    name = test_kernel_task_exempted_cross_session_write,
    suite = tty
);
slopos_testing::stest!(
    name = test_same_session_background_read_sigttin,
    suite = tty
);
slopos_testing::stest!(
    name = test_same_session_background_write_sigttou,
    suite = tty
);
slopos_testing::stest!(name = test_check_write_no_session_allowed, suite = tty);
slopos_testing::stest!(name = test_cross_session_denied_error_variant, suite = tty);
// PTY Pair Atomicity & Lifecycle Hardening
slopos_testing::stest!(name = test_pty_alloc_pair_both_initialized, suite = tty);
slopos_testing::stest!(name = test_pty_close_master_first_frees_pair, suite = tty);
slopos_testing::stest!(name = test_pty_close_slave_first_frees_pair, suite = tty);
slopos_testing::stest!(name = test_pty_reallocation_after_free, suite = tty);
slopos_testing::stest!(name = test_pty_open_slave_validates_type, suite = tty);
slopos_testing::stest!(name = test_pty_open_slave_prevents_free, suite = tty);
slopos_testing::stest!(name = test_partial_open_no_free, suite = tty);
slopos_testing::stest!(name = test_rapid_alloc_free_realloc, suite = tty);
slopos_testing::stest!(name = test_pty_open_slave_after_free, suite = tty);
// Event-Driven Readiness & IXON Completion
slopos_testing::stest!(name = test_poll_events_pollin_with_data, suite = tty);
slopos_testing::stest!(name = test_poll_events_no_pollin_without_data, suite = tty);
slopos_testing::stest!(
    name = test_poll_events_pollout_when_not_stopped,
    suite = tty
);
slopos_testing::stest!(name = test_poll_events_no_pollout_when_stopped, suite = tty);
slopos_testing::stest!(name = test_poll_events_pollhup_on_hangup, suite = tty);
slopos_testing::stest!(
    name = test_poll_events_invalid_index_returns_zero,
    suite = tty
);
slopos_testing::stest!(name = test_ixon_stopped_state_via_push_input, suite = tty);
slopos_testing::stest!(name = test_ixon_any_char_resumes, suite = tty);
slopos_testing::stest!(name = test_poll_events_respects_requested_mask, suite = tty);
slopos_testing::stest!(name = test_pollhup_always_reported, suite = tty);
slopos_testing::stest!(name = test_poll_events_peer_closed_pollhup, suite = tty);
slopos_testing::stest!(name = test_default_console_tty_initial_value, suite = tty);
slopos_testing::stest!(name = test_set_default_console_tty, suite = tty);
slopos_testing::stest!(name = test_switch_active_tty_valid, suite = tty);
slopos_testing::stest!(name = test_switch_active_tty_invalid_index, suite = tty);
slopos_testing::stest!(name = test_switch_active_tty_unallocated, suite = tty);
slopos_testing::stest!(name = test_vconsole_state_initial, suite = tty);
slopos_testing::stest!(name = test_vconsole_write_byte_printable, suite = tty);
slopos_testing::stest!(name = test_vconsole_write_byte_newline, suite = tty);
slopos_testing::stest!(name = test_vconsole_write_byte_cr, suite = tty);
slopos_testing::stest!(name = test_vconsole_write_byte_backspace, suite = tty);
slopos_testing::stest!(name = test_vconsole_scroll_at_bottom, suite = tty);
slopos_testing::stest!(name = test_active_tty_independent_of_fg_pgrp, suite = tty);
slopos_testing::stest!(
    name = test_vconsole_has_framebuffer_default_false,
    suite = tty
);
// Canonical EOF, ISIG Flush & Signal Integrity
slopos_testing::stest!(name = test_canonical_eof_empty_no_phantom, suite = tty);
slopos_testing::stest!(
    name = test_canonical_eof_with_pending_text_no_phantom,
    suite = tty
);
slopos_testing::stest!(name = test_isig_flush_no_noflsh, suite = tty);
slopos_testing::stest!(name = test_isig_flush_with_noflsh, suite = tty);
slopos_testing::stest!(name = test_isig_ctrl_c_clears_edit_buffer, suite = tty);
slopos_testing::stest!(name = test_isig_flush_sigquit, suite = tty);
slopos_testing::stest!(name = test_isig_flush_sigtstp, suite = tty);
slopos_testing::stest!(name = test_double_eof_no_phantom_accumulation, suite = tty);
// Job Control & Controlling TTY Hardening
slopos_testing::stest!(
    name = test_set_fg_pgrp_checked_nonexistent_pgrp,
    suite = tty
);
slopos_testing::stest!(name = test_set_fg_pgrp_checked_clear_allowed, suite = tty);
slopos_testing::stest!(
    name = test_set_fg_pgrp_checked_no_session_skips_validation,
    suite = tty
);
slopos_testing::stest!(name = test_detach_ctty_non_leader, suite = tty);
slopos_testing::stest!(name = test_detach_ctty_session_leader, suite = tty);
slopos_testing::stest!(name = test_detach_ctty_cross_session_denied, suite = tty);
slopos_testing::stest!(name = test_tiocnotty_constant, suite = tty);
// Real TCSETSW/TCSETSF Drain Semantics
slopos_testing::stest!(name = test_is_output_idle_initially_true, suite = tty);
slopos_testing::stest!(name = test_inflight_counter_initial_zero, suite = tty);
slopos_testing::stest!(name = test_write_updates_inflight_counter, suite = tty);
slopos_testing::stest!(name = test_tcsetsw_preserves_input_after_drain, suite = tty);
slopos_testing::stest!(name = test_tcsetsf_flushes_input_after_drain, suite = tty);
slopos_testing::stest!(name = test_is_output_idle_invalid_index, suite = tty);
slopos_testing::stest!(name = test_is_output_idle_unallocated, suite = tty);
slopos_testing::stest!(name = test_drain_invalid_index_error, suite = tty);
slopos_testing::stest!(name = test_driver_output_pending_default_false, suite = tty);
slopos_testing::stest!(name = test_driver_kind_output_pending_dispatch, suite = tty);
slopos_testing::stest!(name = test_pty_output_idle_immediate, suite = tty);
slopos_testing::stest!(name = test_console_drain_immediate, suite = tty);
slopos_testing::stest!(name = test_tcsets_now_skips_drain, suite = tty);
// PTY Lifetime Safety & Scalable Capacity
slopos_testing::stest!(name = test_max_ttys_is_32, suite = tty);
slopos_testing::stest!(name = test_pty_peer_handle_creation, suite = tty);
slopos_testing::stest!(name = test_pty_peer_handle_snapshot, suite = tty);
slopos_testing::stest!(name = test_generation_bumped_on_free, suite = tty);
slopos_testing::stest!(name = test_stale_handle_detected, suite = tty);
slopos_testing::stest!(name = test_pty_alloc_captures_generation, suite = tty);
slopos_testing::stest!(name = test_stale_write_safe_noop, suite = tty);
slopos_testing::stest!(name = test_rapid_alloc_free_stress, suite = tty);
slopos_testing::stest!(name = test_data_flow_with_generation, suite = tty);
slopos_testing::stest!(name = test_validate_peer_out_of_range, suite = tty);
slopos_testing::stest!(name = test_multiple_pty_pairs, suite = tty);
// POSIX Completion Set (Rust-Idiomatic)
slopos_testing::stest!(name = test_ignbrk_discards_break, suite = tty);
slopos_testing::stest!(name = test_brkint_generates_sigint, suite = tty);
slopos_testing::stest!(name = test_parmrk_inserts_marker, suite = tty);
slopos_testing::stest!(
    name = test_nul_without_break_flags_passes_through,
    suite = tty
);
slopos_testing::stest!(name = test_echoke_visual_erase, suite = tty);
slopos_testing::stest!(name = test_echok_newline_on_kill, suite = tty);
slopos_testing::stest!(name = test_echoctl_erase_two_columns, suite = tty);
slopos_testing::stest!(name = test_bytes_available, suite = tty);
slopos_testing::stest!(name = test_raw_disc_bytes_available, suite = tty);
slopos_testing::stest!(name = test_ldisc_kind_bytes_available, suite = tty);
slopos_testing::stest!(name = test_fionread_constant, suite = tty);
slopos_testing::stest!(name = test_kill_empty_line_no_echo, suite = tty);
slopos_testing::stest!(name = test_ignbrk_takes_priority_over_brkint, suite = tty);
// Type-Safe Termios Foundation
slopos_testing::stest!(name = test_input_flags_from_bits, suite = tty);
slopos_testing::stest!(name = test_output_flags_from_bits, suite = tty);
slopos_testing::stest!(name = test_local_flags_from_bits, suite = tty);
slopos_testing::stest!(name = test_cc_index_values, suite = tty);
slopos_testing::stest!(name = test_posix_vdisable, suite = tty);
slopos_testing::stest!(name = test_tty_error_to_errno, suite = tty);
slopos_testing::stest!(name = test_tty_error_signal_interrupt, suite = tty);
slopos_testing::stest!(name = test_user_termios_typed_accessors, suite = tty);
slopos_testing::stest!(
    name = test_ldisc_typed_flags_behavioral_equivalence,
    suite = tty
);
slopos_testing::stest!(name = test_control_flags_empty, suite = tty);
// LdiscKind Dispatch Consolidation
slopos_testing::stest!(name = test_from_id_still_works, suite = tty);
slopos_testing::stest!(name = test_ldisc_ops_linedisc_trait_delegation, suite = tty);
slopos_testing::stest!(name = test_ldisc_ops_rawdisc_trait_delegation, suite = tty);
slopos_testing::stest!(name = test_dispatch_macro_ntty_routing, suite = tty);
slopos_testing::stest!(name = test_dispatch_macro_raw_routing, suite = tty);
slopos_testing::stest!(name = test_process_output_byte_dispatch, suite = tty);
slopos_testing::stest!(name = test_edit_content_dispatch, suite = tty);
// /dev/tty Controlling Terminal Device
slopos_testing::stest!(name = test_open_ref_second_fd_increments_count, suite = tty);
slopos_testing::stest!(
    name = test_dev_tty_operations_identical_to_direct,
    suite = tty
);
slopos_testing::stest!(name = test_open_ref_does_not_modify_session, suite = tty);
slopos_testing::stest!(
    name = test_open_ref_invalid_index_returns_error,
    suite = tty
);
slopos_testing::stest!(name = test_close_ref_decrements_after_open, suite = tty);
slopos_testing::stest!(name = test_multiple_open_ref_sequential, suite = tty);
slopos_testing::stest!(name = test_dev_tty_winsize_matches_direct, suite = tty);
// Background Write Protection (SIGTTOU on tcsetattr)
slopos_testing::stest!(name = test_tcsetattr_background_blocked, suite = tty);
slopos_testing::stest!(name = test_tcsetattr_foreground_allowed, suite = tty);
slopos_testing::stest!(name = test_tcsetattr_no_session_allowed, suite = tty);
slopos_testing::stest!(name = test_tcsetattr_cross_session_denied, suite = tty);
slopos_testing::stest!(name = test_orphaned_pgrp_errno, suite = tty);
slopos_testing::stest!(name = test_tcsetattr_kernel_task_bypass, suite = tty);
slopos_testing::stest!(name = test_tcsetsw_tcsetsf_kernel_task_bypass, suite = tty);
slopos_testing::stest!(name = test_tostop_background_write_check, suite = tty);
slopos_testing::stest!(name = test_kernel_task_check_write_allowed, suite = tty);
// Controlling Terminal Lifecycle Integrity
slopos_testing::stest!(name = test_acquire_ctty_fresh_tty, suite = tty);
slopos_testing::stest!(
    name = test_acquire_ctty_same_session_idempotent,
    suite = tty
);
slopos_testing::stest!(
    name = test_acquire_ctty_different_session_denied,
    suite = tty
);
slopos_testing::stest!(name = test_release_ctty_owning_session, suite = tty);
slopos_testing::stest!(name = test_release_ctty_wrong_session_noop, suite = tty);
slopos_testing::stest!(name = test_hangup_detaches_session, suite = tty);
slopos_testing::stest!(name = test_o_noctty_suppresses_acquire, suite = tty);
slopos_testing::stest!(
    name = test_detach_ctty_non_leader_preserves_session,
    suite = tty
);
slopos_testing::stest!(name = test_detach_ctty_session_leader_detaches, suite = tty);
slopos_testing::stest!(
    name = test_full_lifecycle_acquire_release_reacquire,
    suite = tty
);
slopos_testing::stest!(name = test_double_acquire_race_guard, suite = tty);
slopos_testing::stest!(name = test_hangup_no_session_safe, suite = tty);
slopos_testing::stest!(name = test_rapid_acquire_release_stress, suite = tty);
slopos_testing::stest!(name = test_acquire_invalid_index, suite = tty);
slopos_testing::stest!(name = test_release_invalid_index, suite = tty);
slopos_testing::stest!(name = test_detach_invalid_index, suite = tty);
// Post-Hangup I/O Hardening
slopos_testing::stest!(name = test_hangup_read_returns_eof, suite = tty);
slopos_testing::stest!(name = test_hangup_write_returns_eio, suite = tty);
slopos_testing::stest!(name = test_hangup_poll_returns_pollhup_pollin, suite = tty);
slopos_testing::stest!(name = test_hangup_set_termios_returns_eio, suite = tty);
slopos_testing::stest!(name = test_hangup_set_winsize_returns_eio, suite = tty);
slopos_testing::stest!(name = test_hangup_set_ldisc_returns_eio, suite = tty);
slopos_testing::stest!(name = test_hangup_get_fg_pgrp_still_works, suite = tty);
slopos_testing::stest!(name = test_pty_master_close_slave_eof_eio, suite = tty);
slopos_testing::stest!(name = test_hangup_permanent_eof, suite = tty);
slopos_testing::stest!(
    name = test_pty_slave_poll_pollhup_after_master_close,
    suite = tty
);
slopos_testing::stest!(name = test_hungup_errno_is_eio, suite = tty);
// Extended Line Boundaries (VEOL, VEOL2)
slopos_testing::stest!(name = test_veol_completes_line, suite = tty);
slopos_testing::stest!(name = test_veol2_completes_line, suite = tty);
slopos_testing::stest!(name = test_veol_disabled_no_effect, suite = tty);
slopos_testing::stest!(name = test_veol_and_newline_coexist, suite = tty);
slopos_testing::stest!(name = test_veol_echo_behavior, suite = tty);
slopos_testing::stest!(name = test_veol_no_echo, suite = tty);
slopos_testing::stest!(name = test_veol2_cc_index, suite = tty);
slopos_testing::stest!(name = test_veol_veol2_both_active, suite = tty);
slopos_testing::stest!(name = test_veol_and_eof_coexist, suite = tty);
// UTF-8 Aware Editing (IUTF8)
slopos_testing::stest!(name = test_utf8_char_width, suite = tty);
slopos_testing::stest!(name = test_iutf8_backspace_ascii, suite = tty);
slopos_testing::stest!(name = test_iutf8_backspace_2byte, suite = tty);
slopos_testing::stest!(name = test_iutf8_backspace_3byte_cjk, suite = tty);
slopos_testing::stest!(name = test_iutf8_backspace_4byte_emoji, suite = tty);
slopos_testing::stest!(name = test_no_iutf8_backspace_multibyte, suite = tty);
slopos_testing::stest!(name = test_iutf8_insert_column_tracking, suite = tty);
slopos_testing::stest!(name = test_iutf8_word_erase_mixed, suite = tty);
slopos_testing::stest!(name = test_iutf8_word_erase_preserves_prefix, suite = tty);
slopos_testing::stest!(name = test_iutf8_flag_value, suite = tty);
// Input Buffer Policy (IMAXBEL, IXOFF, CREAD)
slopos_testing::stest!(name = test_cread_enabled_input_processed, suite = tty);
slopos_testing::stest!(name = test_cread_disabled_input_discarded, suite = tty);
slopos_testing::stest!(name = test_cread_disabled_rawdisc, suite = tty);
slopos_testing::stest!(name = test_imaxbel_buffer_full_rings_bell, suite = tty);
slopos_testing::stest!(name = test_imaxbel_not_set_buffer_full_silent, suite = tty);
slopos_testing::stest!(name = test_imaxbel_buffer_not_full_normal, suite = tty);
slopos_testing::stest!(name = test_imaxbel_raw_mode_buffer_full, suite = tty);
slopos_testing::stest!(name = test_ixoff_high_water_sends_xoff, suite = tty);
slopos_testing::stest!(name = test_ixoff_low_water_sends_xon, suite = tty);
slopos_testing::stest!(name = test_ixoff_not_set_no_flow_control, suite = tty);
slopos_testing::stest!(name = test_cread_flag_value, suite = tty);
slopos_testing::stest!(name = test_imaxbel_flag_value, suite = tty);
// Deferred Reprint (PENDIN)
slopos_testing::stest!(name = test_pendin_flag_value, suite = tty);
slopos_testing::stest!(name = test_pendin_auto_set_on_echo_change, suite = tty);
slopos_testing::stest!(name = test_pendin_one_shot, suite = tty);
slopos_testing::stest!(name = test_vreprint_clears_pendin, suite = tty);
slopos_testing::stest!(name = test_pendin_not_set_for_non_echo_flags, suite = tty);
slopos_testing::stest!(name = test_pendin_empty_edit_buffer, suite = tty);
slopos_testing::stest!(name = test_flush_clears_pendin, suite = tty);
slopos_testing::stest!(name = test_flush_input_clears_pendin, suite = tty);
// PTY Namespace & Device Nodes
slopos_testing::stest!(name = test_pty_lock_ioctl_constants, suite = tty);
slopos_testing::stest!(name = test_slave_locked_by_default, suite = tty);
slopos_testing::stest!(name = test_locked_slave_open_rejected, suite = tty);
slopos_testing::stest!(name = test_unlock_enables_open, suite = tty);
slopos_testing::stest!(name = test_get_lock_round_trip, suite = tty);
slopos_testing::stest!(name = test_set_lock_non_master_rejected, suite = tty);
slopos_testing::stest!(name = test_data_flow_after_unlock, suite = tty);
slopos_testing::stest!(name = test_master_close_slave_hangup, suite = tty);
slopos_testing::stest!(name = test_multiple_pairs_with_locks, suite = tty);
slopos_testing::stest!(name = test_non_pty_not_locked, suite = tty);
slopos_testing::stest!(name = test_get_lock_non_master_error, suite = tty);
// PTY Packet Mode (TIOCPKT)
slopos_testing::stest!(name = test_abi_constants, suite = tty);
slopos_testing::stest!(name = test_tiocpkt_on_data_prefixed, suite = tty);
slopos_testing::stest!(name = test_tiocpkt_off_normal_read, suite = tty);
slopos_testing::stest!(name = test_tiocpkt_slave_flush_read, suite = tty);
slopos_testing::stest!(name = test_tiocpkt_ixon_toggle, suite = tty);
slopos_testing::stest!(name = test_tiocpkt_disable_clears_events, suite = tty);
slopos_testing::stest!(name = test_poll_packet_events_pollin, suite = tty);
slopos_testing::stest!(name = test_set_packet_mode_non_master, suite = tty);
// VT100/ANSI Terminal Emulation
slopos_testing::stest!(name = test_parser_print_ascii, suite = tty);
slopos_testing::stest!(name = test_parser_execute_control, suite = tty);
slopos_testing::stest!(name = test_clear_screen, suite = tty);
slopos_testing::stest!(name = test_cursor_position, suite = tty);
slopos_testing::stest!(name = test_sgr_red_foreground, suite = tty);
slopos_testing::stest!(name = test_sgr_reset, suite = tty);
slopos_testing::stest!(name = test_cursor_up, suite = tty);
slopos_testing::stest!(name = test_malformed_sequence_resilience, suite = tty);
slopos_testing::stest!(name = test_sgr_multi_param, suite = tty);
slopos_testing::stest!(name = test_vconsole_clear_screen, suite = tty);
slopos_testing::stest!(name = test_vconsole_cursor_pos, suite = tty);
slopos_testing::stest!(name = test_vconsole_sgr_color, suite = tty);
slopos_testing::stest!(name = test_vconsole_sgr_reset, suite = tty);
slopos_testing::stest!(name = test_vconsole_save_restore_cursor, suite = tty);
slopos_testing::stest!(name = test_parser_fuzz_no_panic, suite = tty);
slopos_testing::stest!(name = test_vconsole_erase_line, suite = tty);
slopos_testing::stest!(name = test_cursor_movement_clamping, suite = tty);
slopos_testing::stest!(name = test_vconsole_scroll_up, suite = tty);
// Advanced PTY & Session Control (EXTPROC, vhangup)
slopos_testing::stest!(name = test_extproc_flag_value, suite = tty);
slopos_testing::stest!(name = test_extproc_no_echo, suite = tty);
slopos_testing::stest!(name = test_extproc_no_canonical_editing, suite = tty);
slopos_testing::stest!(name = test_extproc_signals_still_delivered, suite = tty);
slopos_testing::stest!(name = test_extproc_cleared_resumes_normal, suite = tty);
slopos_testing::stest!(name = test_extproc_bypasses_iexten_editing, suite = tty);
slopos_testing::stest!(name = test_extproc_flow_control_works, suite = tty);
slopos_testing::stest!(name = test_extproc_imaxbel, suite = tty);
slopos_testing::stest!(name = test_vhangup_syscall_constant, suite = tty);
slopos_testing::stest!(name = test_vhangup_triggers_hangup, suite = tty);
slopos_testing::stest!(name = test_extproc_raw_mode_same_behavior, suite = tty);
// Legacy Termios Completion (ECHOPRT, IUCLC, OLCUC)
slopos_testing::stest!(name = test_echoprt_erase_format, suite = tty);
slopos_testing::stest!(name = test_echoprt_close_on_input, suite = tty);
slopos_testing::stest!(name = test_iuclc_maps_upper_to_lower, suite = tty);
slopos_testing::stest!(name = test_iuclc_no_effect_non_alpha, suite = tty);
slopos_testing::stest!(name = test_olcuc_maps_lower_to_upper, suite = tty);
slopos_testing::stest!(name = test_flags_disabled_by_default, suite = tty);
// Per-TTY Poll Notification
// PTY Flow Control (Throttle Mechanism)
slopos_testing::stest!(name = test_throttle_watermark_constants, suite = tty);
slopos_testing::stest!(name = test_pty_initially_unthrottled, suite = tty);
slopos_testing::stest!(name = test_throttle_activates_at_high_water, suite = tty);
slopos_testing::stest!(
    name = test_master_write_short_write_when_throttled,
    suite = tty
);
slopos_testing::stest!(name = test_read_unthrottles_slave, suite = tty);
slopos_testing::stest!(name = test_throttle_cycle_no_data_loss, suite = tty);
slopos_testing::stest!(name = test_console_not_throttled, suite = tty);
slopos_testing::stest!(
    name = test_master_write_full_when_not_throttled,
    suite = tty
);
// Cooked Buffer Overflow Hardening
// c_cflag ABI Completion
slopos_testing::stest!(name = test_control_flag_values, suite = tty);
slopos_testing::stest!(name = test_default_cflag, suite = tty);
slopos_testing::stest!(name = test_cflag_roundtrip, suite = tty);
slopos_testing::stest!(name = test_speed_fields_populated, suite = tty);
slopos_testing::stest!(name = test_speed_follows_baud_change, suite = tty);
slopos_testing::stest!(name = test_cread_value_preserved, suite = tty);
// Missing Ioctls (TCFLSH, TCSBRK, TCXONC)
slopos_testing::stest!(name = test_flush_flow_ioctl_constants, suite = tty);
slopos_testing::stest!(name = test_tcflush_input, suite = tty);
slopos_testing::stest!(name = test_tcflush_output, suite = tty);
slopos_testing::stest!(name = test_tcflush_both, suite = tty);
slopos_testing::stest!(name = test_tcflush_invalid_arg, suite = tty);
slopos_testing::stest!(name = test_tcsbrk_noop, suite = tty);
slopos_testing::stest!(name = test_tcsbrk_drain, suite = tty);
slopos_testing::stest!(name = test_tcxonc_all_actions, suite = tty);
// Edit Buffer Expansion (1024 → 4096)
slopos_testing::stest!(name = test_canonical_input_over_1024, suite = tty);
slopos_testing::stest!(name = test_large_paste_canonical, suite = tty);
slopos_testing::stest!(name = test_backspace_in_expanded_buffer, suite = tty);
// Signal Restart Infrastructure (ERESTARTSYS)
slopos_testing::stest!(name = test_restart_error_to_errno, suite = tty);
slopos_testing::stest!(
    name = test_restart_distinct_from_signal_interrupt,
    suite = tty
);
slopos_testing::stest!(name = test_erestartsys_constant_value, suite = tty);
slopos_testing::stest!(name = test_eintr_constant_value, suite = tty);
slopos_testing::stest!(name = test_sa_restart_flag_value, suite = tty);
slopos_testing::stest!(name = test_sa_restart_distinct, suite = tty);
slopos_testing::stest!(name = test_signal_interrupt_still_eintr, suite = tty);
slopos_testing::stest!(name = test_all_error_variants_preserved, suite = tty);
slopos_testing::stest!(name = test_nonblock_empty_returns_wouldblock, suite = tty);
slopos_testing::stest!(name = test_read_with_data_succeeds, suite = tty);
// Review Fix Regression Tests
slopos_testing::stest!(name = test_review_tcflush_unthrottles_pty, suite = tty);
slopos_testing::stest!(name = test_review_tcflush_both_unthrottles_pty, suite = tty);
slopos_testing::stest!(name = test_review_master_write_batch_boundary, suite = tty);
slopos_testing::stest!(
    name = test_review_speed_fields_merge_into_cflag,
    suite = tty
);
slopos_testing::stest!(name = test_review_speed_ispeed_fallback, suite = tty);
slopos_testing::stest!(name = test_review_speed_unrecognised_noop, suite = tty);
slopos_testing::stest!(name = test_review_pollerr_on_hangup, suite = tty);
slopos_testing::stest!(name = test_review_pollerr_on_peer_closed, suite = tty);
// Bug-fix regression tests (TTY review)
slopos_testing::stest!(
    name = test_bugfix_flush_edit_preserves_remainder,
    suite = tty
);
slopos_testing::stest!(name = test_bugfix_nonblock_write_throttled_pty, suite = tty);
slopos_testing::stest!(
    name = test_bugfix_nonblock_write_unthrottled_pty,
    suite = tty
);
slopos_testing::stest!(name = test_bugfix_rawdisc_input_full, suite = tty);
slopos_testing::stest!(name = test_bugfix_slave_write_stops_on_full, suite = tty);
slopos_testing::stest!(name = test_bugfix_linedisc_input_full, suite = tty);
// Bug-fix regression tests (TTY architectural review)
slopos_testing::stest!(name = test_bugfix_parmrk_atomic_full_insert, suite = tty);
slopos_testing::stest!(
    name = test_bugfix_parmrk_drop_when_insufficient_space,
    suite = tty
);
slopos_testing::stest!(
    name = test_bugfix_parmrk_imaxbel_bell_on_insufficient_space,
    suite = tty
);
slopos_testing::stest!(
    name = test_bugfix_parmrk_drop_when_buffer_completely_full,
    suite = tty
);
slopos_testing::stest!(
    name = test_bugfix_tcxonc_invalid_action_returns_error,
    suite = tty
);
slopos_testing::stest!(name = test_bugfix_tcxonc_boundary_values, suite = tty);
// TCXONC Behavioral Completion
slopos_testing::stest!(name = test_tcooff_blocks_nonblock_write, suite = tty);
slopos_testing::stest!(name = test_tcoon_resumes_write, suite = tty);
slopos_testing::stest!(name = test_tcooff_idempotent, suite = tty);
slopos_testing::stest!(name = test_tcoon_idempotent, suite = tty);
slopos_testing::stest!(name = test_stop_resume_cycle, suite = tty);
slopos_testing::stest!(name = test_tcioff_tcion_succeed, suite = tty);
slopos_testing::stest!(name = test_tcioff_tcion_no_output_stop, suite = tty);
slopos_testing::stest!(name = test_invalid_action_still_errors, suite = tty);
slopos_testing::stest!(name = test_tcooff_pty_slave_write, suite = tty);
slopos_testing::stest!(name = test_output_stopped_independent_of_ixon, suite = tty);
slopos_testing::stest!(name = test_tcxonc_unallocated_slot, suite = tty);
slopos_testing::stest!(name = test_tcxonc_invalid_index, suite = tty);
// Output Queue Visibility (TIOCOUTQ)
slopos_testing::stest!(name = test_tiocoutq_abi_constant, suite = tty);
slopos_testing::stest!(name = test_output_queued_zero_when_idle, suite = tty);
slopos_testing::stest!(name = test_output_queued_reflects_inflight, suite = tty);
slopos_testing::stest!(name = test_output_queued_zero_after_flush, suite = tty);
slopos_testing::stest!(name = test_output_queued_unallocated, suite = tty);
slopos_testing::stest!(name = test_output_queued_invalid_index, suite = tty);
slopos_testing::stest!(name = test_fionread_unchanged, suite = tty);
slopos_testing::stest!(name = test_output_queued_vconsole, suite = tty);
// Input Wake Batching (WAKEUP_CHARS)
slopos_testing::stest!(name = test_wakeup_chars_constant, suite = tty);
slopos_testing::stest!(name = test_canonical_wake_on_newline, suite = tty);
slopos_testing::stest!(name = test_noncanonical_no_wake_per_byte, suite = tty);
slopos_testing::stest!(name = test_noncanonical_wake_at_threshold, suite = tty);
slopos_testing::stest!(name = test_noncanonical_wake_near_full, suite = tty);
slopos_testing::stest!(name = test_flush_input_resets_wake_counter, suite = tty);
slopos_testing::stest!(name = test_flush_all_resets_wake_counter, suite = tty);
slopos_testing::stest!(name = test_rawdisc_wake_batching, suite = tty);
slopos_testing::stest!(name = test_wake_resets_counter, suite = tty);
slopos_testing::stest!(name = test_canonical_eof_wakes, suite = tty);
// TABDLY/XTABS Output Compatibility
slopos_testing::stest!(name = test_tabdly_abi_constants, suite = tty);
slopos_testing::stest!(name = test_default_oflag_includes_xtabs, suite = tty);
slopos_testing::stest!(name = test_xtabs_expands_tab_to_spaces, suite = tty);
slopos_testing::stest!(name = test_tab0_passes_literal_tab, suite = tty);
slopos_testing::stest!(name = test_tab0_column_tracking, suite = tty);
slopos_testing::stest!(name = test_xtabs_column_tracking_mixed, suite = tty);
slopos_testing::stest!(name = test_tabdly_termios_roundtrip, suite = tty);
slopos_testing::stest!(name = test_no_opost_tab_passthrough, suite = tty);
slopos_testing::stest!(name = test_existing_output_unaffected, suite = tty);
// no_room-style Overflow Recovery
slopos_testing::stest!(name = test_no_room_initially_false, suite = tty);
slopos_testing::stest!(name = test_no_room_set_on_cooked_full, suite = tty);
slopos_testing::stest!(name = test_no_room_not_set_before_full, suite = tty);
slopos_testing::stest!(name = test_overflow_count_increments, suite = tty);
slopos_testing::stest!(name = test_overflow_count_saturates, suite = tty);
slopos_testing::stest!(
    name = test_no_room_clears_on_drain_below_threshold,
    suite = tty
);
slopos_testing::stest!(name = test_no_room_stays_above_threshold, suite = tty);
slopos_testing::stest!(name = test_flush_input_clears_no_room, suite = tty);
slopos_testing::stest!(name = test_flush_all_clears_no_room, suite = tty);
slopos_testing::stest!(name = test_fill_drain_cycle_preserves_throttle, suite = tty);
slopos_testing::stest!(name = test_rawdisc_no_room, suite = tty);
slopos_testing::stest!(name = test_imaxbel_preserved_with_no_room, suite = tty);
slopos_testing::stest!(name = test_rawdisc_recovery, suite = tty);
slopos_testing::stest!(name = test_ldisc_kind_dispatch, suite = tty);
// Output Drain Semantics Hardening
slopos_testing::stest!(name = test_drain_idle_fast_path, suite = tty);
slopos_testing::stest!(name = test_drain_hangup_vacuously_complete, suite = tty);
slopos_testing::stest!(name = test_tcsbrk_hangup_returns_error, suite = tty);
slopos_testing::stest!(name = test_tcsbrk_zero_hangup_returns_error, suite = tty);
slopos_testing::stest!(name = test_tcsbrk_zero_healthy_succeeds, suite = tty);
slopos_testing::stest!(name = test_tcsbrk_and_tcsetsw_share_drain, suite = tty);
slopos_testing::stest!(name = test_drain_invalid_index, suite = tty);
slopos_testing::stest!(name = test_drain_unallocated_slot, suite = tty);
slopos_testing::stest!(name = test_pty_tcsbrk_drain_immediate, suite = tty);
slopos_testing::stest!(name = test_console_drain_synchronous, suite = tty);
slopos_testing::stest!(name = test_output_pending_bytes_all_drivers, suite = tty);
slopos_testing::stest!(name = test_output_queued_uses_pending_bytes, suite = tty);
slopos_testing::stest!(name = test_tcsetsw_hangup_returns_error, suite = tty);
slopos_testing::stest!(name = test_tcsetsf_hangup_returns_error, suite = tty);
slopos_testing::stest!(name = test_inflight_accounting_round_trip, suite = tty);
// Core Semantic Correctness (Gold Standard Audit)
slopos_testing::stest!(name = test_input_event_normal_behavior, suite = tty);
slopos_testing::stest!(name = test_input_event_break_brkint, suite = tty);
slopos_testing::stest!(name = test_input_event_break_ignbrk, suite = tty);
slopos_testing::stest!(name = test_input_event_parity_parmrk, suite = tty);
slopos_testing::stest!(name = test_input_event_parity_ignpar, suite = tty);
slopos_testing::stest!(name = test_input_event_overrun_noop, suite = tty);
slopos_testing::stest!(name = test_poll_output_stopped_masks_pollout, suite = tty);
slopos_testing::stest!(name = test_poll_output_not_stopped_has_pollout, suite = tty);
slopos_testing::stest!(name = test_grantpt_unlocks_slave, suite = tty);
slopos_testing::stest!(name = test_b0_hangup, suite = tty);
slopos_testing::stest!(name = test_speed_roundtrip, suite = tty);
slopos_testing::stest!(name = test_batched_ingress_no_data_loss, suite = tty);
slopos_testing::stest!(name = test_batched_ingress_signal_in_middle, suite = tty);
slopos_testing::stest!(name = test_background_read_sigttin_blocked_eio, suite = tty);
slopos_testing::stest!(name = test_receive_buf_accumulates_echo, suite = tty);
// VConsole Unicode & Broadened Xterm Emulation
slopos_testing::stest!(name = test_utf8_2byte_renders_codepoint, suite = tty);
slopos_testing::stest!(name = test_utf8_3byte_renders_codepoint, suite = tty);
slopos_testing::stest!(name = test_utf8_4byte_renders_codepoint, suite = tty);
slopos_testing::stest!(name = test_utf8_invalid_byte_emits_replacement, suite = tty);
slopos_testing::stest!(
    name = test_utf8_truncated_sequence_emits_replacement,
    suite = tty
);
slopos_testing::stest!(name = test_utf8_overlong_rejected, suite = tty);
slopos_testing::stest!(name = test_ascii_still_works, suite = tty);
slopos_testing::stest!(name = test_sgr_256_foreground, suite = tty);
slopos_testing::stest!(name = test_sgr_256_background, suite = tty);
slopos_testing::stest!(name = test_sgr_truecolor_foreground, suite = tty);
slopos_testing::stest!(name = test_sgr_truecolor_background, suite = tty);
slopos_testing::stest!(name = test_vconsole_256_color_sets_fg, suite = tty);
slopos_testing::stest!(name = test_vconsole_truecolor_sets_fg, suite = tty);
slopos_testing::stest!(name = test_bracketed_paste_enable_disable, suite = tty);
slopos_testing::stest!(name = test_decawm_default_on, suite = tty);
slopos_testing::stest!(name = test_decawm_toggle, suite = tty);
slopos_testing::stest!(name = test_decckm_toggle, suite = tty);
slopos_testing::stest!(name = test_decom_toggle, suite = tty);
slopos_testing::stest!(name = test_dectcem_still_works, suite = tty);
slopos_testing::stest!(name = test_alt_screen_still_works, suite = tty);
slopos_testing::stest!(name = test_cell_model_u32, suite = tty);
slopos_testing::stest!(name = test_vconsole_utf8_hello_renders, suite = tty);
slopos_testing::stest!(name = test_double_width_cjk, suite = tty);
slopos_testing::stest!(name = test_invalid_utf8_in_vconsole, suite = tty);
slopos_testing::stest!(name = test_mixed_ascii_utf8_escapes, suite = tty);
slopos_testing::stest!(name = test_256color_cube_mapping, suite = tty);
slopos_testing::stest!(name = test_256color_grayscale_mapping, suite = tty);
slopos_testing::stest!(name = test_is_double_width_ranges, suite = tty);
slopos_testing::stest!(name = test_sgr_standard_colors_unaffected, suite = tty);
slopos_testing::stest!(name = test_parser_fuzz_utf8_no_panic, suite = tty);
slopos_testing::stest!(name = test_vtparser_fuzz_no_panic, suite = tty);
slopos_testing::stest!(name = test_replacement_glyph_exists, suite = tty);
slopos_testing::stest!(name = test_get_glyph_for_codepoint_ascii, suite = tty);
// mod.rs Module Decomposition
slopos_testing::stest!(name = test_mod_reexports_io_functions, suite = tty);
slopos_testing::stest!(name = test_mod_reexports_termios_functions, suite = tty);
slopos_testing::stest!(name = test_mod_reexports_job_control_functions, suite = tty);
slopos_testing::stest!(name = test_mod_reexports_lifecycle_functions, suite = tty);
slopos_testing::stest!(name = test_mod_reexports_poll_functions, suite = tty);
slopos_testing::stest!(name = test_mod_reexports_pty_functions, suite = tty);
slopos_testing::stest!(name = test_tty_struct_fields_accessible, suite = tty);
slopos_testing::stest!(name = test_tty_error_variants_unchanged, suite = tty);
slopos_testing::stest!(name = test_max_ttys_constant, suite = tty);
slopos_testing::stest!(name = test_existing_api_smoke_test, suite = tty);
// POSIX Controlling Terminal Semantics
slopos_testing::stest!(name = test_ctty_can_be_ctty_serial, suite = tty);
slopos_testing::stest!(name = test_ctty_can_be_ctty_vconsole, suite = tty);
slopos_testing::stest!(name = test_ctty_can_be_ctty_pty_slave, suite = tty);
slopos_testing::stest!(name = test_ctty_cannot_be_ctty_pty_master, suite = tty);
slopos_testing::stest!(
    name = test_ctty_acquire_ctty_pty_master_rejected,
    suite = tty
);
slopos_testing::stest!(
    name = test_ctty_acquire_ctty_pty_slave_succeeds,
    suite = tty
);
slopos_testing::stest!(
    name = test_ctty_acquire_ctty_serial_console_succeeds,
    suite = tty
);
slopos_testing::stest!(name = test_ctty_acquire_ctty_vconsole_succeeds, suite = tty);
slopos_testing::stest!(name = test_ctty_o_noctty_constant_value, suite = tty);
slopos_testing::stest!(
    name = test_ctty_set_fg_pgrp_completes_without_deadlock,
    suite = tty
);
slopos_testing::stest!(
    name = test_ctty_set_fg_pgrp_checked_completes_without_deadlock,
    suite = tty
);
slopos_testing::stest!(
    name = test_ctty_pty_master_ctty_does_not_attach_session,
    suite = tty
);
slopos_testing::stest!(name = test_ctty_can_be_ctty_none_driver, suite = tty);
// TIOCOUTQ Byte Accounting & Packet Mode Edge Fix
slopos_testing::stest!(name = test_inflight_byte_granularity, suite = tty);
slopos_testing::stest!(name = test_tiocoutq_returns_bytes_not_ops, suite = tty);
slopos_testing::stest!(name = test_tiocoutq_zero_after_sync_write, suite = tty);
slopos_testing::stest!(name = test_tiocoutq_various_byte_counts, suite = tty);
slopos_testing::stest!(name = test_packet_mode_1byte_with_events, suite = tty);
slopos_testing::stest!(name = test_packet_mode_1byte_data_no_events, suite = tty);
slopos_testing::stest!(name = test_packet_mode_1byte_no_data_nonblock, suite = tty);
slopos_testing::stest!(name = test_packet_mode_2byte_works, suite = tty);
slopos_testing::stest!(
    name = test_tiocoutq_byte_accounting_regression_idle,
    suite = tty
);
slopos_testing::stest!(name = test_packet_mode_data_prefix_regression, suite = tty);
slopos_testing::stest!(name = test_echo_inflight_byte_granularity, suite = tty);
// Missing Ioctls (TIOCGSID, TIOCEXCL) & HUPCL Enforcement
slopos_testing::stest!(name = test_excl_hupcl_tiocgsid_abi_constant, suite = tty);
slopos_testing::stest!(name = test_excl_hupcl_tiocexcl_abi_constants, suite = tty);
slopos_testing::stest!(name = test_excl_hupcl_errno_ebusy_value, suite = tty);
slopos_testing::stest!(
    name = test_excl_hupcl_get_session_id_returns_correct_sid,
    suite = tty
);
slopos_testing::stest!(
    name = test_excl_hupcl_get_session_id_unallocated,
    suite = tty
);
slopos_testing::stest!(
    name = test_excl_hupcl_exclusive_initially_false,
    suite = tty
);
slopos_testing::stest!(name = test_excl_hupcl_set_exclusive_roundtrip, suite = tty);
slopos_testing::stest!(
    name = test_excl_hupcl_exclusive_blocks_second_open,
    suite = tty
);
slopos_testing::stest!(name = test_excl_hupcl_nxcl_allows_second_open, suite = tty);
slopos_testing::stest!(
    name = test_excl_hupcl_exclusive_unallocated_slot,
    suite = tty
);
slopos_testing::stest!(
    name = test_excl_hupcl_hupcl_last_close_triggers_hangup,
    suite = tty
);
slopos_testing::stest!(
    name = test_excl_hupcl_no_hupcl_last_close_no_hangup,
    suite = tty
);
slopos_testing::stest!(
    name = test_excl_hupcl_hupcl_pty_no_double_hangup,
    suite = tty
);
slopos_testing::stest!(name = test_excl_hupcl_close_clears_exclusive, suite = tty);
slopos_testing::stest!(name = test_ttyflags_default_empty, suite = tty);
slopos_testing::stest!(name = test_ttyflags_insert_remove_contains, suite = tty);
slopos_testing::stest!(name = test_mark_hung_up_clears_output_stopped, suite = tty);
slopos_testing::stest!(name = test_packet_events_default_empty, suite = tty);
slopos_testing::stest!(
    name = test_packet_events_from_bits_matches_tiocpkt,
    suite = tty
);
slopos_testing::stest!(name = test_packet_events_bits_roundtrip, suite = tty);
slopos_testing::stest!(name = test_tty_fields_pub_crate_smoke, suite = tty);
slopos_testing::stest!(name = test_session_fields_pub_crate_smoke, suite = tty);
slopos_testing::stest!(name = test_slave_starts_locked, suite = tty);
slopos_testing::stest!(name = test_ttyflags_set_method, suite = tty);
slopos_testing::stest!(name = test_ttyflags_multi_flag_operations, suite = tty);
slopos_testing::stest!(name = test_no_driver_kind_none, suite = tty);
// Phase 21: Deferred Actions RAII & Boilerplate Reduction
slopos_testing::stest!(name = test_p21_postlockwork_default_is_empty, suite = tty);
slopos_testing::stest!(
    name = test_p21_postlockwork_signal_makes_nonempty,
    suite = tty
);
slopos_testing::stest!(name = test_p21_postlockwork_execute_completes, suite = tty);
slopos_testing::stest!(name = test_p21_postlockwork_ixoff_byte, suite = tty);
slopos_testing::stest!(name = test_p21_postlockwork_packet_event, suite = tty);
slopos_testing::stest!(name = test_p21_postlockwork_packet_event_merge, suite = tty);
slopos_testing::stest!(name = test_p21_postlockwork_wake_helpers, suite = tty);
slopos_testing::stest!(
    name = test_p21_postlockwork_zero_pgid_signal_ignored,
    suite = tty
);
slopos_testing::stest!(
    name = test_p21_postlockwork_zero_event_bits_ignored,
    suite = tty
);
slopos_testing::stest!(
    name = test_p21_write_path_peer_cache_consolidation,
    suite = tty
);
slopos_testing::stest!(name = test_p21_forward_ldisc_ops_linedisc, suite = tty);
slopos_testing::stest!(name = test_p21_forward_ldisc_ops_rawdisc, suite = tty);
slopos_testing::stest!(name = test_p21_existing_api_smoke_read_write, suite = tty);
