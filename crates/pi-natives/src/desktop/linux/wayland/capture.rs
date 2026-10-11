#[cfg(feature = "wayland-pipewire")]
use std::{cell::RefCell, rc::Rc, time::Instant};
use std::{os::fd::OwnedFd, time::Duration};

#[cfg(not(feature = "wayland-pipewire"))]
use ashpd::desktop::screenshot::Screenshot;
use ashpd::desktop::{
	PersistMode, Session,
	screencast::{CursorMode, Screencast, SourceType},
};
use image::RgbaImage;
#[cfg(feature = "wayland-pipewire")]
use pipewire as pw;
#[cfg(feature = "wayland-pipewire")]
use pw::{properties::properties, spa};

use super::portal::{read_token, store_token};
use crate::desktop::{
	control,
	error::{CoreResult, DesktopError},
};

const SCREENCAST_TOKEN: &str = "screencast-token";
const PORTAL_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(feature = "wayland-pipewire")]
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

struct ScreenCast {
	node:     u32,
	fd:       OwnedFd,
	position: Option<(i32, i32)>,
	size:     Option<(i32, i32)>,
	session:  Session<'static, Screencast<'static>>,
}

struct CapturedFrame {
	image:  Option<RgbaImage>,
	width:  u32,
	height: u32,
}

async fn portal_request<T>(request: impl Future<Output = Result<T, String>>) -> CoreResult<T> {
	super::libei::cancellable(tokio::time::timeout(PORTAL_TIMEOUT, request))
		.await?
		.map_err(|_| DesktopError::capture_failed("Wayland ScreenCast portal timed out"))?
		.map_err(DesktopError::capture_failed)
}

async fn open_screencast() -> CoreResult<ScreenCast> {
	let portal = portal_request(async {
		Screencast::new()
			.await
			.map_err(|err| format!("ScreenCast portal: {err}"))
	})
	.await?;
	let session = portal_request(async {
		portal
			.create_session()
			.await
			.map_err(|err| format!("ScreenCast CreateSession: {err}"))
	})
	.await?;
	let opened = portal_request(async {
		let restore_token = read_token(SCREENCAST_TOKEN);
		portal
			.select_sources(
				&session,
				CursorMode::Embedded,
				SourceType::Monitor.into(),
				true,
				restore_token.as_deref(),
				PersistMode::ExplicitlyRevoked,
			)
			.await
			.map_err(|err| format!("ScreenCast SelectSources: {err}"))?;
		let response = portal
			.start(&session, None)
			.await
			.map_err(|err| format!("ScreenCast Start: {err}"))?
			.response()
			.map_err(|err| format!("ScreenCast permission: {err}"))?;
		store_token(SCREENCAST_TOKEN, response.restore_token());
		let stream = response
			.streams()
			.first()
			.ok_or_else(|| "ScreenCast returned no monitor stream".to_string())?;
		let node = stream.pipe_wire_node_id();
		let position = stream.position();
		let size = stream.size();
		let fd = portal
			.open_pipe_wire_remote(&session)
			.await
			.map_err(|err| format!("ScreenCast OpenPipeWireRemote: {err}"))?;
		Ok((node, fd, position, size))
	})
	.await;
	match opened {
		Ok((node, fd, position, size)) => Ok(ScreenCast { node, fd, position, size, session }),
		Err(error) => {
			let _ = tokio::time::timeout(crate::desktop::CLOSE_TIMEOUT, session.close()).await;
			Err(error)
		},
	}
}

/// Whether a Screenshot-portal image can be the `ScreenCast` monitor: the
/// portal captures the whole workspace, so on a multi-monitor layout its
/// aspect ratio differs from the selected monitor's logical size.
#[cfg(any(not(feature = "wayland-pipewire"), test))]
fn screenshot_matches_monitor(width: u32, height: u32, size: Option<(i32, i32)>) -> bool {
	let Some((w, h)) = size.filter(|&(w, h)| w > 0 && h > 0) else {
		return true;
	};
	let (image_cross, monitor_cross) = (u64::from(width) * h as u64, u64::from(height) * w as u64);
	// Allow 1% for per-axis rounding of fractional scales.
	image_cross.abs_diff(monitor_cross) * 100 <= monitor_cross
}

