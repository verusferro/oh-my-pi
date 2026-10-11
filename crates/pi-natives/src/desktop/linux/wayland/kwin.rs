//! Top-level window enumeration through `KWin` scripting.
//!
//! Many native Wayland clients (Chromium, custom-drawn terminals) expose no
//! AT-SPI frame, and none can report its global position over AT-SPI. `KWin`
//! knows both. A one-shot `KWin` script reads `workspace.windowList()` and
//! calls back into this process over D-Bus, the only way `KWin` scripts return
//! data.

use std::{
	fs::OpenOptions,
	io::Write,
	os::unix::fs::OpenOptionsExt,
	path::PathBuf,
	sync::{
		atomic::{AtomicU32, Ordering},
		mpsc,
	},
	time::Duration,
};

use serde::Deserialize;
use zbus::{blocking::Connection, message::Header, names::BusName};

use crate::desktop::{
	error::{CoreResult, DesktopError},
	linux::ax::AtSpiWindow,
	types::DesktopWindow,
};

const KWIN: &str = "org.kde.KWin";
const REPLY_PATH: &str = "/sh/omp/KWinWindows";
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);
/// Window ids from this source carry the prefix so they never collide with
/// AT-SPI ids.
pub(super) const ID_PREFIX: &str = "kwin:";

/// The script tolerates Plasma 5 names and always replies, so a script error
/// costs a message instead of the reply timeout.
const SCRIPT: &str = r#"
let reply;
try {
	const active = workspace.activeWindow ?? workspace.activeClient;
	const list = workspace.windowList ? workspace.windowList() : workspace.clientList();
	reply = JSON.stringify(list.filter(w => (w.normalWindow || w.dialog) && !w.minimized).map(w => ({
		id: String(w.internalId), title: w.caption, app: w.resourceClass, pid: w.pid,
		x: w.frameGeometry.x, y: w.frameGeometry.y,
		width: w.frameGeometry.width, height: w.frameGeometry.height,
		focused: w === active,
	})));
} catch (error) {
	reply = JSON.stringify({ error: String(error) });
}
callDBus("__REPLY_TO__", "/sh/omp/KWinWindows", "sh.omp.KWinWindows", "Windows", reply);
"#;

#[derive(Deserialize)]
#[serde(untagged)]
enum Reply {
	Windows(Vec<KWinWindow>),
	Error { error: String },
}

#[derive(Deserialize)]
struct KWinWindow {
	id:      String,
	title:   String,
	app:     String,
	pid:     i64,
	x:       f64,
	y:       f64,
	width:   f64,
	height:  f64,
	focused: bool,
}

impl KWinWindow {
	fn into_window(self) -> AtSpiWindow {
		AtSpiWindow {
			window:         DesktopWindow {
				id:      format!("{ID_PREFIX}{}", self.id),
				title:   self.title,
				app:     self.app,
				pid:     u32::try_from(self.pid).ok().filter(|&pid| pid > 0),
				x:       self.x.round() as i32,
				y:       self.y.round() as i32,
				width:   self.width.round().max(1.0) as u32,
				height:  self.height.round().max(1.0) as u32,
				focused: self.focused,
			},
			position_known: true,
		}
	}
}

struct Collector {
	kwin:  String,
	reply: mpsc::Sender<String>,
}

#[zbus::interface(name = "sh.omp.KWinWindows")]
impl Collector {
	fn windows(&self, #[zbus(header)] header: Header<'_>, json: String) {
		// Any same-user client can call this object; accept only KWin.
		if header
			.sender()
			.is_some_and(|sender| sender.as_str() == self.kwin)
		{
			let _ = self.reply.send(json);
		}
	}
}

/// `KWin`'s top-level windows, or `None` when `KWin` is not on the session bus.
pub(super) fn windows() -> CoreResult<Option<Vec<AtSpiWindow>>> {
	let failed =
		|what: &str, err: zbus::Error| DesktopError::ax_failed(format!("KWin {what}: {err}"));
	// zbus runs with its tokio feature here; its object server spawns onto the
	// ambient runtime and must keep serving while this thread blocks.
	let _runtime = super::portal::portal_runtime()?.enter();
	let connection = Connection::session().map_err(|err| failed("session bus", err))?;
	let dbus =
		zbus::blocking::fdo::DBusProxy::new(&connection).map_err(|err| failed("D-Bus proxy", err))?;
	let kwin = match dbus.get_name_owner(BusName::from_static_str(KWIN).expect("valid bus name")) {
		Ok(owner) => owner.to_string(),
		Err(zbus::fdo::Error::NameHasNoOwner(_)) => return Ok(None),
		Err(err) => return Err(failed("name owner", err.into())),
	};
	let unique = connection
		.unique_name()
		.ok_or_else(|| DesktopError::ax_failed("KWin: session bus gave no unique name"))?
		.to_string();
	let (reply, replies) = mpsc::channel();
	connection
		.object_server()
		.at(REPLY_PATH, Collector { kwin, reply })
		.map_err(|err| failed("reply object", err))?;

	static SEQUENCE: AtomicU32 = AtomicU32::new(0);
	let name =
		format!("omp-windows-{}-{}", std::process::id(), SEQUENCE.fetch_add(1, Ordering::Relaxed));
	let path = std::env::var_os("XDG_RUNTIME_DIR")
		.map_or_else(std::env::temp_dir, PathBuf::from)
		.join(format!("{name}.js"));
	let written = OpenOptions::new()
		.write(true)
		.create_new(true)
		.mode(0o600)
		.open(&path)
		.and_then(|mut file| file.write_all(SCRIPT.replace("__REPLY_TO__", &unique).as_bytes()));
	if let Err(err) = written {
		let _ = std::fs::remove_file(&path);
		return Err(DesktopError::ax_failed(format!("KWin script file: {err}")));
	}
	let script = path.to_string_lossy().into_owned();
	let ran =
		call(&connection, "/Scripting", "org.kde.kwin.Scripting", "loadScript", &(script, &name))
			.and_then(|message| message.body().deserialize::<i32>())
			.and_then(|id| {
				call(&connection, &format!("/Scripting/Script{id}"), "org.kde.kwin.Script", "run", &())
			});
	let json = ran.map_err(|err| failed("script", err)).and_then(|_| {
		replies
			.recv_timeout(REPLY_TIMEOUT)
			.map_err(|_| DesktopError::ax_failed("KWin script did not reply"))
	});
	let _ = call(&connection, "/Scripting", "org.kde.kwin.Scripting", "unloadScript", &(&name,));
	let _ = std::fs::remove_file(&path);
	parse(&json?).map(Some)
}

