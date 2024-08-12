/*! Convert SVG files to PDFs.

This crate allows to convert static (i.e. non-interactive) SVG files to
either standalone PDF files or Form XObjects that can be embedded in another
PDF file and used just like images.

The conversion will translate the SVG content to PDF without rasterizing them
(the only exception being objects with filters on them, but in this case only
this single group will be rasterized, while the remaining contents of the SVG
will still be turned into a vector graphic), so no quality is lost.

## Example
This example reads an SVG file and writes the corresponding PDF back to the disk.

```
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use svg2pdf::usvg::fontdb;
use svg2pdf::{ConversionOptions, PageOptions};
use std::sync::Arc;

let input = "tests/svg/custom/integration/matplotlib/stairs.svg";
let output = "target/stairs.pdf";

let svg = std::fs::read_to_string(input)?;
let mut options = svg2pdf::usvg::Options::default();
options.fontdb_mut().load_system_fonts();
let tree = svg2pdf::usvg::Tree::from_str(&svg, &options)?;

let pdf = svg2pdf::to_pdf(&tree, ConversionOptions::default(), PageOptions::default()).unwrap();
std::fs::write(output, pdf)?;
# Ok(()) }
```

## Supported features
In general, a very large part of the SVG specification is supported, including
but not limited to:
- Paths with simple and complex fills
- Gradients
- Patterns
- Clip paths
- Masks
- Transformations
- Viewbox
- Text
- Raster images and nested SVGs

## Unsupported features
Among the unsupported features are currently:
- The `spreadMethod` attribute of gradients
- Raster images are not color managed but use PDF's DeviceRGB color space
- A number of features that were added in SVG2, See
[here](https://github.com/RazrFalcon/resvg/blob/master/docs/svg2-changelog.md) for a more
comprehensive list.
 */

mod render;
mod util;

use krilla::serialize::{SerializeSettings, SvgSettings};
use std::fmt;
use std::fmt::{Display, Formatter};
use fontdb::Database;
use krilla::document::Document;
pub use usvg;

use crate::ConversionError::UnknownError;
use once_cell::sync::Lazy;
use pdf_writer::{Chunk, Content, Filter, Finish, Pdf, Ref, TextStr};
use usvg::{Size, Transform, Tree};

use crate::render::{tree_to_stream, tree_to_xobject};
use crate::util::context::Context;
use crate::util::helper::{deflate, RectExt, TransformExt};
use crate::util::resources::ResourceContainer;

// The ICC profiles.
static SRGB_ICC_DEFLATED: Lazy<Vec<u8>> =
    Lazy::new(|| deflate(include_bytes!("icc/sRGB-v4.icc")));
static GRAY_ICC_DEFLATED: Lazy<Vec<u8>> =
    Lazy::new(|| deflate(include_bytes!("icc/sGrey-v4.icc")));

/// Options for the resulting PDF file.
#[derive(Copy, Clone)]
pub struct PageOptions {
    /// The DPI that should be assumed for the conversion to PDF.
    ///
    /// _Default:_ 72.0
    pub dpi: f32,
}

impl Default for PageOptions {
    fn default() -> Self {
        Self { dpi: 72.0 }
    }
}

/// A error that can appear during conversion.
#[derive(Copy, Clone, Debug)]
pub enum ConversionError {
    /// The SVG image contains an unrecognized type of image.
    InvalidImage,
    /// An unknown error occurred during the conversion. This could indicate a bug in the
    /// svg2pdf.
    UnknownError,
    /// An error occurred while subsetting a font.
    #[cfg(feature = "text")]
    SubsetError(fontdb::ID),
    /// An error occurred while reading a font.
    #[cfg(feature = "text")]
    InvalidFont(fontdb::ID),
}

impl Display for ConversionError {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            Self::InvalidImage => f.write_str("An unknown type of image appears in the SVG."),
            Self::UnknownError => f.write_str("An unknown error occurred during the conversion. This could indicate a bug in svg2pdf"),
            #[cfg(feature = "text")]
            Self::SubsetError(_) => f.write_str("An error occurred while subsetting a font."),
            #[cfg(feature = "text")]
            Self::InvalidFont(_) => f.write_str("An error occurred while reading a font."),
        }
    }
}

/// The result type for everything.
type Result<T> = std::result::Result<T, ConversionError>;

/// Options for the PDF conversion.
#[derive(Copy, Clone)]
pub struct ConversionOptions {
    /// Whether the content streams should be compressed.
    ///
    /// The smaller PDFs generated by this are generally more practical, but it
    /// might increase run-time a bit.
    ///
    /// _Default:_ `true`.
    pub compress: bool,

    /// How much raster images of rasterized effects should be scaled up.
    ///
    /// Higher values will lead to better quality, but will increase the size of
    /// the pdf.
    ///
    /// _Default:_ 1.5
    pub raster_scale: f32,

    /// Whether text should be embedded as actual selectable text inside
    /// the PDF. If this option is disabled, text will be converted into paths
    /// before rendering.
    ///
    /// _Default:_ `true`.
    pub embed_text: bool,
}

impl Default for ConversionOptions {
    fn default() -> Self {
        Self {
            compress: true,
            raster_scale: 1.5,
            embed_text: true,
        }
    }
}

