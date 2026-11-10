//! Dev tool: rasterize every synthetic Google-product preview into one contact sheet so the
//! artwork in `src/synthetic_previews/` can be reviewed exactly as GPUI will draw it.
//!
//! Ignored by default (it writes a file). Run with:
//!
//! ```sh
//! cargo test --test contact_sheet -- --ignored --nocapture
//! open target/synthetic_previews_contact_sheet.png
//! ```
use chrome_tab_switcher::synthetic_preview::GoogleProduct;
use resvg::tiny_skia::{Color, Pixmap, PixmapPaint, Transform};
use resvg::usvg::{Options, Tree};

#[test]
#[ignore = "writes target/synthetic_previews_contact_sheet.png for visual review"]
fn render_contact_sheet() {
    let cols = 4usize;
    let (w, h, pad) = (266u32, 156u32, 12u32);
    let rows = GoogleProduct::ALL.len().div_ceil(cols) as u32;
    let mut sheet = Pixmap::new(cols as u32 * (w + pad) + pad, rows * (h + pad) + pad).unwrap();
    sheet.fill(Color::from_rgba8(0x0F, 0x17, 0x2A, 255));

    for (i, product) in GoogleProduct::ALL.iter().enumerate() {
        let tree = Tree::from_data(product.svg().as_bytes(), &Options::default()).unwrap();
        let scale = w as f32 / tree.size().width();
        let mut card = Pixmap::new(w, h).unwrap();
        resvg::render(
            &tree,
            Transform::from_scale(scale, scale),
            &mut card.as_mut(),
        );
        let x = pad + (i % cols) as u32 * (w + pad);
        let y = pad + (i / cols) as u32 * (h + pad);
        sheet.draw_pixmap(
            x as i32,
            y as i32,
            card.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
    }

    let out = std::env::var("CONTACT_SHEET_OUT")
        .unwrap_or_else(|_| "target/synthetic_previews_contact_sheet.png".into());
    sheet.save_png(&out).unwrap();
    println!("wrote {out}");
}
