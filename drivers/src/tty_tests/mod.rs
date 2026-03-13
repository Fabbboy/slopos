#![allow(dead_code, unused_imports)]

//! Regression tests for the TTY subsystem.
//!
//! Tests the `LineDisc`, `TtyDriverKind`, `TtyIndex`, TTY table, and
//! the per-TTY public API (compositor focus, foreground pgrp, active TTY).
//!
//! Phase 2 additions: input flag processing, output processing, signal
//! generation, flow control, VLNEXT, VWERASE, ECHOCTL.
//!
//! Phase 6 additions: compositor focus / fg_pgrp split, check_read() as sole
//! read gate, TtyIndex type safety, signal constant verification.

extern crate alloc;

use alloc::boxed::Box;

use slopos_abi::signal::{SIGCONT, SIGHUP, SIGINT, SIGQUIT, SIGTSTP, SIGTTIN, SIGTTOU, SIGWINCH};
use slopos_abi::syscall::{
    CcIndex, ControlFlags, InputFlags, LocalFlags, OutputFlags, POSIX_VDISABLE,
};
use slopos_lib::klog_info;
use slopos_lib::testing::TestResult;

use crate::tty;
use crate::tty::TtyError;
use crate::tty::TtyIndex;
use crate::tty::driver::{DriverId, TtyDriverKind, VConsoleDriver};
use crate::tty::ldisc::{InputAction, LdiscKind, LdiscOps, LineDisc, OutputAction, RawDisc};
use crate::tty::ringbuf::RingBuf;
use crate::tty::session::TtySession;
use crate::tty::session::{
    ForegroundCheck, NO_FOREGROUND_PGRP, NO_SESSION, ProcessGroupId, SessionId,
};
use crate::tty::table::{TTY_GENERATIONS, TTY_OUTPUT_INFLIGHT, TTY_SLOTS};
use crate::tty::vconsole::{
    CellAttributes, CursorAttributes, VCONSOLE_MAX_COLS, VCONSOLE_MAX_ROWS, VConsoleState,
};
use crate::tty::vtparser::{Direction, EraseMode, SgrAttr, VtAction, VtParser};

use crate::tty::pty::PtyPeerHandle;

