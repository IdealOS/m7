//! Transitions: values the compositor moves smoothly to where they're set, instead
//! of jumping. Apps never animate anything themselves: they set the end value, and
//! the compositor draws the frames in between, so motion never waits on an app.
//!
//! What moves (this phase, without window textures): positions (`x`, `y`) and
//! colours, both props and the colours a node's style gives it. Colours move in
//! Oklab, so a fade looks even, alpha included. Size, scale and whole-window
//! opacity wait for window textures.
//!
//! A track is one node's channel moving from one value to another over a duration
//! with an easing. Setting a new target while a track runs starts the new one from
//! where the value is now, so nothing jumps. Time comes from [`Animator::tick`]:
//! the frame loop passes when the frame will reach the screen. With reduce motion
//! on, nothing animates: every change takes effect at once.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use protocol::NodeId;
use serde_json::Value;

use crate::scene::Rgba;

/// How long the colours of a widget changing state (hovered, pressed, focused)
/// take to fade.
pub const STATE_FADE: Duration = Duration::from_millis(120);
/// How long switching between the light and dark themes takes to fade.
pub const THEME_FADE: Duration = Duration::from_millis(250);

/// What's moving on a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    /// One of [`PROPS`].
    Prop(&'static str),
    /// A colour from the node's style.
    Style(StyleColor),
    /// A dialog's backdrop fading in, from 0 to 1.
    Appear,
}

/// The props a node's `transition` can name.
pub const PROPS: [&str; 5] = ["x", "y", "color", "text_color", "border_color"];

/// A colour a style gives a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StyleColor {
    Color,
    Background,
    Border,
    Accent,
}

/// A value a track moves between.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Val {
    Num(f32),
    Color(Rgba),
}

/// How a track speeds up and slows down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ease {
    Linear,
    In,
    Out,
    InOut,
}

impl Ease {
    pub fn parse(s: &str) -> Option<Ease> {
        Some(match s {
            "linear" => Ease::Linear,
            "ease-in" | "in" => Ease::In,
            "ease-out" | "out" | "ease" => Ease::Out,
            "ease-in-out" | "in-out" => Ease::InOut,
            _ => return None,
        })
    }

    /// Progress at time `t` (0 to 1), as a cubic.
    pub fn at(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Ease::Linear => t,
            Ease::In => t * t * t,
            Ease::Out => 1.0 - (1.0 - t).powi(3),
            Ease::InOut => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
                }
            }
        }
    }
}

/// A transition a node asks for on one prop: how long, and how.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spec {
    pub duration: Duration,
    pub ease: Ease,
}

/// Longest transition a node may ask for.
pub const MAX_DURATION: Duration = Duration::from_secs(5);

/// What node props `p` ask for on `prop`: its `transition` prop names it with a
/// duration in milliseconds, or `[ms, "ease-in-out"]`. The default easing is
/// ease-out.
pub fn spec(p: &protocol::Props, prop: &str) -> Option<Spec> {
    let entry = p.get("transition")?.get(prop)?;
    let (ms, ease) = match entry {
        Value::Number(n) => (n.as_f64()?, Ease::Out),
        Value::Array(a) => {
            let ease = match a.get(1) {
                Some(e) => Ease::parse(e.as_str()?)?,
                None => Ease::Out,
            };
            (a.first()?.as_f64()?, ease)
        }
        _ => return None,
    };
    if !ms.is_finite() || ms <= 0.0 {
        return None;
    }
    let duration = Duration::from_secs_f64(ms / 1000.0).min(MAX_DURATION);
    Some(Spec { duration, ease })
}

#[derive(Debug, Clone)]
struct Track {
    from: Val,
    to: Val,
    start: Instant,
    duration: Duration,
    ease: Ease,
}

impl Track {
    fn at(&self, now: Instant) -> Val {
        let t =
            now.saturating_duration_since(self.start).as_secs_f32() / self.duration.as_secs_f32().max(1e-6);
        let k = self.ease.at(t);
        match (self.from, self.to) {
            (Val::Num(a), Val::Num(b)) => Val::Num(a + (b - a) * k),
            (Val::Color(a), Val::Color(b)) => Val::Color(mix(a, b, k)),
            (_, to) => to,
        }
    }
}

/// Every running track.
#[derive(Debug, Default)]
pub struct Animator {
    tracks: HashMap<(NodeId, Channel), Track>,
    /// How many tracks each node has, to skip nodes with none quickly.
    nodes: HashMap<NodeId, u32>,
    now: Option<Instant>,
    /// Make every change at once.
    pub reduce_motion: bool,
}

