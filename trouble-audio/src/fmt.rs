//! Internal logging shims forwarding to `log` and/or `defmt`, whichever features are enabled, so
//! call sites don't repeat the `#[cfg]` dance. Only for format strings valid under both backends
//! (plain `{}` of primitives); sites needing backend-specific formatting (e.g.
//! `defmt::Debug2Format`) keep explicit `#[cfg]` blocks.
#![allow(unused_macros)]

macro_rules! debug {
    ($s:literal $(, $x:expr)* $(,)?) => {{
        #[cfg(feature = "log")]
        ::log::debug!($s $(, $x)*);
        #[cfg(feature = "defmt")]
        ::defmt::debug!($s $(, $x)*);
        #[cfg(not(any(feature = "log", feature = "defmt")))]
        let _ = ($( & $x ),*);
    }};
}

macro_rules! info {
    ($s:literal $(, $x:expr)* $(,)?) => {{
        #[cfg(feature = "log")]
        ::log::info!($s $(, $x)*);
        #[cfg(feature = "defmt")]
        ::defmt::info!($s $(, $x)*);
        #[cfg(not(any(feature = "log", feature = "defmt")))]
        let _ = ($( & $x ),*);
    }};
}

macro_rules! warn {
    ($s:literal $(, $x:expr)* $(,)?) => {{
        #[cfg(feature = "log")]
        ::log::warn!($s $(, $x)*);
        #[cfg(feature = "defmt")]
        ::defmt::warn!($s $(, $x)*);
        #[cfg(not(any(feature = "log", feature = "defmt")))]
        let _ = ($( & $x ),*);
    }};
}
