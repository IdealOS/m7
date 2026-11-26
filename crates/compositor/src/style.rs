//! Style sheets: how things look, CSS-like but simpler. Colours and fonts only;
//! layout stays in props.
//!
//! ```css
//! :root   { --accent: #3b82f6; color: var(--text); font-family: "Inter Variable", sans-serif; }
//! button  { background: var(--control); border-color: var(--control-border); }
//! button.primary, .primary:pressed { background: var(--accent); color: var(--accent-text); }
//! @dark   { :root { --text: #e5e7eb; } }
//! ```
//!
//! A selector is a widget kind (`button`), a class (`.primary`, from a node's
//! `class` prop, which may hold several separated by spaces), or both, each with at
//! most one state: `:hover`, `:pressed`, `:focus`, `:disabled` or `:checked`. A
//! rule with a state sets colours only, so a state never changes a widget's size.
//! `:root` holds variables and the starting colour and font. There are no
//! combinators, so a rule never depends on where a node sits. Selectors separated
//! by commas share a rule. `@dark { ... }` holds rules that apply only in the dark
//! theme.
//!
//! Properties: `color`, `background`, `border-color`, `accent-color`,
//! `caret-color`, `selection-color`, `placeholder-color`; `font-family` (a list,
//! best ending in `sans-serif`, `serif` or `monospace`), `font-size` (px),
//! `font-weight` (`normal`, `bold` or 100 to 900) and `font-style` (`normal` or
//! `italic`). Colours are `#rgb`, `#rrggbb`, `#rrggbbaa`, `transparent` or
//! `var(--name)`; variables, which hold colours, are set in `:root` only. `color`,
//! `accent-color` and the font properties inherit down the node tree, as in CSS.
//!
//! The cascade: the system sheet ([`SYSTEM`]), then the app's own sheet for nodes
//! in its windows. A rule's specificity is 1 for a kind and 10 for a class or a
//! state; the more specific rule wins, then the app's over the system's, then the
//! later one. A node's own colour props (`color`, `text_color`, ...) beat both.

use std::collections::HashMap;
use std::sync::Arc;

use protocol::NodeKind;
use serde_json::Value;

use crate::scene::Rgba;

/// The system's style sheet: the light theme, and the dark one in `@dark`.
pub const SYSTEM: &str = include_str!("system.css");

/// Longest sheet an app may send.
pub const MAX_SHEET_BYTES: usize = 64 * 1024;
/// Most rules in one sheet.
const MAX_RULES: usize = 2000;

/// A widget state a selector can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Hover,
    Pressed,
    Focus,
    Disabled,
    Checked,
}

#[derive(Debug, Clone, PartialEq)]
struct Selector {
    root: bool,
    kind: Option<NodeKind>,
    class: Option<String>,
    state: Option<State>,
}

impl Selector {
    fn specificity(&self) -> u32 {
        self.kind.map_or(0, |_| 1) + self.class.as_ref().map_or(0, |_| 10) + self.state.map_or(0, |_| 10)
    }

    fn matches(&self, node: &NodeInfo) -> bool {
        !self.root
            && self.kind.is_none_or(|k| k == node.kind)
            && self.class.as_ref().is_none_or(|c| node.classes.iter().any(|n| n == c))
            && self.state.is_none_or(|s| node.states.contains(&s))
    }
}

