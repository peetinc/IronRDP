//! Web Audio output for decoded RDPSND blocks, played on the audio rendering thread.
//!
//! Blocks go to an `AudioWorkletNode` running `playout_worklet.js`, which keeps them in a ring
//! buffer sized to the gaps it sees and plays them through one continuous resampler. On a WAN
//! link, or while graphics decoding holds the main thread, blocks reach this module in bursts:
//! the worklet keeps playing through the gaps. See
//! `docs/plans/2026-10-19-web-audio-playout-design.md`.
//!
//! [`PendingBlocks`] and [`DelayWindow`] hold the timing policy and are pure, so they are
//! unit-testable on the host.

use core::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use anyhow::anyhow;
use futures_util::StreamExt as _;
use js_sys::{Array, Float32Array, Object, Reflect};
use tracing::{debug, info, warn};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{AudioContext, AudioContextState, Window};

use super::codec::DecodedAudio;

/// The worklet module, loaded from a Blob URL unless the host page serves it itself
/// (`audio_worklet_url`), as a strict `script-src` requires.
const WORKLET_SOURCE: &str = include_str!("playout_worklet.js");

/// Name the module registers its processor under.
const PROCESSOR: &str = "rdpsnd-playout";

/// Most audio held on the main thread while the worklet module loads.
pub(crate) const PENDING_MAX_SECS: f64 = 2.0;

/// Interval of the main-thread heartbeat. How late it fires shows how long the main thread
/// was busy, which is what delays audio when graphics decoding runs there.
const HEARTBEAT_MS: u32 = 20;

/// The worklet posts stats every second; one summary is logged per this many.
const STATS_PER_SUMMARY: u32 = 10;

/// Whether `resume()` can still start this context. Anything but running or closed
/// (`suspended`, or a state web-sys does not model such as Safari's `interrupted`).
pub(crate) fn is_resumable(state: AudioContextState) -> bool {
    !matches!(state, AudioContextState::Running | AudioContextState::Closed)
}

/// Events that grant user activation, the only time browsers honor `resume()` for a
/// context the autoplay policy blocked ([HTML activation-triggering input events]).
///
/// [HTML activation-triggering input events]: https://html.spec.whatwg.org/multipage/interaction.html#activation-triggering-input-event
const GESTURE_EVENTS: [&str; 4] = ["keydown", "pointerdown", "pointerup", "touchend"];

/// Ask a context blocked by the autoplay policy to start. Fire-and-forget: a rejection
/// only means the browser wants a user gesture first, and the next gesture retries.
fn resume(context: &AudioContext) {
    if !is_resumable(context.state()) {
        return;
    }

    match context.resume() {
        Ok(promise) => wasm_bindgen_futures::spawn_local(async move {
            if let Err(error) = JsFuture::from(promise).await {
                debug!(?error, "AudioContext resume rejected");
            }
        }),
        Err(error) => debug!(?error, "AudioContext resume failed"),
    }
}

/// Blocks received before the worklet is ready, oldest first, holding at most
/// [`PENDING_MAX_SECS`] of audio.
#[derive(Debug, Default)]
pub(crate) struct PendingBlocks {
    blocks: VecDeque<DecodedAudio>,
    secs: f64,
}

impl PendingBlocks {
    pub(crate) fn push(&mut self, block: DecodedAudio) {
        self.secs += duration_secs(&block);
        self.blocks.push_back(block);
        while PENDING_MAX_SECS < self.secs {
            let Some(oldest) = self.blocks.pop_front() else {
                break;
            };
            self.secs -= duration_secs(&oldest);
        }
    }

    pub(crate) fn drain(&mut self) -> impl Iterator<Item = DecodedAudio> + '_ {
        self.secs = 0.0;
        self.blocks.drain(..)
    }

    pub(crate) fn secs(&self) -> f64 {
        self.secs
    }
}

fn duration_secs(block: &DecodedAudio) -> f64 {
    if block.sample_rate == 0 {
        return 0.0;
    }
    #[expect(
        clippy::as_conversions,
        clippy::cast_precision_loss,
        reason = "frame counts of audio blocks are far below f64's exact-integer range"
    )]
    let frames = block.frames() as f64;
    frames / f64::from(block.sample_rate)
}

/// End-to-end delay of RDPSND blocks, from the server's `audio_timestamp` (ms) to their
/// arrival here, relative to the lowest seen this session: how far behind the stream is.
#[derive(Debug, Default)]
pub(crate) struct DelayWindow {
    /// The server clock, unwrapped past u32 overflow.
    last_server: Option<u32>,
    server_base: u64,
    best: Option<f64>,
    current: f64,
    min: Option<f64>,
    max: f64,
}

