//! Capability-first runtime tracker selection.
//!
//! Environment variables are used only to order candidates. A backend is
//! selected only after its transport-specific readiness probe succeeds.

use std::ffi::{OsStr, OsString};
#[cfg(any(
    feature = "niri",
    feature = "wlr-toplevel",
    feature = "gnome",
    feature = "kde"
))]
use std::sync::Mutex;
#[cfg(any(
    feature = "niri",
    feature = "wlr-toplevel",
    feature = "gnome",
    feature = "kde"
))]
use std::time::Duration;
use std::{env, fmt};

use clap::ValueEnum;

use super::WindowTracker;
#[cfg(any(
    feature = "niri",
    feature = "wlr-toplevel",
    feature = "gnome",
    feature = "kde"
))]
use super::{TrackerError, WindowEventSource};

/// Tracker requested by the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum)]
#[value(rename_all = "lower")]
pub enum Backend {
    Auto,
    Niri,
    Wlr,
    Gnome,
    Kde,
}

impl Backend {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Niri => "niri",
            Self::Wlr => "wlr",
            Self::Gnome => "gnome",
            Self::Kde => "kde",
        }
    }

    pub const fn feature(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Niri => Some("niri"),
            Self::Wlr => Some("wlr-toplevel"),
            Self::Gnome => Some("gnome"),
            Self::Kde => Some("kde"),
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// Injectable source of process environment values.
pub trait Environment {
    fn var_os(&self, name: &str) -> Option<OsString>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnvironment;

impl Environment for ProcessEnvironment {
    fn var_os(&self, name: &str) -> Option<OsString> {
        env::var_os(name)
    }
}

/// Untrusted environment observations. These never establish readiness.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnvironmentHints {
    pub niri_socket: Option<OsString>,
    pub sway_socket: Option<OsString>,
    pub hyprland_instance: Option<OsString>,
    pub wayland_display: Option<OsString>,
    pub desktops: Vec<String>,
    pub session_desktops: Vec<String>,
}

impl EnvironmentHints {
    pub fn from_environment(environment: &impl Environment) -> Self {
        Self {
            niri_socket: nonempty(environment.var_os("NIRI_SOCKET")),
            sway_socket: nonempty(environment.var_os("SWAYSOCK")),
            hyprland_instance: nonempty(environment.var_os("HYPRLAND_INSTANCE_SIGNATURE")),
            wayland_display: nonempty(environment.var_os("WAYLAND_DISPLAY")),
            desktops: parse_desktops(environment.var_os("XDG_CURRENT_DESKTOP").as_deref()),
            session_desktops: ["XDG_SESSION_DESKTOP", "DESKTOP_SESSION"]
                .into_iter()
                .flat_map(|name| parse_desktops(environment.var_os(name).as_deref()))
                .collect(),
        }
    }

    pub fn current_desktop_is(&self, expected: &str) -> bool {
        self.desktops
            .iter()
            .any(|desktop| desktop.eq_ignore_ascii_case(expected))
    }

    pub fn has_wlr_compositor_hint(&self) -> bool {
        self.sway_socket.is_some()
            || self.hyprland_instance.is_some()
            || self.current_desktop_is("wlroots")
            || self.current_desktop_is("sway")
            || self.current_desktop_is("hyprland")
    }
}

fn nonempty(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !value.is_empty())
}