impl Animator {
    /// The time tracks are sampled at: the last [`Animator::tick`].
    pub fn now(&self) -> Instant {
        self.now.unwrap_or_else(Instant::now)
    }

    /// Move `node`'s `channel` from `from` to `to`. If it's already moving, it
    /// starts from where it is now instead.
    pub fn start(
        &mut self,
        node: NodeId,
        channel: Channel,
        from: Val,
        to: Val,
        duration: Duration,
        ease: Ease,
    ) {
        let from = self.value(node, channel).unwrap_or(from);
        if self.reduce_motion || duration.is_zero() || from == to {
            self.stop(node, channel);
            return;
        }
        let track = Track { from, to, start: self.now(), duration, ease };
        if self.tracks.insert((node, channel), track).is_none() {
            *self.nodes.entry(node).or_default() += 1;
        }
    }

    fn stop(&mut self, node: NodeId, channel: Channel) {
        if self.tracks.remove(&(node, channel)).is_some() {
            self.dec(node);
        }
    }

    fn dec(&mut self, node: NodeId) {
        if let Some(n) = self.nodes.get_mut(&node) {
            *n -= 1;
            if *n == 0 {
                self.nodes.remove(&node);
            }
        }
    }

    /// Where `node`'s `channel` is now, if it's moving.
    pub fn value(&self, node: NodeId, channel: Channel) -> Option<Val> {
        self.tracks.get(&(node, channel)).map(|t| t.at(self.now()))
    }

    /// Whether anything on `node` is moving.
    pub fn moving(&self, node: NodeId) -> bool {
        self.nodes.contains_key(&node)
    }

    /// Advance to `now`, dropping finished tracks. Returns whether any still run.
    pub fn tick(&mut self, now: Instant) -> bool {
        self.now = Some(now);
        let done: Vec<(NodeId, Channel)> =
            self.tracks.iter().filter(|(_, t)| now >= t.start + t.duration).map(|(k, _)| *k).collect();
        for (node, channel) in done {
            self.stop(node, channel);
        }
        !self.tracks.is_empty()
    }

    pub fn is_animating(&self) -> bool {
        !self.tracks.is_empty()
    }

    /// Forget `node`'s tracks (it was destroyed).
    pub fn forget(&mut self, node: NodeId) {
        if self.nodes.remove(&node).is_some() {
            self.tracks.retain(|(n, _), _| *n != node);
        }
    }

    /// Stop everything (reduce motion was turned on).
    pub fn clear(&mut self) {
        self.tracks.clear();
        self.nodes.clear();
    }
}

// --- Oklab ----------------------------------------------------------------------------

