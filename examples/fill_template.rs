//! Fill a .docx template with sample values.
//!
//! Run with: `cargo run --example fill_template -- <template.docx> <output.docx>`
//!
//! Replaces `{{customer}}`, `{{date}}` and `{{address}}` with text,
//! `{{logo}}` with a small built-in picture and a paragraph holding only
//! `{{items}}` with a table, then prints how many placeholders each step
//! replaced. Placeholders the template doesn't have are skipped.

use std::{
    env,
    error::Error,
    fs::File,
    io::{BufWriter, Write},
    process,
};

use ciclostile::{Docx, Image, Length, Table};

/// The built-in picture: the 3x2 PNG from the crate's test data.
const LOGO: &[u8] = include_bytes!("../src/testdata/3x2.png");

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    let [template, output] = args.as_slice() else {
        eprintln!("usage: fill_template <template.docx> <output.docx>");
        process::exit(1);
    };

    let mut docx = Docx::from_bytes(std::fs::read(template)?)?;

    let replaced = docx.replace_text([
        ("{{customer}}", "Ada Lovelace"),
        ("{{date}}", "3 November 2026"),
        ("{{address}}", "12 St James's Square\nLondon"),
    ]);
    println!("text: {replaced} replaced");

    // Each of the 3x2 pixels becomes a 1 cm square.
    let logo = Image::from_bytes(LOGO)?
        .width(Length::from_cm(3.0))
        .alt_text("Sample logo");
    let replaced = docx.replace_with_image("{{logo}}", &logo);
    println!("images: {replaced} replaced");

    let items = Table::new([
        ["Item", "Quantity", "Price"],
        ["Analytical engine", "1", "£17,000"],
        ["Punched cards", "500", "£25"],
    ])
    .header(true)
    .column_widths([
        Length::from_cm(9.0),
        Length::from_cm(3.0),
        Length::from_cm(4.0),
    ]);
    let replaced = docx.replace_with_table("{{items}}", &items)?;
    println!("tables: {replaced} replaced");

    let mut out = BufWriter::new(File::create(output)?);
    docx.write_to(&mut out)?;
    out.flush()?;
    println!("wrote {output}");
    Ok(())
}
