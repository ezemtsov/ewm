//! Property tests for what a new toplevel's client decides: only listener
//! clients with our pid are Emacs frames, portal windows always float, and an
//! Emacs frame stays out of the layout until Lisp places it, exactly once.

use ewm_core::event::Event;
use ewm_core::testing::{Fixture, TestClient, Toplevel, placement, proptest_cases};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use smithay::utils::{Logical, Size};

const OUTPUTS: [&str; 2] = ["HEAD-A", "HEAD-B"];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    /// Listener client while `emacs_pid` is ours.
    Emacs,
    /// Listener client while `emacs_pid` is someone else's.
    App,
    /// Service-channel client; `emacs_pid` is ours to prove the tag wins.
    Portal,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Hints {
    Plain,
    Parent,
    Fixed,
}

#[derive(Debug, Clone, Copy)]
struct Arrival {
    kind: Kind,
    hints: Hints,
    /// Ask for the toplevel as a frame on `OUTPUTS[i]` once it has arrived.
    place: Option<usize>,
}

fn arrival() -> impl Strategy<Value = Arrival> {
    (
        prop_oneof![Just(Kind::Emacs), Just(Kind::App), Just(Kind::Portal)],
        prop_oneof![Just(Hints::Plain), Just(Hints::Parent), Just(Hints::Fixed)],
        prop::option::of(0..OUTPUTS.len()),
    )
        .prop_map(|(kind, hints, place)| Arrival { kind, hints, place })
}

fn size() -> Size<i32, Logical> {
    Size::from((400, 300))
}

struct Harness {
    fix: Fixture,
    listener: TestClient,
    portal: TestClient,
}

impl Harness {
    fn new() -> Self {
        let mut fix = Fixture::new().unwrap();
        fix.enable_event_capture();
        for output in OUTPUTS {
            fix.add_output(output, 1920, 1080);
        }
        let mut listener = fix.new_client().unwrap();
        let mut portal = fix.new_portal_client().unwrap();
        fix.roundtrip(&mut listener);
        fix.roundtrip(&mut portal);
        Self {
            fix,
            listener,
            portal,
        }
    }

    fn arrive(&mut self, arrival: Arrival) -> Result<(), TestCaseError> {
        // A parent must come from the same client, so it is an arrival of its own.
        let parent = match arrival.hints {
            Hints::Parent => {
                let kind = match arrival.kind {
                    Kind::Portal => Kind::Portal,
                    _ => Kind::App,
                };
                Some(self.open(kind, Hints::Plain, None)?)
            }
            _ => None,
        };
        let (_, sid) = self.open(arrival.kind, arrival.hints, parent.as_ref())?;
        if let Some(i) = arrival.place {
            self.place(sid, arrival.kind, i)?;
        }
        Ok(())
    }

    fn open(
        &mut self,
        kind: Kind,
        hints: Hints,
        parent: Option<&(Toplevel, u64)>,
    ) -> Result<(Toplevel, u64), TestCaseError> {
        let own = std::process::id();
        self.fix.ewm().set_emacs_pid(match kind {
            Kind::App => own + 1,
            _ => own,
        });
        let client = match kind {
            Kind::Portal => &mut self.portal,
            _ => &mut self.listener,
        };
        let toplevel = match (hints, parent) {
            (Hints::Parent, Some((parent, _))) => {
                self.fix
                    .open_toplevel_with_parent(client, "child", parent, size())
            }
            (Hints::Fixed, _) => self.fix.open_toplevel_fixed_size(client, "fixed", size()),
            _ => self.fix.open_toplevel(client, "plain", size()),
        };
        let sid = self
            .fix
            .find_surface_id(client, &toplevel.surface)
            .expect("server did not assign a surface id");
        self.check(sid, kind, hints)?;
        Ok((toplevel, sid))
    }

    /// No toplevel is a frame at arrival, an Emacs one waits for Lisp, the rest map.
    fn check(&mut self, sid: u64, kind: Kind, hints: Hints) -> Result<(), TestCaseError> {
        prop_assert!(!self.fix.ewm_ref().layout.has_frame(sid));
        self.fix.ewm_ref().verify_invariants();

        let events = self.fix.drain_events();
        let new = events
            .iter()
            .filter(|event| matches!(event, Event::New { id, .. } if *id == sid))
            .count();
        let mapped: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Event::Mapped {
                    id, open_floating, ..
                } if *id == sid => Some(*open_floating),
                _ => None,
            })
            .collect();
        prop_assert_eq!(new, 1, "exactly one New per toplevel");
        prop_assert!(
            !shown_or_hidden(&events, sid),
            "nothing shows before placement"
        );

        if kind == Kind::Emacs {
            prop_assert!(
                mapped.is_empty(),
                "an Emacs frame is placed by lisp, never via Mapped"
            );
        } else {
            let floats = kind == Kind::Portal || hints != Hints::Plain;
            prop_assert_eq!(mapped, vec![floats]);
        }
        Ok(())
    }

    /// Only an Emacs toplevel becomes a frame, where asked, and a second ask changes nothing.
    fn place(&mut self, sid: u64, kind: Kind, i: usize) -> Result<(), TestCaseError> {
        self.fix.place_frame(sid, placement(OUTPUTS[i]));
        let is_emacs = kind == Kind::Emacs;
        let placed = self.fix.ewm_ref().layout.has_frame(sid);
        prop_assert_eq!(placed, is_emacs);
        let events = self.fix.drain_events();
        prop_assert_eq!(
            shown_or_hidden(&events, sid),
            is_emacs,
            "placement announces the frame"
        );
        if is_emacs {
            prop_assert_eq!(
                self.fix.ewm_ref().layout.frame_output(sid),
                Some(OUTPUTS[i])
            );
            self.fix.place_frame(sid, placement(OUTPUTS[1 - i]));
            prop_assert_eq!(
                self.fix.ewm_ref().layout.frame_output(sid),
                Some(OUTPUTS[i])
            );
            prop_assert!(!shown_or_hidden(&self.fix.drain_events(), sid));
        }
        self.fix.ewm_ref().verify_invariants();
        Ok(())
    }
}

fn shown_or_hidden(events: &[Event], sid: u64) -> bool {
    events.iter().any(
        |event| matches!(event, Event::FrameShown { id } | Event::FrameHidden { id } if *id == sid),
    )
}

/// A toplevel gone before its first commit was never announced, Lisp hears nothing of it.
#[test]
fn a_toplevel_gone_before_its_first_commit_is_silent() {
    let mut fix = Fixture::new().unwrap();
    fix.enable_event_capture();
    fix.add_output(OUTPUTS[0], 1920, 1080);
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    fix.drain_events();
    let toplevel = client.create_toplevel_uncommitted("gone");
    fix.roundtrip(&mut client);
    let sid = fix.find_surface_id(&client, &toplevel.surface).unwrap();
    toplevel.xdg_toplevel.destroy();
    toplevel.xdg_surface.destroy();
    toplevel.surface.destroy();
    fix.roundtrip(&mut client);
    fix.dispatch();
    assert!(
        !fix.ewm_ref().surfaces.contains_key(&sid),
        "record {sid} kept"
    );
    let events = fix.drain_events();
    assert!(
        events.iter().all(
            |event| !matches!(event, Event::New { id, .. } | Event::Close { id } if *id == sid)
        ),
        "{events:?}"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(proptest_cases(32)))]

    #[test]
    fn toplevel_kind_invariants_hold(arrivals in prop::collection::vec(arrival(), 1..8)) {
        let mut h = Harness::new();
        for arrival in arrivals {
            h.arrive(arrival)?;
        }
    }
}
