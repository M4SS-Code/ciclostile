//! Shared fixtures for the integration tests: a Word-like template whose
//! parts are written as readable XML, a ZIP builder, and helpers that read
//! documents back with the `zip` crate, independently of ciclostile.
//!
//! Test files declare this module as `pub mod common;` so that helpers a
//! file doesn't use are not reported as dead code.

#![expect(
    clippy::missing_panics_doc,
    reason = "helpers fail the calling test by panicking"
)]

use std::{
    io::{Cursor, Read, Write},
    path::Path,
};

use ciclostile::Docx;
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

// ── Parts ────────────────────────────────────────────────────────────────────

/// The XML declaration Word writes, with its CRLF.
macro_rules! declaration {
    () => {
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n"
    };
}

/// The namespace declarations and `mc:Ignorable` Word 2019 writes on the
/// root element of every story part.
macro_rules! word_namespaces {
    () => {
        concat!(
            r#" xmlns:wpc="http://schemas.microsoft.com/office/word/2010/wordprocessingCanvas""#,
            r#" xmlns:cx="http://schemas.microsoft.com/office/drawing/2014/chartex""#,
            r#" xmlns:cx1="http://schemas.microsoft.com/office/drawing/2015/9/8/chartex""#,
            r#" xmlns:cx2="http://schemas.microsoft.com/office/drawing/2015/10/21/chartex""#,
            r#" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006""#,
            r#" xmlns:aink="http://schemas.microsoft.com/office/drawing/2016/ink""#,
            r#" xmlns:am3d="http://schemas.microsoft.com/office/drawing/2017/model3d""#,
            r#" xmlns:o="urn:schemas-microsoft-com:office:office""#,
            r#" xmlns:oel="http://schemas.microsoft.com/office/2019/extlst""#,
            r#" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#,
            r#" xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math""#,
            r#" xmlns:v="urn:schemas-microsoft-com:vml""#,
            r#" xmlns:wp14="http://schemas.microsoft.com/office/word/2010/wordprocessingDrawing""#,
            r#" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing""#,
            r#" xmlns:w10="urn:schemas-microsoft-com:office:word""#,
            r#" xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#,
            r#" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml""#,
            r#" xmlns:w15="http://schemas.microsoft.com/office/word/2012/wordml""#,
            r#" xmlns:w16cex="http://schemas.microsoft.com/office/word/2018/wordml/cex""#,
            r#" xmlns:w16cid="http://schemas.microsoft.com/office/word/2016/wordml/cid""#,
            r#" xmlns:w16="http://schemas.microsoft.com/office/word/2018/wordml""#,
            r#" xmlns:w16se="http://schemas.microsoft.com/office/word/2015/wordml/symex""#,
            r#" xmlns:wpg="http://schemas.microsoft.com/office/word/2010/wordprocessingGroup""#,
            r#" xmlns:wpi="http://schemas.microsoft.com/office/word/2010/wordprocessingInk""#,
            r#" xmlns:wne="http://schemas.microsoft.com/office/word/2006/wordml""#,
            r#" xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape""#,
            r#" mc:Ignorable="w14 w15 w16se w16cid w16 w16cex wp14""#,
        )
    };
}

pub const CONTENT_TYPES: &str = concat!(
    declaration!(),
    r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">"#,
    r#"<Default Extension="png" ContentType="image/png"/>"#,
    r#"<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>"#,
    r#"<Default Extension="xml" ContentType="application/xml"/>"#,
    r#"<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>"#,
    r#"<Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>"#,
    r#"<Override PartName="/word/settings.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml"/>"#,
    r#"<Override PartName="/word/footnotes.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.footnotes+xml"/>"#,
    r#"<Override PartName="/word/endnotes.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.endnotes+xml"/>"#,
    r#"<Override PartName="/word/header1.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml"/>"#,
    r#"<Override PartName="/word/footer1.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml"/>"#,
    r#"<Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>"#,
    r#"<Override PartName="/docProps/app.xml" ContentType="application/vnd.openxmlformats-officedocument.extended-properties+xml"/>"#,
    "</Types>",
);

pub const PACKAGE_RELS: &str = concat!(
    declaration!(),
    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    r#"<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" Target="docProps/app.xml"/>"#,
    r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/>"#,
    r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>"#,
    "</Relationships>",
);

/// Core properties, with a placeholder that must stay: only story parts
/// are searched.
pub const CORE: &str = concat!(
    declaration!(),
    r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties""#,
    r#" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/""#,
    r#" xmlns:dcmitype="http://purl.org/dc/dcmitype/" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">"#,
    "<dc:title>{{title}}</dc:title><dc:creator>Template author</dc:creator>",
    "<cp:lastModifiedBy>Template author</cp:lastModifiedBy><cp:revision>3</cp:revision>",
    r#"<dcterms:created xsi:type="dcterms:W3CDTF">2026-10-01T09:00:00Z</dcterms:created>"#,
    r#"<dcterms:modified xsi:type="dcterms:W3CDTF">2026-10-02T09:00:00Z</dcterms:modified>"#,
    "</cp:coreProperties>",
);

pub const APP: &str = concat!(
    declaration!(),
    r#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties""#,
    r#" xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes">"#,
    "<Template>Normal.dotm</Template><TotalTime>1</TotalTime><Pages>1</Pages>",
    "<Application>Microsoft Office Word</Application><DocSecurity>0</DocSecurity>",
    "<AppVersion>16.0000</AppVersion></Properties>",
);