/// Grab a frame through the Screenshot portal, the route available without
/// linking libpipewire. The portal writes a PNG file, which is always removed.
#[cfg(not(feature = "wayland-pipewire"))]
async fn grab_screenshot_frame(
	size: Option<(i32, i32)>,
	pixels: bool,
) -> CoreResult<CapturedFrame> {
	let uri = portal_request(async {
		let response = Screenshot::request()
			.interactive(false)
			.modal(false)
			.send()
			.await
			.map_err(|err| format!("Screenshot portal: {err}"))?
			.response()
			.map_err(|err| format!("Screenshot permission: {err}"))?;
		Ok(response.uri().clone())
	})
	.await?;
	let path = uri
		.to_file_path()
		.map_err(|()| DesktopError::capture_failed(format!("Screenshot portal returned {uri}")))?;
	let read = || -> Result<CapturedFrame, image::ImageError> {
		let reader = image::ImageReader::open(&path)?.with_guessed_format()?;
		if pixels {
			let image = reader.decode()?.into_rgba8();
			let (width, height) = image.dimensions();
			Ok(CapturedFrame { image: Some(image), width, height })
		} else {
			let (width, height) = reader.into_dimensions()?;
			Ok(CapturedFrame { image: None, width, height })
		}
	};
	let frame = read();
	let _ = std::fs::remove_file(&path);
	let frame = frame
		.map_err(|err| DesktopError::capture_failed(format!("Screenshot portal image: {err}")))?;
	if !screenshot_matches_monitor(frame.width, frame.height, size) {
		return Err(DesktopError::capture_failed(
			"the Screenshot portal captured a multi-monitor workspace; per-monitor Wayland capture \
			 requires the wayland-pipewire feature",
		));
	}
	Ok(frame)
}

#[cfg(feature = "wayland-pipewire")]
struct UserData {
	format: spa::param::video::VideoInfoRaw,
}

#[cfg(feature = "wayland-pipewire")]
fn rgba_from_buffer(
	format: &spa::param::video::VideoInfoRaw,
	data: &mut pw::spa::buffer::Data,
) -> Result<RgbaImage, String> {
	let size = format.size();
	let width = size.width;
	let height = size.height;
	if width == 0 || height == 0 {
		return Err("PipeWire negotiated an empty frame".to_string());
	}
	let chunk = data.chunk();
	let offset = chunk.offset() as usize;
	let bytes = chunk.size() as usize;
	let stride = chunk.stride();
	if stride <= 0 {
		return Err(format!("PipeWire returned unsupported frame stride {stride}"));
	}
	let stride = stride as usize;
	let source = data
		.data()
		.ok_or_else(|| "PipeWire frame buffer is not memory-mapped".to_string())?;
	let end = offset
		.checked_add(bytes)
		.ok_or_else(|| "PipeWire frame size overflow".to_string())?
		.min(source.len());
	let source = source
		.get(offset..end)
		.ok_or_else(|| "PipeWire frame offset is outside the mapped buffer".to_string())?;
	let pixel_size = match format.format() {
		spa::param::video::VideoFormat::RGB | spa::param::video::VideoFormat::BGR => 3,
		spa::param::video::VideoFormat::RGBA
		| spa::param::video::VideoFormat::RGBx
		| spa::param::video::VideoFormat::BGRA
		| spa::param::video::VideoFormat::BGRx => 4,
		other => return Err(format!("PipeWire negotiated unsupported pixel format {other:?}")),
	};
	let row_bytes = (width as usize)
		.checked_mul(pixel_size)
		.ok_or_else(|| "PipeWire row size overflow".to_string())?;
	if stride < row_bytes || source.len() < stride.saturating_mul(height as usize) {
		return Err(format!(
			"PipeWire frame buffer is short: {} bytes for {width}x{height} stride {stride}",
			source.len()
		));
	}
	let mut rgba = vec![
		0;
		(width as usize)
			.saturating_mul(height as usize)
			.saturating_mul(4)
	];
	for y in 0..height as usize {
		control::check().map_err(|error| error.to_string())?;
		let row = &source[y * stride..y * stride + row_bytes];
		for x in 0..width as usize {
			let input = &row[x * pixel_size..];
			let output = &mut rgba[(y * width as usize + x) * 4..];
			match format.format() {
				spa::param::video::VideoFormat::RGB
				| spa::param::video::VideoFormat::RGBA
				| spa::param::video::VideoFormat::RGBx => {
					output[..4].copy_from_slice(&[
						input[0],
						input[1],
						input[2],
						if pixel_size == 4 && format.format() == spa::param::video::VideoFormat::RGBA {
							input[3]
						} else {
							255
						},
					]);
				},
				_ => output[..4].copy_from_slice(&[
					input[2],
					input[1],
					input[0],
					if pixel_size == 4 && format.format() == spa::param::video::VideoFormat::BGRA {
						input[3]
					} else {
						255
					},
				]),
			}
		}
	}
	RgbaImage::from_raw(width, height, rgba)
		.ok_or_else(|| "failed to construct PipeWire RGBA frame".to_string())
}

