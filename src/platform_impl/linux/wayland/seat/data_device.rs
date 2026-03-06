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

use crate::event::WindowEvent;
use crate::platform_impl::wayland::make_wid;
use crate::platform_impl::wayland::state::WinitState;

/// Parse a `text/uri-list` string into file paths.
fn parse_uri_list(data: &str) -> Vec<PathBuf> {
    data.lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .filter_map(|line| {
            let line = line.trim();
            let path_str =
                line.strip_prefix("file://localhost").or_else(|| line.strip_prefix("file://"))?;
            Some(PathBuf::from(percent_decode(path_str)))
        })
        .collect()
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

/// Read file paths from a DnD offer's `text/uri-list` MIME type.
fn read_paths_from_offer(conn: &Connection, offer: &DragOffer) -> Vec<PathBuf> {
    let has_uri_list =
        offer.with_mime_types(|mimes: &[String]| mimes.iter().any(|m| m == "text/uri-list"));

    if !has_uri_list {
        return Vec::new();
    }

    let read_pipe = match offer.receive("text/uri-list".to_string()) {
        Ok(pipe) => pipe,
        Err(e) => {
            warn!("Failed to receive text/uri-list: {e}");
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
            let text = String::from_utf8_lossy(&data);
            parse_uri_list(&text)
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

impl DataDeviceHandler for WinitState {
    fn enter(
        &mut self,
        conn: &Connection,
        _qh: &QueueHandle<Self>,
        wl_data_device: &WlDataDevice,
        _x: f64,
        _y: f64,
        wl_surface: &WlSurface,
    ) {
        let window_id = make_wid(wl_surface);
        debug!("DnD enter on window {window_id:?}");

        let drag_offer: Option<DragOffer> = wl_data_device
            .data::<DataDeviceData>()
            .and_then(|data: &DataDeviceData| data.drag_offer());

        if let Some(ref offer) = drag_offer {
            offer.set_actions(DndAction::Copy | DndAction::Move, DndAction::Copy);
            offer.accept_mime_type(offer.serial, Some("text/uri-list".to_string()));
        }

        let _ = conn.flush();

        let has_files = drag_offer.as_ref().is_some_and(|offer: &DragOffer| {
            offer.with_mime_types(|mimes: &[String]| mimes.iter().any(|m| m == "text/uri-list"))
        });
        self.dnd_offer = drag_offer;
        self.dnd_window = Some(window_id);

        if has_files {
            self.events_sink.push_window_event(
                WindowEvent::HoveredFile(PathBuf::from("(dragging)")),
                window_id,
            );
        }
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
        _x: f64,
        _y: f64,
    ) {
        if let Some(ref offer) = self.dnd_offer {
            offer.accept_mime_type(offer.serial, Some("text/uri-list".to_string()));
            let _ = conn.flush();
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
            offer.accept_mime_type(offer.serial, Some("text/uri-list".to_string()));
            let _ = conn.flush();
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
    fn test_percent_decode() {
        assert_eq!(percent_decode("/path/hello%20world"), "/path/hello world");
        assert_eq!(percent_decode("/path/%E4%B8%AD%E6%96%87"), "/path/中文");
        assert_eq!(percent_decode("/simple/path"), "/simple/path");
    }
}
