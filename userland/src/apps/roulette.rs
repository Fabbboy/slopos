use crate::syscall::{DisplayInfo, roulette, window};

fn text_fallback(fate: u32) {
    eprintln!("ROULETTE: framebuffer unavailable, using text fallback");
    eprintln!("Fate number: {fate}");
}

pub fn roulette_user_main() {
    println!("ROULETTE: start");
    let spin = roulette::spin();
    let fate = spin as u32;

    let mut info = DisplayInfo::default();
    let fb_rc = window::fb_info(&mut info);
    let fb_ok = fb_rc == 0 && info.width != 0 && info.height != 0;

    if !fb_ok {
        text_fallback(fate);
    } else {
        // The virtcon seat, which outranks the compositor's: roulette draws
        // straight to the framebuffer before a compositor exists, and must be
        // able to take the screen back if one is already up.
        if window::screen_acquire(window::SEAT_VIRTCON) < 0 {
            text_fallback(fate);
        } else {
            println!("ROULETTE: fb_info ok, drawing wheel");
            let _ = roulette::draw(fate);
        }
    }

    std::thread::sleep(std::time::Duration::from_millis(180));
    roulette::result(spin);
    std::thread::sleep(std::time::Duration::from_millis(120));
    std::process::exit(0);
}
