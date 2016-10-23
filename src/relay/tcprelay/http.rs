// The MIT License (MIT)

// Copyright (c) 2014 Y. T. CHUNG <zonyitoo@gmail.com>

// Permission is hereby granted, free of charge, to any person obtaining a copy of
// this software and associated documentation files (the "Software"), to deal in
// the Software without restriction, including without limitation the rights to
// use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:

// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.

// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
// FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
// COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
// IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
// CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

/// Http Proxy

use std::io::{self, Read, Write};
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};
use std::mem;
use std::str;
use std::fmt;

use hyper::uri::RequestUri;
use hyper::header::{Header, HeaderFormat, Headers};
use hyper::status::StatusCode;
use hyper::version::HttpVersion;
use hyper::method::Method;
use hyper;

use httparse::{self, Request, Response};

use url::Host;

use ip::IpAddr;

use futures::{self, Future, BoxFuture, Poll};

use tokio_core::io::write_all;

use relay::socks5::Address;

#[derive(Debug)]
pub struct HttpRequest {
    pub version: HttpVersion,
    pub method: Method,
    pub request_uri: RequestUri,
    pub headers: Headers,
}

impl HttpRequest {
    pub fn from_raw<'headers, 'buf: 'headers>(req: &Request<'headers, 'buf>,
                                              headers: &'headers [httparse::Header])
                                              -> hyper::Result<HttpRequest> {
        Ok(HttpRequest {
            version: if req.version.unwrap() == 1 {
                HttpVersion::Http11
            } else {
                HttpVersion::Http10
            },
            method: try!(req.method.unwrap().parse::<Method>()),
            request_uri: try!(req.path.unwrap().parse::<RequestUri>()),
            headers: try!(Headers::from_raw(headers)),
        })
    }

    pub fn clear_request_uri_host(&mut self) {
        let ptr = &mut self.request_uri as *mut RequestUri;
        match &mut self.request_uri {
            &mut RequestUri::AbsoluteUri(ref url) => {
                let mut abs = String::new();
                abs += url.path();
                if let Some(query) = url.query() {
                    abs += "?";
                    abs += query;
                }

                if let Some(frag) = url.fragment() {
                    abs += "#";
                    abs += frag;
                }

                // Force replace
                let unsafe_ref = unsafe { &mut *ptr };
                ::std::mem::replace(unsafe_ref, RequestUri::AbsolutePath(abs));
            }
            _ => {}
        }
    }

    pub fn write_to<W>(self, w: W) -> BoxFuture<W, io::Error>
        where W: Write + Send + 'static
    {
        futures::lazy(move || {
                let mut w = Vec::new();
                try!(write!(w,
                            "{} {} {}\r\n",
                            self.method,
                            self.request_uri,
                            self.version));

                for header in self.headers.iter() {
                    try!(write!(w, "{}: {}\r\n", header.name(), header.value_string()));
                }

                try!(write!(w, "\r\n"));
                Ok(w)
            })
            .and_then(|buf| write_all(w, buf))
            .map(|(w, _)| w)
            .boxed()
    }

    #[inline]
    pub fn get_address(&self) -> Result<Address, StatusCode> {
        get_address(&self.request_uri)
    }
}

