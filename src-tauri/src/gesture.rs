//! Coyote 3.0 toggle-switch gestures.
//!
//! Pure session and patch logic. Callers feed notify bytes and write events.
//! No radio, no window, no Tauri types.

use crate::modulation::{ChannelConfig, CurveType, ParameterSource, ParameterSourceType};

/// One side of the powerbox. The A switch is channel A, the B switch is channel B.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    A,
    B,
}

/// A local change reported by the powerbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    /// Signed steps. Positive is an upward flick.
    Flick { side: Side, steps: i16 },
    Press { side: Side },
}

impl Gesture {
    pub fn side(self) -> Side {
        match self {
            Gesture::Flick { side, .. } | Gesture::Press { side } => side,
        }
    }
}

/// Absolute field values the window copies. Setting them again changes nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IntensityPatch {
    pub ceiling: Option<f64>,
    pub static_value: Option<f64>,
}

/// In-flight slider edits. The gesture overwrites only the field it owns.
#[derive(Debug, Clone, Default)]
pub struct SliderEdit {
    pub ceiling: Option<f64>,
    pub floor: Option<f64>,
    pub static_value: Option<f64>,
    pub axis: Option<String>,
    pub curve: Option<CurveType>,
    pub delay_ms: Option<Option<u32>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedB1 {
    /// Serial 1: our intensity write coming back. Update the baseline, no gesture.
    echo: bool,
    /// `None` when that side's byte is above 200 and must be ignored.
    sides: [Option<u8>; 2],
}

/// Serial 0 is a toggle. Serial 1 is the echo of our own intensity write.
/// Any other serial drops the packet.
fn parse_b1(bytes: &[u8]) -> Option<ParsedB1> {
    if bytes.len() < 4 || bytes[0] != 0xB1 || (bytes[1] != 0 && bytes[1] != 1) {
        return None;
    }
    let side = |value: u8| if value <= 200 { Some(value) } else { None };
    Some(ParsedB1 {
        echo: bytes[1] == 1,
        sides: [side(bytes[2]), side(bytes[3])],
    })
}

/// Arming and baseline for one connection.
///
/// Coyote 2 sessions are constructed disabled and never arm.
#[derive(Debug, Clone)]
pub struct GestureSession {
    enabled: bool,
    soft_limit_ok: bool,
    output_ok: bool,
    armed: bool,
    baseline: [Option<u8>; 2],
    /// Intensities the last successful absolute write sent.
    commanded: [Option<u8>; 2],
    /// Absolute knob write currently in flight. A B1 of these values is that
    /// write, not a toggle.
    pending: [Option<u8>; 2],
    /// Baseline at the moment `pending` was set, per side. Finish adopts the
    /// written knob only when nothing has moved this side since then.
    baseline_at_write: [Option<u8>; 2],
    /// Level before the oldest absolute write that has not been echoed yet.
    /// A late serial-0 report is measured against this as well as the baseline.
    anchor: [Option<u8>; 2],
    /// Value of the latest absolute write on this side. A serial-1 echo applies
    /// only when it matches.
    latest_write: [Option<u8>; 2],
    /// A serial-0 report already moved this side's baseline since the latest write.
    serial0_moved: [bool; 2],
}

impl GestureSession {
    pub fn coyote3() -> Self {
        Self::new(true)
    }

