//! PipeWire video stream for screen casting
//!
//! This module implements PipeWire video streaming for screen sharing.
//! Based on niri's pw_utils.rs implementation.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Cursor;
use std::os::fd::AsRawFd;
use std::rc::Rc;

use anyhow::Context as _;
use pipewire::properties::Properties;
use pipewire::spa::buffer::DataType;
use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pipewire::spa::param::video::{VideoFormat, VideoInfoRaw};
use pipewire::spa::param::format_utils::parse_format;
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::deserialize::PodDeserializer;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{self, ChoiceValue, Pod, PodPropFlags, Property, PropertyFlags};
use pipewire::spa::sys::*;
use pipewire::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Rectangle, SpaTypes};
use pipewire::stream::{Stream, StreamFlags, StreamListener, StreamState};
use smithay::backend::allocator::dmabuf::{AsDmabuf, Dmabuf};
use smithay::backend::allocator::gbm::{GbmBuffer, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::DrmDeviceFd;
use smithay::reexports::gbm::Modifier;
use smithay::utils::{Physical, Size};
use tracing::{debug, info, trace, warn};
use zbus::object_server::SignalEmitter;

use super::PipeWire;
use crate::dbus::screen_cast;

/// Cast state machine
#[derive(Debug)]
enum CastState {
    /// Waiting for format negotiation
    Pending,
    /// Format negotiated, ready to stream
    Ready {
        size: Size<u32, Physical>,
        modifier: Modifier,
        plane_count: i32,
    },
}

/// A screen cast session
pub struct Cast {
    pub stream: Stream,
    _listener: StreamListener<()>,
    pub is_active: Rc<Cell<bool>>,
    pub size: Size<u32, Physical>,
    pub node_id: Rc<Cell<Option<u32>>>,
    #[allow(dead_code)]
    state: Rc<RefCell<CastState>>,
    #[allow(dead_code)]
    dmabufs: Rc<RefCell<HashMap<i64, Dmabuf>>>,
}

impl Cast {
    /// Create a new screen cast stream
    pub fn new(
        pipewire: &PipeWire,
        gbm: GbmDevice<DrmDeviceFd>,
        size: Size<i32, Physical>,
        refresh: u32,
        signal_ctx: SignalEmitter<'static>,
    ) -> anyhow::Result<Self> {
        let size = Size::from((size.w as u32, size.h as u32));

        let stream =
            Stream::new(&pipewire.core, "ewm-screen-cast", Properties::new())
                .context("error creating PipeWire stream")?;

        let node_id = Rc::new(Cell::new(None));
        let is_active = Rc::new(Cell::new(false));
        let state = Rc::new(RefCell::new(CastState::Pending));
        let dmabufs: Rc<RefCell<HashMap<i64, Dmabuf>>> = Rc::new(RefCell::new(HashMap::new()));

        let node_id_clone = node_id.clone();
        let is_active_clone = is_active.clone();

        let state_clone = state.clone();
        let gbm_clone = gbm.clone();

        let listener = stream
            .add_local_listener_with_user_data(())
            .state_changed(move |stream, (), old, new| {
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
                    }
                    _ => {}
                }
            })
            .param_changed({
                let state = state_clone.clone();
                let gbm = gbm_clone.clone();
                move |stream, (), id, pod| {
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

                    // Check if modifier needs fixation
                    let object = pod.as_object().unwrap();
                    let modifier_prop = object.find_prop(pipewire::spa::utils::Id(FormatProperties::VideoModifier.0));

                    if let Some(prop) = modifier_prop {
                        if prop.flags().contains(PodPropFlags::DONT_FIXATE) {
                            debug!("Fixating modifier");

                            let pod_modifier = prop.value();
                            let Ok((_, modifiers)) = PodDeserializer::deserialize_from::<Choice<i64>>(
                                pod_modifier.as_bytes(),
                            ) else {
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
                            };

                            // Update params with fixated modifier
                            let format_obj = make_video_params_fixated(format_size, refresh, modifier);
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
                        Property::new(SPA_PARAM_BUFFERS_blocks, pod::Value::Int(plane_count as i32)),
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
                    let state = state.borrow();
                    let CastState::Ready { size, modifier, .. } = &*state else {
                        trace!("add_buffer but not ready yet");
                        return;
                    };

                    trace!("add_buffer: size={size:?}, modifier={modifier:?}");

                    unsafe {
                        let spa_buffer = (*buffer).buffer;
                        let fourcc = Fourcc::Xrgb8888;

                        let dmabuf = match allocate_dmabuf(&gbm, *size, fourcc, *modifier) {
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
            state,
            dmabufs,
        })
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
                denom: 1000
            },
            Fraction { num: 1, denom: 1 },
            Fraction {
                num: refresh,
                denom: 1000
            }
        ),
    )
}

/// Create fixated video format params
fn make_video_params_fixated(size: Size<u32, Physical>, refresh: u32, modifier: Modifier) -> pod::Object {
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
                denom: 1000
            },
            Fraction { num: 1, denom: 1 },
            Fraction {
                num: refresh,
                denom: 1000
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