pub fn parse_desktops(value: Option<&OsStr>) -> Vec<String> {
    value
        .and_then(OsStr::to_str)
        .into_iter()
        .flat_map(|value| value.split(':'))
        .map(str::trim)
        .filter(|component| !component.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Compile-time availability, separate from runtime readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompiledBackends {
    pub niri: bool,
    pub wlr: bool,
    pub gnome: bool,
    pub kde: bool,
}

impl CompiledBackends {
    /// Compact constructor is useful in exhaustive selection matrix tests.
    #[allow(clippy::fn_params_excessive_bools)]
    pub const fn new(niri: bool, wlr: bool, gnome: bool, kde: bool) -> Self {
        Self {
            niri,
            wlr,
            gnome,
            kde,
        }
    }

    pub const fn is_compiled(self, backend: Backend) -> bool {
        match backend {
            Backend::Auto => true,
            Backend::Niri => self.niri,
            Backend::Wlr => self.wlr,
            Backend::Gnome => self.gnome,
            Backend::Kde => self.kde,
        }
    }
}

/// Safe, non-payload readiness evidence for `info` output.
/// Cross-backend diagnostic vocabulary; subset builds construct only enabled
/// variants.
#[cfg_attr(
    not(all(
        feature = "niri",
        feature = "wlr-toplevel",
        feature = "gnome",
        feature = "kde"
    )),
    allow(dead_code)
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeEvidence {
    NiriIpc,
    WlrManager,
    GnomeBridge { protocol_version: u32 },
    KdeBridge { protocol_version: u32 },
}

impl fmt::Display for ProbeEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NiriIpc => formatter.write_str("IPC handshake and initial snapshot succeeded"),
            Self::WlrManager => {
                formatter.write_str("foreign-toplevel manager bind and initial snapshot succeeded")
            }
            Self::GnomeBridge { protocol_version } => {
                write!(
                    formatter,
                    "authenticated bridge snapshot succeeded (protocol v{protocol_version})"
                )
            }
            Self::KdeBridge { protocol_version } => write!(
                formatter,
                "authenticated bridge snapshot succeeded (protocol v{protocol_version})"
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeFailure {
    /// Redacted diagnostic. Probe implementations must not put window titles,
    /// arbitrary payloads, or attacker-controlled environment values here.
    pub reason: String,
    pub ext_foreign_toplevel_list_detected: bool,
}

impl ProbeFailure {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            ext_foreign_toplevel_list_detected: false,
        }
    }

    #[cfg(test)]
    pub fn ext_list_insufficient(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            ext_foreign_toplevel_list_detected: true,
        }
    }
}

/// A successful probe owns both its evidence and the reusable tracker.
///
/// Implementations which establish an event stream during readiness probing
/// must make the tracker's first `connect()` return that already-connected
/// source. They must not discard it and subscribe a second time.
pub struct ProbeSuccess {
    pub evidence: ProbeEvidence,
    pub tracker: Box<dyn WindowTracker>,
}

pub trait BackendProbes {
    fn probe(&mut self, backend: Backend) -> Result<ProbeSuccess, ProbeFailure>;
}

/// Backends present in this binary. Keep this distinct from runtime probes.
pub const fn compiled_backends() -> CompiledBackends {
    CompiledBackends::new(
        cfg!(feature = "niri"),
        cfg!(feature = "wlr-toplevel"),
        cfg!(feature = "gnome"),
        cfg!(feature = "kde"),
    )
}

/// Production capability probes.
///
/// Each probe performs the backend's real `connect()` handshake. The returned
/// wrapper retains that exact source for the consumer's first `connect()` and
/// delegates later reconnects to the same backend factory.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProductionProbes;

impl BackendProbes for ProductionProbes {
    fn probe(&mut self, backend: Backend) -> Result<ProbeSuccess, ProbeFailure> {
        match backend {
            Backend::Auto => Err(ProbeFailure::new("auto is not a concrete backend")),
            Backend::Niri => {
                #[cfg(feature = "niri")]
                {
                    probe_tracker_with_reconnect(
                        Box::new(super::niri::NiriTracker::probe()),
                        Box::new(super::niri::NiriTracker::default()),
                        ProbeEvidence::NiriIpc,
                    )
                }
                #[cfg(not(feature = "niri"))]
                {
                    Err(ProbeFailure::new("backend is not compiled"))
                }
            }
            Backend::Wlr => {
                #[cfg(feature = "wlr-toplevel")]
                {
                    probe_tracker(
                        Box::new(super::wlr::WlrTracker),
                        // The adapter has established the required manager and
                        // snapshot barrier. Exact negotiated versions can be
                        // added to its public readiness metadata later.
                        ProbeEvidence::WlrManager,
                    )
                }
                #[cfg(not(feature = "wlr-toplevel"))]
                {
                    Err(ProbeFailure::new("backend is not compiled"))
                }
            }
            Backend::Gnome => {
                #[cfg(feature = "gnome")]
                {
                    probe_tracker(
                        Box::new(super::gnome::GnomeTracker),
                        ProbeEvidence::GnomeBridge {
                            protocol_version: 1,
                        },
                    )
                }
                #[cfg(not(feature = "gnome"))]
                {
                    Err(ProbeFailure::new("backend is not compiled"))
                }
            }
            Backend::Kde => {
                #[cfg(feature = "kde")]
                {
                    probe_tracker(
                        Box::new(super::kde::KdeTracker),
                        ProbeEvidence::KdeBridge {
                            protocol_version: 1,
                        },
                    )
                }
                #[cfg(not(feature = "kde"))]
                {
                    Err(ProbeFailure::new("backend is not compiled"))
                }
            }
        }
    }
}

