//! RDP Input Handler Implementation
//!
//! Implements the IronRDP `RdpServerInputHandler` trait to forward input events
//! from RDP clients to the Wayland compositor via Portal RemoteDesktop API.
//!
//! # Overview
//!
//! This module bridges the synchronous IronRDP input event callbacks with the
//! asynchronous Portal API, providing complete keyboard and mouse input forwarding
//! with full scancode translation, modifier tracking, and coordinate transformation.
//!
//! # Architecture
//!
//! ```text
//! RDP Client                    WrdInputHandler                 Wayland
//! ━━━━━━━━━━                    ━━━━━━━━━━━━━━━                 ━━━━━━━
//!
//! Keyboard Event ─────────────> KeyboardEvent
//!   scancode=0x1E                     │
//!   pressed=true                      ├─> KeyboardHandler
//!                                     │     └─> Scancode translation
//!                                     │         (0x1E → evdev KEY_A)
//!                                     │
//!                                     ├─> Portal API
//!                                     │     └─> notify_keyboard_keycode()
//!                                     │
//!                                     └─────────────────────────> Input Stack
//!                                                                   └─> Key Press
//!
//! Mouse Event ────────────────> MouseEvent::Move
//!   x=960, y=540                     │
//!                                    ├─> CoordinateTransformer
//!                                    │     └─> RDP coords → Wayland coords
//!                                    │
//!                                    ├─> Portal API
//!                                    │     └─> notify_pointer_motion_absolute()
//!                                    │
//!                                    └─────────────────────────> Input Stack
//!                                                                  └─> Mouse Move
//! ```
//!
//! # Async/Sync Bridging
//!
//! IronRDP's `RdpServerInputHandler` trait has synchronous methods (`fn`, not `async fn`),
//! but Portal API calls are asynchronous. We bridge this gap by:
//!
//! 1. Trait method called synchronously by IronRDP
//! 2. Clone Arc references to shared state
//! 3. Spawn `tokio::spawn()` async task
//! 4. Task performs async Portal API calls
//! 5. Fire-and-forget (RDP doesn't expect acknowledgment for input events)
//!
//! This pattern ensures the synchronous trait method returns immediately while
//! Portal operations proceed asynchronously without blocking.
//!
//! # Example
//!
//! ```ignore
//! use lamco_rdp_server::server::WrdInputHandler;
//! use lamco_rdp_server::portal::RemoteDesktopManager;
//! use lamco_rdp_server::input::MonitorInfo;
//! use std::sync::Arc;
//!
//! let portal = Arc::new(RemoteDesktopManager::new(/* ... */).await?);
//! let session = portal.create_session().await?;
//! let monitors = vec![/* MonitorInfo instances */];
//!
//! let handler = WrdInputHandler::new(portal, session, monitors)?;
//!
//! // Handler is now ready to receive input events from IronRDP
//! // Events are automatically forwarded to Wayland via Portal
//! ```

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

use ironrdp_pdu::pointer::PointerPositionAttribute;
use ironrdp_server::{
    DisplayUpdate, KeyboardEvent as IronKeyboardEvent, MouseEvent as IronMouseEvent, RGBAPointer,
    RdpServerInputHandler,
};
use tokio::sync::{Mutex, RwLock, mpsc};
use tracing::{debug, error, info, trace, warn};

/// Accumulates non-ASCII Unicode input units for clipboard-paste fallback.
///
/// RDP sends CJK input as a stream of UnicodePressed events. Since keysym
/// injection fails for many CJK characters on KDE, we buffer them and flush
/// via write_text + Ctrl+V when a non-Unicode event (keycode) arrives.
struct CjkPasteBuffer {
    buf: String,
    pending_high_surrogate: Option<u16>,
}

impl CjkPasteBuffer {
    fn new() -> Self {
        Self {
            buf: String::new(),
            pending_high_surrogate: None,
        }
    }

    fn push_char(&mut self, c: char) {
        self.buf.push(c);
    }

    /// Decode a UTF-16 surrogate pair and push the resulting char.
    /// Returns the decoded char if the pair is complete, None if `high` was stored
    /// waiting for a low surrogate, or None if the pair is invalid.
    fn push_surrogate_pair(&mut self, high: u16, low: u16) -> Option<char> {
        if !(0xD800..=0xDBFF).contains(&high) || !(0xDC00..=0xDFFF).contains(&low) {
            self.pending_high_surrogate = None;
            return None;
        }
        let code_point = 0x10000 + (u32::from(high - 0xD800) << 10) + u32::from(low - 0xDC00);
        let c = char::from_u32(code_point)?;
        self.buf.push(c);
        self.pending_high_surrogate = None;
        Some(c)
    }

    fn is_empty(&self) -> bool {
        self.buf.is_empty() && self.pending_high_surrogate.is_none()
    }

    /// Drain buffered text. Returns None if nothing was accumulated.
    fn take_text(&mut self) -> Option<String> {
        self.pending_high_surrogate = None;
        if self.buf.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.buf))
        }
    }
}

use crate::clipboard::provider::ClipboardProvider;
use crate::input::{
    CoordinateTransformer, InputError, KeyboardHandler, MonitorInfo, MouseButton, MouseHandler,
};
use crate::server::gfx_factory::HandlerState;

