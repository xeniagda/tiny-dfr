use std::{
    fs::{File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::{io::{AsFd, BorrowedFd, OwnedFd}, fs::OpenOptionsExt}
    },
    path::Path,
    collections::HashMap
};
use cairo::{
    ImageSurface, Format, Context, Surface,
    FontSlant, FontWeight
};
use drm::{
    ClientCapability, Device as DrmDevice, buffer::DrmFourcc,
    control::{
        connector, Device as ControlDevice, property, ResourceHandle, atomic, AtomicCommitFlags,
        dumbbuffer::DumbBuffer, framebuffer, ClipRect
    }
};
use anyhow::{Result, anyhow};
use input::{
    Libinput, LibinputInterface, Device as InputDevice,
    event::{
        Event, device::DeviceEvent, EventTrait,
        touch::{TouchEvent, TouchEventPosition, TouchEventSlot}
    }
};
use libc::{O_RDONLY, O_RDWR, O_WRONLY};
use input_linux::{uinput::UInputHandle, EventKind, Key, SynchronizeKind};
use input_linux_sys::{uinput_setup, input_id, timeval, input_event};

const DFR_WIDTH: i32 = 2008;
const DFR_HEIGHT: i32 = 64;
const BUTTON_COLOR_INACTIVE: f64 = 0.267;
const BUTTON_COLOR_ACTIVE: f64 = 0.567;

struct Card(File);
impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl ControlDevice for Card {}
impl DrmDevice for Card {}

impl Card {
    fn open(path: &str) -> Self {
        let mut options = OpenOptions::new();
        options.read(true);
        options.write(true);

        Card(options.open(path).unwrap())
    }
}

struct DrmBackend {
    card: Card,
    db: DumbBuffer,
    fb: framebuffer::Handle
}

impl Drop for DrmBackend {
    fn drop(&mut self) {
        self.card.destroy_framebuffer(self.fb).unwrap();
        self.card.destroy_dumb_buffer(self.db).unwrap();
    }
}

struct Button {
    text: String,
    action: Key
}

struct FunctionLayer {
    buttons: Vec<Button>
}

impl FunctionLayer {
    fn draw(&self, surface: &Surface, active_buttons: &[bool], dim: f64) {
        let c = Context::new(&surface).unwrap();
        c.translate(DFR_HEIGHT as f64, 0.0);
        c.rotate((90.0f64).to_radians());
        let button_width = DFR_WIDTH as f64 / (self.buttons.len() + 1) as f64;
        let spacing_width = (DFR_WIDTH as f64 - self.buttons.len() as f64 * button_width) / (self.buttons.len() + 1) as f64;
        c.set_source_rgb(0.0, 0.0, 0.0);
        c.paint().unwrap();
        c.select_font_face("sans-serif", FontSlant::Normal, FontWeight::Normal);
        c.set_font_size(24.0);
        for (i, button) in self.buttons.iter().enumerate() {
            let left_edge = i as f64 * (button_width + spacing_width) + spacing_width;
            let color = (if active_buttons[i] { BUTTON_COLOR_ACTIVE } else { BUTTON_COLOR_INACTIVE }) * dim;
            c.set_source_rgb(color, color, color);
            c.rectangle(left_edge, 0.09 * DFR_HEIGHT as f64, button_width, 0.82 * DFR_HEIGHT as f64);
            c.fill().unwrap();
            c.set_source_rgb(dim, dim, dim);
            let extents = c.text_extents(&button.text).unwrap();
            c.move_to(
                left_edge + button_width / 2.0 - extents.width() / 2.0,
                DFR_HEIGHT as f64 / 2.0 + extents.height() / 2.0
            );
            c.show_text(&button.text).unwrap();
        }
    }
}

fn find_prop_id<T: ResourceHandle>(
    card: &Card,
    handle: T,
    name: &'static str,
) -> Result<property::Handle> {
    let props = card.get_properties(handle)?;
    for id in props.as_props_and_values().0 {
        let info = card.get_property(*id).unwrap();
        if info.name().to_str()? == name {
            return Ok(*id);
        }
    }
    return Err(anyhow!("Property not found"));
}

