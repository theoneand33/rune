//! Drag and drop on a layer-shell overlay, through `wl_data_device` on the overlay's own seat.
//!
//! Drops: an offer carrying `text/uri-list` that enters the surface is accepted as a copy and its
//! list read on a thread; the files it names then hover and drop as a window's do. Drags out:
//! `start_drag` from the press that crossed the threshold, with a `wl_data_source` serving
//! [`crate::drag_out::mime_data`] and the element's picture as the drag icon.

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};

use wayland_client::protocol::wl_buffer::{self, WlBuffer};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::{DndAction, WlDataDeviceManager};
use wayland_client::protocol::wl_data_offer::{self, WlDataOffer};
use wayland_client::protocol::wl_data_source::{self, WlDataSource};
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface::{self, WlSurface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, event_created_child};

use super::{Input, State};
use crate::drag_out::{DragImage, DragOperation, DragOutStart, DragOutcome};

const URI_LIST: &str = "text/uri-list";
/// More than any file list a person drags.
const LIST_LIMIT: u64 = 4 * 1024 * 1024;

/// A file drag over the surface, in surface points.
pub(crate) enum FileInput {
    Hover(Vec<PathBuf>, f32, f32),
    Move(f32, f32),
    Leave,
    Drop(Vec<PathBuf>, f32, f32),
}

/// The offer over the surface, by how far its files have been read.
#[derive(Default)]
enum Offer {
    #[default]
    None,
    /// Entered without files (or from this app's own drag): declined until it leaves.
    Declined(WlDataOffer),
    Reading { offer: WlDataOffer, at: (f32, f32), dropped: bool, list: mpsc::Receiver<Vec<PathBuf>> },
    Over { offer: WlDataOffer, paths: Vec<PathBuf>, at: (f32, f32) },
}

/// A drag this surface started, until its source finishes or is cancelled.
struct Drag {
    source: WlDataSource,
    icon: Option<Icon>,
    data: Vec<(&'static str, Arc<[u8]>)>,
    action: DndAction,
    within_app: bool,
    done: Box<dyn FnOnce(DragOutcome)>,
}

struct Icon { surface: WlSurface, pool: WlShmPool, buffer: WlBuffer }

impl Icon {
    fn destroy(self) {
        self.surface.destroy();
        self.buffer.destroy();
        self.pool.destroy();
    }
}

#[derive(Default)]
pub(super) struct Dnd {
    manager: Option<WlDataDeviceManager>,
    shm: Option<WlShm>,
    device: Option<WlDataDevice>,
    /// The offer the compositor introduced last, with whether it carries files, until an enter
    /// or a selection claims it.
    announced: Option<(WlDataOffer, bool)>,
    offer: Offer,
    /// The action the compositor chose for the offer over the surface.
    offer_action: Option<DndAction>,
    drag: Option<Drag>,
    wake: Option<crate::Waker>,
    /// The serial of the last primary press on the surface: what `start_drag` needs.
    pub(super) press: Option<u32>,
}

impl Dnd {
    pub(super) fn new(manager: Option<WlDataDeviceManager>, shm: Option<WlShm>, wake: Option<crate::Waker>) -> Dnd {
        Dnd { manager, shm, wake, ..Dnd::default() }
    }

    /// Listens on `seat`'s data device, once.
    pub(super) fn attach(&mut self, seat: &wayland_client::protocol::wl_seat::WlSeat, qh: &QueueHandle<State>) {
        if self.device.is_none() && let Some(manager) = &self.manager { self.device = Some(manager.get_data_device(seat, qh, ())); }
    }

    /// Whether the surface can start drags.
    pub(super) fn drags_out(&self) -> bool { self.device.is_some() }

