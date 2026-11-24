//! The fill spec: what to put in which placeholder, read from JSON or built
//! from command-line flags.

use std::{collections::HashSet, fmt, path::PathBuf, str::FromStr};

use anyhow::{Context as _, Result, bail};
use ciclostile::Length;
use serde::{
    Deserialize, Deserializer,
    de::{self, MapAccess, Visitor},
};
use serde_json::Value;

/// The placeholder delimiters `fill` checks the result with, unless the
/// spec turns the check off.
pub(crate) const DEFAULT_DELIMITERS: [&str; 2] = ["{{", "}}"];

/// Everything one `fill` run does.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Spec {
    /// The template to read. `-` reads standard input.
    pub(crate) template: Option<PathBuf>,
    /// Where to write the result. `-` writes to standard output.
    pub(crate) output: Option<PathBuf>,
    /// Text replacements, in the order given.
    #[serde(default)]
    pub(crate) text: TextMap,
    /// Image replacements, applied in order after the text.
    #[serde(default)]
    pub(crate) images: Vec<ImageSpec>,
    /// Table replacements, applied in order after the images.
    #[serde(default)]
    pub(crate) tables: Vec<TableSpec>,
    /// Make Word offer to update fields (tables of contents, page
    /// references) when it opens the result.
    #[serde(default)]
    pub(crate) update_fields: bool,
    /// Fail, writing nothing, when a placeholder to replace isn't found or
    /// one is left in the result.
    #[serde(default)]
    pub(crate) strict: bool,
    /// The opening and closing delimiters of the placeholders to look for
    /// in the result, or `None` (`null` in JSON) not to look.
    #[serde(default = "default_delimiters")]
    pub(crate) delimiters: Option<[String; 2]>,
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            template: None,
            output: None,
            text: TextMap::default(),
            images: Vec::new(),
            tables: Vec::new(),
            update_fields: false,
            strict: false,
            delimiters: default_delimiters(),
        }
    }
}

impl Spec {
    /// Check that no placeholder is given twice, across text, images and
    /// tables: the first replacement of a placeholder replaces every
    /// occurrence, so a second one would find none.
    pub(crate) fn check_unique(&self) -> Result<()> {
        let mut seen = HashSet::new();
        let placeholders = self
            .text
            .0
            .iter()
            .map(|(placeholder, _)| placeholder)
            .chain(self.images.iter().map(|image| &image.placeholder))
            .chain(self.tables.iter().map(|table| &table.placeholder));
        for placeholder in placeholders {
            if !seen.insert(placeholder.as_str()) {
                bail!(
                    "{placeholder} is given more than once: one replacement already \
                     replaces every occurrence"
                );
            }
        }
        Ok(())
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "serde's `default` needs the field's type"
)]
fn default_delimiters() -> Option<[String; 2]> {
    Some(DEFAULT_DELIMITERS.map(str::to_owned))
}

/// The text replacements of a spec, in the order given.
#[derive(Debug, Default)]
pub(crate) struct TextMap(pub(crate) Vec<(String, TextValue)>);

impl TextMap {
    /// Set the value of `placeholder`, in place if it already has one.
    pub(crate) fn set(&mut self, placeholder: &str, value: TextValue) {
        match self.0.iter_mut().find(|(key, _)| key == placeholder) {
            Some((_, existing)) => *existing = value,
            None => self.0.push((placeholder.to_owned(), value)),
        }
    }
}

impl<'de> Deserialize<'de> for TextMap {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// Reads the JSON object keeping its order and every key, so that
        /// [`Spec::check_unique`] refuses one given twice instead of
        /// `serde_json` letting the last one win.
        struct TextMapVisitor;

        impl<'de> Visitor<'de> for TextMapVisitor {
            type Value = TextMap;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object from placeholders to their text")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<TextMap, A::Error> {
                let mut entries: Vec<(String, TextValue)> = Vec::new();
                while let Some((placeholder, value)) = map.next_entry::<String, Value>()? {
                    let value =
                        TextValue::from_json(&placeholder, value).map_err(de::Error::custom)?;
                    entries.push((placeholder, value));
                }
                Ok(TextMap(entries))
            }
        }

        deserializer.deserialize_map(TextMapVisitor)
    }
}

