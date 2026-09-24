//! Placeholder ↔ real value swapping for one host's secrets: injected into
//! request headers and URIs, scrubbed back out of responses.
//!
//! Injection is limited to places a credential goes, because anywhere else
//! the host might keep the value and hand it to someone: a placeholder used
//! as a file name in a GitHub contents URL, or inside Dropbox's
//! `Dropbox-API-Arg` header, would otherwise store the real token.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use bytes::Bytes;
use http::{HeaderName, HeaderValue};
use hyper::body::{Body, Frame, SizeHint};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS, NON_ALPHANUMERIC};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};

/// Characters a client would have escaped had it put the real value in a path.
const PATH: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');
/// Query values: everything but the unreserved characters.
const QUERY: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

type Pairs = Vec<(Vec<u8>, Vec<u8>)>;

/// Header names that carry credentials: `Authorization`, and any name with
/// one of these in it (`x-api-key`, `PRIVATE-TOKEN`, `x-auth-token`, `Cookie`…).
const CREDENTIAL_HEADER_PARTS: &[&str] = &[
    "auth",
    "key",
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "cookie",
];

pub(crate) fn is_credential_header(name: &HeaderName) -> bool {
    let name = name.as_str(); // always lowercase
    CREDENTIAL_HEADER_PARTS.iter().any(|p| name.contains(p))
}

#[derive(Debug)]
pub(crate) struct Swaps {
    /// (placeholder, real), longest placeholder first.
    inject: Pairs,
    /// The subset of `inject` allowed in the URL.
    inject_url: Pairs,
    /// (real, placeholder), longest real value first.
    scrub: Pairs,
}

impl Swaps {
    /// `pairs` is (placeholder, real, allowed in the URL); empty values are
    /// the caller's bug.
    pub fn new(pairs: impl IntoIterator<Item = (String, String, bool)>) -> Self {
        let pairs: Vec<(Vec<u8>, Vec<u8>, bool)> = pairs
            .into_iter()
            .map(|(p, r, url)| (p.into_bytes(), r.into_bytes(), url))
            .filter(|(p, r, _)| !p.is_empty() && !r.is_empty())
            .collect();
        let longest_first = |mut v: Pairs| {
            v.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
            v
        };
        Self {
            inject: longest_first(
                pairs
                    .iter()
                    .map(|(p, r, _)| (p.clone(), r.clone()))
                    .collect(),
            ),
            inject_url: longest_first(
                pairs
                    .iter()
                    .filter(|(_, _, url)| *url)
                    .map(|(p, r, _)| (p.clone(), r.clone()))
                    .collect(),
            ),
            scrub: longest_first(
                pairs
                    .iter()
                    .map(|(p, r, _)| (r.clone(), p.clone()))
                    .collect(),
            ),
        }
    }

    /// Whether swapping keeps every length, so `Content-Length` stays right.
    pub fn same_lengths(&self) -> bool {
        self.inject.iter().all(|(p, r)| p.len() == r.len())
    }

    pub fn inject(&self, s: &[u8]) -> Option<Vec<u8>> {
        replace_all(s, &self.inject)
    }

    pub fn scrub(&self, s: &[u8]) -> Option<Vec<u8>> {
        replace_all(s, &self.scrub)
    }

    /// A credential header with placeholders swapped for real values,
    /// including inside `Basic` credentials (git over HTTPS sends
    /// `user:token` that way). `None` for any other header, when nothing
    /// changed, or when the result isn't a valid header value.
    pub fn inject_header(&self, name: &HeaderName, value: &HeaderValue) -> Option<HeaderValue> {
        if !is_credential_header(name) {
            return None;
        }
        let raw = value.as_bytes();
        let mut out = self.inject(raw);
        let current = out.as_deref().unwrap_or(raw);
        if current.len() > 6 && current[..6].eq_ignore_ascii_case(b"basic ") {
            let encoded = current[6..].trim_ascii();
            if let Some(creds) = STANDARD.decode(encoded).ok().and_then(|d| self.inject(&d)) {
                out = Some(format!("Basic {}", STANDARD.encode(creds)).into_bytes());
            }
        }
        let mut header = HeaderValue::from_bytes(&out?).ok()?;
        header.set_sensitive(true);
        Some(header)
    }