    pub fn coyote2() -> Self {
        Self::new(false)
    }

    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            soft_limit_ok: false,
            output_ok: false,
            armed: false,
            baseline: [None, None],
            commanded: [None, None],
            pending: [None, None],
            baseline_at_write: [None, None],
            anchor: [None, None],
            latest_write: [None, None],
            serial0_moved: [false, false],
        }
    }

    pub fn is_armed(&self) -> bool {
        self.armed
    }

    pub fn baseline(&self) -> [Option<u8>; 2] {
        self.baseline
    }

    /// A successful BF soft-limit write. Arms only once an output write has also succeeded.
    pub fn soft_limit_written(&mut self) {
        if !self.enabled {
            return;
        }
        self.soft_limit_ok = true;
        self.armed = self.soft_limit_ok && self.output_ok;
    }

    /// A successful absolute write of both channel knobs.
    pub fn output_written(&mut self, channel_a: u8, channel_b: u8) {
        if !self.enabled {
            return;
        }
        self.pending = [None, None];
        let commanded = [Some(channel_a.min(200)), Some(channel_b.min(200))];
        for index in 0..2 {
            if self.anchor[index].is_none() {
                self.anchor[index] = self.baseline[index];
            }
            self.latest_write[index] = commanded[index];
            self.serial0_moved[index] = false;
        }
        self.commanded = commanded;
        self.baseline = commanded;
        self.output_ok = true;
        self.armed = self.soft_limit_ok && self.output_ok;
    }

    /// Remember an absolute knob write that has been handed to the radio but
    /// not yet acknowledged. `None` leaves that side's in-flight value alone.
    pub fn begin_level_write(&mut self, levels: [Option<u8>; 2]) {
        if !self.enabled {
            return;
        }
        for index in 0..2 {
            if let Some(level) = levels[index] {
                let level = level.min(200);
                if self.anchor[index].is_none() {
                    self.anchor[index] = self.baseline[index];
                }
                self.latest_write[index] = Some(level);
                self.serial0_moved[index] = false;
                self.baseline_at_write[index] = self.baseline[index];
                self.pending[index] = Some(level);
            }
        }
    }

    /// The absolute write finished. On success the in-flight values become the
    /// knob the next toggle is measured from.
    pub fn finish_level_write(&mut self, ok: bool) {
        if self.enabled && ok {
            for index in 0..2 {
                if let Some(level) = self.pending[index] {
                    if self.baseline[index] == self.baseline_at_write[index] {
                        self.commanded[index] = Some(level);
                        self.baseline[index] = Some(level);
                    }
                }
            }
            self.output_ok = true;
            self.armed = self.soft_limit_ok && self.output_ok;
        }
        self.pending = [None, None];
    }

    /// Disconnect clears the baseline and disarms. The next connection needs both writes again.
    pub fn disconnect(&mut self) {
        let enabled = self.enabled;
        *self = Self::new(enabled);
    }

    /// Classify one notify. A report that is not armed or malformed leaves the
    /// baseline where it is. A serial-1 echo updates the baseline and is not a gesture.
    pub fn notify(&mut self, bytes: &[u8]) -> Vec<Gesture> {
        if !self.enabled || !self.armed {
            return Vec::new();
        }
        let Some(parsed) = parse_b1(bytes) else {
            return Vec::new();
        };

        if parsed.echo {
            self.apply_echo(parsed.sides);
            return Vec::new();
        }

        let mut gestures = Vec::new();
        for (index, reported) in parsed.sides.into_iter().enumerate() {
            let Some(reported) = reported else {
                continue;
            };
            let Some(baseline) = self.baseline[index] else {
                continue;
            };
            if self.pending[index] == Some(reported) || reported == baseline {
                continue;
            }
            let side = if index == 0 { Side::A } else { Side::B };
            // A drop to 0 is a press from the last confirmed level, including
            // while a different knob write is in flight.
            if reported == 0 && baseline >= 2 {
                self.adopt_serial0(index, 0);
                gestures.push(Gesture::Press { side });
                continue;
            }
            let in_flight = self.pending[index].is_some();
            let reference = closer_level(
                reported,
                baseline,
                if in_flight {
                    self.pending[index]
                } else {
                    self.anchor[index]
                },
            );
            let steps = reported as i16 - reference as i16;
            if steps == 0 {
                continue;
            }
            // After the write has returned, a report nearer the pre-write level
            // is one step from that level. Do not store that byte: pause must
            // keep measuring later flicks from the zero the box is holding.
            let from_old_level = !in_flight
                && self.anchor[index] == Some(reference)
                && self.anchor[index] != Some(baseline);
            gestures.push(Gesture::Flick { side, steps });
            if !from_old_level {
                self.adopt_serial0(index, reported);
            }
        }
        gestures
    }

    fn adopt_serial0(&mut self, index: usize, reported: u8) {
        self.baseline[index] = Some(reported);
        self.commanded[index] = Some(reported);
        self.serial0_moved[index] = true;
    }

    fn apply_echo(&mut self, sides: [Option<u8>; 2]) {
        for (index, reported) in sides.into_iter().enumerate() {
            let Some(reported) = reported else {
                continue;
            };
            if self.latest_write[index] != Some(reported) {
                continue;
            }
            self.anchor[index] = None;
            if !self.serial0_moved[index] {
                self.baseline[index] = Some(reported);
                self.commanded[index] = Some(reported);
            }
            self.serial0_moved[index] = false;
        }
    }
}

/// While a knob write is in flight, a serial-0 report is measured from
/// whichever of the old baseline and the pending knob it is closer to.
/// A tie stays on the baseline, so a step still in flight from the old knob
/// is not pulled toward the write.
fn closer_level(reported: u8, baseline: u8, pending: Option<u8>) -> u8 {
    let Some(pending) = pending else {
        return baseline;
    };
    let from_pending = (reported as i16 - pending as i16).unsigned_abs();
    let from_baseline = (reported as i16 - baseline as i16).unsigned_abs();
    if from_pending < from_baseline {
        pending
    } else {
        baseline
    }
}

/// Apply one gesture to one intensity source.
///
/// Linked sources move only `range_max`. Fixed sources move only `static_value`.
/// A cap below the linked floor leaves the source unchanged.
pub fn apply_intensity_gesture(
    source: &ParameterSource,
    gesture: Gesture,
    cap: u8,
) -> ParameterSource {
    let mut next = source.clone();
    match source.source_type {
        ParameterSourceType::Linked => {
            if (cap as f64) < source.range_min {
                return next;
            }
            let ceiling = match gesture {
                Gesture::Press { .. } => source.range_min,
                Gesture::Flick { steps, .. } => {
                    (source.range_max + steps as f64).clamp(source.range_min, cap as f64)
                }
            };
            next.range_max = ceiling;
        }
        ParameterSourceType::Static => {
            let current = source.static_value.unwrap_or(0.0);
            let value = match gesture {
                Gesture::Press { .. } => 0.0,
                Gesture::Flick { steps, .. } => (current + steps as f64).clamp(0.0, cap as f64),
            };
            next.static_value = Some(value);
        }
    }
    next
}

/// Copy absolute patch fields onto a source. A second copy is a no-op.
pub fn apply_intensity_patch(source: &ParameterSource, patch: &IntensityPatch) -> ParameterSource {
    let mut next = source.clone();
    if let Some(ceiling) = patch.ceiling {
        next.range_max = ceiling;
    }
    if let Some(value) = patch.static_value {
        next.static_value = Some(value);
    }
    next
}