/// The value of a text placeholder: the text itself, or a file to read it
/// from.
#[derive(Debug)]
pub(crate) enum TextValue {
    Inline(String),
    File(PathBuf),
}

impl TextValue {
    /// Read the JSON value given for `placeholder`: a string, a number or a
    /// boolean as text, or `{"file": "path"}`.
    fn from_json(placeholder: &str, value: Value) -> Result<Self, String> {
        let expected = r#"a string, a number, a boolean or {"file": "path"}"#;
        match value {
            Value::String(text) => Ok(Self::Inline(text)),
            Value::Number(number) => Ok(Self::Inline(number.to_string())),
            Value::Bool(value) => Ok(Self::Inline(value.to_string())),
            Value::Object(mut object) => {
                let file = object.remove("file");
                if let Some(field) = object.keys().next() {
                    return Err(format!(
                        "text: {placeholder} has an unknown field {field:?}: expected {expected}"
                    ));
                }
                match file {
                    Some(Value::String(path)) => Ok(Self::File(PathBuf::from(path))),
                    _ => Err(format!(
                        "text: {placeholder} needs \"file\" as a string path: expected {expected}"
                    )),
                }
            }
            other => Err(format!(
                "text: {placeholder} is {other}: expected {expected}"
            )),
        }
    }
}

/// An image to put in place of a placeholder.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImageSpec {
    /// The placeholder the image replaces, every occurrence of it.
    pub(crate) placeholder: String,
    /// A PNG or JPEG file.
    pub(crate) path: PathBuf,
    /// The display width; the height follows unless set too.
    pub(crate) width: Option<Len>,
    /// The display height; the width follows unless set too.
    pub(crate) height: Option<Len>,
    /// The text screen readers announce in place of the image.
    pub(crate) alt_text: Option<String>,
}

/// A table to put in place of each paragraph holding only a placeholder.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TableSpec {
    /// The placeholder, alone in its paragraph.
    pub(crate) placeholder: String,
    /// Cell texts, row by row.
    pub(crate) rows: Vec<Vec<Cell>>,
    /// Make the first row a bold header that repeats on every page.
    #[serde(default)]
    pub(crate) header: bool,
    /// A table style id defined in the template, such as `TableGrid`.
    pub(crate) style: Option<String>,
    /// The widths of the columns, from the first one on.
    #[serde(default)]
    pub(crate) column_widths: Vec<Len>,
}

/// A table cell. Numbers and booleans are accepted so that JSON written by
/// hand or by a program doesn't need every cell quoted; `null` is empty.
#[derive(Debug)]
pub(crate) struct Cell(pub(crate) String);

impl<'de> Deserialize<'de> for Cell {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::String(text) => Ok(Self(text)),
            Value::Null => Ok(Self(String::new())),
            Value::Bool(value) => Ok(Self(value.to_string())),
            Value::Number(number) => Ok(Self(number.to_string())),
            other => Err(serde::de::Error::custom(format!(
                "a table cell must be a string, number, boolean or null, not {other}"
            ))),
        }
    }
}

/// A length with its unit, such as `4cm`, `40mm`, `1.5in`, `12pt`,
/// `100px` or `360000emu`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Len(pub(crate) Length);