/// The main part. Word-2019 style: one line after the declaration, rsid
/// and paraId attributes, placeholders split across runs by spell-check
/// marks and edits. `wp:docPr` ids 1 (text box) and 2 (picture) are taken.
pub const DOCUMENT: &str = concat!(
    declaration!(),
    "<w:document",
    word_namespaces!(),
    "><w:body>",
    // The title placeholder was typed in three goes and flagged by the
    // spelling checker.
    r#"<w:p w14:paraId="1A2B3C4D" w14:textId="77777777" w:rsidR="00B57C6E" w:rsidRDefault="00B57C6E" w:rsidP="00B57C6E">"#,
    r#"<w:pPr><w:pStyle w:val="Title"/></w:pPr>"#,
    r#"<w:r><w:rPr><w:b/></w:rPr><w:t xml:space="preserve">Invoice {{</w:t></w:r>"#,
    r#"<w:proofErr w:type="spellStart"/>"#,
    r#"<w:r w:rsidRPr="00A13F2E"><w:rPr><w:b/></w:rPr><w:t>number</w:t></w:r>"#,
    r#"<w:proofErr w:type="spellEnd"/>"#,
    r#"<w:r><w:rPr><w:b/></w:rPr><w:t>}}</w:t></w:r></w:p>"#,
    // A bookmarked placeholder after a tracked deletion of another copy,
    // and an endnote reference.
    r#"<w:p w14:paraId="2B3C4D5E" w14:textId="77777777" w:rsidR="00B57C6E" w:rsidRDefault="00B57C6E">"#,
    r#"<w:r><w:t xml:space="preserve">Dear </w:t></w:r>"#,
    r#"<w:del w:id="1" w:author="Template author" w:date="2026-10-02T09:00:00Z">"#,
    r#"<w:r w:rsidDel="00A13F2E"><w:delText xml:space="preserve">{{customer}} </w:delText></w:r></w:del>"#,
    r#"<w:bookmarkStart w:id="0" w:name="Customer"/>"#,
    r#"<w:r w:rsidR="00A13F2E"><w:t>{{customer}}</w:t></w:r>"#,
    r#"<w:bookmarkEnd w:id="0"/><w:r><w:t>,</w:t></w:r>"#,
    r#"<w:r><w:rPr><w:rStyle w:val="EndnoteReference"/></w:rPr><w:endnoteReference w:id="1"/></w:r></w:p>"#,
    // A hyperlink, a tracked insertion and a footnote reference.
    r#"<w:p w14:paraId="3C4D5E6F" w14:textId="77777777" w:rsidR="00B57C6E" w:rsidRDefault="00B57C6E">"#,
    r#"<w:r><w:t xml:space="preserve">Visit </w:t></w:r>"#,
    r#"<w:hyperlink r:id="rId5" w:history="1"><w:r><w:rPr><w:rStyle w:val="Hyperlink"/></w:rPr>"#,
    r#"<w:t>{{site}}</w:t></w:r></w:hyperlink><w:r><w:t xml:space="preserve"> or call </w:t></w:r>"#,
    r#"<w:ins w:id="2" w:author="Template author" w:date="2026-10-02T09:00:00Z"><w:r><w:t>{{phone}}</w:t></w:r></w:ins>"#,
    r#"<w:r><w:t>.</w:t></w:r>"#,
    r#"<w:r><w:rPr><w:rStyle w:val="FootnoteReference"/></w:rPr><w:footnoteReference w:id="1"/></w:r></w:p>"#,
    // The image placeholder.
    r#"<w:p w14:paraId="4D5E6F70" w14:textId="77777777" w:rsidR="00B57C6E" w:rsidRDefault="00B57C6E">"#,
    r#"<w:pPr><w:jc w:val="center"/></w:pPr><w:r><w:t>{{logo}}</w:t></w:r></w:p>"#,
    // An existing picture: `wp:docPr` id 2, relationship rId7.
    r#"<w:p w14:paraId="5E6F7081" w14:textId="77777777" w:rsidR="00B57C6E" w:rsidRDefault="00B57C6E">"#,
    r#"<w:r><w:t xml:space="preserve">Signed: </w:t></w:r><w:r><w:rPr><w:noProof/></w:rPr><w:drawing>"#,
    r#"<wp:inline distT="0" distB="0" distL="0" distR="0" wp14:anchorId="0F1E2D3C" wp14:editId="4B5A6978">"#,
    r#"<wp:extent cx="285750" cy="190500"/><wp:effectExtent l="0" t="0" r="0" b="0"/>"#,
    r#"<wp:docPr id="2" name="Picture 2" descr="Signature"/><wp:cNvGraphicFramePr>"#,
    r#"<a:graphicFrameLocks xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" noChangeAspect="1"/>"#,
    r#"</wp:cNvGraphicFramePr><a:graphic xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#,
    r#"<a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture">"#,
    r#"<pic:pic xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture">"#,
    r#"<pic:nvPicPr><pic:cNvPr id="2" name="Picture 2"/><pic:cNvPicPr/></pic:nvPicPr>"#,
    r#"<pic:blipFill><a:blip r:embed="rId7"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>"#,
    r#"<pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="285750" cy="190500"/></a:xfrm>"#,
    r#"<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData>"#,
    r#"</a:graphic></wp:inline></w:drawing></w:r></w:p>"#,
    // The table placeholder, typed in two goes, right before a table.
    r#"<w:p w14:paraId="6F708192" w14:textId="77777777" w:rsidR="00B57C6E" w:rsidRDefault="00B57C6E">"#,
    r#"<w:pPr><w:spacing w:after="120"/></w:pPr><w:r><w:t xml:space="preserve"> {{</w:t></w:r>"#,
    r#"<w:r w:rsidR="00A13F2E"><w:t>items}}</w:t></w:r></w:p>"#,
    // A table with a placeholder in a cell.
    r#"<w:tbl><w:tblPr><w:tblStyle w:val="TableGrid"/><w:tblW w:w="0" w:type="auto"/>"#,
    r#"<w:tblLook w:val="04A0" w:firstRow="1" w:lastRow="0" w:firstColumn="1" w:lastColumn="0" w:noHBand="0" w:noVBand="1"/>"#,
    r#"</w:tblPr><w:tblGrid><w:gridCol w:w="4814"/><w:gridCol w:w="4814"/></w:tblGrid>"#,
    r#"<w:tr w:rsidR="00B57C6E" w14:paraId="708192A3" w14:textId="77777777">"#,
    r#"<w:tc><w:tcPr><w:tcW w:w="4814" w:type="dxa"/></w:tcPr><w:p><w:r><w:t>Total</w:t></w:r></w:p></w:tc>"#,
    r#"<w:tc><w:tcPr><w:tcW w:w="4814" w:type="dxa"/></w:tcPr><w:p><w:pPr><w:jc w:val="right"/></w:pPr>"#,
    r#"<w:r><w:t>{{total}}</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#,
    // A text box. Word writes its content twice: as DrawingML (`wp:docPr`
    // id 1) and as VML for older readers.
    r#"<w:p w14:paraId="8192A3B4" w14:textId="77777777" w:rsidR="00B57C6E" w:rsidRDefault="00B57C6E">"#,
    r#"<w:r><mc:AlternateContent><mc:Choice Requires="wps"><w:drawing>"#,
    r#"<wp:anchor distT="45720" distB="45720" distL="114300" distR="114300" simplePos="0""#,
    r#" relativeHeight="251659264" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">"#,
    r#"<wp:simplePos x="0" y="0"/><wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset>"#,
    r#"</wp:positionH><wp:positionV relativeFrom="paragraph"><wp:posOffset>0</wp:posOffset></wp:positionV>"#,
    r#"<wp:extent cx="2360930" cy="1404620"/><wp:effectExtent l="0" t="0" r="0" b="0"/>"#,
    r#"<wp:wrapSquare wrapText="bothSides"/><wp:docPr id="1" name="Text Box 1"/><wp:cNvGraphicFramePr/>"#,
    r#"<a:graphic xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#,
    r#"<a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><wps:wsp>"#,
    r#"<wps:cNvSpPr txBox="1"><a:spLocks noChangeArrowheads="1"/></wps:cNvSpPr><wps:spPr bwMode="auto">"#,
    r#"<a:xfrm><a:off x="0" y="0"/><a:ext cx="2360930" cy="1404620"/></a:xfrm>"#,
    r#"<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></wps:spPr>"#,
    r#"<wps:txbx><w:txbxContent><w:p><w:r><w:t>{{boxed}}</w:t></w:r></w:p></w:txbxContent></wps:txbx>"#,
    r#"<wps:bodyPr rot="0" vert="horz" wrap="square" lIns="91440" tIns="45720" rIns="91440" bIns="45720""#,
    r#" anchor="t" anchorCtr="0"><a:spAutoFit/></wps:bodyPr></wps:wsp></a:graphicData></a:graphic>"#,
    r#"</wp:anchor></w:drawing></mc:Choice><mc:Fallback><w:pict>"#,
    r#"<v:shapetype id="_x0000_t202" coordsize="21600,21600" o:spt="202" path="m,l,21600r21600,l21600,xe">"#,
    r#"<v:stroke joinstyle="miter"/><v:path gradientshapeok="t" o:connecttype="rect"/></v:shapetype>"#,
    r##"<v:shape id="Text Box 1" o:spid="_x0000_s1026" type="#_x0000_t202""##,
    r#" style="position:absolute;margin-left:0;margin-top:0;width:185.9pt;height:110.6pt;z-index:251659264">"#,
    r#"<v:textbox style="mso-fit-shape-to-text:t"><w:txbxContent><w:p><w:r><w:t>{{boxed}}</w:t></w:r></w:p>"#,
    r#"</w:txbxContent></v:textbox><w10:wrap type="square"/></v:shape></w:pict></mc:Fallback>"#,
    r#"</mc:AlternateContent></w:r></w:p>"#,
    // A content control.
    r#"<w:sdt><w:sdtPr><w:alias w:val="Date"/><w:tag w:val="date"/><w:id w:val="-1234567"/></w:sdtPr>"#,
    r#"<w:sdtContent><w:p w14:paraId="92A3B4C5" w14:textId="77777777" w:rsidR="00B57C6E" w:rsidRDefault="00B57C6E">"#,
    r#"<w:r><w:t>{{date}}</w:t></w:r></w:p></w:sdtContent></w:sdt>"#,
    // The section, with its header and footer.
    r#"<w:sectPr w:rsidR="00B57C6E"><w:headerReference w:type="default" r:id="rId6"/>"#,
    r#"<w:footerReference w:type="default" r:id="rId10"/><w:pgSz w:w="11906" w:h="16838"/>"#,
    r#"<w:pgMar w:top="1417" w:right="1134" w:bottom="1134" w:left="1134" w:header="708" w:footer="708" w:gutter="0"/>"#,
    r#"<w:cols w:space="708"/><w:docGrid w:linePitch="360"/></w:sectPr>"#,
    "</w:body></w:document>",
);

