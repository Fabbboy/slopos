//! Test fixtures for automatic setup/teardown via RAII.

/// Available fixture types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixtureKind {
    /// No fixture needed - test runs standalone
    None,
    /// Scheduler fixture - initializes task manager and scheduler
    Scheduler,
    /// Memory fixture - for memory subsystem tests
    Memory,
    /// IRQ fixture - for interrupt tests
    Irq,
}

/// Trait for test fixtures with automatic setup/teardown.
///
/// Implementors provide setup logic and cleanup via Drop.
/// The KIND constant allows the test runner to create fixtures dynamically.
pub trait TestFixture: Sized {
    /// The fixture kind for dynamic dispatch in test runner
    const KIND: FixtureKind;

    /// Setup the test environment. Returns Err on failure.
    fn setup() -> Result<Self, &'static str>;

    /// Teardown is handled by Drop implementation
    fn teardown(&mut self);
}

/// No-op fixture for tests that don't need setup/teardown.
pub struct NoFixture;

impl TestFixture for NoFixture {
    const KIND: FixtureKind = FixtureKind::None;

    fn setup() -> Result<Self, &'static str> {
        Ok(Self)
    }

    fn teardown(&mut self) {}
}

impl Drop for NoFixture {
    fn drop(&mut self) {
        self.teardown();
    }
}