fn call<B: serde::Serialize + zbus::zvariant::DynamicType>(
	connection: &Connection,
	path: &str,
	iface: &str,
	method: &str,
	body: &B,
) -> zbus::Result<zbus::Message> {
	connection.call_method(Some(KWIN), path, Some(iface), method, body)
}

fn parse(json: &str) -> CoreResult<Vec<AtSpiWindow>> {
	match serde_json::from_str(json) {
		Ok(Reply::Windows(windows)) => Ok(windows.into_iter().map(KWinWindow::into_window).collect()),
		Ok(Reply::Error { error }) => Err(DesktopError::ax_failed(format!("KWin script: {error}"))),
		Err(err) => Err(DesktopError::ax_failed(format!("KWin script reply: {err}"))),
	}
}

/// AT-SPI windows completed by `KWin`: a window both report keeps its AT-SPI id
/// (so AX calls resolve) and gains `KWin`'s global position when AT-SPI lacks
/// it; windows only `KWin` reports are appended. Matching is by pid and title.
pub(super) fn merge(
	mut atspi: Vec<AtSpiWindow>,
	kwin: Vec<AtSpiWindow>,
	limit: usize,
) -> Vec<AtSpiWindow> {
	for window in kwin {
		let same = |entry: &&mut AtSpiWindow| {
			entry.window.pid == window.window.pid && entry.window.title == window.window.title
		};
		match atspi.iter_mut().find(same) {
			Some(entry) if !entry.position_known => {
				let DesktopWindow { x, y, width, height, .. } = window.window;
				entry.window = DesktopWindow { x, y, width, height, ..entry.window.clone() };
				entry.position_known = true;
			},
			Some(_) => {},
			None => atspi.push(window),
		}
	}
	atspi.truncate(limit);
	atspi
}

#[cfg(test)]
mod tests {
	use super::*;

	fn atspi(id: &str, pid: u32, title: &str, position_known: bool) -> AtSpiWindow {
		AtSpiWindow {
			window: DesktopWindow {
				id:      id.into(),
				title:   title.into(),
				app:     "a".into(),
				pid:     Some(pid),
				x:       0,
				y:       0,
				width:   800,
				height:  600,
				focused: false,
			},
			position_known,
		}
	}

	#[test]
	fn script_reply_maps_to_global_logical_windows() {
		let windows = parse(
			r#"[{"id":"{4917af0b}","title":"Default","app":"so.stencil.tern","pid":69018,
			"x":10.4,"y":20.6,"width":1536,"height":834.4,"focused":true},
			{"id":"{x}","title":"t","app":"a","pid":0,"x":0,"y":0,"width":0.2,"height":5,"focused":false}]"#,
		)
		.expect("valid reply");
		let first = &windows[0].window;
		assert_eq!(first.id, "kwin:{4917af0b}");
		assert_eq!((first.x, first.y, first.width, first.height), (10, 21, 1536, 834));
		assert_eq!(first.pid, Some(69018));
		assert!(first.focused && windows[0].position_known);
		// KWin reports pid 0 for windows without a process; sizes never reach 0.
		assert_eq!((windows[1].window.pid, windows[1].window.width), (None, 1));
	}

	#[test]
	fn script_error_reply_surfaces_as_failure() {
		let Err(err) = parse(r#"{"error":"TypeError: workspace.windowList is not a function"}"#)
		else {
			panic!("a script error must fail");
		};
		assert!(err.message.contains("TypeError"), "{err}");
	}

	#[test]
	fn merge_keeps_atspi_ids_fills_positions_and_appends_kwin_only_windows() {
		let mut known = atspi("atspi:known", 1, "Editor", true);
		known.window.x = 5;
		let merged = merge(
			vec![known, atspi("atspi:unplaced", 2, "Chat", false)],
			vec![
				AtSpiWindow {
					window:         DesktopWindow { x: 99, ..atspi("kwin:a", 1, "Editor", true).window },
					position_known: true,
				},
				AtSpiWindow {
					window:         DesktopWindow {
						x: 300,
						y: 40,
						..atspi("kwin:b", 2, "Chat", true).window
					},
					position_known: true,
				},
				atspi("kwin:c", 3, "Terminal", true),
			],
			48,
		);
		let ids: Vec<_> = merged
			.iter()
			.map(|entry| entry.window.id.as_str())
			.collect();
		assert_eq!(ids, ["atspi:known", "atspi:unplaced", "kwin:c"]);
		// A real AT-SPI position wins; a missing one takes KWin's.
		assert_eq!(merged[0].window.x, 5);
		assert_eq!(
			(merged[1].window.x, merged[1].window.y, merged[1].position_known),
			(300, 40, true)
		);
		assert_eq!(merge(merged, Vec::new(), 2).len(), 2);
	}
}