/// Map a Unicode code point to an evdev keycode and whether Shift is needed.
/// Covers printable ASCII (0x20-0x7E) on US QWERTY layout.
fn unicode_to_evdev(cp: u16) -> Option<(u32, bool)> {
    // evdev keycodes from lamco-rdp-input::mapper::keycodes
    const KEY_SPACE: u32 = 57;
    const KEY_1: u32 = 2;
    const KEY_2: u32 = 3;
    const KEY_3: u32 = 4;
    const KEY_4: u32 = 5;
    const KEY_5: u32 = 6;
    const KEY_6: u32 = 7;
    const KEY_7: u32 = 8;
    const KEY_8: u32 = 9;
    const KEY_9: u32 = 10;
    const KEY_0: u32 = 11;
    const KEY_MINUS: u32 = 12;
    const KEY_EQUAL: u32 = 13;
    const KEY_TAB: u32 = 15;
    const KEY_Q: u32 = 16;
    const KEY_W: u32 = 17;
    const KEY_E: u32 = 18;
    const KEY_R: u32 = 19;
    const KEY_T: u32 = 20;
    const KEY_Y: u32 = 21;
    const KEY_U: u32 = 22;
    const KEY_I: u32 = 23;
    const KEY_O: u32 = 24;
    const KEY_P: u32 = 25;
    const KEY_LEFTBRACE: u32 = 26;
    const KEY_RIGHTBRACE: u32 = 27;
    const KEY_ENTER: u32 = 28;
    const KEY_A: u32 = 30;
    const KEY_S: u32 = 31;
    const KEY_D: u32 = 32;
    const KEY_F: u32 = 33;
    const KEY_G: u32 = 34;
    const KEY_H: u32 = 35;
    const KEY_J: u32 = 36;
    const KEY_K: u32 = 37;
    const KEY_L: u32 = 38;
    const KEY_SEMICOLON: u32 = 39;
    const KEY_APOSTROPHE: u32 = 40;
    const KEY_GRAVE: u32 = 41;
    const KEY_BACKSLASH: u32 = 43;
    const KEY_Z: u32 = 44;
    const KEY_X: u32 = 45;
    const KEY_C: u32 = 46;
    const KEY_V: u32 = 47;
    const KEY_B: u32 = 48;
    const KEY_N: u32 = 49;
    const KEY_M: u32 = 50;
    const KEY_COMMA: u32 = 51;
    const KEY_DOT: u32 = 52;
    const KEY_SLASH: u32 = 53;

    // (evdev_keycode, needs_shift)
    match cp {
        // Whitespace
        0x20 => Some((KEY_SPACE, false)),        // ' '
        0x09 => Some((KEY_TAB, false)),          // Tab
        0x0A | 0x0D => Some((KEY_ENTER, false)), // Newline / CR

        // Digits
        0x30 => Some((KEY_0, false)), // '0'
        0x31 => Some((KEY_1, false)), // '1'
        0x32 => Some((KEY_2, false)), // '2'
        0x33 => Some((KEY_3, false)), // '3'
        0x34 => Some((KEY_4, false)), // '4'
        0x35 => Some((KEY_5, false)), // '5'
        0x36 => Some((KEY_6, false)), // '6'
        0x37 => Some((KEY_7, false)), // '7'
        0x38 => Some((KEY_8, false)), // '8'
        0x39 => Some((KEY_9, false)), // '9'

        // Lowercase letters
        0x61 => Some((KEY_A, false)), // 'a'
        0x62 => Some((KEY_B, false)),
        0x63 => Some((KEY_C, false)),
        0x64 => Some((KEY_D, false)),
        0x65 => Some((KEY_E, false)),
        0x66 => Some((KEY_F, false)),
        0x67 => Some((KEY_G, false)),
        0x68 => Some((KEY_H, false)),
        0x69 => Some((KEY_I, false)),
        0x6A => Some((KEY_J, false)),
        0x6B => Some((KEY_K, false)),
        0x6C => Some((KEY_L, false)),
        0x6D => Some((KEY_M, false)),
        0x6E => Some((KEY_N, false)),
        0x6F => Some((KEY_O, false)),
        0x70 => Some((KEY_P, false)),
        0x71 => Some((KEY_Q, false)),
        0x72 => Some((KEY_R, false)),
        0x73 => Some((KEY_S, false)),
        0x74 => Some((KEY_T, false)),
        0x75 => Some((KEY_U, false)),
        0x76 => Some((KEY_V, false)),
        0x77 => Some((KEY_W, false)),
        0x78 => Some((KEY_X, false)),
        0x79 => Some((KEY_Y, false)),
        0x7A => Some((KEY_Z, false)), // 'z'

        // Uppercase letters (same keys, with Shift)
        0x41 => Some((KEY_A, true)), // 'A'
        0x42 => Some((KEY_B, true)),
        0x43 => Some((KEY_C, true)),
        0x44 => Some((KEY_D, true)),
        0x45 => Some((KEY_E, true)),
        0x46 => Some((KEY_F, true)),
        0x47 => Some((KEY_G, true)),
        0x48 => Some((KEY_H, true)),
        0x49 => Some((KEY_I, true)),
        0x4A => Some((KEY_J, true)),
        0x4B => Some((KEY_K, true)),
        0x4C => Some((KEY_L, true)),
        0x4D => Some((KEY_M, true)),
        0x4E => Some((KEY_N, true)),
        0x4F => Some((KEY_O, true)),
        0x50 => Some((KEY_P, true)),
        0x51 => Some((KEY_Q, true)),
        0x52 => Some((KEY_R, true)),
        0x53 => Some((KEY_S, true)),
        0x54 => Some((KEY_T, true)),
        0x55 => Some((KEY_U, true)),
        0x56 => Some((KEY_V, true)),
        0x57 => Some((KEY_W, true)),
        0x58 => Some((KEY_X, true)),
        0x59 => Some((KEY_Y, true)),
        0x5A => Some((KEY_Z, true)), // 'Z'

        // Symbols (unshifted)
        0x2D => Some((KEY_MINUS, false)),      // '-'
        0x3D => Some((KEY_EQUAL, false)),      // '='
        0x5B => Some((KEY_LEFTBRACE, false)),  // '['
        0x5D => Some((KEY_RIGHTBRACE, false)), // ']'
        0x5C => Some((KEY_BACKSLASH, false)),  // '\'
        0x3B => Some((KEY_SEMICOLON, false)),  // ';'
        0x27 => Some((KEY_APOSTROPHE, false)), // '\''
        0x60 => Some((KEY_GRAVE, false)),      // '`'
        0x2C => Some((KEY_COMMA, false)),      // ','
        0x2E => Some((KEY_DOT, false)),        // '.'
        0x2F => Some((KEY_SLASH, false)),      // '/'

        // Symbols (shifted)
        0x21 => Some((KEY_1, true)),          // '!'
        0x40 => Some((KEY_2, true)),          // '@'
        0x23 => Some((KEY_3, true)),          // '#'
        0x24 => Some((KEY_4, true)),          // '$'
        0x25 => Some((KEY_5, true)),          // '%'
        0x5E => Some((KEY_6, true)),          // '^'
        0x26 => Some((KEY_7, true)),          // '&'
        0x2A => Some((KEY_8, true)),          // '*'
        0x28 => Some((KEY_9, true)),          // '('
        0x29 => Some((KEY_0, true)),          // ')'
        0x5F => Some((KEY_MINUS, true)),      // '_'
        0x2B => Some((KEY_EQUAL, true)),      // '+'
        0x7B => Some((KEY_LEFTBRACE, true)),  // '{'
        0x7D => Some((KEY_RIGHTBRACE, true)), // '}'
        0x7C => Some((KEY_BACKSLASH, true)),  // '|'
        0x3A => Some((KEY_SEMICOLON, true)),  // ':'
        0x22 => Some((KEY_APOSTROPHE, true)), // '"'
        0x7E => Some((KEY_GRAVE, true)),      // '~'
        0x3C => Some((KEY_COMMA, true)),      // '<'
        0x3E => Some((KEY_DOT, true)),        // '>'
        0x3F => Some((KEY_SLASH, true)),      // '?'

        _ => None,
    }
}

