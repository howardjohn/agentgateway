use aws_event_stream_parser::{EventStreamCodec, Message};
use bytes::Bytes;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_sse_codec::{Event, Frame, SseEncoder};

use super::transform::parser as transform_parser;
use crate::*;

pub fn transform<O: Serialize>(
	b: http::Body,
	mut f: impl FnMut(Message) -> Option<O> + Send + 'static,
) -> http::Body {
	let decoder = EventStreamCodec;
	let encoder = SseEncoder::new();

	transform_parser(b, decoder, encoder, move |o| {
		let transformed = f(o)?;
		let json_bytes = serde_json::to_vec(&transformed).ok()?;
		Some(Frame::Event(Event::<Bytes> {
			data: Bytes::from(json_bytes),
			name: std::borrow::Cow::Borrowed(""),
			id: None,
		}))
	})
}

fn unwrap_sse_data(frame: Message) -> Bytes {
	Bytes::copy_from_slice(&frame.body)
}

pub(super) fn unwrap_json<T: DeserializeOwned>(frame: Message) -> anyhow::Result<Option<T>> {
	Ok(serde_json::from_slice(&unwrap_sse_data(frame))?)
}
