//! Hand-drawn "fake" page previews for Google products whose real screenshots Chrome refuses
//! to capture (enterprise `ExtensionSettings` policies block `captureVisibleTab` on
//! `*.google.com` for many managed browsers). Drawn in the same flat, text-free style as the
//! README artwork, at the card's 266×156 aspect, and used only when no real capture exists.

/// A Google product with bespoke artwork in `synthetic_previews/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GoogleProduct {
    Gmail,
    Calendar,
    Maps,
    Drive,
    Docs,
    Sheets,
    Slides,
    Forms,
    Meet,
    Chat,
    Keep,
    Photos,
    Search,
    Translate,
    News,
    Gemini,
    Cloud,
    Contacts,
}

impl GoogleProduct {
    pub const ALL: [GoogleProduct; 18] = [
        Self::Gmail,
        Self::Calendar,
        Self::Maps,
        Self::Drive,
        Self::Docs,
        Self::Sheets,
        Self::Slides,
        Self::Forms,
        Self::Meet,
        Self::Chat,
        Self::Keep,
        Self::Photos,
        Self::Search,
        Self::Translate,
        Self::News,
        Self::Gemini,
        Self::Cloud,
        Self::Contacts,
    ];

    /// Stable identifier used in cache keys (`synthetic:<key>`).
    pub fn key(self) -> &'static str {
        match self {
            Self::Gmail => "gmail",
            Self::Calendar => "calendar",
            Self::Maps => "maps",
            Self::Drive => "drive",
            Self::Docs => "docs",
            Self::Sheets => "sheets",
            Self::Slides => "slides",
            Self::Forms => "forms",
            Self::Meet => "meet",
            Self::Chat => "chat",
            Self::Keep => "keep",
            Self::Photos => "photos",
            Self::Search => "search",
            Self::Translate => "translate",
            Self::News => "news",
            Self::Gemini => "gemini",
            Self::Cloud => "cloud",
            Self::Contacts => "contacts",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.key() == key)
    }

    /// SVG markup (266×156 viewBox, declared at 2× so GPUI rasterizes crisp Retina output).
    pub fn svg(self) -> &'static str {
        match self {
            Self::Gmail => include_str!("synthetic_previews/gmail.svg"),
            Self::Calendar => include_str!("synthetic_previews/calendar.svg"),
            Self::Maps => include_str!("synthetic_previews/maps.svg"),
            Self::Drive => include_str!("synthetic_previews/drive.svg"),
            Self::Docs => include_str!("synthetic_previews/docs.svg"),
            Self::Sheets => include_str!("synthetic_previews/sheets.svg"),
            Self::Slides => include_str!("synthetic_previews/slides.svg"),
            Self::Forms => include_str!("synthetic_previews/forms.svg"),
            Self::Meet => include_str!("synthetic_previews/meet.svg"),
            Self::Chat => include_str!("synthetic_previews/chat.svg"),
            Self::Keep => include_str!("synthetic_previews/keep.svg"),
            Self::Photos => include_str!("synthetic_previews/photos.svg"),
            Self::Search => include_str!("synthetic_previews/search.svg"),
            Self::Translate => include_str!("synthetic_previews/translate.svg"),
            Self::News => include_str!("synthetic_previews/news.svg"),
            Self::Gemini => include_str!("synthetic_previews/gemini.svg"),
            Self::Cloud => include_str!("synthetic_previews/cloud.svg"),
            Self::Contacts => include_str!("synthetic_previews/contacts.svg"),
        }
    }
}

/// Pseudo-URL scheme used in the preview cache so synthetic art shares the favicon/preview
/// cache and the `effective_preview_url` fallback chain with real captures.
pub const SYNTHETIC_SCHEME: &str = "synthetic:";

pub fn synthetic_preview_url(product: GoogleProduct) -> String {
    format!("{SYNTHETIC_SCHEME}{}", product.key())
}

pub fn product_from_synthetic_url(url: &str) -> Option<GoogleProduct> {
    url.strip_prefix(SYNTHETIC_SCHEME)
        .and_then(GoogleProduct::from_key)
}