    /// A request's path-and-query with the URL-allowed placeholders swapped,
    /// the real values percent-encoded the way a client would have sent them.
    pub fn inject_uri(&self, path_and_query: &str) -> Option<String> {
        if self.inject_url.is_empty() {
            return None;
        }
        let (path, query) = match path_and_query.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (path_and_query, None),
        };
        let encoded = |set: &'static AsciiSet| -> Pairs {
            self.inject_url
                .iter()
                .map(|(p, r)| {
                    let r = String::from_utf8_lossy(r);
                    (
                        p.clone(),
                        utf8_percent_encode(&r, set).to_string().into_bytes(),
                    )
                })
                .collect()
        };
        let new_path = replace_all(path.as_bytes(), &encoded(PATH));
        let new_query = query.and_then(|q| replace_all(q.as_bytes(), &encoded(QUERY)));
        if new_path.is_none() && new_query.is_none() {
            return None;
        }
        let mut out = new_path.unwrap_or_else(|| path.as_bytes().to_vec());
        if let Some(q) = query {
            out.push(b'?');
            out.extend(new_query.unwrap_or_else(|| q.as_bytes().to_vec()));
        }
        String::from_utf8(out).ok()
    }
}

fn replace_all(hay: &[u8], pairs: &Pairs) -> Option<Vec<u8>> {
    let mut out: Option<Vec<u8>> = None;
    let (mut i, mut last) = (0, 0);
    while i < hay.len() {
        if let Some((from, to)) = pairs.iter().find(|(f, _)| hay[i..].starts_with(f)) {
            let buf = out.get_or_insert_with(|| Vec::with_capacity(hay.len()));
            buf.extend_from_slice(&hay[last..i]);
            buf.extend_from_slice(to);
            i += from.len();
            last = i;
        } else {
            i += 1;
        }
    }
    let mut buf = out?;
    buf.extend_from_slice(&hay[last..]);
    Some(buf)
}

/// Streaming real → placeholder replacement. Holds back the last
/// `longest - 1` bytes of each chunk, since a secret can straddle two chunks.
pub(crate) struct Scrubber {
    swaps: Arc<Swaps>,
    first: [bool; 256],
    keep: usize,
    carry: Vec<u8>,
}

impl Scrubber {
    pub fn new(swaps: Arc<Swaps>) -> Self {
        let mut first = [false; 256];
        for (real, _) in &swaps.scrub {
            first[real[0] as usize] = true;
        }
        let keep = swaps.scrub.first().map_or(0, |(real, _)| real.len() - 1);
        Self {
            swaps,
            first,
            keep,
            carry: Vec::new(),
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.run(chunk, false)
    }

    pub fn finish(&mut self) -> Vec<u8> {
        self.run(&[], true)
    }

    fn run(&mut self, chunk: &[u8], end: bool) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(chunk);
        // Every position before `limit` has room for the longest secret, so a
        // match starting there is complete or isn't one.
        let limit = if end {
            buf.len()
        } else {
            buf.len().saturating_sub(self.keep)
        };
        let mut out = Vec::with_capacity(buf.len());
        let (mut i, mut last) = (0, 0);
        while i < limit {
            if self.first[buf[i] as usize] {
                if let Some((real, ph)) = self
                    .swaps
                    .scrub
                    .iter()
                    .find(|(r, _)| buf[i..].starts_with(r))
                {
                    out.extend_from_slice(&buf[last..i]);
                    out.extend_from_slice(ph);
                    i += real.len();
                    last = i;
                    continue;
                }
            }
            i += 1;
        }
        let cut = i.min(buf.len());
        out.extend_from_slice(&buf[last..cut]);
        self.carry = buf[cut..].to_vec();
        out
    }
}

