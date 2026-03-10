//! Wayland DnD (drag and drop) support via wl_data_device.
//!
//! Integrates SCTK's `DataDeviceManagerState` into winit-wayland to emit
//! `WindowEvent::DroppedFile`, `HoveredFile`, and `HoveredFileCancelled`.

use std::io::Read;
use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;

use sctk::data_device_manager::WritePipe;
use sctk::data_device_manager::data_device::{DataDeviceData, DataDeviceHandler};
use sctk::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use sctk::data_device_manager::data_source::DataSourceHandler;
use sctk::reexports::client::protocol::wl_data_device::WlDataDevice;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction;
use sctk::reexports::client::protocol::wl_data_source::WlDataSource;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Proxy, QueueHandle};
use tracing::{debug, warn};

use crate::dpi::PhysicalPosition;
use crate::event::WindowEvent;
use crate::platform_impl::wayland::make_wid;
use crate::platform_impl::wayland::state::WinitState;

fn dnd_device_id() -> crate::event::DeviceId {
    crate::event::DeviceId(crate::platform_impl::DeviceId::Wayland(
        crate::platform_impl::linux::wayland::DeviceId,
    ))
}

/// Parse a `text/uri-list` string into file paths (or URL pseudo-paths for http/https).
fn parse_uri_list(data: &str) -> Vec<PathBuf> {
    data.lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .filter_map(|line| {
            let line = line.trim();
            if let Some(path_str) =
                line.strip_prefix("file://localhost").or_else(|| line.strip_prefix("file://"))
            {
                Some(PathBuf::from(percent_decode(path_str)))
            } else if line.starts_with("http://") || line.starts_with("https://") {
                // Pass HTTP/HTTPS URLs through as pseudo-paths for the application to handle
                Some(PathBuf::from(line))
            } else {
                None
            }
        })
        .collect()
}

/// Decode text/x-moz-url data. Firefox sends UTF-16LE, Chrome sends UTF-8.
fn decode_moz_url(data: &[u8]) -> String {
    // Check for UTF-16LE BOM or if odd bytes are mostly zero (UTF-16LE ASCII)
    let is_utf16 = data.starts_with(&[0xFF, 0xFE])
        || (data.len() >= 4 && data[1] == 0 && data[3] == 0);
    if is_utf16 {
        let start = if data.starts_with(&[0xFF, 0xFE]) { 2 } else { 0 };
        let u16s: Vec<u16> = data[start..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16_lossy(&u16s)
    } else {
        String::from_utf8_lossy(data).into_owned()
    }
}

/// Simple percent-decoding for file paths.
fn percent_decode(input: &str) -> String {
    let mut output = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                output.push(byte);
                i += 3;
                continue;
            }
        }
        output.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(output).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Read file paths from a DnD offer using the best available MIME type.
fn read_paths_from_offer(conn: &Connection, offer: &DragOffer) -> Vec<PathBuf> {
    let mime = offer.with_mime_types(|mimes: &[String]| {
        WinitState::pick_best_mime(mimes).map(|s| s.to_string())
    });

    let Some(mime) = mime else {
        return Vec::new();
    };

    debug!("DnD reading MIME type: {mime}");

    let read_pipe = match offer.receive(mime.clone()) {
        Ok(pipe) => pipe,
        Err(e) => {
            warn!("Failed to receive {mime}: {e}");
            return Vec::new();
        },
    };

    let owned_fd = match read_pipe.as_fd().try_clone_to_owned() {
        Ok(fd) => fd,
        Err(e) => {
            warn!("Failed to dup DnD pipe fd: {e}");
            return Vec::new();
        },
    };

    drop(read_pipe);
    let _ = conn.flush();

    let handle = std::thread::spawn(move || read_from_fd(owned_fd));

    match handle.join() {
        Ok(data) if !data.is_empty() => {
            if mime == "text/x-moz-url" {
                // text/x-moz-url: "URL\nTitle\n" — UTF-16LE (Firefox) or UTF-8 (Chrome)
                let text = decode_moz_url(&data);
                debug!("DnD text/x-moz-url decoded: {text}");
                parse_uri_list(&text)
            } else if mime == "text/uri-list" {
                let text = String::from_utf8_lossy(&data);
                parse_uri_list(&text)
            } else {
                // For text/plain, try to parse as URI list (single URL per line)
                let text = String::from_utf8_lossy(&data);
                let text = text.trim();
                if text.starts_with("http://") || text.starts_with("https://")
                    || text.starts_with("file://")
                {
                    parse_uri_list(text)
                } else {
                    debug!("DnD text/plain is not a URL: {text}");
                    Vec::new()
                }
            }
        },
        Ok(_) => Vec::new(),
        Err(_) => {
            warn!("DnD read thread panicked");
            Vec::new()
        },
    }
}

