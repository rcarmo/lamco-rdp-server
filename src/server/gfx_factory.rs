//! GFX Server Factory for EGFX/H.264 Video Streaming
//!
//! This module implements `ironrdp_server::GfxServerFactory` to integrate
//! our EGFX handler with IronRDP's server infrastructure.
//!
//! # Architecture
//!
//! ```text
//! LamcoGfxFactory (implements GfxServerFactory)
//!       │
//!       ├─► Creates Arc<Mutex<GraphicsPipelineServer>>
//!       │
//!       ├─► Returns GfxDvcBridge for DrdynvcServer (handles client messages)
//!       │
//!       └─► Stores GfxServerHandle for display handler (frame sending)
//! ```
//!
//! # Hybrid Architecture
//!
//! This factory implements the "Hybrid" approach (Option E) for proactive EGFX
//! frame sending:
//!
//! 1. **GfxDvcBridge** - Wraps the GraphicsPipelineServer in Arc<Mutex<>>
//!    and implements DvcProcessor for the DrdynvcServer to use
//!
//! 2. **GfxServerHandle** - Clone of the Arc given to display handler for
//!    calling send_avc420_frame() directly
//!
//! 3. **ServerEvent::Egfx** - Routes the resulting DVC messages to the wire

use std::sync::{Arc, Mutex};

use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer};
use ironrdp_graphics::zgfx::CompressionMode;
use ironrdp_server::{
    GfxDvcBridge, GfxServerFactory, GfxServerHandle, ServerEvent, ServerEventSender,
};
use tokio::sync::{RwLock, mpsc};

use crate::egfx::LamcoGraphicsHandler;

/// Factory for creating EGFX graphics pipeline handlers
///
/// This factory is passed to the RdpServer builder and creates
/// a shared `GraphicsPipelineServer` for each client connection.
///
/// # Platform Quirks
///
/// The factory accepts a `force_avc420_only` flag which is passed to the handler.
/// This is used when platform detection (e.g., RHEL 9) identifies that AVC444
/// produces visual artifacts. The handler will then disable AVC444 regardless
/// of client capability.
///
/// # Usage
///
/// ```ignore
/// // Check if platform has AVC444 quirk
/// let force_avc420 = capabilities.profile.has_quirk(&Quirk::ForceAvc420);
///
/// let gfx_factory = LamcoGfxFactory::with_quirks(width, height, force_avc420);
///
/// // Get handle for display handler before passing to RdpServer
/// let gfx_handle = gfx_factory.server_handle();
///
/// let server = RdpServer::builder()
///     .with_gfx_handler(gfx_factory)
///     // ...
///     .build();
///
/// // Display handler uses gfx_handle to send frames
/// display_handler.set_gfx_server(gfx_handle);
/// ```
pub struct LamcoGfxFactory {
    /// Initial desktop dimensions
    width: u16,
    height: u16,

    /// Shared state for checking handler readiness from other parts of the server
    handler_state: Arc<RwLock<Option<HandlerState>>>,

    /// Shared GraphicsPipelineServer for proactive frame sending
    /// Created lazily on first call to build_server_with_handle()
    server_handle: Arc<RwLock<Option<GfxServerHandle>>>,

    /// Force AVC420-only mode due to platform quirks (e.g., RHEL 9)
    force_avc420_only: bool,

    /// Maximum frames in flight before backpressure
    max_frames_in_flight: u32,

    /// ZGFX compression mode for EGFX data
    compression_mode: CompressionMode,
}

/// Shared handler state accessible from display handler
///
/// This state is updated by `WrdGraphicsHandler` callbacks and read by
/// `EgfxFrameSender` to determine EGFX readiness and get the DVC channel ID.
#[derive(Debug, Clone, Default)]
pub struct HandlerState {
    /// Whether EGFX channel is ready (capabilities negotiated)
    pub is_ready: bool,
    /// Whether AVC420 (H.264 YUV420) codec is supported
    pub is_avc420_enabled: bool,
    /// Whether AVC444 (H.264 YUV444) codec is supported
    pub is_avc444_enabled: bool,
    /// Whether this client needs Android RD Client pointer workaround updates.
    ///
    /// Android clients that negotiate EGFX with AVC_DISABLED do not reliably draw
    /// a visible local pointer unless the server sends explicit pointer PDUs.
    /// Windows clients must not receive this workaround because the Android cursor
    /// bitmap is vertically flipped for that client quirk.
    pub needs_android_pointer_updates: bool,
    /// Primary surface ID for frame sending (None = no surface yet)
    /// Note: Surface ID 0 is valid in EGFX, so we use Option
    pub primary_surface_id: Option<u16>,
    /// DVC channel ID assigned to EGFX (needed for encode_dvc_messages)
    pub dvc_channel_id: u32,
}