/// Pick the artwork for a page, or `None` if it isn't one of the Google apps we have art for.
///
/// Matches `google.<tld>` and its subdomains (`mail.google.com`, `www.google.co.uk`, …) plus the
/// workspace editors living under `docs.google.com/<kind>` and `google.com/maps`. Deliberately
/// strict: a Google host we don't recognise (Chromium infra, Web Store, Groups, Sites, Support,
/// Accounts, docs/marketing sites, …) gets no fake page — a wrong preview is worse than the
/// honest favicon placeholder.
pub fn google_product_for_url(page_url: &str) -> Option<GoogleProduct> {
    let (host, path) = split_host_path(page_url)?;
    let host = host.to_ascii_lowercase();
    let path = path.to_ascii_lowercase();

    let sub = google_subdomain(&host)?;
    let is_home_or_search = path.is_empty()
        || path == "/"
        || ["/search", "/webhp", "/imghp", "/videohp"]
            .iter()
            .any(|p| path.starts_with(p));

    Some(match sub {
        "mail" if path.starts_with("/chat") => GoogleProduct::Chat,
        "mail" => GoogleProduct::Gmail,
        "calendar" => GoogleProduct::Calendar,
        "maps" => GoogleProduct::Maps,
        "drive" => GoogleProduct::Drive,
        "docs" if path.starts_with("/spreadsheets") => GoogleProduct::Sheets,
        "docs" if path.starts_with("/presentation") => GoogleProduct::Slides,
        "docs" if path.starts_with("/forms") => GoogleProduct::Forms,
        "docs" => GoogleProduct::Docs,
        "sheets" => GoogleProduct::Sheets,
        "slides" => GoogleProduct::Slides,
        "forms" => GoogleProduct::Forms,
        "meet" => GoogleProduct::Meet,
        "chat" => GoogleProduct::Chat,
        "keep" => GoogleProduct::Keep,
        "photos" => GoogleProduct::Photos,
        "translate" => GoogleProduct::Translate,
        "news" => GoogleProduct::News,
        "gemini" | "bard" => GoogleProduct::Gemini,
        "contacts" => GoogleProduct::Contacts,
        // Only the console looks like the console; `cloud.google.com` is docs/marketing.
        "console.cloud" => GoogleProduct::Cloud,
        "" | "www" if path.starts_with("/maps") => GoogleProduct::Maps,
        "" | "www" if is_home_or_search => GoogleProduct::Search,
        _ => return None,
    })
}

/// `"https://mail.google.com/mail/u/0/#inbox"` → `("mail.google.com", "/mail/u/0/")`.
fn split_host_path(url: &str) -> Option<(&str, &str)> {
    let trimmed = url.trim();
    let scheme_len = ["https://", "http://"]
        .iter()
        .find(|s| trimmed.len() >= s.len() && trimmed[..s.len()].eq_ignore_ascii_case(s))
        .map(|s| s.len())?;
    let rest = &trimmed[scheme_len..];
    let rest = rest.split(['#', '?']).next().unwrap_or(rest);
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    Some((host, path))
}

