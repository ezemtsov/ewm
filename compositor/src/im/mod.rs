//! Input method support: the relay, text input commit gating, intercepted key repeat and the
//! compositor methods that drive them.

use std::os::unix::net::UnixStream;
use std::sync::Arc;

use serde::Deserialize;
use smithay::reexports::wayland_server::backend::DisconnectReason;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use tracing::debug;

use crate::{ClientState, Event, Ewm, State, module};

pub mod popup;
pub mod relay;
pub mod repeat;
pub mod text_input;

/// Who serves text input for clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TextInputProvider {
    /// Emacs input methods, through the relay.
    #[default]
    Emacs,
    /// An input_method_v2 client such as fcitx5 or ibus.
    External,
}

impl Ewm {
    /// Connect the input method relay over a socketpair of our own.
    fn connect_im_relay(&mut self) {
        if self.im_relay.is_some() {
            return;
        }
        let (server_end, client_end) =
            UnixStream::pair().expect("failed to create IM relay socketpair");
        // Socketpair peer credentials are our own.
        let client = self
            .display_handle
            .insert_client(server_end, Arc::new(ClientState::default()))
            .expect("failed to insert IM relay client");
        let channel = relay::connect(client_end);
        let source = self
            .loop_handle
            .insert_source(channel, |event, _, state| {
                if let smithay::reexports::calloop::channel::Event::Msg(event) = event {
                    state.handle_im_event(event);
                }
            })
            .expect("failed to register IM relay event source");
        self.im_relay = Some((client, source));
    }

    /// Disconnect the relay so an external input method can take the seat.
    fn disconnect_im_relay(&mut self) {
        let Some((client, source)) = self.im_relay.take() else {
            return;
        };
        self.loop_handle.remove(source);
        self.display_handle
            .backend_handle()
            .kill_client(client.id(), DisconnectReason::ConnectionClosed);
    }

    /// Update text_input focus for input method support.
    ///
    /// With Emacs as the provider, Emacs surfaces clear text_input like None
    /// does (leave + set_focus(None)), causing the client to disable. Commits
    /// that arrive during the resulting disable->enable gap are queued in
    /// `im_text_input` and drained on the next `ImEvent::Activated` for the
    /// same surface. An external input method serves Emacs frames too.
    pub(crate) fn update_text_input_focus(&self, surface: Option<&WlSurface>) {
        let surface = match self.text_input_provider {
            TextInputProvider::Emacs => {
                surface.filter(|surface| !self.is_emacs_frame_surface(surface))
            }
            TextInputProvider::External => surface,
        };
        text_input::update_focus(&self.seat, surface);
    }

    pub(crate) fn active_text_input_surface_id(&self) -> Option<u64> {
        text_input::active_surface_id(&self.seat, |surface| self.surface_id(surface))
    }

    fn commit_active_text_input(&self, text: String) {
        text_input::commit_active(&self.seat, text)
    }

    pub(crate) fn preedit_active_text_input(&self, text: String) {
        text_input::preedit_active(&self.seat, text)
    }

    pub(crate) fn replace_active_text_input(&self, before: u32, after: u32, text: String) -> bool {
        text_input::replace_active(&self.seat, before, after, text)
    }

    fn deliver_replace_ack(&mut self, surface_id: u64, r: text_input::Replacement) {
        if self.replace_active_text_input(r.before, r.after, r.text) {
            self.queue_event(Event::TextInputReplaced { surface_id });
        }
    }

    /// Commit `text` from the input method for `surface_id`, queued or dropped if it is stale.
    pub(crate) fn im_commit(&mut self, text: String, surface_id: u64) {
        let active_surface_id = self.active_text_input_surface_id();
        match self
            .im_text_input
            .commit(active_surface_id, surface_id, text)
        {
            text_input::CommitResult::Deliver(text) => {
                self.commit_active_text_input(text);
            }
            text_input::CommitResult::Queued => {
                debug!("ImCommit: queued (surface_id={surface_id})");
            }
            text_input::CommitResult::DroppedStale => {
                debug!("ImCommit: dropped stale commit (surface_id={surface_id})");
            }
        }
    }

