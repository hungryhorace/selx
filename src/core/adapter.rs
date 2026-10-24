//! Adapter trait, fan-out routing, and the focus policy applied afterwards.

use crate::core::event::{EventKind, Origin, SelectionEvent};
use serde::{Deserialize, Serialize};

/// An outbound integration: something that can receive selection events.
///
/// Adapters are used from a single-threaded runtime and need not be `Send`.
pub trait Adapter {
    /// Short stable identifier, e.g. "log", "claude-ide", "text-inject".
    fn name(&self) -> &str;

    /// Whether this adapter can express a weak (ambient) signal.
    /// If `false`, the router never hands it `Ambient` events.
    fn supports_ambient(&self) -> bool;

    /// Deliver one event. Called only with events this adapter accepts.
    ///
    /// Return `Delivery::Dropped` when the event was understood but had no
    /// receiver (e.g. no agent connected); reserve `Err` for real failures.
    fn deliver(&self, event: &SelectionEvent) -> anyhow::Result<Delivery>;

    /// Whether delivering here means an *agent* was told. `false` for
    /// recorders like the event log, which always accept an event and so
    /// would otherwise mask "nothing was actually told" from the user.
    fn is_agent(&self) -> bool {
        true
    }

    /// A client that is about to connect said where it is running, in
    /// multiplexer coordinates. Adapters that can use them — to name the
    /// pane a delivery reached — record them; the rest ignore it. Nothing
    /// guarantees the client ever connects, or that only its own adapter
    /// hears this.
    fn register_origin(&self, _origin: &Origin) {}

    /// Number of connected agent clients; used for idle-shutdown accounting.
    fn client_count(&self) -> usize {
        0
    }

    /// Called once on daemon shutdown (any exit path) so the adapter can
    /// remove anything it owns outside the state dir.
    fn shutdown(&self) {}
}

/// Outcome of a single delivery attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// Delivered, optionally naming the multiplexer pane it reached.
    /// The pane id is opaque to core; only the [`Focuser`] interprets it.
    Delivered(Option<String>),
    Dropped(String),
}

impl Delivery {
    /// Delivered by an adapter with no pane to point at.
    pub fn delivered() -> Delivery {
        Delivery::Delivered(None)
    }
}

/// Moves the user's attention to a pane that just received an event.
///
/// Core holds one of these and calls it at most once per event, with the
/// panes adapters reported, in adapter order. Everything about *which* pane
/// deserves focus — and the rule that the sending pane never does — belongs
/// to the implementation, because core cannot read a pane id.
pub trait Focuser {
    /// Focus one of `panes`. Returns the pane focused, or `None` when none
    /// of them should be.
    fn focus(&self, panes: &[String], event: &SelectionEvent) -> anyhow::Result<Option<String>>;
}

/// When a delivery moves focus. Focus follows *delivery*, not the keypress:
/// an event nothing received must not drag the user anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FocusMode {
    /// Any delivery to a pane moves focus there.
    #[default]
    Delivered,
    /// Only explicit sends move focus; ambient ones deliver silently.
    Explicit,
    /// Never move focus.
    Off,
}

/// Whether an event moves focus at all, before any pane is chosen.
///
/// Three conditions, and each rules out a way of yanking the user somewhere
/// they were not: nothing reached a pane; the mode says no; or the sender
/// named no origin — a send from a plain terminal or a script has no seat
/// in a multiplexer to be moved *from*, so moving one would surprise
/// whoever is sitting at it. Targeting keeps its origin-less fallback: an
/// agent can still be told, it just does not steal the screen.
pub fn should_follow(mode: FocusMode, event: &SelectionEvent, panes: &[String]) -> bool {
    !panes.is_empty() && event.origin.is_some() && mode.wants(event.kind)
}

impl FocusMode {
    /// The one spelling table for this enum; see [`EventKind::as_str`].
    pub fn as_str(self) -> &'static str {
        match self {
            FocusMode::Delivered => "delivered",
            FocusMode::Explicit => "explicit",
            FocusMode::Off => "off",
        }
    }

    /// Every value, for error messages and exhaustive tests.
    pub const VALUES: [FocusMode; 3] = [FocusMode::Delivered, FocusMode::Explicit, FocusMode::Off];

    /// Does an event of this kind move focus at all?
    pub fn wants(self, kind: EventKind) -> bool {
        match self {
            FocusMode::Delivered => true,
            FocusMode::Explicit => kind == EventKind::Explicit,
            FocusMode::Off => false,
        }
    }
}