fn get_address(uri: &RequestUri) -> Result<Address, StatusCode> {
    match uri {
        &RequestUri::Authority(ref s) => {
            match s.parse::<SocketAddr>() {
                Ok(addr) => Ok(Address::SocketAddress(addr)),
                Err(_) => {
                    let mut sp = s.splitn(2, ':');
                    match (sp.next(), sp.next()) {
                        (Some(host), Some(port)) => {
                            let port = match port.parse::<u16>() {
                                Ok(port) => port,
                                Err(err) => {
                                    error!("Failed to parse Url, {}", err);
                                    return Err(StatusCode::BadRequest);
                                }
                            };

                            Ok(Address::DomainNameAddress(host.to_owned(), port))
                        }
                        (host, port) => {
                            error!("Failed to parse Url, {:?}:{:?}", host, port);
                            return Err(StatusCode::BadRequest);
                        }
                    }
                }
            }
        }
        &RequestUri::AbsoluteUri(ref uri) => {
            if !uri.has_host() {
                error!("URI does not have Host: {:?}", uri);
                return Err(StatusCode::BadRequest);
            }

            let port = uri.port_or_known_default().unwrap_or(80);

            let addr = match uri.host().unwrap() {
                Host::Domain(dom) => Address::DomainNameAddress(dom.to_owned(), port),
                Host::Ipv4(v4) => Address::SocketAddress(SocketAddr::V4(SocketAddrV4::new(v4, port))),
                Host::Ipv6(v6) => Address::SocketAddress(SocketAddr::V6(SocketAddrV6::new(v6, port, 0, 0))),
            };

            Ok(addr)
        }
        u => {
            error!("Invalid Uri {:?}", u);
            Err(StatusCode::BadRequest)
        }
    }
}

#[derive(Debug)]
pub struct HttpResponse {
    pub version: HttpVersion,
    pub status: StatusCode,
    pub message: Option<String>,
    pub headers: Headers,
}

impl HttpResponse {
    /// Creates an empty Response
    pub fn new() -> HttpResponse {
        HttpResponse {
            version: HttpVersion::Http11,
            status: StatusCode::Ok,
            message: None,
            headers: Headers::new(),
        }
    }

    pub fn from_raw<'headers, 'buf: 'headers>(rsp: &Response<'headers, 'buf>,
                                              headers: &'headers [httparse::Header])
                                              -> hyper::Result<HttpResponse> {
        Ok(HttpResponse {
            version: if rsp.version.unwrap() == 1 {
                HttpVersion::Http11
            } else {
                HttpVersion::Http10
            },
            status: StatusCode::from_u16(rsp.code.unwrap()),
            message: rsp.reason.map(|s| s.to_owned()).clone(),
            headers: try!(Headers::from_raw(headers)),
        })
    }

    pub fn write_to<W>(self, w: W) -> BoxFuture<W, io::Error>
        where W: Write + Send + 'static
    {
        futures::lazy(move || {
                let mut w = Vec::new();
                let msg = self.message
                    .as_ref()
                    .map(|s| &s[..])
                    .or_else(|| self.status.canonical_reason())
                    .unwrap_or("<unknown status code>");
                try!(write!(w, "{} {} {}\r\n", self.version, self.status.to_u16(), msg));
                for header in self.headers.iter() {
                    try!(write!(w, "{}: {}\r\n", header.name(), header.value_string()));
                }

                try!(write!(w, "\r\n"));
                Ok(w)
            })
            .and_then(|buf| write_all(w, buf))
            .map(|(w, _)| w)
            .boxed()
    }
}

pub fn write_response<W>(w: W, version: HttpVersion, status: StatusCode) -> BoxFuture<W, io::Error>
    where W: Write + Send + 'static
{
    let buf = format!("{} {}\r\n\r\n", version, status);
    write_all(w, buf.into_bytes()).map(|(w, _)| w).boxed()
}

#[derive(Debug, Clone)]
pub struct XForwardFor(pub Vec<IpAddr>);

impl Header for XForwardFor {
    fn header_name() -> &'static str {
        "X-Forward-For"
    }

    fn parse_header(raw: &[Vec<u8>]) -> hyper::Result<XForwardFor> {
        let mut ips = Vec::new();
        for raw_h in raw.iter() {
            let xfor = try!(str::from_utf8(&raw_h[..]));
            for xfor_str in xfor.split(',') {
                let trimmed = xfor_str.trim();
                if trimmed.is_empty() {
                    // Ignore empty string
                    continue;
                }
                match trimmed.parse::<IpAddr>() {
                    Ok(i) => ips.push(i),
                    Err(..) => return Err(hyper::Error::Header),
                }
            }
        }

        Ok(XForwardFor(ips))
    }
}

