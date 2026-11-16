//! The shape of the signal calls.
//!
//! A call that registers, revokes or sends a signal is predicted from its
//! kind: what it returns, what it does while the actor is stopping, and how
//! it is undone. The doc of every call in the table names its kind and links
//! this page.
//!
//! # The kinds
//!
//! | Kind | Returns | Made while the actor is stopping or stopped | Undo |
//! |---|---|---|---|
//! | Registration of a notice or an answer the actor then waits for: `monitor`, `monitor_priority`, `link`, `link_priority`, `monitor_group`, `monitor_scope`, `request`, `request_within` and the event manager's `add_sup_handler` | `Result<Id or (), FamilyError>`; the two group monitors return the current members with the id | refused with the family's `Stopping`. A `Down`, a `LinkExit` and a `HandlerExit` owed at the stop are delivered once there, and a later one could never be delivered; a request's reply and a group monitor's changes pending at the stop are dropped, so one registered then could never be read | a revoke |
//! | Registration of a source of the actor's own: `schedule`, `every`, `every_with`, `add_stream`, `subscribe`, `alias` | the timers `Result<RecurringId, TimerError>`, the others a bare id or `Alias` | accepted, and it produces nothing: everything of this class ends with the actor, as Erlang/OTP cancels the timers of a process that exits | a revoke |
//! | Revoke: `demonitor`, `demonitor_flush`, `unlink`, `cancel_request`, `cancel_timer`, `cancel_stream`, `unsubscribe`, `unalias` | `bool` | answers as always | none |
//! | Send: `exit`, `exit_priority`, the alias sends | `Result<(), E>`; `exit` and `exit_priority` answer `Full` at the receiver's bound; the alias sends wait for room as a handle's `notify` does, and their `try_` forms answer `Full`; none waits on the receiver's own task | accepted: a stopping actor may still signal others | none |
//! | Call from outside, through the mailbox: `send`, `send_timeout`, a machine's `call`, the event manager and the application controller | `AskError` for a handle's own calls; a reference type has its own error with `Timeout`, `NoTimeDriver`, the ended answer and `CallingSelf` | answered `Closed` while the mailbox is closed and the actor has not ended (a reference's own `Closed`, as `EventError::Closed`), and with the final reason once the actor has ended or the call was queued (`EventError::Ended`, `AppError::Ended`, `AskError::ResponseDropped`) | none: a bound ends the caller's wait only |
//! | Call from outside, through the system channel: `SupervisorRef`, `Sys` | the reference's own error, as above | served until the loop leaves the system branch | none |
//!
//! # What a revoke answers
//!
//! A revoke returns `bool`.
//!
//! - A registration of one item (a monitor, a link, a request, a one-shot
//!   timer): `true` when nothing was produced and nothing will be; `false`
//!   when there was no such registration or its item already exists. Then the
//!   family says whether that item is dropped (`demonitor_flush`,
//!   `cancel_request`) or still handled (`demonitor`, `unlink`,
//!   `cancel_timer`).
//! - A registration of many items (a recurring timer, a stream, a
//!   subscription, an alias): `true` when the registration was still held (a
//!   stream that finished and a subscription whose map panicked are held
//!   until the loop takes their end). Nothing more is put in the mailbox after
//!   the call, except an item another worker is putting there at that
//!   instant, and an item already queued is still handled.
//! - An alias is exact: `unalias` waits for a send between its check and its
//!   put. A group monitor's `true` is exact too.
//!
//! # Bounds
//!
//! Each family's bound is one row. Monitors, links, timers and streams have
//! no cap: they are counted by the calls the user makes, as in Erlang/OTP.
//!
//! | Family | Bound |
//! |---|---|
//! | Requests in flight | the actor's mailbox capacity, or the number set with `set_request_cap`; over it `request` answers `RequestError::InFlightLimit` |
//! | Exit signals waiting for a trapping target | the target's mailbox capacity (64 for a supervisor that has not called `trap_exit`); over it `exit` and `exit_priority` answer `ExitError::Full` and send nothing |
//! | A group subscription's queue | the watcher's mailbox capacity; an overflow is reported once as a `Resync` and never blocks a join |
//! | A telemetry subscription's queue | set with `Filter::queue` between 1 and 4096, 64 by default; an overflow is counted and reported as a `SubscriptionNotice::Overflow` |
//! | The system channel | 64 queued requests; a caller of a `SupervisorRef` call waits for room, bounded by the reference's `timeout` (none by default), and a caller of a `Sys` call by its `timeout` (5 seconds by default) |
//! | Monitors, links, timers and streams | none |
//!
//! # Items pending at the owner's stop
//!
//! An owed notice (`Down`, `LinkExit`, `HandlerExit`) is delivered once
//! through `handle`, and none under `Kill`. An answer to the actor's own
//! question (a request's reply), a group monitor's changes and every item of
//! a source of the actor's own are dropped.
