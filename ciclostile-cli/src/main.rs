//! `ciclostile`: fill .docx templates from the command line.
//!
//! Made to be driven by scripts and agents: every edit can come from a JSON
//! spec, from flags, or from both, and each run prints a JSON report of how
//! many times each placeholder was replaced.

mod spec;

use std::{
    collections::HashSet,
    ffi::OsString,
    fs,
    io::{self, Read as _, Write as _},
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context as _, Result, bail};
use ciclostile::{Docx, Image, Table};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use self::spec::{Spec, TextValue};

const FILL_HELP: &str = r#"SPEC FORMAT (--spec):
  {
    "template": "template.docx",
    "output": "filled.docx",
    "text": {
      "{{customer}}": "Ada Lovelace",
      "{{address}}": "12 St James's Square\nLondon",
      "{{quantity}}": 3,
      "{{notes}}": {"file": "notes.txt"}
    },
    "images": [
      {"placeholder": "{{logo}}", "path": "logo.png", "width": "4cm", "alt_text": "Logo"}
    ],
    "tables": [
      {
        "placeholder": "{{items}}",
        "rows": [["Item", "Qty"], ["Analytical engine", 1]],
        "header": true,
        "style": "TableGrid",
        "column_widths": ["9cm", "3cm"]
      }
    ],
    "update_fields": false,
    "strict": false
  }

  Every field is optional. Unknown fields are an error, and so is a
  placeholder given twice: one replacement replaces every occurrence.
  Relative paths are relative to the current directory. Lengths take a
  unit: cm, mm, in, pt, px or emu. Flags are added to the spec: a --text
  flag overrides the same placeholder in the spec, images and tables are
  appended.

ORDER:
  Text is replaced first, in one pass (inserted text is never searched
  again), then images in order, then tables in order. A text value can't
  hold an image or table placeholder, which that later step would replace.
  A table placeholder must be the only content of its paragraph.

REPORT:
  A JSON object on standard output (standard error when the document goes
  to standard output): the output path, whether it was written, a
  {placeholder, replaced} entry for every text, image and table
  replacement in the order given, and "missing", the placeholders to
  replace that were found nowhere.

EXIT CODES:
  0 success; 1 an error, with nothing written and no report; 2 invalid
  arguments; 3 with --strict, placeholders missing, with nothing written
  and the report printed.

EXAMPLES:
  ciclostile fill template.docx -o out.docx --text '{{name}}=Ada' \
    --image '{{logo}}=logo.png,width=4cm,alt=Company logo' \
    --table '{{items}}=[["Item","Qty"],["Cards",500]]'
  ciclostile fill --spec spec.json
  echo '{"text": {"{{name}}": "Ada"}}' | ciclostile fill t.docx -o out.docx --spec -
  ciclostile fill t.docx --dry-run --strict --text '{{name}}=Ada'"#;

/// The exit code of a `--strict` run with placeholders missing. clap exits
/// with 2 on invalid arguments.
const EXIT_INCOMPLETE: u8 = 3;

#[derive(Debug, Parser)]
#[command(name = "ciclostile", version, about, propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Fill a .docx template: replace placeholders with text, images and
    /// tables, keeping everything else untouched.
    #[command(after_long_help = FILL_HELP)]
    Fill(FillArgs),
}

#[derive(Debug, Args)]
struct FillArgs {
    /// The template to fill, or `-` for standard input. Overrides the
    /// spec's "template".
    template: Option<PathBuf>,

    /// Where to write the result, or `-` for standard output. Overrides
    /// the spec's "output".
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,

    /// A JSON spec: a file path, `-` for standard input, or the JSON
    /// itself when it starts with `{`. See `ciclostile fill --help`.
    #[arg(short, long, value_name = "PATH|-|JSON")]
    spec: Option<String>,

    /// Replace PLACEHOLDER with TEXT. A newline in TEXT becomes a line break
    /// (in bash, `$'one\ntwo'`); a literal backslash-n stays as it is.
    #[arg(short, long, value_name = "PLACEHOLDER=TEXT")]
    text: Vec<String>,

    /// Replace PLACEHOLDER with the content of a UTF-8 text file.
    #[arg(long, value_name = "PLACEHOLDER=PATH")]
    text_file: Vec<String>,

