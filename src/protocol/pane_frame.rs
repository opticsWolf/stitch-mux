//! Flat structured pane frames.
//!
//! `PaneDelta` is convenient and version-tolerant, but every cell is encoded
//! as a separate MessagePack map and every glyph/link owns a `String`. This
//! module adds an opt-in frame layout with fixed-size cells, row records, and
//! interned sideband tables. The legacy delta frame stays available for older
//! clients.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::core::{Attrs, Cell, Color, Cursor, Modes};

use super::{
    Frame, FrameError, MessageKind, Notification, ProcessInfo, Progress, ShellMarks,
};

/// Fixed-width encoded cell. Twenty bytes, no padding read by the codec.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct PaneWireCell {
    /// Unicode codepoint, or `CLUSTER_FLAG | cluster index`.
    pub ch: u32,
    pub fg: u32,
    pub bg: u32,
    pub attrs: u16,
    pub width: u8,
    pub link: u32,
}

pub const CELL_LEN: usize = 20;
const CLUSTER_FLAG: u32 = 1 << 31;
const ROW_RECORD_LEN: usize = 8;

impl PaneWireCell {
    pub fn to_bytes(self, out: &mut [u8; CELL_LEN]) {
        out[0..4].copy_from_slice(&self.ch.to_le_bytes());
        out[4..8].copy_from_slice(&self.fg.to_le_bytes());
        out[8..12].copy_from_slice(&self.bg.to_le_bytes());
        out[12..14].copy_from_slice(&self.attrs.to_le_bytes());
        out[14] = self.width;
        out[15] = 0; // explicit padding byte; keep the codec deterministic
        out[16..20].copy_from_slice(&self.link.to_le_bytes());
    }

