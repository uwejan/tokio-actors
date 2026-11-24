//! Request ids: `request`, `request_within` and `cancel_request`.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::{ActorContext, ForwarderKind, PanicSlot};
use crate::actor::handle::ActorHandle;
use crate::actor::monitor::{MonitorShared, MonitorState};
use crate::actor::{request, Actor};
use crate::error::RequestError;
use crate::time_probe::require_runtime_timers;
use crate::types::{ActorStatus, MonitorId, RequestId, RequestReply};

impl<A: Actor> ActorContext<A> {
    /// Asks another actor and gets the answer as one of this actor's own
    /// messages, without waiting: the request goes out from a task of the
    /// crate, and the callee's response, or the reason there is none, arrives
    /// through `map` as a [`RequestReply`] that names the returned
    /// [`RequestId`].
    ///
    /// Kind: a registration of a notice or an answer the actor then waits for; refused with `RequestError::Stopping` while the actor is stopping or stopped. See [`signals`](crate::signals).
    ///
    /// The reply enters this actor's mailbox through an ordinary slot, after
    /// every message the callee sent this actor before it answered, so a full
    /// mailbox delays it like any other message. `map` runs on a task of the
    /// crate, inside a catch: a panic in it fails this actor. The callee's
    /// end gives a reply too: `result` is exactly what
    /// [`send`](crate::actor::handle::ActorHandle::send) would have returned,
    /// including [`AskError::ResponseDropped`](crate::error::AskError) with
    /// the callee's final reason. `map` owns the reply: if it panics while
    /// the response in the reply has a `Drop` that panics too, the second
    /// panic happens during an unwind and ends the process, as it would
    /// anywhere else.
    ///
    /// A request to this actor itself is allowed, and cannot deadlock: it is
    /// handled after the current turn, as any message sent to oneself is.
    ///
    /// At most as many requests as this actor's mailbox holds may be in
    /// flight; [`set_request_cap`](Self::set_request_cap) raises or lowers
    /// the limit. A request leaves the count when its reply reaches `handle`
    /// or it is cancelled with [`cancel_request`](Self::cancel_request). A
    /// request has no deadline of its own: use
    /// [`request_within`](Self::request_within) for one.
    ///
    /// A reply that is still pending when this actor stops, queued or not, is
    /// dropped and never delivered, under any stop reason: it answers this
    /// actor's own question, and nothing is owed. The callee still handles
    /// the request. Only a typed handle names a callee, because `map` needs
    /// the reply type.
    ///
    /// Erlang/OTP: `gen_server:send_request/2`, with `wait_response` and
    /// `check_response` replaced by the mapped message (there is no selective
    /// receive here, so the message is the only way a reply arrives).
    ///
    /// # Errors
    /// - [`RequestError::Stopping`] once this actor is stopping.
    /// - [`RequestError::InFlightLimit`] while the cap is reached.
    ///
    /// # Examples
    /// ```rust
    /// use tokio::sync::mpsc;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorHandle, ActorResult, RequestReply,
    /// };
    ///
    /// struct Doubler;
    ///
    /// impl Actor for Doubler {
    ///     type Message = u32;
    ///     type Response = u32;
    ///     async fn handle(&mut self, msg: u32, _: &mut ActorContext<Self>) -> ActorResult<u32> {
    ///         Ok(msg * 2)
    ///     }
    /// }
    ///
    /// enum Msg {
    ///     Ask(ActorHandle<Doubler>, u32),
    ///     Answer(RequestReply<u32>),
    /// }
    ///
    /// struct Asker(mpsc::UnboundedSender<u32>);
    ///
    /// impl Actor for Asker {
    ///     type Message = Msg;
    ///     type Response = ();
    ///     async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         match msg {
    ///             Msg::Ask(doubler, value) => {
    ///                 ctx.request(&doubler, value, Msg::Answer).expect("the asker is running");
    ///             }
    ///             Msg::Answer(reply) => {
    ///                 let _ = self.0.send(reply.result.expect("the doubler answers"));
    ///             }
    ///         }
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let (answers, mut received) = mpsc::unbounded_channel();
    /// let doubler = Doubler.spawn().await?;
    /// let asker = Asker(answers).spawn().await?;
    /// asker.notify(Msg::Ask(doubler, 21)).await?;
    /// assert_eq!(received.recv().await, Some(42));
    /// # Ok(())
    /// # }
    /// ```
    pub fn request<B, F>(
        &mut self,
        target: &ActorHandle<B>,
        msg: B::Message,
        map: F,
    ) -> Result<RequestId, RequestError>
    where
        B: Actor,
        F: Fn(RequestReply<B::Response>) -> A::Message + Send + Sync + 'static,
    {
        self.register_request(target, msg, None, map)
    }

