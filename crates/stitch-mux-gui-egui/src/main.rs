//! Standalone entry point for the egui reference adapter.

use std::error::Error;

use stitch_mux_protocol::Endpoint;

fn main() -> Result<(), Box<dyn Error>> {
    let session = std::env::args().nth(1);
    let endpoint = match std::env::var("STITCH_MUX_SOCKET") {
        Ok(value) => Endpoint::from_override(&value),
        Err(_) => Endpoint::default_endpoint()?,
    };
    stitch_mux_gui_egui::run(endpoint, session)
}