#[cfg(feature = "wayland-pipewire")]
fn grab_pipewire_frame(node: u32, fd: OwnedFd, pixels: bool) -> CoreResult<CapturedFrame> {
	control::check()?;
	pw::init();
	let mainloop = pw::main_loop::MainLoopRc::new(None)
		.map_err(|err| DesktopError::capture_failed(format!("PipeWire main loop: {err}")))?;
	let context = pw::context::ContextRc::new(&mainloop, None)
		.map_err(|err| DesktopError::capture_failed(format!("PipeWire context: {err}")))?;
	let core = context
		.connect_fd_rc(fd, None)
		.map_err(|err| DesktopError::capture_failed(format!("PipeWire remote: {err}")))?;
	let stream = pw::stream::StreamBox::new(&core, "omp-computer-capture", properties! {
		*pw::keys::MEDIA_TYPE => "Video",
		*pw::keys::MEDIA_CATEGORY => "Capture",
		*pw::keys::MEDIA_ROLE => "Screen",
	})
	.map_err(|err| DesktopError::capture_failed(format!("PipeWire stream: {err}")))?;
	let result: Rc<RefCell<Option<CoreResult<CapturedFrame>>>> = Rc::new(RefCell::new(None));
	let callback_result = Rc::clone(&result);
	let _listener = stream
		.add_local_listener_with_user_data(UserData { format: Default::default() })
		.param_changed(|_, user, id, param| {
			let Some(param) = param else {
				return;
			};
			if id == spa::param::ParamType::Format.as_raw() {
				let _ = user.format.parse(param);
			}
		})
		.process(move |stream, user| {
			if callback_result.borrow().is_some() {
				return;
			}
			let Some(mut buffer) = stream.dequeue_buffer() else {
				return;
			};
			let Some(data) = buffer.datas_mut().first_mut() else {
				return;
			};
			let size = user.format.size();
			let frame = if size.width == 0 || size.height == 0 {
				Err(DesktopError::capture_failed("PipeWire negotiated an empty frame"))
			} else if pixels {
				rgba_from_buffer(&user.format, data)
					.map(|image| CapturedFrame {
						image:  Some(image),
						width:  size.width,
						height: size.height,
					})
					.map_err(DesktopError::capture_failed)
			} else {
				// Validation needs the negotiated pixel extent, not a copy or
				// conversion of the compositor's image buffer.
				Ok(CapturedFrame { image: None, width: size.width, height: size.height })
			};
			*callback_result.borrow_mut() = Some(frame);
		})
		.register()
		.map_err(|err| DesktopError::capture_failed(format!("PipeWire listener: {err}")))?;
	let object = spa::pod::object!(
		spa::utils::SpaTypes::ObjectParamFormat,
		spa::param::ParamType::EnumFormat,
		spa::pod::property!(
			spa::param::format::FormatProperties::MediaType,
			Id,
			spa::param::format::MediaType::Video
		),
		spa::pod::property!(
			spa::param::format::FormatProperties::MediaSubtype,
			Id,
			spa::param::format::MediaSubtype::Raw
		),
		spa::pod::property!(
			spa::param::format::FormatProperties::VideoFormat,
			Choice,
			Enum,
			Id,
			spa::param::video::VideoFormat::BGRx,
			spa::param::video::VideoFormat::BGRx,
			spa::param::video::VideoFormat::BGRA,
			spa::param::video::VideoFormat::RGBx,
			spa::param::video::VideoFormat::RGBA,
			spa::param::video::VideoFormat::RGB,
			spa::param::video::VideoFormat::BGR
		),
		spa::pod::property!(
			spa::param::format::FormatProperties::VideoSize,
			Choice,
			Range,
			Rectangle,
			spa::utils::Rectangle { width: 1920, height: 1080 },
			spa::utils::Rectangle { width: 1, height: 1 },
			spa::utils::Rectangle { width: 16384, height: 16384 }
		)
	);
	let values = spa::pod::serialize::PodSerializer::serialize(
		std::io::Cursor::new(Vec::new()),
		&spa::pod::Value::Object(object),
	)
	.map_err(|err| DesktopError::capture_failed(format!("PipeWire format serialization: {err}")))?
	.0
	.into_inner();
	let param = spa::pod::Pod::from_bytes(&values)
		.ok_or_else(|| DesktopError::capture_failed("PipeWire rejected format parameters"))?;
	stream
		.connect(
			spa::utils::Direction::Input,
			Some(node),
			pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
			&mut [param],
		)
		.map_err(|err| DesktopError::capture_failed(format!("PipeWire connect: {err}")))?;
	let deadline = Instant::now() + FRAME_TIMEOUT;
	loop {
		control::check()?;
		if let Some(result) = result.borrow_mut().take() {
			return result;
		}
		if Instant::now() >= deadline {
			return Err(DesktopError::capture_failed("PipeWire frame timed out"));
		}
		if mainloop.loop_().iterate(Duration::from_millis(10)) < 0 {
			return Err(DesktopError::capture_failed("PipeWire loop failed before producing a frame"));
		}
	}
}