    /// As [`request`](Self::request), bounded by `within`: one deadline spans
    /// sending the request and waiting for its answer, and when it passes first
    /// the reply carries [`AskError::Timeout`](crate::error::AskError) (its
    /// `enqueued` flag says whether the callee had the request). The callee is
    /// not cancelled, and an answer that comes late is discarded, so the
    /// request gets exactly one reply.
    ///
    /// Kind: a registration of a notice or an answer the actor then waits for; refused with `RequestError::Stopping` while the actor is stopping or stopped. See [`signals`](crate::signals).
    ///
    /// Erlang/OTP: `gen_server:send_request/2` with the timeout of
    /// `receive_response/2`.
    ///
    /// # Panics
    /// Panics, before it does anything else, when this actor's runtime was
    /// built without the Tokio time driver.
    ///
    /// # Errors
    /// As [`request`](Self::request).
    ///
    /// # Examples
    /// ```rust
    /// use std::time::Duration;
    /// use tokio::sync::mpsc;
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorHandle, ActorResult, AskError, RequestReply,
    /// };
    ///
    /// struct Sleeper;
    ///
    /// impl Actor for Sleeper {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         tokio::time::sleep(Duration::from_secs(3600)).await;
    ///         Ok(())
    ///     }
    /// }
    ///
    /// enum Msg {
    ///     Ask(ActorHandle<Sleeper>),
    ///     Answer(RequestReply<()>),
    /// }
    ///
    /// struct Asker(mpsc::UnboundedSender<bool>);
    ///
    /// impl Actor for Asker {
    ///     type Message = Msg;
    ///     type Response = ();
    ///     async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         match msg {
    ///             Msg::Ask(sleeper) => {
    ///                 let within = Duration::from_millis(50);
    ///                 ctx.request_within(&sleeper, (), within, Msg::Answer)
    ///                     .expect("the asker is running");
    ///             }
    ///             Msg::Answer(reply) => {
    ///                 let timed_out = matches!(reply.result, Err(AskError::Timeout { .. }));
    ///                 let _ = self.0.send(timed_out);
    ///             }
    ///         }
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let (answers, mut received) = mpsc::unbounded_channel();
    /// let sleeper = Sleeper.spawn().await?;
    /// let asker = Asker(answers).spawn().await?;
    /// asker.notify(Msg::Ask(sleeper)).await?;
    /// assert_eq!(received.recv().await, Some(true));
    /// # Ok(())
    /// # }
    /// ```
    pub fn request_within<B, F>(
        &mut self,
        target: &ActorHandle<B>,
        msg: B::Message,
        within: Duration,
        map: F,
    ) -> Result<RequestId, RequestError>
    where
        B: Actor,
        F: Fn(RequestReply<B::Response>) -> A::Message + Send + Sync + 'static,
    {
        require_runtime_timers("ActorContext::request_within", &self.runtime);
        self.register_request(target, msg, Some(within), map)
    }

    /// Drops a request's reply: `true` means no reply had been queued and
    /// none will be, `false` means one had been queued, and it is dropped
    /// when its turn comes instead of reaching `handle`. An id this actor did
    /// not make, one that is unknown, and one whose reply was already handed
    /// to `handle` all return `false`.
    ///
    /// Kind: a revoke, answers `bool`. See [`signals`](crate::signals).
    ///
    /// The callee is not told: a request that is already in its mailbox is
    /// still handled, and its answer is discarded. A request still waiting
    /// for room in the callee's mailbox is never sent.
    ///
    /// Erlang/OTP has no call that cancels one request: a request id is
    /// dropped from a collection through the `Delete` argument of
    /// `gen_server:wait_response/3`, `receive_response/3` and
    /// `check_response/3`, and a late reply is discarded by the monitor alias.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorHandle, ActorResult,
    /// };
    ///
    /// struct Echo;
    ///
    /// impl Actor for Echo {
    ///     type Message = u32;
    ///     type Response = u32;
    ///     async fn handle(&mut self, msg: u32, _: &mut ActorContext<Self>) -> ActorResult<u32> {
    ///         Ok(msg)
    ///     }
    /// }
    ///
    /// struct Asker(ActorHandle<Echo>);
    ///
    /// impl Actor for Asker {
    ///     type Message = ();
    ///     type Response = bool;
    ///     async fn handle(&mut self, _: (), ctx: &mut ActorContext<Self>) -> ActorResult<bool> {
    ///         let id = ctx.request(&self.0, 1, |_| ()).expect("running");
    ///         // The handler has not yielded, so no reply can have been queued.
    ///         Ok(ctx.cancel_request(id))
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let echo = Echo.spawn().await?;
    /// let asker = Asker(echo).spawn().await?;
    /// assert!(asker.send(()).await?);
    /// assert_eq!(asker.get_status().await?.requests_in_flight, 0);
    /// # Ok(())
    /// # }
    /// ```
    pub fn cancel_request(&mut self, id: RequestId) -> bool {
        self.monitors
            .as_deref_mut()
            .and_then(|state| state.requests.cancel(id.monitor()))
            .unwrap_or(false)
    }

