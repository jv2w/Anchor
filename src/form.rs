//! Forms: the modal, button list and custom windows a player answers, with typed answers.
//!
//! Build a [`Modal`], [`Buttons`] or [`Custom`] form and send it with
//! [`Context::send_form`](crate::Context::send_form); the handler runs once, in the tick, with
//! the player's [`Response`]:
//!
//! ```
//! use anchor::Context;
//! use anchor::form::{Button, Buttons, Custom, Image, Response, Value};
//! use anchor::player;
//!
//! fn open_buttons(cx: &mut Context<'_>, player: player::Id) {
//!     let buttons = Buttons::new("Lobby")
//!         .with_content("Pick a game")
//!         .button(
//!             Button::new("PvP")
//!                 .with_image(Image::Path("textures/items/iron_sword".to_owned())),
//!         )
//!         .button("Settings");
//!     let sent = cx.send_form(player, buttons, |cx, player, response| {
//!         if response == Response::Buttons(1) {
//!             let settings = Custom::new("Settings")
//!                 .toggle("Music", true)
//!                 .slider("Volume", 0.0, 100.0, 5.0, 50.0);
//!             let _ = cx.send_form(player, settings, |_, _, response| {
//!                 if let Response::Custom(values) = response {
//!                     let _ = matches!(values.first(), Some(Value::Bool(true)));
//!                 }
//!             });
//!         }
//!     });
//!     let _ = sent;
//! }
//! ```
//!
//! The answer is checked against the form that was sent: its JSON is at most 64 KiB, and every
//! value must fit its element (a toggle a bool, a slider a number within its range on a step, a
//! dropdown an index of an option, an input text of at most 8 KiB). An answer that does not fit
//! is dropped, and the handler of that form never runs. A form whose own default answer would
//! not fit is refused when it is sent. A player has at most 16 forms waiting for an answer.

use std::fmt;

use serde::Serialize;
use serde_json::{Value as Json, json};

use crate::data::{JsonError, to_json};
use crate::math;

/// The most elements (buttons, labels, inputs…) one form has.
const MAX_ELEMENTS: usize = 256;
/// The longest form sent, in bytes of JSON.
const MAX_FORM_BYTES: usize = 256 * 1024;
/// The longest text an input answers, in bytes.
const MAX_INPUT_BYTES: usize = 8 * 1024;
/// The most options of one dropdown or steps of one step slider.
const MAX_OPTIONS: usize = 256;
/// The most forms one player has waiting for an answer.
pub(crate) const MAX_PENDING: usize = 16;
/// The longest answer accepted, in bytes of JSON; a longer one is dropped before it is parsed.
pub(crate) const MAX_RESPONSE_BYTES: usize = 64 * 1024;
/// The `timeout` the official server writes on every slider, copied as it is: its unit is not known.
const SLIDER_TIMEOUT: u32 = 100;
/// How far, in steps, a slider answer may lie off a step or outside the range and still count:
/// the client computes the value in 32-bit floats.
const STEP_TOLERANCE: f64 = 1e-3;

/// A form to send to a player: one of the three kinds Bedrock shows.
#[derive(Debug, Clone, PartialEq)]
// api-class: Form
pub enum Form {
    /// A question with two buttons.
    Modal(Modal),
    /// A list of buttons.
    Buttons(Buttons),
    /// A window of inputs the player fills out.
    Custom(Custom),
}

impl Form {
    /// Checks the form and returns its JSON and the shape of its answer.
    pub(crate) fn prepare(&self) -> Result<(Vec<u8>, Shape)> {
        let (json, shape) = match self {
            Self::Modal(m) => (
                json!({
                    "type": "modal",
                    "title": m.title,
                    "content": m.content,
                    "button1": m.first,
                    "button2": m.second,
                }),
                Shape::Modal,
            ),
            Self::Buttons(b) => buttons(b)?,
            Self::Custom(c) => custom(c)?,
        };
        let bytes = encode(&json, MAX_FORM_BYTES, "the JSON is longer than 256 KiB")?;
        Ok((bytes, shape))
    }
}

impl From<Buttons> for Form {
    fn from(form: Buttons) -> Self {
        Self::Buttons(form)
    }
}

impl From<Custom> for Form {
    fn from(form: Custom) -> Self {
        Self::Custom(form)
    }
}

