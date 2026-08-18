//! The diagnostic console's keyboard trigger: a two-step chord — SysRq
//! (Alt+PrintScreen on an AT keyboard) to arm, then one command key. Both
//! keypresses are consumed, so neither reaches the TTY or the focused GUI
//! application.
//!
//! The caller must feed this *ahead of* `resolve()`: a command key is a position
//! rather than a character, so [`usage_to_command`] consults no layout, and a key
//! routed through `resolve()` would compose with a pending dead key instead.

use slopos_abi::input::keycode::*;

use crate::keymap::Mods;

/// What the caller should do with the key it just decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Not the console's. Keep processing normally.
    Pass,
    /// The console consumed it. Deliver nothing.
    Eat,
    /// The console consumed it and selected this command key.
    Run(u8),
}

/// Usages 0..=255, one bit each.
const EATEN_WORDS: usize = 4;

#[derive(Debug, Clone, Copy)]
pub struct SysrqFsm {
    /// When the trigger was armed, or `None` when idle.
    armed_at_ms: Option<u64>,
    /// Usages consumed on press, so their release is consumed too: a release with
    /// no matching press reads as a key stuck down.
    eaten: [u64; EATEN_WORDS],
}

impl Default for SysrqFsm {
    fn default() -> Self {
        Self::new()
    }
}

impl SysrqFsm {
    pub const fn new() -> Self {
        Self {
            armed_at_ms: None,
            eaten: [0; EATEN_WORDS],
        }
    }

    pub fn is_armed(&self) -> bool {
        self.armed_at_ms.is_some()
    }

    fn mark_eaten(&mut self, usage: u16) {
        if let Some(word) = self.eaten.get_mut((usage >> 6) as usize) {
            *word |= 1u64 << (usage & 63);
        }
    }

    fn take_eaten(&mut self, usage: u16) -> bool {
        let Some(word) = self.eaten.get_mut((usage >> 6) as usize) else {
            return false;
        };
        let bit = 1u64 << (usage & 63);
        let was = *word & bit != 0;
        *word &= !bit;
        was
    }

    /// Feed one decoded key transition. `arm_ms` bounds how long an armed trigger
    /// waits, so an accidental arm does not turn the next keystroke into a command.
    pub fn feed(
        &mut self,
        usage: u16,
        pressed: bool,
        mods: Mods,
        now_ms: u64,
        arm_ms: u16,
    ) -> Verdict {
        if !pressed {
            return if self.take_eaten(usage) {
                Verdict::Eat
            } else {
                Verdict::Pass
            };
        }

        if is_arm_key(usage, mods) {
            self.armed_at_ms = Some(now_ms);
            self.mark_eaten(usage);
            return Verdict::Eat;
        }

        let Some(armed_at) = self.armed_at_ms else {
            return Verdict::Pass;
        };

        // A modifier neither selects nor disarms: shift is pressed first when
        // reaching for a shifted command key.
        if is_modifier(usage) {
            return Verdict::Pass;
        }

        if now_ms.saturating_sub(armed_at) > u64::from(arm_ms) {
            self.armed_at_ms = None;
            return Verdict::Pass;
        }

        self.armed_at_ms = None;
        match usage_to_command(usage) {
            Some(key) => {
                self.mark_eaten(usage);
                Verdict::Run(key)
            }
            // An unmapped key is likelier to be the operator moving on than a
            // mistyped command, so it disarms and is delivered.
            None => Verdict::Pass,
        }
    }
}

/// Whether this press arms the trigger. `KEY_SYSRQ` is what an AT keyboard
/// reports for Alt+PrintScreen; some keyboards and emulators report plain
/// PrintScreen with Alt in the modifier state, so both spellings arm.
fn is_arm_key(usage: u16, mods: Mods) -> bool {
    usage == KEY_SYSRQ || (usage == KEY_PRINTSCREEN && mods.alt)
}

