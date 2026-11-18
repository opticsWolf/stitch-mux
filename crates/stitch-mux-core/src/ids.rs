use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id_type {
    ($name:ident, $label:expr) => {
        #[derive(
            Clone,
            Copy,
            Debug,
            Default,
            Eq,
            Hash,
            Ord,
            PartialEq,
            PartialOrd,
            Serialize,
            Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub u64);

        impl $name {
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self(value)
            }
        }

        impl From<$name> for u64 {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($label, "{}"), self.0)
            }
        }
    };
}

id_type!(SessionId, "s");
id_type!(WindowId, "w");
id_type!(PaneId, "p");
id_type!(ClientId, "c");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_through_u64() {
        let pane = PaneId::new(42);
        assert_eq!(u64::from(pane), 42);
        assert_eq!(PaneId::from(42_u64), pane);
        assert_eq!(pane.to_string(), "p42");
    }
}
