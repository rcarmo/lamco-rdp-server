//! Data-Control Clipboard Provider (Wayland ext-data-control / wlr-data-control)
//!
//! Bridges `xdg_desktop_portal_generic::ClipboardBackend` to the `ClipboardProvider`
//! trait. All ClipboardBackend methods are synchronous, so they run on
//! `tokio::task::spawn_blocking`.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use xdg_desktop_portal_generic::ClipboardBackend;

use crate::clipboard::{
    error::{ClipboardError, Result},
    provider::{ClipboardProvider, ClipboardProviderEvent},
};

/// Data-control clipboard provider.
///
/// Uses native Wayland data-control protocols (ext-data-control-v1 or
/// wlr-data-control-v1) via the portal-generic library. No Portal daemon
/// or D-Bus required.
pub struct DataControlClipboardProvider {
    /// The underlying clipboard backend from portal-generic
    backend: Arc<Mutex<Box<dyn ClipboardBackend>>>,
    /// Kept alive so the receiver channel doesn't close if the callback clone is dropped
    _event_tx: mpsc::UnboundedSender<ClipboardProviderEvent>,
    /// Receiver end (taken by subscribe())
    event_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<ClipboardProviderEvent>>>,
    /// Shutdown signal
    shutdown: Arc<AtomicBool>,
    /// Data-control sources need bytes available synchronously when the
    /// compositor asks for them. Keep eagerly fetched RDP data here so a
    /// later SetClipboard can publish MIME types together with their bytes.
    source_data: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl DataControlClipboardProvider {
    /// Create a new data-control clipboard provider.
    pub fn new(backend: Arc<Mutex<Box<dyn ClipboardBackend>>>) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        // Hook the selection-changed callback to emit events
        let tx_clone = event_tx.clone();
        {
            let mut guard = backend
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.on_selection_changed(Box::new(move |mime_types| {
                // Data-control signals are always authoritative: the compositor only fires
                // the selection event when a DIFFERENT client takes ownership
                let _ = tx_clone.send(ClipboardProviderEvent::SelectionChanged {
                    mime_types,
                    force: true,
                });
            }));
        }

        info!("Data-control clipboard provider created");

        Self {
            backend,
            _event_tx: event_tx,
            event_rx: std::sync::Mutex::new(Some(event_rx)),
            shutdown: Arc::new(AtomicBool::new(false)),
            source_data: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl ClipboardProvider for DataControlClipboardProvider {
    fn name(&self) -> &'static str {
        "data-control"
    }

    fn supports_file_transfer(&self) -> bool {
        // Data-control supports arbitrary MIME types including text/uri-list
        true
    }

    async fn announce_formats(&self, mime_types: Vec<String>) -> Result<()> {
        let backend = Arc::clone(&self.backend);
        let cached_source_data = {
            let guard = self.source_data.lock().map_err(|e| {
                ClipboardError::PortalError(format!("Source data lock poisoned: {e}"))
            })?;

            mime_types
                .iter()
                .filter_map(|mime| guard.get(mime).map(|data| (mime.clone(), data.clone())))
                .collect::<HashMap<_, _>>()
        };

        tokio::task::spawn_blocking(move || {
            let mut guard = backend
                .lock()
                .map_err(|e| ClipboardError::PortalError(format!("Backend lock poisoned: {e}")))?;

            // Data-control cannot rely on Portal-style delayed rendering: by the
            // time a remote paste happens, the compositor expects the source to
            // serve data synchronously. Publish any eagerly fetched bytes with
            // the MIME list.
            let data = xdg_desktop_portal_generic::types::ClipboardData {
                mime_types,
                data: cached_source_data,
            };

            guard
                .set_clipboard(data)
                .map_err(|e| ClipboardError::PortalError(format!("set_clipboard failed: {e}")))?;

            Ok(())
        })
        .await
        .map_err(|e| ClipboardError::PortalError(format!("announce_formats task panicked: {e}")))?
    }

    async fn read_data(&self, mime_type: &str) -> Result<Vec<u8>> {
        let backend = Arc::clone(&self.backend);
        let mime_owned = mime_type.to_string();

        tokio::task::spawn_blocking(move || {
            let guard = backend
                .lock()
                .map_err(|e| ClipboardError::PortalError(format!("Backend lock poisoned: {e}")))?;

            match guard.read_selection(&mime_owned) {
                Ok(Some(data)) => {
                    debug!("data-control: read {} bytes for {}", data.len(), mime_owned);
                    Ok(data)
                }
                Ok(None) => {
                    warn!("data-control: no data available for {}", mime_owned);
                    Ok(Vec::new())
                }
                Err(e) => Err(ClipboardError::PortalError(format!(
                    "read_selection failed: {e}"
                ))),
            }
        })
        .await
        .map_err(|e| ClipboardError::PortalError(format!("read_data task panicked: {e}")))?
    }

    async fn provide_data(&self, mime_type: &str, data: Vec<u8>) -> Result<()> {
        let backend = Arc::clone(&self.backend);
        let mime_owned = mime_type.to_string();

        {
            let mut guard = self.source_data.lock().map_err(|e| {
                ClipboardError::PortalError(format!("Source data lock poisoned: {e}"))
            })?;
            guard.insert(mime_owned.clone(), data.clone());
        }

        tokio::task::spawn_blocking(move || {
            let mut guard = backend
                .lock()
                .map_err(|e| ClipboardError::PortalError(format!("Backend lock poisoned: {e}")))?;

            guard.update_source_data(&mime_owned, data).map_err(|e| {
                ClipboardError::PortalError(format!("update_source_data failed: {e}"))
            })?;

            Ok(())
        })
        .await
        .map_err(|e| ClipboardError::PortalError(format!("provide_data task panicked: {e}")))?
    }

    fn requires_upfront_data(&self) -> bool {
        true
    }

    async fn complete_transfer(
        &self,
        serial: u32,
        _mime_type: &str,
        _data: Vec<u8>,
        success: bool,
    ) -> Result<()> {
        let backend = Arc::clone(&self.backend);

        tokio::task::spawn_blocking(move || {
            let mut guard = backend
                .lock()
                .map_err(|e| ClipboardError::PortalError(format!("Backend lock poisoned: {e}")))?;

            guard
                .write_done(serial, success)
                .map_err(|e| ClipboardError::PortalError(format!("write_done failed: {e}")))?;

            Ok(())
        })
        .await
        .map_err(|e| ClipboardError::PortalError(format!("complete_transfer task panicked: {e}")))?
    }

    #[expect(
        clippy::expect_used,
        reason = "subscribe() is a one-shot initialization call"
    )]
    fn subscribe(&self) -> mpsc::UnboundedReceiver<ClipboardProviderEvent> {
        self.event_rx
            .lock()
            .expect("subscribe called from single thread")
            .take()
            .expect("subscribe() called more than once")
    }

