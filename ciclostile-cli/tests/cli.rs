//! `ciclostile fill` end to end, on the library's sample template and on
//! the invoice example.

#[path = "../../tests/common/mod.rs"]
pub mod common;

use std::{
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use common::Fixture;
use serde_json::{Value, json};

/// The exit code of a `--strict` run with placeholders missing.
const INCOMPLETE: i32 = 3;

/// A fresh directory holding the sample template as `template.docx` and a
/// 3x2 PNG as `logo.png`. The process id keeps two test runs sharing a
/// target directory apart.
fn workdir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("cli-{}", std::process::id()))
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("template.docx"), Fixture::template().zip()).unwrap();
    fs::write(dir.join("logo.png"), common::PNG).unwrap();
    dir
}

/// Run `ciclostile` in `dir` with `args`, feeding it `stdin`, which it may
/// exit without reading.
fn run(dir: &Path, args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ciclostile"))
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Err(err) = child.stdin.take().unwrap().write_all(stdin) {
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe, "{err}");
    }
    child.wait_with_output().unwrap()
}

/// Run `ciclostile fill template.docx` in `dir` with `args` after it.
fn fill(dir: &Path, args: &[&str]) -> Output {
    let args: Vec<&str> = ["fill", "template.docx"]
        .into_iter()
        .chain(args.iter().copied())
        .collect();
    run(dir, &args, b"")
}

/// Parse the JSON `output` printed on standard output.
fn report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "{err}: stdout {:?}, stderr {:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Return what `output` printed on standard error.
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Return the `replaced` count of `placeholder` in the report section
/// `kind`.
fn replaced(report: &Value, kind: &str, placeholder: &str) -> u64 {
    report[kind]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["placeholder"] == placeholder)
        .unwrap_or_else(|| panic!("no {kind} entry for {placeholder}: {report:#}"))["replaced"]
        .as_u64()
        .unwrap()
}

/// Return the main part of the .docx file at `path`.
fn document(path: &Path) -> String {
    common::read_text(&fs::read(path).unwrap(), "word/document.xml")
}

// ── fill ─────────────────────────────────────────────────────────────────────

