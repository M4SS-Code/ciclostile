//! Guarantees of the public API, checked at compile time: a template can be
//! opened once and cloned for each output, possibly on other threads, and
//! every error can travel as a `Box<dyn Error + Send + Sync>`.

use std::{error::Error, fmt::Debug};

use ciclostile::{
    Docx, Image, Length, Table,
    error::{InvalidDocx, InvalidImage, InvalidPlaceholder, InvalidXml, InvalidZip},
};

const _: () = {
    const fn assert_value<T: Clone + Debug + Send + Sync>() {}
    const fn assert_error<T: Error + Send + Sync + 'static>() {}

    assert_value::<Docx>();
    assert_value::<Image>();
    assert_value::<Table>();
    assert_value::<Length>();

    assert_error::<InvalidDocx>();
    assert_error::<InvalidImage>();
    assert_error::<InvalidPlaceholder>();
    assert_error::<InvalidZip>();
    assert_error::<InvalidXml>();
};
