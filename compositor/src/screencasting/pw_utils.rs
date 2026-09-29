//! Ported from niri's `src/screencasting/pw_utils.rs`. Owns the
//! PipeWire connection (`PipeWire`), the per-cast stream (`Cast`),
//! and the callback-driven format negotiation state machine.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Cursor;
use std::mem;
use std::mem::size_of;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::Duration;

use anyhow::Context as _;
use pipewire::context::ContextRc;
use pipewire::core::{CoreRc, PW_ID_CORE};
use pipewire::loop_::Timeout;
use pipewire::main_loop::MainLoopRc;
use pipewire::properties::PropertiesBox;
use pipewire::spa::buffer::DataType;
use pipewire::spa::param::ParamType;
use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pipewire::spa::param::format_utils::parse_format;
use pipewire::spa::param::video::{VideoFormat, VideoInfoRaw};
use pipewire::spa::pod::deserialize::PodDeserializer;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{self, ChoiceValue, Pod, PodPropFlags, Property, PropertyFlags};
use pipewire::spa::sys::*;
use pipewire::spa::utils::{
    Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Rectangle, SpaTypes,
};
use pipewire::stream::{Stream, StreamFlags, StreamListener, StreamRc, StreamState};
use pipewire::sys::pw_buffer;
use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::dmabuf::{AsDmabuf, Dmabuf};
use smithay::backend::allocator::gbm::{GbmBuffer, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::RenderElement;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::sync::SyncPoint;
use smithay::output::{Output, OutputModeSource};
use smithay::reexports::calloop::channel::Sender;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use smithay::reexports::gbm::Modifier;
use smithay::utils::{Physical, Point, Scale, Size, Transform};
use tracing::{debug, info, trace, warn};
use zbus::object_server::SignalEmitter;

use crate::State;
use crate::dbus::{CastTarget, CursorMode, screen_cast};

/// PipeWire connection state. Outer code drops and recreates this on
/// `PwToCompositor::FatalError`.
pub struct PipeWire {
    _context: ContextRc,
    pub core: CoreRc,
    pub token: RegistrationToken,
    event_loop: LoopHandle<'static, State>,
    to_compositor: Sender<PwToCompositor>,
}

/// Unique id for a screencast session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CastSessionId(u64);

impl CastSessionId {
    pub fn next() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for CastSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique id for a screencast stream within a session. EWM emits one
/// stream per session today, but the type stays distinct to match the
/// reference design and to keep a stream's identity stable if/when
/// multi-stream sessions are added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CastStreamId(u64);

impl CastStreamId {
    pub fn next() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for CastStreamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Events from PipeWire callbacks back to the compositor main thread.
#[derive(Debug)]
pub enum PwToCompositor {
    /// Core connection died (EPIPE on PW_ID_CORE). The compositor should
    /// stop active casts and drop the `PipeWire` instance; the next
    /// `StartCast` will lazy-reinit.
    FatalError,
    /// Stop the cast with this session id. Sent on stream errors that
    /// the compositor can't recover from at the callback site (e.g.
    /// `StreamState::Error`, signal-emit failures).
    StopCast { session_id: CastSessionId },
    /// Queue a redraw on the output of the cast with this session id.
    /// Sent when a stream first reaches `Streaming`, so the consumer
    /// receives an initial frame even if no other activity drives
    /// renders.
    Redraw { session_id: CastSessionId },
}

impl PipeWire {
    /// Initialize PipeWire and integrate with the calloop event loop.
    ///
    /// `to_compositor` is a persistent sender owned by the compositor;
    /// each `PipeWire` instance clones it. The receiver side stays
    /// registered across PipeWire reinits.
    pub fn new(
        event_loop: LoopHandle<'static, State>,
        to_compositor: Sender<PwToCompositor>,
    ) -> anyhow::Result<Self> {
        info!("Initializing PipeWire");

        let main_loop = MainLoopRc::new(None).context("error creating PipeWire MainLoop")?;
        let context =
            ContextRc::new(&main_loop, None).context("error creating PipeWire Context")?;
        let core = context
            .connect_rc(None)
            .context("error connecting to PipeWire")?;

        let to_compositor_listener = to_compositor.clone();
        let listener = core
            .add_listener_local()
            .error(move |id, seq, res, message| {
                warn!(id, seq, res, message, "PipeWire error");
                if id == PW_ID_CORE
                    && res == -32
                    && let Err(err) = to_compositor_listener.send(PwToCompositor::FatalError)
                {
                    warn!("error sending PipeWire FatalError: {err:?}");
                }
            })
            .register();
        // Listener leaks intentionally; captured Sender clone leaks too,
        // but the receiver outlives all senders.
        mem::forget(listener);

        struct MainLoopFd(MainLoopRc);
        impl AsFd for MainLoopFd {
            fn as_fd(&self) -> BorrowedFd<'_> {
                self.0.loop_().fd()
            }
        }

        let generic = Generic::new(MainLoopFd(main_loop), Interest::READ, Mode::Level);
        let token = event_loop
            .insert_source(generic, move |_, wrapper, _: &mut State| {
                wrapper.0.loop_().iterate(Timeout::None);
                Ok(PostAction::Continue)
            })
            .map_err(|e| anyhow::anyhow!("error inserting PipeWire source: {}", e))?;

        info!("PipeWire initialized successfully");

        Ok(Self {
            _context: context,
            core,
            token,
            event_loop,
            to_compositor,
        })
    }
}

/// Allowance for frame timing - if delay is below this, proceed anyway
const CAST_DELAY_ALLOWANCE: Duration = Duration::from_micros(100);

/// Result of `Cast::ensure_size`. Callers branch on this each frame:
/// `Ready` -> render now; `Pending` -> skip this frame, retry next time.
#[derive(PartialEq, Eq)]
pub enum CastSizeChange {
    Ready,
    Pending,
}

/// Cast state machine: ResizePending -> ConfirmationPending -> Ready
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum CastState {
    /// Waiting for PipeWire to negotiate format at the requested size.
    /// This is both the initial state and the state after output resize.
    ResizePending { pending_size: Size<u32, Physical> },
    /// Modifier fixated, waiting for PipeWire to confirm the chosen format.
    /// Only entered when DONT_FIXATE was set (multiple modifiers offered).
    ConfirmationPending {
        size: Size<u32, Physical>,
        modifier: Modifier,
        plane_count: i32,
    },
    /// Format confirmed, ready to stream.
    Ready {
        size: Size<u32, Physical>,
        modifier: Modifier,
        #[allow(dead_code)]
        plane_count: i32,
        /// Damage tracker for content elements (skip-if-no-damage optimization)
        damage_tracker: Option<OutputDamageTracker>,
        /// Separate damage tracker for cursor elements
        cursor_damage_tracker: Option<OutputDamageTracker>,
        /// Last cursor position for detecting cursor-only movement
        last_cursor_location: Option<Point<i32, Physical>>,
    },
}

impl CastState {
    fn pending_size(&self) -> Option<Size<u32, Physical>> {
        match self {
            CastState::ResizePending { pending_size } => Some(*pending_size),
            CastState::ConfirmationPending { size, .. } => Some(*size),
            CastState::Ready { .. } => None,
        }
    }

    fn expected_format_size(&self) -> Size<u32, Physical> {
        match self {
            CastState::ResizePending { pending_size } => *pending_size,
            CastState::ConfirmationPending { size, .. } => *size,
            CastState::Ready { size, .. } => *size,
        }
    }
}

/// A screen cast session
pub struct Cast {
    event_loop: LoopHandle<'static, State>,
    pub session_id: CastSessionId,
    // Listener is dropped before Stream to prevent a use-after-free.
    _listener: StreamListener<()>,
    pub stream: StreamRc,
    /// What this cast is capturing (output or window)
    pub target: CastTarget,
    /// Monotonic time of last frame capture
    pub last_frame_time: Duration,
    /// Frame sequence counter for SPA_META_Header
    sequence_counter: u64,
    pub cursor_mode: CursorMode,
    /// Whether the stream uses alpha (BGRA). True for layout-entry casts.
    alpha: bool,
    /// Render formats (modifier list) for renegotiation on resize
    render_formats: Vec<i64>,
    /// Timer token for scheduled redraw (cancelled on next frame or cast stop)
    scheduled_redraw: Option<RegistrationToken>,
    /// State shared with PipeWire callbacks. Wrapped in Rc<RefCell> so
    /// every closure clones the Rc and `borrow_mut`s the inner fields
    /// rather than each field being its own Rc<Cell>.
    inner: Rc<RefCell<CastInner>>,
}

/// Mutable Cast state shared across PipeWire callbacks.
struct CastInner {
    is_active: bool,
    node_id: Option<u32>,
    state: CastState,
    refresh: u32,
    min_time_between_frames: Duration,
    dmabufs: HashMap<i64, Dmabuf>,
    /// Buffers dequeued from PipeWire awaiting GPU render completion.
    /// Stored oldest-first; completed buffers are queued back in order.
    rendering_buffers: Vec<(NonNull<pw_buffer>, SyncPoint)>,
}

impl PipeWire {
    /// Create a new screen cast stream attached to this PipeWire core.
    #[allow(clippy::too_many_arguments)]
    pub fn start_cast(
        &self,
        session_id: CastSessionId,
        gbm: GbmDevice<DrmDeviceFd>,
        size: Size<i32, Physical>,
        refresh: u32,
        target: CastTarget,
        alpha: bool,
        signal_ctx: SignalEmitter<'static>,
        render_formats: Vec<i64>,
        cursor_mode: CursorMode,
    ) -> anyhow::Result<Cast> {
        let cursor_mode = if cursor_mode == CursorMode::Metadata {
            debug!("cursor metadata is not implemented yet; embedding cursor instead");
            CursorMode::Embedded
        } else {
            cursor_mode
        };

        let event_loop = self.event_loop.clone();
        let to_compositor = self.to_compositor.clone();
        let size = Size::from((size.w as u32, size.h as u32));

        let stream = StreamRc::new(self.core.clone(), "ewm-screen-cast", PropertiesBox::new())
            .context("error creating PipeWire stream")?;

        let inner = Rc::new(RefCell::new(CastInner {
            is_active: false,
            node_id: None,
            state: CastState::ResizePending { pending_size: size },
            refresh,
            min_time_between_frames: Duration::ZERO,
            dmabufs: HashMap::new(),
            rendering_buffers: Vec::new(),
        }));

        // Sent when the cast hits an unrecoverable error. State::on_pw_msg
        // calls Ewm::stop_cast which removes the cast and emits D-Bus
        // Closed.
        let stop_cast = {
            let to_compositor = to_compositor.clone();
            move || {
                if let Err(err) = to_compositor.send(PwToCompositor::StopCast { session_id }) {
                    warn!(%session_id, "error sending StopCast: {err:?}");
                }
            }
        };

        let to_compositor_clone = to_compositor.clone();
        let stop_cast_state_changed = stop_cast.clone();
        let gbm_clone = gbm.clone();

        let listener = stream
            .add_local_listener_with_user_data(())
            .state_changed({
                let inner = inner.clone();
                let stop_cast = stop_cast_state_changed;
                move |stream: &Stream, (), old, new| {
                    debug!("PipeWire stream state: {old:?} -> {new:?}");
                    let mut inner = inner.borrow_mut();

                    match new {
                        StreamState::Paused => {
                            if inner.node_id.is_none() {
                                let id = stream.node_id();
                                info!("PipeWire stream paused, node_id: {id}");
                                inner.node_id = Some(id);

                                info!("Emitting PipeWireStreamAdded signal with node_id={}", id);
                                async_io::block_on(async {
                                    let res = screen_cast::Stream::pipe_wire_stream_added(
                                        &signal_ctx,
                                        id,
                                    )
                                    .await;
                                    if let Err(err) = res {
                                        warn!("Error sending PipeWireStreamAdded: {err:?}");
                                        stop_cast();
                                    } else {
                                        info!("PipeWireStreamAdded signal emitted successfully");
                                    }
                                });
                            }
                            inner.is_active = false;
                        }
                        StreamState::Streaming => {
                            info!("PipeWire stream now streaming");
                            inner.is_active = true;
                            // Kick a redraw so the consumer gets an
                            // initial frame even if nothing else is
                            // driving renders on this output.
                            if let Err(err) =
                                to_compositor_clone.send(PwToCompositor::Redraw { session_id })
                            {
                                warn!("error sending Redraw to compositor: {err:?}");
                            }
                        }
                        StreamState::Error(msg) => {
                            warn!("PipeWire stream error: {msg}");
                            if inner.is_active {
                                inner.is_active = false;
                                stop_cast();
                            }
                        }
                        _ => {}
                    }
                }
            })
            .param_changed({
                let inner = inner.clone();
                let stop_cast = stop_cast.clone();
                let gbm = gbm_clone.clone();
                let param_render_formats = render_formats.clone();
                let param_alpha = alpha;
                move |stream: &Stream, (), id, pod| {
                    if ParamType::from_raw(id) != ParamType::Format {
                        return;
                    }

                    let Some(pod) = pod else { return };

                    let (m_type, m_subtype) = match parse_format(pod) {
                        Ok(x) => x,
                        Err(err) => {
                            warn!("error parsing format: {err:?}");
                            return;
                        }
                    };

                    if m_type != MediaType::Video || m_subtype != MediaSubtype::Raw {
                        return;
                    }

                    let mut format = VideoInfoRaw::new();
                    if let Err(err) = format.parse(pod) {
                        warn!("error parsing video format: {err:?}");
                        return;
                    }
                    debug!("PipeWire format: {format:?}");

                    let format_size = Size::from((format.size().width, format.size().height));

                    let mut inner = inner.borrow_mut();
                    let inner = &mut *inner;

                    // Validate format size against expected size
                    if format_size != inner.state.expected_format_size() {
                        if !matches!(&inner.state, CastState::ResizePending { .. }) {
                            warn!("unexpected format size, stopping cast");
                            stop_cast();
                            return;
                        }
                        debug!("wrong size during resize, waiting");
                        return;
                    }

                    // Extract max framerate and compute min_time_between_frames
                    let max_frame_rate = format.max_framerate();
                    if max_frame_rate.num > 0 {
                        let min_frame_time = Duration::from_micros(
                            1_000_000 * u64::from(max_frame_rate.denom)
                                / u64::from(max_frame_rate.num),
                        );
                        inner.min_time_between_frames = min_frame_time;
                        debug!("min_time_between_frames set to {:?}", min_frame_time);
                    }

                    // Check if modifier needs fixation
                    let object = pod.as_object().unwrap();
                    let modifier_prop = object
                        .find_prop(pipewire::spa::utils::Id(FormatProperties::VideoModifier.0));

                    if let Some(prop) = modifier_prop
                        && prop.flags().contains(PodPropFlags::DONT_FIXATE)
                    {
                        debug!("Fixating modifier");

                        let pod_modifier = prop.value();
                        let Ok((_, modifiers)) = PodDeserializer::deserialize_from::<Choice<i64>>(
                            pod_modifier.as_bytes(),
                        ) else {
                            warn!("wrong modifier property type");
                            stop_cast();
                            return;
                        };

                        let ChoiceEnum::Enum { alternatives, .. } = modifiers.1 else {
                            warn!("wrong modifier choice type");
                            stop_cast();
                            return;
                        };

                        let fourcc = if param_alpha {
                            Fourcc::Argb8888
                        } else {
                            Fourcc::Xrgb8888
                        };
                        let (modifier, plane_count) = match find_preferred_modifier(
                            &gbm,
                            format_size,
                            fourcc,
                            alternatives,
                        ) {
                            Ok(x) => x,
                            Err(err) => {
                                warn!("couldn't find preferred modifier: {err:?}");
                                stop_cast();
                                return;
                            }
                        };

                        debug!(
                            "modifier fixated: {modifier:?}, plane_count: {plane_count}, \
                                     moving to confirmation pending"
                        );

                        inner.state = CastState::ConfirmationPending {
                            size: format_size,
                            modifier,
                            plane_count: plane_count as i32,
                        };

                        // Offer fixated format first, original as fallback
                        let fixated_obj = make_video_params_fixated(
                            format_size,
                            inner.refresh,
                            modifier,
                            param_alpha,
                        );
                        let fallback_obj = make_video_params(
                            format_size,
                            inner.refresh,
                            &param_render_formats,
                            param_alpha,
                        );
                        let mut b1 = Vec::new();
                        let pod1 = make_pod(&mut b1, fixated_obj);
                        let mut b2 = Vec::new();
                        let pod2 = make_pod(&mut b2, fallback_obj);

                        if let Err(err) = stream.update_params(&mut [pod1, pod2]) {
                            warn!("error updating format params: {err:?}");
                        }
                        return;
                    }

                    // Modifier is already fixated; verify it matches if we're confirming
                    let modifier = Modifier::from(format.modifier());
                    let plane_count = match &inner.state {
                        CastState::ConfirmationPending {
                            modifier: expected_mod,
                            plane_count,
                            ..
                        } if *expected_mod == modifier => {
                            debug!("modifier confirmed, moving to ready");
                            *plane_count
                        }
                        _ => {
                            // First negotiation with single modifier, or modifier changed.
                            // Do a test allocation to validate.
                            let fourcc = if param_alpha {
                                Fourcc::Argb8888
                            } else {
                                Fourcc::Xrgb8888
                            };
                            let (_, pc) = match find_preferred_modifier(
                                &gbm,
                                format_size,
                                fourcc,
                                vec![format.modifier() as i64],
                            ) {
                                Ok(x) => x,
                                Err(err) => {
                                    warn!("test allocation failed: {err:?}");
                                    stop_cast();
                                    return;
                                }
                            };
                            debug!("ready with modifier: {modifier:?}, plane_count: {pc}");
                            pc as i32
                        }
                    };

                    inner.state = CastState::Ready {
                        size: format_size,
                        modifier,
                        plane_count,
                        damage_tracker: None,
                        cursor_damage_tracker: None,
                        last_cursor_location: None,
                    };

                    // Set buffer params + meta header
                    let buffer_obj = pod::object!(
                        SpaTypes::ObjectParamBuffers,
                        ParamType::Buffers,
                        Property::new(
                            SPA_PARAM_BUFFERS_buffers,
                            pod::Value::Choice(ChoiceValue::Int(Choice(
                                ChoiceFlags::empty(),
                                ChoiceEnum::Range {
                                    default: 8,
                                    min: 2,
                                    max: 16
                                }
                            ))),
                        ),
                        Property::new(SPA_PARAM_BUFFERS_blocks, pod::Value::Int(plane_count)),
                        Property::new(
                            SPA_PARAM_BUFFERS_dataType,
                            pod::Value::Choice(ChoiceValue::Int(Choice(
                                ChoiceFlags::empty(),
                                ChoiceEnum::Flags {
                                    default: 1 << DataType::DmaBuf.as_raw(),
                                    flags: vec![1 << DataType::DmaBuf.as_raw()],
                                },
                            ))),
                        ),
                    );

                    let meta_header_obj = pod::object!(
                        SpaTypes::ObjectParamMeta,
                        ParamType::Meta,
                        Property::new(
                            SPA_PARAM_META_type,
                            pod::Value::Id(pipewire::spa::utils::Id(SPA_META_Header)),
                        ),
                        Property::new(
                            SPA_PARAM_META_size,
                            pod::Value::Int(size_of::<spa_meta_header>() as i32),
                        ),
                    );

                    let mut b1 = Vec::new();
                    let pod1 = make_pod(&mut b1, buffer_obj);
                    let mut b2 = Vec::new();
                    let pod2 = make_pod(&mut b2, meta_header_obj);

                    if let Err(err) = stream.update_params(&mut [pod1, pod2]) {
                        warn!("error updating buffer params: {err:?}");
                    }
                }
            })
            .add_buffer({
                let inner = inner.clone();
                let gbm = gbm_clone.clone();
                let add_buf_event_loop = event_loop.clone();
                let add_buf_target = target.clone();
                let add_buf_alpha = alpha;
                move |stream, (), buffer| {
                    let mut inner = inner.borrow_mut();

                    let CastState::Ready { size, modifier, .. } = &inner.state else {
                        trace!("add_buffer but not ready yet");
                        return;
                    };
                    let size = *size;
                    let modifier = *modifier;

                    trace!("add_buffer: size={size:?}, modifier={modifier:?}");

                    unsafe {
                        let spa_buffer = (*buffer).buffer;
                        let fourcc = if add_buf_alpha {
                            Fourcc::Argb8888
                        } else {
                            Fourcc::Xrgb8888
                        };

                        let dmabuf = match allocate_dmabuf(&gbm, size, fourcc, modifier) {
                            Ok(d) => d,
                            Err(err) => {
                                warn!("error allocating dmabuf: {err:?}");
                                return;
                            }
                        };

                        let plane_count = dmabuf.num_planes();
                        assert_eq!((*spa_buffer).n_datas as usize, plane_count);

                        for (i, fd) in dmabuf.handles().enumerate() {
                            let spa_data = (*spa_buffer).datas.add(i);
                            assert!((*spa_data).type_ & (1 << DataType::DmaBuf.as_raw()) > 0);

                            (*spa_data).type_ = DataType::DmaBuf.as_raw();
                            (*spa_data).maxsize = 1;
                            (*spa_data).fd = fd.as_raw_fd() as i64;
                            (*spa_data).flags = SPA_DATA_FLAG_READWRITE;

                            let chunk = (*spa_data).chunk;
                            (*chunk).stride = dmabuf.strides().nth(i).unwrap_or(0) as i32;
                            (*chunk).offset = dmabuf.offsets().nth(i).unwrap_or(0);
                        }

                        let fd = (*(*spa_buffer).datas).fd;
                        let prev = inner.dmabufs.insert(fd, dmabuf);
                        // PipeWire shouldn't hand us the same fd twice
                        // for the same stream. The insert must always
                        // happen (not inside debug_assert!, which is
                        // a no-op in release builds and would silently
                        // drop the dmabuf - closing its fds before
                        // PipeWire serializes them to the consumer).
                        debug_assert!(prev.is_none());
                    }

                    // During size re-negotiation, force a redraw once we got a newly sized
                    // buffer.
                    if inner.dmabufs.len() == 1 && stream.state() == StreamState::Streaming {
                        let redraw_target = add_buf_target.clone();
                        let _ = add_buf_event_loop.insert_source(
                            Timer::from_duration(Duration::ZERO),
                            move |_, _, state| {
                                let output =
                                    state.ewm.screen_cast_output_for_target(&redraw_target);
                                if let Some(output) = output {
                                    state.ewm.queue_redraw(&output);
                                }
                                TimeoutAction::Drop
                            },
                        );
                    }
                }
            })
            .remove_buffer({
                let inner = inner.clone();
                move |_stream, (), buffer| {
                    trace!("remove_buffer");
                    let mut inner = inner.borrow_mut();
                    inner
                        .rendering_buffers
                        .retain(|(buf, _)| buf.as_ptr() != buffer);
                    unsafe {
                        let spa_buffer = (*buffer).buffer;
                        let spa_data = (*spa_buffer).datas;
                        if (*spa_buffer).n_datas > 0 {
                            let fd = (*spa_data).fd;
                            inner.dmabufs.remove(&fd);
                        }
                    }
                }
            })
            .register()
            .context("error registering stream listener")?;

        // Create format parameters with available modifiers
        let mut buffer = Vec::new();
        let obj = make_video_params(size, refresh, &render_formats, alpha);
        let params = make_pod(&mut buffer, obj);

        stream
            .connect(
                Direction::Output,
                None,
                StreamFlags::DRIVER | StreamFlags::ALLOC_BUFFERS,
                &mut [params],
            )
            .context("error connecting stream")?;

        info!("PipeWire stream created for {:?} size {:?}", target, size);

        Ok(Cast {
            event_loop,
            session_id,
            _listener: listener,
            stream,
            target,
            last_frame_time: Duration::ZERO,
            sequence_counter: 0,
            cursor_mode,
            alpha,
            render_formats,
            scheduled_redraw: None,
            inner,
        })
    }
}

fn frame_damage_tracker(
    tracker: &mut Option<OutputDamageTracker>,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
) -> &mut OutputDamageTracker {
    let tracker =
        tracker.get_or_insert_with(|| OutputDamageTracker::new(size, scale, Transform::Normal));

    let OutputModeSource::Static {
        scale: tracker_scale,
        ..
    } = tracker.mode()
    else {
        unreachable!();
    };
    if *tracker_scale != scale {
        *tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
    }

    tracker
}

impl Cast {
    /// Whether the stream has reached `Streaming`. Once an unrecoverable
    /// error occurs, the cast sends itself a `StopCast` and is removed
    /// from `Screencasting.casts`, so callers don't need a separate
    /// "errored" probe.
    pub fn is_active(&self) -> bool {
        self.inner.borrow().is_active
    }

    /// Renegotiate stream size if needed. Returns `Ready` when the stream
    /// is at `new_size` and ready to render, or `Pending` while waiting
    /// for PipeWire to confirm the resize. Use `set_refresh` separately
    /// to update the refresh rate.
    pub fn ensure_size(&self, new_size: Size<i32, Physical>) -> anyhow::Result<CastSizeChange> {
        let mut inner = self.inner.borrow_mut();
        let new_size = Size::from((new_size.w as u32, new_size.h as u32));

        match &inner.state {
            CastState::Ready { size, .. } if *size == new_size => {
                return Ok(CastSizeChange::Ready);
            }
            _ if inner.state.pending_size() == Some(new_size) => {
                return Ok(CastSizeChange::Pending);
            }
            _ => {}
        }

        info!(
            target = ?self.target,
            ?new_size,
            "size changed, renegotiating PipeWire stream"
        );

        inner.state = CastState::ResizePending {
            pending_size: new_size,
        };

        let obj = make_video_params(new_size, inner.refresh, &self.render_formats, self.alpha);
        let mut buffer = Vec::new();
        let params = make_pod(&mut buffer, obj);

        self.stream
            .update_params(&mut [params])
            .context("error updating stream params")?;

        Ok(CastSizeChange::Pending)
    }

    /// Compute extra delay needed before capturing next frame.
    fn compute_extra_delay(&self, target_frame_time: Duration) -> Duration {
        let last = self.last_frame_time;
        let min = self.inner.borrow().min_time_between_frames;

        if last.is_zero() {
            trace!(
                ?target_frame_time,
                ?last,
                "last is zero, recording first frame"
            );
            return Duration::ZERO;
        }

        if target_frame_time < last {
            warn!(
                ?target_frame_time,
                ?last,
                "target frame time is below last, did it overflow?"
            );
            return Duration::ZERO;
        }

        let diff = target_frame_time - last;
        if diff < min {
            let delay = min - diff;
            trace!(
                ?target_frame_time,
                ?last,
                "frame is too soon: min={min:?}, delay={delay:?}",
            );
            return delay;
        }

        Duration::ZERO
    }

    /// Check frame timing and schedule a redraw if too early.
    /// Returns true if the frame was delayed (caller should skip rendering).
    pub fn check_time_and_schedule(
        &mut self,
        output: &Output,
        target_frame_time: Duration,
    ) -> bool {
        let delay = self.compute_extra_delay(target_frame_time);
        if delay >= CAST_DELAY_ALLOWANCE {
            trace!("delay >= allowance, scheduling redraw");
            self.schedule_redraw(output.clone(), target_frame_time + delay);
            true
        } else {
            self.remove_scheduled_redraw();
            false
        }
    }

    /// Schedule a timer-based redraw for this cast's output.
    fn schedule_redraw(&mut self, output: Output, target_time: Duration) {
        if self.scheduled_redraw.is_some() {
            return;
        }

        let now = crate::utils::get_monotonic_time();
        let duration = target_time.saturating_sub(now);
        let timer = Timer::from_duration(duration);
        let token = self
            .event_loop
            .insert_source(timer, move |_, _, state| {
                if state.ewm.output_state.contains_key(&output) {
                    state.ewm.queue_redraw(&output);
                }
                TimeoutAction::Drop
            })
            .unwrap();
        self.scheduled_redraw = Some(token);
    }

    /// Cancel any pending scheduled redraw.
    fn remove_scheduled_redraw(&mut self) {
        if let Some(token) = self.scheduled_redraw.take() {
            self.event_loop.remove(token);
        }
    }

    /// Queue a rendered buffer back to PipeWire after GPU sync completes.
    ///
    /// If the sync fence FD can be exported, registers a calloop source that
    /// triggers when the GPU is done. Otherwise queues immediately.
    unsafe fn queue_after_sync(&mut self, pw_buffer: NonNull<pw_buffer>, sync_point: SyncPoint) {
        let mut sync_point = sync_point;
        let sync_fd = match sync_point.export() {
            Some(sync_fd) => Some(sync_fd),
            None => {
                // Either pre-signalled (no wait needed) or export failed.
                // Queue immediately rather than risk getting stuck.
                sync_point = SyncPoint::signaled();
                None
            }
        };

        self.inner
            .borrow_mut()
            .rendering_buffers
            .push((pw_buffer, sync_point));

        match sync_fd {
            None => {
                trace!("sync_fd is None, queueing completed buffers");
                self.queue_completed_buffers();
            }
            Some(sync_fd) => {
                trace!("scheduling buffer to queue after GPU sync");
                let session_id = self.session_id;
                let source = Generic::new(sync_fd, Interest::READ, Mode::OneShot);
                self.event_loop
                    .insert_source(source, move |_, _, state| {
                        if let Some(cast) = state
                            .ewm
                            .casting
                            .casts
                            .iter_mut()
                            .find(|c| c.session_id == session_id)
                        {
                            cast.queue_completed_buffers();
                        }
                        Ok(PostAction::Remove)
                    })
                    .unwrap();
            }
        }
    }

    /// Queue all completed (GPU-done) buffers back to PipeWire in order.
    fn queue_completed_buffers(&self) {
        let mut inner = self.inner.borrow_mut();
        let bufs = &mut inner.rendering_buffers;

        // Queue buffers in order up to the first still-rendering one.
        let first_in_progress = bufs
            .iter()
            .position(|(_, sync)| !sync.is_reached())
            .unwrap_or(bufs.len());

        for (buffer, _) in bufs.drain(..first_in_progress) {
            trace!("queueing completed buffer");
            unsafe {
                self.stream.queue_raw_buffer(buffer.as_ptr());
            }
        }
    }

    /// Update the stream's refresh rate, renegotiating if changed.
    pub fn set_refresh(&self, refresh: u32) -> anyhow::Result<()> {
        let mut inner = self.inner.borrow_mut();
        if inner.refresh == refresh {
            return Ok(());
        }

        debug!("cast FPS changed, updating stream FPS");
        inner.refresh = refresh;

        let size = inner.state.expected_format_size();
        let obj = make_video_params(size, refresh, &self.render_formats, self.alpha);
        let mut buffer = Vec::new();
        let params = make_pod(&mut buffer, obj);

        self.stream
            .update_params(&mut [params])
            .context("error updating stream params for refresh")?;
        Ok(())
    }

    /// Dequeue a buffer, render to it, and queue it back.
    ///
    /// Content and cursor elements are tracked separately for damage. When only
    /// the cursor position changes (e.g. mouse-follows-focus on each keystroke),
    /// the content damage tracker reports no damage, allowing the screencast to
    /// detect cursor-only updates efficiently.
    ///
    /// Returns true if a frame was rendered.
    pub fn dequeue_buffer_and_render<E>(
        &mut self,
        renderer: &mut GlesRenderer,
        content_elements: &[E],
        cursor_elements: &[E],
        cursor_location: Point<i32, Physical>,
        _size: Size<i32, Physical>,
        scale: Scale<f64>,
    ) -> bool
    where
        E: RenderElement<GlesRenderer>,
    {
        let mut inner = self.inner.borrow_mut();
        if !inner.is_active {
            return false;
        }

        // Get ready state and check damage
        let CastState::Ready {
            size: ready_size,
            damage_tracker,
            cursor_damage_tracker,
            last_cursor_location,
            ..
        } = &mut inner.state
        else {
            trace!("dequeue_buffer_and_render: not ready yet");
            return false;
        };

        let size = Size::from((ready_size.w as i32, ready_size.h as i32));

        let dt = frame_damage_tracker(damage_tracker, size, scale);
        let (content_damage, _states) = dt.damage_output(1, content_elements).unwrap();

        let (has_cursor_update, cursor_damage_len, cursor_moved) =
            if self.cursor_mode.includes_cursor() {
                let cursor_dt = frame_damage_tracker(cursor_damage_tracker, size, scale);
                let (cursor_damage, _) = cursor_dt.damage_output(1, cursor_elements).unwrap();
                let cursor_moved = *last_cursor_location != Some(cursor_location);
                *last_cursor_location = Some(cursor_location);

                (
                    cursor_damage.is_some() || cursor_moved,
                    cursor_damage.as_ref().map(|d| d.len()),
                    cursor_moved,
                )
            } else {
                *last_cursor_location = None;
                (false, None, false)
            };

        if content_damage.is_none() && !has_cursor_update {
            trace!("no damage, skipping PipeWire frame");
            return false;
        }
        trace!(
            content_count = content_elements.len(),
            cursor_count = cursor_elements.len(),
            content_damage = ?content_damage.as_ref().map(|d| d.len()),
            cursor_damage = ?cursor_damage_len,
            cursor_moved,
            "PipeWire frame has damage"
        );

        // Use raw buffer API for direct spa_buffer access
        let pw_buffer = unsafe { NonNull::new(self.stream.dequeue_raw_buffer()) };
        let Some(pw_buffer) = pw_buffer else {
            trace!("no available buffer in pw stream");
            return false;
        };

        unsafe {
            let spa_buf = (*pw_buffer.as_ptr()).buffer;
            let fd = (*(*spa_buf).datas).fd;
            let Some(dmabuf) = inner.dmabufs.get(&fd) else {
                warn!("dmabuf not found for fd {}", fd);
                return_unused_buffer(&self.stream, pw_buffer);
                return false;
            };
            let dmabuf = dmabuf.clone();
            drop(inner);

            // Chain cursor elements (front) onto content elements for rendering
            match crate::render::render_to_dmabuf(
                renderer,
                dmabuf,
                size,
                scale,
                Transform::Normal,
                cursor_elements.iter().chain(content_elements.iter()).rev(),
            ) {
                Ok(sync_point) => {
                    mark_buffer_as_good(spa_buf, &mut self.sequence_counter);
                    self.queue_after_sync(pw_buffer, sync_point);
                    true
                }
                Err(err) => {
                    warn!("error rendering to dmabuf: {err:?}");
                    return_unused_buffer(&self.stream, pw_buffer);
                    false
                }
            }
        }
    }
}

/// Mark a buffer as corrupted and queue it back to avoid starving PipeWire's pool.
unsafe fn return_unused_buffer(stream: &StreamRc, buf: NonNull<pw_buffer>) {
    unsafe {
        let buf = buf.as_ptr();
        let spa_buf = (*buf).buffer;
        let chunk = (*(*spa_buf).datas).chunk;
        // Some consumers check for size == 0 instead of the CORRUPTED flag.
        (*chunk).size = 0;
        (*chunk).flags = SPA_CHUNK_FLAG_CORRUPTED as i32;

        if let Some(header) = find_meta_header(spa_buf) {
            let header = header.as_ptr();
            (*header).flags = SPA_META_HEADER_FLAG_CORRUPTED;
        }

        stream.queue_raw_buffer(buf);
    }
}

/// Mark buffer as successfully rendered with sequence metadata.
unsafe fn mark_buffer_as_good(spa_buf: *mut spa_buffer, sequence: &mut u64) {
    unsafe {
        let chunk = (*(*spa_buf).datas).chunk;
        // OBS checks for size != 0 as a workaround, so set to 1.
        (*chunk).size = 1;
        (*chunk).flags = SPA_CHUNK_FLAG_NONE as i32;

        *sequence = sequence.wrapping_add(1);
        if let Some(header) = find_meta_header(spa_buf) {
            let header = header.as_ptr();
            (*header).pts = crate::utils::get_monotonic_time().as_nanos() as i64;
            (*header).flags = 0;
            (*header).seq = *sequence;
            (*header).dts_offset = 0;
        }
    }
}

/// Find the SPA_META_Header in a spa_buffer.
unsafe fn find_meta_header(buffer: *mut spa_buffer) -> Option<NonNull<spa_meta_header>> {
    unsafe {
        let p =
            spa_buffer_find_meta_data(buffer, SPA_META_Header, size_of::<spa_meta_header>()).cast();
        NonNull::new(p)
    }
}

/// Create video format parameters with available modifiers.
/// Sets DONT_FIXATE only when multiple modifiers are offered.
fn make_video_params(
    size: Size<u32, Physical>,
    refresh: u32,
    render_formats: &[i64],
    alpha: bool,
) -> pod::Object {
    let default = render_formats
        .first()
        .copied()
        .unwrap_or(u64::from(Modifier::Linear) as i64);
    let alternatives: Vec<i64> = render_formats.to_vec();

    let dont_fixate = if alternatives.len() > 1 {
        PropertyFlags::DONT_FIXATE
    } else {
        PropertyFlags::empty()
    };

    // Window casts use BGRA (alpha) for transparency; output casts use BGRx
    let video_format = if alpha {
        VideoFormat::BGRA
    } else {
        VideoFormat::BGRx
    };

    pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(FormatProperties::VideoFormat, Id, video_format),
        Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY | dont_fixate,
            value: pod::Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default,
                    alternatives,
                }
            )))
        },
        pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle {
                width: size.w,
                height: size.h,
            }
        ),
        pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            Fraction { num: 0, denom: 1 }
        ),
        pod::property!(
            FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            Fraction {
                num: refresh,
                denom: 1
            },
            Fraction { num: 1, denom: 1 },
            Fraction {
                num: refresh,
                denom: 1
            }
        ),
    )
}

