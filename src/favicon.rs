use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Sender;
use gpui::{Image, ImageFormat, ImageSource, RenderImage};
use image::{Frame, ImageBuffer, Rgba};
use parking_lot::Mutex;
use smallvec::SmallVec;

use crate::synthetic_preview::{
    google_product_for_url, product_from_synthetic_url, synthetic_preview_url,
};

#[derive(Clone)]
pub enum CachedFavicon {
    Raster(Arc<RenderImage>),
    Svg(Arc<Image>),
}

impl CachedFavicon {
    pub fn to_image_source(&self) -> ImageSource {
        match self {
            CachedFavicon::Raster(render_img) => ImageSource::Render(render_img.clone()),
            CachedFavicon::Svg(svg_img) => ImageSource::Image(svg_img.clone()),
        }
    }
}

pub struct FaviconCache {
    capacity: usize,
    map: HashMap<String, CachedFavicon>,
    order: VecDeque<String>,
    in_flight: HashSet<String>,
    failed: HashSet<String>,
}

impl FaviconCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            map: HashMap::new(),
            order: VecDeque::new(),
            in_flight: HashSet::new(),
            failed: HashSet::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn get(&mut self, url: &str) -> Option<CachedFavicon> {
        let img = self.map.get(url)?.clone();
        self.touch(url);
        Some(img)
    }

    pub fn insert(&mut self, url: String, image: CachedFavicon) {
        self.in_flight.remove(&url);
        if self.map.contains_key(&url) {
            self.map.insert(url.clone(), image);
            self.touch(&url);
            return;
        }

        while self.map.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            } else {
                break;
            }
        }

        self.order.push_back(url.clone());
        self.map.insert(url, image);
    }

    pub fn mark_in_flight(&mut self, url: &str) -> bool {
        if self.map.contains_key(url)
            || self.in_flight.contains(url)
            || self.failed.contains(url)
        {
            return false;
        }
        self.in_flight.insert(url.to_string());
        true
    }

    pub fn mark_failed(&mut self, url: &str) {
        self.in_flight.remove(url);
        self.failed.insert(url.to_string());
    }

    fn touch(&mut self, url: &str) {
        if let Some(pos) = self.order.iter().position(|k| k == url) {
            self.order.remove(pos);
        }
        self.order.push_back(url.to_string());
    }
}

/// Resolve the effective favicon URL to use for a tab:
/// 1. `fav_icon_url` if present and starts with `http://`, `https://`, or `data:`
/// 2. Otherwise derive `https://<host>/favicon.ico` from `page_url` if `page_url` is HTTP(S).
pub fn effective_favicon_url(fav_icon_url: Option<&str>, page_url: &str) -> Option<String> {
    if let Some(fav) = fav_icon_url.map(str::trim).filter(|s| !s.is_empty()) {
        if fav.starts_with("http://") || fav.starts_with("https://") || fav.starts_with("data:") {
            return Some(fav.to_string());
        }
    }
    origin_favicon_url(page_url)
}

pub fn origin_favicon_url(page_url: &str) -> Option<String> {
    let trimmed = page_url.trim();
    let (scheme, rest) = if let Some(r) = trimmed.strip_prefix("https://") {
        ("https://", r)
    } else if let Some(r) = trimmed.strip_prefix("http://") {
        ("http://", r)
    } else {
        return None;
    };
    let host = rest.split(['/', '?', '#']).next()?;
    if host.is_empty() {
        return None;
    }
    Some(format!("{scheme}{host}/favicon.ico"))
}

pub fn google_s2_favicon_url(page_url: &str) -> Option<String> {
    let trimmed = page_url.trim();
    let rest = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next()?;
    if host.is_empty() || host.starts_with("127.0.0.1") || host.starts_with("localhost") {
        return None;
    }
    Some(format!(
        "https://www.google.com/s2/favicons?sz=32&domain={host}"
    ))
}

