//! Restoring the windows and tabs of the last run. Everything here is plain data and plain
//! functions: what is saved ([`snapshot`]), what a launch does with it ([`plan`]), how a restored
//! remote tab waits for its connection ([`lazy`]) and when the record is written ([`save`]). The
//! windows, the store and the connections are in `workspace::restore`.
pub(crate) mod lazy;
pub(crate) mod plan;
pub(crate) mod save;
pub(crate) mod snapshot;

pub(crate) use lazy::{Deferred, Event, Step, Wait};
pub(crate) use plan::{Open, Plan, Screen, WindowPlan};
pub(crate) use save::Saver;
pub(crate) use snapshot::{Frame, Located, Source, TabState, WindowState};