#[cfg(any(feature = "wlr-toplevel", feature = "gnome", feature = "kde"))]
fn probe_tracker(
    tracker: Box<dyn WindowTracker>,
    evidence: ProbeEvidence,
) -> Result<ProbeSuccess, ProbeFailure> {
    let reconnect = tracker;
    probe_tracker_with_reconnect_box(reconnect, evidence)
}

#[cfg(feature = "niri")]
fn probe_tracker_with_reconnect(
    probe: Box<dyn WindowTracker>,
    reconnect: Box<dyn WindowTracker>,
    evidence: ProbeEvidence,
) -> Result<ProbeSuccess, ProbeFailure> {
    let source = probe.connect().map_err(|error| {
        ProbeFailure::new(format!(
            "{} readiness handshake failed: {error}",
            probe.backend_name()
        ))
    })?;
    let source = validate_and_replay_initial_snapshot(source, probe.backend_name())?;
    Ok(ProbeSuccess {
        evidence,
        tracker: Box::new(PreparedTracker {
            backend_name: reconnect.backend_name(),
            tracker: reconnect,
            prepared: Mutex::new(Some(source)),
        }),
    })
}

#[cfg(any(feature = "wlr-toplevel", feature = "gnome", feature = "kde"))]
fn probe_tracker_with_reconnect_box(
    tracker: Box<dyn WindowTracker>,
    evidence: ProbeEvidence,
) -> Result<ProbeSuccess, ProbeFailure> {
    let source = tracker.connect().map_err(|error| {
        ProbeFailure::new(format!(
            "{} readiness handshake failed: {error}",
            tracker.backend_name()
        ))
    })?;
    let source = validate_and_replay_initial_snapshot(source, tracker.backend_name())?;
    Ok(ProbeSuccess {
        evidence,
        tracker: Box::new(PreparedTracker {
            backend_name: tracker.backend_name(),
            tracker,
            prepared: Mutex::new(Some(source)),
        }),
    })
}

#[cfg(any(
    feature = "niri",
    feature = "wlr-toplevel",
    feature = "gnome",
    feature = "kde"
))]
fn validate_and_replay_initial_snapshot(
    mut source: Box<dyn WindowEventSource>,
    backend: &str,
) -> Result<Box<dyn WindowEventSource>, ProbeFailure> {
    let update = match source.recv_timeout(Duration::from_secs(5)) {
        Ok(Some(update @ super::Update::Snapshot(_))) => update,
        Ok(Some(_)) => {
            return Err(ProbeFailure::new(format!(
                "{backend} readiness yielded an incremental update before its snapshot"
            )));
        }
        Ok(None) => {
            return Err(ProbeFailure::new(format!(
                "{backend} readiness timed out waiting for its snapshot"
            )));
        }
        Err(error) => {
            return Err(ProbeFailure::new(format!(
                "{backend} readiness snapshot failed: {error}"
            )));
        }
    };
    Ok(Box::new(ReplayInitialSource {
        initial: Some(update),
        inner: source,
    }))
}

#[cfg(any(
    feature = "niri",
    feature = "wlr-toplevel",
    feature = "gnome",
    feature = "kde"
))]
struct ReplayInitialSource {
    initial: Option<super::Update>,
    inner: Box<dyn WindowEventSource>,
}

