//! Standalone executable named by the Chrome/Edge Native Messaging manifest.

#[tokio::main]
async fn main() {
    match rivo_tools::browser_extension_host::run().await {
        Ok(()) => {}
        // Tokio's stdin bridge uses a blocking worker that cannot be cancelled
        // by timeout. Exit directly after the host has sent its structured
        // deadline response so Chrome can observe disconnect and relaunch it.
        Err(rivo_tools::browser_extension::ExtensionBrowserError::Timeout) => {
            std::process::exit(0);
        }
        Err(error) => {
            // Native Messaging stdout is a protocol stream; diagnostics belong
            // on stderr so a failed host never corrupts extension framing.
            eprintln!("rivo browser extension host failed: {error}");
            std::process::exit(1);
        }
    }
}