/// A colour as written: a value, or a variable to look up.
#[derive(Debug, Clone, PartialEq)]
pub enum ColorRef {
    Value(Rgba),
    Var(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prop {
    Color,
    Background,
    BorderColor,
    AccentColor,
    CaretColor,
    SelectionColor,
    PlaceholderColor,
    FontFamily,
    FontSize,
    FontWeight,
    FontStyle,
}

#[derive(Debug, Clone, PartialEq)]
enum Decl {
    Color(Prop, ColorRef),
    Families(Arc<[String]>),
    Size(f32),
    Weight(u16),
    Italic(bool),
}

#[derive(Debug, Clone, PartialEq)]
struct Rule {
    selectors: Vec<Selector>,
    decls: Vec<Decl>,
    /// `--name: value` declarations (in `:root` rules only).
    vars: Vec<(String, ColorRef)>,
    dark: bool,
}

/// A parsed style sheet.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sheet {
    rules: Vec<Rule>,
}

/// Why a sheet didn't parse: what, and where (1-based).
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub line: usize,
    pub col: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "line {}, column {}: {}", self.line, self.col, self.message)
    }
}

impl Sheet {
    pub fn parse(src: &str) -> Result<Sheet, ParseError> {
        if src.len() > MAX_SHEET_BYTES {
            return Err(ParseError { line: 1, col: 1, message: format!("over {MAX_SHEET_BYTES} bytes") });
        }
        Parser { src, pos: 0 }.sheet()
    }

    /// The built-in system sheet.
    pub fn system() -> Sheet {
        Sheet::parse(SYSTEM).expect("the system style sheet parses")
    }

    /// This sheet's rules, then `other`'s: `other` overrides this one where they
    /// match equally, as if it were written at the end.
    pub fn then(&self, other: &Sheet) -> Sheet {
        Sheet { rules: self.rules.iter().chain(&other.rules).cloned().collect() }
    }
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn error<T>(&self, message: impl Into<String>) -> Result<T, ParseError> {
        let before = &self.src[..self.pos];
        let line = before.matches('\n').count() + 1;
        let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        Err(ParseError { line, col, message: message.into() })
    }

    fn rest(&self) -> &str {
        &self.src[self.pos..]
    }

    /// Skip whitespace and comments.
    fn skip(&mut self) -> Result<(), ParseError> {
        loop {
            let trimmed = self.rest().trim_start();
            self.pos = self.src.len() - trimmed.len();
            if self.rest().starts_with("/*") {
                match self.rest()[2..].find("*/") {
                    Some(end) => self.pos += end + 4,
                    None => return self.error("a comment that never ends"),
                }
            } else {
                return Ok(());
            }
        }
    }

