//! eidra's agent: the session log, the loop, the tools and the confirmation policy
//! (docs/DESIGN.md §4–§7). It never prints; frontends render its events.

mod agent;
pub mod checkpoint;
mod confirm;
pub mod display;
mod event;
mod home;
pub mod media;
mod profile;
mod session;
pub mod skills;
mod text;
mod tools;

pub use agent::{
    Agent, AgentOptions, Background, Interrupter, Outcome, Report, reply_request, request_stats,
};
pub use confirm::{ConfirmPolicy, Verdict};
pub use event::{Approval, Decision, Event, Frontend, RequestStats};
pub use home::eidra_home;
pub use profile::Profile;
pub use session::{Header, Log, Summary, list as list_sessions, text_of};
pub use text::{fnv1a, truncate_middle, utc_datetime};
pub use tools::run_command;