/// The main part's relationships, in Word's shuffled order. The largest id
/// is rId10.
pub const DOCUMENT_RELS: &str = concat!(
    declaration!(),
    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    r#"<Relationship Id="rId7" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.png"/>"#,
    r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings" Target="settings.xml"/>"#,
    r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>"#,
    r#"<Relationship Id="rId6" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/>"#,
    r#"<Relationship Id="rId5" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com/" TargetMode="External"/>"#,
    r#"<Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/endnotes" Target="endnotes.xml"/>"#,
    r#"<Relationship Id="rId10" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer" Target="footer1.xml"/>"#,
    r#"<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes" Target="footnotes.xml"/>"#,
    "</Relationships>",
);

pub const STYLES: &str = concat!(
    declaration!(),
    r#"<w:styles xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006""#,
    r#" xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#,
    r#" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml" mc:Ignorable="w14">"#,
    r#"<w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Calibri" w:hAnsi="Calibri" w:cs="Times New Roman"/>"#,
    r#"<w:sz w:val="22"/><w:szCs w:val="22"/><w:lang w:val="en-GB" w:eastAsia="en-US" w:bidi="ar-SA"/>"#,
    r#"</w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="160" w:line="259" w:lineRule="auto"/>"#,
    r#"</w:pPr></w:pPrDefault></w:docDefaults>"#,
    r#"<w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:qFormat/></w:style>"#,
    r#"<w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/><w:basedOn w:val="Normal"/>"#,
    r#"<w:next w:val="Normal"/><w:qFormat/><w:rPr><w:sz w:val="56"/></w:rPr></w:style>"#,
    r#"<w:style w:type="table" w:default="1" w:styleId="TableNormal"><w:name w:val="Normal Table"/>"#,
    r#"<w:tblPr><w:tblInd w:w="0" w:type="dxa"/><w:tblCellMar><w:top w:w="0" w:type="dxa"/>"#,
    r#"<w:left w:w="108" w:type="dxa"/><w:bottom w:w="0" w:type="dxa"/><w:right w:w="108" w:type="dxa"/>"#,
    r#"</w:tblCellMar></w:tblPr></w:style>"#,
    r#"<w:style w:type="table" w:styleId="TableGrid"><w:name w:val="Table Grid"/><w:basedOn w:val="TableNormal"/>"#,
    r#"<w:pPr><w:spacing w:after="0" w:line="240" w:lineRule="auto"/></w:pPr><w:tblPr><w:tblBorders>"#,
    r#"<w:top w:val="single" w:sz="4" w:space="0" w:color="auto"/><w:left w:val="single" w:sz="4" w:space="0" w:color="auto"/>"#,
    r#"<w:bottom w:val="single" w:sz="4" w:space="0" w:color="auto"/><w:right w:val="single" w:sz="4" w:space="0" w:color="auto"/>"#,
    r#"<w:insideH w:val="single" w:sz="4" w:space="0" w:color="auto"/><w:insideV w:val="single" w:sz="4" w:space="0" w:color="auto"/>"#,
    r#"</w:tblBorders></w:tblPr></w:style>"#,
    r#"<w:style w:type="paragraph" w:styleId="Header"><w:name w:val="header"/><w:basedOn w:val="Normal"/></w:style>"#,
    r#"<w:style w:type="paragraph" w:styleId="Footer"><w:name w:val="footer"/><w:basedOn w:val="Normal"/></w:style>"#,
    r#"<w:style w:type="character" w:styleId="Hyperlink"><w:name w:val="Hyperlink"/>"#,
    r#"<w:rPr><w:color w:val="0563C1"/><w:u w:val="single"/></w:rPr></w:style>"#,
    r#"<w:style w:type="paragraph" w:styleId="FootnoteText"><w:name w:val="footnote text"/><w:basedOn w:val="Normal"/>"#,
    r#"<w:rPr><w:sz w:val="20"/></w:rPr></w:style>"#,
    r#"<w:style w:type="character" w:styleId="FootnoteReference"><w:name w:val="footnote reference"/>"#,
    r#"<w:rPr><w:vertAlign w:val="superscript"/></w:rPr></w:style>"#,
    r#"<w:style w:type="paragraph" w:styleId="EndnoteText"><w:name w:val="endnote text"/><w:basedOn w:val="Normal"/></w:style>"#,
    r#"<w:style w:type="character" w:styleId="EndnoteReference"><w:name w:val="endnote reference"/>"#,
    r#"<w:rPr><w:vertAlign w:val="superscript"/></w:rPr></w:style>"#,
    "</w:styles>",
);

