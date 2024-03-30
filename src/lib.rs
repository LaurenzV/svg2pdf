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
* # fn main() -> Result<(), Box<dyn std::error::Error>> {
* use svg2pdf::usvg::fontdb;
* use svg2pdf::ConversionOptions;
*
* let input = "tests/svg/custom/integration/matplotlib/stairs.svg";
* let output = "target/stairs.pdf";
*
* let svg = std::fs::read_to_string(input)?;
* let options = svg2pdf::usvg::Options::default();
* let mut db = fontdb::Database::new();
* db.load_system_fonts();
* let tree = svg2pdf::usvg::Tree::from_str(&svg, &options, &db)?;
*
* let pdf = svg2pdf::to_pdf(&tree, Options::default(), &db);
* std::fs::write(output, pdf)?;
* # Ok(()) }
* ```

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

pub use usvg;

use once_cell::sync::Lazy;
use pdf_writer::{Chunk, Content, Filter, Finish, Pdf, Rect, Ref, TextStr};
#[cfg(feature = "text")]
use usvg::fontdb;
use usvg::Tree;

use crate::render::{tree_to_stream, tree_to_xobject};
use crate::util::context::Context;
use crate::util::helper::deflate;
use crate::util::resources::ResourceContainer;

// The ICC profiles.
static SRGB_ICC_DEFLATED: Lazy<Vec<u8>> =
    Lazy::new(|| deflate(include_bytes!("icc/sRGB-v4.icc")));
static GRAY_ICC_DEFLATED: Lazy<Vec<u8>> =
    Lazy::new(|| deflate(include_bytes!("icc/sGrey-v4.icc")));

/// Preferences for the PDF conversion.
#[derive(Copy, Clone)]
pub struct Options {
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
    /// _Default:_ 1
    pub raster_scale: f32,

    /// Whether text should be embedded as actual selectable text inside
    /// the PDF. If this option is disabled, text will be converted into paths
    /// before rendering.
    ///
    /// _Default:_ `true`.
    pub embed_text: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            compress: false,
            raster_scale: 1.0,
            embed_text: true,
        }
    }
}

/// Convert a [`usvg` tree](Tree) into a standalone PDF buffer.
///
/// IMPORTANT: The fontdb that is passed to this function needs to be the
/// same one that was used to convert the SVG string into a [`usvg` tree](Tree)!
///
/// ## Example
/// The example below reads an SVG file, processes text within it, then converts
/// it into a PDF and finally writes it back to the file system.
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use svg2pdf::usvg::fontdb;
/// use svg2pdf::Options;
///
/// let input = "tests/svg/custom/integration/matplotlib/stairs.svg";
/// let output = "target/stairs.pdf";
///
/// let svg = std::fs::read_to_string(input)?;
/// let options = svg2pdf::usvg::Options::default();
/// let mut db = fontdb::Database::new();
/// db.load_system_fonts();
/// let mut tree = svg2pdf::usvg::Tree::from_str(&svg, &options, &db)?;
///
///
/// let pdf = svg2pdf::to_pdf(&tree, Options::default(), &db);
/// std::fs::write(output, pdf)?;
/// # Ok(()) }
/// ```
pub fn to_pdf(
    tree: &Tree,
    options: Options,
    #[cfg(feature = "text")] fontdb: &fontdb::Database,
) -> Vec<u8> {
    let mut ctx = Context::new(
        #[cfg(feature = "text")]
        tree,
        options,
        #[cfg(feature = "text")]
        fontdb,
    );
    let mut pdf = Pdf::new();

    let catalog_ref = ctx.alloc_ref();
    let page_tree_ref = ctx.alloc_ref();
    let page_ref = ctx.alloc_ref();
    let content_ref = ctx.alloc_ref();

    pdf.catalog(catalog_ref).pages(page_tree_ref);
    pdf.pages(page_tree_ref).count(1).kids([page_ref]);

    // Generate main content
    let mut rc = ResourceContainer::new();
    let mut content = Content::new();
    tree_to_stream(tree, &mut pdf, &mut content, &mut ctx, &mut rc);
    let content_stream = ctx.finish_content(content);
    let mut stream = pdf.stream(content_ref, &content_stream);

    if ctx.options.compress {
        stream.filter(Filter::FlateDecode);
    }
    stream.finish();

    let mut page = pdf.page(page_ref);
    let mut page_resources = page.resources();
    rc.finish(&mut page_resources);
    page_resources.finish();

    page.media_box(Rect::new(0.0, 0.0, tree.size().width(), tree.size().height()));
    page.parent(page_tree_ref);
    page.group()
        .transparency()
        .isolated(true)
        .knockout(false)
        .color_space()
        .icc_based(ctx.srgb_ref());
    page.contents(content_ref);
    page.finish();

    ctx.write_global_objects(&mut pdf);

    let document_info_id = ctx.alloc_ref();
    pdf.document_info(document_info_id).producer(TextStr("svg2pdf"));

    pdf.finish()
}

/// Convert a [Tree] into a [`Chunk`].
///
/// IMPORTANT: The fontdb that is passed to this function needs to be the
/// same one that was used to convert the SVG string into a [`usvg` tree](Tree)!
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
/// let mut db = fontdb::Database::new();
/// db.load_system_fonts();
/// let tree = svg2pdf::usvg::Tree::from_str(&svg, &svg2pdf::usvg::Options::default(), &db)?;
/// let (mut svg_chunk, svg_id) = svg2pdf::to_chunk(&tree, svg2pdf::Options::default(), &db);
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
    options: Options,
    #[cfg(feature = "text")] fontdb: &fontdb::Database,
) -> (Chunk, Ref) {
    let mut chunk = Chunk::new();

    let mut ctx = Context::new(
        #[cfg(feature = "text")]
        tree,
        options,
        #[cfg(feature = "text")]
        fontdb,
    );
    let x_ref = tree_to_xobject(tree, &mut chunk, &mut ctx);
    ctx.write_global_objects(&mut chunk);
    (chunk, x_ref)
}