    async fn health_check(&self) -> Result<()> {
        let backend = Arc::clone(&self.backend);
        tokio::task::spawn_blocking(move || {
            let guard = backend
                .lock()
                .map_err(|e| ClipboardError::PortalError(format!("Backend lock poisoned: {e}")))?;
            let protocol = guard.protocol_type();
            debug!("data-control health check: protocol={:?}", protocol);
            Ok(())
        })
        .await
        .map_err(|e| ClipboardError::PortalError(format!("health_check panicked: {e}")))?
    }

    async fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        debug!("Data-control clipboard provider shut down");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use xdg_desktop_portal_generic::{ClipboardProtocol, types::ClipboardData};

    #[derive(Default)]
    struct MockClipboardBackend {
        last_clipboard: Option<ClipboardData>,
        source_updates: HashMap<String, Vec<u8>>,
    }

    impl ClipboardBackend for MockClipboardBackend {
        fn protocol_type(&self) -> ClipboardProtocol {
            ClipboardProtocol::ExtDataControl
        }

        fn get_clipboard(&self) -> xdg_desktop_portal_generic::Result<ClipboardData> {
            Ok(self.last_clipboard.clone().unwrap_or_default())
        }

        fn set_clipboard(&mut self, data: ClipboardData) -> xdg_desktop_portal_generic::Result<()> {
            self.last_clipboard = Some(data);
            Ok(())
        }

        fn on_selection_changed(&mut self, _callback: Box<dyn Fn(Vec<String>) + Send + Sync>) {}

        fn read_selection(
            &self,
            mime_type: &str,
        ) -> xdg_desktop_portal_generic::Result<Option<Vec<u8>>> {
            Ok(self
                .last_clipboard
                .as_ref()
                .and_then(|clipboard| clipboard.data.get(mime_type).cloned()))
        }

        fn update_source_data(
            &mut self,
            mime_type: &str,
            data: Vec<u8>,
        ) -> xdg_desktop_portal_generic::Result<()> {
            self.source_updates.insert(mime_type.to_string(), data);
            Ok(())
        }

        fn write_done(
            &mut self,
            _serial: u32,
            _success: bool,
        ) -> xdg_desktop_portal_generic::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_provider_name_compiles() {
        fn assert_provider<T: ClipboardProvider>() {}
        assert_provider::<DataControlClipboardProvider>();
    }

    #[tokio::test]
    async fn test_announce_formats_includes_eagerly_cached_source_data() {
        let backend: Arc<Mutex<Box<dyn ClipboardBackend>>> =
            Arc::new(Mutex::new(Box::<MockClipboardBackend>::default()));
        let provider = DataControlClipboardProvider::new(Arc::clone(&backend));

        provider
            .provide_data("text/plain", b"hello from rdp".to_vec())
            .await
            .unwrap();
        provider
            .announce_formats(vec![
                "text/plain".to_string(),
                "text/plain;charset=utf-8".to_string(),
            ])
            .await
            .unwrap();

        let guard = backend.lock().unwrap();
        let clipboard = guard.get_clipboard().unwrap();
        assert_eq!(
            clipboard.data.get("text/plain"),
            Some(&b"hello from rdp".to_vec())
        );
        assert!(!clipboard.data.contains_key("text/plain;charset=utf-8"));
    }
}