impl FromStr for Len {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let text = text.trim();
        let split = text
            .find(|c: char| c.is_ascii_alphabetic())
            .with_context(|| format!("length {text:?} has no unit (cm, mm, in, pt, px or emu)"))?;
        let (number, unit) = text.split_at(split);
        let number = number.trim();
        let parse = || -> Result<f64> {
            let value: f64 = number
                .parse()
                .with_context(|| format!("invalid number {number:?} in length {text:?}"))?;
            if !value.is_finite() || value < 0.0 {
                bail!("length {text:?} must be a finite, non-negative number");
            }
            Ok(value)
        };
        let length = match unit.to_ascii_lowercase().as_str() {
            "cm" => Length::from_cm(parse()?),
            "mm" => Length::from_mm(parse()?),
            "in" => Length::from_inches(parse()?),
            "pt" => Length::from_pt(parse()?),
            "px" => Length::from_px(
                number
                    .parse()
                    .with_context(|| format!("pixels must be a whole number in {text:?}"))?,
            ),
            "emu" => Length::from_emu(
                number
                    .parse()
                    .with_context(|| format!("EMU must be a whole number in {text:?}"))?,
            ),
            _ => bail!("unknown unit {unit:?} in length {text:?} (use cm, mm, in, pt, px or emu)"),
        };
        Ok(Self(length))
    }
}

impl<'de> Deserialize<'de> for Len {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse()
            .map_err(|err| serde::de::Error::custom(format!("{err:#}")))
    }
}

/// Split a `PLACEHOLDER=VALUE` flag argument at its first `=`; `value`
/// names the value in the error message, such as `TEXT` or `PATH`.
pub(crate) fn split_pair<'a>(flag: &str, value: &str, arg: &'a str) -> Result<(&'a str, &'a str)> {
    match arg.split_once('=') {
        Some((key, rest)) if !key.is_empty() => Ok((key, rest)),
        _ => bail!("{flag} expects PLACEHOLDER={value}, got {arg:?}"),
    }
}

/// Parse an `--image PLACEHOLDER=PATH[,width=..][,height=..][,alt=..]`
/// argument.
///
/// A comma-separated segment that doesn't start with a known option name
/// belongs to the segment before it, so paths and alt texts can hold
/// commas, as long as no comma in them is followed by `width=`, `height=`
/// or `alt=`.
pub(crate) fn parse_image_flag(arg: &str) -> Result<ImageSpec> {
    const OPTIONS: [&str; 3] = ["width", "height", "alt"];

    let (placeholder, rest) = split_pair("--image", "PATH", arg)?;
    let mut segments = rest.split(',');
    let mut path = segments.next().unwrap_or_default().to_owned();
    let mut options: Vec<(&str, String)> = Vec::new();
    for segment in segments {
        match segment.split_once('=') {
            Some((name, value)) if OPTIONS.contains(&name) => {
                options.push((name, value.to_owned()));
            }
            _ => {
                let target = options.last_mut().map_or(&mut path, |(_, value)| value);
                target.push(',');
                target.push_str(segment);
            }
        }
    }
    if path.is_empty() {
        bail!("--image {arg:?} has no file path");
    }

    let mut image = ImageSpec {
        placeholder: placeholder.to_owned(),
        path: PathBuf::from(path),
        width: None,
        height: None,
        alt_text: None,
    };
    for (name, value) in options {
        match name {
            "width" => image.width = Some(value.parse()?),
            "height" => image.height = Some(value.parse()?),
            _ => image.alt_text = Some(value),
        }
    }
    Ok(image)
}

