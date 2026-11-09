//! Moves the pointer via XTEST (as RustDesk does) and fills/reads back a window via GetImage
//! (as the X11 capturer does). Run while `x11-privacy on` is active.
use x11rb::{connection::Connection, protocol::{xproto::*, xtest::ConnectionExt as _}, COPY_DEPTH_FROM_PARENT};
fn main() {
    let (c, s) = x11rb::connect(None).unwrap();
    let scr = &c.setup().roots[s];
    for (x, y) in [(100i16, 200i16), (640, 400)] {
        c.xtest_fake_input(MOTION_NOTIFY_EVENT, 0, 0, scr.root, x, y, 0).unwrap();
        c.flush().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        let p = c.query_pointer(scr.root).unwrap().reply().unwrap();
        println!("xtest move to ({x},{y}) -> pointer at ({},{})", p.root_x, p.root_y);
    }
    let w = c.generate_id().unwrap();
    c.create_window(COPY_DEPTH_FROM_PARENT, w, scr.root, 0, 0, 50, 50, 0, WindowClass::INPUT_OUTPUT, 0,
        &CreateWindowAux::new().background_pixel(0x00ff8800).override_redirect(1)).unwrap();
    c.map_window(w).unwrap(); c.clear_area(false, w, 0, 0, 0, 0).unwrap(); c.flush().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let img = c.get_image(ImageFormat::Z_PIXMAP, scr.root, 10, 10, 1, 1, !0).unwrap().reply().unwrap();
    println!("captured pixel BGRX = {:?} (expect [0, 136, 255, ..])", &img.data[..4]);
}