impl From<Modal> for Form {
    fn from(form: Modal) -> Self {
        Self::Modal(form)
    }
}

/// A question with a title, a text and two buttons. The answer is
/// [`Response::Modal`]`(true)` for the first button and `(false)` for the second.
#[derive(Debug, Clone, PartialEq)]
pub struct Modal {
    title: String,
    content: String,
    first: String,
    second: String,
}

impl Modal {
    /// A modal form titled `title` that shows `content` above the buttons `first` and `second`.
    pub fn new(
        title: impl Into<String>,
        content: impl Into<String>,
        first: impl Into<String>,
        second: impl Into<String>,
    ) -> Self {
        Self {
            title: title.into(),
            content: content.into(),
            first: first.into(),
            second: second.into(),
        }
    }
}

/// A list of buttons: a title, a text and buttons, with headers, labels and dividers between
/// them. The answer is [`Response::Buttons`] with the index of the button clicked, counting
/// buttons only.
#[derive(Debug, Clone, PartialEq)]
pub struct Buttons {
    title: String,
    content: String,
    elements: Vec<ButtonsElement>,
}

impl Buttons {
    /// A button list titled `title`, without text or buttons.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            content: String::new(),
            elements: Vec::new(),
        }
    }

    /// Adds a button: a [`Button`], or its text.
    #[must_use]
    pub fn button(mut self, button: impl Into<Button>) -> Self {
        self.elements.push(ButtonsElement::Button(button.into()));
        self
    }

    /// Adds a divider: a horizontal line.
    #[must_use]
    pub fn divider(mut self) -> Self {
        self.elements.push(ButtonsElement::Divider);
        self
    }

    /// Adds a header: larger text that starts a section.
    #[must_use]
    pub fn header(mut self, text: impl Into<String>) -> Self {
        self.elements.push(ButtonsElement::Header(text.into()));
        self
    }

    /// Adds a label: a box of text.
    #[must_use]
    pub fn label(mut self, text: impl Into<String>) -> Self {
        self.elements.push(ButtonsElement::Label(text.into()));
        self
    }

    /// The button list with `content` as the text shown above the elements.
    #[must_use]
    pub fn with_content(mut self, content: impl Into<String>) -> Self {
        self.content = content.into();
        self
    }
}

/// One element of a button list, in order.
#[derive(Debug, Clone, PartialEq)]
enum ButtonsElement {
    Button(Button),
    Header(String),
    Label(String),
    Divider,
}

/// A button of a button list: its text and an optional image.
#[derive(Debug, Clone, PartialEq)]
pub struct Button {
    text: String,
    image: Option<Image>,
}

impl Button {
    /// A button showing `text`, without an image.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            image: None,
        }
    }

    /// The button showing `image`.
    #[must_use]
    pub fn with_image(mut self, image: Image) -> Self {
        self.image = Some(image);
        self
    }
}

impl From<&str> for Button {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

impl From<String> for Button {
    fn from(text: String) -> Self {
        Self::new(text)
    }
}

/// The image of a button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Image {
    /// A texture of the game or of a resource pack, such as `textures/items/apple`.
    Path(String),
    /// An image the client downloads, such as `https://example.com/icon.png`.
    Url(String),
}

/// A custom form: a title and elements the player fills out. The answer is
/// [`Response::Custom`] with one [`Value`] per element, in order.
#[derive(Debug, Clone, PartialEq)]
pub struct Custom {
    title: String,
    elements: Vec<Element>,
}