    pub fn from_bytes(bytes: &[u8; CELL_LEN]) -> Self {
        Self {
            ch: u32::from_le_bytes(bytes[0..4].try_into().expect("four bytes")),
            fg: u32::from_le_bytes(bytes[4..8].try_into().expect("four bytes")),
            bg: u32::from_le_bytes(bytes[8..12].try_into().expect("four bytes")),
            attrs: u16::from_le_bytes(bytes[12..14].try_into().expect("two bytes")),
            width: bytes[14],
            link: u32::from_le_bytes(bytes[16..20].try_into().expect("four bytes")),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct PaneRowRecord {
    pub row: u16,
    pub x0: u16,
    pub len: u16,
    pub flags: u16,
}

impl PaneRowRecord {
    fn to_bytes(self, out: &mut [u8; ROW_RECORD_LEN]) {
        out[0..2].copy_from_slice(&self.row.to_le_bytes());
        out[2..4].copy_from_slice(&self.x0.to_le_bytes());
        out[4..6].copy_from_slice(&self.len.to_le_bytes());
        out[6..8].copy_from_slice(&self.flags.to_le_bytes());
    }

    fn from_bytes(bytes: &[u8; ROW_RECORD_LEN]) -> Self {
        Self {
            row: u16::from_le_bytes(bytes[0..2].try_into().expect("two bytes")),
            x0: u16::from_le_bytes(bytes[2..4].try_into().expect("two bytes")),
            len: u16::from_le_bytes(bytes[4..6].try_into().expect("two bytes")),
            flags: u16::from_le_bytes(bytes[6..8].try_into().expect("two bytes")),
        }
    }
}

/// Metadata carried once per structured frame.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PaneFrameMeta {
    pub pane_id: u64,
    pub seq: u64,
    pub cursor: Cursor,
    pub modes: Modes,
    pub title: String,
    pub cwd: Option<String>,
    pub foreground: Option<ProcessInfo>,
    pub shell_marks: Option<ShellMarks>,
    pub program_status: Option<crate::core::ProgramStatus>,
    pub notification: Option<Notification>,
    pub progress: Option<Progress>,
    pub alt_screen: bool,
    pub scrollback_len: u64,
    pub total_lines: u64,
    pub scrollback_bytes: u64,
    pub bell: bool,
}

impl PaneFrameMeta {
    pub fn from_delta(delta: &super::PaneDelta) -> Self {
        Self {
            pane_id: delta.pane_id,
            seq: delta.seq,
            cursor: delta.cursor,
            modes: delta.modes,
            title: delta.title.clone(),
            cwd: delta.cwd.clone(),
            foreground: delta.foreground.clone(),
            shell_marks: delta.shell_marks.clone(),
            program_status: delta.program_status.clone(),
            notification: delta.notification.clone(),
            progress: delta.progress.clone(),
            alt_screen: delta.alt_screen,
            scrollback_len: delta.scrollback_len,
            total_lines: delta.total_lines,
            scrollback_bytes: delta.scrollback_bytes,
            bell: delta.bell,
        }
    }

    pub fn into_delta(self, rows: Vec<super::RowUpdate>) -> super::PaneDelta {
        super::PaneDelta {
            pane_id: self.pane_id,
            seq: self.seq,
            rows,
            cursor: self.cursor,
            modes: self.modes,
            title: self.title,
            cwd: self.cwd,
            foreground: self.foreground,
            shell_marks: self.shell_marks,
            program_status: self.program_status,
            notification: self.notification,
            progress: self.progress,
            alt_screen: self.alt_screen,
            scrollback_len: self.scrollback_len,
            total_lines: self.total_lines,
            scrollback_bytes: self.scrollback_bytes,
            bell: self.bell,
        }
    }
}

/// Encoded structured frame, ready to be serialized into a protocol frame.
#[derive(Clone, Debug, Default)]
pub struct StructuredPaneFrame {
    pub meta: Option<PaneFrameMeta>,
    pub records: Vec<PaneRowRecord>,
    pub cells: Vec<PaneWireCell>,
    pub clusters: Vec<String>,
    pub links: Vec<String>,
}

impl StructuredPaneFrame {
    /// Converts a legacy delta into a flat frame, interning clusters and links.
    pub fn from_delta(delta: &super::PaneDelta) -> Self {
        let meta = PaneFrameMeta::from_delta(delta);
        let mut clusters = Vec::new();
        let mut cluster_ids: HashMap<String, u32> = HashMap::new();
        let mut links = Vec::new();
        let mut link_ids: HashMap<String, u32> = HashMap::new();
        let mut records = Vec::with_capacity(delta.rows.len());
        let mut cells = Vec::new();

        for update in &delta.rows {
            let start = cells.len();
            for cell in &update.cells {
                cells.push(wire_cell(
                    cell,
                    &mut clusters,
                    &mut cluster_ids,
                    &mut links,
                    &mut link_ids,
                ));
            }
            records.push(PaneRowRecord {
                row: update.row,
                x0: 0,
                len: cells.len().saturating_sub(start).min(u16::MAX as usize) as u16,
                flags: u16::from(update.wrapped),
            });
        }

        Self {
            meta: Some(meta),
            records,
            cells,
            clusters,
            links,
        }
    }

    /// Serializes into a `Frame` payload.
    pub fn encode(self) -> Result<Frame, FrameError> {
        let meta = self.meta.ok_or_else(|| FrameError::Decode("missing pane frame metadata".into()))?;
        let meta_bytes = rmp_serde::to_vec(&meta)
            .map_err(|error| FrameError::Encode(error.to_string()))?;

        let mut record_bytes = vec![0_u8; self.records.len() * ROW_RECORD_LEN];
        for (slot, record) in record_bytes
            .chunks_exact_mut(ROW_RECORD_LEN)
            .zip(&self.records)
        {
            record.to_bytes(slot.try_into().expect("row record slot"));
        }

        let mut cell_bytes = vec![0_u8; self.cells.len() * CELL_LEN];
        for (slot, cell) in cell_bytes.chunks_exact_mut(CELL_LEN).zip(&self.cells) {
            cell.to_bytes(slot.try_into().expect("cell slot"));
        }

        let cluster_bytes = rmp_serde::to_vec(&self.clusters)
            .map_err(|error| FrameError::Encode(error.to_string()))?;
        let link_bytes = rmp_serde::to_vec(&self.links)
            .map_err(|error| FrameError::Encode(error.to_string()))?;

        let mut payload = Vec::with_capacity(
            20 + meta_bytes.len() + record_bytes.len() + cell_bytes.len()
                + cluster_bytes.len() + link_bytes.len(),
        );
        push_section(&mut payload, &meta_bytes);
        push_section(&mut payload, &record_bytes);
        push_section(&mut payload, &cell_bytes);
        push_section(&mut payload, &cluster_bytes);
        push_section(&mut payload, &link_bytes);
        Frame::new(MessageKind::StructuredPaneFrame, payload)
    }
}

/// Borrowed view over a decoded structured frame payload.
#[derive(Debug)]
pub struct DecodedPaneFrame<'a> {
    pub meta: PaneFrameMeta,
    records: &'a [u8],
    cells: &'a [u8],
    clusters: Vec<String>,
    links: Vec<String>,
}

impl<'a> DecodedPaneFrame<'a> {
    /// Materializes a legacy `PaneDelta` for the existing client apply path.
    pub fn to_delta(&self) -> Result<super::PaneDelta, FrameError> {
        let mut rows = Vec::with_capacity(self.records.len() / ROW_RECORD_LEN);
        let mut cell_offset = 0_usize;
        for record in self.records.chunks_exact(ROW_RECORD_LEN) {
            let record = PaneRowRecord::from_bytes(record.try_into().expect("row record"));
            let len = usize::from(record.len) * CELL_LEN;
            let raw = self
                .cells
                .get(cell_offset..cell_offset.saturating_add(len))
                .ok_or_else(|| FrameError::Decode("row cell range out of bounds".into()))?;
            cell_offset += len;

            let mut cells = Vec::with_capacity(usize::from(record.len));
            for chunk in raw.chunks_exact(CELL_LEN) {
                let wire = PaneWireCell::from_bytes(chunk.try_into().expect("cell"));
                cells.push(unwire_cell(
                    wire,
                    &self.clusters,
                    &self.links,
                )?);
            }
            rows.push(super::RowUpdate {
                row: record.row,
                cells,
                wrapped: record.flags & 1 != 0,
            });
        }
        Ok(self.meta.clone().into_delta(rows))
    }
}

/// Decodes a structured pane frame without copying the cell array.
pub fn decode_pane_frame(frame: &Frame) -> Result<DecodedPaneFrame<'_>, FrameError> {
    if frame.kind != MessageKind::StructuredPaneFrame {
        return Err(FrameError::WrongKind {
            expected: MessageKind::StructuredPaneFrame,
            actual: frame.kind,
        });
    }
    let mut rest = frame.payload.as_slice();
    let (meta_bytes, next) = take_section(rest)?;
    rest = next;
    let (record_bytes, next) = take_section(rest)?;
    rest = next;
    let (cell_bytes, next) = take_section(rest)?;
    rest = next;
    let (cluster_bytes, next) = take_section(rest)?;
    rest = next;
    let (link_bytes, next) = take_section(rest)?;
    if !next.is_empty() {
        return Err(FrameError::Decode("trailing pane frame bytes".into()));
    }