    /// Replace PLACEHOLDER with a PNG or JPEG image. Options: width=LEN,
    /// height=LEN (the other follows the aspect ratio), alt=TEXT.
    #[arg(
        short,
        long,
        value_name = "PLACEHOLDER=PATH[,width=LEN][,height=LEN][,alt=TEXT]"
    )]
    image: Vec<String>,

    /// Replace the paragraph holding only PLACEHOLDER with a table. JSON is
    /// an array of rows, or an object with `"rows"` and the optional
    /// `"header"`, `"style"` and `"column_widths"`; `@FILE` reads it from a file.
    #[arg(long, value_name = "PLACEHOLDER=JSON|@FILE")]
    table: Vec<String>,

    /// Make Word offer to update fields (tables of contents, page
    /// references) when it opens the result.
    #[arg(long)]
    update_fields: bool,

    /// Fail with exit code 3, writing nothing, when a placeholder to
    /// replace isn't found.
    #[arg(long)]
    strict: bool,

    /// Apply the edits and print the report, but write nothing. No output
    /// path is needed.
    #[arg(long)]
    dry_run: bool,
}

/// The JSON report printed after a `fill` run.
#[derive(Debug, Serialize)]
struct Report {
    /// Where the result goes, if anywhere.
    output: Option<PathBuf>,
    /// Whether the result was written.
    written: bool,
    /// The text replacements, in the order given.
    text: Vec<Replaced>,
    /// The image replacements, in the order given.
    images: Vec<Replaced>,
    /// The table replacements, in the order given.
    tables: Vec<Replaced>,
    /// The placeholders to replace that were found nowhere.
    missing: Vec<String>,
}

/// What one replacement did, for the report.
#[derive(Debug, Serialize)]
struct Replaced {
    /// The placeholder replaced.
    placeholder: String,
    /// How many occurrences were replaced.
    replaced: usize,
}