/// A response body run through a [`Scrubber`]; trailers pass through after
/// the held-back tail is flushed.
pub(crate) struct Scrubbed<B> {
    inner: B,
    scrubber: Scrubber,
    trailers: Option<http::HeaderMap>,
    done: bool,
}

impl<B> Scrubbed<B> {
    pub fn new(inner: B, swaps: Arc<Swaps>) -> Self {
        Self {
            inner,
            scrubber: Scrubber::new(swaps),
            trailers: None,
            done: false,
        }
    }
}

impl<B> Body for Scrubbed<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = &mut *self;
        loop {
            if let Some(trailers) = this.trailers.take() {
                return Poll::Ready(Some(Ok(Frame::trailers(trailers))));
            }
            if this.done {
                return Poll::Ready(None);
            }
            match ready!(Pin::new(&mut this.inner).poll_frame(cx)) {
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(data) => {
                        let out = this.scrubber.push(&data);
                        if !out.is_empty() {
                            return Poll::Ready(Some(Ok(Frame::data(out.into()))));
                        }
                    }
                    Err(frame) => {
                        this.trailers = frame.into_trailers().ok();
                        this.done = true;
                        let rest = this.scrubber.finish();
                        if !rest.is_empty() {
                            return Poll::Ready(Some(Ok(Frame::data(rest.into()))));
                        }
                    }
                },
                Some(Err(e)) => return Poll::Ready(Some(Err(e))),
                None => {
                    this.done = true;
                    let rest = this.scrubber.finish();
                    if !rest.is_empty() {
                        return Poll::Ready(Some(Ok(Frame::data(rest.into()))));
                    }
                }
            }
        }
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frames::Frames;
    use http_body_util::BodyExt as _;

    const PH: &str = "ghp_0123456789abcdef0123";
    const REAL: &str = "ghp_REALREALREALREALREAL";

    fn swaps() -> Swaps {
        Swaps::new([
            (PH.to_string(), REAL.to_string(), false),
            ("fffffffffffffffff".to_string(), "short".to_string(), false),
        ])
    }

    fn hn(name: &'static str) -> HeaderName {
        HeaderName::from_static(name)
    }

    #[test]
    fn bearer_custom_and_basic_headers_get_the_real_value() {
        let s = swaps();
        let hv = |v: &str| HeaderValue::from_str(v).unwrap();
        let auth = hn("authorization");
        let bearer = s
            .inject_header(&auth, &hv(&format!("Bearer {PH}")))
            .unwrap();
        assert_eq!(bearer, format!("Bearer {REAL}").as_str());
        assert!(bearer.is_sensitive());
        for name in ["x-api-key", "private-token", "x-goog-api-key", "cookie"] {
            assert_eq!(s.inject_header(&hn(name), &hv(PH)).unwrap(), REAL, "{name}");
        }

        let basic = format!("Basic {}", STANDARD.encode(format!("x-access-token:{PH}")));
        let got = s.inject_header(&auth, &hv(&basic)).unwrap();
        let decoded = STANDARD.decode(&got.as_bytes()[6..]).unwrap();
        assert_eq!(decoded, format!("x-access-token:{REAL}").as_bytes());

        assert!(s
            .inject_header(&auth, &hv("Bearer something-else"))
            .is_none());
        assert!(s.inject_header(&auth, &hv("Basic !!notbase64")).is_none());
    }

    #[test]
    fn headers_that_are_not_credentials_keep_the_placeholder() {
        let s = swaps();
        let arg = HeaderValue::from_str(&format!("{{\"path\": \"/{PH}.txt\"}}")).unwrap();
        assert!(s.inject_header(&hn("dropbox-api-arg"), &arg).is_none());
        let ua = HeaderValue::from_str(PH).unwrap();
        assert!(s.inject_header(&hn("user-agent"), &ua).is_none());
    }

    #[test]
    fn only_this_hosts_placeholders_are_swapped() {
        let s = Swaps::new([(PH.to_string(), REAL.to_string(), false)]);
        let other = "sk-ffffffffffffffffffffffff";
        let v = HeaderValue::from_str(&format!("{PH} {other}")).unwrap();
        assert_eq!(
            s.inject_header(&hn("authorization"), &v).unwrap(),
            format!("{REAL} {other}").as_str()
        );
    }

    #[test]
    fn uri_path_and_query_are_swapped_and_encoded_only_where_allowed() {
        let s = Swaps::new([
            ("aaaaaaaaaaaaaaaa".to_string(), "123:AB/c".to_string(), true),
            ("bbbbbbbbbbbbbbbb".to_string(), "k+y=1".to_string(), true),
            (
                "cccccccccccccccc".to_string(),
                "not-in-urls".to_string(),
                false,
            ),
        ]);
        assert_eq!(
            s.inject_uri("/botaaaaaaaaaaaaaaaa/getMe?key=bbbbbbbbbbbbbbbb&x=cccccccccccccccc")
                .unwrap(),
            "/bot123:AB%2Fc/getMe?key=k%2By%3D1&x=cccccccccccccccc"
        );
        assert!(s.inject_uri("/plain?x=1").is_none());
        assert!(swaps().inject_uri(&format!("/contents/{PH}.txt")).is_none());
    }

    #[test]
    fn scrubber_catches_secrets_split_across_every_chunk_boundary() {
        let swaps = Arc::new(swaps());
        let text = format!("token={REAL}; again {REAL}{REAL} and short, end short");
        let want =
            format!("token={PH}; again {PH}{PH} and fffffffffffffffff, end fffffffffffffffff");
        for a in 0..text.len() {
            for b in a..text.len() {
                let mut sc = Scrubber::new(swaps.clone());
                let mut out = sc.push(&text.as_bytes()[..a]);
                out.extend(sc.push(&text.as_bytes()[a..b]));
                out.extend(sc.push(&text.as_bytes()[b..]));
                out.extend(sc.finish());
                assert_eq!(String::from_utf8(out).unwrap(), want, "split at {a},{b}");
            }
        }
    }

    #[tokio::test]
    async fn scrubbed_body_flushes_the_tail_before_trailers() {
        let swaps = Arc::new(self::swaps());
        let mut trailers = http::HeaderMap::new();
        trailers.insert("x-t", HeaderValue::from_static("1"));
        let frames: Vec<Result<Frame<Bytes>, std::convert::Infallible>> = vec![
            Ok(Frame::data(Bytes::from(&REAL[..10]))),
            Ok(Frame::data(Bytes::from(&REAL[10..]))),
            Ok(Frame::trailers(trailers)),
        ];
        let body = Scrubbed::new(Frames::new(frames), swaps);
        let collected = body.collect().await.unwrap();
        assert_eq!(collected.trailers().unwrap()["x-t"], "1");
        assert_eq!(collected.to_bytes(), PH.as_bytes());

        let plain = http_body_util::Full::new(Bytes::from("no secrets"));
        let plain = Scrubbed::new(plain, Arc::new(self::swaps()));
        assert_eq!(plain.collect().await.unwrap().to_bytes(), "no secrets");
    }

    /// A body that yields a fixed list of frames.
    mod frames {
        use super::*;

        pub struct Frames<E>(std::collections::VecDeque<Result<Frame<Bytes>, E>>);

        impl<E> Frames<E> {
            pub fn new(frames: Vec<Result<Frame<Bytes>, E>>) -> Self {
                Self(frames.into())
            }
        }

        impl<E> Body for Frames<E>
        where
            E: Unpin,
        {
            type Data = Bytes;
            type Error = E;

            fn poll_frame(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Option<Result<Frame<Bytes>, E>>> {
                Poll::Ready(self.0.pop_front())
            }
        }
    }
}