impl DelayWindow {
    pub(crate) fn observe(&mut self, local_ms: f64, server_ts: u32) {
        if self.last_server.is_some_and(|last| server_ts < last) {
            self.server_base += 1 << 32;
        }
        self.last_server = Some(server_ts);
        #[expect(
            clippy::as_conversions,
            clippy::cast_precision_loss,
            reason = "milliseconds since the server clock started, far below 2^53"
        )]
        let server_clock_ms = (self.server_base + u64::from(server_ts)) as f64;

        let delay = local_ms - server_clock_ms;
        let best = self.best.map_or(delay, |best| best.min(delay));
        self.best = Some(best);
        let excess = delay - best;
        self.current = excess;
        self.min = Some(self.min.map_or(excess, |min| min.min(excess)));
        self.max = self.max.max(excess);
    }

    /// The newest block's delay above the best seen, in ms; `None` before the first block.
    pub(crate) fn latest(&self) -> Option<f64> {
        self.best.map(|_| self.current)
    }

    /// The window's (min, current, max) delay above the best seen, in ms; starts a new window.
    pub(crate) fn take(&mut self) -> (f64, f64, f64) {
        let window = (self.min.unwrap_or(self.current), self.current, self.max);
        self.min = None;
        self.max = self.current;
        window
    }
}

#[derive(Debug, Default)]
struct Diagnostics {
    delay: DelayWindow,
    heartbeat_late_ms_max: f64,
    stats_received: u32,
}

impl Diagnostics {
    /// Handles one stats message from the worklet: DEBUG every second, an INFO summary
    /// with the main-thread view every [`STATS_PER_SUMMARY`].
    fn on_worklet_stats(&mut self, stats: &JsValue) {
        let field = |name: &str| {
            Reflect::get(stats, &JsValue::from_str(name))
                .ok()
                .and_then(|value| value.as_f64())
                .unwrap_or(0.0)
        };
        let (target_ms, depth_mean, depth_min, depth_max) = (
            field("targetMs"),
            field("depthMsMean"),
            field("depthMsMin"),
            field("depthMsMax"),
        );
        let (underruns, gap_ms_max, dropped_ms, trimmed_ms) = (
            field("underruns"),
            field("gapMsMax"),
            field("droppedMs"),
            field("trimmedMs"),
        );
        debug!(target_ms, depth_mean, depth_min, depth_max, underruns, "Audio playout");

        self.stats_received += 1;
        if !self.stats_received.is_multiple_of(STATS_PER_SUMMARY) {
            return;
        }
        let (delay_min, delay_now, delay_max) = self.delay.take();
        let heartbeat_late_ms_max = core::mem::take(&mut self.heartbeat_late_ms_max);
        info!(
            target_ms,
            depth_mean,
            depth_min,
            depth_max,
            underruns,
            gap_ms_max,
            dropped_ms,
            trimmed_ms,
            delay_min,
            delay_now,
            delay_max,
            heartbeat_late_ms_max,
            "Audio playout stats"
        );
    }
}

#[derive(Debug)]
enum Node {
    /// The worklet module is loading; blocks wait here.
    Loading(PendingBlocks),
    Ready {
        node: AudioWorkletNode,
        _on_message: Closure<dyn FnMut(MessageEvent)>,
        _on_error: Closure<dyn FnMut(JsValue)>,
    },
    /// Loading failed; audio stays silent.
    Failed,
}

#[derive(Debug)]
pub(crate) struct Player {
    context: AudioContext,
    node: Rc<RefCell<Node>>,
    /// Set by the processor's error event: posting more blocks would be pointless.
    processor_failed: Rc<Cell<bool>>,
    diagnostics: Rc<RefCell<Diagnostics>>,
    /// Cleared on drop to stop the heartbeat.
    alive: Rc<Cell<bool>>,
    /// Whether playback already asked a suspended context to start. Later attempts come
    /// from [`GestureListener`], where browsers actually honor them.
    resume_attempted: bool,
    _gesture_listener: Option<GestureListener>,
}

/// Page-wide capture listener resuming the context on any user gesture, including ones
/// outside the session canvas. Removed on drop.
#[derive(Debug)]
struct GestureListener {
    window: Window,
    callback: Closure<dyn FnMut()>,
}

