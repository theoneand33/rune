//! XDND source for [`super::begin`] on X11 windows, and on XWayland, whose compositor carries the
//! drag on to Wayland apps. A thread on a private connection owns `XdndSelection` from a hidden
//! window, follows the pointer by polling while the button that started the drag stays down, and
//! speaks XDND (version 5 down to 3) to the aware window under it. The data goes out by
//! selection requests, as [`super::mime_data`] lists it.
//!
//! The press's window keeps the pointer (winit holds its implicit grab), so the thread watches
//! the pointer instead of grabbing it. When it ends, the thread stores the outcome and wakes the
//! loop; [`settle`] runs `done` there.
//!
//! shortcut: no drag image, cursor feedback or Escape to cancel, and no INCR for data over the
//! server's request size; add a grab and an override-redirect image window if users ask.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, KeyButMask, PropMode,
    SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

use super::{DragOperation, DragOutStart, DragOutcome};

type Slot = Arc<Mutex<Option<DragOutcome>>>;
type Done = Box<dyn FnOnce(DragOutcome)>;
type Data = Vec<(&'static str, Arc<[u8]>)>;

thread_local! {
    /// Sessions still out, with where their thread leaves the outcome.
    static PENDING: RefCell<Vec<(Slot, Done)>> = const { RefCell::new(Vec::new()) };
}

/// The newest XDND version spoken; targets that announce less get theirs.
const VERSION: u32 = 5;
/// How long after the release a target may take to answer: its status, then its finish.
const STATUS_WAIT: Duration = Duration::from_millis(500);
const FINISH_WAIT: Duration = Duration::from_secs(10);

pub(super) fn begin(window: &winit::window::Window, start: DragOutStart, done: Done, wake: Option<crate::Waker>) {
    let own = match window.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Xlib(h)) => h.window as Window,
        Ok(RawWindowHandle::Xcb(h)) => h.window.get(),
        // A Wayland window: its seat is winit's, which starts no drags.
        _ => return done(DragOutcome::Cancelled),
    };
    let data = super::mime_data(&start.item);
    if data.is_empty() { return done(DragOutcome::Cancelled); }
    let moves = start.item.offers(DragOperation::Move) && !start.item.offers(DragOperation::Copy);
    let within_app = start.item.drops_within_app();
    let slot = Slot::default();
    PENDING.with(|p| p.borrow_mut().push((slot.clone(), done)));
    let spawned = std::thread::Builder::new().name("rune-xdnd".into()).spawn(move || {
        let outcome = run(own, within_app, moves, data).unwrap_or(DragOutcome::Cancelled);
        *slot.lock().unwrap() = Some(outcome);
        if let Some(wake) = wake { wake(); }
    });
    if spawned.is_err() { settle_with(Some(DragOutcome::Cancelled)); }
}

/// Runs `done` for each session whose thread has finished.
pub(super) fn settle() { settle_with(None) }

/// `fail`: the newest session never started; it ends so.
fn settle_with(fail: Option<DragOutcome>) {
    let ended: Vec<_> = PENDING.with(|p| {
        let mut p = p.borrow_mut();
        if let Some(outcome) = fail && let Some((slot, _)) = p.last() { *slot.lock().unwrap() = Some(outcome); }
        let (ended, open): (Vec<_>, Vec<_>) = p.drain(..).partition(|(slot, _)| slot.lock().unwrap().is_some());
        *p = open;
        ended
    });
    for (slot, done) in ended {
        if let Some(outcome) = slot.lock().unwrap().take() { done(outcome); }
    }
}

struct Atoms {
    aware: Atom, proxy: Atom, selection: Atom, type_list: Atom, enter: Atom, position: Atom, status: Atom, leave: Atom,
    drop: Atom, finished: Atom, copy: Atom, moves: Atom, link: Atom, targets: Atom, pid: Atom,
}

impl Atoms {
    fn new(conn: &RustConnection) -> Result<Atoms, Box<dyn std::error::Error>> {
        let atom = |name: &str| -> Result<Atom, Box<dyn std::error::Error>> { Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom) };
        Ok(Atoms {
            aware: atom("XdndAware")?, proxy: atom("XdndProxy")?, selection: atom("XdndSelection")?, type_list: atom("XdndTypeList")?,
            enter: atom("XdndEnter")?, position: atom("XdndPosition")?, status: atom("XdndStatus")?, leave: atom("XdndLeave")?,
            drop: atom("XdndDrop")?, finished: atom("XdndFinished")?, copy: atom("XdndActionCopy")?, moves: atom("XdndActionMove")?,
            link: atom("XdndActionLink")?, targets: atom("TARGETS")?, pid: atom("_NET_WM_PID")?,
        })
    }
}

