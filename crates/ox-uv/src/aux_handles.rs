use crate::handle::{Callback, HandleKind, PhaseState, wrong_kind};
use crate::{CallbackError, Error, Handle, HandleId, Result, UvLoop};

macro_rules! phase_handle {
    ($name:ident, $variant:ident, $label:literal) => {
        #[doc = concat!("A libuv ", $label, " phase handle.")]
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct $name {
            id: HandleId,
        }

        impl $name {
            #[doc = concat!("Allocates an inactive ", $label, " handle.")]
            pub fn new(uv_loop: &mut UvLoop) -> Result<Self> {
                let id = uv_loop.allocate(HandleKind::$variant(PhaseState {
                    active: false,
                    generation: 0,
                    callback: None,
                }))?;
                Ok(Self { id })
            }

            /// Starts one callback per applicable loop iteration.
            pub fn start<F>(&self, uv_loop: &mut UvLoop, callback: F) -> Result<()>
            where
                F: FnMut(&mut UvLoop, HandleId) -> std::result::Result<(), CallbackError>
                    + 'static,
            {
                let state = uv_loop.state_mut(self.id)?;
                if state.closing {
                    return Err(Error::ClosingHandle(self.id));
                }
                let HandleKind::$variant(inner) = &mut state.kind else {
                    return Err(wrong_kind(self.id, $label));
                };
                inner.active = true;
                inner.generation = inner.generation.wrapping_add(1);
                inner.callback = Some(Box::new(callback) as Callback);
                Ok(())
            }

            /// Stops callbacks without closing the handle.
            pub fn stop(&self, uv_loop: &mut UvLoop) -> Result<()> {
                let state = uv_loop.state_mut(self.id)?;
                let HandleKind::$variant(inner) = &mut state.kind else {
                    return Err(wrong_kind(self.id, $label));
                };
                inner.active = false;
                inner.generation = inner.generation.wrapping_add(1);
                Ok(())
            }
        }

        impl Handle for $name {
            fn id(&self) -> HandleId {
                self.id
            }
        }
    };
}

phase_handle!(Idle, Idle, "idle");
phase_handle!(Prepare, Prepare, "prepare");
phase_handle!(Check, Check, "check");
