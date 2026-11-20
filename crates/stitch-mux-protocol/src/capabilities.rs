/// Client capability bits requested in Hello.
pub mod client {
    pub const STRUCTURED_VIEW: u64 = 1 << 0;
    pub const ANSI: u64 = 1 << 1;
    pub const TRUECOLOR: u64 = 1 << 2;
    pub const SYNC_OUTPUT: u64 = 1 << 3;
    pub const KITTY_KEYS: u64 = 1 << 4;
    pub const MOUSE: u64 = 1 << 5;
    pub const FOCUS: u64 = 1 << 6;
    pub const BRACKETED_PASTE: u64 = 1 << 7;
    pub const LAYOUT_CONTROL: u64 = 1 << 8;
    pub const CAPTURE: u64 = 1 << 9;
    pub const RESYNC: u64 = 1 << 10;
    pub const PROGRAM_STATUS: u64 = 1 << 11;
    pub const SHELL_INTEGRATION: u64 = 1 << 12;
    pub const HYPERLINKS: u64 = 1 << 13;
    pub const EVENTS: u64 = 1 << 14;
    pub const ACTIONS: u64 = 1 << 15;
}

/// Server capability bits advertised in HelloAck.
pub mod server {
    pub const STRUCTURED_VIEW: u64 = 1 << 0;
    pub const ANSI: u64 = 1 << 1;
    pub const TRUECOLOR: u64 = 1 << 2;
    pub const STRUCTURED_DELTA: u64 = 1 << 3;
    pub const CAPTURE: u64 = 1 << 4;
    pub const RESYNC: u64 = 1 << 5;
    pub const LAYOUT_CONTROL: u64 = 1 << 6;
    pub const PROGRAM_STATUS: u64 = 1 << 7;
    pub const SHELL_INTEGRATION: u64 = 1 << 8;
    pub const HYPERLINKS: u64 = 1 << 9;
    pub const EVENTS: u64 = 1 << 10;
    pub const ACTIONS: u64 = 1 << 11;
    pub const DETACHED_PANES: u64 = 1 << 12;
}

pub const DEFAULT_CLIENT_CAPABILITIES: u64 = client::STRUCTURED_VIEW
    | client::ANSI
    | client::TRUECOLOR
    | client::KITTY_KEYS
    | client::MOUSE
    | client::FOCUS
    | client::BRACKETED_PASTE
    | client::LAYOUT_CONTROL
    | client::CAPTURE
    | client::RESYNC
    | client::PROGRAM_STATUS
    | client::SHELL_INTEGRATION
    | client::HYPERLINKS
    | client::EVENTS
    | client::ACTIONS;

pub const DEFAULT_SERVER_CAPABILITIES: u64 = server::STRUCTURED_VIEW
    | server::ANSI
    | server::TRUECOLOR
    | server::STRUCTURED_DELTA
    | server::CAPTURE
    | server::RESYNC
    | server::LAYOUT_CONTROL
    | server::PROGRAM_STATUS
    | server::SHELL_INTEGRATION
    | server::HYPERLINKS
    | server::EVENTS
    | server::ACTIONS;

#[cfg(test)]
mod tests {
    use super::{client, server};

    #[test]
    fn capability_bits_do_not_overlap_across_peers_by_mistake() {
        assert_eq!(client::STRUCTURED_VIEW, server::STRUCTURED_VIEW);
        assert_eq!(client::EVENTS, 1 << 14);
        assert_eq!(server::DETACHED_PANES, 1 << 12);
    }
}
