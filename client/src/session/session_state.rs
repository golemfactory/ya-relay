use derive_more::Display;
use educe::Educe;
use std::sync::{Arc, Weak};
use strum_macros::EnumCount;

use crate::direct_session::DirectSession;
use crate::error::{SessionError, TransitionError};

#[derive(Clone, Educe, Display, Debug, EnumCount)]
#[educe(PartialEq)]
pub enum SessionState {
    #[display(fmt = "Outgoing-{}", _0)]
    Outgoing(InitState),
    #[display(fmt = "Incoming-{}", _0)]
    Incoming(InitState),
    #[display(fmt = "Reverse-{}", _0)]
    ReverseConnection(ReverseState),
    #[display(fmt = "Relayed-{}", _0)]
    Relayed(RelayedState),
    /// Holds established session.
    #[display(fmt = "Established")]
    Established(#[educe(PartialEq(ignore))] Weak<DirectSession>),
    /// Last attempt to init session failed. Holds error.
    #[display(fmt = "FailedEstablish")]
    FailedEstablish(SessionError),
    Closing,
    /// Session was closed gracefully. (Still the reason
    /// for closing could be some kind of failure)
    Closed,
    /// Current initialization method failed, but we could try other methods.
    /// This state is equivalent of `ConnectIntent`, but moving to this state doesn't
    /// give you `SessionPermit`.
    RestartConnect,
}

#[derive(Clone, PartialEq, Display, Debug, EnumCount)]
pub enum RelayedState {
    Initializing,
    Ready,
}

#[derive(Clone, Educe, Display, EnumCount)]
#[educe(PartialEq, Debug)]
pub enum ReverseState {
    /// `ReverseConnection` message was sent and we are waiting for other Node
    /// to init connection with us.
    Awaiting,
    #[display(fmt = "{}", _0)]
    InProgress(InitState),
    /// Incoming connection is ready, now we need to return control to code
    /// which requested `ReverseConnection`. This intermediate step will be set,
    /// when reverse `SessionPermit` will be dropped.
    #[display(
        fmt = "Finished-{}",
        "_0.as_ref().map_or_else(|_| \"Err\", |_| \"Ok\")"
    )]
    Finished(
        #[educe(PartialEq(ignore))]
        #[educe(Debug(ignore))]
        Result<Arc<DirectSession>, SessionError>,
    ),
}

#[derive(Clone, PartialEq, Display, Debug, EnumCount)]
pub enum InitState {
    /// This state indicates that someone plans to initialize connection,
    /// so we are not allowed to do this. Instead we should wait until
    /// connection will be ready.
    ConnectIntent,
    /// State set on the beginning of initialization function.
    Initializing,
    /// First round of `Session` requests, challenges sent.
    ChallengeHandshake,
    /// Second round of handshake, challenge response received.
    HandshakeResponse,
    /// Challenge from other party is valid.
    ChallengeVerified,
    /// Session is registered in `SessionLayer`.
    /// We should be ready to receive packets, but shouldn't send packets yet,
    /// because we are waiting for `ResumeForwarding` packet.
    SessionRegistered,
    /// We received (or sent in case of initiator) `ResumeForwarding` packet,
    /// so Protocol initialization part is finished.
    /// From this state session can transition to established.
    Ready,
}

impl InitState {
    #[allow(clippy::match_like_matches_macro)]
    pub fn allowed(&self, new_state: &InitState) -> bool {
        match (self, &new_state) {
            (InitState::ConnectIntent, InitState::Initializing) => true,
            (InitState::Initializing, InitState::ChallengeHandshake) => true,
            (InitState::ChallengeHandshake, InitState::HandshakeResponse) => true,
            (InitState::HandshakeResponse, InitState::ChallengeVerified) => true,
            (InitState::ChallengeVerified, InitState::SessionRegistered) => true,
            (InitState::SessionRegistered, InitState::Ready) => true,
            _ => false,
        }
    }
}

impl RelayedState {
    #[allow(clippy::match_like_matches_macro)]
    pub fn allowed(&self, new_state: &RelayedState) -> bool {
        match (self, &new_state) {
            (RelayedState::Initializing, RelayedState::Ready) => true,
            _ => false,
        }
    }
}

impl ReverseState {
    #[allow(clippy::match_like_matches_macro)]
    pub fn allowed(&self, new_state: &ReverseState) -> bool {
        match (self, &new_state) {
            (ReverseState::Awaiting, ReverseState::InProgress(InitState::ConnectIntent)) => true,
            (ReverseState::InProgress(prev), ReverseState::InProgress(next)) => prev.allowed(next),
            (ReverseState::InProgress(InitState::Ready), ReverseState::Finished(Ok(_))) => true,
            (ReverseState::InProgress(_), ReverseState::Finished(Err(_))) => true,
            _ => false,
        }
    }
}

impl SessionState {
    pub fn transition_incoming(
        &mut self,
        new_state: InitState,
    ) -> Result<SessionState, TransitionError> {
        // If we are initializing incoming session, we don't know if it is due to ReverseConnection
        // message or other Node just started connection without reason.
        // That's why we need to translate `InitState` depending on the context.
        let new_state = match self {
            SessionState::Incoming(_) => SessionState::Incoming(new_state),
            SessionState::ReverseConnection(_) => {
                SessionState::ReverseConnection(ReverseState::InProgress(new_state))
            }
            _ => SessionState::Incoming(new_state),
        };
        self.transition(new_state)
    }

    pub fn transition_outgoing(
        &mut self,
        new_state: InitState,
    ) -> Result<SessionState, TransitionError> {
        self.transition(SessionState::Outgoing(new_state))
    }