/// Type alias for shared handler state
pub type SharedHandlerState = Arc<RwLock<Option<HandlerState>>>;

impl LamcoGfxFactory {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            handler_state: Arc::new(RwLock::new(None)),
            server_handle: Arc::new(RwLock::new(None)),
            force_avc420_only: false,
            max_frames_in_flight: 3,
            compression_mode: CompressionMode::Never, // Default: no compression
        }
    }

    /// Use this constructor when platform detection has identified quirks
    /// that affect codec selection (e.g., RHEL 9 AVC444 blur issue).
    pub fn with_quirks(width: u16, height: u16, force_avc420_only: bool) -> Self {
        Self {
            width,
            height,
            handler_state: Arc::new(RwLock::new(None)),
            server_handle: Arc::new(RwLock::new(None)),
            force_avc420_only,
            max_frames_in_flight: 3,
            compression_mode: CompressionMode::Never,
        }
    }

    pub fn with_config(
        width: u16,
        height: u16,
        force_avc420_only: bool,
        max_frames_in_flight: u32,
        compression_mode: CompressionMode,
    ) -> Self {
        Self {
            width,
            height,
            handler_state: Arc::new(RwLock::new(None)),
            server_handle: Arc::new(RwLock::new(None)),
            force_avc420_only,
            max_frames_in_flight,
            compression_mode,
        }
    }

    /// Get shared reference to handler state
    ///
    /// This can be used by the display handler to check if EGFX is ready
    /// and which codecs are available.
    pub fn handler_state(&self) -> Arc<RwLock<Option<HandlerState>>> {
        Arc::clone(&self.handler_state)
    }

    /// Get the shared GraphicsPipelineServer handle
    ///
    /// This returns the handle that was created by `build_server_with_handle()`.
    /// Use this to access the server for frame sending from the display handler.
    ///
    /// Returns `None` if `build_server_with_handle()` hasn't been called yet
    /// (i.e., the RDP connection hasn't started the channel attachment phase).
    pub fn server_handle(&self) -> Arc<RwLock<Option<GfxServerHandle>>> {
        Arc::clone(&self.server_handle)
    }
}

impl ServerEventSender for LamcoGfxFactory {
    fn set_sender(&mut self, _sender: mpsc::UnboundedSender<ServerEvent>) {
        // GFX factory doesn't need the server event sender directly;
        // EgfxFrameSender already has its own event_tx from server setup.
    }
}

impl GfxServerFactory for LamcoGfxFactory {
    fn build_gfx_handler(&self) -> Box<dyn GraphicsPipelineHandler> {
        let handler =
            LamcoGraphicsHandler::with_quirks(self.width, self.height, self.force_avc420_only);
        Box::new(handler)
    }

    fn build_server_with_handle(&self) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        // This is called while IronRDP attaches channels for a new connection.
        // Clear readiness here, before the new client's EGFX capability exchange;
        // the handler below will repopulate it from on_ready(). Do not clear this
        // later from the display pipeline, because that races with Android's fast
        // AVC_DISABLED negotiation and leaves the pipeline stuck in bitmap fallback.
        for attempt in 0..100 {
            match self.handler_state.try_write() {
                Ok(mut state) => {
                    *state = None;
                    break;
                }
                Err(_) if attempt < 10 => std::thread::yield_now(),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(1)),
            }
        }

        // Handler updates handler_state when callbacks are invoked,
        // allowing EgfxFrameSender to check EGFX readiness
        let handler = LamcoGraphicsHandler::with_config(
            self.width,
            self.height,
            Arc::clone(&self.handler_state),
            self.force_avc420_only,
            self.max_frames_in_flight,
        );

        // std::sync::Mutex (not tokio) because DvcProcessor trait
        // has synchronous methods that cannot use async locks
        let server = Arc::new(Mutex::new(GraphicsPipelineServer::with_compression(
            Box::new(handler),
            self.compression_mode,
        )));

        // This callback is synchronous, while the display pipeline polls the
        // same tokio RwLock from async code. A single try_write() can lose the
        // new per-connection handle under read contention, leaving EGFX
        // negotiated but permanently "not ready" until bitmap fallback crashes
        // Android with 0xd06/0x200d. Retry briefly: readers hold the lock only
        // for a very short readiness check.
        let mut stored_handle = false;
        for attempt in 0..100 {
            if let Ok(mut handle_guard) = self.server_handle.try_write() {
                *handle_guard = Some(Arc::clone(&server));
                stored_handle = true;
                break;
            }

            if attempt < 10 {
                std::thread::yield_now();
            } else {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }

        if !stored_handle {
            tracing::error!("EGFX: failed to store GfxServerHandle after retries");
        }

        let bridge = GfxDvcBridge::new(Arc::clone(&server));

        Some((bridge, server))
    }
}