impl Custom {
    /// A custom form titled `title`, without elements.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            elements: Vec::new(),
        }
    }

    /// Adds a divider: a horizontal line. Its value is [`Value::Empty`].
    #[must_use]
    pub fn divider(mut self) -> Self {
        self.elements.push(Element::Divider);
        self
    }

    /// Adds a dropdown titled `text` with `options`, `default` selected. Its value is
    /// [`Value::Index`] of an option. Sending the form fails unless there are 1 to 256 options
    /// and `default` is one of them.
    #[must_use]
    pub fn dropdown<S: Into<String>>(
        mut self,
        text: impl Into<String>,
        options: impl IntoIterator<Item = S>,
        default: usize,
    ) -> Self {
        self.elements.push(Element::Dropdown {
            text: text.into(),
            options: options.into_iter().map(Into::into).collect(),
            default,
        });
        self
    }

    /// Adds a header: larger text that starts a section. Its value is [`Value::Empty`].
    #[must_use]
    pub fn header(mut self, text: impl Into<String>) -> Self {
        self.elements.push(Element::Header(text.into()));
        self
    }

    /// Adds a text box titled `text`, showing `placeholder` while empty and filled with
    /// `default`. Its value is [`Value::Text`]. Sending the form fails unless `default` is
    /// at most 8 KiB and the answers of all the defaults together are at most 64 KiB.
    #[must_use]
    pub fn input(
        mut self,
        text: impl Into<String>,
        placeholder: impl Into<String>,
        default: impl Into<String>,
    ) -> Self {
        self.elements.push(Element::Input {
            text: text.into(),
            placeholder: placeholder.into(),
            default: default.into(),
        });
        self
    }

    /// Adds a label: a box of text. Its value is [`Value::Empty`].
    #[must_use]
    pub fn label(mut self, text: impl Into<String>) -> Self {
        self.elements.push(Element::Label(text.into()));
        self
    }

    /// Adds a slider titled `text` from `min` to `max` in steps of `step`, set at `default`.
    /// Its value is [`Value::Number`], `min` plus a whole number of steps, within the range
    /// (`max` when the range is not a whole number of steps and the answer is its last step).
    /// Sending the form fails unless the numbers are finite, `min <= default <= max` and
    /// `step > 0`.
    #[must_use]
    pub fn slider(
        mut self,
        text: impl Into<String>,
        min: f64,
        max: f64,
        step: f64,
        default: f64,
    ) -> Self {
        self.elements.push(Element::Slider {
            text: text.into(),
            min,
            max,
            step,
            default,
        });
        self
    }

    /// Adds a slider titled `text` over the named `steps`, `default` selected. Its value is
    /// [`Value::Index`] of a step. Sending the form fails unless there are 1 to 256 steps and
    /// `default` is one of them.
    #[must_use]
    pub fn step_slider<S: Into<String>>(
        mut self,
        text: impl Into<String>,
        steps: impl IntoIterator<Item = S>,
        default: usize,
    ) -> Self {
        self.elements.push(Element::StepSlider {
            text: text.into(),
            steps: steps.into_iter().map(Into::into).collect(),
            default,
        });
        self
    }

    /// Adds a switch titled `text`, on when `default`. Its value is [`Value::Bool`].
    #[must_use]
    pub fn toggle(mut self, text: impl Into<String>, default: bool) -> Self {
        self.elements.push(Element::Toggle {
            text: text.into(),
            default,
        });
        self
    }
}

/// One element of a custom form, in order.
#[derive(Debug, Clone, PartialEq)]
enum Element {
    Label(String),
    Header(String),
    Divider,
    Input {
        text: String,
        placeholder: String,
        default: String,
    },
    Toggle {
        text: String,
        default: bool,
    },
    Slider {
        text: String,
        min: f64,
        max: f64,
        step: f64,
        default: f64,
    },
    Dropdown {
        text: String,
        options: Vec<String>,
        default: usize,
    },
    StepSlider {
        text: String,
        steps: Vec<String>,
        default: usize,
    },
}

/// A player's answer to a form.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Response {
    /// The player closed the form without answering, or left the game with it open.
    Closed,
    /// The client could not show the form because it was busy.
    Busy,
    /// The button of a [`Modal`]: true for the first, false for the second.
    Modal(bool),
    /// The index of the [`Buttons`] button clicked, counting buttons only.
    Buttons(usize),
    /// The values of a [`Custom`] form, one per element in order.
    Custom(Vec<Value>),
}

/// The value of one element of a [`Custom`] form.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Value {
    /// A label, header or divider, which hold no value.
    Empty,
    /// The text of an input.
    Text(String),
    /// Whether a toggle is on.
    Bool(bool),
    /// The number of a slider.
    Number(f64),
    /// The option of a dropdown or the step of a step slider.
    Index(usize),
}

/// Why a form was not sent.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The player is not in the game: it has left, or has not joined yet.
    Offline,
    /// The player has 16 forms waiting for an answer.
    Full,
    /// The player was sent 2^32 forms; form IDs do not wrap.
    Exhausted,
    /// The form breaks a rule of its builder or is too large.
    Invalid(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Offline => f.write_str("the player is not in the game"),
            Self::Full => write!(
                f,
                "the player has {MAX_PENDING} forms waiting for an answer"
            ),
            Self::Exhausted => f.write_str("the player's form IDs are used up"),
            Self::Invalid(reason) => write!(f, "invalid form: {reason}"),
        }
    }
}