/// Convert an RDP Unicode input code unit into an XKB keysym.
///
/// X11/XKB represents Unicode characters outside Latin-1 as `0x01000000 | codepoint`.
/// RDP Unicode input delivers UTF-16 code units; the current IronRDP server API exposes
/// each unit as `u16`, so supplementary-plane characters that require surrogate pairs
/// cannot be represented as a single keysym here.
fn unicode_to_keysym(cp: u16) -> Option<i32> {
    match cp {
        0xD800..=0xDFFF => None,
        0x0000..=0x001F | 0x007F..=0x009F => None,
        0x0020..=0x00FF => Some(i32::from(cp)),
        _ => Some((0x0100_0000u32 | u32::from(cp)) as i32),
    }
}

fn portal_err(e: impl std::fmt::Display) -> InputError {
    InputError::PortalError(e.to_string())
}

/// WRD Input Handler
///
/// Bridges IronRDP input events to our Portal-based input injection system.
/// This handler receives keyboard and mouse events from RDP clients and forwards
/// them to the Wayland compositor through the RemoteDesktop portal.
///
/// Since IronRDP's trait methods are synchronous but portal operations are async,
/// we use channels and spawned tasks to bridge the gap.
/// Input event for batching/multiplexing
#[derive(Debug)]
pub enum InputEvent {
    /// Keyboard event from RDP client
    Keyboard(IronKeyboardEvent),
    /// Mouse event from RDP client
    Mouse(IronMouseEvent),
}

/// WRD input handler that bridges IronRDP input events to Portal injection
///
/// Receives keyboard and mouse events from RDP clients and injects them
/// into the Wayland compositor via the Portal RemoteDesktop API.
pub struct LamcoInputHandler {
    /// Session handle for input injection (abstraction over Portal/Mutter)
    session_handle: Arc<dyn crate::session::SessionHandle>,

    /// Keyboard event handler (pub for multiplexer access)
    pub keyboard_handler: Arc<Mutex<KeyboardHandler>>,

    /// Mouse event handler (pub for multiplexer access)
    pub mouse_handler: Arc<Mutex<MouseHandler>>,

    /// Coordinate transformer for multi-monitor support (pub for multiplexer access)
    pub coordinate_transformer: Arc<Mutex<CoordinateTransformer>>,

    /// Primary stream node ID for input injection (PipeWire node ID)
    primary_stream_id: u32,

    /// Input event queue sender (for multiplexer - bounded with drop policy)
    input_tx: mpsc::Sender<InputEvent>,

    /// Display update channel used only for Android RD Client pointer workaround PDUs.
    pointer_update_tx: Option<Arc<Mutex<mpsc::Sender<DisplayUpdate>>>>,

    /// Shared EGFX capability state used to gate Android-only pointer workaround PDUs.
    gfx_handler_state: Option<Arc<RwLock<Option<HandlerState>>>>,

    /// Whether the Android workaround cursor bitmap was already sent this connection.
    pointer_shape_sent: Arc<AtomicBool>,

    /// Whether CJK clipboard-paste fallback is enabled (from config)
    cjk_paste_enabled: bool,

    /// Clipboard provider for writing text during CJK paste fallback
    clipboard_provider: Option<Arc<dyn ClipboardProvider>>,

