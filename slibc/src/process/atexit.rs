//! atexit — the last rites before the Wheel stops spinning.

const ATEXIT_MAX: usize = 32;

static mut HANDLERS: [Option<unsafe extern "C" fn()>; ATEXIT_MAX] = [None; ATEXIT_MAX];
static mut COUNT: usize = 0;

/// Register a function to be called at normal process termination.
///
/// Returns 0 on success, -1 if the table is full.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atexit(func: unsafe extern "C" fn()) -> i32 {
    if COUNT >= ATEXIT_MAX {
        return -1;
    }
    HANDLERS[COUNT] = Some(func);
    COUNT += 1;
    0
}

/// Run all registered atexit handlers in LIFO order.
pub unsafe fn run_atexit_handlers() {
    while COUNT > 0 {
        COUNT -= 1;
        if let Some(f) = HANDLERS[COUNT] {
            f();
        }
        HANDLERS[COUNT] = None;
    }
}