#[cfg(any(
    feature = "niri",
    feature = "wlr-toplevel",
    feature = "gnome",
    feature = "kde"
))]
impl WindowEventSource for ReplayInitialSource {
    fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<super::Update>, TrackerError> {
        if let Some(initial) = self.initial.take() {
            return Ok(Some(initial));
        }
        self.inner.recv_timeout(timeout)
    }
}

#[cfg(any(
    feature = "niri",
    feature = "wlr-toplevel",
    feature = "gnome",
    feature = "kde"
))]
struct PreparedTracker {
    backend_name: &'static str,
    tracker: Box<dyn WindowTracker>,
    prepared: Mutex<Option<Box<dyn WindowEventSource>>>,
}

#[cfg(any(
    feature = "niri",
    feature = "wlr-toplevel",
    feature = "gnome",
    feature = "kde"
))]
impl WindowTracker for PreparedTracker {
    fn backend_name(&self) -> &'static str {
        self.backend_name
    }

    fn connect(&self) -> Result<Box<dyn WindowEventSource>, TrackerError> {
        let prepared = self
            .prepared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(source) = prepared {
            return Ok(source);
        }
        self.tracker.connect()
    }
}

/// Select a production backend using the real process environment.
pub fn select_runtime(request: Backend) -> Result<SelectedBackend, DetectionError> {
    let hints = EnvironmentHints::from_environment(&ProcessEnvironment);
    select_backend(request, hints, compiled_backends(), &mut ProductionProbes)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeStatus {
    Usable(ProbeEvidence),
    Unavailable(ProbeFailure),
    FeatureDisabled { feature: &'static str },
    NotProbedAfterSelection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub backend: Backend,
    pub hint: Option<&'static str>,
    pub status: ProbeStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectionReport {
    pub request: Backend,
    pub selected: Option<Backend>,
    pub hints: EnvironmentHints,
    pub compiled: CompiledBackends,
    pub candidates: Vec<Candidate>,
}

impl DetectionReport {
    /// Format diagnostics without exposing environment values or window data.
    pub fn format_info(&self) -> String {
        use std::fmt::Write as _;
        let mut output = String::new();
        let _ = writeln!(output, "Backend request: {}", self.request);
        let _ = writeln!(
            output,
            "Selected backend: {}",
            self.selected.map_or("none", Backend::name)
        );
        output.push_str("Environment hints (ordering only, not capabilities):\n");
        let _ = writeln!(
            output,
            "  NIRI_SOCKET: {}",
            set(self.hints.niri_socket.as_ref())
        );
        let _ = writeln!(
            output,
            "  SWAYSOCK: {}",
            set(self.hints.sway_socket.as_ref())
        );
        let _ = writeln!(
            output,
            "  HYPRLAND_INSTANCE_SIGNATURE: {}",
            set(self.hints.hyprland_instance.as_ref())
        );
        let _ = writeln!(
            output,
            "  WAYLAND_DISPLAY: {}",
            set(self.hints.wayland_display.as_ref())
        );
        let _ = writeln!(
            output,
            "  current desktop components: {} (GNOME: {}, KDE: {})",
            self.hints.desktops.len(),
            self.hints.current_desktop_is("gnome"),
            self.hints.current_desktop_is("kde") || self.hints.current_desktop_is("plasma")
        );
        output.push_str("Capabilities:\n");
        for candidate in &self.candidates {
            let _ = write!(output, "  {}: ", candidate.backend);
            match &candidate.status {
                ProbeStatus::Usable(evidence) => {
                    let _ = writeln!(output, "usable ({evidence})");
                }
                ProbeStatus::Unavailable(failure) => {
                    let ext = if failure.ext_foreign_toplevel_list_detected {
                        "; ext-foreign-toplevel-list detected but insufficient"
                    } else {
                        ""
                    };
                    let _ = writeln!(output, "unavailable ({}{ext})", failure.reason);
                }
                ProbeStatus::FeatureDisabled { feature } => {
                    let _ = writeln!(output, "feature disabled (enable '{feature}')");
                }
                ProbeStatus::NotProbedAfterSelection => {
                    output.push_str("not probed after selection\n");
                }
            }
        }
        output
    }
}

fn set(value: Option<&OsString>) -> &'static str {
    if value.is_some() { "set" } else { "unset" }
}

pub struct SelectedBackend {
    pub backend: Backend,
    pub tracker: Box<dyn WindowTracker>,
    pub report: DetectionReport,
}

impl fmt::Debug for SelectedBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SelectedBackend")
            .field("backend", &self.backend)
            .field("tracker", &self.tracker.backend_name())
            .field("report", &self.report)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectionError {
    pub report: DetectionReport,
}

impl fmt::Display for DetectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.report.request != Backend::Auto {
            let candidate = self
                .report
                .candidates
                .first()
                .expect("explicit request has candidate");
            return match &candidate.status {
                ProbeStatus::FeatureDisabled { feature } => write!(
                    formatter,
                    "backend '{}' is not available in this build; rebuild with --features {feature}",
                    candidate.backend
                ),
                ProbeStatus::Unavailable(failure) => write!(
                    formatter,
                    "backend '{}' unavailable: {}",
                    candidate.backend, failure.reason
                ),
                _ => formatter.write_str("requested backend could not be selected"),
            };
        }

        formatter.write_str("no usable window-tracking backend")?;
        for candidate in &self.report.candidates {
            write!(formatter, "; {}: ", candidate.backend)?;
            match &candidate.status {
                ProbeStatus::Unavailable(failure) => formatter.write_str(&failure.reason)?,
                ProbeStatus::FeatureDisabled { feature } => {
                    write!(formatter, "feature '{feature}' disabled")?;
                }
                ProbeStatus::Usable(_) => formatter.write_str("usable")?,
                ProbeStatus::NotProbedAfterSelection => formatter.write_str("not probed")?,
            }
        }
        Ok(())
    }
}

