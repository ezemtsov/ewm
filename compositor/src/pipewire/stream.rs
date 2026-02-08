//! PipeWire video stream for screen casting
//!
//! This module implements PipeWire video streaming for screen sharing.

use std::cell::Cell;
use std::io::Cursor;
use std::rc::Rc;

use anyhow::Context as _;
use pipewire::properties::Properties;
use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pipewire::spa::param::video::VideoFormat;
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{self, Pod, Property, PropertyFlags};
use pipewire::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Rectangle, SpaTypes};
use pipewire::spa::pod::ChoiceValue;
use pipewire::stream::{Stream, StreamFlags, StreamListener, StreamState};
use smithay::utils::{Physical, Size};
use tracing::{debug, info, warn};

use super::PipeWire;

/// A screen cast session
pub struct Cast {
    pub stream: Stream,
    _listener: StreamListener<()>,
    pub is_active: Rc<Cell<bool>>,
    pub size: Size<u32, Physical>,
    pub node_id: Rc<Cell<Option<u32>>>,
}

impl Cast {
    /// Create a new screen cast stream
    pub fn new(
        pipewire: &PipeWire,
        size: Size<i32, Physical>,
        refresh: u32,
    ) -> anyhow::Result<Self> {
        let size = Size::from((size.w as u32, size.h as u32));

        let stream =
            Stream::new(&pipewire.core, "ewm-screen-cast", Properties::new())
                .context("error creating PipeWire stream")?;

        let node_id = Rc::new(Cell::new(None));
        let is_active = Rc::new(Cell::new(false));

        let node_id_clone = node_id.clone();
        let is_active_clone = is_active.clone();

        let listener = stream
            .add_local_listener_with_user_data(())
            .state_changed(move |stream, (), old, new| {
                debug!("PipeWire stream state: {old:?} -> {new:?}");

                match new {
                    StreamState::Paused => {
                        let id = stream.node_id();
                        info!("PipeWire stream paused, node_id: {id}");
                        node_id_clone.set(Some(id));
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
            .param_changed(|_stream, (), id, _param| {
                if id != ParamType::Format.as_raw() {
                    return;
                }
                debug!("PipeWire format changed");
            })
            .add_buffer(|_stream, (), _buffer| {
                debug!("PipeWire add_buffer callback");
            })
            .remove_buffer(|_stream, (), _buffer| {
                debug!("PipeWire remove_buffer callback");
            })
            .register()
            .context("error registering stream listener")?;

        // Create format parameters
        let mut buffer = Vec::new();
        let params = make_video_params(&mut buffer, size, refresh);
        let mut params_ref: [&Pod; 1] = [params];

        stream
            .connect(
                pipewire::spa::utils::Direction::Output,
                None,
                StreamFlags::DRIVER | StreamFlags::ALLOC_BUFFERS,
                &mut params_ref,
            )
            .context("error connecting stream")?;

        info!("PipeWire stream created for size {:?}", size);

        Ok(Self {
            stream,
            _listener: listener,
            is_active,
            size,
            node_id,
        })
    }
}

/// Create video format parameters for the stream
fn make_video_params(buffer: &mut Vec<u8>, size: Size<u32, Physical>, refresh: u32) -> &Pod {
    // Build the format object
    let obj = pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(FormatProperties::VideoFormat, Id, VideoFormat::BGRx),
        Property {
            key: FormatProperties::VideoSize.as_raw(),
            flags: PropertyFlags::empty(),
            value: pod::Value::Rectangle(Rectangle {
                width: size.w,
                height: size.h,
            }),
        },
        Property {
            key: FormatProperties::VideoFramerate.as_raw(),
            flags: PropertyFlags::empty(),
            value: pod::Value::Fraction(Fraction { num: 0, denom: 1 }),
        },
        Property {
            key: FormatProperties::VideoMaxFramerate.as_raw(),
            flags: PropertyFlags::empty(),
            value: pod::Value::Choice(ChoiceValue::Fraction(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Range {
                    default: Fraction {
                        num: refresh,
                        denom: 1,
                    },
                    min: Fraction { num: 1, denom: 1 },
                    max: Fraction {
                        num: refresh,
                        denom: 1,
                    },
                },
            ))),
        },
    );

    PodSerializer::serialize(Cursor::new(&mut *buffer), &pod::Value::Object(obj)).unwrap();
    Pod::from_bytes(buffer).unwrap()
}