impl std::error::Error for Error {}

/// A result with the module's [`enum@Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What an answer to a sent form must look like; the form's texts are not kept.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Shape {
    Modal,
    /// The number of buttons.
    Buttons(usize),
    Custom(Vec<Expect>),
}

impl Shape {
    /// The answer `data` (at most [`MAX_RESPONSE_BYTES`]) gives, if it fits the form: `null`
    /// closes any form.
    pub(crate) fn parse(&self, data: &[u8]) -> Option<Response> {
        let json: Json = serde_json::from_slice(data).ok()?;
        if json.is_null() {
            return Some(Response::Closed);
        }
        match self {
            Self::Modal => json.as_bool().map(Response::Modal),
            Self::Buttons(count) => index(&json, *count).map(Response::Buttons),
            Self::Custom(expect) => {
                let Json::Array(values) = json else {
                    return None;
                };
                // The client sends one value per element; anything after them is ignored.
                if values.len() < expect.len() {
                    return None;
                }
                let values = values
                    .into_iter()
                    .zip(expect)
                    .map(|(v, e)| value(v, *e))
                    .collect::<Option<Vec<Value>>>()?;
                Some(Response::Custom(values))
            }
        }
    }
}

/// What one value of a custom form answer must be.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Expect {
    /// Anything (a label, header or divider; the client sends `null`).
    Nothing,
    Text,
    Bool,
    Number {
        min: f64,
        max: f64,
        step: f64,
    },
    /// An index below this.
    Index(usize),
}

/// The JSON and answer shape of a button list.
fn buttons(list: &Buttons) -> Result<(Json, Shape)> {
    if list.elements.len() > MAX_ELEMENTS {
        return Err(Error::Invalid("more than 256 elements"));
    }
    let count = list
        .elements
        .iter()
        .filter(|e| matches!(e, ButtonsElement::Button(_)))
        .count();
    let elements: Vec<Json> = list
        .elements
        .iter()
        .map(|e| match e {
            ButtonsElement::Button(b) => match &b.image {
                Some(image) => {
                    let (kind, data) = match image {
                        Image::Path(p) => ("path", p),
                        Image::Url(u) => ("url", u),
                    };
                    json!({
                        "type": "button",
                        "text": b.text,
                        "image": { "type": kind, "data": data },
                    })
                }
                // The official server writes the key for a button without an image too.
                None => json!({ "type": "button", "text": b.text, "image": null }),
            },
            ButtonsElement::Header(t) => json!({ "type": "header", "text": t }),
            ButtonsElement::Label(t) => json!({ "type": "label", "text": t }),
            ButtonsElement::Divider => json!({ "type": "divider", "text": "" }),
        })
        .collect();
    let json = json!({
        "type": "form",
        "title": list.title,
        "content": list.content,
        "elements": elements,
    });
    Ok((json, Shape::Buttons(count)))
}