fn try_open_card(path: &str) -> Result<DrmBackend> {
    let card = Card::open(path);
    card.set_client_capability(ClientCapability::UniversalPlanes, true).unwrap();
    card.set_client_capability(ClientCapability::Atomic, true).unwrap();
    card.acquire_master_lock().unwrap();


    let res = card.resource_handles().unwrap();
    let coninfo = res
        .connectors()
        .iter()
        .flat_map(|con| card.get_connector(*con, true))
        .collect::<Vec<_>>();
    let crtcinfo = res
        .crtcs()
        .iter()
        .flat_map(|crtc| card.get_crtc(*crtc))
        .collect::<Vec<_>>();

    let con = coninfo
        .iter()
        .find(|&i| i.state() == connector::State::Connected)
        .ok_or(anyhow!("No connected connectors found")).unwrap();

    let &mode = con.modes().get(0).ok_or(anyhow!("No modes found")).unwrap();
    let (disp_width, disp_height) = mode.size();
    if disp_height / disp_width < 30 {
        return Err(anyhow!("This does not look like a touchbar"));
    }
    let crtc = crtcinfo.get(0).ok_or(anyhow!("No crtcs found")).unwrap();
    let fmt = DrmFourcc::Xrgb8888;
    let db = card.create_dumb_buffer((64, disp_height.into()), fmt, 32).unwrap();

    let fb = card.add_framebuffer(&db, 24, 32).unwrap();
    let plane = *card.plane_handles().unwrap().get(0).ok_or(anyhow!("No planes found")).unwrap();

    let mut atomic_req = atomic::AtomicModeReq::new();
    atomic_req.add_property(
        con.handle(),
        find_prop_id(&card, con.handle(), "CRTC_ID").unwrap(),
        property::Value::CRTC(Some(crtc.handle())),
    );
    let blob = card.create_property_blob(&mode).unwrap();

    atomic_req.add_property(
        crtc.handle(),
        find_prop_id(&card, crtc.handle(), "MODE_ID").unwrap(),
        blob,
    );
    atomic_req.add_property(
        crtc.handle(),
        find_prop_id(&card, crtc.handle(), "ACTIVE").unwrap(),
        property::Value::Boolean(true),
    );
    atomic_req.add_property(
        plane,
        find_prop_id(&card, plane, "FB_ID").unwrap(),
        property::Value::Framebuffer(Some(fb)),
    );
    atomic_req.add_property(
        plane,
        find_prop_id(&card, plane, "CRTC_ID").unwrap(),
        property::Value::CRTC(Some(crtc.handle())),
    );
    atomic_req.add_property(
        plane,
        find_prop_id(&card, plane, "SRC_X").unwrap(),
        property::Value::UnsignedRange(0),
    );
    atomic_req.add_property(
        plane,
        find_prop_id(&card, plane, "SRC_Y").unwrap(),
        property::Value::UnsignedRange(0),
    );
    atomic_req.add_property(
        plane,
        find_prop_id(&card, plane, "SRC_W").unwrap(),
        property::Value::UnsignedRange((mode.size().0 as u64) << 16),
    );
    atomic_req.add_property(
        plane,
        find_prop_id(&card, plane, "SRC_H").unwrap(),
        property::Value::UnsignedRange((mode.size().1 as u64) << 16),
    );
    atomic_req.add_property(
        plane,
        find_prop_id(&card, plane, "CRTC_X").unwrap(),
        property::Value::SignedRange(0),
    );
    atomic_req.add_property(
        plane,
        find_prop_id(&card, plane, "CRTC_Y").unwrap(),
        property::Value::SignedRange(0),
    );
    atomic_req.add_property(
        plane,
        find_prop_id(&card, plane, "CRTC_W").unwrap(),
        property::Value::UnsignedRange(mode.size().0 as u64),
    );
    atomic_req.add_property(
        plane,
        find_prop_id(&card, plane, "CRTC_H").unwrap(),
        property::Value::UnsignedRange(mode.size().1 as u64),
    );

    card.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, atomic_req).unwrap();


    Ok(DrmBackend { card, db, fb })
}


struct Interface;

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        OpenOptions::new()
            .custom_flags(flags)
            .read((flags & O_RDONLY != 0) | (flags & O_RDWR != 0))
            .write((flags & O_WRONLY != 0) | (flags & O_RDWR != 0))
            .open(path)
            .map(|file| file.into())
            .map_err(|err| err.raw_os_error().unwrap())
    }
    fn close_restricted(&mut self, fd: OwnedFd) {
        _ = File::from(fd);
    }
}


fn button_hit(num: u32, idx: u32, x: f64, y: f64) -> bool {
    let button_width = DFR_WIDTH as f64 / (num + 1) as f64;
    let spacing_width = (DFR_WIDTH as f64 - num as f64 * button_width) / (num + 1) as f64;
    let left_edge = idx as f64 * (button_width + spacing_width) + spacing_width;
    if x < left_edge || x > (left_edge + button_width) {
        return false
    }
    y > 0.09 * DFR_HEIGHT as f64 && y < 0.91 * DFR_HEIGHT as f64
}

fn emit<F>(uinput: &mut UInputHandle<F>, ty: EventKind, code: u16, value: i32) where F: AsRawFd {
    uinput.write(&[input_event {
        value: value,
        type_: ty as u16,
        code: code,
        time: timeval {
            tv_sec: 0,
            tv_usec: 0
        }
    }]).unwrap();
}

