//! nuncio-mime — MIME parsing/building, HTML sanitizing, reply quoting,
//! threading, `mailto:` handling and recipient helpers.
//!
//! The crate only depends on `nuncio-core`; every other workspace crate talks to
//! mail content through this API. See `docs/ARCHITECTURE.md` §7 for the security
//! requirements implemented by [`sanitize`].
//!
//! Typical flow:
//!
//! 1. [`parse::parse`] a raw RFC 822 message.
//! 2. [`sanitize::render_document`] its HTML for the reader iframe.
//! 3. [`reply::build_reply`] to produce a composer draft.
//! 4. [`build::build_message`] to turn the draft into bytes for SMTP/APPEND.

pub mod addresses;
pub mod attachments;
pub mod build;
pub mod mailto;
pub mod parse;
pub mod reply;
pub mod sanitize;
pub mod text;
pub mod threading;
mod util;

pub use addresses::{
    dedupe_addresses, format_address, format_address_list, parse_address_list, parse_one_address,
};
pub use attachments::{is_dangerous, mime_for_filename};
pub use build::{
    add_bcc_header, build_message, build_message_with_bcc, build_raw_with_attachment_eml,
    ForwardAsAttachment, OutgoingMessage,
};
pub use mailto::{is_mailto, parse_mailto, MailtoData};
pub use parse::{
    extract_part, extract_part_by_index, parse, parse_headers_only, CalendarPart, ParsedAttachment,
    ParsedHeaders, ParsedMessage, PartPath,
};
pub use reply::{build_reply, ReplyParts};
pub use sanitize::{
    cid_resource_url, has_remote_content, render_document, sanitize_fragment, RenderInput,
    RenderedHtml,
};
pub use text::{
    decode_preview_part, html_to_text, linkify_html, normalize_newlines, plain_text_to_html,
    strip_text,
};
pub use threading::{normalize_message_id, thread_key};
pub use util::{decode_header_phrase, escape_html};
