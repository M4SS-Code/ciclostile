# ciclostile

Fill .docx templates: replace text, insert images and tables, and keep everything else untouched.

Open a Word document, replace placeholders such as `{{name}}` with
text, an image or a table, and write the result. ciclostile doesn't model
the document: it finds the placeholders in the XML and edits it in
place, so whatever else the template holds - styles, numbering, fields,
comments, tracked changes, embedded objects - comes out as it went in.

That is meant literally: parts that no replacement touches are copied
without being re-serialized, and in a part that is edited, every byte
of XML the edit doesn't need to change stays as it was.

*Ciclostile* (/tʃikloˈstiːle/) is Italian for *mimeograph*.

## Quick start

Open the template, fill in its placeholders and write the result. The
[`fill_template`](examples/fill_template.rs) example does the same from
the command line.

```rust
use ciclostile::{Docx, Image, Length, Table};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut docx = Docx::from_bytes(std::fs::read("template.docx")?)?;

    docx.replace_text([
        ("{{customer}}", "Ada Lovelace"),
        ("{{date}}", "3 November 2026"),
        ("{{address}}", "12 St James's Square\nLondon"),
    ]);

    let logo = Image::from_bytes(std::fs::read("logo.png")?)?.width(Length::from_cm(4.0));
    docx.replace_with_image("{{logo}}", &logo);

    let items = Table::new([
        ["Item", "Quantity"],
        ["Analytical engine", "1"],
        ["Punched cards", "500"],
    ])
    .header(true);
    docx.replace_with_table("{{items}}", &items)?;

    std::fs::write("output.docx", docx.to_bytes())?;
    Ok(())
}
```

## License

Licensed under either of

- Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