/// Parse a `--table PLACEHOLDER=JSON` argument, where JSON (or `@FILE`
/// holding it) is either an array of rows or an object with `rows` and
/// the optional `header`, `style` and `column_widths`.
pub(crate) fn parse_table_flag(arg: &str) -> Result<TableSpec> {
    let (placeholder, rest) = split_pair("--table", "JSON", arg)?;
    let json = match rest.strip_prefix('@') {
        Some(path) => {
            std::fs::read_to_string(path).with_context(|| format!("reading table file {path:?}"))?
        }
        None => rest.to_owned(),
    };
    let value: Value = serde_json::from_str(&json)
        .with_context(|| format!("--table {placeholder}: invalid JSON"))?;
    let mut object = match value {
        rows @ Value::Array(_) => serde_json::Map::from_iter([("rows".to_owned(), rows)]),
        Value::Object(object) => object,
        _ => bail!("--table {placeholder}: expected an array of rows or an object"),
    };
    if object.contains_key("placeholder") {
        bail!("--table {placeholder}: the placeholder goes before '=', not in the JSON");
    }
    object.insert(
        "placeholder".to_owned(),
        Value::String(placeholder.to_owned()),
    );
    serde_json::from_value(Value::Object(object))
        .with_context(|| format!("--table {placeholder}: invalid table"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_need_a_unit() {
        assert_eq!("4cm".parse::<Len>().unwrap().0, Length::from_cm(4.0));
        assert_eq!(
            " 1.5 in".parse::<Len>().unwrap().0,
            Length::from_inches(1.5)
        );
        assert_eq!("100PX".parse::<Len>().unwrap().0, Length::from_px(100));
        assert_eq!("12pt".parse::<Len>().unwrap().0, Length::from_pt(12.0));
        assert_eq!("40mm".parse::<Len>().unwrap().0, Length::from_mm(40.0));
        assert_eq!("7emu".parse::<Len>().unwrap().0, Length::from_emu(7));
        assert!("4".parse::<Len>().is_err());
        assert!("4ft".parse::<Len>().is_err());
        assert!("-1cm".parse::<Len>().is_err());
        assert!("1.5px".parse::<Len>().is_err());
    }

    #[test]
    fn image_flag_keeps_commas_in_alt_text() {
        let image = parse_image_flag("{{logo}}=a,b.png,width=4cm,alt=Hello, world").unwrap();
        assert_eq!(image.placeholder, "{{logo}}");
        assert_eq!(image.path, PathBuf::from("a,b.png"));
        assert_eq!(image.width, Some(Len(Length::from_cm(4.0))));
        assert_eq!(image.height, None);
        assert_eq!(image.alt_text.as_deref(), Some("Hello, world"));
    }

    #[test]
    fn table_flag_takes_rows_or_an_object() {
        let table = parse_table_flag(r#"{{t}}=[["a", 1], [true, null]]"#).unwrap();
        let cells: Vec<Vec<&str>> = table
            .rows
            .iter()
            .map(|row| row.iter().map(|cell| cell.0.as_str()).collect())
            .collect();
        assert_eq!(cells, [["a", "1"], ["true", ""]]);
        assert!(!table.header);

        let table = parse_table_flag(
            r#"{{t}}={"rows": [["a"]], "header": true, "column_widths": ["2cm"]}"#,
        )
        .unwrap();
        assert!(table.header);
        assert_eq!(table.column_widths, [Len(Length::from_cm(2.0))]);

        assert!(parse_table_flag(r#"{{t}}={"rows": [], "colour": "red"}"#).is_err());
        assert!(parse_table_flag("{{t}}=nope").is_err());
    }

    #[test]
    fn delimiters_default_and_can_be_turned_off() {
        let spec: Spec = serde_json::from_str("{}").unwrap();
        assert_eq!(spec.delimiters, Some(["{{".to_owned(), "}}".to_owned()]));
        assert_eq!(Spec::default().delimiters, spec.delimiters);
        let spec: Spec = serde_json::from_str(r#"{"delimiters": null}"#).unwrap();
        assert_eq!(spec.delimiters, None);
        let spec: Spec = serde_json::from_str(r#"{"delimiters": ["[", "]"]}"#).unwrap();
        assert_eq!(spec.delimiters, Some(["[".to_owned(), "]".to_owned()]));
    }

    #[test]
    fn text_values_can_be_numbers_and_booleans() {
        let spec: Spec =
            serde_json::from_str(r#"{"text": {"{{n}}": 3, "{{f}}": 1.5, "{{b}}": true}}"#).unwrap();
        let values: Vec<(&str, &str)> = spec
            .text
            .0
            .iter()
            .map(|(key, value)| match value {
                TextValue::Inline(text) => (key.as_str(), text.as_str()),
                TextValue::File(_) => panic!("{key} is a file"),
            })
            .collect();
        assert_eq!(
            values,
            [("{{n}}", "3"), ("{{f}}", "1.5"), ("{{b}}", "true")]
        );
    }
}
