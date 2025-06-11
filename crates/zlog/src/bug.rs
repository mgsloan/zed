use std::sync::{Arc, Mutex};

// Bug handler functionality
type BugHandler = Arc<dyn Fn(&str, &str, &str, u32) + Send + Sync>;

static BUG_HANDLER: Mutex<Option<BugHandler>> = Mutex::new(None);

/// Set a global bug handler that will be called when a bug! macro is invoked for the first time.
/// The handler receives: (message, module_path, file, line)
pub fn set_bug_handler<F>(handler: F)
where
    F: Fn(&str, &str, &str, u32) + Send + Sync + 'static,
{
    *BUG_HANDLER.lock().unwrap() = Some(Arc::new(handler));
}

/// Call the bug handler if one is set
pub(crate) fn call_bug_handler(message: &str, module_path: &str, file: &str, line: u32) {
    if let Some(handler) = BUG_HANDLER.lock().unwrap().as_ref() {
        handler(message, module_path, file, line);
    }
}

/// Clear the bug handler
pub fn clear_bug_handler() {
    *BUG_HANDLER.lock().unwrap() = None;
}

// Bug macros
#[macro_export]
macro_rules! bug {
    ($logger:expr => $($arg:tt)+) => {
        $crate::bug_impl!($logger, $($arg)+)
    };
    ($($arg:tt)+) => {
        $crate::bug_impl!($crate::default_logger!(), $($arg)+)
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! bug_impl {
    ($logger:expr, $($arg:tt)+) => {{
        use std::sync::atomic::{AtomicBool, Ordering};

        // Create a unique static for each call site
        static BUG_ALREADY_REPORTED: AtomicBool = AtomicBool::new(false);

        let message = format!($($arg)+);
        let should_report = !BUG_ALREADY_REPORTED.swap(true, Ordering::SeqCst);

        if should_report {
            $crate::bug::call_bug_handler(
                &message,
                module_path!(),
                file!(),
                line!(),
            );
        }

        $crate::log_with_prefix!($logger, $crate::log_impl::Level::Error, "bug: ", $($arg)+);
    }};
}

// Tests
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Logger, Scope, bug};
    use std::sync::{Arc, Mutex};

    #[test]
    fn test_bug_macro_only_reports_once() {
        // Clear any existing handler first
        clear_bug_handler();

        // Set up a test handler that captures bug reports
        let reports = Arc::new(Mutex::new(Vec::new()));
        let reports_clone = reports.clone();

        set_bug_handler(move |message, module, file, line| {
            reports_clone.lock().unwrap().push((
                message.to_string(),
                module.to_string(),
                file.to_string(),
                line,
            ));
        });

        // Test in a loop - only the first iteration should report
        for i in 0..3 {
            bug!("Test bug message: {}", i);
        }

        // Check that only one report was made
        let captured_reports = reports.lock().unwrap();
        assert_eq!(captured_reports.len(), 1);
        assert_eq!(captured_reports[0].0, "Test bug message: 0");

        // Clear the handler
        clear_bug_handler();
    }

    #[test]
    fn test_bug_macro_different_locations() {
        // Clear any existing handler first
        clear_bug_handler();

        // Set up a test handler that captures bug reports
        let reports = Arc::new(Mutex::new(Vec::new()));
        let reports_clone = reports.clone();

        set_bug_handler(move |message, _module, _file, _line| {
            reports_clone.lock().unwrap().push(message.to_string());
        });

        // First location
        bug!("Bug at location 1");

        // Second location
        if true {
            bug!("Bug at location 2");
        }

        // Third location
        match Some(1) {
            Some(_) => bug!("Bug at location 3"),
            None => {}
        }

        // Check that all three locations reported
        let captured_reports = reports.lock().unwrap();
        assert_eq!(captured_reports.len(), 3);
        assert_eq!(captured_reports[0], "Bug at location 1");
        assert_eq!(captured_reports[1], "Bug at location 2");
        assert_eq!(captured_reports[2], "Bug at location 3");

        // Clear the handler
        clear_bug_handler();
    }

    #[test]
    fn test_bug_macro_with_logger() {
        // Clear any existing handler first
        clear_bug_handler();

        // Set up a test handler
        let reports = Arc::new(Mutex::new(Vec::new()));
        let reports_clone = reports.clone();

        set_bug_handler(move |message, _module, _file, _line| {
            reports_clone.lock().unwrap().push(message.to_string());
        });

        let scope: Scope = ["test_scope", "", "", ""];
        let logger = Logger { scope };

        // Test with explicit logger
        bug!(logger => "Bug with logger: {}", "custom");

        let captured_reports = reports.lock().unwrap();
        assert_eq!(captured_reports.len(), 1);
        assert_eq!(captured_reports[0], "Bug with logger: custom");

        // Clear the handler
        clear_bug_handler();
    }

    #[test]
    fn test_bug_macro_without_handler() {
        // Clear any existing handler
        clear_bug_handler();

        // This should not panic, just log
        bug!("Bug without handler");

        // If we get here, the test passed (no panic occurred)
    }
}