fn read_from_fd(fd: OwnedFd) -> Vec<u8> {
    let mut file = std::fs::File::from(fd);
    let mut data = Vec::new();
    match file.read_to_end(&mut data) {
        Ok(_) => data,
        Err(e) => {
            warn!("Failed to read DnD data: {e}");
            Vec::new()
        },
    }
}

impl WinitState {
    /// Pick the best MIME type from a DnD offer for file/URL handling.
    /// Preference: text/uri-list > text/x-moz-url > text/plain.
    fn pick_best_mime(mimes: &[String]) -> Option<&str> {
        for preferred in &["text/uri-list", "text/x-moz-url", "text/plain"] {
            if let Some(m) = mimes.iter().find(|m| m.as_str() == *preferred) {
                return Some(m.as_str());
            }
        }
        None
    }
}

impl DataDeviceHandler for WinitState {
    fn enter(
        &mut self,
        conn: &Connection,
        _qh: &QueueHandle<Self>,
        wl_data_device: &WlDataDevice,
        x: f64,
        y: f64,
        wl_surface: &WlSurface,
    ) {
        let window_id = make_wid(wl_surface);
        debug!("DnD enter on window {window_id:?}");

        let drag_offer: Option<DragOffer> = wl_data_device
            .data::<DataDeviceData>()
            .and_then(|data: &DataDeviceData| data.drag_offer());

        if let Some(ref offer) = drag_offer {
            offer.with_mime_types(|mimes: &[String]| {
                debug!("DnD offered MIME types: {mimes:?}");
            });
            offer.set_actions(DndAction::Copy | DndAction::Move, DndAction::Copy);

            // Accept the best available MIME type
            let accepted = offer.with_mime_types(|mimes: &[String]| {
                Self::pick_best_mime(mimes).map(|s| s.to_string())
            });
            if let Some(ref mime) = accepted {
                offer.accept_mime_type(offer.serial, Some(mime.clone()));
            }
        }

        let _ = conn.flush();

        let has_files = drag_offer.as_ref().is_some_and(|offer: &DragOffer| {
            offer.with_mime_types(|mimes: &[String]| {
                Self::pick_best_mime(mimes).is_some()
            })
        });
        self.dnd_offer = drag_offer;
        self.dnd_window = Some(window_id);

        if has_files {
            self.events_sink.push_window_event(
                WindowEvent::HoveredFile(PathBuf::from("(dragging)")),
                window_id,
            );
        }

        // Emit cursor position so the application knows where the drag is
        self.events_sink.push_window_event(
            WindowEvent::CursorMoved {
                device_id: dnd_device_id(),
                position: PhysicalPosition::new(x, y),
            },
            window_id,
        );
    }

    fn leave(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _data_device: &WlDataDevice) {
        debug!("DnD leave");
        if let Some(window_id) = self.dnd_window.take() {
            self.events_sink.push_window_event(
                WindowEvent::HoveredFileCancelled,
                window_id,
            );
        }
        self.dnd_offer = None;
    }

    fn motion(
        &mut self,
        conn: &Connection,
        _qh: &QueueHandle<Self>,
        _data_device: &WlDataDevice,
        x: f64,
        y: f64,
    ) {
        if let Some(ref offer) = self.dnd_offer {
            let accepted = offer.with_mime_types(|mimes: &[String]| {
                Self::pick_best_mime(mimes).map(|s| s.to_string())
            });
            if let Some(mime) = accepted {
                offer.accept_mime_type(offer.serial, Some(mime));
                let _ = conn.flush();
            }
        }

        // Update cursor position during DnD so the app can track drop location
        if let Some(window_id) = self.dnd_window {
            self.events_sink.push_window_event(
                WindowEvent::CursorMoved {
                    device_id: dnd_device_id(),
                    position: PhysicalPosition::new(x, y),
                },
                window_id,
            );
        }
    }

