//! Shared allocation tracking exported for loadable exporters.
//!
//! Exporters resolve these symbols from the core library so one
//! `otel_ensure_all_released()` check sees handles from the core and from every
//! loadable exporter in the process.

/// Records one caller-owned handle allocation.
#[unsafe(no_mangle)]
pub extern "C" fn otel_handle_created() {
    crate::abi::handle_created();
}

/// Records one caller-owned handle release.
#[unsafe(no_mangle)]
pub extern "C" fn otel_handle_destroyed() {
    crate::abi::handle_destroyed();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::live_handle_count;
    use crate::handles::otel_live_handle_count;
    use std::sync::{Mutex, PoisonError};

    static TRACKING_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn exported_counters_move_the_live_handle_count() {
        let _guard = TRACKING_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let before = live_handle_count();
        otel_handle_created();
        let created = live_handle_count();
        let exported = otel_live_handle_count();
        otel_handle_destroyed();
        // Other tests share the process-wide counter, so only require our
        // allocation to be included in both observations.
        assert!(created > before);
        assert!(exported > before);
    }
}