/// Everything left of `google.<tld>`: `"mail"` for `mail.google.com`, `"console.cloud"` for
/// `console.cloud.google.com`, `""` for the bare apex, or `None` for non-Google hosts.
fn google_subdomain(host: &str) -> Option<&str> {
    let labels: Vec<&str> = host.split('.').collect();
    let google_idx = labels.iter().position(|l| *l == "google")?;
    // Everything right of "google" must look like a public suffix: com, co.uk, com.au, de, …
    let suffix = &labels[google_idx + 1..];
    if suffix.is_empty()
        || suffix.len() > 2
        || !suffix
            .iter()
            .all(|l| !l.is_empty() && l.len() <= 3 && l.chars().all(|c| c.is_ascii_alphabetic()))
    {
        return None;
    }
    if google_idx == 0 {
        return Some("");
    }
    // Byte length of "google.<suffix>" plus the dot that joins it to the subdomain.
    let tail_len: usize = labels[google_idx..].iter().map(|l| l.len() + 1).sum();
    Some(&host[..host.len() - tail_len])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_google_urls_to_products() {
        use GoogleProduct::*;
        let cases = [
            ("https://mail.google.com/mail/u/0/#inbox", Some(Gmail)),
            ("https://mail.google.com/chat/u/0/#chat/home", Some(Chat)),
            (
                "https://calendar.google.com/calendar/u/0/r/week",
                Some(Calendar),
            ),
            ("https://www.google.com/maps/place/Seattle", Some(Maps)),
            ("https://maps.google.com/?q=coffee", Some(Maps)),
            ("https://drive.google.com/drive/my-drive", Some(Drive)),
            ("https://docs.google.com/document/d/abc/edit", Some(Docs)),
            (
                "https://docs.google.com/spreadsheets/d/abc/edit#gid=0",
                Some(Sheets),
            ),
            (
                "https://docs.google.com/presentation/d/abc/edit",
                Some(Slides),
            ),
            ("https://docs.google.com/forms/d/abc/edit", Some(Forms)),
            ("https://docs.google.com/", Some(Docs)),
            ("https://meet.google.com/abc-defg-hij", Some(Meet)),
            ("https://chat.google.com/room/x", Some(Chat)),
            ("https://keep.google.com/u/0/", Some(Keep)),
            ("https://photos.google.com/album/x", Some(Photos)),
            ("https://www.google.com/search?q=gpui", Some(Search)),
            ("https://www.google.com/", Some(Search)),
            ("https://google.com", Some(Search)),
            ("https://www.google.com/webhp?hl=en", Some(Search)),
            ("https://www.google.co.uk/search?q=tea", Some(Search)),
            ("https://translate.google.com/?sl=en&tl=fr", Some(Translate)),
            ("https://news.google.com/home", Some(News)),
            ("https://gemini.google.com/app", Some(Gemini)),
            (
                "https://console.cloud.google.com/run?project=x",
                Some(Cloud),
            ),
            ("https://cloud.google.com/pricing", None),
            ("https://contacts.google.com/", Some(Contacts)),
            ("HTTPS://MAIL.GOOGLE.COM/MAIL/", Some(Gmail)),
            // Google hosts we have no art for: no fake page, keep the favicon placeholder
            ("https://accounts.google.com/signin", None),
            ("https://analytics.google.com/analytics/web/", None),
            ("https://chromewebstore.google.com/detail/x", None),
            ("https://chrome.google.com/webstore", None),
            ("https://chromium.googlesource.com/chromium/src", None),
            ("https://groups.google.com/g/x", None),
            ("https://sites.google.com/view/x", None),
            ("https://support.google.com/chrome/answer/1", None),
            ("https://developers.google.com/maps", None),
            ("https://www.google.com/chrome/", None),
            ("https://www.google.com/intl/en/about/", None),
            // Not Google
            ("https://github.com/google/gvisor", None),
            ("https://google.example.com/", None),
            ("https://notgoogle.com/", None),
            ("https://www.youtube.com/watch?v=x", None),
            ("chrome://extensions", None),
            ("", None),
        ];
        for (url, expected) in cases {
            assert_eq!(google_product_for_url(url), expected, "{url}");
        }
    }

    #[test]
    fn synthetic_url_round_trips_through_cache_key() {
        for product in GoogleProduct::ALL {
            let url = synthetic_preview_url(product);
            assert!(url.starts_with(SYNTHETIC_SCHEME));
            assert_eq!(product_from_synthetic_url(&url), Some(product));
        }
        assert_eq!(product_from_synthetic_url("synthetic:nope"), None);
        assert_eq!(product_from_synthetic_url("https://x"), None);
    }

    #[test]
    fn every_product_has_card_sized_text_free_artwork_that_rasterizes() {
        for product in GoogleProduct::ALL {
            let svg = product.svg();
            assert!(
                svg.contains(r#"viewBox="0 0 266 156""#),
                "{}: artwork must match the card's 266x156 screenshot area",
                product.key()
            );
            assert!(
                !svg.contains("<text"),
                "{}: no <text> — the daemon's SVG renderer has no fonts",
                product.key()
            );

            // Render with the same resvg GPUI uses and make sure something non-trivial is drawn.
            let tree =
                resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
                    .unwrap_or_else(|e| panic!("{}: invalid SVG: {e}", product.key()));
            let mut pixmap = resvg::tiny_skia::Pixmap::new(266, 156).unwrap();
            let scale = 266.0 / tree.size().width();
            resvg::render(
                &tree,
                resvg::tiny_skia::Transform::from_scale(scale, scale),
                &mut pixmap.as_mut(),
            );
            // tiny-skia pixmaps are premultiplied RGBA8, 4 bytes per pixel.
            let pixels = pixmap.pixels();
            let distinct: std::collections::HashSet<u32> = pixels
                .iter()
                .map(|p| u32::from_le_bytes([p.red(), p.green(), p.blue(), p.alpha()]))
                .collect();
            assert!(
                distinct.len() >= 8,
                "{}: rendered to only {} distinct colours — looks blank",
                product.key(),
                distinct.len()
            );
            let transparent = pixels.iter().filter(|p| p.alpha() == 0).count();
            assert_eq!(
                transparent,
                0,
                "{}: card art must be fully opaque",
                product.key()
            );
        }
    }
}