pub const SETTINGS: &str = concat!(
    declaration!(),
    r#"<w:settings xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
    r#"<w:zoom w:percent="100"/><w:proofState w:spelling="clean" w:grammar="clean"/>"#,
    r#"<w:defaultTabStop w:val="708"/><w:characterSpacingControl w:val="doNotCompress"/>"#,
    r#"<w:footnotePr><w:footnote w:id="-1"/><w:footnote w:id="0"/></w:footnotePr>"#,
    r#"<w:endnotePr><w:endnote w:id="-1"/><w:endnote w:id="0"/></w:endnotePr>"#,
    r#"<w:compat><w:compatSetting w:name="compatibilityMode" w:uri="http://schemas.microsoft.com/office/word" w:val="15"/>"#,
    r#"</w:compat><w:rsids><w:rsidRoot w:val="00B57C6E"/><w:rsid w:val="00A13F2E"/><w:rsid w:val="00B57C6E"/>"#,
    r#"</w:rsids><w:decimalSymbol w:val="."/><w:listSeparator w:val=","/></w:settings>"#,
);

/// Document settings as Word 2019 writes them for a document without
/// footnotes or endnotes: `w:compat` is the first child that the schema
/// orders after `w:updateFields`.
pub const WORD_SETTINGS: &str = concat!(
    declaration!(),
    r#"<w:settings xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006""#,
    r#" xmlns:o="urn:schemas-microsoft-com:office:office""#,
    r#" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#,
    r#" xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math""#,
    r#" xmlns:v="urn:schemas-microsoft-com:vml" xmlns:w10="urn:schemas-microsoft-com:office:word""#,
    r#" xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#,
    r#" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml""#,
    r#" xmlns:w15="http://schemas.microsoft.com/office/word/2012/wordml""#,
    r#" xmlns:w16cex="http://schemas.microsoft.com/office/word/2018/wordml/cex""#,
    r#" xmlns:w16cid="http://schemas.microsoft.com/office/word/2016/wordml/cid""#,
    r#" xmlns:w16="http://schemas.microsoft.com/office/word/2018/wordml""#,
    r#" xmlns:w16se="http://schemas.microsoft.com/office/word/2015/wordml/symex""#,
    r#" xmlns:sl="http://schemas.openxmlformats.org/schemaLibrary/2006/main""#,
    r#" mc:Ignorable="w14 w15 w16se w16cid w16 w16cex">"#,
    r#"<w:zoom w:percent="100"/><w:proofState w:spelling="clean" w:grammar="clean"/>"#,
    r#"<w:defaultTabStop w:val="708"/><w:hyphenationZone w:val="283"/>"#,
    r#"<w:characterSpacingControl w:val="doNotCompress"/>"#,
    r#"<w:compat><w:compatSetting w:name="compatibilityMode" w:uri="http://schemas.microsoft.com/office/word" w:val="15"/>"#,
    r#"<w:compatSetting w:name="overrideTableStyleFontSizeAndJustification" w:uri="http://schemas.microsoft.com/office/word" w:val="1"/>"#,
    r#"<w:compatSetting w:name="enableOpenTypeFeatures" w:uri="http://schemas.microsoft.com/office/word" w:val="1"/>"#,
    r#"<w:compatSetting w:name="doNotFlipMirrorIndents" w:uri="http://schemas.microsoft.com/office/word" w:val="1"/>"#,
    r#"<w:compatSetting w:name="differentiateMultirowTableHeaders" w:uri="http://schemas.microsoft.com/office/word" w:val="1"/>"#,
    r#"</w:compat><w:rsids><w:rsidRoot w:val="00B57C6E"/><w:rsid w:val="00A13F2E"/><w:rsid w:val="00B57C6E"/></w:rsids>"#,
    r#"<m:mathPr><m:mathFont m:val="Cambria Math"/><m:brkBin m:val="before"/><m:brkBinSub m:val="--"/>"#,
    r#"<m:smallFrac m:val="0"/><m:dispDef/><m:lMargin m:val="0"/><m:rMargin m:val="0"/>"#,
    r#"<m:defJc m:val="centerGroup"/><m:wrapIndent m:val="1440"/><m:intLim m:val="subSup"/>"#,
    r#"<m:naryLim m:val="undOvr"/></m:mathPr><w:themeFontLang w:val="en-GB"/>"#,
    r#"<w:clrSchemeMapping w:bg1="light1" w:t1="dark1" w:bg2="light2" w:t2="dark2" w:accent1="accent1""#,
    r#" w:accent2="accent2" w:accent3="accent3" w:accent4="accent4" w:accent5="accent5""#,
    r#" w:accent6="accent6" w:hyperlink="hyperlink" w:followedHyperlink="followedHyperlink"/>"#,
    r#"<w:shapeDefaults><o:shapedefaults v:ext="edit" spidmax="1026"/><o:shapelayout v:ext="edit">"#,
    r#"<o:idmap v:ext="edit" data="1"/></o:shapelayout></w:shapeDefaults>"#,
    r#"<w:decimalSymbol w:val="."/><w:listSeparator w:val=","/>"#,
    r#"<w14:docId w14:val="1A2B3C4D"/><w15:chartTrackingRefBased/>"#,
    r#"<w15:docId w15:val="{9B5E1E4A-3C3F-4C6B-9F0E-2B7A5D3C1E8F}"/></w:settings>"#,
);