impl Drop for GestureListener {
    fn drop(&mut self) {
        for event in GESTURE_EVENTS {
            if let Err(error) = self.window.remove_event_listener_with_callback_and_bool(
                event,
                self.callback.as_ref().unchecked_ref(),
                true,
            ) {
                debug!(?error, event, "Failed to remove audio gesture listener");
            }
        }
    }
}

impl Player {
    /// A player whose worklet module is loaded from `worklet_url`, or from a Blob URL when
    /// `None` (fine unless the page's CSP restricts `script-src`).
    pub(crate) fn new(worklet_url: Option<String>) -> anyhow::Result<Self> {
        let context = AudioContext::new().map_err(|error| anyhow!("create AudioContext: {error:?}"))?;

        let gesture_listener = web_sys::window().map(|window| {
            let gesture_context = context.clone();
            let callback = Closure::<dyn FnMut()>::new(move || resume(&gesture_context));
            for event in GESTURE_EVENTS {
                // Capture phase, so a canvas handler stopping propagation cannot hide the gesture.
                if let Err(error) =
                    window.add_event_listener_with_callback_and_bool(event, callback.as_ref().unchecked_ref(), true)
                {
                    warn!(?error, event, "Failed to install audio gesture listener");
                }
            }
            GestureListener { window, callback }
        });

        let player = Self {
            context,
            node: Rc::new(RefCell::new(Node::Loading(PendingBlocks::default()))),
            processor_failed: Rc::new(Cell::new(false)),
            diagnostics: Rc::new(RefCell::new(Diagnostics::default())),
            alive: Rc::new(Cell::new(true)),
            resume_attempted: false,
            _gesture_listener: gesture_listener,
        };

        wasm_bindgen_futures::spawn_local(load_worklet(
            player.context.clone(),
            worklet_url,
            Rc::clone(&player.node),
            Rc::clone(&player.processor_failed),
            Rc::clone(&player.diagnostics),
        ));

        let (alive, diagnostics) = (Rc::clone(&player.alive), Rc::clone(&player.diagnostics));
        wasm_bindgen_futures::spawn_local(async move {
            let mut ticks = gloo_timers::future::IntervalStream::new(HEARTBEAT_MS);
            let mut last = now_ms();
            while ticks.next().await.is_some() && alive.get() {
                let now = now_ms();
                let late = now - last - f64::from(HEARTBEAT_MS);
                let mut diagnostics = diagnostics.borrow_mut();
                diagnostics.heartbeat_late_ms_max = diagnostics.heartbeat_late_ms_max.max(late);
                last = now;
            }
        });

        Ok(player)
    }

    /// Notes a block's arrival with the server's `audio_timestamp`, for the delay statistics.
    pub(crate) fn note_arrival(&mut self, server_ts: u32) {
        self.diagnostics.borrow_mut().delay.observe(now_ms(), server_ts);
    }