    fn eat(&mut self, c: char) -> Result<bool, ParseError> {
        self.skip()?;
        if self.rest().starts_with(c) {
            self.pos += c.len_utf8();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn expect(&mut self, c: char) -> Result<(), ParseError> {
        if self.eat(c)? { Ok(()) } else { self.error(format!("expected `{c}`")) }
    }

    /// A name: letters, digits, `-` and `_`.
    fn ident(&mut self) -> Result<&str, ParseError> {
        self.skip()?;
        let len = self
            .rest()
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(self.rest().len());
        if len == 0 {
            return self.error("expected a name");
        }
        let start = self.pos;
        self.pos += len;
        Ok(&self.src[start..self.pos])
    }

    fn sheet(mut self) -> Result<Sheet, ParseError> {
        let mut rules = Vec::new();
        loop {
            self.skip()?;
            if self.rest().is_empty() {
                break;
            }
            if self.rest().starts_with('@') {
                self.pos += 1;
                let at = self.ident()?.to_owned();
                if at != "dark" {
                    return self.error(format!("unknown at-rule @{at}: only @dark"));
                }
                self.expect('{')?;
                while !self.eat('}')? {
                    if self.rest().is_empty() {
                        return self.error("@dark never closes with `}`");
                    }
                    if self.rest().starts_with('@') {
                        return self.error("@dark can't hold another at-rule");
                    }
                    rules.push(self.rule(true)?);
                }
            } else {
                rules.push(self.rule(false)?);
            }
            if rules.len() > MAX_RULES {
                return self.error(format!("more than {MAX_RULES} rules"));
            }
        }
        Ok(Sheet { rules })
    }

    fn rule(&mut self, dark: bool) -> Result<Rule, ParseError> {
        let mut selectors = vec![self.selector()?];
        while self.eat(',')? {
            selectors.push(self.selector()?);
        }
        self.expect('{')?;
        let root = selectors.iter().all(|s| s.root);
        if !root && selectors.iter().any(|s| s.root) {
            return self.error(":root can't share a rule with other selectors");
        }
        let (mut decls, mut vars) = (Vec::new(), Vec::new());
        loop {
            if self.eat('}')? {
                break;
            }
            if self.rest().is_empty() {
                return self.error("a rule that never closes with `}`");
            }
            self.skip()?;
            if self.rest().starts_with("--") {
                self.pos += 2;
                let name = self.ident()?.to_owned();
                if !root {
                    return self.error(format!("--{name}: variables are set in :root only"));
                }
                self.expect(':')?;
                vars.push((name, self.color()?));
            } else {
                let at = self.pos;
                let name = self.ident()?.to_owned();
                self.expect(':')?;
                let decl = self.decl(&name, at)?;
                if !matches!(decl, Decl::Color(..)) && selectors.iter().any(|s| s.state.is_some()) {
                    self.pos = at;
                    return self.error(format!("{name}: a rule with a :state sets colours only"));
                }
                decls.push(decl);
            }
            if !self.eat(';')? {
                self.skip()?;
                if self.rest().is_empty() {
                    return self.error("a rule that never closes with `}`");
                }
                if !self.rest().starts_with('}') {
                    return self.error("expected `;` or `}`");
                }
            }
        }
        Ok(Rule { selectors, decls, vars, dark })
    }

    fn selector(&mut self) -> Result<Selector, ParseError> {
        self.skip()?;
        let mut s = Selector { root: false, kind: None, class: None, state: None };
        if self.rest().starts_with(":root") {
            self.pos += 5;
            s.root = true;
            return Ok(s);
        }
        if self.rest().starts_with(|c: char| c.is_ascii_alphabetic()) {
            let name = self.ident()?.to_owned();
            match serde_json::from_value::<NodeKind>(Value::String(name.clone())) {
                Ok(kind) => s.kind = Some(kind),
                Err(_) => return self.error(format!("no widget kind {name:?}")),
            }
        }
        if self.rest().starts_with('.') {
            self.pos += 1;
            s.class = Some(self.ident()?.to_owned());
        }
        if self.rest().starts_with(':') {
            self.pos += 1;
            let state = self.ident()?.to_owned();
            s.state = Some(match state.as_str() {
                "hover" => State::Hover,
                "pressed" => State::Pressed,
                "focus" => State::Focus,
                "disabled" => State::Disabled,
                "checked" => State::Checked,
                other => return self.error(format!("no state :{other}")),
            });
        }
        if s.kind.is_none() && s.class.is_none() {
            return self.error("expected a widget kind, a .class or :root");
        }
        // No combinators: after a selector comes only a comma or the rule's `{`.
        self.skip()?;
        if !self.rest().starts_with([',', '{']) {
            return self.error("selectors are a kind, a .class and a :state, with nothing between");
        }
        Ok(s)
    }

    /// The raw text of a value, up to `;` or `}`.
    fn value_text(&mut self) -> Result<&str, ParseError> {
        self.skip()?;
        let len = self.rest().find([';', '}']).unwrap_or(self.rest().len());
        let start = self.pos;
        self.pos += len;
        let text = self.src[start..self.pos].trim();
        if text.is_empty() {
            self.pos = start;
            return self.error("expected a value");
        }
        Ok(text)
    }

    fn color(&mut self) -> Result<ColorRef, ParseError> {
        let start = self.pos;
        let text = self.value_text()?.to_owned();
        match parse_color_ref(&text) {
            Some(c) => Ok(c),
            None => {
                self.pos = start;
                self.skip()?;
                self.error(format!(
                    "{text:?} isn't a colour: #rgb, #rrggbb, #rrggbbaa, transparent or var(--name)"
                ))
            }
        }
    }

    fn decl(&mut self, name: &str, at: usize) -> Result<Decl, ParseError> {
        let prop = match name {
            "color" => Prop::Color,
            "background" => Prop::Background,
            "border-color" => Prop::BorderColor,
            "accent-color" => Prop::AccentColor,
            "caret-color" => Prop::CaretColor,
            "selection-color" => Prop::SelectionColor,
            "placeholder-color" => Prop::PlaceholderColor,
            "font-family" => Prop::FontFamily,
            "font-size" => Prop::FontSize,
            "font-weight" => Prop::FontWeight,
            "font-style" => Prop::FontStyle,
            other => {
                self.pos = at;
                return self.error(format!("no property {other:?} (styles set colours and fonts only)"));
            }
        };
        if !matches!(prop, Prop::FontFamily | Prop::FontSize | Prop::FontWeight | Prop::FontStyle) {
            return Ok(Decl::Color(prop, self.color()?));
        }
        let start = self.pos;
        let text = self.value_text()?.to_owned();
        let bad = |p: &mut Self, why: &str| {
            p.pos = start;
            p.skip().and_then(|()| p.error(format!("{text:?}: {why}")))
        };
        match prop {
            Prop::FontFamily => {
                let families: Vec<String> = text
                    .split(',')
                    .map(|f| f.trim().trim_matches(|c| c == '"' || c == '\'').trim().to_owned())
                    .collect();
                if families.iter().any(String::is_empty) {
                    return bad(self, "a list of family names, separated by commas");
                }
                Ok(Decl::Families(families.into()))
            }
            Prop::FontSize => {
                let n = text.strip_suffix("px").unwrap_or(&text).trim().parse::<f32>().ok();
                match n {
                    Some(n) if n.is_finite() => Ok(Decl::Size(crate::theme::size::clamp(n))),
                    _ => bad(self, "a size in px"),
                }
            }
            Prop::FontWeight => match text.as_str() {
                "normal" => Ok(Decl::Weight(400)),
                "bold" => Ok(Decl::Weight(700)),
                n => match n.parse::<u16>() {
                    Ok(w) if (100..=900).contains(&w) => Ok(Decl::Weight(w)),
                    _ => bad(self, "normal, bold, or 100 to 900"),
                },
            },
            _ => match text.as_str() {
                "normal" => Ok(Decl::Italic(false)),
                "italic" | "oblique" => Ok(Decl::Italic(true)),
                _ => bad(self, "normal or italic"),
            },
        }
    }
}

/// `#rgb`, `#rrggbb`, `#rrggbbaa`, `transparent` or `var(--name)`.
fn parse_color_ref(text: &str) -> Option<ColorRef> {
    if text == "transparent" {
        return Some(ColorRef::Value(Rgba::TRANSPARENT));
    }
    if let Some(inner) = text.strip_prefix("var(").and_then(|t| t.strip_suffix(')')) {
        let name = inner.trim().strip_prefix("--")?;
        let ok = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        return ok.then(|| ColorRef::Var(name.to_owned()));
    }
    if let Some(hex) = text.strip_prefix('#')
        && hex.len() == 3
    {
        let long: String = hex.chars().flat_map(|c| [c, c]).collect();
        return Rgba::parse(&format!("#{long}")).map(ColorRef::Value);
    }
    Rgba::parse(text).map(ColorRef::Value)
}

// --- Computed styles ---------------------------------------------------------------

/// Variables, by name without the `--`.
pub type Vars = HashMap<String, Rgba>;

/// A font, as text is shaped with it.
#[derive(Debug, Clone, PartialEq)]
pub struct Font {
    /// Family names in order of preference, ending (usually) in a generic one.
    pub families: Arc<[String]>,
    pub size: f32,
    pub weight: u16,
    pub italic: bool,
}

impl Default for Font {
    fn default() -> Font {
        Font {
            families: Arc::from(vec!["sans-serif".to_owned()]),
            size: crate::theme::size::BODY,
            weight: 400,
            italic: false,
        }
    }
}

/// A node's computed style.
#[derive(Debug, Clone)]
pub struct Style {
    /// The variables in force: the system's, and the app's inside its windows.
    pub vars: Arc<Vars>,
    pub color: Rgba,
    pub accent: Rgba,
    pub font: Font,
    pub background: Option<Rgba>,
    pub border: Option<Rgba>,
    pub caret: Option<Rgba>,
    pub selection: Option<Rgba>,
    pub placeholder: Option<Rgba>,
}

impl Style {
    /// A variable's colour, or magenta if the sheet forgot it (so it shows).
    pub fn var(&self, name: &str) -> Rgba {
        self.vars.get(name).copied().unwrap_or(Rgba([255, 0, 255, 255]))
    }

    /// A colour prop's value: `#rrggbb`, `#rrggbbaa`, `$name` or `var(--name)`.
    pub fn resolve(&self, text: &str) -> Option<Rgba> {
        if let Some(name) = text.strip_prefix('$') {
            return self.vars.get(name).copied();
        }
        match parse_color_ref(text)? {
            ColorRef::Value(c) => Some(c),
            ColorRef::Var(name) => self.vars.get(&name).copied(),
        }
    }
}

/// What the cascade needs to know about a node.
pub struct NodeInfo<'a> {
    pub kind: NodeKind,
    pub classes: Vec<&'a str>,
    pub states: Vec<State>,
}

impl<'a> NodeInfo<'a> {
    /// From a node's kind and props: its `class` prop, and the states the
    /// compositor keeps in props (`_hover`, `pressed`, `_focus`, `disabled`, `checked`).
    pub fn of(kind: NodeKind, props: &'a protocol::Props) -> NodeInfo<'a> {
        let flag = |k: &str| props.get(k).and_then(Value::as_bool) == Some(true);
        let classes = props
            .get("class")
            .and_then(Value::as_str)
            .map(|c| c.split_whitespace().collect())
            .unwrap_or_default();
        let mut states = Vec::new();
        // `_active` is a window title bar's while its window is the active one:
        // `:focus` too, but without the focus ring a widget with `_focus` gets.
        for (key, state) in [
            ("_hover", State::Hover),
            ("pressed", State::Pressed),
            ("_focus", State::Focus),
            ("_active", State::Focus),
            ("disabled", State::Disabled),
            ("checked", State::Checked),
        ] {
            if flag(key) {
                states.push(state);
            }
        }
        NodeInfo { kind, classes, states }
    }
}

/// The sheets in force for part of the scene, and their variables.
#[derive(Debug, Clone)]
pub struct Scope {
    /// The system sheet, then the app's.
    sheets: Vec<Arc<Sheet>>,
    dark: bool,
    pub vars: Arc<Vars>,
}

impl Scope {
    /// The system sheet alone, light or dark.
    pub fn system(sheet: Arc<Sheet>, dark: bool) -> Scope {
        let vars = Arc::new(collect_vars(std::slice::from_ref(&sheet), dark));
        Scope { sheets: vec![sheet], dark, vars }
    }