    /// Flag to pause cooperation sync during CJK paste operations.
    /// When set to true, the cooperation event handler skips Klipper sync events.
    cjk_paste_paused: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl LamcoInputHandler {
    pub fn new(
        session_handle: Arc<dyn crate::session::SessionHandle>,
        monitors: Vec<MonitorInfo>,
        primary_stream_id: u32,
        input_tx: mpsc::Sender<InputEvent>,
        pointer_update_tx: Option<Arc<Mutex<mpsc::Sender<DisplayUpdate>>>>,
        gfx_handler_state: Option<Arc<RwLock<Option<HandlerState>>>>,
        mut input_rx: mpsc::Receiver<InputEvent>,
        mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
        cjk_paste_enabled: bool,
        clipboard_provider: Option<Arc<dyn ClipboardProvider>>,
        cjk_paste_paused: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Self, InputError> {
        let keyboard_handler = Arc::new(Mutex::new(KeyboardHandler::new()));
        let mouse_handler = Arc::new(Mutex::new(MouseHandler::new()));

        let coordinate_transformer = Arc::new(Mutex::new(CoordinateTransformer::new(monitors)?));

        debug!(
            "Input handler using PipeWire stream node ID: {}",
            primary_stream_id
        );

        // Start input batching task (10ms windows for responsive typing)
        // Receives from multiplexer input queue, batches, and sends to Portal
        let session_handle_clone = Arc::clone(&session_handle);
        let keyboard_clone = Arc::clone(&keyboard_handler);
        let mouse_clone = Arc::clone(&mouse_handler);
        let coord_clone = Arc::clone(&coordinate_transformer);
        let cjk_enabled_task = cjk_paste_enabled;
        let clipboard_provider_task = clipboard_provider.clone();
        let cjk_paste_paused_task = cjk_paste_paused.clone();
        let pointer_update_tx_task = pointer_update_tx.clone();
        let gfx_handler_state_task = gfx_handler_state.clone();
        let pointer_shape_sent = Arc::new(AtomicBool::new(false));
        let pointer_shape_sent_task = Arc::clone(&pointer_shape_sent);

        tokio::spawn(async move {
            let mut keyboard_batch = Vec::with_capacity(16);
            let mut mouse_batch = Vec::with_capacity(16);
            let mut last_flush = Instant::now();
            let batch_interval = tokio::time::Duration::from_millis(10);
            let mut cjk_buffer = CjkPasteBuffer::new();

            // Rate-limit input injection errors to avoid log spam when the
            // portal session becomes unresponsive (e.g. PipeWire stream pauses)
            let consecutive_mouse_errors = AtomicU64::new(0);
            let consecutive_kbd_errors = AtomicU64::new(0);

            loop {
                tokio::select! {
                    Some(event) = input_rx.recv() => {
                        match event {
                            InputEvent::Keyboard(kbd) => {
                                trace!("📥 Input queue: received keyboard event");
                                keyboard_batch.push(kbd);
                            }
                            InputEvent::Mouse(mouse) => {
                                trace!("📥 Input queue: received mouse event");
                                mouse_batch.push(mouse);
                            }
                        }
                    }

                    () = tokio::time::sleep_until(tokio::time::Instant::from_std(last_flush + batch_interval)) => {
                        // Process keyboard batch
                        if !keyboard_batch.is_empty() {
                            trace!("🔄 Input batching: flushing {} keyboard events", keyboard_batch.len());
                        }
                        for kbd_event in keyboard_batch.drain(..) {
                            if let Err(e) = Self::handle_keyboard_event_impl(
                                &session_handle_clone,
                                &keyboard_clone,
                                kbd_event,
                                &mut cjk_buffer,
                                cjk_enabled_task,
                                &clipboard_provider_task,
                                &cjk_paste_paused_task,
                            ).await {
                                let count = consecutive_kbd_errors.fetch_add(1, Ordering::Relaxed) + 1;
                                if count == 1 {
                                    warn!("Portal keyboard injection failed: {e}");
                                } else if count.is_power_of_two() {
                                    warn!("Portal keyboard injection failed ({count} consecutive): {e}");
                                }
                            } else {
                                let prev = consecutive_kbd_errors.swap(0, Ordering::Relaxed);
                                if prev > 1 {
                                    info!("Portal keyboard injection recovered after {prev} failures");
                                }
                            }
                        }

                        // Process mouse batch
                        if !mouse_batch.is_empty() {
                            trace!("🔄 Input batching: flushing {} mouse events", mouse_batch.len());
                        }
                        for mouse_event in mouse_batch.drain(..) {
                            if let Err(e) = Self::handle_mouse_event_impl(
                                &session_handle_clone,
                                &mouse_clone,
                                &coord_clone,
                                mouse_event,
                                primary_stream_id,
                                &pointer_update_tx_task,
                                &gfx_handler_state_task,
                                &pointer_shape_sent_task,
                            ).await {
                                let count = consecutive_mouse_errors.fetch_add(1, Ordering::Relaxed) + 1;
                                if count == 1 {
                                    warn!("Portal mouse injection failed: {e}");
                                } else if count.is_power_of_two() {
                                    warn!("Portal mouse injection failed ({count} consecutive): {e}");
                                }
                            } else {
                                let prev = consecutive_mouse_errors.swap(0, Ordering::Relaxed);
                                if prev > 1 {
                                    info!("Portal mouse injection recovered after {prev} failures");
                                }
                            }
                        }

                        last_flush = Instant::now();
                    }

                    _ = shutdown_rx.recv() => {
                        info!("🛑 Input batching task received shutdown signal");
                        break;
                    }
                }
            }

            let mouse_errs = consecutive_mouse_errors.load(Ordering::Relaxed);
            let kbd_errs = consecutive_kbd_errors.load(Ordering::Relaxed);
            if mouse_errs > 0 || kbd_errs > 0 {
                info!(
                    "Input batching task stopped (pending errors: mouse={mouse_errs}, kbd={kbd_errs})"
                );
            } else {
                info!("Input batching task stopped");
            }
        });

        info!("Input batching task started (REAL task, 10ms flush interval)");

        Ok(Self {
            session_handle,
            keyboard_handler,
            mouse_handler,
            coordinate_transformer,
            primary_stream_id,
            input_tx,
            pointer_update_tx,
            gfx_handler_state,
            pointer_shape_sent,
            cjk_paste_enabled,
            clipboard_provider,
            cjk_paste_paused,
        })
    }

    /// Notify input handler that client reconnected
    ///
    /// Resets internal state to handle new client connection.
    /// Call this when reconnection is detected (e.g., display_updates channel recreated).
    pub async fn notify_reconnection(&self) {
        info!("🔄 Input handler: Client reconnected, resetting state");

        {
            let mut kbd = self.keyboard_handler.lock().await;
            *kbd = KeyboardHandler::new();
            debug!("Keyboard handler state reset");
        }

        {
            let mut mouse = self.mouse_handler.lock().await;
            *mouse = MouseHandler::new();
            debug!("Mouse handler state reset");
        }

        self.pointer_shape_sent.store(false, Ordering::Release);
        debug!("Android pointer workaround state reset");

        info!("✅ Input handler ready for reconnected client");
    }

    /// Update coordinate transformer when monitor configuration changes
    ///
    /// This should be called when the RDP client requests a different resolution
    /// or when monitor configuration changes.
    pub async fn update_monitors(&self, monitors: Vec<MonitorInfo>) -> Result<(), InputError> {
        let mut transformer = self.coordinate_transformer.lock().await;
        *transformer = CoordinateTransformer::new(monitors)?;
        debug!("Updated monitor configuration");
        Ok(())
    }

    /// Handle keyboard event implementation (static for batching task)
    async fn handle_keyboard_event_impl(
        session_handle: &Arc<dyn crate::session::SessionHandle>,
        keyboard_handler: &Arc<Mutex<KeyboardHandler>>,
        event: IronKeyboardEvent,
        cjk_buffer: &mut CjkPasteBuffer,
        cjk_paste_enabled: bool,
        clipboard_provider: &Option<Arc<dyn ClipboardProvider>>,
        cjk_paste_paused: &Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<(), InputError> {
        let mut keyboard = keyboard_handler.lock().await;

        match event {
            IronKeyboardEvent::Pressed { code, extended } => {
                // Flush any buffered CJK text before a regular keycode event
                if !cjk_buffer.is_empty() {
                    drop(keyboard);
                    Self::flush_cjk_buffer(
                        session_handle,
                        cjk_buffer,
                        clipboard_provider,
                        cjk_paste_paused,
                    )
                    .await;
                    keyboard = keyboard_handler.lock().await;
                }

                // Log V key specifically to trace Ctrl+V paste operations
                if code == 0x2F {
                    // V key scancode
                    info!(
                        "⌨️ V key pressed (scancode=0x{:02X}, extended={})",
                        code, extended
                    );
                }
                debug!("Keyboard pressed: code={}, extended={}", code, extended);

                let kbd_event = keyboard.handle_key_down(code as u16, extended, false)?;

                let keycode = match kbd_event {
                    crate::input::KeyboardEvent::KeyDown { keycode, .. }
                    | crate::input::KeyboardEvent::KeyRepeat { keycode, .. } => keycode,
                    crate::input::KeyboardEvent::KeyUp { keycode, .. } => {
                        // handle_key_down returned KeyUp (shouldn't happen but handle gracefully)
                        warn!(
                            "handle_key_down returned KeyUp for code {} - using keycode anyway",
                            code
                        );
                        keycode
                    }
                    #[expect(
                        unreachable_patterns,
                        reason = "defensive: future KeyboardEvent variants"
                    )]
                    other => {
                        error!("handle_key_down returned unexpected event: {:?}", other);
                        return Err(InputError::InvalidKeyEvent(format!(
                            "Unexpected event type: {other:?}"
                        )));
                    }
                };

                // Log V key injection to Portal
                if keycode == 47 {
                    // evdev KEY_V
                    info!(
                        "⌨️ Injecting V key press to Portal (evdev keycode={})",
                        keycode
                    );
                }

                session_handle
                    .notify_keyboard_keycode(keycode as i32, true)
                    .await
                    .map_err(portal_err)?;
            }

            IronKeyboardEvent::Released { code, extended } => {
                // Log V key releases
                if code == 0x2F {
                    // V key scancode
                    info!(
                        "⌨️ V key released (scancode=0x{:02X}, extended={})",
                        code, extended
                    );
                }
                debug!("Keyboard released: code={}, extended={}", code, extended);

                let kbd_event = keyboard.handle_key_up(code as u16, extended, false)?;

                let keycode = match kbd_event {
                    crate::input::KeyboardEvent::KeyUp { keycode, .. } => keycode,
                    _ => {
                        return Err(InputError::InvalidKeyEvent(
                            "Unexpected event type".to_string(),
                        ));
                    }
                };

                // Log V key injection release to Portal
                if keycode == 47 {
                    // evdev KEY_V
                    info!(
                        "⌨️ Injecting V key release to Portal (evdev keycode={})",
                        keycode
                    );
                }

                session_handle
                    .notify_keyboard_keycode(keycode as i32, false)
                    .await
                    .map_err(portal_err)?;
            }

            IronKeyboardEvent::UnicodePressed(unicode) => {
                if let Some((keycode, needs_shift)) = unicode_to_evdev(unicode) {
                    debug!(
                        "Unicode press 0x{:04X} -> evdev {} (shift={})",
                        unicode, keycode, needs_shift
                    );
                    // KEY_LEFTSHIFT = 42
                    if needs_shift {
                        session_handle
                            .notify_keyboard_keycode(42, true)
                            .await
                            .map_err(portal_err)?;
                    }
                    session_handle
                        .notify_keyboard_keycode(keycode as i32, true)
                        .await
                        .map_err(portal_err)?;
                } else if cjk_paste_enabled {
                    // KDE/xdg-desktop-portal-kde cannot turn CJK Unicode keysyms into
                    // physical keycodes, so do not try keysym injection for non-ASCII
                    // Unicode here. Buffer the committed text and paste it immediately.
                    if (0xD800..=0xDBFF).contains(&unicode) {
                        cjk_buffer.pending_high_surrogate = Some(unicode);
                        debug!(
                            "Unicode press 0x{:04X}: stored high surrogate for CJK paste",
                            unicode
                        );
                    } else if (0xDC00..=0xDFFF).contains(&unicode) {
                        if let Some(high) = cjk_buffer.pending_high_surrogate.take() {
                            if cjk_buffer.push_surrogate_pair(high, unicode).is_some() {
                                info!(
                                    "Unicode press surrogate pair 0x{:04X}+0x{:04X}: buffered for CJK paste",
                                    high, unicode
                                );
                                drop(keyboard);
                                Self::flush_cjk_buffer(
                                    session_handle,
                                    cjk_buffer,
                                    clipboard_provider,
                                    cjk_paste_paused,
                                )
                                .await;
                            } else {
                                debug!(
                                    "Unicode press 0x{:04X}: invalid surrogate pair, discarding",
                                    unicode
                                );
                            }
                        } else {
                            debug!(
                                "Unicode press 0x{:04X}: lone low surrogate, discarding",
                                unicode
                            );
                        }
                    } else if let Some(c) = char::from_u32(u32::from(unicode)) {
                        cjk_buffer.push_char(c);
                        info!("Unicode press 0x{:04X}: buffered for CJK paste", unicode);
                        drop(keyboard);
                        Self::flush_cjk_buffer(
                            session_handle,
                            cjk_buffer,
                            clipboard_provider,
                            cjk_paste_paused,
                        )
                        .await;
                    } else {
                        debug!("Unicode press 0x{:04X}: no mapping, discarding", unicode);
                    }
                } else if let Some(keysym) = unicode_to_keysym(unicode) {
                    debug!(
                        "Unicode press 0x{:04X} -> XKB keysym 0x{:08X}",
                        unicode, keysym
                    );
                    session_handle
                        .notify_keyboard_keysym(keysym, true)
                        .await
                        .map_err(portal_err)?;
                } else {
                    debug!(
                        "Unicode press 0x{:04X}: no evdev or keysym mapping",
                        unicode
                    );
                }
            }

            IronKeyboardEvent::UnicodeReleased(unicode) => {
                // Skip release events for chars that were buffered into the CJK buffer
                // (they have no corresponding keycode to release)
                if unicode_to_evdev(unicode).is_some() {
                    debug!("Unicode release 0x{:04X} -> evdev", unicode);
                    if let Some((keycode, needs_shift)) = unicode_to_evdev(unicode) {
                        session_handle
                            .notify_keyboard_keycode(keycode as i32, false)
                            .await
                            .map_err(portal_err)?;
                        if needs_shift {
                            session_handle
                                .notify_keyboard_keycode(42, false)
                                .await
                                .map_err(portal_err)?;
                        }
                    }
                } else if let Some(keysym) = unicode_to_keysym(unicode) {
                    debug!(
                        "Unicode release 0x{:04X} -> XKB keysym 0x{:08X}",
                        unicode, keysym
                    );
                    session_handle
                        .notify_keyboard_keysym(keysym, false)
                        .await
                        .map_err(portal_err)?;
                } else {
                    // Buffered CJK character — no release event needed
                    debug!(
                        "Unicode release 0x{:04X}: skipping (buffered or unmapped)",
                        unicode
                    );
                }
            }

            IronKeyboardEvent::Synchronize(flags) => {
                debug!("Keyboard synchronize: {:?}", flags);
                // Update toggle key states based on sync flags
                // The flags tell us the client's current Caps/Num/Scroll lock states
                // We should sync our local state but portal doesn't have direct sync API
                // This is handled implicitly when keys are pressed
            }
        }

        Ok(())
    }

    /// Flush buffered CJK text via clipboard write + synthetic Ctrl+V.
    ///
    /// Pauses cooperation sync before writing to clipboard so Klipper's
    /// clipboardHistoryUpdated doesn't trigger "已复制" toasts on the client.
    /// Saves and restores the original clipboard content around the paste.
    async fn flush_cjk_buffer(
        session_handle: &Arc<dyn crate::session::SessionHandle>,
        cjk_buffer: &mut CjkPasteBuffer,
        clipboard_provider: &Option<Arc<dyn ClipboardProvider>>,
        cjk_paste_paused: &Option<Arc<std::sync::atomic::AtomicBool>>,
    ) {
        use std::sync::atomic::Ordering;

        let Some(text) = cjk_buffer.take_text() else {
            return;
        };
        let char_count = text.chars().count();

        if let Some(provider) = clipboard_provider {
            // Save current clipboard content so we can restore it after the paste.
            let saved_clipboard = provider.read_data("text/plain").await.ok();

            // Pause cooperation sync BEFORE writing to clipboard.
            // This prevents Klipper's clipboardHistoryUpdated from triggering
            // SendInitiateCopy back to the client (which shows "已复制" toast).
            if let Some(flag) = cjk_paste_paused {
                flag.store(true, Ordering::Release);
                debug!("CJK paste: cooperation sync paused");
            }

            if let Err(e) = provider.write_text(&text).await {
                warn!("CJK paste fallback: clipboard write failed: {e}");
            }

            // Allow clipboard to propagate before sending Ctrl+V
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

            // Ctrl+V: KEY_LEFTCTRL=29, KEY_V=47
            let send_key = |keycode: i32, pressed: bool| {
                let sh = Arc::clone(session_handle);
                async move {
                    if let Err(e) = sh.notify_keyboard_keycode(keycode, pressed).await {
                        warn!("CJK paste fallback: keycode inject failed: {e}");
                    }
                }
            };
            send_key(29, true).await;
            send_key(47, true).await;
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            send_key(47, false).await;
            send_key(29, false).await;

            // Wait for Ctrl+V to be processed before restoring clipboard
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

            // Restore original clipboard content
            if let Some(saved) = saved_clipboard {
                if let Ok(saved_text) = String::from_utf8(saved) {
                    if let Err(e) = provider.write_text(&saved_text).await {
                        warn!("CJK paste: clipboard restore failed: {e}");
                    } else {
                        debug!("CJK paste: clipboard content restored");
                    }
                }
            }

            // Wait for Klipper to process the restore write before resuming sync.
            // Without this, the Klipper event from the restore could arrive at the
            // cooperation handler AFTER we set paused=false, causing a spurious "已复制".
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

            // Resume cooperation sync after clipboard is restored and Klipper has settled
            if let Some(flag) = cjk_paste_paused {
                flag.store(false, Ordering::Release);
                debug!("CJK paste: cooperation sync resumed");
            }
        }

        info!("CJK paste fallback: flushed {char_count} chars via clipboard");
    }

    fn create_android_arrow_cursor() -> RGBAPointer {
        const W: u16 = 32;
        const H: u16 = 32;
        const HOT_X: u16 = 4;
        const HOT_Y: u16 = 27;
        let mut data = vec![0u8; usize::from(W) * usize::from(H) * 4];

        // Draw a simple Breeze-like left pointer, stored vertically flipped for
        // Microsoft RD Client on Android. Windows clients must not receive this
        // bitmap, because they render it with normal orientation.
        for y in 0..24u16 {
            for x in 0..=y.min(14) {
                let border = x == 0 || x == y.min(14) || y == 23;
                let src_y = H - 1 - y;
                let idx = (usize::from(src_y) * usize::from(W) + usize::from(x)) * 4;
                let (r, g, b, a) = if border {
                    (0, 0, 0, 255)
                } else {
                    (255, 255, 255, 255)
                };
                data[idx] = r;
                data[idx + 1] = g;
                data[idx + 2] = b;
                data[idx + 3] = a;
            }
        }

        RGBAPointer {
            cache_index: 0,
            width: W,
            height: H,
            hot_x: HOT_X,
            hot_y: HOT_Y,
            data,
        }
    }

    async fn needs_android_pointer_updates(
        gfx_handler_state: &Option<Arc<RwLock<Option<HandlerState>>>>,
    ) -> bool {
        let Some(state) = gfx_handler_state else {
            return false;
        };
        state
            .read()
            .await
            .as_ref()
            .is_some_and(|s| s.needs_android_pointer_updates)
    }

    async fn send_android_pointer_shape_once(
        pointer_update_tx: &Option<Arc<Mutex<mpsc::Sender<DisplayUpdate>>>>,
        gfx_handler_state: &Option<Arc<RwLock<Option<HandlerState>>>>,
        pointer_shape_sent: &Arc<AtomicBool>,
    ) {
        if !Self::needs_android_pointer_updates(gfx_handler_state).await {
            return;
        }
        if pointer_shape_sent
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        let Some(update_tx) = pointer_update_tx else {
            return;
        };
        let sender = update_tx.lock().await;
        if let Err(err) = sender.try_send(DisplayUpdate::RGBAPointer(
            Self::create_android_arrow_cursor(),
        )) {
            trace!("Dropping Android pointer shape update: {err}");
            pointer_shape_sent.store(false, Ordering::Release);
        } else {
            debug!("Sent Android RD Client pointer shape workaround");
        }
    }

    async fn send_android_pointer_position_update(
        pointer_update_tx: &Option<Arc<Mutex<mpsc::Sender<DisplayUpdate>>>>,
        gfx_handler_state: &Option<Arc<RwLock<Option<HandlerState>>>>,
        x: u16,
        y: u16,
    ) {
        if !Self::needs_android_pointer_updates(gfx_handler_state).await {
            return;
        }
        let Some(update_tx) = pointer_update_tx else {
            return;
        };

        let update = DisplayUpdate::PointerPosition(PointerPositionAttribute { x, y });
        let sender = update_tx.lock().await;
        if let Err(err) = sender.try_send(update) {
            trace!("Dropping Android pointer position update: {err}");
        }
    }

    /// Handle mouse event with full error handling and logging
    /// Handle mouse event implementation (static for batching task)
    async fn handle_mouse_event_impl(
        session_handle: &Arc<dyn crate::session::SessionHandle>,
        mouse_handler: &Arc<Mutex<MouseHandler>>,
        coordinate_transformer: &Arc<Mutex<CoordinateTransformer>>,
        event: IronMouseEvent,
        stream_id: u32,
        pointer_update_tx: &Option<Arc<Mutex<mpsc::Sender<DisplayUpdate>>>>,
        gfx_handler_state: &Option<Arc<RwLock<Option<HandlerState>>>>,
        pointer_shape_sent: &Arc<AtomicBool>,
    ) -> Result<(), InputError> {
        let mut mouse = mouse_handler.lock().await;
        let mut transformer = coordinate_transformer.lock().await;

        match event {
            IronMouseEvent::Move { x, y } => {
                debug!("Mouse move: x={}, y={}", x, y);

                let mouse_event =
                    mouse.handle_absolute_move(x as u32, y as u32, &mut transformer)?;

                let (stream_x, stream_y) = match mouse_event {
                    crate::input::MouseEvent::Move { x, y, .. } => (x, y),
                    _ => {
                        return Err(InputError::InvalidMouseEvent(
                            "Unexpected event type".to_string(),
                        ));
                    }
                };

                Self::send_android_pointer_shape_once(
                    pointer_update_tx,
                    gfx_handler_state,
                    pointer_shape_sent,
                )
                .await;
                Self::send_android_pointer_position_update(
                    pointer_update_tx,
                    gfx_handler_state,
                    x,
                    y,
                )
                .await;

                session_handle
                    .notify_pointer_motion_absolute(stream_id, stream_x, stream_y)
                    .await
                    .map_err(portal_err)?;
            }

            IronMouseEvent::RelMove { x, y } => {
                debug!("Mouse relative move: dx={}, dy={}", x, y);

                let mouse_event = mouse.handle_relative_move(x, y, &mut transformer)?;

                let (stream_x, stream_y) = match mouse_event {
                    crate::input::MouseEvent::Move { x, y, .. } => (x, y),
                    _ => {
                        return Err(InputError::InvalidMouseEvent(
                            "Unexpected event type".to_string(),
                        ));
                    }
                };

                // We converted relative to absolute already
                let pointer_x = stream_x.clamp(0.0, f64::from(u16::MAX)).round() as u16;
                let pointer_y = stream_y.clamp(0.0, f64::from(u16::MAX)).round() as u16;
                Self::send_android_pointer_shape_once(
                    pointer_update_tx,
                    gfx_handler_state,
                    pointer_shape_sent,
                )
                .await;
                Self::send_android_pointer_position_update(
                    pointer_update_tx,
                    gfx_handler_state,
                    pointer_x,
                    pointer_y,
                )
                .await;

                session_handle
                    .notify_pointer_motion_absolute(stream_id, stream_x, stream_y)
                    .await
                    .map_err(portal_err)?;
            }

            IronMouseEvent::LeftPressed => {
                mouse.handle_button_down(MouseButton::Left)?;
                session_handle
                    .notify_pointer_button(272, true) // BTN_LEFT
                    .await
                    .map_err(portal_err)?;
            }

            IronMouseEvent::LeftReleased => {
                mouse.handle_button_up(MouseButton::Left)?;
                session_handle
                    .notify_pointer_button(272, false)
                    .await
                    .map_err(portal_err)?;
            }

            IronMouseEvent::RightPressed => {
                mouse.handle_button_down(MouseButton::Right)?;
                session_handle
                    .notify_pointer_button(273, true) // BTN_RIGHT
                    .await
                    .map_err(portal_err)?;
            }

            IronMouseEvent::RightReleased => {
                mouse.handle_button_up(MouseButton::Right)?;
                session_handle
                    .notify_pointer_button(273, false)
                    .await
                    .map_err(portal_err)?;
            }

            IronMouseEvent::MiddlePressed => {
                mouse.handle_button_down(MouseButton::Middle)?;
                session_handle
                    .notify_pointer_button(274, true) // BTN_MIDDLE
                    .await
                    .map_err(portal_err)?;
            }

            IronMouseEvent::MiddleReleased => {
                mouse.handle_button_up(MouseButton::Middle)?;
                session_handle
                    .notify_pointer_button(274, false)
                    .await
                    .map_err(portal_err)?;
            }

            IronMouseEvent::Button4Pressed => {
                mouse.handle_button_down(MouseButton::Extra1)?;
                session_handle
                    .notify_pointer_button(275, true) // BTN_SIDE
                    .await
                    .map_err(portal_err)?;
            }

            IronMouseEvent::Button4Released => {
                mouse.handle_button_up(MouseButton::Extra1)?;
                session_handle
                    .notify_pointer_button(275, false)
                    .await
                    .map_err(portal_err)?;
            }

            IronMouseEvent::Button5Pressed => {
                mouse.handle_button_down(MouseButton::Extra2)?;
                session_handle
                    .notify_pointer_button(276, true) // BTN_EXTRA
                    .await
                    .map_err(portal_err)?;
            }

            IronMouseEvent::Button5Released => {
                mouse.handle_button_up(MouseButton::Extra2)?;
                session_handle
                    .notify_pointer_button(276, false)
                    .await
                    .map_err(portal_err)?;
            }

            IronMouseEvent::VerticalScroll { value } => {
                // RDP scroll units are in 120ths
                mouse.handle_scroll(0, value as i32)?;
                let delta_y = (value as f64 / 120.0) * 15.0;
                session_handle
                    .notify_pointer_axis(0.0, delta_y)
                    .await
                    .map_err(portal_err)?;
            }

            IronMouseEvent::Scroll { x, y } => {
                mouse.handle_scroll(x, y)?;
                let delta_x = (x as f64 / 120.0) * 15.0;
                let delta_y = (y as f64 / 120.0) * 15.0;
                session_handle
                    .notify_pointer_axis(delta_x, delta_y)
                    .await
                    .map_err(portal_err)?;
            }
        }

        Ok(())
    }
}

impl RdpServerInputHandler for LamcoInputHandler {
    fn keyboard(&mut self, event: IronKeyboardEvent) {
        trace!("⌨️  Input multiplexer: routing keyboard to queue");
        if let Err(e) = self.input_tx.try_send(InputEvent::Keyboard(event)) {
            error!("Failed to queue keyboard event for batching: {}", e);
        }
    }