/// Raised by `--strict` when placeholders are missing: the report is
/// still printed, and the exit code tells it apart from other failures.
#[derive(Debug, thiserror::Error)]
#[error("placeholders not found: {}", missing.join(", "))]
struct Incomplete {
    /// The placeholders to replace that were found nowhere.
    missing: Vec<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Fill(args) => fill(&args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            if err.is::<Incomplete>() {
                ExitCode::from(EXIT_INCOMPLETE)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

fn fill(args: &FillArgs) -> Result<()> {
    let mut spec = load_spec(args.spec.as_deref())?;
    merge_flags(&mut spec, args)?;
    spec.check_unique()?;
    let stdin = Path::new("-");

    let template = spec
        .template
        .clone()
        .context("no template: pass it as an argument or set \"template\" in the spec")?;
    if template == stdin && args.spec.as_deref() == Some("-") {
        bail!("the spec and the template can't both come from standard input");
    }
    let output = spec.output.clone();
    if output.is_none() && !args.dry_run {
        bail!("no output: pass -o PATH, set \"output\" in the spec, or use --dry-run");
    }

    let mut docx = open(&template)?;
    let mut report = apply(&mut docx, &spec)?;
    let to_stdout = output.as_deref() == Some(stdin);
    let incomplete = (spec.strict && !report.missing.is_empty()).then(|| Incomplete {
        missing: report.missing.clone(),
    });

    if incomplete.is_none()
        && !args.dry_run
        && let Some(output) = &output
    {
        let bytes = docx.to_bytes();
        if to_stdout {
            let mut stdout = io::stdout().lock();
            stdout.write_all(&bytes)?;
            stdout.flush()?;
        } else {
            write_atomically(output, &bytes)?;
        }
        report.written = true;
    }
    report.output = output;

    let json = serde_json::to_string_pretty(&report)?;
    if to_stdout {
        eprintln!("{json}");
    } else {
        println!("{json}");
    }
    match incomplete {
        Some(incomplete) => Err(incomplete.into()),
        None => Ok(()),
    }
}

/// Read and open the .docx file at `path`, or on standard input for `-`.
fn open(path: &Path) -> Result<Docx> {
    let bytes = if path == Path::new("-") {
        read_stdin()?
    } else {
        fs::read(path).with_context(|| format!("reading {}", path.display()))?
    };
    Docx::from_bytes(bytes).with_context(|| format!("opening {}", path.display()))
}

/// Write `bytes` to `path` through a temporary file next to it, so that
/// `path` never holds part of the document.
///
/// The temporary file is created new, never opened if a file or a symlink
/// already has its name, so a file planted in the directory can't redirect
/// the write. It takes the permissions of the file it replaces, if any.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .with_context(|| format!("{} isn't a file path", path.display()))?;
    let (temporary, mut file) = create_temporary(path, name)
        .with_context(|| format!("creating a temporary file next to {}", path.display()))?;
    let written = (|| {
        if let Ok(existing) = fs::metadata(path) {
            file.set_permissions(existing.permissions())?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if written.is_err() {
        // Best effort: the error that matters is the one returned.
        let _ = fs::remove_file(&temporary);
    }
    written.with_context(|| format!("writing {}", path.display()))
}

/// Create a new, empty file named after `name` next to `path`, trying
/// other names while one is taken.
fn create_temporary(path: &Path, name: &std::ffi::OsStr) -> io::Result<(PathBuf, fs::File)> {
    let mut attempt = 0_u32;
    loop {
        let mut temporary_name = OsString::from(".");
        temporary_name.push(name);
        temporary_name.push(format!(".{}.{attempt}.tmp", std::process::id()));
        let temporary = path.with_file_name(temporary_name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists && attempt < 100 => {
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Read the spec given to `--spec`, or start from an empty one.
fn load_spec(arg: Option<&str>) -> Result<Spec> {
    let Some(arg) = arg else {
        return Ok(Spec::default());
    };
    let (json, source) = if arg.trim_start().starts_with('{') {
        (arg.to_owned(), "inline spec".to_owned())
    } else if arg == "-" {
        let bytes = read_stdin()?;
        let json = String::from_utf8(bytes).context("the spec on standard input isn't UTF-8")?;
        (json, "spec on standard input".to_owned())
    } else {
        let json = fs::read_to_string(arg).with_context(|| format!("reading spec {arg}"))?;
        (json, format!("spec {arg}"))
    };
    serde_json::from_str(&json).with_context(|| format!("invalid {source}"))
}

/// Add the edits given as flags to `spec`.
fn merge_flags(spec: &mut Spec, args: &FillArgs) -> Result<()> {
    if let Some(template) = &args.template {
        spec.template = Some(template.clone());
    }
    if let Some(output) = &args.output {
        spec.output = Some(output.clone());
    }
    // A flag overrides the spec's value, but not another flag.
    let mut flagged = HashSet::new();
    let texts = args.text.iter().map(|arg| ("--text", arg));
    let files = args.text_file.iter().map(|arg| ("--text-file", arg));
    for (flag, arg) in texts.chain(files) {
        let inline = flag == "--text";
        let (key, value) = spec::split_pair(flag, if inline { "TEXT" } else { "PATH" }, arg)?;
        if !flagged.insert(key) {
            bail!("{key} is given more than once in --text and --text-file");
        }
        let value = if inline {
            TextValue::Inline(value.to_owned())
        } else {
            TextValue::File(value.into())
        };
        spec.text.set(key, value);
    }
    for arg in &args.image {
        spec.images.push(spec::parse_image_flag(arg)?);
    }
    for arg in &args.table {
        spec.tables.push(spec::parse_table_flag(arg)?);
    }
    spec.update_fields |= args.update_fields;
    spec.strict |= args.strict;
    Ok(())
}

/// Apply every edit of `spec` to `docx`, and report what each did.
///
/// Inputs are all read and checked before the document is touched, so a
/// missing image file fails the run without half an edit.
fn apply(docx: &mut Docx, spec: &Spec) -> Result<Report> {
    let text = read_text(spec)?;
    let images = read_images(spec)?;
    let tables = make_tables(spec);

    let text_report: Vec<Replaced> = docx
        .replace_text(text.iter().map(|(key, value)| (*key, value)))
        .into_iter()
        .map(|(key, replaced)| Replaced {
            placeholder: key.to_owned(),
            replaced,
        })
        .collect();

    let images_report: Vec<Replaced> = images
        .iter()
        .map(|(placeholder, image)| Replaced {
            placeholder: (*placeholder).to_owned(),
            replaced: docx.replace_with_image(placeholder, image),
        })
        .collect();

    let mut tables_report = Vec::with_capacity(tables.len());
    for (placeholder, table) in &tables {
        let replaced = docx
            .replace_with_table(placeholder, table)
            .with_context(|| format!("replacing {placeholder} with a table"))?;
        tables_report.push(Replaced {
            placeholder: (*placeholder).to_owned(),
            replaced,
        });
    }

    if spec.update_fields {
        docx.update_fields_on_open();
    }

    let missing = [&text_report, &images_report, &tables_report]
        .into_iter()
        .flatten()
        .filter(|entry| entry.replaced == 0)
        .map(|entry| entry.placeholder.clone())
        .collect();
    Ok(Report {
        output: None,
        written: false,
        text: text_report,
        images: images_report,
        tables: tables_report,
        missing,
    })
}

/// Read the text values of `spec`, and check that none holds an image or
/// table placeholder: those are replaced after the text, so they would be
/// replaced in the value too.
fn read_text(spec: &Spec) -> Result<Vec<(&str, String)>> {
    let mut text = Vec::with_capacity(spec.text.0.len());
    for (key, value) in &spec.text.0 {
        let value = match value {
            TextValue::Inline(value) => value.clone(),
            TextValue::File(file) => fs::read_to_string(file)
                .with_context(|| format!("reading text for {key} from {}", file.display()))?,
        };
        text.push((key.as_str(), value));
    }
    let later = spec
        .images
        .iter()
        .map(|image| image.placeholder.as_str())
        .chain(spec.tables.iter().map(|table| table.placeholder.as_str()));
    for placeholder in later {
        if let Some((key, _)) = text.iter().find(|(_, value)| value.contains(placeholder)) {
            bail!(
                "the text for {key} holds {placeholder}, which an image or a table replaces \
                 after the text"
            );
        }
    }
    Ok(text)
}

/// Read and size the images of `spec`.
fn read_images(spec: &Spec) -> Result<Vec<(&str, Image)>> {
    spec.images
        .iter()
        .map(|spec| {
            let data = fs::read(&spec.path)
                .with_context(|| format!("reading image {}", spec.path.display()))?;
            let mut image = Image::from_bytes(data)
                .with_context(|| format!("reading image {}", spec.path.display()))?;
            if let Some(width) = spec.width {
                image = image.width(width.0);
            }
            if let Some(height) = spec.height {
                image = image.height(height.0);
            }
            if let Some(alt_text) = &spec.alt_text {
                image = image.alt_text(alt_text.as_str());
            }
            Ok((spec.placeholder.as_str(), image))
        })
        .collect()
}

/// Make the tables of `spec`.
fn make_tables(spec: &Spec) -> Vec<(&str, Table)> {
    spec.tables
        .iter()
        .map(|spec| {
            let rows = spec
                .rows
                .iter()
                .map(|row| row.iter().map(|cell| cell.0.as_str()));
            let mut table = Table::new(rows)
                .header(spec.header)
                .column_widths(spec.column_widths.iter().map(|width| width.0));
            if let Some(style) = &spec.style {
                table = table.style(style.as_str());
            }
            (spec.placeholder.as_str(), table)
        })
        .collect()
}

/// Read all of standard input.
fn read_stdin() -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    io::stdin()
        .lock()
        .read_to_end(&mut bytes)
        .context("reading standard input")?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory for one test.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ciclostile-cli-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn writing_never_follows_a_planted_symlink() {
        let dir = scratch("symlink");
        let victim = dir.join("victim");
        fs::write(&victim, "keep me").unwrap();
        // Where the first temporary file for `out.docx` would go.
        let planted = dir.join(format!(".out.docx.{}.0.tmp", std::process::id()));
        std::os::unix::fs::symlink(&victim, &planted).unwrap();

        let output = dir.join("out.docx");
        write_atomically(&output, b"document").unwrap();
        assert_eq!(fs::read(&output).unwrap(), b"document");
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep me");
        // The planted link is left alone, and no temporary file remains.
        assert!(fs::symlink_metadata(&planted).unwrap().is_symlink());
        let mut names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 3, "{names:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn writing_keeps_the_permissions_of_the_file_it_replaces() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = scratch("permissions");
        let output = dir.join("out.docx");
        fs::write(&output, "old").unwrap();
        fs::set_permissions(&output, fs::Permissions::from_mode(0o600)).unwrap();
        write_atomically(&output, b"new").unwrap();
        assert_eq!(fs::read(&output).unwrap(), b"new");
        let mode = fs::metadata(&output).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }
}
