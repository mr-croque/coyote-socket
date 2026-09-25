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
    /// `None` when that side's byte is above 200 and must be ignored.
    sides: [Option<u8>; 2],
}

/// `Some` when the packet is a serial-0 B1. `None` drops the whole packet.
fn parse_b1(bytes: &[u8]) -> Option<ParsedB1> {
    if bytes.len() < 4 || bytes[0] != 0xB1 || bytes[1] != 0 {
        return None;
    }
    let side = |value: u8| if value <= 200 { Some(value) } else { None };
    Some(ParsedB1 {
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

    /// A successful B0 output write. Sets the baseline to the intensities just sent.
    pub fn output_written(&mut self, channel_a: u8, channel_b: u8) {
        if !self.enabled {
            return;
        }
        self.baseline = [Some(channel_a.min(200)), Some(channel_b.min(200))];
        self.output_ok = true;
        self.armed = self.soft_limit_ok && self.output_ok;
    }

    /// Disconnect clears the baseline and disarms. The next connection needs both writes again.
    pub fn disconnect(&mut self) {
        let enabled = self.enabled;
        *self = Self::new(enabled);
    }

    /// Classify one notify. A report that is not armed, malformed, or a non-zero serial
    /// leaves the baseline where it is.
    pub fn notify(&mut self, bytes: &[u8]) -> Vec<Gesture> {
        if !self.enabled || !self.armed {
            return Vec::new();
        }
        let Some(parsed) = parse_b1(bytes) else {
            return Vec::new();
        };

        let mut gestures = Vec::new();
        for (index, reported) in parsed.sides.into_iter().enumerate() {
            let Some(reported) = reported else {
                continue;
            };
            let Some(baseline) = self.baseline[index] else {
                continue;
            };
            if reported == baseline {
                continue;
            }
            let side = if index == 0 { Side::A } else { Side::B };
            let gesture = if reported == 0 && baseline >= 2 {
                Gesture::Press { side }
            } else {
                Gesture::Flick {
                    side,
                    steps: reported as i16 - baseline as i16,
                }
            };
            self.baseline[index] = Some(reported);
            gestures.push(gesture);
        }
        gestures
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

pub async fn push_notify(bytes: &[u8]) -> Vec<Gesture> {
    session_slot().lock().await.notify(bytes)
}

/// Write each gesture onto the live channel config, then tell the window the new fields.
///
/// Output pause does not pass through here: the device loop already skips sends while
/// paused, and this only stores the new ceiling or fixed intensity.
pub async fn commit_gestures(gestures: Vec<Gesture>) {
    if gestures.is_empty() {
        return;
    }

    let settings = crate::settings::get_settings().await;
    let cap_a = settings.general.channel_a_max_intensity.min(200);
    let cap_b = settings.general.channel_b_max_intensity.min(200);

    let state = crate::processing::get_processing_state().await;
    let guard = state.read().await;
    let mut pending = Vec::with_capacity(gestures.len());

    for gesture in gestures {
        let (channel_id, cap) = match gesture.side() {
            Side::A => (crate::processing::ChannelId::A, cap_a),
            Side::B => (crate::processing::ChannelId::B, cap_b),
        };
        let mut config = guard.channel(channel_id).config.clone();
        let before = config.intensity.clone();
        let after = apply_intensity_gesture(&before, gesture, cap);
        let patch = intensity_patch(&before, &after);
        config.intensity = after;
        pending.push((channel_id, config, patch));
    }
    drop(guard);

    let mut patches = Vec::with_capacity(pending.len());
    for (channel_id, config, patch) in pending {
        crate::install_channel_config(channel_id, config, None).await;
        patches.push((channel_id, patch));
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

        assert!(session.notify(&b1(1, 40, 40)).is_empty());
        assert_eq!(session.baseline(), [Some(11), Some(10)]);

        assert!(session.notify(&[0xB0, 0, 12, 10]).is_empty());
        assert_eq!(session.baseline(), [Some(11), Some(10)]);
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
        // The next output write puts the powerbox back to the level Coyote Socket sent.
        // A notify still in flight, or a second flick that lands on the same absolute
        // intensity, counts as another step. There is no time debounce.
        session.output_written(15, 15);
        assert_eq!(
            session.notify(&b1(0, 16, 15)),
            vec![Gesture::Flick { side: Side::A, steps: 1 }]
        );
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