    pub fn transition(&mut self, new_state: SessionState) -> Result<SessionState, TransitionError> {
        let allowed = match (&self, &new_state) {
            // Initialization can fail in any state.
            (SessionState::Relayed(_), SessionState::FailedEstablish(_)) => true,
            (SessionState::Incoming(_), SessionState::FailedEstablish(_)) => true,
            (SessionState::Outgoing(_), SessionState::FailedEstablish(_)) => true,
            (SessionState::ReverseConnection(_), SessionState::FailedEstablish(_)) => true,
            (SessionState::RestartConnect, SessionState::FailedEstablish(_)) => true,
            (SessionState::FailedEstablish(_), SessionState::FailedEstablish(_)) => true,
            // Session can be moved to `Established` only if it was set to `Ready` by `SessionProtocol`
            // or in case of `ReverseConnection`, when reverse `SessionPermit` sets `Reverse-Finished` state.
            (SessionState::Incoming(InitState::Ready), SessionState::Established(_)) => true,
            (SessionState::Outgoing(InitState::Ready), SessionState::Established(_)) => true,
            (SessionState::Relayed(RelayedState::Ready), SessionState::Established(_)) => true,
            (
                SessionState::ReverseConnection(ReverseState::Finished(Ok(_))),
                SessionState::Established(_),
            ) => true,
            // Enables reverse connection. When establishing outgoing session, we send `ReverseConnection`
            // and start waiting for other party to send handshake message.
            (
                SessionState::Outgoing(InitState::ConnectIntent),
                SessionState::ReverseConnection(ReverseState::Awaiting),
            ) => true,
            (
                SessionState::RestartConnect,
                SessionState::ReverseConnection(ReverseState::Awaiting),
            ) => true,
            // We can establish Relayed connection if we made connect intent or we tried other methods
            // that failed and we are in restating state.
            (
                SessionState::Outgoing(InitState::ConnectIntent),
                SessionState::Relayed(RelayedState::Initializing),
            ) => true,
            (SessionState::RestartConnect, SessionState::Relayed(RelayedState::Initializing)) => {
                true
            }
            // We can start new session if it was closed, or if we failed to establish it previously.
            (SessionState::Closed, SessionState::Outgoing(InitState::ConnectIntent)) => true,
            (SessionState::Closed, SessionState::Incoming(InitState::ConnectIntent)) => true,
            (
                SessionState::FailedEstablish(_),
                SessionState::Outgoing(InitState::ConnectIntent),
            ) => true,
            (
                SessionState::FailedEstablish(_),
                SessionState::Incoming(InitState::ConnectIntent),
            ) => true,
            // If session is established, we don't want to block this transition to avoid unexpected
            // behaviors. On the other side when in `Established` state, getting transition request
            // to `Established` is already unexpected.
            (SessionState::Established(_), SessionState::Established(_)) => true,
            // In case of every initialization method we can retry using other method.
            (SessionState::Relayed(_), SessionState::RestartConnect) => true,
            (SessionState::Incoming(_), SessionState::RestartConnect) => true,
            (SessionState::Outgoing(_), SessionState::RestartConnect) => true,
            (SessionState::ReverseConnection(_), SessionState::RestartConnect) => true,
            (SessionState::RestartConnect, SessionState::RestartConnect) => true,
            // We can attempt to make p2p connection, when previous initialization method failed.
            (SessionState::RestartConnect, SessionState::Outgoing(InitState::Initializing)) => true,
            // We can start closing only Established session. In other cases we want to make
            // transition to `FailedEstablish` state.
            (SessionState::Established(_), SessionState::Closing) => true,
            (SessionState::Closing, SessionState::Closed) => true,
            (SessionState::Closed, SessionState::Closed) => true,
            (SessionState::Incoming(prev), SessionState::Incoming(next)) => prev.allowed(next),
            (SessionState::Outgoing(prev), SessionState::Outgoing(next)) => prev.allowed(next),
            (SessionState::Relayed(prev), SessionState::Relayed(next)) => prev.allowed(next),
            (SessionState::ReverseConnection(prev), SessionState::ReverseConnection(next)) => {
                prev.allowed(next)
            }
            _ => false,
        };

        if !allowed {
            Err(TransitionError::InvalidTransition(self.clone(), new_state))
        } else {
            *self = new_state;
            Ok(self.clone())
        }
    }

    #[allow(clippy::match_like_matches_macro)]
    pub fn is_finished(&self) -> bool {
        match self {
            SessionState::Established(_)
            | SessionState::Closed
            | SessionState::FailedEstablish(_) => true,
            _ => false,
        }
    }
}

#[allow(dead_code)]
/// This enum describes our knowledge about other Nodes, when
/// we are disconnected, or we have trouble keeping connection alive.
pub enum NodeState {
    /// We didn't try to communicate with this Node yet.
    Unknown,
    /// We don't get any response from other Node for longer period of time.
    /// TODO: We should handle gracefully temporary network problems. Expiration tracker
    ///       could have 2 phases: first finds Nodes not responding to ping. Than he tries
    ///       to ping them in regular interval for certain period of time, hoping that Node will be back.
    ///       This way we could stall connection for this period (pausing forwarding for example), but
    ///       it could work seamlessly afterwards.
    NotResponding,
    /// Node can't be reached.
    Unreachable,
    /// Node was shutdown. How we can know this for sure? Maybe we could pass `Reason` in
    /// `Disconnected` message?
    Shutdown,
    /// Node is reconnecting all the time and connection is unstable, can break at any time.
    Unstable,
}