impl std::error::Error for DetectionError {}

/// Pure candidate ordering. Desktop identity changes priority, never outcome.
pub fn candidate_order(hints: &EnvironmentHints) -> Vec<(Backend, Option<&'static str>)> {
    let mut order = vec![(
        Backend::Niri,
        hints.niri_socket.as_ref().map(|_| "NIRI_SOCKET is set"),
    )];

    for desktop in &hints.desktops {
        let candidate = if desktop.eq_ignore_ascii_case("gnome")
            || desktop.eq_ignore_ascii_case("gnome-classic")
        {
            Some((Backend::Gnome, "current desktop contains GNOME"))
        } else if desktop.eq_ignore_ascii_case("kde") || desktop.eq_ignore_ascii_case("plasma") {
            Some((Backend::Kde, "current desktop contains KDE/Plasma"))
        } else {
            None
        };
        if let Some(candidate) = candidate
            && !order.iter().any(|(backend, _)| *backend == candidate.0)
        {
            order.push((candidate.0, Some(candidate.1)));
        }
    }

    order.push((
        Backend::Wlr,
        hints
            .has_wlr_compositor_hint()
            .then_some("wlroots compositor hint is set"),
    ));
    for backend in [Backend::Gnome, Backend::Kde] {
        if !order.iter().any(|(existing, _)| *existing == backend) {
            order.push((backend, None));
        }
    }
    order
}