    pub(crate) fn enqueue(&mut self, audio: &DecodedAudio) {
        // Autoplay policy: a context created without user activation starts suspended.
        // Ask once here (enough when the page already had a gesture, e.g. the Connect
        // click); the gesture listener retries otherwise.
        if !self.resume_attempted {
            self.resume_attempted = true;
            resume(&self.context);
        }

        // While suspended (or interrupted) the worklet does not run: anything posted would
        // queue on its port without bound. Drop the block; playback starts fresh once it runs.
        if self.context.state() != AudioContextState::Running {
            return;
        }
        if audio.frames() == 0 || audio.sample_rate == 0 || self.processor_failed.get() {
            return;
        }

        match &mut *self.node.borrow_mut() {
            Node::Loading(pending) => pending.push(audio.clone()),
            Node::Ready { node, .. } => {
                let late_ms = self.diagnostics.borrow().delay.latest();
                if let Err(error) = post_block(node, audio, late_ms) {
                    debug!(?error, "Failed to post RDPSND block to the playout worklet");
                }
            }
            Node::Failed => {}
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.alive.set(false);
        if let Node::Ready { node, .. } = &*self.node.borrow() {
            // Detach the handlers before their closures go: the port can still deliver a
            // message after the context starts closing.
            node.port().set_onmessage(None);
            node.set_onprocessorerror(None);
            let _ = node.disconnect();
        }
        if let Err(error) = self.context.close() {
            debug!(?error, "Failed to close AudioContext");
        }
    }
}

/// Loads the worklet module, creates the playout node and flushes the blocks that arrived
/// meanwhile. Any failure leaves audio silent with one warning.
async fn load_worklet(
    context: AudioContext,
    worklet_url: Option<String>,
    node_slot: Rc<RefCell<Node>>,
    processor_failed: Rc<Cell<bool>>,
    diagnostics: Rc<RefCell<Diagnostics>>,
) {
    let result = async {
        let worklet: Worklet = Reflect::get(&context, &JsValue::from_str("audioWorklet"))
            .ok()
            .filter(|value| !value.is_undefined())
            .ok_or_else(|| JsValue::from_str("AudioWorklet is unavailable (secure context required)"))?
            .unchecked_into();

        let (url, blob_url) = match worklet_url {
            Some(url) => (url, None),
            None => {
                let options = Object::new();
                Reflect::set(
                    &options,
                    &JsValue::from_str("type"),
                    &JsValue::from_str("text/javascript"),
                )?;
                let blob = JsBlob::new(&Array::of1(&JsValue::from_str(WORKLET_SOURCE)), &options)?;
                let url = create_object_url(&blob)?;
                (url.clone(), Some(url))
            }
        };
        let loaded = JsFuture::from(worklet.add_module(&url)).await;
        if let Some(blob_url) = blob_url {
            revoke_object_url(&blob_url);
        }
        loaded?;

        let options = Object::new();
        Reflect::set(&options, &JsValue::from_str("numberOfInputs"), &JsValue::from(0))?;
        Reflect::set(&options, &JsValue::from_str("numberOfOutputs"), &JsValue::from(1))?;
        Reflect::set(
            &options,
            &JsValue::from_str("outputChannelCount"),
            &Array::of1(&JsValue::from(2)),
        )?;
        let node = AudioWorkletNode::new(&context, PROCESSOR, &options)?;
        node.connect(&context.destination())?;
        Ok::<_, JsValue>(node)
    }
    .await;

    let node = match result {
        Ok(node) => node,
        Err(error) => {
            warn!(
                ?error,
                "Audio playback unavailable: the playout worklet did not load (a page CSP must allow its URL in script-src; see audioWorkletUrl)"
            );
            *node_slot.borrow_mut() = Node::Failed;
            return;
        }
    };

    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
        let data = event.data();
        let is_stats = Reflect::get(&data, &JsValue::from_str("type"))
            .ok()
            .and_then(|kind| kind.as_string())
            .is_some_and(|kind| kind == "stats");
        if is_stats {
            diagnostics.borrow_mut().on_worklet_stats(&data);
        }
    });
    node.port().set_onmessage(Some(on_message.as_ref().unchecked_ref()));

    let on_error = Closure::<dyn FnMut(JsValue)>::new(move |error: JsValue| {
        warn!(?error, "Audio playout worklet failed; audio is silent");
        processor_failed.set(true);
    });
    node.set_onprocessorerror(Some(on_error.as_ref().unchecked_ref()));

    let mut slot = node_slot.borrow_mut();
    if let Node::Loading(pending) = &mut *slot {
        debug!(pending_secs = pending.secs(), "Audio playout worklet ready");
        for block in pending.drain() {
            // Their arrival times are gone; the worklet falls back to measuring outages.
            if let Err(error) = post_block(&node, &block, None) {
                debug!(?error, "Failed to post RDPSND block to the playout worklet");
            }
        }
    }
    *slot = Node::Ready {
        node,
        _on_message: on_message,
        _on_error: on_error,
    };
}

/// Posts one block as `{ type: 'block', rate, channels, lateMs }`, transferring the sample
/// buffers. `lateMs` is left out when the block's delay is unknown.
fn post_block(node: &AudioWorkletNode, audio: &DecodedAudio, late_ms: Option<f64>) -> Result<(), JsValue> {
    let channels = Array::new();
    let transfer = Array::new();
    for samples in &audio.channels {
        let array = Float32Array::from(samples.as_slice());
        transfer.push(&array.buffer());
        channels.push(&array);
    }
    let message = Object::new();
    Reflect::set(&message, &JsValue::from_str("type"), &JsValue::from_str("block"))?;
    Reflect::set(&message, &JsValue::from_str("rate"), &JsValue::from(audio.sample_rate))?;
    Reflect::set(&message, &JsValue::from_str("channels"), &channels)?;
    if let Some(late_ms) = late_ms {
        Reflect::set(&message, &JsValue::from_str("lateMs"), &JsValue::from(late_ms))?;
    }
    node.port().post_message(&message, &transfer)
}

fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|window| window.performance())
        .map_or(0.0, |performance| performance.now())
}

