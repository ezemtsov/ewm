//! One-shot reply channel used for request/reply commands between Emacs and
//! the compositor. The mutex serializes "deliver" against "give up": once
//! either side commits, the other observes the outcome and reacts.

use std::fmt;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

enum ReplyState<T> {
    Empty,
    Ready(T),
    Abandoned,
}

struct ReplySlot<T> {
    state: Mutex<ReplyState<T>>,
    cvar: Condvar,
}

pub fn reply_channel<T>() -> (Reply<T>, ReplyReceiver<T>) {
    let slot = Arc::new(ReplySlot {
        state: Mutex::new(ReplyState::Empty),
        cvar: Condvar::new(),
    });
    (Reply(slot.clone()), ReplyReceiver(slot))
}

/// Sender half. Embedded in request-style command variants.
#[must_use = "request commands must deliver their reply"]
pub struct Reply<T>(Arc<ReplySlot<T>>);

impl<T> Reply<T> {
    /// Deliver the reply, or return the value if the receiver gave up.
    pub fn send(self, value: T) -> Result<(), T> {
        let mut s = self.0.state.lock().unwrap();
        if matches!(*s, ReplyState::Abandoned) {
            return Err(value);
        }
        *s = ReplyState::Ready(value);
        self.0.cvar.notify_one();
        Ok(())
    }
}

impl<T> Drop for Reply<T> {
    fn drop(&mut self) {
        let mut s = self.0.state.lock().unwrap();
        if matches!(*s, ReplyState::Empty) {
            *s = ReplyState::Abandoned;
            self.0.cvar.notify_one();
        }
    }
}

impl<T> fmt::Debug for Reply<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<reply>")
    }
}

pub enum ReplyError {
    Timeout,
    Disconnected,
}

pub struct ReplyReceiver<T>(Arc<ReplySlot<T>>);

impl<T> ReplyReceiver<T> {
    pub fn recv_timeout(self, timeout: Duration) -> Result<T, ReplyError> {
        let deadline = Instant::now() + timeout;
        let mut s = self.0.state.lock().unwrap();
        while matches!(*s, ReplyState::Empty) {
            let now = Instant::now();
            if now >= deadline {
                *s = ReplyState::Abandoned;
                return Err(ReplyError::Timeout);
            }
            s = self.0.cvar.wait_timeout(s, deadline - now).unwrap().0;
        }
        match std::mem::replace(&mut *s, ReplyState::Abandoned) {
            ReplyState::Ready(v) => Ok(v),
            ReplyState::Abandoned => Err(ReplyError::Disconnected),
            ReplyState::Empty => unreachable!(),
        }
    }
}