    fn selection(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _data_device: &WlDataDevice,
    ) {
    }

    fn drop_performed(
        &mut self,
        conn: &Connection,
        _qh: &QueueHandle<Self>,
        wl_data_device: &WlDataDevice,
    ) {
        debug!("DnD drop performed");
        let Some(window_id) = self.dnd_window.take() else {
            return;
        };

        let offer: Option<DragOffer> = wl_data_device
            .data::<DataDeviceData>()
            .and_then(|data: &DataDeviceData| data.drag_offer())
            .or_else(|| self.dnd_offer.take());

        if let Some(offer) = offer {
            let paths = read_paths_from_offer(conn, &offer);

            offer.finish();
            offer.destroy();

            for path in paths {
                self.events_sink.push_window_event(
                    WindowEvent::DroppedFile(path),
                    window_id,
                );
            }
        }

        self.dnd_offer = None;
    }
}

impl DataOfferHandler for WinitState {
    fn source_actions(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _offer: &mut DragOffer,
        actions: DndAction,
    ) {
        debug!("DnD source_actions: {actions:?}");
    }

    fn selected_action(
        &mut self,
        conn: &Connection,
        _qh: &QueueHandle<Self>,
        offer: &mut DragOffer,
        actions: DndAction,
    ) {
        debug!("DnD selected_action: {actions:?}");
        if !actions.is_empty() {
            let accepted = offer.with_mime_types(|mimes: &[String]| {
                Self::pick_best_mime(mimes).map(|s| s.to_string())
            });
            if let Some(mime) = accepted {
                offer.accept_mime_type(offer.serial, Some(mime));
                let _ = conn.flush();
            }
        }
    }
}

impl DataSourceHandler for WinitState {
    fn accept_mime(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _source: &WlDataSource,
        _mime: Option<String>,
    ) {
    }

    fn send_request(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _source: &WlDataSource,
        _mime: String,
        _fd: WritePipe,
    ) {
    }

    fn cancelled(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _source: &WlDataSource) {}

    fn dnd_dropped(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _source: &WlDataSource) {
    }

    fn dnd_finished(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _source: &WlDataSource,
    ) {
    }

    fn action(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _source: &WlDataSource,
        _action: DndAction,
    ) {
    }
}

sctk::delegate_data_device!(WinitState);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_uri_list() {
        let input = "file:///home/user/photo.jpg\r\nfile:///tmp/hello%20world.txt\r\n# comment\r\n";
        let paths = parse_uri_list(input);
        assert_eq!(paths, vec![
            PathBuf::from("/home/user/photo.jpg"),
            PathBuf::from("/tmp/hello world.txt"),
        ]);
    }

    #[test]
    fn test_parse_uri_list_localhost() {
        let input = "file://localhost/home/user/doc.pdf\n";
        let paths = parse_uri_list(input);
        assert_eq!(paths, vec![PathBuf::from("/home/user/doc.pdf")]);
    }

    #[test]
    fn test_parse_uri_list_http() {
        let input = "https://example.com/image.png\r\n";
        let paths = parse_uri_list(input);
        assert_eq!(paths, vec![PathBuf::from("https://example.com/image.png")]);
    }

    #[test]
    fn test_parse_uri_list_mixed() {
        let input = "file:///home/user/photo.jpg\r\nhttps://example.com/img.png\r\n";
        let paths = parse_uri_list(input);
        assert_eq!(paths, vec![
            PathBuf::from("/home/user/photo.jpg"),
            PathBuf::from("https://example.com/img.png"),
        ]);
    }

    #[test]
    fn test_percent_decode() {
        assert_eq!(percent_decode("/path/hello%20world"), "/path/hello world");
        assert_eq!(percent_decode("/path/%E4%B8%AD%E6%96%87"), "/path/中文");
        assert_eq!(percent_decode("/simple/path"), "/simple/path");
    }
}