#[test]
fn flags_fill_text_images_and_tables() {
    let dir = workdir("flags");
    let output = fill(
        &dir,
        &[
            "-o",
            "out.docx",
            "--text",
            "{{customer}}=Ada Lovelace",
            "--text",
            "{{date}}=3 November 2026",
            "--image",
            "{{logo}}=logo.png,width=3cm,alt=Logo, small",
            "--image",
            "{{signature}}=logo.png,height=1cm",
            "--table",
            r#"{{items}}={"rows": [["Item", "Qty"], ["Cards", 500]], "header": true,
                          "style": "CliStyle", "column_widths": ["2cm", "1cm"]}"#,
            "--update-fields",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let report = report(&output);
    assert_eq!(report["written"], true);
    assert_eq!(report["output"], "out.docx");
    assert_eq!(replaced(&report, "text", "{{customer}}"), 1);
    assert_eq!(replaced(&report, "text", "{{date}}"), 1);
    assert_eq!(replaced(&report, "images", "{{logo}}"), 2);
    assert_eq!(replaced(&report, "images", "{{signature}}"), 1);
    assert_eq!(replaced(&report, "tables", "{{items}}"), 1);
    assert_eq!(report["missing"], json!([]));

    let zip = fs::read(dir.join("out.docx")).unwrap();
    let main = common::read_text(&zip, "word/document.xml");
    assert!(main.contains("Ada Lovelace"));
    // The 3x2 logo 3 cm wide, 2 cm high.
    assert!(
        main.contains(r#"<wp:extent cx="1080000" cy="720000"/>"#),
        "{main}"
    );
    assert!(main.contains(r#"descr="Logo, small""#), "{main}");
    assert!(main.contains("<w:tblHeader/>") && main.contains(">500<"));
    assert!(main.contains(r#"<w:tblStyle w:val="CliStyle"/>"#), "{main}");
    assert!(
        main.contains(r#"<w:gridCol w:w="1134"/><w:gridCol w:w="567"/>"#),
        "{main}"
    );
    // The signature 1 cm high, 1.5 cm wide.
    let footer = common::read_text(&zip, "word/footer1.xml");
    assert!(
        footer.contains(r#"<wp:extent cx="540000" cy="360000"/>"#),
        "{footer}"
    );
    assert!(common::read_text(&zip, "word/settings.xml").contains("w:updateFields"));
}

#[test]
fn fields_are_left_alone_unless_asked() {
    let dir = workdir("no-update-fields");
    let output = fill(&dir, &["-o", "out.docx", "--text", "{{customer}}=Ada"]);
    assert!(output.status.success(), "{output:?}");
    let zip = fs::read(dir.join("out.docx")).unwrap();
    assert!(!common::read_text(&zip, "word/settings.xml").contains("w:updateFields"));
}

#[test]
fn text_comes_from_files_too() {
    let dir = workdir("text-file");
    fs::write(dir.join("phone.txt"), "+44 20 7946 0000").unwrap();
    let output = fill(
        &dir,
        &["-o", "out.docx", "--text-file", "{{phone}}=phone.txt"],
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(replaced(&report(&output), "text", "{{phone}}"), 1);
    assert!(document(&dir.join("out.docx")).contains("+44 20 7946 0000"));

    let output = fill(&dir, &["--dry-run", "--text-file", "{{phone}}=nowhere.txt"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        stderr(&output).contains("nowhere.txt"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn tables_come_from_files_too() {
    let dir = workdir("table-file");
    fs::write(dir.join("rows.json"), r#"[["From", "file"]]"#).unwrap();
    let output = fill(&dir, &["-o", "out.docx", "--table", "{{items}}=@rows.json"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(replaced(&report(&output), "tables", "{{items}}"), 1);
    assert!(document(&dir.join("out.docx")).contains(">file<"));
}

#[test]
fn spec_from_stdin_combines_with_flags() {
    let dir = workdir("spec");
    fs::write(dir.join("phone.txt"), "+44 20 7946 0000").unwrap();
    let spec = r#"{
        "template": "elsewhere.docx",
        "output": "elsewhere-out.docx",
        "text": {"{{site}}": "example.com", "{{customer}}": "From spec",
                 "{{phone}}": {"file": "phone.txt"}, "{{total}}": 17000},
        "update_fields": true
    }"#;
    let output = run(
        &dir,
        &[
            "fill",
            "template.docx",
            "--spec",
            "-",
            "-o",
            "out.docx",
            "--text",
            "{{customer}}=From flag",
        ],
        spec.as_bytes(),
    );
    assert!(output.status.success(), "{output:?}");
    let report = report(&output);
    assert_eq!(report["output"], "out.docx");
    // The text entries in the order the spec gives them, a flag in place
    // of the spec's value.
    let order: Vec<&str> = report["text"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["placeholder"].as_str().unwrap())
        .collect();
    assert_eq!(
        order,
        ["{{site}}", "{{customer}}", "{{phone}}", "{{total}}"]
    );

    let zip = fs::read(dir.join("out.docx")).unwrap();
    let main = common::read_text(&zip, "word/document.xml");
    assert!(main.contains("From flag") && !main.contains("From spec"));
    assert!(main.contains("+44 20 7946 0000") && main.contains("17000"));
    // A flag only adds to the spec's "update_fields".
    assert!(common::read_text(&zip, "word/settings.xml").contains("w:updateFields"));
    assert!(!dir.join("elsewhere-out.docx").exists());
}

#[test]
fn text_is_replaced_in_one_pass() {
    let dir = workdir("one-pass");
    let output = fill(
        &dir,
        &[
            "-o",
            "out.docx",
            "--text",
            "{{customer}}={{date}}",
            "--text",
            "{{date}}=3 November",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let report = report(&output);
    assert_eq!(replaced(&report, "text", "{{customer}}"), 1);
    // The template's own `{{date}}`, not the one inserted for the customer.
    assert_eq!(replaced(&report, "text", "{{date}}"), 1);
    let paragraphs = common::paragraphs(&document(&dir.join("out.docx")));
    assert!(
        paragraphs.iter().any(|text| text == "Dear {{date}},"),
        "{paragraphs:#?}"
    );
}

#[test]
fn counts_come_from_the_single_pass() {
    // `Invoice` is found, but only inside the occurrence of the longer key,
    // which wins: so it replaced nothing, and is missing.
    let dir = workdir("counts");
    let output = fill(
        &dir,
        &[
            "--dry-run",
            "--strict",
            "--spec",
            r#"{"text": {"Invoice": "Fattura", "Invoice {{number}}": "X"}}"#,
        ],
    );
    assert_eq!(output.status.code(), Some(INCOMPLETE), "{output:?}");
    let report = report(&output);
    assert_eq!(replaced(&report, "text", "Invoice"), 0);
    assert_eq!(replaced(&report, "text", "Invoice {{number}}"), 1);
    assert_eq!(report["missing"], json!(["Invoice"]));
}

#[test]
fn strict_fails_on_missing_placeholders() {
    let dir = workdir("strict-missing");
    for (kind, args) in [
        ("text", ["--text", "{{nowhere}}=x"]),
        ("images", ["--image", "{{nowhere}}=logo.png"]),
        ("tables", ["--table", "{{nowhere}}=[[1]]"]),
    ] {
        // Without strict, the run succeeds and reports it.
        let mut lenient = vec!["--dry-run"];
        lenient.extend(args);
        let output = fill(&dir, &lenient);
        assert!(output.status.success(), "{output:?}");
        let json = report(&output);
        assert_eq!(replaced(&json, kind, "{{nowhere}}"), 0);
        assert_eq!(json["missing"], json!(["{{nowhere}}"]));

        // With it, nothing is written.
        let mut strict = vec!["-o", "out.docx", "--strict"];
        strict.extend(args);
        let output = fill(&dir, &strict);
        assert_eq!(output.status.code(), Some(INCOMPLETE), "{output:?}");
        let json = report(&output);
        assert_eq!(json["written"], false);
        assert_eq!(json["missing"], json!(["{{nowhere}}"]));
        assert!(
            stderr(&output).contains("not found: {{nowhere}}"),
            "{}",
            stderr(&output)
        );
        assert!(!dir.join("out.docx").exists());
    }

    // A spec's "strict" counts as much as the flag.
    let output = fill(
        &dir,
        &[
            "--dry-run",
            "--spec",
            r#"{"strict": true}"#,
            "--text",
            "{{nowhere}}=x",
        ],
    );
    assert_eq!(output.status.code(), Some(INCOMPLETE), "{output:?}");
}

#[test]
fn dry_run_writes_nothing() {
    let dir = workdir("dry-run");
    for args in [&["--dry-run"][..], &["--dry-run", "-o", "out.docx"]] {
        let mut args = args.to_vec();
        args.extend(["--text", "{{customer}}=Ada"]);
        let output = fill(&dir, &args);
        assert!(output.status.success(), "{output:?}");
        let json = report(&output);
        assert_eq!(json["written"], false);
        assert_eq!(replaced(&json, "text", "{{customer}}"), 1);
        assert!(!dir.join("out.docx").exists());
    }
}

#[test]
fn output_replaces_the_old_file_whole() {
    let dir = workdir("overwrite");
    fs::write(
        dir.join("out.docx"),
        b"an old, longer file that isn't a document at all",
    )
    .unwrap();
    let output = fill(&dir, &["-o", "out.docx", "--text", "{{customer}}=Ada"]);
    assert!(output.status.success(), "{output:?}");
    assert!(document(&dir.join("out.docx")).contains("Ada"));
    // No temporary file is left next to it.
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["logo.png", "out.docx", "template.docx"]);
}

#[test]
fn output_to_stdout_moves_the_report_to_stderr() {
    let dir = workdir("stdout");
    let template = fs::read(dir.join("template.docx")).unwrap();
    let output = run(
        &dir,
        &["fill", "-", "-o", "-", "--text", "{{customer}}=Ada"],
        &template,
    );
    assert!(output.status.success(), "{output:?}");
    let main = common::read_text(&output.stdout, "word/document.xml");
    assert!(main.contains("Ada"));
    let report: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(report["written"], true);
}

#[test]
fn invalid_specs_and_flags_fail_and_name_the_problem() {
    let dir = workdir("errors");
    fs::write(dir.join("not-an-image.png"), b"plain text").unwrap();
    let cases: &[(&[&str], &str)] = &[
        (
            &["--table", "{{customer}}=[[1]]"],
            "must be the only content of its paragraph",
        ),
        (&["--image", "{{logo}}=missing.png"], "missing.png"),
        (
            &["--image", "{{logo}}=not-an-image.png"],
            "not-an-image.png",
        ),
        (&["--image", "{{logo}}=logo.png,width=3"], "has no unit"),
        (&["--image", "{{logo}}="], "has no file path"),
        (&["--text", "=x"], "--text expects PLACEHOLDER=TEXT"),
        (
            &["--text-file", "{{a}}"],
            "--text-file expects PLACEHOLDER=PATH",
        ),
        (
            &["--table", r#"{{items}}={"placeholder": "x", "rows": []}"#],
            "goes before '='",
        ),
        (
            &["--table", r#"{{items}}=[[["nested"]]]"#],
            "a table cell must be",
        ),
        (&["--spec", r#"{"txt": {}}"#], "unknown field"),
        (
            &[
                "--spec",
                r#"{"images": [{"placeholder": "a", "path": "b", "size": 1}]}"#,
            ],
            "unknown field",
        ),
        (
            &[
                "--spec",
                r#"{"text": {"{{a}}": {"file": "x", "encoding": "latin1"}}}"#,
            ],
            "unknown field \"encoding\"",
        ),
        (
            &["--spec", r#"{"text": {"{{a}}": null}}"#],
            "text: {{a}} is null",
        ),
        (
            &["--spec", r#"{"text": {"{{a}}": "1", "{{a}}": "2"}}"#],
            "{{a}} is given more than once",
        ),
        (
            &["--text", "{{a}}=1", "--text", "{{a}}=2"],
            "{{a}} is given more than once",
        ),
        (
            &[
                "--image",
                "{{logo}}=logo.png",
                "--image",
                "{{logo}}=logo.png",
            ],
            "{{logo}} is given more than once",
        ),
        (
            &["--text", "{{logo}}=x", "--image", "{{logo}}=logo.png"],
            "{{logo}} is given more than once",
        ),
        (
            &[
                "--text",
                "{{customer}}=Ada {{items}}",
                "--table",
                "{{items}}=[[1]]",
            ],
            "the text for {{customer}} holds {{items}}",
        ),
    ];
    for (args, needle) in cases {
        let mut args = args.to_vec();
        args.extend(["-o", "out.docx"]);
        let output = fill(&dir, &args);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
        assert!(
            stderr(&output).contains(needle),
            "{args:?}: {}",
            stderr(&output)
        );
        assert!(output.stdout.is_empty(), "{args:?}: no report on errors");
    }
    assert!(!dir.join("out.docx").exists());
}

#[test]
fn invalid_invocations_fail_and_name_the_problem() {
    let dir = workdir("invocations");
    for (args, needle) in [
        (&["fill", "template.docx"][..], "no output"),
        (&["fill", "-o", "out.docx"], "no template"),
        (
            &["fill", "-", "--spec", "-", "-o", "out.docx"],
            "both come from standard input",
        ),
    ] {
        let output = run(&dir, args, b"{}");
        assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
        assert!(
            stderr(&output).contains(needle),
            "{args:?}: {}",
            stderr(&output)
        );
    }

    // Invalid arguments exit with clap's 2, not the 3 of `--strict`.
    let output = fill(&dir, &["--bogus"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn inline_specs_may_start_with_whitespace() {
    let dir = workdir("inline");
    let output = fill(
        &dir,
        &[
            "--dry-run",
            "--spec",
            r#"  {"text": {"{{customer}}": "Ada"}}"#,
        ],
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(replaced(&report(&output), "text", "{{customer}}"), 1);
}

#[test]
fn invoice_example_fills_every_placeholder() {
    // A copy, so that a run that writes when it shouldn't can't leave files
    // in the source tree.
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/invoice");
    let dir = workdir("invoice");
    for name in ["spec.json", "template.docx", "logo.png"] {
        fs::copy(example.join(name), dir.join(name)).unwrap();
    }
    // The spec sets "strict", so a placeholder it misses fails the run.
    let output = run(&dir, &["fill", "--spec", "spec.json", "--dry-run"], b"");
    assert!(output.status.success(), "{output:?}");
    let report = report(&output);
    assert_eq!(report["written"], false);
    assert_eq!(report["missing"], json!([]));
    assert_eq!(replaced(&report, "tables", "{{items}}"), 1);
}