impl std::fmt::Display for FocusMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for FocusMode {
    type Err = crate::core::event::ParseError;

    fn from_str(s: &str) -> Result<FocusMode, Self::Err> {
        match s {
            "delivered" => Ok(FocusMode::Delivered),
            "explicit" => Ok(FocusMode::Explicit),
            "off" => Ok(FocusMode::Off),
            _ => Err(crate::core::event::ParseError::new(
                "focus mode",
                s,
                &["delivered", "explicit", "off"],
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdapterInfo {
    pub name: String,
    pub supports_ambient: bool,
    pub clients: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct RouteOutcome {
    /// Adapters that accepted the event.
    pub delivered: Vec<String>,
    /// The subset of `delivered` that told an actual agent. Empty means
    /// nobody was told, whatever the recorders did.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
    /// Panes that received it, in adapter order; opaque to core.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panes: Vec<String>,
    /// The pane focus was moved to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused: Option<String>,
    pub dropped: Vec<Dropped>,
    pub failed: Vec<Failed>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dropped {
    pub adapter: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Failed {
    pub adapter: String,
    pub error: String,
}

/// Fan-out: every event goes to every adapter; each adapter's capability
/// filter is applied here so no adapter can accidentally see an event it
/// declared it cannot represent.
#[derive(Default)]
pub struct Router {
    adapters: Vec<Box<dyn Adapter>>,
}

impl Router {
    pub fn new(adapters: Vec<Box<dyn Adapter>>) -> Self {
        Router { adapters }
    }

    pub fn route(&self, event: &SelectionEvent) -> RouteOutcome {
        let mut out = RouteOutcome::default();
        for a in &self.adapters {
            if event.kind == EventKind::Ambient && !a.supports_ambient() {
                out.dropped.push(Dropped {
                    adapter: a.name().to_string(),
                    reason: "adapter does not support ambient events".into(),
                });
                continue;
            }
            match a.deliver(event) {
                Ok(Delivery::Delivered(pane)) => {
                    out.delivered.push(a.name().to_string());
                    if a.is_agent() {
                        out.agents.push(a.name().to_string());
                    }
                    out.panes.extend(pane);
                }
                Ok(Delivery::Dropped(reason)) => out.dropped.push(Dropped {
                    adapter: a.name().to_string(),
                    reason,
                }),
                Err(e) => out.failed.push(Failed {
                    adapter: a.name().to_string(),
                    error: e.to_string(),
                }),
            }
        }
        out
    }

    pub fn register_origin(&self, origin: &Origin) {
        for a in &self.adapters {
            a.register_origin(origin);
        }
    }

    pub fn infos(&self) -> Vec<AdapterInfo> {
        self.adapters
            .iter()
            .map(|a| AdapterInfo {
                name: a.name().to_string(),
                supports_ambient: a.supports_ambient(),
                clients: a.client_count(),
            })
            .collect()
    }

    pub fn client_count(&self) -> usize {
        self.adapters.iter().map(|a| a.client_count()).sum()
    }

    pub fn shutdown(&self) {
        for a in &self.adapters {
            a.shutdown();
        }
    }
}

/// Adapter and focuser fakes.
///
/// `pub(crate)` rather than private to `tests` below: `daemon.rs`'s unit
/// tests route through a `Router` and need exactly these, and a second copy
/// there would be a second thing to keep in step.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    pub(crate) struct Fake {
        pub name: &'static str,
        pub ambient: bool,
        /// Every event this adapter accepted, in order.
        pub seen: Rc<RefCell<Vec<SelectionEvent>>>,
        pub fail: bool,
        pub drop_all: bool,
        /// The pane a delivery reports having reached.
        pub pane: Option<&'static str>,
        pub agent: bool,
        pub clients: usize,
    }

    impl Default for Fake {
        fn default() -> Fake {
            Fake {
                name: "fake",
                ambient: true,
                seen: Default::default(),
                fail: false,
                drop_all: false,
                pane: None,
                agent: true,
                clients: 0,
            }
        }
    }

    impl Adapter for Fake {
        fn name(&self) -> &str {
            self.name
        }
        fn supports_ambient(&self) -> bool {
            self.ambient
        }
        fn deliver(&self, event: &SelectionEvent) -> anyhow::Result<Delivery> {
            if self.fail {
                anyhow::bail!("boom");
            }
            if self.drop_all {
                return Ok(Delivery::Dropped("nobody home".into()));
            }
            self.seen.borrow_mut().push(event.clone());
            Ok(Delivery::Delivered(self.pane.map(String::from)))
        }
        fn is_agent(&self) -> bool {
            self.agent
        }
        fn client_count(&self) -> usize {
            self.clients
        }
    }

    /// Focuses whatever it is offered first, recording every call. `calls`
    /// is shared so a test can still see it after the focuser is boxed.
    #[derive(Default)]
    pub(crate) struct FakeFocuser {
        pub calls: Rc<RefCell<Vec<Vec<String>>>>,
        /// Focus nothing, as a real focuser does when only the sending
        /// pane received the event.
        pub refuse: bool,
        pub fail: bool,
    }

    impl Focuser for FakeFocuser {
        fn focus(
            &self,
            panes: &[String],
            _event: &SelectionEvent,
        ) -> anyhow::Result<Option<String>> {
            self.calls.borrow_mut().push(panes.to_vec());
            if self.fail {
                anyhow::bail!("herdr is not listening");
            }
            if self.refuse {
                return Ok(None);
            }
            Ok(panes.first().cloned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Fake;
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn ev(kind: EventKind) -> SelectionEvent {
        SelectionEvent::new("/ws", "/ws/a.rs", kind)
            .with_lines(1, 2)
            .with_source("test")
    }

    #[test]
    fn ambient_is_dropped_for_non_ambient_adapters() {
        let seen_a = Rc::new(RefCell::new(vec![]));
        let seen_b = Rc::new(RefCell::new(vec![]));
        let router = Router::new(vec![
            Box::new(Fake {
                name: "amb",
                seen: seen_a.clone(),
                ..Fake::default()
            }),
            Box::new(Fake {
                name: "strict",
                ambient: false,
                seen: seen_b.clone(),
                ..Fake::default()
            }),
        ]);

        let out = router.route(&ev(EventKind::Ambient));
        assert_eq!(out.delivered, vec!["amb"]);
        assert_eq!(out.dropped.len(), 1);
        assert_eq!(out.dropped[0].adapter, "strict");
        assert_eq!(seen_a.borrow().len(), 1);
        assert_eq!(seen_b.borrow().len(), 0);

        let out = router.route(&ev(EventKind::Explicit));
        assert_eq!(out.delivered, vec!["amb", "strict"]);
        assert!(out.dropped.is_empty());
        assert_eq!(seen_b.borrow().len(), 1);
    }

    #[test]
    fn adapter_reported_drops_are_recorded() {
        let router = Router::new(vec![Box::new(Fake {
            name: "idle",
            drop_all: true,
            ..Fake::default()
        })]);
        let out = router.route(&ev(EventKind::Explicit));
        assert!(out.delivered.is_empty());
        assert_eq!(out.dropped.len(), 1);
        assert_eq!(out.dropped[0].reason, "nobody home");
    }

    #[test]
    fn failures_are_reported_not_fatal() {
        let router = Router::new(vec![
            Box::new(Fake {
                name: "bad",
                fail: true,
                ..Fake::default()
            }),
            Box::new(Fake {
                name: "good",
                ..Fake::default()
            }),
        ]);
        let out = router.route(&ev(EventKind::Explicit));
        assert_eq!(out.delivered, vec!["good"]);
        assert_eq!(out.failed.len(), 1);
        assert_eq!(out.failed[0].adapter, "bad");
    }

    #[test]
    fn panes_and_agent_deliveries_are_reported_separately() {
        let router = Router::new(vec![
            Box::new(Fake {
                name: "log",
                agent: false,
                ..Fake::default()
            }),
            Box::new(Fake {
                name: "claude",
                ..Fake::default()
            }),
            Box::new(Fake {
                name: "text-inject",
                pane: Some("w1:p2"),
                ..Fake::default()
            }),
        ]);
        let out = router.route(&ev(EventKind::Explicit));
        assert_eq!(out.delivered, vec!["log", "claude", "text-inject"]);
        assert_eq!(out.agents, vec!["claude", "text-inject"]);
        assert_eq!(out.panes, vec!["w1:p2"]);
        assert_eq!(out.focused, None);

        // A recorder alone is not an agent: this is what tells the user
        // nothing was actually told.
        let router = Router::new(vec![Box::new(Fake {
            name: "log",
            agent: false,
            ..Fake::default()
        })]);
        let out = router.route(&ev(EventKind::Ambient));
        assert_eq!(out.delivered, vec!["log"]);
        assert!(out.agents.is_empty());
        assert!(out.panes.is_empty());
    }

    fn from_pane(kind: EventKind) -> SelectionEvent {
        ev(kind).with_origin(crate::core::Origin {
            mux: "herdr".into(),
            session: None,
            workspace: Some("w1".into()),
            tab: Some("w1:t1".into()),
            pane: Some("w1:p1".into()),
        })
    }

    #[test]
    fn focus_needs_a_pane_a_mode_and_a_sender_with_a_seat() {
        let panes = vec!["w1:p2".to_string()];
        assert!(should_follow(
            FocusMode::Delivered,
            &from_pane(EventKind::Ambient),
            &panes
        ));
        // Nothing reached a pane: a dropped send leaves the user put.
        assert!(!should_follow(
            FocusMode::Delivered,
            &from_pane(EventKind::Ambient),
            &[]
        ));
        // No origin: the sender has no seat to be moved from, so a plain
        // terminal never yanks a herdr pane into view. Targeting is
        // unaffected — the agent was still told.
        assert!(!should_follow(
            FocusMode::Delivered,
            &ev(EventKind::Explicit),
            &panes
        ));
        assert!(!should_follow(
            FocusMode::Explicit,
            &from_pane(EventKind::Ambient),
            &panes
        ));
        assert!(!should_follow(
            FocusMode::Off,
            &from_pane(EventKind::Explicit),
            &panes
        ));
    }

    #[test]
    fn focus_modes() {
        for (mode, ambient, explicit) in [
            (FocusMode::Delivered, true, true),
            (FocusMode::Explicit, false, true),
            (FocusMode::Off, false, false),
        ] {
            assert_eq!(mode.wants(EventKind::Ambient), ambient, "{mode:?}");
            assert_eq!(mode.wants(EventKind::Explicit), explicit, "{mode:?}");
        }
        assert_eq!(FocusMode::default(), FocusMode::Delivered);
    }

    /// `Display`, `FromStr` and serde must spell every value the same way.
    /// Three tables that agree today; this is what stops them drifting —
    /// the config file, `SELX_FOCUS` and `status --json` each go through a
    /// different one.
    #[test]
    fn spellings_match_the_wire_format() {
        for mode in FocusMode::VALUES {
            let name = mode.to_string();
            assert_eq!(name, mode.as_str());
            assert_eq!(name.parse(), Ok(mode));
            assert_eq!(serde_json::to_string(&mode).unwrap(), format!("\"{name}\""));
            assert_eq!(
                serde_json::from_str::<FocusMode>(&format!("\"{name}\"")).unwrap(),
                mode
            );
        }
        for kind in EventKind::VALUES {
            let name = kind.to_string();
            assert_eq!(name, kind.as_str());
            assert_eq!(name.parse(), Ok(kind));
            assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{name}\""));
        }

        // A bad value names what it wanted and what it got.
        let e = "sometimes".parse::<FocusMode>().unwrap_err();
        assert_eq!(
            e.to_string(),
            "focus mode must be delivered, explicit or off (got \"sometimes\")"
        );
        let e = "maybe".parse::<EventKind>().unwrap_err();
        assert_eq!(
            e.to_string(),
            "event kind must be ambient or explicit (got \"maybe\")"
        );
    }
}