    /// Sets how many requests this actor may have in flight at once. Unset,
    /// the limit is the capacity of its mailbox. A request over the limit is
    /// refused with [`RequestError::InFlightLimit`]; requests already in
    /// flight are unaffected by a lower limit. A cap of `0` is read as `1`.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{
    ///     actor::{context::ActorContext, Actor, ActorExt},
    ///     ActorHandle, ActorResult,
    /// };
    ///
    /// struct Echo;
    ///
    /// impl Actor for Echo {
    ///     type Message = u32;
    ///     type Response = u32;
    ///     async fn handle(&mut self, msg: u32, _: &mut ActorContext<Self>) -> ActorResult<u32> {
    ///         Ok(msg)
    ///     }
    /// }
    ///
    /// struct Asker(ActorHandle<Echo>);
    ///
    /// impl Actor for Asker {
    ///     // `Some(cap)` starts a round of requests; `None` is a reply.
    ///     type Message = Option<usize>;
    ///     type Response = usize;
    ///     async fn handle(
    ///         &mut self,
    ///         msg: Option<usize>,
    ///         ctx: &mut ActorContext<Self>,
    ///     ) -> ActorResult<usize> {
    ///         let Some(cap) = msg else { return Ok(0) };
    ///         ctx.set_request_cap(cap);
    ///         let mut made = 0;
    ///         while ctx.request(&self.0, 1, |_| None).is_ok() {
    ///             made += 1;
    ///         }
    ///         Ok(made)
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let echo = Echo.spawn().await?;
    /// let asker = Asker(echo).spawn().await?;
    /// assert_eq!(asker.send(Some(3)).await?, 3);
    /// # Ok(())
    /// # }
    /// ```
    pub fn set_request_cap(&mut self, cap: usize) {
        let owner = self.owner();
        self.monitors
            .get_or_insert_with(|| Box::new(MonitorState::new(owner)))
            .requests
            .set_cap(cap);
    }

    /// Makes a request and starts its task. `within` is the deadline of
    /// `request_within`.
    fn register_request<B, F>(
        &mut self,
        target: &ActorHandle<B>,
        msg: B::Message,
        within: Option<Duration>,
        map: F,
    ) -> Result<RequestId, RequestError>
    where
        B: Actor,
        F: Fn(RequestReply<B::Response>) -> A::Message + Send + Sync + 'static,
    {
        if matches!(self.status(), ActorStatus::Stopping | ActorStatus::Stopped) {
            return Err(RequestError::Stopping);
        }
        let default_cap = self.self_handle.mailbox_capacity();
        if self
            .monitors
            .as_deref()
            .is_some_and(|state| !state.requests.has_room(default_cap))
        {
            return Err(RequestError::InFlightLimit);
        }
        let id = MonitorId::next();
        let shared = MonitorShared::for_request(id);
        let token = CancellationToken::new();
        let owner = self.owner();
        self.monitors
            .get_or_insert_with(|| Box::new(MonitorState::new(owner.clone())))
            .requests
            .insert(id, shared.clone(), token.clone());
        let task = request::RequestTask {
            asker: self.self_handle.clone(),
            callee: target.clone(),
            msg,
            within,
            shared,
            token,
            map: request::HeldMap::new(map, owner),
        };
        self.spawn_forwarder(
            ForwarderKind::Request(id),
            request::request_forward(task),
            PanicSlot::default(),
        );
        Ok(RequestId::from_monitor(id))
    }
}