fn to_linear(c: u8) -> f32 {
    let v = c as f32 / 255.0;
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

fn from_linear(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let s = if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
    (s * 255.0).round().clamp(0.0, 255.0) as u8
}

/// sRGB to Oklab (Björn Ottosson's matrices).
fn oklab(c: Rgba) -> [f32; 3] {
    let [r, g, b] = [to_linear(c.0[0]), to_linear(c.0[1]), to_linear(c.0[2])];
    let l = (0.412_221_47 * r + 0.536_332_55 * g + 0.051_445_99 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

fn from_oklab([ll, a, b]: [f32; 3], alpha: u8) -> Rgba {
    let l = (ll + 0.396_337_78 * a + 0.215_803_76 * b).powi(3);
    let m = (ll - 0.105_561_346 * a - 0.063_854_17 * b).powi(3);
    let s = (ll - 0.089_484_18 * a - 1.291_485_5 * b).powi(3);
    Rgba([
        from_linear(4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s),
        from_linear(-1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s),
        from_linear(-0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s),
        alpha,
    ])
}

/// The colour `k` of the way from `a` to `b`, in Oklab, alpha straight. A fully
/// transparent end takes the other end's colour, so a fade in or out doesn't pass
/// through black.
pub fn mix(a: Rgba, b: Rgba, k: f32) -> Rgba {
    let a = if a.0[3] == 0 { Rgba([b.0[0], b.0[1], b.0[2], 0]) } else { a };
    let b = if b.0[3] == 0 { Rgba([a.0[0], a.0[1], a.0[2], 0]) } else { b };
    let (x, y) = (oklab(a), oklab(b));
    let lab = [x[0] + (y[0] - x[0]) * k, x[1] + (y[1] - x[1]) * k, x[2] + (y[2] - x[2]) * k];
    let alpha = (a.0[3] as f32 + (b.0[3] as f32 - a.0[3] as f32) * k).round() as u8;
    from_oklab(lab, alpha)
}

/// `c` as a colour prop's text.
pub fn hex(c: Rgba) -> String {
    format!("#{:02x}{:02x}{:02x}{:02x}", c.0[0], c.0[1], c.0[2], c.0[3])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hexc(s: &str) -> Rgba {
        Rgba::parse(s).unwrap()
    }

    #[test]
    fn easings_start_at_zero_and_end_at_one() {
        for e in [Ease::Linear, Ease::In, Ease::Out, Ease::InOut] {
            assert_eq!((e.at(0.0), e.at(1.0)), (0.0, 1.0), "{e:?}");
            assert!(e.at(0.5) > 0.0 && e.at(0.5) < 1.0);
        }
        assert!(Ease::Out.at(0.2) > Ease::In.at(0.2), "ease-out starts fast");
    }

    #[test]
    fn colours_mix_in_oklab_and_round_trip() {
        for c in ["#000000ff", "#ffffffff", "#3b82f6ff", "#d9480f80"] {
            let c = hexc(c);
            assert_eq!(mix(c, c, 0.5), c);
            assert_eq!(mix(c, hexc("#123456ff"), 0.0), c);
        }
        // Halfway from black to white in Oklab is a mid grey, lighter than sRGB's 50%.
        let mid = mix(hexc("#000000"), hexc("#ffffff"), 0.5);
        assert!(mid.0[0] == mid.0[1] && mid.0[1] == mid.0[2] && mid.0[0] > 90 && mid.0[0] < 120, "{mid:?}");
        // Fading in from transparent keeps the colour and moves only alpha.
        let fade = mix(Rgba::TRANSPARENT, hexc("#3b82f6"), 0.5);
        assert_eq!((fade.0[0], fade.0[1], fade.0[2]), (0x3b, 0x82, 0xf6));
        assert!((fade.0[3] as i32 - 128).abs() <= 1);
    }

    #[test]
    fn a_track_moves_then_ends_and_a_new_target_starts_from_where_it_is() {
        let t0 = Instant::now();
        let mut a = Animator::default();
        a.tick(t0);
        let ms = |n: u64| Duration::from_millis(n);
        a.start(1, Channel::Prop("x"), Val::Num(0.0), Val::Num(100.0), ms(100), Ease::Linear);
        assert!(a.moving(1) && a.is_animating());
        a.tick(t0 + ms(50));
        assert_eq!(a.value(1, Channel::Prop("x")), Some(Val::Num(50.0)));
        // Retarget halfway: the new track starts at 50, not 0.
        a.start(1, Channel::Prop("x"), Val::Num(0.0), Val::Num(0.0), ms(100), Ease::Linear);
        assert_eq!(a.value(1, Channel::Prop("x")), Some(Val::Num(50.0)));
        a.tick(t0 + ms(100));
        assert_eq!(a.value(1, Channel::Prop("x")), Some(Val::Num(25.0)));
        assert!(!a.tick(t0 + ms(150)), "finished");
        assert!(!a.moving(1) && a.value(1, Channel::Prop("x")).is_none());
        // Reduce motion: nothing starts.
        a.reduce_motion = true;
        a.start(2, Channel::Prop("y"), Val::Num(0.0), Val::Num(10.0), ms(100), Ease::Out);
        assert!(!a.is_animating());
    }

    #[test]
    fn a_nodes_transition_prop_names_durations_and_easings() {
        let p =
            json!({"transition": {"x": 200, "color": [150, "ease-in-out"], "y": [0], "text_color": "fast"}});
        let p = p.as_object().unwrap();
        assert_eq!(spec(p, "x"), Some(Spec { duration: Duration::from_millis(200), ease: Ease::Out }));
        assert_eq!(spec(p, "color").map(|s| s.ease), Some(Ease::InOut));
        assert_eq!(spec(p, "y"), None, "zero");
        assert_eq!(spec(p, "text_color"), None);
        assert_eq!(spec(p, "border_color"), None);
        let long = json!({"transition": {"x": 60_000}});
        assert_eq!(spec(long.as_object().unwrap(), "x").unwrap().duration, MAX_DURATION);
    }
}