    /// Replace the surrounding text of `surface_id` with `text`, queued or dropped if it is stale.
    pub(crate) fn text_input_replace(&mut self, text: String, surface_id: u64) {
        let active_surface_id = self.active_text_input_surface_id();
        match self
            .im_text_input
            .replace(active_surface_id, surface_id, text)
        {
            text_input::ReplaceResult::Deliver(r) => {
                self.deliver_replace_ack(surface_id, r);
            }
            text_input::ReplaceResult::Queued => {
                debug!("TextInputReplace: queued (surface_id={surface_id})");
            }
            text_input::ReplaceResult::DroppedStale => {
                debug!("TextInputReplace: dropped stale replace (surface_id={surface_id})");
                self.queue_event(Event::TextInputReplaceDropped { surface_id });
            }
        }
    }
}

impl State {
    /// Hand the seat's input method slot to the relay or to an external client.
    pub(crate) fn set_text_input_provider(&mut self, provider: TextInputProvider) {
        self.ewm.text_input_provider = provider;
        match provider {
            TextInputProvider::Emacs => self.ewm.connect_im_relay(),
            TextInputProvider::External => {
                self.ewm.disconnect_im_relay();
                self.handle_im_event(relay::ImEvent::Deactivated);
            }
        }
        // Emacs frames join or leave text-input focus with the provider.
        self.ewm
            .update_text_input_focus(self.ewm.keyboard_focus.as_ref());
    }

    /// Handle one IM relay activate/deactivate event. Delivered by the calloop
    /// channel so `is_active()` and the Emacs intercept round-trip land before
    /// the next key, not one dispatch late.
    fn handle_im_event(&mut self, event: relay::ImEvent) {
        match event {
            relay::ImEvent::Activated => {
                if self.ewm.im_text_input.activate() {
                    self.ewm.queue_event(Event::TextInputActivated);
                }

                // Drain commits queued during the disable->enable gap.
                let active_surface_id = self.ewm.active_text_input_surface_id();
                if let Some(text) = self
                    .ewm
                    .im_text_input
                    .drain_pending_for_active_surface(active_surface_id)
                {
                    debug!("ImCommit: draining queued commit(s)");
                    self.ewm.commit_active_text_input(text);
                }
            }
            relay::ImEvent::Deactivated => {
                if self.ewm.im_text_input.deactivate() {
                    self.ewm.queue_event(Event::TextInputDeactivated);
                }
                module::set_text_input_surrounding(None);
            }
            relay::ImEvent::SurroundingText {
                text,
                cursor,
                anchor,
            } => {
                let active_surface_id = self.ewm.active_text_input_surface_id();
                self.ewm.im_text_input.set_surrounding(
                    active_surface_id,
                    text.len() as u32,
                    cursor,
                    anchor,
                );
                // Publish for `ewm-edit` to pull, rather than pushing per keystroke.
                module::set_text_input_surrounding(active_surface_id.map(|surface_id| {
                    module::SurroundingSnapshot {
                        surface_id,
                        text,
                        cursor,
                        anchor,
                    }
                }));
                // A finish queued during the refocus gap now has fresh lengths.
                let drained = self
                    .ewm
                    .im_text_input
                    .drain_pending_replace(active_surface_id);
                if let Some((surface_id, r)) = drained.deliver {
                    debug!("TextInputReplace: draining queued replace (surface_id={surface_id})");
                    self.ewm.deliver_replace_ack(surface_id, r);
                }
                for surface_id in drained.dropped {
                    debug!(
                        "TextInputReplace: dropped stale queued replace (surface_id={surface_id})"
                    );
                    self.ewm
                        .queue_event(Event::TextInputReplaceDropped { surface_id });
                }
            }
        }
    }

    /// Forward back to the client a key Emacs declined to translate.
    pub(crate) fn forward_text_input_key(
        &mut self,
        surface_id: u64,
        keycode: u32,
        ctrl: bool,
        alt: bool,
        shift: bool,
        logo: bool,
    ) {
        if self.ewm.active_text_input_surface_id() != Some(surface_id) {
            return;
        }
        self.tap_key(keycode, (ctrl, alt, shift, logo));
    }
}