#[wasm_bindgen]
extern "C" {
    #[derive(Debug, Clone)]
    type Worklet;

    #[wasm_bindgen(method, js_name = addModule)]
    fn add_module(this: &Worklet, url: &str) -> js_sys::Promise;

    #[derive(Debug, Clone)]
    type AudioWorkletNode;

    #[wasm_bindgen(constructor, catch)]
    fn new(context: &AudioContext, name: &str, options: &Object) -> Result<AudioWorkletNode, JsValue>;

    #[wasm_bindgen(method, getter)]
    fn port(this: &AudioWorkletNode) -> MessagePort;

    #[wasm_bindgen(method, catch)]
    fn connect(this: &AudioWorkletNode, destination: &web_sys::AudioDestinationNode) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch)]
    fn disconnect(this: &AudioWorkletNode) -> Result<(), JsValue>;

    #[wasm_bindgen(method, setter = onprocessorerror)]
    fn set_onprocessorerror(this: &AudioWorkletNode, handler: Option<&js_sys::Function>);

    #[derive(Debug, Clone)]
    type MessagePort;

    #[wasm_bindgen(method, catch, js_name = postMessage)]
    fn post_message(this: &MessagePort, message: &JsValue, transfer: &Array) -> Result<(), JsValue>;

    #[wasm_bindgen(method, setter = onmessage)]
    fn set_onmessage(this: &MessagePort, handler: Option<&js_sys::Function>);

    #[derive(Debug, Clone)]
    type MessageEvent;

    #[wasm_bindgen(method, getter)]
    fn data(this: &MessageEvent) -> JsValue;

    #[wasm_bindgen(js_name = Blob)]
    type JsBlob;

    #[wasm_bindgen(constructor, catch, js_class = "Blob")]
    fn new(parts: &Array, options: &Object) -> Result<JsBlob, JsValue>;

    #[wasm_bindgen(js_namespace = URL, js_name = createObjectURL, catch)]
    fn create_object_url(blob: &JsBlob) -> Result<String, JsValue>;

    #[wasm_bindgen(js_namespace = URL, js_name = revokeObjectURL)]
    fn revoke_object_url(url: &str);
}

#[cfg(test)]
#[expect(
    clippy::float_cmp,
    reason = "the expected delays are exact sums of the integers fed in"
)]
mod tests {
    use super::*;

    fn block(sample_rate: u32, frames: usize) -> DecodedAudio {
        DecodedAudio {
            sample_rate,
            channels: vec![vec![0.0; frames]; 2],
        }
    }

    #[test]
    fn pending_blocks_keep_at_most_two_seconds() {
        let mut pending = PendingBlocks::default();
        // 23.2 ms blocks: 200 of them is 4.6 s.
        for _ in 0..200 {
            pending.push(block(44_100, 1024));
        }
        assert!(pending.secs() <= PENDING_MAX_SECS);
        assert!(PENDING_MAX_SECS - 0.03 < pending.secs(), "keeps the newest ~2 s");
        assert_eq!(pending.drain().count(), 86);
        assert_eq!(pending.secs(), 0.0);
    }

    #[test]
    fn delay_window_reports_delay_above_best_seen() {
        let mut window = DelayWindow::default();
        window.observe(1000.0, 500);
        window.observe(1100.0, 550); // 50 ms behind the best
        window.observe(1150.0, 650); // back to the best
        assert_eq!(window.take(), (0.0, 0.0, 50.0));

        window.observe(1300.0, 700); // 100 ms behind
        assert_eq!(window.take(), (100.0, 100.0, 100.0));
    }

    #[test]
    fn delay_window_latest_is_the_newest_delay_above_best() {
        let mut window = DelayWindow::default();
        assert_eq!(window.latest(), None);
        window.observe(1_000.0, 900);
        window.observe(1_500.0, 1_000);
        assert_eq!(window.latest(), Some(400.0));
        window.take();
        assert_eq!(window.latest(), Some(400.0));
    }

    #[test]
    fn delay_window_follows_the_server_clock_across_u32_wrap() {
        let mut window = DelayWindow::default();
        window.observe(1000.0, u32::MAX - 10);
        window.observe(1021.0, 10); // 21 ms later on both clocks, across the wrap
        assert_eq!(window.take(), (0.0, 0.0, 0.0));
    }

    #[test]
    fn resumable_states() {
        assert!(is_resumable(AudioContextState::Suspended));
        assert!(!is_resumable(AudioContextState::Running));
        assert!(!is_resumable(AudioContextState::Closed));
    }
}