/// The aware window under the pointer: where messages go (its proxy, if it names one), the
/// window they name, and the version both speak.
#[derive(Clone, Copy, PartialEq)]
struct Target { to: Window, window: Window, version: u32 }

/// The session's dealings with its current target.
struct Session {
    target: Option<Target>,
    /// A position went out and its status hasn't come back; XDND sends one at a time.
    waiting: bool,
    sent: Option<(i16, i16)>,
    accepted: bool,
    action: Atom,
}

fn run(own: Window, within_app: bool, moves: bool, data: Data) -> Result<DragOutcome, Box<dyn std::error::Error>> {
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots.get(screen).ok_or("no screen")?.root;
    let atoms = Atoms::new(&conn)?;
    let types: Vec<Atom> = data.iter().map(|(mime, _)| conn.intern_atom(false, mime.as_bytes()).map(|c| c.reply())).collect::<Result<Result<Vec<_>, _>, _>>()??.into_iter().map(|r| r.atom).collect();
    let source = conn.generate_id()?;
    conn.create_window(0, source, root, -10, -10, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &CreateWindowAux::new())?;
    conn.change_property32(PropMode::REPLACE, source, atoms.type_list, AtomEnum::ATOM, &types)?;
    conn.set_selection_owner(source, atoms.selection, CURRENT_TIME)?;
    conn.flush()?;
    let action = if moves { atoms.moves } else { atoms.copy };
    let skip = |w: Window| w == own || w == source || (!within_app && pid(&conn, &atoms, w) == Some(std::process::id()));
    let mut s = Session { target: None, waiting: false, sent: None, accepted: false, action: NONE };
    let send = |to: Window, window: Window, kind: Atom, data: [u32; 5]| -> Result<(), Box<dyn std::error::Error>> {
        conn.send_event(false, to, EventMask::NO_EVENT, ClientMessageEvent::new(32, window, kind, data))?;
        Ok(())
    };

    // Follow the pointer while the button is down.
    loop {
        pump(&conn, &atoms, source, &types, &data, &mut s, None)?;
        let p = conn.query_pointer(root)?.reply()?;
        if !p.mask.contains(KeyButMask::BUTTON1) { break; }
        let at = (p.root_x, p.root_y);
        let under = target_at(&conn, &atoms, root, at, &skip);
        if under != s.target {
            if let Some(t) = s.target { send(t.to, t.window, atoms.leave, [source, 0, 0, 0, 0])?; }
            if let Some(t) = under {
                let more = u32::from(types.len() > 3);
                let first = |i: usize| types.get(i).copied().unwrap_or(NONE);
                send(t.to, t.window, atoms.enter, [source, t.version << 24 | more, first(0), first(1), first(2)])?;
            }
            s = Session { target: under, waiting: false, sent: None, accepted: false, action: NONE };
        }
        if let Some(t) = s.target && !s.waiting && s.sent != Some(at) {
            let xy = (u32::from(at.0 as u16) << 16) | u32::from(at.1 as u16);
            send(t.to, t.window, atoms.position, [source, 0, xy, CURRENT_TIME, action])?;
            s.waiting = true;
            s.sent = Some(at);
        }
        conn.flush()?;
        std::thread::sleep(Duration::from_millis(15));
    }

    // Released: drop where the last status allowed it.
    let Some(t) = s.target else { return Ok(DragOutcome::Cancelled) };
    let until = Instant::now() + STATUS_WAIT;
    while s.waiting && Instant::now() < until { pump(&conn, &atoms, source, &types, &data, &mut s, Some(until))?; }
    if !s.accepted {
        send(t.to, t.window, atoms.leave, [source, 0, 0, 0, 0])?;
        conn.flush()?;
        return Ok(DragOutcome::Cancelled);
    }
    send(t.to, t.window, atoms.drop, [source, 0, CURRENT_TIME, 0, 0])?;
    conn.flush()?;
    let until = Instant::now() + FINISH_WAIT;
    while Instant::now() < until {
        if let Some((accepted, action)) = pump(&conn, &atoms, source, &types, &data, &mut s, Some(until))? {
            // Before version 5 a finish carries neither; the last status stands.
            let (accepted, action) = if t.version >= 5 { (accepted, action) } else { (true, s.action) };
            return Ok(if !accepted { DragOutcome::Cancelled } else { outcome(&atoms, action) });
        }
    }
    // The target took the drop but never said it was done.
    Ok(outcome(&atoms, s.action))
}

