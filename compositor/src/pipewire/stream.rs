//! PipeWire video stream for screen casting
//!
//! This module implements PipeWire video streaming for screen sharing.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Cursor;
use std::os::fd::AsRawFd;
use std::rc::Rc;

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
use std::time::Duration;

use smithay::backend::allocator::dmabuf::{AsDmabuf, Dmabuf};
use smithay::backend::allocator::gbm::{GbmBuffer, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::RenderElement;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::output::OutputModeSource;
use smithay::reexports::gbm::Modifier;
use smithay::utils::{Physical, Scale, Size, Transform};
use tracing::{debug, info, trace, warn};
use zbus::object_server::SignalEmitter;

use super::PipeWire;
use crate::dbus::screen_cast;

/// Allowance for frame timing - if delay is below this, proceed anyway
const CAST_DELAY_ALLOWANCE: Duration = Duration::from_micros(100);

/// Cast state machine
#[derive(Debug)]
enum CastState {
    /// Waiting for format negotiation
    Pending,
    /// Format negotiated, ready to stream
    Ready {
        size: Size<u32, Physical>,
        modifier: Modifier,
        #[allow(dead_code)] // Stored for potential future use/debugging
        plane_count: i32,
        /// Damage tracker for skip-if-no-damage optimization (lazily initialized)
        damage_tracker: Option<OutputDamageTracker>,
    },
}

/// A screen cast session
pub struct Cast {
    pub stream: StreamRc,
    _listener: StreamListener<()>,
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
}

impl Cast {
    /// Create a new screen cast stream
    pub fn new(
        pipewire: &PipeWire,
        gbm: GbmDevice<DrmDeviceFd>,
        size: Size<i32, Physical>,
        refresh: u32,
        output_name: String,
        signal_ctx: SignalEmitter<'static>,
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
        let state = Rc::new(RefCell::new(CastState::Pending));
        let dmabufs: Rc<RefCell<HashMap<i64, Dmabuf>>> = Rc::new(RefCell::new(HashMap::new()));
        let min_time_between_frames = Rc::new(Cell::new(Duration::ZERO));
        let had_error = Rc::new(Cell::new(false));

        let node_id_clone = node_id.clone();
        let is_active_clone = is_active.clone();
        let had_error_clone = had_error.clone();

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
                                        // Mark as errored - client won't be able to connect
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
                    let gbm = gbm_clone.clone();
                    let min_time_between_frames = min_time_between_frames_clone.clone();
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

                        let format_size = Size::from((format.size().width, format.size().height));

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
                                    return;
                                };

                                let ChoiceEnum::Enum { alternatives, .. } = modifiers.1 else {
                                    warn!("wrong modifier choice type");
                                    return;
                                };

                                // Try to find a working modifier via test allocation
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
                                        return;
                                    }
                                };

                                debug!("Found modifier: {modifier:?}, plane_count: {plane_count}");

                                *state.borrow_mut() = CastState::Ready {
                                    size: format_size,
                                    modifier,
                                    plane_count: plane_count as i32,
                                    damage_tracker: None,
                                };

                                // Update params with fixated modifier
                                let format_obj =
                                    make_video_params_fixated(format_size, refresh, modifier);
                                let mut b1 = Vec::new();
                                let pod1 = make_pod(&mut b1, format_obj);

                                if let Err(err) = stream.update_params(&mut [pod1]) {
                                    warn!("error updating format params: {err:?}");
                                }
                                return;
                            }
                        }

                        // Modifier is already fixated, set buffer params
                        let modifier = Modifier::from(format.modifier());
                        let fourcc = Fourcc::Xrgb8888;

                        let (_, plane_count) = match find_preferred_modifier(
                            &gbm,
                            format_size,
                            fourcc,
                            vec![format.modifier() as i64],
                        ) {
                            Ok(x) => x,
                            Err(err) => {
                                warn!("test allocation failed: {err:?}");
                                return;
                            }
                        };

                        debug!("Ready with modifier: {modifier:?}, plane_count: {plane_count}");

                        *state.borrow_mut() = CastState::Ready {
                            size: format_size,
                            modifier,
                            plane_count: plane_count as i32,
                            damage_tracker: None,
                        };

                        // Set buffer params
                        let buffer_obj = pod::object!(
                            SpaTypes::ObjectParamBuffers,
                            ParamType::Buffers,
                            Property::new(
                                SPA_PARAM_BUFFERS_buffers,
                                pod::Value::Choice(ChoiceValue::Int(Choice(
                                    ChoiceFlags::empty(),
                                    ChoiceEnum::Range {
                                        default: 16,
                                        min: 2,
                                        max: 16
                                    }
                                ))),
                            ),
                            Property::new(
                                SPA_PARAM_BUFFERS_blocks,
                                pod::Value::Int(plane_count as i32)
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

                        let mut b1 = Vec::new();
                        let pod1 = make_pod(&mut b1, buffer_obj);

                        if let Err(err) = stream.update_params(&mut [pod1]) {
                            warn!("error updating buffer params: {err:?}");
                        }
                    }
                })
                .add_buffer({
                    let state = state_clone.clone();
                    let dmabufs = dmabufs.clone();
                    let gbm = gbm_clone.clone();
                    move |_stream, (), buffer| {
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
                            }

                            let fd = (*(*spa_buffer).datas).fd;
                            dmabufs.borrow_mut().insert(fd, dmabuf);
                        }
                    }
                })
                .remove_buffer({
                    let dmabufs = dmabufs.clone();
                    move |_stream, (), buffer| {
                        trace!("remove_buffer");
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

        // Create format parameters with Linear modifier for simplicity
        let mut buffer = Vec::new();
        let obj = make_video_params(size, refresh);
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
            stream,
            _listener: listener,
            is_active,
            size,
            node_id,
            output_name,
            state,
            dmabufs,
            last_frame_time: Duration::ZERO,
            min_time_between_frames,
            had_error,
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

    /// Compute extra delay needed before capturing next frame.
    /// Returns Duration::ZERO if frame can be captured now.
    fn compute_extra_delay(&self, target_frame_time: Duration) -> Duration {
        let last = self.last_frame_time;
        let min = self.min_time_between_frames.get();

        if last.is_zero() {
            trace!(
                ?target_frame_time,
                ?last,
                "last is zero, recording first frame"
            );
            return Duration::ZERO;
        }

        if target_frame_time < last {
            // Record frame with a warning; in case it was an overflow this will fix it.
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
                "frame is too soon: min={min:?}, delay={:?}",
                delay
            );
            return delay;
        }

        Duration::ZERO
    }

    /// Returns true if frame should be skipped (too soon based on frame rate).
    pub fn should_skip_frame(&self, target_frame_time: Duration) -> bool {
        self.compute_extra_delay(target_frame_time) >= CAST_DELAY_ALLOWANCE
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

        // Use the negotiated size from CastState to match dmabuf dimensions
        let size = Size::from((ready_size.w as i32, ready_size.h as i32));

        // Initialize or reset damage tracker if needed
        let dt = damage_tracker
            .get_or_insert_with(|| OutputDamageTracker::new(size, scale, Transform::Normal));

        // Check if scale changed (size change creates new Ready state)
        let OutputModeSource::Static { scale: t_scale, .. } = dt.mode() else {
            unreachable!();
        };
        if *t_scale != scale {
            *dt = OutputDamageTracker::new(size, scale, Transform::Normal);
        }

        // Check damage - skip if none
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

        let Some(mut buffer) = self.stream.dequeue_buffer() else {
            trace!("no available buffer in pw stream");
            return false;
        };

        let fd = buffer.datas_mut()[0].as_raw().fd;
        let dmabufs = self.dmabufs.borrow();
        let Some(dmabuf) = dmabufs.get(&fd) else {
            warn!("dmabuf not found for fd {}", fd);
            return false;
        };

        // Render to the dmabuf
        if let Err(err) = crate::render::render_to_dmabuf(
            renderer,
            dmabuf.clone(),
            size,
            scale,
            Transform::Normal,
            elements.iter().rev(),
        ) {
            warn!("error rendering to dmabuf: {err:?}");
            return false;
        }

        // Update buffer chunk metadata
        for (data, (stride, offset)) in buffer
            .datas_mut()
            .iter_mut()
            .zip(dmabuf.strides().zip(dmabuf.offsets()))
        {
            let chunk = data.chunk_mut();
            *chunk.size_mut() = 1; // Size is set to 1 for DMA-BUF chunks
            *chunk.stride_mut() = stride as i32;
            *chunk.offset_mut() = offset;
        }

        trace!("frame rendered to pw stream");
        true
    }
}

/// Create video format parameters with Linear modifier
fn make_video_params(size: Size<u32, Physical>, refresh: u32) -> pod::Object {
    // Use Linear modifier for simplicity
    let linear = u64::from(Modifier::Linear) as i64;

    pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(FormatProperties::VideoFormat, Id, VideoFormat::BGRx),
        Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY,
            value: pod::Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: linear,
                    alternatives: vec![linear],
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

/// Create fixated video format params
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

    let dmabuf = buffer
        .export()
        .context("error exporting GBM buffer as dmabuf")?;
    let plane_count = dmabuf.num_planes();

    Ok((modifier, plane_count))
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
        info!(output = %self.output_name, "Disconnecting PipeWire stream");
        if let Err(err) = self.stream.disconnect() {
            warn!(output = %self.output_name, "Error disconnecting PipeWire stream: {err:?}");
        }
    }
}