    let meta: PaneFrameMeta = rmp_serde::from_slice(meta_bytes)
        .map_err(|error| FrameError::Decode(error.to_string()))?;
    let clusters: Vec<String> = rmp_serde::from_slice(cluster_bytes)
        .map_err(|error| FrameError::Decode(error.to_string()))?;
    let links: Vec<String> = rmp_serde::from_slice(link_bytes)
        .map_err(|error| FrameError::Decode(error.to_string()))?;

    if record_bytes.len() % ROW_RECORD_LEN != 0 || cell_bytes.len() % CELL_LEN != 0 {
        return Err(FrameError::Decode("unaligned pane frame sections".into()));
    }

    Ok(DecodedPaneFrame {
        meta,
        records: record_bytes,
        cells: cell_bytes,
        clusters,
        links,
    })
}

fn push_section(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn take_section(bytes: &[u8]) -> Result<(&[u8], &[u8]), FrameError> {
    if bytes.len() < 4 {
        return Err(FrameError::Decode("truncated pane frame section".into()));
    }
    let len = u32::from_le_bytes(bytes[0..4].try_into().expect("four bytes")) as usize;
    let rest = &bytes[4..];
    if rest.len() < len {
        return Err(FrameError::Decode("short pane frame section".into()));
    }
    Ok((&rest[..len], &rest[len..]))
}

fn wire_cell(
    cell: &Cell,
    clusters: &mut Vec<String>,
    cluster_ids: &mut HashMap<String, u32>,
    links: &mut Vec<String>,
    link_ids: &mut HashMap<String, u32>,
) -> PaneWireCell {
    let text = cell.text.as_str();
    let ch = if text.is_empty() {
        0 // blank cells are intentionally empty, not a space glyph
    } else if let Some(first) = text.chars().next() {
        if text.chars().count() == 1 {
            u32::from(first)
        } else {
            let index = intern(clusters, cluster_ids, text);
            CLUSTER_FLAG | index
        }
    } else {
        0
    };

    let link = match &cell.link {
        Some(uri) if !uri.is_empty() => intern(links, link_ids, uri),
        _ => 0,
    };

    PaneWireCell {
        ch,
        fg: color_code(cell.fg),
        bg: color_code(cell.bg),
        attrs: cell.attrs.0 as u16,
        width: cell.width,
        link,
    }
}

fn unwire_cell(
    wire: PaneWireCell,
    clusters: &[String],
    links: &[String],
) -> Result<Cell, FrameError> {
    let text = if wire.ch == 0 {
        String::new()
    } else if wire.ch & CLUSTER_FLAG != 0 {
        let id = (wire.ch & !CLUSTER_FLAG) as usize;
        clusters
            .get(id.checked_sub(1).unwrap_or(usize::MAX))
            .cloned()
            .ok_or_else(|| FrameError::Decode("cluster index out of range".into()))?
    } else {
        char::from_u32(wire.ch)
            .unwrap_or(' ')
            .to_string()
    };

    let link = if wire.link == 0 {
        None
    } else {
        Some(
            links
                .get((wire.link as usize).checked_sub(1).unwrap_or(usize::MAX))
                .cloned()
                .ok_or_else(|| FrameError::Decode("link index out of range".into()))?,
        )
    };

    Ok(Cell {
        text: text.into(),
        fg: decode_color(wire.fg),
        bg: decode_color(wire.bg),
        attrs: Attrs(u32::from(wire.attrs)),
        width: wire.width,
        link,
    })
}

fn intern(
    values: &mut Vec<String>,
    ids: &mut HashMap<String, u32>,
    value: &str,
) -> u32 {
    if let Some(id) = ids.get(value) {
        return *id;
    }
    let id = values.len() as u32 + 1;
    values.push(value.to_owned());
    ids.insert(value.to_owned(), id);
    id
}

const RGB_FLAG: u32 = 1 << 16;

fn color_code(color: Color) -> u32 {
    match color {
        Color::Default => 0,
        Color::Indexed(index) => u32::from(index) + 1,
        Color::Rgb(r, g, b) => {
            RGB_FLAG | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
        }
    }
}

fn decode_color(code: u32) -> Color {
    if code & RGB_FLAG != 0 {
        Color::Rgb(
            ((code >> 16) & 0xff) as u8,
            ((code >> 8) & 0xff) as u8,
            (code & 0xff) as u8,
        )
    } else {
        match code {
            0 => Color::Default,
            index => Color::Indexed((index - 1) as u8),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Cursor, Modes};
    use crate::protocol::{PaneDelta, RowUpdate};

    fn delta() -> PaneDelta {
        PaneDelta {
            pane_id: 7,
            seq: 3,
            rows: vec![
                RowUpdate {
                    row: 1,
                    cells: vec![
                        Cell {
                            text: "a".into(),
                            fg: Color::Rgb(1, 2, 3),
                            bg: Color::Indexed(9),
                            attrs: Attrs::BOLD,
                            width: 1,
                            link: None,
                        },
                        Cell {
                            text: "e\u{301}".into(),
                            ..Cell::blank()
                        },
                    ],
                    wrapped: true,
                },
                RowUpdate {
                    row: 4,
                    cells: vec![Cell {
                        text: "z".into(),
                        ..Cell::blank()
                    }],
                    wrapped: false,
                },
            ],
            cursor: Cursor::default(),
            modes: Modes::default(),
            title: "t".into(),
            cwd: None,
            foreground: None,
            shell_marks: None,
            program_status: None,
            notification: None,
            progress: None,
            alt_screen: false,
            scrollback_len: 0,
            total_lines: 0,
            scrollback_bytes: 0,
            bell: false,
        }
    }

    #[test]
    fn wire_cell_layout_is_stable() {
        assert_eq!(std::mem::size_of::<PaneWireCell>(), CELL_LEN);
        let wire = PaneWireCell {
            ch: 0x1234_5678,
            fg: 0x0102_0304,
            bg: 0x0506_0708,
            attrs: 0xabcd,
            width: 2,
            link: 0x1122_3344,
        };
        let mut bytes = [0_u8; CELL_LEN];
        wire.to_bytes(&mut bytes);
        assert_eq!(PaneWireCell::from_bytes(&bytes), wire);
    }

    #[test]
    fn structured_frame_round_trip_preserves_clusters_and_links() {
        let mut source = delta();
        source.rows[0].cells[1].link = Some("https://example.com".into());
        let frame = StructuredPaneFrame::from_delta(&source).encode().expect("encode");
        let decoded = decode_pane_frame(&frame).expect("decode");
        let round_trip = decoded.to_delta().expect("materialize");
        assert_eq!(round_trip, source);
    }
}