    /// A read file list landing: the hover it was waiting for, or the drop.
    pub(super) fn poll(&mut self, input: &mut Vec<Input>) {
        let Offer::Reading { list, .. } = &self.offer else { return };
        let paths = match list.try_recv() {
            Ok(paths) => paths,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Vec::new(),
        };
        let Offer::Reading { offer, at, dropped, .. } = std::mem::take(&mut self.offer) else { return };
        if dropped {
            self.finish(offer, &paths);
            if !paths.is_empty() { input.push(Input::Files(FileInput::Drop(paths, at.0, at.1))); }
        } else if paths.is_empty() {
            // Accepted before the list was read: no action now refuses the drop.
            if offer.version() >= 3 { offer.set_actions(DndAction::empty(), DndAction::empty()); }
            self.offer = Offer::Declined(offer);
        } else {
            input.push(Input::Files(FileInput::Hover(paths.clone(), at.0, at.1)));
            self.offer = Offer::Over { offer, paths, at };
        }
    }

    /// Ends a dropped offer: a copy the compositor agreed to is finished, then the offer goes.
    fn finish(&self, offer: WlDataOffer, paths: &[PathBuf]) {
        if !paths.is_empty() && offer.version() >= 3 && self.offer_action.is_some_and(|a| a.intersects(DndAction::Copy | DndAction::Move)) { offer.finish(); }
        offer.destroy();
    }

    /// Starts dragging `start`'s item from `origin`, under the press that crossed the threshold.
    pub(super) fn start(&mut self, qh: &QueueHandle<State>, compositor: &WlCompositor, origin: &WlSurface, scale: i32, start: DragOutStart, picture: Option<DragImage>, done: Box<dyn FnOnce(DragOutcome)>) {
        let data = crate::drag_out::mime_data(&start.item);
        let (Some(manager), Some(device), Some(serial)) = (&self.manager, &self.device, self.press) else { return done(DragOutcome::Cancelled) };
        if data.is_empty() || self.drag.is_some() { return done(DragOutcome::Cancelled); }
        let source = manager.create_data_source(qh, ());
        for (mime, _) in &data { source.offer((*mime).to_owned()); }
        if source.version() >= 3 {
            let mut actions = DndAction::empty();
            if start.item.offers(DragOperation::Copy) { actions |= DndAction::Copy; }
            if start.item.offers(DragOperation::Move) { actions |= DndAction::Move; }
            source.set_actions(if actions.is_empty() { DndAction::Copy } else { actions });
        }
        let icon = picture.and_then(|picture| self.icon(qh, compositor, scale, &start, &picture));
        device.start_drag(Some(&source), origin, icon.as_ref().map(|i| &i.surface), serial);
        if let Some(icon) = &icon { icon.surface.commit(); }
        self.drag = Some(Drag { source, icon, data, action: DndAction::empty(), within_app: start.item.drops_within_app(), done });
    }

    /// The drag icon: `picture` in shared memory on its own surface, placed so the press keeps
    /// its spot on it. None when the compositor or the image won't do.
    fn icon(&self, qh: &QueueHandle<State>, compositor: &WlCompositor, scale: i32, start: &DragOutStart, picture: &DragImage) -> Option<Icon> {
        let shm = self.shm.as_ref()?;
        let rgba = crate::image::png_rgba8(picture.png_data()).ok()?;
        let scale = if compositor.version() >= 3 { scale.max(1) } else { 1 };
        // A buffer must be a whole number of points.
        let (w, h) = (rgba.width() as i32 / scale * scale, rgba.height() as i32 / scale * scale);
        if w == 0 || h == 0 { return None; }
        let mut argb = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h as u32 {
            for x in 0..w as u32 {
                let [r, g, b, a] = rgba.get_pixel(x, y).0;
                let pre = |c: u8| ((u16::from(c) * u16::from(a) + 127) / 255) as u8;
                argb.extend_from_slice(&[pre(b), pre(g), pre(r), a]);
            }
        }
        let file = shared_memory(&argb)?;
        let pool = shm.create_pool(file.as_fd(), argb.len() as i32, qh, ());
        let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
        let surface = compositor.create_surface(qh, IconSurface);
        if surface.version() >= 3 { surface.set_buffer_scale(scale); }
        let natural = ((w / scale) as f32, (h / scale) as f32);
        let frame = picture.frame(start.bounds, start.press, natural);
        let hot = ((start.press.0 - frame[0]).round() as i32, (start.press.1 - frame[1]).round() as i32);
        if surface.version() >= 5 {
            surface.attach(Some(&buffer), 0, 0);
            surface.offset(-hot.0, -hot.1);
        } else {
            surface.attach(Some(&buffer), -hot.0, -hot.1);
        }
        surface.damage(0, 0, natural.0 as i32, natural.1 as i32);
        Some(Icon { surface, pool, buffer })
    }

