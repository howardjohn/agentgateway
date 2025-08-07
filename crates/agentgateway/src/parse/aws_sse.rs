use std::fmt;
use std::fmt::Debug;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use aws_event_stream_parser::{EventStreamCodec, Message};
use axum_core::Error;
use bytes::{Buf, Bytes, BytesMut};
use futures::{Stream, StreamExt, TryStreamExt};
use http_body::Body;
use http_body_util::BodyExt;
use pin_project_lite::pin_project;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_sse_codec::{Event, Frame, SseDecoder, SseEncoder};
use tokio_util::codec::{Decoder, Encoder, FramedRead};
use tokio_util::io::StreamReader;

use super::passthrough::parser as passthrough_parser;
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