    fn mouse(&mut self, event: IronMouseEvent) {
        trace!("🖱️  Input multiplexer: routing mouse to queue");
        if let Err(e) = self.input_tx.try_send(InputEvent::Mouse(event)) {
            error!("Failed to queue mouse event for batching: {}", e);
        }
    }
}

/// RdpServer needs ownership but we want to share state
impl Clone for LamcoInputHandler {
    fn clone(&self) -> Self {
        Self {
            session_handle: Arc::clone(&self.session_handle),
            keyboard_handler: Arc::clone(&self.keyboard_handler),
            mouse_handler: Arc::clone(&self.mouse_handler),
            coordinate_transformer: Arc::clone(&self.coordinate_transformer),
            primary_stream_id: self.primary_stream_id,
            input_tx: self.input_tx.clone(),
            pointer_update_tx: self.pointer_update_tx.clone(),
            gfx_handler_state: self.gfx_handler_state.clone(),
            pointer_shape_sent: Arc::clone(&self.pointer_shape_sent),
            cjk_paste_enabled: self.cjk_paste_enabled,
            clipboard_provider: self.clipboard_provider.clone(),
            cjk_paste_paused: self.cjk_paste_paused.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CjkPasteBuffer;
    use super::unicode_to_keysym;

    #[test]
    fn unicode_to_keysym_maps_bmp_cjk_to_xkb_unicode_keysym() {
        assert_eq!(unicode_to_keysym('中' as u16), Some(0x0100_4E2D));
        assert_eq!(unicode_to_keysym('文' as u16), Some(0x0100_6587));
    }

    #[test]
    fn unicode_to_keysym_keeps_latin1_keysyms_direct() {
        assert_eq!(unicode_to_keysym('é' as u16), Some(0x00E9));
    }

    #[test]
    fn unicode_to_keysym_rejects_surrogate_code_units() {
        assert_eq!(unicode_to_keysym(0xD83D), None);
        assert_eq!(unicode_to_keysym(0xDE00), None);
    }

    #[test]
    fn test_input_handler_clone() {
        // Verify clone compiles and works
        // Full tests require portal mocking
    }

    #[test]
    fn test_cjk_buffer_basic() {
        let mut buf = CjkPasteBuffer::new();
        buf.push_char('中');
        buf.push_char('文');
        buf.push_char('字');
        assert!(!buf.is_empty());
        assert_eq!(buf.take_text(), Some("中文字".to_string()));
        assert!(buf.is_empty());
        assert_eq!(buf.take_text(), None);
    }

    #[test]
    fn test_surrogate_pair() {
        let mut buf = CjkPasteBuffer::new();
        // U+1F600 GRINNING FACE: high=0xD83D, low=0xDE00
        let result = buf.push_surrogate_pair(0xD83D, 0xDE00);
        assert_eq!(result, Some('😀'));
        assert_eq!(buf.take_text(), Some("😀".to_string()));
    }

    #[test]
    fn test_buffer_empty_returns_none() {
        let mut buf = CjkPasteBuffer::new();
        assert!(buf.is_empty());
        assert_eq!(buf.take_text(), None);
    }
}