/// Create fixated video format params (single modifier, no DONT_FIXATE)
fn make_video_params_fixated(
    size: Size<u32, Physical>,
    refresh: u32,
    modifier: Modifier,
    alpha: bool,
) -> pod::Object {
    let modifier_val = u64::from(modifier) as i64;
    let video_format = if alpha {
        VideoFormat::BGRA
    } else {
        VideoFormat::BGRx
    };

    pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(FormatProperties::VideoFormat, Id, video_format),
        Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY,
            value: pod::Value::Long(modifier_val)
        },
        pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle {
                width: size.w,
                height: size.h,
            }
        ),
        pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            Fraction { num: 0, denom: 1 }
        ),
        pod::property!(
            FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            Fraction {
                num: refresh,
                denom: 1
            },
            Fraction { num: 1, denom: 1 },
            Fraction {
                num: refresh,
                denom: 1
            }
        ),
    )
}

fn make_pod(buffer: &mut Vec<u8>, object: pod::Object) -> &Pod {
    PodSerializer::serialize(Cursor::new(&mut *buffer), &pod::Value::Object(object)).unwrap();
    Pod::from_bytes(buffer).unwrap()
}

fn find_preferred_modifier(
    gbm: &GbmDevice<DrmDeviceFd>,
    size: Size<u32, Physical>,
    fourcc: Fourcc,
    modifiers: Vec<i64>,
) -> anyhow::Result<(Modifier, usize)> {
    debug!("find_preferred_modifier: size={size:?}, fourcc={fourcc}, modifiers={modifiers:?}");

    let (buffer, modifier) = allocate_buffer(gbm, size, fourcc, &modifiers)?;

    match buffer.export() {
        Ok(dmabuf) => Ok((modifier, dmabuf.num_planes())),
        Err(err) if modifiers.len() > 1 => {
            // Tiled modifiers can produce multi-FD buffers that Smithay can't export.
            // Fall back to Linear which always produces a single FD.
            debug!("export failed with {modifier:?}: {err}, falling back to Linear");
            let linear = &[u64::from(Modifier::Linear) as i64];
            let (buffer, modifier) = allocate_buffer(gbm, size, fourcc, linear)?;
            let dmabuf = buffer
                .export()
                .context("error exporting GBM buffer as dmabuf")?;
            Ok((modifier, dmabuf.num_planes()))
        }
        Err(err) => Err(err).context("error exporting GBM buffer as dmabuf"),
    }
}