pub fn select_backend(
    request: Backend,
    hints: EnvironmentHints,
    compiled: CompiledBackends,
    probes: &mut impl BackendProbes,
) -> Result<SelectedBackend, DetectionError> {
    let order = if request == Backend::Auto {
        candidate_order(&hints)
    } else {
        vec![(request, None)]
    };
    let mut report = DetectionReport {
        request,
        selected: None,
        hints,
        compiled,
        candidates: Vec::with_capacity(order.len()),
    };

    for (index, (backend, hint)) in order.iter().copied().enumerate() {
        if !compiled.is_compiled(backend) {
            report.candidates.push(Candidate {
                backend,
                hint,
                status: ProbeStatus::FeatureDisabled {
                    feature: backend.feature().unwrap_or("niri"),
                },
            });
            continue;
        }

        match probes.probe(backend) {
            Ok(success) => {
                report.selected = Some(backend);
                report.candidates.push(Candidate {
                    backend,
                    hint,
                    status: ProbeStatus::Usable(success.evidence),
                });
                for (remaining, remaining_hint) in order.iter().copied().skip(index + 1) {
                    report.candidates.push(Candidate {
                        backend: remaining,
                        hint: remaining_hint,
                        status: if compiled.is_compiled(remaining) {
                            ProbeStatus::NotProbedAfterSelection
                        } else {
                            ProbeStatus::FeatureDisabled {
                                feature: remaining.feature().unwrap_or("niri"),
                            }
                        },
                    });
                }
                return Ok(SelectedBackend {
                    backend,
                    tracker: success.tracker,
                    report,
                });
            }
            Err(failure) => report.candidates.push(Candidate {
                backend,
                hint,
                status: ProbeStatus::Unavailable(failure),
            }),
        }
    }

    Err(DetectionError { report })
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};

    use super::*;
    use crate::tracker::{TrackerError, WindowEventSource};

    struct NeverTracker;
    impl WindowTracker for NeverTracker {
        fn backend_name(&self) -> &'static str {
            "fake"
        }
        fn connect(&self) -> Result<Box<dyn WindowEventSource>, TrackerError> {
            panic!("pure selector must not connect")
        }
    }

    struct ScriptedProbes {
        results: HashMap<Backend, VecDeque<Result<ProbeEvidence, ProbeFailure>>>,
        calls: Vec<Backend>,
    }

    impl ScriptedProbes {
        fn new(
            results: impl IntoIterator<Item = (Backend, Result<ProbeEvidence, ProbeFailure>)>,
        ) -> Self {
            let mut map: HashMap<Backend, VecDeque<_>> = HashMap::new();
            for (backend, result) in results {
                map.entry(backend).or_default().push_back(result);
            }
            Self {
                results: map,
                calls: Vec::new(),
            }
        }
    }

    impl BackendProbes for ScriptedProbes {
        fn probe(&mut self, backend: Backend) -> Result<ProbeSuccess, ProbeFailure> {
            self.calls.push(backend);
            self.results
                .get_mut(&backend)
                .and_then(VecDeque::pop_front)
                .unwrap_or_else(|| Err(ProbeFailure::new("scripted unavailable")))
                .map(|evidence| ProbeSuccess {
                    evidence,
                    tracker: Box::new(NeverTracker),
                })
        }
    }

    fn hints(desktop: &str) -> EnvironmentHints {
        EnvironmentHints {
            desktops: parse_desktops(Some(OsStr::new(desktop))),
            ..EnvironmentHints::default()
        }
    }
    fn all() -> CompiledBackends {
        CompiledBackends::new(true, true, true, true)
    }
    fn ok(backend: Backend) -> (Backend, Result<ProbeEvidence, ProbeFailure>) {
        let evidence = match backend {
            Backend::Niri => ProbeEvidence::NiriIpc,
            Backend::Wlr => ProbeEvidence::WlrManager,
            Backend::Gnome => ProbeEvidence::GnomeBridge {
                protocol_version: 1,
            },
            Backend::Kde => ProbeEvidence::KdeBridge {
                protocol_version: 1,
            },
            Backend::Auto => unreachable!(),
        };
        (backend, Ok(evidence))
    }
    fn fail(backend: Backend, reason: &str) -> (Backend, Result<ProbeEvidence, ProbeFailure>) {
        (backend, Err(ProbeFailure::new(reason)))
    }

    #[test]
    fn desktop_parser_is_component_based_ascii_insensitive_and_ignores_empty() {
        let parsed = parse_desktops(Some(OsStr::new(":GNOME-Classic::gNoMe:notgnome:")));
        assert_eq!(parsed, ["GNOME-Classic", "gNoMe", "notgnome"]);
        let hints = EnvironmentHints {
            desktops: parsed,
            ..EnvironmentHints::default()
        };
        assert!(hints.current_desktop_is("gnome"));
        assert!(
            !EnvironmentHints {
                desktops: vec!["notgnome".into()],
                ..EnvironmentHints::default()
            }
            .current_desktop_is("gnome")
        );
    }

    #[test]
    fn niri_wins_even_when_wlr_is_usable() {
        let mut probes = ScriptedProbes::new([ok(Backend::Niri), ok(Backend::Wlr)]);
        let selected = select_backend(
            Backend::Auto,
            EnvironmentHints::default(),
            all(),
            &mut probes,
        )
        .unwrap();
        assert_eq!(selected.backend, Backend::Niri);
        assert_eq!(probes.calls, [Backend::Niri]);
    }

    #[test]
    fn stale_niri_hint_falls_through_to_wlr() {
        let environment = EnvironmentHints {
            niri_socket: Some("stale".into()),
            ..EnvironmentHints::default()
        };
        let mut probes = ScriptedProbes::new([fail(Backend::Niri, "refused"), ok(Backend::Wlr)]);
        assert_eq!(
            select_backend(Backend::Auto, environment, all(), &mut probes)
                .unwrap()
                .backend,
            Backend::Wlr
        );
    }

    #[test]
    fn stale_gnome_hint_falls_through_to_wlr() {
        let mut probes = ScriptedProbes::new([
            fail(Backend::Niri, "absent"),
            fail(Backend::Gnome, "no owner"),
            ok(Backend::Wlr),
        ]);
        let selected = select_backend(Backend::Auto, hints("gnome"), all(), &mut probes).unwrap();
        assert_eq!(selected.backend, Backend::Wlr);
        assert_eq!(probes.calls, [Backend::Niri, Backend::Gnome, Backend::Wlr]);
    }

    #[test]
    fn mixed_case_gnome_classic_prioritizes_gnome() {
        assert_eq!(
            candidate_order(&hints("gNoMe-ClAsSiC:GNOME"))[..3]
                .iter()
                .map(|c| c.0)
                .collect::<Vec<_>>(),
            [Backend::Niri, Backend::Gnome, Backend::Wlr]
        );
    }

    #[test]
    fn kde_snapshot_timeout_falls_through() {
        let mut probes = ScriptedProbes::new([
            fail(Backend::Niri, "absent"),
            fail(
                Backend::Kde,
                "KWin owner present; initial snapshot timed out",
            ),
            ok(Backend::Wlr),
        ]);
        assert_eq!(
            select_backend(Backend::Auto, hints("KDE"), all(), &mut probes)
                .unwrap()
                .backend,
            Backend::Wlr
        );
    }

    #[test]
    fn wlr_hints_do_not_establish_capability() {
        let environment = EnvironmentHints {
            sway_socket: Some("stale".into()),
            ..EnvironmentHints::default()
        };
        let mut probes = ScriptedProbes::new([
            fail(Backend::Niri, "absent"),
            fail(Backend::Wlr, "global absent"),
            ok(Backend::Gnome),
        ]);
        assert_eq!(
            select_backend(Backend::Auto, environment, all(), &mut probes)
                .unwrap()
                .backend,
            Backend::Gnome
        );
    }

    #[test]
    fn wlr_without_compositor_hint_can_be_selected() {
        let mut probes = ScriptedProbes::new([fail(Backend::Niri, "absent"), ok(Backend::Wlr)]);
        assert_eq!(
            select_backend(
                Backend::Auto,
                EnvironmentHints::default(),
                all(),
                &mut probes
            )
            .unwrap()
            .backend,
            Backend::Wlr
        );
    }

    #[test]
    fn bridge_is_fallback_after_unavailable_wlr_without_hint() {
        let mut probes = ScriptedProbes::new([
            fail(Backend::Niri, "absent"),
            fail(Backend::Wlr, "global absent"),
            ok(Backend::Gnome),
        ]);
        assert_eq!(
            select_backend(
                Backend::Auto,
                EnvironmentHints::default(),
                all(),
                &mut probes
            )
            .unwrap()
            .backend,
            Backend::Gnome
        );
    }

    #[test]
    fn explicit_override_probes_once_and_never_falls_back() {
        let mut probes = ScriptedProbes::new([fail(Backend::Gnome, "no bridge"), ok(Backend::Wlr)]);
        let error = select_backend(
            Backend::Gnome,
            EnvironmentHints::default(),
            all(),
            &mut probes,
        )
        .unwrap_err();
        assert_eq!(probes.calls, [Backend::Gnome]);
        assert_eq!(error.to_string(), "backend 'gnome' unavailable: no bridge");
    }

    #[test]
    fn explicit_disabled_feature_is_actionable_and_does_not_probe() {
        let mut probes = ScriptedProbes::new([]);
        let error = select_backend(
            Backend::Gnome,
            EnvironmentHints::default(),
            CompiledBackends::new(true, true, false, true),
            &mut probes,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "backend 'gnome' is not available in this build; rebuild with --features gnome"
        );
        assert_eq!(probes.calls, []);
    }

    #[test]
    fn auto_skips_disabled_candidate() {
        let mut probes = ScriptedProbes::new([fail(Backend::Niri, "absent"), ok(Backend::Wlr)]);
        let selected = select_backend(
            Backend::Auto,
            hints("GNOME"),
            CompiledBackends::new(true, true, false, true),
            &mut probes,
        )
        .unwrap();
        assert_eq!(selected.backend, Backend::Wlr);
        assert_eq!(probes.calls, [Backend::Niri, Backend::Wlr]);
    }

    #[test]
    fn aggregate_error_is_stable_and_ordered() {
        let mut probes = ScriptedProbes::new([
            fail(Backend::Niri, "socket absent"),
            fail(Backend::Gnome, "bridge absent"),
            fail(Backend::Wlr, "global absent"),
            fail(Backend::Kde, "KWin absent"),
        ]);
        let error = select_backend(Backend::Auto, hints("GNOME"), all(), &mut probes).unwrap_err();
        assert_eq!(
            error.to_string(),
            "no usable window-tracking backend; niri: socket absent; gnome: bridge absent; wlr: global absent; kde: KWin absent"
        );
    }

    #[test]
    fn info_redacts_environment_values_and_distinguishes_hints() {
        let secret = "SECRET_TITLE_OR_PAYLOAD";
        let environment = EnvironmentHints {
            niri_socket: Some(secret.into()),
            ..hints("gnome")
        };
        let mut probes = ScriptedProbes::new([ok(Backend::Niri)]);
        let info = select_backend(Backend::Auto, environment, all(), &mut probes)
            .unwrap()
            .report
            .format_info();
        assert!(!info.contains(secret));
        assert!(info.contains("ordering only, not capabilities"));
        assert!(info.contains("NIRI_SOCKET: set"));
        assert!(info.contains("gnome: not probed after selection"));
    }

    #[test]
    fn info_reports_ext_list_as_detected_but_insufficient() {
        let mut probes = ScriptedProbes::new([
            fail(Backend::Niri, "absent"),
            (
                Backend::Wlr,
                Err(ProbeFailure::ext_list_insufficient("WLR manager absent")),
            ),
            fail(Backend::Gnome, "absent"),
            fail(Backend::Kde, "absent"),
        ]);
        let info = select_backend(
            Backend::Auto,
            EnvironmentHints::default(),
            all(),
            &mut probes,
        )
        .unwrap_err()
        .report
        .format_info();
        assert!(info.contains("ext-foreign-toplevel-list detected but insufficient"));
    }

    #[cfg(any(
        feature = "niri",
        feature = "wlr-toplevel",
        feature = "gnome",
        feature = "kde"
    ))]
    #[test]
    fn readiness_consumes_validates_and_replays_initial_snapshot() {
        use crate::tracker::fake::{ScriptedSource, Step};
        use crate::tracker::{Snapshot, Update};

        let initial = Update::Snapshot(Snapshot {
            windows: Vec::new(),
            focused: None,
        });
        let source = Box::new(ScriptedSource::new([Step::Update(initial.clone())]));
        let mut replay = validate_and_replay_initial_snapshot(source, "fake").unwrap();
        assert_eq!(
            replay.recv_timeout(Duration::from_millis(1)).unwrap(),
            Some(initial)
        );
    }
}
