//! Filling clones of a template: each clone is independent of the template
//! and of the other clones, also across threads.

pub mod common;

use std::thread;

use ciclostile::{Docx, Image};
use common::{Fixture, JPEG, PNG};

/// Return the paragraph texts of the main part of the archive `zip`.
fn body(zip: &[u8]) -> Vec<String> {
    common::paragraphs(&common::read_text(zip, "word/document.xml"))
}

#[test]
fn clones_are_filled_independently() {
    let template = Docx::from_bytes(Fixture::template().zip()).unwrap();
    let pristine = template.to_bytes();

    let mut ada = template.clone();
    ada.replace_text([("{{customer}}", "Ada")]);
    ada.replace_with_image("{{logo}}", &Image::from_bytes(PNG).unwrap());
    let mut grace = template.clone();
    grace.replace_text([("{{customer}}", "Grace")]);
    grace.replace_with_image("{{logo}}", &Image::from_bytes(JPEG).unwrap());
    let (ada, grace) = (ada.to_bytes(), grace.to_bytes());

    // The template is untouched...
    assert_eq!(template.to_bytes(), pristine);
    // ... and each clone only has its own text, media and drawing ids.
    assert!(body(&ada).contains(&"Dear Ada,".to_owned()));
    assert!(body(&grace).contains(&"Dear Grace,".to_owned()));
    let names = |zip: &[u8]| common::names(zip).split_off(common::names(&pristine).len());
    assert_eq!(
        names(&ada),
        ["word/_rels/header1.xml.rels", "word/media/image2.png"]
    );
    assert_eq!(
        names(&grace),
        ["word/_rels/header1.xml.rels", "word/media/image1.jpeg"]
    );
    for output in [&ada, &grace] {
        let ids = common::drawing_ids(&common::read_text(output, "word/document.xml"));
        assert_eq!(ids, [3, 2, 1], "the new picture, then the template's");
    }
}

#[test]
fn clone_of_a_filled_document_keeps_its_changes() {
    let mut docx = Docx::from_bytes(Fixture::template().zip()).unwrap();
    docx.replace_text([("{{customer}}", "Ada")]);
    docx.replace_with_image("{{logo}}", &Image::from_bytes(PNG).unwrap());
    let before = docx.to_bytes();

    let mut copy = docx.clone();
    assert_eq!(copy.to_bytes(), before);
    copy.replace_text([("Ada", "Grace")]);
    copy.replace_with_image("{{signature}}", &Image::from_bytes(PNG).unwrap());
    assert_eq!(docx.to_bytes(), before);
    assert!(body(&copy.to_bytes()).contains(&"Dear Grace,".to_owned()));
}

#[test]
fn clones_can_be_filled_on_other_threads() {
    let template = Docx::from_bytes(Fixture::template().zip()).unwrap();
    let names = ["Ada", "Grace", "Hedy", "Katherine"];
    let outputs: Vec<_> = thread::scope(|scope| {
        let handles: Vec<_> = names
            .iter()
            .map(|name| {
                let template = &template;
                scope.spawn(move || {
                    let mut docx = template.clone();
                    docx.replace_text([("{{customer}}", *name)]);
                    docx.to_bytes()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    for (name, output) in names.iter().zip(&outputs) {
        assert!(body(output).contains(&format!("Dear {name},")));
    }
}
