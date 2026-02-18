//! PipeWire video stream for screen casting
//!
//! Ported from niri's PipeWire stream implementation (`screencasting/pw_utils.rs`).
//! Implements PipeWire video streaming for screen sharing.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Cursor;
use std::mem::size_of;
use std::os::fd::AsRawFd;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::Duration;

use anyhow::Context as _;
use pipewire::properties::PropertiesBox;
use pipewire::spa::buffer::DataType;
use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pipewire::spa::param::format_utils::parse_format;
use pipewire::spa::param::video::{VideoFormat, VideoInfoRaw};
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::deserialize::PodDeserializer;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{self, ChoiceValue, Pod, PodPropFlags, Property, PropertyFlags};
use pipewire::spa::sys::*;
use pipewire::spa::utils::{
    Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Rectangle, SpaTypes,
};
use pipewire::stream::{Stream, StreamFlags, StreamListener, StreamRc, StreamState};
use pipewire::sys::pw_buffer;
use smithay::backend::allocator::dmabuf::{AsDmabuf, Dmabuf};
use smithay::backend::allocator::gbm::{GbmBuffer, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::RenderElement;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::sync::SyncPoint;
use smithay::output::{Output, OutputModeSource};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use smithay::reexports::gbm::Modifier;
use smithay::utils::{Physical, Scale, Size, Transform};
use tracing::{debug, info, trace, warn};
use zbus::object_server::SignalEmitter;

use super::PipeWire;
use crate::dbus::screen_cast;
use crate::State;

/// Allowance for frame timing - if delay is below this, proceed anyway
const CAST_DELAY_ALLOWANCE: Duration = Duration::from_micros(100);

/// Cast state machine: ResizePending → ConfirmationPending → Ready
#[derive(Debug)]
enum CastState {
    /// Waiting for PipeWire to negotiate format at the requested size.
    /// This is both the initial state and the state after output resize.
    ResizePending {
        pending_size: Size<u32, Physical>,
    },
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
        /// Damage tracker for skip-if-no-damage optimization (lazily initialized)
        damage_tracker: Option<OutputDamageTracker>,
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
    pub session_id: usize,
    // Listener is dropped before Stream to prevent a use-after-free.
    _listener: StreamListener<()>,
    pub stream: StreamRc,
    pub is_active: Rc<Cell<bool>>,
    pub size: Size<u32, Physical>,
    pub node_id: Rc<Cell<Option<u32>>>,
    /// The output name this cast is capturing
    pub output_name: String,
    state: Rc<RefCell<CastState>>,
    dmabufs: Rc<RefCell<HashMap<i64, Dmabuf>>>,
    /// Monotonic time of last frame capture
    pub last_frame_time: Duration,
    /// Minimum time between frames (set during format negotiation)
    min_time_between_frames: Rc<Cell<Duration>>,
    /// Flag indicating a fatal error occurred (e.g., signal emission failed)
    had_error: Rc<Cell<bool>>,
    /// Frame sequence counter for SPA_META_Header
    sequence_counter: u64,
    /// Cursor mode: 0=Hidden, 1=Embedded, 2=Metadata
    pub cursor_mode: u32,
    /// Render formats (modifier list) for renegotiation on resize
    render_formats: Vec<i64>,
    /// Output refresh rate for renegotiation on resize
    refresh: u32,
    /// Timer token for scheduled redraw (cancelled on next frame or cast stop)
    scheduled_redraw: Option<RegistrationToken>,
    /// Buffers dequeued from PipeWire awaiting GPU render completion.
    /// Stored oldest-first; completed buffers are queued back in order.
    /// Shared with PipeWire callbacks (remove_buffer cleans up on buffer removal).
    rendering_buffers: Rc<RefCell<Vec<(NonNull<pw_buffer>, SyncPoint)>>>,
}

impl Cast {
    /// Create a new screen cast stream
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pipewire: &PipeWire,
        event_loop: LoopHandle<'static, State>,
        session_id: usize,
        gbm: GbmDevice<DrmDeviceFd>,
        size: Size<i32, Physical>,
        refresh: u32,
        output_name: String,
        signal_ctx: SignalEmitter<'static>,
        render_formats: Vec<i64>,
        cursor_mode: u32,
    ) -> anyhow::Result<Self> {
        let size = Size::from((size.w as u32, size.h as u32));

        let stream = StreamRc::new(
            pipewire.core.clone(),
            "ewm-screen-cast",
            PropertiesBox::new(),
        )
        .context("error creating PipeWire stream")?;

        let node_id = Rc::new(Cell::new(None));
        let is_active = Rc::new(Cell::new(false));
        let state = Rc::new(RefCell::new(CastState::ResizePending { pending_size: size }));
        let dmabufs: Rc<RefCell<HashMap<i64, Dmabuf>>> = Rc::new(RefCell::new(HashMap::new()));
        let min_time_between_frames = Rc::new(Cell::new(Duration::ZERO));
        let had_error = Rc::new(Cell::new(false));
        let rendering_bufs: Rc<RefCell<Vec<(NonNull<pw_buffer>, SyncPoint)>>> =
            Rc::new(RefCell::new(Vec::new()));

        let node_id_clone = node_id.clone();
        let is_active_clone = is_active.clone();
        let had_error_clone = had_error.clone();
        let rendering_bufs_clone = rendering_bufs.clone();

        let state_clone = state.clone();
        let gbm_clone = gbm.clone();
        let min_time_between_frames_clone = min_time_between_frames.clone();

        let listener =
            stream
                .add_local_listener_with_user_data(())
                .state_changed(move |stream: &Stream, (), old, new| {
                    debug!("PipeWire stream state: {old:?} -> {new:?}");

                    match new {
                        StreamState::Paused => {
                            if node_id_clone.get().is_none() {
                                let id = stream.node_id();
                                info!("PipeWire stream paused, node_id: {id}");
                                node_id_clone.set(Some(id));

                                info!("Emitting PipeWireStreamAdded signal with node_id={}", id);
                                async_io::block_on(async {
                                    let res = screen_cast::Stream::pipe_wire_stream_added(
                                        &signal_ctx,
                                        id,
                                    )
                                    .await;

                                    if let Err(err) = res {
                                        warn!("Error sending PipeWireStreamAdded: {err:?}");
                                        had_error_clone.set(true);
                                    } else {
                                        info!("PipeWireStreamAdded signal emitted successfully");
                                    }
                                });
                            }
                            is_active_clone.set(false);
                        }
                        StreamState::Streaming => {
                            info!("PipeWire stream now streaming");
                            is_active_clone.set(true);
                        }
                        StreamState::Error(msg) => {
                            warn!("PipeWire stream error: {msg}");
                            is_active_clone.set(false);
                            had_error_clone.set(true);
                        }
                        _ => {}
                    }
                })
                .param_changed({
                    let state = state_clone.clone();
                    let had_error = had_error.clone();
                    let gbm = gbm_clone.clone();
                    let min_time_between_frames = min_time_between_frames_clone.clone();
                    let param_render_formats = render_formats.clone();
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

                        // Validate format size against expected size
                        let mut state_ref = state.borrow_mut();
                        if format_size != state_ref.expected_format_size() {
                            if !matches!(&*state_ref, CastState::ResizePending { .. }) {
                                warn!("unexpected format size, stopping cast");
                                had_error.set(true);
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
                            min_time_between_frames.set(min_frame_time);
                            debug!("min_time_between_frames set to {:?}", min_frame_time);
                        }

                        // Check if modifier needs fixation
                        let object = pod.as_object().unwrap();
                        let modifier_prop = object
                            .find_prop(pipewire::spa::utils::Id(FormatProperties::VideoModifier.0));

                        if let Some(prop) = modifier_prop {
                            if prop.flags().contains(PodPropFlags::DONT_FIXATE) {
                                debug!("Fixating modifier");

                                let pod_modifier = prop.value();
                                let Ok((_, modifiers)) =
                                    PodDeserializer::deserialize_from::<Choice<i64>>(
                                        pod_modifier.as_bytes(),
                                    )
                                else {
                                    warn!("wrong modifier property type");
                                    had_error.set(true);
                                    return;
                                };

                                let ChoiceEnum::Enum { alternatives, .. } = modifiers.1 else {
                                    warn!("wrong modifier choice type");
                                    had_error.set(true);
                                    return;
                                };

                                let fourcc = Fourcc::Xrgb8888;
                                let (modifier, plane_count) = match find_preferred_modifier(
                                    &gbm,
                                    format_size,
                                    fourcc,
                                    alternatives,
                                ) {
                                    Ok(x) => x,
                                    Err(err) => {
                                        warn!("couldn't find preferred modifier: {err:?}");
                                        had_error.set(true);
                                        return;
                                    }
                                };

                                debug!(
                                    "modifier fixated: {modifier:?}, plane_count: {plane_count}, \
                                     moving to confirmation pending"
                                );

                                *state_ref = CastState::ConfirmationPending {
                                    size: format_size,
                                    modifier,
                                    plane_count: plane_count as i32,
                                };
                                drop(state_ref);

                                // Offer fixated format first, original as fallback
                                let fixated_obj =
                                    make_video_params_fixated(format_size, refresh, modifier);
                                let fallback_obj = make_video_params(
                                    format_size,
                                    refresh,
                                    &param_render_formats,
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
                        }

                        // Modifier is already fixated — verify it matches if we're confirming
                        let modifier = Modifier::from(format.modifier());
                        let plane_count = match &*state_ref {
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
                                let fourcc = Fourcc::Xrgb8888;
                                let (_, pc) = match find_preferred_modifier(
                                    &gbm,
                                    format_size,
                                    fourcc,
                                    vec![format.modifier() as i64],
                                ) {
                                    Ok(x) => x,
                                    Err(err) => {
                                        warn!("test allocation failed: {err:?}");
                                        had_error.set(true);
                                        return;
                                    }
                                };
                                debug!("ready with modifier: {modifier:?}, plane_count: {pc}");
                                pc as i32
                            }
                        };

                        *state_ref = CastState::Ready {
                            size: format_size,
                            modifier,
                            plane_count,
                            damage_tracker: None,
                        };
                        drop(state_ref);

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
                            Property::new(
                                SPA_PARAM_BUFFERS_blocks,
                                pod::Value::Int(plane_count)
                            ),
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
                    let state = state_clone.clone();
                    let dmabufs = dmabufs.clone();
                    let gbm = gbm_clone.clone();
                    let add_buf_event_loop = event_loop.clone();
                    let add_buf_output_name = output_name.clone();
                    move |stream, (), buffer| {
                        let state_ref = state.borrow();
                        let CastState::Ready { size, modifier, .. } = &*state_ref else {
                            trace!("add_buffer but not ready yet");
                            return;
                        };
                        let size = *size;
                        let modifier = *modifier;
                        drop(state_ref);

                        trace!("add_buffer: size={size:?}, modifier={modifier:?}");

                        unsafe {
                            let spa_buffer = (*buffer).buffer;
                            let fourcc = Fourcc::Xrgb8888;

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
                            dmabufs.borrow_mut().insert(fd, dmabuf);
                        }

                        // During size re-negotiation, force a redraw once we got a newly sized buffer.
                        if dmabufs.borrow().len() == 1
                            && stream.state() == StreamState::Streaming
                        {
                            let output_name = add_buf_output_name.clone();
                            let _ = add_buf_event_loop.insert_source(
                                Timer::from_duration(Duration::ZERO),
                                move |_, _, state| {
                                    let output = state
                                        .ewm
                                        .space
                                        .outputs()
                                        .find(|o| o.name() == output_name)
                                        .cloned();
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
                    let dmabufs = dmabufs.clone();
                    let rendering_bufs = rendering_bufs_clone.clone();
                    move |_stream, (), buffer| {
                        trace!("remove_buffer");
                        rendering_bufs
                            .borrow_mut()
                            .retain(|(buf, _)| buf.as_ptr() != buffer);
                        unsafe {
                            let spa_buffer = (*buffer).buffer;
                            let spa_data = (*spa_buffer).datas;
                            if (*spa_buffer).n_datas > 0 {
                                let fd = (*spa_data).fd;
                                dmabufs.borrow_mut().remove(&fd);
                            }
                        }
                    }
                })
                .register()
                .context("error registering stream listener")?;

        // Create format parameters with available modifiers
        let mut buffer = Vec::new();
        let obj = make_video_params(size, refresh, &render_formats);
        let params = make_pod(&mut buffer, obj);

        stream
            .connect(
                Direction::Output,
                None,
                StreamFlags::DRIVER | StreamFlags::ALLOC_BUFFERS,
                &mut [params],
            )
            .context("error connecting stream")?;

        info!("PipeWire stream created for size {:?}", size);

        Ok(Self {
            event_loop,
            session_id,
            _listener: listener,
            stream,
            is_active,
            size,
            node_id,
            output_name,
            state,
            dmabufs,
            last_frame_time: Duration::ZERO,
            min_time_between_frames,
            had_error,
            sequence_counter: 0,
            cursor_mode,
            render_formats,
            refresh,
            scheduled_redraw: None,
            rendering_buffers: rendering_bufs,
        })
    }

    /// Check if the stream is actively streaming (and hasn't had a fatal error)
    pub fn is_streaming(&self) -> bool {
        self.is_active.get() && !self.had_error.get()
    }

    /// Check if the stream has encountered a fatal error
    pub fn has_error(&self) -> bool {
        self.had_error.get()
    }

    /// Check if the cast is not yet ready to render
    pub fn is_resize_pending(&self) -> bool {
        !matches!(*self.state.borrow(), CastState::Ready { .. })
    }

    /// Handle output size change by renegotiating with PipeWire.
    pub fn ensure_size(&mut self, new_size: Size<i32, Physical>, refresh: u32) {
        let new_size = Size::from((new_size.w as u32, new_size.h as u32));

        {
            let state = self.state.borrow();
            match &*state {
                CastState::Ready { size, .. } if *size == new_size => return,
                _ if state.pending_size() == Some(new_size) => return,
                _ => {}
            }
        }

        info!(
            output = %self.output_name,
            ?new_size,
            "output size changed, renegotiating PipeWire stream"
        );

        self.size = new_size;
        self.refresh = refresh;
        *self.state.borrow_mut() = CastState::ResizePending { pending_size: new_size };

        let obj = make_video_params(new_size, refresh, &self.render_formats);
        let mut buffer = Vec::new();
        let params = make_pod(&mut buffer, obj);

        if let Err(err) = self.stream.update_params(&mut [params]) {
            warn!("error updating stream params for resize: {err:?}");
        }
    }

    /// Compute extra delay needed before capturing next frame.
    fn compute_extra_delay(&self, target_frame_time: Duration) -> Duration {
        let last = self.last_frame_time;
        let min = self.min_time_between_frames.get();

        if last.is_zero() {
            trace!(?target_frame_time, ?last, "last is zero, recording first frame");
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

    /// Returns the delay before the next frame can be captured.
    /// Duration::ZERO means the frame can be captured now.
    pub fn frame_delay(&self, target_frame_time: Duration) -> Duration {
        let delay = self.compute_extra_delay(target_frame_time);
        if delay < CAST_DELAY_ALLOWANCE {
            Duration::ZERO
        } else {
            delay
        }
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

        self.rendering_buffers
            .borrow_mut()
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
                        if let Some(cast) = state.ewm.screen_casts.get_mut(&session_id) {
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
        let mut bufs = self.rendering_buffers.borrow_mut();

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

    /// Update the stream's refresh rate (renegotiate if changed).
    pub fn set_refresh(&mut self, refresh: u32) {
        if self.refresh == refresh {
            return;
        }

        debug!("cast FPS changed, updating stream FPS");
        self.refresh = refresh;

        let size = self.state.borrow().expected_format_size();
        let obj = make_video_params(size, refresh, &self.render_formats);
        let mut buffer = Vec::new();
        let params = make_pod(&mut buffer, obj);

        if let Err(err) = self.stream.update_params(&mut [params]) {
            warn!("error updating stream params for refresh: {err:?}");
        }
    }

    /// Dequeue a buffer, render to it, and queue it back.
    /// Returns true if a frame was rendered.
    pub fn dequeue_buffer_and_render<E>(
        &mut self,
        renderer: &mut GlesRenderer,
        elements: &[E],
        _size: Size<i32, Physical>,
        scale: Scale<f64>,
    ) -> bool
    where
        E: RenderElement<GlesRenderer>,
    {
        if !self.is_streaming() {
            return false;
        }

        // Get ready state and check damage
        let mut state = self.state.borrow_mut();
        let CastState::Ready {
            size: ready_size,
            damage_tracker,
            ..
        } = &mut *state
        else {
            trace!("dequeue_buffer_and_render: not ready yet");
            return false;
        };

        let size = Size::from((ready_size.w as i32, ready_size.h as i32));

        let dt = damage_tracker
            .get_or_insert_with(|| OutputDamageTracker::new(size, scale, Transform::Normal));

        let OutputModeSource::Static { scale: t_scale, .. } = dt.mode() else {
            unreachable!();
        };
        if *t_scale != scale {
            *dt = OutputDamageTracker::new(size, scale, Transform::Normal);
        }

        let (damage, _states) = dt.damage_output(1, elements).unwrap();
        if damage.is_none() {
            trace!("no damage, skipping PipeWire frame");
            return false;
        }
        trace!(
            element_count = elements.len(),
            damage_regions = ?damage.as_ref().map(|d| d.len()),
            "PipeWire frame has damage"
        );

        drop(state);

        // Use raw buffer API for direct spa_buffer access (following niri)
        let pw_buffer = unsafe { NonNull::new(self.stream.dequeue_raw_buffer()) };
        let Some(pw_buffer) = pw_buffer else {
            trace!("no available buffer in pw stream");
            return false;
        };

        unsafe {
            let spa_buf = (*pw_buffer.as_ptr()).buffer;
            let fd = (*(*spa_buf).datas).fd;
            let dmabufs = self.dmabufs.borrow();
            let Some(dmabuf) = dmabufs.get(&fd) else {
                warn!("dmabuf not found for fd {}", fd);
                return_unused_buffer(&self.stream, pw_buffer);
                return false;
            };
            let dmabuf = dmabuf.clone();
            drop(dmabufs);

            match crate::render::render_to_dmabuf(
                renderer,
                dmabuf,
                size,
                scale,
                Transform::Normal,
                elements.iter().rev(),
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

/// Mark buffer as successfully rendered with sequence metadata.
unsafe fn mark_buffer_as_good(spa_buf: *mut spa_buffer, sequence: &mut u64) {
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

/// Find the SPA_META_Header in a spa_buffer.
unsafe fn find_meta_header(buffer: *mut spa_buffer) -> Option<NonNull<spa_meta_header>> {
    let p = spa_buffer_find_meta_data(buffer, SPA_META_Header, size_of::<spa_meta_header>()).cast();
    NonNull::new(p)
}

/// Create video format parameters with available modifiers.
/// Sets DONT_FIXATE only when multiple modifiers are offered.
fn make_video_params(
    size: Size<u32, Physical>,
    refresh: u32,
    render_formats: &[i64],
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

    pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(FormatProperties::VideoFormat, Id, VideoFormat::BGRx),
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
) -> pod::Object {
    let modifier_val = u64::from(modifier) as i64;

    pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(FormatProperties::VideoFormat, Id, VideoFormat::BGRx),
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
        info!(output = %self.output_name, "Disconnecting PipeWire stream");
        if let Err(err) = self.stream.disconnect() {
            warn!(output = %self.output_name, "Error disconnecting PipeWire stream: {err:?}");
        }
    }
}