    /// This scope with an app's sheet added, for the inside of its windows.
    pub fn with_app(&self, app: Arc<Sheet>) -> Scope {
        let mut sheets = self.sheets.clone();
        sheets.push(app);
        let vars = Arc::new(collect_vars(&sheets, self.dark));
        Scope { sheets, dark: self.dark, vars }
    }

    fn rules(&self) -> impl Iterator<Item = (usize, usize, &Rule)> {
        self.sheets.iter().enumerate().flat_map(move |(origin, s)| {
            s.rules
                .iter()
                .enumerate()
                .filter(move |(_, r)| !r.dark || self.dark)
                .map(move |(i, r)| (origin, i, r))
        })
    }

    /// The style at the top of this scope: inherited from `parent` (if any), with
    /// the `:root` rules' colour and font applied.
    pub fn root(&self, parent: Option<&Style>) -> Style {
        let mut style = match parent {
            Some(p) => Style { vars: self.vars.clone(), ..inherit(p) },
            None => Style {
                vars: self.vars.clone(),
                color: Rgba::BLACK,
                accent: self.vars.get("accent").copied().unwrap_or(Rgba::BLACK),
                font: Font::default(),
                background: None,
                border: None,
                caret: None,
                selection: None,
                placeholder: None,
            },
        };
        // Every sheet's :root, with this scope's variables: at the top of an app's
        // window the system's `accent-color: var(--accent)` picks up the app's own
        // --accent, and then the app's :root rules apply.
        for (_, _, rule) in self.rules() {
            if rule.selectors.iter().all(|s| s.root) {
                apply(&mut style, &rule.decls, &self.vars);
            }
        }
        style
    }