impl HeaderFormat for XForwardFor {
    fn fmt_header(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut first = true;
        for ip in &self.0 {
            if first {
                first = false;
            } else {
                try!(write!(f, ", "));
            }

            try!(write!(f, "{}", ip));
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct XRealIp(pub Option<IpAddr>);

impl Header for XRealIp {
    fn header_name() -> &'static str {
        "X-Real-IP"
    }

    fn parse_header(raw: &[Vec<u8>]) -> hyper::Result<XRealIp> {
        let mut ip = None;
        for raw_ip in raw.iter() {
            let x_ip = try!(str::from_utf8(&raw_ip[..]));
            match x_ip.trim().parse::<IpAddr>() {
                Ok(i) => {
                    if let Some(prev_ip) = ip.take() {
                        if prev_ip != i {
                            return Err(hyper::Error::Header);
                        }
                    }

                    ip = Some(i);
                }
                Err(..) => return Err(hyper::Error::Header),
            }
        }

        Ok(XRealIp(ip))
    }
}

impl HeaderFormat for XRealIp {
    fn fmt_header(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(ref ip) = self.0 {
            try!(write!(f, "{}", ip));
        }

        Ok(())
    }
}

/// HTTP Client
pub enum RequestReader<R>
    where R: Read
{
    Pending { r: R, buf: Vec<u8> },
    Empty,
}

impl<R> RequestReader<R>
    where R: Read
{
    pub fn new(r: R) -> RequestReader<R> {
        RequestReader::with_buf(r, Vec::new())
    }

    pub fn with_buf(r: R, buf: Vec<u8>) -> RequestReader<R> {
        RequestReader::Pending { r: r, buf: buf }
    }
}

impl<R> Future for RequestReader<R>
    where R: Read
{
    type Item = (R, HttpRequest, Vec<u8>);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let mut lbuf = [0u8; 4096];
        let (req, len) = match self {
            &mut RequestReader::Pending { ref mut r, ref mut buf } => {
                let mut http_req = None;
                let mut total_len = 0;
                loop {
                    let n = try_nb!(r.read(&mut lbuf));
                    buf.extend_from_slice(&lbuf[..n]);

                    // Maximum 128 headers
                    let mut headers = [httparse::EMPTY_HEADER; 128];
                    let headers_ptr = &headers as *const _;
                    let mut req = Request::new(&mut headers);
                    match req.parse(&mut buf[..]) {
                        Ok(httparse::Status::Partial) => {
                            if n == 0 {
                                // Already EOF!
                                let err = io::Error::new(io::ErrorKind::UnexpectedEof, "Unexpected Eof");
                                return Err(err);
                            }
                        }
                        Ok(httparse::Status::Complete(len)) => {
                            total_len = len;

                            // Make borrow checker happy
                            let headers_ref = unsafe { &*headers_ptr };
                            let hreq = match HttpRequest::from_raw(&req, headers_ref) {
                                Ok(r) => r,
                                Err(err) => {
                                    error!("HttpRequest::from_raw: {}", err);
                                    let err = io::Error::new(io::ErrorKind::Other, "Hyper error");
                                    return Err(err);
                                }
                            };
                            http_req = Some(hreq);
                            break;
                        }
                        Err(err) => {
                            error!("Request parse: {:?}", err);
                            let err = io::Error::new(io::ErrorKind::Other, "Hyper error");
                            return Err(err);
                        }
                    }
                }

                (http_req.unwrap(), total_len)
            }
            &mut RequestReader::Empty => panic!("poll a RequestReader after it's done"),
        };

        match mem::replace(self, RequestReader::Empty) {
            RequestReader::Pending { r, buf } => Ok((r, req, buf[len..].to_vec()).into()),
            RequestReader::Empty => unreachable!(),
        }
    }
}