pub fn intensity_patch(before: &ParameterSource, after: &ParameterSource) -> IntensityPatch {
    IntensityPatch {
        ceiling: (before.range_max != after.range_max).then_some(after.range_max),
        static_value: (before.static_value != after.static_value).then(|| after.static_value.unwrap_or(0.0)),
    }
}

/// The gesture's field is taken from the committed source. Slider edits of every other field remain.
pub fn apply_gesture_over_slider(
    committed: &ParameterSource,
    slider: &SliderEdit,
    gesture: Gesture,
    cap: u8,
) -> ParameterSource {
    let patched = apply_intensity_gesture(committed, gesture, cap);
    let owns_ceiling = committed.source_type == ParameterSourceType::Linked;
    let owns_static = committed.source_type == ParameterSourceType::Static;

    let mut merged = committed.clone();
    if let Some(floor) = slider.floor {
        merged.range_min = floor;
    }
    if let Some(axis) = &slider.axis {
        merged.source_axis = Some(axis.clone());
    }
    if let Some(curve) = slider.curve.clone() {
        merged.curve = curve;
    }
    if let Some(delay) = slider.delay_ms {
        merged.delay_ms = delay;
    }
    if owns_ceiling {
        merged.range_max = patched.range_max;
    } else if let Some(ceiling) = slider.ceiling {
        merged.range_max = ceiling;
    }
    if owns_static {
        merged.static_value = patched.static_value;
    } else if let Some(value) = slider.static_value {
        merged.static_value = Some(value);
    }
    merged
}

/// Patch only the gestured channel. The other channel is returned unchanged.
pub fn apply_gesture_to_pair(
    channel_a: &ChannelConfig,
    channel_b: &ChannelConfig,
    gesture: Gesture,
    cap_a: u8,
    cap_b: u8,
) -> (ChannelConfig, ChannelConfig) {
    match gesture.side() {
        Side::A => (
            apply_gesture_to_config(channel_a, gesture, cap_a),
            channel_b.clone(),
        ),
        Side::B => (
            channel_a.clone(),
            apply_gesture_to_config(channel_b, gesture, cap_b),
        ),
    }
}

fn apply_gesture_to_config(
    config: &ChannelConfig,
    gesture: Gesture,
    cap: u8,
) -> ChannelConfig {
    let mut next = config.clone();
    next.intensity = apply_intensity_gesture(&config.intensity, gesture, cap);
    next
}

