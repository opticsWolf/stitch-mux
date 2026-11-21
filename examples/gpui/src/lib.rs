//! Optional GPUI adapter spike.
//!
//! The `gpui` feature exists to prove that a second native toolkit can consume
//! the same toolkit-independent `SessionView` without changing the protocol or
//! the daemon. It deliberately does not pull GPUI into the default workspace:
//! GPUI is pre-1.0 and a release dependency needs a pinned version, which is a
//! separate product decision. See `docs/GPUI-SPIKE.md`.

use stitch_mux::client::SessionView;
use stitch_mux::core::Rect;

pub const ADAPTER: &str = "gpui";

/// The minimal projection any GPUI window needs from the shared client state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SpikeFrame {
    pub cols: u16,
    pub rows: u16,
    pub focus: Option<u64>,
    pub panes: Vec<(u64, Rect)>,
}

/// Build a GPUI-independent frame from the same state egui and Qt consume.
pub fn from_view(view: &SessionView) -> SpikeFrame {
    let panes = view
        .layout
        .as_ref()
        .map(|layout| {
            layout
                .panes
                .iter()
                .map(|geometry| (geometry.pane_id, geometry.content))
                .collect()
        })
        .unwrap_or_default();
    SpikeFrame {
        cols: view.size.cols,
        rows: view.size.rows,
        focus: view.focus,
        panes,
    }
}

/// A feature-gated hook for the future GPUI window title.
#[cfg(feature = "gpui")]
pub fn window_title(view: &SessionView) -> String {
    if view.name.is_empty() {
        "stitch-mux".to_owned()
    } else {
        format!("stitch-mux — {}", view.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spike_uses_the_shared_client_state() {
        let view = SessionView {
            size: stitch_mux::core::Size::new(90, 30),
            focus: Some(7),
            ..Default::default()
        };
        let frame = from_view(&view);
        assert_eq!((frame.cols, frame.rows), (90, 30));
        assert_eq!(frame.focus, Some(7));
        assert!(frame.panes.is_empty());
    }
}