/// Extract a YouTube video thumbnail (`mqdefault.jpg`, 320x180) from a YouTube watch/shorts/youtu.be URL
/// so even unvisited background YouTube tabs show their video preview immediately.
pub fn youtube_thumbnail_url(page_url: &str) -> Option<String> {
    let trimmed = page_url.trim();
    let rest = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))?;
    let rest = rest
        .strip_prefix("www.")
        .or_else(|| rest.strip_prefix("m."))
        .unwrap_or(rest);

    let video_id = if let Some(after_host) = rest.strip_prefix("youtube.com/") {
        if let Some(query) = after_host.strip_prefix("watch?") {
            query.split('&').find_map(|pair| pair.strip_prefix("v="))
        } else if let Some(shorts) = after_host.strip_prefix("shorts/") {
            shorts.split(['/', '?', '&', '#']).next()
        } else {
            None
        }
    } else if let Some(short_link) = rest.strip_prefix("youtu.be/") {
        short_link.split(['/', '?', '&', '#']).next()
    } else {
        None
    }?;

    let clean_id: String = video_id
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if clean_id.len() < 6 {
        return None;
    }
    Some(format!("https://i.ytimg.com/vi/{clean_id}/mqdefault.jpg"))
}

/// Preview source for a tab, in priority order: a real capture from the extension, a YouTube
/// video thumbnail, or hand-drawn artwork for Google products (whose captures enterprise
/// policies commonly block). `None` means "show the favicon placeholder".
pub fn effective_preview_url(preview_url: Option<&str>, page_url: &str) -> Option<String> {
    if let Some(p) = preview_url.map(str::trim).filter(|s| !s.is_empty()) {
        if p.starts_with("data:image/") || p.starts_with("https://") || p.starts_with("http://") {
            return Some(p.to_string());
        }
    }
    youtube_thumbnail_url(page_url)
        .or_else(|| google_product_for_url(page_url).map(synthetic_preview_url))
}

pub fn decode_favicon_bytes(
    bytes: &[u8],
    content_type: Option<&str>,
    url: &str,
) -> Option<CachedFavicon> {
    if bytes.is_empty() {
        return None;
    }

    let ct_lower = content_type.unwrap_or("").to_ascii_lowercase();
    let is_svg = ct_lower.contains("image/svg")
        || url.ends_with(".svg")
        || bytes
            .windows(4)
            .take(256)
            .any(|w| w.eq_ignore_ascii_case(b"<svg"));

    if is_svg {
        return Some(CachedFavicon::Svg(Arc::new(Image::from_bytes(
            ImageFormat::Svg,
            bytes.to_vec(),
        ))));
    }

    // Decode raster image (PNG, ICO, JPEG, WEBP, GIF, BMP) directly into BGRA RenderImage
    // so GPUI renders it synchronously on the very first frame without async asset delay.
    let decoded = image::load_from_memory(bytes).ok()?;
    let resized = if decoded.width() > 720 || decoded.height() > 450 {
        let w = decoded.width();
        let h = decoded.height();
        let cropped = if w >= 900 && h >= 550 {
            decoded.crop_imm(0, 0, w, (h * 4 / 5).max(1))
        } else {
            decoded
        };
        cropped.thumbnail(640, 400)
    } else {
        decoded
    };
    let mut rgba: ImageBuffer<Rgba<u8>, Vec<u8>> = resized.into_rgba8();
    // Convert RGBA -> BGRA as expected by GPUI's Metal texture atlas
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let frames = SmallVec::from_elem(Frame::new(rgba), 1);
    Some(CachedFavicon::Raster(Arc::new(RenderImage::new(frames))))
}

pub fn decode_data_uri(data_uri: &str) -> Option<CachedFavicon> {
    let rest = data_uri.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let is_base64 = meta.ends_with(";base64");
    let mime = meta.trim_end_matches(";base64");

    if is_base64 {
        let bytes = decode_base64_simple(payload.trim())?;
        decode_favicon_bytes(&bytes, Some(mime), data_uri)
    } else {
        decode_favicon_bytes(payload.as_bytes(), Some(mime), data_uri)
    }
}

