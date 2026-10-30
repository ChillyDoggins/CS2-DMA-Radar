//! Tiny HTTP server for the browser radar.
//!   GET /              -> radar page (src/radar.html, built into the exe)
//!   GET /players.json  -> latest snapshot from the game loop
//!   GET /maps.json     -> radar calibration for every map in assets/overviews
//!   GET /assets/...    -> map images, icons and overview files (assets/, built in by build.rs)

use crate::overview;
use anyhow::{Result, anyhow};
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Response, Server};

const PAGE: &str = include_str!("radar.html");
const ASSETS: &[(&str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/assets.rs"));

/// Latest players JSON, written by the game loop and read by the server.
pub type Snapshot = Arc<Mutex<String>>;

/// Starts serving in a background thread. Returns how many maps have a radar.
pub fn start(addr: &str, snapshot: Snapshot) -> Result<usize> {
    let server = Server::http(addr).map_err(|e| anyhow!("{e}"))?;
    let (maps, map_count) = overview::maps_json(ASSETS);
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let url = req.url().split('?').next().unwrap_or("");
            let (body, content_type, status): (Vec<u8>, &str, u16) = match url {
                "/" | "/index.html" => (PAGE.into(), "text/html; charset=utf-8", 200),
                "/players.json" => (snapshot.lock().unwrap().clone().into(), "application/json", 200),
                "/maps.json" => (maps.clone().into(), "application/json", 200),
                _ => match ASSETS.iter().find(|(p, _)| *p == url) {
                    Some((p, data)) => {
                        let ty = if p.ends_with(".png") { "image/png" } else { "text/plain; charset=utf-8" };
                        (data.to_vec(), ty, 200)
                    }
                    None => (b"not found".to_vec(), "text/plain", 404),
                },
            };
            let header = Header::from_bytes("Content-Type", content_type).unwrap();
            let cache = if url.starts_with("/assets/") { "max-age=3600" } else { "no-store" };
            let cache = Header::from_bytes("Cache-Control", cache).unwrap();
            let _ = req.respond(
                Response::from_data(body)
                    .with_status_code(status)
                    .with_header(header)
                    .with_header(cache),
            );
        }
    });
    Ok(map_count)
}
