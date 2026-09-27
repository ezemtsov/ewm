//! Pins the Lisp form of every compositor event and hands the ert suite one of each.

use std::collections::{BTreeSet, HashMap};
use std::num::NonZeroU64;
use std::path::PathBuf;

use ewm_core::event::{Event, OutputInfo, OutputMode};

const VARIANTS: usize = 33;

/// Exhaustive by construction: a new variant does not compile until it is numbered here.
fn variant(event: &Event) -> usize {
    match event {
        Event::Ready => 0,
        Event::New { .. } => 1,
        Event::Mapped { .. } => 2,
        Event::Close { .. } => 3,
        Event::Minimize { .. } => 4,
        Event::Title { .. } => 5,
        Event::Focus { .. } => 6,
        Event::OutputDetected(_) => 7,
        Event::OutputDisconnected { .. } => 8,
        Event::OutputsComplete => 9,
        Event::Layouts { .. } => 10,
        Event::LayoutSwitched { .. } => 11,
        Event::TextInputActivated => 12,
        Event::TextInputDeactivated => 13,
        Event::TextInputReplaced { .. } => 14,
        Event::TextInputReplaceDropped { .. } => 15,
        Event::Key { .. } => 16,
        Event::InterceptedCommand { .. } => 17,
        Event::WorkingArea { .. } => 18,
        Event::OutputConfigChanged { .. } => 19,
        Event::SelectionChanged { .. } => 20,
        Event::ActivateWorkspace { .. } => 21,
        Event::MaximizeRequest { .. } => 22,
        Event::UnmaximizeRequest { .. } => 23,
        Event::Bell { .. } => 24,
        Event::IdleStateChanged { .. } => 25,
        Event::Environment { .. } => 26,
        Event::FrameShown { .. } => 27,
        Event::FrameHidden { .. } => 28,
        Event::FrameMoved { .. } => 29,
        Event::ActivateSurface { .. } => 30,
        Event::Overview { .. } => 31,
        Event::Hide { .. } => 32,
    }
}

/// One of each variant, plus a second sample where an `Option` changes the shape.
fn every_event() -> Vec<Event> {
    let id = |n| NonZeroU64::new(n).unwrap();
    let output = OutputInfo {
        name: "eDP-1".into(),
        make: "BOE".into(),
        model: "0x0a1b".into(),
        serial: String::new(),
        width_mm: 344,
        height_mm: 215,
        x: 0,
        y: 0,
        scale: 1.5,
        transform: 0,
        modes: vec![OutputMode {
            width: 2880,
            height: 1800,
            refresh: 90000,
            preferred: true,
            current: true,
        }],
    };
    vec![
        Event::Ready,
        Event::New {
            id: 7,
            app: "Alacritty".into(),
            output: None,
            pid: 4242,
        },
        Event::New {
            id: 1,
            app: "emacs".into(),
            output: Some("eDP-1".into()),
            pid: 4000,
        },
        Event::Mapped {
            id: 7,
            open_floating: false,
            output: None,
            frame_surface_id: Some(1),
            width: None,
            height: None,
        },
        Event::Mapped {
            id: 9,
            open_floating: true,
            output: Some("eDP-1".into()),
            frame_surface_id: None,
            width: Some(800.0),
            height: Some(600.0),
        },
        Event::Close { id: 7 },
        Event::Minimize { id: 7 },
        Event::ActivateSurface { id: 7 },
        Event::Title {
            id: 7,
            app: "Alacritty".into(),
            title: "~".into(),
        },
        Event::Focus {
            focus_id: id(7),
            x: Some(10.5),
            y: Some(20.0),
            pointer: true,
        },
        Event::Focus {
            focus_id: id(1),
            x: None,
            y: None,
            pointer: false,
        },
        Event::OutputDetected(output),
        Event::OutputDisconnected {
            name: "DP-1".into(),
        },
        Event::OutputsComplete,
        Event::Layouts {
            layouts: vec!["us".into(), "ru".into()],
            current: 0,
        },
        Event::LayoutSwitched {
            layout: "ru".into(),
            index: 1,
        },
        Event::TextInputActivated,
        Event::TextInputDeactivated,
        Event::TextInputReplaced { surface_id: 7 },
        Event::TextInputReplaceDropped { surface_id: 7 },
        Event::Key {
            keycode: 38,
            keysym: 0x61,
            utf8: Some("a".into()),
            surface_id: 7,
            ctrl: false,
            alt: false,
            shift: false,
            logo: false,
        },
        Event::Key {
            keycode: 36,
            keysym: 0xff0d,
            utf8: None,
            surface_id: 7,
            ctrl: true,
            alt: false,
            shift: false,
            logo: false,
        },
        Event::InterceptedCommand { key: "s-d".into() },
        Event::WorkingArea {
            output: "eDP-1".into(),
            x: 0,
            y: 32,
            width: 1920,
            height: 1168,
        },
        Event::OutputConfigChanged {
            name: "eDP-1".into(),
            width: 2880,
            height: 1800,
            refresh: 90000,
            x: 0,
            y: 0,
            scale: 1.5,
            transform: 0,
        },
        Event::SelectionChanged {
            text: "copied".into(),
        },
        Event::ActivateWorkspace {
            output: "eDP-1".into(),
            frame_index: 2,
        },
        Event::MaximizeRequest { id: 7 },
        Event::UnmaximizeRequest { id: 7 },
        Event::Bell { id: Some(7) },
        Event::Bell { id: None },
        Event::IdleStateChanged { idle: true },
        Event::Environment {
            vars: HashMap::from([("WAYLAND_DISPLAY".to_string(), "wayland-ewm-vt2".to_string())]),
        },
        Event::FrameShown { id: 1 },
        Event::FrameHidden { id: 1 },
        Event::FrameMoved {
            id: 1,
            output: "DP-1".into(),
            home_output: "BOE 0x0a1b".into(),
        },
        Event::Overview { open: true },
        Event::Overview { open: false },
        Event::Hide { hidden: true },
        Event::Hide { hidden: false },
    ]
}

fn fixture() -> String {
    let mut text = String::from(concat!(
        ";;; every-event.el --- one of each compositor event, as Lisp receives it -*- lexical-binding: t -*-\n",
        ";; Generated by compositor/tests/protocol_events.rs; regenerate with\n",
        ";;   EWM_UPDATE_FIXTURES=1 cargo test --test protocol_events\n\n",
        "(defconst ewm-test-every-event\n  '(",
    ));
    for event in every_event() {
        let value = elisp::serde::to_value(&event).unwrap();
        text.push_str(&format!("\n    {value}"));
    }
    text.push_str("))\n\n(provide 'every-event)\n;;; every-event.el ends here\n");
    text
}

#[test]
fn every_variant_is_sampled() {
    let sampled: BTreeSet<usize> = every_event().iter().map(variant).collect();
    assert_eq!(sampled, (0..VARIANTS).collect());
}

#[test]
fn fixture_matches_the_events() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("tests/fixtures/every-event.el");
    let expected = fixture();
    if std::env::var_os("EWM_UPDATE_FIXTURES").is_some() {
        std::fs::write(&path, &expected).unwrap();
    }
    let actual = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        actual == expected,
        "{} is stale; rerun with EWM_UPDATE_FIXTURES=1",
        path.display()
    );
}
