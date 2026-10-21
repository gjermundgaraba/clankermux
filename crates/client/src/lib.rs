//! The client side of the clankermux protocol: a connection with request
//! routing, the attachment state machine, and the pure helpers a terminal
//! client needs (key splitting and the query-answer filter).

pub mod attachment;
pub mod connection;
pub mod expect;
mod queue;
pub mod vt;

pub use attachment::{Action, Attachment};
pub use connection::{Connection, Endpoint, Error, Frame, Receiver};