/// A header without a relationships part, whose image placeholder is split
/// between runs with different formatting.
pub const HEADER: &str = concat!(
    declaration!(),
    "<w:hdr",
    word_namespaces!(),
    ">",
    r#"<w:p w14:paraId="A3B4C5D6" w14:textId="77777777" w:rsidR="00B57C6E" w:rsidRDefault="00B57C6E">"#,
    r#"<w:pPr><w:pStyle w:val="Header"/></w:pPr><w:r><w:t xml:space="preserve">{{company}} - </w:t></w:r>"#,
    r#"<w:r><w:t>{{</w:t></w:r><w:r w:rsidR="00A13F2E"><w:rPr><w:i/></w:rPr><w:t>logo}}</w:t></w:r></w:p>"#,
    "</w:hdr>",
);

/// A footer with a relationships part (a hyperlink, rId1) and a page
/// number field.
pub const FOOTER: &str = concat!(
    declaration!(),
    "<w:ftr",
    word_namespaces!(),
    ">",
    r#"<w:p w14:paraId="B4C5D6E7" w14:textId="77777777" w:rsidR="00B57C6E" w:rsidRDefault="00B57C6E">"#,
    r#"<w:pPr><w:pStyle w:val="Footer"/></w:pPr><w:r><w:t xml:space="preserve">Page </w:t></w:r>"#,
    r#"<w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText xml:space="preserve"> PAGE </w:instrText></w:r>"#,
    r#"<w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>1</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r>"#,
    r#"<w:r><w:t xml:space="preserve"> - </w:t></w:r><w:hyperlink r:id="rId1" w:history="1">"#,
    r#"<w:r><w:rPr><w:rStyle w:val="Hyperlink"/></w:rPr><w:t>{{company}}</w:t></w:r></w:hyperlink></w:p>"#,
    r#"<w:p w14:paraId="C5D6E7F8" w14:textId="77777777" w:rsidR="00B57C6E" w:rsidRDefault="00B57C6E">"#,
    r#"<w:pPr><w:pStyle w:val="Footer"/><w:jc w:val="right"/></w:pPr><w:r><w:t>{{signature}}</w:t></w:r></w:p>"#,
    "</w:ftr>",
);

