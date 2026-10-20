//! Host stub of `wifi`: only the network-up flag the status icons read.
use std::sync::atomic::{AtomicBool, Ordering};

static NET_UP: AtomicBool = AtomicBool::new(true);
pub fn net_up() -> bool { NET_UP.load(Ordering::Relaxed) }
pub fn set_net_up(on: bool) { NET_UP.store(on, Ordering::Relaxed); }
