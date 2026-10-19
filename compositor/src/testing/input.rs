//! Input backend that feeds absolute events through the real handlers.

use std::path::PathBuf;

use smithay::backend::input::{
    AbsolutePositionEvent, Device, DeviceCapability, Event, GestureBeginEvent, GestureEndEvent,
    GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent,
    GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
    InputBackend, InputTime, PointerMotionAbsoluteEvent, TouchDownEvent, TouchEvent, TouchSlot,
    TouchUpEvent, UnusedEvent,
};
use smithay::output::Output;

use crate::input::EwmInputDevice;

/// A touchscreen or absolute pointer, optionally bound to one output.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct TestDevice {
    pub output: Option<Output>,
}

impl Device for TestDevice {
    fn id(&self) -> String {
        "test device".into()
    }

    fn name(&self) -> String {
        "test device".into()
    }

    fn has_capability(&self, capability: DeviceCapability) -> bool {
        matches!(
            capability,
            DeviceCapability::Touch | DeviceCapability::Pointer
        )
    }

    fn usb_id(&self) -> Option<(u32, u32)> {
        None
    }

    fn syspath(&self) -> Option<PathBuf> {
        None
    }
}

impl EwmInputDevice for TestDevice {
    fn output(&self) -> Option<&Output> {
        self.output.as_ref()
    }
}

/// Position as fractions of the device's own extent.
#[derive(Default)]
pub struct TestAbsoluteEvent {
    pub device: TestDevice,
    pub x: f64,
    pub y: f64,
}

impl Event<TestInputBackend> for TestAbsoluteEvent {
    fn time(&self) -> InputTime {
        InputTime::from_millis(0)
    }

    fn device(&self) -> TestDevice {
        self.device.clone()
    }
}

impl AbsolutePositionEvent<TestInputBackend> for TestAbsoluteEvent {
    fn x(&self) -> f64 {
        self.x
    }

    fn y(&self) -> f64 {
        self.y
    }

    fn x_transformed(&self, width: i32) -> f64 {
        self.x * f64::from(width)
    }

    fn y_transformed(&self, height: i32) -> f64 {
        self.y * f64::from(height)
    }
}

impl TouchEvent<TestInputBackend> for TestAbsoluteEvent {
    fn slot(&self) -> TouchSlot {
        Some(0).into()
    }
}

impl TouchDownEvent<TestInputBackend> for TestAbsoluteEvent {}
impl TouchUpEvent<TestInputBackend> for TestAbsoluteEvent {}
impl PointerMotionAbsoluteEvent<TestInputBackend> for TestAbsoluteEvent {}

/// A touchpad gesture: `fingers` going down, moving, pinching or lifting.
#[derive(Default)]
pub struct TestGestureEvent {
    pub fingers: u32,
    pub dx: f64,
    pub dy: f64,
    pub scale: f64,
    pub rotation: f64,
    pub cancelled: bool,
}

impl Event<TestInputBackend> for TestGestureEvent {
    fn time(&self) -> InputTime {
        InputTime::from_millis(0)
    }

    fn device(&self) -> TestDevice {
        TestDevice::default()
    }
}

impl GestureBeginEvent<TestInputBackend> for TestGestureEvent {
    fn fingers(&self) -> u32 {
        self.fingers
    }
}

impl GestureSwipeUpdateEvent<TestInputBackend> for TestGestureEvent {
    fn delta_x(&self) -> f64 {
        self.dx
    }

    fn delta_y(&self) -> f64 {
        self.dy
    }
}

impl GesturePinchUpdateEvent<TestInputBackend> for TestGestureEvent {
    fn delta_x(&self) -> f64 {
        self.dx
    }

    fn delta_y(&self) -> f64 {
        self.dy
    }

    fn scale(&self) -> f64 {
        self.scale
    }

    fn rotation(&self) -> f64 {
        self.rotation
    }
}

impl GestureEndEvent<TestInputBackend> for TestGestureEvent {
    fn cancelled(&self) -> bool {
        self.cancelled
    }
}

impl GestureSwipeBeginEvent<TestInputBackend> for TestGestureEvent {}
impl GestureSwipeEndEvent<TestInputBackend> for TestGestureEvent {}
impl GesturePinchBeginEvent<TestInputBackend> for TestGestureEvent {}
impl GesturePinchEndEvent<TestInputBackend> for TestGestureEvent {}
impl GestureHoldBeginEvent<TestInputBackend> for TestGestureEvent {}
impl GestureHoldEndEvent<TestInputBackend> for TestGestureEvent {}

pub struct TestInputBackend;

impl InputBackend for TestInputBackend {
    type Device = TestDevice;

    type KeyboardKeyEvent = UnusedEvent;
    type PointerAxisEvent = UnusedEvent;
    type PointerButtonEvent = UnusedEvent;
    type PointerMotionEvent = UnusedEvent;
    type PointerMotionAbsoluteEvent = TestAbsoluteEvent;

    type GestureSwipeBeginEvent = TestGestureEvent;
    type GestureSwipeUpdateEvent = TestGestureEvent;
    type GestureSwipeEndEvent = TestGestureEvent;
    type GesturePinchBeginEvent = TestGestureEvent;
    type GesturePinchUpdateEvent = TestGestureEvent;
    type GesturePinchEndEvent = TestGestureEvent;
    type GestureHoldBeginEvent = TestGestureEvent;
    type GestureHoldEndEvent = TestGestureEvent;

    type TouchDownEvent = TestAbsoluteEvent;
    type TouchUpEvent = TestAbsoluteEvent;
    type TouchMotionEvent = UnusedEvent;
    type TouchCancelEvent = UnusedEvent;
    type TouchFrameEvent = UnusedEvent;
    type TabletToolAxisEvent = UnusedEvent;
    type TabletToolProximityEvent = UnusedEvent;
    type TabletToolTipEvent = UnusedEvent;
    type TabletToolButtonEvent = UnusedEvent;

    type SwitchToggleEvent = UnusedEvent;

    type SpecialEvent = UnusedEvent;
}