fn decode_base64_simple(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lut = [255u8; 256];
    for (i, &b) in TABLE.iter().enumerate() {
        lut[b as usize] = i as u8;
    }

    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u8;

    for b in input.bytes() {
        if b == b'=' {
            break;
        }
        if b.is_ascii_whitespace() {
            continue;
        }
        let val = lut[b as usize];
        if val == 255 {
            return None;
        }
        buf = (buf << 6) | (val as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

#[derive(Clone)]
struct FetchRequest {
    primary_url: String,
    fallback_url: Option<String>,
}

#[derive(Clone)]
pub struct FaviconService {
    cache: Arc<Mutex<FaviconCache>>,
    fetch_tx: tokio::sync::mpsc::UnboundedSender<FetchRequest>,
}

impl FaviconService {
    pub fn spawn(repaint_notify: Sender<()>) -> Self {
        let cache = Arc::new(Mutex::new(FaviconCache::new(512)));
        let (fetch_tx, mut fetch_rx) = tokio::sync::mpsc::unbounded_channel::<FetchRequest>();
        let cache_clone = cache.clone();

        std::thread::Builder::new()
            .name("favicon-fetcher".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("failed to build favicon tokio runtime");

                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(5))
                    .user_agent(
                        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
                    )
                    .build()
                    .expect("failed to build reqwest client");

                rt.block_on(async move {
                    while let Some(req) = fetch_rx.recv().await {
                        let client_ref = client.clone();
                        let cache_ref = cache_clone.clone();
                        let notify_ref = repaint_notify.clone();

                        tokio::spawn(async move {
                            if let Some(icon) =
                                fetch_and_decode(&client_ref, &req.primary_url).await
                            {
                                cache_ref.lock().insert(req.primary_url, icon);
                                let _ = notify_ref.try_send(());
                                return;
                            }

                            if let Some(fallback) = req.fallback_url.as_deref() {
                                if let Some(icon) =
                                    fetch_and_decode(&client_ref, fallback).await
                                {
                                    cache_ref.lock().insert(req.primary_url, icon);
                                    let _ = notify_ref.try_send(());
                                    return;
                                }
                            }

                            cache_ref.lock().mark_failed(&req.primary_url);
                        });
                    }
                });
            })
            .expect("failed to spawn favicon-fetcher thread");

        Self { cache, fetch_tx }
    }

    pub fn get_or_fetch(
        &self,
        fav_icon_url: Option<&str>,
        page_url: &str,
    ) -> Option<CachedFavicon> {
        let primary_url = effective_favicon_url(fav_icon_url, page_url)?;

        if primary_url.starts_with("data:") {
            let cache_key = compact_data_uri_key("fav", &primary_url);
            let mut guard = self.cache.lock();
            if let Some(cached) = guard.get(&cache_key) {
                return Some(cached);
            }
            if let Some(decoded) = decode_data_uri(&primary_url) {
                guard.insert(cache_key, decoded.clone());
                return Some(decoded);
            }
            guard.mark_failed(&cache_key);
            return None;
        }

        let mut guard = self.cache.lock();
        if let Some(img) = guard.get(&primary_url) {
            return Some(img);
        }
        if guard.mark_in_flight(&primary_url) {
            let fallback_url = google_s2_favicon_url(page_url);
            let _ = self.fetch_tx.send(FetchRequest {
                primary_url,
                fallback_url,
            });
        }
        None
    }

    pub fn get_or_fetch_preview(
        &self,
        preview_url: Option<&str>,
        page_url: &str,
    ) -> Option<CachedFavicon> {
        let target_url = effective_preview_url(preview_url, page_url)?;

        if let Some(product) = product_from_synthetic_url(&target_url) {
            let mut guard = self.cache.lock();
            if let Some(cached) = guard.get(&target_url) {
                return Some(cached);
            }
            let decoded = decode_favicon_bytes(
                product.svg().as_bytes(),
                Some("image/svg+xml"),
                &target_url,
            )?;
            guard.insert(target_url, decoded.clone());
            return Some(decoded);
        }

        if target_url.starts_with("data:") {
            let cache_key = compact_data_uri_key("prev", &target_url);
            let mut guard = self.cache.lock();
            if let Some(cached) = guard.get(&cache_key) {
                return Some(cached);
            }
            if let Some(decoded) = decode_data_uri(&target_url) {
                guard.insert(cache_key, decoded.clone());
                return Some(decoded);
            }
            guard.mark_failed(&cache_key);
            return None;
        }

        let mut guard = self.cache.lock();
        if let Some(img) = guard.get(&target_url) {
            return Some(img);
        }
        if guard.mark_in_flight(&target_url) {
            let _ = self.fetch_tx.send(FetchRequest {
                primary_url: target_url,
                fallback_url: None,
            });
        }
        None
    }

    pub fn prefetch_batch(&self, items: &[(Option<String>, String)]) {
        for (fav_url, page_url) in items {
            let _ = self.get_or_fetch(fav_url.as_deref(), page_url);
            let _ = self.get_or_fetch_preview(None, page_url);
        }
    }
}

fn compact_data_uri_key(prefix: &str, data_uri: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data_uri.hash(&mut hasher);
    format!("{prefix}:data:{}:{:016x}", data_uri.len(), hasher.finish())
}

async fn fetch_and_decode(client: &reqwest::Client, url: &str) -> Option<CachedFavicon> {
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let bytes = resp.bytes().await.ok()?;
    decode_favicon_bytes(&bytes, ct.as_deref(), url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn decodes_svg_and_raster_images_into_cached_favicon() {
        let svg_bytes = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"></svg>"#;
        let svg_img =
            decode_favicon_bytes(svg_bytes, Some("image/svg+xml"), "https://a.com/icon.svg")
                .expect("should decode svg");
        assert!(matches!(svg_img, CachedFavicon::Svg(_)));

        let buf: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(16, 16, Rgba([59, 130, 246, 255]));
        let mut ico_bytes = Vec::new();
        buf.write_to(
            &mut Cursor::new(&mut ico_bytes),
            image::ImageFormat::Ico,
        )
        .unwrap();

        let decoded_ico = decode_favicon_bytes(
            &ico_bytes,
            Some("image/x-icon"),
            "https://example.com/favicon.ico",
        )
        .expect("should decode ico into RenderImage");
        assert!(matches!(decoded_ico, CachedFavicon::Raster(_)));
    }

    #[test]
    fn lru_cache_evicts_least_recently_used_entry() {
        let mut cache = FaviconCache::new(2);
        let img1 = CachedFavicon::Svg(Arc::new(Image::from_bytes(ImageFormat::Svg, vec![1])));
        let img2 = CachedFavicon::Svg(Arc::new(Image::from_bytes(ImageFormat::Svg, vec![2])));
        let img3 = CachedFavicon::Svg(Arc::new(Image::from_bytes(ImageFormat::Svg, vec![3])));

        cache.insert("https://a.com/favicon.ico".into(), img1);
        cache.insert("https://b.com/favicon.ico".into(), img2);

        assert!(cache.get("https://a.com/favicon.ico").is_some());

        cache.insert("https://c.com/favicon.ico".into(), img3);
        assert_eq!(cache.len(), 2);
        assert!(cache.get("https://a.com/favicon.ico").is_some());
        assert!(cache.get("https://b.com/favicon.ico").is_none());
        assert!(cache.get("https://c.com/favicon.ico").is_some());
    }

    #[tokio::test]
    async fn favicon_service_fetches_http_favicon_and_notifies_repaint() {
        let buf: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(16, 16, Rgba([255, 0, 0, 255]));
        let mut png_bytes = Vec::new();
        buf.write_to(
            &mut Cursor::new(&mut png_bytes),
            image::ImageFormat::Png,
        )
        .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = png_bytes.clone();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut req_buf = [0u8; 1024];
                let _ = socket.read(&mut req_buf).await;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            }
        });

        let (repaint_tx, repaint_rx) = crossbeam_channel::unbounded();
        let service = FaviconService::spawn(repaint_tx);
        let fav_url = format!("http://{addr}/favicon.png");
        let page_url = format!("http://{addr}/index.html");

        // First call initiates async fetch and returns None
        assert!(service.get_or_fetch(Some(&fav_url), &page_url).is_none());

        // Wait for repaint notification
        tokio::task::spawn_blocking(move || {
            repaint_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("favicon service must notify repaint after fetching")
        })
        .await
        .unwrap();

        // Second call returns cached RenderImage immediately
        let cached = service.get_or_fetch(Some(&fav_url), &page_url);
        assert!(cached.is_some());
    }

    #[test]
    fn derives_youtube_thumbnail_url_from_watch_shorts_and_youtu_be() {
        assert_eq!(
            youtube_thumbnail_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=42s"),
            Some("https://i.ytimg.com/vi/dQw4w9WgXcQ/mqdefault.jpg".to_string())
        );
        assert_eq!(
            youtube_thumbnail_url("https://youtu.be/dQw4w9WgXcQ?si=123"),
            Some("https://i.ytimg.com/vi/dQw4w9WgXcQ/mqdefault.jpg".to_string())
        );
        assert_eq!(
            youtube_thumbnail_url("https://www.youtube.com/shorts/abc123XYZ_-"),
            Some("https://i.ytimg.com/vi/abc123XYZ_-/mqdefault.jpg".to_string())
        );
        assert_eq!(youtube_thumbnail_url("https://github.com/org/repo"), None);
    }
}