pub(crate) fn boxed_vconsole_state() -> Box<VConsoleState> {
    let mut state = Box::<VConsoleState>::new_uninit();
    unsafe {
        let state_ref = state.as_mut_ptr();
        let default_cell = CellAttributes {
            fg: 0x00AAAAAA,
            bg: 0x00000000,
        };
        let default_cursor = CursorAttributes {
            fg: 0x00AAAAAA,
            bg: 0x00000000,
            bold: false,
            underline: false,
            inverse: false,
        };
        (*state_ref).cursor_row = 0;
        (*state_ref).cursor_col = 0;
        (*state_ref).rows = 25;
        (*state_ref).cols = 80;
        (*state_ref).fb = None;
        for r in 0..VCONSOLE_MAX_ROWS {
            (*state_ref).cells[r].fill(b' ' as u32);
            for c in 0..VCONSOLE_MAX_COLS {
                (*state_ref).cell_attrs[r][c] = default_cell;
            }
        }
        (*state_ref).parser = VtParser::new();
        (*state_ref).cursor_attrs = default_cursor;
        (*state_ref).saved_cursor_row = 0;
        (*state_ref).saved_cursor_col = 0;
        (*state_ref).saved_cursor_attrs = default_cursor;
        (*state_ref).cursor_visible = true;
        for r in 0..VCONSOLE_MAX_ROWS {
            (*state_ref).alt_screen_cells[r].fill(b' ' as u32);
            for c in 0..VCONSOLE_MAX_COLS {
                (*state_ref).alt_screen_attrs[r][c] = default_cell;
            }
        }
        (*state_ref).alt_screen_cursor_row = 0;
        (*state_ref).alt_screen_cursor_col = 0;
        (*state_ref).in_alt_screen = false;
        state.assume_init()
    }
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

pub mod test_driver;
pub mod test_integration;
pub mod test_ioctls;
pub mod test_ldisc;
pub mod test_poll;
pub mod test_pty;
pub mod test_rawdisc;
pub mod test_ringbuf;
pub mod test_session;
pub mod test_table;
pub mod test_vconsole;

pub use test_driver::*;
pub use test_integration::*;
pub use test_ioctls::*;
pub use test_ldisc::*;
pub use test_poll::*;
pub use test_pty::*;
pub use test_rawdisc::*;
pub use test_ringbuf::*;
pub use test_session::*;
pub use test_table::*;
pub use test_vconsole::*;

slopos_lib::define_test_suite!(
    tty,
    [
        test_ringbuf_new_is_empty,
        test_ringbuf_push_pop,
        test_ringbuf_full_returns_false,
        test_ringbuf_peek_does_not_consume,
        test_ringbuf_peek_at_offset,
        test_ringbuf_read_bulk,
        test_ringbuf_read_partial,
        test_ringbuf_flush_resets,
        test_ringbuf_wraparound,
        test_ringbuf_capacity_and_free,
        test_pty_data_roundtrip,
        test_pty_hangup_propagation,
        test_errno_background_maps_to_eio,
        test_ldisc_ringbuf_integration,
        test_echo_batching_correctness,
        test_ldisc_new_has_no_data,
        test_ldisc_read_empty,
        test_ldisc_canonical_newline,
        test_ldisc_canonical_backspace,
        test_ldisc_canonical_kill,
        test_ldisc_canonical_eof,
        test_ldisc_signal_ctrl_c,
        test_ldisc_raw_mode,
        test_ldisc_set_termios_flush,
        test_ldisc_flush_all,
        test_ldisc_echo_printable,
        test_ldisc_echo_newline,
        test_ldisc_multiple_reads,
        test_ldisc_backspace_empty,
        test_session_new_empty,
        test_session_attach,
        test_session_detach,
        test_session_check_read_foreground,
        test_session_check_read_background,
        test_session_check_read_no_session,
        test_session_check_read_kernel_task,
        test_session_check_write_no_tostop,
        test_session_check_write_tostop_background,
        // Phase 6: check_read replaces task_has_access
        test_session_check_read_replaces_task_has_access_foreground,
        test_session_check_read_replaces_task_has_access_background,
        test_session_check_read_replaces_task_has_access_permissive,
        test_session_set_fg_pgrp_checked_allowed,
        test_session_set_fg_pgrp_checked_denied,
        test_session_set_fg_pgrp_checked_no_session,
        test_tty_get_session_id_default,
        test_tty_attach_session,
        test_tty_detach_session,
        test_tty_detach_session_by_id,
        test_tty_set_fg_pgrp_checked,
        test_tty_index_eq,
        test_driver_none_no_panic,
        test_vconsole_drain_returns_zero,
        test_table_init_allocates_tty0_and_tty1,
        test_table_tty0_has_index_zero,
        test_table_tty0_active,
        test_table_with_tty_exists,
        test_table_with_tty_empty,
        test_active_tty_default,
        test_set_active_tty,
        test_foreground_pgrp,
        test_compositor_focus,
        test_keyboard_enter_scancode_reaches_active_tty,
        test_keyboard_scancode_routes_to_active_tty_index,
        test_keyboard_extended_up_arrow_reaches_tty,
        // Phase 2: Input flag processing
        test_ldisc_icrnl,
        test_ldisc_igncr,
        test_ldisc_inlcr,
        test_ldisc_istrip,
        // Phase 2: Output processing
        test_ldisc_opost_onlcr,
        test_ldisc_opost_ocrnl,
        test_ldisc_output_raw,
        // Phase 2: Signal generation
        test_ldisc_signal_ctrl_backslash,
        test_ldisc_signal_ctrl_z,
        // Phase 2: Flow control
        test_ldisc_flow_control_ixon,
        // Phase 2: ECHOCTL
        test_ldisc_echoctl,
        // Phase 2: VLNEXT
        test_ldisc_vlnext,
        // Phase 2: VWERASE
        test_ldisc_vwerase,
        // Phase 2: edit_content / reprint
        test_ldisc_edit_content,
        // Phase 2: Output processing via TTY write
        test_tty_write_returns_input_len,
        // Phase 3: Input pipeline cleanup
        test_keyboard_no_input_event_delivery,
        test_keyboard_break_code_no_input,
        test_keyboard_modifier_no_input,
        test_keyboard_press_release_single_char,
        test_vconsole_drain_via_drain_hw_input,
        test_keyboard_multi_key_sequence,
        // Phase 5: FD integration
        test_tty_write_output_processing,
        test_tty_write_raw_passthrough,
        test_tty_write_invalid_index,
        test_tty_per_tty_termios_isolation,
        test_tty_per_tty_winsize_isolation,
        test_tty_per_tty_fg_pgrp_isolation,
        test_tty_per_tty_has_data_isolation,
        test_tty_per_tty_session_isolation,
        test_tty_read_invalid_tty_returns_error,
        // Phase 6: Control-Plane Correctness
        test_tty_index_abi_type,
        test_signal_constants,
        test_set_compositor_focus_does_not_set_fg_pgrp,
        test_check_read_sole_gate_background,
        test_tty_open_count_lifecycle,
        test_tty_hangup_sets_flag_and_detaches_session,
        test_tty_hangup_nonblock_read_eio,
        test_tty_hangup_blocking_read_eof,
        test_phase9_tty_error_variants,
        test_phase9_read_returns_result,
        test_phase9_read_invalid_index_error,
        test_phase9_read_not_allocated_error,
        test_phase9_write_returns_result,
        test_phase9_get_termios_returns_result,
        test_phase9_vmin0_vtime0_immediate_return,
        test_phase9_vmin_enforcement,
        test_phase9_set_fg_pgrp_checked_permission_denied,
        test_phase9_hangup_read_returns_hung_up,
        // Phase 8: Per-TTY Locking & Performance
        test_phase8_per_tty_lock_independence,
        test_phase8_driver_id_round_trip,
        test_phase8_split_write_returns_input_len,
        test_phase8_idle_cb_iterates_all_ttys,
        test_phase8_merged_drain_read,
        test_phase8_with_tty_per_slot,
        test_phase8_driver_id_traits,
        // Phase 10: Job Control Correctness
        test_phase10_sigttou_constant,
        test_phase10_check_write_tostop_blocks_background,
        test_phase10_check_write_no_tostop_allows_background,
        test_phase10_check_write_tostop_allows_foreground,
        test_phase10_check_read_cross_session_rejected,
        test_phase10_check_read_same_session_foreground,
        test_phase10_check_read_kernel_task_allowed,
        test_phase10_tty_write_foreground_with_tostop,
        // Phase 11: Non-Canonical Timing Fix
        test_phase11_vmin_vtime_enough_data_returns_immediately,
        test_phase11_vmin_vtime_partial_nonblock,
        test_phase11_vmin_vtime_no_data_nonblock,
        test_phase11_vmin_vtime_interbyte_timeout_returns_partial,
        test_phase11_ldisc_vmin_vtime_helper,
        // Phase 12: Sane Defaults & Output Column Tracking
        test_phase12_default_termios_has_icrnl,
        test_phase12_default_termios_has_opost_onlcr,
        test_phase12_default_termios_has_full_lflag,
        test_phase12_output_column_tracking_printable,
        test_phase12_output_column_tracking_newline,
        test_phase12_output_column_tracking_cr,
        test_phase12_output_column_tracking_tab,
        test_phase12_output_column_tracking_backspace,
        test_phase12_onocr_at_column_zero,
        test_phase12_default_onlcr_newline_expands,
        // Phase 13: ABI Signal Constant Unification
        test_phase13_signal_values_from_signal_module,
        test_phase13_ldisc_signal_uses_signal_module,
        test_phase13_hangup_signals_from_signal_module,
        test_phase13_job_control_signals_from_signal_module,
        // Phase 14: Responsibility Split — PTY Foundation
        test_phase14_session_id_zero_is_none,
        test_phase14_session_id_round_trip,
        test_phase14_pgrp_id_zero_is_none,
        test_phase14_pgrp_id_round_trip,
        test_phase14_session_option_fields,
        test_phase14_session_option_attach_detach,
        test_phase14_raw_disc_new_empty,
        test_phase14_raw_disc_input_read,
        test_phase14_raw_disc_output_passthrough,
        test_phase14_raw_disc_flush,
        test_phase14_ldisc_kind_ntty_delegation,
        test_phase14_ldisc_kind_raw_delegation,
        test_phase14_pty_driver_id_variants,
        test_phase14_pty_master_driver_kind,
        test_phase14_pty_slave_driver_kind,
        // Phase 15: POSIX Quick Wins
        test_phase15_canonical_one_line_per_read,
        test_phase15_canonical_has_data_line_count,
        test_phase15_canonical_eof_line_boundary,
        test_phase15_sigwinch_constant,
        test_phase15_word_erase_path_boundary,
        test_phase15_word_erase_mixed_boundary,
        test_phase15_word_erase_trailing_spaces,
        test_phase15_canonical_small_buffer_read,
        test_phase16_tcsetsw_preserves_pending_input,
        test_phase16_tcsetsf_flushes_pending_input,
        test_phase16_read_with_attach_false_skips_auto_attach,
        test_phase18_read_with_attach_true_skips_durable_attach,
        test_phase18_acquire_and_release_controlling_terminal,
        test_phase18_release_wrong_session_is_noop,
        test_phase16_get_ldisc_default_is_ntty,
        test_phase16_set_ldisc_round_trip_preserves_termios,
        test_phase16_set_ldisc_invalid_id_rejected,
        test_phase17_pty_alloc_returns_master_and_slave,
        test_phase17_pty_master_to_slave_flow,
        test_phase17_pty_slave_to_master_flow,
        test_phase17_master_close_hangs_up_slave,
        test_phase17_slave_close_returns_master_eof,
        test_phase17_pty_canonical_editing_on_slave,
        // Phase 19: Strict Session Gates & Foreground Outcomes
        test_phase19_bootstrap_allowed_no_session_read,
        test_phase19_bootstrap_allowed_no_fg_pgrp,
        test_phase19_denied_cross_session_read,
        test_phase19_denied_cross_session_write_tostop,
        test_phase19_cross_session_write_no_tostop_still_denied,
        test_phase19_kernel_task_exempted_cross_session_read,
        test_phase19_kernel_task_exempted_cross_session_write,
        test_phase19_same_session_background_read_sigttin,
        test_phase19_same_session_background_write_sigttou,
        test_phase19_check_write_no_session_allowed,
        test_phase19_cross_session_denied_error_variant,
        // Phase 20: PTY Pair Atomicity & Lifecycle Hardening
        test_phase20_pty_alloc_pair_both_initialized,
        test_phase20_pty_close_master_first_frees_pair,
        test_phase20_pty_close_slave_first_frees_pair,
        test_phase20_pty_reallocation_after_free,
        test_phase20_pty_open_slave_validates_type,
        test_phase20_pty_open_slave_prevents_free,
        test_phase20_partial_open_no_free,
        test_phase20_rapid_alloc_free_realloc,
        test_phase20_pty_open_slave_after_free,
        // Phase 21: Event-Driven Readiness & IXON Completion
        test_phase21_poll_events_pollin_with_data,
        test_phase21_poll_events_no_pollin_without_data,
        test_phase21_poll_events_pollout_when_not_stopped,
        test_phase21_poll_events_no_pollout_when_stopped,
        test_phase21_poll_events_pollhup_on_hangup,
        test_phase21_poll_events_invalid_index_returns_zero,
        test_phase21_ixon_stopped_state_via_push_input,
        test_phase21_ixon_any_char_resumes,
        test_phase21_poll_events_respects_requested_mask,
        test_phase21_pollhup_always_reported,
        test_phase21_poll_events_peer_closed_pollhup,
        test_phase22_default_console_tty_initial_value,
        test_phase22_set_default_console_tty,
        test_phase22_switch_active_tty_valid,
        test_phase22_switch_active_tty_invalid_index,
        test_phase22_switch_active_tty_unallocated,
        test_phase22_vconsole_state_initial,
        test_phase22_vconsole_write_byte_printable,
        test_phase22_vconsole_write_byte_newline,
        test_phase22_vconsole_write_byte_cr,
        test_phase22_vconsole_write_byte_backspace,
        test_phase22_vconsole_scroll_at_bottom,
        test_phase22_active_tty_independent_of_fg_pgrp,
        test_phase22_vconsole_has_framebuffer_default_false,
        // Phase 23: Canonical EOF, ISIG Flush & Signal Integrity
        test_phase23_canonical_eof_empty_no_phantom,
        test_phase23_canonical_eof_with_pending_text_no_phantom,
        test_phase23_isig_flush_no_noflsh,
        test_phase23_isig_flush_with_noflsh,
        test_phase23_isig_ctrl_c_clears_edit_buffer,
        test_phase23_isig_flush_sigquit,
        test_phase23_isig_flush_sigtstp,
        test_phase23_double_eof_no_phantom_accumulation,
        // Phase 24: Job Control & Controlling TTY Hardening
        test_phase24_set_fg_pgrp_checked_nonexistent_pgrp,
        test_phase24_set_fg_pgrp_checked_clear_allowed,
        test_phase24_set_fg_pgrp_checked_no_session_skips_validation,
        test_phase24_detach_ctty_non_leader,
        test_phase24_detach_ctty_session_leader,
        test_phase24_detach_ctty_cross_session_denied,
        test_phase24_tiocnotty_constant,
        // Phase 25: Real TCSETSW/TCSETSF Drain Semantics
        test_phase25_is_output_idle_initially_true,
        test_phase25_inflight_counter_initial_zero,
        test_phase25_write_updates_inflight_counter,
        test_phase25_tcsetsw_preserves_input_after_drain,
        test_phase25_tcsetsf_flushes_input_after_drain,
        test_phase25_is_output_idle_invalid_index,
        test_phase25_is_output_idle_unallocated,
        test_phase25_drain_invalid_index_error,
        test_phase25_driver_output_pending_default_false,
        test_phase25_driver_kind_output_pending_dispatch,
        test_phase25_pty_drain_immediate,
        test_phase25_console_drain_immediate,
        test_phase25_tcsets_now_skips_drain,
        // Phase 26: PTY Lifetime Safety & Scalable Capacity
        test_phase26_max_ttys_is_32,
        test_phase26_pty_peer_handle_creation,
        test_phase26_pty_peer_handle_snapshot,
        test_phase26_generation_bumped_on_free,
        test_phase26_stale_handle_detected,
        test_phase26_pty_alloc_captures_generation,
        test_phase26_stale_write_safe_noop,
        test_phase26_rapid_alloc_free_stress,
        test_phase26_data_flow_with_generation,
        test_phase26_validate_peer_out_of_range,
        test_phase26_multiple_pty_pairs,
        // Phase 27: POSIX Completion Set (Rust-Idiomatic)
        test_phase27_ignbrk_discards_break,
        test_phase27_brkint_generates_sigint,
        test_phase27_parmrk_inserts_marker,
        test_phase27_nul_without_break_flags_passes_through,
        test_phase27_echoke_visual_erase,
        test_phase27_echok_newline_on_kill,
        test_phase27_echoctl_erase_two_columns,
        test_phase27_bytes_available,
        test_phase27_raw_disc_bytes_available,
        test_phase27_ldisc_kind_bytes_available,
        test_phase27_fionread_constant,
        test_phase27_kill_empty_line_no_echo,
        test_phase27_ignbrk_takes_priority_over_brkint,
        // Phase 28: Type-Safe Termios Foundation
        test_phase28_input_flags_from_bits,
        test_phase28_output_flags_from_bits,
        test_phase28_local_flags_from_bits,
        test_phase28_cc_index_values,
        test_phase28_posix_vdisable,
        test_phase28_tty_error_to_errno,
        test_phase28_tty_error_signal_interrupt,
        test_phase28_user_termios_typed_accessors,
        test_phase28_ldisc_typed_flags_behavioral_equivalence,
        test_phase28_control_flags_empty,
        // Phase 29: LdiscKind Dispatch Consolidation
        test_phase29_from_id_still_works,
        test_phase29_ldisc_ops_linedisc_trait_delegation,
        test_phase29_ldisc_ops_rawdisc_trait_delegation,
        test_phase29_dispatch_macro_ntty_routing,
        test_phase29_dispatch_macro_raw_routing,
        test_phase29_process_output_byte_dispatch,
        test_phase29_edit_content_dispatch,
        // Phase 30: /dev/tty Controlling Terminal Device
        test_phase30_open_ref_second_fd_increments_count,
        test_phase30_dev_tty_operations_identical_to_direct,
        test_phase30_open_ref_does_not_modify_session,
        test_phase30_open_ref_invalid_index_returns_error,
        test_phase30_close_ref_decrements_after_open,
        test_phase30_multiple_open_ref_sequential,
        test_phase30_dev_tty_winsize_matches_direct,
        // Phase 31: Background Write Protection (SIGTTOU on tcsetattr)
        test_phase31_tcsetattr_background_blocked,
        test_phase31_tcsetattr_foreground_allowed,
        test_phase31_tcsetattr_no_session_allowed,
        test_phase31_tcsetattr_cross_session_denied,
        test_phase31_orphaned_pgrp_errno,
        test_phase31_tcsetattr_kernel_task_bypass,
        test_phase31_tcsetsw_tcsetsf_kernel_task_bypass,
        test_phase31_tostop_background_write_check,
        test_phase31_kernel_task_check_write_allowed,
        // Phase 32: Controlling Terminal Lifecycle Integrity
        test_phase32_acquire_ctty_fresh_tty,
        test_phase32_acquire_ctty_same_session_idempotent,
        test_phase32_acquire_ctty_different_session_denied,
        test_phase32_release_ctty_owning_session,
        test_phase32_release_ctty_wrong_session_noop,
        test_phase32_hangup_detaches_session,
        test_phase32_o_noctty_suppresses_acquire,
        test_phase32_detach_ctty_non_leader_preserves_session,
        test_phase32_detach_ctty_session_leader_detaches,
        test_phase32_full_lifecycle_acquire_release_reacquire,
        test_phase32_double_acquire_race_guard,
        test_phase32_hangup_no_session_safe,
        test_phase32_rapid_acquire_release_stress,
        test_phase32_acquire_invalid_index,
        test_phase32_release_invalid_index,
        test_phase32_detach_invalid_index,
        // Phase 33: Post-Hangup I/O Hardening
        test_phase33_hangup_read_returns_eof,
        test_phase33_hangup_write_returns_eio,
        test_phase33_hangup_poll_returns_pollhup_pollin,
        test_phase33_hangup_set_termios_returns_eio,
        test_phase33_hangup_set_winsize_returns_eio,
        test_phase33_hangup_set_ldisc_returns_eio,
        test_phase33_hangup_get_fg_pgrp_still_works,
        test_phase33_pty_master_close_slave_eof_eio,
        test_phase33_hangup_permanent_eof,
        test_phase33_pty_slave_poll_pollhup_after_master_close,
        test_phase33_hungup_errno_is_eio,
        // Phase 34: Extended Line Boundaries (VEOL, VEOL2)
        test_phase34_veol_completes_line,
        test_phase34_veol2_completes_line,
        test_phase34_veol_disabled_no_effect,
        test_phase34_veol_and_newline_coexist,
        test_phase34_veol_echo_behavior,
        test_phase34_veol_no_echo,
        test_phase34_veol2_cc_index,
        test_phase34_veol_veol2_both_active,
        test_phase34_veol_and_eof_coexist,
        // Phase 35: UTF-8 Aware Editing (IUTF8)
        test_phase35_utf8_char_width,
        test_phase35_iutf8_backspace_ascii,
        test_phase35_iutf8_backspace_2byte,
        test_phase35_iutf8_backspace_3byte_cjk,
        test_phase35_iutf8_backspace_4byte_emoji,
        test_phase35_no_iutf8_backspace_multibyte,
        test_phase35_iutf8_insert_column_tracking,
        test_phase35_iutf8_word_erase_mixed,
        test_phase35_iutf8_word_erase_preserves_prefix,
        test_phase35_iutf8_flag_value,
        // Phase 36: Input Buffer Policy (IMAXBEL, IXOFF, CREAD)
        test_phase36_cread_enabled_input_processed,
        test_phase36_cread_disabled_input_discarded,
        test_phase36_cread_disabled_rawdisc,
        test_phase36_imaxbel_buffer_full_rings_bell,
        test_phase36_imaxbel_not_set_buffer_full_silent,
        test_phase36_imaxbel_buffer_not_full_normal,
        test_phase36_imaxbel_raw_mode_buffer_full,
        test_phase36_ixoff_high_water_sends_xoff,
        test_phase36_ixoff_low_water_sends_xon,
        test_phase36_ixoff_not_set_no_flow_control,
        test_phase36_cread_flag_value,
        test_phase36_imaxbel_flag_value,
        // Phase 37: Deferred Reprint (PENDIN)
        test_phase37_pendin_flag_value,
        test_phase37_pendin_auto_set_on_echo_change,
        test_phase37_pendin_one_shot,
        test_phase37_vreprint_clears_pendin,
        test_phase37_pendin_not_set_for_non_echo_flags,
        test_phase37_pendin_empty_edit_buffer,
        test_phase37_flush_clears_pendin,
        test_phase37_flush_input_clears_pendin,
        // Phase 38: PTY Namespace & Device Nodes
        test_phase38_ioctl_constants,
        test_phase38_slave_locked_by_default,
        test_phase38_locked_slave_open_rejected,
        test_phase38_unlock_enables_open,
        test_phase38_get_lock_round_trip,
        test_phase38_set_lock_non_master_rejected,
        test_phase38_data_flow_after_unlock,
        test_phase38_master_close_slave_hangup,
        test_phase38_multiple_pairs_with_locks,
        test_phase38_non_pty_not_locked,
        test_phase38_get_lock_non_master_error,
        // Phase 39: PTY Packet Mode (TIOCPKT)
        test_phase39_abi_constants,
        test_phase39_tiocpkt_on_data_prefixed,
        test_phase39_tiocpkt_off_normal_read,
        test_phase39_tiocpkt_slave_flush_read,
        test_phase39_tiocpkt_ixon_toggle,
        test_phase39_tiocpkt_disable_clears_events,
        test_phase39_poll_packet_events_pollin,
        test_phase39_set_packet_mode_non_master,
        // Phase 40: VT100/ANSI Terminal Emulation
        test_phase40_parser_print_ascii,
        test_phase40_parser_execute_control,
        test_phase40_clear_screen,
        test_phase40_cursor_position,
        test_phase40_sgr_red_foreground,
        test_phase40_sgr_reset,
        test_phase40_cursor_up,
        test_phase40_malformed_sequence_resilience,
        test_phase40_sgr_multi_param,
        test_phase40_vconsole_clear_screen,
        test_phase40_vconsole_cursor_pos,
        test_phase40_vconsole_sgr_color,
        test_phase40_vconsole_sgr_reset,
        test_phase40_vconsole_save_restore_cursor,
        test_phase40_parser_fuzz_no_panic,
        test_phase40_vconsole_erase_line,
        test_phase40_cursor_movement_clamping,
        test_phase40_vconsole_scroll_up,
        // Phase 41: Advanced PTY & Session Control (EXTPROC, vhangup)
        test_phase41_extproc_flag_value,
        test_phase41_extproc_no_echo,
        test_phase41_extproc_no_canonical_editing,
        test_phase41_extproc_signals_still_delivered,
        test_phase41_extproc_cleared_resumes_normal,
        test_phase41_extproc_bypasses_iexten_editing,
        test_phase41_extproc_flow_control_works,
        test_phase41_extproc_imaxbel,
        test_phase41_vhangup_syscall_constant,
        test_phase41_vhangup_triggers_hangup,
        test_phase41_extproc_raw_mode_same_behavior,
        // Phase 42: Legacy Termios Completion (ECHOPRT, IUCLC, OLCUC)
        test_phase42_echoprt_erase_format,
        test_phase42_echoprt_close_on_input,
        test_phase42_iuclc_maps_upper_to_lower,
        test_phase42_iuclc_no_effect_non_alpha,
        test_phase42_olcuc_maps_lower_to_upper,
        test_phase42_flags_disabled_by_default,
        // Finishing Phase 1: Per-TTY Poll Notification
        test_fp1_poll_waiters_exist,
        test_fp1_push_input_wakes_correct_poll_waiter,
        test_fp1_push_input_does_not_wake_other_slot,
        test_fp1_hangup_wakes_correct_poll_waiter,
        test_fp1_pty_packet_event_wakes_master_poll_waiter,
        test_fp1_poll_sleep_on_empty_slots_does_not_panic,
        // Finishing Phase 2: PTY Flow Control (Throttle Mechanism)
        test_fp2_throttle_watermark_constants,
        test_fp2_pty_initially_unthrottled,
        test_fp2_throttle_activates_at_high_water,
        test_fp2_master_write_short_write_when_throttled,
        test_fp2_read_unthrottles_slave,
        test_fp2_throttle_cycle_no_data_loss,
        test_fp2_console_not_throttled,
        test_fp2_master_write_full_when_not_throttled,
        // Finishing Phase 3: Cooked Buffer Overflow Hardening
        test_fp3_push_cooked_returns_false_when_full,
        test_fp3_push_cooked_returns_true_when_space,
        test_fp3_canonical_flush_fits_in_cooked,
        test_fp3_imaxbel_bell_on_cooked_overflow,
        test_fp3_no_imaxbel_silent_drop,
        // Finishing Phase 4: c_cflag ABI Completion
        test_fp4_control_flag_values,
        test_fp4_default_cflag,
        test_fp4_cflag_roundtrip,
        test_fp4_speed_fields_populated,
        test_fp4_speed_follows_baud_change,
        test_fp4_cread_value_preserved,
        // Finishing Phase 5: Missing Ioctls (TCFLSH, TCSBRK, TCXONC)
        test_fp5_ioctl_constants,
        test_fp5_tcflush_input,
        test_fp5_tcflush_output,
        test_fp5_tcflush_both,
        test_fp5_tcflush_invalid_arg,
        test_fp5_tcsbrk_noop,
        test_fp5_tcsbrk_drain,
        test_fp5_tcxonc_all_actions,
        // Finishing Phase 6: Edit Buffer Expansion (1024 → 4096)
        test_fp6_canonical_input_over_1024,
        test_fp6_large_paste_canonical,
        test_fp6_backspace_in_expanded_buffer,
        // Finishing Phase 7: Signal Restart Infrastructure (ERESTARTSYS)
        test_fp7_restart_error_to_errno,
        test_fp7_restart_distinct_from_signal_interrupt,
        test_fp7_erestartsys_constant_value,
        test_fp7_eintr_constant_value,
        test_fp7_sa_restart_flag_value,
        test_fp7_sa_restart_distinct,
        test_fp7_signal_interrupt_still_eintr,
        test_fp7_all_error_variants_preserved,
        test_fp7_nonblock_empty_returns_wouldblock,
        test_fp7_read_with_data_succeeds,
        // Review Fix Regression Tests
        test_review_tcflush_unthrottles_pty,
        test_review_tcflush_both_unthrottles_pty,
        test_review_master_write_batch_boundary,
        test_review_speed_fields_merge_into_cflag,
        test_review_speed_ispeed_fallback,
        test_review_speed_unrecognised_noop,
        test_review_pollerr_on_hangup,
        test_review_pollerr_on_peer_closed,
        // Bug-fix regression tests (TTY review)
        test_bugfix_flush_edit_preserves_remainder,
        test_bugfix_nonblock_write_throttled_pty,
        test_bugfix_nonblock_write_unthrottled_pty,
        test_bugfix_rawdisc_input_full,
        test_bugfix_slave_write_stops_on_full,
        test_bugfix_linedisc_input_full,
        // Bug-fix regression tests (TTY architectural review)
        test_bugfix_parmrk_atomic_full_insert,
        test_bugfix_parmrk_drop_when_insufficient_space,
        test_bugfix_parmrk_imaxbel_bell_on_insufficient_space,
        test_bugfix_parmrk_drop_when_buffer_completely_full,
        test_bugfix_tcxonc_invalid_action_returns_error,
        test_bugfix_tcxonc_boundary_values,
        // Finishing Phase 8: TCXONC Behavioral Completion
        test_fp8_tcooff_blocks_nonblock_write,
        test_fp8_tcoon_resumes_write,
        test_fp8_tcooff_idempotent,
        test_fp8_tcoon_idempotent,
        test_fp8_stop_resume_cycle,
        test_fp8_tcioff_tcion_succeed,
        test_fp8_tcioff_tcion_no_output_stop,
        test_fp8_invalid_action_still_errors,
        test_fp8_tcooff_pty_slave_write,
        test_fp8_output_stopped_independent_of_ixon,
        test_fp8_tcxonc_unallocated_slot,
        test_fp8_tcxonc_invalid_index,
        // Finishing Phase 9: Output Queue Visibility (TIOCOUTQ)
        test_fp9_tiocoutq_abi_constant,
        test_fp9_output_queued_zero_when_idle,
        test_fp9_output_queued_reflects_inflight,
        test_fp9_output_queued_zero_after_flush,
        test_fp9_output_queued_unallocated,
        test_fp9_output_queued_invalid_index,
        test_fp9_fionread_unchanged,
        test_fp9_output_queued_vconsole,
        // Finishing Phase 10: Input Wake Batching (WAKEUP_CHARS)
        test_fp10_wakeup_chars_constant,
        test_fp10_canonical_wake_on_newline,
        test_fp10_noncanonical_no_wake_per_byte,
        test_fp10_noncanonical_wake_at_threshold,
        test_fp10_noncanonical_wake_near_full,
        test_fp10_flush_input_resets_wake_counter,
        test_fp10_flush_all_resets_wake_counter,
        test_fp10_rawdisc_wake_batching,
        test_fp10_wake_resets_counter,
        test_fp10_canonical_eof_wakes,
        // Finishing Phase 11: TABDLY/XTABS Output Compatibility
        test_fp11_tabdly_abi_constants,
        test_fp11_default_oflag_includes_xtabs,
        test_fp11_xtabs_expands_tab_to_spaces,
        test_fp11_tab0_passes_literal_tab,
        test_fp11_tab0_column_tracking,
        test_fp11_xtabs_column_tracking_mixed,
        test_fp11_tabdly_termios_roundtrip,
        test_fp11_no_opost_tab_passthrough,
        test_fp11_existing_output_unaffected,
        // Finishing Phase 12: no_room-style Overflow Recovery
        test_fp12_no_room_initially_false,
        test_fp12_no_room_set_on_cooked_full,
        test_fp12_no_room_not_set_before_full,
        test_fp12_overflow_count_increments,
        test_fp12_overflow_count_saturates,
        test_fp12_no_room_clears_on_drain_below_threshold,
        test_fp12_no_room_stays_above_threshold,
        test_fp12_flush_input_clears_no_room,
        test_fp12_flush_all_clears_no_room,
        test_fp12_fill_drain_cycle_preserves_throttle,
        test_fp12_rawdisc_no_room,
        test_fp12_imaxbel_preserved_with_no_room,
        test_fp12_rawdisc_recovery,
        test_fp12_ldisc_kind_dispatch,
        // Finishing Phase 13: Output Drain Semantics Hardening
        test_fp13_drain_idle_fast_path,
        test_fp13_drain_hangup_vacuously_complete,
        test_fp13_tcsbrk_hangup_returns_error,
        test_fp13_tcsbrk_zero_hangup_returns_error,
        test_fp13_tcsbrk_zero_healthy_succeeds,
        test_fp13_tcsbrk_and_tcsetsw_share_drain,
        test_fp13_drain_invalid_index,
        test_fp13_drain_unallocated_slot,
        test_fp13_pty_drain_immediate,
        test_fp13_console_drain_synchronous,
        test_fp13_output_pending_bytes_all_drivers,
        test_fp13_output_queued_uses_pending_bytes,
        test_fp13_tcsetsw_hangup_returns_error,
        test_fp13_tcsetsf_hangup_returns_error,
        test_fp13_inflight_accounting_round_trip,
        // Finishing Phase 14: Core Semantic Correctness (Gold Standard Audit)
        test_fp14_input_event_normal_behavior,
        test_fp14_input_event_break_brkint,
        test_fp14_input_event_break_ignbrk,
        test_fp14_input_event_parity_parmrk,
        test_fp14_input_event_parity_ignpar,
        test_fp14_input_event_overrun_noop,
        test_fp14_poll_output_stopped_masks_pollout,
        test_fp14_poll_output_not_stopped_has_pollout,
        test_fp14_grantpt_unlocks_slave,
        test_fp14_b0_hangup,
        test_fp14_speed_roundtrip,
        test_fp14_batched_ingress_no_data_loss,
        test_fp14_batched_ingress_signal_in_middle,
        test_fp14_background_read_sigttin_blocked_eio,
        test_fp14_receive_buf_accumulates_echo,
        // Finishing Phase 15: VConsole Unicode & Broadened Xterm Emulation
        test_fp15_utf8_2byte_renders_codepoint,
        test_fp15_utf8_3byte_renders_codepoint,
        test_fp15_utf8_4byte_renders_codepoint,
        test_fp15_utf8_invalid_byte_emits_replacement,
        test_fp15_utf8_truncated_sequence_emits_replacement,
        test_fp15_utf8_overlong_rejected,
        test_fp15_ascii_still_works,
        test_fp15_sgr_256_foreground,
        test_fp15_sgr_256_background,
        test_fp15_sgr_truecolor_foreground,
        test_fp15_sgr_truecolor_background,
        test_fp15_vconsole_256_color_sets_fg,
        test_fp15_vconsole_truecolor_sets_fg,
        test_fp15_bracketed_paste_enable_disable,
        test_fp15_decawm_default_on,
        test_fp15_decawm_toggle,
        test_fp15_decckm_toggle,
        test_fp15_decom_toggle,
        test_fp15_dectcem_still_works,
        test_fp15_alt_screen_still_works,
        test_fp15_cell_model_u32,
        test_fp15_vconsole_utf8_hello_renders,
        test_fp15_double_width_cjk,
        test_fp15_invalid_utf8_in_vconsole,
        test_fp15_mixed_ascii_utf8_escapes,
        test_fp15_256color_cube_mapping,
        test_fp15_256color_grayscale_mapping,
        test_fp15_is_double_width_ranges,
        test_fp15_sgr_standard_colors_unaffected,
        test_fp15_parser_fuzz_utf8_no_panic,
        test_fp15_replacement_glyph_exists,
        test_fp15_get_glyph_for_codepoint_ascii,
        // Finishing Phase 16: mod.rs Module Decomposition
        test_fp16_mod_reexports_io_functions,
        test_fp16_mod_reexports_termios_functions,
        test_fp16_mod_reexports_job_control_functions,
        test_fp16_mod_reexports_lifecycle_functions,
        test_fp16_mod_reexports_poll_functions,
        test_fp16_mod_reexports_pty_functions,
        test_fp16_tty_struct_fields_accessible,
        test_fp16_tty_error_variants_unchanged,
        test_fp16_max_ttys_constant,
        test_fp16_existing_api_smoke_test,
    ]
);