/// Convert a [`usvg` tree](Tree) into a standalone PDF buffer.
///
/// ## Example
/// The example below reads an SVG file, processes text within it, then converts
/// it into a PDF and finally writes it back to the file system.
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use svg2pdf::usvg::fontdb;
/// use svg2pdf::{ConversionOptions, PageOptions};
/// use std::sync::Arc;
///
/// let input = "tests/svg/custom/integration/matplotlib/stairs.svg";
/// let output = "target/stairs.pdf";
///
/// let svg = std::fs::read_to_string(input)?;
/// let mut options = svg2pdf::usvg::Options::default();
/// options.fontdb_mut().load_system_fonts();
/// let mut tree = svg2pdf::usvg::Tree::from_str(&svg, &options)?;
///
///
/// let pdf = svg2pdf::to_pdf(&tree, ConversionOptions::default(), PageOptions::default()).unwrap();
/// std::fs::write(output, pdf)?;
/// # Ok(()) }
/// ```
pub fn to_pdf(
    tree: &Tree,
    conversion_options: ConversionOptions,
    page_options: PageOptions,
) -> Result<Vec<u8>> {
    let mut document_builder = Document::new(SerializeSettings {
        hex_encode_binary_streams: false,
        compress_content_streams: true,
        no_device_cs: true,
        svg_settings: SvgSettings::default(),
    });

    let mut page = document_builder.start_page(tree.size());
    let mut surface = page.surface();
    let mut fontdb = Database::new();
     krilla::svg::render_tree(tree, SvgSettings::default(), &mut surface, &mut fontdb);
    surface.finish();
    page.finish();

    Ok(document_builder.finish(&fontdb))
}

/// Convert a [Tree] into a [`Chunk`].
///
/// This method is intended for use in an existing [`pdf-writer`] workflow. It
/// will always produce a chunk that contains all the necessary objects
/// to embed the SVG into an existing chunk. This method returns the chunk that
/// was produced as part of that as well as the object reference of the root XObject.
/// The XObject will have the width and height of one printer's
/// point, just like an [`ImageXObject`](pdf_writer::writers::ImageXObject)
/// would.
///
/// The resulting object can be used by embedding the chunk into your existing chunk
/// and renumbering it appropriately.
///
/// ## Example
/// Write a PDF file with some text and an SVG graphic.
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use std::collections::HashMap;
/// use std::sync::Arc;
/// use svg2pdf;
/// use pdf_writer::{Content, Finish, Name, Pdf, Rect, Ref, Str};
/// use svg2pdf::usvg::fontdb;
///
/// // Allocate the indirect reference IDs and names.
/// let mut alloc = Ref::new(1);
/// let catalog_id = alloc.bump();
/// let page_tree_id = alloc.bump();
/// let page_id = alloc.bump();
/// let font_id = alloc.bump();
/// let content_id = alloc.bump();
/// let font_name = Name(b"F1");
/// let svg_name = Name(b"S1");
///
/// // Let's first convert the SVG into an independent chunk.
/// let path = "tests/svg/custom/integration/wikimedia/coat_of_the_arms_of_edinburgh_city_council.svg";
/// let svg = std::fs::read_to_string(path)?;
/// let mut options = svg2pdf::usvg::Options::default();
/// options.fontdb_mut().load_system_fonts();
/// let tree = svg2pdf::usvg::Tree::from_str(&svg, &options)?;
/// let (mut svg_chunk, svg_id) = svg2pdf::to_chunk(&tree, svg2pdf::ConversionOptions::default()).unwrap();
///
/// // Renumber the chunk so that we can embed it into our existing workflow, and also make sure
/// // to update `svg_id`.
/// let mut map = HashMap::new();
/// let svg_chunk = svg_chunk.renumber(|old| {
///   *map.entry(old).or_insert_with(|| alloc.bump())
/// });
/// let svg_id = map.get(&svg_id).unwrap();
///
/// // Start writing the PDF.
/// let mut pdf = Pdf::new();
/// pdf.catalog(catalog_id).pages(page_tree_id);
/// pdf.pages(page_tree_id).kids([page_id]).count(1);
///
/// // Set up a simple A4 page.
/// let mut page = pdf.page(page_id);
/// page.media_box(Rect::new(0.0, 0.0, 595.0, 842.0));
/// page.parent(page_tree_id);
/// page.contents(content_id);
///
/// // Add the font and, more importantly, the SVG to the resource dictionary
/// // so that it can be referenced in the content stream.
/// let mut resources = page.resources();
/// resources.x_objects().pair(svg_name, svg_id);
/// resources.fonts().pair(font_name, font_id);
/// resources.finish();
/// page.finish();
///
/// // Set a predefined font, so we do not have to load anything extra.
/// pdf.type1_font(font_id).base_font(Name(b"Helvetica"));
///
/// // Write a content stream with some text and our SVG.
/// let mut content = Content::new();
/// content
///     .begin_text()
///     .set_font(font_name, 16.0)
///     .next_line(108.0, 734.0)
///     .show(Str(b"Look at my wonderful (distorted) vector graphic!"))
///     .end_text();
///
/// // Add our graphic.
/// content
///     .transform([300.0, 0.0, 0.0, 225.0, 147.5, 385.0])
///     .x_object(svg_name);
///
///
/// pdf.stream(content_id, &content.finish());
/// // Write the SVG chunk into the PDF page.
/// pdf.extend(&svg_chunk);
///
/// // Write the file to the disk.
/// std::fs::write("target/embedded.pdf", pdf.finish())?;
/// # Ok(()) }
/// ```
pub fn to_chunk(
    tree: &Tree,
    conversion_options: ConversionOptions,
) -> Result<(Chunk, Ref)> {
    let mut chunk = Chunk::new();

    let mut ctx = Context::new(tree, conversion_options);
    let x_ref = tree_to_xobject(tree, &mut chunk, &mut ctx)?;
    ctx.write_global_objects(&mut chunk)?;
    Ok((chunk, x_ref))
}
