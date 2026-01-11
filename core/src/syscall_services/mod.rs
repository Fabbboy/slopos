pub mod fate;
pub mod input;
pub mod tty;
pub mod video;

pub use fate::*;
pub use input::*;
pub use tty::*;
pub use video::*;

pub fn init_all_syscall_services(
    video: &'static VideoServices,
    input: &'static InputServices,
    tty: &'static TtyServices,
    fate: &'static FateServices,
) {
    register_video_services(video);
    register_input_services(input);
    register_tty_services(tty);
    register_fate_services(fate);
}