    /// The style of a node below `parent` in this scope.
    pub fn compute(&self, parent: &Style, node: &NodeInfo) -> Style {
        let mut matched: Vec<(u32, usize, usize, &Rule)> = Vec::new();
        for (origin, order, rule) in self.rules() {
            if let Some(spec) =
                rule.selectors.iter().filter(|s| s.matches(node)).map(Selector::specificity).max()
            {
                matched.push((spec, origin, order, rule));
            }
        }
        matched.sort_by_key(|&(spec, origin, order, _)| (spec, origin, order));
        let mut style = inherit(parent);
        for (_, _, _, rule) in matched {
            apply(&mut style, &rule.decls, &self.vars);
        }
        style
    }
}

/// A child's starting style: the inherited properties, nothing else.
fn inherit(parent: &Style) -> Style {
    Style {
        vars: parent.vars.clone(),
        color: parent.color,
        accent: parent.accent,
        font: parent.font.clone(),
        background: None,
        border: None,
        caret: None,
        selection: None,
        placeholder: None,
    }
}

fn apply(style: &mut Style, decls: &[Decl], vars: &Vars) {
    for d in decls {
        match d {
            Decl::Color(prop, c) => {
                let Some(c) = resolve(c, vars) else { continue };
                match prop {
                    Prop::Color => style.color = c,
                    Prop::Background => style.background = Some(c),
                    Prop::BorderColor => style.border = Some(c),
                    Prop::AccentColor => style.accent = c,
                    Prop::CaretColor => style.caret = Some(c),
                    Prop::SelectionColor => style.selection = Some(c),
                    Prop::PlaceholderColor => style.placeholder = Some(c),
                    _ => {}
                }
            }
            Decl::Families(f) => style.font.families = f.clone(),
            Decl::Size(s) => style.font.size = *s,
            Decl::Weight(w) => style.font.weight = *w,
            Decl::Italic(i) => style.font.italic = *i,
        }
    }
}

fn resolve(c: &ColorRef, vars: &Vars) -> Option<Rgba> {
    match c {
        ColorRef::Value(v) => Some(*v),
        ColorRef::Var(name) => vars.get(name).copied(),
    }
}

/// Every variable the sheets set, later sheets (and `@dark`, when dark) winning,
/// with variables that name other variables followed (up to a few steps).
fn collect_vars(sheets: &[Arc<Sheet>], dark: bool) -> Vars {
    let mut raw: HashMap<String, ColorRef> = HashMap::new();
    for sheet in sheets {
        for pass_dark in [false, true] {
            if pass_dark && !dark {
                continue;
            }
            for rule in sheet.rules.iter().filter(|r| r.dark == pass_dark) {
                for (name, value) in &rule.vars {
                    raw.insert(name.clone(), value.clone());
                }
            }
        }
    }
    let mut out = Vars::new();
    for name in raw.keys() {
        let mut at = name;
        for _ in 0..8 {
            match &raw[at] {
                ColorRef::Value(v) => {
                    out.insert(name.clone(), *v);
                    break;
                }
                ColorRef::Var(next) if raw.contains_key(next) => at = next,
                ColorRef::Var(_) => break,
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn props(v: Value) -> protocol::Props {
        v.as_object().unwrap().clone()
    }

    fn style_of(scope: &Scope, kind: NodeKind, p: Value) -> Style {
        let p = props(p);
        scope.compute(&scope.root(None), &NodeInfo::of(kind, &p))
    }

    fn hex(s: &str) -> Rgba {
        Rgba::parse(s).unwrap()
    }

    #[test]
    fn the_system_sheet_parses_and_sets_every_variable_in_both_themes() {
        let sheet = Arc::new(Sheet::system());
        let light = Scope::system(sheet.clone(), false);
        let dark = Scope::system(sheet, true);
        for name in
            ["desktop", "surface", "text", "muted", "accent", "accent-text", "control", "focus", "selection"]
        {
            assert!(light.vars.contains_key(name) && dark.vars.contains_key(name), "--{name}");
        }
        assert_ne!(light.vars["text"], dark.vars["text"]);
        assert_eq!(light.root(None).color, light.vars["text"]);
    }

    #[test]
    fn specificity_then_origin_then_order_decide() {
        let system = Arc::new(
            Sheet::parse(
                ":root { --a: #111111; --b: #222222 }
                 button { background: var(--a) }
                 button:pressed { background: #333333 }
                 .big { color: #444444 }
                 label { color: #555555 } label { color: #666666 }",
            )
            .unwrap(),
        );
        let s = Scope::system(system.clone(), false);
        assert_eq!(style_of(&s, NodeKind::Button, json!({})).background, Some(hex("#111111")));
        assert_eq!(style_of(&s, NodeKind::Button, json!({"pressed": true})).background, Some(hex("#333333")));
        assert_eq!(style_of(&s, NodeKind::Label, json!({})).color, hex("#666666"), "the later rule");
        assert_eq!(
            style_of(&s, NodeKind::Label, json!({"class": "x big"})).color,
            hex("#444444"),
            "a class beats a kind"
        );
        // An app's rule beats the system's at the same specificity, and its
        // variables replace the system's inside its windows.
        let app = Arc::new(Sheet::parse(":root { --a: #aaaaaa } button { color: #bbbbbb }").unwrap());
        let a = s.with_app(app);
        let b = style_of(&a, NodeKind::Button, json!({}));
        assert_eq!((b.background, b.color), (Some(hex("#aaaaaa")), hex("#bbbbbb")));
        // A system :root colour that names a variable follows the app's value.
        let system =
            Arc::new(Sheet::parse(":root { --accent: #0000ff; accent-color: var(--accent) }").unwrap());
        let app = Arc::new(Sheet::parse(":root { --accent: #ff8800 }").unwrap());
        let top = Scope::system(system, false);
        let inside = top.with_app(app).root(Some(&top.root(None)));
        assert_eq!((top.root(None).accent, inside.accent), (hex("#0000ff"), hex("#ff8800")));
        // ...but not a more specific system rule.
        let app = Arc::new(Sheet::parse("button { background: #cccccc }").unwrap());
        let pressed = style_of(&s.with_app(app), NodeKind::Button, json!({"pressed": true}));
        assert_eq!(pressed.background, Some(hex("#333333")));
    }

    #[test]
    fn colour_and_fonts_inherit_but_backgrounds_dont() {
        let sheet = Arc::new(
            Sheet::parse(
                ":root { color: #101010; font-family: Inter, sans-serif; font-size: 15px }
                 .code { font-family: \"DejaVu Sans Mono\", monospace; background: #202020; font-weight: bold }
                 .note { font-style: italic }",
            )
            .unwrap(),
        );
        let s = Scope::system(sheet, false);
        let root = s.root(None);
        assert_eq!((root.color, root.font.size), (hex("#101010"), 15.0));
        let p = props(json!({"class": "code"}));
        let code = s.compute(&root, &NodeInfo::of(NodeKind::Vstack, &p));
        assert_eq!(&*code.font.families, ["DejaVu Sans Mono", "monospace"]);
        assert_eq!((code.font.weight, code.background), (700, Some(hex("#202020"))));
        let child = s.compute(&code, &NodeInfo::of(NodeKind::Label, &protocol::Props::new()));
        assert_eq!(&*child.font.families, ["DejaVu Sans Mono", "monospace"], "fonts inherit");
        assert_eq!((child.color, child.background), (hex("#101010"), None), "backgrounds don't");
        let p = props(json!({"class": "note"}));
        assert!(s.compute(&child, &NodeInfo::of(NodeKind::Label, &p)).font.italic);
    }

    #[test]
    fn dark_rules_apply_only_in_the_dark_theme() {
        let sheet = Arc::new(
            Sheet::parse(":root { --t: #000000; color: var(--t) } @dark { :root { --t: #ffffff } label { color: #ff0000 } }").unwrap(),
        );
        assert_eq!(
            style_of(&Scope::system(sheet.clone(), false), NodeKind::Label, json!({})).color,
            hex("#000000")
        );
        let dark = Scope::system(sheet, true);
        assert_eq!(dark.root(None).color, hex("#ffffff"));
        assert_eq!(style_of(&dark, NodeKind::Label, json!({})).color, hex("#ff0000"));
    }

    #[test]
    fn colours_take_every_form_and_props_resolve_tokens() {
        let sheet = Arc::new(
            Sheet::parse(":root { --x: #abc; --y: var(--x); --z: #11223344 } label { color: var(--y); background: transparent }")
                .unwrap(),
        );
        let s = Scope::system(sheet, false);
        let l = style_of(&s, NodeKind::Label, json!({}));
        assert_eq!((l.color, l.background), (hex("#aabbcc"), Some(Rgba::TRANSPARENT)));
        assert_eq!(l.resolve("$z"), Some(hex("#11223344")));
        assert_eq!(l.resolve("var(--x)"), Some(hex("#aabbcc")));
        assert_eq!(l.resolve("#010203"), Some(hex("#010203")));
        assert_eq!(l.resolve("$nope"), None);
    }

    #[test]
    fn mistakes_are_reported_with_where_they_are() {
        let err = |src: &str| Sheet::parse(src).unwrap_err();
        let e = err("button {\n  colour: red;\n}");
        assert_eq!((e.line, e.col), (2, 3));
        assert!(e.message.contains("no property \"colour\""), "{e}");
        assert!(err("buton { color: #fff }").message.contains("no widget kind"));
        assert!(err("button:hovered { color: #fff }").message.contains("no state"));
        assert!(err("button .x { color: #fff }").message.contains("nothing between"));
        assert!(err("vstack > button { color: #fff }").message.contains("nothing between"));
        assert!(err("button { color: red }").message.contains("isn't a colour"));
        assert!(err("button { --x: #fff }").message.contains(":root only"));
        assert!(err("button { margin: 4px }").message.contains("colours and fonts only"));
        assert!(err("button { color: #fff").message.contains("never closes"));
        assert!(err("@media x { }").message.contains("only @dark"));
        assert!(err("button:hover { font-weight: bold }").message.contains("colours only"));
        assert!(err("/* open").message.contains("never ends"));
        assert!(err(&"a".repeat(MAX_SHEET_BYTES + 1)).message.contains("bytes"));
        // Comments, missing final semicolons and selector lists are fine.
        assert!(Sheet::parse("/* hi */ button, .x:focus, label.y { color: #fff } :root{--a:#000}").is_ok());
    }
}
