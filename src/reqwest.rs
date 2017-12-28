//! # Reqwest-based EventSource client

extern crate reqwest as reqw;

mod errors {
    error_chain! {
        foreign_links {
            Reqwest(super::reqw::Error);
            Io(::std::io::Error);
        }

        errors {
            Http(status: super::reqw::StatusCode) {
                description("HTTP request failed")
                display("HTTP status code: {}", status)
            }

            InvalidContentType(mime_type: super::reqw::mime::Mime) {
                description("unexpected Content-Type header")
                display("unexpected Content-Type: {}", mime_type)
            }

            NoContentType {
                description("no Content-Type header in response")
                display("Content-Type missing")
            }
        }
    }
}
pub use self::errors::*;

use std::io::{BufRead, BufReader};
use std::time::{Duration, Instant};
use super::event::{Event, ParseResult, parse_event_line};
use self::reqw::header::{Headers, Accept, ContentType, qitem};
use self::reqw::mime;

const DEFAULT_RETRY: u64 = 5000;

/// A client for a Server-Sent Events endpoint.
///
/// Read events by iterating over the client.
pub struct Client {
    client: reqw::Client,
    response: Option<BufReader<reqw::Response>>,
    url: reqw::Url,
    last_event_id: Option<String>,
    last_try: Option<Instant>,

    /// Reconnection time in milliseconds. Note that the reconnection time can be changed by the
    /// event stream, so changing this may not make a difference.
    pub retry: Duration,

    /// Default headers that should be applied to requests. If the conflict with per-request
    /// headers they will be overwritten.
    pub default_headers: Headers,
}

impl Client {
    /// Constructs a new EventSource client for the given URL.
    ///
    /// This does not start an HTTP request.
    pub fn new(url: reqw::Url) -> Client {
        Client {
            client: reqw::Client::new(),
            response: None,
            url: url,
            last_event_id: None,
            last_try: None,
            retry: Duration::from_millis(DEFAULT_RETRY),
            default_headers: Headers::new(),
        }
    }

    fn next_request(&mut self) -> Result<()> {
        let mut headers = self.default_headers.clone();
        headers.set(Accept(vec![qitem(mime::TEXT_EVENT_STREAM)]));
        if let Some(ref id) = self.last_event_id {
            headers.set_raw("Last-Event-ID", vec![id.as_bytes().to_vec()]);
        }

        let res = self.client.get(self.url.clone()).headers(headers).send()?;

        // Check status code and Content-Type.
        {
            let status = res.status();
            if !status.is_success() {
                return Err(ErrorKind::Http(status.clone()).into());
            }
            if let Some(&ContentType(ref content_type)) = res.headers().get::<ContentType>() {
                // Compare type and subtype only, MIME parameters are ignored.
                if (content_type.type_(), content_type.subtype()) !=
                    (mime::TEXT, mime::EVENT_STREAM)
                {
                    return Err(ErrorKind::InvalidContentType(content_type.clone()).into());
                }
            } else {
                return Err(ErrorKind::NoContentType.into());
            }
        }

        self.response = Some(BufReader::new(res));
        Ok(())
    }
}

// Helper macro for Option<Result<...>>
macro_rules! try_option {
    ($e:expr) => (match $e {
        Ok(val) => val,
        Err(err) => return Some(Err(::std::convert::From::from(err))),
    });
}

/// Iterate over the client to get events.
///
/// HTTP requests are made transparently while iterating.
impl Iterator for Client {
    type Item = Result<Event>;

    fn next(&mut self) -> Option<Result<Event>> {
        if self.response.is_none() {
            // We may have to wait for the next request.
            if let Some(last_try) = self.last_try {
                let elapsed = last_try.elapsed();
                if elapsed < self.retry {
                    ::std::thread::sleep(self.retry - elapsed);
                }
            }
            // Set here in case the request fails.
            self.last_try = Some(Instant::now());

            try_option!(self.next_request());
        }

        let result = {
            let mut event = Event::new();
            let mut line = String::new();
            let reader = self.response.as_mut().unwrap();

            loop {
                match reader.read_line(&mut line) {
                    // Got new bytes from stream
                    Ok(_n) if _n > 0 => {
                        match parse_event_line(&line, &mut event) {
                            ParseResult::Next => (), // okay, just continue
                            ParseResult::Dispatch => {
                                if let Some(ref id) = event.id {
                                    self.last_event_id = Some(id.clone());
                                }
                                return Some(Ok(event));
                            }
                            ParseResult::SetRetry(ref retry) => {
                                self.retry = *retry;
                            }
                        }
                        line.clear();
                    }
                    // Nothing read from stream
                    Ok(_) => break None,
                    Err(err) => break Some(Err(::std::convert::From::from(err))),
                }
            }
        };

        match result {
            None | Some(Err(_)) => {
                // EOF or a stream error, retry after timeout
                self.last_try = Some(Instant::now());
                self.response = None;
                self.next()
            }
            _ => result,
        }
    }
}