/// Append one investigation line to the ring log and to `gesture-trace.log`
/// beside the executable. Called on connect, on each B1, and about once a
/// second while a Coyote 3 output write is succeeding.
pub fn trace_line(message: &str) {
    crate::log_info!("[gesture] {message}");
    crate::logging::flush_now();

    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("gesture-trace.log"))
    else {
        return;
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    use std::io::Write;
    let _ = writeln!(file, "[{timestamp}] {message}");
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn fmt_baseline(baseline: [Option<u8>; 2]) -> String {
    format!("A={} B={}", fmt_level(baseline[0]), fmt_level(baseline[1]))
}

fn fmt_level(value: Option<u8>) -> String {
    match value {
        Some(level) => level.to_string(),
        None => "-".to_string(),
    }
}

fn fmt_gestures(gestures: &[Gesture]) -> String {
    if gestures.is_empty() {
        return "none".to_string();
    }
    gestures
        .iter()
        .map(|gesture| match gesture {
            Gesture::Flick { side, steps } => format!("flick {side:?} {steps:+}"),
            Gesture::Press { side } => format!("press {side:?}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn channel_id_label(channel_id: crate::processing::ChannelId) -> char {
    channel_id.as_char()
}

fn session_slot() -> &'static tokio::sync::Mutex<GestureSession> {
    static SESSION: std::sync::OnceLock<tokio::sync::Mutex<GestureSession>> = std::sync::OnceLock::new();
    SESSION.get_or_init(|| tokio::sync::Mutex::new(GestureSession::coyote2()))
}

/// Install a fresh session for the connection that just succeeded.
pub async fn install_session(session: GestureSession) {
    *session_slot().lock().await = session;
}

pub async fn disconnect_session() {
    session_slot().lock().await.disconnect();
}

pub async fn note_soft_limit_written() {
    session_slot().lock().await.soft_limit_written();
}

pub async fn note_output_written(channel_a: u8, channel_b: u8) {
    session_slot().lock().await.output_written(channel_a, channel_b);
}

/// Ceiling (or fixed value) and the known device knob, sampled while the
/// session lock is held so a toggle cannot update one without the other.
pub async fn sample_intensity_bounds() -> IntensityBounds {
    let session = session_slot().lock().await;
    let known = session.baseline();
    let state = crate::processing::get_processing_state().await;
    let guard = state.read().await;
    let mut bounds = IntensityBounds {
        known,
        range_min: [0, 0],
        range_max: [0, 0],
        is_static: [false, false],
        static_value: [0, 0],
    };
    for (index, channel_id) in [
        crate::processing::ChannelId::A,
        crate::processing::ChannelId::B,
    ]
    .into_iter()
    .enumerate()
    {
        let source = &guard.channel(channel_id).config.intensity;
        bounds.range_min[index] = source.range_min.clamp(0.0, 200.0).round() as u8;
        bounds.range_max[index] = source.range_max.clamp(0.0, 200.0).round() as u8;
        bounds.is_static[index] = source.source_type == ParameterSourceType::Static;
        bounds.static_value[index] = source
            .static_value
            .unwrap_or(0.0)
            .clamp(0.0, 200.0)
            .round() as u8;
    }
    bounds
}

/// Knob inputs for one output tick. `known` is the device level the session
/// believes is current.
#[derive(Debug, Clone, Copy)]
pub struct IntensityBounds {
    pub known: [Option<u8>; 2],
    pub range_min: [u8; 2],
    pub range_max: [u8; 2],
    pub is_static: [bool; 2],
    pub static_value: [u8; 2],
}

pub async fn begin_level_write(levels: [Option<u8>; 2]) {
    session_slot().lock().await.begin_level_write(levels);
}

pub async fn finish_level_write(ok: bool) {
    session_slot().lock().await.finish_level_write(ok);
}

/// Classify a notify and store any intensity change before releasing the session.
///
/// The output tick samples the baseline and the ceiling while holding this same
/// session lock, so both values are visible together. Log lines are written
/// after the locks are released.
pub async fn ingest_notify(bytes: &[u8]) {
    let settings = crate::settings::get_settings().await;
    let caps = [
        settings.general.channel_a_max_intensity.min(200),
        settings.general.channel_b_max_intensity.min(200),
    ];

    let (notify_line, commit_lines, patches) = {
        let mut session = session_slot().lock().await;
        let armed = session.is_armed();
        let before = session.baseline();
        let gestures = session.notify(bytes);
        let after = session.baseline();
        let notify_line = format!(
            "notify hex={} armed={armed} before={} after={} gestures={}",
            hex_bytes(bytes),
            fmt_baseline(before),
            fmt_baseline(after),
            fmt_gestures(&gestures),
        );

        let mut commit_lines = Vec::new();
        let mut patches = Vec::new();
        if !gestures.is_empty() {
            let state = crate::processing::get_processing_state().await;
            let mut guard = state.write().await;
            for gesture in gestures {
                let (channel_id, cap_index) = match gesture.side() {
                    Side::A => (crate::processing::ChannelId::A, 0),
                    Side::B => (crate::processing::ChannelId::B, 1),
                };
                let intensity = &mut guard.channel_mut(channel_id).config.intensity;
                let before_source = intensity.clone();
                let after_source =
                    apply_intensity_gesture(&before_source, gesture, caps[cap_index]);
                commit_lines.push(format!(
                    "commit {} {gesture:?} ceiling {:.0}->{:.0} static {:?}->{:?} cap={}",
                    channel_id_label(channel_id),
                    before_source.range_max,
                    after_source.range_max,
                    before_source.static_value,
                    after_source.static_value,
                    caps[cap_index],
                ));
                let patch = intensity_patch(&before_source, &after_source);
                *intensity = after_source;
                patches.push((channel_id, patch));
            }
        }
        (notify_line, commit_lines, patches)
    };

    trace_line(&notify_line);
    for line in commit_lines {
        trace_line(&line);
    }
    for (channel_id, patch) in patches {
        if patch.ceiling.is_none() && patch.static_value.is_none() {
            continue;
        }
        crate::emit_intensity_field(channel_id, patch.ceiling, patch.static_value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b1(serial: u8, a: u8, b: u8) -> Vec<u8> {
        vec![0xB1, serial, a, b]
    }

    fn arm(session: &mut GestureSession, a: u8, b: u8) {
        session.soft_limit_written();
        session.output_written(a, b);
        assert!(session.is_armed());
    }

    fn linked(min: f64, max: f64) -> ParameterSource {
        let mut source = ParameterSource::linked_source("L0", min, max, CurveType::Exponential);
        source.curve_strength = Some(2.5);
        source.delay_ms = Some(40);
        source.midpoint = Some(true);
        source.static_value = Some(7.0);
        source
    }

    fn fixed(value: f64) -> ParameterSource {
        let mut source = ParameterSource::static_source(value);
        source.range_min = 4.0;
        source.range_max = 80.0;
        source.source_axis = Some("R2".to_string());
        source.curve = CurveType::Logarithmic;
        source.delay_ms = Some(25);
        source
    }

    fn assert_linked_untouched(before: &ParameterSource, after: &ParameterSource) {
        assert_eq!(after.source_type, before.source_type);
        assert_eq!(after.range_min, before.range_min);
        assert_eq!(after.source_axis, before.source_axis);
        assert_eq!(after.curve, before.curve);
        assert_eq!(after.curve_strength, before.curve_strength);
        assert_eq!(after.delay_ms, before.delay_ms);
        assert_eq!(after.midpoint, before.midpoint);
        assert_eq!(after.static_value, before.static_value);
        assert_eq!(after.buttplug_links.is_none(), before.buttplug_links.is_none());
    }

    fn assert_fixed_untouched(before: &ParameterSource, after: &ParameterSource) {
        assert_eq!(after.source_type, before.source_type);
        assert_eq!(after.range_min, before.range_min);
        assert_eq!(after.range_max, before.range_max);
        assert_eq!(after.source_axis, before.source_axis);
        assert_eq!(after.curve, before.curve);
        assert_eq!(after.delay_ms, before.delay_ms);
    }

    #[test]
    fn parses_b1_and_ignores_trailing_short_and_bad_serial() {
        let mut session = GestureSession::coyote3();
        arm(&mut session, 10, 10);

        let mut packet = b1(0, 11, 10);
        packet.extend_from_slice(&[0xFF, 0xEE]);
        assert_eq!(
            session.notify(&packet),
            vec![Gesture::Flick { side: Side::A, steps: 1 }]
        );

        assert!(session.notify(&[0xB1, 0, 12]).is_empty());
        assert_eq!(session.baseline()[0], Some(11));

        // 40 is not the latest write (10), so this echo does not move the baseline.
        assert!(session.notify(&b1(1, 40, 40)).is_empty());
        assert_eq!(session.baseline(), [Some(11), Some(10)]);

        assert!(session.notify(&b1(2, 12, 10)).is_empty());
        assert_eq!(session.baseline(), [Some(11), Some(10)]);

        assert!(session.notify(&[0xB0, 0, 12, 10]).is_empty());
        assert_eq!(session.baseline(), [Some(11), Some(10)]);
    }

    #[test]
    fn in_flight_flick_is_measured_from_the_closer_level() {
        let mut toward_write = GestureSession::coyote3();
        arm(&mut toward_write, 40, 10);
        toward_write.begin_level_write([Some(80), None]);
        assert_eq!(
            toward_write.notify(&b1(0, 81, 10)),
            vec![Gesture::Flick {
                side: Side::A,
                steps: 1
            }]
        );
        assert_eq!(toward_write.baseline(), [Some(81), Some(10)]);
        toward_write.finish_level_write(true);
        assert_eq!(toward_write.baseline(), [Some(81), Some(10)]);

        let mut toward_old = GestureSession::coyote3();
        arm(&mut toward_old, 40, 10);
        toward_old.begin_level_write([Some(80), None]);
        assert_eq!(
            toward_old.notify(&b1(0, 41, 10)),
            vec![Gesture::Flick {
                side: Side::A,
                steps: 1
            }]
        );
        assert_eq!(toward_old.baseline(), [Some(41), Some(10)]);

        let mut press = GestureSession::coyote3();
        arm(&mut press, 40, 10);
        press.begin_level_write([Some(80), None]);
        assert_eq!(
            press.notify(&b1(0, 0, 10)),
            vec![Gesture::Press { side: Side::A }]
        );
    }

    #[test]
    fn serial_one_echo_sets_the_baseline_without_a_gesture() {
        let mut session = GestureSession::coyote3();
        arm(&mut session, 40, 10);
        session.begin_level_write([Some(80), None]);
        session.finish_level_write(true);
        assert_eq!(session.baseline(), [Some(80), Some(10)]);

        // 50 is not the knob we wrote.
        assert!(session.notify(&b1(1, 50, 10)).is_empty());
        assert_eq!(session.baseline(), [Some(80), Some(10)]);

        assert!(session.notify(&b1(1, 80, 10)).is_empty());
        assert_eq!(session.baseline(), [Some(80), Some(10)]);
        assert_eq!(
            session.notify(&b1(0, 81, 10)),
            vec![Gesture::Flick {
                side: Side::A,
                steps: 1
            }]
        );
        assert_eq!(session.baseline(), [Some(81), Some(10)]);

        // The echo of the write we already passed must not undo the flick.
        assert!(session.notify(&b1(1, 80, 10)).is_empty());
        assert_eq!(session.baseline(), [Some(81), Some(10)]);
    }

    #[test]
    fn late_serial0_after_pause_is_one_step_and_baseline_stays_zero() {
        let mut session = GestureSession::coyote3();
        arm(&mut session, 40, 30);
        session.output_written(0, 0);
        assert_eq!(session.baseline(), [Some(0), Some(0)]);

        assert_eq!(
            session.notify(&b1(0, 41, 30)),
            vec![Gesture::Flick {
                side: Side::A,
                steps: 1
            }]
        );
        assert_eq!(session.baseline(), [Some(0), Some(0)]);

        assert!(session.notify(&b1(1, 0, 0)).is_empty());
        assert_eq!(session.baseline(), [Some(0), Some(0)]);
        assert_eq!(
            session.notify(&b1(0, 1, 0)),
            vec![Gesture::Flick {
                side: Side::A,
                steps: 1
            }]
        );
        assert_eq!(session.baseline(), [Some(1), Some(0)]);
    }

    #[test]
    fn late_serial0_after_a_large_write_is_one_step_from_the_old_level() {
        let mut session = GestureSession::coyote3();
        arm(&mut session, 40, 10);
        session.begin_level_write([Some(90), None]);
        session.finish_level_write(true);
        assert_eq!(session.baseline(), [Some(90), Some(10)]);

        assert_eq!(
            session.notify(&b1(0, 41, 10)),
            vec![Gesture::Flick {
                side: Side::A,
                steps: 1
            }]
        );
        assert_eq!(session.baseline(), [Some(90), Some(10)]);

        // An older echo must not pull the baseline back.
        session.begin_level_write([Some(100), None]);
        session.finish_level_write(true);
        assert!(session.notify(&b1(1, 90, 10)).is_empty());
        assert_eq!(session.baseline(), [Some(100), Some(10)]);
        assert!(session.notify(&b1(1, 100, 10)).is_empty());
        assert_eq!(session.baseline(), [Some(100), Some(10)]);
    }

    #[test]
    fn value_above_200_ignores_that_side_only() {
        let mut session = GestureSession::coyote3();
        arm(&mut session, 10, 10);
        assert_eq!(
            session.notify(&b1(0, 201, 12)),
            vec![Gesture::Flick { side: Side::B, steps: 2 }]
        );
        assert_eq!(session.baseline(), [Some(10), Some(12)]);
    }

    #[test]
    fn disarmed_notify_does_not_move_baseline() {
        let mut session = GestureSession::coyote3();
        assert!(!session.is_armed());
        assert!(session.notify(&b1(0, 11, 11)).is_empty());
        assert_eq!(session.baseline(), [None, None]);

        session.soft_limit_written();
        assert!(!session.is_armed());
        assert!(session.notify(&b1(0, 11, 11)).is_empty());
        assert_eq!(session.baseline(), [None, None]);

        session.output_written(10, 20);
        assert!(session.is_armed());
        assert_eq!(session.baseline(), [Some(10), Some(20)]);
    }

    #[test]
    fn output_before_soft_limit_stays_disarmed_until_both_succeed() {
        let mut session = GestureSession::coyote3();
        session.output_written(10, 10);
        assert!(!session.is_armed());
        assert!(session.notify(&b1(0, 11, 10)).is_empty());
        assert_eq!(session.baseline(), [Some(10), Some(10)]);

        session.soft_limit_written();
        assert!(session.is_armed());
        assert_eq!(
            session.notify(&b1(0, 11, 10)),
            vec![Gesture::Flick { side: Side::A, steps: 1 }]
        );
    }

    #[test]
    fn coyote2_never_arms() {
        let mut session = GestureSession::coyote2();
        session.soft_limit_written();
        session.output_written(10, 10);
        assert!(!session.is_armed());
        assert!(session.notify(&b1(0, 50, 50)).is_empty());
        assert_eq!(session.baseline(), [None, None]);
    }

    #[test]
    fn disconnect_clears_baseline_and_requires_both_writes_again() {
        let mut session = GestureSession::coyote3();
        arm(&mut session, 10, 10);
        session.disconnect();
        assert!(!session.is_armed());
        assert_eq!(session.baseline(), [None, None]);
        assert!(session.notify(&b1(0, 12, 10)).is_empty());

        session.soft_limit_written();
        assert!(session.notify(&b1(0, 12, 10)).is_empty());
        session.output_written(10, 10);
        assert_eq!(
            session.notify(&b1(0, 12, 10)),
            vec![Gesture::Flick { side: Side::A, steps: 2 }]
        );
    }

    #[test]
    fn classifies_steps_press_and_zero_baseline() {
        let mut up = GestureSession::coyote3();
        arm(&mut up, 10, 20);
        assert_eq!(
            up.notify(&b1(0, 11, 20)),
            vec![Gesture::Flick { side: Side::A, steps: 1 }]
        );

        let mut down = GestureSession::coyote3();
        arm(&mut down, 10, 20);
        assert_eq!(
            down.notify(&b1(0, 10, 19)),
            vec![Gesture::Flick { side: Side::B, steps: -1 }]
        );

        let mut several = GestureSession::coyote3();
        arm(&mut several, 10, 20);
        assert_eq!(
            several.notify(&b1(0, 15, 17)),
            vec![
                Gesture::Flick { side: Side::A, steps: 5 },
                Gesture::Flick { side: Side::B, steps: -3 },
            ]
        );

        let mut press = GestureSession::coyote3();
        arm(&mut press, 40, 2);
        assert_eq!(
            press.notify(&b1(0, 0, 0)),
            vec![Gesture::Press { side: Side::A }, Gesture::Press { side: Side::B }]
        );

        let mut one_step = GestureSession::coyote3();
        arm(&mut one_step, 1, 1);
        assert_eq!(
            one_step.notify(&b1(0, 0, 1)),
            vec![Gesture::Flick { side: Side::A, steps: -1 }]
        );

        let mut already_zero = GestureSession::coyote3();
        arm(&mut already_zero, 0, 0);
        assert!(already_zero.notify(&b1(0, 0, 0)).is_empty());
        assert_eq!(already_zero.baseline(), [Some(0), Some(0)]);
    }

    #[test]
    fn switch_moves_the_held_knob_up_down_and_press() {
        // The powerbox holds the ceiling. A flick reports the new absolute
        // knob. A short press reports 0.
        let mut session = GestureSession::coyote3();
        arm(&mut session, 90, 67);

        assert_eq!(
            session.notify(&b1(0, 91, 67)),
            vec![Gesture::Flick {
                side: Side::A,
                steps: 1
            }]
        );
        assert_eq!(
            session.notify(&b1(0, 90, 67)),
            vec![Gesture::Flick {
                side: Side::A,
                steps: -1
            }]
        );
        assert_eq!(
            session.notify(&b1(0, 0, 67)),
            vec![Gesture::Press { side: Side::A }]
        );
        assert!(session.notify(&b1(0, 0, 67)).is_empty());
        assert_eq!(session.baseline(), [Some(0), Some(67)]);
    }

    #[test]
    fn in_flight_absolute_write_is_not_a_flick() {
        let mut session = GestureSession::coyote3();
        arm(&mut session, 80, 67);
        session.begin_level_write([Some(90), None]);
        assert!(session.notify(&b1(0, 90, 67)).is_empty());
        assert_eq!(session.baseline(), [Some(80), Some(67)]);

        session.finish_level_write(true);
        assert_eq!(session.baseline(), [Some(90), Some(67)]);
        assert_eq!(
            session.notify(&b1(0, 89, 67)),
            vec![Gesture::Flick {
                side: Side::A,
                steps: -1
            }]
        );
    }

    #[test]
    fn downward_flick_and_press_still_count_when_they_leave_the_commanded_level() {
        let mut down = GestureSession::coyote3();
        arm(&mut down, 20, 20);
        assert_eq!(
            down.notify(&b1(0, 19, 20)),
            vec![Gesture::Flick {
                side: Side::A,
                steps: -1
            }]
        );
        assert_eq!(
            down.notify(&b1(0, 20, 20)),
            vec![Gesture::Flick {
                side: Side::A,
                steps: 1
            }]
        );
        assert_eq!(down.baseline(), [Some(20), Some(20)]);

        let mut press = GestureSession::coyote3();
        arm(&mut press, 40, 8);
        assert_eq!(
            press.notify(&b1(0, 0, 8)),
            vec![Gesture::Press { side: Side::A }]
        );
        assert!(press.notify(&b1(0, 0, 8)).is_empty());
        assert_eq!(press.baseline(), [Some(0), Some(8)]);
    }

    #[test]
    fn duplicate_and_echo_do_nothing() {
        let mut session = GestureSession::coyote3();
        arm(&mut session, 15, 15);
        assert_eq!(
            session.notify(&b1(0, 16, 15)),
            vec![Gesture::Flick { side: Side::A, steps: 1 }]
        );
        assert!(session.notify(&b1(0, 16, 15)).is_empty());
        assert_eq!(session.baseline(), [Some(16), Some(15)]);

        session.output_written(15, 15);
        assert!(session.notify(&b1(0, 15, 15)).is_empty());
        assert_eq!(session.baseline(), [Some(15), Some(15)]);
    }

    #[test]
    fn malformed_packet_between_flicks_keeps_baseline() {
        let mut session = GestureSession::coyote3();
        arm(&mut session, 10, 10);
        assert_eq!(
            session.notify(&b1(0, 11, 10)),
            vec![Gesture::Flick { side: Side::A, steps: 1 }]
        );
        assert!(session.notify(&[0xB1, 0]).is_empty());
        assert_eq!(session.baseline()[0], Some(11));
        assert_eq!(
            session.notify(&b1(0, 12, 10)),
            vec![Gesture::Flick { side: Side::A, steps: 1 }]
        );
    }

    #[test]
    fn output_write_clears_the_slate_so_the_same_level_counts_again() {
        let mut session = GestureSession::coyote3();
        arm(&mut session, 15, 15);
        assert_eq!(
            session.notify(&b1(0, 16, 15)),
            vec![Gesture::Flick { side: Side::A, steps: 1 }]
        );
        // The write returns the knob to 15. A repeat of the level we already
        // counted is not another step. One step past that old level still is,
        // and the baseline stays on the value we wrote.
        session.output_written(15, 15);
        assert!(session.notify(&b1(0, 16, 15)).is_empty());
        assert_eq!(session.baseline(), [Some(15), Some(15)]);
        assert_eq!(
            session.notify(&b1(0, 17, 15)),
            vec![Gesture::Flick { side: Side::A, steps: 1 }]
        );
        assert_eq!(session.baseline(), [Some(15), Some(15)]);
    }

    #[test]
    fn paused_output_still_yields_a_gesture() {
        let mut session = GestureSession::coyote3();
        arm(&mut session, 15, 8);
        // Output paused: no further output_written call. The notify still classifies.
        assert_eq!(
            session.notify(&b1(0, 15, 9)),
            vec![Gesture::Flick { side: Side::B, steps: 1 }]
        );
        assert_eq!(session.baseline(), [Some(15), Some(9)]);
    }

    #[test]
    fn pause_zero_write_resets_baseline_before_the_next_flick() {
        let mut session = GestureSession::coyote3();
        arm(&mut session, 40, 30);
        // Successful pause zero-write: the powerbox is at 0,0.
        session.output_written(0, 0);
        assert_eq!(session.baseline(), [Some(0), Some(0)]);
        assert_eq!(
            session.notify(&b1(0, 1, 0)),
            vec![Gesture::Flick {
                side: Side::A,
                steps: 1
            }]
        );
        assert_eq!(session.baseline(), [Some(1), Some(0)]);
    }

    #[test]
    fn linked_flick_moves_only_the_ceiling() {
        let source = linked(10.0, 20.0);
        let next = apply_intensity_gesture(
            &source,
            Gesture::Flick { side: Side::A, steps: 1 },
            200,
        );
        assert_eq!(next.range_max, 21.0);
        assert_linked_untouched(&source, &next);
    }

    #[test]
    fn linked_press_sets_ceiling_to_floor_including_when_floor_is_above_zero() {
        let source = linked(10.0, 20.0);
        let next = apply_intensity_gesture(&source, Gesture::Press { side: Side::A }, 200);
        assert_eq!(next.range_max, 10.0);
        assert_eq!(next.range_min, 10.0);
        assert_linked_untouched(&source, &next);

        let already = linked(10.0, 10.0);
        let unchanged = apply_intensity_gesture(&already, Gesture::Press { side: Side::B }, 200);
        assert_eq!(unchanged.range_max, 10.0);
        assert_linked_untouched(&already, &unchanged);
    }

    #[test]
    fn fixed_flick_and_press_move_only_the_number() {
        let source = fixed(12.0);
        let up = apply_intensity_gesture(
            &source,
            Gesture::Flick { side: Side::B, steps: 1 },
            200,
        );
        assert_eq!(up.static_value, Some(13.0));
        assert_fixed_untouched(&source, &up);

        let stopped = apply_intensity_gesture(&source, Gesture::Press { side: Side::B }, 200);
        assert_eq!(stopped.static_value, Some(0.0));
        assert_fixed_untouched(&source, &stopped);

        let zero = fixed(0.0);
        let still = apply_intensity_gesture(&zero, Gesture::Press { side: Side::A }, 200);
        assert_eq!(still.static_value, Some(0.0));
        assert_fixed_untouched(&zero, &still);
    }

    #[test]
    fn clamps_to_floor_and_soft_cap_and_fifty_steps_stop_on_the_cap() {
        let source = linked(10.0, 20.0);
        let capped = apply_intensity_gesture(
            &source,
            Gesture::Flick { side: Side::A, steps: 100 },
            25,
        );
        assert_eq!(capped.range_max, 25.0);

        let floored = apply_intensity_gesture(
            &source,
            Gesture::Flick { side: Side::A, steps: -100 },
            200,
        );
        assert_eq!(floored.range_max, 10.0);
        assert_eq!(floored.range_min, 10.0);

        let mut stepped = linked(10.0, 10.0);
        for _ in 0..50 {
            stepped = apply_intensity_gesture(
                &stepped,
                Gesture::Flick { side: Side::A, steps: 1 },
                30,
            );
        }
        assert_eq!(stepped.range_max, 30.0);

        let mut fixed_steps = fixed(0.0);
        for _ in 0..50 {
            fixed_steps = apply_intensity_gesture(
                &fixed_steps,
                Gesture::Flick { side: Side::B, steps: 1 },
                30,
            );
        }
        assert_eq!(fixed_steps.static_value, Some(30.0));
        assert_eq!(fixed_steps.range_max, 80.0);
    }

    #[test]
    fn cap_below_floor_leaves_the_ceiling_unchanged() {
        let source = linked(10.0, 20.0);
        for gesture in [
            Gesture::Flick { side: Side::A, steps: 1 },
            Gesture::Flick { side: Side::A, steps: -1 },
            Gesture::Press { side: Side::A },
        ] {
            let next = apply_intensity_gesture(&source, gesture, 5);
            assert_eq!(next.range_max, 20.0);
            assert_eq!(next.range_min, 10.0);
            assert_linked_untouched(&source, &next);
        }
    }

    #[test]
    fn gesture_touches_only_that_channel_and_not_balance() {
        let mut channel_a = ChannelConfig::channel_a_default();
        channel_a.intensity = linked(10.0, 20.0);
        channel_a.intensity_balance.range_max = 255.0;
        let mut channel_b = ChannelConfig::channel_b_default();
        channel_b.intensity = fixed(8.0);

        let (next_a, next_b) = apply_gesture_to_pair(
            &channel_a,
            &channel_b,
            Gesture::Flick { side: Side::A, steps: 1 },
            200,
            200,
        );
        assert_eq!(next_a.intensity.range_max, 21.0);
        assert_eq!(next_a.frequency.static_value, channel_a.frequency.static_value);
        assert_eq!(
            next_a.frequency_balance.static_value,
            channel_a.frequency_balance.static_value
        );
        assert_eq!(
            next_a.intensity_balance.range_max,
            channel_a.intensity_balance.range_max
        );
        assert_eq!(next_a.intensity.range_min, 10.0);
        assert_eq!(next_b.intensity.static_value, channel_b.intensity.static_value);
        assert_eq!(next_b.intensity.range_max, channel_b.intensity.range_max);
        assert_eq!(next_b.frequency.static_value, channel_b.frequency.static_value);

        let (still_a, pressed_b) = apply_gesture_to_pair(
            &channel_a,
            &channel_b,
            Gesture::Press { side: Side::B },
            200,
            200,
        );
        assert_eq!(still_a.intensity.range_max, channel_a.intensity.range_max);
        assert_eq!(pressed_b.intensity.static_value, Some(0.0));
        assert_eq!(pressed_b.frequency_balance.static_value, channel_b.frequency_balance.static_value);
        assert_eq!(pressed_b.intensity_balance.static_value, channel_b.intensity_balance.static_value);
    }

    #[test]
    fn applying_the_same_patch_twice_is_identical() {
        let source = linked(10.0, 20.0);
        let changed = apply_intensity_gesture(
            &source,
            Gesture::Flick { side: Side::A, steps: 3 },
            200,
        );
        let patch = intensity_patch(&source, &changed);
        assert_eq!(patch.ceiling, Some(23.0));
        assert_eq!(patch.static_value, None);

        let once = apply_intensity_patch(&source, &patch);
        let twice = apply_intensity_patch(&once, &patch);
        assert_eq!(once.range_max, twice.range_max);
        assert_eq!(once.range_min, twice.range_min);
        assert_eq!(once.static_value, twice.static_value);
        assert_eq!(once.source_axis, twice.source_axis);
        assert_eq!(once.curve, twice.curve);
        assert_eq!(once.delay_ms, twice.delay_ms);
    }

    #[test]
    fn flick_wins_the_ceiling_and_leaves_the_slider_s_other_fields() {
        let committed = linked(10.0, 20.0);
        let slider = SliderEdit {
            ceiling: Some(50.0),
            floor: Some(12.0),
            static_value: Some(99.0),
            axis: Some("R2".to_string()),
            curve: Some(CurveType::Linear),
            delay_ms: Some(Some(80)),
        };
        let merged = apply_gesture_over_slider(
            &committed,
            &slider,
            Gesture::Flick { side: Side::A, steps: 1 },
            200,
        );
        assert_eq!(merged.range_max, 21.0);
        assert_eq!(merged.range_min, 12.0);
        assert_eq!(merged.static_value, Some(99.0));
        assert_eq!(merged.source_axis.as_deref(), Some("R2"));
        assert_eq!(merged.curve, CurveType::Linear);
        assert_eq!(merged.delay_ms, Some(80));
    }
}
