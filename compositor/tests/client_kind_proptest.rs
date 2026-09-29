//! Property tests for what a new toplevel's client decides: only listener
//! clients with our pid are Emacs frames, portal windows always float, and
//! pending Emacs frames go to Emacs toplevels alone, in order.
//! Own binary: the pending-frame queue is a process-wide static.

use ewm_core::event::Event;
use ewm_core::testing::{Fixture, TestClient, Toplevel, clear_pending_frames, prepare_frame};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use serde_json::json;
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
    /// Queue a tiled Emacs frame for `OUTPUTS[i]` before the toplevel arrives.
    pending: Option<usize>,
}

fn arrival() -> impl Strategy<Value = Arrival> {
    (
        prop_oneof![Just(Kind::Emacs), Just(Kind::App), Just(Kind::Portal)],
        prop_oneof![Just(Hints::Plain), Just(Hints::Parent), Just(Hints::Fixed)],
        prop::option::of(0..OUTPUTS.len()),
    )
        .prop_map(|(kind, hints, pending)| Arrival {
            kind,
            hints,
            pending,
        })
}

fn size() -> Size<i32, Logical> {
    Size::from((400, 300))
}

struct Harness {
    fix: Fixture,
    listener: TestClient,
    portal: TestClient,
    /// Model of the pending-frame queue, front first.
    pending: Vec<&'static str>,
}

impl Harness {
    fn new() -> Self {
        clear_pending_frames();
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
            pending: Vec::new(),
        }
    }

    fn arrive(&mut self, arrival: Arrival) -> Result<(), TestCaseError> {
        if let Some(i) = arrival.pending {
            prepare_frame(OUTPUTS[i]);
            self.pending.push(OUTPUTS[i]);
        }
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
        self.open(arrival.kind, arrival.hints, parent.as_ref())?;
        Ok(())
    }

    fn open(
        &mut self,
        kind: Kind,
        hints: Hints,
        parent: Option<&Toplevel>,
    ) -> Result<Toplevel, TestCaseError> {
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
            (Hints::Parent, Some(parent)) => {
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
        Ok(toplevel)
    }

    fn check(&mut self, sid: u64, kind: Kind, hints: Hints) -> Result<(), TestCaseError> {
        let is_emacs = kind == Kind::Emacs;
        prop_assert_eq!(self.fix.ewm_ref().is_emacs_frame(sid), is_emacs);

        let events = self.fix.drain_events();
        let new: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Event::New { id, output, .. } if *id == sid => Some(output.clone()),
                _ => None,
            })
            .collect();
        let mapped: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Event::Mapped {
                    id, open_floating, ..
                } if *id == sid => Some(*open_floating),
                _ => None,
            })
            .collect();
        prop_assert_eq!(new.len(), 1, "exactly one New per toplevel");

        if is_emacs {
            // Emacs takes the oldest pending frame, and is placed by lisp, never via Mapped.
            match self.pending.first().copied() {
                Some(output) => {
                    self.pending.remove(0);
                    prop_assert_eq!(new[0].as_deref(), Some(output));
                }
                None => prop_assert!(new[0].is_some()),
            }
            prop_assert!(mapped.is_empty());
        } else {
            prop_assert_eq!(&new[0], &None, "apps carry no pairing output");
            let floats = kind == Kind::Portal || hints != Hints::Plain;
            prop_assert_eq!(mapped, vec![floats]);
        }
        prop_assert_eq!(
            &self.fix.debug_state()["pending_frame_outputs"],
            &json!(self.pending)
        );
        Ok(())
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        ..ProptestConfig::default()
    })]

    #[test]
    fn toplevel_kind_invariants_hold(arrivals in prop::collection::vec(arrival(), 1..8)) {
        let mut h = Harness::new();
        for arrival in arrivals {
            h.arrive(arrival)?;
        }
    }
}