/// The JSON and answer shape of a custom form.
fn custom(c: &Custom) -> Result<(Json, Shape)> {
    if c.elements.len() > MAX_ELEMENTS {
        return Err(Error::Invalid("more than 256 elements"));
    }
    let mut content = Vec::with_capacity(c.elements.len());
    let mut expect = Vec::with_capacity(c.elements.len());
    let mut default_answer = Vec::with_capacity(c.elements.len());
    for e in &c.elements {
        default_answer.push(default_value(e));
        let (json, e) = match e {
            Element::Label(t) => (json!({ "type": "label", "text": t }), Expect::Nothing),
            Element::Header(t) => (json!({ "type": "header", "text": t }), Expect::Nothing),
            Element::Divider => (json!({ "type": "divider", "text": "" }), Expect::Nothing),
            Element::Input {
                text,
                placeholder,
                default,
            } => {
                if default.len() > MAX_INPUT_BYTES {
                    return Err(Error::Invalid("an input default is longer than 8 KiB"));
                }
                (
                    json!({
                        "type": "input",
                        "text": text,
                        "placeholder": placeholder,
                        "default": default,
                    }),
                    Expect::Text,
                )
            }
            Element::Toggle { text, default } => (
                json!({ "type": "toggle", "text": text, "default": default }),
                Expect::Bool,
            ),
            &Element::Slider {
                ref text,
                min,
                max,
                step,
                default,
            } => {
                check_slider(min, max, step, default)?;
                (
                    // The official server (`@minecraft/server-ui`) writes `"timeout": 100` on
                    // every slider.
                    json!({
                        "type": "slider",
                        "text": text,
                        "min": min,
                        "max": max,
                        "step": step,
                        "default": default,
                        "timeout": SLIDER_TIMEOUT,
                    }),
                    Expect::Number { min, max, step },
                )
            }
            Element::Dropdown {
                text,
                options,
                default,
            } => {
                check_options(options, *default)?;
                (
                    json!({
                        "type": "dropdown",
                        "text": text,
                        "options": options,
                        "default": default,
                    }),
                    Expect::Index(options.len()),
                )
            }
            Element::StepSlider {
                text,
                steps,
                default,
            } => {
                check_options(steps, *default)?;
                (
                    json!({
                        "type": "step_slider",
                        "text": text,
                        "steps": steps,
                        "default": default,
                    }),
                    Expect::Index(steps.len()),
                )
            }
        };
        content.push(json);
        expect.push(e);
    }
    check_default_answer(&default_answer)?;
    // The official server writes `"icon": null` on every custom form.
    let json = json!({
        "type": "custom_form",
        "title": c.title,
        "icon": null,
        "content": content,
    });
    Ok((json, Shape::Custom(expect)))
}

/// Checks that the answer a client sends when the player changes nothing, `default_answer`, fits
/// [`MAX_RESPONSE_BYTES`] as JSON.
fn check_default_answer(default_answer: &[Json]) -> Result<()> {
    encode(
        default_answer,
        MAX_RESPONSE_BYTES,
        "the default answer is longer than 64 KiB",
    )?;
    Ok(())
}

/// `value` as JSON of at most `limit` bytes; `too_large` is the reason a longer one is refused.
fn encode(
    value: &(impl Serialize + ?Sized),
    limit: usize,
    too_large: &'static str,
) -> Result<Vec<u8>> {
    to_json(value, limit).map_err(|e| match e {
        JsonError::TooLarge => Error::Invalid(too_large),
        JsonError::Encode => Error::Invalid("not serializable"),
    })
}

/// Checks the numbers of a slider: finite, ordered, a positive step, and a range whose number
/// of steps is finite.
fn check_slider(min: f64, max: f64, step: f64, default: f64) -> Result<()> {
    let finite = [min, max, step, default].iter().all(|v| v.is_finite());
    if !(finite && min <= default && default <= max) || step <= 0.0 {
        return Err(Error::Invalid(
            "a slider needs finite numbers, min <= default <= max and step > 0",
        ));
    }
    if !((max - min) / step).is_finite() {
        return Err(Error::Invalid("a slider has too many steps"));
    }
    Ok(())
}

/// Checks the options of a dropdown or the steps of a step slider.
fn check_options(options: &[String], default: usize) -> Result<()> {
    if options.is_empty() || options.len() > MAX_OPTIONS || default >= options.len() {
        return Err(Error::Invalid(
            "a dropdown or step slider needs 1 to 256 options and a default among them",
        ));
    }
    Ok(())
}

/// The value a client sends for `element` when the player changes nothing.
fn default_value(element: &Element) -> Json {
    match element {
        Element::Label(_) | Element::Header(_) | Element::Divider => Json::Null,
        Element::Input { default, .. } => json!(default),
        Element::Toggle { default, .. } => json!(default),
        Element::Slider { default, .. } => json!(default),
        Element::Dropdown { default, .. } | Element::StepSlider { default, .. } => json!(default),
    }
}

/// The value `json` gives an element expecting `expect`, if it fits.
fn value(json: Json, expect: Expect) -> Option<Value> {
    match (expect, json) {
        (Expect::Nothing, _) => Some(Value::Empty),
        (Expect::Text, Json::String(s)) => (s.len() <= MAX_INPUT_BYTES).then_some(Value::Text(s)),
        (Expect::Bool, Json::Bool(b)) => Some(Value::Bool(b)),
        (Expect::Number { min, max, step }, Json::Number(n)) => {
            slider(n.as_f64()?, min, max, step).map(Value::Number)
        }
        (Expect::Index(len), json) => index(&json, len).map(Value::Index),
        _ => None,
    }
}

