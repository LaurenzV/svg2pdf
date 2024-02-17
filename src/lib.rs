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

let path = "tests/svg/custom/integration/matplotlib/time_series.svg";
let svg = std::fs::read_to_string(path)?;
let mut db = fontdb::Database::new();
db.load_system_fonts();

// This can only fail if the SVG is malformed. This one is not.
let pdf = svg2pdf::convert_str(&svg, svg2pdf::Options::default(), &db)?;

// ... and now you have a Vec<u8> which you could write to a file or
// transmit over the network!
std::fs::write("target/time_series.pdf", pdf)?;
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
- Text (although it will be converted into paths)
- Raster images and nested SVGs

## Unsupported features
Among the unsupported features are currently:
- The `spreadMethod` attribute of gradients
- Filters
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
use usvg::{fontdb, Align, AspectRatio, NonZeroRect, Size, Transform, Tree, ViewBox};

use crate::render::tree_to_stream;
use crate::util::context::Context;
use crate::util::helper::{deflate, dpi_ratio};

// The ICC profiles.
static SRGB_ICC_DEFLATED: Lazy<Vec<u8>> =
    Lazy::new(|| deflate(include_bytes!("icc/sRGB-v4.icc")));
static GRAY_ICC_DEFLATED: Lazy<Vec<u8>> =
    Lazy::new(|| deflate(include_bytes!("icc/sGrey-v4.icc")));

/// Set size and scaling preferences for the conversion.
#[derive(Copy, Clone)]
pub struct Options {
    /// Specific dimensions the SVG will be forced to fill in nominal SVG
    /// pixels. If this is `Some`, the resulting PDF will always have the
    /// corresponding size converted to PostScript points according to `dpi`. If
    /// it is `None`, the PDF will take on the native size of the SVG.
    ///
    /// Normally, unsized SVGs will take on the size of the target viewport. In
    /// order to achieve the behavior in which your SVG will take its native
    /// size and the size of your viewport only if it has no native size, you
    /// need to create a [`usvg` tree](usvg::Tree) for your file in your own
    /// code. You will then need to set the `default_size` field of the
    /// [`usvg::Options`] struct to your viewport size and set this field
    /// according to `tree.svg_node().size`.
    ///
    /// _Default:_ `None`.
    pub viewport: Option<Size>,

    /// Override the scaling mode of the SVG within its viewport. Look
    /// [here][aspect] to learn about the different possible modes.
    ///
    /// _Default:_ `None`.
    ///
    /// [aspect]: https://developer.mozilla.org/en-US/docs/Web/SVG/Attribute/preserveAspectRatio
    pub aspect: Option<AspectRatio>,

    /// The dots per inch to assume for the conversion to PDF's printer's
    /// points. Common values include `72.0` (1pt = 1px; Adobe and macOS) and
    /// `96.0` (Microsoft) for standard resolution screens and multiples of
    /// `300.0` for print quality.
    ///
    /// This, of course, does not change the output quality (except for very
    /// high values, where precision might degrade due to floating point
    /// errors). Instead, it sets what the physical dimensions of one nominal
    /// pixel should be on paper when printed without scaling.
    ///
    /// _Default:_ `72.0`.
    pub dpi: f32,

    /// Whether the content streams should be compressed.
    ///
    /// The smaller PDFs generated by this are generally more practical but it
    /// increases runtime a bit.
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
}

impl Default for Options {
    fn default() -> Self {
        Self {
            dpi: 72.0,
            viewport: None,
            aspect: None,
            compress: true,
            raster_scale: 1.0,
        }
    }
}

