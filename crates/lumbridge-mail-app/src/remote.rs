//! Images for the reader: the one network call the app makes on its own, and only after the person
//! pressed "Load images" (for this message, or for a sender they allowed).
//!
//! [`Http`] fetches with no cookies (ureq's cookie store is not compiled in), no `Referer`, no
//! credentials, a generic user agent, 3 redirects at most, 5 s to connect and 15 s in all, and at most
//! [`MAX_IMAGE_BYTES`]. It refuses addresses on this machine or the local network (a mail cannot make
//! the app poke the router), and anything but `http`/`https`.
//!
//! Every image, remote or an inline `cid:` part, goes through [`decode`] before GPUI sees it: only PNG,
//! JPEG, GIF and WebP (never SVG), and the header must declare a sane size, so a hostile file cannot
//! make the decoder allocate gigabytes.

use gpui::{Image, ImageFormat};
use std::io::Read;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

pub const MAX_IMAGE_BYTES: u64 = 5 << 20;
pub const MAX_SIDE: u32 = 12_000;
pub const MAX_PIXELS: u64 = 40_000_000;
/// Remote images fetched per message at most.
pub const MAX_IMAGES_PER_MESSAGE: usize = 40;

pub trait Fetcher: Send + Sync {
    /// The bytes at `url`, or why not. Blocking: call it from a background task.
    fn fetch(&self, url: &str) -> Result<Vec<u8>, String>;
}

/// A decoded-enough image: GPUI decodes the bytes itself when it paints.
#[derive(Clone)]
pub struct Picture {
    pub image: Arc<Image>,
    pub width: u32,
    pub height: u32,
}

impl std::fmt::Debug for Picture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Picture({}x{})", self.width, self.height)
    }
}

fn sniff(bytes: &[u8]) -> Option<(ImageFormat, image::ImageFormat)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some((ImageFormat::Png, image::ImageFormat::Png))
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some((ImageFormat::Jpeg, image::ImageFormat::Jpeg))
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some((ImageFormat::Gif, image::ImageFormat::Gif))
    } else if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some((ImageFormat::Webp, image::ImageFormat::WebP))
    } else {
        None
    }
}

/// Check an image's format and declared size; hand it to GPUI only if both are sane.
pub fn decode(bytes: Vec<u8>) -> Result<Picture, String> {
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err("image too large".into());
    }
    let (format, ifmt) = sniff(&bytes).ok_or("not a PNG, JPEG, GIF or WebP image")?;
    let reader = image::ImageReader::with_format(std::io::Cursor::new(&bytes), ifmt);
    let (width, height) = reader.into_dimensions().map_err(|e| format!("unreadable image: {e}"))?;
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(format!("image size {width}x{height} refused"));
    }
    Ok(Picture { image: Arc::new(Image::from_bytes(format, bytes)), width, height })
}

/// A `data:image/...;base64,` URI's bytes (the render crate has already checked type and size).
pub fn data_uri_bytes(uri: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let (_, b64) = uri.split_once(";base64,")?;
    base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok()
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast()
                || v4.is_multicast() || v4.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (64..128).contains(&o[1])) // CGNAT, including tailnets
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19)))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_loopback() || v6.is_unspecified() || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00   // unique local
                || (s[0] & 0xffc0) == 0xfe80   // link local
                || s[0] == 0x2001 && s[1] == 0x0db8)
        }
    }
}

/// Resolves names and drops every address that is not on the public internet.
struct PublicOnly;

impl ureq::Resolver for PublicOnly {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<SocketAddr>> {
        let addrs: Vec<SocketAddr> = netloc.to_socket_addrs()?.filter(|a| is_public(a.ip())).collect();
        if addrs.is_empty() {
            return Err(std::io::Error::other("refused: not a public address"));
        }
        Ok(addrs)
    }
}

struct Anything;

impl ureq::Resolver for Anything {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<SocketAddr>> {
        Ok(netloc.to_socket_addrs()?.collect())
    }
}

pub struct Http {
    agent: ureq::Agent,
}

impl Default for Http {
    fn default() -> Self {
        Self::new()
    }
}

impl Http {
    pub fn new() -> Self {
        Self::build(false)
    }

    /// Tests only: allow loopback so a local server can stand in for a mail sender's CDN.
    #[doc(hidden)]
    pub fn allowing_local_addresses() -> Self {
        Self::build(true)
    }

    fn build(local: bool) -> Self {
        let b = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .redirects(3)
            .max_idle_connections(0)
            .user_agent("Mozilla/5.0");
        let b = if local { b.resolver(Anything) } else { b.resolver(PublicOnly) };
        Self { agent: b.build() }
    }
}

impl Fetcher for Http {
    fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        let lower = url.to_ascii_lowercase();
        if !(lower.starts_with("https://") || lower.starts_with("http://")) {
            return Err("only http and https images are loaded".into());
        }
        let resp = self.agent.get(url).set("Accept", "image/png,image/jpeg,image/gif,image/webp").call()
            .map_err(|e| match e {
                ureq::Error::Status(code, _) => format!("the server said {code}"),
                ureq::Error::Transport(t) => t.to_string(),
            })?;
        if let Some(len) = resp.header("Content-Length").and_then(|l| l.trim().parse::<u64>().ok())
            && len > MAX_IMAGE_BYTES
        {
            return Err("image too large".into());
        }
        let mut bytes = Vec::new();
        resp.into_reader().take(MAX_IMAGE_BYTES + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err("image too large".into());
        }
        Ok(bytes)
    }
}