    /// The drag this surface started ended.
    fn ended(&mut self, outcome: impl FnOnce(DndAction) -> DragOutcome) {
        let Some(drag) = self.drag.take() else { return };
        drag.source.destroy();
        if let Some(icon) = drag.icon { icon.destroy(); }
        (drag.done)(outcome(drag.action));
    }

    pub(super) fn destroy(&mut self) {
        self.ended(|_| DragOutcome::Cancelled);
        if let Some((offer, _)) = self.announced.take() { offer.destroy(); }
        match std::mem::take(&mut self.offer) {
            Offer::Declined(offer) | Offer::Reading { offer, .. } | Offer::Over { offer, .. } => offer.destroy(),
            Offer::None => {}
        }
        if let Some(device) = self.device.take() { if device.version() >= 2 { device.release(); } }
        if let Some(manager) = self.manager.take() { drop(manager); }
    }
}

/// `bytes` in an anonymous shared-memory file.
#[cfg(target_os = "linux")]
fn shared_memory(bytes: &[u8]) -> Option<std::fs::File> {
    use std::os::fd::FromRawFd;
    // SAFETY: a fresh descriptor this function owns from here on.
    let fd = unsafe { libc::memfd_create(c"rune-drag-icon".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 { return None; }
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.write_all(bytes).ok()?;
    Some(file)
}

/// shortcut: no drag icon off Linux (no memfd); add `shm_open` if a BSD user asks.
#[cfg(not(target_os = "linux"))]
fn shared_memory(_: &[u8]) -> Option<std::fs::File> { None }

/// Reads the offer's file list on a thread, so a slow source never stalls the overlay.
fn read_list(offer: &WlDataOffer, wake: Option<crate::Waker>) -> Option<mpsc::Receiver<Vec<PathBuf>>> {
    let (reader, writer) = std::io::pipe().ok()?;
    offer.receive(URI_LIST.to_owned(), writer.as_fd());
    drop(writer);
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new().name("rune-dnd-read".into()).spawn(move || {
        let mut list = String::new();
        let _ = reader.take(LIST_LIMIT).read_to_string(&mut list);
        let _ = tx.send(crate::drag_out::uri_list_paths(&list));
        if let Some(wake) = wake { wake(); }
    }).ok()?;
    Some(rx)
}

impl Dispatch<WlDataDeviceManager, ()> for State {
    fn event(_: &mut Self, _: &WlDataDeviceManager, _: <WlDataDeviceManager as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
/// The drag icon's surface: its scale events aren't the overlay's.
struct IconSurface;
impl Dispatch<WlSurface, IconSurface> for State {
    fn event(_: &mut Self, _: &WlSurface, _: wl_surface::Event, _: &IconSurface, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlShm, ()> for State {
    fn event(_: &mut Self, _: &WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlShmPool, ()> for State {
    fn event(_: &mut Self, _: &WlShmPool, _: <WlShmPool as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlBuffer, ()> for State {
    fn event(_: &mut Self, _: &WlBuffer, _: wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlDataDevice, ()> for State {
    fn event(state: &mut Self, _: &WlDataDevice, event: wl_data_device::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let dnd = &mut state.dnd;
        match event {
            wl_data_device::Event::DataOffer { id } => {
                if let Some((old, _)) = dnd.announced.replace((id, false)) { old.destroy(); }
            }
            wl_data_device::Event::Enter { serial, surface, x, y, id } => {
                let claimed = dnd.announced.take_if(|(offer, _)| Some(&*offer) == id.as_ref());
                let Some((offer, files)) = claimed else { if let Some(id) = id { id.destroy() } return; };
                let at = (x as f32, y as f32);
                let own = dnd.drag.as_ref().is_some_and(|d| !d.within_app);
                let list = (state.surface.as_ref() == Some(&surface) && files && !own).then(|| read_list(&offer, dnd.wake.clone())).flatten();
                dnd.offer_action = None;
                dnd.offer = match list {
                    Some(list) => {
                        offer.accept(serial, Some(URI_LIST.to_owned()));
                        if offer.version() >= 3 { offer.set_actions(DndAction::Copy, DndAction::Copy); }
                        Offer::Reading { offer, at, dropped: false, list }
                    }
                    None => {
                        offer.accept(serial, None);
                        Offer::Declined(offer)
                    }
                };
            }
            wl_data_device::Event::Motion { x, y, .. } => {
                let to = (x as f32, y as f32);
                match &mut dnd.offer {
                    Offer::Reading { at, .. } => *at = to,
                    Offer::Over { at, .. } => {
                        *at = to;
                        state.input.push(Input::Files(FileInput::Move(to.0, to.1)));
                    }
                    _ => {}
                }
            }
            // A dropped offer waiting for its list stays until it lands.
            wl_data_device::Event::Leave => match std::mem::take(&mut dnd.offer) {
                reading @ Offer::Reading { dropped: true, .. } => dnd.offer = reading,
                Offer::Over { offer, .. } => {
                    offer.destroy();
                    state.input.push(Input::Files(FileInput::Leave));
                }
                Offer::Declined(offer) | Offer::Reading { offer, .. } => offer.destroy(),
                Offer::None => {}
            },
            wl_data_device::Event::Drop => match std::mem::take(&mut dnd.offer) {
                Offer::Reading { offer, at, list, .. } => dnd.offer = Offer::Reading { offer, at, dropped: true, list },
                Offer::Over { offer, paths, at } => {
                    dnd.finish(offer, &paths);
                    state.input.push(Input::Files(FileInput::Drop(paths, at.0, at.1)));
                }
                Offer::Declined(offer) => offer.destroy(),
                Offer::None => {}
            },
            // The clipboard is `arboard`'s; its offers here are let go.
            wl_data_device::Event::Selection { id } => {
                if let Some(offer) = id {
                    if dnd.announced.as_ref().is_some_and(|(a, _)| *a == offer) { dnd.announced = None; }
                    offer.destroy();
                }
            }
            _ => {}
        }
    }

    event_created_child!(State, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

impl Dispatch<WlDataOffer, ()> for State {
    fn event(state: &mut Self, offer: &WlDataOffer, event: wl_data_offer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let dnd = &mut state.dnd;
        match event {
            wl_data_offer::Event::Offer { mime_type } if mime_type == URI_LIST => {
                if let Some((announced, files)) = &mut dnd.announced && announced == offer { *files = true; }
            }
            wl_data_offer::Event::Action { dnd_action: WEnum::Value(action) } => dnd.offer_action = Some(action),
            _ => {}
        }
    }
}

impl Dispatch<WlDataSource, ()> for State {
    fn event(state: &mut Self, source: &WlDataSource, event: wl_data_source::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let dnd = &mut state.dnd;
        if dnd.drag.as_ref().is_none_or(|d| d.source != *source) { return; }
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                let Some(bytes) = dnd.drag.as_ref().and_then(|d| d.data.iter().find(|(m, _)| *m == mime_type)).map(|(_, b)| b.clone()) else { return };
                // On a thread: the reader may be this app, or slow.
                let _ = std::thread::Builder::new().name("rune-dnd-send".into()).spawn(move || {
                    let _ = std::fs::File::from(fd).write_all(&bytes);
                });
            }
            wl_data_source::Event::Action { dnd_action: WEnum::Value(action) } => {
                if let Some(drag) = &mut dnd.drag { drag.action = action; }
            }
            wl_data_source::Event::DndFinished => dnd.ended(|action| if action.contains(DndAction::Move) { DragOutcome::Moved } else { DragOutcome::Copied }),
            wl_data_source::Event::Cancelled => dnd.ended(|_| DragOutcome::Cancelled),
            _ => {}
        }
    }
}