pub const FOOTER_RELS: &str = concat!(
    declaration!(),
    r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com/" TargetMode="External"/>"#,
    "</Relationships>",
);

pub const FOOTNOTES: &str = concat!(
    declaration!(),
    "<w:footnotes",
    word_namespaces!(),
    ">",
    r#"<w:footnote w:type="separator" w:id="-1"><w:p><w:pPr><w:spacing w:after="0" w:line="240" w:lineRule="auto"/>"#,
    r#"</w:pPr><w:r><w:separator/></w:r></w:p></w:footnote>"#,
    r#"<w:footnote w:type="continuationSeparator" w:id="0"><w:p><w:pPr><w:spacing w:after="0" w:line="240" w:lineRule="auto"/>"#,
    r#"</w:pPr><w:r><w:continuationSeparator/></w:r></w:p></w:footnote>"#,
    r#"<w:footnote w:id="1"><w:p><w:pPr><w:pStyle w:val="FootnoteText"/></w:pPr>"#,
    r#"<w:r><w:rPr><w:rStyle w:val="FootnoteReference"/></w:rPr><w:footnoteRef/></w:r>"#,
    r#"<w:r><w:t xml:space="preserve"> Prices in {{currency}}.</w:t></w:r></w:p></w:footnote>"#,
    "</w:footnotes>",
);

pub const ENDNOTES: &str = concat!(
    declaration!(),
    "<w:endnotes",
    word_namespaces!(),
    ">",
    r#"<w:endnote w:type="separator" w:id="-1"><w:p><w:pPr><w:spacing w:after="0" w:line="240" w:lineRule="auto"/>"#,
    r#"</w:pPr><w:r><w:separator/></w:r></w:p></w:endnote>"#,
    r#"<w:endnote w:type="continuationSeparator" w:id="0"><w:p><w:pPr><w:spacing w:after="0" w:line="240" w:lineRule="auto"/>"#,
    r#"</w:pPr><w:r><w:continuationSeparator/></w:r></w:p></w:endnote>"#,
    r#"<w:endnote w:id="1"><w:p><w:pPr><w:pStyle w:val="EndnoteText"/></w:pPr>"#,
    r#"<w:r><w:rPr><w:rStyle w:val="EndnoteReference"/></w:rPr><w:endnoteRef/></w:r>"#,
    r#"<w:r><w:t xml:space="preserve"> Issued by {{company}} in London.</w:t></w:r></w:p></w:endnote>"#,
    "</w:endnotes>",
);

/// A picture with the `wp:docPr` id `$id`, as run content.
macro_rules! picture {
    ($id:literal) => {
        concat!(
            r#"<w:drawing><wp:inline distT="0" distB="0" distL="0" distR="0">"#,
            r#"<wp:extent cx="285750" cy="190500"/><wp:effectExtent l="0" t="0" r="0" b="0"/>"#,
            r#"<wp:docPr id=""#,
            $id,
            r#"" name="Picture "#,
            $id,
            r#""/><a:graphic xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#,
            r#"<a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture"/>"#,
            r#"</a:graphic></wp:inline></w:drawing>"#,
        )
    };
}

/// Relationships from the main part to [`COMMENTS`] and [`GLOSSARY`], to
/// add to [`DOCUMENT_RELS`].
pub const COMMENTS_AND_GLOSSARY_RELS: &str = concat!(
    r#"<Relationship Id="rId11" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="comments.xml"/>"#,
    r#"<Relationship Id="rId12" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/glossaryDocument" Target="glossary/document.xml"/>"#,
);

/// Comments, which are not searched, with a picture whose `wp:docPr` id is
/// 3.
pub const COMMENTS: &str = concat!(
    declaration!(),
    "<w:comments",
    word_namespaces!(),
    ">",
    r#"<w:comment w:id="0" w:author="Template author" w:date="2026-10-02T09:00:00Z" w:initials="TA">"#,
    r#"<w:p><w:r><w:annotationRef/></w:r><w:r><w:t xml:space="preserve">Use {{logo}} here: </w:t></w:r>"#,
    "<w:r>",
    picture!("3"),
    "</w:r></w:p></w:comment></w:comments>",
);

/// A glossary document with a building block holding a picture whose
/// `wp:docPr` id is 4.
pub const GLOSSARY: &str = concat!(
    declaration!(),
    "<w:glossaryDocument",
    word_namespaces!(),
    "><w:docParts><w:docPart><w:docPartPr><w:name w:val=\"Logo block\"/>",
    "<w:category><w:name w:val=\"General\"/><w:gallery w:val=\"placeholder\"/></w:category>",
    "</w:docPartPr><w:docPartBody><w:p><w:r>",
    picture!("4"),
    "</w:r></w:p></w:docPartBody></w:docPart></w:docParts></w:glossaryDocument>",
);

// ── Images ───────────────────────────────────────────────────────────────────

/// A complete 3x2 8-bit RGB PNG: red, green, blue / white, black, gray.
pub const PNG: &[u8] = include_bytes!("../../src/testdata/3x2.png");

