//! Log lines on stdout: `RUST_LOG`'s filter, `info` by default, alloy's HTTP transport
//! held at `info`, and the RPC URL and signer spec written redacted.

use std::{
    io::{
        self,
        IsTerminal,
        Write,
    },
    sync::Arc,
};

use alloy::transports::http::reqwest::Url;
use tracing::Level;
use tracing_subscriber::{
    filter::filter_fn,
    fmt::MakeWriter,
    layer::SubscriberExt,
    util::SubscriberInitExt,
    EnvFilter,
};

/// The target of alloy's HTTP transport, whose debug span names the whole URL.
const TRANSPORT: &str = "alloy_transport_http";

/// Install the subscriber, redacting `rpc_url` and `signer`.
pub fn init(rpc_url: Option<&str>, signer: Option<&str>) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let writer = Redacting(redactions(rpc_url, signer).into());
    let transport = filter_fn(|meta| {
        let rest = meta.target().strip_prefix(TRANSPORT);
        let ours = rest.is_some_and(|rest| rest.is_empty() || rest.starts_with("::"));
        !ours || *meta.level() <= Level::INFO
    });
    let subscriber = tracing_subscriber::fmt().with_env_filter(filter);
    let subscriber = subscriber.with_ansi(io::stdout().is_terminal());
    subscriber
        .with_writer(writer)
        .finish()
        .with(transport)
        .init();
}

/// Each secret and what is written instead, longest first: the URL whole as given, as
/// parsed and without its userinfo, as reqwest's errors name it; then its userinfo,
/// path and query; the spec, with and without `0x`. A part under 4 characters is
/// left, so as not to rewrite unrelated text.
fn redactions(rpc_url: Option<&str>, signer: Option<&str>) -> Vec<(String, String)> {
    let mut secrets = vec![];
    if let Some(raw) = rpc_url {
        let url = Url::parse(raw).ok();
        let shown = url.as_ref().map_or("…".into(), |u| {
            format!("{}/…", u.origin().ascii_serialization())
        });
        secrets.push((raw.to_owned(), shown.clone()));
        if let Some(mut url) = url {
            secrets.push((url.as_str().to_owned(), shown.clone()));
            let parts = [
                url.username(),
                url.password().unwrap_or_default(),
                url.path(),
            ];
            secrets.extend(parts.map(|p| (p.to_owned(), "…".into())));
            secrets.push((url.query().unwrap_or_default().to_owned(), "…".into()));
            let _ = (url.set_username(""), url.set_password(None));
            secrets.push((url.as_str().to_owned(), shown));
        }
    }
    if let Some(spec) = signer {
        let bare = spec.trim().trim_start_matches("0x");
        secrets.extend([spec, bare].map(|s| (s.to_owned(), "…".into())));
    }
    secrets.retain(|(secret, _)| secret.len() >= 4);
    secrets.sort_by_key(|(secret, _)| std::cmp::Reverse(secret.len()));
    secrets
}

/// Stdout, with each secret replaced in every line written.
#[derive(Clone)]
struct Redacting(Arc<[(String, String)]>);

impl<'a> MakeWriter<'a> for Redacting {
    type Writer = Self;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

impl Write for Redacting {
    /// A whole formatted event: the subscriber writes each in one call.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut line = String::from_utf8_lossy(buf).into_owned();
        for (secret, shown) in self.0.iter() {
            line = line.replace(secret, shown);
        }
        io::stdout().write_all(line.as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }
}