fn main() {
    let mut surface = ImageSurface::create(Format::ARgb32, DFR_HEIGHT, DFR_WIDTH).unwrap();
    let layer = FunctionLayer {
        buttons: vec![
            Button { text: "F1".to_string(), action: Key::F1 },
            Button { text: "F2".to_string(), action: Key::F2 },
            Button { text: "F3".to_string(), action: Key::F3 },
            Button { text: "F4".to_string(), action: Key::F4 },
            Button { text: "F5".to_string(), action: Key::F5 },
            Button { text: "F6".to_string(), action: Key::F6 },
            Button { text: "F7".to_string(), action: Key::F7 },
            Button { text: "F8".to_string(), action: Key::F8 },
            Button { text: "F9".to_string(), action: Key::F9 },
            Button { text: "F10".to_string(), action: Key::F10 },
            Button { text: "F11".to_string(), action: Key::F11 },
            Button { text: "F12".to_string(), action: Key::F12 }
        ]
    };
    let mut button_state = vec![false; 12];
    let mut needs_redraw = true;
    let mut drm = try_open_card("/dev/dri/card0").unwrap();
    let mut input = Libinput::new_with_udev(Interface);
    input.udev_assign_seat("seat0").unwrap();
    let mut uinput = UInputHandle::new(OpenOptions::new().write(true).open("/dev/uinput").unwrap());
    uinput.set_evbit(EventKind::Key).unwrap();
    for button in &layer.buttons {
        uinput.set_keybit(button.action).unwrap();
    }
    uinput.dev_setup(&uinput_setup {
        id: input_id {
            bustype: 0x19,
            vendor: 0x1209,
            product: 0x316E,
            version: 1
        },
        ff_effects_max: 0,
        name: [
            b'D', b'y', b'n', b'a', b'm', b'i', b'c', b' ',
            b'F', b'u', b'n', b'c', b't', b'i', b'o', b'n', b' ',
            b'R', b'o', b'w', b' ',
            b'V', b'i', b'r', b't', b'u', b'a', b'l', b' ',
            b'I', b'n', b'p', b'u', b't', b' ',
            b'D', b'e', b'v', b'i', b'c', b'e',
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
        ]
    }).unwrap();
    uinput.dev_create().unwrap();
    let mut digitizer: Option<InputDevice> = None;
    let mut touches = HashMap::new();
    loop {
        if needs_redraw {
            needs_redraw = false;
            layer.draw(&surface, &button_state, 1.0);
            let mut map = drm.card.map_dumb_buffer(&mut drm.db).unwrap();
            let data = surface.data().unwrap();
            map.as_mut()[..data.len()].copy_from_slice(&data);
            drm.card.dirty_framebuffer(drm.fb, &[ClipRect{x1: 0, y1: 0, x2: DFR_HEIGHT as u16, y2: DFR_WIDTH as u16}]).unwrap();
        }
        input.dispatch().unwrap();
        for event in &mut input {
            match event {
                Event::Device(DeviceEvent::Added(evt)) => {
                    let dev = evt.device();
                    if dev.name().contains("MacBookPro17,1 Touch Bar") {
                        digitizer = Some(dev);
                    }
                },
                Event::Touch(te) => {
                    if Some(te.device()) != digitizer {
                        continue
                    }
                    match te {
                        TouchEvent::Down(dn) => {
                            let x = dn.x_transformed(DFR_WIDTH as u32);
                            let y = dn.y_transformed(DFR_HEIGHT as u32);
                            let btn = (x / (DFR_WIDTH as f64 / 12.0)) as u32;
                            if button_hit(12, btn, x, y) {
                                touches.insert(dn.seat_slot(), btn);
                                button_state[btn as usize] = true;
                                needs_redraw = true;
                                emit(&mut uinput, EventKind::Key, layer.buttons[btn as usize].action as u16, 1);
                                emit(&mut uinput, EventKind::Synchronize, SynchronizeKind::Report as u16, 0);
                            }
                        },
                        TouchEvent::Motion(mtn) => {
                            if !touches.contains_key(&mtn.seat_slot()) {
                                continue;
                            }

                            let x = mtn.x_transformed(DFR_WIDTH as u32);
                            let y = mtn.y_transformed(DFR_HEIGHT as u32);
                            let btn = *touches.get(&mtn.seat_slot()).unwrap();
                            let hit = button_hit(12, btn, x, y);
                            if button_state[btn as usize] != hit {
                                button_state[btn as usize] = hit;
                                needs_redraw = true;
                                emit(&mut uinput, EventKind::Key, layer.buttons[btn as usize].action as u16, hit as i32);
                                emit(&mut uinput, EventKind::Synchronize, SynchronizeKind::Report as u16, 0);
                            }
                        },
                        TouchEvent::Up(up) => {
                            if !touches.contains_key(&up.seat_slot()) {
                                continue;
                            }
                            let btn = *touches.get(&up.seat_slot()).unwrap() as usize;
                            if button_state[btn] {
                                button_state[btn] = false;
                                needs_redraw = true;
                                emit(&mut uinput, EventKind::Key, layer.buttons[btn].action as u16, 0);
                                emit(&mut uinput, EventKind::Synchronize, SynchronizeKind::Report as u16, 0);
                            }
                        }
                        _ => {}
                    }
                },
                _ => {}
            }
        }
    }


}