/// Made-up pictures for tests and screenshots: a soft gradient per URL, no network.
pub struct Painted;

impl Painted {
    /// A small made-up landscape (sky, sun, two hills), coloured by `seed`.
    pub fn png(width: u32, height: u32, seed: u64) -> Vec<u8> {
        let pick = |k: u64| (seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(k as u32 * 7) % 360) as f32;
        let hue = pick(1);
        let hsl = |h: f32, s: f32, l: f32| -> [u8; 3] {
            let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
            let hp = (h.rem_euclid(360.0)) / 60.0;
            let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
            let (r, g, b) = match hp as u32 {
                0 => (c, x, 0.0),
                1 => (x, c, 0.0),
                2 => (0.0, c, x),
                3 => (0.0, x, c),
                4 => (x, 0.0, c),
                _ => (c, 0.0, x),
            };
            let m = l - c / 2.0;
            [((r + m) * 255.0) as u8, ((g + m) * 255.0) as u8, ((b + m) * 255.0) as u8]
        };
        let (w, h) = (width.max(1) as f32, height.max(1) as f32);
        let sun = (w * (0.25 + (pick(2) % 50.0) / 100.0), h * 0.32, h.min(w) * 0.12);
        let img = image::RgbaImage::from_fn(width, height, |x, y| {
            let (fx, fy) = (x as f32 / w, y as f32 / h);
            let hill1 = 0.62 + 0.08 * (fx * 6.0 + pick(3) / 60.0).sin();
            let hill2 = 0.76 + 0.06 * (fx * 9.0 + pick(4) / 50.0).sin();
            let c = if fy > hill2 {
                hsl(hue + 140.0, 0.35, 0.30 + 0.05 * fy)
            } else if fy > hill1 {
                hsl(hue + 120.0, 0.30, 0.45)
            } else {
                let d = ((x as f32 - sun.0).powi(2) + (y as f32 - sun.1).powi(2)).sqrt();
                if d < sun.2 { hsl(hue + 40.0, 0.85, 0.75) } else { hsl(hue + 200.0, 0.45, 0.80 - 0.25 * fy) }
            };
            image::Rgba([c[0], c[1], c[2], 255])
        });
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).expect("encode png");
        out.into_inner()
    }
}

impl Fetcher for Painted {
    fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        let seed = url.bytes().fold(1469598103934665603u64, |h, b| (h ^ u64::from(b)).wrapping_mul(1099511628211));
        // Sizes from the URL when it says (`.../600x300/...`), else a banner.
        let (w, h) = url
            .split('/')
            .find_map(|part| part.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))))
            .unwrap_or((600, 240));
        Ok(Self::png(w, h, seed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    #[test]
    fn only_sane_raster_images_pass() {
        let png = Painted::png(40, 20, 7);
        let p = decode(png).unwrap();
        assert_eq!((p.width, p.height), (40, 20));
        assert!(decode(b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec()).is_err(), "no SVG from mail");
        assert!(decode(b"GIF89a".to_vec()).is_err(), "truncated");
        // A PNG header that claims 100000 x 100000 pixels.
        let mut bomb = Painted::png(1, 1, 1);
        bomb[16..20].copy_from_slice(&100_000u32.to_be_bytes());
        bomb[20..24].copy_from_slice(&100_000u32.to_be_bytes());
        assert!(decode(bomb).is_err());
        assert_eq!(data_uri_bytes("data:image/png;base64,aGk="), Some(b"hi".to_vec()));
    }

    #[test]
    fn local_and_private_addresses_are_refused() {
        for ip in ["127.0.0.1", "10.1.2.3", "192.168.0.1", "169.254.1.1", "100.64.0.1", "::1", "fd00::1", "fe80::1", "0.0.0.0"] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        assert!(is_public("93.184.216.34".parse().unwrap()));
        let err = Http::new().fetch("http://127.0.0.1:9/x.png").unwrap_err();
        assert!(err.contains("refused"), "{err}");
        assert!(Http::new().fetch("file:///etc/passwd").is_err());
        assert!(Http::new().fetch("ftp://x.example/a.png").is_err());
    }

    /// A one-shot HTTP server on loopback; returns what the client sent.
    fn serve(body: Vec<u8>) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/img.png", listener.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut req = String::new();
            let mut r = BufReader::new(s.try_clone().unwrap());
            loop {
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                req.push_str(&line);
            }
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nSet-Cookie: id=1\r\nConnection: close\r\n\r\n");
            let _ = s.write_all(&body);
            req
        });
        (url, h)
    }

    #[test]
    fn requests_carry_no_cookies_or_referrer_and_are_capped() {
        let png = Painted::png(8, 8, 3);
        let (url, server) = serve(png.clone());
        let got = Http::allowing_local_addresses().fetch(&url).unwrap();
        assert_eq!(got, png);
        let req = server.join().unwrap().to_ascii_lowercase();
        assert!(!req.contains("cookie") && !req.contains("referer") && !req.contains("authorization"), "{req}");
        assert!(req.contains("user-agent: mozilla/5.0"), "{req}");

        let (url, server) = serve(vec![0u8; (MAX_IMAGE_BYTES + 10) as usize]);
        assert_eq!(Http::allowing_local_addresses().fetch(&url).unwrap_err(), "image too large");
        let _ = server.join();
    }
}