fn outcome(atoms: &Atoms, action: Atom) -> DragOutcome {
    if action == atoms.moves { DragOutcome::Moved } else if action == atoms.link { DragOutcome::Linked } else { DragOutcome::Copied }
}

/// Handles what arrived: statuses, data requests and the finish, whose (accepted, action) it
/// returns. With `until`, waits for an event up to then.
fn pump(conn: &RustConnection, atoms: &Atoms, source: Window, types: &[Atom], data: &Data, s: &mut Session, until: Option<Instant>) -> Result<Option<(bool, Atom)>, Box<dyn std::error::Error>> {
    loop {
        let Some(event) = conn.poll_for_event()? else {
            if until.is_some_and(|u| Instant::now() < u) { std::thread::sleep(Duration::from_millis(5)); continue; }
            return Ok(None);
        };
        match event {
            Event::ClientMessage(e) if e.format == 32 => {
                let d = e.data.as_data32();
                let from_target = s.target.is_some_and(|t| t.window == d[0]);
                if e.type_ == atoms.status && from_target {
                    s.waiting = false;
                    s.accepted = d[1] & 1 != 0;
                    s.action = if s.accepted { d[4] } else { NONE };
                } else if e.type_ == atoms.finished && from_target {
                    return Ok(Some((d[1] & 1 != 0, d[2])));
                }
            }
            Event::SelectionRequest(e) if e.selection == atoms.selection => answer(conn, atoms, source, types, data, &e)?,
            _ => {}
        }
        if until.is_some() { return Ok(None); }
    }
}

/// Sends the data a target asked for, or refuses a type the drag doesn't offer.
fn answer(conn: &RustConnection, atoms: &Atoms, source: Window, types: &[Atom], data: &Data, e: &SelectionRequestEvent) -> Result<(), Box<dyn std::error::Error>> {
    // An obsolete requestor names no property: the target is the property.
    let property = if e.property == NONE { e.target } else { e.property };
    let given = if e.owner != source {
        false
    } else if e.target == atoms.targets {
        let mut list = vec![atoms.targets];
        list.extend_from_slice(types);
        conn.change_property32(PropMode::REPLACE, e.requestor, property, AtomEnum::ATOM, &list).is_ok()
    } else if let Some(i) = types.iter().position(|&t| t == e.target) {
        // Too large for one request (no INCR): this type is refused, the others still go.
        conn.change_property8(PropMode::REPLACE, e.requestor, property, e.target, &data[i].1).is_ok()
    } else {
        false
    };
    let notify = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT, sequence: 0, time: e.time, requestor: e.requestor,
        selection: e.selection, target: e.target, property: if given { property } else { NONE },
    };
    conn.send_event(false, e.requestor, EventMask::NO_EVENT, notify)?;
    conn.flush()?;
    Ok(())
}

/// The innermost XDND-aware window at `at` (root coordinates), walking down from the root in
/// stacking order. A skipped window (this app's own) ends the walk with none.
fn target_at(conn: &RustConnection, atoms: &Atoms, root: Window, at: (i16, i16), skip: &impl Fn(Window) -> bool) -> Option<Target> {
    let mut parent = root;
    loop {
        let child = conn.translate_coordinates(root, parent, at.0, at.1).ok()?.reply().ok()?.child;
        if child == NONE || skip(child) { return None; }
        if let Some(version) = aware(conn, atoms, child) {
            let proxy = window_property(conn, child, atoms.proxy);
            // A proxy names itself too; one that doesn't is stale and ignored.
            let to = proxy.filter(|&p| window_property(conn, p, atoms.proxy) == Some(p)).unwrap_or(child);
            return Some(Target { to, window: child, version: version.min(VERSION) });
        }
        parent = child;
    }
}

/// The XDND version a window takes drops with, if it takes them (version 3 and up).
fn aware(conn: &RustConnection, atoms: &Atoms, window: Window) -> Option<u32> {
    let reply = conn.get_property(false, window, atoms.aware, AtomEnum::ATOM, 0, 1).ok()?.reply().ok()?;
    reply.value32()?.next().filter(|&v| v >= 3)
}

fn window_property(conn: &RustConnection, window: Window, property: Atom) -> Option<Window> {
    let reply = conn.get_property(false, window, property, AtomEnum::WINDOW, 0, 1).ok()?.reply().ok()?;
    reply.value32()?.next().filter(|&w| w != NONE)
}

fn pid(conn: &RustConnection, atoms: &Atoms, window: Window) -> Option<u32> {
    let reply = conn.get_property(false, window, atoms.pid, AtomEnum::CARDINAL, 0, 1).ok()?.reply().ok()?;
    reply.value32()?.next()
}
