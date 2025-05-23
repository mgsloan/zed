/*

use collections::HashMap;
use std::{
    sync::{RwLock, atomic::AtomicI32},
    time::Instant,
};

// todo! Does module_path include line / col?
static mut THROTTLE_MAP: HashMap<&str, RwLock<ThrottleEntry>> = HashMap::default();

struct ThrottleEntry {
    start_time: Instant,
    occurrences: AtomicI32,
}

pub fn check_throttled(module_path: &str) {
    Instant::now()
    // Use entry API?
    if let Some(throttle_entry) = THROTTLE_MAP.get(module_path) {
        throttle_entry.
    } else {
    }
}


#[macro_export]
macro_rules! log_once_every {
*/
