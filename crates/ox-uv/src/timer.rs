use std::time::{Duration, Instant};

use crate::handle::{Callback, HandleKind, TimerState, wrong_kind};
use crate::{CallbackError, Error, Handle, HandleId, Result, UvLoop};

/// Millisecond timer handle with libuv repeat semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Timer {
    id: HandleId,
}

impl Timer {
    /// Allocates an inactive timer.
    pub fn new(uv_loop: &mut UvLoop) -> Result<Self> {
        let id = uv_loop.allocate(HandleKind::Timer(TimerState {
            active: false,
            started: false,
            deadline: None,
            repeat: Duration::ZERO,
            generation: 0,
            callback: None,
        }))?;
        Ok(Self { id })
    }

    /// Starts or restarts the timer; zero timeout fires on the next loop turn.
    pub fn start<F>(
        &self,
        uv_loop: &mut UvLoop,
        timeout_ms: u64,
        repeat_ms: u64,
        callback: F,
    ) -> Result<()>
    where
        F: FnMut(&mut UvLoop, HandleId) -> std::result::Result<(), CallbackError> + 'static,
    {
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(timeout_ms))
            .ok_or(Error::TimeOverflow)?;
        let state = uv_loop.state_mut(self.id)?;
        if state.closing {
            return Err(Error::ClosingHandle(self.id));
        }
        let HandleKind::Timer(timer) = &mut state.kind else {
            return Err(wrong_kind(self.id, "timer"));
        };
        timer.active = true;
        timer.started = true;
        timer.deadline = Some(deadline);
        timer.repeat = Duration::from_millis(repeat_ms);
        timer.generation = timer.generation.wrapping_add(1);
        timer.callback = Some(Box::new(callback) as Callback);
        Ok(())
    }

    /// Stops the timer without changing its repeat value or callback.
    pub fn stop(&self, uv_loop: &mut UvLoop) -> Result<()> {
        let timer = timer_state_mut(uv_loop, self.id)?;
        timer.active = false;
        timer.deadline = None;
        timer.generation = timer.generation.wrapping_add(1);
        Ok(())
    }

    /// Restarts a previously-started repeating timer from now.
    pub fn again(&self, uv_loop: &mut UvLoop) -> Result<()> {
        let timer = timer_state_mut(uv_loop, self.id)?;
        if !timer.started {
            return Err(Error::TimerNeverStarted(self.id));
        }
        if timer.repeat.is_zero() {
            return Err(Error::TimerNotRepeating(self.id));
        }
        let Some(deadline) = Instant::now().checked_add(timer.repeat) else {
            timer.active = false;
            timer.deadline = None;
            return Err(Error::TimeOverflow);
        };
        timer.deadline = Some(deadline);
        timer.active = true;
        timer.generation = timer.generation.wrapping_add(1);
        Ok(())
    }

    /// Updates the repeat used after the next callback without moving its current deadline.
    pub fn set_repeat(&self, uv_loop: &mut UvLoop, repeat_ms: u64) -> Result<()> {
        timer_state_mut(uv_loop, self.id)?.repeat = Duration::from_millis(repeat_ms);
        Ok(())
    }

    /// Returns the configured repeat interval in milliseconds.
    pub fn get_repeat(&self, uv_loop: &UvLoop) -> Result<u64> {
        let state = uv_loop.state(self.id).ok_or(Error::InvalidHandle(self.id))?;
        let HandleKind::Timer(timer) = &state.kind else {
            return Err(wrong_kind(self.id, "timer"));
        };
        Ok(u64::try_from(timer.repeat.as_millis()).unwrap_or(u64::MAX))
    }
}

impl Handle for Timer {
    fn id(&self) -> HandleId {
        self.id
    }
}

fn timer_state_mut(uv_loop: &mut UvLoop, id: HandleId) -> Result<&mut TimerState> {
    let state = uv_loop.state_mut(id)?;
    if state.closing {
        return Err(Error::ClosingHandle(id));
    }
    let HandleKind::Timer(timer) = &mut state.kind else {
        return Err(wrong_kind(id, "timer"));
    };
    Ok(timer)
}