/// A 3x2 baseline grayscale JPEG (JFIF), every pixel 160.
pub const JPEG: &[u8] = include_bytes!("../../src/testdata/3x2.jpeg");

// ── Packages ─────────────────────────────────────────────────────────────────

/// The entries of a ZIP archive to build: names and contents, in archive
/// order.
///
/// A name ending with `/` is a directory. Files with an `.xml` or `.rels`
/// extension are deflated and the others stored, so archives exercise both
/// methods: the template stores its picture and `_rels/.rels`, a name
/// without an extension.
#[derive(Clone, Debug)]
pub struct Fixture {
    entries: Vec<(String, Vec<u8>)>,
}

impl Fixture {
    /// A package holding `entries`.
    #[must_use]
    pub fn new<'a>(entries: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|(name, content)| (name.to_owned(), content.to_vec()))
                .collect(),
        }
    }

    /// The sample template: a Word document with placeholders in the body,
    /// a table, a text box, a content control, a header without
    /// relationships, a footer with relationships, footnotes and endnotes.
    #[must_use]
    pub fn template() -> Self {
        Self::new([
            ("[Content_Types].xml", CONTENT_TYPES.as_bytes()),
            ("_rels/.rels", PACKAGE_RELS.as_bytes()),
            ("docProps/core.xml", CORE.as_bytes()),
            ("docProps/app.xml", APP.as_bytes()),
            ("word/", b"".as_slice()),
            ("word/document.xml", DOCUMENT.as_bytes()),
            ("word/_rels/document.xml.rels", DOCUMENT_RELS.as_bytes()),
            ("word/styles.xml", STYLES.as_bytes()),
            ("word/settings.xml", SETTINGS.as_bytes()),
            ("word/footnotes.xml", FOOTNOTES.as_bytes()),
            ("word/endnotes.xml", ENDNOTES.as_bytes()),
            ("word/header1.xml", HEADER.as_bytes()),
            ("word/footer1.xml", FOOTER.as_bytes()),
            ("word/_rels/footer1.xml.rels", FOOTER_RELS.as_bytes()),
            ("word/media/image1.png", PNG),
        ])
    }

    /// Replace the content of the entry `name`, or add the entry at the
    /// end.
    #[must_use]
    pub fn with(mut self, name: &str, content: impl AsRef<[u8]>) -> Self {
        let content = content.as_ref().to_vec();
        match self
            .entries
            .iter_mut()
            .find(|(existing, _)| existing == name)
        {
            Some((_, existing)) => *existing = content,
            None => self.entries.push((name.to_owned(), content)),
        }
        self
    }

    /// Remove the entry `name`, which must exist.
    #[must_use]
    pub fn without(mut self, name: &str) -> Self {
        let before = self.entries.len();
        self.entries.retain(|(existing, _)| existing != name);
        assert_eq!(self.entries.len() + 1, before, "no entry named {name:?}");
        self
    }

    /// Return the names of the entries, in order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|(name, _)| name.as_str()).collect()
    }

    /// Return the content of the entry `name`.
    #[must_use]
    pub fn content(&self, name: &str) -> &[u8] {
        let Some((_, content)) = self.entries.iter().find(|(existing, _)| existing == name) else {
            panic!("no entry named {name:?}");
        };
        content
    }

    /// Build the ZIP archive.
    #[must_use]
    pub fn zip(&self) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, content) in &self.entries {
            if name.ends_with('/') {
                writer
                    .add_directory(name.as_str(), SimpleFileOptions::default())
                    .unwrap();
                continue;
            }
            let xml = Path::new(name).extension().is_some_and(|ext| {
                ext.eq_ignore_ascii_case("xml") || ext.eq_ignore_ascii_case("rels")
            });
            let method = if xml {
                CompressionMethod::Deflated
            } else {
                CompressionMethod::Stored
            };
            writer
                .start_file(
                    name.as_str(),
                    SimpleFileOptions::default().compression_method(method),
                )
                .unwrap();
            writer.write_all(content).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }
}

// ── Reading archives ─────────────────────────────────────────────────────────

/// One entry of an archive as stored: everything a byte-for-byte copy
/// keeps.
#[derive(Debug, PartialEq, Eq)]
pub struct RawEntry {
    pub name: String,
    pub method: CompressionMethod,
    pub crc32: u32,
    pub size: u64,
    pub compressed: Vec<u8>,
}

/// Return the entries of the archive `zip` in archive order, without
/// decompressing them.
#[must_use]
pub fn raw_entries(zip: &[u8]) -> Vec<RawEntry> {
    let mut archive = ZipArchive::new(Cursor::new(zip)).unwrap();
    (0..archive.len())
        .map(|index| {
            let mut file = archive.by_index_raw(index).unwrap();
            let mut compressed = Vec::new();
            file.read_to_end(&mut compressed).unwrap();
            RawEntry {
                name: file.name().unwrap().into_owned(),
                method: file.compression(),
                crc32: file.crc32(),
                size: file.size(),
                compressed,
            }
        })
        .collect()
}

/// Return the entry names of the archive `zip`, in archive order.
#[must_use]
pub fn names(zip: &[u8]) -> Vec<String> {
    raw_entries(zip)
        .into_iter()
        .map(|entry| entry.name)
        .collect()
}

/// Read the entry `name` of the archive `zip`, decompressed; the `zip`
/// crate checks its CRC-32.
#[must_use]
pub fn read(zip: &[u8], name: &str) -> Vec<u8> {
    let mut archive = ZipArchive::new(Cursor::new(zip)).unwrap();
    let mut file = archive
        .by_name(name)
        .unwrap_or_else(|err| panic!("reading {name:?}: {err}"));
    let mut content = Vec::new();
    file.read_to_end(&mut content).unwrap();
    content
}