fn allocate_buffer(
    gbm: &GbmDevice<DrmDeviceFd>,
    size: Size<u32, Physical>,
    fourcc: Fourcc,
    modifiers: &[i64],
) -> anyhow::Result<(GbmBuffer, Modifier)> {
    let (w, h) = (size.w, size.h);
    let flags = GbmBufferFlags::RENDERING;

    if modifiers.len() == 1 && Modifier::from(modifiers[0] as u64) == Modifier::Invalid {
        let bo = gbm
            .create_buffer_object::<()>(w, h, fourcc, flags)
            .context("error creating GBM buffer object")?;

        let buffer = GbmBuffer::from_bo(bo, true);
        Ok((buffer, Modifier::Invalid))
    } else {
        let modifiers = modifiers
            .iter()
            .map(|m| Modifier::from(*m as u64))
            .filter(|m| *m != Modifier::Invalid);

        let bo = gbm
            .create_buffer_object_with_modifiers2::<()>(w, h, fourcc, modifiers, flags)
            .context("error creating GBM buffer object with modifiers")?;

        let modifier = bo.modifier();
        let buffer = GbmBuffer::from_bo(bo, false);
        Ok((buffer, modifier))
    }
}

fn allocate_dmabuf(
    gbm: &GbmDevice<DrmDeviceFd>,
    size: Size<u32, Physical>,
    fourcc: Fourcc,
    modifier: Modifier,
) -> anyhow::Result<Dmabuf> {
    let (buffer, _) = allocate_buffer(gbm, size, fourcc, &[u64::from(modifier) as i64])?;
    let dmabuf = buffer
        .export()
        .context("error exporting GBM buffer as dmabuf")?;
    Ok(dmabuf)
}

impl Drop for Cast {
    fn drop(&mut self) {
        self.remove_scheduled_redraw();
        info!(target = ?self.target, "Disconnecting PipeWire stream");
        if let Err(err) = self.stream.disconnect() {
            warn!(target = ?self.target, "Error disconnecting PipeWire stream: {err:?}");
        }
    }
}