/// `json` as an index below `len`.
fn index(json: &Json, len: usize) -> Option<usize> {
    let i = usize::try_from(json.as_u64()?).ok()?;
    (i < len).then_some(i)
}

/// A slider answer `v` snapped to its step, if it lies within the range and on a step (both
/// within [`STEP_TOLERANCE`] steps).
fn slider(v: f64, min: f64, max: f64, step: f64) -> Option<f64> {
    let steps = (v - min) / step;
    let whole = steps.round();
    let last = ((max - min) / step).round();
    let off = steps - whole;
    let fits = steps.is_finite()
        && -STEP_TOLERANCE < off
        && off < STEP_TOLERANCE
        && steps > -STEP_TOLERANCE
        && whole <= last;
    fits.then(|| math::clamp(min + whole * step, min, max))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(form: impl Into<Form>) -> Json {
        let (bytes, _) = form.into().prepare().unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn shape(form: impl Into<Form>) -> Shape {
        form.into().prepare().unwrap().1
    }

    // api: Form
    #[test]
    fn forms_serialize_to_the_json_the_official_server_sends_with_escaping() {
        assert_eq!(
            json(Modal::new("T \"q\"", "body\n", "Yes", "No")),
            json!({
                "type": "modal", "title": "T \"q\"", "content": "body\n",
                "button1": "Yes", "button2": "No",
            })
        );
        let buttons = Buttons::new("M")
            .with_content("c")
            .header("h")
            .button("plain")
            .label("l")
            .divider()
            .button(Button::new("path").with_image(Image::Path("textures/items/apple".to_owned())))
            .button(Button::new("url").with_image(Image::Url("https://e.x/i.png".to_owned())));
        assert_eq!(
            json(buttons.clone()),
            json!({
                "type": "form", "title": "M", "content": "c",
                "elements": [
                    { "type": "header", "text": "h" },
                    { "type": "button", "text": "plain", "image": null },
                    { "type": "label", "text": "l" },
                    { "type": "divider", "text": "" },
                    { "type": "button", "text": "path",
                      "image": { "type": "path", "data": "textures/items/apple" } },
                    { "type": "button", "text": "url",
                      "image": { "type": "url", "data": "https://e.x/i.png" } },
                ],
            })
        );
        assert_eq!(shape(buttons), Shape::Buttons(3));
        let custom = Custom::new("C")
            .label("l")
            .header("h")
            .divider()
            .input("name", "Steve", "")
            .toggle("on", true)
            .slider("vol", 0.0, 10.0, 0.5, 2.5)
            .dropdown("pick", ["a", "b"], 1)
            .step_slider("size", ["s", "m", "l"], 0);
        assert_eq!(
            json(custom),
            json!({
                "type": "custom_form", "title": "C", "icon": null,
                "content": [
                    { "type": "label", "text": "l" },
                    { "type": "header", "text": "h" },
                    { "type": "divider", "text": "" },
                    { "type": "input", "text": "name", "placeholder": "Steve", "default": "" },
                    { "type": "toggle", "text": "on", "default": true },
                    { "type": "slider", "text": "vol", "min": 0.0, "max": 10.0, "step": 0.5,
                      "default": 2.5, "timeout": 100 },
                    { "type": "dropdown", "text": "pick", "options": ["a", "b"], "default": 1 },
                    { "type": "step_slider", "text": "size", "steps": ["s", "m", "l"],
                      "default": 0 },
                ],
            })
        );
        let (bytes, _) = Form::from(Modal::new("\u{1}", "", "", ""))
            .prepare()
            .unwrap();
        assert!(
            String::from_utf8(bytes)
                .unwrap()
                .contains(r#""title":"\u0001""#)
        );
    }

    #[test]
    fn invalid_forms_are_refused() {
        let invalid = |form: Custom| matches!(Form::from(form).prepare(), Err(Error::Invalid(_)));
        let base = || Custom::new("C");
        assert!(invalid(base().slider("s", 0.0, 1.0, 0.0, 0.0)));
        assert!(invalid(base().slider("s", 0.0, 1.0, -1.0, 0.0)));
        assert!(invalid(base().slider("s", 1.0, 0.0, 1.0, 0.5)));
        assert!(invalid(base().slider("s", 0.0, 1.0, 1.0, 2.0)));
        assert!(invalid(base().slider("s", 0.0, f64::NAN, 1.0, 0.0)));
        assert!(invalid(base().slider("s", 0.0, f64::INFINITY, 1.0, 0.0)));
        assert!(invalid(base().slider("s", -f64::MAX, f64::MAX, 1.0, 0.0)));
        assert!(invalid(base().dropdown("d", Vec::<String>::new(), 0)));
        assert!(invalid(base().dropdown("d", ["a"], 1)));
        assert!(invalid(base().step_slider("d", vec!["x"; 257], 0)));
        let many = (0..257).fold(base(), |c, _| c.divider());
        assert!(invalid(many));
        let buttons = (0..257).fold(Buttons::new("M"), |b, _| b.button("b"));
        assert!(matches!(
            Form::from(buttons).prepare(),
            Err(Error::Invalid(_))
        ));
        let huge = Modal::new("x".repeat(MAX_FORM_BYTES), "", "", "");
        assert!(matches!(Form::from(huge).prepare(), Err(Error::Invalid(_))));
        assert!(
            Form::from(base().slider("s", 0.0, 1.0, 0.25, 1.0))
                .prepare()
                .is_ok()
        );
    }

    #[test]
    fn a_form_whose_default_answer_would_be_dropped_is_refused() {
        let invalid = |form: Custom| matches!(Form::from(form).prepare(), Err(Error::Invalid(_)));
        let inputs = |count: usize, bytes: usize| {
            (0..count).fold(Custom::new("C"), |c, _| c.input("i", "", "x".repeat(bytes)))
        };
        assert!(!invalid(inputs(1, MAX_INPUT_BYTES)));
        assert!(invalid(inputs(1, MAX_INPUT_BYTES + 1)));
        // 7 inputs of 8 KiB answer in under 64 KiB, 9 do not.
        let seven = inputs(7, MAX_INPUT_BYTES);
        let nine = inputs(9, MAX_INPUT_BYTES);
        assert!(!invalid(seven.clone()));
        assert!(invalid(nine));
        let (_, shape) = Form::from(seven).prepare().unwrap();
        let answer = serde_json::to_vec(&vec!["x".repeat(MAX_INPUT_BYTES); 7]).unwrap();
        assert!(answer.len() <= MAX_RESPONSE_BYTES);
        assert!(shape.parse(&answer).is_some());
    }

    #[test]
    fn the_refusals_name_the_limits_they_enforce() {
        let reason = |form: Form| match form.prepare() {
            Err(Error::Invalid(reason)) => reason,
            other => panic!("{other:?}"),
        };
        let elements = format!("more than {MAX_ELEMENTS} elements");
        let buttons = (0..=MAX_ELEMENTS).fold(Buttons::new("M"), |b, _| b.button("b"));
        assert_eq!(reason(Form::from(buttons)), elements);
        let custom = (0..=MAX_ELEMENTS).fold(Custom::new("C"), |c, _| c.divider());
        assert_eq!(reason(Form::from(custom)), elements);
        let options = format!("1 to {MAX_OPTIONS} options");
        let dropdown = Custom::new("C").dropdown("d", vec!["x"; MAX_OPTIONS + 1], 0);
        assert!(reason(Form::from(dropdown)).contains(&options));
        let huge = Modal::new("x".repeat(MAX_FORM_BYTES), "", "", "");
        assert_eq!(
            reason(Form::from(huge)),
            format!("the JSON is longer than {} KiB", MAX_FORM_BYTES / 1024)
        );
        let input = Custom::new("C").input("i", "", "x".repeat(MAX_INPUT_BYTES + 1));
        assert_eq!(
            reason(Form::from(input)),
            format!(
                "an input default is longer than {} KiB",
                MAX_INPUT_BYTES / 1024
            )
        );
        let answer = (0..=MAX_RESPONSE_BYTES / MAX_INPUT_BYTES).fold(Custom::new("C"), |c, _| {
            c.input("i", "", "x".repeat(MAX_INPUT_BYTES))
        });
        assert_eq!(
            reason(Form::from(answer)),
            format!(
                "the default answer is longer than {} KiB",
                MAX_RESPONSE_BYTES / 1024
            )
        );
        let source = include_str!("form.rs");
        for text in [
            format!("fails unless there are {options}"),
            format!("fails unless there are 1 to {MAX_OPTIONS} steps"),
            format!("at most {} KiB and the answers", MAX_INPUT_BYTES / 1024),
            format!("together are at most {} KiB", MAX_RESPONSE_BYTES / 1024),
        ] {
            assert!(source.contains(&text), "{text}");
        }
    }

    #[test]
    fn answers_parse_by_the_shape_of_the_form() {
        let modal = shape(Modal::new("", "", "a", "b"));
        assert_eq!(modal.parse(b"true"), Some(Response::Modal(true)));
        assert_eq!(modal.parse(b"false\n"), Some(Response::Modal(false)));
        assert_eq!(modal.parse(b"null"), Some(Response::Closed));
        assert_eq!(modal.parse(b"1"), None);
        let buttons = shape(Buttons::new("").label("l").button("a").button("b"));
        assert_eq!(buttons.parse(b"1"), Some(Response::Buttons(1)));
        assert_eq!(buttons.parse(b"null"), Some(Response::Closed));
        for bad in [
            &b"2"[..],
            b"-1",
            b"1.5",
            b"\"0\"",
            b"true",
            b"",
            b"[0]",
            b"0 0",
        ] {
            assert_eq!(buttons.parse(bad), None, "{}", String::from_utf8_lossy(bad));
        }
        let custom = shape(
            Custom::new("")
                .label("l")
                .input("i", "", "")
                .toggle("t", false)
                .slider("s", -1.0, 1.0, 0.1, 0.0)
                .dropdown("d", ["a", "b"], 0)
                .step_slider("ss", ["x", "y", "z"], 0),
        );
        assert_eq!(
            custom.parse(br#"[null, "hi \"x\"", true, 0.30000001192092896, 1, 2, "extra"]"#),
            Some(Response::Custom(vec![
                Value::Empty,
                Value::Text("hi \"x\"".to_owned()),
                Value::Bool(true),
                Value::Number(-1.0 + 13.0 * 0.1),
                Value::Index(1),
                Value::Index(2),
            ]))
        );
        assert_eq!(custom.parse(b"null"), Some(Response::Closed));
        for bad in [
            &br#"[null, "", true, 0, 1]"#[..],
            br"[null, 1, true, 0, 1, 2]",
            br#"[null, "", 1, 0, 1, 2]"#,
            br#"[null, "", true, 1.5, 1, 2]"#,
            br#"[null, "", true, 0.05, 1, 2]"#,
            br#"[null, "", true, "0", 1, 2]"#,
            br#"[null, "", true, 0, 2, 2]"#,
            br#"[null, "", true, 0, 1, 3]"#,
            br#"[null, "", true, 0, -1, 2]"#,
            br#"[null, "", true, 1e400, 1, 2]"#,
            br#"{"0": null}"#,
            br#"[null, "", true, 0, 1, 2"#,
        ] {
            assert_eq!(custom.parse(bad), None, "{}", String::from_utf8_lossy(bad));
        }
        let long = format!(
            "[null, \"{}\", true, 0, 0, 0]",
            "a".repeat(MAX_INPUT_BYTES + 1)
        );
        assert_eq!(custom.parse(long.as_bytes()), None);
        let deep = "[".repeat(10_000);
        assert_eq!(custom.parse(deep.as_bytes()), None);
    }

    #[test]
    fn slider_answers_snap_to_the_step_within_float_error() {
        assert_eq!(slider(0.699_999_988, 0.0, 0.7, 0.1), Some(0.7));
        assert_eq!(slider(0.100_000_001_49, 0.0, 0.1, 0.1), Some(0.1));
        assert_eq!(slider(-0.000_000_1, 0.0, 1.0, 0.5), Some(0.0));
        assert_eq!(slider(1.000_000_1, 0.0, 1.0, 0.5), Some(1.0));
        assert_eq!(slider(1.5, 0.0, 1.0, 0.5), None);
        assert_eq!(slider(-0.5, 0.0, 1.0, 0.5), None);
        assert_eq!(slider(0.25, 0.0, 1.0, 0.5), None);
        assert_eq!(slider(f64::MAX, -1.0, 1.0, 1e-300), None);
        assert_eq!(slider(5.0, 5.0, 5.0, 1.0), Some(5.0));
    }
}