fn capture_stream(pixels: bool) -> CoreResult<(CapturedFrame, super::PortalGeometry)> {
	control::check()?;
	let runtime = super::portal::portal_runtime()?;
	let ScreenCast { node, fd, position, size, session } = runtime.block_on(open_screencast())?;
	#[cfg(feature = "wayland-pipewire")]
	let captured = grab_pipewire_frame(node, fd, pixels);
	#[cfg(not(feature = "wayland-pipewire"))]
	let captured = {
		drop((node, fd));
		runtime.block_on(grab_screenshot_frame(size, pixels))
	};
	let _ = runtime.block_on(async {
		tokio::time::timeout(crate::desktop::CLOSE_TIMEOUT, session.close()).await
	});
	control::check()?;
	let captured = captured?;
	let geometry = super::PortalGeometry::new(position, size, captured.width, captured.height);
	Ok((captured, geometry))
}

pub(super) fn capture() -> CoreResult<(RgbaImage, super::PortalGeometry)> {
	let (frame, geometry) = capture_stream(true)?;
	let image = frame
		.image
		.ok_or_else(|| DesktopError::internal("PipeWire capture omitted pixels"))?;
	Ok((image, geometry))
}

pub(super) fn geometry() -> CoreResult<super::PortalGeometry> {
	capture_stream(false).map(|(_, geometry)| geometry)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn cancelled_geometry_refresh_never_opens_a_portal_session() {
		let source = control::CancellationSource::default();
		let token = source.token();
		source.cancel();
		let result = control::with_token_for_test(&token, geometry);
		assert_eq!(result.unwrap_err().code, token.check().unwrap_err().code);
	}

	#[test]
	fn screenshot_must_cover_only_the_screencast_monitor() {
		// Scale-1.25 single monitor: 2400x1350 physical pixels for 1920x1080
		// logical.
		assert!(screenshot_matches_monitor(2400, 1350, Some((1920, 1080))));
		// Two 1920x1080 monitors side by side: the workspace shot is 3840x1080.
		assert!(!screenshot_matches_monitor(3840, 1080, Some((1920, 1080))));
		// No logical size from the portal: nothing to compare against.
		assert!(screenshot_matches_monitor(3840, 1080, None));
	}
}
