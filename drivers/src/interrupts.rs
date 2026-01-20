pub use slopos_lib::testing::config::{Suite, TestConfig, Verbosity, config_from_cmdline};
pub use slopos_lib::testing::suite_masks::{
    SUITE_ALL, SUITE_BASIC, SUITE_CONTROL, SUITE_MEMORY, SUITE_SCHEDULER,
};

pub type InterruptTestConfig = TestConfig;