/// Read the entry `name` of the archive `zip` as UTF-8 text.
#[must_use]
pub fn read_text(zip: &[u8], name: &str) -> String {
    String::from_utf8(read(zip, name)).unwrap()
}

/// Return whether the archive `zip` has an entry named `name`.
#[must_use]
pub fn contains(zip: &[u8], name: &str) -> bool {
    names(zip).iter().any(|existing| existing == name)
}

/// Return what ciclostile writes for the archive `zip` when nothing is
/// replaced: the baseline an edited document is compared with, written by
/// the same code.
#[must_use]
pub fn untouched(zip: &[u8]) -> Vec<u8> {
    Docx::from_bytes(zip.to_vec()).unwrap().to_bytes().to_vec()
}

/// Check that `output`, which ciclostile wrote from the archive `input`
/// without replacing anything, holds the entries of `input` in the same
/// order and nothing else: compressed entries copied byte for byte, and
/// uncompressed ones with identical content, CRC-32 and size, but
/// compressed.
pub fn assert_copied(input: &[u8], output: &[u8]) {
    let before = raw_entries(input);
    let after = raw_entries(output);
    let names = |entries: &[RawEntry]| -> Vec<String> {
        entries.iter().map(|entry| entry.name.clone()).collect()
    };
    assert_eq!(names(&after), names(&before), "entries differ");
    for (old, new) in before.iter().zip(&after) {
        if old.method == CompressionMethod::Stored && !old.name.ends_with('/') {
            assert_eq!(
                new.method,
                CompressionMethod::Deflated,
                "{} should come out compressed",
                old.name
            );
            assert_eq!((new.crc32, new.size), (old.crc32, old.size), "{}", old.name);
            // The stored bytes are the content.
            assert_eq!(read(output, &new.name), old.compressed, "{}", old.name);
        } else {
            assert_eq!(old, new, "{} should be copied byte for byte", old.name);
        }
    }
}

/// Check that `after` starts with every entry of `before`, in the same
/// order, and that each of them is stored exactly as before - compressed
/// bytes, method, CRC-32 and size - except those named in `changed`, whose
/// content must differ. Return the names of the entries `after` adds.
///
/// `before` is normally the [`untouched`] output of the same template, so
/// that both archives come out of the same writer.
#[must_use = "check the entries added"]
pub fn assert_kept_except(before: &[u8], after: &[u8], changed: &[&str]) -> Vec<String> {
    let before = raw_entries(before);
    let mut after = raw_entries(after);
    assert!(after.len() >= before.len(), "entries were removed");
    let added = after.split_off(before.len());
    for (old, new) in before.iter().zip(&after) {
        assert_eq!(old.name, new.name, "entry order changed");
        if changed.contains(&old.name.as_str()) {
            assert_ne!(old.crc32, new.crc32, "{} should have changed", old.name);
        } else {
            assert_eq!(old, new, "{} should be kept byte for byte", old.name);
        }
    }
    added.into_iter().map(|entry| entry.name).collect()
}

// ── Reading parts ────────────────────────────────────────────────────────────

/// Return the text of each paragraph (`w:p`) of a WordprocessingML part,
/// in the order the paragraphs end, so a text box paragraph comes before
/// the paragraph holding it.
///
/// Text is what Word shows: `w:t` content unescaped, `w:br` and `w:cr` as
/// `\n`, `w:tab` run content as `\t`. Deleted text (`w:delText`) and field
/// instructions are left out. This is a scanner for the markup the tests
/// produce, not an XML parser.
#[must_use]
pub fn paragraphs(xml: &str) -> Vec<String> {
    let mut done = Vec::new();
    let mut open: Vec<String> = Vec::new();
    let mut in_text = false;
    let mut in_tab_stops = false;
    let mut rest = xml;
    while let Some(start) = rest.find('<') {
        if in_text && let Some(paragraph) = open.last_mut() {
            paragraph.push_str(&unescape(&rest[..start]));
        }
        let end = start + rest[start..].find('>').expect("unterminated tag");
        let tag = &rest[start + 1..end];
        rest = &rest[end + 1..];

        let closing = tag.starts_with('/');
        let empty = tag.ends_with('/');
        let name = tag
            .trim_start_matches('/')
            .split(|c: char| c.is_ascii_whitespace() || c == '/')
            .next()
            .unwrap_or_default();
        match name {
            "w:p" if closing => done.push(open.pop().expect("unbalanced paragraph")),
            "w:p" if empty => done.push(String::new()),
            "w:p" => open.push(String::new()),
            "w:t" => in_text = !closing && !empty,
            "w:tabs" => in_tab_stops = !closing && !empty,
            "w:br" | "w:cr" => {
                let paragraph = open.last_mut().expect("break outside a paragraph");
                paragraph.push('\n');
            }
            "w:tab" if !in_tab_stops => {
                let paragraph = open.last_mut().expect("tab outside a paragraph");
                paragraph.push('\t');
            }
            _ => {}
        }
    }
    done
}

/// Unescape the five predefined entities.
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Return the `id` of every `wp:docPr` element in `xml`, in document order.
#[must_use]
pub fn drawing_ids(xml: &str) -> Vec<u32> {
    xml.split("<wp:docPr ")
        .skip(1)
        .map(|tag| {
            let value = tag.split("id=\"").nth(1).expect("docPr without id");
            value[..value.find('"').unwrap()].parse().unwrap()
        })
        .collect()
}

/// Return how many times `needle` occurs in `haystack`.
#[must_use]
pub fn count(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}