/// Canonical usage → the ASCII key a command is registered under. Letters and
/// digits only: punctuation moves between the layouts SlopOS ships.
pub const fn usage_to_command(usage: u16) -> Option<u8> {
    // Contiguous HID ranges, so this is arithmetic rather than a table.
    if usage >= KEY_A && usage <= KEY_Z {
        return Some(b'a' + (usage - KEY_A) as u8);
    }
    if usage >= KEY_1 && usage <= KEY_9 {
        return Some(b'1' + (usage - KEY_1) as u8);
    }
    if usage == KEY_0 {
        return Some(b'0');
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARM_MS: u16 = 3000;

    fn mods() -> Mods {
        Mods::default()
    }

    fn alt() -> Mods {
        Mods {
            alt: true,
            ..Mods::default()
        }
    }

    #[test]
    fn arm_then_select_runs_the_command() {
        let mut fsm = SysrqFsm::new();
        assert_eq!(fsm.feed(KEY_SYSRQ, true, mods(), 0, ARM_MS), Verdict::Eat);
        assert!(fsm.is_armed());
        assert_eq!(
            fsm.feed(KEY_T, true, mods(), 10, ARM_MS),
            Verdict::Run(b't')
        );
        assert!(!fsm.is_armed());
    }

    #[test]
    fn alt_printscreen_also_arms() {
        let mut fsm = SysrqFsm::new();
        assert_eq!(
            fsm.feed(KEY_PRINTSCREEN, true, alt(), 0, ARM_MS),
            Verdict::Eat
        );
        assert!(fsm.is_armed());
    }

    #[test]
    fn printscreen_without_alt_does_not_arm() {
        let mut fsm = SysrqFsm::new();
        assert_eq!(
            fsm.feed(KEY_PRINTSCREEN, true, mods(), 0, ARM_MS),
            Verdict::Pass
        );
        assert!(!fsm.is_armed());
    }

    #[test]
    fn idle_keys_pass_straight_through() {
        let mut fsm = SysrqFsm::new();
        assert_eq!(fsm.feed(KEY_T, true, mods(), 0, ARM_MS), Verdict::Pass);
        assert_eq!(fsm.feed(KEY_T, false, mods(), 1, ARM_MS), Verdict::Pass);
    }

    #[test]
    fn the_arm_expires() {
        let mut fsm = SysrqFsm::new();
        fsm.feed(KEY_SYSRQ, true, mods(), 0, ARM_MS);
        assert_eq!(
            fsm.feed(KEY_T, true, mods(), u64::from(ARM_MS) + 1, ARM_MS),
            Verdict::Pass
        );
        assert!(!fsm.is_armed());
    }

    #[test]
    fn a_modifier_does_not_disarm() {
        let mut fsm = SysrqFsm::new();
        fsm.feed(KEY_SYSRQ, true, mods(), 0, ARM_MS);
        assert_eq!(
            fsm.feed(KEY_LEFTSHIFT, true, mods(), 5, ARM_MS),
            Verdict::Pass
        );
        assert!(fsm.is_armed());
        assert_eq!(
            fsm.feed(KEY_M, true, mods(), 10, ARM_MS),
            Verdict::Run(b'm')
        );
    }

    #[test]
    fn an_unmapped_key_disarms_and_is_delivered() {
        let mut fsm = SysrqFsm::new();
        fsm.feed(KEY_SYSRQ, true, mods(), 0, ARM_MS);
        assert_eq!(fsm.feed(KEY_UP, true, mods(), 5, ARM_MS), Verdict::Pass);
        assert!(!fsm.is_armed());
    }

    #[test]
    fn every_eaten_press_eats_its_release() {
        let mut fsm = SysrqFsm::new();
        fsm.feed(KEY_SYSRQ, true, mods(), 0, ARM_MS);
        assert_eq!(fsm.feed(KEY_SYSRQ, false, mods(), 1, ARM_MS), Verdict::Eat);
        fsm.feed(KEY_SYSRQ, true, mods(), 2, ARM_MS);
        fsm.feed(KEY_T, true, mods(), 3, ARM_MS);
        assert_eq!(fsm.feed(KEY_T, false, mods(), 4, ARM_MS), Verdict::Eat);
        assert_eq!(fsm.feed(KEY_G, false, mods(), 5, ARM_MS), Verdict::Pass);
    }

    #[test]
    fn re_arming_replaces_the_deadline() {
        let mut fsm = SysrqFsm::new();
        fsm.feed(KEY_SYSRQ, true, mods(), 0, ARM_MS);
        fsm.feed(KEY_SYSRQ, false, mods(), 1, ARM_MS);
        fsm.feed(KEY_SYSRQ, true, mods(), 2900, ARM_MS);
        // Measured from the second arm, so this is inside the window.
        assert_eq!(
            fsm.feed(KEY_W, true, mods(), 3500, ARM_MS),
            Verdict::Run(b'w')
        );
    }

    #[test]
    fn the_command_table_covers_letters_and_digits() {
        assert_eq!(usage_to_command(KEY_A), Some(b'a'));
        assert_eq!(usage_to_command(KEY_Z), Some(b'z'));
        assert_eq!(usage_to_command(KEY_1), Some(b'1'));
        assert_eq!(usage_to_command(KEY_9), Some(b'9'));
        assert_eq!(usage_to_command(KEY_0), Some(b'0'));
        assert_eq!(usage_to_command(KEY_ENTER), None);
        assert_eq!(usage_to_command(KEY_LEFTSHIFT), None);
        assert_eq!(usage_to_command(KEY_KP_1), None);
    }

    #[test]
    fn the_command_key_is_the_position_not_the_glyph() {
        let mut fsm = SysrqFsm::new();
        fsm.feed(KEY_SYSRQ, true, mods(), 0, ARM_MS);
        let shifted = Mods {
            shift: true,
            ..Mods::default()
        };
        assert_eq!(
            fsm.feed(KEY_T, true, shifted, 5, ARM_MS),
            Verdict::Run(b't')
        );
    }
}
