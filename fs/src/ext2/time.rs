/// Unix-like timestamp for inode metadata, used only for mtime/ctime ordering.
pub fn now_unix() -> u32 {
    // TODO(tech-debt): always returns 0 — wire to CLOCK_REALTIME once the
    // kernel has a wall clock.
    #[cfg(not(test))]
    {
        0
    }
    #[cfg(test)]
    {
        0
    }
}