/// Convert an SVG source string to a standalone PDF buffer.
///
/// Returns an error if the SVG string is malformed.
pub fn convert_str(
    src: &str,
    options: Options,
    fontdb: &fontdb::Database,
) -> Result<Vec<u8>, usvg::Error> {
    let mut usvg_options = usvg::Options::default();
    if let Some(size) = options.viewport {
        usvg_options.default_size = size;
    }
    let tree = Tree::from_str(src, &usvg_options, fontdb)?;
    Ok(convert_tree(&tree, options))
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
/// use svg2pdf::Options;
///
/// let input = "tests/svg/custom/integration/matplotlib/step.svg";
/// let output = "target/step.pdf";
///
/// let svg = std::fs::read_to_string(input)?;
/// let options = svg2pdf::usvg::Options::default();
/// let mut db = fontdb::Database::new();
/// db.load_system_fonts();
/// let mut tree = svg2pdf::usvg::Tree::from_str(&svg, &options, &db)?;
///
///
/// let pdf = svg2pdf::convert_tree(&tree, Options::default());
/// std::fs::write(output, pdf)?;
/// # Ok(()) }
/// ```
pub fn convert_tree(tree: &Tree, options: Options) -> Vec<u8> {
    let pdf_size = pdf_size(tree, options);
    let mut ctx = Context::new(tree, options, None);
    let mut pdf = Pdf::new();

    let catalog_ref = ctx.alloc_ref();
    let page_tree_ref = ctx.alloc_ref();
    let page_ref = ctx.alloc_ref();
    let content_ref = ctx.alloc_ref();

    pdf.catalog(catalog_ref).pages(page_tree_ref);
    pdf.pages(page_tree_ref).count(1).kids([page_ref]);

    // Generate main content
    ctx.deferrer.push();
    let mut content = Content::new();
    tree_to_stream(
        tree,
        &mut pdf,
        &mut content,
        &mut ctx,
        initial_transform(options.aspect, tree, pdf_size),
    );
    let content_stream = ctx.finish_content(content);
    let mut stream = pdf.stream(content_ref, &content_stream);

    if ctx.options.compress {
        stream.filter(Filter::FlateDecode);
    }
    stream.finish();

    let mut page = pdf.page(page_ref);
    let mut page_resources = page.resources();
    ctx.deferrer.pop(&mut page_resources);
    page_resources.finish();

    page.media_box(Rect::new(0.0, 0.0, pdf_size.width(), pdf_size.height()));
    page.parent(page_tree_ref);
    page.group()
        .transparency()
        .isolated(true)
        .knockout(false)
        .color_space()
        .icc_based(ctx.deferrer.srgb_ref());
    page.contents(content_ref);
    page.finish();

    write_color_spaces(&mut ctx, &mut pdf);

    let document_info_id = ctx.alloc_ref();
    pdf.document_info(document_info_id).producer(TextStr("svg2pdf"));

    pdf.finish()
}

/// Convert a [`usvg` tree](Tree) into a Form XObject that can be used as
/// part of a larger document.
///
/// This method is intended for use in an existing [`pdf-writer`] workflow. It
/// will always produce an XObject with the width and height of one printer's
/// point, just like an [`ImageXObject`](pdf_writer::writers::ImageXObject)
/// would.
///
/// The resulting object can be used by registering a name and the `start_ref`
/// with a page's [`/XObject`](pdf_writer::writers::Resources::x_objects)
/// resources dictionary and then invoking the [`Do`](Content::x_object)
/// operator with the name in the page's content stream.
///
/// As the conversion process may need to create multiple indirect objects in
/// the PDF, this function allocates consecutive IDs starting at `start_ref` for
/// its objects and returns the next available ID for your future writing.
///
/// ## Example
/// Write a PDF file with some text and an SVG graphic.
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use svg2pdf;
/// use pdf_writer::{Content, Finish, Name, Pdf, Rect, Ref, Str};
/// use svg2pdf::usvg::fontdb;
///
/// // Allocate the indirect reference IDs and names.
/// let catalog_id = Ref::new(1);
/// let page_tree_id = Ref::new(2);
/// let page_id = Ref::new(3);
/// let font_id = Ref::new(4);
/// let content_id = Ref::new(5);
/// let svg_id = Ref::new(6);
/// let font_name = Name(b"F1");
/// let svg_name = Name(b"S1");
///
/// // Start writing a PDF.
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
/// // Let's add an SVG graphic to this file.
/// // We need to load its source first and manually parse it into a usvg Tree.
/// let path = "tests/svg/custom/integration/matplotlib/step.svg";
/// let svg = std::fs::read_to_string(path)?;
/// let mut db = fontdb::Database::new();
/// db.load_system_fonts();
/// let tree = svg2pdf::usvg::Tree::from_str(&svg, &svg2pdf::usvg::Options::default(), &db)?;
///
/// // Then, we will write it to the page as the 6th indirect object.
/// //
/// // This call allocates some indirect object reference IDs for itself. If we
/// // wanted to write some more indirect objects afterwards, we could use the
/// // return value as the next unused reference ID.
/// svg2pdf::convert_tree_into(&tree, svg2pdf::Options::default(), &mut pdf, svg_id);
///
/// // Write a content stream with some text and our SVG.
/// let mut content = Content::new();
/// content
///     .begin_text()
///     .set_font(font_name, 16.0)
///     .next_line(108.0, 734.0)
///     .show(Str(b"Look at my wonderful vector graphic!"))
///     .end_text();
///
/// // Add our graphic.
/// content
///     .transform([300.0, 0.0, 0.0, 225.0, 147.5, 385.0])
///     .x_object(svg_name);
///
/// // Write the file to the disk.
/// pdf.stream(content_id, &content.finish());
/// std::fs::write("target/embedded.pdf", pdf.finish())?;
/// # Ok(()) }
/// ```
pub fn convert_tree_into(
    tree: &Tree,
    options: Options,
    chunk: &mut Chunk,
    start_ref: Ref,
) -> Ref {
    let pdf_size = pdf_size(tree, options);
    let mut ctx = Context::new(tree, options, Some(start_ref.get()));

    let x_ref = ctx.alloc_ref();
    ctx.deferrer.push();

    let mut content = Content::new();
    tree_to_stream(
        tree,
        chunk,
        &mut content,
        &mut ctx,
        initial_transform(options.aspect, tree, pdf_size),
    );
    let content_stream = ctx.finish_content(content);

    let mut x_object = chunk.form_xobject(x_ref, &content_stream);
    x_object.bbox(Rect::new(0.0, 0.0, pdf_size.width(), pdf_size.height()));
    x_object.matrix([
        1.0 / pdf_size.width(),
        0.0,
        0.0,
        1.0 / pdf_size.height(),
        0.0,
        0.0,
    ]);

    if ctx.options.compress {
        x_object.filter(Filter::FlateDecode);
    }

    let mut resources = x_object.resources();
    ctx.deferrer.pop(&mut resources);

    resources.finish();
    x_object.finish();

    write_color_spaces(&mut ctx, chunk);

    ctx.alloc_ref()
}

fn write_color_spaces(ctx: &mut Context, chunk: &mut Chunk) {
    if ctx.deferrer.used_srgb() {
        chunk
            .icc_profile(ctx.deferrer.srgb_ref(), &SRGB_ICC_DEFLATED)
            .n(3)
            .range([0.0, 1.0, 0.0, 1.0, 0.0, 1.0])
            .filter(Filter::FlateDecode);
    }

    if ctx.deferrer.used_sgray() {
        chunk
            .icc_profile(ctx.deferrer.sgray_ref(), &GRAY_ICC_DEFLATED)
            .n(1)
            .range([0.0, 1.0])
            .filter(Filter::FlateDecode);
    }
}

/// Return the dimensions of the PDF page
fn pdf_size(tree: &Tree, options: Options) -> Size {
    // If no custom viewport is defined, we use the size of the tree.
    let viewport_size = options.viewport.unwrap_or(tree.size());
    Size::from_wh(
        // dpi_ratio is in dot per user unit so dividing by it gave user unit
        viewport_size.width() / dpi_ratio(options.dpi),
        viewport_size.height() / dpi_ratio(options.dpi),
    )
    .unwrap()
}

/// Return the initial transform that is necessary for the conversion between SVG coordinates
/// and the final PDF page.
fn initial_transform(
    aspect: Option<AspectRatio>,
    tree: &Tree,
    pdf_size: Size,
) -> Transform {
    // Account for the custom viewport that has been passed in the Options struct. If nothing has
    // been passed, pdf_size should be the same as tree.size, so the transform will just be the
    // default transform.
    let view_box = ViewBox {
        rect: NonZeroRect::from_xywh(0.0, 0.0, tree.size().width(), tree.size().height())
            .unwrap(),
        aspect: aspect.unwrap_or(AspectRatio {
            defer: false,
            align: Align::None,
            slice: false,
        }),
    };
    let custom_viewport_transform = view_box.to_transform(pdf_size);

    // Account for the direction of the y axis and the shift of the origin in the coordinate system.
    let pdf_transform = Transform::from_row(1.0, 0.0, 0.0, -1.0, 0.0, pdf_size.height());

    pdf_transform.pre_concat(custom_viewport_transform)
}
