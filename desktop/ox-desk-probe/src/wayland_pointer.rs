//! Drive the probe's private Wayland seat through the compositor's virtual pointer protocol.
use std::io::{self, BufRead, Write};
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_pointer, wl_registry},
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

struct Seat;
delegate_noop!(Seat: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(Seat: ignore ZwlrVirtualPointerV1);
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Seat {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

pub fn run() -> io::Result<()> {
    let conn = Connection::connect_to_env().map_err(io::Error::other)?;
    let (globals, mut queue) = registry_queue_init::<Seat>(&conn).map_err(io::Error::other)?;
    let handle = queue.handle();
    let manager: ZwlrVirtualPointerManagerV1 =
        globals.bind(&handle, 1..=2, ()).map_err(io::Error::other)?;
    let pointer = manager.create_virtual_pointer(None, &handle, ());
    for line in io::stdin().lock().lines() {
        let line = line?;
        let fields: Vec<_> = line.split_whitespace().collect();
        let time = (ox_desk_core::perf::monotonic_ns() / 1_000_000) as u32;
        let integer = |value: &str| value.parse::<u32>().map_err(io::Error::other);
        match fields.as_slice() {
            ["move", x, y, w, h] => {
                pointer.motion_absolute(time, integer(x)?, integer(y)?, integer(w)?, integer(h)?)
            }
            ["press"] => pointer.button(time, 0x110, wl_pointer::ButtonState::Pressed),
            ["release"] => pointer.button(time, 0x110, wl_pointer::ButtonState::Released),
            ["click"] => {
                pointer.button(time, 0x110, wl_pointer::ButtonState::Pressed);
                pointer.frame();
                pointer.button(time, 0x110, wl_pointer::ButtonState::Released);
            }
            ["down"] => {
                pointer.axis_source(wl_pointer::AxisSource::Wheel);
                pointer.axis_discrete(time, wl_pointer::Axis::VerticalScroll, 15., 1);
            }
            _ => return Err(io::Error::other("invalid pointer command")),
        }
        pointer.frame();
        queue.roundtrip(&mut Seat).map_err(io::Error::other)?;
        writeln!(io::stdout().lock(), "ok")?;
    }
    Ok(())
}
